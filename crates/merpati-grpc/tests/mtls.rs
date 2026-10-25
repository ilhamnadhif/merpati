//! Mutual TLS: the client certificate configured in the app settings for
//! the target host (PEM or PKCS#12, passphrase from a `{{vault:...}}`
//! reference) is presented to a tonic server that requires one; the extra
//! CA setting is trusted for the server certificate.

mod common;

use std::time::Duration;

use common::{Reflection, cert_fixture, protos_dir, self_signed, spawn_mtls};
use merpati_core::settings::{CertificateSettings, ClientCertificate};
use merpati_grpc::{GrpcError, Target, invoke_unary, load_proto_files, load_via_reflection};
use merpati_http::SecretResolver;

const SVC: &str = "helloworld.Greeter";

fn p12(file: &str, host: &str) -> ClientCertificate {
    ClientCertificate {
        host: host.into(),
        pfx_path: cert_fixture(file).display().to_string(),
        passphrase: "{{vault:grpc-client}}".into(),
        enabled: true,
        ..Default::default()
    }
}

fn vault() -> SecretResolver {
    SecretResolver::new(|s| s.replace("{{vault:grpc-client}}", "merpati"))
}

struct Fixture {
    port: u16,
    _dir: tempfile::TempDir,
    ca_path: String,
}

async fn server() -> Fixture {
    let (cert, key) = self_signed();
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("server-ca.pem");
    std::fs::write(&ca_path, &cert).unwrap();
    let client_ca = std::fs::read_to_string(cert_fixture("ca.pem")).unwrap();
    let server = spawn_mtls(Reflection::V1, (cert, key), client_ca).await;
    Fixture { port: server.addr.port(), _dir: dir, ca_path: ca_path.display().to_string() }
}

impl Fixture {
    /// A verifying target trusting the server certificate through the CA setting.
    fn target(&self, client_certs: Vec<ClientCertificate>) -> Target {
        let certificates = CertificateSettings { ca_path: self.ca_path.clone(), ca_enabled: true, client_certs };
        Target {
            url: format!("grpcs://localhost:{}", self.port),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        }
        .with_certificates(certificates, Some(vault()))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pkcs12_and_pem_client_certificates() {
    let f = server().await;
    let pem = ClientCertificate {
        host: "localhost".into(),
        cert_path: cert_fixture("client-ec.pem").display().to_string(),
        key_path: cert_fixture("client-ec.key").display().to_string(),
        enabled: true,
        ..Default::default()
    };
    for certs in [
        vec![p12("client-rsa-aes.p12", "localhost")],
        vec![p12("client-rsa-legacy.p12", &format!("localhost:{}", f.port))],
        vec![p12("client-ec-aes.pfx", "*")],
        // The exact host beats the `*` fallback (whose file does not exist).
        vec![ClientCertificate { pfx_path: "/missing.p12".into(), ..p12("", "*") }, pem],
    ] {
        let t = f.target(certs.clone());
        let schema = load_via_reflection(&t).await.unwrap_or_else(|e| panic!("{certs:?}: {e}"));
        let resp = invoke_unary(&t, &schema, SVC, "SayHello", r#"{"name":"mtls"}"#).await.unwrap();
        assert!(resp.json.contains("Hello, mtls!"), "{}", resp.json);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_or_unusable_client_certificate() {
    let f = server().await;
    // No certificate for this host: the server refuses the connection and
    // the error explains why.
    let t = f.target(vec![p12("client-rsa-aes.p12", "other.example.com")]);
    let err = load_via_reflection(&t).await.unwrap_err();
    assert!(matches!(err, GrpcError::Connect(_) | GrpcError::Tls(_)), "{err:?}");
    assert!(
        err.to_string().contains("client certificate") && err.to_string().contains("Settings > Certificates"),
        "{err}"
    );
    let schema = load_proto_files(
        &[std::path::PathBuf::from("helloworld/greeter.proto")],
        &[std::path::PathBuf::from(".")],
        Some(&protos_dir()),
    )
    .unwrap();
    let err = invoke_unary(&t, &schema, SVC, "SayHello", "{}").await.unwrap_err();
    assert!(
        err.to_string().contains("client certificate") && err.to_string().contains("Settings > Certificates"),
        "{err}"
    );

    // A disabled entry is ignored.
    let mut off = p12("client-rsa-aes.p12", "localhost");
    off.enabled = false;
    assert!(load_via_reflection(&f.target(vec![off])).await.is_err());

    // Passphrase problems are reported before connecting, without the password.
    let mut wrong = p12("client-rsa-aes.p12", "localhost");
    wrong.passphrase = "not-it".into();
    let err = load_via_reflection(&f.target(vec![wrong])).await.unwrap_err();
    assert!(matches!(err, GrpcError::Tls(_)), "{err:?}");
    assert!(err.to_string().contains("wrong password") && !err.to_string().contains("not-it"), "{err}");
    let mut unresolved = p12("client-rsa-aes.p12", "localhost");
    unresolved.passphrase = "{{vault:nope}}".into();
    let err = load_via_reflection(&f.target(vec![unresolved])).await.unwrap_err();
    assert!(err.to_string().contains("\"nope\""), "{err}");

    // Without the CA setting the self-signed server certificate is rejected.
    let mut t = f.target(vec![p12("client-rsa-aes.p12", "localhost")]);
    t.certificates.ca_enabled = false;
    let err = load_via_reflection(&t).await.unwrap_err();
    assert!(err.to_string().contains("certificate"), "{err}");
    // ... unless verification is off.
    t.verify_ssl = false;
    load_via_reflection(&t).await.unwrap();
}
