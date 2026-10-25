//! Auth schemes for Merpati (PRD FR-A1..A4).
//!
//! Every scheme works on the generic [`Auth`] model (Postman parameter names)
//! carried by a [`PreparedRequest`] whose variables are already resolved.
//! [`apply`] adds credentials before sending; [`handle_challenge`] answers
//! `401` challenges for Digest and NTLM (bound to the TLS channel over
//! HTTPS). Secrets are never logged.

mod awsv4;
pub mod challenge;
mod channel_binding;
mod digest;
mod edgegrid;
mod error;
mod fields;
mod hawk;
mod jwt;
mod ntlm;
mod oauth1;
pub mod oauth2;
mod simple;
mod util;

use merpati_core::tokens::TokenStore;
use merpati_core::{Auth, AuthType, PreparedRequest};

pub use error::AuthError;
pub use fields::{FieldKind, FieldSpec, fields};
pub use merpati_core::tokens::OAuth2Token;

/// What `apply` needs besides the request.
pub struct AuthContext<'a> {
    /// OAuth 2.0 token storage, keyed by the auth owner id.
    pub tokens: &'a dyn TokenStore,
    /// The client for token requests: a `reqwest::Client` (one for every
    /// URL), or the [`HttpEngine`](merpati_http::HttpEngine), which also
    /// presents the client certificate configured for the token endpoint.
    pub http: &'a dyn TokenHttp,
}

/// Where OAuth 2.0 token requests get their HTTP client from.
pub trait TokenHttp: Send + Sync {
    /// The client for a request to `url`.
    fn client_for(&self, url: &str) -> Result<reqwest::Client, AuthError>;
}

/// The same client (and so no client certificate, unless it has one) for
/// every token endpoint.
impl TokenHttp for reqwest::Client {
    fn client_for(&self, _url: &str) -> Result<reqwest::Client, AuthError> {
        Ok(self.clone())
    }
}

/// The engine's proxy and CA settings plus the client certificate
/// configured for the token endpoint's host, with the same rules as
/// requests (mutual TLS token endpoints, RFC 8705).
impl TokenHttp for merpati_http::HttpEngine {
    fn client_for(&self, url: &str) -> Result<reqwest::Client, AuthError> {
        self.client_for_url(url).map_err(|e| AuthError::Http(e.message))
    }
}

/// Informational messages about what `apply` did (fetched/refreshed tokens,
/// missing tokens). Never contains secrets.
#[derive(Debug, Default)]
pub struct AuthApplied {
    pub notes: Vec<String>,
}

/// Applies `req.auth` (already variable-resolved, never `Inherit`) to `req`.
pub async fn apply(req: &mut PreparedRequest, ctx: &AuthContext<'_>) -> Result<AuthApplied, AuthError> {
    if req.auth.kind == AuthType::OAuth2 {
        return oauth2::apply(req, ctx).await;
    }
    apply_sync(req)?;
    Ok(AuthApplied::default())
}

/// Every scheme except OAuth 2.0 (which may need the network).
fn apply_sync(req: &mut PreparedRequest) -> Result<(), AuthError> {
    let auth: Auth = req.auth.clone();
    match auth.kind {
        AuthType::Inherit | AuthType::None => Ok(()),
        AuthType::ApiKey => {
            simple::api_key(&auth, req);
            Ok(())
        }
        AuthType::Bearer => {
            simple::bearer(&auth, req);
            Ok(())
        }
        AuthType::Basic => {
            simple::basic(&auth, req);
            Ok(())
        }
        AuthType::Digest => digest::apply(req),
        AuthType::Jwt => jwt::apply_jwt(req),
        AuthType::OAuth1 => oauth1::apply(req),
        AuthType::Hawk => hawk::apply(req),
        AuthType::AwsV4 => awsv4::apply(req),
        AuthType::Ntlm => ntlm::apply(req),
        AuthType::EdgeGrid => edgegrid::apply(req),
        AuthType::Asap => jwt::apply_asap(req),
        AuthType::OAuth2 => {
            oauth2::preview(req);
            Ok(())
        }
    }
}

/// Answers a challenge for Digest and NTLM after a response. Returns `true`
/// when `req` was updated and should be sent again; the caller retries at
/// most 3 times. Other schemes and non-401 responses return `false`.
///
/// `peer_certificate` is the DER of the server's TLS leaf certificate the
/// response arrived with (`merpati_http::SendOutcome::peer_certificate`),
/// `None` over plain HTTP. NTLM binds its AUTHENTICATE message to it
/// (`tls-server-end-point`, RFC 5929), which servers enforcing Extended
/// Protection require.
pub fn handle_challenge(
    req: &mut PreparedRequest,
    status: u16,
    headers: &[(String, String)],
    peer_certificate: Option<&[u8]>,
) -> Result<bool, AuthError> {
    if status != 401 {
        return Ok(false);
    }
    match req.auth.kind {
        AuthType::Digest => digest::handle_challenge(req, headers),
        AuthType::Ntlm => ntlm::handle_challenge(req, headers, peer_certificate),
        _ => Ok(false),
    }
}

/// What applying the auth would change, computed without network access.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthPreview {
    /// Headers added or replaced.
    pub headers: Vec<(String, String)>,
    /// True when the URL (query string) or body changed too.
    pub url_changed: bool,
    pub body_changed: bool,
    /// The request with auth applied.
    pub request: PreparedRequest,
}

/// Shows the auth data `apply` would add, for codegen and the UI's
/// "auto-generated headers" view. Returns `None` for schemes whose data
/// depends on a server round trip (Digest without nonce, NTLM) or on stored
/// OAuth 2.0 tokens that are not pasted manually, and on errors.
pub fn preview(req: &PreparedRequest) -> Option<AuthPreview> {
    let mut signed = req.clone();
    match req.auth.kind {
        AuthType::Inherit | AuthType::None | AuthType::Ntlm => return None,
        AuthType::OAuth2 => {
            if !oauth2::preview(&mut signed) {
                return None;
            }
        }
        AuthType::Digest if req.auth.get("nonce").is_empty() => return None,
        _ => apply_sync(&mut signed).ok()?,
    }
    let headers: Vec<(String, String)> = signed.headers.iter().filter(|h| !req.headers.contains(h)).cloned().collect();
    Some(AuthPreview {
        headers,
        url_changed: signed.url != req.url,
        body_changed: signed.body != req.body,
        request: signed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::tokens::MemoryTokenStore;

    #[tokio::test]
    async fn apply_dispatches_and_ignores_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tokens = MemoryTokenStore::default();
        let http = reqwest::Client::new();
        let ctx = AuthContext { tokens: &tokens, http: &http };
        let mut req = PreparedRequest { url: "http://x/".into(), auth: Auth::none(), ..Default::default() };
        let before = req.clone();
        apply(&mut req, &ctx).await.unwrap();
        assert_eq!(req, before);
        req.auth = Auth::bearer("t");
        apply(&mut req, &ctx).await.unwrap();
        assert_eq!(req.header("authorization"), Some("Bearer t"));
    }

    #[test]
    fn preview_reports_added_headers() {
        let req = PreparedRequest { url: "http://x/".into(), auth: Auth::basic("a", "b"), ..Default::default() };
        let p = preview(&req).unwrap();
        assert_eq!(p.headers, vec![("Authorization".to_string(), "Basic YTpi".to_string())]);
        assert!(!p.url_changed);
        let q = PreparedRequest { url: "http://x/".into(), auth: Auth::api_key("k", "v", true), ..Default::default() };
        let p = preview(&q).unwrap();
        assert!(p.url_changed && p.headers.is_empty());
        let ntlm = PreparedRequest { url: "http://x/".into(), auth: Auth::new(AuthType::Ntlm), ..Default::default() };
        assert!(preview(&ntlm).is_none());
    }

    #[test]
    fn handle_challenge_only_on_401() {
        let mut req = PreparedRequest {
            url: "http://x/".into(),
            auth: Auth::new(AuthType::Digest).with("username", "u"),
            ..Default::default()
        };
        let h = vec![("www-authenticate".to_string(), r#"Digest realm="r", nonce="n""#.to_string())];
        assert!(!handle_challenge(&mut req, 403, &h, None).unwrap());
        assert!(handle_challenge(&mut req, 401, &h, None).unwrap());
    }
}
