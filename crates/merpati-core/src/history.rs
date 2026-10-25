//! Request history, stored as JSON lines in the workspace's local folder and
//! capped to a configurable number of entries.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{HttpRequest, Id};
use crate::util::new_id;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(default = "new_id")]
    pub id: Id,
    /// Unix milliseconds.
    pub timestamp: i64,
    /// The request as edited (with `{{variables}}`), restorable into a tab.
    pub request: HttpRequest,
    /// The URL that was actually sent.
    #[serde(default)]
    pub resolved_url: String,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub elapsed_ms: f64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub error: Option<String>,
    /// Saved request it came from, if any.
    #[serde(default)]
    pub source_request_id: Option<Id>,
    #[serde(default)]
    pub name: Option<String>,
}

impl HistoryEntry {
    pub fn matches(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let q = query.to_lowercase();
        self.request.url.to_lowercase().contains(&q)
            || self.resolved_url.to_lowercase().contains(&q)
            || self.request.method.to_lowercase() == q
            || self.name.as_deref().map(|n| n.to_lowercase().contains(&q)).unwrap_or(false)
            || self.status.map(|s| s.to_string() == q).unwrap_or(false)
    }
}

pub struct HistoryStore {
    path: PathBuf,
    limit: usize,
}

impl HistoryStore {
    pub fn new(path: impl Into<PathBuf>, limit: usize) -> Self {
        HistoryStore { path: path.into(), limit: limit.max(1) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the newest `limit` entries, newest first. Corrupt lines are skipped.
    pub fn load(&self) -> Vec<HistoryEntry> {
        let Ok(file) = std::fs::File::open(&self.path) else { return Vec::new() };
        // Split on bytes: a line that is not UTF-8 (cut off by a crash) is
        // skipped like any other corrupt line instead of ending the read.
        let mut entries: Vec<HistoryEntry> = std::io::BufReader::new(file)
            .split(b'\n')
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_slice(&l).ok())
            .collect();
        if entries.len() > self.limit {
            entries.drain(..entries.len() - self.limit);
        }
        entries.reverse();
        entries
    }

    pub fn append(&self, entry: &HistoryEntry) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        f.write_all(&to_line(entry)?)?;
        drop(f);
        // Compact when the file grows well past the limit.
        let len = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if len > 64 * 1024 && self.line_count() > self.limit * 2 {
            self.compact()?;
        }
        Ok(())
    }

    fn line_count(&self) -> usize {
        std::fs::File::open(&self.path).map(|f| std::io::BufReader::new(f).lines().count()).unwrap_or(0)
    }

    fn compact(&self) -> std::io::Result<()> {
        let mut entries = self.load();
        entries.reverse();
        let mut out = Vec::new();
        for e in &entries {
            out.extend_from_slice(&to_line(e)?);
        }
        crate::storage::write_atomic(&self.path, &out)
    }

    pub fn remove(&self, id: &str) -> std::io::Result<()> {
        let mut entries = self.load();
        entries.retain(|e| e.id != id);
        entries.reverse();
        let mut out = Vec::new();
        for e in &entries {
            out.extend_from_slice(&to_line(e)?);
        }
        crate::storage::write_atomic(&self.path, &out)
    }

    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// One JSON line. Literal values of sensitive auth parameters are blanked
/// (FR-A4): the history file is plain text, and the request can still be
/// restored, with the credential typed again or taken from a variable.
fn to_line(entry: &HistoryEntry) -> std::io::Result<Vec<u8>> {
    let mut entry = entry.clone();
    entry.request.auth.scrub_secrets();
    let mut line = serde_json::to_vec(&entry).map_err(std::io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str) -> HistoryEntry {
        HistoryEntry {
            id: new_id(),
            timestamp: 0,
            request: HttpRequest::new("GET", url),
            resolved_url: url.into(),
            status: Some(200),
            elapsed_ms: 1.0,
            size: 1,
            error: None,
            source_request_id: None,
            name: None,
        }
    }

    #[test]
    fn capped_and_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path().join("h.jsonl"), 3);
        for i in 0..5 {
            store.append(&entry(&format!("http://h/{i}"))).unwrap();
        }
        let all = store.load();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].request.url, "http://h/4");
        assert!(all[0].matches("h/4"));
        assert!(!all[0].matches("zzz"));
        store.remove(&all[1].id).unwrap();
        assert_eq!(store.load().len(), 2);
        store.clear().unwrap();
        assert!(store.load().is_empty());
    }

    #[test]
    fn a_broken_line_does_not_hide_newer_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.jsonl");
        let store = HistoryStore::new(&path, 10);
        store.append(&entry("http://h/old")).unwrap();
        // A crash mid-write can cut a multi-byte character in half.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":\"x\",\"url\":\"caf\xc3").unwrap();
        f.write_all(b"\n").unwrap();
        drop(f);
        store.append(&entry("http://h/new")).unwrap();
        let urls: Vec<_> = store.load().into_iter().map(|e| e.request.url).collect();
        assert_eq!(urls, vec!["http://h/new", "http://h/old"]);
    }

    #[test]
    fn auth_secrets_are_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.jsonl");
        let store = HistoryStore::new(&path, 10);
        let mut e = entry("http://h/1");
        e.request.auth = crate::auth::Auth::basic("bob", "hunter2-pw");
        store.append(&e).unwrap();
        e.request.auth = crate::auth::Auth::bearer("{{token}}");
        store.append(&e).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("hunter2-pw"), "{text}");
        let loaded = store.load();
        assert_eq!(loaded[0].request.auth.get("token"), "{{token}}");
        assert_eq!(loaded[1].request.auth.get("username"), "bob");
    }
}
