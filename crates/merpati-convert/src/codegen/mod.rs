//! Code snippets from a prepared request (FR-R10).
//!
//! Generators take a fully resolved [`PreparedRequest`] (variables resolved,
//! auth already applied) so the snippet reproduces exactly what Merpati sends.
//! Output is deterministic: the same request always yields the same text.

mod langs;

use std::path::Path;

use merpati_core::prepared::{MultipartPart, PreparedBody, PreparedRequest, ResolvedSettings};
use merpati_core::settings::RequestDefaults;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Curl,
    Http,
    Httpie,
    Wget,
    JavaScriptFetch,
    NodeAxios,
    Python,
    Go,
    Rust,
    Java,
    JavaHttpClient,
    Kotlin,
    CSharp,
    Php,
    Swift,
    Ruby,
    Dart,
    PowerShell,
}

impl Language {
    pub const ALL: &'static [Language] = &[
        Language::Curl,
        Language::Http,
        Language::Httpie,
        Language::Wget,
        Language::JavaScriptFetch,
        Language::NodeAxios,
        Language::Python,
        Language::Go,
        Language::Rust,
        Language::Java,
        Language::JavaHttpClient,
        Language::Kotlin,
        Language::CSharp,
        Language::Php,
        Language::Swift,
        Language::Ruby,
        Language::Dart,
        Language::PowerShell,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Language::Curl => "cURL",
            Language::Http => "HTTP",
            Language::Httpie => "HTTPie",
            Language::Wget => "Wget",
            Language::JavaScriptFetch => "JavaScript - Fetch",
            Language::NodeAxios => "NodeJs - Axios",
            Language::Python => "Python - Requests",
            Language::Go => "Go - Native",
            Language::Rust => "Rust - reqwest",
            Language::Java => "Java - OkHttp",
            Language::JavaHttpClient => "Java - HttpClient",
            Language::Kotlin => "Kotlin - OkHttp",
            Language::CSharp => "C# - HttpClient",
            Language::Php => "PHP - cURL",
            Language::Swift => "Swift - URLSession",
            Language::Ruby => "Ruby - Net::HTTP",
            Language::Dart => "Dart - http",
            Language::PowerShell => "PowerShell - RestMethod",
        }
    }

    /// Hint for the syntax highlighter (`bash`, `javascript`, ...).
    pub fn syntax(self) -> &'static str {
        match self {
            Language::Curl | Language::Httpie | Language::Wget => "bash",
            Language::Http => "http",
            Language::JavaScriptFetch | Language::NodeAxios => "javascript",
            Language::Python => "python",
            Language::Go => "go",
            Language::Rust => "rust",
            Language::Java | Language::JavaHttpClient => "java",
            Language::Kotlin => "kotlin",
            Language::CSharp => "csharp",
            Language::Php => "php",
            Language::Swift => "swift",
            Language::Ruby => "ruby",
            Language::Dart => "dart",
            Language::PowerShell => "powershell",
        }
    }

    /// Stable identifier for settings and the CLI (`curl`, `python`, ...).
    pub fn id(self) -> &'static str {
        match self {
            Language::Curl => "curl",
            Language::Http => "http",
            Language::Httpie => "httpie",
            Language::Wget => "wget",
            Language::JavaScriptFetch => "js-fetch",
            Language::NodeAxios => "node-axios",
            Language::Python => "python",
            Language::Go => "go",
            Language::Rust => "rust",
            Language::Java => "java-okhttp",
            Language::JavaHttpClient => "java-httpclient",
            Language::Kotlin => "kotlin",
            Language::CSharp => "csharp",
            Language::Php => "php",
            Language::Swift => "swift",
            Language::Ruby => "ruby",
            Language::Dart => "dart",
            Language::PowerShell => "powershell",
        }
    }

    pub fn from_id(id: &str) -> Option<Language> {
        Language::ALL.iter().copied().find(|l| l.id().eq_ignore_ascii_case(id))
    }
}

/// Generation options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Leave out the headers Merpati adds to every request (`User-Agent:
    /// MerpatiRuntime/...`, `Accept: */*`), like Postman's snippets do.
    pub skip_default_headers: bool,
}

/// Generates a snippet including every header of the prepared request.
pub fn generate(lang: Language, req: &PreparedRequest) -> String {
    generate_with(lang, req, Options::default())
}

pub fn generate_with(lang: Language, req: &PreparedRequest, opts: Options) -> String {
    let view = View::new(req, opts);
    let mut out = match lang {
        Language::Curl => langs::curl(&view),
        Language::Http => langs::http(&view),
        Language::Httpie => langs::httpie(&view),
        Language::Wget => langs::wget(&view),
        Language::JavaScriptFetch => langs::js_fetch(&view),
        Language::NodeAxios => langs::node_axios(&view),
        Language::Python => langs::python(&view),
        Language::Go => langs::go(&view),
        Language::Rust => langs::rust(&view),
        Language::Java => langs::java_okhttp(&view),
        Language::JavaHttpClient => langs::java_httpclient(&view),
        Language::Kotlin => langs::kotlin(&view),
        Language::CSharp => langs::csharp(&view),
        Language::Php => langs::php(&view),
        Language::Swift => langs::swift(&view),
        Language::Ruby => langs::ruby(&view),
        Language::Dart => langs::dart(&view),
        Language::PowerShell => langs::powershell(&view),
    };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub(crate) const BOUNDARY: &str = "----MerpatiFormBoundary7MA4YWxkTrZu0gW";

pub(crate) enum BodyView<'a> {
    None,
    Text(String),
    File(&'a Path),
    Multipart(&'a [MultipartPart]),
}

/// A request reduced to what generators need.
pub(crate) struct View<'a> {
    pub method: String,
    pub url: &'a str,
    /// Headers to emit (Content-Length removed; Content-Type removed for
    /// multipart because clients set the boundary themselves).
    pub headers: Vec<(&'a str, &'a str)>,
    pub body: BodyView<'a>,
    pub settings: &'a ResolvedSettings,
}

impl<'a> View<'a> {
    fn new(req: &'a PreparedRequest, opts: Options) -> Self {
        let body = match &req.body {
            PreparedBody::None => BodyView::None,
            PreparedBody::Bytes(b) if b.is_empty() => BodyView::None,
            PreparedBody::Bytes(b) => BodyView::Text(String::from_utf8_lossy(b).into_owned()),
            PreparedBody::File(p) => BodyView::File(p),
            PreparedBody::Multipart(parts) => BodyView::Multipart(parts),
        };
        let multipart = matches!(body, BodyView::Multipart(_));
        let defaults = RequestDefaults::default().default_headers();
        let headers = req
            .headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("content-length"))
            .filter(|(k, _)| !(multipart && k.eq_ignore_ascii_case("content-type")))
            .filter(|(k, v)| {
                !(opts.skip_default_headers && defaults.iter().any(|(dk, dv)| dk.eq_ignore_ascii_case(k) && dv == v))
            })
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        View { method: req.method.to_uppercase(), url: &req.url, headers, body, settings: &req.settings }
    }

    pub fn content_type(&self) -> Option<&'a str> {
        self.headers.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).map(|(_, v)| *v)
    }

    pub fn has_body(&self) -> bool {
        !matches!(self.body, BodyView::None)
    }

    /// Headers with duplicate names merged (`a, b`), for languages that use
    /// a map. Order follows first appearance.
    pub fn merged_headers(&self) -> Vec<(&'a str, String)> {
        let mut out: Vec<(&'a str, String)> = Vec::new();
        for (k, v) in &self.headers {
            match out.iter_mut().find(|(ek, _)| ek.eq_ignore_ascii_case(k)) {
                Some((_, ev)) => {
                    ev.push_str(", ");
                    ev.push_str(v);
                }
                None => out.push((k, v.to_string())),
            }
        }
        out
    }

    /// Body text parsed as JSON when the content type says JSON. Used by the
    /// JavaScript generators, so `None` when a number would lose precision
    /// as a JS double (integers beyond 2^53).
    pub fn json_body(&self) -> Option<serde_json::Value> {
        match &self.body {
            BodyView::Text(t)
                if self.content_type().map(|c| c.to_ascii_lowercase().contains("json")).unwrap_or(false) =>
            {
                serde_json::from_str::<serde_json::Value>(t)
                    .ok()
                    .filter(|v| (v.is_object() || v.is_array()) && exact_in_js(v))
            }
            _ => None,
        }
    }

    /// Url-encoded pairs when the body is a form.
    pub fn form_pairs(&self) -> Option<Vec<(String, String)>> {
        match &self.body {
            BodyView::Text(t)
                if self
                    .content_type()
                    .map(|c| c.to_ascii_lowercase().contains("application/x-www-form-urlencoded"))
                    .unwrap_or(false) =>
            {
                Some(url::form_urlencoded::parse(t.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
            }
            _ => None,
        }
    }

    pub fn timeout_ms(&self) -> Option<u128> {
        self.settings.timeout.map(|d| d.as_millis())
    }
}

/// True when every number of `v` survives a round trip through a JS double.
fn exact_in_js(v: &serde_json::Value) -> bool {
    const MAX_SAFE: u64 = (1 << 53) - 1;
    match v {
        serde_json::Value::Number(n) => match (n.as_u64(), n.as_i64()) {
            (Some(u), _) => u <= MAX_SAFE,
            (_, Some(i)) => i.unsigned_abs() <= MAX_SAFE,
            _ => true,
        },
        serde_json::Value::Array(items) => items.iter().all(exact_in_js),
        serde_json::Value::Object(map) => map.values().all(exact_in_js),
        _ => true,
    }
}

pub(crate) fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned())
}

pub(crate) fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// How a double-quoted string literal writes non-printable characters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unicode {
    /// `\u0001`
    Json,
    /// `\u{1}`
    Braces,
}

/// A double-quoted string literal with C-style escapes.
pub(crate) fn dq(s: &str, unicode: Unicode, escape_dollar: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' if escape_dollar => out.push_str("\\$"),
            // U+0085/U+2028/U+2029 end a line in C# (and older JS).
            c if (c as u32) < 0x20 || c as u32 == 0x7f || matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}') => {
                match unicode {
                    Unicode::Json => out.push_str(&format!("\\u{:04x}", c as u32)),
                    Unicode::Braces => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// POSIX shell single-quoted word.
pub(crate) fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A shell word: bare when it is a plain token (`POST`), quoted otherwise.
pub(crate) fn sh_word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        s.to_string()
    } else {
        sh(s)
    }
}

/// Single-quoted literal where `\` and `'` are escaped with a backslash (PHP, Ruby).
pub(crate) fn sq_backslash(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// PowerShell single-quoted literal. PowerShell also treats the typographic
/// single quotes U+2018..U+201B as quotes, so they are doubled too.
pub(crate) fn ps(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Manual multipart body as text (for formats without a multipart API).
/// File contents are represented by a placeholder line.
pub(crate) fn multipart_text(parts: &[MultipartPart]) -> String {
    use merpati_core::prepared::MultipartValue;
    // Quoted names as browsers (and curl) write them: `"`, CR and LF are
    // percent-encoded.
    let quoted = |s: &str| s.replace('"', "%22").replace('\r', "%0D").replace('\n', "%0A");
    let mut s = String::new();
    for p in parts {
        s.push_str(&format!("--{BOUNDARY}\r\n"));
        match &p.value {
            MultipartValue::Text(v) => {
                s.push_str(&format!("Content-Disposition: form-data; name=\"{}\"\r\n", quoted(&p.name)));
                if let Some(ct) = &p.content_type {
                    s.push_str(&format!("Content-Type: {ct}\r\n"));
                }
                s.push_str("\r\n");
                s.push_str(v);
                s.push_str("\r\n");
            }
            MultipartValue::File(path) => {
                s.push_str(&format!(
                    "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                    quoted(&p.name),
                    quoted(&file_name(path))
                ));
                s.push_str(&format!(
                    "Content-Type: {}\r\n\r\n",
                    p.content_type.as_deref().unwrap_or("application/octet-stream")
                ));
                s.push_str(&format!("<contents of {}>\r\n", path_str(path)));
            }
        }
    }
    s.push_str(&format!("--{BOUNDARY}--\r\n"));
    s
}

#[cfg(test)]
mod tests;
