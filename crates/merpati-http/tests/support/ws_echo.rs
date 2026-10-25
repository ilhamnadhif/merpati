//! A WebSocket echo server and a client round trip, for the proxy tests.

#![allow(dead_code)]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use merpati_core::settings::ProxySettings;
use merpati_http::tls::TlsSetup;
use merpati_http::ws::{WsConfig, WsDirection, WsEvent, WsPayload, WsSession};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;

/// An echo server on 127.0.0.1 (behind TLS with `tls`); returns its port.
pub async fn echo_server(tls: Option<tokio_rustls::TlsAcceptor>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let tls = tls.clone();
            tokio::spawn(async move {
                match tls {
                    Some(acceptor) => {
                        if let Ok(s) = acceptor.accept(tcp).await {
                            echo(s).await;
                        }
                    }
                    None => echo(tcp).await,
                }
            });
        }
    });
    port
}

async fn echo<S: AsyncRead + AsyncWrite + Unpin>(stream: S) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
    while let Some(Ok(msg)) = ws.next().await {
        if msg.is_close() {
            break;
        }
        if (msg.is_text() || msg.is_binary()) && ws.send(msg).await.is_err() {
            break;
        }
    }
}

/// Connects, sends "ping" and waits for the echo; the error events joined
/// on failure.
pub async fn round_trip(url: String, proxy: ProxySettings, tls: TlsSetup) -> Result<(), String> {
    // Reading the OS trust store takes seconds in a fresh test process on a
    // busy machine; the timeouts below are about the connection.
    merpati_http::tls::load_native_roots().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cfg = WsConfig { url, proxy, tls, connect_timeout: Duration::from_secs(5), ..Default::default() };
    let session = WsSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| {
        let _ = tx.send(e);
    });
    session.send_text("ping");
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut errors = Vec::new();
        while let Some(e) = rx.recv().await {
            match e {
                WsEvent::Message { direction: WsDirection::Incoming, payload: WsPayload::Text(t), .. }
                    if t == "ping" =>
                {
                    return Ok(());
                }
                WsEvent::Error(m) => errors.push(m),
                WsEvent::Closed { .. } => break,
                _ => {}
            }
        }
        Err(errors.join("; "))
    })
    .await
    .unwrap_or_else(|_| Err("timed out".into()));
    session.close(1000, "");
    result
}

/// A port nothing listens on.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}
