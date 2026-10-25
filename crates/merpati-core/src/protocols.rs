//! Request models for the non-HTTP protocols (PRD 7.1): gRPC, MQTT,
//! Socket.IO, MCP and AI model requests.
//!
//! Credentials always live in the request's `auth` field so that the secret
//! handling of `storage` (keeping literal secrets out of committed files) and
//! `{{vault:...}}` references work the same as for HTTP.

use serde::{Deserialize, Serialize};

use crate::auth::Auth;
use crate::model::{Id, KeyValue, WsSavedMessage};
use crate::util::{default_true, is_default, is_true, new_id};

// ---------------------------------------------------------------------------
// gRPC (FR-X2)
// ---------------------------------------------------------------------------

/// Where the service definitions come from.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GrpcSchemaSource {
    /// Ask the server via gRPC server reflection (v1, falling back to v1alpha).
    #[default]
    Reflection,
    /// Compile local `.proto` files (paths may be relative to the workspace).
    ProtoFiles {
        #[serde(default)]
        files: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        import_paths: Vec<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GrpcRequest {
    /// `localhost:50051`, `grpc://host:port` (plaintext) or `grpcs://host:port` (TLS).
    #[serde(default)]
    pub url: String,
    /// Fully qualified service name, e.g. `helloworld.Greeter`.
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub schema: GrpcSchemaSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<KeyValue>,
    /// Message being composed, as JSON (protobuf JSON mapping).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<WsSavedMessage>,
    /// Force TLS on/off; `None` decides by scheme (`grpcs://` = TLS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_ssl: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
}

// ---------------------------------------------------------------------------
// Interactive sessions in the Collection Runner (WebSocket, Socket.IO, MQTT)
// ---------------------------------------------------------------------------

fn default_wait_ms() -> u64 {
    1000
}

fn default_session_timeout_ms() -> u64 {
    30_000
}

/// When the Collection Runner and `merpati run` end a WebSocket, Socket.IO
/// or MQTT session. The runner connects, sends the saved messages in order
/// (MQTT subscribes first; Socket.IO waits for each requested ack), then
/// collects incoming messages until the first of: `max_messages` incoming
/// messages, a message matching `until_event`, `wait_ms` without an
/// incoming message, `timeout_ms` since connecting, or the server closing
/// the connection. Stored as `runner` in the request JSON, omitted while
/// every field has its default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRunner {
    /// Silence that ends the session once every saved message was sent;
    /// each incoming message restarts it. `0` = never stop for silence.
    /// Default 1000 ms.
    #[serde(default = "default_wait_ms")]
    pub wait_ms: u64,
    /// Stop after this many incoming messages (Socket.IO acks are not
    /// counted); `None` = no limit. `Some(0)` stops right after sending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_messages: Option<u32>,
    /// Stop when a matching message arrives: the event name (Socket.IO),
    /// a topic filter with `+`/`#` (MQTT), or the message text or its JSON
    /// `type`/`event` field (WebSocket). May contain `{{variables}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_event: Option<String>,
    /// Upper bound for the whole session, connecting included. `0` = no
    /// limit. Default 30000 ms.
    #[serde(default = "default_session_timeout_ms")]
    pub timeout_ms: u64,
    /// Socket.IO: arguments that answer every acknowledgement the server
    /// asks for (a JSON array, or one JSON value or text). `None` leaves
    /// them unanswered. May contain `{{variables}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_reply: Option<String>,
}

impl Default for SessionRunner {
    fn default() -> Self {
        SessionRunner {
            wait_ms: default_wait_ms(),
            max_messages: None,
            until_event: None,
            timeout_ms: default_session_timeout_ms(),
            ack_reply: None,
        }
    }
}

impl SessionRunner {
    pub fn is_default(&self) -> bool {
        *self == SessionRunner::default()
    }

    /// `until_event` unless blank.
    pub fn until(&self) -> Option<&str> {
        self.until_event.as_deref().filter(|s| !s.trim().is_empty())
    }
}

// ---------------------------------------------------------------------------
// MQTT (FR-X5)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MqttVersion {
    #[default]
    V311,
    V5,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MqttSubscription {
    pub topic: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub qos: u8,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MqttMessage {
    #[serde(default = "new_id")]
    pub id: Id,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub topic: String,
    #[serde(default)]
    pub payload: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub qos: u8,
    #[serde(default, skip_serializing_if = "is_default")]
    pub retain: bool,
    /// MQTT 5 user properties, in order (a name may repeat).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_properties: Vec<KeyValue>,
}

impl Default for MqttMessage {
    fn default() -> Self {
        MqttMessage {
            id: new_id(),
            name: String::new(),
            topic: String::new(),
            payload: String::new(),
            qos: 0,
            retain: false,
            user_properties: Vec::new(),
        }
    }
}

fn default_keep_alive() -> u16 {
    60
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MqttRequest {
    /// `mqtt://host:1883`, `mqtts://host:8883`, `ws://host:8080/mqtt` or `wss://...`.
    #[serde(default)]
    pub url: String,
    /// Empty = generated.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_id: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub version: MqttVersion,
    #[serde(default = "default_keep_alive")]
    pub keep_alive_secs: u16,
    #[serde(default = "default_true")]
    pub clean_session: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<MqttSubscription>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<MqttMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_will: Option<MqttMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_ssl: Option<bool>,
    /// Username/password as Basic auth.
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
    /// Stop condition in the Collection Runner.
    #[serde(default, skip_serializing_if = "SessionRunner::is_default")]
    pub runner: SessionRunner,
}

impl Default for MqttRequest {
    fn default() -> Self {
        MqttRequest {
            url: String::new(),
            client_id: String::new(),
            version: MqttVersion::V311,
            keep_alive_secs: 60,
            clean_session: true,
            subscriptions: Vec::new(),
            messages: Vec::new(),
            last_will: None,
            verify_ssl: None,
            auth: Auth::default(),
            runner: SessionRunner::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Socket.IO (FR-X4)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SocketIoListener {
    pub event: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

/// How the text of a [`SocketIoArg`] is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SocketIoArgKind {
    /// A JSON value: `"hello"`, `42`, `{"a": 1}`, ...
    #[default]
    Json,
    /// A plain string, sent as is.
    Text,
    /// Bytes written as hex (`de ad be ef`) or base64, sent as a binary
    /// attachment (a `Buffer` on the server).
    Binary,
    /// The bytes of a file, sent as a binary attachment. The path may be
    /// relative to the workspace folder.
    File,
}

/// One emit argument; `value` may contain `{{variables}}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SocketIoArg {
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: SocketIoArgKind,
    #[serde(default)]
    pub value: String,
}

impl SocketIoArg {
    pub fn new(kind: SocketIoArgKind, value: impl Into<String>) -> Self {
        SocketIoArg { kind, value: value.into() }
    }

    /// Decodes the text of a [`SocketIoArgKind::Binary`] argument: hex
    /// (an even number of hex digits, whitespace ignored) or base64
    /// (standard or URL-safe, padding optional). Blank text is zero bytes.
    pub fn decode_binary(text: &str) -> Option<Vec<u8>> {
        use base64::Engine as _;
        use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let hex = compact.strip_prefix("0x").unwrap_or(&compact);
        if hex.len().is_multiple_of(2) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect();
        }
        [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD].iter().find_map(|e| e.decode(&compact).ok())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SocketIoMessage {
    #[serde(default = "new_id")]
    pub id: Id,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub event: String,
    /// Arguments as a JSON array text, e.g. `["hello", {"a": 1}]` (a single
    /// JSON value is one argument, other text one string). Only used while
    /// `arguments` is empty; messages saved before `arguments` existed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub args: String,
    /// The arguments one by one, each JSON, text or binary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<SocketIoArg>,
    /// Request an acknowledgement.
    #[serde(default, skip_serializing_if = "is_default")]
    pub ack: bool,
}

impl Default for SocketIoMessage {
    fn default() -> Self {
        SocketIoMessage {
            id: new_id(),
            name: String::new(),
            event: String::new(),
            args: String::new(),
            arguments: Vec::new(),
            ack: false,
        }
    }
}

impl SocketIoMessage {
    /// The arguments as a list: `arguments`, or the legacy `args` text split
    /// into JSON arguments (a JSON array gives its items; other text that is
    /// not JSON stays one argument, as text unless it holds `{{variables}}`
    /// that may make it JSON once resolved).
    pub fn argument_list(&self) -> Vec<SocketIoArg> {
        if !self.arguments.is_empty() || self.args.trim().is_empty() {
            return self.arguments.clone();
        }
        match serde_json::from_str::<serde_json::Value>(&self.args) {
            Ok(serde_json::Value::Array(items)) => {
                items.iter().map(|v| SocketIoArg::new(SocketIoArgKind::Json, v.to_string())).collect()
            }
            Ok(_) => vec![SocketIoArg::new(SocketIoArgKind::Json, self.args.trim())],
            Err(_) if self.args.contains("{{") => vec![SocketIoArg::new(SocketIoArgKind::Json, self.args.clone())],
            Err(_) => vec![SocketIoArg::new(SocketIoArgKind::Text, self.args.clone())],
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SocketIoRequest {
    /// `http://localhost:3000` (the Engine.IO endpoint host).
    #[serde(default)]
    pub url: String,
    /// Handshake path; empty = `/socket.io/`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Namespace; empty = `/`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<KeyValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<KeyValue>,
    /// JSON object sent as the handshake `auth` payload.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub auth_payload: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listeners: Vec<SocketIoListener>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<SocketIoMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_ssl: Option<bool>,
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
    /// Transports to use; default: long-polling upgraded to WebSocket.
    #[serde(default, skip_serializing_if = "is_default")]
    pub transport: SocketIoTransport,
    /// Protocol revision of the server; default: Socket.IO v3/v4.
    #[serde(default, skip_serializing_if = "is_default")]
    pub version: SocketIoVersion,
    /// Stop condition in the Collection Runner.
    #[serde(default, skip_serializing_if = "SessionRunner::is_default")]
    pub runner: SessionRunner,
}

/// Engine.IO transports of a Socket.IO connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SocketIoTransport {
    /// Like the official client: connect with HTTP long-polling, then
    /// upgrade to WebSocket when the server offers it (falls back to a
    /// direct WebSocket when the server refuses polling).
    #[default]
    Auto,
    /// WebSocket only, without long-polling first.
    WebSocket,
    /// HTTP long-polling only, never upgraded.
    Polling,
}

/// Socket.IO protocol revision spoken by the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SocketIoVersion {
    /// Socket.IO v3 and v4 servers (Engine.IO 4, Socket.IO protocol 5).
    #[default]
    V4,
    /// Socket.IO v2 servers (Engine.IO 3, Socket.IO protocol 4), and v3/v4
    /// servers started with `allowEIO3: true`.
    V2,
}

// ---------------------------------------------------------------------------
// MCP (FR-X8)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpTransport {
    /// Launch a local server and talk JSON-RPC over its stdin/stdout.
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<KeyValue>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        cwd: String,
    },
    /// Remote server over Streamable HTTP (with SSE responses).
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<KeyValue>,
    },
}

impl Default for McpTransport {
    fn default() -> Self {
        McpTransport::Http { url: String::new(), headers: Vec::new() }
    }
}

/// Wire protocol for a remote ([`McpTransport::Http`]) server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpHttpTransport {
    /// Streamable HTTP, falling back to the legacy HTTP+SSE transport when
    /// the server rejects the initial POST with a 4xx status.
    #[default]
    Auto,
    /// Streamable HTTP only (protocol 2025-03-26 and later).
    Streamable,
    /// Legacy HTTP+SSE (protocol 2024-11-05): an event stream from GET, and
    /// messages POSTed to the endpoint it announces.
    Sse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpCallKind {
    #[default]
    Tool,
    Prompt,
    Resource,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpSavedCall {
    #[serde(default = "new_id")]
    pub id: Id,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default)]
    pub kind: McpCallKind,
    /// Tool or prompt name, or resource URI.
    pub target: String,
    /// Arguments as JSON object text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arguments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct McpRequest {
    #[serde(default)]
    pub transport: McpTransport,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub saved_calls: Vec<McpSavedCall>,
    /// Bearer/API key for remote servers. OAuth 2.0 here supplies the client
    /// (`clientId`, `clientSecret`, `scope`, `redirect_uri`) for the MCP
    /// authorization flow; endpoints are discovered unless given.
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
    /// Remote transport flavor (ignored for stdio).
    #[serde(default, skip_serializing_if = "crate::util::is_default")]
    pub http_transport: McpHttpTransport,
}

// ---------------------------------------------------------------------------
// AI model requests (FR-X9)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    #[default]
    OpenAi,
    Anthropic,
    Google,
    /// Any OpenAI-compatible endpoint (Ollama, LM Studio, vLLM, ...).
    OpenAiCompatible,
}

impl AiProvider {
    pub const ALL: [AiProvider; 4] =
        [AiProvider::OpenAi, AiProvider::Anthropic, AiProvider::Google, AiProvider::OpenAiCompatible];

    pub fn label(self) -> &'static str {
        match self {
            AiProvider::OpenAi => "OpenAI",
            AiProvider::Anthropic => "Anthropic",
            AiProvider::Google => "Google Gemini",
            AiProvider::OpenAiCompatible => "OpenAI-compatible (local)",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AiMessage {
    /// `user`, `assistant` or `system`.
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AiRequest {
    #[serde(default)]
    pub provider: AiProvider,
    /// Empty = the provider's default endpoint.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub system: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<AiMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub stream: bool,
    /// Extra JSON merged into the provider request body.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub extra: String,
    /// Ids of MCP request items whose tools are offered to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<Id>,
    /// The API key, as a Bearer token (usually `{{vault:openai}}`).
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
}
