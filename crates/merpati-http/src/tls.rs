//! TLS setup shared by the HTTP and WebSocket clients.

use std::sync::{Arc, Once, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

static INSTALL: Once = Once::new();

/// Installs the process-wide rustls crypto provider (ring). Safe to call
/// many times.
pub fn install_provider() {
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The OS trust store, read once per process. Reading it walks the macOS
/// Security framework trust settings (hundreds of milliseconds, seconds under
/// load), which every WebSocket, Socket.IO, MQTT and gRPC connection paid
/// inside its connect timeout.
static ROOTS: OnceLock<rustls::RootCertStore> = OnceLock::new();

fn native_roots() -> &'static rustls::RootCertStore {
    ROOTS.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        roots
    })
}

/// Reads the OS trust store once, for a background thread.
pub(crate) fn read_native_roots() {
    native_roots();
}

/// Waits until the OS trust store is read, without blocking the async
/// runtime; connect timeouts started after it do not include the reading.
pub async fn load_native_roots() {
    if ROOTS.get().is_none() {
        let _ = tokio::task::spawn_blocking(native_roots).await;
    }
}

/// A rustls client config for WebSocket connections: system roots plus
/// optional extra PEM roots, or no verification at all.
pub fn client_config(verify: bool, extra_roots_pem: Option<&[u8]>) -> Result<rustls::ClientConfig, String> {
    client_config_with(verify, extra_roots_pem, None)
}

/// Like [`client_config`], presenting `identity` when the server asks for a
/// client certificate (mutual TLS).
pub fn client_config_with(
    verify: bool,
    extra_roots_pem: Option<&[u8]>,
    identity: Option<&crate::certs::ClientIdentity>,
) -> Result<rustls::ClientConfig, String> {
    install_provider();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let builder = if verify {
        let mut roots = native_roots().clone();
        if let Some(pem) = extra_roots_pem {
            for cert in rustls::pki_types::pem::PemObject::pem_slice_iter(pem) {
                let cert: CertificateDer<'static> = cert.map_err(|e| e.to_string())?;
                roots.add(cert).map_err(|e| e.to_string())?;
            }
        }
        builder.with_root_certificates(roots)
    } else {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
    };
    match identity {
        None => Ok(builder.with_no_client_auth()),
        Some(id) => builder
            .with_client_auth_cert(id.cert_chain.clone(), id.key.clone_key())
            .map_err(|e| format!("Invalid client certificate or key: {e}")),
    }
}

/// Extra trusted roots and the client certificate for one connection, taken
/// from the certificate settings like the HTTP client does (CA bundle when
/// enabled, the client certificate whose host pattern matches). Used by the
/// WebSocket, Socket.IO and MQTT clients.
#[derive(Clone, Debug, Default)]
pub struct TlsSetup {
    pub extra_roots_pem: Option<Vec<u8>>,
    pub identity: Option<crate::certs::ClientIdentity>,
}

impl PartialEq for TlsSetup {
    fn eq(&self, other: &Self) -> bool {
        let id = |i: &Option<crate::certs::ClientIdentity>| {
            i.as_ref().map(|i| (i.cert_chain.clone(), i.key.secret_der().to_vec()))
        };
        self.extra_roots_pem == other.extra_roots_pem && id(&self.identity) == id(&other.identity)
    }
}

impl Eq for TlsSetup {}

impl TlsSetup {
    /// Reads the CA bundle (only when `verify` is on, as for HTTP) and loads
    /// the client certificate configured for `host`/`port`, so that file and
    /// passphrase problems are reported before connecting.
    pub fn for_host(
        settings: &merpati_core::settings::CertificateSettings,
        secrets: Option<&crate::certs::SecretResolver>,
        verify: bool,
        host: &str,
        port: Option<u16>,
    ) -> Result<TlsSetup, String> {
        let mut setup = TlsSetup::default();
        if verify {
            setup.extra_roots_pem = crate::certs::extra_roots(settings)?;
        }
        if let Some(cert) = crate::certs::find_client_certificate(settings, host, port) {
            setup.identity = Some(crate::certs::load_client_identity(cert, secrets)?);
        }
        Ok(setup)
    }

    /// [`TlsSetup::for_host`] for the host and port of `url` when it uses
    /// TLS (`https`, `wss`, `mqtts`, `ssl`, `tls`). Plain URLs only get the
    /// CA bundle, for TLS to an HTTPS proxy (the HTTP engine trusts it there
    /// too); unparsable URLs get an empty setup (connecting reports them).
    pub fn for_url(
        settings: &merpati_core::settings::CertificateSettings,
        secrets: Option<&crate::certs::SecretResolver>,
        verify: bool,
        url: &str,
    ) -> Result<TlsSetup, String> {
        let raw = url.trim();
        let with_scheme = if raw.contains("://") { raw.to_string() } else { format!("mqtt://{raw}") };
        match url::Url::parse(&with_scheme) {
            Ok(u) if !matches!(u.scheme(), "https" | "wss" | "mqtts" | "ssl" | "tls") => {
                let extra_roots_pem = if verify { crate::certs::extra_roots(settings)? } else { None };
                Ok(TlsSetup { extra_roots_pem, identity: None })
            }
            Ok(u) => match u.host_str() {
                Some(host) => TlsSetup::for_host(settings, secrets, verify, host, u.port_or_known_default()),
                None => Ok(TlsSetup::default()),
            },
            Err(_) => Ok(TlsSetup::default()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.extra_roots_pem.is_none() && self.identity.is_none()
    }

    /// A rustls config with these roots and client certificate.
    pub fn client_config(&self, verify: bool) -> Result<rustls::ClientConfig, String> {
        client_config_with(verify, self.extra_roots_pem.as_deref(), self.identity.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_roots_are_read_once() {
        assert!(std::ptr::eq(native_roots(), native_roots()));
        // Extra roots are per config and never leak into the shared store.
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let before = native_roots().len();
        client_config(true, Some(ck.cert.pem().as_bytes())).unwrap();
        assert_eq!(native_roots().len(), before);
        let started = std::time::Instant::now();
        for _ in 0..20 {
            client_config(true, None).unwrap();
        }
        // Reading the OS trust store takes 100+ ms per call on macOS.
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    }
}
