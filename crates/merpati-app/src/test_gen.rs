//! Script code generated from a response: `pm.test` blocks for a value of
//! the body, a `pm.*.set` line for a value, and a whole test suite (status,
//! time, content type, JSON schema) for "Generate tests".
//!
//! Paths are the JSON paths of the body viewer (`$.data[0].id`,
//! `$["a key"]`); they become JavaScript accessors on `pm.response.json()`.

use serde_json::Value;

/// One step of a JSON path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// Parses `$.data[0].id`, `data[0].id`, `$["a b"].c` (the leading `$` is
/// optional).
pub fn parse_path(path: &str) -> Vec<Seg> {
    let s = path.trim();
    let s = s.strip_prefix('$').unwrap_or(s);
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let ident = |i: &mut usize| {
        let start = *i;
        while *i < c.len() && c[*i] != '.' && c[*i] != '[' {
            *i += 1;
        }
        c[start..*i].iter().collect::<String>()
    };
    while i < c.len() {
        match c[i] {
            '.' => {
                i += 1;
                let k = ident(&mut i);
                if !k.is_empty() {
                    out.push(Seg::Key(k));
                }
            }
            '[' => {
                i += 1;
                if let Some(&quote) = c.get(i).filter(|q| **q == '"' || **q == '\'') {
                    // `["a b"]` or `['a b']`, with backslash escapes.
                    i += 1;
                    let mut key = String::new();
                    while i < c.len() && c[i] != quote {
                        if c[i] == '\\' && i + 1 < c.len() {
                            i += 1;
                            match c[i] {
                                'n' => key.push('\n'),
                                'r' => key.push('\r'),
                                't' => key.push('\t'),
                                'u' => {
                                    let hex: String = c[i + 1..(i + 5).min(c.len())].iter().collect();
                                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                                        key.push(ch);
                                        i += 4;
                                    }
                                }
                                other => key.push(other),
                            }
                        } else {
                            key.push(c[i]);
                        }
                        i += 1;
                    }
                    out.push(Seg::Key(key));
                    while i < c.len() && c[i] != ']' {
                        i += 1;
                    }
                } else {
                    let start = i;
                    while i < c.len() && c[i] != ']' {
                        i += 1;
                    }
                    let inner: String = c[start..i].iter().collect();
                    out.push(match inner.trim().parse() {
                        Ok(n) => Seg::Index(n),
                        Err(_) => Seg::Key(inner),
                    });
                }
                i += 1;
            }
            _ => {
                let k = ident(&mut i);
                out.push(Seg::Key(k));
            }
        }
    }
    out
}

/// The value at `path` in `root`.
pub fn lookup<'a>(root: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    path.iter().try_fold(root, |v, seg| match seg {
        Seg::Key(k) => v.get(k.as_str()),
        Seg::Index(i) => v.get(*i),
    })
}

fn is_js_ident(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// A JavaScript string literal.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Accessor after `pm.response.json()`: `.data[0].id`, `["a b"]`.
pub fn js_path(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) if is_js_ident(k) => {
                out.push('.');
                out.push_str(k);
            }
            Seg::Key(k) => {
                out.push('[');
                out.push_str(&js_str(k));
                out.push(']');
            }
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

/// The path as people read it (`data[0].id`); `body` for the root.
pub fn display_path(path: &[Seg]) -> String {
    let js = js_path(path);
    match js.strip_prefix('.') {
        Some(rest) => rest.to_string(),
        None if js.is_empty() => "body".to_string(),
        None => js,
    }
}

/// A variable name for the value at `path`: its key (`id`), or the key of
/// the array it is in.
pub fn suggested_name(path: &[Seg]) -> String {
    let key = path
        .iter()
        .rev()
        .find_map(|s| match s {
            Seg::Key(k) => Some(k.as_str()),
            Seg::Index(_) => None,
        })
        .unwrap_or("value");
    let name: String =
        key.chars().map(|c| if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') { c } else { '_' }).collect();
    if name.trim_matches('_').is_empty() { "value".to_string() } else { name }
}

/// Where "Set as variable" writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VarScope {
    /// The active environment (id).
    Environment(String),
    /// The request's collection (id).
    Collection(String),
    Globals,
}

impl VarScope {
    fn js_object(&self) -> &'static str {
        match self {
            VarScope::Environment(_) => "pm.environment",
            VarScope::Collection(_) => "pm.collectionVariables",
            VarScope::Globals => "pm.globals",
        }
    }
}

/// `pm.environment.set("id", pm.response.json().data[0].id);`
pub fn set_variable_line(scope: &VarScope, name: &str, path: &[Seg]) -> String {
    format!("{}.set({}, pm.response.json(){});", scope.js_object(), js_str(name), js_path(path))
}

/// Text cut to `max` characters with an ellipsis.
fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

/// Strings longer than this are checked for their type, not their text.
const MAX_LITERAL: usize = 200;
/// Objects with more keys than this are checked for their key count.
const MAX_KEYS: usize = 40;

/// A readable `pm.test` that checks the value at `path`: equality for
/// scalars, type and length for arrays, type and keys for objects.
pub fn value_test(path: &[Seg], value: &Value) -> String {
    let target = format!("pm.response.json(){}", js_path(path));
    let shown = display_path(path);
    let (name, assertion) = match value {
        Value::Null => (format!("{shown} is null"), "to.be.null".to_string()),
        Value::String(s) if s.chars().count() > MAX_LITERAL => {
            (format!("{shown} is a non-empty string"), "to.be.a(\"string\").that.is.not.empty".to_string())
        }
        Value::String(s) => (format!("{shown} is {}", short(&js_str(s), 48)), format!("to.eql({})", js_str(s))),
        Value::Number(_) | Value::Bool(_) => (format!("{shown} is {value}"), format!("to.eql({value})")),
        Value::Array(a) if a.is_empty() => {
            (format!("{shown} is an empty array"), "to.be.an(\"array\").that.is.empty".into())
        }
        Value::Array(a) => {
            let n = a.len();
            let items = if n == 1 { "1 item".to_string() } else { format!("{n} items") };
            (format!("{shown} has {items}"), format!("to.be.an(\"array\").that.has.lengthOf({n})"))
        }
        Value::Object(m) if m.is_empty() => {
            (format!("{shown} is an empty object"), "to.be.an(\"object\").that.is.empty".into())
        }
        Value::Object(m) if m.len() > MAX_KEYS => {
            let code = format!(
                "pm.expect({target}).to.be.an(\"object\");\n    pm.expect(Object.keys({target})).to.have.lengthOf({});",
                m.len()
            );
            return test_block(&format!("{shown} has {} keys", m.len()), &code);
        }
        Value::Object(m) => {
            let keys: Vec<&str> = m.keys().map(String::as_str).collect();
            let listed = if keys.len() > 5 { format!("{}, …", keys[..5].join(", ")) } else { keys.join(", ") };
            let args = keys.iter().map(|k| js_str(k)).collect::<Vec<_>>().join(", ");
            (format!("{shown} has keys {listed}"), format!("to.be.an(\"object\").that.includes.all.keys({args})"))
        }
    };
    test_block(&name, &format!("pm.expect({target}).{assertion};"))
}

fn test_block(name: &str, body: &str) -> String {
    format!("pm.test({}, () => {{\n    {body}\n}});\n", js_str(name))
}

/// What "Generate tests" needs to know about a response.
pub struct ResponseFacts<'a> {
    pub status: u16,
    pub status_text: &'a str,
    pub time_ms: f64,
    /// The `Content-Type` header, if any.
    pub content_type: &'a str,
    /// The parsed body, if it is JSON (and not too large).
    pub json: Option<Value>,
    pub body_empty: bool,
}

/// A bound a little above `ms` that reads well: at least 500 ms, three
/// times the measured time, rounded up to 100/500/1000 ms steps.
pub fn time_bound(ms: f64) -> u64 {
    let v = (ms * 3.0).max(500.0).ceil() as u64;
    let step = if v <= 1000 {
        100
    } else if v <= 5000 {
        500
    } else {
        1000
    };
    v.div_ceil(step) * step
}

/// Objects nested deeper than this get only a type.
const SCHEMA_DEPTH: usize = 4;

/// A JSON schema of `v`: types of every key (all required), arrays
/// described by their first item; `null` values accept anything.
pub fn json_schema(v: &Value) -> Value {
    schema_at(v, 0)
}

fn schema_at(v: &Value, depth: usize) -> Value {
    use serde_json::json;
    match v {
        Value::Null => json!({}),
        Value::Bool(_) => json!({"type": "boolean"}),
        Value::Number(_) => json!({"type": "number"}),
        Value::String(_) => json!({"type": "string"}),
        Value::Array(items) => match items.first() {
            Some(first) if depth < SCHEMA_DEPTH => json!({"type": "array", "items": schema_at(first, depth + 1)}),
            _ => json!({"type": "array"}),
        },
        Value::Object(m) if depth >= SCHEMA_DEPTH || m.is_empty() || m.len() > MAX_KEYS => json!({"type": "object"}),
        Value::Object(m) => {
            let props: serde_json::Map<String, Value> =
                m.iter().map(|(k, v)| (k.clone(), schema_at(v, depth + 1))).collect();
            let required: Vec<Value> = m.keys().map(|k| Value::String(k.clone())).collect();
            json!({"type": "object", "required": required, "properties": props})
        }
    }
}

/// JSON with a 4-space indent (arrays of short scalars on one line), every
/// line after the first indented by `indent` more spaces.
fn pretty_indented(v: &Value, indent: &str) -> String {
    fn write(v: &Value, level: usize, base: &str, out: &mut String) {
        let pad = |n: usize| format!("{base}{}", "    ".repeat(n));
        match v {
            Value::Object(m) if !m.is_empty() => {
                out.push_str("{\n");
                for (i, (k, child)) in m.iter().enumerate() {
                    out.push_str(&pad(level + 1));
                    out.push_str(&js_str(k));
                    out.push_str(": ");
                    write(child, level + 1, base, out);
                    out.push_str(if i + 1 < m.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad(level));
                out.push('}');
            }
            Value::Array(a) if !a.is_empty() => {
                let inline = a.iter().all(|x| !x.is_object() && !x.is_array())
                    && a.iter().map(|x| x.to_string().len() + 2).sum::<usize>() <= 72;
                if inline {
                    let items: Vec<String> = a.iter().map(Value::to_string).collect();
                    out.push_str(&format!("[{}]", items.join(", ")));
                    return;
                }
                out.push_str("[\n");
                for (i, child) in a.iter().enumerate() {
                    out.push_str(&pad(level + 1));
                    write(child, level + 1, base, out);
                    out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
                }
                out.push_str(&pad(level));
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(v, 0, indent, &mut out);
    out
}

/// The tests of "Generate tests", and how many there are.
pub fn generate(f: &ResponseFacts) -> (String, usize) {
    let mut blocks = Vec::new();
    blocks.push(test_block(
        &format!("Status code is {}", f.status),
        &format!("pm.response.to.have.status({});", f.status),
    ));
    let bound = time_bound(f.time_ms);
    blocks.push(test_block(
        &format!("Response time is below {bound} ms"),
        &format!("pm.expect(pm.response.responseTime).to.be.below({bound});"),
    ));
    let media = f.content_type.split(';').next().unwrap_or("").trim();
    if !media.is_empty() {
        blocks.push(test_block(
            &format!("Content-Type is {media}"),
            &format!("pm.expect(pm.response.headers.get(\"Content-Type\")).to.include({});", js_str(media)),
        ));
    }
    match &f.json {
        Some(v) => {
            let schema = pretty_indented(&json_schema(v), "    ");
            blocks.push(test_block(
                "Body matches the JSON schema",
                &format!("const schema = {schema};\n    pm.response.to.have.jsonSchema(schema);"),
            ));
        }
        None if !f.body_empty => {
            blocks.push(test_block("Body is not empty", "pm.expect(pm.response.text()).to.not.be.empty;"));
        }
        None => {}
    }
    let reason = if f.status_text.is_empty() { String::new() } else { format!(" {}", f.status_text) };
    let header = format!("// Generated from a {}{reason} response.\n", f.status);
    (format!("{header}{}", blocks.join("\n")), blocks.len())
}

/// Appends `code` to a script with a blank line between them.
pub fn append_code(script: &mut String, code: &str) {
    let trimmed = script.trim_end();
    if trimmed.is_empty() {
        script.clear();
    } else {
        script.truncate(trimmed.len());
        script.push_str("\n\n");
    }
    script.push_str(code.trim_end());
    script.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::response::ResponseBody;
    use merpati_core::script::{ScriptEngine, ScriptEvent, ScriptJob, ScriptRequest, ScriptResponse, ScriptSource};
    use serde_json::json;

    #[test]
    fn paths_parse_and_become_accessors() {
        let p = parse_path("$.data[0].id");
        assert_eq!(p, vec![Seg::Key("data".into()), Seg::Index(0), Seg::Key("id".into())]);
        assert_eq!(js_path(&p), ".data[0].id");
        assert_eq!(display_path(&p), "data[0].id");
        assert_eq!(suggested_name(&p), "id");
        let p = parse_path("$[\"user-name\"].first[2]");
        assert_eq!(p, vec![Seg::Key("user-name".into()), Seg::Key("first".into()), Seg::Index(2)]);
        assert_eq!(js_path(&p), "[\"user-name\"].first[2]");
        assert_eq!(display_path(&p), "[\"user-name\"].first[2]");
        assert_eq!(suggested_name(&p), "first");
        // Keys that are not identifiers in JavaScript.
        assert_eq!(js_path(&parse_path("$.123abc")), "[\"123abc\"]");
        assert_eq!(js_path(&parse_path("data.items")), ".data.items");
        assert!(parse_path("$").is_empty());
        assert_eq!(display_path(&[]), "body");
        assert_eq!(suggested_name(&[Seg::Index(0)]), "value");
        assert_eq!(parse_path("$[\"a\\\"b\"]"), vec![Seg::Key("a\"b".into())]);
        // The body viewer's form (`jsonpath::push_key`).
        let p = parse_path("$.data[0]['user name']['it\\'s'].x");
        assert_eq!(
            p,
            vec![
                Seg::Key("data".into()),
                Seg::Index(0),
                Seg::Key("user name".into()),
                Seg::Key("it's".into()),
                Seg::Key("x".into())
            ]
        );
        assert_eq!(js_path(&p), ".data[0][\"user name\"][\"it's\"].x");
        assert_eq!(parse_path("$['a\\nb\\u0041']"), vec![Seg::Key("a\nbA".into())]);
    }

    #[test]
    fn lookup_follows_paths() {
        let v = json!({"data": [{"id": 7, "a b": true}]});
        assert_eq!(lookup(&v, &parse_path("$.data[0].id")), Some(&json!(7)));
        assert_eq!(lookup(&v, &parse_path("$.data[0][\"a b\"]")), Some(&json!(true)));
        assert_eq!(lookup(&v, &parse_path("$.data[3]")), None);
        assert_eq!(lookup(&v, &parse_path("$")), Some(&v));
    }

    #[test]
    fn set_lines_match_the_scope() {
        let p = parse_path("$.data[0].id");
        assert_eq!(
            set_variable_line(&VarScope::Environment("e".into()), "id", &p),
            "pm.environment.set(\"id\", pm.response.json().data[0].id);"
        );
        assert_eq!(
            set_variable_line(&VarScope::Collection("c".into()), "id", &p),
            "pm.collectionVariables.set(\"id\", pm.response.json().data[0].id);"
        );
        assert_eq!(set_variable_line(&VarScope::Globals, "t", &[]), "pm.globals.set(\"t\", pm.response.json());");
    }

    #[test]
    fn value_tests_read_well() {
        let t = value_test(&parse_path("$.data[0].id"), &json!(1));
        assert_eq!(
            t,
            "pm.test(\"data[0].id is 1\", () => {\n    pm.expect(pm.response.json().data[0].id).to.eql(1);\n});\n"
        );
        let t = value_test(&parse_path("$.name"), &json!("Ada"));
        assert!(t.contains("pm.test(\"name is \\\"Ada\\\"\""), "{t}");
        assert!(t.contains(".to.eql(\"Ada\")"), "{t}");
        assert!(value_test(&parse_path("$.tags"), &json!([1, 2])).contains("has 2 items"));
        assert!(
            value_test(&parse_path("$.user"), &json!({"id": 1, "name": "x"})).contains("all.keys(\"id\", \"name\")")
        );
        assert!(value_test(&parse_path("$.x"), &Value::Null).contains("to.be.null"));
        assert!(value_test(&parse_path("$.bio"), &json!("x".repeat(300))).contains("non-empty string"));
    }

    #[test]
    fn time_bounds_are_round() {
        assert_eq!(time_bound(12.0), 500);
        assert_eq!(time_bound(240.0), 800);
        assert_eq!(time_bound(700.0), 2500);
        assert_eq!(time_bound(2100.0), 7000);
    }

    #[test]
    fn appending_keeps_one_blank_line() {
        let mut s = String::new();
        append_code(&mut s, "a();\n");
        assert_eq!(s, "a();\n");
        append_code(&mut s, "b();");
        assert_eq!(s, "a();\n\nb();\n");
    }

    #[test]
    fn schema_describes_the_body() {
        let s = json_schema(&json!({"id": 1, "tags": ["a"], "owner": null, "items": []}));
        assert_eq!(s["required"], json!(["id", "tags", "owner", "items"]));
        assert_eq!(s["properties"]["id"], json!({"type": "number"}));
        assert_eq!(s["properties"]["tags"], json!({"type": "array", "items": {"type": "string"}}));
        assert_eq!(s["properties"]["owner"], json!({}));
        assert_eq!(s["properties"]["items"], json!({"type": "array"}));
    }

    /// Runs post-response `code` against a response through the script
    /// engine; returns `(name, passed, error)` per test.
    fn run(code: &str, status: u16, ct: &str, body: &str) -> Vec<(String, bool, Option<String>)> {
        let mut job = ScriptJob::new(
            ScriptEvent::PostResponse,
            vec![ScriptSource { location: "request \"gen\"".into(), code: code.into() }],
            ScriptRequest { method: "GET".into(), url: "http://localhost/x".into(), ..Default::default() },
        );
        job.response = Some(ScriptResponse {
            code: status,
            status: "OK".into(),
            headers: vec![("content-type".into(), ct.into())],
            response_size: body.len() as u64,
            body: ResponseBody::Memory(body.to_string().into()),
            response_time_ms: 42.0,
        });
        let out = merpati_script::BoaEngine::new().run(job);
        assert!(out.errors.is_empty(), "script errors: {:?}", out.errors);
        out.tests.into_iter().map(|t| (t.name, t.passed, t.error)).collect()
    }

    /// The generated code runs in Merpati's engine and passes on the
    /// response it came from, and the schema catches a changed body.
    #[test]
    fn generated_tests_pass_in_the_engine() {
        let body = json!({
            "data": [{"id": 1, "name": "Ada", "tags": ["x"], "active": true, "manager": null}],
            "page": 1,
            "meta": {"total": 1, "next": null}
        });
        let text = body.to_string();
        let facts = ResponseFacts {
            status: 200,
            status_text: "OK",
            time_ms: 42.0,
            content_type: "application/json; charset=utf-8",
            json: Some(body.clone()),
            body_empty: false,
        };
        let (code, n) = generate(&facts);
        assert_eq!(n, 4);
        assert!(code.contains("\"required\": [\"data\", \"page\", \"meta\"],"), "{code}");
        let results = run(&code, 200, "application/json; charset=utf-8", &text);
        assert_eq!(results.len(), 4, "{code}\n{results:?}");
        assert!(results.iter().all(|r| r.1), "{code}\n{results:?}");
        // A body of another shape fails the schema test only.
        let bad = json!({"data": [{"id": "1", "name": "Ada", "tags": [], "active": true, "manager": 3}], "page": 1, "meta": {}});
        let results = run(&code, 200, "application/json", &bad.to_string());
        let failed: Vec<&str> = results.iter().filter(|r| !r.1).map(|r| r.0.as_str()).collect();
        assert_eq!(failed, vec!["Body matches the JSON schema"], "{results:?}");

        // Value tests and set lines run too.
        let mut script = String::new();
        for path in
            ["$.data[0].id", "$.data[0].name", "$.data[0].tags", "$.data[0]", "$.data[0].active", "$.meta.next", "$"]
        {
            let segs = parse_path(path);
            append_code(&mut script, &value_test(&segs, lookup(&body, &segs).unwrap()));
        }
        append_code(&mut script, &set_variable_line(&VarScope::Globals, "id", &parse_path("$.data[0].id")));
        let results = run(&script, 200, "application/json", &text);
        assert_eq!(results.len(), 7, "{script}\n{results:?}");
        assert!(results.iter().all(|r| r.1), "{script}\n{results:?}");

        // Text bodies get a non-empty check instead of a schema.
        let facts = ResponseFacts {
            status: 404,
            status_text: "Not Found",
            time_ms: 900.0,
            content_type: "text/html",
            json: None,
            body_empty: false,
        };
        let (code, n) = generate(&facts);
        assert_eq!(n, 4);
        let results = run(&code, 404, "text/html", "<p>gone</p>");
        assert!(results.iter().all(|r| r.1), "{code}\n{results:?}");
    }
}
