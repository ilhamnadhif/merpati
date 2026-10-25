//! Connections to upstream servers: TCP (optionally through an upstream
//! HTTP proxy), TLS, HTTP/1.1 or HTTP/2 handshakes, and a small pool.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper::client::conn::{TrySendError, http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use parking_lot::Mutex;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::UpstreamProxy;
use crate::exchange::ProxyBody;

const MAX_IDLE_PER_HOST: usize = 8;
const MAX_IDLE_TOTAL: usize = 256;

/// Where requests go. Also the pool key.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct Target {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// TLS server name when it differs from `host` (client SNI).
    pub sni: Option<String>,
}

#[derive(Debug)]
pub(crate) struct UpstreamError {
    pub status: StatusCode,
    pub message: String,
}

impl UpstreamError {
    fn bad_gateway(message: impl Into<String>) -> Self {
        UpstreamError { status: StatusCode::BAD_GATEWAY, message: message.into() }
    }
}

/// A TCP or TLS stream to the server.
pub(crate) enum UpStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for UpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            UpStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            UpStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for UpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            UpStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            UpStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            UpStream::Plain(s) => Pin::new(s).poll_flush(cx),
            UpStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            UpStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            UpStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            UpStream::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            UpStream::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            UpStream::Plain(s) => s.is_write_vectored(),
            UpStream::Tls(s) => s.is_write_vectored(),
        }
    }
}

/// How a request's connection was obtained.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ConnInfo {
    pub reused: bool,
    pub connect: Option<Duration>,
    pub tls: Option<Duration>,
}

pub(crate) enum Sender {
    H1(http1::SendRequest<ProxyBody>),
    H2(http2::SendRequest<ProxyBody>),
}

impl Sender {
    pub fn is_h2(&self) -> bool {
        matches!(self, Sender::H2(_))
    }

    fn is_closed(&self) -> bool {
        match self {
            Sender::H1(s) => s.is_closed(),
            Sender::H2(s) => s.is_closed(),
        }
    }

    fn is_ready(&self) -> bool {
        match self {
            Sender::H1(s) => s.is_ready(),
            Sender::H2(s) => s.is_ready(),
        }
    }

    // hyper's own error type; boxing it would only add an allocation per request.
    #[allow(clippy::result_large_err)]
    pub async fn send(
        &mut self,
        req: Request<ProxyBody>,
    ) -> Result<Response<Incoming>, TrySendError<Request<ProxyBody>>> {
        match self {
            Sender::H1(s) => s.try_send_request(req).await,
            Sender::H2(s) => s.try_send_request(req).await,
        }
    }
}

/// Idle upstream connections, keyed by target.
#[derive(Default)]
pub(crate) struct Pool {
    idle: Mutex<HashMap<Target, Vec<Sender>>>,
}

impl Pool {
    pub fn checkout(&self, target: &Target) -> Option<Sender> {
        let mut map = self.idle.lock();
        let list = map.get_mut(target)?;
        list.retain(|s| !s.is_closed());
        if let Some(Sender::H2(s)) = list.iter().find(|s| s.is_h2()) {
            return Some(Sender::H2(s.clone()));
        }
        let pos = list.iter().position(|s| s.is_ready())?;
        Some(list.swap_remove(pos))
    }

    /// Returns a connection for reuse. HTTP/1.1 connections are checked in
    /// while their response body is still streaming; they become ready once
    /// it is done.
    pub fn checkin(&self, target: &Target, sender: Sender) {
        if sender.is_closed() {
            return;
        }
        let mut map = self.idle.lock();
        let total: usize = map.values().map(Vec::len).sum();
        if total >= MAX_IDLE_TOTAL {
            map.retain(|_, list| {
                list.retain(|s| !s.is_closed());
                !list.is_empty()
            });
            if map.values().map(Vec::len).sum::<usize>() >= MAX_IDLE_TOTAL {
                return;
            }
        }
        let list = map.entry(target.clone()).or_default();
        match sender {
            Sender::H2(_) if list.iter().any(Sender::is_h2) => {}
            Sender::H1(_) if list.len() >= MAX_IDLE_PER_HOST => {}
            s => list.push(s),
        }
    }

    pub fn clear(&self) {
        self.idle.lock().clear();
    }
}

pub(crate) struct Connector {
    tls_h2: Arc<rustls::ClientConfig>,
    tls_h1: Arc<rustls::ClientConfig>,
    upstream_proxy: Option<UpstreamProxy>,
    connect_timeout: Duration,
    self_port: u16,
    token: CancellationToken,
    tracker: TaskTracker,
}

impl Connector {
    pub fn new(
        tls: rustls::ClientConfig,
        upstream_proxy: Option<UpstreamProxy>,
        connect_timeout: Duration,
        self_port: u16,
        token: CancellationToken,
        tracker: TaskTracker,
    ) -> Self {
        let mut h2 = tls.clone();
        h2.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let mut h1 = tls;
        h1.alpn_protocols = vec![b"http/1.1".to_vec()];
        Connector {
            tls_h2: Arc::new(h2),
            tls_h1: Arc::new(h1),
            upstream_proxy,
            connect_timeout,
            self_port,
            token,
            tracker,
        }
    }

    /// A TCP connection to `host:port`, through the upstream proxy if one is
    /// configured.
    pub async fn tcp(&self, host: &str, port: u16) -> Result<TcpStream, UpstreamError> {
        let (dial_host, dial_port) = match &self.upstream_proxy {
            Some(p) => (p.host.as_str(), p.port),
            None => (host, port),
        };
        let dial = happy_eyeballs(dial_host.trim_start_matches('[').trim_end_matches(']'), dial_port);
        let stream = match tokio::time::timeout(self.connect_timeout, dial).await {
            Err(_) => {
                return Err(UpstreamError {
                    status: StatusCode::GATEWAY_TIMEOUT,
                    message: format!("timed out connecting to {}", hostport(dial_host, dial_port)),
                });
            }
            Ok(Err(e)) => {
                return Err(UpstreamError::bad_gateway(format!(
                    "could not connect to {}: {e}",
                    hostport(dial_host, dial_port)
                )));
            }
            Ok(Ok(s)) => s,
        };
        let _ = stream.set_nodelay(true);
        if let (Ok(peer), Ok(local)) = (stream.peer_addr(), stream.local_addr())
            && peer.port() == self.self_port
            && (peer.ip().is_loopback() || peer.ip() == local.ip())
        {
            return Err(UpstreamError {
                status: StatusCode::LOOP_DETECTED,
                message: format!("{} is this proxy; refusing to forward to itself", hostport(dial_host, dial_port)),
            });
        }
        match &self.upstream_proxy {
            Some(p) => self.proxy_connect(stream, p, host, port).await,
            None => Ok(stream),
        }
    }

    async fn proxy_connect(
        &self,
        mut stream: TcpStream,
        proxy: &UpstreamProxy,
        host: &str,
        port: u16,
    ) -> Result<TcpStream, UpstreamError> {
        let authority = hostport(host, port);
        let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
        if let Some(user) = &proxy.username {
            use base64::Engine as _;
            let creds = format!("{user}:{}", proxy.password.as_deref().unwrap_or(""));
            let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
            req.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
        }
        req.push_str("\r\n");
        let proxy_name = hostport(&proxy.host, proxy.port);
        let fail = |e: io::Error| UpstreamError::bad_gateway(format!("upstream proxy {proxy_name}: {e}"));
        let handshake = async {
            stream.write_all(req.as_bytes()).await.map_err(fail)?;
            // Read the response head byte by byte so no tunnel data is consumed.
            let mut head = Vec::with_capacity(256);
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if head.len() > 16 * 1024 {
                    return Err(UpstreamError::bad_gateway(format!(
                        "upstream proxy {proxy_name} sent an oversized response"
                    )));
                }
                let n = stream.read(&mut byte).await.map_err(fail)?;
                if n == 0 {
                    return Err(UpstreamError::bad_gateway(format!(
                        "upstream proxy {proxy_name} closed the connection"
                    )));
                }
                head.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&head);
            let status_line = text.lines().next().unwrap_or_default().to_owned();
            let code = status_line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).unwrap_or(0);
            if (200..300).contains(&code) {
                Ok(())
            } else if code == 407 {
                Err(UpstreamError::bad_gateway(format!(
                    "upstream proxy {proxy_name} requires authentication ({status_line})"
                )))
            } else {
                Err(UpstreamError::bad_gateway(format!(
                    "upstream proxy {proxy_name} refused CONNECT {authority}: {status_line}"
                )))
            }
        };
        match tokio::time::timeout(self.connect_timeout, handshake).await {
            Ok(Ok(())) => Ok(stream),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(UpstreamError {
                status: StatusCode::GATEWAY_TIMEOUT,
                message: format!("upstream proxy {proxy_name} did not answer CONNECT in time"),
            }),
        }
    }

    /// Connects (TCP + TLS) and performs the HTTP handshake.
    pub async fn open(
        &self,
        target: &Target,
        allow_h2: bool,
        upgrades: bool,
    ) -> Result<(Sender, ConnInfo), UpstreamError> {
        let started = Instant::now();
        let tcp = self.tcp(&target.host, target.port).await?;
        let (io, h2, tls) = if target.https {
            let tls_started = Instant::now();
            let name = target.sni.as_deref().unwrap_or(&target.host);
            let server_name = ServerName::try_from(name.to_owned())
                .map_err(|e| UpstreamError::bad_gateway(format!("invalid TLS server name {name:?}: {e}")))?;
            let config = if allow_h2 && !upgrades { self.tls_h2.clone() } else { self.tls_h1.clone() };
            let connect = tokio_rustls::TlsConnector::from(config).connect(server_name, tcp);
            let stream = match tokio::time::timeout(self.connect_timeout, connect).await {
                Err(_) => {
                    return Err(UpstreamError {
                        status: StatusCode::GATEWAY_TIMEOUT,
                        message: format!("TLS handshake with {} timed out", hostport(&target.host, target.port)),
                    });
                }
                Ok(Err(e)) => return Err(UpstreamError::bad_gateway(tls_error_message(target, &e))),
                Ok(Ok(s)) => s,
            };
            let h2 = stream.get_ref().1.alpn_protocol() == Some(b"h2");
            (UpStream::Tls(Box::new(stream)), h2, Some(tls_started.elapsed()))
        } else {
            (UpStream::Plain(tcp), false, None)
        };
        let handshake_err =
            |e: hyper::Error| UpstreamError::bad_gateway(format!("HTTP handshake with {} failed: {e}", target.host));
        let io = TokioIo::new(io);
        let token = self.token.clone();
        let sender = if h2 {
            let mut builder = http2::Builder::new(TokioExecutor::new());
            builder.timer(TokioTimer::new()).adaptive_window(true);
            let (sender, conn) = builder.handshake(io).await.map_err(handshake_err)?;
            self.tracker.spawn(async move {
                tokio::select! {
                    _ = conn => {}
                    _ = token.cancelled() => {}
                }
            });
            Sender::H2(sender)
        } else {
            let (sender, conn) =
                http1::Builder::new().preserve_header_case(true).handshake(io).await.map_err(handshake_err)?;
            self.tracker.spawn(async move {
                tokio::select! {
                    _ = conn.with_upgrades() => {}
                    _ = token.cancelled() => {}
                }
            });
            Sender::H1(sender)
        };
        Ok((sender, ConnInfo { reused: false, connect: Some(started.elapsed()), tls }))
    }
}

/// Delay before racing the next address (RFC 8305 "Connection Attempt Delay").
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Resolves `host` and connects to its addresses with staggered parallel
/// attempts, alternating IPv6 and IPv4, so one unreachable address family
/// does not stall the connection.
async fn happy_eyeballs(host: &str, port: u16) -> io::Result<TcpStream> {
    use futures_util::StreamExt as _;
    use futures_util::stream::FuturesUnordered;

    let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    let first_v6 = resolved.first().is_some_and(|a| a.is_ipv6());
    let (mut primary, mut secondary): (Vec<_>, Vec<_>) = resolved.into_iter().partition(|a| a.is_ipv6() == first_v6);
    primary.reverse();
    secondary.reverse();
    let mut ordered = Vec::with_capacity(primary.len() + secondary.len());
    while !primary.is_empty() || !secondary.is_empty() {
        ordered.extend(primary.pop());
        ordered.extend(secondary.pop());
    }
    let mut addrs = ordered.into_iter();
    let mut pending = FuturesUnordered::new();
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no addresses"));
    if let Some(a) = addrs.next() {
        pending.push(TcpStream::connect(a));
    }
    while !pending.is_empty() {
        let more = addrs.len() > 0;
        tokio::select! {
            res = pending.next() => match res {
                Some(Ok(stream)) => return Ok(stream),
                Some(Err(e)) => {
                    last_err = e;
                    if let Some(a) = addrs.next() {
                        pending.push(TcpStream::connect(a));
                    }
                }
                None => {}
            },
            _ = tokio::time::sleep(ATTEMPT_DELAY), if more => {
                if let Some(a) = addrs.next() {
                    pending.push(TcpStream::connect(a));
                }
            }
        }
    }
    Err(last_err)
}

fn tls_error_message(target: &Target, e: &io::Error) -> String {
    let text = e.to_string();
    let mut msg = format!("TLS handshake with {} failed: {text}", hostport(&target.host, target.port));
    if text.contains("certificate") || text.contains("Certificate") {
        msg.push_str(
            " (the server certificate is not trusted; for self-signed development servers turn off upstream \
             TLS verification for this capture session)",
        );
    }
    msg
}

/// `host:port`, bracketing IPv6 literals.
pub(crate) fn hostport(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn happy_eyeballs_local_and_unreachable() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        // "localhost" usually resolves to ::1 first; nothing listens there, 127.0.0.1 must win.
        let s = happy_eyeballs("localhost", port).await.unwrap();
        assert_eq!(s.peer_addr().unwrap().port(), port);
        drop(l);
        assert!(happy_eyeballs("127.0.0.1", port).await.is_err());
    }

    #[tokio::test]
    #[ignore = "needs internet access"]
    async fn happy_eyeballs_real_host() {
        let host = std::env::var("HE_HOST").unwrap_or_else(|_| "neverssl.com".into());
        let addrs: Vec<_> = tokio::net::lookup_host((host.as_str(), 80)).await.unwrap().collect();
        eprintln!("resolved {addrs:?}");
        let started = Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(20), happy_eyeballs(&host, 80)).await;
        eprintln!("{:?} after {:?}", r.map(|r| r.map(|s| s.peer_addr().unwrap())), started.elapsed());
    }
}
