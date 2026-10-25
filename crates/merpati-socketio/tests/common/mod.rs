//! A minimal in-process Socket.IO server for the integration tests:
//! Engine.IO 3 or 4 over HTTP long-polling and/or WebSocket, with the
//! probe/upgrade handshake, optional TLS, a sticky-session cookie, and a
//! small set of test events (`echo`, `info`, `binary`, `binary-ack`,
//! `kick`, `close-transport`, `go-silent`, `ask`, and the oversized `huge`,
//! `many-attachments` and `big-attachments`).

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;

#[derive(Clone, Debug)]
pub struct Options {
    /// Engine.IO protocol: 4 (Socket.IO v3/v4) or 3 (Socket.IO v2).
    pub eio: u8,
    pub ping_interval_ms: u64,
    pub ping_timeout_ms: u64,
    pub polling: bool,
    pub websocket: bool,
    pub tls: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            eio: 4,
            ping_interval_ms: 25_000,
            ping_timeout_ms: 20_000,
            polling: true,
            websocket: true,
            tls: false,
        }
    }
}

/// One HTTP request or WebSocket handshake seen by the server.
#[derive(Clone, Debug)]
pub struct RequestLog {
    pub transport: String,
    pub method: String,
    pub query: String,
    pub x_test: Option<String>,
    pub cookie: Option<String>,
    pub sid: Option<String>,
}

pub struct Server {
    pub port: u16,
    state: Arc<State>,
}

impl Server {
    pub fn url(&self) -> String {
        format!("{}://127.0.0.1:{}", if self.state.opts.tls { "https" } else { "http" }, self.port)
    }

    /// Engine.IO text packets received from clients (`40`, `3`, `2probe`, ...),
    /// including base64 binary packets of polling payloads (`bAQID`, `b4AQID`).
    pub fn frames(&self) -> Vec<String> {
        self.state.frames.lock().unwrap().clone()
    }

    /// WebSocket binary frames received from clients, as sent (Engine.IO 3
    /// frames start with the message type byte 4).
    pub fn binary_frames(&self) -> Vec<Vec<u8>> {
        self.state.binary_frames.lock().unwrap().clone()
    }

    pub fn requests(&self) -> Vec<RequestLog> {
        self.state.requests.lock().unwrap().clone()
    }
}

struct State {
    opts: Options,
    frames: Mutex<Vec<String>>,
    binary_frames: Mutex<Vec<Vec<u8>>>,
    requests: Mutex<Vec<RequestLog>>,
    sessions: Mutex<HashMap<String, Arc<Sess>>>,
    counter: AtomicU64,
}

#[derive(Clone, Debug)]
enum Out {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Default)]
struct Logic {
    auths: HashMap<String, Value>,
    pongs: u64,
    pings: u64,
    silent: bool,
    next_ask: u64,
}

struct Sess {
    sid: String,
    n: u64,
    eio: u8,
    query: String,
    x_test: Option<String>,
    cookie: Option<String>,
    out: Mutex<VecDeque<Out>>,
    notify: Notify,
    ws: Mutex<Option<mpsc::UnboundedSender<Option<Out>>>>,
    closed: AtomicBool,
    /// A long-polling GET is waiting for packets.
    polling: AtomicBool,
    upgraded: AtomicBool,
    logic: Mutex<Logic>,
}

impl Sess {
    fn push(&self, o: Out) {
        if let Some(tx) = self.ws.lock().unwrap().as_ref() {
            let _ = tx.send(Some(o));
            return;
        }
        self.out.lock().unwrap().push_back(o);
        self.notify.notify_one();
    }

    /// Ends the session: a close packet for polling, a closed socket for WebSocket.
    fn close_transport(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(tx) = self.ws.lock().unwrap().take() {
            let _ = tx.send(None);
            return;
        }
        self.out.lock().unwrap().push_back(Out::Text("1".into()));
        self.notify.notify_one();
    }
}

pub async fn start(opts: Options) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tls = opts.tls.then(tls_acceptor);
    let state = Arc::new(State {
        opts,
        frames: Mutex::default(),
        binary_frames: Mutex::default(),
        requests: Mutex::default(),
        sessions: Mutex::default(),
        counter: AtomicU64::new(0),
    });
    {
        let state = state.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (state, tls) = (state.clone(), tls.clone());
                tokio::spawn(async move {
                    match tls {
                        Some(acceptor) => {
                            if let Ok(s) = acceptor.accept(tcp).await {
                                handle(s, state).await;
                            }
                        }
                        None => handle(tcp, state).await,
                    }
                });
            }
        });
    }
    Server { port, state }
}

/// A TLS acceptor with a fresh self-signed certificate for 127.0.0.1.
fn tls_acceptor() -> tokio_rustls::TlsAcceptor {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![ck.cert.der().clone()], key)
        .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

struct Req {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    fn param(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.query.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
    }
}

async fn read_request<S: AsyncRead + Unpin>(s: &mut S) -> Option<Req> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Some(Req { method, path, query, headers, body })
}

async fn respond<S: AsyncWrite + Unpin>(s: &mut S, status: &str, extra: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.write_all(resp.as_bytes()).await;
    let _ = s.shutdown().await;
}

async fn bad_request<S: AsyncWrite + Unpin>(s: &mut S, code: u8, message: &str) {
    respond(s, "400 Bad Request", "", &json!({"code": code, "message": message}).to_string()).await;
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(mut s: S, state: Arc<State>) {
    let Some(req) = read_request(&mut s).await else { return };
    let opts = state.opts.clone();
    let transport = req.param("transport").unwrap_or_default();
    let is_upgrade = req.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    if req.path != "/socket.io/" {
        return bad_request(&mut s, 0, "Transport unknown").await;
    }
    if opts.eio == 4 && req.param("EIO").as_deref() != Some("4") {
        return bad_request(&mut s, 5, "Unsupported protocol version").await;
    }
    let allowed =
        if is_upgrade { transport == "websocket" && opts.websocket } else { transport == "polling" && opts.polling };
    if !allowed {
        return bad_request(&mut s, 0, "Transport unknown").await;
    }
    let sid = req.param("sid");
    state.requests.lock().unwrap().push(RequestLog {
        transport: transport.clone(),
        method: req.method.clone(),
        query: req.query.clone(),
        x_test: req.header("x-test"),
        cookie: req.header("cookie"),
        sid: sid.clone(),
    });
    let existing = sid.as_ref().map(|sid| state.sessions.lock().unwrap().get(sid).cloned());
    if let Some(None) = existing {
        return bad_request(&mut s, 1, "Session ID unknown").await;
    }
    let existing = existing.flatten();
    if is_upgrade {
        let key = req.header("sec-websocket-key").unwrap_or_default();
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        if s.write_all(resp.as_bytes()).await.is_err() {
            return;
        }
        let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(s, Role::Server, None).await;
        match existing {
            Some(sess) => upgrade(ws, sess, state).await,
            None => {
                let sess = new_session(&state, &req, false);
                websocket(ws, sess, state, true).await;
            }
        }
        return;
    }
    match (req.method.as_str(), existing) {
        ("GET", None) => {
            let sess = new_session(&state, &req, opts.websocket);
            let payload = drain(&sess, opts.eio);
            let cookie = format!("Set-Cookie: sticky=s{}; Path=/\r\n", sess.n);
            respond(&mut s, "200 OK", &cookie, &payload).await;
        }
        ("GET", Some(sess)) => {
            sess.polling.store(true, Ordering::SeqCst);
            loop {
                let notified = sess.notify.notified();
                if !sess.out.lock().unwrap().is_empty() {
                    break;
                }
                notified.await;
            }
            sess.polling.store(false, Ordering::SeqCst);
            let payload = drain(&sess, sess.eio);
            respond(&mut s, "200 OK", "", &payload).await;
        }
        ("POST", Some(sess)) => {
            let body = String::from_utf8_lossy(&req.body).to_string();
            for p in decode_payload(sess.eio, &body) {
                on_text(&state, &sess, &p);
            }
            respond(&mut s, "200 OK", "", "ok").await;
        }
        _ => bad_request(&mut s, 2, "Bad handshake method").await,
    }
}

fn new_session(state: &Arc<State>, req: &Req, upgradable: bool) -> Arc<Sess> {
    let opts = &state.opts;
    let n = state.counter.fetch_add(1, Ordering::SeqCst) + 1;
    let sid = format!("eio-{n}");
    let mut open = json!({
        "sid": sid,
        "upgrades": if upgradable { json!(["websocket"]) } else { json!([]) },
        "pingInterval": opts.ping_interval_ms,
        "pingTimeout": opts.ping_timeout_ms,
    });
    if opts.eio == 4 {
        open["maxPayload"] = json!(1_000_000);
    }
    let sess = Arc::new(Sess {
        sid: sid.clone(),
        n,
        eio: opts.eio,
        query: req.query.clone(),
        x_test: req.header("x-test"),
        cookie: req.header("cookie"),
        out: Mutex::new(VecDeque::from([Out::Text(format!("0{open}"))])),
        notify: Notify::new(),
        ws: Mutex::new(None),
        closed: AtomicBool::new(false),
        polling: AtomicBool::new(false),
        upgraded: AtomicBool::new(false),
        logic: Mutex::default(),
    });
    if opts.eio == 3 {
        // Socket.IO v2 joins the root namespace by itself.
        sess.push(Out::Text("40".into()));
    }
    state.sessions.lock().unwrap().insert(sid, sess.clone());
    if opts.eio == 4 {
        let (sess, interval) = (sess.clone(), Duration::from_millis(opts.ping_interval_ms));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if sess.closed.load(Ordering::SeqCst) {
                    break;
                }
                if !sess.logic.lock().unwrap().silent {
                    sess.push(Out::Text("2".into()));
                }
            }
        });
    }
    sess
}

fn drain(sess: &Sess, eio: u8) -> String {
    let packets: Vec<Out> = sess.out.lock().unwrap().drain(..).collect();
    let parts: Vec<String> = packets
        .iter()
        .map(|p| {
            let s = match p {
                Out::Text(t) => t.clone(),
                Out::Binary(b) if eio == 3 => format!("b4{}", base64::engine::general_purpose::STANDARD.encode(b)),
                Out::Binary(b) => format!("b{}", base64::engine::general_purpose::STANDARD.encode(b)),
            };
            if eio == 3 { format!("{}:{s}", s.encode_utf16().count()) } else { s }
        })
        .collect();
    parts.join(if eio == 3 { "" } else { "\u{1e}" })
}

fn decode_payload(eio: u8, body: &str) -> Vec<String> {
    if eio != 3 {
        return body.split('\u{1e}').map(str::to_string).collect();
    }
    let mut out = Vec::new();
    let mut rest = body;
    while let Some((len, tail)) = rest.split_once(':') {
        let n: usize = len.parse().unwrap();
        let (mut units, mut end) = (0, 0);
        for c in tail.chars() {
            if units >= n {
                break;
            }
            units += c.len_utf16();
            end += c.len_utf8();
        }
        out.push(tail[..end].to_string());
        rest = &tail[end..];
    }
    out
}

/// `2probe` / `3probe`, then `5`: the session moves to this socket.
async fn upgrade<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    mut ws: tokio_tungstenite::WebSocketStream<S>,
    sess: Arc<Sess>,
    state: Arc<State>,
) {
    loop {
        let Some(Ok(Message::Text(t))) = ws.next().await else { return };
        state.frames.lock().unwrap().push(t.to_string());
        match t.as_str() {
            "2probe" => {
                if ws.send(Message::text("3probe")).await.is_err() {
                    return;
                }
                // Like engine.io: until the upgrade completes, flush any
                // pending GET with a noop so the client can pause polling.
                let sess = sess.clone();
                tokio::spawn(async move {
                    while !sess.upgraded.load(Ordering::SeqCst) && !sess.closed.load(Ordering::SeqCst) {
                        if sess.polling.load(Ordering::SeqCst) && sess.out.lock().unwrap().is_empty() {
                            sess.push(Out::Text("6".into()));
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                });
            }
            "5" => break,
            _ => {}
        }
    }
    sess.upgraded.store(true, Ordering::SeqCst);
    websocket(ws, sess, state, false).await;
}

/// Runs a session over WebSocket (`fresh` = no polling before: the open
/// packet is sent first).
async fn websocket<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    sess: Arc<Sess>,
    state: Arc<State>,
    fresh: bool,
) {
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<Out>>();
    {
        // Everything queued for polling (and the open packet) goes out first.
        let mut ws_slot = sess.ws.lock().unwrap();
        let queued: Vec<Out> = sess.out.lock().unwrap().drain(..).collect();
        for o in queued {
            let _ = tx.send(Some(o));
        }
        *ws_slot = Some(tx);
    }
    let _ = fresh;
    let eio = sess.eio;
    let writer = tokio::spawn(async move {
        while let Some(o) = rx.recv().await {
            let m = match o {
                None => {
                    let _ = sink.close().await;
                    return;
                }
                Some(Out::Text(t)) => Message::text(t),
                Some(Out::Binary(b)) if eio == 3 => Message::binary([vec![4u8], b].concat()),
                Some(Out::Binary(b)) => Message::binary(b),
            };
            if sink.send(m).await.is_err() {
                return;
            }
        }
    });
    while let Some(Ok(m)) = stream.next().await {
        match m {
            Message::Text(t) => on_text(&state, &sess, t.as_str()),
            Message::Binary(b) => state.binary_frames.lock().unwrap().push(b.to_vec()),
            Message::Close(_) => break,
            _ => {}
        }
    }
    sess.closed.store(true, Ordering::SeqCst);
    sess.ws.lock().unwrap().take();
    let _ = writer.await;
}

// ---------------------------------------------------------------------------
// Socket.IO logic
// ---------------------------------------------------------------------------

/// Splits a Socket.IO packet into (type, namespace, query, ack id, data);
/// the attachment count of binary packets is skipped.
fn parse(p: &str) -> (u8, String, String, Option<u64>, Value) {
    let kind = p.as_bytes()[0] - b'0';
    let mut rest = &p[1..];
    if kind == 5 || kind == 6 {
        rest = rest.split_once('-').map_or(rest, |(_, r)| r);
    }
    let mut ns = "/".to_string();
    let mut query = String::new();
    if rest.starts_with('/') {
        let end = rest.find(',').unwrap_or(rest.len());
        ns = rest[..end].to_string();
        if let Some((n, q)) = ns.clone().split_once('?') {
            ns = n.to_string();
            query = q.to_string();
        }
        rest = &rest[(end + 1).min(rest.len())..];
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    let id = (digits > 0).then(|| rest[..digits].parse().unwrap());
    rest = &rest[digits..];
    let data = if rest.is_empty() { Value::Null } else { serde_json::from_str(rest).unwrap() };
    (kind, ns, query, id, data)
}

fn on_text(state: &State, sess: &Arc<Sess>, text: &str) {
    state.frames.lock().unwrap().push(text.to_string());
    let v2 = sess.eio == 3;
    match text {
        "3" => {
            sess.logic.lock().unwrap().pongs += 1;
            return;
        }
        // Socket.IO v2: the client pings.
        "2" => {
            let mut logic = sess.logic.lock().unwrap();
            logic.pings += 1;
            if !logic.silent {
                sess.push(Out::Text("3".into()));
            }
            return;
        }
        "1" => {
            sess.closed.store(true, Ordering::SeqCst);
            return;
        }
        _ => {}
    }
    let Some(packet) = text.strip_prefix('4') else { return };
    let (kind, ns, query, id, data) = parse(packet);
    let prefix = |k: u8| if ns == "/" { format!("4{k}") } else { format!("4{k}{ns},") };
    let nsp = if ns == "/" { String::new() } else { format!("{ns},") };
    match kind {
        0 => {
            let auth = if v2 {
                let q: serde_json::Map<String, Value> = url::form_urlencoded::parse(query.as_bytes())
                    .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                    .collect();
                Value::Object(q)
            } else {
                data
            };
            if ns == "/private" && auth.get("token") != Some(&json!("secret")) {
                let err = if v2 { json!("not authorized") } else { json!({"message": "not authorized"}) };
                sess.push(Out::Text(format!("{}{err}", prefix(4))));
            } else {
                sess.logic.lock().unwrap().auths.insert(ns.clone(), auth);
                if v2 {
                    sess.push(Out::Text(prefix(0)));
                } else {
                    sess.push(Out::Text(format!("{}{}", prefix(0), json!({"sid": format!("sock-{}", sess.n)}))));
                }
            }
        }
        2 => {
            let items = data.as_array().cloned().unwrap_or_default();
            let name = items.first().and_then(Value::as_str).unwrap_or_default().to_string();
            let args = Value::Array(items[1..].to_vec());
            match name.as_str() {
                "echo" => {
                    sess.push(Out::Text(format!("{}{}", prefix(2), Value::Array(items.clone()))));
                    if let Some(id) = id {
                        sess.push(Out::Text(format!("{}{id}{args}", prefix(3))));
                    }
                }
                "info" => {
                    let logic = sess.logic.lock().unwrap();
                    let info = json!(["info", {
                        "query": sess.query,
                        "x-test": sess.x_test,
                        "cookie": sess.cookie,
                        "auth": logic.auths.get(&ns),
                        "pongs": logic.pongs,
                        "pings": logic.pings,
                    }]);
                    sess.push(Out::Text(format!("{}{info}", prefix(2))));
                }
                "binary" => {
                    let data = json!(["binary", {"_placeholder": true, "num": 0}, "text"]);
                    sess.push(Out::Text(format!("451-{nsp}{data}")));
                    sess.push(Out::Binary(vec![1u8, 2, 3]));
                }
                "binary-ack" => {
                    let data = json!([{"file": {"_placeholder": true, "num": 0}}]);
                    sess.push(Out::Text(format!("461-{nsp}{}{data}", id.unwrap_or(0))));
                    sess.push(Out::Binary(b"hi".to_vec()));
                }
                // The server emits an event with an ack callback.
                "ask" => {
                    let ask = {
                        let mut logic = sess.logic.lock().unwrap();
                        logic.next_ask += 1;
                        100 + logic.next_ask
                    };
                    sess.push(Out::Text(format!("{}{ask}{}", prefix(2), json!(["question", "ready?"]))));
                }
                // Larger than the client accepts in one polling response.
                "huge" => {
                    let big = "x".repeat(merpati_socketio::MAX_POLLING_BODY + 1);
                    sess.push(Out::Text(format!("{}{}", prefix(2), json!(["huge", big]))));
                }
                // Announces more attachments than the client accepts.
                "many-attachments" => {
                    let data = json!(["many", {"_placeholder": true, "num": 0}]);
                    sess.push(Out::Text(format!("45{}-{nsp}{data}", merpati_socketio::MAX_ATTACHMENTS + 1)));
                    sess.push(Out::Binary(vec![0]));
                }
                // Five 15 MiB attachments: each fits a WebSocket frame, together
                // they exceed the client's limit.
                "big-attachments" => {
                    let placeholders: Vec<Value> = (0..5).map(|n| json!({"_placeholder": true, "num": n})).collect();
                    sess.push(Out::Text(format!("455-{nsp}{}", json!(["big", placeholders]))));
                    for _ in 0..5 {
                        sess.push(Out::Binary(vec![7u8; 15 * 1024 * 1024]));
                    }
                }
                "kick" => sess.push(Out::Text(prefix(1))),
                "close-transport" => sess.close_transport(),
                "go-silent" => sess.logic.lock().unwrap().silent = true,
                _ => {
                    if let Some(id) = id {
                        sess.push(Out::Text(format!("{}{id}{}", prefix(3), json!(["unknown", name]))));
                    }
                }
            }
        }
        // The client acknowledged one of our `ask` events.
        3 => {
            let mut items = vec![json!("answered"), json!(id)];
            items.extend(data.as_array().cloned().unwrap_or_default());
            sess.push(Out::Text(format!("{}{}", prefix(2), Value::Array(items))));
        }
        _ => {}
    }
}
