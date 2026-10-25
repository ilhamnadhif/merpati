//! Every provider against an axum mock: non-streaming JSON, SSE streaming
//! (delivered in small chunks), one tool round through a fake MCP server,
//! provider errors, cancellation and model listing. No real network.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures_util::{StreamExt, stream};
use merpati_ai::{AiConfig, AiError, AiEvent, CancellationToken, list_models, run};
use merpati_core::protocols::{AiMessage, AiProvider};
use merpati_mcp::{McpClient, McpConfig, Tool, Transport};
use parking_lot::Mutex;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Mock provider server
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Recorded {
    path: String,
    query: String,
    headers: HeaderMap,
    body: Value,
}

#[derive(Default)]
struct Mock {
    requests: Mutex<Vec<Recorded>>,
}

impl Mock {
    fn record(&self, path: &str, query: Option<String>, headers: &HeaderMap, body: &Value) {
        self.requests.lock().push(Recorded {
            path: path.to_string(),
            query: query.unwrap_or_default(),
            headers: headers.clone(),
            body: body.clone(),
        });
    }

    fn take(&self) -> Vec<Recorded> {
        std::mem::take(&mut *self.requests.lock())
    }
}

fn h<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// An SSE body delivered in 7-byte chunks to exercise chunk boundaries.
fn sse(events: Vec<(Option<&str>, String)>) -> Response {
    let mut text = String::new();
    for (event, data) in events {
        if let Some(e) = event {
            text.push_str(&format!("event: {e}\n"));
        }
        text.push_str(&format!("data: {data}\n\n"));
    }
    let chunks: Vec<Result<Vec<u8>, Infallible>> = text.into_bytes().chunks(7).map(|c| Ok(c.to_vec())).collect();
    ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(stream::iter(chunks))).into_response()
}

fn data_only(chunks: Vec<Value>) -> Response {
    let mut events: Vec<(Option<&str>, String)> = chunks.into_iter().map(|c| (None, c.to_string())).collect();
    events.push((None, "[DONE]".into()));
    sse(events)
}

/// The text of the tool result the client sent back, if any.
fn tool_result_text(body: &Value) -> Option<String> {
    let all = [body.get("messages"), body.get("contents")];
    for list in all.into_iter().flatten().filter_map(Value::as_array) {
        for m in list {
            if m["role"] == "tool" {
                return m["content"].as_str().map(str::to_string);
            }
            for part in m.get("content").and_then(Value::as_array).into_iter().flatten() {
                if part["type"] == "tool_result" {
                    return part["content"].as_str().map(str::to_string);
                }
            }
            for part in m.get("parts").and_then(Value::as_array).into_iter().flatten() {
                if let Some(r) = part.pointer("/functionResponse/response/result") {
                    return r.as_str().map(str::to_string);
                }
            }
        }
    }
    None
}

async fn openai_chat(State(m): State<Arc<Mock>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    m.record("/openai/v1/chat/completions", None, &headers, &body);
    if h(&headers, "authorization") == "Bearer bad-key" {
        let err = json!({"error": {"message": "Incorrect API key provided: bad-key.", "type": "invalid_request_error", "code": "invalid_api_key"}});
        return (StatusCode::UNAUTHORIZED, Json(err)).into_response();
    }
    let model = body["model"].as_str().unwrap_or("").to_string();
    if model == "slow" {
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let result = tool_result_text(&body);
    let wants_tool = body.get("tools").is_some() && (result.is_none() || model == "always-tool");
    let stream = body["stream"] == true;
    let text = match &result {
        Some(r) => format!("The sum is {r}"),
        None => "Hello from openai".to_string(),
    };
    match (stream, wants_tool) {
        (false, true) => Json(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "model": "gpt-test-0001",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "call_abc", "type": "function", "function": {"name": "add", "arguments": "{\"a\":2,\"b\":3}"}}]}}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 7}
        }))
        .into_response(),
        (false, false) => Json(json!({
            "id": "chatcmpl-2", "object": "chat.completion", "model": "gpt-test-0001",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": text}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }))
        .into_response(),
        (true, true) => data_only(vec![
            json!({"id": "c1", "model": "gpt-test-0001", "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [
                {"index": 0, "id": "call_abc", "type": "function", "function": {"name": "add", "arguments": ""}}]}}]}),
            json!({"id": "c1", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"a\":2,"}}]}}]}),
            json!({"id": "c1", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"b\":3}"}}]}}]}),
            json!({"id": "c1", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"id": "c1", "choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 7}}),
        ]),
        (true, false) => {
            let (a, b) = text.split_at(3);
            data_only(vec![
                json!({"id": "c2", "model": "gpt-test-0001", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
                json!({"id": "c2", "choices": [{"index": 0, "delta": {"content": a}}]}),
                json!({"id": "c2", "choices": [{"index": 0, "delta": {"content": b}}]}),
                json!({"id": "c2", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                json!({"id": "c2", "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}),
            ])
        }
    }
}

async fn openai_models(State(m): State<Arc<Mock>>, headers: HeaderMap) -> Response {
    m.record("/openai/v1/models", None, &headers, &Value::Null);
    Json(json!({"object": "list", "data": [{"id": "gpt-b"}, {"id": "gpt-a"}]})).into_response()
}

async fn anthropic_messages(State(m): State<Arc<Mock>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    m.record("/anthropic/v1/messages", None, &headers, &body);
    if h(&headers, "anthropic-version") != "2023-06-01" {
        return (StatusCode::BAD_REQUEST, "missing anthropic-version").into_response();
    }
    if h(&headers, "x-api-key") == "bad-key" {
        let err = json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}});
        return (StatusCode::UNAUTHORIZED, Json(err)).into_response();
    }
    let model = body["model"].as_str().unwrap_or("");
    let stream = body["stream"] == true;
    if model == "overloaded" {
        return sse(vec![
            (Some("message_start"), json!({"type": "message_start", "message": {"id": "msg_x", "model": "claude-test", "content": [], "usage": {"input_tokens": 3}}}).to_string()),
            (Some("error"), json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}).to_string()),
        ]);
    }
    let result = tool_result_text(&body);
    let wants_tool = body.get("tools").is_some() && result.is_none();
    let text = match &result {
        Some(r) => format!("The sum is {r}"),
        None => "Hello from anthropic".to_string(),
    };
    match (stream, wants_tool) {
        (false, true) => Json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-test",
            "content": [{"type": "text", "text": "Let me add."}, {"type": "tool_use", "id": "toolu_1", "name": "add", "input": {"a": 2, "b": 3}}],
            "stop_reason": "tool_use", "usage": {"input_tokens": 30, "output_tokens": 9}
        }))
        .into_response(),
        (false, false) => Json(json!({
            "id": "msg_2", "type": "message", "role": "assistant", "model": "claude-test",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 40, "output_tokens": 6, "cache_read_input_tokens": 2}
        }))
        .into_response(),
        (true, true) => sse(vec![
            (Some("message_start"), json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-test", "content": [], "stop_reason": null, "usage": {"input_tokens": 30, "output_tokens": 1}}}).to_string()),
            (Some("content_block_start"), json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}).to_string()),
            (Some("content_block_delta"), json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "need add"}}).to_string()),
            (Some("content_block_delta"), json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig123"}}).to_string()),
            (Some("content_block_stop"), json!({"type": "content_block_stop", "index": 0}).to_string()),
            (Some("ping"), json!({"type": "ping"}).to_string()),
            (Some("content_block_start"), json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}).to_string()),
            (Some("content_block_delta"), json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Let me "}}).to_string()),
            (Some("content_block_delta"), json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "add."}}).to_string()),
            (Some("content_block_stop"), json!({"type": "content_block_stop", "index": 1}).to_string()),
            (Some("content_block_start"), json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "add", "input": {}}}).to_string()),
            (Some("content_block_delta"), json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"a\": 2"}}).to_string()),
            (Some("content_block_delta"), json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": ", \"b\": 3}"}}).to_string()),
            (Some("content_block_stop"), json!({"type": "content_block_stop", "index": 2}).to_string()),
            (Some("message_delta"), json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 9}}).to_string()),
            (Some("message_stop"), json!({"type": "message_stop"}).to_string()),
        ]),
        (true, false) => {
            let (a, b) = text.split_at(4);
            sse(vec![
                (Some("message_start"), json!({"type": "message_start", "message": {"id": "msg_2", "type": "message", "role": "assistant", "model": "claude-test", "content": [], "usage": {"input_tokens": 40, "output_tokens": 1}}}).to_string()),
                (Some("content_block_start"), json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}).to_string()),
                (Some("content_block_delta"), json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": a}}).to_string()),
                (Some("content_block_delta"), json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": b}}).to_string()),
                (Some("content_block_stop"), json!({"type": "content_block_stop", "index": 0}).to_string()),
                (Some("message_delta"), json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 6}}).to_string()),
                (Some("message_stop"), json!({"type": "message_stop"}).to_string()),
            ])
        }
    }
}

async fn anthropic_models(State(m): State<Arc<Mock>>, headers: HeaderMap, RawQuery(q): RawQuery) -> Response {
    m.record("/anthropic/v1/models", q.clone(), &headers, &Value::Null);
    if q.unwrap_or_default().contains("after_id=claude-b") {
        Json(json!({"data": [{"id": "claude-a"}], "has_more": false})).into_response()
    } else {
        Json(json!({"data": [{"id": "claude-b"}], "has_more": true, "last_id": "claude-b"})).into_response()
    }
}

async fn google_generate(
    State(m): State<Arc<Mock>>,
    Path(rest): Path<String>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    m.record(&format!("/google/v1beta/models/{rest}"), q.clone(), &headers, &body);
    if h(&headers, "x-goog-api-key") == "bad-key" {
        let err = json!({"error": {"code": 400, "message": "API key not valid. Please pass a valid API key.", "status": "INVALID_ARGUMENT"}});
        return (StatusCode::BAD_REQUEST, Json(err)).into_response();
    }
    let stream = rest.ends_with(":streamGenerateContent");
    assert_eq!(stream, q.as_deref() == Some("alt=sse"), "streaming uses alt=sse");
    let result = tool_result_text(&body);
    let wants_tool = body.get("tools").is_some() && result.is_none();
    let text = match &result {
        Some(r) => format!("The sum is {r}"),
        None => "Hello from google".to_string(),
    };
    let usage = json!({"promptTokenCount": 12, "candidatesTokenCount": 4, "totalTokenCount": 16});
    let call = json!({"functionCall": {"name": "add", "args": {"a": 2, "b": 3}}, "thoughtSignature": "sigG"});
    match (stream, wants_tool) {
        (false, true) => Json(json!({
            "candidates": [{"content": {"role": "model", "parts": [call]}, "finishReason": "STOP"}],
            "usageMetadata": usage, "modelVersion": "gemini-test-001"
        }))
        .into_response(),
        (false, false) => Json(json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": text}]}, "finishReason": "STOP"}],
            "usageMetadata": usage, "modelVersion": "gemini-test-001"
        }))
        .into_response(),
        (true, true) => sse(vec![(
            None,
            json!({"candidates": [{"content": {"role": "model", "parts": [call]}, "finishReason": "STOP"}], "usageMetadata": usage, "modelVersion": "gemini-test-001"}).to_string(),
        )]),
        (true, false) => {
            let (a, b) = text.split_at(5);
            sse(vec![
                (None, json!({"candidates": [{"content": {"role": "model", "parts": [{"text": a}]}}], "modelVersion": "gemini-test-001"}).to_string()),
                (None, json!({"candidates": [{"content": {"role": "model", "parts": [{"text": b}]}, "finishReason": "STOP"}], "usageMetadata": usage, "modelVersion": "gemini-test-001"}).to_string()),
            ])
        }
    }
}

async fn google_models(State(m): State<Arc<Mock>>, headers: HeaderMap, RawQuery(q): RawQuery) -> Response {
    m.record("/google/v1beta/models", q, &headers, &Value::Null);
    Json(json!({"models": [
        {"name": "models/gemini-x", "supportedGenerationMethods": ["generateContent", "countTokens"]},
        {"name": "models/text-embedding", "supportedGenerationMethods": ["embedContent"]}
    ]}))
    .into_response()
}

/// A misbehaving endpoint (`model` picks how): an endless SSE stream, an
/// endless NDJSON line, or a 65 MiB JSON body.
async fn flood(Json(body): Json<Value>) -> Response {
    const MIB: usize = 1024 * 1024;
    let pad = "x".repeat(64 * 1024);
    match body["model"].as_str().unwrap_or("") {
        "sse" => {
            let event = format!("data: {}\n\n", json!({"choices": [], "pad": pad})).into_bytes();
            let chunks = stream::repeat(event).map(Ok::<_, Infallible>);
            ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(chunks)).into_response()
        }
        "ndjson" => {
            let chunks = stream::repeat(pad.into_bytes()).map(Ok::<_, Infallible>);
            ([(header::CONTENT_TYPE, "application/x-ndjson")], Body::from_stream(chunks)).into_response()
        }
        _ => {
            let mut text = br#"{"choices": [{"index": 0, "message": {"role": "assistant", "content": ""#.to_vec();
            text.resize(text.len() + 65 * MIB, b'x');
            text.extend_from_slice(br#""}}]}"#);
            ([(header::CONTENT_TYPE, "application/json")], text).into_response()
        }
    }
}

async fn spawn_mock() -> (String, Arc<Mock>) {
    merpati_http::tls::install_provider();
    let mock = Arc::new(Mock::default());
    let app = Router::new()
        .route("/openai/v1/chat/completions", post(openai_chat))
        .route("/openai/v1/models", get(openai_models))
        .route("/anthropic/v1/messages", post(anthropic_messages))
        .route("/anthropic/v1/models", get(anthropic_models))
        .route("/google/v1beta/models", get(google_models))
        .route("/google/v1beta/models/{*rest}", post(google_generate))
        .route("/flood/v1/chat/completions", post(flood))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mock)
}

// ---------------------------------------------------------------------------
// Fake MCP server (Streamable HTTP, JSON responses) with an `add` tool
// ---------------------------------------------------------------------------

async fn mcp_post(body: String) -> Response {
    let msg: Value = serde_json::from_str(&body).unwrap();
    let Some(id) = msg.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => {
            json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "calc", "version": "1"}})
        }
        "tools/list" => json!({"tools": [{"name": "add", "description": "Add two integers",
            "inputSchema": {"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}}, "required": ["a", "b"]}}]}),
        "tools/call" => {
            let a = msg.pointer("/params/arguments/a").and_then(Value::as_i64).unwrap_or(0);
            let b = msg.pointer("/params/arguments/b").and_then(Value::as_i64).unwrap_or(0);
            json!({"content": [{"type": "text", "text": (a + b).to_string()}]})
        }
        _ => json!({}),
    };
    ([("mcp-session-id", "calc-1")], Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))).into_response()
}

async fn mcp_tools() -> Vec<(McpClient, Tool)> {
    let app = Router::new().route("/mcp", post(mcp_post));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg = McpConfig {
        transport: Transport::Http { url: format!("http://{addr}/mcp"), headers: vec![] },
        ..Default::default()
    };
    let client = McpClient::connect(cfg, |_| {}).await.unwrap();
    let tools = client.list_tools().await.unwrap();
    tools.into_iter().map(|t| (client.clone(), t)).collect()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn config(provider: AiProvider, base: &str, stream: bool) -> AiConfig {
    let (path, model) = match provider {
        AiProvider::OpenAi | AiProvider::OpenAiCompatible => ("/openai/v1", "gpt-test"),
        AiProvider::Anthropic => ("/anthropic", "claude-test"),
        AiProvider::Google => ("/google", "models/gemini-test"),
    };
    AiConfig {
        provider,
        base_url: format!("{base}{path}"),
        api_key: Some("sk-test".into()),
        model: model.into(),
        system: Some("Be brief.".into()),
        messages: vec![AiMessage { role: "user".into(), content: "What is 2 + 3?".into() }],
        stream,
        ..Default::default()
    }
}

type Events = Arc<Mutex<Vec<AiEvent>>>;

fn recorder() -> (Events, impl Fn(AiEvent) + Send + Sync + 'static) {
    let events: Events = Arc::default();
    let sink = events.clone();
    (events, move |e| sink.lock().push(e))
}

fn deltas(events: &Events) -> Vec<String> {
    events.lock().iter().filter_map(|e| if let AiEvent::Delta(d) = e { Some(d.clone()) } else { None }).collect()
}

async fn go(cfg: AiConfig, tools: Vec<(McpClient, Tool)>) -> (Result<merpati_ai::AiResponse, AiError>, Events) {
    let http = reqwest::Client::new();
    let (events, on_event) = recorder();
    let r = run(cfg, &http, tools, on_event, CancellationToken::new()).await;
    (r, events)
}

fn assert_tool_events(events: &Events) {
    let ev = events.lock();
    assert_eq!(ev.first(), Some(&AiEvent::Started));
    assert!(ev.contains(&AiEvent::ToolCall { name: "add".into(), arguments: json!({"a": 2, "b": 3}) }), "{ev:#?}");
    assert!(ev.contains(&AiEvent::ToolResult { name: "add".into(), result: "5".into() }), "{ev:#?}");
    assert!(matches!(ev.last(), Some(AiEvent::Done(_))));
}

// ---------------------------------------------------------------------------
// OpenAI
// ---------------------------------------------------------------------------

#[tokio::test]
async fn openai_non_streaming() {
    let (base, mock) = spawn_mock().await;
    let mut cfg = config(AiProvider::OpenAi, &base, false);
    cfg.temperature = Some(0.5);
    cfg.max_tokens = Some(100);
    let (r, events) = go(cfg, vec![]).await;
    let r = r.unwrap();
    assert_eq!(r.text, "Hello from openai");
    assert_eq!(r.model, "gpt-test-0001");
    assert_eq!(r.finish_reason.as_deref(), Some("stop"));
    assert_eq!((r.input_tokens, r.output_tokens), (Some(10), Some(5)));
    assert_eq!(r.first_token_ms, None);
    assert_eq!(r.raw["id"], "chatcmpl-2");
    assert_eq!(deltas(&events), ["Hello from openai"]);
    assert!(events.lock().contains(&AiEvent::Usage { input_tokens: 10, output_tokens: 5 }));

    let req = &mock.take()[0];
    assert_eq!(h(&req.headers, "authorization"), "Bearer sk-test");
    assert_eq!(req.body["model"], "gpt-test");
    assert_eq!(
        req.body["messages"],
        json!([{"role": "system", "content": "Be brief."}, {"role": "user", "content": "What is 2 + 3?"}])
    );
    assert_eq!(req.body["temperature"], 0.5);
    assert_eq!(req.body["max_completion_tokens"], 100);
    assert!(req.body.get("stream").is_none());
}

#[tokio::test]
async fn openai_streaming() {
    let (base, mock) = spawn_mock().await;
    let mut cfg = config(AiProvider::OpenAi, &base, true);
    cfg.extra = Some(json!({"seed": 7, "stream_options": null}));
    let (r, events) = go(cfg, vec![]).await;
    let r = r.unwrap();
    assert_eq!(deltas(&events), ["Hel", "lo from openai"]);
    assert_eq!(r.text, "Hello from openai");
    assert_eq!((r.input_tokens, r.output_tokens), (Some(10), Some(5)));
    assert!(r.first_token_ms.is_some_and(|t| t <= r.latency_ms));
    assert_eq!(r.raw["choices"][0]["message"]["content"], "Hello from openai");
    assert_eq!(r.raw["choices"][0]["finish_reason"], "stop");
    let req = &mock.take()[0];
    assert_eq!(req.body["stream"], true);
    assert_eq!(req.body["seed"], 7);
    assert!(req.body.get("stream_options").is_none(), "extra null removes a key");
}

#[tokio::test]
async fn openai_tool_round_streaming_and_not() {
    for stream in [true, false] {
        let (base, mock) = spawn_mock().await;
        let tools = mcp_tools().await;
        let (r, events) = go(config(AiProvider::OpenAi, &base, stream), tools).await;
        let r = r.unwrap();
        assert_eq!(r.text, "The sum is 5");
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert_eq!((r.input_tokens, r.output_tokens), (Some(30), Some(12)), "summed over both rounds");
        assert_tool_events(&events);

        let reqs = mock.take();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].body["tools"][0]["function"]["name"], "add");
        assert_eq!(reqs[0].body["tools"][0]["function"]["parameters"]["required"], json!(["a", "b"]));
        let msgs = reqs[1].body["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_abc");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"], "{\"a\":2,\"b\":3}");
        assert_eq!(msgs[3], json!({"role": "tool", "tool_call_id": "call_abc", "content": "5"}));
    }
}

#[tokio::test]
async fn openai_compatible_local_without_key() {
    let (base, mock) = spawn_mock().await;
    let mut cfg = config(AiProvider::OpenAiCompatible, &base, false);
    cfg.api_key = None;
    cfg.max_tokens = Some(64);
    let (r, _) = go(cfg, vec![]).await;
    assert_eq!(r.unwrap().text, "Hello from openai");
    let req = &mock.take()[0];
    assert!(req.headers.get("authorization").is_none());
    assert_eq!(req.body["max_tokens"], 64);
    assert!(req.body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn tool_loop_stops_after_max_rounds() {
    let (base, mock) = spawn_mock().await;
    let tools = mcp_tools().await;
    let mut cfg = config(AiProvider::OpenAi, &base, false);
    cfg.model = "always-tool".into();
    cfg.max_tool_rounds = 2;
    let (r, events) = go(cfg, tools).await;
    let r = r.unwrap();
    assert_eq!(r.finish_reason.as_deref(), Some("max_tool_rounds"));
    assert_eq!(mock.take().len(), 3);
    let calls = events.lock().iter().filter(|e| matches!(e, AiEvent::ToolCall { .. })).count();
    assert_eq!(calls, 2);
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anthropic_non_streaming() {
    let (base, mock) = spawn_mock().await;
    let mut cfg = config(AiProvider::Anthropic, &base, false);
    cfg.messages.insert(0, AiMessage { role: "system".into(), content: "Use metric units.".into() });
    let (r, events) = go(cfg, vec![]).await;
    let r = r.unwrap();
    assert_eq!(r.text, "Hello from anthropic");
    assert_eq!(r.model, "claude-test");
    assert_eq!(r.finish_reason.as_deref(), Some("end_turn"));
    assert_eq!((r.input_tokens, r.output_tokens), (Some(42), Some(6)), "cache reads count as input");
    assert_eq!(deltas(&events), ["Hello from anthropic"]);
    let req = &mock.take()[0];
    assert_eq!(h(&req.headers, "x-api-key"), "sk-test");
    assert!(req.headers.get("authorization").is_none());
    assert_eq!(req.body["system"], "Be brief.\n\nUse metric units.");
    assert_eq!(req.body["max_tokens"], merpati_ai::ANTHROPIC_DEFAULT_MAX_TOKENS);
    assert_eq!(req.body["messages"], json!([{"role": "user", "content": "What is 2 + 3?"}]));
    assert!(req.body.get("temperature").is_none());
}

#[tokio::test]
async fn anthropic_streaming() {
    let (base, _) = spawn_mock().await;
    let (r, events) = go(config(AiProvider::Anthropic, &base, true), vec![]).await;
    let r = r.unwrap();
    assert_eq!(deltas(&events), ["Hell", "o from anthropic"]);
    assert_eq!(r.text, "Hello from anthropic");
    assert_eq!((r.input_tokens, r.output_tokens), (Some(40), Some(6)));
    assert!(r.first_token_ms.is_some());
    assert_eq!(r.raw["content"], json!([{"type": "text", "text": "Hello from anthropic"}]));
    assert_eq!(r.raw["stop_reason"], "end_turn");
}

#[tokio::test]
async fn anthropic_tool_round_streaming_and_not() {
    for stream in [true, false] {
        let (base, mock) = spawn_mock().await;
        let tools = mcp_tools().await;
        let (r, events) = go(config(AiProvider::Anthropic, &base, stream), tools).await;
        let r = r.unwrap();
        assert_eq!(r.text, "Let me add.\n\nThe sum is 5");
        assert_eq!(deltas(&events).concat(), r.text);
        assert_eq!(r.finish_reason.as_deref(), Some("end_turn"));
        assert_tool_events(&events);

        let reqs = mock.take();
        assert_eq!(reqs[0].body["tools"][0]["input_schema"]["required"], json!(["a", "b"]));
        let msgs = reqs[1].body["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        let blocks = msgs[1]["content"].as_array().unwrap();
        let tool_use = blocks.iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tool_use["input"], json!({"a": 2, "b": 3}));
        if stream {
            // Thinking blocks go back unchanged, signature included.
            assert_eq!(blocks[0], json!({"type": "thinking", "thinking": "need add", "signature": "sig123"}));
        }
        assert_eq!(
            msgs[2],
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "5"}]})
        );
    }
}

#[tokio::test]
async fn anthropic_stream_error_event() {
    let (base, _) = spawn_mock().await;
    let mut cfg = config(AiProvider::Anthropic, &base, true);
    cfg.model = "overloaded".into();
    let (r, events) = go(cfg, vec![]).await;
    let err = r.unwrap_err();
    assert_eq!(err, AiError::Api("overloaded_error: Overloaded".into()));
    assert_eq!(events.lock().last(), Some(&AiEvent::Error("overloaded_error: Overloaded".into())));
}

// ---------------------------------------------------------------------------
// Google
// ---------------------------------------------------------------------------

#[tokio::test]
async fn google_non_streaming_and_streaming() {
    let (base, mock) = spawn_mock().await;
    let mut cfg = config(AiProvider::Google, &base, false);
    cfg.temperature = Some(0.2);
    cfg.max_tokens = Some(50);
    let (r, _) = go(cfg, vec![]).await;
    let r = r.unwrap();
    assert_eq!(r.text, "Hello from google");
    assert_eq!(r.model, "gemini-test-001");
    assert_eq!(r.finish_reason.as_deref(), Some("STOP"));
    assert_eq!((r.input_tokens, r.output_tokens), (Some(12), Some(4)));
    let req = &mock.take()[0];
    assert_eq!(req.path, "/google/v1beta/models/gemini-test:generateContent");
    assert_eq!(h(&req.headers, "x-goog-api-key"), "sk-test");
    assert_eq!(req.body["systemInstruction"], json!({"parts": [{"text": "Be brief."}]}));
    assert_eq!(req.body["contents"], json!([{"role": "user", "parts": [{"text": "What is 2 + 3?"}]}]));
    assert_eq!(req.body["generationConfig"], json!({"temperature": 0.2, "maxOutputTokens": 50}));

    let (r, events) = go(config(AiProvider::Google, &base, true), vec![]).await;
    let r = r.unwrap();
    assert_eq!(deltas(&events), ["Hello", " from google"]);
    assert_eq!(r.raw["candidates"][0]["content"]["parts"], json!([{"text": "Hello from google"}]));
    let req = &mock.take()[0];
    assert_eq!(req.path, "/google/v1beta/models/gemini-test:streamGenerateContent");
    assert_eq!(req.query, "alt=sse");
}

#[tokio::test]
async fn google_tool_round_streaming_and_not() {
    for stream in [true, false] {
        let (base, mock) = spawn_mock().await;
        let tools = mcp_tools().await;
        let (r, events) = go(config(AiProvider::Google, &base, stream), tools).await;
        assert_eq!(r.unwrap().text, "The sum is 5");
        assert_tool_events(&events);
        let reqs = mock.take();
        let decl = &reqs[0].body["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "add");
        assert_eq!(decl["parameters"]["properties"]["a"], json!({"type": "integer"}));
        let contents = reqs[1].body["contents"].as_array().unwrap();
        assert_eq!(
            contents[1],
            json!({"role": "model", "parts": [{"functionCall": {"name": "add", "args": {"a": 2, "b": 3}}, "thoughtSignature": "sigG"}]})
        );
        assert_eq!(
            contents[2],
            json!({"role": "user", "parts": [{"functionResponse": {"name": "add", "response": {"result": "5"}}}]})
        );
    }
}

// ---------------------------------------------------------------------------
// Errors, cancellation, model lists
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_errors_surface_their_message() {
    let (base, _) = spawn_mock().await;
    let cases = [
        (AiProvider::OpenAi, 401, "Incorrect API key provided: bad-key."),
        (AiProvider::Anthropic, 401, "invalid x-api-key"),
        (AiProvider::Google, 400, "API key not valid. Please pass a valid API key."),
    ];
    for (provider, status, message) in cases {
        for stream in [false, true] {
            let mut cfg = config(provider, &base, stream);
            cfg.api_key = Some("bad-key".into());
            let (r, events) = go(cfg, vec![]).await;
            match r.unwrap_err() {
                AiError::Provider { status: s, message: m, .. } => {
                    assert_eq!((s, m.as_str()), (status, message), "{provider:?}");
                }
                other => panic!("{provider:?}: {other:?}"),
            }
            let last = events.lock().last().cloned();
            assert_eq!(last, Some(AiEvent::Error(format!("HTTP {status}: {message}"))));
        }
    }
}

#[tokio::test]
async fn cancellation_stops_a_pending_request() {
    let (base, _) = spawn_mock().await;
    let mut cfg = config(AiProvider::OpenAi, &base, true);
    cfg.model = "slow".into();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });
    let http = reqwest::Client::new();
    let started = std::time::Instant::now();
    let r = run(cfg, &http, vec![], |_| {}, cancel).await;
    assert_eq!(r.unwrap_err(), AiError::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn timeouts_are_reported() {
    let (base, _) = spawn_mock().await;
    let mut cfg = config(AiProvider::OpenAi, &base, false);
    cfg.model = "slow".into();
    cfg.timeout = Duration::from_millis(200);
    let (r, _) = go(cfg, vec![]).await;
    assert!(matches!(r.unwrap_err(), AiError::Timeout(_)));
}

#[tokio::test]
async fn validation_errors_happen_before_any_request() {
    let (base, mock) = spawn_mock().await;
    let mut cfg = config(AiProvider::OpenAi, &base, false);
    cfg.messages.clear();
    let (r, _) = go(cfg, vec![]).await;
    assert!(matches!(r.unwrap_err(), AiError::Config(_)));
    assert!(mock.take().is_empty());
}

#[tokio::test]
async fn lists_models_for_each_provider() {
    let (base, mock) = spawn_mock().await;
    let http = reqwest::Client::new();
    let openai = list_models(AiProvider::OpenAi, &format!("{base}/openai/v1/"), Some("sk-test"), &http).await.unwrap();
    assert_eq!(openai, ["gpt-a", "gpt-b"]);
    let anthropic =
        list_models(AiProvider::Anthropic, &format!("{base}/anthropic"), Some("sk-ant"), &http).await.unwrap();
    assert_eq!(anthropic, ["claude-a", "claude-b"], "follows has_more pagination");
    let google = list_models(AiProvider::Google, &format!("{base}/google"), Some("g-key"), &http).await.unwrap();
    assert_eq!(google, ["gemini-x"], "only generateContent models");

    let reqs = mock.take();
    assert_eq!(h(&reqs[0].headers, "authorization"), "Bearer sk-test");
    assert_eq!(h(&reqs[1].headers, "x-api-key"), "sk-ant");
    assert_eq!(h(&reqs[1].headers, "anthropic-version"), "2023-06-01");
    assert_eq!(h(&reqs[3].headers, "x-goog-api-key"), "g-key");
}

#[tokio::test]
async fn model_list_page_tokens_are_url_encoded() {
    // Google page tokens are base64 and may hold `+`, `/` and `=`.
    const TOKEN: &str = "Ch1t+b2/RlbHM=&x=1";
    async fn page(RawQuery(q): RawQuery) -> Response {
        let url = reqwest::Url::parse(&format!("http://x/?{}", q.unwrap_or_default())).unwrap();
        let token = url.query_pairs().find(|(k, _)| k == "pageToken").map(|(_, v)| v.into_owned());
        match token.as_deref() {
            None => Json(json!({"models": [{"name": "models/a"}], "nextPageToken": TOKEN})).into_response(),
            Some(TOKEN) => Json(json!({"models": [{"name": "models/b"}]})).into_response(),
            Some(other) => {
                let err = json!({"error": {"message": format!("Invalid page token {other:?}")}});
                (StatusCode::BAD_REQUEST, Json(err)).into_response()
            }
        }
    }
    merpati_http::tls::install_provider();
    let app = Router::new().route("/v1beta/models", get(page));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::new();
    let models = list_models(AiProvider::Google, &format!("http://{addr}"), Some("k"), &http).await.unwrap();
    assert_eq!(models, ["a", "b"]);
}

#[tokio::test]
async fn anthropic_default_max_tokens_follows_the_model() {
    let (base, mock) = spawn_mock().await;
    for (model, want) in [
        ("claude-3-haiku-20240307", 4096),
        ("claude-3-opus-latest", 4096),
        ("claude-3-5-sonnet-20240620", 4096),
        ("claude-3-5-sonnet-20241022", 8192),
        ("claude-3-5-haiku-latest", 8192),
        ("claude-3-7-sonnet-latest", merpati_ai::ANTHROPIC_DEFAULT_MAX_TOKENS),
        ("claude-sonnet-4-5", merpati_ai::ANTHROPIC_DEFAULT_MAX_TOKENS),
        ("claude-opus-5-5", merpati_ai::ANTHROPIC_DEFAULT_MAX_TOKENS),
    ] {
        for stream in [false, true] {
            let mut cfg = config(AiProvider::Anthropic, &base, stream);
            cfg.model = model.into();
            go(cfg, vec![]).await.0.unwrap();
            assert_eq!(mock.take()[0].body["max_tokens"], want, "{model}");
        }
    }
    // A value set on the request is sent as is.
    let mut cfg = config(AiProvider::Anthropic, &base, false);
    (cfg.model, cfg.max_tokens) = ("claude-3-haiku-20240307".into(), Some(20000));
    go(cfg, vec![]).await.0.unwrap();
    assert_eq!(mock.take()[0].body["max_tokens"], 20000);
}

#[tokio::test]
async fn endless_or_huge_responses_are_cut_off() {
    let (base, _) = spawn_mock().await;
    for (model, stream) in [("sse", true), ("ndjson", true), ("json", false)] {
        let mut cfg = config(AiProvider::OpenAiCompatible, &base, stream);
        cfg.base_url = format!("{base}/flood/v1");
        cfg.model = model.into();
        let started = std::time::Instant::now();
        let err = go(cfg, vec![]).await.0.unwrap_err();
        assert!(matches!(&err, AiError::Parse(m) if m.contains("larger than 64 MiB")), "{model}: {err:?}");
        assert!(started.elapsed() < Duration::from_secs(20), "{model}: {:?}", started.elapsed());
    }
}
