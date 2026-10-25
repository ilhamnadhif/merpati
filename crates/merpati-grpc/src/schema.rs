//! Service definitions: compiling `.proto` files, browsing services and
//! methods, example request templates and JSON <-> protobuf conversion.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use prost_reflect::{
    DescriptorPool, DynamicMessage, FieldDescriptor, Kind, MessageDescriptor, MethodDescriptor, ReflectMessage,
    SerializeOptions,
};
use serde_json::{Map, Value, json};

use crate::error::GrpcError;

/// How deep nested messages are expanded in [`MethodInfo::input_template`].
const TEMPLATE_DEPTH: usize = 2;

/// A set of service definitions, from `.proto` files or server reflection.
/// Cheap to clone (the descriptor pool is reference counted).
#[derive(Clone, Debug)]
pub struct GrpcSchema {
    pool: DescriptorPool,
    warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MethodKind {
    Unary,
    ClientStreaming,
    ServerStreaming,
    Bidi,
}

impl MethodKind {
    pub fn from_flags(client_streaming: bool, server_streaming: bool) -> MethodKind {
        match (client_streaming, server_streaming) {
            (false, false) => MethodKind::Unary,
            (true, false) => MethodKind::ClientStreaming,
            (false, true) => MethodKind::ServerStreaming,
            (true, true) => MethodKind::Bidi,
        }
    }

    /// The client may send more than one message.
    pub fn is_client_streaming(self) -> bool {
        matches!(self, MethodKind::ClientStreaming | MethodKind::Bidi)
    }

    /// The server may answer with more than one message.
    pub fn is_server_streaming(self) -> bool {
        matches!(self, MethodKind::ServerStreaming | MethodKind::Bidi)
    }

    pub fn label(self) -> &'static str {
        match self {
            MethodKind::Unary => "unary",
            MethodKind::ClientStreaming => "client streaming",
            MethodKind::ServerStreaming => "server streaming",
            MethodKind::Bidi => "bidirectional streaming",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServiceInfo {
    /// Fully qualified, e.g. `helloworld.Greeter`.
    pub name: String,
    pub methods: Vec<MethodInfo>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MethodInfo {
    pub name: String,
    pub kind: MethodKind,
    /// Fully qualified message type names.
    pub input_type: String,
    pub output_type: String,
    /// Example request JSON with default-valued fields (nested messages
    /// expanded two levels deep).
    pub input_template: String,
}

impl GrpcSchema {
    pub fn from_pool(pool: DescriptorPool) -> GrpcSchema {
        GrpcSchema { pool, warnings: Vec::new() }
    }

    pub(crate) fn with_warnings(mut self, warnings: Vec<String>) -> GrpcSchema {
        self.warnings = warnings;
        self
    }

    /// Decodes a serialized `google.protobuf.FileDescriptorSet` (e.g. the
    /// output of `protoc --descriptor_set_out --include_imports`, or a schema
    /// cached with [`GrpcSchema::to_descriptor_set_bytes`]).
    pub fn from_descriptor_set_bytes(bytes: &[u8]) -> Result<GrpcSchema, GrpcError> {
        DescriptorPool::decode(bytes).map(GrpcSchema::from_pool).map_err(|e| GrpcError::Descriptor(e.to_string()))
    }

    /// Serializes all files as a `FileDescriptorSet`, e.g. to cache a schema
    /// obtained through reflection.
    pub fn to_descriptor_set_bytes(&self) -> Vec<u8> {
        self.pool.encode_to_vec()
    }

    pub fn pool(&self) -> &DescriptorPool {
        &self.pool
    }

    /// Non-fatal problems met while loading (e.g. a service whose
    /// definition the reflection server could not provide).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// All services, sorted by name; methods in declaration order.
    pub fn services(&self) -> Vec<ServiceInfo> {
        let mut services: Vec<ServiceInfo> = self
            .pool
            .services()
            .map(|s| ServiceInfo {
                name: s.full_name().to_string(),
                methods: s.methods().map(|m| method_info(&m)).collect(),
            })
            .collect();
        services.sort_by(|a, b| a.name.cmp(&b.name));
        services
    }

    pub fn method(&self, service: &str, method: &str) -> Option<MethodInfo> {
        self.method_descriptor(service, method).ok().map(|m| method_info(&m))
    }

    /// Example JSON for any message type (fully qualified name).
    pub fn message_template(&self, full_name: &str) -> Option<String> {
        self.pool.get_message_by_name(full_name.trim().trim_start_matches('.')).map(|m| template_for(&m))
    }

    /// Checks that `json` is a valid request message for the method.
    pub fn validate_request(&self, service: &str, method: &str, json: &str) -> Result<(), GrpcError> {
        parse_message(&self.method_descriptor(service, method)?.input(), json).map(|_| ())
    }

    pub(crate) fn method_descriptor(&self, service: &str, method: &str) -> Result<MethodDescriptor, GrpcError> {
        let service_name = service.trim().trim_start_matches('.');
        let svc = self
            .pool
            .get_service_by_name(service_name)
            .ok_or_else(|| GrpcError::UnknownService(service_name.to_string()))?;
        let method_name = method.trim();
        svc.methods().find(|m| m.name() == method_name).ok_or_else(|| GrpcError::UnknownMethod {
            service: service_name.to_string(),
            method: method_name.to_string(),
        })
    }
}

pub(crate) fn method_kind(m: &MethodDescriptor) -> MethodKind {
    MethodKind::from_flags(m.is_client_streaming(), m.is_server_streaming())
}

fn method_info(m: &MethodDescriptor) -> MethodInfo {
    MethodInfo {
        name: m.name().to_string(),
        kind: method_kind(m),
        input_type: m.input().full_name().to_string(),
        output_type: m.output().full_name().to_string(),
        input_template: template_for(&m.input()),
    }
}

// ---------------------------------------------------------------------------
// .proto compilation
// ---------------------------------------------------------------------------

/// Compiles `.proto` files with the pure-Rust `protox` compiler. Relative
/// file and import paths are resolved against `base_dir` (or the current
/// directory). The directory of each file is an implicit import path, and
/// the Google well-known types (`google/protobuf/*.proto`) are always
/// available.
pub fn load_proto_files(
    files: &[PathBuf],
    import_paths: &[PathBuf],
    base_dir: Option<&Path>,
) -> Result<GrpcSchema, GrpcError> {
    if files.is_empty() {
        return Err(GrpcError::Proto("no .proto files selected".into()));
    }
    let absolute = |p: &Path| -> PathBuf {
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(base) = base_dir {
            base.join(p)
        } else {
            std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
        };
        std::fs::canonicalize(&joined).unwrap_or(joined)
    };
    let mut warnings = Vec::new();
    let mut resolved_files = Vec::new();
    for f in files {
        let path = absolute(f);
        if !path.is_file() {
            return Err(GrpcError::Proto(format!("file not found: {}", path.display())));
        }
        resolved_files.push(path);
    }
    let mut includes: Vec<PathBuf> = Vec::new();
    for p in import_paths {
        let path = absolute(p);
        if path.is_dir() {
            includes.push(path);
        } else {
            warnings.push(format!("import path not found: {}", path.display()));
        }
    }
    for f in &resolved_files {
        if !includes.iter().any(|i| f.starts_with(i))
            && let Some(parent) = f.parent()
        {
            includes.push(parent.to_path_buf());
        }
    }
    let mut compiler = protox::Compiler::new(&includes).map_err(|e| GrpcError::Proto(describe_protox(&e)))?;
    compiler.include_imports(true).include_source_info(false);
    compiler.open_files(&resolved_files).map_err(|e| GrpcError::Proto(describe_protox(&e)))?;
    Ok(GrpcSchema::from_pool(compiler.descriptor_pool()).with_warnings(warnings))
}

/// `file.proto:12:5: message (help)` from a protox error.
fn describe_protox(err: &protox::Error) -> String {
    use miette::Diagnostic;
    let mut location = err.file().map(str::to_string);
    if let (Some(source), Some(label)) = (err.source_code(), err.labels().and_then(|mut l| l.next()))
        && let Ok(span) = source.read_span(label.inner(), 0, 0)
    {
        let name = span.name().map(str::to_string).or(location.take()).unwrap_or_default();
        location = Some(format!("{name}:{}:{}", span.line() + 1, span.column() + 1));
    }
    let mut out = match location {
        Some(loc) if !loc.is_empty() => format!("{loc}: {err}"),
        _ => err.to_string(),
    };
    if let Some(help) = err.help() {
        out.push_str(&format!(" ({help})"));
    } else if err.is_file_not_found() {
        out.push_str(" (add the directory that contains it to the import paths)");
    }
    out
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// Parses protobuf-JSON into a message of type `desc`. Blank input is the
/// empty message.
pub(crate) fn parse_message(desc: &MessageDescriptor, json: &str) -> Result<DynamicMessage, GrpcError> {
    let invalid = |e: serde_json::Error| GrpcError::InvalidJson {
        type_name: desc.full_name().to_string(),
        message: e.to_string(),
    };
    if json.trim().is_empty() {
        return Ok(DynamicMessage::new(desc.clone()));
    }
    let mut de = serde_json::Deserializer::from_str(json);
    let msg = DynamicMessage::deserialize(desc.clone(), &mut de).map_err(invalid)?;
    de.end().map_err(invalid)?;
    Ok(msg)
}

fn json_options() -> SerializeOptions {
    SerializeOptions::new().skip_default_fields(false)
}

/// Pretty protobuf-JSON, including fields that hold their default value.
/// prost-reflect cannot render an `Any` whose type the schema does not
/// define; such a message is rendered by [`message_value`] instead, so only
/// that `Any` shows a placeholder.
pub(crate) fn message_to_json(msg: &DynamicMessage) -> String {
    let pretty = || serde_json::ser::PrettyFormatter::with_indent(b"  ");
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, pretty());
    if msg.serialize_with_options(&mut ser, &json_options()).is_ok() {
        return String::from_utf8(buf).unwrap_or_default();
    }
    // Same two-space indentation as above.
    serde_json::to_string_pretty(&message_value(msg)).unwrap_or_default()
}

/// prost-reflect's JSON for `msg`, if it can render it. Written as text and
/// parsed back so floats keep their shortest form (`0.1`, not the widened f32).
fn reflect_value(msg: &DynamicMessage) -> Option<Value> {
    let mut buf = Vec::new();
    msg.serialize_with_options(&mut serde_json::Serializer::new(&mut buf), &json_options()).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Protobuf-JSON of a message prost-reflect cannot render as a whole: field
/// by field like prost-reflect (field number order, default values included
/// for fields without presence, JSON names), every part that it can render
/// still going through it.
fn message_value(msg: &DynamicMessage) -> Value {
    if let Some(v) = reflect_value(msg) {
        return v;
    }
    let desc = msg.descriptor();
    if desc.full_name() == "google.protobuf.Any" {
        return any_value(msg);
    }
    let mut map = Map::new();
    for field in desc.fields() {
        if field.supports_presence() && !msg.has_field(&field) {
            continue;
        }
        map.insert(field.json_name().to_string(), field_value(&msg.get_field(&field), &field.kind()));
    }
    for (ext, value) in msg.extensions() {
        map.insert(ext.json_name().to_string(), field_value(value, &ext.kind()));
    }
    Value::Object(map)
}

/// One field value, mapped like prost-reflect's serializer does.
fn field_value(value: &prost_reflect::Value, kind: &Kind) -> Value {
    use prost_reflect::{MapKey, Value as V};
    let float = |f: f64, text: String| {
        if f.is_nan() {
            json!("NaN")
        } else if f.is_infinite() {
            json!(if f > 0.0 { "Infinity" } else { "-Infinity" })
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        }
    };
    match value {
        V::Bool(b) => json!(b),
        V::I32(n) => json!(n),
        V::U32(n) => json!(n),
        // 64-bit integers are strings in protobuf-JSON.
        V::I64(n) => json!(n.to_string()),
        V::U64(n) => json!(n.to_string()),
        V::F32(f) => float(f64::from(*f), format!("{f:?}")),
        V::F64(f) => float(*f, format!("{f:?}")),
        V::String(s) => json!(s),
        V::Bytes(b) => {
            use base64::Engine as _;
            json!(base64::engine::general_purpose::STANDARD.encode(b))
        }
        V::EnumNumber(n) => match kind {
            Kind::Enum(e) if e.full_name() == "google.protobuf.NullValue" => Value::Null,
            Kind::Enum(e) => e.get_value(*n).map(|v| json!(v.name())).unwrap_or(json!(n)),
            _ => json!(n),
        },
        V::Message(m) => message_value(m),
        V::List(items) => Value::Array(items.iter().map(|v| field_value(v, kind)).collect()),
        V::Map(entries) => {
            let value_kind = match kind {
                Kind::Message(entry) if entry.is_map_entry() => entry.map_entry_value_field().kind(),
                other => other.clone(),
            };
            let mut entries: Vec<_> = entries.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let key = |k: &MapKey| match k {
                MapKey::Bool(b) => b.to_string(),
                MapKey::I32(n) => n.to_string(),
                MapKey::I64(n) => n.to_string(),
                MapKey::U32(n) => n.to_string(),
                MapKey::U64(n) => n.to_string(),
                MapKey::String(s) => s.clone(),
            };
            Value::Object(entries.into_iter().map(|(k, v)| (key(k), field_value(v, &value_kind))).collect())
        }
    }
}

/// Well-known types whose JSON is not an object of fields: inside an `Any`
/// they go under `value`.
fn has_special_json(name: &str) -> bool {
    matches!(
        name,
        "google.protobuf.Any"
            | "google.protobuf.Timestamp"
            | "google.protobuf.Duration"
            | "google.protobuf.Struct"
            | "google.protobuf.Value"
            | "google.protobuf.ListValue"
            | "google.protobuf.FieldMask"
            | "google.protobuf.Empty"
            | "google.protobuf.DoubleValue"
            | "google.protobuf.FloatValue"
            | "google.protobuf.Int64Value"
            | "google.protobuf.UInt64Value"
            | "google.protobuf.Int32Value"
            | "google.protobuf.UInt32Value"
            | "google.protobuf.BoolValue"
            | "google.protobuf.StringValue"
            | "google.protobuf.BytesValue"
    )
}

/// An `Any` prost-reflect cannot render. A payload type the schema knows is
/// decoded and rendered like prost-reflect does (it holds an unknown `Any`
/// further down); an unknown type or undecodable bytes get the placeholder
/// grpcurl uses (its `unknownAny` fallback): `{"@type", "@error",
/// "@value"}` with the raw bytes in base64, so the rest of the message stays
/// visible. `@value`, not `value`: a packed `BytesValue` is
/// `{"@type", "value": "<base64>"}` in standard protobuf-JSON.
fn any_value(msg: &DynamicMessage) -> Value {
    let text = |name: &str| msg.get_field_by_name(name).and_then(|v| v.as_str().map(str::to_string));
    let type_url = text("type_url").unwrap_or_default();
    let bytes = msg.get_field_by_name("value").and_then(|v| v.as_bytes().cloned()).unwrap_or_default();
    let name = type_url.rsplit_once('/').map_or(type_url.as_str(), |(_, n)| n);
    let decoded = match msg.descriptor().parent_pool().get_message_by_name(name) {
        _ if name.is_empty() => Err("the Any has no type URL".to_string()),
        None => Err(format!("{name} is not in the schema")),
        Some(desc) => {
            DynamicMessage::decode(desc, bytes.as_ref()).map_err(|e| format!("{name} could not be decoded ({e})"))
        }
    };
    let mut out = Map::new();
    out.insert("@type".into(), json!(type_url));
    match decoded {
        Ok(inner) => match message_value(&inner) {
            Value::Object(fields) if !has_special_json(name) => out.extend(fields),
            other => {
                out.insert("value".into(), other);
            }
        },
        Err(problem) => {
            use base64::Engine as _;
            out.insert("@error".into(), json!(format!("{problem}; @value holds the raw message bytes (base64)")));
            out.insert("@value".into(), json!(base64::engine::general_purpose::STANDARD.encode(&bytes)));
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Request templates
// ---------------------------------------------------------------------------

fn template_for(desc: &MessageDescriptor) -> String {
    serde_json::to_string_pretty(&message_template(desc, 0)).unwrap_or_else(|_| "{}".into())
}

fn message_template(desc: &MessageDescriptor, depth: usize) -> Value {
    if let Some(v) = well_known_template(desc) {
        return v;
    }
    let mut map = Map::new();
    if depth > TEMPLATE_DEPTH {
        return Value::Object(map);
    }
    let mut oneofs_seen = HashSet::new();
    // Declaration order reads better than field-number order.
    for proto in &desc.descriptor_proto().field {
        let Some(field) = desc.get_field_by_name(proto.name()) else { continue };
        if let Some(oneof) = field.containing_oneof()
            && !oneof.is_synthetic()
            && !oneofs_seen.insert(oneof.name().to_string())
        {
            // Only the first member of a oneof: setting two is an error.
            continue;
        }
        map.insert(field.json_name().to_string(), field_template(&field, depth));
    }
    Value::Object(map)
}

fn field_template(field: &FieldDescriptor, depth: usize) -> Value {
    if field.is_map()
        && let Kind::Message(entry) = field.kind()
    {
        let key = match entry.map_entry_key_field().kind() {
            Kind::String => "key".to_string(),
            Kind::Bool => "false".to_string(),
            _ => "0".to_string(),
        };
        let value = kind_template(&entry.map_entry_value_field().kind(), depth);
        let mut map = Map::new();
        map.insert(key, value);
        return Value::Object(map);
    }
    let value = kind_template(&field.kind(), depth);
    if field.is_list() { Value::Array(vec![value]) } else { value }
}

fn kind_template(kind: &Kind, depth: usize) -> Value {
    match kind {
        Kind::Double | Kind::Float => json!(0.0),
        Kind::Int32
        | Kind::Sint32
        | Kind::Sfixed32
        | Kind::Uint32
        | Kind::Fixed32
        | Kind::Int64
        | Kind::Sint64
        | Kind::Sfixed64
        | Kind::Uint64
        | Kind::Fixed64 => json!(0),
        Kind::Bool => json!(false),
        Kind::String | Kind::Bytes => json!(""),
        Kind::Enum(e) => {
            if e.full_name() == "google.protobuf.NullValue" {
                Value::Null
            } else {
                json!(e.default_value().name())
            }
        }
        Kind::Message(m) => message_template(m, depth + 1),
    }
}

fn well_known_template(desc: &MessageDescriptor) -> Option<Value> {
    Some(match desc.full_name() {
        "google.protobuf.Timestamp" => json!("1970-01-01T00:00:00Z"),
        "google.protobuf.Duration" => json!("0s"),
        "google.protobuf.FieldMask" => json!(""),
        "google.protobuf.Struct" | "google.protobuf.Empty" => json!({}),
        "google.protobuf.Value" => Value::Null,
        "google.protobuf.ListValue" => json!([]),
        // `@type` must name a message known to the schema.
        "google.protobuf.Any" => match desc.parent_pool().get_message_by_name("google.protobuf.Empty") {
            Some(_) => json!({"@type": "type.googleapis.com/google.protobuf.Empty", "value": {}}),
            None => Value::Null,
        },
        "google.protobuf.DoubleValue" | "google.protobuf.FloatValue" => json!(0.0),
        "google.protobuf.Int32Value"
        | "google.protobuf.UInt32Value"
        | "google.protobuf.Int64Value"
        | "google.protobuf.UInt64Value" => json!(0),
        "google.protobuf.BoolValue" => json!(false),
        "google.protobuf.StringValue" | "google.protobuf.BytesValue" => json!(""),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    const PROTO: &str = r#"
syntax = "proto3";
package t;
import "google/protobuf/any.proto";

message Known { string note = 1; }
message Holder { google.protobuf.Any inner = 1; }
enum Mood { MOOD_UNSPECIFIED = 0; HAPPY = 1; }
message Envelope {
  string id = 1;
  google.protobuf.Any payload = 2;
  repeated google.protobuf.Any extra = 3;
  int64 big = 4;
  map<string, google.protobuf.Any> by_name = 5;
  float ratio = 6;
  Mood mood = 7;
  bytes raw = 8;
  Known known = 9;
  double nan = 10;
}
"#;

    fn schema() -> GrpcSchema {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.proto");
        std::fs::write(&path, PROTO).unwrap();
        load_proto_files(&[path], &[], None).unwrap()
    }

    fn any(desc: &MessageDescriptor, type_url: &str, value: Vec<u8>) -> prost_reflect::Value {
        let mut m = DynamicMessage::new(desc.clone());
        m.transcode_from(&prost_types::Any { type_url: type_url.into(), value }).unwrap();
        prost_reflect::Value::Message(m)
    }

    #[test]
    fn unknown_any_types_do_not_hide_the_message() {
        let schema = schema();
        let pool = schema.pool();
        let msg_of = |name: &str| DynamicMessage::new(pool.get_message_by_name(name).unwrap());
        let any_desc = pool.get_message_by_name("google.protobuf.Any").unwrap();
        let any = |type_url: &str, value: Vec<u8>| any(&any_desc, type_url, value);
        let mut known = msg_of("t.Known");
        known.set_field_by_name("note", prost_reflect::Value::String("hi".into()));
        let unknown = || any("type.googleapis.com/t.Missing", vec![8, 1]);
        let mut holder = msg_of("t.Holder");
        holder.set_field_by_name("inner", unknown());

        let mut env = msg_of("t.Envelope");
        env.set_field_by_name("id", prost_reflect::Value::String("e-1".into()));
        env.set_field_by_name("payload", unknown());
        env.set_field_by_name(
            "extra",
            prost_reflect::Value::List(vec![
                any("type.googleapis.com/t.Known", known.encode_to_vec()),
                any("type.googleapis.com/t.Holder", holder.encode_to_vec()),
                any(
                    "type.googleapis.com/google.protobuf.Any",
                    match unknown() {
                        prost_reflect::Value::Message(m) => m.encode_to_vec(),
                        _ => unreachable!(),
                    },
                ),
                any("type.googleapis.com/t.Known", vec![0xff]),
            ]),
        );
        env.set_field_by_name("big", prost_reflect::Value::I64(1 << 40));
        let mut map = std::collections::HashMap::new();
        map.insert(prost_reflect::MapKey::String("k".into()), unknown());
        env.set_field_by_name("by_name", prost_reflect::Value::Map(map));
        env.set_field_by_name("ratio", prost_reflect::Value::F32(0.1));
        env.set_field_by_name("mood", prost_reflect::Value::EnumNumber(1));
        env.set_field_by_name("raw", prost_reflect::Value::Bytes(vec![1, 2, 3].into()));
        env.set_field_by_name("nan", prost_reflect::Value::F64(f64::NAN));

        // prost-reflect alone (what this used to print) gives up on the whole message.
        assert!(reflect_value(&env).is_none());
        let text = message_to_json(&env);
        let mut v: Value = serde_json::from_str(&text).unwrap();
        // A known type whose bytes do not decode falls back the same way.
        let error = v["extra"][3]["@error"].as_str().unwrap_or_default().to_string();
        assert!(error.starts_with("t.Known could not be decoded") && error.ends_with("(base64)"), "{text}");
        v["extra"][3]["@error"] = json!("decode error");
        let placeholder = json!({
            "@type": "type.googleapis.com/t.Missing",
            "@error": "t.Missing is not in the schema; @value holds the raw message bytes (base64)",
            "@value": "CAE=",
        });
        assert_eq!(
            v,
            json!({
                "id": "e-1",
                "payload": placeholder,
                "extra": [
                    {"@type": "type.googleapis.com/t.Known", "note": "hi"},
                    {"@type": "type.googleapis.com/t.Holder", "inner": placeholder},
                    {"@type": "type.googleapis.com/google.protobuf.Any", "value": placeholder},
                    {"@type": "type.googleapis.com/t.Known", "@error": "decode error", "@value": "/w=="},
                ],
                "big": "1099511627776",
                "byName": {"k": placeholder},
                "ratio": 0.1,
                "mood": "HAPPY",
                "raw": "AQID",
                "nan": "NaN",
            }),
            "{text}"
        );
        // Field order and number formatting as prost-reflect writes them.
        assert!(text.starts_with("{\n  \"id\": \"e-1\",\n  \"payload\": {"), "{text}");
        assert!(text.contains("\"ratio\": 0.1,"), "{text}");
        // `known` is unset (message fields are only written when set).
        assert!(!text.contains("\"known\""), "{text}");

        // Without an unknown Any the output is prost-reflect's own.
        let mut plain = msg_of("t.Envelope");
        plain.set_field_by_name("payload", any("type.googleapis.com/t.Known", known.encode_to_vec()));
        let v: Value = serde_json::from_str(&message_to_json(&plain)).unwrap();
        assert_eq!(v["payload"], json!({"@type": "type.googleapis.com/t.Known", "note": "hi"}));
        assert_eq!(v["big"], json!("0"));
    }
}
