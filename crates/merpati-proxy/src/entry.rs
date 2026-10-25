//! Capture records.

use std::borrow::Cow;
use std::io::Read;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};

/// Upper bound for decoding a compressed body for display.
pub const DEFAULT_DECODE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// A request/response exchange (plain HTTP or decrypted HTTPS).
    Http,
    /// An upgraded WebSocket connection relayed through the proxy.
    WebSocket,
    /// An opaque CONNECT tunnel (not decrypted).
    Tunnel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryState {
    /// Request forwarded, waiting for the response headers (or for the tunnel
    /// to connect).
    Pending,
    /// Response headers received and the body is streaming; for tunnels and
    /// WebSockets: the connection is open.
    Receiving,
    Complete,
    Failed,
}

/// A body as seen on the wire, capped in memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapturedBody {
    /// The first `max_body_bytes` bytes, still content-encoded.
    pub data: Bytes,
    /// Total bytes that went through (encoded size).
    pub size: u64,
    /// `data` holds only a prefix of the body.
    pub truncated: bool,
}

impl CapturedBody {
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

/// A body decoded according to its `Content-Encoding`, for display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecodedBody {
    pub data: Bytes,
    /// The encoding that was removed (`gzip`, `br`, `deflate`, ...), if any.
    pub encoding: Option<String>,
    /// `data` is incomplete: the captured body was truncated or the decoded
    /// output hit the limit.
    pub truncated: bool,
    /// Why decoding failed (unsupported or corrupt encoding); `data` then
    /// holds the raw bytes.
    pub error: Option<String>,
}

impl DecodedBody {
    /// The body as text when it is valid UTF-8 (a cut-off last character of
    /// a truncated body is ignored).
    pub fn text(&self) -> Option<Cow<'_, str>> {
        match std::str::from_utf8(&self.data) {
            Ok(s) => Some(Cow::Borrowed(s)),
            Err(e) if self.truncated && e.error_len().is_none() => {
                Some(Cow::Borrowed(std::str::from_utf8(&self.data[..e.valid_up_to()]).unwrap_or_default()))
            }
            Err(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timings {
    /// DNS + TCP (+ upstream proxy CONNECT) + TLS. `None` on a reused connection.
    pub connect: Option<Duration>,
    /// The upstream TLS handshake part of `connect`.
    pub tls_handshake: Option<Duration>,
    /// From receiving the request to the upstream response headers.
    pub ttfb: Option<Duration>,
    /// From receiving the request to the end of the response body (tunnels:
    /// until the connection closed).
    pub total: Option<Duration>,
    pub reused_connection: bool,
}

/// Byte counts for tunnels and WebSocket connections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// Client to server.
    pub bytes_up: u64,
    /// Server to client.
    pub bytes_down: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WsOpcode {
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

/// One WebSocket message seen by the proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsMessage {
    pub from_client: bool,
    pub opcode: WsOpcode,
    /// Since the connection was upgraded.
    pub at: Duration,
    /// Payload bytes on the wire (compressed size for permessage-deflate).
    pub size: u64,
    /// The (decompressed) payload, capped.
    pub data: Bytes,
    pub truncated: bool,
    /// Sent with permessage-deflate.
    pub compressed: bool,
}

impl WsMessage {
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.data)
    }
}

/// One captured exchange, tunnel or WebSocket connection.
#[derive(Clone, Debug)]
pub struct CaptureEntry {
    pub id: u64,
    pub started_at: DateTime<Utc>,
    pub client_addr: SocketAddr,
    pub kind: EntryKind,
    pub state: EntryState,
    /// HTTPS that was decrypted by the proxy.
    pub tls_intercepted: bool,
    /// `http` or `https`; `tcp` for tunnels (whatever they carry).
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub method: String,
    /// Full URL; `host:port` for tunnels.
    pub url: String,
    /// Client to proxy (`HTTP/1.1`, `HTTP/2`).
    pub http_version: String,
    /// Proxy to server.
    pub upstream_http_version: Option<String>,
    /// As forwarded upstream (hop-by-hop and proxy headers removed).
    pub request_headers: Vec<(String, String)>,
    pub request_body: CapturedBody,
    pub status: Option<u16>,
    pub response_headers: Vec<(String, String)>,
    pub response_trailers: Vec<(String, String)>,
    pub response_body: CapturedBody,
    pub timings: Timings,
    /// Tunnels and WebSockets.
    pub stream: Option<StreamStats>,
    pub ws_messages: Vec<WsMessage>,
    /// WebSocket messages not kept because the per-entry budget was used up.
    pub ws_messages_dropped: u64,
    pub error: Option<String>,
    /// Extra information, e.g. why a CONNECT was tunnelled.
    pub note: Option<String>,
}

impl CaptureEntry {
    pub(crate) fn new(id: u64, client_addr: SocketAddr, kind: EntryKind) -> Self {
        CaptureEntry {
            id,
            started_at: Utc::now(),
            client_addr,
            kind,
            state: EntryState::Pending,
            tls_intercepted: false,
            scheme: String::new(),
            host: String::new(),
            port: 0,
            method: String::new(),
            url: String::new(),
            http_version: String::new(),
            upstream_http_version: None,
            request_headers: Vec::new(),
            request_body: CapturedBody::default(),
            status: None,
            response_headers: Vec::new(),
            response_trailers: Vec::new(),
            response_body: CapturedBody::default(),
            timings: Timings::default(),
            stream: None,
            ws_messages: Vec::new(),
            ws_messages_dropped: 0,
            error: None,
            note: None,
        }
    }

    pub fn request_header(&self, name: &str) -> Option<&str> {
        find_header(&self.request_headers, name)
    }

    pub fn response_header(&self, name: &str) -> Option<&str> {
        find_header(&self.response_headers, name)
    }

    /// Response `Content-Type`, or the request's when there is no response.
    pub fn content_type(&self) -> Option<&str> {
        if self.status.is_some() { self.response_header("content-type") } else { self.request_header("content-type") }
    }

    /// Canonical reason phrase for `status` (`OK`, `Not Found`).
    pub fn status_text(&self) -> &'static str {
        self.status.and_then(|s| http::StatusCode::from_u16(s).ok()).and_then(|s| s.canonical_reason()).unwrap_or("")
    }

    /// Path and query of `url` (`/` when empty).
    pub fn path(&self) -> &str {
        if self.kind == EntryKind::Tunnel {
            return "";
        }
        let rest = self.url.split_once("://").map(|(_, r)| r).unwrap_or(&self.url);
        match rest.find('/') {
            Some(i) => &rest[i..],
            None => "/",
        }
    }

    /// Bytes this entry keeps in memory (for the store budget).
    pub fn stored_bytes(&self) -> usize {
        self.request_body.data.len()
            + self.response_body.data.len()
            + self.ws_messages.iter().map(|m| m.data.len()).sum::<usize>()
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, EntryState::Complete | EntryState::Failed)
    }

    pub fn decoded_request_body(&self) -> DecodedBody {
        decode_body(&self.request_body, self.request_header("content-encoding"), DEFAULT_DECODE_LIMIT)
    }

    pub fn decoded_response_body(&self) -> DecodedBody {
        decode_body(&self.response_body, self.response_header("content-encoding"), DEFAULT_DECODE_LIMIT)
    }
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Removes `content_encoding` (comma separated, applied in order) from
/// `body`, producing at most `limit` bytes. Supports gzip, deflate (zlib or
/// raw) and br.
pub fn decode_body(body: &CapturedBody, content_encoding: Option<&str>, limit: usize) -> DecodedBody {
    let encodings: Vec<String> = content_encoding
        .unwrap_or("")
        .split(',')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e != "identity")
        .collect();
    if encodings.is_empty() || body.data.is_empty() {
        return DecodedBody { data: body.data.clone(), encoding: None, truncated: body.truncated, error: None };
    }
    let mut data = body.data.to_vec();
    let mut truncated = false;
    for enc in encodings.iter().rev() {
        match decode_one(enc, &data, limit, body.truncated) {
            Ok((out, cut)) => {
                data = out;
                truncated |= cut;
            }
            Err(e) => {
                return DecodedBody {
                    data: body.data.clone(),
                    encoding: None,
                    truncated: body.truncated,
                    error: Some(e),
                };
            }
        }
    }
    DecodedBody {
        data: Bytes::from(data),
        encoding: Some(encodings.join(", ")),
        truncated: truncated || body.truncated,
        error: None,
    }
}

fn decode_one(enc: &str, data: &[u8], limit: usize, input_truncated: bool) -> Result<(Vec<u8>, bool), String> {
    match enc {
        "gzip" | "x-gzip" => read_limited(flate2::read::MultiGzDecoder::new(data), limit, input_truncated),
        "deflate" => {
            // Servers disagree on whether "deflate" means zlib or raw deflate.
            let zlib = read_limited(flate2::read::ZlibDecoder::new(data), limit, input_truncated);
            match zlib {
                // Empty output counts only from a complete stream: on truncated
                // input a zlib header error also ends up as empty output.
                Ok(r) if !r.0.is_empty() || !r.1 => Ok(r),
                _ => read_limited(flate2::read::DeflateDecoder::new(data), limit, input_truncated),
            }
        }
        "br" => read_limited(brotli_decompressor::Decompressor::new(data, 8192), limit, input_truncated),
        other => Err(format!("unsupported content encoding {other:?}")),
    }
}

/// Reads up to `limit` bytes. A read error after some output is accepted
/// when the input was truncated (the stream simply ends early).
fn read_limited(mut r: impl Read, limit: usize, input_truncated: bool) -> Result<(Vec<u8>, bool), String> {
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok((out, false)),
            Ok(n) => {
                let room = limit.saturating_sub(out.len());
                if n > room {
                    out.extend_from_slice(&buf[..room]);
                    return Ok((out, true));
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) if input_truncated => return Ok((out, true)),
            Err(e) => return Err(format!("could not decode body: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn body(data: Vec<u8>, truncated: bool) -> CapturedBody {
        CapturedBody { size: data.len() as u64, data: Bytes::from(data), truncated }
    }

    #[test]
    fn decodes_gzip_deflate_and_chains() {
        let text = b"hello hello hello hello".repeat(100);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&text).unwrap();
        let gz = gz.finish().unwrap();
        let d = decode_body(&body(gz.clone(), false), Some("gzip"), 1 << 20);
        assert_eq!(d.data.as_ref(), text.as_slice());
        assert_eq!(d.encoding.as_deref(), Some("gzip"));
        assert!(!d.truncated);

        let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        raw.write_all(&text).unwrap();
        let d = decode_body(&body(raw.finish().unwrap(), false), Some("deflate"), 1 << 20);
        assert_eq!(d.data.as_ref(), text.as_slice());

        // Limit and truncated input.
        let d = decode_body(&body(gz.clone(), false), Some("gzip"), 10);
        assert_eq!(d.data.len(), 10);
        assert!(d.truncated);
        let d = decode_body(&body(gz[..gz.len() / 2].to_vec(), true), Some("gzip"), 1 << 20);
        assert!(d.truncated && d.error.is_none());
        assert!(text.starts_with(&d.data));

        let d = decode_body(&body(b"abc".to_vec(), false), Some("zstd"), 1 << 20);
        assert!(d.error.is_some());
        assert_eq!(d.data.as_ref(), b"abc");
    }

    #[test]
    fn deflate_of_an_empty_body() {
        // A complete zlib stream that decodes to nothing is not an error.
        let zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default()).finish().unwrap();
        let d = decode_body(&body(zlib, false), Some("deflate"), 1 << 20);
        assert_eq!((d.data.as_ref(), d.error.as_deref(), d.truncated), (&b""[..], None, false));
        let raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default()).finish().unwrap();
        let d = decode_body(&body(raw, false), Some("deflate"), 1 << 20);
        assert_eq!((d.data.as_ref(), d.error.as_deref()), (&b""[..], None));
    }

    #[test]
    fn text_of_truncated_utf8() {
        let d = DecodedBody {
            data: Bytes::from_static("héllo".as_bytes()[..2].as_ref()),
            truncated: true,
            ..Default::default()
        };
        assert_eq!(d.text().as_deref(), Some("h"));
    }
}
