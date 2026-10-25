//! Small helpers shared by the commands.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use merpati_core::model::{Collection, Item, RequestKind};
use merpati_core::secrets::{KeychainBackend, SecretStore};
use merpati_core::settings::AppSettings;
use merpati_core::tokens::{MemoryTokenStore, TokenStore};
use merpati_core::variables::VaultLookup;
use merpati_http::{CookieJar, EngineConfig, HttpEngine};
use merpati_runner::Services;

/// `key=value` command line pairs.
pub fn parse_kv(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("expected key=value, got \"{s}\"")),
    }
}

/// Durations like `500ms`, `30s`, `5m`, `1h30m`, or a plain number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let out_of_range = || format!("invalid duration \"{s}\"");
    if let Ok(secs) = s.parse::<f64>() {
        if secs < 0.0 {
            return Err("negative duration".into());
        }
        // `inf`, `NaN` and huge values would panic in `Duration::from_secs_f64`.
        return Duration::try_from_secs_f64(secs).map_err(|_| out_of_range());
    }
    let mut total = Duration::ZERO;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let mut unit = c.to_string();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphabetic() {
                unit.push(n);
                chars.next();
            } else {
                break;
            }
        }
        let value: f64 = num.parse().map_err(|_| format!("invalid duration \"{s}\""))?;
        num.clear();
        let secs = match unit.as_str() {
            "ms" => value / 1000.0,
            "s" | "sec" => value,
            "m" | "min" => value * 60.0,
            "h" => value * 3600.0,
            "d" => value * 86400.0,
            _ => return Err(format!("unknown unit \"{unit}\" in \"{s}\" (use ms, s, m, h, d)")),
        };
        let part = Duration::try_from_secs_f64(secs).map_err(|_| out_of_range())?;
        total = total.checked_add(part).ok_or_else(out_of_range)?;
    }
    if !num.is_empty() {
        return Err(format!("missing unit after \"{num}\" in \"{s}\""));
    }
    Ok(total)
}

/// Script timeout from `--timeout-script`/settings; 0 means no limit, as in
/// newman (runaway loops are still stopped by the engine's loop limit).
pub fn script_timeout(ms: u64) -> Duration {
    if ms == 0 { Duration::from_secs(365 * 24 * 3600) } else { Duration::from_millis(ms) }
}

/// `--color` (newman): `auto` colors a terminal unless `NO_COLOR` is set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    On,
    Off,
}

pub fn use_color(mode: ColorMode) -> bool {
    match mode {
        ColorMode::On => true,
        ColorMode::Off => false,
        ColorMode::Auto => std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal(),
    }
}

/// `--bail` modifiers newman accepts.
const BAIL_MODIFIERS: [&str; 2] = ["failure", "folder"];

/// newman takes `--bail` alone or followed by modifiers (`--bail failure`,
/// `--bail failure,folder`). For `run` and `schedule`, a modifier list right
/// after `--bail` is joined to it (`--bail=failure`) before clap parses the
/// arguments, so any other word after `--bail` (e.g. the collection) stays a
/// positional argument.
pub fn join_bail_modifiers(args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    if !matches!(args.get(1).and_then(|a| a.to_str()), Some("run" | "schedule")) {
        return args;
    }
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.into_iter().peekable();
    let mut options = true;
    while let Some(arg) = iter.next() {
        if arg == "--" {
            options = false;
        }
        if options && arg == "--bail" {
            let modifiers = iter
                .peek()
                .and_then(|n| n.to_str())
                .filter(|n| !n.is_empty() && n.split(',').all(|m| BAIL_MODIFIERS.contains(&m.trim())));
            if let Some(m) = modifiers {
                out.push(format!("--bail={m}").into());
                iter.next();
                continue;
            }
        }
        out.push(arg);
    }
    out
}

/// Parsed `--bail` modifiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BailModifiers {
    pub failure: bool,
    pub folder: bool,
}

pub fn parse_bail(s: &str) -> Result<BailModifiers, String> {
    let mut m = BailModifiers::default();
    for part in s.split(',').map(str::trim) {
        match part {
            "failure" => m.failure = true,
            "folder" => m.folder = true,
            other => return Err(format!("unknown --bail modifier \"{other}\" (failure, folder)")),
        }
    }
    Ok(m)
}

/// Applies run-wide overrides to every request so they win over
/// per-request settings (newman semantics for `--insecure` etc.).
pub fn force_settings(c: &mut Collection, insecure: bool, ignore_redirects: bool, timeout_ms: Option<u64>) {
    fn walk(items: &mut [Item], insecure: bool, ignore_redirects: bool, timeout_ms: Option<u64>) {
        for item in items {
            match item {
                Item::Folder(f) => walk(&mut f.items, insecure, ignore_redirects, timeout_ms),
                Item::Request(r) => {
                    // Sessions run by the runner honor `-k` as well.
                    let session_tls = match &mut r.request {
                        RequestKind::WebSocket(w) => Some(&mut w.verify_ssl),
                        RequestKind::SocketIo(s) => Some(&mut s.verify_ssl),
                        RequestKind::Mqtt(m) => Some(&mut m.verify_ssl),
                        _ => None,
                    };
                    if insecure && let Some(v) = session_tls {
                        *v = Some(false);
                    }
                    if let Some(h) = r.http_mut() {
                        if insecure {
                            h.settings.verify_ssl = Some(false);
                        }
                        if ignore_redirects {
                            h.settings.follow_redirects = Some(false);
                        }
                        if let Some(t) = timeout_ms {
                            h.settings.timeout_ms = Some(t);
                        }
                    }
                }
            }
        }
    }
    walk(&mut c.items, insecure, ignore_redirects, timeout_ms);
}

pub fn load_settings(path: Option<&Path>) -> anyhow::Result<AppSettings> {
    match path {
        Some(p) => {
            let text = std::fs::read_to_string(p)?;
            Ok(serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("invalid settings file {}: {e}", p.display()))?)
        }
        None => Ok(AppSettings::default()),
    }
}

/// `local_dir` is the workspace's `.merpati/` folder: vault names linked to
/// external secret managers there resolve through them.
pub fn services(
    settings: &AppSettings,
    jar: Arc<CookieJar>,
    secrets: Option<Arc<SecretStore>>,
    local_dir: Option<&std::path::Path>,
    script_timeout: Duration,
    keychain: bool,
) -> Services {
    // Scripts run in worker processes started from this executable, so a
    // script that crashes or hangs in native code cannot take the run down.
    let config = merpati_script::IsolationConfig {
        memory_limit_bytes: settings.script_memory_mb.saturating_mul(1024 * 1024),
        ..Default::default()
    };
    let scripts: Arc<dyn merpati_core::script::ScriptEngine> =
        match merpati_script::IsolatedEngine::for_current_exe(config) {
            Ok(e) => {
                e.warm_up();
                Arc::new(e)
            }
            Err(_) => {
                let e = merpati_script::BoaEngine::new();
                e.warm_up();
                Arc::new(e)
            }
        };
    let (tokens, vault): (Arc<dyn TokenStore>, Option<Arc<dyn VaultLookup>>) = match secrets {
        Some(s) => {
            let external = local_dir.filter(|d| merpati_secrets::SecretsConfig::path(d).exists()).and_then(|dir| {
                let client = merpati_secrets::client_for(&settings.proxy, &settings.certificates).ok()?;
                merpati_secrets::ExternalSecrets::open(dir, s.clone(), client)
                    .map_err(|e| eprintln!("warning: external secrets: {}", e.message))
                    .ok()
            });
            let vault: Arc<dyn VaultLookup> = match external {
                Some(e) if !e.linked_names().is_empty() => Arc::new(e),
                _ => s.clone(),
            };
            (s, Some(vault))
        }
        None => (Arc::new(MemoryTokenStore::default()), None),
    };
    let mut config = EngineConfig::from_settings(settings);
    config.secret_resolver = Some(settings_resolver(settings, vault.clone(), keychain));
    let http = Arc::new(HttpEngine::new(config, jar));
    Services {
        http,
        scripts,
        tokens,
        vault,
        defaults: settings.request.clone(),
        script_timeout,
        runtime: tokio::runtime::Handle::current(),
        packages: local_dir
            .and_then(|d| d.parent())
            .map(|ws| merpati_core::script::PackageContext::for_workspace(ws, &settings.package_registry)),
    }
}

/// `merpati-reports/<collection>-<timestamp>.<ext>`.
pub fn default_report_path(dir: &str, collection: &str, ext: &str) -> PathBuf {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S");
    PathBuf::from(dir).join(format!("{}-{ts}.{ext}", merpati_core::util::slugify(collection)))
}

/// Writes text to `path`, or stdout when `None`.
pub fn write_output(path: Option<&Path>, text: &str) -> anyhow::Result<()> {
    match path {
        Some(p) => {
            if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(p, text)?;
            Ok(())
        }
        None => {
            println!("{text}");
            Ok(())
        }
    }
}

/// Expands `{{vault:...}}` in the settings (proxy password, client
/// certificate passphrases): `settings.` names from the app's Keychain entry
/// for settings (read only when the settings refer to it and the keychain is
/// allowed), other names from the workspace vault.
fn settings_resolver(
    settings: &AppSettings,
    vault: Option<Arc<dyn VaultLookup>>,
    keychain: bool,
) -> merpati_http::SecretResolver {
    let refers = serde_json::to_string(settings)
        .is_ok_and(|s| s.contains(&format!("{{{{vault:{}", merpati_core::secrets::SETTINGS_VAULT_PREFIX)));
    let store = if refers && keychain && KeychainBackend::available() {
        SecretStore::new(Box::new(KeychainBackend::for_settings()))
    } else {
        SecretStore::memory()
    };
    let lookup = merpati_core::secrets::SettingsVault { settings: Arc::new(store), inner: vault };
    merpati_http::SecretResolver::new(move |s| {
        let scopes = merpati_core::variables::VariableScopes::default();
        merpati_core::variables::Resolver::new(&scopes).with_vault(&lookup).resolve(s)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("2").unwrap(), Duration::from_secs(2));
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("5m3").is_err());
        // Out-of-range values are errors, not panics.
        assert!(parse_duration("inf").is_err());
        assert!(parse_duration("NaN").is_err());
        assert!(parse_duration("1e30").is_err());
        assert!(parse_duration("99999999999999999999h").is_err());
        assert!(parse_duration("18446744073709551615s18446744073709551615s").is_err());
    }

    #[test]
    fn bail_modifiers_are_joined() {
        let args = |s: &str| s.split(' ').map(std::ffi::OsString::from).collect::<Vec<_>>();
        let joined = |s: &str| join_bail_modifiers(args(s)).join(std::ffi::OsStr::new(" ")).into_string().unwrap();
        assert_eq!(joined("merpati run --bail c.json"), "merpati run --bail c.json");
        assert_eq!(joined("merpati run --bail failure c.json"), "merpati run --bail=failure c.json");
        assert_eq!(joined("merpati run c.json --bail folder,failure"), "merpati run c.json --bail=folder,failure");
        assert_eq!(joined("merpati run c.json --bail"), "merpati run c.json --bail");
        assert_eq!(joined("merpati schedule --bail folder c.json"), "merpati schedule --bail=folder c.json");
        // Only `run`/`schedule`, and not after `--`.
        assert_eq!(joined("merpati flow --bail failure"), "merpati flow --bail failure");
        assert_eq!(joined("merpati run -- --bail failure"), "merpati run -- --bail failure");
        assert_eq!(joined("merpati run --bail failure,nope"), "merpati run --bail failure,nope");
        assert_eq!(parse_bail("failure"), Ok(BailModifiers { failure: true, folder: false }));
        assert_eq!(parse_bail("folder, failure"), Ok(BailModifiers { failure: true, folder: true }));
        assert!(parse_bail("fail").is_err());
    }

    #[test]
    fn kv() {
        assert_eq!(parse_kv("a=b=c").unwrap(), ("a".into(), "b=c".into()));
        assert!(parse_kv("=x").is_err());
        assert!(parse_kv("nope").is_err());
    }
}
