use merpati_mock::listener::{ListenerConfig, WebhookListener, replay};

fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn start(cfg: ListenerConfig) -> WebhookListener {
    WebhookListener::start(cfg).await.unwrap()
}

#[tokio::test]
async fn records_request_and_answers_default() {
    let hooks = start(ListenerConfig::default()).await;
    let before = chrono_now_ms();
    let res = client()
        .post(format!("{}/hooks/github?delivery=1&x=a%20b", hooks.url()))
        .header("X-Hub-Signature-256", "sha256=abc")
        .header("Content-Type", "application/json")
        .body("{\"action\":\"opened\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(res.text().await.unwrap(), "{\"ok\":true}");

    let events = hooks.events();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.id, 1);
    assert_eq!(ev.method, "POST");
    assert_eq!(ev.path, "/hooks/github");
    assert_eq!(ev.query, "delivery=1&x=a%20b");
    assert_eq!(ev.header("x-hub-signature-256"), Some("sha256=abc"));
    assert_eq!(ev.body, b"{\"action\":\"opened\"}");
    assert_eq!(ev.body_text(), "{\"action\":\"opened\"}");
    assert!(!ev.body_truncated);
    assert!(ev.remote_addr.as_deref().unwrap().starts_with("127.0.0.1:"));
    assert!(ev.timestamp_ms >= before - 1000 && ev.timestamp_ms <= chrono_now_ms() + 1000);
    assert_eq!(hooks.event(1).as_ref(), Some(ev));
    hooks.stop().await;
}

fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

#[tokio::test]
async fn ring_buffer_keeps_newest_and_clear_empties() {
    let hooks = start(ListenerConfig { capacity: 3, ..Default::default() }).await;
    let c = client();
    for i in 1..=5 {
        c.get(format!("{}/e/{i}", hooks.url())).send().await.unwrap();
    }
    let events = hooks.events();
    assert_eq!(events.iter().map(|e| e.id).collect::<Vec<_>>(), vec![3, 4, 5]);
    assert_eq!(events.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["/e/3", "/e/4", "/e/5"]);
    assert!(hooks.event(1).is_none());
    hooks.clear();
    assert!(hooks.events().is_empty());
    c.delete(format!("{}/after", hooks.url())).send().await.unwrap();
    let events = hooks.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, 6);
    assert_eq!(events[0].method, "DELETE");
}

#[tokio::test]
async fn body_is_capped_but_request_still_answered() {
    let hooks = start(ListenerConfig { max_body_bytes: 10, ..Default::default() }).await;
    let res = client().put(format!("{}/big", hooks.url())).body("x".repeat(100_000)).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let ev = &hooks.events()[0];
    assert_eq!(ev.body, b"xxxxxxxxxx");
    assert!(ev.body_truncated);
}

#[tokio::test]
async fn configurable_response() {
    let cfg = ListenerConfig {
        response_status: 202,
        response_headers: vec![("X-Hook".into(), "yes".into())],
        response_body: "accepted".into(),
        ..Default::default()
    };
    let hooks = start(cfg).await;
    let c = client();
    let res = c.post(format!("{}/", hooks.url())).send().await.unwrap();
    assert_eq!(res.status(), 202);
    assert_eq!(res.headers()["x-hook"], "yes");
    assert_eq!(res.text().await.unwrap(), "accepted");

    hooks.set_response(
        418,
        vec![("Content-Type".into(), "text/plain".into()), ("Content-Length".into(), "999".into())],
        "teapot",
    );
    let res = c.post(format!("{}/", hooks.url())).send().await.unwrap();
    assert_eq!(res.status(), 418);
    assert_eq!(res.headers()["content-type"], "text/plain");
    assert_eq!(res.text().await.unwrap(), "teapot");
    assert_eq!(hooks.events().len(), 2);
}

#[tokio::test]
async fn replay_forwards_method_headers_body() {
    let source = start(ListenerConfig::default()).await;
    let target = start(ListenerConfig {
        response_status: 201,
        response_headers: vec![("X-Target".into(), "t".into())],
        response_body: "from target".into(),
        ..Default::default()
    })
    .await;
    let c = client();
    c.patch(format!("{}/hooks/stripe?id=9", source.url()))
        .header("X-Signature", "v1=deadbeef")
        .header("Content-Type", "application/json")
        .header("Connection", "keep-alive, X-Drop-Me")
        .header("X-Drop-Me", "1")
        .body("{\"type\":\"charge.succeeded\"}")
        .send()
        .await
        .unwrap();
    let ev = source.events().remove(0);

    let result = replay(&ev, &format!("{}/base/", target.url()), &c).await.unwrap();
    assert_eq!(result.status, 201);
    assert_eq!(result.body, b"from target");
    assert!(result.headers.iter().any(|(k, v)| k == "x-target" && v == "t"));
    assert!(result.elapsed_ms >= 0.0);

    let got = target.events().remove(0);
    assert_eq!(got.method, "PATCH");
    assert_eq!(got.path, "/base/hooks/stripe");
    assert_eq!(got.query, "id=9");
    assert_eq!(got.body, ev.body);
    assert_eq!(got.header("x-signature"), Some("v1=deadbeef"));
    assert_eq!(got.header("content-type"), Some("application/json"));
    assert!(got.header("x-drop-me").is_none(), "headers listed in Connection are hop-by-hop");
    // Host is the target's, not the original listener's.
    assert_eq!(got.header("host"), Some(target.url().trim_start_matches("http://")));
    source.stop().await;
    target.stop().await;
}

#[tokio::test]
async fn replay_errors() {
    let hooks = start(ListenerConfig { max_body_bytes: 2, ..Default::default() }).await;
    let c = client();
    c.post(format!("{}/t", hooks.url())).body("hello").send().await.unwrap();
    let ev = hooks.events().remove(0);
    let err = replay(&ev, &hooks.url(), &c).await.unwrap_err();
    assert!(err.contains("truncated"), "{err}");

    let mut ok = ev.clone();
    ok.body_truncated = false;
    assert!(replay(&ok, "file:///etc", &c).await.unwrap_err().contains("scheme"));
    assert!(replay(&ok, "::nope", &c).await.is_err());

    // Nothing listening on the port of a stopped listener.
    let gone = start(ListenerConfig::default()).await;
    let url = gone.url();
    gone.stop().await;
    assert!(replay(&ok, &url, &c).await.unwrap_err().contains("failed"));
}

#[tokio::test]
async fn stop_closes_the_port() {
    let hooks = start(ListenerConfig::default()).await;
    let url = hooks.url();
    let c = client();
    assert!(c.get(&url).send().await.is_ok());
    hooks.stop().await;
    let c = client();
    assert!(c.get(&url).send().await.is_err());
}
