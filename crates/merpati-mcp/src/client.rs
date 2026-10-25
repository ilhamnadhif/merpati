//! The JSON-RPC 2.0 session shared by all transports, and [`McpClient`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use reqwest::header::AUTHORIZATION;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::config::{McpConfig, McpHttpTransport, Transport};
use crate::error::McpError;
use crate::features::{ElicitationHandler, ElicitationRequest, Root, SamplingHandler, SamplingRequest};
use crate::oauth::{AuthManager, OAuthOptions};
use crate::types::{CallResult, Direction, McpEvent, Prompt, Resource, ResourceTemplate, ServerInfo, Tool};
use crate::{http, legacy, stdio};

/// The version offered first.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
/// Offered in this order when a server rejects the newer one.
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// Versions accepted without a warning when the server answers with them.
const KNOWN_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_PAGES: usize = 500;

pub(crate) type EventFn = Arc<dyn Fn(McpEvent) + Send + Sync>;
type Reply = Result<Value, McpError>;

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// A serialized JSON-RPC message on its way to the transport.
pub(crate) struct Outbound {
    pub json: String,
    /// Set for requests, so a transport failure can fail the waiting call.
    pub request_id: Option<String>,
}

pub(crate) fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Counts what is waiting for the user (a browser login, a sampling or
/// elicitation dialog). Request timeouts are paused meanwhile.
pub(crate) struct Busy {
    count: watch::Sender<usize>,
    last_idle: Mutex<Instant>,
}

pub(crate) struct BusyGuard(Arc<Busy>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        let busy = &self.0;
        busy.count.send_modify(|n| *n = n.saturating_sub(1));
        if *busy.count.borrow() == 0 {
            *busy.last_idle.lock() = Instant::now();
        }
    }
}

impl Busy {
    pub fn new() -> Busy {
        Busy { count: watch::channel(0).0, last_idle: Mutex::new(Instant::now()) }
    }

    pub fn enter(self: &Arc<Self>) -> BusyGuard {
        self.count.send_modify(|n| *n += 1);
        BusyGuard(self.clone())
    }

    fn is_busy(&self) -> bool {
        *self.count.borrow() > 0
    }

    async fn idle(&self) {
        let mut rx = self.count.subscribe();
        let _ = rx.wait_for(|n| *n == 0).await;
    }

    /// Resolves once `timeout` has passed since `start` without the client
    /// waiting for the user in between.
    async fn timer(&self, start: Instant, timeout: Duration) {
        let mut deadline = start + timeout;
        loop {
            tokio::time::sleep_until(deadline.into()).await;
            if self.is_busy() {
                self.idle().await;
                deadline = Instant::now() + timeout;
                continue;
            }
            let idle_since = *self.last_idle.lock();
            if idle_since > start && idle_since + timeout > deadline {
                deadline = idle_since + timeout;
                continue;
            }
            return;
        }
    }
}

/// What the host offers to servers.
#[derive(Default)]
pub(crate) struct Features {
    sampling: Option<Arc<dyn SamplingHandler>>,
    elicitation: Option<Arc<dyn ElicitationHandler>>,
    roots: Option<Mutex<Vec<Root>>>,
}

impl Features {
    fn capabilities(&self) -> Value {
        let mut caps = Map::new();
        if self.roots.is_some() {
            caps.insert("roots".into(), json!({"listChanged": true}));
        }
        if self.sampling.is_some() {
            caps.insert("sampling".into(), json!({}));
        }
        if self.elicitation.is_some() {
            caps.insert("elicitation".into(), json!({}));
        }
        Value::Object(caps)
    }
}

/// State shared between the client handle and the transport tasks.
pub(crate) struct Shared {
    on_event: EventFn,
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    outbox: mpsc::UnboundedSender<Outbound>,
    closed: Mutex<Option<String>>,
    next_id: AtomicI64,
    /// Cancelled once the connection is closed; stops every background task.
    pub cancel: CancellationToken,
    stderr_tail: Mutex<VecDeque<String>>,
    busy: Arc<Busy>,
    features: Arc<Features>,
    server_name: Mutex<String>,
    /// Server-to-client requests being handled, by id (for cancellation).
    inflight: Mutex<HashMap<String, CancellationToken>>,
}

impl Shared {
    fn new(
        on_event: EventFn,
        outbox: mpsc::UnboundedSender<Outbound>,
        busy: Arc<Busy>,
        features: Arc<Features>,
    ) -> Shared {
        Shared {
            on_event,
            pending: Mutex::new(HashMap::new()),
            outbox,
            closed: Mutex::new(None),
            next_id: AtomicI64::new(1),
            cancel: CancellationToken::new(),
            stderr_tail: Mutex::new(VecDeque::new()),
            busy,
            features,
            server_name: Mutex::new(String::new()),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    pub fn emit(&self, event: McpEvent) {
        (self.on_event)(event);
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.emit(McpEvent::Log { level: "warning".into(), message: message.into() });
    }

    pub fn push_stderr(&self, line: &str) {
        let mut tail = self.stderr_tail.lock();
        if tail.len() == 5 {
            tail.pop_front();
        }
        tail.push_back(line.to_string());
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().iter().cloned().collect::<Vec<_>>().join("\n")
    }

    pub fn closed_reason(&self) -> Option<String> {
        self.closed.lock().clone()
    }

    pub fn is_pending(&self, id: &str) -> bool {
        self.pending.lock().contains_key(id)
    }

    /// Completes a waiting request with an error (transport failures).
    pub fn fail(&self, id: &str, err: McpError) {
        if let Some(tx) = self.pending.lock().remove(id) {
            let _ = tx.send(Err(err));
        }
    }

    /// Marks the connection closed (first reason wins), fails every waiting
    /// request, emits [`McpEvent::Closed`] and stops the background tasks.
    pub fn mark_closed(&self, reason: impl Into<String>) {
        self.close(reason.into(), true);
    }

    fn close(&self, reason: String, emit: bool) {
        {
            let mut closed = self.closed.lock();
            if closed.is_some() {
                return;
            }
            *closed = Some(reason.clone());
        }
        let pending: Vec<_> = self.pending.lock().drain().collect();
        for (_, tx) in pending {
            let _ = tx.send(Err(McpError::Closed(reason.clone())));
        }
        if emit {
            self.emit(McpEvent::Closed(reason));
        }
        self.cancel.cancel();
    }

    fn send_value(&self, msg: &Value, request_id: Option<String>) -> Result<(), McpError> {
        let json = msg.to_string();
        self.emit(McpEvent::Traffic { direction: Direction::Outgoing, json: json.clone(), at_ms: now_ms() });
        self.outbox
            .send(Outbound { json, request_id })
            .map_err(|_| McpError::Closed(self.closed_reason().unwrap_or_else(|| "transport stopped".into())))
    }

    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let mut msg = json!({"jsonrpc": "2.0", "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        self.send_value(&msg, None)
    }

    pub async fn request(&self, method: &str, params: Option<Value>, timeout: Duration) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let key = id.to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(key.clone(), tx);
        // Removes the entry when the request ends. One still waiting for its
        // answer (timed out, or the caller dropped this future) is cancelled
        // on the server, except `initialize`, which must not be cancelled.
        struct Guard<'a> {
            shared: &'a Shared,
            key: &'a str,
            id: i64,
            /// Set once the request was handed to the transport.
            cancel: bool,
            reason: &'static str,
        }
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                let waiting = self.shared.pending.lock().remove(self.key).is_some();
                if waiting && self.cancel && self.shared.closed_reason().is_none() {
                    let params = json!({"requestId": self.id, "reason": self.reason});
                    let _ = self.shared.notify("notifications/cancelled", Some(params));
                }
            }
        }
        let mut guard = Guard { shared: self, key: &key, id, cancel: false, reason: "Request abandoned by the client" };
        if let Some(reason) = self.closed_reason() {
            return Err(McpError::Closed(reason));
        }
        let mut msg = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        let start = Instant::now();
        self.send_value(&msg, Some(key.clone()))?;
        guard.cancel = method != "initialize";
        tokio::select! {
            reply = rx => match reply {
                Ok(reply) => reply,
                Err(_) => Err(McpError::Closed(self.closed_reason().unwrap_or_else(|| "connection dropped".into()))),
            },
            _ = self.busy.timer(start, timeout) => {
                guard.reason = "Request timed out";
                Err(McpError::Timeout { method: method.to_string(), after: timeout })
            }
        }
    }

    /// Handles one wire message (a line or an SSE `data` payload).
    pub fn handle_incoming(self: &Arc<Self>, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.emit(McpEvent::Traffic { direction: Direction::Incoming, json: text.to_string(), at_ms: now_ms() });
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(items)) => items.into_iter().for_each(|m| self.dispatch(m)),
            Ok(v) => self.dispatch(v),
            Err(e) => self.warn(format!("Ignoring a message that is not valid JSON: {e}")),
        }
    }

    fn dispatch(self: &Arc<Self>, msg: Value) {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").filter(|v| !v.is_null());
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                self.answer_server_request(method, id.clone(), params);
            }
            (Some(method), None) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                if method == "notifications/message" {
                    let level = params.get("level").and_then(Value::as_str).unwrap_or("info").to_string();
                    let data = match params.get("data") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => String::new(),
                    };
                    let message = match params.get("logger").and_then(Value::as_str) {
                        Some(logger) if !logger.is_empty() => format!("[{logger}] {data}"),
                        _ => data,
                    };
                    self.emit(McpEvent::Log { level, message });
                } else {
                    if method == "notifications/cancelled"
                        && let Some(rid) = params.get("requestId")
                        && let Some(token) = self.inflight.lock().remove(&id_key(rid))
                    {
                        token.cancel();
                    }
                    self.emit(McpEvent::Notification { method: method.to_string(), params });
                }
            }
            (None, Some(id)) => {
                let key = id_key(id);
                let Some(tx) = self.pending.lock().remove(&key) else {
                    self.warn(format!("Response for unknown request id {key}"));
                    return;
                };
                let reply = match msg.get("error").filter(|e| !e.is_null()) {
                    Some(err) => Err(McpError::Rpc {
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: err.get("message").and_then(Value::as_str).unwrap_or("Unknown error").to_string(),
                        data: err.get("data").cloned(),
                    }),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(reply);
            }
            (None, None) => {
                // An error with a null id: the server could not parse a request.
                if let Some(err) = msg.get("error") {
                    let text = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                    self.emit(McpEvent::Log { level: "error".into(), message: format!("Server error: {text}") });
                } else {
                    self.warn("Ignoring a message without method or id");
                }
            }
        }
    }

    /// Sends the response to a server-to-client request.
    fn reply(&self, id: &Value, result: Result<Value, McpError>) {
        let msg = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(McpError::Rpc { code, message, data }) => {
                let mut error = json!({"code": code, "message": message});
                if let Some(d) = data {
                    error["data"] = d;
                }
                json!({"jsonrpc": "2.0", "id": id, "error": error})
            }
            Err(other) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": other.to_string()}}),
        };
        let _ = self.send_value(&msg, None);
    }

    fn method_not_found(&self, id: &Value, method: &str) {
        self.reply(
            id,
            Err(McpError::Rpc {
                code: -32601,
                message: format!("Method not supported by this client: {method}"),
                data: None,
            }),
        );
    }

    /// Runs a host handler for a server request on its own task; the server
    /// may cancel it with `notifications/cancelled`.
    fn run_handler(self: &Arc<Self>, id: Value, fut: impl Future<Output = Result<Value, McpError>> + Send + 'static) {
        let key = id_key(&id);
        let token = CancellationToken::new();
        self.inflight.lock().insert(key.clone(), token.clone());
        let shared = self.clone();
        let busy = self.busy.enter();
        tokio::spawn(async move {
            let _busy = busy;
            let out = tokio::select! {
                r = fut => Some(r),
                _ = token.cancelled() => None,
                _ = shared.cancel.cancelled() => None,
            };
            shared.inflight.lock().remove(&key);
            if let Some(r) = out {
                shared.reply(&id, r);
            }
        });
    }

    /// Requests the server sends to the client. Sampling, elicitation and
    /// roots are served only when the host enabled them.
    fn answer_server_request(self: &Arc<Self>, method: &str, id: Value, params: Value) {
        let server = self.server_name.lock().clone();
        match method {
            "ping" => self.reply(&id, Ok(json!({}))),
            "roots/list" => match &self.features.roots {
                Some(roots) => {
                    let list: Vec<Value> = roots.lock().iter().map(Root::to_value).collect();
                    self.reply(&id, Ok(json!({"roots": list})));
                }
                None => self.method_not_found(&id, method),
            },
            "sampling/createMessage" => match self.features.sampling.clone() {
                Some(handler) => match SamplingRequest::parse(&server, &params) {
                    Ok(req) => {
                        self.run_handler(id, async move { handler.create_message(req).await.map(|r| r.to_value()) })
                    }
                    Err(e) => self.reply(&id, Err(McpError::Rpc { code: -32602, message: e.to_string(), data: None })),
                },
                None => self.method_not_found(&id, method),
            },
            "elicitation/create" => match self.features.elicitation.clone() {
                Some(handler) => {
                    let req = ElicitationRequest::parse(&server, &params);
                    self.run_handler(id, async move { handler.elicit(req).await.map(|r| r.to_value()) });
                }
                None => self.method_not_found(&id, method),
            },
            _ => self.method_not_found(&id, method),
        }
    }
}

pub(crate) enum TransportHandle {
    Stdio(stdio::StdioHandle),
    Http(Arc<http::HttpConn>),
    Sse(Arc<legacy::SseConn>),
}

struct Conn {
    shared: Arc<Shared>,
    transport: TransportHandle,
    timeout: Duration,
    /// For cleanup work when the last handle is dropped outside the runtime.
    runtime: tokio::runtime::Handle,
}

impl Drop for Conn {
    fn drop(&mut self) {
        if self.shared.closed_reason().is_none() {
            if let TransportHandle::Http(h) = &self.transport {
                let h = h.clone();
                self.runtime.spawn(async move { h.terminate().await });
            }
            self.shared.mark_closed("Disconnected");
        }
        self.shared.cancel.cancel();
    }
}

/// A connected MCP session. Cheap to clone; the connection (and a stdio
/// server process) ends with [`McpClient::close`] or when the last clone is
/// dropped.
#[derive(Clone)]
pub struct McpClient {
    conn: Arc<Conn>,
    info: Arc<ServerInfo>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server", &self.info.name)
            .field("transport", &self.transport_name())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// Configures and opens an [`McpClient`]: HTTP client, OAuth sign-in and
/// the client features offered to the server.
pub struct McpClientBuilder {
    cfg: McpConfig,
    http: Option<reqwest::Client>,
    oauth: Option<OAuthOptions>,
    features: Features,
    on_event: EventFn,
}

impl std::fmt::Debug for McpClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClientBuilder")
            .field("cfg", &self.cfg)
            .field("oauth", &self.oauth)
            .field("sampling", &self.features.sampling.is_some())
            .field("elicitation", &self.features.elicitation.is_some())
            .field("roots", &self.features.roots.as_ref().map(|r| r.lock().len()))
            .finish()
    }
}

impl McpClientBuilder {
    /// The HTTP client for remote servers and OAuth requests (to honor the
    /// app's proxy and certificate settings).
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Enables the OAuth authorization flow when a remote server answers 401
    /// (unless the config sends a static `Authorization` header or sets
    /// [`McpConfig::oauth`] to `None`).
    pub fn oauth(mut self, options: OAuthOptions) -> Self {
        self.oauth = Some(options);
        self
    }

    /// Offers `sampling` and serves `sampling/createMessage` with `handler`.
    pub fn sampling(mut self, handler: impl SamplingHandler + 'static) -> Self {
        self.features.sampling = Some(Arc::new(handler));
        self
    }

    /// Offers `elicitation` and serves `elicitation/create` with `handler`.
    pub fn elicitation(mut self, handler: impl ElicitationHandler + 'static) -> Self {
        self.features.elicitation = Some(Arc::new(handler));
        self
    }

    /// Offers `roots` (with `listChanged`) and answers `roots/list` with
    /// this list; change it later with [`McpClient::set_roots`].
    pub fn roots(mut self, roots: Vec<Root>) -> Self {
        self.features.roots = Some(Mutex::new(roots));
        self
    }

    /// Receives every [`McpEvent`] (from a runtime thread).
    pub fn on_event(mut self, on_event: impl Fn(McpEvent) + Send + Sync + 'static) -> Self {
        self.on_event = Arc::new(on_event);
        self
    }

    /// Starts the transport, performs the `initialize` handshake (with
    /// protocol version fallback) and sends `notifications/initialized`.
    /// Remote servers: Streamable HTTP first, then legacy HTTP+SSE when the
    /// POST is rejected with a 4xx status (in [`McpHttpTransport::Auto`]).
    pub async fn connect(self) -> Result<McpClient, McpError> {
        let McpClientBuilder { cfg, http, oauth, features, on_event } = self;
        let features = Arc::new(features);
        let busy = Arc::new(Busy::new());
        let ctx = Attempt { cfg: &cfg, features: &features, busy: &busy, on_event: &on_event };
        let result = match &cfg.transport {
            Transport::Stdio { .. } => ctx.run(Kind::Stdio, None).await,
            Transport::Http { url, headers } => {
                let url = http::parse_url(url)?;
                let headers = http::header_map(headers)?;
                let client = match http {
                    Some(c) => c,
                    None => http::build_client()?,
                };
                let auth = match (oauth, &cfg.oauth) {
                    (Some(opts), Some(settings)) if !headers.contains_key(AUTHORIZATION) => Some(Arc::new(
                        AuthManager::new(client.clone(), &url, settings.clone(), opts, on_event.clone(), busy.clone()),
                    )),
                    _ => None,
                };
                let remote = Remote { url, headers, client, auth };
                match cfg.http_transport {
                    McpHttpTransport::Streamable => ctx.run(Kind::Streamable, Some(&remote)).await,
                    McpHttpTransport::Sse => ctx.run(Kind::Sse, Some(&remote)).await,
                    McpHttpTransport::Auto => match ctx.run(Kind::Streamable, Some(&remote)).await {
                        Err(failed) if failed.started && falls_back(&failed.error) => {
                            (on_event)(McpEvent::Log {
                                level: "warning".into(),
                                message: format!(
                                    "Streamable HTTP was rejected ({}); trying the legacy HTTP+SSE transport",
                                    failed.error
                                ),
                            });
                            match ctx.run(Kind::Sse, Some(&remote)).await {
                                Ok(c) => Ok(c),
                                Err(sse) => Err(Failed { error: combine(failed.error, &sse.error), started: true }),
                            }
                        }
                        other => other,
                    },
                }
            }
        };
        result.map_err(|f| {
            if f.started {
                (on_event)(McpEvent::Closed(format!("Initialization failed: {}", f.error)));
            }
            f.error
        })
    }
}

/// Whether a failed Streamable HTTP `initialize` should be retried with
/// HTTP+SSE (spec: "If it fails with an HTTP 4xx status code").
fn falls_back(e: &McpError) -> bool {
    matches!(e, McpError::Http { status, .. } if (400..500).contains(status) && !matches!(status, 401 | 403 | 408 | 429))
}

fn combine(first: McpError, second: &McpError) -> McpError {
    match first {
        McpError::Http { status, message } => {
            McpError::Http { status, message: format!("{message} (HTTP+SSE fallback: {second})") }
        }
        other => other,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Stdio,
    Streamable,
    Sse,
}

struct Remote {
    url: reqwest::Url,
    headers: reqwest::header::HeaderMap,
    client: reqwest::Client,
    auth: Option<Arc<AuthManager>>,
}

struct Failed {
    error: McpError,
    /// The transport was up (a `Closed` event is due).
    started: bool,
}

struct Attempt<'a> {
    cfg: &'a McpConfig,
    features: &'a Arc<Features>,
    busy: &'a Arc<Busy>,
    on_event: &'a EventFn,
}

impl Attempt<'_> {
    async fn run(&self, kind: Kind, remote: Option<&Remote>) -> Result<McpClient, Failed> {
        let early = |error| Failed { error, started: false };
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared::new(self.on_event.clone(), tx, self.busy.clone(), self.features.clone()));
        let transport = match (kind, remote, &self.cfg.transport) {
            (Kind::Stdio, _, Transport::Stdio { command, args, env, cwd }) => {
                TransportHandle::Stdio(stdio::start(command, args, env, cwd.as_deref(), &shared, rx).map_err(early)?)
            }
            (Kind::Streamable, Some(r), _) => TransportHandle::Http(http::start(
                r.url.clone(),
                r.headers.clone(),
                r.client.clone(),
                r.auth.clone(),
                &shared,
                rx,
                self.cfg.timeout,
            )),
            (Kind::Sse, Some(r), _) => {
                let started = legacy::start(
                    r.url.clone(),
                    r.headers.clone(),
                    r.client.clone(),
                    r.auth.clone(),
                    &shared,
                    rx,
                    self.cfg.timeout,
                )
                .await;
                match started {
                    Ok(c) => TransportHandle::Sse(c),
                    Err(e) => {
                        shared.close(format!("Connection failed: {e}"), false);
                        return Err(Failed { error: e, started: true });
                    }
                }
            }
            _ => return Err(early(McpError::Config("transport mismatch".into()))),
        };
        let conn =
            Arc::new(Conn { shared, transport, timeout: self.cfg.timeout, runtime: tokio::runtime::Handle::current() });
        match initialize(&conn).await {
            Ok(info) => {
                *conn.shared.server_name.lock() = info.title.clone().unwrap_or_else(|| info.name.clone());
                if let TransportHandle::Http(h) = &conn.transport {
                    h.start_listener();
                }
                Ok(McpClient { conn, info: Arc::new(info) })
            }
            Err(e) => {
                conn.shared.close(format!("Initialization failed: {e}"), false);
                if let TransportHandle::Stdio(s) = &conn.transport {
                    s.wait_exit(Duration::from_secs(3)).await;
                }
                Err(Failed { error: e, started: true })
            }
        }
    }
}

impl McpClient {
    /// A builder for a connection with OAuth sign-in, client features or a
    /// custom HTTP client.
    pub fn builder(cfg: McpConfig) -> McpClientBuilder {
        McpClientBuilder { cfg, http: None, oauth: None, features: Features::default(), on_event: Arc::new(|_| {}) }
    }

    /// Starts the transport, performs the `initialize` handshake (with
    /// protocol version fallback) and sends `notifications/initialized`.
    pub async fn connect(
        cfg: McpConfig,
        on_event: impl Fn(McpEvent) + Send + Sync + 'static,
    ) -> Result<McpClient, McpError> {
        Self::builder(cfg).on_event(on_event).connect().await
    }

    /// Like [`McpClient::connect`], using the given HTTP client for remote
    /// servers (to honor the app's proxy and certificate settings).
    pub async fn connect_with_http(
        cfg: McpConfig,
        http: reqwest::Client,
        on_event: impl Fn(McpEvent) + Send + Sync + 'static,
    ) -> Result<McpClient, McpError> {
        Self::builder(cfg).http_client(http).on_event(on_event).connect().await
    }

    pub fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    /// `stdio`, `streamable-http` or `sse` (legacy HTTP+SSE).
    pub fn transport_name(&self) -> &'static str {
        match &self.conn.transport {
            TransportHandle::Stdio(_) => "stdio",
            TransportHandle::Http(_) => "streamable-http",
            TransportHandle::Sse(_) => "sse",
        }
    }

    pub fn is_closed(&self) -> bool {
        self.conn.shared.closed_reason().is_some()
    }

    /// Why the connection ended, once it has.
    pub fn closed_reason(&self) -> Option<String> {
        self.conn.shared.closed_reason()
    }

    /// The per-request timeout from the config.
    pub fn timeout(&self) -> Duration {
        self.conn.timeout
    }

    /// Sends any JSON-RPC request and returns its `result`.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        self.conn.shared.request(method, params, self.conn.timeout).await
    }

    /// Sends a JSON-RPC notification.
    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        self.conn.shared.notify(method, params)
    }

    /// Replaces the roots offered to the server and sends
    /// `notifications/roots/list_changed`. Fails when roots were not enabled
    /// with [`McpClientBuilder::roots`] (capabilities are fixed at connect).
    pub fn set_roots(&self, roots: Vec<Root>) -> Result<(), McpError> {
        let Some(current) = &self.conn.shared.features.roots else {
            return Err(McpError::Config("Roots were not enabled for this connection".into()));
        };
        *current.lock() = roots;
        self.notify("notifications/roots/list_changed", None)
    }

    /// The roots currently offered (empty when roots are not enabled).
    pub fn roots(&self) -> Vec<Root> {
        self.conn.shared.features.roots.as_ref().map(|r| r.lock().clone()).unwrap_or_default()
    }

    /// Round-trip time of a `ping`.
    pub async fn ping(&self) -> Result<Duration, McpError> {
        let start = Instant::now();
        self.request("ping", None).await?;
        Ok(start.elapsed())
    }

    /// `logging/setLevel` (`debug`, `info`, `notice`, `warning`, `error`, ...).
    pub async fn set_log_level(&self, level: &str) -> Result<(), McpError> {
        self.request("logging/setLevel", Some(json!({"level": level}))).await.map(|_| ())
    }

    async fn list_paged(&self, method: &str, key: &str, capability: &str) -> Result<Vec<Value>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        // A server handing out a cursor it already gave would loop forever.
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..MAX_PAGES {
            let params = cursor.as_ref().map(|c| json!({"cursor": c}));
            let result = match self.request(method, params).await {
                Ok(r) => r,
                // A server without the capability may not know the method.
                Err(McpError::Rpc { code: -32601, .. }) if !self.info.has_capability(capability) => return Ok(out),
                Err(e) => return Err(e),
            };
            if let Some(items) = result.get(key).and_then(Value::as_array) {
                out.extend(items.iter().cloned());
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() && seen.insert(next.to_string()) => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    /// `tools/list`, following pagination.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        Ok(self.list_paged("tools/list", "tools", "tools").await?.iter().map(Tool::from_value).collect())
    }

    /// `tools/call`. A tool-level failure is `Ok` with `is_error: true`.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<CallResult, McpError> {
        let arguments = match args {
            Value::Null => Value::Object(Map::new()),
            other => other,
        };
        let result = self.request("tools/call", Some(json!({"name": name, "arguments": arguments}))).await?;
        Ok(CallResult::from_value(result))
    }

    pub async fn list_resources(&self) -> Result<Vec<Resource>, McpError> {
        Ok(self
            .list_paged("resources/list", "resources", "resources")
            .await?
            .iter()
            .map(Resource::from_value)
            .collect())
    }

    pub async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        Ok(self
            .list_paged("resources/templates/list", "resourceTemplates", "resources")
            .await?
            .iter()
            .map(ResourceTemplate::from_value)
            .collect())
    }

    /// `resources/read`; returns the whole result (`{"contents": [...]}`).
    pub async fn read_resource(&self, uri: &str) -> Result<Value, McpError> {
        self.request("resources/read", Some(json!({"uri": uri}))).await
    }

    pub async fn list_prompts(&self) -> Result<Vec<Prompt>, McpError> {
        Ok(self.list_paged("prompts/list", "prompts", "prompts").await?.iter().map(Prompt::from_value).collect())
    }

    /// `prompts/get`; returns the whole result (`{"description", "messages"}`).
    /// Non-string argument values are stringified, as the protocol requires.
    pub async fn get_prompt(&self, name: &str, args: Map<String, Value>) -> Result<Value, McpError> {
        let arguments: Map<String, Value> = args
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    Value::String(s) => Value::String(s),
                    Value::Null => Value::String(String::new()),
                    other => Value::String(other.to_string()),
                };
                (k, v)
            })
            .collect();
        self.request("prompts/get", Some(json!({"name": name, "arguments": arguments}))).await
    }

    /// Ends the session: HTTP sends `DELETE` with the session id; stdio closes
    /// the server's stdin, then terminates the process if it does not exit;
    /// HTTP+SSE drops the event stream.
    pub async fn close(&self) {
        if self.is_closed() {
            return;
        }
        if let TransportHandle::Http(h) = &self.conn.transport {
            h.terminate().await;
        }
        self.conn.shared.mark_closed("Closed by client");
        if let TransportHandle::Stdio(s) = &self.conn.transport {
            s.wait_exit(Duration::from_secs(5)).await;
        }
    }
}

fn is_version_rejection(e: &McpError) -> bool {
    let mentions = |s: &str| s.to_ascii_lowercase().contains("version");
    match e {
        McpError::Rpc { message, data, .. } => {
            mentions(message) || data.as_ref().is_some_and(|d| mentions(&d.to_string()))
        }
        McpError::Http { status: 400, message } => mentions(message),
        _ => false,
    }
}

async fn initialize(conn: &Conn) -> Result<ServerInfo, McpError> {
    let shared = &conn.shared;
    let capabilities = shared.features.capabilities();
    for (i, version) in PROTOCOL_VERSIONS.iter().enumerate() {
        let params = json!({
            "protocolVersion": version,
            "capabilities": capabilities,
            "clientInfo": {"name": "merpati", "title": "Merpati", "version": env!("CARGO_PKG_VERSION")}
        });
        match shared.request("initialize", Some(params), conn.timeout).await {
            Ok(result) => {
                let info = ServerInfo::from_initialize(&result);
                if info.protocol_version.is_empty() {
                    return Err(McpError::Protocol("the initialize result has no protocolVersion".into()));
                }
                if !KNOWN_VERSIONS.contains(&info.protocol_version.as_str()) {
                    shared.warn(format!(
                        "Server chose protocol version {} which this client does not know; continuing anyway",
                        info.protocol_version
                    ));
                }
                match &conn.transport {
                    TransportHandle::Http(h) => h.set_protocol_version(&info.protocol_version),
                    TransportHandle::Sse(h) => h.set_protocol_version(&info.protocol_version),
                    TransportHandle::Stdio(_) => {}
                }
                shared.notify("notifications/initialized", None)?;
                return Ok(info);
            }
            Err(e) if i + 1 < PROTOCOL_VERSIONS.len() && is_version_rejection(&e) => {
                shared.warn(format!(
                    "Server rejected protocol version {version} ({e}); retrying with {}",
                    PROTOCOL_VERSIONS[i + 1]
                ));
                if let TransportHandle::Http(h) = &conn.transport {
                    h.reset_session();
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(McpError::Protocol("no common protocol version".into()))
}
