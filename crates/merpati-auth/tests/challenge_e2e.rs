//! Digest and NTLM handshakes over real HTTP(S), driven the way the request
//! pipeline does it: apply, send with the HTTP engine, handle_challenge with
//! the response's TLS certificate, resend. Also through an HTTP proxy.

#[path = "../../merpati-http/tests/support/proxies.rs"]
mod proxies;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use md5::{Digest, Md5};
use sha2::Sha384;

use merpati_auth::challenge::parse_challenges;
use merpati_auth::{AuthContext, apply, handle_challenge};
use merpati_core::settings::{CertificateSettings, ProxySettings};
use merpati_core::tokens::MemoryTokenStore;
use merpati_core::{Auth, AuthType, PreparedRequest};
use merpati_http::{CookieJar, EngineConfig, HttpEngine, SendOptions};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn h(s: &str) -> String {
    hex::encode(Md5::digest(s.as_bytes()))
}

async fn digest_endpoint(headers: HeaderMap) -> Response {
    let challenge = || {
        (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", r#"Digest realm="test", qop="auth", nonce="n0nce", opaque="op", algorithm=MD5"#)],
        )
            .into_response()
    };
    let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) else { return challenge() };
    let c = parse_challenges([auth]);
    let Some(d) = c.iter().find(|c| c.scheme == "digest") else { return challenge() };
    let p = |k: &str| d.param(k).unwrap_or_default().to_string();
    let ha1 = h(&format!("{}:{}:{}", p("username"), "test", "s3cret"));
    let ha2 = h(&format!("GET:{}", p("uri")));
    let expected = h(&format!("{ha1}:{}:{}:{}:{}:{ha2}", p("nonce"), p("nc"), p("cnonce"), p("qop")));
    if p("response") == expected && p("opaque") == "op" && p("uri") == "/digest?x=1" {
        (StatusCode::OK, "welcome").into_response()
    } else {
        challenge()
    }
}

/// What the NTLM server requires as MsvAvChannelBindings (like Extended
/// Protection set to "Required"), and the values clients sent.
#[derive(Clone)]
struct Epa {
    expected: [u8; 16],
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// MsvAvChannelBindings of a type 3 message's NTLMv2 response, if present.
fn sent_channel_bindings(nt: &[u8]) -> Option<Vec<u8>> {
    // NTProofStr (16) and the fixed part of NTLMv2_CLIENT_CHALLENGE (28), then AV pairs.
    let mut pairs = nt.get(44..)?;
    while pairs.len() >= 4 {
        let id = u16::from_le_bytes([pairs[0], pairs[1]]);
        let len = u16::from_le_bytes([pairs[2], pairs[3]]) as usize;
        let value = pairs.get(4..4 + len)?;
        match id {
            0 => return None,
            10 => return Some(value.to_vec()),
            _ => pairs = &pairs[4 + len..],
        }
    }
    None
}

/// Minimal NTLM server: answers NEGOTIATE with a CHALLENGE and accepts any
/// well-formed AUTHENTICATE for user "User" in domain "Domain" carrying the
/// expected channel bindings.
async fn ntlm_endpoint(State(epa): State<Epa>, headers: HeaderMap) -> Response {
    let unauthorized = |v: String| (StatusCode::UNAUTHORIZED, [("www-authenticate", v)]).into_response();
    let Some(token) = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("NTLM "))
    else {
        return unauthorized("NTLM".into());
    };
    let msg = B64.decode(token).unwrap_or_default();
    if msg.len() < 12 || &msg[..8] != b"NTLMSSP\0" {
        return unauthorized("NTLM".into());
    }
    match msg[8] {
        1 => {
            let mut ch = b"NTLMSSP\0".to_vec();
            ch.extend_from_slice(&2u32.to_le_bytes());
            ch.extend_from_slice(&[0, 0, 0, 0, 48, 0, 0, 0]); // empty target name
            ch.extend_from_slice(&0x0000_0201u32.to_le_bytes()); // unicode | ntlm
            ch.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
            ch.extend_from_slice(&[0; 8]);
            ch.extend_from_slice(&[4, 0, 4, 0, 48, 0, 0, 0]); // target info: EOL only
            ch.extend_from_slice(&[0, 0, 0, 0]);
            unauthorized(format!("NTLM {}", B64.encode(ch)))
        }
        3 => {
            let field = |at: usize| {
                let len = u16::from_le_bytes([msg[at], msg[at + 1]]) as usize;
                let off = u32::from_le_bytes([msg[at + 4], msg[at + 5], msg[at + 6], msg[at + 7]]) as usize;
                msg.get(off..off + len).unwrap_or_default().to_vec()
            };
            let utf16 = |s: &str| s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>();
            let nt = field(20);
            let bindings = sent_channel_bindings(&nt).unwrap_or_default();
            epa.seen.lock().unwrap().push(bindings.clone());
            if field(28) == utf16("Domain") && field(36) == utf16("User") && bindings == epa.expected {
                (StatusCode::OK, "ntlm ok").into_response()
            } else {
                unauthorized("NTLM".into())
            }
        }
        _ => unauthorized("NTLM".into()),
    }
}

fn app(epa: Epa) -> Router {
    Router::new().route("/digest", get(digest_endpoint)).route("/ntlm", get(ntlm_endpoint)).with_state(epa)
}

/// Plain HTTP server: NTLM must carry all-zero channel bindings.
async fn start() -> (String, Epa) {
    let epa = Epa { expected: [0; 16], seen: Default::default() };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(epa.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), epa)
}

struct TlsListener {
    tcp: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((tcp, addr)) = self.tcp.accept().await else { continue };
            if let Ok(tls) = self.acceptor.accept(tcp).await {
                return (tls, addr);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// MD5 of a `gss_channel_bindings_struct` without addresses (what
/// MsvAvChannelBindings holds), computed here independently of the crate.
fn gss_bindings_md5(application_data: &[u8]) -> [u8; 16] {
    let mut gss = vec![0u8; 16];
    gss.extend_from_slice(&(application_data.len() as u32).to_le_bytes());
    gss.extend_from_slice(application_data);
    Md5::digest(&gss).into()
}

/// HTTPS server for `localhost` with a P-384 certificate signed with
/// ECDSA-SHA384, requiring the tls-server-end-point binding of that
/// certificate (SHA-384 of its DER). Returns its base URL, the state and the
/// certificate PEM.
async fn start_tls() -> (String, Epa, String) {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap().self_signed(&key).unwrap();
    let app_data = [b"tls-server-end-point:".as_slice(), &Sha384::digest(cert.der())].concat();
    let epa = Epa { expected: gss_bindings_md5(&app_data), seen: Default::default() };

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key_der.into())
        .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = tcp.local_addr().unwrap().port();
    let listener = TlsListener { tcp, acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)) };
    let router = app(epa.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("https://localhost:{port}"), epa, cert.pem())
}

/// An engine trusting `ca_pem` (TLS verification stays on).
fn engine(ca_pem: Option<&str>, dir: &tempfile::TempDir) -> HttpEngine {
    engine_via(ca_pem, dir, ProxySettings::default())
}

/// [`engine`] with these proxy settings.
fn engine_via(ca_pem: Option<&str>, dir: &tempfile::TempDir, proxy: ProxySettings) -> HttpEngine {
    let mut cfg = EngineConfig { proxy, ..Default::default() };
    if let Some(pem) = ca_pem {
        let path = dir.path().join("server.pem");
        std::fs::write(&path, pem).unwrap();
        cfg.certificates =
            CertificateSettings { ca_path: path.display().to_string(), ca_enabled: true, ..Default::default() };
    }
    HttpEngine::new(cfg, Arc::new(CookieJar::new()))
}

/// The pipeline loop: apply once, then answer up to 3 challenges. With
/// `bind_tls` false the client ignores the server certificate, like Merpati
/// did before channel bindings.
async fn run_with(engine: &HttpEngine, mut req: PreparedRequest, bind_tls: bool) -> (u16, String, usize) {
    let tokens = MemoryTokenStore::default();
    let http = engine.default_client().unwrap();
    apply(&mut req, &AuthContext { tokens: &tokens, http: &http }).await.unwrap();
    let mut sends = 0;
    loop {
        let out = engine.send(&req, &SendOptions::default()).await.unwrap();
        sends += 1;
        let r = &out.response;
        let cert = out.peer_certificate.as_deref().filter(|_| bind_tls);
        if sends > 3 || !handle_challenge(&mut req, r.status, &r.headers, cert).unwrap() {
            return (r.status, r.body.text_lossy(), sends);
        }
    }
}

async fn run(engine: &HttpEngine, req: PreparedRequest) -> (u16, String, usize) {
    run_with(engine, req, true).await
}

fn ntlm_request(url: String) -> PreparedRequest {
    PreparedRequest {
        url,
        auth: Auth::new(AuthType::Ntlm).with("username", "User").with("password", "Password").with("domain", "Domain"),
        ..Default::default()
    }
}

#[tokio::test]
async fn digest_handshake() {
    let (base, _) = start().await;
    let dir = tempfile::tempdir().unwrap();
    let e = engine(None, &dir);
    let req = PreparedRequest {
        url: format!("{base}/digest?x=1"),
        auth: Auth::new(AuthType::Digest).with("username", "mufasa").with("password", "s3cret"),
        ..Default::default()
    };
    let (status, body, sends) = run(&e, req.clone()).await;
    assert_eq!((status, body.as_str(), sends), (200, "welcome", 2));

    let mut wrong = req;
    wrong.auth.set("password", "bad");
    let (status, _, sends) = run(&e, wrong).await;
    assert_eq!((status, sends), (401, 2), "wrong password stops after one retry");
}

#[tokio::test]
async fn ntlm_handshake() {
    let (base, epa) = start().await;
    let dir = tempfile::tempdir().unwrap();
    let e = engine(None, &dir);
    // NEGOTIATE (sent by apply) -> CHALLENGE -> AUTHENTICATE.
    let (status, body, sends) = run(&e, ntlm_request(format!("{base}/ntlm"))).await;
    assert_eq!((status, body.as_str(), sends), (200, "ntlm ok", 2));
    // Plain HTTP: MsvAvChannelBindings is present and all zero.
    assert_eq!(*epa.seen.lock().unwrap(), vec![vec![0u8; 16]]);
}

#[tokio::test]
async fn ntlm_over_https_binds_the_server_certificate() {
    let (base, epa, pem) = start_tls().await;
    let dir = tempfile::tempdir().unwrap();
    let e = engine(Some(&pem), &dir);
    let (status, body, sends) = run(&e, ntlm_request(format!("{base}/ntlm"))).await;
    assert_eq!((status, body.as_str(), sends), (200, "ntlm ok", 2));
    assert_ne!(epa.expected, [0; 16]);
    assert_eq!(*epa.seen.lock().unwrap(), vec![epa.expected.to_vec()]);

    // Without the binding the server rejects the AUTHENTICATE message.
    let (status, _, sends) = run_with(&e, ntlm_request(format!("{base}/ntlm")), false).await;
    assert_eq!((status, sends), (401, 2));
    assert_eq!(epa.seen.lock().unwrap().last(), Some(&vec![0u8; 16]));
}

#[tokio::test]
async fn ntlm_over_https_through_a_proxy_binds_the_server_certificate() {
    let (base, epa, pem) = start_tls().await;
    let proxy = proxies::http_proxy(None).await;
    let dir = tempfile::tempdir().unwrap();
    let e = engine_via(Some(&pem), &dir, proxies::custom(&proxy.url(), None));
    let (status, body, sends) = run(&e, ntlm_request(format!("{base}/ntlm"))).await;
    assert_eq!((status, body.as_str(), sends), (200, "ntlm ok", 2));
    assert_eq!(*epa.seen.lock().unwrap(), vec![epa.expected.to_vec()]);
    // reqwest's tunnel, and the one the engine opened to read the server
    // certificate for the binding.
    let target = base.trim_start_matches("https://");
    assert_eq!(proxy.log(), vec![format!("CONNECT {target}"); 2]);
}
