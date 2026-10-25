//! MCP data types as seen by the UI, parsed leniently from the wire JSON so a
//! slightly non-conforming server still shows up.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn string_field(v: &Value, key: &str) -> String {
    str_field(v, key).unwrap_or_default()
}

/// What the server reported in its `initialize` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
    /// The negotiated protocol version.
    pub protocol_version: String,
    /// Raw `capabilities` object (`tools`, `resources`, `prompts`, `logging`, ...).
    pub capabilities: Value,
    pub instructions: Option<String>,
}

impl ServerInfo {
    pub(crate) fn from_initialize(result: &Value) -> ServerInfo {
        let info = result.get("serverInfo").cloned().unwrap_or(Value::Null);
        ServerInfo {
            name: string_field(&info, "name"),
            version: string_field(&info, "version"),
            title: str_field(&info, "title"),
            protocol_version: string_field(result, "protocolVersion"),
            capabilities: result.get("capabilities").cloned().unwrap_or_else(|| Value::Object(Map::new())),
            instructions: str_field(result, "instructions").filter(|s| !s.is_empty()),
        }
    }

    /// Whether the server advertised a capability (`"tools"`, `"resources"`,
    /// `"prompts"`, `"logging"`, `"completions"`).
    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.get(name).is_some_and(|v| !v.is_null())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// JSON Schema of the arguments (always an object schema).
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    /// `readOnlyHint`, `destructiveHint`, ... as sent by the server.
    pub annotations: Option<Value>,
}

impl Tool {
    pub fn from_value(v: &Value) -> Tool {
        let annotations = v.get("annotations").filter(|a| a.is_object()).cloned();
        let title = str_field(v, "title")
            .or_else(|| annotations.as_ref().and_then(|a| str_field(a, "title")))
            .filter(|s| !s.is_empty());
        let input_schema = v
            .get("inputSchema")
            .filter(|s| s.is_object())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
        Tool {
            name: string_field(v, "name"),
            title,
            description: str_field(v, "description").filter(|s| !s.is_empty()),
            input_schema,
            output_schema: v.get("outputSchema").filter(|s| s.is_object()).cloned(),
            annotations,
        }
    }

    /// Title if present, else the name.
    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

/// One item of a tool result or prompt message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Content {
    Text {
        text: String,
    },
    /// Base64 data.
    Image {
        data: String,
        mime_type: String,
    },
    Audio {
        data: String,
        mime_type: String,
    },
    /// A link to a resource the client may read.
    ResourceLink {
        uri: String,
        name: Option<String>,
        mime_type: Option<String>,
        description: Option<String>,
    },
    /// An embedded resource with `text` or base64 `blob` contents.
    Resource {
        uri: String,
        mime_type: Option<String>,
        text: Option<String>,
        blob: Option<String>,
    },
    /// Anything this client does not know yet.
    Other(Value),
}

impl Content {
    pub fn from_value(v: &Value) -> Content {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => Content::Text { text: string_field(v, "text") },
            "image" => Content::Image { data: string_field(v, "data"), mime_type: string_field(v, "mimeType") },
            "audio" => Content::Audio { data: string_field(v, "data"), mime_type: string_field(v, "mimeType") },
            "resource_link" => Content::ResourceLink {
                uri: string_field(v, "uri"),
                name: str_field(v, "name"),
                mime_type: str_field(v, "mimeType"),
                description: str_field(v, "description"),
            },
            "resource" => {
                let r = v.get("resource").cloned().unwrap_or(Value::Null);
                Content::Resource {
                    uri: string_field(&r, "uri"),
                    mime_type: str_field(&r, "mimeType"),
                    text: str_field(&r, "text"),
                    blob: str_field(&r, "blob"),
                }
            }
            _ => Content::Other(v.clone()),
        }
    }

    /// The wire form (`{"type": "text", "text": ...}`, ...).
    pub fn to_value(&self) -> Value {
        let mut v = match self {
            Content::Text { text } => serde_json::json!({"type": "text", "text": text}),
            Content::Image { data, mime_type } => {
                serde_json::json!({"type": "image", "data": data, "mimeType": mime_type})
            }
            Content::Audio { data, mime_type } => {
                serde_json::json!({"type": "audio", "data": data, "mimeType": mime_type})
            }
            Content::ResourceLink { uri, .. } => serde_json::json!({"type": "resource_link", "uri": uri}),
            Content::Resource { uri, mime_type, text, blob } => {
                let mut r = serde_json::json!({"uri": uri});
                for (k, x) in [("mimeType", mime_type), ("text", text), ("blob", blob)] {
                    if let Some(x) = x {
                        r[k] = Value::String(x.clone());
                    }
                }
                return serde_json::json!({"type": "resource", "resource": r});
            }
            Content::Other(v) => return v.clone(),
        };
        if let Content::ResourceLink { name, mime_type, description, .. } = self {
            for (k, x) in [("name", name), ("mimeType", mime_type), ("description", description)] {
                if let Some(x) = x {
                    v[k] = Value::String(x.clone());
                }
            }
        }
        v
    }

    /// Human readable one-line-or-more rendering (binary data is summarized).
    pub fn to_display_string(&self) -> String {
        match self {
            Content::Text { text } => text.clone(),
            Content::Image { data, mime_type } => format!("[image {mime_type}, {} bytes base64]", data.len()),
            Content::Audio { data, mime_type } => format!("[audio {mime_type}, {} bytes base64]", data.len()),
            Content::ResourceLink { uri, name, .. } => match name {
                Some(n) => format!("[resource link {n}: {uri}]"),
                None => format!("[resource link {uri}]"),
            },
            Content::Resource { uri, text: Some(t), .. } => format!("[resource {uri}]\n{t}"),
            Content::Resource { uri, mime_type, blob, .. } => format!(
                "[resource {uri}{}{}]",
                mime_type.as_deref().map(|m| format!(", {m}")).unwrap_or_default(),
                blob.as_ref().map(|b| format!(", {} bytes base64", b.len())).unwrap_or_default()
            ),
            Content::Other(v) => v.to_string(),
        }
    }
}

/// Result of `tools/call`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CallResult {
    pub content: Vec<Content>,
    /// `structuredContent`, when the tool declares an output schema.
    pub structured: Option<Value>,
    /// The tool reported a failure (`isError: true`); not a protocol error.
    pub is_error: bool,
    /// The complete `result` object.
    pub raw: Value,
}

impl CallResult {
    pub fn from_value(v: Value) -> CallResult {
        let content = v
            .get("content")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(Content::from_value).collect())
            .unwrap_or_default();
        CallResult {
            content,
            structured: v.get("structuredContent").filter(|s| !s.is_null()).cloned(),
            is_error: v.get("isError").and_then(Value::as_bool).unwrap_or(false),
            raw: v,
        }
    }

    /// All content rendered as text (what an AI model gets fed): the text
    /// items joined by newlines, falling back to the structured result.
    pub fn text(&self) -> String {
        let parts: Vec<String> = self.content.iter().map(Content::to_display_string).collect();
        if parts.is_empty() {
            return match &self.structured {
                Some(s) => s.to_string(),
                None => String::new(),
            };
        }
        parts.join("\n")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
}

impl Resource {
    pub fn from_value(v: &Value) -> Resource {
        Resource {
            uri: string_field(v, "uri"),
            name: string_field(v, "name"),
            title: str_field(v, "title"),
            description: str_field(v, "description"),
            mime_type: str_field(v, "mimeType"),
            size: v.get("size").and_then(Value::as_u64),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceTemplate {
    /// RFC 6570 template, e.g. `file:///{path}`.
    pub uri_template: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

impl ResourceTemplate {
    pub fn from_value(v: &Value) -> ResourceTemplate {
        ResourceTemplate {
            uri_template: string_field(v, "uriTemplate"),
            name: string_field(v, "name"),
            title: str_field(v, "title"),
            description: str_field(v, "description"),
            mime_type: str_field(v, "mimeType"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptArgument {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub arguments: Vec<PromptArgument>,
}

impl Prompt {
    pub fn from_value(v: &Value) -> Prompt {
        let arguments = v
            .get("arguments")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|x| PromptArgument {
                        name: string_field(x, "name"),
                        title: str_field(x, "title"),
                        description: str_field(x, "description"),
                        required: x.get("required").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Prompt {
            name: string_field(v, "name"),
            title: str_field(v, "title"),
            description: str_field(v, "description"),
            arguments,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    /// Client to server.
    Outgoing,
    /// Server to client.
    Incoming,
}

/// Everything that happens on a connection, for the UI log. Delivered from a
/// runtime thread.
#[derive(Clone, Debug, PartialEq)]
pub enum McpEvent {
    /// `notifications/message` from the server (level as sent: `info`, `error`, ...),
    /// plus client-side warnings (level `warning`).
    Log { level: String, message: String },
    /// A line the stdio server wrote to stderr.
    Stderr(String),
    /// Any other server notification (`notifications/progress`, `.../list_changed`, ...).
    Notification { method: String, params: Value },
    /// Every JSON-RPC message, verbatim.
    Traffic { direction: Direction, json: String, at_ms: i64 },
    /// The connection ended (reason). Sent once.
    Closed(String),
}

/// Target format of [`crate::export_config`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExportStyle {
    /// `{"mcpServers": {name: {command, args, env} | {url, headers}}}` as used
    /// by Cursor, Windsurf, Claude Code (`.mcp.json`, with `type`) and others.
    #[default]
    McpServers,
    /// `claude_desktop_config.json`: stdio only, so remote servers are wrapped
    /// with the `mcp-remote` bridge.
    ClaudeDesktop,
    /// VS Code `.vscode/mcp.json`: `{"servers": {name: {type, ...}}, "inputs": [...]}`.
    VsCode,
}

impl ExportStyle {
    pub const ALL: [ExportStyle; 3] = [ExportStyle::McpServers, ExportStyle::ClaudeDesktop, ExportStyle::VsCode];

    pub fn label(self) -> &'static str {
        match self {
            ExportStyle::McpServers => "Cursor / Claude Code (mcpServers)",
            ExportStyle::ClaudeDesktop => "Claude Desktop",
            ExportStyle::VsCode => "VS Code (mcp.json)",
        }
    }
}
