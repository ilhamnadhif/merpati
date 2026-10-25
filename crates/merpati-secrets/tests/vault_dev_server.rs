//! Against a real HashiCorp Vault dev server:
//!
//! ```sh
//! docker run --rm -p 8200:8200 -e VAULT_DEV_ROOT_TOKEN_ID=root hashicorp/vault
//! cargo test -p merpati-secrets --test vault_dev_server -- --ignored
//! ```
//!
//! `VAULT_ADDR` (default `http://127.0.0.1:8200`) and `VAULT_TOKEN`
//! (default `root`) select the server.

use std::sync::Arc;

use serde_json::{Value, json};

use merpati_core::secrets::SecretStore;
use merpati_secrets::{
    EntryKind, ErrorKind, ExternalSecrets, KvVersion, LinkedSecret, ProviderConfig, ProviderSettings, SecretsConfig,
    VaultAuth, VaultSettings, default_client,
};

async fn admin(
    http: &reqwest::Client,
    addr: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> Value {
    let resp = http
        .request(method, format!("{addr}/v1/{path}"))
        .header("X-Vault-Token", token)
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("Vault dev server not reachable at {addr}: {e}"));
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(
        status.is_success() || text.contains("existing mount") || text.contains("path is already in use"),
        "{path}: {status} {text}"
    );
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

#[tokio::test]
#[ignore = "needs a Vault dev server (docker run --rm -p 8200:8200 -e VAULT_DEV_ROOT_TOKEN_ID=root hashicorp/vault)"]
async fn real_vault_dev_server() {
    let addr = std::env::var("VAULT_ADDR").unwrap_or_else(|_| "http://127.0.0.1:8200".into());
    let root = std::env::var("VAULT_TOKEN").unwrap_or_else(|_| "root".into());
    let http = default_client().unwrap();
    use reqwest::Method;

    // Seed: KV v2 secret, a KV v1 mount, AppRole with a read-only policy.
    admin(
        &http,
        &addr,
        &root,
        Method::POST,
        "secret/data/merpati/db",
        json!({"data": {"user": "app", "password": "pw-v1"}}),
    )
    .await;
    admin(
        &http,
        &addr,
        &root,
        Method::POST,
        "secret/data/merpati/db",
        json!({"data": {"user": "app", "password": "pw-v2"}}),
    )
    .await;
    admin(&http, &addr, &root, Method::POST, "sys/mounts/kv1", json!({"type": "kv", "options": {"version": "1"}}))
        .await;
    admin(&http, &addr, &root, Method::POST, "kv1/legacy", json!({"value": "v1-value"})).await;
    admin(&http, &addr, &root, Method::POST, "sys/auth/approle", json!({"type": "approle"})).await;
    let policy = r#"path "secret/data/merpati/*" { capabilities = ["read"] }
path "secret/metadata/merpati/*" { capabilities = ["list"] }"#;
    admin(&http, &addr, &root, Method::PUT, "sys/policies/acl/merpati-read", json!({"policy": policy})).await;
    admin(
        &http,
        &addr,
        &root,
        Method::POST,
        "auth/approle/role/merpati",
        json!({"token_policies": ["merpati-read"], "token_ttl": "10m"}),
    )
    .await;
    let role_id = admin(&http, &addr, &root, Method::GET, "auth/approle/role/merpati/role-id", Value::Null).await["data"]["role_id"]
        .as_str()
        .unwrap()
        .to_string();
    let secret_id = admin(&http, &addr, &root, Method::POST, "auth/approle/role/merpati/secret-id", json!({})).await["data"]["secret_id"]
        .as_str()
        .unwrap()
        .to_string();

    let token_p = ProviderConfig::new(
        "dev token",
        ProviderSettings::HashicorpVault(VaultSettings { address: addr.clone(), ..Default::default() }),
    );
    let approle_p = ProviderConfig::new(
        "dev approle",
        ProviderSettings::HashicorpVault(VaultSettings {
            address: addr.clone(),
            auth: VaultAuth::AppRole,
            role_id,
            ..Default::default()
        }),
    );
    let kv1_p = ProviderConfig::new(
        "dev kv1",
        ProviderSettings::HashicorpVault(VaultSettings {
            address: addr.clone(),
            mount: "kv1".into(),
            kv_version: KvVersion::Auto,
            ..Default::default()
        }),
    );
    let config =
        SecretsConfig { providers: vec![token_p.clone(), approle_p.clone(), kv1_p.clone()], links: Vec::new() };
    // Its own client: the admin client's pooled connections belong to this
    // test's runtime, which the blocking lookup below holds up.
    let ext = ExternalSecrets::new(Arc::new(SecretStore::memory()), config, default_client().unwrap());
    ext.set_credential(&token_p.id, "token", &root);
    ext.set_credential(&kv1_p.id, "token", &root);
    ext.set_credential(&approle_p.id, "secret_id", &secret_id);

    let summary = ext.test_provider(&token_p).await.unwrap();
    assert!(summary.contains("root") && summary.contains("KV v2"), "{summary}");
    println!("token: {summary}");
    assert_eq!(ext.resolve_reference(&token_p.id, "merpati/db#password").await.unwrap(), "pw-v2");
    assert_eq!(ext.resolve_reference(&token_p.id, "merpati/db?version=1#password").await.unwrap(), "pw-v1");
    let e = ext.resolve_reference(&token_p.id, "merpati/nope#password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e}");
    let entries = ext.browse(&token_p.id, "merpati/").await.unwrap();
    assert!(entries.iter().any(|e| e.name == "db" && e.kind == EntryKind::Secret), "{entries:?}");

    let summary = ext.test_provider(&kv1_p).await.unwrap();
    assert!(summary.contains("KV v1"), "{summary}");
    assert_eq!(ext.resolve_reference(&kv1_p.id, "legacy").await.unwrap(), "v1-value");

    let summary = ext.test_provider(&approle_p).await.unwrap();
    println!("approle: {summary}");
    assert!(summary.contains("merpati-read"), "{summary}");
    ext.link(LinkedSecret::new("db_password", &approle_p.id, "merpati/db#password")).unwrap();
    assert_eq!(ext.resolve("db_password").await.unwrap(), "pw-v2");
    let e = ext.resolve_reference(&approle_p.id, "other/secret#x").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied, "{e}");
    println!("denied: {e}");

    // Synchronous path from a plain thread.
    let e2 = ext.clone();
    ext.clear_cache();
    let v = std::thread::spawn(move || e2.get_blocking("db_password")).join().unwrap();
    assert_eq!(v.as_deref(), Some("pw-v2"));
}
