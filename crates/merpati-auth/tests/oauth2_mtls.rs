//! OAuth 2.0 token requests to an endpoint that requires a client
//! certificate (mutual TLS, RFC 8705): with the HTTP engine as the token
//! client, the certificate configured for the token endpoint's host is
//! presented, with the same rules as for requests.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use merpati_auth::{AuthContext, AuthError, apply, oauth2};
use merpati_core::settings::{CertificateSettings, ClientCertificate};
use merpati_core::tokens::{MemoryTokenStore, OAuth2Token};
use merpati_core::{Auth, AuthType, PreparedRequest};
use merpati_http::{CookieJar, EngineConfig, HttpEngine};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A CA, and a client certificate it issued (PEM certificate and key).
fn client_pki() -> (CertificateDer<'static>, String, String) {
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Merpati test CA");
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, rcgen::KeyPair::generate().unwrap()).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["client.test".to_string()]).unwrap();
    params.distinguished_name.push(rcgen::DnType::CommonName, "merpati-client");
    let cert = params.signed_by(&key, &ca).unwrap();
    (ca.der().clone(), cert.pem(), key.serialize_pem())
}

/// A token endpoint for `localhost` that requires a client certificate
/// issued by `client_ca`. Returns its port, its certificate (PEM) and the
/// number of token requests it answered.
async fn mtls_token_endpoint(client_ca: CertificateDer<'static>) -> (u16, String, Arc<AtomicUsize>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(client_ca).unwrap();
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build().unwrap();
    let server = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server.signing_key.serialize_der()));
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![server.cert.der().clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let answered = Arc::new(AtomicUsize::new(0));
    let count = answered.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, count) = (acceptor.clone(), count.clone());
            tokio::spawn(async move {
                // The handshake fails without an accepted client certificate.
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                count.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"access_token":"mtls-token","token_type":"Bearer","expires_in":3600}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(resp.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (port, server.cert.pem(), answered)
}

fn client_credentials(port: u16) -> Auth {
    Auth::new(AuthType::OAuth2)
        .with("grant_type", "client_credentials")
        .with("accessTokenUrl", format!("https://localhost:{port}/token"))
        .with("clientId", "cid")
        .with("clientSecret", "secret")
}

#[tokio::test]
async fn token_requests_present_the_client_certificate_of_the_token_endpoint() {
    let (client_ca, cert_pem, key_pem) = client_pki();
    let (port, server_pem, answered) = mtls_token_endpoint(client_ca).await;
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    };
    let certificates = CertificateSettings {
        ca_path: write("server.pem", &server_pem),
        ca_enabled: true,
        client_certs: vec![ClientCertificate {
            host: format!("localhost:{port}"),
            cert_path: write("client.pem", &cert_pem),
            key_path: write("client.key", &key_pem),
            enabled: true,
            ..Default::default()
        }],
    };
    let engine = HttpEngine::new(EngineConfig { certificates, ..Default::default() }, Arc::new(CookieJar::new()));

    // The token is fetched by `apply` for a request to another host.
    let tokens = MemoryTokenStore::default();
    let mut req = PreparedRequest {
        url: "https://api.test/data".into(),
        auth: client_credentials(port),
        auth_owner_id: Some("col-mtls".into()),
        ..Default::default()
    };
    let applied = apply(&mut req, &AuthContext { tokens: &tokens, http: &engine }).await.unwrap();
    assert_eq!(req.header("authorization"), Some("Bearer mtls-token"), "{:?}", applied.notes);

    // "Get New Access Token" and refreshing go the same way.
    let token = oauth2::get_new_token(&client_credentials(port), &engine, &|_| {}).await.unwrap();
    assert_eq!(token.access_token, "mtls-token");
    let old = OAuth2Token { refresh_token: Some("r1".into()), ..token };
    assert_eq!(oauth2::refresh(&client_credentials(port), &old, &engine).await.unwrap().access_token, "mtls-token");
    assert_eq!(answered.load(Ordering::SeqCst), 3);

    // A plain client presents no certificate: the endpoint ends the handshake.
    let plain = engine.default_client().unwrap();
    let err = oauth2::get_new_token(&client_credentials(port), &plain, &|_| {}).await.unwrap_err();
    assert!(matches!(err, AuthError::Http(_)), "{err:?}");
    assert_eq!(answered.load(Ordering::SeqCst), 3);
}
