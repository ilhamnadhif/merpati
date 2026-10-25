//! WebSocket connections through HTTP, HTTPS and SOCKS5 proxies (the
//! tunnels of `merpati_http::proxy`), against in-process proxies.

#[path = "support/proxies.rs"]
mod proxies;
#[path = "support/ws_echo.rs"]
mod ws_echo;

use merpati_http::proxy::{ProxyKind, proxy_for};
use merpati_http::tls::TlsSetup;
use proxies::{custom, http_proxy, https_proxy, socks5_proxy, tls_acceptor};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ws_echo::{echo_server, free_port, round_trip};

fn plain() -> TlsSetup {
    TlsSetup::default()
}

#[tokio::test]
async fn websocket_through_http_proxy() {
    let port = echo_server(None).await;
    let url = format!("ws://127.0.0.1:{port}/");
    let open = http_proxy(None).await;
    round_trip(url.clone(), custom(&open.url(), None), plain()).await.unwrap();
    assert_eq!(open.log(), vec![format!("CONNECT 127.0.0.1:{port}")]);

    let guarded = http_proxy(Some(("alice", "s3cret"))).await;
    round_trip(url.clone(), custom(&guarded.url(), Some(("alice", "s3cret"))), plain()).await.unwrap();
    // Credentials in the proxy URL work too.
    let in_url = guarded.url().replace("://", "://alice:s3cret@");
    round_trip(url, custom(&in_url, None), plain()).await.unwrap();
    assert_eq!(guarded.log(), vec![format!("CONNECT 127.0.0.1:{port}"); 2]);
}

#[tokio::test]
async fn websocket_through_socks5() {
    let port = echo_server(None).await;
    // socks5h: the proxy resolves the host name.
    let open = socks5_proxy(None).await;
    round_trip(format!("ws://localhost:{port}/"), custom(&open.url(), None), plain()).await.unwrap();
    assert_eq!(open.log(), vec![format!("SOCKS5 localhost:{port}")]);

    let guarded = socks5_proxy(Some(("bob", "pa:ss"))).await;
    let settings = custom(&guarded.url(), Some(("bob", "pa:ss")));
    round_trip(format!("ws://127.0.0.1:{port}/"), settings, plain()).await.unwrap();
    assert_eq!(guarded.log(), vec![format!("SOCKS5 127.0.0.1:{port}")]);
}

#[tokio::test]
async fn socks5_without_h_resolves_locally() {
    // Listen on both loopback addresses: "localhost" may resolve to either first.
    let (v4, v6, port) = loop {
        let v4 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = v4.local_addr().unwrap().port();
        if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
            break (v4, Some(v6), port);
        }
        if tokio::net::TcpListener::bind("[::1]:0").await.is_err() {
            break (v4, None, port); // no IPv6 loopback at all
        }
    };
    for listener in std::iter::once(v4).chain(v6) {
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let _ = s.write_all(b"hi").await;
            }
        });
    }
    let proxy = socks5_proxy(None).await;
    let via = proxy_for(&custom(&proxy.url_as("socks5"), None), "localhost", port, false).unwrap().unwrap();
    assert_eq!(via.kind(), ProxyKind::Socks5 { remote_dns: false });
    let mut stream = via.tunnel("localhost", port, &plain(), true).await.unwrap();
    let mut hi = [0u8; 2];
    stream.read_exact(&mut hi).await.unwrap();
    assert_eq!(&hi, b"hi");
    let log = proxy.log();
    assert!(log.len() == 1 && !log[0].contains("localhost"), "the proxy got a name: {log:?}");
}

#[tokio::test]
async fn wss_runs_tls_end_to_end_through_the_tunnel() {
    let (acceptor, pem) = tls_acceptor();
    let port = echo_server(Some(acceptor)).await;
    let url = format!("wss://localhost:{port}/");
    let proxy = http_proxy(None).await;
    let trusted = TlsSetup { extra_roots_pem: Some(pem), identity: None };
    round_trip(url.clone(), custom(&proxy.url(), None), trusted).await.unwrap();
    assert_eq!(proxy.log(), vec![format!("CONNECT localhost:{port}")]);
    // Verification stays on through the tunnel: an unknown CA is refused.
    let err = round_trip(url, custom(&proxy.url(), None), plain()).await.unwrap_err();
    assert!(err.to_lowercase().contains("certificate"), "{err}");
}

#[tokio::test]
async fn https_proxy_is_reached_over_tls() {
    let (acceptor, pem) = tls_acceptor();
    let proxy = https_proxy(Some(("alice", "s3cret")), acceptor).await;
    let port = echo_server(None).await;
    let url = format!("ws://127.0.0.1:{port}/");
    let settings = custom(&proxy.url(), Some(("alice", "s3cret")));
    let trusted = TlsSetup { extra_roots_pem: Some(pem), identity: None };
    round_trip(url.clone(), settings.clone(), trusted).await.unwrap();
    assert_eq!(proxy.log(), vec![format!("CONNECT 127.0.0.1:{port}")]);
    // The proxy's certificate is verified like a server's.
    let err = round_trip(url, settings, plain()).await.unwrap_err();
    assert!(err.contains(&format!("TLS handshake with proxy {}", proxy.url())), "{err}");
}

#[tokio::test]
async fn wrong_or_missing_proxy_credentials_are_explained() {
    let port = echo_server(None).await;
    let url = format!("ws://127.0.0.1:{port}/");
    let http = http_proxy(Some(("alice", "s3cret"))).await;
    let err = round_trip(url.clone(), custom(&http.url(), Some(("alice", "hunter2"))), plain()).await.unwrap_err();
    assert!(err.contains("407") && err.contains("rejected the proxy credentials"), "{err}");
    assert!(err.contains(&http.url()) && !err.contains("hunter2") && !err.contains("alice"), "{err}");
    let err = round_trip(url.clone(), custom(&http.url(), None), plain()).await.unwrap_err();
    assert!(err.contains("407") && err.contains("requires authentication"), "{err}");
    assert!(http.accepted().is_empty());

    let socks = socks5_proxy(Some(("alice", "s3cret"))).await;
    let err = round_trip(url.clone(), custom(&socks.url(), Some(("alice", "hunter2"))), plain()).await.unwrap_err();
    assert!(err.contains("SOCKS5 proxy") && err.contains("rejected the proxy credentials"), "{err}");
    assert!(!err.contains("hunter2") && !err.contains("alice"), "{err}");
    let err = round_trip(url, custom(&socks.url(), None), plain()).await.unwrap_err();
    assert!(err.contains("requires authentication"), "{err}");
    assert!(socks.accepted().is_empty());
}

#[tokio::test]
async fn bypass_list_goes_direct() {
    let port = echo_server(None).await;
    let proxy = http_proxy(None).await;
    let mut settings = custom(&proxy.url(), None);
    settings.bypass = "example.com, 127.0.0.1".into();
    round_trip(format!("ws://127.0.0.1:{port}/"), settings.clone(), plain()).await.unwrap();
    settings.bypass = "*.example.com; localhost".into();
    round_trip(format!("ws://localhost:{port}/"), settings.clone(), plain()).await.unwrap();
    assert!(proxy.log().is_empty(), "{:?}", proxy.log());
    // Hosts not listed still go through the proxy, loopback included (as for HTTP).
    round_trip(format!("ws://127.0.0.1:{port}/"), settings, plain()).await.unwrap();
    assert_eq!(proxy.log(), vec![format!("CONNECT 127.0.0.1:{port}")]);
}

#[tokio::test]
async fn proxy_failures_are_explained() {
    let port = echo_server(None).await;
    let url = format!("ws://127.0.0.1:{port}/");
    let dead = free_port();
    let err = round_trip(url.clone(), custom(&format!("http://127.0.0.1:{dead}"), None), plain()).await.unwrap_err();
    assert!(
        err.contains(&format!("Could not connect to the proxy http://127.0.0.1:{dead}: connection refused")),
        "{err}"
    );

    // The proxy cannot reach the server.
    let proxy = http_proxy(None).await;
    let err = round_trip(format!("ws://127.0.0.1:{dead}/"), custom(&proxy.url(), None), plain()).await.unwrap_err();
    assert!(err.contains(&format!("could not open a tunnel to 127.0.0.1:{dead}: HTTP 502")), "{err}");
    let socks = socks5_proxy(None).await;
    let err = round_trip(format!("ws://127.0.0.1:{dead}/"), custom(&socks.url(), None), plain()).await.unwrap_err();
    assert!(err.contains("could not connect to") && err.contains("connection refused"), "{err}");

    // Unusable settings, without quoting credentials.
    let err = round_trip(url, custom("ftp://user:hunter2@proxy", None), plain()).await.unwrap_err();
    assert!(err.contains("Unsupported proxy scheme") && !err.contains("hunter2"), "{err}");
}

#[tokio::test]
async fn pac_script_chooses_the_proxy() {
    let port = echo_server(None).await;
    let proxy = http_proxy(Some(("alice", "s3cret"))).await;
    let dir = tempfile::tempdir().unwrap();
    let pac = dir.path().join("proxy.pac");
    let addr = proxy.url().trim_start_matches("http://").to_string();
    let script = format!(
        "function FindProxyForURL(url, host) {{\n  if (host === 'localhost') return 'DIRECT';\n  return 'PROXY {addr}; DIRECT';\n}}"
    );
    std::fs::write(&pac, script).unwrap();
    // The script wins over the custom proxy URL; the credentials apply to the proxies it picks.
    let mut settings = custom("http://unused.invalid:1", Some(("alice", "s3cret")));
    settings.pac_url = url::Url::from_file_path(&pac).unwrap().to_string();
    round_trip(format!("ws://127.0.0.1:{port}/"), settings.clone(), plain()).await.unwrap();
    round_trip(format!("ws://localhost:{port}/"), settings.clone(), plain()).await.unwrap();
    assert_eq!(proxy.log(), vec![format!("CONNECT 127.0.0.1:{port}")]);

    // The HTTP engine follows the same script.
    let app = axum::Router::new().route("/x", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = merpati_http::EngineConfig { proxy: settings.clone(), ..Default::default() };
    let engine = merpati_http::HttpEngine::new(config, std::sync::Arc::new(merpati_http::CookieJar::new()));
    let client = engine.default_client().unwrap();
    let body = client.get(format!("http://web.test:{server}/x")).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, "ok");
    assert_eq!(proxy.log().last().unwrap(), &format!("GET web.test:{server}"));
    let before = proxy.log().len();
    client.get(format!("http://localhost:{server}/x")).send().await.unwrap();
    assert_eq!(proxy.log().len(), before);

    // A script that cannot be loaded is a configuration error.
    settings.pac_url = url::Url::from_file_path(dir.path().join("missing.pac")).unwrap().to_string();
    let err = proxy_for(&settings, "a.test", 80, false).unwrap_err();
    assert_eq!(err.kind, merpati_http::proxy::ProxyErrorKind::Config);
    assert!(err.message.starts_with("Proxy auto-config script: cannot read"), "{err}");
}
