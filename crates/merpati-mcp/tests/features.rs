//! Client features (sampling, elicitation, roots) against the
//! `fake_mcp_server` binary, which sends the server-to-client requests.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use merpati_mcp::{
    ElicitFieldKind, ElicitationRequest, ElicitationResponse, McpClient, McpConfig, McpError, Root, SamplingRequest,
    SamplingResult, Transport,
};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

const SERVER: &str = env!("CARGO_BIN_EXE_fake_mcp_server");

fn config(timeout: Duration) -> McpConfig {
    McpConfig {
        transport: Transport::Stdio { command: SERVER.into(), args: vec![], env: vec![], cwd: None },
        timeout,
        ..Default::default()
    }
}

/// The JSON a feature tool returns: `{"result": ...}` or `{"error": ...}`.
async fn tool_json(client: &McpClient, name: &str, args: Value) -> Value {
    let r = client.call_tool(name, args).await.unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::from_str(&r.text()).unwrap_or_else(|_| Value::String(r.text()))
}

#[tokio::test]
async fn features_are_off_by_default() {
    let client = McpClient::connect(config(Duration::from_secs(10)), |_| {}).await.unwrap();
    assert_eq!(tool_json(&client, "caps", json!({})).await, json!({}));
    for tool in ["sample", "elicit", "roots", "ask_unknown"] {
        let v = tool_json(&client, tool, json!({})).await;
        assert_eq!(v["error"]["code"], json!(-32601), "{tool}: {v}");
    }
    assert!(matches!(client.set_roots(vec![]), Err(McpError::Config(_))));
    client.close().await;
}

#[tokio::test]
async fn sampling_elicitation_and_roots_round_trips() {
    let seen_sampling: Arc<Mutex<Option<SamplingRequest>>> = Arc::default();
    let seen_elicitation: Arc<Mutex<Option<ElicitationRequest>>> = Arc::default();
    let reject = Arc::new(AtomicBool::new(false));
    let s = seen_sampling.clone();
    let rj = reject.clone();
    let e = seen_elicitation.clone();
    let client = McpClient::builder(config(Duration::from_secs(10)))
        .sampling(move |req: SamplingRequest| {
            *s.lock() = Some(req.clone());
            let reject = rj.load(Ordering::SeqCst);
            async move {
                if reject {
                    return Err(McpError::rejected("User rejected sampling request"));
                }
                Ok(SamplingResult::text("test-model", format!("echo: {}", req.transcript())))
            }
        })
        .elicitation(move |req: ElicitationRequest| {
            *e.lock() = Some(req);
            async move {
                let mut content = Map::new();
                content.insert("name".into(), json!("Ann"));
                Ok(ElicitationResponse::Accept(content))
            }
        })
        .roots(vec![Root::new("file:///work/a", Some("a".into()))])
        .connect()
        .await
        .unwrap();

    let caps = tool_json(&client, "caps", json!({})).await;
    assert_eq!(caps, json!({"roots": {"listChanged": true}, "sampling": {}, "elicitation": {}}));

    // Sampling.
    let v = tool_json(&client, "sample", json!({"prompt": "What is 2+2?"})).await;
    assert_eq!(
        v["result"],
        json!({"model": "test-model", "role": "assistant", "content": {"type": "text", "text": "echo: user: What is 2+2?"}, "stopReason": "endTurn"})
    );
    let req = seen_sampling.lock().clone().unwrap();
    assert_eq!(req.server, "fake-mcp");
    assert_eq!((req.max_tokens, req.system_prompt.as_deref()), (50, Some("You are a test")));
    assert_eq!(req.model_hints, ["small"]);
    reject.store(true, Ordering::SeqCst);
    let v = tool_json(&client, "sample", json!({})).await;
    assert_eq!(v["error"], json!({"code": -1, "message": "User rejected sampling request"}));

    // Elicitation.
    let v = tool_json(&client, "elicit", json!({})).await;
    assert_eq!(v["result"], json!({"action": "accept", "content": {"name": "Ann"}}));
    let req = seen_elicitation.lock().clone().unwrap();
    assert_eq!(req.message, "Your name?");
    let fields = req.fields();
    assert_eq!((fields[0].name.as_str(), fields[0].label(), fields[0].required), ("name", "Name", true));
    assert!(matches!(fields[0].kind, ElicitFieldKind::Text { .. }));

    // Roots and list_changed.
    let v = tool_json(&client, "roots", json!({})).await;
    assert_eq!(v["result"], json!({"roots": [{"uri": "file:///work/a", "name": "a"}]}));
    client.set_roots(vec![Root::new("file:///work/b", None)]).unwrap();
    assert_eq!(client.roots(), vec![Root::new("file:///work/b", None)]);
    assert_eq!(tool_json(&client, "roots_changed", json!({})).await, json!(1));
    let v = tool_json(&client, "roots", json!({})).await;
    assert_eq!(v["result"], json!({"roots": [{"uri": "file:///work/b"}]}));

    // Unknown server requests still get method-not-found.
    let v = tool_json(&client, "ask_unknown", json!({})).await;
    assert_eq!(v["error"]["code"], json!(-32601));
    client.close().await;
}

#[tokio::test]
async fn server_can_cancel_a_sampling_request() {
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let (d, st) = (dropped.clone(), started.clone());
    let client = McpClient::builder(config(Duration::from_secs(10)))
        .sampling(move |_req: SamplingRequest| {
            st.store(true, Ordering::SeqCst);
            let flag = DropFlag(d.clone());
            async move {
                let _flag = flag;
                std::future::pending::<()>().await;
                Ok(SamplingResult::text("never", "never"))
            }
        })
        .connect()
        .await
        .unwrap();
    assert_eq!(client.call_tool("sample_cancel", json!({})).await.unwrap().text(), "cancel sent");
    // The cancellation may arrive before the handler task first runs; then
    // the handler is never called. Either way nothing may keep running.
    for _ in 0..50 {
        if dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !started.load(Ordering::SeqCst) || dropped.load(Ordering::SeqCst),
        "the handler future was dropped on notifications/cancelled"
    );
    // The connection is still usable and the cancelled request got no answer.
    client.ping().await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn request_timeout_pauses_while_the_user_answers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    // Generous enough for `initialize` on a loaded machine.
    let client = McpClient::builder(config(Duration::from_millis(1500)))
        .sampling(move |_req: SamplingRequest| {
            c.fetch_add(1, Ordering::SeqCst);
            async move {
                // The user takes much longer than the request timeout.
                tokio::time::sleep(Duration::from_millis(4000)).await;
                Ok(SamplingResult::text("slow-model", "done"))
            }
        })
        .connect()
        .await
        .unwrap();
    let v = tool_json(&client, "sample", json!({})).await;
    assert_eq!(v["result"]["model"], json!("slow-model"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    client.close().await;
}
