//! newman options of `merpati run` (FR-L1): `--bail` modifiers,
//! `-x/--suppress-exit-code`, `--cookie-jar`/`--export-cookie-jar`,
//! `--ssl-client-*` against a mutual TLS server, `--color`,
//! `--disable-unicode`, `--export-collection`, and `mock --fail-with`.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use axum::Router;
use axum::extract::Path as UrlPath;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BIN: &str = env!("CARGO_BIN_EXE_merpati");

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

/// `/status/{code}`, `/set-cookie` (sets `sid=abc`) and `/cookies` (echoes
/// the Cookie header).
async fn server() -> String {
    let app = Router::new()
        .route(
            "/status/{code}",
            get(|UrlPath(code): UrlPath<u16>| async move { StatusCode::from_u16(code).unwrap_or(StatusCode::OK) }),
        )
        .route("/set-cookie", get(|| async { ([(header::SET_COOKIE, "sid=abc; Path=/")], "set") }))
        .route(
            "/cookies",
            get(|h: HeaderMap| async move {
                h.get(header::COOKIE).and_then(|v| v.to_str().ok()).unwrap_or("none").to_string().into_response()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A Postman collection of `(name, url, test script lines)` requests;
/// requests whose name contains `/` go into that folder.
fn collection(dir: &Path, name: &str, requests: &[(&str, &str, &[&str])], variables: Value) -> PathBuf {
    let mut items: Vec<Value> = Vec::new();
    for (req, url, script) in requests {
        let item = json!({
            "name": req.rsplit('/').next().unwrap(),
            "event": [{"listen": "test", "script": {"type": "text/javascript", "exec": script}}],
            "request": {"method": "GET", "header": [], "url": url},
        });
        match req.split_once('/') {
            Some((folder, _)) => match items.iter_mut().find(|i| i["name"] == folder) {
                Some(f) => f["item"].as_array_mut().unwrap().push(item),
                None => items.push(json!({"name": folder, "item": [item]})),
            },
            None => items.push(item),
        }
    }
    let c = json!({
        "info": {"name": name, "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
        "variable": variables,
        "item": items,
    });
    let path = dir.join(format!("{name}.postman_collection.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&c).unwrap()).unwrap();
    path
}

const STATUS_200: &[&str] = &["pm.test('status 200', () => pm.response.to.have.status(200));"];

/// `Fails` (500) then `Passes` (200), both testing for 200.
fn failing_collection(dir: &Path) -> PathBuf {
    collection(
        dir,
        "Bail",
        &[("Fails", "{{base}}/status/500", STATUS_200), ("Passes", "{{base}}/status/200", STATUS_200)],
        json!([]),
    )
}

fn executed(dir: &Path) -> u64 {
    let report: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap()).unwrap();
    report["summary"]["executed"].as_u64().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn bail_modifiers_and_suppressed_exit_code() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let fx = failing_collection(dir.path());
    let fx = fx.to_str().unwrap();
    let base_var = format!("base={base}");
    let report = ["-r", "json", "--reporter-json-export", "report.json", "--env-var", &base_var];
    for (bail, runs) in [
        (vec![], 2),
        // A plain `--bail` right before the collection does not swallow it.
        (vec!["--bail"], 1),
        (vec!["--bail", "failure"], 1),
        (vec!["--bail=failure"], 1),
        (vec!["--bail", "failure,folder"], 1),
        // `folder` alone does not stop at failures.
        (vec!["--bail", "folder"], 2),
    ] {
        let mut args = vec!["run"];
        args.extend(&bail);
        args.push(fx);
        args.extend(report);
        let out = merpati(&args, dir.path()).await;
        assert_eq!(out.status.code(), Some(1), "{bail:?}: {}", text(&out));
        assert_eq!(executed(dir.path()), runs, "{bail:?}");
    }
    let out = merpati(&["run", fx, "--bail"], dir.path()).await;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    for args in [vec!["run", "--bail=nope", fx], vec!["run", "--bail", "nope", fx]] {
        let out = merpati(&args, dir.path()).await;
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out));
    }
    // An unknown folder stops the run before anything is sent.
    let out = merpati(&["run", "--bail", "folder", fx, "--folder", "Nope", "--env-var", &base_var], dir.path()).await;
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("\"Nope\" not found"), "{}", text(&out));

    // -x: failures exit 0, usage and load errors still 2.
    let mut args = vec!["run", fx, "-x"];
    args.extend(report);
    let out = merpati(&args, dir.path()).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(executed(dir.path()), 2);
    let out = merpati(&["run", "--suppress-exit-code", "missing.json"], dir.path()).await;
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
}

#[tokio::test(flavor = "multi_thread")]
async fn cookie_jar_is_read_only_and_exported_for_newman() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let fx = collection(
        dir.path(),
        "Cookies",
        &[
            ("Set", "{{base}}/set-cookie", &[]),
            ("Echo", "{{base}}/cookies", &["console.log('sent cookies: ' + pm.response.text());"]),
        ],
        json!([]),
    );
    let fx = fx.to_str().unwrap();
    let base_var = format!("base={base}");
    // A jar as `newman run --export-cookie-jar` writes it.
    let newman_jar = json!({
        "version": "tough-cookie@4.1.3",
        "storeType": "MemoryCookieStore",
        "rejectPublicSuffixes": true,
        "cookies": [{"key": "pre", "value": "1", "domain": "127.0.0.1", "path": "/", "hostOnly": true,
                     "creation": "2026-01-01T00:00:00.000Z", "lastAccessed": "2026-01-01T00:00:00.000Z"}]
    })
    .to_string();
    std::fs::write(dir.path().join("in.json"), &newman_jar).unwrap();
    let out = merpati(
        &["run", fx, "--env-var", &base_var, "--cookie-jar", "in.json", "--export-cookie-jar", "out/jar.json"],
        dir.path(),
    )
    .await;
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    let sent = t.lines().find(|l| l.contains("sent cookies:")).unwrap_or_default();
    assert!(sent.contains("pre=1") && sent.contains("sid=abc"), "{t}");
    assert_eq!(std::fs::read_to_string(dir.path().join("in.json")).unwrap(), newman_jar, "--cookie-jar is only read");

    let exported: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("out/jar.json")).unwrap()).unwrap();
    assert_eq!(exported["storeType"], "MemoryCookieStore");
    assert!(exported["version"].as_str().unwrap().starts_with("tough-cookie@"));
    let cookies = exported["cookies"].as_array().unwrap();
    let sid = cookies.iter().find(|c| c["key"] == "sid").expect("response cookie exported");
    assert_eq!((sid["value"].as_str(), sid["domain"].as_str()), (Some("abc"), Some("127.0.0.1")));
    assert_eq!((sid["path"].as_str(), sid["hostOnly"].as_bool()), (Some("/"), Some(true)));
    assert!(sid["creation"].is_string() && sid["lastAccessed"].is_string());
    assert!(cookies.iter().any(|c| c["key"] == "pre"));

    // The export reads back; reading and exporting the same file persists it.
    let out = merpati(
        &[
            "run",
            fx,
            "--folder",
            "Echo",
            "--env-var",
            &base_var,
            "--cookie-jar",
            "out/jar.json",
            "--export-cookie-jar",
            "out/jar.json",
        ],
        dir.path(),
    )
    .await;
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    let sent = t.lines().find(|l| l.contains("sent cookies:")).unwrap_or_default();
    assert!(sent.contains("pre=1") && sent.contains("sid=abc"), "{t}");

    // A Merpati jar (the app's cookies.json) is read and left alone too.
    let merpati_jar = dir.path().join("merpati-cookies.json");
    let jar = merpati_http::CookieJar::open(&merpati_jar);
    jar.set("127.0.0.1", "app=1; Path=/").unwrap();
    jar.save().unwrap();
    let before = std::fs::read(&merpati_jar).unwrap();
    let out =
        merpati(&["run", fx, "--env-var", &base_var, "--cookie-jar", merpati_jar.to_str().unwrap()], dir.path()).await;
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(t.contains("app=1"), "{t}");
    assert_eq!(std::fs::read(&merpati_jar).unwrap(), before);

    std::fs::write(dir.path().join("bad.json"), "{\"cookies\": 1}").unwrap();
    for jar in ["bad.json", "missing.json"] {
        let out = merpati(&["run", fx, "--env-var", &base_var, "--cookie-jar", jar], dir.path()).await;
        assert_eq!(out.status.code(), Some(2), "{jar}: {}", text(&out));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn export_collection_keeps_final_collection_variables() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let script: &[&str] = &[
        "pm.collectionVariables.set('counter', String(Number(pm.collectionVariables.get('counter')) + 1));",
        "pm.collectionVariables.set('added', 'yes');",
        "pm.collectionVariables.unset('gone');",
    ];
    let fx = collection(
        dir.path(),
        "Export",
        &[("Folder/Count", "{{base}}/status/200", script)],
        json!([
            {"key": "counter", "value": "1"},
            {"key": "apiKey", "value": "sk-1", "type": "secret"},
            {"key": "gone", "value": "x"}
        ]),
    );
    let base_var = format!("base={base}");
    let out = merpati(
        &[
            "run",
            fx.to_str().unwrap(),
            "-n",
            "2",
            "-k",
            "--timeout-request",
            "5000",
            "--env-var",
            &base_var,
            "--export-collection",
            "out/collection.json",
        ],
        dir.path(),
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let exported: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("out/collection.json")).unwrap()).unwrap();
    assert_eq!(exported["info"]["name"], "Export");
    assert!(exported["info"]["schema"].as_str().unwrap().contains("v2.1.0"));
    let vars: Vec<(String, String)> = exported["variable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["key"].as_str().unwrap().to_string(), v["value"].as_str().unwrap().to_string()))
        .collect();
    let pairs = |p: &[(&str, &str)]| p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
    // Values at the end of the run; secrets kept as in --export-environment.
    assert_eq!(vars, pairs(&[("counter", "3"), ("apiKey", "sk-1"), ("added", "yes")]));
    let request = &exported["item"][0]["item"][0];
    assert_eq!(request["name"], "Count");
    // Run-wide overrides (-k, --timeout-request) are not written into requests.
    assert!(request.get("protocolProfileBehavior").is_none(), "{request}");
    assert!(request["request"]["url"]["raw"].as_str().unwrap().starts_with("{{base}}"), "{request}");
}

#[tokio::test(flavor = "multi_thread")]
async fn color_and_ascii_output() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let fx = collection(
        dir.path(),
        "Output",
        &[("Folder/Ok", "{{base}}/status/200", &["pm.test('ok', () => pm.response.to.have.status(200));"])],
        json!([]),
    );
    let fx = fx.to_str().unwrap();
    let base_var = format!("base={base}");
    let run = |extra: &'static [&'static str]| {
        let mut args = vec!["run", fx, "--env-var", &base_var, "--verbose"];
        args.extend(extra);
        let dir = dir.path().to_path_buf();
        let args: Vec<String> = args.into_iter().map(str::to_string).collect();
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            merpati(&args, &dir).await
        }
    };
    let out = run(&["--disable-unicode"]).await;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(stdout.is_ascii(), "{stdout}");
    for want in ["+ Folder", "-> Ok", "Pass ok", "  + request GET", "  | ", "+---------------------------+"] {
        assert!(stdout.contains(want), "{want}: {stdout}");
    }
    let unicode = String::from_utf8_lossy(&run(&[]).await.stdout).to_string();
    assert!(unicode.contains("✓ ok") && unicode.contains("┌ request GET"), "{unicode}");

    // NO_COLOR is set: auto and off print no colors, on does.
    for (args, colored) in [(&[][..], false), (&["--color", "off"][..], false), (&["--color=on"][..], true)] {
        let out = run(args).await;
        assert_eq!(out.status.code(), Some(0), "{}", text(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout).contains("\x1b["), colored, "{args:?}");
    }
    for bad in [&["--color", "always"][..], &["--color", "on", "--no-color"][..]] {
        assert_eq!(run(bad).await.status.code(), Some(2), "{bad:?}");
    }
}

/// Declared Postman types reach scripts, and the exports keep the types
/// scripts gave values.
#[tokio::test(flavor = "multi_thread")]
async fn typed_variables_reach_scripts_and_exports() {
    let base = server().await;
    let dir = tempfile::tempdir().unwrap();
    let script: &[&str] = &[
        "pm.test('typed', () => {",
        "    pm.expect(pm.collectionVariables.get('n')).to.equal(5);",
        "    pm.expect(pm.environment.get('flag')).to.equal(true);",
        "    pm.expect(pm.globals.get('g')).to.equal(1.5);",
        "});",
        "pm.environment.set('count', pm.collectionVariables.get('n') + 1);",
        "pm.globals.set('g', pm.globals.get('g') * 2);",
        "pm.collectionVariables.set('n', pm.collectionVariables.get('n') * 10);",
    ];
    let fx = collection(
        dir.path(),
        "Typed",
        &[("Check", "{{base}}/status/200", script)],
        json!([{"key": "n", "value": 5, "type": "number"}]),
    );
    let env = dir.path().join("env.json");
    let env_json = json!({"name": "Typed", "values": [
        {"key": "base", "value": base, "enabled": true},
        {"key": "flag", "value": true, "type": "boolean", "enabled": true}
    ]});
    std::fs::write(&env, env_json.to_string()).unwrap();
    let globals = dir.path().join("globals.json");
    let globals_json = json!({"name": "Globals", "_postman_variable_scope": "globals", "values": [
        {"key": "g", "value": 1.5, "type": "number", "enabled": true}
    ]});
    std::fs::write(&globals, globals_json.to_string()).unwrap();
    let out = merpati(
        &[
            "run",
            fx.to_str().unwrap(),
            "-e",
            env.to_str().unwrap(),
            "-g",
            globals.to_str().unwrap(),
            "--export-environment",
            "out/env.json",
            "--export-globals",
            "out/globals.json",
            "--export-collection",
            "out/collection.json",
        ],
        dir.path(),
    )
    .await;
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(t.contains("✓ typed"), "{t}");
    let read = |name: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("out").join(name)).unwrap()).unwrap()
    };
    // `{"value", "type"}` of a variable in an export.
    let var = |v: &Value, list: &str, key: &str| {
        let var = v[list].as_array().unwrap().iter().find(|x| x["key"] == key).cloned().unwrap_or_default();
        json!({"value": var["value"], "type": var["type"]})
    };
    let env = read("env.json");
    // A new variable set to a number is Postman's `any` and stays a number.
    assert_eq!(var(&env, "values", "count"), json!({"value": 6, "type": "default"}), "{env}");
    assert_eq!(var(&env, "values", "flag"), json!({"value": true, "type": "boolean"}), "{env}");
    assert_eq!(var(&env, "values", "base")["type"], "default", "{env}");
    // Declared types survive script changes.
    assert_eq!(var(&read("globals.json"), "values", "g"), json!({"value": 3, "type": "number"}));
    assert_eq!(var(&read("collection.json"), "variable", "n"), json!({"value": 50, "type": "number"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn mock_rejects_invalid_fail_with() {
    let dir = tempfile::tempdir().unwrap();
    let fx = failing_collection(dir.path());
    for bad in ["5xx", "999", "dropp"] {
        let out = merpati(&["mock", fx.to_str().unwrap(), "--port", "0", "--fail-with", bad], dir.path()).await;
        assert_eq!(out.status.code(), Some(2), "{bad}: {}", text(&out));
        assert!(text(&out).contains("--fail-with"), "{}", text(&out));
    }
}

// ---------------------------------------------------------------------------
// Client certificates
// ---------------------------------------------------------------------------

/// The client certificate fixtures of merpati-http (issued by its `ca.pem`).
fn cert_fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../merpati-http/tests/fixtures/certs")
}

fn leaf(name: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_file(cert_fixtures().join(name)).unwrap()
}

/// An HTTPS server that asks for a client certificate issued by the fixture
/// CA and answers with which one it got: `rsa`, `ec` or `none`.
async fn mtls_server() -> u16 {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(leaf("ca.pem")).unwrap();
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .allow_unauthenticated()
        .build()
        .unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let key = PrivateKeyDer::from_pem_slice(ck.signing_key.serialize_pem().as_bytes()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![ck.cert.der().clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let known = [(leaf("client-rsa.pem"), "rsa"), (leaf("client-ec.pem"), "ec")];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let known = known.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let presented = tls.get_ref().1.peer_certificates().and_then(|c| c.first().cloned());
                let body = match presented {
                    Some(c) => known.iter().find(|(k, _)| *k == c).map(|(_, n)| *n).unwrap_or("other"),
                    None => "none",
                };
                let mut buf = vec![0u8; 8192];
                let mut got = Vec::new();
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let resp =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = tls.write_all(resp.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn ssl_client_certificates() {
    let port = mtls_server().await;
    let dir = tempfile::tempdir().unwrap();
    let fx = collection(
        dir.path(),
        "mTLS",
        &[
            ("Local", "https://localhost:{{port}}/", &["console.log('local=' + pm.response.text());"]),
            ("Loopback", "https://127.0.0.1:{{port}}/", &["console.log('loopback=' + pm.response.text());"]),
        ],
        json!([]),
    );
    let certs = cert_fixtures();
    let cert = |name: &str| certs.join(name).display().to_string();
    let port_var = format!("port={port}");
    let run = |extra: Vec<String>| {
        let mut args: Vec<String> =
            ["run", fx.to_str().unwrap(), "-k", "--env-var", &port_var].iter().map(|s| s.to_string()).collect();
        args.extend(extra);
        let dir = dir.path().to_path_buf();
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            merpati(&args, &dir).await
        }
    };
    let presented = |o: &Output| {
        let t = text(o);
        let find = |what: &str| t.lines().find_map(|l| l.split(what).nth(1).map(|v| v.trim().to_string()));
        (find("local="), find("loopback="))
    };
    let some = |a: &str, b: &str| (Some(a.to_string()), Some(b.to_string()));

    // Without certificates the server sees none.
    let out = run(vec![]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(presented(&out), some("none", "none"));

    // One certificate for every host, with an encrypted key.
    let global = vec![
        "--ssl-client-cert".to_string(),
        cert("client-ec.pem"),
        "--ssl-client-key".into(),
        cert("client-ec-encrypted.key"),
        "--ssl-client-passphrase".into(),
        "merpati".into(),
    ];
    let out = run(global).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(presented(&out), some("ec", "ec"));

    // The list wins for its hosts; the global certificate is the fallback.
    let list = dir.path().join("certs.json");
    std::fs::write(
        &list,
        json!([{
            "name": "loopback",
            "matches": [format!("https://127.0.0.1:{port}/*")],
            "cert": {"src": cert("client-ec.pem")},
            "key": {"src": cert("client-ec.key")}
        }])
        .to_string(),
    )
    .unwrap();
    let out = run(vec![
        "--ssl-client-cert-list".into(),
        list.display().to_string(),
        "--ssl-client-cert".into(),
        cert("client-rsa.pem"),
        "--ssl-client-key".into(),
        cert("client-rsa.key"),
    ])
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(presented(&out), some("rsa", "ec"));

    // PKCS#12 entry with any port; a pattern without a port means 443.
    // Relative paths resolve against --working-dir.
    std::fs::write(
        &list,
        json!([
            {"name": "local", "matches": ["https://localhost:*/*"], "pfx": {"src": "client-rsa-aes.p12"},
             "passphrase": "merpati"},
            {"name": "default port", "matches": ["https://127.0.0.1/*"], "cert": {"src": "client-ec.pem"},
             "key": {"src": "client-ec.key"}}
        ])
        .to_string(),
    )
    .unwrap();
    let out = run(vec![
        "--ssl-client-cert-list".into(),
        list.display().to_string(),
        "--working-dir".into(),
        certs.display().to_string(),
    ])
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(presented(&out), some("rsa", "none"));

    // Usage errors: a key without certificate, missing files.
    for args in [
        vec!["--ssl-client-key".to_string(), cert("client-ec.key")],
        vec!["--ssl-client-cert".to_string(), "missing.pem".into()],
        vec!["--ssl-client-cert-list".to_string(), "missing.json".into()],
    ] {
        let out = run(args.clone()).await;
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out));
    }
}
