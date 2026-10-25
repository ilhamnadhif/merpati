//! Integration tests against a minimal in-process MQTT broker (3.1.1 and 5,
//! over TCP and WebSocket) that implements just enough of the protocol for
//! CONNECT, SUBSCRIBE/UNSUBSCRIBE, PUBLISH QoS 0/1/2 (MQTT 5 properties are
//! passed on), retained messages, last wills, PING and DISCONNECT.

use std::time::Duration;

use merpati_core::Auth;
use merpati_core::protocols::{MqttMessage, MqttRequest};
use merpati_mqtt::{Direction, MessageProperties, MqttConfig, MqttEvent, MqttSession, MqttVersion, Publish};
use tokio::sync::mpsc;

#[path = "../../merpati-http/tests/support/proxies.rs"]
mod proxies;

// The handshake callback signature is fixed by tungstenite.
#[allow(clippy::result_large_err)]
mod broker {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_rustls::TlsAcceptor;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    #[derive(Clone, Debug)]
    struct Deliver {
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        retain: bool,
        /// MQTT 5 property block of the PUBLISH (without its length),
        /// forwarded to MQTT 5 subscribers.
        props: Vec<u8>,
    }

    struct Sub {
        conn: u64,
        filter: String,
        qos: u8,
        tx: mpsc::UnboundedSender<Deliver>,
    }

    #[derive(Default)]
    struct State {
        subs: Vec<Sub>,
        retained: HashMap<String, (Vec<u8>, u8, Vec<u8>)>,
        next_conn: u64,
        ws_paths: Vec<String>,
    }

    pub struct Broker {
        pub port: u16,
        pub ws_port: u16,
        pub tls_port: u16,
        pub wss_port: u16,
        state: Arc<Mutex<State>>,
    }

    impl Broker {
        pub fn url(&self) -> String {
            format!("mqtt://127.0.0.1:{}", self.port)
        }

        pub fn ws_url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.ws_port)
        }

        pub fn tls_url(&self) -> String {
            format!("mqtts://127.0.0.1:{}", self.tls_port)
        }

        pub fn wss_url(&self) -> String {
            format!("wss://127.0.0.1:{}", self.wss_port)
        }

        pub fn ws_paths(&self) -> Vec<String> {
            self.state.lock().unwrap().ws_paths.clone()
        }
    }

    /// Starts the broker on random ports (TCP, WebSocket, TLS and secure
    /// WebSocket with a self-signed certificate); `credentials` makes it
    /// require that username/password.
    pub async fn start(credentials: Option<(&str, &str)>) -> Broker {
        let state = Arc::new(Mutex::new(State::default()));
        let creds = credentials.map(|(u, p)| (u.to_string(), p.to_string()));
        let tls = super::tls_acceptor();
        let mut ports = Vec::new();
        for (tls, ws) in [(None, false), (None, true), (Some(tls.clone()), false), (Some(tls), true)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            ports.push(listener.local_addr().unwrap().port());
            tokio::spawn(listen(listener, tls, ws, state.clone(), creds.clone()));
        }
        Broker { port: ports[0], ws_port: ports[1], tls_port: ports[2], wss_port: ports[3], state }
    }

    async fn listen(
        listener: TcpListener,
        tls: Option<TlsAcceptor>,
        ws: bool,
        state: Arc<Mutex<State>>,
        creds: Option<(String, String)>,
    ) {
        while let Ok((stream, _)) = listener.accept().await {
            let (tls, state, creds) = (tls.clone(), state.clone(), creds.clone());
            tokio::spawn(async move {
                let stream: Box<dyn Stream> = match tls {
                    Some(acceptor) => match acceptor.accept(stream).await {
                        Ok(s) => Box::new(s),
                        Err(_) => return,
                    },
                    None => Box::new(stream),
                };
                if !ws {
                    return handle(Io::Raw(stream), state, creds).await;
                }
                let paths = state.clone();
                let callback = move |req: &Request, mut resp: Response| {
                    paths.lock().unwrap().ws_paths.push(req.uri().path().to_string());
                    resp.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("mqtt"));
                    Ok(resp)
                };
                if let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await {
                    handle(Io::Ws(Box::new(ws)), state, creds).await;
                }
            });
        }
    }

    trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
    impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

    enum Io {
        Raw(Box<dyn Stream>),
        Ws(Box<WebSocketStream<Box<dyn Stream>>>),
    }

    impl Io {
        async fn read(&mut self) -> Option<Vec<u8>> {
            match self {
                Io::Raw(s) => {
                    let mut buf = vec![0; 16 * 1024];
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => None,
                        Ok(n) => {
                            buf.truncate(n);
                            Some(buf)
                        }
                    }
                }
                Io::Ws(ws) => loop {
                    match ws.next().await {
                        Some(Ok(Message::Binary(b))) => return Some(b.to_vec()),
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                        Some(Ok(_)) => continue,
                    }
                },
            }
        }

        async fn write(&mut self, data: Vec<u8>) -> bool {
            match self {
                Io::Raw(s) => s.write_all(&data).await.is_ok(),
                Io::Ws(ws) => ws.send(Message::binary(data)).await.is_ok(),
            }
        }
    }

    struct Reader<'a> {
        b: &'a [u8],
        i: usize,
    }

    impl Reader<'_> {
        fn u8(&mut self) -> u8 {
            let v = self.b[self.i];
            self.i += 1;
            v
        }
        fn u16(&mut self) -> u16 {
            let v = u16::from_be_bytes([self.b[self.i], self.b[self.i + 1]]);
            self.i += 2;
            v
        }
        fn bin(&mut self) -> Vec<u8> {
            let n = self.u16() as usize;
            let v = self.b[self.i..self.i + n].to_vec();
            self.i += n;
            v
        }
        fn string(&mut self) -> String {
            String::from_utf8(self.bin()).unwrap()
        }
        fn varint(&mut self) -> usize {
            let (mut v, mut mult) = (0usize, 1usize);
            loop {
                let b = self.u8();
                v += (b & 127) as usize * mult;
                mult *= 128;
                if b & 128 == 0 {
                    return v;
                }
            }
        }
        /// Skips an MQTT 5 property block.
        fn props(&mut self) {
            let n = self.varint();
            self.i += n;
        }
        /// An MQTT 5 property block, without its length.
        fn props_raw(&mut self) -> Vec<u8> {
            let n = self.varint();
            let v = self.b[self.i..self.i + n].to_vec();
            self.i += n;
            v
        }
        fn done(&self) -> bool {
            self.i >= self.b.len()
        }
        fn rest(&mut self) -> Vec<u8> {
            let v = self.b[self.i..].to_vec();
            self.i = self.b.len();
            v
        }
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

    fn put_varint(out: &mut Vec<u8>, mut n: usize) {
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
    }

    fn packet(header: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![header];
        put_varint(&mut out, body.len());
        out.extend_from_slice(body);
        out
    }

    fn put_str(out: &mut Vec<u8>, s: &str) {
        out.extend((s.len() as u16).to_be_bytes());
        out.extend(s.as_bytes());
    }

    fn matches(topic: &str, filter: &str) -> bool {
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

    fn route(state: &Mutex<State>, d: Deliver) {
        let mut s = state.lock().unwrap();
        if d.retain {
            if d.payload.is_empty() {
                s.retained.remove(&d.topic);
            } else {
                s.retained.insert(d.topic.clone(), (d.payload.clone(), d.qos, d.props.clone()));
            }
        }
        for sub in s.subs.iter().filter(|sub| matches(&d.topic, &sub.filter)) {
            let _ = sub.tx.send(Deliver { qos: d.qos.min(sub.qos).min(1), retain: false, ..d.clone() });
        }
    }

    async fn handle(mut io: Io, state: Arc<Mutex<State>>, creds: Option<(String, String)>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Deliver>();
        let conn = {
            let mut s = state.lock().unwrap();
            s.next_conn += 1;
            s.next_conn
        };
        let mut buf = Vec::new();
        let mut version = 4u8;
        let mut will: Option<Deliver> = None;
        let mut clean_exit = false;
        let mut next_pkid = 0u16;
        'conn: loop {
            tokio::select! {
                chunk = io.read() => {
                    let Some(chunk) = chunk else { break };
                    buf.extend(chunk);
                    while let Some((header, body)) = take_packet(&mut buf) {
                        let mut r = Reader { b: &body, i: 0 };
                        let mut out = Vec::new();
                        match header >> 4 {
                            1 => {
                                let _protocol = r.string();
                                version = r.u8();
                                let flags = r.u8();
                                let _keep_alive = r.u16();
                                if version == 5 {
                                    r.props();
                                }
                                let _client_id = r.string();
                                if flags & 0x04 != 0 {
                                    if version == 5 {
                                        r.props();
                                    }
                                    let topic = r.string();
                                    let payload = r.bin();
                                    will = Some(Deliver { topic, payload, qos: (flags >> 3) & 3, retain: flags & 0x20 != 0, props: Vec::new() });
                                }
                                let user = (flags & 0x80 != 0).then(|| r.string());
                                let pass = (flags & 0x40 != 0).then(|| String::from_utf8_lossy(&r.bin()).into_owned());
                                let ok = match &creds {
                                    None => true,
                                    Some((u, p)) => user.as_ref() == Some(u) && pass.as_ref() == Some(p),
                                };
                                let code = match (ok, version) {
                                    (true, _) => 0,
                                    (false, 5) => 0x86,
                                    (false, _) => 4,
                                };
                                let body: &[u8] = if version == 5 { &[0, code, 0] } else { &[0, code] };
                                io.write(packet(0x20, body)).await;
                                if !ok {
                                    clean_exit = true;
                                    break 'conn;
                                }
                            }
                            3 => {
                                let qos = (header >> 1) & 3;
                                let retain = header & 1 != 0;
                                let topic = r.string();
                                let pkid = if qos > 0 { r.u16() } else { 0 };
                                let props = if version == 5 { r.props_raw() } else { Vec::new() };
                                let payload = r.rest();
                                if topic == "kick/me" {
                                    if version == 5 {
                                        let reason = b"kicked by test";
                                        let mut body = vec![0x98, 3 + reason.len() as u8, 0x1F];
                                        put_str(&mut body, "kicked by test");
                                        io.write(packet(0xE0, &body)).await;
                                    }
                                    break 'conn;
                                }
                                match qos {
                                    1 => out.push(packet(0x40, &pkid.to_be_bytes())),
                                    2 => out.push(packet(0x50, &pkid.to_be_bytes())),
                                    _ => {}
                                }
                                route(&state, Deliver { topic, payload, qos, retain, props });
                            }
                            6 => {
                                let pkid = r.u16();
                                out.push(packet(0x70, &pkid.to_be_bytes()));
                            }
                            8 => {
                                let pkid = r.u16();
                                if version == 5 {
                                    r.props();
                                }
                                let mut body = pkid.to_be_bytes().to_vec();
                                if version == 5 {
                                    body.push(0);
                                }
                                let mut retained = Vec::new();
                                while !r.done() {
                                    let filter = r.string();
                                    let qos = (r.u8() & 3).min(2);
                                    if filter.starts_with("forbidden") {
                                        body.push(0x80);
                                        continue;
                                    }
                                    body.push(qos);
                                    let mut s = state.lock().unwrap();
                                    s.subs.retain(|x| !(x.conn == conn && x.filter == filter));
                                    for (topic, (payload, q, props)) in &s.retained {
                                        if matches(topic, &filter) {
                                            retained.push(Deliver { topic: topic.clone(), payload: payload.clone(), qos: (*q).min(qos).min(1), retain: true, props: props.clone() });
                                        }
                                    }
                                    s.subs.push(Sub { conn, filter, qos, tx: tx.clone() });
                                }
                                out.push(packet(0x90, &body));
                                for d in retained {
                                    let _ = tx.send(d);
                                }
                            }
                            10 => {
                                let pkid = r.u16();
                                if version == 5 {
                                    r.props();
                                }
                                let mut body = pkid.to_be_bytes().to_vec();
                                if version == 5 {
                                    body.push(0);
                                }
                                while !r.done() {
                                    let filter = r.string();
                                    state.lock().unwrap().subs.retain(|x| !(x.conn == conn && x.filter == filter));
                                    if version == 5 {
                                        body.push(0);
                                    }
                                }
                                out.push(packet(0xB0, &body));
                            }
                            12 => out.push(vec![0xD0, 0]),
                            14 => {
                                clean_exit = true;
                                break 'conn;
                            }
                            _ => {}
                        }
                        for o in out {
                            if !io.write(o).await {
                                break 'conn;
                            }
                        }
                    }
                }
                Some(d) = rx.recv() => {
                    let mut body = Vec::new();
                    put_str(&mut body, &d.topic);
                    if d.qos > 0 {
                        next_pkid = next_pkid.wrapping_add(1).max(1);
                        body.extend(next_pkid.to_be_bytes());
                    }
                    if version == 5 {
                        put_varint(&mut body, d.props.len());
                        body.extend(&d.props);
                    }
                    body.extend(&d.payload);
                    if !io.write(packet(0x30 | (d.qos << 1) | u8::from(d.retain), &body)).await {
                        break;
                    }
                }
            }
        }
        state.lock().unwrap().subs.retain(|s| s.conn != conn);
        if !clean_exit && let Some(w) = will {
            route(&state, w);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A TLS acceptor with a fresh self-signed certificate for 127.0.0.1.
fn tls_acceptor() -> tokio_rustls::TlsAcceptor {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let config =
        rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![ck.cert.der().clone()], key)
            .unwrap();
    tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config))
}

struct Events {
    rx: mpsc::UnboundedReceiver<MqttEvent>,
    seen: Vec<MqttEvent>,
}

impl Events {
    async fn wait_for(&mut self, what: &str, pred: impl Fn(&MqttEvent) -> bool) -> MqttEvent {
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
            _ => panic!("no {what} event; seen: {:#?}", self.seen),
        }
    }

    async fn incoming(&mut self, topic: &str) -> MqttEvent {
        self.wait_for(
            &format!("incoming message on {topic}"),
            |e| matches!(e, MqttEvent::Message { topic: t, direction: Direction::Incoming, .. } if t == topic),
        )
        .await
    }

    async fn disconnected(&mut self) -> String {
        match self.wait_for("disconnected", |e| matches!(e, MqttEvent::Disconnected { .. })).await {
            MqttEvent::Disconnected { reason } => reason,
            _ => unreachable!(),
        }
    }

    fn errors(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter_map(|e| match e {
                MqttEvent::Error(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }
}

fn config(url: String, version: MqttVersion) -> MqttConfig {
    MqttConfig { url, version, connect_timeout: Duration::from_secs(5), ..Default::default() }
}

fn start(cfg: MqttConfig) -> (MqttSession, Events) {
    let (tx, rx) = mpsc::unbounded_channel();
    let session = MqttSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| {
        let _ = tx.send(e);
    });
    (session, Events { rx, seen: Vec::new() })
}

async fn connect(cfg: MqttConfig) -> (MqttSession, Events) {
    let (session, mut events) = start(cfg);
    events.wait_for("connected", |e| matches!(e, MqttEvent::Connected { .. })).await;
    assert_eq!(events.seen[0], MqttEvent::Connecting);
    (session, events)
}

async fn subscribed(events: &mut Events, topic: &str) -> Option<u8> {
    match events
        .wait_for(&format!("suback for {topic}"), |e| matches!(e, MqttEvent::Subscribed { topic: t, .. } if t == topic))
        .await
    {
        MqttEvent::Subscribed { granted_qos, .. } => granted_qos,
        _ => unreachable!(),
    }
}

async fn round_trip(version: MqttVersion, url: String, qos: u8) {
    round_trip_with(config(url, version), qos).await;
}

async fn round_trip_with(cfg: MqttConfig, qos: u8) {
    let (s, mut ev) = connect(cfg).await;
    s.subscribe("test/+", qos);
    assert_eq!(subscribed(&mut ev, "test/+").await, Some(qos));
    s.publish(Publish::new("test/a", "hello").with_qos(qos));
    let out = ev
        .wait_for("outgoing message", |e| matches!(e, MqttEvent::Message { direction: Direction::Outgoing, .. }))
        .await;
    let MqttEvent::Message { topic, payload, qos: q, retain, at_ms, .. } = out else { unreachable!() };
    assert_eq!((topic.as_str(), payload.as_slice(), q, retain), ("test/a", &b"hello"[..], qos, false));
    assert!(at_ms > 0);
    let MqttEvent::Message { payload, qos: q, retain, .. } = ev.incoming("test/a").await else { unreachable!() };
    assert_eq!((payload.as_slice(), q, retain), (&b"hello"[..], qos, false));
    assert!(ev.errors().is_empty(), "{:?}", ev.errors());
    s.disconnect();
    assert_eq!(ev.disconnected().await, "client disconnected");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v311_qos0_round_trip() {
    let broker = broker::start(None).await;
    round_trip(MqttVersion::V311, broker.url(), 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v311_qos1_round_trip() {
    let broker = broker::start(None).await;
    round_trip(MqttVersion::V311, broker.url(), 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v5_qos0_round_trip() {
    let broker = broker::start(None).await;
    round_trip(MqttVersion::V5, broker.url(), 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v5_qos1_round_trip() {
    let broker = broker::start(None).await;
    round_trip(MqttVersion::V5, broker.url(), 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v5_user_properties_round_trip() {
    let broker = broker::start(None).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cfg = config(broker.url(), MqttVersion::V5);
    let s = MqttSession::connect_with_properties(&tokio::runtime::Handle::current(), cfg, move |e, p| {
        let _ = tx.send((e, p));
    });
    let mut next = async |what: &str, pred: &dyn Fn(&MqttEvent) -> bool| {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let (e, p) = rx.recv().await.expect("session ended");
                if pred(&e) {
                    return (e, p);
                }
                assert!(p.is_none(), "properties on a non-message event: {e:?}");
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {what} event"))
    };
    next("connected", &|e| matches!(e, MqttEvent::Connected { .. })).await;
    s.subscribe("props/#", 1);
    next("suback", &|e| matches!(e, MqttEvent::Subscribed { .. })).await;

    // Order and repeated names are kept; values may be empty or non-ASCII.
    let props = MessageProperties {
        user_properties: vec![
            ("trace-id".into(), "abc-123".into()),
            ("tag".into(), "α".into()),
            ("tag".into(), "β 😀".into()),
            ("empty".into(), String::new()),
        ],
    };
    s.publish_with_properties(Publish::new("props/a", "with").with_qos(1), props.clone());
    let out = |e: &MqttEvent| matches!(e, MqttEvent::Message { direction: Direction::Outgoing, .. });
    let inc = |e: &MqttEvent| matches!(e, MqttEvent::Message { direction: Direction::Incoming, .. });
    assert_eq!(next("outgoing", &out).await.1, Some(props.clone()));
    let (e, p) = next("incoming", &inc).await;
    assert!(matches!(e, MqttEvent::Message { topic, payload, .. } if topic == "props/a" && payload == b"with"));
    assert_eq!(p, Some(props));

    // Without properties: Some(empty) on MQTT 5.
    s.publish(Publish::new("props/b", "without"));
    assert_eq!(next("outgoing", &out).await.1, Some(MessageProperties::default()));
    assert_eq!(next("incoming", &inc).await.1, Some(MessageProperties::default()));

    // Strings brokers refuse are not sent.
    let nul = MessageProperties { user_properties: vec![("a\0b".into(), "v".into())] };
    s.publish_with_properties(Publish::new("props/c", "x"), nul);
    let (e, _) = next("error", &|e| matches!(e, MqttEvent::Error(_))).await;
    assert!(matches!(&e, MqttEvent::Error(m) if m.contains("NUL")), "{e:?}");
    s.publish(Publish::new("props/d", "after"));
    let (e, _) = next("outgoing", &out).await;
    assert!(matches!(e, MqttEvent::Message { topic, .. } if topic == "props/d"), "props/c was not sent");
    s.disconnect();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_properties_need_mqtt_5() {
    let broker = broker::start(None).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cfg = config(broker.url(), MqttVersion::V311);
    let s = MqttSession::connect_with_properties(&tokio::runtime::Handle::current(), cfg, move |e, p| {
        let _ = tx.send((e, p));
    });
    s.subscribe("v3/#", 0);
    let props = MessageProperties { user_properties: vec![("k".into(), "v".into())] };
    s.publish_with_properties(Publish::new("v3/a", "refused"), props);
    s.publish(Publish::new("v3/b", "plain"));
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some((e, p)) = rx.recv().await {
            let incoming_b =
                matches!(&e, MqttEvent::Message { topic, direction: Direction::Incoming, .. } if topic == "v3/b");
            seen.push((e, p));
            if incoming_b {
                break;
            }
        }
    })
    .await
    .expect("v3/b delivered");
    assert!(seen.iter().any(|(e, _)| matches!(e, MqttEvent::Error(m) if m.contains("need MQTT 5"))), "{seen:#?}");
    assert!(!seen.iter().any(|(e, _)| matches!(e, MqttEvent::Message { topic, .. } if topic == "v3/a")), "{seen:#?}");
    // MQTT 3.1.1 messages have no properties at all.
    assert!(seen.iter().all(|(_, p)| p.is_none()), "{seen:#?}");
    s.disconnect();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_round_trip_uses_default_path() {
    let broker = broker::start(None).await;
    round_trip(MqttVersion::V311, broker.ws_url(), 1).await;
    round_trip(MqttVersion::V5, broker.ws_url(), 0).await;
    assert_eq!(broker.ws_paths(), vec!["/mqtt".to_string(), "/mqtt".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qos2_publish_completes() {
    let broker = broker::start(None).await;
    let (s, mut ev) = connect(config(broker.url(), MqttVersion::V5)).await;
    s.subscribe("q2/#", 2);
    assert_eq!(subscribed(&mut ev, "q2/#").await, Some(2));
    s.publish(Publish::new("q2/x", vec![0u8, 255, 7]).with_qos(2));
    let MqttEvent::Message { payload, .. } = ev.incoming("q2/x").await else { unreachable!() };
    assert_eq!(payload, vec![0u8, 255, 7]);
    // The QoS 2 flow (PUBREC/PUBREL/PUBCOMP) must not break the connection.
    s.publish(Publish::new("q2/y", "again").with_qos(2));
    ev.incoming("q2/y").await;
    assert!(ev.errors().is_empty(), "{:?}", ev.errors());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_before_connack_are_queued() {
    let broker = broker::start(None).await;
    let (s, mut ev) = start(config(broker.url(), MqttVersion::V311));
    s.subscribe("early/topic", 1);
    s.publish(Publish::new("early/topic", "queued").with_qos(1));
    let MqttEvent::Message { payload, .. } = ev.incoming("early/topic").await else { unreachable!() };
    assert_eq!(payload, b"queued");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_message_is_flagged() {
    let broker = broker::start(None).await;
    let (a, mut a_ev) = connect(config(broker.url(), MqttVersion::V311)).await;
    a.publish(Publish::new("status/device", "online").with_qos(1).with_retain(true));
    a_ev.wait_for("outgoing", |e| matches!(e, MqttEvent::Message { direction: Direction::Outgoing, retain: true, .. }))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (b, mut b_ev) = connect(config(broker.url(), MqttVersion::V5)).await;
    b.subscribe("status/#", 1);
    let MqttEvent::Message { payload, retain, .. } = b_ev.incoming("status/device").await else { unreachable!() };
    assert_eq!(payload, b"online");
    assert!(retain);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribe_stops_delivery() {
    let broker = broker::start(None).await;
    let (s, mut ev) = connect(config(broker.url(), MqttVersion::V5)).await;
    s.subscribe("a/b", 0);
    subscribed(&mut ev, "a/b").await;
    s.unsubscribe("a/b");
    ev.wait_for("unsubscribed", |e| matches!(e, MqttEvent::Unsubscribed { topic } if topic == "a/b")).await;
    s.publish(Publish::new("a/b", "ignored"));
    s.subscribe("c/d", 0);
    subscribed(&mut ev, "c/d").await;
    s.publish(Publish::new("c/d", "seen"));
    ev.incoming("c/d").await;
    let leaked = ev
        .seen
        .iter()
        .any(|e| matches!(e, MqttEvent::Message { topic, direction: Direction::Incoming, .. } if topic == "a/b"));
    assert!(!leaked, "{:#?}", ev.seen);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_subscription_and_invalid_topic() {
    let broker = broker::start(None).await;
    for version in [MqttVersion::V311, MqttVersion::V5] {
        let (s, mut ev) = connect(config(broker.url(), version)).await;
        s.subscribe("forbidden/x", 1);
        assert_eq!(subscribed(&mut ev, "forbidden/x").await, None);
        assert!(ev.errors().iter().any(|e| e.contains("rejected")), "{:?}", ev.errors());
        s.publish(Publish::new("bad/+", "x"));
        ev.wait_for("invalid topic error", |e| matches!(e, MqttEvent::Error(m) if m.contains("Invalid topic"))).await;
        // Still usable afterwards.
        s.subscribe("ok", 0);
        assert_eq!(subscribed(&mut ev, "ok").await, Some(0));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_unsubscribe_filter_is_not_sent() {
    // Brokers drop clients sending an empty or malformed filter (mosquitto:
    // "Empty unsubscription string from ..., disconnecting").
    let broker = broker::start(None).await;
    for version in [MqttVersion::V311, MqttVersion::V5] {
        let (s, mut ev) = connect(config(broker.url(), version)).await;
        for bad in ["", "a/#/b", "a+"] {
            s.unsubscribe(bad);
            ev.wait_for(
                &format!("invalid filter error for {bad:?}"),
                |e| matches!(e, MqttEvent::Error(m) if m.contains("Invalid topic filter")),
            )
            .await;
        }
        s.subscribe("ok", 0);
        assert_eq!(subscribed(&mut ev, "ok").await, Some(0));
        assert!(!ev.seen.iter().any(|e| matches!(e, MqttEvent::Unsubscribed { .. })), "{:#?}", ev.seen);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_last_will_topic_is_refused_before_connecting() {
    let broker = broker::start(None).await;
    for version in [MqttVersion::V311, MqttVersion::V5] {
        let cfg = MqttConfig { last_will: Some(Publish::new("lw/+", "bye")), ..config(broker.url(), version) };
        let (_s, mut ev) = start(cfg);
        assert_eq!(ev.disconnected().await, "connect failed");
        assert!(ev.errors().iter().any(|e| e.contains("last will")), "{:?}", ev.errors());
        assert!(!ev.seen.iter().any(|e| matches!(e, MqttEvent::Connected { .. })), "{:#?}", ev.seen);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_from_request() {
    let broker = broker::start(Some(("alice", "s3cret"))).await;
    for version in [MqttVersion::V311, MqttVersion::V5] {
        let req =
            MqttRequest { url: broker.url(), version, auth: Auth::basic("alice", "{{pw}}"), ..Default::default() };
        let cfg = MqttConfig::from_request(&req, &|s| s.replace("{{pw}}", "s3cret"), true);
        assert!(cfg.client_id.starts_with("merpati-"));
        let (s, mut ev) = connect(cfg).await;
        s.disconnect();
        assert_eq!(ev.disconnected().await, "client disconnected");

        let req = MqttRequest { auth: Auth::basic("alice", "wrong"), ..req };
        let (_s, mut ev) = start(MqttConfig::from_request(&req, &|s| s.to_string(), true));
        assert_eq!(ev.disconnected().await, "connect failed");
        let errors = ev.errors();
        assert!(errors.len() == 1 && errors[0].contains("refused") && errors[0].contains("password"), "{errors:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_from_url() {
    let broker = broker::start(Some(("bob", "p@ss"))).await;
    let url = format!("mqtt://bob:p%40ss@127.0.0.1:{}", broker.port);
    let (s, mut ev) = connect(config(url, MqttVersion::V311)).await;
    s.disconnect();
    ev.disconnected().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_refused_is_reported() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    for version in [MqttVersion::V311, MqttVersion::V5] {
        let (s, mut ev) = start(config(format!("mqtt://127.0.0.1:{port}"), version));
        assert_eq!(ev.disconnected().await, "connect failed");
        assert!(ev.errors()[0].contains("refused"), "{:?}", ev.errors());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!s.is_alive());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_host_and_bad_url_are_reported() {
    let (_s, mut ev) = start(config("mqtt://host.invalid:1883".into(), MqttVersion::V311));
    assert_eq!(ev.disconnected().await, "connect failed");
    assert_eq!(ev.errors().len(), 1);

    let (_s, mut ev) = start(config("http://example.com".into(), MqttVersion::V311));
    assert_eq!(ev.disconnected().await, "connect failed");
    assert!(ev.errors()[0].contains("Unsupported"), "{:?}", ev.errors());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_timeout() {
    // Accepts TCP connections but never answers CONNECT.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    let mut cfg = config(format!("mqtt://127.0.0.1:{port}"), MqttVersion::V311);
    cfg.connect_timeout = Duration::from_secs(1);
    let started = std::time::Instant::now();
    let (_s, mut ev) = start(cfg);
    assert_eq!(ev.disconnected().await, "connect failed");
    assert!(ev.errors()[0].contains("timed out"), "{:?}", ev.errors());
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_while_connecting_cancels() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    let (s, mut ev) = start(config(format!("mqtt://127.0.0.1:{port}"), MqttVersion::V5));
    tokio::time::sleep(Duration::from_millis(100)).await;
    s.disconnect();
    assert_eq!(ev.disconnected().await, "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_disconnect_and_last_will() {
    let broker = broker::start(None).await;
    let (watcher, mut w_ev) = connect(config(broker.url(), MqttVersion::V311)).await;
    watcher.subscribe("will/#", 1);
    subscribed(&mut w_ev, "will/#").await;

    for (version, name) in [(MqttVersion::V311, "v311"), (MqttVersion::V5, "v5")] {
        let req = MqttRequest {
            url: broker.url(),
            version,
            last_will: Some(MqttMessage {
                topic: format!("will/{name}"),
                payload: "gone".into(),
                qos: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        let (s, mut ev) = connect(MqttConfig::from_request(&req, &|s| s.to_string(), true)).await;
        s.publish(Publish::new("kick/me", ""));
        let reason = ev.disconnected().await;
        match version {
            MqttVersion::V5 => {
                assert_eq!(reason, "server disconnected: administrative action (kicked by test)");
                assert!(ev.errors().is_empty(), "{:?}", ev.errors());
            }
            MqttVersion::V311 => {
                assert_eq!(reason, "connection lost");
                assert_eq!(ev.errors().len(), 1, "{:?}", ev.errors());
            }
        }
        let MqttEvent::Message { payload, .. } = w_ev.incoming(&format!("will/{name}")).await else { unreachable!() };
        assert_eq!(payload, b"gone");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!s.is_alive());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_disconnects_cleanly_without_will() {
    let broker = broker::start(None).await;
    let (watcher, mut w_ev) = connect(config(broker.url(), MqttVersion::V5)).await;
    watcher.subscribe("will/#", 0);
    subscribed(&mut w_ev, "will/#").await;
    let mut cfg = config(broker.url(), MqttVersion::V311);
    cfg.last_will = Some(Publish::new("will/dropped", "gone"));
    let (s, mut ev) = connect(cfg).await;
    drop(s);
    assert_eq!(ev.disconnected().await, "client disconnected");
    // A clean DISCONNECT suppresses the will.
    watcher.publish(Publish::new("will/marker", "m"));
    w_ev.incoming("will/marker").await;
    assert!(!w_ev.seen.iter().any(|e| matches!(e, MqttEvent::Message { topic, .. } if topic == "will/dropped")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_and_secure_websocket() {
    let broker = broker::start(None).await;
    for (url, version) in [(broker.tls_url(), MqttVersion::V311), (broker.wss_url(), MqttVersion::V5)] {
        // Self-signed certificate: accepted only with verification off.
        let mut cfg = config(url.clone(), version);
        cfg.verify_ssl = false;
        let (s, mut ev) = connect(cfg).await;
        s.subscribe("secure/#", 1);
        subscribed(&mut ev, "secure/#").await;
        s.publish(Publish::new("secure/x", "over tls").with_qos(1));
        let MqttEvent::Message { payload, .. } = ev.incoming("secure/x").await else { unreachable!() };
        assert_eq!(payload, b"over tls");

        let (_s, mut ev) = start(config(url, version));
        assert_eq!(ev.disconnected().await, "connect failed");
        let errors = ev.errors();
        assert!(errors.len() == 1 && errors[0].to_lowercase().contains("certificate"), "{errors:?}");
    }
    assert_eq!(broker.ws_paths(), vec!["/mqtt".to_string()]);
}

// ---------------------------------------------------------------------------
// Proxies (rumqttc goes through the session's loopback bridge)
// ---------------------------------------------------------------------------

/// Every transport through `proxy`, recording the expected tunnel targets.
async fn all_transports_through(proxy: &proxies::TestProxy, settings: merpati_core::settings::ProxySettings) {
    let broker = broker::start(None).await;
    let transports = [
        (broker.url(), MqttVersion::V311, broker.port),
        (broker.ws_url(), MqttVersion::V5, broker.ws_port),
        (broker.tls_url(), MqttVersion::V5, broker.tls_port),
        (broker.wss_url(), MqttVersion::V311, broker.wss_port),
    ];
    let mut want = Vec::new();
    for (url, version, port) in transports {
        let mut cfg = config(url, version);
        cfg.proxy = settings.clone();
        // Self-signed broker certificate; TLS still runs end to end.
        cfg.verify_ssl = false;
        round_trip_with(cfg, 1).await;
        want.push(port);
    }
    let log = proxy.log();
    let ports: Vec<u16> = log.iter().filter_map(|l| l.rsplit(':').next()?.parse().ok()).collect();
    assert_eq!(ports, want, "{log:?}");
    assert_eq!(broker.ws_paths(), vec!["/mqtt".to_string(); 2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_transport_through_http_proxy() {
    let auth = Some(("alice", "s3cret"));
    let proxy = proxies::http_proxy(auth).await;
    all_transports_through(&proxy, proxies::custom(&proxy.url(), auth)).await;
    assert!(proxy.log().iter().all(|l| l.starts_with("CONNECT 127.0.0.1:")), "{:?}", proxy.log());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_transport_through_socks5() {
    let auth = Some(("bob", "pa55"));
    let proxy = proxies::socks5_proxy(auth).await;
    all_transports_through(&proxy, proxies::custom(&proxy.url(), auth)).await;
    assert!(proxy.log().iter().all(|l| l.starts_with("SOCKS5 127.0.0.1:")), "{:?}", proxy.log());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_failures_are_explained() {
    let broker = broker::start(None).await;
    let auth = Some(("alice", "s3cret"));
    let http = proxies::http_proxy(auth).await;
    let socks = proxies::socks5_proxy(auth).await;
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cases = [
        (proxies::custom(&http.url(), Some(("alice", "hunter2"))), "rejected the proxy credentials"),
        (proxies::custom(&http.url(), None), "requires authentication"),
        (proxies::custom(&socks.url(), Some(("alice", "hunter2"))), "rejected the proxy credentials"),
        (proxies::custom(&format!("http://127.0.0.1:{dead}"), None), "Could not connect to the proxy"),
        (proxies::custom("ftp://proxy", None), "Unsupported proxy scheme"),
    ];
    for (settings, want) in cases {
        for (url, version) in [(broker.url(), MqttVersion::V311), (broker.wss_url(), MqttVersion::V5)] {
            let mut cfg = config(url, version);
            cfg.proxy = settings.clone();
            let (_s, mut ev) = start(cfg);
            assert_eq!(ev.disconnected().await, "connect failed");
            let errors = ev.errors();
            assert!(errors.len() == 1 && errors[0].contains(want), "{want}: {errors:?}");
            assert!(!errors[0].contains("hunter2"), "{errors:?}");
        }
    }
    assert!(http.accepted().is_empty() && socks.accepted().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bypassed_broker_is_reached_directly() {
    let broker = broker::start(None).await;
    let proxy = proxies::http_proxy(None).await;
    let mut cfg = config(broker.url(), MqttVersion::V311);
    cfg.proxy = proxies::custom(&proxy.url(), None);
    cfg.proxy.bypass = "localhost, 127.0.0.1".into();
    round_trip_with(cfg, 0).await;
    assert!(proxy.log().is_empty(), "{:?}", proxy.log());
}
