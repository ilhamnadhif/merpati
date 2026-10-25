//! Hoppscotch collection import (exported JSON: one collection object or an
//! array of them). `<<var>>` becomes `{{var}}`; common `pw.*` script calls are
//! rewritten to the `pm.*` API.

use merpati_core::model::{
    Body, BodyMode, Collection, Folder, FormField, FormFieldKind, HttpRequest, Item, KeyValue, RequestItem, Scripts,
};
use merpati_core::{Auth, AuthType};
use serde_json::Value;

use crate::ConvertError;
use crate::util::{arr, as_string, raw_language_for, str_of};

fn vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<<") {
        match rest[start + 2..].find(">>") {
            Some(end) => {
                let name = &rest[start + 2..start + 2 + end];
                if !name.is_empty() && !name.contains(char::is_whitespace) {
                    out.push_str(&rest[..start]);
                    out.push_str(&format!("{{{{{name}}}}}"));
                } else {
                    out.push_str(&rest[..start + 2 + end + 2]);
                }
                rest = &rest[start + 2 + end + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

fn active(v: &Value) -> bool {
    v.get("active").and_then(Value::as_bool).unwrap_or(true)
}

fn kv(list: &[Value]) -> Vec<KeyValue> {
    list.iter()
        .filter(|p| !str_of(p, "key").is_empty())
        .map(|p| KeyValue {
            key: vars(&str_of(p, "key")),
            value: vars(&str_of(p, "value")),
            enabled: active(p),
            description: str_of(p, "description"),
        })
        .collect()
}

fn script(code: &str, warnings: &mut Vec<String>) -> String {
    if code.trim().is_empty() {
        return String::new();
    }
    let replacements = [
        ("pw.env.getResolve(", "pm.environment.get("),
        ("pw.env.resolve(", "pm.variables.replaceIn("),
        ("pw.env.set(", "pm.environment.set("),
        ("pw.env.get(", "pm.environment.get("),
        ("pw.env.unset(", "pm.environment.unset("),
        ("pw.test(", "pm.test("),
        ("pw.expect(", "pm.expect("),
        ("pw.response.status", "pm.response.code"),
        ("pw.response.body", "pm.response.json()"),
        ("pw.response.headers", "pm.response.headers"),
        ("hopp.env.set(", "pm.environment.set("),
        ("hopp.env.get(", "pm.environment.get("),
        ("hopp.test(", "pm.test("),
        ("hopp.expect(", "pm.expect("),
    ];
    let mut out = code.to_string();
    for (from, to) in replacements {
        out = out.replace(from, to);
    }
    if out.contains("pw.") || out.contains("hopp.") || out.contains(".toBe") {
        let msg = "Hoppscotch scripts were partly translated to the pm API; matchers such as toBe() need manual review"
            .to_string();
        if !warnings.contains(&msg) {
            warnings.push(msg);
        }
    }
    out
}

fn auth(a: Option<&Value>, warnings: &mut Vec<String>) -> Auth {
    let Some(a) = a.filter(|a| a.is_object()) else { return Auth::default() };
    if !a.get("authActive").and_then(Value::as_bool).unwrap_or(true) {
        return Auth::none();
    }
    let s = |k: &str| vars(&str_of(a, k));
    match a.get("authType").and_then(Value::as_str).unwrap_or("inherit") {
        "none" => Auth::none(),
        "inherit" => Auth::default(),
        "basic" => Auth::basic(s("username"), s("password")),
        "bearer" => Auth::bearer(s("token")),
        "api-key" => {
            let to = str_of(a, "addTo").to_ascii_lowercase();
            Auth::api_key(s("key"), s("value"), to.contains("query"))
        }
        "digest" => Auth::new(AuthType::Digest).with("username", s("username")).with("password", s("password")),
        "aws-signature" => Auth::new(AuthType::AwsV4)
            .with("accessKey", s("accessKey"))
            .with("secretKey", s("secretKey"))
            .with("region", s("region"))
            .with("service", s("serviceName"))
            .with("sessionToken", s("serviceToken")),
        "oauth-2" => {
            let info = a.get("grantTypeInfo").unwrap_or(a);
            let g = |k: &str| vars(&str_of(info, k));
            let pkce = info.get("isPKCE").and_then(Value::as_bool).unwrap_or(false);
            let grant = match str_of(info, "grantType").to_ascii_uppercase().as_str() {
                "CLIENT_CREDENTIALS" => "client_credentials",
                "PASSWORD" => "password_credentials",
                "IMPLICIT" => "implicit",
                _ if pkce => "authorization_code_with_pkce",
                _ => "authorization_code",
            };
            let mut out = Auth::new(AuthType::OAuth2)
                .with("grant_type", grant)
                .with("authUrl", if g("authEndpoint").is_empty() { s("authURL") } else { g("authEndpoint") })
                .with(
                    "accessTokenUrl",
                    if g("tokenEndpoint").is_empty() { s("accessTokenURL") } else { g("tokenEndpoint") },
                )
                .with("clientId", if g("clientID").is_empty() { s("clientID") } else { g("clientID") })
                .with("clientSecret", g("clientSecret"))
                .with("scope", if g("scopes").is_empty() { s("scope") } else { g("scopes") })
                .with("addTokenTo", "header");
            if grant == "password_credentials" {
                out.set("username", g("username"));
                out.set("password", g("password"));
            }
            let token = if g("token").is_empty() { s("token") } else { g("token") };
            if !token.is_empty() {
                out.set("accessToken", token);
            }
            out
        }
        other => {
            warnings.push(format!("Hoppscotch auth type `{other}` is not supported"));
            Auth::default()
        }
    }
}

fn body(b: Option<&Value>, req: &mut HttpRequest) {
    let Some(b) = b.filter(|b| b.is_object()) else { return };
    let ct = match b.get("contentType") {
        Some(Value::String(s)) => s.clone(),
        _ => return,
    };
    let lower = ct.to_ascii_lowercase();
    match b.get("body") {
        Some(Value::Array(parts)) if lower.contains("multipart") => {
            req.body.mode = BodyMode::FormData;
            for p in parts {
                let key = vars(&str_of(p, "key"));
                let mut f = if p.get("isFile").and_then(Value::as_bool).unwrap_or(false) {
                    // Exported files are just names; the user re-selects them.
                    FormField { key, kind: FormFieldKind::File, enabled: true, ..Default::default() }
                } else {
                    FormField::text(key, vars(&str_of(p, "value")))
                };
                f.enabled = active(p);
                req.body.form_data.push(f);
            }
        }
        Some(Value::String(text)) if lower.contains("x-www-form-urlencoded") => {
            req.body.mode = BodyMode::UrlEncoded;
            for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let (disabled, line) = match line.strip_prefix('#') {
                    Some(rest) => (true, rest.trim()),
                    None => (false, line),
                };
                let (k, v) = line.split_once(':').unwrap_or((line, ""));
                let mut kv = KeyValue::new(vars(k.trim()), vars(v.trim()));
                kv.enabled = !disabled;
                req.body.urlencoded.push(kv);
            }
            if !req.headers.iter().any(|h| h.key.eq_ignore_ascii_case("content-type")) {
                req.headers.push(KeyValue::new("Content-Type", ct));
            }
        }
        Some(Value::String(_)) if lower.contains("octet-stream") => req.body.mode = BodyMode::Binary,
        Some(Value::String(text)) => {
            req.body = Body::raw_text(vars(text), raw_language_for(&lower));
            // Only add the header when the language default would differ.
            let has_ct = req.headers.iter().any(|h| h.key.eq_ignore_ascii_case("content-type"));
            if req.body.raw_language.content_type() != lower && !has_ct {
                req.headers.push(KeyValue::new("Content-Type", ct));
            }
        }
        _ => {}
    }
}

fn request(r: &Value, inherited_headers: &[KeyValue], warnings: &mut Vec<String>) -> RequestItem {
    let mut req = HttpRequest::new(
        {
            let m = str_of(r, "method");
            if m.is_empty() { "GET".to_string() } else { m.to_uppercase() }
        },
        vars(&str_of(r, "endpoint")),
    );
    let params = kv(arr(r, "params"));
    if !params.is_empty() {
        req.params.extend(params);
        req.sync_url_from_params();
    }
    let own = kv(arr(r, "headers"));
    for h in inherited_headers {
        if !own.iter().any(|o| o.key.eq_ignore_ascii_case(&h.key)) {
            req.headers.push(h.clone());
        }
    }
    req.headers.extend(own);
    body(r.get("body"), &mut req);
    req.auth = auth(r.get("auth"), warnings);
    let mut item = RequestItem::new(str_of(r, "name"), req);
    item.description = str_of(r, "description");
    let mut pre = script(&str_of(r, "preRequestScript"), warnings);
    // Request variables are request-local: set them before anything runs.
    let req_vars: Vec<String> = arr(r, "requestVariables")
        .iter()
        .filter(|v| active(v) && !str_of(v, "key").is_empty())
        .map(|v| {
            format!(
                "pm.variables.set({}, {});",
                serde_json::to_string(&str_of(v, "key")).unwrap_or_default(),
                serde_json::to_string(&vars(&str_of(v, "value"))).unwrap_or_default()
            )
        })
        .collect();
    if !req_vars.is_empty() {
        pre = if pre.is_empty() { req_vars.join("\n") } else { format!("{}\n{pre}", req_vars.join("\n")) };
    }
    item.scripts =
        Scripts { pre_request: pre, post_response: script(&str_of(r, "testScript"), warnings), ..Default::default() };
    item
}

fn folder_items(v: &Value, headers: &[KeyValue], warnings: &mut Vec<String>) -> Vec<Item> {
    let mut merged = headers.to_vec();
    for h in kv(arr(v, "headers")) {
        merged.retain(|m| !m.key.eq_ignore_ascii_case(&h.key));
        merged.push(h);
    }
    let mut items = Vec::new();
    for f in arr(v, "folders") {
        let mut folder = Folder::new(str_of(f, "name"));
        folder.auth = auth(f.get("auth"), warnings);
        folder.description = str_of(f, "description");
        folder.items = folder_items(f, &merged, warnings);
        items.push(Item::Folder(folder));
    }
    for r in arr(v, "requests") {
        items.push(Item::Request(request(r, &merged, warnings)));
    }
    items
}

fn collection(v: &Value, warnings: &mut Vec<String>) -> Collection {
    let mut c = Collection::new(str_of(v, "name"));
    if c.name.is_empty() {
        c.name = "Hoppscotch import".into();
    }
    c.description = str_of(v, "description");
    c.auth = auth(v.get("auth"), warnings);
    c.items = folder_items(v, &[], warnings);
    for var in arr(v, "variables") {
        let key = str_of(var, "key");
        if !key.is_empty() {
            let value = vars(&as_string(var.get("initialValue").or_else(|| var.get("value")).unwrap_or(&Value::Null)));
            let mut variable = merpati_core::variables::Variable::new(key, value);
            if var.get("secret").and_then(Value::as_bool).unwrap_or(false) {
                variable.kind = merpati_core::VarKind::Secret;
            }
            c.variables.push(variable);
        }
    }
    c
}

pub fn import(text: &str) -> Result<(Vec<Collection>, Vec<String>), ConvertError> {
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
    let mut warnings = Vec::new();
    let cols = match &v {
        Value::Array(list) => list.iter().map(|c| collection(c, &mut warnings)).collect(),
        Value::Object(_) if v.get("requests").is_some() => vec![collection(&v, &mut warnings)],
        _ => return Err(ConvertError::Format("not a Hoppscotch collection".into())),
    };
    Ok((cols, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::model::RawLanguage;

    #[test]
    fn variable_syntax() {
        assert_eq!(vars("<<baseUrl>>/u/<<id>>?a=<<b"), "{{baseUrl}}/u/{{id}}?a=<<b");
        assert_eq!(vars("a << b >> c"), "a << b >> c");
    }

    #[test]
    fn imports_collection() {
        let (cols, warnings) = import(include_str!("../testdata/hoppscotch.json")).unwrap();
        assert_eq!(cols.len(), 1);
        let c = &cols[0];
        assert_eq!(c.name, "Hopp API");
        assert_eq!(c.auth.kind, AuthType::Bearer);
        assert_eq!(c.auth.get("token"), "{{token}}");
        let folder = c.items[0].as_folder().unwrap();
        assert_eq!(folder.name, "Users");
        let list = folder.items[0].as_request().unwrap();
        let h = list.http().unwrap();
        assert_eq!(h.url, "{{baseUrl}}/users?page=1");
        assert!(!h.params[1].enabled);
        assert_eq!(h.header("x-team"), Some("core"), "collection headers are inherited");
        assert!(h.auth.is_inherit());
        assert!(
            list.scripts.post_response.contains("pm.test(") && list.scripts.post_response.contains("pm.response.code")
        );
        assert!(list.scripts.pre_request.contains("pm.variables.set(\"limit\", \"10\")"));

        let create = c.items[1].as_request().unwrap().http().unwrap();
        assert_eq!(create.body.raw_language, RawLanguage::Json);
        assert_eq!(create.auth.kind, AuthType::Basic);
        let form = c.items[2].as_request().unwrap().http().unwrap();
        assert_eq!(form.body.mode, BodyMode::UrlEncoded);
        assert_eq!(form.body.urlencoded.len(), 2);
        assert!(!form.body.urlencoded[1].enabled);
        let multi = c.items[3].as_request().unwrap().http().unwrap();
        assert_eq!(multi.body.mode, BodyMode::FormData);
        assert_eq!(multi.body.form_data[1].kind, FormFieldKind::File);
        assert!(!warnings.is_empty(), "toBe matcher should be reported");
    }
}
