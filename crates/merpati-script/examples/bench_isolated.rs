//! Overhead of process isolation:
//! `cargo run --release -p merpati-script --example bench_isolated`.
//!
//! Also the wiring every binary using `IsolatedEngine` needs: this example is
//! its own worker executable.
use std::time::{Duration, Instant};

use merpati_core::response::ResponseBody;
use merpati_core::script::*;
use merpati_script::{BoaEngine, IsolatedEngine, IsolationConfig};

fn pre(code: &str) -> ScriptJob {
    let request = ScriptRequest { method: "GET".into(), url: "{{baseUrl}}/users?page=1".into(), ..Default::default() };
    let mut j = ScriptJob::new(
        ScriptEvent::PreRequest,
        vec![ScriptSource { location: "request".into(), code: code.into() }],
        request,
    );
    j.variables.environment.insert("baseUrl".into(), "https://api.example.com".into());
    j
}

fn post(code: &str, body: String) -> ScriptJob {
    let mut j = pre(code);
    j.event = ScriptEvent::PostResponse;
    j.response = Some(ScriptResponse {
        code: 200,
        status: "OK".into(),
        headers: vec![("Content-Type".into(), "application/json".into())],
        response_size: body.len() as u64,
        body: ResponseBody::Memory(body.into()),
        response_time_ms: 20.0,
    });
    j
}

const PRE: &str = "pm.request.headers.upsert({key: 'X-Request-Id', value: pm.variables.replaceIn('{{$guid}}')});\n\
pm.variables.set('startedAt', Date.now());";
const POST: &str = "pm.test('Status code is 200', function () { pm.response.to.have.status(200); });\n\
const json = pm.response.json();\n\
pm.test('Has users', () => pm.expect(json.data).to.be.an('array').with.lengthOf(2));\n\
pm.environment.set('token', json.token);";
const BODY: &str = r#"{"data":[{"id":1,"name":"Ada"},{"id":2,"name":"Linus"}],"token":"abc"}"#;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn time(label: &str, f: impl FnOnce() -> ScriptOutcome) {
    let t = Instant::now();
    let out = f();
    println!("{label:<52} {:>9.2} ms  {:?}", ms(t.elapsed()), out.errors.first().map(|e| &e.message));
}

fn avg(label: &str, n: u32, mut f: impl FnMut()) {
    let t = Instant::now();
    for _ in 0..n {
        f();
    }
    println!("{label:<52} {:>9.3} ms", ms(t.elapsed()) / f64::from(n));
}

fn rss_of_children() -> Vec<(String, u64)> {
    let me = std::process::id().to_string();
    let out = std::process::Command::new("pgrep").args(["-P", &me]).output();
    let pids = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    pids.lines()
        .map(|pid| {
            let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", pid]).output();
            let kb = out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok()).unwrap_or(0);
            (pid.to_string(), kb)
        })
        .collect()
}

fn main() {
    merpati_script::maybe_run_worker();

    let n = 1000;
    let boa = BoaEngine::new();
    let iso = IsolatedEngine::for_current_exe(IsolationConfig::default()).expect("current exe");

    time("in-process: first job (thread + prelude)", || boa.run(pre(PRE)));
    time("isolated:   first job (process + prelude)", || iso.run(pre(PRE)));
    boa.run(post(POST, BODY.into()));
    iso.run(post(POST, BODY.into()));
    for (pid, kb) in rss_of_children() {
        println!("worker process {pid} after warm-up: rss {} MB", kb / 1024);
    }

    for round in 1..=2 {
        println!("-- round {round}");
        avg("in-process: pre + post job pair (avg)", n, || {
            assert!(boa.run(pre(PRE)).errors.is_empty());
            assert!(boa.run(post(POST, BODY.into())).errors.is_empty());
        });
        avg("isolated:   pre + post job pair (avg)", n, || {
            assert!(iso.run(pre(PRE)).errors.is_empty());
            assert!(iso.run(post(POST, BODY.into())).errors.is_empty());
        });
        avg("in-process: trivial job (avg)", n, || {
            assert!(boa.run(pre("pm.variables.set('a', 1);")).errors.is_empty())
        });
        avg("isolated:   trivial job (avg)", n, || {
            assert!(iso.run(pre("pm.variables.set('a', 1);")).errors.is_empty())
        });
    }

    let code = "pm.test('size', () => pm.expect(pm.response.text().length).to.be.above(0));";
    for (label, size) in [("200 KB body, inline", 200 * 1024), ("4 MB body, spilled to a file", 4 << 20)] {
        let body = format!("{{\"pad\":\"{}\"}}", "x".repeat(size));
        avg(&format!("in-process: post job, {label}"), 50, || {
            assert!(boa.run(post(code, body.clone())).errors.is_empty())
        });
        avg(&format!("isolated:   post job, {label}"), 50, || {
            assert!(iso.run(post(code, body.clone())).errors.is_empty())
        });
    }

    for (pid, kb) in rss_of_children() {
        println!("worker process {pid} after the 4 MB bodies: rss {} MB", kb / 1024);
    }

    time("isolated:   crash (300k-deep JSON.stringify)", || {
        iso.run(pre("let o = {}; for (let i = 0; i < 300000; i++) { o = {a: o}; } JSON.stringify(o);"))
    });
    time("isolated:   next job (fresh process + prelude)", || iso.run(pre(PRE)));
    let mut hang = pre("/(a+)+$/.test('a'.repeat(40) + 'b');");
    hang.timeout = Duration::from_millis(500);
    time("isolated:   regex hang, timeout 500 ms + kill grace", || iso.run(hang));
}
