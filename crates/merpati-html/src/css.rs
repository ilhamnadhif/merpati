//! A small CSS parser: style sheets, declarations, selectors (type, class,
//! id, attribute, descendant/child/sibling combinators and structural
//! pseudo-classes) and value helpers.

use std::collections::HashMap;

use egui::Color32;

use crate::dom::{Dom, NodeId};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Decl {
    pub name: String,
    pub value: String,
    pub important: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub selector: Selector,
    pub specificity: u32,
    pub order: usize,
    pub decls: Vec<Decl>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Stylesheet {
    pub rules: Vec<Rule>,
    /// `@import` URLs.
    pub imports: Vec<String>,
    /// Rules bucketed by the id, first class or tag of their rightmost
    /// compound selector (rebuilt by `add`).
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
    /// Selectors (without the pseudo-element) of `::after`/`::before` rules
    /// with `clear`: the clearfix idiom.
    clearfix: Vec<Selector>,
    /// Viewport width (CSS px) for `@media` width queries; `None` = unknown,
    /// width queries do not apply. Set before `add`.
    pub width: Option<f32>,
    /// Width ranges of every `@media` query seen, matching or not.
    pub queries: Vec<WidthQuery>,
}

/// The width range of a media query in CSS px (inclusive bounds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WidthQuery {
    pub min: f32,
    pub max: f32,
}

impl WidthQuery {
    pub fn matches(self, width: f32) -> bool {
        width >= self.min && width <= self.max
    }
}

/// Whether `@media <list>` applies at `width`; width ranges are added to
/// `queries`. Print, `not` and `prefers-color-scheme` queries never apply;
/// features other than width are ignored.
fn media_applies(list: &str, width: Option<f32>, queries: &mut Vec<WidthQuery>) -> bool {
    let results: Vec<bool> =
        split_top_level(list, ',').into_iter().map(|q| query_applies(q.trim(), width, queries)).collect();
    results.into_iter().any(|r| r)
}

fn query_applies(q: &str, width: Option<f32>, queries: &mut Vec<WidthQuery>) -> bool {
    let q = q.strip_prefix("only ").unwrap_or(q).trim();
    if q.starts_with("not ") {
        return false;
    }
    let mut range = WidthQuery { min: 0.0, max: f32::INFINITY };
    let mut has_width = false;
    for part in q.split(" and ") {
        let part = part.trim();
        let Some(feature) = part.strip_prefix('(') else {
            if !matches!(part, "" | "screen" | "all") {
                return false;
            }
            continue;
        };
        let feature = feature.trim_end_matches(')').trim();
        if feature.contains("prefers-color-scheme") {
            return false;
        }
        if let Some((name, value)) = feature.split_once(':') {
            let name = name.trim().trim_start_matches("-webkit-");
            let (min, max) = match name {
                "min-width" | "min-device-width" => (media_length(value), None),
                "max-width" | "max-device-width" => (None, media_length(value)),
                "width" | "device-width" => (media_length(value), media_length(value)),
                _ => continue,
            };
            if min.is_none() && max.is_none() {
                return false;
            }
            range.min = range.min.max(min.unwrap_or(0.0));
            range.max = range.max.min(max.unwrap_or(f32::INFINITY));
            has_width = true;
        } else if feature.contains("width") {
            if !width_range(feature, &mut range) {
                return false;
            }
            has_width = true;
        }
    }
    if !has_width {
        return true;
    }
    queries.push(range);
    width.is_some_and(|w| range.matches(w))
}

/// Range syntax: `width >= 600px`, `400px <= width < 800px`.
fn width_range(feature: &str, range: &mut WidthQuery) -> bool {
    let mut items: Vec<&str> = Vec::new();
    let mut rest = feature;
    'scan: while !rest.is_empty() {
        for (i, _) in rest.char_indices() {
            for op in ["<=", ">=", "<", ">", "="] {
                if rest[i..].starts_with(op) {
                    items.push(rest[..i].trim());
                    items.push(op);
                    rest = &rest[i + op.len()..];
                    continue 'scan;
                }
            }
        }
        items.push(rest.trim());
        break;
    }
    if items.len() < 3 {
        return false;
    }
    for w in items.windows(3).step_by(2) {
        let (op, value, flipped) = match (w[0], w[2]) {
            ("width", v) => (w[1], v, false),
            (v, "width") => (w[1], v, true),
            _ => return false,
        };
        let Some(v) = media_length(value) else { return false };
        // `<` excludes the bound; widths are whole or half pixels in practice.
        let op = if flipped {
            match op {
                "<" => ">",
                ">" => "<",
                "<=" => ">=",
                ">=" => "<=",
                o => o,
            }
        } else {
            op
        };
        match op {
            ">=" => range.min = range.min.max(v),
            ">" => range.min = range.min.max(v + 0.01),
            "<=" => range.max = range.max.min(v),
            "<" => range.max = range.max.min(v - 0.01),
            _ => {
                range.min = range.min.max(v);
                range.max = range.max.min(v);
            }
        }
    }
    true
}

/// A media query length in CSS px (`em`/`rem` are 16px there).
fn media_length(v: &str) -> Option<f32> {
    let v = v.trim();
    let split = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
    let n: f32 = v[..split].parse().ok()?;
    match &v[split..] {
        "px" => Some(n),
        "em" | "rem" => Some(n * 16.0),
        "" if n == 0.0 => Some(0.0),
        _ => None,
    }
}

impl Stylesheet {
    /// Appends the rules of `css`; `order` continues across style elements.
    pub fn add(&mut self, css: &str) {
        let css = strip_comments(css);
        let mut order = self.rules.len();
        let first = self.rules.len();
        let mut media = Media { width: self.width, queries: std::mem::take(&mut self.queries), depth: 0 };
        parse_rules(&css, &mut self.rules, &mut self.imports, &mut order, &mut media);
        self.queries = media.queries;
        for i in first..self.rules.len() {
            let rule = &self.rules[i];
            let clears = rule.decls.iter().any(|d| {
                d.name == "clear" && matches!(d.value.trim().to_ascii_lowercase().as_str(), "both" | "left" | "right")
            });
            if clears && rule.selector.parts[0].0.pseudos.contains(&Pseudo::Generated) {
                let mut sel = rule.selector.clone();
                sel.parts[0].0.pseudos.retain(|p| *p != Pseudo::Generated);
                self.clearfix.push(sel);
            }
            let key = &self.rules[i].selector.parts[0].0;
            if let Some(id) = &key.id {
                self.by_id.entry(id.clone()).or_default().push(i);
            } else if let Some(class) = key.classes.first() {
                self.by_class.entry(class.clone()).or_default().push(i);
            } else if let Some(tag) = &key.tag {
                self.by_tag.entry(tag.clone()).or_default().push(i);
            } else {
                self.universal.push(i);
            }
        }
    }

    /// Rules matching `el`, in cascade order (specificity, then source order).
    pub fn matching(&self, dom: &Dom, el: NodeId) -> Vec<&Rule> {
        let mut candidates: Vec<usize> = self.universal.clone();
        if let Some(tag) = dom.tag(el)
            && let Some(v) = self.by_tag.get(tag)
        {
            candidates.extend(v);
        }
        if let Some(id) = dom.attr(el, "id")
            && let Some(v) = self.by_id.get(id)
        {
            candidates.extend(v);
        }
        if let Some(class) = dom.attr(el, "class") {
            for c in class.split_whitespace() {
                if let Some(v) = self.by_class.get(c) {
                    candidates.extend(v);
                }
            }
            candidates.sort_unstable();
            candidates.dedup();
        }
        let mut out: Vec<&Rule> =
            candidates.into_iter().map(|i| &self.rules[i]).filter(|r| r.selector.matches(dom, el)).collect();
        out.sort_by_key(|r| (r.specificity, r.order));
        out
    }

    /// Whether a clearfix rule (`.x::after { clear: both }`) applies to `el`.
    pub fn clearfix(&self, dom: &Dom, el: NodeId) -> bool {
        self.clearfix.iter().any(|s| s.matches(dom, el))
    }
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Index of the `}` matching the `{` at `open` (skipping strings).
fn matching_brace(s: &str, open: usize) -> usize {
    let bytes = s.as_bytes();
    let mut depth = 0;
    let mut quote: Option<u8> = None;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' {
                    i += 1;
                } else if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    bytes.len()
}

/// Deepest nesting of `@media`/`@supports` blocks, `:not()` and `calc()`
/// parentheses parsed (the parsers recurse once per level); deeper content
/// is ignored like other invalid CSS.
const MAX_NESTING: usize = 32;

struct Media {
    width: Option<f32>,
    queries: Vec<WidthQuery>,
    /// Nesting of the block being parsed.
    depth: usize,
}

fn parse_rules(css: &str, rules: &mut Vec<Rule>, imports: &mut Vec<String>, order: &mut usize, media: &mut Media) {
    let mut rest = css;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(at) = rest.strip_prefix('@') {
            let brace = at.find('{');
            let semi = at.find(';');
            match (brace, semi) {
                (Some(b), s) if s.is_none_or(|s| b < s) => {
                    let open = 1 + b;
                    let close = matching_brace(rest, open);
                    let prelude = rest[1..open].trim().to_ascii_lowercase();
                    let body = rest.get(open + 1..close).unwrap_or("");
                    // `@media` blocks apply for screens at the viewport width;
                    // `@supports` and `@layer` blocks always.
                    let applies = match prelude.strip_prefix("media") {
                        Some(list) => media_applies(list.trim(), media.width, &mut media.queries),
                        None => prelude.starts_with("supports") || prelude.starts_with("layer"),
                    };
                    if applies && media.depth < MAX_NESTING {
                        media.depth += 1;
                        parse_rules(body, rules, imports, order, media);
                        media.depth -= 1;
                    }
                    rest = rest.get(close + 1..).unwrap_or("");
                }
                (_, Some(s)) => {
                    let stmt = at[..s].trim();
                    if let Some(arg) = stmt.strip_prefix("import") {
                        let url = arg
                            .trim()
                            .trim_start_matches("url(")
                            .trim_end_matches(')')
                            .trim_matches(|c| c == '"' || c == '\'' || c == ')' || c == ' ');
                        let url = url.split(['"', '\'', ')']).next().unwrap_or(url);
                        if !url.is_empty() {
                            imports.push(url.to_string());
                        }
                    }
                    rest = &at[s + 1..];
                }
                _ => break,
            }
            continue;
        }
        let Some(open) = rest.find('{') else { break };
        let close = matching_brace(rest, open);
        let selectors = &rest[..open];
        let body = rest.get(open + 1..close).unwrap_or("");
        let decls = parse_declarations(body);
        if !decls.is_empty() {
            for sel in split_top_level(selectors, ',') {
                if let Some(selector) = Selector::parse(sel.trim()) {
                    let specificity = selector.specificity();
                    rules.push(Rule { selector, specificity, order: *order, decls: decls.clone() });
                    *order += 1;
                }
            }
        }
        rest = rest.get(close + 1..).unwrap_or("");
    }
}

/// Splits at `sep` outside parentheses, brackets and quotes.
pub(crate) fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0;
    let mut prev_backslash = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if c == q && !prev_backslash {
                quote = None;
            }
        } else {
            match c {
                '"' | '\'' => quote = Some(c),
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                c if c == sep && depth == 0 => {
                    out.push(&s[start..i]);
                    start = i + c.len_utf8();
                }
                _ => {}
            }
        }
        prev_backslash = c == '\\' && !prev_backslash;
    }
    out.push(&s[start..]);
    out
}

/// Parses `a: b; c: d !important`.
pub(crate) fn parse_declarations(s: &str) -> Vec<Decl> {
    let mut out = Vec::new();
    for part in split_top_level(s, ';') {
        let Some((name, value)) = part.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || name.contains(char::is_whitespace) {
            continue;
        }
        let mut value = value.trim();
        let mut important = false;
        // `!important` ends the value; whitespace may follow the `!`.
        let lower = value.to_ascii_lowercase();
        if let Some(before) = lower.strip_suffix("important").map(str::trim_end).and_then(|b| b.strip_suffix('!')) {
            important = true;
            value = value[..before.len()].trim();
        }
        if value.is_empty() {
            continue;
        }
        out.push(Decl { name, value: value.to_string(), important });
    }
    out
}

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Combinator {
    Descendant,
    Child,
    Next,
    Subsequent,
}

#[derive(Clone, Debug, PartialEq)]
enum AttrOp {
    Exists,
    Eq(String),
    Word(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
    Dash(String),
}

#[derive(Clone, Debug, PartialEq)]
enum Pseudo {
    /// `:nth-child(an+b)` and friends: (a, b, from_end, of_type).
    Nth(i32, i32, bool, bool),
    OnlyChild,
    OnlyOfType,
    Root,
    Empty,
    Link,
    Not(Vec<Compound>),
    /// `::before`/`::after` (never matches an element).
    Generated,
    /// Dynamic or unsupported pseudo-classes and pseudo-elements.
    Never,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<(String, AttrOp)>,
    pseudos: Vec<Pseudo>,
}

#[derive(Clone, Debug)]
pub(crate) struct Selector {
    /// Rightmost compound first; the combinator links to the next (left) one.
    parts: Vec<(Compound, Option<Combinator>)>,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_' || !c.is_ascii()
}

struct SelParser<'a> {
    chars: Vec<char>,
    i: usize,
    _s: &'a str,
    /// `:not()` nesting.
    depth: usize,
}

impl SelParser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn ident(&mut self) -> Option<String> {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if is_ident_char(c) {
                s.push(c);
                self.i += 1;
            } else if c == '\\' {
                self.i += 1;
                if let Some(e) = self.peek() {
                    s.push(e);
                    self.i += 1;
                }
            } else {
                break;
            }
        }
        (!s.is_empty()).then_some(s)
    }

    fn skip_ws(&mut self) -> bool {
        let start = self.i;
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
        self.i > start
    }

    fn until(&mut self, end: char) -> Option<String> {
        let mut depth = 0;
        let mut s = String::new();
        while let Some(c) = self.peek() {
            self.i += 1;
            if c == '(' {
                depth += 1;
            } else if c == end && depth == 0 {
                return Some(s);
            } else if c == ')' {
                depth -= 1;
            }
            s.push(c);
        }
        None
    }

    fn compound(&mut self) -> Option<Compound> {
        let mut c = Compound::default();
        let start = self.i;
        if self.peek() == Some('*') {
            self.i += 1;
        } else if self.peek().is_some_and(is_ident_char) {
            c.tag = Some(self.ident()?.to_ascii_lowercase());
        }
        loop {
            match self.peek() {
                Some('#') => {
                    self.i += 1;
                    c.id = Some(self.ident()?);
                }
                Some('.') => {
                    self.i += 1;
                    c.classes.push(self.ident()?);
                }
                Some('[') => {
                    self.i += 1;
                    let inner = self.until(']')?;
                    c.attrs.push(parse_attr(&inner)?);
                }
                Some(':') => {
                    self.i += 1;
                    let element = self.peek() == Some(':');
                    if element {
                        self.i += 1;
                    }
                    let name = self.ident()?.to_ascii_lowercase();
                    let arg = if self.peek() == Some('(') {
                        self.i += 1;
                        Some(self.until(')')?)
                    } else {
                        None
                    };
                    if matches!(name.as_str(), "after" | "before") {
                        c.pseudos.push(Pseudo::Generated);
                    } else if element {
                        c.pseudos.push(Pseudo::Never);
                    } else {
                        c.pseudos.push(parse_pseudo(&name, arg.as_deref(), self.depth)?);
                    }
                }
                _ => break,
            }
        }
        (self.i > start).then_some(c)
    }
}

fn parse_attr(inner: &str) -> Option<(String, AttrOp)> {
    let ops = ["~=", "^=", "$=", "*=", "|=", "="];
    for op in ops {
        if let Some((name, value)) = inner.split_once(op) {
            let name = name.trim().to_ascii_lowercase();
            let mut value = value.trim();
            // Case-insensitivity flag `[a=b i]`.
            if let Some(v) = value.strip_suffix(" i").or_else(|| value.strip_suffix(" s")) {
                value = v.trim();
            }
            let value = value.trim_matches(|c| c == '"' || c == '\'').to_string();
            let op = match op {
                "~=" => AttrOp::Word(value),
                "^=" => AttrOp::Prefix(value),
                "$=" => AttrOp::Suffix(value),
                "*=" => AttrOp::Contains(value),
                "|=" => AttrOp::Dash(value),
                _ => AttrOp::Eq(value),
            };
            return (!name.is_empty()).then_some((name, op));
        }
    }
    let name = inner.trim().to_ascii_lowercase();
    (!name.is_empty()).then_some((name, AttrOp::Exists))
}

/// `an+b` (`odd`, `even`, `3`, `-n+2`, `2n`).
fn parse_nth(arg: &str) -> Option<(i32, i32)> {
    let s: String = arg.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    // `of S` selectors are not supported.
    match s.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    match s.split_once('n') {
        Some((a, b)) => {
            let a = match a {
                "" | "+" => 1,
                "-" => -1,
                n => n.parse().ok()?,
            };
            let b = if b.is_empty() { 0 } else { b.parse().ok()? };
            Some((a, b))
        }
        None => Some((0, s.parse().ok()?)),
    }
}

fn parse_pseudo(name: &str, arg: Option<&str>, depth: usize) -> Option<Pseudo> {
    Some(match (name, arg) {
        ("first-child", None) => Pseudo::Nth(0, 1, false, false),
        ("last-child", None) => Pseudo::Nth(0, 1, true, false),
        ("first-of-type", None) => Pseudo::Nth(0, 1, false, true),
        ("last-of-type", None) => Pseudo::Nth(0, 1, true, true),
        ("only-child", None) => Pseudo::OnlyChild,
        ("only-of-type", None) => Pseudo::OnlyOfType,
        ("root", None) => Pseudo::Root,
        ("empty", None) => Pseudo::Empty,
        ("link" | "any-link", None) => Pseudo::Link,
        ("nth-child", Some(a)) => {
            let (x, y) = parse_nth(a)?;
            Pseudo::Nth(x, y, false, false)
        }
        ("nth-last-child", Some(a)) => {
            let (x, y) = parse_nth(a)?;
            Pseudo::Nth(x, y, true, false)
        }
        ("nth-of-type", Some(a)) => {
            let (x, y) = parse_nth(a)?;
            Pseudo::Nth(x, y, false, true)
        }
        ("nth-last-of-type", Some(a)) => {
            let (x, y) = parse_nth(a)?;
            Pseudo::Nth(x, y, true, true)
        }
        ("not", Some(a)) => {
            if depth >= MAX_NESTING {
                return None;
            }
            let mut list = Vec::new();
            for part in split_top_level(a, ',') {
                let mut p = SelParser { chars: part.trim().chars().collect(), i: 0, _s: part, depth: depth + 1 };
                let c = p.compound()?;
                if p.i != p.chars.len() {
                    return None;
                }
                list.push(c);
            }
            Pseudo::Not(list)
        }
        ("is" | "where" | "matches", Some(_)) => return None,
        _ => Pseudo::Never,
    })
}

impl Selector {
    pub fn parse(s: &str) -> Option<Selector> {
        if s.is_empty() {
            return None;
        }
        let mut p = SelParser { chars: s.chars().collect(), i: 0, _s: s, depth: 0 };
        let mut seq: Vec<(Option<Combinator>, Compound)> = Vec::new();
        let mut pending: Option<Combinator> = None;
        p.skip_ws();
        loop {
            let c = p.compound()?;
            seq.push((pending.take(), c));
            let ws = p.skip_ws();
            match p.peek() {
                None => break,
                Some('>') => {
                    p.i += 1;
                    pending = Some(Combinator::Child);
                }
                Some('+') => {
                    p.i += 1;
                    pending = Some(Combinator::Next);
                }
                Some('~') => {
                    p.i += 1;
                    pending = Some(Combinator::Subsequent);
                }
                Some(_) if ws => pending = Some(Combinator::Descendant),
                Some(_) => return None,
            }
            p.skip_ws();
        }
        if pending.is_some() {
            return None;
        }
        // Store right to left: each compound with the combinator to its left neighbour.
        let mut parts = Vec::with_capacity(seq.len());
        for i in (0..seq.len()).rev() {
            let comb = seq[i].0;
            parts.push((seq[i].1.clone(), comb));
        }
        Some(Selector { parts })
    }

    pub fn specificity(&self) -> u32 {
        fn compound(c: &Compound) -> (u32, u32, u32) {
            let mut a = u32::from(c.id.is_some());
            let mut b = (c.classes.len() + c.attrs.len()) as u32;
            let mut t = u32::from(c.tag.is_some());
            for p in &c.pseudos {
                match p {
                    Pseudo::Not(list) => {
                        let best = list.iter().map(compound).max().unwrap_or((0, 0, 0));
                        a += best.0;
                        b += best.1;
                        t += best.2;
                    }
                    _ => b += 1,
                }
            }
            (a, b, t)
        }
        let (mut a, mut b, mut c) = (0, 0, 0);
        for (comp, _) in &self.parts {
            let (x, y, z) = compound(comp);
            a += x;
            b += y;
            c += z;
        }
        (a.min(1023) << 20) | (b.min(1023) << 10) | c.min(1023)
    }

    pub fn matches(&self, dom: &Dom, el: NodeId) -> bool {
        match_from(&self.parts, 0, dom, el)
    }
}

fn match_from(parts: &[(Compound, Option<Combinator>)], idx: usize, dom: &Dom, el: NodeId) -> bool {
    let (comp, comb) = &parts[idx];
    if !matches_compound(comp, dom, el) {
        return false;
    }
    let Some(comb) = comb else { return true };
    let next = idx + 1;
    match comb {
        Combinator::Child => parent_element(dom, el).is_some_and(|p| match_from(parts, next, dom, p)),
        Combinator::Descendant => {
            let mut cur = parent_element(dom, el);
            while let Some(p) = cur {
                if match_from(parts, next, dom, p) {
                    return true;
                }
                cur = parent_element(dom, p);
            }
            false
        }
        Combinator::Next => prev_element(dom, el).is_some_and(|p| match_from(parts, next, dom, p)),
        Combinator::Subsequent => {
            let mut cur = prev_element(dom, el);
            while let Some(p) = cur {
                if match_from(parts, next, dom, p) {
                    return true;
                }
                cur = prev_element(dom, p);
            }
            false
        }
    }
}

fn parent_element(dom: &Dom, el: NodeId) -> Option<NodeId> {
    dom.nodes[el].parent.filter(|p| dom.is_element(*p))
}

fn siblings(dom: &Dom, el: NodeId) -> Vec<NodeId> {
    match dom.nodes[el].parent {
        Some(p) => dom.element_children(p).collect(),
        None => vec![el],
    }
}

fn prev_element(dom: &Dom, el: NodeId) -> Option<NodeId> {
    let sibs = siblings(dom, el);
    let pos = sibs.iter().position(|s| *s == el)?;
    pos.checked_sub(1).map(|i| sibs[i])
}

fn nth_matches(a: i32, b: i32, index: i32) -> bool {
    // In i64: `index - b` and `i32::MIN / -1` overflow i32.
    let (a, b, index) = (i64::from(a), i64::from(b), i64::from(index));
    if a == 0 {
        return index == b;
    }
    let diff = index - b;
    diff % a == 0 && diff / a >= 0
}

fn matches_compound(c: &Compound, dom: &Dom, el: NodeId) -> bool {
    let Some(tag) = dom.tag(el) else { return false };
    if let Some(t) = &c.tag
        && t != tag
    {
        return false;
    }
    if let Some(id) = &c.id
        && dom.attr(el, "id") != Some(id.as_str())
    {
        return false;
    }
    if !c.classes.is_empty() {
        let class = dom.attr(el, "class").unwrap_or("");
        if !c.classes.iter().all(|want| class.split_whitespace().any(|have| have == want)) {
            return false;
        }
    }
    for (name, op) in &c.attrs {
        let Some(v) = dom.attr(el, name) else { return false };
        let ok = match op {
            AttrOp::Exists => true,
            AttrOp::Eq(x) => v == x,
            AttrOp::Word(x) => v.split_whitespace().any(|w| w == x),
            AttrOp::Prefix(x) => !x.is_empty() && v.starts_with(x.as_str()),
            AttrOp::Suffix(x) => !x.is_empty() && v.ends_with(x.as_str()),
            AttrOp::Contains(x) => !x.is_empty() && v.contains(x.as_str()),
            AttrOp::Dash(x) => v == x || v.starts_with(&format!("{x}-")),
        };
        if !ok {
            return false;
        }
    }
    for p in &c.pseudos {
        let ok = match p {
            Pseudo::Never | Pseudo::Generated => false,
            Pseudo::Root => tag == "html",
            Pseudo::Link => matches!(tag, "a" | "area") && dom.attr(el, "href").is_some(),
            Pseudo::Empty => dom.nodes[el].children.iter().all(|c| match &dom.nodes[*c].data {
                crate::dom::NodeData::Text(t) => t.is_empty(),
                crate::dom::NodeData::Element { .. } => false,
                _ => true,
            }),
            Pseudo::OnlyChild => siblings(dom, el).len() == 1,
            Pseudo::OnlyOfType => siblings(dom, el).iter().filter(|s| dom.tag(**s) == Some(tag)).count() == 1,
            Pseudo::Nth(a, b, from_end, of_type) => {
                let index = match dom.nodes[el].parent {
                    Some(p) => {
                        let same = |s: &NodeId| dom.is_element(*s) && (!*of_type || dom.tag(*s) == Some(tag));
                        let kids = &dom.nodes[p].children;
                        let pos = kids.iter().position(|s| *s == el).unwrap_or(0);
                        if *from_end {
                            kids[pos..].iter().filter(|s| same(s)).count()
                        } else {
                            kids[..=pos].iter().filter(|s| same(s)).count()
                        }
                    }
                    None => 1,
                };
                nth_matches(*a, *b, index as i32)
            }
            Pseudo::Not(list) => !list.iter().any(|x| matches_compound(x, dom, el)),
        };
        if !ok {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// A CSS length: absolute (CSS px) or relative to the containing block.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Len {
    #[default]
    Auto,
    Px(f32),
    Percent(f32),
    /// `calc()` of lengths and percentages: px + percent of the containing block.
    Calc(f32, f32),
    /// `clamp(lo, val, hi)` = max(lo, min(val, hi)) of three `(px, percent)`
    /// terms; `min()`/`max()` of two terms use an infinite bound.
    Clamp([(f32, f32); 3]),
}

/// `px + percent%` of `of`, px multiplied by `scale`.
fn linear((px, percent): (f32, f32), of: f32, scale: f32) -> f32 {
    px * scale + of * percent / 100.0
}

impl Len {
    /// Resolves a [`Len::Clamp`] (other variants: `None`).
    pub(crate) fn resolve_clamp(self, of: f32, scale: f32) -> Option<f32> {
        match self {
            Len::Clamp([lo, val, hi]) => {
                Some(linear(val, of, scale).min(linear(hi, of, scale)).max(linear(lo, of, scale)))
            }
            _ => None,
        }
    }
}

impl Len {
    /// Resolves against the containing block width (`auto` -> `None`).
    pub fn resolve(self, of: f32) -> Option<f32> {
        match self {
            Len::Auto => None,
            Len::Px(p) => Some(p),
            Len::Percent(p) => Some(of * p / 100.0),
            Len::Calc(px, p) => Some(px + of * p / 100.0),
            Len::Clamp(_) => self.resolve_clamp(of, 1.0),
        }
    }

    pub fn px_or(self, of: f32, default: f32) -> f32 {
        self.resolve(of).unwrap_or(default)
    }

    /// A length that does not depend on the containing block (`Px`), as CSS px.
    pub fn fixed(self) -> Option<f32> {
        match self {
            Len::Px(p) => Some(p),
            _ => None,
        }
    }

    fn from_parts(px: f32, percent: f32) -> Len {
        if percent == 0.0 {
            Len::Px(px)
        } else if px == 0.0 {
            Len::Percent(percent)
        } else {
            Len::Calc(px, percent)
        }
    }
}

/// Parses a length. `em` is relative to `font_size` (CSS px), `rem` to 16px.
pub(crate) fn parse_length(v: &str, font_size: f32) -> Option<Len> {
    let v = v.trim().to_ascii_lowercase();
    if v == "auto" {
        return Some(Len::Auto);
    }
    if v == "0" {
        return Some(Len::Px(0.0));
    }
    if let Some(inner) = v.strip_prefix("calc(").or_else(|| v.strip_prefix("-webkit-calc(")) {
        let inner = inner.strip_suffix(')')?;
        let (px, percent, unitless) = Calc { s: inner.as_bytes(), i: 0, font_size, depth: 0 }.parse()?;
        return (!unitless || (px == 0.0 && percent == 0.0)).then(|| Len::from_parts(px, percent));
    }
    for (name, kind) in [("min(", MinMax::Min), ("max(", MinMax::Max), ("clamp(", MinMax::Clamp)] {
        if let Some(inner) = v.strip_prefix(name) {
            return min_max(inner.strip_suffix(')')?, kind, font_size);
        }
    }
    let split = v.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+')).unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    let n: f32 = num.parse().ok()?;
    if !n.is_finite() {
        return None;
    }
    match unit_px(n, unit, font_size)? {
        (p, true) => Some(Len::Percent(p)),
        (px, false) => Some(Len::Px(px)),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MinMax {
    Min,
    Max,
    Clamp,
}

/// `min()`, `max()` or `clamp()` whose arguments are `calc()`-like sums.
/// Arguments in px (or in percent) only are folded together, so `min()` and
/// `max()` may mix at most two kinds of terms (`min(100%, 600px)`).
fn min_max(args: &str, kind: MinMax, font_size: f32) -> Option<Len> {
    let mut terms = Vec::new();
    for arg in split_args(args) {
        let (px, percent, unitless) = Calc { s: arg.as_bytes(), i: 0, font_size, depth: 0 }.parse()?;
        if unitless && (px != 0.0 || percent != 0.0) {
            return None;
        }
        terms.push((px, percent));
    }
    let fold = |a: f32, b: f32| if kind == MinMax::Min { a.min(b) } else { a.max(b) };
    let len = |(px, percent): (f32, f32)| Len::from_parts(px, percent);
    match kind {
        MinMax::Clamp => {
            let [lo, val, hi] = <[(f32, f32); 3]>::try_from(terms).ok()?;
            if lo.1 == 0.0 && val.1 == 0.0 && hi.1 == 0.0 {
                return Some(Len::Px(val.0.min(hi.0).max(lo.0)));
            }
            Some(Len::Clamp([lo, val, hi]))
        }
        MinMax::Min | MinMax::Max => {
            let mut px: Option<f32> = None;
            let mut percent: Option<f32> = None;
            let mut mixed = Vec::new();
            for (p, pc) in terms {
                match (p, pc) {
                    (p, 0.0) => px = Some(px.map_or(p, |x| fold(x, p))),
                    (0.0, pc) => percent = Some(percent.map_or(pc, |x| fold(x, pc))),
                    t => mixed.push(t),
                }
            }
            let mut all: Vec<(f32, f32)> = px.map(|p| (p, 0.0)).into_iter().collect();
            all.extend(percent.map(|pc| (0.0, pc)));
            all.extend(mixed);
            match all.as_slice() {
                [one] => Some(len(*one)),
                [a, b] if kind == MinMax::Min => Some(Len::Clamp([(f32::NEG_INFINITY, 0.0), *a, *b])),
                [a, b] => Some(Len::Clamp([*a, *b, (f32::INFINITY, 0.0)])),
                _ => None,
            }
        }
    }
}

/// Top-level comma-separated arguments.
fn split_args(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0, 0);
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Largest length (CSS px, or percent) kept: lengths saturate like
/// Chrome's layout units, so huge values (`1e38em`) stay finite and sums of
/// them never become NaN.
const MAX_LENGTH: f32 = 33_554_431.0;

/// `n` in `unit` as CSS px, or as a percentage (`true`).
fn unit_px(n: f32, unit: &str, font_size: f32) -> Option<(f32, bool)> {
    let px = match unit {
        "px" => n,
        "" => n, // quirks: unitless numbers as px
        "%" => return Some((n.clamp(-MAX_LENGTH, MAX_LENGTH), true)),
        "em" => n * font_size,
        "rem" => n * 16.0,
        "pt" => n * 4.0 / 3.0,
        "pc" => n * 16.0,
        "in" => n * 96.0,
        "cm" => n * 96.0 / 2.54,
        "mm" => n * 96.0 / 25.4,
        "ex" | "ch" => n * font_size * 0.5,
        "vw" | "vh" | "vmin" | "vmax" => n * 8.0,
        _ => return None,
    };
    Some((px.clamp(-MAX_LENGTH, MAX_LENGTH), false))
}

/// A `calc()` expression evaluated to `px + percent%`. Products and quotients
/// need a unitless side, so every value stays linear.
struct Calc<'a> {
    s: &'a [u8],
    i: usize,
    font_size: f32,
    /// Parentheses open around the current term.
    depth: usize,
}

/// (px, percent, unitless).
type CalcValue = (f32, f32, bool);

impl Calc<'_> {
    fn parse(mut self) -> Option<CalcValue> {
        let v = self.sum()?;
        self.ws();
        (self.i == self.s.len() && v.0.is_finite() && v.1.is_finite()).then_some(v)
    }

    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn sum(&mut self) -> Option<CalcValue> {
        let mut acc = self.product()?;
        loop {
            self.ws();
            let sign = match self.s.get(self.i) {
                Some(b'+') => 1.0,
                Some(b'-') => -1.0,
                _ => return Some(acc),
            };
            self.i += 1;
            let rhs = self.product()?;
            if acc.2 != rhs.2 {
                return None;
            }
            acc = (acc.0 + sign * rhs.0, acc.1 + sign * rhs.1, acc.2);
        }
    }

    fn product(&mut self) -> Option<CalcValue> {
        let mut acc = self.atom()?;
        loop {
            self.ws();
            let op = match self.s.get(self.i) {
                Some(b'*') => b'*',
                Some(b'/') => b'/',
                _ => return Some(acc),
            };
            self.i += 1;
            let rhs = self.atom()?;
            acc = match op {
                b'*' if rhs.2 => (acc.0 * rhs.0, acc.1 * rhs.0, acc.2),
                b'*' if acc.2 => (rhs.0 * acc.0, rhs.1 * acc.0, false),
                b'/' if rhs.2 && rhs.0 != 0.0 => (acc.0 / rhs.0, acc.1 / rhs.0, acc.2),
                _ => return None,
            };
        }
    }

    fn atom(&mut self) -> Option<CalcValue> {
        self.ws();
        let rest = &self.s[self.i..];
        if rest.starts_with(b"(") || rest.starts_with(b"calc(") {
            if self.depth >= MAX_NESTING {
                return None;
            }
            self.i += if rest[0] == b'(' { 1 } else { 5 };
            self.depth += 1;
            let v = self.sum()?;
            self.depth -= 1;
            self.ws();
            if self.s.get(self.i) != Some(&b')') {
                return None;
            }
            self.i += 1;
            return Some(v);
        }
        let start = self.i;
        if matches!(self.s.get(self.i), Some(b'-' | b'+')) {
            self.i += 1;
        }
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || *c == b'.') {
            self.i += 1;
        }
        let num: f32 = std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()?;
        let unit_start = self.i;
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'%') {
            self.i += 1;
        }
        let unit = std::str::from_utf8(&self.s[unit_start..self.i]).ok()?;
        if unit.is_empty() {
            return Some((num, 0.0, true));
        }
        match unit_px(num, unit, self.font_size)? {
            (p, true) => Some((0.0, p, false)),
            (px, false) => Some((px, 0.0, false)),
        }
    }
}

/// Splits a value into space-separated tokens, keeping `rgb(...)` together.
pub(crate) fn tokens(v: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start: Option<usize> = None;
    for (i, c) in v.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if c.is_whitespace() && depth == 0 {
            if let Some(s) = start.take() {
                out.push(&v[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push(&v[s..]);
    }
    out
}

fn hex_digit(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

fn clamp_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

fn parse_component(s: &str, max: f32) -> Option<f32> {
    let s = s.trim();
    if let Some(p) = s.strip_suffix('%') {
        return Some(p.trim().parse::<f32>().ok()? / 100.0 * max);
    }
    s.parse::<f32>().ok()
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let h = h.rem_euclid(360.0) / 360.0;
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
}

/// Parses a CSS color. `transparent` is fully transparent; `currentcolor`
/// and unknown values are `None`.
pub(crate) fn parse_color(v: &str) -> Option<Color32> {
    let v = v.trim().to_ascii_lowercase();
    if let Some(hex) = v.strip_prefix('#') {
        let b = hex.as_bytes();
        let d: Option<Vec<u8>> = b.iter().map(|c| hex_digit(*c)).collect();
        let d = d?;
        return Some(match d.len() {
            3 => Color32::from_rgb(d[0] * 17, d[1] * 17, d[2] * 17),
            4 => Color32::from_rgba_unmultiplied(d[0] * 17, d[1] * 17, d[2] * 17, d[3] * 17),
            6 => Color32::from_rgb(d[0] * 16 + d[1], d[2] * 16 + d[3], d[4] * 16 + d[5]),
            8 => {
                Color32::from_rgba_unmultiplied(d[0] * 16 + d[1], d[2] * 16 + d[3], d[4] * 16 + d[5], d[6] * 16 + d[7])
            }
            _ => return None,
        });
    }
    if let Some((func, args)) = v.split_once('(') {
        let args = args.trim_end_matches(')');
        let parts: Vec<&str> = args.split([',', ' ', '/']).map(str::trim).filter(|s| !s.is_empty()).collect();
        if parts.len() < 3 {
            return None;
        }
        let alpha = parts.get(3).and_then(|a| parse_component(a, 1.0)).unwrap_or(1.0);
        return match func.trim() {
            "rgb" | "rgba" => Some(Color32::from_rgba_unmultiplied(
                clamp_u8(parse_component(parts[0], 255.0)?),
                clamp_u8(parse_component(parts[1], 255.0)?),
                clamp_u8(parse_component(parts[2], 255.0)?),
                clamp_u8(alpha * 255.0),
            )),
            "hsl" | "hsla" => {
                let h: f32 = parts[0].trim_end_matches("deg").parse().ok()?;
                let s = parse_component(parts[1], 1.0)?;
                let l = parse_component(parts[2], 1.0)?;
                let (r, g, b) = hsl_to_rgb(h, s, l);
                Some(Color32::from_rgba_unmultiplied(
                    clamp_u8(r * 255.0),
                    clamp_u8(g * 255.0),
                    clamp_u8(b * 255.0),
                    clamp_u8(alpha * 255.0),
                ))
            }
            _ => None,
        };
    }
    if v == "transparent" {
        return Some(Color32::TRANSPARENT);
    }
    NAMED_COLORS.binary_search_by(|(n, _)| n.cmp(&v.as_str())).ok().map(|i| {
        let rgb = NAMED_COLORS[i].1;
        Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    })
}

/// First color found in a `background` shorthand (gradients use their first
/// color stop).
pub(crate) fn background_color(v: &str) -> Option<Color32> {
    let lower = v.to_ascii_lowercase();
    if let Some(start) = lower.find("gradient(") {
        let inner = &lower[start + 9..];
        for part in split_top_level(inner.trim_end_matches(')'), ',') {
            for t in tokens(part) {
                if let Some(c) = parse_color(t) {
                    return Some(c);
                }
            }
        }
        return None;
    }
    tokens(&lower).into_iter().filter(|t| !t.starts_with("url(")).find_map(parse_color)
}

/// CSS named colors, sorted by name.
const NAMED_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_colors_are_sorted() {
        assert!(NAMED_COLORS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#fff"), Some(Color32::WHITE));
        assert_eq!(parse_color("#336699"), Some(Color32::from_rgb(0x33, 0x66, 0x99)));
        assert_eq!(parse_color("rgb(255, 0, 0)"), Some(Color32::from_rgb(255, 0, 0)));
        assert_eq!(parse_color("rgb(100% 0% 0%)"), Some(Color32::from_rgb(255, 0, 0)));
        assert_eq!(parse_color("rgba(0,0,0,0.5)"), Some(Color32::from_rgba_unmultiplied(0, 0, 0, 128)));
        assert_eq!(parse_color("hsl(120, 100%, 25%)"), Some(Color32::from_rgb(0, 128, 0)));
        assert_eq!(parse_color("RebeccaPurple"), Some(Color32::from_rgb(0x66, 0x33, 0x99)));
        assert_eq!(parse_color("transparent"), Some(Color32::TRANSPARENT));
        assert_eq!(parse_color("currentColor"), None);
        assert_eq!(parse_color("#12"), None);
        assert_eq!(background_color("url(x.png) no-repeat #eee"), Some(Color32::from_gray(0xee)));
        assert_eq!(background_color("linear-gradient(to right, #f00, #00f)"), Some(Color32::from_rgb(255, 0, 0)));
    }

    #[test]
    fn lengths() {
        assert_eq!(parse_length("12px", 16.0), Some(Len::Px(12.0)));
        assert_eq!(parse_length("1.5em", 10.0), Some(Len::Px(15.0)));
        assert_eq!(parse_length("2rem", 10.0), Some(Len::Px(32.0)));
        assert_eq!(parse_length("50%", 10.0), Some(Len::Percent(50.0)));
        assert_eq!(parse_length("12pt", 16.0), Some(Len::Px(16.0)));
        assert_eq!(parse_length("auto", 16.0), Some(Len::Auto));
        assert_eq!(parse_length("big", 16.0), None);
        assert_eq!(Len::Percent(50.0).resolve(200.0), Some(100.0));
    }

    #[test]
    fn media_width_queries() {
        let applies = |q: &str, w: Option<f32>| media_applies(q, w, &mut Vec::new());
        assert!(applies("screen", None) && applies("all", Some(10.0)) && applies("(hover: hover)", None));
        assert!(!applies("print", Some(800.0)) && !applies("not screen", Some(800.0)));
        assert!(!applies("(prefers-color-scheme: dark)", Some(800.0)));
        // Width queries need a known width.
        assert!(!applies("(max-width: 600px)", None));
        for (q, yes, no) in [
            ("(max-width: 600px)", 600.0, 601.0),
            ("screen and (min-width: 40em)", 640.0, 639.0),
            ("only screen and (min-width: 400px) and (max-width: 800px)", 400.0, 801.0),
            ("(width >= 600px)", 600.0, 599.0),
            ("(width < 600px)", 599.0, 600.0),
            ("(400px <= width <= 800px)", 800.0, 399.0),
            ("(600px > width)", 599.0, 600.0),
            ("print, (max-width: 500px)", 500.0, 501.0),
        ] {
            assert!(applies(q, Some(yes)), "{q} at {yes}");
            assert!(!applies(q, Some(no)), "{q} at {no}");
        }
        let mut queries = Vec::new();
        media_applies("(min-width: 600px), (max-width: 300px)", Some(100.0), &mut queries);
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0], WidthQuery { min: 600.0, max: f32::INFINITY });
        assert!(!media_applies("(min-width: wide)", Some(100.0), &mut queries));
    }

    #[test]
    fn min_max_clamp() {
        let at = |v: &str, of: f32| parse_length(v, 10.0).and_then(|l| l.resolve(of));
        // Px-only arguments fold at parse time.
        assert_eq!(parse_length("min(10px, 2em, 5px)", 10.0), Some(Len::Px(5.0)));
        assert_eq!(parse_length("max(10px, 2em)", 10.0), Some(Len::Px(20.0)));
        assert_eq!(parse_length("clamp(1rem, 2.5vw, 2rem)", 10.0), Some(Len::Px(20.0)));
        // Percentages need the containing block.
        assert_eq!(at("min(100%, 600px)", 1000.0), Some(600.0));
        assert_eq!(at("min(100%, 600px)", 400.0), Some(400.0));
        assert_eq!(at("max(50%, 200px)", 300.0), Some(200.0));
        assert_eq!(at("max(50%, 200px)", 1000.0), Some(500.0));
        assert_eq!(at("clamp(200px, 50%, 800px)", 2000.0), Some(800.0));
        assert_eq!(at("clamp(200px, 50%, 800px)", 100.0), Some(200.0));
        assert_eq!(at("clamp(200px, 50%, 800px)", 1000.0), Some(500.0));
        assert_eq!(at("min(100% - 2rem, 40em)", 300.0), Some(268.0));
        assert_eq!(at("MIN(100%,600PX)", 100.0), Some(100.0));
        // Three kinds of terms, wrong arity and unitless numbers are rejected.
        assert_eq!(parse_length("min(10px, 50%, 100% - 1px)", 10.0), None);
        assert_eq!(parse_length("clamp(1px, 2px)", 10.0), None);
        assert_eq!(parse_length("min(2, 3px)", 10.0), None);
    }

    #[test]
    fn deep_nesting_is_rejected_without_recursing() {
        let n = 100_000;
        let calc = format!("calc({}1px{})", "(".repeat(n), ")".repeat(n));
        assert_eq!(parse_length(&calc, 16.0), None);
        assert_eq!(parse_length("calc(((1px + 2px)) * 2)", 16.0), Some(Len::Px(6.0)));
        assert!(Selector::parse(&format!("p{}.a{}", ":not(".repeat(n), ")".repeat(n))).is_none());
        assert!(matches("p:not(:not(#p))", "<p id=p>t</p>", "p"));
        let mut s = Stylesheet::default();
        s.add(&format!("{}p {{ color: red }}{}", "@media screen {".repeat(n), "}".repeat(n)));
        assert!(s.rules.is_empty());
        s.add("@supports (display: grid) { @media screen { @layer x { p { color: blue } } } }");
        assert_eq!(s.rules.len(), 1);
    }

    #[test]
    fn stylesheet_rules_and_at_rules() {
        let mut s = Stylesheet::default();
        s.add(
            "/* c */ @import url('x.css'); @charset 'utf-8'; h1, .a > b { color: red; margin: 0 !important }\n\
             @media print { p { color: blue } } @media screen { p { color: green } } @font-face { src: x }",
        );
        assert_eq!(s.imports, ["x.css"]);
        assert_eq!(s.rules.len(), 3);
        assert_eq!(s.rules[0].decls[1], Decl { name: "margin".into(), value: "0".into(), important: true });
        assert_eq!(s.rules[2].decls[0].value, "green");
        assert_eq!(s.rules[1].order, 1);
    }

    #[test]
    fn declarations() {
        let d = parse_declarations("color: red; background: url('a;b.png') #fff; ; bad; font-family: \"A; B\"");
        assert_eq!(d.len(), 3);
        assert_eq!(d[1].value, "url('a;b.png') #fff");
        assert_eq!(d[2].value, "\"A; B\"");
        // Whitespace may follow the `!`; `!important` must end the value.
        let d = parse_declarations("color: blue ! IMPORTANT; margin: 0 !important x; width: 1px!important");
        assert_eq!(d[0], Decl { name: "color".into(), value: "blue".into(), important: true });
        assert!(!d[1].important);
        assert_eq!(d[2], Decl { name: "width".into(), value: "1px".into(), important: true });
    }

    fn matches(sel: &str, html: &str, id: &str) -> bool {
        let dom = Dom::parse(html);
        let el = (0..dom.nodes.len()).find(|n| dom.attr(*n, "id") == Some(id)).unwrap();
        Selector::parse(sel).unwrap_or_else(|| panic!("parse {sel}")).matches(&dom, el)
    }

    #[test]
    fn selector_matching() {
        let html = "<div class='box main' id=root><ul><li id=a>1</li><li id=b class=x>2</li><li id=c>3</li></ul>\
                    <p id=p lang=en-US data-k='v w'>t</p></div>";
        assert!(matches("li", html, "a"));
        assert!(matches("div li", html, "a"));
        assert!(matches(".box.main > ul > li.x", html, "b"));
        assert!(!matches("div > li", html, "a"));
        assert!(matches("#root", html, "root"));
        assert!(matches("li:first-child", html, "a"));
        assert!(matches("li:last-child", html, "c"));
        assert!(matches("li:nth-child(even)", html, "b"));
        assert!(matches("li:nth-child(2n+1)", html, "c"));
        assert!(!matches("li:nth-child(odd)", html, "b"));
        assert!(matches("li:nth-of-type(3)", html, "c"));
        assert!(matches("li:nth-last-child(1)", html, "c"));
        assert!(matches("li:not(.x)", html, "a"));
        assert!(!matches("li:not(.x)", html, "b"));
        assert!(matches("li + li", html, "b"));
        assert!(matches("#a ~ #c", html, "c"));
        assert!(matches("ul + p", html, "p"));
        assert!(matches("[lang|=en]", html, "p"));
        assert!(matches("[data-k~=w]", html, "p"));
        assert!(matches("p[lang^=en]", html, "p"));
        // Extreme `an+b` values must not overflow.
        assert!(!matches("li:nth-child(-n-2147483647)", html, "a"));
        assert!(!matches("li:nth-child(-2147483648n-2147483648)", html, "a"));
        assert!(matches("li:nth-last-child(2147483647n+1)", html, "c"));
        assert!(!matches("li:hover", html, "a"));
        assert!(!matches("p::before", html, "p"));
        assert!(matches("*", html, "p"));
        assert!(Selector::parse("a >").is_none());
        assert!(Selector::parse("").is_none());
    }

    #[test]
    fn specificity_order() {
        let s = |x: &str| Selector::parse(x).unwrap().specificity();
        assert!(s("#a") > s(".a.b.c"));
        assert!(s(".a") > s("div p span"));
        assert!(s("tr:nth-child(even)") > s("tr"));
        assert_eq!(s("li:not(#x)"), s("li#x"));
    }
}
