//! Streamable HTTP transport against an axum server emulating MCP:
//! JSON and SSE responses, session ids, pagination, a GET stream and DELETE.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use futures_util::stream::{self, StreamExt};
use merpati_core::auth::Auth;
use merpati_core::protocols::{McpRequest, McpTransport};
use merpati_mcp::{Content, McpClient, McpConfig, McpError, McpEvent};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};

const SESSION: &str = "sess-42";

#[derive(Default)]
struct AppState {
    allow_get: bool,
    /// `tools/list` cursors go p2 -> p3 -> p2 -> ... (a broken server).
    cycle_cursors: bool,
    list_calls: std::sync::atomic::AtomicUsize,
    deleted: AtomicBool,
    get_opened: AtomicBool,
    versions: Mutex<Vec<String>>,
    ping_answer: Mutex<Option<oneshot::Sender<Value>>>,
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> &'a str {
    h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn sse_of(messages: Vec<Value>) -> Response {
    Sse::new(stream::iter(messages.into_iter().map(|m| Ok::<_, Infallible>(Event::default().data(m.to_string())))))
        .into_response()
}

async fn handle_post(State(st): State<Arc<AppState>>, headers: HeaderMap, body: String) -> Response {
    let accept = header_str(&headers, "accept");
    if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
        return (StatusCode::NOT_ACCEPTABLE, "Accept must list both types").into_response();
    }
    if header_str(&headers, "authorization") != "Bearer test-token" {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"mcp\"")],
            Json(json!({"error": "invalid_token", "error_description": "Missing or invalid access token"})),
        )
            .into_response();
    }
    let msg: Value = serde_json::from_str(&body).expect("client sent valid JSON");
    let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
    let id = msg.get("id").cloned().filter(|i| !i.is_null());

    if method.as_deref() == Some("initialize") {
        let result = json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}, "resources": {}, "prompts": {}, "logging": {}},
            "serverInfo": {"name": "fake-http", "title": "Fake HTTP", "version": "0.9.0"}
        });
        return ([("mcp-session-id", SESSION)], Json(rpc_result(&id.unwrap(), result))).into_response();
    }
    if header_str(&headers, "mcp-session-id") != SESSION {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32000, "message": "Bad Request: No valid session ID provided"}})),
        )
            .into_response();
    }
    if st.deleted.load(Ordering::SeqCst) {
        return StatusCode::NOT_FOUND.into_response();
    }
    st.versions.lock().push(header_str(&headers, "mcp-protocol-version").to_string());

    let (method, id) = match (method, id) {
        (None, Some(_)) => {
            // A response to our server-to-client ping.
            if let Some(tx) = st.ping_answer.lock().take() {
                let _ = tx.send(msg.clone());
            }
            return StatusCode::ACCEPTED.into_response();
        }
        (Some(_), None) | (None, None) => return StatusCode::ACCEPTED.into_response(),
        (Some(m), Some(id)) => (m, id),
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match method.as_str() {
        "ping" => Json(rpc_result(&id, json!({}))).into_response(),
        "tools/list" => {
            let echo = json!({"name": "echo", "description": "Echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}});
            let add = json!({"name": "add", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}});
            let ask = json!({"name": "ask_ping", "inputSchema": {"type": "object"}});
            st.list_calls.fetch_add(1, Ordering::SeqCst);
            let result = match (params.get("cursor").and_then(Value::as_str), st.cycle_cursors) {
                (None, _) => json!({"tools": [echo, add], "nextCursor": "p2"}),
                (Some("p2"), false) => json!({"tools": [ask]}),
                (Some("p2"), true) => json!({"tools": [ask], "nextCursor": "p3"}),
                (Some("p3"), true) => json!({"tools": [], "nextCursor": "p2"}),
                (Some(other), _) => panic!("unexpected cursor {other}"),
            };
            Json(rpc_result(&id, result)).into_response()
        }
        "tools/call" => match params["name"].as_str().unwrap_or("") {
            "echo" => sse_of(vec![
                json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "debug", "data": {"step": 1}}}),
                json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": 1, "progress": 0.5}}),
                rpc_result(&id, json!({"content": [{"type": "text", "text": params["arguments"]["text"]}]})),
            ]),
            "add" => {
                let sum = params["arguments"]["a"].as_f64().unwrap_or(0.0) + params["arguments"]["b"].as_f64().unwrap_or(0.0);
                Json(rpc_result(&id, json!({"content": [{"type": "text", "text": sum.to_string()}], "structuredContent": {"sum": sum}})))
                    .into_response()
            }
            "ask_ping" => {
                let (answer_tx, answer_rx) = oneshot::channel();
                *st.ping_answer.lock() = Some(answer_tx);
                let (tx, rx) = mpsc::unbounded_channel::<Value>();
                tx.send(json!({"jsonrpc": "2.0", "id": "s1", "method": "ping"})).unwrap();
                tokio::spawn(async move {
                    let answer = answer_rx.await.unwrap();
                    let text = if answer["id"] == "s1" && answer.get("result").is_some() { "pong received" } else { "bad" };
                    let _ = tx.send(rpc_result(&id, json!({"content": [{"type": "text", "text": text}]})));
                });
                let events = stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|m| (Ok::<_, Infallible>(Event::default().data(m.to_string())), rx))
                });
                Sse::new(events).into_response()
            }
            other => Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("Unknown tool {other}")}}))
                .into_response(),
        },
        "resources/list" => {
            Json(rpc_result(&id, json!({"resources": [{"uri": "file:///a.txt", "name": "a.txt"}]}))).into_response()
        }
        "resources/read" => sse_of(vec![rpc_result(
            &id,
            json!({"contents": [{"uri": params["uri"], "mimeType": "text/plain", "text": "A"}]}),
        )]),
        "prompts/list" => Json(rpc_result(&id, json!({"prompts": [{"name": "p", "arguments": [{"name": "n"}]}]}))).into_response(),
        "prompts/get" => {
            Json(rpc_result(&id, json!({"messages": [{"role": "user", "content": {"type": "text", "text": params["arguments"]["n"]}}]})))
                .into_response()
        }
        _ => Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})).into_response(),
    }
}

async fn handle_get(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !st.allow_get || header_str(&headers, "mcp-session-id") != SESSION {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    st.get_opened.store(true, Ordering::SeqCst);
    let first = Event::default()
        .id("ev-1")
        .data(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}).to_string());
    let events = stream::iter(vec![Ok::<_, Infallible>(first)]).chain(stream::pending());
    Sse::new(events).into_response()
}

async fn handle_delete(State(st): State<Arc<AppState>>, headers: HeaderMap) -> StatusCode {
    if header_str(&headers, "mcp-session-id") == SESSION {
        st.deleted.store(true, Ordering::SeqCst);
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    }
}

async fn spawn_server(allow_get: bool) -> (String, Arc<AppState>) {
    spawn_with(AppState { allow_get, ..Default::default() }).await
}

async fn spawn_with(state: AppState) -> (String, Arc<AppState>) {
    merpati_http::tls::install_provider();
    let state = Arc::new(state);
    let app =
        Router::new().route("/mcp", post(handle_post).get(handle_get).delete(handle_delete)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/mcp"), state)
}

fn request(url: &str, token: Option<&str>) -> McpRequest {
    McpRequest {
        transport: McpTransport::Http { url: url.into(), headers: vec![] },
        saved_calls: vec![],
        auth: token.map(Auth::bearer).unwrap_or_default(),
        ..Default::default()
    }
}

type Events = Arc<Mutex<Vec<McpEvent>>>;

fn recorder() -> (Events, impl Fn(McpEvent) + Send + Sync + 'static) {
    let events: Events = Arc::default();
    let sink = events.clone();
    (events, move |e| sink.lock().push(e))
}

#[tokio::test]
async fn streamable_http_session() {
    let (url, state) = spawn_server(false).await;
    let (events, on_event) = recorder();
    let cfg = McpConfig::from_request(&request(&url, Some("test-token")), &|s| s.to_string());
    let client = McpClient::connect(cfg, on_event).await.expect("connect");
    let info = client.server_info();
    assert_eq!(
        (info.name.as_str(), info.title.as_deref(), info.version.as_str()),
        ("fake-http", Some("Fake HTTP"), "0.9.0")
    );
    assert_eq!(info.protocol_version, "2025-06-18");

    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["echo", "add", "ask_ping"]);

    // SSE response with notifications before the result.
    let r = client.call_tool("echo", json!({"text": "over sse"})).await.unwrap();
    assert_eq!(r.content, vec![Content::Text { text: "over sse".into() }]);
    // JSON response.
    let r = client.call_tool("add", json!({"a": 1, "b": 2})).await.unwrap();
    assert_eq!(r.structured, Some(json!({"sum": 3.0})));
    // Server-to-client request inside an SSE response, answered by POST.
    let r = client.call_tool("ask_ping", json!({})).await.unwrap();
    assert_eq!(r.text(), "pong received");
    let err = client.call_tool("missing", json!({})).await.unwrap_err();
    assert_eq!(err.rpc_code(), Some(-32602));

    assert_eq!(client.list_resources().await.unwrap()[0].name, "a.txt");
    assert_eq!(client.read_resource("file:///a.txt").await.unwrap()["contents"][0]["text"], "A");
    assert_eq!(client.list_prompts().await.unwrap()[0].arguments[0].name, "n");
    let mut args = Map::new();
    args.insert("n".into(), json!(5));
    assert_eq!(client.get_prompt("p", args).await.unwrap()["messages"][0]["content"]["text"], "5");
    // Nothing advertised for templates and the server does not know the method.
    assert!(client.list_resource_templates().await.is_err());
    client.ping().await.unwrap();

    {
        let ev = events.lock();
        assert!(
            ev.iter().any(
                |e| matches!(e, McpEvent::Log { level, message } if level == "debug" && message == "{\"step\":1}")
            )
        );
        assert!(ev.iter().any(|e| matches!(e, McpEvent::Notification { method, params } if method == "notifications/progress" && params["progress"] == 0.5)));
    }
    let versions = state.versions.lock().clone();
    assert!(!versions.is_empty());
    assert!(versions.iter().all(|v| v == "2025-06-18"), "{versions:?}");

    client.close().await;
    assert!(state.deleted.load(Ordering::SeqCst), "DELETE sent on close");
    assert!(matches!(client.list_tools().await, Err(McpError::Closed(_))));
}

#[tokio::test]
async fn unauthorized_surfaces_server_message() {
    let (url, _) = spawn_server(false).await;
    let cfg = McpConfig::from_request(&request(&url, None), &|s| s.to_string());
    let err = McpClient::connect(cfg, |_| {}).await.unwrap_err();
    let McpError::Http { status, message } = &err else { panic!("{err:?}") };
    assert_eq!(*status, 401);
    assert!(message.contains("Missing or invalid access token"), "{message}");
    assert!(message.contains("Bearer realm"), "{message}");
}

#[tokio::test]
async fn get_stream_delivers_server_notifications() {
    let (url, state) = spawn_server(true).await;
    let (events, on_event) = recorder();
    let cfg = McpConfig::from_request(&request(&url, Some("test-token")), &|s| s.to_string());
    let client = McpClient::connect(cfg, on_event).await.unwrap();
    for _ in 0..100 {
        if events
            .lock()
            .iter()
            .any(|e| matches!(e, McpEvent::Notification { method, .. } if method == "notifications/tools/list_changed"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(state.get_opened.load(Ordering::SeqCst));
    assert!(
        events.lock().iter().any(
            |e| matches!(e, McpEvent::Notification { method, .. } if method == "notifications/tools/list_changed")
        )
    );
    tokio::time::timeout(Duration::from_secs(5), client.close()).await.expect("close does not hang on the open stream");
}

#[tokio::test]
async fn expired_session_is_reported() {
    let (url, state) = spawn_server(false).await;
    let cfg = McpConfig::from_request(&request(&url, Some("test-token")), &|s| s.to_string());
    let client = McpClient::connect(cfg, |_| {}).await.unwrap();
    state.deleted.store(true, Ordering::SeqCst);
    let err = client.list_tools().await.unwrap_err();
    assert!(matches!(&err, McpError::Closed(m) if m.contains("404")), "{err:?}");
}

#[tokio::test]
async fn connection_refused_is_a_transport_error() {
    merpati_http::tls::install_provider();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let cfg = McpConfig::from_request(&request(&format!("http://127.0.0.1:{port}/mcp"), None), &|s| s.to_string());
    let err = McpClient::connect(cfg, |_| {}).await.unwrap_err();
    assert!(matches!(&err, McpError::Transport(m) if m.to_lowercase().contains("refused")), "{err:?}");
}

#[tokio::test]
async fn invalid_url_is_a_config_error() {
    let cfg = McpConfig::from_request(&request("ftp://example.com/mcp", None), &|s| s.to_string());
    assert!(matches!(McpClient::connect(cfg, |_| {}).await, Err(McpError::Config(_))));
}

#[tokio::test]
async fn dropping_on_a_plain_thread_still_deletes_the_session() {
    let (url, state) = spawn_server(false).await;
    let cfg = McpConfig::from_request(&request(&url, Some("test-token")), &|s| s.to_string());
    let client = McpClient::connect(cfg, |_| {}).await.unwrap();
    std::thread::spawn(move || drop(client)).join().unwrap();
    for _ in 0..100 {
        if state.deleted.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no DELETE after drop");
}

#[tokio::test]
async fn pagination_stops_when_cursors_repeat() {
    let (url, state) = spawn_with(AppState { cycle_cursors: true, ..Default::default() }).await;
    let cfg = McpConfig::from_request(&request(&url, Some("test-token")), &|s| s.to_string());
    let client = McpClient::connect(cfg, |_| {}).await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["echo", "add", "ask_ping"]);
    assert_eq!(state.list_calls.load(Ordering::SeqCst), 3);
    client.close().await;
}
