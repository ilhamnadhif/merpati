//! Smithy JSON AST (`model.json`, the output of `smithy build`).

use indexmap::IndexMap;
use serde_json::{Map, Value};

use super::resolve::{RawApply, RawModel, RawShape};
use super::{Diagnostic, Member, Pos, Shape, ShapeKind, ShapeRef};

pub(super) fn looks_like_ast(t: &str) -> bool {
    match serde_json::from_str::<Value>(t) {
        Ok(v) => is_ast(&v),
        Err(_) => false,
    }
}

pub(crate) fn is_ast(v: &Value) -> bool {
    v.get("smithy").is_some_and(Value::is_string) && v.get("shapes").is_none_or(Value::is_object)
}

/// Maps byte offsets to line/column for approximate positions.
struct Locator<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Locator<'a> {
    fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Locator { text, line_starts }
    }

    fn pos_of(&self, offset: usize) -> Pos {
        let line = self.line_starts.partition_point(|s| *s <= offset);
        let start = self.line_starts[line - 1];
        let column = self.text[start..offset].chars().count() + 1;
        Pos::new(line as u32, column as u32)
    }

    /// Offset of the object key `"key":` at or after `from`.
    fn find_key(&self, key: &str, from: usize) -> Option<usize> {
        let needle = format!("\"{key}\"");
        let mut at = from;
        while let Some(i) = self.text.get(at..)?.find(&needle) {
            let start = at + i;
            let after = &self.text[start + needle.len()..];
            if after.trim_start().starts_with(':') {
                return Some(start);
            }
            at = start + needle.len();
        }
        None
    }
}

pub(super) fn parse(text: &str) -> Result<RawModel, Diagnostic> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| Diagnostic::error(Pos::new(e.line() as u32, e.column() as u32), format!("invalid JSON: {e}")))?;
    parse_value(text, &v)
}

/// [`parse`] for JSON that was already parsed (`v` is `text` parsed).
pub(super) fn parse_value(text: &str, v: &Value) -> Result<RawModel, Diagnostic> {
    if !is_ast(v) {
        return Err(Diagnostic::error(Pos::new(1, 1), "not a Smithy JSON AST (missing `\"smithy\": \"2.0\"`)"));
    }
    let loc = Locator::new(text);
    let mut raw = RawModel { version: v["smithy"].as_str().unwrap_or_default().to_string(), ..Default::default() };
    if let Some(meta) = v.get("metadata").and_then(Value::as_object) {
        let meta_at = loc.find_key("metadata", 0).unwrap_or(0);
        for (key, value) in meta {
            let pos = loc.pos_of(loc.find_key(key, meta_at).unwrap_or(meta_at));
            raw.metadata.push((key.clone(), value.clone(), pos));
        }
    }
    let shapes_at = loc.find_key("shapes", 0).unwrap_or(0);
    let empty = Map::new();
    let shapes = v.get("shapes").and_then(Value::as_object).unwrap_or(&empty);
    // Keys are visited in document order, so each search starts after the
    // previous shape.
    let mut cursor = shapes_at;
    for (id, def) in shapes {
        let offset = loc.find_key(id, cursor).or_else(|| loc.find_key(id, shapes_at)).unwrap_or(cursor);
        cursor = offset.max(cursor);
        let pos = loc.pos_of(offset);
        if !id.contains('#') {
            return Err(Diagnostic::error(pos, format!("shape id `{id}` must be absolute (`namespace#Name`)")));
        }
        if raw.namespace.is_empty() {
            raw.namespace = super::namespace_of(id).to_string();
        }
        let ty = def.get("type").and_then(Value::as_str).unwrap_or("");
        if ty == "apply" {
            let traits = obj(def, "traits").iter().map(|(k, v)| (k.clone(), v.clone(), pos)).collect();
            raw.applies.push(RawApply { target: id.clone(), pos, traits });
            continue;
        }
        let Some(kind) = ShapeKind::from_keyword(ty) else {
            return Err(Diagnostic::error(pos, format!("shape `{id}` has unknown type `{ty}`")));
        };
        let mut shape = Shape::new(id.clone(), kind, pos);
        shape.traits = obj(def, "traits").into_iter().collect();
        let target = |v: &Value, what: &str| -> Result<ShapeRef, Diagnostic> {
            match v.get("target").and_then(Value::as_str) {
                Some(t) => Ok(ShapeRef { id: t.to_string(), pos }),
                None => Err(Diagnostic::error(pos, format!("`{what}` of `{id}` needs a `target`"))),
            }
        };
        let list = |key: &str| -> Result<Vec<ShapeRef>, Diagnostic> {
            match def.get(key) {
                None => Ok(Vec::new()),
                Some(Value::Array(a)) => a.iter().map(|x| target(x, key)).collect(),
                Some(_) => Err(Diagnostic::error(pos, format!("`{key}` of `{id}` must be an array"))),
            }
        };
        let refs_map = |key: &str| -> Result<IndexMap<String, ShapeRef>, Diagnostic> {
            obj(def, key).iter().map(|(k, x)| Ok((k.clone(), target(x, key)?))).collect()
        };
        shape.mixins = list("mixins")?;
        let mut member_defs: Vec<(String, &Value)> = Vec::new();
        match kind {
            ShapeKind::List | ShapeKind::Set => {
                if let Some(m) = def.get("member") {
                    member_defs.push(("member".into(), m));
                }
            }
            ShapeKind::Map => {
                for k in ["key", "value"] {
                    if let Some(m) = def.get(k) {
                        member_defs.push((k.into(), m));
                    }
                }
            }
            _ => {
                if let Some(Value::Object(ms)) = def.get("members") {
                    member_defs.extend(ms.iter().map(|(k, v)| (k.clone(), v)));
                }
            }
        }
        for (name, m) in member_defs {
            let mpos = loc.find_key(&name, offset).map(|o| loc.pos_of(o)).unwrap_or(pos);
            let target = match m.get("target").and_then(Value::as_str) {
                Some(t) => t.to_string(),
                None => return Err(Diagnostic::error(mpos, format!("member `{name}` of `{id}` needs a `target`"))),
            };
            let traits = obj(m, "traits").into_iter().collect();
            shape.members.insert(name.clone(), Member { name, target, traits, trait_pos: IndexMap::new(), pos: mpos });
        }
        if let Some(x) = def.get("input") {
            shape.input = Some(target(x, "input")?);
        }
        if let Some(x) = def.get("output") {
            shape.output = Some(target(x, "output")?);
        }
        shape.errors = list("errors")?;
        shape.version = def.get("version").and_then(Value::as_str).map(str::to_string);
        shape.operations = list("operations")?;
        shape.resources = list("resources")?;
        shape.collection_operations = list("collectionOperations")?;
        shape.identifiers = refs_map("identifiers")?;
        shape.properties = refs_map("properties")?;
        for k in ["create", "put", "read", "update", "delete", "list"] {
            if let Some(x) = def.get(k) {
                shape.lifecycle.insert(k.to_string(), target(x, k)?);
            }
        }
        if let Some(rename) = def.get("rename") {
            shape.traits.insert("smithy.api#rename".into(), rename.clone());
        }
        raw.shapes.push(RawShape::new(shape));
    }
    Ok(raw)
}

fn obj(v: &Value, key: &str) -> Map<String, Value> {
    v.get(key).and_then(Value::as_object).cloned().unwrap_or_default()
}
