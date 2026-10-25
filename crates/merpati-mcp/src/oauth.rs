//! MCP authorization for the HTTP transports (spec 2025-06-18, accepting the
//! 2025-03-26 behavior): on 401 the client reads `WWW-Authenticate`, fetches
//! the protected resource metadata (RFC 9728), discovers the authorization
//! server (RFC 8414, OpenID Connect discovery), registers itself (RFC 7591)
//! unless a client id is configured, runs the authorization code flow with
//! PKCE S256 through a loopback redirect on 127.0.0.1, sends the RFC 8707
//! `resource` parameter, stores and refreshes the tokens, and retries the
//! original request.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use parking_lot::Mutex;
use rand::Rng;
use reqwest::Url;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::client::{Busy, EventFn, LATEST_PROTOCOL_VERSION};
use crate::error::McpError;
use crate::http::{MAX_SMALL_BODY, body_prefix};
use crate::types::McpEvent;

/// How long [`OAuthOptions::login_timeout`] waits for the browser by default.
pub const DEFAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Tokens expiring within this many seconds are refreshed before use.
const EXPIRY_SKEW_SECS: i64 = 60;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const B64_URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn random_string(n: usize) -> String {
    rand::rng().sample_iter(rand::distr::Alphanumeric).take(n).map(char::from).collect()
}

/// RFC 7636 S256 code challenge.
pub(crate) fn pkce_s256(verifier: &str) -> String {
    B64_URL.encode(Sha256::digest(verifier.as_bytes()))
}

// ---------------------------------------------------------------------------
// Public configuration and storage
// ---------------------------------------------------------------------------

/// OAuth client settings for one server. All optional: with none set, the
/// client discovers everything and registers itself dynamically.
#[derive(Clone, Default, PartialEq)]
pub struct OAuthSettings {
    /// Pre-registered client id; empty = dynamic client registration.
    pub client_id: String,
    /// Secret of a confidential pre-registered client.
    pub client_secret: String,
    /// Space separated scopes; empty = the scope the server asks for in
    /// `WWW-Authenticate`, else the metadata's `scopes_supported`.
    pub scope: String,
    /// Loopback redirect URI registered for `client_id`
    /// (`http://127.0.0.1:<port>/<path>` or `http://localhost:...`);
    /// empty = `http://127.0.0.1:<random port>/callback`.
    pub redirect_uri: String,
    /// Overrides the discovered authorization endpoint.
    pub authorization_endpoint: String,
    /// Overrides the discovered token endpoint.
    pub token_endpoint: String,
    /// `client_secret_basic`, `client_secret_post` or `none`; empty = automatic.
    pub token_endpoint_auth_method: String,
}

impl fmt::Debug for OAuthSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthSettings")
            .field("client_id", &self.client_id)
            .field("client_secret", &if self.client_secret.is_empty() { "" } else { "<redacted>" })
            .field("scope", &self.scope)
            .field("redirect_uri", &self.redirect_uri)
            .field("authorization_endpoint", &self.authorization_endpoint)
            .field("token_endpoint", &self.token_endpoint)
            .field("token_endpoint_auth_method", &self.token_endpoint_auth_method)
            .finish()
    }
}

/// Tokens for one MCP server, with what is needed to refresh them.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Issuer of the authorization server that granted them.
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub token_endpoint: String,
    #[serde(default)]
    pub client_id: String,
    /// The RFC 8707 resource they were issued for.
    #[serde(default)]
    pub resource: String,
    /// Unix seconds.
    #[serde(default)]
    pub obtained_at: i64,
}

impl OAuthTokens {
    /// True if the access token expires within `skew_secs`.
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        self.expires_at.is_some_and(|exp| now_secs() + skew_secs >= exp)
    }
}

impl fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<redacted>"))
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("resource", &self.resource)
            .finish()
    }
}

/// A client registered dynamically (RFC 7591) with an authorization server.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RegisteredClient {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Unix seconds; `0` or absent = never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<i64>,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub token_endpoint_auth_method: String,
    #[serde(default)]
    pub registered_at: i64,
}

impl RegisteredClient {
    fn secret_expired(&self) -> bool {
        self.client_secret_expires_at.is_some_and(|t| t > 0 && t <= now_secs() + 60)
    }
}

impl fmt::Debug for RegisteredClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "<redacted>"))
            .field("client_secret_expires_at", &self.client_secret_expires_at)
            .field("redirect_uris", &self.redirect_uris)
            .field("token_endpoint_auth_method", &self.token_endpoint_auth_method)
            .finish()
    }
}

/// Where tokens and registered clients are kept (the app uses the system
/// keychain). Tokens are keyed by [`oauth_server_key`] of the MCP server
/// URL, clients by the authorization server's issuer URL. `None` deletes.
pub trait OAuthStorage: Send + Sync {
    fn load_tokens(&self, server: &str) -> Option<OAuthTokens>;
    fn save_tokens(&self, server: &str, tokens: Option<&OAuthTokens>);
    fn load_client(&self, issuer: &str) -> Option<RegisteredClient>;
    fn save_client(&self, issuer: &str, client: Option<&RegisteredClient>);
}

/// In-memory [`OAuthStorage`] (the default; forgets everything on exit).
#[derive(Default)]
pub struct MemoryOAuthStorage {
    tokens: Mutex<HashMap<String, OAuthTokens>>,
    clients: Mutex<HashMap<String, RegisteredClient>>,
}

impl fmt::Debug for MemoryOAuthStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryOAuthStorage")
            .field("tokens", &self.tokens.lock().keys().collect::<Vec<_>>())
            .field("clients", &self.clients.lock().keys().collect::<Vec<_>>())
            .finish()
    }
}

impl OAuthStorage for MemoryOAuthStorage {
    fn load_tokens(&self, server: &str) -> Option<OAuthTokens> {
        self.tokens.lock().get(server).cloned()
    }
    fn save_tokens(&self, server: &str, tokens: Option<&OAuthTokens>) {
        match tokens {
            Some(t) => self.tokens.lock().insert(server.to_string(), t.clone()),
            None => self.tokens.lock().remove(server),
        };
    }
    fn load_client(&self, issuer: &str) -> Option<RegisteredClient> {
        self.clients.lock().get(issuer).cloned()
    }
    fn save_client(&self, issuer: &str, client: Option<&RegisteredClient>) {
        match client {
            Some(c) => self.clients.lock().insert(issuer.to_string(), c.clone()),
            None => self.clients.lock().remove(issuer),
        };
    }
}

/// What the host provides to run the browser login. Without it a 401 is
/// reported as an error, as before.
#[derive(Clone)]
pub struct OAuthOptions {
    pub storage: Arc<dyn OAuthStorage>,
    /// Opens the authorization URL in the user's browser (it may also show
    /// the URL so the user can open it by hand).
    pub open_browser: Arc<dyn Fn(&str) + Send + Sync>,
    /// Cancels a login in progress (and any later one using these options).
    pub cancel: CancellationToken,
    /// How long to wait for the browser login.
    pub login_timeout: Duration,
    /// `client_name` sent with dynamic client registration.
    pub client_name: String,
}

impl OAuthOptions {
    /// In-memory storage, a fresh cancel token, a 5 minute login timeout.
    pub fn new(open_browser: impl Fn(&str) + Send + Sync + 'static) -> OAuthOptions {
        OAuthOptions {
            storage: Arc::new(MemoryOAuthStorage::default()),
            open_browser: Arc::new(open_browser),
            cancel: CancellationToken::new(),
            login_timeout: DEFAULT_LOGIN_TIMEOUT,
            client_name: "Merpati".into(),
        }
    }

    pub fn with_storage(mut self, storage: Arc<dyn OAuthStorage>) -> OAuthOptions {
        self.storage = storage;
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> OAuthOptions {
        self.cancel = cancel;
        self
    }

    pub fn with_login_timeout(mut self, timeout: Duration) -> OAuthOptions {
        self.login_timeout = timeout;
        self
    }
}

impl fmt::Debug for OAuthOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthOptions")
            .field("login_timeout", &self.login_timeout)
            .field("client_name", &self.client_name)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// URLs and headers
// ---------------------------------------------------------------------------

/// Canonical resource URI (RFC 8707) of an MCP server URL: lowercase scheme
/// and host, no fragment, no lone trailing slash. Also the key under which
/// its tokens are stored.
pub fn oauth_server_key(url: &str) -> Option<String> {
    Url::parse(url.trim()).ok().map(|u| canonical_resource(&u))
}

pub(crate) fn canonical_resource(url: &Url) -> String {
    let mut u = url.clone();
    u.set_fragment(None);
    let s = u.to_string();
    if u.path() == "/" && u.query().is_none() { s.trim_end_matches('/').to_string() } else { s }
}

/// Whether metadata for `resource` may be used for requests to `server`:
/// same origin, and the server path lies under the resource path.
fn resource_allowed(server: &Url, resource: &str) -> bool {
    let Ok(r) = Url::parse(resource) else { return false };
    if r.origin() != server.origin() {
        return false;
    }
    let rp = r.path().trim_end_matches('/');
    let sp = server.path().trim_end_matches('/');
    rp.is_empty() || sp == rp || sp.starts_with(&format!("{rp}/"))
}

/// Parameters of the Bearer challenge in a `WWW-Authenticate` header.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Challenge {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

pub(crate) fn parse_www_authenticate(header: &str) -> Challenge {
    let mut challenges: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let chars: Vec<char> = header.chars().collect();
    let mut i = 0;
    let is_sep = |c: char| c == ',' || c.is_whitespace();
    while i < chars.len() {
        while i < chars.len() && is_sep(chars[i]) {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        while i < chars.len() && !is_sep(chars[i]) && chars[i] != '=' {
            i += 1;
        }
        let token: String = chars[start..i].iter().collect();
        let mut j = i;
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        if j < chars.len() && chars[j] == '=' {
            i = j + 1;
            while i < chars.len() && chars[i] == ' ' {
                i += 1;
            }
            let mut value = String::new();
            if i < chars.len() && chars[i] == '"' {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    value.push(chars[i]);
                    i += 1;
                }
                i += 1;
            } else {
                while i < chars.len() && !is_sep(chars[i]) {
                    value.push(chars[i]);
                    i += 1;
                }
            }
            if challenges.is_empty() {
                challenges.push((String::new(), Vec::new()));
            }
            if let Some(last) = challenges.last_mut() {
                last.1.push((token.to_ascii_lowercase(), value));
            }
        } else if !token.is_empty() {
            challenges.push((token.to_ascii_lowercase(), Vec::new()));
        }
    }
    let params = challenges
        .iter()
        .find(|(scheme, _)| scheme == "bearer")
        .or_else(|| challenges.iter().find(|(_, p)| p.iter().any(|(k, _)| k == "resource_metadata")))
        .map(|(_, p)| p.clone())
        .unwrap_or_default();
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    Challenge {
        resource_metadata: get("resource_metadata"),
        scope: get("scope"),
        error: get("error"),
        error_description: get("error_description"),
    }
}

/// Whether a response status asks for (more) authorization: 401, or 403
/// with `error="insufficient_scope"` (step-up).
pub(crate) fn wants_auth(status: u16, www: Option<&str>) -> bool {
    status == 401
        || (status == 403
            && www.is_some_and(|w| parse_www_authenticate(w).error.as_deref() == Some("insufficient_scope")))
}

// ---------------------------------------------------------------------------
// Loopback redirect listener
// ---------------------------------------------------------------------------

struct RedirectListener {
    /// One listener, or IPv4 and IPv6 for a `localhost` redirect URI.
    listeners: Vec<TcpListener>,
    path: String,
    redirect_uri: String,
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn page(title: &str, text: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>Merpati</title><body style=\"font-family:system-ui;padding:3em\"><h2>{}</h2><p>{}</p>",
        html_escape(title),
        html_escape(text)
    )
}

/// The text of a failed login for the browser page (without the
/// "Authorization failed" prefix the page title already has).
fn page_message(e: &McpError) -> String {
    match e {
        McpError::Auth(m) | McpError::Cancelled(m) => m.clone(),
        other => other.to_string(),
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// A redirect for this login: its query parameters, and where the login
/// reports its outcome for the page the browser is waiting for.
struct Callback {
    params: HashMap<String, String>,
    done: oneshot::Sender<Result<(), String>>,
}

/// How long the callback page waits for the token exchange before it tells
/// the user to look in Merpati.
const OUTCOME_WAIT: Duration = Duration::from_secs(60);

async fn handle_redirect(mut stream: tokio::net::TcpStream, path: String, state: Arc<str>, tx: mpsc::Sender<Callback>) {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk)).await {
            Ok(Ok(n)) => n,
            _ => 0,
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let Some(target) = head.lines().next().and_then(|l| l.split_whitespace().nth(1)) else { return };
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else { return };
    if url.path() != path {
        respond(&mut stream, "404 Not Found", &page("Not found", "This is Merpati's OAuth callback.")).await;
        return;
    }
    let params: HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let is_error = params.contains_key("error");
    // Only the redirect of this login counts; another one (an old tab, a
    // forged link) gets an error page while the login keeps waiting. An
    // error without `state` is let through: some servers leave it out of
    // error redirects, and an error cannot smuggle in a code.
    let ours = match params.get("state") {
        Some(s) => **s == *state,
        None => is_error,
    };
    if !ours && (is_error || params.contains_key("code") || params.contains_key("state")) {
        let text = "This sign-in response does not belong to the login Merpati is waiting for (the state does not \
                    match). Finish signing in in the browser window Merpati opened.";
        respond(&mut stream, "400 Bad Request", &page("Unexpected sign-in response", text)).await;
        return;
    }
    if !is_error && !params.contains_key("code") {
        respond(&mut stream, "400 Bad Request", &page("Missing authorization code", "No code was received.")).await;
        return;
    }
    let (done, outcome) = oneshot::channel();
    if tx.send(Callback { params, done }).await.is_err() {
        let text = "Merpati is no longer waiting for this sign-in.";
        respond(&mut stream, "410 Gone", &page("Sign-in not expected", text)).await;
        return;
    }
    // The page says how the login ended: the code is exchanged first.
    let html = match tokio::time::timeout(OUTCOME_WAIT, outcome).await {
        Ok(Ok(Ok(()))) => page("Authorization complete", "You can close this window and return to Merpati."),
        Ok(Ok(Err(text))) => page("Authorization failed", &text),
        _ => page("Authorization in progress", "Return to Merpati to see whether the sign-in succeeded."),
    };
    respond(&mut stream, "200 OK", &html).await;
}

impl RedirectListener {
    /// Binds `redirect_uri` (a loopback http URI) or, when empty, 127.0.0.1
    /// on `preferred_port` (falling back to a random port).
    async fn bind(redirect_uri: &str, preferred_port: Option<u16>) -> Result<RedirectListener, McpError> {
        if redirect_uri.trim().is_empty() {
            let preferred = match preferred_port {
                Some(p) => TcpListener::bind(("127.0.0.1", p)).await.ok(),
                None => None,
            };
            let listener = match preferred {
                Some(l) => l,
                None => TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .map_err(|e| McpError::Auth(format!("could not listen for the redirect: {e}")))?,
            };
            let port = listener.local_addr().map_err(|e| McpError::Auth(e.to_string()))?.port();
            return Ok(RedirectListener {
                listeners: vec![listener],
                path: "/callback".into(),
                redirect_uri: format!("http://127.0.0.1:{port}/callback"),
            });
        }
        let url = Url::parse(redirect_uri.trim())
            .map_err(|_| McpError::Config(format!("Invalid redirect URI \"{redirect_uri}\"")))?;
        let host = url.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_string();
        if url.scheme() != "http" || !matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
            return Err(McpError::Config(format!(
                "The redirect URI must be http://127.0.0.1:<port>/... or http://localhost:<port>/... so Merpati can receive it; got {redirect_uri}"
            )));
        }
        let port = url.port_or_known_default().unwrap_or(80);
        let bind_host = if host == "localhost" { "127.0.0.1" } else { host.as_str() };
        let listener = TcpListener::bind((bind_host, port))
            .await
            .map_err(|e| McpError::Auth(format!("could not listen on {bind_host}:{port} for the redirect: {e}")))?;
        let mut listeners = vec![listener];
        if host == "localhost"
            && let Ok(v6) = TcpListener::bind(("::1", port)).await
        {
            // Browsers may resolve localhost to ::1 first.
            listeners.push(v6);
        }
        let path = if url.path().is_empty() { "/".to_string() } else { url.path().to_string() };
        Ok(RedirectListener { listeners, path, redirect_uri: redirect_uri.trim().to_string() })
    }

    /// Waits for the redirect of the login with `state`, carrying `code` or
    /// `error`; other requests are answered with an error page. Each
    /// connection is served on its own task (browsers open speculative
    /// connections).
    async fn wait(self, state: &str) -> Result<Callback, McpError> {
        let (tx, mut rx) = mpsc::channel(4);
        let (err_tx, mut err_rx) = mpsc::channel::<std::io::Error>(1);
        let state: Arc<str> = Arc::from(state);
        let mut accept_loops = tokio::task::JoinSet::new();
        for listener in self.listeners {
            let (tx, err_tx, path, state) = (tx.clone(), err_tx.clone(), self.path.clone(), state.clone());
            accept_loops.spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            tokio::spawn(handle_redirect(stream, path.clone(), state.clone(), tx.clone()));
                        }
                        Err(e) => {
                            let _ = err_tx.send(e).await;
                            return;
                        }
                    }
                }
            });
        }
        drop(tx);
        // Dropping the JoinSet (on return) stops the accept loops.
        tokio::select! {
            Some(callback) = rx.recv() => Ok(callback),
            Some(e) = err_rx.recv() => Err(McpError::Auth(format!("redirect listener failed: {e}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// The flow
// ---------------------------------------------------------------------------

/// Authorization server endpoints for one server.
#[derive(Clone, Debug)]
struct Discovery {
    /// No metadata was found: 2025-03-26 default endpoints.
    defaults: bool,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    resource: String,
    scopes_supported: Vec<String>,
    token_auth_methods: Vec<String>,
}

/// The client identity used at the token endpoint.
#[derive(Clone)]
struct ClientCreds {
    client_id: String,
    client_secret: Option<String>,
    auth_method: String,
}

struct TokenError {
    error: Option<String>,
    message: String,
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty())
}

/// The OAuth state of one connection, shared by its transports.
pub(crate) struct AuthManager {
    http: reqwest::Client,
    server: Url,
    key: String,
    settings: OAuthSettings,
    opts: OAuthOptions,
    on_event: EventFn,
    busy: Arc<Busy>,
    tokens: Mutex<Option<OAuthTokens>>,
    flow: tokio::sync::Mutex<()>,
}

impl AuthManager {
    pub fn new(
        http: reqwest::Client,
        server: &Url,
        settings: OAuthSettings,
        opts: OAuthOptions,
        on_event: EventFn,
        busy: Arc<Busy>,
    ) -> AuthManager {
        let key = canonical_resource(server);
        let tokens = opts.storage.load_tokens(&key).filter(|t| !t.access_token.is_empty());
        AuthManager {
            http,
            server: server.clone(),
            key,
            settings,
            opts,
            on_event,
            busy,
            tokens: Mutex::new(tokens),
            flow: tokio::sync::Mutex::new(()),
        }
    }

    fn log(&self, message: impl Into<String>) {
        (self.on_event)(McpEvent::Log { level: "auth".into(), message: message.into() });
    }

    pub fn current_token(&self) -> Option<String> {
        self.tokens.lock().as_ref().map(|t| t.access_token.clone())
    }

    fn store(&self, tokens: Option<OAuthTokens>) {
        self.opts.storage.save_tokens(&self.key, tokens.as_ref());
        *self.tokens.lock() = tokens;
    }

    /// The access token to send, refreshed first when it is about to expire.
    pub async fn bearer(&self) -> Option<String> {
        let expired = {
            let t = self.tokens.lock();
            let t = t.as_ref()?;
            t.is_expired(EXPIRY_SKEW_SECS) && t.refresh_token.is_some()
        };
        if !expired {
            return self.current_token();
        }
        let _flow = self.flow.lock().await;
        let tokens = self.tokens.lock().clone()?;
        if tokens.is_expired(EXPIRY_SKEW_SECS)
            && tokens.refresh_token.is_some()
            && let Err(e) = self.refresh(&tokens).await
        {
            self.log(format!("Refreshing the access token failed: {e}"));
        }
        self.current_token()
    }

    /// Handles a 401 (or a 403 `insufficient_scope`) to a request sent with
    /// `used`. `Ok` means the request should be retried. The first attempt
    /// may use the refresh token; later ones sign in through the browser.
    pub async fn on_unauthorized(
        &self,
        status: u16,
        www: Option<&str>,
        used: Option<&str>,
        allow_refresh: bool,
    ) -> Result<(), McpError> {
        let challenge = parse_www_authenticate(www.unwrap_or(""));
        let _flow = self.flow.lock().await;
        let current = self.tokens.lock().clone();
        if status == 401
            && let Some(t) = &current
            && Some(t.access_token.as_str()) != used
        {
            // Another request already got a new token.
            return Ok(());
        }
        if status == 401
            && allow_refresh
            && let Some(t) = current.as_ref().filter(|t| t.refresh_token.is_some())
        {
            match self.refresh(t).await {
                Ok(()) => return Ok(()),
                Err(e) => self.log(format!("Refreshing the access token failed ({e}); signing in again")),
            }
        }
        let mut scope = challenge.scope.clone();
        if status == 403 {
            let mut all: Vec<String> = current
                .as_ref()
                .and_then(|t| t.scope.clone())
                .unwrap_or_default()
                .split_whitespace()
                .map(String::from)
                .collect();
            for s in challenge.scope.as_deref().unwrap_or("").split_whitespace() {
                if !all.iter().any(|x| x == s) {
                    all.push(s.to_string());
                }
            }
            scope = Some(all.join(" ")).filter(|s| !s.is_empty());
            self.log(format!(
                "The server requires more permissions ({}); signing in again",
                scope.as_deref().unwrap_or("")
            ));
        }
        self.authorize(&challenge, scope).await
    }

    async fn get_json(&self, url: &str) -> Option<Value> {
        let resp = self
            .http
            .get(url)
            .header(ACCEPT, "application/json")
            .header("mcp-protocol-version", LATEST_PROTOCOL_VERSION)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        serde_json::from_str::<Value>(&body_prefix(resp, MAX_SMALL_BODY).await).ok().filter(Value::is_object)
    }

    /// RFC 9728 protected resource metadata: the URL from the challenge,
    /// then the path-aware and root well-known URIs.
    async fn protected_resource_metadata(&self, from_header: Option<&str>) -> Option<Value> {
        let origin = self.server.origin().ascii_serialization();
        let mut candidates = Vec::new();
        if let Some(u) = from_header.and_then(|h| self.server.join(h).ok()) {
            candidates.push(u.to_string());
        }
        let path = self.server.path().trim_end_matches('/');
        if !path.is_empty() {
            candidates.push(format!("{origin}/.well-known/oauth-protected-resource{path}"));
        }
        candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
        candidates.dedup();
        for url in candidates {
            if let Some(m) = self.get_json(&url).await
                && m.get("authorization_servers").and_then(Value::as_array).is_some_and(|a| !a.is_empty())
            {
                return Some(m);
            }
        }
        None
    }

    /// RFC 8414 / OpenID Connect discovery for an issuer.
    async fn authorization_server_metadata(&self, issuer: &str) -> Option<Value> {
        let url = Url::parse(issuer).ok()?;
        let origin = url.origin().ascii_serialization();
        let path = url.path().trim_end_matches('/');
        let candidates = if path.is_empty() {
            vec![
                format!("{origin}/.well-known/oauth-authorization-server"),
                format!("{origin}/.well-known/openid-configuration"),
            ]
        } else {
            vec![
                format!("{origin}/.well-known/oauth-authorization-server{path}"),
                format!("{origin}/.well-known/openid-configuration{path}"),
                format!("{origin}{path}/.well-known/openid-configuration"),
            ]
        };
        for c in candidates {
            if let Some(m) = self.get_json(&c).await
                && m.get("authorization_endpoint").and_then(Value::as_str).is_some()
                && m.get("token_endpoint").and_then(Value::as_str).is_some()
            {
                return Some(m);
            }
        }
        None
    }

    async fn discover(&self, challenge: &Challenge) -> Result<Discovery, McpError> {
        let prm = self.protected_resource_metadata(challenge.resource_metadata.as_deref()).await;
        let (issuer, resource, scopes_supported, legacy) = match &prm {
            Some(m) => {
                let resource = match str_of(m, "resource") {
                    Some(r) if resource_allowed(&self.server, &r) => r,
                    Some(r) => {
                        return Err(McpError::Auth(format!(
                            "the protected resource metadata is for {r}, not for {}",
                            self.key
                        )));
                    }
                    None => self.key.clone(),
                };
                let issuer = strings(m, "authorization_servers").into_iter().next().unwrap_or_default();
                (issuer, resource, strings(m, "scopes_supported"), false)
            }
            // 2025-03-26: the authorization server lives at the server's origin.
            None => (self.server.origin().ascii_serialization(), self.key.clone(), Vec::new(), true),
        };
        let s = &self.settings;
        let explicit = !s.authorization_endpoint.is_empty() && !s.token_endpoint.is_empty();
        let meta =
            if explicit && !s.client_id.is_empty() { None } else { self.authorization_server_metadata(&issuer).await };
        let mut d = match meta {
            Some(m) => {
                if let Some(got) = str_of(&m, "issuer")
                    && got.trim_end_matches('/') != issuer.trim_end_matches('/')
                {
                    self.log(format!("The authorization server metadata names issuer {got}, expected {issuer}"));
                }
                let methods = strings(&m, "code_challenge_methods_supported");
                if !methods.is_empty() && !methods.iter().any(|x| x == "S256") {
                    return Err(McpError::Auth(format!("{issuer} does not support PKCE with S256")));
                }
                Discovery {
                    defaults: false,
                    issuer: issuer.clone(),
                    authorization_endpoint: str_of(&m, "authorization_endpoint").unwrap_or_default(),
                    token_endpoint: str_of(&m, "token_endpoint").unwrap_or_default(),
                    registration_endpoint: str_of(&m, "registration_endpoint"),
                    resource,
                    scopes_supported,
                    token_auth_methods: strings(&m, "token_endpoint_auth_methods_supported"),
                }
            }
            None if legacy || explicit => {
                let base = issuer.trim_end_matches('/');
                Discovery {
                    defaults: !explicit,
                    issuer: issuer.clone(),
                    authorization_endpoint: format!("{base}/authorize"),
                    token_endpoint: format!("{base}/token"),
                    registration_endpoint: Some(format!("{base}/register")),
                    resource,
                    scopes_supported,
                    token_auth_methods: Vec::new(),
                }
            }
            None => {
                return Err(McpError::Auth(format!("no authorization server metadata found for {issuer}")));
            }
        };
        if !s.authorization_endpoint.is_empty() {
            d.authorization_endpoint = s.authorization_endpoint.clone();
        }
        if !s.token_endpoint.is_empty() {
            d.token_endpoint = s.token_endpoint.clone();
        }
        let endpoints = [Some(&d.authorization_endpoint), Some(&d.token_endpoint), d.registration_endpoint.as_ref()];
        for e in endpoints.into_iter().flatten() {
            self.check_endpoint(e)?;
        }
        Ok(d)
    }

    /// Endpoints must be http(s) URLs: the authorization URL is handed to the
    /// system browser. Plain http is only expected on loopback.
    fn check_endpoint(&self, endpoint: &str) -> Result<(), McpError> {
        let url = Url::parse(endpoint).map_err(|_| McpError::Auth(format!("invalid endpoint URL {endpoint}")))?;
        match url.scheme() {
            "https" => Ok(()),
            "http" => {
                let host = url.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
                if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
                    self.log(format!("Warning: {endpoint} is not served over HTTPS"));
                }
                Ok(())
            }
            other => Err(McpError::Auth(format!("refusing the endpoint {endpoint} (scheme {other}:)"))),
        }
    }

    async fn register(
        &self,
        d: &Discovery,
        endpoint: &str,
        redirect_uri: &str,
        scope: &str,
    ) -> Result<RegisteredClient, McpError> {
        let mut body = json!({
            "client_name": self.opts.client_name,
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "software_id": "merpati",
            "software_version": env!("CARGO_PKG_VERSION"),
        });
        if !scope.is_empty() {
            body["scope"] = json!(scope);
        }
        let resp = self
            .http
            .post(endpoint)
            .header(ACCEPT, "application/json")
            .json(&body)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.net_err(endpoint, &e))?;
        let status = resp.status();
        let v: Value = serde_json::from_str(&body_prefix(resp, MAX_SMALL_BODY).await).unwrap_or(Value::Null);
        if !status.is_success() {
            let detail = str_of(&v, "error_description").or_else(|| str_of(&v, "error")).unwrap_or_default();
            return Err(McpError::Auth(format!(
                "dynamic client registration at {} failed (HTTP {}){}{}",
                d.issuer,
                status.as_u16(),
                if detail.is_empty() { "" } else { ": " },
                detail
            )));
        }
        let client_id = str_of(&v, "client_id")
            .ok_or_else(|| McpError::Auth("the registration response has no client_id".into()))?;
        let client_secret = str_of(&v, "client_secret");
        let method = str_of(&v, "token_endpoint_auth_method")
            .unwrap_or_else(|| if client_secret.is_some() { "client_secret_basic".into() } else { "none".into() });
        let mut redirect_uris = strings(&v, "redirect_uris");
        if redirect_uris.is_empty() {
            redirect_uris.push(redirect_uri.to_string());
        }
        Ok(RegisteredClient {
            client_id,
            client_secret,
            client_secret_expires_at: v.get("client_secret_expires_at").and_then(Value::as_i64),
            redirect_uris,
            token_endpoint_auth_method: method,
            registered_at: now_secs(),
        })
    }

    fn settings_creds(&self) -> ClientCreds {
        let s = &self.settings;
        ClientCreds {
            client_id: s.client_id.clone(),
            client_secret: Some(s.client_secret.clone()).filter(|x| !x.is_empty()),
            auth_method: s.token_endpoint_auth_method.clone(),
        }
    }

    fn registered_creds(c: &RegisteredClient) -> ClientCreds {
        ClientCreds {
            client_id: c.client_id.clone(),
            client_secret: c.client_secret.clone(),
            auth_method: c.token_endpoint_auth_method.clone(),
        }
    }

    fn net_err(&self, url: &str, e: &reqwest::Error) -> McpError {
        let host = Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        McpError::Auth(merpati_http::error::classify(e, &host, None).message)
    }

    async fn token_request(
        &self,
        endpoint: &str,
        mut form: Vec<(String, String)>,
        client: &ClientCreds,
        methods: &[String],
    ) -> Result<Value, TokenError> {
        let method = match (client.auth_method.as_str(), &client.client_secret) {
            (_, None) => "none",
            ("client_secret_post", Some(_)) => "client_secret_post",
            ("client_secret_basic", Some(_)) => "client_secret_basic",
            (_, Some(_))
                if !methods.is_empty()
                    && !methods.iter().any(|m| m == "client_secret_basic")
                    && methods.iter().any(|m| m == "client_secret_post") =>
            {
                "client_secret_post"
            }
            (_, Some(_)) => "client_secret_basic",
        };
        let mut rb = self.http.post(endpoint).header(ACCEPT, "application/json").timeout(HTTP_TIMEOUT);
        match (method, &client.client_secret) {
            ("client_secret_basic", Some(secret)) => {
                let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
                let basic = B64.encode(format!("{}:{}", enc(&client.client_id), enc(secret)));
                rb = rb.header(AUTHORIZATION, format!("Basic {basic}"));
            }
            ("client_secret_post", Some(secret)) => {
                form.push(("client_id".into(), client.client_id.clone()));
                form.push(("client_secret".into(), secret.clone()));
            }
            _ => form.push(("client_id".into(), client.client_id.clone())),
        }
        let resp = rb.form(&form).send().await.map_err(|e| TokenError {
            error: None,
            message: match self.net_err(endpoint, &e) {
                McpError::Auth(m) => m,
                other => other.to_string(),
            },
        })?;
        let status = resp.status();
        let ct = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let text = body_prefix(resp, MAX_SMALL_BODY).await;
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            if ct.contains("json") {
                Value::Null
            } else {
                // Some servers still answer form encoded.
                Value::Object(
                    url::form_urlencoded::parse(text.as_bytes())
                        .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                        .collect(),
                )
            }
        });
        if let Some(error) = str_of(&v, "error") {
            let message = match str_of(&v, "error_description") {
                Some(d) => format!("{error}: {d}"),
                None => error.clone(),
            };
            return Err(TokenError { error: Some(error), message });
        }
        if !status.is_success() || str_of(&v, "access_token").is_none() {
            let snippet: String = text.chars().take(200).collect();
            return Err(TokenError {
                error: None,
                message: format!(
                    "the token endpoint answered HTTP {} without an access token: {snippet}",
                    status.as_u16()
                ),
            });
        }
        Ok(v)
    }

    fn tokens_from(
        &self,
        v: &Value,
        d_issuer: &str,
        token_endpoint: &str,
        client_id: &str,
        resource: &str,
    ) -> OAuthTokens {
        let now = now_secs();
        let expires_in = match v.get("expires_in") {
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => s.parse().ok(),
            _ => None,
        };
        OAuthTokens {
            access_token: str_of(v, "access_token").unwrap_or_default(),
            token_type: str_of(v, "token_type").unwrap_or_else(|| "Bearer".into()),
            refresh_token: str_of(v, "refresh_token"),
            expires_at: expires_in.map(|e| now + e as i64),
            scope: str_of(v, "scope"),
            issuer: d_issuer.to_string(),
            token_endpoint: token_endpoint.to_string(),
            client_id: client_id.to_string(),
            resource: resource.to_string(),
            obtained_at: now,
        }
    }

    /// Uses the refresh token. A rejected refresh token is forgotten.
    async fn refresh(&self, old: &OAuthTokens) -> Result<(), McpError> {
        let Some(refresh_token) = old.refresh_token.clone() else {
            return Err(McpError::Auth("no refresh token".into()));
        };
        if old.token_endpoint.is_empty() {
            return Err(McpError::Auth("the stored token has no token endpoint".into()));
        }
        let creds = if !self.settings.client_id.is_empty() && self.settings.client_id == old.client_id {
            self.settings_creds()
        } else {
            match self.opts.storage.load_client(&old.issuer).filter(|c| c.client_id == old.client_id) {
                Some(c) => Self::registered_creds(&c),
                None => {
                    ClientCreds { client_id: old.client_id.clone(), client_secret: None, auth_method: String::new() }
                }
            }
        };
        let mut form = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.clone()),
        ];
        if !old.resource.is_empty() {
            form.push(("resource".into(), old.resource.clone()));
        }
        match self.token_request(&old.token_endpoint, form, &creds, &[]).await {
            Ok(v) => {
                let mut t = self.tokens_from(&v, &old.issuer, &old.token_endpoint, &old.client_id, &old.resource);
                if t.refresh_token.is_none() {
                    t.refresh_token = Some(refresh_token);
                }
                if t.scope.is_none() {
                    t.scope = old.scope.clone();
                }
                self.store(Some(t));
                self.log("Access token refreshed");
                Ok(())
            }
            Err(e) => {
                if matches!(
                    e.error.as_deref(),
                    Some("invalid_grant" | "invalid_client" | "unauthorized_client" | "invalid_token")
                ) {
                    // The refresh token (or the client) is dead.
                    self.store(None);
                }
                Err(McpError::Auth(e.message))
            }
        }
    }

    /// The full interactive flow. Stores and returns nothing but errors; the
    /// caller retries its request with the new token.
    async fn authorize(&self, challenge: &Challenge, scope_hint: Option<String>) -> Result<(), McpError> {
        if self.opts.cancel.is_cancelled() {
            return Err(McpError::Cancelled("Authorization cancelled".into()));
        }
        let _busy = self.busy.enter();
        self.log(format!("{} requires authorization; discovering its authorization server", self.key));
        let d = self.discover(challenge).await?;
        let scope = Some(self.settings.scope.trim().to_string())
            .filter(|s| !s.is_empty())
            .or(scope_hint)
            .or_else(|| Some(d.scopes_supported.join(" ")).filter(|s| !s.is_empty()))
            .unwrap_or_default();

        // A dynamically registered client is bound to its redirect URI: reuse
        // its port when possible.
        let stored = if self.settings.client_id.is_empty() {
            self.opts.storage.load_client(&d.issuer).filter(|c| !c.client_id.is_empty() && !c.secret_expired())
        } else {
            None
        };
        let preferred_port = stored
            .as_ref()
            .and_then(|c| c.redirect_uris.iter().find_map(|u| Url::parse(u).ok().and_then(|u| u.port())));
        let listener = RedirectListener::bind(&self.settings.redirect_uri, preferred_port).await?;
        let redirect_uri = listener.redirect_uri.clone();
        let creds = if !self.settings.client_id.is_empty() {
            self.settings_creds()
        } else {
            match stored.filter(|c| c.redirect_uris.contains(&redirect_uri)) {
                Some(c) => Self::registered_creds(&c),
                None => {
                    let Some(endpoint) = d.registration_endpoint.clone() else {
                        return Err(McpError::Auth(format!(
                            "{} does not support dynamic client registration; register a client there and enter its client ID and redirect URI (e.g. http://127.0.0.1:33418/callback) as OAuth 2.0 settings",
                            d.issuer
                        )));
                    };
                    let client = match self.register(&d, &endpoint, &redirect_uri, &scope).await {
                        Ok(c) => c,
                        Err(McpError::Auth(m)) if d.defaults => {
                            return Err(McpError::Auth(format!(
                                "{m}. The server publishes no OAuth metadata (RFC 9728 / RFC 8414); it may expect a static credential: set a Bearer token or API key in the Auth tab, or OAuth 2.0 client settings"
                            )));
                        }
                        Err(e) => return Err(e),
                    };
                    self.log(format!("Registered as client {} with {}", client.client_id, d.issuer));
                    self.opts.storage.save_client(&d.issuer, Some(&client));
                    Self::registered_creds(&client)
                }
            }
        };

        let verifier = random_string(64);
        let state = random_string(32);
        let mut url = Url::parse(&d.authorization_endpoint)
            .map_err(|_| McpError::Auth(format!("invalid authorization endpoint {}", d.authorization_endpoint)))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code");
            q.append_pair("client_id", &creds.client_id);
            q.append_pair("redirect_uri", &redirect_uri);
            q.append_pair("code_challenge", &pkce_s256(&verifier));
            q.append_pair("code_challenge_method", "S256");
            q.append_pair("state", &state);
            q.append_pair("resource", &d.resource);
            if !scope.is_empty() {
                q.append_pair("scope", &scope);
            }
        }
        self.log(format!(
            "Opening the browser to sign in at {}",
            url.host_str().map(str::to_string).unwrap_or_else(|| d.issuer.clone())
        ));
        (self.opts.open_browser)(url.as_str());
        let callback = tokio::select! {
            r = listener.wait(&state) => r?,
            _ = self.opts.cancel.cancelled() => return Err(McpError::Cancelled("Authorization cancelled".into())),
            _ = tokio::time::sleep(self.opts.login_timeout) => {
                return Err(McpError::Auth(format!(
                    "no answer from the browser login within {} s",
                    self.opts.login_timeout.as_secs()
                )));
            }
        };
        let result = self.finish_login(&callback.params, &state, redirect_uri, verifier, &d, &creds, scope).await;
        let _ = callback.done.send(result.as_ref().map(|_| ()).map_err(page_message));
        result
    }

    /// Checks the redirect and exchanges its code for tokens.
    #[allow(clippy::too_many_arguments)]
    async fn finish_login(
        &self,
        params: &HashMap<String, String>,
        state: &str,
        redirect_uri: String,
        verifier: String,
        d: &Discovery,
        creds: &ClientCreds,
        scope: String,
    ) -> Result<(), McpError> {
        if let Some(err) = params.get("error") {
            let text = match params.get("error_description") {
                Some(desc) => format!("{err}: {desc}"),
                None => err.clone(),
            };
            if err == "access_denied" {
                return Err(McpError::Cancelled(format!("Authorization denied ({text})")));
            }
            return Err(McpError::Auth(text));
        }
        if params.get("state").map(String::as_str) != Some(state) {
            return Err(McpError::Auth("the state returned by the authorization server does not match".into()));
        }
        let code = params.get("code").cloned().unwrap_or_default();
        let form = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code),
            ("redirect_uri".to_string(), redirect_uri),
            ("code_verifier".to_string(), verifier),
            ("resource".to_string(), d.resource.clone()),
        ];
        let v = match self.token_request(&d.token_endpoint, form, creds, &d.token_auth_methods).await {
            Ok(v) => v,
            Err(e) => {
                if e.error.as_deref() == Some("invalid_client") && self.settings.client_id.is_empty() {
                    // Forget the registration so the next attempt registers again.
                    self.opts.storage.save_client(&d.issuer, None);
                }
                return Err(McpError::Auth(format!("token request failed: {}", e.message)));
            }
        };
        let mut tokens = self.tokens_from(&v, &d.issuer, &d.token_endpoint, &creds.client_id, &d.resource);
        if tokens.scope.is_none() && !scope.is_empty() {
            tokens.scope = Some(scope);
        }
        self.store(Some(tokens));
        self.log(format!("Authorized by {}", d.issuer));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_www_authenticate() {
        let c = parse_www_authenticate(
            r#"Bearer realm="mcp", error="invalid_token", resource_metadata="https://x.test/.well-known/oauth-protected-resource/mcp", scope="a b""#,
        );
        assert_eq!(c.resource_metadata.as_deref(), Some("https://x.test/.well-known/oauth-protected-resource/mcp"));
        assert_eq!(c.scope.as_deref(), Some("a b"));
        assert_eq!(c.error.as_deref(), Some("invalid_token"));
        let c = parse_www_authenticate(r#"Basic realm="x", Bearer error="insufficient_scope", scope=files:read"#);
        assert_eq!((c.error.as_deref(), c.scope.as_deref()), (Some("insufficient_scope"), Some("files:read")));
        assert!(wants_auth(403, Some(r#"Bearer error="insufficient_scope""#)));
        assert!(!wants_auth(403, Some("Bearer")));
        assert_eq!(parse_www_authenticate("Bearer"), Challenge::default());
        let c = parse_www_authenticate(r#"Bearer resource_metadata="a\"b""#);
        assert_eq!(c.resource_metadata.as_deref(), Some("a\"b"));
    }

    #[test]
    fn canonical_uris() {
        assert_eq!(oauth_server_key("HTTPS://MCP.Example.com/mcp#x").as_deref(), Some("https://mcp.example.com/mcp"));
        assert_eq!(oauth_server_key("https://mcp.example.com/").as_deref(), Some("https://mcp.example.com"));
        assert_eq!(oauth_server_key("http://h:8080/a/").as_deref(), Some("http://h:8080/a/"));
        let server = Url::parse("https://h.test/mcp/v1").unwrap();
        assert!(resource_allowed(&server, "https://h.test/mcp"));
        assert!(resource_allowed(&server, "https://h.test"));
        assert!(resource_allowed(&server, "https://h.test/mcp/v1/"));
        assert!(!resource_allowed(&server, "https://h.test/mc"));
        assert!(!resource_allowed(&server, "https://other.test/mcp"));
    }

    #[test]
    fn pkce_matches_rfc7636_example() {
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(random_string(64).len(), 64);
    }

    #[test]
    fn debug_redacts_secrets() {
        let t = OAuthTokens {
            access_token: "at-secret".into(),
            refresh_token: Some("rt-secret".into()),
            ..Default::default()
        };
        let c =
            RegisteredClient { client_id: "c".into(), client_secret: Some("cs-secret".into()), ..Default::default() };
        let s = OAuthSettings { client_secret: "set-secret".into(), ..Default::default() };
        let all = format!("{t:?} {c:?} {s:?}");
        assert!(!all.contains("at-secret") && !all.contains("rt-secret"), "{all}");
        assert!(!all.contains("cs-secret") && !all.contains("set-secret"), "{all}");
    }
}
