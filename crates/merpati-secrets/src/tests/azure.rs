//! Azure Key Vault against a mock token endpoint and vault.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde_json::json;

use super::{Req, json, manager, serve};
use crate::{AzureAuth, AzureSettings, ErrorKind, ProviderSettings};

#[derive(Default)]
struct AzureMock {
    token_requests: AtomicUsize,
    list_requests: AtomicUsize,
    /// Tokens the vault rejects with 401.
    revoked: Mutex<Vec<String>>,
    api_versions: Mutex<Vec<String>>,
    base: Mutex<String>,
}

async fn handler(st: Arc<AzureMock>, r: Req) -> axum::response::Response {
    if r.path == "/tenant-1/oauth2/v2.0/token" {
        let n = st.token_requests.fetch_add(1, Ordering::SeqCst) + 1;
        let f = r.form();
        let get = |k: &str| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap_or_default();
        if get("grant_type") != "client_credentials" || get("scope") != "https://vault.azure.net/.default" {
            return json(400, json!({"error": "invalid_request", "error_description": "AADSTS900144: bad request"}));
        }
        if get("client_id") != "client-1" {
            return json(
                400,
                json!({"error": "unauthorized_client", "error_description": "AADSTS700016: Application with identifier 'x' was not found in the directory"}),
            );
        }
        if get("client_secret") != "csecret" {
            return json(
                401,
                json!({"error": "invalid_client", "error_description": format!("AADSTS7000215: Invalid client secret provided ({}).\r\nTrace ID: 1", get("client_secret"))}),
            );
        }
        return json(
            200,
            json!({"token_type": "Bearer", "expires_in": 3599, "access_token": format!("aad-token-{n}")}),
        );
    }
    if r.path.starts_with("/unknown-tenant/") {
        return json(
            400,
            json!({"error": "invalid_request", "error_description": "AADSTS90002: Tenant 'unknown-tenant' not found."}),
        );
    }
    let token = r.bearer().unwrap_or_default().to_string();
    if !(token.starts_with("aad-token-") || token == "cli-token") || st.revoked.lock().contains(&token) {
        return json(
            401,
            json!({"error": {"code": "Unauthorized", "message": "AKV10000: Request is missing a Bearer or PoP token."}}),
        );
    }
    let version = r.param("api-version").unwrap_or_default().to_string();
    st.api_versions.lock().push(version.clone());
    if version != "7.5" && version != "7.4" {
        return json(
            400,
            json!({"error": {"code": "BadParameter", "message": "The specified version is not recognized."}}),
        );
    }
    let base = st.base.lock().clone();
    match r.path.as_str() {
        "/secrets" => {
            st.list_requests.fetch_add(1, Ordering::SeqCst);
            if r.param("$skiptoken").is_some() {
                // A nextLink to another host must not be followed.
                return json(
                    200,
                    json!({"value": [{"id": format!("{base}/secrets/json-secret")}], "nextLink": "http://evil.example/secrets?x=1"}),
                );
            }
            json(
                200,
                json!({"value": [{"id": format!("{base}/secrets/db-password")}], "nextLink": format!("{base}/secrets?api-version={version}&$skiptoken=abc")}),
            )
        }
        "/secrets/db-password" => {
            json(200, json!({"value": "azure-secret", "id": format!("{base}/secrets/db-password/v2")}))
        }
        "/secrets/db-password/v1" => {
            json(200, json!({"value": "azure-old", "id": format!("{base}/secrets/db-password/v1")}))
        }
        "/secrets/json-secret" => json(200, json!({"value": r#"{"user":"u","pass":"p"}"#})),
        "/secrets/forbidden" => json(
            403,
            json!({"error": {"code": "Forbidden", "message": "The user, group or application does not have secrets get permission on key vault."}}),
        ),
        p => {
            let name = p.trim_start_matches("/secrets/");
            json(
                404,
                json!({"error": {"code": "SecretNotFound", "message": format!("A secret with (name/id) {name} was not found in this key vault.")}}),
            )
        }
    }
}

fn start() -> (super::MockServer, Arc<AzureMock>) {
    let st = Arc::new(AzureMock::default());
    let s2 = st.clone();
    let server = serve(move |r| handler(s2.clone(), r));
    *st.base.lock() = server.url.clone();
    (server, st)
}

fn settings(url: &str) -> AzureSettings {
    AzureSettings {
        vault_url: url.into(),
        auth: AzureAuth::ClientSecret,
        tenant_id: "tenant-1".into(),
        client_id: "client-1".into(),
        authority_host: url.into(),
        api_version: String::new(),
        cli_path: String::new(),
    }
}

#[tokio::test]
async fn client_credentials_secret_get_and_versions() {
    let (server, st) = start();
    let ext = manager(ProviderSettings::AzureKeyVault(settings(&server.url)));
    let e = ext.resolve_reference("p1", "db-password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("client secret"), "{e}");

    ext.set_credential("p1", "client_secret", "wrong-secret");
    let e = ext.resolve_reference("p1", "db-password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("AADSTS7000215") && !e.message.contains("wrong-secret"), "{e}");

    ext.set_credential("p1", "client_secret", "csecret");
    assert_eq!(ext.resolve_reference("p1", "db-password").await.unwrap(), "azure-secret");
    assert_eq!(ext.resolve_reference("p1", "db-password/v1").await.unwrap(), "azure-old");
    assert_eq!(ext.resolve_reference("p1", "json-secret#pass").await.unwrap(), "p");
    // Token fetched once and reused.
    assert_eq!(st.token_requests.load(Ordering::SeqCst), 2);
    assert!(st.api_versions.lock().iter().all(|v| v == "7.5"));

    let e = ext.resolve_reference("p1", "nope").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("SecretNotFound"), "{e}");
    let e = ext.resolve_reference("p1", "forbidden").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    assert!(e.message.contains("Key Vault Secrets User"), "{e}");
    let e = ext.resolve_reference("p1", "bad_name").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidReference);

    // A revoked token is renewed once.
    st.revoked.lock().push("aad-token-2".into());
    assert_eq!(ext.resolve_reference("p1", "db-password").await.unwrap(), "azure-secret");
    assert_eq!(st.token_requests.load(Ordering::SeqCst), 3);

    // Custom API version is sent as-is.
    let mut s = settings(&server.url);
    s.api_version = "7.4".into();
    let ext2 = manager(ProviderSettings::AzureKeyVault(s));
    ext2.set_credential("p1", "client_secret", "csecret");
    assert_eq!(ext2.resolve_reference("p1", "db-password").await.unwrap(), "azure-secret");
    assert_eq!(st.api_versions.lock().last().unwrap(), "7.4");
}

#[tokio::test]
async fn list_test_and_bad_tenant() {
    let (server, st) = start();
    let ext = manager(ProviderSettings::AzureKeyVault(settings(&server.url)));
    ext.set_credential("p1", "client_secret", "csecret");
    let entries = ext.browse("p1", "").await.unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["db-password", "json-secret"]);
    assert_eq!(st.list_requests.load(Ordering::SeqCst), 2, "foreign nextLink not followed");
    let keys = ext.browse("p1", "json-secret#").await.unwrap();
    let refs: Vec<&str> = keys.iter().filter_map(|e| e.reference.as_deref()).collect();
    assert_eq!(refs, ["json-secret#pass", "json-secret#user"]);
    assert_eq!(ext.browse("p1", "db").await.unwrap().len(), 1);

    let summary = ext.test_provider(&ext.provider("p1").unwrap()).await.unwrap();
    assert!(summary.contains("API 7.5") && summary.contains("client credentials"), "{summary}");

    let mut bad = ext.provider("p1").unwrap();
    if let ProviderSettings::AzureKeyVault(s) = &mut bad.settings {
        s.tenant_id = "unknown-tenant".into();
    }
    let e = ext.test_provider(&bad).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config);
    assert!(e.message.contains("tenant"), "{e}");
}

#[cfg(unix)]
#[tokio::test]
async fn azure_cli_token() {
    let (server, _st) = start();
    let dir = tempfile::tempdir().unwrap();
    let az = super::script(
        dir.path(),
        "az",
        "#!/bin/sh\ncase \"$*\" in\n  *\"account get-access-token --resource https://vault.azure.net --output json --tenant tenant-1\"*)\n    echo '{\"accessToken\":\"cli-token\",\"expires_on\":4070908800,\"tokenType\":\"Bearer\"}';;\n  *) echo \"ERROR: Please run 'az login' to setup account.\" >&2; exit 1;;\nesac\n",
    );
    let mut s = settings(&server.url);
    s.auth = AzureAuth::AzureCli;
    s.cli_path = az.to_string_lossy().into();
    let ext = manager(ProviderSettings::AzureKeyVault(s.clone()));
    assert_eq!(ext.resolve_reference("p1", "db-password").await.unwrap(), "azure-secret");

    s.tenant_id = "other".into();
    let ext = manager(ProviderSettings::AzureKeyVault(s.clone()));
    let e = ext.resolve_reference("p1", "db-password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("az login"), "{e}");

    s.cli_path = dir.path().join("missing-az").to_string_lossy().into();
    let ext = manager(ProviderSettings::AzureKeyVault(s));
    assert_eq!(ext.resolve_reference("p1", "db-password").await.unwrap_err().kind, ErrorKind::CliMissing);
}
