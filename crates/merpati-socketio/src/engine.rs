//! Engine.IO client, protocol 4 (Socket.IO v3/v4) and 3 (Socket.IO v2),
//! over HTTP long-polling and WebSocket, including the polling -> WebSocket
//! upgrade of the official client:
//!
//! 1. handshake with a polling `GET` (or directly over WebSocket);
//! 2. while polling continues, open a WebSocket with the session id and send
//!    `2probe`; the server answers `3probe` (and flushes the pending `GET`
//!    with a noop);
//! 3. pause polling: wait for the in-flight `GET` and `POST`, queueing new
//!    packets, then send `5` (upgrade) and continue on the WebSocket.
//!
//! Protocol differences handled here: v4 servers ping and the client pongs,
//! v3 clients ping; v4 polling payloads are separated by `\x1e` with binary
//! as `b<base64>`, v3 payloads are `<length>:<packet>` (length in UTF-16
//! units) with binary as `b4<base64>` (`b64=1` is requested); v3 WebSocket
//! binary frames start with the packet type byte.
//!
//! The engine runs as its own task; the Socket.IO layer talks to it through
//! [`EngineHandle`] and receives [`EngineEvent`]s.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderName, HeaderValue};
use merpati_core::protocols::SocketIoTransport;
use reqwest::cookie::CookieStore;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::packet::truncate;

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type WsStream = merpati_http::ws::WsStream;

/// How long the WebSocket probe may take before the session stays on polling.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a clean close may take.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Largest polling response body accepted. Servers send at most `maxPayload`
/// (1 MB by default) per response; a larger body ends the session instead of
/// filling memory. WebSocket messages have the same bound (tungstenite's
/// default `max_message_size` of 64 MiB, 16 MiB per frame).
pub const MAX_POLLING_BODY: usize = 64 * 1024 * 1024;
/// Bytes of an error response body kept for the error message.
const MAX_ERROR_BODY: usize = 8 * 1024;

/// The transport currently carrying a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActiveTransport {
    Polling,
    WebSocket,
}

impl ActiveTransport {
    pub fn label(self) -> &'static str {
        match self {
            ActiveTransport::Polling => "polling",
            ActiveTransport::WebSocket => "websocket",
        }
    }
}

/// Data of an Engine.IO message packet.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Frame {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Packet {
    Open(String),
    Close,
    Ping(String),
    Pong(String),
    Message(Frame),
    Upgrade,
    Noop,
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

fn b64() -> &'static base64::engine::GeneralPurpose {
    &base64::engine::general_purpose::STANDARD
}

/// `<type><data>`; binary messages must be encoded by the caller.
fn encode_text(p: &Packet) -> String {
    match p {
        Packet::Open(d) => format!("0{d}"),
        Packet::Close => "1".into(),
        Packet::Ping(d) => format!("2{d}"),
        Packet::Pong(d) => format!("3{d}"),
        Packet::Message(Frame::Text(t)) => format!("4{t}"),
        Packet::Message(Frame::Binary(b)) => format!("4{}", b64().encode(b)),
        Packet::Upgrade => "5".into(),
        Packet::Noop => "6".into(),
    }
}

/// Decodes a text packet, including base64 binary messages (`b<base64>` in
/// protocol 4, `b4<base64>` in protocol 3).
pub(crate) fn decode_text(eio: u8, s: &str) -> Result<Packet, String> {
    if let Some(rest) = s.strip_prefix('b') {
        let data = if eio == 3 { rest.get(1..).unwrap_or_default() } else { rest };
        return b64()
            .decode(data.trim())
            .map(|b| Packet::Message(Frame::Binary(b)))
            .map_err(|e| format!("invalid base64 binary packet: {e}"));
    }
    let mut chars = s.chars();
    let kind = chars.next().ok_or_else(|| "empty Engine.IO packet".to_string())?;
    let data = chars.as_str().to_string();
    Ok(match kind {
        '0' => Packet::Open(data),
        '1' => Packet::Close,
        '2' => Packet::Ping(data),
        '3' => Packet::Pong(data),
        '4' => Packet::Message(Frame::Text(data)),
        '5' => Packet::Upgrade,
        '6' => Packet::Noop,
        _ => return Err(format!("unknown Engine.IO packet \"{}\"", truncate(s))),
    })
}

/// A WebSocket binary frame: raw message data in protocol 4, prefixed with
/// the packet type byte in protocol 3.
pub(crate) fn decode_ws_binary(eio: u8, data: Vec<u8>) -> Result<Packet, String> {
    if eio != 3 {
        return Ok(Packet::Message(Frame::Binary(data)));
    }
    match data.first() {
        Some(4) => Ok(Packet::Message(Frame::Binary(data[1..].to_vec()))),
        Some(t) => Err(format!("unexpected binary Engine.IO packet of type {t}")),
        None => Err("empty binary frame".into()),
    }
}

pub(crate) fn encode_ws(eio: u8, p: &Packet) -> Message {
    match p {
        Packet::Message(Frame::Binary(b)) if eio == 3 => {
            let mut v = Vec::with_capacity(b.len() + 1);
            v.push(4);
            v.extend_from_slice(b);
            Message::binary(v)
        }
        Packet::Message(Frame::Binary(b)) => Message::binary(b.clone()),
        other => Message::text(encode_text(other)),
    }
}

/// One packet of a polling payload.
fn encode_polling_packet(eio: u8, p: &Packet) -> String {
    let s = match p {
        Packet::Message(Frame::Binary(b)) if eio == 3 => format!("b4{}", b64().encode(b)),
        Packet::Message(Frame::Binary(b)) => format!("b{}", b64().encode(b)),
        other => encode_text(other),
    };
    if eio == 3 { format!("{}:{s}", s.encode_utf16().count()) } else { s }
}

pub(crate) fn encode_payload(eio: u8, packets: &[Packet]) -> String {
    let parts: Vec<String> = packets.iter().map(|p| encode_polling_packet(eio, p)).collect();
    parts.join(if eio == 3 { "" } else { "\u{1e}" })
}

/// Decodes a polling response body. Protocol 3 bodies sent as
/// `application/octet-stream` use the binary payload format.
pub(crate) fn decode_payload(eio: u8, body: &[u8], octet_stream: bool) -> Result<Vec<Packet>, String> {
    if eio == 3 && (octet_stream || matches!(body.first(), Some(0 | 1))) {
        return decode_binary_payload_v3(body);
    }
    let text = std::str::from_utf8(body).map_err(|_| "the polling response is not UTF-8 text".to_string())?;
    if eio != 3 {
        return text.split('\u{1e}').filter(|p| !p.is_empty()).map(|p| decode_text(eio, p)).collect();
    }
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let colon = rest.find(':').ok_or_else(|| format!("malformed payload \"{}\"", truncate(rest)))?;
        let n: usize = rest[..colon].parse().map_err(|_| format!("malformed payload \"{}\"", truncate(rest)))?;
        let body = &rest[colon + 1..];
        let (mut units, mut end) = (0usize, 0usize);
        for c in body.chars() {
            if units >= n {
                break;
            }
            units += c.len_utf16();
            end += c.len_utf8();
        }
        if units != n {
            return Err("truncated payload".into());
        }
        if n > 0 {
            out.push(decode_text(3, &body[..end])?);
        }
        rest = &body[end..];
    }
    Ok(out)
}

/// `<0|1><length digits as bytes><0xFF><data>` repeated (protocol 3).
fn decode_binary_payload_v3(data: &[u8]) -> Result<Vec<Packet>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let is_binary = data[i] == 1;
        i += 1;
        let mut len = 0usize;
        let mut digits = 0;
        while i < data.len() && data[i] != 0xFF {
            if data[i] > 9 || digits > 10 {
                return Err("malformed binary payload".into());
            }
            len = len * 10 + data[i] as usize;
            digits += 1;
            i += 1;
        }
        i += 1;
        let chunk = data.get(i..i + len).ok_or_else(|| "truncated binary payload".to_string())?;
        i += len;
        if is_binary {
            out.push(decode_ws_binary(3, chunk.to_vec())?);
        } else {
            let text = std::str::from_utf8(chunk).map_err(|_| "invalid UTF-8 in binary payload".to_string())?;
            out.push(decode_text(3, text)?);
        }
    }
    Ok(out)
}

/// The handshake data of an open packet.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpenInfo {
    pub sid: String,
    pub upgrades: Vec<String>,
    pub ping_interval: Duration,
    pub ping_timeout: Duration,
    pub max_payload: usize,
    /// Protocol 4 servers announce `maxPayload`; protocol 3 servers do not.
    pub has_max_payload: bool,
}

pub(crate) fn parse_open(eio: u8, json: &str) -> Result<OpenInfo, String> {
    let v: Value =
        serde_json::from_str(json).map_err(|_| format!("invalid Engine.IO open packet \"{}\"", truncate(json)))?;
    let sid = v.get("sid").and_then(Value::as_str).filter(|s| !s.is_empty());
    let sid = sid.ok_or_else(|| format!("Engine.IO open packet without sid: {}", truncate(json)))?;
    let ms = |key: &str, default: u64| v.get(key).and_then(Value::as_u64).unwrap_or(default);
    let max = v.get("maxPayload").and_then(Value::as_u64);
    Ok(OpenInfo {
        sid: sid.to_string(),
        upgrades: v
            .get("upgrades")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        ping_interval: Duration::from_millis(ms("pingInterval", 25_000)),
        ping_timeout: Duration::from_millis(ms("pingTimeout", if eio == 3 { 60_000 } else { 20_000 })),
        max_payload: max.map(|m| m as usize).unwrap_or(if eio == 3 { 100_000_000 } else { 1_000_000 }),
        has_max_payload: max.is_some(),
    })
}

// ---------------------------------------------------------------------------
// HTTP / WebSocket requests
// ---------------------------------------------------------------------------

pub(crate) struct EngineConfig {
    /// `http(s)://host:port/<path>/?<user query>` without Engine.IO parameters.
    pub base: url::Url,
    /// Extra headers, `Cookie` excluded (see `user_cookie`).
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// A `Cookie` header set by the user, merged with the jar's cookies.
    pub user_cookie: Option<String>,
    pub cookies: Arc<dyn CookieStore>,
    pub verify_ssl: bool,
    pub tls: merpati_http::tls::TlsSetup,
    /// Proxy for polling (reqwest) and the WebSocket transport alike.
    pub proxy: merpati_core::settings::ProxySettings,
    pub transport: SocketIoTransport,
    /// 3 or 4.
    pub eio: u8,
}

/// A failed request; `rejected` when the server answered with an HTTP error
/// or dropped the request without answering (as opposed to a refused
/// connection, DNS, TLS or timeout failure).
#[derive(Debug)]
pub(crate) struct Failure {
    pub rejected: bool,
    pub message: String,
}

impl Failure {
    fn network(message: String) -> Failure {
        Failure { rejected: false, message }
    }

    fn from_reqwest(e: &reqwest::Error) -> Failure {
        Failure { rejected: dropped_without_response(e), message: describe_reqwest(e) }
    }

    /// An Engine.IO error body (`{"code":5,"message":"Unsupported protocol
    /// version"}`) other than code 0 "Transport unknown".
    fn engine_error_other_than_transport(&self) -> bool {
        let Some(start) = self.message.find('{') else { return false };
        let Ok(v) = serde_json::from_str::<Value>(&self.message[start..]) else { return false };
        v.get("code").and_then(Value::as_u64).is_some_and(|c| c != 0)
    }
}

struct Http {
    cfg: EngineConfig,
    client: reqwest::Client,
    tls: Option<Arc<rustls::ClientConfig>>,
    /// The proxy requests go through (`scheme://host:port`), for messages.
    proxy: Option<String>,
    counter: AtomicU64,
}

impl Http {
    fn new(cfg: EngineConfig) -> Result<Http, String> {
        // reqwest (rustls-no-provider) needs the process-wide provider even without TLS.
        merpati_http::tls::install_provider();
        let tls = if cfg.base.scheme() == "https" {
            let mut c = cfg.tls.client_config(cfg.verify_ssl).map_err(|e| format!("TLS setup failed: {e}"))?;
            c.alpn_protocols = vec![b"http/1.1".to_vec()];
            Some(Arc::new(c))
        } else {
            None
        };
        let b = reqwest::Client::builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(90));
        let mut b = merpati_http::proxy::apply_to_reqwest(b, &cfg.proxy).map_err(|e| e.message)?;
        if let Some(t) = &tls {
            b = b.tls_backend_preconfigured((**t).clone());
        }
        let client = b.build().map_err(|e| format!("Could not create HTTP client: {e}"))?;
        let proxy = merpati_http::proxy::proxy_for_url(&cfg.proxy, cfg.base.as_str(), "http").map(|p| p.to_string());
        Ok(Http { cfg, client, tls, proxy, counter: AtomicU64::new(0) })
    }

    /// A failed polling request; failures caused by the proxy name it.
    fn failure(&self, e: &reqwest::Error) -> Failure {
        let mut f = Failure::from_reqwest(e);
        if let Some(msg) = self.proxy.as_deref().and_then(|p| proxy_failure(e, p)) {
            f = Failure::network(msg);
        }
        f
    }

    /// Engine.IO URL for a transport; `ws` gives the `ws(s)://` form.
    fn url(&self, polling: bool, sid: Option<&str>) -> url::Url {
        let mut url = self.cfg.base.clone();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("EIO", if self.cfg.eio == 3 { "3" } else { "4" });
            q.append_pair("transport", if polling { "polling" } else { "websocket" });
            if polling {
                let n = self.counter.fetch_add(1, Ordering::Relaxed);
                q.append_pair("t", &format!("{}{n}", crate::now_ms()));
                if self.cfg.eio == 3 {
                    q.append_pair("b64", "1");
                }
            }
            if let Some(sid) = sid {
                q.append_pair("sid", sid);
            }
        }
        if !polling {
            let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
            let _ = url.set_scheme(scheme);
        }
        url
    }

    /// The `Cookie` header for `url` (an http(s) URL): the user's cookies
    /// followed by the jar's.
    fn cookie_header(&self, url: &url::Url) -> Option<String> {
        let jar = self.cfg.cookies.cookies(url).and_then(|v| v.to_str().ok().map(str::to_string));
        match (self.cfg.user_cookie.as_deref(), jar) {
            (Some(u), Some(j)) => Some(format!("{u}; {j}")),
            (Some(u), None) => Some(u.to_string()),
            (None, j) => j,
        }
    }

    fn store_cookies(&self, url: &url::Url, headers: &http::HeaderMap) {
        let mut set = headers.get_all(http::header::SET_COOKIE).iter();
        self.cfg.cookies.set_cookies(&mut set, url);
    }

    fn request(&self, method: reqwest::Method, url: &url::Url) -> reqwest::RequestBuilder {
        let mut rb = self.client.request(method, url.clone());
        for (k, v) in &self.cfg.headers {
            rb = rb.header(k.clone(), v.clone());
        }
        if let Some(c) = self.cookie_header(url) {
            rb = rb.header(http::header::COOKIE, c);
        }
        rb
    }

    async fn read(&self, url: &url::Url, mut resp: reqwest::Response) -> Result<(Vec<u8>, bool), Failure> {
        self.store_cookies(url, resp.headers());
        let status = resp.status();
        let octet = resp
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/octet-stream"));
        // Read in chunks so that an endless or huge body fails instead of
        // filling memory; of an error page only the start is kept.
        let too_large =
            || Failure::network(format!("the response body exceeded the {} MiB limit", MAX_POLLING_BODY >> 20));
        let limit = if status.is_success() { MAX_POLLING_BODY } else { MAX_ERROR_BODY };
        if status.is_success() && resp.content_length().is_some_and(|n| n > limit as u64) {
            return Err(too_large());
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| Failure::network(describe_reqwest(&e)))? {
            if body.len() + chunk.len() > limit {
                if status.is_success() {
                    return Err(too_large());
                }
                body.extend_from_slice(&chunk[..limit - body.len()]);
                break;
            }
            body.extend_from_slice(&chunk);
        }
        if status == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
            && let Some(p) = &self.proxy
        {
            // Not "rejected": WebSocket would go through the same proxy.
            return Err(Failure::network(format!(
                "The proxy {p} rejected the request (HTTP 407 Proxy Authentication Required): check the proxy \
                 username and password"
            )));
        }
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            let text = text.trim();
            return Err(Failure {
                rejected: true,
                message: format!(
                    "HTTP {status}{}",
                    if text.is_empty() { String::new() } else { format!(": {}", truncate(text)) }
                ),
            });
        }
        Ok((body, octet))
    }

    async fn get(self: Arc<Self>, sid: Option<String>) -> Result<Vec<Packet>, Failure> {
        let url = self.url(true, sid.as_deref());
        let resp = self.request(reqwest::Method::GET, &url).send().await.map_err(|e| self.failure(&e))?;
        let (body, octet) = self.read(&url, resp).await?;
        decode_payload(self.cfg.eio, &body, octet).map_err(|e| {
            let message = if self.cfg.eio != 3 && looks_like_eio3(&body) {
                "The server answered in the Engine.IO 3 format of Socket.IO v2 servers: choose protocol v2".to_string()
            } else {
                format!("Unexpected polling response: {e}")
            };
            Failure { rejected: false, message }
        })
    }

    async fn post(self: Arc<Self>, sid: String, body: String) -> Result<(), Failure> {
        let url = self.url(true, Some(&sid));
        let resp = self
            .request(reqwest::Method::POST, &url)
            .header(http::header::CONTENT_TYPE, "text/plain;charset=UTF-8")
            .body(body)
            .send()
            .await
            .map_err(|e| self.failure(&e))?;
        self.read(&url, resp).await.map(drop)
    }

    async fn ws_connect(self: Arc<Self>, sid: Option<String>) -> Result<WsStream, Failure> {
        let url = self.url(false, sid.as_deref());
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| Failure::network(format!("Invalid Socket.IO URL \"{url}\": {e}")))?;
        for (k, v) in &self.cfg.headers {
            request.headers_mut().append(k.clone(), v.clone());
        }
        let mut http_url = url.clone();
        let _ = http_url.set_scheme(if url.scheme() == "wss" { "https" } else { "http" });
        if let Some(c) = self.cookie_header(&http_url).and_then(|c| HeaderValue::from_str(&c).ok()) {
            request.headers_mut().insert(http::header::COOKIE, c);
        }
        let connector = match &self.tls {
            Some(c) => tokio_tungstenite::Connector::Rustls(c.clone()),
            None => tokio_tungstenite::Connector::Plain,
        };
        let cfg = &self.cfg;
        let (ws, resp) =
            merpati_http::ws::connect_via_proxy(request, &cfg.proxy, &cfg.tls, cfg.verify_ssl, Some(connector))
                .await
                .map_err(|e| Failure { rejected: matches!(e, tungstenite::Error::Http(_)), message: describe_ws(&e) })?;
        self.store_cookies(&http_url, resp.headers());
        Ok(ws)
    }
}

/// Decodes a `Transfer-Encoding: chunked` body (tungstenite keeps it raw);
/// other bodies are returned as they are.
fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") else { return body.to_vec() };
        let Some(size) = std::str::from_utf8(&rest[..eol]).ok().and_then(|s| usize::from_str_radix(s.trim(), 16).ok())
        else {
            return body.to_vec();
        };
        rest = &rest[eol + 2..];
        if size == 0 {
            return out;
        }
        let Some(chunk) = rest.get(..size) else { return body.to_vec() };
        out.extend_from_slice(chunk);
        rest = rest.get(size + 2..).unwrap_or_default();
    }
}

fn looks_like_eio3(body: &[u8]) -> bool {
    let digits = body.iter().take_while(|b| b.is_ascii_digit()).count();
    digits > 0 && body.get(digits) == Some(&b':')
}

/// The server closed the connection without an HTTP response (e.g. a bare
/// WebSocket server receiving a polling request).
fn dropped_without_response(e: &reqwest::Error) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::UnexpectedEof
            )
        {
            return true;
        }
        if let Some(h) = err.downcast_ref::<hyper::Error>()
            && (h.is_incomplete_message() || h.is_closed() || h.is_parse())
        {
            return true;
        }
        cur = err.source();
    }
    false
}

/// Explains a reqwest failure caused by the proxy `proxy` (unreachable,
/// SOCKS or tunnel errors); `None` for other failures.
fn proxy_failure(e: &reqwest::Error, proxy: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        // reqwest connects to the proxy first: a refusal is the proxy's.
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::ConnectionRefused
            && e.is_connect()
        {
            return Some(format!("Could not connect to the proxy {proxy}: connection refused"));
        }
        parts.push(err.to_string());
        cur = err.source();
    }
    let lower = parts.join(": ").to_ascii_lowercase();
    if !(lower.contains("socks") || lower.contains("tunnel") || lower.contains("proxy")) {
        return None;
    }
    let detail = parts.last().cloned().unwrap_or_default();
    Some(format!("Proxy error ({proxy}): {detail}"))
}

/// The most specific message of a reqwest error chain.
fn describe_reqwest(e: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::ConnectionRefused
        {
            return "Connection refused: is the server running and the port correct?".into();
        }
        let s = err.to_string();
        if !s.is_empty() && !parts.iter().any(|p| p.contains(&s)) {
            parts.push(s);
        }
        cur = err.source();
    }
    parts.join(": ")
}

pub(crate) fn describe_ws(e: &tungstenite::Error) -> String {
    if let tungstenite::Error::Io(io) = e
        && let Some(p) = merpati_http::proxy::find_proxy_error(io)
    {
        return p.message.clone();
    }
    match e {
        tungstenite::Error::Http(resp) => {
            let body =
                resp.body().as_ref().map(|b| String::from_utf8_lossy(&dechunk(b)).into_owned()).unwrap_or_default();
            format!(
                "Handshake rejected with HTTP {}{}",
                resp.status(),
                if body.trim().is_empty() { String::new() } else { format!(": {}", truncate(body.trim())) }
            )
        }
        tungstenite::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => {
            "Connection refused: is the server running and the port correct?".into()
        }
        tungstenite::Error::Tls(t) => format!("TLS error: {t}"),
        tungstenite::Error::Url(u) => format!("Invalid URL: {u}"),
        other => other.to_string(),
    }
}

/// Adds advice for the usual causes of a refused handshake.
fn hint(mut msg: String, transport: SocketIoTransport, eio: u8) -> String {
    if msg.contains("Transport unknown") {
        match transport {
            SocketIoTransport::WebSocket => {
                msg.push_str(" (the server may not accept WebSocket connections: try the Auto or Polling transport)")
            }
            SocketIoTransport::Polling => msg.push_str(
                " (the server may not accept HTTP long-polling: try the Auto or WebSocket transport, or check the path)",
            ),
            SocketIoTransport::Auto => msg.push_str(" (check the Socket.IO path)"),
        }
    } else if msg.contains("Unsupported protocol version") && eio == 3 {
        msg.push_str(
            " (the server does not accept Socket.IO v2 clients: choose v3/v4, or start it with allowEIO3: true)",
        );
    }
    msg
}

// ---------------------------------------------------------------------------
// Engine task
// ---------------------------------------------------------------------------

pub(crate) enum EngineEvent {
    Open {
        sid: String,
        transport: ActiveTransport,
    },
    Message(Frame),
    /// The session now runs over WebSocket.
    Upgraded,
    /// A non-fatal problem (failed upgrade).
    Warning(String),
    /// The connection is over (last event). Not sent after [`EngineHandle::close`].
    Closed {
        reason: &'static str,
        error: Option<String>,
    },
}

enum EngineCmd {
    Send(Frame),
    Close,
}

/// Commands to a running engine. Dropping it closes the connection.
pub(crate) struct EngineHandle {
    tx: mpsc::UnboundedSender<EngineCmd>,
}

impl EngineHandle {
    pub fn send(&self, frame: Frame) {
        let _ = self.tx.send(EngineCmd::Send(frame));
    }

    /// Closes cleanly (pending messages are sent first); the event channel
    /// ends once done.
    pub fn close(&self) {
        let _ = self.tx.send(EngineCmd::Close);
    }
}

pub(crate) fn spawn(cfg: EngineConfig, events: mpsc::UnboundedSender<EngineEvent>) -> EngineHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run(cfg, rx, events));
    EngineHandle { tx }
}

async fn opt<T>(f: &mut Option<BoxFut<T>>) -> T {
    match f {
        Some(f) => f.as_mut().await,
        None => std::future::pending().await,
    }
}

async fn ws_next(ws: &mut Option<WsStream>) -> Option<Result<Message, tungstenite::Error>> {
    match ws {
        Some(w) => w.next().await,
        None => std::future::pending().await,
    }
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

type Opened = (Option<WsStream>, OpenInfo, Vec<Packet>);

async fn handshake(http: Arc<Http>) -> Result<Opened, String> {
    let (transport, eio) = (http.cfg.transport, http.cfg.eio);
    match transport {
        SocketIoTransport::Polling => poll_handshake(http).await.map_err(|e| hint(e.message, transport, eio)),
        SocketIoTransport::WebSocket => ws_handshake(http).await.map_err(|e| hint(e.message, transport, eio)),
        SocketIoTransport::Auto => match poll_handshake(http.clone()).await {
            Ok(v) => Ok(v),
            // Polling refused (e.g. a WebSocket-only server): try WebSocket
            // directly, unless the server gave another Engine.IO error.
            Err(p) if p.rejected && !p.engine_error_other_than_transport() => match ws_handshake(http).await {
                Ok(v) => Ok(v),
                Err(w) if w.message == p.message => Err(hint(p.message, transport, eio)),
                Err(w) => {
                    Err(hint(format!("{} (HTTP long-polling); {} (WebSocket)", p.message, w.message), transport, eio))
                }
            },
            Err(p) => Err(hint(p.message, transport, eio)),
        },
    }
}

async fn poll_handshake(http: Arc<Http>) -> Result<Opened, Failure> {
    let eio = http.cfg.eio;
    let mut packets = http.get(None).await.map_err(|mut e| {
        if e.rejected {
            e.message = format!("Handshake rejected with {}", e.message);
        }
        e
    })?;
    if packets.is_empty() {
        return Err(Failure::network("Empty handshake response".into()));
    }
    match packets.remove(0) {
        Packet::Open(json) => {
            let info = parse_open(eio, &json).map_err(Failure::network)?;
            Ok((None, info, packets))
        }
        other => Err(Failure::network(format!("Expected an Engine.IO open packet, got \"{}\"", encode_text(&other)))),
    }
}

async fn ws_handshake(http: Arc<Http>) -> Result<Opened, Failure> {
    let eio = http.cfg.eio;
    let mut ws = http.ws_connect(None).await?;
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => match decode_text(eio, t.as_str()) {
                Ok(Packet::Open(json)) => {
                    let info = parse_open(eio, &json).map_err(Failure::network)?;
                    return Ok((Some(ws), info, Vec::new()));
                }
                _ => {
                    return Err(Failure::network(format!(
                        "Expected an Engine.IO open packet, got \"{}\"",
                        truncate(t.as_str())
                    )));
                }
            },
            Some(Ok(Message::Close(_))) | None => {
                return Err(Failure::network("The server closed the connection during the handshake".into()));
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(Failure::network(describe_ws(&e))),
        }
    }
}

async fn run(cfg: EngineConfig, mut cmds: mpsc::UnboundedReceiver<EngineCmd>, ev: mpsc::UnboundedSender<EngineEvent>) {
    let http = match Http::new(cfg) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            let _ = ev.send(EngineEvent::Closed { reason: "connect failed", error: Some(e) });
            return;
        }
    };
    let hs = handshake(http.clone());
    tokio::pin!(hs);
    let opened = loop {
        tokio::select! {
            r = &mut hs => break r,
            cmd = cmds.recv() => match cmd {
                Some(EngineCmd::Send(_)) => {}
                Some(EngineCmd::Close) | None => return,
            }
        }
    };
    let (ws, info, early) = match opened {
        Ok(v) => v,
        Err(e) => {
            let _ = ev.send(EngineEvent::Closed { reason: "connect failed", error: Some(e) });
            return;
        }
    };
    let transport = if ws.is_some() { ActiveTransport::WebSocket } else { ActiveTransport::Polling };
    let _ = ev.send(EngineEvent::Open { sid: info.sid.clone(), transport });
    let eio = http.cfg.eio;
    let mut engine = Engine {
        eio,
        sid: info.sid.clone(),
        next_ping: (eio == 3).then(|| Instant::now() + info.ping_interval),
        info,
        http,
        ev,
        out: VecDeque::new(),
        get: None,
        post: None,
        ws,
        probe_conn: None,
        probe_ws: None,
        probe_deadline: None,
        pausing: false,
        last_seen: Instant::now(),
    };
    for p in early {
        if !engine.on_packet(p).await {
            return;
        }
    }
    engine.start_get();
    if engine.ws.is_none()
        && engine.http.cfg.transport == SocketIoTransport::Auto
        && engine.info.upgrades.iter().any(|u| u == "websocket")
    {
        engine.probe_conn = Some(Box::pin(engine.http.clone().ws_connect(Some(engine.sid.clone()))));
        engine.probe_deadline = Some(Instant::now() + PROBE_TIMEOUT);
    }
    engine.run(cmds).await;
}

struct Engine {
    http: Arc<Http>,
    ev: mpsc::UnboundedSender<EngineEvent>,
    eio: u8,
    sid: String,
    info: OpenInfo,
    /// Packets waiting to be written.
    out: VecDeque<Packet>,
    /// In-flight polling requests (polling transport only).
    get: Option<BoxFut<Result<Vec<Packet>, Failure>>>,
    post: Option<BoxFut<Result<(), Failure>>>,
    /// The WebSocket once it carries the session.
    ws: Option<WsStream>,
    /// Upgrade: connecting the probe socket, then waiting for `3probe` on it.
    probe_conn: Option<BoxFut<Result<WsStream, Failure>>>,
    probe_ws: Option<WsStream>,
    probe_deadline: Option<Instant>,
    /// `3probe` received: polling is paused until the in-flight requests end.
    pausing: bool,
    last_seen: Instant,
    /// Protocol 3: when to send the next ping.
    next_ping: Option<Instant>,
}

impl Engine {
    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<EngineCmd>) {
        loop {
            if self.pausing && self.get.is_none() && self.post.is_none() && !self.finish_upgrade().await {
                return;
            }
            // Packets queued while polling was paused (e.g. an abandoned upgrade).
            if !self.pausing && !self.out.is_empty() && !self.flush().await {
                return;
            }
            let ping_deadline = self.last_seen + self.info.ping_interval + self.info.ping_timeout;
            let next_ping = self.next_ping;
            let probe_deadline = self.probe_deadline;
            tokio::select! {
                r = opt(&mut self.get) => {
                    self.get = None;
                    match r {
                        Ok(packets) => {
                            for p in packets {
                                if !self.on_packet(p).await {
                                    return;
                                }
                            }
                            self.start_get();
                        }
                        Err(e) => return self.fail("transport error", format!("Polling request failed: {}", e.message)),
                    }
                }
                r = opt(&mut self.post) => {
                    self.post = None;
                    if let Err(e) = r {
                        return self.fail("transport error", format!("Sending failed: {}", e.message));
                    }
                    if !self.flush().await {
                        return;
                    }
                }
                msg = ws_next(&mut self.ws) => {
                    let packet = match msg {
                        Some(Ok(Message::Text(t))) => decode_text(self.eio, t.as_str()),
                        Some(Ok(Message::Binary(b))) => decode_ws_binary(self.eio, b.to_vec()),
                        Some(Ok(Message::Close(_))) | None => return self.closed("transport close", None),
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => return self.closed("transport error", Some(describe_ws(&e))),
                    };
                    match packet {
                        Ok(p) => {
                            if !self.on_packet(p).await {
                                return;
                            }
                        }
                        Err(e) => self.warn(format!("Ignored malformed packet: {e}")),
                    }
                }
                r = opt(&mut self.probe_conn) => {
                    self.probe_conn = None;
                    match r {
                        Ok(mut ws) => match ws.send(Message::text("2probe")).await {
                            Ok(()) => self.probe_ws = Some(ws),
                            Err(e) => self.upgrade_failed(describe_ws(&e)),
                        },
                        Err(e) => self.upgrade_failed(e.message),
                    }
                }
                msg = ws_next(&mut self.probe_ws) => match msg {
                    Some(Ok(Message::Text(t))) if t.as_str() == "3probe" => self.pausing = true,
                    Some(Ok(Message::Close(_))) | None => self.upgrade_failed("the server closed the probe connection".into()),
                    Some(Err(e)) => self.upgrade_failed(describe_ws(&e)),
                    Some(Ok(_)) => {}
                },
                _ = sleep_opt(probe_deadline) => {
                    let why = if self.pausing { "the pending polling request did not end" } else { "no answer to the probe" };
                    self.upgrade_failed(why.into());
                }
                cmd = cmds.recv() => match cmd {
                    Some(EngineCmd::Send(frame)) => {
                        self.out.push_back(Packet::Message(frame));
                        if !self.flush().await {
                            return;
                        }
                    }
                    Some(EngineCmd::Close) | None => return self.close().await,
                },
                _ = tokio::time::sleep_until(ping_deadline) => {
                    let mut msg = if self.eio == 3 {
                        "No pong from the server: connection lost".to_string()
                    } else {
                        "No ping from the server: connection lost".to_string()
                    };
                    if self.eio != 3 && !self.info.has_max_payload {
                        msg.push_str(" (if this is a Socket.IO v2 server, choose protocol v2)");
                    }
                    return self.fail("ping timeout", msg);
                }
                _ = sleep_opt(next_ping) => {
                    self.next_ping = None;
                    self.out.push_back(Packet::Ping(String::new()));
                    if !self.flush().await {
                        return;
                    }
                }
            }
        }
    }

    fn warn(&self, msg: String) {
        let _ = self.ev.send(EngineEvent::Warning(msg));
    }

    fn upgrade_failed(&mut self, why: String) {
        self.probe_conn = None;
        self.probe_ws = None;
        self.probe_deadline = None;
        if self.pausing {
            self.pausing = false;
            self.start_get();
        }
        self.warn(format!("WebSocket upgrade failed ({why}); staying on HTTP long-polling"));
    }

    /// Reports the end of the connection.
    fn closed(&self, reason: &'static str, error: Option<String>) {
        let _ = self.ev.send(EngineEvent::Closed { reason, error });
    }

    fn fail(&self, reason: &'static str, error: String) {
        self.closed(reason, Some(error));
    }

    /// Starts the next long-polling `GET` when polling is active.
    fn start_get(&mut self) {
        if self.ws.is_none() && !self.pausing && self.get.is_none() {
            self.get = Some(Box::pin(self.http.clone().get(Some(self.sid.clone()))));
        }
    }

    /// Writes queued packets: directly over WebSocket, or as one `POST`
    /// (split at `maxPayload`) when none is in flight. Returns `false` after
    /// a fatal write error (already reported).
    async fn flush(&mut self) -> bool {
        if self.out.is_empty() || self.pausing {
            return true;
        }
        if let Some(ws) = &mut self.ws {
            while let Some(p) = self.out.pop_front() {
                if let Err(e) = ws.send(encode_ws(self.eio, &p)).await {
                    self.closed("transport error", Some(format!("Sending failed: {}", describe_ws(&e))));
                    return false;
                }
            }
            return true;
        }
        if self.post.is_some() {
            return true;
        }
        let body = self.take_payload();
        self.post = Some(Box::pin(self.http.clone().post(self.sid.clone(), body)));
        true
    }

    fn take_payload(&mut self) -> String {
        let mut batch = Vec::new();
        let mut size = 0;
        while let Some(p) = self.out.front() {
            let len = encode_polling_packet(self.eio, p).len() + 1;
            if !batch.is_empty() && size + len > self.info.max_payload {
                break;
            }
            size += len;
            batch.extend(self.out.pop_front());
        }
        encode_payload(self.eio, &batch)
    }

    /// Handles one received packet; `false` when the session is over.
    async fn on_packet(&mut self, p: Packet) -> bool {
        self.last_seen = Instant::now();
        match p {
            Packet::Close => {
                self.closed("transport close", None);
                false
            }
            Packet::Ping(data) => {
                self.out.push_back(Packet::Pong(data));
                self.flush().await
            }
            Packet::Pong(_) => {
                if self.eio == 3 {
                    self.next_ping = Some(Instant::now() + self.info.ping_interval);
                }
                true
            }
            Packet::Message(frame) => {
                let _ = self.ev.send(EngineEvent::Message(frame));
                true
            }
            Packet::Open(_) | Packet::Upgrade | Packet::Noop => true,
        }
    }

    /// Polling is paused and idle: switch to the probed WebSocket.
    async fn finish_upgrade(&mut self) -> bool {
        self.pausing = false;
        self.probe_deadline = None;
        let Some(mut ws) = self.probe_ws.take() else {
            self.start_get();
            return self.flush().await;
        };
        if let Err(e) = ws.send(Message::text("5")).await {
            self.upgrade_failed(describe_ws(&e));
            return self.flush().await;
        }
        self.ws = Some(ws);
        let _ = self.ev.send(EngineEvent::Upgraded);
        self.flush().await
    }

    /// Client-side close: queued packets are written first.
    async fn close(mut self) {
        self.probe_conn = None;
        self.probe_ws = None;
        self.pausing = false;
        let _ = tokio::time::timeout(CLOSE_GRACE, async {
            if let Some(ws) = &mut self.ws {
                while let Some(p) = self.out.pop_front() {
                    if ws.send(encode_ws(self.eio, &p)).await.is_err() {
                        return;
                    }
                }
                let _ = ws.close(None).await;
                while let Some(Ok(m)) = ws.next().await {
                    if matches!(m, Message::Close(_)) {
                        break;
                    }
                }
            } else {
                if let Some(post) = self.post.take() {
                    let _ = post.await;
                }
                self.out.push_back(Packet::Close);
                while !self.out.is_empty() {
                    let body = self.take_payload();
                    if self.http.clone().post(self.sid.clone(), body).await.is_err() {
                        return;
                    }
                }
            }
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(s: &str) -> Packet {
        Packet::Message(Frame::Text(s.into()))
    }

    #[test]
    fn payloads_v4() {
        let packets = vec![Packet::Ping(String::new()), msg("2[\"a\"]"), Packet::Message(Frame::Binary(vec![1, 2, 3]))];
        let enc = encode_payload(4, &packets);
        assert_eq!(enc, "2\u{1e}42[\"a\"]\u{1e}bAQID");
        assert_eq!(decode_payload(4, enc.as_bytes(), false).unwrap(), packets);
        assert_eq!(decode_payload(4, b"6", false).unwrap(), vec![Packet::Noop]);
        assert!(decode_payload(4, b"9x", false).is_err());
    }

    #[test]
    fn payloads_v3() {
        // Lengths count UTF-16 units: "4é😀" is 1 + 1 + 2.
        let packets = vec![msg("é😀"), Packet::Pong("probe".into()), Packet::Message(Frame::Binary(vec![1, 2, 3]))];
        let enc = encode_payload(3, &packets);
        assert_eq!(enc, "4:4é😀6:3probe6:b4AQID");
        assert_eq!(decode_payload(3, enc.as_bytes(), false).unwrap(), packets);
        assert_eq!(decode_payload(3, b"0:", false).unwrap(), vec![]);
        assert!(decode_payload(3, b"9:40", false).is_err());
        assert!(decode_payload(3, b"x:40", false).is_err());
        // Binary payload format: string packet "40", binary packet [4, 1, 2].
        let bin = [0u8, 2, 0xFF, b'4', b'0', 1, 3, 0xFF, 4, 1, 2];
        assert_eq!(decode_payload(3, &bin, true).unwrap(), vec![msg("0"), Packet::Message(Frame::Binary(vec![1, 2]))]);
    }

    #[test]
    fn websocket_frames() {
        assert_eq!(decode_ws_binary(4, vec![1, 2]).unwrap(), Packet::Message(Frame::Binary(vec![1, 2])));
        assert_eq!(decode_ws_binary(3, vec![4, 1, 2]).unwrap(), Packet::Message(Frame::Binary(vec![1, 2])));
        assert!(decode_ws_binary(3, vec![]).is_err());
        assert_eq!(encode_ws(3, &Packet::Message(Frame::Binary(vec![9]))), Message::binary(vec![4, 9]));
        assert_eq!(encode_ws(4, &Packet::Message(Frame::Binary(vec![9]))), Message::binary(vec![9]));
        assert_eq!(encode_ws(4, &Packet::Ping("probe".into())), Message::text("2probe"));
        assert_eq!(decode_text(3, "b4aGk=").unwrap(), Packet::Message(Frame::Binary(b"hi".to_vec())));
        assert_eq!(decode_text(4, "baGk=").unwrap(), Packet::Message(Frame::Binary(b"hi".to_vec())));
    }

    #[test]
    fn open_packets() {
        let o = parse_open(
            4,
            r#"{"sid":"a","upgrades":["websocket"],"pingInterval":300,"pingTimeout":200,"maxPayload":5}"#,
        )
        .unwrap();
        assert_eq!(o.sid, "a");
        assert_eq!(o.upgrades, vec!["websocket".to_string()]);
        assert_eq!((o.ping_interval, o.ping_timeout), (Duration::from_millis(300), Duration::from_millis(200)));
        assert_eq!((o.max_payload, o.has_max_payload), (5, true));
        let o = parse_open(3, r#"{"sid":"b","upgrades":[],"pingInterval":25000,"pingTimeout":5000}"#).unwrap();
        assert!(!o.has_max_payload && o.upgrades.is_empty());
        assert!(parse_open(4, "{}").is_err());
        assert!(parse_open(4, "nope").is_err());
        assert!(looks_like_eio3(b"96:0{\"sid\""));
        assert_eq!(dechunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"), b"hello world");
        assert_eq!(dechunk(b"{\"code\":0}"), b"{\"code\":0}");
        assert_eq!(dechunk(b"zz\r\nx"), b"zz\r\nx");
        let f = |m: &str| Failure { rejected: true, message: m.into() };
        assert!(
            f(r#"Handshake rejected with HTTP 400: {"code":5,"message":"Unsupported protocol version"}"#)
                .engine_error_other_than_transport()
        );
        assert!(
            !f(r#"Handshake rejected with HTTP 400: {"code":0,"message":"Transport unknown"}"#)
                .engine_error_other_than_transport()
        );
        assert!(!f("Handshake rejected with HTTP 404 Not Found: nope").engine_error_other_than_transport());
        assert!(!looks_like_eio3(b"0{\"sid\""));
    }
}
