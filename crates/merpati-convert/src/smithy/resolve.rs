//! Turns the parser output of one or more files into a resolved [`Model`]:
//! relative shape and trait ids become absolute (per file: its namespace and
//! `use` statements), the files' shapes and metadata are merged, mixins are
//! flattened, elided member targets are filled in and `apply` statements are
//! applied.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::Value;

use super::{Diagnostic, Model, Pos, Shape, ShapeRef};

pub(super) const PRELUDE_SHAPES: &[&str] = &[
    "String",
    "Blob",
    "BigInteger",
    "BigDecimal",
    "Timestamp",
    "Document",
    "Boolean",
    "PrimitiveBoolean",
    "Byte",
    "PrimitiveByte",
    "Short",
    "PrimitiveShort",
    "Integer",
    "PrimitiveInteger",
    "Long",
    "PrimitiveLong",
    "Float",
    "PrimitiveFloat",
    "Double",
    "PrimitiveDouble",
    "Unit",
];

pub(super) const PRELUDE_TRAITS: &[&str] = &[
    "addedDefault",
    "auth",
    "authDefinition",
    "box",
    "clientOptional",
    "cors",
    "default",
    "deprecated",
    "documentation",
    "endpoint",
    "enum",
    "enumValue",
    "error",
    "eventHeader",
    "eventPayload",
    "examples",
    "externalDocumentation",
    "hostLabel",
    "http",
    "httpApiKeyAuth",
    "httpBasicAuth",
    "httpBearerAuth",
    "httpChecksumRequired",
    "httpDigestAuth",
    "httpError",
    "httpHeader",
    "httpLabel",
    "httpPayload",
    "httpPrefixHeaders",
    "httpQuery",
    "httpQueryParams",
    "httpResponseCode",
    "idRef",
    "idempotencyToken",
    "idempotent",
    "input",
    "internal",
    "jsonName",
    "length",
    "mediaType",
    "mixin",
    "nestedProperties",
    "noReplace",
    "notProperty",
    "optionalAuth",
    "output",
    "paginated",
    "pattern",
    "private",
    "property",
    "protocolDefinition",
    "range",
    "readonly",
    "recommended",
    "references",
    "rename",
    "requestCompression",
    "required",
    "requiresLength",
    "resourceIdentifier",
    "retryable",
    "sensitive",
    "since",
    "sparse",
    "streaming",
    "suppress",
    "tags",
    "timestampFormat",
    "title",
    "trait",
    "traitValidations",
    "uniqueItems",
    "unitType",
    "unstable",
    "xmlAttribute",
    "xmlFlattened",
    "xmlName",
    "xmlNamespace",
];

/// Traits defined outside a typical model file that we know about.
pub(super) const KNOWN_EXTERNAL_TRAITS: &[&str] = &[
    "aws.protocols#restJson1",
    "aws.protocols#restXml",
    "aws.protocols#awsJson1_0",
    "aws.protocols#awsJson1_1",
    "aws.protocols#awsQuery",
    "aws.protocols#ec2Query",
    "aws.protocols#awsQueryError",
    "aws.protocols#httpChecksum",
    "aws.auth#sigv4",
    "aws.auth#sigv4a",
    "aws.auth#unsignedPayload",
    "aws.api#service",
    "aws.api#arn",
    "aws.api#arnReference",
    "aws.api#clientEndpointDiscovery",
    "aws.api#controlPlane",
    "aws.api#dataPlane",
    "alloy#simpleRestJson",
    "alloy#uuidFormat",
    "alloy#discriminated",
    "alloy#untagged",
    "smithy.protocols#rpcv2Cbor",
    "smithy.rules#endpointRuleSet",
    "smithy.rules#endpointTests",
    "smithy.rules#clientContextParams",
    "smithy.rules#contextParam",
    "smithy.rules#staticContextParams",
    "smithy.waiters#waitable",
    "smithy.test#httpRequestTests",
    "smithy.test#httpResponseTests",
    "smithy.test#httpMalformedRequestTests",
];

#[derive(Clone, Debug)]
pub(crate) struct RawShape {
    pub shape: Shape,
    /// Members written as `$name` whose target comes from a mixin or resource.
    pub elided: Vec<String>,
}

impl RawShape {
    pub fn new(shape: Shape) -> Self {
        RawShape { shape, elided: Vec::new() }
    }
}

/// Trait id (or name before resolution), value, position.
pub(crate) type AppliedTrait = (String, Value, Pos);

/// `use` statements: local name -> (absolute id, position).
pub(crate) type Uses = IndexMap<String, (String, Pos)>;

#[derive(Clone, Debug)]
pub(crate) struct RawApply {
    pub target: String,
    pub pos: Pos,
    pub traits: Vec<AppliedTrait>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RawModel {
    pub version: String,
    /// Metadata entries in file order (a key may repeat; merged later).
    pub metadata: Vec<(String, Value, Pos)>,
    pub namespace: String,
    pub uses: Uses,
    pub shapes: Vec<RawShape>,
    pub applies: Vec<RawApply>,
    pub diagnostics: Vec<Diagnostic>,
}

impl RawModel {
    /// Moves every position into file `file`.
    fn set_file(&mut self, file: u32) {
        let mut f = |p: &mut Pos| p.file = file;
        for s in &mut self.shapes {
            s.shape.for_each_pos(&mut f);
        }
        for a in &mut self.applies {
            f(&mut a.pos);
            a.traits.iter_mut().for_each(|t| f(&mut t.2));
        }
        self.uses.values_mut().for_each(|u| f(&mut u.1));
        self.metadata.iter_mut().for_each(|m| f(&mut m.2));
        self.diagnostics.iter_mut().for_each(|d| f(&mut d.pos));
    }
}

/// The absolute ids a file defines and those it refers to without defining
/// them, for [`super::related_files`]. Relative names that are neither
/// imported nor defined are taken to be in the file's namespace, which is
/// where Smithy looks before the prelude.
pub(super) fn links(raw: &RawModel) -> (HashSet<String>, HashSet<String>) {
    let ns = &raw.namespace;
    let abs = |name: &str| -> String {
        if name.contains('#') {
            return name.to_string();
        }
        match raw.uses.get(name) {
            Some((id, _)) => id.clone(),
            None if ns.is_empty() => name.to_string(),
            None => format!("{ns}#{name}"),
        }
    };
    let defines: HashSet<String> = raw.shapes.iter().map(|s| abs(&s.shape.id)).collect();
    let mut needs = HashSet::new();
    let mut need = |name: &str| {
        let root = name.split('$').next().unwrap_or(name);
        if root.is_empty() {
            return;
        }
        let id = abs(root);
        if !defines.contains(&id) {
            needs.insert(id);
        }
    };
    for s in &raw.shapes {
        let sh = &s.shape;
        sh.traits.keys().for_each(|t| need(t));
        for m in sh.members.values() {
            need(&m.target);
            m.traits.keys().for_each(|t| need(t));
        }
        sh.refs().for_each(|(_, r)| need(&r.id));
    }
    for a in &raw.applies {
        need(&a.target);
        a.traits.iter().for_each(|t| need(&t.0));
    }
    raw.uses.values().for_each(|(id, _)| need(id));
    (defines, needs)
}

struct Resolver<'a> {
    namespace: &'a str,
    uses: &'a Uses,
    /// Absolute ids of the shapes of every file.
    defined: &'a HashSet<String>,
    used_imports: HashSet<String>,
}

impl Resolver<'_> {
    fn shape(&mut self, name: &str) -> String {
        self.resolve(name, PRELUDE_SHAPES)
    }

    fn trait_id(&mut self, name: &str) -> String {
        self.resolve(name, PRELUDE_TRAITS)
    }

    fn resolve(&mut self, name: &str, prelude: &[&str]) -> String {
        if name.contains('#') {
            return name.to_string();
        }
        let (root, member) = match name.split_once('$') {
            Some((r, m)) => (r, Some(m)),
            None => (name, None),
        };
        let local = format!("{}#{root}", self.namespace);
        let abs = if let Some((id, _)) = self.uses.get(root) {
            self.used_imports.insert(root.to_string());
            id.clone()
        } else if !self.namespace.is_empty() && self.defined.contains(&local) {
            local
        } else if prelude.contains(&root) {
            format!("smithy.api#{root}")
        } else if self.namespace.is_empty() {
            root.to_string()
        } else {
            local
        };
        match member {
            Some(m) => format!("{abs}${m}"),
            None => abs,
        }
    }

    fn shape_ref(&mut self, r: &mut ShapeRef) {
        r.id = self.shape(&r.id);
    }
}

fn is_known_trait(id: &str, defined: &HashSet<String>, imported: &HashSet<String>) -> bool {
    match id.split_once('#') {
        Some(("smithy.api", name)) => PRELUDE_TRAITS.contains(&name),
        // Traits of the AWS, Smithy and alloy libraries come from dependencies.
        Some((ns, _)) if is_library_namespace(ns) => true,
        _ => defined.contains(id) || imported.contains(id) || KNOWN_EXTERNAL_TRAITS.contains(&id),
    }
}

fn is_library_namespace(ns: &str) -> bool {
    ns.starts_with("aws.") || ns.starts_with("smithy.") || ns == "alloy" || ns.starts_with("alloy.")
}

fn resolve_traits(
    r: &mut Resolver<'_>,
    traits: &mut IndexMap<String, Value>,
    pos: &mut IndexMap<String, Pos>,
    fallback: Pos,
    known: &dyn Fn(&str) -> bool,
    diags: &mut Vec<Diagnostic>,
    owner: &str,
) {
    let old = std::mem::take(traits);
    let old_pos = std::mem::take(pos);
    for (name, value) in old {
        let id = r.trait_id(&name);
        let p = old_pos.get(&name).copied().unwrap_or(fallback);
        if !known(&id) {
            let local = super::local_name(&id);
            let hint = KNOWN_EXTERNAL_TRAITS
                .iter()
                .find(|k| super::local_name(k) == local)
                .map(|k| format!("; add `use {k}`"))
                .unwrap_or_default();
            diags.push(Diagnostic::warning(p, format!("unknown trait `@{name}`{hint}")).on(owner));
        }
        pos.insert(id.clone(), p);
        traits.insert(id, value);
    }
}

/// `file:line:col` (`line:col` for a single text), for messages that point
/// at a second location.
fn at(files: &[String], p: Pos) -> String {
    match files.get(p.file as usize).filter(|n| !n.is_empty()) {
        Some(name) => format!("{name}:{}:{}", p.line, p.column),
        None => format!("{}:{}", p.line, p.column),
    }
}

/// Smithy's rule for a trait (or metadata key) given twice: lists are
/// concatenated, equal values are kept once, anything else is a conflict
/// (`false`, `old` unchanged).
fn merge_values(old: &mut Value, new: Value) -> bool {
    match (old, new) {
        (Value::Array(a), Value::Array(b)) => {
            a.extend(b);
            true
        }
        (old, new) => *old == new,
    }
}

/// Adds `traits` to a trait map following [`merge_values`]; conflicts are
/// reported at the new trait's position.
fn merge_traits(
    map: &mut IndexMap<String, Value>,
    pos: &mut IndexMap<String, Pos>,
    traits: impl IntoIterator<Item = AppliedTrait>,
    owner: &str,
    shape: &str,
    files: &[String],
    diags: &mut Vec<Diagnostic>,
) {
    for (id, value, p) in traits {
        match map.get_mut(&id) {
            None => {
                map.insert(id.clone(), value);
                pos.insert(id, p);
            }
            Some(old) => {
                if !merge_values(old, value) {
                    let other = pos.get(&id).map(|q| format!(" (also set at {})", at(files, *q))).unwrap_or_default();
                    let msg = format!("conflicting values for trait `@{}` on `{owner}`{other}", super::local_name(&id));
                    diags.push(Diagnostic::error(p, msg).on(shape));
                }
            }
        }
    }
}

/// Whether two definitions of a shape id (from different files) describe the
/// same shape: same type, members with the same targets, same references.
fn same_shape(a: &Shape, b: &Shape) -> bool {
    fn ids(v: &[ShapeRef]) -> Vec<&str> {
        let mut out: Vec<&str> = v.iter().map(|r| r.id.as_str()).collect();
        out.sort_unstable();
        out
    }
    fn map(m: &IndexMap<String, ShapeRef>) -> Vec<(&str, &str)> {
        let mut out: Vec<(&str, &str)> = m.iter().map(|(k, r)| (k.as_str(), r.id.as_str())).collect();
        out.sort_unstable();
        out
    }
    let one = |r: &Option<ShapeRef>| r.as_ref().map(|r| r.id.clone());
    a.kind == b.kind
        && a.members.len() == b.members.len()
        && a.members.iter().all(|(k, m)| b.members.get(k).is_some_and(|n| n.target == m.target))
        && ids(&a.mixins) == ids(&b.mixins)
        && one(&a.input) == one(&b.input)
        && one(&a.output) == one(&b.output)
        && one(&a.for_resource) == one(&b.for_resource)
        && ids(&a.errors) == ids(&b.errors)
        && a.version == b.version
        && ids(&a.operations) == ids(&b.operations)
        && ids(&a.resources) == ids(&b.resources)
        && ids(&a.collection_operations) == ids(&b.collection_operations)
        && map(&a.identifiers) == map(&b.identifiers)
        && map(&a.properties) == map(&b.properties)
        && map(&a.lifecycle) == map(&b.lifecycle)
}

/// A single text: no file names.
pub(super) fn finish(raw: RawModel) -> Model {
    assemble(vec![raw], Vec::new())
}

/// Resolves the files `raws` (named `files`, same order; may be empty for a
/// single text) into one model.
pub(super) fn assemble(mut raws: Vec<RawModel>, files: Vec<String>) -> Model {
    let mut diagnostics = Vec::new();
    for (i, raw) in raws.iter_mut().enumerate() {
        if i > 0 {
            raw.set_file(i as u32);
        }
        for s in &mut raw.shapes {
            if !s.shape.id.contains('#') && !raw.namespace.is_empty() {
                s.shape.id = format!("{}#{}", raw.namespace, s.shape.id);
            }
        }
    }
    // Absolute ids of everything defined in any file.
    let defined_ids: HashSet<String> = raws.iter().flat_map(|r| r.shapes.iter().map(|s| s.shape.id.clone())).collect();

    let mut version = String::new();
    let mut namespace = String::new();
    let mut metadata_entries: Vec<(String, Value, Pos)> = Vec::new();
    let mut order: Vec<RawShape> = Vec::new();
    let mut applies: Vec<(String, Vec<AppliedTrait>, Pos)> = Vec::new();
    // Per file: `use` statements and the ones that were used.
    let mut imports: Vec<(Uses, HashSet<String>)> = Vec::new();
    for raw in raws {
        let RawModel { version: v, metadata, namespace: ns, uses, shapes, applies: raw_applies, diagnostics: d } = raw;
        diagnostics.extend(d);
        if version.is_empty() || v > version {
            version = v;
        }
        if namespace.is_empty() {
            namespace = ns.clone();
        }
        metadata_entries.extend(metadata);
        let imported: HashSet<String> = uses.values().map(|(id, _)| id.clone()).collect();
        let known = |id: &str| is_known_trait(id, &defined_ids, &imported);
        let mut r = Resolver { namespace: &ns, uses: &uses, defined: &defined_ids, used_imports: HashSet::new() };
        for mut s in shapes {
            let sh = &mut s.shape;
            let owner = sh.id.clone();
            resolve_traits(&mut r, &mut sh.traits, &mut sh.trait_pos, sh.pos, &known, &mut diagnostics, &owner);
            for m in sh.members.values_mut() {
                if !m.target.is_empty() {
                    m.target = r.shape(&m.target);
                }
                resolve_traits(&mut r, &mut m.traits, &mut m.trait_pos, m.pos, &known, &mut diagnostics, &owner);
            }
            for x in [&mut sh.input, &mut sh.output, &mut sh.for_resource].into_iter().flatten() {
                r.shape_ref(x);
            }
            for list in
                [&mut sh.mixins, &mut sh.errors, &mut sh.operations, &mut sh.resources, &mut sh.collection_operations]
            {
                for x in list.iter_mut() {
                    r.shape_ref(x);
                }
            }
            for map in [&mut sh.identifiers, &mut sh.properties, &mut sh.lifecycle] {
                for x in map.values_mut() {
                    r.shape_ref(x);
                }
            }
            order.push(s);
        }
        for a in raw_applies {
            let target = r.shape(&a.target);
            let mut traits = Vec::new();
            for (name, value, pos) in a.traits {
                let id = r.trait_id(&name);
                if !known(&id) {
                    diagnostics.push(Diagnostic::warning(pos, format!("unknown trait `@{name}`")));
                }
                traits.push((id, value, pos));
            }
            applies.push((target, traits, a.pos));
        }
        let used = r.used_imports;
        imports.push((uses, used));
    }

    let mut model = Model { version, namespace, files, ..Default::default() };

    // Shapes of every file. A shape id defined twice in one file is an error;
    // across files the definitions must describe the same shape, and their
    // traits are merged like `apply`.
    let mut elided: HashMap<String, Vec<String>> = HashMap::new();
    // Shape ids must also be unique ignoring case.
    let mut folded: HashMap<String, String> = HashMap::new();
    for s in order {
        let RawShape { shape, elided: el } = s;
        let lower = shape.id.to_ascii_lowercase();
        if let Some(other) = folded.get(&lower).filter(|o| **o != shape.id) {
            let at_other = model.shapes.get(other).map(|o| at(&model.files, o.pos)).unwrap_or_default();
            let msg = format!(
                "shape `{}` conflicts with `{other}` defined at {at_other}: shape ids must differ in more than case",
                shape.id
            );
            diagnostics.push(Diagnostic::error(shape.pos, msg).on(&shape.id));
            continue;
        }
        folded.insert(lower, shape.id.clone());
        let Some(prev) = model.shapes.get_mut(&shape.id) else {
            if !el.is_empty() {
                elided.insert(shape.id.clone(), el);
            }
            model.shapes.insert(shape.id.clone(), shape);
            continue;
        };
        if prev.pos.file == shape.pos.file {
            diagnostics
                .push(Diagnostic::error(shape.pos, format!("shape `{}` is defined twice", shape.name())).on(&shape.id));
            continue;
        }
        if !same_shape(prev, &shape) {
            let msg = format!(
                "shape `{}` is defined twice: the definition at {} is different",
                shape.id,
                at(&model.files, prev.pos)
            );
            diagnostics.push(Diagnostic::error(shape.pos, msg).on(&shape.id));
            continue;
        }
        let id = shape.id.clone();
        let local = super::local_name(&id).to_string();
        let traits = shape.traits.into_iter().map(|(k, v)| {
            let p = shape.trait_pos.get(&k).copied().unwrap_or(shape.pos);
            (k, v, p)
        });
        merge_traits(&mut prev.traits, &mut prev.trait_pos, traits, &local, &id, &model.files, &mut diagnostics);
        for (name, m) in shape.members {
            let Some(pm) = prev.members.get_mut(&name) else { continue };
            let traits = m.traits.into_iter().map(|(k, v)| {
                let p = m.trait_pos.get(&k).copied().unwrap_or(m.pos);
                (k, v, p)
            });
            let owner = format!("{local}${name}");
            merge_traits(&mut pm.traits, &mut pm.trait_pos, traits, &owner, &id, &model.files, &mut diagnostics);
        }
    }
    let service_ns = model.services().next().map(|svc| super::namespace_of(&svc.id).to_string());
    if model.files.len() > 1
        && let Some(ns) = service_ns
    {
        model.namespace = ns;
    }

    // Metadata of every file: arrays are concatenated, equal values kept
    // once, anything else conflicts.
    let mut metadata_pos: HashMap<String, Pos> = HashMap::new();
    for (key, value, pos) in metadata_entries {
        match model.metadata.get_mut(&key) {
            None => {
                model.metadata.insert(key.clone(), value);
                metadata_pos.insert(key, pos);
            }
            Some(old) => {
                if !merge_values(old, value) {
                    let first = metadata_pos.get(&key).map(|p| at(&model.files, *p)).unwrap_or_default();
                    diagnostics.push(Diagnostic::error(
                        pos,
                        format!("metadata `{key}` conflicts with the value defined at {first}"),
                    ));
                }
            }
        }
    }

    // `apply` statements: first to shapes and their own members, the rest
    // (members inherited from mixins) after flattening.
    let mut deferred: Vec<(String, Vec<AppliedTrait>, Pos)> = Vec::new();
    for (target, traits, pos) in applies {
        if !apply(&mut model, &target, &traits, true, &mut diagnostics) {
            deferred.push((target, traits, pos));
        }
    }

    flatten_mixins(&mut model, &elided, &mut diagnostics);

    for (target, traits, pos) in deferred {
        if !apply(&mut model, &target, &traits, false, &mut diagnostics) {
            diagnostics.push(Diagnostic::error(pos, format!("cannot apply traits to unknown shape `{target}`")));
        }
    }

    for (uses, used) in &imports {
        for (name, (id, pos)) in uses {
            if !used.contains(name) && !model.shapes.contains_key(id) && !KNOWN_EXTERNAL_TRAITS.contains(&id.as_str()) {
                diagnostics.push(Diagnostic::warning(*pos, format!("unused import `{id}`")));
            }
        }
    }
    model.parse_diagnostics = diagnostics;
    model
}

/// Applies traits to a shape or `Shape$member`. Returns false when the target
/// does not exist (yet). With `merge`, a trait the target already has is
/// merged by Smithy's conflict rules; otherwise (members inherited from a
/// mixin) the applied value replaces the inherited one.
fn apply(model: &mut Model, target: &str, traits: &[AppliedTrait], merge: bool, diags: &mut Vec<Diagnostic>) -> bool {
    let (root, member) = match target.split_once('$') {
        Some((r, m)) => (r, Some(m)),
        None => (target, None),
    };
    let files = &model.files;
    let Some(shape) = model.shapes.get_mut(root) else { return false };
    let (map, pos) = match member {
        Some(m) => match shape.members.get_mut(m) {
            Some(mem) => (&mut mem.traits, &mut mem.trait_pos),
            None => return false,
        },
        None => (&mut shape.traits, &mut shape.trait_pos),
    };
    if merge {
        let owner = match member {
            Some(m) => format!("{}${m}", super::local_name(root)),
            None => super::local_name(root).to_string(),
        };
        merge_traits(map, pos, traits.iter().cloned(), &owner, root, files, diags);
        return true;
    }
    for (id, value, p) in traits {
        map.insert(id.clone(), value.clone());
        pos.insert(id.clone(), *p);
    }
    true
}

fn flatten_mixins(model: &mut Model, elided: &HashMap<String, Vec<String>>, diags: &mut Vec<Diagnostic>) {
    let ids: Vec<String> = model.shapes.keys().cloned().collect();
    let mut done: HashSet<String> = HashSet::new();
    for id in ids {
        let mut stack = Vec::new();
        flatten_one(model, &id, elided, &mut done, &mut stack, diags);
    }
}

fn flatten_one(
    model: &mut Model,
    id: &str,
    elided: &HashMap<String, Vec<String>>,
    done: &mut HashSet<String>,
    stack: &mut Vec<String>,
    diags: &mut Vec<Diagnostic>,
) {
    if done.contains(id) {
        return;
    }
    let Some(shape) = model.shapes.get(id) else { return };
    if stack.iter().any(|s| s == id) {
        diags.push(Diagnostic::error(shape.pos, format!("mixin cycle through `{}`", shape.name())).on(id));
        return;
    }
    stack.push(id.to_string());
    let mixins: Vec<ShapeRef> = shape.mixins.clone();
    for m in &mixins {
        flatten_one(model, &m.id, elided, done, stack, diags);
    }
    stack.pop();
    done.insert(id.to_string());

    let mut members = IndexMap::new();
    let mut inherited_traits: IndexMap<String, Value> = IndexMap::new();
    for m in &mixins {
        let Some(mixin) = model.shapes.get(&m.id) else { continue };
        for (name, mem) in &mixin.members {
            members.entry(name.clone()).or_insert_with(|| mem.clone());
        }
        let local: Vec<String> = mixin
            .get_trait("mixin")
            .and_then(|v| v.get("localTraits"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        for (k, v) in &mixin.traits {
            if k == "smithy.api#mixin" || local.iter().any(|l| l == k || super::local_name(k) == l) {
                continue;
            }
            inherited_traits.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    let resource_targets: HashMap<String, String> = model
        .shapes
        .get(id)
        .and_then(|s| s.for_resource.as_ref())
        .and_then(|r| model.shapes.get(&r.id))
        .map(|res| {
            res.identifiers.iter().chain(res.properties.iter()).map(|(k, v)| (k.clone(), v.id.clone())).collect()
        })
        .unwrap_or_default();
    let elided_here = elided.get(id).cloned().unwrap_or_default();
    let Some(shape) = model.shapes.get_mut(id) else { return };
    let own = std::mem::take(&mut shape.members);
    for (name, mut mem) in own {
        if let Some(inh) = members.get_mut(&name) {
            // Redeclared/elided mixin member: keep the target, merge traits.
            if mem.target.is_empty() {
                mem.target = inh.target.clone();
            }
            let mut traits = inh.traits.clone();
            traits.extend(mem.traits);
            mem.traits = traits;
            let mut tpos = inh.trait_pos.clone();
            tpos.extend(mem.trait_pos);
            mem.trait_pos = tpos;
            *inh = mem;
            continue;
        }
        if mem.target.is_empty() {
            match resource_targets.get(&name) {
                Some(t) => mem.target = t.clone(),
                None => {
                    let why = if elided_here.contains(&name) && shape.for_resource.is_none() && mixins.is_empty() {
                        "the shape has no mixins and no `for` resource"
                    } else {
                        "no mixin or resource defines it"
                    };
                    diags.push(
                        Diagnostic::error(mem.pos, format!("cannot elide the target of member `${name}`: {why}"))
                            .on(id),
                    );
                    mem.target = "smithy.api#Document".into();
                }
            }
        }
        members.insert(name, mem);
    }
    shape.members = members;
    for (k, v) in inherited_traits {
        shape.traits.entry(k).or_insert(v);
    }
}
