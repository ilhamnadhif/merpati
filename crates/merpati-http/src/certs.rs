//! Client certificates (PRD FR-R8): PEM and PKCS#12 (`.p12` / `.pfx`)
//! identities for rustls, shared by the HTTP engine, gRPC and the other TLS
//! clients.
//!
//! PKCS#12 bundles are decoded in pure Rust (`p12-keystore`): PBES2
//! (PBKDF2 + AES-CBC, the OpenSSL 3 default) and the legacy PKCS#12 PBE
//! schemes (3DES and RC2-40, OpenSSL 1.x / Windows exports), with SHA-1 or
//! SHA-2 MACs. PEM keys may be plain (PKCS#8, PKCS#1, SEC1) or encrypted
//! PKCS#8 (`ENCRYPTED PRIVATE KEY`).
//!
//! Passphrases are not meant to live in `settings.json`: the
//! [`ClientCertificate::passphrase`] field holds a `{{vault:name}}`
//! reference that the app's [`SecretResolver`] turns into the password when
//! a connection needs it. A literal passphrase still works (it is then
//! stored in plain text by whoever saves the settings).

use std::fmt;
use std::sync::Arc;

use base64::Engine as _;
use merpati_core::settings::{CertificateSettings, ClientCertificate};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Expands secret references (`{{vault:name}}`, `{{var}}`) in a stored
/// passphrase. The app builds it from its variable resolver and the
/// workspace vault; unknown references must be returned unchanged.
#[derive(Clone)]
pub struct SecretResolver(Arc<dyn Fn(&str) -> String + Send + Sync>);

impl SecretResolver {
    pub fn new(resolve: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        SecretResolver(Arc::new(resolve))
    }

    pub fn resolve(&self, input: &str) -> String {
        (self.0)(input)
    }
}

impl fmt::Debug for SecretResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretResolver(..)")
    }
}

impl PartialEq for SecretResolver {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A certificate chain (leaf first) and its private key.
pub struct ClientIdentity {
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl Clone for ClientIdentity {
    fn clone(&self) -> Self {
        ClientIdentity { cert_chain: self.cert_chain.clone(), key: self.key.clone_key() }
    }
}

impl fmt::Debug for ClientIdentity {
    // Never prints the key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientIdentity").field("certificates", &self.cert_chain.len()).finish_non_exhaustive()
    }
}

impl ClientIdentity {
    /// The chain and key as one PEM document (for `reqwest::Identity::from_pem`).
    pub fn to_pem(&self) -> Vec<u8> {
        let mut out = String::new();
        for cert in &self.cert_chain {
            push_pem(&mut out, "CERTIFICATE", cert.as_ref());
        }
        let label = match &self.key {
            PrivateKeyDer::Pkcs1(_) => "RSA PRIVATE KEY",
            PrivateKeyDer::Sec1(_) => "EC PRIVATE KEY",
            _ => "PRIVATE KEY",
        };
        push_pem(&mut out, label, self.key.secret_der());
        out.into_bytes()
    }
}

fn push_pem(out: &mut String, label: &str, der: &[u8]) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    out.push_str(&format!("-----BEGIN {label}-----\n"));
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
}

/// Index of the enabled client certificate configured for `host[:port]`.
/// The most specific entry wins: exact host before `*.domain` before `*`,
/// and an entry with a port before one without; ties go to the first.
pub fn find_client_certificate_index(settings: &CertificateSettings, host: &str, port: Option<u16>) -> Option<usize> {
    let mut best: Option<(u8, usize)> = None;
    for (i, c) in settings.client_certs.iter().enumerate() {
        if !c.enabled || c.host.trim().is_empty() || !c.matches(host, port) {
            continue;
        }
        let pattern = c.host.trim();
        let (h, has_port) = match pattern.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|ch| ch.is_ascii_digit()) => (h, true),
            _ => (pattern, false),
        };
        let host_rank = if h == "*" {
            0
        } else if h.starts_with("*.") {
            1
        } else {
            2
        };
        let score = host_rank * 2 + u8::from(has_port);
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, i));
        }
    }
    best.map(|(_, i)| i)
}

/// The enabled client certificate configured for `host[:port]`, see
/// [`find_client_certificate_index`].
pub fn find_client_certificate<'a>(
    settings: &'a CertificateSettings,
    host: &str,
    port: Option<u16>,
) -> Option<&'a ClientCertificate> {
    find_client_certificate_index(settings, host, port).map(|i| &settings.client_certs[i])
}

/// The extra CA bundle (PEM) when enabled in the settings.
pub fn extra_roots(settings: &CertificateSettings) -> Result<Option<Vec<u8>>, String> {
    let path = settings.ca_path.trim();
    if !settings.ca_enabled || path.is_empty() {
        return Ok(None);
    }
    std::fs::read(path).map(Some).map_err(|e| format!("Could not read CA certificate file {path}: {e}"))
}

/// Turns the stored passphrase into the password: references are expanded
/// with `secrets`; an unresolved `{{vault:...}}` reference is an error.
pub fn resolve_passphrase(raw: &str, secrets: Option<&SecretResolver>) -> Result<String, String> {
    if !raw.contains("{{") {
        return Ok(raw.to_string());
    }
    let resolved = match secrets {
        Some(s) => s.resolve(raw),
        None => {
            return Err(format!(
                "The passphrase is a reference ({}) but no secret resolver is available to look it up",
                reference_names(raw)
            ));
        }
    };
    if let Some(start) = resolved.find("{{vault:") {
        let name = resolved[start + 8..].split("}}").next().unwrap_or_default().trim();
        return Err(format!("The passphrase refers to the vault secret \"{name}\", which is not set in the vault"));
    }
    Ok(resolved)
}

/// `{{vault:a}}` -> `{{vault:a}}` (only the references, never other text).
fn reference_names(raw: &str) -> String {
    let mut names = Vec::new();
    let mut rest = raw;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start..].find("}}") else { break };
        names.push(&rest[start..start + end + 2]);
        rest = &rest[start + end + 2..];
    }
    names.join(", ")
}

fn describe(cert: &ClientCertificate) -> String {
    match cert.host.trim() {
        "" => "the client certificate".into(),
        host => format!("the client certificate for {host}"),
    }
}

/// Loads a configured client certificate: the PKCS#12 bundle when
/// `pfx_path` is set, otherwise the PEM certificate (chain) and key (the key
/// may also be inside the certificate file). The passphrase is resolved
/// only when the bundle or key is encrypted.
pub fn load_client_identity(
    cert: &ClientCertificate,
    secrets: Option<&SecretResolver>,
) -> Result<ClientIdentity, String> {
    let what = describe(cert);
    let pfx = cert.pfx_path.trim();
    if !pfx.is_empty() {
        let data = std::fs::read(pfx).map_err(|e| format!("Could not read PKCS#12 file {pfx} of {what}: {e}"))?;
        let password = resolve_passphrase(&cert.passphrase, secrets).map_err(|e| format!("{e} ({what})"))?;
        return load_pkcs12(&data, &password).map_err(|e| format!("Invalid PKCS#12 file {pfx} of {what}: {e}"));
    }
    let cert_path = cert.cert_path.trim();
    if cert_path.is_empty() {
        return Err(format!("No certificate file is set for {what}"));
    }
    let mut pem =
        std::fs::read(cert_path).map_err(|e| format!("Could not read certificate file {cert_path} of {what}: {e}"))?;
    let key_path = cert.key_path.trim();
    if !key_path.is_empty() {
        pem.push(b'\n');
        pem.extend(std::fs::read(key_path).map_err(|e| format!("Could not read key file {key_path} of {what}: {e}"))?);
    }
    load_pem(&pem, || resolve_passphrase(&cert.passphrase, secrets)).map_err(|e| format!("{e} ({what})"))
}

/// Parses a PEM document holding the certificate chain and the private key.
/// `password` is called only for an encrypted PKCS#8 key.
pub fn load_pem(pem: &[u8], password: impl FnOnce() -> Result<String, String>) -> Result<ClientIdentity, String> {
    let cert_chain = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Invalid PEM certificate: {e}"))?;
    if cert_chain.is_empty() {
        return Err("No CERTIFICATE found in the PEM file".into());
    }
    let key = match PrivateKeyDer::from_pem_slice(pem) {
        Ok(key) => key,
        Err(rustls::pki_types::pem::Error::NoItemsFound) => {
            if let Some(der) = pem_section(pem, "ENCRYPTED PRIVATE KEY")? {
                decrypt_pkcs8(&der, &password()?)?
            } else if contains(pem, b"Proc-Type: 4,ENCRYPTED") {
                return Err("Keys encrypted in the traditional OpenSSL format are not supported; convert the key \
                     with `openssl pkcs8 -topk8 -in key.pem -out key-pkcs8.pem`"
                    .into());
            } else {
                return Err("No private key found (set the key file, or put the key in the certificate file)".into());
            }
        }
        Err(e) => return Err(format!("Invalid PEM private key: {e}")),
    };
    Ok(ClientIdentity { cert_chain, key })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The DER content of the first `-----BEGIN {label}-----` section.
fn pem_section(pem: &[u8], label: &str) -> Result<Option<Vec<u8>>, String> {
    let text = String::from_utf8_lossy(pem);
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let Some(start) = text.find(&begin) else { return Ok(None) };
    let body = &text[start + begin.len()..];
    let Some(stop) = body.find(&end) else { return Err(format!("Unterminated {label} PEM section")) };
    let b64: String = body[..stop].chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map(Some)
        .map_err(|e| format!("Invalid {label} PEM section: {e}"))
}

fn decrypt_pkcs8(der: &[u8], password: &str) -> Result<PrivateKeyDer<'static>, String> {
    let info =
        pkcs8::EncryptedPrivateKeyInfoRef::try_from(der).map_err(|e| format!("Invalid encrypted private key: {e}"))?;
    let doc = info.decrypt(password.as_bytes()).map_err(|e| match e {
        pkcs8::Error::EncryptedPrivateKey(pkcs8::pkcs5::Error::DecryptFailed) | pkcs8::Error::Asn1(_) => {
            "Could not decrypt the private key: wrong passphrase".to_string()
        }
        other => format!("Could not decrypt the private key: {other}"),
    })?;
    Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(doc.as_bytes().to_vec())))
}

/// Decodes a PKCS#12 bundle: the first private key with its certificate
/// chain (leaf first, then the issuers found in the bundle).
pub fn load_pkcs12(data: &[u8], password: &str) -> Result<ClientIdentity, String> {
    use p12_keystore::error::Error as E;
    let store =
        p12_keystore::KeyStore::from_pkcs12(data, password, p12_keystore::Pkcs12ImportPolicy::Strict).map_err(|e| {
            match e {
                E::MacError(_) | E::UnpadError => "wrong password (or the file is damaged)".to_string(),
                E::UnsupportedEncryptionScheme => "unsupported encryption scheme (supported: PBES2 with AES, \
                 PKCS#12 3DES and RC2-40)"
                    .to_string(),
                E::UnsupportedMacAlgorithm => "unsupported MAC algorithm".to_string(),
                E::DerError(d) => format!("not a PKCS#12 file ({d})"),
                other => other.to_string(),
            }
        })?;
    let (_, chain) = store
        .private_key_chain()
        .ok_or_else(|| "the bundle contains no private key with a matching certificate".to_string())?;
    let cert_chain = chain.certs().iter().map(|c| CertificateDer::from(c.as_der().to_vec())).collect::<Vec<_>>();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(chain.key().as_der().to_vec()));
    Ok(ClientIdentity { cert_chain, key })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(host: &str) -> ClientCertificate {
        ClientCertificate { host: host.into(), enabled: true, ..Default::default() }
    }

    #[test]
    fn most_specific_certificate_wins() {
        let settings = CertificateSettings {
            client_certs: vec![cert("*"), cert("*.example.com"), cert("api.example.com"), cert("api.example.com:8443")],
            ..Default::default()
        };
        assert_eq!(find_client_certificate_index(&settings, "api.example.com", Some(8443)), Some(3));
        assert_eq!(find_client_certificate_index(&settings, "api.example.com", Some(443)), Some(2));
        assert_eq!(find_client_certificate_index(&settings, "www.example.com", Some(443)), Some(1));
        assert_eq!(find_client_certificate_index(&settings, "other.test", None), Some(0));
        let mut off = settings.clone();
        off.client_certs[0].enabled = false;
        assert_eq!(find_client_certificate_index(&off, "other.test", None), None);
        assert_eq!(find_client_certificate_index(&CertificateSettings::default(), "x", None), None);
    }

    #[test]
    fn passphrase_references() {
        assert_eq!(resolve_passphrase("plain", None).unwrap(), "plain");
        assert_eq!(resolve_passphrase("", None).unwrap(), "");
        let err = resolve_passphrase("{{vault:p12}}", None).unwrap_err();
        assert!(err.contains("{{vault:p12}}") && err.contains("no secret resolver"), "{err}");
        let secrets = SecretResolver::new(|s| s.replace("{{vault:p12}}", "s3cret"));
        assert_eq!(resolve_passphrase("{{vault:p12}}", Some(&secrets)).unwrap(), "s3cret");
        let err = resolve_passphrase("{{vault:missing}}", Some(&secrets)).unwrap_err();
        assert!(err.contains("\"missing\"") && !err.contains("s3cret"), "{err}");
        assert_eq!(secrets, secrets.clone());
        assert_ne!(secrets, SecretResolver::new(|s| s.to_string()));
    }
}
