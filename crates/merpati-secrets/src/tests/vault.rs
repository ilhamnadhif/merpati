//! HashiCorp Vault against a mock: token and AppRole auth, KV v1/v2,
//! auto-detection, listing, errors.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde_json::json;

use super::{Req, json, manager, serve};
use crate::{
    EntryKind, ErrorKind, KvVersion, LinkedSecret, ProviderConfig, ProviderSettings, VaultAuth, VaultSettings,
};

#[derive(Default)]
pub(crate) struct VaultMock {
    pub requests: AtomicUsize,
    pub reads: AtomicUsize,
    pub logins: AtomicUsize,
    pub probes: AtomicUsize,
    /// AppRole tokens the server no longer accepts.
    pub revoked: Mutex<Vec<String>>,
    pub namespaces: Mutex<Vec<String>>,
    pub delay_ms: AtomicUsize,
}

fn token_ok(st: &VaultMock, t: &str) -> bool {
    (t == "root-token" || t == "limited-token" || t.starts_with("approle-token-"))
        && !st.revoked.lock().iter().any(|r| r == t)
}

pub(crate) async fn vault_handler(st: Arc<VaultMock>, r: Req) -> axum::response::Response {
    st.requests.fetch_add(1, Ordering::SeqCst);
    if let Some(ns) = r.header("x-vault-namespace") {
        st.namespaces.lock().push(ns.to_string());
    }
    let delay = st.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
    }
    let path = r.path.trim_start_matches("/v1/").to_string();
    if path == "auth/approle/login" {
        let n = st.logins.fetch_add(1, Ordering::SeqCst) + 1;
        let b = r.json();
        if b["role_id"] == "role-1" && b["secret_id"] == "sid-1" {
            return json(200, json!({"auth": {"client_token": format!("approle-token-{n}"), "lease_duration": 3600}}));
        }
        return json(400, json!({"errors": ["invalid role or secret ID"]}));
    }
    let token = r.header("x-vault-token").unwrap_or_default().to_string();
    if !token_ok(&st, &token) {
        return json(403, json!({"errors": ["permission denied"]}));
    }
    let list = r.param("list") == Some("true");
    match path.as_str() {
        "auth/token/lookup-self" => {
            json(200, json!({"data": {"display_name": "token-dev", "policies": ["default", "app"], "ttl": 0}}))
        }
        "sys/internal/ui/mounts" => json(
            200,
            json!({"data": {"secret": {
                "secret/": {"type": "kv", "options": {"version": "2"}},
                "kv1/": {"type": "kv", "options": null},
                "sys/": {"type": "system"}
            }}}),
        ),
        p if p.starts_with("sys/internal/ui/mounts/") => {
            st.probes.fetch_add(1, Ordering::SeqCst);
            match p.trim_start_matches("sys/internal/ui/mounts/") {
                "secret" => json(200, json!({"data": {"type": "kv", "path": "secret/", "options": {"version": "2"}}})),
                "kv1" => json(200, json!({"data": {"type": "kv", "path": "kv1/", "options": null}})),
                _ => json(400, json!({"errors": ["no mount"]})),
            }
        }
        _ if token == "limited-token" => {
            json(403, json!({"errors": [format!("1 error occurred: permission denied for token {token}")]}))
        }
        "secret/metadata" if list => json(200, json!({"data": {"keys": ["app/"]}})),
        "secret/metadata/app" if list => json(200, json!({"data": {"keys": ["db", "empty/"]}})),
        "secret/metadata/app/empty" if list => json(404, json!({"errors": []})),
        "secret/data/app/db" => {
            st.reads.fetch_add(1, Ordering::SeqCst);
            let data = match r.param("version") {
                Some("1") => json!({"username": "admin", "password": "old-pw"}),
                _ => json!({"username": "admin", "password": "s3cr3t", "port": 5432}),
            };
            json(200, json!({"data": {"data": data, "metadata": {"version": 2}}}))
        }
        "secret/data/app/single" => {
            st.reads.fetch_add(1, Ordering::SeqCst);
            json(200, json!({"data": {"data": {"api_key": "only-value"}}}))
        }
        "kv1" if list => json(200, json!({"data": {"keys": ["legacy"]}})),
        "kv1/legacy" => {
            st.reads.fetch_add(1, Ordering::SeqCst);
            json(200, json!({"data": {"value": "v1-value", "other": "x"}}))
        }
        _ => json(404, json!({"errors": []})),
    }
}

pub(crate) fn start() -> (super::MockServer, Arc<VaultMock>) {
    let st = Arc::new(VaultMock::default());
    let s2 = st.clone();
    let server = serve(move |r| vault_handler(s2.clone(), r));
    (server, st)
}

pub(crate) fn token_settings(url: &str) -> VaultSettings {
    VaultSettings { address: url.into(), ..Default::default() }
}

#[tokio::test]
async fn token_auth_kv2_fields_versions_and_test() {
    let (server, st) = start();
    let mut s = token_settings(&server.url);
    s.namespace = "team-a".into();
    let ext = manager(ProviderSettings::HashicorpVault(s.clone()));
    // No token yet: actionable auth error.
    let e = ext.resolve_reference("p1", "app/db#password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("Vault token"), "{e}");

    ext.set_credential("p1", "token", "root-token");
    assert_eq!(ext.resolve_reference("p1", "app/db#password").await.unwrap(), "s3cr3t");
    assert_eq!(ext.resolve_reference("p1", "app/db#port").await.unwrap(), "5432");
    assert_eq!(ext.resolve_reference("p1", "app/db?version=1#password").await.unwrap(), "old-pw");
    assert_eq!(ext.resolve_reference("p1", "app/single").await.unwrap(), "only-value");
    // Auto-detected once per mount.
    assert_eq!(st.probes.load(Ordering::SeqCst), 1);
    assert!(st.namespaces.lock().iter().all(|n| n == "team-a") && !st.namespaces.lock().is_empty());

    let e = ext.resolve_reference("p1", "app/db").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidReference);
    assert!(e.message.contains("password, port, username") && !e.message.contains("s3cr3t"), "{e}");
    let e = ext.resolve_reference("p1", "app/db#nope").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    let e = ext.resolve_reference("p1", "app/missing#x").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound);
    assert!(e.message.contains("secret/app/missing"), "{e}");
    let e = ext.resolve_reference("p1", "app/db?bogus=1#x").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidReference);

    let summary = ext.test_provider(&ext.provider("p1").unwrap()).await.unwrap();
    assert!(
        summary.contains("token-dev") && summary.contains("default, app") && summary.contains("KV v2"),
        "{summary}"
    );

    ext.set_credential("p1", "token", "wrong-token");
    let e = ext.test_provider(&ext.provider("p1").unwrap()).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    let e = ext.resolve_reference("p1", "app/db#password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    assert!(!e.message.contains("wrong-token"));
}

#[tokio::test]
async fn kv1_explicit_and_auto() {
    let (server, st) = start();
    for version in [KvVersion::Auto, KvVersion::V1] {
        let s = VaultSettings { mount: "kv1".into(), kv_version: version, ..token_settings(&server.url) };
        let ext = manager(ProviderSettings::HashicorpVault(s));
        ext.set_credential("p1", "token", "root-token");
        assert_eq!(ext.resolve_reference("p1", "legacy#value").await.unwrap(), "v1-value");
        // `value` is the default field when there are several.
        assert_eq!(ext.resolve_reference("p1", "legacy").await.unwrap(), "v1-value");
        let e = ext.resolve_reference("p1", "legacy?version=2").await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidReference);
        let entries = ext.browse("p1", "").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!((entries[0].name.as_str(), entries[0].kind), ("legacy", EntryKind::Secret));
    }
    assert_eq!(st.probes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn no_mount_uses_first_segment_and_browse() {
    let (server, _st) = start();
    let s = VaultSettings { mount: String::new(), ..token_settings(&server.url) };
    let ext = manager(ProviderSettings::HashicorpVault(s));
    ext.set_credential("p1", "token", "root-token");
    assert_eq!(ext.resolve_reference("p1", "secret/app/db#username").await.unwrap(), "admin");
    assert_eq!(ext.resolve_reference("p1", "kv1/legacy#other").await.unwrap(), "x");

    // Mounts -> folders -> secrets -> fields.
    let mounts = ext.browse("p1", "").await.unwrap();
    let names: Vec<&str> = mounts.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["kv1/", "secret/"]);
    let top = ext.browse("p1", "secret/").await.unwrap();
    assert_eq!(top[0].path.as_deref(), Some("secret/app/"));
    let app = ext.browse("p1", "secret/app/").await.unwrap();
    assert_eq!(app[0].kind, EntryKind::Folder);
    assert_eq!(app[1].reference.as_deref(), Some("secret/app/db"));
    let fields = ext.browse("p1", "secret/app/db").await.unwrap();
    let refs: Vec<&str> = fields.iter().filter_map(|e| e.reference.as_deref()).collect();
    assert_eq!(refs, ["secret/app/db#password", "secret/app/db#port", "secret/app/db#username"]);
    assert!(ext.browse("p1", "secret/app/empty/").await.unwrap().is_empty());

    // References without a mount are rejected before linking.
    let e = ext.link(LinkedSecret::new("x", "p1", "justname")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidReference);
}

#[tokio::test]
async fn approle_login_cache_and_relogin() {
    let (server, st) = start();
    let s = VaultSettings { auth: VaultAuth::AppRole, role_id: "role-1".into(), ..token_settings(&server.url) };
    let ext = manager(ProviderSettings::HashicorpVault(s));
    ext.set_credential("p1", "secret_id", "bad-sid");
    let e = ext.resolve_reference("p1", "app/db#password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.message.contains("AppRole") && !e.message.contains("bad-sid"), "{e}");

    ext.set_credential("p1", "secret_id", "sid-1");
    let logins = st.logins.load(Ordering::SeqCst);
    assert_eq!(ext.resolve_reference("p1", "app/db#password").await.unwrap(), "s3cr3t");
    assert_eq!(ext.resolve_reference("p1", "app/db#username").await.unwrap(), "admin");
    // One login, token reused.
    assert_eq!(st.logins.load(Ordering::SeqCst), logins + 1);

    // The server revokes the token: one transparent re-login.
    st.revoked.lock().push(format!("approle-token-{}", logins + 1));
    assert_eq!(ext.resolve_reference("p1", "app/db#password").await.unwrap(), "s3cr3t");
    assert_eq!(st.logins.load(Ordering::SeqCst), logins + 2);

    let summary = ext.test_provider(&ext.provider("p1").unwrap()).await.unwrap();
    assert!(summary.contains("Connected"), "{summary}");
}

#[tokio::test]
async fn permission_denied_scrubs_token() {
    let (server, _st) = start();
    let ext = manager(ProviderSettings::HashicorpVault(token_settings(&server.url)));
    ext.set_credential("p1", "token", "limited-token");
    let e = ext.resolve_reference("p1", "app/db#password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
    assert!(e.message.contains("***") && !e.message.contains("limited-token"), "{e}");
    assert!(e.message.contains("policy"), "{e}");
}

#[tokio::test]
async fn unreachable_and_bad_config() {
    let ext = manager(ProviderSettings::HashicorpVault(token_settings("http://127.0.0.1:9")));
    ext.set_credential("p1", "token", "root-token");
    let e = ext.resolve_reference("p1", "app/db#password").await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Network, "{e}");
    assert!(e.message.contains("127.0.0.1:9"), "{e}");

    let bad = ProviderConfig {
        id: "p1".into(),
        name: "x".into(),
        settings: ProviderSettings::HashicorpVault(VaultSettings {
            address: "vault:8200".into(),
            ..Default::default()
        }),
    };
    assert_eq!(ext.test_provider(&bad).await.unwrap_err().kind, ErrorKind::Config);
}

#[tokio::test]
async fn client_for_uses_custom_proxy_and_checks_ca() {
    use merpati_core::settings::{CertificateSettings, ProxySettings};
    let (server, st) = start();
    // The mock answers proxied (absolute-form) requests too.
    let proxy = ProxySettings { custom_enabled: true, custom_url: server.url.clone(), ..Default::default() };
    let http = crate::client_for(&proxy, &CertificateSettings::default()).unwrap();
    let config = crate::SecretsConfig {
        providers: vec![ProviderConfig {
            id: "p1".into(),
            name: "via proxy".into(),
            settings: ProviderSettings::HashicorpVault(token_settings("http://vault.invalid:8200")),
        }],
        links: Vec::new(),
    };
    let ext = crate::ExternalSecrets::new(Arc::new(merpati_core::secrets::SecretStore::memory()), config, http);
    ext.set_credential("p1", "token", "root-token");
    assert_eq!(ext.resolve_reference("p1", "app/db#password").await.unwrap(), "s3cr3t");
    assert!(st.requests.load(Ordering::SeqCst) >= 1);

    let certs = CertificateSettings { ca_enabled: true, ca_path: "/nonexistent/ca.pem".into(), ..Default::default() };
    let e = crate::client_for(&ProxySettings::default(), &certs).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config);
    assert!(e.message.contains("/nonexistent/ca.pem"), "{e}");
}
