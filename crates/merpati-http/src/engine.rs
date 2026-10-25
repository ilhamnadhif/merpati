//! The HTTP engine.
//!
//! Redirects are followed here rather than by reqwest so every hop is
//! recorded and per-request settings (max redirects, method rewriting,
//! forwarding of `Authorization`) apply. Bodies larger than the configured
//! limit are streamed to a temporary file, so a 100 MB response costs a few
//! megabytes of memory.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tokio_util::sync::CancellationToken;
use url::Url;

use merpati_core::model::HttpVersion;
use merpati_core::prepared::{MultipartValue, PreparedBody, PreparedRequest, ResolvedSettings};
use merpati_core::response::{HttpResponse, ResponseBody, Timings};
use merpati_core::settings::{AppSettings, CertificateSettings, ProxySettings};

use crate::certs::SecretResolver;
use crate::cookies::CookieJar;
use crate::error::{HttpError, HttpErrorKind, classify};

#[derive(Clone, Debug, PartialEq)]
pub struct EngineConfig {
    pub proxy: ProxySettings,
    pub certificates: CertificateSettings,
    /// Bodies larger than this are written to a temporary file.
    pub max_body_in_memory: u64,
    /// Where temporary body files go (system temp dir when `None`).
    pub temp_dir: Option<PathBuf>,
    /// Expands `{{vault:name}}` passphrases of client certificates (PKCS#12
    /// bundles, encrypted PEM keys). Without it only literal passphrases work.
    pub secret_resolver: Option<SecretResolver>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig::from_settings(&AppSettings::default())
    }
}

impl EngineConfig {
    pub fn from_settings(s: &AppSettings) -> Self {
        EngineConfig {
            proxy: s.proxy.clone(),
            certificates: s.certificates.clone(),
            max_body_in_memory: s.max_body_in_memory_bytes,
            temp_dir: None,
            secret_resolver: None,
        }
    }
}

/// The settings keep the proxy password in the vault (`{{vault:name}}`);
/// the engine, and everything reading its config, needs the password.
fn with_proxy_password(mut config: EngineConfig) -> EngineConfig {
    if config.proxy.password.contains("{{")
        && let Some(secrets) = &config.secret_resolver
    {
        config.proxy.password = secrets.resolve(&config.proxy.password);
        if config.proxy.password.contains("{{vault:") {
            tracing::warn!("the proxy password refers to a vault secret that is not set");
        }
    }
    config
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub received: u64,
    /// From `Content-Length`, when known.
    pub total: Option<u64>,
}

type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

#[derive(Clone, Default)]
pub struct SendOptions {
    pub cancel: CancellationToken,
    pub progress: Option<ProgressFn>,
}

impl SendOptions {
    pub fn with_cancel(cancel: CancellationToken) -> Self {
        SendOptions { cancel, progress: None }
    }
}

/// How many times a request is retried with the next choice of a PAC script
/// when its proxy cannot be reached.
const MAX_PROXY_FAILOVERS: u32 = 3;
/// How long reading the server certificate through a proxy may take (see
/// [`SendOutcome::peer_certificate`]) when the request has no timeout.
const PEER_CERTIFICATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What actually went over the wire for the final hop.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SentRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body_size: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct SendOutcome {
    pub response: HttpResponse,
    pub sent: SentRequest,
    /// DER of the server's leaf TLS certificate when the final hop was
    /// `https`, for TLS channel bindings (NTLM Extended Protection). Kept out
    /// of `response` so it never reaches history or exports. `None` for plain
    /// HTTP. Through a proxy reqwest does not report the tunnelled
    /// connection's certificate, so it is only there for a 401 asking for
    /// NTLM or Negotiate, read over a TLS handshake of its own through the
    /// same proxy.
    pub peer_certificate: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ClientKey {
    verify_ssl: bool,
    version: HttpVersion,
    client_cert: Option<usize>,
    cookies: bool,
    unix_socket: Option<PathBuf>,
}

pub struct HttpEngine {
    config: RwLock<EngineConfig>,
    cookies: Arc<CookieJar>,
    clients: Mutex<HashMap<ClientKey, reqwest::Client>>,
}

impl HttpEngine {
    pub fn new(config: EngineConfig, cookies: Arc<CookieJar>) -> Self {
        crate::tls::install_provider();
        // The system proxy settings and the OS trust store are read on one
        // background thread, one after the other (reading both at once
        // contends inside the macOS frameworks), so the first connection
        // does not wait for them.
        let system_proxy = config.proxy.use_system && !config.proxy.custom_enabled;
        let _ = std::thread::Builder::new().name("net-settings".into()).spawn(move || {
            if system_proxy {
                crate::proxy::read_system_proxy();
            }
            crate::tls::read_native_roots();
        });
        HttpEngine { config: RwLock::new(with_proxy_password(config)), cookies, clients: Mutex::new(HashMap::new()) }
    }

    pub fn cookies(&self) -> &Arc<CookieJar> {
        &self.cookies
    }

    pub fn config(&self) -> EngineConfig {
        self.config.read().clone()
    }

    /// Replaces proxy/certificate settings; cached clients are rebuilt lazily.
    pub fn set_config(&self, config: EngineConfig) {
        *self.config.write() = with_proxy_password(config);
        self.clients.lock().clear();
    }

    /// A client with default settings and no cookie jar, for auxiliary calls.
    /// It presents no client certificate; see [`HttpEngine::client_for_url`].
    pub fn default_client(&self) -> Result<reqwest::Client, HttpError> {
        let key = ClientKey {
            verify_ssl: true,
            version: HttpVersion::Auto,
            client_cert: None,
            cookies: false,
            unix_socket: None,
        };
        self.client(key)
    }

    /// [`HttpEngine::default_client`] presenting the client certificate
    /// configured for the host of `url` (same rules as requests), for
    /// auxiliary calls to one endpoint such as OAuth 2.0 token requests
    /// (mutual TLS, RFC 8705).
    pub fn client_for_url(&self, url: &str) -> Result<reqwest::Client, HttpError> {
        let key = ClientKey {
            verify_ssl: true,
            version: HttpVersion::Auto,
            client_cert: self.client_cert_for(Url::parse(url.trim()).ok().as_ref()),
            cookies: false,
            unix_socket: None,
        };
        self.client(key)
    }

    /// The client certificate configured for `url`'s host and port.
    fn client_cert_for(&self, url: Option<&Url>) -> Option<usize> {
        let url = url?;
        // Client certificates only matter for TLS (and loading one may need
        // its passphrase), so plain http:// requests never select one.
        if url.scheme() != "https" {
            return None;
        }
        crate::certs::find_client_certificate_index(
            &self.config.read().certificates,
            url.host_str()?,
            url.port_or_known_default(),
        )
    }

    /// A client honoring the given request settings (TLS verification, HTTP
    /// version, cookies) and the client certificate configured for `url`.
    pub fn client_for(&self, settings: &ResolvedSettings, url: &str) -> Result<reqwest::Client, HttpError> {
        let (unix, url) = match unix_target(url) {
            Some((sock, u)) => (Some(sock), u),
            None => (None, url.to_string()),
        };
        self.client_for_hop(settings, Url::parse(&url).ok().as_ref(), unix)
    }

    /// [`HttpEngine::client_for`] for an already parsed URL (one hop of a
    /// redirect chain) and the local socket every hop goes through.
    fn client_for_hop(
        &self,
        settings: &ResolvedSettings,
        url: Option<&Url>,
        unix_socket: Option<PathBuf>,
    ) -> Result<reqwest::Client, HttpError> {
        self.client(ClientKey {
            verify_ssl: settings.verify_ssl,
            version: settings.http_version,
            client_cert: self.client_cert_for(url),
            cookies: !settings.disable_cookies,
            unix_socket,
        })
    }

    fn client(&self, key: ClientKey) -> Result<reqwest::Client, HttpError> {
        if let Some(c) = self.clients.lock().get(&key) {
            return Ok(c.clone());
        }
        let client = build_client(&self.config.read(), &key, &self.cookies)?;
        self.clients.lock().insert(key, client.clone());
        Ok(client)
    }

    /// Sends a prepared request (auth already applied) and reads the full
    /// response.
    pub async fn send(&self, req: &PreparedRequest, opts: &SendOptions) -> Result<SendOutcome, HttpError> {
        let start = Instant::now();
        let (unix, initial_url) = match unix_target(&req.url) {
            Some((sock, u)) => (Some(sock), u),
            None => (None, req.url.clone()),
        };
        let mut url = Url::parse(&initial_url)
            .map_err(|e| HttpError::new(HttpErrorKind::InvalidUrl, format!("Invalid URL \"{}\": {e}", req.url)))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(HttpError::new(
                HttpErrorKind::InvalidUrl,
                format!("Unsupported protocol \"{}:\" (use http or https)", url.scheme()),
            ));
        }
        let mut client = self.client_for_hop(&req.settings, Some(&url), unix.clone())?;
        let mut method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|_| HttpError::new(HttpErrorKind::InvalidHeader, format!("Invalid method \"{}\"", req.method)))?;
        let mut headers = req.headers.clone();
        let mut send_body = true;
        let mut redirects = Vec::new();
        let timeout_ms = req.settings.timeout.map(|t| t.as_millis());
        let mut proxy_failovers = 0;

        loop {
            let host = url.host_str().unwrap_or("").to_string();
            let mut builder = client.request(method.clone(), url.clone());
            if let Some(t) = req.settings.timeout {
                let remaining = t.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    return Err(HttpError::new(
                        HttpErrorKind::Timeout,
                        format!("Request to {host} timed out after {} ms", t.as_millis()),
                    ));
                }
                builder = builder.timeout(remaining);
            }
            let mut body_headers = headers.clone();
            let mut body_size = None;
            if send_body {
                match &req.body {
                    PreparedBody::None => {}
                    PreparedBody::Bytes(b) => {
                        body_size = Some(b.len() as u64);
                        builder = builder.body(b.clone());
                    }
                    PreparedBody::File(path) => {
                        let file = tokio::fs::File::open(path).await.map_err(|e| {
                            HttpError::new(
                                HttpErrorKind::File,
                                format!("Could not read body file {}: {e}", path.display()),
                            )
                        })?;
                        let len = file.metadata().await.map(|m| m.len()).ok();
                        if let Some(len) = len {
                            body_size = Some(len);
                            if !body_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
                                body_headers.push(("Content-Length".into(), len.to_string()));
                            }
                        }
                        let stream = tokio_util::io::ReaderStream::new(file);
                        builder = builder.body(reqwest::Body::wrap_stream(stream));
                    }
                    PreparedBody::Multipart(parts) => {
                        // A user-set multipart Content-Type lacks the body's boundary
                        // (reqwest generates it); let reqwest set the header alone.
                        body_headers.retain(|(k, v)| {
                            !(k.eq_ignore_ascii_case("content-type")
                                && v.trim().to_ascii_lowercase().starts_with("multipart/form-data"))
                        });
                        let mut form = reqwest::multipart::Form::new();
                        for part in parts {
                            let mut p = match &part.value {
                                MultipartValue::Text(t) => reqwest::multipart::Part::text(t.clone()),
                                MultipartValue::File(path) => {
                                    reqwest::multipart::Part::file(path).await.map_err(|e| {
                                        HttpError::new(
                                            HttpErrorKind::File,
                                            format!("Could not read form-data file {}: {e}", path.display()),
                                        )
                                    })?
                                }
                            };
                            if let Some(ct) = &part.content_type {
                                p = p.mime_str(ct).map_err(|e| {
                                    HttpError::new(
                                        HttpErrorKind::InvalidHeader,
                                        format!("Invalid content type \"{ct}\": {e}"),
                                    )
                                })?;
                            }
                            form = form.part(part.name.clone(), p);
                        }
                        builder = builder.headers(header_map(&body_headers)?);
                        builder = builder.multipart(form);
                        body_headers.clear();
                    }
                }
            }
            if !body_headers.is_empty() {
                builder = builder.headers(header_map(&body_headers)?);
            }
            let rq = builder.build().map_err(|e| classify(&e, &host, timeout_ms))?;
            let mut sent = SentRequest {
                method: rq.method().to_string(),
                url: rq.url().to_string(),
                headers: rq
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                    .collect(),
                body_size,
            };
            if !req.settings.disable_cookies
                && !sent.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("cookie"))
                && let Some(c) = reqwest::cookie::CookieStore::cookies(self.cookies.as_ref(), rq.url())
            {
                sent.headers.push(("cookie".into(), String::from_utf8_lossy(c.as_bytes()).into_owned()));
            }

            let result = tokio::select! {
                r = client.execute(rq) => r,
                _ = opts.cancel.cancelled() => return Err(HttpError::cancelled()),
            };
            let resp = match result {
                Ok(resp) => resp,
                Err(e) => {
                    // The proxy a PAC script chose cannot be reached: retry
                    // with its next choice, like browsers (nothing was sent).
                    if proxy_failovers < MAX_PROXY_FAILOVERS && unix.is_none() && crate::error::is_unreachable(&e) {
                        let proxy = self.config.read().proxy.clone();
                        if crate::proxy::skip_unreachable_proxy(&proxy, &url) {
                            proxy_failovers += 1;
                            continue;
                        }
                    }
                    return Err(classify(&e, &host, timeout_ms));
                }
            };
            let status = resp.status().as_u16();

            // A Location with raw UTF-8 is decoded as browsers do; `join` percent-encodes it.
            if req.settings.follow_redirects
                && matches!(status, 301 | 302 | 303 | 307 | 308)
                && let Some(next) =
                    resp.headers().get(reqwest::header::LOCATION).map(|l| String::from_utf8_lossy(l.as_bytes()))
            {
                if redirects.len() as u32 >= req.settings.max_redirects {
                    return Err(HttpError::new(
                        HttpErrorKind::TooManyRedirects,
                        format!("Exceeded the maximum of {} redirects", req.settings.max_redirects),
                    ));
                }
                let next_url = url.join(&next).map_err(|e| {
                    HttpError::new(HttpErrorKind::InvalidUrl, format!("Invalid redirect location \"{next}\": {e}"))
                })?;
                let to_get = status == 303 && method != reqwest::Method::HEAD
                    || matches!(status, 301 | 302)
                        && !req.settings.follow_original_method
                        && method != reqwest::Method::GET
                        && method != reqwest::Method::HEAD;
                if to_get {
                    method = reqwest::Method::GET;
                    send_body = false;
                    headers.retain(|(k, _)| {
                        !k.eq_ignore_ascii_case("content-type") && !k.eq_ignore_ascii_case("content-length")
                    });
                }
                if next_url.origin() != url.origin() && !req.settings.follow_auth_header {
                    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
                }
                redirects.push(url.to_string());
                url = next_url;
                // The client certificate is chosen per host: the next hop may
                // need another one (or none), e.g. after http:// -> https://.
                client = self.client_for_hop(&req.settings, Some(&url), unix.clone())?;
                continue;
            }

            let ttfb_ms = start.elapsed().as_secs_f64() * 1000.0;
            let version = match resp.version() {
                reqwest::Version::HTTP_09 => "HTTP/0.9",
                reqwest::Version::HTTP_10 => "HTTP/1.0",
                reqwest::Version::HTTP_11 => "HTTP/1.1",
                reqwest::Version::HTTP_2 => "HTTP/2",
                reqwest::Version::HTTP_3 => "HTTP/3",
                _ => "HTTP",
            }
            .to_string();
            let status_text = resp.status().canonical_reason().unwrap_or("").to_string();
            let resp_headers: Vec<(String, String)> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect();
            let header_size = (version.len() + 1 + 3 + 1 + status_text.len() + 2) as u64
                + resp_headers.iter().map(|(k, v)| (k.len() + 2 + v.len() + 2) as u64).sum::<u64>()
                + 2;
            let remote_addr = resp.remote_addr().map(|a| a.to_string());
            // An http:// request through an https:// proxy reports the proxy's certificate.
            let https = resp.url().scheme() == "https";
            let mut peer_certificate = https
                .then(|| resp.extensions().get::<reqwest::tls::TlsInfo>()?.peer_certificate().map(<[u8]>::to_vec))
                .flatten();
            let final_url = resp.url().to_string();
            let body = self.read_body(resp, opts, &host, timeout_ms).await?;
            let total_ms = start.elapsed().as_secs_f64() * 1000.0;
            // Through a proxy reqwest reports no certificate; NTLM needs it
            // for its channel binding.
            if peer_certificate.is_none()
                && https
                && status == 401
                && unix.is_none()
                && wants_tls_binding(&resp_headers)
            {
                let limit =
                    req.settings.timeout.map_or(PEER_CERTIFICATE_TIMEOUT, |t| t.saturating_sub(start.elapsed()));
                let fetch = tokio::time::timeout(limit, self.tunnelled_peer_certificate(req.settings.verify_ssl, &url));
                peer_certificate = tokio::select! {
                    c = fetch => c.ok().flatten(),
                    _ = opts.cancel.cancelled() => return Err(HttpError::cancelled()),
                };
            }
            let response = HttpResponse {
                status,
                status_text,
                version,
                headers: resp_headers,
                body,
                url: final_url,
                timings: Timings { ttfb_ms, total_ms },
                header_size,
                remote_addr,
                redirects,
            };
            return Ok(SendOutcome { response, sent, peer_certificate });
        }
    }

    /// The leaf certificate of the HTTPS server of `url`, read over a TLS
    /// handshake of its own through the proxy the request went through,
    /// with the same trust settings and client certificate. `None` without
    /// a proxy or when it fails.
    async fn tunnelled_peer_certificate(&self, verify: bool, url: &Url) -> Option<Vec<u8>> {
        let host = url.host_str()?.trim_start_matches('[').trim_end_matches(']').to_string();
        let port = url.port_or_known_default()?;
        let (proxy, certificates, secrets) = {
            let c = self.config.read();
            (c.proxy.clone(), c.certificates.clone(), c.secret_resolver.clone())
        };
        let via = crate::proxy::ProxyResolver::new(&proxy).ok()?.proxy_for(&host, port, true)?;
        let tls = crate::tls::TlsSetup::for_host(&certificates, secrets.as_ref(), verify, &host, Some(port)).ok()?;
        let config = tls.client_config(verify).ok()?;
        let name = rustls::pki_types::ServerName::try_from(host.clone()).ok()?;
        let result = async {
            let stream = via.tunnel(&host, port, &tls, verify).await.map_err(|e| e.message)?;
            let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, stream)
                .await
                .map_err(|e| format!("TLS handshake with {host} failed: {e}"))?;
            let cert = tls.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| c.to_vec());
            cert.ok_or_else(|| format!("{host} sent no certificate"))
        }
        .await;
        result.inspect_err(|e| tracing::debug!("no TLS channel binding for {host}: {e}")).ok()
    }

    async fn read_body(
        &self,
        mut resp: reqwest::Response,
        opts: &SendOptions,
        host: &str,
        timeout_ms: Option<u128>,
    ) -> Result<ResponseBody, HttpError> {
        let (limit, temp_dir) = {
            let c = self.config.read();
            (c.max_body_in_memory, c.temp_dir.clone())
        };
        let total = resp.content_length();
        let mut mem: Vec<u8> = Vec::with_capacity(total.unwrap_or(0).min(limit).min(1 << 20) as usize);
        let mut file: Option<(std::io::BufWriter<std::fs::File>, tempfile::TempPath)> = None;
        let mut received = 0u64;
        loop {
            let chunk = tokio::select! {
                c = resp.chunk() => c.map_err(|e| classify(&e, host, timeout_ms))?,
                _ = opts.cancel.cancelled() => return Err(HttpError::cancelled()),
            };
            let Some(chunk) = chunk else { break };
            received += chunk.len() as u64;
            if file.is_none() && mem.len() as u64 + chunk.len() as u64 > limit {
                let tmp = match &temp_dir {
                    Some(d) => {
                        std::fs::create_dir_all(d).ok();
                        tempfile::NamedTempFile::with_prefix_in("merpati-body-", d)
                    }
                    None => tempfile::NamedTempFile::with_prefix("merpati-body-"),
                }
                .map_err(|e| HttpError::new(HttpErrorKind::File, format!("Could not create temporary file: {e}")))?;
                let (f, path) = tmp.into_parts();
                let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
                w.write_all(&mem).map_err(file_err)?;
                mem = Vec::new();
                file = Some((w, path));
            }
            match &mut file {
                Some((w, _)) => w.write_all(&chunk).map_err(file_err)?,
                None => mem.extend_from_slice(&chunk),
            }
            if let Some(p) = &opts.progress {
                p(Progress { received, total });
            }
        }
        Ok(match file {
            Some((mut w, path)) => {
                w.flush().map_err(file_err)?;
                ResponseBody::File { path: Arc::new(path), len: received }
            }
            None => ResponseBody::Memory(Bytes::from(mem)),
        })
    }
}

/// Whether a 401 answer carries an NTLM or Negotiate challenge with a token
/// (the message the client answers bound to the TLS channel).
fn wants_tls_binding(headers: &[(String, String)]) -> bool {
    let values = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("www-authenticate")).map(|(_, v)| v.as_str());
    crate::challenge::parse_challenges(values)
        .iter()
        .any(|c| matches!(c.scheme.as_str(), "ntlm" | "negotiate") && c.token68.is_some())
}

fn file_err(e: std::io::Error) -> HttpError {
    HttpError::new(HttpErrorKind::File, format!("Could not write response to temporary file: {e}"))
}

fn header_map(headers: &[(String, String)]) -> Result<HeaderMap, HttpError> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.trim().as_bytes())
            .map_err(|_| HttpError::new(HttpErrorKind::InvalidHeader, format!("Invalid header name \"{k}\"")))?;
        let value = HeaderValue::from_bytes(v.trim().as_bytes())
            .map_err(|_| HttpError::new(HttpErrorKind::InvalidHeader, format!("Invalid value for header \"{k}\"")))?;
        map.append(name, value);
    }
    Ok(map)
}

/// Recognizes local socket URLs (FR-X7) and returns the socket path and the
/// URL to request over it:
/// - Unix domain sockets: `http://unix:/var/run/docker.sock:/containers/json`
///   (request/Postman style) and `http+unix://%2Fvar%2Frun%2Fdocker.sock/containers/json`.
/// - Windows named pipes: `http://npipe:docker_engine:/info`,
///   `http://npipe:\\.\pipe\docker_engine:/info` (also `//./pipe/...`, or
///   `unix:` with a pipe path) and `http+npipe://%5C%5C.%5Cpipe%5Cdocker_engine/info`
///   (or `http+npipe://docker_engine/info`). The returned path is always
///   `\\.\pipe\<name>`, see [`named_pipe_path`].
pub fn unix_target(url: &str) -> Option<(PathBuf, String)> {
    for (prefix, pipe) in [("http+unix://", false), ("http+npipe://", true)] {
        if let Some(rest) = url.strip_prefix(prefix) {
            let (enc, path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, "/"),
            };
            let socket = percent_decode(enc);
            let socket = if pipe { pipe_name_to_path(&socket) } else { socket };
            return Some((PathBuf::from(socket), format!("http://localhost{path}")));
        }
    }
    for scheme in ["http", "https"] {
        for (kind, pipe) in [("unix:", false), ("npipe:", true)] {
            let prefix = format!("{scheme}://{kind}");
            if let Some(rest) = url.strip_prefix(&prefix) {
                let (socket, path) = match rest.find(":/") {
                    Some(i) => (&rest[..i], &rest[i + 1..]),
                    None => (rest, "/"),
                };
                let socket = if pipe { pipe_name_to_path(socket) } else { socket.to_string() };
                return Some((PathBuf::from(socket), format!("{scheme}://localhost{path}")));
            }
        }
    }
    None
}

/// `docker_engine` / `//./pipe/docker_engine` -> `\\.\pipe\docker_engine`.
fn pipe_name_to_path(name: &str) -> String {
    named_pipe_path(Path::new(name)).unwrap_or_else(|| format!(r"\\.\pipe\{}", name.trim_start_matches(['\\', '/'])))
}

/// The Windows named pipe (`\\.\pipe\name`, also written `//./pipe/name`)
/// a socket path refers to, normalized to backslashes; `None` for Unix
/// socket paths.
pub fn named_pipe_path(socket: &Path) -> Option<String> {
    let s = socket.to_string_lossy();
    let normalized = s.replace('/', r"\");
    let lower = normalized.to_ascii_lowercase();
    let is_pipe = lower.starts_with(r"\\.\pipe\") || lower.starts_with(r"\\?\pipe\");
    (is_pipe && normalized.len() > r"\\.\pipe\".len()).then_some(normalized)
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn build_client(cfg: &EngineConfig, key: &ClientKey, cookies: &Arc<CookieJar>) -> Result<reqwest::Client, HttpError> {
    let mut b = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .tcp_nodelay(true)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tls_danger_accept_invalid_certs(!key.verify_ssl)
        .tls_danger_accept_invalid_hostnames(!key.verify_ssl)
        // Puts the server certificate on responses (`SendOutcome::peer_certificate`).
        // Per connection, so it costs a copy of the certificate per response.
        .tls_info(true);
    if key.cookies {
        b = b.cookie_provider(cookies.clone());
    }
    match key.version {
        HttpVersion::Auto => {}
        HttpVersion::Http1 => b = b.http1_only(),
        HttpVersion::Http2 => b = b.http2_prior_knowledge(),
    }
    b = crate::proxy::apply_to_reqwest(b, &cfg.proxy)?;
    let certs = &cfg.certificates;
    if certs.ca_enabled && !certs.ca_path.trim().is_empty() && key.verify_ssl {
        let pem = std::fs::read(certs.ca_path.trim()).map_err(|e| {
            HttpError::new(
                HttpErrorKind::Certificate,
                format!("Could not read CA certificate file {}: {e}", certs.ca_path),
            )
        })?;
        let roots = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
            HttpError::new(HttpErrorKind::Certificate, format!("Invalid CA certificate file {}: {e}", certs.ca_path))
        })?;
        b = b.tls_certs_merge(roots);
    }
    if let Some(idx) = key.client_cert {
        let identity = crate::certs::load_client_identity(&certs.client_certs[idx], cfg.secret_resolver.as_ref())
            .map_err(|e| HttpError::new(HttpErrorKind::Certificate, e))?;
        let identity = reqwest::Identity::from_pem(&identity.to_pem()).map_err(|e| {
            HttpError::new(
                HttpErrorKind::Certificate,
                format!("Invalid client certificate/key for {}: {e}", certs.client_certs[idx].host),
            )
        })?;
        b = b.identity(identity);
    }
    if let Some(sock) = &key.unix_socket {
        b = local_socket(b, sock)?;
    }
    b.build().map_err(|e| HttpError::new(HttpErrorKind::Other, format!("Could not create HTTP client: {e}")))
}

/// Routes every connection of the client through a Unix socket or (on
/// Windows) a named pipe.
fn local_socket(b: reqwest::ClientBuilder, socket: &Path) -> Result<reqwest::ClientBuilder, HttpError> {
    match named_pipe_path(socket) {
        #[cfg(windows)]
        Some(pipe) => Ok(b.windows_named_pipe(pipe)),
        #[cfg(not(windows))]
        Some(pipe) => {
            let _ = b;
            Err(HttpError::new(
                HttpErrorKind::InvalidUrl,
                format!("Named pipes ({pipe}) are only available on Windows; use a Unix socket path here"),
            ))
        }
        #[cfg(unix)]
        None => Ok(b.unix_socket(socket.to_path_buf())),
        #[cfg(not(unix))]
        None => {
            let _ = b;
            Err(HttpError::new(
                HttpErrorKind::InvalidUrl,
                format!(
                    "Unix sockets ({}) are not supported on this platform; use a named pipe, e.g. http://npipe:docker_engine:/info",
                    socket.display()
                ),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_urls() {
        assert_eq!(
            unix_target("http://unix:/var/run/docker.sock:/containers/json?all=1"),
            Some((PathBuf::from("/var/run/docker.sock"), "http://localhost/containers/json?all=1".into()))
        );
        assert_eq!(
            unix_target("http+unix://%2Ftmp%2Fapi.sock/v1/ping"),
            Some((PathBuf::from("/tmp/api.sock"), "http://localhost/v1/ping".into()))
        );
        assert_eq!(unix_target("http://localhost/x"), None);
    }

    #[test]
    fn named_pipe_urls() {
        let pipe = PathBuf::from(r"\\.\pipe\docker_engine");
        let want = Some((pipe.clone(), "http://localhost/v1.41/info".to_string()));
        assert_eq!(unix_target("http://npipe:docker_engine:/v1.41/info"), want);
        assert_eq!(unix_target(r"http://npipe:\\.\pipe\docker_engine:/v1.41/info"), want);
        assert_eq!(unix_target("http://npipe://./pipe/docker_engine:/v1.41/info"), want);
        assert_eq!(unix_target("http+npipe://%5C%5C.%5Cpipe%5Cdocker_engine/v1.41/info"), want);
        assert_eq!(unix_target("http+npipe://docker_engine/v1.41/info"), want);
        let (sock, _) = unix_target(r"http://unix:\\.\pipe\docker_engine:/x").unwrap();
        assert_eq!(named_pipe_path(&sock).as_deref(), Some(r"\\.\pipe\docker_engine"));
        assert_eq!(named_pipe_path(Path::new("//./pipe/a")).as_deref(), Some(r"\\.\pipe\a"));
        assert_eq!(named_pipe_path(Path::new("/var/run/docker.sock")), None);
        assert_eq!(named_pipe_path(Path::new(r"\\.\pipe\")), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn named_pipes_need_windows() {
        let cfg = EngineConfig::default();
        let key = ClientKey {
            verify_ssl: true,
            version: HttpVersion::Auto,
            client_cert: None,
            cookies: false,
            unix_socket: Some(PathBuf::from(r"\\.\pipe\x")),
        };
        let err = build_client(&cfg, &key, &Arc::new(CookieJar::new())).unwrap_err();
        assert!(err.message.contains("only available on Windows"), "{err}");
    }
}
