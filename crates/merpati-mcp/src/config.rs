//! Connection settings, built from the saved [`McpRequest`].

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use merpati_core::auth::AuthType;
use merpati_core::model::KeyValue;
pub use merpati_core::protocols::McpHttpTransport;
use merpati_core::protocols::{McpRequest, McpTransport};

use crate::oauth::OAuthSettings;

/// Default per-request timeout (also used for `initialize`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, PartialEq)]
pub enum Transport {
    /// Spawn `command` and speak newline-delimited JSON-RPC on stdin/stdout.
    Stdio { command: String, args: Vec<String>, env: Vec<(String, String)>, cwd: Option<PathBuf> },
    /// Remote server (`https://host/mcp`, or `https://host/sse` for the
    /// legacy transport); see [`McpConfig::http_transport`].
    Http { url: String, headers: Vec<(String, String)> },
}

#[derive(Clone, PartialEq)]
pub struct McpConfig {
    pub transport: Transport,
    /// Per-request timeout. Paused while the client waits for the user
    /// (browser login, sampling or elicitation dialogs). Over HTTP it also
    /// limits each POST of a notification or response.
    pub timeout: Duration,
    /// Wire protocol for [`Transport::Http`].
    pub http_transport: McpHttpTransport,
    /// Client settings for the OAuth flow, run when the server answers 401
    /// and the host supplied [`crate::OAuthOptions`]. `None` disables it
    /// (static credentials are configured).
    pub oauth: Option<OAuthSettings>,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            transport: Transport::Http { url: String::new(), headers: Vec::new() },
            timeout: DEFAULT_TIMEOUT,
            http_transport: McpHttpTransport::Auto,
            oauth: Some(OAuthSettings::default()),
        }
    }
}

fn enabled_pairs(rows: &[KeyValue], resolve: &dyn Fn(&str) -> String) -> Vec<(String, String)> {
    rows.iter()
        .filter(|kv| kv.enabled && !kv.key.trim().is_empty())
        .map(|kv| (resolve(kv.key.trim()), resolve(&kv.value)))
        .collect()
}

impl McpConfig {
    /// Resolves variables in every field. The request's auth (already resolved
    /// by the caller) becomes an `Authorization: Bearer` or `Basic` header, or
    /// an API key header or query parameter, for HTTP servers unless one is
    /// set explicitly; OAuth 2.0 auth
    /// supplies the client for the MCP authorization flow (`clientId`,
    /// `clientSecret`, `scope`, `redirect_uri`, optional `authUrl` /
    /// `accessTokenUrl` overriding discovery, `client_authentication`).
    pub fn from_request(req: &McpRequest, resolve: &dyn Fn(&str) -> String) -> McpConfig {
        let mut oauth = None;
        let transport = match &req.transport {
            McpTransport::Stdio { command, args, env, cwd } => {
                let cwd = resolve(cwd.trim());
                Transport::Stdio {
                    command: resolve(command.trim()),
                    args: args.iter().map(|a| resolve(a)).collect(),
                    env: enabled_pairs(env, resolve),
                    cwd: if cwd.trim().is_empty() { None } else { Some(PathBuf::from(cwd.trim())) },
                }
            }
            McpTransport::Http { url, headers } => {
                let mut headers = enabled_pairs(headers, resolve);
                let has = |headers: &[(String, String)], name: &str| {
                    headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
                };
                let mut api_key = false;
                let mut url = resolve(url.trim()).trim().to_string();
                match req.auth.kind {
                    AuthType::Bearer => {
                        let token = resolve(req.auth.get("token"));
                        let token = token.trim();
                        if !token.is_empty() && !has(&headers, "authorization") {
                            headers.push(("Authorization".into(), format!("Bearer {token}")));
                        }
                    }
                    AuthType::Basic => {
                        let (user, pass) = (resolve(req.auth.get("username")), resolve(req.auth.get("password")));
                        if (!user.is_empty() || !pass.is_empty()) && !has(&headers, "authorization") {
                            let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
                            headers.push(("Authorization".into(), format!("Basic {encoded}")));
                        }
                    }
                    AuthType::ApiKey => {
                        let key = resolve(req.auth.get("key")).trim().to_string();
                        api_key = !key.is_empty();
                        let value = resolve(req.auth.get("value"));
                        if api_key && req.auth.get("in") == "query" {
                            let pair: String =
                                url::form_urlencoded::Serializer::new(String::new()).append_pair(&key, &value).finish();
                            let (base, fragment) = url.split_once('#').map_or((url.as_str(), ""), |(b, f)| (b, f));
                            let sep = if base.contains('?') { '&' } else { '?' };
                            let frag = if fragment.is_empty() { String::new() } else { format!("#{fragment}") };
                            url = format!("{base}{sep}{pair}{frag}");
                        } else if api_key && !has(&headers, &key) {
                            headers.push((key, value));
                        }
                    }
                    _ => {}
                }
                let static_auth = has(&headers, "authorization") || api_key;
                if !static_auth {
                    oauth = Some(if req.auth.kind == AuthType::OAuth2 {
                        oauth_settings(&req.auth, resolve)
                    } else {
                        OAuthSettings::default()
                    });
                }
                Transport::Http { url, headers }
            }
        };
        McpConfig { transport, timeout: DEFAULT_TIMEOUT, http_transport: req.http_transport, oauth }
    }

    /// Short description for logs: the command line or the URL (never headers).
    pub fn describe(&self) -> String {
        match &self.transport {
            Transport::Stdio { command, args, .. } => {
                let mut s = command.clone();
                for a in args {
                    s.push(' ');
                    s.push_str(a);
                }
                s
            }
            // Without the query, which may carry an API key.
            Transport::Http { url, .. } => url.split('?').next().unwrap_or_default().to_string(),
        }
    }
}

/// OAuth client settings from an OAuth 2.0 [`merpati_core::auth::Auth`].
fn oauth_settings(auth: &merpati_core::auth::Auth, resolve: &dyn Fn(&str) -> String) -> OAuthSettings {
    let get = |k: &str| resolve(auth.get(k)).trim().to_string();
    OAuthSettings {
        client_id: get("clientId"),
        client_secret: get("clientSecret"),
        scope: get("scope"),
        redirect_uri: get("redirect_uri"),
        authorization_endpoint: get("authUrl"),
        token_endpoint: get("accessTokenUrl"),
        token_endpoint_auth_method: match auth.get("client_authentication") {
            "body" => "client_secret_post".into(),
            "header" => "client_secret_basic".into(),
            _ => String::new(),
        },
    }
}

pub(crate) fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "authorization" || n == "cookie" || n.contains("key") || n.contains("token") || n.contains("secret")
}

fn redacted(pairs: &[(String, String)]) -> Vec<(String, &str)> {
    pairs.iter().map(|(k, v)| (k.clone(), if is_sensitive_header(k) { "<redacted>" } else { v.as_str() })).collect()
}

impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Transport::Stdio { command, args, env, cwd } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", &redacted(env))
                .field("cwd", cwd)
                .finish(),
            Transport::Http { url, headers } => {
                f.debug_struct("Http").field("url", url).field("headers", &redacted(headers)).finish()
            }
        }
    }
}

impl fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpConfig")
            .field("transport", &self.transport)
            .field("timeout", &self.timeout)
            .field("http_transport", &self.http_transport)
            .field("oauth", &self.oauth)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::auth::Auth;

    #[test]
    fn from_request_resolves_and_adds_bearer() {
        let req = McpRequest {
            transport: McpTransport::Http {
                url: " {{base}}/mcp ".into(),
                headers: vec![KeyValue::new("X-Team", "{{team}}"), KeyValue::new("X-Off", "1").disabled()],
            },
            saved_calls: vec![],
            auth: Auth::bearer("sekret"),
            http_transport: McpHttpTransport::Sse,
        };
        let resolve = |s: &str| s.replace("{{base}}", "http://h").replace("{{team}}", "core");
        let cfg = McpConfig::from_request(&req, &resolve);
        let Transport::Http { url, headers } = &cfg.transport else { panic!() };
        assert_eq!(url, "http://h/mcp");
        assert_eq!(
            headers,
            &vec![("X-Team".to_string(), "core".to_string()), ("Authorization".into(), "Bearer sekret".into())]
        );
        assert!(!format!("{cfg:?}").contains("sekret"));
        assert_eq!(cfg.http_transport, McpHttpTransport::Sse);
        assert!(cfg.oauth.is_none(), "a static token disables the OAuth flow");
    }

    #[test]
    fn basic_auth_and_query_api_keys() {
        let mut req = McpRequest {
            transport: McpTransport::Http { url: "http://h/mcp?v=1#x".into(), headers: vec![] },
            ..Default::default()
        };
        req.auth = Auth::new(AuthType::Basic).with("username", "ada").with("password", "{{pw}}");
        let resolve = |s: &str| s.replace("{{pw}}", "s3cret");
        let cfg = McpConfig::from_request(&req, &resolve);
        let Transport::Http { headers, .. } = &cfg.transport else { panic!() };
        assert_eq!(headers, &vec![("Authorization".to_string(), "Basic YWRhOnMzY3JldA==".to_string())]);
        assert!(cfg.oauth.is_none());

        req.auth = Auth::new(AuthType::ApiKey).with("key", "api key").with("value", "k&1").with("in", "query");
        let cfg = McpConfig::from_request(&req, &resolve);
        let Transport::Http { url, headers } = &cfg.transport else { panic!() };
        assert_eq!(url, "http://h/mcp?v=1&api+key=k%261#x");
        assert!(headers.is_empty());
        assert!(cfg.oauth.is_none(), "a static key disables the OAuth flow");
        assert_eq!(cfg.describe(), "http://h/mcp");
    }

    #[test]
    fn from_request_oauth_settings() {
        let mut req = McpRequest {
            transport: McpTransport::Http { url: "https://h/mcp".into(), headers: vec![] },
            ..Default::default()
        };
        let resolve = |s: &str| s.replace("{{secret}}", "shh");
        let cfg = McpConfig::from_request(&req, &resolve);
        assert_eq!(cfg.oauth, Some(OAuthSettings::default()), "automatic discovery and registration");
        assert_eq!(cfg.http_transport, McpHttpTransport::Auto);
        req.auth = Auth::new(AuthType::OAuth2)
            .with("clientId", "cid")
            .with("clientSecret", "{{secret}}")
            .with("scope", "read")
            .with("redirect_uri", "http://localhost:8765/cb")
            .with("client_authentication", "body");
        let cfg = McpConfig::from_request(&req, &resolve);
        let o = cfg.oauth.clone().unwrap();
        assert_eq!((o.client_id.as_str(), o.client_secret.as_str(), o.scope.as_str()), ("cid", "shh", "read"));
        assert_eq!(o.redirect_uri, "http://localhost:8765/cb");
        assert_eq!(o.token_endpoint_auth_method, "client_secret_post");
        assert!(!format!("{cfg:?}").contains("shh"));
        req.auth = Auth::api_key("X-Key", "v", false);
        assert!(McpConfig::from_request(&req, &resolve).oauth.is_none());
    }

    #[test]
    fn from_request_resolves_auth_variables() {
        let mut req = McpRequest {
            transport: McpTransport::Http { url: "https://h/mcp".into(), headers: vec![] },
            auth: Auth::bearer("{{token}}"),
            ..Default::default()
        };
        let resolve =
            |s: &str| s.replace("{{token}}", "sekret").replace("{{header}}", "X-Key").replace("{{key}}", "k1");
        let headers = |cfg: &McpConfig| match &cfg.transport {
            Transport::Http { headers, .. } => headers.clone(),
            Transport::Stdio { .. } => panic!("not http"),
        };
        let cfg = McpConfig::from_request(&req, &resolve);
        assert_eq!(headers(&cfg), vec![("Authorization".to_string(), "Bearer sekret".to_string())]);
        assert!(cfg.oauth.is_none());
        req.auth = Auth::api_key("{{header}}", "{{key}}", false);
        let cfg = McpConfig::from_request(&req, &resolve);
        assert_eq!(headers(&cfg), vec![("X-Key".to_string(), "k1".to_string())]);
        assert!(cfg.oauth.is_none());
        // A token that resolves to nothing sends no header and leaves OAuth on.
        req.auth = Auth::bearer("{{missing}}");
        let cfg = McpConfig::from_request(&req, &|s: &str| s.replace("{{missing}}", ""));
        assert!(headers(&cfg).is_empty());
        assert!(cfg.oauth.is_some());
    }

    #[test]
    fn from_request_stdio() {
        let req = McpRequest {
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "{{pkg}}".into()],
                env: vec![KeyValue::new("TOKEN", "{{t}}")],
                cwd: String::new(),
            },
            ..Default::default()
        };
        let resolve = |s: &str| s.replace("{{pkg}}", "server-everything").replace("{{t}}", "x");
        let cfg = McpConfig::from_request(&req, &resolve);
        assert_eq!(
            cfg.transport,
            Transport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "server-everything".into()],
                env: vec![("TOKEN".into(), "x".into())],
                cwd: None
            }
        );
        assert_eq!(cfg.describe(), "npx -y server-everything");
    }
}
