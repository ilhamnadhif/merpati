//! OpenAI Chat Completions (`POST {base}/chat/completions`), also used for
//! OpenAI-compatible servers (Ollama, LM Studio, vLLM, llama.cpp, ...).

use std::collections::BTreeMap;

use merpati_mcp::sse::SseEvent;
use serde_json::{Value, json};

use crate::{
    AiConfig, AiError, HttpReq, Provider, Round, StreamAcc, ToolCallReq, ToolOutcome, ToolSpec, merge_json, num_f32,
    parse_arguments, str_of,
};

pub(crate) struct OpenAi {
    /// OpenAI-compatible server: `max_tokens` instead of `max_completion_tokens`.
    pub compat: bool,
}

pub(crate) fn auth_headers(key: Option<&str>) -> Vec<(&'static str, String, bool)> {
    key.map(|k| vec![("authorization", format!("Bearer {k}"), true)]).unwrap_or_default()
}

/// `content` may be a string or (some compatible servers) an array of parts.
fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect(),
        _ => String::new(),
    }
}

fn usage(v: &Value) -> (Option<u64>, Option<u64>) {
    let u = v.get("usage").filter(|u| u.is_object());
    (
        u.and_then(|u| u.get("prompt_tokens")).and_then(Value::as_u64),
        u.and_then(|u| u.get("completion_tokens")).and_then(Value::as_u64),
    )
}

fn call_id(raw: Option<&str>, index: usize) -> String {
    match raw {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => format!("call_{index}"),
    }
}

/// The assistant turn to append to the history, plus the parsed tool calls.
fn assistant_turn(text: &str, raw_calls: &[(String, String, String)]) -> (Value, Vec<ToolCallReq>) {
    let mut msg = json!({"role": "assistant", "content": text});
    let mut calls = Vec::new();
    if !raw_calls.is_empty() {
        let mut history = Vec::new();
        for (id, name, args) in raw_calls {
            let (arguments, invalid) = parse_arguments(&Value::String(args.clone()));
            // Servers parse the arguments of the history (Ollama rejects the
            // request otherwise); a bad call is reported to the model instead.
            let sent = if invalid.is_none() && !args.trim().is_empty() { args.as_str() } else { "{}" };
            history.push(json!({"id": id, "type": "function", "function": {"name": name, "arguments": sent}}));
            calls.push(ToolCallReq { id: id.clone(), name: name.clone(), arguments, invalid });
        }
        msg["tool_calls"] = Value::Array(history);
    }
    (msg, calls)
}

impl Provider for OpenAi {
    fn messages(&self, cfg: &AiConfig) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(system) = &cfg.system {
            out.push(json!({"role": "system", "content": system}));
        }
        for m in &cfg.messages {
            let role = if m.role.is_empty() { "user" } else { m.role.as_str() };
            out.push(json!({"role": role, "content": m.content}));
        }
        out
    }

    fn build(&self, cfg: &AiConfig, messages: &[Value], tools: &[ToolSpec], stream: bool) -> HttpReq {
        let mut body = json!({"model": cfg.model, "messages": messages});
        if let Some(t) = cfg.temperature {
            body["temperature"] = num_f32(t);
        }
        if let Some(m) = cfg.max_tokens {
            body[if self.compat { "max_tokens" } else { "max_completion_tokens" }] = json!(m);
        }
        if stream {
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage": true});
        }
        if !tools.is_empty() {
            body["tools"] = tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.schema}})
                })
                .collect();
        }
        if let Some(extra) = &cfg.extra {
            merge_json(&mut body, extra);
        }
        HttpReq {
            url: format!("{}/chat/completions", cfg.base_url.trim_end_matches('/')),
            headers: auth_headers(cfg.api_key.as_deref()),
            body,
        }
    }

    fn parse(&self, body: Value) -> Result<Round, AiError> {
        let Some(choice) = body.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else {
            let msg = body
                .pointer("/error/message")
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("the response has no choices");
            return Err(AiError::Api(msg.to_string()));
        };
        let message = choice.get("message").cloned().unwrap_or(Value::Null);
        let text = content_text(message.get("content").unwrap_or(&Value::Null));
        let raw_calls: Vec<(String, String, String)> = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let f = c.get("function").cloned().unwrap_or(Value::Null);
                        let args = match f.get("arguments") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) if !other.is_null() => other.to_string(),
                            _ => String::new(),
                        };
                        (call_id(c.get("id").and_then(Value::as_str), i), str_of(&f, "name").unwrap_or_default(), args)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (assistant, tool_calls) = assistant_turn(&text, &raw_calls);
        let (input_tokens, output_tokens) = usage(&body);
        Ok(Round {
            text,
            tool_calls,
            finish_reason: str_of(choice, "finish_reason"),
            model: str_of(&body, "model"),
            input_tokens,
            output_tokens,
            raw: body,
            assistant,
        })
    }

    fn stream(&self) -> Box<dyn StreamAcc> {
        Box::new(OpenAiStream::default())
    }

    fn tool_results(&self, outcomes: &[ToolOutcome]) -> Vec<Value> {
        outcomes
            .iter()
            .map(|o| {
                let content = if o.is_error && !o.text.starts_with("Error") {
                    format!("Error: {}", o.text)
                } else {
                    o.text.clone()
                };
                json!({"role": "tool", "tool_call_id": o.call_id, "content": content})
            })
            .collect()
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct OpenAiStream {
    id: Option<String>,
    model: Option<String>,
    created: Option<Value>,
    text: String,
    calls: BTreeMap<u64, PartialCall>,
    finish_reason: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    usage: Option<Value>,
}

impl StreamAcc for OpenAiStream {
    fn event(&mut self, ev: &SseEvent, delta: &mut dyn FnMut(&str)) -> Result<(), AiError> {
        let data = ev.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| AiError::Parse(format!("invalid stream chunk ({e}): {data}")))?;
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            let msg = err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string());
            return Err(AiError::Api(msg));
        }
        if self.id.is_none() {
            self.id = str_of(&v, "id");
            self.created = v.get("created").cloned();
        }
        if let Some(m) = str_of(&v, "model") {
            self.model = Some(m);
        }
        let (i, o) = usage(&v);
        if i.is_some() || o.is_some() {
            self.input_tokens = i;
            self.output_tokens = o;
            self.usage = v.get("usage").cloned();
        }
        for choice in v.get("choices").and_then(Value::as_array).into_iter().flatten() {
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            let d = choice.get("delta").cloned().unwrap_or(Value::Null);
            let piece = content_text(d.get("content").unwrap_or(&Value::Null));
            if !piece.is_empty() {
                self.text.push_str(&piece);
                delta(&piece);
            }
            for (pos, tc) in d.get("tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let index = tc.get("index").and_then(Value::as_u64).unwrap_or(pos as u64);
                let entry = self.calls.entry(index).or_default();
                if let Some(id) = tc.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    entry.id = id.to_string();
                }
                if let Some(f) = tc.get("function") {
                    if let Some(name) = f.get("name").and_then(Value::as_str).filter(|s| !s.is_empty())
                        && entry.name.is_empty()
                    {
                        entry.name = name.to_string();
                    }
                    match f.get("arguments") {
                        Some(Value::String(s)) => entry.arguments.push_str(s),
                        Some(obj @ Value::Object(_)) => entry.arguments = obj.to_string(),
                        _ => {}
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_string());
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Round, AiError> {
        let raw_calls: Vec<(String, String, String)> =
            self.calls.into_values().enumerate().map(|(i, c)| (call_id(Some(&c.id), i), c.name, c.arguments)).collect();
        let (assistant, tool_calls) = assistant_turn(&self.text, &raw_calls);
        let raw = json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "message": assistant, "finish_reason": self.finish_reason}],
            "usage": self.usage,
        });
        Ok(Round {
            text: self.text,
            tool_calls,
            finish_reason: self.finish_reason,
            model: self.model,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            raw,
            assistant,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_tool_arguments_are_always_json() {
        let body = json!({"model": "m", "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
        "role": "assistant", "content": null, "tool_calls": [
            {"id": "a", "type": "function", "function": {"name": "now", "arguments": ""}},
            {"id": "b", "type": "function", "function": {"name": "add", "arguments": "{\"a\": 1, \"b\""}},
            {"id": "c", "type": "function", "function": {"name": "add", "arguments": "{\"a\": 1,  \"b\": 2}"}}
        ]}}]});
        let round = OpenAi { compat: true }.parse(body).unwrap();
        let invalid: Vec<bool> = round.tool_calls.iter().map(|c| c.invalid.is_some()).collect();
        assert_eq!(invalid, [false, true, false]);
        // Servers parse these (Ollama answers 400 "invalid tool call arguments").
        let sent: Vec<&str> = round.assistant["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["function"]["arguments"].as_str().unwrap())
            .collect();
        assert_eq!(sent, ["{}", "{}", "{\"a\": 1,  \"b\": 2}"]);
    }
}
