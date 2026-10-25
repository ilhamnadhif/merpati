//! Smithy IDL lexer and parser. Produces a [`RawModel`] with relative ids;
//! [`super::resolve`] turns it into a [`super::Model`].

use indexmap::IndexMap;
use serde_json::{Map, Number, Value};

use super::resolve::{RawApply, RawModel, RawShape};
use super::{Diagnostic, Member, Pos, Shape, ShapeKind, ShapeRef};

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// Identifier, namespace, or shape id (`a.b#C$d`).
    Ident(String),
    Str(String),
    Num(Number),
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Colon,
    /// `:=`
    Walrus,
    Eq,
    At,
    Dollar,
    Eof,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    pos: Pos,
    /// `///` lines directly before this token.
    docs: Vec<String>,
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Ident(s) => format!("`{s}`"),
        Tok::Str(s) => format!("string \"{}\"", s.chars().take(30).collect::<String>()),
        Tok::Num(n) => format!("number {n}"),
        Tok::LBrace => "`{`".into(),
        Tok::RBrace => "`}`".into(),
        Tok::LBracket => "`[`".into(),
        Tok::RBracket => "`]`".into(),
        Tok::LParen => "`(`".into(),
        Tok::RParen => "`)`".into(),
        Tok::Colon => "`:`".into(),
        Tok::Walrus => "`:=`".into(),
        Tok::Eq => "`=`".into(),
        Tok::At => "`@`".into(),
        Tok::Dollar => "`$`".into(),
        Tok::Eof => "end of file".into(),
    }
}

struct Lexer {
    chars: Vec<char>,
    i: usize,
    line: u32,
    col: u32,
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl Lexer {
    fn peek(&self, off: usize) -> Option<char> {
        self.chars.get(self.i + off).copied()
    }

    fn pos(&self) -> Pos {
        Pos::new(self.line, self.col)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.i).copied()?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn ident_part(&mut self, out: &mut String) {
        while let Some(c) = self.peek(0) {
            if is_ident_char(c) {
                out.push(c);
                self.bump();
            } else {
                break;
            }
        }
    }

    fn tokens(mut self) -> Result<Vec<Token>, Diagnostic> {
        let mut out = Vec::new();
        let mut docs: Vec<String> = Vec::new();
        while let Some(c) = self.peek(0) {
            let pos = self.pos();
            if c.is_whitespace() || c == ',' || c == '\u{feff}' {
                self.bump();
                continue;
            }
            if c == '/' {
                if self.peek(1) != Some('/') {
                    return Err(Diagnostic::error(pos, "unexpected `/` (comments start with `//`)"));
                }
                let doc = self.peek(2) == Some('/');
                let mut text = String::new();
                while let Some(ch) = self.peek(0) {
                    if ch == '\n' {
                        break;
                    }
                    text.push(ch);
                    self.bump();
                }
                if doc {
                    let body = text.trim_start_matches('/');
                    let body = body.strip_prefix(' ').unwrap_or(body);
                    docs.push(body.trim_end_matches('\r').to_string());
                }
                continue;
            }
            let tok = match c {
                '{' => self.single(Tok::LBrace),
                '}' => self.single(Tok::RBrace),
                '[' => self.single(Tok::LBracket),
                ']' => self.single(Tok::RBracket),
                '(' => self.single(Tok::LParen),
                ')' => self.single(Tok::RParen),
                '=' => self.single(Tok::Eq),
                '@' => self.single(Tok::At),
                '$' => self.single(Tok::Dollar),
                ':' => {
                    self.bump();
                    if self.peek(0) == Some('=') {
                        self.bump();
                        Tok::Walrus
                    } else {
                        Tok::Colon
                    }
                }
                '"' => {
                    if self.peek(1) == Some('"') && self.peek(2) == Some('"') {
                        Tok::Str(self.text_block(pos)?)
                    } else {
                        Tok::Str(self.quoted(pos)?)
                    }
                }
                '-' | '0'..='9' => Tok::Num(self.number(pos)?),
                c if is_ident_start(c) => Tok::Ident(self.shape_id(pos)?),
                other => return Err(Diagnostic::error(pos, format!("unexpected character `{other}`"))),
            };
            out.push(Token { tok, pos, docs: std::mem::take(&mut docs) });
        }
        out.push(Token { tok: Tok::Eof, pos: self.pos(), docs });
        Ok(out)
    }

    fn single(&mut self, t: Tok) -> Tok {
        self.bump();
        t
    }

    /// `ns.a.b#Name$member`, `Name`, `ns.a`.
    fn shape_id(&mut self, start: Pos) -> Result<String, Diagnostic> {
        let mut s = String::new();
        // `__x` is valid, `_` alone or `__` is not, but be lenient.
        self.ident_part(&mut s);
        while self.peek(0) == Some('.') && self.peek(1).is_some_and(is_ident_start) {
            s.push('.');
            self.bump();
            self.ident_part(&mut s);
        }
        if self.peek(0) == Some('#') {
            if !self.peek(1).is_some_and(is_ident_start) {
                return Err(Diagnostic::error(start, format!("invalid shape id `{s}#`")));
            }
            s.push('#');
            self.bump();
            self.ident_part(&mut s);
        }
        if self.peek(0) == Some('$') && self.peek(1).is_some_and(is_ident_start) {
            s.push('$');
            self.bump();
            self.ident_part(&mut s);
        }
        Ok(s)
    }

    fn number(&mut self, start: Pos) -> Result<Number, Diagnostic> {
        let mut s = String::new();
        if self.peek(0) == Some('-') {
            s.push('-');
            self.bump();
        }
        let digits = |lx: &mut Lexer, s: &mut String| {
            let mut n = 0;
            while let Some(c) = lx.peek(0).filter(char::is_ascii_digit) {
                s.push(c);
                lx.bump();
                n += 1;
            }
            n
        };
        if digits(self, &mut s) == 0 {
            return Err(Diagnostic::error(start, "invalid number"));
        }
        let mut float = false;
        if self.peek(0) == Some('.') && self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
            float = true;
            s.push('.');
            self.bump();
            digits(self, &mut s);
        }
        if matches!(self.peek(0), Some('e' | 'E')) {
            float = true;
            s.push('e');
            self.bump();
            if let Some(sign @ ('+' | '-')) = self.peek(0) {
                s.push(sign);
                self.bump();
            }
            if digits(self, &mut s) == 0 {
                return Err(Diagnostic::error(start, "invalid number exponent"));
            }
        }
        if self.peek(0).is_some_and(is_ident_char) {
            return Err(Diagnostic::error(self.pos(), "invalid character in number"));
        }
        let parsed = if float {
            s.parse::<f64>().ok().and_then(Number::from_f64)
        } else {
            s.parse::<i64>().map(Number::from).ok().or_else(|| s.parse::<f64>().ok().and_then(Number::from_f64))
        };
        parsed.ok_or_else(|| Diagnostic::error(start, format!("invalid number `{s}`")))
    }

    fn escape(&mut self, out: &mut String) -> Result<(), Diagnostic> {
        let pos = self.pos();
        self.bump(); // backslash
        let Some(c) = self.bump() else { return Err(Diagnostic::error(pos, "unterminated escape")) };
        match c {
            '"' => out.push('"'),
            '\'' => out.push('\''),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\n' => {} // line continuation
            '\r' if self.peek(0) == Some('\n') => {
                self.bump();
            }
            'u' => {
                let mut code = 0u32;
                for _ in 0..4 {
                    let d = self.bump().and_then(|c| c.to_digit(16));
                    match d {
                        Some(d) => code = code * 16 + d,
                        None => return Err(Diagnostic::error(pos, "invalid unicode escape")),
                    }
                }
                // Surrogate pairs: 😀
                if (0xD800..0xDC00).contains(&code) && self.peek(0) == Some('\\') && self.peek(1) == Some('u') {
                    self.bump();
                    self.bump();
                    let mut low = 0u32;
                    for _ in 0..4 {
                        match self.bump().and_then(|c| c.to_digit(16)) {
                            Some(d) => low = low * 16 + d,
                            None => return Err(Diagnostic::error(pos, "invalid unicode escape")),
                        }
                    }
                    code = 0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                }
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            other => return Err(Diagnostic::error(pos, format!("invalid escape `\\{other}`"))),
        }
        Ok(())
    }

    fn quoted(&mut self, start: Pos) -> Result<String, Diagnostic> {
        self.bump();
        let mut out = String::new();
        loop {
            match self.peek(0) {
                None => return Err(Diagnostic::error(start, "unterminated string")),
                Some('"') => {
                    self.bump();
                    return Ok(out);
                }
                Some('\\') => self.escape(&mut out)?,
                Some(c) => {
                    out.push(c);
                    self.bump();
                }
            }
        }
    }

    /// `"""` text blocks with incidental indentation removed.
    fn text_block(&mut self, start: Pos) -> Result<String, Diagnostic> {
        for _ in 0..3 {
            self.bump();
        }
        // The opening delimiter must be followed by a newline.
        while self.peek(0).is_some_and(|c| c == ' ' || c == '\t' || c == '\r') {
            self.bump();
        }
        if self.peek(0) != Some('\n') {
            return Err(Diagnostic::error(start, "text block must start with a new line after `\"\"\"`"));
        }
        self.bump();
        // Collect raw content (escapes kept) up to the closing delimiter.
        let mut raw = String::new();
        loop {
            match self.peek(0) {
                None => return Err(Diagnostic::error(start, "unterminated text block")),
                Some('"') if self.peek(1) == Some('"') && self.peek(2) == Some('"') => {
                    for _ in 0..3 {
                        self.bump();
                    }
                    break;
                }
                Some('\\') => {
                    raw.push('\\');
                    self.bump();
                    if let Some(c) = self.bump() {
                        raw.push(c);
                    }
                }
                Some(c) => {
                    raw.push(c);
                    self.bump();
                }
            }
        }
        let raw = raw.replace("\r\n", "\n");
        let lines: Vec<&str> = raw.split('\n').collect();
        // The last line (before the closing delimiter) counts for indentation
        // even when it is whitespace only.
        let last = lines.len() - 1;
        let indent = lines
            .iter()
            .enumerate()
            .filter(|(i, l)| *i == last || !l.trim().is_empty())
            .map(|(_, l)| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
            .min()
            .unwrap_or(0);
        let mut joined = String::new();
        for (i, l) in lines.iter().enumerate() {
            let stripped: String = l.chars().skip(indent).collect();
            let stripped = stripped.trim_end_matches([' ', '\t']);
            if i == last {
                joined.push_str(stripped);
            } else {
                joined.push_str(stripped);
                joined.push('\n');
            }
        }
        // Process escapes.
        let mut sub = Lexer { chars: joined.chars().collect(), i: 0, line: start.line, col: start.column };
        let mut out = String::new();
        while let Some(c) = sub.peek(0) {
            if c == '\\' {
                sub.escape(&mut out)?;
            } else {
                out.push(c);
                sub.bump();
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Node values (with positions, so shape references can be reported)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Node {
    Null,
    Bool(bool),
    Num(Number),
    Str(String),
    /// Unquoted shape id.
    Id(String),
    Array(Vec<(Node, Pos)>),
    Object(Vec<(String, Pos, Node, Pos)>),
}

impl Node {
    fn to_json(&self) -> Value {
        match self {
            Node::Null => Value::Null,
            Node::Bool(b) => Value::Bool(*b),
            Node::Num(n) => Value::Number(n.clone()),
            Node::Str(s) | Node::Id(s) => Value::String(s.clone()),
            Node::Array(items) => Value::Array(items.iter().map(|(n, _)| n.to_json()).collect()),
            Node::Object(kvs) => {
                let mut m = Map::new();
                for (k, _, v, _) in kvs {
                    m.insert(k.clone(), v.to_json());
                }
                Value::Object(m)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    toks: Vec<Token>,
    i: usize,
    raw: RawModel,
    input_suffix: String,
    output_suffix: String,
    /// Nesting of the node value being parsed (see [`MAX_NODE_DEPTH`]).
    depth: usize,
}

/// Node values nested deeper are a syntax error (the parser, `Node` and
/// its JSON are recursive). Same limit as `serde_json` for the JSON AST.
const MAX_NODE_DEPTH: usize = 128;

type Res<T> = Result<T, Diagnostic>;

/// Traits collected before a shape or member, plus `///` docs.
#[derive(Default)]
struct TraitList {
    traits: Vec<(String, Value, Pos)>,
    docs: Vec<String>,
}

pub(super) fn looks_like_idl(t: &str) -> bool {
    let mut saw_namespace = false;
    for line in t.lines().take(200) {
        let l = line.trim();
        if l.is_empty() || l.starts_with("//") {
            continue;
        }
        if l.starts_with("$version") {
            return true;
        }
        if let Some(rest) = l.strip_prefix("namespace ") {
            let ns = rest.trim();
            if !ns.is_empty() && ns.chars().all(|c| is_ident_char(c) || c == '.') {
                saw_namespace = true;
                continue;
            }
        }
        if saw_namespace {
            let first = l.split_whitespace().next().unwrap_or("");
            if first == "use" || first == "apply" || first.starts_with('@') || ShapeKind::from_keyword(first).is_some()
            {
                return true;
            }
        }
    }
    false
}

pub(super) fn parse(text: &str) -> Result<RawModel, Diagnostic> {
    let lexer = Lexer { chars: text.chars().collect(), i: 0, line: 1, col: 1 };
    let toks = lexer.tokens()?;
    let mut p = Parser {
        toks,
        i: 0,
        raw: RawModel::default(),
        input_suffix: "Input".into(),
        output_suffix: "Output".into(),
        depth: 0,
    };
    p.file()?;
    Ok(p.raw)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }

    fn peek_at(&self, off: usize) -> &Tok {
        &self.toks[(self.i + off).min(self.toks.len() - 1)].tok
    }

    fn pos(&self) -> Pos {
        self.toks[self.i].pos
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.i].clone();
        if self.i < self.toks.len() - 1 {
            self.i += 1;
        }
        t
    }

    fn err<T>(&self, msg: impl Into<String>) -> Res<T> {
        Err(Diagnostic::error(self.pos(), msg))
    }

    fn unexpected<T>(&self, expected: &str) -> Res<T> {
        self.err(format!("expected {expected}, found {}", describe(self.peek())))
    }

    fn expect(&mut self, t: Tok, what: &str) -> Res<Pos> {
        if *self.peek() == t { Ok(self.bump().pos) } else { self.unexpected(what) }
    }

    fn is_ident(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == kw)
    }

    fn ident(&mut self, what: &str) -> Res<(String, Pos)> {
        match self.peek() {
            Tok::Ident(s) => {
                let s = s.clone();
                let pos = self.bump().pos;
                Ok((s, pos))
            }
            _ => self.unexpected(what),
        }
    }

    /// A plain identifier: no namespace, `#` or `$`.
    fn simple_ident(&mut self, what: &str) -> Res<(String, Pos)> {
        let (s, pos) = self.ident(what)?;
        if s.contains(['.', '#', '$']) {
            return Err(Diagnostic::error(pos, format!("expected {what}, found shape id `{s}`")));
        }
        Ok((s, pos))
    }

    fn file(&mut self) -> Res<()> {
        // Control section.
        while *self.peek() == Tok::Dollar && matches!(self.peek_at(1), Tok::Ident(_) | Tok::Str(_)) {
            self.control()?;
        }
        // Metadata section.
        while self.is_ident("metadata") && !matches!(self.peek_at(1), Tok::Colon | Tok::Walrus) {
            self.metadata()?;
        }
        if self.is_ident("namespace") {
            self.bump();
            let (ns, pos) = self.ident("a namespace")?;
            if ns.contains(['#', '$']) {
                return Err(Diagnostic::error(pos, format!("invalid namespace `{ns}`")));
            }
            self.raw.namespace = ns;
        }
        while self.is_ident("use") {
            let use_pos = self.bump().pos;
            let (id, pos) = self.ident("an absolute shape id")?;
            let Some((_, name)) = id.split_once('#') else {
                return Err(Diagnostic::error(
                    pos,
                    format!("`use` needs an absolute shape id (`ns#Name`), found `{id}`"),
                ));
            };
            if name.contains('$') {
                return Err(Diagnostic::error(pos, "`use` cannot import a member"));
            }
            if let Some((prev, _)) = self.raw.uses.get(name)
                && *prev != id
            {
                return Err(Diagnostic::error(use_pos, format!("`{name}` is already imported as `{prev}`")));
            }
            self.raw.uses.insert(name.to_string(), (id, pos));
        }
        loop {
            match self.peek() {
                Tok::Eof => break,
                Tok::Dollar => return self.err("control statements (`$version: ...`) must come first"),
                Tok::Ident(s) if s == "metadata" => return self.err("metadata must come before the namespace"),
                Tok::Ident(s) if s == "namespace" => return self.err("only one namespace is allowed per file"),
                Tok::Ident(s) if s == "use" => return self.err("`use` statements must come before shapes"),
                Tok::Ident(s) if s == "apply" => self.apply()?,
                _ => {
                    if self.raw.namespace.is_empty() {
                        return self.err("expected `namespace` before shape definitions");
                    }
                    self.shape_statement()?;
                }
            }
        }
        Ok(())
    }

    fn node_key(&mut self) -> Res<(String, Pos)> {
        match self.peek().clone() {
            Tok::Ident(s) => Ok((s, self.bump().pos)),
            Tok::Str(s) => Ok((s, self.bump().pos)),
            _ => self.unexpected("a key"),
        }
    }

    fn control(&mut self) -> Res<()> {
        self.bump(); // $
        let (key, pos) = self.node_key()?;
        self.expect(Tok::Colon, "`:`")?;
        let (value, _) = self.node_value()?;
        let json = value.to_json();
        match key.as_str() {
            "version" => {
                let v = match &json {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => return Err(Diagnostic::error(pos, "`$version` must be a string")),
                };
                if !(v == "1" || v == "2" || v.starts_with("1.") || v.starts_with("2.")) {
                    return Err(Diagnostic::error(pos, format!("unsupported Smithy IDL version `{v}`")));
                }
                self.raw.version = if v.contains('.') { v } else { format!("{v}.0") };
            }
            "operationInputSuffix" => self.input_suffix = json.as_str().unwrap_or("Input").to_string(),
            "operationOutputSuffix" => self.output_suffix = json.as_str().unwrap_or("Output").to_string(),
            other => {
                self.raw.diagnostics.push(Diagnostic::warning(pos, format!("unknown control statement `${other}`")))
            }
        }
        Ok(())
    }

    fn metadata(&mut self) -> Res<()> {
        self.bump();
        let (key, pos) = self.node_key()?;
        self.expect(Tok::Eq, "`=`")?;
        let (value, _) = self.node_value()?;
        // Repeated keys are merged (or reported) with the other files' metadata.
        self.raw.metadata.push((key, value.to_json(), pos));
        Ok(())
    }

    fn node_value(&mut self) -> Res<(Node, Pos)> {
        if self.depth >= MAX_NODE_DEPTH {
            return self.err(format!("node value nested deeper than {MAX_NODE_DEPTH} levels"));
        }
        self.depth += 1;
        let node = self.node_value_inner();
        self.depth -= 1;
        node
    }

    fn node_value_inner(&mut self) -> Res<(Node, Pos)> {
        let pos = self.pos();
        let node = match self.peek().clone() {
            Tok::LBrace => {
                self.bump();
                let mut kvs = Vec::new();
                while *self.peek() != Tok::RBrace {
                    let (k, kp) = self.node_key()?;
                    self.expect(Tok::Colon, "`:`")?;
                    let (v, vp) = self.node_value()?;
                    kvs.push((k, kp, v, vp));
                }
                self.bump();
                Node::Object(kvs)
            }
            Tok::LBracket => {
                self.bump();
                let mut items = Vec::new();
                while *self.peek() != Tok::RBracket {
                    if *self.peek() == Tok::Eof {
                        return self.unexpected("`]`");
                    }
                    items.push(self.node_value()?);
                }
                self.bump();
                Node::Array(items)
            }
            Tok::Str(s) => {
                self.bump();
                Node::Str(s)
            }
            Tok::Num(n) => {
                self.bump();
                Node::Num(n)
            }
            Tok::Ident(s) => {
                self.bump();
                match s.as_str() {
                    "true" => Node::Bool(true),
                    "false" => Node::Bool(false),
                    "null" => Node::Null,
                    _ => Node::Id(s),
                }
            }
            _ => return self.unexpected("a value"),
        };
        Ok((node, pos))
    }

    fn trait_(&mut self) -> Res<(String, Value, Pos)> {
        let at = self.expect(Tok::At, "`@`")?;
        let (name, _) = self.ident("a trait name")?;
        if *self.peek() != Tok::LParen {
            return Ok((name, Value::Object(Map::new()), at));
        }
        self.bump();
        if *self.peek() == Tok::RParen {
            self.bump();
            return Ok((name, Value::Object(Map::new()), at));
        }
        let structured = matches!(self.peek(), Tok::Ident(_) | Tok::Str(_)) && *self.peek_at(1) == Tok::Colon;
        let value = if structured {
            let mut m = Map::new();
            while *self.peek() != Tok::RParen {
                let (k, kp) = self.node_key()?;
                self.expect(Tok::Colon, "`:`")?;
                let (v, _) = self.node_value()?;
                if m.insert(k.clone(), v.to_json()).is_some() {
                    return Err(Diagnostic::error(kp, format!("duplicate key `{k}` in trait `@{name}`")));
                }
                if *self.peek() == Tok::Eof {
                    return self.unexpected("`)`");
                }
            }
            Value::Object(m)
        } else {
            let (v, _) = self.node_value()?;
            if *self.peek() != Tok::RParen {
                return self.unexpected(&format!("`)` to close trait `@{name}`"));
            }
            v.to_json()
        };
        self.bump();
        Ok((name, value, at))
    }

    fn traits(&mut self) -> Res<TraitList> {
        let mut list = TraitList::default();
        loop {
            list.docs.extend(self.toks[self.i].docs.iter().cloned());
            if *self.peek() != Tok::At {
                break;
            }
            let t = self.trait_()?;
            list.traits.push(t);
        }
        Ok(list)
    }

    fn apply_traits(list: TraitList, traits: &mut IndexMap<String, Value>, pos_out: &mut IndexMap<String, Pos>) {
        let has_doc = list.traits.iter().any(|(n, _, _)| n == "documentation" || n == "smithy.api#documentation");
        if !list.docs.is_empty() && !has_doc {
            traits.insert("documentation".into(), Value::String(list.docs.join("\n")));
        }
        for (name, value, pos) in list.traits {
            pos_out.entry(name.clone()).or_insert(pos);
            traits.insert(name, value);
        }
    }

    fn mixins(&mut self) -> Res<Vec<ShapeRef>> {
        if !self.is_ident("with") || *self.peek_at(1) != Tok::LBracket {
            return Ok(Vec::new());
        }
        self.bump();
        self.bump();
        let mut out = Vec::new();
        while *self.peek() != Tok::RBracket {
            let (id, pos) = self.ident("a mixin shape id")?;
            out.push(ShapeRef { id, pos });
        }
        self.bump();
        Ok(out)
    }

    fn for_resource(&mut self) -> Res<Option<ShapeRef>> {
        if !self.is_ident("for") || !matches!(self.peek_at(1), Tok::Ident(_)) {
            return Ok(None);
        }
        self.bump();
        let (id, pos) = self.ident("a resource shape id")?;
        Ok(Some(ShapeRef { id, pos }))
    }

    fn shape_statement(&mut self) -> Res<()> {
        let list = self.traits()?;
        let (kw, kw_pos) = self.ident("a shape definition")?;
        let Some(kind) = ShapeKind::from_keyword(&kw) else {
            return Err(Diagnostic::error(kw_pos, format!("unknown shape type `{kw}`")));
        };
        let (name, name_pos) = self.simple_ident("a shape name")?;
        let mut raw = RawShape::new(Shape::new(name, kind, name_pos));
        Self::apply_traits(list, &mut raw.shape.traits, &mut raw.shape.trait_pos);
        match kind {
            ShapeKind::Enum | ShapeKind::IntEnum => {
                raw.shape.mixins = self.mixins()?;
                if *self.peek() == Tok::LBrace {
                    self.members(&mut raw)?;
                } else if raw.shape.mixins.is_empty() {
                    return self.unexpected("`{` with enum members");
                }
            }
            k if k.is_simple() => raw.shape.mixins = self.mixins()?,
            ShapeKind::List | ShapeKind::Set | ShapeKind::Map | ShapeKind::Structure | ShapeKind::Union => {
                raw.shape.for_resource = self.for_resource()?;
                raw.shape.mixins = self.mixins()?;
                self.members(&mut raw)?;
            }
            ShapeKind::Service | ShapeKind::Resource => {
                raw.shape.mixins = self.mixins()?;
                if *self.peek() == Tok::LBrace {
                    let (node, _) = self.node_value()?;
                    self.entity_body(&mut raw.shape, node)?;
                } else if raw.shape.mixins.is_empty() {
                    return self.unexpected("`{`");
                }
            }
            ShapeKind::Operation => {
                raw.shape.mixins = self.mixins()?;
                if *self.peek() == Tok::LBrace {
                    self.operation_body(&mut raw)?;
                } else if raw.shape.mixins.is_empty() {
                    return self.unexpected("`{`");
                }
            }
            _ => unreachable!(),
        }
        self.raw.shapes.push(raw);
        Ok(())
    }

    fn members(&mut self, raw: &mut RawShape) -> Res<()> {
        self.expect(Tok::LBrace, "`{`")?;
        let is_enum = matches!(raw.shape.kind, ShapeKind::Enum | ShapeKind::IntEnum);
        loop {
            let list = self.traits()?;
            if *self.peek() == Tok::RBrace {
                if !list.traits.is_empty() {
                    return self.unexpected("a member after the traits");
                }
                self.bump();
                break;
            }
            let (name, pos, target, elided) = if *self.peek() == Tok::Dollar {
                let dollar = self.bump().pos;
                let (name, _) = self.simple_ident("a member name")?;
                (name, dollar, String::new(), true)
            } else {
                let (name, pos) = self.simple_ident("a member name or `}`")?;
                if is_enum {
                    (name, pos, "smithy.api#Unit".to_string(), false)
                } else {
                    self.expect(Tok::Colon, &format!("`:` and a target after member `{name}`"))?;
                    let (target, _) = self.ident("a target shape id")?;
                    (name, pos, target, false)
                }
            };
            let mut member =
                Member { name: name.clone(), target, traits: IndexMap::new(), trait_pos: IndexMap::new(), pos };
            Self::apply_traits(list, &mut member.traits, &mut member.trait_pos);
            if *self.peek() == Tok::Eq {
                let eq = self.bump().pos;
                let (v, _) = self.node_value()?;
                let key = if is_enum { "enumValue" } else { "default" };
                member.trait_pos.insert(key.into(), eq);
                member.traits.insert(key.into(), v.to_json());
            }
            if is_enum && !member.traits.contains_key("enumValue") {
                if raw.shape.kind == ShapeKind::IntEnum {
                    return Err(Diagnostic::error(pos, format!("intEnum member `{name}` needs a value (`= 1`)")));
                }
                member.traits.insert("enumValue".into(), Value::String(name.clone()));
            }
            if raw.shape.members.contains_key(&name) {
                return Err(Diagnostic::error(pos, format!("duplicate member `{name}`")));
            }
            if elided {
                raw.elided.push(name.clone());
            }
            raw.shape.members.insert(name, member);
        }
        Ok(())
    }

    fn entity_body(&mut self, shape: &mut Shape, node: Node) -> Res<()> {
        let Node::Object(kvs) = node else { unreachable!() };
        let service = shape.kind == ShapeKind::Service;
        for (key, kpos, value, vpos) in kvs {
            let one = |v: &Node, p: Pos| -> Res<ShapeRef> {
                match v {
                    Node::Id(s) | Node::Str(s) => Ok(ShapeRef { id: s.clone(), pos: p }),
                    _ => Err(Diagnostic::error(p, format!("`{key}` must be a shape id"))),
                }
            };
            let list = |v: &Node, p: Pos| -> Res<Vec<ShapeRef>> {
                match v {
                    Node::Array(items) => items.iter().map(|(n, p)| one(n, *p)).collect(),
                    _ => Err(Diagnostic::error(p, format!("`{key}` must be a list of shape ids"))),
                }
            };
            let map = |v: &Node, p: Pos| -> Res<IndexMap<String, ShapeRef>> {
                match v {
                    Node::Object(kvs) => kvs.iter().map(|(k, _, n, p)| Ok((k.clone(), one(n, *p)?))).collect(),
                    _ => Err(Diagnostic::error(p, format!("`{key}` must be an object of shape ids"))),
                }
            };
            match (service, key.as_str()) {
                (true, "version") => match &value {
                    Node::Str(s) => shape.version = Some(s.clone()),
                    _ => return Err(Diagnostic::error(vpos, "`version` must be a string")),
                },
                (_, "operations") => shape.operations = list(&value, vpos)?,
                (_, "resources") => shape.resources = list(&value, vpos)?,
                (true, "errors") => shape.errors = list(&value, vpos)?,
                (true, "rename") => {
                    shape.traits.insert("smithy.api#rename".into(), value.to_json());
                }
                (false, "identifiers") => shape.identifiers = map(&value, vpos)?,
                (false, "properties") => shape.properties = map(&value, vpos)?,
                (false, "collectionOperations") => shape.collection_operations = list(&value, vpos)?,
                (false, k @ ("create" | "put" | "read" | "update" | "delete" | "list")) => {
                    shape.lifecycle.insert(k.to_string(), one(&value, vpos)?);
                }
                _ => {
                    return Err(Diagnostic::error(
                        kpos,
                        format!("unknown {} property `{key}`", if service { "service" } else { "resource" }),
                    ));
                }
            }
        }
        if !service {
            // Canonical lifecycle order.
            let order = ["create", "put", "read", "update", "delete", "list"];
            shape.lifecycle.sort_by_key(|k, _| order.iter().position(|o| o == k).unwrap_or(order.len()));
        }
        Ok(())
    }

    fn operation_body(&mut self, raw: &mut RawShape) -> Res<()> {
        self.expect(Tok::LBrace, "`{`")?;
        loop {
            if *self.peek() == Tok::RBrace {
                self.bump();
                break;
            }
            let (key, kpos) = self.simple_ident("`input`, `output`, `errors` or `}`")?;
            match key.as_str() {
                "input" | "output" => {
                    let target = if *self.peek() == Tok::Walrus {
                        let walrus = self.bump().pos;
                        let suffix = if key == "input" { &self.input_suffix } else { &self.output_suffix };
                        let name = format!("{}{suffix}", raw.shape.name());
                        let list = self.traits()?;
                        let mut inline = RawShape::new(Shape::new(name.clone(), ShapeKind::Structure, walrus));
                        Self::apply_traits(list, &mut inline.shape.traits, &mut inline.shape.trait_pos);
                        inline.shape.traits.insert(key.clone(), Value::Object(Map::new()));
                        inline.shape.for_resource = self.for_resource()?;
                        inline.shape.mixins = self.mixins()?;
                        self.members(&mut inline)?;
                        self.raw.shapes.push(inline);
                        ShapeRef { id: name, pos: walrus }
                    } else {
                        self.expect(Tok::Colon, "`:` or `:=`")?;
                        let (id, pos) = self.ident("a structure shape id")?;
                        ShapeRef { id, pos }
                    };
                    if key == "input" {
                        raw.shape.input = Some(target);
                    } else {
                        raw.shape.output = Some(target);
                    }
                }
                "errors" => {
                    self.expect(Tok::Colon, "`:`")?;
                    self.expect(Tok::LBracket, "`[`")?;
                    while *self.peek() != Tok::RBracket {
                        let (id, pos) = self.ident("an error shape id or `]`")?;
                        raw.shape.errors.push(ShapeRef { id, pos });
                    }
                    self.bump();
                }
                other => return Err(Diagnostic::error(kpos, format!("unknown operation property `{other}`"))),
            }
        }
        Ok(())
    }

    fn apply(&mut self) -> Res<()> {
        self.bump(); // apply
        let (target, pos) = self.ident("a shape id to apply traits to")?;
        let list = if *self.peek() == Tok::LBrace {
            self.bump();
            let list = self.traits()?;
            self.expect(Tok::RBrace, "`}` or a trait")?;
            list
        } else if *self.peek() == Tok::At {
            let t = self.trait_()?;
            TraitList { traits: vec![t], docs: Vec::new() }
        } else {
            return self.unexpected("a trait or `{`");
        };
        self.raw.applies.push(RawApply { target, pos, traits: list.traits });
        Ok(())
    }
}
