//! 1Password through the `op` CLI (`op read`) or a Connect server.
//!
//! References are secret references: `op://vault/item[/section]/field`
//! (the `op://` prefix is optional). Vaults and items may be names or ids.

use std::ffi::OsString;

use serde_json::Value;

use crate::cli;
use crate::env::{Env, base_url, send};
use crate::error::SecretError;
use crate::model::{OnePasswordMode, OnePasswordSettings, check_url};
use crate::providers::BrowseEntry;

const CLI: &str = "1Password CLI";
const CONNECT: &str = "1Password Connect";

/// `op://vault/item[/section]/field[?query]`, normalized.
#[derive(Debug, PartialEq, Eq)]
struct OpRef {
    vault: String,
    item: String,
    section: Option<String>,
    field: String,
    query: Option<String>,
}

impl OpRef {
    fn parse(input: &str) -> Result<OpRef, SecretError> {
        let raw = input.trim();
        let body = raw.strip_prefix("op://").unwrap_or(raw).trim_start_matches('/');
        let (path, query) = match body.split_once('?') {
            Some((p, q)) => (p, Some(q.to_string())),
            None => (body, None),
        };
        let parts: Vec<&str> = path.split('/').map(str::trim).collect();
        let bad = || {
            SecretError::reference(format!(
                "\"{raw}\" is not a 1Password secret reference; use op://vault/item/field or op://vault/item/section/field."
            ))
        };
        if parts.iter().any(|p| p.is_empty()) {
            return Err(bad());
        }
        match parts.as_slice() {
            [v, i, f] => {
                Ok(OpRef { vault: v.to_string(), item: i.to_string(), section: None, field: f.to_string(), query })
            }
            [v, i, s, f] => Ok(OpRef {
                vault: v.to_string(),
                item: i.to_string(),
                section: Some(s.to_string()),
                field: f.to_string(),
                query,
            }),
            _ => Err(bad()),
        }
    }

    fn uri(&self) -> String {
        let mut s = format!("op://{}/{}/", self.vault, self.item);
        if let Some(sec) = &self.section {
            s.push_str(sec);
            s.push('/');
        }
        s.push_str(&self.field);
        if let Some(q) = &self.query {
            s.push('?');
            s.push_str(q);
        }
        s
    }
}

/// Name usable in a reference segment (ids for names containing `/`).
fn segment(name: &str, id: &str) -> String {
    if name.is_empty() || name.contains('/') || name.contains('?') { id.to_string() } else { name.to_string() }
}

// ---------------------------------------------------------------------------
// CLI

async fn op(env: &Env, id: &str, s: &OnePasswordSettings, args: &[&str]) -> Result<Vec<u8>, SecretError> {
    let program = cli::find(&cli::OP, &s.cli_path, &env.process)?;
    let mut argv: Vec<OsString> = args.iter().map(Into::into).collect();
    let account = s.account.trim();
    if !account.is_empty() {
        argv.push("--account".into());
        argv.push(account.into());
    }
    let sa_token = env.credential(id, "service_account_token");
    let mut envs = Vec::new();
    if let Some(t) = &sa_token {
        envs.push(("OP_SERVICE_ACCOUNT_TOKEN", t.clone()));
    }
    let out = cli::run(&program, &argv, &envs, env.cli_timeout, CLI).await?;
    if out.success {
        return Ok(out.stdout);
    }
    let secrets: Vec<&str> = sa_token.iter().map(String::as_str).collect();
    let line = out.error_line(&secrets);
    Err(classify_cli_error(&line, account))
}

fn classify_cli_error(line: &str, account: &str) -> SecretError {
    let l = line.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    if l.contains("account") && has(&["not found", "no account", "could not find", "isn't an account"]) {
        let which = if account.is_empty() {
            "The default 1Password account".to_string()
        } else {
            format!("Account \"{account}\"")
        };
        return SecretError::config(format!(
            "{which} is not configured in the 1Password CLI ({line}). Check the account setting or run `op account list`."
        ));
    }
    if has(&[
        "not currently signed in",
        "not signed in",
        "session expired",
        "sign in",
        "signin",
        "authorization prompt dismissed",
        "authorization timeout",
        "unauthorized",
        "invalid token",
        "authentication",
        "no accounts configured",
        "connecting to desktop app",
        "service account",
    ]) {
        return SecretError::auth(format!(
            "The 1Password CLI is not signed in ({line}). Unlock the 1Password app with \"Integrate with 1Password CLI\" enabled, run `op signin`, or set a service account token."
        ));
    }
    if has(&["forbidden", "permission", "access denied", "not authorized"]) {
        return SecretError::denied(format!("1Password denied access ({line})."));
    }
    if has(&[
        "isn't an item",
        "isn't a vault",
        "isn't a field",
        "not found",
        "could not find",
        "no item",
        "doesn't exist",
        "does not exist",
        "no such",
    ]) {
        return SecretError::not_found(format!("1Password: {line}"));
    }
    SecretError::provider(format!("The 1Password CLI failed: {line}"))
}

fn cli_json(bytes: &[u8]) -> Result<Value, SecretError> {
    serde_json::from_slice(bytes).map_err(|_| SecretError::provider("The 1Password CLI printed unexpected output."))
}

async fn cli_fetch(env: &Env, id: &str, s: &OnePasswordSettings, r: &OpRef) -> Result<String, SecretError> {
    let uri = r.uri();
    let out = op(env, id, s, &["read", "--no-newline", &uri]).await?;
    let mut value =
        String::from_utf8(out).map_err(|_| SecretError::provider(format!("{uri} is not valid UTF-8 text.")))?;
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    Ok(value)
}

async fn cli_test(env: &Env, id: &str, s: &OnePasswordSettings) -> Result<String, SecretError> {
    let version = op(env, id, s, &["--version"])
        .await
        .map(|v| String::from_utf8_lossy(&v).trim().to_string())
        .unwrap_or_default();
    let vaults = cli_json(&op(env, id, s, &["vault", "list", "--format", "json"]).await?)?;
    let n = vaults.as_array().map(Vec::len).unwrap_or(0);
    let who = if s.account.trim().is_empty() { String::new() } else { format!(" (account {})", s.account.trim()) };
    let version = if version.is_empty() { String::new() } else { format!(" {version}") };
    Ok(format!("1Password CLI{version}{who}: {n} vault{} accessible", if n == 1 { "" } else { "s" }))
}

async fn cli_browse(
    env: &Env,
    id: &str,
    s: &OnePasswordSettings,
    parts: &[&str],
) -> Result<Vec<BrowseEntry>, SecretError> {
    match parts {
        [] => {
            let v = cli_json(&op(env, id, s, &["vault", "list", "--format", "json"]).await?)?;
            Ok(named_list(&v, "name", |seg| format!("{seg}/")))
        }
        [vault] => {
            let v = cli_json(&op(env, id, s, &["item", "list", "--vault", vault, "--format", "json"]).await?)?;
            Ok(named_list(&v, "title", |seg| format!("{vault}/{seg}/")))
        }
        [vault, item] => {
            let v = cli_json(&op(env, id, s, &["item", "get", item, "--vault", vault, "--format", "json"]).await?)?;
            Ok(field_entries(&v, vault, item))
        }
        _ => Ok(Vec::new()),
    }
}

/// Folders from a JSON array of `{id, <name_key>}`.
fn named_list(v: &Value, name_key: &str, path: impl Fn(&str) -> String) -> Vec<BrowseEntry> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    let name = e.get(name_key).and_then(Value::as_str).unwrap_or_default();
                    let id = e.get("id").and_then(Value::as_str).unwrap_or_default();
                    let seg = segment(name, id);
                    BrowseEntry::folder(if name.is_empty() { id } else { name }, path(&seg))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Field rows of an item (CLI and Connect use the same shape); values are dropped.
fn field_entries(item: &Value, vault: &str, item_seg: &str) -> Vec<BrowseEntry> {
    let sections: Vec<(String, String)> = item
        .get("sections")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|s| {
                    let id = s.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    let label = s.get("label").and_then(Value::as_str).unwrap_or_default().to_string();
                    (id, label)
                })
                .collect()
        })
        .unwrap_or_default();
    let Some(fields) = item.get("fields").and_then(Value::as_array) else {
        return Vec::new();
    };
    fields
        .iter()
        .filter_map(|f| {
            let id = f.get("id").and_then(Value::as_str).unwrap_or_default();
            let label = f.get("label").and_then(Value::as_str).unwrap_or_default();
            if id.is_empty() && label.is_empty() {
                return None;
            }
            let field_seg = segment(label, id);
            let section = f.get("section").and_then(|s| {
                let sid = s.get("id").and_then(Value::as_str).unwrap_or_default();
                let slabel = s
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| sections.iter().find(|(i, _)| i == sid).map(|(_, l)| l.clone()))
                    .unwrap_or_default();
                (!sid.is_empty() && sid != "add more").then(|| (segment(&slabel, sid), slabel))
            });
            let reference =
                f.get("reference").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| match &section {
                    Some((sseg, _)) => format!("op://{vault}/{item_seg}/{sseg}/{field_seg}"),
                    None => format!("op://{vault}/{item_seg}/{field_seg}"),
                });
            let shown = if label.is_empty() { id } else { label };
            let name = match &section {
                Some((_, sl)) if !sl.is_empty() => format!("{sl} / {shown}"),
                _ => shown.to_string(),
            };
            Some(BrowseEntry::field(name, reference))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Connect

async fn connect_get(
    env: &Env,
    id: &str,
    s: &OnePasswordSettings,
    path: &str,
    what: &str,
) -> Result<Value, SecretError> {
    check_url("1Password Connect URL", &s.connect_url)?;
    let token = env.required_credential(id, "connect_token", "Connect token", CONNECT)?;
    let url = format!("{}{path}", base_url(&s.connect_url));
    let reply = send(env.http().get(url).bearer_auth(&token), CONNECT, env.request_timeout).await?;
    if reply.ok() {
        return reply.json(CONNECT);
    }
    let detail = reply.error_text(&[token.as_str()]);
    let detail =
        if detail.is_empty() { format!("HTTP {}", reply.status) } else { format!("HTTP {}: {detail}", reply.status) };
    Err(match reply.status {
        401 => SecretError::auth(format!("1Password Connect rejected the token ({detail}). Check the Connect token.")),
        403 => SecretError::denied(format!("The Connect token may not access {what} ({detail}).")),
        404 => SecretError::not_found(format!("{what} was not found in 1Password Connect ({detail}).")),
        _ => SecretError::provider(format!("1Password Connect returned an error for {what} ({detail}).")),
    })
}

/// Finds an entry by id, then exact name, then case-insensitive name.
fn find_named<'a>(list: &'a Value, key: &str, wanted: &str) -> Option<&'a Value> {
    let a = list.as_array()?;
    let name = |e: &Value| e.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    a.iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(wanted))
        .or_else(|| a.iter().find(|e| name(e) == wanted))
        .or_else(|| a.iter().find(|e| name(e).eq_ignore_ascii_case(wanted)))
}

fn id_of(v: &Value) -> String {
    v.get("id").and_then(Value::as_str).unwrap_or_default().to_string()
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

async fn connect_vault_id(env: &Env, id: &str, s: &OnePasswordSettings, vault: &str) -> Result<String, SecretError> {
    let vaults = connect_get(env, id, s, "/v1/vaults", "the vault list").await?;
    find_named(&vaults, "name", vault).map(id_of).ok_or_else(|| {
        SecretError::not_found(format!(
            "1Password vault \"{vault}\" was not found or the Connect token cannot access it."
        ))
    })
}

async fn connect_item(
    env: &Env,
    id: &str,
    s: &OnePasswordSettings,
    vault: &str,
    item: &str,
) -> Result<Value, SecretError> {
    let vid = connect_vault_id(env, id, s, vault).await?;
    let items =
        connect_get(env, id, s, &format!("/v1/vaults/{}/items", enc(&vid)), &format!("vault \"{vault}\"")).await?;
    let iid = find_named(&items, "title", item).map(id_of).ok_or_else(|| {
        SecretError::not_found(format!("1Password item \"{item}\" was not found in vault \"{vault}\"."))
    })?;
    connect_get(env, id, s, &format!("/v1/vaults/{}/items/{}", enc(&vid), enc(&iid)), &format!("item \"{item}\"")).await
}

fn pick_connect_field(item: &Value, r: &OpRef) -> Result<String, SecretError> {
    let empty = Vec::new();
    let fields = item.get("fields").and_then(Value::as_array).unwrap_or(&empty);
    let section_id = match &r.section {
        None => None,
        Some(sec) => {
            let sections = item.get("sections").cloned().unwrap_or(Value::Null);
            Some(find_named(&sections, "label", sec).map(id_of).ok_or_else(|| {
                SecretError::not_found(format!("Section \"{sec}\" was not found in 1Password item \"{}\".", r.item))
            })?)
        }
    };
    let in_section: Vec<&Value> = fields
        .iter()
        .filter(|f| match &section_id {
            None => true,
            Some(sid) => f.pointer("/section/id").and_then(Value::as_str) == Some(sid.as_str()),
        })
        .collect();
    let candidates = Value::Array(in_section.iter().map(|f| (*f).clone()).collect());
    let field = find_named(&candidates, "label", &r.field).ok_or_else(|| {
        let mut labels: Vec<&str> = in_section.iter().filter_map(|f| f.get("label").and_then(Value::as_str)).collect();
        labels.sort();
        SecretError::not_found(format!(
            "Field \"{}\" was not found in 1Password item \"{}\" (fields: {}).",
            r.field,
            r.item,
            if labels.is_empty() { "none".into() } else { labels.join(", ") }
        ))
    })?;
    match field.get("value") {
        Some(Value::String(v)) => Ok(v.clone()),
        Some(Value::Null) | None => {
            Err(SecretError::not_found(format!("Field \"{}\" of item \"{}\" is empty.", r.field, r.item)))
        }
        Some(other) => Ok(other.to_string()),
    }
}

async fn connect_fetch(env: &Env, id: &str, s: &OnePasswordSettings, r: &OpRef) -> Result<String, SecretError> {
    if r.query.is_some() {
        return Err(SecretError::reference(
            "Reference options (?attribute=...) are only supported with the 1Password CLI.",
        ));
    }
    let item = connect_item(env, id, s, &r.vault, &r.item).await?;
    pick_connect_field(&item, r)
}

async fn connect_browse(
    env: &Env,
    id: &str,
    s: &OnePasswordSettings,
    parts: &[&str],
) -> Result<Vec<BrowseEntry>, SecretError> {
    match parts {
        [] => {
            let v = connect_get(env, id, s, "/v1/vaults", "the vault list").await?;
            Ok(named_list(&v, "name", |seg| format!("{seg}/")))
        }
        [vault] => {
            let vid = connect_vault_id(env, id, s, vault).await?;
            let v = connect_get(env, id, s, &format!("/v1/vaults/{}/items", enc(&vid)), &format!("vault \"{vault}\""))
                .await?;
            Ok(named_list(&v, "title", |seg| format!("{vault}/{seg}/")))
        }
        [vault, item] => {
            let v = connect_item(env, id, s, vault, item).await?;
            Ok(field_entries(&v, vault, item))
        }
        _ => Ok(Vec::new()),
    }
}

// ---------------------------------------------------------------------------

pub(crate) async fn fetch(
    env: &Env,
    id: &str,
    s: &OnePasswordSettings,
    reference: &str,
) -> Result<String, SecretError> {
    let r = OpRef::parse(reference)?;
    match s.mode {
        OnePasswordMode::Cli => cli_fetch(env, id, s, &r).await,
        OnePasswordMode::Connect => connect_fetch(env, id, s, &r).await,
    }
}

pub(crate) async fn test(env: &Env, id: &str, s: &OnePasswordSettings) -> Result<String, SecretError> {
    match s.mode {
        OnePasswordMode::Cli => cli_test(env, id, s).await,
        OnePasswordMode::Connect => {
            let v = connect_get(env, id, s, "/v1/vaults", "the vault list").await?;
            let n = v.as_array().map(Vec::len).unwrap_or(0);
            Ok(format!(
                "Connected to 1Password Connect at {}; {n} vault{} accessible",
                base_url(&s.connect_url),
                if n == 1 { "" } else { "s" }
            ))
        }
    }
}

/// `""` lists vaults, `vault/` items, `vault/item/` fields.
pub(crate) async fn browse(
    env: &Env,
    id: &str,
    s: &OnePasswordSettings,
    path: &str,
) -> Result<Vec<BrowseEntry>, SecretError> {
    let p = path.trim();
    let p = p.strip_prefix("op://").unwrap_or(p).trim_matches('/');
    let parts: Vec<&str> = if p.is_empty() { Vec::new() } else { p.split('/').collect() };
    match s.mode {
        OnePasswordMode::Cli => cli_browse(env, id, s, &parts).await,
        OnePasswordMode::Connect => connect_browse(env, id, s, &parts).await,
    }
}

pub(crate) fn check_reference(reference: &str) -> Result<(), SecretError> {
    OpRef::parse(reference).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use serde_json::json;

    #[test]
    fn parse_refs() {
        let r = OpRef::parse("op://Dev/DB/password").unwrap();
        assert_eq!(
            (r.vault.as_str(), r.item.as_str(), r.section.as_deref(), r.field.as_str()),
            ("Dev", "DB", None, "password")
        );
        assert_eq!(r.uri(), "op://Dev/DB/password");
        let r = OpRef::parse("Dev/DB/admin/one-time password?attribute=otp").unwrap();
        assert_eq!(r.section.as_deref(), Some("admin"));
        assert_eq!(r.uri(), "op://Dev/DB/admin/one-time password?attribute=otp");
        assert!(OpRef::parse("op://Dev/DB").is_err());
        assert!(OpRef::parse("op://Dev//x").is_err());
    }

    #[test]
    fn classify() {
        assert_eq!(
            classify_cli_error("You are not currently signed in. Please run `op signin`", "").kind,
            ErrorKind::Auth
        );
        assert_eq!(classify_cli_error("\"nope\" isn't an item in the \"Dev\" vault", "").kind, ErrorKind::NotFound);
        assert_eq!(classify_cli_error("account \"x\" not found", "x").kind, ErrorKind::Config);
        assert_eq!(classify_cli_error("something odd", "").kind, ErrorKind::Provider);
    }

    #[test]
    fn connect_fields() {
        let item = json!({
            "id": "i1", "title": "DB",
            "sections": [{"id": "s1", "label": "admin"}],
            "fields": [
                {"id": "password", "label": "password", "value": "p1"},
                {"id": "f2", "label": "password", "value": "p2", "section": {"id": "s1"}},
                {"id": "f3", "label": "empty"}
            ]
        });
        let r = |s: &str| OpRef::parse(s).unwrap();
        assert_eq!(pick_connect_field(&item, &r("v/DB/password")).unwrap(), "p1");
        assert_eq!(pick_connect_field(&item, &r("v/DB/admin/password")).unwrap(), "p2");
        assert_eq!(pick_connect_field(&item, &r("v/DB/ADMIN/f2")).unwrap(), "p2");
        assert_eq!(pick_connect_field(&item, &r("v/DB/empty")).unwrap_err().kind, ErrorKind::NotFound);
        let e = pick_connect_field(&item, &r("v/DB/nope")).unwrap_err();
        assert!(e.message.contains("empty, password, password") && !e.message.contains("p1"), "{e}");
        let entries = field_entries(&item, "v", "DB");
        assert_eq!(entries[1].reference.as_deref(), Some("op://v/DB/admin/password"));
        assert_eq!(entries[1].name, "admin / password");
    }
}
