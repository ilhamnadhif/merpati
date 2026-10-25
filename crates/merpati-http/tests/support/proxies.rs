//! Minimal in-process proxies for the proxy tests of every protocol crate
//! (included with `#[path = ".../merpati-http/tests/support/proxies.rs"]`):
//!
//! - an HTTP proxy: `CONNECT` tunnels, plus forwarding of absolute-form
//!   requests (`GET http://host/...`, one request per connection, rewritten
//!   to origin form), optionally behind TLS (an `https://` proxy);
//! - a SOCKS5 proxy (no authentication or username/password).
//!
//! Both optionally require credentials and record what clients asked for.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct TestProxy {
    pub port: u16,
    scheme: &'static str,
    log: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestProxy {
    /// `http://127.0.0.1:<port>`, `https://localhost:<port>` or `socks5h://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        let host = if self.scheme == "https" { "localhost" } else { "127.0.0.1" };
        format!("{}://{host}:{}", self.scheme, self.port)
    }

    /// The same proxy under another scheme (`socks5` vs `socks5h`).
    pub fn url_as(&self, scheme: &str) -> String {
        format!("{scheme}://127.0.0.1:{}", self.port)
    }

    /// Everything clients asked for: `CONNECT host:port`, `GET host:port`
    /// (forwarded request), `SOCKS5 host:port`; refused credentials are
    /// logged as `DENIED ...`.
    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// The accepted requests only.
    pub fn accepted(&self) -> Vec<String> {
        self.log().into_iter().filter(|l| !l.starts_with("DENIED")).collect()
    }
}

/// `Basic base64(user:pass)`.
pub fn basic(user: &str, pass: &str) -> String {
    format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()))
}

/// Standard base64, without a dependency.
fn base64(input: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// An HTTP proxy on 127.0.0.1; `auth` = required username and password.
/// `host:port` to dial: names under `.test` (reserved, RFC 6761) are this
/// machine, so tests can use non-loopback names the system proxy applies to.
fn upstream_addr(authority: &str) -> String {
    match authority.rsplit_once(':') {
        Some((host, port)) if host.ends_with(".test") => format!("127.0.0.1:{port}"),
        _ => authority.to_string(),
    }
}

pub async fn http_proxy(auth: Option<(&str, &str)>) -> TestProxy {
    start_http(auth, None).await
}

/// An HTTP proxy behind TLS (an `https://` proxy) with `acceptor`'s
/// certificate (made for `localhost`).
pub async fn https_proxy(auth: Option<(&str, &str)>, acceptor: tokio_rustls::TlsAcceptor) -> TestProxy {
    start_http(auth, Some(acceptor)).await
}

async fn start_http(auth: Option<(&str, &str)>, tls: Option<tokio_rustls::TlsAcceptor>) -> TestProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let expected = auth.map(|(u, p)| basic(u, p));
    let scheme = if tls.is_some() { "https" } else { "http" };
    let task = {
        let log = log.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (log, expected, tls) = (log.clone(), expected.clone(), tls.clone());
                tokio::spawn(async move {
                    match tls {
                        Some(acceptor) => {
                            if let Ok(s) = acceptor.accept(tcp).await {
                                http_conn(s, expected, log).await;
                            }
                        }
                        None => http_conn(tcp, expected, log).await,
                    }
                });
            }
        })
    };
    TestProxy { port, scheme, log, task }
}

async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Option<Vec<u8>> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        if s.read(&mut b).await.ok()? == 0 || head.len() > 64 * 1024 {
            return None;
        }
        head.push(b[0]);
    }
    Some(head)
}

async fn http_conn<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    expected: Option<String>,
    log: Arc<Mutex<Vec<String>>>,
) {
    let Some(head) = read_head(&mut s).await else { return };
    let head = String::from_utf8_lossy(&head).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, target) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let authority = match target.strip_prefix("http://") {
        Some(rest) => rest.split('/').next().unwrap_or_default().to_string(),
        None => target.clone(),
    };
    if let Some(want) = &expected {
        let got = headers.iter().find(|(k, _)| k == "proxy-authorization").map(|(_, v)| v.as_str());
        if got != Some(want.as_str()) {
            log.lock().unwrap().push(format!("DENIED {method} {authority}"));
            let _ = s
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        }
    }
    log.lock().unwrap().push(format!("{method} {authority}"));
    let Ok(mut upstream) = TcpStream::connect(upstream_addr(&authority).as_str()).await else {
        let _ = s.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        return;
    };
    if method == "CONNECT" {
        if s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.is_err() {
            return;
        }
    } else {
        // Forwarding: origin form, without the proxy headers.
        let path = target.strip_prefix("http://").and_then(|r| r.find('/').map(|i| &r[i..])).unwrap_or("/");
        let mut out = format!("{method} {path} HTTP/1.1\r\n");
        for (k, v) in &headers {
            if !k.starts_with("proxy-") {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
        }
        out.push_str("\r\n");
        if upstream.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
    let _ = tokio::io::copy_bidirectional(&mut s, &mut upstream).await;
}

/// A SOCKS5 proxy on 127.0.0.1; `auth` = required username and password.
pub async fn socks5_proxy(auth: Option<(&str, &str)>) -> TestProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let auth = auth.map(|(u, p)| (u.to_string(), p.to_string()));
    let task = {
        let log = log.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (log, auth) = (log.clone(), auth.clone());
                tokio::spawn(async move {
                    let _ = socks_conn(tcp, auth, log).await;
                });
            }
        })
    };
    TestProxy { port, scheme: "socks5h", log, task }
}

async fn socks_conn(
    mut s: TcpStream,
    auth: Option<(String, String)>,
    log: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let mut hdr = [0u8; 2];
    s.read_exact(&mut hdr).await?;
    let mut methods = vec![0u8; hdr[1] as usize];
    s.read_exact(&mut methods).await?;
    let wanted = if auth.is_some() { 2 } else { 0 };
    if hdr[0] != 5 || !methods.contains(&wanted) {
        log.lock().unwrap().push("DENIED SOCKS5 no acceptable method".into());
        return s.write_all(&[5, 0xFF]).await;
    }
    s.write_all(&[5, wanted]).await?;
    if let Some((user, pass)) = &auth {
        let mut v = [0u8; 2];
        s.read_exact(&mut v).await?;
        let mut u = vec![0u8; v[1] as usize];
        s.read_exact(&mut u).await?;
        let mut plen = [0u8; 1];
        s.read_exact(&mut plen).await?;
        let mut p = vec![0u8; plen[0] as usize];
        s.read_exact(&mut p).await?;
        if u != user.as_bytes() || p != pass.as_bytes() {
            log.lock().unwrap().push("DENIED SOCKS5 credentials".into());
            return s.write_all(&[1, 1]).await;
        }
        s.write_all(&[1, 0]).await?;
    }
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            format!("[{}]", std::net::Ipv6Addr::from(a))
        }
        _ => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            s.read_exact(&mut name).await?;
            String::from_utf8_lossy(&name).into_owned()
        }
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port).await?;
    let target = format!("{host}:{}", u16::from_be_bytes(port));
    log.lock().unwrap().push(format!("SOCKS5 {target}"));
    let reply_ok = [5, 0, 0, 1, 127, 0, 0, 1, 0, 0];
    match TcpStream::connect(upstream_addr(&target).as_str()).await {
        Ok(mut upstream) => {
            s.write_all(&reply_ok).await?;
            tokio::io::copy_bidirectional(&mut s, &mut upstream).await.map(drop)
        }
        Err(_) => s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await,
    }
}

/// A TLS acceptor with a fresh self-signed certificate for `localhost` and
/// 127.0.0.1, and that certificate in PEM (to trust it as an extra CA).
pub fn tls_acceptor() -> (tokio_rustls::TlsAcceptor, Vec<u8>) {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let config =
        rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![ck.cert.der().clone()], key)
            .unwrap();
    let b64 = base64(ck.cert.der());
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
    let pem = format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"));
    (tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config)), pem.into_bytes())
}

/// Custom proxy settings for `url` (bypass empty).
pub fn custom(url: &str, auth: Option<(&str, &str)>) -> merpati_core::settings::ProxySettings {
    let (username, password) = auth.map(|(u, p)| (u.to_string(), p.to_string())).unwrap_or_default();
    merpati_core::settings::ProxySettings {
        custom_enabled: true,
        custom_url: url.to_string(),
        username,
        password,
        ..Default::default()
    }
}
