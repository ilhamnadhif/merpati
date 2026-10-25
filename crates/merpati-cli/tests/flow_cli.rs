//! End-to-end tests of `merpati flow` on a temporary workspace against a
//! local server with an OpenAI-compatible endpoint.

use std::path::{Path, PathBuf};
use std::process::Output;

use axum::Router;
use axum::extract::Path as UrlPath;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};

use merpati_core::environment::Environment;
use merpati_core::model::{Collection, HttpRequest, Item, RequestItem};
use merpati_core::protocols::AiProvider;
use merpati_core::secrets::SecretStore;
use merpati_core::storage::WorkspaceStore;
use merpati_flows::{
    AiBlockConfig, Block, BlockConfig, Edge, Flow, FlowInput, ItemRef, JoinConfig, OutputConfig, PORT_IN,
    ParallelConfig, RequestConfig, RequestSource, TemplateConfig, storage,
};

const BIN: &str = env!("CARGO_BIN_EXE_merpati");

async fn server() -> String {
    let app = Router::new()
        .route(
            "/login",
            post(|body: String| async move {
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                if v["password"] == "secret" {
                    axum::Json(json!({"token": format!("tok-{}", v["username"].as_str().unwrap_or(""))}))
                        .into_response()
                } else {
                    (StatusCode::UNAUTHORIZED, "bad credentials").into_response()
                }
            }),
        )
        .route(
            "/items",
            get(|h: HeaderMap| async move {
                if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer tok-ada") {
                    return (StatusCode::UNAUTHORIZED, "no token").into_response();
                }
                axum::Json(json!({"items": [
                    {"id": 1, "active": true}, {"id": 2, "active": false}, {"id": 3, "active": true}
                ]}))
                .into_response()
            }),
        )
        .route(
            "/sleep/{ms}",
            get(|UrlPath(ms): UrlPath<u64>| async move {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                axum::Json(json!({"slept": ms}))
            }),
        )
        .route(
            "/items/{id}",
            get(|UrlPath(id): UrlPath<u32>| async move { axum::Json(json!({"id": id, "detail": format!("detail-{id}")})) }),
        )
        .route(
            "/v1/chat/completions",
            post(|body: String| async move {
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let n = v["messages"].as_array().map(|a| a.len()).unwrap_or(0);
                axum::Json(json!({
                    "model": v["model"],
                    "choices": [{"message": {"role": "assistant", "content": format!("{n} messages summarized")}, "finish_reason": "stop"}]
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn merpati(args: &[&str], cwd: &Path) -> Output {
    tokio::process::Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("MERPATI_NO_KEYCHAIN", "1")
        .env("NO_COLOR", "1")
        .output()
        .await
        .expect("run merpati")
}

fn text(o: &Output) -> String {
    format!("{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/flows/login-list-summarize.flow.json")
}

fn edge(from: &str, port: &str, to: &str) -> Edge {
    Edge::new(from, port, to, PORT_IN)
}

fn output(id: &str, name: &str, value: &str) -> Block {
    Block::with_id(
        id,
        id,
        BlockConfig::Output(OutputConfig { name: name.into(), value: value.into(), ..Default::default() }),
    )
}

/// Workspace with a "Local" environment, a collection with one saved request
/// and three flows (the example, one using the saved request, one invalid).
fn workspace(dir: &Path, base: &str) -> (PathBuf, String) {
    let root = dir.join("ws");
    let secrets = SecretStore::memory();
    let (store, _) = WorkspaceStore::create(&root, "Flows WS").unwrap();
    let env = Environment::new("Local")
        .with_var("baseUrl", base)
        .with_var("aiBaseUrl", &format!("{base}/v1"))
        .with_var("aiModel", "mock-model");
    store.save_environment(&env, &secrets).unwrap();
    let mut c = Collection::new("Shop");
    let item = RequestItem::new("Get item", HttpRequest::new("GET", "{{baseUrl}}/items/{{itemId}}"));
    let item_id = item.id.clone();
    c.items.push(Item::Request(item));
    store.save_collection(&c, &secrets).unwrap();

    let flows = root.join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    std::fs::copy(example(), flows.join("login-list-summarize.flow.json")).unwrap();

    let mut saved = Flow::new("Saved item");
    saved.inputs = vec![FlowInput { name: "label".into(), required: true, ..Default::default() }];
    saved.blocks.push(Block::with_id(
        "get",
        "Get",
        BlockConfig::Request(RequestConfig {
            source: RequestSource::Saved(ItemRef::new(&c.id, &item_id)),
            ..Default::default()
        }),
    ));
    saved.blocks.push(output("detail", "detail", "{{blocks.get.output.json.detail}}"));
    saved.blocks.push(output("label", "label", "{{inputs.label}}"));
    let start = saved.blocks[0].id.clone();
    saved.edges = vec![edge(&start, "next", "get"), edge("get", "success", "detail"), edge("detail", "next", "label")];
    storage::save(&root, &saved).unwrap();

    let mut bad = Flow::new("Broken");
    let start = bad.blocks[0].id.clone();
    for id in ["a", "b"] {
        bad.blocks.push(Block::with_id(
            id,
            id,
            BlockConfig::Template(TemplateConfig { template: id.into(), ..Default::default() }),
        ));
    }
    bad.edges = vec![edge(&start, "next", "a"), edge("a", "success", "b"), edge("b", "success", "a")];
    storage::save(&root, &bad).unwrap();
    (root, c.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn flow_list_run_and_validate() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let (root, _) = workspace(dir.path(), &base);
    let ws = root.to_str().unwrap();

    // list
    let o = merpati(&["flow", "list", ws], dir.path()).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let t = text(&o);
    for name in ["Login, list and summarize", "Saved item", "Broken"] {
        assert!(t.contains(name), "{t}");
    }
    let o = merpati(&["flow", "list", ws, "--json"], dir.path()).await;
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 3);

    // run the example with both reporters
    let report = dir.path().join("report.json");
    let o = merpati(
        &[
            "flow",
            "run",
            ws,
            "Login, list and summarize",
            "-e",
            "Local",
            "-r",
            "cli,json",
            "--reporter-json-export",
            report.to_str().unwrap(),
        ],
        dir.path(),
    )
    .await;
    let t = text(&o);
    assert_eq!(o.status.code(), Some(0), "{t}");
    assert!(t.contains("Flow passed"), "{t}");
    assert!(t.contains("✓ Log in  POST"), "{t}");
    assert!(t.contains("item 3/3"), "{t}");
    assert!(t.contains("summary: 2 messages summarized"), "{t}");
    let r: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(r["run"]["success"], true);
    assert_eq!(r["run"]["outputs"]["active_items"], 2);
    assert_eq!(r["run"]["requests"], 4);
    assert_eq!(r["flow"]["name"], "Login, list and summarize");

    // a failing run exits 1
    let o =
        merpati(&["flow", "run", ws, "login-list-summarize", "-e", "Local", "--input", "password=wrong"], dir.path())
            .await;
    let t = text(&o);
    assert_eq!(o.status.code(), Some(1), "{t}");
    assert!(t.contains("Flow failed"), "{t}");
    assert!(t.contains("Login failed: HTTP 401"), "{t}");

    // saved request, --var, required input, flow given by path
    let o = merpati(&["flow", "run", ws, "Saved item", "-e", "Local", "--var", "itemId=3"], dir.path()).await;
    assert_eq!(o.status.code(), Some(2), "missing required input: {}", text(&o));
    assert!(text(&o).contains("input \"label\" is required"), "{}", text(&o));
    let path = root.join("flows/saved-item.flow.json");
    let report = dir.path().join("saved.json");
    let o = merpati(
        &[
            "flow",
            "run",
            ws,
            path.to_str().unwrap(),
            "-e",
            "Local",
            "--var",
            "itemId=3",
            "--input",
            "label=three",
            "-r",
            "json",
            "--reporter-json-export",
            report.to_str().unwrap(),
        ],
        dir.path(),
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(!String::from_utf8_lossy(&o.stdout).contains("Flow passed"), "json only: no CLI output");
    let r: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(r["run"]["outputs"], json!({"detail": "detail-3", "label": "three"}));

    // invalid flows and unknown flows are load errors (2)
    let o = merpati(&["flow", "run", ws, "Broken"], dir.path()).await;
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("cycle"), "{}", text(&o));
    let o = merpati(&["flow", "run", ws, "nope"], dir.path()).await;
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("not found"), "{}", text(&o));
    let o = merpati(&["flow", "run", dir.path().join("missing").to_str().unwrap(), "x"], dir.path()).await;
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    let o = merpati(&["flow", "run", ws, "Saved item", "-r", "teapot"], dir.path()).await;
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));

    // validate
    let o = merpati(&["flow", "validate", ws, "Broken"], dir.path()).await;
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    let o = merpati(&["flow", "validate", ws, "Login, list and summarize"], dir.path()).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(text(&o).contains("OK"), "{}", text(&o));
}

#[tokio::test(flavor = "multi_thread")]
async fn flow_run_parallel_branches() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let secrets = SecretStore::memory();
    let (store, _) = WorkspaceStore::create(&root, "Parallel WS").unwrap();
    let env = Environment::new("Local")
        .with_var("baseUrl", &base)
        .with_var("aiBaseUrl", &format!("{base}/v1"))
        .with_var("aiModel", "mock-model");
    store.save_environment(&env, &secrets).unwrap();

    // Two 300 ms requests and a streaming AI block at the same time.
    let mut f = Flow::new("Parallel");
    let start = f.blocks[0].id.clone();
    let sleep = |id: &str, label: &str, y: f32| {
        Block::with_id(
            id,
            label,
            BlockConfig::Request(RequestConfig {
                source: RequestSource::inline(HttpRequest::new("GET", "{{baseUrl}}/sleep/300")),
                ..Default::default()
            }),
        )
        .at(300.0, y)
    };
    f.blocks.extend([
        Block::with_id("fan", "Fan out", BlockConfig::Parallel(ParallelConfig {})),
        sleep("a", "A", 0.0),
        sleep("b", "B", 100.0),
        Block::with_id(
            "ask",
            "Ask",
            BlockConfig::Ai(AiBlockConfig {
                provider: Some(AiProvider::OpenAiCompatible),
                base_url: "{{aiBaseUrl}}".into(),
                model: "{{aiModel}}".into(),
                prompt: "Hello".into(),
                stream: true,
                ..Default::default()
            }),
        )
        .at(300.0, 200.0),
        Block::with_id("join", "Join", BlockConfig::Join(JoinConfig {})),
        output("count", "count", "{{blocks.join.output.count}}"),
        output("slept", "slept", "{{blocks.join.output.results.A.json.slept}}"),
    ]);
    f.edges = vec![
        edge(&start, "next", "fan"),
        edge("fan", "branches", "a"),
        edge("fan", "branches", "b"),
        edge("fan", "branches", "ask"),
        edge("a", "success", "join"),
        edge("b", "success", "join"),
        edge("ask", "success", "join"),
        edge("join", "next", "count"),
        edge("count", "next", "slept"),
    ];
    storage::save(&root, &f).unwrap();
    let ws = root.to_str().unwrap();

    let o = merpati(&["flow", "validate", ws, "Parallel"], dir.path()).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(text(&o).contains("0 warnings"), "{}", text(&o));

    let report = dir.path().join("parallel.json");
    let o = merpati(
        &[
            "flow",
            "run",
            ws,
            "Parallel",
            "-e",
            "Local",
            "--verbose",
            "-r",
            "cli,json",
            "--reporter-json-export",
            report.to_str().unwrap(),
        ],
        dir.path(),
    )
    .await;
    let t = text(&o);
    assert_eq!(o.status.code(), Some(0), "{t}");
    assert!(t.contains("✓ Fan out  3 branches"), "{t}");
    assert!(t.contains("✓ A [1]  GET"), "{t}");
    assert!(t.contains("✓ Join  3/3 branches"), "{t}");
    // --verbose shows the streamed answer.
    assert!(t.contains("│ 1 messages summarized"), "{t}");
    assert!(t.contains("count: 3") && t.contains("slept: 300"), "{t}");
    let r: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(r["run"]["success"], true);
    // The two 300 ms requests overlapped.
    let steps = r["run"]["steps"].as_array().unwrap();
    let span = |id: &str| {
        let s = steps.iter().find(|s| s["block"] == id).unwrap();
        let start = s["started_ms"].as_f64().unwrap();
        (start, start + s["duration_ms"].as_f64().unwrap())
    };
    let (a, b) = (span("a"), span("b"));
    assert!(a.0 < b.1 && b.0 < a.1, "{a:?} {b:?}");
    assert!(a.1 - a.0 >= 299.0 && b.1 - b.0 >= 299.0, "{a:?} {b:?}");
    let branches: Vec<&str> = steps.iter().filter_map(|s| s["branch"].as_str()).collect();
    assert_eq!(branches.len(), 3);
    for b in ["1", "2", "3"] {
        assert!(branches.contains(&b), "{branches:?}");
    }

    // Without --verbose the streamed text is not printed.
    let o = merpati(&["flow", "run", ws, "Parallel", "-e", "Local"], dir.path()).await;
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(!text(&o).contains("│ 1 messages summarized"), "{}", text(&o));
}
