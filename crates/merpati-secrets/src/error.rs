//! Errors that tell the user what to fix. Messages never contain secret
//! values or credentials: provider responses are only quoted for error
//! statuses, shortened, and scrubbed of every credential used for the call.

use serde::{Deserialize, Serialize};

/// What went wrong, so the UI can pick an icon or a "fix it" action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Provider settings are incomplete or invalid (missing address, region...).
    Config,
    /// The reference does not match the provider's syntax.
    InvalidReference,
    /// Credentials are missing, invalid or expired.
    Auth,
    /// Authenticated, but not allowed to read or list this secret.
    PermissionDenied,
    /// The secret, field, vault or item does not exist.
    NotFound,
    /// A command line tool (`op`, `az`, `credential_process`) could not be started.
    CliMissing,
    /// The provider could not be reached (DNS, TLS, connection refused).
    Network,
    /// The provider or tool did not answer in time.
    Timeout,
    /// Any other error reported by the provider.
    Provider,
    /// Unknown provider id or vault name that is not linked.
    NotLinked,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SecretError {
    pub kind: ErrorKind,
    pub message: String,
}

impl SecretError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        SecretError { kind, message: message.into() }
    }

    pub fn config(message: impl Into<String>) -> Self {
        SecretError::new(ErrorKind::Config, message)
    }

    pub fn reference(message: impl Into<String>) -> Self {
        SecretError::new(ErrorKind::InvalidReference, message)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        SecretError::new(ErrorKind::Auth, message)
    }

    pub fn denied(message: impl Into<String>) -> Self {
        SecretError::new(ErrorKind::PermissionDenied, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        SecretError::new(ErrorKind::NotFound, message)
    }

    pub fn provider(message: impl Into<String>) -> Self {
        SecretError::new(ErrorKind::Provider, message)
    }

    /// Maps a transport error, naming the provider and what to check.
    pub(crate) fn transport(what: &str, e: &reqwest::Error) -> Self {
        let mut cause = String::new();
        let mut src: Option<&dyn std::error::Error> = std::error::Error::source(e);
        while let Some(s) = src {
            cause = s.to_string();
            src = s.source();
        }
        let url = e.url().map(|u| format!(" at {}", strip_query(u))).unwrap_or_default();
        let detail = if cause.is_empty() { String::new() } else { format!(": {}", shorten(&cause)) };
        if e.is_timeout() {
            SecretError::new(ErrorKind::Timeout, format!("{what}{url} did not answer in time{detail}"))
        } else if e.is_connect() {
            SecretError::new(
                ErrorKind::Network,
                format!("Could not connect to {what}{url}{detail}. Check the address, network and proxy settings."),
            )
        } else {
            SecretError::new(ErrorKind::Network, format!("Request to {what}{url} failed{detail}"))
        }
    }
}

fn strip_query(u: &url::Url) -> String {
    let mut u = u.clone();
    u.set_query(None);
    u.set_fragment(None);
    let _ = u.set_password(None);
    u.to_string()
}

/// Removes credentials (at least 4 characters long) from a message.
pub(crate) fn scrub(message: &str, secrets: &[&str]) -> String {
    let mut out = message.to_string();
    for s in secrets {
        if s.len() >= 4 && out.contains(s) {
            out = out.replace(s, "***");
        }
    }
    out
}

/// First line of a provider message (continued while it ends with `:`, as
/// in Vault's `1 error occurred:\n\t* permission denied`), at most 300
/// characters.
pub(crate) fn shorten(message: &str) -> String {
    let mut lines = message.lines().map(str::trim).filter(|l| !l.is_empty());
    let mut line = lines.next().unwrap_or_default().to_string();
    while line.ends_with(':') {
        match lines.next() {
            Some(next) => {
                line.push(' ');
                line.push_str(next.trim_start_matches("* "));
            }
            None => break,
        }
    }
    let line = line.strip_prefix("1 error occurred: ").map(str::to_string).unwrap_or(line);
    if line.chars().count() > 300 {
        let mut s: String = line.chars().take(300).collect();
        s.push('…');
        s
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_and_shorten() {
        assert_eq!(scrub("token s3cr3t-tok rejected", &["s3cr3t-tok", "abc"]), "token *** rejected");
        assert_eq!(shorten("\n  first line \nsecond"), "first line");
        assert_eq!(shorten("1 error occurred:\n\t* permission denied\n\n"), "permission denied");
        assert_eq!(shorten("2 errors occurred:\n\t* a\n\t* b"), "2 errors occurred: a");
        assert_eq!(shorten(&"x".repeat(400)).chars().count(), 301);
        let e = SecretError::auth("bad");
        assert_eq!((e.to_string().as_str(), e.kind), ("bad", ErrorKind::Auth));
    }
}
