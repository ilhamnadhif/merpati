//! Secret storage: vault entries, secret variable values, sensitive auth
//! parameters and OAuth tokens. None of these are written to workspace files.
//!
//! All secrets of a workspace are kept as one JSON map in a single backend
//! entry (one Keychain item per workspace), so opening a workspace costs at
//! most one Keychain prompt.

use std::path::PathBuf;

use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};

use crate::tokens::{OAuth2Token, TokenStore};
use crate::variables::VaultLookup;

pub trait SecretBackend: Send + Sync {
    fn load(&self) -> Result<IndexMap<String, String>, String>;
    fn save(&self, secrets: &IndexMap<String, String>) -> Result<(), String>;
    fn describe(&self) -> String;
}

/// Keeps secrets only for the lifetime of the process.
#[derive(Default)]
pub struct MemoryBackend;

impl SecretBackend for MemoryBackend {
    fn load(&self) -> Result<IndexMap<String, String>, String> {
        Ok(IndexMap::new())
    }

    fn save(&self, _: &IndexMap<String, String>) -> Result<(), String> {
        Ok(())
    }

    fn describe(&self) -> String {
        "memory".into()
    }
}

/// Plain JSON file, used when no OS keychain is available (CI, headless
/// Linux). The file lives in the workspace's git-ignored `.merpati/` folder.
pub struct FileBackend {
    pub path: PathBuf,
}

impl SecretBackend for FileBackend {
    fn load(&self) -> Result<IndexMap<String, String>, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IndexMap::new()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, secrets: &IndexMap<String, String>) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let data = serde_json::to_vec_pretty(secrets).map_err(|e| e.to_string())?;
        crate::storage::write_atomic(&self.path, &data).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("file {}", self.path.display())
    }
}

/// In-memory view over a backend. Reads are served from memory; writes are
/// persisted immediately.
pub struct SecretStore {
    backend: Box<dyn SecretBackend>,
    map: RwLock<IndexMap<String, String>>,
    load_error: Option<String>,
    /// Held from snapshot to backend write, so that of two concurrent
    /// writers the older snapshot can never be saved last.
    persist_lock: Mutex<()>,
}

impl Default for SecretStore {
    fn default() -> Self {
        SecretStore::new(Box::new(MemoryBackend))
    }
}

pub const VAULT_KEY_PREFIX: &str = "vault:";
pub const OAUTH_KEY_PREFIX: &str = "oauth2:";

impl SecretStore {
    pub fn new(backend: Box<dyn SecretBackend>) -> Self {
        let (map, load_error) = match backend.load() {
            Ok(m) => (m, None),
            Err(e) => {
                tracing::warn!("could not load secrets from {}: {e}", backend.describe());
                (IndexMap::new(), Some(e))
            }
        };
        SecretStore { backend, map: RwLock::new(map), load_error, persist_lock: Mutex::new(()) }
    }

    pub fn memory() -> Self {
        SecretStore::default()
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn backend_description(&self) -> String {
        self.backend.describe()
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.map.read().get(key).cloned()
    }

    pub fn set(&self, key: &str, value: &str) {
        let changed = {
            let mut map = self.map.write();
            if map.get(key).map(String::as_str) == Some(value) {
                false
            } else {
                map.insert(key.to_string(), value.to_string());
                true
            }
        };
        if changed {
            self.persist();
        }
    }

    pub fn remove(&self, key: &str) {
        let removed = self.map.write().shift_remove(key).is_some();
        if removed {
            self.persist();
        }
    }

    /// Applies many changes with a single backend write.
    pub fn update(&self, f: impl FnOnce(&mut IndexMap<String, String>)) {
        let before = self.map.read().clone();
        {
            let mut map = self.map.write();
            f(&mut map);
        }
        if *self.map.read() != before {
            self.persist();
        }
    }

    pub fn keys_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.map.read().keys().filter(|k| k.starts_with(prefix)).cloned().collect()
    }

    fn persist(&self) {
        if let Some(e) = &self.load_error {
            // The backend holds secrets that could not be read; saving the
            // in-memory map would replace all of them. Changes stay in memory.
            tracing::error!("not saving secrets to {}: they could not be loaded ({e})", self.backend.describe());
            return;
        }
        let _guard = self.persist_lock.lock();
        let snapshot = self.map.read().clone();
        if let Err(e) = self.backend.save(&snapshot) {
            tracing::error!("could not save secrets to {}: {e}", self.backend.describe());
        }
    }

    // Vault helpers: names are visible to the user, values are not.

    pub fn vault_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .keys_with_prefix(VAULT_KEY_PREFIX)
            .into_iter()
            .map(|k| k[VAULT_KEY_PREFIX.len()..].to_string())
            .collect();
        names.sort();
        names
    }

    pub fn vault_get(&self, name: &str) -> Option<String> {
        self.get(&format!("{VAULT_KEY_PREFIX}{name}"))
    }

    pub fn vault_set(&self, name: &str, value: &str) {
        self.set(&format!("{VAULT_KEY_PREFIX}{name}"), value)
    }

    pub fn vault_remove(&self, name: &str) {
        self.remove(&format!("{VAULT_KEY_PREFIX}{name}"))
    }
}

impl VaultLookup for SecretStore {
    fn get(&self, key: &str) -> Option<String> {
        self.vault_get(key)
    }
}

/// Vault names of the app settings' own secrets (proxy password, client
/// certificate passphrases): `{{vault:settings.<name>}}` in `settings.json`.
pub const SETTINGS_VAULT_PREFIX: &str = "settings.";

/// A vault that looks up `settings.` names in the store of the app
/// settings' secrets and every other name in `inner` (the workspace vault).
pub struct SettingsVault {
    pub settings: std::sync::Arc<SecretStore>,
    pub inner: Option<std::sync::Arc<dyn VaultLookup>>,
}

impl VaultLookup for SettingsVault {
    fn get(&self, key: &str) -> Option<String> {
        match key.strip_prefix(SETTINGS_VAULT_PREFIX) {
            Some(name) => self.settings.vault_get(name),
            None => self.inner.as_ref()?.get(key),
        }
    }
}

impl TokenStore for SecretStore {
    fn get(&self, owner_id: &str) -> Option<OAuth2Token> {
        SecretStore::get(self, &format!("{OAUTH_KEY_PREFIX}{owner_id}")).and_then(|s| serde_json::from_str(&s).ok())
    }

    fn set(&self, owner_id: &str, token: Option<OAuth2Token>) {
        let key = format!("{OAUTH_KEY_PREFIX}{owner_id}");
        match token {
            Some(t) => SecretStore::set(self, &key, &serde_json::to_string(&t).unwrap_or_default()),
            None => self.remove(&key),
        }
    }
}

/// macOS Keychain / Windows Credential Manager / Linux keyutils backend.
#[cfg(feature = "keychain")]
pub struct KeychainBackend {
    pub service: String,
    pub account: String,
}

#[cfg(feature = "keychain")]
impl KeychainBackend {
    /// One entry per workspace, keyed by the workspace id.
    pub fn for_workspace(workspace_id: &str) -> Self {
        KeychainBackend { service: "dev.merpati.workspace".into(), account: workspace_id.into() }
    }

    /// The app settings' own secrets, one entry for every workspace (see
    /// [`SETTINGS_VAULT_PREFIX`]).
    pub fn for_settings() -> Self {
        KeychainBackend::for_workspace("app-settings")
    }

    fn entry(&self) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&self.service, &self.account).map_err(|e| e.to_string())
    }

    /// Checks whether the platform keychain works at all (it does not on
    /// headless CI machines).
    pub fn available() -> bool {
        keyring::Entry::new("dev.merpati.probe", "probe").is_ok()
    }
}

#[cfg(feature = "keychain")]
impl SecretBackend for KeychainBackend {
    fn load(&self) -> Result<IndexMap<String, String>, String> {
        match self.entry()?.get_password() {
            Ok(json) => serde_json::from_str(&json).map_err(|e| e.to_string()),
            Err(keyring::Error::NoEntry) => Ok(IndexMap::new()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, secrets: &IndexMap<String, String>) -> Result<(), String> {
        let entry = self.entry()?;
        if secrets.is_empty() {
            return match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.to_string()),
            };
        }
        let json = serde_json::to_string(secrets).map_err(|e| e.to_string())?;
        entry.set_password(&json).map_err(|e| e.to_string())
    }

    fn describe(&self) -> String {
        format!("keychain {}/{}", self.service, self.account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_backend_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        {
            let s = SecretStore::new(Box::new(FileBackend { path: path.clone() }));
            s.vault_set("api", "k1");
            TokenStore::set(&s, "col1", Some(OAuth2Token { access_token: "t".into(), ..Default::default() }));
        }
        let s = SecretStore::new(Box::new(FileBackend { path }));
        assert_eq!(s.vault_get("api").as_deref(), Some("k1"));
        assert_eq!(s.vault_names(), vec!["api"]);
        assert_eq!(TokenStore::get(&s, "col1").unwrap().access_token, "t");
        assert_eq!(VaultLookup::get(&s, "api").as_deref(), Some("k1"));
    }

    #[test]
    fn unreadable_backend_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        // Hand-edited or written by a newer version: cannot be parsed.
        let original = "{\"vault:api\": \"k1\", \"vault:db\": 42}";
        std::fs::write(&path, original).unwrap();
        let s = SecretStore::new(Box::new(FileBackend { path: path.clone() }));
        assert!(s.load_error().is_some());
        // Saving a new token must not replace the secrets it could not read.
        TokenStore::set(&s, "col1", Some(OAuth2Token { access_token: "t".into(), ..Default::default() }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(TokenStore::get(&s, "col1").unwrap().access_token, "t");
    }

    #[test]
    fn concurrent_writes_never_save_an_older_snapshot_last() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        type Saves = Arc<parking_lot::Mutex<Vec<IndexMap<String, String>>>>;
        /// The first save is slow (it waits for the second writer, at most 500 ms).
        struct SlowFirstSave {
            saves: Saves,
            calls: Arc<AtomicUsize>,
            release: parking_lot::Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl SecretBackend for SlowFirstSave {
            fn load(&self) -> Result<IndexMap<String, String>, String> {
                Ok(IndexMap::new())
            }
            fn save(&self, secrets: &IndexMap<String, String>) -> Result<(), String> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    let _ = self.release.lock().recv_timeout(Duration::from_millis(500));
                }
                self.saves.lock().push(secrets.clone());
                Ok(())
            }
            fn describe(&self) -> String {
                "slow".into()
            }
        }
        let saves = Saves::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        let backend =
            SlowFirstSave { saves: saves.clone(), calls: calls.clone(), release: parking_lot::Mutex::new(rx) };
        let store = Arc::new(SecretStore::new(Box::new(backend)));
        let first = {
            let store = store.clone();
            std::thread::spawn(move || store.set("a", "1"))
        };
        while calls.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        store.set("b", "2");
        let _ = tx.send(());
        first.join().unwrap();
        let last = saves.lock().last().cloned().unwrap();
        assert_eq!(last.get("b").map(String::as_str), Some("2"), "the newer secret was overwritten on disk");
        assert_eq!(last.get("a").map(String::as_str), Some("1"));
    }
}

#[cfg(test)]
mod settings_vault_tests {
    use super::*;

    #[test]
    fn settings_names_come_from_the_settings_store() {
        let settings = std::sync::Arc::new(SecretStore::memory());
        settings.vault_set("proxy-password", "s3cret");
        let workspace = std::sync::Arc::new(SecretStore::memory());
        workspace.vault_set("token", "t0k");
        workspace.vault_set("settings.proxy-password", "not this one");
        let inner: std::sync::Arc<dyn VaultLookup> = workspace;
        let vault = SettingsVault { settings: settings.clone(), inner: Some(inner) };
        assert_eq!(vault.get("settings.proxy-password").as_deref(), Some("s3cret"));
        assert_eq!(vault.get("token").as_deref(), Some("t0k"));
        assert_eq!(vault.get("settings.missing"), None);
        let alone = SettingsVault { settings, inner: None };
        assert_eq!(alone.get("token"), None);
    }
}
