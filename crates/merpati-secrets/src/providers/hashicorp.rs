//! HashiCorp Vault: token or AppRole auth, KV v1 and v2.
//!
//! References are `path[?version=N][#field]`, relative to the configured KV
//! mount (or `mount/path...` when no mount is configured).

use std::time::Duration;

use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::env::{Env, HttpReply, base_url, send};
use crate::error::SecretError;
use crate::model::{KvVersion, VaultAuth, VaultSettings, check_url};
use crate::providers::BrowseEntry;
use crate::reference::{Reference, encode_path, key_list, value_string};

const WHAT: &str = "HashiCorp Vault";

struct Vault<'a> {
    env: &'a Env,
    id: &'a str,
    s: &'a VaultSettings,
    base: String,
}

/// What a request was for, to word errors.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Read,
    List,
    Probe,
    Identity,
}

impl<'a> Vault<'a> {
    fn new(env: &'a Env, id: &'a str, s: &'a VaultSettings) -> Result<Self, SecretError> {
        check_url("Vault address", &s.address)?;
        Ok(Vault { env, id, s, base: base_url(&s.address) })
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb =
            self.env.http().request(method, format!("{}/v1/{path}", self.base)).header("X-Vault-Request", "true");
        let ns = self.s.namespace.trim().trim_matches('/');
        if !ns.is_empty() {
            rb = rb.header("X-Vault-Namespace", ns);
        }
        rb
    }

    async fn token(&self, fresh: bool) -> Result<String, SecretError> {
        match self.s.auth {
            VaultAuth::Token => self.env.required_credential(self.id, "token", "Vault token", WHAT),
            VaultAuth::AppRole => {
                if !fresh && let Some(t) = self.env.token(self.id) {
                    return Ok(t.value);
                }
                self.approle_login().await
            }
        }
    }

    async fn approle_login(&self) -> Result<String, SecretError> {
        let role_id = self.s.role_id.trim();
        if role_id.is_empty() {
            return Err(SecretError::config("AppRole login needs a role ID."));
        }
        let secret_id = self.env.credential(self.id, "secret_id");
        let mut body = json!({ "role_id": role_id });
        if let Some(sid) = &secret_id {
            body["secret_id"] = Value::String(sid.clone());
        }
        let mount = match self.s.approle_mount.trim().trim_matches('/') {
            "" => "approle",
            m => m,
        };
        let path = format!("auth/{}/login", encode_path(mount));
        let reply = send(self.request(Method::POST, &path).json(&body), WHAT, self.env.request_timeout).await?;
        let secrets: Vec<&str> = secret_id.iter().map(String::as_str).collect();
        if !reply.ok() {
            let detail = reply.error_text(&secrets);
            return Err(match reply.status {
                400 | 401 | 403 => SecretError::auth(format!(
                    "Vault AppRole login failed ({detail}). Check the role ID, the secret ID and the AppRole mount \"{mount}\"."
                )),
                404 => SecretError::config(format!(
                    "Vault has no AppRole auth method at \"auth/{mount}\" (404). Check the AppRole mount path and namespace."
                )),
                _ => status_error(&reply, "AppRole login", Purpose::Identity, &secrets),
            });
        }
        let v = reply.json(WHAT)?;
        let token = v.pointer("/auth/client_token").and_then(Value::as_str).unwrap_or_default().to_string();
        if token.is_empty() {
            return Err(SecretError::provider("Vault AppRole login returned no client token."));
        }
        let lease = v.pointer("/auth/lease_duration").and_then(Value::as_u64).unwrap_or(0);
        let lifetime = if lease == 0 { Duration::from_secs(3600) } else { Duration::from_secs(lease) };
        self.env.set_token(self.id, token.clone(), lifetime);
        Ok(token)
    }

    /// GET with the token; an AppRole token rejected with 403 is renewed once.
    async fn get(
        &self,
        path: &str,
        query: &[(&str, String)],
        what: &str,
        purpose: Purpose,
    ) -> Result<Value, SecretError> {
        let mut token = self.token(false).await?;
        let mut retried = false;
        loop {
            let rb = self.request(Method::GET, path).header("X-Vault-Token", &token).query(query);
            let reply = send(rb, WHAT, self.env.request_timeout).await?;
            if reply.ok() {
                return reply.json(WHAT);
            }
            if reply.status == 403 && self.s.auth == VaultAuth::AppRole && !retried {
                retried = true;
                self.env.drop_token(self.id);
                token = self.token(true).await?;
                continue;
            }
            return Err(status_error(&reply, what, purpose, &[token.as_str()]));
        }
    }

    fn split_mount(&self, path: &str) -> Result<(String, String), SecretError> {
        let mount = self.s.mount.trim().trim_matches('/');
        if !mount.is_empty() {
            return Ok((mount.to_string(), path.trim_matches('/').to_string()));
        }
        match path.trim_matches('/').split_once('/') {
            Some((m, rest)) if !m.is_empty() => Ok((m.to_string(), rest.to_string())),
            _ => Err(SecretError::reference(format!(
                "\"{path}\" needs the KV mount as first segment (mount/path) because no mount is set in the provider settings."
            ))),
        }
    }

    async fn kv_version(&self, mount: &str) -> Result<KvVersion, SecretError> {
        if self.s.kv_version != KvVersion::Auto {
            return Ok(self.s.kv_version);
        }
        let key = (self.id.to_string(), mount.to_string());
        if let Some(v) = self.env.kv_versions.lock().get(&key) {
            return Ok(*v);
        }
        let path = format!("sys/internal/ui/mounts/{}", encode_path(mount));
        let detected = match self.get(&path, &[], &format!("Vault mount \"{mount}\""), Purpose::Probe).await {
            Ok(v) => {
                let version = v.pointer("/data/options/version").and_then(Value::as_str).unwrap_or("1");
                if version == "2" { KvVersion::V2 } else { KvVersion::V1 }
            }
            Err(e) if e.kind == crate::ErrorKind::Network || e.kind == crate::ErrorKind::Timeout => return Err(e),
            // Auth problems are reported by the real call (not cached, so a
            // fixed token detects again).
            Err(e) if e.kind == crate::ErrorKind::Auth => return Ok(KvVersion::V2),
            // Old Vault or a token that may not probe: the default engine.
            Err(_) => KvVersion::V2,
        };
        self.env.kv_versions.lock().insert(key, detected);
        Ok(detected)
    }

    /// The key/value map of a secret.
    async fn read_map(&self, r: &Reference) -> Result<(String, Map<String, Value>), SecretError> {
        r.only_params(&["version"], WHAT)?;
        let (mount, path) = self.split_mount(&r.path)?;
        if path.is_empty() {
            return Err(SecretError::reference(format!("Reference \"{}\" names a mount but no secret path.", r.path)));
        }
        let label = format!("Vault secret \"{mount}/{path}\"");
        let version = self.kv_version(&mount).await?;
        let data = match version {
            KvVersion::V2 | KvVersion::Auto => {
                let mut query = Vec::new();
                if let Some(v) = r.param("version") {
                    if v.parse::<u64>().is_err() {
                        return Err(SecretError::reference(format!("Vault version \"{v}\" is not a number.")));
                    }
                    query.push(("version", v.to_string()));
                }
                let api = format!("{}/data/{}", encode_path(&mount), encode_path(&path));
                let v = self.get(&api, &query, &label, Purpose::Read).await?;
                match v.pointer("/data/data") {
                    Some(Value::Object(m)) => m.clone(),
                    _ => {
                        return Err(SecretError::not_found(format!(
                            "{label} has no data (the version may be deleted or destroyed)."
                        )));
                    }
                }
            }
            KvVersion::V1 => {
                if r.param("version").is_some() {
                    return Err(SecretError::reference("\"?version=\" works only with KV version 2 mounts."));
                }
                let api = format!("{}/{}", encode_path(&mount), encode_path(&path));
                let v = self.get(&api, &[], &label, Purpose::Read).await?;
                match v.get("data") {
                    Some(Value::Object(m)) => m.clone(),
                    _ => return Err(SecretError::not_found(format!("{label} has no data."))),
                }
            }
        };
        Ok((label, data))
    }
}

fn status_error(reply: &HttpReply, what: &str, purpose: Purpose, secrets: &[&str]) -> SecretError {
    let detail = reply.error_text(secrets);
    let detail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
    match reply.status {
        401 | 403 if purpose == Purpose::Identity => SecretError::auth(format!(
            "Vault rejected the token (HTTP {}{detail}). Check the token or AppRole credentials and the namespace.",
            reply.status
        )),
        401 | 403 => SecretError::denied(format!(
            "Access to {what} was denied (HTTP {}{detail}). The token may be invalid or expired, or its policy does not allow {}.",
            reply.status,
            if purpose == Purpose::List { "listing this path" } else { "reading this path" }
        )),
        404 => SecretError::not_found(format!("{what} was not found. Check the path and the KV mount.")),
        301 | 302 | 307 | 308 => SecretError::provider(format!(
            "Vault answered with a redirect (HTTP {}); use the address of the active node.",
            reply.status
        )),
        503 => SecretError::provider(format!("Vault is sealed or unavailable (HTTP 503{detail}).")),
        s => SecretError::provider(format!("Vault returned HTTP {s} for {what}{detail}")),
    }
}

fn pick_field(label: &str, data: &Map<String, Value>, field: Option<&str>) -> Result<String, SecretError> {
    match field {
        Some(f) => data.get(f).map(value_string).ok_or_else(|| {
            SecretError::not_found(format!("Field \"{f}\" not found in {label} (fields: {}).", key_list(data.keys())))
        }),
        None if data.len() == 1 => Ok(data.values().next().map(value_string).unwrap_or_default()),
        None => match data.get("value") {
            Some(v) => Ok(value_string(v)),
            None => Err(SecretError::reference(format!(
                "{label} has several fields ({}); add #field to the reference.",
                key_list(data.keys())
            ))),
        },
    }
}

pub(crate) fn check_reference(s: &VaultSettings, reference: &str) -> Result<(), SecretError> {
    let r = Reference::parse(reference)?;
    r.only_params(&["version"], WHAT)?;
    if s.mount.trim().trim_matches('/').is_empty() && !r.path.contains('/') {
        return Err(SecretError::reference(format!(
            "\"{}\" needs the KV mount as first segment (mount/path) because no mount is set in the provider settings.",
            r.path
        )));
    }
    Ok(())
}

pub(crate) async fn fetch(env: &Env, id: &str, s: &VaultSettings, reference: &str) -> Result<String, SecretError> {
    let vault = Vault::new(env, id, s)?;
    let r = Reference::parse(reference)?;
    let (label, data) = vault.read_map(&r).await?;
    pick_field(&label, &data, r.field.as_deref())
}

pub(crate) async fn test(env: &Env, id: &str, s: &VaultSettings) -> Result<String, SecretError> {
    let vault = Vault::new(env, id, s)?;
    let v = vault.get("auth/token/lookup-self", &[], "the token", Purpose::Identity).await?;
    let name = v.pointer("/data/display_name").and_then(Value::as_str).unwrap_or("token");
    let policies: Vec<&str> = v
        .pointer("/data/policies")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut summary = format!("Connected to Vault at {} as \"{name}\"", vault.base);
    if !policies.is_empty() {
        summary.push_str(&format!(" (policies: {})", policies.join(", ")));
    }
    let mount = s.mount.trim().trim_matches('/');
    if !mount.is_empty() {
        let version = vault.kv_version(mount).await?;
        let n = if version == KvVersion::V1 { 1 } else { 2 };
        summary.push_str(&format!("; KV v{n} at \"{mount}/\""));
    }
    Ok(summary)
}

pub(crate) async fn browse(
    env: &Env,
    id: &str,
    s: &VaultSettings,
    path: &str,
) -> Result<Vec<BrowseEntry>, SecretError> {
    let vault = Vault::new(env, id, s)?;
    let path = path.trim_start_matches('/');
    let no_mount = s.mount.trim().trim_matches('/').is_empty();
    if no_mount && path.is_empty() {
        // List the KV mounts visible to the token.
        let v = vault.get("sys/internal/ui/mounts", &[], "the list of mounts", Purpose::List).await?;
        let mounts = v.pointer("/data/secret").and_then(Value::as_object).cloned().unwrap_or_default();
        return Ok(mounts
            .iter()
            .filter(|(_, m)| m.get("type").and_then(Value::as_str).is_some_and(|t| t == "kv" || t == "generic"))
            .map(|(name, _)| BrowseEntry::folder(name.clone(), name.clone()))
            .collect());
    }
    if !path.is_empty() && !path.ends_with('/') {
        // A secret: list its field names (values are dropped).
        let r = Reference::parse(path)?;
        let (_, data) = vault.read_map(&r).await?;
        return Ok(data.keys().map(|k| BrowseEntry::field(k.clone(), format!("{}#{k}", r.path))).collect());
    }
    let (mount, sub) = if no_mount {
        let (m, rest) = path.trim_end_matches('/').split_once('/').unwrap_or((path.trim_end_matches('/'), ""));
        (m.to_string(), rest.to_string())
    } else {
        (s.mount.trim().trim_matches('/').to_string(), path.trim_end_matches('/').to_string())
    };
    let version = vault.kv_version(&mount).await?;
    let api = match (version, sub.is_empty()) {
        (KvVersion::V1, true) => encode_path(&mount),
        (KvVersion::V1, false) => format!("{}/{}", encode_path(&mount), encode_path(&sub)),
        (_, true) => format!("{}/metadata", encode_path(&mount)),
        (_, false) => format!("{}/metadata/{}", encode_path(&mount), encode_path(&sub)),
    };
    let label = format!("Vault path \"{mount}/{sub}\"");
    let v = match vault.get(&api, &[("list", "true".into())], &label, Purpose::List).await {
        Ok(v) => v,
        // Vault answers 404 for an empty folder.
        Err(e) if e.kind == crate::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let keys: Vec<String> = v
        .pointer("/data/keys")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    Ok(keys
        .into_iter()
        .map(|k| {
            let full = format!("{path}{k}");
            if k.ends_with('/') { BrowseEntry::folder(k, full) } else { BrowseEntry::secret(k, full, true) }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_selection() {
        let mut m = Map::new();
        m.insert("only".into(), json!("v"));
        assert_eq!(pick_field("s", &m, None).unwrap(), "v");
        m.insert("value".into(), json!(1));
        assert_eq!(pick_field("s", &m, None).unwrap(), "1");
        m.remove("value");
        m.insert("other".into(), json!(true));
        let e = pick_field("s", &m, None).unwrap_err();
        assert_eq!(e.kind, crate::ErrorKind::InvalidReference);
        assert!(e.message.contains("only, other"));
        assert_eq!(pick_field("s", &m, Some("nope")).unwrap_err().kind, crate::ErrorKind::NotFound);
    }
}
