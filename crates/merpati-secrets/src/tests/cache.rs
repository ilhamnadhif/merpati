//! Cache/TTL, precedence, the synchronous lookup from different threads and
//! `{{vault:name}}` resolution through `VaultLookup`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use merpati_core::secrets::SecretStore;
use merpati_core::variables::{Resolver, VariableScopes, VaultLookup};

use super::vault::{VaultMock, start, token_settings};
use super::{MockServer, client};
use crate::{
    ErrorKind, ExternalSecrets, LinkStatus, LinkedSecret, ProviderConfig, ProviderSettings, SecretsConfig,
    vault_references,
};

fn setup() -> (MockServer, Arc<VaultMock>, ExternalSecrets) {
    let (server, st) = start();
    let ext = super::manager(ProviderSettings::HashicorpVault(token_settings(&server.url)));
    ext.set_credential("p1", "token", "root-token");
    (server, st, ext)
}

fn wait_until(mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn ttl_cache_and_expiry() {
    let (_server, st, ext) = setup();
    ext.link(LinkedSecret::new("db", "p1", "app/db#password").with_ttl(1)).unwrap();
    assert_eq!(ext.status("db"), LinkStatus::NotFetched);
    assert_eq!(ext.get_blocking("db").as_deref(), Some("s3cr3t"));
    assert_eq!(ext.get_blocking("db").as_deref(), Some("s3cr3t"));
    assert_eq!(VaultLookup::get(&ext, " db ").as_deref(), Some("s3cr3t"));
    assert_eq!(st.reads.load(Ordering::SeqCst), 1);
    assert!(matches!(ext.status("db"), LinkStatus::Cached { expires_in, .. } if expires_in <= Duration::from_secs(1)));
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(ext.status("db"), LinkStatus::NotFetched);
    assert_eq!(ext.get_blocking("db").as_deref(), Some("s3cr3t"));
    assert_eq!(st.reads.load(Ordering::SeqCst), 2);

    // Two names on the same reference share one cache entry.
    ext.link(LinkedSecret::new("db2", "p1", "app/db#password").with_ttl(1)).unwrap();
    assert_eq!(ext.get_blocking("db2").as_deref(), Some("s3cr3t"));
    assert_eq!(st.reads.load(Ordering::SeqCst), 2);

    // TTL 0: fetched on every use.
    ext.link(LinkedSecret::new("live", "p1", "app/db#username").with_ttl(0)).unwrap();
    ext.get_blocking("live");
    ext.get_blocking("live");
    assert_eq!(st.reads.load(Ordering::SeqCst), 4);

    ext.clear_cache();
    assert_eq!(ext.get_blocking("db").as_deref(), Some("s3cr3t"));
    assert_eq!(st.reads.load(Ordering::SeqCst), 5);
}

#[test]
fn each_link_honors_its_own_ttl() {
    let (_server, st, ext) = setup();
    // Same reference, different lifetimes: they share the cache entry, but
    // TTL 0 still means "fetch on every use".
    ext.link(LinkedSecret::new("cached", "p1", "app/db#password").with_ttl(300)).unwrap();
    ext.link(LinkedSecret::new("live", "p1", "app/db#password").with_ttl(0)).unwrap();
    assert_eq!(ext.get_blocking("cached").as_deref(), Some("s3cr3t"));
    assert_eq!(ext.get_blocking("live").as_deref(), Some("s3cr3t"));
    assert_eq!(st.reads.load(Ordering::SeqCst), 2);
    assert_eq!(ext.get_blocking("cached").as_deref(), Some("s3cr3t"));
    assert_eq!(st.reads.load(Ordering::SeqCst), 2);

    // Lowering the TTL of a link applies to the value already cached.
    ext.link(LinkedSecret::new("cached", "p1", "app/db#password").with_ttl(1)).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(ext.status("cached"), LinkStatus::NotFetched);
    assert_eq!(ext.get_blocking("cached").as_deref(), Some("s3cr3t"));
    assert_eq!(st.reads.load(Ordering::SeqCst), 3);
}

#[test]
fn link_shadows_local_value_and_unlink_restores_it() {
    let (_server, _st, ext) = setup();
    ext.store().vault_set("db", "local-value");
    ext.store().vault_set("only_local", "L");
    assert_eq!(ext.get_blocking("db").as_deref(), Some("local-value"));
    assert_eq!(ext.status("db"), LinkStatus::NotLinked);
    ext.link(LinkedSecret::new("db", "p1", "app/db#password")).unwrap();
    ext.link(LinkedSecret::new("remote_only", "p1", "app/db#username")).unwrap();
    assert_eq!(ext.get_blocking("db").as_deref(), Some("s3cr3t"));
    assert_eq!(ext.get_blocking("only_local").as_deref(), Some("L"));
    assert_eq!(ext.all_names(), ["db", "only_local", "remote_only"]);
    assert_eq!(ext.linked_names(), ["db", "remote_only"]);

    // A failing link does not fall back to the shadowed local value.
    ext.link(LinkedSecret::new("db", "p1", "app/missing#x")).unwrap();
    assert_eq!(ext.get_blocking("db"), None);
    assert_eq!(ext.last_error("db").unwrap().kind, ErrorKind::NotFound);

    assert!(ext.unlink("db").unwrap());
    assert!(!ext.unlink("db").unwrap());
    assert_eq!(ext.get_blocking("db").as_deref(), Some("local-value"));
}

#[test]
fn resolver_uses_linked_values() {
    let (_server, _st, ext) = setup();
    ext.store().vault_set("local", "L1");
    ext.link(LinkedSecret::new("db", "p1", "app/db#password")).unwrap();
    ext.link(LinkedSecret::new("broken", "p1", "app/missing#x")).unwrap();
    let mut scopes = VariableScopes::default();
    scopes.environment.insert("auth".into(), "Bearer {{vault:db}}".into());
    let resolver = Resolver::new(&scopes).with_vault(&ext);
    assert_eq!(resolver.resolve("{{auth}}"), "Bearer s3cr3t");
    assert_eq!(resolver.resolve("{{vault:db}}:{{vault:local}}"), "s3cr3t:L1");
    let mut unresolved = Vec::new();
    assert_eq!(resolver.resolve_tracking("x={{vault:broken}}", &mut unresolved), "x={{vault:broken}}");
    assert_eq!(unresolved, ["vault:broken"]);

    // Through an `Arc<dyn VaultLookup>`, as the pipeline holds it.
    let lookup: Arc<dyn VaultLookup> = Arc::new(ext.clone());
    assert_eq!(Resolver::new(&scopes).with_vault(lookup.as_ref()).resolve("{{vault:db}}"), "s3cr3t");
}

#[test]
fn error_backoff_and_refresh() {
    let (_server, st, ext) = setup();
    ext.link(LinkedSecret::new("gone", "p1", "app/missing#x")).unwrap();
    assert_eq!(ext.get_blocking("gone"), None);
    let after_first = st.requests.load(Ordering::SeqCst);
    assert_eq!(ext.get_blocking("gone"), None);
    assert_eq!(ext.get_cached("gone"), None);
    assert_eq!(st.requests.load(Ordering::SeqCst), after_first, "failure is remembered");
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let errors = rt.block_on(ext.prefetch(&["gone"]));
    assert_eq!(errors.len(), 1);
    assert_eq!(st.requests.load(Ordering::SeqCst), after_first, "prefetch honors the backoff");
    let errors = rt.block_on(ext.refresh(&["gone"]));
    assert_eq!(errors[0].1.kind, ErrorKind::NotFound);
    assert!(st.requests.load(Ordering::SeqCst) > after_first, "refresh retries");

    ext.set_error_backoff(Duration::ZERO);
    let n = st.requests.load(Ordering::SeqCst);
    assert_eq!(ext.get_blocking("gone"), None);
    assert!(st.requests.load(Ordering::SeqCst) > n);
}

#[tokio::test]
async fn prefetch_then_cached_lookup() {
    let (_server, st, ext) = setup();
    ext.store().vault_set("local", "L");
    ext.link(LinkedSecret::new("db", "p1", "app/db#password")).unwrap();
    ext.link(LinkedSecret::new("user", "p1", "app/db#username")).unwrap();
    ext.link(LinkedSecret::new("gone", "p1", "app/missing#x")).unwrap();
    let errors = ext.prefetch(&["db", "db", "local", "gone", "unknown"]).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, "gone");
    assert_eq!(st.reads.load(Ordering::SeqCst), 1);
    let cached = ext.cached_lookup();
    assert_eq!(cached.get("db").as_deref(), Some("s3cr3t"));
    assert_eq!(cached.get("local").as_deref(), Some("L"));
    // Not fetched yet: returns at once and fetches in the background.
    assert_eq!(cached.get("user"), None);
    assert!(wait_until(|| cached.get("user").is_some()));
    assert_eq!(st.reads.load(Ordering::SeqCst), 2);

    let errors = ext.prefetch_referenced(["GET {{base}}/x?k={{vault:db}}", "{{vault:user}} {{vault:gone}}"]).await;
    assert_eq!(errors.len(), 1, "cached names are skipped, the failing one reported");
    assert_eq!(st.reads.load(Ordering::SeqCst), 2);
    assert_eq!(ext.resolve("db").await.unwrap(), "s3cr3t");
    assert_eq!(ext.resolve("local").await.unwrap(), "L");
    assert_eq!(ext.resolve("nothing").await.unwrap_err().kind, ErrorKind::NotLinked);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_lookup_on_multi_thread_worker() {
    let (_server, _st, ext) = setup();
    ext.link(LinkedSecret::new("db", "p1", "app/db#password")).unwrap();
    // Directly on a worker (block_in_place) and from a spawned task.
    assert_eq!(ext.get_blocking("db").as_deref(), Some("s3cr3t"));
    ext.clear_cache();
    let e2 = ext.clone();
    let v = tokio::spawn(async move { e2.get_blocking("db") }).await.unwrap();
    assert_eq!(v.as_deref(), Some("s3cr3t"));
    ext.clear_cache();
    let e3 = ext.clone();
    let v = tokio::task::spawn_blocking(move || e3.get_blocking("db")).await.unwrap();
    assert_eq!(v.as_deref(), Some("s3cr3t"));
}

#[tokio::test]
async fn sync_lookup_on_current_thread_runtime() {
    let (_server, _st, ext) = setup();
    ext.link(LinkedSecret::new("db", "p1", "app/db#password")).unwrap();
    assert_eq!(ext.get_blocking("db").as_deref(), Some("s3cr3t"));
}

#[test]
fn sync_lookup_from_script_threads_single_flight() {
    let (_server, st, ext) = setup();
    st.delay_ms.store(200, Ordering::SeqCst);
    ext.link(LinkedSecret::new("db", "p1", "app/db#password")).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let e = ext.clone();
            std::thread::spawn(move || e.get_blocking("db"))
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap().as_deref(), Some("s3cr3t"));
    }
    assert_eq!(st.reads.load(Ordering::SeqCst), 1);
}

#[test]
fn sync_timeout_then_value_arrives() {
    let (_server, st, ext) = setup();
    ext.link(LinkedSecret::new("db", "p1", "app/db#password")).unwrap();
    // Warm the token/KV probe so only the read is slow.
    ext.link(LinkedSecret::new("warm", "p1", "app/single")).unwrap();
    assert!(ext.get_blocking("warm").is_some());
    st.delay_ms.store(600, Ordering::SeqCst);
    ext.set_sync_timeout(Duration::from_millis(100));
    let start = Instant::now();
    assert_eq!(ext.get_blocking("db"), None);
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(matches!(ext.status("db"), LinkStatus::Fetching | LinkStatus::Failed(_)));
    assert!(wait_until(|| ext.get_cached("db").is_some()), "the late value is cached");
    assert!(matches!(ext.status("db"), LinkStatus::Cached { .. }));
}

#[test]
fn credential_change_invalidates() {
    let (_server, _st, ext) = setup();
    ext.link(LinkedSecret::new("db", "p1", "app/db#password")).unwrap();
    assert!(ext.get_blocking("db").is_some());
    assert!(ext.has_credential("p1", "token"));
    ext.set_credential("p1", "token", "limited-token");
    assert_eq!(ext.get_blocking("db"), None);
    assert_eq!(ext.last_error("db").unwrap().kind, ErrorKind::PermissionDenied);
    ext.set_credential("p1", "token", "");
    assert!(!ext.has_credential("p1", "token"));
    assert_eq!(ext.get_blocking("db"), None);
    assert_eq!(ext.last_error("db").unwrap().kind, ErrorKind::Auth);
}

#[test]
fn provider_lifecycle_and_persistence() {
    let (server, _st) = start();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SecretStore::memory());
    let ext = ExternalSecrets::open(dir.path(), store.clone(), client()).unwrap();
    let p = ProviderConfig::new("Vault", ProviderSettings::HashicorpVault(token_settings(&server.url)));
    ext.upsert_provider(p.clone()).unwrap();
    ext.set_credential(&p.id, "token", "root-token");
    ext.link(LinkedSecret::new("db", &p.id, "app/db#password")).unwrap();
    assert!(ext.link(LinkedSecret::new("x", "nope", "a/b")).is_err());
    assert!(ext.link(LinkedSecret::new("{bad}", &p.id, "a/b")).is_err());
    assert_eq!(ext.get_blocking("db").as_deref(), Some("s3cr3t"));
    assert_eq!(ext.credential_keys(), [format!("extsecrets:{}:token", p.id)]);

    let file = std::fs::read_to_string(dir.path().join(crate::CONFIG_FILE)).unwrap();
    assert!(file.contains("app/db#password") && !file.contains("root-token") && !file.contains("s3cr3t"));
    let reopened = ExternalSecrets::open(dir.path(), store.clone(), client()).unwrap();
    assert_eq!(reopened.config(), ext.config());

    // Changing settings drops the cached value.
    let mut changed = p.clone();
    if let ProviderSettings::HashicorpVault(s) = &mut changed.settings {
        s.address = "http://127.0.0.1:9".into();
    }
    ext.upsert_provider(changed).unwrap();
    assert_eq!(ext.get_blocking("db"), None);
    assert_eq!(ext.last_error("db").unwrap().kind, ErrorKind::Network);

    let removed = ext.remove_provider(&p.id).unwrap();
    assert_eq!(removed, ["db"]);
    assert!(ext.credential_keys().is_empty());
    assert!(!ext.is_linked("db"));
    assert_eq!(SecretsConfig::load(dir.path()).unwrap(), SecretsConfig::default());
}

#[test]
fn debug_output_and_references() {
    let (_server, _st, ext) = setup();
    ext.link(LinkedSecret::new("db", "p1", "app/db#password")).unwrap();
    assert!(ext.get_blocking("db").is_some());
    let dbg = format!("{ext:?} {:?}", ext.cached_lookup());
    assert!(!dbg.contains("s3cr3t") && !dbg.contains("root-token"), "{dbg}");
    assert!(dbg.contains("db"));
    assert_eq!(vault_references("{{vault:a}} {{ vault:b }} {{c}} {{vault:a}} {{vault: }}"), ["a", "b"]);
}
