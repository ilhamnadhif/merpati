//! The workspace data model: collections, folders, requests and examples.
//!
//! The shape intentionally mirrors Postman Collection v2.1 closely enough that
//! import and export are lossless for the fields Merpati understands, while
//! staying idiomatic Rust.

use serde::{Deserialize, Serialize};

use crate::auth::Auth;
use crate::util::{default_true, is_default, is_true, new_id};
use crate::variables::Variable;

pub type Id = String;

/// A key/value row as used by query params, headers, url-encoded bodies and
/// path variables. Disabled rows are kept so the UI can toggle them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyValue {
    pub key: String,
    #[serde(default)]
    pub value: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl KeyValue {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        KeyValue { key: key.into(), value: value.into(), enabled: true, description: String::new() }
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormFieldKind {
    #[default]
    Text,
    File,
}

/// A row of a multipart/form-data body.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: FormFieldKind,
    /// File paths for `kind == File`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl FormField {
    pub fn text(key: impl Into<String>, value: impl Into<String>) -> Self {
        FormField { key: key.into(), value: value.into(), enabled: true, ..Default::default() }
    }

    pub fn file(key: impl Into<String>, path: impl Into<String>) -> Self {
        FormField {
            key: key.into(),
            kind: FormFieldKind::File,
            files: vec![path.into()],
            enabled: true,
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyMode {
    #[default]
    None,
    Raw,
    UrlEncoded,
    FormData,
    Binary,
    GraphQl,
}

impl BodyMode {
    pub const ALL: [BodyMode; 6] =
        [BodyMode::None, BodyMode::FormData, BodyMode::UrlEncoded, BodyMode::Raw, BodyMode::Binary, BodyMode::GraphQl];

    pub fn label(self) -> &'static str {
        match self {
            BodyMode::None => "none",
            BodyMode::Raw => "raw",
            BodyMode::UrlEncoded => "x-www-form-urlencoded",
            BodyMode::FormData => "form-data",
            BodyMode::Binary => "binary",
            BodyMode::GraphQl => "GraphQL",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawLanguage {
    #[default]
    Json,
    Text,
    Xml,
    Html,
    JavaScript,
}

impl RawLanguage {
    pub const ALL: [RawLanguage; 5] =
        [RawLanguage::Text, RawLanguage::JavaScript, RawLanguage::Json, RawLanguage::Html, RawLanguage::Xml];

    pub fn label(self) -> &'static str {
        match self {
            RawLanguage::Json => "JSON",
            RawLanguage::Text => "Text",
            RawLanguage::Xml => "XML",
            RawLanguage::Html => "HTML",
            RawLanguage::JavaScript => "JavaScript",
        }
    }

    /// The Content-Type a raw body of this language gets when the user has not
    /// set one explicitly (same defaults as Postman).
    pub fn content_type(self) -> &'static str {
        match self {
            RawLanguage::Json => "application/json",
            RawLanguage::Text => "text/plain",
            RawLanguage::Xml => "application/xml",
            RawLanguage::Html => "text/html",
            RawLanguage::JavaScript => "application/javascript",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphQlBody {
    #[serde(default)]
    pub query: String,
    /// Variables as JSON text, kept verbatim so the editor can show it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub variables: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
}

/// Request body. Like Postman, the data for every mode is kept at the same
/// time so switching modes in the UI does not lose what the user typed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Body {
    #[serde(default)]
    pub mode: BodyMode,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub raw_language: RawLanguage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urlencoded: Vec<KeyValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub form_data: Vec<FormField>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub binary_path: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub graphql: GraphQlBody,
}

impl Body {
    pub fn raw_json(text: impl Into<String>) -> Self {
        Body { mode: BodyMode::Raw, raw: text.into(), raw_language: RawLanguage::Json, ..Default::default() }
    }

    pub fn raw_text(text: impl Into<String>, language: RawLanguage) -> Self {
        Body { mode: BodyMode::Raw, raw: text.into(), raw_language: language, ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.mode == BodyMode::None
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HttpVersion {
    #[default]
    Auto,
    Http1,
    Http2,
}

/// Per-request settings. `None` means "use the workspace default".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_redirects: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_redirects: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_original_method: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_auth_header: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_ssl: Option<bool>,
    /// Automatically percent-encode characters in the URL that need it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encode_url: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_cookies: Option<bool>,
    /// 0 disables the timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_version: Option<HttpVersion>,
    /// Remove the default headers Merpati adds (`User-Agent`, `Accept`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_default_headers: Vec<String>,
}

impl RequestSettings {
    pub fn is_empty(&self) -> bool {
        *self == RequestSettings::default()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpRequest {
    #[serde(default = "default_method")]
    pub method: String,
    /// Raw URL as typed, including the enabled query string and `{{vars}}`.
    #[serde(default)]
    pub url: String,
    /// Query parameters, including disabled ones. Enabled ones mirror `url`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<KeyValue>,
    /// Values for `:name` path segments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_vars: Vec<KeyValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<KeyValue>,
    /// Kept when the mode is `None` but other modes still hold data.
    #[serde(default, skip_serializing_if = "is_default")]
    pub body: Body,
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "RequestSettings::is_empty")]
    pub settings: RequestSettings,
}

fn default_method() -> String {
    "GET".to_string()
}

impl Default for HttpRequest {
    fn default() -> Self {
        HttpRequest {
            method: default_method(),
            url: String::new(),
            params: Vec::new(),
            path_vars: Vec::new(),
            headers: Vec::new(),
            body: Body::default(),
            auth: Auth::default(),
            settings: RequestSettings::default(),
        }
    }
}

impl HttpRequest {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        let mut r = HttpRequest { method: method.into(), ..Default::default() };
        r.set_url(url.into());
        r
    }

    /// Sets the URL and re-derives the query params from it, keeping disabled
    /// params and descriptions of params that still exist.
    pub fn set_url(&mut self, url: String) {
        self.params = crate::urlutil::merge_params_from_url(&url, &self.params);
        self.path_vars = crate::urlutil::merge_path_vars(&url, &self.path_vars);
        self.url = url;
    }

    /// Rebuilds the query part of `url` from `params` after the params table
    /// was edited.
    pub fn sync_url_from_params(&mut self) {
        self.url = crate::urlutil::replace_query(&self.url, &self.params);
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().rev().find(|h| h.enabled && h.key.eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WsMessageKind {
    #[default]
    Text,
    /// Body holds base64 or hex encoded bytes, see `WsSavedMessage::encoding`.
    Binary,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WsSavedMessage {
    #[serde(default = "new_id")]
    pub id: Id,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: WsMessageKind,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WsRequest {
    #[serde(default)]
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<KeyValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<KeyValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subprotocols: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<WsSavedMessage>,
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_ssl: Option<bool>,
    /// Stop condition in the Collection Runner.
    #[serde(default, skip_serializing_if = "crate::protocols::SessionRunner::is_default")]
    pub runner: crate::protocols::SessionRunner,
}

/// What kind of request an item is.
#[allow(clippy::large_enum_variant)] // Http is the common case; boxing would only add indirection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "lowercase")]
pub enum RequestKind {
    Http(HttpRequest),
    #[serde(rename = "websocket")]
    WebSocket(WsRequest),
    Grpc(crate::protocols::GrpcRequest),
    Mqtt(crate::protocols::MqttRequest),
    #[serde(rename = "socketio")]
    SocketIo(crate::protocols::SocketIoRequest),
    Mcp(crate::protocols::McpRequest),
    Ai(crate::protocols::AiRequest),
}

impl Default for RequestKind {
    fn default() -> Self {
        RequestKind::Http(HttpRequest::default())
    }
}

impl RequestKind {
    pub fn as_http(&self) -> Option<&HttpRequest> {
        match self {
            RequestKind::Http(r) => Some(r),
            _ => None,
        }
    }

    pub fn as_http_mut(&mut self) -> Option<&mut HttpRequest> {
        match self {
            RequestKind::Http(r) => Some(r),
            _ => None,
        }
    }

    pub fn auth(&self) -> &Auth {
        match self {
            RequestKind::Http(r) => &r.auth,
            RequestKind::WebSocket(r) => &r.auth,
            RequestKind::Grpc(r) => &r.auth,
            RequestKind::Mqtt(r) => &r.auth,
            RequestKind::SocketIo(r) => &r.auth,
            RequestKind::Mcp(r) => &r.auth,
            RequestKind::Ai(r) => &r.auth,
        }
    }

    pub fn auth_mut(&mut self) -> &mut Auth {
        match self {
            RequestKind::Http(r) => &mut r.auth,
            RequestKind::WebSocket(r) => &mut r.auth,
            RequestKind::Grpc(r) => &mut r.auth,
            RequestKind::Mqtt(r) => &mut r.auth,
            RequestKind::SocketIo(r) => &mut r.auth,
            RequestKind::Mcp(r) => &mut r.auth,
            RequestKind::Ai(r) => &mut r.auth,
        }
    }

    /// Short label for the tree view (`GET`, `WS`, `GQL`).
    pub fn badge(&self) -> String {
        match self {
            RequestKind::Http(r) if r.body.mode == BodyMode::GraphQl => "GQL".into(),
            RequestKind::Http(r) => r.method.to_uppercase(),
            RequestKind::WebSocket(_) => "WS".into(),
            RequestKind::Grpc(_) => "gRPC".into(),
            RequestKind::Mqtt(_) => "MQTT".into(),
            RequestKind::SocketIo(_) => "SIO".into(),
            RequestKind::Mcp(_) => "MCP".into(),
            RequestKind::Ai(_) => "AI".into(),
        }
    }

    /// Human name of the protocol.
    pub fn protocol_label(&self) -> &'static str {
        match self {
            RequestKind::Http(r) if r.body.mode == BodyMode::GraphQl => "GraphQL",
            RequestKind::Http(_) => "HTTP",
            RequestKind::WebSocket(_) => "WebSocket",
            RequestKind::Grpc(_) => "gRPC",
            RequestKind::Mqtt(_) => "MQTT",
            RequestKind::SocketIo(_) => "Socket.IO",
            RequestKind::Mcp(_) => "MCP",
            RequestKind::Ai(_) => "AI",
        }
    }

    pub fn url(&self) -> &str {
        match self {
            RequestKind::Http(r) => &r.url,
            RequestKind::WebSocket(r) => &r.url,
            RequestKind::Grpc(r) => &r.url,
            RequestKind::Mqtt(r) => &r.url,
            RequestKind::SocketIo(r) => &r.url,
            RequestKind::Mcp(r) => match &r.transport {
                crate::protocols::McpTransport::Http { url, .. } => url,
                crate::protocols::McpTransport::Stdio { command, .. } => command,
            },
            RequestKind::Ai(r) => &r.model,
        }
    }
}

/// Scripts attached to a collection, folder or request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Scripts {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pre_request: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub post_response: String,
    /// Runs for each message of streaming protocols (gRPC, WebSocket,
    /// Socket.IO, MQTT; FR-S7).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub on_message: String,
}

impl Scripts {
    pub fn is_empty(&self) -> bool {
        self.pre_request.trim().is_empty() && self.post_response.trim().is_empty() && self.on_message.trim().is_empty()
    }

    /// The script for one event.
    pub fn code(&self, event: crate::script::ScriptEvent) -> &String {
        use crate::script::ScriptEvent;
        match event {
            ScriptEvent::PreRequest => &self.pre_request,
            ScriptEvent::PostResponse => &self.post_response,
            ScriptEvent::OnMessage => &self.on_message,
        }
    }

    pub fn code_mut(&mut self, event: crate::script::ScriptEvent) -> &mut String {
        use crate::script::ScriptEvent;
        match event {
            ScriptEvent::PreRequest => &mut self.pre_request,
            ScriptEvent::PostResponse => &mut self.post_response,
            ScriptEvent::OnMessage => &mut self.on_message,
        }
    }
}

/// A saved request/response pair. Examples document an endpoint and feed the
/// mock server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Example {
    #[serde(default = "new_id")]
    pub id: Id,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<HttpRequest>,
    #[serde(default = "default_status")]
    pub status: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status_text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<KeyValue>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// Language hint for the body viewer (`json`, `xml`, `html`, `text`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

fn default_status() -> u16 {
    200
}

impl Default for Example {
    fn default() -> Self {
        Example {
            id: new_id(),
            name: "Example".into(),
            request: None,
            status: 200,
            status_text: "OK".into(),
            headers: Vec::new(),
            body: String::new(),
            language: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestItem {
    #[serde(default = "new_id")]
    pub id: Id,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default)]
    pub request: RequestKind,
    #[serde(default, skip_serializing_if = "Scripts::is_empty")]
    pub scripts: Scripts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<Example>,
}

impl RequestItem {
    pub fn new(name: impl Into<String>, request: HttpRequest) -> Self {
        RequestItem {
            id: new_id(),
            name: name.into(),
            description: String::new(),
            request: RequestKind::Http(request),
            scripts: Scripts::default(),
            examples: Vec::new(),
        }
    }

    pub fn http(&self) -> Option<&HttpRequest> {
        self.request.as_http()
    }

    pub fn http_mut(&mut self) -> Option<&mut HttpRequest> {
        self.request.as_http_mut()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    #[serde(default = "new_id")]
    pub id: Id,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Scripts::is_empty")]
    pub scripts: Scripts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<Variable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<Item>,
}

impl Folder {
    pub fn new(name: impl Into<String>) -> Self {
        Folder {
            id: new_id(),
            name: name.into(),
            description: String::new(),
            auth: Auth::default(),
            scripts: Scripts::default(),
            variables: Vec::new(),
            items: Vec::new(),
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Item {
    Folder(Folder),
    Request(RequestItem),
}

impl Item {
    pub fn id(&self) -> &str {
        match self {
            Item::Folder(f) => &f.id,
            Item::Request(r) => &r.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Item::Folder(f) => &f.name,
            Item::Request(r) => &r.name,
        }
    }

    pub fn set_name(&mut self, name: String) {
        match self {
            Item::Folder(f) => f.name = name,
            Item::Request(r) => r.name = name,
        }
    }

    pub fn as_request(&self) -> Option<&RequestItem> {
        match self {
            Item::Request(r) => Some(r),
            _ => None,
        }
    }

    pub fn as_folder(&self) -> Option<&Folder> {
        match self {
            Item::Folder(f) => Some(f),
            _ => None,
        }
    }

    /// Deep clone with fresh ids everywhere (used by "Duplicate").
    pub fn duplicate(&self) -> Item {
        let mut copy = self.clone();
        copy.regenerate_ids();
        copy
    }

    fn regenerate_ids(&mut self) {
        match self {
            Item::Folder(f) => {
                f.id = new_id();
                for child in &mut f.items {
                    child.regenerate_ids();
                }
            }
            Item::Request(r) => {
                r.id = new_id();
                for ex in &mut r.examples {
                    ex.id = new_id();
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    #[serde(default = "new_id")]
    pub id: Id,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Collection-level auth. `Inherit` at this level behaves like `None`.
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    pub auth: Auth,
    #[serde(default, skip_serializing_if = "Scripts::is_empty")]
    pub scripts: Scripts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<Variable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<Item>,
}

/// One level of the chain from a collection down to an item, used to resolve
/// inherited auth, scripts and folder variables.
#[derive(Clone, Copy, Debug)]
pub enum Ancestor<'a> {
    Collection(&'a Collection),
    Folder(&'a Folder),
}

impl<'a> Ancestor<'a> {
    pub fn id(&self) -> &'a str {
        match self {
            Ancestor::Collection(c) => &c.id,
            Ancestor::Folder(f) => &f.id,
        }
    }

    pub fn name(&self) -> &'a str {
        match self {
            Ancestor::Collection(c) => &c.name,
            Ancestor::Folder(f) => &f.name,
        }
    }

    pub fn auth(&self) -> &'a Auth {
        match self {
            Ancestor::Collection(c) => &c.auth,
            Ancestor::Folder(f) => &f.auth,
        }
    }

    pub fn scripts(&self) -> &'a Scripts {
        match self {
            Ancestor::Collection(c) => &c.scripts,
            Ancestor::Folder(f) => &f.scripts,
        }
    }

    pub fn variables(&self) -> &'a [Variable] {
        match self {
            Ancestor::Collection(c) => &c.variables,
            Ancestor::Folder(f) => &f.variables,
        }
    }
}

/// Where an inherited auth came from, for display ("inherited from folder X").
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveAuth<'a> {
    pub auth: &'a Auth,
    /// Id of the collection/folder/request that defines it.
    pub owner_id: &'a str,
    pub owner_name: &'a str,
}

impl Collection {
    pub fn new(name: impl Into<String>) -> Self {
        Collection {
            id: new_id(),
            name: name.into(),
            description: String::new(),
            auth: Auth::default(),
            scripts: Scripts::default(),
            variables: Vec::new(),
            items: Vec::new(),
        }
    }

    /// Finds an item anywhere in the tree.
    pub fn find(&self, id: &str) -> Option<&Item> {
        fn walk<'a>(items: &'a [Item], id: &str) -> Option<&'a Item> {
            for item in items {
                if item.id() == id {
                    return Some(item);
                }
                if let Item::Folder(f) = item
                    && let Some(found) = walk(&f.items, id)
                {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.items, id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Item> {
        fn walk<'a>(items: &'a mut [Item], id: &str) -> Option<&'a mut Item> {
            for item in items {
                if item.id() == id {
                    return Some(item);
                }
                if let Item::Folder(f) = item
                    && let Some(found) = walk(&mut f.items, id)
                {
                    return Some(found);
                }
            }
            None
        }
        walk(&mut self.items, id)
    }

    pub fn find_request(&self, id: &str) -> Option<&RequestItem> {
        self.find(id).and_then(Item::as_request)
    }

    pub fn find_request_mut(&mut self, id: &str) -> Option<&mut RequestItem> {
        match self.find_mut(id) {
            Some(Item::Request(r)) => Some(r),
            _ => None,
        }
    }

    /// Returns the folder (or the collection root when `parent_id` is the
    /// collection id or `None`) children vector.
    pub fn children_mut(&mut self, parent_id: Option<&str>) -> Option<&mut Vec<Item>> {
        match parent_id {
            None => Some(&mut self.items),
            Some(pid) if pid == self.id => Some(&mut self.items),
            Some(pid) => match self.find_mut(pid) {
                Some(Item::Folder(f)) => Some(&mut f.items),
                _ => None,
            },
        }
    }

    /// The chain of containers from the collection down to (excluding) the
    /// item with `id`. `None` if the item does not exist.
    pub fn ancestors(&self, id: &str) -> Option<Vec<Ancestor<'_>>> {
        fn walk<'a>(items: &'a [Item], id: &str, path: &mut Vec<Ancestor<'a>>) -> bool {
            for item in items {
                if item.id() == id {
                    return true;
                }
                if let Item::Folder(f) = item {
                    path.push(Ancestor::Folder(f));
                    if walk(&f.items, id, path) {
                        return true;
                    }
                    path.pop();
                }
            }
            false
        }
        let mut path = vec![Ancestor::Collection(self)];
        if id == self.id {
            return Some(Vec::new());
        }
        walk(&self.items, id, &mut path).then_some(path)
    }

    /// Id of the folder (or collection) that directly contains `id`.
    pub fn parent_of(&self, id: &str) -> Option<&str> {
        self.ancestors(id).and_then(|a| a.last().map(|x| x.id()))
    }

    /// Resolves the auth that applies to the item with `id`, walking up
    /// through `Inherit` levels. Works for requests and folders.
    pub fn effective_auth<'a>(&'a self, id: &str) -> Option<EffectiveAuth<'a>> {
        let item = self.find(id)?;
        let own: (&Auth, &str, &str) = match item {
            Item::Request(r) => (r.request.auth(), &r.id, &r.name),
            Item::Folder(f) => (&f.auth, &f.id, &f.name),
        };
        if !own.0.is_inherit() {
            return Some(EffectiveAuth { auth: own.0, owner_id: own.1, owner_name: own.2 });
        }
        let chain = self.ancestors(id)?;
        Some(effective_from_chain(&chain).unwrap_or(EffectiveAuth {
            auth: &crate::auth::NO_AUTH,
            owner_id: &self.id,
            owner_name: &self.name,
        }))
    }

    /// Depth-first list of all requests in display order.
    pub fn requests(&self) -> Vec<&RequestItem> {
        fn walk<'a>(items: &'a [Item], out: &mut Vec<&'a RequestItem>) {
            for item in items {
                match item {
                    Item::Request(r) => out.push(r),
                    Item::Folder(f) => walk(&f.items, out),
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.items, &mut out);
        out
    }

    pub fn request_count(&self) -> usize {
        self.requests().len()
    }

    /// Removes an item anywhere in the tree and returns it.
    pub fn remove(&mut self, id: &str) -> Option<Item> {
        fn walk(items: &mut Vec<Item>, id: &str) -> Option<Item> {
            if let Some(pos) = items.iter().position(|i| i.id() == id) {
                return Some(items.remove(pos));
            }
            for item in items.iter_mut() {
                if let Item::Folder(f) = item
                    && let Some(found) = walk(&mut f.items, id)
                {
                    return Some(found);
                }
            }
            None
        }
        walk(&mut self.items, id)
    }

    /// Inserts `item` into `parent_id` (folder or collection) at `index`
    /// (clamped). Returns false when the parent does not exist or would create
    /// a cycle.
    pub fn insert(&mut self, parent_id: Option<&str>, index: usize, item: Item) -> bool {
        if let (Some(pid), Item::Folder(f)) = (parent_id, &item)
            && (pid == f.id || contains_id(&f.items, pid))
        {
            return false;
        }
        match self.children_mut(parent_id) {
            Some(children) => {
                let idx = index.min(children.len());
                children.insert(idx, item);
                true
            }
            None => false,
        }
    }

    /// Moves an item to a new parent/index. Moving a folder into itself or a
    /// descendant is rejected.
    pub fn move_item(&mut self, id: &str, new_parent: Option<&str>, index: usize) -> bool {
        if let Some(pid) = new_parent {
            if pid == id {
                return false;
            }
            if let Some(Item::Folder(f)) = self.find(id)
                && contains_id(&f.items, pid)
            {
                return false;
            }
            if pid != self.id && !matches!(self.find(pid), Some(Item::Folder(_))) {
                return false;
            }
        }
        // Adjust the index when moving down within the same parent.
        let same_parent_pos = {
            let current_parent = self.parent_of(id).map(str::to_owned);
            let target = new_parent.map(str::to_owned).unwrap_or_else(|| self.id.clone());
            if current_parent.as_deref() == Some(target.as_str()) {
                self.children_mut(Some(&target)).and_then(|c| c.iter().position(|i| i.id() == id))
            } else {
                None
            }
        };
        let Some(item) = self.remove(id) else { return false };
        let index = match same_parent_pos {
            Some(pos) if pos < index => index - 1,
            _ => index,
        };
        self.insert(new_parent, index, item)
    }
}

fn contains_id(items: &[Item], id: &str) -> bool {
    items.iter().any(|i| {
        i.id() == id
            || match i {
                Item::Folder(f) => contains_id(&f.items, id),
                _ => false,
            }
    })
}

/// Walks the chain from the innermost container outwards and returns the first
/// auth that is not `Inherit`.
pub fn effective_from_chain<'a>(chain: &[Ancestor<'a>]) -> Option<EffectiveAuth<'a>> {
    chain.iter().rev().find(|a| !a.auth().is_inherit()).map(|a| EffectiveAuth {
        auth: a.auth(),
        owner_id: a.id(),
        owner_name: a.name(),
    })
}

/// Shorthand for building trees in tests and importers.
impl From<RequestItem> for Item {
    fn from(r: RequestItem) -> Self {
        Item::Request(r)
    }
}

impl From<Folder> for Item {
    fn from(f: Folder) -> Self {
        Item::Folder(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Auth, AuthType};

    fn sample() -> Collection {
        let mut c = Collection::new("API");
        c.auth = Auth::bearer("{{token}}");
        let mut f = Folder::new("Users");
        f.items.push(RequestItem::new("List", HttpRequest::new("GET", "{{base}}/users")).into());
        let mut inner = Folder::new("Admin");
        inner.auth = Auth::basic("a", "b");
        inner.items.push(RequestItem::new("Delete", HttpRequest::new("DELETE", "{{base}}/users/1")).into());
        f.items.push(inner.into());
        c.items.push(f.into());
        c.items.push(RequestItem::new("Health", HttpRequest::new("GET", "{{base}}/health")).into());
        c
    }

    #[test]
    fn auth_inheritance() {
        let c = sample();
        let list = c.requests().into_iter().find(|r| r.name == "List").unwrap();
        let eff = c.effective_auth(&list.id).unwrap();
        assert_eq!(eff.auth.kind, AuthType::Bearer);
        assert_eq!(eff.owner_id, c.id);

        let del = c.requests().into_iter().find(|r| r.name == "Delete").unwrap();
        let eff = c.effective_auth(&del.id).unwrap();
        assert_eq!(eff.auth.kind, AuthType::Basic);
        assert_eq!(eff.owner_name, "Admin");
    }

    #[test]
    fn no_auth_when_nothing_set() {
        let mut c = sample();
        c.auth = Auth::default();
        let health = c.requests().into_iter().find(|r| r.name == "Health").unwrap().id.clone();
        assert_eq!(c.effective_auth(&health).unwrap().auth.kind, AuthType::None);
    }

    #[test]
    fn move_and_cycle_protection() {
        let mut c = sample();
        let users = c.items[0].id().to_string();
        let admin = c.items[0].as_folder().unwrap().items[1].id().to_string();
        // Can't move a folder into its own descendant.
        assert!(!c.move_item(&users, Some(&admin), 0));
        // Move Health into Admin.
        let health = c.items[1].id().to_string();
        assert!(c.move_item(&health, Some(&admin), 0));
        assert_eq!(c.items.len(), 1);
        assert_eq!(c.parent_of(&health), Some(admin.as_str()));
        assert_eq!(c.request_count(), 3);
    }

    #[test]
    fn reorder_within_parent() {
        let mut c = sample();
        let first = c.items[0].id().to_string();
        assert!(c.move_item(&first, None, 2));
        assert_eq!(c.items[1].id(), first);
    }

    #[test]
    fn duplicate_gets_new_ids() {
        let c = sample();
        let d = c.items[0].duplicate();
        assert_ne!(d.id(), c.items[0].id());
        let orig_ids: Vec<_> = c.requests().iter().map(|r| r.id.clone()).collect();
        if let Item::Folder(f) = &d {
            for child in &f.items {
                assert!(!orig_ids.contains(&child.id().to_string()));
            }
        }
    }

    #[test]
    fn request_file_shape() {
        let r = RequestItem::new("List", HttpRequest::new("GET", "https://x.test/a?b=1"));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["request"]["protocol"], "http");
        assert_eq!(v["request"]["params"][0]["key"], "b");
        let back: RequestItem = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn body_data_survives_mode_none() {
        let mut req = HttpRequest::new("POST", "https://x.test");
        req.body = Body::raw_json("{\"a\": 1}");
        req.body.mode = BodyMode::None;
        let r = RequestItem::new("r", req);
        let back: RequestItem = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        // An untouched body is still left out of the file.
        let v = serde_json::to_value(RequestItem::new("r", HttpRequest::new("GET", "https://x.test"))).unwrap();
        assert!(v["request"].get("body").is_none());
    }
}
