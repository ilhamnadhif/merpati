//! Streamable HTTP transport (MCP 2025-03-26 and later): every client
//! message is POSTed; the server answers with JSON, an SSE stream or 202.
//! An optional GET stream carries server-initiated messages.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::Url;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::client::{Outbound, Shared};
use crate::config::is_sensitive_header;
use crate::error::McpError;
use crate::oauth::{AuthManager, wants_auth};
use crate::sse::SseParser;

const SESSION_HEADER: &str = "mcp-session-id";
pub(crate) const VERSION_HEADER: &str = "mcp-protocol-version";
/// Authorization attempts per message: a refresh, then a browser login.
pub(crate) const AUTH_ATTEMPTS: usize = 2;
/// Largest message accepted from a server (a JSON response body, an SSE
/// event, a stdio line); larger ones fail the request or the session.
pub(crate) const MAX_MESSAGE_BYTES: usize = crate::sse::MAX_EVENT_BYTES;
/// What is read of error and OAuth response bodies (the rest is ignored).
pub(crate) const MAX_SMALL_BODY: usize = 1024 * 1024;
/// How long a notification or response POST holds back the next message
/// (to keep their order) before the next one is sent anyway.
const ORDER_GRACE: Duration = Duration::from_secs(3);
/// Attempts to resume a broken response stream without receiving anything.
const MAX_RESUMES: usize = 5;
/// Upper bound for the reconnection time a server asks for with `retry:`.
const MAX_RETRY: Duration = Duration::from_secs(60);
/// Reconnection delay of the GET stream when the server sets none.
const LISTEN_RETRY: Duration = Duration::from_secs(2);

pub(crate) struct HttpConn {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    auth: Option<Arc<AuthManager>>,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    shared: Arc<Shared>,
    /// Limit for a notification or response POST (the per-request timeout).
    timeout: Duration,
}

/// The body, or `None` when it is longer than `max` bytes (reading stops there).
pub(crate) async fn body_within(mut resp: reqwest::Response, max: usize) -> reqwest::Result<Option<Vec<u8>>> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// At most the first `max` bytes of the body as text (the rest is not read);
/// what arrived before a network error.
pub(crate) async fn body_prefix(mut resp: reqwest::Response, max: usize) -> String {
    let mut body = Vec::new();
    while body.len() < max
        && let Ok(Some(chunk)) = resp.chunk().await
    {
        let room = max - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// How long an ordered POST holds back the next message: [`ORDER_GRACE`],
/// shorter with a short request timeout (so the next request can still
/// make it in time).
pub(crate) fn order_grace(timeout: Duration) -> Duration {
    ORDER_GRACE.min(timeout / 4)
}

/// Waits `retry` (the server's `retry:` in ms, capped at [`MAX_RETRY`]) or
/// `default`.
async fn reconnect_delay(retry: Option<u64>, default: Duration) {
    let delay = retry.map(|ms| Duration::from_millis(ms).min(MAX_RETRY)).unwrap_or(default);
    tokio::time::sleep(delay).await;
}

fn is_event_stream(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.to_ascii_lowercase().starts_with("text/event-stream"))
}

/// How reading an event stream ended.
enum StreamEnd {
    /// The request was answered (or there is none to wait for).
    Answered,
    /// The server closed the stream.
    Closed,
    /// The connection broke.
    Broken(McpError),
    /// An event over the size limit: the stream is abandoned.
    TooLarge(McpError),
}

pub(crate) fn build_client() -> Result<reqwest::Client, McpError> {
    merpati_http::tls::install_provider();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .tcp_nodelay(true)
        .build()
        .map_err(|e| McpError::Config(format!("Could not create the HTTP client: {e}")))
}

/// Validates a remote server URL (http or https).
pub(crate) fn parse_url(url: &str) -> Result<Url, McpError> {
    if url.trim().is_empty() {
        return Err(McpError::Config("No URL given for the MCP server".into()));
    }
    let parsed = Url::parse(url.trim()).map_err(|e| McpError::Config(format!("Invalid URL \"{url}\": {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(McpError::Config(format!(
            "Unsupported scheme \"{}:\" for a remote MCP server (use http or https)",
            parsed.scheme()
        )));
    }
    Ok(parsed)
}

/// The configured headers; secrets are marked sensitive.
pub(crate) fn header_map(headers: &[(String, String)]) -> Result<HeaderMap, McpError> {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.trim().as_bytes())
            .map_err(|_| McpError::Config(format!("Invalid header name \"{k}\"")))?;
        let mut value = HeaderValue::from_str(v.trim())
            .map_err(|_| McpError::Config(format!("Invalid value for header \"{k}\"")))?;
        if is_sensitive_header(k) {
            value.set_sensitive(true);
        }
        map.append(name, value);
    }
    Ok(map)
}

/// Adds the configured headers and the OAuth access token.
pub(crate) fn with_auth(
    mut rb: reqwest::RequestBuilder,
    headers: &HeaderMap,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    rb = rb.headers(headers.clone());
    if let Some(t) = token
        && let Ok(mut v) = HeaderValue::from_str(&format!("Bearer {t}"))
    {
        v.set_sensitive(true);
        rb = rb.header(AUTHORIZATION, v);
    }
    rb
}

/// All `WWW-Authenticate` challenges of a response, comma separated.
pub(crate) fn www_authenticate(resp: &reqwest::Response) -> Option<String> {
    let all: Vec<&str> = resp.headers().get_all(WWW_AUTHENTICATE).iter().filter_map(|v| v.to_str().ok()).collect();
    (!all.is_empty()).then(|| all.join(", "))
}

/// The text of an error body: a JSON-RPC or OAuth error message, else the
/// (shortened) body.
pub(crate) fn error_message(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body.trim()).ok();
    parsed
        .as_ref()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("error_description"))
                .or_else(|| v.get("message"))
                .or_else(|| v.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let t = body.trim();
            let text = if t.starts_with('<') { html_text(t) } else { t.to_string() };
            if text.chars().count() > 500 { format!("{}...", text.chars().take(500).collect::<String>()) } else { text }
        })
}

/// The visible text of an HTML error page (`<pre>` content when present,
/// e.g. Express' "Cannot POST /sse"), whitespace collapsed.
fn html_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let inner = match (lower.find("<pre>"), lower.find("</pre>")) {
        (Some(a), Some(b)) if b > a => &html[a + 5..b],
        _ => match (lower.find("<body"), lower.find("</body>")) {
            (Some(a), Some(b)) if b > a => &html[a..b],
            _ => html,
        },
    };
    let mut out = String::new();
    let mut in_tag = false;
    for c in inner.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Explains a 401/403 that could not be resolved.
pub(crate) fn unauthorized_message(mut message: String, www: Option<String>, oauth: bool) -> String {
    if message.is_empty() {
        message = "Unauthorized".into();
    }
    if oauth {
        message.push_str(". The server did not accept the OAuth access token");
    } else {
        message.push_str(
            ". The server requires authorization: set a Bearer token in the Auth tab, or enable OAuth sign-in",
        );
    }
    if let Some(w) = www {
        message.push_str(&format!(" (WWW-Authenticate: {w})"));
    }
    message
}

pub(crate) fn start(
    url: Url,
    headers: HeaderMap,
    client: reqwest::Client,
    auth: Option<Arc<AuthManager>>,
    shared: &Arc<Shared>,
    mut outbox: mpsc::UnboundedReceiver<Outbound>,
    timeout: Duration,
) -> Arc<HttpConn> {
    let conn = Arc::new(HttpConn {
        client,
        url,
        headers,
        auth,
        session_id: Mutex::new(None),
        protocol_version: Mutex::new(None),
        shared: shared.clone(),
        timeout,
    });
    let poster = conn.clone();
    let cancel = shared.cancel.clone();
    tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => break,
                m = outbox.recv() => match m { Some(m) => m, None => break },
            };
            let ordered = msg.request_id.is_none();
            let conn = poster.clone();
            // Requests may stream for a long time: they run concurrently.
            let mut post = tokio::spawn(async move { conn.post(msg).await });
            if ordered {
                // Notifications and responses are quick (202) and keep their
                // order, e.g. `notifications/initialized` before the next
                // request; a POST that hangs holds the others back for at
                // most `order_grace` and gives up after the request timeout.
                let _ = tokio::time::timeout(order_grace(timeout), &mut post).await;
            }
        }
    });
    conn
}

impl HttpConn {
    fn host(&self) -> String {
        self.url.host_str().unwrap_or("server").to_string()
    }

    fn net_err(&self, e: reqwest::Error) -> McpError {
        McpError::Transport(merpati_http::error::classify(&e, &self.host(), None).message)
    }

    pub fn set_protocol_version(&self, v: &str) {
        *self.protocol_version.lock() = Some(v.to_string());
    }

    pub fn reset_session(&self) {
        *self.session_id.lock() = None;
    }

    async fn token(&self) -> Option<String> {
        match &self.auth {
            Some(a) => a.bearer().await,
            None => None,
        }
    }

    fn with_session(&self, rb: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
        let mut rb = with_auth(rb, &self.headers, token);
        if let Some(sid) = self.session_id.lock().clone() {
            rb = rb.header(SESSION_HEADER, sid);
        }
        if let Some(v) = self.protocol_version.lock().clone() {
            rb = rb.header(VERSION_HEADER, v);
        }
        rb
    }

    fn capture_session(&self, resp: &reqwest::Response) {
        if let Some(sid) = resp.headers().get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
            let sid = sid.trim();
            if !sid.is_empty() {
                *self.session_id.lock() = Some(sid.to_string());
            }
        }
    }

    async fn post(self: Arc<Self>, msg: Outbound) {
        let cancel = self.shared.cancel.clone();
        let result = match &msg.request_id {
            // A request ends with its answer, its timeout or its cancellation
            // (the client stops waiting): then the POST is dropped too.
            Some(id) => tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.settled(id) => return,
                r = self.post_inner(&msg) => r,
            },
            None => tokio::select! {
                _ = cancel.cancelled() => return,
                r = tokio::time::timeout(self.timeout, self.post_inner(&msg)) => r.unwrap_or_else(|_| {
                    Err(McpError::Transport(format!(
                        "no answer from the server within {} s",
                        self.timeout.as_secs_f64().round()
                    )))
                }),
            },
        };
        if let Err(e) = result {
            match &msg.request_id {
                Some(id) => self.shared.fail(id, e),
                None => self.shared.warn(format!("Sending a message failed: {e}")),
            }
        }
    }

    /// Resolves once nobody waits for the request any more.
    async fn settled(&self, id: &str) {
        while self.shared.is_pending(id) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// POSTs the message, authorizing and retrying on 401.
    async fn send_authorized(&self, msg: &Outbound) -> Result<reqwest::Response, McpError> {
        let mut attempt = 0;
        loop {
            let token = self.token().await;
            let rb = self
                .with_session(self.client.post(self.url.clone()), token.as_deref())
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "application/json, text/event-stream")
                .body(msg.json.clone());
            let resp = rb.send().await.map_err(|e| self.net_err(e))?;
            let status = resp.status().as_u16();
            if let Some(auth) = &self.auth
                && attempt < AUTH_ATTEMPTS
                && wants_auth(status, www_authenticate(&resp).as_deref())
            {
                let www = www_authenticate(&resp);
                drop(resp);
                auth.on_unauthorized(status, www.as_deref(), token.as_deref(), attempt == 0).await?;
                attempt += 1;
                continue;
            }
            return Ok(resp);
        }
    }

    async fn post_inner(&self, msg: &Outbound) -> Result<(), McpError> {
        let resp = self.send_authorized(msg).await?;
        self.capture_session(&resp);
        let status = resp.status();
        if status.as_u16() == 202 || status.as_u16() == 204 {
            // Accepted: a notification or response, or the answer arrives on
            // the GET stream.
            return Ok(());
        }
        if !status.is_success() {
            return self.status_error(resp, msg).await;
        }
        let content_type =
            resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            return self.read_sse(resp, msg.request_id.as_deref()).await;
        }
        let body = body_within(resp, MAX_MESSAGE_BYTES).await.map_err(|e| self.net_err(e))?.ok_or_else(|| {
            McpError::Protocol(format!("the response is larger than {}", crate::sse::size_text(MAX_MESSAGE_BYTES)))
        })?;
        let body = String::from_utf8_lossy(&body);
        if body.trim().is_empty() {
            if msg.request_id.is_some() {
                return Err(McpError::Protocol(format!("empty response body (HTTP {})", status.as_u16())));
            }
            return Ok(());
        }
        self.shared.handle_incoming(&body);
        if let Some(id) = &msg.request_id
            && self.shared.is_pending(id)
        {
            let snippet: String = body.trim().chars().take(200).collect();
            return Err(McpError::Protocol(format!(
                "HTTP {} without a JSON-RPC response to this request ({content_type}): {snippet}",
                status.as_u16()
            )));
        }
        Ok(())
    }

    async fn status_error(&self, resp: reqwest::Response, msg: &Outbound) -> Result<(), McpError> {
        let status = resp.status().as_u16();
        let www = www_authenticate(&resp);
        let body = body_prefix(resp, MAX_SMALL_BODY).await;
        let parsed: Option<Value> = serde_json::from_str(body.trim()).ok();
        // A JSON-RPC error for this very request is more useful than the status.
        if let (Some(v), Some(id)) = (&parsed, &msg.request_id)
            && v.get("error").is_some()
            && v.get("id").map(|i| crate::client::id_key(i) == *id).unwrap_or(false)
        {
            self.shared.handle_incoming(body.trim());
            return Ok(());
        }
        let mut message = error_message(&body);
        if status == 404 && self.session_id.lock().is_some() {
            return Err(McpError::Closed("the server no longer knows this session (HTTP 404); reconnect".into()));
        }
        if status == 401 || status == 403 {
            message = unauthorized_message(message, www, self.auth.is_some());
        }
        if status == 405 && msg.request_id.is_some() {
            message
                .push_str(". The endpoint does not accept POST; is this a Streamable HTTP MCP endpoint (often /mcp)?");
        }
        Err(McpError::Http { status, message })
    }

    /// Reads the event stream answering a POST. When it ends or breaks before
    /// the request is answered and the server gave its events ids, the
    /// stream is resumed with a `GET` carrying `Last-Event-ID` (after the
    /// server's `retry:` delay), as the spec allows; servers may also close
    /// the stream on purpose and let the client poll this way.
    async fn read_sse(&self, resp: reqwest::Response, request_id: Option<&str>) -> Result<(), McpError> {
        let mut parser = SseParser::new();
        let mut end = self.read_events(resp, &mut parser, request_id).await;
        let mut last_id = parser.last_event_id().map(str::to_string);
        let mut retry = parser.retry_ms();
        let mut idle_resumes = 0;
        loop {
            let error = match end {
                StreamEnd::Answered => return Ok(()),
                StreamEnd::TooLarge(e) => return Err(e),
                StreamEnd::Closed => McpError::Protocol("the server ended the event stream without answering".into()),
                StreamEnd::Broken(e) => e,
            };
            let (Some(id), Some(last)) = (request_id, last_id.clone()) else { return Err(error) };
            if !self.shared.is_pending(id) {
                return Ok(());
            }
            if idle_resumes == MAX_RESUMES {
                return Err(error);
            }
            idle_resumes += 1;
            reconnect_delay(retry, Duration::from_millis(500)).await;
            let resp = match self.resume(&last).await {
                Ok(r) => r,
                Err(e) => {
                    return Err(McpError::Protocol(format!(
                        "{error}; resuming the stream with Last-Event-ID failed: {e}"
                    )));
                }
            };
            let mut parser = SseParser::new();
            end = self.read_events(resp, &mut parser, request_id).await;
            if let Some(id) = parser.last_event_id()
                && last_id.as_deref() != Some(id)
            {
                last_id = Some(id.to_string());
                idle_resumes = 0;
            }
            retry = parser.retry_ms().or(retry);
        }
    }

    /// Hands the messages of an event stream to the session until it ends or
    /// `request_id` is answered.
    async fn read_events(
        &self,
        resp: reqwest::Response,
        parser: &mut SseParser,
        request_id: Option<&str>,
    ) -> StreamEnd {
        let too_large = |e: crate::sse::SseTooLarge| StreamEnd::TooLarge(McpError::Protocol(e.to_string()));
        let answered = || request_id.is_some_and(|id| !self.shared.is_pending(id));
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => return StreamEnd::Broken(self.net_err(e)),
            };
            match parser.feed(&chunk) {
                Ok(events) => {
                    for ev in events.into_iter().filter(|ev| ev.event == "message") {
                        self.shared.handle_incoming(&ev.data);
                    }
                }
                Err(e) => return too_large(e),
            }
            if answered() {
                return StreamEnd::Answered;
            }
        }
        match parser.finish() {
            Ok(Some(ev)) if ev.event == "message" => self.shared.handle_incoming(&ev.data),
            Ok(_) => {}
            Err(e) => return too_large(e),
        }
        if request_id.is_none() || answered() { StreamEnd::Answered } else { StreamEnd::Closed }
    }

    /// `GET` resuming a broken stream after the event `last_id`.
    async fn resume(&self, last_id: &str) -> Result<reqwest::Response, McpError> {
        let token = self.token().await;
        let rb = self
            .with_session(self.client.get(self.url.clone()), token.as_deref())
            .header(ACCEPT, "text/event-stream")
            .header("last-event-id", last_id);
        let resp = rb.send().await.map_err(|e| self.net_err(e))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let message = error_message(&body_prefix(resp, MAX_SMALL_BODY).await);
            return Err(McpError::Http { status, message });
        }
        if !is_event_stream(&resp) {
            return Err(McpError::Protocol(format!("HTTP {status} without an event stream")));
        }
        Ok(resp)
    }

    /// Opens the optional GET stream for server-initiated messages. Servers
    /// that do not offer one answer 405, which ends the attempt quietly.
    pub fn start_listener(self: &Arc<Self>) {
        let conn = self.clone();
        tokio::spawn(async move {
            let cancel = conn.shared.cancel.clone();
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = conn.listen() => {}
            }
        });
    }

    async fn listen(&self) {
        let mut last_event_id: Option<String> = None;
        // The reconnection time the server set with `retry:` (ms).
        let mut retry: Option<u64> = None;
        let mut failures = 0;
        let mut auth_attempts = 0;
        while failures < 3 {
            let token = self.token().await;
            let mut rb = self
                .with_session(self.client.get(self.url.clone()), token.as_deref())
                .header(ACCEPT, "text/event-stream");
            if let Some(id) = &last_event_id {
                rb = rb.header("last-event-id", id.clone());
            }
            let started = Instant::now();
            let mut got_events = false;
            match rb.send().await {
                Ok(resp) if resp.status().is_success() && is_event_stream(&resp) => {
                    auth_attempts = 0;
                    let mut stream = resp.bytes_stream();
                    let mut parser = SseParser::new();
                    while let Some(Ok(chunk)) = stream.next().await {
                        let events = match parser.feed(&chunk) {
                            Ok(events) => events,
                            Err(e) => {
                                self.shared.warn(format!("Stopped listening for server messages: {e}"));
                                return;
                            }
                        };
                        for ev in events {
                            got_events = true;
                            if ev.event == "message" {
                                self.shared.handle_incoming(&ev.data);
                            }
                        }
                        if let Some(id) = parser.last_event_id() {
                            last_event_id = Some(id.to_string());
                        }
                        retry = parser.retry_ms().or(retry);
                    }
                }
                Ok(resp) if self.auth.is_some() && resp.status().as_u16() == 401 && auth_attempts < AUTH_ATTEMPTS => {
                    let www = www_authenticate(&resp);
                    drop(resp);
                    let first = auth_attempts == 0;
                    auth_attempts += 1;
                    if let Some(auth) = &self.auth
                        && auth.on_unauthorized(401, www.as_deref(), token.as_deref(), first).await.is_err()
                    {
                        return;
                    }
                    continue;
                }
                // 405 (no GET stream) or any other answer: stop listening.
                Ok(_) => return,
                Err(_) => {}
            }
            if got_events || started.elapsed() > Duration::from_secs(5) {
                failures = 0;
            } else {
                failures += 1;
            }
            reconnect_delay(retry, LISTEN_RETRY).await;
        }
    }

    /// Ends the session on the server (`DELETE` with the session id).
    pub async fn terminate(&self) {
        if self.session_id.lock().is_none() {
            return;
        }
        let token = self.auth.as_ref().and_then(|a| a.current_token());
        let rb = self.with_session(self.client.delete(self.url.clone()), token.as_deref());
        let _ = tokio::time::timeout(Duration::from_secs(5), rb.send()).await;
        *self.session_id.lock() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages() {
        let express = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<title>Error</title>\n</head>\n<body>\n<pre>Cannot POST /sse</pre>\n</body>\n</html>";
        assert_eq!(error_message(express), "Cannot POST /sse");
        assert_eq!(error_message("<html><body><h1>Not  Found</h1><p>nope</p></body></html>"), "Not Found nope");
        assert_eq!(error_message(r#"{"error": {"message": "bad"}}"#), "bad");
        assert_eq!(error_message(r#"{"error": "invalid_token", "error_description": "expired"}"#), "expired");
        assert_eq!(error_message(" plain "), "plain");
    }
}
