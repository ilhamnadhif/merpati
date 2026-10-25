//! Module cache and lockfile: fetched modules are stored per URL in the
//! host's cache directory and pinned by SHA-256 in `packages.lock.json`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use merpati_core::script::PackageContext;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::fetch::{self, Limits};
use super::spec::Registry;

/// Serialises lockfile updates of concurrent jobs in this process.
static LOCK_WRITE: Mutex<()> = Mutex::new(());

pub(crate) const LOCKFILE_VERSION: u32 = 1;

/// Engine-wide limits for packages.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PackageLimits {
    pub module_bytes: u64,
    pub graph_bytes: u64,
    pub fetch_timeout: Duration,
}

/// `packages.lock.json`: sorted maps, so diffs stay small.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Lockfile {
    #[serde(rename = "lockfileVersion", default)]
    pub version: u32,
    /// Specifier required by a script -> entry URL.
    #[serde(default)]
    pub packages: BTreeMap<String, String>,
    /// Module URL -> `sha256-<base64>` of its content.
    #[serde(default)]
    pub modules: BTreeMap<String, String>,
}

/// `5 MB`, `64 KB`.
pub(crate) fn size_text(n: u64) -> String {
    if n >= 1024 * 1024 && n.is_multiple_of(1024 * 1024) {
        format!("{} MB", n / (1024 * 1024))
    } else {
        format!("{} KB", n / 1024)
    }
}

pub(crate) fn integrity(bytes: &[u8]) -> String {
    format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)))
}

#[derive(Serialize, Deserialize)]
struct CacheMeta {
    url: String,
    final_url: String,
    integrity: String,
}

/// A module's source, ready to parse.
pub(crate) struct Obtained {
    /// URL after redirects; relative imports resolve against it.
    pub final_url: String,
    pub source: String,
    pub integrity: String,
}

fn read_lock(path: &Path) -> Result<Lockfile, String> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{} is not a valid lockfile: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Lockfile::default()),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{seq}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn cache_key(url: &str) -> String {
    hex::encode(Sha256::digest(url.as_bytes()))
}

/// Package state of one job.
pub(crate) struct Session {
    pub ctx: Arc<PackageContext>,
    pub registry: Result<Registry, String>,
    pub limits: PackageLimits,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    lock: Option<Lockfile>,
    lock_loaded: bool,
    /// Pins and specifiers added by this job.
    added: Lockfile,
    /// Module URLs whose pins were checked in this job.
    pub verified: HashSet<String>,
    /// Bytes loaded by the current `pm.require`.
    pub graph_bytes: u64,
    /// Pins added by the current `pm.require`, dropped if it fails.
    graph_pins: Vec<String>,
}

impl Session {
    pub fn new(ctx: Arc<PackageContext>, limits: PackageLimits, deadline: Instant, cancel: Arc<AtomicBool>) -> Self {
        let registry = Registry::parse(&ctx.registry_base);
        Session {
            ctx,
            registry,
            limits,
            deadline,
            cancel,
            lock: None,
            lock_loaded: false,
            added: Lockfile::default(),
            verified: HashSet::new(),
            graph_bytes: 0,
            graph_pins: Vec::new(),
        }
    }

    pub fn registry(&self) -> Result<&Registry, String> {
        self.registry.as_ref().map_err(Clone::clone)
    }

    fn lock(&mut self) -> Result<Option<&Lockfile>, String> {
        if !self.lock_loaded {
            self.lock = match &self.ctx.lockfile_path {
                Some(p) => Some(read_lock(p)?),
                None => None,
            };
            self.lock_loaded = true;
        }
        Ok(self.lock.as_ref())
    }

    fn pin(&mut self, url: &str) -> Result<Option<String>, String> {
        if let Some(p) = self.added.modules.get(url) {
            return Ok(Some(p.clone()));
        }
        Ok(self.lock()?.and_then(|l| l.modules.get(url).cloned()))
    }

    fn lockfile_name(&self) -> String {
        self.ctx
            .lockfile_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "packages.lock.json".into())
    }

    /// Checks `integrity` against the lockfile, pinning it when new.
    pub fn check_pin(&mut self, url: &str, integrity: &str) -> Result<(), String> {
        if self.ctx.lockfile_path.is_none() {
            return Ok(());
        }
        match self.pin(url)? {
            Some(p) if p == integrity => Ok(()),
            Some(p) => Err(self.mismatch(url, &p, integrity)),
            None => {
                self.added.modules.insert(url.to_string(), integrity.to_string());
                self.graph_pins.push(url.to_string());
                Ok(())
            }
        }
    }

    fn mismatch(&self, url: &str, pinned: &str, got: &str) -> String {
        format!(
            "integrity mismatch for {url}: {} pins {pinned} but the content has {got}. \
             If the package was changed on purpose, remove its entries from {} to accept the new content.",
            self.lockfile_name(),
            self.lockfile_name()
        )
    }

    /// Starts loading a module graph (one `pm.require`); returns the state
    /// of an enclosing load to hand back to [`Session::end_graph`].
    pub fn begin_graph(&mut self) -> (u64, Vec<String>) {
        (std::mem::take(&mut self.graph_bytes), std::mem::take(&mut self.graph_pins))
    }

    /// Ends a graph load. The pins of a failed load are forgotten, so a
    /// broken module is not locked in; sizes count towards the enclosing load.
    pub fn end_graph(&mut self, outer: (u64, Vec<String>), failed: bool) {
        if failed {
            for url in std::mem::take(&mut self.graph_pins) {
                self.added.modules.remove(&url);
                self.verified.remove(&url);
            }
        }
        self.graph_bytes += outer.0;
        self.graph_pins = outer.1;
    }

    /// Removes a module from the cache (it failed to parse).
    pub fn evict(&mut self, url: &str) {
        if let Some(dir) = &self.ctx.cache_dir {
            let key = cache_key(url);
            let _ = std::fs::remove_file(dir.join(format!("{key}.mjs")));
            let _ = std::fs::remove_file(dir.join(format!("{key}.json")));
        }
    }

    /// Records which entry URL a specifier resolved to.
    pub fn note_package(&mut self, spec: &str, url: &str) {
        if self.ctx.lockfile_path.is_none() {
            return;
        }
        let known = self.lock().ok().flatten().and_then(|l| l.packages.get(spec)).is_some_and(|u| u == url);
        if !known {
            self.added.packages.insert(spec.to_string(), url.to_string());
        }
    }

    fn account(&mut self, url: &str, len: usize) -> Result<(), String> {
        self.graph_bytes += len as u64;
        if self.graph_bytes > self.limits.graph_bytes {
            return Err(format!(
                "package is too large: loading {url} brings its modules over the {} limit",
                size_text(self.limits.graph_bytes)
            ));
        }
        Ok(())
    }

    fn read_cache(&self, url: &str) -> Option<Obtained> {
        let dir = self.ctx.cache_dir.as_ref()?;
        let key = cache_key(url);
        let meta: CacheMeta = serde_json::from_slice(&std::fs::read(dir.join(format!("{key}.json"))).ok()?).ok()?;
        let bytes = std::fs::read(dir.join(format!("{key}.mjs"))).ok()?;
        if meta.url != url || bytes.len() as u64 > self.limits.module_bytes || integrity(&bytes) != meta.integrity {
            return None;
        }
        let source = String::from_utf8(bytes).ok()?;
        Some(Obtained { final_url: meta.final_url, source, integrity: meta.integrity })
    }

    fn write_cache(&self, url: &str, got: &Obtained) {
        let Some(dir) = self.ctx.cache_dir.as_ref() else { return };
        let key = cache_key(url);
        let meta =
            CacheMeta { url: url.to_string(), final_url: got.final_url.clone(), integrity: got.integrity.clone() };
        let res = write_atomic(&dir.join(format!("{key}.mjs")), got.source.as_bytes()).and_then(|_| {
            write_atomic(&dir.join(format!("{key}.json")), serde_json::to_string(&meta).unwrap_or_default().as_bytes())
        });
        if let Err(e) = res {
            tracing::warn!("could not cache package module {url}: {e}");
        }
    }

    fn cache_dir_text(&self) -> String {
        self.ctx.cache_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_else(|| "none configured".into())
    }

    /// Gets a module from the cache or the registry, verified against the
    /// lockfile and counted towards the graph limit.
    pub fn obtain(&mut self, url: &str) -> Result<Obtained, String> {
        let pin = if self.ctx.lockfile_path.is_some() { self.pin(url)? } else { None };
        let cached = self.read_cache(url);
        if let Some(c) = cached {
            if pin.as_ref().is_none_or(|p| *p == c.integrity) {
                self.check_pin(url, &c.integrity)?;
                self.account(url, c.source.len())?;
                self.verified.insert(url.to_string());
                return Ok(c);
            }
            if !self.ctx.allow_network {
                return Err(self.mismatch(url, pin.as_deref().unwrap_or_default(), &c.integrity));
            }
        } else if !self.ctx.allow_network {
            return Err(format!(
                "{url} is not in the package cache ({}) and network access is off; \
                 run the script once with network access to download it",
                self.cache_dir_text()
            ));
        }
        let registry = self.registry()?.clone();
        let limits =
            Limits { max_bytes: self.limits.module_bytes, timeout: self.limits.fetch_timeout, deadline: self.deadline };
        let fetched = fetch::fetch(&registry, url, &limits, &self.cancel)?;
        let got = Obtained {
            integrity: integrity(&fetched.bytes),
            final_url: fetched.url,
            source: String::from_utf8(fetched.bytes).map_err(|_| format!("{url} is not UTF-8 JavaScript"))?,
        };
        if let Some(p) = &pin
            && *p != got.integrity
        {
            return Err(self.mismatch(url, p, &got.integrity));
        }
        self.write_cache(url, &got);
        self.check_pin(url, &got.integrity)?;
        self.account(url, got.source.len())?;
        self.verified.insert(url.to_string());
        Ok(got)
    }

    /// Writes new pins to the lockfile (merged with what is on disk).
    pub fn flush(&mut self) -> Result<(), String> {
        let Some(path) = self.ctx.lockfile_path.clone() else { return Ok(()) };
        if self.added.modules.is_empty() && self.added.packages.is_empty() {
            return Ok(());
        }
        let _guard = LOCK_WRITE.lock().unwrap_or_else(|e| e.into_inner());
        let mut lock = read_lock(&path)?;
        let before = lock.clone();
        lock.version = LOCKFILE_VERSION;
        for (k, v) in std::mem::take(&mut self.added.modules) {
            lock.modules.entry(k).or_insert(v);
        }
        for (k, v) in std::mem::take(&mut self.added.packages) {
            lock.packages.insert(k, v);
        }
        if lock != before {
            let mut text = serde_json::to_string_pretty(&lock).map_err(|e| e.to_string())?;
            text.push('\n');
            write_atomic(&path, text.as_bytes()).map_err(|e| format!("could not write {}: {e}", path.display()))?;
        }
        self.lock = Some(lock);
        Ok(())
    }

    pub fn library_dir(&self) -> Option<&PathBuf> {
        self.ctx.library_dir.as_ref()
    }
}
