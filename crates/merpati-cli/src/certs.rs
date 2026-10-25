//! newman's client certificate options as entries of the certificate
//! settings (`settings.certificates.client_certs`).
//!
//! `--ssl-client-cert`/`--ssl-client-key`/`--ssl-client-passphrase` apply to
//! every host; `--ssl-client-cert-list` is newman's JSON list
//! (`[{name, matches: ["https://host:port/*"], cert: {src}, key: {src},
//! pfx: {src}, passphrase}]`) whose entries take precedence over them, the
//! global certificate being the fallback (newman README).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use merpati_core::settings::ClientCertificate;

#[derive(Default)]
pub struct CertArgs<'a> {
    pub cert: Option<&'a Path>,
    pub key: Option<&'a Path>,
    pub passphrase: Option<&'a str>,
    pub list: Option<&'a Path>,
    /// Relative certificate paths resolve against this directory
    /// (`--working-dir`), else the current directory, as in newman.
    pub base: Option<&'a Path>,
}

/// The client certificates of the command line, most important first.
pub fn client_certificates(args: &CertArgs<'_>) -> Result<Vec<ClientCertificate>> {
    let mut out = Vec::new();
    if let Some(list) = args.list {
        let text = std::fs::read_to_string(list)
            .with_context(|| format!("could not read --ssl-client-cert-list {}", list.display()))?;
        out = parse_list(&text, args.base)
            .with_context(|| format!("invalid --ssl-client-cert-list {}", list.display()))?;
    }
    if let Some(cert) = args.cert {
        let cert = file(&cert.display().to_string(), args.base, "--ssl-client-cert")?;
        let pkcs12 = Path::new(&cert)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("p12") || e.eq_ignore_ascii_case("pfx"));
        let key = match args.key {
            Some(k) => file(&k.display().to_string(), args.base, "--ssl-client-key")?,
            None => String::new(),
        };
        // `*` ranks below the host patterns of the list, so it is the fallback.
        out.push(ClientCertificate {
            host: "*".into(),
            cert_path: if pkcs12 { String::new() } else { cert.clone() },
            pfx_path: if pkcs12 { cert } else { String::new() },
            key_path: key,
            passphrase: args.passphrase.unwrap_or_default().to_string(),
            enabled: true,
        });
    }
    Ok(out)
}

/// An existing file, resolved against `base`.
fn file(path: &str, base: Option<&Path>, what: &str) -> Result<String> {
    let p = PathBuf::from(path);
    let p = match base {
        Some(b) if p.is_relative() => b.join(p),
        _ => p,
    };
    if !p.is_file() {
        bail!("{what}: {} does not exist", p.display());
    }
    Ok(p.display().to_string())
}

fn parse_list(text: &str, base: Option<&Path>) -> Result<Vec<ClientCertificate>> {
    let v: Value = serde_json::from_str(text)?;
    let Value::Array(entries) = v else { bail!("expected a JSON array of certificates") };
    let mut out = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let name = e.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("#{}", i + 1));
        // `{"src": "path"}`, or the path itself.
        let src = |key: &str| -> Result<String> {
            let path = match e.get(key) {
                Some(Value::Object(o)) => o.get("src").and_then(Value::as_str),
                Some(Value::String(s)) => Some(s.as_str()),
                _ => None,
            };
            match path.map(str::trim).filter(|p| !p.is_empty()) {
                Some(p) => file(p, base, &format!("certificate \"{name}\" {key}.src")),
                None => Ok(String::new()),
            }
        };
        let (cert_path, key_path, pfx_path) = (src("cert")?, src("key")?, src("pfx")?);
        if cert_path.is_empty() && pfx_path.is_empty() {
            bail!("certificate \"{name}\" has neither cert.src nor pfx.src");
        }
        let matches = e
            .get("matches")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("certificate \"{name}\" needs `matches`, a list of URL patterns"))?;
        let template = ClientCertificate {
            host: String::new(),
            cert_path,
            key_path,
            pfx_path,
            passphrase: e.get("passphrase").and_then(Value::as_str).unwrap_or_default().to_string(),
            enabled: true,
        };
        for m in matches {
            let pattern = m.as_str().ok_or_else(|| anyhow!("certificate \"{name}\": `matches` must hold strings"))?;
            if let Some(host) = pattern_host(pattern) {
                out.push(ClientCertificate { host, ..template.clone() });
            }
        }
    }
    Ok(out)
}

/// The `host[:port]` a URL match pattern (`https://*.example.com/*`,
/// `https://localhost:8443/*`, `<all_urls>`) stands for. As in
/// postman-collection's `UrlMatchPattern`, a pattern without a port matches
/// the default port only (443) and `:*` any port; the path cannot select a
/// certificate (one per TLS connection). Patterns for other schemes than
/// `https`/`*` never apply to TLS connections and give `None`.
fn pattern_host(pattern: &str) -> Option<String> {
    let p = pattern.trim();
    if p == "<all_urls>" {
        return Some("*".into());
    }
    let (scheme, rest) = p.split_once("://").unwrap_or(("https", p));
    if !(scheme == "*" || scheme.eq_ignore_ascii_case("https")) {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(':') && (p == "*" || p.chars().all(|c| c.is_ascii_digit())) => (h, Some(p)),
        _ => (authority, None),
    };
    // IPv6 hosts keep their brackets, as in the URLs they are matched with.
    let host = host.to_ascii_lowercase();
    match (host.as_str(), port) {
        ("", _) => None,
        // Merpati's `*` entry matches any host and port.
        ("*", _) => Some("*".into()),
        (h, Some("*")) => Some(h.to_string()),
        (h, Some(p)) => Some(format!("{h}:{p}")),
        (h, None) => Some(format!("{h}:443")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_patterns() {
        assert_eq!(pattern_host("https://api.example.com/*").as_deref(), Some("api.example.com:443"));
        assert_eq!(pattern_host("https://*.example.com:8443/v1/*").as_deref(), Some("*.example.com:8443"));
        assert_eq!(pattern_host("*://localhost:*/*").as_deref(), Some("localhost"));
        assert_eq!(pattern_host("https://*/*").as_deref(), Some("*"));
        assert_eq!(pattern_host("<all_urls>").as_deref(), Some("*"));
        assert_eq!(pattern_host("HTTPS://API.Example.com").as_deref(), Some("api.example.com:443"));
        assert_eq!(pattern_host("https://[::1]:9443/*").as_deref(), Some("[::1]:9443"));
        assert_eq!(pattern_host("http://api.example.com/*"), None);
        assert_eq!(pattern_host("https:///x"), None);
    }

    #[test]
    fn list_and_global_certificate() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["a.pem", "a.key", "b.p12", "global.pem"] {
            std::fs::write(dir.path().join(f), "x").unwrap();
        }
        let list = dir.path().join("list.json");
        std::fs::write(
            &list,
            r#"[
                {"name": "a", "matches": ["https://a.test/*", "https://*.a.test:8443/*"],
                 "cert": {"src": "a.pem"}, "key": {"src": "a.key"}, "passphrase": "pw"},
                {"matches": ["https://b.test:*/*", "http://b.test/*"], "pfx": {"src": "b.p12"}}
            ]"#,
        )
        .unwrap();
        let global = dir.path().join("global.pem");
        let args = CertArgs {
            cert: Some(Path::new("global.pem")),
            passphrase: Some("g"),
            list: Some(&list),
            base: Some(dir.path()),
            ..Default::default()
        };
        let certs = client_certificates(&args).unwrap();
        let hosts: Vec<&str> = certs.iter().map(|c| c.host.as_str()).collect();
        assert_eq!(hosts, vec!["a.test:443", "*.a.test:8443", "b.test", "*"]);
        assert!(certs.iter().all(|c| c.enabled));
        assert_eq!(certs[0].key_path, dir.path().join("a.key").display().to_string());
        assert_eq!(certs[0].passphrase, "pw");
        assert_eq!(certs[2].pfx_path, dir.path().join("b.p12").display().to_string());
        assert_eq!((certs[3].cert_path.as_str(), certs[3].passphrase.as_str()), (global.to_str().unwrap(), "g"));
        // The most specific entry wins; the global one is the fallback.
        let settings = merpati_core::settings::CertificateSettings { client_certs: certs, ..Default::default() };
        let pick = |h: &str, p: u16| merpati_http::certs::find_client_certificate_index(&settings, h, Some(p));
        assert_eq!(pick("a.test", 443), Some(0));
        assert_eq!(pick("a.test", 8443), Some(3));
        assert_eq!(pick("x.a.test", 8443), Some(1));
        assert_eq!(pick("b.test", 9000), Some(2));

        for (bad, why) in [
            (r#"{"matches": []}"#, "array"),
            (r#"[{"name": "n", "matches": ["https://x/*"]}]"#, "neither"),
            (r#"[{"name": "n", "cert": {"src": "a.pem"}}]"#, "matches"),
            (r#"[{"name": "n", "cert": {"src": "nope.pem"}, "matches": []}]"#, "does not exist"),
        ] {
            std::fs::write(&list, bad).unwrap();
            let err = format!("{:#}", client_certificates(&args).unwrap_err());
            assert!(err.contains(why), "{bad}: {err}");
        }
    }
}
