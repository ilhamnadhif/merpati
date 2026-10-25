//! Measures sandbox overhead: `cargo run --release -p merpati-script --example bench_jobs`.
use std::time::{Duration, Instant};

use merpati_core::model::{Body, KeyValue};
use merpati_core::response::ResponseBody;
use merpati_core::script::*;
use merpati_script::BoaEngine;

fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output();
    out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok()).unwrap_or(0)
}

fn request() -> ScriptRequest {
    ScriptRequest {
        id: Some("r".into()),
        name: Some("List users".into()),
        method: "GET".into(),
        url: "{{baseUrl}}/users?page=1".into(),
        headers: vec![KeyValue::new("Accept", "application/json")],
        body: Body::default(),
    }
}

fn pre(code: &str) -> ScriptJob {
    let mut j = ScriptJob::new(
        ScriptEvent::PreRequest,
        vec![ScriptSource { location: "request".into(), code: code.into() }],
        request(),
    );
    j.variables.environment.insert("baseUrl".into(), "https://api.example.com".into());
    j
}

fn post(code: &str) -> ScriptJob {
    let body = r#"{"data":[{"id":1,"name":"Ada"},{"id":2,"name":"Linus"}],"token":"abc"}"#;
    let mut j = ScriptJob::new(
        ScriptEvent::PostResponse,
        vec![ScriptSource { location: "request".into(), code: code.into() }],
        request(),
    );
    j.response = Some(ScriptResponse {
        code: 200,
        status: "OK".into(),
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: ResponseBody::Memory(body.into()),
        response_time_ms: 20.0,
        response_size: body.len() as u64,
    });
    j
}

const PRE: &str = "pm.request.headers.upsert({key: 'X-Request-Id', value: pm.variables.replaceIn('{{$guid}}')});\npm.variables.set('startedAt', Date.now());";
const POST: &str = "pm.test('Status code is 200', function () { pm.response.to.have.status(200); });\n\
const json = pm.response.json();\n\
pm.test('Has users', () => { pm.expect(json.data).to.be.an('array').with.lengthOf(2); pm.expect(json.data[0]).to.have.property('name', 'Ada'); });\n\
pm.test('Fast', () => pm.expect(pm.response.responseTime).to.be.below(500));\n\
pm.environment.set('token', json.token);";

const PRE_GRPC: &str = "pm.request.metadata.upsert({key: 'x-request-id', value: pm.variables.replaceIn('{{$guid}}')});\n\
pm.request.messages.idx(0).data.name = 'Grace';";

fn time(label: &str, f: impl FnOnce() -> ScriptOutcome) -> Duration {
    let t = Instant::now();
    let out = f();
    let d = t.elapsed();
    assert!(out.errors.is_empty(), "{label}: {:?}", out.errors);
    println!("{label:<44} {:>9.2} ms", d.as_secs_f64() * 1000.0);
    d
}

fn main() {
    println!("rss at start: {} KB", rss_kb());
    let e = BoaEngine::new();
    time("cold: first job (worker + prelude)", || e.run(pre(PRE)));
    time("first post-response (loads chai)", || e.run(post(POST)));
    println!("rss after prelude + chai: {} KB", rss_kb());

    let n = 300;
    let t = Instant::now();
    for _ in 0..n {
        let a = e.run(pre(PRE));
        let b = e.run(post(POST));
        assert!(a.errors.is_empty() && b.errors.is_empty());
        assert!(b.tests.iter().all(|t| t.passed));
    }
    let per = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("{:<44} {:>9.2} ms", "warm: typical pre + post job pair (avg)", per);
    let t = Instant::now();
    for _ in 0..n {
        e.run(pre("pm.variables.set('a', 1);"));
    }
    println!("{:<44} {:>9.2} ms", "warm: trivial job (avg)", t.elapsed().as_secs_f64() * 1000.0 / n as f64);

    time("first use of lodash (_)", || e.run(pre("_.map([1], x => x);")));
    time("first use of moment", || e.run(pre("moment().format();")));
    time("first use of CryptoJS", || e.run(pre("CryptoJS.SHA256('x').toString();")));
    time("first use of tv4", || e.run(pre("tv4.validate({}, {});")));
    time("first use of Intl (toLocaleString)", || {
        e.run(pre("(1234.5).toLocaleString('en-US', {style: 'currency', currency: 'USD'});"))
    });
    time("first use of Web Streams", || e.run(pre("new ReadableStream().getReader();")));
    time("lodash again (cached)", || e.run(pre("_.map([1], x => x);")));
    time("Intl again (cached)", || e.run(pre("new Date().toLocaleString(); new Intl.NumberFormat('de').format(1.5);")));
    println!("rss with every library loaded: {} KB", rss_kb());

    for round in 1..=3 {
        for _ in 0..1000 {
            let b = e.run(post(POST));
            assert!(b.errors.is_empty());
        }
        println!("rss after {} more post jobs (round {round}): {} KB", 1000, rss_kb());
    }

    let grpc = merpati_core::protocols::GrpcRequest {
        url: "grpc://localhost:50051".into(),
        service: "helloworld.Greeter".into(),
        method: "SayHello".into(),
        message: "{\"name\": \"Ada\"}".into(),
        ..Default::default()
    };
    let protocol_job = || {
        ScriptJob::for_protocol(
            ScriptEvent::PreRequest,
            vec![ScriptSource { location: "request".into(), code: PRE_GRPC.into() }],
            merpati_core::script::protocol::grpc(&grpc),
        )
    };
    e.run(protocol_job());
    let t = Instant::now();
    for _ in 0..n {
        let out = e.run(protocol_job());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
    }
    println!("{:<44} {:>9.2} ms", "warm: gRPC pre-request job (avg)", t.elapsed().as_secs_f64() * 1000.0 / n as f64);

    let ws = std::env::temp_dir().join(format!("merpati-bench-{}", std::process::id()));
    std::fs::create_dir_all(ws.join("packages")).unwrap();
    std::fs::write(ws.join("packages/mathutil.js"), "module.exports = { double: (x) => x * 2 };\n").unwrap();
    let pkg_job = || {
        let mut j = pre("pm.variables.set('d', pm.require('mathutil').double(2));");
        j.packages = Some(PackageContext::for_workspace(&ws, ""));
        j
    };
    e.run(pkg_job());
    let t = Instant::now();
    for _ in 0..n {
        let out = e.run(pkg_job());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
    }
    println!(
        "{:<44} {:>9.2} ms",
        "warm: job requiring a Package Library file",
        t.elapsed().as_secs_f64() * 1000.0 / n as f64
    );
    let _ = std::fs::remove_dir_all(&ws);

    let t = Instant::now();
    let out = e.run(pre("let s = 0; for (let i = 0; i < 1e6; i++) { s += i; }"));
    assert!(out.errors.is_empty());
    println!("{:<44} {:>9.2} ms", "1M loop iterations in user code", t.elapsed().as_secs_f64() * 1000.0);
}
