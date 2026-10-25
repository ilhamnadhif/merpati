//! Socket.IO client sessions (PRD FR-X4): connect to one namespace, report
//! every incoming event, emit events with JSON and binary arguments and
//! optional acknowledgements, and acknowledge events the server emits with a
//! callback.
//!
//! The session mirrors `merpati_http::ws::WsSession`: [`SocketIoSession::connect`]
//! starts the connection on a tokio runtime, every state change and message is
//! reported through a callback, and the returned handle only queues commands.
//! Dropping the handle disconnects.
//!
//! Protocol notes:
//! - Transports ([`SocketIoTransport`]): `Auto` connects with HTTP
//!   long-polling and upgrades to WebSocket when the server offers it, like
//!   the official client (and falls back to a direct WebSocket when the server
//!   refuses polling); `WebSocket` and `Polling` use only one transport.
//!   Headers, cookies (from the jar and a `Cookie` header), query parameters
//!   and auth apply to every request of every transport.
//! - Versions ([`SocketIoVersion`]): `V4` speaks Engine.IO 4 / Socket.IO
//!   protocol 5 (Socket.IO v3 and v4 servers); `V2` speaks Engine.IO 3 /
//!   protocol 4 (Socket.IO v2, or v3/v4 servers with `allowEIO3`). In `V2` the
//!   client sends the pings, the root namespace is joined by the server, and
//!   the auth payload (which v2 does not have) is sent as query parameters.
//! - A missing ping (v4) or pong (v2) for `pingInterval + pingTimeout` ends
//!   the session ("ping timeout").
//! - Binary attachments of incoming events and acks are delivered as base64
//!   strings in place of their placeholders. Outgoing arguments ([`EmitArg`],
//!   see [`SocketIoSession::emit_args`] and [`SocketIoSession::ack_args`]) may
//!   hold binary data anywhere inside: the packet is then sent as
//!   BINARY_EVENT / BINARY_ACK with `{"_placeholder":true,"num":n}` in the
//!   JSON, followed by the attachments as Engine.IO binary messages
//!   (WebSocket binary frames, with the leading `0x04` byte in Engine.IO 3;
//!   `b<base64>` / `b4<base64>` packets in polling payloads). Outgoing
//!   events report binary data as base64 strings too.
//! - Events the server emits with an acknowledgement callback are listed by
//!   [`SocketIoSession::pending_acks`] until answered with
//!   [`SocketIoSession::ack`].
//! - A server cannot exhaust memory: a polling response over
//!   [`MAX_POLLING_BODY`], a WebSocket message over 64 MiB, or a packet with
//!   more than [`MAX_ATTACHMENTS`] attachments or [`MAX_ATTACHMENT_BYTES`] of
//!   them ends the session with an error.

mod args;
mod engine;
mod packet;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use merpati_core::auth::AuthType;
use merpati_core::protocols::SocketIoRequest;
pub use merpati_core::protocols::{SocketIoTransport, SocketIoVersion};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub use args::{ArgError, EmitArg, message_args, parse_args, resolve_arg, resolve_args};
pub use engine::{ActiveTransport, MAX_POLLING_BODY};
use engine::{EngineEvent, EngineHandle, Frame};
use packet::{
    ACK, BINARY_ACK, BINARY_EVENT, CONNECT, CONNECT_ERROR, DISCONNECT, EVENT, Packet, decode_packet,
    encode_data_packet, encode_packet, fill_placeholders,
};

/// Event names the Socket.IO client API refuses to emit.
const RESERVED_EVENTS: [&str; 6] =
    ["connect", "connect_error", "disconnect", "disconnecting", "newListener", "removeListener"];
/// How long a client disconnect may take.
const DISCONNECT_GRACE: Duration = Duration::from_secs(3);
/// Most binary attachments an incoming packet may announce; more ends the
/// session (the count comes from the server).
pub const MAX_ATTACHMENTS: usize = 10_000;
/// Largest total size of the binary attachments of one incoming packet;
/// more ends the session instead of filling memory.
pub const MAX_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;

/// A cookie store used for every request of a session, e.g. the workspace
/// jar (`merpati_http::CookieJar`) so that cookies are shared with HTTP
/// requests and shown in the cookie manager.
#[derive(Clone)]
pub struct SharedCookies(pub Arc<dyn reqwest::cookie::CookieStore>);

impl SharedCookies {
    pub fn new<C: reqwest::cookie::CookieStore + 'static>(store: Arc<C>) -> Self {
        SharedCookies(store)
    }
}

impl std::fmt::Debug for SharedCookies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedCookies(..)")
    }
}

impl PartialEq for SharedCookies {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SocketIoConfig {
    /// `http://host:3000`, `https://...`, `ws://...` or `wss://...`. Only
    /// scheme, host, port and query are used: the path comes from `path`.
    pub url: String,
    /// Engine.IO handshake path, default `/socket.io/` (a trailing slash is
    /// added like the JavaScript client does).
    pub path: String,
    /// Namespace, default `/`.
    pub namespace: String,
    /// Extra HTTP headers on every polling request and the WebSocket
    /// handshake. A `Cookie` header is merged with the jar's cookies.
    pub headers: Vec<(String, String)>,
    /// Extra query parameters on every request.
    pub query: Vec<(String, String)>,
    /// Payload of the namespace CONNECT packet (`socket.handshake.auth` on
    /// the server). Must be a JSON object. With [`SocketIoVersion::V2`] its
    /// entries are sent as query parameters instead.
    pub auth: Option<Value>,
    pub verify_ssl: bool,
    /// Extra trusted roots and client certificate for `https`/`wss`.
    pub tls: merpati_http::tls::TlsSetup,
    /// Proxy settings for polling and WebSocket alike, applied like the HTTP
    /// engine does (see `merpati_http::proxy`); the default connects directly.
    pub proxy: merpati_core::settings::ProxySettings,
    /// Covers the Engine.IO handshake and the namespace CONNECT reply.
    pub connect_timeout: Duration,
    pub transport: SocketIoTransport,
    pub version: SocketIoVersion,
    /// Cookie store for the session; `None` = a fresh jar per session.
    pub cookies: Option<SharedCookies>,
    /// Answers every acknowledgement the server asks for with these
    /// arguments right after reporting the event (used by the Collection
    /// Runner). `None` keeps them in [`SocketIoSession::pending_acks`].
    pub auto_ack: Option<Vec<EmitArg>>,
}

impl Default for SocketIoConfig {
    fn default() -> Self {
        SocketIoConfig {
            url: String::new(),
            path: "/socket.io/".into(),
            namespace: "/".into(),
            headers: Vec::new(),
            query: Vec::new(),
            auth: None,
            verify_ssl: true,
            tls: merpati_http::tls::TlsSetup::default(),
            proxy: Default::default(),
            connect_timeout: Duration::from_secs(20),
            transport: SocketIoTransport::Auto,
            version: SocketIoVersion::V4,
            cookies: None,
            auto_ack: None,
        }
    }
}

impl SocketIoConfig {
    /// Builds a config from a saved request. Every string goes through
    /// `resolve` (variable substitution); disabled headers and params are
    /// skipped. Like `io("http://host/admin")` in the JavaScript client, a URL
    /// path is used as the namespace when `namespace` is empty. Bearer, Basic
    /// and API-key auth become an `Authorization` header (or the API-key
    /// header / query parameter); other auth types are ignored. An
    /// `auth_payload` that is not valid JSON is kept as a string so that
    /// connecting reports the problem.
    pub fn from_request(
        req: &SocketIoRequest,
        resolve: &dyn Fn(&str) -> String,
        default_verify_ssl: bool,
    ) -> SocketIoConfig {
        let url = resolve(&req.url).trim().to_string();
        let mut namespace = resolve(&req.namespace).trim().to_string();
        if namespace.is_empty() {
            namespace = url_path(&url).unwrap_or_default();
        }
        let path = resolve(&req.path).trim().to_string();
        let mut headers: Vec<(String, String)> = req
            .headers
            .iter()
            .filter(|kv| kv.enabled && !kv.key.trim().is_empty())
            .map(|kv| (resolve(&kv.key).trim().to_string(), resolve(&kv.value)))
            .collect();
        let mut query: Vec<(String, String)> = req
            .params
            .iter()
            .filter(|kv| kv.enabled && !kv.key.trim().is_empty())
            .map(|kv| (resolve(&kv.key).trim().to_string(), resolve(&kv.value)))
            .collect();
        let auth = req.auth.map_strings(resolve);
        match auth.kind {
            AuthType::Bearer => {
                let token = auth.get_string("token");
                if !token.is_empty() {
                    headers.push(("Authorization".into(), format!("Bearer {token}")));
                }
            }
            AuthType::Basic => {
                let (user, pass) = (auth.get_string("username"), auth.get_string("password"));
                if !user.is_empty() || !pass.is_empty() {
                    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
                    headers.push(("Authorization".into(), format!("Basic {encoded}")));
                }
            }
            AuthType::ApiKey => {
                let (key, value) = (auth.get_string("key"), auth.get_string("value"));
                if !key.is_empty() {
                    if auth.get_string("in") == "query" {
                        query.push((key, value));
                    } else {
                        headers.push((key, value));
                    }
                }
            }
            _ => {}
        }
        let payload = resolve(&req.auth_payload);
        let payload = payload.trim();
        let auth_payload = (!payload.is_empty())
            .then(|| serde_json::from_str(payload).unwrap_or_else(|_| Value::String(payload.to_string())));
        SocketIoConfig {
            url,
            path: if path.is_empty() { "/socket.io/".into() } else { path },
            namespace: normalize_namespace(&namespace),
            headers,
            query,
            auth: auth_payload,
            verify_ssl: req.verify_ssl.unwrap_or(default_verify_ssl),
            tls: merpati_http::tls::TlsSetup::default(),
            proxy: Default::default(),
            connect_timeout: SocketIoConfig::default().connect_timeout,
            transport: req.transport,
            version: req.version,
            cookies: None,
            auto_ack: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SocketIoEvent {
    Connecting,
    /// The namespace accepted the connection; `sid` is the socket id.
    Connected {
        sid: String,
        namespace: String,
    },
    /// An event received from the server, or one we emitted (`Outgoing`,
    /// reported when handed to the transport).
    Event {
        name: String,
        args: Vec<Value>,
        direction: Direction,
        at_ms: i64,
    },
    /// The server acknowledged the emit that returned `id`.
    Ack {
        id: u64,
        args: Vec<Value>,
        at_ms: i64,
    },
    Error(String),
    /// The session is over (always the last event). Reasons follow the
    /// JavaScript client: `io server disconnect`, `io client disconnect`,
    /// `transport close`, `transport error`, `ping timeout`, plus
    /// `connect failed`, `connect error` (namespace refused) and `cancelled`.
    Disconnected {
        reason: String,
    },
}

/// An event the server emitted with an acknowledgement callback, waiting for
/// [`SocketIoSession::ack`]. `id` is the server's ack id (unrelated to the
/// ids returned by [`SocketIoSession::emit`]); `at_ms` equals the `at_ms` of
/// the matching [`SocketIoEvent::Event`].
#[derive(Clone, Debug, PartialEq)]
pub struct AckRequest {
    pub id: u64,
    pub event: String,
    pub args: Vec<Value>,
    pub at_ms: i64,
}

struct Emit {
    event: String,
    args: Vec<EmitArg>,
    ack: Option<u64>,
}

enum Command {
    Emit(Emit),
    Ack(u64, Vec<EmitArg>),
    Disconnect,
}

#[derive(Default)]
struct Shared {
    pending_acks: Mutex<Vec<AckRequest>>,
    transport: Mutex<Option<ActiveTransport>>,
}

/// Handle to a running Socket.IO session. Dropping it disconnects.
pub struct SocketIoSession {
    tx: mpsc::UnboundedSender<Command>,
    next_ack: AtomicU64,
    shared: Arc<Shared>,
}

impl SocketIoSession {
    /// Connects in the background on `rt`; every state change and message is
    /// reported through `on_event` (called from a runtime thread). Emits
    /// issued before the namespace is connected are queued.
    pub fn connect(
        rt: &tokio::runtime::Handle,
        cfg: SocketIoConfig,
        on_event: impl Fn(SocketIoEvent) + Send + Sync + 'static,
    ) -> SocketIoSession {
        let (tx, rx) = mpsc::unbounded_channel();
        let on_event: OnEvent = Arc::new(on_event);
        let shared = Arc::new(Shared::default());
        rt.spawn(run(cfg, rx, on_event, shared.clone()));
        SocketIoSession { tx, next_ack: AtomicU64::new(0), shared }
    }

    /// Queues an event with JSON arguments. With `want_ack` the returned id
    /// matches the later [`SocketIoEvent::Ack`]. Returns `None` without
    /// `want_ack` or when the session has ended. Invalid (empty or reserved)
    /// event names are reported as [`SocketIoEvent::Error`].
    pub fn emit(&self, event: &str, args: Vec<Value>, want_ack: bool) -> Option<u64> {
        self.emit_args(event, args.into_iter().map(EmitArg::Json).collect(), want_ack)
    }

    /// Like [`emit`](Self::emit), with arguments that may hold binary data
    /// (sent as a BINARY_EVENT with attachments).
    pub fn emit_args(&self, event: &str, args: Vec<EmitArg>, want_ack: bool) -> Option<u64> {
        let ack = want_ack.then(|| self.next_ack.fetch_add(1, Ordering::Relaxed));
        self.tx.send(Command::Emit(Emit { event: event.to_string(), args, ack })).ok()?;
        ack
    }

    /// Answers the server's acknowledgement request `id` (see
    /// [`pending_acks`](Self::pending_acks)) with JSON `args`. Unknown ids
    /// are reported as [`SocketIoEvent::Error`]. Returns `false` when the
    /// session has ended.
    pub fn ack(&self, id: u64, args: Vec<Value>) -> bool {
        self.ack_args(id, args.into_iter().map(EmitArg::Json).collect())
    }

    /// Like [`ack`](Self::ack), with arguments that may hold binary data
    /// (sent as a BINARY_ACK with attachments).
    pub fn ack_args(&self, id: u64, args: Vec<EmitArg>) -> bool {
        self.tx.send(Command::Ack(id, args)).is_ok()
    }

    /// Server events still waiting for an acknowledgement, oldest first.
    pub fn pending_acks(&self) -> Vec<AckRequest> {
        self.shared.pending_acks.lock().map(|p| p.clone()).unwrap_or_default()
    }

    /// The transport carrying the session (`None` before the handshake and
    /// after the session ended). Changes from `Polling` to `WebSocket` when
    /// an `Auto` session upgrades.
    pub fn transport(&self) -> Option<ActiveTransport> {
        self.shared.transport.lock().ok().and_then(|t| *t)
    }

    /// Leaves the namespace and closes the connection.
    pub fn disconnect(&self) {
        let _ = self.tx.send(Command::Disconnect);
    }

    /// `false` once the session has ended (after [`SocketIoEvent::Disconnected`]).
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }
}

impl Drop for SocketIoSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Disconnect);
    }
}

type OnEvent = Arc<dyn Fn(SocketIoEvent) + Send + Sync>;

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn normalize_namespace(ns: &str) -> String {
    let ns = ns.trim();
    if ns.is_empty() {
        "/".into()
    } else if ns.starts_with('/') {
        ns.to_string()
    } else {
        format!("/{ns}")
    }
}

/// `/socket.io` -> `/socket.io/`, empty -> `/socket.io/`.
fn normalize_path(path: &str) -> String {
    let path = path.trim();
    let path = if path.is_empty() { "/socket.io" } else { path };
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else if trimmed.starts_with('/') {
        format!("{trimmed}/")
    } else {
        format!("/{trimmed}/")
    }
}

fn with_scheme(raw: &str) -> String {
    if raw.contains("://") { raw.to_string() } else { format!("http://{raw}") }
}

/// The non-root path of a URL, used as namespace.
fn url_path(raw: &str) -> Option<String> {
    let url = url::Url::parse(&with_scheme(raw)).ok()?;
    let path = url.path().trim_end_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

/// Query keys the Engine.IO client sets itself.
fn reserved_query_key(k: &str) -> bool {
    matches!(k, "EIO" | "transport" | "sid" | "t" | "b64" | "j")
}

/// Auth payload entries as query parameters (Socket.IO v2).
fn auth_query(auth: Option<&Value>) -> Vec<(String, String)> {
    let Some(Value::Object(o)) = auth else { return Vec::new() };
    o.iter()
        .map(|(k, v)| {
            let v = match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            (k.clone(), v)
        })
        .collect()
}

/// The Engine.IO endpoint `http(s)://host:port{path}?{query}`, without the
/// Engine.IO parameters (added per request).
fn base_url(cfg: &SocketIoConfig) -> Result<url::Url, String> {
    let raw = cfg.url.trim();
    if raw.is_empty() {
        return Err("Socket.IO URL is empty".into());
    }
    let mut url = url::Url::parse(&with_scheme(raw)).map_err(|e| format!("Invalid Socket.IO URL \"{raw}\": {e}"))?;
    let scheme = match url.scheme() {
        "http" | "ws" => "http",
        "https" | "wss" => "https",
        other => {
            return Err(format!(
                "Unsupported Socket.IO URL scheme \"{other}://\" (use http://, https://, ws:// or wss://)"
            ));
        }
    };
    if url.set_scheme(scheme).is_err() {
        // `url` refuses ws -> http in place; rebuild.
        let rest = &url.as_str()[url.scheme().len()..];
        url =
            url::Url::parse(&format!("{scheme}{rest}")).map_err(|e| format!("Invalid Socket.IO URL \"{raw}\": {e}"))?;
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!("Invalid Socket.IO URL \"{raw}\": missing host"));
    }
    url.set_path(&normalize_path(&cfg.path));
    url.set_fragment(None);
    let existing: Vec<(String, String)> =
        url.query_pairs().into_owned().filter(|(k, _)| !reserved_query_key(k)).collect();
    url.set_query(None);
    let mut pairs: Vec<(String, String)> = existing;
    pairs.extend(cfg.query.iter().filter(|(k, _)| !reserved_query_key(k)).cloned());
    if cfg.version == SocketIoVersion::V2 {
        pairs.extend(auth_query(cfg.auth.as_ref()).into_iter().filter(|(k, _)| !reserved_query_key(k)));
    }
    if !pairs.is_empty() {
        let mut q = url.query_pairs_mut();
        for (k, v) in &pairs {
            q.append_pair(k, v);
        }
    }
    Ok(url)
}

fn engine_config(cfg: &SocketIoConfig) -> Result<engine::EngineConfig, String> {
    let base = base_url(cfg)?;
    let mut headers = Vec::new();
    let mut user_cookie: Option<String> = None;
    for (k, v) in &cfg.headers {
        let (Ok(name), Ok(value)) = (http::HeaderName::from_bytes(k.trim().as_bytes()), http::HeaderValue::from_str(v))
        else {
            return Err(format!("Invalid header \"{k}\""));
        };
        if name == http::header::COOKIE {
            let v = v.trim();
            user_cookie = Some(match user_cookie {
                Some(prev) => format!("{prev}; {v}"),
                None => v.to_string(),
            });
        } else {
            headers.push((name, value));
        }
    }
    let cookies: Arc<dyn reqwest::cookie::CookieStore> = match &cfg.cookies {
        Some(c) => c.0.clone(),
        None => Arc::new(reqwest::cookie::Jar::default()),
    };
    Ok(engine::EngineConfig {
        base,
        headers,
        user_cookie,
        cookies,
        verify_ssl: cfg.verify_ssl,
        tls: cfg.tls.clone(),
        proxy: cfg.proxy.clone(),
        transport: cfg.transport,
        eio: if cfg.version == SocketIoVersion::V2 { 3 } else { 4 },
    })
}

// ---------------------------------------------------------------------------
// Session task (Socket.IO layer)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the Engine.IO handshake.
    Open,
    /// Waiting for the namespace CONNECT reply.
    Namespace,
    Connected,
}

struct Session {
    engine: EngineHandle,
    on_event: OnEvent,
    shared: Arc<Shared>,
    namespace: String,
    v2: bool,
    auth: Option<Value>,
    /// Socket.IO v2 CONNECT query (`/ns?<query>,`).
    connect_query: String,
    engine_sid: String,
    phase: Phase,
    pending: Vec<Emit>,
    /// A binary packet waiting for its attachments.
    partial: Option<Partial>,
    auto_ack: Option<Vec<EmitArg>>,
}

/// A binary packet, its attachments so far and their total size.
struct Partial {
    packet: Packet,
    attachments: Vec<Vec<u8>>,
    bytes: usize,
}

async fn run(cfg: SocketIoConfig, mut rx: mpsc::UnboundedReceiver<Command>, on_event: OnEvent, shared: Arc<Shared>) {
    on_event(SocketIoEvent::Connecting);
    let fail = |msg: String| {
        on_event(SocketIoEvent::Error(msg));
        on_event(SocketIoEvent::Disconnected { reason: "connect failed".into() });
    };
    if let Some(auth) = &cfg.auth
        && !auth.is_object()
    {
        return fail("The auth payload must be a JSON object, e.g. {\"token\": \"...\"}".into());
    }
    let engine_cfg = match engine_config(&cfg) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let v2 = cfg.version == SocketIoVersion::V2;
    let connect_query = if v2 {
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in cfg.query.iter().cloned().chain(auth_query(cfg.auth.as_ref())) {
            q.append_pair(&k, &v);
        }
        q.finish()
    } else {
        String::new()
    };
    let (ev_tx, mut events) = mpsc::unbounded_channel();
    let engine = engine::spawn(engine_cfg, ev_tx);
    let deadline = Instant::now() + cfg.connect_timeout;
    let timed_out = || format!("Connection timed out after {} s", cfg.connect_timeout.as_secs_f64().round() as u64);
    let mut s = Session {
        engine,
        on_event: on_event.clone(),
        shared: shared.clone(),
        namespace: normalize_namespace(&cfg.namespace),
        v2,
        auth: cfg.auth.clone(),
        connect_query,
        engine_sid: String::new(),
        phase: Phase::Open,
        pending: Vec::new(),
        partial: None,
        auto_ack: cfg.auto_ack.clone(),
    };
    let end = |shared: &Shared| {
        if let Ok(mut t) = shared.transport.lock() {
            *t = None;
        }
        if let Ok(mut p) = shared.pending_acks.lock() {
            p.clear();
        }
    };
    loop {
        tokio::select! {
            ev = events.recv() => {
                let stop = match ev {
                    Some(ev) => s.on_engine(ev),
                    None => {
                        s.emit_event(SocketIoEvent::Error("The connection ended unexpectedly".into()));
                        s.emit_event(SocketIoEvent::Disconnected { reason: s.lost("transport error") });
                        true
                    }
                };
                if stop {
                    end(&shared);
                    return;
                }
            }
            cmd = rx.recv() => match cmd {
                Some(Command::Emit(e)) => {
                    if s.phase == Phase::Connected {
                        s.send_emit(e);
                    } else {
                        s.pending.push(e);
                    }
                }
                Some(Command::Ack(id, args)) => s.send_ack(id, args),
                Some(Command::Disconnect) | None => {
                    let connected = s.phase == Phase::Connected;
                    if connected {
                        s.send(encode_packet(DISCONNECT, &s.namespace, "", None, None));
                    }
                    s.engine.close();
                    // The engine ends its event stream once the close is done.
                    let _ = tokio::time::timeout(DISCONNECT_GRACE, async {
                        while events.recv().await.is_some() {}
                    })
                    .await;
                    let reason = if connected { "io client disconnect" } else { "cancelled" };
                    end(&shared);
                    on_event(SocketIoEvent::Disconnected { reason: reason.into() });
                    return;
                }
            },
            _ = tokio::time::sleep_until(deadline), if s.phase != Phase::Connected => {
                s.engine.close();
                end(&shared);
                return fail(timed_out());
            }
        }
    }
}

impl Session {
    fn emit_event(&self, e: SocketIoEvent) {
        (self.on_event)(e)
    }

    /// Disconnect reason for a dropped connection; before the namespace is
    /// connected it is a failed connection attempt.
    fn lost(&self, reason: &str) -> String {
        if self.phase == Phase::Connected { reason.into() } else { "connect failed".into() }
    }

    fn set_transport(&self, t: ActiveTransport) {
        if let Ok(mut cur) = self.shared.transport.lock() {
            *cur = Some(t);
        }
    }

    fn send(&self, packet: String) {
        self.engine.send(Frame::Text(packet));
    }

    /// Sends an EVENT or ACK packet, followed by its binary attachments.
    fn send_data(&self, kind: u8, id: Option<u64>, data: &[EmitArg]) {
        let (packet, attachments) = encode_data_packet(kind, &self.namespace, id, data);
        self.send(packet);
        for a in attachments {
            self.engine.send(Frame::Binary(a));
        }
    }

    /// Handles an engine event; `true` when the session is over.
    fn on_engine(&mut self, ev: EngineEvent) -> bool {
        match ev {
            EngineEvent::Open { sid, transport } => {
                self.set_transport(transport);
                self.engine_sid = sid;
                self.phase = Phase::Namespace;
                if !self.v2 {
                    self.send(encode_packet(CONNECT, &self.namespace, "", None, self.auth.as_ref()));
                } else if self.namespace != "/" {
                    // The server joins v2 clients to "/" by itself.
                    self.send(encode_packet(CONNECT, &self.namespace, &self.connect_query, None, None));
                }
                false
            }
            EngineEvent::Upgraded => {
                self.set_transport(ActiveTransport::WebSocket);
                false
            }
            EngineEvent::Warning(w) => {
                self.emit_event(SocketIoEvent::Error(w));
                false
            }
            EngineEvent::Message(Frame::Text(t)) => match decode_packet(&t) {
                Ok(packet) => self.on_packet(packet),
                Err(e) => {
                    self.emit_event(SocketIoEvent::Error(format!("Ignored malformed packet: {e}")));
                    false
                }
            },
            EngineEvent::Message(Frame::Binary(b)) => self.on_binary(b),
            EngineEvent::Closed { reason, error } => {
                match error {
                    Some(e) => self.emit_event(SocketIoEvent::Error(e)),
                    None if self.phase != Phase::Connected => self.emit_event(SocketIoEvent::Error(
                        "The server closed the connection before the namespace was connected".into(),
                    )),
                    None => {}
                }
                let reason = if reason == "connect failed" { reason.to_string() } else { self.lost(reason) };
                self.emit_event(SocketIoEvent::Disconnected { reason });
                true
            }
        }
    }

    fn send_emit(&mut self, e: Emit) {
        if e.event.is_empty() {
            self.emit_event(SocketIoEvent::Error("Event name must not be empty".into()));
            return;
        }
        if RESERVED_EVENTS.contains(&e.event.as_str()) {
            self.emit_event(SocketIoEvent::Error(format!("\"{}\" is a reserved event name", e.event)));
            return;
        }
        let mut data = Vec::with_capacity(e.args.len() + 1);
        data.push(EmitArg::Json(Value::String(e.event.clone())));
        data.extend(e.args.iter().cloned());
        self.send_data(EVENT, e.ack, &data);
        self.emit_event(SocketIoEvent::Event {
            name: e.event,
            args: e.args.iter().map(EmitArg::to_json).collect(),
            direction: Direction::Outgoing,
            at_ms: now_ms(),
        });
    }

    fn send_ack(&mut self, id: u64, args: Vec<EmitArg>) {
        let found = self.shared.pending_acks.lock().ok().and_then(|mut p| {
            let i = p.iter().position(|a| a.id == id)?;
            Some(p.remove(i))
        });
        if found.is_none() || self.phase != Phase::Connected {
            self.emit_event(SocketIoEvent::Error(format!("No acknowledgement #{id} is pending")));
            return;
        }
        self.send_data(ACK, Some(id), &args);
    }

    /// Ends the session after a packet it cannot accept; returns `true`.
    fn protocol_error(&mut self, message: String) -> bool {
        self.partial = None;
        self.emit_event(SocketIoEvent::Error(message));
        self.emit_event(SocketIoEvent::Disconnected { reason: self.lost("parse error") });
        true
    }

    /// Collects an attachment of the pending binary packet; `true` when the
    /// session is over.
    fn on_binary(&mut self, data: Vec<u8>) -> bool {
        let Some(partial) = &mut self.partial else { return false };
        partial.bytes += data.len();
        if partial.bytes > MAX_ATTACHMENT_BYTES {
            return self.protocol_error(format!(
                "The binary attachments of one packet exceed the {} MiB limit; closing the connection",
                MAX_ATTACHMENT_BYTES >> 20
            ));
        }
        partial.attachments.push(data);
        if partial.attachments.len() >= partial.packet.attachments
            && let Some(Partial { mut packet, attachments, .. }) = self.partial.take()
        {
            if let Some(data) = &mut packet.data {
                fill_placeholders(data, &attachments);
            }
            packet.kind = if packet.kind == BINARY_EVENT { EVENT } else { ACK };
            self.dispatch(packet);
        }
        false
    }

    /// Handles a Socket.IO packet; `true` when the session is over.
    fn on_packet(&mut self, packet: Packet) -> bool {
        if packet.namespace != self.namespace {
            return false;
        }
        match packet.kind {
            CONNECT => {
                if self.phase == Phase::Namespace {
                    let sid = match packet.data.as_ref().and_then(|d| d.get("sid")).and_then(Value::as_str) {
                        Some(sid) => sid.to_string(),
                        // Socket.IO v2: the socket id derives from the engine id.
                        None if self.namespace == "/" => self.engine_sid.clone(),
                        None => format!("{}#{}", self.namespace, self.engine_sid),
                    };
                    self.phase = Phase::Connected;
                    self.emit_event(SocketIoEvent::Connected { sid, namespace: self.namespace.clone() });
                    for e in std::mem::take(&mut self.pending) {
                        self.send_emit(e);
                    }
                }
                false
            }
            DISCONNECT => {
                self.emit_event(SocketIoEvent::Disconnected { reason: "io server disconnect".into() });
                true
            }
            CONNECT_ERROR => {
                let message = match &packet.data {
                    Some(Value::Object(o)) => match o.get("message") {
                        Some(Value::String(m)) => m.clone(),
                        _ => Value::Object(o.clone()).to_string(),
                    },
                    Some(Value::String(m)) => m.clone(),
                    Some(other) => other.to_string(),
                    None => "no reason given".into(),
                };
                if self.phase == Phase::Connected {
                    // Socket.IO v2 ERROR packet after connecting (`socket.error()`).
                    self.emit_event(SocketIoEvent::Error(format!("Error from the server: {message}")));
                    return false;
                }
                self.emit_event(SocketIoEvent::Error(format!(
                    "The server refused the connection to namespace \"{}\": {message}",
                    self.namespace
                )));
                self.emit_event(SocketIoEvent::Disconnected { reason: "connect error".into() });
                true
            }
            BINARY_EVENT | BINARY_ACK if packet.attachments > MAX_ATTACHMENTS => self.protocol_error(format!(
                "The server announced {} binary attachments in one packet (at most {MAX_ATTACHMENTS} are accepted); \
                 closing the connection",
                packet.attachments
            )),
            BINARY_EVENT | BINARY_ACK if packet.attachments > 0 => {
                self.partial = Some(Partial { packet, attachments: Vec::new(), bytes: 0 });
                false
            }
            _ => {
                self.dispatch(packet);
                false
            }
        }
    }

    /// Reports an EVENT or ACK packet.
    fn dispatch(&mut self, packet: Packet) {
        let at_ms = now_ms();
        match packet.kind {
            EVENT | BINARY_EVENT => {
                let mut items = match packet.data {
                    Some(Value::Array(items)) => items,
                    _ => Vec::new(),
                };
                match (!items.is_empty()).then(|| items.remove(0)) {
                    Some(Value::String(name)) => {
                        if let Some(id) = packet.id
                            && let Ok(mut p) = self.shared.pending_acks.lock()
                        {
                            p.push(AckRequest { id, event: name.clone(), args: items.clone(), at_ms });
                        }
                        self.emit_event(SocketIoEvent::Event {
                            name,
                            args: items,
                            direction: Direction::Incoming,
                            at_ms,
                        });
                        if let (Some(id), Some(args)) = (packet.id, self.auto_ack.clone()) {
                            self.send_ack(id, args);
                        }
                    }
                    _ => self.emit_event(SocketIoEvent::Error("Ignored an event without a name".into())),
                }
            }
            ACK | BINARY_ACK => {
                let args = match packet.data {
                    Some(Value::Array(items)) => items,
                    Some(other) => vec![other],
                    None => Vec::new(),
                };
                if let Some(id) = packet.id {
                    self.emit_event(SocketIoEvent::Ack { id, args, at_ms });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::model::KeyValue;
    use merpati_core::{Auth, protocols::SocketIoRequest};
    use serde_json::json;

    #[test]
    fn urls() {
        let cfg = |url: &str, path: &str| SocketIoConfig {
            url: url.into(),
            path: path.into(),
            query: vec![("token".into(), "a b".into()), ("EIO".into(), "9".into())],
            ..Default::default()
        };
        let u = base_url(&cfg("http://localhost:3000", "/socket.io/")).unwrap();
        assert_eq!(u.as_str(), "http://localhost:3000/socket.io/?token=a+b");
        let u = base_url(&cfg("wss://example.com/ignored?x=1&EIO=3&sid=old", "custom")).unwrap();
        assert_eq!(u.as_str(), "https://example.com/custom/?x=1&token=a+b");
        let u = base_url(&cfg("ws://localhost:3000", "")).unwrap();
        assert_eq!(u.as_str(), "http://localhost:3000/socket.io/?token=a+b");
        let u = base_url(&cfg("localhost:3000", "")).unwrap();
        assert_eq!(u.as_str(), "http://localhost:3000/socket.io/?token=a+b");
        let u = base_url(&SocketIoConfig { url: "http://h".into(), ..Default::default() }).unwrap();
        assert_eq!(u.as_str(), "http://h/socket.io/");
        assert!(base_url(&cfg("ftp://x", "")).unwrap_err().contains("Unsupported"));
        assert!(base_url(&cfg("", "")).is_err());
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("/socket.io"), "/socket.io/");
        // Socket.IO v2: the auth payload becomes query parameters.
        let v2 = SocketIoConfig {
            url: "http://h".into(),
            version: SocketIoVersion::V2,
            auth: Some(json!({"token": "t", "n": 1, "transport": "x"})),
            ..Default::default()
        };
        assert_eq!(base_url(&v2).unwrap().as_str(), "http://h/socket.io/?token=t&n=1");
    }

    #[test]
    fn cookie_headers_are_split_out() {
        let cfg = SocketIoConfig {
            url: "http://h".into(),
            headers: vec![("Cookie".into(), "a=1".into()), ("X-A".into(), "b".into()), ("cookie".into(), "c=2".into())],
            ..Default::default()
        };
        let e = engine_config(&cfg).unwrap();
        assert_eq!(e.user_cookie.as_deref(), Some("a=1; c=2"));
        assert_eq!(e.headers.len(), 1);
        assert_eq!(e.eio, 4);
        let bad = SocketIoConfig { headers: vec![("bad header".into(), "x".into())], ..cfg };
        assert!(engine_config(&bad).err().unwrap().contains("Invalid header"));
    }

    #[test]
    fn from_request_maps_fields() {
        let req = SocketIoRequest {
            url: "{{base}}/admin".into(),
            headers: vec![KeyValue::new("X-Test", "{{v}}"), KeyValue::new("X-Off", "1").disabled()],
            params: vec![KeyValue::new("room", "{{v}}")],
            auth_payload: r#"{"token": "{{v}}"}"#.into(),
            auth: Auth::bearer("{{v}}"),
            ..Default::default()
        };
        let resolve = |s: &str| s.replace("{{base}}", "http://localhost:3000").replace("{{v}}", "42");
        let cfg = SocketIoConfig::from_request(&req, &resolve, true);
        assert_eq!(cfg.url, "http://localhost:3000/admin");
        assert_eq!(cfg.namespace, "/admin");
        assert_eq!(cfg.path, "/socket.io/");
        assert_eq!(
            cfg.headers,
            vec![("X-Test".to_string(), "42".to_string()), ("Authorization".to_string(), "Bearer 42".to_string())]
        );
        assert_eq!(cfg.query, vec![("room".to_string(), "42".to_string())]);
        assert_eq!(cfg.auth, Some(json!({"token": "42"})));
        assert!(cfg.verify_ssl);
        assert_eq!((cfg.transport, cfg.version), (SocketIoTransport::Auto, SocketIoVersion::V4));

        let req = SocketIoRequest {
            url: "http://h".into(),
            namespace: "chat".into(),
            path: "/io".into(),
            auth_payload: "not json".into(),
            auth: Auth::api_key("key", "v", true),
            verify_ssl: Some(false),
            transport: SocketIoTransport::Polling,
            version: SocketIoVersion::V2,
            ..Default::default()
        };
        let cfg = SocketIoConfig::from_request(&req, &|s| s.to_string(), true);
        assert_eq!((cfg.namespace.as_str(), cfg.path.as_str()), ("/chat", "/io"));
        assert_eq!(cfg.auth, Some(json!("not json")));
        assert_eq!(cfg.query, vec![("key".to_string(), "v".to_string())]);
        assert!(!cfg.verify_ssl);
        assert_eq!((cfg.transport, cfg.version), (SocketIoTransport::Polling, SocketIoVersion::V2));
        let cfg = SocketIoConfig::from_request(&SocketIoRequest::default(), &|s| s.to_string(), true);
        assert_eq!((cfg.namespace.as_str(), cfg.auth.is_none()), ("/", true));
        let req = SocketIoRequest { auth: Auth::basic("u", "p"), ..Default::default() };
        let cfg = SocketIoConfig::from_request(&req, &|s| s.to_string(), true);
        assert_eq!(cfg.headers, vec![("Authorization".to_string(), "Basic dTpw".to_string())]);
        let req = SocketIoRequest { auth: Auth::basic("", ""), ..Default::default() };
        assert!(SocketIoConfig::from_request(&req, &|s| s.to_string(), true).headers.is_empty());
    }

    #[test]
    fn request_model_is_backward_compatible() {
        let req: SocketIoRequest = serde_json::from_str(r#"{"url": "http://h"}"#).unwrap();
        assert_eq!((req.transport, req.version), (SocketIoTransport::Auto, SocketIoVersion::V4));
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("transport").is_none() && json.get("version").is_none(), "{json}");
        let req = SocketIoRequest {
            transport: SocketIoTransport::WebSocket,
            version: SocketIoVersion::V2,
            ..Default::default()
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!((json["transport"].as_str(), json["version"].as_str()), (Some("websocket"), Some("v2")));
        assert_eq!(serde_json::from_value::<SocketIoRequest>(json).unwrap(), req);
    }

    #[test]
    fn message_model_is_backward_compatible() {
        use merpati_core::protocols::{SocketIoArg, SocketIoArgKind, SocketIoMessage};
        // Saved before per-argument kinds existed.
        let old: SocketIoMessage =
            serde_json::from_str(r#"{"id": "m1", "event": "join", "args": "[\"room\", 1]", "ack": true}"#).unwrap();
        assert_eq!((old.args.as_str(), old.arguments.is_empty(), old.ack), (r#"["room", 1]"#, true, true));
        let json = serde_json::to_value(&old).unwrap();
        assert_eq!(json, json!({"id": "m1", "event": "join", "args": "[\"room\", 1]", "ack": true}));

        let new = SocketIoMessage {
            id: "m2".into(),
            event: "upload".into(),
            arguments: vec![
                SocketIoArg::new(SocketIoArgKind::Json, r#"{"name": "a.png"}"#),
                SocketIoArg::new(SocketIoArgKind::Text, "hi"),
                SocketIoArg::new(SocketIoArgKind::Binary, "AQID"),
                SocketIoArg::new(SocketIoArgKind::File, "files/a.png"),
            ],
            ..Default::default()
        };
        let json = serde_json::to_value(&new).unwrap();
        assert_eq!(
            json,
            json!({"id": "m2", "event": "upload", "arguments": [
                {"value": "{\"name\": \"a.png\"}"},
                {"kind": "text", "value": "hi"},
                {"kind": "binary", "value": "AQID"},
                {"kind": "file", "value": "files/a.png"},
            ]})
        );
        assert_eq!(serde_json::from_value::<SocketIoMessage>(json).unwrap(), new);

        // What scripts see as `pm.request.message`.
        use merpati_core::script::protocol::socketio_message;
        assert_eq!(socketio_message(&old)["args"], json!(["room", 1]));
        assert_eq!(socketio_message(&new)["args"], json!([{"name": "a.png"}, "hi", "AQID", "files/a.png"]));
        let hex = SocketIoMessage { arguments: vec![SocketIoArg::new(SocketIoArgKind::Binary, "01 02")], ..new };
        assert_eq!(socketio_message(&hex)["args"], json!(["AQI="]));
    }
}
