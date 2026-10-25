//! Non-HTTP requests in the pipeline: an AI request against an
//! OpenAI-compatible mock, and WebSocket, Socket.IO and MQTT sessions
//! against in-process servers (see `common`), with pre-request,
//! on-message and post-response scripts and the stop conditions.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::{Value, json};

use merpati_core::auth::Auth;
use merpati_core::model::{Collection, RequestItem, RequestKind, Scripts, WsMessageKind, WsRequest, WsSavedMessage};
use merpati_core::protocols::{
    AiMessage, AiProvider, AiRequest, McpRequest, McpTransport, MqttMessage, MqttRequest, MqttSubscription,
    SessionRunner, SocketIoMessage, SocketIoRequest, SocketIoTransport,
};
use merpati_core::settings::RequestDefaults;
use merpati_core::tokens::MemoryTokenStore;
use merpati_core::variables::{VariableScopes, VaultLookup};
use merpati_http::{CancellationToken, CookieJar, EngineConfig, HttpEngine, HttpErrorKind};
use merpati_runner::report::{CliReporter, JunitReporter, Reporter};
use merpati_runner::runner::RunEvent;
use merpati_runner::{ExecuteInput, Execution, RunOptions, Services, execute, run_collection};

async fn mock_ai() -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|Json(body): Json<Value>| async move {
            let model = body["model"].as_str().unwrap_or("").to_string();
            let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null);
            Json(json!({
                "id": "c1",
                "object": "chat.completion",
                "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": format!("pong: {}", last["content"].as_str().unwrap_or(""))}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/v1")
}

struct TestVault;

impl VaultLookup for TestVault {
    fn get(&self, key: &str) -> Option<String> {
        (key == "secret").then(|| "s3cr3t-value".to_string())
    }
}

fn services() -> Services {
    merpati_http::tls::install_provider();
    Services {
        http: Arc::new(HttpEngine::new(EngineConfig::default(), Arc::new(CookieJar::new()))),
        scripts: Arc::new(merpati_script::BoaEngine::new()),
        tokens: Arc::new(MemoryTokenStore::default()),
        vault: Some(Arc::new(TestVault)),
        defaults: RequestDefaults::default(),
        script_timeout: Duration::from_secs(20),
        runtime: tokio::runtime::Handle::current(),
        packages: None,
    }
}

fn item(name: &str, request: RequestKind, scripts: Scripts) -> RequestItem {
    let mut item = RequestItem::new(name, merpati_core::model::HttpRequest::new("GET", ""));
    item.request = request;
    item.scripts = scripts;
    item
}

/// Executes the only request of a one-request collection.
async fn run_one(c: &Collection, vars: &mut VariableScopes, cancel: &CancellationToken) -> Execution {
    run_one_with(&services(), c, vars, cancel).await
}

async fn run_one_with(
    services: &Services,
    c: &Collection,
    vars: &mut VariableScopes,
    cancel: &CancellationToken,
) -> Execution {
    let id = match &c.items[0] {
        merpati_core::model::Item::Request(r) => r.id.clone(),
        _ => unreachable!(),
    };
    let input = ExecuteInput {
        collection: c,
        request_id: &id,
        request_override: None,
        iteration: 0,
        iteration_count: 1,
        base_dir: None,
    };
    execute(services, input, vars, cancel).await
}

fn collection(item: RequestItem) -> Collection {
    let mut c = Collection::new("Sessions");
    c.items.push(item.into());
    c
}

fn ws_message(body: &str) -> WsSavedMessage {
    WsSavedMessage { name: body.into(), body: body.into(), ..Default::default() }
}

fn runner(wait_ms: u64, max_messages: Option<u32>, until: Option<&str>, timeout_ms: u64) -> SessionRunner {
    SessionRunner { wait_ms, max_messages, until_event: until.map(str::to_string), timeout_ms, ack_reply: None }
}

fn assert_tests(exec: &Execution, expected: usize) {
    assert!(exec.script_errors.is_empty(), "script errors: {:?}", exec.script_errors);
    assert_eq!(exec.tests.len(), expected, "{:#?}", exec.tests);
    assert!(exec.tests.iter().all(|t| t.passed), "{:#?}", exec.tests);
}

fn console(exec: &Execution) -> String {
    exec.console.iter().map(|l| l.message.as_str()).collect::<Vec<_>>().join("\n")
}

fn header<'a>(exec: &'a Execution, name: &str) -> Option<&'a str> {
    exec.response.as_ref()?.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_request_runs_with_scripts() {
    let base = mock_ai().await;
    let ai = AiRequest {
        provider: AiProvider::OpenAiCompatible,
        base_url: base,
        model: "mock-1".into(),
        stream: false,
        messages: vec![AiMessage { role: "user".into(), content: "ping".into() }],
        ..Default::default()
    };
    let item = item(
        "Ask",
        RequestKind::Ai(ai),
        Scripts {
            pre_request: "pm.request.model = 'mock-2'; pm.environment.set('asked', 'yes');".into(),
            post_response: "pm.test('answered', () => pm.expect(pm.response.text()).to.equal('pong: ping'));\n\
                            pm.test('model from pre-request', () => pm.expect(pm.response.model).to.equal('mock-2'));"
                .into(),
            ..Default::default()
        },
    );
    let c = collection(item);
    let mut vars = VariableScopes::default();
    let exec = run_one(&c, &mut vars, &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}", exec.error);
    assert_tests(&exec, 2);
    assert_eq!(vars.environment.get("asked").map(String::as_str), Some("yes"));
    assert_eq!(exec.response.as_ref().map(|r| r.status), Some(200));
}

#[test]
fn runner_settings_are_optional_in_request_json() {
    // Requests saved before the setting existed load with the defaults...
    let ws: WsRequest = serde_json::from_value(json!({"url": "ws://x"})).unwrap();
    assert_eq!(ws.runner, SessionRunner::default());
    assert_eq!((ws.runner.wait_ms, ws.runner.max_messages, ws.runner.timeout_ms), (1000, None, 30_000));
    // ...and the defaults are not written.
    assert!(serde_json::to_value(&ws).unwrap().get("runner").is_none());
    assert!(serde_json::to_value(MqttRequest::default()).unwrap().get("runner").is_none());
    assert!(serde_json::to_value(SocketIoRequest::default()).unwrap().get("runner").is_none());
    let sio: SocketIoRequest = serde_json::from_value(json!({
        "url": "http://x",
        "runner": {"max_messages": 3, "until_event": null}
    }))
    .unwrap();
    assert_eq!(sio.runner, runner(1000, Some(3), None, 30_000));
    let v = serde_json::to_value(&sio).unwrap();
    assert_eq!(v["runner"], json!({"wait_ms": 1000, "max_messages": 3, "timeout_ms": 30000}));
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_session_sends_receives_and_runs_scripts() {
    let server = common::websocket().await;
    let ws = WsRequest {
        url: format!("{}/wrong?x=1", server.url),
        messages: vec![ws_message("hello"), ws_message("{{name}}"), ws_message("{{vault:secret}}")],
        runner: runner(300, None, None, 10_000),
        ..Default::default()
    };
    let scripts = Scripts {
        // The pre-request script fixes the path and changes the first message.
        pre_request: "pm.request.url = pm.request.url.replace('/wrong', '/echo');\n\
                      pm.request.messages[0].data = 'changed';"
            .into(),
        on_message:
            "pm.test('echo ' + pm.info.protocol, () => pm.expect(pm.message.direction).to.equal('incoming'));\n\
                     pm.collectionVariables.set('seen', String(Number(pm.collectionVariables.get('seen') || 0) + 1));"
                .into(),
        post_response: "pm.test('summary', () => {\n\
                          pm.expect(pm.response.messages.count()).to.equal(3);\n\
                          pm.expect(pm.response.messages.idx(0).data).to.equal('changed');\n\
                          pm.expect(pm.response.messages.idx(1).text()).to.equal('world');\n\
                          pm.expect(pm.response.sentCount).to.equal(3);\n\
                          pm.expect(pm.response.sent[2].data).to.equal('{{vault:secret}}');\n\
                          pm.expect(pm.response.stopReason).to.equal('silence');\n\
                          pm.expect(pm.response.handshakeStatus).to.equal(101);\n\
                          pm.expect(pm.response.firstMessageTime).to.be.a('number');\n\
                          pm.response.to.be.ok;\n\
                        });\n\
                        pm.test('request has saved messages', () => pm.expect(pm.request.messages.length).to.equal(3));"
            .into(),
    };
    let mut c = collection(item("Echo", RequestKind::WebSocket(ws), scripts));
    // Inherited auth, resolved at send time.
    c.auth = Auth::bearer("{{token}}");
    let mut vars = VariableScopes::default();
    vars.environment.insert("name".into(), "world".into());
    vars.environment.insert("token".into(), "tok-123".into());
    let started = Instant::now();
    let exec = run_one(&c, &mut vars, &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}\n{}", exec.error, console(&exec));
    assert_tests(&exec, 5);
    assert_eq!(vars.collection.get("seen").map(String::as_str), Some("3"));
    let seen = server.seen();
    assert_eq!(seen.uris, vec!["/echo?x=1".to_string()]);
    assert_eq!(seen.authorization, vec![Some("Bearer tok-123".to_string())]);
    assert_eq!(seen.received, vec!["changed", "world", "s3cr3t-value"]);
    let r = exec.response.as_ref().unwrap();
    assert_eq!(r.status, 101);
    assert!(r.status_text.contains("3 sent, 3 received"), "{}", r.status_text);
    assert_eq!(header(&exec, "stop-reason"), Some("silence"));
    // Silence stop: 300 ms after the last message, not the 10 s timeout.
    assert!(started.elapsed() < Duration::from_secs(5));
    // The request in reports keeps its variables unresolved; the outgoing
    // secret is not shown resolved.
    assert_eq!(exec.request.as_ref().map(|r| r.method.as_str()), Some("WS"));
    let log = console(&exec);
    assert!(log.contains("↑ changed") && log.contains("↓ world"), "{log}");
    assert!(log.contains("↑ {{vault:secret}}"), "{log}");
    assert!(!log.contains("tok-123"), "{log}");
    let body = String::from_utf8(r.body.read_range(0, 1 << 20).unwrap()).unwrap();
    let transcript: Vec<Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(transcript.len(), 6);
    assert!(!body.contains("tok-123"));
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_stops_after_max_messages() {
    let server = common::websocket().await;
    let ws = WsRequest {
        url: server.url.clone(),
        messages: vec![ws_message("burst 10")],
        runner: runner(0, Some(3), None, 10_000),
        ..Default::default()
    };
    let scripts = Scripts {
        on_message: "pm.test('tick', () => pm.expect(pm.message.json().type).to.equal('tick'));".into(),
        post_response: "pm.test('three', () => {\n\
                          pm.expect(pm.response.receivedCount).to.equal(3);\n\
                          pm.expect(pm.response.stopReason).to.equal('max_messages');\n\
                        });"
        .into(),
        ..Default::default()
    };
    let c = collection(item("Burst", RequestKind::WebSocket(ws), scripts));
    let exec = run_one(&c, &mut VariableScopes::default(), &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}", exec.error);
    assert_tests(&exec, 4);
    assert_eq!(header(&exec, "messages-received"), Some("3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_stops_at_until_event_and_on_server_close() {
    let server = common::websocket().await;
    // `{"type": "done"}` ends the session; wait_ms 0 means no silence stop.
    let ws = WsRequest {
        url: server.url.clone(),
        messages: vec![ws_message("burst 2")],
        runner: runner(0, None, Some("{{last}}"), 10_000),
        ..Default::default()
    };
    let scripts = Scripts {
        post_response: "pm.test('until', () => {\n\
                          pm.expect(pm.response.stopReason).to.equal('until_event');\n\
                          pm.expect(pm.response.messages.count()).to.equal(3);\n\
                          pm.expect(pm.response.messages.idx(2).data.type).to.equal('done');\n\
                        });"
        .into(),
        ..Default::default()
    };
    let c = collection(item("Until", RequestKind::WebSocket(ws), scripts.clone()));
    let mut vars = VariableScopes::default();
    vars.environment.insert("last".into(), "done".into());
    let exec = run_one(&c, &mut vars, &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}", exec.error);
    assert_tests(&exec, 1);

    // The server closing the connection ends the session too (not an error).
    let ws = WsRequest {
        url: server.url.clone(),
        messages: vec![ws_message("hi"), ws_message("close")],
        runner: runner(0, None, None, 10_000),
        ..Default::default()
    };
    let scripts = Scripts {
        post_response: "pm.test('closed', () => {\n\
                          pm.expect(pm.response.stopReason).to.equal('closed');\n\
                          pm.expect(pm.response.closeCode).to.equal(1000);\n\
                          pm.expect(pm.response.closeReason).to.equal('bye');\n\
                        });"
        .into(),
        ..Default::default()
    };
    let c = collection(item("Close", RequestKind::WebSocket(ws), scripts));
    let exec = run_one(&c, &mut VariableScopes::default(), &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}", exec.error);
    assert_tests(&exec, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_timeout_cancellation_and_connect_errors() {
    let server = common::websocket().await;
    // Overall timeout while the server stays quiet.
    let ws = WsRequest {
        url: server.url.clone(),
        messages: vec![ws_message("quiet")],
        runner: runner(0, None, None, 400),
        ..Default::default()
    };
    let scripts = Scripts {
        post_response: "pm.test('timeout', () => pm.expect(pm.response.stopReason).to.equal('timeout'));".into(),
        ..Default::default()
    };
    let c = collection(item("Quiet", RequestKind::WebSocket(ws.clone()), scripts));
    let started = Instant::now();
    let exec = run_one(&c, &mut VariableScopes::default(), &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}", exec.error);
    assert_tests(&exec, 1);
    assert!(started.elapsed() < Duration::from_secs(4));

    // Cancellation (the app's Stop button, Ctrl-C) ends a session without limits.
    let unlimited = WsRequest { runner: runner(0, None, None, 0), ..ws };
    let scripts = Scripts { post_response: "pm.test('never runs', () => {});".into(), ..Default::default() };
    let c = collection(item("Endless", RequestKind::WebSocket(unlimited), scripts));
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel();
        });
    }
    let started = Instant::now();
    let exec = run_one(&c, &mut VariableScopes::default(), &cancel).await;
    assert_eq!(exec.error.as_ref().map(|e| e.kind), Some(HttpErrorKind::Cancelled));
    assert!(exec.tests.is_empty());
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());

    // Nothing listening: the request fails without post-response scripts.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ws = WsRequest { url: format!("ws://127.0.0.1:{port}"), ..Default::default() };
    let scripts = Scripts { post_response: "pm.test('never runs', () => {});".into(), ..Default::default() };
    let c = collection(item("Refused", RequestKind::WebSocket(ws), scripts));
    let exec = run_one(&c, &mut VariableScopes::default(), &CancellationToken::new()).await;
    let err = exec.error.as_ref().expect("connect error");
    assert!(err.message.contains("WebSocket"), "{}", err.message);
    assert!(exec.tests.is_empty() && exec.response.is_none());

    // pm.execution.skipRequest() in the pre-request script.
    let ws = WsRequest { url: server.url.clone(), ..Default::default() };
    let scripts = Scripts { pre_request: "pm.execution.skipRequest();".into(), ..Default::default() };
    let c = collection(item("Skipped", RequestKind::WebSocket(ws), scripts));
    let exec = run_one(&c, &mut VariableScopes::default(), &CancellationToken::new()).await;
    assert!(exec.skipped && exec.response.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn socketio_session_with_acks_auth_and_until_event() {
    let server = common::socketio().await;
    let sio = SocketIoRequest {
        url: server.url.clone(),
        namespace: "/admin".into(),
        transport: SocketIoTransport::WebSocket,
        auth_payload: r#"{"token": "{{tok}}"}"#.into(),
        messages: vec![
            SocketIoMessage {
                event: "echo".into(),
                args: r#"["hi", {"n": 1}]"#.into(),
                ack: true,
                ..Default::default()
            },
            SocketIoMessage { event: "whoami".into(), ..Default::default() },
            SocketIoMessage { event: "burst".into(), args: "[2]".into(), ..Default::default() },
        ],
        runner: runner(0, None, Some("done"), 10_000),
        ..Default::default()
    };
    let scripts = Scripts {
        pre_request: "pm.request.messages[2].args = [3];".into(),
        on_message: "if (pm.message.event === 'tick') pm.test('tick ' + pm.message.data, () => {});".into(),
        post_response: "pm.test('summary', () => {\n\
                          const r = pm.response;\n\
                          pm.expect(r.stopReason).to.equal('until_event');\n\
                          pm.expect(r.namespace).to.equal('/admin');\n\
                          pm.expect(r.sid).to.equal('sock1');\n\
                          pm.expect(r.sent[0].ackArgs).to.eql(['hi', {n: 1}]);\n\
                          const events = r.messages.map((m) => m.event);\n\
                          pm.expect(events).to.eql(['echo', 'whoami', 'tick', 'tick', 'tick', 'done']);\n\
                          pm.expect(r.messages[1].data).to.eql({token: 'abc'});\n\
                        });"
        .into(),
    };
    let c = collection(item("Socket", RequestKind::SocketIo(sio), scripts));
    let mut vars = VariableScopes::default();
    vars.environment.insert("tok".into(), "abc".into());
    let exec = run_one(&c, &mut vars, &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}\n{}", exec.error, console(&exec));
    assert_tests(&exec, 4);
    let seen = server.seen();
    assert_eq!(seen.auth, vec![json!({"token": "abc"})]);
    assert_eq!(seen.received, vec![r#"echo ["hi",{"n":1}]"#, "whoami []", "burst [3]"]);
    let r = exec.response.as_ref().unwrap();
    assert_eq!(r.status, 200);
    assert!(r.status_text.contains("3 sent, 6 received"), "{}", r.status_text);
    assert_eq!(header(&exec, "namespace"), Some("/admin"));
}

#[tokio::test(flavor = "multi_thread")]
async fn socketio_session_answers_server_acks() {
    let server = common::socketio().await;
    let sio = SocketIoRequest {
        url: server.url.clone(),
        transport: SocketIoTransport::WebSocket,
        messages: vec![SocketIoMessage { event: "ask".into(), ..Default::default() }],
        runner: SessionRunner {
            ack_reply: Some(r#"["yes", {{n}}]"#.into()),
            ..runner(0, None, Some("answered"), 10_000)
        },
        ..Default::default()
    };
    let scripts = Scripts {
        post_response: "pm.test('answered', () => {\n\
                          const last = pm.response.messages[pm.response.messages.length - 1];\n\
                          pm.expect(last.event).to.equal('answered');\n\
                          pm.expect(last.data).to.eql(['yes', 5]);\n\
                        });"
        .into(),
        ..Default::default()
    };
    let c = collection(item("Ask", RequestKind::SocketIo(sio.clone()), scripts));
    let mut vars = VariableScopes::default();
    vars.environment.insert("n".into(), "5".into());
    let exec = run_one(&c, &mut vars, &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}\n{}", exec.error, console(&exec));
    assert_tests(&exec, 1);
    assert_eq!(server.seen().received, vec!["ask []", r#"ack 77 ["yes",5]"#]);

    // Without `ack_reply` the question stays unanswered.
    let server = common::socketio().await;
    let quiet = SocketIoRequest { url: server.url.clone(), runner: runner(200, None, None, 10_000), ..sio };
    let c = collection(item("Ask", RequestKind::SocketIo(quiet), Scripts::default()));
    let exec = run_one(&c, &mut VariableScopes::default(), &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}", exec.error);
    assert_eq!(server.seen().received, vec!["ask []"]);
}

/// The example of docs/scripting.md ("Sessions in the Collection Runner").
#[tokio::test(flavor = "multi_thread")]
async fn socketio_session_documented_example() {
    let server = common::socketio().await;
    let sio =
        SocketIoRequest { url: server.url.clone(), transport: SocketIoTransport::WebSocket, ..Default::default() };
    let scripts = Scripts {
        pre_request: "pm.request.messages.push({ event: 'burst', args: [3] });\n\
                      pm.request.runner.untilEvent = 'done';"
            .into(),
        post_response: "pm.test('finished normally', () => pm.expect(pm.response.stopReason).to.equal('until_event'));\n\
                        pm.test('three ticks', () => pm.expect(pm.response.messages.filter((m) => m.event === 'tick')).to.have.length(3));"
            .into(),
        ..Default::default()
    };
    let c = collection(item("Doc", RequestKind::SocketIo(sio), scripts));
    let exec = run_one(&c, &mut VariableScopes::default(), &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}\n{}", exec.error, console(&exec));
    assert_tests(&exec, 2);
    assert_eq!(server.seen().received, vec!["burst [3]"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn mqtt_session_subscribes_then_publishes() {
    let broker = common::mqtt(Some(("alice", "pw-1"))).await;
    let mqtt = MqttRequest {
        url: broker.url.clone(),
        client_id: "runner-test".into(),
        auth: Auth::basic("alice", "{{password}}"),
        subscriptions: vec![
            MqttSubscription { topic: "echo/#".into(), qos: 1, enabled: true },
            MqttSubscription { topic: "tick/+".into(), qos: 0, enabled: true },
            MqttSubscription { topic: "status/#".into(), qos: 0, enabled: true },
            MqttSubscription { topic: "off/#".into(), qos: 0, enabled: false },
        ],
        messages: vec![
            MqttMessage { topic: "echo/a".into(), payload: r#"{"n": 1}"#.into(), qos: 1, ..Default::default() },
            MqttMessage { topic: "cmd/burst".into(), payload: "2".into(), ..Default::default() },
        ],
        runner: runner(0, None, Some("status/+"), 10_000),
        ..Default::default()
    };
    let scripts = Scripts {
        pre_request: "pm.request.messages[1].payload = '3';".into(),
        on_message: "if (pm.message.topic.startsWith('tick/')) \
                     pm.test(pm.message.topic, () => pm.expect(pm.message.data.i).to.be.a('number'));"
            .into(),
        post_response: "pm.test('summary', () => {\n\
                          const r = pm.response;\n\
                          pm.expect(r.stopReason).to.equal('until_event');\n\
                          pm.expect(r.clientId).to.equal('runner-test');\n\
                          pm.expect(r.subscriptions.map((s) => s.grantedQos)).to.eql([1, 0, 0]);\n\
                          pm.expect(r.messages.idx(0).topic).to.equal('echo/a');\n\
                          pm.expect(r.messages.idx(0).json().n).to.equal(1);\n\
                          pm.expect(r.receivedCount).to.equal(5);\n\
                          pm.expect(r.sent[1].payload).to.equal('3');\n\
                        });"
        .into(),
    };
    let c = collection(item("Broker", RequestKind::Mqtt(mqtt), scripts));
    let mut vars = VariableScopes::default();
    vars.environment.insert("password".into(), "pw-1".into());
    let exec = run_one(&c, &mut vars, &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}\n{}", exec.error, console(&exec));
    assert_tests(&exec, 4);
    let seen = broker.seen();
    assert_eq!(seen.auth, vec![json!("alice:pw-1")]);
    assert_eq!(seen.received, vec![r#"echo/a {"n": 1}"#, "cmd/burst 3"]);
    assert!(!console(&exec).contains("pw-1"));

    // Wrong credentials: the connection fails.
    let mqtt = MqttRequest { url: broker.url.clone(), auth: Auth::basic("alice", "nope"), ..Default::default() };
    let c = collection(item("Refused", RequestKind::Mqtt(mqtt), Scripts::default()));
    let exec = run_one(&c, &mut VariableScopes::default(), &CancellationToken::new()).await;
    assert!(exec.error.as_ref().is_some_and(|e| e.message.starts_with("MQTT")), "{:?}", exec.error);
}

/// Shared buffer for the CLI reporter output.
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn collection_run_with_sessions_writes_junit() {
    let ws_server = common::websocket().await;
    let sio_server = common::socketio().await;
    let broker = common::mqtt(None).await;
    let test = |name: &str| format!("pm.test('{name}', () => pm.expect(pm.response.receivedCount).to.be.above(0));");
    let mut c = Collection::new("Realtime");
    c.items.push(
        item(
            "WS echo",
            RequestKind::WebSocket(WsRequest {
                url: ws_server.url.clone(),
                messages: vec![ws_message("one"), WsSavedMessage { kind: WsMessageKind::Binary, ..ws_message("0a0b") }],
                runner: runner(0, Some(2), None, 10_000),
                ..Default::default()
            }),
            Scripts {
                on_message: "pm.test('ws message', () => {});".into(),
                post_response: test("ws received"),
                ..Default::default()
            },
        )
        .into(),
    );
    c.items.push(
        item(
            "SIO echo",
            RequestKind::SocketIo(SocketIoRequest {
                url: sio_server.url.clone(),
                transport: SocketIoTransport::WebSocket,
                messages: vec![SocketIoMessage { event: "echo".into(), args: "[1]".into(), ..Default::default() }],
                runner: runner(0, Some(1), None, 10_000),
                ..Default::default()
            }),
            Scripts { post_response: test("sio received"), ..Default::default() },
        )
        .into(),
    );
    c.items.push(
        item(
            "MQTT echo",
            RequestKind::Mqtt(MqttRequest {
                url: broker.url.clone(),
                subscriptions: vec![MqttSubscription { topic: "e/#".into(), qos: 0, enabled: true }],
                messages: vec![MqttMessage { topic: "e/1".into(), payload: "x".into(), ..Default::default() }],
                runner: runner(0, Some(1), None, 10_000),
                ..Default::default()
            }),
            Scripts {
                post_response: format!(
                    "{}\npm.test('fails on purpose', () => pm.expect(1).to.equal(2));",
                    test("mqtt received")
                ),
                ..Default::default()
            },
        )
        .into(),
    );
    let dir = tempfile::tempdir().unwrap();
    let junit_path = dir.path().join("junit.xml");
    let out = Buf::default();
    let mut reporters: Vec<Box<dyn Reporter>> =
        vec![Box::new(JunitReporter::new(&junit_path, "Realtime")), Box::new(CliReporter::new(out.clone(), false))];
    let mut vars = VariableScopes::default();
    let summary = run_collection(
        &services(),
        &c,
        &mut vars,
        RunOptions::default(),
        &mut |e: RunEvent| {
            for r in reporters.iter_mut() {
                r.on_event(&e);
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    for r in reporters.iter_mut() {
        r.finish(&summary).unwrap();
    }
    assert_eq!((summary.executed, summary.skipped, summary.requests_failed), (3, 0, 0));
    assert_eq!((summary.tests_passed, summary.tests_failed), (5, 1));
    let xml = std::fs::read_to_string(&junit_path).unwrap();
    for name in ["ws message", "ws received", "sio received", "mqtt received", "fails on purpose"] {
        assert!(xml.contains(&format!("name=\"{name}\"")), "{name} missing in {xml}");
    }
    assert!(xml.contains("Realtime.MQTT echo") && xml.contains("AssertionFailure"), "{xml}");
    let cli = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    assert!(cli.contains("WS ws://") && cli.contains("101 Switching Protocols · 2 sent, 2 received"), "{cli}");
    assert!(cli.contains("SIO http://") && cli.contains("200 Connected · 1 sent, 1 received"), "{cli}");
    assert!(cli.contains("MQTT mqtt://"), "{cli}");
    assert_eq!(ws_server.seen().received, vec!["one", "binary 2"]);
}

/// The Socket.IO session against the real socket.io server of
/// `merpati-socketio/tests/node/server.js`:
///
/// ```text
/// npm install socket.io@4   # in some folder A
/// MERPATI_SIO4_MODULES=A/node_modules cargo test -p merpati-runner --test protocols -- --ignored
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs node and socket.io (MERPATI_SIO4_MODULES)"]
async fn socketio_session_against_real_server() {
    use tokio::io::AsyncBufReadExt;
    let Ok(modules) = std::env::var("MERPATI_SIO4_MODULES") else {
        eprintln!("skipped: set MERPATI_SIO4_MODULES to a node_modules folder containing socket.io");
        return;
    };
    let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../merpati-socketio/tests/node/server.js");
    let mut child = tokio::process::Command::new("node")
        .arg(script)
        .env("NODE_PATH", modules)
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("node");
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line()).await.unwrap().unwrap().unwrap();
    let info: Value = serde_json::from_str(&line).unwrap();
    for transport in [SocketIoTransport::Auto, SocketIoTransport::WebSocket, SocketIoTransport::Polling] {
        let sio = SocketIoRequest {
            url: format!("http://127.0.0.1:{}", info["ports"]["default"]),
            transport,
            auth_payload: r#"{"user": "{{user}}"}"#.into(),
            messages: vec![
                SocketIoMessage { event: "echo".into(), args: r#"["a", 1]"#.into(), ack: true, ..Default::default() },
                SocketIoMessage { event: "info".into(), ..Default::default() },
            ],
            runner: runner(300, Some(2), None, 15_000),
            ..Default::default()
        };
        let scripts = Scripts {
            on_message: "pm.test('got ' + pm.message.event, () => {});".into(),
            post_response: "pm.test('summary', () => {\n\
                              pm.expect(pm.response.sent[0].ackArgs).to.eql(['a', 1]);\n\
                              pm.expect(pm.response.messages.idx(1).data.auth).to.eql({user: 'bob'});\n\
                              pm.expect(pm.response.stopReason).to.equal('max_messages');\n\
                            });"
            .into(),
            ..Default::default()
        };
        let c = collection(item("Real", RequestKind::SocketIo(sio), scripts));
        let mut vars = VariableScopes::default();
        vars.environment.insert("user".into(), "bob".into());
        let exec = run_one(&c, &mut vars, &CancellationToken::new()).await;
        assert!(exec.error.is_none(), "{transport:?}: {:?}\n{}", exec.error, console(&exec));
        assert_tests(&exec, 3);
    }
}

/// A minimal HTTP CONNECT proxy; returns its URL and the tunnel targets.
async fn connect_proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let targets = log.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let targets = targets.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut b = [0u8; 1];
                    if client.read(&mut b).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let Some(target) = head.strip_prefix("CONNECT ").and_then(|r| r.split(' ').next()) else { return };
                targets.lock().unwrap().push(target.to_string());
                let Ok(mut upstream) = tokio::net::TcpStream::connect(target).await else { return };
                let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (url, log)
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_use_the_engine_proxy() {
    let (proxy_url, tunnels) = connect_proxy().await;
    let mut services = services();
    let proxy = merpati_core::settings::ProxySettings {
        custom_enabled: true,
        custom_url: proxy_url.clone(),
        ..Default::default()
    };
    let config = EngineConfig { proxy, ..Default::default() };
    services.http = Arc::new(HttpEngine::new(config, Arc::new(CookieJar::new())));
    let ws_server = common::websocket().await;
    let sio_server = common::socketio().await;
    let broker = common::mqtt(None).await;
    let requests = [
        RequestKind::WebSocket(WsRequest {
            url: format!("{}/echo", ws_server.url),
            messages: vec![ws_message("hi")],
            runner: runner(200, None, None, 10_000),
            ..Default::default()
        }),
        RequestKind::SocketIo(SocketIoRequest {
            url: sio_server.url.clone(),
            transport: SocketIoTransport::WebSocket,
            messages: vec![SocketIoMessage { event: "echo".into(), args: "[1]".into(), ..Default::default() }],
            runner: runner(200, None, None, 10_000),
            ..Default::default()
        }),
        RequestKind::Mqtt(MqttRequest {
            url: broker.url.clone(),
            messages: vec![MqttMessage { topic: "a/b".into(), payload: "x".into(), ..Default::default() }],
            runner: runner(200, None, None, 10_000),
            ..Default::default()
        }),
    ];
    for request in requests {
        let c = collection(item("Via proxy", request, Scripts::default()));
        let exec = run_one_with(&services, &c, &mut VariableScopes::default(), &CancellationToken::new()).await;
        assert!(exec.error.is_none(), "{:?}\n{}", exec.error, console(&exec));
        assert!(console(&exec).contains(&format!(" via proxy {proxy_url}")), "{}", console(&exec));
    }
    let host = |url: &str| url.split("://").nth(1).unwrap().to_string();
    assert_eq!(*tunnels.lock().unwrap(), vec![host(&ws_server.url), host(&sio_server.url), host(&broker.url)]);
    assert_eq!(broker.seen().received, vec!["a/b x"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_oauth_token_fetch_can_be_cancelled() {
    // A token endpoint that accepts connections and never answers.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://{}/token", silent.local_addr().unwrap());
    tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((s, _)) = silent.accept().await {
            open.push(s);
        }
    });
    let server = common::websocket().await;
    let ws = WsRequest {
        url: server.url.clone(),
        messages: vec![ws_message("hi")],
        runner: runner(0, Some(1), None, 0),
        auth: Auth::new(merpati_core::auth::AuthType::OAuth2)
            .with("grant_type", "client_credentials")
            .with("accessTokenUrl", token_url)
            .with("clientId", "cid"),
        ..Default::default()
    };
    let c = collection(item("Ws", RequestKind::WebSocket(ws), Scripts::default()));
    let services = services();
    // Building the first HTTP client (system certificates) can take seconds.
    services.http.default_client().unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let started = Instant::now();
    let exec = run_one_with(&services, &c, &mut VariableScopes::default(), &cancel).await;
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert_eq!(exec.error.as_ref().map(|e| e.kind), Some(HttpErrorKind::Cancelled), "{:?}", exec.error);
}

/// A minimal Streamable HTTP MCP server without tools that records the
/// `Authorization` header of every request.
async fn mock_mcp() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let s = seen.clone();
    let app = Router::new().route(
        "/mcp",
        post(move |h: axum::http::HeaderMap, body: String| async move {
            let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            s.lock().unwrap().push(auth);
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            if v.get("id").is_none() {
                return axum::http::StatusCode::ACCEPTED.into_response();
            }
            let result = match v["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock", "version": "1"}
                }),
                Some("tools/list") => json!({"tools": []}),
                _ => json!({}),
            };
            Json(json!({"jsonrpc": "2.0", "id": v["id"], "result": result})).into_response()
        })
        .get(|| async { axum::http::StatusCode::METHOD_NOT_ALLOWED }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/mcp"), seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_requests_and_ai_mcp_servers_inherit_the_collection_auth() {
    let (url, seen) = mock_mcp().await;
    let mcp = McpRequest { transport: McpTransport::Http { url, headers: Vec::new() }, ..Default::default() };
    assert!(mcp.auth.is_inherit());
    let mut c = collection(item("Server", RequestKind::Mcp(mcp), Scripts::default()));
    c.auth = Auth::bearer("{{token}}");
    let mut vars = VariableScopes::default();
    vars.collection.insert("token".into(), "collection-secret".into());
    let exec = run_one(&c, &mut vars, &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}\n{}", exec.error, console(&exec));
    let auths = seen.lock().unwrap().clone();
    assert!(!auths.is_empty() && auths.iter().all(|a| a == "Bearer collection-secret"), "{auths:?}");

    // The same server attached to an AI request.
    seen.lock().unwrap().clear();
    let server_id = c.requests()[0].id.clone();
    let ai = AiRequest {
        provider: AiProvider::OpenAiCompatible,
        base_url: mock_ai().await,
        model: "m".into(),
        messages: vec![AiMessage { role: "user".into(), content: "hi".into() }],
        mcp_servers: vec![server_id],
        ..Default::default()
    };
    let mut ai_c = collection(item("Ask", RequestKind::Ai(ai), Scripts::default()));
    ai_c.auth = c.auth.clone();
    ai_c.items.extend(c.items.clone());
    let exec = run_one(&ai_c, &mut vars, &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}\n{}", exec.error, console(&exec));
    let auths = seen.lock().unwrap().clone();
    assert!(!auths.is_empty() && auths.iter().all(|a| a == "Bearer collection-secret"), "{auths:?}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn cancel_interrupts_connecting_to_mcp_servers() {
    // An MCP server that never answers `initialize` (the default timeout is 60 s).
    let hang = McpRequest {
        transport: McpTransport::Stdio {
            command: "sh".into(),
            args: vec!["-c".into(), "cat > /dev/null".into()],
            env: Vec::new(),
            cwd: String::new(),
        },
        ..Default::default()
    };
    let services = services();
    // Building the first HTTP client (system certificates) can take seconds.
    services.http.default_client().unwrap();
    let mcp_c = collection(item("Hang", RequestKind::Mcp(hang), Scripts::default()));
    let ai = AiRequest {
        provider: AiProvider::OpenAiCompatible,
        base_url: mock_ai().await,
        model: "m".into(),
        mcp_servers: vec![mcp_c.requests()[0].id.clone()],
        ..Default::default()
    };
    let mut ai_c = collection(item("Ask", RequestKind::Ai(ai), Scripts::default()));
    ai_c.items.extend(mcp_c.items.clone());
    for c in [&mcp_c, &ai_c] {
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c2.cancel();
        });
        let started = Instant::now();
        let exec = run_one_with(&services, c, &mut VariableScopes::default(), &cancel).await;
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert_eq!(exec.error.as_ref().map(|e| e.kind), Some(HttpErrorKind::Cancelled), "{:?}", exec.error);
    }
}
