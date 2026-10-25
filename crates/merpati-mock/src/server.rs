//! The HTTP side of the mock server (axum).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use http::header::{HeaderName, HeaderValue};
use http::{Method, StatusCode};
use merpati_core::model::Collection;
use parking_lot::{Mutex, RwLock};
use rand::Rng;
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::matcher::{IncomingRequest, MatchResult, MockRoute, find_match, routes};
use crate::template::TemplateContext;
use crate::{FailureKind, MockConfig};

/// Largest request body read for matching and logging.
const MAX_REQUEST_BODY: usize = 16 * 1024 * 1024;
/// Request bodies longer than this are truncated in the log.
const LOG_BODY_LIMIT: usize = 16 * 1024;

/// One request received by the mock server.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MockLogEntry {
    /// Unix milliseconds when the request arrived.
    pub timestamp_ms: i64,
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    /// Request body (lossy UTF-8), truncated to 16 KiB.
    pub body: String,
    /// `request name / example name` when an example matched.
    pub matched: Option<String>,
    /// Status sent; 0 when a simulated failure dropped the connection.
    pub status: u16,
    pub duration_ms: f64,
}

struct Shared {
    collection: RwLock<Arc<Collection>>,
    config: MockConfig,
    logs: Mutex<VecDeque<MockLogEntry>>,
}

impl Shared {
    fn push_log(&self, entry: MockLogEntry) {
        let cap = self.config.log_capacity.max(1);
        let mut logs = self.logs.lock();
        while logs.len() >= cap {
            logs.pop_front();
        }
        logs.push_back(entry);
    }
}

/// A running mock server. Dropping it also shuts the server down.
pub struct MockServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Binds to `config.host:config.port` (port 0 picks a free one) and starts
    /// serving examples from `collection`.
    pub async fn start(collection: Collection, config: MockConfig) -> std::io::Result<MockServer> {
        let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            collection: RwLock::new(Arc::new(collection)),
            config,
            logs: Mutex::new(VecDeque::new()),
        });
        let app = Router::new().fallback(handle).with_state(shared.clone());
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = rx.await;
            });
            if let Err(e) = server.await {
                tracing::error!("mock server error: {e}");
            }
        });
        tracing::info!("mock server listening on http://{addr}");
        Ok(MockServer { addr, shared, shutdown: Some(tx), task: Some(task) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Base URL to point requests at, e.g. `http://127.0.0.1:53211`.
    pub fn url(&self) -> String {
        let host = match self.addr.ip() {
            ip if ip.is_unspecified() => "127.0.0.1".to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
            ip => ip.to_string(),
        };
        format!("http://{host}:{}", self.addr.port())
    }

    pub fn config(&self) -> &MockConfig {
        &self.shared.config
    }

    /// Logged requests, oldest first.
    pub fn logs(&self) -> Vec<MockLogEntry> {
        self.shared.logs.lock().iter().cloned().collect()
    }

    pub fn clear_logs(&self) {
        self.shared.logs.lock().clear();
    }

    /// Replaces the served collection; takes effect for the next request.
    pub fn set_collection(&self, c: Collection) {
        *self.shared.collection.write() = Arc::new(c);
    }

    pub fn routes(&self) -> Vec<MockRoute> {
        routes(&self.shared.collection.read())
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

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

fn header_pairs(map: &http::HeaderMap) -> Vec<(String, String)> {
    map.iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect()
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    let mut res = Response::new(Body::from(value.to_string()));
    *res.status_mut() = status;
    res.headers_mut().insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    res
}

fn not_found() -> Response {
    json_response(
        StatusCode::NOT_FOUND,
        serde_json::json!({"error": {
            "name": "mockRequestNotFoundError",
            "message": "Double check your method and the request path and try again.",
            "header": "No matching requests"
        }}),
    )
}

fn add_cors(res: &mut Response, incoming: &IncomingRequest) {
    let h = res.headers_mut();
    let origin = incoming.header("origin").and_then(|o| HeaderValue::from_str(o).ok());
    if !h.contains_key(http::header::ACCESS_CONTROL_ALLOW_ORIGIN) {
        match origin {
            Some(o) => {
                h.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, o);
                h.insert(http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
                h.append(http::header::VARY, HeaderValue::from_static("Origin"));
            }
            None => {
                h.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            }
        }
    }
    if !h.contains_key(http::header::ACCESS_CONTROL_EXPOSE_HEADERS) {
        h.insert(http::header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("*"));
    }
}

fn preflight(incoming: &IncomingRequest) -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::NO_CONTENT;
    let h = res.headers_mut();
    h.insert(
        http::header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS"),
    );
    let allow_headers = incoming
        .header("access-control-request-headers")
        .and_then(|v| HeaderValue::from_str(v).ok())
        .unwrap_or_else(|| HeaderValue::from_static("*"));
    h.insert(http::header::ACCESS_CONTROL_ALLOW_HEADERS, allow_headers);
    h.insert(http::header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
    add_cors(&mut res, incoming);
    res
}

/// Headers that describe the original transfer, not the example body.
fn is_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length" | "transfer-encoding" | "connection" | "keep-alive" | "content-encoding" | "upgrade"
    )
}

fn content_type_for(language: Option<&str>, body: &str) -> &'static str {
    match language.map(str::to_ascii_lowercase).as_deref() {
        Some("json") => "application/json; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("javascript") => "application/javascript; charset=utf-8",
        Some("text") => "text/plain; charset=utf-8",
        _ => {
            let t = body.trim_start();
            if (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<serde_json::Value>(body).is_ok() {
                "application/json; charset=utf-8"
            } else if t.starts_with("<?xml") {
                "application/xml; charset=utf-8"
            } else if t.starts_with('<') {
                "text/html; charset=utf-8"
            } else {
                "text/plain; charset=utf-8"
            }
        }
    }
}

fn example_response(m: &MatchResult<'_>, shared: &Shared, incoming: &IncomingRequest) -> Response {
    let ctx = TemplateContext::new(&shared.config.variables, &m.path_params, incoming);
    let ex = m.example;
    let body = ctx.render(&ex.body);
    let status = StatusCode::from_u16(ex.status).unwrap_or(StatusCode::OK);
    let mut res = Response::new(Body::empty());
    *res.status_mut() = status;
    let mut has_ct = false;
    for kv in ex.headers.iter().filter(|kv| kv.enabled && !kv.key.trim().is_empty()) {
        if is_hop_header(&kv.key) {
            continue;
        }
        let (Ok(name), Ok(value)) =
            (HeaderName::from_bytes(kv.key.trim().as_bytes()), HeaderValue::from_str(&ctx.render(&kv.value)))
        else {
            continue;
        };
        has_ct |= name == http::header::CONTENT_TYPE;
        res.headers_mut().append(name, value);
    }
    if !has_ct && !body.is_empty() {
        let ct = content_type_for(ex.language.as_deref(), &body);
        res.headers_mut().insert(http::header::CONTENT_TYPE, HeaderValue::from_static(ct));
    }
    *res.body_mut() = Body::from(body);
    res
}

/// Re-streams the body in 50 ms slices of at most `bps / 20` bytes.
fn throttled(res: Response, bps: u64) -> Response {
    use futures_util::StreamExt;
    let slice = (bps / 20).max(1) as usize;
    let (mut parts, body) = res.into_parts();
    // Keep the length known (the re-stream would otherwise go out chunked).
    if let Some(len) = axum::body::HttpBody::size_hint(&body).exact() {
        parts.headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    let chunks = body.into_data_stream().flat_map(move |chunk| {
        let pieces: Vec<Result<Bytes, axum::Error>> = match chunk {
            Ok(b) => (0..b.len()).step_by(slice).map(|i| Ok(b.slice(i..(i + slice).min(b.len())))).collect(),
            Err(e) => vec![Err(e)],
        };
        futures_util::stream::iter(pieces)
    });
    let paced = chunks.then(|piece| async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        piece
    });
    Response::from_parts(parts, Body::from_stream(paced))
}

/// A body that fails immediately, so the client sees a broken connection.
fn dropped_connection() -> Response {
    let stream = futures_util::stream::once(async {
        Err::<Bytes, std::io::Error>(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "simulated network failure",
        ))
    });
    Response::new(Body::from_stream(stream))
}

async fn handle(State(shared): State<Arc<Shared>>, req: Request) -> Response {
    let started = Instant::now();
    let timestamp_ms = chrono::Utc::now().timestamp_millis();
    let (parts, body) = req.into_parts();
    let body = to_bytes(body, MAX_REQUEST_BODY).await.unwrap_or_default();
    let incoming = IncomingRequest {
        method: parts.method.as_str().to_uppercase(),
        path: parts.uri.path().to_string(),
        query: url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes()).into_owned().collect(),
        headers: header_pairs(&parts.headers),
        body: body.to_vec(),
    };
    let cfg = &shared.config;

    let mut matched = None;
    let mut dropped = false;
    let mut res =
        if cfg.cors && parts.method == Method::OPTIONS && incoming.header("access-control-request-method").is_some() {
            preflight(&incoming)
        } else {
            let jitter = match cfg.network.jitter_ms {
                0 => 0,
                j => rand::rng().random_range(0..=j),
            };
            let delay = cfg.delay_ms + cfg.network.latency_ms + jitter;
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            let failure = cfg.failure.as_ref().filter(|f| f.rate > 0.0 && rand::rng().random::<f32>() < f.rate);
            match failure.map(|f| f.kind) {
                Some(FailureKind::Status(code)) => {
                    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    json_response(
                        status,
                        serde_json::json!({"error": {"name": "mockSimulatedFailure", "message": "Simulated failure"}}),
                    )
                }
                Some(FailureKind::Drop) => {
                    dropped = true;
                    dropped_connection()
                }
                Some(FailureKind::Timeout) => {
                    tokio::time::sleep(Duration::from_secs(300)).await;
                    dropped = true;
                    dropped_connection()
                }
                None => {
                    let collection = shared.collection.read().clone();
                    match find_match(&collection, &cfg.variables, &incoming) {
                        Some(m) => {
                            matched = Some(m.label());
                            example_response(&m, &shared, &incoming)
                        }
                        None => not_found(),
                    }
                }
            }
        };
    if cfg.cors {
        add_cors(&mut res, &incoming);
    }
    if let Some(bps) = cfg.network.bandwidth_bytes_per_sec.filter(|b| *b > 0) {
        res = throttled(res, bps);
    }

    let mut log_body = String::from_utf8_lossy(&body[..body.len().min(LOG_BODY_LIMIT)]).into_owned();
    if body.len() > LOG_BODY_LIMIT {
        log_body.push_str("…(truncated)");
    }
    shared.push_log(MockLogEntry {
        timestamp_ms,
        method: incoming.method.clone(),
        path: incoming.path.clone(),
        query: parts.uri.query().unwrap_or("").to_string(),
        headers: incoming.headers.clone(),
        body: log_body,
        matched,
        status: if dropped { 0 } else { res.status().as_u16() },
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
    });
    res
}
