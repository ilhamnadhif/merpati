//! Network errors classified by their real cause, so the UI can say "DNS
//! lookup failed" instead of a generic "error sending request".

use std::error::Error as StdError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HttpErrorKind {
    InvalidUrl,
    InvalidHeader,
    Dns,
    ConnectionRefused,
    ConnectionReset,
    Timeout,
    Certificate,
    Tls,
    Proxy,
    TooManyRedirects,
    Cancelled,
    /// Reading a body file (binary or form-data upload) failed.
    File,
    Protocol,
    Other,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct HttpError {
    pub kind: HttpErrorKind,
    /// One-line human readable explanation.
    pub message: String,
    /// The full error chain, for the console.
    pub detail: String,
}

impl HttpError {
    pub fn new(kind: HttpErrorKind, message: impl Into<String>) -> Self {
        let message = message.into();
        HttpError { kind, detail: message.clone(), message }
    }

    pub fn cancelled() -> Self {
        HttpError::new(HttpErrorKind::Cancelled, "Request cancelled")
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            HttpErrorKind::InvalidUrl => "Invalid URL",
            HttpErrorKind::InvalidHeader => "Invalid header",
            HttpErrorKind::Dns => "DNS lookup failed",
            HttpErrorKind::ConnectionRefused => "Connection refused",
            HttpErrorKind::ConnectionReset => "Connection reset",
            HttpErrorKind::Timeout => "Timeout",
            HttpErrorKind::Certificate => "Invalid certificate",
            HttpErrorKind::Tls => "TLS error",
            HttpErrorKind::Proxy => "Proxy error",
            HttpErrorKind::TooManyRedirects => "Too many redirects",
            HttpErrorKind::Cancelled => "Cancelled",
            HttpErrorKind::File => "File error",
            HttpErrorKind::Protocol => "Protocol error",
            HttpErrorKind::Other => "Error",
        }
    }
}

fn chain(err: &(dyn StdError + 'static)) -> Vec<String> {
    let mut out = vec![err.to_string()];
    let mut cur = err.source();
    while let Some(e) = cur {
        let s = e.to_string();
        if out.last() != Some(&s) {
            out.push(s);
        }
        cur = e.source();
    }
    out
}

fn find_io<'a>(err: &'a (dyn StdError + 'static)) -> Option<&'a std::io::Error> {
    let mut cur: Option<&'a (dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return Some(io);
        }
        cur = e.source();
    }
    None
}

fn find_rustls<'a>(err: &'a (dyn StdError + 'static)) -> Option<&'a rustls::Error> {
    let mut cur: Option<&'a (dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(r) = e.downcast_ref::<rustls::Error>() {
            return Some(r);
        }
        // rustls errors are often wrapped in io::Error::other.
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && let Some(inner) = io.get_ref()
            && let Some(r) = inner.downcast_ref::<rustls::Error>()
        {
            return Some(r);
        }
        cur = e.source();
    }
    None
}

/// Whether opening the connection failed because its first hop (the
/// server, or the proxy in between) could not be reached: refused,
/// unreachable network or host, or a failed DNS lookup. Nothing was sent.
pub(crate) fn is_unreachable(err: &reqwest::Error) -> bool {
    use std::io::ErrorKind as K;
    if !err.is_connect() || err.is_timeout() {
        return false;
    }
    let refused = find_io(err).is_some_and(|e| {
        matches!(e.kind(), K::ConnectionRefused | K::HostUnreachable | K::NetworkUnreachable | K::AddrNotAvailable)
    });
    refused || chain(err).iter().any(|m| m.to_lowercase().contains("dns error"))
}

/// Turns a reqwest error into a classified [`HttpError`]. `host` and
/// `timeout_ms` only improve the message.
pub fn classify(err: &reqwest::Error, host: &str, timeout_ms: Option<u128>) -> HttpError {
    let parts = chain(err);
    let detail = parts.join(": ");
    let lower = detail.to_lowercase();

    if err.is_timeout() || find_io(err).map(|e| e.kind() == std::io::ErrorKind::TimedOut).unwrap_or(false) {
        let msg = match timeout_ms {
            Some(ms) => format!("Request to {host} timed out after {ms} ms"),
            None => format!("Request to {host} timed out"),
        };
        return HttpError::new(HttpErrorKind::Timeout, msg).with_detail(detail);
    }
    if let Some(tls) = find_rustls(err) {
        return match tls {
            rustls::Error::InvalidCertificate(reason) => HttpError::new(
                HttpErrorKind::Certificate,
                format!("SSL certificate problem for {host}: {reason:?}. Disable \"Verify SSL\" in the request settings to skip verification."),
            )
            .with_detail(detail),
            other => HttpError::new(HttpErrorKind::Tls, format!("TLS handshake with {host} failed: {other}"))
                .with_detail(detail),
        };
    }
    if lower.contains("dns error")
        || lower.contains("failed to lookup address")
        || lower.contains("nodename nor servname")
    {
        return HttpError::new(HttpErrorKind::Dns, format!("Could not resolve host \"{host}\" (DNS lookup failed)"))
            .with_detail(detail);
    }
    if let Some(io) = find_io(err) {
        match io.kind() {
            std::io::ErrorKind::ConnectionRefused => {
                return HttpError::new(
                    HttpErrorKind::ConnectionRefused,
                    format!("Connection refused by {host}: is the server running and the port correct?"),
                )
                .with_detail(detail);
            }
            std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe => {
                return HttpError::new(HttpErrorKind::ConnectionReset, format!("Connection to {host} was reset"))
                    .with_detail(detail);
            }
            _ => {}
        }
    }
    if lower.contains("certificate") {
        return HttpError::new(HttpErrorKind::Certificate, format!("SSL certificate problem for {host}"))
            .with_detail(detail);
    }
    if lower.contains("proxy") {
        return HttpError::new(HttpErrorKind::Proxy, format!("Proxy error while connecting to {host}"))
            .with_detail(detail);
    }
    if err.is_connect() {
        return HttpError::new(HttpErrorKind::ConnectionRefused, format!("Could not connect to {host}"))
            .with_detail(detail);
    }
    if err.is_builder() {
        return HttpError::new(HttpErrorKind::InvalidUrl, parts.last().cloned().unwrap_or_default())
            .with_detail(detail);
    }
    let msg = parts.last().cloned().unwrap_or_else(|| "request failed".into());
    HttpError::new(HttpErrorKind::Protocol, msg).with_detail(detail)
}
