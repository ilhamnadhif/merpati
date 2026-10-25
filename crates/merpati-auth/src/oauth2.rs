//! OAuth 2.0: token acquisition (all common grant types), refresh, and
//! attaching the access token to requests.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use merpati_core::Auth;
use merpati_core::PreparedRequest;
use merpati_core::tokens::OAuth2Token;

use crate::error::AuthError;
use crate::util::{B64, B64_URL, now_secs, param, random_alnum};
use crate::{AuthApplied, AuthContext, TokenHttp};

/// How long to wait for the user to finish logging in in the browser.
pub const REDIRECT_TIMEOUT: Duration = Duration::from_secs(300);
/// Tokens expiring within this many seconds are treated as expired.
const EXPIRY_SKEW_SECS: i64 = 30;
/// How long a token endpoint may take to answer.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantType {
    AuthorizationCode,
    AuthorizationCodeWithPkce,
    Implicit,
    Password,
    ClientCredentials,
}

impl GrantType {
    /// Parses Postman's `grant_type` values (plus RFC 6749 spellings).
    pub fn parse(s: &str) -> GrantType {
        match s.trim() {
            "authorization_code_with_pkce" => GrantType::AuthorizationCodeWithPkce,
            "implicit" | "token" => GrantType::Implicit,
            "password_credentials" | "password" => GrantType::Password,
            "client_credentials" => GrantType::ClientCredentials,
            _ => GrantType::AuthorizationCode,
        }
    }

    pub fn of(auth: &Auth) -> GrantType {
        GrantType::parse(auth.get("grant_type"))
    }

    /// Grants that need a browser.
    pub fn is_interactive(self) -> bool {
        matches!(self, GrantType::AuthorizationCode | GrantType::AuthorizationCodeWithPkce | GrantType::Implicit)
    }
}

/// RFC 7636 code challenge for a verifier.
pub fn pkce_challenge(verifier: &str, method: &str) -> String {
    if method.eq_ignore_ascii_case("plain") {
        verifier.to_string()
    } else {
        B64_URL.encode(Sha256::digest(verifier.as_bytes()))
    }
}

/// A random 64-character code verifier (RFC 7636 section 4.1).
pub fn generate_code_verifier() -> String {
    random_alnum(64)
}

/// Key under which the token for this request's auth is stored.
pub fn owner_key(req: &PreparedRequest) -> String {
    if let Some(id) = req.auth_owner_id.as_ref().filter(|s| !s.is_empty()) {
        return id.clone();
    }
    let a = &req.auth;
    let ident = ["accessTokenUrl", "authUrl", "clientId", "grant_type", "scope", "username"]
        .iter()
        .map(|k| a.get(k))
        .collect::<Vec<_>>()
        .join("\u{1f}");
    format!("oauth2-anon-{}", &hex::encode(Sha256::digest(ident.as_bytes()))[..32])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SendAs {
    Body,
    Url,
    Header,
}

struct ExtraParam {
    key: String,
    value: String,
    send_as: SendAs,
}

/// Postman's `authRequestParams` / `tokenRequestParams` / `refreshRequestParams`.
fn extra_params(auth: &Auth, key: &str) -> Vec<ExtraParam> {
    let Some(Value::Array(items)) = auth.params.get(key) else { return Vec::new() };
    items
        .iter()
        .filter(|p| p.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|p| {
            let k = p.get("key")?.as_str()?.to_string();
            if k.is_empty() {
                return None;
            }
            let value = match p.get("value") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            };
            let send_as = match p.get("send_as").and_then(Value::as_str) {
                Some("request_url") => SendAs::Url,
                Some("request_header") => SendAs::Header,
                _ => SendAs::Body,
            };
            Some(ExtraParam { key: k, value, send_as })
        })
        .collect()
}

fn require<'a>(auth: &'a Auth, key: &'static str) -> Result<&'a str, AuthError> {
    let v = param(auth, key);
    if v.is_empty() { Err(AuthError::Missing { scheme: "OAuth 2.0", field: key }) } else { Ok(v) }
}

fn opt(auth: &Auth, key: &str) -> Option<String> {
    let v = param(auth, key);
    (!v.is_empty()).then(|| v.to_string())
}

/// Parses a token endpoint (or implicit redirect) response.
pub(crate) fn parse_token(fields: &HashMap<String, Value>, token_name: &str) -> Result<OAuth2Token, AuthError> {
    let s = |k: &str| match fields.get(k) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    if let Some(error) = s("error") {
        return Err(AuthError::OAuth2Server { error, description: s("error_description") });
    }
    let access_token = s("access_token").ok_or_else(|| AuthError::OAuth2("response has no access_token".into()))?;
    let now = now_secs();
    let expires_at = s("expires_in").and_then(|e| e.parse::<f64>().ok()).map(|e| now + e as i64);
    Ok(OAuth2Token {
        access_token,
        token_type: s("token_type").unwrap_or_else(|| "Bearer".into()),
        refresh_token: s("refresh_token"),
        id_token: s("id_token"),
        expires_at,
        scope: s("scope"),
        obtained_at: now,
        name: token_name.to_string(),
    })
}

fn body_fields(content_type: &str, text: &str) -> HashMap<String, Value> {
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(text) {
        return o.into_iter().collect();
    }
    if content_type.contains("json") {
        return HashMap::new();
    }
    url::form_urlencoded::parse(text.as_bytes()).map(|(k, v)| (k.into_owned(), Value::String(v.into_owned()))).collect()
}

/// POSTs to the token endpoint with client authentication and extra params.
async fn token_request(
    auth: &Auth,
    http: &dyn TokenHttp,
    url: &str,
    form: Vec<(String, String)>,
    extra_key: &str,
) -> Result<OAuth2Token, AuthError> {
    token_request_within(auth, http, url, form, extra_key, TOKEN_TIMEOUT).await
}

/// [`token_request`] that gives up after `timeout`.
async fn token_request_within(
    auth: &Auth,
    http: &dyn TokenHttp,
    url: &str,
    mut form: Vec<(String, String)>,
    extra_key: &str,
    timeout: Duration,
) -> Result<OAuth2Token, AuthError> {
    let client_id = auth.get("clientId");
    let client_secret = auth.get("clientSecret");
    let in_body = auth.get("client_authentication").eq_ignore_ascii_case("body");
    let mut headers: Vec<(String, String)> = Vec::new();
    if in_body || client_secret.is_empty() {
        // Public clients (no secret) identify themselves in the body.
        if !client_id.is_empty() {
            form.push(("client_id".into(), client_id.into()));
        }
        if !client_secret.is_empty() {
            form.push(("client_secret".into(), client_secret.into()));
        }
    } else {
        headers.push(("Authorization".into(), format!("Basic {}", B64.encode(format!("{client_id}:{client_secret}")))));
    }
    let mut target = url::Url::parse(url).map_err(|_| AuthError::Url(url.to_string()))?;
    for p in extra_params(auth, extra_key) {
        match p.send_as {
            SendAs::Body => form.push((p.key, p.value)),
            SendAs::Url => {
                target.query_pairs_mut().append_pair(&p.key, &p.value);
            }
            SendAs::Header => headers.push((p.key, p.value)),
        }
    }
    let mut rb = http
        .client_for(target.as_str())?
        .post(target)
        .timeout(timeout)
        .header("Accept", "application/json")
        .form(&form);
    for (k, v) in headers {
        rb = rb.header(k, v);
    }
    let resp = rb.send().await.map_err(|e| match e.is_timeout() {
        true if timeout.as_secs() > 0 => {
            AuthError::Http(format!("no answer from the token endpoint within {} s", timeout.as_secs()))
        }
        true => AuthError::Http(format!("no answer from the token endpoint within {} ms", timeout.as_millis())),
        false => AuthError::Http(describe_reqwest(&e)),
    })?;
    let status = resp.status();
    let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let text = resp.text().await.map_err(|e| AuthError::Http(describe_reqwest(&e)))?;
    let fields = body_fields(&ct, &text);
    match parse_token(&fields, auth.get("tokenName")) {
        Ok(t) => Ok(t),
        Err(AuthError::OAuth2(_)) if !status.is_success() => {
            let snippet: String = text.chars().take(200).collect();
            Err(AuthError::Http(format!("HTTP {}: {snippet}", status.as_u16())))
        }
        Err(e) => Err(e),
    }
}

fn describe_reqwest(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

fn push_common(auth: &Auth, form: &mut Vec<(String, String)>) {
    for key in ["scope", "audience", "resource"] {
        if let Some(v) = opt(auth, key) {
            form.push((key.into(), v));
        }
    }
}

/// Client credentials or password grant: no user interaction needed.
async fn fetch_direct(auth: &Auth, http: &dyn TokenHttp) -> Result<OAuth2Token, AuthError> {
    let url = require(auth, "accessTokenUrl")?;
    let mut form = Vec::new();
    match GrantType::of(auth) {
        GrantType::Password => {
            form.push(("grant_type".into(), "password".into()));
            form.push(("username".into(), auth.get("username").into()));
            form.push(("password".into(), auth.get("password").into()));
        }
        _ => form.push(("grant_type".into(), "client_credentials".into())),
    }
    push_common(auth, &mut form);
    token_request(auth, http, url, form, "tokenRequestParams").await
}

/// Uses a refresh token to get a new access token. A refresh token missing
/// from the response means the old one stays valid. `http` is a
/// `reqwest::Client` or the engine (see [`AuthContext::http`]).
pub async fn refresh(auth: &Auth, token: &OAuth2Token, http: &dyn TokenHttp) -> Result<OAuth2Token, AuthError> {
    let refresh_token = token
        .refresh_token
        .as_deref()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AuthError::OAuth2("no refresh token".into()))?;
    let url = match opt(auth, "refreshTokenUrl") {
        Some(u) => u,
        None => require(auth, "accessTokenUrl")?.to_string(),
    };
    let mut form = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh_token.to_string()),
    ];
    if let Some(scope) = opt(auth, "scope") {
        form.push(("scope".into(), scope));
    }
    let mut new = token_request(auth, http, &url, form, "refreshRequestParams").await?;
    if new.refresh_token.is_none() {
        new.refresh_token = token.refresh_token.clone();
    }
    if new.name.is_empty() {
        new.name = token.name.clone();
    }
    Ok(new)
}

/// One-shot HTTP listener that captures the authorization redirect.
struct RedirectListener {
    listener: TcpListener,
    path: String,
    redirect_uri: String,
}

async fn bind_redirect(redirect_uri: &str) -> Result<RedirectListener, AuthError> {
    if redirect_uri.trim().is_empty() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        return Ok(RedirectListener {
            listener,
            path: "/callback".into(),
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        });
    }
    let url = url::Url::parse(redirect_uri).map_err(|_| AuthError::Url(redirect_uri.to_string()))?;
    let host = url.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_string();
    let local = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1");
    if url.scheme() != "http" || !local {
        return Err(AuthError::OAuth2(format!(
            "the callback URL must be http://localhost:<port>/... (or 127.0.0.1) so Merpati can receive the redirect; got {redirect_uri}"
        )));
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let bind_host = if host == "localhost" { "127.0.0.1" } else { host.as_str() };
    let listener = TcpListener::bind((bind_host, port))
        .await
        .map_err(|e| AuthError::OAuth2(format!("could not listen on {bind_host}:{port} for the redirect: {e}")))?;
    let path = if url.path().is_empty() { "/".to_string() } else { url.path().to_string() };
    Ok(RedirectListener { listener, path, redirect_uri: redirect_uri.to_string() })
}

const PAGE_DONE: &str = "<!doctype html><meta charset=utf-8><title>Merpati</title><body style=\"font-family:system-ui;padding:3em\"><h2>Authorization complete</h2><p>You can close this window and return to Merpati.</p>";
const PAGE_FRAGMENT: &str = "<!doctype html><meta charset=utf-8><title>Merpati</title><body style=\"font-family:system-ui;padding:3em\"><p id=m>Completing authorization…</p><script>if(location.hash.length>1){location.replace(location.pathname+'?'+location.hash.substring(1));}else{document.getElementById('m').textContent='No authorization data was received.';}</script>";

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

impl RedirectListener {
    /// Waits for a request on the callback path carrying `code`, `error` or
    /// `access_token` and returns its query parameters.
    async fn wait(self, timeout: Duration) -> Result<HashMap<String, String>, AuthError> {
        // Connections are served concurrently: browsers open idle
        // speculative connections that must not hold up the callback.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let path = std::sync::Arc::new(self.path);
        let fut = async {
            loop {
                tokio::select! {
                    accepted = self.listener.accept() => {
                        let (stream, _) = accepted?;
                        let (tx, path) = (tx.clone(), path.clone());
                        tokio::spawn(async move {
                            if let Some(params) = serve_callback(stream, &path).await {
                                let _ = tx.send(params);
                            }
                        });
                    }
                    Some(params) = rx.recv() => return Ok::<_, AuthError>(params),
                }
            }
        };
        tokio::time::timeout(timeout, fut).await.map_err(|_| AuthError::Timeout)?
    }
}

/// Answers one connection to the redirect listener; the query parameters
/// when it is the redirect carrying `code`, `error` or `access_token`.
async fn serve_callback(mut stream: tokio::net::TcpStream, callback_path: &str) -> Option<HashMap<String, String>> {
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
    if buf.is_empty() {
        return None;
    }
    let head = String::from_utf8_lossy(&buf);
    let target = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").to_string();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if path != callback_path {
        respond(&mut stream, "404 Not Found", "not found").await;
        return None;
    }
    let params: HashMap<String, String> =
        url::form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    if params.contains_key("code") || params.contains_key("error") || params.contains_key("access_token") {
        respond(&mut stream, "200 OK", PAGE_DONE).await;
        return Some(params);
    }
    // Implicit flow: data is in the fragment, which only the browser sees.
    respond(&mut stream, "200 OK", PAGE_FRAGMENT).await;
    None
}

fn check_redirect(params: &HashMap<String, String>, state: &str) -> Result<(), AuthError> {
    if let Some(error) = params.get("error") {
        return Err(AuthError::OAuth2Server {
            error: error.clone(),
            description: params.get("error_description").cloned(),
        });
    }
    if !state.is_empty() && params.get("state").map(String::as_str) != Some(state) {
        return Err(AuthError::OAuth2("state returned by the authorization server does not match".into()));
    }
    Ok(())
}

/// Builds the authorization endpoint URL.
pub fn authorization_url(
    auth: &Auth,
    response_type: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: Option<(&str, &str)>,
) -> Result<String, AuthError> {
    let base = require(auth, "authUrl")?;
    let mut url = url::Url::parse(base).map_err(|_| AuthError::Url(base.to_string()))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", response_type);
        q.append_pair("client_id", auth.get("clientId"));
        q.append_pair("redirect_uri", redirect_uri);
        if let Some(scope) = opt(auth, "scope") {
            q.append_pair("scope", &scope);
        }
        if !state.is_empty() {
            q.append_pair("state", state);
        }
        if let Some((challenge, method)) = code_challenge {
            q.append_pair("code_challenge", challenge);
            q.append_pair("code_challenge_method", if method.eq_ignore_ascii_case("plain") { "plain" } else { "S256" });
        }
        for key in ["audience", "resource"] {
            if let Some(v) = opt(auth, key) {
                q.append_pair(key, &v);
            }
        }
        for p in extra_params(auth, "authRequestParams") {
            q.append_pair(&p.key, &p.value);
        }
    }
    Ok(url.to_string())
}

/// "Get New Access Token": runs the configured grant, opening the browser
/// when user interaction is required. `http` is a `reqwest::Client` or the
/// engine (see [`AuthContext::http`]).
pub async fn get_new_token(
    auth: &Auth,
    http: &dyn TokenHttp,
    open_browser: &(dyn Fn(&str) + Send + Sync),
) -> Result<OAuth2Token, AuthError> {
    let grant = GrantType::of(auth);
    if !grant.is_interactive() {
        return fetch_direct(auth, http).await;
    }
    require(auth, "authUrl")?;
    require(auth, "clientId")?;
    let state = opt(auth, "state").unwrap_or_else(|| random_alnum(24));
    let listener = bind_redirect(param(auth, "redirect_uri")).await?;
    let redirect_uri = listener.redirect_uri.clone();

    if grant == GrantType::Implicit {
        let url = authorization_url(auth, "token", &redirect_uri, &state, None)?;
        open_browser(&url);
        let params = listener.wait(REDIRECT_TIMEOUT).await?;
        check_redirect(&params, &state)?;
        let fields = params.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
        return parse_token(&fields, auth.get("tokenName"));
    }

    let token_url = require(auth, "accessTokenUrl")?.to_string();
    let pkce = grant == GrantType::AuthorizationCodeWithPkce;
    let verifier = if pkce { opt(auth, "code_verifier").unwrap_or_else(generate_code_verifier) } else { String::new() };
    let method = match param(auth, "challengeAlgorithm") {
        "" => "S256",
        m => m,
    };
    let challenge = pkce.then(|| pkce_challenge(&verifier, method));
    let url = authorization_url(auth, "code", &redirect_uri, &state, challenge.as_deref().map(|c| (c, method)))?;
    open_browser(&url);
    let params = listener.wait(REDIRECT_TIMEOUT).await?;
    check_redirect(&params, &state)?;
    let code =
        params.get("code").cloned().ok_or_else(|| AuthError::OAuth2("redirect has no authorization code".into()))?;
    let mut form = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code),
        ("redirect_uri".to_string(), redirect_uri),
    ];
    if pkce {
        form.push(("code_verifier".into(), verifier));
    }
    token_request(auth, http, &token_url, form, "tokenRequestParams").await
}

fn place_token(req: &mut PreparedRequest, auth: &Auth, token: &str) {
    if auth.get("addTokenTo").eq_ignore_ascii_case("queryParams") {
        req.add_query("access_token", token);
        return;
    }
    let prefix = match auth.params.get("headerPrefix") {
        None => "Bearer",
        Some(_) => param(auth, "headerPrefix"),
    };
    if prefix.is_empty() {
        req.set_header("Authorization", token.to_string());
    } else {
        req.set_header("Authorization", format!("{prefix} {token}"));
    }
}

/// Attaches a token: a valid stored one, a refreshed one, the manually
/// pasted `accessToken`, or a freshly fetched one for non-interactive grants.
pub(crate) async fn apply(req: &mut PreparedRequest, ctx: &AuthContext<'_>) -> Result<AuthApplied, AuthError> {
    let auth = req.auth.clone();
    let key = owner_key(req);
    let grant = GrantType::of(&auth);
    let mut applied = AuthApplied::default();
    let stored = ctx.tokens.get(&key);
    let mut token: Option<String> =
        stored.as_ref().filter(|t| !t.is_expired(EXPIRY_SKEW_SECS)).map(|t| t.access_token.clone());

    if token.is_none()
        && auth.get_bool("autoRefresh", true)
        && let Some(old) = stored.as_ref().filter(|t| t.refresh_token.is_some())
    {
        match refresh(&auth, old, ctx.http).await {
            Ok(new) => {
                token = Some(new.access_token.clone());
                ctx.tokens.set(&key, Some(new));
                applied.notes.push("Refreshed the OAuth 2.0 access token".into());
            }
            Err(e) => applied.notes.push(format!("Could not refresh the OAuth 2.0 token: {e}")),
        }
    }
    if token.is_none() {
        token = opt(&auth, "accessToken");
    }
    if token.is_none() && !grant.is_interactive() && !param(&auth, "accessTokenUrl").is_empty() {
        let new = fetch_direct(&auth, ctx.http).await?;
        token = Some(new.access_token.clone());
        ctx.tokens.set(&key, Some(new));
        applied.notes.push("Fetched a new OAuth 2.0 access token".into());
    }
    if token.is_none()
        && let Some(old) = stored
    {
        applied.notes.push("The OAuth 2.0 access token has expired".into());
        token = Some(old.access_token);
    }
    match token {
        Some(t) => place_token(req, &auth, &t),
        None => applied.notes.push("No OAuth 2.0 access token yet; use \"Get New Access Token\"".into()),
    }
    Ok(applied)
}

/// Synchronous part of `apply` for previews: stored token is not available
/// here, so only a manually pasted token is shown.
pub(crate) fn preview(req: &mut PreparedRequest) -> bool {
    let auth = req.auth.clone();
    match opt(&auth, "accessToken") {
        Some(t) => {
            place_token(req, &auth, &t);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", "S256"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(pkce_challenge("abc", "plain"), "abc");
        let v = generate_code_verifier();
        assert!((43..=128).contains(&v.len()));
    }

    #[test]
    fn token_parsing() {
        let fields =
            body_fields("application/json", r#"{"access_token":"a","expires_in":"3600","token_type":"bearer"}"#);
        let t = parse_token(&fields, "mine").unwrap();
        assert_eq!(t.access_token, "a");
        assert!(t.expires_at.unwrap() > now_secs() + 3500);
        assert_eq!(t.name, "mine");
        // GitHub-style form response.
        let fields = body_fields("application/x-www-form-urlencoded", "access_token=x&scope=repo&token_type=bearer");
        assert_eq!(parse_token(&fields, "").unwrap().scope.as_deref(), Some("repo"));
        let fields = body_fields("application/json", r#"{"error":"invalid_client","error_description":"nope"}"#);
        assert_eq!(
            parse_token(&fields, "").unwrap_err().to_string(),
            "OAuth 2.0 server returned `invalid_client`: nope"
        );
    }

    #[tokio::test]
    async fn silent_token_endpoints_time_out() {
        // Accepts connections and never answers.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                open.push(s);
            }
        });
        let auth = Auth::new(merpati_core::AuthType::OAuth2).with("clientId", "cid");
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::new();
        let started = std::time::Instant::now();
        let err =
            token_request_within(&auth, &http, &url, Vec::new(), "tokenRequestParams", Duration::from_millis(300))
                .await
                .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(err.to_string(), "token request failed: no answer from the token endpoint within 300 ms");
    }

    #[tokio::test]
    async fn idle_connections_do_not_hold_up_the_callback() {
        let listener = bind_redirect("").await.unwrap();
        let addr = listener.listener.local_addr().unwrap();
        let waiting = tokio::spawn(listener.wait(Duration::from_secs(30)));
        // A speculative connection that sends nothing (browsers open these).
        let _idle = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut real = tokio::net::TcpStream::connect(addr).await.unwrap();
        real.write_all(b"GET /callback?code=abc&state=s HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let started = std::time::Instant::now();
        let params = waiting.await.unwrap().unwrap();
        assert_eq!(params.get("code").map(String::as_str), Some("abc"));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn grant_type_names() {
        assert_eq!(GrantType::parse("password_credentials"), GrantType::Password);
        assert_eq!(GrantType::parse("authorization_code_with_pkce"), GrantType::AuthorizationCodeWithPkce);
        assert_eq!(GrantType::parse(""), GrantType::AuthorizationCode);
        assert!(!GrantType::ClientCredentials.is_interactive());
    }

    #[test]
    fn auth_url_contains_everything() {
        let auth = Auth::new(merpati_core::AuthType::OAuth2)
            .with("authUrl", "https://idp.test/authorize?prompt=login")
            .with("clientId", "cid")
            .with("scope", "openid profile")
            .with("audience", "api://x")
            .with("authRequestParams", serde_json::json!([{"key": "foo", "value": "bar", "enabled": true}, {"key": "off", "value": "1", "enabled": false}]));
        let u = authorization_url(&auth, "code", "http://localhost:5555/cb", "st", Some(("ch", "S256"))).unwrap();
        let parsed = url::Url::parse(&u).unwrap();
        let q: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["prompt"], "login");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], "openid profile");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["audience"], "api://x");
        assert_eq!(q["foo"], "bar");
        assert!(!q.contains_key("off"));
    }

    #[test]
    fn non_local_redirect_rejected() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err = rt.block_on(bind_redirect("https://oauth.example.com/callback")).err().unwrap();
        assert!(err.to_string().contains("localhost"));
    }
}
