mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use merpati_core::model::{BodyMode, RawLanguage, RequestKind};
use merpati_proxy::{
    CaptureFilter, CaptureProxy, CertificateAuthority, EntryKind, EntryState, ImportOptions, MemoryCaStore,
    ProxyConfig, ProxyError, ProxyEvent, UpstreamProxy, WsOpcode, to_har,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_http_get_and_post_are_captured() {
    let upstream = spawn_http().await;
    let (proxy, _) = start_proxy(|_| {}).await;
    let http = client(&proxy, None);

    let r = http.get(format!("http://{upstream}/hello?x=1")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "hello world");
    let r = http
        .post(format!("http://{upstream}/echo"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret-token")
        .body(r#"{"a":1}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), r#"{"a":1}"#);

    let entries = finished(&proxy, 2).await;
    let get = &entries[0];
    assert_eq!(get.kind, EntryKind::Http);
    assert_eq!(get.state, EntryState::Complete);
    assert_eq!(get.method, "GET");
    assert_eq!(get.url, format!("http://{upstream}/hello?x=1"));
    assert_eq!(get.http_version, "HTTP/1.1");
    assert_eq!(get.upstream_http_version.as_deref(), Some("HTTP/1.1"));
    assert_eq!(get.status, Some(200));
    assert_eq!(get.response_body.data.as_ref(), b"hello world");
    assert!(!get.tls_intercepted);
    assert!(get.timings.connect.is_some() && get.timings.ttfb.is_some() && get.timings.total.is_some());
    // No proxy headers are forwarded or recorded.
    assert!(get.request_header("proxy-connection").is_none());
    assert!(get.request_header("proxy-authorization").is_none());
    assert_eq!(get.request_header("host"), Some(upstream.to_string().as_str()));

    let post = &entries[1];
    assert_eq!(post.method, "POST");
    assert_eq!(post.request_body.data.as_ref(), br#"{"a":1}"#);
    assert_eq!(post.request_header("authorization"), Some("Bearer secret-token"));
    assert!(post.timings.reused_connection, "second request should reuse the pooled upstream connection");
    assert_eq!(proxy.entry(post.id).unwrap().id, post.id);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn https_interception_with_reqwest_and_http2() {
    let cert = ca_signed_cert();
    let upstream = spawn_https(&cert, &[b"h2", b"http/1.1"]).await;
    let (proxy, ca) = start_proxy(|c| c.extra_upstream_roots_pem = Some(cert.ca_pem.clone())).await;
    // The client trusts only the Merpati CA: it can only succeed through interception.
    let http = client(&proxy, Some(ca.cert_pem()));

    let r = http.get(format!("https://127.0.0.1:{}/json", upstream.port())).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.version(), reqwest::Version::HTTP_2);
    assert_eq!(r.text().await.unwrap(), JSON_BODY);
    let r = http.get(format!("https://localhost:{}/version", upstream.port())).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "HTTP/2.0");

    let entries = finished(&proxy, 2).await;
    let e = &entries[0];
    assert!(e.tls_intercepted);
    assert_eq!(e.scheme, "https");
    assert_eq!(e.url, format!("https://127.0.0.1:{}/json", upstream.port()));
    assert_eq!(e.http_version, "HTTP/2");
    assert_eq!(e.upstream_http_version.as_deref(), Some("HTTP/2"));
    assert_eq!(e.response_body.data.as_ref(), JSON_BODY.as_bytes());
    assert!(e.timings.tls_handshake.is_some());
    assert_eq!(entries[1].host, "localhost");
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http2_only_and_http1_only_upstreams() {
    let cert = ca_signed_cert();
    let h2_only = spawn_https(&cert, &[b"h2"]).await;
    let h1_only = spawn_https(&cert, &[b"http/1.1"]).await;
    let (proxy, ca) = start_proxy(|c| c.extra_upstream_roots_pem = Some(cert.ca_pem.clone())).await;
    let http = client(&proxy, Some(ca.cert_pem()));
    let h1_client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(proxy.url()).unwrap())
        .tls_certs_only([reqwest::Certificate::from_pem(ca.cert_pem().as_bytes()).unwrap()])
        .http1_only()
        .build()
        .unwrap();

    let r = h1_client.get(format!("https://127.0.0.1:{}/version", h2_only.port())).send().await.unwrap();
    assert_eq!(r.version(), reqwest::Version::HTTP_11);
    assert_eq!(r.text().await.unwrap(), "HTTP/2.0", "the proxy must speak HTTP/2 to an h2-only server");
    let r = http.get(format!("https://127.0.0.1:{}/version", h1_only.port())).send().await.unwrap();
    assert_eq!(r.version(), reqwest::Version::HTTP_2);
    assert_eq!(r.text().await.unwrap(), "HTTP/1.1");

    let entries = finished(&proxy, 2).await;
    assert_eq!(entries[0].http_version, "HTTP/1.1");
    assert_eq!(entries[0].upstream_http_version.as_deref(), Some("HTTP/2"));
    assert_eq!(entries[1].http_version, "HTTP/2");
    assert_eq!(entries[1].upstream_http_version.as_deref(), Some("HTTP/1.1"));
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn self_signed_upstream_needs_skip_verify() {
    let cert = self_signed_cert();
    let upstream = spawn_https(&cert, &[b"http/1.1"]).await;
    let url = format!("https://127.0.0.1:{}/hello", upstream.port());

    let (strict, ca) = start_proxy(|_| {}).await;
    let r = client(&strict, Some(ca.cert_pem())).get(&url).send().await.unwrap();
    assert_eq!(r.status(), 502);
    let body = r.text().await.unwrap();
    assert!(body.contains("certificate"), "{body}");
    let e = &finished(&strict, 1).await[0];
    assert_eq!(e.state, EntryState::Failed);
    assert!(e.error.as_deref().unwrap().contains("not trusted"), "{:?}", e.error);
    strict.stop().await;

    let (lax, ca) = start_proxy(|c| c.verify_upstream_tls = false).await;
    let r = client(&lax, Some(ca.cert_pem())).get(&url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "hello world");
    lax.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn curl_through_the_proxy() {
    let cert = ca_signed_cert();
    let https = spawn_https(&cert, &[b"h2", b"http/1.1"]).await;
    let http = spawn_http().await;
    let (proxy, ca) = start_proxy(|c| c.extra_upstream_roots_pem = Some(cert.ca_pem.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, ca.cert_pem()).unwrap();
    let ca_path = ca_path.to_str().unwrap();
    let px = proxy.url();

    let out = curl(&["-x", &px, "--cacert", ca_path, &format!("https://127.0.0.1:{}/json", https.port())]).await;
    assert!(out.success, "{}", out.stderr);
    assert_eq!(out.body, JSON_BODY);
    let out =
        curl(&["-x", &px, "--http1.1", "--cacert", ca_path, &format!("https://localhost:{}/hello", https.port())])
            .await;
    assert!(out.success, "{}", out.stderr);
    assert_eq!(out.status, "200 1.1");
    let out = curl(&["-x", &px, "-d", "a=1&b=two", &format!("http://{http}/echo")]).await;
    assert!(out.success, "{}", out.stderr);
    assert_eq!(out.body, "a=1&b=two");
    // Without the CA the client refuses our certificate; that is recorded.
    let out = curl(&["-x", &px, &format!("https://127.0.0.1:{}/hello", https.port())]).await;
    assert!(!out.success);

    let entries = wait_until(&proxy, "4 entries", |e| e.len() >= 4 && e.iter().all(|e| e.is_finished())).await;
    assert!(entries[0].tls_intercepted);
    assert_eq!(entries[1].http_version, "HTTP/1.1");
    assert_eq!(entries[2].request_body.data.as_ref(), b"a=1&b=two");
    let item = entries[2].to_request_item(&ImportOptions::default()).unwrap();
    assert_eq!(item.http().unwrap().body.mode, BodyMode::UrlEncoded);
    let failed = &entries[3];
    assert_eq!(failed.kind, EntryKind::Tunnel);
    assert_eq!(failed.state, EntryState::Failed);
    assert!(failed.error.as_deref().unwrap().contains("does not trust the Merpati CA"), "{:?}", failed.error);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tunnel_mode_and_passthrough_hosts() {
    let cert = ca_signed_cert();
    let upstream = spawn_https(&cert, &[b"http/1.1"]).await;

    // Interception off: the client sees the real server certificate.
    let (proxy, _) = start_proxy(|c| c.intercept_tls = false).await;
    let http = client(&proxy, Some(&cert.ca_pem));
    let r = http.get(format!("https://127.0.0.1:{}/hello", upstream.port())).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "hello world");
    drop(http); // closes the tunnel
    let e = &finished(&proxy, 1).await[0];
    assert_eq!(e.kind, EntryKind::Tunnel);
    assert_eq!(e.method, "CONNECT");
    assert_eq!(e.url, format!("127.0.0.1:{}", upstream.port()));
    assert_eq!(e.note.as_deref(), Some("TLS interception is off"));
    let stats = e.stream.clone().unwrap();
    assert!(stats.bytes_up > 100 && stats.bytes_down > 100, "{stats:?}");
    assert!(e.timings.total.is_some());
    assert!(!proxy.is_intercepting());
    proxy.stop().await;

    // Interception on, but the host is passthrough.
    let (proxy, _) = start_proxy(|c| c.passthrough_hosts = vec!["127.0.0.?".into()]).await;
    let http = client(&proxy, Some(&cert.ca_pem));
    let r = http.get(format!("https://127.0.0.1:{}/hello", upstream.port())).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "hello world");
    drop(http);
    let e = &finished(&proxy, 1).await[0];
    assert_eq!(e.kind, EntryKind::Tunnel);
    assert!(e.note.as_deref().unwrap().contains("passthrough"));
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_tls_switches_host_to_passthrough() {
    let cert = ca_signed_cert();
    let upstream = spawn_https(&cert, &[b"http/1.1"]).await;
    let (proxy, _) = start_proxy(|c| c.passthrough_on_tls_error = true).await;
    let url = format!("https://127.0.0.1:{}/hello", upstream.port());
    // A client that only trusts the real server (like a pinned app) rejects us once...
    assert!(client(&proxy, Some(&cert.ca_pem)).get(&url).send().await.is_err());
    wait_until(&proxy, "the TLS failure", |e| e.iter().any(|e| e.state == EntryState::Failed)).await;
    // ...and is tunnelled afterwards.
    let http = client(&proxy, Some(&cert.ca_pem));
    assert_eq!(http.get(&url).send().await.unwrap().text().await.unwrap(), "hello world");
    drop(http);
    let entries = finished(&proxy, 2).await;
    assert_eq!(entries[0].state, EntryState::Failed);
    assert_eq!(entries[1].state, EntryState::Complete);
    assert!(entries[1].note.as_deref().unwrap().contains("rejected"));
    assert_eq!(proxy.tls_failed_hosts(), vec!["127.0.0.1".to_string()]);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h2_only_client_without_client_http2_is_tunnelled() {
    let cert = ca_signed_cert();
    let upstream = spawn_https(&cert, &[b"h2", b"http/1.1"]).await;
    let (proxy, ca) = start_proxy(|c| {
        c.client_http2 = false;
        c.passthrough_on_tls_error = true;
        c.extra_upstream_roots_pem = Some(cert.ca_pem.clone());
    })
    .await;

    // A client that offers only h2 has no protocol in common with the
    // proxy's HTTP/1.1 certificate: it is tunnelled (and sees the real
    // server), not reported as distrusting the Merpati CA.
    let tunnel = connect_tunnel(&proxy, &format!("localhost:{}", upstream.port())).await;
    let mut tls = (*client_tls_config(&cert.ca_pem)).clone();
    tls.alpn_protocols = vec![b"h2".to_vec()];
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let stream = tokio_rustls::TlsConnector::from(Arc::new(tls)).connect(name, tunnel).await.unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    drop(stream);
    let e = &finished(&proxy, 1).await[0];
    assert_eq!((e.kind, e.state), (EntryKind::Tunnel, EntryState::Complete));
    assert!(e.note.as_deref().unwrap().contains("only HTTP/2"), "{:?}", e.note);
    assert!(proxy.tls_failed_hosts().is_empty());

    // The host is still decrypted for clients that speak HTTP/1.1.
    let http = client(&proxy, Some(ca.cert_pem()));
    let r = http.get(format!("https://localhost:{}/version", upstream.port())).send().await.unwrap();
    assert_eq!(r.version(), reqwest::Version::HTTP_11);
    let entries = finished(&proxy, 2).await;
    assert!(entries[1].tls_intercepted);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn options_with_an_empty_path_is_forwarded_as_asterisk() {
    // A raw upstream that reports each request line.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut conn = tokio::io::BufReader::new(tcp);
                let mut at_start = true;
                loop {
                    let mut line = String::new();
                    if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    if at_start {
                        let _ = tx.send(line.trim_end().to_owned());
                    }
                    at_start = line == "\r\n";
                    if at_start {
                        let resp = b"HTTP/1.1 200 OK\r\nAllow: GET, OPTIONS\r\nContent-Length: 0\r\n\r\n";
                        conn.get_mut().write_all(resp).await.unwrap();
                    }
                }
            });
        }
    });
    let (proxy, _) = start_proxy(|_| {}).await;

    // Pipelined on one connection: only the empty path means the whole server.
    let mut raw = tokio::net::TcpStream::connect(proxy.addr()).await.unwrap();
    let targets = [format!("http://{upstream}"), format!("http://{upstream}/"), format!("http://{upstream}/x")];
    let mut requests = String::new();
    for t in &targets {
        requests.push_str(&format!("OPTIONS {t} HTTP/1.1\r\nHost: {upstream}\r\n\r\n"));
    }
    requests.push_str(&format!("GET http://{upstream} HTTP/1.1\r\nHost: {upstream}\r\n\r\n"));
    raw.write_all(requests.as_bytes()).await.unwrap();
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap());
    }
    assert_eq!(seen, ["OPTIONS * HTTP/1.1", "OPTIONS / HTTP/1.1", "OPTIONS /x HTTP/1.1", "GET / HTTP/1.1"]);

    let entries = finished(&proxy, 4).await;
    assert_eq!((entries[0].method.as_str(), entries[0].url.as_str()), ("OPTIONS", targets[0].as_str()));
    assert_eq!(entries[0].status, Some(200));
    assert_eq!(entries[1].url, targets[1]);
    drop(raw);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bodies_in_flight_count_toward_the_total_budget() {
    const CHUNK: usize = 64 * 1024;
    // An upstream that sends a chunked body of CHUNK bytes and ends it only
    // after `go`.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stalling = listener.local_addr().unwrap();
    let (go, go_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let mut go_rx = go_rx.clone();
            tokio::spawn(async move {
                let mut conn = tokio::io::BufReader::new(tcp);
                let mut line = String::new();
                while conn.read_line(&mut line).await.unwrap_or(0) > 0 && line != "\r\n" {
                    line.clear();
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n{CHUNK:x}\r\n"
                );
                let conn = conn.get_mut();
                conn.write_all(head.as_bytes()).await.unwrap();
                conn.write_all(&[b'x'; CHUNK]).await.unwrap();
                conn.write_all(b"\r\n").await.unwrap();
                let _ = go_rx.wait_for(|go| *go).await;
                let _ = conn.write_all(b"0\r\n\r\n").await;
            });
        }
    });
    let plain = spawn_http().await;
    let (proxy, _) = start_proxy(|c| {
        c.max_body_bytes = CHUNK;
        c.max_total_body_bytes = 100 * 1024;
    })
    .await;
    let http = client(&proxy, None);

    // A finished entry holding 60 KiB.
    http.get(format!("http://{plain}/stream/61440")).send().await.unwrap().bytes().await.unwrap();
    let old = finished(&proxy, 1).await[0].id;
    assert_eq!(proxy.stats().stored_body_bytes, 61440);

    // Three bodies stream at the same time.
    let (half, mut half_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = Vec::new();
    for i in 0..3 {
        let (http, half) = (http.clone(), half.clone());
        tasks.push(tokio::spawn(async move {
            let mut r = http.get(format!("http://{stalling}/{i}")).send().await.unwrap();
            let mut n = 0;
            while let Some(c) = r.chunk().await.unwrap() {
                n += c.len();
                if n == CHUNK {
                    half.send(()).unwrap();
                }
            }
            n
        }));
    }
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(10), half_rx.recv()).await.unwrap().unwrap();
    }
    // The buffered prefixes count while still streaming: the finished entry
    // was evicted for them, and together they stop at the budget.
    let stats = proxy.stats();
    assert_eq!((stats.stored_body_bytes, stats.in_flight_body_bytes), (0, 100 * 1024), "{stats:?}");
    assert!(proxy.entry(old).is_none());
    assert_eq!(proxy.entries().len(), 3);

    go.send(true).unwrap();
    for t in tasks {
        assert_eq!(t.await.unwrap(), CHUNK);
    }
    // Each body fits the per-body cap; the budget cut at least two of them.
    let entries = finished(&proxy, 3).await;
    let kept: Vec<usize> = entries.iter().map(|e| e.response_body.data.len()).collect();
    assert_eq!(kept.iter().sum::<usize>(), 100 * 1024, "{kept:?}");
    assert!(entries.iter().all(|e| e.response_body.size == CHUNK as u64));
    assert!(entries.iter().filter(|e| e.response_body.truncated).count() >= 2, "{kept:?}");
    let stats = proxy.stats();
    assert_eq!((stats.stored_body_bytes, stats.in_flight_body_bytes), (100 * 1024, 0), "{stats:?}");
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filters_and_ring_buffer() {
    let upstream = spawn_http().await;
    let (proxy, _) = start_proxy(|c| {
        c.capacity = 3;
        c.filter = CaptureFilter { methods: vec!["GET".into()], ..CaptureFilter::default() };
    })
    .await;
    let http = client(&proxy, None);
    let base = format!("http://{upstream}");
    http.post(format!("{base}/echo")).body("x").send().await.unwrap().text().await.unwrap();
    for i in 0..5 {
        http.get(format!("{base}/hello?n={i}")).send().await.unwrap().text().await.unwrap();
    }
    let entries = finished(&proxy, 3).await;
    let urls: Vec<&str> = entries.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(urls, vec![format!("{base}/hello?n=2"), format!("{base}/hello?n=3"), format!("{base}/hello?n=4")]);

    // Host filters (live): localhost only, so 127.0.0.1 is not recorded.
    proxy.clear();
    proxy.set_filter(CaptureFilter { include_hosts: vec!["localhost".into()], ..CaptureFilter::default() });
    http.get(format!("{base}/hello")).send().await.unwrap().text().await.unwrap();
    http.get(format!("http://localhost:{}/hello", upstream.port())).send().await.unwrap().text().await.unwrap();
    let entries = finished(&proxy, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(proxy.entries().len(), 1);
    assert_eq!(entries[0].host, "localhost");

    // Exclusion, URL substring and content type.
    proxy.clear();
    proxy.set_filter(CaptureFilter {
        exclude_hosts: vec!["localhost".into()],
        url_contains: "/JSON".into(),
        content_types: vec!["application/json".into()],
        ..CaptureFilter::default()
    });
    http.get(format!("http://localhost:{}/json", upstream.port())).send().await.unwrap().text().await.unwrap();
    http.get(format!("{base}/hello?x=/json")).send().await.unwrap().text().await.unwrap(); // text/plain
    http.get(format!("{base}/json")).send().await.unwrap().text().await.unwrap();
    let entries = finished(&proxy, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(proxy.entries().len(), 1);
    assert_eq!(entries[0].url, format!("{base}/json"));
    assert!(proxy.filter().matches(&entries[0]));

    // Paused recording still forwards.
    proxy.clear();
    proxy.set_filter(CaptureFilter::default());
    proxy.set_recording(false);
    assert_eq!(http.get(format!("{base}/hello")).send().await.unwrap().status(), 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(proxy.entries().is_empty());
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_streamed_bodies_are_capped() {
    let upstream = spawn_http().await;
    let cert = ca_signed_cert();
    let tls_upstream = spawn_https(&cert, &[b"h2", b"http/1.1"]).await;
    let (proxy, ca) = start_proxy(|c| {
        c.max_body_bytes = 64 * 1024;
        c.extra_upstream_roots_pem = Some(cert.ca_pem.clone());
    })
    .await;
    let http = client(&proxy, Some(ca.cert_pem()));
    const SIZE: u64 = 24 * 1024 * 1024;

    // Download, plain and intercepted.
    for url in
        [format!("http://{upstream}/stream/{SIZE}"), format!("https://127.0.0.1:{}/stream/{SIZE}", tls_upstream.port())]
    {
        let mut stream = http.get(&url).send().await.unwrap().bytes_stream();
        let mut n = 0u64;
        while let Some(c) = stream.next().await {
            n += c.unwrap().len() as u64;
        }
        assert_eq!(n, SIZE);
    }
    // Chunked upload (no Content-Length).
    let chunks = futures_util::stream::iter((0..(SIZE / 65536)).map(|_| Ok::<_, std::io::Error>(vec![b'y'; 65536])));
    let r =
        http.post(format!("http://{upstream}/count")).body(reqwest::Body::wrap_stream(chunks)).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), SIZE.to_string());

    let entries = finished(&proxy, 3).await;
    for e in &entries[..2] {
        assert_eq!(e.response_body.size, SIZE);
        assert_eq!(e.response_body.data.len(), 64 * 1024);
        assert!(e.response_body.truncated);
    }
    let up = &entries[2];
    assert_eq!(up.request_body.size, SIZE);
    assert_eq!(up.request_body.data.len(), 64 * 1024);
    assert!(up.request_body.truncated);
    assert!(up.request_header("transfer-encoding").is_none());
    assert!(proxy.stats().stored_body_bytes <= 4 * 64 * 1024);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compressed_bodies_are_decoded_for_display() {
    let upstream = spawn_http().await;
    let (proxy, _) = start_proxy(|_| {}).await;
    let http = client(&proxy, None);
    for path in ["gzip", "br"] {
        let r = http.get(format!("http://{upstream}/{path}")).send().await.unwrap();
        assert_eq!(r.text().await.unwrap(), JSON_BODY, "client gets the original bytes and decodes them itself");
    }
    let entries = finished(&proxy, 2).await;
    let gz = &entries[0];
    assert_eq!(&gz.response_body.data[..2], &[0x1f, 0x8b], "raw body is stored still compressed");
    let decoded = gz.decoded_response_body();
    assert_eq!(decoded.text().unwrap(), JSON_BODY);
    assert_eq!(decoded.encoding.as_deref(), Some("gzip"));
    assert_eq!(gz.response_body.size as usize, gz.response_body.data.len());
    let br = entries[1].decoded_response_body();
    assert_eq!(br.text().unwrap(), JSON_BODY);
    // The saved example holds the decoded body without Content-Encoding.
    let ex = gz.to_example().unwrap();
    assert_eq!(ex.body, JSON_BODY);
    assert!(ex.headers.iter().all(|h| !h.key.eq_ignore_ascii_case("content-encoding")));
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_passthrough_with_message_capture() {
    let ws = spawn_ws().await;
    let cert = ca_signed_cert();
    let wss = spawn_wss(&cert).await;
    let (proxy, ca) = start_proxy(|c| c.extra_upstream_roots_pem = Some(cert.ca_pem.clone())).await;

    // 1. ws:// inside CONNECT (what browsers do through a proxy).
    let tunnel = connect_tunnel(&proxy, &ws.to_string()).await;
    let (mut sock, _) = tokio_tungstenite::client_async(format!("ws://{ws}/chat"), tunnel).await.unwrap();
    sock.send(Message::text("hello")).await.unwrap();
    assert_eq!(sock.next().await.unwrap().unwrap().into_text().unwrap(), "hello");
    sock.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
    assert_eq!(sock.next().await.unwrap().unwrap().into_data().as_ref(), &[1, 2, 3]);
    sock.close(None).await.unwrap();
    while sock.next().await.is_some() {}
    drop(sock);

    // 2. wss:// with interception.
    let tunnel = connect_tunnel(&proxy, &format!("localhost:{}", wss.port())).await;
    let connector = tokio_tungstenite::Connector::Rustls(client_tls_config(ca.cert_pem()));
    let (mut sock, _) = tokio_tungstenite::client_async_tls_with_config(
        format!("wss://localhost:{}/secure", wss.port()),
        tunnel,
        None,
        Some(connector),
    )
    .await
    .unwrap();
    sock.send(Message::text("secret")).await.unwrap();
    assert_eq!(sock.next().await.unwrap().unwrap().into_text().unwrap(), "secret");
    drop(sock);

    // 3. Absolute-form upgrade sent to the proxy directly.
    let mut raw = tokio::net::TcpStream::connect(proxy.addr()).await.unwrap();
    raw.write_all(
        format!(
            "GET http://{ws}/abs HTTP/1.1\r\nHost: {ws}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        raw.read_exact(&mut b).await.unwrap();
        head.push(b[0]);
    }
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));
    let mut sock = tokio_tungstenite::WebSocketStream::from_raw_socket(
        raw,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;
    sock.send(Message::text("abs")).await.unwrap();
    assert_eq!(sock.next().await.unwrap().unwrap().into_text().unwrap(), "abs");
    drop(sock);

    let entries = wait_until(&proxy, "3 websocket entries", |e| {
        e.iter().filter(|e| e.kind == EntryKind::WebSocket && e.is_finished()).count() == 3
    })
    .await;
    let plain = &entries[0];
    assert_eq!(plain.url, format!("http://{ws}/chat"));
    assert_eq!(plain.status, Some(101));
    let msgs: Vec<(bool, WsOpcode, Vec<u8>)> =
        plain.ws_messages.iter().map(|m| (m.from_client, m.opcode, m.data.to_vec())).collect();
    assert_eq!(msgs[0], (true, WsOpcode::Text, b"hello".to_vec()));
    assert_eq!(msgs[1], (false, WsOpcode::Text, b"hello".to_vec()));
    assert_eq!(msgs[2], (true, WsOpcode::Binary, vec![1, 2, 3]));
    assert_eq!(msgs[3], (false, WsOpcode::Binary, vec![1, 2, 3]));
    assert!(msgs.iter().any(|m| m.1 == WsOpcode::Close));
    assert!(plain.stream.as_ref().unwrap().bytes_up > 0);
    let secure = &entries[1];
    assert!(secure.tls_intercepted);
    assert_eq!(secure.url, format!("https://localhost:{}/secure", wss.port()));
    assert_eq!(secure.ws_messages[0].text(), "secret");
    assert_eq!(entries[2].ws_messages[0].text(), "abs");

    // HAR in Chrome's format: text as is, binary in base64, control frames left out.
    let har = to_har(entries.iter().filter(|e| e.kind == EntryKind::WebSocket).map(|e| e.as_ref()));
    let first = &har["log"]["entries"][0];
    assert_eq!(first["_resourceType"], "websocket");
    let msgs: Vec<(&str, u64, &str)> = first["_webSocketMessages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["type"].as_str().unwrap(), m["opcode"].as_u64().unwrap(), m["data"].as_str().unwrap()))
        .collect();
    assert_eq!(msgs, [("send", 1, "hello"), ("receive", 1, "hello"), ("send", 2, "AQID"), ("receive", 2, "AQID")]);
    let time = first["_webSocketMessages"][0]["time"].as_f64().unwrap();
    assert!((time - plain.started_at.timestamp_micros() as f64 / 1e6).abs() < 10.0, "epoch seconds: {time}");

    let item = secure.to_request_item(&ImportOptions::default()).unwrap();
    let RequestKind::WebSocket(w) = &item.request else { panic!("expected a websocket item") };
    assert_eq!(w.url, format!("wss://localhost:{}/secure", wss.port()));
    assert_eq!(w.messages[0].body, "secret");
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_down_is_reported() {
    let port = dead_port().await;
    let (proxy, ca) = start_proxy(|_| {}).await;
    let http = client(&proxy, Some(ca.cert_pem()));
    let r = http.get(format!("http://127.0.0.1:{port}/x")).send().await.unwrap();
    assert_eq!(r.status(), 502);
    assert!(r.headers().contains_key("x-merpati-proxy-error"));
    let r = http.get(format!("https://127.0.0.1:{port}/y")).send().await.unwrap();
    assert_eq!(r.status(), 502, "intercepted HTTPS gets a 502 inside TLS");
    let entries = finished(&proxy, 2).await;
    for e in &entries {
        assert_eq!(e.state, EntryState::Failed);
        assert!(e.error.as_deref().unwrap().contains("could not connect"), "{:?}", e.error);
        assert!(e.status.is_none());
    }
    proxy.stop().await;

    // Tunnel mode: the CONNECT itself fails.
    let (proxy, _) = start_proxy(|c| c.intercept_tls = false).await;
    let err = client(&proxy, None).get(format!("https://127.0.0.1:{port}/")).send().await.unwrap_err();
    assert!(err.is_connect() || err.is_request(), "{err:?}");
    let e = &finished(&proxy, 1).await[0];
    assert_eq!(e.kind, EntryKind::Tunnel);
    assert_eq!(e.state, EntryState::Failed);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chained_through_an_upstream_proxy() {
    let upstream = spawn_http().await;
    let cert = ca_signed_cert();
    let tls_upstream = spawn_https(&cert, &[b"http/1.1"]).await;
    // The outer proxy decrypts too (with its own CA) and does not verify the test server.
    let (outer, outer_ca) = start_proxy(|c| c.verify_upstream_tls = false).await;
    let outer_url = outer.url();
    let (inner, ca) = start_proxy(|c| {
        c.upstream_proxy = Some(UpstreamProxy::parse(&outer_url).unwrap());
        c.extra_upstream_roots_pem = Some(outer_ca.cert_pem().to_owned());
    })
    .await;
    let http = client(&inner, Some(ca.cert_pem()));
    assert_eq!(http.get(format!("http://{upstream}/hello")).send().await.unwrap().text().await.unwrap(), "hello world");
    let r = http.get(format!("https://localhost:{}/hello", tls_upstream.port())).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "hello world");
    let inner_entries = finished(&inner, 2).await;
    assert!(inner_entries.iter().all(|e| e.status == Some(200)), "{inner_entries:#?}");
    // Every upstream connection of the inner proxy went through the outer one (CONNECT).
    let outer_entries = finished(&outer, 2).await;
    let urls: Vec<&str> = outer_entries.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(
        urls,
        vec![format!("http://{upstream}/hello"), format!("https://localhost:{}/hello", tls_upstream.port())]
    );
    inner.stop().await;
    outer.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_page_serves_the_ca() {
    let (proxy, ca) = start_proxy(|_| {}).await;
    let http = client(&proxy, None);
    let pem = http.get("http://merpati.proxy/ca.pem").send().await.unwrap().text().await.unwrap();
    assert_eq!(pem, ca.cert_pem());
    let direct = reqwest::Client::builder().no_proxy().build().unwrap();
    let der = direct.get(format!("{}/ca.crt", proxy.url())).send().await.unwrap();
    assert_eq!(der.headers()["content-type"], "application/x-x509-ca-cert");
    assert_eq!(der.bytes().await.unwrap().as_ref(), ca.cert_der());
    let page = direct.get(proxy.url()).send().await.unwrap().text().await.unwrap();
    assert!(page.contains(&ca.fingerprint_sha256()));
    // Requests to the proxy itself are never forwarded in a loop.
    let r = http.get(format!("{}/ca.pem", proxy.url())).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(proxy.entries().is_empty());
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_callback_and_conversion() {
    let upstream = spawn_http().await;
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = count.clone();
    let (proxy, _) = start_proxy(move |c| {
        c.on_event = Some(Arc::new(move |_e: &ProxyEvent| {
            c2.fetch_add(1, Ordering::Relaxed);
        }))
    })
    .await;
    let mut rx = proxy.subscribe();
    let http = client(&proxy, None);
    http.post(format!("http://{upstream}/echo?v=2"))
        .header("content-type", "application/json")
        .header("x-trace", "abc")
        .body(r#"{"name":"merpati"}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let e = finished(&proxy, 1).await.remove(0);
    let mut kinds = Vec::new();
    while let Ok(ev) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        kinds.push(match ev.unwrap() {
            ProxyEvent::Added(_) => "added",
            ProxyEvent::Updated(e) if e.is_finished() => "finished",
            ProxyEvent::Updated(_) => "updated",
            _ => "other",
        });
    }
    assert_eq!(kinds, vec!["added", "updated", "finished"]);
    assert!(count.load(Ordering::Relaxed) >= 3);

    let item = e.to_request_item(&ImportOptions::default()).unwrap();
    assert_eq!(item.name, "POST /echo");
    let req = item.http().unwrap();
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, format!("http://{upstream}/echo?v=2"));
    assert_eq!(req.params[0].key, "v");
    let names: Vec<String> = req.headers.iter().map(|h| h.key.to_ascii_lowercase()).collect();
    for banned in ["host", "content-length", "connection", "proxy-connection", "accept-encoding"] {
        assert!(!names.contains(&banned.to_string()), "{banned} in {names:?}");
    }
    assert!(names.contains(&"x-trace".to_string()));
    assert_eq!(req.body.mode, BodyMode::Raw);
    assert_eq!(req.body.raw_language, RawLanguage::Json);
    assert_eq!(req.body.raw, r#"{"name":"merpati"}"#);
    assert_eq!(item.examples.len(), 1);
    assert_eq!(item.examples[0].status, 200);
    assert_eq!(item.examples[0].body, r#"{"name":"merpati"}"#);

    let har = to_har(proxy.entries().iter().map(|e| e.as_ref()));
    assert_eq!(har["log"]["entries"][0]["request"]["method"], "POST");
    assert_eq!(har["log"]["entries"][0]["response"]["content"]["text"], r#"{"name":"merpati"}"#);
    assert_eq!(har["log"]["entries"].as_array().unwrap().len(), 1);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_abort_marks_entry_failed() {
    let upstream = spawn_http().await;
    let (proxy, _) = start_proxy(|_| {}).await;
    let http = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(proxy.url()).unwrap())
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    assert!(http.get(format!("http://{upstream}/slow")).send().await.is_err());
    drop(http);
    let e = &finished(&proxy, 1).await[0];
    assert_eq!(e.state, EntryState::Failed);
    assert!(e.error.as_deref().unwrap().contains("aborted"), "{:?}", e.error);
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn head_and_empty_responses() {
    let upstream = spawn_http().await;
    let (proxy, _) = start_proxy(|_| {}).await;
    let http = client(&proxy, None);
    let r = http.head(format!("http://{upstream}/hello")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-length"], "11");
    assert!(r.bytes().await.unwrap().is_empty());
    for code in [204u16, 304, 404, 500] {
        let r = http.get(format!("http://{upstream}/status/{code}")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), code);
    }
    let entries = finished(&proxy, 5).await;
    assert!(entries.iter().all(|e| e.state == EntryState::Complete));
    assert_eq!(entries[0].method, "HEAD");
    assert!(entries[0].response_body.is_empty());
    assert_eq!(entries[2].status, Some(304));
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn userinfo_in_the_request_target_is_not_sent_as_host() {
    let upstream = spawn_http().await;
    let (proxy, _) = start_proxy(|_| {}).await;
    let mut s = tokio::net::TcpStream::connect(proxy.addr()).await.unwrap();
    let req =
        format!("GET http://user:secret@{upstream}/headers HTTP/1.1\r\nHost: other.test\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).await.unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    let body: serde_json::Value = serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["host"], upstream.to_string());
    let e = &finished(&proxy, 1).await[0];
    assert_eq!(e.request_header("host"), Some(upstream.to_string().as_str()));
    assert_eq!(e.url, format!("http://{upstream}/headers"));
    proxy.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_stop_and_ca_persistence() {
    init();
    let bad = ProxyConfig { bind: "0.0.0.0:0".parse().unwrap(), ..ProxyConfig::default() };
    assert!(matches!(CaptureProxy::start(bad).await, Err(ProxyError::NotLoopback(_))));

    let store = MemoryCaStore::new();
    let (ca1, created) = CertificateAuthority::load_or_generate(&store).unwrap();
    assert!(created);
    let (ca2, created) = CertificateAuthority::load_or_generate(&store).unwrap();
    assert!(!created);
    assert_eq!(ca1.cert_pem(), ca2.cert_pem());
    assert_eq!(ca1.fingerprint_sha256(), ca2.fingerprint_sha256());
    assert!(!format!("{:?}", store.get().unwrap()).contains("PRIVATE KEY"));
    let other = CertificateAuthority::generate().unwrap();
    assert!(CertificateAuthority::from_pem(ca1.cert_pem(), &other.to_pem().key_pem).is_err());
    assert!(ca1.trust_instructions("/tmp/ca.pem").contains("security add-trusted-cert"));
    assert!(ca1.not_after() > chrono::Utc::now() + chrono::Duration::days(3000));

    let (proxy, _) = start_proxy(|_| {}).await;
    let addr = proxy.addr();
    assert!(tokio::net::TcpStream::connect(addr).await.is_ok());
    proxy.stop().await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

/// Manual check against a real site: `cargo test -p merpati-proxy --test proxy -- --ignored real_world`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs internet access"]
async fn real_world_https_with_curl() {
    let (proxy, ca) = start_proxy(|_| {}).await;
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, ca.cert_pem()).unwrap();
    let out = curl(&["-x", &proxy.url(), "--cacert", ca_path.to_str().unwrap(), "https://example.com/"]).await;
    assert!(out.success, "{}", out.stderr);
    assert!(out.body.contains("Example Domain"));
    let e = &finished(&proxy, 1).await[0];
    eprintln!(
        "{} {} {:?} client={} upstream={:?} connect={:?} tls={:?} ttfb={:?} total={:?} bytes={}",
        e.method,
        e.url,
        e.status,
        e.http_version,
        e.upstream_http_version,
        e.timings.connect,
        e.timings.tls_handshake,
        e.timings.ttfb,
        e.timings.total,
        e.response_body.size
    );
    assert!(e.tls_intercepted);
    assert_eq!(e.status, Some(200));

    // Best effort: a few more real sites (printed, not asserted).
    for url in ["https://www.google.com/", "https://github.com/", "https://httpbin.org/gzip", "http://neverssl.com/"] {
        proxy.clear();
        let out =
            curl(&["-x", &proxy.url(), "--cacert", ca_path.to_str().unwrap(), "--compressed", "-o", "/dev/null", url])
                .await;
        let entries = wait_until(&proxy, "entry", |e| e.iter().any(|e| e.is_finished())).await;
        let e = &entries[0];
        let decoded = e.decoded_response_body();
        eprintln!(
            "{url}: curl ok={} status={} | entry {:?} client={} upstream={:?} enc={:?} raw={}B decoded={}B err={:?}",
            out.success,
            out.status,
            e.status,
            e.http_version,
            e.upstream_http_version,
            decoded.encoding,
            e.response_body.size,
            decoded.data.len(),
            e.error
        );
    }
    proxy.stop().await;
}
