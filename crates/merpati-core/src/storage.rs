//! On-disk workspace format (FR-C3): a folder of small, pretty-printed JSON
//! files that diff well in Git.
//!
//! ```text
//! my-workspace/
//!   merpati.json                       workspace manifest
//!   .gitignore                         ignores .merpati/
//!   globals.json
//!   environments/staging.json
//!   collections/sample-api/
//!     collection.json                  auth, scripts, variables, child order
//!     users/
//!       folder.json
//!       list-users.request.json        one file per request
//!   .merpati/                          local only: current values, history, cookies
//! ```
//!
//! Secrets never reach these files: secret variable values, literal values of
//! sensitive auth parameters and current values of secret variables are moved
//! into the [`SecretStore`] on save and injected back on load.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::auth::Auth;
use crate::environment::Environment;
use crate::error::{Error, Result};
use crate::model::*;
use crate::secrets::SecretStore;
use crate::util::{new_id, slugify};
use crate::variables::{Variable, is_pure_reference};

pub const MANIFEST_FILE: &str = "merpati.json";
pub const LOCAL_DIR: &str = ".merpati";
pub const FORMAT_NAME: &str = "merpati-workspace";
pub const FORMAT_VERSION: u32 = 1;
const COLLECTION_FILE: &str = "collection.json";
const FOLDER_FILE: &str = "folder.json";
const REQUEST_SUFFIX: &str = ".request.json";
pub const GLOBALS_OWNER: &str = "globals";

/// Writes via a temporary file and rename so a crash of the app never leaves
/// a half-written file behind. There is deliberately no fsync per file: it
/// made saving a 10,000-request collection take ~45 s, and the rename already
/// gives all-or-nothing files for app crashes.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // Renaming over a symlink would replace the link (settings kept in a
    // dotfiles repository, say): write the file it points to instead.
    let resolved = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path).ok(),
        _ => None,
    };
    let path = resolved.as_deref().unwrap_or(path);
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, data)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn to_pretty<T: Serialize>(v: &T) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(v).expect("model serializes");
    out.push(b'\n');
    out
}

fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<(T, u64)> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let v = serde_json::from_slice(&bytes).map_err(|e| Error::json(path, e))?;
    Ok((v, hash_bytes(&bytes)))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub id: Id,
    pub name: String,
}

/// Local, git-ignored state of a workspace.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalState {
    pub active_environment: Option<Id>,
    /// owner id (environment, collection, folder or `globals`) -> key -> current value.
    /// Only for non-secret variables; secret current values go to the secret store.
    pub current_values: IndexMap<String, IndexMap<String, String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContainerFile {
    #[serde(default = "new_id")]
    id: Id,
    name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(default, skip_serializing_if = "Auth::is_inherit")]
    auth: Auth,
    #[serde(default, skip_serializing_if = "Scripts::is_empty")]
    scripts: Scripts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    variables: Vec<Variable>,
    /// Child entry names in display order: `users` (folder) or
    /// `list-users.request.json`.
    #[serde(default)]
    order: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct GlobalsFile {
    #[serde(default)]
    variables: Vec<Variable>,
}

/// A loaded workspace.
#[derive(Clone, Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub id: Id,
    pub name: String,
    pub collections: Vec<Collection>,
    pub environments: Vec<Environment>,
    pub globals: Vec<Variable>,
    pub active_environment: Option<Id>,
    /// Files that could not be read; the rest of the workspace still loads.
    pub load_errors: Vec<String>,
}

impl Workspace {
    pub fn collection(&self, id: &str) -> Option<&Collection> {
        self.collections.iter().find(|c| c.id == id)
    }

    pub fn collection_mut(&mut self, id: &str) -> Option<&mut Collection> {
        self.collections.iter_mut().find(|c| c.id == id)
    }

    pub fn environment(&self, id: &str) -> Option<&Environment> {
        self.environments.iter().find(|e| e.id == id)
    }

    pub fn active_environment(&self) -> Option<&Environment> {
        self.active_environment.as_deref().and_then(|id| self.environment(id))
    }

    /// Finds the collection containing an item.
    pub fn collection_of(&self, item_id: &str) -> Option<&Collection> {
        self.collections.iter().find(|c| c.id == item_id || c.find(item_id).is_some())
    }

    pub fn local_dir(&self) -> PathBuf {
        self.root.join(LOCAL_DIR)
    }
}

#[derive(Default)]
struct Index {
    /// relative path -> hash of the content we last wrote or read.
    written: HashMap<PathBuf, u64>,
    collection_dirs: HashMap<Id, PathBuf>,
    env_files: HashMap<Id, PathBuf>,
    state: LocalState,
}

/// Persists a workspace. Cheap to share between threads; all methods take
/// `&self`.
pub struct WorkspaceStore {
    root: PathBuf,
    id: Id,
    index: Mutex<Index>,
}

/// Secret store key for a sensitive auth parameter.
pub fn auth_secret_key(owner: &str, param: &str) -> String {
    format!("auth:{owner}:{param}")
}

fn var_secret_key(owner: &str, key: &str) -> String {
    format!("var:{owner}:{key}")
}

fn cur_secret_key(owner: &str, key: &str) -> String {
    format!("cur:{owner}:{key}")
}

fn owner_prefixes(owner: &str) -> [String; 3] {
    [format!("auth:{owner}:"), format!("var:{owner}:"), format!("cur:{owner}:")]
}

/// Secret owner of the auth of an example's request. Nested under the
/// request (`auth:<request>:...`), so saving the request also drops the
/// secrets of examples that were removed; example ids alone are shared
/// between a collection and its forks.
fn example_owner(request_id: &str, example_id: &str) -> String {
    format!("{request_id}:example:{example_id}")
}

/// Where each variable's secret and current values are kept: its name, with
/// `#2`, `#3`... for later variables of the same name (a disabled and an
/// enabled `token`, say), so that duplicates keep their own values.
fn var_slots(vars: &[Variable]) -> Vec<String> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    vars.iter()
        .map(|v| {
            let n = seen.entry(v.key.as_str()).or_default();
            *n += 1;
            if *n == 1 { v.key.clone() } else { format!("{}#{n}", v.key) }
        })
        .collect()
}

/// Collects secrets out of a model while rendering it to files.
#[derive(Default)]
struct SecretSink {
    owners: Vec<String>,
    secrets: Vec<(String, String)>,
    current: IndexMap<String, IndexMap<String, String>>,
}

impl SecretSink {
    fn auth(&mut self, owner: &str, auth: &mut Auth) {
        self.touch(owner);
        let sensitive = auth.kind.sensitive_params();
        for (k, v) in auth.params.iter_mut() {
            if !sensitive.contains(&k.as_str()) {
                continue;
            }
            if let serde_json::Value::String(s) = v
                && !s.is_empty()
                && !is_pure_reference(s)
            {
                self.secrets.push((auth_secret_key(owner, k), std::mem::take(s)));
            }
        }
    }

    fn variables(&mut self, owner: &str, vars: &mut [Variable]) {
        self.touch(owner);
        let slots = var_slots(vars);
        for (v, slot) in vars.iter_mut().zip(slots) {
            if v.is_secret() {
                if !v.value.is_empty() {
                    self.secrets.push((var_secret_key(owner, &slot), std::mem::take(&mut v.value)));
                }
                if let Some(cur) = v.current.take() {
                    self.secrets.push((cur_secret_key(owner, &slot), cur));
                }
            } else if let Some(cur) = v.current.take() {
                self.current.entry(owner.to_string()).or_default().insert(slot, cur);
            }
        }
    }

    fn touch(&mut self, owner: &str) {
        if !self.owners.iter().any(|o| o == owner) {
            self.owners.push(owner.to_string());
        }
    }

    fn commit(self, store: &SecretStore, state: &mut LocalState) {
        store.update(|map| {
            for owner in &self.owners {
                let prefixes = owner_prefixes(owner);
                map.retain(|k, _| !prefixes.iter().any(|p| k.starts_with(p.as_str())));
            }
            for (k, v) in &self.secrets {
                map.insert(k.clone(), v.clone());
            }
        });
        for owner in &self.owners {
            state.current_values.shift_remove(owner);
        }
        for (owner, vals) in self.current {
            state.current_values.insert(owner, vals);
        }
    }
}

/// Puts secrets and current values back into a freshly loaded model.
struct SecretSource<'a> {
    store: &'a SecretStore,
    state: &'a LocalState,
}

impl SecretSource<'_> {
    fn auth(&self, owner: &str, auth: &mut Auth) {
        let sensitive = auth.kind.sensitive_params();
        for (k, v) in auth.params.iter_mut() {
            if !sensitive.contains(&k.as_str()) {
                continue;
            }
            let empty = matches!(v, serde_json::Value::String(s) if s.is_empty()) || v.is_null();
            if empty && let Some(secret) = self.store.get(&auth_secret_key(owner, k)) {
                *v = serde_json::Value::String(secret);
            }
        }
    }

    fn variables(&self, owner: &str, vars: &mut [Variable]) {
        let current = self.state.current_values.get(owner);
        let slots = var_slots(vars);
        for (v, slot) in vars.iter_mut().zip(slots) {
            if v.is_secret() {
                if v.value.is_empty()
                    && let Some(s) = self.store.get(&var_secret_key(owner, &slot))
                {
                    v.value = s;
                }
                v.current = self.store.get(&cur_secret_key(owner, &slot));
            } else {
                v.current = current.and_then(|c| c.get(&slot)).cloned();
            }
        }
    }
}

fn is_managed_file(name: &str) -> bool {
    name == COLLECTION_FILE || name == FOLDER_FILE || name.ends_with(REQUEST_SUFFIX)
}

/// Device names Windows reserves, also with an extension (`nul.request.json`).
fn is_reserved_on_windows(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_lowercase();
    match stem.as_str() {
        "con" | "prn" | "aux" | "nul" => true,
        s => s.strip_prefix("com").or_else(|| s.strip_prefix("lpt")).is_some_and(|d| {
            let mut c = d.chars();
            matches!((c.next(), c.next()), (Some('0'..='9' | '¹' | '²' | '³'), None))
        }),
    }
}

/// An `order` entry must name a direct child: one plain path component, so
/// a hand-edited or merged file can neither reach outside its folder (`../x`,
/// `/x`) nor make loading recurse into the folder itself (`.`).
fn is_child_name(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!((parts.next(), parts.next()), (Some(std::path::Component::Normal(_)), None)) && !name.contains(['/', '\\'])
}

/// Whether an existing file or folder name can stay for `slug`: the slug in
/// any case (hand-made names; on a case-insensitive file system writing the
/// slug spelling goes to the same entry anyway) or a numbered variant of it.
fn keeps_name(existing: &str, slug: &str) -> bool {
    let n = existing.to_lowercase();
    (n == slug || n.starts_with(&format!("{slug}-"))) && !is_reserved_on_windows(&n)
}

fn unique_name(base: &str, suffix: &str, used: &mut HashSet<String>) -> String {
    let mut candidate = format!("{base}{suffix}");
    let mut n = 2;
    while used.contains(&candidate.to_lowercase()) || is_reserved_on_windows(&candidate) {
        candidate = format!("{base}-{n}{suffix}");
        n += 1;
    }
    used.insert(candidate.to_lowercase());
    candidate
}

impl WorkspaceStore {
    pub fn is_workspace(root: &Path) -> bool {
        root.join(MANIFEST_FILE).is_file()
    }

    /// Creates a new, empty workspace at `root` (which may already exist).
    pub fn create(root: &Path, name: &str) -> Result<(WorkspaceStore, Workspace)> {
        if Self::is_workspace(root) {
            return Err(Error::Other(format!("{} already contains a workspace", root.display())));
        }
        std::fs::create_dir_all(root).map_err(|e| Error::io(root, e))?;
        let manifest =
            Manifest { format: FORMAT_NAME.into(), version: FORMAT_VERSION, id: new_id(), name: name.to_string() };
        write_atomic(&root.join(MANIFEST_FILE), &to_pretty(&manifest)).map_err(|e| Error::io(root, e))?;
        let gitignore = root.join(".gitignore");
        if !gitignore.exists() {
            std::fs::write(&gitignore, format!("{LOCAL_DIR}/\n")).map_err(|e| Error::io(&gitignore, e))?;
        }
        let store = WorkspaceStore { root: root.to_path_buf(), id: manifest.id.clone(), index: Mutex::default() };
        let ws = Workspace {
            root: root.to_path_buf(),
            id: manifest.id,
            name: manifest.name,
            collections: Vec::new(),
            environments: Vec::new(),
            globals: Vec::new(),
            active_environment: None,
            load_errors: Vec::new(),
        };
        Ok((store, ws))
    }

    /// Reads only the manifest (to find the workspace id before the secret
    /// store is opened).
    pub fn read_manifest(root: &Path) -> Result<Manifest> {
        let path = root.join(MANIFEST_FILE);
        let (m, _): (Manifest, _) = read_json(&path)?;
        if m.format != FORMAT_NAME {
            return Err(Error::Format(format!("{} is not a Merpati workspace", path.display())));
        }
        if m.version > FORMAT_VERSION {
            return Err(Error::Format(format!(
                "workspace format version {} is newer than this Merpati supports ({FORMAT_VERSION})",
                m.version
            )));
        }
        Ok(m)
    }

    pub fn open(root: &Path, secrets: &SecretStore) -> Result<(WorkspaceStore, Workspace)> {
        let manifest = Self::read_manifest(root)?;
        let mut index = Index::default();
        let mut load_errors = Vec::new();

        let state_path = root.join(LOCAL_DIR).join("state.json");
        index.state = match std::fs::read(&state_path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                load_errors.push(format!("{}: {e}", state_path.display()));
                LocalState::default()
            }),
            Err(_) => LocalState::default(),
        };
        let source = SecretSource { store: secrets, state: &index.state };

        let mut globals = Vec::new();
        let gpath = root.join("globals.json");
        if gpath.is_file() {
            match read_json::<GlobalsFile>(&gpath) {
                Ok((g, h)) => {
                    globals = g.variables;
                    index.written.insert(PathBuf::from("globals.json"), h);
                }
                Err(e) => load_errors.push(e.to_string()),
            }
        }
        source.variables(GLOBALS_OWNER, &mut globals);

        let mut environments = Vec::new();
        let mut env_ids = HashSet::new();
        let env_dir = root.join("environments");
        if let Ok(rd) = std::fs::read_dir(&env_dir) {
            let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            paths.sort();
            for p in paths.into_iter().filter(|p| p.extension().map(|e| e == "json").unwrap_or(false)) {
                match read_json::<Environment>(&p) {
                    Ok((mut env, h)) => {
                        let rel = p.strip_prefix(root).unwrap_or(&p).to_path_buf();
                        source.variables(&env.id, &mut env.variables);
                        // A file copied by hand (or left by a merge) has the
                        // same id; files are tracked by id, so without its own
                        // id saving one environment would overwrite or delete
                        // the other's file. Secrets were injected first.
                        if !env_ids.insert(env.id.clone()) {
                            env.id = new_id();
                        }
                        index.written.insert(rel.clone(), h);
                        index.env_files.insert(env.id.clone(), rel);
                        environments.push(env);
                    }
                    Err(e) => load_errors.push(e.to_string()),
                }
            }
        }

        let mut collections = Vec::new();
        let mut collection_ids = HashSet::new();
        let col_dir = root.join("collections");
        if let Ok(rd) = std::fs::read_dir(&col_dir) {
            let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
            dirs.sort();
            for d in dirs {
                if !d.join(COLLECTION_FILE).is_file() {
                    continue;
                }
                match load_container(root, &d, COLLECTION_FILE, &mut index.written, &mut load_errors) {
                    Ok((meta, items)) => {
                        let mut c = Collection {
                            id: meta.id,
                            name: meta.name,
                            description: meta.description,
                            auth: meta.auth,
                            scripts: meta.scripts,
                            variables: meta.variables,
                            items,
                        };
                        inject_collection(&source, &mut c);
                        // A copied collection folder: own ids for it and its
                        // items (see environments above; secrets and tokens
                        // are kept per item id).
                        if !collection_ids.insert(c.id.clone()) {
                            c.id = new_id();
                            for item in &mut c.items {
                                *item = item.duplicate();
                            }
                        }
                        let rel = d.strip_prefix(root).unwrap_or(&d).to_path_buf();
                        index.collection_dirs.insert(c.id.clone(), rel);
                        collections.push(c);
                    }
                    Err(e) => load_errors.push(e.to_string()),
                }
            }
        }

        let active = index.state.active_environment.clone().filter(|id| environments.iter().any(|e| &e.id == id));
        let store = WorkspaceStore { root: root.to_path_buf(), id: manifest.id.clone(), index: Mutex::new(index) };
        let ws = Workspace {
            root: root.to_path_buf(),
            id: manifest.id,
            name: manifest.name,
            collections,
            environments,
            globals,
            active_environment: active,
            load_errors,
        };
        Ok((store, ws))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_id(&self) -> &str {
        &self.id
    }

    pub fn local_dir(&self) -> PathBuf {
        self.root.join(LOCAL_DIR)
    }

    pub fn rename_workspace(&self, name: &str) -> Result<()> {
        let manifest =
            Manifest { format: FORMAT_NAME.into(), version: FORMAT_VERSION, id: self.id.clone(), name: name.into() };
        let path = self.root.join(MANIFEST_FILE);
        write_atomic(&path, &to_pretty(&manifest)).map_err(|e| Error::io(path, e))
    }

    fn write_state(&self, state: &LocalState) -> Result<()> {
        let path = self.local_dir().join("state.json");
        write_atomic(&path, &to_pretty(state)).map_err(|e| Error::io(path, e))
    }

    pub fn set_active_environment(&self, id: Option<&str>) -> Result<()> {
        let mut idx = self.index.lock();
        idx.state.active_environment = id.map(str::to_string);
        let state = idx.state.clone();
        drop(idx);
        self.write_state(&state)
    }

    /// Writes `files` (relative path -> content), skipping unchanged ones.
    fn write_files(&self, idx: &mut Index, files: &[(PathBuf, Vec<u8>)]) -> Result<()> {
        for (rel, data) in files {
            let h = hash_bytes(data);
            let abs = self.root.join(rel);
            if idx.written.get(rel) == Some(&h) && abs.is_file() {
                continue;
            }
            write_atomic(&abs, data).map_err(|e| Error::io(&abs, e))?;
            idx.written.insert(rel.clone(), h);
        }
        Ok(())
    }

    /// Removes managed files under `dir_rel` that are not in `keep`, then
    /// empty directories. Only files this store read or wrote are removed: a
    /// file that failed to load (merge conflict markers, say) or that
    /// appeared since is the user's data, not a leftover.
    fn prune(&self, idx: &mut Index, dir_rel: &Path, keep: &HashSet<PathBuf>) -> Result<()> {
        let abs = self.root.join(dir_rel);
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        walk_managed(&abs, &mut files, &mut dirs);
        // On a case- (or normalization-) insensitive file system a path can
        // name a file under another spelling: a hand-made `Users.request.json`
        // *is* the `users.request.json` just written (the rename kept the old
        // spelling). So files are matched by identity, not by path.
        let mut ids = None;
        for f in files {
            let rel = f.strip_prefix(&self.root).unwrap_or(&f).to_path_buf();
            if keep.contains(&rel) {
                continue;
            }
            let (kept, known): &(HashSet<_>, HashMap<_, PathBuf>) = ids.get_or_insert_with(|| {
                let kept = keep.iter().filter_map(|k| file_id(&self.root.join(k))).collect();
                let known = idx
                    .written
                    .keys()
                    .filter(|k| k.starts_with(dir_rel))
                    .filter_map(|k| Some((file_id(&self.root.join(k))?, k.clone())))
                    .collect();
                (kept, known)
            });
            let id = file_id(&f);
            if id.as_ref().is_some_and(|id| kept.contains(id)) {
                continue;
            }
            let known_as = match idx.written.contains_key(&rel) {
                true => Some(rel.clone()),
                false => id.and_then(|id| known.get(&id).cloned()),
            };
            if let Some(key) = known_as {
                std::fs::remove_file(&f).map_err(|e| Error::io(&f, e))?;
                idx.written.remove(&key);
                idx.written.remove(&rel);
            }
        }
        // Deepest first so parents become empty.
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        dirs.push(abs);
        for d in dirs {
            if std::fs::read_dir(&d).map(|mut r| r.next().is_none()).unwrap_or(false) {
                let _ = std::fs::remove_dir(&d);
            }
        }
        Ok(())
    }

    /// Managed files under `dir_rel` (relative paths) that this store
    /// neither read nor wrote.
    fn unknown_files(&self, idx: &Index, dir_rel: &Path) -> HashSet<PathBuf> {
        let mut files = Vec::new();
        walk_managed(&self.root.join(dir_rel), &mut files, &mut Vec::new());
        let mut unknown: HashSet<PathBuf> = files
            .into_iter()
            .map(|f| f.strip_prefix(&self.root).unwrap_or(&f).to_path_buf())
            .filter(|rel| !idx.written.contains_key(rel))
            .collect();
        if !unknown.is_empty() {
            // A known file listed under another spelling (`Users.request.json`
            // for `users.request.json`) on a case-insensitive file system.
            let known: HashSet<_> = idx
                .written
                .keys()
                .filter(|k| k.starts_with(dir_rel))
                .filter_map(|k| file_id(&self.root.join(k)))
                .collect();
            unknown.retain(|u| !file_id(&self.root.join(u)).is_some_and(|id| known.contains(&id)));
        }
        unknown
    }

    pub fn save_collection(&self, c: &Collection, secrets: &SecretStore) -> Result<()> {
        let mut idx = self.index.lock();
        let slug = slugify(&c.name);
        let old_dir = idx.collection_dirs.get(&c.id).cloned();
        let dir = match &old_dir {
            Some(d) if d.file_name().and_then(|n| n.to_str()).map(|n| keeps_name(n, &slug)).unwrap_or(false) => {
                d.clone()
            }
            _ => {
                let mut used: HashSet<String> = idx
                    .collection_dirs
                    .iter()
                    .filter(|(id, _)| *id != &c.id)
                    .filter_map(|(_, d)| d.file_name().and_then(|n| n.to_str()).map(|n| n.to_lowercase()))
                    .collect();
                // Also avoid directories that exist on disk but are not loaded.
                if let Ok(rd) = std::fs::read_dir(self.root.join("collections")) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_lowercase();
                        if old_dir.as_ref().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_lowercase())
                            != Some(name.clone())
                        {
                            used.insert(name);
                        }
                    }
                }
                PathBuf::from("collections").join(unique_name(&slug, "", &mut used))
            }
        };

        let mut sink = SecretSink::default();
        let mut model = c.clone();
        let mut files = Vec::new();
        let unknown = self.unknown_files(&idx, &dir);
        render_collection(&mut model, &dir, &unknown, &mut sink, &mut files);
        self.write_files(&mut idx, &files)?;
        let keep: HashSet<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
        self.prune(&mut idx, &dir, &keep)?;
        if let Some(old) = old_dir.filter(|o| o != &dir && !same_file(&self.root.join(o), &self.root.join(&dir))) {
            self.prune(&mut idx, &old, &HashSet::new())?;
        }
        idx.collection_dirs.insert(c.id.clone(), dir);
        sink.commit(secrets, &mut idx.state);
        let state = idx.state.clone();
        drop(idx);
        self.write_state(&state)
    }

    pub fn delete_collection(&self, id: &str, secrets: &SecretStore) -> Result<()> {
        let mut idx = self.index.lock();
        if let Some(dir) = idx.collection_dirs.remove(id) {
            self.prune(&mut idx, &dir, &HashSet::new())?;
        }
        let prefix_owner = format!(":{id}:");
        secrets.update(|m| m.retain(|k, _| !k.contains(&prefix_owner)));
        idx.state.current_values.shift_remove(id);
        Ok(())
    }

    pub fn save_environment(&self, env: &Environment, secrets: &SecretStore) -> Result<()> {
        let mut idx = self.index.lock();
        let slug = slugify(&env.name);
        let old = idx.env_files.get(&env.id).cloned();
        let rel = match &old {
            Some(p) if p.file_stem().and_then(|n| n.to_str()).map(|n| keeps_name(n, &slug)).unwrap_or(false) => {
                p.clone()
            }
            _ => {
                let mut used: HashSet<String> = idx
                    .env_files
                    .iter()
                    .filter(|(id, _)| *id != &env.id)
                    .filter_map(|(_, p)| p.file_name().and_then(|n| n.to_str()).map(|n| n.to_lowercase()))
                    .collect();
                if let Ok(rd) = std::fs::read_dir(self.root.join("environments")) {
                    for e in rd.flatten() {
                        used.insert(e.file_name().to_string_lossy().to_lowercase());
                    }
                }
                if let Some(o) = &old {
                    used.remove(&o.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default());
                }
                PathBuf::from("environments").join(unique_name(&slug, ".json", &mut used))
            }
        };
        let mut sink = SecretSink::default();
        let mut model = env.clone();
        sink.variables(&model.id, &mut model.variables);
        self.write_files(&mut idx, &[(rel.clone(), to_pretty(&model))])?;
        if let Some(o) = old.filter(|o| o != &rel) {
            let abs = self.root.join(&o);
            if !same_file(&abs, &self.root.join(&rel)) {
                let _ = std::fs::remove_file(&abs);
            }
            idx.written.remove(&o);
        }
        idx.env_files.insert(env.id.clone(), rel);
        sink.commit(secrets, &mut idx.state);
        let state = idx.state.clone();
        drop(idx);
        self.write_state(&state)
    }

    pub fn delete_environment(&self, id: &str, secrets: &SecretStore) -> Result<()> {
        let mut idx = self.index.lock();
        if let Some(rel) = idx.env_files.remove(id) {
            let abs = self.root.join(&rel);
            if abs.exists() {
                std::fs::remove_file(&abs).map_err(|e| Error::io(&abs, e))?;
            }
            idx.written.remove(&rel);
        }
        let prefixes = owner_prefixes(id);
        secrets.update(|m| m.retain(|k, _| !prefixes.iter().any(|p| k.starts_with(p.as_str()))));
        idx.state.current_values.shift_remove(id);
        if idx.state.active_environment.as_deref() == Some(id) {
            idx.state.active_environment = None;
        }
        let state = idx.state.clone();
        drop(idx);
        self.write_state(&state)
    }

    pub fn save_globals(&self, vars: &[Variable], secrets: &SecretStore) -> Result<()> {
        let mut idx = self.index.lock();
        let mut sink = SecretSink::default();
        let mut model = vars.to_vec();
        sink.variables(GLOBALS_OWNER, &mut model);
        let file = GlobalsFile { variables: model };
        self.write_files(&mut idx, &[(PathBuf::from("globals.json"), to_pretty(&file))])?;
        sink.commit(secrets, &mut idx.state);
        let state = idx.state.clone();
        drop(idx);
        self.write_state(&state)
    }

    /// Path of the folder containing a collection's files, if saved.
    pub fn collection_dir(&self, id: &str) -> Option<PathBuf> {
        self.index.lock().collection_dirs.get(id).map(|d| self.root.join(d))
    }
}

/// Identity of the file at `p`, to tell when two paths name the same file.
#[cfg(unix)]
fn file_id(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
}

/// Windows file systems are case-insensitive: compare paths ignoring case.
#[cfg(not(unix))]
fn file_id(p: &Path) -> Option<String> {
    p.exists().then(|| p.to_string_lossy().to_lowercase())
}

/// Whether two paths name the same existing file or folder (another spelling
/// on a case- or normalization-insensitive file system).
fn same_file(a: &Path, b: &Path) -> bool {
    file_id(a).is_some_and(|id| file_id(b) == Some(id))
}

/// Managed files (and every directory) under `abs`, recursively.
fn walk_managed(abs: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(abs) else { return };
    for e in rd.flatten() {
        let p = e.path();
        // The entry type needs no `stat` (10,000 files per save); symlinks
        // are followed as before.
        let is_dir = match e.file_type() {
            Ok(t) if !t.is_symlink() => t.is_dir(),
            _ => p.is_dir(),
        };
        if is_dir {
            walk_managed(&p, files, dirs);
            dirs.push(p);
        } else if p.file_name().and_then(|n| n.to_str()).map(is_managed_file).unwrap_or(false) {
            files.push(p);
        }
    }
}

fn render_collection(
    c: &mut Collection,
    dir: &Path,
    unknown: &HashSet<PathBuf>,
    sink: &mut SecretSink,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) {
    sink.auth(&c.id.clone(), &mut c.auth);
    sink.variables(&c.id.clone(), &mut c.variables);
    let order = render_children(&mut c.items, dir, unknown, sink, files);
    let meta = ContainerFile {
        id: c.id.clone(),
        name: c.name.clone(),
        description: c.description.clone(),
        auth: c.auth.clone(),
        scripts: c.scripts.clone(),
        variables: c.variables.clone(),
        order,
    };
    files.push((dir.join(COLLECTION_FILE), to_pretty(&meta)));
}

fn render_children(
    items: &mut [Item],
    dir: &Path,
    unknown: &HashSet<PathBuf>,
    sink: &mut SecretSink,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) -> Vec<String> {
    let mut used = HashSet::new();
    used.insert(COLLECTION_FILE.to_string());
    used.insert(FOLDER_FILE.to_string());
    // Never write over a request or folder here that failed to load.
    for p in unknown {
        let Ok(rest) = p.strip_prefix(dir) else { continue };
        let mut parts = rest.iter();
        match (parts.next(), parts.next(), parts.next()) {
            (Some(file), None, _) => used.insert(file.to_string_lossy().to_lowercase()),
            (Some(sub), Some(meta), None) if meta == FOLDER_FILE => used.insert(sub.to_string_lossy().to_lowercase()),
            _ => false,
        };
    }
    let mut order = Vec::with_capacity(items.len());
    for item in items.iter_mut() {
        match item {
            Item::Folder(f) => {
                let name = unique_name(&slugify(&f.name), "", &mut used);
                let sub = dir.join(&name);
                sink.auth(&f.id.clone(), &mut f.auth);
                sink.variables(&f.id.clone(), &mut f.variables);
                let child_order = render_children(&mut f.items, &sub, unknown, sink, files);
                let meta = ContainerFile {
                    id: f.id.clone(),
                    name: f.name.clone(),
                    description: f.description.clone(),
                    auth: f.auth.clone(),
                    scripts: f.scripts.clone(),
                    variables: f.variables.clone(),
                    order: child_order,
                };
                files.push((sub.join(FOLDER_FILE), to_pretty(&meta)));
                order.push(name);
            }
            Item::Request(r) => {
                let name = unique_name(&slugify(&r.name), REQUEST_SUFFIX, &mut used);
                let id = r.id.clone();
                sink.auth(&id, r.request.auth_mut());
                // "Save as example" copies the request, credentials included.
                for ex in &mut r.examples {
                    if let Some(req) = &mut ex.request {
                        sink.auth(&example_owner(&id, &ex.id), &mut req.auth);
                    }
                }
                files.push((dir.join(&name), to_pretty(r)));
                order.push(name);
            }
        }
    }
    order
}

fn load_container(
    root: &Path,
    dir: &Path,
    meta_file: &str,
    written: &mut HashMap<PathBuf, u64>,
    errors: &mut Vec<String>,
) -> Result<(ContainerFile, Vec<Item>)> {
    let meta_path = dir.join(meta_file);
    let (meta, h): (ContainerFile, u64) = read_json(&meta_path)?;
    written.insert(meta_path.strip_prefix(root).unwrap_or(&meta_path).to_path_buf(), h);

    // Entries listed in `order` first, then anything else found on disk
    // (e.g. files added by hand or by a Git merge), alphabetically.
    let mut seen = HashSet::new();
    let mut names: Vec<String> =
        meta.order.iter().filter(|n| is_child_name(n) && seen.insert(n.to_string())).cloned().collect();
    let mut extra = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            let is_entry = (p.is_dir() && p.join(FOLDER_FILE).is_file()) || name.ends_with(REQUEST_SUFFIX);
            if is_entry && !seen.contains(&name) {
                extra.push(name);
            }
        }
    }
    if !extra.is_empty() {
        // On a case-insensitive file system an entry of `order` may be
        // listed with another spelling (`Users` for `users`): load it once.
        let listed: HashSet<_> = names.iter().filter_map(|n| file_id(&dir.join(n))).collect();
        extra.retain(|n| !file_id(&dir.join(n)).is_some_and(|id| listed.contains(&id)));
    }
    extra.sort();
    names.extend(extra);

    let mut items = Vec::new();
    for name in names {
        let p = dir.join(&name);
        if name.ends_with(REQUEST_SUFFIX) {
            if !p.is_file() {
                continue;
            }
            match read_json::<RequestItem>(&p) {
                Ok((r, h)) => {
                    written.insert(p.strip_prefix(root).unwrap_or(&p).to_path_buf(), h);
                    items.push(Item::Request(r));
                }
                Err(e) => errors.push(e.to_string()),
            }
        } else if p.join(FOLDER_FILE).is_file() {
            match load_container(root, &p, FOLDER_FILE, written, errors) {
                Ok((m, children)) => items.push(Item::Folder(Folder {
                    id: m.id,
                    name: m.name,
                    description: m.description,
                    auth: m.auth,
                    scripts: m.scripts,
                    variables: m.variables,
                    items: children,
                })),
                Err(e) => errors.push(e.to_string()),
            }
        }
    }
    Ok((meta, items))
}

fn inject_collection(source: &SecretSource<'_>, c: &mut Collection) {
    let id = c.id.clone();
    source.auth(&id, &mut c.auth);
    source.variables(&id, &mut c.variables);
    fn walk(source: &SecretSource<'_>, items: &mut [Item]) {
        for item in items {
            match item {
                Item::Folder(f) => {
                    let id = f.id.clone();
                    source.auth(&id, &mut f.auth);
                    source.variables(&id, &mut f.variables);
                    walk(source, &mut f.items);
                }
                Item::Request(r) => {
                    let id = r.id.clone();
                    source.auth(&id, r.request.auth_mut());
                    for ex in &mut r.examples {
                        if let Some(req) = &mut ex.request {
                            source.auth(&example_owner(&id, &ex.id), &mut req.auth);
                        }
                    }
                }
            }
        }
    }
    walk(source, &mut c.items);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postman;

    fn sample_collection() -> Collection {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/sample.postman_collection.json")).unwrap();
        postman::import_collection(&v).unwrap().0
    }

    fn all_text(root: &Path) -> String {
        fn walk(p: &Path, out: &mut String) {
            for e in std::fs::read_dir(p).unwrap().flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
                }
            }
        }
        let mut s = String::new();
        walk(root, &mut s);
        s
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let mut c = sample_collection();
        c.variables[0].current = Some("http://localhost:9999".into());
        store.save_collection(&c, &secrets).unwrap();
        let mut env = Environment::new("Staging").with_var("baseUrl", "https://staging");
        env.variables.push(Variable::secret("apiKey", "k-123"));
        store.save_environment(&env, &secrets).unwrap();
        store.set_active_environment(Some(&env.id)).unwrap();
        store.save_globals(&[Variable::new("g", "1")], &secrets).unwrap();

        assert!(dir.path().join("collections/sample-api/collection.json").is_file());
        assert!(dir.path().join("collections/sample-api/users/folder.json").is_file());
        assert!(dir.path().join("collections/sample-api/users/list-users.request.json").is_file());
        assert!(dir.path().join("environments/staging.json").is_file());

        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert!(ws.load_errors.is_empty(), "{:?}", ws.load_errors);
        assert_eq!(ws.collections.len(), 1);
        assert_eq!(ws.collections[0], c);
        assert_eq!(ws.environments[0], env);
        assert_eq!(ws.active_environment.as_deref(), Some(env.id.as_str()));
        assert_eq!(ws.globals[0].value, "1");
    }

    #[test]
    fn variable_types_survive_reload_and_old_files_load() {
        use crate::variables::VarType;
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let typed = |k: &str, v: &str, ty| Variable { ty, ..Variable::new(k, v) };
        let mut c = Collection::new("Typed");
        c.variables = vec![typed("n", "5", VarType::Number), Variable::new("plain", "x")];
        let mut f = Folder::new("F");
        f.variables.push(typed("b", "true", VarType::Boolean));
        c.items.push(f.into());
        store.save_collection(&c, &secrets).unwrap();
        let mut env = Environment::new("Typed");
        env.variables = vec![typed("o", r#"{"a":1}"#, VarType::Any), typed("t", "7", VarType::String)];
        env.variables[0].current = Some("[1]".into());
        store.save_environment(&env, &secrets).unwrap();
        store.save_globals(&[typed("g", "false", VarType::Boolean)], &secrets).unwrap();
        // An environment file written before variables had types.
        let old = r#"{"id": "old", "name": "Old", "variables": [
            {"key": "a", "value": "1"}, {"key": "b", "value": "2", "enabled": false, "description": "d"}]}"#;
        std::fs::write(dir.path().join("environments/old.json"), old).unwrap();

        let file: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("collections/typed/collection.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            file["variables"],
            serde_json::json!([{"key": "n", "value": "5", "type": "number"}, {"key": "plain", "value": "x"}])
        );

        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert!(ws.load_errors.is_empty(), "{:?}", ws.load_errors);
        assert_eq!(ws.collections[0], c);
        assert_eq!(ws.environment(&env.id), Some(&env));
        assert_eq!(ws.globals[0].ty, VarType::Boolean);
        let old = ws.environment("old").unwrap();
        assert_eq!(old.variables.len(), 2);
        assert!(old.variables.iter().all(|v| v.ty == VarType::Default));
        assert_eq!((old.variables[1].value.as_str(), old.variables[1].enabled), ("2", false));
    }

    #[test]
    fn secrets_never_reach_files() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let mut c = Collection::new("Secretive");
        c.auth = Auth::basic("user", "hunter2");
        let mut v = Variable::secret("apiKey", "sk-live-123");
        v.current = Some("sk-current-456".into());
        c.variables.push(v);
        let mut plain = Variable::new("plain", "initial");
        plain.current = Some("local-only".into());
        c.variables.push(plain);
        let mut r = HttpRequest::new("GET", "http://x");
        r.auth = Auth::bearer("tok-789");
        c.items.push(RequestItem::new("r", r).into());
        store.save_collection(&c, &secrets).unwrap();

        let text = all_text(&dir.path().join("collections"));
        for secret in ["hunter2", "sk-live-123", "sk-current-456", "tok-789", "local-only"] {
            assert!(!text.contains(secret), "{secret} leaked into collection files");
        }
        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws.collections[0], c);

        // Replacing a literal with a reference drops the stored secret.
        let mut c2 = c.clone();
        c2.auth = Auth::basic("user", "{{pw}}");
        store.save_collection(&c2, &secrets).unwrap();
        assert!(secrets.get(&auth_secret_key(&c.id, "password")).is_none());
    }

    #[test]
    fn rename_and_delete_prune_files() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let mut c = sample_collection();
        store.save_collection(&c, &secrets).unwrap();
        // Rename a request and remove another.
        let id = c.requests().iter().find(|r| r.name == "Get user").unwrap().id.clone();
        c.find_mut(&id).unwrap().set_name("Fetch user".into());
        let upload = c.requests().iter().find(|r| r.name == "Upload").unwrap().id.clone();
        c.remove(&upload);
        store.save_collection(&c, &secrets).unwrap();
        let base = dir.path().join("collections/sample-api");
        assert!(base.join("users/fetch-user.request.json").is_file());
        assert!(!base.join("users/get-user.request.json").exists());
        assert!(!base.join("upload.request.json").exists());
        // Rename the collection: directory moves.
        c.name = "Renamed".into();
        store.save_collection(&c, &secrets).unwrap();
        assert!(dir.path().join("collections/renamed/collection.json").is_file());
        assert!(!base.exists());
        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws.collections[0], c);
        store.delete_collection(&c.id, &secrets).unwrap();
        assert!(!dir.path().join("collections/renamed").exists());
    }

    #[test]
    fn duplicate_names_get_unique_files_and_hand_added_files_load() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let mut c = Collection::new("Dupes");
        c.items.push(RequestItem::new("Same", HttpRequest::new("GET", "http://a")).into());
        c.items.push(RequestItem::new("Same", HttpRequest::new("GET", "http://b")).into());
        store.save_collection(&c, &secrets).unwrap();
        let base = dir.path().join("collections/dupes");
        assert!(base.join("same.request.json").is_file());
        assert!(base.join("same-2.request.json").is_file());
        // A file added outside Merpati (e.g. via git) shows up at the end.
        let extra = RequestItem::new("Extra", HttpRequest::new("GET", "http://c"));
        std::fs::write(base.join("extra.request.json"), serde_json::to_vec(&extra).unwrap()).unwrap();
        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        let names: Vec<_> = ws.collections[0].items.iter().map(|i| i.name().to_string()).collect();
        assert_eq!(names, vec!["Same", "Same", "Extra"]);
    }

    #[test]
    fn example_auth_secrets_never_reach_files() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let mut c = Collection::new("Examples");
        let mut req = HttpRequest::new("GET", "http://x");
        req.auth = Auth::bearer("tok-request-1");
        let mut item = RequestItem::new("r", req.clone());
        // "Save as example" copies the request, auth included.
        item.examples.push(Example { request: Some(req), ..Default::default() });
        c.items.push(item.into());
        store.save_collection(&c, &secrets).unwrap();
        let text = all_text(&dir.path().join("collections"));
        assert!(!text.contains("tok-request-1"), "example auth leaked into collection files:\n{text}");
        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws.collections[0], c);
    }

    #[test]
    fn unreadable_files_survive_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let c = sample_collection();
        store.save_collection(&c, &secrets).unwrap();
        let base = dir.path().join("collections/sample-api");
        // A merge conflict leaves a request and a folder that do not parse.
        std::fs::write(base.join("upload.request.json"), "<<<<<<< HEAD\n{}\n").unwrap();
        std::fs::write(base.join("users/folder.json"), "<<<<<<< HEAD\n{}\n").unwrap();
        let (store, mut ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws.load_errors.len(), 2, "{:?}", ws.load_errors);
        // Saving the rest (with new items named like the unreadable ones)
        // must neither delete nor overwrite them.
        let c = &mut ws.collections[0];
        c.items.push(RequestItem::new("Upload", HttpRequest::new("PUT", "http://new")).into());
        c.items.push(Folder::new("Users").into());
        store.save_collection(c, &secrets).unwrap();
        assert!(std::fs::read_to_string(base.join("upload.request.json")).unwrap().starts_with("<<<<<<<"));
        assert!(std::fs::read_to_string(base.join("users/folder.json")).unwrap().starts_with("<<<<<<<"));
        assert!(base.join("users/list-users.request.json").is_file());
        let (_, ws2) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws2.collections[0].items.len(), ws.collections[0].items.len());
    }

    #[test]
    fn order_entries_cannot_point_outside_their_folder() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let mut c = Collection::new("Api");
        c.items.push(RequestItem::new("A", HttpRequest::new("GET", "http://a")).into());
        store.save_collection(&c, &secrets).unwrap();
        let outside = RequestItem::new("Outside", HttpRequest::new("GET", "http://outside"));
        std::fs::write(dir.path().join("outside.request.json"), serde_json::to_vec(&outside).unwrap()).unwrap();
        let meta_path = dir.path().join("collections/api/collection.json");
        let mut meta: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        let abs = dir.path().join("outside.request.json").to_string_lossy().to_string();
        meta["order"] = serde_json::json!(["a.request.json", "a.request.json", "../../outside.request.json", abs]);
        std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        let names: Vec<_> = ws.collections[0].items.iter().map(|i| i.name().to_string()).collect();
        assert_eq!(names, vec!["A"]);
    }

    #[test]
    fn self_referencing_order_entry_does_not_recurse() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let mut c = Collection::new("Api");
        let mut f = Folder::new("Users");
        f.items.push(RequestItem::new("List", HttpRequest::new("GET", "http://a")).into());
        c.items.push(f.into());
        store.save_collection(&c, &secrets).unwrap();
        let meta_path = dir.path().join("collections/api/users/folder.json");
        let mut meta: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        meta["order"] = serde_json::json!([".", "list.request.json"]);
        std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws.collections[0], c);
    }

    #[test]
    fn copied_files_with_the_same_id_keep_their_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let env = Environment::new("Staging").with_var("host", "staging");
        store.save_environment(&env, &secrets).unwrap();
        let c = sample_collection();
        store.save_collection(&c, &secrets).unwrap();
        // Duplicated by hand (or left behind by a merge): same ids, new names.
        let mut copy = env.clone();
        copy.name = "Staging copy".into();
        std::fs::write(dir.path().join("environments/staging-copy.json"), serde_json::to_vec(&copy).unwrap()).unwrap();
        let src = dir.path().join("collections/sample-api");
        let dst = dir.path().join("collections/sample-api-copy");
        fn copy_dir(from: &Path, to: &Path) {
            std::fs::create_dir_all(to).unwrap();
            for e in std::fs::read_dir(from).unwrap().flatten() {
                if e.path().is_dir() {
                    copy_dir(&e.path(), &to.join(e.file_name()));
                } else {
                    std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
                }
            }
        }
        copy_dir(&src, &dst);
        let meta_path = dst.join("collection.json");
        let mut meta: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        meta["name"] = serde_json::json!("Sample API copy");
        std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

        let (store, mut ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws.environments.len(), 2);
        assert_eq!(ws.collections.len(), 2);
        // Edit and save each one; every edit must end up in its own file.
        for e in &mut ws.environments {
            e.variables[0].value = format!("edited {}", e.name);
            store.save_environment(e, &secrets).unwrap();
        }
        for c in &mut ws.collections {
            c.description = format!("edited {}", c.name);
            store.save_collection(c, &secrets).unwrap();
        }
        let (_, ws2) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        let mut envs: Vec<_> =
            ws2.environments.iter().map(|e| (e.name.clone(), e.variables[0].value.clone())).collect();
        envs.sort();
        assert_eq!(
            envs,
            vec![
                ("Staging".to_string(), "edited Staging".to_string()),
                ("Staging copy".to_string(), "edited Staging copy".to_string())
            ]
        );
        let mut cols: Vec<_> =
            ws2.collections.iter().map(|c| (c.name.clone(), c.description.clone(), c.request_count())).collect();
        cols.sort();
        assert_eq!(
            cols,
            vec![
                ("Sample API".to_string(), "edited Sample API".to_string(), 5),
                ("Sample API copy".to_string(), "edited Sample API copy".to_string(), 5)
            ]
        );
        // The copy got its own ids, so the two never share item ids either.
        let ids = |c: &Collection| c.requests().iter().map(|r| r.id.clone()).collect::<HashSet<_>>();
        assert!(ids(&ws2.collections[0]).is_disjoint(&ids(&ws2.collections[1])));
        assert_ne!(ws2.environments[0].id, ws2.environments[1].id);
    }

    #[test]
    fn hand_made_names_differing_only_in_case_survive_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        store.save_collection(&Collection::new("Api"), &secrets).unwrap();
        // Added by hand (or by another tool) with capital letters; on a
        // case-insensitive file system the slugged names are the same files.
        let base = dir.path().join("collections/api");
        let users = RequestItem::new("Users", HttpRequest::new("GET", "http://u"));
        std::fs::write(base.join("Users.request.json"), serde_json::to_vec(&users).unwrap()).unwrap();
        let admin = Folder::new("Admin");
        let meta = serde_json::json!({"id": admin.id, "name": "Admin", "order": []});
        std::fs::create_dir_all(base.join("Admin")).unwrap();
        std::fs::write(base.join("Admin/folder.json"), meta.to_string()).unwrap();
        let delete = RequestItem::new("Delete", HttpRequest::new("DELETE", "http://d"));
        std::fs::write(base.join("Admin/Delete.request.json"), serde_json::to_vec(&delete).unwrap()).unwrap();
        let (store, mut ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws.collections[0].request_count(), 2, "{:?}", ws.load_errors);
        store.save_collection(&ws.collections[0], &secrets).unwrap();
        let (_, ws2) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert!(ws2.load_errors.is_empty(), "{:?}", ws2.load_errors);
        assert_eq!(ws2.collections[0], ws.collections[0]);
        // Renaming and removing later still removes the old files.
        let c = &mut ws.collections[0];
        c.find_mut(&users.id).unwrap().set_name("People".into());
        c.remove(&delete.id);
        store.save_collection(c, &secrets).unwrap();
        store.save_collection(c, &secrets).unwrap();
        let (_, ws3) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert!(ws3.load_errors.is_empty(), "{:?}", ws3.load_errors);
        assert_eq!(&ws3.collections[0], c);
    }

    #[test]
    fn hand_made_collection_and_environment_names_in_capitals_survive_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        WorkspaceStore::create(dir.path(), "Test").unwrap();
        let mut c = Collection::new("API");
        c.items.push(RequestItem::new("List", HttpRequest::new("GET", "http://l")).into());
        let meta = serde_json::json!({"id": c.id, "name": "API", "order": ["list.request.json"]});
        let base = dir.path().join("collections/API");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("collection.json"), meta.to_string()).unwrap();
        std::fs::write(base.join("list.request.json"), serde_json::to_vec(c.items[0].as_request().unwrap()).unwrap())
            .unwrap();
        let env = Environment::new("Staging").with_var("host", "h");
        std::fs::create_dir_all(dir.path().join("environments")).unwrap();
        std::fs::write(dir.path().join("environments/Staging.json"), serde_json::to_vec(&env).unwrap()).unwrap();
        let (store, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!((ws.collections.len(), ws.environments.len()), (1, 1), "{:?}", ws.load_errors);
        store.save_collection(&ws.collections[0], &secrets).unwrap();
        store.save_environment(&ws.environments[0], &secrets).unwrap();
        let (_, ws2) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws2.collections, vec![c]);
        assert_eq!(ws2.environments, vec![env]);
    }

    #[test]
    fn duplicate_variable_names_keep_their_own_values() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        // A common pattern: the same name twice, one of them disabled.
        let mut env = Environment::new("Env");
        let mut dev = Variable::secret("token", "dev-secret");
        dev.enabled = false;
        dev.current = Some("dev-current".into());
        let mut prod = Variable::secret("token", "prod-secret");
        prod.current = Some("prod-current".into());
        env.variables.extend([dev, prod]);
        let mut a = Variable::new("host", "a.test");
        a.current = Some("a.local".into());
        a.enabled = false;
        let mut b = Variable::new("host", "b.test");
        b.current = Some("b.local".into());
        env.variables.extend([a, b]);
        store.save_environment(&env, &secrets).unwrap();
        let text = all_text(dir.path());
        assert!(!text.contains("dev-secret") && !text.contains("prod-secret"));
        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws.environments[0], env);
    }

    #[test]
    fn windows_reserved_names_are_avoided() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let mut c = Collection::new("AUX");
        let mut f = Folder::new("Con");
        f.items.push(RequestItem::new("NUL", HttpRequest::new("GET", "http://a")).into());
        c.items.push(f.into());
        c.items.push(RequestItem::new("com1", HttpRequest::new("GET", "http://b")).into());
        store.save_collection(&c, &secrets).unwrap();
        store.save_environment(&Environment::new("prn"), &secrets).unwrap();
        fn names(p: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(p).unwrap().flatten() {
                out.push(e.file_name().to_string_lossy().to_string());
                if e.path().is_dir() {
                    names(&e.path(), out);
                }
            }
        }
        let mut all = Vec::new();
        names(dir.path(), &mut all);
        for n in &all {
            let stem = n.split('.').next().unwrap();
            assert!(!["aux", "con", "nul", "com1", "prn"].contains(&stem), "reserved name {n} in {all:?}");
        }
        let (_, ws) = WorkspaceStore::open(dir.path(), &secrets).unwrap();
        assert_eq!(ws.collections[0], c);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_through_a_symlink_keeps_the_link() {
        let dir = tempfile::tempdir().unwrap();
        // Settings kept in a dotfiles repository and linked into place.
        let real = dir.path().join("dotfiles/settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "old").unwrap();
        let link = dir.path().join("config/settings.json");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
    }

    #[test]
    fn unchanged_files_are_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = SecretStore::memory();
        let (store, _) = WorkspaceStore::create(dir.path(), "Test").unwrap();
        let c = sample_collection();
        store.save_collection(&c, &secrets).unwrap();
        let p = dir.path().join("collections/sample-api/upload.request.json");
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        store.save_collection(&c, &secrets).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), before);
    }
}
