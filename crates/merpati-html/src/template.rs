//! Handlebars templating with Postman visualizer semantics.

use handlebars::{
    BlockContext, BlockParams, Context, Handlebars, Helper, HelperDef, HelperResult, Output, RenderContext,
    RenderErrorReason, Renderable, handlebars_helper,
};
use serde_json::Value;

use crate::Error;

// `x.length` of Handlebars.js (arrays, strings and `length` properties).
handlebars_helper!(js_length: |x: Json| match x {
    Value::Array(a) => Value::from(a.len()),
    Value::String(s) => Value::from(s.encode_utf16().count()),
    Value::Object(o) => o.get("length").cloned().unwrap_or(Value::Null),
    _ => Value::Null,
});

/// How Handlebars.js prints a value (`String(value)`): arrays joined with
/// `,` (`null` items empty), objects as `[object Object]`.
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_owned(),
        other => handlebars::JsonRender::render(other),
    }
}

handlebars_helper!(js_str: |x: Json| Value::String(js_string(x)));

/// `isEmpty` of Handlebars.js: what `#with` skips. Unlike handlebars-rust,
/// `{}` and `0` are not empty.
fn js_is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(_) | Value::Object(_) => false,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
    }
}

/// `#if` / `#unless` of Handlebars.js: falsy are `false`, missing/`null`,
/// `""`, `[]` and `0` (unless `includeZero=true`); `{}` is truthy.
struct JsIf {
    positive: bool,
}

impl HelperDef for JsIf {
    fn call<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        r: &'reg Handlebars<'reg>,
        ctx: &'rc Context,
        rc: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        let name = if self.positive { "if" } else { "unless" };
        let param = h.param(0).ok_or(RenderErrorReason::ParamNotFoundForIndex(name, 0))?;
        let include_zero = h.hash_get("includeZero").and_then(|v| v.value().as_bool()).unwrap_or(false);
        let truthy = match param.value() {
            Value::Number(n) => include_zero || n.as_f64().is_some_and(|f| f != 0.0),
            v => !js_is_empty(v),
        };
        match if truthy == self.positive { h.template() } else { h.inverse() } {
            Some(t) => t.render(r, ctx, rc, out),
            None => Ok(()),
        }
    }
}

/// `#with` of Handlebars.js (the block renders unless the value is empty).
struct JsWith;

impl HelperDef for JsWith {
    fn call<'reg: 'rc, 'rc>(
        &self,
        h: &Helper<'rc>,
        r: &'reg Handlebars<'reg>,
        ctx: &'rc Context,
        rc: &mut RenderContext<'reg, 'rc>,
        out: &mut dyn Output,
    ) -> HelperResult {
        let param = h.param(0).ok_or(RenderErrorReason::ParamNotFoundForIndex("with", 0))?;
        if js_is_empty(param.value()) {
            return match h.inverse() {
                Some(t) => t.render(r, ctx, rc, out),
                None => Ok(()),
            };
        }
        // As handlebars-rust's own `with`.
        let mut block = BlockContext::new();
        match param.context_path() {
            Some(path) => block.base_path_mut().clone_from(path),
            None => block.set_base_value(param.value().clone()),
        }
        if let Some(parent) = rc.block() {
            block.local_variables_mut().clone_from(parent.local_variables());
        }
        if let Some(name) = h.block_param() {
            let mut params = BlockParams::new();
            if param.context_path().is_some() {
                params.add_path(name, Vec::new())?;
            } else {
                params.add_value(name, param.value().clone())?;
            }
            block.set_block_params(params);
        }
        rc.push_block(block);
        let result = match h.template() {
            Some(t) => t.render(r, ctx, rc, out),
            None => Ok(()),
        };
        rc.pop_block();
        result
    }
}

/// Helpers handlebars-rust has and Handlebars.js does not. Handlebars.js
/// prints a data field of the same name for a bare `{{len}}`.
const EXTRA_HELPERS: [&str; 11] = ["raw", "eq", "ne", "gt", "gte", "lt", "lte", "and", "or", "not", "len"];

/// Built-in helpers of Handlebars.js: a bare `{{each}}` is a helper call
/// there too.
const JS_HELPERS: [&str; 6] = ["if", "unless", "each", "with", "lookup", "log"];

/// Length of the comment (`{{! }}`, `{{!-- --}}`) or raw block
/// (`{{{{raw}}}} ... {{{{/raw}}}}`) at the start of `tag`, which is copied
/// as is.
fn verbatim_len(tag: &str) -> Option<usize> {
    let end = if tag.starts_with("{{!--") {
        tag.find("--}}").map(|e| e + 4)
    } else if tag.starts_with("{{!") {
        tag.find("}}").map(|e| e + 2)
    } else {
        let open = tag.strip_prefix("{{{{")?;
        let name = open.split(|c: char| c.is_whitespace() || c == '}').next().unwrap_or("");
        let close = format!("{{{{{{{{/{name}}}}}}}}}");
        tag.find(&close).map(|e| e + close.len())
    };
    Some(end.unwrap_or(tag.len()))
}

/// Makes value mustaches print like Handlebars.js (see [`js_string`]):
/// `{{path}}` becomes `{{js_str path}}` and `{{lookup a b}}` becomes
/// `{{js_str (lookup a b)}}`. A bare extra helper name (`{{len}}`,
/// `{{{raw}}}`, never a valid helper call) reads the data field
/// (`{{js_str this.len}}`), as in Handlebars.js.
fn rewrite_values(template: &str) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let tag = &rest[start..];
        // `\{{` is an escaped mustache.
        let verbatim = if rest[..start].ends_with('\\') { Some(2) } else { verbatim_len(tag) };
        if let Some(n) = verbatim {
            out.push_str(&tag[..n]);
            rest = &tag[n..];
            continue;
        }
        let Some(close) = tag.find("}}") else { break };
        let inner = &tag[2..close];
        // A `~` (whitespace control), then one `{` or `&` (unescaped).
        let tilde = usize::from(inner.starts_with('~'));
        let triple = inner[tilde..].starts_with('{');
        let sigil = tilde + usize::from(triple || inner[tilde..].starts_with('&'));
        let body = &inner[sigil..];
        let name = body.trim();
        let mut name = name.strip_suffix('~').unwrap_or(name).trim_end();
        if triple {
            // `{{~{x}~}}`: the closing brace comes before the `~`.
            name = name.strip_suffix('}').unwrap_or(name).trim_end();
        }
        let path = name.starts_with(|c: char| c.is_alphanumeric() || matches!(c, '_' | '$' | '@' | '.' | '[' | '-'))
            && name.chars().all(is_path_char);
        let replacement = if path && EXTRA_HELPERS.contains(&name) {
            Some(format!("js_str this.{name}"))
        } else if path && name != "else" && !JS_HELPERS.contains(&name) {
            // `{{.}}` is `{{this}}` (handlebars-rust only knows the latter).
            Some(format!("js_str {}", if name == "." { "this" } else { name }))
        } else if name.strip_prefix("lookup").is_some_and(|r| r.starts_with(char::is_whitespace)) {
            Some(format!("js_str ({name})"))
        } else {
            None
        };
        match replacement {
            // handlebars-rust takes only a name after `&`: use `{{{ }}}`.
            Some(r) if !triple && sigil > tilde => {
                out.push_str("{{");
                out.push_str(&inner[..tilde]);
                out.push('{');
                out.push_str(&r);
                out.push('}');
                if body.trim_end().ends_with('~') {
                    out.push('~');
                }
            }
            Some(r) => {
                // Replace the expression, keeping `~`, `{`, `&` and spacing.
                let pos = sigil + body.len() - body.trim_start().len();
                out.push_str("{{");
                out.push_str(&inner[..pos]);
                out.push_str(&r);
                out.push_str(&inner[pos + name.len()..]);
            }
            None => out.push_str(&tag[..close]),
        }
        rest = &tag[close..];
    }
    out.push_str(rest);
    out
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '$' | '@' | '.' | '/' | '[' | ']' | '-')
}

/// Rewrites `path.length` (JavaScript property access, very common in
/// Postman templates) into calls of the `js_length` helper, since
/// handlebars-rust cannot index arrays with `length`.
fn rewrite_length(template: &str) -> String {
    if !template.contains(".length") {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        // `\{{` is an escaped mustache.
        if rest[..start].ends_with('\\') {
            out.push_str(&rest[..start + 2]);
            rest = &rest[start + 2..];
            continue;
        }
        out.push_str(&rest[..start]);
        let tag = &rest[start..];
        if let Some(end) = verbatim_len(tag) {
            out.push_str(&tag[..end]);
            rest = &tag[end..];
            continue;
        }
        let Some(close) = tag.find("}}") else {
            out.push_str(tag);
            return out;
        };
        let inner = &tag[2..close];
        out.push_str("{{");
        out.push_str(&rewrite_mustache(inner));
        out.push_str("}}");
        rest = &tag[close + 2..];
    }
    out.push_str(rest);
    out
}

fn rewrite_mustache(inner: &str) -> String {
    let body_start =
        inner.find(|c: char| !matches!(c, '{' | '~' | '#' | '^' | '&' | '/' | '>' | '*') && !c.is_whitespace());
    let Some(body_start) = body_start else { return inner.to_string() };
    let sigils = &inner[..body_start];
    let block = sigils.contains(['#', '^', '/', '>', '*']);
    let chars: Vec<(usize, char)> = inner.char_indices().collect();
    let mut out = String::with_capacity(inner.len() + 16);
    let mut i = 0;
    let mut first_token = true;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let (pos, c) = chars[i];
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            out.push(c);
            first_token = pos < body_start;
            i += 1;
            continue;
        }
        if pos >= body_start && is_path_char(c) {
            let mut j = i;
            while j < chars.len() && is_path_char(chars[j].1) {
                j += 1;
            }
            let end = chars.get(j).map(|x| x.0).unwrap_or(inner.len());
            let token = &inner[pos..end];
            match token.strip_suffix(".length").filter(|p| !p.is_empty()) {
                Some(path) if first_token && !block => {
                    out.push_str("js_length ");
                    out.push_str(path);
                }
                Some(path) if !first_token => {
                    out.push_str("(js_length ");
                    out.push_str(path);
                    out.push(')');
                }
                _ => out.push_str(token),
            }
            first_token = false;
            i = j;
            continue;
        }
        out.push(c);
        if pos >= body_start && !c.is_whitespace() && c != '(' && c != ')' && c != '=' {
            first_token = false;
        }
        i += 1;
    }
    out
}

/// Renders a visualizer template (`pm.visualizer.set(template, data)`).
///
/// Handlebars.js semantics: `{{x}}` is HTML-escaped (`& < > " ' \` =`),
/// `{{{x}}}` is raw, missing values render as nothing, arrays print as
/// `a,b`, `{}` is truthy, and the built-in
/// helpers `if`, `unless`, `each` (`@index`, `@key`, `@first`, `@last`),
/// `with`, `lookup` and `log` are available (plus `eq`, `ne`, `gt`, `lt`,
/// `and`, `or`, `not`, `len` of handlebars-rust).
pub fn render_template(template: &str, data: &Value) -> Result<String, Error> {
    let mut hb = Handlebars::new();
    hb.set_strict_mode(false);
    hb.register_escape_fn(handlebars::html_escape);
    hb.register_helper("js_length", Box::new(js_length));
    hb.register_helper("js_str", Box::new(js_str));
    hb.register_helper("if", Box::new(JsIf { positive: true }));
    hb.register_helper("unless", Box::new(JsIf { positive: false }));
    hb.register_helper("with", Box::new(JsWith));
    hb.render_template(&rewrite_values(&rewrite_length(template)), data).map_err(|e| {
        let (message, line, column) = match e.reason() {
            RenderErrorReason::TemplateError(t) => {
                let pos = t.pos();
                (t.reason().to_string(), pos.map(|p| p.0), pos.map(|p| p.1))
            }
            other => (other.to_string(), e.line_no, e.column_no),
        };
        Error::Template { message, line, column }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn escaping_matches_handlebars_js() {
        let data = json!({"x": "<b>\"Tom\" & 'Jerry'</b> `=`"});
        assert_eq!(
            render_template("{{x}}", &data).unwrap(),
            "&lt;b&gt;&quot;Tom&quot; &amp; &#x27;Jerry&#x27;&lt;/b&gt; &#x60;&#x3D;&#x60;"
        );
        assert_eq!(render_template("{{{x}}}", &data).unwrap(), "<b>\"Tom\" & 'Jerry'</b> `=`");
        assert_eq!(render_template("{{&x}}", &data).unwrap(), "<b>\"Tom\" & 'Jerry'</b> `=`");
    }

    #[test]
    fn data_fields_named_like_extra_helpers() {
        let data = json!({"raw": "<i>r</i>", "len": 3, "eq": "same", "items": [{"not": "x"}]});
        assert_eq!(
            render_template("{{raw}}|{{{raw}}}|{{& raw}}", &data).unwrap(),
            "&lt;i&gt;r&lt;/i&gt;|<i>r</i>|<i>r</i>"
        );
        assert_eq!(render_template("{{ len }}-{{~eq~}}", &data).unwrap(), "3-same");
        assert_eq!(render_template("{{#each items}}{{not}}{{/each}}", &data).unwrap(), "x");
        // Whitespace control is kept.
        assert_eq!(render_template("a {{~eq~}} b|{{~{raw}~}} |{{~ len}} ", &data).unwrap(), "asameb|<i>r</i>|3 ");
        // Helper calls with parameters are untouched.
        assert_eq!(render_template("{{#if (eq len 3)}}yes{{/if}} {{len items}}", &data).unwrap(), "yes 1");
    }

    #[test]
    fn builtin_helpers() {
        let data = json!({
            "users": [{"name": "Ada", "admin": true}, {"name": "Bob", "admin": false}],
            "meta": {"count": 2, "tags": {"a": 1, "b": 2}},
            "empty": []
        });
        let t = "{{#each users}}{{@index}}:{{name}}{{#if admin}}*{{/if}}{{#unless @last}},{{/unless}}{{/each}}";
        assert_eq!(render_template(t, &data).unwrap(), "0:Ada*,1:Bob");
        assert_eq!(render_template("{{#with meta}}{{count}}{{/with}}", &data).unwrap(), "2");
        assert_eq!(render_template("{{#each meta.tags}}{{@key}}={{this}};{{/each}}", &data).unwrap(), "a=1;b=2;");
        assert_eq!(render_template("{{lookup users 1}}", &data).unwrap(), "[object Object]");
        assert_eq!(render_template("{{lookup (lookup users 1) \"name\"}}", &data).unwrap(), "Bob");
        assert_eq!(render_template("{{#each empty}}x{{else}}none{{/each}}", &data).unwrap(), "none");
        assert_eq!(render_template("{{#if empty}}y{{else}}n{{/if}}", &data).unwrap(), "n");
        assert_eq!(render_template("{{missing.deep}}|{{users.0.name}}", &data).unwrap(), "|Ada");
        assert_eq!(render_template("{{#each users}}{{../meta.count}}{{/each}}", &data).unwrap(), "22");
    }

    #[test]
    fn values_print_like_handlebars_js() {
        let data = json!({
            "a": [1, "<b>", null, [2, 3], {"k": 1}, true, 1.5], "o": {"k": 1}, "n": 10, "f": 0.25, "s": "<i>",
            "len": ["x", "y"], "rows": [["a", "b"], ["c"]], "t": true
        });
        assert_eq!(render_template("{{a}}", &data).unwrap(), "1,&lt;b&gt;,,2,3,[object Object],true,1.5");
        assert_eq!(
            render_template("{{{a}}}|{{&o}}|{{~ o ~}} |", &data).unwrap(),
            "1,<b>,,2,3,[object Object],true,1.5|[object Object]|[object Object]|"
        );
        assert_eq!(
            render_template("{{n}} {{f}} {{s}} {{{s}}} {{t}} {{missing}}|{{123}}", &data).unwrap(),
            "10 0.25 &lt;i&gt; <i> true |123"
        );
        assert_eq!(render_template("{{len}} {{lookup rows 0}} {{{lookup rows 1}}}", &data).unwrap(), "x,y a,b c");
        assert_eq!(
            render_template("{{#each rows}}[{{this}}|{{.}}|{{@index}}|{{../n}}]{{/each}}", &data).unwrap(),
            "[a,b|a,b|0|10][c|c|1|10]"
        );
        assert_eq!(render_template("{{#each rows as |r i|}}{{r}}{{i}}{{else}}none{{/each}}", &data).unwrap(), "a,b0c1");
        // Comments, raw blocks and escaped mustaches are left alone.
        assert_eq!(
            render_template("{{!-- {{a}} --}}{{! {{a}}}}{{{{raw}}}}{{a}}{{{{/raw}}}}\\{{a}}", &data).unwrap(),
            "}}{{a}}{{a}}"
        );
    }

    #[test]
    fn truthiness_matches_handlebars_js() {
        let data =
            json!({"obj": {}, "arr": [], "zero": 0, "half": 0.5, "s": "", "f": false, "n": null, "tiny": 1e-310});
        let t = "{{#if obj}}a{{/if}}{{#if arr}}b{{/if}}{{#if zero}}c{{/if}}{{#if half}}d{{/if}}{{#if s}}e{{/if}}\
                 {{#if f}}f{{/if}}{{#if n}}g{{/if}}{{#if missing}}h{{/if}}{{#if tiny}}i{{/if}}";
        assert_eq!(render_template(t, &data).unwrap(), "adi");
        assert_eq!(
            render_template("{{#unless obj}}u{{else}}x{{/unless}}{{#unless zero}}z{{/unless}}", &data).unwrap(),
            "xz"
        );
        assert_eq!(
            render_template("{{#if zero includeZero=true}}0{{/if}}{{#if arr includeZero=true}}a{{/if}}", &data)
                .unwrap(),
            "0"
        );
        assert_eq!(
            render_template("{{#if obj}}{{else if zero}}{{else}}{{/if}}{{#if n}}{{else if obj}}o{{/if}}", &data)
                .unwrap(),
            "o"
        );
        // `with` renders its block unless the value is empty (`0` and `{}` are not).
        let data = json!({"obj": {}, "zero": 0, "arr": [], "user": {"name": "Ada"}});
        assert_eq!(
            render_template(
                "{{#with obj}}o{{/with}}{{#with zero}}{{this}}{{/with}}{{#with arr}}a{{else}}-{{/with}}",
                &data
            )
            .unwrap(),
            "o0-"
        );
        assert_eq!(render_template("{{#with user as |u|}}{{u.name}} {{name}}{{/with}}", &data).unwrap(), "Ada Ada");
        assert_eq!(
            render_template("{{#each user}}{{#with ../user}}{{@key}}{{/with}}{{/each}}", &data).unwrap(),
            "name"
        );
    }

    #[test]
    fn javascript_length() {
        let data = json!({"items": [1, 2, 3], "name": "héllo", "obj": {"length": 7}, "none": []});
        let t = "{{items.length}}|{{name.length}}|{{obj.length}}|{{#if none.length}}y{{else}}n{{/if}}|\
                 {{#each items}}{{../items.length}}{{/each}}|{{#with items}}{{this.length}}{{/with}}|\
                 {{lookup items \"x.length\"}}|{{!-- items.length --}}|{{~items.length~}}";
        assert_eq!(render_template(t, &data).unwrap(), "3|5|7|n|333|3|||3");
        assert_eq!(rewrite_length("{{#if (gt items.length 2)}}"), "{{#if (gt (js_length items) 2)}}");
        assert_eq!(rewrite_length("a \\{{x.length}}"), "a \\{{x.length}}");
        assert_eq!(rewrite_length("no lengths {{x}}"), "no lengths {{x}}");
        let raw = "{{{{raw}}}}{{x.length}}{{{{/raw}}}} {{!-- {{x.length}} --}}";
        assert_eq!(rewrite_length(raw), raw);
    }

    #[test]
    fn template_errors_have_positions() {
        let err = render_template("<p>\n{{#each items}}\n<li>{{name}}</li>\n", &json!({})).unwrap_err();
        let Error::Template { line, .. } = &err;
        assert!(line.is_some(), "{err}");
        let err = render_template("{{#if}}{{/each}}", &json!({})).unwrap_err();
        assert!(err.to_string().starts_with("template error"), "{err}");
    }
}
