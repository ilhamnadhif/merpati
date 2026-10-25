//! Akamai EdgeGrid (EG1-HMAC-SHA256).
//!
//! Matches Akamai's signers (edgegrid-python's test data, shared with the
//! perl, ruby and php signers, plus node and php extras, is vendored in
//! `tests/fixtures/edgegrid`) with Postman's parameter names:
//! - signed data: method, scheme, host, `path?query`, canonical headers,
//!   content hash and the unsigned header, joined by tabs; the key is the
//!   base64 HMAC-SHA256 of the timestamp under the client secret;
//! - host: the `baseURL` host when set (Postman), else an explicit `Host`
//!   header (edgegrid-python), else the URL's `host[:port]`;
//! - `headersToSign` (comma separated) in the configured order, as
//!   `name:value` with the name lowercased and the value trimmed and its
//!   whitespace runs collapsed; missing or empty headers are skipped;
//! - content hash for POST only: base64 SHA-256 of the first `maxBodySize`
//!   bytes (default 131072), empty for an empty body.

use std::io::Read;

use base64::Engine;
use serde_json::Value;

use merpati_core::{PreparedBody, PreparedRequest};

use crate::error::AuthError;
use crate::util::{B64, hmac_sha256, host_header, param, parse_url, request_target, sha256};

const DEFAULT_MAX_BODY: usize = 131_072;

pub(crate) struct Input<'a> {
    pub client_token: &'a str,
    pub access_token: &'a str,
    pub client_secret: &'a str,
    pub timestamp: &'a str,
    pub nonce: &'a str,
    pub method: &'a str,
    pub scheme: &'a str,
    /// `host[:port]` as signed.
    pub host: &'a str,
    /// Path and query as sent.
    pub path: &'a str,
    /// (name, value) of the headers to sign, in configured order.
    pub headers: Vec<(String, String)>,
    /// The body, already cut to the maximum size; hashed for POST only.
    pub body: &'a [u8],
}

/// The tab-separated string the signature covers.
pub(crate) fn data_to_sign(i: &Input<'_>, auth_header: &str) -> String {
    let canonical_headers: Vec<String> = i
        .headers
        .iter()
        .map(|(k, v)| {
            format!("{}:{}", k.trim().to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" "))
        })
        .collect();
    let content_hash = if i.method.eq_ignore_ascii_case("POST") && !i.body.is_empty() {
        B64.encode(sha256(i.body))
    } else {
        String::new()
    };
    [
        &i.method.to_ascii_uppercase(),
        i.scheme,
        i.host,
        i.path,
        &canonical_headers.join("\t"),
        &content_hash,
        auth_header,
    ]
    .join("\t")
}

pub(crate) fn signing_key(client_secret: &str, timestamp: &str) -> String {
    B64.encode(hmac_sha256(client_secret.as_bytes(), timestamp.as_bytes()))
}

pub(crate) fn header_value(i: &Input<'_>) -> String {
    let base = format!(
        "EG1-HMAC-SHA256 client_token={};access_token={};timestamp={};nonce={};",
        i.client_token, i.access_token, i.timestamp, i.nonce
    );
    let key = signing_key(i.client_secret, i.timestamp);
    let signature = B64.encode(hmac_sha256(key.as_bytes(), data_to_sign(i, &base).as_bytes()));
    format!("{base}signature={signature}")
}

/// Names from `headersToSign`: a comma separated string (spaces and
/// semicolons also separate) or a list.
fn headers_to_sign(v: Option<&Value>) -> Vec<String> {
    let names: Vec<String> = match v {
        Some(Value::String(s)) => s.split([',', ';', ' ', '\t', '\n']).map(str::to_string).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    };
    names.iter().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect()
}

/// Host signed in place of the URL's: the host of `baseURL` (with or
/// without a scheme).
fn base_url_host(base: &str) -> Result<String, AuthError> {
    let full = if base.contains("://") { base.to_string() } else { format!("https://{base}") };
    match url::Url::parse(&full) {
        Ok(u) if u.host_str().is_some_and(|h| !h.is_empty()) => Ok(host_header(&u)),
        _ => Err(AuthError::invalid(format!("Akamai EdgeGrid: invalid Base URL `{base}`"))),
    }
}

/// Up to `max` bytes of the body, reading files from disk. Multipart bodies
/// are built (with a random boundary) only when sending, so like Postman
/// they get no content hash.
fn body_prefix(body: &PreparedBody, max: usize) -> Result<Vec<u8>, AuthError> {
    Ok(match body {
        PreparedBody::None | PreparedBody::Multipart(_) => Vec::new(),
        PreparedBody::Bytes(b) => b[..b.len().min(max)].to_vec(),
        PreparedBody::File(path) => {
            let mut out = Vec::new();
            std::fs::File::open(path)?.take(max as u64).read_to_end(&mut out)?;
            out
        }
    })
}

pub(crate) fn apply(req: &mut PreparedRequest) -> Result<(), AuthError> {
    let auth = req.auth.clone();
    for field in ["clientToken", "accessToken", "clientSecret"] {
        if param(&auth, field).is_empty() {
            return Err(AuthError::Missing { scheme: "Akamai EdgeGrid", field });
        }
    }
    let url = parse_url(req)?;
    let timestamp = match param(&auth, "timestamp") {
        "" => chrono::Utc::now().format("%Y%m%dT%H:%M:%S+0000").to_string(),
        t => t.to_string(),
    };
    let nonce = match param(&auth, "nonce") {
        "" => uuid::Uuid::new_v4().to_string(),
        n => n.to_string(),
    };
    let host = match (param(&auth, "baseURL"), req.header("host").map(str::trim)) {
        ("", Some(h)) if !h.is_empty() => h.to_string(),
        ("", _) => host_header(&url),
        (base, _) => base_url_host(base)?,
    };
    let headers: Vec<(String, String)> = headers_to_sign(auth.params.get("headersToSign"))
        .into_iter()
        .filter_map(|h| req.header(&h).filter(|v| !v.trim().is_empty()).map(|v| (h.clone(), v.to_string())))
        .collect();
    let max_body =
        auth.get_string("maxBodySize").trim().parse().ok().filter(|&n: &usize| n > 0).unwrap_or(DEFAULT_MAX_BODY);
    let body = if req.method.eq_ignore_ascii_case("POST") { body_prefix(&req.body, max_body)? } else { Vec::new() };
    let value = header_value(&Input {
        client_token: param(&auth, "clientToken"),
        access_token: param(&auth, "accessToken"),
        client_secret: param(&auth, "clientSecret"),
        timestamp: &timestamp,
        nonce: &nonce,
        method: &req.method,
        scheme: url.scheme(),
        host: &host,
        path: &request_target(&url),
        headers,
        body: &body,
    });
    req.set_header("Authorization", value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::{Auth, AuthType};

    const TESTDATA: &str = include_str!("../tests/fixtures/edgegrid/testdata.json");
    const TESTCASES: &str = include_str!("../tests/fixtures/edgegrid/testcases.json");
    const EXTRA: &str = include_str!("../tests/fixtures/edgegrid/extra_cases.json");

    fn data() -> Value {
        serde_json::from_str(TESTDATA).unwrap()
    }

    /// EdgeGrid auth with the shared test credentials.
    fn auth(d: &Value) -> Auth {
        Auth::new(AuthType::EdgeGrid)
            .with("clientToken", d["client_token"].clone())
            .with("accessToken", d["access_token"].clone())
            .with("clientSecret", d["client_secret"].clone())
            .with("timestamp", d["timestamp"].clone())
            .with("nonce", d["nonce"].clone())
    }

    /// Request for an Akamai test case (`request.{method,path,headers,data,query}`).
    fn request(base_url: &str, case: &Value, auth: Auth) -> PreparedRequest {
        let r = &case["request"];
        let mut url = url::Url::parse(base_url).unwrap().join(r["path"].as_str().unwrap()).unwrap().to_string();
        if let Some(q) = r["query"].as_object() {
            let mut u = url::Url::parse(&url).unwrap();
            for (k, v) in q {
                u.query_pairs_mut().append_pair(k, v.as_str().unwrap());
            }
            url = u.to_string();
        }
        let headers = r["headers"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|h| h.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())))
            .collect();
        let body = match &r["data"] {
            Value::String(s) => PreparedBody::Bytes(s.clone().into()),
            _ => PreparedBody::None,
        };
        PreparedRequest { method: r["method"].as_str().unwrap().into(), url, headers, body, auth, ..Default::default() }
    }

    fn signed(req: &mut PreparedRequest) -> String {
        apply(req).unwrap();
        req.header("authorization").unwrap().to_string()
    }

    #[test]
    fn akamai_testcases() {
        // edgegrid-python testcases.json (also in edgegrid-perl/-ruby/-php
        // testdata.json): headers_to_sign X-Test1..3, max_body 2048. The
        // "leading and interior spaces" case is rejected by python-requests
        // before signing; its signature is the one the other signers expect.
        let d = data();
        let cases: Vec<Value> = serde_json::from_str(TESTCASES).unwrap();
        assert_eq!(cases.len(), 15);
        let base = d["base_url"].as_str().unwrap();
        for case in &cases {
            let name = case["testName"].as_str().unwrap();
            let a =
                auth(&d).with("headersToSign", d["headers_to_sign"].clone()).with("maxBodySize", d["max_body"].clone());
            let mut req = request(base, case, a);
            assert_eq!(signed(&mut req), case["expectedAuthorization"].as_str().unwrap(), "{name}");
            if let Some(expected) = case["expectedDataToSign"].as_str() {
                // The python tests compare the data with an empty auth header.
                let url = parse_url(&req).unwrap();
                let names = headers_to_sign(Some(&d["headers_to_sign"]));
                let headers =
                    names.into_iter().filter_map(|h| req.header(&h).map(|v| (h.clone(), v.to_string()))).collect();
                let body = body_prefix(&req.body, 2048).unwrap();
                let input = Input {
                    client_token: "",
                    access_token: "",
                    client_secret: "",
                    timestamp: "",
                    nonce: "",
                    method: &req.method,
                    scheme: url.scheme(),
                    host: &host_header(&url),
                    path: &request_target(&url),
                    headers,
                    body: &body,
                };
                assert_eq!(data_to_sign(&input, ""), expected, "{name}: data to sign");
            }
        }
    }

    #[test]
    fn akamai_testdata_extras() {
        let d = data();
        assert_eq!(
            signing_key(d["client_secret"].as_str().unwrap(), d["timestamp"].as_str().unwrap()),
            d["sign_key_test"]
        );
        assert_eq!(B64.encode(sha256(b"test_body")), d["content_hash_test"]);
        let base = url::Url::parse(d["base_url"].as_str().unwrap()).unwrap();
        // requests' json= body for {"key": "value"} with params extended=true.
        let mut req = PreparedRequest {
            method: "POST".into(),
            url: base.join("/testapi/v1/t3?extended=true").unwrap().to_string(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: PreparedBody::Bytes(r#"{"key": "value"}"#.into()),
            auth: auth(&d),
            ..Default::default()
        };
        assert_eq!(signed(&mut req), d["jsontest_hash"]);
        // requests_toolbelt MultipartEncoder body with boundary "multipart_boundary".
        let sample = include_str!("../tests/fixtures/edgegrid/sample_file.txt");
        let multipart = format!(
            "--multipart_boundary\r\nContent-Disposition: form-data; name=\"foo\"\r\n\r\nbar\r\n\
             --multipart_boundary\r\nContent-Disposition: form-data; name=\"baz\"; filename=\"sample_file.txt\"\r\n\
             \r\n{sample}\r\n--multipart_boundary--\r\n"
        );
        req.body = PreparedBody::Bytes(multipart.into());
        assert_eq!(signed(&mut req), d["multipart_hash_test"]);
    }

    #[test]
    fn extra_cases() {
        // Cases from edgegrid-node's test.js, edgegrid-php's testdata.json and
        // edgegrid-python 2.0.8 run on inputs of our own (see the fixture's README).
        let d = data();
        let doc: Value = serde_json::from_str(EXTRA).unwrap();
        let cases = doc["cases"].as_array().unwrap();
        for case in cases {
            let name = case["testName"].as_str().unwrap();
            let mut a = auth(&d);
            for (k, v) in case["auth"].as_object().into_iter().flatten() {
                a = a.with(k, v.clone());
            }
            let mut req = request(case["base_url"].as_str().unwrap_or(d["base_url"].as_str().unwrap()), case, a);
            if let Some(n) = case["request"]["zero_body_len"].as_u64() {
                req.body = PreparedBody::Bytes(vec![0u8; n as usize].into());
            }
            if let Some(hex_body) = case["request"]["data_hex"].as_str() {
                req.body = PreparedBody::Bytes(hex::decode(hex_body).unwrap().into());
            }
            assert_eq!(signed(&mut req), case["expectedAuthorization"].as_str().unwrap(), "{name}");
        }
    }

    #[test]
    fn file_bodies_are_hashed_up_to_max_body() {
        let d = data();
        let dir = std::env::temp_dir().join(format!("merpati-edgegrid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("body.bin");
        std::fs::write(&path, vec![0u8; 200_000]).unwrap();
        let case = serde_json::json!({"request": {"method": "POST", "path": "/testapi/v1/t3"}});
        let mut req = request(d["base_url"].as_str().unwrap(), &case, auth(&d));
        req.body = PreparedBody::File(path.clone());
        // edgegrid-python 2.0.8 for the same 200000 zero bytes (128 KiB hashed).
        assert!(signed(&mut req).ends_with("signature=doyTK5K27fYWW6EQLA+beZrE4opirExc90qGNQRRNq0="));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn host_selection_and_missing_fields() {
        let d = data();
        let case = serde_json::json!({"request": {"method": "GET", "path": "/"}});
        let mut plain = request(d["base_url"].as_str().unwrap(), &case, auth(&d));
        let expected = signed(&mut plain);
        // baseURL (Postman) wins over the URL host, with or without a scheme.
        for base in [
            "https://akaa-baseurl-xxxxxxxxxxx-xxxxxxxxxxxxx.luna.akamaiapis.net/",
            "akaa-baseurl-xxxxxxxxxxx-xxxxxxxxxxxxx.luna.akamaiapis.net",
        ] {
            let mut req = request("https://127.0.0.1/", &case, auth(&d).with("baseURL", base));
            assert_eq!(signed(&mut req), expected, "{base}");
        }
        // An explicit Host header wins over the URL host (edgegrid-python).
        let mut req = request("https://127.0.0.1/", &case, auth(&d));
        req.headers.push(("Host".into(), "akaa-baseurl-xxxxxxxxxxx-xxxxxxxxxxxxx.luna.akamaiapis.net".into()));
        assert_eq!(signed(&mut req), expected);
        let mut bad = request("https://127.0.0.1/", &case, auth(&d).with("baseURL", "https://"));
        assert!(apply(&mut bad).is_err());

        let mut missing =
            PreparedRequest { url: "https://x/".into(), auth: Auth::new(AuthType::EdgeGrid), ..Default::default() };
        assert!(matches!(apply(&mut missing), Err(AuthError::Missing { field: "clientToken", .. })));
    }
}
