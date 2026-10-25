//! AWS Secrets Manager against a mock that recomputes every SigV4 signature.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Req, json, manager, serve};
use crate::env::ProcessEnv;
use crate::{AwsCredentialSource, AwsSettings, EntryKind, ErrorKind, ProviderSettings};

const KEYS: &[(&str, &str, Option<&str>)] = &[
    ("AKIDTEST", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", None),
    ("ASIATEMP", "temp-secret-key", Some("session-tok")),
    ("AKIDENV", "env-secret-key", None),
];

#[derive(Default)]
struct AwsMock {
    calls: AtomicUsize,
    /// (access key id, signed headers) of accepted requests.
    accepted: Mutex<Vec<(String, String)>>,
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn sha_hex(d: &[u8]) -> String {
    hex::encode(Sha256::digest(d))
}

/// Independent SigV4 check of a received request. Returns the access key id
/// and signed headers, or the AWS error type.
fn verify(r: &Req) -> Result<(String, String), &'static str> {
    let auth = r.header("authorization").ok_or("MissingAuthenticationTokenException")?;
    let rest = auth.strip_prefix("AWS4-HMAC-SHA256 ").ok_or("IncompleteSignature")?;
    let (mut credential, mut signed, mut signature) = (None, None, None);
    for part in rest.split(", ") {
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = Some(v);
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed = Some(v);
        } else if let Some(v) = part.strip_prefix("Signature=") {
            signature = Some(v);
        }
    }
    let (credential, signed, signature) = (
        credential.ok_or("IncompleteSignature")?,
        signed.ok_or("IncompleteSignature")?,
        signature.ok_or("IncompleteSignature")?,
    );
    let (akid, scope) = credential.split_once('/').ok_or("IncompleteSignature")?;
    let scope_parts: Vec<&str> = scope.split('/').collect();
    let [date, region, service, "aws4_request"] = scope_parts.as_slice() else { return Err("IncompleteSignature") };
    if *service != "secretsmanager" || *region != "eu-west-1" {
        return Err("InvalidSignatureException");
    }
    let (_, secret, session) = KEYS.iter().find(|(k, _, _)| *k == akid).ok_or("UnrecognizedClientException")?;
    let amz_date = r.header("x-amz-date").ok_or("IncompleteSignature")?;
    if !amz_date.starts_with(date) {
        return Err("InvalidSignatureException");
    }
    let names: Vec<&str> = signed.split(';').collect();
    for required in ["content-type", "host", "x-amz-date", "x-amz-target"] {
        if !names.contains(&required) {
            return Err("IncompleteSignature");
        }
    }
    if let Some(tok) = session
        && (!names.contains(&"x-amz-security-token") || r.header("x-amz-security-token") != Some(*tok))
    {
        return Err("UnrecognizedClientException");
    }
    let mut canonical_headers = String::new();
    for h in &names {
        let v = r.header(h).ok_or("IncompleteSignature")?;
        canonical_headers.push_str(&format!("{h}:{}\n", v.split_whitespace().collect::<Vec<_>>().join(" ")));
    }
    let canonical_request =
        format!("{}\n{}\n\n{canonical_headers}\n{signed}\n{}", r.method.as_str(), r.path, sha_hex(&r.body));
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha_hex(canonical_request.as_bytes()));
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    let k = hmac(&k, b"aws4_request");
    let expected = hex::encode(hmac(&k, string_to_sign.as_bytes()));
    if expected != signature {
        return Err("InvalidSignatureException");
    }
    Ok((akid.to_string(), signed.to_string()))
}

async fn handler(st: Arc<AwsMock>, r: Req) -> axum::response::Response {
    st.calls.fetch_add(1, Ordering::SeqCst);
    if r.header("content-type") != Some("application/x-amz-json-1.1") {
        return json(400, json!({"__type": "SerializationException"}));
    }
    match verify(&r) {
        Ok(a) => st.accepted.lock().push(a),
        Err(t) => {
            return json(
                400,
                json!({"__type": t, "message": "The request signature we calculated does not match the signature you provided."}),
            );
        }
    }
    let body = r.json();
    match r.header("x-amz-target").unwrap_or_default() {
        "secretsmanager.GetSecretValue" => match body["SecretId"].as_str().unwrap_or_default() {
            "prod/db" if body["VersionStage"] == "AWSPREVIOUS" => {
                json(200, json!({"Name": "prod/db", "SecretString": r#"{"password":"old-pw"}"#}))
            }
            "prod/db" => json(
                200,
                json!({"Name": "prod/db", "SecretString": r#"{"username":"admin","password":"aws-pw","port":5432}"#}),
            ),
            "plain" => json(200, json!({"Name": "plain", "SecretString": "plain-value"})),
            "bin" => json(200, json!({"Name": "bin", "SecretBinary": "AAEC"})),
            "forbidden" => json(
                400,
                json!({"__type": "AccessDeniedException", "Message": "User: arn:aws:iam::123:user/dev is not authorized to perform: secretsmanager:GetSecretValue"}),
            ),
            _ => json(
                400,
                json!({"__type": "com.amazonaws.secretsmanager#ResourceNotFoundException", "message": "Secrets Manager can't find the specified secret."}),
            ),
        },
        "secretsmanager.ListSecrets" => {
            let all = ["prod/db", "plain", "bin"];
            let prefix = body.pointer("/Filters/0/Values/0").and_then(Value::as_str).unwrap_or("").to_string();
            let names: Vec<&str> = all.iter().copied().filter(|n| n.starts_with(&prefix)).collect();
            let (page, next): (&[&str], Option<&str>) = match body["NextToken"].as_str() {
                None if names.len() > 2 => (&names[..2], Some("page-2")),
                None => (&names[..], None),
                Some(_) => (&names[2..], None),
            };
            let list: Vec<Value> = page
                .iter()
                .map(|n| json!({"Name": n, "ARN": format!("arn:aws:secretsmanager:eu-west-1:123:secret:{n}")}))
                .collect();
            let mut v = json!({"SecretList": list});
            if let Some(n) = next {
                v["NextToken"] = json!(n);
            }
            json(200, v)
        }
        _ => json(400, json!({"__type": "UnknownOperationException"})),
    }
}

fn start() -> (super::MockServer, Arc<AwsMock>) {
    let st = Arc::new(AwsMock::default());
    let s2 = st.clone();
    (serve(move |r| handler(s2.clone(), r)), st)
}

fn static_settings(url: &str) -> AwsSettings {
    AwsSettings {
        region: "eu-west-1".into(),
        credentials: AwsCredentialSource::Static,
        profile: String::new(),
        endpoint: url.into(),
    }
}

#[tokio::test]
async fn static_keys_signed_and_json_keys() {
    let (server, st) = start();
    let ext = manager(ProviderSettings::AwsSecretsManager(static_settings(&server.url)));
    let e = ext.resolve_reference("p1", "prod/db").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("access key ID"), "{e}");

    ext.set_credential("p1", "access_key_id", "AKIDTEST");
    ext.set_credential("p1", "secret_access_key", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY");
    assert_eq!(
        ext.resolve_reference("p1", "prod/db").await.unwrap(),
        r#"{"username":"admin","password":"aws-pw","port":5432}"#
    );
    assert_eq!(ext.resolve_reference("p1", "prod/db#password").await.unwrap(), "aws-pw");
    assert_eq!(ext.resolve_reference("p1", "prod/db#port").await.unwrap(), "5432");
    assert_eq!(ext.resolve_reference("p1", "prod/db?versionStage=AWSPREVIOUS#password").await.unwrap(), "old-pw");
    assert_eq!(ext.resolve_reference("p1", "plain").await.unwrap(), "plain-value");
    assert_eq!(ext.resolve_reference("p1", "bin").await.unwrap(), "AAEC");
    {
        let accepted = st.accepted.lock();
        assert_eq!(accepted.len(), 6);
        assert!(accepted.iter().all(|(k, h)| k == "AKIDTEST" && h == "content-type;host;x-amz-date;x-amz-target"));
    }

    let e = ext.resolve_reference("p1", "plain#key").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidReference);
    assert!(!e.message.contains("plain-value"), "{e}");
    let e = ext.resolve_reference("p1", "prod/db#nope").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("password, port, username") && !e.message.contains("aws-pw"), "{e}");
    let e = ext.resolve_reference("p1", "missing").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("eu-west-1"), "{e}");
    let e = ext.resolve_reference("p1", "forbidden").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    assert!(e.message.contains("secretsmanager:GetSecretValue"), "{e}");
    let e = ext.resolve_reference("p1", "prod/db?bogus=1").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidReference);

    // Wrong secret key: the mock's own SigV4 computation disagrees.
    ext.set_credential("p1", "secret_access_key", "not-the-right-key");
    let e = ext.resolve_reference("p1", "plain").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("InvalidSignatureException") && !e.message.contains("not-the-right-key"), "{e}");
}

#[tokio::test]
async fn session_token_is_signed() {
    let (server, st) = start();
    let ext = manager(ProviderSettings::AwsSecretsManager(static_settings(&server.url)));
    ext.set_credential("p1", "access_key_id", "ASIATEMP");
    ext.set_credential("p1", "secret_access_key", "temp-secret-key");
    let e = ext.resolve_reference("p1", "plain").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth, "missing session token must fail: {e}");
    ext.set_credential("p1", "session_token", "session-tok");
    assert_eq!(ext.resolve_reference("p1", "plain").await.unwrap(), "plain-value");
    let accepted = st.accepted.lock();
    assert_eq!(accepted.last().unwrap().1, "content-type;host;x-amz-date;x-amz-security-token;x-amz-target");
}

#[tokio::test]
async fn default_chain_from_environment_list_and_browse() {
    let (server, st) = start();
    let s = AwsSettings {
        region: String::new(),
        credentials: AwsCredentialSource::DefaultChain,
        ..static_settings(&server.url)
    };
    let mut ext = manager(ProviderSettings::AwsSecretsManager(s));
    ext.set_process_env(ProcessEnv::fixed(&[
        ("AWS_ACCESS_KEY_ID", "AKIDENV"),
        ("AWS_SECRET_ACCESS_KEY", "env-secret-key"),
        ("AWS_DEFAULT_REGION", "eu-west-1"),
        ("HOME", "/nonexistent"),
    ]));
    assert_eq!(ext.resolve_reference("p1", "prod/db#username").await.unwrap(), "admin");
    assert_eq!(st.accepted.lock()[0].0, "AKIDENV");

    let summary = ext.test_provider(&ext.provider("p1").unwrap()).await.unwrap();
    assert!(summary.contains("eu-west-1") && summary.contains("environment variables"), "{summary}");

    let before = st.calls.load(Ordering::SeqCst);
    let all = ext.browse("p1", "").await.unwrap();
    assert_eq!(st.calls.load(Ordering::SeqCst) - before, 2, "two pages");
    let names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["bin", "plain", "prod/db"]);
    assert!(all.iter().all(|e| e.kind == EntryKind::Secret));
    let db = all.iter().find(|e| e.name == "prod/db").unwrap();
    assert_eq!(db.reference.as_deref(), Some("prod/db"));
    let keys = ext.browse("p1", db.path.as_deref().unwrap()).await.unwrap();
    let refs: Vec<&str> = keys.iter().filter_map(|e| e.reference.as_deref()).collect();
    assert_eq!(refs, ["prod/db#password", "prod/db#port", "prod/db#username"]);
    let filtered = ext.browse("p1", "pro").await.unwrap();
    assert_eq!(filtered.len(), 1);

    // Nothing configured anywhere: tells the user where credentials can come from.
    let mut ext = manager(ProviderSettings::AwsSecretsManager(AwsSettings::default()));
    ext.set_process_env(ProcessEnv::fixed(&[("HOME", "/nonexistent"), ("AWS_REGION", "eu-west-1")]));
    let e = ext.resolve_reference("p1", "plain").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("AWS_ACCESS_KEY_ID") && e.message.contains("credentials"), "{e}");
}
