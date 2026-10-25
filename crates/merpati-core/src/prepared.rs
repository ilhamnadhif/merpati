//! Turning a model request into a concrete, fully resolved request.
//!
//! `prepare` is pure: it resolves variables, path variables, default headers
//! and the body, but performs no I/O. Auth is resolved here and applied later
//! by `merpati-auth`, because some schemes need the network.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;

use crate::auth::{Auth, AuthType};
use crate::model::{BodyMode, FormFieldKind, HttpRequest, HttpVersion, RequestSettings};
use crate::settings::RequestDefaults;
use crate::urlutil;
use crate::variables::Resolver;

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSettings {
    pub follow_redirects: bool,
    pub max_redirects: u32,
    pub follow_original_method: bool,
    pub follow_auth_header: bool,
    pub verify_ssl: bool,
    pub encode_url: bool,
    pub disable_cookies: bool,
    pub timeout: Option<Duration>,
    pub http_version: HttpVersion,
}

impl Default for ResolvedSettings {
    fn default() -> Self {
        ResolvedSettings::resolve(&RequestSettings::default(), &RequestDefaults::default())
    }
}

impl ResolvedSettings {
    pub fn resolve(s: &RequestSettings, d: &RequestDefaults) -> Self {
        let timeout_ms = s.timeout_ms.unwrap_or(d.timeout_ms);
        ResolvedSettings {
            follow_redirects: s.follow_redirects.unwrap_or(d.follow_redirects),
            max_redirects: s.max_redirects.unwrap_or(d.max_redirects),
            follow_original_method: s.follow_original_method.unwrap_or(false),
            follow_auth_header: s.follow_auth_header.unwrap_or(false),
            verify_ssl: s.verify_ssl.unwrap_or(d.verify_ssl),
            encode_url: s.encode_url.unwrap_or(d.encode_url),
            disable_cookies: s.disable_cookies.unwrap_or(false),
            timeout: (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms)),
            http_version: s.http_version.unwrap_or(d.http_version),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MultipartValue {
    Text(String),
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MultipartPart {
    pub name: String,
    pub value: MultipartValue,
    pub content_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum PreparedBody {
    #[default]
    None,
    Bytes(Bytes),
    File(PathBuf),
    Multipart(Vec<MultipartPart>),
}

impl PreparedBody {
    /// Bytes of the body when known without I/O (raw, url-encoded, GraphQL).
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            PreparedBody::Bytes(b) => Some(b),
            PreparedBody::None => Some(&[]),
            _ => None,
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, PreparedBody::None)
    }
}

/// A request ready to hand to a protocol client.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: PreparedBody,
    pub settings: ResolvedSettings,
    /// Resolved auth to apply before sending (never `Inherit`).
    pub auth: Auth,
    /// Id of the collection/folder/request that owns `auth` (token storage key).
    pub auth_owner_id: Option<String>,
    /// Names of variables that could not be resolved.
    pub unresolved: Vec<String>,
}

impl Default for PreparedRequest {
    fn default() -> Self {
        PreparedRequest {
            method: "GET".into(),
            url: String::new(),
            headers: Vec::new(),
            body: PreparedBody::None,
            settings: ResolvedSettings::default(),
            auth: Auth::none(),
            auth_owner_id: None,
            unresolved: Vec::new(),
        }
    }
}

impl PreparedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }

    /// Sets a header, replacing any existing ones with the same name.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.into()));
    }

    pub fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }

    /// Appends a query parameter (already encoded) to the URL.
    pub fn add_query(&mut self, key: &str, value: &str) {
        let (base, query, fragment) = urlutil::split(&self.url);
        let enc_k = percent_encoding::utf8_percent_encode(key, QUERY_ENCODE).to_string();
        let enc_v = percent_encoding::utf8_percent_encode(value, QUERY_ENCODE).to_string();
        let q = match query {
            Some(q) if !q.is_empty() => format!("{q}&{enc_k}={enc_v}"),
            _ => format!("{enc_k}={enc_v}"),
        };
        self.url = format!("{base}?{q}{fragment}");
    }
}

/// Characters escaped in query components we generate ourselves.
pub const QUERY_ENCODE: &percent_encoding::AsciiSet =
    &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

/// Inputs to [`prepare`] beyond the request itself.
pub struct PrepareContext<'a> {
    pub resolver: Resolver<'a>,
    /// Effective (possibly inherited) auth, still unresolved.
    pub auth: &'a Auth,
    pub auth_owner_id: Option<&'a str>,
    pub defaults: &'a RequestDefaults,
    /// Base directory for relative file paths in bodies.
    pub base_dir: Option<&'a Path>,
}

fn resolve_path(p: &str, base: Option<&Path>) -> PathBuf {
    let path = PathBuf::from(p);
    match base {
        Some(b) if path.is_relative() => b.join(path),
        _ => path,
    }
}

pub fn prepare(req: &HttpRequest, ctx: &PrepareContext<'_>) -> PreparedRequest {
    let mut unresolved = Vec::new();
    let mut r = |s: &str| ctx.resolver.resolve_tracking(s, &mut unresolved);

    let method = {
        let m = r(req.method.trim());
        if m.is_empty() { "GET".to_string() } else { m.to_uppercase() }
    };

    let settings = ResolvedSettings::resolve(&req.settings, ctx.defaults);

    let path_vars: Vec<(String, String)> =
        req.path_vars.iter().filter(|p| p.enabled).map(|p| (p.key.clone(), r(&p.value))).collect();
    let mut url = r(req.url.trim());
    url = urlutil::substitute_path_vars(&url, &path_vars);
    url = urlutil::ensure_scheme(&url);
    if settings.encode_url {
        url = urlutil::encode_loose(&url);
    }

    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .filter(|h| h.enabled && !h.key.trim().is_empty())
        .map(|h| (r(h.key.trim()), r(&h.value)))
        .collect();
    let has = |headers: &[(String, String)], name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));

    let body = match req.body.mode {
        BodyMode::None => PreparedBody::None,
        BodyMode::Raw => {
            let text = r(&req.body.raw);
            if !has(&headers, "content-type") {
                headers.push(("Content-Type".into(), req.body.raw_language.content_type().into()));
            }
            PreparedBody::Bytes(Bytes::from(text))
        }
        BodyMode::UrlEncoded => {
            let mut ser = url::form_urlencoded::Serializer::new(String::new());
            for f in req.body.urlencoded.iter().filter(|f| f.enabled && !f.key.is_empty()) {
                ser.append_pair(&r(&f.key), &r(&f.value));
            }
            if !has(&headers, "content-type") {
                headers.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
            }
            PreparedBody::Bytes(Bytes::from(ser.finish()))
        }
        BodyMode::FormData => {
            let mut parts = Vec::new();
            for f in req.body.form_data.iter().filter(|f| f.enabled && !f.key.is_empty()) {
                let name = r(&f.key);
                let content_type = f.content_type.as_ref().map(|c| r(c)).filter(|c| !c.is_empty());
                match f.kind {
                    FormFieldKind::Text => {
                        parts.push(MultipartPart { name, value: MultipartValue::Text(r(&f.value)), content_type })
                    }
                    FormFieldKind::File => {
                        for file in &f.files {
                            parts.push(MultipartPart {
                                name: name.clone(),
                                value: MultipartValue::File(resolve_path(&r(file), ctx.base_dir)),
                                content_type: content_type.clone(),
                            });
                        }
                    }
                }
            }
            PreparedBody::Multipart(parts)
        }
        BodyMode::Binary => {
            if req.body.binary_path.trim().is_empty() {
                PreparedBody::None
            } else {
                PreparedBody::File(resolve_path(&r(&req.body.binary_path), ctx.base_dir))
            }
        }
        BodyMode::GraphQl => {
            let gql = &req.body.graphql;
            let vars_text = r(&gql.variables);
            let variables = if vars_text.trim().is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                serde_json::from_str(&vars_text).unwrap_or(serde_json::Value::Null)
            };
            let mut obj = serde_json::Map::new();
            obj.insert("query".into(), serde_json::Value::String(r(&gql.query)));
            obj.insert("variables".into(), variables);
            if let Some(op) = gql.operation_name.as_ref().filter(|o| !o.is_empty()) {
                obj.insert("operationName".into(), serde_json::Value::String(r(op)));
            }
            if !has(&headers, "content-type") {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
            PreparedBody::Bytes(Bytes::from(serde_json::Value::Object(obj).to_string()))
        }
    };

    for (name, value) in ctx.defaults.default_headers() {
        let disabled = req.settings.disabled_default_headers.iter().any(|d| d.eq_ignore_ascii_case(name));
        if !disabled && !has(&headers, name) {
            headers.push((name.to_string(), value));
        }
    }

    let auth = if ctx.auth.kind == AuthType::Inherit {
        Auth::none()
    } else {
        ctx.auth.map_strings(|s| ctx.resolver.resolve_tracking(s, &mut unresolved))
    };

    PreparedRequest {
        method,
        url,
        headers,
        body,
        settings,
        auth,
        auth_owner_id: ctx.auth_owner_id.map(str::to_string),
        unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Body, FormField, KeyValue, RawLanguage};
    use crate::variables::VariableScopes;

    fn ctx<'a>(scopes: &'a VariableScopes, auth: &'a Auth, defaults: &'a RequestDefaults) -> PrepareContext<'a> {
        PrepareContext { resolver: Resolver::new(scopes), auth, auth_owner_id: None, defaults, base_dir: None }
    }

    #[test]
    fn resolves_everything() {
        let mut scopes = VariableScopes::default();
        scopes.environment.insert("base".into(), "localhost:8080".into());
        scopes.environment.insert("id".into(), "42".into());
        scopes.environment.insert("tok".into(), "abc".into());
        let mut req = HttpRequest::new("post", "{{base}}/users/:id?verbose=1");
        req.path_vars = vec![KeyValue::new("id", "{{id}}")];
        req.headers.push(KeyValue::new("X-Trace", "{{missing}}"));
        req.headers.push(KeyValue::new("X-Off", "1").disabled());
        req.body = Body::raw_text("{\"id\": {{id}}}", RawLanguage::Json);
        let auth = Auth::bearer("{{tok}}");
        let defaults = RequestDefaults::default();
        let p = prepare(&req, &ctx(&scopes, &auth, &defaults));
        assert_eq!(p.method, "POST");
        assert_eq!(p.url, "http://localhost:8080/users/42?verbose=1");
        assert_eq!(p.header("content-type"), Some("application/json"));
        assert_eq!(p.header("x-trace"), Some("{{missing}}"));
        assert!(p.header("x-off").is_none());
        assert!(p.header("user-agent").unwrap().starts_with("MerpatiRuntime/"));
        assert_eq!(p.body.as_bytes().unwrap(), b"{\"id\": 42}");
        assert_eq!(p.auth.get("token"), "abc");
        assert_eq!(p.unresolved, vec!["missing"]);
    }

    #[test]
    fn urlencoded_and_graphql() {
        let scopes = VariableScopes::default();
        let defaults = RequestDefaults::default();
        let auth = Auth::default();
        let mut req = HttpRequest::new("POST", "https://x.test/");
        req.body.mode = BodyMode::UrlEncoded;
        req.body.urlencoded = vec![KeyValue::new("a b", "c&d"), KeyValue::new("skip", "1").disabled()];
        let p = prepare(&req, &ctx(&scopes, &auth, &defaults));
        assert_eq!(p.body.as_bytes().unwrap(), b"a+b=c%26d");
        assert_eq!(p.auth.kind, AuthType::None);

        req.body.mode = BodyMode::GraphQl;
        req.body.graphql.query = "{ me { id } }".into();
        req.body.graphql.variables = "{\"x\": 1}".into();
        let p = prepare(&req, &ctx(&scopes, &auth, &defaults));
        let v: serde_json::Value = serde_json::from_slice(p.body.as_bytes().unwrap()).unwrap();
        assert_eq!(v["variables"]["x"], 1);
        assert_eq!(v["query"], "{ me { id } }");
    }

    #[test]
    fn form_data_files_relative_to_base() {
        let scopes = VariableScopes::default();
        let defaults = RequestDefaults::default();
        let auth = Auth::default();
        let mut req = HttpRequest::new("POST", "https://x.test/");
        req.body.mode = BodyMode::FormData;
        req.body.form_data = vec![FormField::text("name", "v"), FormField::file("upload", "files/a.png")];
        let base = PathBuf::from("/ws");
        let c = PrepareContext { base_dir: Some(&base), ..ctx(&scopes, &auth, &defaults) };
        let p = prepare(&req, &c);
        match p.body {
            PreparedBody::Multipart(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[1].value, MultipartValue::File(PathBuf::from("/ws/files/a.png")));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn add_query_encodes() {
        let mut p = PreparedRequest { url: "http://h/p#x".into(), ..Default::default() };
        p.add_query("api key", "a/b");
        assert_eq!(p.url, "http://h/p?api%20key=a%2Fb#x");
        p.add_query("z", "1");
        assert_eq!(p.url, "http://h/p?api%20key=a%2Fb&z=1#x");
    }
}
