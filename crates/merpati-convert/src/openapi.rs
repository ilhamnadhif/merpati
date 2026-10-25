//! OpenAPI 3.0/3.1 and Swagger 2.0 import, OpenAPI 3.0 export (FR-C4, FR-D2).

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use merpati_core::model::{
    Body, BodyMode, Collection, Example, Folder, FormField, FormFieldKind, HttpRequest, Item, KeyValue, RawLanguage,
    RequestItem, RequestKind,
};
use merpati_core::variables::Variable;
use merpati_core::{Auth, AuthType};
use serde_json::{Map, Value, json};

use crate::ConvertError;
use crate::util::{as_string, pretty, status_text};

static NULL: Value = Value::Null;
const METHODS: [&str; 8] = ["get", "put", "post", "delete", "options", "head", "patch", "trace"];
const MAX_DEPTH: usize = 12;
/// Size budget of one generated example, in JSON nodes (long strings count
/// one node per 64 bytes). Schemas that reference each other can expand to
/// millions of nodes within [`MAX_DEPTH`] even though no reference is
/// expanded inside itself; past the budget, values become `null`.
const EXAMPLE_BUDGET: usize = 5_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Request,
    Response,
}

struct Importer<'a> {
    doc: &'a Value,
    v2: bool,
    warnings: Vec<String>,
    /// Placeholder variables referenced by generated auth.
    auth_vars: IndexMap<String, bool>,
    /// What is left of [`EXAMPLE_BUDGET`] for the example being generated.
    example_budget: usize,
    /// Whether the example being generated ran out of budget.
    example_cut: bool,
}

impl<'a> Importer<'a> {
    fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    /// Looks up a local `$ref` (`#/components/schemas/Pet`).
    fn pointer(&mut self, r: &str) -> Option<&'a Value> {
        let Some(frag) = r.strip_prefix('#') else {
            self.warn(format!("external reference `{r}` is not supported"));
            return None;
        };
        let decoded = percent_encoding::percent_decode_str(frag).decode_utf8_lossy().into_owned();
        let found = self.doc.pointer(&decoded);
        if found.is_none() {
            self.warn(format!("unresolved reference `{r}`"));
        }
        found
    }

    /// Follows `$ref` chains.
    fn resolve(&mut self, v: &'a Value) -> &'a Value {
        let mut cur = v;
        for _ in 0..32 {
            match cur.get("$ref").and_then(Value::as_str) {
                Some(r) => match self.pointer(r) {
                    Some(next) => cur = next,
                    None => return &NULL,
                },
                None => return cur,
            }
        }
        self.warn("reference chain too deep");
        &NULL
    }

    // -- schema examples ----------------------------------------------------

    fn example_from_schema(
        &mut self,
        schema: &'a Value,
        dir: Direction,
        depth: usize,
        seen: &mut Vec<String>,
    ) -> Value {
        if depth > MAX_DEPTH || !schema.is_object() {
            return Value::Null;
        }
        if self.example_budget == 0 {
            self.example_cut = true;
            return Value::Null;
        }
        self.example_budget -= 1;
        if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
            if seen.iter().any(|s| s == r) {
                return Value::Null;
            }
            seen.push(r.to_string());
            let target = self.resolve(schema);
            let v = self.example_from_schema(target, dir, depth + 1, seen);
            seen.pop();
            return v;
        }
        for key in ["example", "x-example", "default", "const"] {
            if let Some(v) = schema.get(key) {
                return self.literal(v);
            }
        }
        if let Some(Value::Array(ex)) = schema.get("examples")
            && let Some(first) = ex.first()
        {
            return self.literal(first);
        }
        if let Some(Value::Array(e)) = schema.get("enum")
            && let Some(first) = e.first()
        {
            return self.literal(first);
        }
        if let Some(Value::Array(all)) = schema.get("allOf") {
            let mut merged = Map::new();
            let mut last = Value::Null;
            for s in all {
                match self.example_from_schema(s, dir, depth + 1, seen) {
                    Value::Object(o) => merged.extend(o),
                    other if !other.is_null() => last = other,
                    _ => {}
                }
            }
            // Properties declared next to allOf also apply.
            if let Value::Object(extra) = self.object_example(schema, dir, depth, seen) {
                merged.extend(extra);
            }
            return if merged.is_empty() { last } else { Value::Object(merged) };
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(Value::Array(alts)) = schema.get(key)
                && let Some(first) = alts.iter().find(|s| s.get("type").and_then(Value::as_str) != Some("null"))
            {
                return self.example_from_schema(first, dir, depth + 1, seen);
            }
        }
        let ty = match schema.get("type") {
            Some(Value::String(t)) => t.clone(),
            Some(Value::Array(ts)) => {
                ts.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("null").to_string()
            }
            _ if schema.get("properties").is_some() || schema.get("additionalProperties").is_some() => "object".into(),
            _ if schema.get("items").is_some() => "array".into(),
            _ => String::new(),
        };
        match ty.as_str() {
            "object" => self.object_example(schema, dir, depth, seen),
            "array" => match schema.get("items") {
                Some(items) => {
                    let item = self.example_from_schema(items, dir, depth + 1, seen);
                    if item.is_null() { json!([]) } else { Value::Array(vec![item]) }
                }
                None => json!([]),
            },
            "integer" => json!(schema.get("minimum").and_then(Value::as_i64).unwrap_or(0)),
            "number" => json!(schema.get("minimum").and_then(Value::as_f64).unwrap_or(0.0)),
            "boolean" => json!(true),
            "null" => Value::Null,
            "string" => Value::String(string_example(schema.get("format").and_then(Value::as_str))),
            _ => Value::Null,
        }
    }

    fn object_example(&mut self, schema: &'a Value, dir: Direction, depth: usize, seen: &mut Vec<String>) -> Value {
        let mut out = Map::new();
        if let Some(Value::Object(props)) = schema.get("properties") {
            for (name, prop) in props {
                let resolved = self.resolve(prop);
                let skip = match dir {
                    Direction::Request => resolved.get("readOnly").and_then(Value::as_bool).unwrap_or(false),
                    Direction::Response => resolved.get("writeOnly").and_then(Value::as_bool).unwrap_or(false),
                };
                if skip {
                    continue;
                }
                out.insert(name.clone(), self.example_from_schema(prop, dir, depth + 1, seen));
            }
        }
        if out.is_empty()
            && let Some(extra @ Value::Object(_)) = schema.get("additionalProperties")
        {
            let v = self.example_from_schema(extra, dir, depth + 1, seen);
            out.insert("key".into(), v);
        }
        Value::Object(out)
    }

    /// A literal example of the spec, charged to the example budget.
    fn literal(&mut self, v: &Value) -> Value {
        fn cost(v: &Value, limit: usize) -> usize {
            match v {
                Value::String(s) => 1 + s.len() / 64,
                Value::Array(items) => items.iter().fold(1, |n, x| if n > limit { n } else { n + cost(x, limit - n) }),
                Value::Object(map) => map.values().fold(1, |n, x| if n > limit { n } else { n + cost(x, limit - n) }),
                _ => 1,
            }
        }
        let n = cost(v, self.example_budget);
        if n > self.example_budget {
            self.example_cut = true;
            return Value::Null;
        }
        self.example_budget -= n;
        v.clone()
    }

    fn schema_example(&mut self, schema: &'a Value, dir: Direction) -> Value {
        self.example_budget = EXAMPLE_BUDGET;
        self.example_cut = false;
        let v = self.example_from_schema(schema, dir, 0, &mut Vec::new());
        if self.example_cut {
            let name = schema.get("$ref").and_then(Value::as_str).unwrap_or("a schema");
            self.warn(format!(
                "the example generated from {name} was cut at {EXAMPLE_BUDGET} values (large or recursive schema)"
            ));
        }
        v
    }

    /// Example of a media type object (`example`, `examples`, or schema).
    fn media_examples(&mut self, media: &'a Value, dir: Direction) -> Vec<(Option<String>, Value)> {
        if let Some(ex) = media.get("example") {
            return vec![(None, ex.clone())];
        }
        if let Some(Value::Object(exs)) = media.get("examples") {
            let mut out = Vec::new();
            for (name, ex) in exs {
                let ex = self.resolve(ex);
                let label =
                    ex.get("summary").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| name.clone());
                if let Some(v) = ex.get("value") {
                    out.push((Some(label), v.clone()));
                } else if ex.get("externalValue").is_some() {
                    self.warn("externalValue examples are not fetched");
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
        match media.get("schema") {
            Some(s) => vec![(None, self.schema_example(s, dir))],
            None => vec![(None, Value::Null)],
        }
    }

    // -- auth ---------------------------------------------------------------

    fn security_schemes(&self) -> &'a Value {
        if self.v2 {
            self.doc.get("securityDefinitions").unwrap_or(&NULL)
        } else {
            self.doc.pointer("/components/securitySchemes").unwrap_or(&NULL)
        }
    }

    fn var(&mut self, name: &str, secret: bool) -> String {
        self.auth_vars.entry(name.to_string()).or_insert(secret);
        format!("{{{{{name}}}}}")
    }

    fn auth_for(&mut self, requirements: &'a Value) -> Option<Auth> {
        let Value::Array(reqs) = requirements else { return None };
        if reqs.is_empty() {
            return Some(Auth::none());
        }
        let first = reqs.iter().find_map(|r| r.as_object())?;
        if first.is_empty() {
            // `{}` means "no auth required" as an alternative.
            return Some(Auth::none());
        }
        if first.len() > 1 {
            self.warn("security requirements combining several schemes: only the first is imported");
        }
        let (name, scopes) = first.iter().next()?;
        let scheme_v = self.security_schemes().get(name)?;
        let scheme = self.resolve(scheme_v);
        let scopes: Vec<String> = scopes.as_array().map(|a| a.iter().map(as_string).collect()).unwrap_or_default();
        self.auth_from_scheme(name, scheme, &scopes)
    }

    fn auth_from_scheme(&mut self, name: &str, scheme: &'a Value, scopes: &[String]) -> Option<Auth> {
        let ty = scheme.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "basic" => Some(Auth::basic(self.var("username", false), self.var("password", true))),
            "http" => match scheme.get("scheme").and_then(Value::as_str).map(str::to_ascii_lowercase).as_deref() {
                Some("basic") => Some(Auth::basic(self.var("username", false), self.var("password", true))),
                Some("bearer") => Some(Auth::bearer(self.var("bearerToken", true))),
                Some("digest") => Some(
                    Auth::new(AuthType::Digest)
                        .with("username", self.var("username", false))
                        .with("password", self.var("password", true)),
                ),
                other => {
                    self.warn(format!("http auth scheme `{}` is not supported", other.unwrap_or("")));
                    None
                }
            },
            "apiKey" => {
                let key = scheme.get("name").and_then(Value::as_str).unwrap_or("X-API-Key").to_string();
                match scheme.get("in").and_then(Value::as_str) {
                    Some("query") => Some(Auth::api_key(key, self.var("apiKey", true), true)),
                    Some("header") => Some(Auth::api_key(key, self.var("apiKey", true), false)),
                    _ => {
                        self.warn(format!("API key `{name}` sent in a cookie cannot be imported as auth"));
                        None
                    }
                }
            }
            "oauth2" => Some(self.oauth2(scheme, scopes)),
            "openIdConnect" => {
                self.warn(format!(
                    "OpenID Connect scheme `{name}` imported as OAuth 2.0; fill in the auth and token URLs"
                ));
                let mut a = Auth::new(AuthType::OAuth2)
                    .with("grant_type", "authorization_code")
                    .with("clientId", self.var("clientId", false))
                    .with("clientSecret", self.var("clientSecret", true))
                    .with("addTokenTo", "header");
                if !scopes.is_empty() {
                    a.set("scope", scopes.join(" "));
                }
                Some(a)
            }
            other => {
                self.warn(format!("security scheme `{name}` of type `{other}` is not supported"));
                None
            }
        }
    }

    fn oauth2(&mut self, scheme: &'a Value, scopes: &[String]) -> Auth {
        let mut a = Auth::new(AuthType::OAuth2);
        let (grant, flow): (&str, &Value) = if self.v2 {
            let grant = match scheme.get("flow").and_then(Value::as_str) {
                Some("accessCode") => "authorization_code",
                Some("application") => "client_credentials",
                Some("password") => "password_credentials",
                _ => "implicit",
            };
            (grant, scheme)
        } else {
            let flows = scheme.get("flows").unwrap_or(&NULL);
            [
                ("authorizationCode", "authorization_code"),
                ("clientCredentials", "client_credentials"),
                ("password", "password_credentials"),
                ("implicit", "implicit"),
            ]
            .iter()
            .find_map(|(k, g)| flows.get(*k).map(|f| (*g, f)))
            .unwrap_or(("authorization_code", &NULL))
        };
        a.set("grant_type", grant);
        if let Some(u) = flow.get("authorizationUrl").and_then(Value::as_str) {
            a.set("authUrl", u);
        }
        if let Some(u) = flow.get("tokenUrl").and_then(Value::as_str) {
            a.set("accessTokenUrl", u);
        }
        if let Some(u) = flow.get("refreshUrl").and_then(Value::as_str) {
            a.set("refreshTokenUrl", u);
        }
        let scope = if scopes.is_empty() {
            flow.get("scopes").and_then(Value::as_object).map(|m| m.keys().cloned().collect::<Vec<_>>().join(" "))
        } else {
            Some(scopes.join(" "))
        };
        if let Some(s) = scope.filter(|s| !s.is_empty()) {
            a.set("scope", s);
        }
        a.set("clientId", self.var("clientId", false));
        if grant != "implicit" {
            a.set("clientSecret", self.var("clientSecret", true));
        }
        if grant == "password_credentials" {
            a.set("username", self.var("username", false));
            a.set("password", self.var("password", true));
        }
        a.set("addTokenTo", "header");
        a
    }

    // -- servers ------------------------------------------------------------

    fn base_url(&mut self) -> String {
        if self.v2 {
            let schemes: Vec<&str> = self
                .doc
                .get("schemes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let scheme = if schemes.contains(&"https") { "https" } else { schemes.first().copied().unwrap_or("https") };
            let host = match self.doc.get("host").and_then(Value::as_str) {
                Some(h) => h.to_string(),
                None => {
                    self.warn("no `host` defined; baseUrl set to http://localhost");
                    return format!(
                        "http://localhost{}",
                        self.doc.get("basePath").and_then(Value::as_str).unwrap_or("")
                    );
                }
            };
            let base = self.doc.get("basePath").and_then(Value::as_str).unwrap_or("");
            return format!("{scheme}://{host}{}", base.trim_end_matches('/'));
        }
        let servers = self.doc.get("servers").and_then(Value::as_array).cloned().unwrap_or_default();
        match servers.first() {
            Some(s) => {
                if servers.len() > 1 {
                    self.warn(format!("{} additional servers ignored; baseUrl uses the first", servers.len() - 1));
                }
                let url = server_url(s);
                if !url.contains("://") {
                    self.warn(format!("server URL `{url}` is relative; edit the baseUrl variable"));
                }
                url
            }
            None => {
                self.warn("no servers defined; baseUrl set to http://localhost");
                "http://localhost".into()
            }
        }
    }

    // -- operations ---------------------------------------------------------

    fn parameters(&mut self, path_item: &'a Value, op: &'a Value) -> Vec<&'a Value> {
        let mut out: Vec<&'a Value> = Vec::new();
        for src in [path_item.get("parameters"), op.get("parameters")] {
            let Some(Value::Array(list)) = src else { continue };
            for p in list {
                let p = self.resolve(p);
                let key = (p.get("name").cloned(), p.get("in").cloned());
                out.retain(|e| (e.get("name").cloned(), e.get("in").cloned()) != key);
                out.push(p);
            }
        }
        out
    }

    fn param_value(&mut self, p: &'a Value) -> Option<String> {
        let explicit = p
            .get("example")
            .or_else(|| p.get("x-example"))
            .cloned()
            .or_else(|| {
                p.get("examples")
                    .and_then(Value::as_object)
                    .and_then(|m| m.values().next())
                    .map(|e| self_resolve_static(self.doc, e).get("value").cloned().unwrap_or(Value::Null))
            })
            .or_else(|| p.get("default").cloned());
        let from_schema = || -> Option<Value> {
            let s = if self.is_v2_param(p) { Some(p) } else { p.get("schema") };
            let s = s?;
            let s = self_resolve_static(self.doc, s);
            s.get("example")
                .or_else(|| s.get("default"))
                .or_else(|| s.get("enum").and_then(|e| e.get(0)))
                .or_else(|| s.get("items").and_then(|i| i.get("enum")).and_then(|e| e.get(0)))
                .cloned()
        };
        let v = explicit.or_else(from_schema)?;
        Some(match v {
            Value::Array(items) => items.iter().map(as_string).collect::<Vec<_>>().join(","),
            other => as_string(&other),
        })
    }

    fn is_v2_param(&self, p: &Value) -> bool {
        self.v2 && p.get("schema").is_none()
    }

    fn import(&mut self) -> Result<Collection, ConvertError> {
        let info = self.doc.get("info").unwrap_or(&NULL);
        let mut c = Collection::new(info.get("title").and_then(Value::as_str).unwrap_or("Imported API"));
        c.description = info.get("description").and_then(Value::as_str).unwrap_or("").to_string();
        let base = self.base_url();
        c.variables.push(Variable::new("baseUrl", base));
        if let Some(auth) = self.doc.get("security").and_then(|s| self.auth_for(s)) {
            c.auth = auth;
        }

        // Folder per tag, in the order of `tags`, then order of first use.
        let mut tag_desc: HashMap<String, String> = HashMap::new();
        let mut folder_order: Vec<String> = Vec::new();
        if let Some(Value::Array(tags)) = self.doc.get("tags") {
            for t in tags {
                if let Some(n) = t.get("name").and_then(Value::as_str) {
                    folder_order.push(n.to_string());
                    if let Some(d) = t.get("description").and_then(Value::as_str) {
                        tag_desc.insert(n.to_string(), d.to_string());
                    }
                }
            }
        }
        let mut folders: IndexMap<String, Vec<Item>> = IndexMap::new();
        let mut root: Vec<Item> = Vec::new();

        let paths = match self.doc.get("paths") {
            Some(Value::Object(p)) => p,
            _ => {
                self.warn("document has no paths");
                return Ok(c);
            }
        };
        for (path, path_item) in paths {
            let path_item = self.resolve(path_item);
            for method in METHODS {
                let Some(op) = path_item.get(method) else { continue };
                let item = self.operation(path, method, path_item, op, &c.auth)?;
                let folder = op
                    .get("tags")
                    .and_then(Value::as_array)
                    .and_then(|t| t.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| first_static_segment(path));
                match folder {
                    Some(f) => {
                        if !folder_order.contains(&f) {
                            folder_order.push(f.clone());
                        }
                        folders.entry(f).or_default().push(item);
                    }
                    None => root.push(item),
                }
            }
        }
        for name in folder_order {
            if let Some(items) = folders.shift_remove(&name) {
                let mut f = Folder::new(name.clone());
                f.description = tag_desc.get(&name).cloned().unwrap_or_default();
                f.items = items;
                c.items.push(Item::Folder(f));
            }
        }
        c.items.extend(root);
        for (name, secret) in std::mem::take(&mut self.auth_vars) {
            let v = if secret { Variable::secret(name, "") } else { Variable::new(name, "") };
            c.variables.push(v);
        }
        Ok(c)
    }

    fn operation(
        &mut self,
        path: &str,
        method: &str,
        path_item: &'a Value,
        op: &'a Value,
        collection_auth: &Auth,
    ) -> Result<Item, ConvertError> {
        let params = self.parameters(path_item, op);
        let mut extra_vars: Vec<(String, String)> = Vec::new();

        // URL: `{id}` segments become `:id`; ids embedded in a segment become `{{id}}`.
        let mut url_path = String::new();
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            url_path.push('/');
            if seg.starts_with('{') && seg.ends_with('}') && seg.matches('{').count() == 1 {
                url_path.push(':');
                url_path.push_str(&seg[1..seg.len() - 1]);
            } else if seg.contains('{') {
                let mut rest = seg;
                while let Some(start) = rest.find('{') {
                    url_path.push_str(&rest[..start]);
                    match rest[start..].find('}') {
                        Some(end) => {
                            let name = &rest[start + 1..start + end];
                            url_path.push_str(&format!("{{{{{name}}}}}"));
                            extra_vars.push((name.to_string(), String::new()));
                            rest = &rest[start + end + 1..];
                        }
                        None => {
                            url_path.push_str(&rest[start..]);
                            rest = "";
                        }
                    }
                }
                url_path.push_str(rest);
            } else {
                url_path.push_str(seg);
            }
        }
        if url_path.is_empty() {
            url_path.push('/');
        }
        let base = match op
            .get("servers")
            .or_else(|| path_item.get("servers"))
            .and_then(Value::as_array)
            .and_then(|s| s.first())
        {
            Some(s) if !self.v2 => server_url(s).trim_end_matches('/').to_string(),
            _ => "{{baseUrl}}".to_string(),
        };
        let mut req = HttpRequest::new(method.to_uppercase(), format!("{base}{url_path}"));

        let mut query = Vec::new();
        let mut cookies = Vec::new();
        let mut form_params: Vec<&'a Value> = Vec::new();
        let mut body_param: Option<&'a Value> = None;
        for p in &params {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
            let description = p.get("description").and_then(Value::as_str).unwrap_or("").to_string();
            let location = p.get("in").and_then(Value::as_str).unwrap_or("");
            if location == "body" {
                body_param = Some(p);
                continue;
            }
            if location == "formData" {
                form_params.push(p);
                continue;
            }
            let mut value = self.param_value(p);
            if value.is_none() && required && location != "path" {
                // Required params get a typed placeholder so the request is sendable.
                let schema = if self.is_v2_param(p) { Some(*p) } else { p.get("schema") };
                if let Some(schema) = schema {
                    match self.schema_example(schema, Direction::Request) {
                        Value::Null | Value::Object(_) => {}
                        Value::Array(items) => value = Some(items.iter().map(as_string).collect::<Vec<_>>().join(",")),
                        other => value = Some(as_string(&other)),
                    }
                }
            }
            let enabled = required || value.is_some();
            let kv = KeyValue { key: name.clone(), value: value.unwrap_or_default(), enabled, description };
            match location {
                "path" => {
                    if let Some(pv) = req.path_vars.iter_mut().find(|v| v.key == name) {
                        pv.value = kv.value;
                        pv.description = kv.description;
                    } else if let Some(v) = extra_vars.iter_mut().find(|(k, _)| *k == name) {
                        v.1 = kv.value;
                    }
                }
                "query" => query.push(kv),
                "header" => {
                    if !["accept", "content-type", "authorization"].contains(&name.to_ascii_lowercase().as_str()) {
                        req.headers.push(kv);
                    }
                }
                "cookie" if kv.enabled => {
                    cookies.push(format!("{}={}", kv.key, kv.value));
                }
                _ => {}
            }
        }
        if !query.is_empty() {
            req.params = query;
            req.sync_url_from_params();
        }
        if !cookies.is_empty() {
            req.headers.push(KeyValue::new("Cookie", cookies.join("; ")));
        }

        // Request body.
        if self.v2 {
            let consumes = op
                .get("consumes")
                .or_else(|| self.doc.get("consumes"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>())
                .unwrap_or_default();
            if let Some(bp) = body_param {
                let ct = consumes
                    .iter()
                    .find(|c| c.contains("json"))
                    .cloned()
                    .or_else(|| consumes.first().cloned())
                    .unwrap_or_else(|| "application/json".into());
                let schema = bp.get("schema").unwrap_or(&NULL);
                let ex =
                    bp.get("x-example").cloned().unwrap_or_else(|| self.schema_example(schema, Direction::Request));
                self.set_body_from_value(&mut req, &ct, &ex, schema);
            } else if !form_params.is_empty() {
                let multipart = consumes.iter().any(|c| c.contains("multipart"))
                    || form_params.iter().any(|p| p.get("type").and_then(Value::as_str) == Some("file"));
                for p in &form_params {
                    let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let is_file = p.get("type").and_then(Value::as_str) == Some("file");
                    let value = self.param_value(p).unwrap_or_default();
                    let desc = p.get("description").and_then(Value::as_str).unwrap_or("").to_string();
                    if multipart {
                        let mut f = if is_file { FormField::file(name, "") } else { FormField::text(name, value) };
                        if is_file {
                            f.files.clear();
                        }
                        f.description = desc;
                        req.body.form_data.push(f);
                    } else {
                        req.body.urlencoded.push(KeyValue { key: name, value, enabled: true, description: desc });
                    }
                }
                req.body.mode = if multipart { BodyMode::FormData } else { BodyMode::UrlEncoded };
                if !multipart {
                    req.headers.push(KeyValue::new("Content-Type", "application/x-www-form-urlencoded"));
                }
            }
        } else if let Some(rb) = op.get("requestBody") {
            let rb = self.resolve(rb);
            if let Some(Value::Object(content)) = rb.get("content")
                && let Some((ct, media)) = pick_media(content)
            {
                let schema = media.get("schema").unwrap_or(&NULL);
                let ex = self
                    .media_examples(media, Direction::Request)
                    .into_iter()
                    .next()
                    .map(|(_, v)| v)
                    .unwrap_or(Value::Null);
                self.set_body_from_value(&mut req, ct, &ex, schema);
            }
        }

        // Accept header from the first success response's media type.
        let responses = op.get("responses").and_then(Value::as_object);
        let produces: Vec<String> = if self.v2 {
            op.get("produces")
                .or_else(|| self.doc.get("produces"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let accept = if self.v2 {
            produces.iter().find(|c| c.contains("json")).cloned().or_else(|| produces.first().cloned())
        } else {
            responses.and_then(|rs| {
                rs.iter().filter(|(code, _)| code.starts_with('2')).find_map(|(_, r)| {
                    let r = self_resolve_static(self.doc, r);
                    r.get("content")
                        .and_then(Value::as_object)
                        .and_then(|c| pick_media(c))
                        .map(|(ct, _)| ct.to_string())
                })
            })
        };
        if let Some(a) = accept {
            req.headers.push(KeyValue::new("Accept", a));
        }

        // Auth.
        if let Some(sec) = op.get("security")
            && let Some(a) = self.auth_for(sec)
            && &a != collection_auth
        {
            req.auth = a;
        }

        let name = op
            .get("summary")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| op.get("operationId").and_then(Value::as_str))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| format!("{} {path}", method.to_uppercase()));
        let mut description = op.get("description").and_then(Value::as_str).unwrap_or("").to_string();
        if op.get("deprecated").and_then(Value::as_bool).unwrap_or(false) {
            description =
                if description.is_empty() { "Deprecated.".into() } else { format!("Deprecated. {description}") };
        }
        for (k, _) in extra_vars {
            self.warn(format!("path parameter `{k}` inside a segment became the variable {{{{{k}}}}}"));
        }

        let mut item = RequestItem::new(name, req.clone());
        item.description = description;
        if let Some(rs) = responses {
            for (code, resp) in rs {
                let resp = self.resolve(resp);
                item.examples.extend(self.examples_for(code, resp, &req, &produces));
            }
        }
        Ok(Item::Request(item))
    }

    fn set_body_from_value(&mut self, req: &mut HttpRequest, ct: &str, ex: &Value, schema: &'a Value) {
        let lower = ct.to_ascii_lowercase();
        if lower.contains("json") || lower == "*/*" {
            req.body = Body::raw_text(if ex.is_null() { String::new() } else { pretty(ex) }, RawLanguage::Json);
            req.headers.push(KeyValue::new("Content-Type", if lower == "*/*" { "application/json" } else { ct }));
        } else if lower.contains("x-www-form-urlencoded") || lower.contains("multipart/form-data") {
            let multipart = lower.contains("multipart");
            let schema = self.resolve(schema);
            let mut names: Vec<(String, Value, bool, String)> = Vec::new();
            let props = schema.get("properties").and_then(Value::as_object);
            if let Some(props) = props {
                for (n, prop) in props {
                    let prop = self.resolve(prop);
                    let is_file = is_binary_schema(prop)
                        || prop
                            .get("items")
                            .map(|i| is_binary_schema(self_resolve_static(self.doc, i)))
                            .unwrap_or(false);
                    let desc = prop.get("description").and_then(Value::as_str).unwrap_or("").to_string();
                    let v = ex.get(n).cloned().unwrap_or(Value::Null);
                    names.push((n.clone(), v, is_file, desc));
                }
            } else if let Value::Object(o) = ex {
                for (n, v) in o {
                    names.push((n.clone(), v.clone(), false, String::new()));
                }
            }
            for (n, v, is_file, desc) in names {
                let value = match &v {
                    Value::Object(_) | Value::Array(_) => v.to_string(),
                    other => as_string(other),
                };
                if multipart {
                    let mut f = if is_file {
                        FormField { key: n, kind: FormFieldKind::File, enabled: true, ..Default::default() }
                    } else {
                        FormField::text(n, value)
                    };
                    f.description = desc;
                    req.body.form_data.push(f);
                } else {
                    req.body.urlencoded.push(KeyValue { key: n, value, enabled: true, description: desc });
                }
            }
            req.body.mode = if multipart { BodyMode::FormData } else { BodyMode::UrlEncoded };
            if !multipart {
                req.headers.push(KeyValue::new("Content-Type", ct));
            }
        } else if lower.contains("xml") {
            let text = match ex {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => {
                    let root = schema_xml_name(self.doc, schema).unwrap_or_else(|| "root".into());
                    json_to_xml(&root, other, 0)
                }
            };
            req.body = Body::raw_text(text, RawLanguage::Xml);
            req.headers.push(KeyValue::new("Content-Type", ct));
        } else if lower.starts_with("text/") {
            let text = match ex {
                Value::Null => String::new(),
                other => as_string(other),
            };
            req.body = Body::raw_text(text, crate::util::raw_language_for(&lower));
            req.headers.push(KeyValue::new("Content-Type", ct));
        } else {
            req.body.mode = BodyMode::Binary;
            req.headers.push(KeyValue::new("Content-Type", ct));
        }
    }

    fn examples_for(&mut self, code: &str, resp: &'a Value, req: &HttpRequest, produces: &[String]) -> Vec<Example> {
        let status = match code {
            "default" => 500,
            c => c
                .parse::<u16>()
                .ok()
                .or_else(|| {
                    // `2XX` style ranges.
                    c.chars().next().and_then(|d| d.to_digit(10)).map(|d| d as u16 * 100)
                })
                .unwrap_or(200),
        };
        let desc = resp.get("description").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let base_name =
            if desc.is_empty() { format!("{status} {}", status_text(status)).trim().to_string() } else { desc };
        let mut original = req.clone();
        original.auth = Auth::default();
        original.settings = Default::default();
        let mk = |name: String, ct: Option<&str>, body: String| Example {
            id: merpati_core::util::new_id(),
            name,
            request: Some(original.clone()),
            status,
            status_text: status_text(status).to_string(),
            headers: ct.map(|c| vec![KeyValue::new("Content-Type", c)]).unwrap_or_default(),
            body,
            language: ct.and_then(crate::util::language_for_content_type),
        };
        let render = |doc: &Value, ct: &str, v: &Value, schema: &Value| -> String {
            match v {
                Value::Null => String::new(),
                Value::String(s) if !ct.contains("json") => s.clone(),
                other if ct.contains("xml") => {
                    json_to_xml(&schema_xml_name(doc, schema).unwrap_or_else(|| "root".into()), other, 0)
                }
                other => pretty(other),
            }
        };
        let mut out = Vec::new();
        if self.v2 {
            if let Some(Value::Object(exs)) = resp.get("examples") {
                for (ct, v) in exs {
                    out.push(mk(base_name.clone(), Some(ct), render(self.doc, ct, v, &NULL)));
                }
                return out;
            }
            let ct = produces
                .iter()
                .find(|c| c.contains("json"))
                .or_else(|| produces.first())
                .map(String::as_str)
                .unwrap_or("application/json");
            match resp.get("schema") {
                Some(schema) => {
                    let v = self.schema_example(schema, Direction::Response);
                    out.push(mk(base_name, Some(ct), render(self.doc, ct, &v, schema)));
                }
                None => out.push(mk(base_name, None, String::new())),
            }
            return out;
        }
        match resp.get("content").and_then(Value::as_object).and_then(|c| pick_media(c)) {
            Some((ct, media)) => {
                let schema = media.get("schema").unwrap_or(&NULL);
                let examples = self.media_examples(media, Direction::Response);
                let several = examples.len() > 1;
                for (label, v) in examples {
                    let name = match (&label, several) {
                        (Some(l), true) => format!("{base_name} ({l})"),
                        _ => base_name.clone(),
                    };
                    out.push(mk(name, Some(ct), render(self.doc, ct, &v, schema)));
                }
            }
            None => out.push(mk(base_name, None, String::new())),
        }
        out
    }
}

/// `$ref` resolution without warnings, for read-only helpers.
fn self_resolve_static<'a>(doc: &'a Value, v: &'a Value) -> &'a Value {
    let mut cur = v;
    for _ in 0..32 {
        match cur.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix('#')) {
            Some(p) => match doc.pointer(&percent_encoding::percent_decode_str(p).decode_utf8_lossy()) {
                Some(next) => cur = next,
                None => return &NULL,
            },
            None => return cur,
        }
    }
    &NULL
}

fn is_binary_schema(s: &Value) -> bool {
    matches!(s.get("format").and_then(Value::as_str), Some("binary") | Some("base64"))
        || s.get("type").and_then(Value::as_str) == Some("file")
        || s.get("contentMediaType").is_some()
}

fn schema_xml_name(doc: &Value, schema: &Value) -> Option<String> {
    if let Some(n) = schema.pointer("/xml/name").and_then(Value::as_str) {
        return Some(n.to_string());
    }
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        let target = self_resolve_static(doc, schema);
        if let Some(n) = target.pointer("/xml/name").and_then(Value::as_str) {
            return Some(n.to_string());
        }
        return r.rsplit('/').next().map(str::to_string);
    }
    None
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Renders an example value as indented XML.
fn json_to_xml(name: &str, v: &Value, depth: usize) -> String {
    let pad = "  ".repeat(depth);
    match v {
        Value::Object(o) => {
            let mut s = format!("{pad}<{name}>\n");
            for (k, child) in o {
                s.push_str(&json_to_xml(k, child, depth + 1));
            }
            s.push_str(&format!("{pad}</{name}>\n"));
            if depth == 0 { s.trim_end().to_string() } else { s }
        }
        Value::Array(items) => {
            let mut s = String::new();
            for it in items {
                s.push_str(&json_to_xml(name, it, depth));
                if depth == 0 {
                    s.push('\n');
                }
            }
            if depth == 0 { s.trim_end().to_string() } else { s }
        }
        Value::Null => format!("{pad}<{name}/>\n"),
        other => {
            let s = format!("{pad}<{name}>{}</{name}>\n", xml_escape(&as_string(other)));
            if depth == 0 { s.trim_end().to_string() } else { s }
        }
    }
}

fn string_example(format: Option<&str>) -> String {
    match format {
        Some("date") => "2024-01-01",
        Some("date-time") => "2024-01-01T00:00:00Z",
        Some("time") => "00:00:00",
        Some("email") => "user@example.com",
        Some("uuid") => "3fa85f64-5717-4562-b3fc-2c963f66afa6",
        Some("uri") | Some("url") => "https://example.com",
        Some("hostname") => "example.com",
        Some("ipv4") => "192.168.0.1",
        Some("ipv6") => "::1",
        Some("byte") => "c3RyaW5n",
        Some("binary") => "",
        Some("password") => "********",
        _ => "string",
    }
    .to_string()
}

fn server_url(s: &Value) -> String {
    let mut url = s.get("url").and_then(Value::as_str).unwrap_or("/").to_string();
    if let Some(Value::Object(vars)) = s.get("variables") {
        for (k, v) in vars {
            let def = v.get("default").map(as_string).unwrap_or_default();
            url = url.replace(&format!("{{{k}}}"), &def);
        }
    }
    url.trim_end_matches('/').to_string()
}

fn first_static_segment(path: &str) -> Option<String> {
    path.split('/').find(|s| !s.is_empty() && !s.starts_with('{')).map(str::to_string)
}

/// Picks the most useful media type: JSON, forms, XML, text, then anything.
fn pick_media(content: &Map<String, Value>) -> Option<(&str, &Value)> {
    let rank = |ct: &str| -> u8 {
        let c = ct.to_ascii_lowercase();
        if c == "application/json" {
            0
        } else if c.contains("json") {
            1
        } else if c.contains("x-www-form-urlencoded") {
            2
        } else if c.contains("multipart/form-data") {
            3
        } else if c.contains("xml") {
            4
        } else if c.starts_with("text/") {
            5
        } else if c == "*/*" {
            6
        } else {
            7
        }
    };
    content.iter().min_by_key(|(ct, _)| rank(ct)).map(|(k, v)| (k.as_str(), v))
}

/// Imports OpenAPI 3.0/3.1 or Swagger 2.0 (JSON or YAML).
pub fn import(text: &str) -> Result<(Collection, Vec<String>), ConvertError> {
    let doc = crate::parse_structured(text)?;
    let v2 = match (doc.get("swagger").and_then(Value::as_str), doc.get("openapi").and_then(Value::as_str)) {
        (Some(s), _) if s.starts_with('2') => true,
        (_, Some(o)) if o.starts_with('3') => false,
        (Some(s), _) => return Err(ConvertError::Format(format!("unsupported Swagger version {s}"))),
        (_, Some(o)) => return Err(ConvertError::Format(format!("unsupported OpenAPI version {o}"))),
        _ => return Err(ConvertError::Format("not an OpenAPI or Swagger document".into())),
    };
    let mut imp = Importer {
        doc: &doc,
        v2,
        warnings: Vec::new(),
        auth_vars: IndexMap::new(),
        example_budget: EXAMPLE_BUDGET,
        example_cut: false,
    };
    let c = imp.import()?;
    Ok((c, imp.warnings))
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

fn infer_schema(v: &Value) -> Value {
    match v {
        Value::Null => json!({"nullable": true}),
        Value::Bool(_) => json!({"type": "boolean"}),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({"type": "integer"}),
        Value::Number(_) => json!({"type": "number"}),
        Value::String(_) => json!({"type": "string"}),
        Value::Array(items) => match items.first() {
            Some(first) => json!({"type": "array", "items": infer_schema(first)}),
            None => json!({"type": "array", "items": {}}),
        },
        Value::Object(o) => {
            let props: Map<String, Value> = o.iter().map(|(k, v)| (k.clone(), infer_schema(v))).collect();
            json!({"type": "object", "properties": props})
        }
    }
}

/// Path template of a raw URL: host/scheme/`{{baseUrl}}` removed, `:id` and
/// `{{id}}` path parts turned into `{id}`. Returns the path params found.
fn url_to_path(raw: &str) -> (String, Vec<String>) {
    let (base, _, _) = merpati_core::urlutil::split(raw);
    let mut rest = base.trim();
    if let Some(i) = rest.find("://") {
        let after = &rest[i + 3..];
        rest = after.find('/').map(|j| &after[j..]).unwrap_or("");
    } else if rest.starts_with("{{") {
        if let Some(end) = rest.find("}}") {
            rest = &rest[end + 2..];
        }
    } else if !rest.starts_with('/') {
        rest = rest.find('/').map(|j| &rest[j..]).unwrap_or("");
    }
    let mut params = Vec::new();
    let mut out = String::new();
    for seg in rest.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        if let Some(name) = seg.strip_prefix(':') {
            out.push_str(&format!("{{{name}}}"));
            params.push(name.to_string());
        } else if seg.contains("{{") {
            let mut s = seg.to_string();
            for name in merpati_core::variables::references(seg) {
                s = s.replace(&format!("{{{{{name}}}}}"), &format!("{{{name}}}"));
                params.push(name.to_string());
            }
            out.push_str(&s);
        } else {
            out.push_str(seg);
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    (out, params)
}

fn operation_id(name: &str, used: &mut HashSet<String>) -> String {
    let mut id = String::new();
    let mut upper = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if id.is_empty() {
                id.extend(ch.to_lowercase());
            } else if upper {
                id.extend(ch.to_uppercase());
            } else {
                id.push(ch);
            }
            upper = false;
        } else {
            upper = true;
        }
    }
    if id.is_empty() || id.starts_with(|c: char| c.is_ascii_digit()) {
        id = format!("op{id}");
    }
    let mut candidate = id.clone();
    let mut n = 2;
    while !used.insert(candidate.clone()) {
        candidate = format!("{id}{n}");
        n += 1;
    }
    candidate
}

struct Exporter {
    schemes: Map<String, Value>,
    /// Literal values of secret-type collection and folder variables.
    secret_values: Vec<String>,
}

impl Exporter {
    /// Whether a parameter value may be written as an `example`. Like the
    /// generated docs, credentials never are: values of credential-like names
    /// (`Authorization`, `X-Api-Key`, `token`, ...), values that look like
    /// one (`Bearer ...`, a JWT), values holding a secret variable's value,
    /// and `{{variable}}` references (unresolved here).
    fn shows_example(&self, name: &str, value: &str) -> bool {
        !value.is_empty()
            && !value.contains("{{")
            && !crate::docs::is_credential_name(name)
            && !looks_like_credential(value)
            && !self.secret_values.iter().any(|s| value.contains(s.as_str()))
    }

    /// Registers a security scheme for an auth and returns its name.
    fn scheme_for(&mut self, auth: &Auth) -> Option<String> {
        let (name, scheme) = match auth.kind {
            AuthType::Basic => ("basicAuth".to_string(), json!({"type": "http", "scheme": "basic"})),
            AuthType::Bearer => ("bearerAuth".to_string(), json!({"type": "http", "scheme": "bearer"})),
            AuthType::Jwt => {
                ("jwtAuth".to_string(), json!({"type": "http", "scheme": "bearer", "bearerFormat": "JWT"}))
            }
            AuthType::Digest => ("digestAuth".to_string(), json!({"type": "http", "scheme": "digest"})),
            AuthType::ApiKey => {
                let key = auth.get("key");
                let location = if auth.get("in") == "query" { "query" } else { "header" };
                let suffix: String =
                    key.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
                let name = if suffix.is_empty() { "apiKey".to_string() } else { format!("apiKey_{suffix}") };
                (name, json!({"type": "apiKey", "name": key, "in": location}))
            }
            AuthType::OAuth2 => {
                let grant = auth.get("grant_type");
                let (flow_name, needs_auth, needs_token) = match grant {
                    "client_credentials" => ("clientCredentials", false, true),
                    "password_credentials" => ("password", false, true),
                    "implicit" => ("implicit", true, false),
                    _ => ("authorizationCode", true, true),
                };
                let mut flow = Map::new();
                if needs_auth {
                    flow.insert("authorizationUrl".into(), json!(auth.get("authUrl")));
                }
                if needs_token {
                    flow.insert("tokenUrl".into(), json!(auth.get("accessTokenUrl")));
                }
                if !auth.get("refreshTokenUrl").is_empty() {
                    flow.insert("refreshUrl".into(), json!(auth.get("refreshTokenUrl")));
                }
                let scopes: Map<String, Value> =
                    auth.get("scope").split_whitespace().map(|s| (s.to_string(), json!(""))).collect();
                flow.insert("scopes".into(), Value::Object(scopes));
                ("oauth2".to_string(), json!({"type": "oauth2", "flows": {flow_name: flow}}))
            }
            _ => return None,
        };
        self.schemes.entry(name.clone()).or_insert(scheme);
        Some(name)
    }

    fn security(&mut self, auth: &Auth) -> Option<Value> {
        match auth.kind {
            AuthType::Inherit => None,
            AuthType::None => Some(json!([])),
            _ => {
                let name = self.scheme_for(auth)?;
                let scopes: Vec<Value> = if auth.kind == AuthType::OAuth2 {
                    auth.get("scope").split_whitespace().map(|s| json!(s)).collect()
                } else {
                    Vec::new()
                };
                Some(json!([{ name: scopes }]))
            }
        }
    }
}

/// An `Authorization`-style value (`Bearer abc`, `Basic ...`) or a JWT.
fn looks_like_credential(value: &str) -> bool {
    let t = value.trim();
    let is_jwt = |s: &str| {
        s.starts_with("eyJ")
            && s.split('.').count() == 3
            && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '='))
    };
    match t.split_once(char::is_whitespace) {
        Some((scheme, rest)) => {
            !rest.trim().is_empty()
                && ["basic", "bearer", "digest", "token", "negotiate", "ntlm", "hoba", "mutual", "aws4-hmac-sha256"]
                    .contains(&scheme.to_ascii_lowercase().as_str())
        }
        None => is_jwt(t),
    }
}

/// A form field's example value: credential-like fields are masked as in the
/// generated docs.
fn field_example(key: &str, value: &str) -> Value {
    if crate::docs::is_credential_name(key) { json!(crate::docs::mask_value(value)) } else { json!(value) }
}

fn body_to_request_body(body: &Body, headers: &[KeyValue]) -> Option<Value> {
    let header_ct = headers
        .iter()
        .rev()
        .find(|h| h.enabled && h.key.eq_ignore_ascii_case("content-type"))
        .map(|h| h.value.split(';').next().unwrap_or("").trim().to_string());
    let (ct, media) = match body.mode {
        BodyMode::None => return None,
        BodyMode::Raw => {
            let ct = header_ct.unwrap_or_else(|| body.raw_language.content_type().to_string());
            let media = if ct.contains("json") {
                match serde_json::from_str::<Value>(&body.raw) {
                    Ok(mut v) => {
                        crate::docs::mask_credentials_json(&mut v);
                        json!({"schema": infer_schema(&v), "example": v})
                    }
                    Err(_) => json!({"schema": {"type": "string"}, "example": body.raw}),
                }
            } else {
                json!({"schema": {"type": "string"}, "example": body.raw})
            };
            (ct, media)
        }
        BodyMode::UrlEncoded | BodyMode::FormData => {
            let multipart = body.mode == BodyMode::FormData;
            let mut props = Map::new();
            let mut example = Map::new();
            if multipart {
                for f in body.form_data.iter().filter(|f| !f.key.is_empty()) {
                    match f.kind {
                        FormFieldKind::File => {
                            props.insert(f.key.clone(), json!({"type": "string", "format": "binary"}));
                        }
                        FormFieldKind::Text => {
                            let mut s = json!({"type": "string"});
                            if !f.description.is_empty() {
                                s["description"] = json!(f.description);
                            }
                            props.insert(f.key.clone(), s);
                            example.insert(f.key.clone(), field_example(&f.key, &f.value));
                        }
                    }
                }
            } else {
                for f in body.urlencoded.iter().filter(|f| !f.key.is_empty()) {
                    let mut s = json!({"type": "string"});
                    if !f.description.is_empty() {
                        s["description"] = json!(f.description);
                    }
                    props.insert(f.key.clone(), s);
                    example.insert(f.key.clone(), field_example(&f.key, &f.value));
                }
            }
            let ct = if multipart { "multipart/form-data" } else { "application/x-www-form-urlencoded" };
            let mut media = json!({"schema": {"type": "object", "properties": props}});
            if !example.is_empty() {
                media["example"] = Value::Object(example);
            }
            (ct.to_string(), media)
        }
        BodyMode::Binary => (
            header_ct.unwrap_or_else(|| "application/octet-stream".into()),
            json!({"schema": {"type": "string", "format": "binary"}}),
        ),
        BodyMode::GraphQl => {
            let vars = serde_json::from_str::<Value>(&body.graphql.variables).unwrap_or(json!({}));
            (
                "application/json".into(),
                json!({
                    "schema": {"type": "object", "properties": {"query": {"type": "string"}, "variables": {"type": "object"}}},
                    "example": {"query": body.graphql.query, "variables": vars}
                }),
            )
        }
    };
    Some(json!({"content": {ct: media}}))
}

fn example_value(e: &Example) -> (String, Value) {
    let ct = e
        .headers
        .iter()
        .find(|h| h.key.eq_ignore_ascii_case("content-type"))
        .map(|h| h.value.split(';').next().unwrap_or("").trim().to_string())
        .unwrap_or_else(|| match e.language.as_deref() {
            Some("json") => "application/json".into(),
            Some("xml") => "application/xml".into(),
            Some("html") => "text/html".into(),
            _ => {
                if serde_json::from_str::<Value>(&e.body).is_ok() {
                    "application/json".into()
                } else {
                    "text/plain".into()
                }
            }
        });
    let v = if ct.contains("json") {
        match serde_json::from_str::<Value>(&e.body) {
            Ok(mut v) => {
                crate::docs::mask_credentials_json(&mut v);
                v
            }
            Err(_) => json!(e.body),
        }
    } else {
        json!(e.body)
    };
    (ct, v)
}

fn responses_for(examples: &[Example]) -> Value {
    if examples.is_empty() {
        return json!({"200": {"description": "Successful response"}});
    }
    let mut by_code: IndexMap<u16, Vec<&Example>> = IndexMap::new();
    for e in examples {
        by_code.entry(e.status).or_default().push(e);
    }
    let mut out = Map::new();
    for (code, exs) in by_code {
        let first = exs[0];
        let description = if first.name.is_empty() { status_text(code).to_string() } else { first.name.clone() };
        let mut content: IndexMap<String, Vec<(String, Value)>> = IndexMap::new();
        for e in &exs {
            if e.body.is_empty() {
                continue;
            }
            let (ct, v) = example_value(e);
            content.entry(ct).or_default().push((e.name.clone(), v));
        }
        let mut resp = json!({"description": description});
        if !content.is_empty() {
            let mut cmap = Map::new();
            for (ct, values) in content {
                let schema = infer_schema(&values[0].1);
                let media = if values.len() == 1 {
                    json!({"schema": schema, "example": values[0].1})
                } else {
                    let mut ex = Map::new();
                    let mut used = HashSet::new();
                    for (name, v) in values {
                        let key = operation_id(&name, &mut used);
                        ex.insert(key, json!({"summary": name, "value": v}));
                    }
                    json!({"schema": schema, "examples": ex})
                };
                cmap.insert(ct, media);
            }
            resp["content"] = Value::Object(cmap);
        }
        out.insert(code.to_string(), resp);
    }
    Value::Object(out)
}

fn guess_server(c: &Collection) -> Option<String> {
    if let Some(v) =
        c.variables.iter().find(|v| v.key.eq_ignore_ascii_case("baseUrl") || v.key.eq_ignore_ascii_case("base_url"))
        && !v.value.is_empty()
    {
        return Some(v.value.clone());
    }
    for r in c.requests() {
        if let RequestKind::Http(h) = &r.request
            && let Ok(u) = url::Url::parse(&h.url)
            && let Some(host) = u.host_str()
        {
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            return Some(format!("{}://{host}{port}", u.scheme()));
        }
    }
    None
}

/// Builds an OpenAPI 3.0.3 document from a collection. Requests become
/// operations (top-level folders become tags), examples become responses.
pub fn export(c: &Collection) -> Value {
    fn secret_values(items: &[Item], out: &mut Vec<String>) {
        for item in items {
            if let Item::Folder(f) = item {
                out.extend(
                    f.variables.iter().filter(|v| v.is_secret() && !v.value.is_empty()).map(|v| v.value.clone()),
                );
                secret_values(&f.items, out);
            }
        }
    }
    let mut secrets: Vec<String> =
        c.variables.iter().filter(|v| v.is_secret() && !v.value.is_empty()).map(|v| v.value.clone()).collect();
    secret_values(&c.items, &mut secrets);
    let mut ex = Exporter { schemes: Map::new(), secret_values: secrets };
    let mut paths: Map<String, Value> = Map::new();
    let mut used_ids = HashSet::new();
    let mut tags: Vec<Value> = Vec::new();

    fn walk<'c>(items: &'c [Item], tag: Option<&'c Folder>, out: &mut Vec<(&'c RequestItem, Option<&'c Folder>)>) {
        for item in items {
            match item {
                Item::Request(r) => out.push((r, tag)),
                Item::Folder(f) => walk(&f.items, Some(tag.unwrap_or(f)), out),
            }
        }
    }
    let mut reqs = Vec::new();
    walk(&c.items, None, &mut reqs);

    for (r, folder) in reqs {
        let RequestKind::Http(h) = &r.request else { continue };
        let method = h.method.to_ascii_lowercase();
        if !METHODS.contains(&method.as_str()) {
            continue;
        }
        let (path, path_params) = url_to_path(&h.url);
        let entry = paths.entry(path).or_insert_with(|| json!({}));
        if entry.get(&method).is_some() {
            continue;
        }
        let mut op = Map::new();
        op.insert("summary".into(), json!(r.name));
        if !r.description.is_empty() {
            op.insert("description".into(), json!(r.description));
        }
        op.insert("operationId".into(), json!(operation_id(&r.name, &mut used_ids)));
        if let Some(f) = folder {
            op.insert("tags".into(), json!([f.name]));
            if !tags.iter().any(|t| t["name"] == json!(f.name)) {
                let mut t = json!({"name": f.name});
                if !f.description.is_empty() {
                    t["description"] = json!(f.description);
                }
                tags.push(t);
            }
        }
        let mut params = Vec::new();
        for name in &path_params {
            let mut p = json!({"name": name, "in": "path", "required": true, "schema": {"type": "string"}});
            if let Some(v) = h.path_vars.iter().find(|v| &v.key == name) {
                if ex.shows_example(name, &v.value) {
                    p["example"] = json!(v.value);
                }
                if !v.description.is_empty() {
                    p["description"] = json!(v.description);
                }
            }
            params.push(p);
        }
        for q in h.params.iter().filter(|q| !q.key.is_empty()) {
            let mut p = json!({"name": q.key, "in": "query", "schema": {"type": "string"}});
            if ex.shows_example(&q.key, &q.value) {
                p["example"] = json!(q.value);
            }
            if !q.description.is_empty() {
                p["description"] = json!(q.description);
            }
            params.push(p);
        }
        for hd in h.headers.iter().filter(|x| x.enabled && !x.key.is_empty()) {
            if ["content-type", "accept", "authorization", "cookie"].contains(&hd.key.to_ascii_lowercase().as_str()) {
                continue;
            }
            let mut p = json!({"name": hd.key, "in": "header", "schema": {"type": "string"}});
            if ex.shows_example(&hd.key, &hd.value) {
                p["example"] = json!(hd.value);
            }
            params.push(p);
        }
        if !params.is_empty() {
            op.insert("parameters".into(), Value::Array(params));
        }
        if let Some(rb) = body_to_request_body(&h.body, &h.headers) {
            op.insert("requestBody".into(), rb);
        }
        op.insert("responses".into(), responses_for(&r.examples));
        if let Some(sec) = ex.security(&h.auth) {
            op.insert("security".into(), sec);
        }
        entry[&method] = Value::Object(op);
    }

    let mut doc = Map::new();
    doc.insert("openapi".into(), json!("3.0.3"));
    let mut info = Map::new();
    info.insert("title".into(), json!(c.name));
    if !c.description.is_empty() {
        info.insert("description".into(), json!(c.description));
    }
    info.insert("version".into(), json!("1.0.0"));
    doc.insert("info".into(), Value::Object(info));
    if let Some(s) = guess_server(c) {
        doc.insert("servers".into(), json!([{"url": s}]));
    }
    if !tags.is_empty() {
        doc.insert("tags".into(), Value::Array(tags));
    }
    if let Some(sec) = ex.security(&c.auth) {
        doc.insert("security".into(), sec);
    }
    doc.insert("paths".into(), Value::Object(paths));
    if !ex.schemes.is_empty() {
        doc.insert("components".into(), json!({"securitySchemes": ex.schemes}));
    }
    Value::Object(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PETSTORE_V3: &str = include_str!("../testdata/petstore.openapi.yaml");
    const PETSTORE_V2: &str = include_str!("../testdata/petstore.swagger.json");

    fn find<'c>(c: &'c Collection, name: &str) -> &'c RequestItem {
        c.requests().into_iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no request {name}"))
    }

    #[test]
    fn imports_openapi3_yaml() {
        let (c, warnings) = import(PETSTORE_V3).unwrap();
        assert_eq!(c.name, "Swagger Petstore");
        assert_eq!(c.variables[0].key, "baseUrl");
        assert_eq!(c.variables[0].value, "https://petstore.example.com/v1");
        // Folders follow `tags` order.
        let folders: Vec<&str> = c.items.iter().filter_map(|i| i.as_folder()).map(|f| f.name.as_str()).collect();
        assert_eq!(folders, vec!["pets", "store"]);
        assert_eq!(c.items[0].as_folder().unwrap().description, "Everything about your pets");

        let list = find(&c, "List all pets");
        let http = list.http().unwrap();
        assert_eq!(http.method, "GET");
        assert_eq!(http.url, "{{baseUrl}}/pets?limit=20");
        assert_eq!(http.params.len(), 2);
        assert!(!http.params[1].enabled, "optional params without example are disabled");
        assert_eq!(http.header("accept"), Some("application/json"));
        assert_eq!(list.examples.len(), 2);
        assert_eq!(list.examples[0].status, 200);
        let body: Value = serde_json::from_str(&list.examples[0].body).unwrap();
        assert_eq!(body[0]["name"], "doggie");
        assert_eq!(list.examples[1].status, 500);

        let show = find(&c, "Info for a specific pet");
        let h = show.http().unwrap();
        assert_eq!(h.url, "{{baseUrl}}/pets/:petId");
        assert_eq!(h.path_vars[0].key, "petId");
        assert_eq!(h.path_vars[0].value, "42");
        assert!(h.headers.iter().find(|x| x.key == "X-Request-Id").unwrap().enabled);

        let create = find(&c, "createPets");
        let h = create.http().unwrap();
        assert_eq!(h.body.mode, BodyMode::Raw);
        let b: Value = serde_json::from_str(&h.body.raw).unwrap();
        assert_eq!(b["name"], "string");
        assert!(b.get("id").is_none(), "readOnly properties are skipped in requests");
        assert_eq!(b["tag"]["name"], "string");
        assert_eq!(h.auth.kind, AuthType::OAuth2);
        assert_eq!(h.auth.get("grant_type"), "authorization_code");
        assert_eq!(h.auth.get("scope"), "write:pets");

        let upload = find(&c, "Upload an image");
        let h = upload.http().unwrap();
        assert_eq!(h.body.mode, BodyMode::FormData);
        assert_eq!(h.body.form_data[0].kind, FormFieldKind::File);
        assert_eq!(h.body.form_data[1].key, "caption");

        let order = find(&c, "Place an order");
        let h = order.http().unwrap();
        assert_eq!(h.body.mode, BodyMode::UrlEncoded);
        assert_eq!(h.auth.kind, AuthType::None);

        assert_eq!(c.auth.kind, AuthType::ApiKey);
        assert_eq!(c.auth.get("key"), "X-API-Key");
        assert!(c.variables.iter().any(|v| v.key == "apiKey" && v.is_secret()));

        // Cyclic schema does not hang and yields a value.
        let tree = find(&c, "Get category tree");
        let ex: Value = serde_json::from_str(&tree.examples[0].body).unwrap();
        assert_eq!(ex["name"], "string");
        assert!(warnings.iter().all(|w| !w.contains("unresolved")), "{warnings:?}");
    }

    #[test]
    fn imports_swagger2_json() {
        let (c, _) = import(PETSTORE_V2).unwrap();
        assert_eq!(c.variables[0].value, "https://petstore.swagger.io/v2");
        let add = find(&c, "Add a new pet to the store");
        let h = add.http().unwrap();
        assert_eq!(h.method, "POST");
        assert_eq!(h.body.raw_language, RawLanguage::Json);
        let b: Value = serde_json::from_str(&h.body.raw).unwrap();
        assert_eq!(b["photoUrls"], json!(["string"]));
        assert_eq!(b["status"], "available");
        assert_eq!(h.auth.kind, AuthType::OAuth2);
        assert_eq!(h.auth.get("grant_type"), "implicit");

        let upload = find(&c, "uploads an image");
        let h = upload.http().unwrap();
        assert_eq!(h.body.mode, BodyMode::FormData);
        assert!(h.body.form_data.iter().any(|f| f.kind == FormFieldKind::File && f.key == "file"));
        assert_eq!(h.path_vars[0].key, "petId");

        let by_status = find(&c, "Finds Pets by status");
        let h = by_status.http().unwrap();
        assert_eq!(h.params[0].key, "status");
        assert_eq!(h.params[0].value, "available");
        let get = find(&c, "Find pet by ID");
        assert_eq!(get.http().unwrap().auth.kind, AuthType::ApiKey);
        assert!(get.examples.iter().any(|e| e.status == 404));
    }

    #[test]
    fn export_roundtrips_main_shape() {
        let (c, _) = import(PETSTORE_V3).unwrap();
        let doc = export(&c);
        assert_eq!(doc["openapi"], "3.0.3");
        assert_eq!(doc["servers"][0]["url"], "https://petstore.example.com/v1");
        assert!(doc["paths"]["/pets/{petId}"]["get"].is_object());
        assert_eq!(doc["paths"]["/pets/{petId}"]["get"]["parameters"][0]["in"], "path");
        assert_eq!(doc["paths"]["/pets"]["get"]["tags"][0], "pets");
        assert!(doc["components"]["securitySchemes"]["oauth2"].is_object());
        // Re-import the export.
        let text = serde_json::to_string(&doc).unwrap();
        let (again, _) = import(&text).unwrap();
        assert_eq!(again.request_count(), c.request_count());
        let show = find(&again, "Info for a specific pet");
        assert_eq!(show.http().unwrap().url, "{{baseUrl}}/pets/:petId");
    }

    #[test]
    fn export_from_hand_made_collection() {
        let mut c = Collection::new("Mine");
        c.variables.push(Variable::new("baseUrl", "http://localhost:3000"));
        c.auth = Auth::bearer("{{token}}");
        let mut r = HttpRequest::new("POST", "{{baseUrl}}/users/:id/notes?draft=true");
        r.path_vars[0].value = "7".into();
        r.body = Body::raw_json("{\"text\": \"hi\", \"n\": 1}");
        let mut item = RequestItem::new("Add note", r);
        item.examples.push(Example {
            status: 201,
            name: "Created".into(),
            body: "{\"ok\": true}".into(),
            ..Default::default()
        });
        c.items.push(item.into());
        let doc = export(&c);
        let op = &doc["paths"]["/users/{id}/notes"]["post"];
        assert_eq!(op["operationId"], "addNote");
        assert_eq!(op["requestBody"]["content"]["application/json"]["schema"]["properties"]["n"]["type"], "integer");
        assert_eq!(op["responses"]["201"]["content"]["application/json"]["example"]["ok"], true);
        assert_eq!(doc["security"][0]["bearerAuth"], json!([]));
        assert_eq!(doc["servers"][0]["url"], "http://localhost:3000");
    }

    #[test]
    fn parameter_examples_follow_refs() {
        let doc = json!({
            "openapi": "3.0.3", "info": {"title": "t", "version": "1"},
            "paths": {"/items/{id}": {"get": {
                "parameters": [
                    {"name": "id", "in": "path", "required": true, "schema": {"type": "string"},
                     "examples": {"one": {"$ref": "#/components/examples/ItemId"}}},
                    {"name": "q", "in": "query", "schema": {"type": "string"},
                     "examples": {"one": {"$ref": "#/components/examples/Query"}}}
                ],
                "responses": {"200": {"description": "ok"}}
            }}},
            "components": {"examples": {"ItemId": {"value": "42"}, "Query": {"value": "shoes"}}}
        });
        let (c, warnings) = import(&doc.to_string()).unwrap();
        let h = c.requests()[0].http().unwrap();
        assert_eq!(h.path_vars[0].value, "42", "{warnings:?}");
        assert_eq!(h.url, "{{baseUrl}}/items/:id?q=shoes");
    }

    #[test]
    fn rejects_non_openapi() {
        assert!(import("{\"foo\": 1}").is_err());
        assert!(import("openapi: 4.0.0\npaths: {}").is_err());
    }

    #[test]
    fn xml_rendering() {
        let v = json!({"id": 1, "tags": ["a", "b"], "owner": {"name": "x & y"}});
        let x = json_to_xml("Pet", &v, 0);
        assert_eq!(
            x,
            "<Pet>\n  <id>1</id>\n  <tags>a</tags>\n  <tags>b</tags>\n  <owner>\n    <name>x &amp; y</name>\n  </owner>\n</Pet>"
        );
    }

    #[test]
    fn recursive_schemas_example_is_bounded() {
        // Eight schemas whose properties reference every schema: no reference
        // is expanded inside itself, yet the paths multiply up to MAX_DEPTH.
        let names: Vec<String> = (0..8).map(|i| format!("S{i}")).collect();
        let mut schemas = Map::new();
        for n in &names {
            let props: Map<String, Value> = names
                .iter()
                .map(|t| (format!("to{t}"), json!({"$ref": format!("#/components/schemas/{t}")})))
                .chain([("label".to_string(), json!({"type": "string", "example": "x".repeat(200)}))])
                .collect();
            schemas.insert(n.clone(), json!({"type": "object", "properties": props}));
        }
        let doc = json!({
            "openapi": "3.0.3", "info": {"title": "t", "version": "1"},
            "paths": {"/s": {"post": {
                "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/S0"}}}},
                "responses": {"200": {"description": "ok",
                    "content": {"application/json": {"schema": {"$ref": "#/components/schemas/S1"}}}}}
            }}},
            "components": {"schemas": schemas}
        });
        let started = std::time::Instant::now();
        let (c, warnings) = import(&doc.to_string()).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let r = &c.requests()[0];
        let body = &r.http().unwrap().body.raw;
        assert!(body.len() < 512 * 1024, "request example is {} bytes", body.len());
        assert!(r.examples[0].body.len() < 512 * 1024, "response example is {} bytes", r.examples[0].body.len());
        assert!(serde_json::from_str::<Value>(body).unwrap()["toS1"].is_object());
        assert!(warnings.iter().any(|w| w.contains("#/components/schemas/S0") && w.contains("cut")), "{warnings:?}");
    }

    #[test]
    fn export_never_writes_credentials_as_examples() {
        let mut c = Collection::new("Creds");
        c.variables.push(Variable::secret("tenant", "tenant-secret-1"));
        let mut r = HttpRequest::new("POST", "https://h.test/t/:tenant/items/:id?access_token=query-secret-2&page=2");
        r.path_vars[0].value = "tenant-secret-1".into();
        r.path_vars[1].value = "42".into();
        r.headers.push(KeyValue::new("X-Api-Key", "header-secret-3"));
        r.headers.push(KeyValue::new("X-Upstream", "Bearer header-secret-4"));
        r.headers.push(KeyValue::new("X-Session-Jwt", "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl"));
        r.headers.push(KeyValue::new("X-Forwarded-Jwt", "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl"));
        r.headers.push(KeyValue::new("X-Trace", "trace-1"));
        r.body = Body::raw_json("{\"user\": \"ada\", \"password\": \"body-secret-5\"}");
        let mut item = RequestItem::new("Create", r);
        item.examples.push(Example {
            status: 200,
            name: "OK".into(),
            body: "{\"access_token\": \"response-secret-6\", \"expires\": 3600}".into(),
            ..Default::default()
        });
        c.items.push(item.into());
        let mut form = HttpRequest::new("PUT", "https://h.test/login");
        form.body.mode = BodyMode::UrlEncoded;
        form.body.urlencoded = vec![KeyValue::new("client_secret", "form-secret-7"), KeyValue::new("scope", "read")];
        c.items.push(RequestItem::new("Login", form).into());

        let text = serde_json::to_string(&export(&c)).unwrap();
        for secret in (1..=7).map(|n| format!("secret-{n}")).chain(["c2lnbmF0dXJl".to_string()]) {
            assert!(!text.contains(&secret), "{secret} leaked: {text}");
        }
        let doc = export(&c);
        let params = doc["paths"]["/t/{tenant}/items/{id}"]["post"]["parameters"].as_array().unwrap().clone();
        let example = |name: &str| params.iter().find(|p| p["name"] == name).map(|p| p["example"].clone());
        assert_eq!(example("id"), Some(json!("42")));
        assert_eq!(example("page"), Some(json!("2")));
        assert_eq!(example("X-Trace"), Some(json!("trace-1")));
        assert_eq!(example("X-Api-Key"), Some(Value::Null), "the parameter stays, without example");
        let body = &doc["paths"]["/t/{tenant}/items/{id}"]["post"]["requestBody"]["content"]["application/json"];
        assert_eq!(body["example"]["user"], "ada");
        assert_eq!(body["example"]["password"], crate::docs::REDACTED);
        let form = &doc["paths"]["/login"]["put"]["requestBody"]["content"]["application/x-www-form-urlencoded"];
        assert_eq!(form["example"]["scope"], "read");
    }
}
