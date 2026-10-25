//! Streamable HTTP against misbehaving servers: a notification POST that
//! never answers, a response stream that breaks before the answer (resumed
//! with `Last-Event-ID`), a GET stream asking for a short `retry:`, and
//! responses larger than the client accepts.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use futures_util::stream::{self, StreamExt};
use merpati_mcp::{McpClient, McpConfig, McpError, McpEvent, Transport};
use parking_lot::Mutex;
use serde_json::{Value, json};

const SESSION: &str = "sess-7";
const MIB: usize = 1024 * 1024;

#[derive(Default)]
struct AppState {
    /// `notifications/initialized` is never answered.
    hang_initialized: bool,
    /// The GET stream sends one event with `retry: 150`, then closes.
    listen_with_retry: bool,
    gets: AtomicUsize,
    /// `Last-Event-ID` headers of GET requests.
    resumed_from: Mutex<Vec<String>>,
    /// The `tools/call` id answered on the resumed stream.
    resume_id: Mutex<Option<Value>>,
    initialized_seen: AtomicBool,
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> &'a str {
    h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn text_result(id: &Value, text: &str) -> Value {
    result(id, json!({"content": [{"type": "text", "text": text}]}))
}

async fn handle_post(State(st): State<Arc<AppState>>, body: String) -> Response {
    let msg: Value = serde_json::from_str(&body).unwrap();
    let method = msg["method"].as_str().unwrap_or("").to_string();
    let Some(id) = msg.get("id").cloned().filter(|i| !i.is_null()) else {
        if method == "notifications/initialized" {
            st.initialized_seen.store(true, Ordering::SeqCst);
            if st.hang_initialized {
                std::future::pending::<()>().await;
            }
        }
        return StatusCode::ACCEPTED.into_response();
    };
    match method.as_str() {
        "initialize" => {
            let r = json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "streams", "version": "1"}});
            ([("mcp-session-id", SESSION)], Json(result(&id, r))).into_response()
        }
        "ping" => Json(result(&id, json!({}))).into_response(),
        "tools/call" => match msg["params"]["name"].as_str().unwrap_or("") {
            // A progress event with an id, then the stream ends without the
            // answer: the client must resume it with a GET.
            "resume" => {
                *st.resume_id.lock() = Some(id);
                let progress = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": 1, "progress": 0.5}});
                let first = Event::default().id("e1").retry(Duration::from_millis(100)).data(progress.to_string());
                Sse::new(stream::iter([Ok::<_, Infallible>(first)])).into_response()
            }
            // 65 MiB of JSON with a Content-Length.
            "big-json" => {
                let mut body = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":""#)
                    .into_bytes();
                body.resize(body.len() + 65 * MIB, b'x');
                body.extend_from_slice(br#""}]}}"#);
                ([(header::CONTENT_TYPE, "application/json")], body).into_response()
            }
            // One 66 MiB event, streamed without a length.
            "big-sse" => {
                let head =
                    format!(r#"data: {{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":""#);
                let chunks = std::iter::once(head.into_bytes())
                    .chain((0..66).map(|_| vec![b'x'; MIB]))
                    .chain(std::iter::once(b"\"}]}}\n\n".to_vec()))
                    .map(Ok::<_, Infallible>);
                ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(stream::iter(chunks))).into_response()
            }
            other => Json(text_result(&id, other)).into_response(),
        },
        _ => Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "nope"}})).into_response(),
    }
}

async fn handle_get(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if header_str(&headers, "mcp-session-id") != SESSION {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let last = header_str(&headers, "last-event-id").to_string();
    if last == "e1" {
        st.resumed_from.lock().push(last);
        let id = st.resume_id.lock().clone().unwrap_or(Value::Null);
        let answer = Event::default().id("e2").data(text_result(&id, "resumed").to_string());
        return Sse::new(stream::iter([Ok::<_, Infallible>(answer)]).chain(stream::pending())).into_response();
    }
    if !st.listen_with_retry {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let n = st.gets.fetch_add(1, Ordering::SeqCst) + 1;
    let note = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
    let ev = Event::default().id(format!("l{n}")).retry(Duration::from_millis(150)).data(note.to_string());
    Sse::new(stream::iter([Ok::<_, Infallible>(ev)])).into_response()
}

async fn spawn(state: AppState) -> (String, Arc<AppState>) {
    merpati_http::tls::install_provider();
    let state = Arc::new(state);
    let app = Router::new().route("/mcp", post(handle_post).get(handle_get)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/mcp"), state)
}

fn config(url: &str, timeout: Duration) -> McpConfig {
    McpConfig { transport: Transport::Http { url: url.into(), headers: vec![] }, timeout, ..Default::default() }
}

type Events = Arc<Mutex<Vec<McpEvent>>>;

fn recorder() -> (Events, impl Fn(McpEvent) + Send + Sync + 'static) {
    let events: Events = Arc::default();
    let sink = events.clone();
    (events, move |e| sink.lock().push(e))
}

async fn wait_for(events: &Events, what: &str, pred: impl Fn(&McpEvent) -> bool) {
    for _ in 0..200 {
        if events.lock().iter().any(&pred) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}; events: {:#?}", events.lock());
}

#[tokio::test]
async fn a_hung_notification_post_does_not_block_later_messages() {
    let (url, state) = spawn(AppState { hang_initialized: true, ..Default::default() }).await;
    let (events, on_event) = recorder();
    let client = McpClient::connect(config(&url, Duration::from_secs(2)), on_event).await.unwrap();
    let started = Instant::now();
    client.ping().await.expect("the ping is not stuck behind notifications/initialized");
    assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
    assert!(state.initialized_seen.load(Ordering::SeqCst), "initialized was sent first");
    // The hung POST itself gives up after the request timeout.
    wait_for(&events, "warning", |e| {
        matches!(e, McpEvent::Log { level, message } if level == "warning" && message.contains("no answer from the server within 2 s"))
    })
    .await;
    client.ping().await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn a_broken_response_stream_is_resumed_with_last_event_id() {
    let (url, state) = spawn(AppState::default()).await;
    let (events, on_event) = recorder();
    let client = McpClient::connect(config(&url, Duration::from_secs(10)), on_event).await.unwrap();
    let r = client.call_tool("resume", json!({})).await.expect("answered on the resumed stream");
    assert_eq!(r.text(), "resumed");
    assert_eq!(state.resumed_from.lock().as_slice(), ["e1"]);
    assert!(
        events
            .lock()
            .iter()
            .any(|e| matches!(e, McpEvent::Notification { method, .. } if method == "notifications/progress"))
    );
    // The session goes on.
    assert_eq!(client.call_tool("echo", json!({})).await.unwrap().text(), "echo");
    client.close().await;
}

#[tokio::test]
async fn the_get_stream_reconnects_after_the_servers_retry_delay() {
    let (url, state) = spawn(AppState { listen_with_retry: true, ..Default::default() }).await;
    let client = McpClient::connect(config(&url, Duration::from_secs(10)), |_| {}).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    // retry: 150 instead of the default 2 s between connections.
    let gets = state.gets.load(Ordering::SeqCst);
    assert!(gets >= 4, "{gets} GET requests");
    client.close().await;
}

#[tokio::test]
async fn responses_over_the_size_limit_fail_the_request_not_the_session() {
    let (url, _) = spawn(AppState::default()).await;
    let client = McpClient::connect(config(&url, Duration::from_secs(30)), |_| {}).await.unwrap();
    for tool in ["big-json", "big-sse"] {
        let err = client.call_tool(tool, json!({})).await.unwrap_err();
        assert!(matches!(&err, McpError::Protocol(m) if m.contains("larger than 64 MiB")), "{tool}: {err:?}");
        client.ping().await.unwrap();
    }
    client.close().await;
}
