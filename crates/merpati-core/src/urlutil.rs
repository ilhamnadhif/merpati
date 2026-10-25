//! Helpers for raw (unresolved) URLs: query/params syncing, `:path` variables
//! and light normalization. These work on strings that may still contain
//! `{{variables}}`, so they cannot rely on a full URL parser.

use crate::model::KeyValue;

/// Byte index of the `?` that starts the query, ignoring `?` inside `{{ }}`.
fn query_start(url: &str) -> Option<usize> {
    let bytes = url.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                depth += 1;
                i += 2;
                continue;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') && depth > 0 => {
                depth -= 1;
                i += 2;
                continue;
            }
            b'?' if depth == 0 => return Some(i),
            b'#' if depth == 0 => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Splits into (before query, query without `?`, fragment including `#`).
pub fn split(url: &str) -> (&str, Option<&str>, &str) {
    match query_start(url) {
        Some(q) => {
            let rest = &url[q + 1..];
            match rest.find('#') {
                Some(h) => (&url[..q], Some(&rest[..h]), &rest[h..]),
                None => (&url[..q], Some(rest), ""),
            }
        }
        None => match url.find('#') {
            Some(h) => (&url[..h], None, &url[h..]),
            None => (url, None, ""),
        },
    }
}

/// Parses a raw query string into rows, without percent-decoding so the user
/// sees exactly what they typed.
pub fn parse_query(query: &str) -> Vec<KeyValue> {
    if query.is_empty() {
        return Vec::new();
    }
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => KeyValue::new(k, v),
            None => KeyValue::new(pair, ""),
        })
        .collect()
}

/// The query string of the enabled `params`. Characters that would change
/// how it splits into params (`&` and `#`, plus `=` in keys) are
/// percent-encoded outside `{{variables}}`, so a typed value comes back as
/// one param; everything else is kept as typed.
pub fn build_query(params: &[KeyValue]) -> String {
    params
        .iter()
        .filter(|p| p.enabled && !(p.key.is_empty() && p.value.is_empty()))
        .map(|p| {
            let key = escape_query_part(&p.key, true);
            if p.value.is_empty() { key } else { format!("{key}={}", escape_query_part(&p.value, false)) }
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn escape_query_part(s: &str, key: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        // `{{name}}` is copied as is.
        if rest.starts_with("{{")
            && let Some(end) = rest.find("}}")
        {
            out.push_str(&rest[..end + 2]);
            rest = &rest[end + 2..];
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        match c {
            '&' => out.push_str("%26"),
            '#' => out.push_str("%23"),
            '=' if key => out.push_str("%3D"),
            c => out.push(c),
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Params derived from `url`, keeping disabled rows from `existing` at their
/// previous positions and descriptions of rows that still match.
pub fn merge_params_from_url(url: &str, existing: &[KeyValue]) -> Vec<KeyValue> {
    let (_, query, _) = split(url);
    let mut parsed = parse_query(query.unwrap_or(""));
    // Carry descriptions over by (key, occurrence) of enabled rows.
    let enabled_old: Vec<&KeyValue> = existing.iter().filter(|p| p.enabled).collect();
    for (i, p) in parsed.iter_mut().enumerate() {
        if let Some(old) = enabled_old.get(i)
            && old.key == p.key
        {
            p.description = old.description.clone();
        }
    }
    for (idx, old) in existing.iter().enumerate() {
        if !old.enabled {
            let at = idx.min(parsed.len());
            parsed.insert(at, old.clone());
        }
    }
    parsed
}

/// Replaces the query part of `url` with the enabled `params`.
pub fn replace_query(url: &str, params: &[KeyValue]) -> String {
    let (base, _, fragment) = split(url);
    let q = build_query(params);
    if q.is_empty() { format!("{base}{fragment}") } else { format!("{base}?{q}{fragment}") }
}

/// Start of the path part (after `scheme://host`), tolerating variables.
fn path_start(base: &str) -> usize {
    let after_scheme = base.find("://").map(|i| i + 3).unwrap_or(0);
    // Skip a leading `{{baseUrl}}`-style host.
    let rest = &base[after_scheme..];
    let mut i = 0;
    let bytes = rest.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'{'
            && bytes.get(i + 1) == Some(&b'{')
            && let Some(end) = rest[i..].find("}}")
        {
            i += end + 2;
            continue;
        }
        if bytes[i] == b'/' {
            return after_scheme + i;
        }
        i += 1;
    }
    base.len()
}

/// Names of `:var` path segments, in order.
pub fn path_var_names(url: &str) -> Vec<String> {
    let (base, _, _) = split(url);
    let path = &base[path_start(base)..];
    path.split('/')
        .filter_map(|seg| seg.strip_prefix(':'))
        .filter(|name| {
            !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
        })
        .map(str::to_string)
        .collect()
}

pub fn merge_path_vars(url: &str, existing: &[KeyValue]) -> Vec<KeyValue> {
    path_var_names(url)
        .into_iter()
        .map(|name| existing.iter().find(|e| e.key == name).cloned().unwrap_or_else(|| KeyValue::new(name, "")))
        .collect()
}

/// Replaces `:var` path segments with values (already resolved).
pub fn substitute_path_vars(url: &str, vars: &[(String, String)]) -> String {
    if vars.is_empty() {
        return url.to_string();
    }
    let (base, query, fragment) = split(url);
    let start = path_start(base);
    let (head, path) = base.split_at(start);
    let path = path
        .split('/')
        .map(|seg| match seg.strip_prefix(':') {
            Some(name) => match vars.iter().find(|(k, _)| k == name) {
                Some((_, v)) if !v.is_empty() => v.clone(),
                _ => seg.to_string(),
            },
            None => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/");
    let mut out = format!("{head}{path}");
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out.push_str(fragment);
    out
}

/// Adds `http://` when the URL has no scheme (FR-R1).
pub fn ensure_scheme(url: &str) -> String {
    let trimmed = url.trim();
    let has_scheme = trimmed
        .find("://")
        .map(|i| i > 0 && trimmed[..i].chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'))
        .unwrap_or(false);
    if has_scheme || trimmed.is_empty() { trimmed.to_string() } else { format!("http://{trimmed}") }
}

/// Percent-encodes characters that are not valid in a URL while leaving
/// existing escapes and reserved characters alone.
pub fn encode_loose(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for ch in url.chars() {
        let keep = ch.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(ch);
        if keep {
            out.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_parts() {
        assert_eq!(split("http://a/b?x=1#f"), ("http://a/b", Some("x=1"), "#f"));
        assert_eq!(split("{{base}}/b"), ("{{base}}/b", None, ""));
        assert_eq!(split("{{a?b}}/c?d"), ("{{a?b}}/c", Some("d"), ""));
    }

    #[test]
    fn params_roundtrip_keeps_disabled() {
        let existing = vec![KeyValue::new("a", "1"), KeyValue::new("off", "x").disabled(), KeyValue::new("b", "2")];
        let merged = merge_params_from_url("http://h/p?a=1&b=3", &existing);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[1].key, "off");
        assert!(!merged[1].enabled);
        assert_eq!(merged[2].value, "3");
        assert_eq!(replace_query("http://h/p?old=1#frag", &merged), "http://h/p?a=1&b=3#frag");
        assert_eq!(replace_query("http://h/p?old=1", &[]), "http://h/p");
    }

    #[test]
    fn typed_separators_stay_inside_their_param() {
        let rows =
            vec![KeyValue::new("q", "rock & roll #1"), KeyValue::new("a=b", "{{token}}"), KeyValue::new("é", "ü")];
        let url = replace_query("http://h/p#top", &rows);
        assert_eq!(url, "http://h/p?q=rock %26 roll %231&a%3Db={{token}}&é=ü#top");
        let back = merge_params_from_url(&url, &[]);
        assert_eq!(back.len(), 3);
        assert_eq!((back[0].key.as_str(), back[0].value.as_str()), ("q", "rock %26 roll %231"));
        assert_eq!((back[1].key.as_str(), back[1].value.as_str()), ("a%3Db", "{{token}}"));
        // Already encoded text is kept as typed.
        assert_eq!(build_query(&[KeyValue::new("q", "a%26b")]), "q=a%26b");
    }

    #[test]
    fn path_vars() {
        assert_eq!(path_var_names("{{base}}/users/:id/posts/:postId?x=:no"), vec!["id", "postId"]);
        assert_eq!(path_var_names("http://localhost:8080/a"), Vec::<String>::new());
        let out = substitute_path_vars("http://h:80/u/:id?q=1", &[("id".into(), "7".into())]);
        assert_eq!(out, "http://h:80/u/7?q=1");
    }

    #[test]
    fn scheme_and_encoding() {
        assert_eq!(ensure_scheme("localhost:3000/x"), "http://localhost:3000/x");
        assert_eq!(ensure_scheme("https://a.b"), "https://a.b");
        assert_eq!(encode_loose("http://h/a b?q=é"), "http://h/a%20b?q=%C3%A9");
        assert_eq!(encode_loose("http://h/a%20b"), "http://h/a%20b");
    }
}
