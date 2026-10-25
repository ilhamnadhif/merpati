//! The local certificate authority used for HTTPS interception.
//!
//! The CA is a P-256 key plus a self-signed certificate. Leaf certificates
//! for intercepted hosts are generated on demand (one shared in-memory leaf
//! key, a fresh random serial per certificate) and cached per host.
//!
//! Merpati never writes the CA private key anywhere by itself: the host
//! application persists it through [`CaStore`] (the desktop app keeps it in
//! the OS keychain) or handles the PEM strings directly.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PublicKeyData, SanType, SerialNumber,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::{CertifiedKey, SigningKey};
use sha2::{Digest, Sha256};

use crate::error::ProxyError;

/// Leaf certificates kept in memory before the cache is reset.
const LEAF_CACHE_LIMIT: usize = 2048;
/// Leaf validity. Apple platforms reject server certificates valid for more
/// than 825 days even under user-installed roots; one year is safe everywhere.
const LEAF_VALIDITY_DAYS: i64 = 365;
const CA_VALIDITY_DAYS: i64 = 3650;

/// The CA certificate and private key in PEM form, as persisted by the host.
#[derive(Clone, PartialEq, Eq)]
pub struct CaPem {
    pub cert_pem: String,
    /// PKCS#8 PEM ("BEGIN PRIVATE KEY"). Keep it in the keychain, never in
    /// the workspace.
    pub key_pem: String,
}

impl fmt::Debug for CaPem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaPem").field("cert_pem", &self.cert_pem).field("key_pem", &"<redacted>").finish()
    }
}

/// Where the host application keeps the CA between runs.
pub trait CaStore: Send + Sync {
    /// Returns the stored CA, or `None` when nothing has been stored yet.
    fn load(&self) -> Result<Option<CaPem>, String>;
    /// Persists a newly generated CA.
    fn save(&self, ca: &CaPem) -> Result<(), String>;
}

/// An in-memory [`CaStore`], for tests and throwaway sessions.
#[derive(Default)]
pub struct MemoryCaStore {
    inner: Mutex<Option<CaPem>>,
}

impl MemoryCaStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> Option<CaPem> {
        self.inner.lock().clone()
    }
}

impl CaStore for MemoryCaStore {
    fn load(&self) -> Result<Option<CaPem>, String> {
        Ok(self.inner.lock().clone())
    }

    fn save(&self, ca: &CaPem) -> Result<(), String> {
        *self.inner.lock() = Some(ca.clone());
        Ok(())
    }
}

/// A local root CA able to mint leaf certificates for intercepted hosts.
pub struct CertificateAuthority {
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    key_pem: String,
    issuer: Issuer<'static, KeyPair>,
    common_name: String,
    not_before: DateTime<Utc>,
    not_after: DateTime<Utc>,
    leaf_key: KeyPair,
    leaf_signer: Arc<dyn SigningKey>,
    leaves: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl fmt::Debug for CertificateAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertificateAuthority")
            .field("common_name", &self.common_name)
            .field("not_after", &self.not_after)
            .field("fingerprint_sha256", &self.fingerprint_sha256())
            .finish_non_exhaustive()
    }
}

impl CertificateAuthority {
    /// Generates a brand new CA (valid for ten years).
    pub fn generate() -> Result<Self, ProxyError> {
        let key = KeyPair::generate()?;
        let now = time::OffsetDateTime::now_utc();
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, format!("Merpati Proxy CA {}", Utc::now().format("%Y-%m-%d %H:%M:%S")));
        dn.push(DnType::OrganizationName, "Merpati");
        dn.push(DnType::OrganizationalUnitName, "Local capture proxy");
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages =
            vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(CA_VALIDITY_DAYS);
        params.serial_number = Some(random_serial());
        let cert = params.self_signed(&key)?;
        Self::from_pem(&cert.pem(), &key.serialize_pem())
    }

    /// Loads a CA from its certificate and PKCS#8 private key PEM. Fails when
    /// the certificate is not a CA or the key does not belong to it.
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, ProxyError> {
        let key = KeyPair::from_pem(key_pem).map_err(|e| ProxyError::Ca(format!("invalid CA private key: {e}")))?;
        let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|e| ProxyError::Ca(format!("invalid CA certificate PEM: {e}")))?;
        let (common_name, not_before, not_after) = {
            let (_, x509) = x509_parser::parse_x509_certificate(cert_der.as_ref())
                .map_err(|e| ProxyError::Ca(format!("invalid CA certificate: {e}")))?;
            if !x509.is_ca() {
                return Err(ProxyError::Ca("the certificate is not a certificate authority".into()));
            }
            if x509.public_key().raw != key.subject_public_key_info().as_slice() {
                return Err(ProxyError::Ca("the private key does not match the CA certificate".into()));
            }
            let cn = x509
                .subject()
                .iter_common_name()
                .next()
                .and_then(|c| c.as_str().ok())
                .unwrap_or("Merpati Proxy CA")
                .to_owned();
            let nb = DateTime::from_timestamp(x509.validity().not_before.timestamp(), 0).unwrap_or_default();
            let na = DateTime::from_timestamp(x509.validity().not_after.timestamp(), 0).unwrap_or_default();
            (cn, nb, na)
        };
        let issuer = Issuer::from_ca_cert_der(&cert_der, key)?;
        let leaf_key = KeyPair::generate()?;
        let leaf_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let leaf_signer =
            rustls::crypto::ring::sign::any_supported_type(&leaf_der).map_err(|e| ProxyError::Tls(e.to_string()))?;
        let cert_pem = pem_of(cert_der.as_ref());
        Ok(CertificateAuthority {
            cert_pem,
            cert_der,
            key_pem: key_pem.to_owned(),
            issuer,
            common_name,
            not_before,
            not_after,
            leaf_key,
            leaf_signer,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    /// Loads the CA from `store`, or generates and saves a new one when the
    /// store is empty or holds an expired CA. The flag is `true` when a new CA
    /// was created (the user has to trust it before interception works).
    pub fn load_or_generate(store: &dyn CaStore) -> Result<(Self, bool), ProxyError> {
        if let Some(pem) = store.load().map_err(ProxyError::CaStore)? {
            let ca = Self::from_pem(&pem.cert_pem, &pem.key_pem)?;
            if !ca.is_expired() {
                return Ok((ca, false));
            }
        }
        let ca = Self::generate()?;
        store.save(&ca.to_pem()).map_err(ProxyError::CaStore)?;
        Ok((ca, true))
    }

    /// Certificate and key PEM for persisting.
    pub fn to_pem(&self) -> CaPem {
        CaPem { cert_pem: self.cert_pem.clone(), key_pem: self.key_pem.clone() }
    }

    /// The CA certificate (PEM) the user installs as trusted.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The CA certificate in DER form (`.crt`/`.cer` for iOS and Android).
    pub fn cert_der(&self) -> &[u8] {
        self.cert_der.as_ref()
    }

    pub fn common_name(&self) -> &str {
        &self.common_name
    }

    pub fn not_before(&self) -> DateTime<Utc> {
        self.not_before
    }

    pub fn not_after(&self) -> DateTime<Utc> {
        self.not_after
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.not_after
    }

    /// SHA-256 fingerprint of the certificate, `AB:CD:...`.
    pub fn fingerprint_sha256(&self) -> String {
        let digest = Sha256::digest(self.cert_der.as_ref());
        digest.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
    }

    /// Whether the OS trust store (keychain on macOS) currently lists this CA
    /// as trusted for TLS. Read-only; blocking and fairly slow (tens of ms),
    /// call it off the UI thread.
    pub fn is_trusted_by_system(&self) -> bool {
        rustls_native_certs::load_native_certs().certs.iter().any(|c| c.as_ref() == self.cert_der.as_ref())
    }

    /// Human instructions for trusting the CA, with `cert_path` being where
    /// the user saved [`cert_pem`](Self::cert_pem). Merpati never changes the
    /// trust store itself.
    pub fn trust_instructions(&self, cert_path: &str) -> String {
        trust_instructions(&self.common_name, cert_path)
    }

    /// The leaf certificate for `host` (DNS name or IP), generated on first use.
    pub fn leaf_for(&self, host: &str) -> Result<Arc<CertifiedKey>, ProxyError> {
        let host = normalize_host(host);
        if let Some(ck) = self.leaves.lock().get(&host) {
            return Ok(ck.clone());
        }
        let ck = Arc::new(self.issue_leaf(&host)?);
        let mut cache = self.leaves.lock();
        if cache.len() >= LEAF_CACHE_LIMIT {
            cache.clear();
        }
        Ok(cache.entry(host).or_insert(ck).clone())
    }

    fn issue_leaf(&self, host: &str) -> Result<CertifiedKey, ProxyError> {
        let mut params = CertificateParams::default();
        let san = match host.parse::<IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(
                host.to_owned().try_into().map_err(|e| ProxyError::Ca(format!("invalid host name {host:?}: {e}")))?,
            ),
        };
        params.subject_alt_names = vec![san];
        let mut dn = DistinguishedName::new();
        // CN is limited to 64 characters; the SAN carries the real name.
        dn.push(DnType::CommonName, host.chars().take(64).collect::<String>());
        dn.push(DnType::OrganizationName, "Merpati capture proxy");
        params.distinguished_name = dn;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        let ca_end = time::OffsetDateTime::from_unix_timestamp(self.not_after.timestamp()).unwrap_or(now);
        params.not_after = (now + time::Duration::days(LEAF_VALIDITY_DAYS)).min(ca_end);
        params.serial_number = Some(random_serial());
        let cert = params.signed_by(&self.leaf_key, &self.issuer)?;
        Ok(CertifiedKey::new(vec![cert.der().clone()], self.leaf_signer.clone()))
    }
}

/// Lowercased host without brackets or a trailing dot.
pub(crate) fn normalize_host(host: &str) -> String {
    host.trim().trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase()
}

fn random_serial() -> SerialNumber {
    let mut bytes: [u8; 16] = rand::random();
    // Positive and non-zero leading byte keeps the DER INTEGER minimal and positive.
    bytes[0] = (bytes[0] & 0x7f) | 0x01;
    SerialNumber::from_slice(&bytes)
}

fn pem_of(der: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

/// Instructions for trusting the Merpati CA on common platforms. See the
/// crate documentation for the same text.
pub fn trust_instructions(common_name: &str, cert_path: &str) -> String {
    format!(
        "Trust the Merpati capture CA \"{common_name}\" so HTTPS can be inspected.\n\
         Only do this on machines and devices you own; remove it when you are done.\n\
         \n\
         macOS (current user, asks for your login password):\n\
         \x20 security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db \"{cert_path}\"\n\
         macOS (all users):\n\
         \x20 sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain \"{cert_path}\"\n\
         Remove it again:\n\
         \x20 security delete-certificate -c \"{common_name}\" ~/Library/Keychains/login.keychain-db\n\
         \n\
         Or open Keychain Access, import the file, double-click the certificate and set\n\
         \"When using this certificate\" to \"Always Trust\".\n\
         \n\
         Firefox uses its own store: Settings > Privacy & Security > Certificates > View Certificates >\n\
         Authorities > Import (or set security.enterprise_roots.enabled to true).\n\
         curl: --cacert \"{cert_path}\"   Node.js: NODE_EXTRA_CA_CERTS=\"{cert_path}\"\n\
         Python requests: REQUESTS_CA_BUNDLE=\"{cert_path}\" (replaces the default bundle)\n\
         \n\
         iOS / Android: point the device's Wi-Fi proxy at this computer (the proxy must listen on the LAN),\n\
         open http://merpati.proxy/ in the device browser and download the certificate.\n\
         iOS: install the profile, then enable it under Settings > General > About > Certificate Trust Settings.\n\
         Android: Settings > Security > Encryption & credentials > Install a certificate > CA certificate\n\
         (apps only trust user CAs when their network security config allows it).\n"
    )
}
