//! GraphQL helpers for the schema explorer and query editor (FR-X1):
//! the standard introspection query, a typed view of its result, context
//! aware autocomplete and query skeletons.

use indexmap::IndexMap;
use serde_json::Value;

use crate::ConvertError;

pub const INTROSPECTION_QUERY: &str = r#"query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types {
      ...FullType
    }
    directives {
      name
      description
      locations
      args {
        ...InputValue
      }
    }
  }
}

fragment FullType on __Type {
  kind
  name
  description
  fields(includeDeprecated: true) {
    name
    description
    args {
      ...InputValue
    }
    type {
      ...TypeRef
    }
    isDeprecated
    deprecationReason
  }
  inputFields {
    ...InputValue
  }
  interfaces {
    ...TypeRef
  }
  enumValues(includeDeprecated: true) {
    name
    description
    isDeprecated
    deprecationReason
  }
  possibleTypes {
    ...TypeRef
  }
}

fragment InputValue on __InputValue {
  name
  description
  type {
    ...TypeRef
  }
  defaultValue
}

fragment TypeRef on __Type {
  kind
  name
  ofType {
    kind
    name
    ofType {
      kind
      name
      ofType {
        kind
        name
        ofType {
          kind
          name
          ofType {
            kind
            name
            ofType {
              kind
              name
              ofType {
                kind
                name
              }
            }
          }
        }
      }
    }
  }
}"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeKind {
    Scalar,
    Object,
    Interface,
    Union,
    Enum,
    InputObject,
}

impl TypeKind {
    fn parse(s: &str) -> Option<TypeKind> {
        Some(match s {
            "SCALAR" => TypeKind::Scalar,
            "OBJECT" => TypeKind::Object,
            "INTERFACE" => TypeKind::Interface,
            "UNION" => TypeKind::Union,
            "ENUM" => TypeKind::Enum,
            "INPUT_OBJECT" => TypeKind::InputObject,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            TypeKind::Scalar => "scalar",
            TypeKind::Object => "type",
            TypeKind::Interface => "interface",
            TypeKind::Union => "union",
            TypeKind::Enum => "enum",
            TypeKind::InputObject => "input",
        }
    }
}

/// A (possibly wrapped) type reference: `[User!]!`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeRef {
    Named(String),
    List(Box<TypeRef>),
    NonNull(Box<TypeRef>),
}

impl TypeRef {
    fn parse(v: &Value) -> Option<TypeRef> {
        match v.get("kind")?.as_str()? {
            "NON_NULL" => Some(TypeRef::NonNull(Box::new(TypeRef::parse(v.get("ofType")?)?))),
            "LIST" => Some(TypeRef::List(Box::new(TypeRef::parse(v.get("ofType")?)?))),
            _ => Some(TypeRef::Named(v.get("name")?.as_str()?.to_string())),
        }
    }

    /// The innermost named type.
    pub fn named(&self) -> &str {
        match self {
            TypeRef::Named(n) => n,
            TypeRef::List(t) | TypeRef::NonNull(t) => t.named(),
        }
    }

    pub fn is_non_null(&self) -> bool {
        matches!(self, TypeRef::NonNull(_))
    }

    pub fn display(&self) -> String {
        match self {
            TypeRef::Named(n) => n.clone(),
            TypeRef::List(t) => format!("[{}]", t.display()),
            TypeRef::NonNull(t) => format!("{}!", t.display()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InputValue {
    pub name: String,
    pub description: Option<String>,
    pub ty: TypeRef,
    pub default_value: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub description: Option<String>,
    pub args: Vec<InputValue>,
    pub ty: TypeRef,
    pub deprecated: bool,
    pub deprecation_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumValue {
    pub name: String,
    pub description: Option<String>,
    pub deprecated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeDef {
    pub name: String,
    pub kind: TypeKind,
    pub description: Option<String>,
    pub fields: Vec<Field>,
    pub input_fields: Vec<InputValue>,
    pub enum_values: Vec<EnumValue>,
    pub interfaces: Vec<String>,
    pub possible_types: Vec<String>,
}

impl TypeDef {
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// True for built-in introspection types (`__Schema`, ...).
    pub fn is_introspection(&self) -> bool {
        self.name.starts_with("__")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Directive {
    pub name: String,
    pub description: Option<String>,
    pub locations: Vec<String>,
    pub args: Vec<InputValue>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Schema {
    pub query_type: Option<String>,
    pub mutation_type: Option<String>,
    pub subscription_type: Option<String>,
    pub types: IndexMap<String, TypeDef>,
    pub directives: Vec<Directive>,
}

impl Schema {
    pub fn get(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(name)
    }

    /// Root type name for `query`, `mutation` or `subscription`.
    pub fn root(&self, operation: &str) -> Option<&str> {
        match operation.to_ascii_lowercase().as_str() {
            "query" => self.query_type.as_deref(),
            "mutation" => self.mutation_type.as_deref(),
            "subscription" => self.subscription_type.as_deref(),
            _ => None,
        }
    }

    /// Named user types (no introspection types), sorted by name.
    pub fn user_types(&self) -> Vec<&TypeDef> {
        let mut v: Vec<&TypeDef> = self.types.values().filter(|t| !t.is_introspection()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn input_values(v: &Value, key: &str) -> Vec<InputValue> {
    list(v, key)
        .iter()
        .filter_map(|a| {
            Some(InputValue {
                name: a.get("name")?.as_str()?.to_string(),
                description: opt_str(a, "description"),
                ty: TypeRef::parse(a.get("type")?)?,
                default_value: opt_str(a, "defaultValue"),
            })
        })
        .collect()
}

/// Parses an introspection result. Accepts the full response (`{"data":
/// {"__schema": ...}}`), the `data` object, or `__schema` itself.
pub fn parse_introspection(json: &Value) -> Result<Schema, ConvertError> {
    let schema = json
        .pointer("/data/__schema")
        .or_else(|| json.get("__schema"))
        .or_else(|| json.get("types").map(|_| json))
        .ok_or_else(|| {
            let msg = json
                .pointer("/errors/0/message")
                .and_then(Value::as_str)
                .map(|m| format!("introspection failed: {m}"))
                .unwrap_or_else(|| "not an introspection result (missing __schema)".into());
            ConvertError::Format(msg)
        })?;
    let mut out = Schema {
        query_type: schema.pointer("/queryType/name").and_then(Value::as_str).map(str::to_string),
        mutation_type: schema.pointer("/mutationType/name").and_then(Value::as_str).map(str::to_string),
        subscription_type: schema.pointer("/subscriptionType/name").and_then(Value::as_str).map(str::to_string),
        ..Default::default()
    };
    for t in list(schema, "types") {
        let (Some(name), Some(kind)) =
            (t.get("name").and_then(Value::as_str), t.get("kind").and_then(Value::as_str).and_then(TypeKind::parse))
        else {
            continue;
        };
        let fields = list(t, "fields")
            .iter()
            .filter_map(|f| {
                Some(Field {
                    name: f.get("name")?.as_str()?.to_string(),
                    description: opt_str(f, "description"),
                    args: input_values(f, "args"),
                    ty: TypeRef::parse(f.get("type")?)?,
                    deprecated: f.get("isDeprecated").and_then(Value::as_bool).unwrap_or(false),
                    deprecation_reason: opt_str(f, "deprecationReason"),
                })
            })
            .collect();
        let names = |key: &str| -> Vec<String> {
            list(t, key).iter().filter_map(|x| x.get("name").and_then(Value::as_str).map(str::to_string)).collect()
        };
        out.types.insert(
            name.to_string(),
            TypeDef {
                name: name.to_string(),
                kind,
                description: opt_str(t, "description"),
                fields,
                input_fields: input_values(t, "inputFields"),
                enum_values: list(t, "enumValues")
                    .iter()
                    .filter_map(|e| {
                        Some(EnumValue {
                            name: e.get("name")?.as_str()?.to_string(),
                            description: opt_str(e, "description"),
                            deprecated: e.get("isDeprecated").and_then(Value::as_bool).unwrap_or(false),
                        })
                    })
                    .collect(),
                interfaces: names("interfaces"),
                possible_types: names("possibleTypes"),
            },
        );
    }
    for d in list(schema, "directives") {
        if let Some(name) = d.get("name").and_then(Value::as_str) {
            out.directives.push(Directive {
                name: name.to_string(),
                description: opt_str(d, "description"),
                locations: list(d, "locations").iter().filter_map(Value::as_str).map(str::to_string).collect(),
                args: input_values(d, "args"),
            });
        }
    }
    if out.types.is_empty() {
        return Err(ConvertError::Format("introspection result has no types".into()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Autocomplete
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionKind {
    Field,
    Argument,
    Keyword,
    Fragment,
    EnumValue,
    InputField,
    Directive,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub label: String,
    /// Type signature or short description (`String!`, `[User]`).
    pub detail: String,
    /// Text to insert in place of the typed prefix.
    pub insert: String,
    pub kind: SuggestionKind,
    pub deprecated: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Name(String),
    Punct(char),
    Spread,
    Value,
}

/// Tokens of `src`, skipping whitespace, commas, comments and strings
/// (strings become `Tok::Value`).
fn tokenize(src: &str) -> Vec<Tok> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() || c == ',' || c == '\u{feff}' {
            i += 1;
        } else if c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '"' {
            if chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"') {
                i += 3;
                while i < chars.len()
                    && !(chars[i] == '"' && chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"'))
                {
                    i += 1;
                }
                i += 3;
            } else {
                i += 1;
                while i < chars.len() && chars[i] != '"' && chars[i] != '\n' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            out.push(Tok::Value);
        } else if c == '.' && chars.get(i + 1) == Some(&'.') && chars.get(i + 2) == Some(&'.') {
            out.push(Tok::Spread);
            i += 3;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(Tok::Name(chars[start..i].iter().collect()));
        } else if c.is_ascii_digit() || c == '-' {
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '+' || chars[i] == '-')
            {
                i += 1;
            }
            out.push(Tok::Value);
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    out
}

#[derive(Clone, Debug)]
enum Frame {
    /// Inside `{ }` of a selection on this type (None when unknown).
    Selection(Option<String>),
    /// Inside `( )` of a field's arguments.
    Args { parent: Option<String>, field: String, used: Vec<String> },
    /// Inside `{ }` of an input object value of this input type.
    InputObject(Option<String>),
    /// Variable definitions, directive args, lists: no suggestions.
    Other,
}

struct Walker<'s> {
    schema: &'s Schema,
    stack: Vec<Frame>,
    /// Type the next `{` opens a selection on.
    pending: Option<Option<String>>,
    /// Last field name seen in the current selection (for `(` and `{`).
    last_field: Option<String>,
    /// Last argument name seen (for input object values).
    last_arg: Option<String>,
}

impl<'s> Walker<'s> {
    fn field_type(&self, parent: Option<&str>, field: &str) -> Option<String> {
        if field == "__typename" {
            return Some("String".into());
        }
        let t = self.schema.get(parent?)?;
        t.field(field).map(|f| f.ty.named().to_string())
    }

    fn arg_type(&self, parent: Option<&str>, field: &str, arg: &str) -> Option<String> {
        let t = self.schema.get(parent?)?;
        let f = t.field(field)?;
        f.args.iter().find(|a| a.name == arg).map(|a| a.ty.named().to_string())
    }

    fn input_field_type(&self, input: Option<&str>, name: &str) -> Option<String> {
        let t = self.schema.get(input?)?;
        t.input_fields.iter().find(|a| a.name == name).map(|a| a.ty.named().to_string())
    }

    fn run(&mut self, toks: &[Tok]) {
        let mut i = 0;
        while i < toks.len() {
            let tok = &toks[i];
            let next = toks.get(i + 1);
            match self.stack.last().cloned() {
                None => match tok {
                    Tok::Name(n) if matches!(n.as_str(), "query" | "mutation" | "subscription") => {
                        self.pending = Some(self.schema.root(n).map(str::to_string));
                    }
                    Tok::Name(n) if n == "fragment" => {
                        // fragment Name on Type
                        if let (Some(Tok::Name(_)), Some(Tok::Name(on)), Some(Tok::Name(ty))) =
                            (toks.get(i + 1), toks.get(i + 2), toks.get(i + 3))
                            && on == "on"
                        {
                            self.pending = Some(Some(ty.clone()));
                            i += 3;
                        }
                    }
                    Tok::Punct('(') => self.stack.push(Frame::Other),
                    Tok::Punct('{') => {
                        let ty = self.pending.take().unwrap_or_else(|| self.schema.query_type.clone());
                        self.stack.push(Frame::Selection(ty));
                        self.last_field = None;
                    }
                    _ => {}
                },
                Some(Frame::Selection(ty)) => match tok {
                    Tok::Name(n) => {
                        if next == Some(&Tok::Punct(':')) {
                            // alias: skip it and the colon.
                            i += 1;
                        } else {
                            self.last_field = Some(n.clone());
                        }
                    }
                    Tok::Spread => {
                        if let (Some(Tok::Name(on)), Some(Tok::Name(t))) = (toks.get(i + 1), toks.get(i + 2)) {
                            if on == "on" {
                                self.pending = Some(Some(t.clone()));
                                i += 2;
                            }
                        } else if toks.get(i + 1) == Some(&Tok::Punct('{')) {
                            self.pending = Some(ty.clone());
                        } else if let Some(Tok::Name(_)) = toks.get(i + 1) {
                            i += 1;
                        }
                        self.last_field = None;
                    }
                    Tok::Punct('@') => {
                        // Directive: skip its name and arguments.
                        if let Some(Tok::Name(_)) = toks.get(i + 1) {
                            i += 1;
                            if toks.get(i + 1) == Some(&Tok::Punct('(')) {
                                self.stack.push(Frame::Other);
                                i += 1;
                            }
                        }
                    }
                    Tok::Punct('(') => match &self.last_field {
                        Some(f) => {
                            self.stack.push(Frame::Args { parent: ty.clone(), field: f.clone(), used: Vec::new() })
                        }
                        None => self.stack.push(Frame::Other),
                    },
                    Tok::Punct('{') => {
                        let inner = match self.pending.take() {
                            Some(t) => t,
                            None => self.last_field.as_ref().and_then(|f| self.field_type(ty.as_deref(), f)),
                        };
                        self.stack.push(Frame::Selection(inner));
                        self.last_field = None;
                    }
                    Tok::Punct('}') => {
                        self.stack.pop();
                        self.last_field = None;
                    }
                    _ => {}
                },
                Some(Frame::Args { parent, field, mut used }) => match tok {
                    Tok::Name(n) if next == Some(&Tok::Punct(':')) => {
                        used.push(n.clone());
                        self.last_arg = Some(n.clone());
                        if let Some(Frame::Args { used: u, .. }) = self.stack.last_mut() {
                            *u = used;
                        }
                        i += 1;
                    }
                    Tok::Punct('{') => {
                        let t = self.last_arg.as_ref().and_then(|a| self.arg_type(parent.as_deref(), &field, a));
                        self.stack.push(Frame::InputObject(t));
                    }
                    Tok::Punct('[') => self.stack.push(Frame::Other),
                    Tok::Punct(')') => {
                        self.stack.pop();
                    }
                    _ => {}
                },
                Some(Frame::InputObject(t)) => match tok {
                    Tok::Name(n) if next == Some(&Tok::Punct(':')) => {
                        self.last_arg = Some(n.clone());
                        i += 1;
                    }
                    Tok::Punct('{') => {
                        let inner = self.last_arg.as_ref().and_then(|a| self.input_field_type(t.as_deref(), a));
                        self.stack.push(Frame::InputObject(inner));
                    }
                    Tok::Punct('[') => self.stack.push(Frame::Other),
                    Tok::Punct('}') => {
                        self.stack.pop();
                    }
                    _ => {}
                },
                Some(Frame::Other) => match tok {
                    Tok::Punct('(') | Tok::Punct('[') | Tok::Punct('{') => self.stack.push(Frame::Other),
                    Tok::Punct(')') | Tok::Punct(']') | Tok::Punct('}') => {
                        self.stack.pop();
                    }
                    _ => {}
                },
            }
            i += 1;
        }
    }
}

fn field_suggestions(schema: &Schema, ty: Option<&str>, out: &mut Vec<Suggestion>) {
    let Some(t) = ty.and_then(|t| schema.get(t)) else { return };
    for f in &t.fields {
        let named = schema.get(f.ty.named());
        let has_selection =
            named.map(|n| matches!(n.kind, TypeKind::Object | TypeKind::Interface | TypeKind::Union)).unwrap_or(false);
        let required_args = f.args.iter().any(|a| a.ty.is_non_null() && a.default_value.is_none());
        let mut insert = f.name.clone();
        if required_args {
            insert.push('(');
        } else if has_selection {
            insert.push_str(" {");
        }
        out.push(Suggestion {
            label: f.name.clone(),
            detail: f.ty.display(),
            insert,
            kind: SuggestionKind::Field,
            deprecated: f.deprecated,
        });
    }
    if matches!(t.kind, TypeKind::Union | TypeKind::Interface) {
        for p in &t.possible_types {
            out.push(Suggestion {
                label: format!("... on {p}"),
                detail: "inline fragment".into(),
                insert: format!("... on {p} {{"),
                kind: SuggestionKind::Fragment,
                deprecated: false,
            });
        }
    }
    out.push(Suggestion {
        label: "__typename".into(),
        detail: "String!".into(),
        insert: "__typename".into(),
        kind: SuggestionKind::Field,
        deprecated: false,
    });
}

/// Suggestions valid at byte offset `cursor` of `query`.
pub fn suggest(schema: &Schema, query: &str, cursor: usize) -> Vec<Suggestion> {
    let mut cursor = cursor.min(query.len());
    while !query.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let before = &query[..cursor];
    let prefix_len = before.chars().rev().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').count();
    let prefix = &before[before.len() - prefix_len..];
    let context = &before[..before.len() - prefix_len];
    // Inside a string or comment: nothing to suggest.
    if let Some(line) = context.rsplit('\n').next()
        && line.contains('#')
    {
        return Vec::new();
    }
    let toks = tokenize(context);
    let mut w = Walker { schema, stack: Vec::new(), pending: None, last_field: None, last_arg: None };
    w.run(&toks);
    let mut out = Vec::new();
    let after_at = context.trim_end().ends_with('@');
    match w.stack.last() {
        _ if after_at => {
            for d in &schema.directives {
                out.push(Suggestion {
                    label: d.name.clone(),
                    detail: d.locations.join(", "),
                    insert: if d.args.is_empty() { d.name.clone() } else { format!("{}(", d.name) },
                    kind: SuggestionKind::Directive,
                    deprecated: false,
                });
            }
        }
        None => {
            for kw in ["query", "mutation", "subscription", "fragment"] {
                let available = match kw {
                    "mutation" => schema.mutation_type.is_some(),
                    "subscription" => schema.subscription_type.is_some(),
                    _ => true,
                };
                if available {
                    out.push(Suggestion {
                        label: kw.into(),
                        detail: "keyword".into(),
                        insert: format!("{kw} "),
                        kind: SuggestionKind::Keyword,
                        deprecated: false,
                    });
                }
            }
        }
        Some(Frame::Selection(ty)) => field_suggestions(schema, ty.as_deref(), &mut out),
        Some(Frame::Args { parent, field, used }) => {
            // After `name:` suggest values (enums, booleans); else argument names.
            let trimmed = context.trim_end();
            if trimmed.ends_with(':') {
                let arg = w.last_arg.clone().unwrap_or_default();
                if let Some(t) = w.arg_type(parent.as_deref(), field, &arg) {
                    value_suggestions(schema, &t, &mut out);
                }
            } else if let Some(f) = parent.as_deref().and_then(|p| schema.get(p)).and_then(|p| p.field(field)) {
                for a in f.args.iter().filter(|a| !used.contains(&a.name)) {
                    out.push(Suggestion {
                        label: a.name.clone(),
                        detail: a.ty.display(),
                        insert: format!("{}: ", a.name),
                        kind: SuggestionKind::Argument,
                        deprecated: false,
                    });
                }
            }
        }
        Some(Frame::InputObject(t)) => {
            let trimmed = context.trim_end();
            if trimmed.ends_with(':') {
                let name = w.last_arg.clone().unwrap_or_default();
                if let Some(ft) = w.input_field_type(t.as_deref(), &name) {
                    value_suggestions(schema, &ft, &mut out);
                }
            } else if let Some(def) = t.as_deref().and_then(|t| schema.get(t)) {
                for f in &def.input_fields {
                    out.push(Suggestion {
                        label: f.name.clone(),
                        detail: f.ty.display(),
                        insert: format!("{}: ", f.name),
                        kind: SuggestionKind::InputField,
                        deprecated: false,
                    });
                }
            }
        }
        Some(Frame::Other) => {}
    }
    if !prefix.is_empty() {
        let p = prefix.to_ascii_lowercase();
        out.retain(|s| {
            s.label.to_ascii_lowercase().starts_with(&p) || s.label.starts_with("... on") && "on".starts_with(&p)
        });
        // Exact-prefix (case-sensitive) matches first, keep schema order otherwise.
        out.sort_by_key(|s| !s.label.starts_with(prefix));
    }
    out
}

fn value_suggestions(schema: &Schema, ty: &str, out: &mut Vec<Suggestion>) {
    match schema.get(ty) {
        Some(t) if t.kind == TypeKind::Enum => {
            for e in &t.enum_values {
                out.push(Suggestion {
                    label: e.name.clone(),
                    detail: t.name.clone(),
                    insert: e.name.clone(),
                    kind: SuggestionKind::EnumValue,
                    deprecated: e.deprecated,
                });
            }
        }
        _ if ty == "Boolean" => {
            for b in ["true", "false"] {
                out.push(Suggestion {
                    label: b.into(),
                    detail: "Boolean".into(),
                    insert: b.into(),
                    kind: SuggestionKind::EnumValue,
                    deprecated: false,
                });
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Skeletons
// ---------------------------------------------------------------------------

fn leaf(schema: &Schema, name: &str) -> bool {
    schema.get(name).map(|t| matches!(t.kind, TypeKind::Scalar | TypeKind::Enum)).unwrap_or(true)
}

fn selection(schema: &Schema, ty: &str, depth: usize, max_depth: usize, indent: usize, out: &mut String) {
    let Some(t) = schema.get(ty) else { return };
    let pad = "  ".repeat(indent);
    match t.kind {
        TypeKind::Union => {
            out.push_str(&format!("{pad}__typename\n"));
            // Members are objects in a valid schema; a union listing unions
            // (itself, say) must not recurse past the depth limit.
            if depth > max_depth {
                return;
            }
            for p in &t.possible_types {
                out.push_str(&format!("{pad}... on {p} {{\n"));
                selection(schema, p, depth + 1, max_depth, indent + 1, out);
                out.push_str(&format!("{pad}}}\n"));
            }
        }
        TypeKind::Object | TypeKind::Interface => {
            let mut any = false;
            for f in t.fields.iter().filter(|f| !f.deprecated && !f.name.starts_with("__")) {
                if f.args.iter().any(|a| a.ty.is_non_null() && a.default_value.is_none()) {
                    continue;
                }
                let named = f.ty.named();
                if leaf(schema, named) {
                    out.push_str(&format!("{pad}{}\n", f.name));
                    any = true;
                } else if depth < max_depth {
                    let mut inner = String::new();
                    selection(schema, named, depth + 1, max_depth, indent + 1, &mut inner);
                    if !inner.is_empty() {
                        out.push_str(&format!("{pad}{} {{\n{inner}{pad}}}\n", f.name));
                        any = true;
                    }
                }
            }
            if !any {
                out.push_str(&format!("{pad}__typename\n"));
            }
        }
        _ => {}
    }
}

/// A ready-to-run operation for a root field: every argument becomes a
/// variable and object results get a selection of their scalar fields (two
/// levels deep). `root` is `query`, `mutation`, `subscription` or a type name.
pub fn skeleton(schema: &Schema, root: &str, field: &str) -> String {
    let (op, root_type) = match schema.root(root) {
        Some(t) => (root.to_ascii_lowercase(), t.to_string()),
        None => {
            let op = if Some(root) == schema.mutation_type.as_deref() {
                "mutation"
            } else if Some(root) == schema.subscription_type.as_deref() {
                "subscription"
            } else {
                "query"
            };
            (op.to_string(), root.to_string())
        }
    };
    let Some(f) = schema.get(&root_type).and_then(|t| t.field(field)) else {
        return format!("{op} {{\n  {field}\n}}\n");
    };
    let mut name: String = field.chars().take(1).flat_map(char::to_uppercase).collect();
    name.push_str(&field.chars().skip(1).collect::<String>());
    let vars: Vec<String> = f.args.iter().map(|a| format!("${}: {}", a.name, a.ty.display())).collect();
    let args: Vec<String> = f.args.iter().map(|a| format!("{}: ${}", a.name, a.name)).collect();
    let mut out = format!("{op} {name}");
    if !vars.is_empty() {
        out.push_str(&format!("({})", vars.join(", ")));
    }
    out.push_str(" {\n  ");
    out.push_str(field);
    if !args.is_empty() {
        out.push_str(&format!("({})", args.join(", ")));
    }
    let named = f.ty.named();
    if leaf(schema, named) {
        out.push('\n');
    } else {
        let mut inner = String::new();
        selection(schema, named, 1, 2, 2, &mut inner);
        out.push_str(&format!(" {{\n{inner}  }}\n"));
    }
    out.push_str("}\n");
    out
}

/// Default variables JSON for a skeleton (`{"id": null}`), so the variables
/// editor can be pre-filled.
pub fn skeleton_variables(schema: &Schema, root: &str, field: &str) -> String {
    let root_type = schema.root(root).unwrap_or(root);
    let Some(f) = schema.get(root_type).and_then(|t| t.field(field)) else { return "{}".into() };
    let mut map = serde_json::Map::new();
    for a in &f.args {
        let v = match a.default_value.as_deref().map(serde_json::from_str::<Value>) {
            Some(Ok(v)) => v,
            _ => match a.ty.named() {
                "Int" | "Float" => Value::from(0),
                "Boolean" => Value::Bool(false),
                "String" | "ID" => Value::String(String::new()),
                other => match schema.get(other) {
                    Some(t) if t.kind == TypeKind::Enum => {
                        t.enum_values.first().map(|e| Value::String(e.name.clone())).unwrap_or(Value::Null)
                    }
                    Some(t) if t.kind == TypeKind::InputObject => Value::Object(Default::default()),
                    _ => Value::Null,
                },
            },
        };
        map.insert(a.name.clone(), v);
    }
    serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        let v: Value = serde_json::from_str(include_str!("../testdata/github_introspection.json")).unwrap();
        parse_introspection(&v).unwrap()
    }

    fn labels(s: &[Suggestion]) -> Vec<&str> {
        s.iter().map(|x| x.label.as_str()).collect()
    }

    #[test]
    fn skeleton_of_a_self_containing_union_terminates() {
        // Invalid, but introspection results come from arbitrary servers.
        let v = serde_json::json!({"__schema": {
            "queryType": {"name": "Query"},
            "types": [
                {"kind": "OBJECT", "name": "Query", "fields": [
                    {"name": "u", "args": [], "type": {"kind": "UNION", "name": "U"}}]},
                {"kind": "UNION", "name": "U", "possibleTypes": [{"name": "U"}, {"name": "V"}]},
                {"kind": "UNION", "name": "V", "possibleTypes": [{"name": "U"}]}
            ]
        }});
        let s = parse_introspection(&v).unwrap();
        let q = skeleton(&s, "query", "u");
        assert!(q.starts_with("query U {\n  u {\n    __typename\n"), "{q}");
    }

    #[test]
    fn parses_types() {
        let s = schema();
        assert_eq!(s.query_type.as_deref(), Some("Query"));
        assert_eq!(s.mutation_type.as_deref(), Some("Mutation"));
        let repo = s.get("Query").unwrap().field("repository").unwrap();
        assert_eq!(repo.args.len(), 2);
        assert_eq!(repo.args[0].ty.display(), "String!");
        let search = s.get("Query").unwrap().field("search").unwrap();
        assert_eq!(search.ty.display(), "SearchResultItemConnection!");
        assert_eq!(s.get("SearchResultItem").unwrap().possible_types, vec!["Repository", "User"]);
        assert_eq!(s.get("SearchType").unwrap().enum_values.len(), 2);
        assert!(s.user_types().iter().all(|t| !t.name.starts_with("__")));
        assert!(parse_introspection(&serde_json::json!({"errors": [{"message": "nope"}]})).is_err());
    }

    #[test]
    fn suggests_fields_in_context() {
        let s = schema();
        let q = "query { viewer { lo";
        let out = suggest(&s, q, q.len());
        assert_eq!(labels(&out), vec!["login"]);

        let q = "{ repository(owner: \"a\", name: \"b\") { owner { ";
        let out = suggest(&s, q, q.len());
        assert!(labels(&out).contains(&"login"), "{:?}", labels(&out));

        let q = "{ viewer { repositories(first: 3) { nodes { ";
        let out = suggest(&s, q, q.len());
        assert!(labels(&out).contains(&"stargazerCount"));
        // Required args get `(`, objects get ` {`.
        let issues = out.iter().find(|x| x.label == "issues").unwrap();
        assert_eq!(issues.insert, "issues(");
        let owner = out.iter().find(|x| x.label == "owner").unwrap();
        assert_eq!(owner.insert, "owner {");

        let q = "{ me: viewer { name } search(query: \"x\", type: USER) { nodes { ";
        let out = suggest(&s, q, q.len());
        assert!(labels(&out).contains(&"... on Repository"), "{:?}", labels(&out));
    }

    #[test]
    fn suggests_args_and_values() {
        let s = schema();
        let q = "{ repository(owner: \"x\", ";
        let out = suggest(&s, q, q.len());
        assert_eq!(labels(&out), vec!["name"]);
        let q = "{ search(query: \"x\", type: ";
        let out = suggest(&s, q, q.len());
        assert_eq!(labels(&out), vec!["REPOSITORY", "USER"]);
        let q = "mutation { addStar(input: { st";
        let out = suggest(&s, q, q.len());
        assert_eq!(labels(&out), vec!["starrableId"]);
    }

    #[test]
    fn suggests_keywords_and_skips_strings() {
        let s = schema();
        let out = suggest(&s, "mu", 2);
        assert_eq!(labels(&out), vec!["mutation"]);
        let q = "{ viewer { # comment lo";
        assert!(suggest(&s, q, q.len()).is_empty());
        let q = "fragment F on Repository { na";
        assert_eq!(labels(&suggest(&s, q, q.len())), vec!["name"]);
        let q = "{ viewer { ...on User { log";
        assert_eq!(labels(&suggest(&s, q, q.len())), vec!["login"]);
    }

    #[test]
    fn skeletons() {
        let s = schema();
        let q = skeleton(&s, "query", "repository");
        assert_eq!(
            q,
            "query Repository($owner: String!, $name: String!) {\n  repository(owner: $owner, name: $name) {\n    name\n    stargazerCount\n    owner {\n      login\n    }\n  }\n}\n"
        );
        let m = skeleton(&s, "mutation", "addStar");
        assert!(m.starts_with(
            "mutation AddStar($input: AddStarInput!) {\n  addStar(input: $input) {\n    clientMutationId\n"
        ));
        let search = skeleton(&s, "query", "search");
        assert!(search.contains("... on User {"), "{search}");
        let vars: Value = serde_json::from_str(&skeleton_variables(&s, "query", "search")).unwrap();
        assert_eq!(vars["type"], "REPOSITORY");
        assert_eq!(vars["first"], 0);
    }
}
