//! [`ExternalSecrets`]: links, the value cache and the lookups used by
//! `{{vault:name}}`.
//!
//! All provider I/O runs on one small process-wide runtime owned by this
//! crate, so the async API can be awaited from any runtime and the
//! synchronous [`VaultLookup`] path can wait for a fetch without ever
//! needing the caller's runtime to make progress.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use tokio::runtime::{Runtime, RuntimeFlavor};

use merpati_core::secrets::SecretStore;
use merpati_core::settings::{CertificateSettings, ProxySettings};
use merpati_core::variables::{VAULT_PREFIX, VaultLookup, references};

use crate::env::Env;
use crate::error::{ErrorKind, SecretError};
use crate::model::{CREDENTIAL_KEY_PREFIX, LinkedSecret, ProviderConfig, SecretsConfig, credential_key};
use crate::providers::{self, BrowseEntry};

/// How long a failed fetch is not retried by lookups and `prefetch`
/// (explicit `refresh` always retries).
pub const DEFAULT_ERROR_BACKOFF: Duration = Duration::from_secs(15);

/// How long the synchronous lookup waits for a fetch.
pub const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(15);

fn runtime() -> Result<&'static Runtime, SecretError> {
    static RT: OnceLock<Option<Runtime>> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("merpati-secrets")
            .enable_all()
            .build()
            .map_err(|e| tracing::error!("could not start the secret manager runtime: {e}"))
            .ok()
    })
    .as_ref()
    .ok_or_else(|| SecretError::provider("Could not start the secret manager worker threads."))
}

/// Runs a provider future on the crate runtime and awaits it from anywhere.
async fn spawn<T, F>(fut: F) -> Result<T, SecretError>
where
    F: std::future::Future<Output = Result<T, SecretError>> + Send + 'static,
    T: Send + 'static,
{
    runtime()?.spawn(fut).await.map_err(|_| SecretError::provider("The secret manager task failed unexpectedly."))?
}

/// A client for provider calls with the app's proxy and extra CA settings
/// (rustls with ring, system roots, no redirects; client certificates are
/// not used).
///
/// Give [`ExternalSecrets`] a client of its own: a pooled connection is
/// driven by the runtime that opened it, so sharing a pool with code on
/// another runtime can make a synchronous lookup wait for that runtime.
pub fn client_for(proxy: &ProxySettings, certs: &CertificateSettings) -> Result<reqwest::Client, SecretError> {
    merpati_http::tls::install_provider();
    let b = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    // Same proxy rules as the HTTP engine; errors never quote the proxy URL.
    let mut b = merpati_http::proxy::apply_to_reqwest(b, proxy)
        .map_err(|e| SecretError::config(format!("{} (proxy settings)", e.message)))?;
    if certs.ca_enabled && !certs.ca_path.trim().is_empty() {
        let path = certs.ca_path.trim();
        let pem = std::fs::read(path)
            .map_err(|e| SecretError::config(format!("Could not read CA certificate file {path}: {e}")))?;
        let roots = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| SecretError::config(format!("Invalid CA certificate file {path}: {e}")))?;
        b = b.tls_certs_merge(roots);
    }
    b.build().map_err(|e| SecretError::config(format!("Could not create the HTTP client: {e}")))
}

/// [`client_for`] with the system proxy and no extra CA.
pub fn default_client() -> Result<reqwest::Client, SecretError> {
    client_for(&ProxySettings { use_system: true, ..Default::default() }, &CertificateSettings::default())
}

/// `{{vault:name}}` names referenced in a text, without duplicates.
pub fn vault_references(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in references(text) {
        if let Some(name) = r.strip_prefix(VAULT_PREFIX) {
            let name = name.trim();
            if !name.is_empty() && !out.iter().any(|n| n == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// What the UI shows next to a vault name.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkStatus {
    /// A local vault value (or nothing); no provider involved.
    NotLinked,
    /// Linked, never fetched or expired.
    NotFetched,
    Fetching,
    Cached {
        age: Duration,
        expires_in: Duration,
    },
    Failed(SecretError),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    provider: String,
    reference: String,
}

impl Key {
    fn of(link: &LinkedSecret) -> Key {
        Key { provider: link.provider_id.clone(), reference: link.reference.trim().to_string() }
    }
}

/// How old a cached value may be for `link`. Links to the same reference
/// share one cache entry, so the entry's own TTL (of the link that fetched
/// it) is capped by this: TTL 0 still fetches on every use, and a lowered
/// TTL applies to the value already cached.
fn max_age(link: &LinkedSecret) -> Duration {
    Duration::from_secs(link.ttl_secs)
}

struct Cached {
    value: String,
    fetched: Instant,
    ttl: Duration,
}

struct Failure {
    error: SecretError,
    at: Instant,
}

struct Inner {
    env: Env,
    config: RwLock<SecretsConfig>,
    /// Workspace local folder; mutations are saved there when set.
    config_dir: Option<PathBuf>,
    cache: Mutex<HashMap<Key, Cached>>,
    errors: Mutex<HashMap<Key, Failure>>,
    flights: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>,
    /// Bumped by invalidations so fetches started before do not cache.
    generation: AtomicU64,
    sync_timeout: RwLock<Duration>,
    error_backoff: RwLock<Duration>,
}

impl Inner {
    fn cached(&self, key: &Key, max_age: Duration) -> Option<String> {
        let mut cache = self.cache.lock();
        match cache.get(key) {
            Some(c) if c.fetched.elapsed() < c.ttl.min(max_age) => Some(c.value.clone()),
            Some(c) if c.fetched.elapsed() >= c.ttl => {
                cache.remove(key);
                None
            }
            _ => None,
        }
    }

    fn recent_error(&self, key: &Key) -> Option<SecretError> {
        let backoff = *self.error_backoff.read();
        self.errors.lock().get(key).filter(|f| f.at.elapsed() < backoff).map(|f| f.error.clone())
    }

    fn in_flight(&self, key: &Key) -> bool {
        self.flights.lock().get(key).is_some_and(|m| m.try_lock().is_err())
    }

    fn record_error(&self, key: &Key, error: SecretError) {
        self.errors.lock().insert(key.clone(), Failure { error, at: Instant::now() });
    }

    async fn fetch(self: Arc<Self>, link: LinkedSecret, force: bool) -> Result<String, SecretError> {
        let key = Key::of(&link);
        if !force && let Some(v) = self.cached(&key, max_age(&link)) {
            return Ok(v);
        }
        let flight = self.flights.lock().entry(key.clone()).or_default().clone();
        let guard = flight.lock().await;
        if !force {
            if let Some(v) = self.cached(&key, max_age(&link)) {
                return Ok(v);
            }
            if let Some(e) = self.recent_error(&key) {
                return Err(e);
            }
        }
        let generation = self.generation.load(Ordering::SeqCst);
        let provider = self.config.read().provider(&link.provider_id).cloned();
        let result = match provider {
            Some(p) => providers::fetch(&self.env, &p, &link.reference).await,
            None => Err(SecretError::new(
                ErrorKind::NotLinked,
                format!("Vault secret \"{}\" is linked to a provider that no longer exists; link it again.", link.name),
            )),
        };
        if self.generation.load(Ordering::SeqCst) == generation {
            match &result {
                Ok(value) => {
                    self.errors.lock().remove(&key);
                    if link.ttl_secs > 0 {
                        let ttl = Duration::from_secs(link.ttl_secs);
                        self.cache
                            .lock()
                            .insert(key.clone(), Cached { value: value.clone(), fetched: Instant::now(), ttl });
                    }
                    tracing::debug!(name = %link.name, provider = %link.provider_id, "fetched linked vault secret");
                }
                Err(e) => {
                    tracing::debug!(name = %link.name, provider = %link.provider_id, kind = ?e.kind, "linked vault secret failed");
                    self.record_error(&key, e.clone());
                }
            }
        }
        drop(guard);
        let mut flights = self.flights.lock();
        if Arc::strong_count(&flight) <= 2 {
            flights.remove(&key);
        }
        result
    }
}

/// External secret managers for one workspace.
///
/// **Precedence:** a vault name that is linked always resolves through its
/// provider; a local vault value with the same name is shadowed (kept, but
/// not used) until the link is removed. When the provider fails, the
/// reference stays unresolved (`{{vault:name}}` is left as-is and reported
/// as an unresolved variable) instead of silently falling back to a
/// possibly stale local value. Names that are not linked resolve from the
/// local vault exactly as before.
#[derive(Clone)]
pub struct ExternalSecrets {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ExternalSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cfg = self.inner.config.read();
        f.debug_struct("ExternalSecrets")
            .field("providers", &cfg.providers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>())
            .field("links", &cfg.links.iter().map(|l| l.name.as_str()).collect::<Vec<_>>())
            .field("cached", &self.inner.cache.lock().len())
            .finish()
    }
}

impl ExternalSecrets {
    /// Credentials are read from `store`. `http` should carry the app's
    /// proxy and CA settings and not be shared with other code: use
    /// [`client_for`]`(&settings.proxy, &settings.certificates)`.
    pub fn new(store: Arc<SecretStore>, config: SecretsConfig, http: reqwest::Client) -> Self {
        ExternalSecrets::build(store, config, http, None)
    }

    /// Loads `<local_dir>/external-secrets.json` and saves every change
    /// made through this value back to it.
    pub fn open(local_dir: &Path, store: Arc<SecretStore>, http: reqwest::Client) -> Result<Self, SecretError> {
        let config = SecretsConfig::load(local_dir)?;
        Ok(ExternalSecrets::build(store, config, http, Some(local_dir.to_path_buf())))
    }

    fn build(store: Arc<SecretStore>, config: SecretsConfig, http: reqwest::Client, dir: Option<PathBuf>) -> Self {
        ExternalSecrets {
            inner: Arc::new(Inner {
                env: Env::new(store, http),
                config: RwLock::new(config),
                config_dir: dir,
                cache: Mutex::new(HashMap::new()),
                errors: Mutex::new(HashMap::new()),
                flights: Mutex::new(HashMap::new()),
                generation: AtomicU64::new(0),
                sync_timeout: RwLock::new(DEFAULT_SYNC_TIMEOUT),
                error_backoff: RwLock::new(DEFAULT_ERROR_BACKOFF),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_process_env(&mut self, env: crate::env::ProcessEnv) {
        Arc::get_mut(&mut self.inner).expect("not shared yet").env.process = env;
    }

    /// Replaces the HTTP client (after proxy or certificate changes).
    pub fn set_http_client(&self, http: reqwest::Client) {
        *self.inner.env.http.write() = http;
    }

    /// How long [`get_blocking`](Self::get_blocking) waits for a fetch.
    pub fn set_sync_timeout(&self, timeout: Duration) {
        *self.inner.sync_timeout.write() = timeout;
    }

    /// How long a failure is remembered before lookups try again.
    pub fn set_error_backoff(&self, backoff: Duration) {
        *self.inner.error_backoff.write() = backoff;
    }

    pub fn store(&self) -> &Arc<SecretStore> {
        &self.inner.env.store
    }

    // --- configuration ---------------------------------------------------

    pub fn config(&self) -> SecretsConfig {
        self.inner.config.read().clone()
    }

    fn save(&self) -> Result<(), SecretError> {
        match &self.inner.config_dir {
            Some(dir) => self.inner.config.read().save(dir),
            None => Ok(()),
        }
    }

    /// Replaces the whole configuration; changed or removed providers lose
    /// their cached tokens and values.
    pub fn set_config(&self, config: SecretsConfig) -> Result<(), SecretError> {
        let old = std::mem::replace(&mut *self.inner.config.write(), config.clone());
        for p in &old.providers {
            if config.provider(&p.id) != Some(p) {
                self.invalidate_provider(&p.id);
            }
        }
        self.save()
    }

    pub fn providers(&self) -> Vec<ProviderConfig> {
        self.inner.config.read().providers.clone()
    }

    pub fn provider(&self, id: &str) -> Option<ProviderConfig> {
        self.inner.config.read().provider(id).cloned()
    }

    /// Adds or replaces (by id) a provider.
    pub fn upsert_provider(&self, provider: ProviderConfig) -> Result<(), SecretError> {
        let changed = {
            let mut cfg = self.inner.config.write();
            match cfg.providers.iter_mut().find(|p| p.id == provider.id) {
                Some(existing) => {
                    let changed = *existing != provider;
                    *existing = provider.clone();
                    changed
                }
                None => {
                    cfg.providers.push(provider.clone());
                    false
                }
            }
        };
        if changed {
            self.invalidate_provider(&provider.id);
        }
        self.save()
    }

    /// Removes a provider, its credentials and its links. Returns the names
    /// that were linked to it (they resolve from the local vault again).
    pub fn remove_provider(&self, id: &str) -> Result<Vec<String>, SecretError> {
        let removed: Vec<String> = {
            let mut cfg = self.inner.config.write();
            cfg.providers.retain(|p| p.id != id);
            let names = cfg.links.iter().filter(|l| l.provider_id == id).map(|l| l.name.clone()).collect();
            cfg.links.retain(|l| l.provider_id != id);
            names
        };
        let prefix = credential_key(id, "");
        self.inner.env.store.update(|m| m.retain(|k, _| !k.starts_with(&prefix)));
        self.invalidate_provider(id);
        self.save()?;
        Ok(removed)
    }

    pub fn links(&self) -> Vec<LinkedSecret> {
        self.inner.config.read().links.clone()
    }

    pub fn linked(&self, name: &str) -> Option<LinkedSecret> {
        self.inner.config.read().link(name.trim()).cloned()
    }

    pub fn is_linked(&self, name: &str) -> bool {
        self.inner.config.read().link(name.trim()).is_some()
    }

    /// Links (or re-links) a vault name. Checks the name, the provider and
    /// the reference syntax; does not contact the provider.
    pub fn link(&self, link: LinkedSecret) -> Result<(), SecretError> {
        let mut link = link;
        link.name = link.name.trim().to_string();
        link.reference = link.reference.trim().to_string();
        if link.name.is_empty() || link.name.contains(['{', '}']) {
            return Err(SecretError::config("A vault name must not be empty or contain braces."));
        }
        {
            let mut cfg = self.inner.config.write();
            let provider = cfg.provider(&link.provider_id).ok_or_else(|| {
                SecretError::new(ErrorKind::NotLinked, "The selected secret manager does not exist any more.")
            })?;
            provider.settings.check_reference(&link.reference)?;
            match cfg.links.iter_mut().find(|l| l.name == link.name) {
                Some(existing) => *existing = link,
                None => cfg.links.push(link),
            }
        }
        self.save()
    }

    /// Removes a link; the local vault value (if any) is used again.
    pub fn unlink(&self, name: &str) -> Result<bool, SecretError> {
        let removed = {
            let mut cfg = self.inner.config.write();
            let before = cfg.links.len();
            cfg.links.retain(|l| l.name != name.trim());
            before != cfg.links.len()
        };
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Linked names, sorted.
    pub fn linked_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.config.read().links.iter().map(|l| l.name.clone()).collect();
        v.sort();
        v
    }

    /// Local and linked vault names, sorted and deduplicated (for
    /// autocomplete and the vault list).
    pub fn all_names(&self) -> Vec<String> {
        let mut v = self.inner.env.store.vault_names();
        v.extend(self.linked_names());
        v.sort();
        v.dedup();
        v
    }

    // --- credentials -----------------------------------------------------

    /// Stores (or, with an empty value, removes) a credential field such as
    /// `token` or `client_secret`, and drops what was cached with the old one.
    pub fn set_credential(&self, provider_id: &str, field: &str, value: &str) {
        let key = credential_key(provider_id, field);
        if value.is_empty() {
            self.inner.env.store.remove(&key);
        } else {
            self.inner.env.store.set(&key, value);
        }
        self.invalidate_provider(provider_id);
    }

    pub fn has_credential(&self, provider_id: &str, field: &str) -> bool {
        self.inner.env.credential(provider_id, field).is_some()
    }

    /// Credential keys of all providers in the secret store.
    pub fn credential_keys(&self) -> Vec<String> {
        self.inner.env.store.keys_with_prefix(CREDENTIAL_KEY_PREFIX)
    }

    /// Drops cached tokens, values and errors of one provider. Call it after
    /// changing its credentials directly in the secret store.
    pub fn invalidate_provider(&self, provider_id: &str) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        self.inner.env.forget(provider_id);
        let prefix = format!("{provider_id}#");
        self.inner.env.tokens.lock().retain(|k, _| !k.starts_with(&prefix));
        self.inner.cache.lock().retain(|k, _| k.provider != provider_id);
        self.inner.errors.lock().retain(|k, _| k.provider != provider_id);
    }

    /// Forgets every cached value, token and error.
    pub fn clear_cache(&self) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        self.inner.cache.lock().clear();
        self.inner.errors.lock().clear();
        self.inner.env.tokens.lock().clear();
        self.inner.env.kv_versions.lock().clear();
    }

    // --- provider actions (async, never cached) -------------------------

    /// "Test connection": authenticates and makes one cheap call. Works for
    /// unsaved settings; credentials are read for `provider.id`.
    pub async fn test_provider(&self, provider: &ProviderConfig) -> Result<String, SecretError> {
        self.invalidate_provider(&provider.id);
        let inner = self.inner.clone();
        let p = provider.clone();
        spawn(async move { providers::test(&inner.env, &p).await }).await
    }

    /// Lists folders, secrets or fields under `path` ("" for the top level;
    /// pass an entry's `path` to go deeper). Never returns secret values.
    pub async fn browse(&self, provider_id: &str, path: &str) -> Result<Vec<BrowseEntry>, SecretError> {
        let p = self.provider_or_err(provider_id)?;
        let inner = self.inner.clone();
        let path = path.to_string();
        spawn(async move { providers::browse(&inner.env, &p, &path).await }).await
    }

    /// Fetches one reference now, bypassing (and not filling) the cache.
    pub async fn resolve_reference(&self, provider_id: &str, reference: &str) -> Result<String, SecretError> {
        let p = self.provider_or_err(provider_id)?;
        let inner = self.inner.clone();
        let reference = reference.to_string();
        spawn(async move { providers::fetch(&inner.env, &p, &reference).await }).await
    }

    fn provider_or_err(&self, id: &str) -> Result<ProviderConfig, SecretError> {
        self.provider(id).ok_or_else(|| SecretError::new(ErrorKind::NotLinked, "Unknown secret manager."))
    }

    // --- vault names -----------------------------------------------------

    /// The value of `{{vault:name}}`, fetching a linked secret if needed.
    pub async fn resolve(&self, name: &str) -> Result<String, SecretError> {
        let name = name.trim();
        match self.linked(name) {
            Some(link) => spawn(self.inner.clone().fetch(link, false)).await,
            None => self.inner.env.store.vault_get(name).ok_or_else(|| {
                SecretError::new(ErrorKind::NotLinked, format!("Vault secret \"{name}\" is not defined."))
            }),
        }
    }

    /// Fetches the linked names that are not cached, concurrently. Call it
    /// before running a request or collection so the synchronous lookup
    /// only hits the cache. Names that are not linked are ignored. Returns
    /// the failures, to show in the console.
    pub async fn prefetch<S: AsRef<str>>(&self, names: &[S]) -> Vec<(String, SecretError)> {
        self.fetch_many(names, false).await
    }

    /// [`prefetch`](Self::prefetch) of every `{{vault:...}}` in the texts.
    pub async fn prefetch_referenced<'a>(
        &self,
        texts: impl IntoIterator<Item = &'a str>,
    ) -> Vec<(String, SecretError)> {
        let mut names: Vec<String> = Vec::new();
        for t in texts {
            for n in vault_references(t) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        self.prefetch(&names).await
    }

    /// Fetches linked names again, ignoring the cache and earlier failures.
    pub async fn refresh<S: AsRef<str>>(&self, names: &[S]) -> Vec<(String, SecretError)> {
        self.fetch_many(names, true).await
    }

    async fn fetch_many<S: AsRef<str>>(&self, names: &[S], force: bool) -> Vec<(String, SecretError)> {
        let mut seen = HashSet::new();
        let mut jobs = Vec::new();
        for n in names {
            let n = n.as_ref().trim();
            if !seen.insert(n.to_string()) {
                continue;
            }
            let Some(link) = self.linked(n) else { continue };
            if !force && self.inner.cached(&Key::of(&link), max_age(&link)).is_some() {
                continue;
            }
            jobs.push((n.to_string(), link));
        }
        if jobs.is_empty() {
            return Vec::new();
        }
        let rt = match runtime() {
            Ok(rt) => rt,
            Err(e) => return jobs.into_iter().map(|(n, _)| (n, e.clone())).collect(),
        };
        let handles: Vec<_> =
            jobs.into_iter().map(|(n, link)| (n, rt.spawn(self.inner.clone().fetch(link, force)))).collect();
        let mut errors = Vec::new();
        for (n, h) in handles {
            match h.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => errors.push((n, e)),
                Err(_) => errors.push((n, SecretError::provider("The secret manager task failed unexpectedly."))),
            }
        }
        errors
    }

    /// Synchronous lookup used by [`VaultLookup`]: local values directly,
    /// linked values from the cache or, when missing, fetched on the crate
    /// runtime while this thread waits up to the sync timeout. On a
    /// multi-threaded tokio worker the wait uses `block_in_place`, so other
    /// tasks keep running; failures are remembered for the error backoff so
    /// a broken provider does not stall every lookup.
    pub fn get_blocking(&self, name: &str) -> Option<String> {
        let name = name.trim();
        let Some(link) = self.linked(name) else {
            return self.inner.env.store.vault_get(name);
        };
        let key = Key::of(&link);
        let max_age = max_age(&link);
        if let Some(v) = self.inner.cached(&key, max_age) {
            return Some(v);
        }
        if self.inner.recent_error(&key).is_some() {
            return None;
        }
        let rt = match runtime() {
            Ok(rt) => rt,
            Err(e) => {
                self.inner.record_error(&key, e);
                return None;
            }
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let inner = self.inner.clone();
        rt.spawn(async move {
            let _ = tx.send(inner.fetch(link, false).await);
        });
        let timeout = *self.inner.sync_timeout.read();
        let wait = move || rx.recv_timeout(timeout);
        let result = match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(wait),
            _ => wait(),
        };
        match result {
            Ok(Ok(v)) => Some(v),
            Ok(Err(_)) => None,
            Err(_) => {
                if self.inner.cached(&key, max_age).is_none() {
                    self.inner.record_error(
                        &key,
                        SecretError::new(
                            ErrorKind::Timeout,
                            format!(
                                "Fetching vault secret \"{name}\" took longer than {} s; it is used once it arrives.",
                                timeout.as_secs_f32()
                            ),
                        ),
                    );
                }
                self.inner.cached(&key, max_age)
            }
        }
    }

    /// Never blocks: local values, or cached linked values. A linked value
    /// that is not cached starts a background fetch and returns `None`.
    pub fn get_cached(&self, name: &str) -> Option<String> {
        let name = name.trim();
        let Some(link) = self.linked(name) else {
            return self.inner.env.store.vault_get(name);
        };
        let key = Key::of(&link);
        if let Some(v) = self.inner.cached(&key, max_age(&link)) {
            return Some(v);
        }
        if self.inner.recent_error(&key).is_none()
            && !self.inner.in_flight(&key)
            && let Ok(rt) = runtime()
        {
            rt.spawn(self.inner.clone().fetch(link, false));
        }
        None
    }

    /// A [`VaultLookup`] that never blocks (for UI previews and tooltips).
    pub fn cached_lookup(&self) -> CachedLookup {
        CachedLookup(self.clone())
    }

    pub fn status(&self, name: &str) -> LinkStatus {
        let Some(link) = self.linked(name) else { return LinkStatus::NotLinked };
        let key = Key::of(&link);
        if self.inner.in_flight(&key) {
            return LinkStatus::Fetching;
        }
        if let Some(c) = self.inner.cache.lock().get(&key)
            && c.fetched.elapsed() < c.ttl.min(max_age(&link))
        {
            let age = c.fetched.elapsed();
            return LinkStatus::Cached { age, expires_in: c.ttl.min(max_age(&link)).saturating_sub(age) };
        }
        match self.inner.errors.lock().get(&key) {
            Some(f) => LinkStatus::Failed(f.error.clone()),
            None => LinkStatus::NotFetched,
        }
    }

    /// The last error of a linked name, if its last fetch failed.
    pub fn last_error(&self, name: &str) -> Option<SecretError> {
        match self.status(name) {
            LinkStatus::Failed(e) => Some(e),
            _ => None,
        }
    }
}

impl VaultLookup for ExternalSecrets {
    fn get(&self, key: &str) -> Option<String> {
        self.get_blocking(key)
    }
}

/// Non-blocking [`VaultLookup`]; see [`ExternalSecrets::get_cached`].
#[derive(Clone, Debug)]
pub struct CachedLookup(ExternalSecrets);

impl VaultLookup for CachedLookup {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get_cached(key)
    }
}
