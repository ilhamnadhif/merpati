//! Reference syntax shared by the providers: `path?query#field`.

use serde_json::Value;

use crate::error::SecretError;

/// A parsed `path?key=value#field` reference.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reference {
    pub path: String,
    pub query: Vec<(String, String)>,
    pub field: Option<String>,
}

impl Reference {
    pub fn parse(input: &str) -> Result<Reference, SecretError> {
        let input = input.trim();
        let (rest, field) = match input.rsplit_once('#') {
            Some((r, f)) => (r, Some(f.trim().to_string()).filter(|f| !f.is_empty())),
            None => (input, None),
        };
        let (path, query) = match rest.split_once('?') {
            Some((p, q)) => (p, q),
            None => (rest, ""),
        };
        let path = path.trim().trim_matches('/').to_string();
        if path.is_empty() {
            return Err(SecretError::reference(format!("Reference \"{input}\" has no secret name or path.")));
        }
        let query =
            url::form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        Ok(Reference { path, query, field })
    }

    pub fn param(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }

    /// Fails on query parameters other than `allowed`.
    pub fn only_params(&self, allowed: &[&str], provider: &str) -> Result<(), SecretError> {
        match self.query.iter().find(|(k, _)| !allowed.iter().any(|a| a.eq_ignore_ascii_case(k))) {
            None => Ok(()),
            Some((k, _)) => Err(SecretError::reference(format!(
                "Unknown option \"{k}\" in {provider} reference; supported: {}",
                if allowed.is_empty() { "none".to_string() } else { allowed.join(", ") }
            ))),
        }
    }
}

/// String form of a JSON value: strings as-is, everything else as JSON.
pub(crate) fn value_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Picks `key` from a JSON object secret. A key that is not found as-is is
/// tried as a dotted path (`db.password`). Never echoes the secret.
pub(crate) fn extract_json_key(raw: &str, key: &str, what: &str) -> Result<String, SecretError> {
    let parsed: Value = serde_json::from_str(raw).map_err(|_| {
        SecretError::reference(format!(
            "{what} is not a JSON object, so key \"{key}\" cannot be extracted; remove \"#{key}\" from the reference."
        ))
    })?;
    let Value::Object(map) = &parsed else {
        return Err(SecretError::reference(format!(
            "{what} is not a JSON object, so key \"{key}\" cannot be extracted; remove \"#{key}\" from the reference."
        )));
    };
    if let Some(v) = map.get(key) {
        return Ok(value_string(v));
    }
    let mut cur = &parsed;
    for part in key.split('.') {
        match cur.get(part) {
            Some(v) => cur = v,
            None => {
                return Err(SecretError::not_found(format!(
                    "Key \"{key}\" not found in {what} (keys: {}).",
                    key_list(map.keys())
                )));
            }
        }
    }
    Ok(value_string(cur))
}

pub(crate) fn key_list<'a>(keys: impl Iterator<Item = &'a String>) -> String {
    let mut v: Vec<&str> = keys.map(String::as_str).collect();
    if v.is_empty() {
        return "none".into();
    }
    v.sort();
    if v.len() > 20 {
        let more = v.len() - 20;
        v.truncate(20);
        return format!("{}, … {more} more", v.join(", "));
    }
    v.join(", ")
}

/// Percent-encodes each `/`-separated segment of a path.
pub(crate) fn encode_path(path: &str) -> String {
    const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~')
        .remove(b'@')
        .remove(b'+')
        .remove(b'=')
        .remove(b':')
        .remove(b',');
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_encoding::utf8_percent_encode(s, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_references() {
        let r = Reference::parse(" app/db?version=3#password ").unwrap();
        assert_eq!(r.path, "app/db");
        assert_eq!(r.param("version"), Some("3"));
        assert_eq!(r.field.as_deref(), Some("password"));
        let r = Reference::parse("/name/").unwrap();
        assert_eq!((r.path.as_str(), r.field.as_deref()), ("name", None));
        assert!(Reference::parse("#x").is_err());
        assert!(r.only_params(&[], "Azure").is_ok());
        assert!(Reference::parse("a?bad=1").unwrap().only_params(&["version"], "Vault").is_err());
        let arn = Reference::parse("arn:aws:secretsmanager:eu-west-1:123:secret:db-AbCd#user").unwrap();
        assert_eq!(arn.path, "arn:aws:secretsmanager:eu-west-1:123:secret:db-AbCd");
    }

    #[test]
    fn json_keys() {
        let raw = r#"{"user":"u","port":5432,"nested":{"pw":"p"},"a.b":"dot"}"#;
        assert_eq!(extract_json_key(raw, "user", "s").unwrap(), "u");
        assert_eq!(extract_json_key(raw, "port", "s").unwrap(), "5432");
        assert_eq!(extract_json_key(raw, "nested.pw", "s").unwrap(), "p");
        assert_eq!(extract_json_key(raw, "a.b", "s").unwrap(), "dot");
        let e = extract_json_key(raw, "nope", "secret x").unwrap_err();
        assert!(e.message.contains("a.b, nested, port, user"), "{e}");
        let e = extract_json_key("plain-s3cret", "k", "secret x").unwrap_err();
        assert!(!e.message.contains("plain-s3cret"));
    }

    #[test]
    fn encodes_segments() {
        assert_eq!(encode_path("/a b/c%d//e"), "a%20b/c%25d/e");
    }
}
