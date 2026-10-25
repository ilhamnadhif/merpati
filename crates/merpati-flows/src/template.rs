//! Flow references inside `{{ }}` and template rendering.
//!
//! A reference whose first segment is one of [`ROOTS`] followed by `.` or
//! `[` is a flow reference (`{{blocks.login.output.json.token}}`); anything
//! else is a normal variable (`{{baseUrl}}`, `{{$guid}}`, `{{vault:key}}`)
//! resolved through the usual scopes. Flow variables and inputs live in the
//! data scope, so `{{name}}` finds them too.

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;
use merpati_core::variables::{Resolver, VariableScopes, VaultLookup, is_pure_reference, references, value_to_string};
use serde::Serialize;
use serde_json::{Value, json};

use crate::model::{CompareOp, Flow};

/// First segments that make a reference a flow reference.
pub const ROOTS: [&str; 6] = ["blocks", "inputs", "vars", "loop", "input", "flow"];

const MAX_DEPTH: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Index(i64),
}

/// Parses `a.b[0]["c d"].e` into segments. Plain segments may contain any
/// character except `.`, `[` and `]` (so labels with spaces or dashes work);
/// brackets take an index (negative counts from the end) or a quoted key.
pub fn parse_path(s: &str) -> Option<Vec<Segment>> {
    let s = s.trim();
    let b = s.as_bytes();
    let mut segs = Vec::new();
    let mut i = 0;
    let read_ident = |i: &mut usize| -> Option<String> {
        let start = *i;
        while *i < b.len() && !matches!(b[*i], b'.' | b'[' | b']') {
            *i += 1;
        }
        let id = s[start..*i].trim();
        (!id.is_empty()).then(|| id.to_string())
    };
    segs.push(Segment::Key(read_ident(&mut i)?));
    while i < b.len() {
        match b[i] {
            b'.' => {
                i += 1;
                segs.push(Segment::Key(read_ident(&mut i)?));
            }
            b'[' => {
                i += 1;
                while i < b.len() && b[i] == b' ' {
                    i += 1;
                }
                if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                    let quote = b[i];
                    i += 1;
                    let mut key = String::new();
                    let mut start = i;
                    loop {
                        if i >= b.len() {
                            return None;
                        }
                        if b[i] == b'\\' && i + 1 < b.len() {
                            key.push_str(&s[start..i]);
                            start = i + 1;
                            i += 2;
                            continue;
                        }
                        if b[i] == quote {
                            key.push_str(&s[start..i]);
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    while i < b.len() && b[i] == b' ' {
                        i += 1;
                    }
                    if i >= b.len() || b[i] != b']' {
                        return None;
                    }
                    i += 1;
                    segs.push(Segment::Key(key));
                } else {
                    let start = i;
                    while i < b.len() && b[i] != b']' {
                        i += 1;
                    }
                    if i >= b.len() {
                        return None;
                    }
                    let inner = s[start..i].trim();
                    i += 1;
                    if inner.is_empty() {
                        return None;
                    }
                    segs.push(match inner.parse::<i64>() {
                        Ok(n) => Segment::Index(n),
                        Err(_) => Segment::Key(inner.to_string()),
                    });
                }
            }
            _ => return None,
        }
    }
    Some(segs)
}

/// The flow root of a reference name, if it has one (`blocks.x` → `blocks`).
/// Bare roots (`input`) count too.
pub fn flow_root(name: &str) -> Option<&'static str> {
    let name = name.trim();
    ROOTS.into_iter().find(|r| {
        name == *r || (name.starts_with(r) && matches!(name.as_bytes().get(r.len()), Some(b'.') | Some(b'[')))
    })
}

/// A root followed by a path (`blocks.x`, `inputs["a"]`): always a flow
/// reference, never a variable.
pub fn is_explicit_flow_ref(name: &str) -> bool {
    matches!(flow_root(name), Some(r) if name.trim().len() > r.len())
}

fn index(len: usize, n: i64) -> Option<usize> {
    let i = if n < 0 { len as i64 + n } else { n };
    (i >= 0 && (i as usize) < len).then_some(i as usize)
}

/// Follows `segs` into `v`. `length` gives the size of arrays, strings and
/// objects when there is no such key.
pub fn walk(v: &Value, segs: &[Segment]) -> Option<Value> {
    let mut cur = v;
    for (n, seg) in segs.iter().enumerate() {
        let last = n + 1 == segs.len();
        let next = match (seg, cur) {
            (Segment::Key(k), Value::Object(m)) => match m.get(k) {
                Some(x) => Some(x),
                None if k == "length" && last => return Some(Value::from(m.len())),
                None => None,
            },
            (Segment::Key(k), Value::Array(a)) => {
                if k == "length" {
                    return last.then(|| Value::from(a.len()));
                }
                k.parse::<i64>().ok().and_then(|i| index(a.len(), i)).map(|i| &a[i])
            }
            (Segment::Key(k), Value::String(s)) if k == "length" => {
                return last.then(|| Value::from(s.chars().count()));
            }
            (Segment::Index(i), Value::Array(a)) => index(a.len(), *i).map(|i| &a[i]),
            (Segment::Index(i), Value::Object(m)) => m.get(&i.to_string()),
            _ => None,
        };
        cur = next?;
    }
    Some(cur.clone())
}

fn walk_map(map: &IndexMap<String, Value>, segs: &[Segment]) -> Option<Value> {
    match segs.split_first() {
        None => Some(Value::Object(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())),
        Some((Segment::Key(k), rest)) => walk(map.get(k)?, rest),
        Some((Segment::Index(i), rest)) => walk(map.get(&i.to_string())?, rest),
    }
}

/// The latest result of a block, as seen by templates (`blocks.<key>`) and
/// scripts (`blocks[key]`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BlockRecord {
    #[serde(serialize_with = "ser_arc")]
    pub output: Arc<Value>,
    pub port: String,
    pub error: Option<String>,
    pub duration_ms: f64,
    /// How many times the block ran so far.
    pub runs: usize,
}

fn ser_arc<S: serde::Serializer>(v: &Arc<Value>, s: S) -> Result<S::Ok, S::Error> {
    v.as_ref().serialize(s)
}

impl BlockRecord {
    pub fn to_value(&self) -> Value {
        json!({
            "output": *self.output,
            "port": self.port,
            "error": self.error,
            "duration_ms": self.duration_ms,
            "runs": self.runs,
        })
    }
}

/// What flow references resolve against.
pub struct RefCtx<'a> {
    pub flow: &'a Flow,
    pub records: &'a HashMap<String, BlockRecord>,
    pub inputs: &'a IndexMap<String, Value>,
    pub vars: &'a IndexMap<String, Value>,
    /// Innermost loop: `{ item, index, count, first, last }`.
    pub loop_ctx: Option<&'a Value>,
    /// Output of the block whose edge activated the current block.
    pub input: &'a Value,
}

impl RefCtx<'_> {
    /// Evaluates a flow reference; `None` when it does not resolve.
    pub fn eval(&self, name: &str) -> Option<Value> {
        let segs = parse_path(name)?;
        let Segment::Key(root) = &segs[0] else { return None };
        let rest = &segs[1..];
        match root.as_str() {
            "blocks" => {
                let Some((key, rest)) = rest.split_first() else { return Some(self.blocks_value()) };
                let key = match key {
                    Segment::Key(k) => k.clone(),
                    Segment::Index(i) => i.to_string(),
                };
                let block = self.flow.find_block(&key)?;
                let rec = self.records.get(&block.id)?;
                match rest.split_first() {
                    None => Some(rec.to_value()),
                    Some((Segment::Key(f), more)) => match f.as_str() {
                        "output" => walk(&rec.output, more),
                        "port" | "error" | "duration_ms" | "runs" => walk(&rec.to_value(), rest),
                        // Shorthand: `blocks.x.json.id` = `blocks.x.output.json.id`.
                        _ => walk(&rec.output, rest),
                    },
                    Some(_) => walk(&rec.output, rest),
                }
            }
            "inputs" => walk_map(self.inputs, rest),
            "vars" => walk_map(self.vars, rest),
            "loop" => walk(self.loop_ctx?, rest),
            "input" => walk(self.input, rest),
            "flow" => walk(&json!({"id": self.flow.id, "name": self.flow.name}), rest),
            _ => None,
        }
    }

    /// All block records keyed by id.
    pub fn blocks_value(&self) -> Value {
        let mut m = serde_json::Map::new();
        for b in &self.flow.blocks {
            if let Some(r) = self.records.get(&b.id) {
                m.insert(b.id.clone(), r.to_value());
            }
        }
        Value::Object(m)
    }
}

/// Renders templates against flow references and variable scopes.
pub struct Renderer<'a> {
    pub refs: &'a RefCtx<'a>,
    /// Data scope holds the flow inputs and variables.
    pub scopes: &'a VariableScopes,
    pub vault: Option<&'a dyn VaultLookup>,
}

impl Renderer<'_> {
    /// Value of one reference (the text inside the braces).
    pub fn lookup(&self, name: &str) -> Option<Value> {
        self.lookup_depth(name, 0)
    }

    fn lookup_depth(&self, name: &str, depth: usize) -> Option<Value> {
        let name = name.trim();
        if is_explicit_flow_ref(name) {
            return self.refs.eval(name);
        }
        if let Some(v) = self.scopes.local.get(name) {
            return Some(Value::String(v.clone()));
        }
        if let Some(v) = self.scopes.data.get(name) {
            return Some(v.clone());
        }
        let mut resolver = Resolver::new(self.scopes);
        if let Some(v) = self.vault {
            resolver = resolver.with_vault(v);
        }
        if let Some((s, _)) = resolver.lookup(name) {
            if depth < MAX_DEPTH && s.contains("{{") {
                return Some(Value::String(self.render_inner(&s, depth + 1, &mut None)));
            }
            return Some(Value::String(s));
        }
        if flow_root(name).is_some() {
            return self.refs.eval(name);
        }
        None
    }

    pub fn render(&self, template: &str) -> String {
        self.render_inner(template, 0, &mut None)
    }

    /// Like [`render`](Self::render) and reports references that did not resolve.
    pub fn render_tracking(&self, template: &str, unresolved: &mut Vec<String>) -> String {
        let mut sink = Some(std::mem::take(unresolved));
        let out = self.render_inner(template, 0, &mut sink);
        *unresolved = sink.unwrap_or_default();
        out
    }

    fn render_inner(&self, template: &str, depth: usize, unresolved: &mut Option<Vec<String>>) -> String {
        if !template.contains("{{") {
            return template.to_string();
        }
        let mut out = String::with_capacity(template.len());
        for tok in merpati_core::variables::tokenize(template) {
            match tok.kind {
                merpati_core::variables::TokenKind::Text => out.push_str(&template[tok.range]),
                merpati_core::variables::TokenKind::Variable => {
                    let name = template[tok.name_range.clone()].trim();
                    match self.lookup_depth(name, depth) {
                        Some(v) => out.push_str(&value_to_string(&v)),
                        None => {
                            if let Some(list) = unresolved.as_mut()
                                && !list.iter().any(|n| n == name)
                            {
                                list.push(name.to_string());
                            }
                            out.push_str(&template[tok.range]);
                        }
                    }
                }
            }
        }
        out
    }

    /// A pure `{{reference}}` keeps its JSON type (null when unresolved);
    /// anything else renders to a string.
    pub fn render_value(&self, template: &str) -> Value {
        if is_pure_reference(template)
            && let Some(name) = references(template.trim()).first()
        {
            return self.lookup(name).unwrap_or(Value::Null);
        }
        Value::String(self.render(template))
    }
}

/// Flow references used in any string inside `v` (deduplicated, in order).
pub fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            for r in references(s) {
                if flow_root(r).is_some() && !out.iter().any(|x| x == r) {
                    out.push(r.to_string());
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
        Value::Object(m) => m.values().for_each(|x| collect_refs(x, out)),
        _ => {}
    }
}

pub fn to_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// JavaScript-like truthiness, plus `"false"`, `"0"` and `"null"` are false.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => {
            let t = s.trim();
            !(t.is_empty() || t == "false" || t == "0" || t == "null" || t == "undefined")
        }
        Value::Array(_) | Value::Object(_) => true,
    }
}

pub fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (to_number(a), to_number(b)) {
        return x == y;
    }
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Null, Value::Null) => true,
        _ => value_to_string(a) == value_to_string(b),
    }
}

/// Evaluates `left op right` for Condition blocks.
pub fn compare(op: CompareOp, left: &Value, right: &Value) -> Result<bool, String> {
    let ls = || value_to_string(left);
    let rs = || value_to_string(right);
    let ord = || -> Option<std::cmp::Ordering> {
        match (to_number(left), to_number(right)) {
            (Some(x), Some(y)) => x.partial_cmp(&y),
            _ => Some(ls().cmp(&rs())),
        }
    };
    Ok(match op {
        CompareOp::Eq => loose_eq(left, right),
        CompareOp::Ne => !loose_eq(left, right),
        CompareOp::Gt => ord() == Some(std::cmp::Ordering::Greater),
        CompareOp::Gte => matches!(ord(), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)),
        CompareOp::Lt => ord() == Some(std::cmp::Ordering::Less),
        CompareOp::Lte => matches!(ord(), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)),
        CompareOp::Contains | CompareOp::NotContains => {
            let found = match left {
                Value::Array(a) => a.iter().any(|x| loose_eq(x, right)),
                Value::Object(m) => m.contains_key(&rs()),
                _ => ls().contains(&rs()),
            };
            found == (op == CompareOp::Contains)
        }
        CompareOp::StartsWith => ls().starts_with(&rs()),
        CompareOp::EndsWith => ls().ends_with(&rs()),
        CompareOp::Matches => {
            let re = regex::Regex::new(&rs()).map_err(|e| format!("invalid regular expression: {e}"))?;
            re.is_match(&ls())
        }
        CompareOp::Exists | CompareOp::NotExists => {
            let exists = match left {
                Value::Null => false,
                Value::String(s) => !s.trim().is_empty() && !is_pure_reference(s),
                _ => true,
            };
            exists == (op == CompareOp::Exists)
        }
        CompareOp::Empty => is_empty_value(left),
        CompareOp::NotEmpty => !is_empty_value(left),
        CompareOp::Truthy => truthy(left),
        CompareOp::Falsy => !truthy(left),
    })
}

/// Strips a Markdown code fence around JSON (models like to add one).
pub fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
        if let Some(inner) = rest.trim_end().strip_suffix("```") {
            return inner.trim();
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        use Segment::*;
        assert_eq!(
            parse_path("blocks.Get users.output.json.items[0].id").unwrap(),
            vec![
                Key("blocks".into()),
                Key("Get users".into()),
                Key("output".into()),
                Key("json".into()),
                Key("items".into()),
                Index(0),
                Key("id".into())
            ]
        );
        assert_eq!(
            parse_path("blocks[\"a.b\"].output.headers['content-type']").unwrap(),
            vec![
                Key("blocks".into()),
                Key("a.b".into()),
                Key("output".into()),
                Key("headers".into()),
                Key("content-type".into())
            ]
        );
        assert_eq!(parse_path("x[-1]").unwrap(), vec![Key("x".into()), Index(-1)]);
        assert!(parse_path("a..b").is_none());
        assert!(parse_path("a[0").is_none());
        assert!(parse_path("").is_none());
    }

    #[test]
    fn walking() {
        let v = json!({"items": [{"id": 1}, {"id": 2}], "name": "ab"});
        let p = |s: &str| parse_path(s).unwrap();
        assert_eq!(walk(&v, &p("items[1].id")[..]), Some(json!(2)));
        assert_eq!(walk(&v, &p("items[-1].id")[..]), Some(json!(2)));
        assert_eq!(walk(&v, &p("items.0.id")[..]), Some(json!(1)));
        assert_eq!(walk(&v, &p("items.length")[..]), Some(json!(2)));
        assert_eq!(walk(&v, &p("name.length")[..]), Some(json!(2)));
        assert_eq!(walk(&v, &p("items[5]")[..]), None);
    }

    #[test]
    fn roots() {
        assert_eq!(flow_root("blocks.a"), Some("blocks"));
        assert_eq!(flow_root("input"), Some("input"));
        assert_eq!(flow_root("inputs[\"x\"]"), Some("inputs"));
        assert_eq!(flow_root("inputsx"), None);
        assert!(is_explicit_flow_ref("vars.x"));
        assert!(!is_explicit_flow_ref("vars"));
        assert!(!is_explicit_flow_ref("baseUrl"));
    }

    #[test]
    fn comparisons() {
        assert!(compare(CompareOp::Eq, &json!(200), &json!("200")).unwrap());
        assert!(compare(CompareOp::Gt, &json!("10"), &json!(9)).unwrap());
        assert!(compare(CompareOp::Contains, &json!(["a", "b"]), &json!("b")).unwrap());
        assert!(compare(CompareOp::Contains, &json!("hello"), &json!("ell")).unwrap());
        assert!(compare(CompareOp::NotContains, &json!({"a": 1}), &json!("b")).unwrap());
        assert!(compare(CompareOp::Matches, &json!("abc123"), &json!("^[a-z]+\\d+$")).unwrap());
        assert!(compare(CompareOp::Matches, &json!("x"), &json!("(")).is_err());
        assert!(compare(CompareOp::Exists, &json!("x"), &Value::Null).unwrap());
        assert!(!compare(CompareOp::Exists, &json!("{{nope}}"), &Value::Null).unwrap());
        assert!(compare(CompareOp::Empty, &json!([]), &Value::Null).unwrap());
        assert!(compare(CompareOp::Falsy, &json!("false"), &Value::Null).unwrap());
        assert!(compare(CompareOp::Eq, &json!(true), &json!("true")).unwrap());
    }

    #[test]
    fn fences() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence(" {\"a\":1} "), "{\"a\":1}");
    }
}
