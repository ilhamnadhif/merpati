//! Interop with the real Socket.IO servers (Node.js), run on demand:
//!
//! ```text
//! npm install socket.io@4   # in some folder A
//! npm install socket.io@2   # in some folder B
//! MERPATI_SIO4_MODULES=A/node_modules MERPATI_SIO2_MODULES=B/node_modules \
//!   cargo test -p merpati-socketio --test real_servers -- --ignored
//! ```
//!
//! `tests/node/server.js` starts a default server (polling + WebSocket), a
//! polling-only and a WebSocket-only server, one that delays upgrades, plus
//! (v4) ones with `allowEIO3: true` for the v2 client. Binary arguments are
//! checked byte for byte: the server describes every `Buffer` it receives.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use merpati_socketio::{
    ActiveTransport, Direction, EmitArg, SocketIoConfig, SocketIoEvent, SocketIoSession, SocketIoTransport,
    SocketIoVersion,
};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

const TRANSPORTS: [SocketIoTransport; 3] =
    [SocketIoTransport::Auto, SocketIoTransport::WebSocket, SocketIoTransport::Polling];

struct Node {
    _child: tokio::process::Child,
    version: String,
    ports: HashMap<String, u16>,
}

impl Node {
    fn url(&self, server: &str) -> String {
        format!("http://127.0.0.1:{}", self.ports[server])
    }
}

/// Starts `tests/node/server.js` with the given node_modules, or `None`
/// (test skipped) when the variable is not set.
async fn node(env: &str) -> Option<Node> {
    let Ok(modules) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a node_modules folder containing socket.io");
        return None;
    };
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/node/server.js");
    let mut child = tokio::process::Command::new("node")
        .arg(script)
        .env("NODE_PATH", modules)
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("node");
    let stdout = child.stdout.take().unwrap();
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line()).await.unwrap().unwrap().unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    let ports = v["ports"].as_object().unwrap().iter().map(|(k, p)| (k.clone(), p.as_u64().unwrap() as u16)).collect();
    let version = v["version"].as_str().unwrap().to_string();
    eprintln!("socket.io {version} on {ports:?}");
    Some(Node { _child: child, version, ports })
}

struct Events {
    rx: mpsc::UnboundedReceiver<SocketIoEvent>,
    seen: Vec<SocketIoEvent>,
    label: String,
}

impl Events {
    async fn wait_for(&mut self, what: &str, pred: impl Fn(&SocketIoEvent) -> bool) -> SocketIoEvent {
        let r = tokio::time::timeout(Duration::from_secs(15), async {
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

    async fn incoming(&mut self, name: &str) -> Vec<Value> {
        let e = self
            .wait_for(
                &format!("incoming {name}"),
                |e| matches!(e, SocketIoEvent::Event { name: n, direction: Direction::Incoming, .. } if n == name),
            )
            .await;
        let SocketIoEvent::Event { args, .. } = e else { unreachable!() };
        args
    }

    async fn ack(&mut self, id: u64) -> Vec<Value> {
        match self.wait_for(&format!("ack {id}"), |e| matches!(e, SocketIoEvent::Ack { id: i, .. } if *i == id)).await {
            SocketIoEvent::Ack { args, .. } => args,
            _ => unreachable!(),
        }
    }

    async fn connected(&mut self) -> String {
        match self.wait_for("connected", |e| matches!(e, SocketIoEvent::Connected { .. })).await {
            SocketIoEvent::Connected { sid, .. } => sid,
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

fn start(cfg: SocketIoConfig) -> (SocketIoSession, Events) {
    let label = format!("{:?}/{:?}/{}", cfg.transport, cfg.version, cfg.namespace);
    let (tx, rx) = mpsc::unbounded_channel();
    let s = SocketIoSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| {
        let _ = tx.send(e);
    });
    (s, Events { rx, seen: Vec::new(), label })
}

fn cfg(url: String, ns: &str, transport: SocketIoTransport, version: SocketIoVersion) -> SocketIoConfig {
    SocketIoConfig {
        url,
        namespace: ns.into(),
        transport,
        version,
        connect_timeout: Duration::from_secs(10),
        headers: vec![("X-Test".into(), "yes".into()), ("Cookie".into(), "user=1".into())],
        query: vec![("room".into(), "lobby".into())],
        ..Default::default()
    }
}

/// An ack-requesting emit, returning the server's ack args.
async fn call(s: &SocketIoSession, ev: &mut Events, event: &str) -> Vec<Value> {
    let id = s.emit(event, vec![], true).unwrap();
    ev.ack(id).await
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Emits binary arguments (top-level and nested, including an empty buffer)
/// with an ack; the server answers with what it received, every `Buffer` as
/// `{buffer: hex}`. Returns the transport the server saw the event on.
async fn upload(s: &SocketIoSession, ev: &mut Events, label: &str) -> String {
    let bytes: Vec<u8> = (0..=255u8).rev().collect();
    let args = vec![
        EmitArg::Binary(bytes.clone()),
        EmitArg::Json(json!("text")),
        EmitArg::Object(vec![
            ("name".into(), json!("a.bin").into()),
            ("parts".into(), EmitArg::Array(vec![vec![0u8, 1].into(), json!(7).into(), EmitArg::Binary(Vec::new())])),
        ]),
    ];
    let id = s.emit_args("upload", args, true).unwrap();
    let mut got = ev.ack(id).await;
    let want = json!([
        {"buffer": hex(&bytes)},
        "text",
        {"name": "a.bin", "parts": [{"buffer": "0001"}, 7, {"buffer": ""}]},
    ]);
    assert_eq!(got[0]["args"], want, "{label}");
    got[0]["transport"].take().as_str().unwrap_or_default().to_string()
}

async fn suite(node: &Node, server: &str, version: SocketIoVersion) {
    let url = node.url(server);
    for transport in TRANSPORTS {
        let label = format!("socket.io {} [{server}] {transport:?}/{version:?}", node.version);
        eprintln!("{label}");
        // Root namespace: args, acks both ways, binary, headers, cookies.
        let (s, mut ev) = start(cfg(url.clone(), "/", transport, version));
        ev.connected().await;
        // Binary emit right away (Auto: usually while the upgrade is under way).
        let first = upload(&s, &mut ev, &label).await;
        match transport {
            SocketIoTransport::Polling => assert_eq!(first, "polling", "{label}"),
            SocketIoTransport::WebSocket => assert_eq!(first, "websocket", "{label}"),
            SocketIoTransport::Auto => eprintln!("  first binary emit went over {first}"),
        }
        let args = vec![json!("hi"), json!({"a": [1, 2], "é": "😀"}), json!(null), json!(3.5), json!(true)];
        let id = s.emit("echo", args.clone(), true).unwrap();
        assert_eq!(ev.incoming("echo").await, args, "{label}");
        assert_eq!(ev.ack(id).await, args, "{label}");

        s.emit("ask", vec![], false);
        assert_eq!(ev.incoming("question").await, vec![json!("ready?")], "{label}");
        let pending = s.pending_acks();
        assert_eq!(pending.len(), 1, "{label}");
        s.ack(pending[0].id, vec![json!("yes"), json!(42)]);
        assert_eq!(ev.incoming("answered").await, vec![json!("yes"), json!(42)], "{label}");

        s.emit("binary", vec![], false);
        assert_eq!(ev.incoming("binary").await, vec![json!("AQID"), json!("text")], "{label}");
        assert_eq!(call(&s, &mut ev, "binary-ack").await, vec![json!({"file": "aGk="})], "{label}");

        s.emit("info", vec![], false);
        let info = ev.incoming("info").await;
        assert_eq!(info[0]["xTest"], json!("yes"), "{label}");
        assert!(info[0]["cookie"].as_str().unwrap_or_default().contains("user=1"), "{label}: {info:?}");
        assert_eq!(info[0]["query"]["room"], json!("lobby"), "{label}");
        assert_eq!(info[0]["query"]["EIO"], json!(if version == SocketIoVersion::V2 { "3" } else { "4" }));

        // The transport on both ends.
        let want = match transport {
            SocketIoTransport::Polling => ActiveTransport::Polling,
            _ => ActiveTransport::WebSocket,
        };
        for _ in 0..100 {
            if s.transport() == Some(want) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(s.transport(), Some(want), "{label}");
        assert_eq!(call(&s, &mut ev, "transport").await, vec![json!(want.label())], "{label}");

        // Binary emits and binary ack replies on the final transport.
        assert_eq!(upload(&s, &mut ev, &label).await, want.label(), "{label}");
        s.emit_args("upload", vec![EmitArg::Binary(b"no ack".to_vec())], false);
        let uploaded = ev.incoming("uploaded").await;
        assert_eq!(uploaded[0]["args"], json!([{"buffer": hex(b"no ack")}]), "{label}");
        s.emit("ask-binary", vec![], false);
        assert_eq!(ev.incoming("question-binary").await, vec![json!("send bytes")], "{label}");
        let pending = s.pending_acks();
        assert_eq!(pending.len(), 1, "{label}");
        let reply = vec![
            EmitArg::Binary(vec![9, 8, 7]),
            EmitArg::Object(vec![("ok".into(), json!(true).into()), ("raw".into(), vec![0xffu8].into())]),
        ];
        assert!(s.ack_args(pending[0].id, reply));
        assert_eq!(
            ev.incoming("answered-binary").await,
            vec![json!([{"buffer": "090807"}, {"ok": true, "raw": {"buffer": "ff"}}])],
            "{label}"
        );

        // Every request carried the header and cookies, including the
        // server's sticky cookie after the polling handshake.
        // (A direct WebSocket handshake carries no sid; `info` covered it.)
        let requests = call(&s, &mut ev, "requests").await;
        let requests = requests[0].as_array().unwrap();
        assert_eq!(requests.is_empty(), transport == SocketIoTransport::WebSocket, "{label}");
        for r in requests {
            assert_eq!(r["xTest"], json!("yes"), "{label}: {r}");
            assert!(r["cookie"].as_str().unwrap_or_default().contains("user=1"), "{label}: {r}");
            if transport != SocketIoTransport::WebSocket {
                // Requests with a sid come after the handshake.
                assert!(r["cookie"].as_str().unwrap_or_default().contains("sticky="), "{label}: {r}");
            }
        }
        if transport == SocketIoTransport::Polling {
            assert!(requests.iter().any(|r| r["method"] == "POST"), "{label}");
        }

        // Heartbeats keep the session alive past pingInterval + pingTimeout.
        tokio::time::sleep(Duration::from_millis(3_500)).await;
        assert!(s.is_alive(), "{label}");
        assert_eq!(call(&s, &mut ev, "echo").await, Vec::<Value>::new(), "{label}");
        assert!(ev.errors().is_empty(), "{label}: {:?}", ev.errors());

        s.disconnect();
        assert_eq!(ev.disconnected().await, "io client disconnect", "{label}");

        // Custom namespace and server disconnect.
        let (s, mut ev) = start(cfg(url.clone(), "/admin", transport, version));
        let sid = ev.connected().await;
        s.emit("info", vec![], false);
        let info = ev.incoming("info").await;
        assert_eq!(info[0]["nsp"], json!("/admin"), "{label}");
        assert_eq!(info[0]["id"], json!(sid), "{label}");
        s.emit("kick", vec![], false);
        assert_eq!(ev.disconnected().await, "io server disconnect", "{label}");

        // Namespace middleware with handshake auth (v2: query parameters).
        let (_s, mut ev) = start(cfg(url.clone(), "/private", transport, version));
        assert_eq!(ev.disconnected().await, "connect error", "{label}");
        assert!(ev.errors()[0].contains("not authorized"), "{label}: {:?}", ev.errors());
        let mut c = cfg(url.clone(), "/private", transport, version);
        c.auth = Some(json!({"token": "secret"}));
        let (s, mut ev) = start(c);
        ev.connected().await;
        s.emit("info", vec![], false);
        let info = ev.incoming("info").await;
        if version == SocketIoVersion::V4 {
            assert_eq!(info[0]["auth"], json!({"token": "secret"}), "{label}");
        } else {
            assert_eq!(info[0]["query"]["token"], json!("secret"), "{label}");
        }
    }
}

/// Auto against a server that answers upgrade probes after a second: binary
/// emits and acks go over long-polling first, then over the WebSocket.
async fn binary_across_upgrade(node: &Node, server: &str, version: SocketIoVersion) {
    let label = format!("socket.io {} [{server}] Auto/{version:?}", node.version);
    eprintln!("{label}");
    let (s, mut ev) = start(cfg(node.url(server), "/", SocketIoTransport::Auto, version));
    ev.connected().await;
    assert_eq!(upload(&s, &mut ev, &label).await, "polling", "{label}");
    s.emit("ask-binary", vec![], false);
    ev.incoming("question-binary").await;
    let id = s.pending_acks()[0].id;
    // Two binary packets back to back (they may share one polling payload).
    let ack = s.emit_args("upload", vec![EmitArg::Binary(vec![1, 2])], true).unwrap();
    s.ack_args(id, vec![EmitArg::Binary(vec![3])]);
    let got = ev.ack(ack).await;
    assert_eq!(got[0]["args"], json!([{"buffer": "0102"}]), "{label}");
    // Either may arrive first.
    let answered = ev.seen.iter().find_map(|e| match e {
        SocketIoEvent::Event { name, args, .. } if name == "answered-binary" => Some(args.clone()),
        _ => None,
    });
    let answered = match answered {
        Some(a) => a,
        None => ev.incoming("answered-binary").await,
    };
    assert_eq!(answered, vec![json!([{"buffer": "03"}])], "{label}");
    for _ in 0..100 {
        if s.transport() == Some(ActiveTransport::WebSocket) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(s.transport(), Some(ActiveTransport::WebSocket), "{label}");
    assert_eq!(upload(&s, &mut ev, &label).await, "websocket", "{label}");
    assert!(ev.errors().is_empty(), "{label}: {:?}", ev.errors());
    s.disconnect();
    assert_eq!(ev.disconnected().await, "io client disconnect", "{label}");
}

async fn restricted_servers(node: &Node, version: SocketIoVersion) {
    let label = format!("socket.io {}", node.version);
    // Polling-only: auto stays on polling; WebSocket-only mode is refused.
    let (s, mut ev) = start(cfg(node.url("polling"), "/", SocketIoTransport::Auto, version));
    ev.connected().await;
    assert_eq!(call(&s, &mut ev, "transport").await, vec![json!("polling")], "{label}");
    assert_eq!(s.transport(), Some(ActiveTransport::Polling));
    let (_s, mut ev) = start(cfg(node.url("polling"), "/", SocketIoTransport::WebSocket, version));
    assert_eq!(ev.disconnected().await, "connect failed", "{label}");
    eprintln!("websocket against polling-only: {:?}", ev.errors());
    assert!(ev.errors()[0].contains("Transport unknown") && ev.errors()[0].contains("Polling transport"));

    // WebSocket-only: auto falls back to WebSocket; polling mode is refused.
    let (s, mut ev) = start(cfg(node.url("websocket"), "/", SocketIoTransport::Auto, version));
    ev.connected().await;
    assert_eq!(call(&s, &mut ev, "transport").await, vec![json!("websocket")], "{label}");
    let (_s, mut ev) = start(cfg(node.url("websocket"), "/", SocketIoTransport::Polling, version));
    assert_eq!(ev.disconnected().await, "connect failed", "{label}");
    eprintln!("polling against websocket-only: {:?}", ev.errors());
    assert!(ev.errors()[0].contains("Transport unknown") && ev.errors()[0].contains("WebSocket transport"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Node.js and socket.io@4 (MERPATI_SIO4_MODULES)"]
async fn socket_io_v4_server() {
    let Some(node) = node("MERPATI_SIO4_MODULES").await else { return };
    suite(&node, "default", SocketIoVersion::V4).await;
    binary_across_upgrade(&node, "slow-upgrade", SocketIoVersion::V4).await;
    restricted_servers(&node, SocketIoVersion::V4).await;
    // The v2 client against a v4 server with allowEIO3, and without it.
    suite(&node, "eio3", SocketIoVersion::V2).await;
    binary_across_upgrade(&node, "eio3-slow-upgrade", SocketIoVersion::V2).await;
    let (_s, mut ev) = start(cfg(node.url("default"), "/", SocketIoTransport::Auto, SocketIoVersion::V2));
    assert_eq!(ev.disconnected().await, "connect failed");
    eprintln!("v2 client against v4 without allowEIO3: {:?}", ev.errors());
    assert!(ev.errors()[0].contains("allowEIO3"), "{:?}", ev.errors());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Node.js and socket.io@2 (MERPATI_SIO2_MODULES)"]
async fn socket_io_v2_server() {
    let Some(node) = node("MERPATI_SIO2_MODULES").await else { return };
    suite(&node, "default", SocketIoVersion::V2).await;
    binary_across_upgrade(&node, "slow-upgrade", SocketIoVersion::V2).await;
    restricted_servers(&node, SocketIoVersion::V2).await;
    // The v4 client against a v2 server explains the mismatch.
    let (_s, mut ev) = start(cfg(node.url("default"), "/", SocketIoTransport::Polling, SocketIoVersion::V4));
    assert_eq!(ev.disconnected().await, "connect failed");
    eprintln!("v4 client against v2 server: {:?}", ev.errors());
    assert!(ev.errors()[0].contains("choose protocol v2"), "{:?}", ev.errors());
}
