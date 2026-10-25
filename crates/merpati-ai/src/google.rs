//! Google Gemini API: `models/{model}:generateContent` and
//! `:streamGenerateContent?alt=sse`.

use merpati_mcp::sse::SseEvent;
use serde_json::{Map, Value, json};

use crate::{
    AiConfig, AiError, HttpReq, Provider, Round, StreamAcc, ToolCallReq, ToolOutcome, ToolSpec, merge_json, num_f32,
    parse_arguments, str_of,
};

/// Prefix of call ids made up locally (Gemini only sometimes sends ids);
/// such ids are not echoed back in `functionResponse`.
const LOCAL_ID: &str = "merpati_call_";

pub(crate) struct Google;

/// `{base}/v1beta` unless the base already names an API version.
pub(crate) fn versioned(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let last = base.rsplit('/').next().unwrap_or("");
    if last.starts_with("v1") { base.to_string() } else { format!("{base}/v1beta") }
}

pub(crate) fn auth_headers(key: Option<&str>) -> Vec<(&'static str, String, bool)> {
    key.map(|k| vec![("x-goog-api-key", k.to_string(), true)]).unwrap_or_default()
}

/// Gemini accepts an OpenAPI subset for function parameters; drop what it
/// rejects (`$schema`, `additionalProperties`, unknown formats, ...), inline
/// `$ref`s and turn `["string", "null"]` into `nullable`.
pub(crate) fn gemini_schema(schema: &Value, root: &Value, depth: usize) -> Value {
    reduce(schema, root, depth, &mut vec![schema])
}

/// `expanding` holds the schemas being expanded on the current path. The
/// subset has no references, so a recursive type is cut where it refers
/// back to itself: expanding it again would grow exponentially with every
/// recursive property.
fn reduce<'a>(schema: &'a Value, root: &'a Value, depth: usize, expanding: &mut Vec<&'a Value>) -> Value {
    let before = expanding.len();
    let mut s = schema;
    for _ in 0..16 {
        match s.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix('#')).and_then(|p| root.pointer(p)) {
            Some(target) if expanding.iter().any(|e| std::ptr::eq(*e, target)) => {
                expanding.truncate(before);
                return json!({"type": "string"});
            }
            Some(target) => {
                expanding.push(target);
                s = target;
            }
            None => break,
        }
    }
    let out = reduce_resolved(s, root, depth, expanding);
    expanding.truncate(before);
    out
}

fn reduce_resolved<'a>(s: &'a Value, root: &'a Value, depth: usize, expanding: &mut Vec<&'a Value>) -> Value {
    let Some(obj) = s.as_object() else { return json!({}) };
    if depth > 16 {
        return json!({"type": "string"});
    }
    let mut out = Map::new();
    match obj.get("type") {
        Some(Value::String(t)) => {
            out.insert("type".into(), json!(t));
        }
        Some(Value::Array(types)) => {
            let non_null: Vec<&str> = types.iter().filter_map(Value::as_str).filter(|t| *t != "null").collect();
            if let Some(t) = non_null.first() {
                out.insert("type".into(), json!(t));
            }
            if types.iter().any(|t| t.as_str() == Some("null")) {
                out.insert("nullable".into(), json!(true));
            }
        }
        _ => {}
    }
    let ty = out.get("type").and_then(Value::as_str).unwrap_or("").to_string();
    for key in [
        "description",
        "title",
        "nullable",
        "minItems",
        "maxItems",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "pattern",
        "minProperties",
        "maxProperties",
    ] {
        if let Some(v) = obj.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(Value::String(f)) = obj.get("format") {
        let ok = match ty.as_str() {
            "string" => matches!(f.as_str(), "enum" | "date-time"),
            "number" => matches!(f.as_str(), "float" | "double"),
            "integer" => matches!(f.as_str(), "int32" | "int64"),
            _ => false,
        };
        if ok {
            out.insert("format".into(), json!(f));
        }
    }
    let enum_values =
        obj.get("enum").and_then(Value::as_array).cloned().or_else(|| obj.get("const").map(|c| vec![c.clone()]));
    if let Some(values) = enum_values {
        let strings: Vec<Value> = values
            .iter()
            .filter(|v| !v.is_null())
            .map(|v| match v {
                Value::String(s) => json!(s),
                other => json!(other.to_string()),
            })
            .collect();
        if !strings.is_empty() {
            out.insert("enum".into(), Value::Array(strings));
            out.insert("type".into(), json!("string"));
        }
    }
    if let Some(Value::Object(props)) = obj.get("properties") {
        let mapped: Map<String, Value> =
            props.iter().map(|(k, v)| (k.clone(), reduce(v, root, depth + 1, expanding))).collect();
        if let Some(Value::Array(req)) = obj.get("required") {
            let req: Vec<Value> =
                req.iter().filter(|r| r.as_str().is_some_and(|r| mapped.contains_key(r))).cloned().collect();
            if !req.is_empty() {
                out.insert("required".into(), Value::Array(req));
            }
        }
        if !mapped.is_empty() {
            out.insert("properties".into(), Value::Object(mapped));
        }
        out.entry("type").or_insert(json!("object"));
    }
    if let Some(items) = obj.get("items") {
        out.insert("items".into(), reduce(items, root, depth + 1, expanding));
        out.entry("type").or_insert(json!("array"));
    } else if ty == "array" {
        out.insert("items".into(), json!({"type": "string"}));
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(options)) = obj.get(key) {
            let opts: Vec<&Value> =
                options.iter().filter(|o| o.get("type").and_then(Value::as_str) != Some("null")).collect();
            if options.len() != opts.len() {
                out.insert("nullable".into(), json!(true));
            }
            match opts.as_slice() {
                [one] => {
                    let mut inner = match reduce(one, root, depth + 1, expanding) {
                        Value::Object(m) => m,
                        _ => Map::new(),
                    };
                    inner.extend(out);
                    out = inner;
                }
                [] => {}
                many => {
                    out.insert("anyOf".into(), many.iter().map(|o| reduce(o, root, depth + 1, expanding)).collect());
                }
            }
        }
    }
    if let Some(Value::Array(parts)) = obj.get("allOf") {
        for p in parts {
            if let Value::Object(m) = reduce(p, root, depth + 1, expanding) {
                for (k, v) in m {
                    out.entry(k).or_insert(v);
                }
            }
        }
    }
    if !out.contains_key("type") && !out.contains_key("anyOf") {
        out.insert("type".into(), json!("string"));
    }
    Value::Object(out)
}

fn usage(v: &Value) -> (Option<u64>, Option<u64>) {
    let u = v.get("usageMetadata").filter(|u| u.is_object());
    let field = |k: &str| u.and_then(|u| u.get(k)).and_then(Value::as_u64);
    let output = match (field("candidatesTokenCount"), field("thoughtsTokenCount")) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    };
    (field("promptTokenCount"), output)
}

fn blocked(v: &Value) -> Option<AiError> {
    let reason = v.pointer("/promptFeedback/blockReason").and_then(Value::as_str)?;
    Some(AiError::Api(format!("The prompt was blocked by Gemini ({reason})")))
}

fn is_plain_text(p: &Value) -> bool {
    p.as_object().is_some_and(|m| m.len() == 1 && m.contains_key("text"))
}

fn round_from_parts(
    parts: Vec<Value>,
    finish: Option<String>,
    model: Option<String>,
    usage_v: &Value,
    raw: Value,
) -> Round {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for p in &parts {
        if let Some(t) = p.get("text").and_then(Value::as_str)
            && p.get("thought").and_then(Value::as_bool) != Some(true)
        {
            text.push_str(t);
        }
        if let Some(fc) = p.get("functionCall") {
            let (arguments, invalid) = parse_arguments(fc.get("args").unwrap_or(&Value::Null));
            tool_calls.push(ToolCallReq {
                id: str_of(fc, "id")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("{LOCAL_ID}{}", tool_calls.len())),
                name: str_of(fc, "name").unwrap_or_default(),
                arguments,
                invalid,
            });
        }
    }
    let (input_tokens, output_tokens) = usage(usage_v);
    Round {
        text,
        tool_calls,
        finish_reason: finish,
        model,
        input_tokens,
        output_tokens,
        raw,
        assistant: json!({"role": "model", "parts": parts}),
    }
}

impl Provider for Google {
    fn messages(&self, cfg: &AiConfig) -> Vec<Value> {
        cfg.messages
            .iter()
            .filter(|m| m.role != "system" && !m.content.trim().is_empty())
            .map(|m| json!({"role": if m.role == "assistant" { "model" } else { "user" }, "parts": [{"text": m.content}]}))
            .collect()
    }

    fn build(&self, cfg: &AiConfig, messages: &[Value], tools: &[ToolSpec], stream: bool) -> HttpReq {
        let model = cfg.model.trim().trim_start_matches("models/");
        let base = versioned(&cfg.base_url);
        let url = if stream {
            format!("{base}/models/{model}:streamGenerateContent?alt=sse")
        } else {
            format!("{base}/models/{model}:generateContent")
        };
        let mut body = json!({"contents": messages});
        let mut system: Vec<&str> = cfg.system.as_deref().into_iter().collect();
        system.extend(
            cfg.messages
                .iter()
                .filter(|m| m.role == "system" && !m.content.trim().is_empty())
                .map(|m| m.content.as_str()),
        );
        if !system.is_empty() {
            body["systemInstruction"] = json!({"parts": [{"text": system.join("\n\n")}]});
        }
        let mut generation = Map::new();
        if let Some(t) = cfg.temperature {
            generation.insert("temperature".into(), num_f32(t));
        }
        if let Some(m) = cfg.max_tokens {
            generation.insert("maxOutputTokens".into(), json!(m));
        }
        if !generation.is_empty() {
            body["generationConfig"] = Value::Object(generation);
        }
        if !tools.is_empty() {
            let decls: Vec<Value> = tools
                .iter()
                .map(|t| {
                    let mut d = json!({"name": t.name, "description": t.description});
                    let params = gemini_schema(&t.schema, &t.schema, 0);
                    // An object without properties is rejected; omit it.
                    if params.get("properties").is_some() {
                        d["parameters"] = params;
                    }
                    d
                })
                .collect();
            body["tools"] = json!([{"functionDeclarations": decls}]);
        }
        if let Some(extra) = &cfg.extra {
            merge_json(&mut body, extra);
        }
        HttpReq { url, headers: auth_headers(cfg.api_key.as_deref()), body }
    }

    fn parse(&self, body: Value) -> Result<Round, AiError> {
        let Some(candidate) = body.get("candidates").and_then(Value::as_array).and_then(|c| c.first()).cloned() else {
            if let Some(e) = blocked(&body) {
                return Err(e);
            }
            if let Some(msg) = body.pointer("/error/message").and_then(Value::as_str) {
                return Err(AiError::Api(msg.to_string()));
            }
            return Err(AiError::Parse("the response has no candidates".into()));
        };
        let parts = candidate.pointer("/content/parts").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(round_from_parts(
            parts,
            str_of(&candidate, "finishReason"),
            str_of(&body, "modelVersion"),
            &body,
            body.clone(),
        ))
    }

    fn stream(&self) -> Box<dyn StreamAcc> {
        Box::new(GoogleStream::default())
    }

    fn tool_results(&self, outcomes: &[ToolOutcome]) -> Vec<Value> {
        let parts: Vec<Value> = outcomes
            .iter()
            .map(|o| {
                let response = if o.is_error { json!({"error": o.text}) } else { json!({"result": o.text}) };
                let mut fr = json!({"name": o.name, "response": response});
                if !o.call_id.starts_with(LOCAL_ID) {
                    fr["id"] = json!(o.call_id);
                }
                json!({"functionResponse": fr})
            })
            .collect();
        vec![json!({"role": "user", "parts": parts})]
    }
}

#[derive(Default)]
struct GoogleStream {
    parts: Vec<Value>,
    finish_reason: Option<String>,
    model: Option<String>,
    usage: Value,
    response_id: Option<String>,
}

impl StreamAcc for GoogleStream {
    fn event(&mut self, ev: &SseEvent, delta: &mut dyn FnMut(&str)) -> Result<(), AiError> {
        let data = ev.data.trim();
        if data.is_empty() {
            return Ok(());
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| AiError::Parse(format!("invalid stream chunk ({e}): {data}")))?;
        let v = match v {
            Value::Array(mut a) if !a.is_empty() => a.remove(0),
            other => other,
        };
        if let Some(msg) = v.pointer("/error/message").and_then(Value::as_str) {
            return Err(AiError::Api(msg.to_string()));
        }
        if v.get("candidates").is_none()
            && let Some(e) = blocked(&v)
        {
            return Err(e);
        }
        if let Some(m) = str_of(&v, "modelVersion") {
            self.model = Some(m);
        }
        if let Some(id) = str_of(&v, "responseId") {
            self.response_id = Some(id);
        }
        if v.get("usageMetadata").is_some() {
            self.usage = json!({"usageMetadata": v["usageMetadata"]});
        }
        if let Some(c) = v.get("candidates").and_then(Value::as_array).and_then(|c| c.first()) {
            for p in c.pointer("/content/parts").and_then(Value::as_array).into_iter().flatten() {
                if let Some(t) = p.get("text").and_then(Value::as_str)
                    && p.get("thought").and_then(Value::as_bool) != Some(true)
                {
                    delta(t);
                }
                match self.parts.last_mut() {
                    Some(last) if is_plain_text(p) && is_plain_text(last) => {
                        let joined =
                            format!("{}{}", last["text"].as_str().unwrap_or(""), p["text"].as_str().unwrap_or(""));
                        last["text"] = Value::String(joined);
                    }
                    _ => self.parts.push(p.clone()),
                }
            }
            if let Some(r) = str_of(c, "finishReason") {
                self.finish_reason = Some(r);
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Round, AiError> {
        let raw = json!({
            "candidates": [{"content": {"role": "model", "parts": self.parts}, "finishReason": self.finish_reason}],
            "usageMetadata": self.usage.get("usageMetadata").cloned().unwrap_or(Value::Null),
            "modelVersion": self.model,
            "responseId": self.response_id,
        });
        Ok(round_from_parts(self.parts, self.finish_reason, self.model, &self.usage, raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_reduced_to_the_supported_subset() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "url": {"type": "string", "format": "uri"},
                "when": {"type": "string", "format": "date-time"},
                "n": {"type": ["integer", "null"]},
                "mode": {"enum": ["a", "b"]},
                "level": {"enum": [1, 2]},
                "tags": {"type": "array", "items": {"$ref": "#/definitions/Tag"}},
                "opt": {"anyOf": [{"type": "string"}, {"type": "null"}]}
            },
            "required": ["url", "ghost"],
            "definitions": {"Tag": {"type": "object", "properties": {"k": {"type": "string"}}}}
        });
        let out = gemini_schema(&schema, &schema, 0);
        assert_eq!(
            out,
            json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": {"type": "string"},
                    "when": {"type": "string", "format": "date-time"},
                    "n": {"type": "integer", "nullable": true},
                    "mode": {"type": "string", "enum": ["a", "b"]},
                    "level": {"type": "string", "enum": ["1", "2"]},
                    "tags": {"type": "array", "items": {"type": "object", "properties": {"k": {"type": "string"}}}},
                    "opt": {"type": "string", "nullable": true}
                }
            })
        );
    }

    #[test]
    fn recursive_schemas_are_cut_at_the_first_repetition() {
        // A binary tree: two recursive references per level.
        let tree = json!({
            "type": "object",
            "properties": {"root": {"$ref": "#/$defs/Node"}},
            "$defs": {"Node": {"type": "object", "properties": {
                "value": {"type": "integer"},
                "left": {"$ref": "#/$defs/Node"},
                "right": {"$ref": "#/$defs/Node"}
            }}}
        });
        let out = gemini_schema(&tree, &tree, 0);
        assert_eq!(
            out,
            json!({"type": "object", "properties": {"root": {"type": "object", "properties": {
                "value": {"type": "integer"},
                "left": {"type": "string"},
                "right": {"type": "string"}
            }}}})
        );
        // Three recursive references per level (and / or / not): about 3^16
        // expansions before the cut.
        let filter = json!({
            "type": "object",
            "properties": {"where": {"$ref": "#/$defs/F"}},
            "$defs": {"F": {"type": "object", "properties": {
                "and": {"type": "array", "items": {"$ref": "#/$defs/F"}},
                "or": {"type": "array", "items": {"$ref": "#/$defs/F"}},
                "not": {"$ref": "#/$defs/F"}
            }}}
        });
        let out = gemini_schema(&filter, &filter, 0);
        let f = &out["properties"]["where"]["properties"];
        assert_eq!(f["and"], json!({"type": "array", "items": {"type": "string"}}));
        assert_eq!(f["not"], json!({"type": "string"}));
        // A reference to the root.
        let list = json!({"type": "object", "properties": {"name": {"type": "string"}, "next": {"$ref": "#"}}});
        assert_eq!(
            gemini_schema(&list, &list, 0),
            json!({"type": "object", "properties": {"name": {"type": "string"}, "next": {"type": "string"}}})
        );
    }

    #[test]
    fn versioned_base() {
        assert_eq!(
            versioned("https://generativelanguage.googleapis.com"),
            "https://generativelanguage.googleapis.com/v1beta"
        );
        assert_eq!(versioned("https://proxy/v1/"), "https://proxy/v1");
    }
}
