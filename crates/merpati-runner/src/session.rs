//! Interactive sessions in the pipeline: WebSocket, Socket.IO and MQTT
//! requests run in the Collection Runner and CLI as a scripted session.
//!
//! 1. The pre-request script runs; besides the usual fields `pm.request`
//!    has `messages` (the saved messages to send, in the shape of
//!    `protocol::ws_message` / `socketio_message` / `mqtt_message`) and
//!    `runner` (`{waitMs, maxMessages, untilEvent, timeoutMs}`); changes to
//!    both are applied.
//! 2. The session connects with the settings the app uses (variables,
//!    headers, auth, TLS verification, Socket.IO namespace/transport/
//!    version/auth payload, MQTT client id/credentials/keep-alive/clean
//!    session/last will).
//! 3. The saved messages are sent in order. MQTT subscribes to the enabled
//!    subscriptions first and waits for their SUBACKs; Socket.IO waits for
//!    the acknowledgement of each emit that requests one.
//! 4. Incoming messages run the on-message script until the request's
//!    [`SessionRunner`] condition, then the session disconnects cleanly.
//! 5. The post-response script gets a summary as `pm.response`:
//!    `messages` (incoming, as `pm.message`), `sent` (outgoing; Socket.IO
//!    emits carry `ackArgs` once acknowledged), `receivedCount`,
//!    `sentCount`, `stopReason` (`max_messages`, `until_event`, `silence`,
//!    `timeout`, `closed` or `error`), `error`, and timings in ms:
//!    `connectTime`, `firstMessageTime` (first send to first incoming
//!    message), `responseTime` (start to the last incoming message, without
//!    the final silence) and `duration` (start to stop). WebSocket adds
//!    `handshakeStatus`, `subprotocol`, `closeCode`, `closeReason`; Socket.IO
//!    `sid`, `namespace`; MQTT `clientId`, `sessionPresent` and
//!    `subscriptions` (`[{topic, qos, grantedQos}]`).
//!
//! Only a failed connection or an abnormal end (transport error, ping
//! timeout) fails the request; which stop condition ended the session is
//! left to the tests (`pm.response.stopReason`). Outgoing text that uses
//! `{{vault:...}}` secrets is shown in the console, the report and
//! `pm.response.sent` with those references unresolved.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use merpati_core::auth::Auth;
use merpati_core::model::{HttpRequest, WsMessageKind, WsRequest};
use merpati_core::prepared::{PrepareContext, prepare};
use merpati_core::protocols::{
    MqttMessage, MqttRequest, MqttSubscription, SessionRunner, SocketIoArg, SocketIoArgKind, SocketIoMessage,
    SocketIoRequest,
};
use merpati_core::script::protocol::{self as proto, Direction};
use merpati_core::script::{ProtocolContext, ScriptEvent};
use merpati_core::variables::{Resolver, VAULT_PREFIX, VariableScopes};
use merpati_http::{HttpError, HttpErrorKind};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::pipeline::{ConsoleKind, Execution};
use crate::protocol::{Ctx, clip, fail, ms, response, shown_request};

/// Messages kept per direction for `pm.response` and the report body.
const MAX_KEPT: usize = 10_000;
/// Message lines written to the console per session.
const MAX_LOGGED: usize = 200;
/// How long a clean disconnect may take.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// Event names the Socket.IO client API refuses to emit.
const RESERVED_EVENTS: [&str; 6] =
    ["connect", "connect_error", "disconnect", "disconnecting", "newListener", "removeListener"];

/// A session event, the same for every protocol.
enum Ev {
    Connected {
        status: u16,
        info: Vec<(&'static str, Value)>,
    },
    /// The oldest queued outgoing message was written to the network.
    Sent {
        at_ms: i64,
    },
    /// An incoming message as `pm.message`.
    Received(Value),
    Ack {
        id: u64,
        args: Vec<Value>,
    },
    Subscribed {
        topic: String,
        granted: Option<u8>,
    },
    Error(String),
    /// The session ended without us asking; `failed` marks an abnormal end.
    Closed {
        reason: String,
        code: Option<u16>,
        failed: bool,
    },
}

/// Why a session ended.
#[derive(Clone, Debug, PartialEq)]
enum Stop {
    MaxMessages,
    UntilEvent,
    Silence,
    Timeout,
    Closed(String),
    Error(String),
    Cancelled,
}

impl Stop {
    fn key(&self) -> &'static str {
        match self {
            Stop::MaxMessages => "max_messages",
            Stop::UntilEvent => "until_event",
            Stop::Silence => "silence",
            Stop::Timeout => "timeout",
            Stop::Closed(_) => "closed",
            Stop::Error(_) => "error",
            Stop::Cancelled => "cancelled",
        }
    }

    /// The connection is already gone.
    fn ended(&self) -> bool {
        matches!(self, Stop::Closed(_) | Stop::Error(_))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    WebSocket,
    SocketIo,
    Mqtt,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::WebSocket => "WebSocket",
            Kind::SocketIo => "Socket.IO",
            Kind::Mqtt => "MQTT",
        }
    }

    /// Whether an incoming message satisfies `until_event`.
    fn matches(self, until: &str, m: &Value) -> bool {
        match self {
            Kind::WebSocket => match &m["data"] {
                Value::String(s) => s == until,
                Value::Object(o) => ["type", "event"].iter().any(|k| o.get(*k).and_then(Value::as_str) == Some(until)),
                _ => false,
            },
            Kind::SocketIo => m["event"].as_str() == Some(until),
            Kind::Mqtt => m["topic"].as_str().is_some_and(|t| topic_matches(until, t)),
        }
    }

    /// One console line for a message.
    fn line(self, m: &Value) -> String {
        let text = match self {
            Kind::WebSocket => proto::payload_text(&m["data"]),
            Kind::SocketIo => format!("{} {}", m["event"].as_str().unwrap_or(""), m["args"]),
            Kind::Mqtt => format!("{} {}", m["topic"].as_str().unwrap_or(""), proto::payload_text(&m["data"])),
        };
        clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), 200)
    }
}

/// MQTT topic filter matching (`+` one level, `#` the rest).
fn topic_matches(filter: &str, topic: &str) -> bool {
    let (mut f, mut t) = (filter.split('/'), topic.split('/'));
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Sleeps until `at`, or forever without one.
async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Text of an MQTT payload: UTF-8, or a hex dump.
fn payload_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
    }
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

/// `pm.request.runner`.
fn runner_json(r: &SessionRunner) -> Value {
    json!({
        "waitMs": r.wait_ms,
        "maxMessages": r.max_messages,
        "untilEvent": r.until_event,
        "timeoutMs": r.timeout_ms,
        "ackReply": r.ack_reply,
    })
}

fn apply_runner(r: &mut SessionRunner, before: &Value, v: &Value) {
    let Some(new) = v.get("runner").filter(|n| *n != before) else { return };
    if let Some(n) = new.get("waitMs").and_then(Value::as_u64) {
        r.wait_ms = n;
    }
    if let Some(n) = new.get("maxMessages") {
        r.max_messages = n.as_u64().map(|n| n.min(u64::from(u32::MAX)) as u32);
    }
    if let Some(s) = new.get("untilEvent") {
        r.until_event = s.as_str().filter(|s| !s.trim().is_empty()).map(str::to_string);
    }
    if let Some(n) = new.get("timeoutMs").and_then(Value::as_u64) {
        r.timeout_ms = n;
    }
    if let Some(s) = new.get("ackReply") {
        r.ack_reply = match s {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        };
    }
}

/// Arguments of `SessionRunner::ack_reply`: a JSON array is the argument
/// list, any other JSON value or text a single argument.
fn ack_reply_args(text: &str) -> Vec<merpati_socketio::EmitArg> {
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(Value::Array(items)) => items.into_iter().map(merpati_socketio::EmitArg::Json).collect(),
        Ok(v) => vec![merpati_socketio::EmitArg::Json(v)],
        Err(_) => vec![merpati_socketio::EmitArg::Json(Value::String(text.to_string()))],
    }
}

/// The items of `pm.request.messages` that a pre-request script changed,
/// as `(index, item)`, or `None` when the list is unchanged.
fn changed_messages<'v>(before: &'v Value, v: &'v Value) -> Option<Vec<(usize, Option<&'v Value>, &'v Value)>> {
    let list = v.get("messages")?.as_array()?;
    if Some(v.get("messages")?) == before.get("messages") {
        return None;
    }
    let old = before.get("messages").and_then(Value::as_array);
    Some(list.iter().enumerate().map(|(i, m)| (i, old.and_then(|o| o.get(i)), m)).collect())
}

/// Request JSON for the session's scripts: the protocol fields plus
/// `messages` and `runner`.
fn with_extras(mut ctx: ProtocolContext, messages: Vec<Value>, runner: &SessionRunner) -> ProtocolContext {
    ctx.request["messages"] = Value::Array(messages);
    ctx.request["runner"] = runner_json(runner);
    ctx
}

/// The request's own auth, or the one inherited from folders/collection,
/// with the id of its owner (for cached OAuth 2.0 tokens).
pub(crate) fn effective_auth(ctx: &Ctx<'_>, own: &Auth) -> (Auth, String) {
    if !own.is_inherit() {
        return (own.clone(), ctx.item.id.clone());
    }
    match merpati_core::model::effective_from_chain(&ctx.chain) {
        Some(eff) => (eff.auth.clone(), eff.owner_id.to_string()),
        None => (Auth::none(), ctx.collection.id.clone()),
    }
}

/// Resolves variables but not `{{vault:...}}` secrets, for display.
fn plain_resolver(ctx: &Ctx<'_>, vars: &VariableScopes) -> impl Fn(&str) -> String + use<> {
    let scopes = ctx.scopes(vars);
    move |s: &str| Resolver::new(&scopes).resolve(s)
}

/// Whether any text uses a vault secret (directly or through a variable).
fn uses_vault(plain: &dyn Fn(&str) -> String, texts: &[&str]) -> bool {
    let marker = format!("{{{{{VAULT_PREFIX}");
    texts.iter().any(|t| t.contains("{{") && plain(t).contains(&marker))
}

/// Prepares a GET to `url` with `auth` like the HTTP pipeline (variables,
/// OAuth 2.0 tokens, API keys...) and returns what auth added: headers and
/// query parameters.
async fn auth_parts(
    ctx: &Ctx<'_>,
    auth: &Auth,
    owner: &str,
    url: &str,
    vars: &VariableScopes,
    exec: &mut Execution,
) -> Result<(Vec<(String, String)>, Vec<(String, String)>), String> {
    if matches!(auth.kind, merpati_core::auth::AuthType::None | merpati_core::auth::AuthType::Inherit) {
        return Ok((Vec::new(), Vec::new()));
    }
    let scopes = ctx.scopes(vars);
    let mut resolver = Resolver::new(&scopes);
    if let Some(v) = &ctx.services.vault {
        resolver = resolver.with_vault(v.as_ref());
    }
    let defaults = ctx.services.defaults.clone();
    let pctx = PrepareContext { resolver, auth, auth_owner_id: Some(owner), defaults: &defaults, base_dir: None };
    let mut prepared = prepare(&HttpRequest::new("GET", url.to_string()), &pctx);
    let before_headers = prepared.headers.clone();
    let before_query = query_pairs(&prepared.url);
    apply_auth(ctx, &mut prepared, exec).await?;
    let headers = prepared.headers.iter().filter(|h| !before_headers.contains(h)).cloned().collect();
    let query = query_pairs(&prepared.url).into_iter().filter(|q| !before_query.contains(q)).collect();
    Ok((headers, query))
}

fn query_pairs(url: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(url).map(|u| u.query_pairs().into_owned().collect()).unwrap_or_default()
}

/// `merpati_auth::apply` with the pipeline's token store; notes go to the console.
async fn apply_auth(
    ctx: &Ctx<'_>,
    prepared: &mut merpati_core::prepared::PreparedRequest,
    exec: &mut Execution,
) -> Result<(), String> {
    let actx = merpati_auth::AuthContext { tokens: ctx.services.tokens.as_ref(), http: ctx.services.http.as_ref() };
    // Fetching an OAuth 2.0 token can take long; Stop/Ctrl+C must not wait for it.
    let applied = tokio::select! {
        r = merpati_auth::apply(prepared, &actx) => r,
        _ = ctx.cancel.cancelled() => return Err("cancelled".into()),
    };
    match applied {
        Ok(applied) => {
            for n in &applied.notes {
                exec.network(ConsoleKind::Info, n.clone());
            }
            exec.auth_notes.extend(applied.notes);
            Ok(())
        }
        Err(e) => Err(format!("Auth error: {e}")),
    }
}

/// A URL for the console, without credentials.
fn shown_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        _ => url.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The session driver
// ---------------------------------------------------------------------------

struct Session<'c, 'a> {
    ctx: &'c Ctx<'a>,
    kind: Kind,
    rx: mpsc::UnboundedReceiver<Ev>,
    runner: SessionRunner,
    /// `until_event` with variables resolved.
    until: Option<String>,
    /// `pm.request` of on-message and post-response scripts.
    request: ProtocolContext,
    started: Instant,
    deadline: Option<tokio::time::Instant>,
    status: u16,
    info: Vec<(&'static str, Value)>,
    connected_ms: Option<f64>,
    /// Outgoing messages as shown, waiting for the transport to send them.
    outbox: VecDeque<Value>,
    /// Every message in order, for the report body.
    transcript: Vec<Value>,
    received: Vec<Value>,
    sent: Vec<Value>,
    received_count: usize,
    sent_count: usize,
    acks: HashMap<u64, Vec<Value>>,
    subscriptions: Vec<(String, Option<u8>)>,
    first_send: Option<Instant>,
    first_in: Option<Instant>,
    last_in: Option<Instant>,
    last_sent: Option<Instant>,
    /// Start of the current silence.
    quiet_since: tokio::time::Instant,
    /// The error reported right before the connection ended.
    last_error: Option<String>,
    close: Option<(Option<u16>, String)>,
    logged: usize,
}

impl<'c, 'a> Session<'c, 'a> {
    fn new(
        ctx: &'c Ctx<'a>,
        kind: Kind,
        rx: mpsc::UnboundedReceiver<Ev>,
        runner: &SessionRunner,
        request: ProtocolContext,
        vars: &VariableScopes,
    ) -> Self {
        let resolve = ctx.resolver(vars);
        let started = Instant::now();
        Session {
            ctx,
            kind,
            rx,
            runner: runner.clone(),
            until: runner.until().map(&resolve),
            request,
            started,
            deadline: (runner.timeout_ms > 0)
                .then(|| tokio::time::Instant::now() + Duration::from_millis(runner.timeout_ms)),
            status: 0,
            info: Vec::new(),
            connected_ms: None,
            outbox: VecDeque::new(),
            transcript: Vec::new(),
            received: Vec::new(),
            sent: Vec::new(),
            received_count: 0,
            sent_count: 0,
            acks: HashMap::new(),
            subscriptions: Vec::new(),
            first_send: None,
            first_in: None,
            last_in: None,
            last_sent: None,
            quiet_since: tokio::time::Instant::now(),
            last_error: None,
            close: None,
            logged: 0,
        }
    }

    fn label(&self) -> &'static str {
        self.kind.label()
    }

    fn log(&mut self, exec: &mut Execution, arrow: &str, m: &Value) {
        self.logged += 1;
        if self.logged <= MAX_LOGGED {
            exec.network(ConsoleKind::Network, format!("{arrow} {}", self.kind.line(m)));
        } else if self.logged == MAX_LOGGED + 1 {
            exec.network(ConsoleKind::Info, "… further messages are not logged".into());
        }
    }

    fn keep(&mut self, m: Value, incoming: bool) {
        if self.transcript.len() < 2 * MAX_KEPT {
            self.transcript.push(m.clone());
        }
        let list = if incoming { &mut self.received } else { &mut self.sent };
        if list.len() < MAX_KEPT {
            list.push(m);
        }
    }

    /// Queues the shown form of an outgoing message; `Ev::Sent` completes it.
    fn queue(&mut self, shown: Value) {
        self.outbox.push_back(shown);
    }

    fn record_sent(&mut self, at_ms: i64, exec: &mut Execution) {
        let Some(mut m) = self.outbox.pop_front() else { return };
        m["direction"] = json!(Direction::Outgoing);
        m["timestamp"] = json!(at_ms);
        self.sent_count += 1;
        let now = Instant::now();
        self.first_send.get_or_insert(now);
        self.last_sent = Some(now);
        self.log(exec, "↑", &m);
        self.keep(m, false);
    }

    /// Waits for one event and handles it; `Err` when the session must end.
    /// With `silence`, `wait_ms` without an incoming message ends it.
    async fn step(&mut self, vars: &mut VariableScopes, exec: &mut Execution, silence: bool) -> Result<(), Stop> {
        if self.ctx.cancel.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        if self.deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            return Err(Stop::Timeout);
        }
        let quiet_end =
            (silence && self.runner.wait_ms > 0).then(|| self.quiet_since + Duration::from_millis(self.runner.wait_ms));
        let ev = tokio::select! {
            biased;
            _ = self.ctx.cancel.cancelled() => return Err(Stop::Cancelled),
            ev = self.rx.recv() => ev,
            _ = sleep_until(self.deadline) => return Err(Stop::Timeout),
            _ = sleep_until(quiet_end) => return Err(Stop::Silence),
        };
        match ev {
            Some(ev) => self.handle(ev, vars, exec).await,
            None => Err(Stop::Closed("session ended".into())),
        }
    }

    async fn handle(&mut self, ev: Ev, vars: &mut VariableScopes, exec: &mut Execution) -> Result<(), Stop> {
        match ev {
            Ev::Connected { status, info } => {
                self.status = status;
                self.info = info;
                self.last_error = None;
                let t = ms(self.started);
                self.connected_ms = Some(t);
                exec.network(
                    ConsoleKind::Info,
                    format!("{} connected in {}", self.label(), merpati_core::util::human_ms(t)),
                );
            }
            Ev::Sent { at_ms } => {
                self.last_error = None;
                self.record_sent(at_ms, exec);
            }
            Ev::Received(m) => {
                self.last_error = None;
                self.received_count += 1;
                let now = Instant::now();
                self.first_in.get_or_insert(now);
                self.last_in = Some(now);
                self.log(exec, "↓", &m);
                let hit = self.until.as_deref().is_some_and(|u| self.kind.matches(u, &m));
                self.keep(m.clone(), true);
                self.ctx.scripts(ScriptEvent::OnMessage, self.request.clone().with_message(m), vars, exec).await;
                self.quiet_since = tokio::time::Instant::now();
                if hit {
                    return Err(Stop::UntilEvent);
                }
                if self.runner.max_messages.is_some_and(|n| n > 0 && self.received_count >= n as usize) {
                    return Err(Stop::MaxMessages);
                }
            }
            Ev::Ack { id, args } => {
                self.last_error = None;
                exec.network(
                    ConsoleKind::Network,
                    format!("↓ ack #{id} {}", clip(&Value::Array(args.clone()).to_string(), 200)),
                );
                self.acks.insert(id, args);
            }
            Ev::Subscribed { topic, granted } => {
                self.last_error = None;
                match granted {
                    Some(q) => exec.network(ConsoleKind::Info, format!("subscribed to {topic} (QoS {q})")),
                    None => exec.network(ConsoleKind::Warn, format!("subscription to {topic} was rejected")),
                }
                self.subscriptions.push((topic, granted));
            }
            Ev::Error(e) => {
                exec.network(ConsoleKind::Warn, format!("{}: {e}", self.label()));
                self.last_error = Some(e);
            }
            Ev::Closed { reason, code, failed } => {
                self.close = Some((code, reason.clone()));
                return Err(match self.last_error.take() {
                    Some(e) => Stop::Error(e),
                    None if failed => Stop::Error(reason),
                    None => Stop::Closed(reason),
                });
            }
        }
        Ok(())
    }

    /// Handles events until `done` holds.
    async fn wait_for(
        &mut self,
        vars: &mut VariableScopes,
        exec: &mut Execution,
        done: impl Fn(&Self) -> bool,
    ) -> Result<(), Stop> {
        while !done(self) {
            self.step(vars, exec, false).await?;
        }
        Ok(())
    }

    /// Collects incoming messages after the last send until a stop condition.
    async fn collect(&mut self, vars: &mut VariableScopes, exec: &mut Execution) -> Stop {
        if self.runner.max_messages.is_some_and(|n| self.received_count >= n as usize) {
            return Stop::MaxMessages;
        }
        self.quiet_since = tokio::time::Instant::now();
        loop {
            if let Err(stop) = self.step(vars, exec, true).await {
                return stop;
            }
        }
    }

    /// After asking the session to disconnect: records messages still being
    /// sent and waits (briefly) for the end.
    async fn drain(&mut self, exec: &mut Execution) {
        let until = tokio::time::Instant::now() + CLOSE_GRACE;
        loop {
            match tokio::time::timeout_at(until, self.rx.recv()).await {
                Ok(Some(Ev::Sent { at_ms })) => self.record_sent(at_ms, exec),
                Ok(Some(Ev::Closed { .. })) | Ok(None) | Err(_) => break,
                Ok(Some(_)) => {}
            }
        }
    }

    /// Reports a connection that did not come up.
    fn connect_failed(&self, stop: Stop, exec: &mut Execution) {
        let label = self.label();
        match stop {
            Stop::Cancelled => exec.error = Some(HttpError::cancelled()),
            Stop::Timeout => {
                fail(exec, format!("{label}: not connected within {}", human(self.runner.timeout_ms)));
            }
            Stop::Error(e) => fail(exec, format!("{label}: {e}")),
            Stop::Closed(reason) => fail(exec, format!("{label}: connection closed ({reason})")),
            other => fail(exec, format!("{label}: connection failed ({})", other.key())),
        }
    }

    fn describe(&self, stop: &Stop) -> String {
        match stop {
            Stop::MaxMessages => format!("stopped after {} messages", self.received_count),
            Stop::UntilEvent => format!("stopped at \"{}\"", self.until.as_deref().unwrap_or("")),
            Stop::Silence => format!("stopped after {} of silence", human(self.runner.wait_ms)),
            Stop::Timeout => format!("timed out after {}", human(self.runner.timeout_ms)),
            Stop::Closed(reason) => match &self.close {
                Some((Some(code), r)) if !r.is_empty() => format!("closed by the server ({code}: {r})"),
                Some((Some(code), _)) => format!("closed by the server ({code})"),
                _ if reason.is_empty() => "closed by the server".into(),
                _ => format!("closed by the server ({reason})"),
            },
            Stop::Error(e) => format!("failed: {e}"),
            Stop::Cancelled => "cancelled".into(),
        }
    }

    /// Builds the summary, runs the post-response scripts and fills
    /// `exec.response`; `extra` holds the protocol's own summary fields.
    async fn finish(
        mut self,
        stop: Stop,
        mut extra: Map<String, Value>,
        vars: &mut VariableScopes,
        exec: &mut Execution,
    ) {
        let label = self.label();
        let duration = ms(self.started);
        let since = |t: Option<Instant>| t.map(|t| t.duration_since(self.started).as_secs_f64() * 1000.0);
        let response_time = since(self.last_in).or(since(self.last_sent)).or(self.connected_ms).unwrap_or(duration);
        let first_message = match (self.first_in, self.first_send) {
            (Some(i), Some(s)) if i >= s => Some(i.duration_since(s).as_secs_f64() * 1000.0),
            (Some(i), _) => since(Some(i)).map(|t| t - self.connected_ms.unwrap_or(0.0)),
            _ => None,
        };
        for m in &mut self.sent {
            if let Some(args) = m.get("ackId").and_then(Value::as_u64).and_then(|id| self.acks.get(&id)) {
                m["ackArgs"] = Value::Array(args.clone());
            }
        }
        let error = match &stop {
            Stop::Error(e) => Some(e.clone()),
            _ => None,
        };
        let text = self.describe(&stop);
        exec.network(
            if error.is_some() { ConsoleKind::Error } else { ConsoleKind::Info },
            format!(
                "{label} session ended: {} sent, {} received, {text} ({})",
                self.sent_count,
                self.received_count,
                merpati_core::util::human_ms(duration)
            ),
        );
        // Point at an expectation the session ended without.
        if matches!(stop, Stop::Silence | Stop::Timeout | Stop::Closed(_)) {
            if let Some(u) = &self.until {
                exec.network(ConsoleKind::Warn, format!("\"{u}\" did not arrive"));
            } else if let Some(n) = self.runner.max_messages.filter(|n| self.received_count < *n as usize) {
                exec.network(ConsoleKind::Warn, format!("{} of {n} expected messages arrived", self.received_count));
            }
        }
        if self.received.len() < self.received_count || self.sent.len() < self.sent_count {
            exec.network(ConsoleKind::Info, format!("only the first {MAX_KEPT} messages per direction are kept"));
        }
        let mut summary = json!({
            "messages": self.received,
            "sent": self.sent,
            "receivedCount": self.received_count,
            "sentCount": self.sent_count,
            "stopReason": stop.key(),
            "error": error,
            "connectTime": self.connected_ms,
            "firstMessageTime": first_message,
            "responseTime": response_time,
            "duration": duration,
        });
        for (k, v) in &self.info {
            summary[*k] = v.clone();
        }
        if let Some(o) = summary.as_object_mut() {
            o.append(&mut extra);
        }
        let cancelled = stop == Stop::Cancelled;
        if !cancelled {
            let protocol = self.request.clone().with_response(summary);
            self.ctx.scripts(ScriptEvent::PostResponse, protocol, vars, exec).await;
        }
        let phrase = if self.status == 101 { "Switching Protocols" } else { "Connected" };
        let status_text = format!("{phrase} · {} sent, {} received, {text}", self.sent_count, self.received_count);
        let mut headers = vec![
            ("messages-sent".to_string(), self.sent_count.to_string()),
            ("messages-received".to_string(), self.received_count.to_string()),
            ("stop-reason".to_string(), stop.key().to_string()),
            ("session-duration".to_string(), merpati_core::util::human_ms(duration)),
        ];
        for (k, v) in &self.info {
            let v = match v {
                Value::String(s) => s.clone(),
                Value::Null => continue,
                other => other.to_string(),
            };
            headers.push((header_name(k), v));
        }
        let body = serde_json::to_string_pretty(&self.transcript).unwrap_or_default();
        exec.response = Some(response(self.status, status_text, headers, body, response_time));
        if cancelled {
            exec.error = Some(HttpError::cancelled());
        } else if let Some(e) = error {
            exec.error = Some(HttpError::new(HttpErrorKind::Other, format!("{label}: {e}")));
        }
    }
}

/// `sessionPresent` -> `session-present`.
fn header_name(k: &str) -> String {
    let mut out = String::new();
    for c in k.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn human(ms: u64) -> String {
    merpati_core::util::human_ms(ms as f64)
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

fn ws_context(r: &WsRequest) -> ProtocolContext {
    with_extras(proto::websocket(r), r.messages.iter().map(proto::ws_message).collect(), &r.runner)
}

fn apply_ws(r: &mut WsRequest, before: &Value, v: &Value) {
    let url = r.url.clone();
    proto::apply_websocket(r, v);
    if r.url != url {
        r.params = merpati_core::urlutil::merge_params_from_url(&r.url, &r.params);
    }
    apply_runner(&mut r.runner, &before["runner"], v);
    if let Some(items) = changed_messages(before, v) {
        r.messages = items
            .into_iter()
            .map(|(i, old, m)| {
                let mut saved = r.messages.get(i).cloned().unwrap_or_default();
                if old == Some(m) {
                    return saved;
                }
                let (data, kind) = match m {
                    Value::Object(o) => (o.get("data").cloned().unwrap_or(Value::Null), o.get("type")),
                    other => (other.clone(), None),
                };
                saved.body = proto::payload_text(&data);
                if let Some(k) = kind.and_then(Value::as_str) {
                    saved.kind = if k == "binary" { WsMessageKind::Binary } else { WsMessageKind::Text };
                }
                if saved.id.is_empty() {
                    saved.id = merpati_core::util::new_id();
                }
                saved
            })
            .collect();
    }
}

fn ws_event(e: merpati_http::ws::WsEvent) -> Option<Ev> {
    use merpati_http::ws::{WsDirection, WsEvent, WsPayload};
    Some(match e {
        WsEvent::Connecting => return None,
        WsEvent::Connected { status, protocol, .. } => {
            // Not `protocol`: `pm.response.protocol` is the protocol name.
            Ev::Connected { status, info: vec![("handshakeStatus", json!(status)), ("subprotocol", json!(protocol))] }
        }
        WsEvent::Message { direction: WsDirection::Outgoing, at_ms, .. } => Ev::Sent { at_ms },
        WsEvent::Message { direction: WsDirection::Incoming, payload, at_ms } => {
            let binary = matches!(payload, WsPayload::Binary(_));
            let text = match &payload {
                WsPayload::Text(t) => t.clone(),
                WsPayload::Binary(_) => payload.display(),
            };
            let mut m = proto::message(proto::json_or_text(&text), Direction::Incoming, at_ms);
            m["type"] = json!(if binary { "binary" } else { "text" });
            m["binary"] = json!(binary);
            Ev::Received(m)
        }
        WsEvent::Error(e) => Ev::Error(e),
        WsEvent::Closed { code, reason, .. } => Ev::Closed { reason, code, failed: false },
    })
}

/// Handshake settings like the app: query params, headers, auth (applied
/// like for HTTP), cookies from the jar, TLS settings of the engine.
async fn ws_config(
    ctx: &Ctx<'_>,
    req: &WsRequest,
    vars: &VariableScopes,
    exec: &mut Execution,
) -> Result<merpati_http::ws::WsConfig, String> {
    use reqwest::cookie::CookieStore;
    let (auth, owner) = effective_auth(ctx, &req.auth);
    let mut http = HttpRequest::new("GET", req.url.clone());
    if !req.params.is_empty() {
        http.params = req.params.clone();
        http.sync_url_from_params();
    }
    http.headers = req.headers.clone();
    let mut defaults = ctx.services.defaults.clone();
    defaults.send_user_agent = true;
    let scopes = ctx.scopes(vars);
    let mut resolver = Resolver::new(&scopes);
    if let Some(v) = &ctx.services.vault {
        resolver = resolver.with_vault(v.as_ref());
    }
    let pctx =
        PrepareContext { resolver, auth: &auth, auth_owner_id: Some(&owner), defaults: &defaults, base_dir: None };
    let mut prepared = prepare(&http, &pctx);
    for name in &prepared.unresolved {
        exec.network(ConsoleKind::Warn, format!("variable \"{name}\" is not defined"));
    }
    exec.unresolved = prepared.unresolved.clone();
    apply_auth(ctx, &mut prepared, exec).await?;
    let mut url = prepared.url.clone();
    if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{rest}");
    } else if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{rest}");
    }
    let mut headers: Vec<(String, String)> =
        prepared.headers.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("accept")).cloned().collect();
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("cookie")) {
        let http_url = url.replacen("ws://", "http://", 1).replacen("wss://", "https://", 1);
        if let Ok(u) = reqwest::Url::parse(&http_url)
            && let Some(c) = ctx.services.http.cookies().cookies(&u).and_then(|c| c.to_str().ok().map(str::to_string))
        {
            headers.push(("Cookie".into(), c));
        }
    }
    let verify_ssl = req.verify_ssl.unwrap_or(ctx.services.defaults.verify_ssl);
    let tls = ctx.tls_setup(&url, verify_ssl)?;
    Ok(merpati_http::ws::WsConfig {
        url,
        headers,
        subprotocols: req.subprotocols.clone(),
        verify_ssl,
        tls,
        proxy: ctx.proxy(),
        ..Default::default()
    })
}

pub(crate) async fn websocket(ctx: &Ctx<'_>, r: &WsRequest, vars: &mut VariableScopes, exec: &mut Execution) {
    let mut req = r.clone();
    let before = ws_context(&req);
    if let Some(v) = ctx.scripts(ScriptEvent::PreRequest, before.clone(), vars, exec).await {
        apply_ws(&mut req, &before.request, &v);
    }
    exec.request = Some(shown_request("WS", req.url.clone()));
    if exec.skipped || ctx.cancel.is_cancelled() {
        return;
    }
    let cfg = match ws_config(ctx, &req, vars, exec).await {
        Ok(c) => c,
        Err(_) if ctx.cancel.is_cancelled() => {
            exec.error = Some(HttpError::cancelled());
            return;
        }
        Err(e) => return fail(exec, format!("WebSocket: {e}")),
    };
    let via = ctx.via_proxy(&cfg.url, "ws");
    exec.network(ConsoleKind::Info, format!("WebSocket connect {}{via}", shown_url(&cfg.url)));
    let (tx, rx) = mpsc::unbounded_channel();
    let mut s = Session::new(ctx, Kind::WebSocket, rx, &req.runner, ws_context(&req), vars);
    let session = merpati_http::ws::WsSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| {
        if let Some(ev) = ws_event(e) {
            let _ = tx.send(ev);
        }
    });
    if let Err(stop) = s.wait_for(vars, exec, |s| s.connected_ms.is_some()).await {
        return s.connect_failed(stop, exec);
    }
    let mut queued = 0;
    for m in &req.messages {
        let resolve = ctx.resolver(vars);
        let plain = plain_resolver(ctx, vars);
        let text = resolve(&m.body);
        let shown = if uses_vault(&plain, &[&m.body]) { plain(&m.body) } else { text.clone() };
        let kind = match m.kind {
            WsMessageKind::Text => {
                session.send_text(text);
                "text"
            }
            WsMessageKind::Binary => match SocketIoArg::decode_binary(&text) {
                Some(bytes) => {
                    session.send_binary(bytes);
                    "binary"
                }
                None => {
                    let name = if m.name.is_empty() { &m.body } else { &m.name };
                    exec.network(
                        ConsoleKind::Warn,
                        format!("message \"{}\" skipped: binary messages must be hex or base64", clip(name, 40)),
                    );
                    continue;
                }
            },
        };
        s.queue(json!({"data": proto::json_or_text(&shown), "type": kind}));
        queued += 1;
    }
    let stop = match s.wait_for(vars, exec, |s| s.sent_count >= queued).await {
        Ok(()) => s.collect(vars, exec).await,
        Err(stop) => stop,
    };
    if !stop.ended() {
        session.close(1000, "");
        s.drain(exec).await;
    }
    drop(session);
    let (code, reason) = match &s.close {
        Some((code, reason)) if stop.ended() => (json!(code), json!(reason)),
        _ => (Value::Null, Value::Null),
    };
    let extra = Map::from_iter([("closeCode".to_string(), code), ("closeReason".to_string(), reason)]);
    s.finish(stop, extra, vars, exec).await;
}

// ---------------------------------------------------------------------------
// Socket.IO
// ---------------------------------------------------------------------------

fn sio_context(r: &SocketIoRequest) -> ProtocolContext {
    with_extras(proto::socketio(r), r.messages.iter().map(proto::socketio_message).collect(), &r.runner)
}

fn apply_sio(r: &mut SocketIoRequest, before: &Value, v: &Value) {
    proto::apply_socketio(r, v);
    apply_runner(&mut r.runner, &before["runner"], v);
    if let Some(items) = changed_messages(before, v) {
        r.messages = items
            .into_iter()
            .map(|(i, old, m)| {
                let mut saved: SocketIoMessage = r.messages.get(i).cloned().unwrap_or_default();
                if old == Some(m) {
                    return saved;
                }
                if let Some(e) = m.get("event").and_then(Value::as_str) {
                    saved.event = e.to_string();
                    if saved.name.is_empty() {
                        saved.name = e.to_string();
                    }
                }
                if let Some(a) = m.get("ack").and_then(Value::as_bool) {
                    saved.ack = a;
                }
                if let Some(args) = m.get("args").filter(|a| old.and_then(|o| o.get("args")) != Some(*a)) {
                    let items = match args {
                        Value::Array(items) => items.clone(),
                        other => vec![other.clone()],
                    };
                    saved.args = String::new();
                    saved.arguments =
                        items.iter().map(|a| SocketIoArg::new(SocketIoArgKind::Json, a.to_string())).collect();
                }
                saved
            })
            .collect();
    }
}

fn sio_event(e: merpati_socketio::SocketIoEvent) -> Option<Ev> {
    use merpati_socketio::{Direction as SioDirection, SocketIoEvent};
    Some(match e {
        SocketIoEvent::Connecting => return None,
        SocketIoEvent::Connected { sid, namespace } => {
            Ev::Connected { status: 200, info: vec![("sid", json!(sid)), ("namespace", json!(namespace))] }
        }
        SocketIoEvent::Event { direction: SioDirection::Outgoing, at_ms, .. } => Ev::Sent { at_ms },
        SocketIoEvent::Event { name, args, direction: SioDirection::Incoming, at_ms } => {
            // Like the app: `data` is the first argument.
            let data = args.first().cloned().unwrap_or(Value::Null);
            let mut m = proto::message(data, Direction::Incoming, at_ms);
            m["event"] = json!(name);
            m["args"] = json!(args);
            Ev::Received(m)
        }
        SocketIoEvent::Ack { id, args, .. } => Ev::Ack { id, args },
        SocketIoEvent::Error(e) => Ev::Error(e),
        SocketIoEvent::Disconnected { reason } => {
            let failed =
                matches!(reason.as_str(), "transport error" | "ping timeout" | "connect failed" | "connect error");
            Ev::Closed { reason, code: None, failed }
        }
    })
}

pub(crate) async fn socketio(ctx: &Ctx<'_>, r: &SocketIoRequest, vars: &mut VariableScopes, exec: &mut Execution) {
    let mut req = r.clone();
    let before = sio_context(&req);
    if let Some(v) = ctx.scripts(ScriptEvent::PreRequest, before.clone(), vars, exec).await {
        apply_sio(&mut req, &before.request, &v);
    }
    exec.request = Some(shown_request("SIO", req.url.clone()));
    if exec.skipped || ctx.cancel.is_cancelled() {
        return;
    }
    // Auth of every type is applied like for HTTP (inherited auth included);
    // `from_request` only knows Bearer, Basic and API keys.
    let (auth, owner) = effective_auth(ctx, &req.auth);
    let mut plain_req = req.clone();
    plain_req.auth = Auth::none();
    let resolve = ctx.resolver(vars);
    let mut cfg =
        merpati_socketio::SocketIoConfig::from_request(&plain_req, &resolve, ctx.services.defaults.verify_ssl);
    match auth_parts(ctx, &auth, &owner, &cfg.url, vars, exec).await {
        Ok((headers, query)) => {
            cfg.headers.extend(headers);
            cfg.query.extend(query);
        }
        Err(_) if ctx.cancel.is_cancelled() => {
            exec.error = Some(HttpError::cancelled());
            return;
        }
        Err(e) => return fail(exec, format!("Socket.IO: {e}")),
    }
    if cfg.url.contains("{{") {
        exec.network(ConsoleKind::Warn, format!("the URL has undefined variables: {}", cfg.url));
    }
    cfg.cookies = Some(merpati_socketio::SharedCookies::new(ctx.services.http.cookies().clone()));
    cfg.proxy = ctx.proxy();
    cfg.auto_ack =
        req.runner.ack_reply.as_deref().filter(|s| !s.trim().is_empty()).map(|s| ack_reply_args(&resolve(s)));
    match ctx.tls_setup(&cfg.url, cfg.verify_ssl) {
        Ok(tls) => cfg.tls = tls,
        Err(e) => return fail(exec, format!("Socket.IO: {e}")),
    }
    let via = ctx.via_proxy(&cfg.url, "http");
    exec.network(
        ConsoleKind::Info,
        format!("Socket.IO connect {} namespace {}{via}", shown_url(&cfg.url), cfg.namespace),
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let mut s = Session::new(ctx, Kind::SocketIo, rx, &req.runner, sio_context(&req), vars);
    let session = merpati_socketio::SocketIoSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| {
        if let Some(ev) = sio_event(e) {
            let _ = tx.send(ev);
        }
    });
    if let Err(stop) = s.wait_for(vars, exec, |s| s.connected_ms.is_some()).await {
        return s.connect_failed(stop, exec);
    }
    let mut queued = 0;
    let mut stop = None;
    for m in &req.messages {
        let resolve = ctx.resolver(vars);
        let plain = plain_resolver(ctx, vars);
        let event = resolve(&m.event).trim().to_string();
        if event.is_empty() || RESERVED_EVENTS.contains(&event.as_str()) {
            exec.network(ConsoleKind::Warn, format!("emit \"{event}\" skipped: not a valid event name"));
            continue;
        }
        let args = match merpati_socketio::message_args(m, &resolve, ctx.base_dir) {
            Ok(a) => a,
            Err(e) => {
                exec.network(ConsoleKind::Warn, format!("emit \"{event}\" skipped: {e}"));
                continue;
            }
        };
        let mut texts: Vec<&str> = vec![&m.event, &m.args];
        texts.extend(m.arguments.iter().map(|a| a.value.as_str()));
        let shown_args: Vec<Value> = if uses_vault(&plain, &texts) {
            match merpati_socketio::message_args(m, &plain, ctx.base_dir) {
                Ok(a) => a.iter().map(merpati_socketio::EmitArg::to_json).collect(),
                Err(_) => vec![json!("(hidden)")],
            }
        } else {
            args.iter().map(merpati_socketio::EmitArg::to_json).collect()
        };
        let id = session.emit_args(&event, args, m.ack);
        let mut shown = json!({
            "event": event,
            "args": shown_args,
            "data": shown_args.first().cloned().unwrap_or(Value::Null),
            "ack": m.ack,
        });
        if let Some(id) = id {
            shown["ackId"] = json!(id);
        }
        s.queue(shown);
        queued += 1;
        if let Some(id) = id
            && let Err(e) = s.wait_for(vars, exec, |s| s.acks.contains_key(&id)).await
        {
            if matches!(e, Stop::Timeout) {
                exec.network(ConsoleKind::Warn, format!("no acknowledgement for \"{event}\""));
            }
            stop = Some(e);
            break;
        }
    }
    let stop = match stop {
        Some(stop) => stop,
        None => match s.wait_for(vars, exec, |s| s.sent_count >= queued).await {
            Ok(()) => s.collect(vars, exec).await,
            Err(stop) => stop,
        },
    };
    if !stop.ended() {
        session.disconnect();
        s.drain(exec).await;
    }
    drop(session);
    s.finish(stop, Map::new(), vars, exec).await;
}

// ---------------------------------------------------------------------------
// MQTT
// ---------------------------------------------------------------------------

fn mqtt_context(r: &MqttRequest) -> ProtocolContext {
    with_extras(proto::mqtt(r), r.messages.iter().map(proto::mqtt_message).collect(), &r.runner)
}

fn apply_mqtt(r: &mut MqttRequest, before: &Value, v: &Value) {
    proto::apply_mqtt(r, v);
    apply_runner(&mut r.runner, &before["runner"], v);
    if let Some(list) = v.get("subscriptions").filter(|s| Some(*s) != before.get("subscriptions"))
        && let Some(items) = list.as_array()
    {
        r.subscriptions = items
            .iter()
            .filter_map(|s| match s {
                Value::String(t) => Some(MqttSubscription { topic: t.clone(), qos: 0, enabled: true }),
                Value::Object(o) => Some(MqttSubscription {
                    topic: o.get("topic")?.as_str()?.to_string(),
                    qos: o.get("qos").and_then(Value::as_u64).unwrap_or(0).min(2) as u8,
                    enabled: true,
                }),
                _ => None,
            })
            .collect();
    }
    if let Some(items) = changed_messages(before, v) {
        r.messages = items
            .into_iter()
            .map(|(i, old, m)| {
                let mut saved: MqttMessage = r.messages.get(i).cloned().unwrap_or_default();
                if old != Some(m) {
                    proto::apply_mqtt_message(&mut saved, m);
                }
                saved
            })
            .collect();
    }
}

fn mqtt_event(e: merpati_mqtt::MqttEvent) -> Option<Ev> {
    use merpati_mqtt::{Direction as MqttDirection, MqttEvent};
    Some(match e {
        MqttEvent::Connecting | MqttEvent::Unsubscribed { .. } => return None,
        MqttEvent::Connected { session_present } => {
            Ev::Connected { status: 200, info: vec![("sessionPresent", json!(session_present))] }
        }
        MqttEvent::Subscribed { topic, granted_qos } => Ev::Subscribed { topic, granted: granted_qos },
        MqttEvent::Message { direction: MqttDirection::Outgoing, at_ms, .. } => Ev::Sent { at_ms },
        MqttEvent::Message { topic, payload, qos, retain, direction: MqttDirection::Incoming, at_ms } => {
            let text = payload_text(&payload);
            let mut m = proto::message(proto::json_or_text(&text), Direction::Incoming, at_ms);
            m["topic"] = json!(topic);
            m["payload"] = json!(text);
            m["qos"] = json!(qos);
            m["retain"] = json!(retain);
            Ev::Received(m)
        }
        MqttEvent::Error(e) => Ev::Error(e),
        MqttEvent::Disconnected { reason } => Ev::Closed { reason, code: None, failed: false },
    })
}

pub(crate) async fn mqtt(ctx: &Ctx<'_>, r: &MqttRequest, vars: &mut VariableScopes, exec: &mut Execution) {
    let mut req = r.clone();
    let before = mqtt_context(&req);
    if let Some(v) = ctx.scripts(ScriptEvent::PreRequest, before.clone(), vars, exec).await {
        apply_mqtt(&mut req, &before.request, &v);
    }
    exec.request = Some(shown_request("MQTT", req.url.clone()));
    if exec.skipped || ctx.cancel.is_cancelled() {
        return;
    }
    // Credentials are Basic auth, possibly inherited.
    let mut conn_req = req.clone();
    conn_req.auth = effective_auth(ctx, &req.auth).0;
    let resolve = ctx.resolver(vars);
    let mut cfg = merpati_mqtt::MqttConfig::from_request(&conn_req, &resolve, ctx.services.defaults.verify_ssl);
    cfg.proxy = ctx.proxy();
    match ctx.tls_setup(&cfg.url, cfg.verify_ssl) {
        Ok(tls) => cfg.tls = tls,
        Err(e) => return fail(exec, format!("MQTT: {e}")),
    }
    if cfg.url.contains("{{") {
        exec.network(ConsoleKind::Warn, format!("the URL has undefined variables: {}", cfg.url));
    }
    let client_id = cfg.client_id.clone();
    let via = ctx.via_proxy(&cfg.url, "mqtt");
    exec.network(ConsoleKind::Info, format!("MQTT connect {} as {client_id}{via}", shown_url(&cfg.url)));
    let (tx, rx) = mpsc::unbounded_channel();
    let mut s = Session::new(ctx, Kind::Mqtt, rx, &req.runner, mqtt_context(&req), vars);
    let session = merpati_mqtt::MqttSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| {
        if let Some(ev) = mqtt_event(e) {
            let _ = tx.send(ev);
        }
    });
    if let Err(stop) = s.wait_for(vars, exec, |s| s.connected_ms.is_some()).await {
        return s.connect_failed(stop, exec);
    }
    // Subscribe first so that replies to our publishes are not missed.
    let mut subscribed = Vec::new();
    for sub in req.subscriptions.iter().filter(|s| s.enabled && !s.topic.trim().is_empty()) {
        let topic = resolve(&sub.topic).trim().to_string();
        session.subscribe(&topic, sub.qos.min(2));
        subscribed.push(json!({"topic": topic, "qos": sub.qos.min(2)}));
    }
    let wanted = subscribed.len();
    let stop = match s.wait_for(vars, exec, |s| s.subscriptions.len() >= wanted).await {
        Ok(()) => mqtt_publish(ctx, &req, &session, &mut s, vars, exec).await,
        Err(stop) => stop,
    };
    if !stop.ended() {
        session.disconnect();
        s.drain(exec).await;
    }
    drop(session);
    let granted: Vec<Value> = subscribed
        .into_iter()
        .map(|mut sub| {
            let topic = sub["topic"].as_str().unwrap_or("").to_string();
            sub["grantedQos"] = json!(s.subscriptions.iter().find(|(t, _)| *t == topic).and_then(|(_, q)| *q));
            sub
        })
        .collect();
    let extra = Map::from_iter([
        ("clientId".to_string(), json!(client_id)),
        ("subscriptions".to_string(), Value::Array(granted)),
    ]);
    s.finish(stop, extra, vars, exec).await;
}

/// Publishes the saved messages, then collects incoming ones.
async fn mqtt_publish(
    ctx: &Ctx<'_>,
    req: &MqttRequest,
    session: &merpati_mqtt::MqttSession,
    s: &mut Session<'_, '_>,
    vars: &mut VariableScopes,
    exec: &mut Execution,
) -> Stop {
    let mut queued = 0;
    for m in &req.messages {
        let resolve = ctx.resolver(vars);
        let plain = plain_resolver(ctx, vars);
        let topic = resolve(&m.topic).trim().to_string();
        if topic.is_empty() || topic.contains(['+', '#']) {
            exec.network(ConsoleKind::Warn, format!("publish to \"{topic}\" skipped: not a valid topic"));
            continue;
        }
        let payload = resolve(&m.payload);
        let shown_topic = if uses_vault(&plain, &[&m.topic]) { plain(&m.topic) } else { topic.clone() };
        let shown = if uses_vault(&plain, &[&m.payload]) { plain(&m.payload) } else { payload.clone() };
        let qos = m.qos.min(2);
        let publish = merpati_mqtt::Publish { topic, payload: payload.into_bytes(), qos, retain: m.retain };
        let user_properties: Vec<(String, String)> = m
            .user_properties
            .iter()
            .filter(|p| p.enabled && !p.key.trim().is_empty())
            .map(|p| (resolve(&p.key), resolve(&p.value)))
            .collect();
        if user_properties.is_empty() {
            session.publish(publish);
        } else {
            session.publish_with_properties(publish, merpati_mqtt::MessageProperties { user_properties });
        }
        s.queue(json!({
            "topic": shown_topic,
            "payload": shown,
            "data": proto::json_or_text(&shown),
            "qos": qos,
            "retain": m.retain,
        }));
        queued += 1;
    }
    match s.wait_for(vars, exec, |s| s.sent_count >= queued).await {
        Ok(()) => s.collect(vars, exec).await,
        Err(stop) => stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::model::WsSavedMessage;

    #[test]
    fn topic_filters() {
        assert!(topic_matches("a/+/c", "a/b/c"));
        assert!(topic_matches("a/#", "a/b/c"));
        assert!(topic_matches("#", "x"));
        assert!(topic_matches("a/b", "a/b"));
        assert!(!topic_matches("a/+", "a/b/c"));
        assert!(!topic_matches("a/b", "a/c"));
    }

    #[test]
    fn until_matching() {
        let ws = |data: Value| json!({"data": data});
        assert!(Kind::WebSocket.matches("done", &ws(json!("done"))));
        assert!(Kind::WebSocket.matches("done", &ws(json!({"type": "done"}))));
        assert!(Kind::WebSocket.matches("done", &ws(json!({"event": "done", "x": 1}))));
        assert!(!Kind::WebSocket.matches("done", &ws(json!("not done"))));
        assert!(Kind::SocketIo.matches("bye", &json!({"event": "bye"})));
        assert!(Kind::Mqtt.matches("status/#", &json!({"topic": "status/a/b"})));
    }

    #[test]
    fn runner_changes_from_scripts() {
        let mut r = SessionRunner::default();
        let before = runner_json(&r);
        let v = json!({"runner": {"waitMs": 0, "maxMessages": 3, "untilEvent": "done", "timeoutMs": 500}});
        apply_runner(&mut r, &before, &v);
        assert_eq!(
            r,
            SessionRunner {
                wait_ms: 0,
                max_messages: Some(3),
                until_event: Some("done".into()),
                timeout_ms: 500,
                ack_reply: None,
            }
        );
        apply_runner(&mut r, &before, &json!({"runner": {"maxMessages": null, "untilEvent": ""}}));
        assert_eq!((r.max_messages, r.until_event.as_deref()), (None, None));
    }

    #[test]
    fn header_names() {
        assert_eq!(header_name("sessionPresent"), "session-present");
        assert_eq!(header_name("sid"), "sid");
    }

    #[test]
    fn mqtt_message_changes() {
        let mut r = MqttRequest {
            messages: vec![MqttMessage { topic: "a".into(), payload: "1".into(), ..Default::default() }],
            ..Default::default()
        };
        let before = mqtt_context(&r).request;
        let mut v = before.clone();
        v["messages"][0]["payload"] = json!("2");
        v["messages"].as_array_mut().unwrap().push(json!({"topic": "b", "payload": {"x": 1}}));
        v["subscriptions"] = json!([{"topic": "a/#", "qos": 1}]);
        apply_mqtt(&mut r, &before, &v);
        assert_eq!(r.messages.len(), 2);
        assert_eq!((r.messages[0].topic.as_str(), r.messages[0].payload.as_str()), ("a", "2"));
        assert_eq!(r.messages[1].topic, "b");
        assert_eq!(r.subscriptions, vec![MqttSubscription { topic: "a/#".into(), qos: 1, enabled: true }]);
    }

    #[test]
    fn ws_message_changes() {
        let mut r = WsRequest {
            url: "ws://h/x?a=1".into(),
            messages: vec![WsSavedMessage { name: "m".into(), body: "hi".into(), ..Default::default() }],
            ..Default::default()
        };
        r.params = merpati_core::urlutil::merge_params_from_url(&r.url, &[]);
        let before = ws_context(&r).request;
        let mut v = before.clone();
        v["url"] = json!("ws://h/y?b=2");
        v["messages"][0]["data"] = json!({"n": 1});
        apply_ws(&mut r, &before, &v);
        assert_eq!(r.url, "ws://h/y?b=2");
        assert_eq!(r.params.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(), vec!["b"]);
        assert_eq!(r.messages[0].name, "m");
        assert_eq!(serde_json::from_str::<Value>(&r.messages[0].body).unwrap(), json!({"n": 1}));
    }
}
