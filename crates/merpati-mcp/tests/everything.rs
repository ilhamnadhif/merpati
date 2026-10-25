//! Real-world checks against `@modelcontextprotocol/server-everything`
//! (needs Node and network). Ignored by default; run with
//! `cargo test -p merpati-mcp --test everything -- --ignored --nocapture`.
//! `MCP_EVERYTHING_PKG` overrides the package (e.g. a pinned version) and
//! `NPX` the npx binary.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use merpati_core::protocols::McpHttpTransport;
use merpati_mcp::{
    ElicitFieldKind, ElicitationRequest, ElicitationResponse, McpClient, McpConfig, McpEvent, Root, SamplingRequest,
    SamplingResult, Transport,
};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

fn package() -> String {
    std::env::var("MCP_EVERYTHING_PKG").unwrap_or_else(|_| "@modelcontextprotocol/server-everything".into())
}

fn npx() -> PathBuf {
    if let Ok(p) = std::env::var("NPX") {
        return PathBuf::from(p);
    }
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join("npx")).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("npx"))
}

/// A server process started in its own process group (npx spawns node).
struct NodeServer {
    child: tokio::process::Child,
}

impl Drop for NodeServer {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id().and_then(|p| i32::try_from(p).ok()) {
            // SAFETY: plain syscall on the group we created.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
        let _ = self.child.start_kill();
    }
}

async fn start_http(mode: &str) -> (NodeServer, u16) {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut cmd = tokio::process::Command::new(npx());
    cmd.args(["-y", &package(), mode])
        .env("PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn().expect("spawn npx");
    let server = NodeServer { child };
    let start = Instant::now();
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        assert!(start.elapsed() < Duration::from_secs(120), "{mode} server did not start");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    (server, port)
}

/// Form values for every field (defaults, else a plausible value).
fn fill(req: &ElicitationRequest) -> Map<String, Value> {
    let mut out = Map::new();
    for f in req.fields() {
        let v = match (&f.default, &f.kind) {
            (Some(d), _) => d.clone(),
            (None, ElicitFieldKind::Text { format, .. }) => json!(match format.as_deref() {
                Some("email") => "ann@example.com",
                Some("uri") => "https://example.com",
                Some("date") => "2000-01-31",
                Some("date-time") => "2000-01-31T12:00:00Z",
                _ => "Ann Example",
            }),
            (None, ElicitFieldKind::Number { minimum, .. }) => json!(minimum.unwrap_or(1.0).max(1.0) as i64),
            (None, ElicitFieldKind::Boolean) => json!(true),
            (None, ElicitFieldKind::Choice { values, .. }) => json!(values[0]),
            (None, ElicitFieldKind::MultiChoice { values, .. }) => json!([values[0]]),
            (None, ElicitFieldKind::Other(_)) => continue,
        };
        out.insert(f.name.clone(), v);
    }
    out
}

struct Seen {
    sampling: AtomicUsize,
    elicitation: AtomicUsize,
    problems: Mutex<Vec<String>>,
}

fn builder(cfg: McpConfig, seen: &Arc<Seen>) -> merpati_mcp::McpClientBuilder {
    let (s1, s2) = (seen.clone(), seen.clone());
    McpClient::builder(cfg)
        .sampling(move |req: SamplingRequest| {
            s1.sampling.fetch_add(1, Ordering::SeqCst);
            println!("  sampling request: {:?} (maxTokens {})", req.transcript(), req.max_tokens);
            async move { Ok(SamplingResult::text("merpati-test-model", "Hello from the Merpati sampling handler")) }
        })
        .elicitation(move |req: ElicitationRequest| {
            s2.elicitation.fetch_add(1, Ordering::SeqCst);
            let values = fill(&req);
            let problems = req.validate(&values);
            println!("  elicitation: {:?} with {} fields", req.message, req.fields().len());
            s2.problems.lock().extend(problems);
            async move { Ok(ElicitationResponse::Accept(values)) }
        })
        .roots(vec![Root::new("file:///tmp/merpati-root-one", Some("one".into()))])
        .on_event(|e| {
            if let McpEvent::Log { level, message } = &e
                && (level == "warning" || level == "error")
            {
                println!("  [{level}] {message}");
            }
        })
}

async fn exercise(client: &McpClient, seen: &Arc<Seen>) {
    let info = client.server_info();
    println!(
        "  server {} {} protocol {} via {}",
        info.name,
        info.version,
        info.protocol_version,
        client.transport_name()
    );

    let tools = client.list_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    println!("  {} tools: {}", names.len(), names.join(", "));
    for t in ["echo", "get-sum", "trigger-sampling-request", "trigger-elicitation-request", "get-roots-list"] {
        assert!(names.contains(&t), "missing tool {t}");
    }

    let r = client.call_tool("echo", json!({"message": "hello merpati"})).await.expect("echo");
    assert!(r.text().contains("hello merpati"), "{}", r.text());
    let r = client.call_tool("get-sum", json!({"a": 2, "b": 3})).await.expect("get-sum");
    assert!(r.text().contains('5'), "{}", r.text());
    println!("  echo / get-sum ok: {:?}", r.text());

    let resources = client.list_resources().await.expect("resources/list");
    assert!(!resources.is_empty());
    let read = client.read_resource(&resources[0].uri).await.expect("resources/read");
    assert!(read["contents"].as_array().is_some_and(|c| !c.is_empty()), "{read}");
    let templates = client.list_resource_templates().await.expect("templates");
    println!("  {} resources, {} templates; read {} ok", resources.len(), templates.len(), resources[0].uri);

    let prompts = client.list_prompts().await.expect("prompts/list");
    let pnames: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    println!("  prompts: {}", pnames.join(", "));
    let p = client.get_prompt("simple-prompt", Map::new()).await.expect("simple-prompt");
    assert!(p["messages"].as_array().is_some_and(|m| !m.is_empty()));
    let mut args = Map::new();
    args.insert("city".into(), json!("Jakarta"));
    let p = client.get_prompt("args-prompt", args).await.expect("args-prompt");
    assert!(p.to_string().contains("Jakarta"), "{p}");

    let r = client.call_tool("trigger-sampling-request", json!({"prompt": "Say hi", "maxTokens": 20})).await;
    let r = r.expect("trigger-sampling-request");
    assert!(!r.is_error, "{}", r.text());
    assert!(r.text().contains("merpati-test-model"), "{}", r.text());
    assert_eq!(seen.sampling.load(Ordering::SeqCst), 1);
    println!("  sampling round trip ok");

    let r = client.call_tool("trigger-elicitation-request", json!({})).await.expect("trigger-elicitation-request");
    assert!(!r.is_error, "{}", r.text());
    assert!(r.text().contains("Ann Example") || r.text().to_lowercase().contains("accept"), "{}", r.text());
    assert_eq!(seen.elicitation.load(Ordering::SeqCst), 1);
    assert!(seen.problems.lock().is_empty(), "{:?}", seen.problems.lock());
    println!("  elicitation round trip ok");

    let r = client.call_tool("get-roots-list", json!({})).await.expect("get-roots-list");
    assert!(r.text().contains("merpati-root-one"), "{}", r.text());
    client.set_roots(vec![Root::new("file:///tmp/merpati-root-two", Some("two".into()))]).unwrap();
    let mut updated = false;
    for _ in 0..50 {
        let r = client.call_tool("get-roots-list", json!({})).await.expect("get-roots-list");
        if r.text().contains("merpati-root-two") {
            updated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(updated, "the server re-read the roots after list_changed");
    println!("  roots + list_changed ok");
    client.ping().await.expect("ping");
}

fn seen() -> Arc<Seen> {
    Arc::new(Seen { sampling: AtomicUsize::new(0), elicitation: AtomicUsize::new(0), problems: Mutex::new(Vec::new()) })
}

#[tokio::test]
#[ignore = "needs Node and network"]
async fn everything_stdio() {
    let cfg = McpConfig {
        transport: Transport::Stdio {
            command: npx().display().to_string(),
            args: vec!["-y".into(), package(), "stdio".into()],
            env: vec![],
            cwd: None,
        },
        timeout: Duration::from_secs(120),
        ..Default::default()
    };
    println!("stdio:");
    let seen = seen();
    let client = builder(cfg, &seen).connect().await.expect("connect");
    assert_eq!(client.transport_name(), "stdio");
    exercise(&client, &seen).await;
    client.close().await;
}

fn http_config(url: String, mode: McpHttpTransport) -> McpConfig {
    McpConfig {
        transport: Transport::Http { url, headers: vec![] },
        timeout: Duration::from_secs(60),
        http_transport: mode,
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "needs Node and network"]
async fn everything_streamable_http() {
    let (_server, port) = start_http("streamableHttp").await;
    println!("streamableHttp:");
    let seen = seen();
    let cfg = http_config(format!("http://127.0.0.1:{port}/mcp"), McpHttpTransport::Auto);
    let client = builder(cfg, &seen).connect().await.expect("connect");
    assert_eq!(client.transport_name(), "streamable-http");
    exercise(&client, &seen).await;
    client.close().await;
}

#[tokio::test]
#[ignore = "needs Node and network"]
async fn everything_sse() {
    let (_server, port) = start_http("sse").await;
    println!("sse (explicit):");
    let seen1 = seen();
    let cfg = http_config(format!("http://127.0.0.1:{port}/sse"), McpHttpTransport::Sse);
    let client = builder(cfg, &seen1).connect().await.expect("connect");
    assert_eq!(client.transport_name(), "sse");
    exercise(&client, &seen1).await;
    client.close().await;

    println!("sse (auto fallback):");
    let seen2 = seen();
    let cfg = http_config(format!("http://127.0.0.1:{port}/sse"), McpHttpTransport::Auto);
    let client = builder(cfg, &seen2).connect().await.expect("connect");
    assert_eq!(client.transport_name(), "sse");
    exercise(&client, &seen2).await;
    client.close().await;
}

/// Finds `node` the way [`npx`] does.
fn node() -> PathBuf {
    let npx = npx();
    let sibling = npx.with_file_name("node");
    if sibling.is_file() {
        return sibling;
    }
    let dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.into_iter().map(|d| d.join("node")).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("node"))
}

/// `@modelcontextprotocol/sdk` as installed by npx for server-everything
/// (or `MCP_SDK_DIR`).
fn sdk_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("MCP_SDK_DIR") {
        return Some(PathBuf::from(d));
    }
    let cache = PathBuf::from(std::env::var_os("HOME")?).join(".npm/_npx");
    std::fs::read_dir(cache)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join("node_modules/@modelcontextprotocol/sdk"))
        .find(|p| p.join("dist/esm/examples/server/simpleStreamableHttp.js").is_file())
}

/// OAuth against the TypeScript SDK's reference implementation: its example
/// server with `--oauth --oauth-strict` (protected resource metadata, a
/// separate demo authorization server with dynamic registration that
/// approves every request, RFC 8707 resource checking).
#[tokio::test]
#[ignore = "needs Node and the MCP TypeScript SDK (installed by the server-everything tests)"]
async fn sdk_example_server_oauth() {
    let Some(sdk) = sdk_dir() else {
        println!("@modelcontextprotocol/sdk not found; run everything_stdio first or set MCP_SDK_DIR");
        return;
    };
    let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (mcp_port, auth_port) = (free(), free());
    let mut cmd = tokio::process::Command::new(node());
    cmd.arg(sdk.join("dist/esm/examples/server/simpleStreamableHttp.js"))
        .args(["--oauth", "--oauth-strict"])
        .env("MCP_PORT", mcp_port.to_string())
        .env("MCP_AUTH_PORT", auth_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let _server = NodeServer { child: cmd.spawn().expect("spawn node") };
    let start = Instant::now();
    for port in [mcp_port, auth_port] {
        while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
            assert!(start.elapsed() < Duration::from_secs(60), "example server did not start");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    merpati_http::tls::install_provider();
    let storage: Arc<dyn merpati_mcp::OAuthStorage> = Arc::new(merpati_mcp::MemoryOAuthStorage::default());
    let opened = Arc::new(AtomicUsize::new(0));
    let options = |opened: Arc<AtomicUsize>| {
        merpati_mcp::OAuthOptions::new(move |url: &str| {
            opened.fetch_add(1, Ordering::SeqCst);
            println!("  browser: {}", url.split('?').next().unwrap_or(url));
            let url = url.to_string();
            tokio::spawn(async move {
                // The demo authorization server approves at once and redirects to the loopback.
                let _ = reqwest::Client::new().get(url).send().await;
            });
        })
        .with_storage(storage.clone())
        .with_login_timeout(Duration::from_secs(30))
    };
    let url = format!("http://localhost:{mcp_port}/mcp");
    println!("sdk example server with OAuth at {url}:");
    let log = |e: McpEvent| {
        if let McpEvent::Log { level, message } = &e
            && (level == "auth" || level == "warning")
        {
            println!("  [{level}] {message}");
        }
    };
    let cfg = http_config(url.clone(), McpHttpTransport::Auto);
    let client =
        McpClient::builder(cfg.clone()).oauth(options(opened.clone())).on_event(log).connect().await.expect("connect");
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    let r = client.call_tool("greet", json!({"name": "Merpati"})).await.expect("greet");
    println!("  greet: {}", r.text());
    assert!(r.text().contains("Merpati"));
    let tokens = storage.load_tokens(&merpati_mcp::oauth_server_key(&url).unwrap()).expect("stored");
    println!("  token for resource {} from {} (client {})", tokens.resource, tokens.issuer, tokens.client_id);
    client.close().await;

    // Reconnect with the stored token: no browser.
    let client =
        McpClient::builder(cfg).oauth(options(opened.clone())).on_event(log).connect().await.expect("reconnect");
    assert_eq!(opened.load(Ordering::SeqCst), 1, "stored token reused");
    assert!(!client.list_tools().await.unwrap().is_empty());
    println!("  reconnect with stored token ok");
    client.close().await;
}
