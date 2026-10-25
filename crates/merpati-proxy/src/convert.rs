//! Turning captured entries into Merpati requests and examples.

use merpati_core::model::{
    Body, BodyMode, Example, FormField, FormFieldKind, GraphQlBody, HttpRequest, KeyValue, RawLanguage, RequestItem,
    RequestKind, WsMessageKind, WsRequest, WsSavedMessage,
};

use crate::entry::{CaptureEntry, EntryKind, WsOpcode};

/// How captured requests become collection items.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Keep `Cookie` headers (they often carry session secrets).
    pub keep_cookies: bool,
    /// Drop `Accept-Encoding` so Merpati negotiates what it can decode.
    pub drop_accept_encoding: bool,
    /// Drop browser noise: `sec-ch-ua*`, `sec-fetch-*`, `upgrade-insecure-requests`, `priority`, `dnt`.
    pub drop_browser_headers: bool,
    /// Attach the captured response as an example.
    pub include_example: bool,
    /// Client-to-server WebSocket text messages saved with a WebSocket request.
    pub max_ws_messages: usize,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            keep_cookies: true,
            drop_accept_encoding: true,
            drop_browser_headers: false,
            include_example: true,
            max_ws_messages: 20,
        }
    }
}

/// Headers that never belong in a saved request: hop-by-hop, proxy, and the
/// ones the HTTP engine derives itself.
const ALWAYS_DROP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "proxy-authenticate",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "expect",
    "http2-settings",
];

const WS_HANDSHAKE: &[&str] =
    &["sec-websocket-key", "sec-websocket-version", "sec-websocket-extensions", "sec-websocket-protocol"];

fn is_browser_header(name: &str) -> bool {
    name.starts_with("sec-ch-ua")
        || name.starts_with("sec-fetch-")
        || matches!(name, "upgrade-insecure-requests" | "priority" | "dnt")
}

/// `content-type` -> `Content-Type`.
fn title_case(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + &c.as_str().to_ascii_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn mime_of(content_type: Option<&str>) -> String {
    content_type.unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

fn raw_language(mime: &str) -> Option<RawLanguage> {
    if mime.contains("json") {
        Some(RawLanguage::Json)
    } else if mime.contains("xml") {
        Some(RawLanguage::Xml)
    } else if mime.contains("html") {
        Some(RawLanguage::Html)
    } else if mime.contains("javascript") || mime.contains("ecmascript") {
        Some(RawLanguage::JavaScript)
    } else if mime.starts_with("text/") || mime.is_empty() || mime.contains("graphql") || mime.contains("csv") {
        Some(RawLanguage::Text)
    } else {
        None
    }
}

impl CaptureEntry {
    /// Item name: `GET /path` (host for `/`).
    pub fn request_name(&self) -> String {
        let path = self.path().split('?').next().unwrap_or("/");
        let target = if path == "/" || path.is_empty() { self.host.as_str() } else { path };
        let mut name = format!("{} {}", self.method, target);
        if name.chars().count() > 100 {
            name = name.chars().take(99).collect::<String>() + "…";
        }
        name
    }

    fn import_headers(&self, opts: &ImportOptions, extra_drop: &[&str]) -> Vec<KeyValue> {
        let mut out: Vec<KeyValue> = Vec::new();
        for (k, v) in &self.request_headers {
            let lower = k.to_ascii_lowercase();
            if ALWAYS_DROP.contains(&lower.as_str())
                || extra_drop.contains(&lower.as_str())
                || lower.starts_with(':')
                || (!opts.keep_cookies && lower == "cookie")
                || (opts.drop_accept_encoding && lower == "accept-encoding")
                || (opts.drop_browser_headers && is_browser_header(&lower))
            {
                continue;
            }
            out.push(KeyValue::new(title_case(&lower), v.clone()));
        }
        out
    }

    /// The captured request as a Merpati HTTP request, without hop-by-hop or
    /// proxy headers. Bodies become raw (by content type), url-encoded,
    /// form-data (file parts need the file chosen again) or GraphQL.
    pub fn to_http_request(&self, opts: &ImportOptions) -> HttpRequest {
        self.http_request_with_notes(opts).0
    }

    fn http_request_with_notes(&self, opts: &ImportOptions) -> (HttpRequest, Vec<String>) {
        let mut notes = Vec::new();
        let mut req = HttpRequest::new(self.method.clone(), self.url.clone());
        let mime = mime_of(self.request_header("content-type"));
        let multipart = mime.starts_with("multipart/form-data");
        req.headers = self.import_headers(
            opts,
            if multipart { &["content-type", "content-encoding"] } else { &["content-encoding"] },
        );
        if self.request_body.size > 0 {
            if self.request_body.truncated {
                notes.push(format!(
                    "The request body was captured only partially ({} of {} bytes).",
                    self.request_body.data.len(),
                    self.request_body.size
                ));
            }
            let decoded = self.decoded_request_body();
            let data = decoded.data.as_ref();
            if mime == "application/x-www-form-urlencoded" {
                req.body = Body {
                    mode: BodyMode::UrlEncoded,
                    urlencoded: url::form_urlencoded::parse(data)
                        .map(|(k, v)| KeyValue::new(k.into_owned(), v.into_owned()))
                        .collect(),
                    ..Body::default()
                };
            } else if multipart {
                let boundary = self
                    .request_header("content-type")
                    .and_then(|ct| ct.split(';').find_map(|p| p.trim().strip_prefix("boundary=")))
                    .map(|b| b.trim_matches('"').to_owned());
                match boundary.and_then(|b| parse_multipart(data, &b)) {
                    Some(fields) => {
                        if fields.iter().any(|f| f.kind == FormFieldKind::File) {
                            notes.push("File parts were captured; choose the files again before sending.".into());
                        }
                        req.body = Body { mode: BodyMode::FormData, form_data: fields, ..Body::default() };
                    }
                    None => notes.push("The multipart body could not be parsed and was not imported.".into()),
                }
            } else if let Some(text) = decoded.text() {
                if let Some(gql) = graphql_body(&mime, &text, &self.url) {
                    req.body = Body { mode: BodyMode::GraphQl, graphql: gql, ..Body::default() };
                } else {
                    let lang = raw_language(&mime).unwrap_or(RawLanguage::Text);
                    req.body = Body::raw_text(text.into_owned(), lang);
                }
            } else {
                notes.push(format!("The binary request body ({} bytes) was not imported.", self.request_body.size));
            }
        }
        (req, notes)
    }

    /// The captured response as an example (decoded body, without
    /// `Content-Encoding`/`Content-Length`). `None` without a response.
    pub fn to_example(&self) -> Option<Example> {
        let status = self.status?;
        if self.kind != EntryKind::Http {
            return None;
        }
        let decoded = self.decoded_response_body();
        let mime = mime_of(self.response_header("content-type"));
        let body = decoded.text().map(|t| t.into_owned()).unwrap_or_default();
        let headers = self
            .response_headers
            .iter()
            .filter(|(k, _)| {
                let k = k.to_ascii_lowercase();
                !ALWAYS_DROP.contains(&k.as_str())
                    && !(decoded.encoding.is_some() && k == "content-encoding")
                    && k != "content-length"
            })
            .map(|(k, v)| KeyValue::new(title_case(k), v.clone()))
            .collect();
        let language = match raw_language(&mime) {
            Some(RawLanguage::Json) => "json",
            Some(RawLanguage::Xml) => "xml",
            Some(RawLanguage::Html) => "html",
            Some(RawLanguage::JavaScript) => "javascript",
            _ => "text",
        };
        Some(Example {
            name: format!("{} {}", status, self.status_text()).trim().to_owned(),
            request: Some(self.to_http_request(&ImportOptions { include_example: false, ..ImportOptions::default() })),
            status,
            status_text: self.status_text().to_owned(),
            headers,
            body,
            language: Some(language.to_owned()),
            ..Example::default()
        })
    }

    /// A collection item for this entry: an HTTP request (with the response
    /// as an example) or a WebSocket request. `None` for tunnels.
    pub fn to_request_item(&self, opts: &ImportOptions) -> Option<RequestItem> {
        match self.kind {
            EntryKind::Tunnel => None,
            EntryKind::WebSocket => Some(self.ws_item(opts)),
            EntryKind::Http => {
                let (req, notes) = self.http_request_with_notes(opts);
                let mut item = RequestItem::new(self.request_name(), req);
                item.description = notes.join("\n");
                if opts.include_example
                    && let Some(ex) = self.to_example()
                {
                    item.examples.push(ex);
                }
                Some(item)
            }
        }
    }

    fn ws_item(&self, opts: &ImportOptions) -> RequestItem {
        let url = match self.url.split_once("://") {
            Some((scheme, rest)) => format!("{}://{rest}", if scheme == "https" { "wss" } else { "ws" }),
            None => self.url.clone(),
        };
        let subprotocols = self
            .request_header("sec-websocket-protocol")
            .map(|p| p.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let messages = self
            .ws_messages
            .iter()
            .filter(|m| m.from_client && m.opcode == WsOpcode::Text && !m.truncated)
            .take(opts.max_ws_messages)
            .enumerate()
            .map(|(i, m)| WsSavedMessage {
                name: format!("Message {}", i + 1),
                body: m.text().into_owned(),
                kind: WsMessageKind::Text,
                ..WsSavedMessage::default()
            })
            .collect();
        let ws = WsRequest {
            url,
            headers: self.import_headers(opts, WS_HANDSHAKE),
            subprotocols,
            messages,
            ..WsRequest::default()
        };
        let mut item = RequestItem::new(self.request_name(), HttpRequest::default());
        item.request = RequestKind::WebSocket(ws);
        item
    }
}

/// Converts entries to items (tunnels are skipped), oldest first.
pub fn to_request_items<'a>(
    entries: impl IntoIterator<Item = &'a CaptureEntry>,
    opts: &ImportOptions,
) -> Vec<RequestItem> {
    entries.into_iter().filter_map(|e| e.to_request_item(opts)).collect()
}

fn graphql_body(mime: &str, text: &str, url: &str) -> Option<GraphQlBody> {
    if mime == "application/graphql" {
        return Some(GraphQlBody { query: text.to_owned(), ..GraphQlBody::default() });
    }
    if !mime.contains("json") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = value.as_object()?;
    let query = obj.get("query")?.as_str()?;
    let looks_like_gql = url.to_ascii_lowercase().contains("graphql")
        || ["query", "mutation", "subscription", "fragment", "{"].iter().any(|k| query.trim_start().starts_with(k));
    if !looks_like_gql {
        return None;
    }
    let variables = match obj.get("variables") {
        Some(v) if !v.is_null() => serde_json::to_string_pretty(v).unwrap_or_default(),
        _ => String::new(),
    };
    Some(GraphQlBody {
        query: query.to_owned(),
        variables,
        operation_name: obj.get("operationName").and_then(|v| v.as_str()).map(str::to_owned),
    })
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn parse_multipart(body: &[u8], boundary: &str) -> Option<Vec<FormField>> {
    let delim = format!("--{boundary}");
    let delim = delim.as_bytes();
    let mut pos = find(body, delim, 0)? + delim.len();
    let mut fields = Vec::new();
    loop {
        if body[pos..].starts_with(b"--") {
            break;
        }
        let start = pos + if body[pos..].starts_with(b"\r\n") { 2 } else { 0 };
        let next = find(body, delim, start);
        let end = next.unwrap_or(body.len());
        let part = &body[start..end];
        let part = part.strip_suffix(b"\r\n").unwrap_or(part);
        let header_end = find(part, b"\r\n\r\n", 0)?;
        let head = String::from_utf8_lossy(&part[..header_end]);
        let content = &part[header_end + 4..];
        let mut name = None;
        let mut filename = None;
        let mut ctype = None;
        for line in head.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            match k.trim().to_ascii_lowercase().as_str() {
                "content-disposition" => {
                    for p in v.split(';').map(str::trim) {
                        if let Some(n) = p.strip_prefix("name=") {
                            name = Some(n.trim_matches('"').to_owned());
                        } else if let Some(f) = p.strip_prefix("filename=") {
                            filename = Some(f.trim_matches('"').to_owned());
                        }
                    }
                }
                "content-type" => ctype = Some(v.trim().to_owned()),
                _ => {}
            }
        }
        let key = name.unwrap_or_default();
        let field = match filename {
            Some(f) => FormField {
                key,
                kind: FormFieldKind::File,
                content_type: ctype,
                description: format!("Captured upload \"{f}\" ({} bytes); choose the file again.", content.len()),
                enabled: true,
                ..FormField::default()
            },
            None => {
                let mut field = FormField::text(key, String::from_utf8_lossy(content).into_owned());
                field.content_type = ctype;
                field
            }
        };
        fields.push(field);
        match next {
            Some(n) => pos = n + delim.len(),
            None => break,
        }
        if pos >= body.len() {
            break;
        }
    }
    Some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::CapturedBody;
    use bytes::Bytes;

    fn entry(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> CaptureEntry {
        let mut e = CaptureEntry::new(1, "127.0.0.1:1".parse().unwrap(), EntryKind::Http);
        e.method = method.into();
        e.url = url.into();
        e.host = "api.test".into();
        e.request_headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        e.request_body = CapturedBody { data: Bytes::copy_from_slice(body), size: body.len() as u64, truncated: false };
        e
    }

    #[test]
    fn json_request_without_hop_by_hop_headers() {
        let e = entry(
            "POST",
            "https://api.test/v1/users?x=1",
            &[
                ("host", "api.test"),
                ("connection", "keep-alive"),
                ("proxy-authorization", "Basic abc"),
                ("content-type", "application/json"),
                ("content-length", "13"),
                ("accept-encoding", "gzip, br, zstd"),
                ("x-api-key", "k"),
                ("cookie", "sid=1"),
            ],
            br#"{"name":"a"}"#,
        );
        let item = e.to_request_item(&ImportOptions::default()).unwrap();
        assert_eq!(item.name, "POST /v1/users");
        let req = item.http().unwrap();
        let names: Vec<&str> = req.headers.iter().map(|h| h.key.as_str()).collect();
        assert_eq!(names, vec!["Content-Type", "X-Api-Key", "Cookie"]);
        assert_eq!(req.body.mode, BodyMode::Raw);
        assert_eq!(req.body.raw_language, RawLanguage::Json);
        assert_eq!(req.params[0].key, "x");
        let no_cookies = e.to_http_request(&ImportOptions { keep_cookies: false, ..Default::default() });
        assert!(no_cookies.header("cookie").is_none());
    }

    #[test]
    fn form_multipart_and_graphql_bodies() {
        let e = entry(
            "POST",
            "http://api.test/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"user=a%20b&pass=x",
        );
        let r = e.to_http_request(&ImportOptions::default());
        assert_eq!(r.body.mode, BodyMode::UrlEncoded);
        assert_eq!(r.body.urlencoded[0].value, "a b");

        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nhello\r\n--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\x89PNG\r\n--XyZ--\r\n";
        let e = entry("POST", "http://api.test/upload", &[("content-type", "multipart/form-data; boundary=XyZ")], body);
        let item = e.to_request_item(&ImportOptions::default()).unwrap();
        let r = item.http().unwrap();
        assert_eq!(r.body.mode, BodyMode::FormData);
        assert_eq!(r.body.form_data.len(), 2);
        assert_eq!(r.body.form_data[0].value, "hello");
        assert_eq!(r.body.form_data[1].kind, FormFieldKind::File);
        assert!(r.header("content-type").is_none());
        assert!(item.description.contains("choose the files"));

        let e = entry(
            "POST",
            "https://api.test/graphql",
            &[("content-type", "application/json")],
            br#"{"query":"query Q { me { id } }","variables":{"a":1},"operationName":"Q"}"#,
        );
        let r = e.to_http_request(&ImportOptions::default());
        assert_eq!(r.body.mode, BodyMode::GraphQl);
        assert_eq!(r.body.graphql.operation_name.as_deref(), Some("Q"));
        assert!(r.body.graphql.variables.contains("\"a\": 1"));
    }

    #[test]
    fn example_and_websocket_items() {
        let mut e = entry("GET", "https://api.test/", &[], b"");
        e.status = Some(404);
        e.response_headers =
            vec![("content-type".into(), "application/json".into()), ("content-length".into(), "2".into())];
        e.response_body = CapturedBody { data: Bytes::from_static(b"{}"), size: 2, truncated: false };
        let ex = e.to_example().unwrap();
        assert_eq!(ex.name, "404 Not Found");
        assert_eq!(ex.language.as_deref(), Some("json"));
        assert_eq!(ex.headers.len(), 1);
        assert_eq!(e.request_name(), "GET api.test");

        let mut ws = entry(
            "GET",
            "https://api.test/socket",
            &[("sec-websocket-key", "x"), ("sec-websocket-protocol", "chat, v2")],
            b"",
        );
        ws.kind = EntryKind::WebSocket;
        ws.ws_messages.push(crate::entry::WsMessage {
            from_client: true,
            opcode: WsOpcode::Text,
            at: std::time::Duration::ZERO,
            size: 2,
            data: Bytes::from_static(b"hi"),
            truncated: false,
            compressed: false,
        });
        let item = ws.to_request_item(&ImportOptions::default()).unwrap();
        let RequestKind::WebSocket(w) = &item.request else { panic!("expected websocket") };
        assert_eq!(w.url, "wss://api.test/socket");
        assert_eq!(w.subprotocols, vec!["chat", "v2"]);
        assert!(w.headers.is_empty());
        assert_eq!(w.messages[0].body, "hi");
    }
}
