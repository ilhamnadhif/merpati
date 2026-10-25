//! `IsolatedEngine`: jobs run in worker processes with the same results as
//! `BoaEngine`, and crashes, runaway native code, memory hogs and
//! cancellation are contained.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use merpati_core::model::{Body, BodyMode, KeyValue, RawLanguage};
use merpati_core::response::ResponseBody;
use merpati_core::script::*;
use merpati_core::variables::VariableScopes;
use merpati_script::{BoaEngine, IsolatedEngine, IsolationConfig};
use serde_json::json;

fn worker_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_merpati-script-worker"))
}

fn engine() -> IsolatedEngine {
    IsolatedEngine::new(worker_exe(), IsolationConfig::default())
}

fn request() -> ScriptRequest {
    ScriptRequest {
        id: Some("r1".into()),
        name: Some("List users".into()),
        method: "POST".into(),
        url: "{{baseUrl}}/users?page=1".into(),
        headers: vec![KeyValue::new("Accept", "*/*"), KeyValue::new("X-Off", "1").disabled()],
        body: Body {
            mode: BodyMode::Raw,
            raw: "{\"a\":1}".into(),
            raw_language: RawLanguage::Json,
            ..Default::default()
        },
    }
}

fn source(code: &str) -> ScriptSource {
    ScriptSource { location: "request \"List users\"".into(), code: code.into() }
}

fn pre(code: &str) -> ScriptJob {
    let mut j = ScriptJob::new(ScriptEvent::PreRequest, vec![source(code)], request());
    j.variables.environment.insert("baseUrl".into(), "https://api.example.com".into());
    j
}

fn post(code: &str, body: ResponseBody) -> ScriptJob {
    let mut j = ScriptJob::new(ScriptEvent::PostResponse, vec![source(code)], request());
    j.response = Some(ScriptResponse {
        code: 200,
        status: "OK".into(),
        headers: vec![("Content-Type".into(), "application/json".into())],
        response_size: body.len(),
        body,
        response_time_ms: 12.5,
    });
    j
}

fn file_body(content: &[u8]) -> ResponseBody {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    ResponseBody::File { path: Arc::new(f.into_temp_path()), len: content.len() as u64 }
}

/// Runs the job on both engines and checks the outcomes are identical.
fn same_as_in_process(isolated: &IsolatedEngine, make: impl Fn() -> ScriptJob) -> ScriptOutcome {
    let expected = BoaEngine::new().run(make());
    let got = isolated.run(make());
    assert_eq!(got, expected);
    got
}

fn env<'a>(out: &'a ScriptOutcome, key: &str) -> Option<&'a str> {
    out.variables.environment.get(key).map(String::as_str)
}

#[test]
fn outcomes_match_the_in_process_engine() {
    let e = engine();
    e.warm_up();
    let out = same_as_in_process(&e, || {
        let mut j = pre("pm.test('method', () => pm.expect(pm.request.method).to.equal('POST'));\n\
             pm.test('fails', () => pm.expect(1).to.equal(2));\n\
             console.log('hello', {a: [1, 2]}, pm.variables.get('baseUrl'), pm.info.requestName);\n\
             console.warn('careful', pm.cookies.get('sid'), pm.iterationData.get('row'));\n\
             pm.environment.set('token', 'abc');\n\
             pm.collectionVariables.set('count', 2);\n\
             pm.globals.unset('old');\n\
             pm.variables.set('local', true);\n\
             pm.request.headers.upsert({key: 'X-Trace', value: '1'});\n\
             pm.request.url = pm.request.url.toString().replace('users', 'people');\n\
             pm.request.body.raw = JSON.stringify({b: 2});\n\
             pm.sendRequest('https://x.test', (err) => console.log('no http:', String(err)));\n\
             pm.execution.setNextRequest('Next');");
        j.scripts
            .insert(0, ScriptSource { location: "collection \"C\"".into(), code: "throw new Error('boom')".into() });
        j.scripts.insert(1, ScriptSource { location: "folder \"F\"".into(), code: "let = ;".into() });
        j.variables.globals.insert("old".into(), "1".into());
        j.variables.data.insert("row".into(), json!({"n": 1.5, "tags": ["a"]}));
        j.info = ScriptInfo {
            request_name: "List users".into(),
            request_id: "r1".into(),
            iteration: 1,
            iteration_count: 3,
            collection_name: "C".into(),
        };
        j.cookies = vec![ScriptCookie {
            name: "sid".into(),
            value: "s3cr3t".into(),
            domain: "api.example.com".into(),
            path: "/".into(),
            ..Default::default()
        }];
        j
    });
    assert_eq!(out.errors.len(), 2, "{:?}", out.errors);
    assert_eq!(out.tests.len(), 2);
    assert_eq!(env(&out, "token"), Some("abc"));
    assert_eq!(out.request.url, "{{baseUrl}}/people?page=1");
    assert_eq!(out.next_request, Some(NextRequest::Named("Next".into())));

    let body = r#"{"items":[{"id":1},{"id":2},{"id":3}]}"#;
    let out = same_as_in_process(&e, || {
        post(
            "const data = pm.response.json();\n\
             pm.test('status', () => pm.response.to.have.status(200));\n\
             pm.test('items', () => pm.expect(data.items).to.have.lengthOf(3));\n\
             pm.visualizer.set('<b>{{n}}</b>', {n: data.items.length});\n\
             console.log(pm.response.headers.get('Content-Type'), pm.response.responseTime, pm.response.size().body);",
            ResponseBody::Memory(body.into()),
        )
    });
    assert!(out.errors.is_empty() && out.tests.iter().all(|t| t.passed), "{out:?}");
    assert_eq!(out.visualization.as_ref().map(|v| v.data.clone()), Some(json!({"n": 3})));

    let grpc = merpati_core::protocols::GrpcRequest {
        url: "grpc://localhost:50051".into(),
        service: "helloworld.Greeter".into(),
        method: "SayHello".into(),
        message: "{\"name\": \"Ada\"}".into(),
        ..Default::default()
    };
    let out = same_as_in_process(&e, || {
        ScriptJob::for_protocol(
            ScriptEvent::PreRequest,
            vec![source("pm.request.messages.idx(0).data.name = 'Grace';")],
            protocol::grpc(&grpc),
        )
    });
    assert_eq!(out.protocol_request.unwrap()["messages"][0]["data"]["name"], "Grace");
}

#[test]
fn typed_variables_cross_the_process_boundary() {
    use merpati_core::VarType;
    let e = engine();
    let out = same_as_in_process(&e, || {
        let mut j = pre("pm.environment.set('n', pm.environment.get('n') + 1);\n\
             pm.environment.set('o', {a: [pm.collectionVariables.get('b')]});\n\
             pm.variables.set('l', 1.5);\n\
             pm.globals.set('s', 9);");
        j.variables.environment.insert("n".into(), "41".into());
        j.variables.types.environment.insert("n".into(), VarType::Number);
        j.variables.collection.insert("b".into(), "true".into());
        j.variables.types.collection.insert("b".into(), VarType::Boolean);
        j.variables.globals.insert("s".into(), "x".into());
        j.variables.types.globals.insert("s".into(), VarType::String);
        j
    });
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(env(&out, "n"), Some("42"));
    assert_eq!(env(&out, "o"), Some(r#"{"a":[true]}"#));
    let types = &out.variables.types;
    assert_eq!(types.environment.get("n"), Some(&VarType::Number));
    assert_eq!(types.environment.get("o"), Some(&VarType::Any));
    assert_eq!(types.local.get("l"), Some(&VarType::Any));
    assert_eq!((out.variables.globals["s"].as_str(), types.globals.get("s")), ("9", Some(&VarType::String)));

    let vars = out.variables.clone();
    let out = same_as_in_process(&e, || {
        let mut j = post(
            "pm.test('typed', () => {\n\
               pm.expect(pm.environment.get('n')).to.equal(42);\n\
               pm.expect(pm.environment.get('o')).to.eql({a: [true]});\n\
               pm.expect(pm.variables.get('l')).to.equal(1.5);\n\
               pm.expect(pm.globals.get('s')).to.equal('9');\n\
               pm.expect(pm.collectionVariables.toObject()).to.eql({b: true});\n\
             });",
            ResponseBody::Memory("{}".into()),
        );
        j.variables = vars.clone();
        j
    });
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.tests[0].passed, "{:?}", out.tests);
}

/// Fake network for `pm.sendRequest`; remembers the temp files it handed out.
#[derive(Default)]
struct FakeHttp {
    files: Mutex<Vec<PathBuf>>,
}

impl ScriptHttp for FakeHttp {
    fn send(&self, req: ScriptRequest, scopes: &VariableScopes) -> Result<ScriptResponse, String> {
        let url = merpati_core::Resolver::new(scopes).resolve(&req.url);
        let ok = |body: ResponseBody| ScriptResponse {
            code: 200,
            status: "OK".into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            response_size: body.len(),
            body,
            response_time_ms: 3.0,
        };
        if url.ends_with("/fail") {
            Err("connect ECONNREFUSED 127.0.0.1:1".into())
        } else if url.ends_with("/memory") {
            Ok(ok(ResponseBody::Memory(json!({"kind": "memory", "method": req.method}).to_string().into())))
        } else if url.ends_with("/file") {
            let body = file_body(json!({"kind": "file", "raw": req.body.raw}).to_string().as_bytes());
            self.files.lock().unwrap().push(body.file_path().unwrap().to_path_buf());
            Ok(ok(body))
        } else if url.ends_with("/big") {
            Ok(ok(ResponseBody::Memory(json!({"pad": "x".repeat(400_000)}).to_string().into())))
        } else {
            Err(format!("unexpected URL {url}"))
        }
    }
}

#[test]
fn send_request_round_trips_through_the_app() {
    let e = engine();
    let http = Arc::new(FakeHttp::default());
    let code = "pm.environment.set('host', 'https://api.test');\n\
        pm.sendRequest('{{host}}/memory', (err, res) => {\n\
          pm.environment.set('cb', res.json().kind + ':' + res.json().method);\n\
          pm.test('callback', () => pm.expect(res.code).to.equal(200));\n\
        });\n\
        const f = await pm.sendRequest({url: '{{host}}/file', method: 'POST', body: {mode: 'raw', raw: 'hi'}});\n\
        pm.environment.set('file', f.json().kind + ':' + f.json().raw + ':' + f.text().length);\n\
        const big = await pm.sendRequest('{{host}}/big');\n\
        pm.environment.set('big', String(big.text().length));\n\
        try { await pm.sendRequest('{{host}}/fail'); } catch (err) { pm.environment.set('fail', String(err)); }\n\
        pm.sendRequest('{{host}}/fail', (err, res) => pm.environment.set('cbfail', String(err) + ':' + res));";
    let job = || {
        let mut j = pre(code);
        j.http = Some(http.clone());
        j
    };
    let out = same_as_in_process(&e, job);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.tests.iter().all(|t| t.passed), "{:?}", out.tests);
    assert_eq!(env(&out, "cb"), Some("memory:GET"));
    assert_eq!(env(&out, "file"), Some("file:hi:26"));
    assert_eq!(env(&out, "big"), Some("400010"));
    assert!(env(&out, "fail").unwrap().contains("ECONNREFUSED"), "{:?}", env(&out, "fail"));
    assert!(env(&out, "cbfail").unwrap().contains("ECONNREFUSED"), "{:?}", env(&out, "cbfail"));
    // The response files lived until the job ended, then were deleted.
    let files = http.files.lock().unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| !f.exists()));
}

#[test]
fn post_response_reads_file_backed_and_large_bodies() {
    let e = engine();
    let items: Vec<_> = (0..12000).map(|i| json!({"id": i, "name": format!("user {i}")})).collect();
    let text = json!({"items": items}).to_string();
    let code = "pm.test('items', () => pm.expect(pm.response.json().items).to.have.lengthOf(12000));\n\
        pm.environment.set('len', String(pm.response.text().length));\n\
        pm.environment.set('last', pm.response.json().items[11999].name);";

    let file = file_body(text.as_bytes());
    let out = same_as_in_process(&e, || post(code, file.clone()));
    assert!(out.errors.is_empty() && out.tests[0].passed, "{out:?}");
    assert_eq!(env(&out, "len"), Some(text.len().to_string().as_str()));
    assert_eq!(env(&out, "last"), Some("user 11999"));
    // The worker read the app's file without deleting it.
    assert!(file.file_path().unwrap().exists());

    // Larger than what travels inline.
    assert!(text.len() > 256 * 1024);
    let out = same_as_in_process(&e, || post(code, ResponseBody::Memory(text.clone().into())));
    assert!(out.errors.is_empty() && out.tests[0].passed, "{out:?}");

    // A body nobody reads is never opened, like in-process.
    let missing = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    std::fs::remove_file(&missing).unwrap();
    let body = ResponseBody::File { path: Arc::new(missing), len: 10 };
    let out = e.run(post("pm.test('code', () => pm.expect(pm.response.code).to.equal(200));", body));
    assert!(out.errors.is_empty() && out.tests[0].passed, "{out:?}");
}

#[test]
fn a_crashing_worker_fails_only_its_job() {
    let e = engine();
    let job = pre("let o = {}; for (let i = 0; i < 300000; i++) { o = {a: o}; }\nJSON.stringify(o);");
    let (request, variables) = (job.request.clone(), job.variables.clone());
    let out = e.run(job);
    assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
    assert_eq!(out.errors[0].location, "script engine");
    assert!(
        out.errors[0].message.starts_with(
            "The script engine stopped unexpectedly (for example a stack overflow from very deeply nested data); \
             the app was not affected."
        ),
        "{:?}",
        out.errors
    );
    assert_eq!(out.request, request);
    assert_eq!(out.variables, variables);

    // The next job gets a fresh worker.
    let out = e.run(pre("pm.environment.set('after', 'crash');"));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(env(&out, "after"), Some("crash"));
}

/// A link to the worker executable with a unique name, to find its processes.
fn unique_worker(dir: &Path, name: &str) -> PathBuf {
    let exe = dir.join(name);
    #[cfg(unix)]
    std::os::unix::fs::symlink(worker_exe(), &exe).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(worker_exe(), &exe).unwrap();
    exe
}

/// Processes started from `exe`; `None` where `ps` is unavailable.
fn processes_of(exe: &Path) -> Option<usize> {
    let out = std::process::Command::new("ps").args(["-A", "-ww", "-o", "command="]).output().ok()?;
    let exe = exe.to_str()?;
    Some(String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains(exe)).count())
}

#[test]
fn runaway_native_code_is_killed_at_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let exe = unique_worker(dir.path(), "worker-timeout");
    let e = IsolatedEngine::new(exe.clone(), IsolationConfig::default());
    assert!(e.run(pre("pm.environment.set('warm', 1);")).errors.is_empty());
    if let Some(n) = processes_of(&exe) {
        assert_eq!(n, 1, "the idle worker is running");
    }

    // Catastrophic backtracking never reaches a host call.
    let mut job = pre("pm.environment.set('before', 1);\n/(a+)+$/.test('a'.repeat(40) + 'b');");
    job.timeout = Duration::from_millis(500);
    let started = Instant::now();
    let out = e.run(job);
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
    assert_eq!(out.errors[0].message, "Script execution timed out after 500 ms");
    assert_eq!(env(&out, "before"), None, "partial results of a killed job are lost");
    if let Some(n) = processes_of(&exe) {
        assert_eq!(n, 0, "the worker was killed and reaped");
    }

    assert!(e.run(pre("pm.environment.set('again', 1);")).errors.is_empty());
}

#[test]
fn workers_idle_after_a_burst_are_shut_down() {
    let dir = tempfile::tempdir().unwrap();
    let exe = unique_worker(dir.path(), "worker-idle");
    let config = IsolationConfig {
        max_idle_workers: 4,
        idle_timeout: std::time::Duration::from_millis(200),
        ..Default::default()
    };
    let e = IsolatedEngine::new(exe.clone(), config);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let (e, barrier) = (e.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                e.run(pre("const t = Date.now(); while (Date.now() - t < 300) {} pm.environment.set('x', 1);"))
            })
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap().errors.is_empty());
    }
    let Some(after_burst) = processes_of(&exe) else { return };
    assert!(after_burst >= 2, "{after_burst}");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert_eq!(processes_of(&exe), Some(1), "one worker stays for the next job");
    assert!(e.run(pre("pm.environment.set('again', 1);")).errors.is_empty());
}

#[test]
fn dropping_the_engine_stops_its_workers() {
    let dir = tempfile::tempdir().unwrap();
    let exe = unique_worker(dir.path(), "worker-drop");
    let e = IsolatedEngine::new(exe.clone(), IsolationConfig { max_idle_workers: 2, ..Default::default() });
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let e = e.clone();
            std::thread::spawn(move || e.run(pre("pm.environment.set('x', 1);")))
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap().errors.is_empty());
    }
    let Some(running) = processes_of(&exe) else { return };
    assert!(running >= 1);
    drop(e);
    assert_eq!(processes_of(&exe), Some(0));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn memory_hogs_are_killed() {
    let config = IsolationConfig { memory_limit_bytes: 300 * 1024 * 1024, ..Default::default() };
    let e = IsolatedEngine::new(worker_exe(), config);
    // Bounded (about 1 GB) in case the limit were not enforced.
    let mut job = pre("let mb = 'x'; for (let k = 0; k < 20; k++) { mb += mb; }\n\
         const keep = []; for (let i = 0; i < 1000; i++) { keep.push(mb + i); }");
    job.timeout = Duration::from_secs(60);
    let started = Instant::now();
    let out = e.run(job);
    assert!(started.elapsed() < Duration::from_secs(20), "took {:?}", started.elapsed());
    assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
    assert_eq!(out.errors[0].location, "script engine");
    assert_eq!(out.errors[0].message, "Script used more than 300 MB of memory");

    assert!(e.run(pre("pm.environment.set('after', 1);")).errors.is_empty());
}

fn cancel_after(job: &mut ScriptJob, delay: Duration) {
    let cancel = Arc::new(AtomicBool::new(false));
    job.cancel = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        cancel.store(true, Ordering::Relaxed);
    });
}

#[test]
fn cancellation_from_another_thread() {
    let e = engine();
    assert!(e.run(pre("pm.environment.set('warm', 1);")).errors.is_empty());

    // Stopped at a host call: partial results are kept, the worker is reused.
    let mut job = pre("console.log('before');\nsetTimeout(() => console.log('never'), 20000);");
    job.timeout = Duration::from_secs(30);
    cancel_after(&mut job, Duration::from_millis(300));
    let started = Instant::now();
    let out = e.run(job);
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
    assert_eq!(out.errors[0].location, "request \"List users\"");
    assert_eq!(out.errors[0].message, "Script execution was cancelled");
    assert_eq!(out.console[0].message, "before");
    assert!(out.console.iter().all(|c| c.message != "never"), "{:?}", out.console);

    // Stuck in native code: the worker is killed.
    let mut job = pre("/(a+)+$/.test('a'.repeat(40) + 'b');");
    job.timeout = Duration::from_secs(30);
    cancel_after(&mut job, Duration::from_millis(300));
    let started = Instant::now();
    let out = e.run(job);
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
    assert_eq!(out.errors[0].location, "script engine");
    assert_eq!(out.errors[0].message, "Script execution was cancelled");

    assert!(e.run(pre("pm.environment.set('after', 1);")).errors.is_empty());
}

#[test]
fn concurrent_jobs_get_their_own_workers() {
    let e = engine();
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let e = e.clone();
            std::thread::spawn(move || {
                for round in 0..3 {
                    let out = e.run(post(
                        &format!(
                            "pm.test('n', () => pm.expect(pm.response.json().n).to.equal({i})); \
                             pm.environment.set('i', '{i}-{round}');"
                        ),
                        ResponseBody::Memory(format!("{{\"n\": {i}}}").into()),
                    ));
                    assert!(out.errors.is_empty(), "{:?}", out.errors);
                    assert!(out.tests[0].passed, "{:?}", out.tests);
                    assert_eq!(env(&out, "i"), Some(format!("{i}-{round}").as_str()));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn empty_jobs_and_missing_workers() {
    // No code: no worker is started at all.
    let e = IsolatedEngine::new(PathBuf::from("/nonexistent/merpati-worker"), IsolationConfig::default());
    let out = e.run(pre("  \n"));
    assert!(out.errors.is_empty());
    assert_eq!(out.request, request());

    let out = e.run(pre("pm.environment.set('x', 1);"));
    assert_eq!(out.errors.len(), 1);
    assert!(out.errors[0].message.starts_with("could not start script worker:"), "{:?}", out.errors);
    assert_eq!(out.request, request());
}

#[test]
fn maybe_run_worker_returns_in_a_normal_process() {
    let started = Instant::now();
    merpati_script::maybe_run_worker();
    assert!(started.elapsed() < Duration::from_millis(50));
}
