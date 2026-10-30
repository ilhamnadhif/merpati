//! Syntax highlighting for the editors and the response viewer: JSON,
//! JavaScript, XML/HTML and GraphQL, plus `{{variable}}` and search overlays.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Json,
    JavaScript,
    Xml,
    GraphQl,
    Yaml,
    Text,
}

impl Lang {
    pub fn from_language(l: merpati_core::response::BodyLanguage) -> Lang {
        use merpati_core::response::BodyLanguage as B;
        match l {
            B::Json => Lang::Json,
            B::Xml | B::Html => Lang::Xml,
            B::JavaScript | B::Css => Lang::JavaScript,
            _ => Lang::Text,
        }
    }

    pub fn from_raw(l: merpati_core::model::RawLanguage) -> Lang {
        use merpati_core::model::RawLanguage as R;
        match l {
            R::Json => Lang::Json,
            R::Xml | R::Html => Lang::Xml,
            R::JavaScript => Lang::JavaScript,
            R::Text => Lang::Text,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Key,
    Str,
    Num,
    Keyword,
    Comment,
    Punct,
    Tag,
    Attr,
}

struct Palette {
    text: Color32,
    key: Color32,
    string: Color32,
    number: Color32,
    keyword: Color32,
    comment: Color32,
    punct: Color32,
    tag: Color32,
    attr: Color32,
}

/// Syntax colors tuned to the app palette (`theme.rs`): blue keys, green
/// strings, amber numbers, violet keywords, accent tags.
fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            text: Color32::from_rgb(222, 223, 228),
            key: Color32::from_rgb(122, 172, 255),
            string: Color32::from_rgb(130, 204, 140),
            number: Color32::from_rgb(240, 178, 96),
            keyword: Color32::from_rgb(190, 150, 250),
            comment: Color32::from_rgb(118, 120, 130),
            punct: Color32::from_rgb(146, 148, 158),
            tag: Color32::from_rgb(246, 128, 104),
            attr: Color32::from_rgb(232, 190, 110),
        }
    } else {
        Palette {
            text: Color32::from_rgb(26, 26, 32),
            key: Color32::from_rgb(30, 88, 200),
            string: Color32::from_rgb(20, 122, 66),
            number: Color32::from_rgb(176, 96, 0),
            keyword: Color32::from_rgb(124, 72, 210),
            comment: Color32::from_rgb(140, 140, 152),
            punct: Color32::from_rgb(110, 110, 122),
            tag: Color32::from_rgb(190, 58, 26),
            attr: Color32::from_rgb(150, 96, 0),
        }
    }
}

/// Colors of JSON parts, for views that draw JSON themselves (tree view).
pub struct JsonColors {
    pub key: Color32,
    pub string: Color32,
    pub number: Color32,
    /// `true`, `false`, `null`.
    pub keyword: Color32,
    pub punct: Color32,
}

pub fn json_colors(dark: bool) -> JsonColors {
    let p = palette(dark);
    JsonColors { key: p.key, string: p.string, number: p.number, keyword: p.keyword, punct: p.punct }
}

/// Background of search matches.
pub fn search_background(dark: bool) -> Color32 {
    if dark { Color32::from_rgba_unmultiplied(240, 180, 72, 80) } else { Color32::from_rgb(255, 228, 150) }
}

impl Palette {
    fn color(&self, k: Kind) -> Color32 {
        match k {
            Kind::Text => self.text,
            Kind::Key => self.key,
            Kind::Str => self.string,
            Kind::Num => self.number,
            Kind::Keyword => self.keyword,
            Kind::Comment => self.comment,
            Kind::Punct => self.punct,
            Kind::Tag => self.tag,
            Kind::Attr => self.attr,
        }
    }
}

const JS_KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];
const GQL_KEYWORDS: &[&str] = &[
    "query",
    "mutation",
    "subscription",
    "fragment",
    "on",
    "type",
    "input",
    "enum",
    "interface",
    "union",
    "scalar",
    "schema",
    "extend",
    "directive",
    "implements",
    "true",
    "false",
    "null",
];

fn lex(text: &str, lang: Lang) -> Vec<(usize, Kind)> {
    // Returns (start, kind) change points.
    let b = text.as_bytes();
    let mut out: Vec<(usize, Kind)> = Vec::new();
    let mut push = |pos: usize, k: Kind| {
        if out.last().map(|l| l.1 != k).unwrap_or(true) {
            out.push((pos, k));
        }
    };
    let mut i = 0;
    match lang {
        Lang::Text => push(0, Kind::Text),
        Lang::Yaml => {
            let mut line_start = 0;
            for line in text.split_inclusive('\n') {
                let trimmed = line.trim_start();
                let indent = line.len() - trimmed.len();
                let mut pos = line_start + indent;
                let mut rest = trimmed;
                if let Some(r) = rest.strip_prefix("- ") {
                    push(pos, Kind::Punct);
                    pos += 2;
                    rest = r;
                }
                if rest.starts_with('#') {
                    push(pos, Kind::Comment);
                } else if let Some(colon) =
                    rest.find(": ").or_else(|| rest.trim_end().strip_suffix(':').map(|k| k.len()))
                {
                    push(pos, Kind::Key);
                    push(pos + colon, Kind::Punct);
                    let value = rest[colon + 1..].trim_start();
                    let vpos = pos + colon + 1 + (rest[colon + 1..].len() - value.len());
                    let v = value.trim_end();
                    let kind = if v.starts_with('"') || v.starts_with('\'') {
                        Kind::Str
                    } else if v.starts_with('#') {
                        Kind::Comment
                    } else if matches!(v, "true" | "false" | "null" | "~") {
                        Kind::Keyword
                    } else if v.parse::<f64>().is_ok() {
                        Kind::Num
                    } else {
                        Kind::Text
                    };
                    push(vpos, kind);
                } else {
                    push(pos, Kind::Text);
                }
                line_start += line.len();
            }
        }
        Lang::Json | Lang::JavaScript | Lang::GraphQl => {
            while i < b.len() {
                let c = b[i];
                if lang == Lang::JavaScript && c == b'/' && b.get(i + 1) == Some(&b'/')
                    || lang == Lang::GraphQl && c == b'#'
                {
                    push(i, Kind::Comment);
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                if lang == Lang::JavaScript && c == b'/' && b.get(i + 1) == Some(&b'*') {
                    push(i, Kind::Comment);
                    i += 2;
                    while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(b.len());
                    continue;
                }
                if c == b'"' || (lang == Lang::JavaScript && (c == b'\'' || c == b'`')) {
                    let start = i;
                    i += 1;
                    while i < b.len() && b[i] != c {
                        if b[i] == b'\\' {
                            i += 1;
                        }
                        if c != b'`' && b[i.min(b.len() - 1)] == b'\n' {
                            break;
                        }
                        i += 1;
                    }
                    i = (i + 1).min(b.len());
                    let mut kind = Kind::Str;
                    if lang == Lang::Json {
                        let mut j = i;
                        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                            j += 1;
                        }
                        if b.get(j) == Some(&b':') {
                            kind = Kind::Key;
                        }
                    }
                    push(start, kind);
                    continue;
                }
                if c.is_ascii_digit() || (c == b'-' && b.get(i + 1).map(|d| d.is_ascii_digit()).unwrap_or(false)) {
                    let prev_ident = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
                    if !prev_ident {
                        push(i, Kind::Num);
                        i += 1;
                        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                            i += 1;
                        }
                        continue;
                    }
                }
                if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
                    let start = i;
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                        i += 1;
                    }
                    let word = &text[start..i];
                    let kw = match lang {
                        Lang::Json => matches!(word, "true" | "false" | "null"),
                        Lang::JavaScript => JS_KEYWORDS.contains(&word),
                        Lang::GraphQl => GQL_KEYWORDS.contains(&word) || word.starts_with('$'),
                        _ => false,
                    };
                    let is_pm = lang == Lang::JavaScript && word == "pm";
                    push(
                        start,
                        if kw {
                            Kind::Keyword
                        } else if is_pm {
                            Kind::Key
                        } else {
                            Kind::Text
                        },
                    );
                    continue;
                }
                if c.is_ascii_punctuation() {
                    push(i, Kind::Punct);
                } else {
                    push(i, Kind::Text);
                }
                i += 1;
            }
        }
        Lang::Xml => {
            // Bytes, not `&text[i..]`: `i` walks byte by byte through text
            // such as "★" and may stop inside a character.
            while i < b.len() {
                if b[i..].starts_with(b"<!--") {
                    push(i, Kind::Comment);
                    match b[i..].windows(3).position(|w| w == b"-->") {
                        Some(e) => i += e + 3,
                        None => i = b.len(),
                    }
                    continue;
                }
                if b[i] == b'<' {
                    push(i, Kind::Punct);
                    i += 1;
                    if i < b.len() && (b[i] == b'/' || b[i] == b'?' || b[i] == b'!') {
                        i += 1;
                    }
                    push(i, Kind::Tag);
                    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
                        i += 1;
                    }
                    // Attributes until '>'.
                    while i < b.len() && b[i] != b'>' {
                        let c = b[i];
                        if c == b'"' || c == b'\'' {
                            push(i, Kind::Str);
                            i += 1;
                            while i < b.len() && b[i] != c {
                                i += 1;
                            }
                            i = (i + 1).min(b.len());
                            continue;
                        }
                        if c.is_ascii_alphabetic() {
                            push(i, Kind::Attr);
                        } else {
                            push(i, Kind::Punct);
                        }
                        i += 1;
                    }
                    push(i, Kind::Punct);
                    i = (i + 1).min(b.len());
                    continue;
                }
                push(i, Kind::Text);
                i += 1;
            }
        }
    }
    if out.is_empty() {
        out.push((0, Kind::Text));
    }
    out
}

/// Names that resolve; used to color `{{var}}` references.
#[derive(Default)]
pub struct VarNames {
    names: HashSet<String>,
    fingerprint: u64,
}

impl VarNames {
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        let mut v: Vec<String> = names.into_iter().collect();
        v.sort();
        v.dedup();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.hash(&mut h);
        VarNames { fingerprint: h.finish(), names: v.into_iter().collect() }
    }

    pub fn contains(&self, name: &str) -> bool {
        let n = name.trim();
        self.names.contains(n)
            || n.starts_with('$') && merpati_core::dynamic::NAMES.contains(&n)
            || n.starts_with("vault:")
    }
}

#[derive(Clone, Copy)]
pub struct HlKey<'a> {
    pub text: &'a str,
    pub lang: Lang,
    pub dark: bool,
    pub search: &'a str,
    pub vars: Option<&'a VarNames>,
    pub font_size: f32,
    pub wrap: bool,
}

impl Hash for HlKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text.hash(state);
        self.lang.hash(state);
        self.dark.hash(state);
        self.search.hash(state);
        self.vars.map(|v| v.fingerprint).hash(state);
        self.font_size.to_bits().hash(state);
        self.wrap.hash(state);
    }
}

#[derive(Default)]
pub struct Highlighter;

impl egui::cache::ComputerMut<HlKey<'_>, LayoutJob> for Highlighter {
    fn compute(&mut self, key: HlKey<'_>) -> LayoutJob {
        highlight(key)
    }
}

pub type HighlightCache = egui::cache::FrameCache<LayoutJob, Highlighter>;

/// Cached highlighting, for use inside `TextEdit` layouters.
pub fn cached(ctx: &egui::Context, key: HlKey<'_>) -> LayoutJob {
    ctx.memory_mut(|m| m.caches.cache::<HighlightCache>().get(key).clone())
}

pub fn highlight(key: HlKey<'_>) -> LayoutJob {
    let text = key.text;
    let pal = palette(key.dark);
    let font = FontId::monospace(key.font_size);
    let mut job = LayoutJob { text: String::new(), ..Default::default() };
    job.wrap.max_width = f32::INFINITY;
    if text.is_empty() {
        // One empty section keeps the row as tall as the font: without it the
        // galley has no height and the caret of an empty field hangs below the
        // middle of a vertically centered input.
        job.append("", 0.0, TextFormat { font_id: font, color: pal.color(Kind::Text), ..Default::default() });
        return job;
    }
    // Large inputs: skip lexing, keep only plain color to stay responsive.
    let points = if text.len() > 2_000_000 { vec![(0, Kind::Text)] } else { lex(text, key.lang) };

    // Overlays: variable references and search matches.
    let mut var_ranges: Vec<(usize, usize, bool)> = Vec::new();
    if let Some(vars) = key.vars {
        for t in merpati_core::variables::tokenize(text) {
            if t.kind == merpati_core::variables::TokenKind::Variable {
                let ok = vars.contains(&text[t.name_range.clone()]);
                var_ranges.push((t.range.start, t.range.end, ok));
            }
        }
    }
    let mut search_ranges: Vec<(usize, usize)> = Vec::new();
    if !key.search.is_empty() {
        let hay = text.to_lowercase();
        let needle = key.search.to_lowercase();
        if hay.len() == text.len() {
            let mut from = 0;
            while let Some(p) = hay[from..].find(&needle) {
                let s = from + p;
                search_ranges.push((s, s + needle.len()));
                from = s + needle.len().max(1);
                if search_ranges.len() > 10_000 {
                    break;
                }
            }
        }
    }
    let mut cuts: Vec<usize> = points.iter().map(|p| p.0).collect();
    for (s, e, _) in &var_ranges {
        cuts.push(*s);
        cuts.push(*e);
    }
    for (s, e) in &search_ranges {
        cuts.push(*s);
        cuts.push(*e);
    }
    cuts.push(text.len());
    // A change point inside a character moves to the character's end, so
    // every piece is valid text and none of it is left out.
    for c in &mut cuts {
        while *c < text.len() && !text.is_char_boundary(*c) {
            *c += 1;
        }
    }
    cuts.sort_unstable();
    cuts.dedup();

    let search_bg = search_background(key.dark);
    let var_ok = crate::theme::var_defined(key.dark);
    let var_bad = crate::theme::var_undefined(key.dark);
    let mut pi = 0;
    let mut vi = 0;
    let mut si = 0;
    for w in cuts.windows(2) {
        let (s, e) = (w[0], w[1]);
        if s >= e {
            continue;
        }
        while pi + 1 < points.len() && points[pi + 1].0 <= s {
            pi += 1;
        }
        while vi < var_ranges.len() && var_ranges[vi].1 <= s {
            vi += 1;
        }
        while si < search_ranges.len() && search_ranges[si].1 <= s {
            si += 1;
        }
        let mut color = pal.color(points[pi].1);
        if let Some((vs, ve, ok)) = var_ranges.get(vi)
            && *vs <= s
            && e <= *ve
        {
            color = if *ok { var_ok } else { var_bad };
        }
        let mut fmt = TextFormat { font_id: font.clone(), color, ..Default::default() };
        if let Some((ss, se)) = search_ranges.get(si)
            && *ss <= s
            && e <= *se
        {
            fmt.background = search_bg;
        }
        job.append(&text[s..e], 0.0, fmt);
    }
    job
}

/// Pretty-prints JSON keeping key order; returns `None` if invalid.
#[cfg(test)]
pub fn pretty_json(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    Some(pretty_json_value(&v))
}

/// Pretty-prints a value like `serde_json::to_string_pretty` (two-space
/// indent, one member per line), so the response viewer can map each line
/// back to the value printed on it.
pub fn pretty_json_value(v: &serde_json::Value) -> String {
    let mut out = String::new();
    write_json_pretty(&mut out, v, 0);
    out
}

/// Appends `v` pretty-printed, its inner lines indented `indent` levels.
pub fn write_json_pretty(out: &mut String, v: &serde_json::Value, indent: usize) {
    use serde_json::Value;
    let pad = |out: &mut String, n: usize| {
        for _ in 0..n {
            out.push_str("  ");
        }
    };
    match v {
        Value::Object(m) if !m.is_empty() => {
            out.push_str("{\n");
            for (i, (k, child)) in m.iter().enumerate() {
                pad(out, indent + 1);
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push_str(": ");
                write_json_pretty(out, child, indent + 1);
                if i + 1 < m.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            pad(out, indent);
            out.push('}');
        }
        Value::Array(a) if !a.is_empty() => {
            out.push_str("[\n");
            for (i, child) in a.iter().enumerate() {
                pad(out, indent + 1);
                write_json_pretty(out, child, indent + 1);
                if i + 1 < a.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            pad(out, indent);
            out.push(']');
        }
        _ => out.push_str(&serde_json::to_string(v).unwrap_or_default()),
    }
}

/// Very small XML/HTML pretty printer: one tag per line with indentation.
pub fn pretty_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut depth: usize = 0;
    let mut i = 0;
    let b = text.as_bytes();
    let mut pending_text = String::new();
    let indent = |d: usize, out: &mut String| {
        for _ in 0..d {
            out.push_str("  ");
        }
    };
    while i < b.len() {
        if b[i] == b'<' {
            let t = pending_text.trim();
            if !t.is_empty() {
                indent(depth, &mut out);
                out.push_str(t);
                out.push('\n');
            }
            pending_text.clear();
            let end = text[i..].find('>').map(|e| i + e + 1).unwrap_or(b.len());
            let tag = &text[i..end];
            let closing = tag.starts_with("</");
            let self_closing = tag.ends_with("/>") || tag.starts_with("<?") || tag.starts_with("<!");
            if closing {
                depth = depth.saturating_sub(1);
            }
            indent(depth, &mut out);
            out.push_str(tag);
            if !closing && !self_closing {
                let name: String = tag[1..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
                    .collect();
                // An element holding only text stays on one line: `<title>Hi</title>`.
                let rest = &text[end..];
                if let Some(lt) = rest.find('<')
                    && let Some(after) = rest[lt..].strip_prefix("</").and_then(|r| r.strip_prefix(name.as_str()))
                    && let Some(gt) = after.find('>')
                    && after[..gt].trim().is_empty()
                {
                    out.push_str(rest[..lt].trim());
                    out.push_str(&rest[lt..lt + 2 + name.len() + gt + 1]);
                    out.push('\n');
                    i = end + lt + 2 + name.len() + gt + 1;
                    continue;
                }
                out.push('\n');
                let name = name.to_lowercase();
                let void = matches!(
                    name.as_str(),
                    "br" | "hr"
                        | "img"
                        | "input"
                        | "meta"
                        | "link"
                        | "area"
                        | "base"
                        | "col"
                        | "embed"
                        | "source"
                        | "track"
                        | "wbr"
                );
                if !void {
                    depth += 1;
                }
            } else {
                out.push('\n');
            }
            i = end;
        } else {
            let ch = text[i..].chars().next().unwrap_or(' ');
            pending_text.push(ch);
            i += ch.len_utf8();
        }
    }
    let t = pending_text.trim();
    if !t.is_empty() {
        out.push_str(t);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Non-ASCII text anywhere (an HTML page with "★", a JSON string with
    /// emoji, an unquoted attribute) never panics and keeps every character.
    #[test]
    fn non_ascii_text_is_highlighted_whole() {
        let samples = [
            "<p>Rated ★★★★☆</p><!-- é --><a title=ñ x=\"ü\">→</a>",
            "<html><body>★<br/>日本語 <img alt='🙂'>\n</body></html>",
            "★<!--",
            "<t ★ é=1>",
            r#"{"név": "Zoë 🙂", "ключ": [1, "→"], "x": "\é"}"#,
            "const s = '★'; // ünïcode\n/* 🙂 */ pm.test(\"é\", () => {})",
            "query { ★ é: f(a: \"ü\") # 🙂\n}",
            "naïve: ★\n- 🙂: \"é\"\n# ü",
            "plain ★ text",
        ];
        let vars = VarNames::new(["é".to_string()]);
        for text in samples {
            for lang in [Lang::Xml, Lang::Json, Lang::JavaScript, Lang::GraphQl, Lang::Yaml, Lang::Text] {
                for search in ["", "é", "★", "a"] {
                    let job = highlight(HlKey {
                        text,
                        lang,
                        dark: false,
                        search,
                        vars: Some(&vars),
                        font_size: 13.0,
                        wrap: false,
                    });
                    assert_eq!(job.text, text, "{lang:?} search {search:?}");
                }
            }
        }
    }

    #[test]
    fn json_keys_and_values() {
        let pts = lex(r#"{"a": 1, "b": "x", "c": true}"#, Lang::Json);
        assert!(pts.iter().any(|p| p.1 == Kind::Key));
        assert!(pts.iter().any(|p| p.1 == Kind::Str));
        assert!(pts.iter().any(|p| p.1 == Kind::Num));
        assert!(pts.iter().any(|p| p.1 == Kind::Keyword));
    }

    #[test]
    fn job_covers_all_text() {
        let vars = VarNames::new(["ok".to_string()]);
        for (text, lang) in [
            ("{\"u\": \"{{ok}} {{bad}}\"}", Lang::Json),
            ("// c\nconst x = 'é{{ok}}';", Lang::JavaScript),
            ("<a href=\"x\">t</a><!-- c -->", Lang::Xml),
            ("query { me { id } } # c", Lang::GraphQl),
            ("a: 1\n- b: \"x\"\n# c\nkey:\n  nested: true\n", Lang::Yaml),
        ] {
            let job = highlight(HlKey {
                text,
                lang,
                dark: true,
                search: "o",
                vars: Some(&vars),
                font_size: 12.0,
                wrap: false,
            });
            assert_eq!(job.text, text);
        }
    }

    /// The viewer maps lines back to values, so the printer must lay JSON
    /// out exactly like serde_json does.
    #[test]
    fn pretty_json_matches_serde() {
        let samples = [
            r#"{"a": 1, "b": [1, 2, {"c": null, "d": []}], "e": {}, "f": "x\"y\n", "g": {"h": [[]]}, "é": 1.5e3}"#,
            "[]",
            "{}",
            "\"text\"",
            "[[1], [], {\"k\": [true, false]}]",
            "-0.25",
        ];
        for s in samples {
            let v: serde_json::Value = serde_json::from_str(s).unwrap();
            assert_eq!(pretty_json_value(&v), serde_json::to_string_pretty(&v).unwrap(), "{s}");
        }
        assert_eq!(pretty_json(b"{\"a\":1}").as_deref(), Some("{\n  \"a\": 1\n}"));
        assert_eq!(pretty_json(b"{oops"), None);
    }

    #[test]
    fn xml_pretty() {
        let p = pretty_xml("<a><b x=\"1\">t</b><c/></a>");
        assert_eq!(p, "<a>\n  <b x=\"1\">t</b>\n  <c/>\n</a>\n");
        // Mixed content keeps one node per line; empty elements and
        // prefixed names stay on one line too.
        let p = pretty_xml("<p>Hello <b>world</b>!</p><x:y a='1'> </x:y ><e></e><br><i>★ é</i>");
        assert_eq!(p, "<p>\n  Hello\n  <b>world</b>\n  !\n</p>\n<x:y a='1'></x:y >\n<e></e>\n<br>\n<i>★ é</i>\n");
        // A closing tag of another element is not this element's end.
        let p = pretty_xml("<ab>t</a></ab>");
        assert!(p.starts_with("<ab>\n  t\n"), "{p}");
    }
}
