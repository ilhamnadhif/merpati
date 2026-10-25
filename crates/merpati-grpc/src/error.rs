//! Error type and gRPC status code names.

/// Every failure of this crate, with a human-readable message suitable for
/// showing in the UI as-is.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GrpcError {
    #[error("Invalid gRPC URL \"{url}\": {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("Could not load .proto files: {0}")]
    Proto(String),
    #[error("Invalid descriptor set: {0}")]
    Descriptor(String),
    #[error("Unknown service \"{0}\"")]
    UnknownService(String),
    #[error("Unknown method \"{method}\" in service \"{service}\"")]
    UnknownMethod { service: String, method: String },
    #[error("Unknown message type \"{0}\"")]
    UnknownMessage(String),
    #[error("Bad JSON for type {type_name}: {message}")]
    InvalidJson { type_name: String, message: String },
    #[error("Invalid metadata \"{key}\": {reason}")]
    InvalidMetadata { key: String, reason: String },
    /// TCP connect, DNS or HTTP/2 handshake failure.
    #[error("{0}")]
    Connect(String),
    #[error("TLS error: {0}")]
    Tls(String),
    #[error("{0}")]
    Timeout(String),
    /// The server (or the transport) ended the call with a non-OK status.
    #[error("{code_name} ({code}): {message}")]
    Status { code: i32, code_name: String, message: String, metadata: Vec<(String, String)> },
    #[error("Server reflection failed: {0}")]
    Reflection(String),
    /// `send` on a unary / server-streaming call: only one request message is allowed.
    #[error("Method {0} takes a single request message; more cannot be sent")]
    NotClientStreaming(String),
    /// `send` after `end_stream`, `cancel` or after the call finished.
    #[error("The request stream is closed")]
    StreamClosed,
    #[error("Method {method} is {kind}, not unary")]
    NotUnary { method: String, kind: String },
    #[error("{0}")]
    Other(String),
}

impl GrpcError {
    /// The gRPC status code this error corresponds to (`UNAVAILABLE` for
    /// connection problems, `INVALID_ARGUMENT` for bad input, ...).
    pub fn code(&self) -> i32 {
        match self {
            GrpcError::Status { code, .. } => *code,
            GrpcError::Connect(_) | GrpcError::Tls(_) => 14,
            GrpcError::Timeout(_) => 4,
            GrpcError::UnknownService(_) | GrpcError::UnknownMethod { .. } | GrpcError::NotUnary { .. } => 12,
            GrpcError::InvalidJson { .. } | GrpcError::InvalidMetadata { .. } | GrpcError::InvalidUrl { .. } => 3,
            GrpcError::StreamClosed | GrpcError::NotClientStreaming(_) => 9,
            _ => 2,
        }
    }
}

/// Canonical name of a gRPC status code (`5` -> `NOT_FOUND`).
pub fn status_code_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "CANCELLED",
        2 => "UNKNOWN",
        3 => "INVALID_ARGUMENT",
        4 => "DEADLINE_EXCEEDED",
        5 => "NOT_FOUND",
        6 => "ALREADY_EXISTS",
        7 => "PERMISSION_DENIED",
        8 => "RESOURCE_EXHAUSTED",
        9 => "FAILED_PRECONDITION",
        10 => "ABORTED",
        11 => "OUT_OF_RANGE",
        12 => "UNIMPLEMENTED",
        13 => "INTERNAL",
        14 => "UNAVAILABLE",
        15 => "DATA_LOSS",
        16 => "UNAUTHENTICATED",
        _ => "UNKNOWN",
    }
}

/// Builds a [`GrpcError::Status`] from a code and message.
pub(crate) fn status_error(code: i32, message: impl Into<String>, metadata: Vec<(String, String)>) -> GrpcError {
    GrpcError::Status { code, code_name: status_code_name(code).to_string(), message: message.into(), metadata }
}

/// Describes a transport-level failure (a status that carries a source
/// error, e.g. the connection dropped): the root cause, plus a hint for the
/// usual plaintext-vs-TLS mix-up.
pub(crate) fn transport_failure(src: &(dyn std::error::Error + 'static), tls: bool) -> String {
    let mut root = src;
    while let Some(next) = root.source() {
        root = next;
    }
    let cause = root.to_string();
    let mut msg = format!("Transport error: {cause}");
    let suspicious = ["connection closed", "frame", "connection reset", "broken pipe", "invalid"];
    if !tls && suspicious.iter().any(|s| cause.contains(s)) {
        msg.push_str(" (is this a gRPC server? If it uses TLS, use grpcs://)");
    }
    msg
}

/// Joins an error and its sources into one line, skipping repeated parts
/// (hyper/tonic errors often repeat their source in their own message).
pub(crate) fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        let s = e.to_string();
        if !s.is_empty() && !parts.iter().any(|p| p.contains(&s)) {
            parts.push(s);
        }
        cur = e.source();
    }
    parts.join(": ")
}
