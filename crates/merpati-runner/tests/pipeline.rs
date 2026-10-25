//! Pipeline and runner tests with a tiny line-based script engine, so they
//! exercise the flow (variables, scripts order, flow control, reporters)
//! independently of the JavaScript engine.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{any, get, post};
use indexmap::IndexMap;
use serde_json::Value;

use merpati_core::auth::Auth;
use merpati_core::model::{Collection, Folder, HttpRequest, Item, KeyValue, RequestItem};
use merpati_core::script::{
    ConsoleEntry, LogLevel, NextRequest, ScriptEngine, ScriptError, ScriptJob, ScriptOutcome, TestResult,
};
use merpati_core::settings::RequestDefaults;
use merpati_core::tokens::MemoryTokenStore;
use merpati_core::variables::{Variable, VariableScopes};
use merpati_http::{CancellationToken, CookieJar, EngineConfig, HttpEngine};
use merpati_runner::report::{HtmlReporter, JsonReporter, JunitReporter, Reporter};
use merpati_runner::runner::RunEvent;
use merpati_runner::{ExecuteInput, RunOptions, Services, execute, run_collection};

/// Script lines: `env.set k v`, `col.set k v`, `local.set k v`, `header.add k v`,
/// `capture k <json field>`, `test.status N`, `next NAME|null`, `skip`, `log MSG`, `throw MSG`.
struct DslEngine;

impl ScriptEngine for DslEngine {
    fn name(&self) -> &'static str {
        "dsl"
    }

    fn run(&self, job: ScriptJob) -> ScriptOutcome {
        let mut out =
            ScriptOutcome { request: job.request.clone(), variables: job.variables.clone(), ..Default::default() };
        for s in &job.scripts {
            for line in s.code.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let parts: Vec<&str> = line.splitn(3, ' ').collect();
                match parts.as_slice() {
                    ["env.set", k, v] => {
                        out.variables.environment.insert(k.to_string(), v.to_string());
                    }
                    ["col.set", k, v] => {
                        out.variables.collection.insert(k.to_string(), v.to_string());
                    }
                    ["local.set", k, v] => {
                        out.variables.local.insert(k.to_string(), v.to_string());
                    }
                    ["header.add", k, v] => out.request.headers.push(KeyValue::new(*k, *v)),
                    ["capture", k, field] => {
                        let body = job.response.as_ref().unwrap().body.read_all().unwrap();
                        let v: Value = serde_json::from_slice(&body).unwrap();
                        out.variables.environment.insert(k.to_string(), v[field].as_str().unwrap_or("").to_string());
                    }
                    ["test.status", n] => {
                        let code = job.response.as_ref().map(|r| r.code).unwrap_or(0);
                        let want: u16 = n.parse().unwrap();
                        out.tests.push(TestResult {
                            name: format!("status is {want}"),
                            passed: code == want,
                            error: (code != want).then(|| format!("expected {code} to equal {want}")),
                            skipped: false,
                        });
                    }
                    ["next", "null"] => out.next_request = Some(NextRequest::Stop),
                    ["next", rest @ ..] => out.next_request = Some(NextRequest::Named(rest.join(" "))),
                    ["skip"] => out.skip_request = true,
                    ["log", rest @ ..] => out.console.push(ConsoleEntry {
                        level: LogLevel::Log,
                        message: format!("{}: {}", s.location, rest.join(" ")),
                    }),
                    ["throw", rest @ ..] => {
                        out.errors.push(ScriptError { location: s.location.clone(), message: rest.join(" ") })
                    }
                    other => panic!("unknown dsl line {other:?}"),
                }
            }
        }
        out
    }
}

async fn server() -> String {
    let app = Router::new()
        .route("/login", post(|| async { axum::Json(serde_json::json!({"token": "t-123"})) }))
        .route(
            "/me",
            get(|h: HeaderMap| async move {
                match h.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some("Bearer t-123") => (StatusCode::OK, "me").into_response_parts(),
                    _ => (StatusCode::UNAUTHORIZED, "nope").into_response_parts(),
                }
            }),
        )
        .route(
            "/echo",
            any(|uri: axum::http::Uri, h: HeaderMap| async move {
                let hdrs: IndexMap<String, String> =
                    h.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
                axum::Json(serde_json::json!({"uri": uri.to_string(), "headers": hdrs}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

trait IntoParts {
    fn into_response_parts(self) -> axum::response::Response;
}

impl IntoParts for (StatusCode, &'static str) {
    fn into_response_parts(self) -> axum::response::Response {
        axum::response::IntoResponse::into_response(self)
    }
}

fn services() -> Services {
    Services {
        http: Arc::new(HttpEngine::new(EngineConfig::default(), Arc::new(CookieJar::new()))),
        scripts: Arc::new(DslEngine),
        tokens: Arc::new(MemoryTokenStore::default()),
        vault: None,
        defaults: RequestDefaults::default(),
        script_timeout: Duration::from_secs(5),
        runtime: tokio::runtime::Handle::current(),
        packages: None,
    }
}

fn req(name: &str, method: &str, url: &str) -> RequestItem {
    RequestItem::new(name, HttpRequest::new(method, url))
}

fn scopes(base: &str) -> VariableScopes {
    let mut v = VariableScopes::default();
    v.environment.insert("base".into(), base.into());
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn token_flow_with_inherited_auth() {
    let base = server().await;
    let mut c = Collection::new("API");
    c.auth = Auth::bearer("{{token}}");
    let mut login = req("Login", "POST", "{{base}}/login");
    login.request.as_http_mut().unwrap().auth = Auth::none();
    login.scripts.post_response = "capture token token\ntest.status 200".into();
    let mut me = req("Me", "GET", "{{base}}/me");
    me.scripts.post_response = "test.status 200".into();
    c.items.push(login.into());
    c.items.push(me.into());

    let mut vars = scopes(&base);
    let mut events = Vec::new();
    let summary = run_collection(
        &services(),
        &c,
        &mut vars,
        RunOptions::default(),
        &mut |e| events.push(e),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(summary.executed, 2);
    assert_eq!(summary.tests_passed, 2, "{summary:?}");
    assert_eq!(vars.environment["token"], "t-123");
    assert!(summary.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn script_order_folder_vars_and_request_mutation() {
    let base = server().await;
    let mut c = Collection::new("API");
    c.scripts.pre_request = "log pre".into();
    c.variables.push(Variable::new("who", "collection"));
    let mut f = Folder::new("Users");
    f.scripts.pre_request = "log pre".into();
    f.variables.push(Variable::new("who", "folder"));
    let mut r = req("Echo", "GET", "{{base}}/echo?who={{who}}&l={{l}}");
    r.scripts.pre_request = "log pre\nheader.add X-Added yes\nlocal.set l 1\ncol.set changed 1".into();
    f.items.push(r.into());
    c.items.push(f.into());
    let id = c.requests()[0].id.clone();

    let mut vars = scopes(&base);
    vars.collection = merpati_core::variables::to_map(&c.variables);
    let exec = execute(
        &services(),
        ExecuteInput {
            collection: &c,
            request_id: &id,
            request_override: None,
            iteration: 0,
            iteration_count: 1,
            base_dir: None,
        },
        &mut vars,
        &CancellationToken::new(),
    )
    .await;
    assert!(exec.error.is_none(), "{:?}", exec.error);
    let logs: Vec<&str> =
        exec.console.iter().filter(|l| l.phase == "pre-request").map(|l| l.message.as_str()).collect();
    assert_eq!(logs, vec!["collection \"API\": pre", "folder \"Users\": pre", "request \"Echo\": pre"]);
    let body: Value = serde_json::from_slice(&exec.response.unwrap().body.read_all().unwrap()).unwrap();
    assert_eq!(body["uri"], "/echo?who=folder&l=1", "folder var overrides collection var; local var resolves");
    assert_eq!(body["headers"]["x-added"], "yes");
    // Script writes go to the collection scope; folder values stay out of it.
    assert_eq!(vars.collection.get("changed").map(String::as_str), Some("1"));
    assert_eq!(vars.collection.get("who").map(String::as_str), Some("collection"));
}

#[tokio::test(flavor = "multi_thread")]
async fn set_next_request_skip_and_iterations() {
    let base = server().await;
    let mut c = Collection::new("Flow");
    let mut a = req("A", "GET", "{{base}}/echo?u={{user}}");
    a.scripts.post_response = "next C".into();
    let b = req("B", "GET", "{{base}}/echo");
    let mut cc = req("C", "GET", "{{base}}/echo");
    cc.scripts.pre_request = "skip".into();
    let mut d = req("D", "GET", "{{base}}/echo");
    d.scripts.post_response = "next null".into();
    let e = req("E", "GET", "{{base}}/echo");
    for r in [a, b, cc, d, e] {
        c.items.push(Item::Request(r));
    }
    let data: Vec<_> = ["ada", "bob"]
        .iter()
        .map(|u| {
            let mut row = IndexMap::new();
            row.insert("user".to_string(), Value::String(u.to_string()));
            row
        })
        .collect();
    let mut vars = scopes(&base);
    let mut order = Vec::new();
    let mut urls = Vec::new();
    let opts = RunOptions { data, ..Default::default() };
    let summary = run_collection(
        &services(),
        &c,
        &mut vars,
        opts,
        &mut |e| {
            if let RunEvent::RequestFinished(r) = e {
                order.push(format!("{}{}", r.name, r.iteration));
                if r.name == "A" {
                    urls.push(r.url.clone());
                }
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    // A jumps to C (skipped), D stops the iteration, so B and E never run.
    assert_eq!(order, vec!["A0", "C0", "D0", "A1", "C1", "D1"]);
    assert!(urls[0].ends_with("u=ada") && urls[1].ends_with("u=bob"), "{urls:?}");
    assert_eq!(summary.iterations, 2);
    assert_eq!(summary.skipped, 2);
    assert!(vars.data.is_empty());
}

/// postman-runtime looks the target up by id first, then by name, and with
/// duplicate names picks the last item carrying the name.
#[tokio::test(flavor = "multi_thread")]
async fn set_next_request_with_duplicate_names() {
    let base = server().await;
    let mut c = Collection::new("Dups");
    let mut a = req("A", "GET", "{{base}}/echo");
    a.scripts.post_response = "next Dup".into();
    let mut first = req("Dup", "GET", "{{base}}/echo?n=1");
    first.scripts.post_response = "next null".into();
    let b = req("B", "GET", "{{base}}/echo");
    let mut last = req("Dup", "GET", "{{base}}/echo?n=2");
    // A name equal to another item's id still resolves to that id.
    last.scripts.post_response = format!("next {}", first.id);
    let mut decoy = req(&first.id, "GET", "{{base}}/echo?n=decoy");
    decoy.scripts.post_response = "next null".into();
    for r in [a, first, b, last, decoy] {
        c.items.push(Item::Request(r));
    }
    let mut vars = scopes(&base);
    let mut order = Vec::new();
    run_collection(
        &services(),
        &c,
        &mut vars,
        RunOptions::default(),
        &mut |e| {
            if let RunEvent::RequestFinished(r) = e {
                order.push(r.url.rsplit('/').next().unwrap_or_default().to_string());
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(order, vec!["echo", "echo?n=2", "echo?n=1"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn reporters_and_failures() {
    let base = server().await;
    let mut c = Collection::new("Reports");
    let mut ok = req("Ok", "GET", "{{base}}/echo");
    ok.scripts.post_response = "test.status 200".into();
    let mut bad = req("Unauthorized", "GET", "{{base}}/me");
    bad.scripts.post_response = "test.status 200\nthrow boom".into();
    let down = req("Down", "GET", "http://127.0.0.1:9/");
    c.items.push(ok.into());
    c.items.push(bad.into());
    c.items.push(down.into());
    let dir = tempfile::tempdir().unwrap();
    let mut reporters: Vec<Box<dyn Reporter>> = vec![
        Box::new(JunitReporter::new(dir.path().join("out/junit.xml"), "Reports")),
        Box::new(JsonReporter::new(dir.path().join("out/report.json"))),
        Box::new(HtmlReporter::new(dir.path().join("out/report.html"))),
        Box::new(merpati_runner::report::CliReporter::new(Vec::new(), false)),
    ];
    let mut vars = scopes(&base);
    let summary = run_collection(
        &services(),
        &c,
        &mut vars,
        RunOptions::default(),
        &mut |e| {
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
    assert_eq!(summary.tests_passed, 1);
    assert_eq!(summary.tests_failed, 1);
    assert_eq!(summary.script_errors, 1);
    assert_eq!(summary.requests_failed, 1);
    assert!(!summary.success());
    let junit = std::fs::read_to_string(dir.path().join("out/junit.xml")).unwrap();
    assert!(junit.contains("expected 401 to equal 200"), "{junit}");
    assert!(junit.contains("<testsuite name=\"Unauthorized\""));
    assert!(junit.contains("RequestError"));
    let json: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("out/report.json")).unwrap()).unwrap();
    assert_eq!(json["results"].as_array().unwrap().len(), 3);
    let html = std::fs::read_to_string(dir.path().join("out/report.html")).unwrap();
    assert!(html.contains("Connection refused"));
}

#[tokio::test(flavor = "multi_thread")]
async fn bail_and_cancel() {
    let base = server().await;
    let mut c = Collection::new("Bail");
    let mut a = req("A", "GET", "{{base}}/me");
    a.scripts.post_response = "test.status 200".into();
    c.items.push(a.into());
    c.items.push(req("B", "GET", "{{base}}/echo").into());
    let mut vars = scopes(&base);
    let opts = RunOptions { bail: true, ..Default::default() };
    let s = run_collection(&services(), &c, &mut vars, opts, &mut |_| {}, &CancellationToken::new()).await.unwrap();
    assert!(s.bailed);
    assert_eq!(s.executed, 1);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let s = run_collection(&services(), &c, &mut vars, RunOptions::default(), &mut |_| {}, &cancel).await.unwrap();
    assert!(s.cancelled);
    assert_eq!(s.executed, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn thousand_iterations_keep_flat_state() {
    let base = server().await;
    let mut c = Collection::new("Load");
    let mut r = req("Echo", "GET", "{{base}}/echo");
    r.scripts.post_response = "test.status 200".into();
    c.items.push(r.into());
    let mut vars = scopes(&base);
    let mut finished = 0usize;
    let opts = RunOptions { iterations: Some(1000), ..Default::default() };
    let s = run_collection(
        &services(),
        &c,
        &mut vars,
        opts,
        &mut |e| {
            if matches!(e, RunEvent::RequestFinished(_)) {
                finished += 1;
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(finished, 1000);
    assert_eq!(s.tests_passed, 1000);
    assert!(s.response_time_avg_ms() > 0.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn simulated_failures() {
    let base = server().await;
    let mut c = Collection::new("Chaos");
    c.items.push(req("Echo", "GET", "{{base}}/echo").into());
    let mut vars = scopes(&base);
    let opts = RunOptions { iterations: Some(20), simulate_failure_rate: 1.0, ..Default::default() };
    let s = run_collection(&services(), &c, &mut vars, opts, &mut |_| {}, &CancellationToken::new()).await.unwrap();
    assert_eq!(s.requests_failed, 20);
    let opts = RunOptions { iterations: Some(20), simulate_failure_rate: 0.0, ..Default::default() };
    let s = run_collection(&services(), &c, &mut vars, opts, &mut |_| {}, &CancellationToken::new()).await.unwrap();
    assert_eq!(s.requests_failed, 0);
}

fn data_rows(users: &[&str]) -> Vec<IndexMap<String, Value>> {
    users
        .iter()
        .map(|u| {
            let mut row = IndexMap::new();
            row.insert("user".to_string(), Value::String(u.to_string()));
            row
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn extra_iterations_reuse_the_last_data_row() {
    // newman/postman-runtime: iterations past the end of the data file get the
    // last row again (they do not wrap around to the first one).
    let base = server().await;
    let mut c = Collection::new("Data");
    c.items.push(req("A", "GET", "{{base}}/echo?u={{user}}").into());
    let mut vars = scopes(&base);
    let mut urls = Vec::new();
    let opts = RunOptions { data: data_rows(&["ada", "bob"]), iterations: Some(3), ..Default::default() };
    let summary = run_collection(
        &services(),
        &c,
        &mut vars,
        opts,
        &mut |e| {
            if let RunEvent::RequestFinished(r) = e {
                urls.push(r.url.clone());
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(summary.iterations, 3);
    let users: Vec<&str> = urls.iter().map(|u| u.rsplit('=').next().unwrap()).collect();
    assert_eq!(users, vec!["ada", "bob", "bob"], "{urls:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn local_variables_last_for_the_whole_run() {
    // Postman: pm.variables.set() values are scoped to the collection run, so
    // later requests (and iterations) of the same run see them.
    let base = server().await;
    let mut c = Collection::new("Locals");
    let mut a = req("A", "GET", "{{base}}/echo?seen={{tok}}");
    a.scripts.post_response = "local.set tok abc".into();
    c.items.push(a.into());
    c.items.push(req("B", "GET", "{{base}}/echo?t={{tok}}").into());
    let mut vars = scopes(&base);
    let mut urls = Vec::new();
    let opts = RunOptions { iterations: Some(2), ..Default::default() };
    run_collection(
        &services(),
        &c,
        &mut vars,
        opts,
        &mut |e| {
            if let RunEvent::RequestFinished(r) = e {
                urls.push(format!("{}{} {}", r.name, r.iteration, r.url));
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(urls[1].starts_with("B0") && urls[1].ends_with("t=abc"), "{urls:?}");
    assert!(urls[2].starts_with("A1") && urls[2].ends_with("seen=abc"), "{urls:?}");
    assert!(vars.local.is_empty(), "local variables do not outlive the run");

    // A single execution outside a run still starts with no local variables.
    vars.local.insert("tok".into(), "stale".into());
    let id = c.requests()[1].id.clone();
    let input = ExecuteInput {
        collection: &c,
        request_id: &id,
        request_override: None,
        iteration: 0,
        iteration_count: 1,
        base_dir: None,
    };
    let exec = execute(&services(), input, &mut vars, &CancellationToken::new()).await;
    assert!(exec.unresolved.contains(&"tok".to_string()), "{:?}", exec.unresolved);
}

fn input<'a>(c: &'a Collection, id: &'a str) -> ExecuteInput<'a> {
    ExecuteInput {
        collection: c,
        request_id: id,
        request_override: None,
        iteration: 0,
        iteration_count: 1,
        base_dir: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_token_fetch_honors_cancel_and_request_timeout() {
    use merpati_core::auth::AuthType;
    use merpati_http::HttpErrorKind;
    // A token endpoint that accepts connections and never answers.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://{}/token", silent.local_addr().unwrap());
    tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((s, _)) = silent.accept().await {
            open.push(s);
        }
    });
    let base = server().await;
    let oauth = Auth::new(AuthType::OAuth2)
        .with("grant_type", "client_credentials")
        .with("accessTokenUrl", token_url)
        .with("clientId", "cid");
    let mut c = Collection::new("OAuth");
    let mut r = req("Echo", "GET", "{{base}}/echo");
    r.request.as_http_mut().unwrap().auth = oauth.clone();
    c.items.push(r.into());
    let mut timed = req("Timed", "GET", "{{base}}/echo");
    let http = timed.request.as_http_mut().unwrap();
    http.auth = oauth;
    http.settings.timeout_ms = Some(300);
    c.items.push(timed.into());
    let ids: Vec<String> = c.requests().iter().map(|r| r.id.clone()).collect();

    let services = services();
    // Building the first HTTP client (system proxy and certificates) can take
    // seconds in a fresh test binary; keep it out of the timings.
    services.http.default_client().unwrap();

    // Stop/Ctrl+C while the token is being fetched.
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let started = std::time::Instant::now();
    let exec = execute(&services, input(&c, &ids[0]), &mut scopes(&base), &cancel).await;
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert_eq!(exec.error.as_ref().map(|e| e.kind), Some(HttpErrorKind::Cancelled), "{:?}", exec.error);

    // The request timeout bounds the token fetch too.
    let started = std::time::Instant::now();
    let exec = execute(&services, input(&c, &ids[1]), &mut scopes(&base), &CancellationToken::new()).await;
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert_eq!(exec.error.as_ref().map(|e| e.kind), Some(HttpErrorKind::Timeout), "{:?}", exec.error);
}

#[tokio::test(flavor = "multi_thread")]
async fn digest_challenge_after_redirect_signs_the_final_url() {
    use merpati_core::auth::AuthType;
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let s = seen.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route("/old", any(|| async { (StatusCode::TEMPORARY_REDIRECT, [("location", "/new")]) }))
        .route(
            "/away",
            any(move || async move {
                (StatusCode::TEMPORARY_REDIRECT, [("location", format!("http://localhost:{port}/new"))])
            }),
        )
        .route(
            "/new",
            any(move |h: HeaderMap| async move {
                let auth = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                s.lock().unwrap().push(auth.clone());
                if auth.starts_with("Digest ") && auth.contains("username=\"u\"") && auth.contains("uri=\"/new\"") {
                    (StatusCode::OK, [("www-authenticate", String::new())], "ok")
                } else {
                    let challenge = "Digest realm=\"r\", nonce=\"n1\", qop=\"auth\"".to_string();
                    (StatusCode::UNAUTHORIZED, [("www-authenticate", challenge)], "no")
                }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let digest = Auth::new(AuthType::Digest).with("username", "u").with("password", "p");
    let mut c = Collection::new("Digest");
    for (name, path) in [("Moved", "/old"), ("Elsewhere", "/away")] {
        let mut r = req(name, "GET", &format!("http://127.0.0.1:{port}{path}"));
        r.request.as_http_mut().unwrap().auth = digest.clone();
        c.items.push(r.into());
    }
    let ids: Vec<String> = c.requests().iter().map(|r| r.id.clone()).collect();

    let exec =
        execute(&services(), input(&c, &ids[0]), &mut VariableScopes::default(), &CancellationToken::new()).await;
    assert!(exec.error.is_none(), "{:?}", exec.error);
    assert_eq!(exec.response.as_ref().map(|r| r.status), Some(200), "{:?}", seen.lock().unwrap());

    // Another origin: the credentials are not sent there (like the redirect
    // itself, which drops Authorization), so the challenge stays unanswered.
    seen.lock().unwrap().clear();
    let exec =
        execute(&services(), input(&c, &ids[1]), &mut VariableScopes::default(), &CancellationToken::new()).await;
    assert_eq!(exec.response.as_ref().map(|r| r.status), Some(401));
    assert!(seen.lock().unwrap().iter().all(|a| a.is_empty()), "{:?}", seen.lock().unwrap());
}
