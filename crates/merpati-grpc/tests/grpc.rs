mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use common::{Reflection, protos_dir, self_signed, spawn};
use merpati_grpc::{
    Direction, GrpcCall, GrpcError, GrpcEvent, GrpcSchema, MethodKind, Target, invoke, invoke_unary, load_proto_files,
    load_via_reflection,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const SVC: &str = "helloworld.Greeter";

fn file_schema() -> GrpcSchema {
    load_proto_files(&[PathBuf::from("helloworld/greeter.proto")], &[PathBuf::from(".")], Some(&protos_dir())).unwrap()
}

fn target(url: String) -> Target {
    Target { url, timeout: Some(Duration::from_secs(10)), ..Default::default() }
}

fn parse(json: &str) -> Value {
    serde_json::from_str(json).unwrap()
}

// --- events -----------------------------------------------------------------

struct Events {
    rx: mpsc::UnboundedReceiver<GrpcEvent>,
    seen: Vec<GrpcEvent>,
}

fn events() -> (impl Fn(GrpcEvent) + Send + Sync + 'static, Events) {
    let (tx, rx) = mpsc::unbounded_channel();
    (move |e| drop(tx.send(e)), Events { rx, seen: Vec::new() })
}

impl Events {
    /// Waits for the first event matching `pred`, recording everything seen.
    async fn until(&mut self, pred: impl Fn(&GrpcEvent) -> bool) -> GrpcEvent {
        loop {
            let e = tokio::time::timeout(Duration::from_secs(10), self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting; seen: {:#?}", self.seen))
                .expect("event channel closed");
            self.seen.push(e.clone());
            if pred(&e) {
                return e;
            }
        }
    }

    async fn finished(&mut self) -> (i32, String, String) {
        match self.until(|e| matches!(e, GrpcEvent::Finished { .. })).await {
            GrpcEvent::Finished { code, code_name, message, .. } => (code, code_name, message),
            _ => unreachable!(),
        }
    }

    fn received(&self) -> Vec<Value> {
        self.seen
            .iter()
            .filter_map(|e| match e {
                GrpcEvent::Message { direction: Direction::Received, json, .. } => Some(parse(json)),
                _ => None,
            })
            .collect()
    }

    fn sent(&self) -> Vec<Value> {
        self.seen
            .iter()
            .filter_map(|e| match e {
                GrpcEvent::Message { direction: Direction::Sent, json, .. } => Some(parse(json)),
                _ => None,
            })
            .collect()
    }
}

fn is_received_with(e: &GrpcEvent, text: &str) -> bool {
    matches!(e, GrpcEvent::Message { direction: Direction::Received, json, .. } if json.contains(text))
}

// --- schema -----------------------------------------------------------------

#[test]
fn proto_files_services_methods_and_template() {
    let schema = file_schema();
    let services = schema.services();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].name, SVC);
    let kinds: Vec<_> = services[0].methods.iter().map(|m| (m.name.as_str(), m.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            ("SayHello", MethodKind::Unary),
            ("LotsOfReplies", MethodKind::ServerStreaming),
            ("LotsOfGreetings", MethodKind::ClientStreaming),
            ("BidiHello", MethodKind::Bidi),
        ]
    );
    let m = schema.method(SVC, "SayHello").unwrap();
    assert_eq!(m.input_type, "helloworld.HelloRequest");
    assert_eq!(m.output_type, "helloworld.HelloReply");
    assert_eq!(
        parse(&m.input_template),
        json!({
            "name": "",
            "times": 0,
            "mood": "MOOD_UNSPECIFIED",
            "tags": [""],
            "address": {"city": "", "geo": {"lat": 0.0, "lng": 0.0, "note": {}}},
            "scores": {"key": 0},
            "email": "",
            "sentAt": "1970-01-01T00:00:00Z",
            "avatar": "",
            "vip": false
        })
    );
    // Declaration order is kept.
    assert!(m.input_template.find("\"name\"").unwrap() < m.input_template.find("\"vip\"").unwrap());
    // Every template is a valid request for its method.
    for method in &services[0].methods {
        schema.validate_request(SVC, &method.name, &method.input_template).unwrap();
    }
    assert!(schema.method(SVC, "Nope").is_none());
    assert!(schema.method("helloworld.Nope", "SayHello").is_none());
    assert!(matches!(schema.validate_request("x.Y", "Z", "{}"), Err(GrpcError::UnknownService(_))));
    assert_eq!(parse(&schema.message_template("common.Note").unwrap()), json!({"text": ""}));
}

#[test]
fn proto_files_absolute_paths_and_descriptor_round_trip() {
    let file = protos_dir().join("helloworld/greeter.proto");
    let schema = load_proto_files(&[file], &[protos_dir()], None).unwrap();
    let bytes = schema.to_descriptor_set_bytes();
    let again = GrpcSchema::from_descriptor_set_bytes(&bytes).unwrap();
    assert_eq!(again.services(), schema.services());
}

#[test]
fn proto_file_errors_are_readable() {
    // Missing import path for common/geo.proto.
    let err = load_proto_files(&[PathBuf::from("helloworld/greeter.proto")], &[], Some(&protos_dir())).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("common/geo.proto"), "{msg}");

    let err = load_proto_files(&[PathBuf::from("broken.proto")], &[], Some(&protos_dir())).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("broken.proto:7:"), "{msg}");

    let err = load_proto_files(&[PathBuf::from("nope.proto")], &[], Some(&protos_dir())).unwrap_err();
    assert!(err.to_string().contains("file not found"), "{err}");
}

#[test]
fn invalid_json_is_reported_with_type() {
    let schema = file_schema();
    for bad in [r#"{"nme": "x"}"#, r#"{"times": "abc"}"#, r#"{"name": }"#, r#"{"email": "a", "phone": "b"}"#, "[1]"] {
        match schema.validate_request(SVC, "SayHello", bad) {
            Err(GrpcError::InvalidJson { type_name, message }) => {
                assert_eq!(type_name, "helloworld.HelloRequest");
                assert!(!message.is_empty());
            }
            other => panic!("{bad}: expected InvalidJson, got {other:?}"),
        }
    }
    let err = schema.validate_request(SVC, "SayHello", r#"{"nme": "x"}"#).unwrap_err();
    assert!(err.to_string().starts_with("Bad JSON for type helloworld.HelloRequest:"), "{err}");
    // Blank is the empty message; proto field names are accepted too.
    schema.validate_request(SVC, "SayHello", "  ").unwrap();
    schema.validate_request(SVC, "SayHello", r#"{"sent_at": "2024-01-02T03:04:05Z", "scores": {"a": "12"}}"#).unwrap();
}

// --- reflection -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn reflection_v1_matches_proto_files() {
    let server = spawn(Reflection::V1, None).await;
    let schema = load_via_reflection(&target(server.url())).await.unwrap();
    // The reflection service itself is not listed.
    assert_eq!(schema.services(), file_schema().services());
    assert!(schema.warnings().is_empty(), "{:?}", schema.warnings());
}

#[tokio::test(flavor = "multi_thread")]
async fn reflection_falls_back_to_v1alpha() {
    let server = spawn(Reflection::V1Alpha, None).await;
    let schema = load_via_reflection(&target(server.url())).await.unwrap();
    assert_eq!(schema.services(), file_schema().services());
}

#[tokio::test(flavor = "multi_thread")]
async fn reflection_unsupported_is_explained() {
    let server = spawn(Reflection::None, None).await;
    let err = load_via_reflection(&target(server.url())).await.unwrap_err();
    assert!(matches!(err, GrpcError::Reflection(_)), "{err:?}");
    assert!(err.to_string().contains("does not support server reflection"), "{err}");
}

// --- unary ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn unary_with_metadata_round_trip() {
    let server = spawn(Reflection::V1, None).await;
    let schema = load_via_reflection(&target(server.url())).await.unwrap();
    let mut t = target(format!("grpc://{}", server.url()));
    let binary = [1u8, 2, 3, 255];
    t.metadata = vec![
        ("X-User".into(), "ada".into()),
        ("x-trace-bin".into(), base64::engine::general_purpose::STANDARD_NO_PAD.encode(binary)),
        ("other".into(), "not echoed".into()),
    ];
    let req = r#"{"name": "Ada", "times": 7, "mood": "HAPPY", "address": {"city": "Paris"}}"#;
    let resp = invoke_unary(&t, &schema, SVC, "SayHello", req).await.unwrap();
    assert_eq!(parse(&resp.json), json!({"message": "Hello, Ada from Paris!", "index": 7, "mood": "HAPPY"}));
    assert_eq!((resp.code, resp.code_name.as_str()), (0, "OK"));
    let header = |k: &str| resp.headers.iter().find(|(hk, _)| hk == k).map(|(_, v)| v.clone());
    assert_eq!(header("echo-x-user").as_deref(), Some("ada"));
    assert_eq!(header("echo-x-trace-bin").as_deref(), Some("AQID/w=="));
    assert_eq!(header("content-type").as_deref(), Some("application/grpc"));
    assert!(header("echo-other").is_none());
    assert!(!resp.trailers.iter().any(|(k, _)| k == "grpc-status"));

    // Default-valued fields are rendered.
    let resp = invoke_unary(&t, &schema, SVC, "SayHello", "{}").await.unwrap();
    assert_eq!(parse(&resp.json), json!({"message": "Hello, !", "index": 0, "mood": "MOOD_UNSPECIFIED"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn unary_error_status_and_bad_input() {
    let server = spawn(Reflection::None, None).await;
    let schema = file_schema();
    let t = target(server.url());
    match invoke_unary(&t, &schema, SVC, "SayHello", r#"{"name": "missing"}"#).await {
        Err(GrpcError::Status { code, code_name, message, metadata }) => {
            assert_eq!((code, code_name.as_str(), message.as_str()), (5, "NOT_FOUND", "no greeting for missing"));
            assert!(metadata.contains(&("x-reason".to_string(), "unknown-person".to_string())), "{metadata:?}");
        }
        other => panic!("expected NOT_FOUND, got {other:?}"),
    }
    let err = invoke_unary(&t, &schema, SVC, "SayHello", r#"{"nme": 1}"#).await.unwrap_err();
    assert!(matches!(err, GrpcError::InvalidJson { .. }), "{err:?}");
    let err = invoke_unary(&t, &schema, SVC, "LotsOfReplies", "{}").await.unwrap_err();
    assert!(matches!(err, GrpcError::NotUnary { .. }), "{err:?}");
    let err = invoke_unary(&t, &schema, "helloworld.Nope", "SayHello", "{}").await.unwrap_err();
    assert_eq!(err.to_string(), "Unknown service \"helloworld.Nope\"");
    let mut bad_md = t.clone();
    bad_md.metadata = vec![("x-bin".into(), "%%%".into())];
    let err = invoke_unary(&bad_md, &schema, SVC, "SayHello", "{}").await.unwrap_err();
    assert!(matches!(err, GrpcError::InvalidMetadata { .. }), "{err:?}");

    // The same NOT_FOUND through the event API.
    let (on_event, mut ev) = events();
    let rt = tokio::runtime::Handle::current();
    let call = GrpcCall::start(
        &rt,
        t.clone(),
        Arc::new(schema.clone()),
        SVC,
        "SayHello",
        Some(r#"{"name":"missing"}"#.into()),
        on_event,
    )
    .unwrap();
    assert_eq!(ev.finished().await, (5, "NOT_FOUND".into(), "no greeting for missing".into()));
    assert!(ev.seen.iter().any(|e| matches!(e, GrpcEvent::Trailers(t) if t.iter().any(|(k, _)| k == "x-reason"))));
    assert!(!ev.seen.iter().any(|e| matches!(e, GrpcEvent::Error(_))));
    assert!(matches!(call.send("{}"), Err(GrpcError::NotClientStreaming(_))));

    // Invalid first message fails before connecting.
    let (on_event, _ev) = events();
    let err = GrpcCall::start(&rt, t, Arc::new(schema), SVC, "SayHello", Some("{oops".into()), on_event).err().unwrap();
    assert!(matches!(err, GrpcError::InvalidJson { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn deadline_exceeded() {
    let server = spawn(Reflection::None, None).await;
    let mut t = target(server.url());
    t.timeout = Some(Duration::from_millis(300));
    let started = std::time::Instant::now();
    let err = invoke_unary(&t, &file_schema(), SVC, "SayHello", r#"{"name": "slow"}"#).await.unwrap_err();
    assert_eq!(err.code(), 4, "{err:?}");
    assert!(err.to_string().starts_with("DEADLINE_EXCEEDED"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_refused() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let t = target(format!("127.0.0.1:{port}"));
    let err = invoke_unary(&t, &file_schema(), SVC, "SayHello", "{}").await.unwrap_err();
    assert!(matches!(err, GrpcError::Connect(_)), "{err:?}");
    assert!(err.to_string().contains("Connection refused"), "{err}");

    let (on_event, mut ev) = events();
    let _call = GrpcCall::start(
        &tokio::runtime::Handle::current(),
        t,
        Arc::new(file_schema()),
        SVC,
        "SayHello",
        None,
        on_event,
    )
    .unwrap();
    let (code, name, _) = ev.finished().await;
    assert_eq!((code, name.as_str()), (14, "UNAVAILABLE"));
    assert!(matches!(ev.seen[0], GrpcEvent::Connecting));
    assert!(matches!(&ev.seen[1], GrpcEvent::Error(m) if m.contains("Connection refused")));
    assert!(ev.sent().is_empty());
}

// --- streaming --------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn server_streaming() {
    let server = spawn(Reflection::None, None).await;
    let schema = Arc::new(file_schema());
    let (on_event, mut ev) = events();
    let call = GrpcCall::start(
        &tokio::runtime::Handle::current(),
        target(server.url()),
        schema.clone(),
        SVC,
        "LotsOfReplies",
        Some(r#"{"name": "Bo", "times": 3}"#.into()),
        on_event,
    )
    .unwrap();
    assert_eq!(call.kind(), MethodKind::ServerStreaming);
    assert_eq!(ev.finished().await.0, 0);
    let msgs: Vec<String> = ev.received().iter().map(|m| m["message"].as_str().unwrap().to_string()).collect();
    assert_eq!(msgs, vec!["Hello, Bo #1", "Hello, Bo #2", "Hello, Bo #3"]);
    assert_eq!(ev.sent().len(), 1);
    assert_eq!(ev.sent()[0]["name"], "Bo");
    // Order: Connecting, headers, messages..., trailers, finished.
    assert!(matches!(ev.seen.first(), Some(GrpcEvent::Connecting)));
    let pos = |pred: &dyn Fn(&GrpcEvent) -> bool| ev.seen.iter().position(pred).unwrap();
    let headers = pos(&|e| matches!(e, GrpcEvent::ResponseHeaders(_)));
    let first_msg = pos(&|e| is_received_with(e, "#1"));
    let trailers = pos(&|e| matches!(e, GrpcEvent::Trailers(_)));
    assert!(headers < first_msg && first_msg < trailers);
    assert!(matches!(ev.seen.last(), Some(GrpcEvent::Finished { .. })));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(call.is_finished());

    // Collecting variant, plus an error status after one message.
    let r = invoke(&target(server.url()), &schema, SVC, "LotsOfReplies", &[r#"{"name":"fail-midway"}"#.into()])
        .await
        .unwrap();
    assert_eq!(r.messages.len(), 1);
    assert_eq!((r.code, r.code_name.as_str(), r.message.as_str()), (8, "RESOURCE_EXHAUSTED", "quota used up"));
}

#[tokio::test(flavor = "multi_thread")]
async fn client_streaming() {
    let server = spawn(Reflection::None, None).await;
    let (on_event, mut ev) = events();
    let call = GrpcCall::start(
        &tokio::runtime::Handle::current(),
        target(server.url()),
        Arc::new(file_schema()),
        SVC,
        "LotsOfGreetings",
        Some(r#"{"name": "a"}"#.into()),
        on_event,
    )
    .unwrap();
    call.send(r#"{"name": "b"}"#).unwrap();
    assert!(matches!(call.send(r#"{"name": 5}"#), Err(GrpcError::InvalidJson { .. })));
    call.send(r#"{"name": "c"}"#).unwrap();
    assert!(call.can_send());
    call.end_stream();
    assert!(!call.can_send());
    assert!(matches!(call.send("{}"), Err(GrpcError::StreamClosed)));
    assert_eq!(ev.finished().await.0, 0);
    assert_eq!(ev.received(), vec![json!({"message": "Hello, a, b, c!", "index": 3, "mood": "MOOD_UNSPECIFIED"})]);
    assert_eq!(ev.sent().len(), 3);

    // Collecting variant with no initial message at all.
    let r = invoke(&target(server.url()), &file_schema(), SVC, "LotsOfGreetings", &[]).await.unwrap();
    assert_eq!(r.code, 0);
    assert_eq!(parse(&r.messages[0])["index"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn bidi_streaming_interactive_and_cancel() {
    let server = spawn(Reflection::V1, None).await;
    let schema = Arc::new(load_via_reflection(&target(server.url())).await.unwrap());
    let rt = tokio::runtime::Handle::current();
    let (on_event, mut ev) = events();
    let call = GrpcCall::start(
        &rt,
        target(server.url()),
        schema.clone(),
        SVC,
        "BidiHello",
        Some(r#"{"name":"one"}"#.into()),
        on_event,
    )
    .unwrap();
    ev.until(|e| is_received_with(e, "Hello, one!")).await;
    call.send(r#"{"name":"two","mood":"GRUMPY"}"#).unwrap();
    let reply = ev.until(|e| is_received_with(e, "Hello, two!")).await;
    if let GrpcEvent::Message { json, .. } = reply {
        assert_eq!(parse(&json), json!({"message": "Hello, two!", "index": 2, "mood": "GRUMPY"}));
    }
    call.end_stream();
    assert_eq!(ev.finished().await.0, 0);

    // Cancel an open stream; nothing sent first.
    let (on_event, mut ev) = events();
    let call = GrpcCall::start(&rt, target(server.url()), schema, SVC, "BidiHello", None, on_event).unwrap();
    ev.until(|e| matches!(e, GrpcEvent::ResponseHeaders(_))).await;
    call.cancel();
    assert_eq!(ev.finished().await, (1, "CANCELLED".into(), "Cancelled by client".into()));
    assert!(ev.sent().is_empty());
    assert!(matches!(call.send("{}"), Err(GrpcError::StreamClosed)));
}

// --- TLS --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn tls_with_and_without_verification() {
    let server = spawn(Reflection::V1, Some(self_signed())).await;
    let port = server.addr.port();
    let mut t = target(format!("grpcs://localhost:{port}"));
    t.verify_ssl = false;
    let schema = load_via_reflection(&t).await.unwrap();
    let resp = invoke_unary(&t, &schema, SVC, "SayHello", r#"{"name":"tls"}"#).await.unwrap();
    assert_eq!(parse(&resp.json)["message"], "Hello, tls!");

    // host:port + forced TLS behaves the same.
    let forced = Target { url: format!("localhost:{port}"), tls: Some(true), ..t.clone() };
    invoke_unary(&forced, &schema, SVC, "SayHello", "{}").await.unwrap();

    // Verification rejects the self-signed certificate.
    let verified = Target { verify_ssl: true, ..t.clone() };
    let err = invoke_unary(&verified, &schema, SVC, "SayHello", "{}").await.unwrap_err();
    assert!(matches!(err, GrpcError::Tls(_)), "{err:?}");
    assert!(err.to_string().contains("certificate"), "{err}");

    // Plaintext against a TLS port fails cleanly instead of hanging.
    let plain = Target { url: format!("grpc://localhost:{port}"), timeout: Some(Duration::from_secs(3)), ..t };
    assert!(invoke_unary(&plain, &schema, SVC, "SayHello", "{}").await.is_err());
}

// --- GUI usage: start from a plain thread with a runtime Handle --------------

#[test]
fn start_from_non_runtime_thread() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let server = rt.block_on(spawn(Reflection::V1, None));
    let schema = Arc::new(rt.block_on(load_via_reflection(&target(server.url()))).unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    let call = GrpcCall::start(
        rt.handle(),
        target(server.url()),
        schema,
        SVC,
        "BidiHello",
        Some(r#"{"name":"gui"}"#.into()),
        move |e| drop(tx.send(e)),
    )
    .unwrap();
    let mut got_reply = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(10)).expect("event") {
            GrpcEvent::Message { direction: Direction::Received, json, .. } => {
                assert!(json.contains("Hello, gui!"));
                got_reply = true;
                call.end_stream();
            }
            GrpcEvent::Finished { code, .. } => {
                assert_eq!(code, 0);
                break;
            }
            _ => {}
        }
    }
    assert!(got_reply);
}
