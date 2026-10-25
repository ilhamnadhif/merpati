/// Errors produced while applying auth. Messages are meant for the user and
/// never contain secret values.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{scheme}: `{field}` is required")]
    Missing { scheme: &'static str, field: &'static str },
    #[error("{0}")]
    Invalid(String),
    #[error("invalid request URL `{0}`")]
    Url(String),
    #[error("could not sign the JWT: {0}")]
    Jwt(String),
    #[error("invalid private key: {0}")]
    Key(String),
    #[error("OAuth 2.0: {0}")]
    OAuth2(String),
    #[error("OAuth 2.0 server returned `{error}`{}", .description.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
    OAuth2Server { error: String, description: Option<String> },
    #[error("token request failed: {0}")]
    Http(String),
    #[error("timed out waiting for the authorization redirect")]
    Timeout,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl AuthError {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        AuthError::Invalid(msg.into())
    }
}
