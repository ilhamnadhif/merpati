use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};

use merpati_core::model::HttpVersion;
use merpati_core::prepared::{MultipartPart, MultipartValue, PreparedBody, PreparedRequest};
use merpati_core::settings::{CertificateSettings, ProxySettings};
use merpati_http::{CancellationToken, CookieJar, EngineConfig, HttpEngine, HttpErrorKind, SendOptions};

async fn echo(req: Request) -> Response {
    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let headers: Vec<(String, String)> =
        req.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string())).collect();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::json!({
        "method": method,
        "uri": uri,
        "headers": headers,
        "body": String::from_utf8_lossy(&body),
    });
    ([(header::CONTENT_TYPE, "application/json")], json.to_string()).into_response()
}

async fn spawn() -> String {
    let app = Router::new()
        .route("/echo", any(echo))
        .route(
            "/redirect/{n}",
            any(|axum::extract::Path(n): axum::extract::Path<u32>| async move {
                if n == 0 {
                    Redirect::to("/echo").into_response()
                } else {
                    Redirect::to(&format!("/redirect/{}", n - 1)).into_response()
                }
            }),
        )
        .route("/see-other", post(|| async { Redirect::to("/echo") }))
        .route("/found", any(|| async { (StatusCode::FOUND, [(header::LOCATION, "/echo")]) }))
        .route("/temp", any(|| async { (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, "/echo")]) }))
        .route("/set-cookie", get(|| async { ([(header::SET_COOKIE, "sid=xyz; Path=/")], "ok") }))
        .route(
            "/utf8-location",
            get(|| async {
                // Raw UTF-8 in Location, as many servers send it.
                let location = header::HeaderValue::from_bytes("/landed/caf\u{e9}?q=\u{e9}".as_bytes()).unwrap();
                (StatusCode::FOUND, [(header::LOCATION, location)])
            }),
        )
        .route("/landed/{name}", any(echo))
        .route(
            "/big",
            get(|| async {
                let chunk = vec![b'a'; 1024 * 1024];
                let body: Vec<u8> = chunk.repeat(12);
                body
            }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "late"
            }),
        )
        .route(
            "/upload",
            post(|headers: HeaderMap, body: Bytes| async move {
                let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                format!("{ct}\n{}", String::from_utf8_lossy(&body))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn engine() -> HttpEngine {
    let cfg = EngineConfig { max_body_in_memory: 4 * 1024 * 1024, ..Default::default() };
    HttpEngine::new(cfg, Arc::new(CookieJar::new()))
}

fn req(method: &str, url: String) -> PreparedRequest {
    PreparedRequest { method: method.into(), url, ..Default::default() }
}

fn json(out: &merpati_http::SendOutcome) -> serde_json::Value {
    serde_json::from_slice(&out.response.body.read_all().unwrap()).unwrap()
}

#[tokio::test]
async fn basic_send_and_headers() {
    let base = spawn().await;
    let e = engine();
    let mut r = req("POST", format!("{base}/echo?x=1"));
    r.headers.push(("X-Test".into(), "héllo".into()));
    r.body = PreparedBody::Bytes("payload".into());
    let out = e.send(&r, &SendOptions::default()).await.unwrap();
    assert_eq!(out.response.status, 200);
    assert_eq!(out.response.status_text, "OK");
    assert_eq!(out.response.version, "HTTP/1.1");
    assert_eq!(out.peer_certificate, None, "no TLS, no certificate");
    let v = json(&out);
    assert_eq!(v["method"], "POST");
    assert_eq!(v["uri"], "/echo?x=1");
    assert_eq!(v["body"], "payload");
    assert!(out.response.timings.total_ms >= out.response.timings.ttfb_ms);
    assert!(out.response.header_size > 0);
    assert!(out.sent.headers.iter().any(|(k, _)| k == "x-test"));
}

#[tokio::test]
async fn follows_redirects_with_limits() {
    let base = spawn().await;
    let e = engine();
    let out = e.send(&req("GET", format!("{base}/redirect/3")), &SendOptions::default()).await.unwrap();
    assert_eq!(out.response.status, 200);
    assert_eq!(out.response.redirects.len(), 4);
    assert!(out.response.url.ends_with("/echo"));

    let mut r = req("GET", format!("{base}/redirect/3"));
    r.settings.max_redirects = 2;
    let err = e.send(&r, &SendOptions::default()).await.unwrap_err();
    assert_eq!(err.kind, HttpErrorKind::TooManyRedirects);

    let mut r = req("GET", format!("{base}/redirect/0"));
    r.settings.follow_redirects = false;
    let out = e.send(&r, &SendOptions::default()).await.unwrap();
    assert_eq!(out.response.status, 303);
}

#[tokio::test]
async fn redirect_method_semantics() {
    let base = spawn().await;
    let e = engine();
    let mut r = req("POST", format!("{base}/found"));
    r.body = PreparedBody::Bytes("data".into());
    let out = e.send(&r, &SendOptions::default()).await.unwrap();
    let v = json(&out);
    assert_eq!(v["method"], "GET", "302 turns POST into GET by default");
    assert_eq!(v["body"], "");

    r.settings.follow_original_method = true;
    let v = json(&e.send(&r, &SendOptions::default()).await.unwrap());
    assert_eq!(v["method"], "POST");
    assert_eq!(v["body"], "data");

    let mut r = req("PUT", format!("{base}/temp"));
    r.body = PreparedBody::Bytes("keep".into());
    let v = json(&e.send(&r, &SendOptions::default()).await.unwrap());
    assert_eq!(v["method"], "PUT", "307 keeps the method and body");
    assert_eq!(v["body"], "keep");
}

#[tokio::test]
async fn follows_non_ascii_redirect_locations() {
    let base = spawn().await;
    let out = engine().send(&req("GET", format!("{base}/utf8-location")), &SendOptions::default()).await.unwrap();
    assert_eq!(out.response.status, 200);
    assert_eq!(json(&out)["uri"], "/landed/caf%C3%A9?q=%C3%A9");
}

#[tokio::test]
async fn cookies_are_stored_and_sent() {
    let base = spawn().await;
    let e = engine();
    e.send(&req("GET", format!("{base}/set-cookie")), &SendOptions::default()).await.unwrap();
    assert_eq!(e.cookies().list().len(), 1);
    let out = e.send(&req("GET", format!("{base}/echo")), &SendOptions::default()).await.unwrap();
    let v = json(&out);
    let has_cookie = v["headers"].as_array().unwrap().iter().any(|h| h[0] == "cookie" && h[1] == "sid=xyz");
    assert!(has_cookie);

    let mut r = req("GET", format!("{base}/echo"));
    r.settings.disable_cookies = true;
    let v = json(&e.send(&r, &SendOptions::default()).await.unwrap());
    assert!(!v["headers"].as_array().unwrap().iter().any(|h| h[0] == "cookie"));
}

#[tokio::test]
async fn large_bodies_spill_to_disk() {
    let base = spawn().await;
    let e = engine();
    let seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let s2 = seen.clone();
    let opts = SendOptions {
        progress: Some(Arc::new(move |p| s2.store(p.received, std::sync::atomic::Ordering::Relaxed))),
        ..Default::default()
    };
    let out = e.send(&req("GET", format!("{base}/big")), &opts).await.unwrap();
    assert!(out.response.body.is_file());
    assert_eq!(out.response.body.len(), 12 * 1024 * 1024);
    assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 12 * 1024 * 1024);
    assert_eq!(out.response.body.read_range(5, 3).unwrap(), b"aaa");
    let path = out.response.body.file_path().unwrap().to_path_buf();
    assert!(path.exists());
    drop(out);
    assert!(!path.exists(), "temp file is removed with the response");
}

#[tokio::test]
async fn timeout_and_cancel() {
    let base = spawn().await;
    let e = engine();
    let mut r = req("GET", format!("{base}/slow"));
    r.settings.timeout = Some(Duration::from_millis(200));
    let err = e.send(&r, &SendOptions::default()).await.unwrap_err();
    assert_eq!(err.kind, HttpErrorKind::Timeout, "{err:?}");
    assert!(err.message.contains("200 ms"));

    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c2.cancel();
    });
    let err = e.send(&req("GET", format!("{base}/slow")), &SendOptions::with_cancel(cancel)).await.unwrap_err();
    assert_eq!(err.kind, HttpErrorKind::Cancelled);
}

#[tokio::test]
async fn classified_errors() {
    let e = engine();
    // Port 9 on localhost is almost certainly closed.
    let err = e.send(&req("GET", "http://127.0.0.1:9/".into()), &SendOptions::default()).await.unwrap_err();
    assert_eq!(err.kind, HttpErrorKind::ConnectionRefused, "{err:?}");
    let err = e.send(&req("GET", "http://does-not-exist.invalid/".into()), &SendOptions::default()).await.unwrap_err();
    assert_eq!(err.kind, HttpErrorKind::Dns, "{err:?}");
    let err = e.send(&req("GET", "ftp://x/".into()), &SendOptions::default()).await.unwrap_err();
    assert_eq!(err.kind, HttpErrorKind::InvalidUrl);
    let mut r = req("GET", "http://127.0.0.1:9/".into());
    r.headers.push(("bad header".into(), "x".into()));
    let err = e.send(&r, &SendOptions::default()).await.unwrap_err();
    assert_eq!(err.kind, HttpErrorKind::InvalidHeader);
}

#[tokio::test]
async fn multipart_and_file_bodies() {
    let base = spawn().await;
    let e = engine();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "file-content").unwrap();
    let mut r = req("POST", format!("{base}/upload"));
    // A user-provided multipart content type without boundary is replaced.
    r.headers.push(("Content-Type".into(), "multipart/form-data".into()));
    r.body = PreparedBody::Multipart(vec![
        MultipartPart { name: "title".into(), value: MultipartValue::Text("hi".into()), content_type: None },
        MultipartPart { name: "doc".into(), value: MultipartValue::File(file.clone()), content_type: None },
    ]);
    let out = e.send(&r, &SendOptions::default()).await.unwrap();
    let text = out.response.body.text_lossy();
    assert!(text.starts_with("multipart/form-data; boundary="), "{text}");
    assert!(text.contains("name=\"title\""));
    assert!(text.contains("filename=\"note.txt\""));
    assert!(text.contains("file-content"));

    // One with a boundary (copied from another request) cannot match the
    // body's: it is replaced too, rather than sent next to the real one.
    let mut r = req("POST", format!("{base}/echo"));
    r.headers.push(("Content-Type".into(), "multipart/form-data; boundary=----copied".into()));
    r.body = PreparedBody::Multipart(vec![MultipartPart {
        name: "title".into(),
        value: MultipartValue::Text("hi".into()),
        content_type: None,
    }]);
    let v = json(&e.send(&r, &SendOptions::default()).await.unwrap());
    let types: Vec<&str> = v["headers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h[0] == "content-type")
        .map(|h| h[1].as_str().unwrap())
        .collect();
    assert_eq!(types.len(), 1, "{types:?}");
    let boundary = types[0].strip_prefix("multipart/form-data; boundary=").unwrap();
    assert!(v["body"].as_str().unwrap().starts_with(&format!("--{boundary}\r\n")), "{v}");

    let mut r = req("POST", format!("{base}/upload"));
    r.body = PreparedBody::File(file);
    let out = e.send(&r, &SendOptions::default()).await.unwrap();
    assert!(out.response.body.text_lossy().ends_with("file-content"));

    let mut r = req("POST", format!("{base}/upload"));
    r.body = PreparedBody::File(dir.path().join("missing.bin"));
    assert_eq!(e.send(&r, &SendOptions::default()).await.unwrap_err().kind, HttpErrorKind::File);
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_requests() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let app = Router::new().route("/ping", get(|| async { "pong" }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let e = engine();
    let url = format!("http://unix:{}:/ping", sock.display());
    let out = e.send(&req("GET", url), &SendOptions::default()).await.unwrap();
    assert_eq!(out.response.body.text_lossy(), "pong");
    assert_eq!(out.peer_certificate, None);
}

// ---------------------------------------------------------------------------
// HTTPS: the server certificate on responses (TLS channel bindings)
// ---------------------------------------------------------------------------

struct TlsListener {
    tcp: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((tcp, addr)) = self.tcp.accept().await else { continue };
            if let Ok(tls) = self.acceptor.accept(tcp).await {
                return (tls, addr);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// HTTPS echo server for `localhost` speaking HTTP/1.1 and HTTP/2 (ALPN).
/// Returns its port, certificate DER and an engine trusting it.
async fn spawn_tls(dir: &tempfile::TempDir, proxy: Option<String>) -> (u16, Vec<u8>, HttpEngine) {
    merpati_http::tls::install_provider();
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![ck.cert.der().clone()], key.into())
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = tcp.local_addr().unwrap().port();
    let listener = TlsListener { tcp, acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)) };
    let app = Router::new().route("/echo", any(echo)).route(
        "/ntlm",
        get(|| async { (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "NTLM TlRMTVNTUAACAAAA")]) }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let ca = dir.path().join("server.pem");
    std::fs::write(&ca, ck.cert.pem()).unwrap();
    let certificates =
        CertificateSettings { ca_path: ca.display().to_string(), ca_enabled: true, ..Default::default() };
    let proxy = proxy.map(|url| ProxySettings { custom_url: url, custom_enabled: true, ..Default::default() });
    let cfg = EngineConfig { certificates, proxy: proxy.unwrap_or_default(), ..Default::default() };
    (port, ck.cert.der().to_vec(), HttpEngine::new(cfg, Arc::new(CookieJar::new())))
}

#[tokio::test]
async fn https_responses_carry_the_server_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let (port, cert, e) = spawn_tls(&dir, None).await;
    for (version, wire) in
        [(HttpVersion::Auto, "HTTP/2"), (HttpVersion::Http1, "HTTP/1.1"), (HttpVersion::Http2, "HTTP/2")]
    {
        let mut r = req("GET", format!("https://localhost:{port}/echo"));
        r.settings.http_version = version;
        // The second request reuses the cached client and its pooled connection.
        for attempt in 0..2 {
            let out = e.send(&r, &SendOptions::default()).await.unwrap();
            assert_eq!((out.response.status, out.response.version.as_str()), (200, wire), "{version:?}");
            assert_eq!(out.peer_certificate.as_deref(), Some(cert.as_slice()), "{version:?} #{attempt}");
        }
    }
    // Plain HTTP with the same engine: no certificate.
    let base = spawn().await;
    let out = e.send(&req("GET", format!("{base}/redirect/1")), &SendOptions::default()).await.unwrap();
    assert_eq!((out.response.status, out.peer_certificate), (200, None));
}

/// HTTP CONNECT proxy: tunnels to the requested `host:port`, counting tunnels.
async fn connect_proxy() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let tunnels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = tunnels.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let count = count.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                let Some(target) = head.strip_prefix("CONNECT ").and_then(|r| r.split(' ').next()) else { return };
                let Ok(mut upstream) = tokio::net::TcpStream::connect(target).await else { return };
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (url, tunnels)
}

#[tokio::test]
async fn https_through_a_connect_proxy() {
    let (proxy, tunnels) = connect_proxy().await;
    let dir = tempfile::tempdir().unwrap();
    let (port, _, e) = spawn_tls(&dir, Some(proxy)).await;
    let out = e.send(&req("GET", format!("https://localhost:{port}/echo")), &SendOptions::default()).await.unwrap();
    assert_eq!(out.response.status, 200);
    assert_eq!(tunnels.load(std::sync::atomic::Ordering::SeqCst), 1);
    // reqwest 0.13 reports no TLS info for tunneled connections (if this
    // starts failing, the certificate is available there too).
    assert_eq!(out.peer_certificate, None);
}

#[tokio::test]
async fn ntlm_challenges_through_a_proxy_carry_the_server_certificate() {
    let (proxy, tunnels) = connect_proxy().await;
    let dir = tempfile::tempdir().unwrap();
    let (port, cert, e) = spawn_tls(&dir, Some(proxy)).await;
    let out = e.send(&req("GET", format!("https://localhost:{port}/ntlm")), &SendOptions::default()).await.unwrap();
    assert_eq!(out.response.status, 401);
    // Read over a TLS handshake of its own through the same proxy, for the
    // NTLM channel binding.
    assert_eq!(out.peer_certificate.as_deref(), Some(cert.as_slice()));
    assert_eq!(tunnels.load(std::sync::atomic::Ordering::SeqCst), 2);
}
