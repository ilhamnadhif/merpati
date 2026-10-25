use std::time::{Duration, Instant};

use merpati_core::model::{Collection, Example, HttpRequest, KeyValue, RequestItem};
use merpati_mock::{FailureSimulation, MockConfig, MockServer, NetworkConditions};

fn example(name: &str, status: u16, body: &str) -> Example {
    Example { name: name.into(), status, body: body.into(), ..Default::default() }
}

fn item(name: &str, method: &str, url: &str, examples: Vec<Example>) -> RequestItem {
    let mut r = RequestItem::new(name, HttpRequest::new(method, url));
    r.examples = examples;
    r
}

fn collection() -> Collection {
    let mut c = Collection::new("API");
    let mut ok = example("OK", 200, "{\"id\": \"{{id}}\", \"n\": {{$randomInt}}}");
    ok.headers.push(KeyValue::new("Content-Type", "application/json"));
    ok.headers.push(KeyValue::new("X-Env", "{{env}}"));
    ok.headers.push(KeyValue::new("Content-Length", "999"));
    let missing = example("Missing", 404, "{\"error\": \"not found\"}");
    c.items.push(item("Get user", "GET", "{{baseUrl}}/users/:id", vec![ok, missing]).into());
    let mut echo = example("Echo", 201, "hello {{$body 'name'}}");
    echo.language = Some("text".into());
    c.items.push(item("Create", "POST", "{{baseUrl}}/users", vec![echo]).into());
    let mut p1 = example("Page 1", 200, "p1");
    p1.request = Some(HttpRequest::new("GET", "{{baseUrl}}/items?page=1"));
    let mut p2 = example("Page 2", 200, "p2");
    p2.request = Some(HttpRequest::new("GET", "{{baseUrl}}/items?page=2"));
    c.items.push(item("Items", "GET", "{{baseUrl}}/items", vec![p1, p2]).into());
    c
}

fn config() -> MockConfig {
    let mut cfg = MockConfig::default();
    cfg.variables.environment.insert("env".into(), "test".into());
    cfg
}

fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn serves_matching_example_with_templating() {
    let server = MockServer::start(collection(), config()).await.unwrap();
    let res = client().get(format!("{}/users/7", server.url())).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(res.headers()["x-env"], "test");
    let v: serde_json::Value = res.json().await.unwrap();
    assert_eq!(v["id"], "7");
    assert!(v["n"].as_u64().unwrap() <= 1000);

    let res = client().post(format!("{}/users", server.url())).body("{\"name\": \"Ada\"}").send().await.unwrap();
    assert_eq!(res.status(), 201);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/plain"));
    assert_eq!(res.text().await.unwrap(), "hello Ada");
    server.stop().await;
}

#[tokio::test]
async fn not_found_is_postman_shaped() {
    let server = MockServer::start(collection(), config()).await.unwrap();
    let res = client().delete(format!("{}/users/7", server.url())).send().await.unwrap();
    assert_eq!(res.status(), 404);
    let v: serde_json::Value = res.json().await.unwrap();
    assert_eq!(v["error"]["name"], "mockRequestNotFoundError");
    let logs = server.logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].status, 404);
    assert!(logs[0].matched.is_none());
}

#[tokio::test]
async fn header_selection_and_query_scoring() {
    let server = MockServer::start(collection(), config()).await.unwrap();
    let c = client();
    let res =
        c.get(format!("{}/users/1", server.url())).header("x-mock-response-name", "Missing").send().await.unwrap();
    assert_eq!(res.status(), 404);
    assert_eq!(res.text().await.unwrap(), "{\"error\": \"not found\"}");
    let res = c.get(format!("{}/users/1", server.url())).header("x-mock-response-code", "404").send().await.unwrap();
    assert_eq!(res.status(), 404);
    let res = c.get(format!("{}/items?page=2", server.url())).send().await.unwrap();
    assert_eq!(res.text().await.unwrap(), "p2");
    let res = c.get(format!("{}/items?page=1", server.url())).send().await.unwrap();
    assert_eq!(res.text().await.unwrap(), "p1");
}

#[tokio::test]
async fn delay_is_applied() {
    let cfg = MockConfig { delay_ms: 250, ..config() };
    let server = MockServer::start(collection(), cfg).await.unwrap();
    let t = Instant::now();
    let res = client().get(format!("{}/users/1", server.url())).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(t.elapsed() >= Duration::from_millis(250));
    assert!(server.logs()[0].duration_ms >= 250.0);
}

#[tokio::test]
async fn network_conditions_throttle_and_jitter() {
    let mut col = collection();
    col.items.push(item("Big", "GET", "{{baseUrl}}/big", vec![example("Big", 200, &"x".repeat(40_000))]).into());
    let network = NetworkConditions { latency_ms: 100, jitter_ms: 50, bandwidth_bytes_per_sec: Some(100_000) };
    let server = MockServer::start(col, MockConfig { network, ..config() }).await.unwrap();
    let t = Instant::now();
    let res = client().get(format!("{}/big", server.url())).send().await.unwrap();
    assert_eq!(res.headers()["content-length"], "40000");
    assert_eq!(res.bytes().await.unwrap().len(), 40_000);
    // 100 ms latency + 40 kB at 100 kB/s (8 slices of 5 kB, 50 ms each).
    let elapsed = t.elapsed();
    assert!(elapsed >= Duration::from_millis(500), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(2000), "{elapsed:?}");
}

#[tokio::test]
async fn simulated_timeout_never_answers() {
    let cfg = MockConfig { failure: Some(FailureSimulation::timeout(1.0)), ..config() };
    let server = MockServer::start(collection(), cfg).await.unwrap();
    let c = reqwest::Client::builder().no_proxy().timeout(Duration::from_millis(300)).build().unwrap();
    let err = c.get(format!("{}/users/1", server.url())).send().await.unwrap_err();
    assert!(err.is_timeout(), "{err}");
}

#[tokio::test]
async fn hot_reload() {
    let server = MockServer::start(collection(), config()).await.unwrap();
    let c = client();
    assert_eq!(c.get(format!("{}/health", server.url())).send().await.unwrap().status(), 404);
    let mut col = collection();
    col.items.push(item("Health", "GET", "{{baseUrl}}/health", vec![example("Up", 200, "up")]).into());
    server.set_collection(col);
    let res = c.get(format!("{}/health", server.url())).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "up");
    assert!(server.routes().iter().any(|r| r.path == "/health"));
}

#[tokio::test]
async fn cors_preflight_and_headers() {
    let server = MockServer::start(collection(), config()).await.unwrap();
    let c = client();
    let res = c
        .request(reqwest::Method::OPTIONS, format!("{}/users/1", server.url()))
        .header("Origin", "http://localhost:5173")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type, x-token")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(res.headers()["access-control-allow-origin"], "http://localhost:5173");
    assert_eq!(res.headers()["access-control-allow-headers"], "content-type, x-token");
    assert!(res.headers()["access-control-allow-methods"].to_str().unwrap().contains("POST"));
    let res = c.get(format!("{}/users/1", server.url())).send().await.unwrap();
    assert_eq!(res.headers()["access-control-allow-origin"], "*");

    let no_cors = MockServer::start(collection(), MockConfig { cors: false, ..config() }).await.unwrap();
    let res = c.get(format!("{}/users/1", no_cors.url())).send().await.unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn logs_are_captured_and_capped() {
    let cfg = MockConfig { log_capacity: 3, ..config() };
    let server = MockServer::start(collection(), cfg).await.unwrap();
    let c = client();
    for i in 0..5 {
        c.post(format!("{}/users?i={i}", server.url()))
            .header("X-Trace", format!("t{i}"))
            .body(format!("{{\"name\": \"n{i}\"}}"))
            .send()
            .await
            .unwrap();
    }
    let logs = server.logs();
    assert_eq!(logs.len(), 3);
    assert_eq!(logs[0].query, "i=2");
    assert_eq!(logs[2].body, "{\"name\": \"n4\"}");
    assert_eq!(logs[2].matched.as_deref(), Some("Create / Echo"));
    assert_eq!(logs[2].status, 201);
    assert_eq!(logs[2].method, "POST");
    assert!(logs[2].headers.iter().any(|(k, v)| k == "x-trace" && v == "t4"));
    server.clear_logs();
    assert!(server.logs().is_empty());
}

#[tokio::test]
async fn failure_simulation() {
    let cfg = MockConfig { failure: Some(FailureSimulation::status(1.0, 503)), ..config() };
    let server = MockServer::start(collection(), cfg).await.unwrap();
    let res = client().get(format!("{}/users/1", server.url())).send().await.unwrap();
    assert_eq!(res.status(), 503);

    let cfg = MockConfig { failure: Some(FailureSimulation::drop_connection(1.0)), ..config() };
    let server = MockServer::start(collection(), cfg).await.unwrap();
    let outcome = match client().get(format!("{}/users/1", server.url())).send().await {
        Ok(res) => res.bytes().await.map(|_| ()),
        Err(e) => Err(e),
    };
    assert!(outcome.is_err(), "dropped connection must surface as a client error");
    assert_eq!(server.logs()[0].status, 0);
}

#[tokio::test]
async fn concurrent_requests_and_clean_stop() {
    let server = MockServer::start(collection(), config()).await.unwrap();
    let url = server.url();
    let c = client();
    let mut tasks = Vec::new();
    for i in 0..50 {
        let c = c.clone();
        let url = format!("{url}/users/{i}");
        tasks.push(tokio::spawn(async move { c.get(url).send().await.unwrap().status().as_u16() }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    assert_eq!(server.logs().len(), 50);
    let addr = server.addr();
    server.stop().await;
    let fresh = reqwest::Client::builder().no_proxy().pool_max_idle_per_host(0).build().unwrap();
    assert!(fresh.get(format!("http://{addr}/users/1")).send().await.is_err());
}
