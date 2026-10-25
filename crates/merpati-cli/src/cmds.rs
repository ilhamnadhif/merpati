//! The commands other than `run`/`schedule`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};

use merpati_core::model::{Collection, Item, RequestKind};
use merpati_core::postman::{self, ExportOptions};
use merpati_core::prepared::{PrepareContext, prepare};
use merpati_core::storage::WorkspaceStore;
use merpati_core::variables::{self, Resolver};
use merpati_http::CookieJar;
use merpati_mock::{MockConfig, MockServer};
use merpati_runner::perf::{PerfOptions, run_perf};

use crate::run::cancel_on_ctrl_c;
use crate::source::{self, WsCtx};
use crate::util;

// ---------------------------------------------------------------------------
// mock
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct MockArgs {
    /// Collection file, workspace or collection folder.
    pub source: PathBuf,
    #[arg(short = 'c', long)]
    pub collection: Option<String>,
    /// Port to listen on (0 picks a free port).
    #[arg(short = 'p', long, default_value_t = 3000)]
    pub port: u16,
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Artificial latency in ms.
    #[arg(long, default_value_t = 0)]
    pub delay: u64,
    /// Extra random latency, 0..=N ms per response.
    #[arg(long, default_value_t = 0)]
    pub jitter: u64,
    /// Limit response bodies to this many KB/s.
    #[arg(long, value_name = "KB_PER_SEC")]
    pub bandwidth: Option<u64>,
    /// Network preset: fast-4g, fast-3g, slow-3g or satellite (replaces
    /// --jitter/--bandwidth; --delay still adds up).
    #[arg(long)]
    pub network: Option<String>,
    /// Fail this fraction (0..1) of requests.
    #[arg(long, default_value_t = 0.0)]
    pub fail_rate: f32,
    /// How failures look: an HTTP status code (100-599, e.g. 503), `drop`
    /// (broken connection) or `timeout` (no answer).
    #[arg(long, default_value = "500", value_name = "STATUS|drop|timeout")]
    pub fail_with: merpati_mock::FailureKind,
    /// Environment used to resolve {{variables}} in example URLs and bodies.
    #[arg(short = 'e', long)]
    pub environment: Option<String>,
    /// Do not add CORS headers.
    #[arg(long = "no-cors")]
    pub no_cors: bool,
    /// Do not print incoming requests.
    #[arg(long)]
    pub quiet: bool,
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
}

pub async fn mock(args: MockArgs) -> Result<i32> {
    let loaded = source::load(&args.source, args.collection.as_deref(), source::keychain_allowed(args.no_keychain))?;
    let env = source::resolve_environment(args.environment.as_deref(), loaded.ws.as_ref())?;
    let globals = loaded.ws.as_ref().map(|c| c.ws.globals.clone()).unwrap_or_default();
    let vars = source::scopes(&loaded.collection, env.as_ref(), &globals, &[], &[]);
    let network = match args.network.as_deref() {
        Some(name) => {
            let key = |s: &str| s.to_lowercase().replace([' ', '_'], "-");
            merpati_mock::NetworkConditions::PRESETS
                .iter()
                .find(|(n, _)| key(n) == key(name))
                .map(|(_, c)| *c)
                .ok_or_else(|| {
                    anyhow::anyhow!("unknown network preset {name:?} (fast-4g, fast-3g, slow-3g, satellite)")
                })?
        }
        None => merpati_mock::NetworkConditions {
            latency_ms: 0,
            jitter_ms: args.jitter,
            bandwidth_bytes_per_sec: args.bandwidth.map(|kb| kb.max(1) * 1000),
        },
    };
    let failure = (args.fail_rate > 0.0)
        .then(|| merpati_mock::FailureSimulation { rate: args.fail_rate.clamp(0.0, 1.0), kind: args.fail_with });
    let config = MockConfig {
        host: args.host.clone(),
        port: args.port,
        delay_ms: args.delay,
        network,
        failure,
        variables: vars,
        cors: !args.no_cors,
        ..Default::default()
    };
    let server = MockServer::start(loaded.collection.clone(), config)
        .await
        .with_context(|| format!("could not listen on {}:{}", args.host, args.port))?;
    println!("Mock server for \"{}\" running at {}", loaded.collection.name, server.url());
    let routes = server.routes();
    if routes.is_empty() {
        println!("(no requests with examples; every call will return 404)");
    }
    for r in &routes {
        println!("  {:<7} {:<40} {} [{}]", r.method, r.path, r.request_name, r.examples.join(", "));
    }
    println!("Press Ctrl+C to stop.");
    let cancel = cancel_on_ctrl_c();
    let mut last: Option<(i64, String, String, u64)> = None;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            _ = cancel.cancelled() => break,
        }
        if args.quiet {
            continue;
        }
        let logs = server.logs();
        let key = |e: &merpati_mock::MockLogEntry| {
            (e.timestamp_ms, e.method.clone(), e.path.clone(), e.duration_ms.to_bits())
        };
        let start = match &last {
            Some(l) => logs.iter().rposition(|e| &key(e) == l).map(|i| i + 1).unwrap_or(0),
            None => 0,
        };
        for e in &logs[start..] {
            let time = chrono::DateTime::from_timestamp_millis(e.timestamp_ms)
                .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
                .unwrap_or_default();
            let q = if e.query.is_empty() { String::new() } else { format!("?{}", e.query) };
            println!(
                "{time}  {} {}{q} → {} {}  {}",
                e.method,
                e.path,
                e.status,
                e.matched.as_deref().unwrap_or("(no match)"),
                merpati_core::util::human_ms(e.duration_ms)
            );
        }
        if let Some(e) = logs.last() {
            last = Some(key(e));
        }
    }
    server.stop().await;
    Ok(0)
}

// ---------------------------------------------------------------------------
// init / import / export / convert / validate
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct InitArgs {
    #[arg(default_value = ".")]
    pub dir: PathBuf,
    #[arg(long)]
    pub name: Option<String>,
}

fn dir_name(dir: &Path) -> String {
    dir.canonicalize()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Workspace".into())
}

pub fn init(args: InitArgs) -> Result<i32> {
    std::fs::create_dir_all(&args.dir)?;
    let name = args.name.unwrap_or_else(|| dir_name(&args.dir));
    WorkspaceStore::create(&args.dir, &name)?;
    println!("Created workspace \"{name}\" in {}", args.dir.display());
    Ok(0)
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Postman, OpenAPI/Swagger, Insomnia, HAR, Hoppscotch, Thunder Client, WSDL, Smithy or cURL file.
    /// Several files or directories make one Smithy model (directories: the
    /// `sources` and `imports` of their `smithy-build.json`, otherwise every
    /// `.smithy` file and Smithy JSON AST inside, recursively).
    #[arg(required = true, value_name = "FILE")]
    pub files: Vec<PathBuf>,
    /// Workspace directory (created if missing).
    #[arg(short = 'w', long, default_value = ".")]
    pub workspace: PathBuf,
    /// Rename the imported collection.
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
}

pub fn import(args: ImportArgs) -> Result<i32> {
    let (detected, mut imported) = match args.files.as_slice() {
        [file] if !file.is_dir() => {
            let text = std::fs::read_to_string(file).with_context(|| format!("could not read {}", file.display()))?;
            let format = merpati_convert::detect(&text);
            let imported =
                merpati_convert::import_auto(&text).with_context(|| format!("could not import {}", file.display()))?;
            (format.map(|f| f.label().to_string()), imported)
        }
        paths => import_smithy_model(paths)?,
    };
    if !WorkspaceStore::is_workspace(&args.workspace) {
        std::fs::create_dir_all(&args.workspace)?;
        let name = dir_name(&args.workspace);
        WorkspaceStore::create(&args.workspace, &name)?;
        println!("Created workspace \"{name}\" in {}", args.workspace.display());
    }
    let ctx = source::open_workspace(&args.workspace, source::keychain_allowed(args.no_keychain))?;
    if let (Some(name), [c]) = (&args.name, imported.collections.as_mut_slice()) {
        c.name = name.clone();
    }
    if let Some(f) = detected {
        println!("Detected {f}");
    }
    for c in &imported.collections {
        let existed = ctx.ws.collection(&c.id).is_some();
        ctx.store.save_collection(c, &ctx.secrets)?;
        println!(
            "{} collection \"{}\" ({} requests)",
            if existed { "Updated" } else { "Imported" },
            c.name,
            c.request_count()
        );
    }
    for e in &imported.environments {
        ctx.store.save_environment(e, &ctx.secrets)?;
        println!("Imported environment \"{}\" ({} variables)", e.name, e.variables.len());
    }
    if !imported.globals.is_empty() {
        let mut globals = ctx.ws.globals.clone();
        for g in &imported.globals {
            match globals.iter_mut().find(|x| x.key == g.key) {
                Some(x) => *x = g.clone(),
                None => globals.push(g.clone()),
            }
        }
        ctx.store.save_globals(&globals, &ctx.secrets)?;
        println!("Imported {} global variables", imported.globals.len());
    }
    for w in &imported.warnings {
        eprintln!("warning: {w}");
    }
    Ok(0)
}

/// Several files and/or directories as one Smithy model (FR-D1): files are
/// named by their path relative to the current directory in warnings. The
/// `maven.dependencies` of a `smithy-build.json` are read from the local
/// Maven repository and come after the project's own files.
fn import_smithy_model(paths: &[PathBuf]) -> Result<(Option<String>, merpati_convert::Imported)> {
    use merpati_convert::smithy::SourceFile;
    let mut sources = Vec::new();
    let mut dependencies: Vec<SourceFile> = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut found = Vec::new();
            let build = path.join("smithy-build.json");
            if build.is_file() {
                let text = std::fs::read_to_string(&build)?;
                let entries = merpati_convert::smithy::build_config_paths(&text).map_err(|e| anyhow!(e))?;
                let maven = merpati_convert::smithy::resolve_maven_dependencies(&text, None).map_err(|e| anyhow!(e))?;
                for w in &maven.warnings {
                    eprintln!("warning: {w}");
                }
                // Projects sharing a dependency read its files once.
                for f in maven.sources() {
                    if !dependencies.iter().any(|d| d.name == f.name) {
                        dependencies.push(f);
                    }
                }
                for entry in entries {
                    let p = path.join(&entry);
                    if p.is_dir() {
                        smithy_files_in(&p, &mut found).with_context(|| format!("could not read {}", p.display()))?;
                    } else {
                        found.extend(smithy_file(&p)?);
                    }
                }
            } else {
                smithy_files_in(path, &mut found).with_context(|| format!("could not read {}", path.display()))?;
            }
            if found.is_empty() {
                bail!("no Smithy files (.smithy or JSON AST) in {}", path.display());
            }
            sources.extend(found);
            continue;
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
        let name = path.display().to_string();
        match SourceFile::detect(name, &text) {
            Some(f) => sources.push(f),
            None => bail!(
                "{} is not a Smithy model; several files or a directory can only be imported as one Smithy model",
                path.display()
            ),
        }
    }
    // After the project's files: the first file's namespace is the model's.
    sources.extend(dependencies);
    let refs: Vec<&SourceFile> = sources.iter().collect();
    let (c, warnings) = merpati_convert::smithy::import_sources(&refs).context("could not import the Smithy model")?;
    let label = format!("Smithy ({} files)", sources.len());
    Ok((Some(label), merpati_convert::Imported { collections: vec![c], warnings, ..Default::default() }))
}

/// Smithy files under `dir`, sorted, skipping hidden and `build` directories
/// (Smithy build output repeats the model).
fn smithy_files_in(dir: &Path, out: &mut Vec<merpati_convert::smithy::SourceFile>) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if path.is_dir() {
            if !name.starts_with('.') && name != "build" {
                smithy_files_in(&path, out)?;
            }
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if (ext == "smithy" || ext == "json") && name != "smithy-build.json" {
            out.extend(smithy_file(&path)?);
        }
    }
    Ok(())
}

/// A `.smithy` file (even a broken one) or a Smithy JSON AST.
fn smithy_file(path: &Path) -> Result<Option<merpati_convert::smithy::SourceFile>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let name = path.display().to_string();
    Ok(if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("smithy")) {
        Some(merpati_convert::smithy::SourceFile::parse(name, &text))
    } else {
        merpati_convert::smithy::SourceFile::detect(name, &text)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    Postman,
    Openapi,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    #[arg(short = 'w', long, default_value = ".")]
    pub workspace: PathBuf,
    /// Collection name or id.
    #[arg(short = 'c', long)]
    pub collection: Option<String>,
    /// Export this environment (Postman format) instead of a collection.
    #[arg(short = 'e', long)]
    pub environment: Option<String>,
    /// Export the workspace globals (Postman format).
    #[arg(long)]
    pub globals: bool,
    #[arg(short = 'f', long, value_enum, default_value_t = ExportFormat::Postman)]
    pub format: ExportFormat,
    /// Output file (stdout when omitted).
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,
    /// Include secret values (they are blanked by default).
    #[arg(long = "include-secrets")]
    pub include_secrets: bool,
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
}

fn export_collection(
    c: &Collection,
    format: ExportFormat,
    include_secrets: bool,
) -> Result<(serde_json::Value, usize)> {
    Ok(match format {
        ExportFormat::Postman => {
            let (v, report) =
                postman::export_collection(c, ExportOptions { include_secrets, use_current_values: false });
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            (v, report.redacted)
        }
        ExportFormat::Openapi => (merpati_convert::openapi::export(c), 0),
    })
}

pub fn export(args: ExportArgs) -> Result<i32> {
    let ctx = source::open_workspace(&args.workspace, source::keychain_allowed(args.no_keychain))?;
    let opts = ExportOptions { include_secrets: args.include_secrets, use_current_values: false };
    let (value, redacted) = if let Some(e) = &args.environment {
        let env = source::load_environment(e, Some(&ctx))?;
        let (v, r) = postman::export_environment(&env, opts);
        (v, r.redacted)
    } else if args.globals {
        let (v, r) = postman::export_globals(&ctx.ws.globals, opts);
        (v, r.redacted)
    } else {
        let c = source::pick_collection(&ctx.ws.collections, args.collection.as_deref(), "the workspace")?;
        export_collection(&c, args.format, args.include_secrets)?
    };
    util::write_output(args.output.as_deref(), &serde_json::to_string_pretty(&value)?)?;
    if redacted > 0 {
        eprintln!("note: {redacted} secret value(s) were blanked; use --include-secrets to keep them");
    }
    if let Some(o) = &args.output {
        eprintln!("written to {}", o.display());
    }
    Ok(0)
}

#[derive(Args, Debug)]
pub struct ConvertArgs {
    /// Any importable file.
    pub input: PathBuf,
    #[arg(long, value_enum)]
    pub to: ExportFormat,
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,
    #[arg(short = 'c', long)]
    pub collection: Option<String>,
    /// Keep secret values (secret variables, auth credentials); they are
    /// blanked by default.
    #[arg(long = "include-secrets")]
    pub include_secrets: bool,
}

pub fn convert(args: ConvertArgs) -> Result<i32> {
    let text =
        std::fs::read_to_string(&args.input).with_context(|| format!("could not read {}", args.input.display()))?;
    let imported =
        merpati_convert::import_auto(&text).with_context(|| format!("could not read {}", args.input.display()))?;
    for w in &imported.warnings {
        eprintln!("warning: {w}");
    }
    let (value, redacted) = if imported.collections.is_empty() {
        let env =
            imported.environments.first().ok_or_else(|| anyhow!("nothing to convert in {}", args.input.display()))?;
        if args.to != ExportFormat::Postman {
            bail!("environments can only be converted to Postman format");
        }
        let opts = ExportOptions { include_secrets: args.include_secrets, use_current_values: false };
        let (v, report) = postman::export_environment(env, opts);
        (v, report.redacted)
    } else {
        // Without -c a file with several collections converts the first one;
        // an explicit -c that matches nothing is an error.
        let origin = args.input.display().to_string();
        let c = match args.collection.as_deref() {
            Some(sel) => source::pick_collection(&imported.collections, Some(sel), &origin)?,
            None => imported.collections[0].clone(),
        };
        export_collection(&c, args.to, args.include_secrets)?
    };
    util::write_output(args.output.as_deref(), &serde_json::to_string_pretty(&value)?)?;
    if redacted > 0 {
        eprintln!("note: {redacted} secret value(s) were blanked; use --include-secrets to keep them");
    }
    Ok(0)
}

#[derive(Args, Debug)]
pub struct ValidateArgs {
    /// File or workspace directory.
    pub path: PathBuf,
    /// Treat warnings (e.g. undefined variables) as errors.
    #[arg(long)]
    pub strict: bool,
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
}

/// Undefined `{{variables}}` used by requests, given all known names.
fn undefined_refs(c: &Collection, known: &std::collections::HashSet<String>) -> Vec<(String, String)> {
    fn walk(
        items: &[Item],
        known: &std::collections::HashSet<String>,
        folder_vars: &[String],
        out: &mut Vec<(String, String)>,
    ) {
        for item in items {
            match item {
                Item::Folder(f) => {
                    let mut fv = folder_vars.to_vec();
                    fv.extend(f.variables.iter().map(|v| v.key.clone()));
                    walk(&f.items, known, &fv, out);
                }
                Item::Request(r) => {
                    let RequestKind::Http(h) = &r.request else { continue };
                    let mut texts = vec![h.url.as_str(), h.body.raw.as_str()];
                    texts.extend(h.headers.iter().filter(|x| x.enabled).map(|x| x.value.as_str()));
                    for t in texts {
                        for name in variables::references(t) {
                            let defined = known.contains(name)
                                || folder_vars.iter().any(|f| f == name)
                                || name.starts_with('$')
                                || name.starts_with(variables::VAULT_PREFIX);
                            if !defined && !out.iter().any(|(rn, n)| rn == &r.name && n == name) {
                                out.push((r.name.clone(), name.to_string()));
                            }
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&c.items, known, &[], &mut out);
    out
}

/// Structural problems in a Postman collection that the tolerant importer
/// would silently skip.
fn postman_structure_errors(v: &serde_json::Value) -> Vec<String> {
    fn items(v: &serde_json::Value, path: &str, out: &mut Vec<String>) {
        let Some(list) = v.get("item") else { return };
        let Some(list) = list.as_array() else {
            out.push(format!("{path}: `item` must be an array"));
            return;
        };
        for (i, it) in list.iter().enumerate() {
            let here = format!("{path}/item[{i}]");
            if !it.is_object() {
                out.push(format!("{here}: must be an object"));
                continue;
            }
            if it.get("name").and_then(|n| n.as_str()).is_none() {
                out.push(format!("{here}: missing `name`"));
            }
            if it.get("item").is_none() && it.get("request").is_none() {
                out.push(format!("{here}: neither a folder (`item`) nor a request (`request`)"));
            }
            if it.get("request").filter(|r| r.is_object()).is_some_and(|r| r.get("url").is_none()) {
                out.push(format!("{here}: request has no `url`"));
            }
            items(it, &here, out);
        }
    }
    let mut out = Vec::new();
    if v.pointer("/info/name").and_then(|n| n.as_str()).is_none() {
        out.push("info: missing `name`".into());
    }
    items(v, "", &mut out);
    out
}

pub fn validate(args: ValidateArgs) -> Result<i32> {
    let mut warnings = 0usize;
    let mut errors = 0usize;
    let mut known = std::collections::HashSet::new();
    let collections: Vec<Collection> = if args.path.is_dir() {
        let ctx: WsCtx = source::open_workspace(&args.path, source::keychain_allowed(args.no_keychain))?;
        println!(
            "Workspace \"{}\": {} collections, {} environments",
            ctx.ws.name,
            ctx.ws.collections.len(),
            ctx.ws.environments.len()
        );
        for e in &ctx.ws.load_errors {
            eprintln!("error: {e}");
            errors += 1;
        }
        for e in &ctx.ws.environments {
            known.extend(e.variables.iter().map(|v| v.key.clone()));
        }
        known.extend(ctx.ws.globals.iter().map(|v| v.key.clone()));
        ctx.ws.collections.clone()
    } else {
        let text =
            std::fs::read_to_string(&args.path).with_context(|| format!("could not read {}", args.path.display()))?;
        let format = merpati_convert::detect(&text).ok_or_else(|| anyhow!("unrecognized format"))?;
        if format == merpati_convert::ImportFormat::PostmanCollection {
            let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
            for e in postman_structure_errors(&v) {
                eprintln!("error: {e}");
                errors += 1;
            }
        }
        let imported = match merpati_convert::import_auto(&text) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("error: {e}");
                return Ok(2);
            }
        };
        println!(
            "{}: {} ({} collections, {} environments, {} globals)",
            args.path.display(),
            format.label(),
            imported.collections.len(),
            imported.environments.len(),
            imported.globals.len()
        );
        for w in &imported.warnings {
            eprintln!("warning: {w}");
            warnings += 1;
        }
        if errors > 0 {
            return Ok(2);
        }
        for e in &imported.environments {
            known.extend(e.variables.iter().map(|v| v.key.clone()));
        }
        known.extend(imported.globals.iter().map(|v| v.key.clone()));
        imported.collections
    };
    for c in &collections {
        println!("  collection \"{}\": {} requests", c.name, c.request_count());
        let mut k = known.clone();
        k.extend(c.variables.iter().map(|v| v.key.clone()));
        for (req, name) in undefined_refs(c, &k) {
            eprintln!(
                "warning: \"{req}\" uses {{{{{name}}}}}, which is not defined in the collection, environments or globals"
            );
            warnings += 1;
        }
        for r in c.requests() {
            if r.request.url().trim().is_empty() {
                eprintln!("warning: \"{}\" has no URL", r.name);
                warnings += 1;
            }
        }
    }
    if errors > 0 {
        return Ok(2);
    }
    if args.strict && warnings > 0 {
        return Ok(1);
    }
    println!("OK ({warnings} warning{})", if warnings == 1 { "" } else { "s" });
    Ok(0)
}

// ---------------------------------------------------------------------------
// codegen
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct CodegenArgs {
    pub source: PathBuf,
    #[arg(short = 'c', long)]
    pub collection: Option<String>,
    /// Request name, id, or `Folder/Request` path.
    #[arg(long)]
    pub request: Option<String>,
    /// Language id (`curl`, `python`, ...) or `list`.
    #[arg(short = 'l', long, default_value = "curl")]
    pub lang: String,
    #[arg(short = 'e', long)]
    pub environment: Option<String>,
    #[arg(long = "env-var", value_name = "KEY=VALUE", value_parser = util::parse_kv)]
    pub env_var: Vec<(String, String)>,
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
}

fn find_request<'a>(c: &'a Collection, sel: &str) -> Option<&'a merpati_core::model::RequestItem> {
    fn walk<'a>(
        items: &'a [Item],
        path: &mut Vec<String>,
        sel: &str,
        out: &mut Option<&'a merpati_core::model::RequestItem>,
    ) {
        for item in items {
            if out.is_some() {
                return;
            }
            match item {
                Item::Folder(f) => {
                    path.push(f.name.clone());
                    walk(&f.items, path, sel, out);
                    path.pop();
                }
                Item::Request(r) => {
                    let full = path.iter().chain(std::iter::once(&r.name)).cloned().collect::<Vec<_>>().join("/");
                    if r.id == sel || full.eq_ignore_ascii_case(sel) {
                        *out = Some(r);
                    }
                }
            }
        }
    }
    let mut found = None;
    walk(&c.items, &mut Vec::new(), sel, &mut found);
    found.or_else(|| c.requests().into_iter().find(|r| r.name.eq_ignore_ascii_case(sel)))
}

pub async fn codegen(args: CodegenArgs) -> Result<i32> {
    use merpati_convert::codegen::{Language, generate};
    if args.lang == "list" {
        for l in Language::ALL {
            println!("{:<16} {}", l.id(), l.label());
        }
        return Ok(0);
    }
    let lang = Language::from_id(&args.lang)
        .ok_or_else(|| anyhow!("unknown language \"{}\"; run with --lang list to see all", args.lang))?;
    let loaded = source::load(&args.source, args.collection.as_deref(), source::keychain_allowed(args.no_keychain))?;
    let c = &loaded.collection;
    let item = match &args.request {
        Some(sel) => find_request(c, sel).ok_or_else(|| anyhow!("request \"{sel}\" not found in \"{}\"", c.name))?,
        None => match c.requests().as_slice() {
            [one] => one,
            _ => bail!("choose a request with --request (name, id or Folder/Name)"),
        },
    };
    let RequestKind::Http(http) = &item.request else { bail!("\"{}\" is not an HTTP request", item.name) };
    let env = source::resolve_environment(args.environment.as_deref(), loaded.ws.as_ref())?;
    let globals = loaded.ws.as_ref().map(|c| c.ws.globals.clone()).unwrap_or_default();
    let mut vars = source::scopes(c, env.as_ref(), &globals, &args.env_var, &[]);
    let chain = c.ancestors(&item.id).unwrap_or_default();
    for a in &chain {
        if let merpati_core::model::Ancestor::Folder(f) = a {
            vars.collection.extend(variables::to_map(&f.variables));
        }
    }
    let eff = c.effective_auth(&item.id);
    let auth = eff.as_ref().map(|e| e.auth.clone()).unwrap_or_default();
    let owner = eff.as_ref().map(|e| e.owner_id.to_string());
    let defaults = merpati_core::settings::RequestDefaults::default();
    let mut resolver = Resolver::new(&vars);
    if let Some(ctx) = &loaded.ws {
        resolver = resolver.with_vault(ctx.secrets.as_ref());
    }
    let ctx = PrepareContext {
        resolver,
        auth: &auth,
        auth_owner_id: owner.as_deref(),
        defaults: &defaults,
        base_dir: Some(&loaded.base_dir),
    };
    let mut prepared = prepare(http, &ctx);
    for n in &prepared.unresolved {
        eprintln!("warning: variable \"{n}\" is not defined");
    }
    match merpati_auth::preview(&prepared) {
        Some(p) => prepared = p.request,
        None if !matches!(prepared.auth.kind, merpati_core::AuthType::None | merpati_core::AuthType::Inherit) => {
            eprintln!("note: {} auth needs a server round trip and is not included", prepared.auth.kind.label());
        }
        None => {}
    }
    println!("{}", generate(lang, &prepared));
    Ok(0)
}

// ---------------------------------------------------------------------------
// perf
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct PerfArgs {
    pub source: PathBuf,
    #[arg(short = 'c', long)]
    pub collection: Option<String>,
    /// Requests to load (name, id or Folder/Name; repeatable). All when omitted.
    #[arg(long)]
    pub request: Vec<String>,
    /// Number of virtual users.
    #[arg(long, default_value_t = 10)]
    pub vus: usize,
    #[arg(long, value_parser = util::parse_duration, default_value = "30s")]
    pub duration: Duration,
    #[arg(long = "ramp-up", value_parser = util::parse_duration, default_value = "0s")]
    pub ramp_up: Duration,
    #[arg(short = 'e', long)]
    pub environment: Option<String>,
    #[arg(long = "env-var", value_name = "KEY=VALUE", value_parser = util::parse_kv)]
    pub env_var: Vec<(String, String)>,
    #[arg(short = 'k', long)]
    pub insecure: bool,
    #[arg(long = "timeout-request", value_name = "MS")]
    pub timeout_request: Option<u64>,
    /// Write the full report as JSON.
    #[arg(long, value_name = "FILE")]
    pub json: Option<PathBuf>,
    /// Exit with 1 when the error rate (0..1) is above this.
    #[arg(long = "max-error-rate")]
    pub max_error_rate: Option<f64>,
    #[arg(long)]
    pub silent: bool,
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
}

pub async fn perf(args: PerfArgs) -> Result<i32> {
    let mut loaded =
        source::load(&args.source, args.collection.as_deref(), source::keychain_allowed(args.no_keychain))?;
    util::force_settings(&mut loaded.collection, args.insecure, false, args.timeout_request);
    let c = &loaded.collection;
    let request_ids = if args.request.is_empty() {
        None
    } else {
        let mut ids = Vec::new();
        for sel in &args.request {
            ids.push(find_request(c, sel).ok_or_else(|| anyhow!("request \"{sel}\" not found"))?.id.clone());
        }
        Some(ids)
    };
    let env = source::resolve_environment(args.environment.as_deref(), loaded.ws.as_ref())?;
    let globals = loaded.ws.as_ref().map(|c| c.ws.globals.clone()).unwrap_or_default();
    let vars = source::scopes(c, env.as_ref(), &globals, &args.env_var, &[]);
    let mut settings = merpati_core::settings::AppSettings::default();
    if args.insecure {
        settings.request.verify_ssl = false;
    }
    let services = util::services(
        &settings,
        Arc::new(CookieJar::new()),
        loaded.ws.as_ref().map(|c| c.secrets.clone()),
        loaded.ws.as_ref().map(|c| c.store.local_dir()).as_deref(),
        Duration::from_millis(settings.script_timeout_ms),
        false,
    );
    let opts = PerfOptions { virtual_users: args.vus, duration: args.duration, ramp_up: args.ramp_up, request_ids };
    if !args.silent {
        println!(
            "Load testing \"{}\" with {} virtual users for {:?} (ramp-up {:?})",
            c.name, args.vus, args.duration, args.ramp_up
        );
        println!("{:>6} {:>8} {:>12} {:>8} {:>5}", "second", "req/s", "avg latency", "errors", "VUs");
    }
    let cancel = cancel_on_ctrl_c();
    let silent = args.silent;
    let mut on_sample = |s: &merpati_runner::perf::PerfSample| {
        if !silent {
            println!(
                "{:>6} {:>8.0} {:>12} {:>8} {:>5}",
                s.second,
                s.rps,
                merpati_core::util::human_ms(s.avg_latency_ms),
                s.errors,
                s.active_vus
            );
        }
    };
    let report = run_perf(&services, c, &vars, opts, &mut on_sample, &cancel).await;
    if !args.silent {
        let l = &report.latency;
        let ms = merpati_core::util::human_ms;
        println!();
        println!(
            "requests:    {} ({} errors, {:.2}% error rate)",
            report.total,
            report.errors,
            report.error_rate * 100.0
        );
        println!("throughput:  {:.1} req/s", report.throughput_rps);
        println!(
            "latency:     avg {}  min {}  p50 {}  p90 {}  p95 {}  p99 {}  max {}",
            ms(l.avg_ms),
            ms(l.min_ms),
            ms(l.p50_ms),
            ms(l.p90_ms),
            ms(l.p95_ms),
            ms(l.p99_ms),
            ms(l.max_ms)
        );
        if report.tests_failed > 0 {
            println!("assertions:  {} failed", report.tests_failed);
        }
        if report.per_request.len() > 1 {
            println!("\n{:<32} {:>8} {:>8} {:>10} {:>10}", "request", "total", "errors", "p50", "p95");
            for r in &report.per_request {
                println!(
                    "{:<32} {:>8} {:>8} {:>10} {:>10}",
                    r.name,
                    r.total,
                    r.errors,
                    ms(r.latency.p50_ms),
                    ms(r.latency.p95_ms)
                );
            }
        }
        if report.cancelled {
            println!("\n(cancelled)");
        }
    }
    if let Some(p) = &args.json {
        util::write_output(Some(p), &serde_json::to_string_pretty(&report)?)?;
    }
    let too_many_errors = args.max_error_rate.map(|m| report.error_rate > m).unwrap_or(false);
    Ok(if too_many_errors || report.total == 0 { 1 } else { 0 })
}

#[derive(clap::Args, Debug)]
pub struct DocsArgs {
    /// Postman collection file, Merpati workspace, or collection folder.
    pub source: PathBuf,
    #[arg(short = 'c', long)]
    pub collection: Option<String>,
    /// Output folder for the HTML site, or file for Markdown.
    #[arg(short = 'o', long, default_value = "merpati-docs")]
    pub out: PathBuf,
    /// Write a single Markdown file instead of an HTML site.
    #[arg(long)]
    pub markdown: bool,
    /// Environment used to resolve variables in examples and snippets.
    #[arg(short = 'e', long)]
    pub environment: Option<String>,
    /// Snippet languages (comma separated ids, e.g. curl,python).
    #[arg(long, default_value = "curl")]
    pub languages: String,
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
}

pub fn docs(args: DocsArgs) -> Result<i32> {
    use merpati_convert::codegen::Language;
    use merpati_convert::docs;
    let loaded = source::load(&args.source, args.collection.as_deref(), source::keychain_allowed(args.no_keychain))?;
    let c = &loaded.collection;
    let env = source::resolve_environment(args.environment.as_deref(), loaded.ws.as_ref())?;
    let globals = loaded.ws.as_ref().map(|c| c.ws.globals.clone()).unwrap_or_default();
    let mut variables = source::scopes(c, env.as_ref(), &globals, &[], &[]);
    let mut all_vars = globals.clone();
    if let Some(e) = &env {
        all_vars.extend(e.variables.iter().cloned());
    }
    all_vars.extend(c.variables.iter().cloned());
    docs::redact_secret_variables(&mut variables, &all_vars);
    let mut languages = Vec::new();
    for id in args.languages.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        languages.push(Language::from_id(id).ok_or_else(|| anyhow!("unknown language \"{id}\""))?);
    }
    let opts = docs::DocsOptions { variables, snippet_languages: languages, title: Some(c.name.clone()) };
    if args.markdown {
        std::fs::write(&args.out, docs::to_markdown(c, &opts))
            .with_context(|| format!("could not write {}", args.out.display()))?;
        println!("Wrote {}", args.out.display());
    } else {
        let index = docs::write_html_site(c, &opts, &args.out)
            .with_context(|| format!("could not write {}", args.out.display()))?;
        println!("Wrote {}", index.display());
    }
    Ok(0)
}
