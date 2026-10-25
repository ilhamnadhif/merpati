//! Insomnia import: v4 export JSON (`"_type": "export"`) and the v5 YAML
//! collection format (`type: collection.insomnia.rest/5.0`).
//!
//! Nunjucks templates are converted where Merpati has an equivalent:
//! `{{ _.var }}` -> `{{var}}`, `{% uuid %}` -> `{{$guid}}`, `{% now %}` ->
//! `{{$timestamp}}`/`{{$isoTimestamp}}`, `{% faker 'x' %}` -> `{{$x}}`.
//! Other tags (response chaining, base64, ...) are kept verbatim and reported.

use std::collections::HashMap;

use indexmap::IndexMap;
use merpati_core::model::{
    Body, BodyMode, Collection, Folder, FormField, FormFieldKind, HttpRequest, Item, KeyValue, RawLanguage,
    RequestItem, RequestKind, Scripts, WsRequest,
};
use merpati_core::variables::Variable;
use merpati_core::{Auth, AuthType, Environment};
use serde_json::Value;

use crate::ConvertError;
use crate::util::{arr, as_string, raw_language_for, str_of};

pub(crate) struct Templates<'w> {
    pub warnings: &'w mut Vec<String>,
}

impl Templates<'_> {
    fn warn(&mut self, msg: String) {
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    /// Converts Insomnia/Nunjucks template syntax to Merpati's.
    pub fn convert(&mut self, s: &str) -> String {
        if !s.contains("{{") && !s.contains("{%") {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        loop {
            let next_var = rest.find("{{");
            let next_tag = rest.find("{%");
            let (start, is_tag) = match (next_var, next_tag) {
                (Some(v), Some(t)) if t < v => (t, true),
                (Some(v), _) => (v, false),
                (None, Some(t)) => (t, true),
                (None, None) => break,
            };
            out.push_str(&rest[..start]);
            let close = if is_tag { "%}" } else { "}}" };
            let Some(end) = rest[start + 2..].find(close) else {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            };
            let inner = rest[start + 2..start + 2 + end].trim();
            let original = &rest[start..start + 2 + end + 2];
            if is_tag {
                out.push_str(&self.tag(inner, original));
            } else {
                out.push_str(&format!("{{{{{}}}}}", variable_name(inner)));
            }
            rest = &rest[start + 2 + end + 2..];
        }
        out.push_str(rest);
        out
    }

    fn tag(&mut self, inner: &str, original: &str) -> String {
        let name: String = inner.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        let args: Vec<String> = inner[name.len()..]
            .split(',')
            .map(|a| a.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
            .filter(|a| !a.is_empty())
            .collect();
        match name.as_str() {
            "uuid" => "{{$guid}}".into(),
            "now" => match args.first().map(String::as_str) {
                Some("iso-8601") => "{{$isoTimestamp}}".into(),
                Some("millis") => {
                    self.warn("`{% now 'millis' %}` became {{$timestamp}} (seconds)".into());
                    "{{$timestamp}}".into()
                }
                _ => "{{$timestamp}}".into(),
            },
            "faker" => match args.first() {
                Some(f) => format!("{{{{${f}}}}}"),
                None => original.to_string(),
            },
            other => {
                self.warn(format!("template tag `{{% {other} %}}` is not supported and was kept as text"));
                original.to_string()
            }
        }
    }
}

/// `_.foo`, `_['foo-bar']`, `foo` -> `foo`.
fn variable_name(inner: &str) -> String {
    let s = inner.trim();
    let s = s.strip_prefix("_.").unwrap_or(s);
    if let Some(rest) = s.strip_prefix("_[") {
        return rest.trim_end_matches(']').trim_matches(|c| c == '\'' || c == '"').to_string();
    }
    s.to_string()
}

/// Flattens environment data (`{"a": {"b": 1}}` -> `a.b = 1`).
fn flatten(prefix: &str, v: &Value, out: &mut Vec<Variable>, t: &mut Templates) {
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, child, out, t);
            }
        }
        other => {
            if !prefix.is_empty() {
                let value = match other {
                    Value::Array(_) => other.to_string(),
                    o => as_string(o),
                };
                out.push(Variable::new(prefix, t.convert(&value)));
            }
        }
    }
}

fn kv_list(list: &[Value], t: &mut Templates) -> Vec<KeyValue> {
    list.iter()
        .filter(|p| !str_of(p, "name").is_empty() || !str_of(p, "value").is_empty())
        .map(|p| KeyValue {
            key: t.convert(&str_of(p, "name")),
            value: t.convert(&str_of(p, "value")),
            enabled: !p.get("disabled").and_then(Value::as_bool).unwrap_or(false),
            description: str_of(p, "description"),
        })
        .collect()
}

pub(crate) fn convert_auth(a: Option<&Value>, t: &mut Templates) -> Auth {
    let Some(a) = a.filter(|a| a.as_object().map(|o| !o.is_empty()).unwrap_or(false)) else {
        return Auth::default();
    };
    if a.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
        return Auth::none();
    }
    let s = |k: &str, t: &mut Templates| t.convert(&str_of(a, k));
    match a.get("type").and_then(Value::as_str).unwrap_or("") {
        "none" => Auth::none(),
        "inherit" => Auth::default(),
        "basic" => Auth::basic(s("username", t), s("password", t)),
        "bearer" => {
            let prefix = str_of(a, "prefix");
            let token = s("token", t);
            if prefix.is_empty() || prefix.eq_ignore_ascii_case("bearer") {
                Auth::bearer(token)
            } else {
                Auth::api_key("Authorization", format!("{prefix} {token}"), false)
            }
        }
        "apikey" => {
            let add_to = str_of(a, "addTo");
            if add_to == "cookie" {
                t.warn("API key auth sent as a cookie was imported as a Cookie header key".into());
                return Auth::api_key("Cookie", format!("{}={}", s("key", t), s("value", t)), false);
            }
            Auth::api_key(s("key", t), s("value", t), add_to == "queryParams")
        }
        "digest" => Auth::new(AuthType::Digest).with("username", s("username", t)).with("password", s("password", t)),
        "ntlm" => {
            let user = s("username", t);
            let (domain, name) = match user.split_once('\\') {
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (String::new(), user),
            };
            Auth::new(AuthType::Ntlm).with("username", name).with("password", s("password", t)).with("domain", domain)
        }
        "oauth1" => Auth::new(AuthType::OAuth1)
            .with("consumerKey", s("consumerKey", t))
            .with("consumerSecret", s("consumerSecret", t))
            .with("token", s("tokenKey", t))
            .with("tokenSecret", s("tokenSecret", t))
            .with("signatureMethod", str_of(a, "signatureMethod"))
            .with("realm", s("realm", t))
            .with("callback", s("callback", t))
            .with("verifier", s("verifier", t))
            .with("nonce", s("nonce", t))
            .with("timestamp", s("timestamp", t))
            .with("version", if str_of(a, "version").is_empty() { "1.0".to_string() } else { str_of(a, "version") })
            .with("privateKey", s("privateKey", t)),
        "oauth2" => {
            let pkce = a.get("usePkce").and_then(Value::as_bool).unwrap_or(false);
            let grant = match str_of(a, "grantType").as_str() {
                "authorization_code" if pkce => "authorization_code_with_pkce",
                "authorization_code" => "authorization_code",
                "client_credentials" => "client_credentials",
                "password" => "password_credentials",
                "implicit" => "implicit",
                other => {
                    t.warn(format!("OAuth 2.0 grant type `{other}` imported as authorization_code"));
                    "authorization_code"
                }
            };
            let mut auth = Auth::new(AuthType::OAuth2)
                .with("grant_type", grant)
                .with("accessTokenUrl", s("accessTokenUrl", t))
                .with("authUrl", s("authorizationUrl", t))
                .with("clientId", s("clientId", t))
                .with("clientSecret", s("clientSecret", t))
                .with("scope", s("scope", t))
                .with("state", s("state", t))
                .with("redirect_uri", s("redirectUrl", t))
                .with("addTokenTo", "header")
                .with(
                    "client_authentication",
                    if a.get("credentialsInBody").and_then(Value::as_bool).unwrap_or(false) {
                        "body"
                    } else {
                        "header"
                    },
                );
            if grant == "password_credentials" {
                auth.set("username", s("username", t));
                auth.set("password", s("password", t));
            }
            if pkce {
                auth.set("challengeAlgorithm", if str_of(a, "pkceMethod") == "plain" { "plain" } else { "S256" });
            }
            for (from, to) in [("audience", "audience"), ("resource", "resource"), ("tokenPrefix", "headerPrefix")] {
                let v = s(from, t);
                if !v.is_empty() {
                    auth.set(to, v);
                }
            }
            auth
        }
        "hawk" => Auth::new(AuthType::Hawk)
            .with("authId", s("id", t))
            .with("authKey", s("key", t))
            .with(
                "algorithm",
                if str_of(a, "algorithm").is_empty() { "sha256".to_string() } else { str_of(a, "algorithm") },
            )
            .with("extraData", s("ext", t)),
        "iam" => Auth::new(AuthType::AwsV4)
            .with("accessKey", s("accessKeyId", t))
            .with("secretKey", s("secretAccessKey", t))
            .with("sessionToken", s("sessionToken", t))
            .with("region", s("region", t))
            .with("service", s("service", t)),
        "asap" => Auth::new(AuthType::Asap)
            .with("issuer", s("issuer", t))
            .with("subject", s("subject", t))
            .with("audience", s("audience", t))
            .with("kid", s("keyId", t))
            .with("privateKey", s("privateKey", t))
            .with("claims", s("additionalClaims", t)),
        other => {
            t.warn(format!("Insomnia auth type `{other}` is not supported"));
            Auth::default()
        }
    }
}

fn convert_body(b: Option<&Value>, req: &mut HttpRequest, t: &mut Templates) {
    let Some(b) = b.filter(|b| b.is_object()) else { return };
    let mime = str_of(b, "mimeType");
    let lower = mime.to_ascii_lowercase();
    let text = b.get("text").and_then(Value::as_str);
    if lower.contains("graphql") {
        req.body.mode = BodyMode::GraphQl;
        let parsed: Value = text.and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
        req.body.graphql.query = t.convert(&str_of(&parsed, "query"));
        req.body.graphql.variables = match parsed.get("variables") {
            Some(Value::Null) | None => String::new(),
            Some(Value::String(s)) => t.convert(s),
            Some(v) => t.convert(&serde_json::to_string_pretty(v).unwrap_or_default()),
        };
        req.body.graphql.operation_name =
            parsed.get("operationName").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        // Insomnia sends GraphQL as JSON.
        req.headers.retain(|h| !h.key.eq_ignore_ascii_case("content-type") || !h.value.contains("graphql"));
    } else if lower.contains("x-www-form-urlencoded") {
        req.body.mode = BodyMode::UrlEncoded;
        req.body.urlencoded = kv_list(arr(b, "params"), t);
    } else if lower.contains("multipart/form-data") {
        req.body.mode = BodyMode::FormData;
        req.body.form_data = arr(b, "params")
            .iter()
            .map(|p| {
                let is_file = p.get("type").and_then(Value::as_str) == Some("file");
                let mut f = if is_file {
                    let file = str_of(p, "fileName");
                    FormField {
                        key: t.convert(&str_of(p, "name")),
                        kind: FormFieldKind::File,
                        files: if file.is_empty() { Vec::new() } else { vec![file] },
                        enabled: true,
                        ..Default::default()
                    }
                } else {
                    FormField::text(t.convert(&str_of(p, "name")), t.convert(&str_of(p, "value")))
                };
                f.enabled = !p.get("disabled").and_then(Value::as_bool).unwrap_or(false);
                f.description = str_of(p, "description");
                f
            })
            .collect();
        // The recorded Content-Type has no boundary; let the client set it.
        req.headers.retain(|h| !h.key.eq_ignore_ascii_case("content-type"));
    } else if let Some(file) = b.get("fileName").and_then(Value::as_str) {
        req.body.mode = BodyMode::Binary;
        req.body.binary_path = file.to_string();
    } else if let Some(text) = text {
        let lang = if mime.is_empty() { RawLanguage::Text } else { raw_language_for(&lower) };
        req.body = Body::raw_text(t.convert(text), lang);
    }
}

fn convert_script(code: &str, t: &mut Templates) -> String {
    if code.contains("insomnia.") {
        t.warn("Insomnia scripts were converted by renaming `insomnia.` to `pm.`; review them".into());
        code.replace("insomnia.", "pm.")
    } else {
        code.to_string()
    }
}

fn convert_request(r: &Value, t: &mut Templates) -> RequestItem {
    let url = t.convert(&str_of(r, "url"));
    let method = str_of(r, "method");
    let mut req = HttpRequest::new(if method.is_empty() { "GET".to_string() } else { method.to_uppercase() }, url);
    let extra = kv_list(arr(r, "parameters"), t);
    if !extra.is_empty() {
        req.params.extend(extra);
        req.sync_url_from_params();
    }
    for p in arr(r, "pathParameters") {
        let name = str_of(p, "name");
        let value = t.convert(&str_of(p, "value"));
        if let Some(pv) = req.path_vars.iter_mut().find(|v| v.key == name) {
            pv.value = value;
        }
    }
    req.headers = kv_list(arr(r, "headers"), t);
    convert_body(r.get("body"), &mut req, t);
    req.auth = convert_auth(r.get("authentication"), t);
    // v4 keeps settings at the top level, v5 under `settings`.
    let settings = r.get("settings").unwrap_or(r);
    let follow =
        settings.get("settingFollowRedirects").or_else(|| settings.get("followRedirects")).and_then(Value::as_str);
    match follow {
        Some("on") => req.settings.follow_redirects = Some(true),
        Some("off") => req.settings.follow_redirects = Some(false),
        _ => {}
    }
    if let Some(enc) = settings.get("settingEncodeUrl").or_else(|| settings.get("encodeUrl")).and_then(Value::as_bool) {
        req.settings.encode_url = Some(enc);
    }
    if settings.get("settingSendCookies").and_then(Value::as_bool) == Some(false) {
        req.settings.disable_cookies = Some(true);
    }
    let scripts_v = r.get("scripts");
    let pre = r
        .get("preRequestScript")
        .or_else(|| scripts_v.and_then(|s| s.get("preRequest")))
        .and_then(Value::as_str)
        .unwrap_or("");
    let post = r
        .get("afterResponseScript")
        .or_else(|| scripts_v.and_then(|s| s.get("afterResponse")))
        .and_then(Value::as_str)
        .unwrap_or("");
    let description =
        r.get("description").or_else(|| r.pointer("/meta/description")).map(as_string).unwrap_or_default();
    let mut item = RequestItem::new(str_of(r, "name"), req);
    if item.name.is_empty() {
        item.name = crate::util::request_name_from_url(
            &item.http().map(|h| h.method.clone()).unwrap_or_default(),
            &str_of(r, "url"),
        );
    }
    item.description = description;
    item.scripts =
        Scripts { pre_request: convert_script(pre, t), post_response: convert_script(post, t), ..Default::default() };
    item
}

fn convert_websocket(r: &Value, t: &mut Templates) -> RequestItem {
    let ws = WsRequest {
        url: t.convert(&str_of(r, "url")),
        headers: kv_list(arr(r, "headers"), t),
        auth: convert_auth(r.get("authentication"), t),
        ..Default::default()
    };
    let mut item = RequestItem::new(str_of(r, "name"), HttpRequest::default());
    item.request = RequestKind::WebSocket(ws);
    item.description = str_of(r, "description");
    item
}

fn sort_key(v: &Value) -> f64 {
    v.get("metaSortKey").or_else(|| v.pointer("/meta/sortKey")).and_then(Value::as_f64).unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// v4
// ---------------------------------------------------------------------------

fn import_v4(v: &Value, warnings: &mut Vec<String>) -> (Vec<Collection>, Vec<Environment>) {
    let resources = arr(v, "resources");
    let mut by_parent: HashMap<String, Vec<&Value>> = HashMap::new();
    for r in resources {
        by_parent.entry(str_of(r, "parentId")).or_default().push(r);
    }
    for list in by_parent.values_mut() {
        list.sort_by(|a, b| sort_key(a).partial_cmp(&sort_key(b)).unwrap_or(std::cmp::Ordering::Equal));
    }
    let workspaces: Vec<&Value> = resources.iter().filter(|r| str_of(r, "_type") == "workspace").collect();
    let multiple = workspaces.len() > 1;
    let mut t = Templates { warnings };
    let mut collections = Vec::new();
    let mut environments = Vec::new();
    let mut skipped: IndexMap<String, usize> = IndexMap::new();

    /// `open` holds the ids being walked: a group whose `_id` is missing or
    /// repeats an enclosing one would recurse forever.
    fn children(
        parent: &str,
        by_parent: &HashMap<String, Vec<&Value>>,
        t: &mut Templates,
        skipped: &mut IndexMap<String, usize>,
        open: &mut Vec<String>,
    ) -> Vec<Item> {
        let mut items = Vec::new();
        open.push(parent.to_string());
        for r in by_parent.get(parent).map(Vec::as_slice).unwrap_or(&[]) {
            match str_of(r, "_type").as_str() {
                "request" => items.push(Item::Request(convert_request(r, t))),
                "websocket_request" => items.push(Item::Request(convert_websocket(r, t))),
                "request_group" => {
                    let mut f = Folder::new(str_of(r, "name"));
                    f.description = str_of(r, "description");
                    f.auth = convert_auth(r.get("authentication"), t);
                    if let Some(env) = r.get("environment") {
                        flatten("", env, &mut f.variables, t);
                    }
                    f.scripts = Scripts {
                        pre_request: convert_script(r.get("preRequestScript").and_then(Value::as_str).unwrap_or(""), t),
                        post_response: convert_script(
                            r.get("afterResponseScript").and_then(Value::as_str).unwrap_or(""),
                            t,
                        ),
                        ..Default::default()
                    };
                    let id = str_of(r, "_id");
                    if open.contains(&id) {
                        t.warn(format!("Insomnia folder `{}` contains itself; its contents were skipped", f.name));
                    } else {
                        f.items = children(&id, by_parent, t, skipped, open);
                    }
                    items.push(Item::Folder(f));
                }
                "environment" | "cookie_jar" | "api_spec" | "unit_test_suite" | "unit_test" | "proto_file"
                | "proto_directory" | "mock_server" | "mock_route" => {
                    if !matches!(str_of(r, "_type").as_str(), "environment" | "cookie_jar") {
                        *skipped.entry(str_of(r, "_type")).or_default() += 1;
                    }
                }
                other => *skipped.entry(other.to_string()).or_default() += 1,
            }
        }
        open.pop();
        items
    }

    for ws in &workspaces {
        let id = str_of(ws, "_id");
        let mut c = Collection::new(str_of(ws, "name"));
        c.description = str_of(ws, "description");
        c.items = children(&id, &by_parent, &mut t, &mut skipped, &mut Vec::new());
        // Base environment -> collection variables; sub environments -> environments.
        for env in by_parent.get(&id).map(Vec::as_slice).unwrap_or(&[]) {
            if str_of(env, "_type") != "environment" {
                continue;
            }
            if let Some(data) = env.get("data") {
                flatten("", data, &mut c.variables, &mut t);
            }
            for sub in by_parent.get(&str_of(env, "_id")).map(Vec::as_slice).unwrap_or(&[]) {
                if str_of(sub, "_type") != "environment" {
                    continue;
                }
                let name = if multiple { format!("{} - {}", c.name, str_of(sub, "name")) } else { str_of(sub, "name") };
                let mut e = Environment::new(name);
                if let Some(data) = sub.get("data") {
                    flatten("", data, &mut e.variables, &mut t);
                }
                environments.push(e);
            }
        }
        collections.push(c);
    }
    if workspaces.is_empty() {
        // Partial exports (a folder or request) have no workspace resource.
        let ids: std::collections::HashSet<String> = resources.iter().map(|r| str_of(r, "_id")).collect();
        let mut c = Collection::new("Insomnia import");
        let roots: Vec<String> = by_parent.keys().filter(|p| !ids.contains(*p)).cloned().collect();
        for root in roots {
            c.items.extend(children(&root, &by_parent, &mut t, &mut skipped, &mut Vec::new()));
        }
        collections.push(c);
    }
    for (kind, n) in skipped {
        t.warn(format!("{n} Insomnia `{kind}` resource(s) were not imported"));
    }
    (collections, environments)
}

// ---------------------------------------------------------------------------
// v5
// ---------------------------------------------------------------------------

fn v5_items(list: &[Value], t: &mut Templates) -> Vec<Item> {
    let mut sorted: Vec<&Value> = list.iter().collect();
    sorted.sort_by(|a, b| sort_key(a).partial_cmp(&sort_key(b)).unwrap_or(std::cmp::Ordering::Equal));
    let mut items = Vec::new();
    for v in sorted {
        if v.get("children").is_some() || (v.get("url").is_none() && v.get("method").is_none()) {
            let mut f = Folder::new(str_of(v, "name"));
            f.description = v.pointer("/meta/description").map(as_string).unwrap_or_default();
            f.auth = convert_auth(v.get("authentication"), t);
            if let Some(env) = v.get("environment") {
                flatten("", env, &mut f.variables, t);
            }
            if let Some(s) = v.get("scripts") {
                f.scripts = Scripts {
                    pre_request: convert_script(s.get("preRequest").and_then(Value::as_str).unwrap_or(""), t),
                    post_response: convert_script(s.get("afterResponse").and_then(Value::as_str).unwrap_or(""), t),
                    ..Default::default()
                };
            }
            f.items = v5_items(arr(v, "children"), t);
            items.push(Item::Folder(f));
        } else if str_of(v, "url").starts_with("ws") && v.get("method").is_none() {
            items.push(Item::Request(convert_websocket(v, t)));
        } else {
            items.push(Item::Request(convert_request(v, t)));
        }
    }
    items
}

fn import_v5(v: &Value, warnings: &mut Vec<String>) -> (Vec<Collection>, Vec<Environment>) {
    let mut t = Templates { warnings };
    let ty = str_of(v, "type");
    let mut collections = Vec::new();
    let mut environments = Vec::new();
    let envs = v.get("environments");
    let mut base_vars = Vec::new();
    if let Some(env) = envs {
        if let Some(data) = env.get("data") {
            flatten("", data, &mut base_vars, &mut t);
        }
        for sub in arr(env, "subEnvironments") {
            let mut e = Environment::new(str_of(sub, "name"));
            if let Some(data) = sub.get("data") {
                flatten("", data, &mut e.variables, &mut t);
            }
            environments.push(e);
        }
    }
    if ty.starts_with("collection.insomnia.rest") {
        let mut c = Collection::new(str_of(v, "name"));
        c.description = v.pointer("/meta/description").map(as_string).unwrap_or_default();
        c.items = v5_items(arr(v, "collection"), &mut t);
        c.variables = base_vars;
        collections.push(c);
    } else if ty.starts_with("environment.insomnia.rest") {
        let mut e = Environment::new(str_of(v, "name"));
        e.variables = base_vars;
        environments.insert(0, e);
    } else {
        t.warn(format!("Insomnia document type `{ty}` is not supported"));
    }
    (collections, environments)
}

/// Imports an Insomnia export. Returns collections, environments and warnings.
#[allow(clippy::type_complexity)]
pub fn import(text: &str) -> Result<(Vec<Collection>, Vec<Environment>, Vec<String>), ConvertError> {
    let v = crate::parse_structured(text)?;
    let mut warnings = Vec::new();
    let (c, e) = if v.get("_type").and_then(Value::as_str) == Some("export") {
        let fmt = v.get("__export_format").and_then(Value::as_u64).unwrap_or(4);
        if fmt < 3 {
            return Err(ConvertError::Format(format!(
                "Insomnia export format {fmt} is too old; re-export from Insomnia"
            )));
        }
        import_v4(&v, &mut warnings)
    } else if v.get("type").and_then(Value::as_str).map(|t| t.contains("insomnia.rest")).unwrap_or(false) {
        import_v5(&v, &mut warnings)
    } else {
        return Err(ConvertError::Format("not an Insomnia export".into()));
    };
    Ok((c, e, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        let mut w = Vec::new();
        let mut t = Templates { warnings: &mut w };
        assert_eq!(t.convert("{{ _.base_url }}/a/{{token}}"), "{{base_url}}/a/{{token}}");
        assert_eq!(t.convert("{{ _['api-key'] }}"), "{{api-key}}");
        assert_eq!(t.convert("id={% uuid 'v4' %}&t={% now 'iso-8601' %}"), "id={{$guid}}&t={{$isoTimestamp}}");
        assert_eq!(t.convert("{% faker 'randomEmail' %}"), "{{$randomEmail}}");
        assert_eq!(t.convert("{% response 'body', 'req_1', '$.id' %}"), "{% response 'body', 'req_1', '$.id' %}");
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn imports_v4() {
        let (cols, envs, warnings) = import(include_str!("../testdata/insomnia_v4.json")).unwrap();
        assert_eq!(cols.len(), 1);
        let c = &cols[0];
        assert_eq!(c.name, "Demo API");
        assert_eq!(c.variables.iter().find(|v| v.key == "base_url").unwrap().value, "http://localhost:3000");
        assert_eq!(c.variables.iter().find(|v| v.key == "auth.user").unwrap().value, "admin");
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].name, "Staging");
        assert_eq!(envs[0].get("base_url"), Some("https://staging.example.com"));

        // Sorted by metaSortKey: folder first, then the root request.
        assert_eq!(c.items[0].name(), "Users");
        let users = c.items[0].as_folder().unwrap();
        assert_eq!(users.variables[0].key, "page_size");
        let list = users.items[0].as_request().unwrap();
        assert_eq!(list.name, "List users");
        let h = list.http().unwrap();
        assert_eq!(h.url, "{{base_url}}/users?page=1");
        assert_eq!(h.params.len(), 2);
        assert!(!h.params[1].enabled);
        assert_eq!(h.auth.kind, AuthType::Bearer);
        assert_eq!(h.auth.get("token"), "{{token}}");
        assert!(list.scripts.post_response.contains("pm.test"));

        let create = users.items[1].as_request().unwrap().http().unwrap();
        assert_eq!(create.body.mode, BodyMode::Raw);
        assert_eq!(create.body.raw_language, RawLanguage::Json);
        assert!(create.body.raw.contains("{{$guid}}"));
        assert_eq!(create.auth.kind, AuthType::Basic);
        assert_eq!(create.settings.follow_redirects, Some(false));

        let gql = c.items[1].as_request().unwrap().http().unwrap();
        assert_eq!(gql.body.mode, BodyMode::GraphQl);
        assert_eq!(gql.body.graphql.query, "{ me { id } }");
        assert!(gql.body.graphql.variables.contains("\"x\""));

        let upload = c.items[2].as_request().unwrap().http().unwrap();
        assert_eq!(upload.body.mode, BodyMode::FormData);
        assert_eq!(upload.body.form_data[1].files, vec!["/tmp/a.png"]);
        assert_eq!(upload.auth.kind, AuthType::OAuth2);
        assert_eq!(upload.auth.get("grant_type"), "client_credentials");
        assert_eq!(upload.auth.get("client_authentication"), "body");

        let ws = c.items[3].as_request().unwrap();
        assert!(matches!(ws.request, RequestKind::WebSocket(_)));
        assert!(warnings.iter().any(|w| w.contains("unit_test_suite")), "{warnings:?}");
    }

    #[test]
    fn api_key_in_cookie_becomes_a_cookie_header() {
        let mut w = Vec::new();
        let mut t = Templates { warnings: &mut w };
        let a = serde_json::json!({"type": "apikey", "key": "session", "value": "{{ _.sid }}", "addTo": "cookie"});
        let auth = convert_auth(Some(&a), &mut t);
        assert_eq!(auth.kind, AuthType::ApiKey);
        assert_eq!(auth.get("key"), "Cookie");
        assert_eq!(auth.get("value"), "session={{sid}}");
        assert_eq!(auth.get("in"), "header");
    }

    #[test]
    fn v4_group_cycles_do_not_overflow_the_stack() {
        for text in [
            // A group reusing its workspace's id.
            r#"{"_type": "export", "__export_format": 4, "resources": [
                {"_id": "w", "_type": "workspace", "name": "W"},
                {"_id": "w", "_type": "request_group", "parentId": "w", "name": "G"}]}"#,
            // Groups without `_id` contain every resource without `parentId`.
            r#"{"_type": "export", "__export_format": 4, "resources": [
                {"_type": "request_group", "parentId": "p", "name": "G1"},
                {"_type": "request_group", "name": "G2"},
                {"_type": "request", "name": "R", "url": "http://h"}]}"#,
        ] {
            let (cols, _, _) = import(text).unwrap();
            assert_eq!(cols.len(), 1);
        }
    }

    #[test]
    fn imports_v5_yaml() {
        let (cols, envs, _) = import(include_str!("../testdata/insomnia_v5.yaml")).unwrap();
        assert_eq!(cols[0].name, "Demo v5");
        assert_eq!(cols[0].variables[0].key, "base_url");
        assert_eq!(envs[0].name, "Production");
        let folder = cols[0].items[0].as_folder().unwrap();
        assert_eq!(folder.name, "Things");
        let r = folder.items[0].as_request().unwrap();
        let h = r.http().unwrap();
        assert_eq!(h.method, "POST");
        assert_eq!(h.url, "{{base_url}}/things");
        assert_eq!(h.body.raw, "{\"n\": 1}");
        assert_eq!(r.scripts.pre_request, "pm.environment.set('a', 1);");
        assert_eq!(cols[0].items[1].name(), "Ping");
    }
}
