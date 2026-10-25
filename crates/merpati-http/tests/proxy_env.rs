//! The system proxy from environment variables, for the tunnels and the
//! HTTP engine alike. Environment variables are process-wide, so this test
//! binary holds a single test that sets them before anything reads them.

#[path = "support/proxies.rs"]
mod proxies;
#[path = "support/ws_echo.rs"]
mod ws_echo;

use std::sync::Arc;

use merpati_core::settings::ProxySettings;
use merpati_http::proxy::proxy_for;
use merpati_http::tls::TlsSetup;
use merpati_http::{CookieJar, EngineConfig, HttpEngine};
use tokio::io::AsyncWriteExt;
use ws_echo::{echo_server, round_trip};

/// An HTTP server answering every request with "ok"; returns its port.
async fn ok_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
            });
        }
    });
    port
}

#[tokio::test]
async fn system_proxy_from_environment() {
    let http = proxies::http_proxy(None).await;
    let socks = proxies::socks5_proxy(None).await;
    // SAFETY: the only test in this binary; nothing else reads the
    // environment concurrently.
    unsafe {
        for k in ["http_proxy", "https_proxy", "all_proxy", "no_proxy", "ALL_PROXY", "REQUEST_METHOD"] {
            std::env::remove_var(k);
        }
        std::env::set_var("HTTP_PROXY", http.url());
        std::env::set_var("HTTPS_PROXY", socks.url());
        std::env::set_var("NO_PROXY", "bypassed.test, 10.0.0.0/8");
    }
    let system = ProxySettings { use_system: true, ..Default::default() };

    // Plain connections use HTTP_PROXY, TLS ones HTTPS_PROXY, NO_PROXY is honoured.
    let p = proxy_for(&system, "example.com", 80, false).unwrap().unwrap();
    assert_eq!(p.to_string(), http.url());
    let p = proxy_for(&system, "example.com", 443, true).unwrap().unwrap();
    assert_eq!(p.to_string(), socks.url());
    for host in ["api.bypassed.test", "bypassed.test", "10.1.2.3"] {
        assert_eq!(proxy_for(&system, host, 80, false).unwrap(), None, "{host}");
    }
    // Like browsers, loopback never goes through the system proxy.
    for host in ["localhost", "app.localhost", "127.0.0.1", "127.8.9.10", "::1", "[::1]"] {
        assert_eq!(proxy_for(&system, host, 80, false).unwrap(), None, "{host}");
    }

    // A WebSocket session goes through HTTP_PROXY (`.test` names are this
    // machine for the test proxies).
    let port = echo_server(None).await;
    let url = format!("ws://echo.test:{port}/");
    round_trip(url.clone(), system.clone(), TlsSetup::default()).await.unwrap();
    assert_eq!(http.log(), vec![format!("CONNECT echo.test:{port}")]);
    // Loopback and "use system proxy" off: direct.
    let direct = format!("ws://127.0.0.1:{port}/");
    round_trip(direct.clone(), system.clone(), TlsSetup::default()).await.unwrap();
    round_trip(direct, ProxySettings::default(), TlsSetup::default()).await.unwrap();
    assert_eq!(http.log().len(), 1);
    // The custom proxy wins over the environment.
    let custom = proxies::http_proxy(None).await;
    let mut both = proxies::custom(&custom.url(), None);
    both.use_system = true;
    round_trip(url, both, TlsSetup::default()).await.unwrap();
    assert_eq!((http.log().len(), custom.log().len()), (1, 1));

    // The HTTP engine uses the same resolver: the variables, and loopback direct.
    let server = ok_server().await;
    let engine = HttpEngine::new(EngineConfig { proxy: system, ..Default::default() }, Arc::new(CookieJar::new()));
    let client = engine.default_client().unwrap();
    let body = client.get(format!("http://web.test:{server}/x")).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, "ok");
    assert_eq!(http.log().last().unwrap(), &format!("GET web.test:{server}"));
    let before = http.log().len();
    let body = client.get(format!("http://127.0.0.1:{server}/y")).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, "ok");
    assert_eq!(http.log().len(), before);
}
