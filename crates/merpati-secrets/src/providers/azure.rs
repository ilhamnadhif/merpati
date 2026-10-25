//! Azure Key Vault secrets, with client credentials or an Azure CLI token.
//!
//! References are `secret-name[/version][#jsonKey]`.

use std::ffi::OsString;
use std::time::Duration;

use serde_json::Value;

use crate::cli;
use crate::env::{Env, HttpReply, base_url, send};
use crate::error::{ErrorKind, SecretError};
use crate::model::{AzureAuth, AzureSettings, check_url};
use crate::providers::BrowseEntry;
use crate::reference::{Reference, extract_json_key};

const WHAT: &str = "Azure Key Vault";
pub(crate) const DEFAULT_API_VERSION: &str = "7.5";
const DEFAULT_AUTHORITY: &str = "https://login.microsoftonline.com";

/// Token audience for a vault: `https://vault.azure.net` for
/// `x.vault.azure.net`, the matching sovereign cloud otherwise.
fn resource(vault: &url::Url) -> String {
    let host = vault.host_str().unwrap_or_default();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() >= 3 && labels[1] == "vault" {
        return format!("https://{}", labels[1..].join("."));
    }
    "https://vault.azure.net".into()
}

fn api_version(s: &AzureSettings) -> &str {
    match s.api_version.trim() {
        "" => DEFAULT_API_VERSION,
        v => v,
    }
}

fn auth_label(s: &AzureSettings) -> &'static str {
    match s.auth {
        AzureAuth::ClientSecret => "client credentials",
        AzureAuth::AzureCli => "the Azure CLI",
    }
}

async fn token(env: &Env, id: &str, s: &AzureSettings, fresh: bool) -> Result<String, SecretError> {
    if !fresh && let Some(t) = env.token(id) {
        return Ok(t.value);
    }
    let vault = check_url("Key Vault URL", &s.vault_url)?;
    let resource = resource(&vault);
    let (token, lifetime) = match s.auth {
        AzureAuth::ClientSecret => client_credentials(env, id, s, &resource).await?,
        AzureAuth::AzureCli => cli_token(env, s, &resource).await?,
    };
    env.set_token(id, token.clone(), lifetime);
    Ok(token)
}

async fn client_credentials(
    env: &Env,
    id: &str,
    s: &AzureSettings,
    resource: &str,
) -> Result<(String, Duration), SecretError> {
    let tenant = s.tenant_id.trim();
    let client_id = s.client_id.trim();
    if tenant.is_empty() || client_id.is_empty() {
        return Err(SecretError::config("Client credentials need a tenant ID and a client ID."));
    }
    let secret = env.required_credential(id, "client_secret", "client secret", WHAT)?;
    let authority = match base_url(&s.authority_host) {
        a if a.is_empty() => DEFAULT_AUTHORITY.to_string(),
        a => a,
    };
    let url = format!(
        "{authority}/{}/oauth2/v2.0/token",
        percent_encoding::utf8_percent_encode(tenant, percent_encoding::NON_ALPHANUMERIC)
    );
    let scope = format!("{resource}/.default");
    let form = [
        ("grant_type", "client_credentials"),
        ("client_id", client_id),
        ("client_secret", secret.as_str()),
        ("scope", scope.as_str()),
    ];
    let reply = send(env.http().post(url).form(&form), "Microsoft Entra ID", env.request_timeout).await?;
    if !reply.ok() {
        let detail = reply.error_text(&[secret.as_str()]);
        let lower = detail.to_ascii_lowercase();
        return Err(if lower.contains("aadsts90002") || (lower.contains("tenant") && lower.contains("not found")) {
            SecretError::config(format!(
                "Microsoft Entra ID does not know tenant \"{tenant}\" ({detail}). Check the tenant ID."
            ))
        } else if lower.contains("aadsts700016") {
            SecretError::auth(format!(
                "Application \"{client_id}\" was not found in tenant \"{tenant}\" ({detail}). Check the client ID."
            ))
        } else if lower.contains("aadsts7000222") {
            SecretError::auth(format!("The client secret has expired ({detail}). Create a new one and update it."))
        } else {
            SecretError::auth(format!(
                "Microsoft Entra ID rejected the client credentials (HTTP {}: {detail}). Check the tenant ID, client ID and client secret.",
                reply.status
            ))
        });
    }
    let v = reply.json("Microsoft Entra ID")?;
    let token = v.get("access_token").and_then(Value::as_str).unwrap_or_default().to_string();
    if token.is_empty() {
        return Err(SecretError::auth("Microsoft Entra ID returned no access token."));
    }
    let expires_in = match v.get("expires_in") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    };
    Ok((token, Duration::from_secs(expires_in.unwrap_or(3600))))
}

async fn cli_token(env: &Env, s: &AzureSettings, resource: &str) -> Result<(String, Duration), SecretError> {
    let program = cli::find(&cli::AZ, &s.cli_path, &env.process)?;
    let mut args: Vec<OsString> =
        ["account", "get-access-token", "--resource", resource, "--output", "json"].iter().map(Into::into).collect();
    if !s.tenant_id.trim().is_empty() {
        args.push("--tenant".into());
        args.push(s.tenant_id.trim().into());
    }
    let out = cli::run(&program, &args, &[], env.cli_timeout, "Azure CLI").await?;
    if !out.success {
        let line = out.error_line(&[]);
        let lower = line.to_ascii_lowercase();
        return Err(if lower.contains("az login") || lower.contains("aadsts") || lower.contains("expired") {
            SecretError::auth(format!(
                "The Azure CLI is not signed in or its session expired ({line}). Run `az login`."
            ))
        } else {
            SecretError::auth(format!("The Azure CLI could not get an access token: {line}"))
        });
    }
    let v: Value = serde_json::from_slice(&out.stdout)
        .map_err(|_| SecretError::auth("The Azure CLI did not print the expected JSON token."))?;
    let token = v.get("accessToken").and_then(Value::as_str).unwrap_or_default().to_string();
    if token.is_empty() {
        return Err(SecretError::auth("The Azure CLI returned no access token."));
    }
    let now = chrono::Utc::now().timestamp();
    let expires_at = match v.get("expires_on") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
    .or_else(|| {
        v.get("expiresOn").and_then(Value::as_str).and_then(|s| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .and_then(|n| n.and_local_timezone(chrono::Local).single())
                .map(|t| t.timestamp())
        })
    });
    let lifetime = expires_at.map(|t| (t - now).max(0) as u64).unwrap_or(300);
    Ok((token, Duration::from_secs(lifetime)))
}

/// GET with the bearer token; a 401 renews a cached token once.
async fn get(env: &Env, id: &str, s: &AzureSettings, url: &str) -> Result<(HttpReply, String), SecretError> {
    let mut tok = token(env, id, s, false).await?;
    let mut retried = false;
    loop {
        let reply = send(env.http().get(url).bearer_auth(&tok), WHAT, env.request_timeout).await?;
        if reply.status == 401 && !retried {
            retried = true;
            env.drop_token(id);
            tok = token(env, id, s, true).await?;
            continue;
        }
        return Ok((reply, tok));
    }
}

fn status_error(reply: &HttpReply, token: &str, s: &AzureSettings, what: &str, permission: &str) -> SecretError {
    let detail = reply.error_text(&[token]);
    let detail =
        if detail.is_empty() { format!("HTTP {}", reply.status) } else { format!("HTTP {}: {detail}", reply.status) };
    match reply.status {
        401 => SecretError::auth(format!(
            "Azure Key Vault rejected the access token from {} ({detail}). Check the tenant and credentials{}.",
            auth_label(s),
            if s.auth == AzureAuth::AzureCli { " or run `az login` again" } else { "" }
        )),
        403 => SecretError::denied(format!(
            "Azure Key Vault denied {what} ({detail}). Grant the identity the \"Key Vault Secrets User\" role (Azure RBAC) or a \"{permission}\" secret access policy; also check the vault's firewall."
        )),
        404 => SecretError::not_found(format!("{what} was not found in {} ({detail}).", base_url(&s.vault_url))),
        _ => SecretError::provider(format!("Azure Key Vault returned an error for {what} ({detail}).")),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 127 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `name[/version][#key]` -> (reference, name, version).
fn parse_ref(reference: &str) -> Result<(Reference, String, String), SecretError> {
    let r = Reference::parse(reference)?;
    r.only_params(&[], WHAT)?;
    let mut parts = r.path.split('/');
    let name = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or_default().to_string();
    if parts.next().is_some() || !valid_name(&name) {
        return Err(SecretError::reference(format!(
            "\"{}\" is not a Key Vault secret reference: use secret-name, secret-name/version or secret-name#jsonKey (names use letters, digits and dashes).",
            r.path
        )));
    }
    Ok((r, name, version))
}

pub(crate) fn check_reference(reference: &str) -> Result<(), SecretError> {
    parse_ref(reference).map(|_| ())
}

pub(crate) async fn fetch(env: &Env, id: &str, s: &AzureSettings, reference: &str) -> Result<String, SecretError> {
    let (r, name, version) = parse_ref(reference)?;
    let vault = base_url(&s.vault_url);
    let mut url = format!("{vault}/secrets/{name}");
    if !version.is_empty() {
        url.push('/');
        url.push_str(&percent_encoding::utf8_percent_encode(&version, percent_encoding::NON_ALPHANUMERIC).to_string());
    }
    url.push_str(&format!("?api-version={}", api_version(s)));
    let label = format!("Azure secret \"{name}\"");
    let (reply, tok) = get(env, id, s, &url).await?;
    if !reply.ok() {
        return Err(status_error(&reply, &tok, s, &label, "Get"));
    }
    let v = reply.json(WHAT)?;
    let Some(value) = v.get("value").and_then(Value::as_str) else {
        return Err(SecretError::not_found(format!("{label} has no value.")));
    };
    match &r.field {
        Some(key) => extract_json_key(value, key, &label),
        None => Ok(value.to_string()),
    }
}

/// Secret names, following `nextLink` only within the vault.
async fn list(
    env: &Env,
    id: &str,
    s: &AzureSettings,
    max: usize,
    max_pages: usize,
) -> Result<Vec<String>, SecretError> {
    let vault = base_url(&s.vault_url);
    let mut url = format!("{vault}/secrets?api-version={}&maxresults={max}", api_version(s));
    let mut names = Vec::new();
    for _ in 0..max_pages {
        let (reply, tok) = get(env, id, s, &url).await?;
        if !reply.ok() {
            return Err(status_error(&reply, &tok, s, "listing secrets", "List"));
        }
        let v = reply.json(WHAT)?;
        if let Some(items) = v.get("value").and_then(Value::as_array) {
            names.extend(
                items
                    .iter()
                    .filter_map(|i| i.get("id").and_then(Value::as_str))
                    .filter_map(|id| id.trim_end_matches('/').rsplit('/').next())
                    .map(str::to_string),
            );
        }
        match v.get("nextLink").and_then(Value::as_str) {
            Some(next) if next.starts_with(&format!("{vault}/")) => url = next.to_string(),
            _ => break,
        }
    }
    Ok(names)
}

pub(crate) async fn test(env: &Env, id: &str, s: &AzureSettings) -> Result<String, SecretError> {
    let base = format!("Connected to {} (API {}) using {}", base_url(&s.vault_url), api_version(s), auth_label(s));
    token(env, id, s, true).await?;
    match list(env, id, s, 1, 1).await {
        Ok(n) => Ok(format!("{base}; {}", if n.is_empty() { "no secrets visible" } else { "secrets are visible" })),
        Err(e) if e.kind == ErrorKind::PermissionDenied => Ok(format!(
            "{base}. The credentials are valid, but listing secrets is not allowed; reading secrets may still work."
        )),
        Err(e) => Err(e),
    }
}

/// `""` or a prefix lists secrets; `name#` lists the JSON keys of one.
pub(crate) async fn browse(
    env: &Env,
    id: &str,
    s: &AzureSettings,
    path: &str,
) -> Result<Vec<BrowseEntry>, SecretError> {
    if let Some(name) = path.strip_suffix('#') {
        let value = fetch(env, id, s, name).await?;
        let keys: Vec<String> = serde_json::from_str::<Value>(&value)
            .ok()
            .and_then(|j| j.as_object().map(|m| m.keys().cloned().collect()))
            .unwrap_or_default();
        return Ok(keys.into_iter().map(|k| BrowseEntry::field(k.clone(), format!("{name}#{k}"))).collect());
    }
    let prefix = path.trim().to_ascii_lowercase();
    let names = list(env, id, s, 25, 40).await?;
    Ok(names
        .into_iter()
        .filter(|n| n.to_ascii_lowercase().starts_with(&prefix))
        .map(|n| BrowseEntry { path: Some(format!("{n}#")), ..BrowseEntry::secret(n.clone(), n, false) })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert_eq!(resource(&u("https://my-vault.vault.azure.net")), "https://vault.azure.net");
        assert_eq!(resource(&u("https://kv.vault.azure.cn/")), "https://vault.azure.cn");
        assert_eq!(resource(&u("https://kv.vault.usgovcloudapi.net")), "https://vault.usgovcloudapi.net");
        assert_eq!(resource(&u("http://127.0.0.1:9000")), "https://vault.azure.net");
        assert!(valid_name("db-password-2"));
        assert!(!valid_name("db_password"));
    }
}
