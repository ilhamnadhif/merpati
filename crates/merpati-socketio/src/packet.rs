//! Socket.IO packet codec, protocol v5 (Socket.IO v3/v4) and v4
//! (Socket.IO v2). Both share the text format
//! `<type>[<attachments>-][<namespace>[?query],][<id>][<json>]`; they differ
//! in what the packets carry (see the session layer).

use base64::Engine as _;
use serde_json::{Value, json};

use crate::EmitArg;

pub(crate) const CONNECT: u8 = 0;
pub(crate) const DISCONNECT: u8 = 1;
pub(crate) const EVENT: u8 = 2;
pub(crate) const ACK: u8 = 3;
/// `CONNECT_ERROR` in protocol v5, `ERROR` in v4.
pub(crate) const CONNECT_ERROR: u8 = 4;
pub(crate) const BINARY_EVENT: u8 = 5;
pub(crate) const BINARY_ACK: u8 = 6;

#[derive(Debug, PartialEq)]
pub(crate) struct Packet {
    pub kind: u8,
    /// Without a `?query` suffix (protocol v4 CONNECT packets may carry one).
    pub namespace: String,
    pub id: Option<u64>,
    pub attachments: usize,
    pub data: Option<Value>,
}

/// Decodes `<type>[<attachments>-][<namespace>,][<id>][<json>]`.
pub(crate) fn decode_packet(s: &str) -> Result<Packet, String> {
    let bytes = s.as_bytes();
    let kind = match bytes.first() {
        Some(c @ b'0'..=b'6') => c - b'0',
        _ => return Err(format!("unknown Socket.IO packet \"{}\"", truncate(s))),
    };
    let mut i = 1;
    let digits = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let mut attachments = 0;
    if kind == BINARY_EVENT || kind == BINARY_ACK {
        let end = digits(i);
        if end == i || bytes.get(end) != Some(&b'-') {
            return Err(format!("malformed binary packet \"{}\"", truncate(s)));
        }
        attachments = s[i..end].parse().map_err(|_| "too many attachments".to_string())?;
        i = end + 1;
    }
    let mut namespace = "/".to_string();
    if bytes.get(i) == Some(&b'/') {
        let start = i;
        while i < bytes.len() && bytes[i] != b',' {
            i += 1;
        }
        namespace = s[start..i].to_string();
        if let Some(q) = namespace.find('?') {
            namespace.truncate(q);
        }
        if i < bytes.len() {
            i += 1;
        }
    }
    let end = digits(i);
    let id = if end > i { Some(s[i..end].parse().map_err(|_| "ack id out of range".to_string())?) } else { None };
    let rest = &s[end..];
    let data = if rest.is_empty() {
        None
    } else {
        Some(serde_json::from_str(rest).map_err(|e| format!("invalid Socket.IO payload: {e}"))?)
    };
    Ok(Packet { kind, namespace, id, attachments, data })
}

/// Encodes a packet (without the Engine.IO message type). `query` is the
/// protocol v4 CONNECT query string (`/admin?token=x,`).
pub(crate) fn encode_packet(kind: u8, namespace: &str, query: &str, id: Option<u64>, data: Option<&Value>) -> String {
    encode(kind, 0, namespace, query, id, data)
}

/// Encodes an EVENT (`data` = `[name, ...args]`) or ACK packet. With binary
/// data it becomes a BINARY_EVENT / BINARY_ACK whose attachments must follow
/// it as binary messages, in order.
pub(crate) fn encode_data_packet(
    kind: u8,
    namespace: &str,
    id: Option<u64>,
    data: &[EmitArg],
) -> (String, Vec<Vec<u8>>) {
    let (json, attachments) = deconstruct(data);
    let kind = match (kind, attachments.is_empty()) {
        (EVENT, false) => BINARY_EVENT,
        (ACK, false) => BINARY_ACK,
        (kind, _) => kind,
    };
    (encode(kind, attachments.len(), namespace, "", id, Some(&Value::Array(json))), attachments)
}

fn encode(kind: u8, attachments: usize, namespace: &str, query: &str, id: Option<u64>, data: Option<&Value>) -> String {
    let mut s = String::new();
    s.push(char::from(b'0' + kind));
    if kind == BINARY_EVENT || kind == BINARY_ACK {
        s.push_str(&attachments.to_string());
        s.push('-');
    }
    if namespace != "/" {
        s.push_str(namespace);
        if !query.is_empty() {
            s.push('?');
            s.push_str(query);
        }
        s.push(',');
    }
    if let Some(id) = id {
        s.push_str(&id.to_string());
    }
    if let Some(data) = data {
        s.push_str(&data.to_string());
    }
    s
}

pub(crate) fn truncate(s: &str) -> String {
    match s.char_indices().nth(80) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}

/// Replaces the binary data in `args` with `{"_placeholder": true, "num": n}`
/// and returns it as the attachments, numbered depth-first like the official
/// client does.
pub(crate) fn deconstruct(args: &[EmitArg]) -> (Vec<Value>, Vec<Vec<u8>>) {
    fn walk(arg: &EmitArg, out: &mut Vec<Vec<u8>>) -> Value {
        match arg {
            EmitArg::Json(v) => v.clone(),
            EmitArg::Binary(b) => {
                out.push(b.clone());
                json!({"_placeholder": true, "num": out.len() - 1})
            }
            EmitArg::Array(items) => Value::Array(items.iter().map(|a| walk(a, out)).collect()),
            EmitArg::Object(entries) => Value::Object(entries.iter().map(|(k, a)| (k.clone(), walk(a, out))).collect()),
        }
    }
    let mut attachments = Vec::new();
    let json = args.iter().map(|a| walk(a, &mut attachments)).collect();
    (json, attachments)
}

/// Replaces `{"_placeholder": true, "num": n}` with attachment `n` as base64.
pub(crate) fn fill_placeholders(v: &mut Value, attachments: &[Vec<u8>]) {
    if let Value::Object(o) = v
        && o.get("_placeholder") == Some(&Value::Bool(true))
    {
        let bin = o.get("num").and_then(Value::as_u64).and_then(|n| attachments.get(n as usize));
        if let Some(bin) = bin {
            *v = Value::String(base64::engine::general_purpose::STANDARD.encode(bin));
        }
        return;
    }
    match v {
        Value::Array(items) => items.iter_mut().for_each(|x| fill_placeholders(x, attachments)),
        Value::Object(o) => o.values_mut().for_each(|x| fill_placeholders(x, attachments)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn packet_round_trips() {
        let p = decode_packet(r#"2/admin,12["hello",{"a":1}]"#).unwrap();
        assert_eq!(
            p,
            Packet {
                kind: 2,
                namespace: "/admin".into(),
                id: Some(12),
                attachments: 0,
                data: Some(json!(["hello", {"a": 1}]))
            }
        );
        assert_eq!(decode_packet("0").unwrap().data, None);
        assert_eq!(decode_packet(r#"0{"sid":"x"}"#).unwrap().data, Some(json!({"sid": "x"})));
        assert_eq!(decode_packet("1/chat,").unwrap().namespace, "/chat");
        assert_eq!(decode_packet("1/chat").unwrap().namespace, "/chat");
        assert_eq!(decode_packet("0/chat?token=1,").unwrap().namespace, "/chat");
        assert_eq!(decode_packet(r#"37["ok"]"#).unwrap().id, Some(7));
        assert_eq!(decode_packet(r#"4"Not authorized""#).unwrap().data, Some(json!("Not authorized")));
        let b = decode_packet(r#"52-/x,3["f",{"_placeholder":true,"num":0},{"_placeholder":true,"num":1}]"#).unwrap();
        assert_eq!((b.kind, b.attachments, b.namespace.as_str(), b.id), (5, 2, "/x", Some(3)));
        assert!(decode_packet("9").is_err());
        assert!(decode_packet("5[]").is_err());
        assert!(decode_packet("2[oops").is_err());

        assert_eq!(encode_packet(CONNECT, "/", "", None, None), "0");
        assert_eq!(
            encode_packet(CONNECT, "/admin", "", None, Some(&json!({"token": "t"}))),
            r#"0/admin,{"token":"t"}"#
        );
        assert_eq!(encode_packet(CONNECT, "/admin", "token=t&x=1", None, None), "0/admin?token=t&x=1,");
        assert_eq!(encode_packet(EVENT, "/", "", Some(5), Some(&json!(["e", 1]))), r#"25["e",1]"#);
        assert_eq!(encode_packet(DISCONNECT, "/admin", "", None, None), "1/admin,");
        assert_eq!(encode_packet(ACK, "/a", "", Some(3), Some(&json!(["ok"]))), r#"3/a,3["ok"]"#);
    }

    #[test]
    fn binary_packets_use_numbered_placeholders() {
        // No binary data: a plain EVENT / ACK.
        let (text, bins) = encode_data_packet(EVENT, "/", Some(1), &[json!("e").into(), json!({"a": 1}).into()]);
        assert_eq!((text.as_str(), bins.len()), (r#"21["e",{"a":1}]"#, 0));
        let (text, bins) = encode_data_packet(ACK, "/x", Some(4), &[]);
        assert_eq!((text.as_str(), bins.len()), ("3/x,4[]", 0));

        // Depth-first numbering through arrays and objects, like the official client.
        let data = [
            json!("upload").into(),
            EmitArg::Binary(vec![0, 1]),
            EmitArg::Object(vec![
                ("name".into(), json!("a.bin").into()),
                ("parts".into(), EmitArg::Array(vec![EmitArg::Binary(vec![2]), json!(null).into(), vec![3u8].into()])),
            ]),
            EmitArg::Binary(Vec::new()),
        ];
        let (text, bins) = encode_data_packet(EVENT, "/", None, &data);
        let p = r#"{"_placeholder":true,"num":"#;
        assert_eq!(text, format!(r#"54-["upload",{p}0}},{{"name":"a.bin","parts":[{p}1}},null,{p}2}}]}},{p}3}}]"#));
        assert_eq!(bins, vec![vec![0, 1], vec![2], vec![3], vec![]]);
        let decoded = decode_packet(&text).unwrap();
        assert_eq!((decoded.kind, decoded.attachments), (BINARY_EVENT, 4));

        // Namespace and ack id come after the attachment count.
        let (text, bins) = encode_data_packet(EVENT, "/admin", Some(7), &[json!("f").into(), b"hi"[..].into()]);
        assert_eq!(text, format!(r#"51-/admin,7["f",{p}0}}]"#));
        assert_eq!(bins, vec![b"hi".to_vec()]);
        let (text, bins) =
            encode_data_packet(ACK, "/", Some(12), &[EmitArg::Object(vec![("file".into(), vec![9u8].into())])]);
        assert_eq!(text, format!(r#"61-12[{{"file":{p}0}}}}]"#));
        assert_eq!(bins, vec![vec![9]]);
        let (text, _) = encode_data_packet(ACK, "/chat", Some(3), &[vec![1u8].into(), vec![2u8].into()]);
        assert_eq!(text, format!(r#"62-/chat,3[{p}0}},{p}1}}]"#));

        // What the receiving side reconstructs.
        let mut v = decode_packet(&text).unwrap().data.unwrap();
        fill_placeholders(&mut v, &[vec![1], vec![2]]);
        assert_eq!(v, json!(["AQ==", "Ag=="]));
    }

    #[test]
    fn placeholders_become_base64() {
        let mut v = json!(["f", {"_placeholder": true, "num": 0}, {"nested": [{"_placeholder": true, "num": 1}]}]);
        fill_placeholders(&mut v, &[vec![1, 2, 3], b"hi".to_vec()]);
        assert_eq!(v, json!(["f", "AQID", {"nested": ["aGk="]}]));
    }
}
