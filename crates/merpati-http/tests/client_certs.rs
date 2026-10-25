//! Client certificates (FR-R8): PEM and PKCS#12 bundles in every common
//! encoding (fixtures made by `tests/fixtures/certs/generate.sh` with
//! OpenSSL 3), passphrases from `{{vault:...}}` references, and mutual TLS
//! against a server that requires a client certificate.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use merpati_core::prepared::PreparedRequest;
use merpati_core::settings::{CertificateSettings, ClientCertificate};
use merpati_http::certs::{load_client_identity, load_pem, load_pkcs12};
use merpati_http::{CookieJar, EngineConfig, HttpEngine, HttpErrorKind, SecretResolver, SendOptions};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "merpati";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/certs").join(name)
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(fixture(name)).unwrap()
}

fn leaf(name: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(&read(name)).unwrap()
}

fn vault() -> SecretResolver {
    SecretResolver::new(|s| s.replace("{{vault:client-p12}}", PASSWORD))
}

#[test]
fn pkcs12_modern_and_legacy_encodings() {
    for (file, pem) in [
        ("client-rsa-aes.p12", "client-rsa.pem"), // PBES2 / AES-256-CBC, HMAC-SHA256 MAC
        ("client-rsa-legacy.p12", "client-rsa.pem"), // RC2-40 + 3DES, SHA-1 MAC
        ("client-rsa-3des.p12", "client-rsa.pem"), // 3DES, SHA-1 MAC
        ("client-ec-aes.pfx", "client-ec.pem"),   // EC P-256 key
    ] {
        let id = load_pkcs12(&read(file), PASSWORD).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(id.cert_chain.len(), 2, "{file}: leaf + CA");
        assert_eq!(id.cert_chain[0], leaf(pem), "{file}: leaf first");
        assert_eq!(id.cert_chain[1], leaf("ca.pem"), "{file}");
        assert!(matches!(id.key, PrivateKeyDer::Pkcs8(_)), "{file}");
        // Round-trips through PEM (what reqwest is given).
        let again = load_pem(&id.to_pem(), || unreachable!()).unwrap();
        assert_eq!(again.cert_chain, id.cert_chain);
        assert_eq!(again.key.secret_der(), id.key.secret_der());
        assert!(!format!("{id:?}").contains("PRIVATE"), "{id:?}");
    }
    let id = load_pkcs12(&read("client-ec-nopass.p12"), "").unwrap();
    assert_eq!(id.cert_chain, vec![leaf("client-ec.pem")]);
}

#[test]
fn pkcs12_errors_are_readable() {
    for file in ["client-rsa-aes.p12", "client-rsa-legacy.p12"] {
        let err = load_pkcs12(&read(file), "wrong-secret").unwrap_err();
        assert!(err.contains("wrong password"), "{file}: {err}");
        assert!(!err.contains("wrong-secret"), "the password leaked: {err}");
    }
    let err = load_pkcs12(b"definitely not der", PASSWORD).unwrap_err();
    assert!(err.contains("not a PKCS#12 file"), "{err}");
    let err = load_pkcs12(&read("ca.pem"), PASSWORD).unwrap_err();
    assert!(err.contains("not a PKCS#12 file"), "{err}");
}

#[test]
fn client_certificate_settings_load() {
    let p12 = |pass: &str| ClientCertificate {
        host: "localhost".into(),
        pfx_path: fixture("client-rsa-aes.p12").display().to_string(),
        passphrase: pass.into(),
        enabled: true,
        ..Default::default()
    };
    let id = load_client_identity(&p12("{{vault:client-p12}}"), Some(&vault())).unwrap();
    assert_eq!(id.cert_chain[0], leaf("client-rsa.pem"));
    assert!(load_client_identity(&p12(PASSWORD), None).is_ok(), "literal passphrase");
    let err = load_client_identity(&p12("{{vault:client-p12}}"), None).unwrap_err();
    assert!(err.contains("no secret resolver") && err.contains("localhost"), "{err}");
    let err = load_client_identity(&p12("{{vault:other}}"), Some(&vault())).unwrap_err();
    assert!(err.contains("\"other\""), "{err}");
    let err = load_client_identity(&p12("nope"), None).unwrap_err();
    assert!(err.contains("wrong password") && err.contains("client-rsa-aes.p12"), "{err}");
    let missing = ClientCertificate { pfx_path: "/no/such/file.p12".into(), ..p12("") };
    assert!(load_client_identity(&missing, None).unwrap_err().contains("Could not read"));

    // PEM certificate + separate key, plain and encrypted (PKCS#8 / AES).
    let pem = |key: &str, pass: &str| ClientCertificate {
        host: "localhost".into(),
        cert_path: fixture("client-ec.pem").display().to_string(),
        key_path: fixture(key).display().to_string(),
        passphrase: pass.into(),
        enabled: true,
        ..Default::default()
    };
    let plain = load_client_identity(&pem("client-ec.key", "{{vault:unused}}"), None).unwrap();
    assert_eq!(plain.cert_chain, vec![leaf("client-ec.pem")]);
    let enc = load_client_identity(&pem("client-ec-encrypted.key", "{{vault:client-p12}}"), Some(&vault())).unwrap();
    assert_eq!(enc.key.secret_der(), plain.key.secret_der());
    let err = load_client_identity(&pem("client-ec-encrypted.key", "bad"), None).unwrap_err();
    assert!(err.contains("wrong passphrase"), "{err}");
    let no_key = ClientCertificate { key_path: String::new(), ..pem("client-ec.key", "") };
    assert!(load_client_identity(&no_key, None).unwrap_err().contains("No private key"));
}

// ---------------------------------------------------------------------------
// Mutual TLS over HTTP
// ---------------------------------------------------------------------------

/// An HTTPS server for `localhost` that requires a client certificate issued
/// by the fixture CA and answers with the presented leaf certificate
/// (base64 DER). Returns its port and the PEM of its own certificate.
async fn mtls_server() -> (u16, String) {
    merpati_http::tls::install_provider();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(leaf("ca.pem")).unwrap();
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let key = PrivateKeyDer::from_pem_slice(ck.signing_key.serialize_pem().as_bytes()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![ck.cert.der().clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let presented = tls.get_ref().1.peer_certificates().and_then(|c| c.first().cloned());
                let body = presented
                    .map(|c| base64::engine::general_purpose::STANDARD.encode(c.as_ref()))
                    .unwrap_or_else(|| "none".into());
                let mut buf = vec![0u8; 8192];
                let mut got = Vec::new();
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let resp =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = tls.write_all(resp.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (port, ck.cert.pem())
}

fn engine(certs: Vec<ClientCertificate>, server_pem: &str, dir: &tempfile::TempDir) -> HttpEngine {
    let ca = dir.path().join("server.pem");
    std::fs::write(&ca, server_pem).unwrap();
    let cfg = EngineConfig {
        certificates: CertificateSettings { ca_path: ca.display().to_string(), ca_enabled: true, client_certs: certs },
        secret_resolver: Some(vault()),
        ..Default::default()
    };
    HttpEngine::new(cfg, Arc::new(CookieJar::new()))
}

async fn presented(e: &HttpEngine, port: u16) -> Result<Vec<u8>, merpati_http::HttpError> {
    let req = PreparedRequest { method: "GET".into(), url: format!("https://localhost:{port}/"), ..Default::default() };
    let out = e.send(&req, &SendOptions::default()).await?;
    Ok(base64::engine::general_purpose::STANDARD.decode(out.response.body.text_lossy().as_bytes()).unwrap())
}

#[tokio::test]
async fn mutual_tls_with_pkcs12_and_pem() {
    let (port, server_pem) = mtls_server().await;
    let dir = tempfile::tempdir().unwrap();
    let p12 = |file: &str, host: &str| ClientCertificate {
        host: host.into(),
        pfx_path: fixture(file).display().to_string(),
        passphrase: "{{vault:client-p12}}".into(),
        enabled: true,
        ..Default::default()
    };
    for file in ["client-rsa-aes.p12", "client-rsa-legacy.p12", "client-rsa-3des.p12"] {
        let e = engine(vec![p12(file, &format!("localhost:{port}"))], &server_pem, &dir);
        let der = presented(&e, port).await.unwrap_or_else(|err| panic!("{file}: {err:?}"));
        assert_eq!(der, leaf("client-rsa.pem").as_ref(), "{file}");
    }
    // The most specific entry wins over the `*` fallback.
    let e = engine(vec![p12("client-rsa-aes.p12", "*"), p12("client-ec-aes.pfx", "localhost")], &server_pem, &dir);
    assert_eq!(presented(&e, port).await.unwrap(), leaf("client-ec.pem").as_ref());

    let pem = ClientCertificate {
        host: "*".into(),
        cert_path: fixture("client-ec.pem").display().to_string(),
        key_path: fixture("client-ec-encrypted.key").display().to_string(),
        passphrase: "{{vault:client-p12}}".into(),
        enabled: true,
        ..Default::default()
    };
    let e = engine(vec![pem], &server_pem, &dir);
    assert_eq!(presented(&e, port).await.unwrap(), leaf("client-ec.pem").as_ref());

    // No certificate for this host: the server refuses the handshake.
    let e = engine(vec![p12("client-rsa-aes.p12", "other.example.com")], &server_pem, &dir);
    assert!(presented(&e, port).await.is_err());

    // Unresolvable passphrase: a certificate error naming the secret.
    let mut bad = p12("client-rsa-aes.p12", "localhost");
    bad.passphrase = "{{vault:missing}}".into();
    let e = engine(vec![bad], &server_pem, &dir);
    let err = presented(&e, port).await.unwrap_err();
    assert_eq!(err.kind, HttpErrorKind::Certificate, "{err:?}");
    assert!(err.message.contains("\"missing\""), "{err:?}");
    // ... which does not affect plain http:// requests to the same host.
    let plain = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plain_port = plain.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut s, _)) = plain.accept().await {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
        }
    });
    let req =
        PreparedRequest { method: "GET".into(), url: format!("http://localhost:{plain_port}/"), ..Default::default() };
    let out = e.send(&req, &SendOptions::default()).await.unwrap();
    assert_eq!(out.response.body.text_lossy(), "ok");
}

/// The client certificate is chosen per hop: after `http://` -> `https://`
/// the redirect target's certificate is presented.
#[tokio::test]
async fn redirect_to_https_presents_the_targets_certificate() {
    let (port, server_pem) = mtls_server().await;
    let dir = tempfile::tempdir().unwrap();
    let plain = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plain_port = plain.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = plain.accept().await {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 302 Found\r\nLocation: https://localhost:{port}/\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    let cert = ClientCertificate {
        host: format!("localhost:{port}"),
        pfx_path: fixture("client-ec-aes.pfx").display().to_string(),
        passphrase: "{{vault:client-p12}}".into(),
        enabled: true,
        ..Default::default()
    };
    let e = engine(vec![cert], &server_pem, &dir);
    let req =
        PreparedRequest { method: "GET".into(), url: format!("http://127.0.0.1:{plain_port}/"), ..Default::default() };
    let out = e.send(&req, &SendOptions::default()).await.unwrap();
    assert_eq!(out.response.redirects.len(), 1);
    let der = base64::engine::general_purpose::STANDARD.decode(out.response.body.text_lossy().as_bytes()).unwrap();
    assert_eq!(der, leaf("client-ec.pem").as_ref());
}

/// The setup WebSocket, Socket.IO and MQTT use: the same CA and client
/// certificate selection as the HTTP client, the certificate only for TLS
/// schemes.
#[tokio::test]
async fn tls_setup_for_other_protocols() {
    use merpati_http::tls::TlsSetup;
    let (port, server_pem) = mtls_server().await;
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("server.pem");
    std::fs::write(&ca, &server_pem).unwrap();
    let settings = CertificateSettings {
        ca_path: ca.display().to_string(),
        ca_enabled: true,
        client_certs: vec![ClientCertificate {
            host: "localhost".into(),
            pfx_path: fixture("client-ec-aes.pfx").display().to_string(),
            passphrase: "{{vault:client-p12}}".into(),
            enabled: true,
            ..Default::default()
        }],
    };
    let secrets = vault();
    for url in [format!("wss://localhost:{port}/socket"), format!("mqtts://localhost:{port}"), "localhost".into()] {
        let setup = TlsSetup::for_url(&settings, Some(&secrets), true, &url).unwrap();
        let tls = !url.starts_with("localhost");
        assert_eq!(setup.identity.is_some(), tls, "{url}");
        assert!(setup.extra_roots_pem.is_some(), "{url}");
    }
    // Plain URLs only get the CA bundle (for TLS to an HTTPS proxy).
    for url in ["ws://localhost/", "mqtt://localhost", "http://localhost"] {
        let setup = TlsSetup::for_url(&settings, Some(&secrets), true, url).unwrap();
        assert!(setup.identity.is_none() && setup.extra_roots_pem.is_some(), "{url}");
        assert!(TlsSetup::for_url(&settings, Some(&secrets), false, url).unwrap().is_empty(), "{url}");
    }
    // Without verification the CA bundle is not read (as for HTTP).
    let setup = TlsSetup::for_url(&settings, Some(&secrets), false, "wss://localhost/").unwrap();
    assert!(setup.extra_roots_pem.is_none() && setup.identity.is_some());

    // A real handshake presents the configured certificate and trusts the CA.
    let setup = TlsSetup::for_url(&settings, Some(&secrets), true, &format!("wss://localhost:{port}/")).unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(setup.client_config(true).unwrap()));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(name, tcp).await.unwrap();
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out).await;
    let body = String::from_utf8_lossy(&out);
    let b64 = body.split("\r\n\r\n").nth(1).unwrap_or_default();
    let der = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
    assert_eq!(der, leaf("client-ec.pem").as_ref());

    // Problems surface before connecting.
    let mut bad = settings.clone();
    bad.client_certs[0].passphrase = "{{vault:missing}}".into();
    let err = TlsSetup::for_url(&bad, Some(&secrets), true, "wss://localhost/").unwrap_err();
    assert!(err.contains("\"missing\""), "{err}");
}
