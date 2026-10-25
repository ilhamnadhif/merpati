//! A tiny MCP server over stdio, used by the integration tests (feature
//! `fake-server`). Behavior switches (environment variables):
//!
//! - `FAKE_MCP_ONLY_VERSION=<v>`: reject any other protocol version with a
//!   JSON-RPC error (to exercise the client's version fallback).
//! - `FAKE_MCP_EXIT_ON_START=1`: print to stderr and exit with code 3.
//!
//! The unlisted tool `flood` writes one endless line to stdout.
//!
//! Tools `sample`, `elicit`, `roots` and `ask_unknown` send the matching
//! server-to-client request and return the client's answer as JSON text;
//! `caps` returns the capabilities the client declared.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

const SUPPORTED: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

fn send(out: &mut impl Write, v: &Value) {
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn tools() -> Vec<Value> {
    vec![
        json!({"name": "echo", "title": "Echo", "description": "Echoes text back",
               "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
        json!({"name": "add", "description": "Adds two numbers",
               "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]},
               "outputSchema": {"type": "object", "properties": {"sum": {"type": "number"}}}}),
        json!({"name": "fail", "description": "Always fails", "inputSchema": {"type": "object"}}),
        json!({"name": "env", "description": "Reports FAKE_VAR and the working directory", "inputSchema": {"type": "object"}}),
        json!({"name": "slow", "description": "Sleeps 1.5 s", "inputSchema": {"type": "object"}}),
        json!({"name": "ask_ping", "description": "Pings the client before answering", "inputSchema": {"type": "object"}}),
        json!({"name": "caps", "description": "Client capabilities", "inputSchema": {"type": "object"}}),
        json!({"name": "sample", "description": "Asks the client to sample", "inputSchema": {"type": "object"}}),
        json!({"name": "sample_cancel", "description": "Asks, then cancels", "inputSchema": {"type": "object"}}),
        json!({"name": "elicit", "description": "Asks the user", "inputSchema": {"type": "object"}}),
        json!({"name": "roots", "description": "Lists the client's roots", "inputSchema": {"type": "object"}}),
        json!({"name": "roots_changed", "description": "Counts roots/list_changed", "inputSchema": {"type": "object"}}),
        json!({"name": "ask_unknown", "description": "Sends an unknown request", "inputSchema": {"type": "object"}}),
    ]
}

fn main() {
    eprintln!("fake server starting");
    if std::env::var("FAKE_MCP_EXIT_ON_START").is_ok() {
        eprintln!("fatal: boom");
        std::process::exit(3);
    }
    let only = std::env::var("FAKE_MCP_ONLY_VERSION").ok();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    // Tool call waiting for the client's answer to our ping.
    let mut waiting_ping: Option<Value> = None;
    // Server request id -> the tool call waiting for its answer.
    let mut waiting: HashMap<String, Value> = HashMap::new();
    let mut client_caps = json!({});
    let mut roots_changed = 0;
    let mut next_request = 0;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            send(&mut out, &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}));
            continue;
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        if method.is_empty()
            && let Some(call_id) = msg.get("id").and_then(Value::as_str).and_then(|i| waiting.remove(i))
        {
            let answer = msg.get("result").or_else(|| msg.get("error")).cloned().unwrap_or(Value::Null);
            let kind = if msg.get("error").is_some() { "error" } else { "result" };
            send(
                &mut out,
                &json!({"jsonrpc": "2.0", "id": call_id,
                        "result": {"content": [{"type": "text", "text": json!({kind: answer}).to_string()}]}}),
            );
            continue;
        }
        if method.is_empty() {
            // A response from the client.
            if msg.get("id") == Some(&json!("srv-ping"))
                && let Some(call_id) = waiting_ping.take()
            {
                let ok = msg.get("result").is_some();
                send(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": call_id,
                            "result": {"content": [{"type": "text", "text": if ok { "pong received" } else { "no pong" }}]}}),
                );
            }
            continue;
        }
        let Some(id) = id else {
            if method == "notifications/initialized" {
                eprintln!("client initialized");
            }
            if method == "notifications/roots/list_changed" {
                roots_changed += 1;
            }
            continue;
        };
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let error =
            |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        let response = match method.as_str() {
            "initialize" => {
                client_caps = params.get("capabilities").cloned().unwrap_or(json!({}));
                let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
                match &only {
                    Some(v) if v != requested => json!({"jsonrpc": "2.0", "id": id, "error": {
                        "code": -32602, "message": "Unsupported protocol version", "data": {"supported": [v], "requested": requested}}}),
                    _ => {
                        let version = if SUPPORTED.contains(&requested) { requested } else { SUPPORTED[0] };
                        reply(json!({
                            "protocolVersion": only.as_deref().unwrap_or(version),
                            "capabilities": {"tools": {"listChanged": true}, "resources": {}, "prompts": {}, "logging": {}},
                            "serverInfo": {"name": "fake-mcp", "version": "1.2.3"},
                            "instructions": "Use echo to test."
                        }))
                    }
                }
            }
            "ping" => reply(json!({})),
            "tools/list" => {
                let all = tools();
                let start: usize =
                    params.get("cursor").and_then(Value::as_str).and_then(|c| c.parse().ok()).unwrap_or(0);
                let page: Vec<Value> = all.iter().skip(start).take(4).cloned().collect();
                let mut result = json!({"tools": page});
                if start + 4 < all.len() {
                    result["nextCursor"] = json!((start + 4).to_string());
                }
                reply(result)
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                eprintln!("called {name}");
                match name {
                    "echo" => {
                        send(
                            &mut out,
                            &json!({"jsonrpc": "2.0", "method": "notifications/message",
                                               "params": {"level": "info", "logger": "echo", "data": "echoing"}}),
                        );
                        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
                        reply(json!({"content": [{"type": "text", "text": text}]}))
                    }
                    "add" => {
                        let a = args.get("a").and_then(Value::as_f64).unwrap_or(0.0);
                        let b = args.get("b").and_then(Value::as_f64).unwrap_or(0.0);
                        reply(
                            json!({"content": [{"type": "text", "text": format!("{}", a + b)}], "structuredContent": {"sum": a + b}}),
                        )
                    }
                    "fail" => reply(json!({"content": [{"type": "text", "text": "it broke"}], "isError": true})),
                    "env" => {
                        let var = std::env::var("FAKE_VAR").unwrap_or_default();
                        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
                        reply(json!({"content": [{"type": "text", "text": var}, {"type": "text", "text": cwd}]}))
                    }
                    "slow" => {
                        std::thread::sleep(Duration::from_millis(1500));
                        reply(json!({"content": [{"type": "text", "text": "late"}]}))
                    }
                    // Not listed: a line that never ends, until the client hangs up.
                    "flood" => {
                        let chunk = vec![b'x'; 1024 * 1024];
                        while out.write_all(&chunk).is_ok() {}
                        return;
                    }
                    "caps" => reply(json!({"content": [{"type": "text", "text": client_caps.to_string()}]})),
                    "roots_changed" => reply(json!({"content": [{"type": "text", "text": roots_changed.to_string()}]})),
                    "sample" | "sample_cancel" | "elicit" | "roots" | "ask_unknown" => {
                        next_request += 1;
                        let rid = format!("srv-{name}-{next_request}");
                        let (m, p) = match name {
                            "sample" | "sample_cancel" => (
                                "sampling/createMessage",
                                json!({
                                    "messages": [{"role": "user", "content": {"type": "text", "text": args.get("prompt").and_then(Value::as_str).unwrap_or("hi")}}],
                                    "systemPrompt": "You are a test",
                                    "maxTokens": 50,
                                    "modelPreferences": {"hints": [{"name": "small"}]}
                                }),
                            ),
                            "elicit" => (
                                "elicitation/create",
                                json!({
                                    "message": "Your name?",
                                    "requestedSchema": {"type": "object", "properties": {"name": {"type": "string", "title": "Name"}}, "required": ["name"]}
                                }),
                            ),
                            "roots" => ("roots/list", json!({})),
                            _ => ("foo/bar", json!({})),
                        };
                        send(&mut out, &json!({"jsonrpc": "2.0", "id": rid, "method": m, "params": p}));
                        if name == "sample_cancel" {
                            send(
                                &mut out,
                                &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": rid, "reason": "changed my mind"}}),
                            );
                            reply(json!({"content": [{"type": "text", "text": "cancel sent"}]}))
                        } else {
                            waiting.insert(rid, id.clone());
                            continue;
                        }
                    }
                    "ask_ping" => {
                        send(&mut out, &json!({"jsonrpc": "2.0", "id": "srv-ping", "method": "ping"}));
                        waiting_ping = Some(id.clone());
                        continue;
                    }
                    _ => error(-32602, &format!("Unknown tool: {name}")),
                }
            }
            "resources/list" => reply(json!({"resources": [
                {"uri": "mem://greeting", "name": "greeting", "mimeType": "text/plain", "size": 5}
            ]})),
            "resources/templates/list" => reply(json!({"resourceTemplates": [
                {"uriTemplate": "mem://items/{id}", "name": "item"}
            ]})),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
                reply(json!({"contents": [{"uri": uri, "mimeType": "text/plain", "text": "hello"}]}))
            }
            "prompts/list" => reply(json!({"prompts": [
                {"name": "greet", "description": "Greets someone", "arguments": [{"name": "who", "required": true}]}
            ]})),
            "prompts/get" => {
                let who = params.pointer("/arguments/who").and_then(Value::as_str).unwrap_or("?");
                reply(json!({"description": "Greeting", "messages": [
                    {"role": "user", "content": {"type": "text", "text": format!("Say hi to {who}")}}
                ]}))
            }
            _ => error(-32601, &format!("Method not found: {method}")),
        };
        send(&mut out, &response);
    }
    eprintln!("stdin closed, bye");
}
