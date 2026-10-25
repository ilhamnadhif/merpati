//! Proxy support shared by every client: which proxy a connection to a host
//! goes through, and tunnels (HTTP `CONNECT`, SOCKS5, SOCKS4) for the
//! protocols that reqwest does not carry: WebSocket, Socket.IO's WebSocket
//! transport, MQTT and gRPC.
//!
//! The rules are the HTTP engine's (reqwest's): the custom proxy when it is
//! enabled, with its bypass list; otherwise the system proxy when
//! `use_system` is set (`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` /
//! `NO_PROXY`, then the OS settings on macOS and Windows: a proxy
//! auto-config (PAC) script when one is set, else one found by automatic
//! proxy discovery (WPAD over DNS, see `wpad`) when the OS settings ask for
//! it, else the HTTP/HTTPS proxies read by hyper-util's matcher exactly as
//! reqwest reads them, else the macOS SOCKS proxy); otherwise a direct
//! connection. Plain connections (`ws`, `mqtt`, `grpc`) use the HTTP proxy,
//! TLS ones (`wss`, `mqtts`, `grpcs`) the HTTPS proxy. Like browsers, the
//! system proxy is never used for `localhost` and loopback addresses, nor
//! (macOS) for the exceptions and simple host names excluded in System
//! Settings; the HTTP engine uses the same resolver. A custom proxy only
//! skips the hosts of its bypass list (so local traffic can be recorded).
//!
//! When a PAC script lists several proxies (`PROXY a:8080; PROXY b:8080;
//! DIRECT`), the tunnels fall over to the next choice when a proxy cannot
//! be reached, and the HTTP engine retries the request with it; like Chrome,
//! a proxy that could not be reached is then tried last for 5 minutes.
//!
//! The tunnels answer `Proxy-Authenticate: Basic` and `Digest` (RFC 7616).
//! reqwest only sends Basic credentials and has no hook to answer a
//! proxy's challenge, so plain requests of the HTTP engine through a proxy
//! that requires Digest fail with HTTP 407.
//!
//! TLS to the target always runs end to end inside the tunnel. Proxy
//! credentials never appear in errors or `Debug` output.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::HeaderValue;
use hyper_util::client::proxy::matcher::{Intercept, Matcher};
use merpati_core::settings::ProxySettings;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use crate::challenge::Challenge;
use crate::tls::TlsSetup;

mod pac;
mod wpad;
use pac::{Pac, PacChoice};

/// Longest proxy answer to `CONNECT` accepted (status line and headers).
const MAX_CONNECT_RESPONSE: usize = 16 * 1024;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyErrorKind {
    /// The proxy settings are unusable (bad URL, unsupported scheme).
    Config,
    /// The proxy could not be reached (DNS, refused, TLS to the proxy).
    Unreachable,
    /// The proxy wants credentials or rejected them (HTTP 407, SOCKS).
    Auth,
    /// The proxy could not or would not connect to the target.
    Refused,
    /// The target host name could not be resolved locally (`socks5://`, `socks4://`).
    Dns,
    /// The proxy answered something unexpected.
    Protocol,
}

/// A proxy failure, with a message that can be shown as is (it names the
/// proxy by scheme, host and port, never by its credentials).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProxyError {
    pub kind: ProxyErrorKind,
    pub message: String,
}

impl ProxyError {
    fn new(kind: ProxyErrorKind, message: impl Into<String>) -> ProxyError {
        ProxyError { kind, message: message.into() }
    }
}

impl From<ProxyError> for io::Error {
    /// Kind `Other`, so callers matching on `ConnectionRefused` for direct
    /// connections keep the proxy's message.
    fn from(e: ProxyError) -> io::Error {
        io::Error::other(e)
    }
}

impl From<ProxyError> for crate::HttpError {
    fn from(e: ProxyError) -> crate::HttpError {
        crate::HttpError::new(crate::HttpErrorKind::Proxy, e.message)
    }
}

/// The [`ProxyError`] in an error chain, also inside `io::Error`s (as
/// returned by [`dial`] through tungstenite, tonic or rumqttc).
pub fn find_proxy_error<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a ProxyError> {
    let mut cur: Option<&'a (dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(p) = e.downcast_ref::<ProxyError>() {
            return Some(p);
        }
        // `io::Error::source` skips the wrapped error itself.
        if let Some(io) = e.downcast_ref::<io::Error>()
            && let Some(p) = io.get_ref().and_then(|inner| inner.downcast_ref::<ProxyError>())
        {
            return Some(p);
        }
        cur = e.source();
    }
    None
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Whether the custom proxy (a proxy URL or a PAC script) is in use.
fn custom_enabled(settings: &ProxySettings) -> bool {
    settings.custom_enabled && (!settings.custom_url.trim().is_empty() || custom_pac(settings).is_some())
}

/// The PAC script of the custom proxy settings; it wins over the proxy URL.
fn custom_pac(settings: &ProxySettings) -> Option<&str> {
    Some(settings.pac_url.trim()).filter(|u| settings.custom_enabled && !u.is_empty())
}

/// The custom proxy URL with the settings' credentials, parsed like
/// reqwest's `Proxy::all`: `http://` is assumed without a scheme and SOCKS
/// proxies default to port 1080. The URL may carry credentials, so it is
/// never quoted in errors.
fn custom_proxy_url(settings: &ProxySettings) -> Result<url::Url, ProxyError> {
    let raw = settings.custom_url.trim();
    let invalid = |e: url::ParseError| ProxyError::new(ProxyErrorKind::Config, format!("Invalid proxy URL: {e}"));
    let mut url = match url::Url::parse(raw) {
        Ok(u) if u.has_host() || raw.contains("://") => u,
        // `proxy:3128` parses as scheme "proxy"; `127.0.0.1:3128` not at all.
        Ok(_) | Err(url::ParseError::RelativeUrlWithoutBase) => {
            url::Url::parse(&format!("http://{raw}")).map_err(invalid)?
        }
        Err(e) => return Err(invalid(e)),
    };
    let Some(kind) = ProxyKind::from_scheme(url.scheme()) else {
        return Err(ProxyError::new(
            ProxyErrorKind::Config,
            format!(
                "Unsupported proxy scheme \"{}://\" (use http://, https://, socks5://, socks5h://, socks4:// or socks4a://)",
                url.scheme()
            ),
        ));
    };
    if url.host_str().is_none_or(str::is_empty) {
        return Err(ProxyError::new(ProxyErrorKind::Config, "Invalid proxy URL: missing host"));
    }
    if url.port().is_none() && matches!(kind, ProxyKind::Socks5 { .. } | ProxyKind::Socks4 { .. }) {
        let _ = url.set_port(Some(1080));
    }
    if !settings.username.is_empty() {
        let ok = url.set_username(&settings.username).is_ok() && url.set_password(Some(&settings.password)).is_ok();
        if !ok {
            return Err(ProxyError::new(ProxyErrorKind::Config, "Invalid proxy URL: cannot carry credentials"));
        }
    }
    Ok(url)
}

/// The bypass list in `NO_PROXY` syntax: comma (or semicolon) separated,
/// with `*.example.com` written as `.example.com` (the matcher only knows a
/// bare `*` as wildcard; a leading dot matches the domain's subdomains).
fn bypass_list(raw: &str) -> String {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.strip_prefix("*.") {
            Some(rest) => format!(".{rest}"),
            None => s.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Applies the proxy settings to a reqwest client the way the HTTP engine
/// does: the custom proxy (with its credentials and bypass list), the
/// system proxy (chosen per request by [`ProxyResolver`], so the loopback
/// and OS exceptions apply), or no proxy.
pub fn apply_to_reqwest(
    builder: reqwest::ClientBuilder,
    settings: &ProxySettings,
) -> Result<reqwest::ClientBuilder, ProxyError> {
    if custom_pac(settings).is_some() || (!custom_enabled(settings) && settings.use_system) {
        let resolver = ProxyResolver::new(settings)?;
        Ok(builder.proxy(reqwest::Proxy::custom(move |url| resolver.proxy_url(url))))
    } else if custom_enabled(settings) {
        let url = custom_proxy_url(settings)?;
        // Already validated; reqwest's error is not quoted as it may show the URL.
        let mut proxy = reqwest::Proxy::all(url.as_str())
            .map_err(|_| ProxyError::new(ProxyErrorKind::Config, "Invalid proxy URL"))?;
        let bypass = bypass_list(&settings.bypass);
        if !bypass.is_empty() {
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&bypass));
        }
        Ok(builder.proxy(proxy))
    } else {
        Ok(builder.no_proxy())
    }
}

/// Hosts the system proxy is not used for, in `NO_PROXY` syntax: loopback
/// (as browsers do) plus the exceptions of the OS network settings.
fn system_bypass(os: &OsProxy) -> String {
    let mut list = vec!["localhost".to_string(), ".localhost".into(), "127.0.0.0/8".into(), "::1".into()];
    list.extend(os.exceptions.iter().map(|e| exception_to_no_proxy(e)));
    list.join(",")
}

/// A macOS/Windows style exception (`*.local`, `169.254/16`) in `NO_PROXY`
/// syntax (`.local`, `169.254.0.0/16`).
fn exception_to_no_proxy(e: &str) -> String {
    let e = e.trim();
    if let Some(rest) = e.strip_prefix("*.") {
        return format!(".{rest}");
    }
    if let Some((ip, bits)) = e.split_once('/')
        && !ip.contains(':')
        && ip.split('.').all(|o| o.parse::<u8>().is_ok())
    {
        let mut octets: Vec<&str> = ip.split('.').collect();
        octets.resize(4, "0");
        return format!("{}/{bits}", octets.join("."));
    }
    e.to_string()
}

/// Where a PAC script comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PacSource {
    /// `http(s)://` or `file://` URL.
    Url(String),
    /// The script itself (macOS `ProxyAutoConfigJavaScript`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Script(String),
    /// A WPAD candidate (`http://wpad.<domain>/wpad.dat`), given less time.
    Wpad(String),
}

/// The parts of the OS proxy settings hyper-util's matcher does not read.
#[derive(Debug, Default)]
struct OsProxy {
    /// Hosts and domains that connect directly (`*.local`, `169.254/16`).
    exceptions: Vec<String>,
    /// Host names without a dot connect directly.
    exclude_simple: bool,
    pac: Option<PacSource>,
    /// `host:port` of the macOS SOCKS proxy.
    socks: Option<String>,
    /// Whether an HTTP or HTTPS proxy is set (they win over SOCKS).
    manual: bool,
    /// Automatic proxy discovery (WPAD) is on; used without a PAC URL.
    auto_discover: bool,
    /// The DNS search domains WPAD looks in (read when it is on).
    search_domains: Vec<String>,
}

#[cfg(target_os = "macos")]
fn os_proxy() -> OsProxy {
    use system_configuration::core_foundation::array::CFArray;
    use system_configuration::core_foundation::base::{CFType, TCFType};
    use system_configuration::core_foundation::number::CFNumber;
    use system_configuration::core_foundation::string::{CFString, CFStringRef};
    use system_configuration::dynamic_store::SCDynamicStoreBuilder;
    use system_configuration::sys::schema_definitions::*;
    let Some(map) = SCDynamicStoreBuilder::new("merpati").build().and_then(|s| s.get_proxies()) else {
        return OsProxy::default();
    };
    let number = |key: CFStringRef| map.find(key).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_i32());
    let string = |key: CFStringRef| {
        map.find(key).and_then(|v| v.downcast::<CFString>()).map(|s| s.to_string()).filter(|s| !s.trim().is_empty())
    };
    // SAFETY: framework constants, valid for the life of the process.
    unsafe {
        let exceptions = map
            .find(kSCPropNetProxiesExceptionsList)
            .and_then(|v| v.downcast::<CFArray>())
            .map(|a| {
                a.get_all_values()
                    .into_iter()
                    // SAFETY: the items of a CFArray owned by `a`, retained by `wrap_under_get_rule`.
                    .filter_map(|p| CFType::wrap_under_get_rule(p).downcast::<CFString>())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let pac = match number(kSCPropNetProxiesProxyAutoConfigEnable) {
            Some(1) => string(kSCPropNetProxiesProxyAutoConfigJavaScript)
                .map(PacSource::Script)
                .or_else(|| string(kSCPropNetProxiesProxyAutoConfigURLString).map(PacSource::Url)),
            _ => None,
        };
        let socks = match (number(kSCPropNetProxiesSOCKSEnable), string(kSCPropNetProxiesSOCKSProxy)) {
            (Some(1), Some(host)) => {
                let port = number(kSCPropNetProxiesSOCKSPort).unwrap_or(1080);
                Some(authority(strip_brackets(host.trim()), u16::try_from(port).unwrap_or(1080)))
            }
            _ => None,
        };
        let auto_discover = number(kSCPropNetProxiesProxyAutoDiscoveryEnable) == Some(1);
        OsProxy {
            exceptions,
            exclude_simple: number(kSCPropNetProxiesExcludeSimpleHostnames) == Some(1),
            pac,
            socks,
            manual: number(kSCPropNetProxiesHTTPEnable) == Some(1) || number(kSCPropNetProxiesHTTPSEnable) == Some(1),
            auto_discover,
            search_domains: if auto_discover { wpad::search_domains() } else { Vec::new() },
        }
    }
}

#[cfg(windows)]
fn os_proxy() -> OsProxy {
    let Ok(key) =
        windows_registry::CURRENT_USER.open("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
    else {
        return OsProxy::default();
    };
    // "Automatically detect settings" lives only in the binary connection settings.
    let auto_discover = key
        .open("Connections")
        .and_then(|c| c.get_value("DefaultConnectionSettings"))
        .is_ok_and(|v| wpad::auto_detect_flag(&v));
    OsProxy {
        // `<local>` in the bypass list: names without a dot connect directly.
        exclude_simple: key.get_string("ProxyOverride").is_ok_and(|v| v.split(';').any(|e| e.trim() == "<local>")),
        pac: key.get_string("AutoConfigURL").ok().filter(|u| !u.trim().is_empty()).map(PacSource::Url),
        manual: key.get_u32("ProxyEnable").unwrap_or(0) != 0,
        auto_discover,
        search_domains: if auto_discover { wpad::search_domains() } else { Vec::new() },
        ..OsProxy::default()
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_proxy() -> OsProxy {
    OsProxy::default()
}

/// Whether a proxy environment variable is set; they win over the OS settings.
fn env_proxy_set() -> bool {
    ["ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

/// How long a loaded PAC script is reused, and how long a failure to load
/// one is remembered (a missing WPAD script as long as a loaded one: most
/// networks have none).
const PAC_TTL: Duration = Duration::from_secs(300);
const PAC_FAILURE_TTL: Duration = Duration::from_secs(60);

/// The PAC script from `source`, loaded at most once per [`PAC_TTL`].
fn load_pac(source: &PacSource) -> Result<Arc<Pac>, ProxyError> {
    load_pac_via(source, None)
}

/// [`load_pac`] with the DNS lookup of a downloaded script's host replaced
/// by `resolve_to` (tests).
fn load_pac_via(source: &PacSource, resolve_to: Option<SocketAddr>) -> Result<Arc<Pac>, ProxyError> {
    type Cache = HashMap<PacKey, (Instant, Result<Arc<Pac>, ProxyError>)>;
    type PacKey = (u8, String);
    static CACHE: std::sync::Mutex<Option<Cache>> = std::sync::Mutex::new(None);
    // Loads one at a time, without blocking the lookups of loaded scripts
    // (WPAD probes run in the background).
    static LOADING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let key = match source {
        PacSource::Url(u) => (0, u.trim().to_string()),
        PacSource::Script(s) => (1, s.clone()),
        PacSource::Wpad(u) => (2, u.clone()),
    };
    let cached = |key: &PacKey| {
        let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let (at, result) = guard.get_or_insert_with(Default::default).get(key)?;
        let ttl = if result.is_ok() || key.0 == 2 { PAC_TTL } else { PAC_FAILURE_TTL };
        (at.elapsed() < ttl).then(|| result.clone())
    };
    if let Some(result) = cached(&key) {
        return result;
    }
    let _loading = LOADING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(result) = cached(&key) {
        return result;
    }
    let result = match source {
        PacSource::Url(u) => Pac::load(u, pac::LOAD_TIMEOUT, resolve_to),
        PacSource::Script(s) => Pac::from_source(s.clone()),
        PacSource::Wpad(u) => Pac::load(u, wpad::WPAD_TIMEOUT, resolve_to),
    }
    .map(Arc::new);
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(Default::default);
    cache.retain(|_, (at, _)| at.elapsed() < PAC_TTL);
    cache.insert(key, (Instant::now(), result.clone()));
    result
}

// ---------------------------------------------------------------------------
// Proxy selection
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyKind {
    /// HTTP proxy, tunnelling with `CONNECT`.
    Http,
    /// HTTP proxy reached over TLS (`https://` proxy URL), tunnelling with `CONNECT`.
    Https,
    /// SOCKS5: `socks5h://` (`remote_dns`) lets the proxy resolve host
    /// names, `socks5://` resolves them locally.
    Socks5 { remote_dns: bool },
    /// SOCKS4a (`remote_dns`) or SOCKS4 (IPv4 targets resolved locally).
    Socks4 { remote_dns: bool },
}

impl ProxyKind {
    fn from_scheme(scheme: &str) -> Option<ProxyKind> {
        Some(match scheme.to_ascii_lowercase().as_str() {
            "http" => ProxyKind::Http,
            "https" => ProxyKind::Https,
            "socks5" => ProxyKind::Socks5 { remote_dns: false },
            "socks5h" => ProxyKind::Socks5 { remote_dns: true },
            "socks4" => ProxyKind::Socks4 { remote_dns: false },
            "socks4a" => ProxyKind::Socks4 { remote_dns: true },
            _ => return None,
        })
    }

    pub fn scheme(self) -> &'static str {
        match self {
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
            ProxyKind::Socks5 { remote_dns: false } => "socks5",
            ProxyKind::Socks5 { remote_dns: true } => "socks5h",
            ProxyKind::Socks4 { remote_dns: false } => "socks4",
            ProxyKind::Socks4 { remote_dns: true } => "socks4a",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            ProxyKind::Http => 80,
            ProxyKind::Https => 443,
            ProxyKind::Socks5 { .. } | ProxyKind::Socks4 { .. } => 1080,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ProxyKind::Http | ProxyKind::Https => "proxy",
            ProxyKind::Socks5 { .. } => "SOCKS5 proxy",
            ProxyKind::Socks4 { .. } => "SOCKS4 proxy",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Credentials {
    None,
    /// `Proxy-Authorization` value for HTTP proxies.
    Basic(HeaderValue),
    /// Username and password for SOCKS proxies.
    Raw(String, String),
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Credentials::None => "None",
            Credentials::Basic(_) => "Basic(***)",
            Credentials::Raw(..) => "Raw(***)",
        })
    }
}

/// The proxy a connection goes through.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyServer {
    kind: ProxyKind,
    /// Without IPv6 brackets.
    host: String,
    port: u16,
    credentials: Credentials,
    /// The next choices of a PAC script (`None` is `DIRECT`), for when this
    /// proxy cannot be reached.
    fallbacks: Vec<Option<ProxyServer>>,
}

impl fmt::Display for ProxyServer {
    /// `scheme://host:port`, never with credentials.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.kind.scheme(), authority(&self.host, self.port))
    }
}

impl fmt::Debug for ProxyServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fallbacks: Vec<String> =
            self.fallbacks.iter().map(|f| f.as_ref().map_or("DIRECT".into(), ToString::to_string)).collect();
        f.debug_struct("ProxyServer")
            .field("url", &self.to_string())
            .field("credentials", &self.has_credentials())
            .field("fallbacks", &fallbacks)
            .finish()
    }
}

impl ProxyServer {
    fn from_intercept(i: &Intercept) -> Option<ProxyServer> {
        let uri = i.uri();
        let kind = ProxyKind::from_scheme(uri.scheme_str()?)?;
        let host = strip_brackets(uri.host()?).to_string();
        let port = uri.port_u16().unwrap_or(kind.default_port());
        let credentials = match kind {
            ProxyKind::Http | ProxyKind::Https => match (i.basic_auth(), i.raw_auth()) {
                (Some(v), _) => Credentials::Basic(v.clone()),
                (None, Some((u, p))) => Credentials::Basic(basic_auth(u, p)),
                (None, None) => Credentials::None,
            },
            ProxyKind::Socks5 { .. } | ProxyKind::Socks4 { .. } => match i.raw_auth() {
                Some((u, p)) => Credentials::Raw(u.to_string(), p.to_string()),
                None => Credentials::None,
            },
        };
        Some(ProxyServer { kind, host, port, credentials, fallbacks: Vec::new() })
    }

    /// A proxy chosen by a PAC script (`http://host:port`, ...), with the
    /// credentials of the URL.
    fn from_url(url: &url::Url) -> Option<ProxyServer> {
        let kind = ProxyKind::from_scheme(url.scheme())?;
        let host = strip_brackets(url.host_str()?).to_string();
        let port = url.port().unwrap_or(kind.default_port());
        let decode = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
        let credentials = match (url.username(), kind) {
            ("", _) => Credentials::None,
            (user, ProxyKind::Http | ProxyKind::Https) => {
                Credentials::Basic(basic_auth(&decode(user), &decode(url.password().unwrap_or_default())))
            }
            (user, _) => Credentials::Raw(decode(user), decode(url.password().unwrap_or_default())),
        };
        Some(ProxyServer { kind, host, port, credentials, fallbacks: Vec::new() })
    }

    pub fn kind(&self) -> ProxyKind {
        self.kind
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn has_credentials(&self) -> bool {
        self.credentials != Credentials::None
    }

    /// The next choices of the PAC script that picked this proxy (`None` is
    /// `DIRECT`), tried in order by [`ProxyServer::tunnel`] when this proxy
    /// cannot be reached.
    pub fn fallbacks(&self) -> &[Option<ProxyServer>] {
        &self.fallbacks
    }

    /// Connects to the proxy and opens a tunnel to `host:port`. For an
    /// HTTPS proxy, `tls` (its extra trusted roots) and `verify` apply to the
    /// TLS connection to the proxy, as they do in the HTTP engine; the
    /// target's client certificate is not presented to the proxy.
    ///
    /// When the proxy cannot be reached and a PAC script listed more
    /// choices, the next one is used (a `DIRECT` choice connects directly),
    /// as browsers do; the unreachable proxy is then tried last for a while.
    pub async fn tunnel(&self, host: &str, port: u16, tls: &TlsSetup, verify: bool) -> Result<ProxyStream, ProxyError> {
        let mut tried = self;
        let mut result = self.tunnel_once(host, port, tls, verify).await;
        let mut next = self.fallbacks.iter();
        loop {
            match &result {
                Err(e) if e.kind == ProxyErrorKind::Unreachable => mark_unreachable(&tried.to_string()),
                _ => return result,
            }
            let Some(choice) = next.next() else { return result };
            if let Err(e) = &result {
                tracing::debug!("{e}; trying the next choice of the proxy auto-config script");
            }
            match choice {
                Some(proxy) => {
                    tried = proxy;
                    result = proxy.tunnel_once(host, port, tls, verify).await;
                }
                None => return connect_direct(host, port).await,
            }
        }
    }

    async fn tunnel_once(
        &self,
        host: &str,
        port: u16,
        tls: &TlsSetup,
        verify: bool,
    ) -> Result<ProxyStream, ProxyError> {
        let host = strip_brackets(host);
        match self.kind {
            ProxyKind::Http | ProxyKind::Https => self.http_tunnel(host, port, tls, verify).await,
            ProxyKind::Socks5 { remote_dns } => {
                let mut tcp = self.connect_tcp().await?;
                self.socks5_connect(&mut tcp, host, port, remote_dns).await?;
                Ok(ProxyStream::Tcp(tcp))
            }
            ProxyKind::Socks4 { remote_dns } => {
                let mut tcp = self.connect_tcp().await?;
                self.socks4_connect(&mut tcp, host, port, remote_dns).await?;
                Ok(ProxyStream::Tcp(tcp))
            }
        }
    }

    /// A connection to an HTTP proxy, inside TLS for an HTTPS proxy.
    async fn open_http(&self, tls: &TlsSetup, verify: bool) -> Result<ProxyStream, ProxyError> {
        let tcp = self.connect_tcp().await?;
        if self.kind != ProxyKind::Https {
            return Ok(ProxyStream::Tcp(tcp));
        }
        let config = crate::tls::client_config_with(verify, tls.extra_roots_pem.as_deref(), None)
            .map_err(|e| ProxyError::new(ProxyErrorKind::Config, format!("TLS setup for proxy {self} failed: {e}")))?;
        let name = rustls::pki_types::ServerName::try_from(self.host.clone()).map_err(|e| {
            ProxyError::new(ProxyErrorKind::Config, format!("Invalid proxy host name \"{}\": {e}", self.host))
        })?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(|e| {
            ProxyError::new(ProxyErrorKind::Unreachable, format!("TLS handshake with proxy {self} failed: {e}"))
        })?;
        Ok(ProxyStream::Tls(Box::new(tls)))
    }

    /// `CONNECT host:port` through an HTTP(S) proxy, answering its Digest
    /// challenge (RFC 7616) when it asks for one.
    async fn http_tunnel(
        &self,
        host: &str,
        port: u16,
        tls: &TlsSetup,
        verify: bool,
    ) -> Result<ProxyStream, ProxyError> {
        let target = authority(host, port);
        let mut stream = self.open_http(tls, verify).await?;
        // Basic credentials go with the first CONNECT (as reqwest sends
        // them), unless the proxy asked for Digest before.
        let mut auth = if wants_digest(self) { None } else { self.basic_credentials() };
        let (mut digests, mut basic_retried) = (0, false);
        loop {
            let Some(refusal) = self.http_connect(&mut stream, &target, auth.as_ref()).await? else {
                return Ok(stream);
            };
            let stale = |c: &Challenge| c.param("stale").is_some_and(|s| s.eq_ignore_ascii_case("true"));
            let basic = refusal.challenges.iter().any(|c| c.scheme == "basic");
            match (best_digest(&refusal.challenges), self.user_password()) {
                // A first answer, or a second one for an expired nonce.
                (Some(challenge), Some((user, password))) if digests == 0 || (digests == 1 && stale(challenge)) => {
                    remember_digest(self);
                    auth = Some(self.digest_authorization(challenge, &target, &user, &password)?);
                    digests += 1;
                }
                // Asked for Digest before, Basic now.
                (None, Some(_)) if basic && auth.is_none() && !basic_retried => {
                    auth = self.basic_credentials();
                    basic_retried = true;
                }
                _ => return Err(self.auth_error(&refusal, auth.is_some())),
            }
            if !refusal.reusable {
                stream = self.open_http(tls, verify).await?;
            }
        }
    }

    async fn connect_tcp(&self) -> Result<TcpStream, ProxyError> {
        let addrs: Vec<_> = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .map_err(|_| {
                ProxyError::new(
                    ProxyErrorKind::Unreachable,
                    format!("Could not resolve the proxy host \"{}\"", self.host),
                )
            })?
            .collect();
        let mut last = None;
        for addr in addrs {
            match TcpStream::connect(addr).await {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    return Ok(s);
                }
                Err(e) => last = Some(e),
            }
        }
        let reason = match last {
            Some(e) if e.kind() == io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
            Some(e) => e.to_string(),
            None => "no address".to_string(),
        };
        Err(ProxyError::new(ProxyErrorKind::Unreachable, format!("Could not connect to the proxy {self}: {reason}")))
    }

    /// An I/O failure while talking to the proxy.
    fn io_error(&self, e: io::Error) -> ProxyError {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            ProxyError::new(ProxyErrorKind::Protocol, format!("The {} {self} closed the connection", self.kind.label()))
        } else {
            ProxyError::new(
                ProxyErrorKind::Unreachable,
                format!("Connection to the {} {self} failed: {e}", self.kind.label()),
            )
        }
    }

    /// The `Proxy-Authorization` value for Basic authentication.
    fn basic_credentials(&self) -> Option<HeaderValue> {
        match &self.credentials {
            Credentials::None => None,
            Credentials::Basic(v) => Some(v.clone()),
            Credentials::Raw(u, p) => Some(basic_auth(u, p)),
        }
    }

    /// The username and password, for Digest.
    fn user_password(&self) -> Option<(String, String)> {
        match &self.credentials {
            Credentials::None => None,
            Credentials::Basic(v) => decode_basic(v),
            Credentials::Raw(u, p) => Some((u.clone(), p.clone())),
        }
    }

    /// The `Proxy-Authorization` value answering a Digest challenge for
    /// `CONNECT target`.
    fn digest_authorization(
        &self,
        challenge: &Challenge,
        target: &str,
        user: &str,
        password: &str,
    ) -> Result<HeaderValue, ProxyError> {
        let offered: Vec<String> =
            challenge.param("qop").unwrap_or_default().split(',').map(|q| q.trim().to_ascii_lowercase()).collect();
        // CONNECT has no body, so `auth-int` is as easy as `auth`.
        let qop = ["auth", "auth-int"].into_iter().find(|q| offered.iter().any(|o| o == q)).unwrap_or_default();
        let mut random = [0u8; 16];
        rustls::crypto::ring::default_provider()
            .secure_random
            .fill(&mut random)
            .map_err(|_| ProxyError::new(ProxyErrorKind::Config, "No random numbers for the proxy Digest nonce"))?;
        let value = crate::digest::header_value(&crate::digest::DigestInput {
            username: user,
            password,
            realm: challenge.param("realm").unwrap_or_default(),
            nonce: challenge.param("nonce").unwrap_or_default(),
            algorithm: challenge.param("algorithm").unwrap_or("MD5"),
            qop,
            nc: "00000001",
            cnonce: &hex::encode(random),
            opaque: challenge.param("opaque").unwrap_or_default(),
            method: "CONNECT",
            uri: target,
            body: b"",
        })
        .map_err(|e| ProxyError::new(ProxyErrorKind::Auth, format!("The proxy {self} asks for Digest with an {e}")))?;
        let mut v = HeaderValue::from_bytes(value.as_bytes()).map_err(|_| {
            ProxyError::new(ProxyErrorKind::Config, "The proxy username contains characters that cannot be sent")
        })?;
        v.set_sensitive(true);
        Ok(v)
    }

    /// The error for a `CONNECT` the proxy still refuses with 407; `sent`
    /// tells whether credentials went with it.
    fn auth_error(&self, refusal: &AuthRequired, sent: bool) -> ProxyError {
        let status = &refusal.status;
        let supported = |c: &Challenge| match c.scheme.as_str() {
            "basic" => true,
            "digest" => crate::digest::parse_algorithm(c.param("algorithm").unwrap_or_default()).is_some(),
            _ => false,
        };
        if !refusal.challenges.is_empty() && !refusal.challenges.iter().any(supported) {
            let name = |scheme: &str| match scheme {
                "ntlm" => "NTLM".to_string(),
                "negotiate" => "Negotiate".to_string(),
                other => sanitize(other),
            };
            let mut schemes: Vec<String> = refusal.challenges.iter().map(|c| name(&c.scheme)).collect();
            schemes.dedup();
            return ProxyError::new(
                ProxyErrorKind::Auth,
                format!(
                    "The proxy {self} asks for {} authentication ({status}), which is not supported (Basic and Digest are)",
                    schemes.join("/")
                ),
            );
        }
        if sent {
            ProxyError::new(
                ProxyErrorKind::Auth,
                format!(
                    "The proxy {self} rejected the proxy credentials ({status}): check the proxy username and password"
                ),
            )
        } else {
            ProxyError::new(
                ProxyErrorKind::Auth,
                format!("The proxy {self} requires authentication ({status}): set the proxy username and password"),
            )
        }
    }

    /// Sends `CONNECT target` (with `auth` as `Proxy-Authorization`).
    /// `Ok(None)` once the tunnel is open; `Ok(Some(..))` when the proxy
    /// answers 407.
    async fn http_connect<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
        target: &str,
        auth: Option<&HeaderValue>,
    ) -> Result<Option<AuthRequired>, ProxyError> {
        let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n").into_bytes();
        if let Some(v) = auth {
            req.extend_from_slice(b"Proxy-Authorization: ");
            req.extend_from_slice(v.as_bytes());
            req.extend_from_slice(b"\r\n");
        }
        req.extend_from_slice(b"\r\n");
        stream.write_all(&req).await.map_err(|e| self.io_error(e))?;
        stream.flush().await.map_err(|e| self.io_error(e))?;
        // Byte by byte, so nothing after the answer is consumed: it belongs
        // to the tunnelled protocol.
        let mut head = Vec::with_capacity(128);
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= MAX_CONNECT_RESPONSE {
                return Err(ProxyError::new(
                    ProxyErrorKind::Protocol,
                    format!("The proxy {self} sent an oversized answer to CONNECT"),
                ));
            }
            let mut b = [0u8; 1];
            match stream.read(&mut b).await {
                Ok(0) => {
                    return Err(ProxyError::new(
                        ProxyErrorKind::Protocol,
                        format!("The proxy {self} closed the connection without answering CONNECT {target}"),
                    ));
                }
                Ok(_) => head.push(b[0]),
                Err(e) => return Err(self.io_error(e)),
            }
        }
        let status_line = head.split(|b| *b == b'\n').next().unwrap_or_default();
        let status_line = String::from_utf8_lossy(status_line);
        let mut parts = status_line.trim().splitn(3, ' ');
        let version = parts.next().unwrap_or_default();
        let code = parts.next().and_then(|c| c.parse::<u16>().ok());
        let reason = sanitize(parts.next().unwrap_or_default());
        let code = match code {
            Some(c) if version.starts_with("HTTP/") => c,
            _ => {
                return Err(ProxyError::new(
                    ProxyErrorKind::Protocol,
                    format!("The proxy {self} did not answer CONNECT with HTTP (is it an HTTP proxy?)"),
                ));
            }
        };
        let status = if reason.is_empty() { format!("HTTP {code}") } else { format!("HTTP {code} {reason}") };
        match code {
            200..=299 => return Ok(None),
            407 => {}
            _ => {
                return Err(ProxyError::new(
                    ProxyErrorKind::Refused,
                    format!("The proxy {self} could not open a tunnel to {target}: {status}"),
                ));
            }
        }
        let text = String::from_utf8_lossy(&head);
        let headers: Vec<(String, &str)> = text
            .split("\r\n")
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim()))
            .collect();
        let values = |name: &'static str| headers.iter().filter(move |(k, _)| k == name).map(|(_, v)| *v);
        let challenges = crate::challenge::parse_challenges(values("proxy-authenticate"));
        // The connection can carry the next CONNECT when the body's end is
        // known and the proxy keeps it open; it is read here.
        let keep_alive = version.eq_ignore_ascii_case("HTTP/1.1")
            && !values("connection").any(|v| v.to_ascii_lowercase().contains("close"))
            && values("transfer-encoding").next().is_none();
        let length = values("content-length").next().and_then(|v| v.parse::<usize>().ok());
        let reusable = match length {
            Some(n) if keep_alive && n <= MAX_CONNECT_RESPONSE => {
                let mut body = vec![0u8; n];
                stream.read_exact(&mut body).await.is_ok()
            }
            _ => false,
        };
        Ok(Some(AuthRequired { status, challenges, reusable }))
    }

    async fn socks5_connect(
        &self,
        stream: &mut TcpStream,
        host: &str,
        port: u16,
        remote_dns: bool,
    ) -> Result<(), ProxyError> {
        let creds = match &self.credentials {
            Credentials::Raw(u, p) => Some((u.as_str(), p.as_str())),
            _ => None,
        };
        if let Some((u, p)) = creds
            && (u.len() > 255 || p.len() > 255)
        {
            return Err(ProxyError::new(
                ProxyErrorKind::Config,
                "SOCKS5 proxy usernames and passwords are limited to 255 bytes",
            ));
        }
        // Greeting: no authentication, plus username/password when we have them.
        let greeting: &[u8] = if creds.is_some() { &[5, 2, 0, 2] } else { &[5, 1, 0] };
        stream.write_all(greeting).await.map_err(|e| self.io_error(e))?;
        let mut choice = [0u8; 2];
        stream.read_exact(&mut choice).await.map_err(|e| self.io_error(e))?;
        if choice[0] != 5 {
            return Err(ProxyError::new(
                ProxyErrorKind::Protocol,
                format!("{self} did not answer as a SOCKS5 proxy (is it an HTTP proxy?)"),
            ));
        }
        match (choice[1], creds) {
            (0, _) => {}
            (2, Some((u, p))) => {
                let mut msg = Vec::with_capacity(3 + u.len() + p.len());
                msg.push(1);
                msg.push(u.len() as u8);
                msg.extend_from_slice(u.as_bytes());
                msg.push(p.len() as u8);
                msg.extend_from_slice(p.as_bytes());
                stream.write_all(&msg).await.map_err(|e| self.io_error(e))?;
                let mut status = [0u8; 2];
                stream.read_exact(&mut status).await.map_err(|e| self.io_error(e))?;
                if status[1] != 0 {
                    return Err(ProxyError::new(
                        ProxyErrorKind::Auth,
                        format!(
                            "The SOCKS5 proxy {self} rejected the proxy credentials: check the proxy username and password"
                        ),
                    ));
                }
            }
            (0xFF, None) => {
                return Err(ProxyError::new(
                    ProxyErrorKind::Auth,
                    format!("The SOCKS5 proxy {self} requires authentication: set the proxy username and password"),
                ));
            }
            (0xFF, Some(_)) => {
                return Err(ProxyError::new(
                    ProxyErrorKind::Auth,
                    format!("The SOCKS5 proxy {self} accepts none of the offered authentication methods"),
                ));
            }
            (m, _) => {
                return Err(ProxyError::new(
                    ProxyErrorKind::Protocol,
                    format!("The SOCKS5 proxy {self} chose an unsupported authentication method ({m})"),
                ));
            }
        }
        let mut req = vec![5, 1, 0];
        match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => {
                req.push(1);
                req.extend_from_slice(&ip.octets());
            }
            Ok(IpAddr::V6(ip)) => {
                req.push(4);
                req.extend_from_slice(&ip.octets());
            }
            Err(_) if remote_dns => {
                if host.len() > 255 {
                    return Err(ProxyError::new(ProxyErrorKind::Config, format!("Host name too long: {host}")));
                }
                req.push(3);
                req.push(host.len() as u8);
                req.extend_from_slice(host.as_bytes());
            }
            Err(_) => match resolve(host, port, false).await? {
                IpAddr::V4(ip) => {
                    req.push(1);
                    req.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    req.push(4);
                    req.extend_from_slice(&ip.octets());
                }
            },
        }
        req.extend_from_slice(&port.to_be_bytes());
        stream.write_all(&req).await.map_err(|e| self.io_error(e))?;
        let mut reply = [0u8; 4];
        stream.read_exact(&mut reply).await.map_err(|e| self.io_error(e))?;
        if reply[1] != 0 {
            let why = match reply[1] {
                1 => "general failure",
                2 => "connection not allowed by the proxy's rules",
                3 => "network unreachable",
                4 => "host unreachable",
                5 => "connection refused",
                6 => "TTL expired",
                7 => "command not supported",
                8 => "address type not supported",
                _ => "unknown error",
            };
            return Err(ProxyError::new(
                ProxyErrorKind::Refused,
                format!("The SOCKS5 proxy {self} could not connect to {}: {why}", authority(host, port)),
            ));
        }
        // The bound address, unused.
        let rest = match reply[3] {
            1 => 4 + 2,
            4 => 16 + 2,
            3 => {
                let mut len = [0u8; 1];
                stream.read_exact(&mut len).await.map_err(|e| self.io_error(e))?;
                len[0] as usize + 2
            }
            _ => {
                return Err(ProxyError::new(
                    ProxyErrorKind::Protocol,
                    format!("The SOCKS5 proxy {self} sent an invalid reply"),
                ));
            }
        };
        let mut skip = vec![0u8; rest];
        stream.read_exact(&mut skip).await.map_err(|e| self.io_error(e))?;
        Ok(())
    }

    async fn socks4_connect(
        &self,
        stream: &mut TcpStream,
        host: &str,
        port: u16,
        remote_dns: bool,
    ) -> Result<(), ProxyError> {
        let user = match &self.credentials {
            Credentials::Raw(u, _) => u.as_str(),
            _ => "",
        };
        let mut req = vec![4, 1];
        req.extend_from_slice(&port.to_be_bytes());
        let ip = match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => Some(ip),
            Ok(IpAddr::V6(_)) => None,
            Err(_) if remote_dns => None,
            Err(_) => match resolve(host, port, true).await? {
                IpAddr::V4(ip) => Some(ip),
                IpAddr::V6(_) => None,
            },
        };
        match ip {
            Some(ip) => {
                req.extend_from_slice(&ip.octets());
                req.extend_from_slice(user.as_bytes());
                req.push(0);
            }
            None if remote_dns && host.parse::<IpAddr>().is_err() => {
                // SOCKS4a: 0.0.0.x and the host name after the user id.
                req.extend_from_slice(&[0, 0, 0, 1]);
                req.extend_from_slice(user.as_bytes());
                req.push(0);
                req.extend_from_slice(host.as_bytes());
                req.push(0);
            }
            None => {
                return Err(ProxyError::new(
                    ProxyErrorKind::Config,
                    format!("SOCKS4 proxies only reach IPv4 addresses; use a SOCKS5 proxy for {host}"),
                ));
            }
        }
        stream.write_all(&req).await.map_err(|e| self.io_error(e))?;
        let mut reply = [0u8; 8];
        stream.read_exact(&mut reply).await.map_err(|e| self.io_error(e))?;
        match reply[1] {
            0x5A => Ok(()),
            0x5C | 0x5D => Err(ProxyError::new(
                ProxyErrorKind::Auth,
                format!("The SOCKS4 proxy {self} rejected the request (identd check failed)"),
            )),
            _ => Err(ProxyError::new(
                ProxyErrorKind::Refused,
                format!("The SOCKS4 proxy {self} could not connect to {}", authority(host, port)),
            )),
        }
    }
}

/// A proxy's 407 answer to `CONNECT`.
struct AuthRequired {
    /// `HTTP 407 Proxy Authentication Required`, for messages.
    status: String,
    challenges: Vec<Challenge>,
    /// The answer's body was read and the connection kept open: the next
    /// `CONNECT` can go over it.
    reusable: bool,
}

/// The Digest challenge with the strongest supported algorithm.
fn best_digest(challenges: &[Challenge]) -> Option<&Challenge> {
    challenges
        .iter()
        .filter(|c| c.scheme == "digest")
        .filter_map(|c| crate::digest::parse_algorithm(c.param("algorithm").unwrap_or("MD5")).map(|(a, _)| (a, c)))
        .max_by_key(|(a, _)| a.strength())
        .map(|(_, c)| c)
}

/// Proxies (`scheme://host:port`) that asked for Digest: they no longer get
/// Basic credentials (the password in clear) with the first `CONNECT`.
static DIGEST_PROXIES: LazyLock<std::sync::Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn wants_digest(proxy: &ProxyServer) -> bool {
    DIGEST_PROXIES.lock().unwrap_or_else(|e| e.into_inner()).contains(&proxy.to_string())
}

fn remember_digest(proxy: &ProxyServer) {
    DIGEST_PROXIES.lock().unwrap_or_else(|e| e.into_inner()).insert(proxy.to_string());
}

/// How long a proxy that could not be reached is tried last (as in Chrome).
const UNREACHABLE_TTL: Duration = Duration::from_secs(300);

/// Proxies (`scheme://host:port`) that recently could not be reached.
static UNREACHABLE: LazyLock<std::sync::Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);

fn mark_unreachable(proxy: &str) {
    let mut bad = UNREACHABLE.lock().unwrap_or_else(|e| e.into_inner());
    bad.retain(|_, at| at.elapsed() < UNREACHABLE_TTL);
    bad.insert(proxy.to_string(), Instant::now());
}

fn is_unreachable(proxy: &str) -> bool {
    UNREACHABLE.lock().unwrap_or_else(|e| e.into_inner()).get(proxy).is_some_and(|at| at.elapsed() < UNREACHABLE_TTL)
}

/// `scheme://host:port` of a proxy URL, the key of the caches above.
fn proxy_key(url: &url::Url) -> Option<String> {
    ProxyServer::from_url(url).map(|p| p.to_string())
}

/// The `DIRECT` choice of a PAC script after its proxies failed.
async fn connect_direct(host: &str, port: u16) -> Result<ProxyStream, ProxyError> {
    let host = strip_brackets(host);
    match TcpStream::connect((host, port)).await {
        Ok(tcp) => {
            let _ = tcp.set_nodelay(true);
            Ok(ProxyStream::Tcp(tcp))
        }
        Err(e) => Err(ProxyError::new(
            ProxyErrorKind::Refused,
            format!(
                "Could not connect to {} directly (the proxies of the proxy auto-config script are unreachable): {e}",
                authority(host, port)
            ),
        )),
    }
}

/// Decides which proxy, if any, connections to a host go through.
pub struct ProxyResolver {
    matcher: Option<Arc<Matcher>>,
    /// Hosts that connect directly: the system proxy's (see
    /// `system_bypass`), or the bypass list with a PAC script.
    bypass: Option<Arc<Matcher>>,
    /// System proxy only: host names without a dot connect directly.
    bypass_simple: bool,
    /// The PAC script choosing the proxy, instead of `matcher`.
    pac: Option<Arc<Pac>>,
    /// Credentials for the proxies the custom PAC script picks.
    pac_credentials: Option<(String, String)>,
}

/// The system proxy configuration: environment variables and OS settings.
struct SystemProxy {
    matcher: Arc<Matcher>,
    bypass: Arc<Matcher>,
    simple: bool,
    pac: Option<Arc<Pac>>,
}

/// A matcher sending every host except those of `no` (`NO_PROXY` syntax)
/// to a dummy proxy: `intercept` is `None` for the hosts to bypass.
fn bypass_matcher(no: String) -> Arc<Matcher> {
    Arc::new(Matcher::builder().all("http://bypass.invalid:9").no(no).build())
}

/// How long the system proxy configuration is reused before it is read again.
const SYSTEM_PROXY_TTL: Duration = Duration::from_secs(60);

/// The system proxy configuration, read at most once a minute per process:
/// the first read of the macOS network settings can take seconds, and so
/// can loading a PAC script or looking for one (WPAD). Once read, an
/// outdated configuration stays in use while a background thread reads it
/// again, so connections never wait for that.
fn system_proxy() -> Arc<SystemProxy> {
    static CACHE: std::sync::Mutex<Option<(Instant, Arc<SystemProxy>)>> = std::sync::Mutex::new(None);
    static REFRESHING: AtomicBool = AtomicBool::new(false);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, s)) = cache.as_ref() {
        if at.elapsed() >= SYSTEM_PROXY_TTL && !REFRESHING.swap(true, Ordering::SeqCst) {
            let refresh = || {
                let s = load_system_proxy();
                *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), s));
                REFRESHING.store(false, Ordering::SeqCst);
            };
            if std::thread::Builder::new().name("system-proxy".into()).spawn(refresh).is_err() {
                REFRESHING.store(false, Ordering::SeqCst);
            }
        }
        return s.clone();
    }
    // The first read: concurrent callers wait for this one.
    let s = load_system_proxy();
    *cache = Some((Instant::now(), s.clone()));
    s
}

/// Reads the environment and the OS proxy settings, loading or discovering
/// their PAC script.
fn load_system_proxy() -> Arc<SystemProxy> {
    let os = os_proxy();
    let env = env_proxy_set();
    // Environment variables win; then a PAC script (browsers connect
    // directly when it cannot be loaded) or one found by auto-discovery,
    // the HTTP/HTTPS proxies and SOCKS.
    let pac = match &os.pac {
        _ if env => None,
        Some(source) => {
            load_pac(source).inspect_err(|e| tracing::warn!("system proxy: {e}; connecting without it")).ok()
        }
        None if os.auto_discover => wpad::discover(&os.search_domains, None).map(|(_, pac)| pac),
        None => None,
    };
    let matcher = match &os.socks {
        Some(socks) if !env && !os.manual => Matcher::builder().all(format!("socks5h://{socks}")).build(),
        _ => Matcher::from_system(),
    };
    Arc::new(SystemProxy {
        matcher: Arc::new(matcher),
        bypass: bypass_matcher(system_bypass(&os)),
        simple: os.exclude_simple,
        pac,
    })
}

/// Reads the system proxy configuration into the cache (it can take
/// seconds), for a background thread.
pub(crate) fn read_system_proxy() {
    system_proxy();
}

impl fmt::Debug for ProxyResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyResolver").field("direct", &(self.matcher.is_none() && self.pac.is_none())).finish()
    }
}

impl ProxyResolver {
    /// Reads the settings (and, for the system proxy, the environment and
    /// OS configuration, cached for a minute). Fails on an invalid custom
    /// proxy URL.
    pub fn new(settings: &ProxySettings) -> Result<ProxyResolver, ProxyError> {
        let mut resolver = ProxyResolver::direct();
        if let Some(location) = custom_pac(settings) {
            resolver.pac = Some(load_pac(&PacSource::Url(location.to_string()))?);
            resolver.bypass = Some(bypass_matcher(bypass_list(&settings.bypass)));
            if !settings.username.is_empty() {
                resolver.pac_credentials = Some((settings.username.clone(), settings.password.clone()));
            }
        } else if custom_enabled(settings) {
            let url = custom_proxy_url(settings)?;
            resolver.matcher =
                Some(Arc::new(Matcher::builder().all(String::from(url)).no(bypass_list(&settings.bypass)).build()));
        } else if settings.use_system {
            let system = system_proxy();
            resolver.matcher = Some(system.matcher.clone());
            resolver.bypass = Some(system.bypass.clone());
            resolver.bypass_simple = system.simple;
            resolver.pac = system.pac.clone();
        }
        Ok(resolver)
    }

    /// Always connects directly.
    pub fn direct() -> ProxyResolver {
        ProxyResolver { matcher: None, bypass: None, bypass_simple: false, pac: None, pac_credentials: None }
    }

    /// The PAC script's choices for `url` in its order (`None` is
    /// `DIRECT`, credentials included), except that proxies which recently
    /// could not be reached come last, as in Chrome. Empty without a script.
    fn pac_choices(&self, url: &url::Url) -> Vec<Option<url::Url>> {
        let (Some(pac), Some(host), Some(port)) = (self.pac.as_ref(), url.host_str(), url.port_or_known_default())
        else {
            return Vec::new();
        };
        let choices =
            pac.choose(&pac_url(url), url.scheme(), strip_brackets(host), port).into_iter().filter_map(|c| match c {
                PacChoice::Direct => Some(None),
                PacChoice::Proxy(mut proxy) => {
                    if let Some((user, password)) = &self.pac_credentials {
                        proxy.set_username(user).ok()?;
                        proxy.set_password(Some(password)).ok()?;
                    }
                    Some(Some(proxy))
                }
            });
        let unreachable = |c: &Option<url::Url>| c.as_ref().and_then(proxy_key).is_some_and(|k| is_unreachable(&k));
        let (mut ordered, last): (Vec<_>, Vec<_>) = choices.partition(|c| !unreachable(c));
        ordered.extend(last);
        ordered
    }

    /// The proxy URL (credentials included) for a request to `url`, for
    /// reqwest's custom proxies. Of a PAC script's choices only the first
    /// is taken (reqwest takes one proxy per connection); see
    /// [`skip_unreachable_proxy`] for the others.
    fn proxy_url(&self, url: &reqwest::Url) -> Option<url::Url> {
        let host = url.host_str()?;
        let port = url.port_or_known_default()?;
        let uri = self.target_uri(host, port, url.scheme() == "https")?;
        if self.pac.is_some() {
            return self.pac_choices(url).into_iter().next().flatten();
        }
        let i = self.matcher.as_ref()?.intercept(&uri)?;
        let mut proxy = url::Url::parse(&i.uri().to_string()).ok()?;
        let (user, password) = match (i.raw_auth(), i.basic_auth()) {
            (Some((u, p)), _) => (u.to_string(), p.to_string()),
            (None, Some(v)) => decode_basic(v)?,
            (None, None) => return Some(proxy),
        };
        proxy.set_username(&user).ok()?;
        proxy.set_password(Some(&password)).ok()?;
        Some(proxy)
    }

    /// `scheme://host:port/` for the matcher, or `None` when the system
    /// proxy's exceptions send `host` directly.
    fn target_uri(&self, host: &str, port: u16, tls: bool) -> Option<http::Uri> {
        let host = strip_brackets(host);
        let scheme = if tls { "https" } else { "http" };
        let uri: http::Uri = format!("{scheme}://{}/", authority(host, port)).parse().ok()?;
        if self.bypass.as_ref().is_some_and(|b| b.intercept(&uri).is_none()) {
            return None;
        }
        if self.bypass_simple && !host.contains('.') && !host.contains(':') && host != "localhost" {
            return None;
        }
        Some(uri)
    }

    /// The proxy for a connection to `host:port`; `tls` selects the HTTPS
    /// proxy (`wss`, `mqtts`, `grpcs`, ...) rather than the HTTP one.
    /// `None` means a direct connection. The other choices of a PAC script
    /// come with it (see [`ProxyServer::tunnel`]).
    pub fn proxy_for(&self, host: &str, port: u16, tls: bool) -> Option<ProxyServer> {
        let uri = self.target_uri(host, port, tls)?;
        if self.pac.is_some() {
            let url = url::Url::parse(&uri.to_string()).ok()?;
            let mut choices = self.pac_choices(&url).into_iter().filter_map(|c| match c {
                None => Some(None),
                Some(u) => ProxyServer::from_url(&u).map(Some),
            });
            let mut first = choices.next()??;
            first.fallbacks = choices.collect();
            return Some(first);
        }
        ProxyServer::from_intercept(&self.matcher.as_ref()?.intercept(&uri)?)
    }
}

/// For the HTTP engine, after a request to `url` failed to connect: when a
/// PAC script chose the proxy it went through, that proxy is remembered as
/// unreachable (so it is tried last), and the result tells whether the
/// script offers a choice that is not known to be unreachable, to retry
/// with (reqwest asks for the proxy again for the new connection).
pub(crate) fn skip_unreachable_proxy(settings: &ProxySettings, url: &url::Url) -> bool {
    let Ok(resolver) = ProxyResolver::new(settings) else { return false };
    if resolver.pac.is_none() {
        return false;
    }
    // A direct connection that failed is not the proxy's fault.
    let Some(used) = resolver.proxy_url(url).as_ref().and_then(proxy_key) else { return false };
    mark_unreachable(&used);
    match resolver.proxy_url(url) {
        None => true,
        Some(next) => proxy_key(&next).is_some_and(|k| !is_unreachable(&k)),
    }
}

/// Web Proxy Auto-Discovery over DNS, as the system proxy runs it when the
/// OS settings ask for it: the first `http://wpad.<domain>/wpad.dat` of
/// the search domains `domains` that serves a PAC script (see the `wpad`
/// module for which domains are tried), and a resolver using that script
/// (loopback connects directly). Results are cached like PAC downloads.
/// It blocks for up to about 3 s per candidate.
pub fn discover_wpad(domains: &[String]) -> Option<(String, ProxyResolver)> {
    discover_wpad_with(domains, None)
}

/// The machine's DNS search domains (and its own domain), where
/// [`discover_wpad`] looks: the global DNS configuration on macOS, the
/// registry on Windows (suffix search list, primary and adapter domains),
/// `/etc/resolv.conf` elsewhere.
pub fn dns_search_domains() -> Vec<String> {
    wpad::search_domains()
}

/// [`discover_wpad`] with every candidate host resolved to `addr`, for
/// tests (the candidates are `wpad.<domain>` on port 80).
#[doc(hidden)]
pub fn discover_wpad_at(domains: &[String], addr: SocketAddr) -> Option<(String, ProxyResolver)> {
    discover_wpad_with(domains, Some(addr))
}

fn discover_wpad_with(domains: &[String], resolve_to: Option<SocketAddr>) -> Option<(String, ProxyResolver)> {
    let (url, pac) = wpad::discover(domains, resolve_to)?;
    let mut resolver = ProxyResolver::direct();
    resolver.pac = Some(pac);
    resolver.bypass = Some(bypass_matcher(system_bypass(&OsProxy::default())));
    Some((url, resolver))
}

/// [`ProxyResolver::new`] followed by [`ProxyResolver::proxy_for`].
pub fn proxy_for(
    settings: &ProxySettings,
    host: &str,
    port: u16,
    tls: bool,
) -> Result<Option<ProxyServer>, ProxyError> {
    Ok(ProxyResolver::new(settings)?.proxy_for(host, port, tls))
}

/// The proxy a connection to `url` (`ws`, `wss`, `http`, `https`, `mqtt`,
/// `mqtts`, ...; `default_scheme` when it has none) goes through, for
/// messages. `None` for direct connections and unusable URLs or settings.
pub fn proxy_for_url(settings: &ProxySettings, url: &str, default_scheme: &str) -> Option<ProxyServer> {
    let raw = url.trim();
    let url = if raw.contains("://") { raw.to_string() } else { format!("{default_scheme}://{raw}") };
    let url = url::Url::parse(&url).ok()?;
    let scheme = url.scheme();
    let tls = matches!(scheme, "https" | "wss" | "mqtts" | "ssl" | "tls" | "grpcs");
    let port = url.port_or_known_default().or(match scheme {
        "mqtt" | "tcp" => Some(1883),
        "mqtts" | "ssl" | "tls" => Some(8883),
        _ => None,
    })?;
    proxy_for(settings, url.host_str()?, port, tls).ok().flatten()
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// A TCP connection to a server, direct or tunnelled through a proxy (the
/// tunnel through an HTTPS proxy runs inside TLS to the proxy).
pub enum ProxyStream {
    Tcp(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl fmt::Debug for ProxyStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ProxyStream::Tcp(_) => "ProxyStream::Tcp",
            ProxyStream::Tls(_) => "ProxyStream::Tls",
        })
    }
}

impl AsyncRead for ProxyStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ProxyStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            ProxyStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ProxyStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ProxyStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            ProxyStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ProxyStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            ProxyStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ProxyStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            ProxyStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ProxyStream::Tcp(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            ProxyStream::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            ProxyStream::Tcp(s) => s.is_write_vectored(),
            ProxyStream::Tls(s) => s.is_write_vectored(),
        }
    }
}

/// Opens a TCP connection to `host:port`, through `proxy` when given (see
/// [`ProxyServer::tunnel`]). Errors of direct connections are returned as
/// they are, so `ConnectionRefused` stays recognizable; proxy failures are
/// a [`ProxyError`] inside an `io::Error` (see [`find_proxy_error`]).
pub async fn dial(
    proxy: Option<&ProxyServer>,
    host: &str,
    port: u16,
    tls: &TlsSetup,
    verify: bool,
) -> io::Result<ProxyStream> {
    match proxy {
        Some(p) => p.tunnel(host, port, tls, verify).await.map_err(io::Error::from),
        None => {
            let tcp = TcpStream::connect((strip_brackets(host), port)).await?;
            let _ = tcp.set_nodelay(true);
            Ok(ProxyStream::Tcp(tcp))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolves a target host locally (`socks5://`, `socks4://`).
async fn resolve(host: &str, port: u16, ipv4_only: bool) -> Result<IpAddr, ProxyError> {
    let addrs = tokio::net::lookup_host((host, port)).await.map_err(|_| {
        ProxyError::new(ProxyErrorKind::Dns, format!("Could not resolve host \"{host}\" (DNS lookup failed)"))
    })?;
    let mut first = None;
    for a in addrs {
        if a.is_ipv4() || !ipv4_only {
            return Ok(a.ip());
        }
        first.get_or_insert(a.ip());
    }
    match first {
        Some(ip) => Ok(ip),
        None => Err(ProxyError::new(ProxyErrorKind::Dns, format!("Could not resolve host \"{host}\" (no address)"))),
    }
}

/// `host:port`, with brackets around IPv6 addresses.
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

fn basic_auth(user: &str, password: &str) -> HeaderValue {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    let mut v = HeaderValue::from_str(&format!("Basic {encoded}")).expect("base64 is a valid header value");
    v.set_sensitive(true);
    v
}

/// The username and password of a `Basic` header value.
fn decode_basic(v: &HeaderValue) -> Option<(String, String)> {
    use base64::Engine as _;
    let encoded = v.to_str().ok()?.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (u, p) = text.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

/// The URL a PAC script sees: without credentials and fragment, and for
/// TLS connections without path and query (as Chrome does, so scripts
/// cannot see private parts of HTTPS URLs).
fn pac_url(url: &url::Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    if matches!(url.scheme(), "https" | "wss") {
        url.set_path("/");
        url.set_query(None);
    }
    url.to_string()
}

/// A proxy's reason phrase, printable and short.
fn sanitize(s: &str) -> String {
    let s: String = s.trim().chars().filter(|c| !c.is_control()).take(120).collect();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(url: &str) -> ProxySettings {
        ProxySettings { custom_enabled: true, custom_url: url.into(), ..Default::default() }
    }

    #[test]
    fn pac_urls_hide_private_parts() {
        let u = |s: &str| pac_url(&url::Url::parse(s).unwrap());
        assert_eq!(u("http://u:p@a.test:8080/x?y=1#f"), "http://a.test:8080/x?y=1");
        assert_eq!(u("https://u:p@a.test/secret?token=1"), "https://a.test/");
        assert_eq!(u("wss://a.test:9443/socket?k=v"), "wss://a.test:9443/");
    }

    #[test]
    fn os_exceptions_as_no_proxy() {
        assert_eq!(exception_to_no_proxy("*.local"), ".local");
        assert_eq!(exception_to_no_proxy("169.254/16"), "169.254.0.0/16");
        assert_eq!(exception_to_no_proxy("10/8"), "10.0.0.0/8");
        assert_eq!(exception_to_no_proxy(" intranet.example "), "intranet.example");
        assert_eq!(exception_to_no_proxy("fe80::/10"), "fe80::/10");
        // Reading the system settings never fails, whatever they are.
        let list = system_bypass(&os_proxy());
        assert!(list.starts_with("localhost,.localhost,127.0.0.0/8,::1"), "{list}");
        let os = OsProxy { exceptions: vec!["*.local".into(), "169.254/16".into()], ..Default::default() };
        assert_eq!(system_bypass(&os), "localhost,.localhost,127.0.0.0/8,::1,.local,169.254.0.0/16");
    }

    #[test]
    fn pac_scripts_choose_per_host() {
        let script = r#"
            function FindProxyForURL(url, host) {
              if (shExpMatch(host, '*.corp.test')) return 'PROXY proxy.corp.test:3128; DIRECT';
              if (dnsDomainIs(host, '.socks.test')) return 'SOCKS5 [::1]:1080';
              if (url.indexOf('/via-https') >= 0) return 'HTTPS secure.test:8443';
              return 'DIRECT';
            }
        "#;
        let mut r = ProxyResolver::direct();
        r.pac = Some(load_pac(&PacSource::Script(script.into())).unwrap());
        r.bypass = Some(bypass_matcher("localhost,127.0.0.0/8,.local".into()));
        let p = r.proxy_for("api.corp.test", 443, true).unwrap();
        assert_eq!((p.kind(), p.host(), p.port()), (ProxyKind::Http, "proxy.corp.test", 3128));
        let p = r.proxy_for("a.socks.test", 1883, false).unwrap();
        assert_eq!((p.kind(), p.host(), p.port()), (ProxyKind::Socks5 { remote_dns: true }, "::1", 1080));
        assert_eq!(r.proxy_for("example.com", 80, false), None);
        // The bypass list wins over the script.
        assert_eq!(r.proxy_for("printer.local", 80, false), None);
        assert_eq!(
            r.proxy_for("localhost.corp.test", 80, false).map(|p| p.to_string()),
            Some("http://proxy.corp.test:3128".into())
        );
        // reqwest's per-request choice sees the path of plain HTTP URLs only.
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert_eq!(
            r.proxy_url(&url("http://x.test/via-https")).map(String::from),
            Some("https://secure.test:8443/".into())
        );
        assert_eq!(r.proxy_url(&url("https://x.test/via-https")), None);
        assert_eq!(r.proxy_url(&url("http://127.0.0.1/via-https")), None);
    }

    #[test]
    fn unreachable_pac_proxies_are_tried_last() {
        let script = "function FindProxyForURL() { return 'PROXY a.order.test:1; SOCKS5 b.order.test:2; DIRECT'; }";
        let mut r = ProxyResolver::direct();
        r.pac = Some(load_pac(&PacSource::Script(script.into())).unwrap());
        let order = |r: &ProxyResolver| {
            let p = r.proxy_for("x.example", 443, true);
            let mut names = vec![p.as_ref().map_or("DIRECT".into(), ToString::to_string)];
            for f in p.iter().flat_map(|p| p.fallbacks()) {
                names.push(f.as_ref().map_or("DIRECT".into(), ToString::to_string));
            }
            names
        };
        let url = reqwest::Url::parse("http://x.example/").unwrap();
        assert_eq!(order(&r), ["http://a.order.test:1", "socks5h://b.order.test:2", "DIRECT"]);
        assert_eq!(r.proxy_url(&url).map(String::from), Some("http://a.order.test:1/".into()));
        mark_unreachable("http://a.order.test:1");
        assert_eq!(order(&r), ["socks5h://b.order.test:2", "DIRECT", "http://a.order.test:1"]);
        assert_eq!(r.proxy_url(&url).map(String::from), Some("socks5h://b.order.test:2".into()));
        mark_unreachable("socks5h://b.order.test:2");
        assert_eq!(order(&r), ["DIRECT"]);
        assert_eq!(r.proxy_url(&url), None);
        assert!(!is_unreachable("http://other.order.test:1"));
    }

    #[test]
    fn digest_challenges_and_basic_credentials() {
        let challenges = crate::challenge::parse_challenges([
            r#"Basic realm="p", Digest realm="p", nonce="1", algorithm=MD5"#,
            r#"Digest realm="p", nonce="2", algorithm=SHA-256, Digest realm="p", nonce="3", algorithm=SHA-1"#,
        ]);
        assert_eq!(best_digest(&challenges).and_then(|c| c.param("nonce")), Some("2"));
        assert!(best_digest(&challenges[..1]).is_none());
        assert_eq!(decode_basic(&basic_auth("alice", "p@ss:word")), Some(("alice".into(), "p@ss:word".into())));
        assert_eq!(decode_basic(&HeaderValue::from_static("Bearer x")), None);
    }

    #[test]
    fn simple_host_names() {
        let mut r = ProxyResolver::direct();
        r.matcher = Some(Arc::new(Matcher::builder().all("http://proxy.example:3128").build()));
        r.bypass_simple = true;
        assert!(r.proxy_for("intranet", 80, false).is_none());
        assert!(r.proxy_for("api.example.com", 80, false).is_some());
        r.bypass_simple = false;
        assert!(r.proxy_for("intranet", 80, false).is_some());
    }

    #[test]
    fn custom_proxy_selection_and_bypass() {
        let mut s = custom("proxy.example:3128");
        s.bypass = "localhost, *.internal; 10.0.0.0/8".into();
        let r = ProxyResolver::new(&s).unwrap();
        let p = r.proxy_for("example.com", 80, false).unwrap();
        assert_eq!((p.kind(), p.host(), p.port()), (ProxyKind::Http, "proxy.example", 3128));
        assert_eq!(p.to_string(), "http://proxy.example:3128");
        // The custom proxy takes TLS targets too.
        assert!(r.proxy_for("example.com", 443, true).is_some());
        for direct in ["localhost", "a.internal", "internal", "10.1.2.3"] {
            assert_eq!(r.proxy_for(direct, 80, false), None, "{direct}");
        }
        assert!(r.proxy_for("127.0.0.1", 80, false).is_some(), "loopback is not exempt by itself");
        assert!(r.proxy_for("[::1]", 80, false).is_some());
        assert_eq!(bypass_list(" a ,*.b;; c "), "a,.b,c");
    }

    #[test]
    fn proxy_urls() {
        let p = |url: &str| proxy_for(&custom(url), "example.com", 443, true);
        let s = p("socks5://127.0.0.1").unwrap().unwrap();
        assert_eq!((s.kind(), s.port()), (ProxyKind::Socks5 { remote_dns: false }, 1080));
        let s = p("socks5h://[::1]:1081").unwrap().unwrap();
        assert_eq!((s.kind(), s.host(), s.port()), (ProxyKind::Socks5 { remote_dns: true }, "::1", 1081));
        assert_eq!(s.to_string(), "socks5h://[::1]:1081");
        let s = p("https://secure.proxy").unwrap().unwrap();
        assert_eq!((s.kind(), s.port()), (ProxyKind::Https, 443));
        let s = p("127.0.0.1:8080").unwrap().unwrap();
        assert_eq!((s.kind(), s.port()), (ProxyKind::Http, 8080));
        let err = p("ftp://user:hunter2@proxy").unwrap_err();
        assert_eq!(err.kind, ProxyErrorKind::Config);
        assert!(err.message.contains("ftp://") && !err.message.contains("hunter2"), "{err}");
        let err = p("http://user:hunter2@[bad").unwrap_err();
        assert!(!err.message.contains("hunter2"), "{err}");
        assert!(p("socks5://").unwrap_err().message.contains("missing host"));
    }

    #[test]
    fn credentials_and_redaction() {
        let mut s = custom("http://proxy:3128");
        s.username = "alice".into();
        s.password = "p@ss:word".into();
        let p = proxy_for(&s, "example.com", 80, false).unwrap().unwrap();
        assert!(p.has_credentials());
        assert_eq!(p.credentials, Credentials::Basic(basic_auth("alice", "p@ss:word")));
        let debug = format!("{p:?} {p}");
        assert!(!debug.contains("p@ss") && !debug.contains("alice"), "{debug}");
        // Credentials in the URL are used when the settings have none.
        let p = proxy_for(&custom("socks5h://bob:s%40cret@proxy"), "example.com", 80, false).unwrap().unwrap();
        assert_eq!(p.credentials, Credentials::Raw("bob".into(), "s@cret".into()));
        assert!(!format!("{p:?}").contains("s@cret"));
    }

    #[test]
    fn precedence() {
        // Neither custom nor system: direct.
        let none = ProxySettings::default();
        assert_eq!(proxy_for(&none, "example.com", 80, false).unwrap(), None);
        // A custom proxy that is not enabled is ignored.
        let disabled = ProxySettings { custom_url: "http://proxy:1".into(), ..Default::default() };
        assert_eq!(proxy_for(&disabled, "example.com", 80, false).unwrap(), None);
    }
}
