//! The 1Password CLI found through `PATH`. This is its own test binary
//! because it changes `PATH` for the process (the only test here).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use merpati_core::secrets::SecretStore;
use merpati_core::variables::{Resolver, VariableScopes};
use merpati_secrets::{
    ErrorKind, ExternalSecrets, LinkedSecret, OnePasswordSettings, ProviderConfig, ProviderSettings, SecretsConfig,
    default_client,
};

const FAKE_OP: &str = r#"#!/bin/sh
if [ "$1" = "read" ] && [ "$3" = "op://Dev/API/token" ]; then printf '%s' "from-path-op"; exit 0; fi
echo "[ERROR] 2024/01/01 10:00:00 \"$3\" isn't an item" >&2
exit 1
"#;

#[tokio::test]
async fn op_is_found_on_path() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().join(".op.tmp");
    std::fs::write(&tmp, FAKE_OP).unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, dir.path().join("op")).unwrap();
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(&old));
    // SAFETY: the only test in this binary; nothing else reads the environment concurrently.
    unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };

    let provider = ProviderConfig::new("1Password", ProviderSettings::OnePassword(OnePasswordSettings::default()));
    let config = SecretsConfig { providers: vec![provider.clone()], links: Vec::new() };
    let ext = ExternalSecrets::new(Arc::new(SecretStore::memory()), config, default_client().unwrap());
    ext.link(LinkedSecret::new("api_token", &provider.id, "op://Dev/API/token")).unwrap();
    ext.link(LinkedSecret::new("other", &provider.id, "op://Dev/Other/x")).unwrap();

    assert_eq!(ext.resolve("api_token").await.unwrap(), "from-path-op");
    let e = ext.resolve("other").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);

    // `{{vault:name}}` from a non-runtime thread, as a script thread would.
    ext.clear_cache();
    let e2 = ext.clone();
    let out = std::thread::spawn(move || {
        let scopes = VariableScopes::default();
        Resolver::new(&scopes).with_vault(&e2).resolve("Bearer {{vault:api_token}}")
    })
    .join()
    .unwrap();
    assert_eq!(out, "Bearer from-path-op");
}
