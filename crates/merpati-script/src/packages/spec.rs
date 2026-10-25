//! Package specifiers: `npm:`/`jsr:` to CDN URLs, local Package Library
//! names to files, and imports inside fetched modules.

use std::path::{Path, PathBuf};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use url::Url;

/// Query added to every package entry URL (esm.sh: one bundled ES module).
pub(crate) const ENTRY_QUERY: &str = "bundle&target=es2022";

/// Node.js built-ins whose esm.sh polyfills cannot work in the sandbox (file
/// system, sockets, processes, threads, code loading). Harmless stubs such
/// as `tty`, `os` or `perf_hooks` (pulled in by the `process` polyfill) are
/// allowed.
const UNSUPPORTED_NODE: &[&str] = &[
    "fs",
    "fs/promises",
    "child_process",
    "net",
    "tls",
    "dgram",
    "dns",
    "dns/promises",
    "http",
    "https",
    "http2",
    "cluster",
    "worker_threads",
    "inspector",
    "vm",
    "repl",
    "readline",
];

/// Characters kept as-is in version ranges; everything else is escaped.
const VERSION_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'.').remove(b'-').remove(b'_').remove(b'+');

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalKind {
    Esm,
    Cjs,
}

/// The configured CDN.
#[derive(Clone, Debug)]
pub(crate) struct Registry {
    /// Base URL without a trailing slash.
    pub base: String,
    url: Url,
}

impl Registry {
    pub fn parse(base: &str) -> Result<Registry, String> {
        let trimmed = base.trim().trim_end_matches('/');
        let url = Url::parse(trimmed).map_err(|e| format!("invalid package registry URL '{base}': {e}"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(format!("invalid package registry URL '{base}': expected http(s)://host"));
        }
        Ok(Registry { base: trimmed.to_string(), url })
    }

    /// Whether `url` is served by the registry (same scheme, host and port).
    pub fn owns(&self, url: &Url) -> bool {
        url.origin() == self.url.origin()
    }

    pub fn host(&self) -> String {
        self.url.host_str().unwrap_or_default().to_string()
    }
}

fn valid_name_part(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'))
}

fn valid_subpath(s: &str) -> bool {
    s.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '@' | '+'))
    })
}

/// Splits `@scope/name@version/sub/path` (scope optional).
fn split_package(rest: &str) -> Option<(&str, Option<&str>, Option<&str>)> {
    let (scope_len, after_scope) = match rest.strip_prefix('@') {
        Some(r) => {
            let slash = r.find('/')?;
            (slash + 2, &r[slash + 1..])
        }
        None => (0, rest),
    };
    let name_end = after_scope.find(['@', '/']).unwrap_or(after_scope.len());
    let name = &rest[..scope_len + name_end];
    let tail = &after_scope[name_end..];
    let (version, sub) = match tail.strip_prefix('@') {
        Some(v) => match v.find('/') {
            Some(i) => (Some(&v[..i]), Some(&v[i + 1..])),
            None => (Some(v), None),
        },
        None => (None, tail.strip_prefix('/')),
    };
    Some((name, version, sub))
}

fn check_parts(spec: &str, name: &str, version: Option<&str>, sub: Option<&str>) -> Result<(), String> {
    let bad = |what: &str| Err(format!("invalid package specifier '{spec}': {what}"));
    let parts: Vec<&str> = name.trim_start_matches('@').split('/').collect();
    if parts.len() > 2 || !parts.iter().all(|p| valid_name_part(p)) {
        return bad("bad package name");
    }
    if let Some(v) = version
        && (v.is_empty() || !v.chars().all(|c| c.is_ascii_alphanumeric() || "._-+^~<>=*|".contains(c)))
    {
        return bad("bad version");
    }
    if let Some(s) = sub
        && !valid_subpath(s)
    {
        return bad("bad subpath");
    }
    Ok(())
}

/// Entry URL of an `npm:` or `jsr:` specifier; `None` for other specifiers.
pub(crate) fn remote_entry(registry: &Registry, spec: &str) -> Option<Result<String, String>> {
    let (prefix, rest) = match spec.split_once(':') {
        Some(("npm", r)) => ("", r),
        Some(("jsr", r)) => ("/jsr", r),
        _ => return None,
    };
    let Some((name, version, sub)) = split_package(rest) else {
        return Some(Err(format!("invalid package specifier '{spec}'")));
    };
    if let Err(e) = check_parts(spec, name, version, sub) {
        return Some(Err(e));
    }
    if prefix == "/jsr" && !name.starts_with('@') {
        return Some(Err(format!("invalid package specifier '{spec}': JSR packages are scoped (jsr:@scope/name)")));
    }
    let mut url = format!("{}{prefix}/{name}", registry.base);
    if let Some(v) = version {
        url.push('@');
        url.extend(utf8_percent_encode(v, VERSION_ESCAPE));
    }
    if let Some(s) = sub {
        url.push('/');
        url.push_str(s);
    }
    url.push('?');
    url.push_str(ENTRY_QUERY);
    Some(Ok(url))
}

/// The Node built-in an import refers to (`node:fs`, esm.sh's `/node/fs.mjs`).
fn node_builtin(specifier: &str, resolved: Option<&Url>) -> Option<String> {
    if let Some(n) = specifier.strip_prefix("node:") {
        return Some(n.to_string());
    }
    let path = resolved?.path();
    let rest = path.strip_prefix("/node/").or_else(|| {
        // Versioned builds: `/v135/node/fs.mjs`.
        let mut segs = path.trim_start_matches('/').splitn(2, '/');
        let first = segs.next()?;
        (first.starts_with('v') && first[1..].chars().all(|c| c.is_ascii_digit()))
            .then(|| segs.next()?.strip_prefix("node/"))
            .flatten()
    })?;
    Some(rest.trim_end_matches(".mjs").trim_end_matches(".js").to_string())
}

pub(crate) fn unsupported_node_message(name: &str, importer: &str) -> String {
    format!(
        "{importer} needs the Node.js built-in module '{name}', which is not available in Merpati scripts \
         (no file system, network, process or thread access)"
    )
}

/// Resolves an import found inside a fetched module.
pub(crate) fn remote_import(registry: &Registry, specifier: &str, referrer: &str) -> Result<String, String> {
    if let Some(r) = remote_entry(registry, specifier) {
        return r;
    }
    if let Some(name) = node_builtin(specifier, None) {
        return node_polyfill(registry, &name, referrer);
    }
    let base = Url::parse(referrer).map_err(|e| format!("bad module URL {referrer}: {e}"))?;
    let is_path = specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../");
    let url = if is_path {
        base.join(specifier).map_err(|e| format!("cannot resolve '{specifier}' from {referrer}: {e}"))?
    } else if specifier.starts_with("http://") || specifier.starts_with("https://") {
        Url::parse(specifier).map_err(|e| format!("bad import URL '{specifier}': {e}"))?
    } else {
        return Err(format!("cannot resolve the bare import '{specifier}' in {referrer}"));
    };
    if !registry.owns(&url) {
        return Err(format!(
            "{referrer} imports {url}, which is not on the package registry ({}); only the configured registry is allowed",
            registry.host()
        ));
    }
    if let Some(name) = node_builtin(specifier, Some(&url))
        && UNSUPPORTED_NODE.contains(&name.as_str())
    {
        return Err(unsupported_node_message(&name, referrer));
    }
    Ok(url.to_string())
}

fn node_polyfill(registry: &Registry, name: &str, referrer: &str) -> Result<String, String> {
    if UNSUPPORTED_NODE.contains(&name) || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '/') {
        return Err(unsupported_node_message(name, referrer));
    }
    Ok(format!("{}/node/{name}.mjs", registry.base))
}

/// Whether `name` looks like a Package Library name (`@scope/name`, `name`,
/// optionally with a subpath).
pub(crate) fn is_library_name(name: &str) -> bool {
    let body = name.strip_prefix('@').unwrap_or(name);
    if body.is_empty() || name.contains('\\') {
        return false;
    }
    let mut segs = body.split('/');
    let first_ok = segs.next().is_some_and(valid_name_part);
    first_ok && segs.all(|s| valid_name_part(s) && s != "..")
}

/// Finds a Package Library file: `<lib>/<name>.js|.mjs|.cjs` or
/// `<lib>/<name>/index.*`. Relative names need the requiring file.
pub(crate) fn local_file(
    lib: &Path,
    spec: &str,
    parent: Option<&Path>,
) -> Result<Option<(PathBuf, LocalKind)>, String> {
    let relative = spec.starts_with("./") || spec.starts_with("../");
    let base = if relative {
        let Some(dir) = parent.and_then(Path::parent) else {
            return Err(format!("Cannot find module '{spec}': relative paths only work inside packages"));
        };
        dir.join(spec)
    } else if is_library_name(spec) {
        lib.join(spec)
    } else {
        return Ok(None);
    };
    let Ok(root) = lib.canonicalize() else { return Ok(None) };
    let name = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut candidates = vec![base.clone()];
    for ext in ["js", "mjs", "cjs"] {
        candidates.push(base.with_file_name(format!("{name}.{ext}")));
    }
    for ext in ["js", "mjs", "cjs"] {
        candidates.push(base.join(format!("index.{ext}")));
    }
    for c in candidates {
        if !c.is_file() {
            continue;
        }
        let Ok(real) = c.canonicalize() else { continue };
        if !real.starts_with(&root) {
            return Err(format!("Cannot find module '{spec}': it resolves outside the package library"));
        }
        let kind = match real.extension().and_then(|e| e.to_str()) {
            Some("mjs") => LocalKind::Esm,
            Some("cjs") => LocalKind::Cjs,
            _ => LocalKind::Cjs,
        };
        return Ok(Some((real, kind)));
    }
    Ok(None)
}

/// `.js` files are ES modules when they contain top-level `import`/`export`
/// statements and no CommonJS exports.
pub(crate) fn sniff_kind(path: &Path, source: &str, guess: LocalKind) -> LocalKind {
    if path.extension().and_then(|e| e.to_str()) != Some("js") {
        return guess;
    }
    if source.contains("module.exports") || source.contains("exports.") {
        return LocalKind::Cjs;
    }
    let esm = source.lines().any(|l| {
        let l = l.trim_start();
        (l.starts_with("export ") || l.starts_with("export{") || l.starts_with("export*"))
            || (l.starts_with("import ") && (l.contains(" from ") || l.contains('\'') || l.contains('"')))
            || l.starts_with("import{")
    });
    if esm { LocalKind::Esm } else { LocalKind::Cjs }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registry {
        Registry::parse("https://esm.sh/").unwrap()
    }

    #[test]
    fn entry_urls() {
        let r = reg();
        let e = |s: &str| remote_entry(&r, s).unwrap();
        assert_eq!(e("npm:lodash-es@4.17.21").unwrap(), "https://esm.sh/lodash-es@4.17.21?bundle&target=es2022");
        assert_eq!(
            e("npm:@scope/pkg@^1.2/sub/x").unwrap(),
            "https://esm.sh/@scope/pkg@%5E1.2/sub/x?bundle&target=es2022"
        );
        assert_eq!(e("npm:dayjs").unwrap(), "https://esm.sh/dayjs?bundle&target=es2022");
        assert_eq!(
            e("jsr:@std/encoding@1/base64").unwrap(),
            "https://esm.sh/jsr/@std/encoding@1/base64?bundle&target=es2022"
        );
        assert!(e("jsr:encoding@1").is_err());
        assert!(e("npm:../etc@1").is_err());
        assert!(e("npm:x@1/../../y").is_err());
        assert!(remote_entry(&r, "lodash").is_none());
    }

    #[test]
    fn imports_inside_modules() {
        let r = reg();
        let from = "https://esm.sh/dayjs@1.11.13?bundle&target=es2022";
        assert_eq!(
            remote_import(&r, "/dayjs@1.11.13/es2022/dayjs.bundle.mjs", from).unwrap(),
            "https://esm.sh/dayjs@1.11.13/es2022/dayjs.bundle.mjs"
        );
        assert_eq!(
            remote_import(&r, "./util.mjs", "https://esm.sh/a@1/es2022/a.mjs").unwrap(),
            "https://esm.sh/a@1/es2022/util.mjs"
        );
        assert!(
            remote_import(&r, "https://evil.example/x.mjs", from).unwrap_err().contains("not on the package registry")
        );
        assert!(remote_import(&r, "/node/fs.mjs", from).unwrap_err().contains("'fs'"));
        assert!(remote_import(&r, "/v135/node/child_process.mjs", from).unwrap_err().contains("child_process"));
        assert_eq!(remote_import(&r, "/node/buffer.mjs", from).unwrap(), "https://esm.sh/node/buffer.mjs");
        assert!(remote_import(&r, "node:net", from).is_err());
        assert!(remote_import(&r, "react", from).unwrap_err().contains("bare import"));
    }

    #[test]
    fn library_names() {
        assert!(is_library_name("@team/utils"));
        assert!(is_library_name("helpers/dates"));
        assert!(!is_library_name("../x"));
        assert!(!is_library_name("@team/../x"));
        assert!(!is_library_name(""));
    }
}
