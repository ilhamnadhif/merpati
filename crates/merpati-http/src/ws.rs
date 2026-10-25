//! WebSocket sessions (FR-X3): a persistent connection with custom headers,
//! text and binary messages, and a timestamped event stream for the UI log.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::proxy::{ProxyKind, ProxyResolver, ProxyStream};

/// A WebSocket connection, direct or through a proxy.
pub type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<ProxyStream>>;

#[derive(Clone, Debug)]
pub struct WsConfig {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub subprotocols: Vec<String>,
    pub verify_ssl: bool,
    pub connect_timeout: Duration,
    /// Extra trusted roots and the client certificate (certificate
    /// settings, see [`TlsSetup::for_url`](crate::tls::TlsSetup::for_url)).
    pub tls: crate::tls::TlsSetup,
    /// Proxy settings, applied like the HTTP engine does (see
    /// [`crate::proxy`]); the default connects directly.
    pub proxy: merpati_core::settings::ProxySettings,
}

impl Default for WsConfig {
    fn default() -> Self {
        WsConfig {
            url: String::new(),
            headers: Vec::new(),
            subprotocols: Vec::new(),
            verify_ssl: true,
            connect_timeout: Duration::from_secs(20),
            tls: crate::tls::TlsSetup::default(),
            proxy: merpati_core::settings::ProxySettings::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsDirection {
    Incoming,
    Outgoing,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WsPayload {
    Text(String),
    Binary(Bytes),
}

impl WsPayload {
    pub fn len(&self) -> usize {
        match self {
            WsPayload::Text(t) => t.len(),
            WsPayload::Binary(b) => b.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Text, or a hex dump for binary payloads.
    pub fn display(&self) -> String {
        match self {
            WsPayload::Text(t) => t.clone(),
            WsPayload::Binary(b) => b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" "),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WsEvent {
    Connecting,
    Connected { status: u16, headers: Vec<(String, String)>, protocol: Option<String>, elapsed_ms: f64 },
    Message { direction: WsDirection, payload: WsPayload, at_ms: i64 },
    Error(String),
    Closed { code: Option<u16>, reason: String, by_peer: bool },
}

enum Command {
    Send(WsPayload),
    Close(u16, String),
}

/// Handle to a running WebSocket session. Dropping it closes the connection.
pub struct WsSession {
    tx: mpsc::UnboundedSender<Command>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl WsSession {
    /// Connects in the background on `rt`; every state change and message is
    /// reported through `on_event` (called from a runtime thread).
    pub fn connect(
        rt: &tokio::runtime::Handle,
        config: WsConfig,
        on_event: impl Fn(WsEvent) + Send + Sync + 'static,
    ) -> WsSession {
        let (tx, rx) = mpsc::unbounded_channel();
        let on_event: Arc<dyn Fn(WsEvent) + Send + Sync> = Arc::new(on_event);
        rt.spawn(run(config, rx, on_event));
        WsSession { tx }
    }

    pub fn send_text(&self, text: impl Into<String>) -> bool {
        self.tx.send(Command::Send(WsPayload::Text(text.into()))).is_ok()
    }

    pub fn send_binary(&self, data: impl Into<Bytes>) -> bool {
        self.tx.send(Command::Send(WsPayload::Binary(data.into()))).is_ok()
    }

    pub fn close(&self, code: u16, reason: impl Into<String>) {
        let _ = self.tx.send(Command::Close(code, reason.into()));
    }

    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }
}

impl Drop for WsSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Close(1000, String::new()));
    }
}

async fn run(config: WsConfig, mut rx: mpsc::UnboundedReceiver<Command>, on_event: Arc<dyn Fn(WsEvent) + Send + Sync>) {
    on_event(WsEvent::Connecting);
    let start = Instant::now();
    let mut request = match config.url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            on_event(WsEvent::Error(format!("Invalid WebSocket URL \"{}\": {e}", config.url)));
            on_event(WsEvent::Closed { code: None, reason: String::new(), by_peer: false });
            return;
        }
    };
    for (k, v) in &config.headers {
        match (http::HeaderName::from_bytes(k.as_bytes()), http::HeaderValue::from_str(v)) {
            (Ok(name), Ok(value)) => {
                request.headers_mut().append(name, value);
            }
            _ => {
                on_event(WsEvent::Error(format!("Invalid header \"{k}\"")));
                on_event(WsEvent::Closed { code: None, reason: String::new(), by_peer: false });
                return;
            }
        }
    }
    if !config.subprotocols.is_empty()
        && let Ok(v) = http::HeaderValue::from_str(&config.subprotocols.join(", "))
    {
        request.headers_mut().insert("Sec-WebSocket-Protocol", v);
    }
    let wss = config.url.starts_with("wss://");
    let https_proxy =
        || crate::proxy::proxy_for_url(&config.proxy, &config.url, "ws").is_some_and(|p| p.kind() == ProxyKind::Https);
    if config.verify_ssl && (wss || https_proxy()) {
        crate::tls::load_native_roots().await;
    }
    let connector = if wss {
        match config.tls.client_config(config.verify_ssl) {
            Ok(c) => Some(tokio_tungstenite::Connector::Rustls(Arc::new(c))),
            Err(e) => {
                on_event(WsEvent::Error(format!("TLS setup failed: {e}")));
                on_event(WsEvent::Closed { code: None, reason: String::new(), by_peer: false });
                return;
            }
        }
    } else {
        Some(tokio_tungstenite::Connector::Plain)
    };
    let connect = connect_via_proxy(request, &config.proxy, &config.tls, config.verify_ssl, connector);
    let connect = tokio::time::timeout(config.connect_timeout, connect);
    tokio::pin!(connect);
    // Messages sent while the handshake is in flight are queued, not lost.
    let mut pending = Vec::new();
    let (stream, response) = loop {
        tokio::select! {
            r = &mut connect => match r {
                Ok(Ok(ok)) => break ok,
                Ok(Err(e)) => {
                    on_event(WsEvent::Error(describe(&e)));
                    on_event(WsEvent::Closed { code: None, reason: String::new(), by_peer: false });
                    return;
                }
                Err(_) => {
                    on_event(WsEvent::Error(format!("Connection timed out after {} s", config.connect_timeout.as_secs())));
                    on_event(WsEvent::Closed { code: None, reason: String::new(), by_peer: false });
                    return;
                }
            },
            cmd = rx.recv() => match cmd {
                Some(Command::Send(p)) => pending.push(p),
                Some(Command::Close(..)) | None => {
                    on_event(WsEvent::Closed { code: None, reason: "cancelled".into(), by_peer: false });
                    return;
                }
            }
        }
    };
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let protocol = response.headers().get("sec-websocket-protocol").and_then(|v| v.to_str().ok()).map(str::to_string);
    on_event(WsEvent::Connected {
        status: response.status().as_u16(),
        headers,
        protocol,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
    });

    let (mut sink, mut source) = stream.split();
    for payload in pending {
        let msg = match &payload {
            WsPayload::Text(t) => Message::text(t.clone()),
            WsPayload::Binary(b) => Message::binary(b.clone()),
        };
        match sink.send(msg).await {
            Ok(()) => on_event(WsEvent::Message { direction: WsDirection::Outgoing, payload, at_ms: now_ms() }),
            Err(e) => on_event(WsEvent::Error(format!("Send failed: {}", describe(&e)))),
        }
    }
    loop {
        tokio::select! {
            msg = source.next() => match msg {
                Some(Ok(Message::Text(t))) => on_event(WsEvent::Message {
                    direction: WsDirection::Incoming,
                    payload: WsPayload::Text(t.as_str().to_string()),
                    at_ms: now_ms(),
                }),
                Some(Ok(Message::Binary(b))) => on_event(WsEvent::Message {
                    direction: WsDirection::Incoming,
                    payload: WsPayload::Binary(b),
                    at_ms: now_ms(),
                }),
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = frame.map(|f| (Some(u16::from(f.code)), f.reason.as_str().to_string())).unwrap_or((None, String::new()));
                    on_event(WsEvent::Closed { code, reason, by_peer: true });
                    return;
                }
                Some(Err(e)) => {
                    on_event(WsEvent::Error(describe(&e)));
                    on_event(WsEvent::Closed { code: None, reason: String::new(), by_peer: true });
                    return;
                }
                None => {
                    on_event(WsEvent::Closed { code: None, reason: String::new(), by_peer: true });
                    return;
                }
            },
            cmd = rx.recv() => match cmd {
                Some(Command::Send(payload)) => {
                    let msg = match &payload {
                        WsPayload::Text(t) => Message::text(t.clone()),
                        WsPayload::Binary(b) => Message::binary(b.clone()),
                    };
                    match sink.send(msg).await {
                        Ok(()) => on_event(WsEvent::Message { direction: WsDirection::Outgoing, payload, at_ms: now_ms() }),
                        Err(e) => on_event(WsEvent::Error(format!("Send failed: {}", describe(&e)))),
                    }
                }
                Some(Command::Close(code, reason)) => {
                    let frame = CloseFrame { code: CloseCode::from(code), reason: reason.clone().into() };
                    let _ = sink.send(Message::Close(Some(frame))).await;
                    let _ = tokio::time::timeout(Duration::from_secs(2), async {
                        while let Some(Ok(m)) = source.next().await {
                            if matches!(m, Message::Close(_)) {
                                break;
                            }
                        }
                    })
                    .await;
                    on_event(WsEvent::Closed { code: Some(code), reason, by_peer: false });
                    return;
                }
                None => {
                    let _ = sink.send(Message::Close(None)).await;
                    on_event(WsEvent::Closed { code: Some(1000), reason: String::new(), by_peer: false });
                    return;
                }
            }
        }
    }
}

/// Opens the connection for a `ws://` / `wss://` request, through the
/// proxy that `proxy` selects for its host (see [`crate::proxy`]), and
/// performs the TLS (inside the tunnel) and WebSocket handshakes. `tls` and
/// `verify` only matter for an HTTPS proxy: TLS to the server is set up by
/// `connector`. Proxy failures come back as `Error::Io` holding a
/// [`ProxyError`](crate::proxy::ProxyError).
pub async fn connect_via_proxy(
    request: tungstenite::handshake::client::Request,
    proxy: &merpati_core::settings::ProxySettings,
    tls: &crate::tls::TlsSetup,
    verify: bool,
    connector: Option<tokio_tungstenite::Connector>,
) -> Result<(WsStream, tungstenite::handshake::client::Response), tungstenite::Error> {
    let uri = request.uri();
    let secure = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") => false,
        _ => return Err(tungstenite::Error::Url(tungstenite::error::UrlError::UnsupportedUrlScheme)),
    };
    let host = uri.host().ok_or(tungstenite::Error::Url(tungstenite::error::UrlError::NoHostName))?.to_string();
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let via = ProxyResolver::new(proxy).map_err(|e| tungstenite::Error::Io(e.into()))?.proxy_for(&host, port, secure);
    let stream = crate::proxy::dial(via.as_ref(), &host, port, tls, verify).await.map_err(tungstenite::Error::Io)?;
    tokio_tungstenite::client_async_tls_with_config(request, stream, None, connector).await
}

fn describe(e: &tungstenite::Error) -> String {
    if let tungstenite::Error::Io(io) = e
        && let Some(p) = crate::proxy::find_proxy_error(io)
    {
        return p.message.clone();
    }
    match e {
        tungstenite::Error::Http(resp) => {
            let body = resp.body().as_ref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
            format!(
                "Handshake rejected with HTTP {}{}",
                resp.status(),
                if body.is_empty() { String::new() } else { format!(": {body}") }
            )
        }
        tungstenite::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => {
            "Connection refused: is the server running and the port correct?".into()
        }
        tungstenite::Error::Tls(t) => format!("TLS error: {t}"),
        tungstenite::Error::Url(u) => format!("Invalid URL: {u}"),
        other => other.to_string(),
    }
}
