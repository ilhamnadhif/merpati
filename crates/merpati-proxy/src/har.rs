//! HAR 1.2 export of captured entries (tunnels are skipped). WebSocket
//! entries carry their messages like Chrome's DevTools export
//! (`_resourceType: "websocket"`, `_webSocketMessages`).

use base64::Engine as _;
use serde_json::{Value, json};

use crate::entry::{CaptureEntry, EntryKind, WsOpcode};

fn ms(d: Option<std::time::Duration>) -> f64 {
    d.map(|d| d.as_secs_f64() * 1000.0).unwrap_or(-1.0)
}

fn headers(list: &[(String, String)]) -> Value {
    Value::Array(list.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect())
}

fn query(url: &str) -> Value {
    let pairs = url::Url::parse(url)
        .map(|u| u.query_pairs().map(|(k, v)| json!({ "name": k, "value": v })).collect())
        .unwrap_or_default();
    Value::Array(pairs)
}

/// Text and binary messages as Chrome writes them: `type` `send`/`receive`,
/// `time` in epoch seconds, `opcode` 1 or 2, binary `data` in base64.
/// Control frames are left out, as in Chrome.
fn websocket_messages(e: &CaptureEntry) -> Value {
    // Message times count from the upgrade, which follows the 101 response.
    let upgraded = e.started_at.timestamp_micros() as f64 / 1e6 + e.timings.ttfb.unwrap_or_default().as_secs_f64();
    let list = e.ws_messages.iter().filter_map(|m| {
        let (opcode, data) = match m.opcode {
            WsOpcode::Text => (1, m.text().into_owned()),
            WsOpcode::Binary => (2, base64::engine::general_purpose::STANDARD.encode(&m.data)),
            WsOpcode::Close | WsOpcode::Ping | WsOpcode::Pong => return None,
        };
        Some(json!({
            "type": if m.from_client { "send" } else { "receive" },
            "time": upgraded + m.at.as_secs_f64(),
            "opcode": opcode,
            "data": data,
        }))
    });
    Value::Array(list.collect())
}

fn entry(e: &CaptureEntry) -> Value {
    let mut request = json!({
        "method": e.method,
        "url": e.url,
        "httpVersion": e.http_version,
        "cookies": [],
        "headers": headers(&e.request_headers),
        "queryString": query(&e.url),
        "headersSize": -1,
        "bodySize": e.request_body.size,
    });
    if e.request_body.size > 0 {
        let decoded = e.decoded_request_body();
        let mime = e.request_header("content-type").unwrap_or("application/octet-stream");
        request["postData"] = match decoded.text() {
            Some(t) => json!({ "mimeType": mime, "text": t }),
            None => json!({
                "mimeType": mime,
                "text": base64::engine::general_purpose::STANDARD.encode(&decoded.data),
                "_encoding": "base64",
            }),
        };
    }
    let decoded = e.decoded_response_body();
    // The length of the whole (decoded) body: without a content encoding
    // that is the size on the wire, even when only a prefix was kept.
    let size = if decoded.encoding.is_none() { e.response_body.size } else { decoded.data.len() as u64 };
    let mut content = json!({
        "size": size,
        "mimeType": e.response_header("content-type").unwrap_or(""),
    });
    if !decoded.data.is_empty() {
        match decoded.text() {
            Some(t) => content["text"] = json!(t),
            None => {
                content["text"] = json!(base64::engine::general_purpose::STANDARD.encode(&decoded.data));
                content["encoding"] = json!("base64");
            }
        }
    }
    let t = &e.timings;
    let connect = t.connect.unwrap_or_default();
    let ttfb = t.ttfb.unwrap_or_default();
    let total = t.total.or(t.ttfb).unwrap_or_default();
    let mut har = json!({
        "startedDateTime": e.started_at.to_rfc3339(),
        "time": total.as_secs_f64() * 1000.0,
        "request": request,
        "response": {
            "status": e.status.unwrap_or(0),
            "statusText": e.status_text(),
            "httpVersion": e.upstream_http_version.as_deref().unwrap_or(&e.http_version),
            "cookies": [],
            "headers": headers(&e.response_headers),
            "content": content,
            "redirectURL": e.response_header("location").unwrap_or(""),
            "headersSize": -1,
            "bodySize": e.response_body.size,
            "_error": e.error,
        },
        "cache": {},
        "timings": {
            "blocked": -1,
            "dns": -1,
            "connect": ms(t.connect),
            "ssl": ms(t.tls_handshake),
            "send": 0,
            "wait": (ttfb.saturating_sub(connect)).as_secs_f64() * 1000.0,
            "receive": (total.saturating_sub(ttfb)).as_secs_f64() * 1000.0,
        },
        "_merpati": {
            "id": e.id,
            "clientAddress": e.client_addr.to_string(),
            "tlsIntercepted": e.tls_intercepted,
            "requestBodyTruncated": e.request_body.truncated,
            "responseBodyTruncated": e.response_body.truncated,
            "websocket": e.kind == EntryKind::WebSocket,
        },
    });
    if e.kind == EntryKind::WebSocket {
        har["_resourceType"] = json!("websocket");
        har["_webSocketMessages"] = websocket_messages(e);
        if e.ws_messages_dropped > 0 {
            har["_merpati"]["webSocketMessagesDropped"] = json!(e.ws_messages_dropped);
        }
    }
    har
}

/// A HAR 1.2 document for `entries` (tunnels are skipped). WebSocket entries
/// carry their text/binary messages in `_webSocketMessages`, as Chrome does.
pub fn to_har<'a>(entries: impl IntoIterator<Item = &'a CaptureEntry>) -> Value {
    let list: Vec<Value> = entries.into_iter().filter(|e| e.kind != EntryKind::Tunnel).map(entry).collect();
    json!({
        "log": {
            "version": "1.2",
            "creator": { "name": "Merpati", "version": env!("CARGO_PKG_VERSION") },
            "entries": list,
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::time::Duration;

    use bytes::Bytes;

    use super::*;
    use crate::entry::{CapturedBody, Timings, WsMessage};

    fn entry(headers: &[(&str, &str)], body: CapturedBody) -> CaptureEntry {
        let mut e = CaptureEntry::new(1, "127.0.0.1:1".parse().unwrap(), EntryKind::Http);
        e.method = "GET".into();
        e.url = "http://api.test/x?a=1".into();
        e.status = Some(200);
        e.response_headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        e.response_body = body;
        e.timings = Timings {
            connect: Some(Duration::from_millis(20)),
            tls_handshake: None,
            ttfb: Some(Duration::from_millis(50)),
            total: Some(Duration::from_millis(80)),
            reused_connection: false,
        };
        e
    }

    #[test]
    fn content_size_is_the_whole_decoded_body() {
        // Only the first MiB of a 10 MB plain body was kept.
        let kept = Bytes::from(vec![b'a'; 1 << 20]);
        let e =
            entry(&[("content-type", "text/plain")], CapturedBody { data: kept, size: 10_000_000, truncated: true });
        let har = to_har([&e]);
        let resp = &har["log"]["entries"][0]["response"];
        assert_eq!(resp["bodySize"], 10_000_000);
        assert_eq!(resp["content"]["size"], 10_000_000);
        let t = &har["log"]["entries"][0]["timings"];
        assert_eq!(
            (t["connect"].as_f64(), t["wait"].as_f64(), t["receive"].as_f64()),
            (Some(20.0), Some(30.0), Some(30.0))
        );
        assert_eq!(har["log"]["entries"][0]["time"].as_f64(), Some(80.0));

        // Compressed: the decoded length.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&[b'z'; 5000]).unwrap();
        let gz = gz.finish().unwrap();
        let size = gz.len() as u64;
        let e = entry(&[("content-encoding", "gzip")], CapturedBody { data: Bytes::from(gz), size, truncated: false });
        let har = to_har([&e]);
        let resp = &har["log"]["entries"][0]["response"];
        assert_eq!(resp["bodySize"], size);
        assert_eq!(resp["content"]["size"], 5000);
        assert_eq!(resp["content"]["text"].as_str().map(str::len), Some(5000));
    }

    #[test]
    fn websocket_messages_in_chrome_format() {
        let mut e = entry(&[("upgrade", "websocket")], CapturedBody::default());
        e.kind = EntryKind::WebSocket;
        e.status = Some(101);
        let msg = |from_client, opcode, at_ms, data: &[u8]| WsMessage {
            from_client,
            opcode,
            at: Duration::from_millis(at_ms),
            size: data.len() as u64,
            data: Bytes::copy_from_slice(data),
            truncated: false,
            compressed: false,
        };
        e.ws_messages = vec![
            msg(true, WsOpcode::Text, 10, "héllo".as_bytes()),
            msg(false, WsOpcode::Ping, 20, b""),
            msg(false, WsOpcode::Binary, 1500, &[0, 1, 0xff]),
            msg(true, WsOpcode::Close, 2000, &[3, 232]),
        ];
        let har = to_har([&e]);
        let ws = &har["log"]["entries"][0];
        assert_eq!(ws["_resourceType"], "websocket");
        let msgs = ws["_webSocketMessages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "control frames are left out: {msgs:?}");
        assert_eq!(
            (&msgs[0]["type"], &msgs[0]["opcode"], &msgs[0]["data"]),
            (&json!("send"), &json!(1), &json!("héllo"))
        );
        assert_eq!(
            (&msgs[1]["type"], &msgs[1]["opcode"], &msgs[1]["data"]),
            (&json!("receive"), &json!(2), &json!("AAH/"))
        );
        // Epoch seconds: start + time to the 101 response + offset after the upgrade.
        let start = e.started_at.timestamp_micros() as f64 / 1e6;
        let t0 = msgs[0]["time"].as_f64().unwrap();
        assert!((t0 - (start + 0.050 + 0.010)).abs() < 1e-3, "{t0} vs {start}");
        assert!((msgs[1]["time"].as_f64().unwrap() - t0 - 1.49).abs() < 1e-3);

        // Plain HTTP entries carry neither.
        let plain = to_har([&entry(&[], CapturedBody::default())]);
        assert!(plain["log"]["entries"][0].get("_webSocketMessages").is_none());
        assert!(plain["log"]["entries"][0].get("_resourceType").is_none());
    }
}
