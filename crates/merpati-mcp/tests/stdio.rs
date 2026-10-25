//! stdio transport against the `fake_mcp_server` binary.

use std::sync::Arc;
use std::time::Duration;

use merpati_mcp::{Content, Direction, McpClient, McpConfig, McpError, McpEvent, Transport, args_template};
use parking_lot::Mutex;
use serde_json::{Map, json};

const SERVER: &str = env!("CARGO_BIN_EXE_fake_mcp_server");

fn config(env: Vec<(&str, &str)>) -> McpConfig {
    McpConfig {
        transport: Transport::Stdio {
            command: SERVER.into(),
            args: vec![],
            env: env.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            cwd: None,
        },
        timeout: Duration::from_secs(10),
        ..Default::default()
    }
}

type Events = Arc<Mutex<Vec<McpEvent>>>;

fn recorder() -> (Events, impl Fn(McpEvent) + Send + Sync + 'static) {
    let events: Events = Arc::default();
    let sink = events.clone();
    (events, move |e| sink.lock().push(e))
}

async fn wait_for(events: &Events, what: &str, pred: impl Fn(&McpEvent) -> bool) {
    for _ in 0..100 {
        if events.lock().iter().any(&pred) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}; events: {:#?}", events.lock());
}

#[tokio::test]
async fn full_session() {
    let (events, on_event) = recorder();
    let client = McpClient::connect(config(vec![]), on_event).await.expect("connect");

    let info = client.server_info();
    assert_eq!(info.name, "fake-mcp");
    assert_eq!(info.version, "1.2.3");
    assert_eq!(info.protocol_version, "2025-06-18");
    assert_eq!(info.instructions.as_deref(), Some("Use echo to test."));
    assert!(info.has_capability("tools") && info.has_capability("logging"));

    // Four pages (4 + 4 + 4 + 1).
    let tools = client.list_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names[..6], ["echo", "add", "fail", "env", "slow", "ask_ping"]);
    assert_eq!(names.len(), 13);
    assert_eq!(tools[0].title.as_deref(), Some("Echo"));
    assert_eq!(args_template(&tools[1].input_schema), json!({"a": 0, "b": 0}));
    assert!(tools[1].output_schema.is_some());

    let r = client.call_tool("echo", json!({"text": "hi there"})).await.unwrap();
    assert_eq!(r.content, vec![Content::Text { text: "hi there".into() }]);
    assert!(!r.is_error);
    assert_eq!(r.text(), "hi there");

    let r = client.call_tool("add", json!({"a": 2, "b": 3.5})).await.unwrap();
    assert_eq!(r.structured, Some(json!({"sum": 5.5})));

    let r = client.call_tool("fail", serde_json::Value::Null).await.unwrap();
    assert!(r.is_error);
    assert_eq!(r.text(), "it broke");

    let err = client.call_tool("nope", json!({})).await.unwrap_err();
    assert_eq!(err.rpc_code(), Some(-32602));
    assert!(err.to_string().contains("Unknown tool: nope"), "{err}");

    // Server-to-client request answered while the tool call waits.
    let r = client.call_tool("ask_ping", json!({})).await.unwrap();
    assert_eq!(r.text(), "pong received");

    let resources = client.list_resources().await.unwrap();
    assert_eq!(resources[0].uri, "mem://greeting");
    assert_eq!(resources[0].size, Some(5));
    let templates = client.list_resource_templates().await.unwrap();
    assert_eq!(templates[0].uri_template, "mem://items/{id}");
    let read = client.read_resource("mem://greeting").await.unwrap();
    assert_eq!(read["contents"][0]["text"], "hello");

    let prompts = client.list_prompts().await.unwrap();
    assert_eq!(prompts[0].name, "greet");
    assert!(prompts[0].arguments[0].required);
    let mut args = Map::new();
    args.insert("who".into(), json!("Ana"));
    let prompt = client.get_prompt("greet", args).await.unwrap();
    assert_eq!(prompt["messages"][0]["content"]["text"], "Say hi to Ana");

    client.ping().await.unwrap();

    // Clones share the session.
    let clone = client.clone();
    assert_eq!(clone.list_prompts().await.unwrap().len(), 1);

    wait_for(&events, "stderr line", |e| matches!(e, McpEvent::Stderr(s) if s == "called echo")).await;
    {
        let ev = events.lock();
        assert!(
            ev.iter().any(
                |e| matches!(e, McpEvent::Log { level, message } if level == "info" && message == "[echo] echoing")
            )
        );
        assert!(ev.iter().any(|e| matches!(e, McpEvent::Traffic { direction: Direction::Outgoing, json, .. } if json.contains("\"initialize\""))));
        assert!(ev.iter().any(|e| matches!(e, McpEvent::Traffic { direction: Direction::Outgoing, json, .. } if json.contains("notifications/initialized"))));
        assert!(ev.iter().any(
            |e| matches!(e, McpEvent::Traffic { direction: Direction::Incoming, json, .. } if json.contains("srv-ping"))
        ));
    }

    client.close().await;
    assert!(client.is_closed());
    wait_for(&events, "stdin-closed stderr", |e| matches!(e, McpEvent::Stderr(s) if s == "stdin closed, bye")).await;
    let closed: Vec<_> = events.lock().iter().filter(|e| matches!(e, McpEvent::Closed(_))).cloned().collect();
    assert_eq!(closed, vec![McpEvent::Closed("Closed by client".into())]);
    assert!(matches!(client.ping().await, Err(McpError::Closed(_))));
}

#[tokio::test]
async fn falls_back_to_older_protocol_version() {
    let (events, on_event) = recorder();
    let client = McpClient::connect(config(vec![("FAKE_MCP_ONLY_VERSION", "2024-11-05")]), on_event).await.unwrap();
    assert_eq!(client.server_info().protocol_version, "2024-11-05");
    let initializes = events
        .lock()
        .iter()
        .filter(|e| matches!(e, McpEvent::Traffic { direction: Direction::Outgoing, json, .. } if json.contains("\"initialize\"")))
        .count();
    assert_eq!(initializes, 3, "2025-06-18, 2025-03-26, then 2024-11-05");
    assert_eq!(client.list_tools().await.unwrap().len(), 13);
    client.close().await;
}

#[tokio::test]
async fn reports_exit_and_stderr_when_server_dies() {
    let (events, on_event) = recorder();
    let err = McpClient::connect(config(vec![("FAKE_MCP_EXIT_ON_START", "1")]), on_event).await.unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, McpError::Closed(_)), "{err:?}");
    assert!(msg.contains("exited with code 3"), "{msg}");
    assert!(msg.contains("fatal: boom"), "{msg}");
    assert!(events.lock().iter().any(|e| matches!(e, McpEvent::Closed(_))));
}

#[tokio::test]
async fn missing_program_is_a_spawn_error() {
    let cfg = McpConfig {
        transport: Transport::Stdio {
            command: "merpati-no-such-mcp-server".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        },
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let err = McpClient::connect(cfg, |_| {}).await.unwrap_err();
    assert!(matches!(err, McpError::Spawn(_)), "{err:?}");
    assert!(err.to_string().contains("merpati-no-such-mcp-server"));
}

#[tokio::test]
async fn timeout_then_session_keeps_working() {
    let (events, on_event) = recorder();
    let mut cfg = config(vec![]);
    cfg.timeout = Duration::from_millis(400);
    let client = McpClient::connect(cfg, on_event).await.unwrap();
    let err = client.call_tool("slow", json!({})).await.unwrap_err();
    assert!(matches!(err, McpError::Timeout { ref method, .. } if method == "tools/call"), "{err:?}");
    assert!(
        events
            .lock()
            .iter()
            .any(|e| matches!(e, McpEvent::Traffic { json, .. } if json.contains("notifications/cancelled")))
    );
    // The late answer is ignored (with a warning); later calls work.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    client.ping().await.unwrap();
    assert!(
        events
            .lock()
            .iter()
            .any(|e| matches!(e, McpEvent::Log { message, .. } if message.contains("unknown request id")))
    );
    client.close().await;
}

#[tokio::test]
async fn passes_env_and_cwd() {
    let dir = std::env::temp_dir().canonicalize().unwrap();
    let cfg = McpConfig {
        transport: Transport::Stdio {
            command: SERVER.into(),
            args: vec![],
            env: vec![("FAKE_VAR".into(), "from-env".into())],
            cwd: Some(dir.clone()),
        },
        timeout: Duration::from_secs(10),
        ..Default::default()
    };
    let client = McpClient::connect(cfg, |_| {}).await.unwrap();
    let r = client.call_tool("env", json!({})).await.unwrap();
    assert_eq!(r.content[0], Content::Text { text: "from-env".into() });
    let Content::Text { text: cwd } = &r.content[1] else { panic!() };
    assert_eq!(std::path::Path::new(cwd).canonicalize().unwrap(), dir);
    client.close().await;
}

#[tokio::test]
async fn dropping_the_client_stops_the_server() {
    let (events, on_event) = recorder();
    let client = McpClient::connect(config(vec![]), on_event).await.unwrap();
    drop(client);
    wait_for(&events, "server exit", |e| matches!(e, McpEvent::Stderr(s) if s == "stdin closed, bye")).await;
    assert!(events.lock().iter().any(|e| matches!(e, McpEvent::Closed(r) if r == "Disconnected")));
}

/// The outgoing `notifications/cancelled` messages, as JSON.
fn cancellations(events: &Events) -> Vec<serde_json::Value> {
    events
        .lock()
        .iter()
        .filter_map(|e| match e {
            McpEvent::Traffic { direction: Direction::Outgoing, json, .. }
                if json.contains("notifications/cancelled") =>
            {
                serde_json::from_str(json).ok()
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn abandoning_a_call_cancels_it_on_the_server() {
    let (events, on_event) = recorder();
    let client = McpClient::connect(config(vec![]), on_event).await.unwrap();
    // The caller gives up long before the 1.5 s answer (e.g. a cancelled run).
    let call = client.call_tool("slow", json!({}));
    assert!(tokio::time::timeout(Duration::from_millis(300), call).await.is_err());
    let call_id = events
        .lock()
        .iter()
        .find_map(|e| match e {
            McpEvent::Traffic { direction: Direction::Outgoing, json, .. } if json.contains("\"tools/call\"") => {
                serde_json::from_str::<serde_json::Value>(json).ok().map(|v| v["id"].clone())
            }
            _ => None,
        })
        .unwrap();
    let sent = cancellations(&events);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["params"]["requestId"], call_id);
    // A call that completes normally sends no cancellation.
    client.ping().await.unwrap();
    assert_eq!(cancellations(&events).len(), 1);
    client.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn initialize_is_never_cancelled() {
    let (events, on_event) = recorder();
    let cfg = McpConfig {
        // Reads everything and never answers.
        transport: Transport::Stdio {
            command: "sh".into(),
            args: vec!["-c".into(), "cat > /dev/null".into()],
            env: vec![],
            cwd: None,
        },
        timeout: Duration::from_millis(300),
        ..Default::default()
    };
    let err = McpClient::connect(cfg, on_event).await.unwrap_err();
    assert!(matches!(err, McpError::Timeout { ref method, .. } if method == "initialize"), "{err:?}");
    // MCP: "The initialize request MUST NOT be cancelled by clients".
    assert!(cancellations(&events).is_empty(), "{:?}", cancellations(&events));
}

#[cfg(unix)]
#[tokio::test]
async fn a_dead_server_takes_its_process_group_along() {
    let alive = |pid: &str| std::process::Command::new("kill").args(["-0", pid]).status().unwrap().success();
    let cfg = McpConfig {
        // Leaves a background process (like `npx` dying before its `node`).
        transport: Transport::Stdio {
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 30 & echo \"bg $!\" >&2; sleep 0.3; exit 3".into()],
            env: vec![],
            cwd: None,
        },
        timeout: Duration::from_secs(10),
        ..Default::default()
    };
    let err = McpClient::connect(cfg, |_| {}).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("exited with code 3"), "{msg}");
    let pid = msg.split("bg ").nth(1).and_then(|s| s.split_whitespace().next()).unwrap().to_string();
    for _ in 0..50 {
        if !alive(&pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = std::process::Command::new("kill").args(["-9", &pid]).status();
    panic!("background process {pid} outlived its server");
}

#[tokio::test]
async fn an_endless_line_closes_the_session_instead_of_filling_memory() {
    let (events, on_event) = recorder();
    let client = McpClient::connect(config(vec![]), on_event).await.unwrap();
    let err = tokio::time::timeout(Duration::from_secs(30), client.call_tool("flood", json!({})))
        .await
        .expect("the call ends")
        .unwrap_err();
    assert!(matches!(&err, McpError::Closed(m) if m.contains("larger than 64 MiB")), "{err:?}");
    assert!(client.is_closed());
    wait_for(&events, "closed", |e| matches!(e, McpEvent::Closed(m) if m.contains("larger than 64 MiB"))).await;
}
