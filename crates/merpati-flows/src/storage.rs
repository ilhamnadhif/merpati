//! Flow files: `<workspace>/flows/<slug>.flow.json`, one pretty-printed JSON
//! file per flow, written atomically.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::Flow;

pub const FLOWS_DIR: &str = "flows";
pub const FLOW_SUFFIX: &str = ".flow.json";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid flow file {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0}")]
    NotFound(String),
}

fn io(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io { path: path.to_path_buf(), source }
}

/// A flow with the file it was loaded from.
#[derive(Clone, Debug, PartialEq)]
pub struct FlowFile {
    pub path: PathBuf,
    pub flow: Flow,
}

pub fn flows_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(FLOWS_DIR)
}

pub fn is_flow_file(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).map(|n| n.ends_with(FLOW_SUFFIX)).unwrap_or(false)
}

/// Pretty JSON with a trailing newline, keys in a fixed order.
pub fn to_json(flow: &Flow) -> String {
    let mut s = serde_json::to_string_pretty(flow).expect("flows serialize");
    s.push('\n');
    s
}

pub fn from_json(text: &str) -> Result<Flow, serde_json::Error> {
    serde_json::from_str(text)
}

/// The `id` a flow file declares; `Some(None)` for a JSON object without one.
fn declared_id(text: &str) -> Option<Option<String>> {
    #[derive(Deserialize)]
    struct IdOnly {
        #[serde(default)]
        id: Option<String>,
    }
    serde_json::from_str::<IdOnly>(text).ok().map(|f| f.id)
}

/// The id of a (hand-written) file without one: its file name, so that
/// every load gives the same flow and saving it finds the file again.
fn implicit_id(path: &Path) -> Option<String> {
    Some(stem(path)).filter(|s| !s.is_empty())
}

pub fn load(path: &Path) -> Result<Flow, StoreError> {
    let text = std::fs::read_to_string(path).map_err(|e| io(path, e))?;
    let mut flow = from_json(&text).map_err(|source| StoreError::Json { path: path.to_path_buf(), source })?;
    if declared_id(&text) == Some(None)
        && let Some(id) = implicit_id(path)
    {
        flow.id = id;
    }
    Ok(flow)
}

fn flow_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| is_flow_file(p)).collect();
    paths.sort();
    paths
}

/// Every flow of a workspace sorted by file name, plus the files that could
/// not be read.
pub fn list(workspace_root: &Path) -> (Vec<FlowFile>, Vec<String>) {
    let mut flows = Vec::new();
    let mut errors = Vec::new();
    for path in flow_paths(&flows_dir(workspace_root)) {
        match load(&path) {
            Ok(flow) => flows.push(FlowFile { path, flow }),
            Err(e) => errors.push(e.to_string()),
        }
    }
    (flows, errors)
}

fn file_id(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    declared_id(&text)?.or_else(|| implicit_id(path))
}

/// The file currently holding the flow with `id`.
pub fn path_of(workspace_root: &Path, id: &str) -> Option<PathBuf> {
    flow_paths(&flows_dir(workspace_root)).into_iter().find(|p| file_id(p).as_deref() == Some(id))
}

fn slug_for(name: &str) -> String {
    let s = merpati_core::util::slugify(name);
    if s.is_empty() { "flow".to_string() } else { s }
}

fn stem(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(FLOW_SUFFIX)).unwrap_or_default().to_string()
}

/// `slug` or `slug-<n>`.
fn stem_matches(stem: &str, slug: &str) -> bool {
    stem == slug
        || stem
            .strip_prefix(slug)
            .and_then(|r| r.strip_prefix('-'))
            .map(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false)
}

/// Saves a flow and returns its file. The file name follows the flow name;
/// when the name changed the old file is replaced by the new one. Another
/// flow's file is never overwritten (`name-2.flow.json` is used instead).
pub fn save(workspace_root: &Path, flow: &Flow) -> Result<PathBuf, StoreError> {
    let dir = flows_dir(workspace_root);
    std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
    let existing = path_of(workspace_root, &flow.id);
    let slug = slug_for(&flow.name);
    let target = match &existing {
        Some(p) if stem_matches(&stem(p), &slug) => p.clone(),
        _ => {
            let mut n = 1;
            loop {
                let name = if n == 1 { format!("{slug}{FLOW_SUFFIX}") } else { format!("{slug}-{n}{FLOW_SUFFIX}") };
                let candidate = dir.join(name);
                if !candidate.exists() || file_id(&candidate).as_deref() == Some(flow.id.as_str()) {
                    break candidate;
                }
                n += 1;
            }
        }
    };
    merpati_core::storage::write_atomic(&target, to_json(flow).as_bytes()).map_err(|e| io(&target, e))?;
    if let Some(old) = existing
        && old != target
    {
        std::fs::remove_file(&old).map_err(|e| io(&old, e))?;
    }
    Ok(target)
}

/// Deletes the flow's file. Returns false when no file had that id.
pub fn delete(workspace_root: &Path, id: &str) -> Result<bool, StoreError> {
    match path_of(workspace_root, id) {
        Some(p) => {
            std::fs::remove_file(&p).map_err(|e| io(&p, e))?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Renames a flow (and its file) and returns the new path.
pub fn rename(workspace_root: &Path, id: &str, new_name: &str) -> Result<PathBuf, StoreError> {
    let path = path_of(workspace_root, id).ok_or_else(|| StoreError::NotFound(format!("flow {id} not found")))?;
    let mut flow = load(&path)?;
    flow.name = new_name.to_string();
    save(workspace_root, &flow)
}

/// Finds a flow by path (a `.json` file), id, name (case-insensitive) or
/// file slug.
pub fn find(workspace_root: &Path, selector: &str) -> Result<FlowFile, StoreError> {
    let as_path = Path::new(selector);
    if as_path.is_file() {
        return Ok(FlowFile { path: as_path.to_path_buf(), flow: load(as_path)? });
    }
    let rel = workspace_root.join(selector);
    if rel.is_file() {
        return Ok(FlowFile { flow: load(&rel)?, path: rel });
    }
    let (flows, errors) = list(workspace_root);
    let slug = merpati_core::util::slugify(selector);
    let found = flows
        .iter()
        .find(|f| f.flow.id == selector)
        .or_else(|| flows.iter().find(|f| f.flow.name == selector))
        .or_else(|| flows.iter().find(|f| f.flow.name.eq_ignore_ascii_case(selector)))
        .or_else(|| flows.iter().find(|f| stem(&f.path) == selector || stem(&f.path) == slug));
    match found {
        Some(f) => Ok(f.clone()),
        None => {
            let names: Vec<String> = flows.iter().map(|f| format!("\"{}\"", f.flow.name)).collect();
            let mut msg = format!("flow \"{selector}\" not found in {}", flows_dir(workspace_root).display());
            if names.is_empty() {
                msg.push_str(" (no flows)");
            } else {
                msg.push_str(&format!("; available: {}", names.join(", ")));
            }
            if !errors.is_empty() {
                msg.push_str(&format!("; unreadable: {}", errors.join("; ")));
            }
            Err(StoreError::NotFound(msg))
        }
    }
}
