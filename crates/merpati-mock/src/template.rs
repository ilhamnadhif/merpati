//! Dynamic response templating (FR-M2).
//!
//! Example bodies and header values may contain:
//! - dynamic variables: `{{$guid}}`, `{{$randomInt}}`, ... (merpati_core::dynamic)
//! - path parameters captured from `:name` segments: `{{id}}`
//! - variables from the mock's variable scopes: `{{baseUrl}}`
//! - request helpers, as in Postman's dynamic mock responses:
//!   `{{$body 'user.name'}}`, `{{$queryParams 'page'}}`,
//!   `{{$pathSegments '1'}}`, `{{$headers 'x-request-id'}}`.
//!   Without an argument the helper returns the whole value as JSON.
//!
//! Unknown references are left untouched.

use std::cell::OnceCell;

use merpati_core::variables::{Resolver, TokenKind, VariableScopes, tokenize};
use serde_json::Value;

use crate::matcher::IncomingRequest;

pub struct TemplateContext<'a> {
    pub vars: &'a VariableScopes,
    pub path_params: &'a [(String, String)],
    pub request: &'a IncomingRequest,
    json_body: OnceCell<Option<Value>>,
}

impl<'a> TemplateContext<'a> {
    pub fn new(vars: &'a VariableScopes, path_params: &'a [(String, String)], request: &'a IncomingRequest) -> Self {
        TemplateContext { vars, path_params, request, json_body: OnceCell::new() }
    }

    fn json_body(&self) -> Option<&Value> {
        self.json_body.get_or_init(|| serde_json::from_slice(&self.request.body).ok()).as_ref()
    }

    fn helper(&self, name: &str, arg: Option<&str>) -> Option<String> {
        match name {
            "$body" => match arg {
                None => Some(String::from_utf8_lossy(&self.request.body).into_owned()),
                Some(path) => lookup_path(self.json_body()?, path).map(value_text),
            },
            "$queryParams" => match arg {
                None => {
                    let obj: serde_json::Map<String, Value> =
                        self.request.query.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
                    Some(Value::Object(obj).to_string())
                }
                Some(key) => self.request.query_param(key).map(str::to_string),
            },
            "$pathSegments" => {
                let segs = self.request.segments();
                match arg {
                    None => Some(Value::from(segs).to_string()),
                    Some(i) => i.parse::<usize>().ok().and_then(|i| segs.get(i).cloned()),
                }
            }
            "$headers" => match arg {
                None => {
                    let obj: serde_json::Map<String, Value> = self
                        .request
                        .headers
                        .iter()
                        .map(|(k, v)| (k.to_ascii_lowercase(), Value::String(v.clone())))
                        .collect();
                    Some(Value::Object(obj).to_string())
                }
                Some(h) => self.request.header(h).map(str::to_string),
            },
            _ => None,
        }
    }

    fn lookup(&self, reference: &str) -> Option<String> {
        let reference = reference.trim();
        let (name, arg) = split_helper(reference);
        if matches!(name, "$body" | "$queryParams" | "$pathSegments" | "$headers") {
            return self.helper(name, arg);
        }
        if let Some((_, v)) = self.path_params.iter().find(|(k, _)| k == reference) {
            return Some(v.clone());
        }
        Resolver::new(self.vars).lookup(reference).map(|(v, _)| v)
    }

    /// Renders all references in `input`.
    pub fn render(&self, input: &str) -> String {
        if !input.contains("{{") {
            return input.to_string();
        }
        let mut out = String::with_capacity(input.len());
        for tok in tokenize(input) {
            match tok.kind {
                TokenKind::Text => out.push_str(&input[tok.range]),
                TokenKind::Variable => match self.lookup(&input[tok.name_range.clone()]) {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&input[tok.range]),
                },
            }
        }
        out
    }
}

/// `$body 'a.b'` -> (`$body`, Some(`a.b`)); quotes are optional.
fn split_helper(reference: &str) -> (&str, Option<&str>) {
    match reference.split_once(char::is_whitespace) {
        Some((name, rest)) => {
            let rest = rest.trim();
            let arg = rest
                .strip_prefix('\'')
                .and_then(|r| r.strip_suffix('\''))
                .or_else(|| rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')))
                .unwrap_or(rest);
            (name, (!arg.is_empty()).then_some(arg))
        }
        None => (reference, None),
    }
}

/// Dot/bracket path lookup: `user.tags[0]`, `items.1.name`.
fn lookup_path<'v>(root: &'v Value, path: &str) -> Option<&'v Value> {
    let normalized = path.replace('[', ".").replace(']', "");
    let mut cur = root;
    for part in normalized.split('.').filter(|p| !p.is_empty()) {
        cur = match cur {
            Value::Object(o) => o.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_and_params() {
        let req = IncomingRequest::new("POST", "/users/7/posts?page=3")
            .with_header("X-Request-Id", "abc")
            .with_body(r#"{"user": {"name": "Ada", "tags": ["x", "y"]}, "n": 5}"#);
        let mut vars = VariableScopes::default();
        vars.environment.insert("env".into(), "staging".into());
        let params = vec![("id".to_string(), "7".to_string())];
        let ctx = TemplateContext::new(&vars, &params, &req);
        assert_eq!(ctx.render("{{id}}"), "7");
        assert_eq!(ctx.render("{{$body 'user.name'}}"), "Ada");
        assert_eq!(ctx.render("{{$body \"user.tags[1]\"}}"), "y");
        assert_eq!(ctx.render("{{$body 'n'}}"), "5");
        assert_eq!(ctx.render("{{$queryParams 'page'}}"), "3");
        assert_eq!(ctx.render("{{$pathSegments '2'}}"), "posts");
        assert_eq!(ctx.render("{{$headers 'x-request-id'}}"), "abc");
        assert_eq!(ctx.render("{{env}}"), "staging");
        assert_eq!(ctx.render("{{unknown}} {{$body 'missing'}}"), "{{unknown}} {{$body 'missing'}}");
        let n: u32 = ctx.render("{{$randomInt}}").parse().unwrap();
        assert!(n <= 1000);
        assert_eq!(ctx.render("{{$guid}}").len(), 36);
    }
}
