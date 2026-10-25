//! Minimal Socket.IO v4 server (Engine.IO 4 over WebSocket only) for trying
//! the client by hand: echoes every event back (acknowledging it when asked)
//! and emits a `tick` event every two seconds. Connect with the `Auto`
//! (falls back to WebSocket) or `WebSocket` transport.
//!
//! `cargo run -p merpati-socketio --example echo_server -- 3000`

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(3000);
    let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    println!("Socket.IO echo server on http://127.0.0.1:{port}");
    let mut n = 0u64;
    while let Ok((stream, _)) = listener.accept().await {
        n += 1;
        tokio::spawn(connection(stream, n));
    }
}

/// Splits a Socket.IO packet (without the Engine.IO `4`) into
/// (type, namespace, ack id, data).
fn parse(p: &str) -> Option<(u8, String, Option<u64>, Value)> {
    let kind = p.as_bytes().first()?.checked_sub(b'0')?;
    let mut rest = &p[1..];
    let mut ns = "/".to_string();
    if rest.starts_with('/') {
        let end = rest.find(',').unwrap_or(rest.len());
        ns = rest[..end].to_string();
        rest = &rest[(end + 1).min(rest.len())..];
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    let id = rest[..digits].parse().ok();
    rest = &rest[digits..];
    let data = if rest.is_empty() { Value::Null } else { serde_json::from_str(rest).ok()? };
    Some((kind, ns, id, data))
}

async fn connection(stream: tokio::net::TcpStream, n: u64) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
    let open = json!({"sid": format!("eio-{n}"), "upgrades": [], "pingInterval": 25000, "pingTimeout": 20000, "maxPayload": 1_000_000});
    if ws.send(Message::text(format!("0{open}"))).await.is_err() {
        return;
    }
    let mut namespaces: Vec<String> = Vec::new();
    let mut ping = tokio::time::interval(Duration::from_secs(25));
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut count = 0u64;
    loop {
        let mut out: Vec<String> = Vec::new();
        tokio::select! {
            _ = ping.tick() => out.push("2".into()),
            _ = tick.tick() => {
                count += 1;
                for ns in &namespaces {
                    let prefix = if ns == "/" { "42".to_string() } else { format!("42{ns},") };
                    out.push(format!("{prefix}{}", json!(["tick", {"count": count}])));
                }
            }
            msg = ws.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t.as_str().to_string(),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                };
                let Some((kind, ns, id, data)) = text.strip_prefix('4').and_then(parse) else { continue };
                let prefix = |k: u8| if ns == "/" { format!("4{k}") } else { format!("4{k}{ns},") };
                match kind {
                    0 => {
                        namespaces.push(ns.clone());
                        out.push(format!("{}{}", prefix(0), json!({"sid": format!("sock-{n}")})));
                    }
                    1 => namespaces.retain(|x| x != &ns),
                    2 => {
                        let items = data.as_array().cloned().unwrap_or_default();
                        out.push(format!("{}{}", prefix(2), Value::Array(items.clone())));
                        if let Some(id) = id {
                            out.push(format!("{}{id}{}", prefix(3), json!([{"received": items.len().saturating_sub(1)}])));
                        }
                    }
                    _ => {}
                }
            }
        }
        for m in out {
            if ws.send(Message::text(m)).await.is_err() {
                return;
            }
        }
    }
}
