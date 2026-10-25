//! Integration tests against the in-process Socket.IO server of
//! `tests/common`: every scenario runs over each transport (auto with
//! upgrade, WebSocket only, polling only) and both protocol revisions
//! (Socket.IO v3/v4 = Engine.IO 4, Socket.IO v2 = Engine.IO 3).

mod common;

use std::time::Duration;

use common::Options;
use merpati_core::model::KeyValue;
use merpati_core::protocols::SocketIoRequest;
use merpati_socketio::{
    ActiveTransport, Direction, EmitArg, SocketIoConfig, SocketIoEvent, SocketIoSession, SocketIoTransport,
    SocketIoVersion, parse_args,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const TRANSPORTS: [SocketIoTransport; 3] =
    [SocketIoTransport::Auto, SocketIoTransport::WebSocket, SocketIoTransport::Polling];
const VERSIONS: [SocketIoVersion; 2] = [SocketIoVersion::V4, SocketIoVersion::V2];

/// Every (transport, version) pair.
fn matrix() -> Vec<(SocketIoTransport, SocketIoVersion)> {
    VERSIONS.iter().flat_map(|v| TRANSPORTS.iter().map(move |t| (*t, *v))).collect()
}

fn eio(version: SocketIoVersion) -> u8 {
    if version == SocketIoVersion::V2 { 3 } else { 4 }
}

async fn server(version: SocketIoVersion) -> common::Server {
    common::start(Options { eio: eio(version), ..Default::default() }).await
}

struct Events {
    rx: mpsc::UnboundedReceiver<SocketIoEvent>,
    seen: Vec<SocketIoEvent>,
    label: String,
}

impl Events {
    async fn wait_for(&mut self, what: &str, pred: impl Fn(&SocketIoEvent) -> bool) -> SocketIoEvent {
        let r = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match self.rx.recv().await {
                    Some(e) => {
                        self.seen.push(e.clone());
                        if pred(&e) {
                            return Some(e);
                        }
                    }
                    None => return None,
                }
            }
        })
        .await;
        match r {
            Ok(Some(e)) => e,
            _ => panic!("[{}] no {what} event; seen: {:#?}", self.label, self.seen),
        }
    }

    /// Next incoming event called `name`; returns its args.
    async fn incoming(&mut self, name: &str) -> Vec<Value> {
        let e = self
            .wait_for(
                &format!("incoming {name}"),
                |e| matches!(e, SocketIoEvent::Event { name: n, direction: Direction::Incoming, .. } if n == name),
            )
            .await;
        let SocketIoEvent::Event { args, at_ms, .. } = e else { unreachable!() };
        assert!(at_ms > 0);
        args
    }

    async fn ack(&mut self, id: u64) -> Vec<Value> {
        match self.wait_for(&format!("ack {id}"), |e| matches!(e, SocketIoEvent::Ack { id: i, .. } if *i == id)).await {
            SocketIoEvent::Ack { args, .. } => args,
            _ => unreachable!(),
        }
    }

    async fn disconnected(&mut self) -> String {
        match self.wait_for("disconnected", |e| matches!(e, SocketIoEvent::Disconnected { .. })).await {
            SocketIoEvent::Disconnected { reason } => reason,
            _ => unreachable!(),
        }
    }

    fn errors(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|e| match e {
                SocketIoEvent::Error(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }
}

fn config(url: String, namespace: &str, transport: SocketIoTransport, version: SocketIoVersion) -> SocketIoConfig {
    SocketIoConfig {
        url,
        namespace: namespace.into(),
        connect_timeout: Duration::from_secs(5),
        transport,
        version,
        ..Default::default()
    }
}

fn start(cfg: SocketIoConfig) -> (SocketIoSession, Events) {
    let label = format!("{:?}/{:?}/{}", cfg.transport, cfg.version, cfg.namespace);
    let (tx, rx) = mpsc::unbounded_channel();
    let session = SocketIoSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| {
        let _ = tx.send(e);
    });
    (session, Events { rx, seen: Vec::new(), label })
}

async fn connect(cfg: SocketIoConfig) -> (SocketIoSession, Events, String) {
    let v2 = cfg.version == SocketIoVersion::V2;
    let (session, mut events) = start(cfg);
    let e = events.wait_for("connected", |e| matches!(e, SocketIoEvent::Connected { .. })).await;
    assert_eq!(events.seen[0], SocketIoEvent::Connecting);
    let SocketIoEvent::Connected { sid, namespace } = e else { unreachable!() };
    if v2 {
        // Socket.IO v2 ids derive from the Engine.IO session id.
        let want = if namespace == "/" { "eio-".to_string() } else { format!("{namespace}#eio-") };
        assert!(sid.starts_with(&want), "{sid}");
    } else {
        assert!(sid.starts_with("sock-"), "{sid}");
    }
    (session, events, namespace)
}

async fn wait_until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_root_namespace_and_echo() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        let (s, mut ev, ns) = connect(config(server.url(), "/", transport, version)).await;
        assert_eq!(ns, "/");
        let args = vec![json!("hi"), json!({"a": [1, 2], "é": "😀"}), json!(null)];
        assert_eq!(s.emit("echo", args.clone(), false), None);
        let out =
            ev.wait_for("outgoing", |e| matches!(e, SocketIoEvent::Event { direction: Direction::Outgoing, .. })).await;
        assert!(matches!(&out, SocketIoEvent::Event { name, args: a, .. } if name == "echo" && *a == args), "{out:?}");
        assert_eq!(ev.incoming("echo").await, args);
        let frames = server.frames();
        // v2 clients do not send CONNECT for the root namespace.
        assert_eq!(frames.contains(&"40".to_string()), version == SocketIoVersion::V4, "{frames:?}");
        assert!(frames.contains(&r#"42["echo","hi",{"a":[1,2],"é":"😀"},null]"#.to_string()), "{frames:?}");
        assert!(ev.errors().is_empty(), "{:?}", ev.errors());
        assert!(s.transport().is_some());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_namespace_events_and_acks() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        let (s, mut ev, ns) = connect(config(server.url(), "/admin", transport, version)).await;
        assert_eq!(ns, "/admin");
        assert_eq!(s.emit("echo", vec![json!(1)], true), Some(0));
        assert_eq!(s.emit("something", vec![json!("x")], true), Some(1));
        assert_eq!(ev.incoming("echo").await, vec![json!(1)]);
        assert_eq!(ev.ack(0).await, vec![json!(1)]);
        assert_eq!(ev.ack(1).await, vec![json!("unknown"), json!("something")]);
        let frames = server.frames();
        assert!(frames.contains(&"40/admin,".to_string()), "{frames:?}");
        assert!(frames.contains(&r#"42/admin,0["echo",1]"#.to_string()), "{frames:?}");
        assert!(frames.contains(&r#"42/admin,1["something","x"]"#.to_string()), "{frames:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_ack_requests_are_answered() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        let (s, mut ev, _) = connect(config(server.url(), "/q", transport, version)).await;
        s.emit("ask", vec![], false);
        assert_eq!(ev.incoming("question").await, vec![json!("ready?")]);
        let pending = s.pending_acks();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!((pending[0].id, pending[0].event.as_str()), (101, "question"));
        assert_eq!(pending[0].args, vec![json!("ready?")]);
        assert!(s.ack(101, vec![json!("yes"), json!({"n": 1})]));
        assert_eq!(ev.incoming("answered").await, vec![json!(101), json!("yes"), json!({"n": 1})]);
        assert!(s.pending_acks().is_empty());
        assert!(server.frames().contains(&r#"43/q,101["yes",{"n":1}]"#.to_string()), "{:?}", server.frames());
        // Answering twice (or an unknown id) is an error, not a packet.
        s.ack(101, vec![]);
        ev.wait_for("unknown ack error", |e| matches!(e, SocketIoEvent::Error(m) if m.contains("#101"))).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_ack_requests_are_answered_automatically() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        let mut cfg = config(server.url(), "/q", transport, version);
        cfg.auto_ack = Some(vec![EmitArg::Json(json!("auto")), EmitArg::Json(json!({"n": 2}))]);
        let (s, mut ev, _) = connect(cfg).await;
        s.emit("ask", vec![], false);
        assert_eq!(ev.incoming("question").await, vec![json!("ready?")]);
        assert_eq!(ev.incoming("answered").await, vec![json!(101), json!("auto"), json!({"n": 2})]);
        assert!(s.pending_acks().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emits_before_connect_are_queued() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        let (s, mut ev) = start(config(server.url(), "/queue", transport, version));
        let id = s.emit("echo", parse_args(r#"["early", 2]"#), true);
        assert_eq!(ev.incoming("echo").await, vec![json!("early"), json!(2)]);
        assert_eq!(ev.ack(id.unwrap()).await, vec![json!("early"), json!(2)]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_upgrades_to_websocket_without_losing_messages() {
    for version in VERSIONS {
        let server = server(version).await;
        let (s, mut ev, _) = connect(config(server.url(), "/up", SocketIoTransport::Auto, version)).await;
        // A burst while the upgrade is in progress arrives complete and in order.
        for i in 0..20 {
            s.emit("echo", vec![json!(i)], false);
        }
        for i in 0..20 {
            assert_eq!(ev.incoming("echo").await, vec![json!(i)]);
        }
        wait_until("upgrade", || s.transport() == Some(ActiveTransport::WebSocket)).await;
        let frames = server.frames();
        let probe = frames.iter().position(|f| f == "2probe").expect("probe");
        let upgrade = frames.iter().position(|f| f == "5").expect("upgrade");
        assert!(probe < upgrade, "{frames:?}");
        s.emit("echo", vec![json!("after")], false);
        assert_eq!(ev.incoming("echo").await, vec![json!("after")]);
        let requests = server.requests();
        assert!(
            requests.iter().any(|r| r.transport == "websocket" && r.sid.as_deref() == Some("eio-1")),
            "{requests:?}"
        );
        assert!(ev.errors().is_empty(), "{:?}", ev.errors());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_only_and_websocket_only_servers() {
    for version in VERSIONS {
        // Polling-only server: auto stays on polling, WebSocket-only fails with advice.
        let polling = common::start(Options { eio: eio(version), websocket: false, ..Default::default() }).await;
        let (s, mut ev, _) = connect(config(polling.url(), "/", SocketIoTransport::Auto, version)).await;
        s.emit("echo", vec![json!("polled")], false);
        assert_eq!(ev.incoming("echo").await, vec![json!("polled")]);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(s.transport(), Some(ActiveTransport::Polling));
        assert!(!polling.frames().contains(&"2probe".to_string()));
        let (_s, mut ev) = start(config(polling.url(), "/", SocketIoTransport::WebSocket, version));
        assert_eq!(ev.disconnected().await, "connect failed");
        let err = &ev.errors()[0];
        assert!(err.contains("HTTP 400") && err.contains("Polling transport"), "{err}");

        // WebSocket-only server: auto falls back to a direct WebSocket.
        let ws = common::start(Options { eio: eio(version), polling: false, ..Default::default() }).await;
        let (s, mut ev, _) = connect(config(ws.url(), "/", SocketIoTransport::Auto, version)).await;
        assert_eq!(s.transport(), Some(ActiveTransport::WebSocket));
        s.emit("echo", vec![json!("direct")], false);
        assert_eq!(ev.incoming("echo").await, vec![json!("direct")]);
        let (_s, mut ev) = start(config(ws.url(), "/", SocketIoTransport::Polling, version));
        assert_eq!(ev.disconnected().await, "connect failed");
        let err = &ev.errors()[0];
        assert!(err.contains("HTTP 400") && err.contains("WebSocket transport"), "{err}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_falls_back_when_polling_requests_are_dropped() {
    // A bare WebSocket server (like examples/echo_server.rs): plain HTTP
    // requests fail the WebSocket handshake and the connection is dropped.
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else { return };
                let open = json!({"sid": "bare", "upgrades": [], "pingInterval": 25000, "pingTimeout": 20000});
                let _ = ws.send(Message::text(format!("0{open}"))).await;
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let reply = match t.as_str() {
                        "40" => r#"40{"sid":"sock-bare"}"#.to_string(),
                        other => other.to_string(), // echo events back
                    };
                    let _ = ws.send(Message::text(reply)).await;
                }
            });
        }
    });
    let url = format!("http://127.0.0.1:{port}");
    let (s, mut ev, _) = connect(config(url.clone(), "/", SocketIoTransport::Auto, SocketIoVersion::V4)).await;
    assert_eq!(s.transport(), Some(ActiveTransport::WebSocket));
    s.emit("echo", vec![json!("bare")], false);
    assert_eq!(ev.incoming("echo").await, vec![json!("bare")]);
    let (_s, mut ev) = start(config(url, "/", SocketIoTransport::Polling, SocketIoVersion::V4));
    assert_eq!(ev.disconnected().await, "connect failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_disconnect() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        for ns in ["/", "/admin"] {
            let (s, mut ev, _) = connect(config(server.url(), ns, transport, version)).await;
            s.emit("kick", vec![], false);
            assert_eq!(ev.disconnected().await, "io server disconnect");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!s.is_alive());
            assert_eq!(s.emit("echo", vec![], true), None);
            assert_eq!(s.transport(), None);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_close() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        let (s, mut ev, _) = connect(config(server.url(), "/", transport, version)).await;
        if transport == SocketIoTransport::Auto {
            wait_until("upgrade", || s.transport() == Some(ActiveTransport::WebSocket)).await;
        }
        s.emit("close-transport", vec![], false);
        assert_eq!(ev.disconnected().await, "transport close", "{transport:?} {version:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_disconnect_and_drop() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        let (s, mut ev, _) = connect(config(server.url(), "/admin", transport, version)).await;
        s.disconnect();
        assert_eq!(ev.disconnected().await, "io client disconnect");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!s.is_alive());
        assert!(server.frames().contains(&"41/admin,".to_string()), "{:?}", server.frames());
        if transport == SocketIoTransport::Polling {
            // The Engine.IO close packet follows over polling.
            assert!(server.frames().contains(&"1".to_string()), "{:?}", server.frames());
        }

        let (s, mut ev, _) = connect(config(server.url(), "/", transport, version)).await;
        drop(s);
        assert_eq!(ev.disconnected().await, "io client disconnect");
        assert!(server.frames().contains(&"41".to_string()), "{:?}", server.frames());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_auth() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        let (_s, mut ev) = start(config(server.url(), "/private", transport, version));
        assert_eq!(ev.disconnected().await, "connect error");
        let errors = ev.errors();
        assert!(errors.len() == 1 && errors[0].contains("not authorized"), "{errors:?}");

        let req = SocketIoRequest {
            url: format!("{}/private", server.url()),
            auth_payload: r#"{"token": "{{token}}"}"#.into(),
            transport,
            version,
            ..Default::default()
        };
        let cfg = SocketIoConfig::from_request(&req, &|s| s.replace("{{token}}", "secret"), true);
        let (s, mut ev, ns) = connect(cfg).await;
        assert_eq!(ns, "/private");
        s.emit("info", vec![], false);
        let info = ev.incoming("info").await;
        assert_eq!(info[0]["auth"], json!({"token": "secret"}));
        if version == SocketIoVersion::V2 {
            // v2 has no auth payload: it travels as query parameters.
            assert!(info[0]["query"].as_str().unwrap().contains("token=secret"), "{info:?}");
            assert!(server.frames().contains(&"40/private?token=secret,".to_string()), "{:?}", server.frames());
        }

        let mut cfg = config(server.url(), "/", transport, version);
        cfg.auth = Some(json!("not an object"));
        let (_s, mut ev) = start(cfg);
        assert_eq!(ev.disconnected().await, "connect failed");
        assert!(ev.errors()[0].contains("JSON object"), "{:?}", ev.errors());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headers_query_cookies_and_heartbeat() {
    for (transport, version) in matrix() {
        let server = common::start(Options {
            eio: eio(version),
            ping_interval_ms: 100,
            ping_timeout_ms: 2_000,
            ..Default::default()
        })
        .await;
        let req = SocketIoRequest {
            url: server.url(),
            headers: vec![KeyValue::new("X-Test", "yes"), KeyValue::new("Cookie", "user=1")],
            params: vec![KeyValue::new("room", "lobby"), KeyValue::new("off", "1").disabled()],
            transport,
            version,
            ..Default::default()
        };
        let (s, mut ev, _) = connect(SocketIoConfig::from_request(&req, &|s| s.to_string(), true)).await;
        tokio::time::sleep(Duration::from_millis(450)).await;
        s.emit("info", vec![], false);
        let info = ev.incoming("info").await;
        let label = format!("{transport:?}/{version:?}");
        assert_eq!(info[0]["x-test"], json!("yes"), "{label}");
        let query = info[0]["query"].as_str().unwrap();
        let want_eio = format!("EIO={}", eio(version));
        assert!(query.contains("room=lobby") && query.contains(&want_eio) && !query.contains("off="), "{query}");
        // v4 servers ping and count pongs; v2 clients ping.
        let beats = if version == SocketIoVersion::V2 { &info[0]["pings"] } else { &info[0]["pongs"] };
        assert!(beats.as_u64().unwrap() >= 3, "{label}: {info:?}");
        assert!(s.is_alive());
        // Every request carried the header, the user's cookie and, after
        // the polling handshake, the server's sticky-session cookie.
        let requests = server.requests();
        assert!(!requests.is_empty());
        for (i, r) in requests.iter().enumerate() {
            assert_eq!(r.x_test.as_deref(), Some("yes"), "{label}: {r:?}");
            let cookie = r.cookie.clone().unwrap_or_default();
            assert!(cookie.contains("user=1"), "{label}: {r:?}");
            if i > 0 && transport != SocketIoTransport::WebSocket {
                assert!(cookie.contains("sticky=s1"), "{label}: {r:?}");
            }
            assert!(r.query.contains("room=lobby"), "{label}: {r:?}");
            if version == SocketIoVersion::V2 && r.transport == "polling" {
                assert!(r.query.contains("b64=1"), "{label}: {r:?}");
            }
        }
        if transport == SocketIoTransport::Polling {
            assert!(requests.iter().any(|r| r.method == "POST"), "{label}: {requests:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_timeout() {
    for (transport, version) in matrix() {
        let server = common::start(Options {
            eio: eio(version),
            ping_interval_ms: 100,
            ping_timeout_ms: 100,
            ..Default::default()
        })
        .await;
        let (s, mut ev, _) = connect(config(server.url(), "/", transport, version)).await;
        s.emit("go-silent", vec![], false);
        let started = std::time::Instant::now();
        assert_eq!(ev.disconnected().await, "ping timeout", "{transport:?}/{version:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
        let errors = ev.errors();
        let want = if version == SocketIoVersion::V2 { "No pong" } else { "No ping" };
        assert!(errors.iter().any(|e| e.contains(want)), "{errors:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_attachments_become_base64() {
    for (transport, version) in matrix() {
        let server = server(version).await;
        let (s, mut ev, _) = connect(config(server.url(), "/bin", transport, version)).await;
        s.emit("binary", vec![], false);
        assert_eq!(ev.incoming("binary").await, vec![json!("AQID"), json!("text")]);
        let id = s.emit("binary-ack", vec![], true).unwrap();
        assert_eq!(ev.ack(id).await, vec![json!({"file": "aGk="})]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_arguments_are_sent_as_attachments() {
    let p = r#"{"_placeholder":true,"num":"#;
    for (transport, version) in matrix() {
        let label = format!("{transport:?}/{version:?}");
        let server = server(version).await;
        let (s, mut ev, _) = connect(config(server.url(), "/bin", transport, version)).await;
        let args = vec![EmitArg::Binary(vec![1, 2, 3]), EmitArg::Object(vec![("f".into(), vec![0xffu8].into())])];
        s.emit_args("upload", args, false);
        let out = ev
            .wait_for(
                "outgoing upload",
                |e| matches!(e, SocketIoEvent::Event { name, direction: Direction::Outgoing, .. } if name == "upload"),
            )
            .await;
        let SocketIoEvent::Event { args, .. } = out else { unreachable!() };
        assert_eq!(args, vec![json!("AQID"), json!({"f": "/w=="})], "{label}");
        s.emit("ask", vec![], false);
        ev.incoming("question").await;
        assert!(s.ack_args(s.pending_acks()[0].id, vec![EmitArg::Binary(vec![7])]));

        // The packets, then each attachment as an Engine.IO binary message.
        let event = format!(r#"452-/bin,["upload",{p}0}},{{"f":{p}1}}}}]"#);
        let ack = format!("461-/bin,101[{p}0}}]");
        let v2 = version == SocketIoVersion::V2;
        let polling = |b: &str| format!("b{}{b}", if v2 { "4" } else { "" });
        let ws = |b: &[u8]| if v2 { [&[4u8][..], b].concat() } else { b.to_vec() };
        wait_until(&label, || {
            let frames = server.frames();
            frames.contains(&ack) && (frames.contains(&polling("Bw==")) || server.binary_frames().contains(&ws(&[7])))
        })
        .await;
        let frames = server.frames();
        let i = frames.iter().position(|f| *f == event).unwrap_or_else(|| panic!("{label}: {frames:?}"));
        match transport {
            SocketIoTransport::Polling => {
                assert_eq!(frames[i + 1..i + 3], [polling("AQID"), polling("/w==")], "{label}");
                let j = frames.iter().position(|f| *f == ack).unwrap();
                assert_eq!(frames[j + 1], polling("Bw=="), "{label}");
                assert!(server.binary_frames().is_empty(), "{label}");
            }
            SocketIoTransport::WebSocket => {
                assert_eq!(server.binary_frames(), vec![ws(&[1, 2, 3]), ws(&[0xff]), ws(&[7])], "{label}");
                assert!(!frames.iter().any(|f| f.starts_with('b')), "{label}: {frames:?}");
            }
            // Before, during or after the upgrade.
            SocketIoTransport::Auto => {}
        }
        assert!(ev.errors().is_empty(), "{label}: {:?}", ev.errors());
    }
}

/// An Engine.IO 4 polling server that connects the root namespace, then
/// answers the next poll with an 80 MiB chunked body (no Content-Length).
async fn chunked_polling_server() -> String {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let polls = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let polls = polls.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut b = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&b[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let reply = |body: &str| {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                };
                let text = if head.starts_with("POST") {
                    reply("ok")
                } else if !head.contains("sid=") {
                    reply(
                        r#"0{"sid":"big","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}"#,
                    )
                } else if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                    reply(r#"40{"sid":"sock-big"}"#)
                } else {
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
                        .await;
                    let chunk =
                        [format!("{:x}\r\n4", 1 << 20).into_bytes(), vec![b'x'; (1 << 20) - 1], b"\r\n".to_vec()];
                    for _ in 0..80 {
                        if s.write_all(&chunk.concat()).await.is_err() {
                            return;
                        }
                    }
                    "0\r\n\r\n".to_string()
                };
                let _ = s.write_all(text.as_bytes()).await;
            });
        }
    });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_payloads_end_the_session() {
    let v4 = SocketIoVersion::V4;
    let server = server(v4).await;
    let incoming = |ev: &Events, name: &str| {
        ev.seen
            .iter()
            .any(|e| matches!(e, SocketIoEvent::Event { name: n, direction: Direction::Incoming, .. } if n == name))
    };
    // A polling response over the limit (WebSocket messages have tungstenite's):
    // announced by Content-Length, or streamed without a length.
    let streaming = chunked_polling_server().await;
    for url in [server.url(), streaming] {
        let (s, mut ev, _) = connect(config(url, "/", SocketIoTransport::Polling, v4)).await;
        s.emit("huge", vec![], false);
        assert_eq!(ev.disconnected().await, "transport error");
        assert!(ev.errors().iter().any(|e| e.contains("exceeded the 64 MiB limit")), "{:?}", ev.errors());
        assert!(!incoming(&ev, "huge"));
    }

    // More attachments than accepted, or too many attachment bytes.
    let cases = [
        ("many-attachments", "many", "binary attachments in one packet", &TRANSPORTS[..]),
        ("big-attachments", "big", "exceed the 64 MiB limit", &[SocketIoTransport::WebSocket][..]),
    ];
    for (emit, name, want, transports) in cases {
        for &transport in transports {
            let (s, mut ev, _) = connect(config(server.url(), "/", transport, v4)).await;
            s.emit(emit, vec![], false);
            assert_eq!(ev.disconnected().await, "parse error", "{emit} {transport:?}");
            assert!(ev.errors().iter().any(|e| e.contains(want)), "{emit} {transport:?}: {:?}", ev.errors());
            assert!(!incoming(&ev, name));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_event_names() {
    let server = server(SocketIoVersion::V4).await;
    let (s, mut ev, _) = connect(config(server.url(), "/", SocketIoTransport::Auto, SocketIoVersion::V4)).await;
    s.emit("disconnect", vec![], false);
    ev.wait_for("reserved error", |e| matches!(e, SocketIoEvent::Error(m) if m.contains("reserved"))).await;
    s.emit("", vec![], false);
    ev.wait_for("empty error", |e| matches!(e, SocketIoEvent::Error(m) if m.contains("empty"))).await;
    s.emit("echo", vec![json!("still ok")], false);
    ev.incoming("echo").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_errors() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let server = server(SocketIoVersion::V4).await;
    for transport in TRANSPORTS {
        let v4 = SocketIoVersion::V4;
        let (s, mut ev) = start(config(format!("http://127.0.0.1:{port}"), "/", transport, v4));
        assert_eq!(ev.disconnected().await, "connect failed");
        assert!(ev.errors()[0].contains("refused"), "{:?}", ev.errors());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!s.is_alive());

        let mut cfg = config(server.url(), "/", transport, v4);
        cfg.path = "/wrong".into();
        let (_s, mut ev) = start(cfg);
        assert_eq!(ev.disconnected().await, "connect failed");
        let err = &ev.errors()[0];
        assert!(err.contains("HTTP 400") && err.contains("Transport unknown"), "{err}");

        let (_s, mut ev) = start(config("http://host.invalid".into(), "/", transport, v4));
        assert_eq!(ev.disconnected().await, "connect failed");
        assert_eq!(ev.errors().len(), 1, "{:?}", ev.errors());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_mismatch_is_explained() {
    // A v4 client talking to a Socket.IO v2 server over polling.
    let v2 = server(SocketIoVersion::V2).await;
    let (_s, mut ev) = start(config(v2.url(), "/", SocketIoTransport::Polling, SocketIoVersion::V4));
    assert_eq!(ev.disconnected().await, "connect failed");
    assert!(ev.errors()[0].contains("choose protocol v2"), "{:?}", ev.errors());
    // A v2 client talking to a v4 server that does not allow EIO 3.
    let v4 = server(SocketIoVersion::V4).await;
    let (_s, mut ev) = start(config(v4.url(), "/", SocketIoTransport::Auto, SocketIoVersion::V2));
    assert_eq!(ev.disconnected().await, "connect failed");
    assert!(ev.errors()[0].contains("allowEIO3"), "{:?}", ev.errors());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_timeout_and_cancel() {
    // Accepts TCP connections but never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    for transport in TRANSPORTS {
        let mut cfg = config(format!("http://127.0.0.1:{port}"), "/", transport, SocketIoVersion::V4);
        cfg.connect_timeout = Duration::from_secs(1);
        let (_s, mut ev) = start(cfg.clone());
        assert_eq!(ev.disconnected().await, "connect failed");
        assert!(ev.errors()[0].contains("timed out"), "{:?}", ev.errors());

        let (s, mut ev) = start(cfg);
        tokio::time::sleep(Duration::from_millis(100)).await;
        s.disconnect();
        assert_eq!(ev.disconnected().await, "cancelled");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_connection() {
    for (transport, version) in matrix() {
        let server = common::start(Options { eio: eio(version), tls: true, ..Default::default() }).await;
        assert!(server.url().starts_with("https://"));
        // Self-signed certificate: accepted only with verification off.
        let mut cfg = config(server.url(), "/secure", transport, version);
        cfg.verify_ssl = false;
        let (s, mut ev, _) = connect(cfg).await;
        let id = s.emit("echo", vec![json!("over tls")], true).unwrap();
        assert_eq!(ev.incoming("echo").await, vec![json!("over tls")]);
        assert_eq!(ev.ack(id).await, vec![json!("over tls")]);
        if transport == SocketIoTransport::Auto {
            wait_until("upgrade over TLS", || s.transport() == Some(ActiveTransport::WebSocket)).await;
        }

        let (_s, mut ev) = start(config(server.url(), "/secure", transport, version));
        assert_eq!(ev.disconnected().await, "connect failed");
        let errors = ev.errors();
        assert!(errors.len() == 1 && errors[0].to_lowercase().contains("certificate"), "{errors:?}");
    }
}
