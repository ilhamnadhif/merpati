//! `pm.require` of npm/JSR packages (served by a local esm.sh-like CDN) and
//! of the local Package Library.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use merpati_core::script::*;
use merpati_script::{BoaEngine, EngineConfig};
use serde_json::Value;

#[derive(Clone)]
enum Reply {
    Js(String),
    Status(u16, String),
    Redirect(String),
    Slow(Duration, String),
}

#[derive(Clone, Default)]
struct Cdn {
    routes: Arc<Mutex<HashMap<String, Reply>>>,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Cdn {
    fn set(&self, path: &str, reply: Reply) {
        self.routes.lock().unwrap().insert(path.to_string(), reply);
    }

    fn js(&self, path: &str, body: &str) {
        self.set(path, Reply::Js(body.to_string()));
    }

    fn hits(&self) -> usize {
        self.hits.lock().unwrap().len()
    }

    /// An esm.sh-style entry stub plus its bundle.
    fn package(&self, name_ver: &str, bundle: &str, has_default: bool) {
        let file = name_ver.split('@').find(|s| !s.is_empty()).unwrap_or("pkg").rsplit('/').next().unwrap().to_string();
        let bundle_path = format!("/{name_ver}/es2022/{file}.bundle.mjs");
        let mut stub = format!("/* esm.sh - {name_ver} */\nexport * from \"{bundle_path}\";\n");
        if has_default {
            stub.push_str(&format!("export {{ default }} from \"{bundle_path}\";\n"));
        }
        self.js(&format!("/{name_ver}?bundle&target=es2022"), &stub);
        self.js(&bundle_path, bundle);
    }
}

async fn serve(State(cdn): State<Cdn>, req: Request) -> Response {
    let key = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
    cdn.hits.lock().unwrap().push(key.clone());
    let reply = cdn.routes.lock().unwrap().get(&key).cloned();
    let js = |body: String| ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8")], body).into_response();
    match reply {
        Some(Reply::Js(body)) => js(body),
        Some(Reply::Status(code, body)) => (StatusCode::from_u16(code).unwrap(), body).into_response(),
        Some(Reply::Redirect(to)) => (StatusCode::FOUND, [(header::LOCATION, to)]).into_response(),
        Some(Reply::Slow(d, body)) => {
            tokio::time::sleep(d).await;
            js(body)
        }
        None => (StatusCode::NOT_FOUND, format!("not found: {key}")).into_response(),
    }
}

/// Starts the CDN on a background runtime; returns its base URL.
fn start_cdn(cdn: &Cdn) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    let state = cdn.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let app = axum::Router::new().fallback(serve).with_state(state);
            axum::serve(listener, app).await.unwrap();
        });
    });
    format!("http://{}", rx.recv().unwrap())
}

fn standard_cdn() -> (Cdn, String) {
    let cdn = Cdn::default();
    let base = start_cdn(&cdn);
    cdn.package(
        "greet@1.0.0",
        "import { shout } from \"./util.mjs\";\nexport function greet(n) { return shout('hello ' + n); }\nexport default { greet };\n",
        true,
    );
    cdn.js("/greet@1.0.0/es2022/util.mjs", "export const shout = (s) => s.toUpperCase();\n");
    cdn.package("named@2.0.0", "export const a = 1;\nexport function b() { return 2; }\n", false);
    cdn.package("onlydefault@1.0.0", "export default function day() { return 'dj'; }\n", true);
    cdn.js(
        "/jsr/@std/enc@1?bundle&target=es2022",
        "export * from \"/@jsr/std__enc@1.0.3/es2022/std__enc.bundle.mjs\";\n",
    );
    cdn.js(
        "/@jsr/std__enc@1.0.3/es2022/std__enc.bundle.mjs",
        "export function encodeHex(s) { return Array.from(new TextEncoder().encode(s)).map((b) => b.toString(16).padStart(2, '0')).join(''); }\n",
    );
    (cdn, base)
}

struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("packages")).unwrap();
        let cache = dir.path().join("cache");
        Workspace { _dir: dir, root, cache }
    }

    fn ctx(&self, base: &str) -> PackageContext {
        let mut c = PackageContext::for_workspace(&self.root, base);
        c.cache_dir = Some(self.cache.clone());
        c
    }

    fn lock(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.root.join("packages.lock.json")).unwrap()).unwrap()
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.root.join("packages").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }
}

fn job(code: &str, packages: Option<PackageContext>) -> ScriptJob {
    let mut j = ScriptJob::new(
        ScriptEvent::PreRequest,
        vec![ScriptSource { location: "request \"t\"".into(), code: code.into() }],
        ScriptRequest { method: "GET".into(), url: "https://x".into(), ..Default::default() },
    );
    j.packages = packages;
    j.timeout = Duration::from_secs(20);
    j
}

fn run_ok(engine: &BoaEngine, j: ScriptJob) -> ScriptOutcome {
    let out = engine.run(j);
    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    let failed: Vec<_> = out.tests.iter().filter(|t| !t.passed).map(|t| (&t.name, &t.error)).collect();
    assert!(failed.is_empty(), "failed tests: {failed:?}");
    out
}

fn first_error(out: &ScriptOutcome) -> String {
    out.errors.first().map(|e| e.message.clone()).unwrap_or_else(|| format!("no error; console: {:?}", out.console))
}

#[test]
fn npm_and_jsr_packages_load_and_are_pinned() {
    let (cdn, base) = standard_cdn();
    let ws = Workspace::new();
    let e = BoaEngine::new();
    let out = run_ok(
        &e,
        job(
            r#"
            const g = pm.require('npm:greet@1.0.0');
            pm.test('default with named exports', () => {
                pm.expect(g.greet('ada')).to.equal('HELLO ADA');
                pm.expect(require('npm:greet@1.0.0')).to.equal(g);
            });
            pm.test('named only gives the namespace', () => {
                const n = pm.require('npm:named@2.0.0');
                pm.expect(n.a).to.equal(1);
                pm.expect(n.b()).to.equal(2);
            });
            pm.test('default only gives the default', () => pm.expect(pm.require('npm:onlydefault@1.0.0')()).to.equal('dj'));
            pm.test('jsr', () => pm.expect(pm.require('jsr:@std/enc@1').encodeHex('hi')).to.equal('6869'));
            pm.test('built-ins still work', () => {
                pm.expect(require('lodash').chunk([1, 2, 3], 2)).to.have.lengthOf(2);
                pm.expect(() => require('fs')).to.throw(/Node.js built-in modules are not available/);
            });
            "#,
            Some(ws.ctx(&base)),
        ),
    );
    assert_eq!(out.tests.len(), 5);

    let lock = ws.lock();
    assert_eq!(lock["lockfileVersion"], 1);
    assert_eq!(lock["packages"]["npm:greet@1.0.0"], format!("{base}/greet@1.0.0?bundle&target=es2022"));
    assert_eq!(lock["packages"]["jsr:@std/enc@1"], format!("{base}/jsr/@std/enc@1?bundle&target=es2022"));
    let modules = lock["modules"].as_object().unwrap();
    assert_eq!(modules.len(), 9, "{modules:?}");
    assert!(modules.values().all(|v| v.as_str().unwrap().starts_with("sha256-")));
    assert!(modules.contains_key(&format!("{base}/greet@1.0.0/es2022/util.mjs")));
    let text = std::fs::read_to_string(ws.root.join("packages.lock.json")).unwrap();
    assert!(text.ends_with("}\n") && text.contains("\n  \"modules\": {\n"), "{text}");

    // The same worker reuses parsed modules; nothing is fetched again.
    let hits = cdn.hits();
    run_ok(
        &e,
        job(
            "pm.test('again', () => pm.expect(pm.require('npm:greet@1.0.0').greet('x')).to.equal('HELLO X'));",
            Some(ws.ctx(&base)),
        ),
    );
    assert_eq!(cdn.hits(), hits);

    // A new engine works offline from the cache.
    let mut offline = ws.ctx(&base);
    offline.allow_network = false;
    run_ok(
        &BoaEngine::new(),
        job(
            "pm.test('offline', () => pm.expect(pm.require('npm:greet@1.0.0').greet('y')).to.equal('HELLO Y'));",
            Some(offline),
        ),
    );
    assert_eq!(cdn.hits(), hits);
}

#[test]
fn offline_without_cache_fails_clearly() {
    let (cdn, base) = standard_cdn();
    let ws = Workspace::new();
    let mut ctx = ws.ctx(&base);
    ctx.allow_network = false;
    let out = BoaEngine::new().run(job("pm.require('npm:greet@1.0.0');", Some(ctx)));
    let msg = first_error(&out);
    assert!(msg.contains("Cannot load 'npm:greet@1.0.0'"), "{msg}");
    assert!(msg.contains("is not in the package cache") && msg.contains("network access is off"), "{msg}");
    assert_eq!(cdn.hits(), 0);
}

#[test]
fn integrity_pins_are_enforced() {
    let (cdn, base) = standard_cdn();
    let ws = Workspace::new();
    run_ok(&BoaEngine::new(), job("pm.require('npm:named@2.0.0');", Some(ws.ctx(&base))));

    // The registry now serves different content for a pinned URL.
    cdn.package("named@2.0.0", "export const a = 666;\nexport function b() { return 2; }\n", false);
    let fresh_cache = tempfile::tempdir().unwrap();
    let mut ctx = ws.ctx(&base);
    ctx.cache_dir = Some(fresh_cache.path().to_path_buf());
    let out = BoaEngine::new().run(job("pm.require('npm:named@2.0.0');", Some(ctx.clone())));
    let msg = first_error(&out);
    assert!(msg.contains("integrity mismatch for") && msg.contains("packages.lock.json pins sha256-"), "{msg}");
    assert!(msg.contains("es2022/named.bundle.mjs"), "{msg}");

    // A pin edited by hand is checked against modules already in memory too.
    let e = BoaEngine::new();
    run_ok(&e, job("pm.require('npm:greet@1.0.0');", Some(ws.ctx(&base))));
    let lock_path = ws.root.join("packages.lock.json");
    let mut lock = ws.lock();
    lock["modules"][format!("{base}/greet@1.0.0/es2022/util.mjs")] = Value::String("sha256-AAAA".into());
    std::fs::write(&lock_path, serde_json::to_string_pretty(&lock).unwrap()).unwrap();
    let out = e.run(job("pm.require('npm:greet@1.0.0');", Some(ws.ctx(&base))));
    assert!(first_error(&out).contains("integrity mismatch"), "{:?}", out.errors);

    // Without a lockfile nothing is pinned.
    let mut unpinned = ws.ctx(&base);
    unpinned.lockfile_path = None;
    run_ok(&BoaEngine::new(), job("pm.require('npm:named@2.0.0');", Some(unpinned)));
}

#[test]
fn registry_policy_and_limits() {
    let (cdn, base) = standard_cdn();
    let ws = Workspace::new();
    let other = base.replace("127.0.0.1", "localhost");
    cdn.set("/away@1.0.0?bundle&target=es2022", Reply::Redirect(format!("{other}/greet@1.0.0/es2022/util.mjs")));
    cdn.set("/moved@1.0.0?bundle&target=es2022", Reply::Redirect("/greet@1.0.0/es2022/util.mjs".into()));
    cdn.js(
        "/crossimport@1.0.0?bundle&target=es2022",
        &format!("export * from \"{other}/greet@1.0.0/es2022/util.mjs\";\n"),
    );
    cdn.js("/usesfs@1.0.0?bundle&target=es2022", "import \"/node/fs.mjs\";\nexport default 1;\n");
    cdn.js(
        "/usesbuffer@1.0.0?bundle&target=es2022",
        "import { Buffer } from \"/node/buffer.mjs\";\nexport const ok = Buffer.from('a').length;\n",
    );
    cdn.js("/node/buffer.mjs", "export const Buffer = globalThis.Buffer;\n");
    cdn.js("/big@1.0.0?bundle&target=es2022", &format!("export const s = '{}';\n", "x".repeat(70 * 1024)));
    cdn.js("/wide@1.0.0?bundle&target=es2022", "export * from './w1.mjs';\nexport * from './w2.mjs';\n");
    cdn.js("/w1.mjs", &format!("export const s1 = '{}';\n", "y".repeat(60 * 1024)));
    cdn.js("/w2.mjs", &format!("export const s2 = '{}';\n", "z".repeat(60 * 1024)));
    cdn.set("/missing@1.0.0?bundle&target=es2022", Reply::Status(404, "version 1.0.0 of 'missing' not found".into()));
    cdn.set("/slow@1.0.0?bundle&target=es2022", Reply::Slow(Duration::from_secs(3), "export default 1;".into()));

    let e = BoaEngine::with_config(EngineConfig {
        package_module_limit: 64 * 1024,
        package_graph_limit: 100 * 1024,
        package_fetch_timeout: Duration::from_millis(300),
        ..Default::default()
    });
    let attempt = |spec: &str| {
        let out = e.run(job(&format!("pm.require('{spec}');"), Some(ws.ctx(&base))));
        first_error(&out)
    };
    assert!(attempt("npm:away@1.0.0").contains("which is not on the package registry"));
    assert!(attempt("npm:crossimport@1.0.0").contains("which is not on the package registry"));
    let msg = attempt("npm:usesfs@1.0.0");
    assert!(msg.contains("needs the Node.js built-in module 'fs'"), "{msg}");
    let msg = attempt("npm:big@1.0.0");
    assert!(msg.contains("is larger than the 64 KB limit"), "{msg}");
    let msg = attempt("npm:wide@1.0.0");
    assert!(msg.contains("package is too large"), "{msg}");
    let msg = attempt("npm:missing@1.0.0");
    assert!(msg.contains("answered 404 Not Found: version 1.0.0 of 'missing' not found"), "{msg}");
    let msg = attempt("npm:slow@1.0.0");
    assert!(msg.contains("timed out after 300 ms"), "{msg}");
    assert!(attempt("npm:bad name@1").contains("invalid package specifier"));
    // Allowed: redirects within the registry and polyfills that work.
    run_ok(
        &e,
        job(
            "pm.test('moved', () => pm.expect(pm.require('npm:moved@1.0.0').shout('a')).to.equal('A'));",
            Some(ws.ctx(&base)),
        ),
    );
    run_ok(
        &e,
        job(
            "pm.test('buffer', () => pm.expect(pm.require('npm:usesbuffer@1.0.0').ok).to.equal(1));",
            Some(ws.ctx(&base)),
        ),
    );
    // Failures were not pinned.
    let lock = ws.lock();
    let modules = lock["modules"].as_object().unwrap();
    assert!(!modules.keys().any(|k| k.contains("usesfs") || k.contains("big@") || k.contains("wide")), "{modules:?}");
}

#[test]
fn broken_packages_report_errors_and_can_be_fixed() {
    let (cdn, base) = standard_cdn();
    let ws = Workspace::new();
    cdn.package("broken@1.0.0", "export const = ;\n", false);
    cdn.package("throws@1.0.0", "throw new Error('init failed');\n", false);
    let e = BoaEngine::new();
    let out = e.run(job("pm.require('npm:broken@1.0.0');", Some(ws.ctx(&base))));
    let msg = first_error(&out);
    assert!(msg.starts_with("SyntaxError") && msg.contains("broken@1.0.0/es2022/broken.bundle.mjs"), "{msg}");
    let out = e.run(job("pm.test('caught', () => pm.require('npm:throws@1.0.0'));", Some(ws.ctx(&base))));
    assert!(out.tests[0].error.as_deref().unwrap_or_default().contains("init failed"), "{:?}", out.tests);

    cdn.package("broken@1.0.0", "export const fixed = true;\n", false);
    run_ok(
        &e,
        job("pm.test('fixed', () => pm.expect(pm.require('npm:broken@1.0.0').fixed).to.be.true);", Some(ws.ctx(&base))),
    );
}

#[test]
fn package_library_files() {
    let (_cdn, base) = standard_cdn();
    let ws = Workspace::new();
    ws.write(
        "@team/utils.js",
        "const _ = require('lodash');\nconst helper = require('./helper');\nlet loads = (globalThis.__loads || 0) + 1;\n\
         module.exports = { double: (x) => x * 2, chunk: (a) => _.chunk(a, 2), helper: helper.name, file: __filename };\n",
    );
    ws.write("@team/helper.js", "exports.name = 'helper';\n");
    ws.write(
        "esm-tool.mjs",
        "import { double } from '@team/utils';\nimport _ from 'lodash';\nimport { greet } from 'npm:greet@1.0.0';\n\
         export const quad = (x) => double(double(x));\nexport const size = _.size([1, 2]);\nexport const hi = greet('esm');\n\
         export default 'esm-tool';\n",
    );
    ws.write("dates.js", "export function year(d) { return new Date(d).getUTCFullYear(); }\n");
    ws.write("counter/index.js", "let n = 0;\nmodule.exports = { next: () => ++n };\n");
    ws.write("uses-npm.cjs", "const g = require('npm:named@2.0.0');\nmodule.exports = () => g.b() + 1;\n");
    let e = BoaEngine::new();
    let code = r#"
        pm.test('commonjs', () => {
            const u = pm.require('@team/utils');
            pm.expect(u.double(2)).to.equal(4);
            pm.expect(u.chunk([1, 2, 3])).to.eql([[1, 2], [3]]);
            pm.expect(u.helper).to.equal('helper');
            pm.expect(u.file).to.match(/utils\.js$/);
            pm.expect(require('@team/utils')).to.equal(u);
        });
        pm.test('es module', () => {
            const t = pm.require('esm-tool');
            pm.expect(t.quad(2)).to.equal(8);
            pm.expect(t.size).to.equal(2);
            pm.expect(t.hi).to.equal('HELLO ESM');
            pm.expect(t.default).to.equal('esm-tool');
        });
        pm.test('sniffed es module', () => pm.expect(pm.require('dates').year('2024-05-01')).to.equal(2024));
        pm.test('index file and fresh state per job', () => pm.expect(pm.require('counter').next()).to.equal(1));
        pm.test('npm from a library file', () => pm.expect(pm.require('uses-npm')()).to.equal(3));
        pm.test('missing', () => pm.expect(() => pm.require('@team/nope')).to.throw(/Cannot find module '@team\/nope' \(looked in the package library/));
        pm.test('relative from a script', () => pm.expect(() => pm.require('./helper')).to.throw(/relative paths only work inside packages/));
        pm.test('unsupported still explained', () => pm.expect(() => require('cheerio')).to.throw(/not bundled with Merpati/));
    "#;
    run_ok(&e, job(code, Some(ws.ctx(&base))));
    // Edits are picked up by the next job, and module state starts over.
    ws.write("@team/helper.js", "exports.name = 'edited';\n");
    run_ok(
        &e,
        job(
            "pm.test('edited', () => pm.expect(pm.require('@team/utils').helper).to.equal('edited'));\
             pm.test('counter', () => pm.expect(pm.require('counter').next()).to.equal(1));",
            Some(ws.ctx(&base)),
        ),
    );
}

#[cfg(unix)]
#[test]
fn package_library_cannot_escape_its_directory() {
    let ws = Workspace::new();
    let outside = ws.root.join("secret.js");
    std::fs::write(&outside, "module.exports = 'secret';").unwrap();
    std::os::unix::fs::symlink(&outside, ws.root.join("packages/leak.js")).unwrap();
    let out = BoaEngine::new().run(job(
        "pm.test('leak', () => pm.expect(() => pm.require('leak')).to.throw(/outside the package library/));\
         pm.test('dots', () => pm.expect(() => pm.require('../secret')).to.throw(/relative paths only work inside packages/));",
        Some(ws.ctx("https://esm.sh")),
    ));
    assert!(out.tests.iter().all(|t| t.passed), "{:?}", out.tests);
}

#[test]
fn without_a_package_context_only_builtins_load() {
    let out = run_ok(
        &BoaEngine::new(),
        job(
            "pm.test('npm: maps to bundled lodash', () => pm.expect(pm.require('npm:lodash@4.17.21').chunk([1, 2], 1)).to.have.lengthOf(2));\
             pm.test('jsr not enabled', () => pm.expect(() => pm.require('jsr:@std/encoding@1')).to.throw(/not enabled for this run/));\
             pm.test('library not enabled', () => pm.expect(() => pm.require('@team/utils')).to.throw(\"Cannot find module '@team/utils'\"));",
            None,
        ),
    );
    assert_eq!(out.tests.len(), 3);
}

#[test]
fn dynamic_import_of_packages() {
    let (_cdn, base) = standard_cdn();
    let ws = Workspace::new();
    run_ok(
        &BoaEngine::new(),
        job(
            "const m = await import('npm:named@2.0.0');\npm.test('import()', () => pm.expect(m.b()).to.equal(2));",
            Some(ws.ctx(&base)),
        ),
    );
}

#[test]
fn concurrent_jobs_share_one_lockfile() {
    let (_cdn, base) = standard_cdn();
    let ws = Arc::new(Workspace::new());
    let e = BoaEngine::with_config(EngineConfig { max_idle_workers: 4, ..Default::default() });
    let specs = ["npm:greet@1.0.0", "npm:named@2.0.0", "npm:onlydefault@1.0.0", "jsr:@std/enc@1"];
    let handles: Vec<_> = specs
        .iter()
        .map(|spec| {
            let (e, ws, base, spec) = (e.clone(), ws.clone(), base.clone(), spec.to_string());
            std::thread::spawn(move || run_ok(&e, job(&format!("pm.require('{spec}');"), Some(ws.ctx(&base)))))
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let lock = ws.lock();
    assert_eq!(lock["packages"].as_object().unwrap().len(), 4);
    assert_eq!(lock["modules"].as_object().unwrap().len(), 9);
}

#[test]
fn packages_can_use_intl_and_streams_while_loading() {
    let (cdn, base) = standard_cdn();
    let ws = Workspace::new();
    // Both polyfills are first touched during module evaluation.
    cdn.package(
        "fmt@1.0.0",
        "const nf = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD' });\n\
         export const money = (n) => nf.format(n);\n\
         export const lines = (s) => Array.fromAsync(ReadableStream.from(s.split('\\n')));\n",
        false,
    );
    ws.write("dates.mjs", "const f = new Intl.DateTimeFormat('en-US', { timeZone: 'UTC', dateStyle: 'medium' });\nexport const day = (t) => f.format(t);\n");
    ws.write(
        "sorter.cjs",
        "const c = new Intl.Collator('en', { numeric: true });\nmodule.exports = (a) => a.slice().sort(c.compare);\n",
    );
    let e = BoaEngine::new();
    let code = r#"
        const { money, lines } = pm.require('npm:fmt@1.0.0');
        pm.test('remote module', async () => {
            pm.expect(money(1234.5)).to.equal('$1,234.50');
            pm.expect(await lines('a\nb')).to.eql(['a', 'b']);
        });
        pm.test('library files', () => {
            pm.expect(pm.require('dates').day(Date.UTC(2026, 9, 23))).to.equal('Oct 23, 2026');
            pm.expect(pm.require('sorter')(['x10', 'x9'])).to.eql(['x9', 'x10']);
        });
    "#;
    run_ok(&e, job(code, Some(ws.ctx(&base))));
    // Cached modules keep working in later jobs.
    run_ok(&e, job(code, Some(ws.ctx(&base))));
}

/// Against the real esm.sh: `cargo test -p merpati-script --test packages -- --ignored`.
#[test]
#[ignore = "needs network access to esm.sh"]
fn real_esm_sh() {
    let ws = Workspace::new();
    let ctx = ws.ctx(DEFAULT_PACKAGE_REGISTRY);
    let code = r#"
        pm.test('lodash-es', () => {
            const _ = pm.require('npm:lodash-es@4.17.21');
            pm.expect(_.chunk([1, 2, 3], 2)).to.eql([[1, 2], [3]]);
            pm.expect(_.kebabCase('Hello World')).to.equal('hello-world');
        });
        pm.test('uuid', () => {
            const { v4, validate } = pm.require('npm:uuid@9.0.1');
            pm.expect(validate(v4())).to.be.true;
        });
        pm.test('dayjs', () => {
            const dayjs = pm.require('npm:dayjs@1.11.13');
            pm.expect(dayjs('2024-02-29').add(1, 'year').format('YYYY-MM-DD')).to.equal('2025-02-28');
        });
        pm.test('jsr @std/encoding', () => {
            const enc = pm.require('jsr:@std/encoding@1');
            pm.expect(enc.encodeBase64('hello')).to.equal('aGVsbG8=');
            pm.expect(enc.encodeHex('hi')).to.equal('6869');
        });
        pm.test('node built-ins refused', () => pm.expect(() => pm.require('npm:fs-extra@11.2.0')).to.throw(/'fs'/));
    "#;
    let t = std::time::Instant::now();
    let out = BoaEngine::new().run(job(code, Some(ctx.clone())));
    println!("first run (network): {:?}", t.elapsed());
    for t in &out.tests {
        println!("{} {} {:?}", if t.passed { "PASS" } else { "FAIL" }, t.name, t.error);
    }
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.tests.iter().all(|t| t.passed));
    let mut offline = ctx;
    offline.allow_network = false;
    let t = std::time::Instant::now();
    let out = BoaEngine::new().run(job(code, Some(offline)));
    println!("second run (offline cache): {:?}", t.elapsed());
    assert!(out.tests.iter().all(|t| t.passed), "{:?}", out.tests);
    println!("{}", std::fs::read_to_string(ws.root.join("packages.lock.json")).unwrap());
}

/// Packages that need `Intl` or Web Streams, against the real esm.sh/JSR:
/// `cargo test -p merpati-script --test packages -- --ignored`.
#[test]
#[ignore = "needs network access to esm.sh"]
fn real_intl_and_streams_packages() {
    let ws = Workspace::new();
    let ctx = ws.ctx(DEFAULT_PACKAGE_REGISTRY);
    let code = r#"
        const d = new Date(Date.UTC(2026, 9, 23, 21, 5, 3));
        pm.test('jsr @std/assert', () => {
            const { assertEquals, assertThrows, assertObjectMatch, AssertionError } = pm.require('jsr:@std/assert@1.0.19');
            assertEquals({ a: [1, 2], b: 'x' }, { a: [1, 2], b: 'x' });
            assertObjectMatch({ a: 1, b: 2 }, { a: 1 });
            assertThrows(() => { throw new TypeError('x'); }, TypeError);
            let err;
            try { assertEquals({ a: 1, b: [1, 2, 3] }, { a: 2, b: [1, 2, 4] }); } catch (e) { err = e; }
            pm.expect(err).to.be.instanceOf(AssertionError);
            pm.expect(err.message).to.include('Values are not equal').and.include('"a": 2');
        });
        pm.test('jsr @std/fmt', () => {
            const { format } = pm.require('jsr:@std/fmt@1.0.10/bytes');
            pm.expect([format(1337), format(1337, { locale: 'de' }), format(1e9, { binary: true })]).to.eql(['1.34 kB', '1,34 kB', '954 MiB']);
            const duration = pm.require('jsr:@std/fmt@1.0.10/duration');
            pm.expect(duration.format(99674, { ignoreZero: true })).to.equal('1m 39s 674ms');
        });
        pm.test('date-fns', () => {
            const { intlFormat, intlFormatDistance, formatDistance, addDays } = pm.require('npm:date-fns@4.1.0');
            pm.expect(intlFormat(d, { year: 'numeric', month: 'long', day: 'numeric', timeZone: 'UTC' }, { locale: 'en-US' })).to.equal('October 23, 2026');
            pm.expect(intlFormatDistance(addDays(d, -1), d)).to.equal('yesterday');
            pm.expect(intlFormatDistance(addDays(d, 3), d, { numeric: 'always' })).to.equal('in 3 days');
            pm.expect(formatDistance(d, addDays(d, 3))).to.equal('3 days');
        });
        pm.test('pretty-bytes', () => {
            const prettyBytes = pm.require('npm:pretty-bytes@7.0.1');
            pm.expect([prettyBytes(1337), prettyBytes(1337, { locale: 'de' }), prettyBytes(1337000, { locale: 'en' })]).to.eql(['1.34 kB', '1,34 kB', '1.34 MB']);
        });
        pm.test('dinero.js, numeral, currency.js', () => {
            const Dinero = pm.require('npm:dinero.js@1.9.1');
            pm.expect(Dinero({ amount: 500050, currency: 'EUR' }).toFormat('$0,0.00')).to.equal('€5,000.50');
            pm.expect(Dinero({ amount: 500050, currency: 'EUR' }).setLocale('id-ID').toFormat('$0,0.00')).to.equal('€5.000,50');
            pm.expect(pm.require('npm:numeral@2.0.6')(1234.567).format('$0,0.00')).to.equal('$1,234.57');
            pm.expect(pm.require('npm:currency.js@2.0.4')(1234.5).add(0.1).format()).to.equal('$1,234.60');
        });
        pm.test('luxon', () => {
            const { DateTime } = pm.require('npm:luxon@3.7.2');
            const dt = DateTime.fromISO('2026-10-23T21:05:03Z', { zone: 'utc' });
            pm.expect(dt.toFormat('yyyy LLL dd HH:mm')).to.equal('2026 Oct 23 21:05');
            pm.expect(dt.toLocaleString(DateTime.DATETIME_MED)).to.equal('Oct 23, 2026, 9:05 PM');
            pm.expect(dt.toRelative({ base: dt.plus({ days: 2 }) })).to.equal('2 days ago');
            pm.expect(dt.setZone('America/New_York').toISO()).to.equal('2026-10-23T17:05:03.000-04:00');
        });
        pm.test('intl-messageformat', () => {
            const { IntlMessageFormat } = pm.require('npm:intl-messageformat@10.7.16');
            const msg = new IntlMessageFormat('{n, plural, one {# message} other {# messages}} worth {v, number, ::currency/USD}', 'en-US');
            pm.expect(msg.format({ n: 1234, v: 5.5 })).to.equal('1,234 messages worth $5.50');
        });
        pm.test('jsr @std/streams', async () => {
            const { TextLineStream, toText, concatReadableStreams, toTransformStream, Buffer } = pm.require('jsr:@std/streams@1.0.13');
            pm.expect(await Array.fromAsync(ReadableStream.from(['a\nb', 'c\r\nd\n']).pipeThrough(new TextLineStream()))).to.eql(['a', 'bc', 'd']);
            pm.expect(await toText(ReadableStream.from(['x', 'y']).pipeThrough(new TextEncoderStream()))).to.equal('xy');
            pm.expect(await Array.fromAsync(concatReadableStreams(ReadableStream.from([1]), ReadableStream.from([2])))).to.eql([1, 2]);
            const doubled = ReadableStream.from([1, 2]).pipeThrough(toTransformStream(async function* (src) { for await (const x of src) yield x * 2; }));
            pm.expect(await Array.fromAsync(doubled)).to.eql([2, 4]);
            const buf = new Buffer();
            const w = buf.writable.getWriter();
            await w.write(new TextEncoder().encode('hello bytes'));
            w.releaseLock();
            pm.expect(await toText(buf.readable)).to.equal('hello bytes');
        });
        pm.test('jsr @std/csv CsvParseStream', async () => {
            const { parse, CsvParseStream } = pm.require('jsr:@std/csv@1.0.6');
            pm.expect(parse('a,b\n1,2\n', { skipFirstRow: true })).to.eql([{ a: '1', b: '2' }]);
            const rows = await Array.fromAsync(ReadableStream.from(['name,age\nAda,36\n', 'Linus,5', '4\n']).pipeThrough(new CsvParseStream({ skipFirstRow: true })));
            pm.expect(rows).to.eql([{ name: 'Ada', age: '36' }, { name: 'Linus', age: '54' }]);
        });
        pm.test('eventsource-parser stream', async () => {
            const { EventSourceParserStream } = pm.require('npm:eventsource-parser@3.0.6/stream');
            const src = ReadableStream.from(['data: hel', 'lo\n\nevent: ping\ndata: {"a":1}\n\n']).pipeThrough(new EventSourceParserStream());
            pm.expect((await Array.fromAsync(src)).map((e) => [e.event || 'message', e.data])).to.eql([['message', 'hello'], ['ping', '{"a":1}']]);
        });
    "#;
    let t = std::time::Instant::now();
    let out = BoaEngine::new().run(job(code, Some(ctx.clone())));
    println!("first run (network): {:?}", t.elapsed());
    for t in &out.tests {
        println!("{} {} {:?}", if t.passed { "PASS" } else { "FAIL" }, t.name, t.error);
    }
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.tests.iter().all(|t| t.passed));
    assert_eq!(out.tests.len(), 10);
    let mut offline = ctx;
    offline.allow_network = false;
    let out = BoaEngine::new().run(job(code, Some(offline)));
    assert!(out.tests.iter().all(|t| t.passed), "{:?}", out.tests);
}
