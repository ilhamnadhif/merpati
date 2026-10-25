//! Proxy configuration and capture filters.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ca::CertificateAuthority;
use crate::entry::CaptureEntry;
use crate::error::ProxyError;
use crate::store::ProxyEvent;

/// Default listening port (`127.0.0.1:8877`).
pub const DEFAULT_PORT: u16 = 8877;

/// Called for every [`ProxyEvent`], from the proxy's tasks. Keep it cheap
/// (e.g. push into a channel and request a repaint).
pub type EventCallback = Arc<dyn Fn(&ProxyEvent) + Send + Sync>;

/// Everything [`CaptureProxy::start`](crate::CaptureProxy::start) needs.
#[derive(Clone)]
pub struct ProxyConfig {
    /// Listening address. Loopback unless `allow_lan` is set.
    pub bind: SocketAddr,
    /// Allow a non-loopback `bind` (e.g. `0.0.0.0` so a phone on the LAN can
    /// use the proxy). Anyone who can reach the port can then use it.
    pub allow_lan: bool,
    /// The CA that signs intercepted hosts' certificates. Without it all
    /// HTTPS traffic is tunnelled.
    pub ca: Option<Arc<CertificateAuthority>>,
    /// Decrypt HTTPS (CONNECT) traffic. Needs `ca`.
    pub intercept_tls: bool,
    /// Host patterns that are never decrypted, only tunnelled (and recorded
    /// as tunnels). Same syntax as [`CaptureFilter::include_hosts`].
    pub passthrough_hosts: Vec<String>,
    /// After a client rejects the Merpati certificate for a host (pinning or
    /// an untrusted CA), tunnel that host for the rest of the session.
    pub passthrough_on_tls_error: bool,
    /// Verify upstream server certificates (system roots plus
    /// `extra_upstream_roots_pem`). Turn off per capture session for
    /// self-signed development servers.
    pub verify_upstream_tls: bool,
    /// Extra PEM roots trusted for upstream servers.
    pub extra_upstream_roots_pem: Option<String>,
    /// Offer HTTP/2 to clients on intercepted connections.
    pub client_http2: bool,
    /// Offer HTTP/2 to upstream HTTPS servers.
    pub upstream_http2: bool,
    /// Chain every upstream connection through this HTTP proxy (CONNECT).
    pub upstream_proxy: Option<UpstreamProxy>,
    /// What gets recorded. Can be changed while running.
    pub filter: CaptureFilter,
    /// Maximum number of entries kept (oldest are evicted).
    pub capacity: usize,
    /// Bytes of each request/response body kept in memory; the rest is
    /// streamed through and only counted (`truncated` is set).
    pub max_body_bytes: usize,
    /// Total body bytes kept across all entries, including the bodies still
    /// streaming; oldest entries are evicted beyond it, and when the bodies
    /// in flight alone reach it they are only counted (`truncated`).
    pub max_total_body_bytes: usize,
    pub connect_timeout: Duration,
    /// Record CONNECT tunnels (host, port, bytes, duration).
    pub record_tunnels: bool,
    /// Parse WebSocket frames relayed through the proxy and record messages.
    pub capture_websocket_messages: bool,
    /// Live event callback (in addition to [`CaptureProxy::subscribe`](crate::CaptureProxy::subscribe)).
    pub on_event: Option<EventCallback>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        ProxyConfig {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)),
            allow_lan: false,
            ca: None,
            intercept_tls: true,
            passthrough_hosts: Vec::new(),
            passthrough_on_tls_error: false,
            verify_upstream_tls: true,
            extra_upstream_roots_pem: None,
            client_http2: true,
            upstream_http2: true,
            upstream_proxy: None,
            filter: CaptureFilter::default(),
            capacity: 2000,
            max_body_bytes: 1024 * 1024,
            max_total_body_bytes: 256 * 1024 * 1024,
            connect_timeout: Duration::from_secs(20),
            record_tunnels: true,
            capture_websocket_messages: true,
            on_event: None,
        }
    }
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("bind", &self.bind)
            .field("allow_lan", &self.allow_lan)
            .field("ca", &self.ca.as_ref().map(|c| c.common_name().to_owned()))
            .field("intercept_tls", &self.intercept_tls)
            .field("passthrough_hosts", &self.passthrough_hosts)
            .field("passthrough_on_tls_error", &self.passthrough_on_tls_error)
            .field("verify_upstream_tls", &self.verify_upstream_tls)
            .field("client_http2", &self.client_http2)
            .field("upstream_http2", &self.upstream_http2)
            .field("upstream_proxy", &self.upstream_proxy)
            .field("filter", &self.filter)
            .field("capacity", &self.capacity)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_total_body_bytes", &self.max_total_body_bytes)
            .field("connect_timeout", &self.connect_timeout)
            .field("record_tunnels", &self.record_tunnels)
            .field("capture_websocket_messages", &self.capture_websocket_messages)
            .finish_non_exhaustive()
    }
}

/// An HTTP proxy that upstream connections are tunnelled through (CONNECT).
#[derive(Clone, PartialEq, Eq)]
pub struct UpstreamProxy {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl fmt::Debug for UpstreamProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamProxy")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl UpstreamProxy {
    /// Parses `http://[user:pass@]host[:port]` (port defaults to 8080).
    pub fn parse(url: &str) -> Result<Self, ProxyError> {
        let parsed = url::Url::parse(url.trim()).map_err(|e| ProxyError::UpstreamProxy(e.to_string()))?;
        if parsed.scheme() != "http" {
            return Err(ProxyError::UpstreamProxy(format!(
                "only http:// upstream proxies are supported, got {}://",
                parsed.scheme()
            )));
        }
        let host = parsed.host_str().ok_or_else(|| ProxyError::UpstreamProxy("missing host".into()))?;
        let decode = |s: &str| percent_decode(s);
        // `url` drops a port equal to the scheme default, so `:80` is looked
        // up in the text.
        let port = parsed.port().unwrap_or(if spells_port_80(url.trim()) { 80 } else { 8080 });
        Ok(UpstreamProxy {
            host: host.trim_start_matches('[').trim_end_matches(']').to_owned(),
            port,
            username: (!parsed.username().is_empty()).then(|| decode(parsed.username())),
            password: parsed.password().map(decode),
        })
    }
}

/// Whether the authority of `url` ends in port 80 (`host:80`, `[::1]:080`).
fn spells_port_80(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let port = match host_port.rsplit_once(']') {
        Some((_, after)) => after.strip_prefix(':'),
        None => host_port.split_once(':').map(|(_, p)| p),
    };
    port.and_then(|p| p.parse::<u16>().ok()) == Some(80)
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Which traffic is recorded. Empty lists/strings mean "no restriction".
///
/// Host patterns are case-insensitive globs: `*` matches any run of
/// characters and `?` one character, so `*.example.com` matches
/// `api.example.com` (not `example.com` itself) and `*example.com` matches
/// both. A `:port` suffix (`localhost:3000`, `*.test:*`, `[::1]:8080`)
/// restricts the port too.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureFilter {
    /// Record only these hosts.
    pub include_hosts: Vec<String>,
    /// Never record these hosts (CONNECT to them is tunnelled, not decrypted).
    pub exclude_hosts: Vec<String>,
    /// Record only these methods (`GET`, `POST`, ...; `CONNECT` for tunnels).
    pub methods: Vec<String>,
    /// Record only URLs containing this text (case-insensitive).
    pub url_contains: String,
    /// Record only responses whose `Content-Type` contains one of these
    /// (`json`, `text/html`, `image/`). Entries then appear once their
    /// response headers arrive.
    pub content_types: Vec<String>,
}

impl CaptureFilter {
    pub fn is_empty(&self) -> bool {
        non_blank(&self.include_hosts).next().is_none()
            && non_blank(&self.exclude_hosts).next().is_none()
            && non_blank(&self.methods).next().is_none()
            && self.url_contains.trim().is_empty()
            && non_blank(&self.content_types).next().is_none()
    }

    pub fn allows_host(&self, host: &str, port: u16) -> bool {
        if non_blank(&self.exclude_hosts).any(|p| host_matches(p, host, port)) {
            return false;
        }
        let mut include = non_blank(&self.include_hosts).peekable();
        include.peek().is_none() || include.any(|p| host_matches(p, host, port))
    }

    pub fn allows_method(&self, method: &str) -> bool {
        let mut methods = non_blank(&self.methods).peekable();
        methods.peek().is_none() || methods.any(|m| m.trim().eq_ignore_ascii_case(method))
    }

    pub fn allows_url(&self, url: &str) -> bool {
        let needle = self.url_contains.trim();
        needle.is_empty() || url.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())
    }

    /// `content_type` is the response (or, without a response, request) type.
    pub fn allows_content_type(&self, content_type: Option<&str>) -> bool {
        let mut wanted = non_blank(&self.content_types).peekable();
        if wanted.peek().is_none() {
            return true;
        }
        let Some(ct) = content_type else { return false };
        let ct = ct.to_ascii_lowercase();
        wanted.any(|w| ct.contains(&w.trim().to_ascii_lowercase()))
    }

    /// Host, method and URL checks (everything known before a response).
    pub fn allows_request(&self, method: &str, url: &str, host: &str, port: u16) -> bool {
        self.allows_host(host, port) && self.allows_method(method) && self.allows_url(url)
    }

    pub(crate) fn has_content_types(&self) -> bool {
        non_blank(&self.content_types).next().is_some()
    }

    /// Whether a recorded entry passes this filter, for filtering the list
    /// in the UI with the same rules.
    pub fn matches(&self, entry: &CaptureEntry) -> bool {
        self.allows_request(&entry.method, &entry.url, &entry.host, entry.port)
            && self.allows_content_type(entry.content_type())
    }
}

fn non_blank(list: &[String]) -> impl Iterator<Item = &str> {
    list.iter().map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// Matches a host pattern (see [`CaptureFilter`]) against `host` and `port`.
pub fn host_matches(pattern: &str, host: &str, port: u16) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    let (host_pat, port_pat) = split_port(pattern);
    if let Some(pp) = port_pat {
        let pp = pp.trim();
        if pp != "*" && pp.parse::<u16>().ok() != Some(port) {
            return false;
        }
    }
    let host = host.trim().trim_start_matches('[').trim_end_matches(']').trim_end_matches('.');
    let host_pat = host_pat.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.');
    glob(host_pat.as_bytes(), host.as_bytes())
}

fn split_port(pattern: &str) -> (&str, Option<&str>) {
    if let Some(rest) = pattern.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((h, after)) => (h, after.strip_prefix(':')),
            None => (pattern, None),
        };
    }
    match pattern.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, Some(p)),
        _ => (pattern, None),
    }
}

/// Case-insensitive glob with `*` and `?`.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_patterns() {
        assert!(host_matches("*.example.com", "api.example.com", 443));
        assert!(host_matches("*.example.com", "a.b.EXAMPLE.com", 443));
        assert!(!host_matches("*.example.com", "example.com", 443));
        assert!(host_matches("*example.com", "example.com", 443));
        assert!(host_matches("example.com", "example.com.", 80));
        assert!(!host_matches("example.com", "notexample.com", 80));
        assert!(host_matches("localhost:3000", "localhost", 3000));
        assert!(!host_matches("localhost:3000", "localhost", 3001));
        assert!(host_matches("*.test:*", "a.test", 1));
        assert!(host_matches("[::1]:8080", "::1", 8080));
        assert!(host_matches("::1", "[::1]", 1));
        assert!(host_matches("10.0.0.?", "10.0.0.7", 1));
        assert!(host_matches("*", "anything", 1));
        assert!(!host_matches("", "anything", 1));
    }

    #[test]
    fn filter_rules() {
        let f = CaptureFilter {
            include_hosts: vec!["*.example.com".into(), " ".into()],
            exclude_hosts: vec!["ads.example.com".into()],
            methods: vec!["get".into(), "POST".into()],
            url_contains: "/API/".into(),
            content_types: vec!["json".into()],
        };
        assert!(f.allows_request("GET", "https://x.example.com/api/v1", "x.example.com", 443));
        assert!(!f.allows_request("GET", "https://ads.example.com/api/", "ads.example.com", 443));
        assert!(!f.allows_request("DELETE", "https://x.example.com/api/", "x.example.com", 443));
        assert!(!f.allows_request("GET", "https://x.example.com/web", "x.example.com", 443));
        assert!(!f.allows_request("GET", "https://other.org/api/", "other.org", 443));
        assert!(f.allows_content_type(Some("application/json; charset=utf-8")));
        assert!(!f.allows_content_type(Some("text/html")));
        assert!(!f.allows_content_type(None));
        assert!(CaptureFilter::default().is_empty());
        assert!(!f.is_empty());
    }

    #[test]
    fn upstream_proxy_url() {
        let p = UpstreamProxy::parse("http://us%40r:p%3Ass@proxy.local:3128").unwrap();
        assert_eq!(p.host, "proxy.local");
        assert_eq!(p.port, 3128);
        assert_eq!(p.username.as_deref(), Some("us@r"));
        assert_eq!(p.password.as_deref(), Some("p:ss"));
        assert!(!format!("{p:?}").contains("p:ss"));
        assert!(UpstreamProxy::parse("socks5://x:1").is_err());
        assert_eq!(UpstreamProxy::parse("http://[::1]").unwrap().host, "::1");
        // An explicit `:80` (dropped by URL parsing as the default) is kept.
        assert_eq!(UpstreamProxy::parse("http://proxy.local").unwrap().port, 8080);
        assert_eq!(UpstreamProxy::parse("http://proxy.local:80").unwrap().port, 80);
        assert_eq!(UpstreamProxy::parse(" http://u:p@proxy.local:080/ ").unwrap().port, 80);
        assert_eq!(UpstreamProxy::parse("http://[::1]:80").unwrap().port, 80);
        assert_eq!(UpstreamProxy::parse("http://u:80@proxy.local").unwrap().port, 8080);
    }
}
