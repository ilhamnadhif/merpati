//! `merpati run` and `merpati schedule`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use tokio_util::sync::CancellationToken;

use merpati_core::postman::{self, ExportOptions};
use merpati_core::variables;
use merpati_http::CookieJar;
use merpati_runner::report::{self, CliReporter, HtmlReporter, JsonReporter, JunitReporter, Reporters, Symbols};
use merpati_runner::runner::plan;
use merpati_runner::{ConsoleKind, RunEvent, RunOptions, RunSummary, run_collection};

use crate::{certs, jar, source, util};

#[derive(Args, Clone, Debug)]
pub struct RunArgs {
    /// Postman collection file, Merpati workspace, or collection folder.
    pub source: PathBuf,
    /// Collection name or id (when the workspace has several).
    #[arg(short = 'c', long)]
    pub collection: Option<String>,
    /// Environment file (Postman format) or workspace environment name.
    #[arg(short = 'e', long)]
    pub environment: Option<String>,
    /// Globals file (Postman format).
    #[arg(short = 'g', long)]
    pub globals: Option<PathBuf>,
    /// CSV or JSON file with one row/object per iteration.
    #[arg(short = 'd', long = "iteration-data")]
    pub iteration_data: Option<PathBuf>,
    #[arg(short = 'n', long = "iteration-count")]
    pub iteration_count: Option<usize>,
    /// Run only these folders (or single requests, by name or id), in the
    /// given order (repeatable).
    #[arg(long)]
    pub folder: Vec<String>,
    /// Set an environment variable (repeatable).
    #[arg(long = "env-var", value_name = "KEY=VALUE", value_parser = util::parse_kv)]
    pub env_var: Vec<(String, String)>,
    /// Set a global variable (repeatable).
    #[arg(long = "global-var", value_name = "KEY=VALUE", value_parser = util::parse_kv)]
    pub global_var: Vec<(String, String)>,
    /// Delay between requests in ms.
    #[arg(long = "delay-request", default_value_t = 0, value_name = "MS")]
    pub delay_request: u64,
    /// Timeout for the whole run in ms (0 = none).
    #[arg(long = "timeout", value_name = "MS")]
    pub timeout: Option<u64>,
    /// Per-request timeout in ms (0 = none).
    #[arg(long = "timeout-request", value_name = "MS")]
    pub timeout_request: Option<u64>,
    /// Script timeout in ms (0 = none).
    #[arg(long = "timeout-script", value_name = "MS")]
    pub timeout_script: Option<u64>,
    /// Disable TLS certificate verification.
    #[arg(short = 'k', long)]
    pub insecure: bool,
    /// Do not follow redirects.
    #[arg(long = "ignore-redirects")]
    pub ignore_redirects: bool,
    /// Stop the run early (newman `--bail [modifiers]`). `failure` (also
    /// what `--bail` alone does) stops once the first request with a failed
    /// test, script error or request error has finished; `folder` stops
    /// before any request runs when a `--folder` is not found, which Merpati
    /// always does. Modifiers follow as `--bail failure`, `--bail=folder` or
    /// `--bail failure,folder`; any other word after `--bail` is the
    /// collection.
    #[arg(long, value_name = "MODIFIERS", num_args = 0..=1, require_equals = true, value_parser = util::parse_bail)]
    pub bail: Option<Option<util::BailModifiers>>,
    /// Exit with 0 even when requests, tests or scripts failed (usage and
    /// load errors still exit with 2).
    #[arg(short = 'x', long = "suppress-exit-code")]
    pub suppress_exit_code: bool,
    /// Fail this fraction (0..1) of requests with a simulated network error.
    #[arg(long, value_name = "RATE", default_value_t = 0.0)]
    pub simulate_failure_rate: f32,
    /// Reporters: cli, json, junit, html (comma separated).
    #[arg(short = 'r', long, value_delimiter = ',', default_value = "cli")]
    pub reporters: Vec<String>,
    #[arg(long = "reporter-json-export", value_name = "FILE")]
    pub reporter_json_export: Option<PathBuf>,
    #[arg(long = "reporter-junit-export", value_name = "FILE")]
    pub reporter_junit_export: Option<PathBuf>,
    #[arg(long = "reporter-html-export", value_name = "FILE")]
    pub reporter_html_export: Option<PathBuf>,
    /// Colored output: auto (when stdout is a terminal and NO_COLOR is not
    /// set), on or off.
    #[arg(long, value_enum, value_name = "WHEN", default_value_t = util::ColorMode::Auto)]
    pub color: util::ColorMode,
    /// Same as `--color off`.
    #[arg(long = "no-color", conflicts_with = "color")]
    pub no_color: bool,
    /// Plain ASCII instead of Unicode symbols (✓ ❏ ↳ │ ┌ and the summary
    /// table) in the CLI reporter and `--verbose` output.
    #[arg(long = "disable-unicode")]
    pub disable_unicode: bool,
    /// No CLI output.
    #[arg(long)]
    pub silent: bool,
    /// Print console, network and header details.
    #[arg(long)]
    pub verbose: bool,
    /// Write the final environment (Postman format).
    #[arg(long = "export-environment", value_name = "FILE")]
    pub export_environment: Option<PathBuf>,
    /// Write the final globals (Postman format).
    #[arg(long = "export-globals", value_name = "FILE")]
    pub export_globals: Option<PathBuf>,
    /// Write the collection after the run (Postman v2.1), with the
    /// collection variables as the scripts left them.
    #[arg(long = "export-collection", value_name = "FILE")]
    pub export_collection: Option<PathBuf>,
    /// Cookie jar to start the run with: newman's (tough-cookie JSON, as
    /// written by `--export-cookie-jar`) or a Merpati jar. It is only read.
    #[arg(long = "cookie-jar", value_name = "FILE")]
    pub cookie_jar: Option<PathBuf>,
    /// Write the cookie jar after the run (tough-cookie JSON, readable by
    /// newman and `--cookie-jar`); may be the `--cookie-jar` file itself.
    #[arg(long = "export-cookie-jar", value_name = "FILE")]
    pub export_cookie_jar: Option<PathBuf>,
    /// Base directory for relative file paths in request bodies and of the
    /// `--ssl-client-*` certificates.
    #[arg(long = "working-dir", value_name = "DIR")]
    pub working_dir: Option<PathBuf>,
    /// Extra trusted CA certificates (PEM).
    #[arg(long = "ssl-extra-ca-certs", value_name = "FILE")]
    pub ssl_extra_ca_certs: Option<PathBuf>,
    /// Client certificate (PEM, or a .p12/.pfx bundle) for every host that
    /// `--ssl-client-cert-list` has no entry for.
    #[arg(long = "ssl-client-cert", value_name = "FILE")]
    pub ssl_client_cert: Option<PathBuf>,
    /// Private key (PEM) of `--ssl-client-cert`, unless the certificate file
    /// holds it.
    #[arg(long = "ssl-client-key", value_name = "FILE", requires = "ssl_client_cert")]
    pub ssl_client_key: Option<PathBuf>,
    /// Passphrase of the `--ssl-client-key` (or of the .p12/.pfx bundle).
    #[arg(long = "ssl-client-passphrase", value_name = "PASSPHRASE", requires = "ssl_client_cert")]
    pub ssl_client_passphrase: Option<String>,
    /// Client certificates per host (newman's JSON list: `[{"name",
    /// "matches": ["https://host:port/*"], "cert": {"src"}, "key": {"src"},
    /// "pfx": {"src"}, "passphrase"}]`). A pattern without a port matches
    /// port 443, `:*` any port. Relative paths resolve against
    /// `--working-dir`, else the current directory.
    #[arg(long = "ssl-client-cert-list", value_name = "FILE")]
    pub ssl_client_cert_list: Option<PathBuf>,
    /// App settings JSON (proxy, certificates, request defaults).
    #[arg(long, value_name = "FILE")]
    pub settings: Option<PathBuf>,
    /// Never use the OS keychain for workspace secrets.
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
    /// Keep credentials (Authorization, Cookie and API-key headers, URL
    /// passwords, token query parameters) in reports and console output;
    /// they are masked by default.
    #[arg(long = "show-secrets")]
    pub show_secrets: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Interactive,
    /// Scheduled runs print one line per run instead of the CLI reporter.
    Scheduled,
}

fn export_path(explicit: &Option<PathBuf>, collection: &str, ext: &str) -> PathBuf {
    explicit.clone().unwrap_or_else(|| util::default_report_path("merpati-reports", collection, ext))
}

/// Runs once and returns the summary plus the files the reporters wrote.
pub async fn run_once(args: &RunArgs, mode: Mode, cancel: &CancellationToken) -> Result<(RunSummary, Vec<PathBuf>)> {
    let keychain = source::keychain_allowed(args.no_keychain);
    let mut loaded = source::load(&args.source, args.collection.as_deref(), keychain)?;
    let quiet = args.silent || mode == Mode::Scheduled;
    for w in &loaded.warnings {
        if !args.silent {
            eprintln!("warning: {w}");
        }
    }
    // The exported collection must not carry the run-wide overrides.
    let original = args.export_collection.as_ref().map(|_| loaded.collection.clone());
    util::force_settings(&mut loaded.collection, args.insecure, args.ignore_redirects, args.timeout_request);
    let ws = loaded.ws.as_ref();
    let environment = source::resolve_environment(args.environment.as_deref(), ws)?;
    let globals = match &args.globals {
        Some(p) => source::load_globals(p)?,
        None => ws.map(|c| c.ws.globals.clone()).unwrap_or_default(),
    };
    let mut vars = source::scopes(&loaded.collection, environment.as_ref(), &globals, &args.env_var, &args.global_var);
    let data = match &args.iteration_data {
        Some(p) => merpati_runner::data::load(p)?,
        None => Vec::new(),
    };

    let mut settings = util::load_settings(args.settings.as_deref())?;
    if let Some(t) = args.timeout_request {
        settings.request.timeout_ms = t;
    }
    if args.insecure {
        settings.request.verify_ssl = false;
    }
    if args.ignore_redirects {
        settings.request.follow_redirects = false;
    }
    if let Some(ca) = &args.ssl_extra_ca_certs {
        settings.certificates.ca_path = ca.display().to_string();
        settings.certificates.ca_enabled = true;
    }
    let mut client_certs = certs::client_certificates(&certs::CertArgs {
        cert: args.ssl_client_cert.as_deref(),
        key: args.ssl_client_key.as_deref(),
        passphrase: args.ssl_client_passphrase.as_deref(),
        list: args.ssl_client_cert_list.as_deref(),
        base: args.working_dir.as_deref(),
    })?;
    // Ahead of the settings file's, so they win ties.
    client_certs.append(&mut settings.certificates.client_certs);
    settings.certificates.client_certs = client_certs;
    let jar = Arc::new(match &args.cookie_jar {
        Some(p) => {
            let (jar, warnings) = jar::load(p)?;
            for w in warnings.iter().filter(|_| !args.silent) {
                eprintln!("warning: {w}");
            }
            jar
        }
        None => CookieJar::new(),
    });
    let script_timeout = util::script_timeout(args.timeout_script.unwrap_or(settings.script_timeout_ms));
    let local_dir = ws.map(|c| c.store.local_dir());
    let services = util::services(
        &settings,
        jar.clone(),
        ws.map(|c| c.secrets.clone()),
        local_dir.as_deref(),
        script_timeout,
        keychain,
    );

    // Several folders run in the given order through a custom order list.
    let order = if args.folder.is_empty() {
        None
    } else {
        let mut ids = Vec::new();
        for f in &args.folder {
            let opts = RunOptions { folder: Some(f.clone()), ..Default::default() };
            let list = plan(&loaded.collection, &opts).map_err(anyhow::Error::msg)?;
            ids.extend(list.into_iter().map(|(r, _)| r.id.clone()));
        }
        Some(ids)
    };
    let opts = RunOptions {
        iterations: args.iteration_count,
        delay_ms: args.delay_request,
        data,
        folder: None,
        order,
        // `--bail folder` alone only concerns `--folder` (see `RunArgs::bail`).
        bail: matches!(args.bail, Some(None) | Some(Some(util::BailModifiers { failure: true, .. }))),
        base_dir: Some(args.working_dir.clone().unwrap_or_else(|| loaded.base_dir.clone())),
        body_preview_bytes: 0,
        keep_console: false,
        simulate_failure_rate: args.simulate_failure_rate.clamp(0.0, 1.0),
    };

    let color = util::use_color(if args.no_color { util::ColorMode::Off } else { args.color });
    let symbols = Symbols::new(!args.disable_unicode);
    let name = loaded.collection.name.clone();
    let mut reporters = Reporters::default();
    let mut written = Vec::new();
    for r in &args.reporters {
        match r.trim() {
            "cli" => {
                if !quiet {
                    reporters.0.push(Box::new(CliReporter::new(std::io::stdout(), color).with_symbols(symbols)));
                }
            }
            "json" => {
                let p = export_path(&args.reporter_json_export, &name, "json");
                written.push(p.clone());
                reporters.0.push(Box::new(JsonReporter::new(p)));
            }
            "junit" => {
                let p = export_path(&args.reporter_junit_export, &name, "xml");
                written.push(p.clone());
                reporters.0.push(Box::new(JunitReporter::new(p, &name)));
            }
            "html" => {
                let p = export_path(&args.reporter_html_export, &name, "html");
                written.push(p.clone());
                reporters.0.push(Box::new(HtmlReporter::new(p)));
            }
            "" => {}
            other => bail!("unknown reporter \"{other}\" (available: cli, json, junit, html)"),
        }
    }

    if !quiet {
        println!("{}\n", if color { format!("\x1b[1m{name}\x1b[0m") } else { name.clone() });
    }
    let verbose = args.verbose && !args.silent;
    let show_secrets = args.show_secrets;
    let show_console = !quiet;
    let mut pending: Vec<String> = Vec::new();
    let mut sink = |e: RunEvent| {
        if let RunEvent::Execution(exec) = &e {
            pending.clear();
            if show_console {
                for line in &exec.console {
                    let network = line.phase == "network";
                    if network && !verbose {
                        continue;
                    }
                    let tag = match line.kind {
                        ConsoleKind::Warn => "warn ",
                        ConsoleKind::Error => "error ",
                        ConsoleKind::Info => "info ",
                        ConsoleKind::Debug => "debug ",
                        _ => "",
                    };
                    // Network lines are the runner's own text (`GET url → 200 OK`).
                    let message = match network {
                        true => line.message.replace('→', symbols.arrow),
                        false => line.message.clone(),
                    };
                    for (i, l) in message.lines().enumerate() {
                        let prefix = if i == 0 { tag } else { "" };
                        pending.push(format!("  {} {prefix}{l}", symbols.block_line));
                    }
                }
            }
            if verbose {
                let line = symbols.block_line;
                let header = |k: &str, v: &str| match show_secrets {
                    true => format!("  {line} {k}: {v}"),
                    false => format!("  {line} {k}: {}", report::redact_header(k, v)),
                };
                if let Some(sent) = &exec.sent {
                    let url = if show_secrets { sent.url.clone() } else { report::redact_url(&sent.url) };
                    pending.push(format!("  {} request {} {url}", symbols.block_start, sent.method));
                    for (k, v) in &sent.headers {
                        pending.push(header(k, v));
                    }
                }
                if let Some(r) = &exec.response {
                    pending.push(format!(
                        "  {} response {} {} ({} bytes)",
                        symbols.block_start,
                        r.version,
                        r.status,
                        r.body.len()
                    ));
                    for (k, v) in &r.headers {
                        pending.push(header(k, v));
                    }
                }
            }
            return;
        }
        match &e {
            RunEvent::RequestFinished(r) if !show_secrets => {
                let mut r = r.clone();
                report::redact_result(&mut r);
                reporters.on_event(&RunEvent::RequestFinished(r));
            }
            _ => reporters.on_event(&e),
        }
        if matches!(e, RunEvent::RequestFinished(_)) && !pending.is_empty() {
            let mut out = std::io::stdout().lock();
            for l in pending.drain(..) {
                let _ = writeln!(out, "{}", if color { format!("\x1b[2m{l}\x1b[0m") } else { l });
            }
        }
    };
    // `--timeout` cancels the whole run after that long (newman).
    let run_cancel = cancel.child_token();
    let timer = args.timeout.filter(|ms| *ms > 0).map(|ms| {
        let c = run_cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            c.cancel();
        })
    });
    let summary = run_collection(&services, &loaded.collection, &mut vars, opts, &mut sink, &run_cancel).await;
    if let Some(t) = timer {
        t.abort();
    }
    let summary = summary.map_err(anyhow::Error::msg)?;
    reporters.finish(&summary).context("could not write reports")?;
    if summary.cancelled && !cancel.is_cancelled() && !args.silent {
        eprintln!("error: the run timed out after {} ms (--timeout)", args.timeout.unwrap_or_default());
    }
    if !args.silent && mode == Mode::Interactive {
        for p in &written {
            eprintln!("report written to {}", p.display());
        }
    }

    let export_opts = ExportOptions { include_secrets: true, use_current_values: true };
    if let Some(path) = &args.export_environment {
        let mut env = environment.clone().unwrap_or_else(|| merpati_core::Environment::new("Exported environment"));
        variables::apply_typed(&mut env.variables, &vars.environment, &vars.types.environment);
        let (v, _) = postman::export_environment(&env, export_opts);
        util::write_output(Some(path), &serde_json::to_string_pretty(&v)?)?;
    }
    if let Some(path) = &args.export_globals {
        let mut g = globals.clone();
        variables::apply_typed(&mut g, &vars.globals, &vars.types.globals);
        let (v, _) = postman::export_globals(&g, export_opts);
        util::write_output(Some(path), &serde_json::to_string_pretty(&v)?)?;
    }
    if let (Some(path), Some(mut c)) = (&args.export_collection, original) {
        variables::apply_typed(&mut c.variables, &vars.collection, &vars.types.collection);
        let (v, _) = postman::export_collection(&c, export_opts);
        util::write_output(Some(path), &serde_json::to_string_pretty(&v)?)?;
    }
    if let Some(path) = &args.export_cookie_jar {
        let v = jar::to_tough_cookie(&jar);
        util::write_output(Some(path), &serde_json::to_string_pretty(&v)?).context("could not write cookie jar")?;
    }
    Ok((summary, written))
}

/// Ctrl+C cancels the token.
pub fn cancel_on_ctrl_c() -> CancellationToken {
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            c.cancel();
        }
    });
    cancel
}

pub async fn run_cmd(args: RunArgs) -> Result<i32> {
    let cancel = cancel_on_ctrl_c();
    let (summary, _) = run_once(&args, Mode::Interactive, &cancel).await?;
    Ok(if summary.success() || args.suppress_exit_code { 0 } else { 1 })
}

#[derive(Args, Clone, Debug)]
pub struct ScheduleArgs {
    /// Interval between run starts (`30s`, `5m`, `1h`).
    #[arg(long, value_parser = util::parse_duration)]
    pub every: Duration,
    /// Stop after this many runs.
    #[arg(long = "max-runs")]
    pub max_runs: Option<usize>,
    /// Directory for per-run JSON summaries (default: <workspace>/.merpati/runs or ./merpati-runs).
    #[arg(long = "runs-dir")]
    pub runs_dir: Option<PathBuf>,
    #[command(flatten)]
    pub run: RunArgs,
}

pub async fn schedule_cmd(args: ScheduleArgs) -> Result<i32> {
    if args.every.is_zero() {
        bail!("--every must be greater than zero");
    }
    let cancel = cancel_on_ctrl_c();
    let runs_dir = match &args.runs_dir {
        Some(d) => d.clone(),
        None => match source::find_workspace_root(&args.run.source).filter(|_| args.run.source.is_dir()) {
            Some(root) => root.join(merpati_core::storage::LOCAL_DIR).join("runs"),
            None => PathBuf::from("merpati-runs"),
        },
    };
    std::fs::create_dir_all(&runs_dir).with_context(|| format!("could not create {}", runs_dir.display()))?;
    if !args.run.silent {
        println!("Running every {:?}; summaries go to {} (Ctrl+C to stop)", args.every, runs_dir.display());
    }
    let mut n = 0usize;
    let mut any_failed = false;
    let mut ticker = tokio::time::interval(args.every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = cancel.cancelled() => break,
        }
        n += 1;
        let started = chrono::Local::now();
        let line = match run_once(&args.run, Mode::Scheduled, &cancel).await {
            Ok((s, _)) => {
                any_failed |= !s.success() && !args.run.suppress_exit_code;
                let file = runs_dir.join(format!("{}.json", started.format("%Y-%m-%dT%H-%M-%S%.3f")));
                std::fs::write(&file, serde_json::to_string_pretty(&s)?)?;
                format!(
                    "{} run #{n}: {} {} requests ({} failed), {}/{} assertions passed, {} script errors, {}",
                    started.format("%Y-%m-%d %H:%M:%S"),
                    if s.success() { "PASS" } else { "FAIL" },
                    s.executed,
                    s.requests_failed,
                    s.tests_passed,
                    s.tests_passed + s.tests_failed,
                    s.script_errors,
                    merpati_core::util::human_ms(s.total_time_ms)
                )
            }
            Err(e) => {
                any_failed = true;
                format!("{} run #{n}: ERROR {e:#}", started.format("%Y-%m-%d %H:%M:%S"))
            }
        };
        if !args.run.silent {
            println!("{line}");
        }
        if args.max_runs.map(|m| n >= m).unwrap_or(false) || cancel.is_cancelled() {
            break;
        }
    }
    Ok(if any_failed { 1 } else { 0 })
}
