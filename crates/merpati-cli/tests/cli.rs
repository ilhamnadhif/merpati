//! End-to-end tests of the `merpati` binary with the real boa script engine
//! against a local server (doubles as the Gate C pipeline check).

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_merpati");

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli.postman_collection.json")
}

async fn server() -> String {
    let app = Router::new()
        .route(
            "/v1/login",
            post(|headers: HeaderMap, body: String| async move {
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let user = v["user"].as_str().unwrap_or("").to_string();
                let client = headers.get("x-client").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
                axum::Json(json!({"token": format!("tok-{user}"), "client": client}))
            }),
        )
        .route(
            "/v1/me",
            get(|headers: HeaderMap| async move {
                let auth = headers.get("authorization").and_then(|h| h.to_str().ok()).unwrap_or("");
                let client = headers.get("x-client").and_then(|h| h.to_str().ok()).unwrap_or("");
                match auth.strip_prefix("Bearer tok-") {
                    Some(user) => axum::Json(json!({"user": user, "client": client})).into_response(),
                    None => (StatusCode::UNAUTHORIZED, "no token").into_response(),
                }
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

fn write_env(dir: &Path, base: &str) -> PathBuf {
    let p = dir.join("local.postman_environment.json");
    let env = json!({
        "id": "env-1",
        "name": "Local",
        "values": [{"key": "baseUrl", "value": base, "type": "default", "enabled": true}],
        "_postman_variable_scope": "environment"
    });
    std::fs::write(&p, env.to_string()).unwrap();
    p
}

#[tokio::test(flavor = "multi_thread")]
async fn run_passes_with_data_junit_and_env_export() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let env = write_env(dir.path(), &base);
    std::fs::write(dir.path().join("users.csv"), "user\nada\nbob\n").unwrap();
    let fx = fixture();
    let out = merpati(
        &[
            "run",
            fx.to_str().unwrap(),
            "-e",
            env.to_str().unwrap(),
            "-d",
            "users.csv",
            "--folder",
            "Auth",
            "-r",
            "cli,junit,json,html",
            "--reporter-junit-export",
            "out/junit.xml",
            "--reporter-json-export",
            "out/report.json",
            "--reporter-html-export",
            "out/report.html",
            "--export-environment",
            "out/final-env.json",
        ],
        dir.path(),
    )
    .await;
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(t.contains("✓ status 200"), "{t}");
    assert!(t.contains("✓ profile of current user"), "{t}");
    assert!(t.contains("logged in as ada") && t.contains("logged in as bob"), "{t}");
    assert!(t.contains("Iteration 2/2"), "{t}");
    assert!(!t.contains("Missing page"), "only the Auth folder runs");

    let junit = std::fs::read_to_string(dir.path().join("out/junit.xml")).unwrap();
    assert_eq!(junit.matches("<testcase").count(), 8, "{junit}");
    assert!(!junit.contains("<failure"), "{junit}");
    assert!(junit.contains("Auth / Login"));

    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("out/report.json")).unwrap()).unwrap();
    assert_eq!(report["summary"]["tests_passed"], 8);
    assert_eq!(report["summary"]["iterations"], 2);
    let html = std::fs::read_to_string(dir.path().join("out/report.html")).unwrap();
    assert!(html.contains("CLI Fixture"));
    // Credentials are masked in reports (--show-secrets keeps them).
    let json_text = report.to_string();
    assert!(json_text.contains("[\"authorization\",\"***\"]"), "{json_text}");
    assert!(!json_text.contains("Bearer tok-") && !html.contains("Bearer tok-"));

    let env: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("out/final-env.json")).unwrap()).unwrap();
    let values = env["values"].as_array().unwrap();
    let token = values.iter().find(|v| v["key"] == "token").expect("token exported");
    assert_eq!(token["value"], "tok-bob");
    assert_eq!(env["_postman_variable_scope"], "environment");
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_test_exits_1_and_reports_failure() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let fx = fixture();
    let base_var = format!("baseUrl={base}");
    let out = merpati(
        &[
            "run",
            fx.to_str().unwrap(),
            "--env-var",
            &base_var,
            "--env-var",
            "user=cy",
            "-r",
            "cli,junit",
            "--reporter-junit-export",
            "junit.xml",
        ],
        dir.path(),
    )
    .await;
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(t.contains("Missing page"), "{t}");
    assert!(t.contains("404"), "{t}");
    let junit = std::fs::read_to_string(dir.path().join("junit.xml")).unwrap();
    assert!(junit.contains("<failure"), "{junit}");
    assert!(junit.contains("page exists"), "{junit}");
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_and_load_errors_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let out = merpati(&["run", "does-not-exist.json"], dir.path()).await;
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let out = merpati(&["run"], dir.path()).await;
    assert_eq!(out.status.code(), Some(2));
    let fx = fixture();
    let out = merpati(&["run", fx.to_str().unwrap(), "-r", "nope"], dir.path()).await;
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("unknown reporter"));
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_import_run_export_validate_codegen() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let fx = fixture();
    let out = merpati(&["init", ws.to_str().unwrap(), "--name", "Team"], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let out = merpati(&["import", fx.to_str().unwrap(), "-w", ws.to_str().unwrap()], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("Imported collection \"CLI Fixture\" (3 requests)"), "{}", text(&out));
    let env = write_env(dir.path(), &base);
    let out = merpati(&["import", env.to_str().unwrap(), "-w", ws.to_str().unwrap()], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));

    // Workspace environment selected by name; folder selected by name.
    let out =
        merpati(&["run", ws.to_str().unwrap(), "-e", "local", "--folder", "Auth", "--env-var", "user=dee"], dir.path())
            .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    // Running a collection folder inside the workspace works too.
    let col_dir = ws.join("collections/cli-fixture");
    let out = merpati(
        &["run", col_dir.to_str().unwrap(), "-e", "Local", "--folder", "Auth", "--env-var", "user=dee", "--silent"],
        dir.path(),
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty(), "--silent prints nothing");

    let out =
        merpati(&["export", "-w", ws.to_str().unwrap(), "-c", "CLI Fixture", "-o", "exported.json"], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let exported: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("exported.json")).unwrap()).unwrap();
    assert_eq!(exported["info"]["name"], "CLI Fixture");
    let out =
        merpati(&["export", "-w", ws.to_str().unwrap(), "-c", "CLI Fixture", "--format", "openapi"], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let oas: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(oas.get("openapi").is_some(), "{oas}");
    let out = merpati(&["export", "-w", ws.to_str().unwrap(), "-e", "Local"], dir.path()).await;
    assert!(String::from_utf8_lossy(&out.stdout).contains(&base));

    let out = merpati(&["validate", ws.to_str().unwrap()], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("{{user}}"), "undefined variable is reported: {}", text(&out));
    let out = merpati(&["validate", ws.to_str().unwrap(), "--strict"], dir.path()).await;
    assert_eq!(out.status.code(), Some(1));
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{\"info\": {\"name\": \"x\"}, \"item\": 5}").unwrap();
    let out = merpati(&["validate", bad.to_str().unwrap()], dir.path()).await;
    assert_ne!(out.status.code(), Some(0));

    let out = merpati(
        &[
            "codegen",
            ws.to_str().unwrap(),
            "--request",
            "Auth/Profile",
            "--lang",
            "curl",
            "-e",
            "Local",
            "--env-var",
            "token=tok-x",
        ],
        dir.path(),
    )
    .await;
    let snippet = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(snippet.contains(&format!("{base}/v1/me")), "{snippet}");
    assert!(snippet.contains("Bearer tok-x"), "auth header is included: {snippet}");
    let out = merpati(&["codegen", ws.to_str().unwrap(), "--lang", "list"], dir.path()).await;
    assert!(String::from_utf8_lossy(&out.stdout).contains("python"));

    let out = merpati(&["convert", fx.to_str().unwrap(), "--to", "openapi", "-o", "api.json"], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(dir.path().join("api.json").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn mock_server_serves_examples() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let fx = fixture();
    let mut child = tokio::process::Command::new(BIN)
        .args(["mock", fx.to_str().unwrap(), "--port", "0"])
        .env("MERPATI_NO_KEYCHAIN", "1")
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut url = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while url.is_none() {
        let line = tokio::time::timeout_at(deadline, lines.next_line()).await.expect("mock start").unwrap().unwrap();
        if let Some(i) = line.find("http://") {
            url = Some(line[i..].trim().to_string());
        }
    }
    let url = url.unwrap();
    let body = tokio::task::spawn_blocking(move || {
        let stream = std::net::TcpStream::connect(url.trim_start_matches("http://")).unwrap();
        use std::io::{Read, Write};
        let mut s = stream;
        s.write_all(b"POST /v1/login HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).unwrap();
        buf
    })
    .await
    .unwrap();
    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
    assert!(body.contains("example-token"), "{body}");
    // The request shows up in the streamed log.
    let mut seen = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(Some(line))) = tokio::time::timeout_at(deadline, lines.next_line()).await {
        if line.contains("POST /v1/login") {
            seen = true;
            break;
        }
    }
    assert!(seen);
    child.kill().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn perf_and_schedule() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let env = write_env(dir.path(), &base);
    let fx = fixture();
    let out = merpati(
        &[
            "perf",
            fx.to_str().unwrap(),
            "--request",
            "Auth/Login",
            "--vus",
            "2",
            "--duration",
            "1s",
            "-e",
            env.to_str().unwrap(),
            "--json",
            "perf.json",
        ],
        dir.path(),
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("throughput"), "{}", text(&out));
    let report: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("perf.json")).unwrap()).unwrap();
    assert!(report["total"].as_u64().unwrap() > 0);
    assert_eq!(report["errors"], 0);

    let out = merpati(
        &[
            "schedule",
            "--every",
            "1s",
            "--max-runs",
            "2",
            "--runs-dir",
            "runs",
            fx.to_str().unwrap(),
            "-e",
            env.to_str().unwrap(),
            "--folder",
            "Auth",
            "--env-var",
            "user=eve",
        ],
        dir.path(),
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("run #1: PASS") && t.contains("run #2: PASS"), "{t}");
    let files: Vec<_> = std::fs::read_dir(dir.path().join("runs")).unwrap().flatten().collect();
    assert_eq!(files.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn newman_folder_selects_a_request_and_zero_timeouts_mean_none() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let env = write_env(dir.path(), &base);
    let fx = fixture();
    // newman: `--folder` also names a single request; `--timeout-*` 0 = no limit.
    let out = merpati(
        &[
            "run",
            fx.to_str().unwrap(),
            "-e",
            env.to_str().unwrap(),
            "--env-var",
            "user=ann",
            "--folder",
            "Login",
            "--timeout-script",
            "0",
            "--timeout-request",
            "0",
            "-r",
            "cli,json",
            "--reporter-json-export",
            "report.json",
        ],
        dir.path(),
    )
    .await;
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(report["summary"]["executed"], 1, "{t}");
    assert_eq!(report["summary"]["tests_passed"], 2, "{t}");
    assert_eq!(report["summary"]["script_errors"], 0, "{t}");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_timeout_stops_the_run() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let env = write_env(dir.path(), &base);
    let fx = fixture();
    let started = std::time::Instant::now();
    let out = merpati(
        &[
            "run",
            fx.to_str().unwrap(),
            "-e",
            env.to_str().unwrap(),
            "--env-var",
            "user=ann",
            "--delay-request",
            "600000",
            "--timeout",
            "500",
        ],
        dir.path(),
    )
    .await;
    let t = text(&out);
    assert_eq!(out.status.code(), Some(1), "{t}");
    assert!(t.contains("timed out"), "{t}");
    // Far below the 10 minute delay (generous for loaded CI machines).
    assert!(started.elapsed() < Duration::from_secs(120), "{t}");
}

#[tokio::test(flavor = "multi_thread")]
async fn odd_durations_are_usage_errors_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    let fx = fixture();
    for args in [
        vec!["schedule", "--every", "0", "--max-runs", "1", fx.to_str().unwrap()],
        vec!["schedule", "--every", "0s", "--max-runs", "1", fx.to_str().unwrap()],
        vec!["perf", fx.to_str().unwrap(), "--duration", "inf"],
        vec!["perf", fx.to_str().unwrap(), "--duration", "99999999999999999999h"],
    ] {
        let out = merpati(&args, dir.path()).await;
        let t = text(&out);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {t}");
        assert!(!t.contains("panicked"), "{args:?}: {t}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn convert_rejects_an_unknown_collection_name() {
    let dir = tempfile::tempdir().unwrap();
    let fx = fixture();
    let out =
        merpati(&["convert", fx.to_str().unwrap(), "--to", "postman", "-c", "Nope", "-o", "x.json"], dir.path()).await;
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("\"Nope\" not found"), "{}", text(&out));
    assert!(!dir.path().join("x.json").exists());
    let out = merpati(&["convert", fx.to_str().unwrap(), "--to", "postman", "-c", "cli fixture"], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
}

#[tokio::test(flavor = "multi_thread")]
async fn convert_blanks_secrets_unless_asked() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.postman_collection.json");
    let collection = json!({
        "info": {"name": "Secrets", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
        "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "tok-literal-123", "type": "string"}]},
        "variable": [{"key": "apiKey", "value": "sk-secret-456", "type": "secret"}],
        "item": [{"name": "r", "request": {"method": "GET", "url": "http://x.test/"}}]
    });
    std::fs::write(&input, collection.to_string()).unwrap();
    let path = input.to_str().unwrap();
    let out = merpati(&["convert", path, "--to", "postman", "-o", "out.json"], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("--include-secrets"), "{}", text(&out));
    let written = std::fs::read_to_string(dir.path().join("out.json")).unwrap();
    assert!(!written.contains("tok-literal-123") && !written.contains("sk-secret-456"), "{written}");
    let out = merpati(&["convert", path, "--to", "postman", "--include-secrets", "-o", "all.json"], dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let written = std::fs::read_to_string(dir.path().join("all.json")).unwrap();
    assert!(written.contains("tok-literal-123") && written.contains("sk-secret-456"), "{written}");
}
