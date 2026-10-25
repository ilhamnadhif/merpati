//! HAR 1.2 (HTTP Archive) import: one request per entry, responses become
//! examples.

use base64::Engine;
use merpati_core::model::{Body, BodyMode, Collection, Example, FormField, HttpRequest, KeyValue, RequestItem};
use serde_json::Value;

use crate::ConvertError;
use crate::util::{arr, as_string, language_for_content_type, raw_language_for, request_name_from_url, str_of};

/// Headers that are derived by the client and should not be replayed.
const SKIP_HEADERS: [&str; 4] = ["content-length", "host", "connection", "accept-encoding"];

pub fn import(text: &str) -> Result<(Collection, Vec<String>), ConvertError> {
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
    let log = v.get("log").ok_or_else(|| ConvertError::Format("not a HAR file (missing `log`)".into()))?;
    let mut warnings = Vec::new();
    let creator = log.pointer("/creator/name").and_then(Value::as_str).unwrap_or("HAR");
    let mut c = Collection::new(format!("{creator} import"));
    let mut skipped_binary = 0;
    for entry in arr(log, "entries") {
        let Some(req_v) = entry.get("request") else { continue };
        let method = str_of(req_v, "method");
        let url = str_of(req_v, "url");
        if url.is_empty() {
            warnings.push("skipped an entry without URL".into());
            continue;
        }
        let mut req =
            HttpRequest::new(if method.is_empty() { "GET".to_string() } else { method.to_uppercase() }, url.clone());
        for h in arr(req_v, "headers") {
            let name = str_of(h, "name");
            let lower = name.to_ascii_lowercase();
            if name.starts_with(':') || SKIP_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            req.headers.push(KeyValue::new(name, str_of(h, "value")));
        }
        if let Some(post) = req_v.get("postData") {
            let mime = str_of(post, "mimeType");
            let lower = mime.to_ascii_lowercase();
            let params = arr(post, "params");
            if lower.contains("multipart/form-data") && !params.is_empty() {
                req.body.mode = BodyMode::FormData;
                for p in params {
                    let name = str_of(p, "name");
                    match p.get("fileName").and_then(Value::as_str) {
                        Some(file) => {
                            let mut f = FormField::file(name, file);
                            f.content_type = p.get("contentType").and_then(Value::as_str).map(str::to_string);
                            req.body.form_data.push(f);
                        }
                        None => req.body.form_data.push(FormField::text(name, str_of(p, "value"))),
                    }
                }
                // The boundary in the recorded header would not match ours.
                req.headers.retain(|h| !h.key.eq_ignore_ascii_case("content-type"));
            } else if lower.contains("x-www-form-urlencoded") && !params.is_empty() {
                req.body.mode = BodyMode::UrlEncoded;
                req.body.urlencoded = urlencoded_params(params, post.get("text").and_then(Value::as_str));
            } else if let Some(text) = post.get("text").and_then(Value::as_str) {
                if lower.contains("x-www-form-urlencoded") {
                    req.body.mode = BodyMode::UrlEncoded;
                    req.body.urlencoded = url::form_urlencoded::parse(text.as_bytes())
                        .map(|(k, v)| KeyValue::new(k.into_owned(), v.into_owned()))
                        .collect();
                } else if lower.contains("multipart/form-data") {
                    // Recorded multipart text without params: keep it raw with its boundary.
                    req.body = Body::raw_text(text, raw_language_for(&lower));
                } else {
                    req.body = Body::raw_text(text, raw_language_for(&lower));
                }
            }
            if req.body.mode == BodyMode::Raw
                && !mime.is_empty()
                && !req.headers.iter().any(|h| h.key.eq_ignore_ascii_case("content-type"))
            {
                req.headers.push(KeyValue::new("Content-Type", mime));
            }
        }
        let name = request_name_from_url(&req.method, &url);
        let mut item = RequestItem::new(name, req.clone());
        if let Some(resp) = entry.get("response") {
            let raw_status = resp.get("status").and_then(Value::as_u64).unwrap_or(0);
            let status = u16::try_from(raw_status).unwrap_or_else(|_| {
                warnings.push(format!(
                    "{} {url}: response status {raw_status} is out of range; the example uses {}",
                    req.method,
                    u16::MAX
                ));
                u16::MAX
            });
            if status > 0 {
                let content = resp.get("content").cloned().unwrap_or(Value::Null);
                let mime = str_of(&content, "mimeType");
                let mut body = str_of(&content, "text");
                if content.get("encoding").and_then(Value::as_str) == Some("base64") {
                    match base64::engine::general_purpose::STANDARD.decode(body.trim()) {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(s) => body = s,
                            Err(_) => {
                                skipped_binary += 1;
                                body = String::new();
                            }
                        },
                        Err(_) => body = String::new(),
                    }
                }
                let headers: Vec<KeyValue> = arr(resp, "headers")
                    .iter()
                    .filter(|h| !str_of(h, "name").starts_with(':'))
                    .map(|h| KeyValue::new(str_of(h, "name"), str_of(h, "value")))
                    .collect();
                let mut original = req.clone();
                original.settings = Default::default();
                item.examples.push(Example {
                    id: merpati_core::util::new_id(),
                    name: format!("{status} {}", as_string(resp.get("statusText").unwrap_or(&Value::Null)))
                        .trim()
                        .to_string(),
                    request: Some(original),
                    status,
                    status_text: str_of(resp, "statusText"),
                    headers,
                    body,
                    language: language_for_content_type(&mime),
                });
            }
        }
        c.items.push(item.into());
    }
    if skipped_binary > 0 {
        warnings.push(format!("{skipped_binary} binary response bodies were not imported as examples"));
    }
    if c.items.is_empty() {
        warnings.push("the HAR file has no entries".into());
    }
    Ok((c, warnings))
}

/// The rows of an urlencoded `postData.params`. Chrome records names and
/// values as sent (`a%20b`, `a+b`), Firefox records them decoded (`a b`), so
/// decoding unconditionally would decode Firefox's values twice (`100%25`
/// becomes `100%`, `1+1` becomes `1 1`). The heuristic:
///
/// * with a `postData.text`, the params are taken as recorded when they equal
///   the pairs decoded from the text, decoded when that makes them equal,
///   and when neither matches the text (the bytes that were sent) wins;
/// * without a text, they are decoded unless some name or value cannot be
///   wire form: a space, a control or non-ASCII character, or a `%` that does
///   not start an escape.
fn urlencoded_params(params: &[Value], text: Option<&str>) -> Vec<KeyValue> {
    let decode = |s: &str| -> String {
        url::form_urlencoded::parse(format!("x={s}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
    };
    let raw: Vec<(String, String)> = params.iter().map(|p| (str_of(p, "name"), str_of(p, "value"))).collect();
    let decoded = || raw.iter().map(|(k, v)| (decode(k), decode(v))).collect::<Vec<_>>();
    let rows = match text.filter(|t| !t.is_empty()) {
        Some(text) => {
            let sent: Vec<(String, String)> =
                url::form_urlencoded::parse(text.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
            if raw == sent {
                raw
            } else {
                let dec = decoded();
                if dec == sent { dec } else { sent }
            }
        }
        None => {
            let not_wire_form = |s: &str| {
                let b = s.as_bytes();
                b.iter().enumerate().any(|(i, &c)| {
                    c == b' '
                        || !c.is_ascii()
                        || c.is_ascii_control()
                        || (c == b'%'
                            && !(b.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                                && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)))
                })
            };
            if raw.iter().any(|(k, v)| not_wire_form(k) || not_wire_form(v)) { raw } else { decoded() }
        }
    };
    rows.into_iter().map(|(k, v)| KeyValue::new(k, v)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::model::RawLanguage;

    #[test]
    fn imports_har() {
        let (c, warnings) = import(include_str!("../testdata/sample.har")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let reqs = c.requests();
        assert_eq!(reqs.len(), 3);
        let get = reqs[0].http().unwrap();
        assert_eq!(get.method, "GET");
        assert_eq!(get.url, "https://api.example.com/users?page=2");
        assert!(get.headers.iter().all(|h| !h.key.starts_with(':') && h.key != "Host"));
        assert_eq!(reqs[0].examples[0].status, 200);
        assert_eq!(reqs[0].examples[0].body, "{\"users\":[]}");
        assert_eq!(reqs[0].examples[0].language.as_deref(), Some("json"));

        let post = reqs[1].http().unwrap();
        assert_eq!(post.body.raw_language, RawLanguage::Json);
        assert_eq!(post.body.raw, "{\"name\":\"Ada\"}");
        assert_eq!(reqs[1].examples[0].body, "created");

        let form = reqs[2].http().unwrap();
        assert_eq!(form.body.mode, BodyMode::UrlEncoded);
        assert_eq!(form.body.urlencoded[0].value, "a b");
    }

    fn form_entry(params: Value, text: Option<&str>, status: Value) -> String {
        let mut post = serde_json::json!({"mimeType": "application/x-www-form-urlencoded", "params": params});
        if let Some(t) = text {
            post["text"] = Value::from(t);
        }
        serde_json::json!({"log": {"entries": [{
            "request": {"method": "POST", "url": "https://h.test/f", "headers": [], "postData": post},
            "response": {"status": status, "statusText": "", "headers": [], "content": {"text": "ok"}}
        }]}})
        .to_string()
    }

    fn rows(har: &str) -> Vec<(String, String)> {
        let (c, _) = import(har).unwrap();
        c.requests()[0].http().unwrap().body.urlencoded.iter().map(|kv| (kv.key.clone(), kv.value.clone())).collect()
    }

    fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn urlencoded_params_are_decoded_once() {
        let expected = pairs(&[("q", "100% sure"), ("sum", "1+1"), ("name", "a b")]);
        let text = "q=100%25+sure&sum=1%2B1&name=a+b";
        // Chrome: params as sent.
        let chrome = serde_json::json!([
            {"name": "q", "value": "100%25+sure"}, {"name": "sum", "value": "1%2B1"}, {"name": "name", "value": "a+b"}
        ]);
        // Firefox: params already decoded.
        let firefox = serde_json::json!([
            {"name": "q", "value": "100% sure"}, {"name": "sum", "value": "1+1"}, {"name": "name", "value": "a b"}
        ]);
        assert_eq!(rows(&form_entry(chrome.clone(), Some(text), 200.into())), expected);
        assert_eq!(rows(&form_entry(firefox.clone(), Some(text), 200.into())), expected);
        // Without the text: Chrome's are wire form, Firefox's (a space, a bare `%`) are not.
        assert_eq!(rows(&form_entry(chrome, None, 200.into())), expected);
        assert_eq!(rows(&form_entry(firefox, None, 200.into())), expected);
        // Params that contradict the text: the text is what was sent.
        let stale = serde_json::json!([{"name": "q", "value": "old"}]);
        assert_eq!(rows(&form_entry(stale, Some("q=new"), 200.into())), pairs(&[("q", "new")]));
    }

    #[test]
    fn status_out_of_range_is_clamped_with_warning() {
        let har = form_entry(serde_json::json!([{"name": "a", "value": "1"}]), None, 70000.into());
        let (c, warnings) = import(&har).unwrap();
        assert_eq!(c.requests()[0].examples[0].status, u16::MAX);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("70000"), "{warnings:?}");
        let (c, warnings) = import(&form_entry(serde_json::json!([]), None, 201.into())).unwrap();
        assert_eq!(c.requests()[0].examples[0].status, 201);
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
