//! Pure request-to-example matching, modelled on Postman's mock matching.
//!
//! Only HTTP requests with examples take part. Each example's own saved
//! request (falling back to the parent request) provides the method and URL.
//! Scheme and host are ignored, `:param` and `{{var}}` path segments are
//! wildcards, and exact segments beat wildcards. Query parameters and the
//! body refine the score; `x-mock-response-*` headers pick a specific example.

use std::borrow::Cow;

use merpati_core::model::{BodyMode, Collection, Example, HttpRequest, KeyValue, RequestItem, RequestKind};
use merpati_core::urlutil;
use merpati_core::variables::{TokenKind, VariableScopes, tokenize};
use serde_json::Value;

/// A request received by the mock server, already split into parts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IncomingRequest {
    pub method: String,
    /// Path as received (may be percent-encoded), without the query string.
    pub path: String,
    /// Decoded query parameters in order.
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl IncomingRequest {
    /// Builds a request from a method and a `path?query` string.
    pub fn new(method: &str, path_and_query: &str) -> Self {
        let (path, query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path_and_query, ""),
        };
        IncomingRequest {
            method: method.to_uppercase(),
            path: if path.is_empty() { "/".into() } else { path.to_string() },
            query: url::form_urlencoded::parse(query.as_bytes()).into_owned().collect(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Decoded, non-empty path segments.
    pub fn segments(&self) -> Vec<String> {
        split_segments(&self.path).into_iter().map(|s| decode(s).into_owned()).collect()
    }

    pub fn query_string(&self) -> String {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &self.query {
            ser.append_pair(k, v);
        }
        ser.finish()
    }
}

/// How well an example matched. Fields are compared in order: path
/// exactness first, then query/body detail, then preference for 2xx.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct MatchScore {
    /// 2 points per exact segment, 1 per wildcard segment.
    pub path: i64,
    /// Query parameter and body similarity (may be negative).
    pub detail: i64,
    /// The example has a 2xx status.
    pub success: bool,
}

#[derive(Clone, Debug)]
pub struct MatchResult<'a> {
    pub request: &'a RequestItem,
    pub example: &'a Example,
    /// Values captured by `:name` and `{{name}}` path segments.
    pub path_params: Vec<(String, String)>,
    pub score: MatchScore,
}

impl MatchResult<'_> {
    /// `request name / example name`, used in logs.
    pub fn label(&self) -> String {
        format!("{} / {}", self.request.name, self.example.name)
    }
}

/// A path segment of an example URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Seg {
    Exact(String),
    /// `:name` or a whole-segment `{{name}}`.
    Param(String),
    /// Segment containing an unresolved variable among other text.
    Wild,
}

fn split_segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

fn decode(s: &str) -> Cow<'_, str> {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy()
}

/// Replaces `{{name}}` with values from `vars`, except dynamic (`$`) names
/// and unknown names, which stay as references (and become wildcards).
pub(crate) fn resolve_static(input: &str, vars: &VariableScopes) -> String {
    if !input.contains("{{") {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    for tok in tokenize(input) {
        match tok.kind {
            TokenKind::Text => out.push_str(&input[tok.range]),
            TokenKind::Variable => {
                let name = input[tok.name_range.clone()].trim();
                match vars.get(name).filter(|_| !name.starts_with('$')) {
                    Some((v, _)) if !v.contains("{{") => out.push_str(&v),
                    _ => out.push_str(&input[tok.range]),
                }
            }
        }
    }
    out
}

/// Index where the path starts in `base` (after `scheme://host[:port]`),
/// skipping over `{{...}}` in the scheme and host parts.
fn path_start(base: &str) -> usize {
    let (offset, rest) = match base.find("://") {
        // A `://` after a `/` is part of the path, not the scheme separator.
        Some(i) if !base[..i].contains('/') => (i + 3, &base[i + 3..]),
        _ => {
            if base.starts_with('/') {
                return 0;
            }
            (0, base)
        }
    };
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && bytes.get(i + 1) == Some(&b'{')
            && let Some(end) = rest[i..].find("}}")
        {
            i += end + 2;
            continue;
        }
        if bytes[i] == b'/' {
            return offset + i;
        }
        i += 1;
    }
    base.len()
}

fn parse_segments(path: &str) -> Vec<Seg> {
    split_segments(path)
        .into_iter()
        .map(|s| {
            if let Some(name) = s.strip_prefix(':').filter(|n| !n.is_empty()) {
                return Seg::Param(name.to_string());
            }
            if s.contains("{{") {
                let toks = tokenize(s);
                if toks.len() == 1 && toks[0].kind == TokenKind::Variable {
                    return Seg::Param(s[toks[0].name_range.clone()].trim().to_string());
                }
                return Seg::Wild;
            }
            Seg::Exact(decode(s).into_owned())
        })
        .collect()
}

/// Path part of a raw URL as shown to the user (`/users/:id`).
pub(crate) fn display_path(raw: &str) -> String {
    let (base, _, _) = urlutil::split(raw.trim());
    let path = match leading_var_rest(base) {
        Some(rest) => rest,
        None => &base[path_start(base)..],
    };
    if path.is_empty() {
        "/".into()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// For URLs like `{{baseUrl}}/users`, the part after the leading variable.
fn leading_var_rest(base: &str) -> Option<&str> {
    let trimmed = base.trim_start();
    if !trimmed.starts_with("{{") {
        return None;
    }
    let end = trimmed.find("}}")? + 2;
    let rest = &trimmed[end..];
    // `{{scheme}}://{{host}}/x`: the variable is the scheme, see `path_start`.
    if rest.starts_with("://") {
        return None;
    }
    // `{{host}}:8080/x` -> `/x`
    match rest.find('/') {
        Some(i) => Some(&rest[i..]),
        None => Some(""),
    }
}

/// Candidate path patterns for an example URL: with the leading variable
/// treated as the host, and with variables resolved.
pub(crate) fn url_patterns(raw: &str, vars: &VariableScopes) -> Vec<Vec<Seg>> {
    let (base, _, _) = urlutil::split(raw.trim());
    let mut out = Vec::new();
    if let Some(rest) = leading_var_rest(base) {
        out.push(parse_segments(rest));
    }
    let resolved = resolve_static(base, vars);
    let resolved = resolved.trim();
    let pattern = parse_segments(&resolved[path_start(resolved)..]);
    if !out.contains(&pattern) {
        out.push(pattern);
    }
    out
}

/// Path score and captured params, or `None` when the path does not match.
fn match_path(pattern: &[Seg], incoming: &[String]) -> Option<(i64, Vec<(String, String)>)> {
    if pattern.len() != incoming.len() {
        return None;
    }
    let mut score = 0;
    let mut params = Vec::new();
    for (seg, actual) in pattern.iter().zip(incoming) {
        match seg {
            Seg::Exact(e) if e == actual => score += 2,
            Seg::Exact(_) => return None,
            Seg::Param(name) => {
                score += 1;
                params.push((name.clone(), actual.clone()));
            }
            Seg::Wild => score += 1,
        }
    }
    Some((score, params))
}

fn query_score(params: &[KeyValue], vars: &VariableScopes, req: &IncomingRequest) -> i64 {
    let mut score = 0;
    let enabled: Vec<&KeyValue> = params.iter().filter(|p| p.enabled && !p.key.is_empty()).collect();
    for p in &enabled {
        let key = decode(&resolve_static(&p.key, vars)).into_owned();
        let want = decode(&resolve_static(&p.value, vars)).into_owned();
        match req.query.iter().find(|(k, _)| *k == key) {
            Some((_, have)) if want.contains("{{") || *have == want => score += 2,
            Some(_) => score -= 1,
            None => score -= 2,
        }
    }
    for (k, _) in &req.query {
        let known = enabled.iter().any(|p| decode(&resolve_static(&p.key, vars)) == k.as_str());
        if !known {
            score -= 1;
        }
    }
    score
}

/// Outcome of comparing request bodies.
enum BodyCmp {
    /// Nothing comparable (no body on either side, or form-data).
    Neutral,
    Equal,
    /// Partial similarity score for JSON objects; negative if unrelated.
    Partial(i64),
}

fn compare_body(example_req: &HttpRequest, vars: &VariableScopes, req: &IncomingRequest) -> BodyCmp {
    let body = &example_req.body;
    match body.mode {
        BodyMode::Raw => {
            let want = resolve_static(&body.raw, vars);
            if want.trim().is_empty() && req.body.is_empty() {
                return BodyCmp::Neutral;
            }
            let have = String::from_utf8_lossy(&req.body);
            match (serde_json::from_str::<Value>(&want), serde_json::from_str::<Value>(&have)) {
                (Ok(a), Ok(b)) if a == b => BodyCmp::Equal,
                (Ok(Value::Object(a)), Ok(Value::Object(b))) => {
                    let mut s = 0;
                    for (k, v) in &a {
                        match b.get(k) {
                            Some(x) if x == v => s += 1,
                            Some(_) => {}
                            None => s -= 1,
                        }
                    }
                    BodyCmp::Partial(s)
                }
                (Ok(_), Ok(_)) => BodyCmp::Partial(-1),
                _ if want.trim() == have.trim() => BodyCmp::Equal,
                _ => BodyCmp::Partial(-1),
            }
        }
        BodyMode::UrlEncoded => {
            let mut want: Vec<(String, String)> = body
                .urlencoded
                .iter()
                .filter(|f| f.enabled && !f.key.is_empty())
                .map(|f| (resolve_static(&f.key, vars), resolve_static(&f.value, vars)))
                .collect();
            let mut have: Vec<(String, String)> = url::form_urlencoded::parse(&req.body).into_owned().collect();
            if want.is_empty() && have.is_empty() {
                return BodyCmp::Neutral;
            }
            want.sort();
            have.sort();
            if want == have { BodyCmp::Equal } else { BodyCmp::Partial(-1) }
        }
        BodyMode::GraphQl => {
            let have: Option<Value> = serde_json::from_slice(&req.body).ok();
            let query = have.as_ref().and_then(|v| v.get("query")).and_then(Value::as_str).unwrap_or("");
            let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
            if norm(query) == norm(&body.graphql.query) { BodyCmp::Equal } else { BodyCmp::Partial(-1) }
        }
        _ => BodyCmp::Neutral,
    }
}

fn header_list(v: Option<&str>) -> Vec<String> {
    v.map(|s| s.split(',').map(|x| x.trim().to_ascii_lowercase()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

/// Effective request (method, URL, params, body) for an example.
fn example_request<'a>(parent: &'a HttpRequest, ex: &'a Example) -> &'a HttpRequest {
    match &ex.request {
        Some(r) if !r.url.trim().is_empty() => r,
        _ => parent,
    }
}

fn http_items(c: &Collection) -> impl Iterator<Item = (&RequestItem, &HttpRequest)> {
    c.requests().into_iter().filter_map(|r| match &r.request {
        RequestKind::Http(h) if !r.examples.is_empty() => Some((r, h)),
        _ => None,
    })
}

/// Finds the best example for `req`, or `None` (the server answers 404).
pub fn find_match<'a>(c: &'a Collection, vars: &VariableScopes, req: &IncomingRequest) -> Option<MatchResult<'a>> {
    let result = find_match_for_method(c, vars, req, &req.method);
    if result.is_none() && req.method.eq_ignore_ascii_case("HEAD") {
        return find_match_for_method(c, vars, req, "GET");
    }
    result
}

fn find_match_for_method<'a>(
    c: &'a Collection,
    vars: &VariableScopes,
    req: &IncomingRequest,
    method: &str,
) -> Option<MatchResult<'a>> {
    let segments = req.segments();
    let want_id = req.header("x-mock-response-id").map(str::trim).filter(|s| !s.is_empty());
    let want_name = req.header("x-mock-response-name").map(str::trim).filter(|s| !s.is_empty());
    let want_code: Option<u16> = req.header("x-mock-response-code").and_then(|s| s.trim().parse().ok());
    let strict_body =
        req.header("x-mock-match-request-body").map(|v| v.trim().eq_ignore_ascii_case("true")).unwrap_or(false);
    let strict_headers = header_list(req.header("x-mock-match-request-headers"));

    let mut best: Option<MatchResult<'a>> = None;
    for (item, parent) in http_items(c) {
        for ex in &item.examples {
            let ereq = example_request(parent, ex);
            let ex_method = if ereq.method.trim().is_empty() { "GET" } else { ereq.method.trim() };
            if !ex_method.eq_ignore_ascii_case(method) {
                continue;
            }
            let Some((path_score, params)) =
                url_patterns(&ereq.url, vars).iter().filter_map(|p| match_path(p, &segments)).max_by_key(|(s, _)| *s)
            else {
                continue;
            };
            if want_id.is_some_and(|id| id != ex.id) {
                continue;
            }
            if want_name.is_some_and(|name| !ex.name.eq_ignore_ascii_case(name)) {
                continue;
            }
            if want_code.is_some_and(|code| code != ex.status) {
                continue;
            }
            if !strict_headers.is_empty() {
                let all = strict_headers.iter().all(|h| {
                    let expected = ereq
                        .headers
                        .iter()
                        .find(|kv| kv.enabled && kv.key.eq_ignore_ascii_case(h))
                        .map(|kv| resolve_static(&kv.value, vars));
                    match (expected, req.header(h)) {
                        (Some(e), Some(a)) => e == a,
                        _ => false,
                    }
                });
                if !all {
                    continue;
                }
            }
            let mut detail = query_score(&ereq.params, vars, req);
            match compare_body(ereq, vars, req) {
                BodyCmp::Equal => detail += 4,
                BodyCmp::Partial(_) if strict_body => continue,
                BodyCmp::Partial(s) => detail += s,
                BodyCmp::Neutral => {}
            }
            let score = MatchScore { path: path_score, detail, success: (200..300).contains(&ex.status) };
            // Strictly greater: on ties the first example in the tree wins.
            if best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(MatchResult { request: item, example: ex, path_params: params, score });
            }
        }
    }

    if best.is_none() {
        // Ids are unique, so an explicit id may select an example on any path.
        if let Some(id) = want_id {
            for (item, _) in http_items(c) {
                if let Some(ex) = item.examples.iter().find(|e| e.id == id) {
                    return Some(MatchResult {
                        request: item,
                        example: ex,
                        path_params: Vec::new(),
                        score: MatchScore::default(),
                    });
                }
            }
        }
    }
    best
}

/// One mockable endpoint, for display in the UI.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct MockRoute {
    pub method: String,
    /// Path pattern, e.g. `/users/:id`.
    pub path: String,
    pub request_id: String,
    pub request_name: String,
    /// Example names served by this route.
    pub examples: Vec<String>,
}

/// Routes served for a collection (for display before the server starts).
pub fn routes(c: &Collection) -> Vec<MockRoute> {
    let mut out: Vec<MockRoute> = Vec::new();
    for (item, parent) in http_items(c) {
        for ex in &item.examples {
            let ereq = example_request(parent, ex);
            let method =
                if ereq.method.trim().is_empty() { "GET".to_string() } else { ereq.method.trim().to_uppercase() };
            let path = display_path(&ereq.url);
            match out.iter_mut().find(|r| r.request_id == item.id && r.method == method && r.path == path) {
                Some(r) => r.examples.push(ex.name.clone()),
                None => out.push(MockRoute {
                    method,
                    path,
                    request_id: item.id.clone(),
                    request_name: item.name.clone(),
                    examples: vec![ex.name.clone()],
                }),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::model::{Body, Folder};

    fn example(name: &str, status: u16, body: &str) -> Example {
        Example { name: name.into(), status, body: body.into(), ..Default::default() }
    }

    fn with_examples(name: &str, method: &str, url: &str, examples: Vec<Example>) -> RequestItem {
        let mut r = RequestItem::new(name, HttpRequest::new(method, url));
        r.examples = examples;
        r
    }

    fn collection() -> Collection {
        let mut c = Collection::new("API");
        let mut users = Folder::new("Users");
        users.items.push(
            with_examples(
                "Get user",
                "GET",
                "{{baseUrl}}/users/:id",
                vec![example("OK", 200, "{\"id\": 1}"), example("Missing", 404, "{}")],
            )
            .into(),
        );
        users
            .items
            .push(with_examples("Me", "GET", "https://api.x.com/users/me", vec![example("Me", 200, "me")]).into());
        users.items.push(
            with_examples("Create", "POST", "{{baseUrl}}/users", vec![example("Created", 201, "created")]).into(),
        );
        c.items.push(users.into());
        // Two examples of a search, distinguished by query.
        let mut page2 = example("Page 2", 200, "p2");
        page2.request = Some(HttpRequest::new("GET", "{{baseUrl}}/search?q=rust&page=2"));
        let mut page1 = example("Page 1", 200, "p1");
        page1.request = Some(HttpRequest::new("GET", "{{baseUrl}}/search?q=rust&page=1"));
        c.items.push(with_examples("Search", "GET", "{{baseUrl}}/search?q=rust", vec![page1, page2]).into());
        // Items without examples never match.
        c.items.push(RequestItem::new("No examples", HttpRequest::new("GET", "{{baseUrl}}/empty")).into());
        c
    }

    fn vars() -> VariableScopes {
        let mut v = VariableScopes::default();
        v.environment.insert("baseUrl".into(), "http://localhost:3000".into());
        v
    }

    #[test]
    fn wildcard_and_exact_paths() {
        let c = collection();
        let m = find_match(&c, &vars(), &IncomingRequest::new("GET", "/users/7")).unwrap();
        assert_eq!(m.label(), "Get user / OK");
        assert_eq!(m.path_params, vec![("id".to_string(), "7".to_string())]);
        // Exact `/users/me` beats `/users/:id`.
        let m = find_match(&c, &vars(), &IncomingRequest::new("GET", "/users/me")).unwrap();
        assert_eq!(m.request.name, "Me");
        // Trailing slash is ignored.
        assert!(find_match(&c, &vars(), &IncomingRequest::new("GET", "/users/7/")).is_some());
    }

    #[test]
    fn method_must_match() {
        let c = collection();
        assert!(find_match(&c, &vars(), &IncomingRequest::new("DELETE", "/users/7")).is_none());
        let m = find_match(&c, &vars(), &IncomingRequest::new("POST", "/users")).unwrap();
        assert_eq!(m.example.status, 201);
        // HEAD falls back to GET examples.
        assert!(find_match(&c, &vars(), &IncomingRequest::new("HEAD", "/users/7")).is_some());
        assert!(find_match(&c, &vars(), &IncomingRequest::new("GET", "/empty")).is_none());
    }

    #[test]
    fn prefers_success_on_ties() {
        let c = collection();
        let m = find_match(&c, &vars(), &IncomingRequest::new("GET", "/users/1")).unwrap();
        assert_eq!(m.example.name, "OK");
    }

    #[test]
    fn query_scoring() {
        let c = collection();
        let m = find_match(&c, &vars(), &IncomingRequest::new("GET", "/search?q=rust&page=2")).unwrap();
        assert_eq!(m.example.name, "Page 2");
        let m = find_match(&c, &vars(), &IncomingRequest::new("GET", "/search?page=1&q=rust")).unwrap();
        assert_eq!(m.example.name, "Page 1");
    }

    #[test]
    fn selection_headers() {
        let c = collection();
        let req = IncomingRequest::new("GET", "/users/7").with_header("x-mock-response-name", "Missing");
        assert_eq!(find_match(&c, &vars(), &req).unwrap().example.status, 404);
        let req = IncomingRequest::new("GET", "/users/7").with_header("X-Mock-Response-Code", "404");
        assert_eq!(find_match(&c, &vars(), &req).unwrap().example.name, "Missing");
        let req = IncomingRequest::new("GET", "/users/7").with_header("x-mock-response-code", "500");
        assert!(find_match(&c, &vars(), &req).is_none());
        let id = c.requests()[0].examples[1].id.clone();
        let req = IncomingRequest::new("GET", "/somewhere/else").with_header("x-mock-response-id", &id);
        assert_eq!(find_match(&c, &vars(), &req).unwrap().example.name, "Missing");
    }

    #[test]
    fn resolved_base_url_with_path_prefix() {
        let mut c = Collection::new("API");
        c.items.push(with_examples("Ping", "GET", "{{api}}/ping", vec![example("Pong", 200, "pong")]).into());
        let mut v = VariableScopes::default();
        v.collection.insert("api".into(), "https://example.com/v2".into());
        // Both the host-stripped and the resolved path match.
        assert!(find_match(&c, &v, &IncomingRequest::new("GET", "/ping")).is_some());
        assert!(find_match(&c, &v, &IncomingRequest::new("GET", "/v2/ping")).is_some());
    }

    #[test]
    fn variable_scheme_and_host_are_not_path() {
        let mut c = Collection::new("API");
        let users = with_examples("Users", "GET", "{{scheme}}://{{host}}/api/users", vec![example("All", 200, "[]")]);
        c.items.push(users.into());
        c.items.push(with_examples("Go", "GET", "{{base}}/go/https://x.test", vec![example("Go", 200, "go")]).into());
        let v = VariableScopes::default();
        assert!(find_match(&c, &v, &IncomingRequest::new("GET", "/api/users")).is_some());
        assert!(find_match(&c, &v, &IncomingRequest::new("GET", "/h/api/users")).is_none());
        assert_eq!(routes(&c)[0].path, "/api/users");
        // A `://` later in the path is not a scheme separator.
        assert!(find_match(&c, &v, &IncomingRequest::new("GET", "/go/https:/x.test")).is_some());
    }

    #[test]
    fn json_body_scoring_and_strict_mode() {
        let mut c = Collection::new("API");
        let mut admin = example("Admin", 200, "admin");
        let mut r = HttpRequest::new("POST", "/login");
        r.body = Body::raw_json("{\"user\": \"admin\"}");
        admin.request = Some(r);
        let mut guest = example("Guest", 200, "guest");
        let mut r = HttpRequest::new("POST", "/login");
        r.body = Body::raw_json("{\"user\": \"guest\"}");
        guest.request = Some(r);
        c.items.push(with_examples("Login", "POST", "/login", vec![admin, guest]).into());
        let v = VariableScopes::default();
        let req = IncomingRequest::new("POST", "/login").with_body(r#"{ "user": "guest" }"#);
        assert_eq!(find_match(&c, &v, &req).unwrap().example.name, "Guest");
        let req = IncomingRequest::new("POST", "/login")
            .with_body(r#"{"user": "nobody"}"#)
            .with_header("x-mock-match-request-body", "true");
        assert!(find_match(&c, &v, &req).is_none());
    }

    #[test]
    fn strict_header_matching() {
        let mut c = Collection::new("API");
        let mut v1 = example("V1", 200, "1");
        let mut r = HttpRequest::new("GET", "/thing");
        r.headers.push(KeyValue::new("Accept-Version", "1"));
        v1.request = Some(r);
        c.items.push(with_examples("Thing", "GET", "/thing", vec![v1]).into());
        let v = VariableScopes::default();
        let req = IncomingRequest::new("GET", "/thing")
            .with_header("x-mock-match-request-headers", "accept-version")
            .with_header("Accept-Version", "2");
        assert!(find_match(&c, &v, &req).is_none());
        let req = IncomingRequest::new("GET", "/thing")
            .with_header("x-mock-match-request-headers", "accept-version")
            .with_header("accept-version", "1");
        assert!(find_match(&c, &v, &req).is_some());
    }

    #[test]
    fn percent_encoded_segments() {
        let mut c = Collection::new("API");
        c.items.push(with_examples("Space", "GET", "/files/my file", vec![example("F", 200, "f")]).into());
        let v = VariableScopes::default();
        assert!(find_match(&c, &v, &IncomingRequest::new("GET", "/files/my%20file")).is_some());
    }

    #[test]
    fn route_listing() {
        let rs = routes(&collection());
        let get_user = rs.iter().find(|r| r.request_name == "Get user").unwrap();
        assert_eq!(get_user.path, "/users/:id");
        assert_eq!(get_user.examples, vec!["OK", "Missing"]);
        assert_eq!(rs.iter().find(|r| r.request_name == "Me").unwrap().path, "/users/me");
        assert!(rs.iter().all(|r| r.request_name != "No examples"));
    }
}
