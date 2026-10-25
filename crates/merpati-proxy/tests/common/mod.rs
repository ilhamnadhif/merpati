#![allow(dead_code)]

use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post};
use futures_util::{SinkExt, Stream, StreamExt};
use merpati_proxy::{CaptureEntry, CaptureProxy, CertificateAuthority, ProxyConfig};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub fn init() {
    merpati_http::tls::install_provider();
}

/// A server certificate plus the PEM of the CA that signed it (or of the
/// certificate itself when self-signed).
pub struct ServerCert {
    pub ca_pem: String,
    pub chain: Vec<CertificateDer<'static>>,
    pub key_der: Vec<u8>,
}

impl ServerCert {
    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }
}

/// A test CA and a `localhost`/`127.0.0.1` certificate signed by it.
pub fn ca_signed_cert() -> ServerCert {
    let ca_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, "Test Upstream CA");
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::CrlSign];
    let ca_cert = params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(params, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let mut lp = CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
    lp.distinguished_name.push(DnType::CommonName, "localhost");
    lp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = lp.signed_by(&leaf_key, &issuer).unwrap();
    ServerCert { ca_pem: ca_cert.pem(), chain: vec![leaf.der().clone()], key_der: leaf_key.serialize_der() }
}

pub fn self_signed_cert() -> ServerCert {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
    ServerCert { ca_pem: ck.cert.pem(), chain: vec![ck.cert.der().clone()], key_der: ck.signing_key.serialize_der() }
}

pub fn server_tls_config(cert: &ServerCert, alpn: &[&[u8]]) -> Arc<rustls::ServerConfig> {
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(cert.chain.clone(), cert.key())
        .unwrap();
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(cfg)
}

pub fn client_tls_config(ca_pem: &str) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls::pki_types::pem::PemObject::pem_slice_iter(ca_pem.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

struct TlsListener {
    tcp: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((tcp, addr)) = self.tcp.accept().await else { continue };
            if let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(5), self.acceptor.accept(tcp)).await {
                return (tls, addr);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

fn chunks(total: u64) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    futures_util::stream::unfold(0u64, move |sent| async move {
        if sent >= total {
            return None;
        }
        let n = (total - sent).min(64 * 1024);
        Some((Ok(Bytes::from(vec![b'x'; n as usize])), sent + n))
    })
}

pub const JSON_BODY: &str = r#"{"items":[1,2,3],"message":"hello hello hello hello hello"}"#;

pub fn app() -> Router {
    Router::new()
        .route("/hello", get(|| async { "hello world" }))
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                "late"
            }),
        )
        .route("/json", get(|| async { ([(header::CONTENT_TYPE, "application/json")], JSON_BODY) }))
        .route(
            "/echo",
            post(|headers: HeaderMap, body: Bytes| async move {
                let ct = headers
                    .get(header::CONTENT_TYPE)
                    .cloned()
                    .unwrap_or(header::HeaderValue::from_static("text/plain"));
                ([(header::CONTENT_TYPE, ct)], body)
            }),
        )
        .route(
            "/gzip",
            get(|| async {
                let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                gz.write_all(JSON_BODY.as_bytes()).unwrap();
                ([(header::CONTENT_TYPE, "application/json"), (header::CONTENT_ENCODING, "gzip")], gz.finish().unwrap())
            }),
        )
        .route(
            "/br",
            get(|| async {
                let mut out = Vec::new();
                {
                    let mut w = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
                    w.write_all(JSON_BODY.as_bytes()).unwrap();
                }
                ([(header::CONTENT_TYPE, "application/json"), (header::CONTENT_ENCODING, "br")], out)
            }),
        )
        .route("/stream/{n}", get(|Path(n): Path<u64>| async move { Body::from_stream(chunks(n)) }))
        .route(
            "/count",
            post(|body: Body| async move {
                let mut s = body.into_data_stream();
                let mut n = 0usize;
                while let Some(c) = s.next().await {
                    n += c.unwrap().len();
                }
                n.to_string()
            }),
        )
        .route(
            "/headers",
            get(|headers: HeaderMap| async move {
                let map: serde_json::Map<String, serde_json::Value> = headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_str().unwrap_or("").to_owned())))
                    .collect();
                axum::Json(serde_json::Value::Object(map))
            }),
        )
        .route("/status/{code}", get(|Path(c): Path<u16>| async move { StatusCode::from_u16(c).unwrap() }))
        .route("/version", get(|req: Request| async move { format!("{:?}", req.version()) }))
}

pub async fn spawn_http() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app()).await.unwrap() });
    addr
}

pub async fn spawn_https(cert: &ServerCert, alpn: &[&[u8]]) -> SocketAddr {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(server_tls_config(cert, alpn));
    tokio::spawn(async move { axum::serve(TlsListener { tcp, acceptor }, app()).await.unwrap() });
    addr
}

async fn ws_echo<S>(stream: S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
    while let Some(Ok(msg)) = ws.next().await {
        if msg.is_text() || msg.is_binary() {
            if ws.send(msg).await.is_err() {
                break;
            }
        } else if msg.is_close() {
            break;
        }
    }
}

/// A plain WebSocket echo server.
pub async fn spawn_ws() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(ws_echo(tcp));
        }
    });
    addr
}

/// A TLS WebSocket echo server.
pub async fn spawn_wss(cert: &ServerCert) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(server_tls_config(cert, &[b"http/1.1"]));
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    ws_echo(tls).await;
                }
            });
        }
    });
    addr
}

/// A port nothing listens on.
pub async fn dead_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

pub async fn start_proxy(f: impl FnOnce(&mut ProxyConfig)) -> (CaptureProxy, Arc<CertificateAuthority>) {
    init();
    let ca = Arc::new(CertificateAuthority::generate().unwrap());
    let mut cfg = ProxyConfig { bind: "127.0.0.1:0".parse().unwrap(), ca: Some(ca.clone()), ..ProxyConfig::default() };
    f(&mut cfg);
    (CaptureProxy::start(cfg).await.unwrap(), ca)
}

/// reqwest through `proxy`, trusting only `ca_pem` (when given).
pub fn client(proxy: &CaptureProxy, ca_pem: Option<&str>) -> reqwest::Client {
    let mut b =
        reqwest::Client::builder().proxy(reqwest::Proxy::all(proxy.url()).unwrap()).timeout(Duration::from_secs(60));
    if let Some(pem) = ca_pem {
        b = b.tls_certs_only([reqwest::Certificate::from_pem(pem.as_bytes()).unwrap()]);
    }
    b.build().unwrap()
}

/// Waits until `pred` holds for the stored entries.
pub async fn wait_until(
    proxy: &CaptureProxy,
    what: &str,
    pred: impl Fn(&[Arc<CaptureEntry>]) -> bool,
) -> Vec<Arc<CaptureEntry>> {
    for _ in 0..400 {
        let entries = proxy.entries();
        if pred(&entries) {
            return entries;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}; entries: {:#?}", proxy.entries());
}

/// Waits for `n` finished entries.
pub async fn finished(proxy: &CaptureProxy, n: usize) -> Vec<Arc<CaptureEntry>> {
    wait_until(proxy, &format!("{n} finished entries"), |e| e.len() >= n && e.iter().all(|e| e.is_finished())).await
}

pub struct CurlOutput {
    pub status: String,
    pub body: String,
    pub stderr: String,
    pub success: bool,
}

/// Runs curl (proxy env vars removed); `-w` prints `code version`.
pub async fn curl(args: &[&str]) -> CurlOutput {
    let out = tokio::process::Command::new("curl")
        .args(["-sS", "--max-time", "30", "-w", "\n%{http_code} %{http_version}"])
        .args(args)
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .await
        .expect("curl must be installed");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, status) = stdout.rsplit_once('\n').map(|(b, s)| (b.to_owned(), s.to_owned())).unwrap_or_default();
    CurlOutput {
        status,
        body,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
    }
}

/// Opens a CONNECT tunnel through the proxy and returns the raw stream.
pub async fn connect_tunnel(proxy: &CaptureProxy, target: &str) -> TcpStream {
    let mut s = TcpStream::connect(proxy.addr()).await.unwrap();
    s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).await.unwrap();
        head.push(b[0]);
    }
    let text = String::from_utf8_lossy(&head);
    assert!(text.starts_with("HTTP/1.1 200"), "CONNECT failed: {text}");
    s
}
