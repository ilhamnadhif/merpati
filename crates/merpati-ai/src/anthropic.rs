//! Anthropic Messages API (`POST {base}/v1/messages`).

use std::collections::BTreeMap;

use merpati_mcp::sse::SseEvent;
use serde_json::{Map, Value, json};

use crate::{
    AiConfig, AiError, HttpReq, Provider, Round, StreamAcc, ToolCallReq, ToolOutcome, ToolSpec,
    anthropic_default_max_tokens, merge_json, num_f32, parse_arguments, str_of,
};

pub(crate) const API_VERSION: &str = "2023-06-01";

pub(crate) struct Anthropic;

/// `{base}/v1{path}`, accepting a base that already ends in `/v1`.
pub(crate) fn endpoint(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") { format!("{base}{path}") } else { format!("{base}/v1{path}") }
}

pub(crate) fn auth_headers(key: Option<&str>) -> Vec<(&'static str, String, bool)> {
    let mut h = vec![("anthropic-version", API_VERSION.to_string(), false)];
    if let Some(k) = key {
        h.push(("x-api-key", k.to_string(), true));
    }
    h
}

fn usage_of(u: &Value) -> (Option<u64>, Option<u64>) {
    if !u.is_object() {
        return (None, None);
    }
    let field = |k: &str| u.get(k).and_then(Value::as_u64);
    // Cached prompt tokens are reported separately from `input_tokens`.
    let input = match (field("input_tokens"), field("cache_read_input_tokens"), field("cache_creation_input_tokens")) {
        (None, None, None) => None,
        (a, b, c) => Some(a.unwrap_or(0) + b.unwrap_or(0) + c.unwrap_or(0)),
    };
    (input, field("output_tokens"))
}

fn round_from_message(msg: Value) -> Round {
    let content = msg.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in &content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => {
                let (arguments, invalid) = parse_arguments(block.get("input").unwrap_or(&Value::Null));
                tool_calls.push(ToolCallReq {
                    id: str_of(block, "id").unwrap_or_default(),
                    name: str_of(block, "name").unwrap_or_default(),
                    arguments,
                    invalid,
                });
            }
            _ => {}
        }
    }
    let (input_tokens, output_tokens) = usage_of(msg.get("usage").unwrap_or(&Value::Null));
    // The API rejects a history whose `tool_use.input` is not an object (a
    // stream cut inside the block); the call is reported as invalid instead.
    let content: Vec<Value> = content
        .into_iter()
        .map(|mut block| {
            if block.get("type").and_then(Value::as_str) == Some("tool_use")
                && !block.get("input").is_some_and(Value::is_object)
            {
                block["input"] = json!({});
            }
            block
        })
        .collect();
    Round {
        text,
        tool_calls,
        finish_reason: str_of(&msg, "stop_reason"),
        model: str_of(&msg, "model"),
        input_tokens,
        output_tokens,
        assistant: json!({"role": "assistant", "content": content}),
        raw: msg,
    }
}

impl Provider for Anthropic {
    fn messages(&self, cfg: &AiConfig) -> Vec<Value> {
        cfg.messages
            .iter()
            .filter(|m| m.role != "system" && !m.content.trim().is_empty())
            .map(|m| json!({"role": if m.role == "assistant" { "assistant" } else { "user" }, "content": m.content}))
            .collect()
    }

    fn build(&self, cfg: &AiConfig, messages: &[Value], tools: &[ToolSpec], stream: bool) -> HttpReq {
        let mut system: Vec<&str> = cfg.system.as_deref().into_iter().collect();
        system.extend(
            cfg.messages
                .iter()
                .filter(|m| m.role == "system" && !m.content.trim().is_empty())
                .map(|m| m.content.as_str()),
        );
        let mut body = json!({
            "model": cfg.model,
            "max_tokens": cfg.max_tokens.unwrap_or_else(|| anthropic_default_max_tokens(&cfg.model)),
            "messages": messages,
        });
        if !system.is_empty() {
            body["system"] = json!(system.join("\n\n"));
        }
        if let Some(t) = cfg.temperature {
            body["temperature"] = num_f32(t);
        }
        if stream {
            body["stream"] = json!(true);
        }
        if !tools.is_empty() {
            body["tools"] = tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}))
                .collect();
        }
        if let Some(extra) = &cfg.extra {
            merge_json(&mut body, extra);
        }
        HttpReq { url: endpoint(&cfg.base_url, "/messages"), headers: auth_headers(cfg.api_key.as_deref()), body }
    }

    fn parse(&self, body: Value) -> Result<Round, AiError> {
        if body.get("type").and_then(Value::as_str) == Some("error") {
            let msg = body.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(AiError::Api(msg.to_string()));
        }
        if body.get("content").is_none() {
            return Err(AiError::Parse("the response has no content".into()));
        }
        Ok(round_from_message(body))
    }

    fn stream(&self) -> Box<dyn StreamAcc> {
        Box::new(AnthropicStream::default())
    }

    fn tool_results(&self, outcomes: &[ToolOutcome]) -> Vec<Value> {
        let blocks: Vec<Value> = outcomes
            .iter()
            .map(|o| {
                let mut b = json!({"type": "tool_result", "tool_use_id": o.call_id, "content": o.text});
                if o.is_error {
                    b["is_error"] = json!(true);
                }
                b
            })
            .collect();
        vec![json!({"role": "user", "content": blocks})]
    }
}

#[derive(Default)]
struct AnthropicStream {
    message: Map<String, Value>,
    blocks: BTreeMap<u64, Value>,
    partial_json: BTreeMap<u64, String>,
    usage: Map<String, Value>,
}

impl StreamAcc for AnthropicStream {
    fn event(&mut self, ev: &SseEvent, delta: &mut dyn FnMut(&str)) -> Result<(), AiError> {
        let data = ev.data.trim();
        if data.is_empty() {
            return Ok(());
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| AiError::Parse(format!("invalid stream event ({e}): {data}")))?;
        let kind = v.get("type").and_then(Value::as_str).unwrap_or(ev.event.as_str());
        let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        match kind {
            "message_start" => {
                if let Some(Value::Object(m)) = v.get("message") {
                    self.message = m.clone();
                    if let Some(Value::Object(u)) = m.get("usage") {
                        self.usage.extend(u.clone());
                    }
                }
            }
            "content_block_start" => {
                let block = v.get("content_block").cloned().unwrap_or_else(|| json!({}));
                if let Some(t) = block.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    delta(t);
                }
                self.blocks.insert(index, block);
            }
            "content_block_delta" => {
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                let block = self.blocks.entry(index).or_insert_with(|| json!({"type": "text", "text": ""}));
                let append = |block: &mut Value, key: &str, piece: &str| {
                    let cur = block.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                    block[key] = Value::String(cur + piece);
                };
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let t = d.get("text").and_then(Value::as_str).unwrap_or("");
                        append(block, "text", t);
                        delta(t);
                    }
                    Some("input_json_delta") => {
                        let p = d.get("partial_json").and_then(Value::as_str).unwrap_or("");
                        self.partial_json.entry(index).or_default().push_str(p);
                    }
                    Some("thinking_delta") => {
                        append(block, "thinking", d.get("thinking").and_then(Value::as_str).unwrap_or(""));
                    }
                    Some("signature_delta") => {
                        block["signature"] = d.get("signature").cloned().unwrap_or(Value::Null);
                    }
                    Some("citations_delta") => {
                        if let Some(c) = d.get("citation") {
                            match block.get_mut("citations").and_then(Value::as_array_mut) {
                                Some(list) => list.push(c.clone()),
                                None => block["citations"] = json!([c]),
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(json_text) = self.partial_json.remove(&index)
                    && let Some(block) = self.blocks.get_mut(&index)
                {
                    block["input"] = if json_text.trim().is_empty() {
                        json!({})
                    } else {
                        // Keep invalid (truncated) input as a string; the tool
                        // loop reports it to the model.
                        serde_json::from_str(&json_text).unwrap_or(Value::String(json_text))
                    };
                }
            }
            "message_delta" => {
                if let Some(Value::Object(d)) = v.get("delta") {
                    for (k, val) in d {
                        self.message.insert(k.clone(), val.clone());
                    }
                }
                if let Some(Value::Object(u)) = v.get("usage") {
                    for (k, val) in u {
                        if !val.is_null() {
                            self.usage.insert(k.clone(), val.clone());
                        }
                    }
                }
            }
            "error" => {
                let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("stream error");
                let kind = v.pointer("/error/type").and_then(Value::as_str).unwrap_or("error");
                return Err(AiError::Api(format!("{kind}: {msg}")));
            }
            _ => {} // ping, message_stop, future event types
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<Round, AiError> {
        // Blocks whose stop event never arrived still get their input.
        for (index, json_text) in std::mem::take(&mut self.partial_json) {
            if let Some(block) = self.blocks.get_mut(&index) {
                block["input"] = serde_json::from_str(&json_text).unwrap_or(Value::String(json_text));
            }
        }
        let mut message = self.message;
        message.entry("type").or_insert(json!("message"));
        message.entry("role").or_insert(json!("assistant"));
        message.insert("content".into(), Value::Array(self.blocks.into_values().collect()));
        message.insert("usage".into(), Value::Object(self.usage));
        Ok(round_from_message(Value::Object(message)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_tool_input_is_not_sent_back_as_a_string() {
        // A stream cut by max_tokens inside a tool_use block.
        let mut acc = Anthropic.stream();
        let mut sink = |_: &str| {};
        for data in [
            json!({"type": "message_start", "message": {"id": "m", "model": "claude", "content": [], "usage": {"input_tokens": 5}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "add", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"a\": 2, \"b\""}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 9}}),
        ] {
            let ev = SseEvent { event: "message".into(), data: data.to_string(), ..Default::default() };
            acc.event(&ev, &mut sink).unwrap();
        }
        let round = acc.finish().unwrap();
        assert!(round.tool_calls[0].invalid.is_some(), "reported to the model as invalid");
        assert_eq!(round.raw["content"][0]["input"], json!("{\"a\": 2, \"b\""));
        // The history must hold an object: the API rejects any other `input`.
        assert_eq!(round.assistant["content"][0]["input"], json!({}));
    }
}
