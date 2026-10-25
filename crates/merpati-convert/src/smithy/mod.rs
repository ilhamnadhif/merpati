//! Smithy import (FR-D1): the IDL 2.0 (and most of 1.0) and the JSON AST
//! (`model.json`), validation with line/column diagnostics, and conversion of
//! HTTP-bound operations to a Merpati [`Collection`].
//!
//! ```text
//! parse / parse_idl / parse_json_ast   text -> Model (syntax error = Err(Diagnostic))
//! Model::validate                      semantic diagnostics (unknown targets, @http, labels, ...)
//! Model::to_collection                 Collection + warnings
//! check                                parse + validate, for live validation in an editor
//! import                               like `openapi::import`: text -> (Collection, warnings)
//!
//! SourceFile::parse                    one file of a multi-file model, parsed but not resolved
//! assemble / parse_files               several files -> one Model
//! check_files / check_sources          multi-file `check`; diagnostics carry the file name
//! import_files / import_sources        multi-file `import`
//! related_files                        the files of a folder that form one model with a given file
//! build_config_paths                   `sources`/`imports` of a smithy-build.json
//! resolve_maven_dependencies           models of its `maven.dependencies` from jars in ~/.m2 (no download)
//! ```
//!
//! A model can be split over several IDL and JSON AST files: each IDL file
//! has its own namespace and `use` statements, shapes may reference shapes
//! of other files, `apply` works across files, metadata is merged (arrays
//! are concatenated, equal values kept once, anything else is a conflict)
//! and a shape defined in several files must be the same shape every time
//! (its traits are then merged like `apply`). References to shapes outside
//! the given files (other than the prelude and well-known AWS/alloy protocol
//! traits) are reported as unknown targets, unless the files of the Maven
//! dependencies that define them are assembled too.

mod convert;
mod idl;
mod json;
mod maven;
mod resolve;
mod validate;

pub use maven::{
    MavenCoordinate, MavenModels, build_config_maven_dependencies, default_maven_repository, jar_model_files,
    resolve_maven_dependencies,
};

use std::collections::{HashMap, HashSet};
use std::fmt;

use indexmap::IndexMap;
use merpati_core::model::Collection;
use serde_json::{Map, Value};

use crate::ConvertError;

/// 1-based position in the source text. `line == 0` means "unknown".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    /// Index of the source file in [`Model::files`] (0 for a single text).
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

impl Pos {
    pub const fn new(line: u32, column: u32) -> Self {
        Pos { file: 0, line, column }
    }

    pub fn is_known(self) -> bool {
        self.line > 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Error,
    Warning,
}

/// A syntax or validation finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub pos: Pos,
    pub message: String,
    /// Absolute id of the shape the finding is about, when there is one.
    pub shape: Option<String>,
    /// Name of the source file (multi-file models only).
    pub file: Option<String>,
}

impl Diagnostic {
    pub fn error(pos: Pos, message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Error, pos, message: message.into(), shape: None, file: None }
    }

    pub fn warning(pos: Pos, message: impl Into<String>) -> Self {
        Diagnostic { severity: Severity::Warning, pos, message: message.into(), shape: None, file: None }
    }

    fn on(mut self, shape: &str) -> Self {
        self.shape = Some(shape.to_string());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    /// `12:5: error: message`, or `main.smithy:12:5: error: message` in a
    /// multi-file model.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        if let Some(file) = &self.file {
            write!(f, "{file}:")?;
        }
        if self.pos.is_known() {
            write!(f, "{}:{}: {sev}: {}", self.pos.line, self.pos.column, self.message)
        } else {
            write!(f, "{sev}: {}", self.message)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShapeKind {
    Blob,
    Boolean,
    Document,
    String,
    Byte,
    Short,
    Integer,
    Long,
    Float,
    Double,
    BigInteger,
    BigDecimal,
    Timestamp,
    Enum,
    IntEnum,
    List,
    /// IDL 1.0 `set` (a list with unique items).
    Set,
    Map,
    Structure,
    Union,
    Service,
    Resource,
    Operation,
}

impl ShapeKind {
    /// The keyword used in the IDL and the `type` of the JSON AST.
    pub fn keyword(self) -> &'static str {
        match self {
            ShapeKind::Blob => "blob",
            ShapeKind::Boolean => "boolean",
            ShapeKind::Document => "document",
            ShapeKind::String => "string",
            ShapeKind::Byte => "byte",
            ShapeKind::Short => "short",
            ShapeKind::Integer => "integer",
            ShapeKind::Long => "long",
            ShapeKind::Float => "float",
            ShapeKind::Double => "double",
            ShapeKind::BigInteger => "bigInteger",
            ShapeKind::BigDecimal => "bigDecimal",
            ShapeKind::Timestamp => "timestamp",
            ShapeKind::Enum => "enum",
            ShapeKind::IntEnum => "intEnum",
            ShapeKind::List => "list",
            ShapeKind::Set => "set",
            ShapeKind::Map => "map",
            ShapeKind::Structure => "structure",
            ShapeKind::Union => "union",
            ShapeKind::Service => "service",
            ShapeKind::Resource => "resource",
            ShapeKind::Operation => "operation",
        }
    }

    pub fn from_keyword(s: &str) -> Option<ShapeKind> {
        Some(match s {
            "blob" => ShapeKind::Blob,
            "boolean" => ShapeKind::Boolean,
            "document" => ShapeKind::Document,
            "string" => ShapeKind::String,
            "byte" => ShapeKind::Byte,
            "short" => ShapeKind::Short,
            "integer" => ShapeKind::Integer,
            "long" => ShapeKind::Long,
            "float" => ShapeKind::Float,
            "double" => ShapeKind::Double,
            "bigInteger" => ShapeKind::BigInteger,
            "bigDecimal" => ShapeKind::BigDecimal,
            "timestamp" => ShapeKind::Timestamp,
            "enum" => ShapeKind::Enum,
            "intEnum" => ShapeKind::IntEnum,
            "list" => ShapeKind::List,
            "set" => ShapeKind::Set,
            "map" => ShapeKind::Map,
            "structure" => ShapeKind::Structure,
            "union" => ShapeKind::Union,
            "service" => ShapeKind::Service,
            "resource" => ShapeKind::Resource,
            "operation" => ShapeKind::Operation,
            _ => return None,
        })
    }

    pub fn is_simple(self) -> bool {
        !matches!(
            self,
            ShapeKind::List
                | ShapeKind::Set
                | ShapeKind::Map
                | ShapeKind::Structure
                | ShapeKind::Union
                | ShapeKind::Service
                | ShapeKind::Resource
                | ShapeKind::Operation
        )
    }
}

/// Traits keyed by absolute shape id (`smithy.api#http`).
pub type Traits = IndexMap<String, Value>;

/// Looks up a trait by name: `http` means `smithy.api#http`; an absolute id
/// is matched exactly. Traits from other namespaces are also found by their
/// local name (`restJson1` finds `aws.protocols#restJson1`).
pub fn find_trait<'a>(traits: &'a Traits, name: &str) -> Option<&'a Value> {
    if name.contains('#') {
        return traits.get(name);
    }
    if let Some(v) = traits.get(&format!("smithy.api#{name}")) {
        return Some(v);
    }
    traits.iter().find(|(k, _)| local_name(k) == name).map(|(_, v)| v)
}

/// `ns#Name$member` -> `Name`.
pub fn local_name(id: &str) -> &str {
    let after = id.rsplit_once('#').map(|(_, n)| n).unwrap_or(id);
    after.split('$').next().unwrap_or(after)
}

/// `ns#Name` -> `ns`.
pub fn namespace_of(id: &str) -> &str {
    id.split_once('#').map(|(ns, _)| ns).unwrap_or("")
}

/// A member of a structure, union, enum, list (`member`) or map (`key`,
/// `value`).
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub name: String,
    /// Absolute id of the target shape. Enum members target `smithy.api#Unit`.
    pub target: String,
    pub traits: Traits,
    /// Where each trait was written (IDL only; the JSON AST has no positions).
    pub trait_pos: IndexMap<String, Pos>,
    pub pos: Pos,
}

impl Member {
    pub fn get_trait(&self, name: &str) -> Option<&Value> {
        find_trait(&self.traits, name)
    }

    pub fn has_trait(&self, name: &str) -> bool {
        self.get_trait(name).is_some()
    }

    /// Documentation from `///` comments or `@documentation`.
    pub fn docs(&self) -> Option<&str> {
        self.get_trait("documentation").and_then(Value::as_str)
    }

    pub fn trait_position(&self, name: &str) -> Pos {
        trait_position(&self.trait_pos, name).unwrap_or(self.pos)
    }
}

fn trait_position(map: &IndexMap<String, Pos>, name: &str) -> Option<Pos> {
    if name.contains('#') {
        return map.get(name).copied();
    }
    map.get(&format!("smithy.api#{name}"))
        .or_else(|| map.iter().find(|(k, _)| local_name(k) == name).map(|(_, p)| p))
        .copied()
}

/// A reference to another shape from a service, resource or operation, with
/// the position it was written at (for diagnostics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapeRef {
    pub id: String,
    pub pos: Pos,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    /// Absolute id (`example.weather#GetCity`).
    pub id: String,
    pub kind: ShapeKind,
    pub pos: Pos,
    pub traits: Traits,
    /// Where each trait was written (IDL only).
    pub trait_pos: IndexMap<String, Pos>,
    pub mixins: Vec<ShapeRef>,
    /// Members in declaration order, after mixins were applied (mixin members
    /// first).
    pub members: IndexMap<String, Member>,
    // -- operation --
    pub input: Option<ShapeRef>,
    pub output: Option<ShapeRef>,
    pub errors: Vec<ShapeRef>,
    // -- service --
    pub version: Option<String>,
    pub operations: Vec<ShapeRef>,
    pub resources: Vec<ShapeRef>,
    // -- resource --
    pub identifiers: IndexMap<String, ShapeRef>,
    pub properties: IndexMap<String, ShapeRef>,
    /// `create`, `put`, `read`, `update`, `delete`, `list` in that order.
    pub lifecycle: IndexMap<String, ShapeRef>,
    pub collection_operations: Vec<ShapeRef>,
    /// `structure X for Resource` / `input := for Resource`.
    pub for_resource: Option<ShapeRef>,
}

impl Shape {
    pub fn new(id: impl Into<String>, kind: ShapeKind, pos: Pos) -> Self {
        Shape {
            id: id.into(),
            kind,
            pos,
            traits: Traits::new(),
            trait_pos: IndexMap::new(),
            mixins: Vec::new(),
            members: IndexMap::new(),
            input: None,
            output: None,
            errors: Vec::new(),
            version: None,
            operations: Vec::new(),
            resources: Vec::new(),
            identifiers: IndexMap::new(),
            properties: IndexMap::new(),
            lifecycle: IndexMap::new(),
            collection_operations: Vec::new(),
            for_resource: None,
        }
    }

    pub fn name(&self) -> &str {
        local_name(&self.id)
    }

    pub fn get_trait(&self, name: &str) -> Option<&Value> {
        find_trait(&self.traits, name)
    }

    pub fn has_trait(&self, name: &str) -> bool {
        self.get_trait(name).is_some()
    }

    pub fn docs(&self) -> Option<&str> {
        self.get_trait("documentation").and_then(Value::as_str)
    }

    /// Position of a trait (`@http`), falling back to the shape's position.
    pub fn trait_position(&self, name: &str) -> Pos {
        trait_position(&self.trait_pos, name).unwrap_or(self.pos)
    }

    /// Every outgoing shape reference except member targets.
    pub(crate) fn refs(&self) -> impl Iterator<Item = (&'static str, &ShapeRef)> {
        let single = [("input", &self.input), ("output", &self.output), ("resource", &self.for_resource)];
        single
            .into_iter()
            .filter_map(|(what, r)| r.as_ref().map(|r| (what, r)))
            .chain(self.mixins.iter().map(|r| ("mixin", r)))
            .chain(self.errors.iter().map(|r| ("error", r)))
            .chain(self.operations.iter().map(|r| ("operation", r)))
            .chain(self.resources.iter().map(|r| ("resource", r)))
            .chain(self.collection_operations.iter().map(|r| ("operation", r)))
            .chain(self.lifecycle.values().map(|r| ("operation", r)))
            .chain(self.identifiers.values().map(|r| ("identifier", r)))
            .chain(self.properties.values().map(|r| ("property", r)))
    }

    /// Visits every position of the shape (its own, its traits', its
    /// members' and its references').
    pub(crate) fn for_each_pos(&mut self, f: &mut impl FnMut(&mut Pos)) {
        f(&mut self.pos);
        self.trait_pos.values_mut().for_each(&mut *f);
        for m in self.members.values_mut() {
            f(&mut m.pos);
            m.trait_pos.values_mut().for_each(&mut *f);
        }
        let single = [&mut self.input, &mut self.output, &mut self.for_resource];
        for r in single.into_iter().flatten() {
            f(&mut r.pos);
        }
        let lists = [
            &mut self.mixins,
            &mut self.errors,
            &mut self.operations,
            &mut self.resources,
            &mut self.collection_operations,
        ];
        for r in lists.into_iter().flatten() {
            f(&mut r.pos);
        }
        for map in [&mut self.identifiers, &mut self.properties, &mut self.lifecycle] {
            for r in map.values_mut() {
                f(&mut r.pos);
            }
        }
    }
}

/// A parsed and resolved Smithy model (one or several files).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    /// `$version` (`"2.0"`, `"1.0"`), or `smithy` of the JSON AST; the
    /// highest one of a multi-file model.
    pub version: String,
    /// Metadata of every file, merged.
    pub metadata: Map<String, Value>,
    /// The namespace of an IDL file (the first one of a JSON AST). In a
    /// multi-file model: the namespace of the first service, else of the first
    /// file.
    pub namespace: String,
    /// Names of the source files, indexed by [`Pos::file`]; empty for a model
    /// parsed from a single text.
    pub files: Vec<String>,
    /// Shapes by absolute id, in declaration order.
    pub shapes: IndexMap<String, Shape>,
    /// Non-fatal findings of parsing and resolution (unknown control
    /// statements, doc comments in odd places, ...).
    pub parse_diagnostics: Vec<Diagnostic>,
}

impl Model {
    pub fn shape(&self, id: &str) -> Option<&Shape> {
        self.shapes.get(id)
    }

    pub fn services(&self) -> impl Iterator<Item = &Shape> {
        self.shapes.values().filter(|s| s.kind == ShapeKind::Service)
    }

    /// Name of the file a position is in (`None` for a single-text model).
    pub fn file_name(&self, pos: Pos) -> Option<&str> {
        self.files.get(pos.file as usize).map(String::as_str).filter(|n| !n.is_empty())
    }

    /// Parse-time diagnostics plus semantic validation, sorted by file and
    /// position.
    pub fn validate(&self) -> Vec<Diagnostic> {
        let mut out = self.parse_diagnostics.clone();
        out.extend(validate::validate(self));
        out.sort_by_key(|a| (a.pos, a.severity));
        out.dedup();
        for d in &mut out {
            d.file = self.file_name(d.pos).map(str::to_string);
        }
        out
    }

    /// Converts the HTTP-bound operations of the model's services.
    pub fn to_collection(&self) -> (Collection, Vec<String>) {
        convert::to_collection(self)
    }
}

/// Whether a parsed JSON document is a Smithy JSON AST (`"smithy": "2.0"`).
pub fn is_json_ast(v: &Value) -> bool {
    json::is_ast(v)
}

/// Whether the text looks like a Smithy IDL file or JSON AST.
pub fn looks_like_smithy(text: &str) -> bool {
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with('{') {
        return json::looks_like_ast(t);
    }
    idl::looks_like_idl(t)
}

/// Parses the IDL or the JSON AST (auto-detected). Syntax errors (and models
/// that cannot be resolved at all) are returned as an error diagnostic.
pub fn parse(text: &str) -> Result<Model, Diagnostic> {
    Ok(resolve::finish(parse_raw(text)?))
}

fn parse_raw(text: &str) -> Result<resolve::RawModel, Diagnostic> {
    let t = text.trim_start_matches('\u{feff}');
    if t.trim_start().starts_with('{') { json::parse(t) } else { idl::parse(t) }
}

/// Parses Smithy IDL (2.0, and the 1.0 constructs still found in the wild).
pub fn parse_idl(text: &str) -> Result<Model, Diagnostic> {
    let raw = idl::parse(text)?;
    Ok(resolve::finish(raw))
}

/// Parses a Smithy JSON AST (`model.json`).
pub fn parse_json_ast(text: &str) -> Result<Model, Diagnostic> {
    let raw = json::parse(text)?;
    Ok(resolve::finish(raw))
}

/// One file of a multi-file model: parsed (IDL or JSON AST, auto-detected)
/// but not resolved yet, so it can be parsed once and assembled with
/// different sets of files (see [`assemble`], [`related_files`]).
#[derive(Clone, Debug)]
pub struct SourceFile {
    /// Name used in diagnostics, usually a (relative) path.
    pub name: String,
    parsed: Result<resolve::RawModel, Diagnostic>,
    /// Absolute ids of the shapes the file defines.
    defines: HashSet<String>,
    /// Absolute ids the file refers to but does not define (approximate:
    /// relative names are taken to be in the file's namespace).
    needs: HashSet<String>,
}

impl SourceFile {
    pub fn parse(name: impl Into<String>, text: &str) -> SourceFile {
        SourceFile::new(name.into(), parse_raw(text))
    }

    fn new(name: String, parsed: Result<resolve::RawModel, Diagnostic>) -> SourceFile {
        let parsed = parsed.map_err(|mut d| {
            d.file = Some(name.clone()).filter(|n| !n.is_empty());
            d
        });
        let (defines, needs) = parsed.as_ref().map(resolve::links).unwrap_or_default();
        SourceFile { name, parsed, defines, needs }
    }

    /// Like [`SourceFile::parse`], but `None` when the text is neither Smithy
    /// IDL nor a Smithy JSON AST (valid JSON without `"smithy"`, or invalid
    /// JSON), for scanning a folder that also holds other specs.
    pub fn detect(name: impl Into<String>, text: &str) -> Option<SourceFile> {
        let t = text.trim_start_matches('\u{feff}');
        if !t.trim_start().starts_with('{') {
            return idl::looks_like_idl(t.trim_start()).then(|| SourceFile::parse(name, text));
        }
        let v: Value = serde_json::from_str(t).ok()?;
        json::is_ast(&v).then(|| SourceFile::new(name.into(), json::parse_value(t, &v)))
    }

    /// The syntax error that keeps the file out of a model.
    pub fn syntax_error(&self) -> Option<&Diagnostic> {
        self.parsed.as_ref().err()
    }

    /// The IDL namespace (the first one of a JSON AST); `None` after a
    /// syntax error.
    pub fn namespace(&self) -> Option<&str> {
        self.parsed.as_ref().ok().map(|r| r.namespace.as_str())
    }

    /// Whether the file only holds metadata (or nothing): no shapes, no
    /// `apply`.
    fn is_metadata_only(&self) -> bool {
        self.parsed.as_ref().is_ok_and(|r| r.shapes.is_empty() && r.applies.is_empty())
    }
}

/// Resolves several files into one model. Fails with the syntax errors of
/// every file that has one.
pub fn assemble(files: &[&SourceFile]) -> Result<Model, Vec<Diagnostic>> {
    let errors: Vec<Diagnostic> = files.iter().filter_map(|f| f.syntax_error().cloned()).collect();
    if !errors.is_empty() {
        return Err(errors);
    }
    let raws = files.iter().filter_map(|f| f.parsed.as_ref().ok().cloned()).collect();
    Ok(resolve::assemble(raws, files.iter().map(|f| f.name.clone()).collect()))
}

/// Parses `(name, text)` pairs and resolves them into one model.
pub fn parse_files(files: &[(impl AsRef<str>, impl AsRef<str>)]) -> Result<Model, Vec<Diagnostic>> {
    let sources = sources(files);
    assemble(&sources.iter().collect::<Vec<_>>())
}

fn sources(files: &[(impl AsRef<str>, impl AsRef<str>)]) -> Vec<SourceFile> {
    files.iter().map(|(name, text)| SourceFile::parse(name.as_ref(), text.as_ref())).collect()
}

/// The model paths of a `smithy-build.json`: its `sources` and `imports`
/// (files or directories, relative to the file's directory), in order.
/// Plugins and projections are not used; the models of its Maven
/// dependencies come from [`resolve_maven_dependencies`].
pub fn build_config_paths(json: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("invalid smithy-build.json: {e}"))?;
    let mut out = Vec::new();
    for key in ["sources", "imports"] {
        match v.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Array(items)) => {
                for item in items {
                    let path = item.as_str().ok_or_else(|| format!("smithy-build.json: `{key}` must list strings"))?;
                    if !out.iter().any(|p| p == path) {
                        out.push(path.to_string());
                    }
                }
            }
            Some(_) => return Err(format!("smithy-build.json: `{key}` must be an array")),
        }
    }
    Ok(out)
}

/// The files that form one model with `files[start]`: those connected to it
/// by shape references in either direction (a file defines a shape another
/// one uses, applies traits to or imports), plus files that only hold
/// metadata. Files with a syntax error are left out (their references are
/// unknown) unless they are `start`. Sorted, `start` included.
///
/// Lets an editor validate a file together with the rest of its folder
/// without merging unrelated models that happen to live next to it.
pub fn related_files(files: &[&SourceFile], start: usize) -> Vec<usize> {
    if start >= files.len() {
        return Vec::new();
    }
    if files[start].syntax_error().is_some() {
        return vec![start];
    }
    let mut definers: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        for id in &f.defines {
            definers.entry(id.as_str()).or_default().push(i);
        }
    }
    // Undirected edges: user -> definer and back.
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); files.len()];
    for (i, f) in files.iter().enumerate() {
        for id in &f.needs {
            for &j in definers.get(id.as_str()).into_iter().flatten() {
                if j != i {
                    edges[i].push(j);
                    edges[j].push(i);
                }
            }
        }
    }
    let mut seen = vec![false; files.len()];
    let mut stack = vec![start];
    seen[start] = true;
    while let Some(i) = stack.pop() {
        for &j in &edges[i] {
            if !seen[j] && files[j].syntax_error().is_none() {
                seen[j] = true;
                stack.push(j);
            }
        }
    }
    (0..files.len()).filter(|&i| seen[i] || files[i].is_metadata_only()).collect()
}

/// Result of [`check`].
#[derive(Clone, Debug, Default)]
pub struct Checked {
    /// `None` when the text (or one of the files) has a syntax error.
    pub model: Option<Model>,
    /// The syntax errors, or every validation finding, sorted by file and
    /// position.
    pub diagnostics: Vec<Diagnostic>,
}

impl Checked {
    pub fn errors(&self) -> usize {
        self.diagnostics.iter().filter(|d| d.is_error()).count()
    }

    /// The findings in one file of a multi-file model.
    pub fn in_file<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Diagnostic> + 'a {
        self.diagnostics.iter().filter(move |d| d.file.as_deref() == Some(name))
    }

    /// One-line status for an editor ("Smithy 2.0: 1 service, 7 operations";
    /// "..., 7 operations in 3 files" for a multi-file model).
    pub fn summary(&self) -> String {
        let Some(m) = &self.model else {
            return self.diagnostics.first().map(ToString::to_string).unwrap_or_else(|| "invalid Smithy model".into());
        };
        let services = m.services().count();
        let ops = m.shapes.values().filter(|s| s.kind == ShapeKind::Operation).count();
        let version = if m.version.is_empty() { "Smithy".to_string() } else { format!("Smithy {}", m.version) };
        let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        let mut s = format!("{version}: {}, {}", plural(services, "service"), plural(ops, "operation"));
        if m.files.len() > 1 {
            s.push_str(&format!(" in {}", plural(m.files.len(), "file")));
        }
        let errors = self.errors();
        if errors > 0 {
            s.push_str(&format!(", {}", plural(errors, "error")));
        }
        s
    }
}

/// Parses and validates; never fails. For live validation in the Spec Hub.
pub fn check(text: &str) -> Checked {
    match parse(text) {
        Ok(model) => {
            let diagnostics = model.validate();
            Checked { model: Some(model), diagnostics }
        }
        Err(d) => Checked { model: None, diagnostics: vec![d] },
    }
}

/// [`check`] for a model split over several `(name, text)` files.
pub fn check_files(files: &[(impl AsRef<str>, impl AsRef<str>)]) -> Checked {
    let sources = sources(files);
    check_sources(&sources.iter().collect::<Vec<_>>())
}

/// [`check`] for already parsed files.
pub fn check_sources(files: &[&SourceFile]) -> Checked {
    match assemble(files) {
        Ok(model) => {
            let diagnostics = model.validate();
            Checked { model: Some(model), diagnostics }
        }
        Err(diagnostics) => Checked { model: None, diagnostics },
    }
}

/// Imports a Smithy IDL file or JSON AST as a collection: one folder per
/// resource (and per service when there are several), one request per
/// HTTP-bound operation. Validation findings become warnings
/// (`line:col: severity: message`); only syntax errors fail.
pub fn import(text: &str) -> Result<(Collection, Vec<String>), ConvertError> {
    let model = parse(text).map_err(|d| ConvertError::Parse(d.to_string()))?;
    Ok(import_model(&model))
}

/// [`import`] for a model split over several `(name, text)` files; warnings
/// start with the file name (`main.smithy:3:1: warning: ...`).
pub fn import_files(files: &[(impl AsRef<str>, impl AsRef<str>)]) -> Result<(Collection, Vec<String>), ConvertError> {
    let sources = sources(files);
    import_sources(&sources.iter().collect::<Vec<_>>())
}

/// [`import`] for already parsed files.
pub fn import_sources(files: &[&SourceFile]) -> Result<(Collection, Vec<String>), ConvertError> {
    let model = assemble(files)
        .map_err(|errors| ConvertError::Parse(errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")))?;
    Ok(import_model(&model))
}

fn import_model(model: &Model) -> (Collection, Vec<String>) {
    let mut warnings: Vec<String> = model.validate().iter().map(ToString::to_string).collect();
    let (c, w) = model.to_collection();
    warnings.extend(w);
    (c, warnings)
}

#[cfg(test)]
mod tests;
