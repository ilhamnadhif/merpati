//! Small helpers shared by the schemes.

use hmac::{Hmac, Mac};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rand::Rng;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use merpati_core::PreparedRequest;

use crate::error::AuthError;

pub(crate) const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
pub(crate) const B64_URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// RFC 3986 unreserved characters are left alone, everything else escaped.
pub(crate) const RFC3986: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

pub(crate) fn rfc3986(s: &str) -> String {
    utf8_percent_encode(s, RFC3986).to_string()
}

pub(crate) fn parse_url(req: &PreparedRequest) -> Result<url::Url, AuthError> {
    url::Url::parse(&req.url).map_err(|_| AuthError::Url(req.url.clone()))
}

/// `path?query` as sent on the wire.
pub(crate) fn request_target(url: &url::Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    }
}

/// `host[:port]`, omitting the port when it is the scheme default.
pub(crate) fn host_header(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    }
}

pub(crate) fn body_bytes(req: &PreparedRequest) -> Option<&[u8]> {
    req.body.as_bytes()
}

pub(crate) fn is_form_body(req: &PreparedRequest) -> bool {
    req.header("content-type")
        .map(|c| c.to_ascii_lowercase().starts_with("application/x-www-form-urlencoded"))
        .unwrap_or(false)
        && matches!(req.body, merpati_core::PreparedBody::Bytes(_) | merpati_core::PreparedBody::None)
}

pub(crate) fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

pub(crate) fn random_alnum(n: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..n).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
}

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut rng = rand::rng();
    (0..bytes).map(|_| format!("{:02x}", rng.random::<u8>())).collect()
}

pub(crate) fn hmac_sha1(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha1>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

pub(crate) fn hmac_sha512(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha512>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

pub(crate) fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

/// Splits one DER element (single-byte tag, definite length) off `b`:
/// `(tag, contents, rest)`. Enough to pick a field out of a certificate or
/// key without an ASN.1 library.
pub(crate) fn der_tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, b) = b.split_first()?;
    let (&first, b) = b.split_first()?;
    let (len, b) = if first < 0x80 {
        (first as usize, b)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let (len, b) = b.split_at_checked(n)?;
        (len.iter().fold(0usize, |acc, &x| acc << 8 | x as usize), b)
    };
    let (contents, rest) = b.split_at_checked(len)?;
    Some((tag, contents, rest))
}

/// Reads a string parameter, treating whitespace-only as empty.
pub(crate) fn param<'a>(auth: &'a merpati_core::Auth, key: &str) -> &'a str {
    auth.get(key).trim()
}

/// Parses an optional JSON object parameter (`payload`, `header`, `claims`).
pub(crate) fn json_object_param(
    auth: &merpati_core::Auth,
    key: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, AuthError> {
    match auth.params.get(key) {
        None | Some(serde_json::Value::Null) => Ok(Default::default()),
        Some(serde_json::Value::Object(o)) => Ok(o.clone()),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Ok(Default::default()),
        Some(serde_json::Value::String(s)) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(serde_json::Value::Object(o)) => Ok(o),
            Ok(_) => Err(AuthError::invalid(format!("`{key}` must be a JSON object"))),
            Err(e) => Err(AuthError::invalid(format!("`{key}` is not valid JSON: {e}"))),
        },
        Some(_) => Err(AuthError::invalid(format!("`{key}` must be a JSON object"))),
    }
}
