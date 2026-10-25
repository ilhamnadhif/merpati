//! Several proxies in a PAC script's answer (`PROXY a; PROXY b; DIRECT`):
//! the tunnels and the HTTP engine fall over to the next one when a proxy
//! cannot be reached. And proxy auto-discovery (WPAD) against a local
//! server standing in for `wpad.<domain>`.
//!
//! Unreachable proxies are remembered process-wide, so every test uses its
//! own ports.

#[path = "support/proxies.rs"]
mod proxies;
#[path = "support/ws_echo.rs"]
mod ws_echo;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use merpati_core::prepared::PreparedRequest;
use merpati_core::settings::ProxySettings;
use merpati_http::proxy::{ProxyErrorKind, discover_wpad_at, proxy_for};
use merpati_http::tls::TlsSetup;
use merpati_http::{CookieJar, EngineConfig, HttpEngine, HttpErrorKind, SendOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ws_echo::{echo_server, free_port, round_trip};

/// Custom proxy settings with the PAC script `script` (a file in `dir`).
fn pac_settings(dir: &tempfile::TempDir, name: &str, script: &str) -> ProxySettings {
    let path = dir.path().join(name);
    std::fs::write(&path, script).unwrap();
    ProxySettings {
        custom_enabled: true,
        pac_url: url::Url::from_file_path(&path).unwrap().to_string(),
        ..Default::default()
    }
}

/// A script answering `result` for every URL.
fn always(result: &str) -> String {
    format!("function FindProxyForURL(url, host) {{ return '{result}'; }}")
}

fn names(fallbacks: &[Option<merpati_http::proxy::ProxyServer>]) -> Vec<String> {
    fallbacks.iter().map(|f| f.as_ref().map_or("DIRECT".into(), |p| p.to_string())).collect()
}

#[tokio::test]
async fn tunnels_fall_over_to_the_next_proxy() {
    let port = echo_server(None).await;
    let live = proxies::http_proxy(None).await;
    let dead = free_port();
    let dir = tempfile::tempdir().unwrap();
    let live_addr = live.url().trim_start_matches("http://").to_string();
    let settings = pac_settings(&dir, "a.pac", &always(&format!("PROXY 127.0.0.1:{dead}; PROXY {live_addr}; DIRECT")));

    let first = proxy_for(&settings, "echo.test", port, false).unwrap().unwrap();
    assert_eq!(first.to_string(), format!("http://127.0.0.1:{dead}"));
    assert_eq!(names(first.fallbacks()), [live.url(), "DIRECT".into()]);
    // `.test` names are this machine for the test proxy only, so the
    // WebSocket session can only have gone through the live proxy.
    round_trip(format!("ws://echo.test:{port}/"), settings.clone(), TlsSetup::default()).await.unwrap();
    assert_eq!(live.log(), [format!("CONNECT echo.test:{port}")]);

    // The dead proxy is now tried last.
    let next = proxy_for(&settings, "echo.test", port, true).unwrap().unwrap();
    assert_eq!(next.to_string(), live.url());
    assert_eq!(names(next.fallbacks()), ["DIRECT".to_string(), format!("http://127.0.0.1:{dead}")]);
}

#[tokio::test]
async fn direct_is_the_last_resort() {
    let port = echo_server(None).await;
    let (dead_a, dead_b) = (free_port(), free_port());
    let dir = tempfile::tempdir().unwrap();
    let with_direct = pac_settings(&dir, "a.pac", &always(&format!("PROXY 127.0.0.1:{dead_a}; DIRECT")));
    round_trip(format!("ws://127.0.0.1:{port}/"), with_direct, TlsSetup::default()).await.unwrap();

    // Without DIRECT the error names the last proxy tried.
    let none_left =
        pac_settings(&dir, "b.pac", &always(&format!("PROXY 127.0.0.1:{dead_b}; SOCKS5 127.0.0.1:{dead_a}")));
    let via = proxy_for(&none_left, "echo.test", port, false).unwrap().unwrap();
    let err = via.tunnel("echo.test", port, &TlsSetup::default(), true).await.unwrap_err();
    assert_eq!(err.kind, ProxyErrorKind::Unreachable);
    assert!(err.message.contains(&format!("socks5h://127.0.0.1:{dead_a}")), "{err}");
}

/// An HTTP server answering "ok" to every request; returns its port.
async fn ok_server() -> u16 {
    let app = axum::Router::new().fallback(|| async { "ok" });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

fn get(url: String) -> PreparedRequest {
    PreparedRequest { method: "GET".into(), url, ..Default::default() }
}

#[tokio::test]
async fn http_requests_retry_with_the_next_proxy() {
    let server = ok_server().await;
    let live = proxies::http_proxy(None).await;
    let dead = free_port();
    let dir = tempfile::tempdir().unwrap();
    let live_addr = live.url().trim_start_matches("http://").to_string();
    let settings = pac_settings(&dir, "a.pac", &always(&format!("PROXY 127.0.0.1:{dead}; PROXY {live_addr}")));
    let engine = HttpEngine::new(EngineConfig { proxy: settings, ..Default::default() }, Arc::new(CookieJar::new()));
    let out = engine.send(&get(format!("http://web.test:{server}/a")), &SendOptions::default()).await.unwrap();
    assert_eq!((out.response.status, out.response.body.text_lossy().as_str()), (200, "ok"));
    assert_eq!(live.log(), [format!("GET web.test:{server}")]);

    // reqwest's own choice skips the dead proxy from now on: a plain
    // client of the engine (no retry) gets through at once.
    let client = engine.default_client().unwrap();
    let body = client.get(format!("http://web.test:{server}/b")).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, "ok");
    assert_eq!(live.log().len(), 2);

    // No other choice: the connection error as it is.
    let alone = pac_settings(&dir, "b.pac", &always(&format!("PROXY 127.0.0.1:{}", free_port())));
    let engine = HttpEngine::new(EngineConfig { proxy: alone, ..Default::default() }, Arc::new(CookieJar::new()));
    let err = engine.send(&get(format!("http://web.test:{server}/c")), &SendOptions::default()).await.unwrap_err();
    assert_eq!(err.kind, HttpErrorKind::ConnectionRefused, "{err:?}");
}

/// Stands in for every `wpad.<domain>` host: serves `script` for
/// `wpad.corp.<tag>.test`, never answers `wpad.slow.<tag>.test` and sends
/// 404 for the others. Records the `Host` of each request.
async fn wpad_server(tag: &'static str, script: String) -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hosts: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = hosts.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let (seen, script) = (seen.clone(), script.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if s.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                let host = head.lines().find_map(|l| l.strip_prefix("host:")).unwrap_or_default().trim().to_string();
                let path_ok = head.starts_with("get /wpad.dat ");
                seen.lock().unwrap().push(host.clone());
                if host == format!("wpad.slow.{tag}.test") {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    return;
                }
                let answer = if host == format!("wpad.corp.{tag}.test") && path_ok {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ns-proxy-autoconfig\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{script}",
                        script.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                };
                let _ = s.write_all(answer.as_bytes()).await;
            });
        }
    });
    (addr, hosts)
}

#[tokio::test]
async fn wpad_discovery_tries_each_search_domain_in_turn() {
    let script = always("PROXY proxy.corp.example:3128; DIRECT");
    let (addr, hosts) = wpad_server("wpad-a", script).await;
    let domains: Vec<String> =
        ["missing.wpad-a.test", "com", "slow.wpad-a.test.", "Corp.wpad-a.test", "never.wpad-a.test"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    let started = Instant::now();
    let (url, resolver) = tokio::task::spawn_blocking({
        let domains = domains.clone();
        move || discover_wpad_at(&domains, addr)
    })
    .await
    .unwrap()
    .expect("the script on wpad.corp");
    assert_eq!(url, "http://wpad.corp.wpad-a.test/wpad.dat");
    // The silent candidate gave up after its 2 s; a public suffix (`com`)
    // was never asked, nor was anything after the first script found.
    let took = started.elapsed();
    assert!(took >= Duration::from_secs(2) && took < Duration::from_secs(8), "{took:?}");
    assert_eq!(*hosts.lock().unwrap(), ["wpad.missing.wpad-a.test", "wpad.slow.wpad-a.test", "wpad.corp.wpad-a.test"]);

    // The script chooses the proxy; loopback stays direct.
    let p = resolver.proxy_for("api.example.org", 443, true).unwrap();
    assert_eq!((p.to_string(), names(p.fallbacks())), ("http://proxy.corp.example:3128".into(), vec!["DIRECT".into()]));
    assert_eq!(resolver.proxy_for("localhost", 80, false), None);

    // Results, failures included, are cached.
    let started = Instant::now();
    let again = tokio::task::spawn_blocking(move || discover_wpad_at(&domains, addr)).await.unwrap();
    assert_eq!(again.map(|(u, _)| u), Some(url));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(hosts.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn wpad_discovery_without_a_script() {
    let (addr, hosts) = wpad_server("wpad-b", String::new()).await;
    let domains = vec!["missing.wpad-b.test".to_string(), "co.uk".to_string(), "local".to_string()];
    let found = tokio::task::spawn_blocking(move || discover_wpad_at(&domains, addr)).await.unwrap();
    assert!(found.is_none());
    assert_eq!(*hosts.lock().unwrap(), ["wpad.missing.wpad-b.test"]);
    // A server that is not there at all.
    let gone = std::net::SocketAddr::from(([127, 0, 0, 1], free_port()));
    let found = tokio::task::spawn_blocking(move || discover_wpad_at(&["x.wpad-b.test".to_string()], gone)).await;
    assert!(found.unwrap().is_none());
}
