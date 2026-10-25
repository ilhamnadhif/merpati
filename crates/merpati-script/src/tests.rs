use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use merpati_core::model::{Body, BodyMode, KeyValue, RawLanguage};
use merpati_core::response::ResponseBody;
use merpati_core::script::{LogLevel, ScriptEvent, ScriptRequest, ScriptResponse, ScriptSource};

use super::*;

fn source(code: &str) -> ScriptSource {
    ScriptSource { location: "request \"t\"".into(), code: code.into() }
}

fn request() -> ScriptRequest {
    ScriptRequest {
        id: Some("r1".into()),
        name: Some("t".into()),
        method: "POST".into(),
        url: "{{baseUrl}}/users/:id?x=1&flag".into(),
        headers: vec![KeyValue::new("Accept", "*/*"), KeyValue::new("X-Off", "1").disabled()],
        body: Body {
            mode: BodyMode::Raw,
            raw: "{\"a\":1}".into(),
            raw_language: RawLanguage::Json,
            urlencoded: vec![KeyValue::new("kept", "for later")],
            ..Default::default()
        },
    }
}

fn job(event: ScriptEvent, code: &str) -> ScriptJob {
    ScriptJob::new(event, vec![source(code)], request())
}

fn post_job(code: &str, body: &str) -> ScriptJob {
    let mut j = job(ScriptEvent::PostResponse, code);
    j.response = Some(ScriptResponse {
        code: 200,
        status: "OK".into(),
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: ResponseBody::Memory(body.to_string().into()),
        response_time_ms: 10.0,
        response_size: body.len() as u64,
    });
    j
}

#[test]
fn smoke() {
    let e = BoaEngine::new();
    let out = e.run(job(ScriptEvent::PreRequest, "pm.environment.set('x', 1); console.log('hi', {a: 1});"));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.variables.environment.get("x").map(String::as_str), Some("1"));
    assert_eq!(out.console[0].message, "hi { a: 1 }");
    assert_eq!(out.console[0].level, LogLevel::Log);
}

#[test]
fn untouched_request_is_returned_verbatim() {
    let e = BoaEngine::new();
    let out =
        e.run(job(ScriptEvent::PreRequest, "const u = pm.request.url.toString(); pm.request.headers.get('accept');"));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.request, request());
}

#[test]
fn modified_body_keeps_inactive_mode_data() {
    let e = BoaEngine::new();
    let out = e.run(job(ScriptEvent::PreRequest, "pm.request.body.raw = '{\"a\":2}';"));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.request.body.raw, "{\"a\":2}");
    assert_eq!(out.request.body.raw_language, RawLanguage::Json);
    assert_eq!(out.request.body.urlencoded, vec![KeyValue::new("kept", "for later")]);
    // Disabled header survives a header edit.
    let out = e.run(job(ScriptEvent::PreRequest, "pm.request.headers.add({key: 'X-New', value: 'v'});"));
    assert_eq!(out.request.headers.len(), 3);
    assert!(!out.request.headers[1].enabled);
    assert_eq!(out.request.headers[2], KeyValue::new("X-New", "v"));
}

#[test]
fn jobs_are_isolated() {
    let e = BoaEngine::with_config(EngineConfig { max_idle_workers: 1, ..Default::default() });
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "leaked = 1; globalThis.alsoLeaked = 2; JSON.parse = () => 'hacked'; console = null; pm.variables.set('l', 'v');",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "console.log(typeof leaked, typeof alsoLeaked, JSON.parse('1'), pm.variables.get('l'));",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.console[0].message, "undefined undefined 1 undefined");
}

#[test]
fn later_scripts_run_after_errors_and_errors_have_locations() {
    let e = BoaEngine::new();
    let mut j = job(ScriptEvent::PreRequest, "");
    j.scripts = vec![
        ScriptSource { location: "collection \"C\"".into(), code: "throw new TypeError('nope')".into() },
        ScriptSource { location: "folder \"F\"".into(), code: "throw 'a string'".into() },
        ScriptSource { location: "folder \"G\"".into(), code: "throw null".into() },
        ScriptSource { location: "request \"R\"".into(), code: "pm.environment.set('ran', true)".into() },
    ];
    let out = e.run(j);
    assert_eq!(out.errors.len(), 3, "{:?}", out.errors);
    assert_eq!(out.errors[0].location, "collection \"C\"");
    assert_eq!(out.errors[0].message, "TypeError: nope (line 1)");
    assert_eq!(out.errors[1].message, "a string");
    assert_eq!(out.errors[2].message, "null");
    assert_eq!(out.variables.environment.get("ran").map(String::as_str), Some("true"));
}

#[test]
fn recovers_after_timeout() {
    let e = BoaEngine::with_config(EngineConfig { max_idle_workers: 1, ..Default::default() });
    let mut j = job(ScriptEvent::PreRequest, "for (;;) { Math.random(); }");
    j.timeout = Duration::from_millis(150);
    let t = Instant::now();
    let out = e.run(j);
    assert!(t.elapsed() < Duration::from_millis(1000), "took {:?}", t.elapsed());
    assert_eq!(out.errors.len(), 1);
    assert!(out.errors[0].message.contains("timed out"), "{:?}", out.errors);
    // The engine is still usable.
    let out = e.run(post_job("pm.test('ok', () => pm.response.to.have.status(200));", "{}"));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.tests[0].passed);
}

#[test]
fn infinite_loop_inside_callbacks_is_stopped() {
    // Code reached through native calls (Array.prototype.forEach) is not
    // budgeted; it is stopped by the loop iteration limit instead.
    let e = BoaEngine::with_config(EngineConfig { loop_iteration_limit: 200_000, ..Default::default() });
    let mut j = job(ScriptEvent::PreRequest, "[1].forEach(() => { while (true) {} });");
    j.timeout = Duration::from_secs(10);
    let out = e.run(j);
    assert_eq!(out.errors.len(), 1);
    assert!(out.errors[0].message.contains("loop"), "{:?}", out.errors);
}

#[test]
fn cancellation() {
    let e = BoaEngine::new();
    let mut j = job(ScriptEvent::PreRequest, "while (true) {}");
    j.timeout = Duration::from_secs(30);
    let cancel = Arc::new(AtomicBool::new(false));
    j.cancel = cancel.clone();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        c2.store(true, Ordering::Relaxed);
    });
    let t = Instant::now();
    let out = e.run(j);
    assert!(t.elapsed() < Duration::from_secs(2), "took {:?}", t.elapsed());
    assert!(out.errors[0].message.contains("cancelled"), "{:?}", out.errors);
}

#[test]
fn empty_jobs_skip_the_engine() {
    let e = BoaEngine::new();
    let out = e.run(job(ScriptEvent::PreRequest, "   \n "));
    assert_eq!(out.request, request());
    assert!(out.errors.is_empty() && out.tests.is_empty());
}

#[test]
fn concurrent_jobs() {
    let e = BoaEngine::new();
    let handles: Vec<_> = (0..6)
        .map(|i| {
            let e = e.clone();
            std::thread::spawn(move || {
                let out = e.run(post_job(
                    &format!("pm.test('n', () => pm.expect(pm.response.json().n).to.equal({i})); pm.environment.set('i', {i});"),
                    &format!("{{\"n\": {i}}}"),
                ));
                assert!(out.errors.is_empty(), "{:?}", out.errors);
                assert!(out.tests[0].passed, "{:?}", out.tests);
                assert_eq!(out.variables.environment.get("i").cloned(), Some(i.to_string()));
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn response_body_is_lazy() {
    // A file-backed body that no script reads is never opened.
    let e = BoaEngine::new();
    let mut j = post_job("pm.test('code', () => pm.expect(pm.response.code).to.equal(200));", "");
    let missing = tempfile_path();
    j.response.as_mut().unwrap().body = ResponseBody::File { path: Arc::new(missing), len: 10 };
    let out = e.run(j);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.tests[0].passed);
}

/// A temp path whose file is already gone, so any read of it fails.
fn tempfile_path() -> tempfile::TempPath {
    let p = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    std::fs::remove_file(&p).unwrap();
    p
}

#[test]
fn send_request_without_http_reports_error() {
    let e = BoaEngine::new();
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "pm.sendRequest('http://x', (err) => pm.environment.set('err', err.message));",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.variables.environment["err"].contains("not available"));
}

#[test]
fn typed_values_within_a_job() {
    let e = BoaEngine::new();
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "pm.environment.set('o', {a: [1]}); pm.environment.set('n', 5); pm.environment.set('u', undefined);\n\
         pm.test('typed', () => { pm.expect(pm.environment.get('o').a[0]).to.equal(1); pm.expect(pm.environment.get('n')).to.equal(5); });",
    ));
    assert!(out.tests[0].passed, "{:?}", out.tests);
    assert_eq!(out.variables.environment["o"], "{\"a\":[1]}");
    assert_eq!(out.variables.environment["n"], "5");
    assert_eq!(out.variables.environment["u"], "");
}

/// Variables with Postman types, as the app hands them to a job.
fn typed_scopes() -> merpati_core::VariableScopes {
    use merpati_core::VarType;
    let mut v = merpati_core::VariableScopes::default();
    for (k, text, ty) in [
        ("n", "5", VarType::Number),
        ("b", "false", VarType::Boolean),
        ("o", r#"{"a":[1]}"#, VarType::Any),
        ("s", "7", VarType::String),
        ("plain", "x", VarType::Default),
    ] {
        v.environment.insert(k.into(), text.into());
        if ty != VarType::Default {
            v.types.environment.insert(k.into(), ty);
        }
    }
    v.collection.insert("c".into(), "1.5".into());
    v.types.collection.insert("c".into(), VarType::Number);
    v.globals.insert("g".into(), "true".into());
    v.types.globals.insert("g".into(), VarType::Boolean);
    v
}

#[test]
fn typed_variables_are_read_and_set_like_postman() {
    use merpati_core::VarType;
    let e = BoaEngine::new();
    let mut j = job(
        ScriptEvent::PreRequest,
        "pm.test('get', () => {\n\
           pm.expect(pm.environment.get('n')).to.equal(5);\n\
           pm.expect(pm.environment.get('b')).to.equal(false);\n\
           pm.expect(pm.environment.get('o')).to.eql({a: [1]});\n\
           pm.expect(pm.environment.get('s')).to.equal('7');\n\
           pm.expect(pm.environment.get('plain')).to.equal('x');\n\
           pm.expect(pm.collectionVariables.get('c')).to.equal(1.5);\n\
           pm.expect(pm.globals.get('g')).to.equal(true);\n\
           pm.expect(pm.variables.get('n')).to.equal(5);\n\
           pm.expect(pm.variables.get('c')).to.equal(1.5);\n\
         });\n\
         pm.test('toObject', () => {\n\
           pm.expect(pm.environment.toObject()).to.eql({n: 5, b: false, o: {a: [1]}, s: '7', plain: 'x'});\n\
           pm.expect(pm.variables.toObject()).to.include({n: 5, c: 1.5, g: true, s: '7'});\n\
           pm.expect(environment.n).to.equal(5);\n\
           pm.expect(pm.environment.toJSON().values[0]).to.eql({key: 'n', value: 5, type: 'number'});\n\
         });\n\
         pm.test('replaceIn', () => pm.expect(pm.variables.replaceIn('{{n}}|{{b}}|{{o}}|{{c}}')).to.equal('5|false|{\"a\":[1]}|1.5'));\n\
         pm.environment.set('n', '42');\n\
         pm.environment.set('s', 9);\n\
         pm.environment.set('b', 0);\n\
         pm.environment.set('plain', 3);\n\
         pm.environment.set('o', 'text');\n\
         pm.environment.set('new', {k: [true]});\n\
         pm.environment.set('typed', '2.50', 'number');\n\
         pm.collectionVariables.set('c', 'x', 'ANY');\n\
         pm.globals.set('g', 'yes', 'nonsense');\n\
         pm.variables.set('l', 2.5);\n\
         pm.test('set', () => {\n\
           pm.expect(pm.environment.get('n')).to.equal(42);\n\
           pm.expect(pm.environment.get('s')).to.equal('9');\n\
           pm.expect(pm.environment.get('b')).to.equal(false);\n\
           pm.expect(pm.environment.get('plain')).to.equal(3);\n\
           pm.expect(pm.environment.get('typed')).to.equal(2.5);\n\
           pm.expect(pm.collectionVariables.get('c')).to.equal('x');\n\
           pm.expect(pm.globals.get('g')).to.equal(true);\n\
           pm.expect(pm.variables.get('l')).to.equal(2.5);\n\
         });",
    );
    j.variables = typed_scopes();
    let out = e.run(j);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.tests.iter().all(|t| t.passed), "{:?}", out.tests);
    let vars = &out.variables;
    let texts: Vec<(&str, &str)> = vars.environment.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(
        texts,
        [
            ("n", "42"),
            ("b", "false"),
            ("o", "text"),
            ("s", "9"),
            ("plain", "3"),
            ("new", r#"{"k":[true]}"#),
            ("typed", "2.5")
        ]
    );
    let types: Vec<(&str, VarType)> = vars.types.environment.iter().map(|(k, t)| (k.as_str(), *t)).collect();
    assert_eq!(
        types,
        [
            ("n", VarType::Number),
            ("b", VarType::Boolean),
            ("s", VarType::String),
            ("plain", VarType::Any),
            ("new", VarType::Any),
            ("typed", VarType::Number),
        ]
    );
    assert_eq!(vars.collection["c"], "x");
    assert!(vars.types.collection.is_empty());
    assert_eq!((vars.globals["g"].as_str(), vars.types.globals["g"]), ("true", VarType::Boolean));
    assert_eq!((vars.local["l"].as_str(), vars.types.local["l"]), ("2.5", VarType::Any));

    // The next job (the test script) gets the values back with their types.
    let mut j = post_job(
        "pm.test('typed', () => {\n\
           pm.expect(pm.environment.get('plain')).to.equal(3);\n\
           pm.expect(pm.environment.get('new')).to.eql({k: [true]});\n\
           pm.expect(pm.environment.get('typed')).to.equal(2.5);\n\
           pm.expect(pm.environment.get('s')).to.equal('9');\n\
           pm.expect(pm.variables.get('l')).to.equal(2.5);\n\
         });\n\
         pm.environment.unset('n');\n\
         pm.environment.set('new', 'plain again');",
        "{}",
    );
    j.variables = out.variables.clone();
    let out = e.run(j);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.tests[0].passed, "{:?}", out.tests);
    assert!(!out.variables.environment.contains_key("n"));
    assert!(!out.variables.types.environment.contains_key("n"));
    assert!(!out.variables.types.environment.contains_key("new"));
    assert_eq!(out.variables.types.environment.get("typed"), Some(&VarType::Number));
}

#[test]
fn legacy_setters_store_strings_like_postman() {
    use merpati_core::VarType;
    let e = BoaEngine::new();
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "postman.setEnvironmentVariable('n', 5);\n\
         postman.setGlobalVariable('t', true);\n\
         postman.setEnvironmentVariable('f', false);\n\
         postman.setEnvironmentVariable('z', 0);\n\
         pm.environment.set('p', 5);",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let v = &out.variables;
    assert_eq!((v.environment["n"].as_str(), v.types.environment.get("n")), ("5", None));
    assert_eq!((v.globals["t"].as_str(), v.types.globals.get("t")), ("true", None));
    assert_eq!((v.environment["f"].as_str(), v.types.environment.get("f")), ("false", Some(&VarType::Any)));
    assert_eq!((v.environment["z"].as_str(), v.types.environment.get("z")), ("0", Some(&VarType::Any)));
    assert_eq!((v.environment["p"].as_str(), v.types.environment.get("p")), ("5", Some(&VarType::Any)));
}

#[test]
fn large_console_output_and_unicode() {
    let e = BoaEngine::new();
    let out = e.run(job(ScriptEvent::PreRequest, "for (let i = 0; i < 2000; i++) console.log('línea', i, '😀');"));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.console.len(), 2000);
    assert_eq!(out.console[1999].message, "línea 1999 😀");
}

#[test]
fn warm_up_does_not_break_anything() {
    let e = BoaEngine::new();
    e.warm_up();
    let out = e.run(post_job("pm.test('t', () => pm.expect(1).to.equal(1));", "{}"));
    assert!(out.tests[0].passed);
}

#[test]
fn timeout_at_host_call_keeps_partial_results() {
    let e = BoaEngine::new();
    let mut j = job(
        ScriptEvent::PreRequest,
        "pm.environment.set('before', 'yes'); pm.test('done before', () => {}); while (true) { pm.variables.get('x'); }",
    );
    j.timeout = Duration::from_millis(100);
    let out = e.run(j);
    assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
    assert!(out.errors[0].message.contains("timed out"));
    assert_eq!(out.variables.environment.get("before").map(String::as_str), Some("yes"));
    assert_eq!(out.tests.len(), 1);
}

#[test]
fn error_messages_mention_lines() {
    let e = BoaEngine::new();
    let out = e.run(job(ScriptEvent::PreRequest, "let a = 1;\nlet b = ;\n"));
    assert_eq!(out.errors[0].message, "SyntaxError: unexpected token ';', primary expression at line 2, col 9");
    let out = e.run(job(ScriptEvent::PreRequest, "let b = ;"));
    assert_eq!(out.errors[0].message, "SyntaxError: unexpected token ';', primary expression at line 1, col 9");
    let out = e.run(job(ScriptEvent::PreRequest, "const x = null;\n\nx.foo;"));
    assert!(out.errors[0].message.starts_with("TypeError"), "{:?}", out.errors);
    assert!(out.errors[0].message.ends_with("(line 3)"), "{:?}", out.errors);
    let out = e.run(job(ScriptEvent::PreRequest, "function f() {\n  throw new Error('deep');\n}\nsetTimeout(f, 1);"));
    assert_eq!(out.errors[0].message, "Error: deep (line 2)");
    // Positions boa cannot attribute are left out rather than guessed.
    let out = e.run(job(ScriptEvent::PreRequest, "\nnotDefined;"));
    assert_eq!(out.errors[0].message, "ReferenceError: notDefined is not defined");
}

#[test]
fn web_globals_used_by_packages() {
    let e = BoaEngine::new();
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "const b = new Blob(['hé', new Uint8Array([33])], {type: 'Text/Plain'});\n\
         b.text().then((t) => pm.environment.set('blob', [b.size, b.type, t, b.slice(0, 1).size].join('|')));\n\
         const f = new File(['x'], 'a.txt');\n\
         const buf = new Uint8Array([1, 2, 3]).buffer.transfer(2);\n\
         pm.environment.set('misc', [f.name, f instanceof Blob, buf.byteLength, new Uint8Array(buf)[1]].join('|'));",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.variables.environment["blob"], "4|text/plain|hé!|1");
    assert_eq!(out.variables.environment["misc"], "a.txt|true|2|2");
}

#[test]
fn patched_api_objects_do_not_leak_into_later_jobs() {
    // One worker serves both jobs.
    let e = BoaEngine::with_config(EngineConfig { max_idle_workers: 1, ..Default::default() });
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "console.log = function () {};\n\
         pm.variables.get = () => 'hacked';\n\
         pm.execution.setNextRequest = () => {};\n\
         postman.setNextRequest = () => {};\n\
         pm.test.skip = () => {};\n\
         pm.helper = 1;\n\
         Object.getPrototypeOf(pm.environment).get = () => 'env-hacked';\n\
         Object.getPrototypeOf(pm.request.headers).get = () => 'hdr-hacked';\n\
         Buffer.prototype.toString = () => 'buf-hacked';\n\
         _.mixin({ leaked: () => 1 });\n\
         try { __mp.exec = (fn) => fn(); } catch (err) {}\n\
         try { Object.defineProperty(globalThis, '__mp', { value: {} }); } catch (err) {}\n\
         globalThis.__mp_install_chai_postman = function () {};\n\
         pm.expect(1).to.equal(1);\n\
         console.count();",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let mut j = post_job(
        "console.log([pm.variables.get('x'), pm.environment.get('x'), pm.request.headers.get('accept'),\n\
           Buffer.from('ok').toString(), typeof _.leaked, typeof pm.helper].join('|'));\n\
         console.count();\n\
         pm.test('chai-postman', () => pm.response.to.have.status(200));\n\
         pm.test.skip('skipped');\n\
         postman.setNextRequest('next');",
        "{}",
    );
    j.variables.environment.insert("x".into(), "env".into());
    let out = e.run(j);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let messages: Vec<&str> = out.console.iter().map(|c| c.message.as_str()).collect();
    assert_eq!(messages, ["env|env|*/*|ok|undefined|undefined", "default: 1"]);
    assert!(out.tests[0].passed, "{:?}", out.tests);
    assert!(out.tests[1].skipped, "{:?}", out.tests);
    assert_eq!(out.next_request, Some(merpati_core::script::NextRequest::Named("next".into())));
}

#[test]
fn patched_built_ins_neither_wipe_variables_nor_leak() {
    let e = BoaEngine::with_config(EngineConfig { max_idle_workers: 1, ..Default::default() });
    let mut j = job(
        ScriptEvent::PreRequest,
        "Map.prototype.forEach = function () {};\n\
         Object.defineProperty(Object.prototype, 'polluted', { value: 1 });\n\
         Object.freeze(Array.prototype);\n\
         pm.environment.set('set', 'yes');",
    );
    j.variables.environment.insert("kept".into(), "1".into());
    let out = e.run(j);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.variables.environment.get("kept").map(String::as_str), Some("1"));
    assert_eq!(out.variables.environment.get("set").map(String::as_str), Some("yes"));
    let mut j = job(
        ScriptEvent::PreRequest,
        "const seen = []; new Map([[1, 2]]).forEach((v) => seen.push(v));\n\
         console.log(seen.length, ({}).polluted, Object.isFrozen(Array.prototype));",
    );
    j.variables.environment.insert("kept".into(), "1".into());
    let out = e.run(j);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.console[0].message, "1 undefined false");
    assert_eq!(out.variables.environment.get("kept").map(String::as_str), Some("1"));
}

#[test]
fn tests_failing_with_null_or_undefined() {
    let e = BoaEngine::new();
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "pm.test('throw undefined', () => { throw undefined; });\n\
         pm.test('throw null', () => { throw null; });\n\
         pm.test('reject null', () => Promise.reject(null));\n\
         pm.test('done with null passes', (done) => done(null));",
    ));
    let results: Vec<(bool, Option<&str>)> = out.tests.iter().map(|t| (t.passed, t.error.as_deref())).collect();
    assert_eq!(results, [(false, Some("undefined")), (false, Some("null")), (false, Some("null")), (true, None)]);
}

#[test]
fn get_random_values_follows_web_crypto_limits() {
    let e = BoaEngine::new();
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "const names = [];\n\
         for (const a of [new Uint8Array(65537), new Float32Array(2), [1, 2]]) {\n\
           try { crypto.getRandomValues(a); names.push('none'); } catch (err) { names.push(err.name); }\n\
         }\n\
         const big = crypto.getRandomValues(new Uint8Array(65536));\n\
         const tail = big.subarray(65536 - 1024).some((b) => b !== 0);\n\
         console.log(names.join(','), tail, crypto.getRandomValues(new Uint32Array(4)).length);",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.console[0].message, "QuotaExceededError,TypeMismatchError,TypeMismatchError true 4");
}

#[test]
fn legacy_buffer_constructor() {
    let e = BoaEngine::new();
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "console.log(new Buffer('user:pass').toString('base64'), new Buffer('6869', 'hex').toString(),\n\
           new Buffer([1, 2]).length, new Buffer(3).length, Buffer.alloc(5, 'ab').toString(),\n\
           Buffer.from('hello').subarray(1, 3) instanceof Buffer);",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.console[0].message, "dXNlcjpwYXNz hi 2 3 ababa true");
}

#[test]
fn request_parts_replaced_with_plain_values() {
    let e = BoaEngine::new();
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "pm.test('kept', () => {});\n\
         pm.environment.set('kept', 'yes');\n\
         pm.request.headers = [{ key: 'X-Plain', value: '1' }];\n\
         pm.request.url.query = 'a=1&b=2';\n\
         pm.request.body = { mode: 'urlencoded', urlencoded: [] };\n\
         pm.request.body.urlencoded = [{ key: 'k', value: 'v' }];",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.request.headers, vec![KeyValue::new("X-Plain", "1")]);
    assert_eq!(out.request.url, "{{baseUrl}}/users/:id?a=1&b=2");
    assert_eq!(out.request.body.mode, BodyMode::UrlEncoded);
    assert_eq!(out.request.body.urlencoded, vec![KeyValue::new("k", "v")]);

    // A request that cannot be converted is sent unchanged; the rest of the
    // results survive.
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "pm.test('kept', () => {});\n\
         pm.environment.set('kept', 'yes');\n\
         pm.request.headers.add({ key: 'X-New', value: '1' });\n\
         pm.request.url.toString = () => { throw new Error('broken url'); };",
    ));
    assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
    assert!(out.errors[0].message.contains("broken url"), "{:?}", out.errors);
    assert_eq!(out.request, request());
    assert!(out.tests[0].passed);
    assert_eq!(out.variables.environment.get("kept").map(String::as_str), Some("yes"));
}

#[test]
fn timer_delays_beyond_32_bits_fire_at_once() {
    // Node runs such timers after 1 ms instead of never.
    let e = BoaEngine::new();
    let mut j = job(
        ScriptEvent::PreRequest,
        "setTimeout(() => pm.environment.set('inf', 'fired'), Infinity);\n\
         setTimeout(() => pm.environment.set('big', 'fired'), 3e9);",
    );
    j.timeout = Duration::from_secs(5);
    let t = Instant::now();
    let out = e.run(j);
    assert!(t.elapsed() < Duration::from_secs(2), "took {:?}", t.elapsed());
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.variables.environment.get("inf").map(String::as_str), Some("fired"));
    assert_eq!(out.variables.environment.get("big").map(String::as_str), Some("fired"));
}

#[test]
fn variable_named_proto_is_kept() {
    let e = BoaEngine::new();
    let mut j =
        job(ScriptEvent::PreRequest, "pm.environment.set('added', Object.keys(pm.environment.toObject()).join());");
    j.variables.environment.insert("__proto__".into(), "x".into());
    let out = e.run(j);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.variables.environment.get("__proto__").map(String::as_str), Some("x"));
    assert_eq!(out.variables.environment.get("added").map(String::as_str), Some("__proto__"));
}

#[test]
fn xml2json_swallows_errors_like_postman() {
    let e = BoaEngine::new();
    let out = e.run(post_job(
        "const parsed = xml2Json(responseBody);\n\
         pm.test('not xml', () => pm.expect(parsed).to.be.undefined);\n\
         console.log(xml2Json(''), JSON.stringify(xml2Json('<a>1</a>')));\n\
         require('xml2js').parseString('', (err, r) => console.log(err, r));",
        "{\"json\": true}",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.tests[0].passed, "{:?}", out.tests);
    let messages: Vec<&str> = out.console.iter().map(|c| c.message.as_str()).collect();
    assert_eq!(messages, ["null {\"a\":\"1\"}", "null null"]);
}

#[test]
fn url_search_params_use_form_encoding() {
    let e = BoaEngine::new();
    let out = e.run(job(
        ScriptEvent::PreRequest,
        "console.log(new URLSearchParams({ q: \"it's (ok)!~*\", s: 'a b' }).toString());",
    ));
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.console[0].message, "q=it%27s+%28ok%29%21%7E*&s=a+b");
}

#[test]
fn replace_in_substitutes_inside_objects_and_arrays() {
    let e = BoaEngine::new();
    let mut j = job(
        ScriptEvent::PreRequest,
        "const tpl = { a: '{{host}}', list: [1, '{{port}}', { deep: '{{host}}:{{port}}' }], when: new Date(0) };\n\
         const out = pm.variables.replaceIn(tpl);\n\
         console.log(JSON.stringify(out), tpl.a, out.when === tpl.when, JSON.stringify(pm.environment.replaceIn(['{{host}}'])));",
    );
    j.variables.environment.insert("host".into(), "example.com".into());
    j.variables.collection.insert("port".into(), "8080".into());
    let out = e.run(j);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(
        out.console[0].message,
        "{\"a\":\"example.com\",\"list\":[1,\"8080\",{\"deep\":\"example.com:8080\"}],\"when\":\"1970-01-01T00:00:00.000Z\"} \
         {{host}} true [\"example.com\"]"
    );
}
