//! Smithy model -> Merpati collection.

use std::collections::HashSet;

use indexmap::IndexMap;
use merpati_core::model::{
    Body, BodyMode, Collection, Example, Folder, HttpRequest, Item, KeyValue, RawLanguage, RequestItem,
};
use merpati_core::variables::Variable;
use merpati_core::{Auth, AuthType};
use serde_json::{Map, Value, json};

use super::validate::{NON_BODY_BINDINGS, RPC_PROTOCOLS, parse_uri, protocol};
use super::{Member, Model, Shape, ShapeKind, local_name};
use crate::util::{as_string, pretty, status_text};

/// Value of the `baseUrl` variable: Smithy models do not define endpoints.
pub const DEFAULT_BASE_URL: &str = "http://localhost";
const MAX_DEPTH: usize = 10;

/// Where a value is serialized; decides the default timestamp format.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Location {
    Body,
    Header,
    Query,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    Json,
    /// awsJson1_x: like JSON but ignores `@jsonName`.
    AwsJson,
    Xml,
}

struct Converter<'a> {
    m: &'a Model,
    warnings: Vec<String>,
    auth_vars: IndexMap<String, bool>,
}

pub(super) fn to_collection(m: &Model) -> (Collection, Vec<String>) {
    let mut cv = Converter { m, warnings: Vec::new(), auth_vars: IndexMap::new() };
    let services: Vec<&Shape> = m.services().collect();
    let ns_name = if m.namespace.is_empty() { "Smithy model".to_string() } else { m.namespace.clone() };
    let mut c = match services.as_slice() {
        [] => {
            let mut c = Collection::new(ns_name);
            let ops: Vec<&Shape> =
                m.shapes.values().filter(|s| s.kind == ShapeKind::Operation && s.has_trait("http")).collect();
            if ops.is_empty() {
                cv.warn("the model has no service and no `@http` operations");
            } else {
                cv.warn("the model has no service; importing every `@http` operation");
            }
            for op in ops {
                if let Some(item) = cv.request(op, None) {
                    c.items.push(item);
                }
            }
            c
        }
        [svc] => {
            let mut c = Collection::new(service_title(svc));
            c.description = service_description(svc);
            c.auth = cv.service_auth(svc).unwrap_or_default();
            c.items = cv.service_items(svc);
            c
        }
        many => {
            let mut c = Collection::new(ns_name);
            for svc in many {
                let mut f = Folder::new(service_title(svc));
                f.description = service_description(svc);
                f.auth = cv.service_auth(svc).unwrap_or_default();
                f.items = cv.service_items(svc);
                c.items.push(Item::Folder(f));
            }
            c
        }
    };
    c.variables.push(Variable::new("baseUrl", DEFAULT_BASE_URL));
    cv.warn("Smithy models do not define endpoints: set the `baseUrl` variable");
    for (name, secret) in std::mem::take(&mut cv.auth_vars) {
        c.variables.push(if secret { Variable::secret(name, "") } else { Variable::new(name, "") });
    }
    (c, cv.warnings)
}

fn service_title(svc: &Shape) -> String {
    svc.get_trait("title").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| svc.name().to_string())
}

fn service_description(svc: &Shape) -> String {
    let mut parts = Vec::new();
    if let Some(d) = svc.docs().map(str::trim).filter(|d| !d.is_empty()) {
        parts.push(d.to_string());
    }
    let mut facts = Vec::new();
    if let Some(v) = &svc.version {
        facts.push(format!("Version: {v}"));
    }
    if let Some(p) = protocol(svc) {
        facts.push(format!("Protocol: {p}"));
    }
    if !facts.is_empty() {
        parts.push(facts.join(". ") + ".");
    }
    parts.join("\n\n")
}

impl<'a> Converter<'a> {
    fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    fn var(&mut self, name: &str, secret: bool) -> String {
        self.auth_vars.entry(name.to_string()).or_insert(secret);
        format!("{{{{{name}}}}}")
    }

    fn shape(&self, id: &str) -> Option<&'a Shape> {
        self.m.shape(id)
    }

    // -- auth -----------------------------------------------------------------

    fn auth_for_scheme(&mut self, scheme: &str, value: &Value) -> Option<Auth> {
        Some(match local_name(scheme) {
            "httpBearerAuth" => Auth::bearer(self.var("bearerToken", true)),
            "httpBasicAuth" => Auth::basic(self.var("username", false), self.var("password", true)),
            "httpDigestAuth" => Auth::new(AuthType::Digest)
                .with("username", self.var("username", false))
                .with("password", self.var("password", true)),
            "httpApiKeyAuth" => {
                let name = value.get("name").and_then(Value::as_str).unwrap_or("X-Api-Key").to_string();
                let in_query = value.get("in").and_then(Value::as_str) == Some("query");
                let key = self.var("apiKey", true);
                let v = match value.get("scheme").and_then(Value::as_str) {
                    Some(s) if !in_query => format!("{s} {key}"),
                    _ => key,
                };
                Auth::api_key(name, v, in_query)
            }
            "sigv4" | "sigv4a" => Auth::new(AuthType::AwsV4)
                .with("accessKey", self.var("awsAccessKeyId", false))
                .with("secretKey", self.var("awsSecretAccessKey", true))
                .with("region", self.var("awsRegion", false))
                .with("service", value.get("name").and_then(Value::as_str).unwrap_or("").to_string()),
            _ => return None,
        })
    }

    /// The first supported auth scheme of a service, honouring `@auth([...])`.
    fn service_auth(&mut self, svc: &Shape) -> Option<Auth> {
        const SCHEMES: [&str; 6] =
            ["httpBearerAuth", "httpApiKeyAuth", "httpBasicAuth", "httpDigestAuth", "sigv4", "sigv4a"];
        let order: Vec<String> = match svc.get_trait("auth").and_then(Value::as_array) {
            Some(list) => list.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            None => svc.traits.keys().filter(|k| SCHEMES.contains(&local_name(k))).cloned().collect(),
        };
        for scheme in order {
            let value = svc.get_trait(local_name(&scheme)).cloned().unwrap_or(Value::Null);
            if let Some(a) = self.auth_for_scheme(&scheme, &value) {
                return Some(a);
            }
            self.warn(format!("auth scheme `{scheme}` is not supported"));
        }
        None
    }

    fn operation_auth(&mut self, op: &Shape, svc: Option<&Shape>) -> Option<Auth> {
        let list = op.get_trait("auth")?.as_array()?;
        let Some(first) = list.iter().filter_map(Value::as_str).next() else { return Some(Auth::none()) };
        let value = svc.and_then(|s| s.get_trait(local_name(first))).cloned().unwrap_or(Value::Null);
        let auth = self.auth_for_scheme(first, &value)?;
        let inherited = svc.and_then(|s| self.service_auth(s));
        (inherited.as_ref() != Some(&auth)).then_some(auth)
    }

    // -- tree -----------------------------------------------------------------

    fn service_items(&mut self, svc: &'a Shape) -> Vec<Item> {
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        for op in &svc.operations {
            if let Some(op) = self.shape(&op.id).filter(|o| o.kind == ShapeKind::Operation)
                && seen.insert(op.id.clone())
                && let Some(item) = self.request(op, Some(svc))
            {
                items.push(item);
            }
        }
        let mut visited = HashSet::new();
        for r in &svc.resources {
            if let Some(f) = self.resource_folder(&r.id, svc, &mut seen, &mut visited) {
                items.push(Item::Folder(f));
            }
        }
        items
    }

    fn resource_folder(
        &mut self,
        id: &str,
        svc: &'a Shape,
        seen: &mut HashSet<String>,
        visited: &mut HashSet<String>,
    ) -> Option<Folder> {
        if !visited.insert(id.to_string()) {
            return None;
        }
        let r = self.shape(id).filter(|r| r.kind == ShapeKind::Resource)?;
        let mut f = Folder::new(r.name());
        let mut desc = Vec::new();
        if let Some(d) = r.docs().map(str::trim).filter(|d| !d.is_empty()) {
            desc.push(d.to_string());
        }
        if !r.identifiers.is_empty() {
            let ids: Vec<String> =
                r.identifiers.iter().map(|(k, v)| format!("`{k}` ({})", local_name(&v.id))).collect();
            desc.push(format!("Identifiers: {}.", ids.join(", ")));
        }
        f.description = desc.join("\n\n");
        let ops: Vec<String> =
            r.lifecycle.values().chain(&r.collection_operations).chain(&r.operations).map(|x| x.id.clone()).collect();
        for op_id in ops {
            if !seen.insert(op_id.clone()) {
                continue;
            }
            if let Some(op) = self.shape(&op_id).filter(|o| o.kind == ShapeKind::Operation)
                && let Some(item) = self.request(op, Some(svc))
            {
                f.items.push(item);
            }
        }
        for child in &r.resources {
            if let Some(cf) = self.resource_folder(&child.id, svc, seen, visited) {
                f.items.push(Item::Folder(cf));
            }
        }
        Some(f)
    }

    // -- requests -------------------------------------------------------------

    fn wire_for(&self, svc: Option<&Shape>) -> Wire {
        match svc.and_then(protocol) {
            Some("restXml") | Some("awsQuery") | Some("ec2Query") => Wire::Xml,
            Some("awsJson1_0") | Some("awsJson1_1") => Wire::AwsJson,
            _ => Wire::Json,
        }
    }

    fn request(&mut self, op: &'a Shape, svc: Option<&'a Shape>) -> Option<Item> {
        let proto = svc.and_then(protocol);
        let input = op.input.as_ref().and_then(|r| self.shape(&r.id)).filter(|s| s.kind == ShapeKind::Structure);
        let example = op
            .get_trait("examples")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find(|e| e.get("input").is_some() || e.get("output").is_some()));
        let ex_input = example.and_then(|e| e.get("input")).and_then(Value::as_object);
        let wire = self.wire_for(svc);

        let mut req = if op.has_trait("http") {
            self.rest_request(op, input, ex_input, wire)
        } else {
            match proto {
                Some(p @ ("awsJson1_0" | "awsJson1_1")) => {
                    let svc = svc.expect("protocol implies a service");
                    let mut req = HttpRequest::new("POST", "{{baseUrl}}/");
                    req.headers.push(KeyValue::new("X-Amz-Target", format!("{}.{}", svc.name(), op.name())));
                    let ct =
                        if p == "awsJson1_0" { "application/x-amz-json-1.0" } else { "application/x-amz-json-1.1" };
                    req.headers.push(KeyValue::new("Content-Type", ct));
                    let body = match input {
                        Some(i) => self.body_object(i, ex_input, Wire::AwsJson, true),
                        None => json!({}),
                    };
                    req.body = Body::raw_json(pretty(&body));
                    req
                }
                Some(p @ ("awsQuery" | "ec2Query")) => {
                    let svc = svc.expect("protocol implies a service");
                    let mut req = HttpRequest::new("POST", "{{baseUrl}}/");
                    req.headers.push(KeyValue::new("Content-Type", "application/x-www-form-urlencoded"));
                    req.body.mode = BodyMode::UrlEncoded;
                    req.body.urlencoded.push(KeyValue::new("Action", op.name()));
                    req.body.urlencoded.push(KeyValue::new("Version", svc.version.clone().unwrap_or_default()));
                    if let Some(i) = input {
                        for mem in i.members.values() {
                            let mut name = mem.name.clone();
                            if p == "ec2Query" {
                                name = uppercase_first(&name);
                            }
                            let v = ex_input
                                .and_then(|e| e.get(&mem.name))
                                .cloned()
                                .unwrap_or_else(|| self.sample(&mem.target, Some(mem), Location::Query, wire, 0));
                            let mut kv = KeyValue::new(name, scalar_text(&v));
                            kv.enabled = mem.has_trait("required");
                            kv.description = mem.docs().unwrap_or_default().to_string();
                            req.body.urlencoded.push(kv);
                        }
                    }
                    req
                }
                Some(p) if RPC_PROTOCOLS.contains(&p) => {
                    self.warn(format!("operation `{}`: protocol `{p}` is not supported; skipped", op.name()));
                    return None;
                }
                _ => {
                    self.warn(format!("operation `{}` has no `@http` trait; skipped", op.name()));
                    return None;
                }
            }
        };
        if let Some(a) = self.operation_auth(op, svc) {
            req.auth = a;
        }

        let mut item = RequestItem::new(op.name(), req.clone());
        item.description = self.op_description(op, svc);
        item.examples = self.examples(op, &req, wire);
        Some(Item::Request(item))
    }

    fn rest_request(
        &mut self,
        op: &Shape,
        input: Option<&'a Shape>,
        ex_input: Option<&Map<String, Value>>,
        wire: Wire,
    ) -> HttpRequest {
        let http = op.get_trait("http").cloned().unwrap_or(Value::Null);
        let method = http.get("method").and_then(Value::as_str).unwrap_or("GET").to_ascii_uppercase();
        let uri = http.get("uri").and_then(Value::as_str).unwrap_or("/");
        let mut path = String::new();
        let mut literal_query = Vec::new();
        match parse_uri(uri) {
            Ok(p) => {
                for (seg, label, _) in &p.segments {
                    path.push('/');
                    if *label {
                        path.push(':');
                    }
                    path.push_str(seg);
                }
                if p.trailing_slash || path.is_empty() {
                    path.push('/');
                }
                literal_query = p.query;
            }
            Err(e) => {
                self.warn(format!("operation `{}`: {e}", op.name()));
                path = uri.split('?').next().unwrap_or("/").to_string();
            }
        }
        let mut req = HttpRequest::new(method, format!("{{{{baseUrl}}}}{path}"));
        let mut params: Vec<KeyValue> = literal_query.into_iter().map(|(k, v)| KeyValue::new(k, v)).collect();

        let members: Vec<&'a Member> = input.map(|s| s.members.values().collect()).unwrap_or_default();
        let ex = |name: &str| ex_input.and_then(|e| e.get(name)).filter(|v| !v.is_null());
        let mut payload: Option<&'a Member> = None;
        let mut body_members: Vec<&'a Member> = Vec::new();
        for mem in &members {
            let docs = mem.docs().unwrap_or_default().to_string();
            let required = mem.has_trait("required");
            let value_for = |cv: &Converter<'a>, loc: Location| -> (String, bool) {
                match ex(&mem.name) {
                    Some(v) => (scalar_text(v), true),
                    None => {
                        let v = cv.sample(&mem.target, Some(mem), loc, wire, 0);
                        (scalar_text(&v), mem.has_trait("default") || mem.has_trait("idempotencyToken"))
                    }
                }
            };
            if mem.has_trait("httpLabel") {
                let (value, from_example) = value_for(self, Location::Query);
                if let Some(pv) = req.path_vars.iter_mut().find(|v| v.key == mem.name) {
                    pv.value = if from_example || mem.has_trait("default") { value } else { String::new() };
                    pv.description = docs;
                }
            } else if let Some(name) = mem.get_trait("httpQuery").and_then(Value::as_str) {
                let (value, known) = value_for(self, Location::Query);
                params.push(KeyValue { key: name.to_string(), value, enabled: required || known, description: docs });
            } else if mem.has_trait("httpQueryParams") {
                let mut kv = KeyValue::new("key", "value").disabled();
                kv.description = if docs.is_empty() { "Free-form query parameters".into() } else { docs };
                params.push(kv);
            } else if let Some(name) = mem.get_trait("httpHeader").and_then(Value::as_str) {
                let (value, known) = value_for(self, Location::Header);
                req.headers.push(KeyValue {
                    key: name.to_string(),
                    value,
                    enabled: required || known,
                    description: docs,
                });
            } else if let Some(prefix) = mem.get_trait("httpPrefixHeaders").and_then(Value::as_str) {
                let mut kv = KeyValue::new(format!("{prefix}name"), "value").disabled();
                kv.description = docs;
                req.headers.push(kv);
            } else if mem.has_trait("httpPayload") {
                payload = Some(mem);
            } else if !NON_BODY_BINDINGS.iter().any(|b| mem.has_trait(b)) {
                body_members.push(mem);
            }
        }
        if !params.is_empty() {
            req.params = params;
            req.sync_url_from_params();
        }

        let mut content_type: Option<String> = None;
        if let Some(p) = payload {
            let target = self.shape(&p.target);
            let kind = target.map(|t| t.kind).or_else(|| prelude_kind(&p.target));
            let media = p
                .get_trait("mediaType")
                .or_else(|| target.and_then(|t| t.get_trait("mediaType")))
                .and_then(Value::as_str)
                .map(str::to_string);
            match kind {
                Some(ShapeKind::Blob) => {
                    req.body.mode = BodyMode::Binary;
                    content_type = Some(media.unwrap_or_else(|| "application/octet-stream".into()));
                }
                Some(ShapeKind::String) | Some(ShapeKind::Enum) => {
                    let text = ex(&p.name).map(scalar_text).unwrap_or_default();
                    let ct = media.unwrap_or_else(|| "text/plain".into());
                    req.body = Body::raw_text(text, crate::util::raw_language_for(&ct));
                    content_type = Some(ct);
                }
                _ => {
                    let value = match ex(&p.name) {
                        Some(v) => self.to_wire(v, &p.target, wire, 0),
                        None => self.sample(&p.target, Some(p), Location::Body, wire, 0),
                    };
                    let (body, ct) = self.encode_body(&value, &p.target, p.get_trait("xmlName"), wire);
                    req.body = body;
                    content_type = Some(media.unwrap_or(ct));
                }
            }
        } else if !body_members.is_empty()
            && let Some(input) = input
        {
            let mut obj = Map::new();
            for mem in &body_members {
                let key = wire_name(mem, wire);
                match ex(&mem.name) {
                    Some(v) => {
                        obj.insert(key, self.to_wire(v, &mem.target, wire, 0));
                    }
                    None if ex_input.is_none() || mem.has_trait("required") => {
                        obj.insert(key, self.sample(&mem.target, Some(mem), Location::Body, wire, 0));
                    }
                    None => {}
                }
            }
            let (body, ct) = self.encode_body(&Value::Object(obj), &input.id, input.get_trait("xmlName"), wire);
            req.body = body;
            content_type = Some(ct);
        }
        if let Some(ct) = content_type {
            match req.headers.iter_mut().find(|h| h.key.eq_ignore_ascii_case("content-type")) {
                // A `@httpHeader("Content-Type")` member: fill in the media type.
                Some(h) if !h.enabled || h.value == "string" => {
                    h.value = ct;
                    h.enabled = true;
                }
                Some(_) => {}
                None => req.headers.push(KeyValue::new("Content-Type", ct)),
            }
        }
        req
    }

    fn encode_body(&self, value: &Value, shape_id: &str, xml_name: Option<&Value>, wire: Wire) -> (Body, String) {
        match wire {
            Wire::Xml => {
                let root = xml_name
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        self.shape(shape_id)
                            .and_then(|s| s.get_trait("xmlName").and_then(Value::as_str).map(str::to_string))
                    })
                    .unwrap_or_else(|| local_name(shape_id).to_string());
                let mut xml = String::new();
                write_xml(&mut xml, &root, value, 0);
                (Body::raw_text(xml, RawLanguage::Xml), "application/xml".into())
            }
            _ => (Body::raw_json(pretty(value)), "application/json".into()),
        }
    }

    /// JSON object for the body members of a structure (awsJson: all members).
    fn body_object(&self, s: &Shape, ex: Option<&Map<String, Value>>, wire: Wire, all: bool) -> Value {
        let mut obj = Map::new();
        for mem in s.members.values() {
            if !all && NON_BODY_BINDINGS.iter().any(|b| mem.has_trait(b)) {
                continue;
            }
            let key = wire_name(mem, wire);
            match ex.and_then(|e| e.get(&mem.name)) {
                Some(v) => {
                    obj.insert(key, self.to_wire(v, &mem.target, wire, 0));
                }
                None if ex.is_none() || mem.has_trait("required") => {
                    obj.insert(key, self.sample(&mem.target, Some(mem), Location::Body, wire, 0));
                }
                None => {}
            }
        }
        Value::Object(obj)
    }

    fn op_description(&self, op: &Shape, svc: Option<&Shape>) -> String {
        let mut parts = Vec::new();
        if op.has_trait("deprecated") {
            let msg = op.get_trait("deprecated").and_then(|d| d.get("message")).and_then(Value::as_str);
            parts.push(match msg {
                Some(m) => format!("Deprecated: {m}"),
                None => "Deprecated.".to_string(),
            });
        }
        if let Some(d) = op.docs().map(str::trim).filter(|d| !d.is_empty()) {
            parts.push(d.to_string());
        }
        let mut facts = Vec::new();
        if op.has_trait("readonly") {
            facts.push("Read-only.".to_string());
        }
        if op.has_trait("idempotent") {
            facts.push("Idempotent.".to_string());
        }
        if let Some(Value::Object(own)) = op.get_trait("paginated") {
            // Service-level `@paginated` provides defaults.
            let mut p =
                svc.and_then(|s| s.get_trait("paginated")).and_then(Value::as_object).cloned().unwrap_or_default();
            p.extend(own.clone());
            let keys: Vec<String> = ["inputToken", "outputToken", "items", "pageSize"]
                .iter()
                .filter_map(|k| p.get(*k).and_then(Value::as_str).map(|v| format!("{k} `{v}`")))
                .collect();
            facts.push(if keys.is_empty() { "Paginated.".into() } else { format!("Paginated: {}.", keys.join(", ")) });
        }
        if !op.errors.is_empty() {
            let errs: Vec<String> = op
                .errors
                .iter()
                .map(|e| {
                    let code = self.shape(&e.id).map(error_status).map(|c| format!(" ({c})")).unwrap_or_default();
                    format!("{}{code}", local_name(&e.id))
                })
                .collect();
            facts.push(format!("Errors: {}.", errs.join(", ")));
        }
        if !facts.is_empty() {
            parts.push(facts.join(" "));
        }
        parts.join("\n\n")
    }

    // -- examples -------------------------------------------------------------

    /// Response examples: from `@examples` when present, otherwise skeletons
    /// of the output and of each error.
    fn examples(&mut self, op: &Shape, req: &HttpRequest, wire: Wire) -> Vec<Example> {
        let code = op.get_trait("http").and_then(|h| h.get("code")).and_then(Value::as_u64).unwrap_or(200) as u16;
        let output = op.output.as_ref().and_then(|r| self.shape(&r.id)).filter(|s| s.kind == ShapeKind::Structure);
        let mut out = Vec::new();
        if let Some(Value::Array(list)) = op.get_trait("examples") {
            for (i, ex) in list.iter().enumerate() {
                let title = ex
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Example {}", i + 1));
                let (status, shape, data) = if let Some(err) = ex.get("error") {
                    let id = err.get("shapeId").and_then(Value::as_str).unwrap_or("");
                    let shape = self.shape(id).or_else(|| {
                        op.errors.iter().find(|e| local_name(&e.id) == local_name(id)).and_then(|e| self.shape(&e.id))
                    });
                    (shape.map(error_status).unwrap_or(400), shape, err.get("content").cloned())
                } else {
                    (code, output, ex.get("output").cloned())
                };
                let mut e = self.response_example(title, status, shape, data.as_ref().and_then(Value::as_object), wire);
                let mut r = req.clone();
                if let Some(Value::Object(input)) = ex.get("input") {
                    self.apply_example_input(&mut r, op, input);
                }
                e.request = Some(r);
                if let Some(d) = ex.get("documentation").and_then(Value::as_str) {
                    e.name = format!("{} ({d})", e.name);
                }
                out.push(e);
            }
            if !out.is_empty() {
                return out;
            }
        }
        let name = format!("{code} {}", status_text(code)).trim().to_string();
        let mut e = self.response_example(name, code, output, None, wire);
        e.request = Some(req.clone());
        out.push(e);
        for err in &op.errors {
            let Some(shape) = self.shape(&err.id) else { continue };
            let status = error_status(shape);
            let mut e = self.response_example(shape.name().to_string(), status, Some(shape), None, wire);
            e.request = Some(req.clone());
            out.push(e);
        }
        out
    }

    fn apply_example_input(&self, r: &mut HttpRequest, op: &Shape, input: &Map<String, Value>) {
        let Some(shape) = op.input.as_ref().and_then(|i| self.shape(&i.id)) else { return };
        for mem in shape.members.values() {
            let Some(v) = input.get(&mem.name) else { continue };
            if mem.has_trait("httpLabel") {
                if let Some(pv) = r.path_vars.iter_mut().find(|p| p.key == mem.name) {
                    pv.value = scalar_text(v);
                }
            } else if let Some(name) = mem.get_trait("httpQuery").and_then(Value::as_str) {
                if let Some(p) = r.params.iter_mut().find(|p| p.key == name) {
                    p.value = scalar_text(v);
                    p.enabled = true;
                }
            } else if let Some(name) = mem.get_trait("httpHeader").and_then(Value::as_str)
                && let Some(h) = r.headers.iter_mut().find(|h| h.key.eq_ignore_ascii_case(name))
            {
                h.value = scalar_text(v);
                h.enabled = true;
            }
        }
        r.sync_url_from_params();
    }

    fn response_example(
        &self,
        name: String,
        status: u16,
        shape: Option<&Shape>,
        data: Option<&Map<String, Value>>,
        wire: Wire,
    ) -> Example {
        let mut e = Example { name, status, status_text: status_text(status).to_string(), ..Default::default() };
        let Some(shape) = shape else { return e };
        let mut obj = Map::new();
        let mut payload: Option<Value> = None;
        let mut payload_target = String::new();
        for mem in shape.members.values() {
            let v = match data.and_then(|d| d.get(&mem.name)) {
                Some(v) => self.to_wire(v, &mem.target, wire, 0),
                None if data.is_some() && !mem.has_trait("required") => continue,
                None => self.sample(&mem.target, Some(mem), Location::Body, wire, 0),
            };
            if let Some(h) = mem.get_trait("httpHeader").and_then(Value::as_str) {
                e.headers.push(KeyValue::new(h, scalar_text(&v)));
            } else if mem.has_trait("httpPayload") {
                payload = Some(v);
                payload_target = mem.target.clone();
            } else if mem.has_trait("httpResponseCode") || NON_BODY_BINDINGS.iter().any(|b| mem.has_trait(b)) {
                continue;
            } else {
                obj.insert(wire_name(mem, wire), v);
            }
        }
        let (value, root) = match payload {
            Some(p) => (p, payload_target),
            None if obj.is_empty() => return e,
            None => (Value::Object(obj), shape.id.clone()),
        };
        match (&value, wire) {
            (Value::String(s), _) if !root.is_empty() && self.kind(&root) == Some(ShapeKind::String) => {
                e.body = s.clone();
                e.language = Some("text".into());
                e.headers.push(KeyValue::new("Content-Type", "text/plain"));
            }
            (_, Wire::Xml) => {
                let mut xml = String::new();
                write_xml(&mut xml, local_name(&root), &value, 0);
                e.body = xml;
                e.language = Some("xml".into());
                e.headers.push(KeyValue::new("Content-Type", "application/xml"));
            }
            _ => {
                e.body = pretty(&value);
                e.language = Some("json".into());
                e.headers.push(KeyValue::new("Content-Type", "application/json"));
            }
        }
        e
    }

    // -- values -----------------------------------------------------------------

    fn kind(&self, id: &str) -> Option<ShapeKind> {
        self.shape(id).map(|s| s.kind).or_else(|| prelude_kind(id))
    }

    /// A placeholder value for a shape.
    fn sample(&self, target: &str, member: Option<&Member>, loc: Location, wire: Wire, depth: usize) -> Value {
        self.sample_in(target, member, loc, wire, depth, &mut Vec::new())
    }

    /// `path` holds the structures and unions being sampled: a recursive
    /// shape is left out at its first repetition, like a too deep one (with
    /// several recursive members the value would grow exponentially).
    fn sample_in(
        &self,
        target: &str,
        member: Option<&Member>,
        loc: Location,
        wire: Wire,
        depth: usize,
        path: &mut Vec<String>,
    ) -> Value {
        if let Some(d) = member.and_then(|m| m.get_trait("default")).filter(|d| !d.is_null()) {
            return d.clone();
        }
        if member.is_some_and(|m| m.has_trait("idempotencyToken")) {
            return Value::String("{{$guid}}".into());
        }
        if depth > MAX_DEPTH || path.iter().any(|p| p == target) {
            return Value::Null;
        }
        let shape = self.shape(target);
        if let Some(d) = shape.and_then(|s| s.get_trait("default")).filter(|d| !d.is_null()) {
            return d.clone();
        }
        let Some(kind) = self.kind(target) else { return Value::Null };
        let traits_of = |name: &str| member.and_then(|m| m.get_trait(name)).or_else(|| shape?.get_trait(name));
        match kind {
            ShapeKind::String => {
                if let Some(Value::Array(e)) = traits_of("enum")
                    && let Some(v) = e.first().and_then(|x| x.get("value"))
                {
                    return v.clone();
                }
                Value::String("string".into())
            }
            ShapeKind::Enum => {
                let first = shape.and_then(|s| s.members.values().next());
                match first {
                    Some(m) => m.get_trait("enumValue").cloned().unwrap_or_else(|| Value::String(m.name.clone())),
                    None => Value::String(String::new()),
                }
            }
            ShapeKind::IntEnum => shape
                .and_then(|s| s.members.values().next())
                .and_then(|m| m.get_trait("enumValue").cloned())
                .unwrap_or(json!(0)),
            ShapeKind::Boolean => json!(true),
            ShapeKind::Byte | ShapeKind::Short | ShapeKind::Integer | ShapeKind::Long | ShapeKind::BigInteger => {
                let min = traits_of("range").and_then(|r| r.get("min")).and_then(Value::as_i64);
                json!(min.unwrap_or(0))
            }
            ShapeKind::Float | ShapeKind::Double | ShapeKind::BigDecimal => {
                let min = traits_of("range").and_then(|r| r.get("min")).and_then(Value::as_f64);
                json!(min.unwrap_or(0.0))
            }
            ShapeKind::Timestamp => {
                let fmt = traits_of("timestampFormat").and_then(Value::as_str).unwrap_or(match (loc, wire) {
                    (Location::Header, _) => "http-date",
                    (Location::Query, _) | (Location::Body, Wire::Xml) => "date-time",
                    (Location::Body, _) => "epoch-seconds",
                });
                match fmt {
                    "epoch-seconds" => json!(1704067200),
                    "http-date" => json!("Mon, 01 Jan 2024 00:00:00 GMT"),
                    _ => json!("2024-01-01T00:00:00Z"),
                }
            }
            ShapeKind::Blob => Value::String(String::new()),
            ShapeKind::Document => json!({}),
            ShapeKind::List | ShapeKind::Set => {
                let Some(m) = shape.and_then(|s| s.members.get("member")) else { return json!([]) };
                match self.sample_in(&m.target, Some(m), loc, wire, depth + 1, path) {
                    Value::Null => json!([]),
                    v => Value::Array(vec![v]),
                }
            }
            ShapeKind::Map => {
                let Some(s) = shape else { return json!({}) };
                let key = s
                    .members
                    .get("key")
                    .map(|k| self.sample_in(&k.target, Some(k), loc, wire, depth + 1, path))
                    .and_then(|v| v.as_str().map(str::to_string))
                    .filter(|k| k != "string")
                    .unwrap_or_else(|| "key".into());
                let value = s
                    .members
                    .get("value")
                    .map(|v| self.sample_in(&v.target, Some(v), loc, wire, depth + 1, path))
                    .unwrap_or(Value::Null);
                let mut m = Map::new();
                m.insert(key, value);
                Value::Object(m)
            }
            ShapeKind::Structure => {
                let Some(s) = shape else { return json!({}) };
                path.push(target.to_string());
                let mut m = Map::new();
                for mem in s.members.values() {
                    let v = self.sample_in(&mem.target, Some(mem), loc, wire, depth + 1, path);
                    if v.is_null() && (depth + 1 > MAX_DEPTH || path.contains(&mem.target)) {
                        continue;
                    }
                    m.insert(wire_name(mem, wire), v);
                }
                path.pop();
                Value::Object(m)
            }
            ShapeKind::Union => {
                let Some(mem) = shape.and_then(|s| s.members.values().next()) else { return json!({}) };
                path.push(target.to_string());
                let v = self.sample_in(&mem.target, Some(mem), loc, wire, depth + 1, path);
                path.pop();
                let mut m = Map::new();
                m.insert(wire_name(mem, wire), v);
                Value::Object(m)
            }
            ShapeKind::Service | ShapeKind::Resource | ShapeKind::Operation => Value::Null,
        }
    }

    /// Renames example keys (member names) to wire names (`@jsonName`).
    fn to_wire(&self, v: &Value, target: &str, wire: Wire, depth: usize) -> Value {
        if depth > MAX_DEPTH {
            return v.clone();
        }
        let Some(shape) = self.shape(target) else { return v.clone() };
        match (v, shape.kind) {
            (Value::Object(o), ShapeKind::Structure | ShapeKind::Union) => {
                let mut out = Map::new();
                for (k, x) in o {
                    match shape.members.get(k) {
                        Some(mem) => {
                            out.insert(wire_name(mem, wire), self.to_wire(x, &mem.target, wire, depth + 1));
                        }
                        None => {
                            out.insert(k.clone(), x.clone());
                        }
                    }
                }
                Value::Object(out)
            }
            (Value::Object(o), ShapeKind::Map) => {
                let Some(vm) = shape.members.get("value") else { return v.clone() };
                Value::Object(
                    o.iter().map(|(k, x)| (k.clone(), self.to_wire(x, &vm.target, wire, depth + 1))).collect(),
                )
            }
            (Value::Array(a), ShapeKind::List | ShapeKind::Set) => {
                let Some(m) = shape.members.get("member") else { return v.clone() };
                Value::Array(a.iter().map(|x| self.to_wire(x, &m.target, wire, depth + 1)).collect())
            }
            _ => v.clone(),
        }
    }
}

fn wire_name(mem: &Member, wire: Wire) -> String {
    let key = match wire {
        Wire::Json => "jsonName",
        Wire::Xml => "xmlName",
        Wire::AwsJson => return mem.name.clone(),
    };
    mem.get_trait(key).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| mem.name.clone())
}

fn uppercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn prelude_kind(id: &str) -> Option<ShapeKind> {
    let name = id.strip_prefix("smithy.api#")?;
    let base = name.strip_prefix("Primitive").unwrap_or(name);
    if base == "Unit" {
        return Some(ShapeKind::Structure);
    }
    let kw = {
        let mut c = base.chars();
        c.next().map(|f| f.to_ascii_lowercase().to_string() + c.as_str()).unwrap_or_default()
    };
    ShapeKind::from_keyword(&kw)
}

/// HTTP status of an error shape: `@httpError`, else 400/500 from `@error`.
fn error_status(s: &Shape) -> u16 {
    if let Some(code) = s.get_trait("httpError").and_then(Value::as_u64) {
        return code as u16;
    }
    match s.get_trait("error").and_then(Value::as_str) {
        Some("server") => 500,
        _ => 400,
    }
}

/// Text for a query/header/label value.
fn scalar_text(v: &Value) -> String {
    match v {
        Value::Array(items) => items.iter().map(as_string).collect::<Vec<_>>().join(","),
        other => as_string(other),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn write_xml(out: &mut String, name: &str, v: &Value, depth: usize) {
    let indent = "  ".repeat(depth);
    match v {
        Value::Object(o) => {
            out.push_str(&format!("{indent}<{name}>\n"));
            for (k, x) in o {
                write_xml(out, k, x, depth + 1);
            }
            out.push_str(&format!("{indent}</{name}>\n"));
        }
        Value::Array(items) => {
            out.push_str(&format!("{indent}<{name}>\n"));
            for x in items {
                write_xml(out, "member", x, depth + 1);
            }
            out.push_str(&format!("{indent}</{name}>\n"));
        }
        Value::Null => out.push_str(&format!("{indent}<{name}/>\n")),
        other => out.push_str(&format!("{indent}<{name}>{}</{name}>\n", xml_escape(&as_string(other)))),
    }
    if depth == 0 && out.ends_with('\n') {
        out.pop();
    }
}
