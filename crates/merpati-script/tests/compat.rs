//! Compatibility corpus: realistic Postman scripts with expected outcomes.
//!
//! Each `tests/compat/*.js` file starts with `// @key: value` header lines:
//!
//! - `@event`: `prerequest` (default `test` when a response is given)
//! - `@response`: JSON `{code, status, headers: [[k, v]], body}`
//! - `@request`: JSON `ScriptRequest`
//! - `@variables`: JSON `VariableScopes`
//! - `@cookies`: JSON list of `ScriptCookie`
//! - `@info`: JSON `ScriptInfo`
//! - `@timeout`: job timeout in ms
//! - `@expect`: JSON object, see `check` below
//!
//! A line `// @split <location>` starts another script of the same job, so
//! multi-level jobs (collection, folder, request) can be expressed.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use merpati_core::response::ResponseBody;
use merpati_core::script::*;
use merpati_core::variables::VariableScopes;
use merpati_script::BoaEngine;
use serde_json::{Value, json};

/// Fake network for `pm.sendRequest`.
struct FakeHttp;

impl ScriptHttp for FakeHttp {
    fn send(&self, req: ScriptRequest, scopes: &VariableScopes) -> Result<ScriptResponse, String> {
        let url = merpati_core::Resolver::new(scopes).resolve(&req.url);
        let (code, body): (u16, Value) = if url.contains("/fail") {
            return Err("connect ECONNREFUSED 127.0.0.1:1".into());
        } else if url.contains("/token") {
            (200, json!({"access_token": "tok-123", "token_type": "Bearer", "expires_in": 3600}))
        } else if url.contains("/echo") {
            (
                200,
                json!({
                    "method": req.method,
                    "url": url,
                    "headers": req.headers.iter().filter(|h| h.enabled).map(|h| (h.key.to_lowercase(), Value::String(h.value.clone()))).collect::<serde_json::Map<_, _>>(),
                    "body": req.body.raw,
                    "mode": serde_json::to_value(req.body.mode).unwrap(),
                    "form": req.body.urlencoded.iter().map(|f| (f.key.clone(), Value::String(f.value.clone()))).collect::<serde_json::Map<_, _>>(),
                }),
            )
        } else if let Some(rest) = url.split("/status/").nth(1) {
            (rest.parse().unwrap_or(500), json!({"status": rest}))
        } else {
            (200, json!({"ok": true}))
        };
        let text = body.to_string();
        Ok(ScriptResponse {
            code,
            status: if code == 200 { "OK".into() } else { "Error".into() },
            headers: vec![("Content-Type".into(), "application/json".into())],
            response_size: text.len() as u64,
            body: ResponseBody::Memory(text.into()),
            response_time_ms: 12.0,
        })
    }
}

struct Case {
    name: String,
    job: ScriptJob,
    expect: Value,
}

fn parse_case(path: &Path) -> Case {
    let text = std::fs::read_to_string(path).unwrap();
    let mut headers = serde_json::Map::new();
    let mut scripts: Vec<ScriptSource> =
        vec![ScriptSource { location: "request \"case\"".into(), code: String::new() }];
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("// @split ") {
            scripts.push(ScriptSource { location: rest.trim().to_string(), code: String::new() });
            continue;
        }
        if let Some((k, v)) = line.strip_prefix("// @").and_then(|rest| rest.split_once(':')) {
            let v = v.trim();
            let val = serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()));
            headers.insert(k.trim().to_string(), val);
            continue;
        }
        let code = &mut scripts.last_mut().unwrap().code;
        code.push_str(line);
        code.push('\n');
    }
    if let Some(Value::String(loc)) = headers.get("location") {
        scripts[0].location = loc.clone();
    }
    let response: Option<ScriptResponse> = headers.get("response").map(|r| {
        let body =
            r.get("body").map(|b| if let Value::String(s) = b { s.clone() } else { b.to_string() }).unwrap_or_default();
        ScriptResponse {
            code: r.get("code").and_then(Value::as_u64).unwrap_or(200) as u16,
            status: r.get("status").and_then(Value::as_str).unwrap_or("OK").to_string(),
            headers: serde_json::from_value(r.get("headers").cloned().unwrap_or(json!([]))).unwrap(),
            response_size: body.len() as u64,
            body: ResponseBody::Memory(body.into()),
            response_time_ms: r.get("responseTime").and_then(Value::as_f64).unwrap_or(42.0),
        }
    });
    let event = match headers.get("event").and_then(Value::as_str) {
        Some("prerequest") => ScriptEvent::PreRequest,
        Some("test") => ScriptEvent::PostResponse,
        _ if response.is_some() => ScriptEvent::PostResponse,
        _ => ScriptEvent::PreRequest,
    };
    let request: ScriptRequest = headers
        .get("request")
        .map(|r| {
            let mut base =
                json!({"id": null, "name": null, "method": "GET", "url": "", "headers": [], "body": {"mode": "none"}});
            for (k, v) in r.as_object().unwrap() {
                base[k] = v.clone();
            }
            serde_json::from_value(base).unwrap()
        })
        .unwrap_or(ScriptRequest {
            id: Some("req-1".into()),
            name: Some("case".into()),
            method: "GET".into(),
            url: "https://api.example.com/users?page=1".into(),
            ..Default::default()
        });
    let mut job = ScriptJob::new(event, scripts, request);
    job.response = response;
    if let Some(v) = headers.get("variables") {
        job.variables = serde_json::from_value(v.clone()).unwrap();
    }
    if let Some(v) = headers.get("cookies") {
        job.cookies = serde_json::from_value(v.clone()).unwrap();
    }
    if let Some(v) = headers.get("info") {
        job.info = serde_json::from_value(v.clone()).unwrap();
    }
    if let Some(ms) = headers.get("timeout").and_then(Value::as_u64) {
        job.timeout = Duration::from_millis(ms);
    }
    job.http = Some(Arc::new(FakeHttp));
    Case {
        name: path.file_name().unwrap().to_string_lossy().to_string(),
        job,
        expect: headers.get("expect").cloned().unwrap_or(json!({})),
    }
}

fn check(out: &ScriptOutcome, expect: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let obj = expect.as_object().cloned().unwrap_or_default();
    for (key, want) in obj {
        match key.as_str() {
            "tests" => {
                let got: Vec<(String, bool)> = out.tests.iter().map(|t| (t.name.clone(), t.passed)).collect();
                let want: Vec<(String, bool)> = serde_json::from_value(want.clone()).unwrap();
                if got != want {
                    problems.push(format!(
                        "tests: want {want:?}, got {got:?} (errors: {:?})",
                        out.tests.iter().filter_map(|t| t.error.clone()).collect::<Vec<_>>()
                    ));
                }
            }
            "test_errors" => {
                for (name, sub) in want.as_object().unwrap() {
                    let sub = sub.as_str().unwrap();
                    match out.tests.iter().find(|t| &t.name == name) {
                        Some(t) if t.error.as_deref().unwrap_or("").contains(sub) => {}
                        Some(t) => problems.push(format!("test {name}: error {:?} does not contain {sub:?}", t.error)),
                        None => problems.push(format!("test {name} missing")),
                    }
                }
            }
            "skipped" => {
                let got: Vec<&str> = out.tests.iter().filter(|t| t.skipped).map(|t| t.name.as_str()).collect();
                let want: Vec<String> = serde_json::from_value(want.clone()).unwrap();
                if got != want {
                    problems.push(format!("skipped: want {want:?}, got {got:?}"));
                }
            }
            "environment" | "globals" | "collection" | "local" => {
                let map = match key.as_str() {
                    "environment" => &out.variables.environment,
                    "globals" => &out.variables.globals,
                    "collection" => &out.variables.collection,
                    _ => &out.variables.local,
                };
                for (k, v) in want.as_object().unwrap() {
                    let got = map.get(k);
                    let ok = match v {
                        Value::Null => got.is_none(),
                        Value::String(s) if s.starts_with('~') => got.map(|g| g.contains(&s[1..])).unwrap_or(false),
                        Value::String(s) => got == Some(s),
                        other => got.map(String::as_str) == Some(other.to_string().as_str()),
                    };
                    if !ok {
                        problems.push(format!("{key}.{k}: want {v}, got {got:?}"));
                    }
                }
            }
            "errors" => {
                let n = want.as_u64().unwrap() as usize;
                if out.errors.len() != n {
                    problems.push(format!("errors: want {n}, got {:?}", out.errors));
                }
            }
            "error_contains" => {
                for sub in want.as_array().unwrap() {
                    let sub = sub.as_str().unwrap();
                    if !out.errors.iter().any(|e| e.message.contains(sub) || e.location.contains(sub)) {
                        problems.push(format!("no error containing {sub:?}: {:?}", out.errors));
                    }
                }
            }
            "console" => {
                let got: Vec<&str> = out.console.iter().map(|c| c.message.as_str()).collect();
                let want: Vec<String> = serde_json::from_value(want.clone()).unwrap();
                if got != want {
                    problems.push(format!("console: want {want:?}, got {got:?}"));
                }
            }
            "console_contains" => {
                for sub in want.as_array().unwrap() {
                    let sub = sub.as_str().unwrap();
                    if !out.console.iter().any(|c| c.message.contains(sub)) {
                        problems.push(format!("console does not contain {sub:?}: {:?}", out.console));
                    }
                }
            }
            "next_request" => {
                let got = match &out.next_request {
                    None => Value::Null,
                    Some(NextRequest::Stop) => json!("<stop>"),
                    Some(NextRequest::Named(n)) => json!(n),
                };
                if got != want {
                    problems.push(format!("next_request: want {want}, got {got}"));
                }
            }
            "skip_request" => {
                if out.skip_request != want.as_bool().unwrap() {
                    problems.push(format!("skip_request: want {want}, got {}", out.skip_request));
                }
            }
            "visualization" => {
                if out.visualization.is_some() != want.as_bool().unwrap() {
                    problems.push(format!("visualization: want {want}, got {:?}", out.visualization));
                }
            }
            "request" => {
                let r = &out.request;
                for (k, v) in want.as_object().unwrap() {
                    let ok = match k.as_str() {
                        "url" => r.url == v.as_str().unwrap(),
                        "method" => r.method == v.as_str().unwrap(),
                        "body_raw" => r.body.raw == v.as_str().unwrap(),
                        "body_mode" => serde_json::to_value(r.body.mode).unwrap() == *v,
                        "headers" => v.as_object().unwrap().iter().all(|(hk, hv)| {
                            let found = r.headers.iter().rev().find(|h| h.enabled && h.key.eq_ignore_ascii_case(hk));
                            match hv {
                                Value::Null => found.is_none(),
                                _ => found.map(|h| h.value.as_str()) == hv.as_str(),
                            }
                        }),
                        "urlencoded" => {
                            let got: serde_json::Map<_, _> = r
                                .body
                                .urlencoded
                                .iter()
                                .map(|f| (f.key.clone(), Value::String(f.value.clone())))
                                .collect();
                            Value::Object(got) == *v
                        }
                        other => panic!("unknown request expectation {other}"),
                    };
                    if !ok {
                        problems.push(format!("request.{k}: want {v}, got {r:?}"));
                    }
                }
            }
            "max_ms" => {}
            other => panic!("unknown expectation key {other}"),
        }
    }
    problems
}

#[test]
fn compat_corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compat");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .collect();
    paths.sort();
    let engine = BoaEngine::new();
    let mut failures = Vec::new();
    let total = paths.len();
    for p in &paths {
        let case = parse_case(p);
        let max_ms = case.expect.get("max_ms").and_then(Value::as_u64);
        let t = Instant::now();
        let out = engine.run(case.job);
        let elapsed = t.elapsed();
        let mut problems = check(&out, &case.expect);
        if let Some(max) = max_ms
            && elapsed.as_millis() as u64 > max
        {
            problems.push(format!("took {elapsed:?}, more than {max} ms"));
        }
        if problems.is_empty() {
            println!("PASS {} ({elapsed:?})", case.name);
        } else {
            println!("FAIL {} ({elapsed:?})", case.name);
            for pr in &problems {
                println!("    {pr}");
            }
            failures.push(case.name);
        }
    }
    println!("compat corpus: {}/{} passed", total - failures.len(), total);
    assert!(failures.is_empty(), "failing cases: {failures:?}");
}
