//! Interop with a real broker (eclipse-mosquitto 2), run on demand:
//!
//! ```text
//! crates/merpati-mqtt/tests/mosquitto/run.sh /tmp/mosquitto-work
//! MERPATI_MOSQUITTO=127.0.0.1 cargo test -p merpati-mqtt --test mosquitto -- --ignored
//! docker rm -f merpati-mosquitto
//! ```
//!
//! Covers MQTT 3.1.1 and 5 over TCP, WebSocket, TLS and secure WebSocket:
//! QoS 0/1/2 (with downgrades), wildcards, retained messages, unsubscribe,
//! last wills on an abrupt connection loss, persistent sessions, large
//! payloads, and v5 messages carrying user properties.

use std::time::Duration;

use merpati_mqtt::{Direction, MqttConfig, MqttEvent, MqttSession, MqttVersion, Publish};
use tokio::sync::{mpsc, watch};

const VERSIONS: [MqttVersion; 2] = [MqttVersion::V311, MqttVersion::V5];

/// (name, scheme, port) of the broker listeners started by run.sh.
const TRANSPORTS: [(&str, &str, u16); 4] =
    [("tcp", "mqtt", 18830), ("ws", "ws", 19001), ("tls", "mqtts", 18883), ("wss", "wss", 19443)];

fn host() -> Option<String> {
    match std::env::var("MERPATI_MOSQUITTO") {
        Ok(h) => Some(h),
        Err(_) => {
            eprintln!("skipped: start tests/mosquitto/run.sh and set MERPATI_MOSQUITTO=127.0.0.1");
            None
        }
    }
}

fn unique(prefix: &str) -> String {
    format!("merpati-test/{prefix}/{:08x}", rand::random::<u32>())
}

struct Events {
    rx: mpsc::UnboundedReceiver<MqttEvent>,
    seen: Vec<MqttEvent>,
    label: String,
}

impl Events {
    async fn wait_for(&mut self, what: &str, pred: impl Fn(&MqttEvent) -> bool) -> MqttEvent {
        match self.try_wait(Duration::from_secs(10), &pred).await {
            Some(e) => e,
            None => panic!("[{}] no {what} event; seen: {:#?}", self.label, self.seen),
        }
    }

    async fn try_wait(&mut self, within: Duration, pred: &impl Fn(&MqttEvent) -> bool) -> Option<MqttEvent> {
        tokio::time::timeout(within, async {
            loop {
                let e = self.rx.recv().await?;
                self.seen.push(e.clone());
                if pred(&e) {
                    return Some(e);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    async fn connected(&mut self) -> bool {
        match self.wait_for("connected", |e| matches!(e, MqttEvent::Connected { .. })).await {
            MqttEvent::Connected { session_present } => session_present,
            _ => unreachable!(),
        }
    }

    async fn subscribed(&mut self, topic: &str) -> Option<u8> {
        let t = topic.to_string();
        match self
            .wait_for(
                &format!("suback {topic}"),
                move |e| matches!(e, MqttEvent::Subscribed { topic, .. } if *topic == t),
            )
            .await
        {
            MqttEvent::Subscribed { granted_qos, .. } => granted_qos,
            _ => unreachable!(),
        }
    }

    /// Next incoming message on `topic`: (payload, qos, retain).
    async fn message(&mut self, topic: &str) -> (Vec<u8>, u8, bool) {
        let t = topic.to_string();
        let e = self
            .wait_for(
                &format!("message on {topic}"),
                move |e| matches!(e, MqttEvent::Message { topic, direction: Direction::Incoming, .. } if *topic == t),
            )
            .await;
        let MqttEvent::Message { payload, qos, retain, .. } = e else { unreachable!() };
        (payload, qos, retain)
    }

    async fn no_message(&mut self, topic: &str, within: Duration) {
        let t = topic.to_string();
        let got = self
            .try_wait(
                within,
                &move |e| matches!(e, MqttEvent::Message { topic, direction: Direction::Incoming, .. } if *topic == t),
            )
            .await;
        assert!(got.is_none(), "[{}] unexpected message on {topic}: {got:?}", self.label);
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
    MqttConfig { url, version, verify_ssl: false, connect_timeout: Duration::from_secs(10), ..Default::default() }
}

fn start(cfg: MqttConfig) -> (MqttSession, Events) {
    let label = format!("{} {:?}", cfg.url, cfg.version);
    let (tx, rx) = mpsc::unbounded_channel();
    let s = MqttSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| {
        let _ = tx.send(e);
    });
    (s, Events { rx, seen: Vec::new(), label })
}

async fn connect(cfg: MqttConfig) -> (MqttSession, Events) {
    let (s, mut ev) = start(cfg);
    ev.connected().await;
    (s, ev)
}

/// Every (transport name, URL, version).
fn matrix(host: &str) -> Vec<(&'static str, String, MqttVersion)> {
    let mut out = Vec::new();
    for v in VERSIONS {
        for (name, scheme, port) in TRANSPORTS {
            out.push((name, format!("{scheme}://{host}:{port}"), v));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a mosquitto broker (tests/mosquitto/run.sh)"]
async fn qos_wildcards_retain_and_unsubscribe() {
    let Some(host) = host() else { return };
    for (name, url, version) in matrix(&host) {
        eprintln!("{name} {version:?}");
        let base = unique(&format!("{name}-{version:?}"));
        let (sub, mut sev) = connect(config(url.clone(), version)).await;
        for q in 0..=2u8 {
            sub.subscribe(&format!("{base}/q{q}"), q);
            assert_eq!(sev.subscribed(&format!("{base}/q{q}")).await, Some(q), "{name} {version:?}");
        }
        sub.subscribe(&format!("{base}/w/+"), 2);
        assert_eq!(sev.subscribed(&format!("{base}/w/+")).await, Some(2));

        let (publ, mut pev) = connect(config(url.clone(), version)).await;
        for q in 0..=2u8 {
            publ.publish(Publish::new(format!("{base}/q{q}"), format!("payload {q} é")).with_qos(q));
            assert_eq!(sev.message(&format!("{base}/q{q}")).await, (format!("payload {q} é").into_bytes(), q, false));
        }
        // QoS is the minimum of the publish and the subscription.
        publ.publish(Publish::new(format!("{base}/q1"), "down").with_qos(2));
        assert_eq!(sev.message(&format!("{base}/q1")).await, (b"down".to_vec(), 1, false));
        publ.publish(Publish::new(format!("{base}/q2"), "up").with_qos(0));
        assert_eq!(sev.message(&format!("{base}/q2")).await, (b"up".to_vec(), 0, false));
        // Wildcard subscription; empty payload.
        publ.publish(Publish::new(format!("{base}/w/x"), "").with_qos(2));
        assert_eq!(sev.message(&format!("{base}/w/x")).await, (Vec::new(), 2, false));
        // Outgoing messages are reported to the publisher.
        pev.wait_for("outgoing", |e| matches!(e, MqttEvent::Message { direction: Direction::Outgoing, .. })).await;

        // Retained: a later subscriber gets it flagged, until it is cleared.
        let retained = format!("{base}/retained");
        publ.publish(Publish::new(&retained, "kept").with_qos(1).with_retain(true));
        let t = retained.clone();
        pev.wait_for(
            "retained publish",
            move |e| matches!(e, MqttEvent::Message { topic, direction: Direction::Outgoing, .. } if *topic == t),
        )
        .await;
        // (Another connection: give the broker time to store it.)
        tokio::time::sleep(Duration::from_millis(200)).await;
        sub.subscribe(&retained, 1);
        sev.subscribed(&retained).await;
        assert_eq!(sev.message(&retained).await, (b"kept".to_vec(), 1, true));
        let (late, mut lev) = connect(config(url.clone(), version)).await;
        late.subscribe(&retained, 2);
        lev.subscribed(&retained).await;
        assert_eq!(lev.message(&retained).await, (b"kept".to_vec(), 1, true));
        publ.publish(Publish::new(&retained, "").with_qos(1).with_retain(true));
        lev.message(&retained).await;
        let (later, mut l2ev) = connect(config(url.clone(), version)).await;
        later.subscribe(&retained, 1);
        l2ev.subscribed(&retained).await;
        l2ev.no_message(&retained, Duration::from_millis(400)).await;

        // Unsubscribe stops delivery.
        sub.unsubscribe(&format!("{base}/q0"));
        sev.wait_for("unsuback", |e| matches!(e, MqttEvent::Unsubscribed { .. })).await;
        publ.publish(Publish::new(format!("{base}/q0"), "gone").with_qos(1));
        sev.no_message(&format!("{base}/q0"), Duration::from_millis(400)).await;

        for (e, label) in [(&sev, "sub"), (&pev, "pub"), (&lev, "late")] {
            assert!(e.errors().is_empty(), "{name} {version:?} {label}: {:?}", e.errors());
        }
        publ.disconnect();
        pev.wait_for("disconnected", |e| matches!(e, MqttEvent::Disconnected { .. })).await;
    }
}

/// A TCP proxy whose connections can be cut without any MQTT DISCONNECT.
struct Proxy {
    port: u16,
    cut: watch::Sender<bool>,
}

async fn proxy(target: String) -> Proxy {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (cut, rx) = watch::channel(false);
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            let mut rx = rx.clone();
            let target = target.clone();
            tokio::spawn(async move {
                let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await else { return };
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                    _ = rx.wait_for(|cut| *cut) => {}
                }
            });
        }
    });
    Proxy { port, cut }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a mosquitto broker (tests/mosquitto/run.sh)"]
async fn last_will_on_connection_loss_only() {
    let Some(host) = host() else { return };
    for (name, scheme, port) in TRANSPORTS {
        for version in VERSIONS {
            eprintln!("{name} {version:?}");
            let base = unique(&format!("will-{name}-{version:?}"));
            let will = format!("{base}/will");
            let direct = format!("{scheme}://{host}:{port}");
            let (watcher, mut wev) = connect(config(direct.clone(), version)).await;
            watcher.subscribe(&will, 1);
            wev.subscribed(&will).await;

            // A clean disconnect does not publish the will.
            let mut cfg = config(direct.clone(), version);
            cfg.last_will = Some(Publish::new(&will, "clean").with_qos(1));
            let (a, mut aev) = connect(cfg).await;
            a.disconnect();
            aev.wait_for("disconnected", |e| matches!(e, MqttEvent::Disconnected { .. })).await;
            wev.no_message(&will, Duration::from_millis(500)).await;

            // Losing the connection does.
            let p = proxy(format!("{host}:{port}")).await;
            let mut cfg = config(format!("{scheme}://127.0.0.1:{}", p.port), version);
            cfg.last_will = Some(Publish::new(&will, "lost").with_qos(1));
            let (b, mut bev) = connect(cfg).await;
            p.cut.send(true).unwrap();
            assert_eq!(wev.message(&will).await, (b"lost".to_vec(), 1, false), "{name} {version:?}");
            let gone = bev.wait_for("disconnected", |e| matches!(e, MqttEvent::Disconnected { .. })).await;
            assert_eq!(gone, MqttEvent::Disconnected { reason: "connection lost".into() });
            assert!(!bev.errors().is_empty());
            drop(b);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a mosquitto broker (tests/mosquitto/run.sh)"]
async fn persistent_session_queues_qos1_messages() {
    let Some(host) = host() else { return };
    for version in VERSIONS {
        let url = format!("mqtt://{host}:18830");
        let topic = unique(&format!("persist-{version:?}"));
        let id = format!("merpati-p-{:06x}", rand::random::<u32>() & 0xFFFFFF);
        let persistent =
            |clean: bool| MqttConfig { client_id: id.clone(), clean_session: clean, ..config(url.clone(), version) };
        let (s, mut ev) = start(persistent(false));
        assert!(!ev.connected().await);
        s.subscribe(&topic, 1);
        ev.subscribed(&topic).await;
        s.disconnect();
        ev.wait_for("disconnected", |e| matches!(e, MqttEvent::Disconnected { .. })).await;

        let (p, _pev) = connect(config(url.clone(), version)).await;
        p.publish(Publish::new(&topic, "while away").with_qos(1));
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (s, mut ev) = start(persistent(false));
        assert!(ev.connected().await, "{version:?}: session present");
        assert_eq!(ev.message(&topic).await, (b"while away".to_vec(), 1, false));
        s.disconnect();
        ev.wait_for("disconnected", |e| matches!(e, MqttEvent::Disconnected { .. })).await;
        // Clean up the stored session.
        let (_s, mut ev) = start(persistent(true));
        assert!(!ev.connected().await);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a mosquitto broker (tests/mosquitto/run.sh)"]
async fn large_payloads() {
    let Some(host) = host() else { return };
    for (name, scheme, port) in TRANSPORTS {
        for version in VERSIONS {
            let url = format!("{scheme}://{host}:{port}");
            let topic = unique(&format!("large-{name}-{version:?}"));
            let (s, mut ev) = connect(config(url, version)).await;
            s.subscribe(&topic, 1);
            ev.subscribed(&topic).await;
            // mosquitto 2.1 accepts packets up to 2,000,000 bytes by default.
            let payload: Vec<u8> = (0..1_500_000).map(|i| (i % 251) as u8).collect();
            s.publish(Publish::new(&topic, payload.clone()).with_qos(1));
            let (got, qos, _) = ev.message(&topic).await;
            assert_eq!((got.len(), qos), (payload.len(), 1), "{name} {version:?}");
            assert!(got == payload, "{name} {version:?}: payload differs");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a mosquitto broker (tests/mosquitto/run.sh)"]
async fn oversize_publish_is_explained() {
    let Some(host) = host() else { return };
    let topic = unique("oversize");
    // MQTT 5: the broker announces its limit in CONNACK; the message is
    // refused locally and the session stays up.
    let (s, mut ev) = connect(config(format!("mqtt://{host}:18830"), MqttVersion::V5)).await;
    s.publish(Publish::new(&topic, vec![7u8; 3 * 1024 * 1024]).with_qos(1));
    let e = ev.wait_for("error", |e| matches!(e, MqttEvent::Error(_))).await;
    assert!(matches!(&e, MqttEvent::Error(m) if m.contains("accepts at most 2000000")), "{e:?}");
    s.subscribe(&topic, 0);
    ev.subscribed(&topic).await;
    s.publish(Publish::new(&topic, "still connected"));
    assert_eq!(ev.message(&topic).await.0, b"still connected");

    // MQTT 3.1.1: the broker drops the connection; the error says why.
    let (s, mut ev) = connect(config(format!("mqtt://{host}:18830"), MqttVersion::V311)).await;
    s.publish(Publish::new(&topic, vec![7u8; 3 * 1024 * 1024]).with_qos(1));
    let e = ev.wait_for("disconnected", |e| matches!(e, MqttEvent::Disconnected { .. })).await;
    assert_eq!(e, MqttEvent::Disconnected { reason: "connection lost".into() });
    let errors = ev.errors();
    assert!(errors.iter().any(|m| m.contains("maximum packet size")), "{errors:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a mosquitto broker (tests/mosquitto/run.sh)"]
async fn v5_messages_with_user_properties() {
    use rumqttc::v5::mqttbytes::v5::PublishProperties;
    let Some(host) = host() else { return };
    let topic = unique("props");
    let (s, mut ev) = connect(config(format!("mqtt://{host}:18830"), MqttVersion::V5)).await;
    s.subscribe(&topic, 1);
    ev.subscribed(&topic).await;
    // Another (plain rumqttc) client publishes with properties.
    let mut opts = rumqttc::v5::MqttOptions::new("merpati-props-pub", host.clone(), 18830);
    opts.set_keep_alive(Duration::from_secs(10));
    let (client, mut el) = rumqttc::v5::AsyncClient::new(opts, 10);
    let driver = tokio::spawn(async move { while el.poll().await.is_ok() {} });
    let props = PublishProperties {
        user_properties: vec![("trace".into(), "abc".into()), ("é".into(), "😀".into())],
        content_type: Some("application/json".into()),
        payload_format_indicator: Some(1),
        ..Default::default()
    };
    client
        .publish_with_properties(topic.clone(), rumqttc::v5::mqttbytes::QoS::AtLeastOnce, false, "{\"a\":1}", props)
        .await
        .unwrap();
    assert_eq!(ev.message(&topic).await, (b"{\"a\":1}".to_vec(), 1, false));
    assert!(ev.errors().is_empty(), "{:?}", ev.errors());
    driver.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a mosquitto broker (tests/mosquitto/run.sh)"]
async fn tls_certificate_verification() {
    let Some(host) = host() else { return };
    for url in [format!("mqtts://{host}:18883"), format!("wss://{host}:19443")] {
        let mut cfg = config(url.clone(), MqttVersion::V311);
        cfg.verify_ssl = true;
        let (_s, mut ev) = start(cfg);
        let e = ev.wait_for("disconnected", |e| matches!(e, MqttEvent::Disconnected { .. })).await;
        assert_eq!(e, MqttEvent::Disconnected { reason: "connect failed".into() }, "{url}");
        let errors = ev.errors();
        assert!(errors.iter().any(|m| m.to_lowercase().contains("certificate")), "{url}: {errors:?}");
        eprintln!("{url}: {errors:?}");
    }
}
