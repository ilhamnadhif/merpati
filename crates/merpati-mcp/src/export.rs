//! Server config snippets for other MCP hosts, and argument templates.

use merpati_core::auth::AuthType;
use merpati_core::model::KeyValue;
use merpati_core::protocols::{McpHttpTransport, McpRequest, McpTransport};
use serde_json::{Map, Value, json};

use crate::types::ExportStyle;

fn pairs(rows: &[KeyValue]) -> Vec<(String, String)> {
    rows.iter()
        .filter(|kv| kv.enabled && !kv.key.trim().is_empty())
        .map(|kv| (kv.key.trim().to_string(), kv.value.clone()))
        .collect()
}

fn object(pairs: &[(String, String)]) -> Map<String, Value> {
    pairs.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()
}

/// Header from the request auth (Bearer or API key in a header), if any.
fn auth_header(req: &McpRequest) -> Option<(String, String)> {
    match req.auth.kind {
        AuthType::Bearer => {
            let token = req.auth.get("token").trim();
            (!token.is_empty()).then(|| ("Authorization".to_string(), format!("Bearer {token}")))
        }
        AuthType::ApiKey if req.auth.get("in") != "query" => {
            let key = req.auth.get("key").trim();
            (!key.is_empty()).then(|| (key.to_string(), req.auth.get("value").to_string()))
        }
        _ => None,
    }
}

fn env_name(header: &str) -> String {
    let mut s: String =
        header.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    s.insert_str(0, "MCP_HEADER_");
    s
}

/// JSON snippet registering this server in another MCP host. Values are
/// exported as stored (variables such as `{{token}}` are not resolved), except
/// that the VS Code style asks for a Bearer token through a password input
/// instead of embedding it. An explicit HTTP+SSE transport exports as
/// `"type": "sse"`; OAuth sign-in is left to the target host, except that a
/// pre-registered client id (and its callback port) is passed to
/// `mcp-remote` for Claude Desktop. Client secrets are never exported.
pub fn export_config(name: &str, req: &McpRequest, style: ExportStyle) -> Value {
    let name = if name.trim().is_empty() { "merpati-server".to_string() } else { name.trim().to_string() };
    let mut inputs: Vec<Value> = Vec::new();
    let server = match &req.transport {
        McpTransport::Stdio { command, args, env, cwd } => {
            let mut s = Map::new();
            if style == ExportStyle::VsCode {
                s.insert("type".into(), json!("stdio"));
            }
            s.insert("command".into(), json!(command.trim()));
            s.insert("args".into(), json!(args));
            let env = pairs(env);
            if !env.is_empty() {
                s.insert("env".into(), Value::Object(object(&env)));
            }
            if !cwd.trim().is_empty() && style != ExportStyle::ClaudeDesktop {
                s.insert("cwd".into(), json!(cwd.trim()));
            }
            s
        }
        McpTransport::Http { url, headers } => {
            let mut headers = pairs(headers);
            let has_auth = |h: &[(String, String)], k: &str| h.iter().any(|(n, _)| n.eq_ignore_ascii_case(k));
            if let Some((k, v)) = auth_header(req)
                && !has_auth(&headers, &k)
            {
                if style == ExportStyle::VsCode && req.auth.kind == AuthType::Bearer {
                    let id = format!("{name}-token");
                    inputs.push(json!({
                        "type": "promptString",
                        "id": id,
                        "description": format!("Bearer token for {name}"),
                        "password": true
                    }));
                    headers.push((k, format!("Bearer ${{input:{id}}}")));
                } else {
                    headers.push((k, v));
                }
            }
            let mut s = Map::new();
            match style {
                ExportStyle::ClaudeDesktop => {
                    // Claude Desktop's config file only launches local
                    // processes; `mcp-remote` bridges stdio to HTTP.
                    let mut args = vec![json!("-y"), json!("mcp-remote"), json!(url.trim())];
                    let oauth_client = (req.auth.kind == AuthType::OAuth2)
                        .then(|| req.auth.get("clientId").trim())
                        .filter(|c| !c.is_empty());
                    if oauth_client.is_some()
                        && let Some(port) =
                            url::Url::parse(req.auth.get("redirect_uri").trim()).ok().and_then(|u| u.port())
                    {
                        // mcp-remote's optional positional callback port.
                        args.push(json!(port.to_string()));
                    }
                    match req.http_transport {
                        McpHttpTransport::Sse => args.extend([json!("--transport"), json!("sse-only")]),
                        McpHttpTransport::Streamable => args.extend([json!("--transport"), json!("http-only")]),
                        McpHttpTransport::Auto => {}
                    }
                    if let Some(client_id) = oauth_client {
                        args.push(json!("--static-oauth-client-info"));
                        args.push(json!(json!({"client_id": client_id}).to_string()));
                    }
                    let mut env = Map::new();
                    for (k, v) in &headers {
                        let var = env_name(k);
                        args.push(json!("--header"));
                        args.push(json!(format!("{k}:${{{var}}}")));
                        env.insert(var, json!(v));
                    }
                    s.insert("command".into(), json!("npx"));
                    s.insert("args".into(), Value::Array(args));
                    if !env.is_empty() {
                        s.insert("env".into(), Value::Object(env));
                    }
                }
                ExportStyle::McpServers | ExportStyle::VsCode => {
                    let kind = if req.http_transport == McpHttpTransport::Sse { "sse" } else { "http" };
                    s.insert("type".into(), json!(kind));
                    s.insert("url".into(), json!(url.trim()));
                    if !headers.is_empty() {
                        s.insert("headers".into(), Value::Object(object(&headers)));
                    }
                }
            }
            s
        }
    };
    let mut servers = Map::new();
    servers.insert(name, Value::Object(server));
    match style {
        ExportStyle::VsCode => {
            let mut root = Map::new();
            if !inputs.is_empty() {
                root.insert("inputs".into(), Value::Array(inputs));
            }
            root.insert("servers".into(), Value::Object(servers));
            Value::Object(root)
        }
        ExportStyle::McpServers | ExportStyle::ClaudeDesktop => json!({"mcpServers": servers}),
    }
}

/// An arguments object for a tool's input schema: every required property
/// with a typed placeholder (its `default`, `const`, first `examples` or
/// `enum` value when given; else `""`, `0`, `false`, `[]`, `{...}` or `null`).
pub fn args_template(schema: &Value) -> Value {
    match template(schema, schema, 0) {
        Value::Object(m) => Value::Object(m),
        _ => Value::Object(Map::new()),
    }
}

fn resolve_ref<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    let mut s = schema;
    for _ in 0..16 {
        match s.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix('#')) {
            Some(pointer) => match root.pointer(pointer) {
                Some(target) => s = target,
                None => return s,
            },
            None => return s,
        }
    }
    s
}

fn schema_type(s: &Value) -> Option<&str> {
    match s.get("type") {
        Some(Value::String(t)) => Some(t),
        Some(Value::Array(types)) => {
            types.iter().filter_map(Value::as_str).find(|t| *t != "null").or(types.first().and_then(Value::as_str))
        }
        _ if s.get("properties").is_some() => Some("object"),
        _ if s.get("items").is_some() => Some("array"),
        _ => None,
    }
}

fn template(schema: &Value, root: &Value, depth: usize) -> Value {
    if depth > 12 {
        return Value::Null;
    }
    let s = resolve_ref(schema, root);
    if let Some(v) = s.get("default") {
        return v.clone();
    }
    if let Some(v) = s.get("const") {
        return v.clone();
    }
    for key in ["examples", "enum"] {
        if let Some(v) = s.get(key).and_then(Value::as_array).and_then(|a| a.first()) {
            return v.clone();
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(options) = s.get(key).and_then(Value::as_array) {
            let pick =
                options.iter().find(|o| schema_type(resolve_ref(o, root)) != Some("null")).or_else(|| options.first());
            if let Some(o) = pick {
                return template(o, root, depth + 1);
            }
        }
    }
    if let Some(parts) = s.get("allOf").and_then(Value::as_array) {
        let mut merged = Map::new();
        let mut last = Value::Null;
        for p in parts {
            match template(p, root, depth + 1) {
                Value::Object(m) => merged.extend(m),
                other => last = other,
            }
        }
        if !merged.is_empty() || schema_type(s) == Some("object") {
            if let Value::Object(own) = object_template(s, root, depth) {
                merged.extend(own);
            }
            return Value::Object(merged);
        }
        return last;
    }
    match schema_type(s) {
        Some("object") => object_template(s, root, depth),
        Some("array") => {
            let min = s.get("minItems").and_then(Value::as_u64).unwrap_or(0);
            match s.get("items") {
                Some(items) if min > 0 => Value::Array(vec![template(items, root, depth + 1)]),
                _ => Value::Array(Vec::new()),
            }
        }
        Some("string") => Value::String(String::new()),
        Some("integer") => s.get("minimum").and_then(Value::as_i64).map(Value::from).unwrap_or_else(|| json!(0)),
        Some("number") => s.get("minimum").cloned().filter(Value::is_number).unwrap_or_else(|| json!(0)),
        Some("boolean") => Value::Bool(false),
        _ => Value::Null,
    }
}

fn object_template(s: &Value, root: &Value, depth: usize) -> Value {
    let props = s.get("properties").and_then(Value::as_object);
    let mut out = Map::new();
    if let Some(required) = s.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            let value = props.and_then(|p| p.get(key)).map(|p| template(p, root, depth + 1)).unwrap_or(Value::Null);
            out.insert(key.to_string(), value);
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::auth::Auth;

    #[test]
    fn template_fills_required_fields() {
        let schema = json!({
            "type": "object",
            "properties": {
                "q": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1},
                "mode": {"enum": ["fast", "slow"]},
                "flag": {"type": ["boolean", "null"]},
                "opt": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "filter": {"$ref": "#/$defs/Filter"},
                "when": {"anyOf": [{"type": "null"}, {"type": "number"}]},
                "named": {"type": "string", "default": "x"}
            },
            "required": ["q", "limit", "mode", "flag", "tags", "filter", "when", "named"],
            "$defs": {"Filter": {"type": "object", "properties": {"field": {"type": "string"}}, "required": ["field"]}}
        });
        assert_eq!(
            args_template(&schema),
            json!({"q": "", "limit": 1, "mode": "fast", "flag": false, "tags": [], "filter": {"field": ""}, "when": 0, "named": "x"})
        );
        assert_eq!(args_template(&json!({"type": "object"})), json!({}));
        assert_eq!(args_template(&json!(null)), json!({}));
    }

    #[test]
    fn export_styles() {
        let stdio = McpRequest {
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@modelcontextprotocol/server-everything".into()],
                env: vec![KeyValue::new("DEBUG", "1")],
                cwd: String::new(),
            },
            ..Default::default()
        };
        assert_eq!(
            export_config("everything", &stdio, ExportStyle::McpServers),
            json!({"mcpServers": {"everything": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-everything"], "env": {"DEBUG": "1"}}}})
        );
        assert_eq!(
            export_config("everything", &stdio, ExportStyle::VsCode)["servers"]["everything"]["type"],
            json!("stdio")
        );

        let http = McpRequest {
            transport: McpTransport::Http { url: "https://api.example.com/mcp".into(), headers: vec![] },
            auth: Auth::bearer("{{token}}"),
            ..Default::default()
        };
        assert_eq!(
            export_config("remote", &http, ExportStyle::McpServers),
            json!({"mcpServers": {"remote": {"type": "http", "url": "https://api.example.com/mcp", "headers": {"Authorization": "Bearer {{token}}"}}}})
        );
        let vs = export_config("remote", &http, ExportStyle::VsCode);
        assert_eq!(vs["servers"]["remote"]["headers"]["Authorization"], json!("Bearer ${input:remote-token}"));
        assert_eq!(vs["inputs"][0]["password"], json!(true));
        let cd = export_config("remote", &http, ExportStyle::ClaudeDesktop);
        assert_eq!(
            cd["mcpServers"]["remote"]["args"],
            json!([
                "-y",
                "mcp-remote",
                "https://api.example.com/mcp",
                "--header",
                "Authorization:${MCP_HEADER_AUTHORIZATION}"
            ])
        );
        assert_eq!(cd["mcpServers"]["remote"]["env"]["MCP_HEADER_AUTHORIZATION"], json!("Bearer {{token}}"));

        let sse = McpRequest {
            transport: McpTransport::Http { url: "https://api.example.com/sse".into(), headers: vec![] },
            auth: merpati_core::auth::Auth::new(AuthType::OAuth2)
                .with("clientId", "cid")
                .with("clientSecret", "never-exported")
                .with("redirect_uri", "http://127.0.0.1:33418/callback"),
            http_transport: McpHttpTransport::Sse,
            ..Default::default()
        };
        assert_eq!(
            export_config("legacy", &sse, ExportStyle::McpServers),
            json!({"mcpServers": {"legacy": {"type": "sse", "url": "https://api.example.com/sse"}}})
        );
        let cd = export_config("legacy", &sse, ExportStyle::ClaudeDesktop);
        assert_eq!(
            cd["mcpServers"]["legacy"]["args"],
            json!([
                "-y",
                "mcp-remote",
                "https://api.example.com/sse",
                "33418",
                "--transport",
                "sse-only",
                "--static-oauth-client-info",
                "{\"client_id\":\"cid\"}"
            ])
        );
        assert!(!cd.to_string().contains("never-exported"));
    }
}
