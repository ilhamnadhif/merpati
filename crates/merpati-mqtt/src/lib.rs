//! MQTT client sessions (PRD FR-X5): connect to a broker with MQTT 3.1.1 or
//! MQTT 5 over TCP, TLS, WebSocket or secure WebSocket, subscribe with a QoS,
//! publish, and a timestamped event stream for the UI message log.
//!
//! The session mirrors `merpati_http::ws::WsSession`: [`MqttSession::connect`]
//! starts the connection on a tokio runtime, every state change and message
//! is reported through a callback, and the returned handle only queues
//! commands. Dropping the handle disconnects.
//!
//! Built on `rumqttc` (rustls with the ring provider installed by
//! `merpati-http`; no aws-lc, OpenSSL or native-tls). The session never
//! reconnects by itself: a lost connection ends it with
//! [`MqttEvent::Disconnected`], and the UI decides whether to connect again.
//!
//! Proxies ([`MqttConfig::proxy`]) work for every transport: rumqttc is
//! pointed at a loopback bridge (see `bridge`) that tunnels through the
//! HTTP, HTTPS, SOCKS5 or SOCKS4 proxy chosen like for HTTP requests.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use merpati_core::auth::AuthType;
use merpati_core::protocols::MqttRequest;
pub use merpati_core::protocols::MqttVersion;
use merpati_core::settings::ProxySettings;
use parking_lot::Mutex;
use rumqttc::{TlsConfiguration, Transport};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

mod bridge;

/// Largest packet accepted or sent (MQTT allows up to 256 MiB).
const MAX_PACKET_SIZE: usize = 64 * 1024 * 1024;
/// Capacity of rumqttc's request channel between the session and its event loop.
const REQUEST_CAPACITY: usize = 1024;
/// How long a clean disconnect may take before the connection is dropped.
const DISCONNECT_GRACE: Duration = Duration::from_secs(3);

/// A message to publish (also used for the last will).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Publish {
    pub topic: String,
    pub payload: Vec<u8>,
    /// 0, 1 or 2; higher values are treated as 2.
    pub qos: u8,
    pub retain: bool,
}

/// MQTT 5 properties of a PUBLISH (see [`MqttSession::publish_with_properties`]
/// and [`MqttSession::connect_with_properties`]). MQTT 3.1.1 has none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageProperties {
    /// User properties, in order; a name may appear several times.
    pub user_properties: Vec<(String, String)>,
}

impl MessageProperties {
    pub fn is_empty(&self) -> bool {
        self.user_properties.is_empty()
    }

    /// Checks what brokers would refuse: MQTT strings must not contain U+0000
    /// and are at most 65,535 bytes long.
    fn validate(&self) -> Result<(), String> {
        for (name, value) in &self.user_properties {
            if name.contains('\0') || value.contains('\0') {
                return Err(format!("user property \"{}\" contains a NUL character (U+0000)", name.escape_debug()));
            }
            if name.len() > usize::from(u16::MAX) || value.len() > usize::from(u16::MAX) {
                let shown: String = name.chars().take(40).collect();
                return Err(format!("user property \"{shown}\" is longer than 65,535 bytes"));
            }
        }
        Ok(())
    }

    /// Bytes of the property block (without its length prefix).
    fn encoded_len(&self) -> usize {
        self.user_properties.iter().map(|(k, v)| 1 + 2 + k.len() + 2 + v.len()).sum()
    }
}

impl Publish {
    pub fn new(topic: impl Into<String>, payload: impl Into<Vec<u8>>) -> Publish {
        Publish { topic: topic.into(), payload: payload.into(), qos: 0, retain: false }
    }

    pub fn with_qos(mut self, qos: u8) -> Publish {
        self.qos = qos;
        self
    }

    pub fn with_retain(mut self, retain: bool) -> Publish {
        self.retain = retain;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MqttConfig {
    /// `mqtt://host:1883` (TCP), `mqtts://host:8883` (TLS), `ws://host:8080/mqtt`
    /// or `wss://host/mqtt`. A URL without scheme is treated as `mqtt://`;
    /// `tcp://`, `ssl://` and `tls://` are accepted as aliases. Default ports
    /// are 1883 / 8883 / 80 / 443. A WebSocket URL without path uses `/mqtt`.
    /// `user:pass@` in the URL is used when `username` is `None`.
    pub url: String,
    /// Empty = a generated `merpati-<random>` id.
    pub client_id: String,
    pub version: MqttVersion,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Zero disables keep-alive on MQTT 3.1.1. MQTT 5 (rumqttc) needs at least
    /// 5 s, so shorter values are raised to 5 s there.
    pub keep_alive: Duration,
    /// MQTT 3.1.1 "clean session" / MQTT 5 "clean start". On MQTT 5 a
    /// persistent session (`false`) is requested with a session expiry of
    /// 0xFFFFFFFF, the spec's equivalent of a 3.1.1 persistent session.
    pub clean_session: bool,
    pub last_will: Option<Publish>,
    pub verify_ssl: bool,
    /// Extra trusted roots and client certificate for `mqtts`/`wss`.
    pub tls: merpati_http::tls::TlsSetup,
    /// Proxy settings, applied like the HTTP engine does (see
    /// `merpati_http::proxy`; `mqtt`/`ws` use the HTTP proxy, `mqtts`/`wss`
    /// the HTTPS one). The default connects directly.
    pub proxy: ProxySettings,
    /// Covers TCP connect, TLS/WebSocket handshakes and the CONNACK.
    pub connect_timeout: Duration,
}

impl Default for MqttConfig {
    fn default() -> Self {
        MqttConfig {
            url: String::new(),
            client_id: String::new(),
            version: MqttVersion::V311,
            username: None,
            password: None,
            keep_alive: Duration::from_secs(60),
            clean_session: true,
            last_will: None,
            verify_ssl: true,
            tls: merpati_http::tls::TlsSetup::default(),
            proxy: ProxySettings::default(),
            connect_timeout: Duration::from_secs(20),
        }
    }
}

impl MqttConfig {
    /// Builds a config from a saved request. Every string goes through
    /// `resolve` (variable substitution). Username and password come from
    /// Basic auth (`username` / `password` parameters); other auth types are
    /// ignored. An empty client id is replaced by a generated one.
    pub fn from_request(req: &MqttRequest, resolve: &dyn Fn(&str) -> String, default_verify_ssl: bool) -> MqttConfig {
        let (username, password) = if req.auth.kind == AuthType::Basic {
            let user = resolve(&req.auth.get_string("username"));
            let pass = resolve(&req.auth.get_string("password"));
            (Some(user).filter(|s| !s.is_empty()), Some(pass).filter(|s| !s.is_empty()))
        } else {
            (None, None)
        };
        let client_id = resolve(&req.client_id).trim().to_string();
        let last_will = req.last_will.as_ref().and_then(|m| {
            let topic = resolve(&m.topic).trim().to_string();
            (!topic.is_empty()).then(|| Publish {
                topic,
                payload: resolve(&m.payload).into_bytes(),
                qos: m.qos.min(2),
                retain: m.retain,
            })
        });
        MqttConfig {
            url: resolve(&req.url).trim().to_string(),
            client_id: if client_id.is_empty() { generate_client_id() } else { client_id },
            version: req.version,
            username,
            password,
            keep_alive: Duration::from_secs(u64::from(req.keep_alive_secs)),
            clean_session: req.clean_session,
            last_will,
            verify_ssl: req.verify_ssl.unwrap_or(default_verify_ssl),
            tls: merpati_http::tls::TlsSetup::default(),
            proxy: ProxySettings::default(),
            connect_timeout: MqttConfig::default().connect_timeout,
        }
    }
}

/// A random client id of the form `merpati-1a2b3c4d` (16 characters, within
/// the 23 every MQTT 3.1.1 broker must accept).
pub fn generate_client_id() -> String {
    format!("merpati-{:08x}", rand::random::<u32>())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MqttEvent {
    Connecting,
    /// CONNACK accepted.
    Connected {
        session_present: bool,
    },
    /// SUBACK for a subscription; `granted_qos` is `None` when the broker
    /// rejected it (an [`MqttEvent::Error`] with the reason precedes it).
    Subscribed {
        topic: String,
        granted_qos: Option<u8>,
    },
    Unsubscribed {
        topic: String,
    },
    /// A message received from the broker, or one of ours written to the
    /// network (`Outgoing`, reported when the PUBLISH is sent).
    Message {
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        retain: bool,
        direction: Direction,
        at_ms: i64,
    },
    Error(String),
    /// The session is over (always the last event).
    Disconnected {
        reason: String,
    },
}

enum Command {
    Subscribe(String, u8),
    Unsubscribe(String),
    Publish(Publish, MessageProperties),
    Disconnect,
}

/// Handle to a running MQTT session. Dropping it disconnects.
pub struct MqttSession {
    tx: mpsc::UnboundedSender<Command>,
}

impl MqttSession {
    /// Connects in the background on `rt`; every state change and message is
    /// reported through `on_event` (called from a runtime thread). Commands
    /// issued before the connection is up are queued and sent once it is.
    pub fn connect(
        rt: &tokio::runtime::Handle,
        cfg: MqttConfig,
        on_event: impl Fn(MqttEvent) + Send + Sync + 'static,
    ) -> MqttSession {
        Self::connect_with_properties(rt, cfg, move |e, _| on_event(e))
    }

    /// Like [`MqttSession::connect`], also reporting MQTT 5 message
    /// properties: the second argument is `Some` for every
    /// [`MqttEvent::Message`] of an MQTT 5 session (the properties received,
    /// or the ones sent for `Outgoing`; possibly empty) and `None` otherwise.
    pub fn connect_with_properties(
        rt: &tokio::runtime::Handle,
        cfg: MqttConfig,
        on_event: impl Fn(MqttEvent, Option<MessageProperties>) + Send + Sync + 'static,
    ) -> MqttSession {
        let (tx, rx) = mpsc::unbounded_channel();
        let report: Report = Arc::new(on_event);
        rt.spawn(run(cfg, rx, report));
        MqttSession { tx }
    }

    /// Subscribes to a topic filter (wildcards `+` and `#` allowed).
    pub fn subscribe(&self, topic: &str, qos: u8) {
        let _ = self.tx.send(Command::Subscribe(topic.to_string(), qos));
    }

    pub fn unsubscribe(&self, topic: &str) {
        let _ = self.tx.send(Command::Unsubscribe(topic.to_string()));
    }

    pub fn publish(&self, p: Publish) {
        self.publish_with_properties(p, MessageProperties::default());
    }

    /// Publishes with MQTT 5 properties. On an MQTT 3.1.1 session a message
    /// with properties is not sent (reported as [`MqttEvent::Error`]).
    pub fn publish_with_properties(&self, p: Publish, props: MessageProperties) {
        let _ = self.tx.send(Command::Publish(p, props));
    }

    /// Sends DISCONNECT and closes the connection.
    pub fn disconnect(&self) {
        let _ = self.tx.send(Command::Disconnect);
    }

    /// `false` once the session has ended (after [`MqttEvent::Disconnected`]).
    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }
}

impl Drop for MqttSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Disconnect);
    }
}

type OnEvent = Arc<dyn Fn(MqttEvent) + Send + Sync>;
/// The host's callback; message events carry their MQTT 5 properties.
type Report = Arc<dyn Fn(MqttEvent, Option<MessageProperties>) + Send + Sync>;

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Requests queued to rumqttc whose packet id is not known yet. rumqttc
/// processes requests in order and reports each one as an `Outgoing` event,
/// so the queues are popped in the same order to recover topics and payloads.
#[derive(Default)]
struct Book {
    /// With their properties on MQTT 5 sessions.
    publishes: VecDeque<(Publish, Option<MessageProperties>)>,
    subscribes: VecDeque<String>,
    unsubscribes: VecDeque<String>,
    sub_ids: HashMap<u16, String>,
    unsub_ids: HashMap<u16, String>,
    /// MQTT 5: the Maximum Packet Size from the broker's CONNACK.
    broker_max_packet: Option<u32>,
}

/// Bytes of an MQTT variable byte integer.
fn varint_len(n: usize) -> usize {
    match n {
        0..=127 => 1,
        128..=16_383 => 2,
        16_384..=2_097_151 => 3,
        _ => 4,
    }
}

/// Size of the PUBLISH packet for `p` with `props` (MQTT 5 only).
fn publish_packet_size(p: &Publish, props: &MessageProperties, version: MqttVersion) -> usize {
    let properties = match version {
        MqttVersion::V5 => varint_len(props.encoded_len()) + props.encoded_len(),
        MqttVersion::V311 => 0,
    };
    let remaining = 2 + p.topic.len() + if p.qos > 0 { 2 } else { 0 } + properties + p.payload.len();
    1 + varint_len(remaining) + remaining
}

/// Publishes this large make the usual reason of a dropped connection worth
/// mentioning.
const LARGE_PUBLISH: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Session task
// ---------------------------------------------------------------------------

async fn run(cfg: MqttConfig, mut rx: mpsc::UnboundedReceiver<Command>, report: Report) {
    let on_event: OnEvent = {
        let report = report.clone();
        Arc::new(move |e| report(e, None))
    };
    on_event(MqttEvent::Connecting);
    // Kept until the session ends.
    let bridge = match start_bridge(&cfg).await {
        Ok(b) => b,
        Err(e) => {
            on_event(MqttEvent::Error(e));
            on_event(MqttEvent::Disconnected { reason: "connect failed".into() });
            return;
        }
    };
    let (client, event_loop) = match build(&cfg, bridge.as_ref().map(bridge::Bridge::rumqttc_proxy)) {
        Ok(v) => v,
        Err(e) => {
            on_event(MqttEvent::Error(e));
            on_event(MqttEvent::Disconnected { reason: "connect failed".into() });
            return;
        }
    };
    let book = Arc::new(Mutex::new(Book::default()));
    let connected = Arc::new(AtomicBool::new(false));
    let via = bridge.as_ref().map(|b| Via { proxy: b.proxy().to_string(), last_error: b.last_error() });
    // The event loop is polled in its own task: `poll()` is not cancel-safe,
    // so it must never race with the command channel in a `select!`.
    let mut poller = tokio::spawn(poll_loop(
        event_loop,
        cfg.connect_timeout,
        via,
        book.clone(),
        connected.clone(),
        on_event.clone(),
        report,
    ));
    loop {
        tokio::select! {
            r = &mut poller => {
                report_panic(r, &on_event);
                return;
            }
            cmd = rx.recv() => match cmd {
                Some(Command::Subscribe(topic, qos)) => {
                    if !rumqttc::valid_filter(&topic) {
                        on_event(MqttEvent::Error(format!("Invalid topic filter \"{topic}\"")));
                        continue;
                    }
                    book.lock().subscribes.push_back(topic.clone());
                    if client.subscribe(topic, qos.min(2)).await.is_err() {
                        book.lock().subscribes.pop_back();
                    }
                }
                Some(Command::Unsubscribe(topic)) => {
                    // Brokers disconnect clients that send an empty or malformed filter.
                    if !rumqttc::valid_filter(&topic) {
                        on_event(MqttEvent::Error(format!("Invalid topic filter \"{topic}\"")));
                        continue;
                    }
                    book.lock().unsubscribes.push_back(topic.clone());
                    if client.unsubscribe(topic).await.is_err() {
                        book.lock().unsubscribes.pop_back();
                    }
                }
                Some(Command::Publish(p, props)) => {
                    if p.topic.is_empty() || !rumqttc::valid_topic(&p.topic) {
                        on_event(MqttEvent::Error(format!(
                            "Invalid topic \"{}\": topic names must be non-empty and cannot contain + or #",
                            p.topic
                        )));
                        continue;
                    }
                    if !props.is_empty() && cfg.version != MqttVersion::V5 {
                        on_event(MqttEvent::Error(
                            "Message not sent: user properties need MQTT 5 (this session uses MQTT 3.1.1)".into(),
                        ));
                        continue;
                    }
                    if let Err(e) = props.validate() {
                        on_event(MqttEvent::Error(format!("Message not sent: {e}")));
                        continue;
                    }
                    let size = publish_packet_size(&p, &props, cfg.version);
                    if size > MAX_PACKET_SIZE {
                        on_event(MqttEvent::Error(format!("Payload too large ({} bytes)", p.payload.len())));
                        continue;
                    }
                    // Sending more than the broker accepts would end the connection.
                    let limit = book.lock().broker_max_packet;
                    if let Some(max) = limit
                        && size > max as usize
                    {
                        on_event(MqttEvent::Error(format!(
                            "Message not sent: the packet would be {size} bytes, but the broker accepts at most {max}"
                        )));
                        continue;
                    }
                    let sent = (cfg.version == MqttVersion::V5).then(|| props.clone());
                    book.lock().publishes.push_back((p.clone(), sent));
                    if client.publish(p, props).await.is_err() {
                        book.lock().publishes.pop_back();
                    }
                }
                Some(Command::Disconnect) | None => {
                    shutdown(&client, poller, connected.load(Ordering::SeqCst), &on_event).await;
                    return;
                }
            }
        }
    }
}

/// Clean shutdown: DISCONNECT when connected (the poll task reports
/// `Disconnected` once it is sent), otherwise cancel the connection attempt.
async fn shutdown(client: &Client, mut poller: JoinHandle<()>, connected: bool, on_event: &OnEvent) {
    if connected {
        let sent = tokio::time::timeout(DISCONNECT_GRACE, async {
            if client.disconnect().await.is_ok() { Some((&mut poller).await) } else { None }
        })
        .await;
        if let Ok(Some(r)) = sent {
            report_panic(r, on_event);
            return;
        }
    }
    poller.abort();
    match poller.await {
        Err(e) if e.is_cancelled() => on_event(MqttEvent::Disconnected {
            reason: if connected { "client disconnected".into() } else { "cancelled".into() },
        }),
        r => report_panic(r, on_event),
    }
}

fn report_panic(r: Result<(), tokio::task::JoinError>, on_event: &OnEvent) {
    if let Err(e) = r
        && e.is_panic()
    {
        on_event(MqttEvent::Error("Internal error: the MQTT event loop panicked".into()));
        on_event(MqttEvent::Disconnected { reason: "internal error".into() });
    }
}

/// The proxy a session goes through (via the bridge).
struct Via {
    /// `scheme://host:port`, without credentials.
    proxy: String,
    last_error: Arc<Mutex<Option<String>>>,
}

impl Via {
    /// The proxy's own explanation of a failed connection, when it has one.
    fn explain(via: Option<&Via>, fallback: String) -> String {
        match via {
            Some(v) => v.last_error.lock().clone().unwrap_or(fallback),
            None => fallback,
        }
    }
}

/// Starts the proxy bridge when the proxy settings select a proxy for the
/// broker (see the `bridge` module).
async fn start_bridge(cfg: &MqttConfig) -> Result<Option<bridge::Bridge>, String> {
    let target = parse_target(cfg)?;
    let via =
        merpati_http::proxy::proxy_for(&cfg.proxy, &target.host, target.port, target.secure).map_err(|e| e.message)?;
    let Some(proxy) = via else { return Ok(None) };
    bridge::Bridge::start(proxy, target.host, target.port, cfg.tls.clone(), cfg.verify_ssl)
        .await
        .map(Some)
        .map_err(|e| format!("Could not start the local proxy bridge: {e}"))
}

async fn poll_loop(
    mut event_loop: EventLoop,
    connect_timeout: Duration,
    via: Option<Via>,
    book: Arc<Mutex<Book>>,
    connected: Arc<AtomicBool>,
    on_event: OnEvent,
    report: Report,
) {
    // Connection phase: the first poll connects and returns the CONNACK.
    match tokio::time::timeout(connect_timeout, event_loop.poll()).await {
        Err(_) => {
            let mut msg = format!("Connection timed out after {} s", connect_timeout.as_secs_f64().round() as u64);
            if let Some(v) = &via {
                msg.push_str(&format!(" (through the proxy {})", v.proxy));
            }
            on_event(MqttEvent::Error(Via::explain(via.as_ref(), msg)));
            on_event(MqttEvent::Disconnected { reason: "connect failed".into() });
            return;
        }
        Ok(Err(Failure::Error(e))) => {
            on_event(MqttEvent::Error(Via::explain(via.as_ref(), e)));
            on_event(MqttEvent::Disconnected { reason: "connect failed".into() });
            return;
        }
        Ok(Err(Failure::ServerDisconnect(reason))) => {
            on_event(MqttEvent::Disconnected { reason });
            return;
        }
        Ok(Ok(Note::ConnAck { session_present, max_packet_size })) => {
            book.lock().broker_max_packet = max_packet_size;
            connected.store(true, Ordering::SeqCst);
            on_event(MqttEvent::Connected { session_present });
        }
        Ok(Ok(other)) => {
            // Not expected before CONNACK; treat as connected.
            connected.store(true, Ordering::SeqCst);
            on_event(MqttEvent::Connected { session_present: false });
            if handle_note(other, &book, &on_event, &report) {
                return;
            }
        }
    }
    loop {
        match event_loop.poll().await {
            Ok(note) => {
                if handle_note(note, &book, &on_event, &report) {
                    return;
                }
            }
            Err(Failure::ServerDisconnect(reason)) => {
                on_event(MqttEvent::Disconnected { reason });
                return;
            }
            Err(Failure::Error(e)) => {
                on_event(MqttEvent::Error(explain_lost_connection(e, &book)));
                on_event(MqttEvent::Disconnected { reason: "connection lost".into() });
                return;
            }
        }
    }
}

/// Rewords a write/read failure caused by the broker closing the socket, and
/// points at the usual cause when a large message was being sent (MQTT
/// 3.1.1 brokers cannot announce their packet size limit).
fn explain_lost_connection(e: String, book: &Mutex<Book>) -> String {
    let lower = e.to_ascii_lowercase();
    let kind = if lower.contains("broken pipe") {
        "broken pipe"
    } else if lower.contains("connection reset") {
        "connection reset"
    } else {
        return e;
    };
    let mut msg = format!("The broker closed the connection ({kind})");
    let large = book.lock().publishes.front().map(|(p, _)| p.payload.len()).filter(|n| *n >= LARGE_PUBLISH);
    if let Some(n) = large {
        msg.push_str(&format!(
            " while a {n}-byte message was being sent; it may exceed the broker's maximum packet size"
        ));
    }
    msg
}

/// Translates one event-loop notification; returns `true` when the session
/// ends. Messages go to `report` with their properties.
fn handle_note(note: Note, book: &Mutex<Book>, on_event: &OnEvent, report: &Report) -> bool {
    match note {
        Note::ConnAck { .. } | Note::Other => {}
        Note::Publish { topic, payload, qos, retain, properties } => report(
            MqttEvent::Message { topic, payload, qos, retain, direction: Direction::Incoming, at_ms: now_ms() },
            properties,
        ),
        Note::SentPublish => {
            let sent = book.lock().publishes.pop_front();
            if let Some((p, properties)) = sent {
                report(
                    MqttEvent::Message {
                        topic: p.topic,
                        payload: p.payload,
                        qos: p.qos.min(2),
                        retain: p.retain,
                        direction: Direction::Outgoing,
                        at_ms: now_ms(),
                    },
                    properties,
                );
            }
        }
        Note::SentSubscribe(pkid) => {
            let mut b = book.lock();
            if let Some(topic) = b.subscribes.pop_front() {
                b.sub_ids.insert(pkid, topic);
            }
        }
        Note::SentUnsubscribe(pkid) => {
            let mut b = book.lock();
            if let Some(topic) = b.unsubscribes.pop_front() {
                b.unsub_ids.insert(pkid, topic);
            }
        }
        Note::SubAck { pkid, granted } => {
            let topic = book.lock().sub_ids.remove(&pkid).unwrap_or_default();
            match granted {
                Ok(qos) => on_event(MqttEvent::Subscribed { topic, granted_qos: Some(qos) }),
                Err(reason) => {
                    on_event(MqttEvent::Error(format!("Subscription to \"{topic}\" rejected: {reason}")));
                    on_event(MqttEvent::Subscribed { topic, granted_qos: None });
                }
            }
        }
        Note::UnsubAck { pkid, error } => {
            let topic = book.lock().unsub_ids.remove(&pkid).unwrap_or_default();
            if let Some(reason) = error {
                on_event(MqttEvent::Error(format!("Unsubscribe from \"{topic}\" failed: {reason}")));
            }
            on_event(MqttEvent::Unsubscribed { topic });
        }
        Note::PubAckFailed(reason) => on_event(MqttEvent::Error(format!("Publish rejected by the broker: {reason}"))),
        Note::SentDisconnect => {
            on_event(MqttEvent::Disconnected { reason: "client disconnected".into() });
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Version-independent wrapper around rumqttc's v4 (3.1.1) and v5 clients
// ---------------------------------------------------------------------------

enum Client {
    V4(rumqttc::AsyncClient),
    V5(rumqttc::v5::AsyncClient),
}

enum EventLoop {
    V4(Box<rumqttc::EventLoop>),
    V5(Box<rumqttc::v5::EventLoop>),
}

/// The event-loop notifications the session cares about.
enum Note {
    ConnAck {
        session_present: bool,
        max_packet_size: Option<u32>,
    },
    /// `properties` is `Some` on MQTT 5.
    Publish {
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        retain: bool,
        properties: Option<MessageProperties>,
    },
    SubAck {
        pkid: u16,
        granted: Result<u8, String>,
    },
    UnsubAck {
        pkid: u16,
        error: Option<String>,
    },
    PubAckFailed(String),
    SentPublish,
    SentSubscribe(u16),
    SentUnsubscribe(u16),
    SentDisconnect,
    Other,
}

enum Failure {
    Error(String),
    /// The broker ended the session (MQTT 5 DISCONNECT); the reason text.
    ServerDisconnect(String),
}

impl Client {
    async fn subscribe(&self, topic: String, qos: u8) -> Result<(), ()> {
        match self {
            Client::V4(c) => c.subscribe(topic, qos_v4(qos)).await.map_err(drop),
            Client::V5(c) => c.subscribe(topic, qos_v5(qos)).await.map_err(drop),
        }
    }

    async fn unsubscribe(&self, topic: String) -> Result<(), ()> {
        match self {
            Client::V4(c) => c.unsubscribe(topic).await.map_err(drop),
            Client::V5(c) => c.unsubscribe(topic).await.map_err(drop),
        }
    }

    /// `props` must be empty on MQTT 3.1.1 (checked by the caller).
    async fn publish(&self, p: Publish, props: MessageProperties) -> Result<(), ()> {
        match self {
            Client::V4(c) => c.publish(p.topic, qos_v4(p.qos), p.retain, p.payload).await.map_err(drop),
            Client::V5(c) if props.is_empty() => {
                c.publish(p.topic, qos_v5(p.qos), p.retain, p.payload).await.map_err(drop)
            }
            Client::V5(c) => {
                let properties = rumqttc::v5::mqttbytes::v5::PublishProperties {
                    user_properties: props.user_properties,
                    ..Default::default()
                };
                c.publish_with_properties(p.topic, qos_v5(p.qos), p.retain, p.payload, properties).await.map_err(drop)
            }
        }
    }

    async fn disconnect(&self) -> Result<(), ()> {
        match self {
            Client::V4(c) => c.disconnect().await.map_err(drop),
            Client::V5(c) => c.disconnect().await.map_err(drop),
        }
    }
}

impl EventLoop {
    async fn poll(&mut self) -> Result<Note, Failure> {
        match self {
            EventLoop::V4(el) => el.poll().await.map(note_v4).map_err(|e| failure_v4(&e)),
            EventLoop::V5(el) => el.poll().await.map(note_v5).map_err(|e| failure_v5(&e)),
        }
    }
}

fn qos_v4(qos: u8) -> rumqttc::QoS {
    match qos {
        0 => rumqttc::QoS::AtMostOnce,
        1 => rumqttc::QoS::AtLeastOnce,
        _ => rumqttc::QoS::ExactlyOnce,
    }
}

fn qos_v5(qos: u8) -> rumqttc::v5::mqttbytes::QoS {
    use rumqttc::v5::mqttbytes::QoS;
    match qos {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        _ => QoS::ExactlyOnce,
    }
}

fn note_v4(ev: rumqttc::Event) -> Note {
    use rumqttc::{Event, Outgoing, Packet, SubscribeReasonCode};
    match ev {
        Event::Incoming(Packet::ConnAck(c)) => {
            Note::ConnAck { session_present: c.session_present, max_packet_size: None }
        }
        Event::Incoming(Packet::Publish(p)) => Note::Publish {
            topic: p.topic,
            payload: p.payload.to_vec(),
            qos: p.qos as u8,
            retain: p.retain,
            properties: None,
        },
        Event::Incoming(Packet::SubAck(s)) => Note::SubAck {
            pkid: s.pkid,
            granted: match s.return_codes.first() {
                Some(SubscribeReasonCode::Success(q)) => Ok(*q as u8),
                Some(SubscribeReasonCode::Failure) => Err("failure".into()),
                None => Err("empty SUBACK".into()),
            },
        },
        Event::Incoming(Packet::UnsubAck(u)) => Note::UnsubAck { pkid: u.pkid, error: None },
        Event::Outgoing(Outgoing::Publish(_)) => Note::SentPublish,
        Event::Outgoing(Outgoing::Subscribe(pkid)) => Note::SentSubscribe(pkid),
        Event::Outgoing(Outgoing::Unsubscribe(pkid)) => Note::SentUnsubscribe(pkid),
        Event::Outgoing(Outgoing::Disconnect) => Note::SentDisconnect,
        _ => Note::Other,
    }
}

fn note_v5(ev: rumqttc::v5::Event) -> Note {
    use rumqttc::Outgoing;
    use rumqttc::v5::Event;
    use rumqttc::v5::mqttbytes::v5::{Packet, PubAckReason, SubscribeReasonCode, UnsubAckReason};
    match ev {
        Event::Incoming(Packet::ConnAck(c)) => Note::ConnAck {
            session_present: c.session_present,
            max_packet_size: c.properties.as_ref().and_then(|p| p.max_packet_size),
        },
        Event::Incoming(Packet::Publish(p)) => Note::Publish {
            topic: String::from_utf8_lossy(&p.topic).into_owned(),
            payload: p.payload.to_vec(),
            qos: p.qos as u8,
            retain: p.retain,
            properties: Some(MessageProperties {
                user_properties: p.properties.map(|props| props.user_properties).unwrap_or_default(),
            }),
        },
        Event::Incoming(Packet::SubAck(s)) => {
            let reason = s.properties.as_ref().and_then(|p| p.reason_string.clone());
            Note::SubAck {
                pkid: s.pkid,
                granted: match s.return_codes.first() {
                    Some(SubscribeReasonCode::Success(q)) => Ok(*q as u8),
                    Some(code) => Err(with_reason(words(&format!("{code:?}")), reason)),
                    None => Err("empty SUBACK".into()),
                },
            }
        }
        Event::Incoming(Packet::UnsubAck(u)) => {
            let failed = u
                .reasons
                .iter()
                .find(|r| !matches!(r, UnsubAckReason::Success | UnsubAckReason::NoSubscriptionExisted));
            let reason = u.properties.as_ref().and_then(|p| p.reason_string.clone());
            Note::UnsubAck { pkid: u.pkid, error: failed.map(|r| with_reason(words(&format!("{r:?}")), reason)) }
        }
        Event::Incoming(Packet::PubAck(a))
            if !matches!(a.reason, PubAckReason::Success | PubAckReason::NoMatchingSubscribers) =>
        {
            let reason = a.properties.as_ref().and_then(|p| p.reason_string.clone());
            Note::PubAckFailed(with_reason(words(&format!("{:?}", a.reason)), reason))
        }
        Event::Outgoing(Outgoing::Publish(_)) => Note::SentPublish,
        Event::Outgoing(Outgoing::Subscribe(pkid)) => Note::SentSubscribe(pkid),
        Event::Outgoing(Outgoing::Unsubscribe(pkid)) => Note::SentUnsubscribe(pkid),
        Event::Outgoing(Outgoing::Disconnect) => Note::SentDisconnect,
        _ => Note::Other,
    }
}

fn failure_v4(e: &rumqttc::ConnectionError) -> Failure {
    use rumqttc::{ConnectionError as E, StateError};
    Failure::Error(match e {
        E::ConnectionRefused(code) => format!("Broker refused the connection: {}", words(&format!("{code:?}"))),
        E::Io(io) | E::MqttState(StateError::Io(io)) => describe_io(io),
        E::NetworkTimeout => "Connection timed out".into(),
        E::FlushTimeout => "Timed out writing to the broker".into(),
        E::MqttState(StateError::ConnectionAborted) => "Connection closed by the broker".into(),
        E::MqttState(StateError::AwaitPingResp) => "The broker stopped answering pings".into(),
        E::Tls(t) => describe_tls(t),
        other => other.to_string(),
    })
}

fn failure_v5(e: &rumqttc::v5::ConnectionError) -> Failure {
    use rumqttc::v5::{ConnectionError as E, StateError};
    match e {
        E::MqttState(StateError::ServerDisconnect { reason_code, reason_string }) => Failure::ServerDisconnect(
            with_reason(format!("server disconnected: {}", words(&format!("{reason_code:?}"))), reason_string.clone()),
        ),
        other => Failure::Error(match other {
            E::ConnectionRefused(code) | E::MqttState(StateError::ConnFail { reason: code }) => {
                format!("Broker refused the connection: {}", words(&format!("{code:?}")))
            }
            E::Io(io) | E::MqttState(StateError::Io(io)) => describe_io(io),
            E::Timeout(_) => "Connection timed out".into(),
            E::MqttState(StateError::ConnectionAborted) => "Connection closed by the broker".into(),
            E::MqttState(StateError::AwaitPingResp) => "The broker stopped answering pings".into(),
            E::Tls(t) => describe_tls(t),
            other => other.to_string(),
        }),
    }
}

fn describe_io(io: &std::io::Error) -> String {
    match io.kind() {
        std::io::ErrorKind::ConnectionRefused => {
            "Connection refused: is the broker running and the port correct?".into()
        }
        _ => io.to_string(),
    }
}

fn describe_tls(t: &rumqttc::TlsError) -> String {
    match t {
        rumqttc::TlsError::Io(io) => format!("TLS error: {io}"),
        rumqttc::TlsError::TLS(e) => format!("TLS error: {e}"),
        other => format!("TLS error: {other}"),
    }
}

fn with_reason(text: String, reason: Option<String>) -> String {
    match reason.filter(|r| !r.is_empty()) {
        Some(r) => format!("{text} ({r})"),
        None => text,
    }
}

/// `BadUserNamePassword` -> `bad user name password`.
fn words(debug: &str) -> String {
    let mut out = String::with_capacity(debug.len() + 8);
    for (i, c) in debug.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push(' ');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Option building
// ---------------------------------------------------------------------------

struct Target {
    /// Host for TCP/TLS, the full URL for WebSocket transports.
    addr: String,
    /// The broker host (without IPv6 brackets) and whether TLS is used.
    host: String,
    secure: bool,
    port: u16,
    transport: Transport,
    /// `user:pass@` from the URL.
    url_credentials: Option<(String, String)>,
}

fn parse_target(cfg: &MqttConfig) -> Result<Target, String> {
    let raw = cfg.url.trim();
    if raw.is_empty() {
        return Err("Broker URL is empty".into());
    }
    let with_scheme = if raw.contains("://") { raw.to_string() } else { format!("mqtt://{raw}") };
    let mut url = url::Url::parse(&with_scheme).map_err(|e| format!("Invalid broker URL \"{raw}\": {e}"))?;
    let scheme = url.scheme().to_ascii_lowercase();
    let host = match url.host_str() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => return Err(format!("Invalid broker URL \"{raw}\": missing host")),
    };
    let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
    let url_credentials =
        (!url.username().is_empty()).then(|| (decode(url.username()), url.password().map(decode).unwrap_or_default()));
    let tls = || -> Result<TlsConfiguration, String> {
        cfg.tls
            .client_config(cfg.verify_ssl)
            .map(|c| TlsConfiguration::Rustls(Arc::new(c)))
            .map_err(|e| format!("TLS setup failed: {e}"))
    };
    let broker = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let secure = matches!(scheme.as_str(), "mqtts" | "ssl" | "tls" | "wss");
    let (addr, port, transport) = match scheme.as_str() {
        "mqtt" | "tcp" => (host, url.port().unwrap_or(1883), Transport::Tcp),
        "mqtts" | "ssl" | "tls" => (host, url.port().unwrap_or(8883), Transport::Tls(tls()?)),
        "ws" | "wss" => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            if url.path().is_empty() || url.path() == "/" {
                url.set_path("/mqtt");
            }
            let port = url.port_or_known_default().unwrap_or(if scheme == "ws" { 80 } else { 443 });
            let transport = if scheme == "ws" { Transport::Ws } else { Transport::Wss(tls()?) };
            (url.to_string(), port, transport)
        }
        other => {
            return Err(format!(
                "Unsupported broker URL scheme \"{other}://\" (use mqtt://, mqtts://, ws:// or wss://)"
            ));
        }
    };
    Ok(Target { addr, host: broker, secure, port, transport, url_credentials })
}

/// rumqttc options for the session; `proxy` routes the connection through
/// the proxy bridge.
fn build(cfg: &MqttConfig, proxy: Option<rumqttc::Proxy>) -> Result<(Client, EventLoop), String> {
    let target = parse_target(cfg)?;
    let client_id = match cfg.client_id.trim() {
        "" => generate_client_id(),
        id => id.to_string(),
    };
    let (username, password) = match (&cfg.username, &target.url_credentials) {
        (None, Some((u, p))) => (Some(u.clone()), cfg.password.clone().or_else(|| Some(p.clone()))),
        _ => (cfg.username.clone(), cfg.password.clone()),
    };
    let username = username.filter(|u| !u.is_empty());
    let password = password.filter(|p| !p.is_empty());
    // A will topic with wildcards (or none) makes brokers refuse the CONNECT.
    if let Some(w) = &cfg.last_will
        && (w.topic.is_empty() || !rumqttc::valid_topic(&w.topic))
    {
        return Err(format!(
            "Invalid last will topic \"{}\": topic names must be non-empty and cannot contain + or #",
            w.topic
        ));
    }
    let timeout_secs = cfg.connect_timeout.as_secs().max(1);
    match cfg.version {
        MqttVersion::V311 => {
            if username.is_none() && password.is_some() {
                return Err("MQTT 3.1.1 requires a username when a password is set".into());
            }
            let mut o = rumqttc::MqttOptions::new(client_id, target.addr, target.port);
            o.set_transport(target.transport);
            if let Some(p) = proxy {
                o.set_proxy(p);
            }
            let keep_alive = cfg.keep_alive.as_secs().min(u64::from(u16::MAX));
            o.set_keep_alive(Duration::from_secs(keep_alive));
            o.set_clean_session(cfg.clean_session);
            o.set_max_packet_size(MAX_PACKET_SIZE, MAX_PACKET_SIZE);
            if let Some(u) = username {
                o.set_credentials(u, password.unwrap_or_default());
            }
            if let Some(w) = &cfg.last_will {
                o.set_last_will(rumqttc::LastWill::new(&w.topic, w.payload.clone(), qos_v4(w.qos), w.retain));
            }
            let (client, mut el) = rumqttc::AsyncClient::new(o, REQUEST_CAPACITY);
            let mut net = el.network_options();
            net.set_connection_timeout(timeout_secs);
            el.set_network_options(net);
            Ok((Client::V4(client), EventLoop::V4(Box::new(el))))
        }
        MqttVersion::V5 => {
            let mut o = rumqttc::v5::MqttOptions::new(client_id, target.addr, target.port);
            o.set_transport(target.transport);
            if let Some(p) = proxy {
                o.set_proxy(p);
            }
            let keep_alive = cfg.keep_alive.as_secs().clamp(5, u64::from(u16::MAX));
            o.set_keep_alive(Duration::from_secs(keep_alive));
            o.set_clean_start(cfg.clean_session);
            if !cfg.clean_session {
                o.set_session_expiry_interval(Some(u32::MAX));
            }
            o.set_max_packet_size(Some(MAX_PACKET_SIZE as u32));
            o.set_connection_timeout(timeout_secs);
            if username.is_some() || password.is_some() {
                o.set_credentials(username.unwrap_or_default(), password.unwrap_or_default());
            }
            if let Some(w) = &cfg.last_will {
                o.set_last_will(rumqttc::v5::mqttbytes::v5::LastWill::new(
                    &w.topic,
                    w.payload.clone(),
                    qos_v5(w.qos),
                    w.retain,
                    None,
                ));
            }
            let (client, el) = rumqttc::v5::AsyncClient::new(o, REQUEST_CAPACITY);
            Ok((Client::V5(client), EventLoop::V5(Box::new(el))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::Auth;
    use merpati_core::protocols::MqttMessage;

    #[test]
    fn from_request_maps_fields() {
        let req = MqttRequest {
            url: "{{host}}".into(),
            client_id: String::new(),
            version: MqttVersion::V5,
            keep_alive_secs: 30,
            clean_session: false,
            last_will: Some(MqttMessage {
                topic: "lw/{{id}}".into(),
                payload: "bye".into(),
                qos: 1,
                ..Default::default()
            }),
            auth: Auth::basic("{{user}}", "secret"),
            ..Default::default()
        };
        let resolve =
            |s: &str| s.replace("{{host}}", "mqtt://broker:1884").replace("{{user}}", "bob").replace("{{id}}", "7");
        let cfg = MqttConfig::from_request(&req, &resolve, false);
        assert_eq!(cfg.url, "mqtt://broker:1884");
        assert!(cfg.client_id.starts_with("merpati-") && cfg.client_id.len() == 16, "{}", cfg.client_id);
        assert_eq!(cfg.version, MqttVersion::V5);
        assert_eq!(cfg.username.as_deref(), Some("bob"));
        assert_eq!(cfg.password.as_deref(), Some("secret"));
        assert_eq!(cfg.keep_alive, Duration::from_secs(30));
        assert!(!cfg.clean_session);
        assert!(!cfg.verify_ssl);
        assert_eq!(
            cfg.last_will,
            Some(Publish { topic: "lw/7".into(), payload: b"bye".to_vec(), qos: 1, retain: false })
        );
    }

    #[test]
    fn from_request_defaults() {
        let req = MqttRequest { client_id: "fixed".into(), verify_ssl: Some(false), ..Default::default() };
        let cfg = MqttConfig::from_request(&req, &|s| s.to_string(), true);
        assert_eq!(cfg.client_id, "fixed");
        assert_eq!(cfg.username, None);
        assert_eq!(cfg.password, None);
        assert!(!cfg.verify_ssl);
        assert!(cfg.clean_session);
        assert_eq!(cfg.last_will, None);
        // Non-basic auth is ignored.
        let req = MqttRequest { auth: Auth::bearer("t"), ..Default::default() };
        assert_eq!(MqttConfig::from_request(&req, &|s| s.to_string(), true).username, None);
    }

    #[test]
    fn targets() {
        let t = |url: &str| parse_target(&MqttConfig { url: url.into(), ..Default::default() });
        let x = t("localhost").unwrap();
        assert_eq!((x.addr.as_str(), x.port), ("localhost", 1883));
        assert!(matches!(x.transport, Transport::Tcp));
        let x = t("mqtts://u%40x:p@example.com").unwrap();
        assert_eq!((x.addr.as_str(), x.port), ("example.com", 8883));
        assert!(matches!(x.transport, Transport::Tls(_)));
        assert_eq!(x.url_credentials, Some(("u@x".into(), "p".into())));
        let x = t("ws://example.com:8080").unwrap();
        assert_eq!((x.addr.as_str(), x.port), ("ws://example.com:8080/mqtt", 8080));
        let x = t("wss://u:p@example.com/custom?x=1").unwrap();
        assert_eq!((x.addr.as_str(), x.port), ("wss://example.com/custom?x=1", 443));
        assert!(t("http://example.com").err().unwrap().contains("Unsupported"));
        assert!(t("").is_err());
    }

    #[test]
    fn publish_sizes_and_lost_connections() {
        let none = MessageProperties::default();
        let p = Publish::new("a/b", vec![0u8; 10]);
        assert_eq!(publish_packet_size(&p, &none, MqttVersion::V311), 1 + 1 + 2 + 3 + 10);
        assert_eq!(publish_packet_size(&p.clone().with_qos(1), &none, MqttVersion::V5), 1 + 1 + 2 + 3 + 2 + 1 + 10);
        // Each user property: identifier, then name and value with 2-byte lengths.
        let props = MessageProperties { user_properties: vec![("k".into(), "vv".into()), ("k".into(), String::new())] };
        assert_eq!(
            publish_packet_size(&p, &props, MqttVersion::V5),
            1 + 1 + 2 + 3 + 1 + (1 + 2 + 1 + 2 + 2) + (1 + 2 + 1 + 2) + 10
        );
        let many = MessageProperties { user_properties: vec![("n".into(), "x".repeat(200))] };
        assert_eq!(publish_packet_size(&p, &many, MqttVersion::V5), 1 + 2 + 2 + 3 + 2 + 206 + 10);
        let big = Publish::new("t", vec![0u8; 3 * 1024 * 1024]);
        assert_eq!(publish_packet_size(&big, &none, MqttVersion::V311), 1 + 4 + 2 + 1 + 3 * 1024 * 1024);
        assert!(props.validate().is_ok());
        let nul = MessageProperties { user_properties: vec![("a\0".into(), "v".into())] };
        assert!(nul.validate().unwrap_err().contains("NUL"));
        let long = MessageProperties { user_properties: vec![("a".into(), "v".repeat(70_000))] };
        assert!(long.validate().unwrap_err().contains("65,535"));
        let book = Mutex::new(Book::default());
        let pipe = "Mqtt state: Mqtt serialization/deserialization error: IO: Broken pipe (os error 32)".to_string();
        assert_eq!(explain_lost_connection(pipe.clone(), &book), "The broker closed the connection (broken pipe)");
        book.lock().publishes.push_back((big, None));
        assert!(explain_lost_connection(pipe, &book).contains("maximum packet size"));
        assert_eq!(explain_lost_connection("other".into(), &book), "other");
    }

    #[test]
    fn words_splits_camel_case() {
        assert_eq!(words("BadUserNamePassword"), "bad user name password");
        assert_eq!(words("NotAuthorized"), "not authorized");
    }
}
