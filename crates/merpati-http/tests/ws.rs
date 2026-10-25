#![allow(clippy::result_large_err)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use merpati_http::ws::{WsConfig, WsDirection, WsEvent, WsPayload, WsSession};

async fn echo_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let callback =
                    |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     mut resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        if let Some(v) = req.headers().get("x-token") {
                            resp.headers_mut().insert("x-echo-token", v.clone());
                        }
                        Ok(resp)
                    };
                let mut ws = tokio_tungstenite::accept_hdr_async(stream, callback).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if msg.is_close() {
                        break;
                    }
                    if msg.is_text() || msg.is_binary() {
                        ws.send(msg).await.unwrap();
                    }
                }
            });
        }
    });
    format!("ws://{addr}")
}

#[tokio::test]
async fn connect_send_receive_close() {
    let url = echo_server().await;
    let events: Arc<Mutex<Vec<WsEvent>>> = Arc::default();
    let ev = events.clone();
    let cfg = WsConfig { url, headers: vec![("X-Token".into(), "t1".into())], ..Default::default() };
    let session = WsSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| ev.lock().unwrap().push(e));
    session.send_text("hello");
    session.send_binary(vec![1u8, 2, 3]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.close(1000, "bye");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let events = events.lock().unwrap().clone();
    let connected = events.iter().find_map(|e| match e {
        WsEvent::Connected { status, headers, .. } => Some((*status, headers.clone())),
        _ => None,
    });
    let (status, headers) = connected.expect("connected");
    assert_eq!(status, 101);
    assert!(headers.iter().any(|(k, v)| k == "x-echo-token" && v == "t1"));
    let incoming: Vec<&WsPayload> = events
        .iter()
        .filter_map(|e| match e {
            WsEvent::Message { direction: WsDirection::Incoming, payload, .. } => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(incoming, vec![&WsPayload::Text("hello".into()), &WsPayload::Binary(vec![1u8, 2, 3].into())]);
    assert!(matches!(events.last(), Some(WsEvent::Closed { code: Some(1000), by_peer: false, .. })));
}

#[tokio::test]
async fn refused_connection_reports_error() {
    let events: Arc<Mutex<Vec<WsEvent>>> = Arc::default();
    let ev = events.clone();
    let cfg = WsConfig { url: "ws://127.0.0.1:9/".into(), ..Default::default() };
    let _s = WsSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| ev.lock().unwrap().push(e));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let events = events.lock().unwrap().clone();
    assert!(events.iter().any(|e| matches!(e, WsEvent::Error(m) if m.contains("refused"))), "{events:?}");
}
