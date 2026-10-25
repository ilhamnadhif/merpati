//! UI translations (English and Indonesian).
//!
//! English source strings are the keys (gettext style), so untranslated
//! strings simply fall back to English.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

use merpati_core::settings::Language;

static LANG: AtomicU8 = AtomicU8::new(0);

pub fn set_language(lang: Language) {
    LANG.store(if lang == Language::Id { 1 } else { 0 }, Ordering::Relaxed);
}

static ID: LazyLock<HashMap<String, String>> = LazyLock::new(|| parse(include_str!("../i18n/id.tsv")));

/// `English<TAB>Indonesian` per line; `\n` and `\t` are escapes.
fn parse(tsv: &str) -> HashMap<String, String> {
    let unescape = |s: &str| s.replace("\\n", "\n").replace("\\t", "\t");
    tsv.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('\t'))
        .map(|(k, v)| (unescape(k), unescape(v)))
        .collect()
}

/// Translates an English UI string to the current language.
pub fn tr(s: &str) -> &str {
    if LANG.load(Ordering::Relaxed) == 1
        && let Some(t) = ID.get(s)
    {
        return t.as_str();
    }
    s
}

/// Translates a template with `{}` placeholders, then fills them in order:
/// `trf("Connected to {} (sid {})", &[&ns, &sid])`.
pub fn trf(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut parts = tr(template).split("{}").peekable();
    while let Some(part) = parts.next() {
        out.push_str(part);
        if parts.peek().is_some()
            && let Some(a) = args.next()
        {
            out.push_str(&a.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `tr("…")`/`trf("…")` key in the app has an Indonesian entry, and
    /// no entry is listed twice.
    #[test]
    fn all_keys_translated() {
        let table = include_str!("../i18n/id.tsv");
        let mut seen = std::collections::HashSet::new();
        for l in table.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (k, v) = l.split_once('\t').unwrap_or_else(|| panic!("no tab in {l:?}"));
            assert!(!v.trim().is_empty(), "empty translation for {k:?}");
            assert!(seen.insert(k), "duplicate entry {k:?}");
            assert_eq!(k.matches("{}").count(), v.matches("{}").count(), "placeholders differ in {k:?}");
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut missing = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == "i18n.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            // Strings in test modules are not shown to users.
            let src = src.split("#[cfg(test)]").next().unwrap_or("");
            for marker in ["tr(\"", "trf(\""] {
                for (i, _) in src.match_indices(marker) {
                    // Skip identifiers that merely end in `tr`/`trf`.
                    let before = src[..i].chars().last().unwrap_or(' ');
                    if before.is_alphanumeric() || before == '_' {
                        continue;
                    }
                    let rest = &src[i + marker.len()..];
                    let key = rest.split('"').next().unwrap_or("");
                    if !key.contains('\\') && !seen.contains(key) {
                        missing.push(format!("{}: {key}", path.file_name().unwrap().to_string_lossy()));
                    }
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "{} untranslated keys:\n{}", missing.len(), missing.join("\n"));
    }

    #[test]
    fn templates_fill_in_order() {
        assert_eq!(trf("{} of {}", &[&1, &3]), "1 of 3");
    }
}
