//! Thunder Client collection import. Declarative Thunder tests (status code,
//! body, JSON query, header, response time, set-env-var) are converted to
//! `pm.test` post-response scripts.

use std::collections::HashMap;

use merpati_core::model::{
    Body, BodyMode, Collection, Folder, FormField, HttpRequest, Item, KeyValue, RawLanguage, RequestItem, Scripts,
};
use merpati_core::variables::Variable;
use merpati_core::{Auth, AuthType};
use serde_json::Value;

use crate::ConvertError;
use crate::util::{arr, str_of};

fn disabled(v: &Value) -> bool {
    v.get("isDisabled").and_then(Value::as_bool).unwrap_or(false)
}

fn kv(list: &[Value]) -> Vec<KeyValue> {
    list.iter()
        .filter(|p| !str_of(p, "name").is_empty())
        .map(|p| {
            let mut kv = KeyValue::new(str_of(p, "name"), str_of(p, "value"));
            kv.enabled = !disabled(p);
            kv
        })
        .collect()
}

fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// A literal for a test's expected value: numbers and booleans stay typed.
fn literal(v: &str) -> String {
    let t = v.trim();
    if t.parse::<f64>().is_ok() || t == "true" || t == "false" || t == "null" { t.to_string() } else { js(v) }
}

/// JS expression for a Thunder `json.a.b[0]` query.
fn json_expr(query: &str) -> Option<String> {
    let q = query.trim();
    if q == "json" {
        return Some("pm.response.json()".into());
    }
    let rest = q.strip_prefix("json")?;
    if rest.starts_with('.') || rest.starts_with('[') { Some(format!("pm.response.json(){rest}")) } else { None }
}

fn assertion(subject: &str, action: &str, value: &str) -> Option<String> {
    let lit = literal(value);
    Some(match action {
        "equal" => format!("pm.expect({subject}).to.eql({lit});"),
        "notequal" | "notEqual" => format!("pm.expect({subject}).to.not.eql({lit});"),
        "contains" => format!("pm.expect(String({subject})).to.include({});", js(value)),
        "notcontains" | "notContains" => format!("pm.expect(String({subject})).to.not.include({});", js(value)),
        "<" => format!("pm.expect({subject}).to.be.below({lit});"),
        "<=" => format!("pm.expect({subject}).to.be.at.most({lit});"),
        ">" => format!("pm.expect({subject}).to.be.above({lit});"),
        ">=" => format!("pm.expect({subject}).to.be.at.least({lit});"),
        "count" => format!("pm.expect({subject}).to.have.lengthOf({lit});"),
        "istype" | "isType" => format!("pm.expect({subject}).to.be.a({});", js(&value.to_lowercase())),
        "isjson" | "isJson" => "pm.expect(() => pm.response.json()).to.not.throw();".to_string(),
        "exists" => format!("pm.expect({subject}).to.exist;"),
        _ => return None,
    })
}

fn tests_to_script(tests: &[Value], warnings: &mut Vec<String>) -> String {
    let mut out = Vec::new();
    for t in tests {
        let ty = str_of(t, "type");
        let custom = str_of(t, "custom");
        let action = str_of(t, "action");
        let value = str_of(t, "value");
        let (subject, label) = match ty.as_str() {
            "res-code" => ("pm.response.code".to_string(), "ResCode".to_string()),
            "res-body" => ("pm.response.text()".to_string(), "ResBody".to_string()),
            "res-time" => ("pm.response.responseTime".to_string(), "ResTime".to_string()),
            "Content-Type" => ("pm.response.headers.get(\"Content-Type\")".to_string(), "Content-Type".to_string()),
            "Content-Length" => {
                ("Number(pm.response.headers.get(\"Content-Length\"))".to_string(), "Content-Length".to_string())
            }
            "Content-Encoding" => {
                ("pm.response.headers.get(\"Content-Encoding\")".to_string(), "Content-Encoding".to_string())
            }
            "header" => (format!("pm.response.headers.get({})", js(&custom)), format!("Header {custom}")),
            "cookie" => (format!("pm.cookies.get({})", js(&custom)), format!("Cookie {custom}")),
            "json-query" => match json_expr(&custom) {
                Some(e) => (e, custom.clone()),
                None => {
                    warnings.push(format!("Thunder test on `{custom}` could not be converted"));
                    continue;
                }
            },
            "set-env-var" => {
                let var = value.trim().trim_start_matches("{{").trim_end_matches("}}").to_string();
                let expr = json_expr(&custom).unwrap_or_else(|| match custom.strip_prefix("header.") {
                    Some(h) => format!("pm.response.headers.get({})", js(h)),
                    None => js(&custom),
                });
                out.push(format!("pm.environment.set({}, {expr});", js(&var)));
                continue;
            }
            other => {
                warnings.push(format!("Thunder test type `{other}` is not supported"));
                continue;
            }
        };
        match assertion(&subject, &action, &value) {
            Some(a) => out.push(format!(
                "pm.test({}, function () {{\n    {a}\n}});",
                js(format!("{label} {action} {value}").trim())
            )),
            None => warnings.push(format!("Thunder test action `{action}` is not supported")),
        }
    }
    out.join("\n\n")
}

fn auth(a: Option<&Value>, warnings: &mut Vec<String>) -> Auth {
    let Some(a) = a.filter(|a| a.is_object()) else { return Auth::default() };
    match str_of(a, "type").as_str() {
        "" | "inherit" => Auth::default(),
        "none" => Auth::none(),
        "bearer" => {
            let prefix = str_of(a, "bearerPrefix");
            if prefix.is_empty() || prefix.eq_ignore_ascii_case("bearer") {
                Auth::bearer(str_of(a, "bearer"))
            } else {
                Auth::api_key("Authorization", format!("{prefix} {}", str_of(a, "bearer")), false)
            }
        }
        "basic" => {
            let b = a.get("basic").unwrap_or(&Value::Null);
            Auth::basic(str_of(b, "username"), str_of(b, "password"))
        }
        "ntlm" => {
            let n = a.get("ntlm").unwrap_or(&Value::Null);
            Auth::new(AuthType::Ntlm)
                .with("username", str_of(n, "username"))
                .with("password", str_of(n, "password"))
                .with("domain", str_of(n, "domain"))
                .with("workstation", str_of(n, "workstation"))
        }
        "aws" => {
            let w = a.get("aws").unwrap_or(&Value::Null);
            Auth::new(AuthType::AwsV4)
                .with("accessKey", str_of(w, "accessKeyId"))
                .with("secretKey", str_of(w, "secretAccessKey"))
                .with("sessionToken", str_of(w, "sessionToken"))
                .with("region", str_of(w, "region"))
                .with("service", str_of(w, "service"))
        }
        "oauth2" => {
            let o = a.get("oauth2").unwrap_or(&Value::Null);
            let grant = match str_of(o, "grantType").as_str() {
                "client_credentials" => "client_credentials",
                "password" => "password_credentials",
                "implicit" => "implicit",
                "authorization_code_pkce" => "authorization_code_with_pkce",
                _ => "authorization_code",
            };
            let mut out = Auth::new(AuthType::OAuth2)
                .with("grant_type", grant)
                .with("authUrl", str_of(o, "authUrl"))
                .with("accessTokenUrl", str_of(o, "tokenUrl"))
                .with("clientId", str_of(o, "clientId"))
                .with("clientSecret", str_of(o, "clientSecret"))
                .with("scope", str_of(o, "scope"))
                .with("redirect_uri", str_of(o, "callbackUrl"))
                .with("addTokenTo", "header");
            if grant == "password_credentials" {
                out.set("username", str_of(o, "username"));
                out.set("password", str_of(o, "password"));
            }
            out
        }
        "apikey" => {
            let k = a.get("apikey").unwrap_or(a);
            Auth::api_key(str_of(k, "key"), str_of(k, "value"), str_of(k, "addTo").contains("query"))
        }
        other => {
            warnings.push(format!("Thunder auth type `{other}` is not supported"));
            Auth::default()
        }
    }
}

fn body(b: Option<&Value>, req: &mut HttpRequest) {
    let Some(b) = b.filter(|b| b.is_object()) else { return };
    match str_of(b, "type").as_str() {
        "json" => req.body = Body::raw_text(str_of(b, "raw"), RawLanguage::Json),
        "xml" => req.body = Body::raw_text(str_of(b, "raw"), RawLanguage::Xml),
        "text" => req.body = Body::raw_text(str_of(b, "raw"), RawLanguage::Text),
        "formencoded" => {
            req.body.mode = BodyMode::UrlEncoded;
            req.body.urlencoded = kv(arr(b, "form"));
        }
        "formdata" => {
            req.body.mode = BodyMode::FormData;
            for f in arr(b, "form") {
                let mut field = FormField::text(str_of(f, "name"), str_of(f, "value"));
                field.enabled = !disabled(f);
                req.body.form_data.push(field);
            }
            for f in arr(b, "files") {
                let mut field = FormField::file(str_of(f, "name"), str_of(f, "value"));
                field.enabled = !disabled(f);
                req.body.form_data.push(field);
            }
        }
        "graphql" => {
            req.body.mode = BodyMode::GraphQl;
            let g = b.get("graphql").unwrap_or(&Value::Null);
            req.body.graphql.query = str_of(g, "query");
            req.body.graphql.variables = str_of(g, "variables");
        }
        "binary" => {
            req.body.mode = BodyMode::Binary;
            req.body.binary_path = str_of(b, "binary");
        }
        _ => {}
    }
}

fn request(r: &Value, base_url: Option<&str>, inherited: &[KeyValue], warnings: &mut Vec<String>) -> RequestItem {
    let mut url = str_of(r, "url");
    if base_url.is_some() && !url.contains("://") && !url.starts_with("{{") {
        url = format!("{{{{baseUrl}}}}{url}");
    }
    // Path params use `{name}` (single braces) or `:name`.
    let path_params: Vec<(String, String)> = arr(r, "params")
        .iter()
        .filter(|p| p.get("isPath").and_then(Value::as_bool).unwrap_or(false))
        .map(|p| (str_of(p, "name"), str_of(p, "value")))
        .collect();
    for (name, _) in &path_params {
        let single = format!("{{{name}}}");
        let double = format!("{{{{{name}}}}}");
        if url.contains(&single) && !url.contains(&double) {
            url = url.replace(&single, &format!(":{name}"));
        }
    }
    let method = str_of(r, "method");
    let mut req = HttpRequest::new(if method.is_empty() { "GET".to_string() } else { method.to_uppercase() }, url);
    let query: Vec<Value> = arr(r, "params")
        .iter()
        .filter(|p| !p.get("isPath").and_then(Value::as_bool).unwrap_or(false))
        .cloned()
        .collect();
    let query = kv(&query);
    // Thunder stores query params both in the URL and the list; keep the list.
    if !query.is_empty() {
        req.params = query;
        req.sync_url_from_params();
    }
    for (name, value) in path_params {
        if let Some(pv) = req.path_vars.iter_mut().find(|v| v.key == name) {
            pv.value = value;
        }
    }
    let own = kv(arr(r, "headers"));
    for h in inherited {
        if !own.iter().any(|o| o.key.eq_ignore_ascii_case(&h.key)) {
            req.headers.push(h.clone());
        }
    }
    req.headers.extend(own);
    body(r.get("body"), &mut req);
    req.auth = auth(r.get("auth"), warnings);
    let mut item = RequestItem::new(str_of(r, "name"), req);
    item.description = str_of(r, "docs");
    let post = tests_to_script(arr(r, "tests"), warnings);
    let inline = |key: &str| -> String {
        r.get(key)
            .map(|p| {
                arr(p, "inlineScripts")
                    .iter()
                    .map(|s| match s.get("script") {
                        Some(Value::Array(lines)) => {
                            lines.iter().map(crate::util::as_string).collect::<Vec<_>>().join("\n")
                        }
                        Some(other) => crate::util::as_string(other),
                        None => String::new(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    };
    let pre = inline("preReq");
    let post_inline = inline("postReq");
    if pre.contains("tc.") || post_inline.contains("tc.") {
        let msg = "Thunder `tc.` scripts were imported as-is and need porting to the pm API".to_string();
        if !warnings.contains(&msg) {
            warnings.push(msg);
        }
    }
    let post = [post, post_inline].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
    item.scripts = Scripts { pre_request: pre, post_response: post, ..Default::default() };
    item
}

fn sort_num(v: &Value) -> f64 {
    v.get("sortNum").and_then(Value::as_f64).unwrap_or(0.0)
}

fn build(
    name: &str,
    settings: Option<&Value>,
    folders: &[&Value],
    requests: &[&Value],
    warnings: &mut Vec<String>,
) -> Collection {
    let mut c = Collection::new(if name.is_empty() { "Thunder Client import" } else { name });
    let base_url =
        settings.and_then(|s| s.pointer("/options/baseUrl")).and_then(Value::as_str).filter(|s| !s.is_empty());
    if let Some(b) = base_url {
        c.variables.push(Variable::new("baseUrl", b));
    }
    let inherited = settings.map(|s| kv(arr(s, "headers"))).unwrap_or_default();
    if let Some(s) = settings {
        c.auth = auth(s.get("auth"), warnings);
        c.scripts.post_response = tests_to_script(arr(s, "tests"), warnings);
    }

    let mut by_container: HashMap<String, Vec<&Value>> = HashMap::new();
    for f in folders {
        by_container.entry(str_of(f, "containerId")).or_default().push(f);
    }
    for r in requests {
        by_container.entry(str_of(r, "containerId")).or_default().push(r);
    }
    for list in by_container.values_mut() {
        list.sort_by(|a, b| sort_num(a).partial_cmp(&sort_num(b)).unwrap_or(std::cmp::Ordering::Equal));
    }
    /// `open` holds the ids of the containers being walked: a folder whose
    /// `_id` is missing or repeats an enclosing one would recurse forever.
    fn walk(
        container: &str,
        map: &HashMap<String, Vec<&Value>>,
        base_url: Option<&str>,
        inherited: &[KeyValue],
        open: &mut Vec<String>,
        warnings: &mut Vec<String>,
    ) -> Vec<Item> {
        let mut items = Vec::new();
        open.push(container.to_string());
        for v in map.get(container).map(Vec::as_slice).unwrap_or(&[]) {
            if v.get("url").is_some() || v.get("method").is_some() {
                items.push(Item::Request(request(v, base_url, inherited, warnings)));
            } else {
                let mut f = Folder::new(str_of(v, "name"));
                if let Some(s) = v.get("settings") {
                    f.auth = auth(s.get("auth"), warnings);
                    f.scripts.post_response = tests_to_script(arr(s, "tests"), warnings);
                }
                let mut headers = inherited.to_vec();
                if let Some(s) = v.get("settings") {
                    for h in kv(arr(s, "headers")) {
                        headers.retain(|x| !x.key.eq_ignore_ascii_case(&h.key));
                        headers.push(h);
                    }
                }
                let id = str_of(v, "_id");
                if open.contains(&id) {
                    warnings.push(format!("Thunder folder `{}` contains itself; its contents were skipped", f.name));
                } else {
                    f.items = walk(&id, map, base_url, &headers, open, warnings);
                }
                items.push(Item::Folder(f));
            }
        }
        open.pop();
        items
    }
    c.items = walk("", &by_container, base_url, &inherited, &mut Vec::new(), warnings);
    c
}

pub fn import(text: &str) -> Result<(Vec<Collection>, Vec<String>), ConvertError> {
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
    if v.get("requests").is_none() {
        return Err(ConvertError::Format("not a Thunder Client collection".into()));
    }
    let mut warnings = Vec::new();
    let folders: Vec<&Value> = arr(&v, "folders").iter().collect();
    let requests: Vec<&Value> = arr(&v, "requests").iter().collect();
    let cols = match v.get("collections").and_then(Value::as_array) {
        // Workspace export: several collections linked by `colId`.
        Some(list) if !list.is_empty() => list
            .iter()
            .map(|col| {
                let id = str_of(col, "_id");
                let fs: Vec<&Value> = folders.iter().copied().filter(|f| str_of(f, "colId") == id).collect();
                let rs: Vec<&Value> = requests.iter().copied().filter(|r| str_of(r, "colId") == id).collect();
                build(&str_of(col, "colName"), col.get("settings"), &fs, &rs, &mut warnings)
            })
            .collect(),
        _ => vec![build(&str_of(&v, "collectionName"), v.get("settings"), &folders, &requests, &mut warnings)],
    };
    Ok((cols, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_collection_and_converts_tests() {
        let (cols, warnings) = import(include_str!("../testdata/thunder.json")).unwrap();
        let c = &cols[0];
        assert_eq!(c.name, "Thunder API");
        assert_eq!(c.auth.kind, AuthType::Bearer);
        let folder = c.items[0].as_folder().unwrap();
        assert_eq!(folder.name, "Users");
        let get = folder.items[0].as_request().unwrap();
        let h = get.http().unwrap();
        assert_eq!(h.url, "{{baseUrl}}/users/:id?expand=true");
        assert_eq!(h.path_vars[0].value, "7");
        assert!(h.headers.iter().any(|x| x.key == "X-Off" && !x.enabled));
        assert_eq!(h.header("x-client"), Some("thunder"));
        let s = &get.scripts.post_response;
        assert!(s.contains("pm.expect(pm.response.code).to.eql(200);"), "{s}");
        assert!(s.contains("pm.expect(pm.response.json().data.id).to.eql(7);"), "{s}");
        assert!(s.contains("pm.expect(pm.response.responseTime).to.be.below(500);"), "{s}");
        assert!(s.contains("pm.environment.set(\"userName\", pm.response.json().data.name);"), "{s}");
        assert!(s.contains("pm.response.headers.get(\"Content-Type\")"), "{s}");

        let post = c.items[1].as_request().unwrap().http().unwrap();
        assert_eq!(post.body.mode, BodyMode::Raw);
        assert_eq!(post.auth.kind, AuthType::Basic);
        let form = c.items[2].as_request().unwrap().http().unwrap();
        assert_eq!(form.body.mode, BodyMode::FormData);
        assert_eq!(form.body.form_data.len(), 2);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn folder_cycles_do_not_overflow_the_stack() {
        // A folder without `_id` lives in (and would contain) the root; two
        // folders may also contain each other.
        for text in [
            r#"{"collectionName": "c", "folders": [{"name": "f"}], "requests": []}"#,
            r#"{"collectionName": "c", "folders": [null], "requests": []}"#,
            r#"{"collectionName": "c", "requests": [{"name": "r", "url": "http://h", "containerId": "b"}],
                "folders": [{"_id": "a", "name": "A"}, {"_id": "b", "containerId": "a", "name": "B"},
                            {"_id": "a", "containerId": "b", "name": "A again"}]}"#,
        ] {
            let (cols, _) = import(text).unwrap();
            assert!(cols[0].items.len() <= 2, "{:?}", cols[0].items);
        }
        let (cols, _) = import(
            r#"{"collectionName": "c", "requests": [{"name": "r", "url": "http://h", "containerId": "b"}],
                "folders": [{"_id": "a", "name": "A"}, {"_id": "b", "containerId": "a", "name": "B"},
                            {"_id": "a", "containerId": "b", "name": "A again"}]}"#,
        )
        .unwrap();
        assert_eq!(cols[0].request_count(), 1);
    }
}
