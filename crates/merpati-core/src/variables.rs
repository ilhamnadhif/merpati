//! Variables, the five scopes, and `{{name}}` resolution.
//!
//! Precedence follows Postman: when the same name exists in several scopes the
//! narrowest wins: local > data > environment > collection > global.

use std::borrow::Cow;
use std::ops::Range;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::util::{default_true, is_default, is_true};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VarKind {
    #[default]
    Default,
    /// Masked in the UI; the value is never written to committed files.
    Secret,
}

/// Postman type of a variable's value, after postman-collection's
/// `Variable.types` (`string`, `number`, `boolean`, `any`).
///
/// Values are always stored as the text `{{name}}` resolves to: JavaScript's
/// `String(value)`, with objects and arrays as JSON. The type tells scripts
/// which value `get()` returns, and how `set()` converts a new value.
/// Behaviour follows postman-collection's `lib/collection/variable.js`
/// (`Variable.types`, `castIn`, `update`) and `variable-scope.js`
/// (`set(key, value, type)`); the four names are the ones of the Collection
/// v2.1 schema.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VarType {
    /// Postman `string`: a value set by a script is stored as `String(value)`.
    String,
    /// Postman `number`: the text of a JavaScript number (`5`, `1.5`,
    /// `1e+21`, `NaN`); set values are converted with `Number(value)`.
    Number,
    /// Postman `boolean`: `true` or `false`; set values are converted with
    /// `Boolean(value)`.
    Boolean,
    /// Postman `any` holding something other than a string: its JSON text
    /// (`5`, `true`, `null`, `{"a":1}`).
    Any,
    /// A string that scripts may replace with a value of any type: Postman's
    /// `any` holding a string, which is what variables without a type and
    /// the `default` type of Postman environments amount to. Unknown type
    /// names read from files mean this too.
    #[default]
    #[serde(other)]
    Default,
}

impl VarType {
    /// A Postman type name (`string`, `number`, `boolean`, `any`, any case).
    /// Other names (`default`, `secret`) mean `any` to Postman: `None`.
    pub fn parse(name: &str) -> Option<VarType> {
        match name.to_ascii_lowercase().as_str() {
            "string" => Some(VarType::String),
            "number" => Some(VarType::Number),
            "boolean" => Some(VarType::Boolean),
            "any" => Some(VarType::Any),
            _ => None,
        }
    }

    /// The Postman name of an explicit type; `None` for [`VarType::Default`].
    pub fn postman_name(self) -> Option<&'static str> {
        match self {
            VarType::Default => None,
            VarType::String => Some("string"),
            VarType::Number => Some("number"),
            VarType::Boolean => Some("boolean"),
            VarType::Any => Some("any"),
        }
    }

    /// Stores a Postman value given the variable's declared type, converting
    /// it like postman-collection's `Variable#castIn` (`String(v)`,
    /// `Number(v)`, `Boolean(v)`, or kept as is): returns its text and type.
    /// Without a type (or `any`) a string stays [`VarType::Default`] and any
    /// other value becomes [`VarType::Any`].
    pub fn cast(declared: Option<VarType>, value: &Value) -> (String, VarType) {
        match declared {
            Some(VarType::String) => (js_string(value), VarType::String),
            Some(VarType::Number) => (js_number_text(js_number(value)), VarType::Number),
            Some(VarType::Boolean) => (js_truthy(value).to_string(), VarType::Boolean),
            Some(VarType::Default | VarType::Any) | None => match value {
                Value::String(s) => (s.clone(), VarType::Default),
                Value::Number(n) => (js_number_text(n.as_f64().unwrap_or(f64::NAN)), VarType::Any),
                other => (other.to_string(), VarType::Any),
            },
        }
    }

    /// The value scripts see for a stored text. Numbers that JSON cannot
    /// carry (`NaN`, `Infinity`) stay text; so does `any` text that is not
    /// JSON (edited by hand).
    pub fn to_json(self, text: &str) -> Value {
        match self {
            VarType::Default | VarType::String => Value::String(text.to_string()),
            VarType::Number => {
                let n = js_string_to_number(text);
                if n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER {
                    Value::from(n as i64)
                } else {
                    serde_json::Number::from_f64(n).map(Value::Number).unwrap_or_else(|| js_number_text(n).into())
                }
            }
            VarType::Boolean => Value::Bool(text_to_bool(text)),
            VarType::Any => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string())),
        }
    }
}

/// 2^53 - 1: integral numbers up to this are exact in JavaScript.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A stored boolean: `true`/`false` as written by [`VarType::cast`]; other
/// text converts like JavaScript's `Boolean(text)` (true unless empty).
fn text_to_bool(text: &str) -> bool {
    match text {
        "true" => true,
        "false" => false,
        other => !other.is_empty(),
    }
}

/// JavaScript's `String(value)`.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number_text(n.as_f64().unwrap_or(f64::NAN)),
        Value::Array(items) => {
            items.iter().map(|x| if x.is_null() { String::new() } else { js_string(x) }).collect::<Vec<_>>().join(",")
        }
        Value::Object(_) => "[object Object]".into(),
    }
}

/// JavaScript's `Number(value)`.
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => js_string_to_number(s),
        other => js_string_to_number(&js_string(other)),
    }
}

/// JavaScript's `Boolean(value)`.
fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JavaScript's `Number(text)`: surrounding white space is ignored, empty
/// text is 0, `0x`/`0o`/`0b` prefixes and `Infinity` are understood and
/// anything else that is not a decimal literal is `NaN`.
fn js_string_to_number(text: &str) -> f64 {
    let t = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if t.is_empty() {
        return 0.0;
    }
    let radix = match t.get(..2) {
        Some("0x" | "0X") => 16,
        Some("0o" | "0O") => 8,
        Some("0b" | "0B") => 2,
        _ => 10,
    };
    if radix != 10 {
        let digits = &t[2..];
        if digits.is_empty() {
            return f64::NAN;
        }
        return digits
            .chars()
            .try_fold(0.0, |acc: f64, c| c.to_digit(radix).map(|d| acc * f64::from(radix) + f64::from(d)))
            .unwrap_or(f64::NAN);
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    if unsigned == "Infinity" {
        return if t.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    // Rust also parses `inf`, `nan` and `infinity`, which are NaN here.
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], Some(&unsigned[i + 1..])),
        None => (unsigned, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let valid = (!int.is_empty() || !frac.is_empty())
        && digits(int)
        && digits(frac)
        && exponent.is_none_or(|e| {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            !e.is_empty() && digits(e)
        });
    if valid { t.parse().unwrap_or(f64::NAN) } else { f64::NAN }
}

/// JavaScript's `String(n)` for a number: `5`, `1.5`, `1e+21`, `1e-7`, `NaN`.
fn js_number_text(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n == 0.0 {
        return "0".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    // Shortest round-trip digits and exponent, then ECMAScript's
    // Number::toString layout.
    let sci = format!("{:e}", n.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = digits.len() as i32;
    // The decimal point goes after `point` digits.
    let point = exp.parse::<i32>().unwrap_or(0) + 1;
    let body = if k <= point && point <= 21 {
        format!("{digits}{}", "0".repeat((point - k) as usize))
    } else if 0 < point && point <= 21 {
        let (int, frac) = digits.split_at(point as usize);
        format!("{int}.{frac}")
    } else if -6 < point && point <= 0 {
        format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
    } else {
        let e = point - 1;
        let sign = if e < 0 { '-' } else { '+' };
        let (first, rest) = digits.split_at(1);
        let dot = if rest.is_empty() { "" } else { "." };
        format!("{first}{dot}{rest}e{sign}{}", e.unsigned_abs())
    };
    if n < 0.0 { format!("-{body}") } else { body }
}

/// A variable in a collection, folder, environment or the globals.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Variable {
    pub key: String,
    /// Initial (shared) value. This is what gets committed to Git, except for
    /// secrets which are stored locally.
    #[serde(default)]
    pub value: String,
    /// Current (local) value. Never serialized with the variable itself;
    /// storage keeps it in the workspace's local state.
    #[serde(skip)]
    pub current: Option<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: VarKind,
    /// Postman type of the initial and current value ([`VarType`]), written
    /// as `"type"` in files only when it is not the default.
    #[serde(rename = "type", default, skip_serializing_if = "is_default")]
    pub ty: VarType,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl Variable {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Variable { key: key.into(), value: value.into(), enabled: true, ..Default::default() }
    }

    pub fn secret(key: impl Into<String>, value: impl Into<String>) -> Self {
        Variable { kind: VarKind::Secret, ..Variable::new(key, value) }
    }

    /// The value used for resolution: current if set, otherwise initial.
    pub fn effective(&self) -> &str {
        self.current.as_deref().unwrap_or(&self.value)
    }

    pub fn is_secret(&self) -> bool {
        self.kind == VarKind::Secret
    }

    /// The effective value as scripts see it, e.g. `5` for a number.
    pub fn effective_json(&self) -> Value {
        self.ty.to_json(self.effective())
    }
}

/// Converts a list of variables to the `key -> effective value` map used for
/// resolution and scripts. Disabled variables are skipped; later duplicates win.
pub fn to_map(vars: &[Variable]) -> IndexMap<String, String> {
    vars.iter().filter(|v| v.enabled && !v.key.is_empty()).map(|v| (v.key.clone(), v.effective().to_string())).collect()
}

/// The [`ScopeTypes`] entries of a list of variables: the type of each name
/// [`to_map`] lists, where it is not [`VarType::Default`]. Use it next to
/// [`to_map`] for the same list (`scopes.types.collection =
/// to_types(&collection.variables)`).
pub fn to_types(vars: &[Variable]) -> IndexMap<String, VarType> {
    let mut types = IndexMap::new();
    for v in vars.iter().filter(|v| v.enabled && !v.key.is_empty()) {
        if v.ty == VarType::Default {
            types.shift_remove(&v.key);
        } else {
            types.insert(v.key.clone(), v.ty);
        }
    }
    types
}

/// Writes back values produced by scripts into a variable list: changed
/// values become the *current* value, new keys are appended, removed keys are
/// deleted. Mirrors how Postman scripts only ever touch current values.
pub fn apply_map(vars: &mut Vec<Variable>, map: &IndexMap<String, String>) {
    vars.retain(|v| !v.enabled || v.key.is_empty() || map.contains_key(&v.key));
    for (k, val) in map {
        match vars.iter_mut().rev().find(|v| v.enabled && &v.key == k) {
            Some(v) => {
                if v.effective() != val {
                    v.current = Some(val.clone());
                }
            }
            None => {
                let mut v = Variable::new(k.clone(), String::new());
                v.current = Some(val.clone());
                vars.push(v);
            }
        }
    }
}

/// [`apply_map`] that also stores the types scripts gave the values: each
/// name in `map` gets its type (the [`Variable::ty`] field) from `types`,
/// [`VarType::Default`] if absent. Pass a scope's values and types from the
/// same [`VariableScopes`], e.g. `apply_typed(&mut env.variables,
/// &scopes.environment, &scopes.types.environment)`, and only for scopes
/// whose types were filled with [`to_types`]: otherwise every type is reset.
pub fn apply_typed(vars: &mut Vec<Variable>, map: &IndexMap<String, String>, types: &IndexMap<String, VarType>) {
    apply_map(vars, map);
    for k in map.keys() {
        if let Some(v) = vars.iter_mut().rev().find(|v| v.enabled && &v.key == k) {
            v.ty = types.get(k).copied().unwrap_or_default();
        }
    }
}

/// Types of the values in [`VariableScopes`], per scope. Names without an
/// entry are [`VarType::Default`] strings; data values are JSON already.
///
/// Fill each scope's types where its values are filled from stored
/// variables, and write script changes back with [`apply_typed`]:
///
/// ```
/// use merpati_core::variables::{self, VariableScopes};
/// use merpati_core::{Environment, VarType, Variable};
///
/// let globals = vec![Variable { ty: VarType::Number, ..Variable::new("n", "5") }];
/// let mut env = Environment::new("Dev").with_var("host", "localhost");
/// let mut scopes = VariableScopes { globals: variables::to_map(&globals), ..Default::default() };
/// scopes.types.globals = variables::to_types(&globals);
/// scopes.environment = env.to_map();
/// scopes.types.environment = env.to_types();
/// // ... `job.variables = scopes`, run the job, `scopes = outcome.variables` ...
/// variables::apply_typed(&mut env.variables, &scopes.environment, &scopes.types.environment);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeTypes {
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub globals: IndexMap<String, VarType>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub collection: IndexMap<String, VarType>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub environment: IndexMap<String, VarType>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub local: IndexMap<String, VarType>,
}

impl ScopeTypes {
    pub fn is_empty(&self) -> bool {
        self.globals.is_empty() && self.collection.is_empty() && self.environment.is_empty() && self.local.is_empty()
    }
}

/// The resolved values of every scope at one point in time. This is what the
/// script engine reads and returns, and what [`Resolver`] resolves against.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VariableScopes {
    #[serde(default)]
    pub globals: IndexMap<String, String>,
    /// Collection variables, with folder variables of the request's ancestors
    /// layered on top.
    #[serde(default)]
    pub collection: IndexMap<String, String>,
    #[serde(default)]
    pub environment: IndexMap<String, String>,
    #[serde(default)]
    pub environment_name: Option<String>,
    /// Current iteration's row from a data file. Values keep their JSON type.
    #[serde(default)]
    pub data: IndexMap<String, Value>,
    /// Request-local variables (`pm.variables.set`), cleared per request.
    #[serde(default)]
    pub local: IndexMap<String, String>,
    /// Postman types of the values above, which scripts read and set (a
    /// number set in a pre-request script is a number in the test script).
    /// Fill it with [`to_types`] next to [`to_map`]; without it every value
    /// is a [`VarType::Default`] string.
    #[serde(default, skip_serializing_if = "ScopeTypes::is_empty")]
    pub types: ScopeTypes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    Local,
    Data,
    Environment,
    Collection,
    Global,
    Dynamic,
    Vault,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Local => "Local",
            Scope::Data => "Data",
            Scope::Environment => "Environment",
            Scope::Collection => "Collection",
            Scope::Global => "Global",
            Scope::Dynamic => "Dynamic",
            Scope::Vault => "Vault",
        }
    }
}

pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl VariableScopes {
    /// Looks a name up through the scopes, narrowest first.
    pub fn get(&self, key: &str) -> Option<(Cow<'_, str>, Scope)> {
        if let Some(v) = self.local.get(key) {
            return Some((Cow::Borrowed(v), Scope::Local));
        }
        if let Some(v) = self.data.get(key) {
            return Some((Cow::Owned(value_to_string(v)), Scope::Data));
        }
        if let Some(v) = self.environment.get(key) {
            return Some((Cow::Borrowed(v), Scope::Environment));
        }
        if let Some(v) = self.collection.get(key) {
            return Some((Cow::Borrowed(v), Scope::Collection));
        }
        self.globals.get(key).map(|v| (Cow::Borrowed(v.as_str()), Scope::Global))
    }

    /// Every visible name with its winning value, for autocomplete.
    pub fn all_names(&self) -> Vec<(String, String, Scope)> {
        let mut seen = IndexMap::new();
        for (k, v) in &self.globals {
            seen.insert(k.clone(), (v.clone(), Scope::Global));
        }
        for (k, v) in &self.collection {
            seen.insert(k.clone(), (v.clone(), Scope::Collection));
        }
        for (k, v) in &self.environment {
            seen.insert(k.clone(), (v.clone(), Scope::Environment));
        }
        for (k, v) in &self.data {
            seen.insert(k.clone(), (value_to_string(v), Scope::Data));
        }
        for (k, v) in &self.local {
            seen.insert(k.clone(), (v.clone(), Scope::Local));
        }
        seen.into_iter().map(|(k, (v, s))| (k, v, s)).collect()
    }
}

/// Source of `{{vault:name}}` values.
pub trait VaultLookup: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
}

pub const VAULT_PREFIX: &str = "vault:";
const MAX_DEPTH: usize = 10;

/// Resolves `{{name}}` references against a set of scopes.
#[derive(Clone, Copy)]
pub struct Resolver<'a> {
    pub scopes: &'a VariableScopes,
    pub vault: Option<&'a dyn VaultLookup>,
}

impl<'a> Resolver<'a> {
    pub fn new(scopes: &'a VariableScopes) -> Self {
        Resolver { scopes, vault: None }
    }

    pub fn with_vault(mut self, vault: &'a dyn VaultLookup) -> Self {
        self.vault = Some(vault);
        self
    }

    /// Value of a single variable name (without braces), with its scope.
    /// Dynamic variables produce a fresh value on every call.
    pub fn lookup(&self, name: &str) -> Option<(String, Scope)> {
        let name = name.trim();
        if let Some(rest) = name.strip_prefix(VAULT_PREFIX) {
            return self.vault.and_then(|v| v.get(rest.trim())).map(|v| (v, Scope::Vault));
        }
        if let Some((v, scope)) = self.scopes.get(name) {
            return Some((v.into_owned(), scope));
        }
        if name.starts_with('$') {
            return crate::dynamic::generate(name).map(|v| (v, Scope::Dynamic));
        }
        None
    }

    /// Resolves every reference in `input`. Unknown names are left as-is.
    pub fn resolve(&self, input: &str) -> String {
        self.resolve_inner(input, &mut Vec::new(), &mut None)
    }

    /// Like [`resolve`](Self::resolve) and also reports unresolved names.
    pub fn resolve_tracking(&self, input: &str, unresolved: &mut Vec<String>) -> String {
        let mut sink = Some(std::mem::take(unresolved));
        let out = self.resolve_inner(input, &mut Vec::new(), &mut sink);
        *unresolved = sink.unwrap_or_default();
        out
    }

    /// `expanding` holds the names whose values are being resolved; a
    /// reference back to one of them (a cycle) is left as-is.
    fn resolve_inner(&self, input: &str, expanding: &mut Vec<String>, unresolved: &mut Option<Vec<String>>) -> String {
        if !input.contains("{{") {
            return input.to_string();
        }
        let mut out = String::with_capacity(input.len());
        for tok in tokenize(input) {
            match tok.kind {
                TokenKind::Text => out.push_str(&input[tok.range]),
                TokenKind::Variable => {
                    let name = &input[tok.name_range.clone()];
                    if expanding.iter().any(|n| n == name.trim()) {
                        out.push_str(&input[tok.range]);
                        continue;
                    }
                    match self.lookup(name) {
                        Some((value, _)) if expanding.len() < MAX_DEPTH && value.contains("{{") => {
                            expanding.push(name.trim().to_string());
                            out.push_str(&self.resolve_inner(&value, expanding, unresolved));
                            expanding.pop();
                        }
                        Some((value, _)) => out.push_str(&value),
                        None => {
                            if let Some(list) = unresolved.as_mut()
                                && !list.iter().any(|n| n == name)
                            {
                                list.push(name.to_string());
                            }
                            out.push_str(&input[tok.range]);
                        }
                    }
                }
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Text,
    Variable,
}

/// A piece of a string: plain text or a `{{reference}}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte range in the input, including the braces for variables.
    pub range: Range<usize>,
    /// Byte range of the name inside the braces (equals `range` for text).
    pub name_range: Range<usize>,
}

/// Splits a string into text and `{{variable}}` tokens. A reference cannot
/// contain `{` or `}` and must not be empty, matching Postman.
pub fn tokenize(input: &str) -> Vec<Token> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find the closing braces, rejecting nested braces.
            let name_start = i + 2;
            let mut j = name_start;
            let mut ok = false;
            while j < bytes.len() {
                match bytes[j] {
                    b'}' if j + 1 < bytes.len() && bytes[j + 1] == b'}' => {
                        ok = j > name_start;
                        break;
                    }
                    b'{' | b'}' => break,
                    _ => j += 1,
                }
            }
            if ok {
                if text_start < i {
                    tokens.push(Token { kind: TokenKind::Text, range: text_start..i, name_range: text_start..i });
                }
                tokens.push(Token { kind: TokenKind::Variable, range: i..j + 2, name_range: name_start..j });
                i = j + 2;
                text_start = i;
                continue;
            }
        }
        i += 1;
    }
    if text_start < input.len() {
        tokens.push(Token {
            kind: TokenKind::Text,
            range: text_start..input.len(),
            name_range: text_start..input.len(),
        });
    }
    tokens
}

/// Names referenced in a string, in order of appearance.
pub fn references(input: &str) -> Vec<&str> {
    tokenize(input).into_iter().filter(|t| t.kind == TokenKind::Variable).map(|t| input[t.name_range].trim()).collect()
}

/// True when the whole string is exactly one `{{reference}}`.
pub fn is_pure_reference(input: &str) -> bool {
    let t = tokenize(input.trim());
    t.len() == 1 && t[0].kind == TokenKind::Variable
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes() -> VariableScopes {
        let mut s = VariableScopes::default();
        s.globals.insert("host".into(), "global.test".into());
        s.globals.insert("only_global".into(), "g".into());
        s.collection.insert("host".into(), "collection.test".into());
        s.environment.insert("host".into(), "env.test".into());
        s.environment.insert("url".into(), "https://{{host}}/{{path}}".into());
        s.environment.insert("path".into(), "v1".into());
        s.environment.insert("loop".into(), "{{loop}}".into());
        s
    }

    #[test]
    fn precedence() {
        let mut s = scopes();
        let r = Resolver::new(&s);
        assert_eq!(r.resolve("{{host}}"), "env.test");
        assert_eq!(r.resolve("{{only_global}}"), "g");
        s.data.insert("host".into(), Value::String("data.test".into()));
        assert_eq!(Resolver::new(&s).resolve("{{host}}"), "data.test");
        s.local.insert("host".into(), "local.test".into());
        assert_eq!(Resolver::new(&s).resolve("{{host}}"), "local.test");
    }

    #[test]
    fn nested_and_unresolved() {
        let s = scopes();
        let r = Resolver::new(&s);
        assert_eq!(r.resolve("{{url}}/users"), "https://env.test/v1/users");
        let mut missing = Vec::new();
        assert_eq!(r.resolve_tracking("a {{nope}} b {{ host }}", &mut missing), "a {{nope}} b env.test");
        assert_eq!(missing, vec!["nope"]);
        // Self reference terminates.
        assert!(r.resolve("{{loop}}").contains("{{loop}}"));
    }

    #[test]
    fn self_references_do_not_multiply() {
        let mut s = VariableScopes::default();
        s.environment.insert("twice".into(), "{{twice}}{{twice}}".into());
        s.environment.insert("prefix".into(), "a{{prefix}}".into());
        s.environment.insert("a".into(), "{{b}}".into());
        s.environment.insert("b".into(), "x{{a}}".into());
        // Without cycle detection the first grows to 2^10 copies (and to
        // billions with more repetitions, hanging the app).
        let r = Resolver::new(&s);
        assert_eq!(r.resolve("{{twice}}"), "{{twice}}{{twice}}");
        assert_eq!(r.resolve("{{prefix}}"), "a{{prefix}}");
        assert_eq!(r.resolve("{{a}}"), "x{{a}}");
        // The same variable may still appear several times without a cycle.
        s.environment.insert("host".into(), "h".into());
        s.environment.insert("pair".into(), "{{host}}-{{host}}".into());
        assert_eq!(Resolver::new(&s).resolve("{{pair}}/{{pair}}"), "h-h/h-h");
    }

    #[test]
    fn tokenizer_edge_cases() {
        assert_eq!(references("{{a}}{{b}}"), vec!["a", "b"]);
        assert!(references("{{}}").is_empty());
        assert!(references("{{a{b}}").is_empty());
        assert_eq!(references("{{{x}}}"), vec!["x"]);
        assert_eq!(references("json {\"a\": \"{{v}}\"}"), vec!["v"]);
        assert!(is_pure_reference(" {{token}} "));
        assert!(!is_pure_reference("Bearer {{token}}"));
    }

    #[test]
    fn dynamic_values_are_fresh() {
        let s = VariableScopes::default();
        let r = Resolver::new(&s);
        let a = r.resolve("{{$guid}}");
        let b = r.resolve("{{$guid}}");
        assert_eq!(a.len(), 36);
        assert_ne!(a, b);
        assert!(r.resolve("{{$timestamp}}").parse::<i64>().is_ok());
        assert_eq!(r.resolve("{{$notAThing}}"), "{{$notAThing}}");
    }

    #[test]
    fn vault_lookup() {
        struct V;
        impl VaultLookup for V {
            fn get(&self, key: &str) -> Option<String> {
                (key == "api").then(|| "s3cret".into())
            }
        }
        let s = VariableScopes::default();
        let r = Resolver::new(&s).with_vault(&V);
        assert_eq!(r.resolve("{{vault:api}}"), "s3cret");
        assert_eq!(r.resolve("{{vault:other}}"), "{{vault:other}}");
    }

    #[test]
    fn numbers_print_like_javascript() {
        let cases = [
            (5.0, "5"),
            (-5.0, "-5"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (-0.0, "0"),
            (100.0, "100"),
            (123_456_789.125, "123456789.125"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.23e22, "1.23e+22"),
            (0.000_001, "0.000001"),
            (1e-7, "1e-7"),
            (-2.5e-8, "-2.5e-8"),
            (9_007_199_254_740_993.0, "9007199254740992"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        for (n, text) in cases {
            assert_eq!(js_number_text(n), text, "{n}");
        }
    }

    #[test]
    fn text_converts_to_numbers_like_javascript() {
        let cases = [
            ("42", 42.0),
            (" 4.5\n", 4.5),
            ("", 0.0),
            ("  ", 0.0),
            ("-1e3", -1000.0),
            ("+.5", 0.5),
            ("5.", 5.0),
            ("1E+2", 100.0),
            ("0x1F", 31.0),
            ("0b101", 5.0),
            ("0o17", 15.0),
            ("Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
        ];
        for (text, n) in cases {
            assert_eq!(js_string_to_number(text), n, "{text:?}");
        }
        for text in
            ["abc", "1,5", "inf", "nan", "infinity", "1e", ".", "e5", "-0x1", "0x", "0xg", "1_000", "--1", "1 2"]
        {
            assert!(js_string_to_number(text).is_nan(), "{text:?}");
        }
    }

    #[test]
    fn values_are_cast_like_postman() {
        use VarType::*;
        use serde_json::json;
        let cast = |ty: Option<VarType>, v: Value| {
            let (text, ty) = VarType::cast(ty, &v);
            (text, ty)
        };
        let t = |s: &str, ty: VarType| (s.to_string(), ty);
        // No type (or `any`): strings stay plain, other values keep their type.
        assert_eq!(cast(None, json!("5")), t("5", Default));
        assert_eq!(cast(Some(Any), json!("5")), t("5", Default));
        assert_eq!(cast(None, json!(5)), t("5", Any));
        assert_eq!(cast(None, json!(1.0)), t("1", Any));
        assert_eq!(cast(None, json!(true)), t("true", Any));
        assert_eq!(cast(None, json!(null)), t("null", Any));
        assert_eq!(cast(None, json!({"a": [1, "x"]})), t(r#"{"a":[1,"x"]}"#, Any));
        // `String(v)`, `Number(v)`, `Boolean(v)`.
        assert_eq!(cast(Some(String), json!(5)), t("5", String));
        assert_eq!(cast(Some(String), json!(null)), t("null", String));
        assert_eq!(cast(Some(String), json!({"a": 1})), t("[object Object]", String));
        assert_eq!(cast(Some(String), json!([1, null, [2, 3]])), t("1,,2,3", String));
        assert_eq!(cast(Some(Number), json!("12")), t("12", Number));
        assert_eq!(cast(Some(Number), json!(" 1.50 ")), t("1.5", Number));
        assert_eq!(cast(Some(Number), json!("abc")), t("NaN", Number));
        assert_eq!(cast(Some(Number), json!(true)), t("1", Number));
        assert_eq!(cast(Some(Number), json!(null)), t("0", Number));
        assert_eq!(cast(Some(Number), json!([7])), t("7", Number));
        assert_eq!(cast(Some(Number), json!({})), t("NaN", Number));
        assert_eq!(cast(Some(Boolean), json!(false)), t("false", Boolean));
        assert_eq!(cast(Some(Boolean), json!("")), t("false", Boolean));
        assert_eq!(cast(Some(Boolean), json!(0)), t("false", Boolean));
        assert_eq!(cast(Some(Boolean), json!(null)), t("false", Boolean));
        // Postman's `Boolean("false")` is true.
        assert_eq!(cast(Some(Boolean), json!("false")), t("true", Boolean));
        assert_eq!(cast(Some(Boolean), json!({})), t("true", Boolean));

        // And back to the values scripts see.
        assert_eq!(Number.to_json("5"), json!(5));
        assert_eq!(Number.to_json("-1.5"), json!(-1.5));
        assert_eq!(Number.to_json("1e+21"), json!(1e21));
        assert_eq!(Number.to_json("NaN"), json!("NaN"));
        assert_eq!(Number.to_json("Infinity"), json!("Infinity"));
        assert_eq!(Boolean.to_json("false"), json!(false));
        assert_eq!(Boolean.to_json("true"), json!(true));
        assert_eq!(Boolean.to_json(""), json!(false));
        assert_eq!(Any.to_json(r#"{"a":1}"#), json!({"a": 1}));
        assert_eq!(Any.to_json("null"), json!(null));
        assert_eq!(Any.to_json("not json"), json!("not json"));
        assert_eq!(String.to_json("5"), json!("5"));
        assert_eq!(Default.to_json("5"), json!("5"));
        assert_eq!(VarType::parse("Number"), Some(Number));
        assert_eq!(VarType::parse("default"), None);
        assert_eq!(VarType::parse("secret"), None);
    }

    #[test]
    fn types_are_optional_in_files() {
        // Written before variables had types.
        let old: Variable = serde_json::from_str(r#"{"key": "a", "value": "1", "kind": "secret"}"#).unwrap();
        assert_eq!(old.ty, VarType::Default);
        assert!(old.is_secret());
        // The default type is not written, so existing files do not change.
        assert_eq!(serde_json::to_string(&Variable::new("a", "1")).unwrap(), r#"{"key":"a","value":"1"}"#);
        for ty in [VarType::String, VarType::Number, VarType::Boolean, VarType::Any] {
            let v = Variable { ty, ..Variable::new("v", "1") };
            let text = serde_json::to_string(&v).unwrap();
            assert_eq!(text, format!(r#"{{"key":"v","value":"1","type":"{}"}}"#, ty.postman_name().unwrap()));
            assert_eq!(serde_json::from_str::<Variable>(&text).unwrap(), v);
        }
        // Type names this version does not know read as plain strings.
        let v: Variable = serde_json::from_str(r#"{"key": "a", "value": "1", "type": "date"}"#).unwrap();
        assert_eq!(v.ty, VarType::Default);
        // Scopes without types (an older peer) and without typed values.
        let s: VariableScopes = serde_json::from_str(r#"{"environment": {"a": "1"}}"#).unwrap();
        assert!(s.types.is_empty());
        assert!(!serde_json::to_string(&s).unwrap().contains("types"));
        let mut typed = s.clone();
        typed.types.environment.insert("a".into(), VarType::Number);
        let text = serde_json::to_string(&typed).unwrap();
        assert!(text.contains(r#""types":{"environment":{"a":"number"}}"#), "{text}");
        assert_eq!(serde_json::from_str::<VariableScopes>(&text).unwrap(), typed);
    }

    #[test]
    fn types_follow_values() {
        let typed = |k: &str, v: &str, ty| Variable { ty, ..Variable::new(k, v) };
        let mut vars = vec![
            typed("n", "1", VarType::Number),
            Variable::new("s", "x"),
            Variable { enabled: false, ..typed("off", "true", VarType::Boolean) },
        ];
        assert_eq!(to_types(&vars), IndexMap::from([("n".to_string(), VarType::Number)]));
        // A later plain duplicate wins, like in `to_map`.
        vars.push(Variable::new("n", "2"));
        assert!(to_types(&vars).is_empty());
        vars.pop();

        let mut map = to_map(&vars);
        map.insert("n".into(), "2".into());
        map.insert("s".into(), "3".into());
        map.insert("new".into(), r#"{"a":1}"#.into());
        let types = IndexMap::from([
            ("n".to_string(), VarType::Number),
            ("s".into(), VarType::Any),
            ("new".into(), VarType::Any),
        ]);
        apply_typed(&mut vars, &map, &types);
        assert_eq!(vars[0].effective_json(), serde_json::json!(2));
        assert_eq!(vars[0].value, "1");
        assert_eq!((vars[1].ty, vars[1].effective_json()), (VarType::Any, serde_json::json!(3)));
        assert_eq!(vars[2].ty, VarType::Boolean, "disabled variables are left alone");
        assert_eq!((vars[3].key.as_str(), vars[3].effective_json()), ("new", serde_json::json!({"a": 1})));
        // Without types (older callers) values change and types stay.
        map.insert("n".into(), "3".into());
        apply_map(&mut vars, &map);
        assert_eq!(vars[0].effective_json(), serde_json::json!(3));
        // `{{name}}` always resolves to the text.
        let scopes = VariableScopes { collection: to_map(&vars), types: ScopeTypes::default(), ..Default::default() };
        assert_eq!(Resolver::new(&scopes).resolve("{{n}}/{{new}}"), r#"3/{"a":1}"#);
    }

    #[test]
    fn apply_map_tracks_current() {
        let mut vars = vec![Variable::new("a", "1"), Variable::new("b", "2")];
        let mut map = to_map(&vars);
        map.insert("a".into(), "10".into());
        map.shift_remove("b");
        map.insert("c".into(), "3".into());
        apply_map(&mut vars, &map);
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].value, "1");
        assert_eq!(vars[0].effective(), "10");
        assert_eq!(vars[1].key, "c");
        assert_eq!(vars[1].effective(), "3");
    }
}
