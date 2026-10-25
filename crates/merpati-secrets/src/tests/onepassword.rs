//! 1Password: a Connect server mock and a fake `op` CLI.

use serde_json::json;

use super::{Req, json, manager, serve};
use crate::{EntryKind, ErrorKind, OnePasswordMode, OnePasswordSettings, ProviderSettings};

async fn connect_handler(r: Req) -> axum::response::Response {
    if r.bearer() != Some("connect-token") {
        return json(401, json!({"status": 401, "message": "Invalid token signature"}));
    }
    match r.path.as_str() {
        "/v1/vaults" => json(200, json!([{"id": "vlt1", "name": "Dev"}, {"id": "vlt2", "name": "Prod"}])),
        "/v1/vaults/vlt1/items" => {
            json(200, json!([{"id": "itm1", "title": "Database"}, {"id": "itm2", "title": "a/b"}]))
        }
        "/v1/vaults/vlt2/items" => {
            json(403, json!({"status": 403, "message": "Authorization: token has no access to vault"}))
        }
        "/v1/vaults/vlt1/items/itm1" => json(
            200,
            json!({
                "id": "itm1", "title": "Database",
                "sections": [{"id": "sec1", "label": "admin"}],
                "fields": [
                    {"id": "username", "label": "username", "value": "dbuser"},
                    {"id": "password", "label": "password", "value": "dbpass"},
                    {"id": "f3", "label": "password", "value": "adminpass", "section": {"id": "sec1"}}
                ]
            }),
        ),
        _ => json(404, json!({"status": 404, "message": "not found"})),
    }
}

fn connect_settings(url: &str) -> OnePasswordSettings {
    OnePasswordSettings { mode: OnePasswordMode::Connect, connect_url: url.into(), ..Default::default() }
}

#[tokio::test]
async fn connect_resolve_browse_and_errors() {
    let server = serve(connect_handler);
    let ext = manager(ProviderSettings::OnePassword(connect_settings(&server.url)));
    let e = ext.resolve_reference("p1", "op://Dev/Database/password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);

    ext.set_credential("p1", "connect_token", "bad-token");
    let e = ext.test_provider(&ext.provider("p1").unwrap()).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("Invalid token signature") && !e.message.contains("bad-token"), "{e}");

    ext.set_credential("p1", "connect_token", "connect-token");
    let summary = ext.test_provider(&ext.provider("p1").unwrap()).await.unwrap();
    assert!(summary.contains("2 vaults"), "{summary}");
    assert_eq!(ext.resolve_reference("p1", "op://Dev/Database/password").await.unwrap(), "dbpass");
    assert_eq!(ext.resolve_reference("p1", "op://vlt1/itm1/username").await.unwrap(), "dbuser");
    assert_eq!(ext.resolve_reference("p1", "dev/database/admin/password").await.unwrap(), "adminpass");

    let e = ext.resolve_reference("p1", "op://Nope/Database/password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    let e = ext.resolve_reference("p1", "op://Dev/Database/nope").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("password, password, username") && !e.message.contains("dbpass"), "{e}");
    let e = ext.resolve_reference("p1", "op://Prod/x/y").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    let e = ext.resolve_reference("p1", "op://Dev/Database/password?attribute=otp").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidReference);

    let vaults = ext.browse("p1", "").await.unwrap();
    assert_eq!(vaults.iter().map(|e| e.path.clone().unwrap()).collect::<Vec<_>>(), ["Dev/", "Prod/"]);
    let items = ext.browse("p1", "Dev/").await.unwrap();
    // Titles with a slash are addressed by id.
    assert_eq!(items.iter().map(|e| e.path.clone().unwrap()).collect::<Vec<_>>(), ["Dev/itm2/", "Dev/Database/"]);
    let fields = ext.browse("p1", "Dev/Database/").await.unwrap();
    assert!(fields.iter().all(|f| f.kind == EntryKind::Field));
    let refs: Vec<&str> = fields.iter().filter_map(|e| e.reference.as_deref()).collect();
    assert_eq!(refs, ["op://Dev/Database/admin/password", "op://Dev/Database/password", "op://Dev/Database/username"]);
}

#[cfg(unix)]
pub(crate) const FAKE_OP: &str = r#"#!/bin/sh
# Fake 1Password CLI for tests.
case "$*" in
  *"--account other"*) echo '[ERROR] 2024/01/01 10:00:00 no account found for filter "other"' >&2; exit 1;;
esac
if [ "$1" = "--version" ]; then echo "2.30.0"; exit 0; fi
if [ -n "$OP_SERVICE_ACCOUNT_TOKEN" ] && [ "$OP_SERVICE_ACCOUNT_TOKEN" != "ops_good" ]; then
  echo "[ERROR] 2024/01/01 10:00:00 invalid token: service account token $OP_SERVICE_ACCOUNT_TOKEN is invalid" >&2; exit 1
fi
case "$1 $2" in
  "vault list") echo '[{"id":"vlt1","name":"Dev"}]'; exit 0;;
  "item list") echo '[{"id":"itm1","title":"Database"}]'; exit 0;;
  "item get") echo '{"id":"itm1","title":"Database","fields":[{"id":"password","label":"password","value":"dbpass","reference":"op://Dev/Database/password"}]}'; exit 0;;
esac
if [ "$1" = "read" ] && [ "$2" = "--no-newline" ]; then
  case "$3" in
    "op://Dev/Database/password") printf '%s' "dbpass"; exit 0;;
    "op://Dev/Database/multi line") printf 'line1\nline2'; exit 0;;
    "op://Locked/x/y") echo '[ERROR] 2024/01/01 10:00:00 You are not currently signed in. Please run `op signin --help` for instructions' >&2; exit 1;;
    *) echo "[ERROR] 2024/01/01 10:00:00 could not read secret '$3': \"x\" isn't an item in the \"Dev\" vault." >&2; exit 1;;
  esac
fi
echo "[ERROR] unknown command $*" >&2; exit 1
"#;

#[cfg(unix)]
#[tokio::test]
async fn cli_read_test_browse_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let op = super::script(dir.path(), "op", FAKE_OP);
    let s = OnePasswordSettings { cli_path: op.to_string_lossy().into(), ..Default::default() };
    let ext = manager(ProviderSettings::OnePassword(s.clone()));
    assert_eq!(ext.resolve_reference("p1", "op://Dev/Database/password").await.unwrap(), "dbpass");
    assert_eq!(ext.resolve_reference("p1", "Dev/Database/password").await.unwrap(), "dbpass");
    assert_eq!(ext.resolve_reference("p1", "op://Dev/Database/multi line").await.unwrap(), "line1\nline2");
    let e = ext.resolve_reference("p1", "op://Locked/x/y").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("op signin"), "{e}");
    let e = ext.resolve_reference("p1", "op://Dev/x/y").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    let e = ext.resolve_reference("p1", "op://Dev/x").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidReference);

    let summary = ext.test_provider(&ext.provider("p1").unwrap()).await.unwrap();
    assert_eq!(summary, "1Password CLI 2.30.0: 1 vault accessible");
    let vaults = ext.browse("p1", "").await.unwrap();
    assert_eq!(vaults[0].path.as_deref(), Some("Dev/"));
    let items = ext.browse("p1", "Dev/").await.unwrap();
    assert_eq!(items[0].path.as_deref(), Some("Dev/Database/"));
    let fields = ext.browse("p1", "Dev/Database/").await.unwrap();
    assert_eq!(fields[0].reference.as_deref(), Some("op://Dev/Database/password"));

    // Service account token is passed via the environment and scrubbed from errors.
    ext.set_credential("p1", "service_account_token", "ops_bad_token");
    let e = ext.resolve_reference("p1", "op://Dev/Database/password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(!e.message.contains("ops_bad_token"), "{e}");
    ext.set_credential("p1", "service_account_token", "ops_good");
    assert_eq!(ext.resolve_reference("p1", "op://Dev/Database/password").await.unwrap(), "dbpass");

    // Unknown account.
    let ext = manager(ProviderSettings::OnePassword(OnePasswordSettings { account: "other".into(), ..s }));
    let e = ext.resolve_reference("p1", "op://Dev/Database/password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config);
    assert!(e.message.contains("\"other\""), "{e}");

    // CLI missing.
    let missing =
        OnePasswordSettings { cli_path: dir.path().join("nope").to_string_lossy().into(), ..Default::default() };
    let ext = manager(ProviderSettings::OnePassword(missing));
    let e = ext.test_provider(&ext.provider("p1").unwrap()).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::CliMissing);
    assert!(e.message.contains("developer.1password.com"), "{e}");
}
