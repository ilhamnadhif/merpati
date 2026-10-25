//! Scripts for non-HTTP protocols (FR-S7): `pm.request`, `pm.response` and
//! `pm.message` for gRPC, WebSocket, Socket.IO, MQTT, MCP and AI jobs.

use std::sync::Arc;

use merpati_core::model::{KeyValue, WsRequest};
use merpati_core::protocols::{
    AiMessage, AiRequest, GrpcRequest, McpCallKind, McpRequest, McpSavedCall, McpTransport, MqttMessage, MqttRequest,
};
use merpati_core::response::ResponseBody;
use merpati_core::script::protocol::{self, Direction};
use merpati_core::script::*;
use merpati_core::variables::VariableScopes;
use merpati_script::BoaEngine;
use serde_json::{Value, json};

fn job(event: ScriptEvent, ctx: ProtocolContext, code: &str) -> ScriptJob {
    let mut j =
        ScriptJob::for_protocol(event, vec![ScriptSource { location: "request \"p\"".into(), code: code.into() }], ctx);
    j.info.request_name = "p".into();
    j
}

fn run(j: ScriptJob) -> ScriptOutcome {
    let out = BoaEngine::new().run(j);
    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    out
}

fn results(out: &ScriptOutcome) -> Vec<(String, bool)> {
    out.tests.iter().map(|t| (t.name.clone(), t.passed)).collect()
}

fn all_passed(out: &ScriptOutcome) {
    let failed: Vec<_> = out.tests.iter().filter(|t| !t.passed).map(|t| (&t.name, &t.error)).collect();
    assert!(failed.is_empty(), "failed tests: {failed:?}");
    assert!(!out.tests.is_empty());
}

fn error_of<'a>(out: &'a ScriptOutcome, name: &str) -> &'a str {
    out.tests.iter().find(|t| t.name == name).and_then(|t| t.error.as_deref()).unwrap_or_default()
}

fn greeter() -> GrpcRequest {
    GrpcRequest {
        url: "grpc://localhost:50051".into(),
        service: "helloworld.Greeter".into(),
        method: "SayHello".into(),
        metadata: vec![
            KeyValue::new("x-a", "1"),
            KeyValue::new("x-gone", "bye"),
            KeyValue::new("x-off", "0").disabled(),
        ],
        message: "{\"name\": \"{{who}}\"}".into(),
        ..Default::default()
    }
}

#[test]
fn grpc_before_invoke_reads_and_changes_the_request() {
    let req = greeter();
    let mut j = job(
        ScriptEvent::PreRequest,
        protocol::grpc(&req),
        r#"
        pm.test('shape', () => {
            pm.expect(pm.info.protocol).to.equal('grpc');
            pm.expect(pm.info.eventName).to.equal('prerequest');
            pm.expect(pm.request.url).to.equal('grpc://localhost:50051');
            pm.expect(pm.request.methodPath).to.equal('helloworld.Greeter/SayHello');
            pm.expect(pm.request.metadata.get('X-A')).to.equal('1');
            pm.expect(pm.request.metadata.has('x-off')).to.be.true;
            pm.expect(pm.request.metadata.toObject(true)).to.eql({'x-a': '1', 'x-gone': 'bye'});
            pm.expect(pm.request.messages).to.have.lengthOf(1);
            pm.expect(pm.request.messages.count()).to.equal(1);
            pm.expect(pm.request.messages.idx(0).data).to.eql({name: '{{who}}'});
        });
        pm.request.metadata.add({key: 'authorization', value: 'Bearer ' + pm.environment.get('token')});
        pm.request.metadata.upsert({key: 'x-a', value: '2'});
        pm.request.metadata.remove('x-gone');
        pm.request.messages[0].data.name = pm.variables.replaceIn('{{who}}') + '!';
        pm.request.url = 'grpcs://api.example.com:443';
        pm.request.methodPath = 'helloworld.Greeter/SayHelloAgain';
        pm.environment.set('sent', 'yes');
        "#,
    );
    j.variables.environment.insert("token".into(), "t0k".into());
    j.variables.environment.insert("who".into(), "Ada".into());
    let out = run(j);
    all_passed(&out);
    assert_eq!(out.variables.environment["sent"], "yes");
    let changed = out.protocol_request.expect("protocol request");
    let mut applied = req.clone();
    protocol::apply_grpc(&mut applied, &changed);
    assert_eq!(applied.url, "grpcs://api.example.com:443");
    assert_eq!(applied.service, "helloworld.Greeter");
    assert_eq!(applied.method, "SayHelloAgain");
    assert_eq!(
        applied.metadata,
        vec![
            KeyValue::new("x-a", "2"),
            KeyValue::new("x-off", "0").disabled(),
            KeyValue::new("authorization", "Bearer t0k")
        ]
    );
    assert_eq!(serde_json::from_str::<Value>(&applied.message).unwrap(), json!({"name": "Ada!"}));
}

#[test]
fn untouched_protocol_request_is_returned_verbatim() {
    let ctx = protocol::grpc(&greeter());
    let input = ctx.request.clone();
    let out = run(job(
        ScriptEvent::PreRequest,
        ctx,
        "pm.request.metadata.get('x-a'); pm.request.messages.idx(0).json(); JSON.stringify(pm.request);",
    ));
    assert_eq!(out.protocol_request, Some(input.clone()));
    let mut applied = greeter();
    protocol::apply_grpc(&mut applied, &input);
    assert_eq!(applied, greeter());
    // The HTTP request of a protocol job is passed through too.
    assert_eq!(out.request, ScriptRequest::default());
}

#[test]
fn grpc_after_response_assertions() {
    let response = protocol::grpc_response(
        0,
        "OK",
        &[("content-type".into(), "application/grpc".into())],
        &[("x-trace".into(), "abc".into())],
        vec![protocol::message(json!({"message": "Hello Ada"}), Direction::Incoming, 1_700_000_000_000)],
        12.5,
    );
    let ctx = protocol::grpc(&greeter()).with_response(response);
    let out = run(job(
        ScriptEvent::PostResponse,
        ctx,
        r#"
        pm.test('status code', () => pm.response.to.have.statusCode(0));
        pm.test('ok', () => pm.response.to.be.ok);
        pm.test('expect form', () => pm.expect(pm.response).to.have.statusCode(0));
        pm.test('metadata', () => {
            pm.response.to.have.metadata('content-type');
            pm.response.to.have.metadata('Content-Type', 'application/grpc');
            pm.response.to.have.metadata('content-type', /grpc/);
            pm.response.to.not.have.metadata('x-missing');
        });
        pm.test('trailer', () => pm.response.to.have.trailer('x-trace', 'abc'));
        pm.test('time', () => pm.response.to.have.responseTime.below(100));
        pm.test('messages', () => {
            pm.expect(pm.response.statusCode).to.equal(0);
            pm.expect(pm.response.messages.count()).to.equal(1);
            pm.expect(pm.response.messages.idx(0).data.message).to.equal('Hello Ada');
            pm.expect(pm.response.messages[0].direction).to.equal('incoming');
            pm.expect(pm.response.json()).to.eql({message: 'Hello Ada'});
            pm.expect(pm.response.metadata.toObject()).to.eql({'content-type': 'application/grpc'});
        });
        pm.test('wrong code', () => pm.response.to.have.statusCode(5));
        pm.test('wrong trailer', () => pm.response.to.have.trailer('grpc-message'));
        pm.test('wrong metadata value', () => pm.response.to.have.metadata('content-type', 'text/plain'));
        "#,
    ));
    assert_eq!(
        results(&out),
        vec![
            ("status code".into(), true),
            ("ok".into(), true),
            ("expect form".into(), true),
            ("metadata".into(), true),
            ("trailer".into(), true),
            ("time".into(), true),
            ("messages".into(), true),
            ("wrong code".into(), false),
            ("wrong trailer".into(), false),
            ("wrong metadata value".into(), false),
        ],
        "{:?}",
        out.tests
    );
    assert!(
        error_of(&out, "wrong code").contains("expected response to have status code 5 but got 0"),
        "{:?}",
        out.tests
    );
    assert!(error_of(&out, "wrong trailer").contains("expected response to have trailer with key 'grpc-message'"));
    assert!(error_of(&out, "wrong metadata value").contains("but got 'application/grpc'"));
}

#[test]
fn grpc_error_status_is_not_ok() {
    let response = protocol::grpc_response(5, "not found", &[], &[], vec![], 3.0);
    let out = run(job(
        ScriptEvent::PostResponse,
        protocol::grpc(&greeter()).with_response(response),
        "pm.test('ok', () => pm.response.to.be.ok); pm.test('code', () => pm.response.to.have.statusCode(5));\
         pm.test('message', () => pm.expect(pm.response.statusMessage).to.equal('not found'));",
    ));
    assert_eq!(results(&out), vec![("ok".into(), false), ("code".into(), true), ("message".into(), true)]);
    assert!(error_of(&out, "ok").contains("status code 5"), "{:?}", out.tests);
}

#[test]
fn grpc_on_message_runs_tests_per_message() {
    let engine = BoaEngine::new();
    for (i, count) in [3, 0].into_iter().enumerate() {
        let msg = protocol::message(json!({"count": count}), Direction::Incoming, 1_700_000_000_000 + i as i64);
        let ctx = protocol::grpc(&greeter()).with_message(msg);
        let mut j = job(
            ScriptEvent::OnMessage,
            ctx,
            r#"
            pm.test('event', () => pm.expect(pm.info.eventName).to.equal('message'));
            pm.test('incoming', () => {
                pm.expect(pm.message.direction).to.equal('incoming');
                pm.expect(pm.message.timestamp).to.be.a('number');
                pm.expect(pm.message.json()).to.have.property('count');
            });
            pm.test('positive count', () => pm.expect(pm.message.data.count).to.be.above(0));
            pm.collectionVariables.set('seen', Number(pm.collectionVariables.get('seen') || 0) + 1);
            pm.test('request still there', () => pm.expect(pm.request.methodPath).to.equal('helloworld.Greeter/SayHello'));
            "#,
        );
        j.variables.collection.insert("seen".into(), i.to_string());
        let out = engine.run(j);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let positive = count > 0;
        assert_eq!(
            results(&out),
            vec![
                ("event".into(), true),
                ("incoming".into(), true),
                ("positive count".into(), positive),
                ("request still there".into(), true)
            ]
        );
        assert_eq!(out.variables.collection["seen"], (i + 1).to_string());
    }
}

#[test]
fn websocket_scripts() {
    let req = WsRequest {
        url: "wss://echo.example.com/ws".into(),
        headers: vec![KeyValue::new("Origin", "https://app")],
        subprotocols: vec!["v1".into()],
        ..Default::default()
    };
    // Before sending a message: headers and the outgoing message can change.
    let mut ctx = protocol::websocket(&req);
    ctx.request["message"] = json!({"data": "{\"op\":\"ping\"}", "type": "text"});
    let out = run(job(
        ScriptEvent::PreRequest,
        ctx,
        r#"
        pm.test('shape', () => {
            pm.expect(pm.info.protocol).to.equal('websocket');
            pm.expect(pm.request.headers.get('origin')).to.equal('https://app');
            pm.expect(pm.request.subprotocols).to.eql(['v1']);
            pm.expect(JSON.parse(pm.request.message.data)).to.eql({op: 'ping'});
        });
        pm.request.headers.upsert({key: 'Authorization', value: 'Bearer x'});
        pm.request.subprotocols.push('v2');
        pm.request.message.data = JSON.stringify({op: 'ping', at: 1});
        "#,
    ));
    all_passed(&out);
    let changed = out.protocol_request.unwrap();
    assert_eq!(changed["message"]["data"], "{\"op\":\"ping\",\"at\":1}");
    let mut applied = req.clone();
    protocol::apply_websocket(&mut applied, &changed);
    assert_eq!(
        applied.headers,
        vec![KeyValue::new("Origin", "https://app"), KeyValue::new("Authorization", "Bearer x")]
    );
    assert_eq!(applied.subprotocols, vec!["v1".to_string(), "v2".into()]);

    // Every received message.
    let mut msg = protocol::message(json!("{\"op\":\"pong\",\"n\":2}"), Direction::Incoming, 1);
    msg["type"] = json!("text");
    let out = run(job(
        ScriptEvent::OnMessage,
        protocol::websocket(&req).with_message(msg),
        r#"
        pm.test('pong', () => {
            pm.expect(pm.message.type).to.equal('text');
            pm.expect(pm.message.text()).to.contain('pong');
            pm.expect(pm.message.json().n).to.equal(2);
        });
        console.log('got', pm.message.json().op);
        "#,
    ));
    all_passed(&out);
    assert_eq!(out.console[0].message, "got pong");

    // After the connection closed.
    let summary = json!({
        "closeCode": 1000, "closeReason": "bye", "responseTime": 250,
        "messages": [protocol::message(json!("a"), Direction::Outgoing, 1), protocol::message(json!("b"), Direction::Incoming, 2)],
    });
    let out = run(job(
        ScriptEvent::PostResponse,
        protocol::websocket(&req).with_response(summary),
        r#"
        pm.test('closed cleanly', () => pm.expect(pm.response.closeCode).to.equal(1000));
        pm.test('received', () => pm.expect(pm.response.messages.filter((m) => m.direction === 'incoming').map((m) => m.text())).to.eql(['b']));
        pm.test('ok', () => pm.response.to.be.ok);
        "#,
    ));
    all_passed(&out);
}

#[test]
fn socketio_scripts() {
    let req = merpati_core::protocols::SocketIoRequest {
        url: "http://localhost:3000".into(),
        namespace: "/chat".into(),
        auth_payload: "{\"token\":\"a\"}".into(),
        ..Default::default()
    };
    let mut ctx = protocol::socketio(&req);
    ctx.request["message"] = protocol::socketio_message(&merpati_core::protocols::SocketIoMessage {
        event: "join".into(),
        args: "[\"room-1\"]".into(),
        ..Default::default()
    });
    let mut j = job(
        ScriptEvent::PreRequest,
        ctx,
        "pm.request.auth.token = pm.environment.get('t'); pm.request.message.args.push({since: 5});\
         pm.test('ns', () => pm.expect(pm.request.namespace).to.equal('/chat'));",
    );
    j.variables.environment.insert("t".into(), "b".into());
    let out = run(j);
    all_passed(&out);
    let changed = out.protocol_request.unwrap();
    assert_eq!(changed["message"]["args"], json!(["room-1", {"since": 5}]));
    let mut applied = req.clone();
    protocol::apply_socketio(&mut applied, &changed);
    assert_eq!(serde_json::from_str::<Value>(&applied.auth_payload).unwrap(), json!({"token": "b"}));

    let mut msg = protocol::message(json!(["hello", {"from": "ada"}]), Direction::Incoming, 5);
    msg["event"] = json!("chat");
    msg["args"] = json!(["hello", {"from": "ada"}]);
    let out = run(job(
        ScriptEvent::OnMessage,
        protocol::socketio(&req).with_message(msg),
        "pm.test('chat', () => { pm.expect(pm.message.event).to.equal('chat');\
           pm.expect(pm.message.args[1].from).to.equal('ada'); pm.expect(pm.message.json()[0]).to.equal('hello'); });",
    ));
    all_passed(&out);
}

#[test]
fn mqtt_scripts() {
    let req = MqttRequest { url: "mqtt://broker:1883".into(), client_id: "c1".into(), ..Default::default() };
    let publish =
        MqttMessage { topic: "sensors/1".into(), payload: "{\"t\": 20}".into(), qos: 1, ..Default::default() };
    let mut ctx = protocol::mqtt(&req);
    ctx.request["message"] = protocol::mqtt_message(&publish);
    let out = run(job(
        ScriptEvent::PreRequest,
        ctx,
        "const p = JSON.parse(pm.request.message.payload); p.t += 1.5; p.at = 'now';\
         pm.request.message.payload = JSON.stringify(p); pm.request.message.retain = true; pm.request.clientId = 'c2';",
    ));
    let changed = out.protocol_request.unwrap();
    let mut applied_req = req.clone();
    protocol::apply_mqtt(&mut applied_req, &changed);
    assert_eq!(applied_req.client_id, "c2");
    let mut applied = publish.clone();
    protocol::apply_mqtt_message(&mut applied, &changed["message"]);
    assert_eq!(applied.payload, "{\"t\":21.5,\"at\":\"now\"}");
    assert!(applied.retain);
    assert_eq!(applied.qos, 1);

    let mut msg = protocol::message(json!("{\"t\":21.5}"), Direction::Incoming, 9);
    msg["topic"] = json!("sensors/1");
    msg["qos"] = json!(1);
    msg["retain"] = json!(false);
    let out = run(job(
        ScriptEvent::OnMessage,
        protocol::mqtt(&req).with_message(msg),
        "pm.test('reading', () => { pm.expect(pm.message.topic).to.match(/^sensors\\//);\
           pm.expect(pm.message.json().t).to.be.within(-40, 60); pm.expect(pm.message.qos).to.equal(1); });\
         pm.test('too hot', () => pm.expect(pm.message.json().t).to.be.below(21));",
    ));
    assert_eq!(results(&out), vec![("reading".into(), true), ("too hot".into(), false)]);
}

#[test]
fn mcp_scripts() {
    let req = McpRequest {
        transport: McpTransport::Http { url: "http://localhost:8000/mcp".into(), headers: vec![] },
        ..Default::default()
    };
    let call = McpSavedCall {
        id: "c".into(),
        name: String::new(),
        kind: McpCallKind::Tool,
        target: "get_weather".into(),
        arguments: "{\"city\": \"Jakarta\"}".into(),
    };
    let out = run(job(
        ScriptEvent::PreRequest,
        protocol::mcp(&req, Some(&call)),
        "pm.test('call', () => { pm.expect(pm.request.kind).to.equal('tool'); pm.expect(pm.request.name).to.equal('get_weather'); });\
         pm.request.arguments.units = 'metric'; pm.request.headers.add({key: 'X-Key', value: 'k'});",
    ));
    all_passed(&out);
    let (mut r2, mut c2) = (req.clone(), call.clone());
    protocol::apply_mcp(&mut r2, Some(&mut c2), out.protocol_request.as_ref().unwrap());
    assert_eq!(serde_json::from_str::<Value>(&c2.arguments).unwrap(), json!({"city": "Jakarta", "units": "metric"}));
    match &r2.transport {
        McpTransport::Http { headers, .. } => assert_eq!(headers, &vec![KeyValue::new("X-Key", "k")]),
        other => panic!("{other:?}"),
    }

    let result = json!({"content": [{"type": "text", "text": "31 C"}, {"type": "text", "text": "sunny"}], "structuredContent": {"temp": 31}});
    let out = run(job(
        ScriptEvent::PostResponse,
        protocol::mcp(&req, Some(&call)).with_response(protocol::mcp_response(result, false, 40.0)),
        "pm.test('result', () => { pm.response.to.be.ok; pm.expect(pm.response.text()).to.equal('31 C\\nsunny');\
           pm.expect(pm.response.json().structuredContent.temp).to.equal(31); pm.expect(pm.response.responseTime).to.equal(40); });",
    ));
    all_passed(&out);
    let out = run(job(
        ScriptEvent::PostResponse,
        protocol::mcp(&req, None).with_response(protocol::mcp_response(json!({"content": []}), true, 1.0)),
        "pm.test('ok', () => pm.response.to.be.ok);",
    ));
    assert_eq!(results(&out), vec![("ok".into(), false)]);
}

#[test]
fn ai_scripts() {
    let req = AiRequest {
        model: "gpt-4o-mini".into(),
        system: "Be brief".into(),
        messages: vec![AiMessage { role: "user".into(), content: "Hi".into() }],
        ..Default::default()
    };
    let out = run(job(
        ScriptEvent::PreRequest,
        protocol::ai(&req),
        "pm.request.messages.push({role: 'user', content: 'Answer as JSON: ' + pm.variables.get('q')});\
         pm.request.temperature = 0; pm.request.model = 'gpt-4.1-mini';\
         pm.test('provider', () => pm.expect(pm.request.provider).to.equal('open_ai'));",
    ));
    all_passed(&out);
    let mut applied = req.clone();
    protocol::apply_ai(&mut applied, out.protocol_request.as_ref().unwrap());
    assert_eq!(applied.model, "gpt-4.1-mini");
    assert_eq!(applied.temperature, Some(0.0));
    assert_eq!(applied.messages.len(), 2);
    assert_eq!(applied.messages[1].content, "Answer as JSON: undefined");
    assert_eq!(applied.extra, "");

    let response = protocol::ai_response(
        "```json\n{\"answer\": 42}\n```",
        "gpt-4.1-mini",
        "stop",
        json!({"inputTokens": 12, "outputTokens": 7}),
        200,
        830.0,
    );
    let out = run(job(
        ScriptEvent::PostResponse,
        protocol::ai(&req).with_response(response),
        "pm.test('answer', () => { pm.response.to.be.ok; pm.response.to.have.statusCode(200);\
           pm.expect(pm.response.json().answer).to.equal(42); pm.expect(pm.response.text()).to.contain('42');\
           pm.expect(pm.response.usage.outputTokens).to.be.below(100); pm.expect(pm.response.finishReason).to.equal('stop'); });\
         pm.test('fast', () => pm.response.to.have.responseTime.below(500));",
    ));
    assert_eq!(results(&out), vec![("answer".into(), true), ("fast".into(), false)]);
}

struct FakeHttp;

impl ScriptHttp for FakeHttp {
    fn send(&self, req: ScriptRequest, _: &VariableScopes) -> Result<ScriptResponse, String> {
        let body = json!({"token": "from-auth", "url": req.url}).to_string();
        Ok(ScriptResponse {
            code: 200,
            status: "OK".into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            response_size: body.len() as u64,
            body: ResponseBody::Memory(body.into()),
            response_time_ms: 1.0,
        })
    }
}

#[test]
fn sandbox_features_work_in_protocol_jobs() {
    let mut j = job(
        ScriptEvent::PreRequest,
        protocol::grpc(&greeter()),
        r#"
        pm.sendRequest('https://auth.example.com/token', (err, res) => {
            pm.environment.set('token', res.json().token);
            pm.request.metadata.upsert({key: 'authorization', value: 'Bearer ' + res.json().token});
        });
        pm.test('legacy request global', () => pm.expect(request.methodPath).to.equal('helloworld.Greeter/SayHello'));
        pm.test('cookies empty', () => pm.expect(pm.cookies.count()).to.equal(0));
        pm.test('no message', () => pm.expect(pm.message).to.be.undefined);
        pm.test('no response', () => pm.expect(pm.response).to.be.undefined);
        "#,
    );
    j.http = Some(Arc::new(FakeHttp));
    let out = run(j);
    all_passed(&out);
    assert_eq!(out.variables.environment["token"], "from-auth");
    let metadata = &out.protocol_request.unwrap()["metadata"];
    assert!(metadata.as_array().unwrap().iter().any(|m| m["value"] == "Bearer from-auth"), "{metadata}");

    // Replacing pm.request and skipping the call.
    let out = run(job(
        ScriptEvent::PreRequest,
        protocol::grpc(&greeter()),
        "pm.request = {url: 'grpc://x:1', methodPath: 'a.B/C', metadata: [], messages: [{data: {}}]};\
         pm.request.metadata.add({key: 'k', value: 'v'}); pm.execution.skipRequest();",
    ));
    assert!(out.skip_request);
    assert_eq!(
        out.protocol_request.unwrap(),
        json!({"url": "grpc://x:1", "methodPath": "a.B/C", "metadata": [{"key": "k", "value": "v"}], "messages": [{"data": {}}]})
    );
}

#[test]
fn protocol_jobs_keep_working_after_errors_and_http_jobs_have_no_protocol_request() {
    let e = BoaEngine::new();
    let ctx = protocol::grpc(&greeter());
    let input = ctx.request.clone();
    let out = e.run(job(ScriptEvent::PreRequest, ctx, "pm.request.url = 'changed'; throw new Error('boom');"));
    assert_eq!(out.errors.len(), 1);
    assert_eq!(out.protocol_request.unwrap()["url"], "changed");
    // Timeouts return the input unchanged.
    let mut j = job(ScriptEvent::PreRequest, protocol::grpc(&greeter()), "while (true) {}");
    j.timeout = std::time::Duration::from_millis(100);
    let out = e.run(j);
    assert!(out.errors[0].message.contains("timed out"));
    assert_eq!(out.protocol_request, Some(input));
    // Empty scripts never reach the engine but still answer.
    let out = e.run(job(ScriptEvent::OnMessage, protocol::grpc(&greeter()), " "));
    assert!(out.protocol_request.is_some());

    let http = ScriptJob::new(
        ScriptEvent::PreRequest,
        vec![ScriptSource { location: "r".into(), code: "pm.request.headers.add({key: 'a', value: 'b'})".into() }],
        ScriptRequest { method: "GET".into(), url: "https://x".into(), ..Default::default() },
    );
    let out = e.run(http);
    assert!(out.protocol_request.is_none());
    assert_eq!(out.request.headers, vec![KeyValue::new("a", "b")]);
}
