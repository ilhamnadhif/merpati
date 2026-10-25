//! The proxy: accept loop, request routing, forwarding, CONNECT handling,
//! TLS interception, tunnels and upgrade relays.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use http::header::{CONNECTION, CONTENT_TYPE, COOKIE, HOST, TE, UPGRADE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version};
use hyper::body::{Body as _, Incoming};
use hyper::service::service_fn;
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use parking_lot::{Mutex, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::ca::{CertificateAuthority, normalize_host};
use crate::config::{CaptureFilter, ProxyConfig, host_matches};
use crate::entry::{CaptureEntry, EntryKind, EntryState, StreamStats, Timings};
use crate::error::ProxyError;
use crate::exchange::{Exchange, ProxyBody, Side, TeeBody, empty, full, headers_vec};
use crate::io::{OptionsSniffer, OptionsTargets, Rewind, relay};
use crate::store::{ProxyEvent, Store};
use crate::upstream::{ConnInfo, Connector, Pool, Target, UpstreamError, hostport};
use crate::ws::{WsLog, WsSniffer};

/// How long an intercepted CONNECT waits for the client's first bytes
/// before treating the tunnel as a server-speaks-first protocol.
const FIRST_BYTES_TIMEOUT: Duration = Duration::from_secs(3);
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(15);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(60);
const TUNNEL_TICK: Duration = Duration::from_secs(1);
const WS_TICK: Duration = Duration::from_millis(300);
const SERVER_CONFIG_CACHE_LIMIT: usize = 4096;
/// Host name that always reaches the proxy's own page (CA download).
pub const INFO_HOST: &str = "merpati.proxy";

/// Counters for the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyStats {
    pub active_connections: usize,
    pub total_connections: u64,
    pub entries: usize,
    /// Body and WebSocket message bytes held by the stored entries.
    pub stored_body_bytes: usize,
    /// Body bytes buffered by exchanges still streaming. Together with
    /// `stored_body_bytes` it stays within [`ProxyConfig::max_total_body_bytes`].
    pub in_flight_body_bytes: usize,
}

pub(crate) struct Shared {
    pub cfg: ProxyConfig,
    filter: RwLock<CaptureFilter>,
    passthrough: RwLock<Vec<String>>,
    recording: AtomicBool,
    pub store: Arc<Store>,
    connector: Connector,
    pool: Pool,
    token: CancellationToken,
    tracker: TaskTracker,
    pub local_addr: SocketAddr,
    tls_failed: Mutex<HashSet<String>>,
    server_configs: Mutex<HashMap<(String, bool), Arc<rustls::ServerConfig>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    active: AtomicUsize,
    total_connections: AtomicU64,
}

/// A running capture proxy. Dropping it stops the proxy too (without
/// waiting); prefer [`stop`](Self::stop).
pub struct CaptureProxy {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for CaptureProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureProxy").field("addr", &self.shared.local_addr).finish_non_exhaustive()
    }
}

impl CaptureProxy {
    /// Binds the listener and starts serving. Must be called inside a Tokio
    /// runtime.
    pub async fn start(config: ProxyConfig) -> Result<CaptureProxy, ProxyError> {
        if !config.allow_lan && !config.bind.ip().is_loopback() {
            return Err(ProxyError::NotLoopback(config.bind));
        }
        merpati_http::tls::install_provider();
        let verify = config.verify_upstream_tls;
        let extra = config.extra_upstream_roots_pem.clone();
        // Loading the system roots is blocking (keychain on macOS).
        let tls = tokio::task::spawn_blocking(move || {
            merpati_http::tls::client_config(verify, extra.as_deref().map(str::as_bytes))
        })
        .await
        .map_err(|e| ProxyError::Tls(e.to_string()))?
        .map_err(ProxyError::Tls)?;
        let listener =
            TcpListener::bind(config.bind).await.map_err(|source| ProxyError::Bind { addr: config.bind, source })?;
        let local_addr = listener.local_addr().map_err(|source| ProxyError::Bind { addr: config.bind, source })?;
        let token = CancellationToken::new();
        let tracker = TaskTracker::new();
        let connector = Connector::new(
            tls,
            config.upstream_proxy.clone(),
            config.connect_timeout,
            local_addr.port(),
            token.clone(),
            tracker.clone(),
        );
        let store = Arc::new(Store::new(config.capacity, config.max_total_body_bytes, config.on_event.clone()));
        let shared = Arc::new(Shared {
            filter: RwLock::new(config.filter.clone()),
            passthrough: RwLock::new(config.passthrough_hosts.clone()),
            recording: AtomicBool::new(true),
            store,
            connector,
            pool: Pool::default(),
            token,
            tracker: tracker.clone(),
            local_addr,
            tls_failed: Mutex::new(HashSet::new()),
            server_configs: Mutex::new(HashMap::new()),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            active: AtomicUsize::new(0),
            total_connections: AtomicU64::new(0),
            cfg: config,
        });
        tracker.spawn(accept_loop(shared.clone(), listener));
        tracing::info!(addr = %local_addr, "capture proxy listening");
        Ok(CaptureProxy { shared })
    }

    pub fn addr(&self) -> SocketAddr {
        self.shared.local_addr
    }

    /// `http://127.0.0.1:PORT`, the value to configure as HTTP and HTTPS proxy.
    pub fn url(&self) -> String {
        let addr = self.shared.local_addr;
        let ip = if addr.ip().is_unspecified() { "127.0.0.1".to_owned() } else { addr.ip().to_string() };
        format!("http://{}", hostport(&ip, addr.port()))
    }

    /// All stored entries, oldest first.
    pub fn entries(&self) -> Vec<Arc<CaptureEntry>> {
        self.shared.store.list()
    }

    pub fn entry(&self, id: u64) -> Option<Arc<CaptureEntry>> {
        self.shared.store.get(id)
    }

    pub fn clear(&self) {
        self.shared.store.clear();
    }

    /// Live events. Slow receivers lose old events (`RecvError::Lagged`);
    /// re-read [`entries`](Self::entries) then.
    pub fn subscribe(&self) -> broadcast::Receiver<ProxyEvent> {
        self.shared.store.subscribe()
    }

    pub fn filter(&self) -> CaptureFilter {
        self.shared.filter.read().clone()
    }

    /// Applies to new requests and CONNECTs.
    pub fn set_filter(&self, filter: CaptureFilter) {
        *self.shared.filter.write() = filter;
    }

    pub fn passthrough_hosts(&self) -> Vec<String> {
        self.shared.passthrough.read().clone()
    }

    /// Applies to new CONNECTs.
    pub fn set_passthrough_hosts(&self, hosts: Vec<String>) {
        *self.shared.passthrough.write() = hosts;
        self.shared.tls_failed.lock().clear();
    }

    /// Pause/resume recording. Traffic keeps flowing while paused (HTTPS is
    /// tunnelled instead of decrypted).
    pub fn set_recording(&self, on: bool) {
        self.shared.recording.store(on, Ordering::Relaxed);
    }

    pub fn is_recording(&self) -> bool {
        self.shared.recording.load(Ordering::Relaxed)
    }

    /// Whether HTTPS is decrypted (interception on and a CA available).
    pub fn is_intercepting(&self) -> bool {
        self.shared.cfg.intercept_tls && self.shared.cfg.ca.is_some()
    }

    pub fn ca(&self) -> Option<&Arc<CertificateAuthority>> {
        self.shared.cfg.ca.as_ref()
    }

    pub fn config(&self) -> &ProxyConfig {
        &self.shared.cfg
    }

    /// Hosts whose clients rejected the Merpati certificate this session.
    pub fn tls_failed_hosts(&self) -> Vec<String> {
        let mut v: Vec<String> = self.shared.tls_failed.lock().iter().cloned().collect();
        v.sort();
        v
    }

    pub fn stats(&self) -> ProxyStats {
        ProxyStats {
            active_connections: self.shared.active.load(Ordering::Relaxed),
            total_connections: self.shared.total_connections.load(Ordering::Relaxed),
            entries: self.shared.store.len(),
            stored_body_bytes: self.shared.store.stored_bytes(),
            in_flight_body_bytes: self.shared.store.in_flight_bytes(),
        }
    }

    /// Stops listening, closes every connection and waits (up to 5 s) for
    /// the proxy's tasks to end. In-flight entries are marked failed.
    pub async fn stop(self) {
        self.shared.token.cancel();
        self.shared.tracker.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.shared.tracker.wait()).await;
        self.shared.pool.clear();
        self.shared.store.emit(ProxyEvent::Stopped);
    }
}

impl Drop for CaptureProxy {
    fn drop(&mut self) {
        self.shared.token.cancel();
        self.shared.tracker.close();
    }
}

struct ActiveGuard(Arc<Shared>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn accept_loop(shared: Arc<Shared>, listener: TcpListener) {
    loop {
        let accepted = tokio::select! {
            _ = shared.token.cancelled() => break,
            r = listener.accept() => r,
        };
        match accepted {
            Ok((tcp, peer)) => {
                let _ = tcp.set_nodelay(true);
                shared.total_connections.fetch_add(1, Ordering::Relaxed);
                shared.active.fetch_add(1, Ordering::Relaxed);
                let s = shared.clone();
                shared.tracker.spawn(async move {
                    let guard = ActiveGuard(s.clone());
                    let targets = Arc::new(OptionsTargets::default());
                    let io = TokioIo::new(OptionsSniffer::new(tcp, targets.clone()));
                    serve_conn(s, Ctx { peer, origin: None, targets: Some(targets) }, io, false).await;
                    drop(guard);
                });
            }
            Err(e) => {
                tracing::debug!("proxy accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Connection context: `origin` is set for connections inside a CONNECT,
/// `targets` for connections from clients to the proxy itself.
#[derive(Clone)]
struct Ctx {
    peer: SocketAddr,
    origin: Option<Arc<Origin>>,
    targets: Option<Arc<OptionsTargets>>,
}

struct Origin {
    https: bool,
    host: String,
    port: u16,
    sni: Option<String>,
}

impl Origin {
    fn default_authority(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        if self.port != default {
            hostport(&self.host, self.port)
        } else if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }
}

/// Serves HTTP on `io`. Boxed because it is reached recursively (CONNECT
/// handling serves the decrypted stream with the same machinery).
fn serve_conn<I>(shared: Arc<Shared>, ctx: Ctx, io: I, h2: bool) -> Pin<Box<dyn Future<Output = ()> + Send>>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    Box::pin(async move {
        let token = shared.token.clone();
        let svc = service_fn(move |req| {
            let shared = shared.clone();
            let ctx = ctx.clone();
            async move { Ok::<_, Infallible>(route(shared, ctx, req).await) }
        });
        if h2 {
            let conn = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .timer(TokioTimer::new())
                .adaptive_window(true)
                .serve_connection(io, svc);
            tokio::select! {
                _ = conn => {}
                _ = token.cancelled() => {}
            }
        } else {
            let conn = hyper::server::conn::http1::Builder::new()
                .preserve_header_case(true)
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(io, svc)
                .with_upgrades();
            tokio::select! {
                _ = conn => {}
                _ = token.cancelled() => {}
            }
        }
    })
}

async fn route(shared: Arc<Shared>, ctx: Ctx, req: Request<Incoming>) -> Response<ProxyBody> {
    match &ctx.origin {
        Some(origin) => {
            if req.method() == Method::CONNECT {
                return error_response(StatusCode::METHOD_NOT_ALLOWED, "nested CONNECT is not supported");
            }
            forward(shared, ctx.peer, origin.clone(), true, false, req).await
        }
        None => {
            if req.method() == Method::CONNECT {
                let resp = handle_connect(shared, ctx.peer, req).await;
                if resp.status() == StatusCode::OK
                    && let Some(t) = &ctx.targets
                {
                    // A tunnel now: no more requests follow on this connection.
                    t.stop();
                }
                return resp;
            }
            let Some(authority) = req.uri().authority().cloned() else {
                // Origin-form: someone opened the proxy address directly.
                return crate::info::respond(&shared, &req);
            };
            // `OPTIONS http://host` (empty path) asks about the server: `OPTIONS *`.
            let asterisk = req.method() == Method::OPTIONS
                && req.uri().scheme().is_some()
                && ctx.targets.as_ref().is_some_and(|t| t.asterisk_form(authority.as_str()));
            let https = req.uri().scheme() == Some(&http::uri::Scheme::HTTPS);
            let host = normalize_host(authority.host());
            let port = authority.port_u16().unwrap_or(if https { 443 } else { 80 });
            if host == INFO_HOST || is_self(&shared, &host, port) {
                return crate::info::respond(&shared, &req);
            }
            let origin = Arc::new(Origin { https, host, port, sni: None });
            forward(shared, ctx.peer, origin, false, asterisk, req).await
        }
    }
}

fn is_self(shared: &Shared, host: &str, port: u16) -> bool {
    if port != shared.local_addr.port() {
        return false;
    }
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified() || ip == shared.local_addr.ip())
}

/// Forwards one request upstream and streams the response back, capturing
/// both sides. `asterisk` sends the request target `*` (server-wide `OPTIONS`).
async fn forward(
    shared: Arc<Shared>,
    peer: SocketAddr,
    origin: Arc<Origin>,
    intercepted: bool,
    asterisk: bool,
    mut req: Request<Incoming>,
) -> Response<ProxyBody> {
    let started = Instant::now();
    let client_version = req.version();
    let upgrade = upgrade_protocol(req.headers());
    let client_upgrade = upgrade.as_ref().map(|_| hyper::upgrade::on(&mut req));
    let (parts, body) = req.into_parts();
    let scheme = if origin.https { "https" } else { "http" };
    let header_host = parts.headers.get(HOST).and_then(|v| v.to_str().ok()).map(str::to_owned);
    // The host information only: `user:pass@` must not end up in `Host` or the URL.
    let uri_authority =
        parts.uri.authority().map(|a| a.as_str().rsplit_once('@').map_or(a.as_str(), |(_, h)| h).to_owned());
    let authority = if intercepted { header_host.or(uri_authority) } else { uri_authority.or(header_host) }
        .unwrap_or_else(|| origin.default_authority());
    let path = if asterisk { "*" } else { parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/") }.to_owned();
    let url = if path == "*" { format!("{scheme}://{authority}") } else { format!("{scheme}://{authority}{path}") };

    let mut headers = parts.headers.clone();
    clean_hop_by_hop(&mut headers, upgrade.is_some());
    headers.remove(http::header::EXPECT);
    merge_cookies(&mut headers);
    if (!intercepted || !headers.contains_key(HOST))
        && let Ok(v) = HeaderValue::from_str(&authority)
    {
        headers.insert(HOST, v);
    }

    let exchange = shared.begin_http(
        peer,
        &origin,
        intercepted,
        &parts.method,
        &url,
        client_version,
        &headers,
        upgrade.as_deref(),
    );
    let replayable = body.is_end_stream();
    let mut body_slot = Some(TeeBody::boxed(body, Side::Request, exchange.as_ref()));
    let target = Target { https: origin.https, host: origin.host.clone(), port: origin.port, sni: origin.sni.clone() };

    let mut first_try = true;
    let outcome = loop {
        let opened = if upgrade.is_some() {
            shared.connector.open(&target, false, true).await
        } else if let Some(sender) = first_try.then(|| shared.pool.checkout(&target)).flatten() {
            Ok((sender, ConnInfo { reused: true, ..ConnInfo::default() }))
        } else {
            shared.connector.open(&target, shared.cfg.upstream_http2, false).await
        };
        let (mut sender, info) = match opened {
            Ok(x) => x,
            Err(e) => break Err(e),
        };
        let body = body_slot.take().unwrap_or_else(empty);
        let up_req =
            match build_upstream_request(&parts.method, &headers, scheme, &authority, &path, sender.is_h2(), body) {
                Ok(r) => r,
                Err(message) => break Err(UpstreamError { status: StatusCode::BAD_REQUEST, message }),
            };
        match sender.send(up_req).await {
            Ok(resp) => break Ok((resp, sender, info)),
            Err(mut e) => {
                if info.reused && first_try {
                    // A pooled connection the server had already closed.
                    first_try = false;
                    if let Some(r) = e.take_message() {
                        body_slot = Some(r.into_body());
                        continue;
                    }
                    if replayable && e.error().is_incomplete_message() {
                        continue;
                    }
                }
                break Err(UpstreamError {
                    status: StatusCode::BAD_GATEWAY,
                    message: format!("{}: {}", hostport(&target.host, target.port), error_chain(e.error())),
                });
            }
        }
    };

    let (mut resp, sender, info) = match outcome {
        Ok(x) => x,
        Err(e) => {
            if let Some(ex) = &exchange {
                ex.fail(e.message.clone());
            }
            tracing::debug!(host = %target.host, port = target.port, status = %e.status, "upstream request failed");
            return error_response(e.status, &e.message);
        }
    };
    let upstream_version = version_str(resp.version());
    let timings = Timings {
        connect: info.connect,
        tls_handshake: info.tls,
        ttfb: Some(started.elapsed()),
        total: None,
        reused_connection: info.reused,
    };

    if let Some(client_upgrade) = client_upgrade
        && resp.status() == StatusCode::SWITCHING_PROTOCOLS
    {
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        let (mut rparts, _) = resp.into_parts();
        clean_hop_by_hop(&mut rparts.headers, true);
        if let Some(ex) = &exchange {
            ex.response_head(101, &rparts.headers, upstream_version, timings);
            ex.update(|e| e.stream = Some(StreamStats::default()));
        }
        let websocket = upgrade.as_deref().is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
        let s = shared.clone();
        shared.tracker.spawn(relay_upgrade(s, client_upgrade, upstream_upgrade, exchange, websocket));
        rparts.version = Version::HTTP_11;
        return Response::from_parts(rparts, empty());
    }

    if upgrade.is_none() {
        shared.pool.checkin(&target, sender);
    }
    let (mut rparts, rbody) = resp.into_parts();
    clean_hop_by_hop(&mut rparts.headers, false);
    if let Some(ex) = &exchange {
        ex.response_head(rparts.status.as_u16(), &rparts.headers, upstream_version, timings);
    }
    rparts.version = match client_version {
        Version::HTTP_10 => Version::HTTP_10,
        Version::HTTP_2 => Version::HTTP_2,
        _ => Version::HTTP_11,
    };
    Response::from_parts(rparts, TeeBody::boxed(rbody, Side::Response, exchange.as_ref()))
}

async fn relay_upgrade(
    shared: Arc<Shared>,
    client: OnUpgrade,
    upstream: OnUpgrade,
    exchange: Option<Arc<Exchange>>,
    websocket: bool,
) {
    let (client, upstream) = tokio::join!(client, upstream);
    let (client, upstream) = match (client, upstream) {
        (Ok(c), Ok(u)) => (c, u),
        (c, u) => {
            if let Some(ex) = &exchange {
                let err = c.err().or(u.err()).map(|e| e.to_string()).unwrap_or_default();
                ex.fail(format!("upgrade failed: {err}"));
            }
            return;
        }
    };
    let log = (websocket && shared.cfg.capture_websocket_messages && exchange.is_some())
        .then(|| Arc::new(Mutex::new(WsLog::new(shared.cfg.max_body_bytes))));
    let sniffers = log.as_ref().map(|l| (WsSniffer::new(true, l.clone()), WsSniffer::new(false, l.clone())));
    let mut seen_changes = 0u64;
    let mut publish = |stats: &StreamStats, force: bool| {
        let Some(ex) = &exchange else { return };
        let messages = log.as_ref().and_then(|l| {
            let l = l.lock();
            (force || l.changes != seen_changes).then(|| {
                seen_changes = l.changes;
                (l.messages.clone(), l.dropped)
            })
        });
        ex.update(|e| {
            e.stream = Some(stats.clone());
            if let Some((msgs, dropped)) = messages {
                e.ws_messages = msgs;
                e.ws_messages_dropped = dropped;
            }
        });
    };
    let (stats, error) =
        relay(TokioIo::new(client), TokioIo::new(upstream), &shared.token, sniffers, WS_TICK, |s| publish(s, false))
            .await;
    publish(&stats, true);
    if let Some(ex) = &exchange {
        ex.finish(error);
    }
}

async fn handle_connect(shared: Arc<Shared>, peer: SocketAddr, mut req: Request<Incoming>) -> Response<ProxyBody> {
    let Some(authority) = req.uri().authority().cloned() else {
        return error_response(StatusCode::BAD_REQUEST, "CONNECT needs a host:port target");
    };
    let host = normalize_host(authority.host());
    let port = authority.port_u16().unwrap_or(443);
    let capture_host = shared.is_recording() && shared.filter.read().allows_host(&host, port);
    let passthrough = shared.passthrough.read().iter().any(|p| host_matches(p, &host, port));
    let failed_before = shared.tls_failed.lock().contains(&host);
    let can_intercept = shared.cfg.intercept_tls && shared.cfg.ca.is_some();
    let on_upgrade = hyper::upgrade::on(&mut req);

    if can_intercept && capture_host && !passthrough && !failed_before {
        let s = shared.clone();
        let token = shared.token.clone();
        shared.tracker.spawn(async move {
            let Ok(upgraded) = on_upgrade.await else { return };
            tokio::select! {
                _ = intercept_connection(s, peer, host, port, TokioIo::new(upgraded)) => {}
                _ = token.cancelled() => {}
            }
        });
        return connect_established();
    }

    let note = if !can_intercept {
        "TLS interception is off"
    } else if !capture_host {
        "not recorded"
    } else if passthrough {
        "passthrough host (not decrypted)"
    } else {
        "the client rejected the Merpati certificate earlier; tunnelled"
    };
    let exchange = if capture_host {
        shared.begin_tunnel(peer, &host, port, req.version(), Some(req.headers()), note, false)
    } else {
        None
    };
    let started = Instant::now();
    let upstream = match shared.connector.tcp(&host, port).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(ex) = &exchange {
                ex.fail(e.message.clone());
            }
            return error_response(e.status, &e.message);
        }
    };
    if let Some(ex) = &exchange {
        let connect = started.elapsed();
        ex.update(|e| {
            e.state = EntryState::Receiving;
            e.status = Some(200);
            e.timings.connect = Some(connect);
        });
    }
    let s = shared.clone();
    shared.tracker.spawn(async move {
        let client = match on_upgrade.await {
            Ok(c) => c,
            Err(e) => {
                if let Some(ex) = &exchange {
                    ex.fail(format!("the client went away: {e}"));
                }
                return;
            }
        };
        run_tunnel(&s, TokioIo::new(client), upstream, exchange, 0).await;
    });
    connect_established()
}

/// Relays a tunnel and finalizes its entry. `prefix_len` bytes were already
/// forwarded to the server.
async fn run_tunnel<A, B>(shared: &Shared, client: A, upstream: B, exchange: Option<Arc<Exchange>>, prefix_len: u64)
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let ex2 = exchange.clone();
    let (mut stats, error) = relay(client, upstream, &shared.token, None, TUNNEL_TICK, |s| {
        if let Some(ex) = &ex2 {
            let mut s = s.clone();
            s.bytes_up += prefix_len;
            ex.update(|e| e.stream = Some(s));
        }
    })
    .await;
    stats.bytes_up += prefix_len;
    if let Some(ex) = exchange {
        ex.update(|e| e.stream = Some(stats));
        ex.finish(error);
    }
}

/// Decides what an intercepted CONNECT carries: TLS (decrypted), plain
/// HTTP (served directly), or anything else (relayed as a tunnel).
async fn intercept_connection(
    shared: Arc<Shared>,
    peer: SocketAddr,
    host: String,
    port: u16,
    mut io: TokioIo<Upgraded>,
) {
    let mut first = vec![0u8; 16 * 1024];
    let n = match tokio::time::timeout(FIRST_BYTES_TIMEOUT, io.read(&mut first)).await {
        Err(_) => {
            let note = "the client sent nothing first (server-speaks-first protocol); tunnelled";
            return tunnel_after_peek(shared, peer, host, port, io, Vec::new(), note).await;
        }
        Ok(Ok(0)) | Ok(Err(_)) => return,
        Ok(Ok(n)) => n,
    };
    first.truncate(n);
    if first[0] == 0x16 {
        tls_intercept(shared, peer, host, port, io, first).await;
    } else if first.starts_with(b"PRI * HTTP/2.0") {
        let origin = Arc::new(Origin { https: false, host, port, sni: None });
        let ctx = Ctx { peer, origin: Some(origin), targets: None };
        serve_conn(shared, ctx, TokioIo::new(Rewind::new(first, io)), true).await;
    } else if looks_like_http(&first) {
        let origin = Arc::new(Origin { https: false, host, port, sni: None });
        let ctx = Ctx { peer, origin: Some(origin), targets: None };
        serve_conn(shared, ctx, TokioIo::new(Rewind::new(first, io)), false).await;
    } else {
        tunnel_after_peek(shared, peer, host, port, io, first, "neither TLS nor HTTP; tunnelled").await;
    }
}

fn looks_like_http(b: &[u8]) -> bool {
    let token = b.iter().take_while(|c| c.is_ascii_uppercase() || **c == b'-' || **c == b'_').count();
    (3..=24).contains(&token) && (token == b.len() || b[token] == b' ')
}

fn feed_acceptor(acceptor: &mut rustls::server::Acceptor, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        if acceptor.read_tls(&mut data)? == 0 {
            break;
        }
    }
    Ok(())
}

async fn tls_intercept(
    shared: Arc<Shared>,
    peer: SocketAddr,
    host: String,
    port: u16,
    mut io: TokioIo<Upgraded>,
    mut raw: Vec<u8>,
) {
    let mut acceptor = rustls::server::Acceptor::default();
    if feed_acceptor(&mut acceptor, &raw).is_err() {
        return tunnel_after_peek(shared, peer, host, port, io, raw, "unreadable TLS ClientHello; tunnelled").await;
    }
    let accepted = loop {
        match acceptor.accept() {
            Ok(Some(accepted)) => break accepted,
            Ok(None) => {
                if raw.len() > 128 * 1024 {
                    return tunnel_after_peek(
                        shared,
                        peer,
                        host,
                        port,
                        io,
                        raw,
                        "oversized TLS ClientHello; tunnelled",
                    )
                    .await;
                }
                let mut buf = vec![0u8; 16 * 1024];
                let n = match tokio::time::timeout(HANDSHAKE_READ_TIMEOUT, io.read(&mut buf)).await {
                    Ok(Ok(n)) if n > 0 => n,
                    _ => return,
                };
                raw.extend_from_slice(&buf[..n]);
                if feed_acceptor(&mut acceptor, &buf[..n]).is_err() {
                    return tunnel_after_peek(
                        shared,
                        peer,
                        host,
                        port,
                        io,
                        raw,
                        "unreadable TLS ClientHello; tunnelled",
                    )
                    .await;
                }
            }
            Err(_) => {
                return tunnel_after_peek(shared, peer, host, port, io, raw, "unrecognised TLS ClientHello; tunnelled")
                    .await;
            }
        }
    };
    let hello = accepted.client_hello();
    let sni = hello.server_name().map(normalize_host);
    let alpn: Vec<Vec<u8>> = hello.alpn().map(|it| it.map(<[u8]>::to_vec).collect()).unwrap_or_default();
    let speaks_http = alpn.is_empty() || alpn.iter().any(|p| p == b"h2" || p == b"http/1.1" || p == b"http/1.0");
    if !speaks_http {
        let protos: Vec<String> = alpn.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect();
        let note = format!("ALPN {} is not HTTP; tunnelled", protos.join(", "));
        return tunnel_after_peek(shared, peer, host, port, io, raw, &note).await;
    }
    if let Some(name) = &sni
        && name != &host
        && shared.passthrough.read().iter().any(|p| host_matches(p, name, port))
    {
        return tunnel_after_peek(shared, peer, host, port, io, raw, "passthrough host (by SNI); tunnelled").await;
    }
    let h2 = shared.cfg.client_http2 && alpn.iter().any(|p| p == b"h2");
    if !alpn.is_empty() && !h2 && !alpn.iter().any(|p| p == b"http/1.1" || p == b"http/1.0") {
        // An h2-only client while HTTP/2 to clients is off: the handshake would
        // fail with `no_application_protocol`, which is no trust problem. Tunnel
        // this connection without marking the host.
        let note = "the client offers only HTTP/2 (ALPN h2) and HTTP/2 to clients is turned off; tunnelled";
        return tunnel_after_peek(shared, peer, host, port, io, raw, note).await;
    }
    let cert_host = sni.clone().unwrap_or_else(|| host.clone());
    let config = match shared.server_config(&cert_host, h2) {
        Ok(c) => c,
        Err(e) => {
            if let Some(ex) = shared.begin_tunnel(peer, &host, port, Version::HTTP_11, None, "TLS interception", true) {
                ex.fail(format!("could not create a certificate for {cert_host}: {e}"));
            }
            return;
        }
    };
    let tls = match tokio_rustls::StartHandshake::from_parts(accepted, io).into_stream(config).await {
        Ok(tls) => tls,
        Err(e) => {
            if let Some(ex) = shared.begin_tunnel(peer, &host, port, Version::HTTP_11, None, "TLS interception", true) {
                ex.fail(format!(
                    "TLS handshake with the client failed: {e}. The client probably does not trust the Merpati CA \
                     (or it pins certificates for this host)."
                ));
            }
            if shared.cfg.passthrough_on_tls_error {
                shared.tls_failed.lock().insert(host);
            }
            return;
        }
    };
    let negotiated_h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2");
    let upstream_sni = sni.filter(|s| s != &host);
    let origin = Arc::new(Origin { https: true, host, port, sni: upstream_sni });
    serve_conn(shared, Ctx { peer, origin: Some(origin), targets: None }, TokioIo::new(tls), negotiated_h2).await;
}

async fn tunnel_after_peek(
    shared: Arc<Shared>,
    peer: SocketAddr,
    host: String,
    port: u16,
    client: TokioIo<Upgraded>,
    prefix: Vec<u8>,
    note: &str,
) {
    let exchange = shared.begin_tunnel(peer, &host, port, Version::HTTP_11, None, note, false);
    let started = Instant::now();
    let mut upstream = match shared.connector.tcp(&host, port).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(ex) = &exchange {
                ex.fail(e.message);
            }
            return;
        }
    };
    if let Some(ex) = &exchange {
        let connect = started.elapsed();
        ex.update(|e| {
            e.state = EntryState::Receiving;
            e.status = Some(200);
            e.timings.connect = Some(connect);
        });
    }
    if !prefix.is_empty()
        && let Err(e) = upstream.write_all(&prefix).await
    {
        if let Some(ex) = &exchange {
            ex.fail(format!("could not forward the first bytes: {e}"));
        }
        return;
    }
    run_tunnel(&shared, client, upstream, exchange, prefix.len() as u64).await;
}

impl Shared {
    fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_http(
        &self,
        peer: SocketAddr,
        origin: &Origin,
        intercepted: bool,
        method: &Method,
        url: &str,
        version: Version,
        headers: &HeaderMap,
        upgrade: Option<&str>,
    ) -> Option<Arc<Exchange>> {
        if !self.is_recording() {
            return None;
        }
        let filter = self.filter.read().clone();
        if !filter.allows_request(method.as_str(), url, &origin.host, origin.port) {
            return None;
        }
        let kind = if upgrade.is_some_and(|u| u.eq_ignore_ascii_case("websocket")) {
            EntryKind::WebSocket
        } else {
            EntryKind::Http
        };
        let mut e = CaptureEntry::new(self.store.next_id(), peer, kind);
        e.tls_intercepted = intercepted && origin.https;
        e.scheme = if origin.https { "https" } else { "http" }.to_owned();
        e.host = origin.host.clone();
        e.port = origin.port;
        e.method = method.as_str().to_owned();
        e.url = url.to_owned();
        e.http_version = version_str(version).to_owned();
        e.request_headers = headers_vec(headers);
        Some(Exchange::begin(self.store.clone(), e, self.cfg.max_body_bytes, &filter))
    }

    /// Starts a tunnel entry. `always` records it even when tunnel recording
    /// is off (used for interception failures).
    #[allow(clippy::too_many_arguments)]
    fn begin_tunnel(
        &self,
        peer: SocketAddr,
        host: &str,
        port: u16,
        version: Version,
        headers: Option<&HeaderMap>,
        note: &str,
        always: bool,
    ) -> Option<Arc<Exchange>> {
        if !self.is_recording() || !(self.cfg.record_tunnels || always) {
            return None;
        }
        let filter = self.filter.read().clone();
        let url = hostport(host, port);
        if !filter.allows_request("CONNECT", &url, host, port) {
            return None;
        }
        let mut e = CaptureEntry::new(self.store.next_id(), peer, EntryKind::Tunnel);
        e.scheme = "tcp".to_owned();
        e.host = host.to_owned();
        e.port = port;
        e.method = "CONNECT".to_owned();
        e.url = url;
        e.http_version = version_str(version).to_owned();
        if let Some(h) = headers {
            let mut h = h.clone();
            h.remove(http::header::PROXY_AUTHORIZATION);
            e.request_headers = headers_vec(&h);
        }
        e.stream = Some(StreamStats::default());
        e.note = Some(note.to_owned());
        Some(Exchange::begin(self.store.clone(), e, self.cfg.max_body_bytes, &filter))
    }

    fn server_config(&self, host: &str, h2: bool) -> Result<Arc<rustls::ServerConfig>, ProxyError> {
        let key = (normalize_host(host), h2);
        if let Some(c) = self.server_configs.lock().get(&key) {
            return Ok(c.clone());
        }
        let ca = self.cfg.ca.as_ref().ok_or_else(|| ProxyError::Ca("no certificate authority".into()))?;
        let certified = ca.leaf_for(host)?;
        let mut config = rustls::ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| ProxyError::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified)));
        config.alpn_protocols = if h2 {
            vec![b"h2".to_vec(), b"http/1.1".to_vec(), b"http/1.0".to_vec()]
        } else {
            vec![b"http/1.1".to_vec(), b"http/1.0".to_vec()]
        };
        let config = Arc::new(config);
        let mut cache = self.server_configs.lock();
        if cache.len() >= SERVER_CONFIG_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, config.clone());
        Ok(config)
    }
}

/// The protocol named in `Upgrade` when the request asks to upgrade.
fn upgrade_protocol(headers: &HeaderMap) -> Option<String> {
    let asks = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case("upgrade"));
    if !asks {
        return None;
    }
    headers.get(UPGRADE).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Removes hop-by-hop and proxy headers. With `keep_upgrade` the
/// `Connection: upgrade` / `Upgrade` pair survives.
pub(crate) fn clean_hop_by_hop(headers: &mut HeaderMap, keep_upgrade: bool) {
    let listed: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty() && !(keep_upgrade && t == "upgrade"))
        .filter_map(|t| HeaderName::from_bytes(t.as_bytes()).ok())
        .collect();
    for name in listed {
        headers.remove(name);
    }
    let te_trailers = headers
        .get(TE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("trailers")));
    for name in
        ["keep-alive", "proxy-connection", "proxy-authorization", "proxy-authenticate", "te", "transfer-encoding"]
    {
        headers.remove(name);
    }
    headers.remove(CONNECTION);
    if keep_upgrade {
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    } else {
        headers.remove(UPGRADE);
    }
    if te_trailers {
        // gRPC requires it end to end.
        headers.insert(TE, HeaderValue::from_static("trailers"));
    }
}

/// HTTP/2 clients may split cookies over several headers; HTTP/1.1 servers
/// expect one.
fn merge_cookies(headers: &mut HeaderMap) {
    let cookies: Vec<String> =
        headers.get_all(COOKIE).iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect();
    if cookies.len() > 1
        && let Ok(v) = HeaderValue::from_str(&cookies.join("; "))
    {
        headers.insert(COOKIE, v);
    }
}

fn build_upstream_request(
    method: &Method,
    headers: &HeaderMap,
    scheme: &str,
    authority: &str,
    path: &str,
    h2: bool,
    body: ProxyBody,
) -> Result<Request<ProxyBody>, String> {
    let uri = if h2 {
        Uri::builder().scheme(scheme).authority(authority).path_and_query(path).build()
    } else {
        Uri::builder().path_and_query(path).build()
    }
    .map_err(|e| format!("invalid request target: {e}"))?;
    let mut req = Request::new(body);
    *req.method_mut() = method.clone();
    *req.uri_mut() = uri;
    *req.headers_mut() = headers.clone();
    if h2 {
        *req.version_mut() = Version::HTTP_2;
        req.headers_mut().remove(HOST);
    } else {
        *req.version_mut() = Version::HTTP_11;
        if !req.headers().contains_key(HOST) {
            let v = HeaderValue::from_str(authority).map_err(|e| format!("invalid host: {e}"))?;
            req.headers_mut().insert(HOST, v);
        }
    }
    Ok(req)
}

pub(crate) fn version_str(v: Version) -> &'static str {
    match v {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/1.1",
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = s.source();
    }
    out
}

pub(crate) fn error_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let mut resp = Response::new(full(format!("Merpati proxy: {message}\n")));
    *resp.status_mut() = status;
    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    resp.headers_mut().insert("x-merpati-proxy-error", HeaderValue::from_static("1"));
    resp
}

fn connect_established() -> Response<ProxyBody> {
    let mut resp = Response::new(empty());
    resp.extensions_mut().insert(hyper::ext::ReasonPhrase::from_static(b"Connection Established"));
    resp
}
