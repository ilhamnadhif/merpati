//! Tunnels through an HTTP proxy that requires Digest authentication
//! (RFC 7616), with and without keeping the connection open after its 407,
//! and through one asking for a scheme Merpati does not speak.

#[path = "support/proxies.rs"]
mod proxies;
#[path = "support/ws_echo.rs"]
mod ws_echo;

use std::sync::{Arc, Mutex};

use md5::Md5;
use merpati_http::challenge::parse_challenges;
use merpati_http::tls::TlsSetup;
use proxies::custom;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use ws_echo::{echo_server, round_trip};

const NONCE: &str = "dcd98b7102dd2f0e8b11d0f600bfb0c093";

#[derive(Clone, Copy)]
enum Mode {
    /// SHA-256 Digest (plus a Basic challenge it does not accept); the 407
    /// has a body and keeps the connection open.
    DigestKeepAlive,
    /// MD5 Digest without qop (RFC 2069); the 407 closes the connection.
    DigestClose,
    /// Only NTLM.
    Ntlm,
}

struct DigestProxy {
    port: u16,
    /// `conn`, `407 <credentials sent>` and `CONNECT <target>` lines.
    log: Arc<Mutex<Vec<String>>>,
}

impl DigestProxy {
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Option<String> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        if s.read(&mut b).await.ok()? == 0 {
            return None;
        }
        head.push(b[0]);
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

/// Checks a `Proxy-Authorization: Digest ...` value for alice / s3cret.
fn valid_digest(value: &str, target: &str, sha256: bool) -> bool {
    let challenges = parse_challenges([value]);
    let Some(d) = challenges.iter().find(|c| c.scheme == "digest") else { return false };
    let p = |k: &str| d.param(k).unwrap_or_default().to_string();
    let h = |s: String| if sha256 { hex::encode(Sha256::digest(s)) } else { hex::encode(Md5::digest(s)) };
    let ha1 = h(format!("{}:proxy:s3cret", p("username")));
    let ha2 = h(format!("CONNECT:{}", p("uri")));
    let expected = if p("qop").is_empty() {
        h(format!("{ha1}:{}:{ha2}", p("nonce")))
    } else {
        h(format!("{ha1}:{}:{}:{}:{}:{ha2}", p("nonce"), p("nc"), p("cnonce"), p("qop")))
    };
    p("username") == "alice" && p("uri") == target && p("nonce") == NONCE && p("response") == expected
}

async fn digest_proxy(mode: Mode) -> DigestProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let shared = log.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            tokio::spawn(conn(s, mode, shared.clone()));
        }
    });
    DigestProxy { port, log }
}

async fn conn(mut s: TcpStream, mode: Mode, log: Arc<Mutex<Vec<String>>>) {
    log.lock().unwrap().push("conn".into());
    while let Some(head) = read_head(&mut s).await {
        let target = head.strip_prefix("CONNECT ").and_then(|r| r.split(' ').next()).unwrap_or_default().to_string();
        let auth = head
            .lines()
            .find_map(|l| l.strip_prefix("Proxy-Authorization: "))
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        let ok = match mode {
            Mode::DigestKeepAlive => valid_digest(&auth, &target, true),
            Mode::DigestClose => valid_digest(&auth, &target, false),
            Mode::Ntlm => false,
        };
        if ok {
            log.lock().unwrap().push(format!("CONNECT {target}"));
            let Ok(mut upstream) = TcpStream::connect(target.as_str()).await else { return };
            if s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.is_ok() {
                let _ = tokio::io::copy_bidirectional(&mut s, &mut upstream).await;
            }
            return;
        }
        let sent = auth.split(' ').next().filter(|a| !a.is_empty()).unwrap_or("none").to_ascii_lowercase();
        log.lock().unwrap().push(format!("407 {sent}"));
        let answer = match mode {
            Mode::DigestKeepAlive => format!(
                "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\n\
                 Proxy-Authenticate: Digest realm=\"proxy\", qop=\"auth,auth-int\", algorithm=SHA-256, nonce=\"{NONCE}\"\r\n\
                 Content-Length: 6\r\n\r\ndenied"
            ),
            Mode::DigestClose => format!(
                "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Digest realm=\"proxy\", nonce=\"{NONCE}\"\r\n\
                 Connection: close\r\n\r\n"
            ),
            Mode::Ntlm => "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: NTLM\r\n\
                 Proxy-Authenticate: Negotiate\r\nConnection: close\r\n\r\n"
                .to_string(),
        };
        if s.write_all(answer.as_bytes()).await.is_err() || !matches!(mode, Mode::DigestKeepAlive) {
            return;
        }
    }
}

async fn tunnel_through(proxy: &DigestProxy, auth: Option<(&str, &str)>) -> Result<(), String> {
    let port = echo_server(None).await;
    round_trip(format!("ws://127.0.0.1:{port}/"), custom(&proxy.url(), auth), TlsSetup::default()).await
}

#[tokio::test]
async fn digest_proxy_keeping_the_connection_open() {
    let proxy = digest_proxy(Mode::DigestKeepAlive).await;
    tunnel_through(&proxy, Some(("alice", "s3cret"))).await.unwrap();
    // Basic first (as reqwest sends it), then Digest on the same connection.
    let log = proxy.log();
    assert_eq!(log[..2], ["conn", "407 basic"]);
    assert!(log[2].starts_with("CONNECT 127.0.0.1:") && log.len() == 3, "{log:?}");

    // The proxy is known to want Digest: Basic credentials (the password in
    // clear) are no longer sent.
    tunnel_through(&proxy, Some(("alice", "s3cret"))).await.unwrap();
    let log = proxy.log();
    assert_eq!(log[3..5], ["conn", "407 none"]);
    assert!(log[5].starts_with("CONNECT 127.0.0.1:") && log.len() == 6, "{log:?}");
}

#[tokio::test]
async fn digest_proxy_closing_the_connection() {
    let proxy = digest_proxy(Mode::DigestClose).await;
    tunnel_through(&proxy, Some(("alice", "s3cret"))).await.unwrap();
    // A new connection for the Digest answer.
    let log = proxy.log();
    assert_eq!(log[..3], ["conn", "407 basic", "conn"]);
    assert!(log[3].starts_with("CONNECT 127.0.0.1:") && log.len() == 4, "{log:?}");
}

#[tokio::test]
async fn digest_proxy_with_wrong_or_missing_credentials() {
    let proxy = digest_proxy(Mode::DigestKeepAlive).await;
    let err = tunnel_through(&proxy, Some(("alice", "hunter2"))).await.unwrap_err();
    assert!(err.contains("407") && err.contains("rejected the proxy credentials"), "{err}");
    assert!(!err.contains("hunter2") && !err.contains("alice"), "{err}");
    assert_eq!(proxy.log(), ["conn", "407 basic", "407 digest"]);
    let err = tunnel_through(&proxy, None).await.unwrap_err();
    assert!(err.contains("requires authentication"), "{err}");
}

#[tokio::test]
async fn unsupported_proxy_schemes_are_named() {
    let proxy = digest_proxy(Mode::Ntlm).await;
    let err = tunnel_through(&proxy, Some(("alice", "s3cret"))).await.unwrap_err();
    assert!(err.contains("asks for NTLM/Negotiate authentication") && err.contains("not supported"), "{err}");
}
