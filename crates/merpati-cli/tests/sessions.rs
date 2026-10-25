//! End-to-end test of `merpati run` with a WebSocket request: the session
//! runs against a local echo server with its scripts, and the tests of the
//! on-message and post-response scripts end up in the JUnit report.

use std::path::Path;
use std::process::Output;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use merpati_core::environment::Environment;
use merpati_core::model::{Collection, Item, RequestItem, RequestKind, Scripts, WsRequest, WsSavedMessage};
use merpati_core::protocols::SessionRunner;
use merpati_core::secrets::SecretStore;
use merpati_core::storage::WorkspaceStore;

const BIN: &str = env!("CARGO_BIN_EXE_merpati");

/// Echoes every text message; `burst N` replies with N `tick` messages.
async fn echo_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else { return };
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let replies: Vec<String> = match t.as_str().strip_prefix("burst ") {
                        Some(n) => (0..n.parse::<usize>().unwrap_or(0)).map(|i| format!("tick {i}")).collect(),
                        None => vec![t.as_str().to_string()],
                    };
                    for r in replies {
                        if ws.send(Message::text(r)).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    url
}

async fn merpati(args: &[&str], cwd: &Path) -> Output {
    tokio::process::Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .output()
        .await
        .expect("run merpati")
}

#[tokio::test(flavor = "multi_thread")]
async fn run_executes_websocket_session_and_writes_junit() {
    let url = echo_server().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let secrets = SecretStore::memory();
    let (store, _) = WorkspaceStore::create(&root, "Sessions").unwrap();
    store.save_environment(&Environment::new("Local").with_var("wsUrl", &url), &secrets).unwrap();
    let ws = WsRequest {
        url: "{{wsUrl}}/chat".into(),
        messages: vec![
            WsSavedMessage { name: "hello".into(), body: "hello".into(), ..Default::default() },
            WsSavedMessage { name: "burst".into(), body: "burst 3".into(), ..Default::default() },
        ],
        runner: SessionRunner { wait_ms: 0, max_messages: Some(4), ..Default::default() },
        ..Default::default()
    };
    let mut item = RequestItem::new("Chat", merpati_core::model::HttpRequest::new("GET", ""));
    item.request = RequestKind::WebSocket(ws);
    item.scripts = Scripts {
        on_message:
            "pm.test('message ' + pm.message.text(), () => pm.expect(pm.message.direction).to.equal('incoming'));"
                .into(),
        post_response: "pm.test('four messages', () => pm.expect(pm.response.messages.count()).to.equal(4));\n\
                        pm.test('stopped by max', () => pm.expect(pm.response.stopReason).to.equal('max_messages'));"
            .into(),
        ..Default::default()
    };
    let mut c = Collection::new("Realtime");
    c.items.push(Item::Request(item));
    store.save_collection(&c, &secrets).unwrap();

    let junit = dir.path().join("junit.xml");
    let out = merpati(
        &[
            "run",
            root.to_str().unwrap(),
            "-e",
            "Local",
            "-r",
            "cli,junit",
            "--reporter-junit-export",
            junit.to_str().unwrap(),
            "--no-keychain",
            "--no-color",
        ],
        dir.path(),
    )
    .await;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("101 Switching Protocols · 2 sent, 4 received"), "{stdout}");
    assert!(stdout.contains("✓ message tick 2") && stdout.contains("✓ stopped by max"), "{stdout}");
    let xml = std::fs::read_to_string(&junit).unwrap();
    for name in ["message hello", "message tick 0", "message tick 2", "four messages", "stopped by max"] {
        assert!(xml.contains(&format!("name=\"{name}\"")), "{name} missing in {xml}");
    }
    assert!(xml.contains("Realtime.Chat") && !xml.contains("<failure"), "{xml}");
}
