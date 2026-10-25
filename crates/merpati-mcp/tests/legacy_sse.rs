//! Legacy HTTP+SSE transport (protocol 2024-11-05) against an axum server:
//! GET /sse announces `/messages?sessionId=..`, POSTs are answered with 202
//! and the responses travel on the event stream.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::stream;
use merpati_core::protocols::{McpHttpTransport, McpRequest, McpTransport};
use merpati_mcp::{Content, McpClient, McpConfig, McpError, McpEvent, SamplingRequest, SamplingResult};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::mpsc;

#[derive(Default)]
struct SseState {
    sessions: Mutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
    /// Server request id -> waiting tool call id.
    waiting: Mutex<HashMap<String, Value>>,
    next: AtomicUsize,
    initialized: AtomicBool,
    stream_dropped: Arc<AtomicBool>,
    /// Announce an endpoint on another origin.
    foreign_endpoint: bool,
    /// End the stream after this many messages.
    close_after: Option<usize>,
}

struct OnDrop(Arc<AtomicBool>);
impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn open_sse(State(st): State<Arc<SseState>>) -> Response {
    let n = st.next.fetch_add(1, Ordering::SeqCst);
    let sid = format!("s{n}");
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    st.sessions.lock().insert(sid.clone(), tx);
    let endpoint = if st.foreign_endpoint {
        format!("http://other.invalid/messages?sessionId={sid}")
    } else {
        format!("/messages?sessionId={sid}")
    };
    let first = stream::iter(vec![Ok::<_, Infallible>(Event::default().event("endpoint").data(endpoint))]);
    let guard = OnDrop(st.stream_dropped.clone());
    let limit = st.close_after.unwrap_or(usize::MAX);
    let messages = stream::unfold((rx, guard, 0usize), move |(mut rx, guard, sent)| async move {
        if sent >= limit {
            return None;
        }
        let m = rx.recv().await?;
        Some((Ok::<_, Infallible>(Event::default().event("message").data(m.to_string())), (rx, guard, sent + 1)))
    });
    use futures_util::StreamExt;
    Sse::new(first.chain(messages)).into_response()
}

async fn post_message(
    State(st): State<Arc<SseState>>,
    Query(q): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let Some(tx) = q.get("sessionId").and_then(|s| st.sessions.lock().get(s).cloned()) else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };
    let msg: Value = serde_json::from_str(&body).expect("valid JSON");
    let id = msg.get("id").cloned().filter(|i| !i.is_null());
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    match (method.as_str(), &id) {
        ("", Some(rid)) => {
            // The client's answer to our request.
            if let Some(call) = rid.as_str().and_then(|r| st.waiting.lock().remove(r)) {
                let text = msg.get("result").map(|r| r["content"]["text"].as_str().unwrap_or("").to_string());
                let _ = tx.send(json!({"jsonrpc": "2.0", "id": call, "result": {"content": [{"type": "text", "text": text.unwrap_or_default()}]}}));
            }
        }
        ("notifications/initialized", None) => st.initialized.store(true, Ordering::SeqCst),
        (_, None) => {}
        ("initialize", Some(_)) => {
            let _ = tx.send(reply(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "legacy-sse", "version": "0.1.0"}
            })));
        }
        ("ping", Some(_)) => {
            let _ = tx.send(reply(json!({})));
        }
        ("tools/list", Some(_)) => {
            let _ = tx.send(reply(json!({"tools": [
                {"name": "echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
                {"name": "sample", "inputSchema": {"type": "object"}}
            ]})));
        }
        ("tools/call", Some(call_id)) => match params["name"].as_str() {
            Some("echo") => {
                let _ = tx.send(reply(json!({"content": [{"type": "text", "text": params["arguments"]["text"]}]})));
            }
            Some("sample") => {
                let rid = format!("srv-{}", st.next.fetch_add(1, Ordering::SeqCst));
                st.waiting.lock().insert(rid.clone(), call_id.clone());
                let _ = tx.send(json!({"jsonrpc": "2.0", "id": rid, "method": "sampling/createMessage", "params": {
                    "messages": [{"role": "user", "content": {"type": "text", "text": "over sse"}}], "maxTokens": 10
                }}));
            }
            _ => {
                let _ =
                    tx.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "Unknown tool"}}));
            }
        },
        (_, Some(_)) => {
            let _ =
                tx.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}));
        }
    }
    (StatusCode::ACCEPTED, "Accepted").into_response()
}

async fn spawn(state: SseState) -> (String, Arc<SseState>) {
    merpati_http::tls::install_provider();
    let state = Arc::new(state);
    let app =
        Router::new().route("/sse", get(open_sse)).route("/messages", post(post_message)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/sse"), state)
}

fn config(url: &str, mode: McpHttpTransport) -> McpConfig {
    let req = McpRequest {
        transport: McpTransport::Http { url: url.into(), headers: vec![] },
        http_transport: mode,
        ..Default::default()
    };
    let mut cfg = McpConfig::from_request(&req, &|s| s.to_string());
    cfg.timeout = Duration::from_secs(5);
    cfg
}

type Events = Arc<Mutex<Vec<McpEvent>>>;

fn recorder() -> (Events, impl Fn(McpEvent) + Send + Sync + 'static) {
    let events: Events = Arc::default();
    let sink = events.clone();
    (events, move |e| sink.lock().push(e))
}

#[tokio::test]
async fn explicit_sse_session() {
    let (url, state) = spawn(SseState::default()).await;
    let client = McpClient::builder(config(&url, McpHttpTransport::Sse))
        .sampling(
            |req: SamplingRequest| async move { Ok(SamplingResult::text("m", format!("got {}", req.transcript()))) },
        )
        .connect()
        .await
        .expect("connect");
    assert_eq!(client.transport_name(), "sse");
    assert_eq!(client.server_info().name, "legacy-sse");
    assert_eq!(client.server_info().protocol_version, "2024-11-05");
    let tools = client.list_tools().await.unwrap();
    assert!(state.initialized.load(Ordering::SeqCst), "notifications/initialized is posted before later requests");
    assert_eq!(tools.len(), 2);
    let r = client.call_tool("echo", json!({"text": "hello sse"})).await.unwrap();
    assert_eq!(r.content, vec![Content::Text { text: "hello sse".into() }]);
    // A server-to-client request on the stream, answered by POST.
    let r = client.call_tool("sample", json!({})).await.unwrap();
    assert_eq!(r.text(), "got user: over sse");
    let err = client.call_tool("nope", json!({})).await.unwrap_err();
    assert_eq!(err.rpc_code(), Some(-32602));
    client.ping().await.unwrap();
    client.close().await;
    for _ in 0..100 {
        if state.stream_dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(state.stream_dropped.load(Ordering::SeqCst), "closing drops the event stream");
}

#[tokio::test]
async fn auto_falls_back_from_streamable_http() {
    let (url, _state) = spawn(SseState::default()).await;
    let (events, on_event) = recorder();
    let client = McpClient::connect(config(&url, McpHttpTransport::Auto), on_event).await.expect("connect");
    assert_eq!(client.transport_name(), "sse");
    assert_eq!(client.list_tools().await.unwrap().len(), 2);
    assert!(events.lock().iter().any(
        |e| matches!(e, McpEvent::Log { level, message } if level == "warning" && message.contains("legacy HTTP+SSE"))
    ));
    assert!(!events.lock().iter().any(|e| matches!(e, McpEvent::Closed(_))), "no Closed event for the first attempt");
    client.close().await;
}

#[tokio::test]
async fn streamable_only_does_not_fall_back() {
    let (url, _state) = spawn(SseState::default()).await;
    let err = McpClient::connect(config(&url, McpHttpTransport::Streamable), |_| {}).await.unwrap_err();
    assert!(matches!(err, McpError::Http { status: 405, .. }), "{err:?}");
}

#[tokio::test]
async fn auto_reports_both_failures() {
    let (url, _state) = spawn(SseState::default()).await;
    let url = url.replace("/sse", "/nothing-here");
    let (events, on_event) = recorder();
    let err = McpClient::connect(config(&url, McpHttpTransport::Auto), on_event).await.unwrap_err();
    let McpError::Http { status: 404, message } = &err else { panic!("{err:?}") };
    assert!(message.contains("HTTP+SSE fallback"), "{message}");
    let closed = events.lock().iter().filter(|e| matches!(e, McpEvent::Closed(_))).count();
    assert_eq!(closed, 1);
}

#[tokio::test]
async fn foreign_endpoint_is_refused() {
    let (url, _state) = spawn(SseState { foreign_endpoint: true, ..Default::default() }).await;
    let err = McpClient::connect(config(&url, McpHttpTransport::Sse), |_| {}).await.unwrap_err();
    assert!(matches!(&err, McpError::Protocol(m) if m.contains("another origin")), "{err:?}");
}

#[tokio::test]
async fn server_closing_the_stream_closes_the_session() {
    // The stream ends after the initialize and tools/list responses.
    let (url, _state) = spawn(SseState { close_after: Some(2), ..Default::default() }).await;
    let (events, on_event) = recorder();
    let client = McpClient::connect(config(&url, McpHttpTransport::Sse), on_event).await.expect("connect");
    assert_eq!(client.list_tools().await.unwrap().len(), 2);
    for _ in 0..100 {
        if client.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        client.closed_reason().is_some_and(|r| r.contains("closed the event stream")),
        "{:?}",
        client.closed_reason()
    );
    assert!(matches!(client.list_tools().await, Err(McpError::Closed(_))));
    assert!(events.lock().iter().any(|e| matches!(e, McpEvent::Closed(_))));
}
