//! Loading what the commands operate on: Postman/OpenAPI/... files, Merpati
//! workspaces, or a collection folder inside a workspace.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use merpati_core::environment::Environment;
use merpati_core::model::Collection;
use merpati_core::secrets::{FileBackend, KeychainBackend, SecretBackend, SecretStore};
use merpati_core::storage::{LOCAL_DIR, Workspace, WorkspaceStore};
use merpati_core::variables::{self, Variable, VariableScopes};

/// An opened workspace with its secret store.
pub struct WsCtx {
    pub store: WorkspaceStore,
    pub ws: Workspace,
    pub secrets: Arc<SecretStore>,
}

pub struct Loaded {
    pub collection: Collection,
    pub ws: Option<WsCtx>,
    /// Base directory for relative file paths in request bodies.
    pub base_dir: PathBuf,
    pub warnings: Vec<String>,
}

/// Keychain use can be disabled for CI or tests.
pub fn keychain_allowed(no_keychain: bool) -> bool {
    !no_keychain && std::env::var_os("MERPATI_NO_KEYCHAIN").is_none()
}

/// The secret store of a workspace: `.merpati/secrets.json` when it exists
/// (or the keychain is disabled), otherwise the OS keychain.
pub fn secret_store(root: &Path, workspace_id: &str, keychain: bool) -> SecretStore {
    let file = root.join(LOCAL_DIR).join("secrets.json");
    let backend: Box<dyn SecretBackend> = if file.exists() || !keychain || !KeychainBackend::available() {
        Box::new(FileBackend { path: file })
    } else {
        Box::new(KeychainBackend::for_workspace(workspace_id))
    };
    SecretStore::new(backend)
}

pub fn open_workspace(root: &Path, keychain: bool) -> Result<WsCtx> {
    let manifest = WorkspaceStore::read_manifest(root)?;
    let secrets = Arc::new(secret_store(root, &manifest.id, keychain));
    let (store, ws) =
        WorkspaceStore::open(root, &secrets).with_context(|| format!("could not open workspace {}", root.display()))?;
    Ok(WsCtx { store, ws, secrets })
}

/// Walks up from `dir` to the directory holding `merpati.json`.
pub fn find_workspace_root(dir: &Path) -> Option<PathBuf> {
    let dir = dir.canonicalize().ok()?;
    dir.ancestors().find(|d| WorkspaceStore::is_workspace(d)).map(Path::to_path_buf)
}

fn matches_name(c: &Collection, sel: &str) -> bool {
    c.id == sel || c.name.eq_ignore_ascii_case(sel) || merpati_core::util::slugify(&c.name) == sel
}

pub fn pick_collection(collections: &[Collection], sel: Option<&str>, origin: &str) -> Result<Collection> {
    match sel {
        Some(s) => collections
            .iter()
            .find(|c| matches_name(c, s))
            .cloned()
            .ok_or_else(|| anyhow!("collection \"{s}\" not found in {origin}; available: {}", names(collections))),
        None => match collections {
            [] => bail!("{origin} contains no collection"),
            [one] => Ok(one.clone()),
            many => bail!("{origin} has {} collections; choose one with -c/--collection: {}", many.len(), names(many)),
        },
    }
}

fn names(cs: &[Collection]) -> String {
    cs.iter().map(|c| format!("\"{}\"", c.name)).collect::<Vec<_>>().join(", ")
}

/// Loads a collection from a file, a workspace or a collection folder.
pub fn load(path: &Path, sel: Option<&str>, keychain: bool) -> Result<Loaded> {
    if path.is_file() {
        let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
        let imported =
            merpati_convert::import_auto(&text).with_context(|| format!("could not load {}", path.display()))?;
        let mut warnings = imported.warnings;
        let collection = match (sel, imported.collections.len()) {
            (None, n) if n > 1 => {
                warnings.push(format!("{} contains {n} collections; using the first", path.display()));
                imported.collections[0].clone()
            }
            _ => pick_collection(&imported.collections, sel, &path.display().to_string())?,
        };
        let base_dir =
            path.parent().map(Path::to_path_buf).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| ".".into());
        return Ok(Loaded { collection, ws: None, base_dir, warnings });
    }
    if path.is_dir() {
        if WorkspaceStore::is_workspace(path) {
            let ctx = open_workspace(path, keychain)?;
            let collection = pick_collection(&ctx.ws.collections, sel, &format!("workspace {}", path.display()))?;
            let warnings = ctx.ws.load_errors.clone();
            return Ok(Loaded { collection, base_dir: path.to_path_buf(), warnings, ws: Some(ctx) });
        }
        if let Some(root) = find_workspace_root(path) {
            let ctx = open_workspace(&root, keychain)?;
            let target = path.canonicalize()?;
            let found = ctx.ws.collections.iter().find(|c| {
                ctx.store
                    .collection_dir(&c.id)
                    .and_then(|d| d.canonicalize().ok())
                    .map(|d| target.starts_with(d))
                    .unwrap_or(false)
            });
            let collection = match (found, sel) {
                (_, Some(s)) => {
                    pick_collection(&ctx.ws.collections, Some(s), &format!("workspace {}", root.display()))?
                }
                (Some(c), None) => c.clone(),
                (None, None) => pick_collection(&ctx.ws.collections, None, &format!("workspace {}", root.display()))?,
            };
            let warnings = ctx.ws.load_errors.clone();
            return Ok(Loaded { collection, base_dir: root, warnings, ws: Some(ctx) });
        }
        bail!("{} is not a Merpati workspace (no merpati.json found)", path.display());
    }
    bail!("{} does not exist", path.display())
}

/// `-e` accepts a Postman environment file or the name/id of a workspace
/// environment.
pub fn load_environment(spec: &str, ws: Option<&WsCtx>) -> Result<Environment> {
    let p = Path::new(spec);
    if p.is_file() {
        let text = std::fs::read_to_string(p).with_context(|| format!("could not read {spec}"))?;
        let imported =
            merpati_convert::import_auto(&text).with_context(|| format!("could not load environment {spec}"))?;
        return imported.environments.into_iter().next().ok_or_else(|| anyhow!("{spec} is not an environment file"));
    }
    if let Some(ctx) = ws {
        if let Some(e) = ctx.ws.environments.iter().find(|e| e.id == spec || e.name.eq_ignore_ascii_case(spec)) {
            return Ok(e.clone());
        }
        let available: Vec<String> = ctx.ws.environments.iter().map(|e| format!("\"{}\"", e.name)).collect();
        bail!("environment \"{spec}\" not found; available: {}", available.join(", "));
    }
    bail!("environment file {spec} does not exist")
}

pub fn load_globals(path: &Path) -> Result<Vec<Variable>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
    Ok(merpati_core::postman::import_globals(&v)?)
}

/// The environment to use: `-e` if given, else the workspace's active one.
pub fn resolve_environment(spec: Option<&str>, ws: Option<&WsCtx>) -> Result<Option<Environment>> {
    match spec {
        Some(s) => load_environment(s, ws).map(Some),
        None => Ok(ws.and_then(|c| c.ws.active_environment().cloned())),
    }
}

pub fn scopes(
    collection: &Collection,
    env: Option<&Environment>,
    globals: &[Variable],
    env_overrides: &[(String, String)],
    global_overrides: &[(String, String)],
) -> VariableScopes {
    let mut s = VariableScopes {
        globals: variables::to_map(globals),
        collection: variables::to_map(&collection.variables),
        ..Default::default()
    };
    // Declared Postman types (`number`, `boolean`, ...) reach scripts typed.
    s.types.globals = variables::to_types(globals);
    s.types.collection = variables::to_types(&collection.variables);
    if let Some(e) = env {
        s.environment = e.to_map();
        s.types.environment = e.to_types();
        s.environment_name = Some(e.name.clone());
    }
    for (k, v) in env_overrides {
        s.environment.insert(k.clone(), v.clone());
    }
    for (k, v) in global_overrides {
        s.globals.insert(k.clone(), v.clone());
    }
    s
}
