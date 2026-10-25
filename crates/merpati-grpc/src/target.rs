//! Where and how to connect: URL rules, TLS choice, metadata and deadline.

use std::time::Duration;

use base64::Engine as _;
use merpati_core::auth::{Auth, AuthType};
use merpati_core::protocols::GrpcRequest;
use merpati_core::settings::{CertificateSettings, ProxySettings};
use merpati_http::SecretResolver;
use tonic::metadata::{AsciiMetadataKey, AsciiMetadataValue, BinaryMetadataKey, BinaryMetadataValue, MetadataMap};

use crate::error::GrpcError;

/// A gRPC server plus per-call settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// `host:port` (plaintext unless `tls == Some(true)`), `grpc://host:port`
    /// (plaintext) or `grpcs://` / `https://host:port` (TLS).
    pub url: String,
    /// Forces TLS on or off; `None` decides by scheme.
    pub tls: Option<bool>,
    pub verify_ssl: bool,
    /// Request metadata. Keys ending in `-bin` carry base64 binary values.
    pub metadata: Vec<(String, String)>,
    /// Deadline for the whole call (connect included). Reflection and
    /// connecting use it as their timeout too.
    pub timeout: Option<Duration>,
    /// The app's certificate settings (`AppSettings::certificates`): the
    /// extra CA is trusted when verifying, and the client certificate
    /// configured for the target host (PEM or PKCS#12) is presented to
    /// servers that ask for one (mutual TLS). Only used for TLS targets.
    pub certificates: CertificateSettings,
    /// Resolves `{{vault:name}}` passphrases of client certificates.
    pub secret_resolver: Option<SecretResolver>,
    /// The app's proxy settings (`AppSettings::proxy`), applied like the
    /// HTTP engine does (see `merpati_http::proxy`): the HTTP/2 connection
    /// is tunnelled through the proxy, TLS runs end to end inside it. The
    /// default connects directly.
    pub proxy: ProxySettings,
}

impl Default for Target {
    fn default() -> Self {
        Target {
            url: String::new(),
            tls: None,
            verify_ssl: true,
            metadata: Vec::new(),
            timeout: None,
            certificates: CertificateSettings::default(),
            secret_resolver: None,
            proxy: ProxySettings::default(),
        }
    }
}

impl Target {
    pub fn new(url: impl Into<String>) -> Target {
        Target { url: url.into(), ..Target::default() }
    }

    /// Builds a target from a saved request. `resolve` expands `{{variables}}`
    /// in the URL and metadata. Enabled metadata rows are used, and the
    /// request's own Bearer / Basic / API-key (header) auth is added as
    /// metadata unless a row with the same key exists (inherited auth must be
    /// added by the caller, see [`auth_metadata`]).
    pub fn from_request(req: &GrpcRequest, resolve: &dyn Fn(&str) -> String, default_verify_ssl: bool) -> Target {
        let mut metadata: Vec<(String, String)> = req
            .metadata
            .iter()
            .filter(|kv| kv.enabled && !kv.key.trim().is_empty())
            .map(|kv| (resolve(&kv.key).trim().to_string(), resolve(&kv.value)))
            .collect();
        for (k, v) in auth_metadata(&req.auth, resolve) {
            if !metadata.iter().any(|(mk, _)| mk.eq_ignore_ascii_case(&k)) {
                metadata.push((k, v));
            }
        }
        Target {
            url: resolve(&req.url).trim().to_string(),
            tls: req.tls,
            verify_ssl: req.verify_ssl.unwrap_or(default_verify_ssl),
            metadata,
            timeout: req.deadline_ms.filter(|ms| *ms > 0).map(Duration::from_millis),
            certificates: CertificateSettings::default(),
            secret_resolver: None,
            proxy: ProxySettings::default(),
        }
    }

    /// The same target with the app's certificate settings and passphrase
    /// resolver (see [`Target::certificates`]).
    pub fn with_certificates(mut self, certificates: CertificateSettings, secrets: Option<SecretResolver>) -> Target {
        self.certificates = certificates;
        self.secret_resolver = secrets;
        self
    }

    /// The same target with the app's proxy settings (see [`Target::proxy`]).
    pub fn with_proxy(mut self, proxy: ProxySettings) -> Target {
        self.proxy = proxy;
        self
    }
}

/// Metadata for the auth types that map directly onto a header: Bearer,
/// Basic and API key sent "in header". Other types yield nothing.
pub fn auth_metadata(auth: &Auth, resolve: &dyn Fn(&str) -> String) -> Vec<(String, String)> {
    match auth.kind {
        AuthType::Bearer => {
            let token = resolve(&auth.get_string("token"));
            if token.trim().is_empty() {
                vec![]
            } else {
                vec![("authorization".into(), format!("Bearer {}", token.trim()))]
            }
        }
        AuthType::Basic => {
            let user = resolve(&auth.get_string("username"));
            let pass = resolve(&auth.get_string("password"));
            if user.is_empty() && pass.is_empty() {
                vec![]
            } else {
                let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
                vec![("authorization".into(), format!("Basic {encoded}"))]
            }
        }
        AuthType::ApiKey => {
            let location = auth.get_string("in");
            let key = resolve(&auth.get_string("key")).trim().to_ascii_lowercase();
            if (location.is_empty() || location == "header") && !key.is_empty() {
                vec![(key, resolve(&auth.get_string("value")))]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// A parsed target address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Address {
    pub tls: bool,
    /// Host without IPv6 brackets, for DNS / TCP / SNI.
    pub host: String,
    pub port: u16,
    /// `host:port` as sent in `:authority` (IPv6 keeps its brackets).
    pub authority: String,
}

pub(crate) fn parse_address(url: &str, tls: Option<bool>) -> Result<Address, GrpcError> {
    let invalid = |reason: &str| GrpcError::InvalidUrl { url: url.to_string(), reason: reason.to_string() };
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(invalid("the URL is empty"));
    }
    let (scheme_tls, rest) = match trimmed.find("://") {
        Some(i) => {
            let scheme = trimmed[..i].to_ascii_lowercase();
            let rest = &trimmed[i + 3..];
            match scheme.as_str() {
                "grpc" | "http" | "h2c" => (false, rest),
                "grpcs" | "https" => (true, rest),
                "dns" => (false, rest.trim_start_matches('/')),
                _ => return Err(invalid(&format!("unsupported scheme \"{scheme}\" (use grpc:// or grpcs://)"))),
            }
        }
        None => (false, trimmed),
    };
    let use_tls = tls.unwrap_or(scheme_tls);
    // Anything after the authority (a path) is ignored: gRPC paths are
    // always /package.Service/Method.
    let authority_str = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority_str.is_empty() {
        return Err(invalid("missing host"));
    }
    if authority_str.contains('@') {
        return Err(invalid("credentials in the URL are not supported; use metadata instead"));
    }
    let authority: http::uri::Authority = authority_str.parse().map_err(|_| invalid("not a valid host:port"))?;
    let host_raw = authority.host();
    let host = host_raw.trim_start_matches('[').trim_end_matches(']').to_string();
    if host.is_empty() {
        return Err(invalid("missing host"));
    }
    let port = match authority.port_u16() {
        Some(p) => p,
        None if authority_str.ends_with(':') => return Err(invalid("missing port")),
        None => {
            if use_tls {
                443
            } else {
                80
            }
        }
    };
    Ok(Address { tls: use_tls, host, port, authority: format!("{host_raw}:{port}") })
}

/// Converts metadata pairs into a tonic map. `-bin` values are base64
/// (standard or URL-safe, padding optional).
pub(crate) fn build_metadata(pairs: &[(String, String)]) -> Result<MetadataMap, GrpcError> {
    let mut map = MetadataMap::new();
    for (key, value) in pairs {
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let bad = |reason: String| GrpcError::InvalidMetadata { key: key.clone(), reason };
        if key.starts_with("grpc-") {
            return Err(bad("keys starting with \"grpc-\" are reserved".into()));
        }
        if key.ends_with("-bin") {
            let bytes = decode_base64(value.trim()).ok_or_else(|| bad("binary (-bin) values must be base64".into()))?;
            let k = BinaryMetadataKey::from_bytes(key.as_bytes()).map_err(|e| bad(e.to_string()))?;
            map.append_bin(k, BinaryMetadataValue::from_bytes(&bytes));
        } else {
            let k = AsciiMetadataKey::from_bytes(key.as_bytes()).map_err(|e| bad(e.to_string()))?;
            let v: AsciiMetadataValue = value
                .trim()
                .parse()
                .map_err(|_| bad("values cannot contain control characters (use a -bin key for binary)".into()))?;
            map.append(k, v);
        }
    }
    Ok(map)
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD].iter().find_map(|e| e.decode(s).ok())
}

/// Metadata as display pairs. Binary values are shown as standard base64;
/// `grpc-status` / `grpc-message` / `grpc-status-details-bin` are dropped
/// (they are reported as the call status).
pub(crate) fn metadata_pairs(map: &MetadataMap) -> Vec<(String, String)> {
    use tonic::metadata::KeyAndValueRef;
    let mut out = Vec::new();
    for kv in map.iter() {
        match kv {
            KeyAndValueRef::Ascii(k, v) => {
                let k = k.as_str();
                if matches!(k, "grpc-status" | "grpc-message") {
                    continue;
                }
                out.push((k.to_string(), String::from_utf8_lossy(v.as_encoded_bytes()).into_owned()));
            }
            KeyAndValueRef::Binary(k, v) => {
                let k = k.as_str();
                if k == "grpc-status-details-bin" {
                    continue;
                }
                let value = match v.to_bytes() {
                    Ok(bytes) => base64::engine::general_purpose::STANDARD.encode(bytes),
                    Err(_) => String::from_utf8_lossy(v.as_encoded_bytes()).into_owned(),
                };
                out.push((k.to_string(), value));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_rules() {
        let a = parse_address("localhost:50051", None).unwrap();
        assert_eq!(
            (a.tls, a.host.as_str(), a.port, a.authority.as_str()),
            (false, "localhost", 50051, "localhost:50051")
        );
        assert!(parse_address("localhost:50051", Some(true)).unwrap().tls);
        assert!(!parse_address("grpc://h:1", None).unwrap().tls);
        assert!(parse_address("grpcs://h:1", None).unwrap().tls);
        assert!(parse_address("https://h", None).unwrap().tls);
        assert_eq!(parse_address("https://h", None).unwrap().port, 443);
        assert!(!parse_address("grpcs://h:1", Some(false)).unwrap().tls);
        let v6 = parse_address("grpc://[::1]:9000/ignored", None).unwrap();
        assert_eq!((v6.host.as_str(), v6.authority.as_str()), ("::1", "[::1]:9000"));
        assert!(parse_address("ftp://h:1", None).is_err());
        assert!(parse_address("", None).is_err());
        assert!(parse_address("grpc://", None).is_err());
    }

    #[test]
    fn metadata_binary_and_errors() {
        let map = build_metadata(&[("X-Trace-Bin".into(), "AAEC".into()), ("x-user".into(), "bob".into())]).unwrap();
        assert_eq!(map.get_bin("x-trace-bin").unwrap().to_bytes().unwrap().as_ref(), &[0u8, 1, 2]);
        assert_eq!(metadata_pairs(&map), vec![("x-trace-bin".into(), "AAEC".into()), ("x-user".into(), "bob".into())]);
        assert!(build_metadata(&[("x-bin".into(), "!!!".into())]).is_err());
        assert!(build_metadata(&[("bad key".into(), "v".into())]).is_err());
        assert!(build_metadata(&[("x".into(), "line\nbreak".into())]).is_err());
    }

    #[test]
    fn from_request_applies_vars_and_auth() {
        let req = GrpcRequest {
            url: "{{host}}".into(),
            metadata: vec![
                merpati_core::KeyValue::new("x-a", "{{v}}"),
                merpati_core::KeyValue::new("x-off", "1").disabled(),
            ],
            deadline_ms: Some(1500),
            auth: Auth::bearer("{{tok}}"),
            ..Default::default()
        };
        let resolve = |s: &str| s.replace("{{host}}", "localhost:1").replace("{{v}}", "A").replace("{{tok}}", "T");
        let t = Target::from_request(&req, &resolve, false);
        assert_eq!(t.url, "localhost:1");
        assert!(!t.verify_ssl);
        assert_eq!(t.timeout, Some(Duration::from_millis(1500)));
        assert_eq!(t.metadata, vec![("x-a".into(), "A".into()), ("authorization".into(), "Bearer T".into())]);
    }
}
