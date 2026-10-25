//! Postman Collection v2.0/v2.1, environment and globals import/export.
//!
//! Import is tolerant: Postman files in the wild mix string and object forms
//! for URLs, headers and descriptions, and v2.0 stores auth parameters as an
//! object instead of a list. Anything that cannot be represented is reported
//! in [`ImportReport::warnings`] instead of being silently dropped.

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::auth::{Auth, AuthType};
use crate::environment::Environment;
use crate::error::{Error, Result};
use crate::model::*;
use crate::urlutil;
use crate::util::new_id;
use crate::variables::{VarKind, VarType, Variable, is_pure_reference};

pub const SCHEMA_V21: &str = "https://schema.getpostman.com/json/collection/v2.1.0/collection.json";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportReport {
    pub warnings: Vec<String>,
}

impl ImportReport {
    fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PostmanFile {
    Collection(Collection),
    Environment(Environment),
    Globals(Vec<Variable>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostmanKind {
    Collection,
    Environment,
    Globals,
}

pub fn detect(v: &Value) -> Option<PostmanKind> {
    if v.get("info").is_some() && v.get("item").is_some() {
        return Some(PostmanKind::Collection);
    }
    if v.get("values").map(Value::is_array).unwrap_or(false) {
        if v.get("_postman_variable_scope").and_then(Value::as_str) == Some("globals") {
            return Some(PostmanKind::Globals);
        }
        return Some(PostmanKind::Environment);
    }
    None
}

pub fn import_any(v: &Value) -> Result<(PostmanFile, ImportReport)> {
    match detect(v) {
        Some(PostmanKind::Collection) => import_collection(v).map(|(c, r)| (PostmanFile::Collection(c), r)),
        Some(PostmanKind::Environment) => {
            Ok((PostmanFile::Environment(import_environment(v)?), ImportReport::default()))
        }
        Some(PostmanKind::Globals) => Ok((PostmanFile::Globals(import_globals(v)?), ImportReport::default())),
        None if v.get("requests").is_some() && v.get("order").is_some() => {
            Err(Error::Format("Postman Collection v1 is not supported; re-export it as v2.1 from Postman".into()))
        }
        None => Err(Error::Format("not a Postman collection, environment or globals file".into())),
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

fn as_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).map(as_string).unwrap_or_default()
}

fn bool_field(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}

fn description(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o.get("content").map(as_string).unwrap_or_default(),
        _ => String::new(),
    }
}

fn id_or_new(v: &Value, key: &str) -> String {
    match v.get(key).and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => new_id(),
    }
}

pub fn import_collection(v: &Value) -> Result<(Collection, ImportReport)> {
    let info = v.get("info").ok_or_else(|| Error::Format("missing `info`".into()))?;
    let mut report = ImportReport::default();
    if let Some(schema) = info.get("schema").and_then(Value::as_str)
        && !schema.contains("v2.0")
        && !schema.contains("v2.1")
    {
        report.warn(format!("unknown schema {schema}; importing as v2.1"));
    }
    let inherited = import_settings(v.get("protocolProfileBehavior"), &RequestSettings::default(), &mut report);
    let mut c = Collection {
        id: id_or_new(info, "_postman_id"),
        name: str_field(info, "name"),
        description: description(info.get("description")),
        auth: import_auth(v.get("auth"), &mut report),
        scripts: import_events(v.get("event"), &mut report),
        variables: import_variables(v.get("variable")),
        items: Vec::new(),
    };
    if c.name.is_empty() {
        c.name = "Imported collection".into();
    }
    if c.auth.is_inherit() {
        // Root level has nothing to inherit from.
        c.auth = Auth::default();
    }
    let no_pruning = disables_body_pruning(v, false);
    c.items = import_items(v.get("item"), &inherited, no_pruning, &mut report);
    for key in ["proxy", "certificate"] {
        if v.get(key).is_some() {
            report.warn(format!("collection-level `{key}` is not imported; configure it in Settings"));
        }
    }
    Ok((c, report))
}

/// Methods whose body Postman drops unless `disableBodyPruning` is set.
fn prunes_body(method: &str) -> bool {
    ["GET", "HEAD", "COPY", "PURGE", "UNLOCK"].iter().any(|m| m.eq_ignore_ascii_case(method.trim()))
}

/// `protocolProfileBehavior.disableBodyPruning` of `v`, else the inherited one.
fn disables_body_pruning(v: &Value, inherited: bool) -> bool {
    v.pointer("/protocolProfileBehavior/disableBodyPruning").and_then(Value::as_bool).unwrap_or(inherited)
}

fn import_items(
    v: Option<&Value>,
    inherited: &RequestSettings,
    no_pruning: bool,
    report: &mut ImportReport,
) -> Vec<Item> {
    let Some(Value::Array(items)) = v else { return Vec::new() };
    items.iter().filter_map(|i| import_item(i, inherited, no_pruning, report)).collect()
}

fn import_item(v: &Value, inherited: &RequestSettings, no_pruning: bool, report: &mut ImportReport) -> Option<Item> {
    if !v.is_object() {
        report.warn("skipped an item that is not an object");
        return None;
    }
    let settings = import_settings(v.get("protocolProfileBehavior"), inherited, report);
    let no_pruning = disables_body_pruning(v, no_pruning);
    if v.get("item").is_some() || v.get("request").is_none() && v.get("response").is_none() {
        let folder = Folder {
            id: id_or_new(v, "id"),
            name: str_field(v, "name"),
            description: description(v.get("description")),
            auth: import_auth(v.get("auth"), report),
            scripts: import_events(v.get("event"), report),
            variables: import_variables(v.get("variable")),
            items: import_items(v.get("item"), &settings, no_pruning, report),
        };
        return Some(Item::Folder(folder));
    }
    if v.get("variable").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
        report.warn("request-level variables are not supported and were not imported");
    }
    let req_v = v.get("request").cloned().unwrap_or(Value::Null);
    let mut request = import_request(&req_v, report);
    request.settings = settings;
    // Postman does not send such a body, so neither should Merpati; the data
    // is kept for when the method changes.
    if !no_pruning && prunes_body(&request.method) && mode_has_data(&request.body, request.body.mode) {
        report.warn(format!(
            "\"{}\": Postman does not send a {} body without `disableBodyPruning`; its body was set to none",
            str_field(v, "name"),
            request.method
        ));
        request.body.mode = BodyMode::None;
    }
    let mut desc = description(req_v.get("description"));
    if desc.is_empty() {
        desc = description(v.get("description"));
    }
    let examples = match v.get("response") {
        Some(Value::Array(rs)) => rs.iter().map(|r| import_example(r, report)).collect(),
        _ => Vec::new(),
    };
    Some(Item::Request(RequestItem {
        id: id_or_new(v, "id"),
        name: str_field(v, "name"),
        description: desc,
        request: RequestKind::Http(request),
        scripts: import_events(v.get("event"), report),
        examples,
    }))
}

/// `protocolProfileBehavior` keys Merpati maps (`disableBodyPruning` is
/// handled with the body).
const KNOWN_PROFILE_KEYS: [&str; 11] = [
    "followRedirects",
    "maxRedirects",
    "followOriginalHttpMethod",
    "followAuthorizationHeader",
    "strictSSL",
    "disableUrlEncoding",
    "disableCookies",
    "requestTimeout",
    "protocolVersion",
    "disabledSystemHeaders",
    "disableBodyPruning",
];

fn import_settings(v: Option<&Value>, inherited: &RequestSettings, report: &mut ImportReport) -> RequestSettings {
    let mut s = inherited.clone();
    let Some(Value::Object(o)) = v else { return s };
    for k in o.keys().filter(|k| !KNOWN_PROFILE_KEYS.contains(&k.as_str())) {
        report.warn(format!("`protocolProfileBehavior.{k}` is not supported and was not imported"));
    }
    let b = |k: &str| o.get(k).and_then(Value::as_bool);
    if let Some(x) = b("followRedirects") {
        s.follow_redirects = Some(x);
    }
    if let Some(x) = o.get("maxRedirects").and_then(Value::as_u64) {
        s.max_redirects = Some(u32::try_from(x).unwrap_or(u32::MAX));
    }
    if let Some(x) = b("followOriginalHttpMethod") {
        s.follow_original_method = Some(x);
    }
    if let Some(x) = b("followAuthorizationHeader") {
        s.follow_auth_header = Some(x);
    }
    if let Some(x) = b("strictSSL") {
        s.verify_ssl = Some(x);
    }
    if let Some(x) = b("disableUrlEncoding") {
        s.encode_url = Some(!x);
    }
    if let Some(x) = b("disableCookies") {
        s.disable_cookies = Some(x);
    }
    if let Some(x) = o.get("requestTimeout").and_then(Value::as_u64) {
        s.timeout_ms = Some(x);
    }
    if let Some(x) = o.get("protocolVersion").and_then(Value::as_str) {
        s.http_version = match x {
            "http1" => Some(HttpVersion::Http1),
            "http2" => Some(HttpVersion::Http2),
            "auto" => Some(HttpVersion::Auto),
            _ => s.http_version,
        };
    }
    if let Some(Value::Object(h)) = o.get("disabledSystemHeaders") {
        s.disabled_default_headers =
            h.iter().filter(|(_, v)| v.as_bool().unwrap_or(false)).map(|(k, _)| k.clone()).collect();
    }
    s
}

pub fn import_request(v: &Value, report: &mut ImportReport) -> HttpRequest {
    if let Value::String(url) = v {
        return HttpRequest::new("GET", url.clone());
    }
    let mut req = HttpRequest { method: str_field(v, "method"), ..Default::default() };
    if req.method.is_empty() {
        req.method = "GET".into();
    }
    let (url, params, path_vars) = import_url(v.get("url"));
    req.url = url;
    req.params = params;
    req.path_vars = path_vars;
    req.headers = import_headers(v.get("header"));
    req.body = import_body(v.get("body"), report);
    req.auth = import_auth(v.get("auth"), report);
    for key in ["proxy", "certificate"] {
        if v.get(key).is_some() {
            report.warn(format!("request-level `{key}` is not imported; configure it in Settings"));
        }
    }
    req
}

fn import_url(v: Option<&Value>) -> (String, Vec<KeyValue>, Vec<KeyValue>) {
    match v {
        Some(Value::String(s)) => {
            let params = urlutil::merge_params_from_url(s, &[]);
            let vars = urlutil::merge_path_vars(s, &[]);
            (s.clone(), params, vars)
        }
        Some(Value::Object(o)) => {
            let query: Vec<KeyValue> = match o.get("query") {
                Some(Value::Array(q)) => q
                    .iter()
                    .map(|p| KeyValue {
                        key: str_field(p, "key"),
                        value: str_field(p, "value"),
                        enabled: !bool_field(p, "disabled").unwrap_or(false),
                        description: description(p.get("description")),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let raw = match o.get("raw").and_then(Value::as_str) {
                Some(r) => r.to_string(),
                None => rebuild_raw(o, &query),
            };
            let params = urlutil::merge_params_from_url(&raw, &query);
            let declared: Vec<KeyValue> = match o.get("variable") {
                Some(Value::Array(vars)) => vars
                    .iter()
                    .map(|p| KeyValue {
                        key: str_field(p, "key"),
                        value: str_field(p, "value"),
                        enabled: !bool_field(p, "disabled").unwrap_or(false),
                        description: description(p.get("description")),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let path_vars = urlutil::merge_path_vars(&raw, &declared);
            (raw, params, path_vars)
        }
        _ => (String::new(), Vec::new(), Vec::new()),
    }
}

fn join_part(v: Option<&Value>, sep: &str) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::Object(o) => o.get("value").map(as_string).unwrap_or_default(),
                other => as_string(other),
            })
            .collect::<Vec<_>>()
            .join(sep),
        _ => String::new(),
    }
}

fn rebuild_raw(o: &Map<String, Value>, query: &[KeyValue]) -> String {
    let mut s = String::new();
    if let Some(p) = o.get("protocol").and_then(Value::as_str) {
        s.push_str(p);
        s.push_str("://");
    }
    s.push_str(&join_part(o.get("host"), "."));
    if let Some(port) = o.get("port") {
        let port = as_string(port);
        if !port.is_empty() {
            s.push(':');
            s.push_str(&port);
        }
    }
    let path = join_part(o.get("path"), "/");
    if !path.is_empty() {
        if !path.starts_with('/') {
            s.push('/');
        }
        s.push_str(&path);
    }
    let q = urlutil::build_query(query);
    if !q.is_empty() {
        s.push('?');
        s.push_str(&q);
    }
    if let Some(h) = o.get("hash").and_then(Value::as_str)
        && !h.is_empty()
    {
        s.push('#');
        s.push_str(h);
    }
    s
}

fn import_headers(v: Option<&Value>) -> Vec<KeyValue> {
    match v {
        Some(Value::Array(hs)) => hs
            .iter()
            .filter(|h| h.is_object())
            .map(|h| KeyValue {
                key: str_field(h, "key"),
                value: str_field(h, "value"),
                enabled: !bool_field(h, "disabled").unwrap_or(false),
                description: description(h.get("description")),
            })
            .collect(),
        Some(Value::String(s)) => s
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                let (disabled, line) = match line.strip_prefix("//") {
                    Some(rest) => (true, rest.trim()),
                    None => (false, line),
                };
                let (k, v) = line.split_once(':')?;
                let mut kv = KeyValue::new(k.trim(), v.trim());
                kv.enabled = !disabled;
                Some(kv)
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn import_body(v: Option<&Value>, report: &mut ImportReport) -> Body {
    let Some(v) = v.filter(|v| v.is_object()) else { return Body::default() };
    // Every mode's data is read, not only the active one, so data kept for
    // inactive modes survives a round trip.
    let mut body = Body::default();
    if let Some(raw) = v.get("raw") {
        body.raw = as_string(raw);
        let lang = v.pointer("/options/raw/language").and_then(Value::as_str).unwrap_or("text");
        body.raw_language = match lang {
            "json" => RawLanguage::Json,
            "xml" => RawLanguage::Xml,
            "html" => RawLanguage::Html,
            "javascript" => RawLanguage::JavaScript,
            _ => RawLanguage::Text,
        };
    }
    if let Some(Value::Array(fs)) = v.get("urlencoded") {
        body.urlencoded = fs
            .iter()
            .map(|f| KeyValue {
                key: str_field(f, "key"),
                value: str_field(f, "value"),
                enabled: !bool_field(f, "disabled").unwrap_or(false),
                description: description(f.get("description")),
            })
            .collect();
    }
    if let Some(Value::Array(fs)) = v.get("formdata") {
        body.form_data = fs
            .iter()
            .map(|f| {
                let is_file = f.get("type").and_then(Value::as_str) == Some("file");
                let files = match f.get("src") {
                    Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
                    Some(Value::Array(a)) => a.iter().map(as_string).filter(|s| !s.is_empty()).collect(),
                    _ => Vec::new(),
                };
                FormField {
                    key: str_field(f, "key"),
                    value: if is_file { String::new() } else { str_field(f, "value") },
                    kind: if is_file { FormFieldKind::File } else { FormFieldKind::Text },
                    files,
                    content_type: f
                        .get("contentType")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    enabled: !bool_field(f, "disabled").unwrap_or(false),
                    description: description(f.get("description")),
                }
            })
            .collect();
    }
    if let Some(src) = v.pointer("/file/src") {
        body.binary_path = as_string(src);
    }
    if v.pointer("/file/content").is_some() {
        report.warn("inline file body content is not supported; only file paths are imported");
    }
    if v.get("graphql").is_some() {
        body.graphql.query = v.pointer("/graphql/query").map(as_string).unwrap_or_default();
        body.graphql.variables = match v.pointer("/graphql/variables") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        body.graphql.operation_name =
            v.pointer("/graphql/operationName").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    }
    body.mode = match str_field(v, "mode").as_str() {
        "raw" => BodyMode::Raw,
        "urlencoded" => BodyMode::UrlEncoded,
        "formdata" => BodyMode::FormData,
        "file" => BodyMode::Binary,
        "graphql" => BodyMode::GraphQl,
        "" | "none" => BodyMode::None,
        other => {
            report.warn(format!("unsupported body mode `{other}`"));
            BodyMode::None
        }
    };
    // A disabled body is kept but not sent, which is what mode `none` does.
    if bool_field(v, "disabled").unwrap_or(false) {
        body.mode = BodyMode::None;
    }
    body
}

pub fn import_auth(v: Option<&Value>, report: &mut ImportReport) -> Auth {
    let Some(v) = v.filter(|v| v.is_object()) else { return Auth::default() };
    let type_str = str_field(v, "type");
    let Some(kind) = AuthType::parse(&type_str) else {
        report.warn(format!("unknown auth type `{type_str}`; set to inherit"));
        return Auth::default();
    };
    let mut params = IndexMap::new();
    match v.get(kind.as_str()) {
        // v2.1: list of {key, value, type}
        Some(Value::Array(list)) => {
            for p in list {
                if let Some(k) = p.get("key").and_then(Value::as_str) {
                    params.insert(k.to_string(), p.get("value").cloned().unwrap_or(Value::Null));
                }
            }
        }
        // v2.0: object map
        Some(Value::Object(o)) => {
            for (k, val) in o {
                params.insert(k.clone(), val.clone());
            }
        }
        _ => {}
    }
    Auth { kind, params }
}

fn import_events(v: Option<&Value>, report: &mut ImportReport) -> Scripts {
    let mut s = Scripts::default();
    let Some(Value::Array(events)) = v else { return s };
    let (mut pre, mut test) = (Vec::new(), Vec::new());
    for e in events {
        let code = match e.pointer("/script/exec") {
            Some(Value::Array(lines)) => lines.iter().map(as_string).collect::<Vec<_>>().join("\n"),
            Some(Value::String(code)) => code.clone(),
            _ => continue,
        };
        let (scripts, label) = match e.get("listen").and_then(Value::as_str) {
            Some("prerequest") => (&mut pre, "pre-request"),
            Some("test") => (&mut test, "post-response"),
            _ => continue,
        };
        // Postman does not run disabled events.
        if bool_field(e, "disabled").unwrap_or(false) {
            report.warn(format!("a disabled {label} script was not imported"));
            continue;
        }
        if e.pointer("/script/packages").is_some_and(|p| p.as_object().is_some_and(|o| !o.is_empty())) {
            report.warn("script packages (`pm.require`) are not supported; scripts using them will fail");
        }
        scripts.push(code);
    }
    for (scripts, out) in [(pre, &mut s.pre_request), (test, &mut s.post_response)] {
        *out = match scripts.len() {
            0 | 1 => scripts.into_iter().next().unwrap_or_default(),
            // Postman runs each event separately; blocks keep their top-level
            // `const`/`let` declarations from clashing in the merged script.
            _ => {
                report.warn("several scripts for the same event were merged into one");
                scripts.iter().map(|c| format!("{{\n{c}\n}}")).collect::<Vec<_>>().join("\n")
            }
        };
    }
    s
}

/// Value, kind and type of a Postman variable. Values are converted by
/// their `type` like postman-collection's `Variable` does (`String(v)`,
/// `Number(v)`, `Boolean(v)`); other types (`any`, `default`, none) keep the
/// JSON value. Secrets are strings; a missing value is an empty string.
fn import_value(x: &Value) -> (String, VarKind, VarType) {
    let type_name = x.get("type").and_then(Value::as_str);
    if type_name == Some("secret") {
        return (str_field(x, "value"), VarKind::Secret, VarType::Default);
    }
    let empty = Value::String(String::new());
    let (value, ty) = VarType::cast(type_name.and_then(VarType::parse), x.get("value").unwrap_or(&empty));
    (value, VarKind::Default, ty)
}

fn import_variables(v: Option<&Value>) -> Vec<Variable> {
    let Some(Value::Array(vars)) = v else { return Vec::new() };
    vars.iter()
        .filter(|x| x.is_object())
        .map(|x| {
            let (value, kind, ty) = import_value(x);
            Variable {
                key: str_field(x, "key"),
                value,
                current: None,
                kind,
                ty,
                enabled: !bool_field(x, "disabled").unwrap_or(false) && bool_field(x, "enabled").unwrap_or(true),
                description: description(x.get("description")),
            }
        })
        .collect()
}

fn import_example(v: &Value, report: &mut ImportReport) -> Example {
    let request = v.get("originalRequest").filter(|r| !r.is_null()).map(|r| import_request(r, report));
    let headers = import_headers(v.get("header"));
    Example {
        id: id_or_new(v, "id"),
        name: {
            let n = str_field(v, "name");
            if n.is_empty() { "Example".into() } else { n }
        },
        request,
        status: v.get("code").and_then(Value::as_u64).and_then(|c| u16::try_from(c).ok()).unwrap_or(200),
        status_text: str_field(v, "status"),
        headers,
        body: str_field(v, "body"),
        language: v
            .get("_postman_previewlanguage")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

pub fn import_environment(v: &Value) -> Result<Environment> {
    let values = v.get("values").ok_or_else(|| Error::Format("missing `values`".into()))?;
    let mut env =
        Environment { id: id_or_new(v, "id"), name: str_field(v, "name"), variables: import_env_values(values) };
    if env.name.is_empty() {
        env.name = "Imported environment".into();
    }
    Ok(env)
}

pub fn import_globals(v: &Value) -> Result<Vec<Variable>> {
    let values = v.get("values").ok_or_else(|| Error::Format("missing `values`".into()))?;
    Ok(import_env_values(values))
}

fn import_env_values(v: &Value) -> Vec<Variable> {
    let Value::Array(vals) = v else { return Vec::new() };
    vals.iter()
        .filter(|x| x.is_object())
        .map(|x| {
            let (value, kind, ty) = import_value(x);
            Variable {
                key: str_field(x, "key"),
                value,
                current: None,
                kind,
                ty,
                enabled: bool_field(x, "enabled").unwrap_or(true) && !bool_field(x, "disabled").unwrap_or(false),
                description: description(x.get("description")),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Include secret variable values and literal sensitive auth parameters.
    /// Off by default so exports never leak credentials.
    pub include_secrets: bool,
    /// Export current values instead of initial values for variables.
    pub use_current_values: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExportReport {
    pub warnings: Vec<String>,
    /// Number of values blanked because they were secrets.
    pub redacted: usize,
}

struct Exporter {
    opts: ExportOptions,
    report: ExportReport,
}

impl Exporter {
    /// The value as Postman stores it: typed (`5`, `true`, `{"a":1}`) except
    /// for secrets.
    fn var_value(&mut self, v: &Variable) -> Value {
        let value = if self.opts.use_current_values { v.effective() } else { &v.value };
        if v.is_secret() {
            if !self.opts.include_secrets && !value.is_empty() {
                self.report.redacted += 1;
                return json!("");
            }
            return json!(value);
        }
        v.ty.to_json(value)
    }

    fn variables(&mut self, vars: &[Variable]) -> Value {
        Value::Array(
            vars.iter()
                .map(|v| {
                    let mut o = Map::new();
                    o.insert("key".into(), json!(v.key));
                    o.insert("value".into(), self.var_value(v));
                    // No type is Postman's `any`, which a value of any JSON type keeps.
                    let ty = if v.is_secret() {
                        Some("secret")
                    } else {
                        v.ty.postman_name().filter(|_| v.ty != VarType::Any)
                    };
                    if let Some(ty) = ty {
                        o.insert("type".into(), json!(ty));
                    }
                    if !v.enabled {
                        o.insert("disabled".into(), json!(true));
                    }
                    if !v.description.is_empty() {
                        o.insert("description".into(), json!(v.description));
                    }
                    Value::Object(o)
                })
                .collect(),
        )
    }

    fn auth(&mut self, a: &Auth) -> Option<Value> {
        match a.kind {
            AuthType::Inherit => None,
            AuthType::None => Some(json!({"type": "noauth"})),
            kind => {
                let sensitive = kind.sensitive_params();
                let list: Vec<Value> = a
                    .params
                    .iter()
                    .map(|(k, v)| {
                        let mut value = v.clone();
                        if !self.opts.include_secrets
                            && sensitive.contains(&k.as_str())
                            && let Value::String(s) = v
                            && !s.is_empty()
                            && !is_pure_reference(s)
                        {
                            self.report.redacted += 1;
                            value = Value::String(String::new());
                        }
                        let ty = match &value {
                            Value::Bool(_) => "boolean",
                            Value::Number(_) => "number",
                            Value::String(_) | Value::Null => "string",
                            _ => "any",
                        };
                        json!({"key": k, "value": value, "type": ty})
                    })
                    .collect();
                let mut o = Map::new();
                o.insert("type".into(), json!(kind.as_str()));
                o.insert(kind.as_str().into(), Value::Array(list));
                Some(Value::Object(o))
            }
        }
    }

    fn events(&self, s: &Scripts) -> Option<Value> {
        let mut events = Vec::new();
        for (listen, code) in [("prerequest", &s.pre_request), ("test", &s.post_response)] {
            if !code.is_empty() {
                let exec: Vec<&str> = code.split('\n').collect();
                events.push(json!({"listen": listen, "script": {"type": "text/javascript", "exec": exec}}));
            }
        }
        (!events.is_empty()).then(|| Value::Array(events))
    }

    fn request(&mut self, r: &HttpRequest, description: &str) -> Value {
        let mut o = Map::new();
        if let Some(a) = self.auth(&r.auth) {
            o.insert("auth".into(), a);
        }
        o.insert("method".into(), json!(r.method));
        o.insert("header".into(), export_kv_list(&r.headers, true));
        if let Some(b) = export_body(&r.body) {
            o.insert("body".into(), b);
        }
        o.insert("url".into(), export_url(&r.url, &r.params, &r.path_vars));
        if !description.is_empty() {
            o.insert("description".into(), json!(description));
        }
        Value::Object(o)
    }

    fn item(&mut self, item: &Item) -> Option<Value> {
        match item {
            Item::Folder(f) => {
                let mut o = Map::new();
                o.insert("id".into(), json!(f.id));
                o.insert("name".into(), json!(f.name));
                if !f.description.is_empty() {
                    o.insert("description".into(), json!(f.description));
                }
                let items: Vec<Value> = f.items.iter().filter_map(|i| self.item(i)).collect();
                o.insert("item".into(), Value::Array(items));
                if let Some(a) = self.auth(&f.auth) {
                    o.insert("auth".into(), a);
                }
                if let Some(e) = self.events(&f.scripts) {
                    o.insert("event".into(), e);
                }
                if !f.variables.is_empty() {
                    let vars = self.variables(&f.variables);
                    o.insert("variable".into(), vars);
                }
                Some(Value::Object(o))
            }
            Item::Request(r) => {
                let RequestKind::Http(http) = &r.request else {
                    self.report.warnings.push(format!(
                        "\"{}\" is a {} request; Postman Collection v2.1 cannot represent it, so it was skipped",
                        r.name,
                        r.request.protocol_label()
                    ));
                    return None;
                };
                let mut o = Map::new();
                o.insert("id".into(), json!(r.id));
                o.insert("name".into(), json!(r.name));
                if let Some(e) = self.events(&r.scripts) {
                    o.insert("event".into(), e);
                }
                let mut ppb = export_settings(&http.settings);
                // Merpati sends a GET body; Postman and newman only do so with this flag.
                if prunes_body(&http.method) && mode_has_data(&http.body, http.body.mode) {
                    ppb.get_or_insert_with(|| json!({}))["disableBodyPruning"] = json!(true);
                }
                if let Some(ppb) = ppb {
                    o.insert("protocolProfileBehavior".into(), ppb);
                }
                o.insert("request".into(), self.request(http, &r.description));
                let responses: Vec<Value> = r.examples.iter().map(|e| self.example(e)).collect();
                o.insert("response".into(), Value::Array(responses));
                Some(Value::Object(o))
            }
        }
    }

    fn example(&mut self, e: &Example) -> Value {
        let mut o = Map::new();
        o.insert("id".into(), json!(e.id));
        o.insert("name".into(), json!(e.name));
        if let Some(r) = &e.request {
            o.insert("originalRequest".into(), self.request(r, ""));
        }
        o.insert("status".into(), json!(e.status_text));
        o.insert("code".into(), json!(e.status));
        if let Some(l) = &e.language {
            o.insert("_postman_previewlanguage".into(), json!(l));
        }
        o.insert("header".into(), export_kv_list(&e.headers, false));
        o.insert("cookie".into(), json!([]));
        o.insert("body".into(), json!(e.body));
        Value::Object(o)
    }
}

fn export_kv_list(list: &[KeyValue], with_type: bool) -> Value {
    Value::Array(
        list.iter()
            .map(|h| {
                let mut o = Map::new();
                o.insert("key".into(), json!(h.key));
                o.insert("value".into(), json!(h.value));
                if with_type {
                    o.insert("type".into(), json!("text"));
                }
                if !h.enabled {
                    o.insert("disabled".into(), json!(true));
                }
                if !h.description.is_empty() {
                    o.insert("description".into(), json!(h.description));
                }
                Value::Object(o)
            })
            .collect(),
    )
}

fn export_settings(s: &RequestSettings) -> Option<Value> {
    let mut o = Map::new();
    if let Some(v) = s.follow_redirects {
        o.insert("followRedirects".into(), json!(v));
    }
    if let Some(v) = s.max_redirects {
        o.insert("maxRedirects".into(), json!(v));
    }
    if let Some(v) = s.follow_original_method {
        o.insert("followOriginalHttpMethod".into(), json!(v));
    }
    if let Some(v) = s.follow_auth_header {
        o.insert("followAuthorizationHeader".into(), json!(v));
    }
    if let Some(v) = s.verify_ssl {
        o.insert("strictSSL".into(), json!(v));
    }
    if let Some(v) = s.encode_url {
        o.insert("disableUrlEncoding".into(), json!(!v));
    }
    if let Some(v) = s.disable_cookies {
        o.insert("disableCookies".into(), json!(v));
    }
    if let Some(v) = s.timeout_ms {
        o.insert("requestTimeout".into(), json!(v));
    }
    if let Some(v) = s.http_version {
        let s = match v {
            HttpVersion::Auto => "auto",
            HttpVersion::Http1 => "http1",
            HttpVersion::Http2 => "http2",
        };
        o.insert("protocolVersion".into(), json!(s));
    }
    if !s.disabled_default_headers.is_empty() {
        let h: Map<String, Value> = s.disabled_default_headers.iter().map(|k| (k.clone(), json!(true))).collect();
        o.insert("disabledSystemHeaders".into(), Value::Object(h));
    }
    (!o.is_empty()).then_some(Value::Object(o))
}

/// Whether `mode` holds anything that would be sent.
fn mode_has_data(b: &Body, mode: BodyMode) -> bool {
    match mode {
        BodyMode::None => false,
        BodyMode::Raw => !b.raw.is_empty(),
        BodyMode::UrlEncoded => !b.urlencoded.is_empty(),
        BodyMode::FormData => !b.form_data.is_empty(),
        BodyMode::Binary => !b.binary_path.is_empty(),
        BodyMode::GraphQl => b.graphql != GraphQlBody::default(),
    }
}

fn export_body(b: &Body) -> Option<Value> {
    let has_raw = !b.raw.is_empty() || b.raw_language != RawLanguage::default();
    // Mode `none` with data left in other modes is Postman's disabled body
    // (kept, not sent); the first mode holding data stands in as its mode.
    let active = match b.mode {
        BodyMode::None if has_raw => BodyMode::Raw,
        BodyMode::None => [BodyMode::UrlEncoded, BodyMode::FormData, BodyMode::Binary, BodyMode::GraphQl]
            .into_iter()
            .find(|m| mode_has_data(b, *m))?,
        m => m,
    };
    let mode = match active {
        BodyMode::None => return None,
        BodyMode::Raw => "raw",
        BodyMode::UrlEncoded => "urlencoded",
        BodyMode::FormData => "formdata",
        BodyMode::Binary => "file",
        BodyMode::GraphQl => "graphql",
    };
    let mut o = Map::new();
    o.insert("mode".into(), json!(mode));
    if b.mode == BodyMode::None {
        o.insert("disabled".into(), json!(true));
    }
    // Data of inactive modes is exported too (the schema allows it) so that
    // switching modes back and forth never loses what the user typed.
    if active == BodyMode::Raw || has_raw {
        o.insert("raw".into(), json!(b.raw));
        let lang = match b.raw_language {
            RawLanguage::Json => "json",
            RawLanguage::Text => "text",
            RawLanguage::Xml => "xml",
            RawLanguage::Html => "html",
            RawLanguage::JavaScript => "javascript",
        };
        o.insert("options".into(), json!({"raw": {"language": lang}}));
    }
    if b.mode == BodyMode::UrlEncoded || !b.urlencoded.is_empty() {
        o.insert("urlencoded".into(), export_kv_list(&b.urlencoded, true));
    }
    if b.mode == BodyMode::FormData || !b.form_data.is_empty() {
        let fields: Vec<Value> = b
            .form_data
            .iter()
            .map(|f| {
                let mut x = Map::new();
                x.insert("key".into(), json!(f.key));
                match f.kind {
                    FormFieldKind::Text => {
                        x.insert("value".into(), json!(f.value));
                        x.insert("type".into(), json!("text"));
                    }
                    FormFieldKind::File => {
                        x.insert("type".into(), json!("file"));
                        let src = if f.files.len() == 1 { json!(f.files[0]) } else { json!(f.files) };
                        x.insert("src".into(), src);
                    }
                }
                if let Some(ct) = &f.content_type {
                    x.insert("contentType".into(), json!(ct));
                }
                if !f.enabled {
                    x.insert("disabled".into(), json!(true));
                }
                if !f.description.is_empty() {
                    x.insert("description".into(), json!(f.description));
                }
                Value::Object(x)
            })
            .collect();
        o.insert("formdata".into(), Value::Array(fields));
    }
    if b.mode == BodyMode::Binary || !b.binary_path.is_empty() {
        o.insert("file".into(), json!({"src": b.binary_path}));
    }
    if b.mode == BodyMode::GraphQl || b.graphql != GraphQlBody::default() {
        let mut g = Map::new();
        g.insert("query".into(), json!(b.graphql.query));
        g.insert("variables".into(), json!(b.graphql.variables));
        if let Some(op) = &b.graphql.operation_name {
            g.insert("operationName".into(), json!(op));
        }
        o.insert("graphql".into(), Value::Object(g));
    }
    Some(Value::Object(o))
}

/// Splits a host part on dots unless it contains variables.
fn host_parts(host: &str) -> Vec<String> {
    if host.contains("{{") { vec![host.to_string()] } else { host.split('.').map(str::to_string).collect() }
}

/// Builds Postman's structured URL object. Tools like newman read the parts
/// (not `raw`), so they must be filled in.
pub fn export_url(raw: &str, params: &[KeyValue], path_vars: &[KeyValue]) -> Value {
    let mut o = Map::new();
    o.insert("raw".into(), json!(raw));
    let (base, query, fragment) = urlutil::split(raw);
    // Whether byte `at` of `s` is inside a `{{variable}}`.
    let in_var = |s: &str, at: usize| s[..at].matches("{{").count() > s[..at].matches("}}").count();
    let (protocol, rest) = match base.find("://") {
        // `{{scheme}}://` is a protocol; a `/` before `://` means it is in the
        // path (`{{base}}/to/http://x`).
        Some(i) if i > 0 && !base[..i].contains('/') && !in_var(base, i) => (Some(&base[..i]), &base[i + 3..]),
        _ => (None, base),
    };
    if let Some(p) = protocol {
        o.insert("protocol".into(), json!(p));
    }
    // Host ends at the first '/' outside of {{ }}.
    let mut host_end = rest.len();
    let mut i = 0;
    let bytes = rest.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'{') {
            match rest[i..].find("}}") {
                Some(e) => {
                    i += e + 2;
                    continue;
                }
                None => break,
            }
        }
        if bytes[i] == b'/' {
            host_end = i;
            break;
        }
        i += 1;
    }
    let (hostport, path) = rest.split_at(host_end);
    let (host, port) = match hostport.rfind(':') {
        // Not a colon inside `{{ }}` or an IPv6 literal (`[::1]`).
        Some(c) if !in_var(hostport, c) && !hostport[c + 1..].is_empty() && !hostport[c + 1..].contains(']') => {
            (&hostport[..c], Some(&hostport[c + 1..]))
        }
        _ => (hostport, None),
    };
    if !host.is_empty() {
        o.insert("host".into(), json!(host_parts(host)));
    }
    if let Some(p) = port {
        o.insert("port".into(), json!(p));
    }
    // Only the separator is dropped: `//a` is the segments "" and "a".
    let segments: Vec<&str> = path.strip_prefix('/').unwrap_or(path).split('/').collect();
    if !path.is_empty() {
        o.insert("path".into(), json!(segments));
    }
    if !params.is_empty() {
        let mut rows = export_kv_list(params, false);
        // `?flag` has a null value in Postman; "" would be sent as `flag=`.
        let mut pieces = query.unwrap_or("").split('&').filter(|p| !p.is_empty());
        if let Value::Array(rows) = &mut rows {
            for (row, p) in rows.iter_mut().zip(params).filter(|(_, p)| p.enabled) {
                if pieces.next() == Some(p.key.as_str()) && p.value.is_empty() {
                    row["value"] = Value::Null;
                }
            }
        }
        o.insert("query".into(), rows);
    }
    if !path_vars.is_empty() {
        o.insert("variable".into(), export_kv_list(path_vars, false));
    }
    if let Some(h) = fragment.strip_prefix('#') {
        o.insert("hash".into(), json!(h));
    }
    Value::Object(o)
}

pub fn export_collection(c: &Collection, opts: ExportOptions) -> (Value, ExportReport) {
    let mut ex = Exporter { opts, report: ExportReport::default() };
    let mut o = Map::new();
    let mut info = Map::new();
    info.insert("_postman_id".into(), json!(c.id));
    info.insert("name".into(), json!(c.name));
    if !c.description.is_empty() {
        info.insert("description".into(), json!(c.description));
    }
    info.insert("schema".into(), json!(SCHEMA_V21));
    o.insert("info".into(), Value::Object(info));
    let items: Vec<Value> = c.items.iter().filter_map(|i| ex.item(i)).collect();
    o.insert("item".into(), Value::Array(items));
    if let Some(a) = ex.auth(&c.auth) {
        o.insert("auth".into(), a);
    }
    if let Some(e) = ex.events(&c.scripts) {
        o.insert("event".into(), e);
    }
    if !c.variables.is_empty() {
        let vars = ex.variables(&c.variables);
        o.insert("variable".into(), vars);
    }
    (Value::Object(o), ex.report)
}

fn export_env_values(ex: &mut Exporter, vars: &[Variable]) -> Value {
    Value::Array(
        vars.iter()
            .map(|v| {
                let ty = match v.ty {
                    _ if v.is_secret() => "secret",
                    VarType::String | VarType::Number | VarType::Boolean => v.ty.postman_name().unwrap_or("default"),
                    // Postman's `any`, which a value of any JSON type keeps.
                    VarType::Default | VarType::Any => "default",
                };
                let mut o = json!({
                    "key": v.key,
                    "value": ex.var_value(v),
                    "type": ty,
                    "enabled": v.enabled,
                });
                if !v.description.is_empty() {
                    o["description"] = json!(v.description);
                }
                o
            })
            .collect(),
    )
}

pub fn export_environment(e: &Environment, opts: ExportOptions) -> (Value, ExportReport) {
    let mut ex = Exporter { opts, report: ExportReport::default() };
    let values = export_env_values(&mut ex, &e.variables);
    let v = json!({
        "id": e.id,
        "name": e.name,
        "values": values,
        "_postman_variable_scope": "environment",
        "_postman_exported_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "_postman_exported_using": concat!("Merpati/", env!("CARGO_PKG_VERSION")),
    });
    (v, ex.report)
}

pub fn export_globals(vars: &[Variable], opts: ExportOptions) -> (Value, ExportReport) {
    let mut ex = Exporter { opts, report: ExportReport::default() };
    let values = export_env_values(&mut ex, vars);
    let v = json!({
        "id": new_id(),
        "name": "Globals",
        "values": values,
        "_postman_variable_scope": "globals",
        "_postman_exported_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "_postman_exported_using": concat!("Merpati/", env!("CARGO_PKG_VERSION")),
    });
    (v, ex.report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../testdata/sample.postman_collection.json");

    #[test]
    fn imports_sample() {
        let v: Value = serde_json::from_str(SAMPLE).unwrap();
        let (c, report) = import_collection(&v).unwrap();
        assert_eq!(c.name, "Sample API");
        assert_eq!(c.auth.kind, AuthType::Bearer);
        assert_eq!(c.auth.get("token"), "{{token}}");
        assert_eq!(c.variables.len(), 2);
        assert!(c.scripts.pre_request.contains("pm.variables.set"));
        let reqs = c.requests();
        assert_eq!(reqs.len(), 5);

        let list = reqs.iter().find(|r| r.name == "List users").unwrap();
        let http = list.http().unwrap();
        assert_eq!(http.url, "{{baseUrl}}/users?page=1");
        assert_eq!(http.params.len(), 2);
        assert!(!http.params[1].enabled);
        assert_eq!(list.examples.len(), 1);
        assert_eq!(list.examples[0].status, 200);
        assert!(list.scripts.post_response.contains("pm.test"));
        assert_eq!(http.settings.follow_redirects, Some(false));

        let create = reqs.iter().find(|r| r.name == "Create user").unwrap().http().unwrap();
        assert_eq!(create.body.mode, BodyMode::Raw);
        assert_eq!(create.body.raw_language, RawLanguage::Json);
        assert_eq!(create.auth.kind, AuthType::Basic);
        assert_eq!(create.auth.get("username"), "admin");

        let get = reqs.iter().find(|r| r.name == "Get user").unwrap().http().unwrap();
        assert_eq!(get.path_vars, vec![KeyValue::new("id", "42")]);

        let upload = reqs.iter().find(|r| r.name == "Upload").unwrap().http().unwrap();
        assert_eq!(upload.body.mode, BodyMode::FormData);
        assert_eq!(upload.body.form_data[1].kind, FormFieldKind::File);
        assert_eq!(upload.body.form_data[1].files, vec!["/tmp/a.png"]);
        assert_eq!(upload.auth.kind, AuthType::None);

        let gql = reqs.iter().find(|r| r.name == "GraphQL").unwrap().http().unwrap();
        assert_eq!(gql.body.mode, BodyMode::GraphQl);
        assert!(gql.body.graphql.query.contains("viewer"));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn v20_auth_object_form_and_string_url() {
        let v = json!({
            "info": {"name": "Old", "schema": "https://schema.getpostman.com/json/collection/v2.0.0/collection.json"},
            "item": [{"name": "r", "request": {"url": "http://x/a?b=1", "method": "POST",
                "auth": {"type": "basic", "basic": {"username": "u", "password": "p"}},
                "header": "Content-Type: text/plain\n// X-Off: 1"}}]
        });
        let (c, _) = import_collection(&v).unwrap();
        let r = c.requests()[0].http().unwrap().clone();
        assert_eq!(r.auth.get("password"), "p");
        assert_eq!(r.params[0].key, "b");
        assert_eq!(r.headers.len(), 2);
        assert!(!r.headers[1].enabled);
    }

    #[test]
    fn roundtrip_is_identical() {
        let v: Value = serde_json::from_str(SAMPLE).unwrap();
        let (c, _) = import_collection(&v).unwrap();
        let opts = ExportOptions { include_secrets: true, ..Default::default() };
        let (exported, report) = export_collection(&c, opts);
        assert!(report.warnings.is_empty());
        let (again, _) = import_collection(&exported).unwrap();
        assert_eq!(c, again);
        // And a second export is byte-for-byte stable.
        let (exported2, _) = export_collection(&again, opts);
        assert_eq!(exported, exported2);
    }

    #[test]
    fn export_redacts_secrets_by_default() {
        let mut c = Collection::new("S");
        c.auth = Auth::basic("user", "hunter2");
        c.variables.push(Variable::secret("apiKey", "abc"));
        c.variables.push(Variable::new("base", "http://x"));
        let mut r = HttpRequest::new("GET", "http://x");
        r.auth = Auth::bearer("{{token}}");
        c.items.push(RequestItem::new("r", r).into());
        let (v, report) = export_collection(&c, ExportOptions::default());
        let text = v.to_string();
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("\"abc\""));
        assert!(text.contains("{{token}}"), "references are kept");
        assert_eq!(report.redacted, 2);
    }

    #[test]
    fn url_object_parts() {
        let u = export_url(
            "https://api.example.com:8443/v1/users/:id?x=1#top",
            &[KeyValue::new("x", "1")],
            &[KeyValue::new("id", "7")],
        );
        assert_eq!(u["protocol"], "https");
        assert_eq!(u["host"], json!(["api", "example", "com"]));
        assert_eq!(u["port"], "8443");
        assert_eq!(u["path"], json!(["v1", "users", ":id"]));
        assert_eq!(u["hash"], "top");
        let u = export_url("{{baseUrl}}/users", &[], &[]);
        assert_eq!(u["host"], json!(["{{baseUrl}}"]));
        assert_eq!(u["path"], json!(["users"]));
    }

    #[test]
    fn environment_roundtrip() {
        let v = json!({"id": "e1", "name": "Staging", "values": [
            {"key": "base", "value": "https://s", "type": "default", "enabled": true},
            {"key": "pw", "value": "x", "type": "secret", "enabled": false}
        ], "_postman_variable_scope": "environment"});
        assert_eq!(detect(&v), Some(PostmanKind::Environment));
        let env = import_environment(&v).unwrap();
        assert_eq!(env.variables.len(), 2);
        assert!(env.variables[1].is_secret());
        assert!(!env.variables[1].enabled);
        let (out, _) = export_environment(&env, ExportOptions { include_secrets: true, ..Default::default() });
        assert_eq!(import_environment(&out).unwrap(), env);
    }

    #[test]
    fn typed_variables_roundtrip() {
        let v = json!({"info": {"name": "T", "schema": SCHEMA_V21}, "item": [
            {"name": "F", "item": [], "variable": [{"key": "fb", "value": "1", "type": "boolean"}]}
        ], "variable": [
            {"key": "s", "value": "text", "type": "string"},
            {"key": "n", "value": 5, "type": "number"},
            {"key": "ns", "value": "1.50", "type": "number"},
            {"key": "b", "value": false, "type": "boolean"},
            {"key": "a", "value": {"x": [1, true, null]}, "type": "any"},
            {"key": "untyped", "value": 2.5},
            {"key": "plain", "value": "p"},
            {"key": "nil", "value": null},
            {"key": "missing", "type": "string"},
            {"key": "sec", "value": "k", "type": "secret"}
        ]});
        let (c, _) = import_collection(&v).unwrap();
        let got: Vec<(&str, &str, VarType)> =
            c.variables.iter().map(|v| (v.key.as_str(), v.value.as_str(), v.ty)).collect();
        assert_eq!(
            got,
            [
                ("s", "text", VarType::String),
                ("n", "5", VarType::Number),
                ("ns", "1.5", VarType::Number),
                ("b", "false", VarType::Boolean),
                ("a", r#"{"x":[1,true,null]}"#, VarType::Any),
                ("untyped", "2.5", VarType::Any),
                ("plain", "p", VarType::Default),
                ("nil", "null", VarType::Any),
                ("missing", "", VarType::String),
                ("sec", "k", VarType::Default),
            ]
        );
        assert!(c.variables[9].is_secret());
        let Item::Folder(f) = &c.items[0] else { panic!() };
        assert_eq!((f.variables[0].ty, f.variables[0].effective_json()), (VarType::Boolean, json!(true)));

        let (out, _) = export_collection(&c, all_secrets());
        assert_eq!(
            out["variable"],
            json!([
                {"key": "s", "value": "text", "type": "string"},
                {"key": "n", "value": 5, "type": "number"},
                {"key": "ns", "value": 1.5, "type": "number"},
                {"key": "b", "value": false, "type": "boolean"},
                {"key": "a", "value": {"x": [1, true, null]}},
                {"key": "untyped", "value": 2.5},
                {"key": "plain", "value": "p"},
                {"key": "nil", "value": null},
                {"key": "missing", "value": "", "type": "string"},
                {"key": "sec", "value": "k", "type": "secret"}
            ])
        );
        assert_eq!(out["item"][0]["variable"], json!([{"key": "fb", "value": true, "type": "boolean"}]));
        assert_eq!(roundtrip(&c), c);
        let (again, _) = export_collection(&roundtrip(&c), all_secrets());
        assert_eq!(again, out);

        // Current values are exported with their type too; NaN, which JSON
        // cannot carry, as text that `Number()` reads back.
        let mut c = c;
        c.variables[1].current = Some("NaN".into());
        let (out, _) = export_collection(&c, ExportOptions { include_secrets: true, use_current_values: true });
        assert_eq!(out["variable"][1], json!({"key": "n", "value": "NaN", "type": "number"}));
        assert_eq!(import_collection(&out).unwrap().0.variables[1].value, "NaN");
    }

    #[test]
    fn typed_environment_roundtrip() {
        let v = json!({"id": "e", "name": "E", "values": [
            {"key": "n", "value": 3, "type": "number", "enabled": true},
            {"key": "o", "value": [1, 2], "type": "default", "enabled": true},
            {"key": "b", "value": true, "type": "boolean", "enabled": true},
            {"key": "s", "value": 7, "type": "string", "enabled": true},
            {"key": "t", "value": "x", "type": "default", "enabled": true},
            {"key": "sec", "value": "k", "type": "secret", "enabled": true}
        ], "_postman_variable_scope": "environment"});
        let env = import_environment(&v).unwrap();
        let got: Vec<(&str, VarType)> = env.variables.iter().map(|v| (v.value.as_str(), v.ty)).collect();
        assert_eq!(
            got,
            [
                ("3", VarType::Number),
                ("[1,2]", VarType::Any),
                ("true", VarType::Boolean),
                ("7", VarType::String),
                ("x", VarType::Default),
                ("k", VarType::Default),
            ]
        );
        let (out, _) = export_environment(&env, all_secrets());
        let mut want = v["values"].clone();
        // `String(7)`.
        want[3]["value"] = json!("7");
        assert_eq!(out["values"], want);
        assert_eq!(import_environment(&out).unwrap(), env);
        let (out, _) = export_globals(&env.variables, all_secrets());
        assert_eq!(import_globals(&out).unwrap(), env.variables);
    }

    fn all_secrets() -> ExportOptions {
        ExportOptions { include_secrets: true, ..Default::default() }
    }

    fn roundtrip(c: &Collection) -> Collection {
        let (v, _) = export_collection(c, all_secrets());
        import_collection(&v).unwrap().0
    }

    #[test]
    fn handmade_roundtrip_is_identical() {
        let mut c = Collection::new("Full");
        c.description = "desc".into();
        c.auth = Auth::new(AuthType::OAuth2)
            .with("useBrowser", true)
            .with("n", 3)
            .with("authRequestParams", json!([{"key": "a", "value": "b", "enabled": true}]))
            .with("obj", json!({"x": 1}));
        c.scripts.pre_request = "a\nb".into();
        c.scripts.post_response = "c".into();
        c.variables.push(Variable::secret("s", "v"));
        let mut d = Variable::new("d", "x");
        d.enabled = false;
        d.description = "dd".into();
        c.variables.push(d);
        let mut f = Folder::new("F");
        f.auth = Auth::none();
        f.description = "fd".into();
        f.scripts.pre_request = "fp".into();
        f.variables.push(Variable::new("fv", "1"));
        f.items.push(Folder::new("Empty").into());
        let mut r = HttpRequest::new("POST", "https://h.test:8443/a/:id?x=1&y#frag");
        r.params.insert(1, KeyValue { description: "off".into(), ..KeyValue::new("z", "9").disabled() });
        r.params[0].description = "pd".into();
        r.path_vars[0].value = "7".into();
        r.path_vars[0].description = "the id".into();
        r.headers.push(KeyValue::new("A", "1"));
        r.headers.push(KeyValue { description: "hd".into(), ..KeyValue::new("B", "2").disabled() });
        r.body.mode = BodyMode::FormData;
        r.body.raw = "<a/>".into();
        r.body.raw_language = RawLanguage::Xml;
        r.body.urlencoded.push(KeyValue::new("u", "1").disabled());
        r.body.form_data.push(FormField::text("t", "v"));
        let mut ff = FormField::file("f", "/a");
        ff.files.push("/b".into());
        ff.content_type = Some("image/png".into());
        ff.enabled = false;
        ff.description = "fdesc".into();
        r.body.form_data.push(ff);
        r.body.binary_path = "/bin".into();
        r.body.graphql = GraphQlBody { query: "q".into(), variables: "{}".into(), operation_name: Some("Op".into()) };
        r.auth = Auth::basic("u", "p");
        r.settings = RequestSettings {
            follow_redirects: Some(false),
            max_redirects: Some(3),
            follow_original_method: Some(true),
            follow_auth_header: Some(true),
            verify_ssl: Some(false),
            encode_url: Some(false),
            disable_cookies: Some(true),
            timeout_ms: Some(10),
            http_version: Some(HttpVersion::Http2),
            disabled_default_headers: vec!["user-agent".into()],
        };
        let mut item = RequestItem::new("R", r.clone());
        item.description = "rd".into();
        item.scripts.post_response = "t".into();
        item.examples.push(Example {
            request: Some(HttpRequest::new("GET", "http://e")),
            status: 404,
            status_text: "Not Found".into(),
            headers: vec![KeyValue::new("H", "v")],
            body: "b".into(),
            language: Some("json".into()),
            ..Default::default()
        });
        f.items.push(item.into());
        c.items.push(f.into());
        assert_eq!(roundtrip(&c), c);
    }

    #[test]
    fn body_mode_none_keeps_data_as_disabled_body() {
        let mut c = Collection::new("B");
        let mut r = HttpRequest::new("POST", "http://h");
        r.body.raw = "keep me".into();
        r.body.raw_language = RawLanguage::Text;
        c.items.push(RequestItem::new("r", r).into());
        let (v, _) = export_collection(&c, all_secrets());
        let body = &v["item"][0]["request"]["body"];
        assert_eq!((&body["mode"], &body["disabled"], &body["raw"]), (&json!("raw"), &json!(true), &json!("keep me")));
        assert_eq!(roundtrip(&c), c);
        // A body without any data is still left out.
        let mut empty = Collection::new("E");
        empty.items.push(RequestItem::new("r", HttpRequest::new("POST", "http://h")).into());
        let (v, _) = export_collection(&empty, all_secrets());
        assert!(v["item"][0]["request"].get("body").is_none(), "{v}");
    }

    #[test]
    fn disabled_postman_body_is_not_sent() {
        let v = json!({"info": {"name": "E"}, "item": [{"name": "r", "request": {"method": "POST", "url": "http://h",
            "body": {"mode": "urlencoded", "disabled": true, "urlencoded": [{"key": "a", "value": "1"}]}}}]});
        let (c, report) = import_collection(&v).unwrap();
        let body = &c.requests()[0].http().unwrap().body;
        assert_eq!(body.mode, BodyMode::None);
        assert_eq!(body.urlencoded, vec![KeyValue::new("a", "1")]);
        assert!(report.warnings.is_empty(), "{report:?}");
        assert_eq!(roundtrip(&c), c);
    }

    #[test]
    fn raw_language_kept_without_raw_text() {
        let mut c = Collection::new("B");
        let mut r = HttpRequest::new("POST", "http://h");
        r.body.mode = BodyMode::UrlEncoded;
        r.body.raw_language = RawLanguage::Xml;
        c.items.push(RequestItem::new("r", r).into());
        assert_eq!(roundtrip(&c), c);
    }

    #[test]
    fn environment_and_globals_keep_descriptions() {
        let mut env = Environment::new("E");
        let mut v = Variable::new("k", "v");
        v.description = "d".into();
        env.variables.push(v);
        let (out, _) = export_environment(&env, all_secrets());
        assert_eq!(import_environment(&out).unwrap(), env);
        let (out, _) = export_globals(&env.variables, all_secrets());
        assert_eq!(import_globals(&out).unwrap(), env.variables);
    }

    #[test]
    fn scripts_of_the_same_event_are_merged() {
        let v = json!({"info": {"name": "E"}, "item": [{"name": "r", "request": {"url": "http://h"},
        "event": [
            {"listen": "test", "script": {"exec": ["const a = 1;"]}},
            {"listen": "prerequest", "script": {"exec": "pre();"}},
            {"listen": "test", "script": {"exec": ["const a = 2;"]}}
        ]}]});
        let (c, report) = import_collection(&v).unwrap();
        let s = &c.requests()[0].scripts;
        assert_eq!(s.post_response, "{\nconst a = 1;\n}\n{\nconst a = 2;\n}");
        assert_eq!(s.pre_request, "pre();");
        assert_eq!(report.warnings.len(), 1, "{report:?}");
    }

    #[test]
    fn disabled_events_are_not_imported() {
        let v = json!({"info": {"name": "E"}, "item": [{"name": "r", "request": {"url": "http://h"},
        "event": [
            {"listen": "test", "disabled": true, "script": {"exec": ["off();"]}},
            {"listen": "test", "script": {"exec": ["on();"], "packages": {}}}
        ]}]});
        let (c, report) = import_collection(&v).unwrap();
        assert_eq!(c.requests()[0].scripts.post_response, "on();");
        assert_eq!(report.warnings.len(), 1, "{report:?}");
        assert!(report.warnings[0].contains("disabled"));
    }

    #[test]
    fn unsupported_item_fields_are_reported() {
        let v = json!({"info": {"name": "E"}, "protocolProfileBehavior": {"tlsDisabledProtocols": ["TLSv1"]},
            "item": [{"name": "r", "request": {"url": "http://h"}, "variable": [{"key": "a", "value": "1"}],
            "protocolProfileBehavior": {"disableBodyPruning": true, "strictSSL": false, "insecureHTTPParser": true}}]});
        let (_, report) = import_collection(&v).unwrap();
        assert_eq!(report.warnings.len(), 3, "{report:?}");
        for needle in ["tlsDisabledProtocols", "insecureHTTPParser", "variables"] {
            assert!(report.warnings.iter().any(|w| w.contains(needle)), "{needle}: {report:?}");
        }
    }

    #[test]
    fn script_packages_are_reported() {
        let v = json!({"info": {"name": "E"}, "item": [], "event": [{"listen": "prerequest",
            "script": {"exec": ["const u = pm.require('@team/utils');"], "packages": {"@team/utils": {"id": "1"}}}}]});
        let (c, report) = import_collection(&v).unwrap();
        assert!(c.scripts.pre_request.contains("pm.require"));
        assert!(report.warnings.iter().any(|w| w.contains("packages")), "{report:?}");
    }

    /// Rebuilds a URL from its parts the way postman-collection's
    /// `Url#toString` does (newman ignores `raw`).
    fn sdk_url(u: &Value) -> String {
        let mut s = String::new();
        if let Some(p) = u.get("protocol").and_then(Value::as_str) {
            s.push_str(p);
            s.push_str("://");
        }
        if let Some(Value::Array(h)) = u.get("host") {
            s.push_str(&h.iter().map(|x| x.as_str().unwrap().to_string()).collect::<Vec<_>>().join("."));
        }
        if let Some(p) = u.get("port").and_then(Value::as_str) {
            s.push(':');
            s.push_str(p);
        }
        if let Some(Value::Array(p)) = u.get("path") {
            s.push('/');
            s.push_str(&p.iter().map(|x| x.as_str().unwrap().to_string()).collect::<Vec<_>>().join("/"));
        }
        if let Some(Value::Array(q)) = u.get("query") {
            let parts: Vec<String> = q
                .iter()
                .filter(|p| !p.get("disabled").and_then(Value::as_bool).unwrap_or(false))
                .map(|p| match p.get("value") {
                    Some(Value::String(v)) => format!("{}={v}", p["key"].as_str().unwrap()),
                    _ => p["key"].as_str().unwrap().to_string(),
                })
                .collect();
            if !parts.is_empty() {
                s.push('?');
                s.push_str(&parts.join("&"));
            }
        }
        if let Some(h) = u.get("hash").and_then(Value::as_str) {
            s.push('#');
            s.push_str(h);
        }
        s
    }

    #[test]
    fn url_parts_rebuild_raw() {
        for raw in [
            "{{protocol}}://{{host}}:{{port}}/api",
            "{{host}}:{{port}}/x",
            "{{baseUrl}}/proxy/http://x/y",
            "http://h//a",
            "http://[::1]/x",
            "http://[::1]:8080/x",
            "https://api.example.com:8443/v1/users/:id?x=1#top",
            "{{baseUrl}}/users?flag&b=&c=1",
            "http://h/",
            "http://h",
            "localhost:3000/x",
        ] {
            let r = HttpRequest::new("GET", raw);
            let u = export_url(&r.url, &r.params, &r.path_vars);
            assert_eq!(sdk_url(&u), raw, "{u}");
        }
        let u = export_url("{{protocol}}://{{host}}:{{port}}/api", &[], &[]);
        assert_eq!(
            (&u["protocol"], &u["host"], &u["port"]),
            (&json!("{{protocol}}"), &json!(["{{host}}"]), &json!("{{port}}"))
        );
        let u = export_url("http://[::1]/x", &[], &[]);
        assert_eq!((&u["host"], u.get("port")), (&json!(["[::1]"]), None));
        let r = HttpRequest::new("GET", "http://h/?flag&b=");
        let u = export_url(&r.url, &r.params, &r.path_vars);
        assert_eq!(u["query"], json!([{"key": "flag", "value": null}, {"key": "b", "value": ""}]));
        assert_eq!(import_url(Some(&u)), (r.url.clone(), r.params.clone(), r.path_vars.clone()));
    }

    fn get_with_body(ppb: Option<Value>) -> Value {
        let mut item = json!({"name": "r", "request": {"method": "GET", "url": "http://h",
            "body": {"mode": "raw", "raw": "{}", "options": {"raw": {"language": "json"}}}}});
        if let Some(ppb) = ppb {
            item["protocolProfileBehavior"] = ppb;
        }
        item
    }

    #[test]
    fn get_body_pruning_follows_postman() {
        // Merpati sends a GET body, so the export must tell Postman to as well.
        let mut c = Collection::new("B");
        let mut r = HttpRequest::new("get", "http://h");
        r.body = Body::raw_json("{}");
        c.items.push(RequestItem::new("r", r.clone()).into());
        r.body.raw.clear();
        c.items.push(RequestItem::new("empty", r.clone()).into());
        r.method = "POST".into();
        r.body.raw = "{}".into();
        c.items.push(RequestItem::new("post", r).into());
        let (v, _) = export_collection(&c, all_secrets());
        assert_eq!(v["item"][0]["protocolProfileBehavior"], json!({"disableBodyPruning": true}), "{v}");
        assert!(v["item"][1].get("protocolProfileBehavior").is_none(), "{v}");
        assert!(v["item"][2].get("protocolProfileBehavior").is_none(), "{v}");
        assert_eq!(roundtrip(&c), c);

        // Postman does not send a GET body without the flag, set here or inherited.
        let flag = || Some(json!({"disableBodyPruning": true}));
        let v = json!({"info": {"name": "P"}, "item": [
            get_with_body(None),
            get_with_body(flag()),
            {"name": "F", "protocolProfileBehavior": {"disableBodyPruning": true}, "item": [get_with_body(None)]},
        ]});
        let (c, report) = import_collection(&v).unwrap();
        let modes: Vec<BodyMode> = c.requests().iter().map(|r| r.http().unwrap().body.mode).collect();
        assert_eq!(modes, [BodyMode::None, BodyMode::Raw, BodyMode::Raw]);
        assert_eq!(c.requests()[0].http().unwrap().body.raw, "{}");
        assert_eq!(report.warnings.len(), 1, "{report:?}");
        assert_eq!(roundtrip(&c), c);
        let root = json!({"info": {"name": "P"}, "protocolProfileBehavior": {"disableBodyPruning": true},
            "item": [get_with_body(None)]});
        assert_eq!(import_collection(&root).unwrap().0.requests()[0].http().unwrap().body.mode, BodyMode::Raw);
    }

    /// Every field replaced by values of the wrong JSON type: import must not
    /// panic, and what it produces must round trip.
    #[test]
    fn wrong_json_types_never_panic_and_roundtrip() {
        let base = json!({
            "info": {"_postman_id": "c", "name": "N", "description": "d", "schema": SCHEMA_V21},
            "protocolProfileBehavior": {"strictSSL": false},
            "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "t", "type": "string"}]},
            "event": [{"listen": "prerequest", "script": {"exec": ["a"]}}],
            "variable": [{"key": "k", "value": "v"}],
            "item": [
                {"id": "r1", "name": "R",
                 "protocolProfileBehavior": {"followRedirects": false, "disabledSystemHeaders": {"accept": true}},
                 "event": [{"listen": "test", "script": {"exec": ["t"]}}],
                 "request": {"method": "POST", "auth": {"type": "basic", "basic": [{"key": "username", "value": "u"}]},
                    "header": [{"key": "h", "value": "v"}],
                    "body": {"mode": "formdata", "raw": "r", "options": {"raw": {"language": "json"}},
                        "urlencoded": [{"key": "u", "value": "1"}], "formdata": [{"key": "f", "type": "file", "src": "/a"}],
                        "file": {"src": "/b"}, "graphql": {"query": "q"}},
                    "url": {"raw": "https://h:1/p/:id?q=1#x", "protocol": "https", "host": ["h"], "port": "1",
                        "path": ["p", ":id"], "query": [{"key": "q", "value": "1"}],
                        "variable": [{"key": "id", "value": "2"}], "hash": "x"}},
                 "response": [{"name": "e", "code": 201, "header": [{"key": "a", "value": "b"}],
                    "originalRequest": {"method": "GET", "url": "http://o"}, "body": "x"}]},
                {"id": "f1", "name": "F", "description": "fd", "item": [{"name": "inner", "request": "http://i"}]}
            ]
        });
        let odd = [json!(null), json!(1), json!("s"), json!([1]), json!({"a": 1}), json!(true), json!([null, {}])];
        let paths = [
            "/info",
            "/item",
            "/auth",
            "/auth/bearer",
            "/event",
            "/variable",
            "/variable/0/value",
            "/protocolProfileBehavior",
            "/item/0",
            "/item/0/name",
            "/item/0/request",
            "/item/0/request/method",
            "/item/0/request/url",
            "/item/0/request/url/host",
            "/item/0/request/url/path",
            "/item/0/request/url/query",
            "/item/0/request/url/query/0/value",
            "/item/0/request/url/variable",
            "/item/0/request/url/port",
            "/item/0/request/url/hash",
            "/item/0/request/url/protocol",
            "/item/0/request/header",
            "/item/0/request/body",
            "/item/0/request/body/mode",
            "/item/0/request/body/raw",
            "/item/0/request/body/options",
            "/item/0/request/body/urlencoded",
            "/item/0/request/body/formdata",
            "/item/0/request/body/formdata/0/src",
            "/item/0/request/body/file",
            "/item/0/request/body/graphql",
            "/item/0/request/auth",
            "/item/0/request/auth/type",
            "/item/0/request/auth/basic",
            "/item/0/response",
            "/item/0/response/0",
            "/item/0/response/0/code",
            "/item/0/response/0/header",
            "/item/0/response/0/originalRequest",
            "/item/0/event",
            "/item/0/event/0/script",
            "/item/0/event/0/script/exec",
            "/item/0/protocolProfileBehavior",
            "/item/0/protocolProfileBehavior/disabledSystemHeaders",
            "/item/1/item",
            "/item/1/description",
            "/item/1/item/0/request",
        ];
        assert!(import_collection(&base).unwrap().1.warnings.is_empty());
        for o in &odd {
            for path in paths {
                let mut d = base.clone();
                *d.pointer_mut(path).unwrap_or_else(|| panic!("{path}")) = o.clone();
                if let Ok((PostmanFile::Collection(c), _)) = import_any(&d) {
                    assert_eq!(roundtrip(&c), c, "{path} = {o}");
                }
            }
        }
        let env = json!({"values": [1, null, {"key": 1, "value": {}, "enabled": "x", "type": 5}]});
        assert!(matches!(import_any(&env), Ok((PostmanFile::Environment(_), _))));
        for raw in ["", "://", "{{", "}}:{{", "http://[", "a:", ":1", "http://é:é/é?é#é", "{{a://b}}/x", "?#"] {
            let _ = export_url(raw, &urlutil::merge_params_from_url(raw, &[]), &[]);
        }
    }

    #[test]
    fn out_of_range_numbers_are_not_truncated() {
        let v = json!({"info": {"name": "E"}, "item": [{"name": "r", "request": {"url": "http://h"},
            "protocolProfileBehavior": {"maxRedirects": 4294967297u64},
            "response": [{"name": "x", "code": 70000}]}]});
        let (c, _) = import_collection(&v).unwrap();
        let r = c.requests()[0];
        // 70000 as u16 would be 4464, 2^32 + 1 as u32 would be 1.
        assert_eq!(r.examples[0].status, 200);
        assert_eq!(r.http().unwrap().settings.max_redirects, Some(u32::MAX));
    }
}
