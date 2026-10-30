//! JSONPath queries over `serde_json::Value`.
//!
//! The syntax is Goessner's original one with the slice and filter rules of
//! RFC 9535:
//!
//! - `$` the root (may be left out: `store.book` means `$.store.book`)
//! - `.name`, `['name']`, `["name"]` a member; `[n]` an element (negative
//!   counts from the end); `[*]` and `.*` every child
//! - `..name`, `..*`, `..[…]` the same on the node and all its descendants
//! - unions `[0,2]`, `['a','b']` and slices `[start:end:step]`
//! - `[(@.length-1)]` Goessner's "last element" script
//! - filters `[?(@.price < 10 && @.inStock)]` with `== != < <= > >=`, `=~ /re/i`,
//!   `&& || !`, parentheses, and literals (numbers, strings, `true`, `false`,
//!   `null`). A bare query such as `[?(@.isbn)]` keeps the children where it
//!   finds something other than `null` or `false`; `$` inside a filter is
//!   the document root.
//!
//! Every match comes with its normalized path (`$.store.book[0].title`).

use std::borrow::Cow;
use std::cell::Cell;
use std::fmt;

use serde_json::Value;

/// One step of a normalized path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Segment<'a> {
    Key(Cow<'a, str>),
    Index(usize),
}

impl Segment<'_> {
    pub fn into_owned(self) -> Segment<'static> {
        match self {
            Segment::Key(k) => Segment::Key(Cow::Owned(k.into_owned())),
            Segment::Index(i) => Segment::Index(i),
        }
    }
}

/// A node found by a query.
#[derive(Clone, Debug)]
pub struct Match<'a> {
    pub value: &'a Value,
    /// Where the value is, from the root.
    pub path: Vec<Segment<'a>>,
}

impl Match<'_> {
    /// The normalized path, e.g. `$.store.book[0].title`.
    pub fn path_string(&self) -> String {
        format_path(&self.path)
    }
}

/// `$.data[0].id`; keys that are not identifiers use `['…']`.
pub fn format_path(path: &[Segment]) -> String {
    let mut out = String::from("$");
    for s in path {
        match s {
            Segment::Key(k) => push_key(&mut out, k),
            Segment::Index(i) => push_index(&mut out, *i),
        }
    }
    out
}

/// Appends a member step to a path string: `.name` or `['odd key']`.
pub fn push_key(out: &mut String, key: &str) {
    let mut chars = key.chars();
    let ident =
        chars.next().is_some_and(|c| c.is_alphabetic() || c == '_') && chars.all(|c| c.is_alphanumeric() || c == '_');
    if ident {
        out.push('.');
        out.push_str(key);
        return;
    }
    out.push_str("['");
    for c in key.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push_str("']");
}

/// Appends an element step to a path string: `[3]`.
pub fn push_index(out: &mut String, index: usize) {
    out.push('[');
    out.push_str(&index.to_string());
    out.push(']');
}

/// The value at `path` below `root`.
pub fn resolve<'v>(root: &'v Value, path: &[Segment]) -> Option<&'v Value> {
    path.iter().try_fold(root, |v, s| match (s, v) {
        (Segment::Key(k), Value::Object(m)) => m.get(k.as_ref()),
        (Segment::Index(i), Value::Array(a)) => a.get(*i),
        _ => None,
    })
}

/// Parses `path` and runs it on `root`.
pub fn query<'a>(root: &'a Value, path: &str) -> Result<Vec<Match<'a>>, ParseError> {
    Ok(JsonPath::parse(path)?.find(root))
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a path could not be parsed. `message` is a fixed English sentence
/// (the UI translates it); `offset` is the character where it went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub message: &'static str,
    pub offset: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at character {})", self.message, self.offset + 1)
    }
}

impl std::error::Error for ParseError {}

pub const ERR_EMPTY: &str = "The path is empty";
pub const ERR_START: &str = "A path starts with $";
pub const ERR_END: &str = "The path ends too early";
pub const ERR_CHAR: &str = "Unexpected character";
pub const ERR_NAME: &str = "Expected a name after the dot";
pub const ERR_BRACKET: &str = "Expected ] to close the bracket";
pub const ERR_QUOTE: &str = "The quoted name is not closed";
pub const ERR_ESCAPE: &str = "Unknown escape in a quoted name";
pub const ERR_NUMBER: &str = "Invalid number";
pub const ERR_PAREN: &str = "Expected ) to close the parenthesis";
pub const ERR_OPERAND: &str = "Expected a value or a query starting with @ or $";
pub const ERR_EQUALS: &str = "Use == to compare";
pub const ERR_SCRIPT: &str = "Only (@.length-n) is supported inside ( )";
pub const ERR_REGEX: &str = "Invalid regular expression";

/// All error messages, for translation tables.
pub const ERROR_MESSAGES: &[&str] = &[
    ERR_EMPTY,
    ERR_START,
    ERR_END,
    ERR_CHAR,
    ERR_NAME,
    ERR_BRACKET,
    ERR_QUOTE,
    ERR_ESCAPE,
    ERR_NUMBER,
    ERR_PAREN,
    ERR_OPERAND,
    ERR_EQUALS,
    ERR_SCRIPT,
    ERR_REGEX,
];

// ---------------------------------------------------------------------------
// Syntax tree
// ---------------------------------------------------------------------------

/// A parsed JSONPath, ready to run on any number of documents.
#[derive(Clone, Debug)]
pub struct JsonPath {
    steps: Vec<Step>,
}

#[derive(Clone, Debug)]
enum Step {
    Child(Vec<Selector>),
    Descendant(Vec<Selector>),
}

#[derive(Clone, Debug)]
enum Selector {
    Name(String),
    Wildcard,
    Index(i64),
    Slice(Option<i64>, Option<i64>, Option<i64>),
    /// `(@.length - n)`: the element `n` from the end.
    FromEnd(i64),
    Filter(Box<Expr>),
}

#[derive(Clone, Debug)]
struct Query {
    absolute: bool,
    steps: Vec<Step>,
}

#[derive(Clone, Debug)]
enum Operand {
    Query(Query),
    Literal(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug)]
enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Compare(Operand, Cmp, Operand),
    Matches(Operand, Regex),
    /// A query or literal on its own: true when it yields something other
    /// than `null` or `false`.
    Truthy(Operand),
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    s: Vec<char>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.s.get(self.i + n).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn eat_str(&mut self, t: &str) -> bool {
        let n = t.chars().count();
        if self.s.len() >= self.i + n && t.chars().zip(&self.s[self.i..]).all(|(a, b)| a == *b) {
            self.i += n;
            true
        } else {
            false
        }
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
    }

    fn err<T>(&self, message: &'static str) -> Result<T, ParseError> {
        Err(ParseError { message, offset: self.i.min(self.s.len()) })
    }

    fn err_here_or_end<T>(&self, message: &'static str) -> Result<T, ParseError> {
        if self.peek().is_none() { self.err(ERR_END) } else { self.err(message) }
    }

    /// Characters of a dot-notation name. Inside filters `-` ends a name so
    /// that `@.length-1` reads as a subtraction.
    fn is_name_char(c: char, in_expr: bool) -> bool {
        c.is_alphanumeric() || c == '_' || c == '$' || (c == '-' && !in_expr) || (!c.is_ascii() && !c.is_whitespace())
    }

    fn name(&mut self, in_expr: bool) -> String {
        let start = self.i;
        while self.peek().is_some_and(|c| Self::is_name_char(c, in_expr)) {
            self.i += 1;
        }
        self.s[start..self.i].iter().collect()
    }

    /// Steps after `$` or `@`, until something that is not a step.
    fn steps(&mut self, in_expr: bool) -> Result<Vec<Step>, ParseError> {
        let mut steps = Vec::new();
        loop {
            if self.eat_str("..") {
                if self.eat('*') {
                    steps.push(Step::Descendant(vec![Selector::Wildcard]));
                } else if self.peek() == Some('[') {
                    steps.push(Step::Descendant(self.bracket()?));
                } else {
                    let n = self.name(in_expr);
                    if n.is_empty() {
                        return self.err_here_or_end(ERR_NAME);
                    }
                    steps.push(Step::Descendant(vec![Selector::Name(n)]));
                }
            } else if self.eat('.') {
                if self.eat('*') {
                    steps.push(Step::Child(vec![Selector::Wildcard]));
                } else {
                    let n = self.name(in_expr);
                    if n.is_empty() {
                        return self.err_here_or_end(ERR_NAME);
                    }
                    steps.push(Step::Child(vec![Selector::Name(n)]));
                }
            } else if self.peek() == Some('[') {
                steps.push(Step::Child(self.bracket()?));
            } else {
                return Ok(steps);
            }
        }
    }

    /// `[ selector (, selector)* ]`
    fn bracket(&mut self) -> Result<Vec<Selector>, ParseError> {
        self.i += 1; // '['
        let mut sels = Vec::new();
        loop {
            self.ws();
            sels.push(self.selector()?);
            self.ws();
            if self.eat(',') {
                continue;
            }
            if self.eat(']') {
                return Ok(sels);
            }
            return self.err_here_or_end(ERR_BRACKET);
        }
    }

    fn selector(&mut self) -> Result<Selector, ParseError> {
        match self.peek() {
            None => self.err(ERR_END),
            Some('\'' | '"') => Ok(Selector::Name(self.string()?)),
            Some('*') => {
                self.i += 1;
                Ok(Selector::Wildcard)
            }
            Some('?') => {
                self.i += 1;
                self.ws();
                Ok(Selector::Filter(Box::new(self.or_expr()?)))
            }
            Some('(') => self.script(),
            Some(c) if c == '-' || c == ':' || c.is_ascii_digit() => {
                let start = if c == ':' { None } else { Some(self.int()?) };
                self.ws();
                if !self.eat(':') {
                    return Ok(Selector::Index(start.unwrap_or(0)));
                }
                self.ws();
                let end = self.opt_int()?;
                self.ws();
                let step = if self.eat(':') {
                    self.ws();
                    self.opt_int()?
                } else {
                    None
                };
                Ok(Selector::Slice(start, end, step))
            }
            Some(_) => self.err(ERR_CHAR),
        }
    }

    /// `(@.length)` or `(@.length - n)`.
    fn script(&mut self) -> Result<Selector, ParseError> {
        let start = self.i;
        self.i += 1;
        self.ws();
        if !self.eat_str("@.length") {
            self.i = start;
            return self.err(ERR_SCRIPT);
        }
        self.ws();
        let offset = if self.eat('-') {
            self.ws();
            -self.int()?
        } else if self.eat('+') {
            self.ws();
            self.int()?
        } else {
            0
        };
        self.ws();
        if !self.eat(')') {
            return self.err_here_or_end(ERR_PAREN);
        }
        Ok(Selector::FromEnd(offset))
    }

    fn opt_int(&mut self) -> Result<Option<i64>, ParseError> {
        if self.peek().is_some_and(|c| c == '-' || c.is_ascii_digit()) { Ok(Some(self.int()?)) } else { Ok(None) }
    }

    fn int(&mut self) -> Result<i64, ParseError> {
        let start = self.i;
        self.eat('-');
        let digits = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == digits {
            return self.err_here_or_end(ERR_NUMBER);
        }
        let text: String = self.s[start..self.i].iter().collect();
        text.parse().or_else(|_| {
            self.i = start;
            self.err(ERR_NUMBER)
        })
    }

    /// A quoted string with JSON-style escapes (`\'` too).
    fn string(&mut self) -> Result<String, ParseError> {
        let quote = self.s[self.i];
        let open = self.i;
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                self.i = open;
                return self.err(ERR_QUOTE);
            };
            self.i += 1;
            if c == quote {
                return Ok(out);
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            let Some(e) = self.peek() else {
                self.i = open;
                return self.err(ERR_QUOTE);
            };
            self.i += 1;
            match e {
                '\'' | '"' | '\\' | '/' => out.push(e),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = self.s.iter().skip(self.i).take(4).collect();
                    match u32::from_str_radix(&hex, 16).ok().filter(|_| hex.len() == 4).and_then(char::from_u32) {
                        Some(ch) => {
                            out.push(ch);
                            self.i += 4;
                        }
                        None => {
                            self.i -= 2;
                            return self.err(ERR_ESCAPE);
                        }
                    }
                }
                _ => {
                    self.i -= 2;
                    return self.err(ERR_ESCAPE);
                }
            }
        }
    }

    fn or_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.and_expr()?;
        loop {
            self.ws();
            if !self.eat_str("||") {
                return Ok(left);
            }
            self.ws();
            left = Expr::Or(Box::new(left), Box::new(self.and_expr()?));
        }
    }

    fn and_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.not_expr()?;
        loop {
            self.ws();
            if !self.eat_str("&&") {
                return Ok(left);
            }
            self.ws();
            left = Expr::And(Box::new(left), Box::new(self.not_expr()?));
        }
    }

    fn not_expr(&mut self) -> Result<Expr, ParseError> {
        self.ws();
        if self.peek() == Some('!') && self.peek_at(1) != Some('=') {
            self.i += 1;
            return Ok(Expr::Not(Box::new(self.not_expr()?)));
        }
        if self.eat('(') {
            self.ws();
            let e = self.or_expr()?;
            self.ws();
            if !self.eat(')') {
                return self.err_here_or_end(ERR_PAREN);
            }
            return Ok(e);
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr, ParseError> {
        let left = self.operand()?;
        self.ws();
        if self.eat_str("=~") {
            self.ws();
            let re = match self.peek() {
                Some('/') => self.regex_literal()?,
                Some('\'' | '"') => {
                    let at = self.i;
                    let pattern = self.string()?;
                    Regex::new(&pattern, false).ok_or(ParseError { message: ERR_REGEX, offset: at })?
                }
                _ => return self.err_here_or_end(ERR_REGEX),
            };
            return Ok(Expr::Matches(left, re));
        }
        let op = if self.eat_str("==") {
            Cmp::Eq
        } else if self.eat_str("!=") {
            Cmp::Ne
        } else if self.eat_str("<=") {
            Cmp::Le
        } else if self.eat_str(">=") {
            Cmp::Ge
        } else if self.eat('<') {
            Cmp::Lt
        } else if self.eat('>') {
            Cmp::Gt
        } else if self.peek() == Some('=') {
            return self.err(ERR_EQUALS);
        } else {
            return Ok(Expr::Truthy(left));
        };
        self.ws();
        let right = self.operand()?;
        Ok(Expr::Compare(left, op, right))
    }

    fn operand(&mut self) -> Result<Operand, ParseError> {
        match self.peek() {
            Some('@') => {
                self.i += 1;
                Ok(Operand::Query(Query { absolute: false, steps: self.steps(true)? }))
            }
            Some('$') => {
                self.i += 1;
                Ok(Operand::Query(Query { absolute: true, steps: self.steps(true)? }))
            }
            Some('\'' | '"') => Ok(Operand::Literal(Value::String(self.string()?))),
            Some(c) if c == '-' || c.is_ascii_digit() => Ok(Operand::Literal(self.number()?)),
            _ => {
                for (word, v) in [("true", Value::Bool(true)), ("false", Value::Bool(false)), ("null", Value::Null)] {
                    if self.eat_str(word) {
                        return Ok(Operand::Literal(v));
                    }
                }
                if self.peek().is_none() { self.err(ERR_END) } else { self.err(ERR_OPERAND) }
            }
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        self.eat('-');
        while self.peek().is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
            // A sign only right after an exponent.
            if matches!(self.peek(), Some('+' | '-')) && !matches!(self.s[self.i - 1], 'e' | 'E') {
                break;
            }
            self.i += 1;
        }
        let text: String = self.s[start..self.i].iter().collect();
        match serde_json::from_str::<serde_json::Number>(&text) {
            Ok(n) => Ok(Value::Number(n)),
            Err(_) => {
                self.i = start;
                self.err(ERR_NUMBER)
            }
        }
    }

    /// `/pattern/flags`
    fn regex_literal(&mut self) -> Result<Regex, ParseError> {
        let open = self.i;
        self.i += 1;
        let mut pattern = String::new();
        loop {
            match self.peek() {
                None => {
                    self.i = open;
                    return self.err(ERR_REGEX);
                }
                Some('/') => {
                    self.i += 1;
                    break;
                }
                Some('\\') if self.peek_at(1) == Some('/') => {
                    pattern.push('/');
                    self.i += 2;
                }
                Some('\\') if self.peek_at(1).is_some() => {
                    pattern.push('\\');
                    pattern.push(self.s[self.i + 1]);
                    self.i += 2;
                }
                Some(c) => {
                    pattern.push(c);
                    self.i += 1;
                }
            }
        }
        let mut icase = false;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            icase |= self.peek() == Some('i');
            self.i += 1;
        }
        Regex::new(&pattern, icase).ok_or(ParseError { message: ERR_REGEX, offset: open })
    }
}

impl Parser {
    fn finish(&self, steps: Vec<Step>) -> Result<JsonPath, ParseError> {
        if self.i < self.s.len() {
            return self.err(ERR_CHAR);
        }
        Ok(JsonPath { steps })
    }
}

impl JsonPath {
    pub fn parse(path: &str) -> Result<JsonPath, ParseError> {
        let chars: Vec<char> = path.chars().collect();
        let lead = chars.iter().take_while(|c| c.is_whitespace()).count();
        let trail = chars.iter().rev().take_while(|c| c.is_whitespace()).count();
        if lead == chars.len() {
            return Err(ParseError { message: ERR_EMPTY, offset: 0 });
        }
        let mut p = Parser { s: chars[..chars.len() - trail].to_vec(), i: lead };
        match p.peek() {
            Some('$') => p.i += 1,
            Some('.' | '[') => {}
            // `store.book`: a leading name means `$.store`.
            Some(c) if Parser::is_name_char(c, false) && c != '$' => {
                let n = p.name(false);
                let mut steps = vec![Step::Child(vec![Selector::Name(n)])];
                steps.extend(p.steps(false)?);
                return p.finish(steps);
            }
            _ => return p.err(ERR_START),
        }
        let steps = p.steps(false)?;
        p.finish(steps)
    }

    /// Every node the path selects, in document order.
    pub fn find<'a>(&self, root: &'a Value) -> Vec<Match<'a>> {
        let mut arena: Vec<(u32, Segment<'a>)> = Vec::new();
        let nodes = run(&self.steps, root, root, Some(&mut arena));
        nodes
            .into_iter()
            .map(|(trail, value)| {
                let mut path = Vec::new();
                let mut t = trail;
                while t != ROOT {
                    let (parent, seg) = &arena[t as usize];
                    path.push(seg.clone());
                    t = *parent;
                }
                path.reverse();
                Match { value, path }
            })
            .collect()
    }
}

impl std::str::FromStr for JsonPath {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        JsonPath::parse(s)
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

const ROOT: u32 = u32::MAX;

type Arena<'a> = Vec<(u32, Segment<'a>)>;

/// Runs `steps` from `start`. With an arena, each node carries an index
/// into it from which its path is rebuilt.
fn run<'a>(
    steps: &[Step],
    start: &'a Value,
    root: &'a Value,
    mut arena: Option<&mut Arena<'a>>,
) -> Vec<(u32, &'a Value)> {
    let mut nodes = vec![(ROOT, start)];
    for step in steps {
        let mut next = Vec::new();
        for &(trail, v) in &nodes {
            match step {
                Step::Child(sels) => {
                    for sel in sels {
                        select(sel, v, root, &mut |seg, child| {
                            next.push((link(&mut arena, trail, seg), child));
                        });
                    }
                }
                Step::Descendant(sels) => descend(sels, trail, v, root, &mut arena, &mut next),
            }
        }
        nodes = next;
        if nodes.is_empty() {
            break;
        }
    }
    nodes
}

fn link<'a>(arena: &mut Option<&mut Arena<'a>>, parent: u32, seg: Segment<'a>) -> u32 {
    match arena {
        Some(a) => {
            a.push((parent, seg));
            (a.len() - 1) as u32
        }
        None => ROOT,
    }
}

/// Applies `sels` to `v` and then to each descendant, in document order.
fn descend<'a>(
    sels: &[Selector],
    trail: u32,
    v: &'a Value,
    root: &'a Value,
    arena: &mut Option<&mut Arena<'a>>,
    out: &mut Vec<(u32, &'a Value)>,
) {
    for sel in sels {
        select(sel, v, root, &mut |seg, child| {
            out.push((link(arena, trail, seg), child));
        });
    }
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let t = link(arena, trail, Segment::Key(Cow::Borrowed(k.as_str())));
                descend(sels, t, child, root, arena, out);
            }
        }
        Value::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                let t = link(arena, trail, Segment::Index(i));
                descend(sels, t, child, root, arena, out);
            }
        }
        _ => {}
    }
}

fn children<'a>(v: &'a Value) -> Box<dyn Iterator<Item = (Segment<'a>, &'a Value)> + 'a> {
    match v {
        Value::Object(m) => Box::new(m.iter().map(|(k, c)| (Segment::Key(Cow::Borrowed(k.as_str())), c))),
        Value::Array(a) => Box::new(a.iter().enumerate().map(|(i, c)| (Segment::Index(i), c))),
        _ => Box::new(std::iter::empty()),
    }
}

fn select<'a>(sel: &Selector, v: &'a Value, root: &'a Value, emit: &mut dyn FnMut(Segment<'a>, &'a Value)) {
    match sel {
        Selector::Name(n) => {
            if let Value::Object(m) = v
                && let Some((k, child)) = m.get_key_value(n.as_str())
            {
                emit(Segment::Key(Cow::Borrowed(k.as_str())), child);
            }
        }
        Selector::Wildcard => {
            for (seg, child) in children(v) {
                emit(seg, child);
            }
        }
        Selector::Index(i) => {
            if let Value::Array(a) = v
                && let Some(idx) = normalize(*i, a.len())
            {
                emit(Segment::Index(idx), &a[idx]);
            }
        }
        Selector::FromEnd(offset) => {
            if let Value::Array(a) = v {
                let idx = a.len() as i64 + offset;
                if (0..a.len() as i64).contains(&idx) {
                    emit(Segment::Index(idx as usize), &a[idx as usize]);
                }
            }
        }
        Selector::Slice(start, end, step) => {
            if let Value::Array(a) = v {
                for idx in slice_indices(a.len(), *start, *end, *step) {
                    emit(Segment::Index(idx), &a[idx]);
                }
            }
        }
        Selector::Filter(expr) => {
            for (seg, child) in children(v) {
                if eval(expr, child, root) {
                    emit(seg, child);
                }
            }
        }
    }
}

fn normalize(i: i64, len: usize) -> Option<usize> {
    let idx = if i < 0 { len as i64 + i } else { i };
    (0..len as i64).contains(&idx).then_some(idx as usize)
}

/// Indices of `[start:end:step]` (RFC 9535 §2.3.4.2).
fn slice_indices(len: usize, start: Option<i64>, end: Option<i64>, step: Option<i64>) -> Vec<usize> {
    let len = len as i64;
    let step = step.unwrap_or(1);
    if step == 0 {
        return Vec::new();
    }
    let norm = |i: i64| if i >= 0 { i } else { len + i };
    let mut out = Vec::new();
    if step > 0 {
        let lower = norm(start.unwrap_or(0)).clamp(0, len);
        let upper = norm(end.unwrap_or(len)).clamp(0, len);
        let mut i = lower;
        while i < upper {
            out.push(i as usize);
            i += step;
        }
    } else {
        let upper = norm(start.unwrap_or(len - 1)).clamp(-1, len - 1);
        let lower = end.map(norm).unwrap_or(-len - 1).clamp(-1, len - 1);
        let mut i = upper;
        while lower < i {
            out.push(i as usize);
            i += step;
        }
    }
    out
}

fn operand_values<'x>(op: &'x Operand, cur: &'x Value, root: &'x Value) -> Vec<&'x Value> {
    match op {
        Operand::Literal(v) => vec![v],
        Operand::Query(q) => {
            let start = if q.absolute { root } else { cur };
            run(&q.steps, start, root, None).into_iter().map(|(_, v)| v).collect()
        }
    }
}

fn eval(expr: &Expr, cur: &Value, root: &Value) -> bool {
    match expr {
        Expr::Or(a, b) => eval(a, cur, root) || eval(b, cur, root),
        Expr::And(a, b) => eval(a, cur, root) && eval(b, cur, root),
        Expr::Not(e) => !eval(e, cur, root),
        Expr::Truthy(op) => {
            operand_values(op, cur, root).iter().any(|v| !matches!(v, Value::Null | Value::Bool(false)))
        }
        Expr::Matches(op, re) => {
            operand_values(op, cur, root).iter().any(|v| matches!(v, Value::String(s) if re.is_match(s)))
        }
        Expr::Compare(l, op, r) => {
            let lv = operand_values(l, cur, root);
            let rv = operand_values(r, cur, root);
            let equal = || (lv.is_empty() && rv.is_empty()) || lv.iter().any(|a| rv.iter().any(|b| json_eq(a, b)));
            match op {
                Cmp::Eq => equal(),
                Cmp::Ne => !equal(),
                _ => lv.iter().any(|a| rv.iter().any(|b| ordered(a, b, *op))),
            }
        }
    }
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(i), Some(j)) => i == j,
            _ => x.as_f64() == y.as_f64(),
        },
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

fn ordered(a: &Value, b: &Value, op: Cmp) -> bool {
    let ord = match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x.partial_cmp(&y),
            _ => None,
        },
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    };
    let Some(ord) = ord else { return false };
    match op {
        Cmp::Lt => ord.is_lt(),
        Cmp::Le => ord.is_le(),
        Cmp::Gt => ord.is_gt(),
        Cmp::Ge => ord.is_ge(),
        Cmp::Eq | Cmp::Ne => false,
    }
}

// ---------------------------------------------------------------------------
// A small regular expression engine for `=~`
// ---------------------------------------------------------------------------

/// Backtracking regular expressions: literals, `.`, classes (`[a-z]`,
/// `[^0-9]`, `\d \w \s` and their negations), groups with `|`, `(?:…)`,
/// quantifiers `* + ? {n} {n,} {n,m}` (greedy or lazy with `?`), anchors
/// `^ $`, and the `i` flag. Unanchored: it finds a match anywhere.
#[derive(Clone, Debug)]
pub struct Regex {
    node: Re,
    icase: bool,
}

#[derive(Clone, Debug)]
enum Re {
    Char(char),
    Any,
    Class(Vec<ClassItem>, bool),
    Start,
    End,
    Concat(Vec<Re>),
    Alt(Vec<Re>),
    Repeat(Box<Re>, u32, u32, bool),
}

#[derive(Clone, Debug)]
enum ClassItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

/// Steps a single match may take before it gives up (catastrophic patterns).
const REGEX_BUDGET: u32 = 1_000_000;

impl Regex {
    pub fn new(pattern: &str, icase: bool) -> Option<Regex> {
        let mut p = ReParser { s: pattern.chars().collect(), i: 0 };
        let node = p.alt()?;
        if p.i != p.s.len() {
            return None;
        }
        Some(Regex { node, icase })
    }

    pub fn is_match(&self, text: &str) -> bool {
        let s: Vec<char> = text.chars().collect();
        let budget = Cell::new(REGEX_BUDGET);
        let m = Matcher { s: &s, icase: self.icase, budget: &budget };
        (0..=s.len()).any(|start| budget.get() > 0 && m.at(&self.node, start, &mut |_| true))
    }
}

struct ReParser {
    s: Vec<char>,
    i: usize,
}

impl ReParser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn alt(&mut self) -> Option<Re> {
        let mut branches = vec![self.concat()?];
        while self.peek() == Some('|') {
            self.i += 1;
            branches.push(self.concat()?);
        }
        Some(if branches.len() == 1 { branches.pop()? } else { Re::Alt(branches) })
    }

    fn concat(&mut self) -> Option<Re> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            items.push(self.quantified(atom)?);
        }
        Some(Re::Concat(items))
    }

    fn quantified(&mut self, atom: Re) -> Option<Re> {
        let (min, max) = match self.peek() {
            Some('*') => (0, u32::MAX),
            Some('+') => (1, u32::MAX),
            Some('?') => (0, 1),
            Some('{') => {
                let close = self.s[self.i..].iter().position(|&c| c == '}')? + self.i;
                let body: String = self.s[self.i + 1..close].iter().collect();
                let (a, b) = match body.split_once(',') {
                    Some((a, "")) => (a.trim().parse().ok()?, u32::MAX),
                    Some((a, b)) => (a.trim().parse().ok()?, b.trim().parse().ok()?),
                    None => {
                        let n = body.trim().parse().ok()?;
                        (n, n)
                    }
                };
                if a > b {
                    return None;
                }
                self.i = close;
                (a, b)
            }
            _ => return Some(atom),
        };
        if matches!(atom, Re::Start | Re::End) {
            return None;
        }
        self.i += 1;
        let greedy = if self.peek() == Some('?') {
            self.i += 1;
            false
        } else {
            true
        };
        Some(Re::Repeat(Box::new(atom), min, max, greedy))
    }

    fn atom(&mut self) -> Option<Re> {
        let c = self.peek()?;
        self.i += 1;
        Some(match c {
            '.' => Re::Any,
            '^' => Re::Start,
            '$' => Re::End,
            '(' => {
                if self.s[self.i..].starts_with(&['?', ':']) {
                    self.i += 2;
                }
                let inner = self.alt()?;
                if self.peek() != Some(')') {
                    return None;
                }
                self.i += 1;
                inner
            }
            '[' => self.class()?,
            '\\' => match self.escape()? {
                Ok(ch) => Re::Char(ch),
                Err(item) => Re::Class(vec![item], false),
            },
            '*' | '+' | '?' | '{' | ')' => return None,
            c => Re::Char(c),
        })
    }

    /// After a backslash: a literal char or a class shorthand.
    fn escape(&mut self) -> Option<Result<char, ClassItem>> {
        let c = self.peek()?;
        self.i += 1;
        Some(match c {
            'd' => Err(ClassItem::Digit(true)),
            'D' => Err(ClassItem::Digit(false)),
            'w' => Err(ClassItem::Word(true)),
            'W' => Err(ClassItem::Word(false)),
            's' => Err(ClassItem::Space(true)),
            'S' => Err(ClassItem::Space(false)),
            'n' => Ok('\n'),
            't' => Ok('\t'),
            'r' => Ok('\r'),
            c if c.is_ascii_alphanumeric() => return None,
            c => Ok(c),
        })
    }

    fn class(&mut self) -> Option<Re> {
        let negated = if self.peek() == Some('^') {
            self.i += 1;
            true
        } else {
            false
        };
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = self.peek()?;
            if c == ']' && !first {
                self.i += 1;
                break;
            }
            first = false;
            self.i += 1;
            let lo = if c == '\\' {
                match self.escape()? {
                    Ok(ch) => ch,
                    Err(item) => {
                        items.push(item);
                        continue;
                    }
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.s.get(self.i + 1).is_some_and(|&n| n != ']') {
                self.i += 1;
                let mut hi = self.peek()?;
                self.i += 1;
                if hi == '\\' {
                    hi = self.escape()?.ok()?;
                }
                if hi < lo {
                    return None;
                }
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Range(lo, lo));
            }
        }
        Some(Re::Class(items, negated))
    }
}

struct Matcher<'s> {
    s: &'s [char],
    icase: bool,
    budget: &'s Cell<u32>,
}

impl Matcher<'_> {
    fn same(&self, a: char, b: char) -> bool {
        a == b || self.icase && a.to_lowercase().eq(b.to_lowercase())
    }

    fn in_class(&self, items: &[ClassItem], c: char) -> bool {
        items.iter().any(|item| match *item {
            ClassItem::Range(lo, hi) => {
                (lo..=hi).contains(&c)
                    || self.icase && c.to_lowercase().chain(c.to_uppercase()).any(|x| (lo..=hi).contains(&x))
            }
            ClassItem::Digit(yes) => c.is_ascii_digit() == yes,
            ClassItem::Word(yes) => (c.is_alphanumeric() || c == '_') == yes,
            ClassItem::Space(yes) => c.is_whitespace() == yes,
        })
    }

    /// Matches `re` at `i`, then hands the end position to `k`.
    fn at(&self, re: &Re, i: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
        let left = self.budget.get();
        if left == 0 {
            return false;
        }
        self.budget.set(left - 1);
        let s = self.s;
        match re {
            Re::Char(c) => i < s.len() && self.same(s[i], *c) && k(i + 1),
            Re::Any => i < s.len() && s[i] != '\n' && k(i + 1),
            Re::Class(items, negated) => i < s.len() && self.in_class(items, s[i]) != *negated && k(i + 1),
            Re::Start => i == 0 && k(i),
            Re::End => i == s.len() && k(i),
            Re::Alt(branches) => branches.iter().any(|b| self.at(b, i, k)),
            Re::Concat(items) => self.seq(items, i, k),
            Re::Repeat(inner, min, max, greedy) => self.repeat(inner, *min, *max, *greedy, 0, i, k),
        }
    }

    fn seq(&self, items: &[Re], i: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
        match items.split_first() {
            None => k(i),
            Some((first, rest)) => self.at(first, i, &mut |j| self.seq(rest, j, k)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &self,
        inner: &Re,
        min: u32,
        max: u32,
        greedy: bool,
        count: u32,
        i: usize,
        k: &mut dyn FnMut(usize) -> bool,
    ) -> bool {
        let more = |k: &mut dyn FnMut(usize) -> bool| {
            count < max
                && self.at(inner, i, &mut |j| {
                    // An empty iteration would loop forever.
                    j != i && self.repeat(inner, min, max, greedy, count + 1, j, k)
                })
        };
        if count < min {
            return more(k);
        }
        // Lazy tries to stop first, greedy to repeat first.
        if !greedy && k(i) {
            return true;
        }
        more(k) || (greedy && k(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bookstore() -> Value {
        json!({
            "store": {
                "book": [
                    {"category": "reference", "author": "Nigel Rees", "title": "Sayings of the Century", "price": 8.95},
                    {"category": "fiction", "author": "Evelyn Waugh", "title": "Sword of Honour", "price": 12.99},
                    {"category": "fiction", "author": "Herman Melville", "title": "Moby Dick",
                     "isbn": "0-553-21311-3", "price": 8.99},
                    {"category": "fiction", "author": "J. R. R. Tolkien", "title": "The Lord of the Rings",
                     "isbn": "0-395-19395-8", "price": 22.99}
                ],
                "bicycle": {"color": "red", "price": 19.95}
            },
            "expensive": 10
        })
    }

    fn values(doc: &Value, path: &str) -> Vec<Value> {
        query(doc, path).unwrap_or_else(|e| panic!("{path}: {e}")).into_iter().map(|m| m.value.clone()).collect()
    }

    fn paths(doc: &Value, path: &str) -> Vec<String> {
        query(doc, path).unwrap_or_else(|e| panic!("{path}: {e}")).iter().map(Match::path_string).collect()
    }

    fn titles(doc: &Value, path: &str) -> Vec<String> {
        values(doc, path).iter().map(|b| b["title"].as_str().unwrap_or("?").to_string()).collect()
    }

    #[test]
    fn goessner_examples() {
        let doc = bookstore();
        let authors =
            vec![json!("Nigel Rees"), json!("Evelyn Waugh"), json!("Herman Melville"), json!("J. R. R. Tolkien")];
        // The authors of all books in the store.
        assert_eq!(values(&doc, "$.store.book[*].author"), authors);
        // All authors.
        assert_eq!(values(&doc, "$..author"), authors);
        assert_eq!(
            paths(&doc, "$..author"),
            ["$.store.book[0].author", "$.store.book[1].author", "$.store.book[2].author", "$.store.book[3].author"]
        );
        // All things in the store: the books and the bicycle.
        assert_eq!(paths(&doc, "$.store.*"), ["$.store.book", "$.store.bicycle"]);
        // The price of everything in the store.
        assert_eq!(
            values(&doc, "$.store..price"),
            [json!(8.95), json!(12.99), json!(8.99), json!(22.99), json!(19.95)]
        );
        // The third book.
        assert_eq!(titles(&doc, "$..book[2]"), ["Moby Dick"]);
        assert_eq!(paths(&doc, "$..book[2]"), ["$.store.book[2]"]);
        // The last book, three ways.
        for p in ["$..book[(@.length-1)]", "$..book[-1:]", "$..book[-1]"] {
            assert_eq!(titles(&doc, p), ["The Lord of the Rings"], "{p}");
        }
        // The first two books.
        for p in ["$..book[0,1]", "$..book[:2]"] {
            assert_eq!(titles(&doc, p), ["Sayings of the Century", "Sword of Honour"], "{p}");
        }
        // Books with an ISBN.
        assert_eq!(titles(&doc, "$..book[?(@.isbn)]"), ["Moby Dick", "The Lord of the Rings"]);
        // Books cheaper than 10.
        assert_eq!(titles(&doc, "$..book[?(@.price<10)]"), ["Sayings of the Century", "Moby Dick"]);
        // Books not more expensive than the root's `expensive`.
        assert_eq!(titles(&doc, "$..book[?(@.price <= $['expensive'])]"), ["Sayings of the Century", "Moby Dick"]);
        // Every member: store, book, 4 books with 18 fields, bicycle with 2, expensive.
        assert_eq!(values(&doc, "$..*").len(), 28);
        assert_eq!(values(&doc, "$..*")[0], doc["store"]);
    }

    #[test]
    fn names_indexes_and_unions() {
        let doc = bookstore();
        assert_eq!(values(&doc, "$['store']['bicycle']['color']"), [json!("red")]);
        assert_eq!(values(&doc, r#"$["store"]["bicycle"].color"#), [json!("red")]);
        assert_eq!(values(&doc, "$.store.bicycle['color','price']"), [json!("red"), json!(19.95)]);
        assert_eq!(values(&doc, "$.store.bicycle[ 'price' , 'color' ]"), [json!(19.95), json!("red")]);
        assert_eq!(titles(&doc, "$.store.book[-2]"), ["Moby Dick"]);
        assert_eq!(titles(&doc, "$.store.book[0,-1]"), ["Sayings of the Century", "The Lord of the Rings"]);
        assert!(values(&doc, "$.store.book[4]").is_empty());
        assert!(values(&doc, "$.store.book[-5]").is_empty());
        assert!(values(&doc, "$.store.missing").is_empty());
        // A name on an array or an index on an object selects nothing.
        assert!(values(&doc, "$.store.book.title").is_empty());
        assert!(values(&doc, "$.store[0]").is_empty());
        assert_eq!(values(&doc, "$"), std::slice::from_ref(&doc));
        assert_eq!(paths(&doc, "$"), ["$"]);
        assert_eq!(values(&doc, "$.store.bicycle.*"), [json!("red"), json!(19.95)]);
        assert_eq!(values(&doc, "$.store.bicycle[*]"), [json!("red"), json!(19.95)]);
    }

    #[test]
    fn lenient_start() {
        let doc = bookstore();
        assert_eq!(values(&doc, "store.bicycle.color"), [json!("red")]);
        assert_eq!(values(&doc, ".store.bicycle.color"), [json!("red")]);
        assert_eq!(values(&doc, "['expensive']"), [json!(10)]);
        assert_eq!(values(&doc, "  $.expensive  "), [json!(10)]);
    }

    #[test]
    fn slices() {
        let doc = json!([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let ints = |p: &str| values(&doc, p).iter().map(|v| v.as_i64().unwrap()).collect::<Vec<_>>();
        assert_eq!(ints("$[1:3]"), [1, 2]);
        assert_eq!(ints("$[:3]"), [0, 1, 2]);
        assert_eq!(ints("$[7:]"), [7, 8, 9]);
        assert_eq!(ints("$[-2:]"), [8, 9]);
        assert_eq!(ints("$[::3]"), [0, 3, 6, 9]);
        assert_eq!(ints("$[1:8:2]"), [1, 3, 5, 7]);
        assert_eq!(ints("$[::-1]"), [9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);
        assert_eq!(ints("$[5:1:-2]"), [5, 3]);
        assert_eq!(ints("$[-1:-4:-1]"), [9, 8, 7]);
        assert!(ints("$[::0]").is_empty());
        assert!(ints("$[20:]").is_empty());
        assert!(ints("$[3:1]").is_empty());
        assert_eq!(ints("$[-100:2]"), [0, 1]);
        assert_eq!(ints("$[0, 2:4, -1]"), [0, 2, 3, 9]);
        assert_eq!(paths(&doc, "$[8:]"), ["$[8]", "$[9]"]);
    }

    #[test]
    fn recursive_descent() {
        let doc = json!({"a": {"id": 1, "b": [{"id": 2}, {"c": {"id": 3}}]}, "id": 0});
        assert_eq!(values(&doc, "$..id"), [json!(0), json!(1), json!(2), json!(3)]);
        assert_eq!(paths(&doc, "$..id"), ["$.id", "$.a.id", "$.a.b[0].id", "$.a.b[1].c.id"]);
        assert_eq!(paths(&doc, "$..[0]"), ["$.a.b[0]"]);
        assert_eq!(paths(&doc, "$.a..['id']"), ["$.a.id", "$.a.b[0].id", "$.a.b[1].c.id"]);
        assert_eq!(values(&doc, "$..*").len(), 9);
        assert!(values(&json!(5), "$..*").is_empty());
    }

    #[test]
    fn filters() {
        let doc = json!({"items": [
            {"name": "apple", "price": 3, "inStock": true, "tags": ["fruit", "red"]},
            {"name": "Banana", "price": 12.5, "inStock": false, "tags": ["fruit"]},
            {"name": "carrot", "price": 1, "inStock": true, "note": null},
            {"name": "dates", "price": "n/a"},
            {"name": "eggplant", "price": 7.0, "inStock": true, "kind": {"veg": true}}
        ]});
        let names =
            |p: &str| values(&doc, p).iter().map(|v| v["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names("$.items[?(@.price < 10 && @.inStock)]"), ["apple", "carrot", "eggplant"]);
        assert_eq!(names("$.items[?(@.price == 7)]"), ["eggplant"]);
        assert_eq!(names("$.items[?(@.price != 3)]"), ["Banana", "carrot", "dates", "eggplant"]);
        assert_eq!(names("$.items[?(@.price >= 7)]"), ["Banana", "eggplant"]);
        assert_eq!(names("$.items[?(@.price > 3 || @.name == 'carrot')]"), ["Banana", "carrot", "eggplant"]);
        assert_eq!(names("$.items[?(@.name == \"dates\")]"), ["dates"]);
        assert_eq!(names("$.items[?(@.inStock == false)]"), ["Banana"]);
        assert_eq!(names("$.items[?(@.note == null)]"), ["carrot"]);
        assert_eq!(names("$.items[?(!@.inStock)]"), ["Banana", "dates"]);
        assert_eq!(names("$.items[?(!(@.price < 5))]"), ["Banana", "dates", "eggplant"]);
        assert_eq!(names("$.items[?(@.kind.veg)]"), ["eggplant"]);
        assert_eq!(names("$.items[?(@.tags[*] == 'red')]"), ["apple"]);
        assert_eq!(names("$.items[?(@.name > 'b')]"), ["carrot", "dates", "eggplant"]);
        assert_eq!(names("$.items[?@.price < 2]"), ["carrot"]);
        assert_eq!(names("$.items[?(@.price<2)]"), ["carrot"]);
        assert_eq!(names("$.items[?(@.price == -1 || @.price == 1e0)]"), ["carrot"]);
        // Both sides missing are equal; one side missing is not.
        assert_eq!(names("$.items[?(@.missing == @.other)]").len(), 5);
        assert!(names("$.items[?(@.missing == 1)]").is_empty());
        // Filters apply to object members too.
        assert_eq!(values(&json!({"a": {"x": 1}, "b": {"x": 2}}), "$[?(@.x > 1)]"), [json!({"x": 2})]);
        assert_eq!(paths(&doc, "$.items[?(@.name == 'dates')].name"), ["$.items[3].name"]);
        assert_eq!(values(&json!([1, 2, 3, 4]), "$[?(@ > 2)]"), [json!(3), json!(4)]);
    }

    #[test]
    fn regex_filters() {
        let doc = json!([{"n": "Apple pie"}, {"n": "banana"}, {"n": "cherry/apple"}, {"n": 4}]);
        let ns = |p: &str| values(&doc, p).iter().map(|v| v["n"].to_string()).collect::<Vec<_>>();
        assert_eq!(ns("$[?(@.n =~ /^a/i)]"), ["\"Apple pie\""]);
        assert_eq!(ns("$[?(@.n =~ /apple/)]"), ["\"cherry/apple\""]);
        assert_eq!(ns("$[?(@.n =~ /y\\/a/)]"), ["\"cherry/apple\""]);
        assert_eq!(ns("$[?(@.n =~ 'an+a')]"), ["\"banana\""]);
        assert_eq!(ns("$[?(@.n =~ /^[a-c][a-z]+$/)]"), ["\"banana\""]);
        assert!(JsonPath::parse("$[?(@.n =~ /(/)]").is_err());

        let re = |p: &str, s: &str| Regex::new(p, false).unwrap().is_match(s);
        assert!(re("a.c", "xabcx"));
        assert!(!re("^a.c$", "xabc"));
        assert!(re("^\\d{3}-\\d{4}$", "555-1234"));
        assert!(!re("^\\d{3}-\\d{4}$", "55-1234"));
        assert!(re("^(cat|dog)s?$", "dogs"));
        assert!(!re("^(cat|dog)s?$", "cow"));
        assert!(re("^[^0-9]+$", "abc"));
        assert!(!re("^[^0-9]+$", "a1c"));
        assert!(re("^a{2,3}$", "aaa"));
        assert!(!re("^a{2,3}$", "aaaa"));
        assert!(re("^a{2,}$", "aaaaa"));
        assert!(re("^\\w+@\\w+\\.com$", "me@mail.com"));
        assert!(re("^(?:ab)*$", "ababab"));
        assert!(re("^a.*?b$", "axxb"));
        assert!(re("^(a*)*$", "aaaa"));
        assert!(re("é", "café"));
        assert!(Regex::new("[A-Z]", true).unwrap().is_match("abc"));
        // A catastrophic pattern gives up instead of hanging.
        let start = std::time::Instant::now();
        assert!(!re("^(a+)+$", &format!("{}b", "a".repeat(40))));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        for bad in ["(", "[a", "a{3,1}", "*a", "a)", "\\q", "^*"] {
            assert!(Regex::new(bad, false).is_none(), "{bad}");
        }
    }

    #[test]
    fn errors_point_at_the_problem() {
        let err = |p: &str| JsonPath::parse(p).unwrap_err();
        assert_eq!(err("").message, ERR_EMPTY);
        assert_eq!(err("$.").message, ERR_END);
        assert_eq!(err("$.store[").message, ERR_END);
        assert_eq!(err("$.store[0").message, ERR_END);
        assert_eq!(err("$.a[0 1]"), ParseError { message: ERR_BRACKET, offset: 6 });
        assert_eq!(err("$['a").message, ERR_QUOTE);
        assert_eq!(err("$['a").offset, 2);
        assert_eq!(err("$['a\\q']").message, ERR_ESCAPE);
        assert_eq!(err("$.a b").message, ERR_CHAR);
        assert_eq!(err("$[?(@.a = 1)]").message, ERR_EQUALS);
        assert_eq!(err("$[?(@.a == )]").message, ERR_OPERAND);
        assert_eq!(err("$[?(@.a == 1]").message, ERR_PAREN);
        assert_eq!(err("$[?()]").message, ERR_OPERAND);
        assert_eq!(err("$[(1+1)]").message, ERR_SCRIPT);
        assert_eq!(err("$[x]").message, ERR_CHAR);
        assert_eq!(err("#a").message, ERR_START);
        assert_eq!(err("$[--1]").message, ERR_NUMBER);
        assert_eq!(err("$..").message, ERR_END);
        assert!(err("$.a[").to_string().contains("character 5"));
    }

    #[test]
    fn trailing_garbage_is_an_error() {
        assert_eq!(JsonPath::parse("$.a)").unwrap_err(), ParseError { message: ERR_CHAR, offset: 3 });
        assert_eq!(JsonPath::parse("$.a ]").unwrap_err().message, ERR_CHAR);
    }

    #[test]
    fn normalized_paths_round_trip() {
        let doc = json!({"a b": {"it's": [{"x\\y": 1}]}, "ok_1": {"név": 2}, "": 3, "1st": 4, "-": 5});
        let ps = paths(&doc, "$..*");
        assert_eq!(
            ps,
            [
                "$['a b']",
                "$.ok_1",
                "$['']",
                "$['1st']",
                "$['-']",
                "$['a b']['it\\'s']",
                "$['a b']['it\\'s'][0]",
                "$['a b']['it\\'s'][0]['x\\\\y']",
                "$.ok_1.név",
            ]
        );
        for p in &ps {
            let found = query(&doc, p).unwrap();
            assert_eq!(found.len(), 1, "{p}");
            assert_eq!(&found[0].path_string(), p);
            assert_eq!(resolve(&doc, &found[0].path), Some(found[0].value));
        }
        assert_eq!(
            format_path(&[Segment::Key("data".into()), Segment::Index(0), Segment::Key("id".into())]),
            "$.data[0].id"
        );
    }

    #[test]
    fn dashed_names_outside_filters() {
        let doc = json!({"content-type": "json", "a": [{"x-y": 1}, {"x-y": 2}], "length": 3});
        assert_eq!(values(&doc, "$.content-type"), [json!("json")]);
        assert_eq!(values(&doc, "$.a[?(@['x-y'] > 1)]"), [json!({"x-y": 2})]);
        assert_eq!(values(&doc, "$.a[( @.length - 2 )]"), [json!({"x-y": 1})]);
        assert_eq!(values(&doc, "$.a[(@.length)]"), Vec::<Value>::new());
    }

    #[test]
    fn numbers_compare_by_value() {
        assert!(json_eq(&json!(1), &json!(1.0)));
        assert!(!json_eq(&json!(1), &json!("1")));
        assert!(json_eq(&json!({"a": [1, 2.0]}), &json!({"a": [1.0, 2]})));
        assert_eq!(values(&json!([{"v": 1.0}, {"v": 2}]), "$[?(@.v == 1)]").len(), 1);
    }
}
