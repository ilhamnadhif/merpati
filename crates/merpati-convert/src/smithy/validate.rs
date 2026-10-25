//! Semantic validation: unknown targets, shape kinds of references and the
//! HTTP binding rules that matter for sending requests.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::resolve::PRELUDE_SHAPES;
use super::{Diagnostic, Member, Model, Pos, Shape, ShapeKind};

pub(super) const REST_PROTOCOLS: &[&str] = &["restJson1", "restXml", "simpleRestJson"];
pub(super) const RPC_PROTOCOLS: &[&str] = &["awsJson1_0", "awsJson1_1", "awsQuery", "ec2Query", "rpcv2Cbor"];

/// Member traits that bind a member to somewhere other than the body.
pub(super) const NON_BODY_BINDINGS: &[&str] =
    &["httpLabel", "httpQuery", "httpQueryParams", "httpHeader", "httpPrefixHeaders", "httpResponseCode"];

pub(super) fn is_prelude(id: &str) -> bool {
    id.strip_prefix("smithy.api#").is_some_and(|n| PRELUDE_SHAPES.contains(&n))
}

/// The protocol trait of a service (`restJson1`, `awsJson1_1`, ...).
pub(super) fn protocol(service: &Shape) -> Option<&'static str> {
    REST_PROTOCOLS.iter().chain(RPC_PROTOCOLS).find(|p| service.has_trait(p)).copied()
}

/// Operations of a service in model order: the service's own operations,
/// then those of each resource (lifecycle first), recursively.
pub(super) fn bound_operations(m: &Model, service: &Shape) -> Vec<String> {
    fn resource(m: &Model, id: &str, out: &mut Vec<String>, seen: &mut HashSet<String>) {
        if !seen.insert(id.to_string()) {
            return;
        }
        let Some(r) = m.shape(id).filter(|r| r.kind == ShapeKind::Resource) else { return };
        for op in r.lifecycle.values().chain(&r.collection_operations).chain(&r.operations) {
            if !out.contains(&op.id) {
                out.push(op.id.clone());
            }
        }
        for child in &r.resources {
            resource(m, &child.id, out, seen);
        }
    }
    let mut out: Vec<String> = Vec::new();
    for op in &service.operations {
        if !out.contains(&op.id) {
            out.push(op.id.clone());
        }
    }
    let mut seen = HashSet::new();
    for r in &service.resources {
        resource(m, &r.id, &mut out, &mut seen);
    }
    out
}

/// A parsed `@http` URI pattern.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct UriPattern {
    /// Path segments: `(text, is_label, greedy)`.
    pub segments: Vec<(String, bool, bool)>,
    /// Literal query pairs from `?a=b&c`.
    pub query: Vec<(String, String)>,
    pub trailing_slash: bool,
}

impl UriPattern {
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter(|(_, l, _)| *l).map(|(s, _, _)| s.as_str())
    }
}

pub(super) fn parse_uri(uri: &str) -> Result<UriPattern, String> {
    if !uri.starts_with('/') {
        return Err(format!("URI `{uri}` must start with `/`"));
    }
    let (path, query) = match uri.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (uri, None),
    };
    let mut out = UriPattern { trailing_slash: path.len() > 1 && path.ends_with('/'), ..Default::default() };
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if let Some(inner) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            let (name, greedy) = match inner.strip_suffix('+') {
                Some(n) => (n, true),
                None => (inner, false),
            };
            if name.is_empty() || name.contains(['{', '}']) {
                return Err(format!("invalid label `{seg}` in URI `{uri}`"));
            }
            if out.labels().any(|l| l == name) {
                return Err(format!("label `{{{name}}}` appears twice in URI `{uri}`"));
            }
            out.segments.push((name.to_string(), true, greedy));
        } else if seg.contains(['{', '}']) {
            return Err(format!("label in `{seg}` must span a whole path segment in URI `{uri}`"));
        } else {
            out.segments.push((seg.to_string(), false, false));
        }
    }
    if let Some(q) = query {
        if q.contains(['{', '}']) {
            return Err(format!("labels are not allowed in the query string of URI `{uri}`"));
        }
        for pair in q.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            out.query.push((k.to_string(), v.to_string()));
        }
    }
    Ok(out)
}

/// `id` as written in the namespace of the shape `owner`: relative when it
/// is in that namespace or the prelude.
fn display(owner: &str, id: &str) -> String {
    match id.split_once('#') {
        Some((ns, name)) if ns == super::namespace_of(owner) || ns == "smithy.api" => name.to_string(),
        _ => id.to_string(),
    }
}

pub(super) fn validate(m: &Model) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let exists = |id: &str| m.shapes.contains_key(id) || is_prelude(id);
    let kind_of = |id: &str| m.shape(id).map(|s| s.kind);

    for s in m.shapes.values() {
        // Unknown targets.
        for mem in s.members.values() {
            if !exists(&mem.target) {
                out.push(
                    Diagnostic::error(
                        mem.pos,
                        format!("unknown target `{}` for member `{}`", display(&s.id, &mem.target), mem.name),
                    )
                    .on(&s.id),
                );
            }
        }
        for (what, r) in s.refs() {
            if !exists(&r.id) {
                out.push(Diagnostic::error(r.pos, format!("unknown {what} `{}`", display(&s.id, &r.id))).on(&s.id));
                continue;
            }
            let expected = match what {
                "input" | "output" | "error" => Some(ShapeKind::Structure),
                "operation" => Some(ShapeKind::Operation),
                "resource" => Some(ShapeKind::Resource),
                _ => None,
            };
            if let (Some(want), Some(got)) = (expected, kind_of(&r.id))
                && want != got
            {
                out.push(
                    Diagnostic::error(
                        r.pos,
                        format!(
                            "{what} `{}` must be a {}, not a {}",
                            display(&s.id, &r.id),
                            want.keyword(),
                            got.keyword()
                        ),
                    )
                    .on(&s.id),
                );
            }
            if what == "error" && m.shape(&r.id).is_some_and(|e| !e.has_trait("error")) {
                out.push(
                    Diagnostic::warning(r.pos, format!("error `{}` has no `@error` trait", display(&s.id, &r.id)))
                        .on(&s.id),
                );
            }
            if what == "mixin" && m.shape(&r.id).is_some_and(|x| !x.has_trait("mixin")) {
                out.push(
                    Diagnostic::error(r.pos, format!("`{}` is not a mixin (missing `@mixin`)", display(&s.id, &r.id)))
                        .on(&s.id),
                );
            }
        }
        if s.kind == ShapeKind::Operation && s.has_trait("http") {
            check_http(m, s, &mut out);
        }
    }

    for svc in m.services() {
        let ops = bound_operations(m, svc);
        let proto = protocol(svc);
        let rest = proto.is_some_and(|p| REST_PROTOCOLS.contains(&p));
        let any_http = ops.iter().any(|b| m.shape(b).is_some_and(|o| o.has_trait("http")));
        let mut routes: HashMap<(String, String), String> = HashMap::new();
        for b in &ops {
            let Some(op) = m.shape(b).filter(|o| o.kind == ShapeKind::Operation) else { continue };
            match op.get_trait("http") {
                None if rest => out.push(
                    Diagnostic::error(
                        op.pos,
                        format!(
                            "operation `{}` is bound to service `{}` ({}) but has no `@http` trait",
                            op.name(),
                            svc.name(),
                            proto.unwrap_or_default()
                        ),
                    )
                    .on(&op.id),
                ),
                None if proto.is_none() && any_http => out.push(
                    Diagnostic::warning(
                        op.pos,
                        format!("operation `{}` has no `@http` trait and cannot be imported as a request", op.name()),
                    )
                    .on(&op.id),
                ),
                None => {}
                Some(http) => {
                    let method = http.get("method").and_then(Value::as_str).unwrap_or("").to_ascii_uppercase();
                    let uri = http.get("uri").and_then(Value::as_str).unwrap_or("");
                    if let Ok(p) = parse_uri(uri) {
                        let mut key = String::new();
                        for (seg, label, greedy) in &p.segments {
                            key.push('/');
                            key.push_str(if *greedy {
                                "{+}"
                            } else if *label {
                                "{}"
                            } else {
                                seg
                            });
                        }
                        // Literal query parameters disambiguate routes (`?x-id=GetObject`).
                        if !p.query.is_empty() {
                            let mut q: Vec<String> = p.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
                            q.sort();
                            key.push('?');
                            key.push_str(&q.join("&"));
                        }
                        if let Some(prev) = routes.insert((method.clone(), key), op.id.clone()) {
                            out.push(
                                Diagnostic::error(
                                    op.trait_position("http"),
                                    format!(
                                        "`{method} {uri}` of `{}` conflicts with operation `{}`",
                                        op.name(),
                                        display(&op.id, &prev)
                                    ),
                                )
                                .on(&op.id),
                            );
                        }
                    }
                }
            }
        }
    }
    out
}

fn check_http(m: &Model, op: &Shape, out: &mut Vec<Diagnostic>) {
    let http = op.get_trait("http").cloned().unwrap_or(Value::Null);
    let tpos = op.trait_position("http");
    let err = |out: &mut Vec<Diagnostic>, pos: Pos, msg: String| out.push(Diagnostic::error(pos, msg).on(&op.id));
    let method = http.get("method").and_then(Value::as_str);
    match method {
        None => err(out, tpos, format!("`@http` of `{}` needs a `method`", op.name())),
        Some(mth) if mth.is_empty() || !mth.chars().all(|c| c.is_ascii_alphabetic()) => {
            err(out, tpos, format!("invalid HTTP method `{mth}` on `{}`", op.name()))
        }
        _ => {}
    }
    if let Some(code) = http.get("code")
        && !code.as_u64().is_some_and(|c| (100..=999).contains(&c))
    {
        err(out, tpos, format!("invalid status code `{code}` in `@http` of `{}`", op.name()));
    }
    let Some(uri) = http.get("uri").and_then(Value::as_str) else {
        err(out, tpos, format!("`@http` of `{}` needs a `uri`", op.name()));
        return;
    };
    let pattern = match parse_uri(uri) {
        Ok(p) => p,
        Err(e) => {
            err(out, tpos, e);
            return;
        }
    };
    let input = op.input.as_ref().and_then(|r| m.shape(&r.id)).filter(|s| s.kind == ShapeKind::Structure);
    let members: Vec<&Member> = input.map(|s| s.members.values().collect()).unwrap_or_default();
    for label in pattern.labels() {
        let found = members.iter().any(|mem| mem.name == label && mem.has_trait("httpLabel"));
        if !found {
            let msg = match members.iter().find(|mem| mem.name == label) {
                Some(_) => {
                    format!("URI label `{{{label}}}` of `{}`: input member `{label}` needs `@httpLabel`", op.name())
                }
                None => format!("URI label `{{{label}}}` of `{}` has no matching `@httpLabel` input member", op.name()),
            };
            err(out, tpos, msg);
        }
    }
    let mut payloads = 0;
    let mut unbound: Vec<&str> = Vec::new();
    for mem in &members {
        if mem.has_trait("httpLabel") {
            if !mem.has_trait("required") {
                err(
                    out,
                    mem.trait_position("httpLabel"),
                    format!("`@httpLabel` member `{}` must be marked `@required`", mem.name),
                );
            }
            if !pattern.labels().any(|l| l == mem.name) {
                err(
                    out,
                    mem.trait_position("httpLabel"),
                    format!("`@httpLabel` member `{}` has no `{{{}}}` label in the URI `{uri}`", mem.name, mem.name),
                );
            }
        }
        if mem.has_trait("httpPayload") {
            payloads += 1;
            if payloads > 1 {
                err(out, mem.trait_position("httpPayload"), "only one member can be bound with `@httpPayload`".into());
            }
        } else if !NON_BODY_BINDINGS.iter().any(|b| mem.has_trait(b)) {
            unbound.push(&mem.name);
        }
    }
    if payloads > 0 && !unbound.is_empty() {
        err(
            out,
            tpos,
            format!(
                "`{}` uses `@httpPayload`, so every other input member must be bound to the URI, query or headers \
                 (unbound: {})",
                op.name(),
                unbound.join(", ")
            ),
        );
    }
    if let Some(mth) = method
        && matches!(mth.to_ascii_uppercase().as_str(), "GET" | "HEAD" | "DELETE" | "TRACE")
        && (payloads > 0 || !unbound.is_empty())
    {
        out.push(
            Diagnostic::warning(tpos, format!("{mth} operation `{}` has input members bound to the body", op.name()))
                .on(&op.id),
        );
    }
    // @paginated member names.
    if let Some(Value::Object(p)) = op.get_trait("paginated") {
        let output = op.output.as_ref().and_then(|r| m.shape(&r.id));
        for (key, in_input) in [("inputToken", true), ("pageSize", true), ("outputToken", false), ("items", false)] {
            let Some(path) = p.get(key).and_then(Value::as_str) else { continue };
            let first = path.split('.').next().unwrap_or(path);
            let shape = if in_input { input } else { output };
            if let Some(s) = shape
                && !s.members.contains_key(first)
            {
                out.push(
                    Diagnostic::warning(
                        op.trait_position("paginated"),
                        format!(
                            "`@paginated({key}: \"{path}\")` of `{}`: no member `{first}` in its {}",
                            op.name(),
                            if in_input { "input" } else { "output" }
                        ),
                    )
                    .on(&op.id),
                );
            }
        }
    }
}
