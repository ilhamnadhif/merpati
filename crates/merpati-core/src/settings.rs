//! Application settings (per user, not per workspace): request defaults,
//! proxy, certificates and UI preferences.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::HttpVersion;

pub const USER_AGENT: &str = concat!("MerpatiRuntime/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestDefaults {
    /// 0 = no timeout.
    pub timeout_ms: u64,
    pub follow_redirects: bool,
    pub max_redirects: u32,
    pub verify_ssl: bool,
    pub encode_url: bool,
    pub http_version: HttpVersion,
    pub send_user_agent: bool,
    pub send_no_cache: bool,
}

impl Default for RequestDefaults {
    fn default() -> Self {
        RequestDefaults {
            timeout_ms: 0,
            follow_redirects: true,
            max_redirects: 10,
            verify_ssl: true,
            encode_url: true,
            http_version: HttpVersion::Auto,
            send_user_agent: true,
            send_no_cache: false,
        }
    }
}

impl RequestDefaults {
    /// Headers added to every request unless the user set them or disabled
    /// them in the request settings.
    pub fn default_headers(&self) -> Vec<(&'static str, String)> {
        let mut h = Vec::new();
        if self.send_user_agent {
            h.push(("User-Agent", USER_AGENT.to_string()));
        }
        h.push(("Accept", "*/*".to_string()));
        if self.send_no_cache {
            h.push(("Cache-Control", "no-cache".to_string()));
        }
        h
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySettings {
    /// Honour `HTTP(S)_PROXY` / system proxy configuration.
    pub use_system: bool,
    /// Custom proxy, e.g. `http://proxy:3128` or `socks5://127.0.0.1:1080`.
    pub custom_url: String,
    pub custom_enabled: bool,
    pub username: String,
    pub password: String,
    /// Comma separated hosts that bypass the proxy.
    pub bypass: String,
    /// Proxy auto-config (PAC) script, `http(s)://` or `file://` URL or a
    /// file path. With the custom proxy enabled it chooses the proxy per
    /// request instead of `custom_url`; `username`/`password` apply to the
    /// proxies it picks.
    pub pac_url: String,
}

/// `url` without the password of its `user:pass@` part.
fn redact_userinfo(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), url),
    };
    match rest.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}***@{host}"),
        None => url.to_string(),
    }
}

impl std::fmt::Debug for ProxySettings {
    /// Without the password, nor the URL's `user:pass@` part.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url = redact_userinfo(&self.custom_url);
        f.debug_struct("ProxySettings")
            .field("use_system", &self.use_system)
            .field("custom_url", &url)
            .field("custom_enabled", &self.custom_enabled)
            .field("username", &self.username)
            .field("password", &if self.password.is_empty() { "" } else { "***" })
            .field("bypass", &self.bypass)
            .field("pac_url", &redact_userinfo(&self.pac_url))
            .finish()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientCertificate {
    /// Host (optionally with `:port`) this certificate is used for. `*` as a
    /// subdomain wildcard is allowed: `*.example.com`.
    pub host: String,
    /// PEM certificate (chain).
    pub cert_path: String,
    /// PEM private key.
    pub key_path: String,
    /// PKCS#12 bundle (`.p12` / `.pfx`) as an alternative to cert + key;
    /// used instead of them when set.
    pub pfx_path: String,
    /// Password of the PKCS#12 bundle or of an encrypted PEM key. Keep a
    /// `{{vault:name}}` reference here rather than the password itself (this
    /// file is plain JSON); references are resolved when connecting, see
    /// `merpati_http::certs::SecretResolver`.
    pub passphrase: String,
    pub enabled: bool,
}

impl ClientCertificate {
    /// The vault entry the passphrase refers to: `{{vault:name}}` -> `name`.
    pub fn passphrase_vault_name(&self) -> Option<&str> {
        let p = self.passphrase.trim().strip_prefix("{{")?.strip_suffix("}}")?.trim();
        p.strip_prefix("vault:").map(str::trim).filter(|n| !n.is_empty())
    }

    pub fn matches(&self, host: &str, port: Option<u16>) -> bool {
        if self.host.trim() == "*" {
            return true;
        }
        let (h, p) = match self.host.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, p.parse::<u16>().ok()),
            _ => (self.host.as_str(), None),
        };
        if let (Some(want), Some(have)) = (p, port)
            && want != have
        {
            return false;
        }
        let h = h.trim().to_ascii_lowercase();
        let host = host.to_ascii_lowercase();
        match h.strip_prefix("*.") {
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => h == host,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CertificateSettings {
    /// Extra CA certificates (PEM) trusted in addition to the system store.
    pub ca_path: String,
    pub ca_enabled: bool,
    pub client_certs: Vec<ClientCertificate>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Id,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseLayout {
    #[default]
    Below,
    Side,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub theme: ThemePreference,
    pub language: Language,
    pub font_size: f32,
    pub editor_font_size: f32,
    pub layout: ResponseLayout,
    pub sidebar_visible: bool,
    pub wrap_response: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        UiSettings {
            theme: ThemePreference::System,
            language: Language::En,
            font_size: 13.0,
            editor_font_size: 12.5,
            layout: ResponseLayout::Below,
            sidebar_visible: true,
            wrap_response: false,
        }
    }
}

/// Release feed used to find newer versions of the app (the releases of the
/// workspace's `repository`; keep the two in sync).
pub const DEFAULT_UPDATE_FEED: &str = "https://api.github.com/repos/ilhamnadhif/merpati/releases/latest";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// Look for a newer release when the app starts. The check sends no
    /// identifiers: one GET of the feed with the app version as User-Agent.
    pub check_on_startup: bool,
    /// GitHub Releases API URL (`…/releases/latest`) or a JSON manifest
    /// (`{"version", "notes", "url", "assets": [{"name", "url"}]}`). Empty
    /// means [`DEFAULT_UPDATE_FEED`], so a changed default reaches existing
    /// settings files.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub feed_url: String,
    /// A version the user chose to skip; it is not offered again.
    pub skipped_version: String,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        UpdateSettings { check_on_startup: true, feed_url: String::new(), skipped_version: String::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub request: RequestDefaults,
    pub proxy: ProxySettings,
    pub certificates: CertificateSettings,
    pub ui: UiSettings,
    pub updates: UpdateSettings,
    /// Response bodies larger than this are kept in a temporary file.
    pub max_body_in_memory_bytes: u64,
    /// Bodies larger than this are not pretty-printed/highlighted.
    pub max_pretty_bytes: u64,
    pub history_limit: usize,
    pub script_timeout_ms: u64,
    /// Memory a script process may use before its job is stopped (MB); 0
    /// means no limit.
    pub script_memory_mb: u64,
    /// esm.sh-compatible CDN for `pm.require('npm:...')` / `jsr:` packages.
    pub package_registry: String,
    pub recent_workspaces: Vec<PathBuf>,
    /// Custom keyboard shortcuts: action id -> shortcut text (`Cmd+Enter`).
    pub shortcuts: indexmap::IndexMap<String, String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            request: RequestDefaults::default(),
            proxy: ProxySettings { use_system: true, ..Default::default() },
            certificates: CertificateSettings::default(),
            ui: UiSettings::default(),
            updates: UpdateSettings::default(),
            max_body_in_memory_bytes: 8 * 1024 * 1024,
            max_pretty_bytes: 5 * 1024 * 1024,
            history_limit: 1000,
            script_timeout_ms: 30_000,
            script_memory_mb: 1024,
            package_registry: crate::script::DEFAULT_PACKAGE_REGISTRY.to_string(),
            recent_workspaces: Vec::new(),
            shortcuts: Default::default(),
        }
    }
}

impl AppSettings {
    /// `~/Library/Application Support/Merpati` on macOS, the XDG config dir on
    /// Linux, `%APPDATA%` on Windows.
    pub fn config_dir() -> PathBuf {
        dirs_config().join("Merpati")
    }

    pub fn default_path() -> PathBuf {
        Self::config_dir().join("settings.json")
    }

    pub fn load(path: &Path) -> AppSettings {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::storage::write_atomic(path, serde_json::to_string_pretty(self).unwrap_or_default().as_bytes())
    }

    pub fn push_recent(&mut self, path: &Path) {
        self.recent_workspaces.retain(|p| p != path);
        self.recent_workspaces.insert(0, path.to_path_buf());
        self.recent_workspaces.truncate(10);
    }
}

fn dirs_config() -> PathBuf {
    if let Ok(p) = std::env::var("MERPATI_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Application Support");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(app) = std::env::var_os("APPDATA") {
            return PathBuf::from(app);
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config");
    }
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_host_matching() {
        let c = ClientCertificate { host: "*.example.com:8443".into(), ..Default::default() };
        assert!(c.matches("api.example.com", Some(8443)));
        assert!(!c.matches("api.example.com", Some(443)));
        assert!(!c.matches("example.com", Some(8443)));
        let c = ClientCertificate { host: "localhost".into(), ..Default::default() };
        assert!(c.matches("LOCALHOST", Some(3000)));
        let c = ClientCertificate { host: "*".into(), ..Default::default() };
        assert!(c.matches("anything.test", None));
    }

    #[test]
    fn cert_passphrase_vault_name() {
        let c = |p: &str| ClientCertificate { passphrase: p.into(), ..Default::default() };
        assert_eq!(c("{{vault:client-p12}}").passphrase_vault_name(), Some("client-p12"));
        assert_eq!(c(" {{ vault: x }} ").passphrase_vault_name(), Some("x"));
        assert_eq!(c("literal").passphrase_vault_name(), None);
        assert_eq!(c("{{env}}").passphrase_vault_name(), None);
        assert_eq!(c("{{vault:}}").passphrase_vault_name(), None);
    }

    #[test]
    fn proxy_debug_hides_credentials() {
        let p = ProxySettings {
            custom_url: "http://bob:url-secret@proxy:3128".into(),
            password: "hunter2".into(),
            ..Default::default()
        };
        let d = format!("{p:?}");
        assert!(!d.contains("hunter2") && !d.contains("url-secret"), "{d}");
        assert!(d.contains("proxy:3128"), "{d}");
        let p = ProxySettings { custom_url: "bob:s3cret@proxy:3128".into(), ..Default::default() };
        assert!(!format!("{p:?}").contains("s3cret"));
        let p = ProxySettings { pac_url: "https://u:pac-secret@corp.test/proxy.pac".into(), ..Default::default() };
        assert!(!format!("{p:?}").contains("pac-secret"));
    }

    #[test]
    fn settings_tolerate_missing_fields() {
        let s: AppSettings = serde_json::from_str("{\"ui\": {\"theme\": \"dark\"}}").unwrap();
        assert_eq!(s.ui.theme, ThemePreference::Dark);
        assert_eq!(s.history_limit, 1000);
    }
}
