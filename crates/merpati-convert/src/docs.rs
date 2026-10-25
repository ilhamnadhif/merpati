//! Static documentation for a collection (PRD FR-D3).
//!
//! [`to_markdown`] and [`to_html_site`] document every folder and request:
//! description (Markdown), method and URL, auth type, parameter / header /
//! body tables, saved examples and code snippets. The HTML site is a single
//! self-contained `index.html` (inline CSS and JS, no external assets) with a
//! sidebar table of contents, client-side search and light/dark themes.
//!
//! Descriptions are rendered by [`markdown_to_html`], a small safe renderer
//! for a Markdown subset; anything else (raw HTML included) is escaped.
//!
//! Secrets never reach the output:
//! * secret-type collection/folder variables, variables referenced by
//!   sensitive auth parameters ([`AuthType::sensitive_params`]) and variables
//!   whose name looks like a credential (`token`, `password`, `apiKey`, ...)
//!   are never resolved; snippets keep their `{{name}}` placeholder;
//! * literal values of sensitive auth parameters, of credential-like headers,
//!   query/form fields and JSON body keys, and URL passwords are replaced by
//!   `REDACTED`;
//! * the auth summary only names the auth type.
//!
//! Variables of an environment or the globals carry no secret flag inside
//! [`VariableScopes`]; strip them with [`redact_secret_variables`] first.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use merpati_core::auth::{Auth, AuthType};
use merpati_core::model::{
    Ancestor, BodyMode, Collection, Example, FormFieldKind, HttpRequest, Item, KeyValue, RequestItem, RequestKind,
    effective_from_chain,
};
use merpati_core::prepared::{PrepareContext, PreparedRequest, prepare};
use merpati_core::protocols::{McpCallKind, McpTransport};
use merpati_core::settings::RequestDefaults;
use merpati_core::urlutil;
use merpati_core::variables::{Resolver, TokenKind, Variable, VariableScopes, is_pure_reference, references, tokenize};
use serde_json::Value;

use crate::codegen::{self, Language};

/// Settings for [`to_markdown`] and [`to_html_site`].
#[derive(Clone, Debug, PartialEq)]
pub struct DocsOptions {
    /// Values used to resolve `{{variables}}` in code snippets. Collection and
    /// folder variables of the documented collection are layered on top
    /// (keys already present in `variables.collection` win over the
    /// collection's own values).
    pub variables: VariableScopes,
    /// One snippet per language for every HTTP request. Empty = no snippets.
    pub snippet_languages: Vec<Language>,
    /// Page title; defaults to the collection name.
    pub title: Option<String>,
}

impl Default for DocsOptions {
    fn default() -> Self {
        DocsOptions { variables: VariableScopes::default(), snippet_languages: vec![Language::Curl], title: None }
    }
}

/// Placeholder for literal secret values.
pub const REDACTED: &str = "REDACTED";

/// Removes every secret-type variable of `vars` (an environment's values or
/// the globals) from all scopes, so its value cannot appear in snippets.
pub fn redact_secret_variables(scopes: &mut VariableScopes, vars: &[Variable]) {
    for v in vars.iter().filter(|v| v.is_secret()) {
        scopes.globals.shift_remove(&v.key);
        scopes.collection.shift_remove(&v.key);
        scopes.environment.shift_remove(&v.key);
        scopes.data.shift_remove(&v.key);
        scopes.local.shift_remove(&v.key);
    }
}

/// The whole documentation as one Markdown document.
pub fn to_markdown(c: &Collection, opts: &DocsOptions) -> String {
    render_markdown(&build(c, opts))
}

/// The documentation as a static site: currently one self-contained
/// `index.html` (paths are relative to the output directory).
pub fn to_html_site(c: &Collection, opts: &DocsOptions) -> Vec<(PathBuf, Vec<u8>)> {
    vec![(PathBuf::from("index.html"), render_html(&build(c, opts)).into_bytes())]
}

/// Writes [`to_html_site`] into `dir` (created if needed) and returns the
/// path of `index.html`.
pub fn write_html_site(c: &Collection, opts: &DocsOptions, dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for (rel, bytes) in to_html_site(c, opts) {
        let path = dir.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
    }
    Ok(dir.join("index.html"))
}

// ---------------------------------------------------------------------------
// Intermediate representation shared by both renderers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
struct Row {
    key: String,
    value: String,
    description: String,
}

#[derive(Clone, Debug, PartialEq)]
struct CodeBlock {
    caption: String,
    lang: String,
    text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct BodyDoc {
    mode: String,
    rows: Vec<Row>,
    code: Vec<CodeBlock>,
}

#[derive(Clone, Debug, PartialEq)]
struct ExampleDoc {
    name: String,
    status: u16,
    status_text: String,
    lang: String,
    body: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct RequestDoc {
    badge: String,
    protocol: &'static str,
    url: String,
    auth: String,
    details: Vec<(String, String)>,
    path_vars: Vec<Row>,
    params: Vec<Row>,
    headers: Vec<Row>,
    body: Option<BodyDoc>,
    examples: Vec<ExampleDoc>,
    snippets: Vec<CodeBlock>,
}

#[derive(Clone, Debug, PartialEq)]
enum EntryKind {
    Folder { auth: String },
    Request(Box<RequestDoc>),
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    /// 0 for top-level items.
    depth: usize,
    anchor: String,
    name: String,
    description: String,
    /// Lowercase text matched by the search box. A folder also stays
    /// visible when one of its descendants matches (resolved in the page).
    search: String,
    kind: EntryKind,
}

struct Doc {
    title: String,
    description: String,
    auth: String,
    variables: Vec<Row>,
    request_count: usize,
    entries: Vec<Entry>,
}

/// An HTTP request waiting for its snippets until every secret name is known.
struct Pending {
    entry: usize,
    request: HttpRequest,
    auth: Auth,
    folder_vars: IndexMap<String, String>,
}

// ---------------------------------------------------------------------------
// Secret handling
// ---------------------------------------------------------------------------

/// Lowercase alphanumerics only: `X-Api_Key` -> `xapikey`.
fn normalize_name(name: &str) -> String {
    name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// Header, field, JSON key or variable names whose values are credentials.
/// Also used by the OpenAPI export.
pub(crate) fn is_credential_name(name: &str) -> bool {
    let n = normalize_name(name);
    if n.is_empty() || n.ends_with("url") || n.ends_with("uri") {
        return false;
    }
    matches!(n.as_str(), "key" | "auth" | "pass" | "pwd" | "sig" | "signature" | "cookie" | "setcookie" | "session")
        || ["password", "passwd", "secret", "token", "apikey", "privatekey", "accesskey", "credential", "authorization"]
            .iter()
            .any(|w| n.contains(w))
}

/// True when `s` holds only `{{references}}` and whitespace.
fn only_references(s: &str) -> bool {
    !s.trim().is_empty()
        && tokenize(s).iter().all(|t| t.kind == TokenKind::Variable || s[t.range.clone()].trim().is_empty())
}

/// Masks a credential value, keeping `{{references}}` and a leading scheme
/// word (`Bearer {{token}}` stays, `Bearer abc` becomes `Bearer REDACTED`).
pub(crate) fn mask_value(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return raw.to_string();
    }
    let (scheme, rest) = match t.split_once(char::is_whitespace) {
        Some((s, r)) if s.chars().all(|c| c.is_ascii_alphabetic()) => (Some(s), r.trim()),
        _ => (None, t),
    };
    let rest = if only_references(rest) { rest.to_string() } else { REDACTED.to_string() };
    match scheme {
        Some(s) => format!("{s} {rest}"),
        None => rest,
    }
}

/// Masks values under credential-like keys anywhere in a JSON document (as
/// the docs do); true if something changed. For the OpenAPI export.
pub(crate) fn mask_credentials_json(v: &mut Value) -> bool {
    Redactor::default().mask_json(v)
}

#[derive(Default)]
struct Redactor {
    /// Variable names that must never be resolved.
    names: HashSet<String>,
}

impl Redactor {
    fn add_refs(&mut self, s: &str) {
        for r in references(s) {
            self.names.insert(r.to_string());
        }
    }

    fn mask(&mut self, raw: &str) -> String {
        self.add_refs(raw);
        mask_value(raw)
    }

    /// Masks the value when the key looks like a credential; true if changed.
    fn mask_kv(&mut self, kv: &mut KeyValue) -> bool {
        if !is_credential_name(&kv.key) {
            return false;
        }
        let masked = self.mask(&kv.value);
        let changed = masked != kv.value;
        kv.value = masked;
        changed
    }

    fn mask_kvs(&mut self, kvs: &mut [KeyValue]) -> bool {
        kvs.iter_mut().fold(false, |changed, kv| self.mask_kv(kv) | changed)
    }

    fn collect_vars(&mut self, vars: &[Variable]) {
        for v in vars.iter().filter(|v| v.is_secret() || is_credential_name(&v.key)) {
            self.names.insert(v.key.clone());
        }
    }

    fn collect_auth(&mut self, auth: &Auth) {
        for p in auth.kind.sensitive_params() {
            self.add_refs(&auth.get_string(p));
        }
    }

    /// Masks values under credential-like keys anywhere in a JSON document.
    fn mask_json(&mut self, v: &mut Value) -> bool {
        match v {
            Value::Object(map) => {
                let mut changed = false;
                for (k, child) in map.iter_mut() {
                    if is_credential_name(k) {
                        match child {
                            Value::String(s) if !s.is_empty() => {
                                let masked = self.mask(s);
                                changed |= masked != *s;
                                *s = masked;
                            }
                            Value::Number(_) => {
                                *child = Value::String(REDACTED.into());
                                changed = true;
                            }
                            _ => changed |= self.mask_json(child),
                        }
                    } else {
                        changed |= self.mask_json(child);
                    }
                }
                changed
            }
            Value::Array(items) => items.iter_mut().fold(false, |changed, x| self.mask_json(x) | changed),
            _ => false,
        }
    }

    /// Masks JSON text if it parses; the text is kept verbatim (formatting
    /// included) unless something had to be masked.
    fn mask_json_text(&mut self, text: &str) -> String {
        match serde_json::from_str::<Value>(text) {
            Ok(mut v) => {
                if self.mask_json(&mut v) {
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                } else {
                    text.to_string()
                }
            }
            Err(_) => text.to_string(),
        }
    }

    /// Replaces the password of `scheme://user:password@host`.
    fn mask_userinfo(&mut self, url: &str) -> String {
        let Some(scheme_end) = url.find("://").map(|i| i + 3) else { return url.to_string() };
        let rest = &url[scheme_end..];
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let Some(at) = authority.rfind('@') else { return url.to_string() };
        let Some(colon) = authority[..at].find(':') else { return url.to_string() };
        let password = &authority[colon + 1..at];
        let masked = self.mask(password);
        format!("{}{}:{masked}{}", &url[..scheme_end], &authority[..colon], &rest[at..])
    }

    /// Sensitive auth parameter for snippets: a pure reference stays (it is
    /// never resolved), anything else is redacted.
    fn auth_secret(&self, auth: &Auth, key: &str) -> String {
        let raw = auth.get_string(key);
        if is_pure_reference(&raw) { raw.trim().to_string() } else { REDACTED.to_string() }
    }
}

// ---------------------------------------------------------------------------
// Building the document
// ---------------------------------------------------------------------------

fn slugify(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
        if s.len() >= 60 {
            break;
        }
    }
    let s = s.trim_end_matches('-');
    if s.is_empty() { "item".to_string() } else { s.to_string() }
}

fn status_reason(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// Characters kept in a code block language class.
fn sanitize_lang(lang: &str) -> String {
    lang.chars().filter(|c| c.is_ascii_alphanumeric() || "-_+.#".contains(*c)).take(32).collect()
}

fn rows(kvs: &[KeyValue]) -> Vec<Row> {
    kvs.iter()
        .filter(|kv| kv.enabled && !kv.key.trim().is_empty())
        .map(|kv| Row { key: kv.key.clone(), value: kv.value.clone(), description: kv.description.clone() })
        .collect()
}

fn auth_summary(own: &Auth, chain: &[Ancestor<'_>]) -> String {
    if !own.is_inherit() {
        return own.kind.label().to_string();
    }
    match effective_from_chain(chain) {
        Some(e) if e.auth.kind != AuthType::None => {
            format!("{} (inherited from {})", e.auth.kind.label(), e.owner_name)
        }
        _ => AuthType::None.label().to_string(),
    }
}

fn effective_auth(own: &Auth, chain: &[Ancestor<'_>]) -> Auth {
    if !own.is_inherit() {
        return own.clone();
    }
    effective_from_chain(chain).map(|e| e.auth.clone()).unwrap_or_else(Auth::none)
}

struct Builder {
    red: Redactor,
    slugs: HashSet<String>,
    entries: Vec<Entry>,
    pending: Vec<Pending>,
}

impl Builder {
    fn anchor(&mut self, name: &str) -> String {
        let base = slugify(name);
        let mut slug = base.clone();
        let mut n = 2;
        while !self.slugs.insert(slug.clone()) {
            slug = format!("{base}-{n}");
            n += 1;
        }
        slug
    }

    fn walk<'a>(&mut self, items: &'a [Item], chain: &mut Vec<Ancestor<'a>>, depth: usize) {
        for item in items {
            match item {
                Item::Folder(f) => {
                    self.red.collect_vars(&f.variables);
                    self.red.collect_auth(&f.auth);
                    let anchor = self.anchor(&f.name);
                    let auth = auth_summary(&f.auth, chain);
                    self.entries.push(Entry {
                        depth,
                        anchor,
                        name: f.name.clone(),
                        description: f.description.clone(),
                        search: String::new(),
                        kind: EntryKind::Folder { auth },
                    });
                    chain.push(Ancestor::Folder(f));
                    self.walk(&f.items, chain, depth + 1);
                    chain.pop();
                }
                Item::Request(r) => self.request(r, chain, depth),
            }
        }
    }

    fn request(&mut self, r: &RequestItem, chain: &[Ancestor<'_>], depth: usize) {
        let anchor = self.anchor(&r.name);
        let own_auth = r.request.auth();
        self.red.collect_auth(own_auth);
        let mut doc = RequestDoc {
            badge: r.request.badge(),
            protocol: r.request.protocol_label(),
            auth: auth_summary(own_auth, chain),
            ..Default::default()
        };
        let red = &mut self.red;
        match &r.request {
            RequestKind::Http(h) => {
                let s = self.sanitize_http(h);
                doc.url = s.url.clone();
                doc.path_vars = rows(&s.path_vars);
                doc.params = rows(&s.params);
                doc.headers = rows(&s.headers);
                doc.body = body_doc(&s);
                let mut folder_vars = IndexMap::new();
                for a in chain {
                    if let Ancestor::Folder(f) = a {
                        for v in f.variables.iter().filter(|v| v.enabled && !v.key.is_empty() && !v.is_secret()) {
                            folder_vars.insert(v.key.clone(), v.effective().to_string());
                        }
                    }
                }
                self.pending.push(Pending {
                    entry: self.entries.len(),
                    request: s,
                    auth: effective_auth(own_auth, chain),
                    folder_vars,
                });
            }
            RequestKind::WebSocket(w) => {
                let mut params = w.params.clone();
                let mut url = w.url.clone();
                if red.mask_kvs(&mut params) {
                    url = urlutil::replace_query(&url, &params);
                }
                doc.url = red.mask_userinfo(&url);
                doc.params = rows(&params);
                let mut headers = w.headers.clone();
                red.mask_kvs(&mut headers);
                doc.headers = rows(&headers);
                if !w.subprotocols.is_empty() {
                    doc.details.push(("Subprotocols".into(), w.subprotocols.join(", ")));
                }
            }
            RequestKind::Grpc(g) => {
                doc.url = red.mask_userinfo(&g.url);
                doc.details.push(("Service".into(), g.service.clone()));
                doc.details.push(("Method".into(), g.method.clone()));
                let mut metadata = g.metadata.clone();
                red.mask_kvs(&mut metadata);
                doc.headers = rows(&metadata);
                if !g.message.trim().is_empty() {
                    doc.body = Some(BodyDoc {
                        mode: "message (JSON)".into(),
                        rows: Vec::new(),
                        code: vec![CodeBlock {
                            caption: String::new(),
                            lang: "json".into(),
                            text: red.mask_json_text(&g.message),
                        }],
                    });
                }
            }
            RequestKind::Mqtt(m) => {
                doc.url = red.mask_userinfo(&m.url);
                if !m.client_id.is_empty() {
                    doc.details.push(("Client ID".into(), m.client_id.clone()));
                }
                doc.params = m
                    .subscriptions
                    .iter()
                    .filter(|s| s.enabled)
                    .map(|s| Row { key: s.topic.clone(), value: format!("QoS {}", s.qos), description: String::new() })
                    .collect();
            }
            RequestKind::SocketIo(s) => {
                doc.url = red.mask_userinfo(&s.url);
                if !s.path.is_empty() {
                    doc.details.push(("Path".into(), s.path.clone()));
                }
                if !s.namespace.is_empty() {
                    doc.details.push(("Namespace".into(), s.namespace.clone()));
                }
                let (mut params, mut headers) = (s.params.clone(), s.headers.clone());
                red.mask_kvs(&mut params);
                red.mask_kvs(&mut headers);
                doc.params = rows(&params);
                doc.headers = rows(&headers);
            }
            RequestKind::Mcp(m) => {
                match &m.transport {
                    McpTransport::Http { url, headers } => {
                        doc.url = red.mask_userinfo(url);
                        doc.details.push(("Transport".into(), "Streamable HTTP".into()));
                        let mut headers = headers.clone();
                        red.mask_kvs(&mut headers);
                        doc.headers = rows(&headers);
                    }
                    McpTransport::Stdio { command, .. } => {
                        // Arguments and environment may carry secrets; only the program is shown.
                        doc.url = command.clone();
                        doc.details.push(("Transport".into(), "stdio".into()));
                    }
                }
                doc.params = m
                    .saved_calls
                    .iter()
                    .map(|c| {
                        let kind = match c.kind {
                            McpCallKind::Tool => "tool",
                            McpCallKind::Prompt => "prompt",
                            McpCallKind::Resource => "resource",
                        };
                        Row { key: c.name.clone(), value: format!("{kind} {}", c.target), description: String::new() }
                    })
                    .collect();
            }
            RequestKind::Ai(a) => {
                doc.url = if a.base_url.is_empty() { a.model.clone() } else { red.mask_userinfo(&a.base_url) };
                doc.details.push(("Provider".into(), a.provider.label().to_string()));
                doc.details.push(("Model".into(), a.model.clone()));
            }
        }
        doc.examples = r.examples.iter().map(|e| self.example(e)).collect();
        self.entries.push(Entry {
            depth,
            anchor,
            name: r.name.clone(),
            description: r.description.clone(),
            search: String::new(),
            kind: EntryKind::Request(Box::new(doc)),
        });
    }

    fn sanitize_http(&mut self, h: &HttpRequest) -> HttpRequest {
        let red = &mut self.red;
        let mut s = h.clone();
        if red.mask_kvs(&mut s.params) {
            s.url = urlutil::replace_query(&s.url, &s.params);
        }
        s.url = red.mask_userinfo(&s.url);
        red.mask_kvs(&mut s.path_vars);
        red.mask_kvs(&mut s.headers);
        red.mask_kvs(&mut s.body.urlencoded);
        for f in s.body.form_data.iter_mut().filter(|f| f.kind == FormFieldKind::Text && is_credential_name(&f.key)) {
            f.value = red.mask(&f.value);
        }
        s.body.raw = red.mask_json_text(&s.body.raw);
        s.body.graphql.variables = red.mask_json_text(&s.body.graphql.variables);
        s
    }

    fn example(&mut self, e: &Example) -> ExampleDoc {
        let lang = e.language.as_deref().unwrap_or("").trim().to_ascii_lowercase();
        let starts_json = matches!(e.body.trim_start().chars().next(), Some('{' | '['));
        let (lang, body) = match serde_json::from_str::<Value>(&e.body) {
            Ok(mut v) if lang == "json" || (lang.is_empty() && starts_json) => {
                self.red.mask_json(&mut v);
                ("json".to_string(), serde_json::to_string_pretty(&v).unwrap_or_default())
            }
            _ => (if lang.is_empty() { "text".into() } else { sanitize_lang(&lang) }, e.body.clone()),
        };
        let status_text =
            if e.status_text.trim().is_empty() { status_reason(e.status).to_string() } else { e.status_text.clone() };
        ExampleDoc { name: e.name.clone(), status: e.status, status_text, lang, body }
    }
}

fn body_doc(s: &HttpRequest) -> Option<BodyDoc> {
    let b = &s.body;
    let mode = b.mode.label().to_string();
    let doc = match b.mode {
        BodyMode::None => return None,
        BodyMode::Raw => BodyDoc {
            mode: format!("raw ({})", b.raw_language.label()),
            rows: Vec::new(),
            code: vec![CodeBlock {
                caption: String::new(),
                lang: b.raw_language.label().to_ascii_lowercase(),
                text: b.raw.clone(),
            }],
        },
        BodyMode::UrlEncoded => BodyDoc { mode, rows: rows(&b.urlencoded), code: Vec::new() },
        BodyMode::FormData => BodyDoc {
            mode,
            rows: b
                .form_data
                .iter()
                .filter(|f| f.enabled && !f.key.is_empty())
                .map(|f| Row {
                    key: f.key.clone(),
                    value: match f.kind {
                        FormFieldKind::Text => f.value.clone(),
                        FormFieldKind::File => {
                            let names: Vec<String> = f.files.iter().map(|p| file_name(p)).collect();
                            format!("[file] {}", names.join(", "))
                        }
                    },
                    description: f.description.clone(),
                })
                .collect(),
            code: Vec::new(),
        },
        BodyMode::Binary => BodyDoc {
            mode,
            rows: vec![Row { key: "file".into(), value: file_name(&b.binary_path), description: String::new() }],
            code: Vec::new(),
        },
        BodyMode::GraphQl => {
            let mut code =
                vec![CodeBlock { caption: "Query".into(), lang: "graphql".into(), text: b.graphql.query.clone() }];
            if !b.graphql.variables.trim().is_empty() {
                code.push(CodeBlock {
                    caption: "Variables".into(),
                    lang: "json".into(),
                    text: b.graphql.variables.clone(),
                });
            }
            BodyDoc { mode, rows: Vec::new(), code }
        }
    };
    Some(doc)
}

/// Only the file name: local paths say nothing useful to a reader.
fn file_name(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string())
}

/// Percent-encodes what `prepare` would, but leaves `{{references}}` intact.
fn encode_keeping_refs(url: &str) -> String {
    tokenize(url)
        .into_iter()
        .map(|t| match t.kind {
            TokenKind::Variable => url[t.range].to_string(),
            TokenKind::Text => urlutil::encode_loose(&url[t.range]),
        })
        .collect()
}

/// Adds the effective auth to the snippet with secrets as placeholders.
/// Schemes that need signing or a token exchange are only named in the
/// auth summary.
fn apply_auth(p: &mut PreparedRequest, auth: &Auth, red: &Redactor, resolver: &Resolver<'_>) {
    let set = |p: &mut PreparedRequest, name: &str, value: String| {
        if !name.trim().is_empty() && !p.has_header(name) {
            p.headers.push((name.to_string(), value));
        }
    };
    match auth.kind {
        AuthType::Bearer => set(p, "Authorization", format!("Bearer {}", red.auth_secret(auth, "token"))),
        AuthType::OAuth2 if auth.get("addTokenTo") != "queryParams" => {
            let prefix = resolver.resolve(auth.get("headerPrefix"));
            let prefix = if prefix.trim().is_empty() { "Bearer".to_string() } else { prefix };
            set(p, "Authorization", format!("{prefix} {}", red.auth_secret(auth, "accessToken")));
        }
        AuthType::Basic => {
            let user = resolver.resolve(&auth.get_string("username"));
            set(p, "Authorization", format!("Basic <base64({user}:{})>", red.auth_secret(auth, "password")));
        }
        AuthType::ApiKey => {
            let key = resolver.resolve(&auth.get_string("key"));
            let value = red.auth_secret(auth, "value");
            if key.trim().is_empty() {
                return;
            }
            if auth.get("in") == "query" {
                let (base, query, fragment) = urlutil::split(&p.url);
                let q = match query {
                    Some(q) if !q.is_empty() => format!("{q}&{key}={value}"),
                    _ => format!("{key}={value}"),
                };
                p.url = format!("{base}?{q}{fragment}");
            } else {
                set(p, &key, value);
            }
        }
        _ => {}
    }
}

fn snippets(p: &Pending, base: &VariableScopes, red: &Redactor, langs: &[Language]) -> Vec<CodeBlock> {
    if langs.is_empty() {
        return Vec::new();
    }
    let mut scopes = base.clone();
    for (k, v) in &p.folder_vars {
        scopes.collection.insert(k.clone(), v.clone());
    }
    // A self reference resolves to itself: secrets stay placeholders and
    // dynamic values (`{{$guid}}`) stay stable across runs.
    let pin = |scopes: &mut VariableScopes, name: &str| {
        scopes.local.insert(name.to_string(), format!("{{{{{name}}}}}"));
    };
    let json = serde_json::to_string(&p.request).unwrap_or_default();
    for name in references(&json).into_iter().filter(|n| n.starts_with('$')) {
        pin(&mut scopes, name);
    }
    for name in &red.names {
        pin(&mut scopes, name);
    }
    let mut req = p.request.clone();
    let encode = req.settings.encode_url.unwrap_or(true);
    req.settings.encode_url = Some(false);
    let defaults = RequestDefaults::default();
    let none = Auth::none();
    let resolver = Resolver::new(&scopes);
    let ctx = PrepareContext { resolver, auth: &none, auth_owner_id: None, defaults: &defaults, base_dir: None };
    let mut prepared = prepare(&req, &ctx);
    // `prepare` adds `http://` in front of an unresolved `{{baseUrl}}`.
    if prepared.url.starts_with("http://{{") && !req.url.trim_start().to_ascii_lowercase().starts_with("http://") {
        prepared.url.replace_range(..7, "");
    }
    if encode {
        prepared.url = encode_keeping_refs(&prepared.url);
    }
    apply_auth(&mut prepared, &p.auth, red, &resolver);
    let opts = codegen::Options { skip_default_headers: true };
    langs
        .iter()
        .map(|&l| CodeBlock {
            caption: l.label().to_string(),
            lang: l.syntax().to_string(),
            text: codegen::generate_with(l, &prepared, opts).trim_end().to_string(),
        })
        .collect()
}

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn build(c: &Collection, opts: &DocsOptions) -> Doc {
    let mut b = Builder { red: Redactor::default(), slugs: HashSet::new(), entries: Vec::new(), pending: Vec::new() };
    b.slugs.insert("overview".into());
    b.red.collect_vars(&c.variables);
    b.red.collect_auth(&c.auth);
    let scope_keys = opts
        .variables
        .globals
        .keys()
        .chain(opts.variables.collection.keys())
        .chain(opts.variables.environment.keys())
        .chain(opts.variables.data.keys())
        .chain(opts.variables.local.keys());
    for k in scope_keys.filter(|k| is_credential_name(k)) {
        b.red.names.insert(k.clone());
    }
    let mut chain = vec![Ancestor::Collection(c)];
    b.walk(&c.items, &mut chain, 0);

    let mut base = opts.variables.clone();
    for v in c.variables.iter().filter(|v| v.enabled && !v.key.is_empty() && !v.is_secret()) {
        base.collection.entry(v.key.clone()).or_insert_with(|| v.effective().to_string());
    }
    let Builder { red, mut entries, pending, .. } = b;
    for p in &pending {
        let code = snippets(p, &base, &red, &opts.snippet_languages);
        if let EntryKind::Request(doc) = &mut entries[p.entry].kind {
            doc.snippets = code;
        }
    }

    for e in &mut entries {
        let (badge, url) = match &e.kind {
            EntryKind::Request(d) => (d.badge.as_str(), d.url.as_str()),
            EntryKind::Folder { .. } => ("", ""),
        };
        e.search = format!("{} {badge} {url} {}", e.name, truncate_chars(&e.description, 500)).to_lowercase();
    }

    let variables = c
        .variables
        .iter()
        .filter(|v| v.enabled && !v.key.is_empty())
        .map(|v| Row {
            key: v.key.clone(),
            value: if v.is_secret() || red.names.contains(&v.key) { REDACTED.into() } else { v.value.clone() },
            description: v.description.clone(),
        })
        .collect();
    let title = opts
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| Some(c.name.clone()).filter(|n| !n.trim().is_empty()))
        .unwrap_or_else(|| "API Documentation".into());
    Doc {
        title,
        description: c.description.clone(),
        auth: auth_summary(&c.auth, &[]),
        variables,
        request_count: c.request_count(),
        entries,
    }
}

// ---------------------------------------------------------------------------
// Safe Markdown subset -> HTML
// ---------------------------------------------------------------------------

const MAX_BLOCK_NESTING: usize = 8;
const MAX_INLINE_NESTING: usize = 16;

/// Escapes text for HTML element content and attribute values.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    esc_into(s, &mut out);
    out
}

fn esc_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// Renders a Markdown subset safely: ATX headings, paragraphs, (nested)
/// bullet and numbered lists, block quotes, rules, fenced code blocks, code
/// spans, links (`http`, `https`, `mailto` and relative only), autolinks,
/// bold, italic and strikethrough. Everything else, raw HTML included, is
/// escaped; images become plain links so nothing is loaded.
pub fn markdown_to_html(md: &str) -> String {
    render_blocks(md, 0)
}

fn is_table_delimiter(line: &str) -> bool {
    line.starts_with('|') && line.contains('-') && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Cells of a `| a | b |` row (escaped pipes `\|` stay in the cell).
fn table_cells(row: &str) -> Vec<&str> {
    let row = row.trim().trim_start_matches('|');
    let row = row.strip_suffix('|').unwrap_or(row);
    let mut cells = Vec::new();
    let (mut start, mut prev) = (0, ' ');
    for (i, c) in row.char_indices() {
        if c == '|' && prev != '\\' {
            cells.push(row[start..i].trim());
            start = i + 1;
        }
        prev = c;
    }
    cells.push(row[start..].trim());
    cells
}

fn indent_width(line: &str) -> usize {
    let mut w = 0;
    for b in line.bytes() {
        match b {
            b' ' => w += 1,
            b'\t' => w += 4,
            _ => break,
        }
    }
    w
}

fn run_len(b: &[u8], i: usize, c: u8) -> usize {
    b[i..].iter().take_while(|&&x| x == c).count()
}

fn fence_open(t: &str) -> Option<(u8, usize, &str)> {
    let b = t.as_bytes();
    let ch = *b.first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let n = run_len(b, 0, ch);
    if n < 3 {
        return None;
    }
    let info = t[n..].trim();
    if ch == b'`' && info.contains('`') {
        return None;
    }
    Some((ch, n, info))
}

fn fence_close(t: &str, ch: u8, n: usize) -> bool {
    let m = run_len(t.as_bytes(), 0, ch);
    m >= n && t[m..].trim().is_empty()
}

fn atx_heading(t: &str) -> Option<(usize, &str)> {
    let level = run_len(t.as_bytes(), 0, b'#');
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &t[level..];
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let text = rest.trim();
    let stripped = text.trim_end_matches('#');
    let text = if stripped.is_empty() {
        ""
    } else if stripped.ends_with([' ', '\t']) {
        stripped.trim_end()
    } else {
        text
    };
    Some((level, text))
}

fn is_hr(t: &str) -> bool {
    let t = t.trim_end();
    let Some(c) = t.chars().next() else { return false };
    matches!(c, '-' | '*' | '_')
        && t.chars().filter(|&x| x == c).count() >= 3
        && t.chars().all(|x| x == c || x == ' ' || x == '\t')
}

struct ListItem<'a> {
    indent: usize,
    ordered: bool,
    start: u64,
    text: &'a str,
}

fn list_item(line: &str) -> Option<ListItem<'_>> {
    let indent = indent_width(line);
    let t = line.trim_start();
    let b = t.as_bytes();
    let marker_ok = |rest: &str| rest.is_empty() || rest.starts_with([' ', '\t']);
    if matches!(b.first(), Some(b'-' | b'*' | b'+')) {
        let rest = &t[1..];
        return marker_ok(rest).then(|| ListItem { indent, ordered: false, start: 1, text: rest.trim() });
    }
    let digits = run_digits(b);
    if (1..=9).contains(&digits) && matches!(b.get(digits), Some(b'.' | b')')) {
        let rest = &t[digits + 1..];
        if marker_ok(rest) {
            let start = t[..digits].parse().unwrap_or(1);
            return Some(ListItem { indent, ordered: true, start, text: rest.trim() });
        }
    }
    None
}

fn run_digits(b: &[u8]) -> usize {
    b.iter().take_while(|c| c.is_ascii_digit()).count()
}

fn starts_other_block(t: &str) -> bool {
    is_hr(t) || atx_heading(t).is_some() || fence_open(t).is_some() || t.starts_with('>')
}

fn flush_paragraph(out: &mut String, para: &mut Vec<&str>) {
    if para.is_empty() {
        return;
    }
    out.push_str("<p>");
    out.push_str(&inline(&para.join("\n")));
    out.push_str("</p>\n");
    para.clear();
}

fn render_blocks(md: &str, depth: usize) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut out = String::new();
    let mut para: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            flush_paragraph(&mut out, &mut para);
            i += 1;
            continue;
        }
        let indent = indent_width(line);
        let t = line.trim_start();
        if indent < 4 {
            if let Some((ch, n, info)) = fence_open(t) {
                flush_paragraph(&mut out, &mut para);
                let lang = sanitize_lang(info.split_whitespace().next().unwrap_or(""));
                let mut code = String::new();
                i += 1;
                while i < lines.len() {
                    let l = lines[i];
                    i += 1;
                    if indent_width(l) < 4 && fence_close(l.trim_start(), ch, n) {
                        break;
                    }
                    let strip = l.bytes().take(indent).take_while(|&c| c == b' ').count();
                    code.push_str(&l[strip..]);
                    code.push('\n');
                }
                if lang.is_empty() {
                    out.push_str("<pre><code>");
                } else {
                    let _ = write!(out, "<pre><code class=\"language-{lang}\">");
                }
                esc_into(&code, &mut out);
                out.push_str("</code></pre>\n");
                continue;
            }
            if let Some((level, text)) = atx_heading(t) {
                flush_paragraph(&mut out, &mut para);
                let _ = writeln!(out, "<h{level}>{}</h{level}>", inline(text));
                i += 1;
                continue;
            }
            if is_hr(t) {
                flush_paragraph(&mut out, &mut para);
                out.push_str("<hr>\n");
                i += 1;
                continue;
            }
            if t.starts_with('>') && depth < MAX_BLOCK_NESTING {
                flush_paragraph(&mut out, &mut para);
                let mut inner = Vec::new();
                while i < lines.len() && indent_width(lines[i]) < 4 && lines[i].trim_start().starts_with('>') {
                    let s = &lines[i].trim_start()[1..];
                    inner.push(s.strip_prefix(' ').unwrap_or(s));
                    i += 1;
                }
                out.push_str("<blockquote>\n");
                out.push_str(&render_blocks(&inner.join("\n"), depth + 1));
                out.push_str("</blockquote>\n");
                continue;
            }
            if list_item(line).is_some() {
                flush_paragraph(&mut out, &mut para);
                i = render_list(&lines, i, &mut out);
                continue;
            }
            // GFM table: a header row, a delimiter row, then body rows.
            if t.starts_with('|') && lines.get(i + 1).is_some_and(|l| is_table_delimiter(l.trim())) {
                flush_paragraph(&mut out, &mut para);
                out.push_str("<table>\n<thead><tr>");
                for cell in table_cells(t) {
                    let _ = write!(out, "<th>{}</th>", inline(cell));
                }
                out.push_str("</tr></thead>\n<tbody>\n");
                i += 2;
                while i < lines.len() && lines[i].trim_start().starts_with('|') {
                    out.push_str("<tr>");
                    for cell in table_cells(lines[i].trim()) {
                        let _ = write!(out, "<td>{}</td>", inline(cell));
                    }
                    out.push_str("</tr>\n");
                    i += 1;
                }
                out.push_str("</tbody>\n</table>\n");
                continue;
            }
        }
        para.push(t);
        i += 1;
    }
    flush_paragraph(&mut out, &mut para);
    out
}

struct Level {
    indent: usize,
    ordered: bool,
}

fn open_list(out: &mut String, item: &ListItem<'_>) {
    match (item.ordered, item.start) {
        (false, _) => out.push_str("<ul>\n"),
        (true, 1) => out.push_str("<ol>\n"),
        (true, n) => {
            let _ = writeln!(out, "<ol start=\"{n}\">");
        }
    }
}

fn close_list(out: &mut String, level: &Level) {
    out.push_str(if level.ordered { "</li>\n</ol>\n" } else { "</li>\n</ul>\n" });
}

/// Renders the list starting at `lines[start]`; returns the next line index.
fn render_list(lines: &[&str], start: usize, out: &mut String) -> usize {
    let mut stack: Vec<Level> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j < lines.len() && list_item(lines[j]).is_some() && !is_hr(lines[j].trim_start()) {
                i = j;
                continue;
            }
            break;
        }
        let t = line.trim_start();
        if indent_width(line) < 4 && starts_other_block(t) {
            break;
        }
        match list_item(line) {
            Some(item) => {
                while stack.last().is_some_and(|l| l.indent > item.indent) {
                    if let Some(l) = stack.pop() {
                        close_list(out, &l);
                    }
                }
                match stack.last() {
                    Some(top) if top.indent == item.indent && top.ordered == item.ordered => out.push_str("</li>\n"),
                    Some(top) if top.indent == item.indent => {
                        if let Some(l) = stack.pop() {
                            close_list(out, &l);
                        }
                        open_list(out, &item);
                        stack.push(Level { indent: item.indent, ordered: item.ordered });
                    }
                    _ => {
                        open_list(out, &item);
                        stack.push(Level { indent: item.indent, ordered: item.ordered });
                    }
                }
                out.push_str("<li>");
                out.push_str(&inline(item.text));
            }
            None => {
                out.push('\n');
                out.push_str(&inline(t));
            }
        }
        i += 1;
    }
    while let Some(l) = stack.pop() {
        close_list(out, &l);
    }
    i
}

fn inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    render_inline(s, 0, false, &mut out);
    out
}

/// Allows `http`, `https`, `mailto` and scheme-less (relative) URLs.
fn safe_url(url: &str) -> Option<String> {
    let cleaned: String = url.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect();
    if cleaned.is_empty() {
        return None;
    }
    if let Some(colon) = cleaned.find(':') {
        let scheme = &cleaned[..colon];
        if !scheme.contains(['/', '?', '#'])
            && !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "mailto")
        {
            return None;
        }
    }
    Some(cleaned)
}

/// `<https://...>` autolink at `i`; returns the URL and the index after `>`.
fn autolink(s: &str, i: usize) -> Option<(&str, usize)> {
    let rest = &s[i + 1..];
    let end = rest.find(|c: char| c == '>' || c == '<' || c.is_whitespace())?;
    if rest.as_bytes()[end] != b'>' {
        return None;
    }
    let url = &rest[..end];
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:"))
        .then_some((url, i + end + 2))
}

/// Index of the next run of exactly `n` backticks at or after `from`.
fn find_tick_run(b: &[u8], from: usize, n: usize) -> Option<usize> {
    let mut j = from;
    while j < b.len() {
        if b[j] == b'`' {
            let m = run_len(b, j, b'`');
            if m == n {
                return Some(j);
            }
            j += m;
        } else {
            j += 1;
        }
    }
    None
}

/// Start of the closing delimiter (`dl` chars of `c`) for emphasis opened
/// right before `from`.
fn find_closer(b: &[u8], from: usize, c: u8, dl: usize) -> Option<usize> {
    let underscore = c == b'_';
    let mut j = from;
    while j < b.len() {
        if b[j] == b'\\' {
            j += 2;
        } else if b[j] == c {
            let m = run_len(b, j, c);
            let prev_ok = j > from && !b[j - 1].is_ascii_whitespace();
            let next_ok = !underscore || b.get(j + m).is_none_or(|x| !x.is_ascii_alphanumeric());
            let len_ok = if dl == 1 { m == 1 } else { m >= dl };
            if prev_ok && next_ok && len_ok {
                return Some(j + m - dl);
            }
            j += m;
        } else {
            j += 1;
        }
    }
    None
}

struct Link<'a> {
    text: &'a str,
    dest: &'a str,
    title: Option<&'a str>,
    end: usize,
}

/// `[text](dest "title")` starting at `start` (which holds `[`).
fn parse_link(s: &str, start: usize) -> Option<Link<'_>> {
    let b = s.as_bytes();
    let limit = (start + 1000).min(b.len());
    let mut depth = 0usize;
    let mut j = start + 1;
    let text_end = loop {
        if j >= limit {
            return None;
        }
        match b[j] {
            b'\\' => {
                j += 2;
                continue;
            }
            b'[' => depth += 1,
            b']' if depth == 0 => break j,
            b']' => depth -= 1,
            _ => {}
        }
        j += 1;
    };
    if b.get(text_end + 1) != Some(&b'(') {
        return None;
    }
    let skip_ws = |mut k: usize| {
        while k < b.len() && b[k].is_ascii_whitespace() {
            k += 1;
        }
        k
    };
    let mut k = skip_ws(text_end + 2);
    let (dest_start, dest_end);
    if b.get(k) == Some(&b'<') {
        let rel = s[k + 1..].find(['>', '\n'])?;
        if b[k + 1 + rel] != b'>' {
            return None;
        }
        dest_start = k + 1;
        dest_end = k + 1 + rel;
        k = dest_end + 1;
    } else {
        dest_start = k;
        let mut parens = 0usize;
        loop {
            if k >= b.len() {
                k = b.len();
                break;
            }
            if k - dest_start > 2000 {
                return None;
            }
            match b[k] {
                b'\\' => {
                    k += 2;
                    continue;
                }
                b'(' => parens += 1,
                b')' if parens == 0 => break,
                b')' => parens -= 1,
                c if c.is_ascii_whitespace() || c.is_ascii_control() => break,
                _ => {}
            }
            k += 1;
        }
        dest_end = k;
    }
    k = skip_ws(k);
    let mut title = None;
    if let Some(&q) = b.get(k)
        && matches!(q, b'"' | b'\'')
    {
        let mut close = k + 1;
        while close < b.len() && b[close] != q {
            close += if b[close] == b'\\' { 2 } else { 1 };
        }
        if close >= b.len() {
            return None;
        }
        title = Some(&s[k + 1..close]);
        k = skip_ws(close + 1);
    }
    if b.get(k) != Some(&b')') {
        return None;
    }
    Some(Link { text: &s[start + 1..text_end], dest: &s[dest_start..dest_end], title, end: k + 1 })
}

/// Drops the backslash of `\\` + ASCII punctuation escapes.
fn unescape_punct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().is_some_and(char::is_ascii_punctuation) {
            continue;
        }
        out.push(c);
    }
    out
}

fn render_inline(s: &str, depth: usize, in_link: bool, out: &mut String) {
    let b = s.as_bytes();
    // Delimiters known to have no closer left: later searches would fail too.
    let mut dead: Vec<(u8, usize)> = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\\' if b.get(i + 1).is_some_and(u8::is_ascii_punctuation) => {
                esc_into(&s[text_start..i], out);
                esc_into(&s[i + 1..i + 2], out);
                i += 2;
                text_start = i;
            }
            b'`' => {
                let n = run_len(b, i, b'`');
                if !dead.contains(&(b'`', n)) {
                    if let Some(close) = find_tick_run(b, i + n, n) {
                        esc_into(&s[text_start..i], out);
                        let code = s[i + n..close].replace('\n', " ");
                        let code = match code.strip_prefix(' ').and_then(|c| c.strip_suffix(' ')) {
                            Some(inner) if !inner.trim().is_empty() => inner.to_string(),
                            _ => code,
                        };
                        out.push_str("<code>");
                        esc_into(&code, out);
                        out.push_str("</code>");
                        i = close + n;
                        text_start = i;
                        continue;
                    }
                    dead.push((b'`', n));
                }
                i += n;
            }
            b'*' | b'_' | b'~' if depth < MAX_INLINE_NESTING => {
                let n = run_len(b, i, c);
                let dl = if n >= 2 { 2 } else { 1 };
                let prev_alnum = i > 0 && b[i - 1].is_ascii_alphanumeric();
                let can_open = !(c == b'~' && dl == 1)
                    && b.get(i + dl).is_some_and(|x| !x.is_ascii_whitespace())
                    && !(c == b'_' && prev_alnum);
                if can_open && !dead.contains(&(c, dl)) {
                    if let Some(close) = find_closer(b, i + dl, c, dl) {
                        esc_into(&s[text_start..i], out);
                        let tag = match (c, dl) {
                            (b'~', _) => "del",
                            (_, 2) => "strong",
                            _ => "em",
                        };
                        let _ = write!(out, "<{tag}>");
                        render_inline(&s[i + dl..close], depth + 1, in_link, out);
                        let _ = write!(out, "</{tag}>");
                        i = close + dl;
                        text_start = i;
                        continue;
                    }
                    dead.push((c, dl));
                }
                i += n;
            }
            b'[' | b'!' if !in_link && depth < MAX_INLINE_NESTING => {
                let image = c == b'!';
                let open = if image { i + 1 } else { i };
                if b.get(open) != Some(&b'[') {
                    i += 1;
                    continue;
                }
                let Some(link) = parse_link(s, open) else {
                    i = open + 1;
                    continue;
                };
                esc_into(&s[text_start..i], out);
                let href = safe_url(link.dest);
                if let Some(href) = &href {
                    out.push_str("<a href=\"");
                    esc_into(href, out);
                    out.push('"');
                    if let Some(t) = link.title {
                        out.push_str(" title=\"");
                        esc_into(&unescape_punct(t), out);
                        out.push('"');
                    }
                    out.push_str(" rel=\"noopener noreferrer\">");
                }
                if image {
                    esc_into(link.text, out);
                } else {
                    render_inline(link.text, depth + 1, true, out);
                }
                if href.is_some() {
                    out.push_str("</a>");
                }
                i = link.end;
                text_start = i;
            }
            b'<' if !in_link => match autolink(s, i) {
                Some((url, end)) => {
                    esc_into(&s[text_start..i], out);
                    out.push_str("<a href=\"");
                    esc_into(url, out);
                    out.push_str("\" rel=\"noopener noreferrer\">");
                    esc_into(url, out);
                    out.push_str("</a>");
                    i = end;
                    text_start = i;
                }
                None => i += 1,
            },
            _ => i += 1,
        }
    }
    esc_into(&s[text_start..], out);
}

// ---------------------------------------------------------------------------
// HTML site
// ---------------------------------------------------------------------------

const CSS: &str = r#"
:root{--bg:#fff;--fg:#1f2328;--muted:#59636e;--border:#d1d9e0;--side:#f6f8fa;--code:#f6f8fa;--accent:#0969da;
--get:#1a7f37;--post:#9a6700;--put:#0969da;--patch:#8250df;--delete:#cf222e;--other:#59636e}
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--muted:#9198a1;--border:#3d444d;--side:#151b23;
--code:#151b23;--accent:#4493f8;--get:#3fb950;--post:#d29922;--put:#4493f8;--patch:#ab7df8;--delete:#f85149;--other:#9198a1}}
*{box-sizing:border-box}html{color-scheme:light dark}[hidden]{display:none!important}
body{margin:0;font:15px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;background:var(--bg);color:var(--fg)}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
aside{position:fixed;top:0;bottom:0;left:0;width:300px;overflow-y:auto;background:var(--side);border-right:1px solid var(--border);padding:16px}
aside .brand{font-weight:600;font-size:17px;margin-bottom:12px}aside .brand a{color:var(--fg)}
#q{width:100%;padding:6px 10px;border:1px solid var(--border);border-radius:6px;background:var(--bg);color:var(--fg);font:inherit;margin-bottom:12px}
nav ul{list-style:none;margin:0;padding-left:14px}nav>ul{padding-left:0}nav li{margin:2px 0}
nav a{color:var(--fg);display:block;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}nav .folder>a{font-weight:600}
main{margin-left:300px;padding:24px 40px 80px;max-width:1140px}
section{border-bottom:1px solid var(--border);padding:8px 0 24px}
h1,h2,h3,h4,h5,h6{line-height:1.3;margin:24px 0 8px}h1{font-size:30px}
.m{display:inline-block;min-width:56px;padding:1px 6px;margin-right:8px;border-radius:4px;font:600 12px/1.6 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
text-align:center;color:#fff;background:var(--other);vertical-align:middle}
.m-get{background:var(--get)}.m-post{background:var(--post)}.m-put{background:var(--put)}.m-patch{background:var(--patch)}.m-delete{background:var(--delete)}
nav .m{min-width:44px;font-size:10px;margin-right:6px}
.url{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;background:var(--code);border:1px solid var(--border);border-radius:6px;padding:6px 10px;word-break:break-all}
.meta{color:var(--muted)}.label{font-weight:600;margin:16px 0 6px}
code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:13px}
:not(pre)>code{background:var(--code);padding:1px 4px;border-radius:4px;word-break:break-word}
pre{background:var(--code);border:1px solid var(--border);border-radius:6px;padding:12px;overflow:auto;margin:6px 0 12px}
table{border-collapse:collapse;width:100%;margin:6px 0 12px;font-size:14px}th,td{border:1px solid var(--border);padding:4px 8px;text-align:left;vertical-align:top}
th{background:var(--side)}blockquote{margin:8px 0;padding:0 12px;border-left:4px solid var(--border);color:var(--muted)}
.status{font-weight:600;font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}.s2{color:var(--get)}.s3{color:var(--put)}.s4{color:var(--post)}.s5{color:var(--delete)}
@media (max-width:800px){aside{position:static;width:auto;border-right:0;border-bottom:1px solid var(--border)}main{margin-left:0;padding:16px}}
"#;

/// Search: walks the sidebar entries and the sections in reverse document
/// order so a folder (`data-d` = depth) stays visible when a descendant matches.
const JS: &str = r#"(function(){var q=document.getElementById("q");
function all(s){return [].slice.call(document.querySelectorAll(s)).reverse();}
var lists=[all("nav li[data-s]"),all("main section[data-s]")];
q.addEventListener("input",function(){var v=q.value.toLowerCase().trim();lists.forEach(function(l){var c=[];
l.forEach(function(e){var d=+e.dataset.d,h=!v||e.dataset.s.indexOf(v)>=0;for(var k=d+1;k<c.length;k++){h=h||c[k];}
c.length=d+1;c[d]=c[d]||h;e.hidden=!h;});});});})();"#;

const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:; \
                   base-uri 'none'; form-action 'none'";

fn method_class(badge: &str) -> &'static str {
    match badge.to_ascii_lowercase().as_str() {
        "get" => "m m-get",
        "post" => "m m-post",
        "put" => "m m-put",
        "patch" => "m m-patch",
        "delete" => "m m-delete",
        _ => "m",
    }
}

fn heading_level(depth: usize) -> usize {
    (depth + 2).min(6)
}

fn html_table(h: &mut String, title: &str, rows: &[Row], key: &str, value: &str) {
    if rows.is_empty() {
        return;
    }
    let with_desc = rows.iter().any(|r| !r.description.trim().is_empty());
    if !title.is_empty() {
        let _ = writeln!(h, "<div class=\"label\">{}</div>", esc(title));
    }
    let _ = write!(h, "<table><thead><tr><th>{key}</th><th>{value}</th>");
    if with_desc {
        h.push_str("<th>Description</th>");
    }
    h.push_str("</tr></thead><tbody>\n");
    for r in rows {
        let _ = write!(h, "<tr><td><code>{}</code></td><td><code>{}</code></td>", esc(&r.key), esc(&r.value));
        if with_desc {
            let _ = write!(h, "<td>{}</td>", inline(&r.description));
        }
        h.push_str("</tr>\n");
    }
    h.push_str("</tbody></table>\n");
}

fn html_code(h: &mut String, block: &CodeBlock) {
    if !block.caption.is_empty() {
        let _ = writeln!(h, "<div class=\"meta\">{}</div>", esc(&block.caption));
    }
    let lang = sanitize_lang(&block.lang);
    if lang.is_empty() {
        h.push_str("<pre><code>");
    } else {
        let _ = write!(h, "<pre><code class=\"language-{lang}\">");
    }
    esc_into(&block.text, h);
    h.push_str("</code></pre>\n");
}

fn html_request(h: &mut String, d: &RequestDoc) {
    let _ = writeln!(h, "<div class=\"url\">{}</div>", esc(if d.url.is_empty() { "(no URL)" } else { &d.url }));
    let _ = writeln!(h, "<p class=\"meta\">{} &middot; Auth: {}</p>", esc(d.protocol), esc(&d.auth));
    if !d.details.is_empty() {
        h.push_str("<table><tbody>\n");
        for (k, v) in &d.details {
            let _ = writeln!(h, "<tr><th>{}</th><td><code>{}</code></td></tr>", esc(k), esc(v));
        }
        h.push_str("</tbody></table>\n");
    }
    html_table(h, "Path variables", &d.path_vars, "Name", "Value");
    let params_title = match d.protocol {
        "MQTT" => "Subscriptions",
        "MCP" => "Saved calls",
        _ => "Query parameters",
    };
    html_table(h, params_title, &d.params, "Key", "Value");
    html_table(h, if d.protocol == "gRPC" { "Metadata" } else { "Headers" }, &d.headers, "Key", "Value");
    if let Some(body) = &d.body {
        let _ = writeln!(h, "<div class=\"label\">Body <span class=\"meta\">{}</span></div>", esc(&body.mode));
        html_table(h, "", &body.rows, "Key", "Value");
        for block in &body.code {
            html_code(h, block);
        }
    }
    if !d.examples.is_empty() {
        h.push_str("<div class=\"label\">Examples</div>\n");
        for ex in &d.examples {
            let _ = writeln!(
                h,
                "<div><span class=\"status s{}\">{} {}</span> {}</div>",
                ex.status / 100,
                ex.status,
                esc(&ex.status_text),
                esc(&ex.name)
            );
            if !ex.body.is_empty() {
                html_code(h, &CodeBlock { caption: String::new(), lang: ex.lang.clone(), text: ex.body.clone() });
            }
        }
    }
    if !d.snippets.is_empty() {
        h.push_str("<div class=\"label\">Code</div>\n");
        for s in &d.snippets {
            html_code(h, s);
        }
    }
}

fn html_toc(h: &mut String, entries: &[Entry]) {
    h.push_str("<ul>\n");
    let mut prev = 0;
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            if e.depth > prev {
                h.push_str("<ul>\n");
            } else {
                h.push_str("</li>\n");
                for _ in e.depth..prev {
                    h.push_str("</ul>\n</li>\n");
                }
            }
        }
        let (class, badge) = match &e.kind {
            EntryKind::Folder { .. } => ("folder", String::new()),
            EntryKind::Request(d) => {
                ("request", format!("<span class=\"{}\">{}</span>", method_class(&d.badge), esc(&d.badge)))
            }
        };
        let _ = write!(
            h,
            "<li class=\"{class}\" data-d=\"{}\" data-s=\"{}\"><a href=\"#{}\" title=\"{}\">{badge}{}</a>",
            e.depth,
            esc(&e.search),
            e.anchor,
            esc(&e.name),
            esc(&e.name)
        );
        prev = e.depth;
    }
    if !entries.is_empty() {
        h.push_str("</li>\n");
        for _ in 0..prev {
            h.push_str("</ul>\n</li>\n");
        }
    }
    h.push_str("</ul>\n");
}

fn render_html(doc: &Doc) -> String {
    let mut h = String::with_capacity(16 * 1024);
    let title = esc(&doc.title);
    h.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(h, "<meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">");
    let _ = writeln!(h, "<meta name=\"generator\" content=\"Merpati\">\n<title>{title}</title>");
    let _ = writeln!(h, "<style>{CSS}</style>\n</head>\n<body>");
    let _ = writeln!(h, "<aside>\n<div class=\"brand\"><a href=\"#overview\">{title}</a></div>");
    h.push_str("<input id=\"q\" type=\"search\" placeholder=\"Search requests\" aria-label=\"Search requests\" autocomplete=\"off\">\n<nav>\n");
    html_toc(&mut h, &doc.entries);
    h.push_str("</nav>\n</aside>\n<main>\n");

    let _ = writeln!(h, "<section id=\"overview\">\n<h1>{title}</h1>");
    let requests = if doc.request_count == 1 { "request" } else { "requests" };
    let _ = writeln!(h, "<p class=\"meta\">{} {requests} &middot; Auth: {}</p>", doc.request_count, esc(&doc.auth));
    if !doc.description.trim().is_empty() {
        let _ = writeln!(h, "<div class=\"desc\">\n{}</div>", markdown_to_html(&doc.description));
    }
    html_table(&mut h, "Variables", &doc.variables, "Key", "Value");
    h.push_str("</section>\n");

    for e in &doc.entries {
        let level = heading_level(e.depth);
        let (class, badge) = match &e.kind {
            EntryKind::Folder { .. } => ("folder", String::new()),
            EntryKind::Request(d) => {
                ("request", format!("<span class=\"{}\">{}</span>", method_class(&d.badge), esc(&d.badge)))
            }
        };
        let _ = writeln!(
            h,
            "<section id=\"{}\" class=\"{class}\" data-d=\"{}\" data-s=\"{}\">",
            e.anchor,
            e.depth,
            esc(&e.search)
        );
        let _ = writeln!(h, "<h{level}>{badge}{}</h{level}>", esc(&e.name));
        match &e.kind {
            EntryKind::Folder { auth } => {
                let _ = writeln!(h, "<p class=\"meta\">Folder &middot; Auth: {}</p>", esc(auth));
                if !e.description.trim().is_empty() {
                    let _ = writeln!(h, "<div class=\"desc\">\n{}</div>", markdown_to_html(&e.description));
                }
            }
            EntryKind::Request(d) => {
                if !e.description.trim().is_empty() {
                    let _ = writeln!(h, "<div class=\"desc\">\n{}</div>", markdown_to_html(&e.description));
                }
                html_request(&mut h, d);
            }
        }
        h.push_str("</section>\n");
    }
    let _ = writeln!(h, "</main>\n<script>{JS}</script>\n</body>\n</html>");
    h
}

// ---------------------------------------------------------------------------
// Markdown output
// ---------------------------------------------------------------------------

/// Escapes text so Markdown renders it literally (and never as HTML).
fn md_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '#' | '|' | '~' | '!' => {
                out.push('\\');
                out.push(c);
            }
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn longest_run(s: &str, c: char) -> usize {
    let (mut best, mut cur) = (0, 0);
    for x in s.chars() {
        cur = if x == c { cur + 1 } else { 0 };
        best = best.max(cur);
    }
    best
}

fn md_code(s: &str) -> String {
    let s = s.replace(['\n', '\r'], " ");
    if s.is_empty() {
        return String::new();
    }
    let fence = "`".repeat(longest_run(&s, '`') + 1);
    let pad = if s.starts_with('`') || s.ends_with('`') { " " } else { "" };
    format!("{fence}{pad}{s}{pad}{fence}")
}

fn md_fence(out: &mut String, lang: &str, text: &str) {
    let fence = "`".repeat(longest_run(text, '`').max(2) + 1);
    let _ = writeln!(out, "{fence}{}\n{}\n{fence}\n", sanitize_lang(lang), text.trim_end_matches(['\n', '\r']));
}

/// Neutralizes raw HTML and links to unsafe URLs in a Markdown description
/// (outside fenced code) so the Markdown output cannot inject markup or
/// `javascript:` links either (the HTML output filters both too).
fn md_description(md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 16);
    let mut fence: Option<(u8, usize)> = None;
    // Text lines are handled together: a link may span lines.
    let mut text: Vec<&str> = Vec::new();
    let flush = |text: &mut Vec<&str>, out: &mut String| {
        if !text.is_empty() {
            for line in defuse_links(&text.join("\n")).split('\n') {
                neutralize_line(line, out);
                out.push('\n');
            }
            text.clear();
        }
    };
    for line in md.lines() {
        let t = line.trim_start();
        match fence {
            Some((ch, n)) => {
                if fence_close(t, ch, n) {
                    fence = None;
                }
                out.push_str(line);
                out.push('\n');
            }
            None => match fence_open(t) {
                Some((ch, n, _)) => {
                    flush(&mut text, &mut out);
                    fence = Some((ch, n));
                    out.push_str(line);
                    out.push('\n');
                }
                None => text.push(line),
            },
        }
    }
    flush(&mut text, &mut out);
    if fence.is_some() {
        out.push_str("```\n");
    }
    out.trim_end().to_string()
}

/// Turns inline links and images (`[t](dest)`, `![a](dest)`) and link
/// reference definitions (`[ref]: dest`) whose destination is not a safe URL
/// into text by escaping their `]`. Code spans are not skipped (whether a
/// backtick opens one depends on the whole paragraph); inside one the added
/// backslash is merely visible.
fn defuse_links(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 8);
    let (mut i, mut start) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b']' => {
                if link_destination(text, i + 1).is_some_and(|dest| !md_dest_is_safe(dest)) {
                    out.push_str(&text[start..i]);
                    out.push_str("\\]");
                    start = i + 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out.push_str(&text[start.min(text.len())..]);
    out
}

/// The destination following a `]` at `pos`: after `(` (inline link) or `:`
/// (reference definition), spaces and at most one line break. Only its
/// scheme matters, so it simply runs to the next whitespace.
fn link_destination(s: &str, pos: usize) -> Option<&str> {
    let b = s.as_bytes();
    if !matches!(b.get(pos), Some(b'(' | b':')) {
        return None;
    }
    let mut k = pos + 1;
    let mut newline = false;
    while let Some(&c) = b.get(k) {
        match c {
            b' ' | b'\t' => k += 1,
            b'\n' if !newline => {
                newline = true;
                k += 1;
            }
            _ => break,
        }
    }
    let rest = &s[k..];
    if let Some(inner) = rest.strip_prefix('<') {
        return Some(&inner[..inner.find(['>', '\n']).unwrap_or(inner.len())]);
    }
    Some(&rest[..rest.find(char::is_whitespace).unwrap_or(rest.len())])
}

/// [`safe_url`] for a Markdown destination, which renderers unescape first:
/// backslash escapes (`javascript\:`) and entities (`javascript&colon;`), so
/// a `&` before the first `/`, `?` or `#` counts as unsafe.
fn md_dest_is_safe(dest: &str) -> bool {
    let d = unescape_punct(dest);
    let head = &d[..d.find(['/', '?', '#']).unwrap_or(d.len())];
    d.trim().is_empty() || (!head.contains('&') && safe_url(&d).is_some())
}

fn neutralize_line(line: &str, out: &mut String) {
    let b = line.as_bytes();
    let (mut i, mut start) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'`' => {
                let n = run_len(b, i, b'`');
                i = find_tick_run(b, i + n, n).map(|close| close + n).unwrap_or(i + n);
            }
            b'<' => match autolink(line, i) {
                Some((_, end)) => i = end,
                None => {
                    out.push_str(&line[start..i]);
                    out.push_str("&lt;");
                    i += 1;
                    start = i;
                }
            },
            _ => i += 1,
        }
    }
    out.push_str(&line[start.min(line.len())..]);
}

fn md_table(out: &mut String, title: &str, rows: &[Row], key: &str, value: &str) {
    if rows.is_empty() {
        return;
    }
    if !title.is_empty() {
        let _ = writeln!(out, "**{title}**\n");
    }
    let with_desc = rows.iter().any(|r| !r.description.trim().is_empty());
    let cell = |s: &str| md_code(s).replace('|', "\\|");
    if with_desc {
        let _ = writeln!(out, "| {key} | {value} | Description |\n| --- | --- | --- |");
    } else {
        let _ = writeln!(out, "| {key} | {value} |\n| --- | --- |");
    }
    for r in rows {
        if with_desc {
            let _ = writeln!(out, "| {} | {} | {} |", cell(&r.key), cell(&r.value), md_text(&r.description));
        } else {
            let _ = writeln!(out, "| {} | {} |", cell(&r.key), cell(&r.value));
        }
    }
    out.push('\n');
}

fn md_request(out: &mut String, d: &RequestDoc) {
    let url = if d.url.is_empty() { "(no URL)".to_string() } else { md_code(&d.url) };
    let _ = writeln!(out, "**{}** {url}\n", md_text(&d.badge));
    let _ = writeln!(out, "*{}* · Auth: {}\n", md_text(d.protocol), md_text(&d.auth));
    for (k, v) in &d.details {
        let _ = writeln!(out, "- {}: {}", md_text(k), md_code(v));
    }
    if !d.details.is_empty() {
        out.push('\n');
    }
    md_table(out, "Path variables", &d.path_vars, "Name", "Value");
    let params_title = match d.protocol {
        "MQTT" => "Subscriptions",
        "MCP" => "Saved calls",
        _ => "Query parameters",
    };
    md_table(out, params_title, &d.params, "Key", "Value");
    md_table(out, if d.protocol == "gRPC" { "Metadata" } else { "Headers" }, &d.headers, "Key", "Value");
    if let Some(body) = &d.body {
        let _ = writeln!(out, "**Body** · {}\n", md_text(&body.mode));
        md_table(out, "", &body.rows, "Key", "Value");
        for block in &body.code {
            if !block.caption.is_empty() {
                let _ = writeln!(out, "*{}*\n", md_text(&block.caption));
            }
            md_fence(out, &block.lang, &block.text);
        }
    }
    if !d.examples.is_empty() {
        out.push_str("**Examples**\n\n");
        for ex in &d.examples {
            let _ = writeln!(
                out,
                "- {} {} — {}\n",
                md_code(&ex.status.to_string()),
                md_text(&ex.status_text),
                md_text(&ex.name)
            );
            if !ex.body.is_empty() {
                md_fence(out, &ex.lang, &ex.body);
            }
        }
    }
    for s in &d.snippets {
        let _ = writeln!(out, "**{}**\n", md_text(&s.caption));
        md_fence(out, &s.lang, &s.text);
    }
}

fn render_markdown(doc: &Doc) -> String {
    let mut out = String::with_capacity(16 * 1024);
    let _ = writeln!(out, "# {}\n", md_text(&doc.title));
    if !doc.description.trim().is_empty() {
        let _ = writeln!(out, "{}\n", md_description(&doc.description));
    }
    let requests = if doc.request_count == 1 { "request" } else { "requests" };
    let _ = writeln!(out, "*{} {requests}* · Auth: {}\n", doc.request_count, md_text(&doc.auth));
    md_table(&mut out, "Variables", &doc.variables, "Key", "Value");
    if !doc.entries.is_empty() {
        out.push_str("## Contents\n\n");
        for e in &doc.entries {
            let badge = match &e.kind {
                EntryKind::Request(d) => format!("{} ", md_code(&d.badge)),
                EntryKind::Folder { .. } => String::new(),
            };
            let _ = writeln!(out, "{}- {badge}[{}](#{})", "  ".repeat(e.depth), md_text(&e.name), e.anchor);
        }
        out.push('\n');
    }
    for e in &doc.entries {
        let hashes = "#".repeat(heading_level(e.depth));
        let _ = writeln!(out, "<a id=\"{}\"></a>\n\n{hashes} {}\n", e.anchor, md_text(&e.name));
        match &e.kind {
            EntryKind::Folder { auth } => {
                let _ = writeln!(out, "*Folder* · Auth: {}\n", md_text(auth));
                if !e.description.trim().is_empty() {
                    let _ = writeln!(out, "{}\n", md_description(&e.description));
                }
            }
            EntryKind::Request(d) => {
                if !e.description.trim().is_empty() {
                    let _ = writeln!(out, "{}\n", md_description(&e.description));
                }
                md_request(&mut out, d);
            }
        }
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use merpati_core::model::{Body, Folder, FormField, RawLanguage, WsRequest};
    use merpati_core::protocols::GrpcRequest;

    use super::*;

    fn html_of(c: &Collection, opts: &DocsOptions) -> String {
        let files = to_html_site(c, opts);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, PathBuf::from("index.html"));
        String::from_utf8(files[0].1.clone()).unwrap()
    }

    fn sample() -> Collection {
        let mut c = Collection::new("Pet API");
        c.description = "Manage **pets**.\n\n- list\n- create".into();
        c.variables.push(Variable::new("baseUrl", "https://api.pets.test"));
        c.auth = Auth::bearer("{{token}}");
        let mut users = Folder::new("Users");
        users.description = "User endpoints".into();
        let mut get = RequestItem::new("Get user", HttpRequest::new("GET", "{{baseUrl}}/users/:id?verbose=true"));
        get.http_mut().unwrap().path_vars = vec![KeyValue::new("id", "42")];
        get.examples.push(Example {
            name: "Found".into(),
            status: 200,
            status_text: String::new(),
            body: "{\"id\":42,\"name\":\"Rex\"}".into(),
            ..Default::default()
        });
        users.items.push(get.into());
        let mut admin = Folder::new("Admin");
        let mut del = RequestItem::new("Delete user", HttpRequest::new("DELETE", "{{baseUrl}}/users/1"));
        del.http_mut().unwrap().auth = Auth::basic("root", "{{rootPassword}}");
        admin.items.push(del.into());
        users.items.push(admin.into());
        c.items.push(users.into());
        let mut create = RequestItem::new("Create pet", HttpRequest::new("POST", "{{baseUrl}}/pets"));
        create.http_mut().unwrap().body = Body::raw_json("{\"name\": \"Rex\", \"id\": \"{{$guid}}\"}");
        c.items.push(create.into());
        c.items.push(
            RequestItem {
                request: RequestKind::WebSocket(WsRequest {
                    url: "wss://ws.pets.test/live".into(),
                    ..Default::default()
                }),
                ..RequestItem::new("Live feed", HttpRequest::default())
            }
            .into(),
        );
        c.items.push(
            RequestItem {
                request: RequestKind::Grpc(GrpcRequest {
                    url: "grpcs://grpc.pets.test".into(),
                    service: "pets.Pets".into(),
                    method: "List".into(),
                    ..Default::default()
                }),
                ..RequestItem::new("List via gRPC", HttpRequest::default())
            }
            .into(),
        );
        // Duplicate name: anchors must stay unique.
        c.items.push(RequestItem::new("Get user", HttpRequest::new("GET", "{{baseUrl}}/me")).into());
        c
    }

    #[test]
    fn every_request_and_folder_appears() {
        let c = sample();
        let opts = DocsOptions::default();
        let md = to_markdown(&c, &opts);
        let html = html_of(&c, &opts);
        for r in c.requests() {
            assert!(md.contains(&md_text(&r.name)), "markdown misses {}", r.name);
            assert!(html.contains(&esc(&r.name)), "html misses {}", r.name);
        }
        for name in ["Users", "Admin"] {
            assert!(md.contains(&format!(" {name}\n")));
            assert!(html.contains(&format!(">{name}</h")));
        }
        // Six requests + two folders, all linked from the sidebar with unique anchors.
        let doc = build(&c, &opts);
        assert_eq!(doc.entries.len(), 8);
        let anchors: HashSet<_> = doc.entries.iter().map(|e| e.anchor.clone()).collect();
        assert_eq!(anchors.len(), 8);
        assert!(anchors.contains("get-user") && anchors.contains("get-user-2"));
        for a in &anchors {
            assert!(html.contains(&format!("href=\"#{a}\"")), "toc misses {a}");
            assert!(html.contains(&format!("id=\"{a}\"")));
            assert!(md.contains(&format!("](#{a})")) && md.contains(&format!("<a id=\"{a}\"></a>")));
        }
        // Nested headings follow the folder depth.
        assert!(html.contains("<h2>Users</h2>") && html.contains("<h3>Admin</h3>"));
        assert!(md.contains("\n## Users\n") && md.contains("\n### Admin\n") && md.contains("\n#### Delete user\n"));
        // Balanced sidebar lists.
        assert_eq!(html.matches("<ul>").count(), html.matches("</ul>").count());
        assert_eq!(html.matches("<li").count(), html.matches("</li>").count());
        // Non-HTTP details.
        assert!(html.contains("wss://ws.pets.test/live") && html.contains("pets.Pets"));
        assert!(md.contains("6 requests"));
    }

    #[test]
    fn page_is_self_contained_with_search_and_themes() {
        let html = html_of(&sample(), &DocsOptions { title: Some("Pets <Docs>".into()), ..Default::default() });
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<title>Pets &lt;Docs&gt;</title>"));
        assert!(html.contains("prefers-color-scheme:dark"));
        assert!(html.contains("id=\"q\"") && html.contains("data-s=\"") && html.contains("addEventListener"));
        assert!(!html.contains("src=\"http") && !html.contains("<link") && !html.contains("@import"));
        assert_eq!(html.matches("<script>").count(), 1);
    }

    #[test]
    fn details_tables_examples_and_snippets() {
        let c = sample();
        let mut opts = DocsOptions::default();
        opts.snippet_languages.push(Language::Python);
        opts.variables.environment.insert("token".into(), "env-secret-token".into());
        let md = to_markdown(&c, &opts);
        let html = html_of(&c, &opts);
        // Raw URL and method.
        assert!(md.contains("**GET** `{{baseUrl}}/users/:id?verbose=true`"));
        assert!(html.contains("<span class=\"m m-get\">GET</span>"));
        // Tables.
        assert!(md.contains("| `verbose` | `true` |") && md.contains("| `id` | `42` |"));
        assert!(html.contains("<td><code>verbose</code></td><td><code>true</code></td>"));
        // Auth summary names only the type, with inheritance.
        assert!(md.contains("Auth: Bearer Token (inherited from Pet API)"));
        assert!(md.contains("Auth: Basic Auth"));
        // Example pretty-printed with its reason phrase.
        assert!(md.contains("`200` OK — Found"));
        assert!(md.contains("{\n  \"id\": 42,\n  \"name\": \"Rex\"\n}"));
        // Snippets: collection variable resolved, dynamic and secret values kept as placeholders.
        assert!(md.contains("```bash\ncurl --location 'https://api.pets.test/users/42?verbose=true'"), "{md}");
        assert!(md.contains("Authorization: Bearer {{token}}"));
        assert!(md.contains("{{$guid}}"));
        assert!(md.contains("Python - Requests") && md.contains("import requests"));
        assert!(md.contains("Basic <base64(root:{{rootPassword}})>"));
        assert!(!md.contains("env-secret-token") && !html.contains("env-secret-token"));
        assert!(!md.contains("User-Agent"), "default headers are left out");
        // Unresolved base URL stays readable (not percent-encoded, no added scheme).
        let mut bare = Collection::new("Bare");
        bare.items.push(RequestItem::new("Ping", HttpRequest::new("GET", "{{host}}/ping me")).into());
        let md = to_markdown(&bare, &DocsOptions::default());
        assert!(md.contains("curl --location '{{host}}/ping%20me'"), "{md}");
    }

    #[test]
    fn secrets_never_appear() {
        let mut c = Collection::new("Secrets");
        c.variables.push(Variable::secret("apiSecret", "var-secret-1"));
        c.variables.push(Variable::new("clientSecret", "named-secret-2"));
        c.variables.push(Variable::new("baseUrl", "https://h.test"));
        c.auth = Auth::bearer("literal-bearer-3");
        let mut f = Folder::new("F");
        f.variables.push(Variable::secret("folderKey", "folder-secret-4"));
        f.auth = Auth::api_key("X-Api-Key", "apikey-literal-5", false);
        let mut r = RequestItem::new(
            "Login",
            HttpRequest::new("POST", "https://user:url-pass-6@h.test/login?api_key=query-secret-7&page=1"),
        );
        {
            let h = r.http_mut().unwrap();
            h.headers.push(KeyValue::new("Authorization", "Bearer header-secret-8"));
            h.headers.push(KeyValue::new("X-Trace", "{{apiSecret}}-{{folderKey}}-{{clientSecret}}"));
            h.headers.push(KeyValue::new("Cookie", "sid=cookie-secret-9"));
            h.body =
                Body::raw_json("{\"user\": \"ada\", \"password\": \"body-secret-10\", \"nested\": {\"token\": 11}}");
            h.auth = Auth::basic("ada", "basic-secret-12");
        }
        r.examples.push(Example {
            name: "OK".into(),
            body: "{\"access_token\": \"example-secret-13\", \"expires\": 3600}".into(),
            ..Default::default()
        });
        f.items.push(r.into());
        let mut form = RequestItem::new("Form", HttpRequest::new("POST", "{{baseUrl}}/form"));
        {
            let h = form.http_mut().unwrap();
            h.body.mode = BodyMode::FormData;
            h.body.form_data = vec![FormField::text("password", "form-secret-14"), FormField::text("name", "ada")];
            h.auth = Auth::new(AuthType::OAuth2).with("accessToken", "oauth-secret-15");
        }
        f.items.push(form.into());
        let mut enc = RequestItem::new("Urlencoded", HttpRequest::new("POST", "{{baseUrl}}/token"));
        {
            let h = enc.http_mut().unwrap();
            h.body.mode = BodyMode::UrlEncoded;
            h.body.urlencoded = vec![KeyValue::new("client_secret", "urlenc-secret-16")];
            h.auth = Auth::bearer("{{envToken}}");
        }
        f.items.push(enc.into());
        c.items.push(f.into());

        let mut opts = DocsOptions { snippet_languages: Language::ALL.to_vec(), ..Default::default() };
        opts.variables.environment.insert("envToken".into(), "env-secret-17".into());
        opts.variables.environment.insert("apiSecret".into(), "env-shadow-18".into());
        opts.variables.globals.insert("password".into(), "global-secret-19".into());
        let mut env = vec![Variable::secret("vaulted", "env-secret-20")];
        env.push(Variable::new("plain", "visible"));
        opts.variables.environment.insert("vaulted".into(), "env-secret-20".into());
        opts.variables.environment.insert("plain".into(), "visible".into());
        redact_secret_variables(&mut opts.variables, &env);
        assert!(!opts.variables.environment.contains_key("vaulted"));

        let md = to_markdown(&c, &opts);
        let html = html_of(&c, &opts);
        let secrets = [
            "var-secret-1",
            "named-secret-2",
            "literal-bearer-3",
            "folder-secret-4",
            "apikey-literal-5",
            "url-pass-6",
            "query-secret-7",
            "header-secret-8",
            "cookie-secret-9",
            "body-secret-10",
            "basic-secret-12",
            "example-secret-13",
            "form-secret-14",
            "oauth-secret-15",
            "urlenc-secret-16",
            "env-secret-17",
            "env-shadow-18",
            "global-secret-19",
            "env-secret-20",
        ];
        for s in secrets {
            assert!(!md.contains(s), "markdown leaks {s}:\n{md}");
            assert!(!html.contains(s), "html leaks {s}");
        }
        // Placeholders and non-secret values are still there.
        assert!(md.contains("Bearer REDACTED") && md.contains("{{envToken}}") && md.contains("https://h.test/form"));
        assert!(md.contains("{{apiSecret}}-{{folderKey}}-{{clientSecret}}"));
        assert!(md.contains("page=1") && md.contains("\"user\": \"ada\"") && md.contains("\"expires\": 3600"));
        assert!(md.contains("Auth: API Key") && md.contains("Auth: OAuth 2.0"));
    }

    #[test]
    fn html_escaping_blocks_xss() {
        let mut c = Collection::new("X <b>");
        c.description = "<script>alert('c')</script>".into();
        let mut r = RequestItem::new(
            "<img src=x onerror=alert(1)>",
            HttpRequest::new("GET", "https://h.test/\"><script>alert(2)</script>"),
        );
        r.description = "Hi <script>alert(1)</script> [x](javascript:alert(3)) [y](JaVa\tScRiPt:alert(4)) \
                         [ok](https://ok.test \"t\\\"q\") <iframe src=//evil.test> ![img](https://img.test/a.png)"
            .into();
        r.http_mut()
            .unwrap()
            .headers
            .push(KeyValue { description: "<b onmouseover=alert(5)>".into(), ..KeyValue::new("X-A\"b", "v<'\">") });
        r.examples.push(Example {
            name: "</pre><script>alert(6)</script>".into(),
            body: "<x>".into(),
            ..Default::default()
        });
        c.items.push(r.into());
        let mut f = Folder::new("F\" onclick=\"alert(7)");
        f.description = "```\n</code></pre><script>alert(8)</script>\n```".into();
        c.items.push(f.into());

        let html = html_of(&c, &DocsOptions::default());
        assert_eq!(html.matches("<script>").count(), 1, "only the search script");
        for bad in ["<img", "<iframe", "<b ", "<b>", "onclick=\"", "<x>", "\"><script"] {
            assert!(!html.contains(bad), "{bad} leaked");
        }
        // Every link points to a safe target.
        for (pos, _) in html.match_indices("href=\"") {
            let target = &html[pos + 6..];
            assert!(target.starts_with('#') || target.starts_with("https://"), "unsafe href: {}", &target[..40]);
        }
        assert!(html.contains("Hi &lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(html.contains("<a href=\"https://ok.test\" title=\"t&quot;q\" rel=\"noopener noreferrer\">ok</a>"));
        assert!(html.contains("<a href=\"https://img.test/a.png\" rel=\"noopener noreferrer\">img</a>"));
        assert!(html.contains("&lt;/code&gt;&lt;/pre&gt;&lt;script&gt;alert(8)"));

        let md = to_markdown(&c, &DocsOptions::default());
        assert!(md.contains("&lt;script>alert('c')&lt;/script>") && md.contains("Hi &lt;script>alert(1)"));
        assert!(md.contains("&lt;iframe") && md.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(!md.contains("<iframe") && !md.contains("<img") && !md.contains("Hi <script"), "{md}");
        // Code (fences, code spans) stays verbatim: Markdown renderers escape it.
        assert!(md.contains("```\n</code></pre><script>alert(8)</script>\n```"));
        assert!(md.contains("`https://h.test/\"><script>alert(2)</script>`"));
        assert!(md.contains("[ok](https://ok.test \"t\\\"q\")"), "{md}");
        assert!(md.contains("![img](https://img.test/a.png)"), "{md}");
        // Besides the table of contents, only the safe links are live (`[y](JaVa\tScRiPt:...)` is no link:
        // a destination cannot hold a tab).
        let targets: Vec<String> = live_link_targets(&md).into_iter().filter(|t| !t.starts_with('#')).collect();
        assert_eq!(targets, ["JaVa", "https://ok.test", "https://img.test/a.png"], "{md}");
    }

    /// The destinations a Markdown renderer would link: after an unescaped
    /// `](` or `]:`, spaces and one line break skipped.
    fn live_link_targets(md: &str) -> Vec<String> {
        let b = md.as_bytes();
        let mut out = Vec::new();
        for i in 0..b.len() {
            let escaped = b[..i].iter().rev().take_while(|&&c| c == b'\\').count() % 2 == 1;
            if b[i] == b']'
                && !escaped
                && let Some(dest) = link_destination(md, i + 1)
            {
                out.push(dest.split(')').next().unwrap_or_default().to_string());
            }
        }
        out
    }

    #[test]
    fn markdown_output_drops_unsafe_links() {
        let mut c = Collection::new("Links");
        c.description = "[a](javascript:alert(1)) [b](<JaVaScRiPt:alert(2)>) ![i](data:text/html;base64,PHNjcmlwdD4=) \
                         [c](vbscript:msgbox) [d](javascript&colon;alert(5)) [e](javascript\\:alert(6)) \
                         [h](\n  javascript:alert(9)) [n\nm](javascript:alert(10)) `[k](javascript:alert(11))`\n\
                         \n[f]: javascript:alert(7)\n[g]:\n  data:text/html,x\n\nuse [f] and [g]; \
                         [ok](https://ok.test) [rel](docs/intro.md) [mail](mailto:a@b.test) [top](#top) [q](?x=1&y=2)"
            .into();
        let md = to_markdown(&c, &DocsOptions::default());
        assert_eq!(
            live_link_targets(&md),
            ["https://ok.test", "docs/intro.md", "mailto:a@b.test", "#top", "?x=1&y=2"],
            "{md}"
        );
        // The link text is still there, as text.
        assert!(md.contains("[a\\](javascript:alert(1))") && md.contains("[n\nm\\](javascript"), "{md}");
        // The HTML output agrees (it keeps `&` literal: `javascript&amp;colon;` is a relative URL there).
        let html = html_of(&c, &DocsOptions::default());
        for bad in ["href=\"javascript:", "href=\"data:", "href=\"vbscript:", "href=\"JaVaScRiPt:"] {
            assert!(!html.contains(bad), "{bad}");
        }
    }

    #[test]
    fn markdown_subset_rendering() {
        let md = "# Title #\n\nSome *em*, **strong**, ~~gone~~, `a < b` and \\*not em\\*.\n\
                  snake_case_name stays; [link](https://x.test/a_(b)) <https://auto.test>\n\n\
                  - one\n- two\n  - nested **b**\n- three\n\n\
                  3. c\n4. d\n\n\
                  > quoted *q*\n\n\
                  ---\n\n\
                  ```json\n{\"a\": \"<b>\"}\n```\n\n\
                  ###### Six\n####### seven";
        let h = markdown_to_html(md);
        assert!(h.contains("<h1>Title</h1>"));
        assert!(h.contains("<em>em</em>") && h.contains("<strong>strong</strong>") && h.contains("<del>gone</del>"));
        assert!(h.contains("<code>a &lt; b</code>") && h.contains("*not em*"));
        assert!(h.contains("snake_case_name stays"));
        assert!(h.contains("<a href=\"https://x.test/a_(b)\" rel=\"noopener noreferrer\">link</a>"));
        assert!(h.contains("<a href=\"https://auto.test\" rel=\"noopener noreferrer\">https://auto.test</a>"));
        assert!(h.contains("<ul>\n<li>one</li>\n<li>two<ul>\n<li>nested <strong>b</strong></li>\n</ul>\n</li>\n<li>three</li>\n</ul>"), "{h}");
        assert!(h.contains("<ol start=\"3\">\n<li>c</li>\n<li>d</li>\n</ol>"));
        assert!(h.contains("<blockquote>\n<p>quoted <em>q</em></p>\n</blockquote>"));
        assert!(h.contains("<hr>"));
        assert!(h.contains("<pre><code class=\"language-json\">{&quot;a&quot;: &quot;&lt;b&gt;&quot;}\n</code></pre>"));
        assert!(h.contains("<h6>Six</h6>") && h.contains("<p>####### seven</p>"));
        assert!(h.contains("<p>Some <em>em</em>"));
        assert_eq!(markdown_to_html("***both***"), "<p><strong><em>both</em></strong></p>\n");
        assert_eq!(markdown_to_html("a * b * c"), "<p>a * b * c</p>\n");
        assert_eq!(markdown_to_html("[no link]"), "<p>[no link]</p>\n");
        assert_eq!(markdown_to_html("[t](javascript:x)"), "<p>t</p>\n");
        assert_eq!(markdown_to_html("[t](data:text/html,x)"), "<p>t</p>\n");
        assert_eq!(
            markdown_to_html("[rel](/docs#a)"),
            "<p><a href=\"/docs#a\" rel=\"noopener noreferrer\">rel</a></p>\n"
        );
        assert_eq!(markdown_to_html("x <b>y</b> & z"), "<p>x &lt;b&gt;y&lt;/b&gt; &amp; z</p>\n");
    }

    #[test]
    fn pathological_input_is_fast_and_safe() {
        let inputs = [
            "*".repeat(20_000),
            "_a".repeat(10_000),
            "[".repeat(20_000),
            "`".repeat(5_000) + &"x`".repeat(5_000),
            ">".repeat(5_000) + " deep",
            "- ".repeat(3_000),
            "\\é\\".repeat(3_000) + "[é](é" + &"(".repeat(3_000),
            (0..2_000).map(|i| format!("{}- item\n", " ".repeat(i % 40))).collect(),
            "**a *b **c *d".repeat(2_000),
        ];
        let started = std::time::Instant::now();
        for input in &inputs {
            let html = markdown_to_html(input);
            assert!(!html.contains("<script"));
            let _ = md_description(input);
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    }

    #[test]
    fn credential_detection() {
        for name in
            ["Authorization", "X-API-Key", "api_key", "access_token", "client_secret", "password", "Cookie", "key"]
        {
            assert!(is_credential_name(name), "{name}");
        }
        for name in ["Content-Type", "accessTokenUrl", "page", "keyword", "monkey", "id"] {
            assert!(!is_credential_name(name), "{name}");
        }
        assert_eq!(mask_value("Bearer {{token}}"), "Bearer {{token}}");
        assert_eq!(mask_value("Bearer abc"), "Bearer REDACTED");
        assert_eq!(mask_value(" {{a}} {{b}} "), "{{a}} {{b}}");
        assert_eq!(mask_value("x{{a}}"), "REDACTED");
        assert_eq!(mask_value(""), "");
    }

    #[test]
    fn writes_site_to_disk() {
        let dir = std::env::temp_dir().join(format!("merpati-docs-test-{}", std::process::id()));
        let index = write_html_site(&sample(), &DocsOptions::default(), &dir).unwrap();
        assert_eq!(index, dir.join("index.html"));
        let text = std::fs::read_to_string(&index).unwrap();
        assert!(text.contains("Pet API"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_snippets_when_no_languages() {
        let mut r = HttpRequest::new("POST", "https://h.test/x");
        r.body = Body::raw_text("<a/>", RawLanguage::Xml);
        let mut c = Collection::new("C");
        c.items.push(RequestItem::new("X", r).into());
        let md = to_markdown(&c, &DocsOptions { snippet_languages: Vec::new(), ..Default::default() });
        assert!(!md.contains("cURL") && md.contains("```xml\n<a/>\n```"));
    }
}

#[cfg(test)]
mod table_tests {
    #[test]
    fn gfm_tables() {
        let html = super::markdown_to_html("| Key | Value |\n| --- | --- |\n| `a` | 1 \\| 2 |\n\nafter");
        assert!(html.contains("<table>"), "{html}");
        assert!(html.contains("<th>Key</th><th>Value</th>"), "{html}");
        assert!(
            html.contains("<td><code>a</code></td><td>1 \\| 2</td>")
                || html.contains("<td>1 | 2</td>")
                || html.contains("1 \\| 2"),
            "{html}"
        );
        assert!(html.contains("<p>after</p>"), "{html}");
    }
}
