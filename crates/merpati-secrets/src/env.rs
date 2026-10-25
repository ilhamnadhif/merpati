//! What providers need to talk to the outside world: the HTTP client,
//! credentials from the secret store, cached auth tokens and the process
//! environment.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use merpati_core::secrets::SecretStore;

use crate::error::{SecretError, scrub, shorten};
use crate::model::{KvVersion, credential_key};

/// A short-lived access token (Vault AppRole, Azure AD, AWS process credentials).
#[derive(Clone)]
pub(crate) struct CachedToken {
    pub value: String,
    pub expires: Instant,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken").field("value", &"<redacted>").field("expires", &self.expires).finish()
    }
}

/// Environment variables, overridable in tests.
#[derive(Clone, Default)]
pub(crate) struct ProcessEnv {
    overrides: Option<Arc<HashMap<String, String>>>,
}

impl ProcessEnv {
    #[cfg(test)]
    pub fn fixed(vars: &[(&str, &str)]) -> Self {
        ProcessEnv { overrides: Some(Arc::new(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())) }
    }

    /// Non-empty value of a variable.
    pub fn var(&self, key: &str) -> Option<String> {
        let v = match &self.overrides {
            Some(m) => m.get(key).cloned(),
            None => std::env::var(key).ok(),
        };
        v.filter(|s| !s.trim().is_empty())
    }

    pub fn home(&self) -> Option<PathBuf> {
        self.var("HOME").or_else(|| self.var("USERPROFILE")).map(PathBuf::from)
    }

    pub fn path_var(&self) -> Option<std::ffi::OsString> {
        match &self.overrides {
            Some(m) => m.get("PATH").map(Into::into),
            None => std::env::var_os("PATH"),
        }
    }
}

pub(crate) struct Env {
    pub http: RwLock<reqwest::Client>,
    pub store: Arc<SecretStore>,
    /// provider id -> auth token.
    pub tokens: Mutex<HashMap<String, CachedToken>>,
    /// (provider id, mount) -> detected KV version.
    pub kv_versions: Mutex<HashMap<(String, String), KvVersion>>,
    pub process: ProcessEnv,
    pub request_timeout: Duration,
    pub cli_timeout: Duration,
}

impl Env {
    pub fn new(store: Arc<SecretStore>, http: reqwest::Client) -> Self {
        Env {
            http: RwLock::new(http),
            store,
            tokens: Mutex::new(HashMap::new()),
            kv_versions: Mutex::new(HashMap::new()),
            process: ProcessEnv::default(),
            request_timeout: Duration::from_secs(20),
            cli_timeout: Duration::from_secs(60),
        }
    }

    pub fn http(&self) -> reqwest::Client {
        self.http.read().clone()
    }

    /// A non-empty credential of a provider.
    pub fn credential(&self, provider_id: &str, field: &str) -> Option<String> {
        self.store.get(&credential_key(provider_id, field)).filter(|s| !s.is_empty())
    }

    pub fn required_credential(
        &self,
        provider_id: &str,
        field: &str,
        label: &str,
        provider: &str,
    ) -> Result<String, SecretError> {
        self.credential(provider_id, field).ok_or_else(|| {
            SecretError::auth(format!(
                "No {label} is set for this {provider} provider; enter it in the provider settings."
            ))
        })
    }

    pub fn token(&self, provider_id: &str) -> Option<CachedToken> {
        let mut tokens = self.tokens.lock();
        match tokens.get(provider_id) {
            Some(t) if t.expires > Instant::now() => Some(t.clone()),
            Some(_) => {
                tokens.remove(provider_id);
                None
            }
            None => None,
        }
    }

    pub fn set_token(&self, provider_id: &str, value: String, lifetime: Duration) {
        // Renew a little early so a token does not expire mid-request.
        let margin = (lifetime / 10).min(Duration::from_secs(60));
        let expires = Instant::now() + lifetime.saturating_sub(margin);
        self.tokens.lock().insert(provider_id.to_string(), CachedToken { value, expires });
    }

    pub fn drop_token(&self, provider_id: &str) {
        self.tokens.lock().remove(provider_id);
    }

    pub fn forget(&self, provider_id: &str) {
        self.tokens.lock().remove(provider_id);
        self.kv_versions.lock().retain(|(p, _), _| p != provider_id);
    }
}

/// Status and body of an HTTP response.
pub(crate) struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpReply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self, what: &str) -> Result<Value, SecretError> {
        if self.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&self.body).map_err(|_| {
            SecretError::provider(format!("{what} returned a response that is not JSON (HTTP {}).", self.status))
        })
    }

    /// Error text from an error response body, shortened and scrubbed.
    pub fn error_text(&self, secrets: &[&str]) -> String {
        let text = match serde_json::from_slice::<Value>(&self.body) {
            Ok(v) => json_error_text(&v),
            Err(_) => String::from_utf8_lossy(&self.body).to_string(),
        };
        // Scrub first: shortening could cut a secret so that it no longer matches.
        shorten(&scrub(&text, secrets))
    }
}

/// The human part of common JSON error shapes.
fn json_error_text(v: &Value) -> String {
    if let Some(errs) = v.get("errors").and_then(Value::as_array) {
        let parts: Vec<String> =
            errs.iter().map(|e| e.as_str().map(str::to_string).unwrap_or_else(|| json_error_text(e))).collect();
        return parts.join("; ");
    }
    if let Some(e) = v.get("error") {
        if let Some(obj) = e.as_object() {
            let code = obj.get("code").and_then(Value::as_str).unwrap_or_default();
            let msg = obj.get("message").and_then(Value::as_str).unwrap_or_default();
            return [code, msg].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(": ");
        }
        if let Some(s) = e.as_str() {
            let desc = v.get("error_description").and_then(Value::as_str).unwrap_or_default();
            return if desc.is_empty() { s.to_string() } else { format!("{s}: {desc}") };
        }
    }
    for key in ["message", "Message", "detail"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    String::new()
}

/// Sends a request and reads the whole body.
pub(crate) async fn send(rb: reqwest::RequestBuilder, what: &str, timeout: Duration) -> Result<HttpReply, SecretError> {
    /// Secret manager answers are small; anything bigger is not one.
    const MAX_BODY: usize = 16 * 1024 * 1024;
    let mut resp = rb.timeout(timeout).send().await.map_err(|e| SecretError::transport(what, &e))?;
    let status = resp.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| SecretError::transport(what, &e))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BODY {
            return Err(SecretError::provider(format!("{what} sent a response larger than 16 MB.")));
        }
    }
    Ok(HttpReply { status, body })
}

/// Base URL without a trailing slash.
pub(crate) fn base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_shapes() {
        let r = |s: &str| HttpReply { status: 400, body: s.as_bytes().to_vec() };
        assert_eq!(r(r#"{"errors":["permission denied"]}"#).error_text(&[]), "permission denied");
        assert_eq!(
            r(r#"{"error":{"code":"SecretNotFound","message":"A secret with (name/id) x was not found"}}"#)
                .error_text(&[]),
            "SecretNotFound: A secret with (name/id) x was not found"
        );
        assert_eq!(
            r(r#"{"error":"invalid_client","error_description":"AADSTS7000215: Invalid client secret provided.\r\nTrace ID: 1"}"#)
                .error_text(&[]),
            "invalid_client: AADSTS7000215: Invalid client secret provided."
        );
        assert_eq!(r(r#"{"__type":"X","message":"tok abcd1234 bad"}"#).error_text(&["abcd1234"]), "tok *** bad");
        assert_eq!(r("plain text").error_text(&[]), "plain text");
    }

    #[test]
    fn error_text_scrubs_a_secret_cut_by_shortening() {
        // The secret straddles the 300 character limit of `shorten`.
        let body = serde_json::json!({ "message": format!("{} tok-0123456789abcdef rejected", "x".repeat(295)) });
        let reply = HttpReply { status: 400, body: body.to_string().into_bytes() };
        let text = reply.error_text(&["tok-0123456789abcdef"]);
        assert!(!text.contains("tok-"), "{text}");
    }

    #[test]
    fn token_cache_expires() {
        let env = Env::new(Arc::new(SecretStore::memory()), crate::default_client().unwrap());
        env.set_token("p", "t".into(), Duration::from_secs(100));
        assert_eq!(env.token("p").unwrap().value, "t");
        assert!(!format!("{:?}", env.token("p").unwrap()).contains("\"t\""));
        env.set_token("p", "t".into(), Duration::ZERO);
        assert!(env.token("p").is_none());
    }
}
