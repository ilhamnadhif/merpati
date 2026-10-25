//! AWS Secrets Manager (JSON 1.1 protocol, SigV4 from merpati-auth).
//!
//! References are `name-or-arn[?versionStage=X|versionId=Y][#jsonKey]`.
//! Credentials come from the provider's static keys or from the default
//! chain: environment variables, then the shared credentials/config files
//! (static keys or `credential_process`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use merpati_core::tokens::MemoryTokenStore;
use merpati_core::{Auth, AuthType, PreparedBody, PreparedRequest};

use crate::cli;
use crate::env::{Env, HttpReply, ProcessEnv, base_url, send};
use crate::error::{ErrorKind, SecretError};
use crate::model::{AwsCredentialSource, AwsSettings};
use crate::providers::BrowseEntry;
use crate::reference::{Reference, extract_json_key};

const WHAT: &str = "AWS Secrets Manager";

#[derive(Clone, PartialEq)]
pub(crate) struct AwsCreds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Where they came from, for messages ("profile \"dev\"").
    pub source: String,
}

impl std::fmt::Debug for AwsCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCreds")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .field("session_token", &self.session_token.as_ref().map(|_| "<redacted>"))
            .field("source", &self.source)
            .finish()
    }
}

impl AwsCreds {
    fn secrets(&self) -> Vec<&str> {
        let mut v = vec![self.secret_access_key.as_str(), self.access_key_id.as_str()];
        if let Some(t) = &self.session_token {
            v.push(t);
        }
        v
    }
}

type Ini = HashMap<String, HashMap<String, String>>;

/// Minimal INI parser for the AWS shared files.
pub(crate) fn parse_ini(text: &str) -> Ini {
    let mut out: Ini = HashMap::new();
    let mut section: Option<String> = None;
    for raw in text.lines() {
        if raw.starts_with([' ', '\t']) {
            // Nested values (`s3 =\n  max_concurrent_requests = 10`) are ignored.
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            out.entry(name.clone()).or_default();
            section = Some(name);
            continue;
        }
        if let (Some(sec), Some((k, v))) = (&section, line.split_once('=')) {
            out.entry(sec.clone()).or_default().insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    out
}

/// One profile from `~/.aws/config` and `~/.aws/credentials` (the latter wins).
fn load_profile(pe: &ProcessEnv, name: &str) -> (HashMap<String, String>, Vec<PathBuf>) {
    let home = pe.home().map(|h| h.join(".aws"));
    let config_path = pe.var("AWS_CONFIG_FILE").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join("config")));
    let creds_path = pe
        .var("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join("credentials")));
    let mut merged = HashMap::new();
    let mut files = Vec::new();
    if let Some(p) = config_path {
        if let Ok(text) = std::fs::read_to_string(&p) {
            let ini = parse_ini(&text);
            let section = if name == "default" {
                ini.get("default").or_else(|| ini.get("profile default"))
            } else {
                ini.get(&format!("profile {name}"))
            };
            if let Some(s) = section {
                merged.extend(s.clone());
            }
        }
        files.push(p);
    }
    if let Some(p) = creds_path {
        if let Ok(text) = std::fs::read_to_string(&p)
            && let Some(s) = parse_ini(&text).get(name)
        {
            merged.extend(s.clone());
        }
        files.push(p);
    }
    (merged, files)
}

fn profile_name(pe: &ProcessEnv, s: &AwsSettings) -> String {
    let configured = s.profile.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    pe.var("AWS_PROFILE").unwrap_or_else(|| "default".into())
}

/// Credentials and the profile's region.
pub(crate) async fn credentials(
    env: &Env,
    id: &str,
    s: &AwsSettings,
) -> Result<(AwsCreds, Option<String>), SecretError> {
    let pe = &env.process;
    let name = profile_name(pe, s);
    let (profile, files) = load_profile(pe, &name);
    let profile_region = profile.get("region").cloned().filter(|r| !r.is_empty());
    if s.credentials == AwsCredentialSource::Static {
        let access_key_id = env.required_credential(id, "access_key_id", "access key ID", WHAT)?;
        let secret_access_key = env.required_credential(id, "secret_access_key", "secret access key", WHAT)?;
        let session_token = env.credential(id, "session_token");
        let creds = AwsCreds { access_key_id, secret_access_key, session_token, source: "static keys".into() };
        return Ok((creds, profile_region));
    }
    if s.profile.trim().is_empty()
        && let (Some(a), Some(k)) = (pe.var("AWS_ACCESS_KEY_ID"), pe.var("AWS_SECRET_ACCESS_KEY"))
    {
        let creds = AwsCreds {
            access_key_id: a,
            secret_access_key: k,
            session_token: pe.var("AWS_SESSION_TOKEN"),
            source: "environment variables".into(),
        };
        return Ok((creds, profile_region));
    }
    let get = |k: &str| profile.get(k).cloned().filter(|v| !v.is_empty());
    if let (Some(a), Some(k)) = (get("aws_access_key_id"), get("aws_secret_access_key")) {
        let creds = AwsCreds {
            access_key_id: a,
            secret_access_key: k,
            session_token: get("aws_session_token"),
            source: format!("profile \"{name}\""),
        };
        return Ok((creds, profile_region));
    }
    if let Some(command) = get("credential_process") {
        return Ok((process_credentials(env, id, &name, &command).await?, profile_region));
    }
    let files = files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
    if profile.keys().any(|k| k.starts_with("sso_")) {
        return Err(SecretError::auth(format!(
            "AWS profile \"{name}\" uses IAM Identity Center (SSO), which is not supported directly. Run `aws sso login --profile {name}` and add `credential_process = aws configure export-credentials --profile {name} --format process` to a profile, or use static keys."
        )));
    }
    if profile.contains_key("role_arn") {
        return Err(SecretError::auth(format!(
            "AWS profile \"{name}\" assumes a role, which is not supported directly. Add a `credential_process` (for example `aws configure export-credentials --profile {name} --format process`) or use static keys."
        )));
    }
    Err(SecretError::auth(format!(
        "No AWS credentials found: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, add profile \"{name}\" with keys or credential_process to {files}, or switch the provider to static keys."
    )))
}

/// Runs `credential_process` (cached until shortly before `Expiration`).
async fn process_credentials(env: &Env, id: &str, profile: &str, command: &str) -> Result<AwsCreds, SecretError> {
    let cache_key = format!("{id}#credential_process#{profile}");
    let source = format!("credential_process of profile \"{profile}\"");
    if let Some(t) = env.token(&cache_key)
        && let Ok(v) = serde_json::from_str::<Value>(&t.value)
    {
        return parse_process_output(&v, &source).map(|(c, _)| c);
    }
    let label = format!("credential_process of AWS profile \"{profile}\"");
    let out = cli::run_shell(command, env.cli_timeout, &label).await?;
    if !out.success {
        return Err(SecretError::auth(format!("{label} failed: {}", out.error_line(&[]))));
    }
    let v: Value = serde_json::from_slice(&out.stdout)
        .map_err(|_| SecretError::auth(format!("{label} did not print the expected JSON credentials.")))?;
    let (creds, lifetime) = parse_process_output(&v, &source)?;
    env.set_token(&cache_key, v.to_string(), lifetime);
    Ok(creds)
}

fn parse_process_output(v: &Value, source: &str) -> Result<(AwsCreds, Duration), SecretError> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let (Some(a), Some(k)) = (s("AccessKeyId"), s("SecretAccessKey")) else {
        return Err(SecretError::auth(format!("{source} returned no AccessKeyId/SecretAccessKey.")));
    };
    let lifetime = s("Expiration")
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(&e).ok())
        .map(|e| (e.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().unwrap_or(Duration::ZERO))
        .unwrap_or(Duration::from_secs(900));
    Ok((
        AwsCreds { access_key_id: a, secret_access_key: k, session_token: s("SessionToken"), source: source.into() },
        lifetime,
    ))
}

fn region(pe: &ProcessEnv, s: &AwsSettings, profile_region: Option<String>) -> Result<String, SecretError> {
    let r = s.region.trim();
    if !r.is_empty() {
        return Ok(r.to_string());
    }
    pe.var("AWS_REGION").or_else(|| pe.var("AWS_DEFAULT_REGION")).or(profile_region).ok_or_else(|| {
        SecretError::config("No AWS region: set it in the provider settings, AWS_REGION, or the profile's `region`.")
    })
}

fn endpoint(s: &AwsSettings, region: &str) -> String {
    let e = s.endpoint.trim();
    if !e.is_empty() {
        return base_url(e);
    }
    let suffix = if region.starts_with("cn-") { "amazonaws.com.cn" } else { "amazonaws.com" };
    format!("https://secretsmanager.{region}.{suffix}")
}

struct Call {
    reply: HttpReply,
    creds: AwsCreds,
    region: String,
}

/// Signs and sends one `secretsmanager.<target>` call.
async fn call(env: &Env, id: &str, s: &AwsSettings, target: &str, body: &Value) -> Result<Call, SecretError> {
    let (creds, profile_region) = credentials(env, id, s).await?;
    let region = region(&env.process, s, profile_region)?;
    let body = serde_json::to_vec(body).map_err(|e| SecretError::provider(e.to_string()))?;
    let mut auth = Auth::new(AuthType::AwsV4)
        .with("accessKey", creds.access_key_id.as_str())
        .with("secretKey", creds.secret_access_key.as_str())
        .with("region", region.as_str())
        .with("service", "secretsmanager");
    if let Some(t) = &creds.session_token {
        auth = auth.with("sessionToken", t.as_str());
    }
    let mut req = PreparedRequest {
        method: "POST".into(),
        url: format!("{}/", endpoint(s, &region)),
        headers: vec![
            ("Content-Type".into(), "application/x-amz-json-1.1".into()),
            ("X-Amz-Target".into(), format!("secretsmanager.{target}")),
        ],
        body: PreparedBody::Bytes(body.clone().into()),
        auth,
        ..Default::default()
    };
    let http = env.http();
    let tokens = MemoryTokenStore::default();
    merpati_auth::apply(&mut req, &merpati_auth::AuthContext { tokens: &tokens, http: &http })
        .await
        .map_err(|e| SecretError::config(format!("Could not sign the AWS request: {e}")))?;
    let mut rb = http.post(&req.url);
    for (k, v) in &req.headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    let reply = send(rb.body(body), WHAT, env.request_timeout).await?;
    Ok(Call { reply, creds, region })
}

fn error_type(v: &Value) -> String {
    let t = v.get("__type").or_else(|| v.get("code")).and_then(Value::as_str).unwrap_or_default();
    t.rsplit('#').next().unwrap_or(t).split(':').next().unwrap_or_default().to_string()
}

fn call_error(c: &Call, what: &str, action: &str) -> SecretError {
    let v: Value = serde_json::from_slice(&c.reply.body).unwrap_or(Value::Null);
    let kind = error_type(&v);
    let msg = c.reply.error_text(&c.creds.secrets());
    let detail = if msg.is_empty() { kind.clone() } else { format!("{kind}: {msg}") };
    let region = &c.region;
    let source = &c.creds.source;
    match kind.as_str() {
        "ResourceNotFoundException" => SecretError::not_found(format!("{what} was not found in region {region}.")),
        "AccessDeniedException" => SecretError::denied(format!(
            "AWS denied {what} ({detail}). The IAM policy for these credentials ({source}) must allow secretsmanager:{action}."
        )),
        "DecryptionFailure" | "KMSAccessDeniedException" => SecretError::denied(format!(
            "AWS could not decrypt {what} ({detail}). The credentials need kms:Decrypt on the secret's KMS key."
        )),
        "UnrecognizedClientException"
        | "InvalidSignatureException"
        | "IncompleteSignature"
        | "SignatureDoesNotMatch"
        | "InvalidClientTokenId"
        | "ExpiredTokenException"
        | "ExpiredToken"
        | "MissingAuthenticationToken"
        | "MissingAuthenticationTokenException"
        | "AuthFailure" => SecretError::auth(format!(
            "AWS rejected the credentials from {source} ({detail}). Check the access key, secret key, session token and region."
        )),
        _ if c.reply.status == 403 => SecretError::denied(format!("AWS denied {what} ({detail}).")),
        _ => SecretError::provider(format!("{WHAT} returned HTTP {} for {what} ({detail}).", c.reply.status)),
    }
}

async fn get_secret_value(env: &Env, id: &str, s: &AwsSettings, r: &Reference) -> Result<(String, Value), SecretError> {
    r.only_params(&["versionStage", "versionId"], WHAT)?;
    let mut body = json!({ "SecretId": r.path });
    if let Some(v) = r.param("versionStage") {
        body["VersionStage"] = json!(v);
    }
    if let Some(v) = r.param("versionId") {
        body["VersionId"] = json!(v);
    }
    let label = format!("AWS secret \"{}\"", r.path);
    let c = call(env, id, s, "GetSecretValue", &body).await?;
    if !c.reply.ok() {
        return Err(call_error(&c, &label, "GetSecretValue"));
    }
    Ok((label, c.reply.json(WHAT)?))
}

pub(crate) fn check_reference(reference: &str) -> Result<(), SecretError> {
    Reference::parse(reference)?.only_params(&["versionStage", "versionId"], WHAT)
}

pub(crate) async fn fetch(env: &Env, id: &str, s: &AwsSettings, reference: &str) -> Result<String, SecretError> {
    let r = Reference::parse(reference)?;
    let (label, v) = get_secret_value(env, id, s, &r).await?;
    if let Some(text) = v.get("SecretString").and_then(Value::as_str) {
        return match &r.field {
            Some(key) => extract_json_key(text, key, &label),
            None => Ok(text.to_string()),
        };
    }
    if let Some(bin) = v.get("SecretBinary").and_then(Value::as_str) {
        if r.field.is_some() {
            return Err(SecretError::reference(format!("{label} is binary, so it has no JSON keys.")));
        }
        // Binary secrets are returned base64-encoded, as the API sends them.
        return Ok(bin.to_string());
    }
    Err(SecretError::not_found(format!("{label} has no value.")))
}

async fn list(
    env: &Env,
    id: &str,
    s: &AwsSettings,
    prefix: &str,
    max_pages: usize,
) -> Result<(Vec<String>, Call), SecretError> {
    let mut names = Vec::new();
    let mut next: Option<String> = None;
    let mut page = 0;
    loop {
        let mut body = json!({ "MaxResults": 100 });
        if !prefix.is_empty() {
            body["Filters"] = json!([{ "Key": "name", "Values": [prefix] }]);
        }
        if let Some(t) = &next {
            body["NextToken"] = json!(t);
        }
        let c = call(env, id, s, "ListSecrets", &body).await?;
        if !c.reply.ok() {
            return Err(call_error(&c, "listing secrets", "ListSecrets"));
        }
        let v = c.reply.json(WHAT)?;
        if let Some(list) = v.get("SecretList").and_then(Value::as_array) {
            names.extend(list.iter().filter_map(|e| e.get("Name").and_then(Value::as_str)).map(str::to_string));
        }
        next = v.get("NextToken").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string);
        page += 1;
        if next.is_none() || page >= max_pages {
            return Ok((names, c));
        }
    }
}

pub(crate) async fn test(env: &Env, id: &str, s: &AwsSettings) -> Result<String, SecretError> {
    let body = json!({ "MaxResults": 100 });
    let c = call(env, id, s, "ListSecrets", &body).await?;
    let base = format!("Connected to {WHAT} in {} using {}", c.region, c.creds.source);
    if c.reply.ok() {
        let n = c.reply.json(WHAT)?.get("SecretList").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        let more = if n >= 100 { "+" } else { "" };
        return Ok(format!("{base}; {n}{more} secrets visible"));
    }
    let e = call_error(&c, "listing secrets", "ListSecrets");
    if e.kind == ErrorKind::PermissionDenied {
        return Ok(format!(
            "{base}. The credentials are valid, but listing is not allowed (secretsmanager:ListSecrets); reading secrets may still work."
        ));
    }
    Err(e)
}

/// `""` or a name prefix lists secrets; `name#` lists the JSON keys of one.
pub(crate) async fn browse(env: &Env, id: &str, s: &AwsSettings, path: &str) -> Result<Vec<BrowseEntry>, SecretError> {
    if let Some(name) = path.strip_suffix('#') {
        let r = Reference::parse(name)?;
        let (_, v) = get_secret_value(env, id, s, &r).await?;
        let keys: Vec<String> = v
            .get("SecretString")
            .and_then(Value::as_str)
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .and_then(|j| j.as_object().map(|m| m.keys().cloned().collect()))
            .unwrap_or_default();
        return Ok(keys.into_iter().map(|k| BrowseEntry::field(k.clone(), format!("{name}#{k}"))).collect());
    }
    let (names, _) = list(env, id, s, path.trim(), 10).await?;
    Ok(names
        .into_iter()
        .map(|n| BrowseEntry { path: Some(format!("{n}#")), ..BrowseEntry::secret(n.clone(), n, false) })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn ini_and_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("credentials");
        let config = dir.path().join("config");
        std::fs::write(
            &creds,
            "[default]\naws_access_key_id = AKDEFAULT\naws_secret_access_key = sk-default\n\n[dev]\naws_access_key_id=AKDEV\naws_secret_access_key=sk-dev\naws_session_token = st\n",
        )
        .unwrap();
        std::fs::write(
            &config,
            "[default]\nregion = us-west-2\n[profile dev]\nregion = eu-central-1\ns3 =\n  max_concurrent_requests = 10\n[profile sso]\nsso_start_url = https://x\n[profile proc]\ncredential_process = echo '{\"Version\":1,\"AccessKeyId\":\"AKPROC\",\"SecretAccessKey\":\"sk-proc\"}'\n",
        )
        .unwrap();
        let pe = ProcessEnv::fixed(&[
            ("AWS_SHARED_CREDENTIALS_FILE", creds.to_str().unwrap()),
            ("AWS_CONFIG_FILE", config.to_str().unwrap()),
            ("AWS_PROFILE", "dev"),
        ]);
        let (p, _) = load_profile(&pe, "dev");
        assert_eq!(p.get("aws_access_key_id").map(String::as_str), Some("AKDEV"));
        assert_eq!(p.get("region").map(String::as_str), Some("eu-central-1"));
        assert!(!p.contains_key("max_concurrent_requests"));

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let mut env =
            Env::new(Arc::new(merpati_core::secrets::SecretStore::memory()), crate::default_client().unwrap());
        env.process = pe;
        let s = AwsSettings::default();
        let (c, r) = rt.block_on(credentials(&env, "p", &s)).unwrap();
        assert_eq!(
            (c.access_key_id.as_str(), c.session_token.as_deref(), r.as_deref()),
            ("AKDEV", Some("st"), Some("eu-central-1"))
        );
        assert_eq!(c.source, "profile \"dev\"");
        assert!(!format!("{c:?}").contains("sk-dev"));

        let s = AwsSettings { profile: "default".into(), ..Default::default() };
        let (c, r) = rt.block_on(credentials(&env, "p", &s)).unwrap();
        assert_eq!((c.access_key_id.as_str(), r.as_deref()), ("AKDEFAULT", Some("us-west-2")));
        assert_eq!(region(&env.process, &s, r).unwrap(), "us-west-2");

        let s = AwsSettings { profile: "sso".into(), ..Default::default() };
        let e = rt.block_on(credentials(&env, "p", &s)).unwrap_err();
        assert!(e.message.contains("aws sso login"), "{e}");

        #[cfg(unix)]
        {
            let s = AwsSettings { profile: "proc".into(), ..Default::default() };
            let (c, _) = rt.block_on(credentials(&env, "p", &s)).unwrap();
            assert_eq!(c.access_key_id, "AKPROC");
            assert!(c.source.contains("credential_process"));
        }

        let s = AwsSettings { profile: "missing".into(), ..Default::default() };
        let e = rt.block_on(credentials(&env, "p", &s)).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Auth);

        // Environment variables win when no profile is configured.
        env.process = ProcessEnv::fixed(&[
            ("AWS_ACCESS_KEY_ID", "AKENV"),
            ("AWS_SECRET_ACCESS_KEY", "sk-env"),
            ("AWS_REGION", "ap-south-1"),
            ("HOME", dir.path().to_str().unwrap()),
        ]);
        let s = AwsSettings::default();
        let (c, r) = rt.block_on(credentials(&env, "p", &s)).unwrap();
        assert_eq!(c.access_key_id, "AKENV");
        assert_eq!(region(&env.process, &s, r).unwrap(), "ap-south-1");
        env.process = ProcessEnv::fixed(&[]);
        assert_eq!(region(&env.process, &s, None).unwrap_err().kind, ErrorKind::Config);
    }

    #[test]
    fn endpoints_and_error_types() {
        let s = AwsSettings::default();
        assert_eq!(endpoint(&s, "eu-west-1"), "https://secretsmanager.eu-west-1.amazonaws.com");
        assert_eq!(endpoint(&s, "cn-north-1"), "https://secretsmanager.cn-north-1.amazonaws.com.cn");
        let s = AwsSettings { endpoint: "http://localhost:4566/".into(), ..Default::default() };
        assert_eq!(endpoint(&s, "x"), "http://localhost:4566");
        assert_eq!(
            error_type(&json!({"__type": "com.amazonaws.secretsmanager#ResourceNotFoundException"})),
            "ResourceNotFoundException"
        );
    }
}
