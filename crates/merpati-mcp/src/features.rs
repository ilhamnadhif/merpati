//! Client features the host can offer to servers: sampling
//! (`sampling/createMessage`), elicitation (`elicitation/create`) and roots
//! (`roots/list`). Each is advertised in `initialize` only when the host
//! enables it on [`crate::McpClientBuilder`].

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::McpError;
use crate::types::Content;

/// A boxed, `Send` future (what the handler traits return).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

/// One message of a sampling conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct SamplingMessage {
    /// `user` or `assistant`.
    pub role: String,
    /// Text, image or audio. Newer servers may send several blocks.
    pub content: Vec<Content>,
}

/// A `sampling/createMessage` request from the server.
#[derive(Clone, Debug, PartialEq)]
pub struct SamplingRequest {
    /// Display name of the requesting server.
    pub server: String,
    pub messages: Vec<SamplingMessage>,
    pub system_prompt: Option<String>,
    pub max_tokens: u64,
    pub temperature: Option<f64>,
    pub stop_sequences: Vec<String>,
    /// Model name hints, most preferred first (`modelPreferences.hints[].name`).
    pub model_hints: Vec<String>,
    /// The whole `modelPreferences` object (`costPriority`, `speedPriority`,
    /// `intelligencePriority`, ...).
    pub model_preferences: Option<Value>,
    /// `none`, `thisServer` or `allServers`.
    pub include_context: Option<String>,
    pub metadata: Option<Value>,
    /// The complete `params` object.
    pub raw: Value,
}

impl SamplingRequest {
    pub(crate) fn parse(server: &str, params: &Value) -> Result<SamplingRequest, String> {
        let messages = params.get("messages").and_then(Value::as_array).ok_or("params.messages is missing")?;
        let messages = messages
            .iter()
            .map(|m| {
                let content = match m.get("content") {
                    Some(Value::Array(items)) => items.iter().map(Content::from_value).collect(),
                    Some(c) => vec![Content::from_value(c)],
                    None => Vec::new(),
                };
                SamplingMessage { role: str_field(m, "role").unwrap_or_else(|| "user".into()), content }
            })
            .collect();
        let prefs = params.get("modelPreferences").filter(|p| p.is_object()).cloned();
        let model_hints = prefs
            .as_ref()
            .and_then(|p| p.get("hints"))
            .and_then(Value::as_array)
            .map(|h| h.iter().filter_map(|x| str_field(x, "name")).collect())
            .unwrap_or_default();
        Ok(SamplingRequest {
            server: server.to_string(),
            messages,
            system_prompt: str_field(params, "systemPrompt").filter(|s| !s.is_empty()),
            max_tokens: params.get("maxTokens").and_then(Value::as_u64).ok_or("params.maxTokens is missing")?,
            temperature: params.get("temperature").and_then(Value::as_f64),
            stop_sequences: params
                .get("stopSequences")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            model_hints,
            model_preferences: prefs,
            include_context: str_field(params, "includeContext"),
            metadata: params.get("metadata").filter(|m| !m.is_null()).cloned(),
            raw: params.clone(),
        })
    }

    /// The text of every message, for a quick preview ("user: ...").
    pub fn transcript(&self) -> String {
        self.messages
            .iter()
            .map(|m| {
                let text: Vec<String> = m.content.iter().map(Content::to_display_string).collect();
                format!("{}: {}", m.role, text.join("\n"))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What the host's model produced.
#[derive(Clone, Debug, PartialEq)]
pub struct SamplingResult {
    /// The model that answered (e.g. `claude-sonnet-4-5`).
    pub model: String,
    /// Normally `assistant`.
    pub role: String,
    pub content: Content,
    /// `endTurn`, `stopSequence`, `maxTokens` or a provider-specific value.
    pub stop_reason: Option<String>,
}

impl SamplingResult {
    /// An assistant text answer.
    pub fn text(model: impl Into<String>, text: impl Into<String>) -> SamplingResult {
        SamplingResult {
            model: model.into(),
            role: "assistant".into(),
            content: Content::Text { text: text.into() },
            stop_reason: Some("endTurn".into()),
        }
    }

    pub(crate) fn to_value(&self) -> Value {
        let mut v = json!({"model": self.model, "role": self.role, "content": self.content.to_value()});
        if let Some(r) = &self.stop_reason {
            v["stopReason"] = json!(r);
        }
        v
    }
}

/// Implemented by the host to serve `sampling/createMessage`: show the
/// request to the user, run it on an AI provider, let the user review the
/// answer. Return [`McpError::rejected`] when the user declines; any other
/// error is sent to the server as an internal error.
///
/// Closures `Fn(SamplingRequest) -> impl Future<Output = Result<SamplingResult, McpError>>`
/// implement it.
pub trait SamplingHandler: Send + Sync {
    fn create_message(&self, request: SamplingRequest) -> BoxFuture<'_, Result<SamplingResult, McpError>>;
}

impl<F, Fut> SamplingHandler for F
where
    F: Fn(SamplingRequest) -> Fut + Send + Sync,
    Fut: Future<Output = Result<SamplingResult, McpError>> + Send + 'static,
{
    fn create_message(&self, request: SamplingRequest) -> BoxFuture<'_, Result<SamplingResult, McpError>> {
        Box::pin(self(request))
    }
}

// ---------------------------------------------------------------------------
// Elicitation
// ---------------------------------------------------------------------------

/// An `elicitation/create` request: the server asks the user for input.
#[derive(Clone, Debug, PartialEq)]
pub struct ElicitationRequest {
    /// Display name of the requesting server.
    pub server: String,
    /// What to show the user.
    pub message: String,
    /// `requestedSchema`: an object schema with primitive properties. Use
    /// [`ElicitationRequest::fields`] to build a form from it.
    pub requested_schema: Value,
    /// `form` (default) or `url` (protocol 2025-11-25: open [`Self::url`]).
    pub mode: String,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
    /// The complete `params` object.
    pub raw: Value,
}

/// The user's answer to an elicitation.
#[derive(Clone, Debug, PartialEq)]
pub enum ElicitationResponse {
    /// Submitted, with the form values (property name to value).
    Accept(Map<String, Value>),
    /// Explicitly said no.
    Decline,
    /// Dismissed without choosing.
    Cancel,
}

impl ElicitationResponse {
    pub(crate) fn to_value(&self) -> Value {
        match self {
            ElicitationResponse::Accept(content) => json!({"action": "accept", "content": content}),
            ElicitationResponse::Decline => json!({"action": "decline"}),
            ElicitationResponse::Cancel => json!({"action": "cancel"}),
        }
    }
}

/// Kind of one elicitation form field.
#[derive(Clone, Debug, PartialEq)]
pub enum ElicitFieldKind {
    /// `format` is `email`, `uri`, `date` or `date-time` when given.
    Text {
        format: Option<String>,
        min_length: Option<u64>,
        max_length: Option<u64>,
    },
    Number {
        integer: bool,
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Boolean,
    /// Pick one of `values` (shown as `labels`, same length).
    Choice {
        values: Vec<String>,
        labels: Vec<String>,
    },
    /// Pick any of `values` (array of enum strings).
    MultiChoice {
        values: Vec<String>,
        labels: Vec<String>,
        min_items: Option<u64>,
        max_items: Option<u64>,
    },
    /// A schema this client does not know how to render (raw schema).
    Other(Value),
}

/// One property of the requested schema, ready for a form.
#[derive(Clone, Debug, PartialEq)]
pub struct ElicitField {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub kind: ElicitFieldKind,
    pub default: Option<Value>,
}

impl ElicitField {
    /// Title if present, else the property name.
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

/// `enum` + `enumNames` (2025-06-18) or `oneOf`/`anyOf` of `{const, title}` (2025-11-25).
fn choices(s: &Value) -> Option<(Vec<String>, Vec<String>)> {
    if let Some(values) = s.get("enum").and_then(Value::as_array) {
        let values: Vec<String> =
            values.iter().map(|v| v.as_str().map(str::to_string).unwrap_or(v.to_string())).collect();
        let names = s.get("enumNames").and_then(Value::as_array);
        let labels = values
            .iter()
            .enumerate()
            .map(|(i, v)| names.and_then(|n| n.get(i)).and_then(Value::as_str).unwrap_or(v).to_string())
            .collect();
        return Some((values, labels));
    }
    let options = s.get("oneOf").or_else(|| s.get("anyOf")).and_then(Value::as_array)?;
    let mut values = Vec::new();
    let mut labels = Vec::new();
    for o in options {
        let v = o.get("const")?;
        let v = v.as_str().map(str::to_string).unwrap_or(v.to_string());
        labels.push(str_field(o, "title").unwrap_or_else(|| v.clone()));
        values.push(v);
    }
    Some((values, labels))
}

fn field_kind(s: &Value) -> ElicitFieldKind {
    let ty = s.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "array" => match s.get("items").and_then(choices) {
            Some((values, labels)) => ElicitFieldKind::MultiChoice {
                values,
                labels,
                min_items: s.get("minItems").and_then(Value::as_u64),
                max_items: s.get("maxItems").and_then(Value::as_u64),
            },
            None => ElicitFieldKind::Other(s.clone()),
        },
        "boolean" => ElicitFieldKind::Boolean,
        "number" | "integer" => ElicitFieldKind::Number {
            integer: ty == "integer",
            minimum: s.get("minimum").and_then(Value::as_f64),
            maximum: s.get("maximum").and_then(Value::as_f64),
        },
        _ => match choices(s) {
            Some((values, labels)) => ElicitFieldKind::Choice { values, labels },
            None if ty == "string" || ty.is_empty() => ElicitFieldKind::Text {
                format: str_field(s, "format"),
                min_length: s.get("minLength").and_then(Value::as_u64),
                max_length: s.get("maxLength").and_then(Value::as_u64),
            },
            None => ElicitFieldKind::Other(s.clone()),
        },
    }
}

impl ElicitationRequest {
    pub(crate) fn parse(server: &str, params: &Value) -> ElicitationRequest {
        ElicitationRequest {
            server: server.to_string(),
            message: str_field(params, "message").unwrap_or_default(),
            requested_schema: params
                .get("requestedSchema")
                .filter(|s| s.is_object())
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            mode: str_field(params, "mode").unwrap_or_else(|| "form".into()),
            url: str_field(params, "url"),
            elicitation_id: str_field(params, "elicitationId"),
            raw: params.clone(),
        }
    }

    /// The schema's properties in declaration order.
    pub fn fields(&self) -> Vec<ElicitField> {
        let required: Vec<&str> = self
            .requested_schema
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(props) = self.requested_schema.get("properties").and_then(Value::as_object) else {
            return Vec::new();
        };
        props
            .iter()
            .map(|(name, s)| ElicitField {
                name: name.clone(),
                title: str_field(s, "title"),
                description: str_field(s, "description"),
                required: required.contains(&name.as_str()),
                kind: field_kind(s),
                default: s.get("default").cloned(),
            })
            .collect()
    }

    /// Checks form values against the schema; returns one message per
    /// problem (empty when the values can be sent).
    pub fn validate(&self, content: &Map<String, Value>) -> Vec<String> {
        let mut problems = Vec::new();
        for f in self.fields() {
            let Some(v) = content.get(&f.name).filter(|v| !v.is_null()) else {
                if f.required {
                    problems.push(format!("{} is required", f.label()));
                }
                continue;
            };
            let bad = |p: &mut Vec<String>, what: String| p.push(format!("{}: {what}", f.label()));
            match &f.kind {
                ElicitFieldKind::Text { min_length, max_length, .. } => match v.as_str() {
                    None => bad(&mut problems, "must be text".into()),
                    Some(s) => {
                        let n = s.chars().count() as u64;
                        if min_length.is_some_and(|m| n < m) || max_length.is_some_and(|m| n > m) {
                            bad(&mut problems, "has the wrong length".into());
                        }
                    }
                },
                ElicitFieldKind::Number { integer, minimum, maximum } => match v.as_f64() {
                    None => bad(&mut problems, "must be a number".into()),
                    Some(x) => {
                        if *integer && x.fract() != 0.0 {
                            bad(&mut problems, "must be a whole number".into());
                        }
                        if minimum.is_some_and(|m| x < m) || maximum.is_some_and(|m| x > m) {
                            bad(&mut problems, "is out of range".into());
                        }
                    }
                },
                ElicitFieldKind::Boolean if !v.is_boolean() => bad(&mut problems, "must be true or false".into()),
                ElicitFieldKind::Choice { values, .. }
                    if !v.as_str().is_some_and(|s| values.iter().any(|x| x == s)) =>
                {
                    bad(&mut problems, "is not one of the choices".into())
                }
                ElicitFieldKind::MultiChoice { values, min_items, max_items, .. } => match v.as_array() {
                    None => bad(&mut problems, "must be a list".into()),
                    Some(items) => {
                        if items.iter().any(|i| !i.as_str().is_some_and(|s| values.iter().any(|x| x == s))) {
                            bad(&mut problems, "contains a value that is not a choice".into());
                        }
                        let n = items.len() as u64;
                        if min_items.is_some_and(|m| n < m) || max_items.is_some_and(|m| n > m) {
                            bad(&mut problems, "has the wrong number of choices".into());
                        }
                    }
                },
                _ => {}
            }
        }
        problems
    }
}

/// Implemented by the host to serve `elicitation/create`: show
/// [`ElicitationRequest::message`] and a form built from
/// [`ElicitationRequest::fields`], then report what the user did.
///
/// Closures `Fn(ElicitationRequest) -> impl Future<Output = Result<ElicitationResponse, McpError>>`
/// implement it.
pub trait ElicitationHandler: Send + Sync {
    fn elicit(&self, request: ElicitationRequest) -> BoxFuture<'_, Result<ElicitationResponse, McpError>>;
}

impl<F, Fut> ElicitationHandler for F
where
    F: Fn(ElicitationRequest) -> Fut + Send + Sync,
    Fut: Future<Output = Result<ElicitationResponse, McpError>> + Send + 'static,
{
    fn elicit(&self, request: ElicitationRequest) -> BoxFuture<'_, Result<ElicitationResponse, McpError>> {
        Box::pin(self(request))
    }
}

// ---------------------------------------------------------------------------
// Roots
// ---------------------------------------------------------------------------

/// A directory (or other URI) the server may operate on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Root {
    /// `file:///...` URI.
    pub uri: String,
    pub name: Option<String>,
}

impl Root {
    pub fn new(uri: impl Into<String>, name: Option<String>) -> Root {
        Root { uri: uri.into(), name }
    }

    /// A `file://` root for an absolute path (named after its last
    /// component); `None` for a relative path.
    pub fn from_path(path: &Path) -> Option<Root> {
        let url = reqwest::Url::from_file_path(path).ok()?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        Some(Root { uri: url.to_string(), name })
    }

    pub(crate) fn to_value(&self) -> Value {
        let mut v = json!({"uri": self.uri});
        if let Some(n) = &self.name {
            v["name"] = json!(n);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sampling_request() {
        let params = json!({
            "messages": [
                {"role": "user", "content": {"type": "text", "text": "Hi"}},
                {"role": "assistant", "content": [{"type": "text", "text": "Hello"}]}
            ],
            "modelPreferences": {"hints": [{"name": "claude-3-sonnet"}], "intelligencePriority": 0.8},
            "systemPrompt": "Be brief",
            "maxTokens": 100,
            "temperature": 0.7,
            "stopSequences": ["\n\n"]
        });
        let r = SamplingRequest::parse("srv", &params).unwrap();
        assert_eq!(r.messages.len(), 2);
        assert_eq!(r.messages[1].content, vec![Content::Text { text: "Hello".into() }]);
        assert_eq!(r.model_hints, ["claude-3-sonnet"]);
        assert_eq!((r.max_tokens, r.temperature), (100, Some(0.7)));
        assert_eq!(r.transcript(), "user: Hi\nassistant: Hello");
        assert!(SamplingRequest::parse("srv", &json!({"messages": []})).is_err());
        assert_eq!(
            SamplingResult::text("m", "ok").to_value(),
            json!({"model": "m", "role": "assistant", "content": {"type": "text", "text": "ok"}, "stopReason": "endTurn"})
        );
    }

    #[test]
    fn elicitation_fields_and_validation() {
        let params = json!({
            "message": "Who are you?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "title": "Name", "minLength": 1},
                    "email": {"type": "string", "format": "email"},
                    "age": {"type": "integer", "minimum": 0, "maximum": 150},
                    "ok": {"type": "boolean", "default": true},
                    "color": {"type": "string", "enum": ["r", "g"], "enumNames": ["Red", "Green"]},
                    "size": {"type": "string", "oneOf": [{"const": "s", "title": "Small"}, {"const": "l", "title": "Large"}]},
                    "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}, "maxItems": 1}
                },
                "required": ["name", "age"]
            }
        });
        let r = ElicitationRequest::parse("srv", &params);
        let f = r.fields();
        assert_eq!(
            f.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["name", "email", "age", "ok", "color", "size", "tags"]
        );
        assert!(f[0].required && !f[1].required);
        assert_eq!(f[0].label(), "Name");
        assert!(matches!(&f[1].kind, ElicitFieldKind::Text { format: Some(x), .. } if x == "email"));
        assert_eq!(f[2].kind, ElicitFieldKind::Number { integer: true, minimum: Some(0.0), maximum: Some(150.0) });
        assert_eq!(f[3].default, Some(json!(true)));
        assert_eq!(
            f[4].kind,
            ElicitFieldKind::Choice {
                values: vec!["r".into(), "g".into()],
                labels: vec!["Red".into(), "Green".into()]
            }
        );
        assert_eq!(
            f[5].kind,
            ElicitFieldKind::Choice {
                values: vec!["s".into(), "l".into()],
                labels: vec!["Small".into(), "Large".into()]
            }
        );
        assert!(matches!(&f[6].kind, ElicitFieldKind::MultiChoice { max_items: Some(1), .. }));

        let ok: Map<String, Value> =
            serde_json::from_value(json!({"name": "Ann", "age": 30, "color": "g", "tags": ["a"]})).unwrap();
        assert!(r.validate(&ok).is_empty(), "{:?}", r.validate(&ok));
        let bad: Map<String, Value> =
            serde_json::from_value(json!({"age": 1.5, "color": "x", "tags": ["a", "b"]})).unwrap();
        assert_eq!(r.validate(&bad).len(), 4, "{:?}", r.validate(&bad));
        assert_eq!(ElicitationResponse::Accept(ok).to_value()["action"], json!("accept"));
        assert_eq!(ElicitationResponse::Decline.to_value(), json!({"action": "decline"}));
    }

    #[test]
    fn root_from_path() {
        if cfg!(unix) {
            let r = Root::from_path(Path::new("/tmp/my project")).unwrap();
            assert_eq!(r.uri, "file:///tmp/my%20project");
            assert_eq!(r.name.as_deref(), Some("my project"));
            assert!(Root::from_path(Path::new("relative")).is_none());
        }
    }
}
