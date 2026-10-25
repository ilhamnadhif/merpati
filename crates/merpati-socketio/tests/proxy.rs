//! Socket.IO sessions through HTTP and SOCKS5 proxies: the polling requests
//! (reqwest, configured like the HTTP engine) and the WebSocket transport
//! (tunnel of `merpati_http::proxy`) alike.

mod common;
#[path = "../../merpati-http/tests/support/proxies.rs"]
mod proxies;

use std::time::Duration;

use common::Options;
use merpati_socketio::{
    ActiveTransport, Direction, SocketIoConfig, SocketIoEvent, SocketIoSession, SocketIoTransport, SocketIoVersion,
};
use proxies::{custom, http_proxy, socks5_proxy};
use serde_json::json;
use tokio::sync::mpsc;

fn config(url: String, transport: SocketIoTransport, proxy: merpati_core::settings::ProxySettings) -> SocketIoConfig {
    SocketIoConfig {
        url,
        transport,
        version: SocketIoVersion::V4,
        connect_timeout: Duration::from_secs(5),
        proxy,
        ..Default::default()
    }
}

/// Connects, emits `echo` and waits for the answer; with `upgrade`, also
/// waits until the session runs over WebSocket. The error events joined on
/// failure.
async fn echo(cfg: SocketIoConfig, upgrade: bool) -> Result<(), String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let session = SocketIoSession::connect(&tokio::runtime::Handle::current(), cfg, move |e| {
        let _ = tx.send(e);
    });
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut errors = Vec::new();
        while let Some(e) = rx.recv().await {
            match e {
                SocketIoEvent::Connected { .. } => {
                    session.emit("echo", vec![json!("hi")], false);
                }
                SocketIoEvent::Event { name, direction: Direction::Incoming, args, .. } if name == "echo" => {
                    assert_eq!(args, vec![json!("hi")]);
                    while upgrade && session.transport() != Some(ActiveTransport::WebSocket) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    return Ok(());
                }
                SocketIoEvent::Error(m) => errors.push(m),
                SocketIoEvent::Disconnected { .. } => break,
                _ => {}
            }
        }
        Err(errors.join("; "))
    })
    .await
    .unwrap_or_else(|_| Err("timed out".into()));
    session.disconnect();
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_and_websocket_through_http_proxy() {
    let server = common::start(Options::default()).await;
    let port = server.port;
    let auth = Some(("alice", "s3cret"));
    for transport in [SocketIoTransport::Polling, SocketIoTransport::WebSocket, SocketIoTransport::Auto] {
        let proxy = http_proxy(auth).await;
        let upgrade = transport == SocketIoTransport::Auto;
        echo(config(server.url(), transport, custom(&proxy.url(), auth)), upgrade).await.unwrap();
        let log = proxy.log();
        // Polling requests are forwarded (absolute form), WebSocket tunnelled.
        let polled = log.iter().any(|l| l == &format!("GET 127.0.0.1:{port}"));
        let tunnelled = log.iter().filter(|l| *l == &format!("CONNECT 127.0.0.1:{port}")).count();
        assert!(log.iter().all(|l| l.ends_with(&format!(" 127.0.0.1:{port}")) && !l.starts_with("DENIED")), "{log:?}");
        match transport {
            SocketIoTransport::Polling => assert!(polled && tunnelled == 0, "{log:?}"),
            SocketIoTransport::WebSocket => assert_eq!(log, vec![format!("CONNECT 127.0.0.1:{port}")]),
            SocketIoTransport::Auto => assert!(polled && tunnelled == 1, "{log:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_and_websocket_through_socks5() {
    let server = common::start(Options::default()).await;
    let auth = Some(("bob", "pa55"));
    for transport in [SocketIoTransport::Polling, SocketIoTransport::WebSocket] {
        let proxy = socks5_proxy(auth).await;
        let url = server.url().replace("127.0.0.1", "localhost");
        echo(config(url, transport, custom(&proxy.url(), auth)), false).await.unwrap();
        let log = proxy.log();
        assert!(!log.is_empty(), "{transport:?}");
        // socks5h: the proxy resolves the name.
        assert!(log.iter().all(|l| l == &format!("SOCKS5 localhost:{}", server.port)), "{log:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_server_through_proxy() {
    let server = common::start(Options { tls: true, ..Default::default() }).await;
    let port = server.port;
    let proxy = http_proxy(None).await;
    // The test server's certificate is self-signed; TLS still runs end to end.
    let mut cfg = config(server.url(), SocketIoTransport::Auto, custom(&proxy.url(), None));
    cfg.verify_ssl = false;
    echo(cfg, true).await.unwrap();
    let log = proxy.log();
    // HTTPS polling is tunnelled as well.
    assert!(log.len() >= 2 && log.iter().all(|l| l == &format!("CONNECT 127.0.0.1:{port}")), "{log:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_proxy_password_is_reported_without_it() {
    let server = common::start(Options::default()).await;
    let auth = Some(("alice", "s3cret"));
    for transport in [SocketIoTransport::Polling, SocketIoTransport::WebSocket] {
        let proxy = http_proxy(auth).await;
        let err = echo(config(server.url(), transport, custom(&proxy.url(), Some(("alice", "hunter2")))), false)
            .await
            .unwrap_err();
        assert!(err.contains("407") && err.contains(&proxy.url()), "{transport:?}: {err}");
        assert!(!err.contains("hunter2"), "{err}");
        assert!(proxy.accepted().is_empty());
        let socks = socks5_proxy(auth).await;
        let err = echo(config(server.url(), transport, custom(&socks.url(), Some(("alice", "hunter2")))), false)
            .await
            .unwrap_err();
        assert!(err.contains(&socks.url()) && err.contains("credentials"), "{transport:?}: {err}");
        assert!(!err.contains("hunter2"), "{err}");
        assert!(socks.accepted().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bypass_list_goes_direct() {
    let server = common::start(Options::default()).await;
    let proxy = http_proxy(None).await;
    let mut settings = custom(&proxy.url(), None);
    settings.bypass = "127.0.0.1".into();
    echo(config(server.url(), SocketIoTransport::Auto, settings), true).await.unwrap();
    assert!(proxy.log().is_empty(), "{:?}", proxy.log());
}
