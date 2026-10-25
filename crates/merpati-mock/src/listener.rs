//! Local webhook listener (PRD FR-X10).
//!
//! Records every incoming request into a bounded ring buffer, answers with a
//! configurable response, and can replay a recorded request to another URL.
//! The listener only binds locally; a public URL needs a tunnel set up by the
//! user.
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! use merpati_mock::listener::{ListenerConfig, WebhookListener};
//! let hooks = WebhookListener::start(ListenerConfig::default()).await?;
//! println!("send webhooks to {}/anything", hooks.url());
//! for ev in hooks.events() {
//!     println!("#{} {} {}", ev.id, ev.method, ev.path);
//! }
//! hooks.stop().await;
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use futures_util::StreamExt;
use http::StatusCode;
use http::header::{HeaderName, HeaderValue};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Settings for [`WebhookListener::start`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerConfig {
    /// Interface to bind, default `127.0.0.1`.
    pub host: String,
    /// 0 picks a free port.
    pub port: u16,
    /// Status of the answer to every webhook (invalid codes fall back to 200).
    pub response_status: u16,
    pub response_headers: Vec<(String, String)>,
    pub response_body: String,
    /// Number of events kept; the oldest is dropped when full (minimum 1).
    pub capacity: usize,
    /// Request bodies longer than this are truncated in the record.
    pub max_body_bytes: usize,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        ListenerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            response_status: 200,
            response_headers: vec![("Content-Type".into(), "application/json".into())],
            response_body: "{\"ok\":true}".into(),
            capacity: 1000,
            max_body_bytes: 1024 * 1024,
        }
    }
}

/// One request received by the listener.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// Sequence number, starting at 1; unique for the listener's lifetime.
    pub id: u64,
    /// Unix milliseconds when the request arrived.
    pub timestamp_ms: i64,
    pub method: String,
    pub path: String,
    /// Raw query string without `?` (empty when absent).
    pub query: String,
    pub headers: Vec<(String, String)>,
    /// Body bytes, at most `max_body_bytes`.
    pub body: Vec<u8>,
    /// True when the body was longer than `max_body_bytes`.
    pub body_truncated: bool,
    /// Peer address (`ip:port`) when known.
    pub remote_addr: Option<String>,
}

impl WebhookEvent {
    /// Body as (lossy) UTF-8 text.
    pub fn body_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// Last value of a header (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Outcome of [`replay`].
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Time from sending until the whole response body was read.
    pub elapsed_ms: f64,
}

#[derive(Clone, Debug)]
struct ResponseSpec {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

struct Shared {
    capacity: usize,
    max_body_bytes: usize,
    response: RwLock<ResponseSpec>,
    events: Mutex<VecDeque<WebhookEvent>>,
    next_id: AtomicU64,
}

/// A running webhook listener. Dropping it also shuts it down.
pub struct WebhookListener {
    addr: SocketAddr,
    shared: Arc<Shared>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl WebhookListener {
    /// Binds to `cfg.host:cfg.port` (port 0 picks a free one) and starts
    /// recording.
    pub async fn start(cfg: ListenerConfig) -> std::io::Result<WebhookListener> {
        let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            capacity: cfg.capacity.max(1),
            max_body_bytes: cfg.max_body_bytes,
            response: RwLock::new(ResponseSpec {
                status: cfg.response_status,
                headers: cfg.response_headers,
                body: cfg.response_body,
            }),
            events: Mutex::new(VecDeque::new()),
            next_id: AtomicU64::new(1),
        });
        let app = Router::new().fallback(handle).with_state(shared.clone());
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let server = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                });
            if let Err(e) = server.await {
                tracing::error!("webhook listener error: {e}");
            }
        });
        tracing::info!("webhook listener on http://{addr}");
        Ok(WebhookListener { addr, shared, shutdown: Some(tx), task: Some(task) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Base URL to send webhooks to, e.g. `http://127.0.0.1:53211`. Any path
    /// under it is accepted.
    pub fn url(&self) -> String {
        let host = match self.addr.ip() {
            ip if ip.is_unspecified() => "127.0.0.1".to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
            ip => ip.to_string(),
        };
        format!("http://{host}:{}", self.addr.port())
    }

    /// Recorded events, oldest first.
    pub fn events(&self) -> Vec<WebhookEvent> {
        self.shared.events.lock().iter().cloned().collect()
    }

    /// The recorded event with `id`, if still in the buffer.
    pub fn event(&self, id: u64) -> Option<WebhookEvent> {
        self.shared.events.lock().iter().find(|e| e.id == id).cloned()
    }

    /// Forgets all recorded events (ids keep increasing).
    pub fn clear(&self) {
        self.shared.events.lock().clear();
    }

    /// Changes the answer sent to subsequent webhooks.
    pub fn set_response(&self, status: u16, headers: Vec<(String, String)>, body: impl Into<String>) {
        *self.shared.response.write() = ResponseSpec { status, headers, body: body.into() };
    }

    /// Stops accepting connections and waits (up to 3 s) for in-flight
    /// requests to finish.
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(Duration::from_secs(3), task).await.is_err() {
                abort.abort();
            }
        }
    }
}

impl Drop for WebhookListener {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

fn header_pairs(map: &http::HeaderMap) -> Vec<(String, String)> {
    map.iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect()
}

/// Hop-by-hop headers (RFC 9110 §7.6.1) plus the ones the HTTP client
/// recomputes for a new connection.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

/// Reads at most `cap` bytes and drains (discards) the rest so the client
/// still gets the configured answer.
async fn read_capped(body: Body, cap: usize) -> (Vec<u8>, bool) {
    let mut stream = body.into_data_stream();
    let mut buf = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        let room = cap - buf.len();
        if chunk.len() > room {
            buf.extend_from_slice(&chunk[..room]);
            truncated = true;
        } else {
            buf.extend_from_slice(&chunk);
        }
    }
    (buf, truncated)
}

fn build_response(spec: &ResponseSpec) -> Response {
    let mut res = Response::new(Body::from(spec.body.clone()));
    *res.status_mut() = StatusCode::from_u16(spec.status).unwrap_or(StatusCode::OK);
    for (k, v) in &spec.headers {
        if is_hop_by_hop(k) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.trim().as_bytes()), HeaderValue::from_str(v)) {
            res.headers_mut().append(name, value);
        }
    }
    res
}

async fn handle(State(shared): State<Arc<Shared>>, req: Request) -> Response {
    let timestamp_ms = chrono::Utc::now().timestamp_millis();
    let remote_addr = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.to_string());
    let (parts, body) = req.into_parts();
    let (body, body_truncated) = read_capped(body, shared.max_body_bytes).await;
    let event = WebhookEvent {
        id: shared.next_id.fetch_add(1, Ordering::Relaxed),
        timestamp_ms,
        method: parts.method.as_str().to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or("").to_string(),
        headers: header_pairs(&parts.headers),
        body,
        body_truncated,
        remote_addr,
    };
    {
        let mut events = shared.events.lock();
        while events.len() >= shared.capacity {
            events.pop_front();
        }
        events.push_back(event);
    }
    let spec = shared.response.read().clone();
    build_response(&spec)
}

/// `target_base_url` with the event's path and query appended.
fn replay_url(event: &WebhookEvent, target_base_url: &str) -> Result<url::Url, String> {
    let mut url =
        url::Url::parse(target_base_url.trim()).map_err(|e| format!("invalid target URL {target_base_url:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("unsupported scheme {:?}; use http or https", url.scheme()));
    }
    let base_path = url.path().trim_end_matches('/').to_string();
    let path = if event.path.starts_with('/') { event.path.clone() } else { format!("/{}", event.path) };
    url.set_path(&format!("{base_path}{path}"));
    let query = match (url.query().filter(|q| !q.is_empty()), event.query.is_empty()) {
        (Some(base), false) => Some(format!("{base}&{}", event.query)),
        (Some(base), true) => Some(base.to_string()),
        (None, false) => Some(event.query.clone()),
        (None, true) => None,
    };
    url.set_query(query.as_deref());
    Ok(url)
}

/// Sends `event` again to `target_base_url` + the event's path and query,
/// with the same method, headers (minus hop-by-hop ones) and body.
///
/// Fails when the recorded body was truncated, since replaying a partial body
/// would misrepresent the original request.
pub async fn replay(
    event: &WebhookEvent,
    target_base_url: &str,
    http: &reqwest::Client,
) -> Result<ReplayResult, String> {
    if event.body_truncated {
        return Err(format!(
            "cannot replay event #{}: its body was truncated to {} bytes (raise max_body_bytes)",
            event.id,
            event.body.len()
        ));
    }
    let url = replay_url(event, target_base_url)?;
    let method = reqwest::Method::from_bytes(event.method.as_bytes())
        .map_err(|_| format!("invalid HTTP method {:?}", event.method))?;
    // Headers named in `Connection` are hop-by-hop as well.
    let listed: Vec<String> = event
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, v)| v.split(',').map(|s| s.trim().to_ascii_lowercase()).collect::<Vec<_>>())
        .collect();
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in &event.headers {
        if is_hop_by_hop(k) || listed.iter().any(|l| l.eq_ignore_ascii_case(k)) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            headers.append(name, value);
        }
    }
    let started = Instant::now();
    let res = http
        .request(method, url.clone())
        .headers(headers)
        .body(event.body.clone())
        .send()
        .await
        .map_err(|e| format!("replay to {url} failed: {e}"))?;
    let status = res.status().as_u16();
    let headers = header_pairs(res.headers());
    let body = res.bytes().await.map_err(|e| format!("reading replay response from {url} failed: {e}"))?.to_vec();
    Ok(ReplayResult { status, headers, body, elapsed_ms: started.elapsed().as_secs_f64() * 1000.0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(path: &str, query: &str) -> WebhookEvent {
        WebhookEvent {
            id: 1,
            timestamp_ms: 0,
            method: "POST".into(),
            path: path.into(),
            query: query.into(),
            headers: Vec::new(),
            body: Vec::new(),
            body_truncated: false,
            remote_addr: None,
        }
    }

    #[test]
    fn replay_url_joins_base_path_and_query() {
        let u = replay_url(&event("/hooks/gh", "a=1"), "http://localhost:3000/").unwrap();
        assert_eq!(u.as_str(), "http://localhost:3000/hooks/gh?a=1");
        let u = replay_url(&event("/x", ""), "https://h.test/api?k=v").unwrap();
        assert_eq!(u.as_str(), "https://h.test/api/x?k=v");
        let u = replay_url(&event("/x", "b=2"), "https://h.test/api/?k=v").unwrap();
        assert_eq!(u.as_str(), "https://h.test/api/x?k=v&b=2");
        assert!(replay_url(&event("/x", ""), "ftp://h.test").is_err());
        assert!(replay_url(&event("/x", ""), "not a url").is_err());
    }

    #[test]
    fn hop_by_hop_names() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("transfer-encoding"));
        assert!(is_hop_by_hop("Host"));
        assert!(!is_hop_by_hop("content-type"));
        assert!(!is_hop_by_hop("x-hub-signature-256"));
    }
}
