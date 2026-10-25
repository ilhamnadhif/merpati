//! A loopback HTTP `CONNECT` endpoint in front of the configured proxy.
//!
//! rumqttc dials brokers itself and only knows HTTP `CONNECT` proxies, so a
//! session that goes through a proxy (HTTP, HTTPS, SOCKS5 or SOCKS4, chosen
//! by `merpati_http::proxy` like for HTTP requests) points rumqttc at this
//! bridge instead. The bridge listens on 127.0.0.1, accepts only `CONNECT`
//! requests carrying its random token as Basic credentials, and opens the
//! tunnel to the broker through the real proxy. TLS and WebSocket
//! handshakes still run in rumqttc, end to end with the broker: the bridge
//! only relays bytes. It lives as long as the session.

use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use merpati_http::proxy::ProxyServer;
use merpati_http::tls::TlsSetup;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

/// How long rumqttc may take to send its `CONNECT` request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest `CONNECT` request accepted.
const MAX_REQUEST: usize = 8 * 1024;
const USERNAME: &str = "merpati";

pub(crate) struct Bridge {
    port: u16,
    token: String,
    proxy: ProxyServer,
    /// The last tunnel failure, reported instead of rumqttc's generic
    /// "proxy answered 502".
    last_error: Arc<Mutex<Option<String>>>,
    task: JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Also ends the relays, which the task owns.
        self.task.abort();
    }
}

/// Where tunnels go and how.
struct Route {
    proxy: ProxyServer,
    host: String,
    port: u16,
    tls: TlsSetup,
    verify: bool,
    /// `Proxy-Authorization` value rumqttc must send.
    expected: String,
    last_error: Arc<Mutex<Option<String>>>,
}

impl Bridge {
    /// Starts a bridge to `host:port` through `proxy`. `tls` and `verify`
    /// apply to TLS with an HTTPS proxy.
    pub(crate) async fn start(
        proxy: ProxyServer,
        host: String,
        port: u16,
        tls: TlsSetup,
        verify: bool,
    ) -> io::Result<Bridge> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let local_port = listener.local_addr()?.port();
        let token = format!("{:032x}", rand::random::<u128>());
        let expected =
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{USERNAME}:{token}")));
        let last_error: Arc<Mutex<Option<String>>> = Arc::default();
        let route =
            Arc::new(Route { proxy: proxy.clone(), host, port, tls, verify, expected, last_error: last_error.clone() });
        let task = tokio::spawn(serve(listener, route));
        Ok(Bridge { port: local_port, token, proxy, last_error, task })
    }

    /// rumqttc's proxy option pointing at the bridge.
    pub(crate) fn rumqttc_proxy(&self) -> rumqttc::Proxy {
        rumqttc::Proxy {
            ty: rumqttc::ProxyType::Http,
            auth: rumqttc::ProxyAuth::Basic { username: USERNAME.into(), password: self.token.clone() },
            addr: Ipv4Addr::LOCALHOST.to_string(),
            port: self.port,
        }
    }

    /// The real proxy, for messages (`scheme://host:port`, no credentials).
    pub(crate) fn proxy(&self) -> &ProxyServer {
        &self.proxy
    }

    /// Why the last tunnel could not be opened, if it could not.
    pub(crate) fn last_error(&self) -> Arc<Mutex<Option<String>>> {
        self.last_error.clone()
    }
}

async fn serve(listener: TcpListener, route: Arc<Route>) {
    let mut relays = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((socket, _)) => {
                    relays.spawn(relay(socket, route.clone()));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            },
            Some(_) = relays.join_next(), if !relays.is_empty() => {}
        }
    }
}

async fn relay(mut client: TcpStream, route: Arc<Route>) {
    let _ = client.set_nodelay(true);
    let authorized = match tokio::time::timeout(REQUEST_TIMEOUT, read_head(&mut client)).await {
        Ok(Some(head)) => head_authorized(&head, &route.expected),
        _ => false,
    };
    if !authorized {
        let _ = client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n").await;
        return;
    }
    let mut upstream = match route.proxy.tunnel(&route.host, route.port, &route.tls, route.verify).await {
        Ok(s) => s,
        Err(e) => {
            *route.last_error.lock() = Some(e.message);
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n").await;
            return;
        }
    };
    if client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

/// The request head (up to the blank line), read byte by byte so that
/// nothing of the tunnelled stream is consumed.
async fn read_head(client: &mut TcpStream) -> Option<Vec<u8>> {
    let mut head = Vec::with_capacity(256);
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_REQUEST {
            return None;
        }
        let mut b = [0u8; 1];
        match client.read(&mut b).await {
            Ok(1) => head.push(b[0]),
            _ => return None,
        }
    }
    Some(head)
}

/// A `CONNECT` request with the bridge's credentials. The target is not
/// checked: the bridge only ever connects to its own broker.
fn head_authorized(head: &[u8], expected: &str) -> bool {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    if !lines.next().is_some_and(|l| l.starts_with("CONNECT ")) {
        return false;
    }
    lines.filter_map(|l| l.split_once(':')).any(|(name, value)| {
        name.trim().eq_ignore_ascii_case("proxy-authorization") && constant_time_eq(value.trim(), expected)
    })
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_need_the_token() {
        let expected = "Basic bWVycGF0aTp0b2tlbg==";
        let ok = b"CONNECT broker:1883 HTTP/1.1\r\nHost: broker:1883\r\nproxy-authorization: Basic bWVycGF0aTp0b2tlbg==\r\n\r\n";
        assert!(head_authorized(ok, expected));
        let wrong = b"CONNECT broker:1883 HTTP/1.1\r\nProxy-Authorization: Basic bWVycGF0aTp0b2tlbh==\r\n\r\n";
        assert!(!head_authorized(wrong, expected));
        let missing = b"CONNECT broker:1883 HTTP/1.1\r\nHost: broker:1883\r\n\r\n";
        assert!(!head_authorized(missing, expected));
        let get = b"GET / HTTP/1.1\r\nProxy-Authorization: Basic bWVycGF0aTp0b2tlbg==\r\n\r\n";
        assert!(!head_authorized(get, expected));
    }
}
