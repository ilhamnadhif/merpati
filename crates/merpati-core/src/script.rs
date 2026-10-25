//! The contract between the request pipeline and a JavaScript engine.
//!
//! The pipeline builds a [`ScriptJob`] for one event of one request (all
//! levels' scripts, collection first), the engine runs it in an isolated
//! sandbox and returns a [`ScriptOutcome`]. Engines are swappable behind the
//! [`ScriptEngine`] trait (PRD section 8).
//!
//! Scripts of non-HTTP requests (FR-S7) use the same job with a
//! [`ProtocolContext`]; [`protocol`] builds it from the request models.
//! Packages for `pm.require` (npm, JSR, the local Package Library) are
//! described by a [`PackageContext`]. See `docs/scripting.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::{Body, KeyValue};
use crate::response::ResponseBody;
use crate::variables::VariableScopes;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScriptEvent {
    /// Runs before the request is sent (Postman `prerequest`).
    PreRequest,
    /// Runs after the response arrived (Postman `test`).
    PostResponse,
    /// Runs for each message of a streaming protocol (`pm.message`).
    OnMessage,
}

impl ScriptEvent {
    /// Postman's `pm.info.eventName`.
    pub fn postman_name(self) -> &'static str {
        match self {
            ScriptEvent::PreRequest => "prerequest",
            ScriptEvent::PostResponse => "test",
            ScriptEvent::OnMessage => "message",
        }
    }

    /// Phase label for consoles and reports: `pre-request`, `post-response`, `on-message`.
    pub fn label(self) -> &'static str {
        match self {
            ScriptEvent::PreRequest => "pre-request",
            ScriptEvent::PostResponse => "post-response",
            ScriptEvent::OnMessage => "on-message",
        }
    }
}

/// One script to run, with where it came from for error messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptSource {
    /// Human readable location: `collection "API"`, `folder "Users"`, `request "List"`.
    pub location: String,
    pub code: String,
}

/// The request as seen by `pm.request`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptRequest {
    pub id: Option<String>,
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    pub headers: Vec<KeyValue>,
    pub body: Body,
}

/// The response as seen by `pm.response`.
#[derive(Clone, Debug, Default)]
pub struct ScriptResponse {
    pub code: u16,
    /// Reason phrase (`OK`, `Not Found`).
    pub status: String,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
    pub response_time_ms: f64,
    pub response_size: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// Unix seconds, if not a session cookie.
    pub expires: Option<i64>,
}

/// Metadata exposed as `pm.info`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptInfo {
    pub request_name: String,
    pub request_id: String,
    /// 0-based iteration index.
    pub iteration: usize,
    pub iteration_count: usize,
    pub collection_name: String,
}

/// Performs `pm.sendRequest` calls. Implementations block the calling thread
/// until the response arrives. Variables in the request are resolved against
/// `scopes` (the script's current view) before sending.
pub trait ScriptHttp: Send + Sync {
    fn send(&self, request: ScriptRequest, scopes: &VariableScopes) -> Result<ScriptResponse, String>;
}

/// Non-HTTP protocols with scripts (FR-S7). GraphQL runs as an HTTP job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolKind {
    Grpc,
    WebSocket,
    SocketIo,
    Mqtt,
    Mcp,
    Ai,
}

impl ProtocolKind {
    /// `pm.info.protocol`.
    pub fn name(self) -> &'static str {
        match self {
            ProtocolKind::Grpc => "grpc",
            ProtocolKind::WebSocket => "websocket",
            ProtocolKind::SocketIo => "socketio",
            ProtocolKind::Mqtt => "mqtt",
            ProtocolKind::Mcp => "mcp",
            ProtocolKind::Ai => "ai",
        }
    }
}

/// Protocol data of a script job for a non-HTTP request. The JSON shapes
/// per protocol are built by [`protocol`] and documented in
/// `docs/scripting.md`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProtocolContext {
    pub kind: ProtocolKind,
    /// `pm.request`. Pre-request scripts may change it; the result is
    /// returned in [`ScriptOutcome::protocol_request`].
    pub request: serde_json::Value,
    /// `pm.response`: the result in post-response scripts (gRPC response,
    /// MCP or AI result, connection summary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<serde_json::Value>,
    /// `pm.message`: the message an on-message script runs for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<serde_json::Value>,
}

impl ProtocolContext {
    pub fn new(kind: ProtocolKind, request: serde_json::Value) -> Self {
        ProtocolContext { kind, request, response: None, message: None }
    }

    pub fn with_response(mut self, response: serde_json::Value) -> Self {
        self.response = Some(response);
        self
    }

    pub fn with_message(mut self, message: serde_json::Value) -> Self {
        self.message = Some(message);
        self
    }
}

/// Default CDN for `npm:` and `jsr:` packages (any esm.sh-compatible server works).
pub const DEFAULT_PACKAGE_REGISTRY: &str = "https://esm.sh";

/// Where `pm.require` finds packages besides the bundled libraries (PRD
/// section 8). Without one (the default) only built-in modules load.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PackageContext {
    /// Local Package Library, normally `<workspace>/packages`.
    pub library_dir: Option<PathBuf>,
    /// Cache of fetched modules (per user), so later runs work offline.
    pub cache_dir: Option<PathBuf>,
    /// Integrity pins, normally `<workspace>/packages.lock.json`. Modules
    /// fetched for the first time are added to it.
    pub lockfile_path: Option<PathBuf>,
    /// esm.sh-compatible CDN base URL; modules are only fetched from its host.
    pub registry_base: String,
    /// Fetch modules missing from the cache; `false` means cache only.
    pub allow_network: bool,
}

impl Default for PackageContext {
    fn default() -> Self {
        PackageContext {
            library_dir: None,
            cache_dir: None,
            lockfile_path: None,
            registry_base: DEFAULT_PACKAGE_REGISTRY.to_string(),
            allow_network: true,
        }
    }
}

impl PackageContext {
    /// The usual layout: `packages/` and `packages.lock.json` in the
    /// workspace, the module cache in the user's config directory.
    /// `registry_base` is normally `AppSettings::package_registry`; empty
    /// means [`DEFAULT_PACKAGE_REGISTRY`].
    pub fn for_workspace(workspace: &Path, registry_base: &str) -> Self {
        let registry = registry_base.trim();
        PackageContext {
            library_dir: Some(workspace.join("packages")),
            cache_dir: Some(Self::default_cache_dir()),
            lockfile_path: Some(workspace.join("packages.lock.json")),
            registry_base: if registry.is_empty() {
                DEFAULT_PACKAGE_REGISTRY.to_string()
            } else {
                registry.to_string()
            },
            allow_network: true,
        }
    }

    /// `<config dir>/package-cache`.
    pub fn default_cache_dir() -> PathBuf {
        crate::settings::AppSettings::config_dir().join("package-cache")
    }
}

pub struct ScriptJob {
    pub event: ScriptEvent,
    /// Scripts in execution order: collection, folders (outer to inner), request.
    pub scripts: Vec<ScriptSource>,
    pub info: ScriptInfo,
    pub request: ScriptRequest,
    /// Present for post-response scripts.
    pub response: Option<ScriptResponse>,
    pub variables: VariableScopes,
    /// Cookies applicable to the request URL (`pm.cookies`).
    pub cookies: Vec<ScriptCookie>,
    pub http: Option<Arc<dyn ScriptHttp>>,
    /// Wall-clock budget for the whole job.
    pub timeout: Duration,
    /// Set by the caller to abort the job early.
    pub cancel: Arc<AtomicBool>,
    /// Set for non-HTTP requests: `pm.request`, `pm.response` and
    /// `pm.message` then come from here instead of `request`/`response`.
    pub protocol: Option<ProtocolContext>,
    /// Enables npm/JSR packages and the local Package Library.
    pub packages: Option<PackageContext>,
}

impl ScriptJob {
    pub fn new(event: ScriptEvent, scripts: Vec<ScriptSource>, request: ScriptRequest) -> Self {
        ScriptJob {
            event,
            scripts,
            info: ScriptInfo::default(),
            request,
            response: None,
            variables: VariableScopes::default(),
            cookies: Vec::new(),
            http: None,
            timeout: Duration::from_secs(30),
            cancel: Arc::new(AtomicBool::new(false)),
            protocol: None,
            packages: None,
        }
    }

    /// A job for a non-HTTP request; `info.request_name`/`request_id`
    /// should still be filled in by the caller.
    pub fn for_protocol(event: ScriptEvent, scripts: Vec<ScriptSource>, protocol: ProtocolContext) -> Self {
        let mut job = ScriptJob::new(event, scripts, ScriptRequest::default());
        job.protocol = Some(protocol);
        job
    }

    pub fn has_code(&self) -> bool {
        self.scripts.iter().any(|s| !s.code.trim().is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    /// Assertion message when failed.
    pub error: Option<String>,
    /// `pm.test.skip(...)`.
    #[serde(default)]
    pub skipped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Log,
    Info,
    Warn,
    Error,
    Debug,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsoleEntry {
    pub level: LogLevel,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptError {
    pub location: String,
    pub message: String,
}

/// Flow control requested by `pm.execution.setNextRequest(...)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NextRequest {
    /// `setNextRequest(null)`: stop the run after this request.
    Stop,
    /// Jump to the request with this name or id.
    Named(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Visualization {
    pub template: String,
    pub data: serde_json::Value,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptOutcome {
    /// The (possibly modified) request. Only meaningful for pre-request.
    pub request: ScriptRequest,
    /// Variable scopes after the scripts ran.
    pub variables: VariableScopes,
    pub tests: Vec<TestResult>,
    pub console: Vec<ConsoleEntry>,
    /// Uncaught errors, one per failing script.
    pub errors: Vec<ScriptError>,
    pub next_request: Option<NextRequest>,
    /// `pm.execution.skipRequest()` was called in a pre-request script.
    pub skip_request: bool,
    pub visualization: Option<Visualization>,
    /// The (possibly modified) `pm.request` of a protocol job; equal to the
    /// input when no script changed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_request: Option<serde_json::Value>,
}

pub trait ScriptEngine: Send + Sync {
    /// Engine name for diagnostics (`boa`, `quickjs`).
    fn name(&self) -> &'static str;

    /// Runs all scripts of the job in order and blocks until done. Must never
    /// panic on bad user code: syntax and runtime errors go to
    /// `ScriptOutcome::errors`, and a failing script does not prevent the
    /// following scripts from running.
    fn run(&self, job: ScriptJob) -> ScriptOutcome;
}

/// Engine that does nothing; used when scripting is disabled.
pub struct NoopEngine;

impl ScriptEngine for NoopEngine {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn run(&self, job: ScriptJob) -> ScriptOutcome {
        ScriptOutcome {
            request: job.request,
            variables: job.variables,
            protocol_request: job.protocol.map(|p| p.request),
            ..Default::default()
        }
    }
}

/// Builds [`ProtocolContext`] JSON from the request models and applies
/// `pm.request` changes made by pre-request scripts back to them. The
/// shapes are documented in `docs/scripting.md`; hosts may add fields.
pub mod protocol {
    use serde::{Deserialize, Serialize};
    use serde_json::{Map, Value, json};

    use super::{ProtocolContext, ProtocolKind};
    use crate::auth::Auth;
    use crate::model::{KeyValue, WsMessageKind, WsRequest, WsSavedMessage};
    use crate::protocols::{AiMessage, AiRequest, GrpcRequest, McpRequest, McpSavedCall, McpTransport, MqttMessage};
    use crate::protocols::{MqttRequest, SocketIoMessage, SocketIoRequest};

    /// Whether a message was received or sent.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum Direction {
        Incoming,
        Outgoing,
    }

    /// `[{key, value, enabled?, description?}]`, the form `pm.request`
    /// header/metadata lists are read from and written back in.
    pub fn kv_json(list: &[KeyValue]) -> Value {
        serde_json::to_value(list).unwrap_or_else(|_| Value::Array(Vec::new()))
    }

    /// `[{key, value}]` for response metadata, trailers and headers.
    pub fn pairs_json(list: &[(String, String)]) -> Value {
        Value::Array(list.iter().map(|(k, v)| json!({"key": k, "value": v})).collect())
    }

    /// Parses a list written by a script; `None` if it is not a list.
    pub fn kv_from_json(v: &Value) -> Option<Vec<KeyValue>> {
        v.is_array().then(|| serde_json::from_value(v.clone()).ok()).flatten()
    }

    /// Message payloads as scripts see them: parsed JSON when the text is
    /// JSON, otherwise the text itself.
    pub fn json_or_text(s: &str) -> Value {
        serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
    }

    /// Inverse of [`json_or_text`]: strings stay as they are.
    pub fn payload_text(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        }
    }

    /// One message: `{data, direction, timestamp}` (`timestamp` in Unix
    /// milliseconds). Protocol fields (`topic`, `event`, `type`, ...) can
    /// be added to the returned object.
    pub fn message(data: Value, direction: Direction, timestamp_ms: i64) -> Value {
        json!({"data": data, "direction": direction, "timestamp": timestamp_ms})
    }

    fn obj(v: &Value) -> Option<&Map<String, Value>> {
        v.as_object()
    }

    fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
        v.get(key).and_then(Value::as_str)
    }

    fn set_str(target: &mut String, v: &Value, key: &str) {
        if let Some(s) = str_field(v, key)
            && s != target
        {
            *target = s.to_string();
        }
    }

    fn set_kv(target: &mut Vec<KeyValue>, v: &Value, key: &str) {
        if let Some(list) = v.get(key).and_then(kv_from_json)
            && &list != target
        {
            *target = list;
        }
    }

    fn set_auth(target: &mut Auth, v: &Value) {
        if let Some(a) = v.get("auth").and_then(|a| serde_json::from_value::<Auth>(a.clone()).ok())
            && &a != target
        {
            *target = a;
        }
    }

    fn auth_json(a: &Auth) -> Value {
        serde_json::to_value(a).unwrap_or(Value::Null)
    }

    // -- gRPC ---------------------------------------------------------------

    /// `{url, service, method, methodPath, metadata, messages: [{data}], deadlineMs?, auth}`.
    pub fn grpc(r: &GrpcRequest) -> ProtocolContext {
        let mut v = json!({
            "url": r.url,
            "service": r.service,
            "method": r.method,
            "methodPath": format!("{}/{}", r.service, r.method),
            "metadata": kv_json(&r.metadata),
            "messages": if r.message.trim().is_empty() { json!([]) } else { json!([{"data": json_or_text(&r.message)}]) },
            "auth": auth_json(&r.auth),
        });
        if let Some(ms) = r.deadline_ms {
            v["deadlineMs"] = json!(ms);
        }
        ProtocolContext::new(ProtocolKind::Grpc, v)
    }

    pub fn apply_grpc(r: &mut GrpcRequest, v: &Value) {
        if obj(v).is_none() {
            return;
        }
        set_str(&mut r.url, v, "url");
        let path = format!("{}/{}", r.service, r.method);
        match str_field(v, "methodPath").and_then(|p| p.rsplit_once('/')) {
            Some((svc, m)) if format!("{svc}/{m}") != path => {
                r.service = svc.trim_start_matches('/').to_string();
                r.method = m.to_string();
            }
            _ => {
                set_str(&mut r.service, v, "service");
                set_str(&mut r.method, v, "method");
            }
        }
        set_kv(&mut r.metadata, v, "metadata");
        if let Some(data) = v.get("messages").and_then(|m| m.get(0)).and_then(|m| m.get("data"))
            && *data != json_or_text(&r.message)
        {
            r.message = payload_text(data);
        }
        if let Some(ms) = v.get("deadlineMs").and_then(Value::as_u64) {
            r.deadline_ms = Some(ms);
        }
        set_auth(&mut r.auth, v);
    }

    /// `{statusCode, statusMessage, metadata, trailers, messages, responseTime}`;
    /// `messages` are [`message`] objects (or bare `{data}`).
    pub fn grpc_response(
        status_code: i32,
        status_message: &str,
        metadata: &[(String, String)],
        trailers: &[(String, String)],
        messages: Vec<Value>,
        response_time_ms: f64,
    ) -> Value {
        json!({
            "statusCode": status_code,
            "statusMessage": status_message,
            "metadata": pairs_json(metadata),
            "trailers": pairs_json(trailers),
            "messages": messages,
            "responseTime": response_time_ms,
        })
    }

    // -- WebSocket ----------------------------------------------------------

    /// `{url, headers, subprotocols}`; set `message` with [`ws_message`]
    /// when running a pre-request script for an outgoing message.
    pub fn websocket(r: &WsRequest) -> ProtocolContext {
        let v = json!({
            "url": r.url,
            "headers": kv_json(&r.headers),
            "subprotocols": r.subprotocols,
        });
        ProtocolContext::new(ProtocolKind::WebSocket, v)
    }

    /// `{data, type}` for `pm.request.message` (`type` is `text` or `binary`).
    pub fn ws_message(m: &WsSavedMessage) -> Value {
        let kind = match m.kind {
            WsMessageKind::Text => "text",
            WsMessageKind::Binary => "binary",
        };
        json!({"data": m.body, "type": kind})
    }

    pub fn apply_websocket(r: &mut WsRequest, v: &Value) {
        if obj(v).is_none() {
            return;
        }
        set_str(&mut r.url, v, "url");
        set_kv(&mut r.headers, v, "headers");
        if let Some(list) = v.get("subprotocols").and_then(|s| serde_json::from_value::<Vec<String>>(s.clone()).ok())
            && list != r.subprotocols
        {
            r.subprotocols = list;
        }
    }

    // -- Socket.IO ----------------------------------------------------------

    /// `{url, path, namespace, headers, params, auth, listeners}`; `auth`
    /// is the parsed handshake payload. Outgoing: [`socketio_message`].
    pub fn socketio(r: &SocketIoRequest) -> ProtocolContext {
        let v = json!({
            "url": r.url,
            "path": r.path,
            "namespace": r.namespace,
            "headers": kv_json(&r.headers),
            "params": kv_json(&r.params),
            "auth": if r.auth_payload.trim().is_empty() { Value::Null } else { json_or_text(&r.auth_payload) },
            "listeners": r.listeners.iter().filter(|l| l.enabled).map(|l| l.event.clone()).collect::<Vec<_>>(),
        });
        ProtocolContext::new(ProtocolKind::SocketIo, v)
    }

    /// `{event, args, ack}`: JSON arguments parsed (kept as text when not
    /// JSON), text as strings, binary data as base64 strings (like received
    /// attachments) and files as their path. Older messages: `args` parsed
    /// from the saved JSON array.
    pub fn socketio_message(m: &SocketIoMessage) -> Value {
        use crate::protocols::{SocketIoArg, SocketIoArgKind};
        let args = if m.arguments.is_empty() {
            match json_or_text(&m.args) {
                Value::Array(a) => Value::Array(a),
                Value::String(s) if s.trim().is_empty() => json!([]),
                other => json!([other]),
            }
        } else {
            let b64 = |b: Vec<u8>| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b);
            Value::Array(
                m.arguments
                    .iter()
                    .map(|a| match a.kind {
                        SocketIoArgKind::Json => json_or_text(a.value.trim()),
                        SocketIoArgKind::Binary => Value::String(
                            SocketIoArg::decode_binary(&a.value).map(b64).unwrap_or_else(|| a.value.clone()),
                        ),
                        SocketIoArgKind::Text | SocketIoArgKind::File => Value::String(a.value.clone()),
                    })
                    .collect(),
            )
        };
        json!({"event": m.event, "args": args, "ack": m.ack})
    }

    pub fn apply_socketio(r: &mut SocketIoRequest, v: &Value) {
        if obj(v).is_none() {
            return;
        }
        set_str(&mut r.url, v, "url");
        set_str(&mut r.path, v, "path");
        set_str(&mut r.namespace, v, "namespace");
        set_kv(&mut r.headers, v, "headers");
        set_kv(&mut r.params, v, "params");
        if let Some(a) = v.get("auth") {
            let before = if r.auth_payload.trim().is_empty() { Value::Null } else { json_or_text(&r.auth_payload) };
            if *a != before {
                r.auth_payload = if a.is_null() { String::new() } else { payload_text(a) };
            }
        }
    }

    // -- MQTT ---------------------------------------------------------------

    /// `{url, clientId, version, keepAlive, cleanSession, subscriptions: [{topic, qos}]}`.
    /// Outgoing publishes: [`mqtt_message`].
    pub fn mqtt(r: &MqttRequest) -> ProtocolContext {
        let v = json!({
            "url": r.url,
            "clientId": r.client_id,
            "version": r.version,
            "keepAlive": r.keep_alive_secs,
            "cleanSession": r.clean_session,
            "subscriptions": r.subscriptions.iter().filter(|s| s.enabled).map(|s| json!({"topic": s.topic, "qos": s.qos})).collect::<Vec<_>>(),
        });
        ProtocolContext::new(ProtocolKind::Mqtt, v)
    }

    /// `{topic, payload, qos, retain}`.
    pub fn mqtt_message(m: &MqttMessage) -> Value {
        json!({"topic": m.topic, "payload": m.payload, "qos": m.qos, "retain": m.retain})
    }

    pub fn apply_mqtt(r: &mut MqttRequest, v: &Value) {
        if obj(v).is_none() {
            return;
        }
        set_str(&mut r.url, v, "url");
        set_str(&mut r.client_id, v, "clientId");
        if let Some(k) = v.get("keepAlive").and_then(Value::as_u64) {
            r.keep_alive_secs = k.min(u64::from(u16::MAX)) as u16;
        }
        if let Some(c) = v.get("cleanSession").and_then(Value::as_bool) {
            r.clean_session = c;
        }
    }

    /// Applies a script-modified `pm.request.message` to an outgoing publish.
    pub fn apply_mqtt_message(m: &mut MqttMessage, v: &Value) {
        set_str(&mut m.topic, v, "topic");
        if let Some(p) = v.get("payload")
            && payload_text(p) != m.payload
        {
            m.payload = payload_text(p);
        }
        if let Some(q) = v.get("qos").and_then(Value::as_u64) {
            m.qos = q.min(2) as u8;
        }
        if let Some(b) = v.get("retain").and_then(Value::as_bool) {
            m.retain = b;
        }
    }

    // -- MCP ----------------------------------------------------------------

    /// `{transport, url, headers | command, args, env, cwd}` plus, for a
    /// call, `{kind, name, arguments}` (`kind` is `tool`, `prompt` or
    /// `resource`; `name` is the tool/prompt name or resource URI).
    pub fn mcp(r: &McpRequest, call: Option<&McpSavedCall>) -> ProtocolContext {
        let mut v = match &r.transport {
            McpTransport::Http { url, headers } => {
                json!({"transport": "http", "url": url, "headers": kv_json(headers)})
            }
            McpTransport::Stdio { command, args, env, cwd } => json!({
                "transport": "stdio", "url": "", "command": command, "args": args, "env": kv_json(env), "cwd": cwd,
            }),
        };
        if let Some(c) = call {
            v["kind"] = serde_json::to_value(c.kind).unwrap_or(Value::Null);
            v["name"] = json!(c.target);
            v["arguments"] = if c.arguments.trim().is_empty() { json!({}) } else { json_or_text(&c.arguments) };
        }
        ProtocolContext::new(ProtocolKind::Mcp, v)
    }

    pub fn apply_mcp(r: &mut McpRequest, call: Option<&mut McpSavedCall>, v: &Value) {
        if obj(v).is_none() {
            return;
        }
        match &mut r.transport {
            McpTransport::Http { url, headers } => {
                set_str(url, v, "url");
                set_kv(headers, v, "headers");
            }
            McpTransport::Stdio { command, args, env, cwd } => {
                set_str(command, v, "command");
                if let Some(list) = v.get("args").and_then(|a| serde_json::from_value::<Vec<String>>(a.clone()).ok()) {
                    *args = list;
                }
                set_kv(env, v, "env");
                set_str(cwd, v, "cwd");
            }
        }
        if let Some(c) = call {
            set_str(&mut c.target, v, "name");
            if let Some(a) = v.get("arguments") {
                let before = if c.arguments.trim().is_empty() { json!({}) } else { json_or_text(&c.arguments) };
                if *a != before {
                    c.arguments = payload_text(a);
                }
            }
        }
    }

    /// `{result, isError, responseTime}`; `result` is the MCP result object
    /// (`{content: [...], structuredContent?}` for tools).
    pub fn mcp_response(result: Value, is_error: bool, response_time_ms: f64) -> Value {
        json!({"result": result, "isError": is_error, "responseTime": response_time_ms})
    }

    // -- AI -----------------------------------------------------------------

    /// `{provider, url, model, system, messages: [{role, content}], temperature, maxTokens, stream, extra}`;
    /// `extra` is the parsed extra-body JSON (`{}` when empty).
    pub fn ai(r: &AiRequest) -> ProtocolContext {
        let v = json!({
            "provider": r.provider,
            "url": r.base_url,
            "model": r.model,
            "system": r.system,
            "messages": r.messages,
            "temperature": r.temperature,
            "maxTokens": r.max_tokens,
            "stream": r.stream,
            "extra": if r.extra.trim().is_empty() { json!({}) } else { json_or_text(&r.extra) },
        });
        ProtocolContext::new(ProtocolKind::Ai, v)
    }

    pub fn apply_ai(r: &mut AiRequest, v: &Value) {
        if obj(v).is_none() {
            return;
        }
        if let Some(p) = v.get("provider").and_then(|p| serde_json::from_value(p.clone()).ok()) {
            r.provider = p;
        }
        set_str(&mut r.base_url, v, "url");
        set_str(&mut r.model, v, "model");
        set_str(&mut r.system, v, "system");
        if let Some(m) = v.get("messages").and_then(|m| serde_json::from_value::<Vec<AiMessage>>(m.clone()).ok())
            && m != r.messages
        {
            r.messages = m;
        }
        if let Some(t) = v.get("temperature") {
            r.temperature = t.as_f64().map(|t| t as f32);
        }
        if let Some(t) = v.get("maxTokens") {
            r.max_tokens = t.as_u64().map(|t| t.min(u64::from(u32::MAX)) as u32);
        }
        if let Some(s) = v.get("stream").and_then(Value::as_bool) {
            r.stream = s;
        }
        if let Some(e) = v.get("extra") {
            let before = if r.extra.trim().is_empty() { json!({}) } else { json_or_text(&r.extra) };
            if *e != before {
                r.extra = if e.as_object().is_some_and(Map::is_empty) { String::new() } else { payload_text(e) };
            }
        }
    }

    /// `{content, model, finishReason, usage, statusCode, responseTime}`;
    /// `content` is the assistant text, `usage` e.g. `{inputTokens, outputTokens}`.
    pub fn ai_response(
        content: &str,
        model: &str,
        finish_reason: &str,
        usage: Value,
        status_code: u16,
        response_time_ms: f64,
    ) -> Value {
        json!({
            "content": content,
            "model": model,
            "finishReason": finish_reason,
            "usage": usage,
            "statusCode": status_code,
            "responseTime": response_time_ms,
        })
    }
}
