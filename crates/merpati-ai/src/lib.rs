//! AI model requests for Merpati (FR-X9): OpenAI Chat Completions (and any
//! OpenAI-compatible endpoint such as Ollama, LM Studio or vLLM), the
//! Anthropic Messages API and Google Gemini `generateContent`, with the
//! user's own API key, optional streaming, and MCP servers attached as tools.

mod anthropic;
mod google;
mod openai;

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use merpati_core::auth::AuthType;
use merpati_core::protocols::{AiMessage, AiProvider, AiRequest};
use merpati_mcp::sse::{SseEvent, SseParser};
use merpati_mcp::{McpClient, Tool};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue, USER_AGENT};
use serde_json::{Map, Value};

pub use merpati_mcp;
pub use tokio_util::sync::CancellationToken;

/// Default overall timeout for one model call (also the idle timeout between
/// streamed chunks).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Tool-call rounds per run before giving up.
pub const DEFAULT_MAX_TOOL_ROUNDS: u32 = 5;
/// `max_tokens` sent to Anthropic (where it is required) when none is set,
/// for models that accept it; see [`anthropic_default_max_tokens`].
pub const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 16000;
/// Largest provider response read (a JSON body, or all of a streamed round),
/// so a misbehaving server cannot exhaust memory.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// What is kept of an error response body.
const MAX_ERROR_BODY: usize = 1024 * 1024;

/// The `max_tokens` sent to Anthropic when the request sets none. Older
/// models reject values above their output limit: Claude 3 (Haiku, Sonnet,
/// Opus) and Claude 2/Instant allow 4096 tokens, Claude 3.5 Sonnet/Haiku
/// 8192 (the first 3.5 Sonnet, `20240620`, 4096 without a beta header).
/// Claude 3.7 and every later model allow at least 32000, so they get
/// [`ANTHROPIC_DEFAULT_MAX_TOKENS`]. Matches ids with a prefix or suffix
/// too (`anthropic.claude-3-haiku-20240307-v1:0`, `claude-3-5-sonnet-v2@20241022`).
pub fn anthropic_default_max_tokens(model: &str) -> u32 {
    let m = model.to_ascii_lowercase().replace('.', "-");
    if m.contains("claude-3-5") {
        if m.contains("20240620") { 4096 } else { 8192 }
    } else if m.contains("claude-3-7") {
        ANTHROPIC_DEFAULT_MAX_TOKENS
    } else if m.contains("claude-3-") || m.contains("claude-2") || m.contains("claude-instant") {
        4096
    } else {
        ANTHROPIC_DEFAULT_MAX_TOKENS
    }
}

#[derive(Clone, PartialEq)]
pub struct AiConfig {
    pub provider: AiProvider,
    /// Without trailing slash, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<AiMessage>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    /// JSON object deep-merged into the provider request body (a `null`
    /// value removes the key).
    pub extra: Option<Value>,
    pub timeout: Duration,
    /// Model → tool → model rounds allowed in one [`run`].
    pub max_tool_rounds: u32,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            provider: AiProvider::OpenAi,
            base_url: default_base_url(AiProvider::OpenAi).to_string(),
            api_key: None,
            model: String::new(),
            system: None,
            messages: Vec::new(),
            temperature: None,
            max_tokens: None,
            stream: true,
            extra: None,
            timeout: DEFAULT_TIMEOUT,
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        }
    }
}

impl fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiConfig")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("system", &self.system)
            .field("messages", &self.messages)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("stream", &self.stream)
            .field("extra", &self.extra)
            .field("timeout", &self.timeout)
            .field("max_tool_rounds", &self.max_tool_rounds)
            .finish()
    }
}

fn needs_key(p: AiProvider) -> bool {
    !matches!(p, AiProvider::OpenAiCompatible)
}

impl AiConfig {
    /// Resolves variables in the base URL, model, system prompt, messages and
    /// extra JSON. The API key comes from `req.auth` (Bearer token or API key
    /// value), already resolved by the caller.
    pub fn from_request(req: &AiRequest, resolve: &dyn Fn(&str) -> String) -> Result<AiConfig, AiError> {
        let base = resolve(req.base_url.trim()).trim().trim_end_matches('/').to_string();
        let base_url = if base.is_empty() { default_base_url(req.provider).to_string() } else { base };
        let api_key = match req.auth.kind {
            AuthType::Bearer => Some(req.auth.get("token")),
            AuthType::ApiKey => Some(req.auth.get("value")),
            _ => None,
        }
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);
        let model = resolve(req.model.trim()).trim().to_string();
        if model.is_empty() {
            return Err(AiError::Config("Choose a model first".into()));
        }
        let system = Some(resolve(&req.system)).filter(|s| !s.trim().is_empty());
        let messages = req
            .messages
            .iter()
            .map(|m| AiMessage { role: m.role.trim().to_ascii_lowercase(), content: resolve(&m.content) })
            .collect();
        let extra = {
            let text = resolve(req.extra.trim());
            if text.trim().is_empty() {
                None
            } else {
                let v: Value = serde_json::from_str(&text)
                    .map_err(|e| AiError::Config(format!("Extra body is not valid JSON: {e}")))?;
                if !v.is_object() {
                    return Err(AiError::Config("Extra body must be a JSON object".into()));
                }
                Some(v)
            }
        };
        if api_key.is_none() && needs_key(req.provider) && base_url == default_base_url(req.provider) {
            return Err(AiError::Config(format!(
                "{} needs an API key: set a Bearer token (for example {{{{vault:{}}}}}) in the Auth tab",
                req.provider.label(),
                match req.provider {
                    AiProvider::Anthropic => "anthropic",
                    AiProvider::Google => "google",
                    _ => "openai",
                }
            )));
        }
        Ok(AiConfig {
            provider: req.provider,
            base_url,
            api_key,
            model,
            system,
            messages,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            stream: req.stream,
            extra,
            timeout: DEFAULT_TIMEOUT,
            max_tool_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        })
    }
}

/// Progress of a [`run`], delivered in order on the calling task.
#[derive(Clone, Debug, PartialEq)]
pub enum AiEvent {
    Started,
    /// Assistant text. Streamed in pieces; in non-streaming mode each round's
    /// text arrives as one delta. Rounds are separated by a `"\n\n"` delta.
    Delta(String),
    /// The model asked for a tool (original MCP tool name).
    ToolCall {
        name: String,
        arguments: Value,
    },
    /// What was fed back to the model.
    ToolResult {
        name: String,
        result: String,
    },
    /// Cumulative token usage so far.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Done(AiResponse),
    Error(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AiResponse {
    /// All assistant text of the run (rounds joined by a blank line).
    pub text: String,
    /// Model reported by the provider (falls back to the requested one).
    pub model: String,
    /// As sent by the provider (`stop`, `end_turn`, `STOP`, `tool_calls`, ...),
    /// or `max_tool_rounds` when the tool loop limit was hit.
    pub finish_reason: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub latency_ms: f64,
    /// Time to the first streamed text (streaming only).
    pub first_token_ms: Option<f64>,
    /// The last provider response; for streams, reassembled in the shape of
    /// the provider's non-streaming response.
    pub raw: Value,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum AiError {
    #[error("{0}")]
    Config(String),
    /// Could not reach the provider.
    #[error("{0}")]
    Network(String),
    /// Non-success HTTP status; `message` is the provider's own explanation.
    #[error("HTTP {status}: {message}")]
    Provider { status: u16, message: String, body: String },
    /// An error reported inside a successful response or stream
    /// (stream `error` events, blocked prompts, ...).
    #[error("{0}")]
    Api(String),
    #[error("Invalid response: {0}")]
    Parse(String),
    #[error("No response within {} s", .0.as_secs())]
    Timeout(Duration),
    #[error("Cancelled")]
    Cancelled,
}

/// Default endpoint per provider.
pub fn default_base_url(p: AiProvider) -> &'static str {
    match p {
        AiProvider::OpenAi => "https://api.openai.com/v1",
        AiProvider::Anthropic => "https://api.anthropic.com",
        AiProvider::Google => "https://generativelanguage.googleapis.com",
        AiProvider::OpenAiCompatible => "http://localhost:11434/v1",
    }
}

/// Model names offered as suggestions in the UI before [`list_models`] ran.
/// Not used anywhere else; any model name is accepted.
pub fn suggested_models(p: AiProvider) -> &'static [&'static str] {
    match p {
        AiProvider::OpenAi => &["gpt-5", "gpt-5-mini", "gpt-4.1-mini"],
        AiProvider::Anthropic => &["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
        AiProvider::Google => &["gemini-2.5-flash", "gemini-2.5-pro"],
        AiProvider::OpenAiCompatible => &["llama3.2", "qwen2.5", "mistral"],
    }
}

// ---------------------------------------------------------------------------
// Provider plumbing
// ---------------------------------------------------------------------------

pub(crate) struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

pub(crate) struct ToolCallReq {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    /// Set when the model produced arguments that are not a JSON object.
    pub invalid: Option<String>,
}

#[derive(Default)]
pub(crate) struct Round {
    pub text: String,
    pub tool_calls: Vec<ToolCallReq>,
    pub finish_reason: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub raw: Value,
    /// The assistant turn in the provider's own message format.
    pub assistant: Value,
}

pub(crate) struct ToolOutcome {
    pub call_id: String,
    pub name: String,
    pub text: String,
    pub is_error: bool,
}

pub(crate) struct HttpReq {
    pub url: String,
    /// (name, value, sensitive)
    pub headers: Vec<(&'static str, String, bool)>,
    pub body: Value,
}

pub(crate) trait Provider: Send + Sync {
    fn messages(&self, cfg: &AiConfig) -> Vec<Value>;
    fn build(&self, cfg: &AiConfig, messages: &[Value], tools: &[ToolSpec], stream: bool) -> HttpReq;
    fn parse(&self, body: Value) -> Result<Round, AiError>;
    fn stream(&self) -> Box<dyn StreamAcc>;
    fn tool_results(&self, outcomes: &[ToolOutcome]) -> Vec<Value>;
}

pub(crate) trait StreamAcc: Send {
    fn event(&mut self, ev: &SseEvent, delta: &mut dyn FnMut(&str)) -> Result<(), AiError>;
    fn finish(self: Box<Self>) -> Result<Round, AiError>;
}

fn provider_for(p: AiProvider) -> Box<dyn Provider> {
    match p {
        AiProvider::OpenAi => Box::new(openai::OpenAi { compat: false }),
        AiProvider::OpenAiCompatible => Box::new(openai::OpenAi { compat: true }),
        AiProvider::Anthropic => Box::new(anthropic::Anthropic),
        AiProvider::Google => Box::new(google::Google),
    }
}

/// Deep merge; `null` in `extra` removes the key.
pub(crate) fn merge_json(target: &mut Value, extra: &Value) {
    if let (Value::Object(t), Value::Object(e)) = (target, extra) {
        for (k, v) in e {
            if v.is_null() {
                t.remove(k);
            } else if let (Some(tv), true) = (t.get_mut(k), v.is_object()) {
                if tv.is_object() {
                    merge_json(tv, v);
                } else {
                    *tv = v.clone();
                }
            } else {
                t.insert(k.clone(), v.clone());
            }
        }
    }
}

/// `0.2f32` as the JSON number `0.2` (not `0.20000000298023224`).
pub(crate) fn num_f32(v: f32) -> Value {
    serde_json::from_str(&v.to_string()).unwrap_or_else(|_| Value::from(f64::from(v)))
}

pub(crate) fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Parses model-produced tool arguments (a JSON string or an object).
pub(crate) fn parse_arguments(v: &Value) -> (Value, Option<String>) {
    match v {
        Value::Object(_) => (v.clone(), None),
        Value::Null => (Value::Object(Map::new()), None),
        Value::String(s) if s.trim().is_empty() => (Value::Object(Map::new()), None),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(obj @ Value::Object(_)) => (obj, None),
            Ok(other) => (other, Some("tool arguments must be a JSON object".into())),
            Err(e) => (Value::String(s.clone()), Some(format!("tool arguments are not valid JSON: {e}"))),
        },
        other => (other.clone(), Some("tool arguments must be a JSON object".into())),
    }
}

/// Pulls a human readable message out of a provider error body.
pub(crate) fn error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body.trim()) {
        let v = match &v {
            Value::Array(a) => a.first().cloned().unwrap_or(Value::Null),
            _ => v,
        };
        for ptr in ["/error/message", "/message", "/detail", "/error", "/error/type"] {
            if let Some(s) = v.pointer(ptr).and_then(Value::as_str)
                && !s.trim().is_empty()
            {
                return s.trim().to_string();
            }
        }
    }
    let t = body.trim();
    if t.is_empty() {
        return "(empty response body)".into();
    }
    if t.chars().count() > 500 { format!("{}...", t.chars().take(500).collect::<String>()) } else { t.to_string() }
}

fn sanitize_tool_name(name: &str) -> String {
    let mut s: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if !s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        s.insert(0, '_');
    }
    s.chars().take(64).collect()
}

/// Provider-safe, unique tool names mapped back to the MCP client and tool.
fn prepare_tools(tools: &[(McpClient, Tool)]) -> (Vec<ToolSpec>, HashMap<String, (McpClient, Tool)>) {
    let mut specs = Vec::new();
    let mut table = HashMap::new();
    for (client, tool) in tools {
        let base = sanitize_tool_name(&tool.name);
        let mut name = base.clone();
        let mut n = 2;
        while table.contains_key(&name) {
            let suffix = format!("_{n}");
            name = format!("{}{suffix}", base.chars().take(64 - suffix.len()).collect::<String>());
            n += 1;
        }
        let mut schema = tool.input_schema.clone();
        if !schema.is_object() {
            schema = serde_json::json!({"type": "object", "properties": {}});
        }
        if schema.get("type").is_none() {
            schema["type"] = Value::String("object".into());
        }
        specs.push(ToolSpec {
            name: name.clone(),
            description: tool.description.clone().or_else(|| tool.title.clone()).unwrap_or_default(),
            schema,
        });
        table.insert(name, (client.clone(), tool.clone()));
    }
    (specs, table)
}

fn network_error(e: &reqwest::Error, url: &str) -> AiError {
    let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    AiError::Network(merpati_http::error::classify(e, &host, None).message)
}

fn request_builder(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    headers: &[(&'static str, String, bool)],
) -> Result<reqwest::RequestBuilder, AiError> {
    let mut rb = http.request(method, url).header(USER_AGENT, concat!("merpati/", env!("CARGO_PKG_VERSION")));
    for (name, value, sensitive) in headers {
        let mut v = HeaderValue::from_str(value)
            .map_err(|_| AiError::Config(format!("The value for header {name} contains invalid characters")))?;
        v.set_sensitive(*sensitive);
        rb = rb.header(*name, v);
    }
    Ok(rb)
}

/// The body as text, or `None` when it is longer than `max` bytes (reading
/// stops there).
async fn body_within(mut resp: reqwest::Response, max: usize) -> reqwest::Result<Option<String>> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

/// At most the first `max` bytes of the body as text; what arrived before a
/// network error.
async fn body_prefix(mut resp: reqwest::Response, max: usize) -> String {
    let mut body = Vec::new();
    while body.len() < max
        && let Ok(Some(chunk)) = resp.chunk().await
    {
        let room = max - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    String::from_utf8_lossy(&body).into_owned()
}

fn too_large() -> AiError {
    AiError::Parse(format!("the response is larger than {} MiB", MAX_RESPONSE_BYTES / (1024 * 1024)))
}

async fn cancellable<T>(
    cancel: &CancellationToken,
    limit: Duration,
    fut: impl Future<Output = T>,
) -> Result<T, AiError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(AiError::Cancelled),
        r = tokio::time::timeout(limit, fut) => r.map_err(|_| AiError::Timeout(limit)),
    }
}

struct RoundCtx<'a> {
    on_event: &'a (dyn Fn(AiEvent) + Send + Sync),
    start: Instant,
    first_token_ms: &'a mut Option<f64>,
    /// Text exists from earlier rounds; the first delta gets a separator.
    needs_separator: bool,
}

async fn send_round(
    cfg: &AiConfig,
    http: &reqwest::Client,
    provider: &dyn Provider,
    req: HttpReq,
    cancel: &CancellationToken,
    ctx: RoundCtx<'_>,
) -> Result<Round, AiError> {
    let RoundCtx { on_event, start, first_token_ms, needs_separator } = ctx;
    let mut rb = request_builder(http, reqwest::Method::POST, &req.url, &req.headers)?.json(&req.body);
    if cfg.stream {
        rb = rb.header(ACCEPT, "text/event-stream");
    }
    let resp = cancellable(cancel, cfg.timeout, rb.send()).await?.map_err(|e| network_error(&e, &req.url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = cancellable(cancel, cfg.timeout, body_prefix(resp, MAX_ERROR_BODY)).await?;
        return Err(AiError::Provider { status: status.as_u16(), message: error_message(&body), body });
    }
    let content_type =
        resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let mut first_in_round = true;
    let stream = cfg.stream;
    let mut delta = |t: &str| {
        if t.is_empty() {
            return;
        }
        if stream && first_token_ms.is_none() {
            *first_token_ms = Some(start.elapsed().as_secs_f64() * 1000.0);
        }
        if first_in_round && needs_separator {
            on_event(AiEvent::Delta("\n\n".into()));
        }
        first_in_round = false;
        on_event(AiEvent::Delta(t.to_string()));
    };
    let is_sse = content_type.starts_with("text/event-stream");
    let is_ndjson = content_type.contains("ndjson");
    if cfg.stream && (is_sse || is_ndjson) {
        let mut acc = provider.stream();
        let mut parser = SseParser::new();
        let mut line_buf: Vec<u8> = Vec::new();
        let mut body = resp.bytes_stream();
        // The round is reassembled in memory: a stream that never ends is cut.
        let mut received = 0;
        let sse_error = |e: merpati_mcp::sse::SseTooLarge| AiError::Parse(e.to_string());
        loop {
            let chunk = match cancellable(cancel, cfg.timeout, body.next()).await? {
                None => break,
                Some(Err(e)) => return Err(network_error(&e, &req.url)),
                Some(Ok(b)) => b,
            };
            received += chunk.len();
            if received > MAX_RESPONSE_BYTES {
                return Err(too_large());
            }
            if is_sse {
                for ev in parser.feed(&chunk).map_err(sse_error)? {
                    acc.event(&ev, &mut delta)?;
                }
            } else {
                // Only the new bytes are searched: a long line stays linear.
                let mut from = line_buf.len();
                line_buf.extend_from_slice(&chunk);
                while let Some(pos) = line_buf[from..].iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = line_buf.drain(..=from + pos).collect();
                    from = 0;
                    let data = String::from_utf8_lossy(&line).trim().to_string();
                    if !data.is_empty() {
                        acc.event(&SseEvent { event: "message".into(), data, ..Default::default() }, &mut delta)?;
                    }
                }
            }
        }
        if let Some(ev) = parser.finish().map_err(sse_error)? {
            acc.event(&ev, &mut delta)?;
        }
        let rest = String::from_utf8_lossy(&line_buf).trim().to_string();
        if !rest.is_empty() {
            acc.event(&SseEvent { event: "message".into(), data: rest, ..Default::default() }, &mut delta)?;
        }
        return acc.finish();
    }
    let body = cancellable(cancel, cfg.timeout, body_within(resp, MAX_RESPONSE_BYTES))
        .await?
        .map_err(|e| network_error(&e, &req.url))?
        .ok_or_else(too_large)?;
    let value: Value = serde_json::from_str(body.trim()).map_err(|e| {
        AiError::Parse(format!("the response is not JSON ({e}): {}", body.chars().take(300).collect::<String>()))
    })?;
    let round = provider.parse(value)?;
    delta(&round.text);
    Ok(round)
}

/// Runs the request. With `tools` (from attached MCP servers) it loops: the
/// model asks for a tool, the tool is called through its [`McpClient`], the
/// result is fed back, up to `cfg.max_tool_rounds` times. Emits
/// [`AiEvent::Started`] first and [`AiEvent::Done`] or [`AiEvent::Error`] last.
pub async fn run(
    cfg: AiConfig,
    http: &reqwest::Client,
    tools: Vec<(McpClient, Tool)>,
    on_event: impl Fn(AiEvent) + Send + Sync,
    cancel: CancellationToken,
) -> Result<AiResponse, AiError> {
    on_event(AiEvent::Started);
    let result = run_inner(&cfg, http, &tools, &on_event, &cancel).await;
    match &result {
        Ok(r) => on_event(AiEvent::Done(r.clone())),
        Err(e) => on_event(AiEvent::Error(e.to_string())),
    }
    result
}

async fn run_inner(
    cfg: &AiConfig,
    http: &reqwest::Client,
    tools: &[(McpClient, Tool)],
    on_event: &(dyn Fn(AiEvent) + Send + Sync),
    cancel: &CancellationToken,
) -> Result<AiResponse, AiError> {
    let start = Instant::now();
    if cfg.model.trim().is_empty() {
        return Err(AiError::Config("Choose a model first".into()));
    }
    if !cfg.messages.iter().any(|m| !m.role.eq_ignore_ascii_case("system") && !m.content.trim().is_empty()) {
        return Err(AiError::Config("Add a message to send".into()));
    }
    let provider = provider_for(cfg.provider);
    let (specs, table) = prepare_tools(tools);
    let mut messages = provider.messages(cfg);
    let mut text = String::new();
    let mut first_token_ms = None;
    let (mut input_total, mut output_total) = (None::<u64>, None::<u64>);
    let mut rounds = 0;
    loop {
        let req = provider.build(cfg, &messages, &specs, cfg.stream);
        let ctx = RoundCtx { on_event, start, first_token_ms: &mut first_token_ms, needs_separator: !text.is_empty() };
        let round = send_round(cfg, http, provider.as_ref(), req, cancel, ctx).await?;
        if !round.text.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&round.text);
        }
        if round.input_tokens.is_some() || round.output_tokens.is_some() {
            input_total = Some(input_total.unwrap_or(0) + round.input_tokens.unwrap_or(0));
            output_total = Some(output_total.unwrap_or(0) + round.output_tokens.unwrap_or(0));
            on_event(AiEvent::Usage {
                input_tokens: input_total.unwrap_or(0),
                output_tokens: output_total.unwrap_or(0),
            });
        }
        let done = round.tool_calls.is_empty() || specs.is_empty();
        if done || rounds >= cfg.max_tool_rounds {
            let finish_reason = if done { round.finish_reason } else { Some("max_tool_rounds".into()) };
            return Ok(AiResponse {
                text,
                model: round.model.filter(|m| !m.is_empty()).unwrap_or_else(|| cfg.model.clone()),
                finish_reason,
                input_tokens: input_total,
                output_tokens: output_total,
                latency_ms: start.elapsed().as_secs_f64() * 1000.0,
                first_token_ms,
                raw: round.raw,
            });
        }
        rounds += 1;
        messages.push(round.assistant);
        let mut outcomes = Vec::with_capacity(round.tool_calls.len());
        for call in &round.tool_calls {
            let entry = table.get(&call.name);
            let display = entry.map(|(_, t)| t.name.clone()).unwrap_or_else(|| call.name.clone());
            on_event(AiEvent::ToolCall { name: display.clone(), arguments: call.arguments.clone() });
            let (result, is_error) = match (entry, &call.invalid) {
                (_, Some(problem)) => (format!("Error: {problem}"), true),
                (None, None) => (format!("Error: unknown tool \"{}\"", call.name), true),
                (Some((client, tool)), None) => {
                    let r = tokio::select! {
                        _ = cancel.cancelled() => return Err(AiError::Cancelled),
                        r = client.call_tool(&tool.name, call.arguments.clone()) => r,
                    };
                    match r {
                        Ok(res) => (res.text(), res.is_error),
                        Err(e) => (format!("Error: {e}"), true),
                    }
                }
            };
            on_event(AiEvent::ToolResult { name: display, result: result.clone() });
            outcomes.push(ToolOutcome { call_id: call.id.clone(), name: call.name.clone(), text: result, is_error });
        }
        messages.extend(provider.tool_results(&outcomes));
    }
}

/// Percent-encodes a query parameter value (page tokens are base64 and may
/// hold `+`, `/`, `=`).
fn query_value(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Model ids available to this key (sorted). `base_url` empty = default.
pub async fn list_models(
    provider: AiProvider,
    base_url: &str,
    api_key: Option<&str>,
    http: &reqwest::Client,
) -> Result<Vec<String>, AiError> {
    let base = base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() { default_base_url(provider) } else { base };
    let key = api_key.map(str::trim).filter(|k| !k.is_empty());
    let limit = Duration::from_secs(30);
    let mut models = Vec::new();
    let mut page_token: Option<String> = None;
    for _ in 0..20 {
        let (url, headers, items_key) = match provider {
            AiProvider::OpenAi | AiProvider::OpenAiCompatible => {
                (format!("{base}/models"), openai::auth_headers(key), "data")
            }
            AiProvider::Anthropic => {
                let mut url = format!("{}?limit=1000", anthropic::endpoint(base, "/models"));
                if let Some(t) = &page_token {
                    url.push_str(&format!("&after_id={}", query_value(t)));
                }
                (url, anthropic::auth_headers(key), "data")
            }
            AiProvider::Google => {
                let mut url = format!("{}/models?pageSize=1000", google::versioned(base));
                if let Some(t) = &page_token {
                    url.push_str(&format!("&pageToken={}", query_value(t)));
                }
                (url, google::auth_headers(key), "models")
            }
        };
        let rb = request_builder(http, reqwest::Method::GET, &url, &headers)?;
        let resp = tokio::time::timeout(limit, rb.send())
            .await
            .map_err(|_| AiError::Timeout(limit))?
            .map_err(|e| network_error(&e, &url))?;
        let status = resp.status();
        let body = tokio::time::timeout(limit, body_within(resp, MAX_RESPONSE_BYTES))
            .await
            .map_err(|_| AiError::Timeout(limit))?
            .map_err(|e| network_error(&e, &url))?
            .ok_or_else(too_large)?;
        if !status.is_success() {
            return Err(AiError::Provider { status: status.as_u16(), message: error_message(&body), body });
        }
        let v: Value =
            serde_json::from_str(&body).map_err(|e| AiError::Parse(format!("model list is not JSON: {e}")))?;
        for item in v.get(items_key).and_then(Value::as_array).into_iter().flatten() {
            if provider == AiProvider::Google {
                let methods = item.get("supportedGenerationMethods").and_then(Value::as_array);
                if methods.is_some_and(|m| !m.iter().any(|x| x.as_str() == Some("generateContent"))) {
                    continue;
                }
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    models.push(name.trim_start_matches("models/").to_string());
                }
            } else if let Some(id) = item.get("id").and_then(Value::as_str) {
                models.push(id.to_string());
            }
        }
        page_token = match provider {
            AiProvider::Anthropic if v.get("has_more").and_then(Value::as_bool) == Some(true) => str_of(&v, "last_id"),
            AiProvider::Google => str_of(&v, "nextPageToken").filter(|t| !t.is_empty()),
            _ => None,
        };
        if page_token.is_none() {
            break;
        }
    }
    models.sort();
    models.dedup();
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::auth::Auth;
    use serde_json::json;

    #[test]
    fn anthropic_max_tokens_by_model_family() {
        for (model, want) in [
            ("claude-3-haiku-20240307", 4096),
            ("anthropic.claude-3-haiku-20240307-v1:0", 4096),
            ("claude-3-sonnet-20240229", 4096),
            ("claude-3-opus@20240229", 4096),
            ("claude-2.1", 4096),
            ("claude-instant-1.2", 4096),
            ("claude-3-5-sonnet-20240620", 4096),
            ("us.anthropic.claude-3-5-sonnet-20241022-v2:0", 8192),
            ("claude-3-5-sonnet-v2@20241022", 8192),
            ("anthropic/claude-3.5-haiku", 8192),
            ("claude-3-7-sonnet-20250219", ANTHROPIC_DEFAULT_MAX_TOKENS),
            ("claude-opus-4-0", ANTHROPIC_DEFAULT_MAX_TOKENS),
            ("claude-haiku-4-5", ANTHROPIC_DEFAULT_MAX_TOKENS),
            ("claude-opus-5-5", ANTHROPIC_DEFAULT_MAX_TOKENS),
            ("my-proxy-model", ANTHROPIC_DEFAULT_MAX_TOKENS),
        ] {
            assert_eq!(anthropic_default_max_tokens(model), want, "{model}");
        }
    }

    #[test]
    fn from_request_defaults_and_validation() {
        let req = AiRequest {
            provider: AiProvider::Anthropic,
            model: "{{model}}".into(),
            system: "Be {{tone}}".into(),
            messages: vec![AiMessage { role: "User".into(), content: "Hi {{name}}".into() }],
            extra: r#"{"top_k": 5}"#.into(),
            auth: Auth::bearer("sk-ant-test"),
            ..Default::default()
        };
        let resolve =
            |s: &str| s.replace("{{model}}", "claude-x").replace("{{tone}}", "brief").replace("{{name}}", "Ana");
        let cfg = AiConfig::from_request(&req, &resolve).unwrap();
        assert_eq!(cfg.base_url, "https://api.anthropic.com");
        assert_eq!(cfg.model, "claude-x");
        assert_eq!(cfg.system.as_deref(), Some("Be brief"));
        assert_eq!(cfg.messages[0], AiMessage { role: "user".into(), content: "Hi Ana".into() });
        assert_eq!(cfg.api_key.as_deref(), Some("sk-ant-test"));
        assert_eq!(cfg.extra, Some(json!({"top_k": 5})));
        assert!(!format!("{cfg:?}").contains("sk-ant-test"));

        let no_key = AiRequest { auth: Auth::default(), ..req.clone() };
        let err = AiConfig::from_request(&no_key, &resolve).unwrap_err();
        assert!(err.to_string().contains("{{vault:anthropic}}"), "{err}");

        let local = AiRequest { provider: AiProvider::OpenAiCompatible, auth: Auth::default(), ..req.clone() };
        assert_eq!(AiConfig::from_request(&local, &resolve).unwrap().base_url, "http://localhost:11434/v1");

        let bad_extra = AiRequest { extra: "[1]".into(), ..req.clone() };
        assert!(matches!(AiConfig::from_request(&bad_extra, &resolve), Err(AiError::Config(_))));
        let no_model = AiRequest { model: " ".into(), ..req };
        assert!(matches!(AiConfig::from_request(&no_model, &resolve), Err(AiError::Config(_))));
    }

    #[test]
    fn merge_and_errors() {
        let mut body = json!({"a": {"b": 1, "c": 2}, "stream_options": {"include_usage": true}});
        merge_json(&mut body, &json!({"a": {"c": 3, "d": 4}, "stream_options": null, "e": [1]}));
        assert_eq!(body, json!({"a": {"b": 1, "c": 3, "d": 4}, "e": [1]}));
        assert_eq!(error_message(r#"{"error": {"message": "Incorrect API key"}}"#), "Incorrect API key");
        assert_eq!(
            error_message(r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#),
            "invalid x-api-key"
        );
        assert_eq!(error_message(r#"[{"error": {"code": 400, "message": "API key not valid"}}]"#), "API key not valid");
        assert_eq!(error_message(r#"{"error": "model 'x' not found"}"#), "model 'x' not found");
        assert_eq!(error_message("plain"), "plain");
    }

    #[test]
    fn tool_names_are_sanitized_and_unique() {
        assert_eq!(sanitize_tool_name("files.read/v2"), "files_read_v2");
        assert_eq!(sanitize_tool_name("2fa"), "_2fa");
        assert_eq!(sanitize_tool_name(&"x".repeat(80)).len(), 64);
    }

    #[test]
    fn run_future_is_send() {
        fn is_send<T: Send>(_: &T) {}
        merpati_http::tls::install_provider();
        let http = reqwest::Client::new();
        let fut = run(AiConfig::default(), &http, vec![], |_| {}, CancellationToken::new());
        is_send(&fut);
    }
}
