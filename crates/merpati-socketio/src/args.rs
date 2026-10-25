//! Emit arguments: JSON values with binary data anywhere inside, and the
//! conversion of saved message arguments ([`SocketIoArg`]) into them.

use std::path::Path;

use base64::Engine as _;
use merpati_core::protocols::{SocketIoArg, SocketIoArgKind, SocketIoMessage};
use serde_json::Value;

/// One argument of an emit or of an acknowledgement reply. Binary data may
/// sit anywhere inside arrays and objects: like the official client, the
/// packet then becomes a BINARY_EVENT / BINARY_ACK carrying the bytes as
/// attachments, which the server receives as `Buffer`s.
#[derive(Clone, Debug, PartialEq)]
pub enum EmitArg {
    Json(Value),
    Binary(Vec<u8>),
    /// An array whose items may hold binary data.
    Array(Vec<EmitArg>),
    /// An object whose values may hold binary data, in this key order.
    Object(Vec<(String, EmitArg)>),
}

impl EmitArg {
    pub fn has_binary(&self) -> bool {
        match self {
            EmitArg::Json(_) => false,
            EmitArg::Binary(_) => true,
            EmitArg::Array(items) => items.iter().any(EmitArg::has_binary),
            EmitArg::Object(entries) => entries.iter().any(|(_, a)| a.has_binary()),
        }
    }

    /// The argument as JSON with binary data as base64 strings, the form in
    /// which incoming attachments are reported.
    pub fn to_json(&self) -> Value {
        match self {
            EmitArg::Json(v) => v.clone(),
            EmitArg::Binary(b) => Value::String(base64::engine::general_purpose::STANDARD.encode(b)),
            EmitArg::Array(items) => Value::Array(items.iter().map(EmitArg::to_json).collect()),
            EmitArg::Object(entries) => Value::Object(entries.iter().map(|(k, a)| (k.clone(), a.to_json())).collect()),
        }
    }
}

impl From<Value> for EmitArg {
    fn from(v: Value) -> Self {
        EmitArg::Json(v)
    }
}

impl From<Vec<u8>> for EmitArg {
    fn from(b: Vec<u8>) -> Self {
        EmitArg::Binary(b)
    }
}

impl From<&[u8]> for EmitArg {
    fn from(b: &[u8]) -> Self {
        EmitArg::Binary(b.to_vec())
    }
}

/// Parses the legacy args text of a `SocketIoMessage`: a JSON array
/// (`["a", 1]`) gives its elements, any other JSON value is a single
/// argument, and text that is not JSON is sent as one string. Blank text
/// means no arguments.
pub fn parse_args(text: &str) -> Vec<Value> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(items)) => items,
        Ok(value) => vec![value],
        Err(_) => vec![Value::String(text.to_string())],
    }
}

/// A saved argument that cannot be sent; `index` is 0-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgError {
    InvalidJson {
        index: usize,
        message: String,
    },
    /// The text of a binary argument is neither hex nor base64.
    InvalidBinary {
        index: usize,
    },
    /// The file of a file argument cannot be read.
    File {
        index: usize,
        path: String,
        message: String,
    },
}

impl ArgError {
    pub fn index(&self) -> usize {
        match self {
            ArgError::InvalidJson { index, .. } | ArgError::InvalidBinary { index } | ArgError::File { index, .. } => {
                *index
            }
        }
    }
}

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArgError::InvalidJson { index, message } => write!(f, "Argument {}: invalid JSON ({message})", index + 1),
            ArgError::InvalidBinary { index } => {
                write!(f, "Argument {}: binary data must be hex or base64", index + 1)
            }
            ArgError::File { index, path, message } => {
                write!(f, "Argument {}: cannot read \"{path}\": {message}", index + 1)
            }
        }
    }
}

impl std::error::Error for ArgError {}

/// Converts one saved argument (already variable-resolved `value`).
/// Relative file paths are joined to `base_dir`.
pub fn resolve_arg(
    index: usize,
    kind: SocketIoArgKind,
    value: &str,
    base_dir: Option<&Path>,
) -> Result<EmitArg, ArgError> {
    match kind {
        SocketIoArgKind::Json => serde_json::from_str(value.trim())
            .map(EmitArg::Json)
            .map_err(|e| ArgError::InvalidJson { index, message: e.to_string() }),
        SocketIoArgKind::Text => Ok(EmitArg::Json(Value::String(value.to_string()))),
        SocketIoArgKind::Binary => {
            SocketIoArg::decode_binary(value).map(EmitArg::Binary).ok_or(ArgError::InvalidBinary { index })
        }
        SocketIoArgKind::File => {
            let path = Path::new(value.trim());
            let path = match base_dir {
                Some(dir) if path.is_relative() => dir.join(path),
                _ => path.to_path_buf(),
            };
            std::fs::read(&path).map(EmitArg::Binary).map_err(|e| ArgError::File {
                index,
                path: value.trim().to_string(),
                message: e.to_string(),
            })
        }
    }
}

/// Converts saved arguments into emit arguments. `resolve` substitutes
/// variables in every value; relative file paths are joined to `base_dir`
/// (the workspace folder).
pub fn resolve_args(
    args: &[SocketIoArg],
    resolve: &dyn Fn(&str) -> String,
    base_dir: Option<&Path>,
) -> Result<Vec<EmitArg>, ArgError> {
    args.iter().enumerate().map(|(i, a)| resolve_arg(i, a.kind, &resolve(&a.value), base_dir)).collect()
}

/// The arguments of a saved message: its `arguments`, or for messages saved
/// before those existed, its `args` text read like [`parse_args`] after
/// variable substitution.
pub fn message_args(
    msg: &SocketIoMessage,
    resolve: &dyn Fn(&str) -> String,
    base_dir: Option<&Path>,
) -> Result<Vec<EmitArg>, ArgError> {
    if msg.arguments.is_empty() {
        return Ok(parse_args(&resolve(&msg.args)).into_iter().map(EmitArg::Json).collect());
    }
    resolve_args(&msg.arguments, resolve, base_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn args_parsing() {
        assert_eq!(parse_args(r#"["a", 1, {"b": true}]"#), vec![json!("a"), json!(1), json!({"b": true})]);
        assert_eq!(parse_args(r#"{"x": 1}"#), vec![json!({"x": 1})]);
        assert_eq!(parse_args("42"), vec![json!(42)]);
        assert_eq!(parse_args(r#""quoted""#), vec![json!("quoted")]);
        assert_eq!(parse_args("hello world"), vec![json!("hello world")]);
        assert_eq!(parse_args("[]"), Vec::<Value>::new());
        assert_eq!(parse_args("   "), Vec::<Value>::new());
    }

    #[test]
    fn emit_args_report_binary_as_base64() {
        let a = EmitArg::Object(vec![
            ("name".into(), json!("x").into()),
            ("data".into(), EmitArg::Array(vec![vec![1u8, 2, 3].into(), json!(1).into()])),
        ]);
        assert!(a.has_binary());
        assert_eq!(a.to_json(), json!({"name": "x", "data": ["AQID", 1]}));
        assert!(!EmitArg::Array(vec![json!([1]).into()]).has_binary());
    }

    #[test]
    fn saved_arguments_are_resolved() {
        let dir = std::env::temp_dir().join(format!("merpati-sio-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 255, 7]).unwrap();
        let resolve = |s: &str| s.replace("{{n}}", "42").replace("{{hex}}", "c0ffee");
        let args = vec![
            SocketIoArg::new(SocketIoArgKind::Json, r#"{"n": {{n}}}"#),
            SocketIoArg::new(SocketIoArgKind::Text, " plain {{n}} "),
            SocketIoArg::new(SocketIoArgKind::Binary, "{{hex}}"),
            SocketIoArg::new(SocketIoArgKind::Binary, "AQID"),
            SocketIoArg::new(SocketIoArgKind::File, "blob.bin"),
        ];
        assert_eq!(
            resolve_args(&args, &resolve, Some(&dir)).unwrap(),
            vec![
                EmitArg::Json(json!({"n": 42})),
                EmitArg::Json(json!(" plain 42 ")),
                EmitArg::Binary(vec![0xc0, 0xff, 0xee]),
                EmitArg::Binary(vec![1, 2, 3]),
                EmitArg::Binary(vec![0, 255, 7]),
            ]
        );
        let abs = SocketIoArg::new(SocketIoArgKind::File, dir.join("blob.bin").display().to_string());
        assert_eq!(resolve_args(&[abs], &resolve, None).unwrap(), vec![EmitArg::Binary(vec![0, 255, 7])]);
        let _ = std::fs::remove_file(dir.join("blob.bin"));
        let _ = std::fs::remove_dir(&dir);

        let id = |s: &str| s.to_string();
        let bad = [SocketIoArg::new(SocketIoArgKind::Json, "1"), SocketIoArg::new(SocketIoArgKind::Binary, "zz!")];
        let err = resolve_args(&bad, &id, None).unwrap_err();
        assert_eq!((err.index(), err.to_string()), (1, "Argument 2: binary data must be hex or base64".to_string()));
        let err = resolve_args(&[SocketIoArg::new(SocketIoArgKind::Json, "{oops")], &id, None).unwrap_err();
        assert!(matches!(err, ArgError::InvalidJson { index: 0, .. }), "{err}");
        let err =
            resolve_args(&[SocketIoArg::new(SocketIoArgKind::File, "missing.bin")], &id, Some(Path::new("/nope")))
                .unwrap_err();
        assert!(err.to_string().starts_with("Argument 1: cannot read \"missing.bin\""), "{err}");

        // Messages saved before `arguments` existed keep their JSON text.
        let old = SocketIoMessage { args: r#"["a", {{n}}]"#.into(), ..Default::default() };
        assert_eq!(message_args(&old, &resolve, None).unwrap(), vec![json!("a").into(), json!(42).into()]);
        let new = SocketIoMessage {
            args: "ignored".into(),
            arguments: vec![SocketIoArg::new(SocketIoArgKind::Binary, "00ff")],
            ..Default::default()
        };
        assert_eq!(message_args(&new, &resolve, None).unwrap(), vec![EmitArg::Binary(vec![0, 255])]);
    }

    #[test]
    fn binary_text_is_hex_or_base64() {
        let d = SocketIoArg::decode_binary;
        assert_eq!(d("de ad\nbe ef"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(d("0xDEAD"), Some(vec![0xde, 0xad]));
        assert_eq!(d("aGk="), Some(b"hi".to_vec()));
        assert_eq!(d("aGk"), Some(b"hi".to_vec()));
        assert_eq!(d("-_8"), Some(vec![0xfb, 0xff]));
        assert_eq!(d(""), Some(Vec::new()));
        assert_eq!(d("abc"), Some(vec![0x69, 0xb7]));
        assert_eq!(d("not base64!"), None);
    }

    #[test]
    fn legacy_args_become_an_argument_list() {
        let list = |args: &str| SocketIoMessage { args: args.into(), ..Default::default() }.argument_list();
        let json = |v: &str| SocketIoArg::new(SocketIoArgKind::Json, v);
        assert_eq!(list(r#"["a", {"b": 1}, "{{x}}"]"#), vec![json(r#""a""#), json(r#"{"b":1}"#), json(r#""{{x}}""#)]);
        assert_eq!(list(r#" {"x": 1} "#), vec![json(r#"{"x": 1}"#)]);
        assert_eq!(list("hello world"), vec![SocketIoArg::new(SocketIoArgKind::Text, "hello world")]);
        assert_eq!(list("{{payload}}"), vec![json("{{payload}}")]);
        assert_eq!(list("  "), Vec::<SocketIoArg>::new());
        let m = SocketIoMessage {
            args: "[1]".into(),
            arguments: vec![SocketIoArg::new(SocketIoArgKind::Text, "t")],
            ..Default::default()
        };
        assert_eq!(m.argument_list(), m.arguments);
    }
}
