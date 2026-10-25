//! cURL command import.
//!
//! Understands POSIX shell quoting (single, double and `$'...'` ANSI-C quotes,
//! backslash escapes, `\` line continuations) as produced by browsers'
//! "Copy as cURL (bash)", and Windows cmd quoting (`^` escapes and
//! continuations) as produced by "Copy as cURL (cmd)".
//!
//! File contents curl reads when the command runs cannot be stored in
//! Merpati's body model, except as a whole binary body (`-d @file` or
//! `--data-binary @file` on its own) or a file upload (`-F name=@file`). The
//! other forms (`-d @file` next to other data, `--data-urlencode name@file`,
//! `--url-query name@file`, `-F name=<file`) keep a placeholder (`@file`,
//! `<file`) where the content goes, and [`parse_with_warnings`] says which
//! values to replace.

use merpati_core::model::{Body, BodyMode, FormField, HttpRequest, HttpVersion, KeyValue, RawLanguage};
use merpati_core::{Auth, AuthType};

use crate::ConvertError;

/// True when the text starts with a `curl` invocation (optionally after a
/// `$ ` prompt).
pub fn looks_like_curl(text: &str) -> bool {
    let t = text.trim_start();
    let t = t.strip_prefix('$').map(str::trim_start).unwrap_or(t);
    let word: String = t.chars().take_while(|c| !c.is_whitespace()).collect();
    word == "curl" || word == "curl.exe"
}

/// Windows cmd syntax: a `^` line continuation, or a word starting with `^"`
/// (how "Copy as cURL (cmd)" quotes every argument), outside POSIX quotes so
/// that `-d 'a=^"b"'` stays a POSIX command.
fn is_cmd_style(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let mut word_start = true;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => i += 1,
            '\'' | '"' => {
                // Backslash escapes only work in "..." and $'...'.
                let escapes = c == '"' || (i > 0 && chars[i - 1] == '$');
                let mut j = i + 1;
                while j < chars.len() && chars[j] != c {
                    j += if escapes && chars[j] == '\\' { 2 } else { 1 };
                }
                if j >= chars.len() {
                    // Unbalanced for POSIX: only cmd can make sense of it.
                    return cmd.contains("^\n") || cmd.contains("^\r\n") || cmd.contains("^\"");
                }
                i = j;
            }
            '^' => match chars.get(i + 1) {
                Some('\n' | '\r') => return true,
                Some('"') if word_start => return true,
                _ => {}
            },
            _ => {}
        }
        word_start = c.is_whitespace();
        i += 1;
    }
    false
}

/// Splits a POSIX shell command line into words. Stops at an unquoted pipe,
/// `;`, `&&` or redirection.
fn split_posix(cmd: &str) -> Result<Vec<String>, ConvertError> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                match chars.get(i + 1) {
                    Some('\n') => {}
                    Some('\r') if chars.get(i + 2) == Some(&'\n') => i += 1,
                    Some(&n) => {
                        cur.push(n);
                        in_word = true;
                    }
                    None => {}
                }
                i += 2;
                continue;
            }
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    cur.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(ConvertError::Parse("unterminated single quote".into()));
                }
                i += 1;
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'\'') => {
                in_word = true;
                i += 2;
                // `\xHH` and octal escapes are bytes; a run of them is UTF-8.
                let mut bytes = Vec::new();
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return Err(ConvertError::Parse("unterminated $'...' quote".into()));
                    };
                    if ch == '\\'
                        && let Some((b, used)) = ansi_c_byte(&chars[i + 1..])
                    {
                        bytes.push(b);
                        i += 1 + used;
                        continue;
                    }
                    for chunk in bytes.utf8_chunks() {
                        cur.push_str(chunk.valid());
                        cur.extend(chunk.invalid().iter().map(|&b| char::from(b)));
                    }
                    bytes.clear();
                    if ch == '\'' {
                        i += 1;
                        break;
                    }
                    if ch == '\\' {
                        let (decoded, used) = ansi_c_escape(&chars[i + 1..]);
                        cur.push_str(&decoded);
                        i += 1 + used;
                        continue;
                    }
                    cur.push(ch);
                    i += 1;
                }
                continue;
            }
            '"' => {
                in_word = true;
                i += 1;
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return Err(ConvertError::Parse("unterminated double quote".into()));
                    };
                    match ch {
                        '"' => {
                            i += 1;
                            break;
                        }
                        '\\' => match chars.get(i + 1) {
                            Some(&n @ ('"' | '\\' | '$' | '`')) => {
                                cur.push(n);
                                i += 2;
                            }
                            Some('\n') => i += 2,
                            _ => {
                                cur.push('\\');
                                i += 1;
                            }
                        },
                        _ => {
                            cur.push(ch);
                            i += 1;
                        }
                    }
                }
                continue;
            }
            '|' | ';' | '>' | '<' if !in_word || cur.is_empty() => break,
            '&' if chars.get(i + 1) == Some(&'&') && (!in_word || cur.is_empty()) => break,
            '#' if !in_word => {
                // Comment until end of line.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
        i += 1;
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// A `\xHH` or octal ANSI-C escape (the text after the backslash): the byte
/// and how many chars were consumed.
fn ansi_c_byte(rest: &[char]) -> Option<(u8, usize)> {
    let (radix, digits): (u32, String) = match rest.first()? {
        'x' => (16, rest[1..].iter().take(2).take_while(|c| c.is_ascii_hexdigit()).collect()),
        '0'..='7' => (8, rest.iter().take(3).take_while(|c| ('0'..='7').contains(*c)).collect()),
        _ => return None,
    };
    let v = u32::from_str_radix(&digits, radix).ok()?;
    let used = if radix == 16 { 1 + digits.len() } else { digits.len() };
    // Like bash, octal values above \377 keep their low byte.
    Some(((v & 0xff) as u8, used))
}

/// Decodes one ANSI-C escape (the text after the backslash). Returns the
/// decoded text and how many chars were consumed.
fn ansi_c_escape(rest: &[char]) -> (String, usize) {
    let Some(&c) = rest.first() else { return ("\\".into(), 0) };
    let simple = match c {
        'n' => Some('\n'),
        't' => Some('\t'),
        'r' => Some('\r'),
        'a' => Some('\u{7}'),
        'b' => Some('\u{8}'),
        'e' | 'E' => Some('\u{1b}'),
        'f' => Some('\u{c}'),
        'v' => Some('\u{b}'),
        '\\' => Some('\\'),
        '\'' => Some('\''),
        '"' => Some('"'),
        '?' => Some('?'),
        _ => None,
    };
    if let Some(ch) = simple {
        return (ch.to_string(), 1);
    }
    let hex = |max: usize| -> (u32, usize) {
        let digits: String = rest[1..].iter().take(max).take_while(|c| c.is_ascii_hexdigit()).collect();
        (u32::from_str_radix(&digits, 16).unwrap_or(0), digits.len())
    };
    match c {
        'x' => {
            let (v, n) = hex(2);
            if n == 0 {
                return ("\\x".into(), 1);
            }
            // Bytes >= 0x80 are part of UTF-8 sequences in practice; collect
            // them as Latin-1 only when standalone.
            (char::from_u32(v).map(String::from).unwrap_or_default(), 1 + n)
        }
        'u' | 'U' => {
            let (v, n) = hex(if c == 'u' { 4 } else { 8 });
            if n == 0 {
                return (format!("\\{c}"), 1);
            }
            (char::from_u32(v).map(String::from).unwrap_or_default(), 1 + n)
        }
        '0'..='7' => {
            let digits: String = rest.iter().take(3).take_while(|c| ('0'..='7').contains(*c)).collect();
            let v = u32::from_str_radix(&digits, 8).unwrap_or(0);
            (char::from_u32(v).map(String::from).unwrap_or_default(), digits.len())
        }
        other => (format!("\\{other}"), 1),
    }
}

/// Splits a Windows cmd command line: first removes `^` escapes and line
/// continuations (what cmd.exe does), then applies the MSVC argv rules.
fn split_cmd(cmd: &str) -> Vec<String> {
    let mut line = String::with_capacity(cmd.len());
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '^' {
            match chars.next() {
                // A caret before a line break escapes the first character of
                // the next line: `^` LF LF is a newline (how Chrome writes one).
                Some(c @ ('\r' | '\n')) => {
                    if c == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    let mut next = chars.clone();
                    if next.peek() == Some(&'\r') {
                        next.next();
                    }
                    if next.peek() == Some(&'\n') {
                        next.next();
                        chars = next;
                        line.push('\n');
                    } else {
                        line.push(' ');
                    }
                }
                Some(n) => line.push(n),
                None => {}
            }
        } else {
            line.push(c);
        }
    }
    let chars: Vec<char> = line.chars().collect();
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            let mut n = 0;
            while i < chars.len() && chars[i] == '\\' {
                n += 1;
                i += 1;
            }
            if chars.get(i) == Some(&'"') {
                cur.extend(std::iter::repeat_n('\\', n / 2));
                if n % 2 == 1 {
                    cur.push('"');
                    i += 1;
                }
            } else {
                cur.extend(std::iter::repeat_n('\\', n));
            }
            in_word = true;
            continue;
        }
        if c == '"' {
            quoted = !quoted;
            in_word = true;
        } else if c.is_whitespace() && !quoted {
            if in_word {
                words.push(std::mem::take(&mut cur));
                in_word = false;
            }
        } else {
            cur.push(c);
            in_word = true;
        }
        i += 1;
    }
    if in_word {
        words.push(cur);
    }
    words
}

/// Short options that take a value.
const SHORT_WITH_ARG: &str = "dHXFubAeowmTxUErcKCzYyDQPt";

/// Long options that take a value (without the leading `--`).
const LONG_WITH_ARG: &[&str] = &[
    "request",
    "header",
    "data",
    "data-raw",
    "data-ascii",
    "data-binary",
    "data-urlencode",
    "json",
    "form",
    "form-string",
    "user",
    "cookie",
    "user-agent",
    "referer",
    "output",
    "write-out",
    "max-time",
    "upload-file",
    "url",
    "proxy",
    "proxy-user",
    "cert",
    "key",
    "cacert",
    "capath",
    "connect-timeout",
    "max-redirs",
    "range",
    "cookie-jar",
    "config",
    "oauth2-bearer",
    "aws-sigv4",
    "retry",
    "retry-delay",
    "retry-max-time",
    "resolve",
    "connect-to",
    "interface",
    "limit-rate",
    "dump-header",
    "request-target",
    "unix-socket",
    "abstract-unix-socket",
    "cert-type",
    "key-type",
    "pass",
    "ciphers",
    "local-port",
    "dns-servers",
    "expect100-timeout",
    "noproxy",
    "keepalive-time",
    "variable",
    "url-query",
    "trace",
    "trace-ascii",
    "stderr",
    "proxy-header",
    "preproxy",
    "socks4",
    "socks4a",
    "socks5",
    "socks5-hostname",
    "speed-limit",
    "speed-time",
    "time-cond",
    "tls-max",
    "max-filesize",
    "netrc-file",
    "pinnedpubkey",
    "service-name",
    "sasl-authzid",
    "hostpubmd5",
    "krb",
    "mail-from",
    "mail-rcpt",
    "mail-auth",
    "ftp-port",
    "quote",
    "telnet-option",
    "tftp-blksize",
    "parallel-max",
    "happy-eyeballs-timeout-ms",
    "doh-url",
    "etag-save",
    "etag-compare",
    "output-dir",
    "create-file-mode",
    "ech",
    "ipfs-gateway",
];

#[derive(Debug)]
enum DataKind {
    /// -d / --data / --data-ascii: `@file` reads a file.
    Data,
    /// --data-raw: never reads files.
    Raw,
    /// --data-binary: `@file` reads a file verbatim.
    Binary,
    /// --data-urlencode.
    Urlencode,
    /// --json.
    Json,
}

impl DataKind {
    fn option(&self) -> &'static str {
        match self {
            DataKind::Data => "--data",
            DataKind::Raw => "--data-raw",
            DataKind::Binary => "--data-binary",
            DataKind::Urlencode => "--data-urlencode",
            DataKind::Json => "--json",
        }
    }
}

#[derive(Default)]
struct Parsed {
    method: Option<String>,
    url: Option<String>,
    url_queries: Vec<String>,
    headers: Vec<KeyValue>,
    data: Vec<(DataKind, String)>,
    form: Vec<FormField>,
    user: Option<String>,
    auth_scheme: Option<AuthType>,
    bearer: Option<String>,
    aws_sigv4: Option<String>,
    cookies: Vec<String>,
    get: bool,
    head: bool,
    upload: Option<String>,
    insecure: bool,
    location: bool,
    max_redirs: Option<u32>,
    max_time_ms: Option<u64>,
    http_version: Option<HttpVersion>,
    warnings: Vec<String>,
}

fn apply_option(p: &mut Parsed, name: &str, value: Option<String>) -> Result<(), ConvertError> {
    let v = || value.clone().unwrap_or_default();
    match name {
        "X" | "request" => p.method = Some(v().to_uppercase()),
        "H" | "header" => {
            let h = v();
            if let Some((k, val)) = h.split_once(':') {
                let val = val.trim();
                // `Name:` alone removes an internal header in curl; nothing to import.
                if !val.is_empty() {
                    p.headers.push(KeyValue::new(k.trim(), val));
                }
            } else if let Some(k) = h.strip_suffix(';') {
                p.headers.push(KeyValue::new(k.trim(), ""));
            }
        }
        "d" | "data" | "data-ascii" => p.data.push((DataKind::Data, v())),
        "data-raw" => p.data.push((DataKind::Raw, v())),
        "data-binary" => p.data.push((DataKind::Binary, v())),
        "data-urlencode" => p.data.push((DataKind::Urlencode, v())),
        "json" => p.data.push((DataKind::Json, v())),
        "F" | "form" => {
            let field = parse_form(&v(), false, &mut p.warnings);
            p.form.push(field);
        }
        "form-string" => p.form.push(parse_form(&v(), true, &mut p.warnings)),
        "u" | "user" => p.user = Some(v()),
        "b" | "cookie" => {
            let c = v();
            if c.contains('=') {
                p.cookies.push(c);
            }
        }
        "A" | "user-agent" => p.headers.push(KeyValue::new("User-Agent", v())),
        "e" | "referer" => p.headers.push(KeyValue::new("Referer", v())),
        "G" | "get" => p.get = true,
        "I" | "head" => p.head = true,
        "T" | "upload-file" => p.upload = Some(v()),
        "k" | "insecure" => p.insecure = true,
        "L" | "location" | "location-trusted" => p.location = true,
        "max-redirs" => p.max_redirs = v().trim().parse().ok(),
        "m" | "max-time" => p.max_time_ms = v().trim().parse::<f64>().ok().map(|s| (s * 1000.0).round() as u64),
        "url" => {
            if p.url.is_none() {
                p.url = Some(v());
            }
        }
        "url-query" => p.url_queries.push(v()),
        "http1.0" | "0" | "http1.1" => p.http_version = Some(HttpVersion::Http1),
        "http2" | "http2-prior-knowledge" => p.http_version = Some(HttpVersion::Http2),
        "digest" => p.auth_scheme = Some(AuthType::Digest),
        "ntlm" => p.auth_scheme = Some(AuthType::Ntlm),
        "basic" => p.auth_scheme = Some(AuthType::Basic),
        "oauth2-bearer" => p.bearer = Some(v()),
        "aws-sigv4" => p.aws_sigv4 = Some(v()),
        _ => {}
    }
    Ok(())
}

/// Reads a `"quoted"` curl -F word. Like curl, only `\\` and `\"` are
/// escapes. Returns the text and the remainder after the closing quote, or
/// `None` without a closing quote (curl then reads the word unquoted).
fn read_quoted(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = s.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some((_, '\\' | '"'))) => {
                out.push(chars.next().map(|(_, n)| n).unwrap_or('\\'));
            }
            '"' => return Some((out, &s[i + 1..])),
            c => out.push(c),
        }
    }
    None
}

/// One `-F` word (a value or a `;param=` value): quoted, or up to the next
/// `;` with trailing spaces dropped. Returns the word and the rest, which
/// starts at the next `;` (or is empty).
fn form_word(s: &str) -> (String, &str) {
    if s.starts_with('"')
        && let Some((word, rest)) = read_quoted(s)
    {
        return (word, rest.find(';').map(|i| &rest[i..]).unwrap_or(""));
    }
    let end = s.find(';').unwrap_or(s.len());
    (s[..end].trim_end().to_string(), &s[end..])
}

/// The content type set by the `;type=...` and `;headers="Content-Type: ..."`
/// parameters after a `-F` value. As in curl, parameters that follow `type=`
/// and are not themselves `-F` keywords belong to the type
/// (`type=text/plain; charset=utf-8`), which is taken verbatim.
fn form_content_type(opts: &str) -> Option<String> {
    let keyword = |s: &str, k: &str| s.get(..k.len()).is_some_and(|p| p.eq_ignore_ascii_case(k));
    // Byte range of the type in `opts`.
    let mut ty: Option<(usize, usize)> = None;
    let mut in_type = false;
    let mut header_ct = None;
    let mut pos = 0;
    while opts[pos..].starts_with(';') {
        let after = &opts[pos + 1..];
        let param_start = opts.len() - after.trim_start().len();
        let param = &opts[param_start..];
        let end = param_start + param.find(';').unwrap_or(param.len());
        if !in_type && keyword(param, "type=") {
            ty = Some((param_start + 5, end));
            in_type = true;
            pos = end;
        } else if in_type && !keyword(param, "filename=") && !keyword(param, "encoder=") && !keyword(param, "headers=")
        {
            if let Some(t) = ty.as_mut() {
                t.1 = end;
            }
            pos = end;
        } else {
            in_type = false;
            let value_start = param.find('=').map(|i| i + 1).unwrap_or(0);
            let (word, rest) = form_word(param[value_start..].trim_start());
            if keyword(param, "headers=")
                && let Some((name, value)) = word.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-type")
            {
                header_ct = Some(value.trim().to_string());
            }
            pos = opts.len() - rest.len();
        }
    }
    ty.map(|(a, b)| opts[a..b].trim().to_string()).filter(|t| !t.is_empty()).or(header_ct)
}

/// Parses a `-F name=content` argument like curl: the name runs to the first
/// `=`; `@path` uploads a file and `<path` sends a file's content as a text
/// field (imported as the text `<path`, see the module docs).
fn parse_form(arg: &str, literal: bool, warnings: &mut Vec<String>) -> FormField {
    let (name, content) = arg.split_once('=').unwrap_or((arg, ""));
    if literal {
        return FormField::text(name, content);
    }
    let (marker, rest) = match content.as_bytes().first() {
        Some(&m @ (b'@' | b'<')) => (Some(m), &content[1..]),
        _ => (None, content),
    };
    let (value, opts) = form_word(rest.trim_start());
    let mut field = match marker {
        Some(b'@') => FormField::file(name, value),
        Some(_) => {
            warnings.push(format!(
                "--form {name}=<{value}: curl sends the content of {} as the text field `{name}`; it was imported \
                 as the placeholder `<{value}`, replace it with the file's content",
                file_label(&value)
            ));
            FormField::text(name, format!("<{value}"))
        }
        None => FormField::text(name, value),
    };
    field.content_type = form_content_type(opts);
    field
}

fn file_label(path: &str) -> String {
    if path == "-" { "standard input".into() } else { format!("`{path}`") }
}

/// curl's `curl_easy_escape`: everything but `A-Za-z0-9-._~` is encoded.
fn percent_encode_component(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, merpati_core::prepared::QUERY_ENCODE).to_string()
}

/// Turns one `--data-urlencode` (or `--url-query`) argument into its encoded
/// form, splitting it like curl: `[name]=content` when there is a `=`,
/// otherwise `[name]@file`, otherwise plain content. Only the content is
/// encoded. A file keeps the placeholder `@file` (with a warning).
fn urlencode_arg(option: &str, arg: &str, warnings: &mut Vec<String>) -> String {
    let (name, sep, rest) = match arg.find('=').or_else(|| arg.find('@')) {
        Some(i) => (&arg[..i], &arg[i..i + 1], &arg[i + 1..]),
        None => ("", "", arg),
    };
    let encoded = if sep == "@" {
        warnings.push(format!(
            "{option} {arg}: curl URL-encodes the content of {}; it was imported as the placeholder `@{rest}`, \
             replace it with the file's content",
            file_label(rest)
        ));
        format!("@{rest}")
    } else {
        percent_encode_component(rest)
    };
    if name.is_empty() { encoded } else { format!("{name}={encoded}") }
}

fn header_value<'a>(headers: &'a [KeyValue], name: &str) -> Option<&'a str> {
    headers.iter().rev().find(|h| h.key.eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

fn looks_like_form_urlencoded(data: &str) -> bool {
    let t = data.trim_start();
    if t.is_empty() || t.starts_with('{') || t.starts_with('[') || t.starts_with('<') {
        return false;
    }
    !data.contains('\n') && data.split('&').all(|pair| !pair.is_empty())
}

/// Parses a cURL command into a request (see [`parse_with_warnings`]).
pub fn parse(cmd: &str) -> Result<HttpRequest, ConvertError> {
    parse_with_warnings(cmd).map(|(req, _)| req)
}

/// Parses a cURL command into a request, plus warnings about what could not
/// be imported exactly (file contents read when curl runs).
pub fn parse_with_warnings(cmd: &str) -> Result<(HttpRequest, Vec<String>), ConvertError> {
    let mut words = if is_cmd_style(cmd) { split_cmd(cmd) } else { split_posix(cmd)? };
    // Drop a shell prompt (`$ curl`, `$curl`) and the program name.
    if words.first().map(|w| w == "$").unwrap_or(false) {
        words.remove(0);
    }
    match words.first().map(|w| w.strip_prefix('$').unwrap_or(w)) {
        Some(w) if w == "curl" || w == "curl.exe" || w.ends_with("/curl") => {
            words.remove(0);
        }
        _ => return Err(ConvertError::Parse("not a curl command".into())),
    }

    let mut p = Parsed::default();
    let mut i = 0;
    let mut end_of_options = false;
    while i < words.len() {
        let w = &words[i];
        i += 1;
        if end_of_options || !w.starts_with('-') || w == "-" {
            if p.url.is_none() {
                p.url = Some(w.clone());
            }
            continue;
        }
        if w == "--" {
            end_of_options = true;
            continue;
        }
        if let Some(long) = w.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) if LONG_WITH_ARG.contains(&n) => (n, Some(v.to_string())),
                _ => (long, None),
            };
            let value = if LONG_WITH_ARG.contains(&name) {
                match inline {
                    Some(v) => Some(v),
                    None => {
                        let v = words.get(i).cloned();
                        i += 1;
                        v
                    }
                }
            } else {
                None
            };
            apply_option(&mut p, name, value)?;
            continue;
        }
        // Short options, possibly combined (`-sSL`, `-XPOST`, `-H'X: 1'`).
        let flags: Vec<char> = w[1..].chars().collect();
        let mut j = 0;
        while j < flags.len() {
            let f = flags[j];
            if SHORT_WITH_ARG.contains(f) {
                let attached: String = flags[j + 1..].iter().collect();
                let value = if attached.is_empty() {
                    let v = words.get(i).cloned();
                    i += 1;
                    v
                } else {
                    Some(attached)
                };
                apply_option(&mut p, &f.to_string(), value)?;
                break;
            }
            apply_option(&mut p, &f.to_string(), None)?;
            j += 1;
        }
    }

    let mut url = p.url.clone().ok_or_else(|| ConvertError::Parse("no URL in curl command".into()))?;
    for q in &p.url_queries {
        // `+` sends the rest as is; otherwise the same rules as --data-urlencode.
        let piece = match q.strip_prefix('+') {
            Some(raw) => raw.to_string(),
            None => urlencode_arg("--url-query", q, &mut p.warnings),
        };
        url = append_query(&url, &piece);
    }

    let mut headers = p.headers.clone();
    if !p.cookies.is_empty() {
        let joined = p.cookies.join("; ");
        match headers.iter_mut().find(|h| h.key.eq_ignore_ascii_case("cookie")) {
            Some(h) => h.value = format!("{}; {joined}", h.value),
            None => headers.push(KeyValue::new("Cookie", joined)),
        }
    }

    let mut body = Body::default();
    let mut implied_method = "GET";
    let is_json = p.data.iter().any(|(k, _)| matches!(k, DataKind::Json));
    if is_json {
        if header_value(&headers, "content-type").is_none() {
            headers.push(KeyValue::new("Content-Type", "application/json"));
        }
        if header_value(&headers, "accept").is_none() {
            headers.push(KeyValue::new("Accept", "application/json"));
        }
    }

    if !p.form.is_empty() {
        body.mode = BodyMode::FormData;
        body.form_data = p.form.clone();
        implied_method = "POST";
    } else if let Some(file) = &p.upload {
        body.mode = BodyMode::Binary;
        body.binary_path = file.clone();
        implied_method = "PUT";
    } else if !p.data.is_empty() {
        let file_ref = match p.data.as_slice() {
            [(DataKind::Data | DataKind::Binary | DataKind::Json, d)] if d.starts_with('@') && d != "@-" && !p.get => {
                Some(d[1..].to_string())
            }
            _ => None,
        };
        let mut pieces = Vec::with_capacity(p.data.len());
        for (k, d) in &p.data {
            pieces.push(match k {
                DataKind::Urlencode => urlencode_arg(k.option(), d, &mut p.warnings),
                DataKind::Raw => d.clone(),
                // curl strips CR/LF only from data read with `-d @file`.
                _ => {
                    if let Some(path) = d.strip_prefix('@')
                        && file_ref.is_none()
                    {
                        p.warnings.push(format!(
                            "{} {d}: curl sends the content of {} here; it was imported as the placeholder `{d}`, \
                             replace it with the file's content",
                            k.option(),
                            file_label(path)
                        ));
                    }
                    d.clone()
                }
            });
        }
        let joined = pieces.join(if is_json { "" } else { "&" });
        if p.get {
            url = append_query(&url, &joined);
        } else if let Some(path) = file_ref {
            body.mode = BodyMode::Binary;
            body.binary_path = path;
            implied_method = "POST";
        } else {
            implied_method = "POST";
            let ct = header_value(&headers, "content-type").map(str::to_ascii_lowercase);
            let ct_ref = ct.as_deref().unwrap_or("");
            let trimmed = joined.trim_start();
            if is_json
                || ct_ref.contains("json")
                || (ct.is_none() && (trimmed.starts_with('{') || trimmed.starts_with('[')))
            {
                body = Body::raw_text(joined, RawLanguage::Json);
            } else if (ct.is_none() || ct_ref.contains("x-www-form-urlencoded")) && looks_like_form_urlencoded(&joined)
            {
                body.mode = BodyMode::UrlEncoded;
                body.urlencoded = url::form_urlencoded::parse(joined.as_bytes())
                    .map(|(k, v)| KeyValue::new(k.into_owned(), v.into_owned()))
                    .collect();
            } else {
                let lang = if ct.is_some() { crate::util::raw_language_for(ct_ref) } else { RawLanguage::Text };
                body = Body::raw_text(joined, lang);
            }
        }
    }

    let method =
        if p.head { "HEAD".to_string() } else { p.method.clone().unwrap_or_else(|| implied_method.to_string()) };

    let mut req = HttpRequest::new(method, url);
    req.headers = headers;
    req.body = body;

    if let Some(token) = &p.bearer {
        req.auth = Auth::bearer(token.clone());
    } else if let Some(user) = &p.user {
        let (u, pw) = user.split_once(':').unwrap_or((user.as_str(), ""));
        if let Some(sig) = &p.aws_sigv4 {
            // --aws-sigv4 "aws:amz:region:service"
            let parts: Vec<&str> = sig.split(':').collect();
            req.auth = Auth::new(AuthType::AwsV4)
                .with("accessKey", u)
                .with("secretKey", pw)
                .with("region", parts.get(2).copied().unwrap_or(""))
                .with("service", parts.get(3).copied().unwrap_or(""));
        } else {
            match p.auth_scheme.unwrap_or(AuthType::Basic) {
                AuthType::Digest => {
                    req.auth = Auth::new(AuthType::Digest).with("username", u).with("password", pw);
                }
                AuthType::Ntlm => {
                    let (domain, name) = match u.split_once('\\') {
                        Some((d, n)) => (d, n),
                        None => ("", u),
                    };
                    req.auth =
                        Auth::new(AuthType::Ntlm).with("username", name).with("password", pw).with("domain", domain);
                }
                _ => req.auth = Auth::basic(u, pw),
            }
        }
    }

    if p.insecure {
        req.settings.verify_ssl = Some(false);
    }
    if p.location {
        req.settings.follow_redirects = Some(true);
    }
    req.settings.max_redirects = p.max_redirs;
    req.settings.timeout_ms = p.max_time_ms;
    req.settings.http_version = p.http_version;
    Ok((req, p.warnings))
}

fn append_query(url: &str, q: &str) -> String {
    if q.is_empty() {
        return url.to_string();
    }
    let (base, query, fragment) = merpati_core::urlutil::split(url);
    match query {
        Some(existing) if !existing.is_empty() => format!("{base}?{existing}&{q}{fragment}"),
        _ => format!("{base}?{q}{fragment}"),
    }
}

#[cfg(test)]
mod tests {
    use merpati_core::model::FormFieldKind;

    use super::*;

    #[test]
    fn basic_get() {
        let r = parse("curl https://api.test/users?page=2").unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.url, "https://api.test/users?page=2");
        assert_eq!(r.params[0].key, "page");
        assert_eq!(r.body.mode, BodyMode::None);
    }

    #[test]
    fn chrome_bash_copy() {
        let cmd = r#"curl 'https://example.com/api/items' \
  -H 'accept: application/json' \
  -H 'content-type: application/json' \
  -b 'session=abc; theme=dark' \
  --data-raw $'{"name":"it\'s","n":1}' \
  --compressed"#;
        let r = parse(cmd).unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.url, "https://example.com/api/items");
        assert_eq!(r.header("accept"), Some("application/json"));
        assert_eq!(r.header("cookie"), Some("session=abc; theme=dark"));
        assert_eq!(r.body.mode, BodyMode::Raw);
        assert_eq!(r.body.raw_language, RawLanguage::Json);
        assert_eq!(r.body.raw, r#"{"name":"it's","n":1}"#);
    }

    #[test]
    fn chrome_cmd_copy() {
        let cmd =
            "curl ^\"https://example.com/api^\" ^\n  -H ^\"accept: */*^\" ^\n  --data-raw ^\"^{^\\^\"a^\\^\":1^}^\"";
        let r = parse(cmd).unwrap();
        assert_eq!(r.url, "https://example.com/api");
        assert_eq!(r.header("accept"), Some("*/*"));
        assert_eq!(r.body.raw, r#"{"a":1}"#);
        assert_eq!(r.method, "POST");
    }

    #[test]
    fn combined_short_flags_and_attached_values() {
        let r = parse("curl -sSLk -XPUT -H'X-A: 1' -uuser:pa:ss http://h/x").unwrap();
        assert_eq!(r.method, "PUT");
        assert_eq!(r.header("x-a"), Some("1"));
        assert_eq!(r.auth.kind, AuthType::Basic);
        assert_eq!(r.auth.get("username"), "user");
        assert_eq!(r.auth.get("password"), "pa:ss");
        assert_eq!(r.settings.verify_ssl, Some(false));
        assert_eq!(r.settings.follow_redirects, Some(true));
    }

    #[test]
    fn urlencoded_data() {
        let r = parse("curl http://h/login -d 'user=a b' -d pass=x%26y --data-urlencode 'note=1&2'").unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.body.mode, BodyMode::UrlEncoded);
        let kv: Vec<(&str, &str)> = r.body.urlencoded.iter().map(|k| (k.key.as_str(), k.value.as_str())).collect();
        assert_eq!(kv, vec![("user", "a b"), ("pass", "x&y"), ("note", "1&2")]);
    }

    #[test]
    fn get_with_data_goes_to_query() {
        let r = parse("curl -G https://h/search -d q=rust -d page=2").unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.url, "https://h/search?q=rust&page=2");
        assert_eq!(r.body.mode, BodyMode::None);
    }

    #[test]
    fn json_flag() {
        let r = parse(r#"curl --json '{"a": 1}' https://h/j"#).unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(r.header("accept"), Some("application/json"));
        assert_eq!(r.body.raw_language, RawLanguage::Json);
    }

    #[test]
    fn forms_and_files() {
        let r = parse(r#"curl -F 'title="hello; world"' -F 'file=@"/tmp/a b.png";type=image/png' -F n=1 https://h/u"#)
            .unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.body.mode, BodyMode::FormData);
        let f = &r.body.form_data;
        assert_eq!(f[0].value, "hello; world");
        assert_eq!(f[1].kind, FormFieldKind::File);
        assert_eq!(f[1].files, vec!["/tmp/a b.png"]);
        assert_eq!(f[1].content_type.as_deref(), Some("image/png"));
        assert_eq!(f[2].value, "1");
    }

    #[test]
    fn binary_and_upload() {
        let r = parse("curl --data-binary @payload.bin http://h/b").unwrap();
        assert_eq!(r.body.mode, BodyMode::Binary);
        assert_eq!(r.body.binary_path, "payload.bin");
        let r = parse("curl -T file.txt http://h/up").unwrap();
        assert_eq!(r.method, "PUT");
        assert_eq!(r.body.binary_path, "file.txt");
    }

    #[test]
    fn misc_flags() {
        let r = parse("curl -I --max-time 2.5 --max-redirs 3 --http2 -A 'Agent/1' -e ref http://h").unwrap();
        assert_eq!(r.method, "HEAD");
        assert_eq!(r.settings.timeout_ms, Some(2500));
        assert_eq!(r.settings.max_redirects, Some(3));
        assert_eq!(r.settings.http_version, Some(HttpVersion::Http2));
        assert_eq!(r.header("user-agent"), Some("Agent/1"));
        assert_eq!(r.header("referer"), Some("ref"));
        let r = parse("curl -o out.json -w '%{http_code}' --url https://h/a | jq .").unwrap();
        assert_eq!(r.url, "https://h/a");
        let r = parse("curl --oauth2-bearer tok https://h").unwrap();
        assert_eq!(r.auth.kind, AuthType::Bearer);
        let r = parse("curl --digest -u a:b https://h").unwrap();
        assert_eq!(r.auth.kind, AuthType::Digest);
        let r = parse("curl --aws-sigv4 'aws:amz:eu-west-1:s3' -u AK:SK https://h").unwrap();
        assert_eq!(r.auth.kind, AuthType::AwsV4);
        assert_eq!(r.auth.get("region"), "eu-west-1");
    }

    #[test]
    fn errors() {
        assert!(parse("wget http://x").is_err());
        assert!(parse("curl -H 'x: y'").is_err());
        assert!(parse("curl 'unterminated").is_err());
    }

    #[test]
    fn literal_data_keeps_newlines() {
        // curl strips CR/LF only from `-d @file`, never from a literal value.
        let r = parse("curl http://h -H 'Content-Type: application/json' -d '{\n  \"a\": 1\n}'").unwrap();
        assert_eq!(r.body.raw, "{\n  \"a\": 1\n}");
        let r = parse("curl http://h -H 'Content-Type: text/plain' --data $'line 1\\r\\nline 2'").unwrap();
        assert_eq!(r.body.raw, "line 1\r\nline 2");
    }

    #[test]
    fn ansi_c_byte_escapes_form_utf8() {
        let r =
            parse(r"curl http://h -H 'Content-Type: text/plain' --data-raw $'caf\xc3\xa9 \303\251 é \xe9'").unwrap();
        assert_eq!(r.body.raw, "café é é é");
    }

    #[test]
    fn chrome_cmd_copy_multiline_data() {
        // Chrome writes a newline inside data as `^` + LF + LF.
        let cmd = "curl ^\"https://h/api^\" ^\n  --data-raw ^\"a^\n\nb^\"";
        let r = parse(cmd).unwrap();
        assert_eq!(r.url, "https://h/api");
        assert_eq!(r.body.raw, "a\nb");
    }

    #[test]
    fn prompt_and_xml_body() {
        let r = parse("$ curl -X POST -H 'Content-Type: application/xml' -d '<a>1</a>' http://h").unwrap();
        assert_eq!(r.body.raw_language, RawLanguage::Xml);
        assert_eq!(r.body.raw, "<a>1</a>");
    }

    #[test]
    fn data_urlencode_file_keeps_placeholder() {
        let (r, w) = parse_with_warnings(
            "curl http://h/n --data-urlencode msg@note.txt --data-urlencode '=a b' --data-urlencode 'a@b=c d' -d x=1",
        )
        .unwrap();
        assert_eq!(r.body.mode, BodyMode::UrlEncoded);
        let kv: Vec<(&str, &str)> = r.body.urlencoded.iter().map(|k| (k.key.as_str(), k.value.as_str())).collect();
        // `=` wins over `@`: `a@b` is a name.
        assert_eq!(kv, vec![("msg", "@note.txt"), ("a b", ""), ("a@b", "c d"), ("x", "1")]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("--data-urlencode msg@note.txt") && w[0].contains("`note.txt`"), "{w:?}");
        let (_, w) = parse_with_warnings("curl http://h --data-urlencode @-").unwrap();
        assert!(w[0].contains("standard input"), "{w:?}");
    }

    #[test]
    fn form_text_from_file_is_not_an_upload() {
        let (r, w) =
            parse_with_warnings("curl http://h/u -F 'bio=<about.txt;type=text/plain' -F 'pic=@me.png'").unwrap();
        let f = &r.body.form_data;
        assert_eq!(f[0].kind, FormFieldKind::Text);
        assert_eq!(f[0].value, "<about.txt");
        assert_eq!(f[0].content_type.as_deref(), Some("text/plain"));
        assert_eq!(f[1].kind, FormFieldKind::File);
        assert_eq!(f[1].files, vec!["me.png"]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`about.txt`") && w[0].contains("`bio`"), "{w:?}");
    }

    #[test]
    fn form_parameters_like_curl() {
        let r = parse(
            r#"curl http://h -F 'a=x;type=text/plain;charset=utf-8; format=flowed;filename=y.txt' -F 'b="q\n\"z\\";headers="Content-Type: odd, \"t\""' -F 'c=@"/p/a,b.bin";filename="n;m";type=image/png'"#,
        )
        .unwrap();
        let f = &r.body.form_data;
        assert_eq!(f[0].value, "x");
        assert_eq!(f[0].content_type.as_deref(), Some("text/plain;charset=utf-8; format=flowed"));
        // Only `\\` and `\"` are escapes inside quotes.
        assert_eq!(f[1].value, "q\\n\"z\\");
        assert_eq!(f[1].content_type.as_deref(), Some("odd, \"t\""));
        assert_eq!(f[2].files, vec!["/p/a,b.bin"]);
        assert_eq!(f[2].content_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn data_file_next_to_other_data() {
        let (r, w) = parse_with_warnings("curl http://h -d @body.txt -d a=1").unwrap();
        assert_eq!(r.body.mode, BodyMode::UrlEncoded);
        assert_eq!(r.body.urlencoded[0].key, "@body.txt");
        assert_eq!(r.body.urlencoded[1].value, "1");
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("--data @body.txt") && w[0].contains("`body.txt`"), "{w:?}");
        // Alone it is the whole (binary) body: nothing to warn about.
        let (r, w) = parse_with_warnings("curl http://h -d @body.txt").unwrap();
        assert_eq!((r.body.mode, r.body.binary_path.as_str()), (BodyMode::Binary, "body.txt"));
        assert!(w.is_empty());
        let (r, w) = parse_with_warnings("curl -G http://h -d @q.txt").unwrap();
        assert_eq!(r.url, "http://h?@q.txt");
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn url_query_is_encoded_like_curl() {
        let (r, w) = parse_with_warnings(
            "curl 'http://h/s?x=1' --url-query 'q=a b&c' --url-query '+raw=a+b' --url-query =é --url-query f@q.txt",
        )
        .unwrap();
        assert_eq!(r.url, "http://h/s?x=1&q=a%20b%26c&raw=a+b&%C3%A9&f=@q.txt");
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("--url-query f@q.txt"), "{w:?}");
    }

    #[test]
    fn prompt_prefixes() {
        for cmd in ["$curl http://h/a", "$ curl http://h/a", "  $  curl http://h/a"] {
            assert_eq!(parse(cmd).unwrap().url, "http://h/a", "{cmd}");
            assert!(looks_like_curl(cmd), "{cmd}");
        }
        assert!(parse("$wget http://h/a").is_err());
    }

    #[test]
    fn posix_data_with_caret_quote_is_not_cmd() {
        let r = parse(r#"curl http://h/r -H 'Content-Type: text/plain' -d 'pattern=^"abc"$'"#).unwrap();
        assert_eq!(r.body.raw, r#"pattern=^"abc"$"#);
        let r = parse("curl http://h/r -H 'Content-Type: text/plain' --data-raw $'it\\'s ^\"x\"'").unwrap();
        assert_eq!(r.body.raw, r#"it's ^"x""#);
        let r = parse("curl \"http://h/r\" -H \"Content-Type: text/plain\" -d \"a^\\\"b\"").unwrap();
        assert_eq!(r.body.raw, r#"a^"b"#);
        // Real cmd copies are still recognized, even with a quote inside.
        let r = parse("curl ^\"https://h/it's^\" ^\n  -H ^\"accept: */*^\"").unwrap();
        assert_eq!(r.url, "https://h/it's");
    }
}
