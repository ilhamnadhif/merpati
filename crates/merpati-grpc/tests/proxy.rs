//! gRPC through HTTP and SOCKS5 proxies: the HTTP/2 connection (and TLS,
//! end to end) runs inside the proxy tunnel.

mod common;
#[path = "../../merpati-http/tests/support/proxies.rs"]
mod proxies;

use std::time::Duration;

use common::{Reflection, self_signed, spawn};
use merpati_core::settings::{CertificateSettings, ProxySettings};
use merpati_grpc::{GrpcError, Target, invoke_unary, load_via_reflection};
use proxies::{custom, http_proxy, socks5_proxy};
use serde_json::Value;

const SVC: &str = "helloworld.Greeter";

fn target(url: String, proxy: ProxySettings) -> Target {
    Target { url, timeout: Some(Duration::from_secs(10)), ..Default::default() }.with_proxy(proxy)
}

async fn say_hello(t: &Target) -> Result<String, GrpcError> {
    let schema = load_via_reflection(t).await?;
    let resp = invoke_unary(t, &schema, SVC, "SayHello", r#"{"name":"proxy"}"#).await?;
    let v: Value = serde_json::from_str(&resp.json).unwrap();
    Ok(v["message"].as_str().unwrap_or_default().to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn reflection_and_calls_through_http_proxy() {
    let server = spawn(Reflection::V1, None).await;
    let auth = Some(("alice", "s3cret"));
    let proxy = http_proxy(auth).await;
    let t = target(server.url(), custom(&proxy.url(), auth));
    assert_eq!(say_hello(&t).await.unwrap(), "Hello, proxy!");
    // One tunnel for reflection, one for the call.
    assert_eq!(proxy.log(), vec![format!("CONNECT {}", server.url()); 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_through_socks5_with_verification() {
    let (cert, key) = self_signed();
    let server = spawn(Reflection::V1, Some((cert.clone(), key))).await;
    let port = server.addr.port();
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, cert).unwrap();
    let certificates =
        CertificateSettings { ca_path: ca.display().to_string(), ca_enabled: true, ..Default::default() };
    let auth = Some(("bob", "pa55"));
    let proxy = socks5_proxy(auth).await;
    let t =
        target(format!("grpcs://localhost:{port}"), custom(&proxy.url(), auth)).with_certificates(certificates, None);
    assert!(t.verify_ssl);
    assert_eq!(say_hello(&t).await.unwrap(), "Hello, proxy!");
    // socks5h: the proxy resolves the name; TLS runs end to end.
    assert_eq!(proxy.log(), vec![format!("SOCKS5 localhost:{port}"); 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_failures_are_explained() {
    let server = spawn(Reflection::V1, None).await;
    let auth = Some(("alice", "s3cret"));
    let http = http_proxy(auth).await;
    let socks = socks5_proxy(auth).await;
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cases = [
        (custom(&http.url(), Some(("alice", "hunter2"))), "rejected the proxy credentials"),
        (custom(&socks.url(), Some(("alice", "hunter2"))), "rejected the proxy credentials"),
        (custom(&socks.url(), None), "requires authentication"),
        (custom(&format!("socks5://127.0.0.1:{dead}"), None), "Could not connect to the proxy"),
        (custom("ftp://user:hunter2@proxy", None), "Unsupported proxy scheme"),
    ];
    for (settings, want) in cases {
        let err = say_hello(&target(server.url(), settings)).await.unwrap_err();
        assert!(matches!(err, GrpcError::Connect(_)) && err.code() == 14, "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains(want) && !msg.contains("hunter2"), "{want}: {msg}");
    }
    assert!(http.accepted().is_empty() && socks.accepted().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn bypassed_server_is_reached_directly() {
    let server = spawn(Reflection::V1, None).await;
    let proxy = http_proxy(None).await;
    let mut settings = custom(&proxy.url(), None);
    settings.bypass = "127.0.0.0/8".into();
    assert_eq!(say_hello(&target(server.url(), settings)).await.unwrap(), "Hello, proxy!");
    assert!(proxy.log().is_empty(), "{:?}", proxy.log());
}
