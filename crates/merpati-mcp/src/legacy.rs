//! Legacy HTTP+SSE transport (protocol 2024-11-05): the client GETs an event
//! stream, the server announces a message endpoint in an `endpoint` event,
//! the client POSTs every JSON-RPC message there and reads all server
//! messages (responses included) from the stream.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use parking_lot::Mutex;
use reqwest::Url;
use reqwest::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, HeaderMap};
use tokio::sync::mpsc;

use crate::client::{Outbound, Shared};
use crate::error::McpError;
use crate::http::{
    AUTH_ATTEMPTS, MAX_MESSAGE_BYTES, MAX_SMALL_BODY, VERSION_HEADER, body_prefix, body_within, error_message,
    order_grace, unauthorized_message, with_auth, www_authenticate,
};
use crate::oauth::{AuthManager, wants_auth};
use crate::sse::{SseEvent, SseParser, size_text};

pub(crate) struct SseConn {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
    auth: Option<Arc<AuthManager>>,
    endpoint: Url,
    protocol_version: Mutex<Option<String>>,
    shared: Arc<Shared>,
}

impl SseConn {
    pub fn set_protocol_version(&self, v: &str) {
        *self.protocol_version.lock() = Some(v.to_string());
    }

    fn host(&self) -> String {
        self.url.host_str().unwrap_or("server").to_string()
    }

    async fn token(&self) -> Option<String> {
        match &self.auth {
            Some(a) => a.bearer().await,
            None => None,
        }
    }

    async fn post(&self, msg: &Outbound) -> Result<(), McpError> {
        let mut attempt = 0;
        let resp = loop {
            let token = self.token().await;
            let mut rb = with_auth(self.client.post(self.endpoint.clone()), &self.headers, token.as_deref())
                .header(CONTENT_TYPE, "application/json")
                .body(msg.json.clone());
            if let Some(v) = self.protocol_version.lock().clone() {
                rb = rb.header(VERSION_HEADER, v);
            }
            let resp = rb.send().await.map_err(|e| net_err(&e, &self.host()))?;
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
            break resp;
        };
        let status = resp.status().as_u16();
        let www = www_authenticate(&resp);
        if !(200..300).contains(&status) {
            let body = body_prefix(resp, MAX_SMALL_BODY).await;
            let mut message = error_message(&body);
            if status == 401 || status == 403 {
                message = unauthorized_message(message, www, self.auth.is_some());
            }
            if status == 404 {
                message.push_str(". The server no longer knows this SSE session; reconnect");
            }
            return Err(McpError::Http { status, message });
        }
        // Responses normally arrive on the stream ("Accepted"); a few servers
        // answer inline.
        let body =
            body_within(resp, MAX_MESSAGE_BYTES).await.map_err(|e| net_err(&e, &self.host()))?.ok_or_else(|| {
                McpError::Protocol(format!("the response is larger than {}", size_text(MAX_MESSAGE_BYTES)))
            })?;
        let body = String::from_utf8_lossy(&body);
        let t = body.trim();
        if t.starts_with('{') || t.starts_with('[') {
            self.shared.handle_incoming(t);
        }
        Ok(())
    }
}

fn net_err(e: &reqwest::Error, host: &str) -> McpError {
    McpError::Transport(merpati_http::error::classify(e, host, None).message)
}

/// Opens the event stream (authorizing on 401).
async fn open_stream(
    client: &reqwest::Client,
    url: &Url,
    headers: &HeaderMap,
    auth: Option<&Arc<AuthManager>>,
) -> Result<reqwest::Response, McpError> {
    let host = url.host_str().unwrap_or("server").to_string();
    let mut attempt = 0;
    loop {
        let token = match auth {
            Some(a) => a.bearer().await,
            None => None,
        };
        let rb = with_auth(client.get(url.clone()), headers, token.as_deref())
            .header(ACCEPT, "text/event-stream")
            .header(CACHE_CONTROL, "no-cache");
        let resp = rb.send().await.map_err(|e| net_err(&e, &host))?;
        let status = resp.status().as_u16();
        let www = www_authenticate(&resp);
        if let Some(a) = auth
            && attempt < AUTH_ATTEMPTS
            && wants_auth(status, www.as_deref())
        {
            drop(resp);
            a.on_unauthorized(status, www.as_deref(), token.as_deref(), attempt == 0).await?;
            attempt += 1;
            continue;
        }
        if !resp.status().is_success() {
            let body = body_prefix(resp, MAX_SMALL_BODY).await;
            let mut message = error_message(&body);
            if status == 401 || status == 403 {
                message = unauthorized_message(message, www, auth.is_some());
            }
            if message.is_empty() {
                message = "the SSE endpoint did not open an event stream".into();
            }
            return Err(McpError::Http { status, message });
        }
        let ct = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        if !ct.starts_with("text/event-stream") {
            return Err(McpError::Protocol(format!(
                "{url} is not an HTTP+SSE endpoint (GET answered with Content-Type \"{ct}\", expected text/event-stream)"
            )));
        }
        return Ok(resp);
    }
}

/// The `endpoint` event's URI, resolved against the stream URL. It must
/// stay on the same origin.
fn resolve_endpoint(base: &Url, data: &str) -> Result<Url, McpError> {
    let endpoint = base
        .join(data.trim())
        .map_err(|e| McpError::Protocol(format!("invalid endpoint \"{}\" in the event stream: {e}", data.trim())))?;
    if endpoint.origin() != base.origin() {
        return Err(McpError::Protocol(format!(
            "the event stream announced an endpoint on another origin ({}); refusing to send messages there",
            endpoint.origin().ascii_serialization()
        )));
    }
    Ok(endpoint)
}

/// Opens the stream, waits for the `endpoint` event, then starts the reader
/// and the poster.
pub(crate) async fn start(
    url: Url,
    headers: HeaderMap,
    client: reqwest::Client,
    auth: Option<Arc<AuthManager>>,
    shared: &Arc<Shared>,
    mut outbox: mpsc::UnboundedReceiver<Outbound>,
    timeout: Duration,
) -> Result<Arc<SseConn>, McpError> {
    let resp = open_stream(&client, &url, &headers, auth.as_ref()).await?;
    let host = url.host_str().unwrap_or("server").to_string();
    let mut stream = resp.bytes_stream();
    let mut parser = SseParser::new();
    let mut early: Vec<SseEvent> = Vec::new();
    let wait_endpoint = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| net_err(&e, &host))?;
            let mut events = parser.feed(&chunk).map_err(|e| McpError::Protocol(e.to_string()))?.into_iter();
            for ev in events.by_ref() {
                if ev.event == "endpoint" {
                    let endpoint = resolve_endpoint(&url, &ev.data)?;
                    early.extend(events);
                    return Ok(endpoint);
                }
                early.push(ev);
            }
        }
        Err(McpError::Protocol("the event stream ended before announcing the message endpoint".into()))
    };
    let endpoint = match tokio::time::timeout(timeout, wait_endpoint).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(McpError::Protocol(format!(
                "no \"endpoint\" event within {} s; is this an HTTP+SSE MCP endpoint?",
                timeout.as_secs()
            )));
        }
    };
    let conn = Arc::new(SseConn {
        client,
        url,
        headers,
        auth,
        endpoint,
        protocol_version: Mutex::new(None),
        shared: shared.clone(),
    });
    for ev in early {
        if ev.event == "message" {
            shared.handle_incoming(&ev.data);
        }
    }
    spawn_reader(shared.clone(), stream, parser);

    let poster = conn.clone();
    let cancel = shared.cancel.clone();
    tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => break,
                m = outbox.recv() => match m { Some(m) => m, None => break },
            };
            let (conn, cancel) = (poster.clone(), cancel.clone());
            let mut post = tokio::spawn(async move {
                let result = tokio::select! {
                    _ = cancel.cancelled() => return,
                    r = tokio::time::timeout(timeout, conn.post(&msg)) => r.unwrap_or_else(|_| {
                        Err(McpError::Transport(format!(
                            "no answer from the server within {} s",
                            timeout.as_secs_f64().round()
                        )))
                    }),
                };
                if let Err(e) = result {
                    match &msg.request_id {
                        Some(id) => conn.shared.fail(id, e),
                        None => conn.shared.warn(format!("Sending a message failed: {e}")),
                    }
                }
            });
            // POSTs are answered quickly ("Accepted"); sending them in order
            // keeps `notifications/initialized` after `initialize`. One that
            // hangs holds the next back for at most `order_grace` and gives up
            // after the request timeout.
            let _ = tokio::time::timeout(order_grace(timeout), &mut post).await;
        }
    });
    Ok(conn)
}

fn spawn_reader<S>(shared: Arc<Shared>, mut stream: S, mut parser: SseParser)
where
    S: Stream<Item = reqwest::Result<bytes::Bytes>> + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let cancel = shared.cancel.clone();
        let reason = loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return,
                c = stream.next() => c,
            };
            match chunk.map(|c| c.map(|bytes| parser.feed(&bytes))) {
                Some(Ok(Ok(events))) => {
                    for ev in events.into_iter().filter(|ev| ev.event == "message") {
                        shared.handle_incoming(&ev.data);
                    }
                }
                Some(Ok(Err(e))) => {
                    shared.mark_closed(format!("The event stream failed: {e}"));
                    return;
                }
                Some(Err(e)) => break format!("The event stream failed: {e}"),
                None => break "The server closed the event stream".to_string(),
            }
        };
        if let Ok(Some(ev)) = parser.finish()
            && ev.event == "message"
        {
            shared.handle_incoming(&ev.data);
        }
        shared.mark_closed(reason);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_resolution() {
        let base = Url::parse("http://h.test:3001/sse").unwrap();
        assert_eq!(
            resolve_endpoint(&base, "/message?sessionId=1").unwrap().as_str(),
            "http://h.test:3001/message?sessionId=1"
        );
        assert_eq!(resolve_endpoint(&base, "http://h.test:3001/m").unwrap().as_str(), "http://h.test:3001/m");
        assert!(resolve_endpoint(&base, "http://evil.test/m").is_err());
    }
}
