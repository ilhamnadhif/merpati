//! Helpers shared by the importers.

use serde_json::{Map, Number, Value};

use crate::ConvertError;

/// Parses YAML into a JSON value. Non-string mapping keys (YAML allows `200:`)
/// are stringified, which is what OpenAPI documents need.
pub fn yaml_to_json(text: &str) -> Result<Value, ConvertError> {
    let y: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).map_err(|e| ConvertError::Yaml(e.to_string()))?;
    Ok(convert_yaml(y))
}

fn convert_yaml(y: serde_yaml_ng::Value) -> Value {
    use serde_yaml_ng::Value as Y;
    match y {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Number(u.into())
            } else {
                n.as_f64().and_then(Number::from_f64).map(Value::Number).unwrap_or(Value::Null)
            }
        }
        Y::String(s) => Value::String(s),
        Y::Sequence(seq) => Value::Array(seq.into_iter().map(convert_yaml).collect()),
        Y::Mapping(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                let key = match k {
                    Y::String(s) => s,
                    Y::Number(n) => n.to_string(),
                    Y::Bool(b) => b.to_string(),
                    Y::Null => "null".into(),
                    other => serde_yaml_ng::to_string(&other).unwrap_or_default().trim().to_string(),
                };
                out.insert(key, convert_yaml(v));
            }
            Value::Object(out)
        }
        Y::Tagged(t) => convert_yaml(t.value),
    }
}

/// A JSON scalar as text (`null` becomes empty).
pub fn as_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

pub fn str_of(v: &Value, key: &str) -> String {
    v.get(key).map(as_string).unwrap_or_default()
}

pub fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// `GET /users/{id}`-style default request name.
pub fn request_name_from_url(method: &str, url: &str) -> String {
    let (base, _, _) = merpati_core::urlutil::split(url);
    let after_scheme = base.find("://").map(|i| &base[i + 3..]).unwrap_or(base);
    let path = after_scheme.find('/').map(|i| &after_scheme[i..]).unwrap_or("/");
    format!("{} {}", method.to_uppercase(), if path.is_empty() { "/" } else { path })
}

/// Pretty JSON text for an example value.
pub fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Reason phrase for common status codes.
pub fn status_text(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        418 => "I'm a teapot",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// Language hint for an example body from its content type.
pub fn language_for_content_type(ct: &str) -> Option<String> {
    let ct = ct.to_ascii_lowercase();
    let lang = if ct.contains("json") {
        "json"
    } else if ct.contains("html") {
        "html"
    } else if ct.contains("xml") {
        "xml"
    } else if ct.contains("javascript") {
        "javascript"
    } else if ct.starts_with("text/") {
        "text"
    } else {
        return None;
    };
    Some(lang.to_string())
}

/// Raw body language for a content type.
pub fn raw_language_for(ct: &str) -> merpati_core::RawLanguage {
    use merpati_core::RawLanguage;
    let ct = ct.to_ascii_lowercase();
    if ct.contains("json") {
        RawLanguage::Json
    } else if ct.contains("html") {
        RawLanguage::Html
    } else if ct.contains("xml") {
        RawLanguage::Xml
    } else if ct.contains("javascript") {
        RawLanguage::JavaScript
    } else {
        RawLanguage::Text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_numeric_keys() {
        let v = yaml_to_json("responses:\n  200:\n    description: ok\n  default:\n    description: d").unwrap();
        assert_eq!(v["responses"]["200"]["description"], "ok");
    }

    #[test]
    fn names() {
        assert_eq!(request_name_from_url("get", "https://a.b/users/1?x=1"), "GET /users/1");
        assert_eq!(request_name_from_url("GET", "{{base}}/x"), "GET /x");
        assert_eq!(request_name_from_url("GET", "https://a.b"), "GET /");
    }
}
