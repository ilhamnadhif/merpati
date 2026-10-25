//! The open workspace: model, persistence (debounced autosave on a
//! background thread), secrets, cookies and history.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use merpati_core::environment::Environment;
use merpati_core::history::{HistoryEntry, HistoryStore};
use merpati_core::model::{Collection, Id};
use merpati_core::secrets::{FileBackend, KeychainBackend, SecretStore};
use merpati_core::storage::{Workspace, WorkspaceStore};
use merpati_core::variables::{self, VarType, Variable, VariableScopes};
use merpati_http::CookieJar;

enum SaveJob {
    Collection(Box<Collection>),
    DeleteCollection(Id),
    Environment(Box<Environment>),
    DeleteEnvironment(Id),
    Globals(Vec<Variable>),
    ActiveEnvironment(Option<Id>),
    Cookies,
    History(Box<HistoryEntry>),
    ClearHistory,
    RemoveHistory(Id),
    Flush(mpsc::Sender<()>),
}

pub struct WsState {
    pub store: Arc<WorkspaceStore>,
    pub data: Workspace,
    pub secrets: Arc<SecretStore>,
    pub cookies: Arc<CookieJar>,
    pub history: Vec<HistoryEntry>,
    history_limit: usize,
    saver: mpsc::Sender<SaveJob>,
    errors: mpsc::Receiver<String>,
    dirty_collections: HashSet<Id>,
    dirty_envs: HashSet<Id>,
    dirty_globals: bool,
    dirty_cookies: bool,
    last_change: Instant,
    pub last_error: Option<String>,
    /// Local forks (`.merpati/forks.json`).
    pub forks: Vec<merpati_core::fork::ForkLink>,
    /// Vault names linked to 1Password, AWS, Azure or HashiCorp Vault.
    pub external: Option<merpati_secrets::ExternalSecrets>,
}

pub use merpati_core::secrets::SETTINGS_VAULT_PREFIX;

/// The secrets of the app settings: one Keychain entry for the app rather
/// than per workspace, so they work in every workspace.
pub fn settings_secrets() -> Arc<SecretStore> {
    static STORE: std::sync::OnceLock<Arc<SecretStore>> = std::sync::OnceLock::new();
    STORE
        .get_or_init(|| {
            // Tests never touch the user's Keychain.
            if cfg!(test) {
                return Arc::new(SecretStore::memory());
            }
            let file = || FileBackend {
                path: merpati_core::settings::AppSettings::config_dir().join("settings-secrets.json"),
            };
            Arc::new(match std::env::var("MERPATI_SECRETS").as_deref() {
                Ok("file") => SecretStore::new(Box::new(file())),
                Ok("memory") => SecretStore::memory(),
                _ if KeychainBackend::available() => SecretStore::new(Box::new(KeychainBackend::for_settings())),
                _ => SecretStore::new(Box::new(file())),
            })
        })
        .clone()
}

/// The workspace vault plus the settings' secrets (`settings.` names).
pub fn settings_vault(workspace: Arc<dyn variables::VaultLookup>) -> merpati_core::secrets::SettingsVault {
    merpati_core::secrets::SettingsVault { settings: settings_secrets(), inner: Some(workspace) }
}

fn open_secrets(root: &Path, workspace_id: &str) -> SecretStore {
    let file = FileBackend { path: root.join(".merpati").join("secrets.json") };
    match std::env::var("MERPATI_SECRETS").as_deref() {
        Ok("file") => return SecretStore::new(Box::new(file)),
        Ok("memory") => return SecretStore::memory(),
        _ => {}
    }
    if KeychainBackend::available() {
        let store = SecretStore::new(Box::new(KeychainBackend::for_workspace(workspace_id)));
        if store.load_error().is_none() {
            return store;
        }
        tracing::warn!("keychain unavailable ({:?}); falling back to a local file", store.load_error());
    }
    SecretStore::new(Box::new(file))
}

fn spawn_saver(
    store: Arc<WorkspaceStore>,
    secrets: Arc<SecretStore>,
    cookies: Arc<CookieJar>,
    history: Arc<HistoryStore>,
) -> (mpsc::Sender<SaveJob>, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel::<SaveJob>();
    let (etx, erx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("merpati-saver".into())
        .spawn(move || {
            for job in rx {
                let res: Result<(), String> = match job {
                    SaveJob::Collection(c) => store.save_collection(&c, &secrets).map_err(|e| e.to_string()),
                    SaveJob::DeleteCollection(id) => store.delete_collection(&id, &secrets).map_err(|e| e.to_string()),
                    SaveJob::Environment(e) => store.save_environment(&e, &secrets).map_err(|e| e.to_string()),
                    SaveJob::DeleteEnvironment(id) => {
                        store.delete_environment(&id, &secrets).map_err(|e| e.to_string())
                    }
                    SaveJob::Globals(g) => store.save_globals(&g, &secrets).map_err(|e| e.to_string()),
                    SaveJob::ActiveEnvironment(id) => {
                        store.set_active_environment(id.as_deref()).map_err(|e| e.to_string())
                    }
                    SaveJob::Cookies => cookies.save().map_err(|e| e.to_string()),
                    SaveJob::History(h) => history.append(&h).map_err(|e| e.to_string()),
                    SaveJob::ClearHistory => history.clear().map_err(|e| e.to_string()),
                    SaveJob::RemoveHistory(id) => history.remove(&id).map_err(|e| e.to_string()),
                    SaveJob::Flush(done) => {
                        let _ = done.send(());
                        Ok(())
                    }
                };
                if let Err(e) = res {
                    tracing::error!("save failed: {e}");
                    let _ = etx.send(e);
                }
            }
        })
        .expect("spawn saver thread");
    (tx, erx)
}

impl WsState {
    pub fn open(root: &Path, history_limit: usize) -> Result<WsState, String> {
        let manifest = WorkspaceStore::read_manifest(root).map_err(|e| e.to_string())?;
        let secrets = Arc::new(open_secrets(root, &manifest.id));
        let (store, data) = WorkspaceStore::open(root, &secrets).map_err(|e| e.to_string())?;
        Ok(Self::from_parts(store, data, secrets, history_limit))
    }

    pub fn create(root: &Path, name: &str, history_limit: usize) -> Result<WsState, String> {
        let (store, data) = WorkspaceStore::create(root, name).map_err(|e| e.to_string())?;
        let secrets = Arc::new(open_secrets(root, &data.id));
        Ok(Self::from_parts(store, data, secrets, history_limit))
    }

    /// A new workspace in `root` with in-memory secrets (tests).
    #[cfg(test)]
    pub fn for_test(root: &Path) -> WsState {
        let (store, data) = WorkspaceStore::create(root, "Test").expect("create workspace");
        Self::from_parts(store, data, Arc::new(SecretStore::memory()), 100)
    }

    fn from_parts(store: WorkspaceStore, data: Workspace, secrets: Arc<SecretStore>, history_limit: usize) -> WsState {
        let store = Arc::new(store);
        let local = store.local_dir();
        let cookies = Arc::new(CookieJar::open(local.join("cookies.json")));
        let history_store = Arc::new(HistoryStore::new(local.join("history.jsonl"), history_limit));
        let history = history_store.load();
        let (saver, errors) = spawn_saver(store.clone(), secrets.clone(), cookies.clone(), history_store);
        WsState {
            store,
            data,
            secrets,
            cookies,
            history,
            history_limit,
            saver,
            errors,
            dirty_collections: HashSet::new(),
            dirty_envs: HashSet::new(),
            dirty_globals: false,
            dirty_cookies: false,
            last_change: Instant::now(),
            last_error: None,
            forks: std::fs::read(local.join("forks.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default(),
            external: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.data.root
    }

    pub fn local_dir(&self) -> PathBuf {
        self.store.local_dir()
    }

    pub fn mark_collection(&mut self, id: &str) {
        self.dirty_collections.insert(id.to_string());
        self.last_change = Instant::now();
    }

    pub fn mark_env(&mut self, id: &str) {
        self.dirty_envs.insert(id.to_string());
        self.last_change = Instant::now();
    }

    pub fn mark_globals(&mut self) {
        self.dirty_globals = true;
        self.last_change = Instant::now();
    }

    pub fn mark_cookies(&mut self) {
        self.dirty_cookies = true;
        self.last_change = Instant::now();
    }

    pub fn has_pending(&self) -> bool {
        !self.dirty_collections.is_empty() || !self.dirty_envs.is_empty() || self.dirty_globals || self.dirty_cookies
    }

    /// Saves dirty items once edits have paused for a moment. Returns the
    /// time until the next check is needed.
    pub fn autosave(&mut self) -> Option<Duration> {
        while let Ok(e) = self.errors.try_recv() {
            self.last_error = Some(e);
        }
        if !self.has_pending() {
            return None;
        }
        let quiet = Duration::from_millis(500);
        let since = self.last_change.elapsed();
        if since < quiet {
            return Some(quiet - since);
        }
        self.flush();
        None
    }

    /// Queues every dirty item for saving right away.
    pub fn flush(&mut self) {
        for id in std::mem::take(&mut self.dirty_collections) {
            if let Some(c) = self.data.collection(&id) {
                let _ = self.saver.send(SaveJob::Collection(Box::new(c.clone())));
            }
        }
        for id in std::mem::take(&mut self.dirty_envs) {
            if let Some(e) = self.data.environment(&id) {
                let _ = self.saver.send(SaveJob::Environment(Box::new(e.clone())));
            }
        }
        if std::mem::take(&mut self.dirty_globals) {
            let _ = self.saver.send(SaveJob::Globals(self.data.globals.clone()));
        }
        if std::mem::take(&mut self.dirty_cookies) {
            let _ = self.saver.send(SaveJob::Cookies);
        }
    }

    /// Flushes and waits until everything is on disk (used on exit).
    pub fn flush_blocking(&mut self) {
        self.flush();
        let (tx, rx) = mpsc::channel();
        if self.saver.send(SaveJob::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
    }

    pub fn add_collection(&mut self, c: Collection) -> Id {
        let id = c.id.clone();
        self.data.collections.push(c);
        self.mark_collection(&id);
        id
    }

    pub fn save_forks(&mut self) {
        let path = self.local_dir().join("forks.json");
        let result = serde_json::to_vec_pretty(&self.forks)
            .map_err(|e| e.to_string())
            .and_then(|json| merpati_core::storage::write_atomic(&path, &json).map_err(|e| e.to_string()));
        if let Err(e) = result {
            self.last_error = Some(format!("Could not save forks: {e}"));
        }
    }

    /// What `{{vault:name}}` resolves through: linked external secrets when
    /// configured, otherwise the local secret store.
    pub fn vault(&self) -> Arc<dyn merpati_core::variables::VaultLookup> {
        match &self.external {
            Some(e) => Arc::new(e.clone()),
            None => self.secrets.clone(),
        }
    }

    pub fn fork_of(&self, collection: &str) -> Option<&merpati_core::fork::ForkLink> {
        self.forks.iter().find(|f| f.fork == collection)
    }

    pub fn delete_collection(&mut self, id: &str) {
        if self.forks.iter().any(|f| f.fork == id || f.parent == id) {
            self.forks.retain(|f| f.fork != id && f.parent != id);
            self.save_forks();
        }
        self.data.collections.retain(|c| c.id != id);
        self.dirty_collections.remove(id);
        let _ = self.saver.send(SaveJob::DeleteCollection(id.to_string()));
    }

    pub fn add_environment(&mut self, e: Environment) -> Id {
        let id = e.id.clone();
        self.data.environments.push(e);
        self.mark_env(&id);
        id
    }

    pub fn delete_environment(&mut self, id: &str) {
        self.data.environments.retain(|e| e.id != id);
        self.dirty_envs.remove(id);
        if self.data.active_environment.as_deref() == Some(id) {
            self.data.active_environment = None;
        }
        let _ = self.saver.send(SaveJob::DeleteEnvironment(id.to_string()));
    }

    pub fn set_active_environment(&mut self, id: Option<Id>) {
        self.data.active_environment = id.clone();
        let _ = self.saver.send(SaveJob::ActiveEnvironment(id));
    }

    pub fn add_history(&mut self, entry: HistoryEntry) {
        let _ = self.saver.send(SaveJob::History(Box::new(entry.clone())));
        self.history.insert(0, entry);
        self.history.truncate(self.history_limit);
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        let _ = self.saver.send(SaveJob::ClearHistory);
    }

    pub fn remove_history(&mut self, id: &str) {
        self.history.retain(|h| h.id != id);
        let _ = self.saver.send(SaveJob::RemoveHistory(id.to_string()));
    }

    /// Scopes for a request in `collection_id` (globals, active environment,
    /// collection variables), with the variables' types for scripts.
    pub fn scopes_for(&self, collection_id: Option<&str>) -> VariableScopes {
        let mut s = VariableScopes { globals: variables::to_map(&self.data.globals), ..Default::default() };
        s.types.globals = variables::to_types(&self.data.globals);
        if let Some(env) = self.data.active_environment() {
            s.environment = env.to_map();
            s.types.environment = env.to_types();
            s.environment_name = Some(env.name.clone());
        }
        if let Some(c) = collection_id.and_then(|id| self.data.collection(id)) {
            s.collection = variables::to_map(&c.variables);
            s.types.collection = variables::to_types(&c.variables);
        }
        s
    }

    /// Secret variable names visible to a request (masked in tooltips).
    pub fn secret_keys(&self, collection_id: Option<&str>) -> HashSet<String> {
        self.secret_keys_in(collection_id, self.data.active_environment.as_deref())
    }

    /// [`WsState::secret_keys`] with the environment `env_id`.
    pub fn secret_keys_in(&self, collection_id: Option<&str>, env_id: Option<&str>) -> HashSet<String> {
        let mut out: HashSet<String> =
            self.data.globals.iter().filter(|v| v.is_secret()).map(|v| v.key.clone()).collect();
        if let Some(env) = env_id.and_then(|id| self.data.environment(id)) {
            out.extend(env.variables.iter().filter(|v| v.is_secret()).map(|v| v.key.clone()));
        }
        if let Some(c) = collection_id.and_then(|id| self.data.collection(id)) {
            out.extend(c.variables.iter().filter(|v| v.is_secret()).map(|v| v.key.clone()));
        }
        out
    }

    /// Applies variable changes made by scripts (diff of before/after) to
    /// the model as current values, then schedules saving. `env_id` is the
    /// environment that was active when `before` was taken.
    pub fn apply_var_changes(
        &mut self,
        collection_id: Option<&str>,
        env_id: Option<&str>,
        before: &VariableScopes,
        after: &VariableScopes,
    ) {
        let (bt, at) = (&before.types, &after.types);
        if apply_diff(&mut self.data.globals, (&before.globals, &bt.globals), (&after.globals, &at.globals)) {
            self.mark_globals();
        }
        if let Some(id) = env_id {
            let changed = self
                .data
                .environments
                .iter_mut()
                .find(|e| e.id == id)
                .map(|e| {
                    let before = (&before.environment, &bt.environment);
                    apply_diff(&mut e.variables, before, (&after.environment, &at.environment))
                })
                .unwrap_or(false);
            if changed {
                self.mark_env(id);
            }
        }
        if let Some(cid) = collection_id {
            let changed = self
                .data
                .collection_mut(cid)
                .map(|c| {
                    let before = (&before.collection, &bt.collection);
                    apply_diff(&mut c.variables, before, (&after.collection, &at.collection))
                })
                .unwrap_or(false);
            if changed {
                self.mark_collection(cid);
            }
        }
    }

    /// Local and linked vault names (for completion and highlighting).
    pub fn vault_names(&self) -> Vec<String> {
        match &self.external {
            Some(e) => e.all_names(),
            None => self.secrets.vault_names(),
        }
    }
}

/// A scope's values and their types ([`VariableScopes::types`]).
type TypedValues<'a> = (&'a IndexMap<String, String>, &'a IndexMap<String, VarType>);

/// Applies only the keys that changed between `before` and `after` (value or
/// type), so edits the user made while a request was in flight are not
/// overwritten. A changed variable takes the type the script gave it.
pub fn apply_diff(vars: &mut Vec<Variable>, before: TypedValues, after: TypedValues) -> bool {
    let ty = |types: &IndexMap<String, VarType>, k: &str| types.get(k).copied().unwrap_or_default();
    let mut changed = false;
    for (k, v) in after.0 {
        if before.0.get(k) == Some(v) && ty(before.1, k) == ty(after.1, k) {
            continue;
        }
        changed = true;
        match vars.iter_mut().rev().find(|x| &x.key == k) {
            Some(var) => {
                var.current = Some(v.clone());
                var.enabled = true;
                var.ty = ty(after.1, k);
            }
            None => {
                let mut var = Variable::new(k.clone(), String::new());
                var.current = Some(v.clone());
                var.ty = ty(after.1, k);
                vars.push(var);
            }
        }
    }
    for k in before.0.keys() {
        if !after.0.contains_key(k) {
            vars.retain(|v| &v.key != k);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_only_touches_changed_keys() {
        let mut vars = vec![Variable::new("a", "1"), Variable::new("b", "2")];
        let before = variables::to_map(&vars);
        let mut after = before.clone();
        after.insert("a".into(), "10".into());
        after.insert("c".into(), "3".into());
        after.shift_remove("b");
        // The user edits "b" meanwhile; the script's removal still applies.
        let none = IndexMap::new();
        assert!(apply_diff(&mut vars, (&before, &none), (&after, &none)));
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].effective(), "10");
        assert_eq!(vars[1].key, "c");
    }

    /// A script's `pm.environment.set` lands in the environment the request
    /// was sent with, even if the user switched environments meanwhile.
    #[test]
    fn script_changes_go_to_the_environment_used_for_sending() {
        let dir = tempfile::tempdir().unwrap();
        let mut ws = WsState::for_test(dir.path());
        let mut dev = Environment::new("Dev");
        dev.variables.push(Variable::new("token", "old"));
        let dev = ws.add_environment(dev);
        let prod = ws.add_environment(Environment::new("Prod"));
        ws.set_active_environment(Some(dev.clone()));
        let before = ws.scopes_for(None);
        let env = ws.data.active_environment.clone();
        let mut after = before.clone();
        after.environment.insert("token".into(), "dev-token".into());
        // The user switches to Prod while the request is in flight.
        ws.set_active_environment(Some(prod.clone()));
        ws.apply_var_changes(None, env.as_deref(), &before, &after);
        assert_eq!(ws.data.environment(&dev).unwrap().to_map().get("token").map(String::as_str), Some("dev-token"));
        assert!(ws.data.environment(&prod).unwrap().variables.is_empty());
    }
}
