//! Update check, the "new version available" pop-up and the installer.
//!
//! On startup (when enabled in Settings › General › Updates) the app fetches
//! the release feed once in the background: a GitHub Releases API URL or a
//! small JSON manifest, with only the app version as User-Agent. When the
//! latest release is newer than this build and not skipped, a pop-up shows
//! the release notes with "Skip this version", "Remind me later" and a main
//! action.
//!
//! On macOS, when Merpati runs from its `.app` bundle and the release has an
//! app archive for this Mac (`Merpati-<version>-macos-<arch>.zip`), the main
//! action is "Install and restart": the feed is read again (a release
//! published after the pop-up opened wins), the archive is downloaded into a
//! private temporary folder and checked against its SHA-256 (GitHub's
//! `digest`, else a `SHA256SUMS*` asset), unpacked with `ditto`, and the app in
//! it must be Merpati (`dev.merpati.app`) of the release's version, with a
//! valid code signature from the same team as this build when this build has
//! one. The running bundle is then replaced (the old one is moved aside and
//! put back if the new one can't be moved in; macOS asks for an administrator
//! password when the folder isn't writable), a helper opens the new version
//! once this process has exited, and Merpati quits the normal way, so the
//! session, workspace and settings are saved as usual.
//!
//! Elsewhere (Windows, Linux, a development build, a release without an app
//! archive) the installer for this platform is downloaded into the Downloads
//! folder, checked the same way and opened by the user, or the release page
//! is opened. A download without a published checksum is never kept.
//!
//! Everything is fetched over HTTPS; plain HTTP is accepted only from this
//! computer when the feed is on this computer too (`MERPATI_UPDATE_URL`,
//! testing).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use egui::RichText;
use serde_json::Value;

use crate::app::{MerpatiApp, Msg};
use crate::i18n::{tr, trf};
use crate::{icons, kit, theme};

/// This build's version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for the next piece of a download.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 10;
/// Context data flag the settings dialog sets to ask for a check now.
const CHECK_NOW: &str = "merpati-update-check-now";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// SHA-256 (lowercase hex) the feed publishes for the file: GitHub's
    /// `digest`, or `sha256` in a manifest.
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// Without a leading `v`.
    pub version: String,
    pub name: String,
    pub notes: String,
    /// Release page for people to read.
    pub page_url: String,
    /// `YYYY-MM-DD`, when known.
    pub published: Option<String>,
    pub assets: Vec<Asset>,
}

/// A step of a download or an install, for the pop-up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stage {
    /// Reading the feed again for the newest release (install).
    Starting,
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    /// Checking the checksum (and, when installing, the new app).
    Verifying,
    /// Replacing the running app.
    Installing,
    /// Installed: Merpati quits and the new version opens.
    Restarting,
}

pub enum UpdateMsg {
    Checked {
        manual: bool,
        result: Result<Option<Release>, String>,
    },
    /// A release newer than the one offered, found when the install started:
    /// it is installed instead.
    Newer(Release),
    Stage(Stage),
    /// The installer is in the Downloads folder.
    Downloaded(Result<PathBuf, String>),
    /// The new version replaced this one and opens once Merpati has quit.
    Installed(Result<(), String>),
}

#[derive(Default)]
pub struct UpdateState {
    pub available: Option<Release>,
    pub dialog_open: bool,
    pub checking: bool,
    /// The download or install under way.
    pub stage: Option<Stage>,
    /// The installer downloaded into the Downloads folder.
    pub downloaded: Option<PathBuf>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Feed
// ---------------------------------------------------------------------------

fn str_field<'a>(v: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str)).unwrap_or("")
}

/// The SHA-256 of an asset: GitHub's `digest` (`sha256:<hex>`) or a
/// manifest's `sha256`, as lowercase hex.
fn asset_sha256(a: &Value) -> Option<String> {
    let hex = a
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|d| d.trim().strip_prefix("sha256:"))
        .or_else(|| a.get("sha256").and_then(Value::as_str))?
        .trim();
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| hex.to_ascii_lowercase())
}

/// A release from a GitHub Releases API object (`tag_name`, `body`,
/// `html_url`, `assets[].browser_download_url`, `assets[].digest`) or a
/// manifest (`version`, `notes`, `url`, `assets[].url`, `assets[].sha256`).
pub fn parse_feed(v: &Value) -> Result<Release, String> {
    // `/releases` lists releases: take the first published, non-draft one.
    let v = match v.as_array() {
        Some(list) => list
            .iter()
            .find(|r| !r.get("draft").and_then(Value::as_bool).unwrap_or(false))
            .ok_or("the release feed lists no releases")?,
        None => v,
    };
    let version = str_field(v, &["version", "tag_name"]).trim().trim_start_matches(['v', 'V']).to_string();
    if version.is_empty() || !version.starts_with(|c: char| c.is_ascii_digit()) {
        return Err("the release feed has no version".into());
    }
    let name = match str_field(v, &["name"]).trim() {
        "" => format!("Merpati {version}"),
        n => n.to_string(),
    };
    let published =
        Some(str_field(v, &["published_at", "date"])).filter(|d| d.len() >= 10).map(|d| d[..10].to_string());
    let assets = v
        .get("assets")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    let name = str_field(a, &["name"]).to_string();
                    let url = str_field(a, &["browser_download_url", "url"]).to_string();
                    let size = a.get("size").and_then(Value::as_u64).unwrap_or(0);
                    (!name.is_empty() && url.starts_with("http")).then(|| Asset {
                        name,
                        url,
                        size,
                        sha256: asset_sha256(a),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release {
        version,
        name,
        notes: str_field(v, &["notes", "body"]).trim().to_string(),
        page_url: str_field(v, &["html_url", "url", "page"]).to_string(),
        published,
        assets,
    })
}

/// `1.2.10` > `1.2.9`; a release > its pre-releases (`1.2.0-beta.2`).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn split(v: &str) -> (Vec<u64>, Option<&str>) {
        let v = v.trim().trim_start_matches(['v', 'V']);
        let v = v.split('+').next().unwrap_or(v);
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (v, None),
        };
        (core.split('.').map(|n| n.parse().unwrap_or(0)).collect(), pre)
    }
    let ((mut na, pa), (mut nb, pb)) = (split(a), split(b));
    let len = na.len().max(nb.len());
    na.resize(len, 0);
    nb.resize(len, 0);
    na.cmp(&nb).then_with(|| match (pa, pb) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let (xs, ys): (Vec<&str>, Vec<&str>) = (x.split('.').collect(), y.split('.').collect());
            for (p, q) in xs.iter().zip(&ys) {
                let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    _ => p.cmp(q),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            xs.len().cmp(&ys.len())
        }
    })
}

const ARM: &[&str] = &["arm64", "aarch64"];
const INTEL: &[&str] = &["x86_64", "x64", "amd64", "intel"];

/// The asset built for `arch` (`std::env::consts::ARCH`) among `candidates`,
/// else a universal one, else one that names no other architecture.
fn for_arch<'a>(candidates: &[&'a Asset], arch: &str) -> Option<&'a Asset> {
    let (ours, others) = if arch == "aarch64" { (ARM, INTEL) } else { (INTEL, ARM) };
    let names = |a: &Asset, words: &[&str]| {
        let n = a.name.to_ascii_lowercase();
        words.iter().any(|w| n.contains(w))
    };
    candidates
        .iter()
        .find(|a| names(a, ours))
        .or_else(|| candidates.iter().find(|a| names(a, &["universal"])))
        .or_else(|| candidates.iter().find(|a| !names(a, others)))
        .copied()
}

/// How this copy of Merpati was installed. An update fetches the same kind
/// of file, so a `.deb` stays a `.deb` and an AppImage an AppImage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Install {
    /// macOS app bundle (the `.zip` replaces it; the `.dmg` otherwise).
    Mac,
    /// Windows, installed with the setup program.
    WindowsSetup,
    /// Windows, unpacked from the `.zip`.
    WindowsZip,
    /// Linux AppImage (`$APPIMAGE` is set): replaced in place.
    AppImage,
    /// Linux package from the `.deb` (Debian, Ubuntu, Mint, ...).
    Deb,
    /// Linux package from the `.rpm` (Fedora, RHEL, openSUSE, ...).
    Rpm,
    /// Linux, unpacked from the `.tar.gz`.
    LinuxArchive,
}

impl Install {
    /// This process's kind of install.
    pub fn current() -> Install {
        static KIND: OnceLock<Install> = OnceLock::new();
        *KIND.get_or_init(|| {
            let exe = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok());
            let appimage = std::env::var_os("APPIMAGE").is_some();
            Install::detect(std::env::consts::OS, exe.as_deref(), appimage, |p| p.exists())
        })
    }

    fn detect(os: &str, exe: Option<&Path>, appimage: bool, exists: impl Fn(&Path) -> bool) -> Install {
        match os {
            "macos" => Install::Mac,
            // The setup program leaves its uninstaller next to the app.
            "windows" if exe.and_then(Path::parent).is_some_and(|d| exists(&d.join("unins000.exe"))) => {
                Install::WindowsSetup
            }
            "windows" => Install::WindowsZip,
            _ if appimage => Install::AppImage,
            // Packages install into /usr/bin; the distribution says which kind.
            _ if exe.is_some_and(|e| e.starts_with("/usr/")) => {
                if exists(Path::new("/etc/debian_version")) {
                    Install::Deb
                } else if ["/etc/fedora-release", "/etc/redhat-release", "/etc/SUSE-brand", "/etc/SuSE-release"]
                    .iter()
                    .any(|f| exists(Path::new(f)))
                {
                    Install::Rpm
                } else {
                    Install::LinuxArchive
                }
            }
            _ => Install::LinuxArchive,
        }
    }

    /// Release files that fit, best first: (the name contains, it ends with).
    fn wanted(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Install::Mac => &[("", ".dmg")],
            Install::WindowsSetup => &[("setup", ".exe"), ("", ".msi"), ("windows", ".zip")],
            Install::WindowsZip => &[("windows", ".zip"), ("setup", ".exe")],
            Install::AppImage => &[("", ".appimage")],
            Install::Deb => &[("", ".deb"), ("", ".appimage")],
            Install::Rpm => &[("", ".rpm"), ("", ".appimage")],
            Install::LinuxArchive => &[("linux", ".tar.gz"), ("", ".appimage")],
        }
    }
}

/// The file an update of an `install` on `arch` (`std::env::consts::ARCH`)
/// downloads, among `assets`.
pub fn pick_asset<'a>(assets: &'a [Asset], install: Install, arch: &str) -> Option<&'a Asset> {
    install.wanted().iter().find_map(|(contains, ends)| {
        let candidates: Vec<&Asset> = assets
            .iter()
            .filter(|a| {
                let n = a.name.to_ascii_lowercase();
                !n.contains("cli") && !n.contains("sha256") && n.contains(contains) && n.ends_with(ends)
            })
            .collect();
        for_arch(&candidates, arch)
    })
}

/// The macOS app archive for `arch`
/// (`Merpati-<version>-macos-<arm64|x86_64|universal>.zip`, the zipped
/// `Merpati.app`) that replaces the running app.
pub fn pick_app_archive<'a>(assets: &'a [Asset], arch: &str) -> Option<&'a Asset> {
    let candidates: Vec<&Asset> = assets
        .iter()
        .filter(|a| {
            let n = a.name.to_ascii_lowercase();
            n.starts_with("merpati-") && n.contains("macos") && n.ends_with(".zip") && !n.contains("cli")
        })
        .collect();
    for_arch(&candidates, arch)
}

/// What the pop-up's main button does with a release.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// Replace the running app with this archive and restart.
    Install(Asset),
    /// Download this installer into the Downloads folder for the user.
    Download(Asset),
    /// Open the release page.
    Page,
}

/// The [`Plan`] for `assets` for an `install` on `arch`. `in_bundle`: this
/// process runs from an app bundle it may replace.
pub fn plan(assets: &[Asset], install: Install, arch: &str, in_bundle: bool) -> Plan {
    if install == Install::Mac
        && in_bundle
        && let Some(a) = pick_app_archive(assets, arch)
    {
        return Plan::Install(a.clone());
    }
    pick_asset(assets, install, arch).cloned().map_or(Plan::Page, Plan::Download)
}

fn plan_here(release: &Release) -> Plan {
    plan(&release.assets, Install::current(), std::env::consts::ARCH, running_bundle().is_some())
}

/// The SHA-256 of `asset` in a `sha256sum`-style listing.
pub fn checksum_for(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == asset && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

// ---------------------------------------------------------------------------
// Where things go
// ---------------------------------------------------------------------------

/// The `.app` bundle this process runs from, when it can replace itself
/// (macOS only).
pub fn running_bundle() -> Option<&'static Path> {
    static BUNDLE: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUNDLE
        .get_or_init(|| {
            if cfg!(target_os = "macos") { std::env::current_exe().ok().and_then(|exe| bundle_of(&exe)) } else { None }
        })
        .as_deref()
}

/// The bundle of an executable at `<bundle>.app/Contents/MacOS/<exe>`, unless
/// it is on a disk image or a read-only copy macOS made (App Translocation).
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let exe = exe.canonicalize().ok()?;
    let macos = exe.parent()?;
    let bundle = macos.parent()?.parent()?;
    let is_app = macos.ends_with("Contents/MacOS") && bundle.extension().is_some_and(|e| e.eq_ignore_ascii_case("app"));
    let path = bundle.to_string_lossy();
    let movable = !path.contains("/AppTranslocation/") && !path.starts_with("/Volumes/");
    (is_app && movable).then(|| bundle.to_path_buf())
}

/// Where downloads go: the user's Downloads folder (temp dir as a
/// fallback; `MERPATI_DOWNLOAD_DIR` overrides it, for tests).
fn downloads_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MERPATI_DOWNLOAD_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    match home.map(|h| h.join("Downloads")) {
        Some(d) if d.is_dir() => d,
        _ => std::env::temp_dir(),
    }
}

/// An asset name from the feed as a plain file name: no folders, not hidden.
fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim().trim_start_matches('.');
    if base.is_empty() { "merpati-update".to_string() } else { base.to_string() }
}

/// A file name in `dir` that does not exist yet (`a.dmg`, `a (1).dmg`, …).
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let split = if name.to_ascii_lowercase().ends_with(".tar.gz") {
        name.len() - ".tar.gz".len()
    } else {
        name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len())
    };
    let (stem, ext) = name.split_at(split);
    (1..1000).map(|i| dir.join(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// Whether `url` is on this computer (`localhost`, `127.0.0.0/8`, `::1`).
fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Which addresses the updater fetches: HTTPS, and plain HTTP to this
/// computer only when the feed is on this computer too (a test feed).
#[derive(Clone, Copy, Debug)]
struct Policy {
    local: bool,
}

impl Policy {
    fn for_feed(feed: &str) -> Policy {
        Policy { local: url::Url::parse(feed.trim()).is_ok_and(|u| is_loopback(&u)) }
    }

    fn allows(self, url: &url::Url) -> bool {
        match url.scheme() {
            "https" => true,
            "http" => self.local && is_loopback(url),
            _ => false,
        }
    }
}

/// A network error with its innermost cause ("error sending request:
/// Connection refused"), without the address.
fn net_error(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut text = e.to_string();
    let mut cause: Option<&dyn std::error::Error> = std::error::Error::source(&e);
    let mut deepest = None;
    while let Some(c) = cause {
        deepest = Some(c);
        cause = c.source();
    }
    if let Some(c) = deepest.map(|c| c.to_string())
        && !text.contains(&c)
    {
        text.push_str(": ");
        text.push_str(&c);
    }
    text
}

/// GETs `url`, following redirects (GitHub sends downloads to its CDN) only
/// to addresses `policy` allows.
async fn get(
    client: &reqwest::Client,
    url: &str,
    policy: Policy,
    accept: &str,
    timeout: Option<Duration>,
) -> Result<reqwest::Response, String> {
    let mut url = url::Url::parse(url.trim()).map_err(|e| e.to_string())?;
    for _ in 0..=MAX_REDIRECTS {
        if !policy.allows(&url) {
            let origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
            return Err(trf("Updates are only downloaded over HTTPS, not from {}.", &[&origin]));
        }
        let mut req =
            client.get(url.clone()).header("User-Agent", format!("Merpati/{CURRENT}")).header("Accept", accept);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req.send().await.map_err(net_error)?;
        let status = resp.status();
        if status.is_redirection()
            && let Some(next) = resp.headers().get("location").and_then(|l| l.to_str().ok())
        {
            url = url.join(next).map_err(|e| e.to_string())?;
            continue;
        }
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        return Ok(resp);
    }
    Err(tr("Too many redirects.").to_string())
}

async fn fetch_feed(client: &reqwest::Client, url: &str, policy: Policy) -> Result<Release, String> {
    let resp = get(client, url, policy, "application/vnd.github+json, application/json", Some(TIMEOUT)).await?;
    let v: Value = resp.json().await.map_err(net_error)?;
    parse_feed(&v)
}

/// The SHA-256 `asset` must have: the feed's digest, else its line in a
/// `SHA256SUMS*` asset of the release (the one naming this OS first). None
/// published is an error: such a download is never kept.
async fn expected_sha256(
    client: &reqwest::Client,
    release: &Release,
    asset: &Asset,
    policy: Policy,
) -> Result<String, String> {
    if let Some(hash) = &asset.sha256 {
        return Ok(hash.clone());
    }
    let mut lists: Vec<&Asset> =
        release.assets.iter().filter(|a| a.name.to_ascii_lowercase().starts_with("sha256sums")).collect();
    lists.sort_by_key(|a| !a.name.to_ascii_lowercase().contains(std::env::consts::OS));
    for list in lists {
        let text =
            get(client, &list.url, policy, "text/plain, */*", Some(TIMEOUT)).await?.text().await.map_err(net_error)?;
        if let Some(hash) = checksum_for(&text, &asset.name) {
            return Ok(hash);
        }
    }
    Err(trf("The release publishes no checksum for {}, so it was not downloaded.", &[&asset.name]))
}

/// Sends background results to the app and wakes it up.
#[derive(Clone)]
struct Reporter {
    tx: std::sync::mpsc::Sender<Msg>,
    ctx: egui::Context,
}

impl Reporter {
    fn of(app: &MerpatiApp) -> Reporter {
        Reporter { tx: app.tx.clone(), ctx: app.ctx.clone() }
    }

    fn send(&self, msg: UpdateMsg) {
        let _ = self.tx.send(Msg::Update(msg));
        self.ctx.request_repaint();
    }
}

/// Downloads `asset` into `dest` (through `<dest>.part`), reporting
/// progress, and checks its size and SHA-256. Nothing is left behind when
/// that fails.
async fn download_to(
    client: &reqwest::Client,
    asset: &Asset,
    dest: &Path,
    sha256: &str,
    policy: Policy,
    report: &Reporter,
) -> Result<(), String> {
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;
    let mut resp = get(client, &asset.url, policy, "application/octet-stream, */*", None).await?;
    let total = resp.content_length().or((asset.size > 0).then_some(asset.size));
    report.send(UpdateMsg::Stage(Stage::Downloading { received: 0, total }));
    let part = dest.with_extension("part");
    let result = async {
        let mut file = tokio::fs::File::create(&part).await.map_err(|e| e.to_string())?;
        let mut hasher = sha2::Sha256::new();
        let (mut received, mut reported) = (0u64, 0u64);
        loop {
            let chunk = tokio::time::timeout(STALL_TIMEOUT, resp.chunk())
                .await
                .map_err(|_| tr("The download stalled.").to_string())?
                .map_err(net_error)?;
            let Some(chunk) = chunk else { break };
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            received += chunk.len() as u64;
            if received - reported > 256 * 1024 {
                reported = received;
                report.send(UpdateMsg::Stage(Stage::Downloading { received, total }));
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        drop(file);
        report.send(UpdateMsg::Stage(Stage::Verifying));
        if asset.size > 0 && received != asset.size {
            return Err(trf("The download is incomplete ({} of {} bytes).", &[&received, &asset.size]));
        }
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != sha256 {
            return Err(tr("The download does not match the published checksum and was deleted.").to_string());
        }
        tokio::fs::rename(&part, dest).await.map_err(|e| e.to_string())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

/// Runs `f` on a thread that may block (child processes, copies).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// Checking, downloading, installing
// ---------------------------------------------------------------------------

fn feed_url(app: &MerpatiApp) -> String {
    std::env::var("MERPATI_UPDATE_URL").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| {
        let u = app.settings.updates.feed_url.trim();
        if u.is_empty() { merpati_core::settings::DEFAULT_UPDATE_FEED.to_string() } else { u.to_string() }
    })
}

/// Whether to check when the app starts: enabled in settings, and not in a
/// scripted run (benchmarks, screenshots) unless the feed is overridden.
pub fn check_on_startup(app: &MerpatiApp) -> bool {
    app.settings.updates.check_on_startup
        && (app.automation.is_none() || std::env::var_os("MERPATI_UPDATE_URL").is_some())
}

/// Fetches the release feed in the background; `manual` checks report
/// "up to date" and errors, startup checks stay quiet.
pub fn check(app: &mut MerpatiApp, manual: bool) {
    if app.update.checking {
        return;
    }
    let client = match app.http.default_client() {
        Ok(c) => c,
        Err(e) => {
            if manual {
                app.toast(trf("Could not check for updates: {}", &[&e.message]), true);
            }
            return;
        }
    };
    app.update.checking = true;
    let url = feed_url(app);
    let report = Reporter::of(app);
    app.rt.spawn(async move {
        let result = fetch_feed(&client, &url, Policy::for_feed(&url))
            .await
            .map(|release| (compare_versions(&release.version, CURRENT) == Ordering::Greater).then_some(release));
        report.send(UpdateMsg::Checked { manual, result });
    });
}

/// Downloads the installer for this platform into the Downloads folder,
/// for the user to open (no in-place install here).
fn start_download(app: &mut MerpatiApp) {
    let Some(release) = app.update.available.clone() else { return };
    let Some(asset) = pick_asset(&release.assets, Install::current(), std::env::consts::ARCH).cloned() else {
        crate::widgets::open_web_link(&release.page_url);
        return;
    };
    let client = match app.http.default_client() {
        Ok(c) => c,
        Err(e) => {
            app.update.error = Some(e.message);
            return;
        }
    };
    let policy = Policy::for_feed(&feed_url(app));
    app.update.error = None;
    app.update.stage = Some(Stage::Downloading { received: 0, total: (asset.size > 0).then_some(asset.size) });
    let report = Reporter::of(app);
    let dest = unique_path(&downloads_dir(), &safe_file_name(&asset.name));
    app.rt.spawn(async move {
        let result = async {
            let sha256 = expected_sha256(&client, &release, &asset, policy).await?;
            download_to(&client, &asset, &dest, &sha256, policy, &report).await?;
            Ok(dest)
        }
        .await;
        report.send(UpdateMsg::Downloaded(result));
    });
}

/// "Install and restart": replaces the running app with the newest release
/// and quits (see the module documentation).
fn start_install(app: &mut MerpatiApp) {
    let (Some(offered), Some(bundle)) = (app.update.available.clone(), running_bundle()) else { return };
    let client = match app.http.default_client() {
        Ok(c) => c,
        Err(e) => {
            app.update.error = Some(e.message);
            return;
        }
    };
    let feed = feed_url(app);
    let prompt = tr("Merpati wants to install an update.").to_string();
    app.update.error = None;
    app.update.downloaded = None;
    app.update.stage = Some(Stage::Starting);
    let report = Reporter::of(app);
    app.rt.spawn(async move {
        let result = install(&client, &feed, offered, bundle, prompt, &report).await;
        report.send(UpdateMsg::Installed(result));
    });
}

async fn install(
    client: &reqwest::Client,
    feed: &str,
    offered: Release,
    bundle: &'static Path,
    prompt: String,
    report: &Reporter,
) -> Result<(), String> {
    let policy = Policy::for_feed(feed);
    let arch = std::env::consts::ARCH;
    // The newest release, should one have come out since the pop-up opened.
    let release = match fetch_feed(client, feed, policy).await {
        Ok(latest)
            if compare_versions(&latest.version, &offered.version) == Ordering::Greater
                && pick_app_archive(&latest.assets, arch).is_some() =>
        {
            report.send(UpdateMsg::Newer(latest.clone()));
            latest
        }
        _ => offered,
    };
    let asset = pick_app_archive(&release.assets, arch)
        .cloned()
        .ok_or_else(|| tr("This release has no app archive for this Mac.").to_string())?;
    // A private folder (0700), deleted when this function returns.
    let work = tempfile::Builder::new().prefix("merpati-update-").tempdir().map_err(|e| e.to_string())?;
    let team = blocking(move || Ok(team_of(bundle))).await?;
    let app = prepare(client, policy, &release, &asset, work.path(), team.clone(), report).await?;
    report.send(UpdateMsg::Stage(Stage::Installing));
    let version = release.version.clone();
    blocking(move || replace_and_relaunch(&app, bundle, &version, team.as_deref(), &prompt)).await
}

/// Downloads the app archive into `work`, checks its SHA-256, unpacks it
/// and checks the app in it. Returns the new app.
async fn prepare(
    client: &reqwest::Client,
    policy: Policy,
    release: &Release,
    asset: &Asset,
    work: &Path,
    team: Option<String>,
    report: &Reporter,
) -> Result<PathBuf, String> {
    let sha256 = expected_sha256(client, release, asset, policy).await?;
    let archive = work.join("update.zip");
    download_to(client, asset, &archive, &sha256, policy, report).await?;
    let (work, version) = (work.to_path_buf(), release.version.clone());
    blocking(move || unpack_and_verify(&archive, &work, &version, team.as_deref())).await
}

/// The signing team of `bundle` (`None`: ad hoc, unsigned, not macOS).
fn team_of(bundle: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        macos::team_id(bundle)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bundle;
        None
    }
}

/// Unpacks the app archive and checks the app in it (bundle id, version,
/// signature, signing team).
fn unpack_and_verify(archive: &Path, work: &Path, version: &str, team: Option<&str>) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let app = macos::unpack(archive, work)?;
        macos::verify_bundle(&app, macos::BUNDLE_ID, version, team)?;
        Ok(app)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (archive, work, version, team);
        Err(tr("Installing updates in place needs macOS.").to_string())
    }
}

/// Puts the checked `app` in place of `bundle` and has it opened once
/// Merpati has quit.
fn replace_and_relaunch(
    app: &Path,
    bundle: &Path,
    version: &str,
    team: Option<&str>,
    prompt: &str,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::replace(app, bundle, version, team, prompt)?;
        macos::relaunch_after_exit(bundle)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, bundle, version, team, prompt);
        Err(tr("Installing updates in place needs macOS.").to_string())
    }
}

/// What the pop-up's main button does: install and restart, download the
/// installer, or open the release page.
pub fn press_primary(app: &mut MerpatiApp) {
    let Some(release) = &app.update.available else { return };
    if app.update.stage.is_some() {
        return;
    }
    match plan_here(release) {
        Plan::Install(_) => start_install(app),
        Plan::Download(_) => start_download(app),
        Plan::Page => {
            crate::widgets::open_web_link(&release.page_url);
        }
    }
}

/// Applies a message from the background tasks.
pub fn on_msg(app: &mut MerpatiApp, msg: UpdateMsg) {
    match msg {
        UpdateMsg::Checked { manual, result } => {
            app.update.checking = false;
            match result {
                // A download or install is under way; it goes on.
                Ok(Some(_)) if app.update.stage.is_some() => app.update.dialog_open |= manual,
                Ok(Some(release)) => {
                    let skipped = !manual && release.version == app.settings.updates.skipped_version;
                    if app.update.available.as_ref().is_some_and(|r| r.version != release.version) {
                        // What was downloaded or failed belongs to the older release.
                        app.update.downloaded = None;
                        app.update.error = None;
                    }
                    app.update.dialog_open = !skipped;
                    app.update.available = Some(release);
                }
                Ok(None) => {
                    if manual {
                        app.toast(trf("Merpati {} is the latest version.", &[&CURRENT]), false);
                    }
                }
                Err(e) => {
                    tracing::info!("update check failed: {e}");
                    if manual {
                        app.toast(trf("Could not check for updates: {}", &[&e]), true);
                    }
                }
            }
        }
        UpdateMsg::Newer(release) => {
            if app.update.stage.is_some() {
                app.update.available = Some(release);
            }
        }
        UpdateMsg::Stage(stage) => {
            if app.update.stage.is_some() {
                app.update.stage = Some(stage);
            }
        }
        UpdateMsg::Downloaded(result) => {
            app.update.stage = None;
            match result {
                // An AppImage takes the place of the running one and restarts.
                #[cfg(target_os = "linux")]
                Ok(path) if Install::current() == Install::AppImage => match appimage::install(&path) {
                    Ok(()) => {
                        app.update.stage = Some(Stage::Restarting);
                        app.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Err(e) => {
                        app.update.downloaded = Some(path);
                        app.update.error = Some(e);
                        app.update.dialog_open = true;
                    }
                },
                Ok(path) => app.update.downloaded = Some(path),
                Err(e) => {
                    app.update.error = Some(e);
                    app.update.dialog_open = true;
                }
            }
        }
        UpdateMsg::Installed(Ok(())) => {
            app.update.stage = Some(Stage::Restarting);
            // Quit the normal way: `on_exit` saves the session, the
            // workspace and the settings; then the helper opens the new app.
            app.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        UpdateMsg::Installed(Err(e)) => {
            tracing::warn!("update not installed: {e}");
            app.update.stage = None;
            app.update.error = Some(e);
            app.update.dialog_open = true;
        }
    }
}

/// Asks for an update check on the next frame (settings, command palette).
pub fn request_check(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::unique(CHECK_NOW), true));
}

/// Starts a check the settings dialog asked for (see [`settings_ui`]).
pub fn poll_requests(app: &mut MerpatiApp, ctx: &egui::Context) {
    let id = egui::Id::unique(CHECK_NOW);
    if ctx.data_mut(|d| d.remove_temp::<bool>(id)).unwrap_or(false) {
        check(app, true);
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

/// A small status-bar pill while a newer version is waiting.
pub fn status_pill(app: &mut MerpatiApp, ui: &mut egui::Ui) {
    let Some(r) = &app.update.available else { return };
    if app.update.dialog_open {
        return;
    }
    let p = theme::pal(ui);
    let text = match app.update.stage {
        Some(_) => tr("Updating…").to_string(),
        None => trf("Update {}", &[&r.version]),
    };
    let label = format!("{} {}", icons::ARROW_LINE_DOWN, text);
    let resp = ui
        .add(egui::Button::new(RichText::new(label).small().color(p.accent_text)).frame(false))
        .on_hover_text(tr("A new version of Merpati is available"));
    if resp.clicked() {
        app.update.dialog_open = true;
    }
}

/// Release notes in Markdown, drawn simply: headings, bullets, paragraphs.
fn notes_ui(ui: &mut egui::Ui, notes: &str) {
    let p = theme::pal(ui);
    let clean = |s: &str| s.replace("**", "").replace('`', "");
    for line in notes.lines() {
        let t = line.trim();
        if t.is_empty() {
            ui.add_space(4.0);
        } else if let Some(h) = t.strip_prefix('#') {
            ui.add_space(4.0);
            ui.label(RichText::new(clean(h.trim_start_matches('#').trim())).font(theme::semibold(13.5)).color(p.text));
        } else if let Some(b) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("•").color(p.text_muted));
                ui.label(RichText::new(clean(b)).color(p.text));
            });
        } else {
            ui.label(RichText::new(clean(t)).color(p.text));
        }
    }
}

/// Download › Verify › Install, with the current step spinning.
fn steps_ui(ui: &mut egui::Ui, stage: Stage) {
    let p = theme::pal(ui);
    let current = match stage {
        Stage::Starting | Stage::Downloading { .. } => 0,
        Stage::Verifying => 1,
        Stage::Installing => 2,
        Stage::Restarting => 3,
    };
    ui.horizontal(|ui| {
        for (i, label) in [tr("Download"), tr("Verify"), tr("Install")].into_iter().enumerate() {
            if i > 0 {
                ui.add_space(2.0);
                ui.label(RichText::new(icons::CARET_RIGHT).font(theme::icon_font(12.0)).color(p.text_faint));
                ui.add_space(2.0);
            }
            if i == current {
                ui.add(egui::Spinner::new().size(14.0).color(p.accent));
            } else {
                let (icon, color) =
                    if i < current { (icons::CHECK_CIRCLE, p.ok) } else { (icons::CIRCLE, p.text_faint) };
                ui.label(RichText::new(icon).font(theme::icon_font(15.0)).color(color));
            }
            ui.label(RichText::new(label).color(if i <= current { p.text } else { p.text_muted }));
        }
    });
}

/// The progress of a download (`install`: and of the install after it).
fn progress_ui(ui: &mut egui::Ui, stage: Stage, install: bool) {
    let p = theme::pal(ui);
    if install {
        steps_ui(ui, stage);
        ui.add_space(8.0);
    }
    let frac = match stage {
        Stage::Starting => 0.0,
        Stage::Downloading { received, total } => total.map(|t| received as f32 / t.max(1) as f32).unwrap_or(0.0),
        _ => 1.0,
    };
    ui.add(egui::ProgressBar::new(frac).desired_height(8.0).fill(p.accent));
    let text = match stage {
        Stage::Starting => tr("Looking for the newest version…").to_string(),
        Stage::Downloading { received, total } => trf(
            "Downloading… {} of {}",
            &[
                &merpati_core::util::human_bytes(received),
                &total.map(merpati_core::util::human_bytes).unwrap_or_else(|| "?".into()),
            ],
        ),
        Stage::Verifying if install => tr("Checking the checksum and the signature…").to_string(),
        Stage::Verifying => tr("Checking the checksum…").to_string(),
        Stage::Installing => tr("Replacing Merpati with the new version…").to_string(),
        Stage::Restarting => tr("Merpati is restarting…").to_string(),
    };
    ui.label(kit::hint(ui, &text));
}

/// The pop-up, when a newer version is available and not dismissed.
pub fn dialog(app: &mut MerpatiApp, ctx: &egui::Context) {
    if !app.update.dialog_open {
        return;
    }
    let Some(release) = app.update.available.clone() else { return };
    #[derive(PartialEq)]
    enum Act {
        None,
        Later,
        Skip,
        Page,
        Primary,
        Open,
    }
    let mut act = Act::None;
    let plan = plan_here(&release);
    let install = matches!(plan, Plan::Install(_))
        || matches!(plan, Plan::Download(_)) && Install::current() == Install::AppImage;
    let stage = app.update.stage;
    let modal = egui::Modal::new(egui::Id::unique("update-available")).show(ctx, |ui| {
        let p = theme::pal(ui);
        ui.set_width(560.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::Image::new(egui::include_image!("../assets/icon-256.png"))
                    .fit_to_exact_size(egui::vec2(52.0, 52.0))
                    .corner_radius(12),
            );
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(tr("A new version of Merpati is available")).font(theme::semibold(17.0)));
                let mut sub = trf("{} is ready — you have {}.", &[&release.name, &CURRENT]);
                if let Some(d) = &release.published {
                    sub.push_str(&format!("  ·  {d}"));
                }
                ui.label(RichText::new(sub).color(p.text_muted));
            });
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(kit::section_title(tr("What's new")));
            if !release.page_url.is_empty() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if kit::ghost(ui, Some(icons::ARROW_SQUARE_OUT), tr("Release notes")).clicked() {
                        act = Act::Page;
                    }
                });
            }
        });
        ui.add_space(4.0);
        kit::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().max_height(240.0).auto_shrink([false, true]).show(ui, |ui| {
                if release.notes.is_empty() {
                    ui.label(RichText::new(tr("See the release page for the changes.")).color(p.text_muted));
                } else {
                    notes_ui(ui, &release.notes);
                }
            });
        });
        if let Some(stage) = stage {
            ui.add_space(12.0);
            progress_ui(ui, stage, install);
        }
        if let Some(path) = &app.update.downloaded {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::CHECK_CIRCLE).font(theme::icon_font(18.0)).color(p.ok));
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ui.label(RichText::new(trf("Downloaded {}", &[&name])).color(p.text));
            });
            let how = if cfg!(target_os = "macos") {
                tr("Open it, drag Merpati to Applications to replace this version, then start Merpati again.")
            } else {
                tr("Open it and replace this version with the new one, then start Merpati again.")
            };
            ui.label(kit::hint(ui, how));
        }
        if let Some(e) = &app.update.error {
            ui.add_space(10.0);
            let title = if install { tr("The update was not installed") } else { tr("Download failed") };
            crate::proto_common::error_banner(ui, title, e, |_| {});
            if install {
                ui.add_space(4.0);
                ui.label(kit::hint(
                    ui,
                    tr("Merpati was not changed. Try again, or get the new version from the release page."),
                ));
            }
        } else if install && stage.is_none() {
            ui.add_space(10.0);
            ui.label(kit::hint(ui, tr("Merpati quits, installs the new version and opens again with your tabs.")));
        }
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let skip = ui.add_enabled_ui(stage.is_none(), |ui| kit::ghost(ui, None, tr("Skip this version"))).inner;
            if skip.clicked() {
                act = Act::Skip;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if stage.is_some() {
                    let (icon, label) = if install {
                        (icons::ARROWS_CLOCKWISE, tr("Installing…"))
                    } else {
                        (icons::DOWNLOAD_SIMPLE, tr("Downloading…"))
                    };
                    ui.add_enabled_ui(false, |ui| kit::primary_icon(ui, icon, label));
                } else if app.update.downloaded.is_some() {
                    if kit::primary_icon(ui, icons::ARROW_SQUARE_OUT, tr("Open installer")).clicked() {
                        act = Act::Open;
                    }
                } else {
                    let (icon, label) = match plan {
                        Plan::Install(_) => (icons::ARROWS_CLOCKWISE, tr("Install and restart")),
                        Plan::Download(_) => (icons::DOWNLOAD_SIMPLE, tr("Download and install")),
                        Plan::Page => (icons::ARROW_SQUARE_OUT, tr("Get the update")),
                    };
                    if kit::primary_icon(ui, icon, label).clicked() {
                        act = Act::Primary;
                    }
                }
                if kit::secondary(ui, None, tr("Remind me later")).clicked() {
                    act = Act::Later;
                }
            });
        });
    });
    if modal.should_close() && act == Act::None {
        act = Act::Later;
    }
    match act {
        Act::None => {}
        Act::Later => app.update.dialog_open = false,
        Act::Skip => {
            app.settings.updates.skipped_version = release.version.clone();
            app.update.dialog_open = false;
            app.update.available = None;
        }
        Act::Page => {
            crate::widgets::open_web_link(&release.page_url);
        }
        Act::Primary => press_primary(app),
        Act::Open => {
            if let Some(path) = &app.update.downloaded {
                let _ = open::that(path);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// macOS: checking and swapping app bundles
// ---------------------------------------------------------------------------

/// macOS's own tools do the work (`ditto`, `codesign`, `plutil`), always
/// with fixed arguments; nothing from the archive runs.
#[cfg(target_os = "macos")]
mod macos {
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use crate::i18n::{tr, trf};

    /// Merpati's bundle identifier (`packaging/macos/Info.plist`).
    pub const BUNDLE_ID: &str = "dev.merpati.app";
    /// Name prefixes, next to the app, of the old app while the new one
    /// moves in, and of the new one while it is copied there.
    const ASIDE: &str = ".Merpati.app.old-";
    const STAGED: &str = ".Merpati.app.new-";

    /// Runs `cmd` without input or output; true when it succeeded.
    fn quiet(cmd: &mut Command) -> bool {
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    pub fn plist_value(app: &Path, key: &str) -> Option<String> {
        let out = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(app.join("Contents/Info.plist"))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// The signing team of an app (`None`: ad hoc or not signed).
    pub fn team_id(app: &Path) -> Option<String> {
        let out = Command::new("/usr/bin/codesign")
            .arg("-dv")
            .arg(app)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find_map(|l| l.strip_prefix("TeamIdentifier="))
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "not set")
            .map(str::to_string)
    }

    pub fn signature_valid(app: &Path) -> bool {
        quiet(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(app))
    }

    /// Unpacks the app archive into `<work>/unpacked`; returns the
    /// `Merpati.app` in it.
    pub fn unpack(archive: &Path, work: &Path) -> Result<PathBuf, String> {
        let dir = work.join("unpacked");
        if !quiet(Command::new("/usr/bin/ditto").args(["-x", "-k"]).arg(archive).arg(&dir)) {
            return Err(tr("The archive could not be unpacked.").into());
        }
        let app = dir.join("Merpati.app");
        // A real folder, not a link to somewhere else.
        match std::fs::symlink_metadata(&app) {
            Ok(m) if m.is_dir() => Ok(app),
            _ => Err(tr("The archive does not contain Merpati.app.").into()),
        }
    }

    /// Checks an app before it replaces this one: bundle `id`, `version`, a
    /// valid signature and, when this build is signed by a team, the same
    /// `team` (an ad-hoc signed update is accepted only by an ad-hoc build).
    pub fn verify_bundle(app: &Path, id: &str, version: &str, team: Option<&str>) -> Result<(), String> {
        if plist_value(app, "CFBundleIdentifier").as_deref() != Some(id) {
            return Err(tr("The download is not Merpati.").into());
        }
        if plist_value(app, "CFBundleShortVersionString").as_deref() != Some(version) {
            return Err(tr("The downloaded version does not match the release.").into());
        }
        if !signature_valid(app) {
            return Err(tr("The downloaded app's signature is not valid.").into());
        }
        if let Some(team) = team
            && team_id(app).as_deref() != Some(team)
        {
            return Err(tr("The downloaded app is signed by someone else.").into());
        }
        Ok(())
    }

    /// Whether this user can replace `bundle` (move it, delete the old copy)
    /// without an administrator password.
    fn user_can_replace(bundle: &Path) -> bool {
        let writable = |p: &Path| quiet(Command::new("/bin/test").arg("-w").arg(p));
        bundle.parent().is_some_and(writable) && writable(bundle) && writable(&bundle.join("Contents"))
    }

    /// Puts `new` in place of `bundle`: as this user, or as administrator
    /// (macOS asks for the password) when `bundle` or its folder isn't
    /// writable. A bundle already at `version` or newer (installed another
    /// way while Merpati ran) is left alone.
    pub fn replace(new: &Path, bundle: &Path, version: &str, team: Option<&str>, prompt: &str) -> Result<(), String> {
        if let Some(installed) = plist_value(bundle, "CFBundleShortVersionString")
            && super::compare_versions(version, &installed) != std::cmp::Ordering::Greater
        {
            return Err(trf("Merpati {} is already installed. Quit Merpati and open it again.", &[&installed]));
        }
        if user_can_replace(bundle) {
            swap_bundle(new, bundle, |from, to| std::fs::rename(from, to))
        } else {
            admin_swap(new, bundle, version, team, prompt)
        }
    }

    fn unique_suffix() -> String {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        format!("{}-{nanos}", std::process::id())
    }

    /// Deletes `path`; a link is removed itself, never followed.
    fn remove(path: &Path) {
        let _ = match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(_) => Ok(()),
        };
    }

    /// Replaces the app at `bundle` with a copy of `new`. The copy is made
    /// next to `bundle` (same volume, so the moves are renames) and checked;
    /// then `bundle` is moved aside, the copy is moved in with `move_in`, and
    /// the old app is moved back when that fails. The old app, and copies an
    /// interrupted update left, are deleted at the end.
    pub fn swap_bundle(
        new: &Path,
        bundle: &Path,
        move_in: impl Fn(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<(), String> {
        let not_found = || tr("The installed app was not found.").to_string();
        let dir = bundle.parent().ok_or_else(not_found)?;
        if !std::fs::symlink_metadata(bundle).is_ok_and(|m| m.is_dir()) {
            return Err(not_found());
        }
        let suffix = unique_suffix();
        let staged = dir.join(format!("{STAGED}{suffix}"));
        let aside = dir.join(format!("{ASIDE}{suffix}"));
        // No resource forks, extended attributes or ACLs from the archive; no
        // set-id bits and nothing writable by others.
        let copied =
            quiet(Command::new("/usr/bin/ditto").args(["--norsrc", "--noextattr", "--noacl"]).arg(new).arg(&staged))
                && quiet(Command::new("/bin/chmod").args(["-R", "u+rwX,go+rX,go-w,a-st"]).arg(&staged))
                && signature_valid(&staged);
        if !copied {
            remove(&staged);
            return Err(tr("The update could not be copied.").into());
        }
        if let Err(e) = std::fs::rename(bundle, &aside) {
            tracing::warn!("moving {} aside: {e}", bundle.display());
            remove(&staged);
            return Err(tr("The installed app could not be moved aside.").into());
        }
        if let Err(e) = move_in(&staged, bundle) {
            tracing::warn!("moving the update to {}: {e}", bundle.display());
            remove(bundle);
            let restored = std::fs::rename(&aside, bundle).is_ok();
            remove(&staged);
            return Err(if restored {
                tr("The new version could not be moved into place.").to_string()
            } else {
                trf("The new version could not be moved into place; the previous one is in {}", &[&aside.display()])
            });
        }
        remove(&aside);
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(ASIDE) || name.starts_with(STAGED) {
                    remove(&entry.path());
                }
            }
        }
        Ok(())
    }

    /// The swap as administrator: replaces the app (`$2`) with the
    /// downloaded one (`$1`). The download is writable by the user, so it is
    /// first copied into a folder only root can write, and everything is
    /// checked on that copy: signature, bundle id (`$5`), version (`$3`) and,
    /// when set, signing team (`$4`); the copy gets the owner of the app it
    /// replaces. The old app is moved aside and put back when the new one
    /// can't be moved in. The last line of a failure is the reason, in the
    /// same words the app translates.
    pub const SWAP_SCRIPT: &str = r#"
fail() { echo "$1" >&2; exit 1; }
src=$1 app=$2 version=$3 team=$4 id=$5
umask 022
[ -d "$app" ] || fail "The installed app was not found."
stage=$(/usr/bin/mktemp -d /private/tmp/merpati-update.XXXXXXXX) || fail "The update could not be copied."
trap '/bin/rm -rf "$stage"' EXIT
new=$stage/Merpati.app
/usr/bin/ditto --norsrc --noextattr --noacl "$src" "$new" 2>/dev/null || fail "The update could not be copied."
if [ "$(/usr/bin/id -u)" = 0 ]; then
    owner=$(/usr/bin/stat -f %u:%g "$app") && /usr/sbin/chown -R "$owner" "$new" || fail "The update could not be copied."
fi
/bin/chmod -R u+rwX,go+rX,go-w,a-st "$new" || fail "The update could not be copied."
/usr/bin/codesign --verify --deep --strict "$new" 2>/dev/null || fail "The downloaded app's signature is not valid."
plist() { /usr/bin/plutil -extract "$1" raw -o - "$new/Contents/Info.plist" 2>/dev/null; }
[ "$(plist CFBundleIdentifier)" = "$id" ] || fail "The download is not Merpati."
[ "$(plist CFBundleShortVersionString)" = "$version" ] || fail "The downloaded version does not match the release."
if [ -n "$team" ]; then
    signer=$(/usr/bin/codesign -dv "$new" 2>&1 | /usr/bin/sed -n 's/^TeamIdentifier=//p')
    [ "$signer" = "$team" ] || fail "The downloaded app is signed by someone else."
fi
dir=$(/usr/bin/dirname "$app")
old=$(/usr/bin/mktemp -u "$dir/.Merpati.app.old-XXXXXXXX") || fail "The installed app could not be moved aside."
# -h: never move into a directory a link points to.
/bin/mv -h "$app" "$old" 2>/dev/null || fail "The installed app could not be moved aside."
if ! /bin/mv -h "$new" "$app" 2>/dev/null; then
    /bin/rm -rf "$app"
    /bin/mv -h "$old" "$app" 2>/dev/null || fail "The new version could not be moved into place; the previous one is in $old"
    fail "The new version could not be moved into place."
fi
/bin/rm -rf "$old"
for leftover in "$dir"/.Merpati.app.old-* "$dir"/.Merpati.app.new-*; do
    if [ -e "$leftover" ] || [ -L "$leftover" ]; then /bin/rm -rf "$leftover"; fi
done
exit 0
"#;

    /// Runs the swap script (item 2) as root with the remaining items as its
    /// arguments, after macOS asked for an administrator password with the
    /// prompt in item 1; nothing changes if it is declined. AppleScript
    /// quotes every argument.
    pub const ADMIN_APPLESCRIPT: &str = r#"on run argv
    set cmd to "/bin/sh -c " & quoted form of (item 2 of argv) & " merpati-update"
    repeat with arg in rest of rest of argv
        set cmd to cmd & " " & quoted form of (contents of arg)
    end repeat
    do shell script cmd with prompt (item 1 of argv) with administrator privileges
end run"#;

    fn admin_swap(new: &Path, bundle: &Path, version: &str, team: Option<&str>, prompt: &str) -> Result<(), String> {
        let out = Command::new("/usr/bin/osascript")
            .args(["-e", ADMIN_APPLESCRIPT, prompt, SWAP_SCRIPT])
            .arg(new)
            .arg(bundle)
            .arg(version)
            .arg(team.unwrap_or(""))
            .arg(BUNDLE_ID)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() { Ok(()) } else { Err(admin_error(&String::from_utf8_lossy(&out.stderr))) }
    }

    /// The reason from osascript's report (`<range>: execution error:
    /// <message> (<number>)`): the password prompt was dismissed (-128), or
    /// the swap's last line (osascript joins lines with `\r`).
    pub fn admin_error(stderr: &str) -> String {
        let mut message = stderr.rsplit_once("execution error: ").map_or(stderr, |(_, m)| m).trim();
        if let Some((rest, number)) = message.rsplit_once(" (")
            && let Some(number) = number.strip_suffix(')')
            && let Ok(number) = number.parse::<i64>()
        {
            if number == -128 {
                return tr("No administrator password was entered, so Merpati was not changed.").into();
            }
            message = rest;
        }
        let reason = message.split(['\r', '\n']).map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("unknown error");
        tr(reason).chars().take(300).collect()
    }

    /// Settings overrides the new version keeps (test setups). `open` passes
    /// its environment on as well; these are named so they survive even
    /// where it doesn't.
    const KEPT_ENV: &[&str] =
        &["MERPATI_CONFIG_DIR", "MERPATI_SECRETS", "MERPATI_UPDATE_URL", "MERPATI_DOWNLOAD_DIR", "RUST_LOG"];
    /// Never passed on: an automation script runs once, not again in the new
    /// version.
    const DROPPED_ENV: &[&str] = &["MERPATI_AUTOMATION"];

    /// Waits for process `$1` to exit, then runs `open` with the other
    /// arguments.
    pub const RELAUNCH_SCRIPT: &str =
        r#"pid=$1; shift; while /bin/kill -0 "$pid" 2>/dev/null; do /bin/sleep 0.2; done; exec /usr/bin/open "$@""#;

    /// Opens `bundle` once this process has exited, from a detached helper.
    /// The process id, the environment to keep and the path are arguments of
    /// a fixed script.
    pub fn relaunch_after_exit(bundle: &Path) -> Result<(), String> {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", RELAUNCH_SCRIPT, "merpati-relaunch"]).arg(std::process::id().to_string()).arg("-n");
        for key in DROPPED_ENV {
            cmd.env_remove(key);
        }
        for key in KEPT_ENV {
            if let Some(value) = std::env::var_os(key) {
                let mut pair = std::ffi::OsString::from(format!("{key}="));
                pair.push(value);
                cmd.arg("--env").arg(pair);
            }
        }
        cmd.arg(bundle)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map(drop)
            .map_err(|e| trf("Merpati could not be restarted: {}", &[&e]))
    }
}

// ---------------------------------------------------------------------------
// Linux: replacing an AppImage
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod appimage {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::CommandExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use crate::i18n::trf;

    /// Waits for process `$1` to exit, then runs the AppImage `$2`.
    const RELAUNCH_SCRIPT: &str =
        r#"pid=$1; while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done; exec "$2" >/dev/null 2>&1"#;

    /// Puts the verified `downloaded` AppImage in place of the running one
    /// (`$APPIMAGE`) and starts it once this process has exited.
    pub fn install(downloaded: &Path) -> Result<(), String> {
        let running = std::env::var_os("APPIMAGE").ok_or("APPIMAGE is not set")?;
        let running = Path::new(&running);
        replace(downloaded, running).map_err(|e| {
            trf(
                "{} could not be replaced ({}). The new version is in {}.",
                &[&running.display(), &e, &downloaded.display()],
            )
        })?;
        let _ = std::fs::remove_file(downloaded);
        relaunch_after_exit(running).map_err(|e| trf("Merpati could not be restarted: {}", &[&e]))
    }

    /// Copies `new` next to `running` (same folder, so the swap is a rename),
    /// makes it executable and moves it over `running`.
    pub(super) fn replace(new: &Path, running: &Path) -> std::io::Result<()> {
        let dir = running.parent().ok_or(std::io::ErrorKind::NotFound)?;
        let name = running.file_name().ok_or(std::io::ErrorKind::NotFound)?.to_string_lossy();
        let staged = dir.join(format!(".{name}.update"));
        let result = std::fs::copy(new, &staged)
            .and_then(|_| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
            .and_then(|_| std::fs::rename(&staged, running));
        if result.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        result
    }

    fn relaunch_after_exit(app: &Path) -> std::io::Result<()> {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", RELAUNCH_SCRIPT, "merpati-relaunch"]).arg(std::process::id().to_string()).arg(app);
        // The new AppImage sets its own; automation must not run again.
        for key in ["MERPATI_AUTOMATION", "APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
            cmd.env_remove(key);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn().map(drop)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_running_appimage_is_replaced_by_a_rename() {
            let dir = tempfile::tempdir().unwrap();
            let running = dir.path().join("Merpati.AppImage");
            let new = dir.path().join("download.AppImage");
            std::fs::write(&running, b"old").unwrap();
            std::fs::write(&new, b"new").unwrap();
            replace(&new, &running).unwrap();
            assert_eq!(std::fs::read(&running).unwrap(), b"new");
            assert_eq!(std::fs::metadata(&running).unwrap().permissions().mode() & 0o777, 0o755);
            assert!(!dir.path().join(".Merpati.AppImage.update").exists());
            // A folder that is missing leaves nothing behind.
            assert!(replace(&new, &dir.path().join("gone/Merpati.AppImage")).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn updates_fetch_the_same_kind_of_file_as_the_install() {
        let names = [
            "Merpati-0.3.0-universal.dmg",
            "Merpati-0.3.0-macos-universal.zip",
            "Merpati-Setup-0.3.0-x64.exe",
            "merpati-0.3.0-windows-x86_64.zip",
            "merpati_0.3.0_amd64.deb",
            "merpati_0.3.0_arm64.deb",
            "merpati-0.3.0-1.x86_64.rpm",
            "merpati-0.3.0-1.aarch64.rpm",
            "Merpati-0.3.0-x86_64.AppImage",
            "Merpati-0.3.0-aarch64.AppImage",
            "merpati-0.3.0-linux-x86_64.tar.gz",
            "merpati-0.3.0-linux-aarch64.tar.gz",
            "merpati-cli-0.3.0-macos-universal.tar.gz",
            "SHA256SUMS-linux.txt",
        ];
        let assets: Vec<Asset> =
            names.iter().map(|n| Asset { name: n.to_string(), url: String::new(), ..Default::default() }).collect();
        let pick = |install, arch| pick_asset(&assets, install, arch).map(|a| a.name.as_str());
        assert_eq!(pick(Install::Mac, "aarch64"), Some("Merpati-0.3.0-universal.dmg"));
        assert_eq!(pick(Install::WindowsSetup, "x86_64"), Some("Merpati-Setup-0.3.0-x64.exe"));
        assert_eq!(pick(Install::WindowsZip, "x86_64"), Some("merpati-0.3.0-windows-x86_64.zip"));
        assert_eq!(pick(Install::Deb, "x86_64"), Some("merpati_0.3.0_amd64.deb"));
        assert_eq!(pick(Install::Deb, "aarch64"), Some("merpati_0.3.0_arm64.deb"));
        assert_eq!(pick(Install::Rpm, "aarch64"), Some("merpati-0.3.0-1.aarch64.rpm"));
        assert_eq!(pick(Install::AppImage, "x86_64"), Some("Merpati-0.3.0-x86_64.AppImage"));
        assert_eq!(pick(Install::LinuxArchive, "aarch64"), Some("merpati-0.3.0-linux-aarch64.tar.gz"));
        // A .deb install falls back to the AppImage when a release has no .deb.
        let only_appimage = &assets[8..10];
        assert_eq!(
            pick_asset(only_appimage, Install::Deb, "x86_64").map(|a| a.name.as_str()),
            Some("Merpati-0.3.0-x86_64.AppImage")
        );
    }

    #[test]
    fn the_kind_of_install_is_recognised() {
        let none = |_: &Path| false;
        let usr = Some(Path::new("/usr/bin/merpati-app"));
        let home = Some(Path::new("/home/a/merpati-0.3.0/merpati-app"));
        assert_eq!(Install::detect("macos", None, false, none), Install::Mac);
        assert_eq!(Install::detect("linux", home, true, none), Install::AppImage);
        assert_eq!(Install::detect("linux", usr, false, |p| p == Path::new("/etc/debian_version")), Install::Deb);
        assert_eq!(Install::detect("linux", usr, false, |p| p == Path::new("/etc/fedora-release")), Install::Rpm);
        assert_eq!(Install::detect("linux", home, false, |_| true), Install::LinuxArchive);
        let exe = Path::new("C:/Program Files/Merpati/merpati-app.exe");
        assert_eq!(
            Install::detect("windows", Some(exe), false, |p| p.ends_with("unins000.exe")),
            Install::WindowsSetup
        );
        assert_eq!(Install::detect("windows", Some(exe), false, none), Install::WindowsZip);
    }

    #[test]
    fn versions_compare_like_semver() {
        assert_eq!(compare_versions("0.2.0", "0.1.9"), Ordering::Greater);
        assert_eq!(compare_versions("v1.2.10", "1.2.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0", "1.0.0-beta.2"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0-beta.10", "1.0.0-beta.2"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0-alpha", "1.0.0-beta"), Ordering::Less);
        assert_eq!(compare_versions("0.1.0+build.5", "0.1.0"), Ordering::Equal);
    }

    #[test]
    fn github_and_manifest_feeds() {
        let gh = json!({
            "tag_name": "v0.2.0", "name": "Merpati 0.2.0", "body": "## New\n- Faster", "draft": false,
            "html_url": "https://github.com/merpati/merpati/releases/tag/v0.2.0",
            "published_at": "2026-11-02T10:00:00Z",
            "assets": [
                {"name": "Merpati-0.2.0-universal.dmg", "browser_download_url": "https://example.test/u.dmg", "size": 10},
                {"name": "merpati-0.2.0-linux-x86_64.tar.gz", "browser_download_url": "https://example.test/l.tgz"},
                {"name": "merpati-0.2.0-windows-x86_64.zip", "browser_download_url": "https://example.test/w.zip"},
                {"name": "merpati-cli-0.2.0-macos-universal.tar.gz", "browser_download_url": "https://example.test/c.tgz"},
                {"name": "SHA256SUMS-macos.txt", "browser_download_url": "https://example.test/s.txt"}
            ]
        });
        let r = parse_feed(&gh).unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.published.as_deref(), Some("2026-11-02"));
        assert_eq!(pick_asset(&r.assets, Install::Mac, "aarch64").unwrap().name, "Merpati-0.2.0-universal.dmg");
        assert_eq!(
            pick_asset(&r.assets, Install::LinuxArchive, "x86_64").unwrap().name,
            "merpati-0.2.0-linux-x86_64.tar.gz"
        );
        assert_eq!(
            pick_asset(&r.assets, Install::WindowsZip, "x86_64").unwrap().name,
            "merpati-0.2.0-windows-x86_64.zip"
        );
        // Never an installer for another processor.
        assert_eq!(pick_asset(&r.assets, Install::LinuxArchive, "aarch64"), None);
        let list = json!([{"tag_name": "v9", "draft": true}, gh]);
        assert_eq!(parse_feed(&list).unwrap().version, "0.2.0");
        let manifest = json!({"version": "1.0.0", "notes": "Hello", "url": "https://example.test/r",
            "assets": [{"name": "Merpati-1.0.0-arm64.dmg", "url": "https://example.test/a.dmg"},
                       {"name": "Merpati-1.0.0-x86_64.dmg", "url": "https://example.test/x.dmg"}]});
        let m = parse_feed(&manifest).unwrap();
        assert_eq!(m.name, "Merpati 1.0.0");
        assert_eq!(pick_asset(&m.assets, Install::Mac, "aarch64").unwrap().name, "Merpati-1.0.0-arm64.dmg");
        assert_eq!(pick_asset(&m.assets, Install::Mac, "x86_64").unwrap().name, "Merpati-1.0.0-x86_64.dmg");
        assert!(parse_feed(&json!({"name": "x"})).is_err());
        assert!(parse_feed(&json!({"tag_name": "nightly"})).is_err());
    }

    #[test]
    fn digests_are_read_from_the_feed() {
        let hex = "AB".repeat(32);
        let feed = json!({"tag_name": "v0.3.0", "assets": [
            {"name": "a.zip", "browser_download_url": "https://x.test/a", "digest": format!("sha256:{hex}")},
            {"name": "b.zip", "browser_download_url": "https://x.test/b", "digest": "sha512:abcd"},
            {"name": "c.zip", "browser_download_url": "https://x.test/c", "digest": "sha256:1234"},
            {"name": "d.zip", "browser_download_url": "https://x.test/d"},
            {"name": "e.zip", "url": "https://x.test/e", "sha256": "cd".repeat(32)}
        ]});
        let r = parse_feed(&feed).unwrap();
        let digests: Vec<Option<&str>> = r.assets.iter().map(|a| a.sha256.as_deref()).collect();
        let (ab, cd) = ("ab".repeat(32), "cd".repeat(32));
        assert_eq!(digests, [Some(ab.as_str()), None, None, None, Some(cd.as_str())]);
    }

    fn asset(name: &str) -> Asset {
        Asset { name: name.into(), url: format!("https://x.test/{name}"), size: 1, sha256: None }
    }

    #[test]
    fn the_app_archive_is_installed_on_macos() {
        let all: Vec<Asset> = [
            "Merpati-0.3.0-universal.dmg",
            "Merpati-0.3.0-macos-arm64.zip",
            "Merpati-0.3.0-macos-x86_64.zip",
            "merpati-cli-0.3.0-macos-arm64.tar.gz",
            "merpati-0.3.0-windows-x86_64.zip",
            "SHA256SUMS-macos.txt",
        ]
        .map(asset)
        .into();
        let install = |assets: &[Asset], arch: &str| match plan(assets, Install::Mac, arch, true) {
            Plan::Install(a) => Some(a.name),
            _ => None,
        };
        assert_eq!(install(&all, "aarch64").as_deref(), Some("Merpati-0.3.0-macos-arm64.zip"));
        assert_eq!(install(&all, "x86_64").as_deref(), Some("Merpati-0.3.0-macos-x86_64.zip"));
        let universal = ["Merpati-0.3.0-universal.dmg", "Merpati-0.3.0-macos-universal.zip"].map(asset);
        assert_eq!(install(&universal, "aarch64").as_deref(), Some("Merpati-0.3.0-macos-universal.zip"));
        assert_eq!(install(&universal, "x86_64").as_deref(), Some("Merpati-0.3.0-macos-universal.zip"));
        // An archive for the other processor is not installed: the disk image is offered.
        let intel_only = ["Merpati-0.3.0-universal.dmg", "Merpati-0.3.0-macos-x86_64.zip"].map(asset);
        assert_eq!(
            plan(&intel_only, Install::Mac, "aarch64", true),
            Plan::Download(asset("Merpati-0.3.0-universal.dmg"))
        );
        // Not running from an app bundle (cargo run), or no archive: the disk image.
        assert_eq!(plan(&all, Install::Mac, "aarch64", false), Plan::Download(asset("Merpati-0.3.0-universal.dmg")));
        assert_eq!(
            plan(&all[..1], Install::Mac, "aarch64", true),
            Plan::Download(asset("Merpati-0.3.0-universal.dmg"))
        );
        // Other systems never install in place; nothing for them opens the release page.
        assert_eq!(
            plan(&all, Install::WindowsZip, "x86_64", true),
            Plan::Download(asset("merpati-0.3.0-windows-x86_64.zip"))
        );
        assert_eq!(plan(&all, Install::LinuxArchive, "x86_64", true), Plan::Page);
    }

    #[test]
    fn only_https_or_a_feed_on_this_computer() {
        let url = |u: &str| url::Url::parse(u).unwrap();
        let github = Policy::for_feed(merpati_core::settings::DEFAULT_UPDATE_FEED);
        assert!(github.allows(&url("https://objects.githubusercontent.com/x")));
        assert!(!github.allows(&url("http://example.test/x")));
        assert!(!github.allows(&url("http://127.0.0.1:8000/x")), "a remote feed can't send us to plain HTTP");
        assert!(!github.allows(&url("file:///etc/passwd")));
        for feed in ["http://127.0.0.1:8000/latest.json", "http://localhost:1/f", "http://[::1]:9/f"] {
            let local = Policy::for_feed(feed);
            assert!(local.allows(&url("http://127.0.0.1:8000/a.zip")), "{feed}");
            assert!(local.allows(&url("http://localhost:8000/a.zip")), "{feed}");
            assert!(!local.allows(&url("http://example.test/a.zip")), "{feed}");
            assert!(local.allows(&url("https://example.test/a.zip")), "{feed}");
        }
        assert!(!Policy::for_feed("http://example.test/latest.json").allows(&url("http://example.test/a")));
    }

    #[test]
    fn bundles_are_found_from_the_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("Merpati.app/Contents/MacOS/merpati-app");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "").unwrap();
        let bundle = dir.path().join("Merpati.app").canonicalize().unwrap();
        assert_eq!(bundle_of(&exe), Some(bundle));
        let dev = dir.path().join("target/debug/merpati-app");
        std::fs::create_dir_all(dev.parent().unwrap()).unwrap();
        std::fs::write(&dev, "").unwrap();
        assert_eq!(bundle_of(&dev), None);
        // Tests don't run from an app bundle: no install in place.
        assert_eq!(running_bundle(), None);
    }

    #[test]
    fn download_names_stay_in_their_folder() {
        assert_eq!(safe_file_name("Merpati-1.0.0.dmg"), "Merpati-1.0.0.dmg");
        assert_eq!(safe_file_name("../../.ssh/authorized_keys"), "authorized_keys");
        assert_eq!(safe_file_name("..\\x.exe"), "x.exe");
        assert_eq!(safe_file_name(".."), "merpati-update");
        assert_eq!(safe_file_name(""), "merpati-update");
        let dir = tempfile::tempdir().unwrap();
        for name in ["Merpati-9.0.0-universal.dmg", "merpati-9.0.0-linux-x86_64.tar.gz"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let next = |n: &str| unique_path(dir.path(), n).file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(next("Merpati-9.0.0-universal.dmg"), "Merpati-9.0.0-universal (1).dmg");
        assert_eq!(next("merpati-9.0.0-linux-x86_64.tar.gz"), "merpati-9.0.0-linux-x86_64 (1).tar.gz");
        assert_eq!(next("new.zip"), "new.zip");
    }

    #[test]
    fn checksums_are_found_by_file_name() {
        let h = "a".repeat(64);
        let sums = format!("{h}  Merpati-0.2.0-universal.dmg\n{}  *merpati.tgz\n", "B".repeat(64));
        assert_eq!(checksum_for(&sums, "Merpati-0.2.0-universal.dmg"), Some(h));
        assert_eq!(checksum_for(&sums, "merpati.tgz"), Some("b".repeat(64)));
        assert_eq!(checksum_for(&sums, "other"), None);
    }

    #[test]
    fn startup_check_finds_a_newer_release() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let feed = json!({"version": "99.0.0", "notes": "- Big", "url": "https://example.test"}).to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{feed}",
                    feed.len()
                );
            }
        });
        app.settings.updates.feed_url = format!("http://127.0.0.1:{port}/latest");
        check(&mut app, false);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while app.update.checking && std::time::Instant::now() < deadline {
            app.pump_messages();
            std::thread::sleep(Duration::from_millis(20));
        }
        let r = app.update.available.clone().expect("newer release found");
        assert_eq!(r.version, "99.0.0");
        assert!(app.update.dialog_open);
        // A skipped version is not offered again at startup.
        app.settings.updates.skipped_version = "99.0.0".into();
        on_msg(&mut app, UpdateMsg::Checked { manual: false, result: Ok(Some(r)) });
        assert!(!app.update.dialog_open);
    }

    /// Serves `files` (path → body) on a local port, one response per
    /// request; `/redirect/<path>` answers with a redirect to `<path>`.
    fn serve(files: Vec<(&'static str, Vec<u8>)>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for s in listener.incoming() {
                let Ok(mut s) = s else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                if let Some(target) = path.strip_prefix("/redirect/") {
                    let location = if target.starts_with("http") { target.to_string() } else { format!("/{target}") };
                    let _ = write!(
                        s,
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    continue;
                }
                let body = files.iter().find(|(p, _)| *p == path).map(|(_, b)| b.clone());
                let (status, body) = match body {
                    Some(b) => ("200 OK", b),
                    None => ("404 Not Found", Vec::new()),
                };
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(&body);
            }
        });
        port
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn installers_are_downloaded_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        // SAFETY: the only test that reads this variable.
        unsafe { std::env::set_var("MERPATI_DOWNLOAD_DIR", &downloads) };
        let installer = vec![7u8; 300_000];
        let good = sha256_hex(&installer);
        let arch = if std::env::consts::ARCH == "aarch64" { "arm64" } else { "x86_64" };
        let name = match std::env::consts::OS {
            "macos" => "Merpati-9.0.0-universal.dmg".to_string(),
            "windows" => format!("Merpati-9.0.0-windows-{arch}.zip"),
            _ => format!("Merpati-9.0.0-linux-{arch}.tar.gz"),
        };
        let port = serve(vec![
            ("/installer", installer.clone()),
            ("/sums-good", format!("{good}  {name}\n").into_bytes()),
            ("/sums-bad", format!("{}  {name}\n", "0".repeat(64)).into_bytes()),
        ]);
        let mut app = MerpatiApp::for_test(&dir.path().join("ws"));
        // Plain HTTP to this computer is allowed because the feed is here too.
        app.settings.updates.feed_url = format!("http://127.0.0.1:{port}/latest.json");
        let local = format!("http://127.0.0.1:{port}");
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // (installer address, checksum list, published digest, the error expected)
        let cases = [
            (format!("{local}/installer"), Some("/sums-good"), None, None),
            (format!("{local}/installer"), Some("/sums-bad"), None, Some("checksum")),
            (format!("{local}/redirect/installer"), None, Some(good.clone()), None),
            (format!("{local}/installer"), None, Some("1".repeat(64)), Some("checksum")),
            (format!("{local}/installer"), None, None, Some("no checksum")),
            ("http://example.test/installer".to_string(), None, Some(good.clone()), Some("HTTPS")),
            (format!("{local}/redirect/http://example.test/x"), None, Some(good.clone()), Some("HTTPS")),
            // The cause is shown, not only "error sending request".
            (format!("http://127.0.0.1:{closed}/installer"), None, Some(good.clone()), Some("refused")),
        ];
        for (url, sums, digest, error) in cases {
            app.update = UpdateState::default();
            let mut assets = vec![Asset { name: name.clone(), url, size: 0, sha256: digest }];
            if let Some(sums) = sums {
                assets.push(Asset {
                    name: "SHA256SUMS.txt".into(),
                    url: format!("{local}{sums}"),
                    size: 0,
                    sha256: None,
                });
            }
            app.update.available = Some(Release {
                version: "9.0.0".into(),
                name: "Merpati 9.0.0".into(),
                notes: String::new(),
                page_url: String::new(),
                published: None,
                assets,
            });
            start_download(&mut app);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while app.update.stage.is_some() && std::time::Instant::now() < deadline {
                app.pump_messages();
                std::thread::sleep(Duration::from_millis(20));
            }
            match error {
                None => {
                    let path = app.update.downloaded.clone().expect("downloaded");
                    assert_eq!(std::fs::read(&path).unwrap(), installer);
                    assert!(path.starts_with(&downloads));
                }
                Some(error) => {
                    assert!(app.update.downloaded.is_none());
                    assert!(app.update.error.as_deref().unwrap_or("").contains(error), "{:?}", app.update.error);
                }
            }
        }
        // Failed downloads left nothing behind; the good ones got unique names.
        let names: Vec<String> = std::fs::read_dir(&downloads)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&name), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".part")), "{names:?}");
    }

    /// Messages of an install: a newer release replaces the one offered, the
    /// steps are shown, and a failure keeps the pop-up open with the reason.
    #[test]
    fn install_messages_drive_the_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let release = |v: &str| Release {
            version: v.into(),
            name: format!("Merpati {v}"),
            notes: String::new(),
            page_url: String::new(),
            published: None,
            assets: Vec::new(),
        };
        app.update.available = Some(release("9.0.0"));
        // Late progress of a finished job changes nothing.
        on_msg(&mut app, UpdateMsg::Stage(Stage::Verifying));
        assert_eq!(app.update.stage, None);
        app.update.stage = Some(Stage::Starting);
        on_msg(&mut app, UpdateMsg::Newer(release("9.0.1")));
        assert_eq!(app.update.available.as_ref().unwrap().version, "9.0.1");
        on_msg(&mut app, UpdateMsg::Stage(Stage::Installing));
        assert_eq!(app.update.stage, Some(Stage::Installing));
        // A check that ends meanwhile doesn't replace what is being installed.
        on_msg(&mut app, UpdateMsg::Checked { manual: true, result: Ok(Some(release("9.0.2"))) });
        assert_eq!(app.update.available.as_ref().unwrap().version, "9.0.1");
        app.update.dialog_open = false;
        on_msg(&mut app, UpdateMsg::Installed(Err("The download is not Merpati.".into())));
        assert_eq!(app.update.stage, None);
        assert!(app.update.dialog_open);
        assert_eq!(app.update.error.as_deref(), Some("The download is not Merpati."));
        app.update.stage = Some(Stage::Installing);
        on_msg(&mut app, UpdateMsg::Installed(Ok(())));
        assert_eq!(app.update.stage, Some(Stage::Restarting));
    }

    #[cfg(target_os = "macos")]
    mod macos_bundles {
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};

        use super::super::macos::*;
        use super::super::*;
        use super::{serve, sha256_hex};

        /// A small ad-hoc signed bundle (its executable is a copy of `true`).
        fn fake_app(parent: &Path, id: &str, version: &str) -> PathBuf {
            let app = parent.join("Merpati.app");
            std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
            std::fs::copy("/usr/bin/true", app.join("Contents/MacOS/merpati-app")).unwrap();
            std::fs::write(app.join("Contents/Resources/notes.txt"), version).unwrap();
            std::fs::write(
                app.join("Contents/Info.plist"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>merpati-app</string>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
                ),
            )
            .unwrap();
            let signed = Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(&app)
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(signed.success());
            app
        }

        fn version(app: &Path) -> Option<String> {
            plist_value(app, "CFBundleShortVersionString")
        }

        /// What is next to the app besides it.
        fn others(dir: &Path) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != "Merpati.app")
                .collect();
            names.sort();
            names
        }

        #[test]
        fn bundles_are_checked_before_install() {
            let tmp = tempfile::tempdir().unwrap();
            let app = fake_app(tmp.path(), BUNDLE_ID, "9.0.0");
            assert_eq!(verify_bundle(&app, BUNDLE_ID, "9.0.0", None), Ok(()));
            assert_eq!(team_id(&app), None, "ad hoc");
            let err = |r: Result<(), String>| r.unwrap_err();
            assert_eq!(
                err(verify_bundle(&app, BUNDLE_ID, "9.0.1", None)),
                "The downloaded version does not match the release."
            );
            assert_eq!(
                err(verify_bundle(&app, BUNDLE_ID, "9.0.0", Some("ABCDE12345"))),
                "The downloaded app is signed by someone else."
            );
            let other = fake_app(&tmp.path().join("other"), "dev.example.other", "9.0.0");
            assert_eq!(err(verify_bundle(&other, BUNDLE_ID, "9.0.0", None)), "The download is not Merpati.");
            // Changed after it was signed.
            std::fs::write(app.join("Contents/Resources/notes.txt"), "evil").unwrap();
            assert_eq!(
                err(verify_bundle(&app, BUNDLE_ID, "9.0.0", None)),
                "The downloaded app's signature is not valid."
            );
        }

        /// The archive the packaging script makes is downloaded (through a
        /// redirect), checked, unpacked and its app checked.
        #[test]
        fn app_archives_are_downloaded_unpacked_and_checked() {
            let tmp = tempfile::tempdir().unwrap();
            let app = fake_app(&tmp.path().join("build"), BUNDLE_ID, "9.0.0");
            let zip = tmp.path().join("Merpati-9.0.0-macos-universal.zip");
            let zipped = Command::new("/usr/bin/ditto")
                .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
                .arg(&app)
                .arg(&zip)
                .status()
                .unwrap();
            assert!(zipped.success());
            let bytes = std::fs::read(&zip).unwrap();
            let (size, good) = (bytes.len() as u64, sha256_hex(&bytes));
            let port = serve(vec![("/zip", bytes), ("/junk", b"not a zip".to_vec())]);
            let merpati = MerpatiApp::for_test(&tmp.path().join("ws"));
            let client = merpati.http.default_client().unwrap();
            let policy = Policy::for_feed(&format!("http://127.0.0.1:{port}/latest.json"));
            let report = Reporter::of(&merpati);
            let run = |version: &str, path: &str, size: u64, sha256: &str| {
                let release = Release {
                    version: version.into(),
                    name: String::new(),
                    notes: String::new(),
                    page_url: String::new(),
                    published: None,
                    assets: vec![Asset {
                        name: "Merpati-9.0.0-macos-universal.zip".into(),
                        url: format!("http://127.0.0.1:{port}{path}"),
                        size,
                        sha256: Some(sha256.into()),
                    }],
                };
                let work = tempfile::tempdir_in(tmp.path()).unwrap();
                let result = merpati.rt.block_on(prepare(
                    &client,
                    policy,
                    &release,
                    &release.assets[0],
                    work.path(),
                    None,
                    &report,
                ));
                (result, work)
            };
            let (result, work) = run("9.0.0", "/redirect/zip", size, &good);
            let unpacked = result.unwrap();
            assert!(unpacked.starts_with(work.path()));
            assert_eq!(version(&unpacked).as_deref(), Some("9.0.0"));
            assert!(signature_valid(&unpacked));
            let (result, _) = run("9.0.1", "/zip", size, &good);
            assert_eq!(result.unwrap_err(), "The downloaded version does not match the release.");
            let (result, work) = run("9.0.0", "/zip", size, &"0".repeat(64));
            assert!(result.unwrap_err().contains("checksum"));
            assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0, "nothing kept");
            let (result, _) = run("9.0.0", "/zip", size + 1, &good);
            assert!(result.unwrap_err().contains("incomplete"));
            let (result, _) = run("9.0.0", "/junk", 9, &sha256_hex(b"not a zip"));
            assert_eq!(result.unwrap_err(), "The archive could not be unpacked.");
        }

        #[test]
        fn swap_installs_a_copy_and_cleans_up() {
            let tmp = tempfile::tempdir().unwrap();
            let (apps, download) = (tmp.path().join("Applications"), tmp.path().join("dl"));
            let app = fake_app(&apps, BUNDLE_ID, "1.0.0");
            let new = fake_app(&download, BUNDLE_ID, "2.0.0");
            // Left by an update that was interrupted earlier.
            std::fs::create_dir_all(apps.join(".Merpati.app.old-1234/Contents")).unwrap();
            std::fs::write(apps.join("Other.txt"), "x").unwrap();

            assert_eq!(swap_bundle(&new, &app, |a, b| std::fs::rename(a, b)), Ok(()));
            assert_eq!(version(&app).as_deref(), Some("2.0.0"));
            assert!(signature_valid(&app));
            assert_eq!(others(&apps), ["Other.txt"]);
            assert!(new.exists(), "the download is copied, not moved");

            // That version (or a newer one) is there now: never replaced again.
            for version in ["2.0.0", "1.5.0"] {
                assert_eq!(
                    replace(&new, &app, version, None, "prompt").unwrap_err(),
                    "Merpati 2.0.0 is already installed. Quit Merpati and open it again."
                );
            }
            assert_eq!(others(&apps), ["Other.txt"]);
        }

        #[test]
        fn swap_puts_the_old_app_back_when_the_new_one_cannot_move_in() {
            let tmp = tempfile::tempdir().unwrap();
            let (apps, download) = (tmp.path().join("Applications"), tmp.path().join("dl"));
            let app = fake_app(&apps, BUNDLE_ID, "1.0.0");
            let new = fake_app(&download, BUNDLE_ID, "2.0.0");
            let failing = |_: &Path, _: &Path| -> std::io::Result<()> { Err(std::io::Error::other("disk full")) };
            assert_eq!(swap_bundle(&new, &app, failing).unwrap_err(), "The new version could not be moved into place.");
            assert_eq!(version(&app).as_deref(), Some("1.0.0"));
            assert!(signature_valid(&app));
            assert!(others(&apps).is_empty(), "{:?}", others(&apps));

            // A damaged update is not copied in at all.
            std::fs::write(new.join("Contents/Resources/notes.txt"), "evil").unwrap();
            assert_eq!(
                swap_bundle(&new, &app, |a, b| std::fs::rename(a, b)).unwrap_err(),
                "The update could not be copied."
            );
            assert_eq!(version(&app).as_deref(), Some("1.0.0"));
            assert!(others(&apps).is_empty());
            // Nothing to replace.
            let gone = apps.join("Gone.app");
            assert_eq!(
                swap_bundle(&new, &gone, |a, b| std::fs::rename(a, b)).unwrap_err(),
                "The installed app was not found."
            );
        }

        /// Runs the administrator swap script as this user (as the password
        /// prompt would, minus root); returns its stderr.
        fn run_swap(script: &str, new: &Path, app: &Path, version: &str, team: &str) -> String {
            let out = Command::new("/bin/sh")
                .args(["-c", script, "merpati-update"])
                .arg(new)
                .arg(app)
                .args([version, team, BUNDLE_ID])
                .output()
                .unwrap();
            assert_eq!(out.status.success(), out.stderr.is_empty());
            String::from_utf8_lossy(&out.stderr).trim().to_string()
        }

        #[test]
        fn admin_swap_checks_the_copy_and_keeps_the_app_on_failure() {
            let tmp = tempfile::tempdir().unwrap();
            let (apps, download) = (tmp.path().join("Applications"), tmp.path().join("dl"));
            let app = fake_app(&apps, BUNDLE_ID, "1.0.0");
            let new = fake_app(&download, BUNDLE_ID, "2.0.0");
            let kept = |app: &Path| {
                assert_eq!(version(app).as_deref(), Some("1.0.0"));
                assert!(others(&apps).is_empty());
            };
            assert_eq!(
                run_swap(SWAP_SCRIPT, &new, &app, "3.0.0", ""),
                "The downloaded version does not match the release."
            );
            kept(&app);
            assert_eq!(
                run_swap(SWAP_SCRIPT, &new, &app, "2.0.0", "ABCDE12345"),
                "The downloaded app is signed by someone else."
            );
            kept(&app);
            let move_in = r#"/bin/mv -h "$new" "$app" 2>/dev/null"#;
            assert!(SWAP_SCRIPT.contains(move_in));
            let failing = SWAP_SCRIPT.replace(move_in, "false");
            assert_eq!(run_swap(&failing, &new, &app, "2.0.0", ""), "The new version could not be moved into place.");
            kept(&app);
            assert!(signature_valid(&app));

            std::fs::create_dir_all(apps.join(".Merpati.app.old-1234/Contents")).unwrap();
            assert_eq!(run_swap(SWAP_SCRIPT, &new, &app, "2.0.0", ""), "");
            assert_eq!(version(&app).as_deref(), Some("2.0.0"));
            assert!(signature_valid(&app));
            assert!(others(&apps).is_empty());
            assert!(new.exists());

            // Changed after it was checked.
            std::fs::write(new.join("Contents/Resources/notes.txt"), "evil").unwrap();
            assert_eq!(run_swap(SWAP_SCRIPT, &new, &app, "2.0.0", ""), "The downloaded app's signature is not valid.");
        }

        /// The administrator prompt can't be shown in a test; the same
        /// AppleScript without it shows every argument arrives unchanged.
        #[test]
        fn applescript_passes_arguments_verbatim() {
            let privileged = " with prompt (item 1 of argv) with administrator privileges";
            assert!(ADMIN_APPLESCRIPT.contains(privileged));
            let plain = ADMIN_APPLESCRIPT.replace(privileged, "");
            let args = ["it's \"quoted\"", "$HOME `id` \\ ; *", "", "two  spaces"];
            let out = Command::new("/usr/bin/osascript")
                .args(["-e", &plain, "prompt", r#"printf '<%s>' "$@""#])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let expected: String = args.iter().map(|a| format!("<{a}>")).collect();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), expected);
        }

        #[test]
        fn admin_errors_are_readable() {
            assert_eq!(
                admin_error("0:301: execution error: User canceled. (-128)\n"),
                "No administrator password was entered, so Merpati was not changed."
            );
            assert_eq!(
                admin_error("7:158: execution error: noise\rThe download is not Merpati. (1)\n"),
                "The download is not Merpati."
            );
            assert_eq!(admin_error(""), "unknown error");
        }

        /// The helper waits for the process to exit before it runs `open`
        /// (here replaced by `echo` into a file).
        #[test]
        fn relaunch_waits_for_the_process_to_exit() {
            let tmp = tempfile::tempdir().unwrap();
            let out = tmp.path().join("opened");
            let script = RELAUNCH_SCRIPT.replace("exec /usr/bin/open \"$@\"", "echo \"$@\" > \"$OUT\"");
            assert_ne!(script, RELAUNCH_SCRIPT);
            let mut sleeper = Command::new("/bin/sleep").arg("1").spawn().unwrap();
            let mut helper = Command::new("/bin/sh")
                .args(["-c", &script, "merpati-relaunch"])
                .arg(sleeper.id().to_string())
                .args(["-n", "/Apps/Merpati.app"])
                .env("OUT", &out)
                .spawn()
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(!out.exists(), "opened too early");
            sleeper.wait().unwrap();
            helper.wait().unwrap();
            assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "-n /Apps/Merpati.app");
        }
    }
}
