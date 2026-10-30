//! Executor, validation and storage tests against a local axum server (with
//! an OpenAI-compatible endpoint for AI blocks) and the real boa engine.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get as route_get, post};
use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use merpati_core::auth::Auth;
use merpati_core::model::{Collection, HttpRequest, Item, KeyValue, RequestItem, RequestKind, Scripts};
use merpati_core::protocols::{AiMessage, AiProvider, AiRequest};
use merpati_core::settings::RequestDefaults;
use merpati_core::tokens::MemoryTokenStore;
use merpati_core::variables::{VarType, Variable, VariableScopes};
use merpati_flows::validate::has_errors;
use merpati_flows::*;
use merpati_http::{CookieJar, EngineConfig, HttpEngine};
use merpati_runner::Services;

// ---------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Mock {
    hits: Arc<AtomicUsize>,
    ai: Arc<Mutex<Vec<Value>>>,
    /// `Authorization` headers the MCP endpoint received.
    mcp_auth: Arc<Mutex<Vec<String>>>,
}

fn bearer_user(h: &HeaderMap) -> Option<String> {
    h.get("authorization")?.to_str().ok()?.strip_prefix("Bearer tok-").map(str::to_string)
}

async fn server() -> (String, Mock) {
    let mock = Mock::default();
    let app = Router::new()
        .route(
            "/login",
            post(|body: String| async move {
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                if v["password"] == "secret" {
                    axum::Json(json!({"token": format!("tok-{}", v["username"].as_str().unwrap_or(""))}))
                        .into_response()
                } else {
                    (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "bad credentials"}))).into_response()
                }
            }),
        )
        .route(
            "/me",
            route_get(|h: HeaderMap| async move {
                match bearer_user(&h) {
                    Some(u) => axum::Json(json!({"user": u})).into_response(),
                    None => (StatusCode::UNAUTHORIZED, "no token").into_response(),
                }
            }),
        )
        .route(
            "/items",
            route_get(|h: HeaderMap| async move {
                if bearer_user(&h).is_none() {
                    return (StatusCode::UNAUTHORIZED, "no token").into_response();
                }
                axum::Json(json!({"items": [
                    {"id": 1, "name": "one", "active": true},
                    {"id": 2, "name": "two", "active": false},
                    {"id": 3, "name": "three", "active": true}
                ]}))
                .into_response()
            }),
        )
        .route(
            "/items/{id}",
            route_get(|Path(id): Path<u32>| async move {
                if id == 404 {
                    return (StatusCode::NOT_FOUND, "missing").into_response();
                }
                axum::Json(json!({"id": id, "detail": format!("detail-{id}")})).into_response()
            }),
        )
        .route(
            "/status/{code}",
            route_get(|Path(code): Path<u16>| async move { (StatusCode::from_u16(code).unwrap(), format!("status {code}")) }),
        )
        .route(
            "/slow",
            route_get(|| async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                "late"
            }),
        )
        .route(
            "/sleep/{ms}",
            route_get(|Path(ms): Path<u64>| async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                axum::Json(json!({"slept": ms}))
            }),
        )
        .route(
            "/sse/v1/chat/completions",
            post(|| async {
                // Three pieces of an answer, 150 ms apart, then [DONE].
                let pieces = ["Hello", " streaming", " world"];
                let chunks = futures_util::stream::unfold(0usize, move |i| async move {
                    if i > pieces.len() {
                        return None;
                    }
                    if i > 0 && i < pieces.len() {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                    }
                    let data = match pieces.get(i) {
                        Some(p) => json!({"model": "sse-model", "choices": [{"index": 0, "delta": {"content": p}}]})
                            .to_string(),
                        None => "[DONE]".to_string(),
                    };
                    Some((Ok::<_, std::convert::Infallible>(format!("data: {data}\n\n")), i + 1))
                });
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], axum::body::Body::from_stream(chunks))
            }),
        )
        .route(
            // A minimal Streamable HTTP MCP server without tools.
            "/mcp",
            post(|State(m): State<Mock>, h: HeaderMap, body: String| async move {
                let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                m.mcp_auth.lock().unwrap().push(auth);
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let result = match v["method"].as_str() {
                    _ if v.get("id").is_none() => return StatusCode::ACCEPTED.into_response(),
                    Some("initialize") => json!({
                        "protocolVersion": "2025-03-26",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "mock", "version": "1"}
                    }),
                    Some("tools/list") => json!({"tools": []}),
                    _ => json!({}),
                };
                axum::Json(json!({"jsonrpc": "2.0", "id": v["id"], "result": result})).into_response()
            })
            .get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .route(
            "/hit",
            route_get(|State(m): State<Mock>| async move {
                let n = m.hits.fetch_add(1, Ordering::SeqCst) + 1;
                axum::Json(json!({"hits": n}))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(|State(m): State<Mock>, body: String| async move {
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                m.ai.lock().unwrap().push(v.clone());
                let last = v["messages"]
                    .as_array()
                    .and_then(|a| a.iter().rev().find(|m| m["role"] == "user"))
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                let text = if last.contains("json please") { "```json\n{\"ok\": true}\n```".to_string() } else {
                    format!("SUMMARY[{last}]")
                };
                axum::Json(json!({
                    "id": "chatcmpl-1",
                    "object": "chat.completion",
                    "model": v["model"],
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12}
                }))
            }),
        )
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mock)
}

fn services() -> Services {
    let engine = merpati_script::BoaEngine::new();
    Services {
        // Direct connections: reading the system proxy settings can take
        // seconds in a freshly built test binary.
        http: Arc::new(HttpEngine::new(
            EngineConfig { proxy: Default::default(), ..Default::default() },
            Arc::new(CookieJar::new()),
        )),
        scripts: Arc::new(engine),
        tokens: Arc::new(MemoryTokenStore::default()),
        vault: None,
        defaults: RequestDefaults::default(),
        script_timeout: Duration::from_secs(20),
        runtime: tokio::runtime::Handle::current(),
        packages: None,
    }
}

// ---------------------------------------------------------------------------
// Flow builders
// ---------------------------------------------------------------------------

fn start() -> Block {
    Block::with_id("start", "Start", BlockConfig::Start(StartConfig {}))
}

fn get(id: &str, url: &str) -> Block {
    request(id, HttpRequest::new("GET", url), Scripts::default())
}

fn request(id: &str, r: HttpRequest, scripts: Scripts) -> Block {
    Block::with_id(
        id,
        id,
        BlockConfig::Request(RequestConfig {
            source: RequestSource::Inline { request: Box::new(r), collection_id: String::new(), scripts },
            ..Default::default()
        }),
    )
}

fn post_json(id: &str, url: &str, body: &str) -> Block {
    let mut r = HttpRequest::new("POST", url);
    r.headers.push(KeyValue::new("Content-Type", "application/json"));
    r.body = merpati_core::model::Body::raw_json(body);
    request(id, r, Scripts::default())
}

fn output(id: &str, name: &str, value: &str) -> Block {
    Block::with_id(
        id,
        id,
        BlockConfig::Output(OutputConfig { name: name.into(), value: value.into(), ..Default::default() }),
    )
}

fn set_var(id: &str, name: &str, value: &str) -> Block {
    Block::with_id(
        id,
        id,
        BlockConfig::SetVariable(SetVariableConfig { name: name.into(), value: value.into(), ..Default::default() }),
    )
}

fn script(id: &str, code: &str) -> Block {
    Block::with_id(id, id, BlockConfig::Script(ScriptConfig { code: code.into() }))
}

fn compare(id: &str, left: &str, op: CompareOp, right: &str) -> Block {
    Block::with_id(
        id,
        id,
        BlockConfig::Condition(ConditionConfig {
            mode: ConditionMode::Compare,
            left: left.into(),
            op,
            right: right.into(),
            ..Default::default()
        }),
    )
}

fn each(id: &str, items: &str) -> Block {
    Block::with_id(
        id,
        id,
        BlockConfig::Loop(LoopConfig { mode: LoopMode::ForEach, items: items.into(), ..Default::default() }),
    )
}

fn count_loop(id: &str, count: &str) -> Block {
    Block::with_id(
        id,
        id,
        BlockConfig::Loop(LoopConfig { mode: LoopMode::Count, count: count.into(), ..Default::default() }),
    )
}

fn log(id: &str, message: &str) -> Block {
    Block::with_id(id, id, BlockConfig::Log(LogConfig { message: message.into(), ..Default::default() }))
}

fn template(id: &str, t: &str) -> Block {
    Block::with_id(id, id, BlockConfig::Template(TemplateConfig { template: t.into(), ..Default::default() }))
}

fn delay(id: &str, ms: &str) -> Block {
    Block::with_id(id, id, BlockConfig::Delay(DelayConfig { ms: ms.into() }))
}

fn parallel(id: &str) -> Block {
    Block::with_id(id, id, BlockConfig::Parallel(ParallelConfig {}))
}

fn join(id: &str) -> Block {
    Block::with_id(id, id, BlockConfig::Join(JoinConfig {}))
}

fn ai(id: &str, base_url: &str, stream: bool) -> Block {
    Block::with_id(
        id,
        id,
        BlockConfig::Ai(AiBlockConfig {
            provider: Some(AiProvider::OpenAiCompatible),
            base_url: base_url.into(),
            model: "test-model".into(),
            prompt: format!("prompt of {id}"),
            stream,
            ..Default::default()
        }),
    )
}

/// Every BlockStarted has exactly one BlockFinished with the same step and
/// block, after it.
fn assert_steps_pair(events: &[FlowEvent]) {
    for (i, e) in events.iter().enumerate() {
        if let FlowEvent::BlockStarted { step, block, .. } = e {
            let finished: Vec<usize> = events
                .iter()
                .enumerate()
                .filter(
                    |(_, f)| matches!(f, FlowEvent::BlockFinished { step: s, block: b, .. } if s == step && b == block),
                )
                .map(|(j, _)| j)
                .collect();
            assert_eq!(finished.len(), 1, "step {step} ({block}) finished {} times", finished.len());
            assert!(finished[0] > i, "step {step} finished before it started");
        }
    }
}

fn event_index(events: &[FlowEvent], started: bool, block: &str) -> usize {
    events
        .iter()
        .position(|e| match e {
            FlowEvent::BlockStarted { block: b, .. } if started => b == block,
            FlowEvent::BlockFinished { block: b, .. } if !started => b == block,
            _ => false,
        })
        .unwrap_or_else(|| panic!("no event for {block}"))
}

fn step<'r>(r: &'r FlowResult, block: &str) -> Vec<&'r StepRecord> {
    r.steps.iter().filter(|s| s.block == block).collect()
}

/// `(from, port, to)` edges into `to.in`; `to` may be `block:port`.
fn flow(blocks: Vec<Block>, edges: &[(&str, &str, &str)]) -> Flow {
    let mut f = Flow::new("Test flow");
    f.blocks = std::iter::once(start()).chain(blocks).collect();
    f.edges = edges
        .iter()
        .map(|(a, p, b)| match b.split_once(':') {
            Some((b, port)) => Edge::new(a, p, b, port),
            None => Edge::new(a, p, b, PORT_IN),
        })
        .collect();
    f
}

fn scopes(base: &str) -> VariableScopes {
    let mut v = VariableScopes::default();
    v.environment.insert("baseUrl".into(), base.into());
    v.environment_name = Some("Local".into());
    v
}

struct Ran {
    result: FlowResult,
    events: Vec<FlowEvent>,
    vars: VariableScopes,
}

async fn run_with(flow: &Flow, env: &FlowEnv, base: &str, opts: FlowRunOptions, cancel: &CancellationToken) -> Ran {
    let mut vars = scopes(base);
    let mut events = Vec::new();
    let mut sink = |e: FlowEvent| events.push(e);
    let result = run_flow(env, flow, &mut vars, opts, &mut sink, cancel).await;
    Ran { result, events, vars }
}

async fn run(flow: &Flow, base: &str, inputs: &[(&str, Value)]) -> Ran {
    let env = FlowEnv { services: services(), collections: Vec::new(), base_dir: None };
    let opts = FlowRunOptions {
        inputs: inputs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        ..Default::default()
    };
    run_with(flow, &env, base, opts, &CancellationToken::new()).await
}

fn order(r: &FlowResult) -> Vec<String> {
    r.steps.iter().map(|s| format!("{}:{}", s.block, s.port)).collect()
}

fn codes(diags: &[Diagnostic], block: &str) -> Vec<&'static str> {
    diags.iter().filter(|d| d.block.as_deref() == Some(block)).map(|d| d.code).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn linear_flow_with_data_references() {
    let (base, _) = server().await;
    let mut f = flow(
        vec![
            post_json("login", "{{baseUrl}}/login", r#"{"username": "{{inputs.user}}", "password": "secret"}"#),
            set_var("token", "token", "{{blocks.login.output.json.token}}"),
            {
                let mut r = HttpRequest::new("GET", "{{baseUrl}}/me");
                r.headers.push(KeyValue::new("Authorization", "Bearer {{vars.token}}"));
                let mut b = request("me", r, Scripts::default());
                b.label = "Who am I".into();
                b
            },
            output("out-user", "user", "{{blocks.me.json.user}}"),
            output("out-status", "status", "{{blocks.who-am-i.output.status}}"),
            output("out-label", "header", "{{blocks[\"Who am I\"].output.headers.content-type}}"),
        ],
        &[
            ("start", "next", "login"),
            ("login", "success", "token"),
            ("token", "next", "me"),
            ("me", "success", "out-user"),
            ("out-user", "next", "out-status"),
            ("out-status", "next", "out-label"),
        ],
    );
    f.inputs = vec![FlowInput::new("user", "ada")];
    let ran = run(&f, &base, &[("user", json!("bob"))]).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["user"], json!("bob"));
    assert_eq!(r.outputs["status"], json!(200));
    assert_eq!(r.outputs["header"], json!("application/json"));
    assert_eq!(r.variables["token"], json!("tok-bob"));
    assert_eq!(r.requests, 2);
    assert_eq!(
        order(r),
        [
            "start:next",
            "login:success",
            "token:next",
            "me:success",
            "out-user:next",
            "out-status:next",
            "out-label:next"
        ]
    );
    // Events: Started first, Finished last, starts and finishes pair up.
    assert!(matches!(ran.events.first(), Some(FlowEvent::Started { .. })));
    assert!(matches!(ran.events.last(), Some(FlowEvent::Finished { success: true, .. })));
    let started = ran.events.iter().filter(|e| matches!(e, FlowEvent::BlockStarted { .. })).count();
    let finished = ran.events.iter().filter(|e| matches!(e, FlowEvent::BlockFinished { .. })).count();
    let executions = ran.events.iter().filter(|e| matches!(e, FlowEvent::Execution { .. })).count();
    assert_eq!((started, finished, executions), (7, 7, 2));
    // Defaults apply when the input is not passed.
    let ran = run(&f, &base, &[]).await;
    assert_eq!(ran.result.outputs["user"], json!("ada"));
}

#[tokio::test(flavor = "multi_thread")]
async fn condition_branches() {
    let (base, _) = server().await;
    let f = flow(
        vec![
            compare("big?", "{{inputs.n}}", CompareOp::Gt, "5"),
            output("big", "branch", "big"),
            output("small", "branch", "small"),
        ],
        &[("start", "next", "big?"), ("big?", "true", "big"), ("big?", "false", "small")],
    );
    let ran = run(&f, &base, &[("n", json!(10))]).await;
    assert_eq!(ran.result.outputs["branch"], json!("big"));
    assert_eq!(order(&ran.result), ["start:next", "big?:true", "big:next"]);
    let ran = run(&f, &base, &[("n", json!("3"))]).await;
    assert_eq!(ran.result.outputs["branch"], json!("small"));

    // JavaScript expression over the incoming block's output.
    let mut cond = Block::with_id(
        "ok?",
        "ok?",
        BlockConfig::Condition(ConditionConfig {
            mode: ConditionMode::Expression,
            expression: "input.status === 200 && input.json.hits > 0 && blocks.hit.port === 'success'".into(),
            ..Default::default()
        }),
    );
    cond.label = "Is OK".into();
    let f = flow(
        vec![get("hit", "{{baseUrl}}/hit"), cond, output("yes", "ok", "yes"), output("no", "ok", "no")],
        &[("start", "next", "hit"), ("hit", "success", "ok?"), ("ok?", "true", "yes"), ("ok?", "false", "no")],
    );
    let ran = run(&f, &base, &[]).await;
    assert!(ran.result.success, "{:#?}", ran.result);
    assert_eq!(ran.result.outputs["ok"], json!("yes"));
}

#[tokio::test(flavor = "multi_thread")]
async fn loop_with_nested_request_and_script() {
    let (base, _) = server().await;
    let mut detail = HttpRequest::new("GET", "{{baseUrl}}/items/{{loop.item.id}}");
    detail.headers.push(KeyValue::new("Authorization", "Bearer {{token}}"));
    let mut list = HttpRequest::new("GET", "{{baseUrl}}/items");
    list.headers.push(KeyValue::new("Authorization", "Bearer {{token}}"));
    let f = flow(
        vec![
            post_json("login", "{{baseUrl}}/login", r#"{"username": "ada", "password": "secret"}"#),
            set_var("save", "token", "{{blocks.login.output.json.token}}"),
            request("list", list, Scripts::default()),
            each("each", "{{blocks.list.output.json.items}}"),
            compare("active", "{{loop.item.active}}", CompareOp::Eq, "true"),
            request("detail", detail, Scripts::default()),
            script(
                "collect",
                "vars.details = (vars.details || []).concat([input.json.detail]);\n\
                 pm.test('item ' + loop.index, () => pm.expect(loop.item.id).to.eql(input.json.id));\n\
                 return vars.details.length;",
            ),
            output("details", "details", "{{vars.details}}"),
            output("iterations", "iterations", "{{blocks.each.output.iterations}}"),
        ],
        &[
            ("start", "next", "login"),
            ("login", "success", "save"),
            ("save", "next", "list"),
            ("list", "success", "each"),
            ("each", "item", "active"),
            ("active", "true", "detail"),
            ("detail", "success", "collect"),
            ("each", "done", "details"),
            ("details", "next", "iterations"),
        ],
    );
    let ran = run(&f, &base, &[]).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["details"], json!(["detail-1", "detail-3"]));
    assert_eq!(r.outputs["iterations"], json!(3));
    assert_eq!(r.requests, 4);
    assert_eq!(r.tests_passed, 2);
    let detail_steps: Vec<&StepRecord> = r.steps.iter().filter(|s| s.block == "detail").collect();
    assert_eq!(detail_steps.len(), 2);
    assert!(detail_steps.iter().all(|s| s.depth == 1 && s.status == Some(200)));
    assert_eq!(detail_steps.iter().map(|s| s.iteration).collect::<Vec<_>>(), [Some(0), Some(2)]);
    let loop_ports: Vec<&str> = r.steps.iter().filter(|s| s.block == "each").map(|s| s.port.as_str()).collect();
    assert_eq!(loop_ports, ["item", "item", "item", "done"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn script_sets_variables_and_returns_values() {
    let (base, _) = server().await;
    let f = flow(
        vec![
            script(
                "calc",
                "vars.a = 1;\n\
                 pm.variables.set('b', 'two');\n\
                 pm.environment.set('envx', 'E-' + pm.environment.get('baseUrl').length);\n\
                 pm.globals.set('g', 'G');\n\
                 console.log('hello from', flow.name);\n\
                 pm.test('math works', () => pm.expect(1 + 1).to.eql(2));\n\
                 return { sum: 1 + 2, input: inputs.x };",
            ),
            output("a", "a", "{{vars.a}}"),
            output("b", "b", "{{b}}"),
            output("s", "s", "{{blocks.calc.output.sum}}"),
            output("x", "x", "{{blocks.calc.output.input}}"),
            script("throws", "throw new Error('boom ' + vars.a);"),
            output("err", "err", "{{blocks.throws.output.error}}"),
        ],
        &[
            ("start", "next", "calc"),
            ("calc", "success", "a"),
            ("a", "next", "b"),
            ("b", "next", "s"),
            ("s", "next", "x"),
            ("x", "next", "throws"),
            ("throws", "failure", "err"),
        ],
    );
    let ran = run(&f, &base, &[("x", json!(42))]).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["a"], json!(1));
    assert_eq!(r.outputs["b"], json!("two"));
    assert_eq!(r.outputs["s"], json!(3));
    assert_eq!(r.outputs["x"], json!(42));
    assert!(r.outputs["err"].as_str().unwrap().contains("boom 1"), "{:?}", r.outputs["err"]);
    assert_eq!(r.variables["a"], json!(1));
    assert_eq!(r.tests_passed, 1);
    assert_eq!(ran.vars.environment["envx"], format!("E-{}", base.len()));
    assert_eq!(ran.vars.globals["g"], "G");
    assert!(
        ran.events.iter().any(|e| matches!(e, FlowEvent::Log { message, .. } if message == "hello from Test flow"))
    );
    assert_eq!(r.failures.len(), 1);
    assert!(r.failures[0].handled);
}

#[tokio::test(flavor = "multi_thread")]
async fn failure_ports() {
    let (base, _) = server().await;

    // Handled: failure edge leads to an Output.
    let f = flow(
        vec![get("r", "{{baseUrl}}/status/404"), output("handled", "handled", "{{blocks.r.output.status}}")],
        &[("start", "next", "r"), ("r", "failure", "handled")],
    );
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["handled"], json!(404));
    assert!(r.failures[0].handled);
    assert!(r.failures[0].message.contains("HTTP 404"), "{}", r.failures[0].message);

    // Unhandled: no failure edge stops the run.
    let f = flow(
        vec![get("r", "{{baseUrl}}/status/500"), output("never", "never", "x")],
        &[("start", "next", "r"), ("r", "success", "never")],
    );
    let ran = run(&f, &base, &[]).await;
    assert!(!ran.result.success);
    assert_eq!(order(&ran.result), ["start:next", "r:failure"]);
    assert_eq!(ran.result.unhandled_failures(), 1);
    assert!(ran.events.iter().any(|e| matches!(e, FlowEvent::Error { block: Some(b), .. } if b == "r")));

    // Handled, but the failure path produced no output.
    let f = flow(
        vec![get("r", "{{baseUrl}}/status/404"), log("note", "failed with {{blocks.r.output.status}}")],
        &[("start", "next", "r"), ("r", "failure", "note")],
    );
    let ran = run(&f, &base, &[]).await;
    assert!(!ran.result.success);
    assert!(ran.result.errors.iter().any(|e| e.contains("no output")), "{:?}", ran.result.errors);
    assert!(ran.events.iter().any(|e| matches!(e, FlowEvent::Log { message, .. } if message == "failed with 404")));

    // A failing test sends the request to `failure` and fails the run even
    // when the failure is handled.
    let scripts = Scripts {
        post_response: "pm.test('is 201', () => pm.response.to.have.status(201));".into(),
        ..Default::default()
    };
    let f = flow(
        vec![
            request("r", HttpRequest::new("GET", "{{baseUrl}}/status/200"), scripts),
            output("o", "o", "{{blocks.r.port}}"),
        ],
        &[("start", "next", "r"), ("r", "failure", "o")],
    );
    let r = run(&f, &base, &[]).await.result;
    assert!(!r.success);
    assert_eq!(r.outputs["o"], json!("failure"));
    assert_eq!((r.tests_passed, r.tests_failed), (0, 1));
    assert!(r.failures[0].message.contains("is 201"), "{}", r.failures[0].message);

    // success_status widens what counts as success.
    let mut b = get("r", "{{baseUrl}}/status/404");
    if let BlockConfig::Request(c) = &mut b.config {
        c.success_status = "2xx, 404".into();
    }
    let f = flow(vec![b, output("o", "o", "ok")], &[("start", "next", "r"), ("r", "success", "o")]);
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");

    // Connection errors fail too.
    let f = flow(vec![get("r", "http://127.0.0.1:1/nothing")], &[("start", "next", "r")]);
    let r = run(&f, &base, &[]).await.result;
    assert!(!r.success);
    assert_eq!(r.steps[1].port, "failure");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_requests_and_delays() {
    let (base, _) = server().await;
    let env = FlowEnv { services: services(), collections: Vec::new(), base_dir: None };
    for blocks in [
        vec![get("slow", "{{baseUrl}}/slow"), output("o", "o", "x")],
        vec![
            Block::with_id("slow", "Wait", BlockConfig::Delay(DelayConfig { ms: "10000".into() })),
            output("o", "o", "x"),
        ],
    ] {
        let port = if matches!(blocks[0].config, BlockConfig::Delay(_)) { "next" } else { "success" };
        let f = flow(blocks, &[("start", "next", "slow"), ("slow", port, "o")]);
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c.cancel();
        });
        let t = Instant::now();
        let ran = run_with(&f, &env, &base, FlowRunOptions::default(), &cancel).await;
        assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
        assert!(ran.result.cancelled);
        assert!(!ran.result.success);
        assert!(!ran.result.outputs.contains_key("o"));
        assert!(matches!(ran.events.last(), Some(FlowEvent::Finished { success: false, .. })));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn max_steps_and_loop_guards() {
    let (base, _) = server().await;
    // A cycle without a Loop: validation flags it and the step limit stops it.
    let f = flow(
        vec![template("a", "a"), template("b", "b")],
        &[("start", "next", "a"), ("a", "success", "b"), ("b", "success", "a")],
    );
    let diags = validate(&f, None);
    assert_eq!(codes(&diags, "a"), ["cycle"]);
    assert_eq!(codes(&diags, "b"), ["cycle"]);
    let env = FlowEnv { services: services(), collections: Vec::new(), base_dir: None };
    let opts = FlowRunOptions { max_steps: Some(25), ..Default::default() };
    let ran = run_with(&f, &env, &base, opts, &CancellationToken::new()).await;
    let r = &ran.result;
    assert!(!r.success);
    assert_eq!(r.steps.len(), 25);
    assert!(r.errors[0].contains("stopped after 25 steps"), "{:?}", r.errors);

    // The flow's own setting applies when the caller sets none.
    let mut f2 = f.clone();
    f2.settings.max_steps = Some(7);
    let r = run(&f2, &base, &[]).await.result;
    assert_eq!(r.steps.len(), 7);

    // Runaway loops fail before running.
    let f = flow(
        vec![count_loop("loop", "5000"), log("body", "x")],
        &[("start", "next", "loop"), ("loop", "item", "body")],
    );
    let r = run(&f, &base, &[]).await.result;
    assert!(!r.success);
    assert!(r.failures[0].message.contains("exceed the limit of 1000"), "{:?}", r.failures);
    let mut b = each("loop", "{{inputs.items}}");
    if let BlockConfig::Loop(c) = &mut b.config {
        c.max_iterations = Some(2);
    }
    let f = flow(vec![b, log("body", "x")], &[("start", "next", "loop"), ("loop", "item", "body")]);
    let r = run(&f, &base, &[("items", json!([1, 2, 3]))]).await.result;
    assert!(!r.success);
    assert!(r.failures[0].message.contains("limit of 2"), "{:?}", r.failures);
    // Not an array.
    let r = run(&f, &base, &[("items", json!("nope"))]).await.result;
    assert!(r.failures[0].message.contains("not a JSON array"), "{:?}", r.failures);
}

#[tokio::test(flavor = "multi_thread")]
async fn loop_break_back_edge_and_js_items() {
    let (base, _) = server().await;
    let f = flow(
        vec![
            count_loop("loop", "10"),
            compare("stop?", "{{loop.index}}", CompareOp::Gte, "2"),
            script("count", "vars.n = (vars.n || 0) + 1; return loop.item;"),
            output("iterations", "iterations", "{{blocks.loop.output.iterations}}"),
            output("n", "n", "{{vars.n}}"),
            output("broken", "broken", "{{blocks.loop.output.broken}}"),
        ],
        &[
            ("start", "next", "loop"),
            ("loop", "item", "stop?"),
            ("stop?", "true", "loop:break"),
            ("stop?", "false", "count"),
            // Closing the loop visually is allowed and just ends the path.
            ("count", "success", "loop"),
            ("loop", "done", "iterations"),
            ("iterations", "next", "n"),
            ("n", "next", "broken"),
        ],
    );
    let diags = validate(&f, None);
    assert!(!has_errors(&diags), "{diags:#?}");
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["iterations"], json!(3));
    assert_eq!(r.outputs["n"], json!(2));
    assert_eq!(r.outputs["broken"], json!(true));

    // Items from a JavaScript expression; nested loops see their own item.
    let mut outer = each("outer", "[1, 2].map(n => ({ n }))");
    outer.label = "Outer".into();
    let f = flow(
        vec![
            outer,
            each("inner", "{{inputs.letters}}"),
            script("pair", "vars.pairs = (vars.pairs || []).concat([blocks.Outer.output.item.n + loop.item]);"),
            output("pairs", "pairs", "{{vars.pairs}}"),
        ],
        &[
            ("start", "next", "outer"),
            ("outer", "item", "inner"),
            ("inner", "item", "pair"),
            ("outer", "done", "pairs"),
        ],
    );
    let r = run(&f, &base, &[("letters", json!(["a", "b"]))]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["pairs"], json!(["1a", "1b", "2a", "2b"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn fan_out_runs_top_to_bottom_depth_first() {
    let (base, _) = server().await;
    let f = flow(
        vec![
            log("low", "low").at(0.0, 200.0),
            log("high", "high").at(0.0, 0.0),
            log("mid", "mid").at(0.0, 100.0),
            log("after-high", "after").at(200.0, 0.0),
        ],
        &[
            ("start", "next", "low"),
            ("start", "next", "high"),
            ("start", "next", "mid"),
            ("high", "next", "after-high"),
        ],
    );
    let r = run(&f, &base, &[]).await.result;
    assert_eq!(order(&r), ["start:next", "high:next", "after-high:next", "mid:next", "low:next"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_request_runs_the_full_pipeline() {
    let (base, _) = server().await;
    let mut c = Collection::new("API");
    c.variables.push(Variable::new("apiBase", &base));
    c.auth = Auth::bearer("{{token}}");
    let mut me = RequestItem::new("Me", HttpRequest::new("GET", "{{apiBase}}/me"));
    me.scripts.post_response = "pm.test('is ada', () => pm.expect(pm.response.json().user).to.eql('ada'));\n\
         pm.collectionVariables.set('seen', 'yes');\n\
         pm.variables.set('fromScript', pm.response.json().user.toUpperCase());"
        .into();
    let me_id = me.id.clone();
    c.items.push(Item::Request(me));
    let cid = c.id.clone();
    let saved = Block::with_id(
        "me",
        "Me",
        BlockConfig::Request(RequestConfig {
            source: RequestSource::Saved(ItemRef::new(&cid, &me_id)),
            ..Default::default()
        }),
    );
    let f = flow(
        vec![
            post_json("login", "{{baseUrl}}/login", r#"{"username": "ada", "password": "secret"}"#),
            set_var("save", "token", "{{blocks.login.output.json.token}}"),
            saved,
            output("user", "user", "{{blocks.me.output.json.user}}"),
            output("from", "from", "{{fromScript}}"),
        ],
        &[
            ("start", "next", "login"),
            ("login", "success", "save"),
            ("save", "next", "me"),
            ("me", "success", "user"),
            ("user", "next", "from"),
        ],
    );
    let collections = vec![c];
    assert!(!has_errors(&validate(&f, Some(&collections))));
    let env = FlowEnv { services: services(), collections, base_dir: None };
    let ran = run_with(&f, &env, &base, FlowRunOptions::default(), &CancellationToken::new()).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["user"], json!("ada"));
    assert_eq!(r.outputs["from"], json!("ADA"));
    assert_eq!(r.tests_passed, 1);
    assert_eq!(r.collection_variables[&cid]["seen"], "yes");

    // A missing saved request is a validation error and a block failure.
    let f = flow(
        vec![Block::with_id(
            "gone",
            "Gone",
            BlockConfig::Request(RequestConfig {
                source: RequestSource::Saved(ItemRef::new("", "nope")),
                ..Default::default()
            }),
        )],
        &[("start", "next", "gone")],
    );
    assert_eq!(codes(&validate(&f, Some(&env.collections)), "gone"), ["request_not_found"]);
    let r = run_with(&f, &env, &base, FlowRunOptions::default(), &CancellationToken::new()).await.result;
    assert!(!r.success);
    assert!(r.failures[0].message.contains("not found"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_block_inline_and_saved() {
    let (base, mock) = server().await;
    let ai = |id: &str, prompt: &str, require_json: bool| {
        Block::with_id(
            id,
            id,
            BlockConfig::Ai(AiBlockConfig {
                provider: Some(AiProvider::OpenAiCompatible),
                base_url: "{{baseUrl}}/v1".into(),
                model: "test-model".into(),
                system: "Be brief.".into(),
                prompt: prompt.into(),
                require_json,
                ..Default::default()
            }),
        )
    };
    let f = flow(
        vec![
            ai("ask", "Hello {{inputs.name}} from {{flow.name}}", false),
            output("text", "text", "{{blocks.ask.output.text}}"),
            ai("json", "json please", true),
            output("ok", "ok", "{{blocks.json.output.json.ok}}"),
            ai("bad", "plain text", true),
            output("bad-port", "bad", "{{blocks.bad.port}}"),
        ],
        &[
            ("start", "next", "ask"),
            ("ask", "success", "text"),
            ("text", "next", "json"),
            ("json", "success", "ok"),
            ("ok", "next", "bad"),
            ("bad", "failure", "bad-port"),
        ],
    );
    let r = run(&f, &base, &[("name", json!("Ada"))]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["text"], json!("SUMMARY[Hello Ada from Test flow]"));
    assert_eq!(r.outputs["ok"], json!(true));
    assert_eq!(r.outputs["bad"], json!("failure"));
    let first = mock.ai.lock().unwrap()[0].clone();
    assert_eq!(first["model"], "test-model");
    assert_eq!(first["messages"][0]["content"], "Be brief.");

    // Saved AI request: its settings, the block's prompt appended.
    let mut c = Collection::new("AI");
    c.variables.push(Variable::new("aiModel", "saved-model"));
    let req = AiRequest {
        provider: AiProvider::OpenAiCompatible,
        base_url: format!("{base}/v1"),
        model: "{{aiModel}}".into(),
        messages: vec![AiMessage { role: "user".into(), content: "Context first".into() }],
        ..Default::default()
    };
    let item = RequestItem {
        id: "saved-ai".into(),
        name: "Saved AI".into(),
        description: String::new(),
        request: RequestKind::Ai(req),
        scripts: Scripts::default(),
        examples: Vec::new(),
    };
    c.items.push(Item::Request(item));
    let block = Block::with_id(
        "saved",
        "Saved",
        BlockConfig::Ai(AiBlockConfig {
            request: Some(ItemRef::new("", "saved-ai")),
            prompt: "Then {{inputs.name}}".into(),
            ..Default::default()
        }),
    );
    let f = flow(
        vec![block, output("t", "t", "{{blocks.saved.output.text}}")],
        &[("start", "next", "saved"), ("saved", "success", "t")],
    );
    let env = FlowEnv { services: services(), collections: vec![c], base_dir: None };
    assert!(!has_errors(&validate(&f, Some(&env.collections))));
    let opts = FlowRunOptions { inputs: IndexMap::from([("name".to_string(), json!("Bob"))]), ..Default::default() };
    let r = run_with(&f, &env, &base, opts, &CancellationToken::new()).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["t"], json!("SUMMARY[Then Bob]"));
    let last = mock.ai.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["model"], "saved-model");
    assert_eq!(last["messages"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn example_flow_runs_both_paths() {
    let (base, mock) = server().await;
    let text = std::fs::read_to_string(example_path()).unwrap();
    let f = storage::from_json(&text).unwrap();
    assert!(!has_errors(&validate(&f, Some(&[]))), "{:#?}", validate(&f, Some(&[])));
    let env = FlowEnv { services: services(), collections: Vec::new(), base_dir: None };
    let mut vars = scopes(&base);
    vars.environment.insert("aiBaseUrl".into(), format!("{base}/v1"));
    vars.environment.insert("aiModel".into(), "m".into());
    let mut sink = |_e: FlowEvent| {};
    let r = run_flow(&env, &f, &mut vars, FlowRunOptions::default(), &mut sink, &CancellationToken::new()).await;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["active_items"], json!(2));
    let summary = r.outputs["summary"].as_str().unwrap();
    assert!(summary.starts_with("SUMMARY[Summarize these items: [{"), "{summary}");
    assert!(summary.contains("\"detail\":\"detail-1\"") && summary.contains("detail-3"), "{summary}");
    assert_eq!(mock.ai.lock().unwrap().len(), 1);

    // Wrong password: the failure path logs and fails the run explicitly.
    let opts =
        FlowRunOptions { inputs: IndexMap::from([("password".to_string(), json!("wrong"))]), ..Default::default() };
    let r = run_flow(&env, &f, &mut vars, opts, &mut sink, &CancellationToken::new()).await;
    assert!(!r.success);
    assert!(r.outputs["error"].as_str().unwrap().contains("401"), "{:?}", r.outputs);
    assert_eq!(r.tests_failed, 1);
}

fn example_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/flows/login-list-summarize.flow.json")
}

#[test]
fn validation_diagnostics() {
    let mut f = flow(
        vec![
            get("no-url", ""),
            Block::with_id(
                "saved",
                "Saved",
                BlockConfig::Request(RequestConfig {
                    source: RequestSource::Saved(ItemRef::new("c1", "missing")),
                    ..Default::default()
                }),
            ),
            template("ghost", "{{blocks.ghost-block.output}} {{inputs.undeclared}}"),
            log("lonely", "hi"),
            count_loop("loop", "3"),
            log("body", "{{loop.item}}"),
            log("outside", "{{loop.index}} {{blocks.later.output}}"),
            log("later", "later"),
            Block::with_id("start2", "Start 2", BlockConfig::Start(StartConfig {})),
            output("noname", "", "x"),
            Block::with_id(
                "cond",
                "Cond",
                BlockConfig::Condition(ConditionConfig { mode: ConditionMode::Expression, ..Default::default() }),
            ),
            compare("regex", "{{input}}", CompareOp::Matches, "("),
        ],
        &[
            ("start", "next", "no-url"),
            ("no-url", "success", "saved"),
            ("saved", "success", "ghost"),
            ("ghost", "success", "loop"),
            ("loop", "item", "body"),
            ("body", "next", "loop"),
            ("loop", "done", "outside"),
            ("outside", "next", "later"),
            ("start", "next", "noname:bogus"),
            ("noname", "next", "cond"),
            ("cond", "true", "regex"),
            ("start", "next", "nowhere"),
        ],
    );
    f.inputs = vec![FlowInput::new("declared", "")];
    let diags = validate(&f, Some(&[]));
    assert!(has_errors(&diags));
    assert_eq!(codes(&diags, "no-url"), ["missing_url"]);
    assert_eq!(codes(&diags, "saved"), ["request_not_found"]);
    assert_eq!(codes(&diags, "ghost"), ["unknown_block_ref", "undeclared_input"]);
    assert_eq!(codes(&diags, "lonely"), ["unreachable"]);
    assert!(codes(&diags, "loop").is_empty(), "{:?}", codes(&diags, "loop"));
    assert!(codes(&diags, "body").is_empty(), "{:?}", codes(&diags, "body"));
    assert_eq!(codes(&diags, "outside"), ["loop_outside", "not_upstream"]);
    assert_eq!(codes(&diags, "start2"), ["multiple_starts"]);
    assert!(codes(&diags, "noname").contains(&"missing_name"));
    assert!(codes(&diags, "noname").contains(&"unknown_port"));
    assert!(codes(&diags, "noname").contains(&"unreachable"));
    assert!(codes(&diags, "cond").contains(&"missing_expression"));
    assert!(codes(&diags, "regex").contains(&"invalid_regex"));
    assert!(diags.iter().any(|d| d.code == "unknown_block" && d.edge == Some(11)));
    let grouped = merpati_flows::validate::by_block(&diags);
    assert!(grouped.contains_key("ghost"));
    assert!(grouped["start"].iter().any(|d| d.code == "unknown_block"));
    // Without collections, saved references are not checked.
    assert!(!codes(&validate(&f, None), "saved").contains(&"request_not_found"));
    // No Start at all.
    let mut empty = Flow::new("Empty");
    empty.blocks.clear();
    assert_eq!(validate(&empty, None)[0].code, "no_start");
}

#[test]
fn storage_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut f = Flow::new("My Flow");
    f.description = "All block kinds".into();
    f.inputs =
        vec![FlowInput::new("user", "ada"), FlowInput { name: "token".into(), required: true, ..Default::default() }];
    f.settings.max_steps = Some(500);
    for (i, kind) in BlockKind::ALL.iter().enumerate().skip(1) {
        let b = Block::new(kind.default_config(), kind.label(), (i as f32 * 100.0, 50.5));
        let id = b.id.clone();
        let prev = f.blocks.last().unwrap().id.clone();
        let port = f.blocks.last().unwrap().kind().output_ports()[0];
        f.add_block(b);
        f.connect(&prev, port, &id);
    }
    let path = storage::save(root, &f).unwrap();
    assert_eq!(path, root.join("flows/my-flow.flow.json"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("{\n  \"version\": 1,\n  \"id\": "), "{text}");
    assert!(text.ends_with("}\n"));
    let block_keys = text.find("\"kind\"").unwrap() < text.find("\"label\": \"Request\"").unwrap();
    assert!(block_keys);
    let loaded = storage::load(&path).unwrap();
    assert_eq!(loaded, f);
    assert_eq!(storage::to_json(&loaded), text, "serialization is stable");

    // Same name, different flow: a new file.
    let other = Flow::new("My Flow");
    let other_path = storage::save(root, &other).unwrap();
    assert_eq!(other_path, root.join("flows/my-flow-2.flow.json"));
    // Saving again keeps the file.
    assert_eq!(storage::save(root, &f).unwrap(), path);

    let (list, errors) = storage::list(root);
    assert!(errors.is_empty());
    assert_eq!(list.len(), 2);

    // Rename moves the file.
    let renamed = storage::rename(root, &f.id, "Renamed flow").unwrap();
    assert_eq!(renamed, root.join("flows/renamed-flow.flow.json"));
    assert!(!path.exists());
    assert_eq!(storage::load(&renamed).unwrap().name, "Renamed flow");

    // Find by id, name, slug and path.
    assert_eq!(storage::find(root, &f.id).unwrap().path, renamed);
    assert_eq!(storage::find(root, "renamed FLOW").unwrap().path, renamed);
    assert_eq!(storage::find(root, "renamed-flow").unwrap().path, renamed);
    assert_eq!(storage::find(root, renamed.to_str().unwrap()).unwrap().flow.id, f.id);
    assert_eq!(storage::find(root, "flows/my-flow-2.flow.json").unwrap().flow.id, other.id);
    let err = storage::find(root, "nope").unwrap_err().to_string();
    assert!(err.contains("not found") && err.contains("\"Renamed flow\""), "{err}");

    // A broken file is reported, the rest still lists.
    std::fs::write(root.join("flows/broken.flow.json"), "{ nope").unwrap();
    let (list, errors) = storage::list(root);
    assert_eq!((list.len(), errors.len()), (2, 1));

    assert!(storage::delete(root, &f.id).unwrap());
    assert!(!storage::delete(root, &f.id).unwrap());
    assert!(!renamed.exists());

    // Unknown kinds and bad configs are load errors with the block id.
    let bad = r#"{"name": "x", "blocks": [{"id": "b1", "kind": "condition", "config": {"operator": "~"}}]}"#;
    let e = storage::from_json(bad).unwrap_err().to_string();
    assert!(e.contains("b1"), "{e}");
    assert!(storage::from_json(r#"{"name": "x", "blocks": [{"id": "b", "kind": "teleport"}]}"#).is_err());
    // Operator aliases.
    let ok = r#"{"name": "x", "blocks": [{"id": "c", "kind": "condition", "config": {"operator": ">="}}]}"#;
    let f = storage::from_json(ok).unwrap();
    assert!(matches!(&f.blocks[0].config, BlockConfig::Condition(c) if c.op == CompareOp::Gte));
}

#[test]
fn example_flow_is_canonical() {
    let path = example_path();
    let text = std::fs::read_to_string(&path).unwrap();
    let f = storage::from_json(&text).unwrap();
    let canonical = storage::to_json(&f);
    if std::env::var_os("MERPATI_BLESS").is_some() {
        std::fs::write(&path, &canonical).unwrap();
        return;
    }
    assert_eq!(text, canonical, "run with MERPATI_BLESS=1 to rewrite {}", path.display());
}

// ---------------------------------------------------------------------------
// Parallel branches, Join and AI streaming
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn parallel_branches_run_at_the_same_time() {
    let (base, _) = server().await;
    // Two 300 ms delays.
    let f = flow(
        vec![
            parallel("fan"),
            delay("wait-a", "300").at(260.0, 0.0),
            delay("wait-b", "300").at(260.0, 120.0),
            join("join"),
            output("count", "count", "{{blocks.join.output.count}}"),
            output("shape", "shape", "{{blocks.join.output}}"),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "wait-b"),
            ("fan", "branches", "wait-a"),
            ("wait-a", "next", "join"),
            ("wait-b", "next", "join"),
            ("join", "next", "count"),
            ("count", "next", "shape"),
        ],
    );
    let diags = validate(&f, None);
    assert!(diags.is_empty(), "{diags:#?}");
    let ran = run(&f, &base, &[]).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    assert!(r.duration_ms >= 299.0 && r.duration_ms < 550.0, "took {} ms", r.duration_ms);
    assert_eq!(r.outputs["count"], json!(2));
    // Branch order is canvas order (wait-a is above wait-b), not edge order.
    assert_eq!(
        r.outputs["shape"],
        json!({
            "results": {"wait-a": {"ms": 300.0}, "wait-b": {"ms": 300.0}},
            "items": [{"ms": 300.0}, {"ms": 300.0}],
            "sources": ["wait-a", "wait-b"],
            "count": 2,
            "branches": 2,
        })
    );
    // Both delays started before either finished; events still pair by step.
    assert!(event_index(&ran.events, true, "wait-b") < event_index(&ran.events, false, "wait-a"));
    assert_steps_pair(&ran.events);
    assert_eq!(
        order(r),
        ["start:next", "fan:branches", "wait-a:next", "wait-b:next", "join:next", "count:next", "shape:next"]
    );
    let (a, b, j) = (step(r, "wait-a")[0], step(r, "wait-b")[0], step(r, "join")[0]);
    assert_eq!((a.branch.as_deref(), a.depth), (Some("1"), 1));
    assert_eq!((b.branch.as_deref(), b.depth), (Some("2"), 1));
    assert_eq!((j.branch.as_deref(), j.depth), (None, 0));
    assert!(ran.events.iter().any(
        |e| matches!(e, FlowEvent::BlockStarted { block, branch: Some(b), depth: 1, .. } if block == "wait-b" && b == "2")
    ));

    // Two 300 ms requests overlap too; the Parallel passes its input on.
    // (A first run builds the HTTP client, which takes a while once.)
    let env = FlowEnv { services: services(), collections: Vec::new(), base_dir: None };
    let warm_up = flow(vec![get("w", "{{baseUrl}}/sleep/0")], &[("start", "next", "w")]);
    let ran = run_with(&warm_up, &env, &base, FlowRunOptions::default(), &CancellationToken::new()).await;
    assert!(ran.result.success, "{:#?}", ran.result);
    let f = flow(
        vec![
            template("id", "300"),
            parallel("fan"),
            get("a", "{{baseUrl}}/sleep/{{input}}").at(0.0, 0.0),
            get("b", "{{baseUrl}}/sleep/{{blocks.id.output}}").at(0.0, 100.0),
            join("join"),
            output("slept", "slept", "{{blocks.join.output.results.a.json.slept}}"),
            output("b-status", "b", "{{blocks.b.output.status}}"),
        ],
        &[
            ("start", "next", "id"),
            ("id", "success", "fan"),
            ("fan", "branches", "a"),
            ("fan", "branches", "b"),
            ("a", "success", "join"),
            ("b", "success", "join"),
            ("join", "next", "slept"),
            ("slept", "next", "b-status"),
        ],
    );
    assert!(validate(&f, None).is_empty(), "{:#?}", validate(&f, None));
    let r = run_with(&f, &env, &base, FlowRunOptions::default(), &CancellationToken::new()).await.result;
    assert!(r.success, "{r:#?}");
    assert!(r.duration_ms < 580.0, "took {} ms", r.duration_ms);
    let (a, b) = (step(&r, "a")[0], step(&r, "b")[0]);
    assert!(a.started_ms < b.started_ms + b.duration_ms && b.started_ms < a.started_ms + a.duration_ms, "{a:?} {b:?}");
    assert_eq!(r.outputs["slept"], json!(300));
    assert_eq!(r.outputs["b"], json!(200));
    assert_eq!(r.requests, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn branches_start_from_a_snapshot_and_merge_in_branch_order() {
    let (base, _) = server().await;
    let env_var = |id: &str, name: &str, value: &str| {
        Block::with_id(
            id,
            id,
            BlockConfig::SetVariable(SetVariableConfig {
                name: name.into(),
                value: value.into(),
                scope: VariableScope::Environment,
            }),
        )
    };
    let f = flow(
        vec![
            set_var("x0", "x", "zero"),
            parallel("fan"),
            // Branch 1 (top) sets x later in time than branch 2.
            delay("a1", "150").at(0.0, 0.0),
            set_var("a2", "x", "one").at(200.0, 0.0),
            script("a3", "pm.environment.set('fromA', 'A'); vars.onlyA = 1; return vars.x;").at(400.0, 0.0),
            // Branch 2 never sees branch 1's changes.
            template("b1", "{{vars.x}}").at(0.0, 200.0),
            delay("b2", "250").at(200.0, 200.0),
            template("b3", "{{vars.onlyA}}").at(400.0, 200.0),
            set_var("b4", "x", "two").at(600.0, 200.0),
            env_var("b5", "fromB", "B").at(800.0, 200.0),
            join("join"),
            output("x", "x", "{{vars.x}}"),
            output("env", "env", "{{fromA}}{{fromB}}"),
            output("results", "results", "{{blocks.join.output.results}}"),
        ],
        &[
            ("start", "next", "x0"),
            ("x0", "next", "fan"),
            ("fan", "branches", "a1"),
            ("fan", "branches", "b1"),
            ("a1", "next", "a2"),
            ("a2", "next", "a3"),
            ("a3", "success", "join"),
            ("b1", "success", "b2"),
            ("b2", "next", "b3"),
            ("b3", "success", "b4"),
            ("b4", "next", "b5"),
            ("b5", "next", "join"),
            ("join", "next", "x"),
            ("x", "next", "env"),
            ("env", "next", "results"),
        ],
    );
    let ran = run(&f, &base, &[]).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    // Both branches set x: the later branch (bottom) wins, whatever the timing.
    assert_eq!(r.outputs["x"], json!("two"));
    assert_eq!(r.variables["x"], json!("two"));
    assert_eq!(r.variables["onlyA"], json!(1));
    assert_eq!(r.outputs["env"], json!("AB"));
    assert_eq!(ran.vars.environment["fromA"], "A");
    assert_eq!(ran.vars.environment["fromB"], "B");
    assert_eq!(r.outputs["results"], json!({"a3": "one", "b5": "B"}));
    assert_eq!(*r.blocks["b1"].output, json!("zero"));
    assert_eq!(*r.blocks["b3"].output, Value::Null);

    // A block reached from both branches runs in each; its record adds up.
    let f = flow(
        vec![
            parallel("fan"),
            template("t1", "one").at(0.0, 0.0),
            template("t2", "two").at(0.0, 100.0),
            template("shared", "{{input}}!"),
            join("join"),
            output("runs", "runs", "{{blocks.shared.runs}}"),
            output("last", "last", "{{blocks.shared.output}}"),
            output("join-out", "join", "{{blocks.join.output}}"),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "t1"),
            ("fan", "branches", "t2"),
            ("t1", "success", "shared"),
            ("t2", "success", "shared"),
            ("shared", "success", "join"),
            ("join", "next", "runs"),
            ("runs", "next", "last"),
            ("last", "next", "join-out"),
        ],
    );
    assert!(!has_errors(&validate(&f, None)), "{:#?}", validate(&f, None));
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["runs"], json!(2));
    assert_eq!(r.outputs["last"], json!("two!"));
    assert_eq!(
        r.outputs["join"],
        json!({
            "results": {"shared": "two!"},
            "items": ["one!", "two!"],
            "sources": ["shared", "shared"],
            "count": 2,
            "branches": 2,
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_in_parallel_branches() {
    let (base, _) = server().await;

    // Handled: the failure path leads into the Join.
    let f = flow(
        vec![
            parallel("fan"),
            get("bad", "{{baseUrl}}/status/404").at(0.0, 0.0),
            template("fallback", "fallback").at(200.0, 0.0),
            get("good", "{{baseUrl}}/hit").at(0.0, 100.0),
            join("join"),
            output("o", "o", "{{blocks.join.output.results}}"),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "bad"),
            ("fan", "branches", "good"),
            ("bad", "failure", "fallback"),
            ("fallback", "success", "join"),
            ("good", "success", "join"),
            ("join", "next", "o"),
        ],
    );
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["o"]["fallback"], json!("fallback"));
    assert_eq!(r.outputs["o"]["good"]["json"]["hits"], json!(1));
    assert_eq!(r.failures.len(), 1);
    assert!(r.failures[0].handled);

    // Unhandled: the run stops at once; the other branch is interrupted and
    // the Join never fires.
    let blocks = || {
        vec![
            parallel("fan"),
            get("boom", "{{baseUrl}}/status/500").at(0.0, 0.0),
            delay("slow", "5000").at(0.0, 100.0),
            join("join"),
            output("o", "o", "{{blocks.join.output.count}}/{{blocks.join.output.branches}}"),
        ]
    };
    let edges = [
        ("start", "next", "fan"),
        ("fan", "branches", "boom"),
        ("fan", "branches", "slow"),
        ("boom", "success", "join"),
        ("slow", "next", "join"),
        ("join", "next", "o"),
    ];
    let f = flow(blocks(), &edges);
    let ran = run(&f, &base, &[]).await;
    let r = &ran.result;
    assert!(!r.success);
    assert!(!r.cancelled);
    assert!(r.duration_ms < 2000.0, "took {} ms", r.duration_ms);
    assert_eq!(r.unhandled_failures(), 1);
    assert_eq!(r.failures[0].block, "boom");
    // The other branch was interrupted, or the run stopped before it started.
    if let Some(slow) = step(r, "slow").first() {
        assert!(slow.failed && slow.error.as_deref().unwrap_or("").contains("stopped"), "{slow:?}");
    }
    assert!(step(r, "join").is_empty());
    assert!(r.outputs.is_empty());
    assert_steps_pair(&ran.events);

    // continue_on_failure: the other branch finishes and the Join fires
    // without the failed one; the run still fails.
    let mut f = flow(blocks(), &edges);
    if let Some(BlockConfig::Delay(c)) = f.block_mut("slow").map(|b| &mut b.config) {
        c.ms = "100".into();
    }
    f.settings.continue_on_failure = true;
    let r = run(&f, &base, &[]).await.result;
    assert!(!r.success);
    assert_eq!(r.outputs["o"], json!("1/2"));
    assert_eq!(r.unhandled_failures(), 1);
    assert!(!step(&r, "slow")[0].failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_every_branch() {
    let (base, _) = server().await;
    let env = FlowEnv { services: services(), collections: Vec::new(), base_dir: None };
    let f = flow(
        vec![
            parallel("fan"),
            delay("wait", "10000").at(0.0, 0.0),
            get("slow", "{{baseUrl}}/slow").at(0.0, 100.0),
            script("spin", "await new Promise(r => setTimeout(r, 8000)); return 1;").at(0.0, 200.0),
            join("join"),
            output("o", "o", "x"),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "wait"),
            ("fan", "branches", "slow"),
            ("fan", "branches", "spin"),
            ("wait", "next", "join"),
            ("slow", "success", "join"),
            ("spin", "success", "join"),
            ("join", "next", "o"),
        ],
    );
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.cancel();
    });
    let t = Instant::now();
    let ran = run_with(&f, &env, &base, FlowRunOptions::default(), &cancel).await;
    assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
    let r = &ran.result;
    assert!(r.cancelled);
    assert!(!r.success);
    assert!(r.outputs.is_empty());
    assert!(r.failures.is_empty(), "{:?}", r.failures);
    for id in ["wait", "slow"] {
        assert!(step(r, id)[0].failed, "{id}");
    }
    assert!(step(r, "join").is_empty());
    assert_steps_pair(&ran.events);
    assert!(matches!(ran.events.last(), Some(FlowEvent::Finished { success: false, .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_and_loops() {
    let (base, _) = server().await;

    // A Parallel inside a loop body runs its group once per iteration.
    let f = flow(
        vec![
            count_loop("loop", "3"),
            parallel("fan"),
            template("a", "{{loop.index}}-a").at(0.0, 0.0),
            template("b", "{{loop.index}}-b").at(0.0, 100.0),
            join("join"),
            script("collect", "vars.seen = (vars.seen || []).concat(input.items);"),
            output("seen", "seen", "{{vars.seen}}"),
        ],
        &[
            ("start", "next", "loop"),
            ("loop", "item", "fan"),
            ("fan", "branches", "a"),
            ("fan", "branches", "b"),
            ("a", "success", "join"),
            ("b", "success", "join"),
            ("join", "next", "collect"),
            ("loop", "done", "seen"),
        ],
    );
    assert!(validate(&f, None).is_empty(), "{:#?}", validate(&f, None));
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["seen"], json!(["0-a", "0-b", "1-a", "1-b", "2-a", "2-b"]));
    let a = step(&r, "a");
    assert_eq!(a.iter().map(|s| s.iteration).collect::<Vec<_>>(), [Some(0), Some(1), Some(2)]);
    assert!(a.iter().all(|s| s.depth == 2 && s.branch.as_deref() == Some("1")));
    assert!(step(&r, "join").iter().all(|s| s.depth == 1 && s.branch.is_none()));

    // A loop inside a branch; its `done` leads into the Join.
    let f = flow(
        vec![
            parallel("fan"),
            count_loop("inner", "3").at(0.0, 0.0),
            script("count", "vars.n = (vars.n || 0) + 1;"),
            delay("d", "50").at(0.0, 200.0),
            join("join"),
            output("n", "n", "{{vars.n}}"),
            output("sources", "sources", "{{blocks.join.output.sources}}"),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "inner"),
            ("fan", "branches", "d"),
            ("inner", "item", "count"),
            ("inner", "done", "join"),
            ("d", "next", "join"),
            ("join", "next", "n"),
            ("n", "next", "sources"),
        ],
    );
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["n"], json!(3));
    assert_eq!(r.outputs["sources"], json!(["inner", "d"]));
    assert!(step(&r, "count").iter().all(|s| s.depth == 2 && s.branch.as_deref() == Some("1")));

    // A break from inside a branch stops the loop when the group is done
    // and drops the rest of that iteration (the Join, which both branches can
    // reach, so it waits for the whole group).
    let blocks = || {
        vec![
            count_loop("loop", "10"),
            parallel("fan"),
            compare("stop?", "{{loop.index}}", CompareOp::Gte, "2").at(0.0, 0.0),
            compare("never", "1", CompareOp::Eq, "2").at(200.0, 0.0),
            template("t", "{{loop.index}}").at(0.0, 100.0),
            join("join"),
            script("collect", "vars.ts = (vars.ts || []).concat(input.items);"),
            output("iterations", "iterations", "{{blocks.loop.output.iterations}}"),
            output("ts", "ts", "{{vars.ts}}"),
        ]
    };
    let mut edges = vec![
        ("start", "next", "loop"),
        ("loop", "item", "fan"),
        ("fan", "branches", "stop?"),
        ("fan", "branches", "t"),
        ("stop?", "true", "loop:break"),
        ("t", "success", "join"),
        ("join", "next", "collect"),
        ("loop", "done", "iterations"),
        ("iterations", "next", "ts"),
    ];
    let early = flow(blocks(), &edges);
    edges.extend([("stop?", "false", "never"), ("never", "true", "join")]);
    let r = run(&flow(blocks(), &edges), &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["iterations"], json!(3));
    assert_eq!(r.outputs["ts"], json!([0, 1]));
    assert_eq!(r.blocks["loop"].output["broken"], json!(true));
    // When only the other branch can reach the Join, it does not wait for
    // the branch that breaks: it already ran in the last iteration.
    let r = run(&early, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["iterations"], json!(3));
    assert_eq!(r.outputs["ts"], json!([0, 1, 2]));
    assert_eq!(r.blocks["loop"].output["broken"], json!(true));

    // Nested Parallel: the inner Join belongs to the inner group.
    let f = flow(
        vec![
            parallel("outer"),
            parallel("inner").at(0.0, 0.0),
            template("x", "x").at(0.0, 0.0),
            template("y", "y").at(0.0, 50.0),
            join("inner-join").at(0.0, 0.0),
            template("z", "z").at(0.0, 300.0),
            join("outer-join").at(0.0, 400.0),
            output("o", "o", "{{blocks.outer-join.output}}"),
        ],
        &[
            ("start", "next", "outer"),
            ("outer", "branches", "inner"),
            ("outer", "branches", "z"),
            ("inner", "branches", "x"),
            ("inner", "branches", "y"),
            ("x", "success", "inner-join"),
            ("y", "success", "inner-join"),
            ("inner-join", "next", "outer-join"),
            ("z", "success", "outer-join"),
            ("outer-join", "next", "o"),
        ],
    );
    assert!(validate(&f, None).is_empty(), "{:#?}", validate(&f, None));
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["o"]["sources"], json!(["inner-join", "z"]));
    assert_eq!(r.outputs["o"]["results"]["inner-join"]["items"], json!(["x", "y"]));
    assert_eq!(step(&r, "x")[0].branch.as_deref(), Some("1.1"));
    assert_eq!(step(&r, "x")[0].depth, 2);
    assert_eq!(step(&r, "inner-join")[0].branch.as_deref(), Some("1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_branches_share_the_step_budget() {
    let (base, _) = server().await;
    let f = flow(
        vec![
            parallel("fan"),
            count_loop("l1", "100").at(0.0, 0.0),
            log("x1", "one"),
            count_loop("l2", "100").at(0.0, 100.0),
            log("x2", "two"),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "l1"),
            ("fan", "branches", "l2"),
            ("l1", "item", "x1"),
            ("l2", "item", "x2"),
        ],
    );
    let env = FlowEnv { services: services(), collections: Vec::new(), base_dir: None };
    let opts = FlowRunOptions { max_steps: Some(50), ..Default::default() };
    let ran = run_with(&f, &env, &base, opts, &CancellationToken::new()).await;
    let r = &ran.result;
    assert!(!r.success);
    assert_eq!(r.steps.len(), 50);
    assert_eq!(r.errors.iter().filter(|e| e.contains("stopped after 50 steps")).count(), 1, "{:?}", r.errors);
    // Steps are reported in step order; both branches made progress.
    assert!(r.steps.windows(2).all(|w| w[0].step < w[1].step));
    assert!(!step(r, "x1").is_empty() && !step(r, "x2").is_empty());
    assert_steps_pair(&ran.events);
}

#[tokio::test(flavor = "multi_thread")]
async fn max_concurrent_limits_running_branches() {
    let (base, _) = server().await;
    let mut f = flow(
        vec![
            parallel("fan"),
            delay("d1", "200").at(0.0, 0.0),
            delay("d2", "200").at(0.0, 100.0),
            delay("d3", "200").at(0.0, 200.0),
            join("join"),
            output("shape", "shape", "{{blocks.join.output}}"),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "d3"),
            ("fan", "branches", "d2"),
            ("fan", "branches", "d1"),
            ("d1", "next", "join"),
            ("d2", "next", "join"),
            ("d3", "next", "join"),
            ("join", "next", "shape"),
        ],
    );
    let shape = json!({
        "results": {"d1": {"ms": 200.0}, "d2": {"ms": 200.0}, "d3": {"ms": 200.0}},
        "items": [{"ms": 200.0}, {"ms": 200.0}, {"ms": 200.0}],
        "sources": ["d1", "d2", "d3"],
        "count": 3,
        "branches": 3,
    });
    let end = |s: &StepRecord| s.started_ms + s.duration_ms;

    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert!(r.duration_ms < 450.0, "unlimited took {} ms", r.duration_ms);
    assert_eq!(r.outputs["shape"], shape);

    // One at a time, in branch order.
    f.settings.max_concurrent = 1;
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert!(r.duration_ms >= 599.0, "took {} ms", r.duration_ms);
    assert_eq!(r.outputs["shape"], shape);
    let (d1, d2, d3) = (step(&r, "d1")[0], step(&r, "d2")[0], step(&r, "d3")[0]);
    assert!(d2.started_ms + 5.0 >= end(d1) && d3.started_ms + 5.0 >= end(d2), "{d1:?} {d2:?} {d3:?}");
    assert_eq!([d1.branch.as_deref(), d2.branch.as_deref(), d3.branch.as_deref()], [Some("1"), Some("2"), Some("3")]);

    // Two at a time: the third starts once one of them is done.
    f.settings.max_concurrent = 2;
    let r = run(&f, &base, &[]).await.result;
    assert!(r.success, "{r:#?}");
    assert!(r.duration_ms >= 399.0, "took {} ms", r.duration_ms);
    assert_eq!(r.outputs["shape"], shape);
    let (d1, d2, d3) = (step(&r, "d1")[0], step(&r, "d2")[0], step(&r, "d3")[0]);
    assert!(d1.started_ms < end(d2) && d2.started_ms < end(d1), "{d1:?} {d2:?}");
    assert!(d3.started_ms + 5.0 >= end(d1).min(end(d2)), "{d1:?} {d2:?} {d3:?}");

    // More than there are branches: no limit.
    f.settings.max_concurrent = 5;
    let r = run(&f, &base, &[]).await.result;
    assert!(r.duration_ms < 450.0, "took {} ms", r.duration_ms);
    assert_eq!(r.outputs["shape"], shape);
}

#[tokio::test(flavor = "multi_thread")]
async fn join_waits_only_for_the_branches_that_reach_it() {
    let (base, _) = server().await;
    // Branches 1 and 2 lead into the Join; branch 3 does not and takes long.
    // `delay_a` / `delay_c` shift the timing without changing the result.
    let build = |delay_a: &str, delay_c: &str| {
        flow(
            vec![
                parallel("fan"),
                delay("a0", delay_a).at(0.0, 0.0),
                set_var("a", "x", "A").at(200.0, 0.0),
                set_var("b", "y", "B").at(200.0, 100.0),
                delay("c", delay_c).at(0.0, 200.0),
                set_var("c2", "x", "C").at(200.0, 200.0),
                set_var("c3", "z", "C").at(400.0, 200.0),
                join("join").at(400.0, 50.0),
                script("seen", "return [vars.x, vars.y, vars.z ?? null];").at(600.0, 50.0),
                set_var("after", "x", "J").at(800.0, 50.0),
                output("seen-out", "seen", "{{blocks.seen.output}}").at(1000.0, 50.0),
                output("join-out", "join", "{{blocks.join.output}}").at(1200.0, 50.0),
            ],
            &[
                ("start", "next", "fan"),
                ("fan", "branches", "a0"),
                ("fan", "branches", "b"),
                ("fan", "branches", "c"),
                ("a0", "next", "a"),
                ("a", "next", "join"),
                ("b", "next", "join"),
                ("c", "next", "c2"),
                ("c2", "next", "c3"),
                ("join", "next", "seen"),
                ("seen", "success", "after"),
                ("after", "next", "seen-out"),
                ("seen-out", "next", "join-out"),
            ],
        )
    };
    let f = build("0", "400");
    assert!(validate(&f, None).is_empty(), "{:#?}", validate(&f, None));
    let ran = run(&f, &base, &[]).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    // The Join and what follows ran while branch 3 was still waiting.
    assert!(event_index(&ran.events, false, "seen-out") < event_index(&ran.events, false, "c"));
    assert_eq!(r.outputs["join"]["count"], json!(2));
    assert_eq!(r.outputs["join"]["branches"], json!(3));
    assert_eq!(r.outputs["join"]["sources"], json!(["a", "b"]));
    // It saw branches 1 and 2 but not branch 3.
    assert_eq!(r.outputs["seen"], json!(["A", "B", null]));
    // Branches are applied in order (3 overrides 1), then the Join's path.
    let finals = |r: &FlowResult| (r.variables["x"].clone(), r.variables["y"].clone(), r.variables["z"].clone());
    assert_eq!(finals(r), (json!("J"), json!("B"), json!("C")));
    let (j, s) = (step(r, "join")[0], step(r, "seen")[0]);
    assert_eq!((j.branch.as_deref(), j.depth), (Some("1+2"), 0));
    assert_eq!((s.branch.as_deref(), s.depth), (Some("1+2"), 0));
    assert_steps_pair(&ran.events);

    // Branch 3 finishing first, or one branch at a time, changes nothing.
    for (delay_a, delay_c, limit) in [("300", "0", 0), ("0", "100", 1), ("0", "0", 2)] {
        let mut f = build(delay_a, delay_c);
        f.settings.max_concurrent = limit;
        let r = run(&f, &base, &[]).await.result;
        assert!(r.success, "{r:#?}");
        assert_eq!(r.outputs["seen"], json!(["A", "B", null]), "{delay_a}/{delay_c}/{limit}");
        assert_eq!(finals(&r), (json!("J"), json!("B"), json!("C")), "{delay_a}/{delay_c}/{limit}");
    }

    // Chained: the second Join waits for the first one's path too, and its
    // arrivals come in member order (branches first).
    let f = flow(
        vec![
            parallel("fan"),
            template("a", "a").at(0.0, 0.0),
            join("j1").at(200.0, 0.0),
            template("x", "x").at(400.0, 0.0),
            delay("b", "150").at(0.0, 100.0),
            join("j2").at(600.0, 50.0),
            delay("c", "400").at(0.0, 200.0),
            output("o", "o", "{{blocks.j2.output}}"),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "a"),
            ("fan", "branches", "b"),
            ("fan", "branches", "c"),
            ("a", "success", "j1"),
            ("j1", "next", "x"),
            ("x", "success", "j2"),
            ("b", "next", "j2"),
            ("j2", "next", "o"),
        ],
    );
    let ran = run(&f, &base, &[]).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["o"]["sources"], json!(["b", "x"]));
    assert_eq!(r.outputs["o"]["count"], json!(2));
    assert_eq!(step(r, "j1")[0].branch.as_deref(), Some("1"));
    assert_eq!(step(r, "j2")[0].branch.as_deref(), Some("1+2"));
    assert!(event_index(&ran.events, false, "o") < event_index(&ran.events, false, "c"));
}

#[tokio::test(flavor = "multi_thread")]
async fn variable_types_survive_blocks_and_branches() {
    let (base, _) = server().await;
    let mut c = Collection::new("API");
    c.variables.push(Variable { ty: VarType::Number, ..Variable::new("limit", "3") });
    let cid = c.id.clone();
    let in_collection = |id: &str, script: &str| {
        let scripts = Scripts { pre_request: script.into(), ..Default::default() };
        Block::with_id(
            id,
            id,
            BlockConfig::Request(RequestConfig {
                source: RequestSource::Inline {
                    request: Box::new(HttpRequest::new("GET", "{{baseUrl}}/hit")),
                    collection_id: cid.clone(),
                    scripts,
                },
                ..Default::default()
            }),
        )
    };
    let env_var = |id: &str, name: &str, value: &str| {
        Block::with_id(
            id,
            id,
            BlockConfig::SetVariable(SetVariableConfig {
                name: name.into(),
                value: value.into(),
                scope: VariableScope::Environment,
            }),
        )
    };
    let f = flow(
        vec![
            script(
                "set",
                "pm.environment.set('n', 5); pm.globals.set('ok', true); pm.variables.set('m', 7); return 2;",
            ),
            script(
                "check",
                "return [typeof pm.environment.get('n'), typeof pm.globals.get('ok'), typeof pm.variables.get('m')];",
            ),
            env_var("k", "k", "{{blocks.set.output}}"),
            in_collection(
                "req1",
                "pm.test('stored number', () => pm.expect(typeof pm.collectionVariables.get('limit')).to.eql('number'));\n\
                 pm.collectionVariables.set('flag', true);",
            ),
            in_collection(
                "req2",
                "pm.test('set boolean', () => pm.expect(pm.collectionVariables.get('flag')).to.eql(true));",
            ),
            parallel("fan"),
            script("b1", "pm.environment.set('p', 1.5); return typeof pm.environment.get('n');").at(0.0, 0.0),
            script("b2", "pm.globals.set('q', {a: 1}); return typeof pm.environment.get('k');").at(0.0, 100.0),
            join("join"),
            script(
                "after",
                "return [typeof pm.environment.get('p'), typeof pm.globals.get('q'), typeof pm.environment.get('n')];",
            ),
        ],
        &[
            ("start", "next", "set"),
            ("set", "success", "check"),
            ("check", "success", "k"),
            ("k", "next", "req1"),
            ("req1", "success", "req2"),
            ("req2", "success", "fan"),
            ("fan", "branches", "b1"),
            ("fan", "branches", "b2"),
            ("b1", "success", "join"),
            ("b2", "success", "join"),
            ("join", "next", "after"),
        ],
    );
    let env = FlowEnv { services: services(), collections: vec![c], base_dir: None };
    let ran = run_with(&f, &env, &base, FlowRunOptions::default(), &CancellationToken::new()).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(*r.blocks["check"].output, json!(["number", "boolean", "number"]));
    assert_eq!(r.variables["m"], json!(7));
    assert_eq!(r.tests_passed, 2);
    assert_eq!(*r.blocks["b1"].output, json!("number"));
    assert_eq!(*r.blocks["b2"].output, json!("number"));
    assert_eq!(*r.blocks["after"].output, json!(["number", "object", "number"]));
    assert_eq!(ran.vars.environment["p"], "1.5");
    assert!(ran.vars.types.environment.contains_key("p") && ran.vars.types.globals.contains_key("q"));
}

#[test]
fn max_concurrent_setting() {
    let mut f = flow(
        vec![parallel("fan"), template("a", "a"), template("b", "b")],
        &[("start", "next", "fan"), ("fan", "branches", "a"), ("fan", "branches", "b")],
    );
    assert!(validate(&f, None).is_empty(), "{:#?}", validate(&f, None));
    assert!(!storage::to_json(&f).contains("max_concurrent"));
    f.settings.max_concurrent = 1;
    let diags = validate(&f, None);
    assert_eq!(
        diags.iter().map(|d| (d.code, d.block.as_deref(), d.field.as_deref())).collect::<Vec<_>>(),
        [("sequential_branches", None, Some("max_concurrent"))]
    );
    assert!(!has_errors(&diags));
    f.settings.max_concurrent = 2;
    assert!(validate(&f, None).is_empty());
    let text = storage::to_json(&f);
    assert!(text.contains("\"max_concurrent\": 2"), "{text}");
    assert_eq!(storage::from_json(&text).unwrap(), f);
    // Older files have no such field: no limit.
    let old = r#"{"name": "x", "settings": {"continue_on_failure": true}, "blocks": []}"#;
    assert_eq!(storage::from_json(old).unwrap().settings.max_concurrent, 0);
}

#[test]
fn parallel_validation() {
    let f = flow(
        vec![
            parallel("one-branch"),
            template("a", "a"),
            join("j1"),
            join("lonely"),
            parallel("empty"),
            parallel("fan"),
            template("x", "x"),
            parallel("nested"),
            template("u", "u"),
            template("v", "v"),
            join("j-inner"),
            join("j-outer"),
            join("after"),
            template("t0", "t0"),
            join("outside"),
        ],
        &[
            ("start", "next", "one-branch"),
            ("one-branch", "branches", "a"),
            ("a", "success", "j1"),
            ("j1", "next", "empty"),
            ("start", "next", "fan"),
            ("fan", "branches", "x"),
            ("fan", "branches", "nested"),
            ("nested", "branches", "u"),
            ("nested", "branches", "v"),
            ("u", "success", "j-inner"),
            ("v", "success", "j-inner"),
            ("j-inner", "next", "j-outer"),
            ("x", "success", "j-outer"),
            ("j-outer", "next", "after"),
            ("start", "next", "t0"),
            ("t0", "success", "outside"),
        ],
    );
    let diags = validate(&f, None);
    assert!(!has_errors(&diags), "{diags:#?}");
    assert_eq!(codes(&diags, "one-branch"), ["few_branches"]);
    assert_eq!(codes(&diags, "empty"), ["few_branches"]);
    assert!(codes(&diags, "j1").is_empty(), "{:?}", codes(&diags, "j1"));
    assert_eq!(codes(&diags, "lonely"), ["join_no_inputs"]);
    for id in ["fan", "nested", "j-inner", "j-outer"] {
        assert!(codes(&diags, id).is_empty(), "{id}: {:?}", codes(&diags, id));
    }
    assert_eq!(codes(&diags, "after"), ["join_outside_parallel"]);
    assert_eq!(codes(&diags, "outside"), ["join_outside_parallel"]);
    let d = diags.iter().find(|d| d.block.as_deref() == Some("outside")).unwrap();
    assert_eq!(d.edge, Some(15));
}

#[test]
fn parallel_flow_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = flow(
        vec![
            parallel("fan").at(200.0, 0.0),
            ai("ask", "{{baseUrl}}/v1", true).at(400.0, 0.0),
            delay("wait", "10").at(400.0, 150.0),
            join("join").at(600.0, 0.0),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "ask"),
            ("fan", "branches", "wait"),
            ("ask", "success", "join"),
            ("wait", "next", "join"),
        ],
    );
    f.name = "Parallel".into();
    let path = storage::save(dir.path(), &f).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    for needle in ["\"kind\": \"parallel\"", "\"kind\": \"join\"", "\"from_port\": \"branches\"", "\"stream\": true"] {
        assert!(text.contains(needle), "{needle} in {text}");
    }
    let loaded = storage::load(&path).unwrap();
    assert_eq!(loaded, f);
    assert_eq!(storage::to_json(&loaded), text);
    // New AI blocks stream; AI blocks of older files (no field) do not, and
    // saving them again leaves the file unchanged.
    assert!(matches!(BlockKind::Ai.default_config(), BlockConfig::Ai(c) if c.stream));
    let old = r#"{"name": "x", "blocks": [{"id": "a", "kind": "ai", "label": "", "position": [0.0, 0.0], "config": {"model": "m", "prompt": "p"}}]}"#;
    let f = storage::from_json(old).unwrap();
    assert!(matches!(&f.blocks[0].config, BlockConfig::Ai(c) if !c.stream));
    assert!(!storage::to_json(&f).contains("stream"));
    let parallel =
        r#"{"name": "x", "blocks": [{"id": "p", "kind": "parallel"}, {"id": "j", "kind": "join", "config": null}]}"#;
    let f = storage::from_json(parallel).unwrap();
    assert_eq!(f.blocks[0].kind().output_ports(), [PORT_BRANCHES]);
    assert_eq!(f.blocks[1].kind().output_ports(), [PORT_NEXT]);
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_block_streams_its_answer() {
    let (base, _) = server().await;
    let f = flow(
        vec![ai("ask", "{{baseUrl}}/sse/v1", true), output("text", "text", "{{blocks.ask.output.text}}")],
        &[("start", "next", "ask"), ("ask", "success", "text")],
    );
    let env = FlowEnv { services: services(), collections: Vec::new(), base_dir: None };
    let mut vars = scopes(&base);
    let mut events: Vec<(Instant, FlowEvent)> = Vec::new();
    let mut sink = |e: FlowEvent| events.push((Instant::now(), e));
    let r = run_flow(&env, &f, &mut vars, FlowRunOptions::default(), &mut sink, &CancellationToken::new()).await;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["text"], json!("Hello streaming world"));
    let ask_step = step(&r, "ask")[0].step;
    let deltas: Vec<(Instant, String)> = events
        .iter()
        .filter_map(|(t, e)| match e {
            FlowEvent::AiDelta { step, block, text } if *step == ask_step && block == "ask" => Some((*t, text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["Hello", " streaming", " world"]);
    // The first piece arrived well before the block finished.
    let finished = events
        .iter()
        .find_map(|(t, e)| matches!(e, FlowEvent::BlockFinished { block, .. } if block == "ask").then_some(*t))
        .unwrap();
    assert!(finished.duration_since(deltas[0].0) >= Duration::from_millis(250), "pieces were not live");
    let es: Vec<FlowEvent> = events.into_iter().map(|(_, e)| e).collect();
    let first_delta = es.iter().position(|e| matches!(e, FlowEvent::AiDelta { .. })).unwrap();
    assert!(event_index(&es, true, "ask") < first_delta);

    // Without `stream` no pieces are reported.
    let f = flow(
        vec![ai("ask", "{{baseUrl}}/v1", false), output("text", "text", "{{blocks.ask.output.text}}")],
        &[("start", "next", "ask"), ("ask", "success", "text")],
    );
    let ran = run(&f, &base, &[]).await;
    assert!(ran.result.success, "{:#?}", ran.result);
    assert!(!ran.events.iter().any(|e| matches!(e, FlowEvent::AiDelta { .. })));

    // Two streaming AI blocks in parallel overlap (each takes ~300 ms) and
    // their pieces interleave. Same services as the first run: the HTTP
    // client is built already.
    let f = flow(
        vec![
            parallel("fan"),
            ai("left", "{{baseUrl}}/sse/v1", true).at(0.0, 0.0),
            ai("right", "{{baseUrl}}/sse/v1", true).at(0.0, 100.0),
            join("join"),
            output("both", "both", "{{blocks.join.output.items}}"),
        ],
        &[
            ("start", "next", "fan"),
            ("fan", "branches", "left"),
            ("fan", "branches", "right"),
            ("left", "success", "join"),
            ("right", "success", "join"),
            ("join", "next", "both"),
        ],
    );
    let ran = run_with(&f, &env, &base, FlowRunOptions::default(), &CancellationToken::new()).await;
    let r = &ran.result;
    assert!(r.success, "{r:#?}");
    assert!(r.duration_ms < 580.0, "took {} ms", r.duration_ms);
    let (left, right) = (step(r, "left")[0], step(r, "right")[0]);
    assert!(right.started_ms < left.started_ms + left.duration_ms, "{left:?} {right:?}");
    assert_eq!(r.outputs["both"].as_array().map(|a| a.len()), Some(2));
    let blocks: Vec<&str> = ran
        .events
        .iter()
        .filter_map(|e| match e {
            FlowEvent::AiDelta { block, .. } => Some(block.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(blocks.len(), 6);
    let last_left = blocks.iter().rposition(|b| *b == "left").unwrap();
    let first_right = blocks.iter().position(|b| *b == "right").unwrap();
    assert!(first_right < last_left, "pieces did not interleave: {blocks:?}");
    assert_steps_pair(&ran.events);
}

/// The app spawns runs on its runtime, so the future must be `Send`.
#[allow(dead_code)]
fn run_flow_future_is_send(
    env: &FlowEnv,
    flow: &Flow,
    vars: &mut VariableScopes,
    sink: &mut (dyn FnMut(FlowEvent) + Send),
    cancel: &CancellationToken,
) {
    fn is_send<T: Send>(_: &T) {}
    let fut = run_flow(env, flow, vars, FlowRunOptions::default(), sink, cancel);
    is_send(&fut);
}

#[tokio::test(flavor = "multi_thread")]
async fn huge_delay_fails_instead_of_panicking() {
    let (base, _) = server().await;
    for ms in ["1e30", "{{inputs.ms}}"] {
        let f =
            flow(vec![delay("wait", ms), output("o", "o", "x")], &[("start", "next", "wait"), ("wait", "next", "o")]);
        let r = run(&f, &base, &[("ms", json!(1e300))]).await.result;
        assert!(!r.success, "{r:#?}");
        assert_eq!(step(&r, "wait")[0].port, "failure");
        assert!(r.failures[0].message.contains("too long"), "{:?}", r.failures);
    }
}

#[test]
fn saving_a_flow_file_without_an_id_keeps_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("flows")).unwrap();
    let path = root.join("flows/hand-written.flow.json");
    std::fs::write(&path, r#"{"name": "Hand written", "blocks": [{"id": "start", "kind": "start"}]}"#).unwrap();
    let loaded = storage::find(root, "hand-written").unwrap();
    assert_eq!(loaded.path, path);
    // Loading again gives the same flow (same id).
    assert_eq!(storage::load(&path).unwrap().id, loaded.flow.id);
    // Saving it writes that file, not a copy next to it.
    let mut flow = loaded.flow.clone();
    flow.description = "edited".into();
    assert_eq!(storage::save(root, &flow).unwrap(), path);
    let (list, errors) = storage::list(root);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(list.len(), 1, "{:?}", list.iter().map(|f| &f.path).collect::<Vec<_>>());
    assert_eq!(list[0].flow.description, "edited");
    assert!(storage::delete(root, &flow.id).unwrap());
    assert!(!path.exists());
}

#[test]
fn validation_rejects_numbers_the_run_rejects() {
    for (ms, bad) in [("250", false), ("0", false), ("-1", true), ("inf", true), ("NaN", true), ("1e30", true)] {
        let f = flow(vec![delay("wait", ms)], &[("start", "next", "wait")]);
        assert_eq!(codes(&validate(&f, None), "wait").contains(&"bad_delay"), bad, "delay {ms}");
    }
    for (count, bad) in [("3", false), ("2.5", false), ("x", true), ("-1", true), ("inf", true), ("NaN", true)] {
        let f = flow(
            vec![count_loop("loop", count), log("body", "x")],
            &[("start", "next", "loop"), ("loop", "item", "body")],
        );
        assert_eq!(codes(&validate(&f, None), "loop").contains(&"bad_count"), bad, "count {count}");
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn cancel_interrupts_connecting_to_mcp_servers() {
    let (base, _) = server().await;
    // An MCP server that never answers `initialize` (the default timeout is 60 s).
    let mcp = merpati_core::protocols::McpRequest {
        transport: merpati_core::protocols::McpTransport::Stdio {
            command: "sh".into(),
            args: vec!["-c".into(), "cat > /dev/null".into()],
            env: Vec::new(),
            cwd: String::new(),
        },
        ..Default::default()
    };
    let ai_req = AiRequest {
        provider: AiProvider::OpenAiCompatible,
        base_url: format!("{base}/v1"),
        model: "test-model".into(),
        mcp_servers: vec!["mcp-hang".into()],
        ..Default::default()
    };
    let mut c = Collection::new("AI");
    for (id, request) in [("mcp-hang", RequestKind::Mcp(mcp)), ("saved-ai", RequestKind::Ai(ai_req))] {
        c.items.push(Item::Request(RequestItem {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            request,
            scripts: Scripts::default(),
            examples: Vec::new(),
        }));
    }
    let block = Block::with_id(
        "ask",
        "Ask",
        BlockConfig::Ai(AiBlockConfig {
            request: Some(ItemRef::new("", "saved-ai")),
            prompt: "hi".into(),
            ..Default::default()
        }),
    );
    let f = flow(vec![block, output("o", "o", "x")], &[("start", "next", "ask"), ("ask", "success", "o")]);
    let env = FlowEnv { services: services(), collections: vec![c], base_dir: None };
    // Building the first HTTP client (system certificates) can take seconds.
    env.services.http.default_client().unwrap();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c.cancel();
    });
    let t = Instant::now();
    let ran = run_with(&f, &env, &base, FlowRunOptions::default(), &cancel).await;
    assert!(t.elapsed() < Duration::from_secs(10), "took {:?}", t.elapsed());
    let r = &ran.result;
    assert!(r.cancelled && !r.success, "{r:#?}");
    assert!(r.failures.is_empty(), "{:?}", r.failures);
    assert_eq!(step(r, "ask")[0].error.as_deref(), Some("cancelled"));
    assert!(!r.outputs.contains_key("o"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_block_mcp_servers_inherit_the_collection_auth() {
    let (base, mock) = server().await;
    let mcp = merpati_core::protocols::McpRequest {
        transport: merpati_core::protocols::McpTransport::Http { url: format!("{base}/mcp"), headers: Vec::new() },
        ..Default::default()
    };
    assert!(mcp.auth.is_inherit());
    let ai_req = AiRequest {
        provider: AiProvider::OpenAiCompatible,
        base_url: format!("{base}/v1"),
        model: "test-model".into(),
        mcp_servers: vec!["mcp".into()],
        ..Default::default()
    };
    let mut c = Collection::new("AI");
    c.auth = Auth::bearer("{{token}}");
    c.variables.push(Variable::new("token", "collection-secret"));
    for (id, request) in [("mcp", RequestKind::Mcp(mcp)), ("saved-ai", RequestKind::Ai(ai_req))] {
        c.items.push(Item::Request(RequestItem {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            request,
            scripts: Scripts::default(),
            examples: Vec::new(),
        }));
    }
    let block = Block::with_id(
        "ask",
        "Ask",
        BlockConfig::Ai(AiBlockConfig {
            request: Some(ItemRef::new("", "saved-ai")),
            prompt: "hi".into(),
            ..Default::default()
        }),
    );
    let f = flow(
        vec![block, output("o", "o", "{{blocks.ask.output.text}}")],
        &[("start", "next", "ask"), ("ask", "success", "o")],
    );
    let env = FlowEnv { services: services(), collections: vec![c], base_dir: None };
    let r = run_with(&f, &env, &base, FlowRunOptions::default(), &CancellationToken::new()).await.result;
    assert!(r.success, "{r:#?}");
    assert_eq!(r.outputs["o"], json!("SUMMARY[hi]"));
    let seen = mock.mcp_auth.lock().unwrap().clone();
    assert!(!seen.is_empty() && seen.iter().all(|a| a == "Bearer collection-secret"), "{seen:?}");
}
