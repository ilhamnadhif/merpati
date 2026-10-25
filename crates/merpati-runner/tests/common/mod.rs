//! In-process servers for the session tests: a WebSocket echo server, a
//! minimal Socket.IO v4 server (Engine.IO 4 over WebSocket only) and a
//! minimal MQTT 3.1.1 broker (TCP, QoS 0/1 publishes, delivered as QoS 0).
//!
//! Test commands understood by all of them: `burst N` sends N `tick`
//! messages followed by one `done` message; anything else is echoed.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// What a server saw from its clients.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// Request URIs of the handshakes (path and query).
    pub uris: Vec<String>,
    pub authorization: Vec<Option<String>>,
    pub cookies: Vec<Option<String>>,
    /// Text messages, emitted events (`event args-json`) or published topics
    /// (`topic payload`), in order.
    pub received: Vec<String>,
    /// Socket.IO auth payloads; MQTT `user:password`.
    pub auth: Vec<Value>,
}

pub struct Server {
    pub url: String,
    pub seen: Arc<Mutex<Seen>>,
}

impl Server {
    pub fn seen(&self) -> Seen {
        self.seen.lock().unwrap().clone()
    }
}

fn header(req: &Request, name: &str) -> Option<String> {
    req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

// The handshake callback signature is fixed by tungstenite.
#[allow(clippy::result_large_err)]
fn record(
    seen: &Arc<Mutex<Seen>>,
) -> impl FnOnce(&Request, Response) -> Result<Response, tokio_tungstenite::tungstenite::handshake::server::ErrorResponse>
+ use<> {
    let seen = seen.clone();
    move |req: &Request, resp: Response| {
        let mut s = seen.lock().unwrap();
        s.uris.push(req.uri().to_string());
        s.authorization.push(header(req, "authorization"));
        s.cookies.push(header(req, "cookie"));
        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

/// Echo server. Text commands: `burst N` (N `{"type":"tick","i":i}` frames
/// then `{"type":"done"}`), `finish` (`{"type":"done"}`), `close` (closes
/// with 1000 "bye"), `quiet` (no reply); anything else is echoed.
pub async fn websocket() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let s = seen.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let seen = s.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, record(&seen)).await else { return };
                while let Some(Ok(msg)) = ws.next().await {
                    let text = match msg {
                        Message::Text(t) => t.as_str().to_string(),
                        Message::Binary(b) => {
                            seen.lock().unwrap().received.push(format!("binary {}", b.len()));
                            let _ = ws.send(Message::Binary(b)).await;
                            continue;
                        }
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    seen.lock().unwrap().received.push(text.clone());
                    let replies: Vec<String> = if let Some(n) = text.strip_prefix("burst ") {
                        let n: usize = n.trim().parse().unwrap_or(0);
                        let mut r: Vec<String> = (0..n).map(|i| json!({"type": "tick", "i": i}).to_string()).collect();
                        r.push(json!({"type": "done"}).to_string());
                        r
                    } else if text == "finish" {
                        vec![json!({"type": "done"}).to_string()]
                    } else if text == "close" {
                        let frame = CloseFrame { code: CloseCode::Normal, reason: "bye".into() };
                        let _ = ws.close(Some(frame)).await;
                        while let Some(Ok(_)) = ws.next().await {}
                        return;
                    } else if text == "quiet" {
                        Vec::new()
                    } else {
                        vec![text]
                    };
                    for r in replies {
                        if ws.send(Message::text(r)).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    Server { url, seen }
}

// ---------------------------------------------------------------------------
// Socket.IO (Engine.IO 4, WebSocket transport only)
// ---------------------------------------------------------------------------

/// Splits `/ns,rest` into (`/ns`, `rest`); the root namespace is empty.
fn split_ns(s: &str) -> (&str, &str) {
    if s.starts_with('/') {
        match s.find(',') {
            Some(i) => (&s[..i], &s[i + 1..]),
            None => (s, ""),
        }
    } else {
        ("", s)
    }
}

/// Socket.IO server. Events: `echo` (emits `echo` back and acks with the
/// same arguments), `burst` n (n `tick` events then `done`), `whoami`
/// (emits `whoami` with the handshake auth payload), `quiet` (no reply,
/// never acks).
pub async fn socketio() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let s = seen.clone();
    tokio::spawn(async move {
        let mut n = 0;
        while let Ok((tcp, _)) = listener.accept().await {
            n += 1;
            let seen = s.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, record(&seen)).await else { return };
                let open = json!({"sid": format!("eio{n}"), "upgrades": [], "pingInterval": 25000, "pingTimeout": 20000, "maxPayload": 1000000});
                if ws.send(Message::text(format!("0{open}"))).await.is_err() {
                    return;
                }
                let mut auth = Value::Null;
                while let Some(Ok(msg)) = ws.next().await {
                    let Message::Text(t) = msg else {
                        if matches!(msg, Message::Close(_)) {
                            break;
                        }
                        continue;
                    };
                    let t = t.as_str().to_string();
                    let mut out = Vec::new();
                    if t == "2" {
                        out.push("3".to_string());
                    } else if let Some(rest) = t.strip_prefix("40") {
                        let (ns, payload) = split_ns(rest);
                        auth = serde_json::from_str(payload).unwrap_or(Value::Null);
                        seen.lock().unwrap().auth.push(auth.clone());
                        let prefix = if ns.is_empty() { String::new() } else { format!("{ns},") };
                        out.push(format!("40{prefix}{}", json!({"sid": format!("sock{n}")})));
                    } else if t.starts_with("41") {
                        break;
                    } else if let Some(rest) = t.strip_prefix("43") {
                        // The client answered our `question` (see "ask").
                        let (ns, rest) = split_ns(rest);
                        let digits = rest.chars().take_while(char::is_ascii_digit).count();
                        let args: Value = serde_json::from_str(&rest[digits..]).unwrap_or(Value::Null);
                        seen.lock().unwrap().received.push(format!("ack {} {args}", &rest[..digits]));
                        let prefix = if ns.is_empty() { String::new() } else { format!("{ns},") };
                        out.push(format!("42{prefix}{}", json!(["answered", args])));
                    } else if let Some(rest) = t.strip_prefix("42") {
                        let (ns, rest) = split_ns(rest);
                        let digits = rest.chars().take_while(char::is_ascii_digit).count();
                        let ack: Option<u64> = rest[..digits].parse().ok();
                        let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&rest[digits..]) else { continue };
                        let event = items.first().and_then(Value::as_str).unwrap_or("").to_string();
                        let args: Vec<Value> = items[1..].to_vec();
                        seen.lock().unwrap().received.push(format!("{event} {}", Value::Array(args.clone())));
                        let prefix = if ns.is_empty() { String::new() } else { format!("{ns},") };
                        let emit = |name: &str, args: &[Value]| {
                            let mut all = vec![json!(name)];
                            all.extend_from_slice(args);
                            format!("42{prefix}{}", Value::Array(all))
                        };
                        match event.as_str() {
                            "echo" => {
                                if let Some(id) = ack {
                                    out.push(format!("43{prefix}{id}{}", Value::Array(args.clone())));
                                }
                                out.push(emit("echo", &args));
                            }
                            "burst" => {
                                let count = args.first().and_then(Value::as_u64).unwrap_or(0);
                                for i in 0..count {
                                    out.push(emit("tick", &[json!(i)]));
                                }
                                out.push(emit("done", &[json!(count)]));
                            }
                            "whoami" => {
                                if let Some(id) = ack {
                                    out.push(format!("43{prefix}{id}{}", json!([auth])));
                                }
                                out.push(emit("whoami", std::slice::from_ref(&auth)));
                            }
                            // Asks the client for an acknowledgement (id 77).
                            "ask" => out.push(format!("42{prefix}77{}", json!(["question", "ready?"]))),
                            _ => {}
                        }
                    }
                    for o in out {
                        if ws.send(Message::text(o)).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    Server { url, seen }
}

// ---------------------------------------------------------------------------
// MQTT 3.1.1
// ---------------------------------------------------------------------------

struct Sub {
    filter: String,
    tx: mpsc::UnboundedSender<(String, Vec<u8>)>,
}

#[derive(Default)]
struct Broker {
    subs: Vec<Sub>,
}

fn take_packet(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    let (mut len, mut mult, mut i) = (0usize, 1usize, 1usize);
    loop {
        let b = *buf.get(i)?;
        len += (b & 127) as usize * mult;
        mult *= 128;
        i += 1;
        if b & 128 == 0 {
            break;
        }
    }
    if buf.len() < i + len {
        return None;
    }
    let header = buf[0];
    let body = buf[i..i + len].to_vec();
    buf.drain(..i + len);
    Some((header, body))
}

fn packet(header: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![header];
    let mut n = body.len();
    loop {
        let mut b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            b |= 128;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

fn read_str(b: &[u8], i: &mut usize) -> Vec<u8> {
    let n = u16::from_be_bytes([b[*i], b[*i + 1]]) as usize;
    let v = b[*i + 2..*i + 2 + n].to_vec();
    *i += 2 + n;
    v
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend((s.len() as u16).to_be_bytes());
    out.extend(s);
}

fn topic_matches(topic: &str, filter: &str) -> bool {
    let (mut t, mut f) = (topic.split('/'), filter.split('/'));
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn route(broker: &Mutex<Broker>, topic: &str, payload: &[u8]) {
    for s in broker.lock().unwrap().subs.iter().filter(|s| topic_matches(topic, &s.filter)) {
        let _ = s.tx.send((topic.to_string(), payload.to_vec()));
    }
}

/// MQTT 3.1.1 broker; `credentials` makes it require that user/password.
/// Publishing `N` to `cmd/burst` makes the broker publish `tick/0..N` and
/// then `status/done`.
pub async fn mqtt(credentials: Option<(&str, &str)>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("mqtt://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let s = seen.clone();
    let creds = credentials.map(|(u, p)| format!("{u}:{p}"));
    let broker: Arc<Mutex<Broker>> = Arc::default();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let (seen, creds, broker) = (s.clone(), creds.clone(), broker.clone());
            tokio::spawn(async move {
                let (tx, mut rx) = mpsc::unbounded_channel::<(String, Vec<u8>)>();
                let mut buf = Vec::new();
                let mut chunk = vec![0u8; 16 * 1024];
                loop {
                    tokio::select! {
                        n = tcp.read(&mut chunk) => {
                            let Ok(n) = n else { break };
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                            while let Some((header, b)) = take_packet(&mut buf) {
                                let mut out = Vec::new();
                                let mut i = 0;
                                match header >> 4 {
                                    1 => {
                                        read_str(&b, &mut i);
                                        let _level = b[i];
                                        let flags = b[i + 1];
                                        i += 4;
                                        let _client_id = read_str(&b, &mut i);
                                        if flags & 0x04 != 0 {
                                            read_str(&b, &mut i);
                                            read_str(&b, &mut i);
                                        }
                                        let user = (flags & 0x80 != 0).then(|| read_str(&b, &mut i));
                                        let pass = (flags & 0x40 != 0).then(|| read_str(&b, &mut i));
                                        let given = format!(
                                            "{}:{}",
                                            String::from_utf8_lossy(&user.unwrap_or_default()),
                                            String::from_utf8_lossy(&pass.unwrap_or_default())
                                        );
                                        seen.lock().unwrap().auth.push(json!(given));
                                        let ok = creds.as_ref().is_none_or(|c| *c == given);
                                        out.push(packet(0x20, &[0, if ok { 0 } else { 4 }]));
                                        if !ok {
                                            let _ = tcp.write_all(&out.concat()).await;
                                            return;
                                        }
                                    }
                                    3 => {
                                        let qos = (header >> 1) & 3;
                                        let topic = String::from_utf8_lossy(&read_str(&b, &mut i)).into_owned();
                                        if qos > 0 {
                                            out.push(packet(0x40, &b[i..i + 2]));
                                            i += 2;
                                        }
                                        let payload = b[i..].to_vec();
                                        seen.lock().unwrap().received.push(format!("{topic} {}", String::from_utf8_lossy(&payload)));
                                        if topic == "cmd/burst" {
                                            let n: usize = String::from_utf8_lossy(&payload).trim().parse().unwrap_or(0);
                                            for k in 0..n {
                                                route(&broker, &format!("tick/{k}"), json!({"i": k}).to_string().as_bytes());
                                            }
                                            route(&broker, "status/done", b"done");
                                        } else {
                                            route(&broker, &topic, &payload);
                                        }
                                    }
                                    8 => {
                                        let pkid = [b[0], b[1]];
                                        i = 2;
                                        let mut ack = pkid.to_vec();
                                        while i < b.len() {
                                            let filter = String::from_utf8_lossy(&read_str(&b, &mut i)).into_owned();
                                            let qos = b[i] & 3;
                                            i += 1;
                                            if filter.starts_with("forbidden") {
                                                ack.push(0x80);
                                                continue;
                                            }
                                            ack.push(qos.min(1));
                                            broker.lock().unwrap().subs.push(Sub { filter, tx: tx.clone() });
                                        }
                                        out.push(packet(0x90, &ack));
                                    }
                                    12 => out.push(vec![0xD0, 0]),
                                    14 => return,
                                    _ => {}
                                }
                                for o in out {
                                    if tcp.write_all(&o).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        Some((topic, payload)) = rx.recv() => {
                            let mut body = Vec::new();
                            put_str(&mut body, topic.as_bytes());
                            body.extend(payload);
                            if tcp.write_all(&packet(0x30, &body)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    Server { url, seen }
}
