//! HTTP response representation shared by the HTTP engine, scripts, the
//! runner and the UI. Large bodies live in a temporary file so memory use stays
//! flat regardless of response size.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Response body, either in memory or spilled to a temporary file that is
/// deleted when the last clone is dropped.
#[derive(Clone, Debug)]
pub enum ResponseBody {
    Memory(Bytes),
    File { path: Arc<tempfile::TempPath>, len: u64 },
}

impl Default for ResponseBody {
    fn default() -> Self {
        ResponseBody::Memory(Bytes::new())
    }
}

impl ResponseBody {
    pub fn len(&self) -> u64 {
        match self {
            ResponseBody::Memory(b) => b.len() as u64,
            ResponseBody::File { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_file(&self) -> bool {
        matches!(self, ResponseBody::File { .. })
    }

    pub fn file_path(&self) -> Option<&Path> {
        match self {
            ResponseBody::File { path, .. } => Some(path.as_ref()),
            ResponseBody::Memory(_) => None,
        }
    }

    /// Reads the whole body. For file-backed bodies this allocates the full
    /// size, so callers that only display should use [`read_range`].
    pub fn read_all(&self) -> std::io::Result<Bytes> {
        match self {
            ResponseBody::Memory(b) => Ok(b.clone()),
            ResponseBody::File { path, .. } => std::fs::read(path.as_ref() as &Path).map(Bytes::from),
        }
    }

    /// Up to `len` bytes starting at `start`.
    pub fn read_range(&self, start: u64, len: usize) -> std::io::Result<Vec<u8>> {
        match self {
            ResponseBody::Memory(b) => {
                let s = (start as usize).min(b.len());
                let e = s.saturating_add(len).min(b.len());
                Ok(b[s..e].to_vec())
            }
            ResponseBody::File { path, .. } => {
                let mut f = std::fs::File::open(path.as_ref() as &Path)?;
                f.seek(SeekFrom::Start(start))?;
                let mut buf = Vec::with_capacity(len.min(1 << 20));
                f.take(len as u64).read_to_end(&mut buf)?;
                Ok(buf)
            }
        }
    }

    /// The body as text, lossily decoded. Convenience for small bodies.
    pub fn text_lossy(&self) -> String {
        self.read_all().map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
    }

    pub fn save_to(&self, dest: &Path) -> std::io::Result<()> {
        match self {
            ResponseBody::Memory(b) => std::fs::write(dest, b),
            ResponseBody::File { path, .. } => std::fs::copy(path.as_ref() as &Path, dest).map(|_| ()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// Time until response headers were received.
    pub ttfb_ms: f64,
    /// Time until the body was fully received.
    pub total_ms: f64,
}

#[derive(Clone, Debug, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub status_text: String,
    /// `HTTP/1.1`, `HTTP/2`, ...
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
    /// Final URL after redirects.
    pub url: String,
    pub timings: Timings,
    /// Approximate size of the status line and headers on the wire.
    pub header_size: u64,
    pub remote_addr: Option<String>,
    /// URLs visited before the final one.
    pub redirects: Vec<String>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn headers_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn content_type(&self) -> &str {
        self.header("content-type").unwrap_or("")
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn total_size(&self) -> u64 {
        self.header_size + self.body.len()
    }

    /// Best guess of the body language for syntax highlighting.
    pub fn language(&self) -> BodyLanguage {
        BodyLanguage::detect(self.content_type(), &self.body.read_range(0, 64).unwrap_or_default())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyLanguage {
    Json,
    Xml,
    Html,
    JavaScript,
    Css,
    Text,
    Image,
    Binary,
}

impl BodyLanguage {
    pub fn detect(content_type: &str, head: &[u8]) -> BodyLanguage {
        let ct = content_type.to_ascii_lowercase();
        if ct.contains("json") {
            return BodyLanguage::Json;
        }
        if ct.contains("html") {
            return BodyLanguage::Html;
        }
        if ct.contains("xml") {
            return BodyLanguage::Xml;
        }
        if ct.contains("javascript") || ct.contains("ecmascript") {
            return BodyLanguage::JavaScript;
        }
        if ct.contains("css") {
            return BodyLanguage::Css;
        }
        if ct.starts_with("image/") {
            return BodyLanguage::Image;
        }
        if ct.starts_with("text/") {
            return BodyLanguage::Text;
        }
        let trimmed: Vec<u8> = head.iter().copied().skip_while(|b| b.is_ascii_whitespace()).take(16).collect();
        match trimmed.first() {
            Some(b'{') | Some(b'[') => BodyLanguage::Json,
            Some(b'<') => {
                let lower = String::from_utf8_lossy(&trimmed).to_ascii_lowercase();
                if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
                    BodyLanguage::Html
                } else {
                    BodyLanguage::Xml
                }
            }
            _ if std::str::from_utf8(head).is_ok() || head.is_empty() => BodyLanguage::Text,
            _ => BodyLanguage::Binary,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BodyLanguage::Json => "json",
            BodyLanguage::Xml => "xml",
            BodyLanguage::Html => "html",
            BodyLanguage::JavaScript => "javascript",
            BodyLanguage::Css => "css",
            BodyLanguage::Text => "text",
            BodyLanguage::Image => "image",
            BodyLanguage::Binary => "binary",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_language() {
        assert_eq!(BodyLanguage::detect("application/json; charset=utf-8", b""), BodyLanguage::Json);
        assert_eq!(BodyLanguage::detect("", b"  {\"a\":1}"), BodyLanguage::Json);
        assert_eq!(BodyLanguage::detect("", b"<!DOCTYPE html><html>"), BodyLanguage::Html);
        assert_eq!(BodyLanguage::detect("", b"<?xml version"), BodyLanguage::Xml);
        assert_eq!(BodyLanguage::detect("", &[0xff, 0xfe, 0x00]), BodyLanguage::Binary);
    }

    #[test]
    fn file_body_ranges() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"0123456789").unwrap();
        let body = ResponseBody::File { path: Arc::new(f.into_temp_path()), len: 10 };
        assert_eq!(body.read_range(3, 4).unwrap(), b"3456");
        assert_eq!(body.read_range(8, 10).unwrap(), b"89");
        assert_eq!(body.read_all().unwrap().len(), 10);
    }
}
