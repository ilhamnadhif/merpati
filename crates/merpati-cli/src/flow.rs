//! `merpati flow run | list | validate`: Local Flows from the command line.
//!
//! Exit codes match `merpati run`: 0 the flow succeeded, 1 it failed (an
//! unhandled block failure, a failed test, a failure path that produced no
//! output, cancellation), 2 usage or load errors (including flows with
//! validation errors and missing required inputs).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use indexmap::IndexMap;
use serde_json::{Value, json};

use merpati_core::script::LogLevel;
use merpati_core::storage::WorkspaceStore;
use merpati_core::variables::{self, VariableScopes};
use merpati_flows::validate::{Severity, has_errors};
use merpati_flows::{BlockKind, Flow, FlowEnv, FlowEvent, FlowResult, FlowRunOptions, PORT_BRANCHES, storage};
use merpati_http::CookieJar;

use crate::{run, source, util};

#[derive(Args, Debug)]
pub struct FlowArgs {
    #[command(subcommand)]
    pub command: FlowCommand,
}

#[allow(clippy::large_enum_variant)] // Parsed once per process.
#[derive(Subcommand, Debug)]
pub enum FlowCommand {
    /// Run a flow of a workspace.
    Run(FlowRunArgs),
    /// List the flows of a workspace.
    List(FlowListArgs),
    /// Check a flow for errors (exit 2 on errors, 1 on warnings with --strict).
    Validate(FlowValidateArgs),
}

#[derive(Args, Debug)]
pub struct FlowRunArgs {
    /// Merpati workspace folder.
    pub workspace: PathBuf,
    /// Flow name, id, file slug or path to a .flow.json file.
    pub flow: String,
    /// Environment file (Postman format) or workspace environment name.
    #[arg(short = 'e', long)]
    pub environment: Option<String>,
    /// Globals file (Postman format); default: the workspace globals.
    #[arg(short = 'g', long)]
    pub globals: Option<PathBuf>,
    /// Value for a flow input (repeatable).
    #[arg(long = "input", value_name = "NAME=VALUE", value_parser = util::parse_kv)]
    pub input: Vec<(String, String)>,
    /// Initial flow variable (repeatable).
    #[arg(long = "var", value_name = "NAME=VALUE", value_parser = util::parse_kv)]
    pub var: Vec<(String, String)>,
    /// Set an environment variable (repeatable).
    #[arg(long = "env-var", value_name = "KEY=VALUE", value_parser = util::parse_kv)]
    pub env_var: Vec<(String, String)>,
    /// Set a global variable (repeatable).
    #[arg(long = "global-var", value_name = "KEY=VALUE", value_parser = util::parse_kv)]
    pub global_var: Vec<(String, String)>,
    /// Reporters: cli, json (comma separated).
    #[arg(short = 'r', long, value_delimiter = ',', default_value = "cli")]
    pub reporters: Vec<String>,
    #[arg(long = "reporter-json-export", value_name = "FILE")]
    pub reporter_json_export: Option<PathBuf>,
    /// Stop after this many block executions (default: the flow's setting or 10000).
    #[arg(long = "max-steps", value_name = "N")]
    pub max_steps: Option<usize>,
    /// Per-request timeout in ms.
    #[arg(long = "timeout-request", value_name = "MS")]
    pub timeout_request: Option<u64>,
    /// Script timeout in ms.
    #[arg(long = "timeout-script", value_name = "MS")]
    pub timeout_script: Option<u64>,
    /// Disable TLS certificate verification.
    #[arg(short = 'k', long)]
    pub insecure: bool,
    /// App settings JSON (proxy, certificates, request defaults).
    #[arg(long, value_name = "FILE")]
    pub settings: Option<PathBuf>,
    #[arg(long = "no-color")]
    pub no_color: bool,
    /// No CLI output.
    #[arg(long)]
    pub silent: bool,
    /// Also print network lines, debug logs, warnings and the text of
    /// streaming AI blocks as it arrives.
    #[arg(long)]
    pub verbose: bool,
    /// Never use the OS keychain for workspace secrets.
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
}

#[derive(Args, Debug)]
pub struct FlowListArgs {
    /// Merpati workspace folder.
    pub workspace: PathBuf,
    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct FlowValidateArgs {
    /// Merpati workspace folder.
    pub workspace: PathBuf,
    /// Flow name, id, file slug or path to a .flow.json file.
    pub flow: String,
    /// Treat warnings as errors (exit 1).
    #[arg(long)]
    pub strict: bool,
    #[arg(long = "no-keychain")]
    pub no_keychain: bool,
}

pub async fn flow_cmd(args: FlowArgs) -> Result<i32> {
    match args.command {
        FlowCommand::Run(a) => run_flow(a).await,
        FlowCommand::List(a) => list(a),
        FlowCommand::Validate(a) => validate(a),
    }
}

fn workspace_root(path: &Path) -> Result<PathBuf> {
    if WorkspaceStore::is_workspace(path) {
        return Ok(path.to_path_buf());
    }
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    source::find_workspace_root(path)
        .ok_or_else(|| anyhow!("{} is not a Merpati workspace (no merpati.json found)", path.display()))
}

fn list(args: FlowListArgs) -> Result<i32> {
    let root = workspace_root(&args.workspace)?;
    let (flows, errors) = storage::list(&root);
    for e in &errors {
        eprintln!("warning: {e}");
    }
    if args.json {
        let v: Vec<Value> = flows
            .iter()
            .map(|f| {
                json!({
                    "id": f.flow.id,
                    "name": f.flow.name,
                    "description": f.flow.description,
                    "blocks": f.flow.blocks.len(),
                    "inputs": f.flow.inputs.iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
                    "path": f.path.strip_prefix(&root).unwrap_or(&f.path),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    if flows.is_empty() {
        println!("No flows in {}", storage::flows_dir(&root).display());
        return Ok(0);
    }
    let w = flows.iter().map(|f| f.flow.name.chars().count()).max().unwrap_or(4).max(4);
    println!("{:<w$}  {:<36}  {:>6}  FILE", "NAME", "ID", "BLOCKS");
    for f in &flows {
        println!(
            "{:<w$}  {:<36}  {:>6}  {}",
            f.flow.name,
            f.flow.id,
            f.flow.blocks.len(),
            f.path.strip_prefix(&root).unwrap_or(&f.path).display()
        );
    }
    Ok(0)
}

fn print_diagnostics(flow: &Flow, diags: &[merpati_flows::Diagnostic], warnings: bool) {
    for d in diags {
        if d.severity == Severity::Warning && !warnings {
            continue;
        }
        let tag = if d.severity == Severity::Error { "error" } else { "warning" };
        let at = d.block.as_deref().and_then(|id| flow.block(id)).map(|b| format!(" [{}]", b.display_name()));
        eprintln!("{tag}{}: {}", at.unwrap_or_default(), d.message);
    }
}

fn validate(args: FlowValidateArgs) -> Result<i32> {
    let root = workspace_root(&args.workspace)?;
    let ctx = source::open_workspace(&root, source::keychain_allowed(args.no_keychain))?;
    let file = match storage::find(&root, &args.flow) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(2);
        }
    };
    let diags = merpati_flows::validate(&file.flow, Some(&ctx.ws.collections));
    print_diagnostics(&file.flow, &diags, true);
    let errors = diags.iter().filter(|d| d.is_error()).count();
    let warnings = diags.len() - errors;
    if errors > 0 {
        eprintln!("{} error{}, {warnings} warning{}", errors, plural(errors), plural(warnings));
        return Ok(2);
    }
    if args.strict && warnings > 0 {
        return Ok(1);
    }
    println!("OK: \"{}\" ({} blocks, {warnings} warning{})", file.flow.name, file.flow.blocks.len(), plural(warnings));
    Ok(0)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// CLI values are strings unless the declared default is typed (number,
/// boolean, array, object) and the value parses as JSON.
fn input_values(flow: &Flow, provided: &[(String, String)]) -> IndexMap<String, Value> {
    provided
        .iter()
        .map(|(k, v)| {
            let typed = flow
                .inputs
                .iter()
                .find(|i| &i.name == k)
                .filter(|i| !matches!(i.default, Value::String(_) | Value::Null))
                .and_then(|_| serde_json::from_str::<Value>(v).ok());
            (k.clone(), typed.unwrap_or_else(|| Value::String(v.clone())))
        })
        .collect()
}

async fn run_flow(args: FlowRunArgs) -> Result<i32> {
    let keychain = source::keychain_allowed(args.no_keychain);
    let root = workspace_root(&args.workspace)?;
    let ctx = source::open_workspace(&root, keychain)?;
    if !args.silent {
        for w in &ctx.ws.load_errors {
            eprintln!("warning: {w}");
        }
    }
    let file = match storage::find(&root, &args.flow) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(2);
        }
    };
    let flow = file.flow;
    let diags = merpati_flows::validate(&flow, Some(&ctx.ws.collections));
    if !args.silent || has_errors(&diags) {
        print_diagnostics(&flow, &diags, !args.silent);
    }
    if has_errors(&diags) {
        eprintln!("error: the flow \"{}\" has errors; fix them before running", flow.name);
        return Ok(2);
    }
    let inputs = input_values(&flow, &args.input);
    if let Err(errors) = merpati_flows::resolve_inputs(&flow, &inputs) {
        for e in errors {
            eprintln!("error: {e} (pass it with --input NAME=VALUE)");
        }
        return Ok(2);
    }
    for r in &args.reporters {
        if !matches!(r.trim(), "cli" | "json" | "") {
            bail!("unknown reporter \"{}\" (available: cli, json)", r.trim());
        }
    }

    let environment = source::resolve_environment(args.environment.as_deref(), Some(&ctx))?;
    let globals = match &args.globals {
        Some(p) => source::load_globals(p)?,
        None => ctx.ws.globals.clone(),
    };
    let mut vars = VariableScopes { globals: variables::to_map(&globals), ..Default::default() };
    vars.types.globals = variables::to_types(&globals);
    if let Some(e) = &environment {
        vars.environment = e.to_map();
        vars.types.environment = e.to_types();
        vars.environment_name = Some(e.name.clone());
    }
    for (k, v) in &args.env_var {
        vars.environment.insert(k.clone(), v.clone());
    }
    for (k, v) in &args.global_var {
        vars.globals.insert(k.clone(), v.clone());
    }

    let mut settings = util::load_settings(args.settings.as_deref())?;
    if let Some(t) = args.timeout_request {
        settings.request.timeout_ms = t;
    }
    if args.insecure {
        settings.request.verify_ssl = false;
    }
    let mut collections = ctx.ws.collections.clone();
    for c in &mut collections {
        util::force_settings(c, args.insecure, false, args.timeout_request);
    }
    let script_timeout = util::script_timeout(args.timeout_script.unwrap_or(settings.script_timeout_ms));
    let local_dir = ctx.store.local_dir();
    let services = util::services(
        &settings,
        Arc::new(CookieJar::new()),
        Some(ctx.secrets.clone()),
        Some(&local_dir),
        script_timeout,
        keychain,
    );
    let env = FlowEnv { services, collections, base_dir: Some(root.clone()) };
    let opts = FlowRunOptions {
        inputs,
        variables: args.var.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect(),
        max_steps: args.max_steps,
    };

    let quiet = args.silent || !args.reporters.iter().any(|r| r.trim() == "cli");
    let color = util::use_color(if args.no_color { util::ColorMode::Off } else { util::ColorMode::Auto });
    let branches = flow
        .blocks
        .iter()
        .filter(|b| b.kind() == BlockKind::Parallel)
        .map(|b| (b.id.clone(), flow.edges_from(&b.id, PORT_BRANCHES).count()))
        .collect();
    let mut printer = Printer {
        color,
        verbose: args.verbose,
        quiet,
        steps: HashMap::new(),
        pending: HashMap::new(),
        streams: HashMap::new(),
        branches,
    };
    if !quiet {
        println!("{}\n", printer.bold(&flow.name));
    }
    let cancel = run::cancel_on_ctrl_c();
    let mut sink = |e: FlowEvent| printer.event(&e);
    let result = merpati_flows::run_flow(&env, &flow, &mut vars, opts, &mut sink, &cancel).await;
    if !quiet {
        printer.summary(&result);
    }
    if args.reporters.iter().any(|r| r.trim() == "json") {
        let path = args
            .reporter_json_export
            .clone()
            .unwrap_or_else(|| util::default_report_path("merpati-reports", &flow.name, "json"));
        let report = json!({
            "flow": {"id": flow.id, "name": flow.name, "file": file.path},
            "environment": vars.environment_name,
            "diagnostics": diags,
            "run": result,
        });
        util::write_output(Some(&path), &serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("could not write {}", path.display()))?;
        if !args.silent {
            eprintln!("report written to {}", path.display());
        }
    }
    Ok(if result.success { 0 } else { 1 })
}

/// A block that is running.
struct Running {
    depth: usize,
    kind: BlockKind,
    label: String,
    block: String,
    branch: Option<String>,
}

/// Prints one line per finished block. Parallel branches finish in any
/// order, so lines are keyed by step and block, never by "the current one".
struct Printer {
    color: bool,
    verbose: bool,
    quiet: bool,
    /// Running blocks by step.
    steps: HashMap<usize, Running>,
    /// Log lines of running blocks by block id, printed after their result
    /// line.
    pending: HashMap<String, Vec<String>>,
    /// Streamed AI text of running steps that is not printed yet (the
    /// current, unfinished line).
    streams: HashMap<usize, String>,
    /// Branch count of every Parallel block.
    branches: HashMap<String, usize>,
}

impl Printer {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }

    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }

    fn indent(depth: usize) -> String {
        "  ".repeat(depth + 1)
    }

    fn out(&self, line: String) {
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "{line}");
    }

    fn stream_line(&self, depth: usize, line: &str) {
        self.out(self.paint("2", &format!("{}  │ {line}", Self::indent(depth))));
    }

    fn event(&mut self, e: &FlowEvent) {
        if self.quiet {
            return;
        }
        match e {
            FlowEvent::BlockStarted { step, block, kind, label, depth, branch, .. } => {
                let running = Running {
                    depth: *depth,
                    kind: *kind,
                    label: label.clone(),
                    block: block.clone(),
                    branch: branch.clone(),
                };
                self.steps.insert(*step, running);
            }
            FlowEvent::AiDelta { step, text, .. } => {
                if !self.verbose {
                    return;
                }
                let Some(depth) = self.steps.get(step).map(|r| r.depth) else { return };
                let buf = self.streams.entry(*step).or_default();
                buf.push_str(text);
                let mut lines = Vec::new();
                while let Some(pos) = buf.find('\n') {
                    lines.push(buf[..pos].to_string());
                    buf.drain(..=pos);
                }
                for l in lines {
                    self.stream_line(depth, &l);
                }
            }
            FlowEvent::BlockFinished { step, block, port, output, duration_ms, failed, error, tests } => {
                let Some(Running { depth, kind, label, branch, .. }) = self.steps.remove(step) else { return };
                if let Some(rest) = self.streams.remove(step).filter(|r| !r.is_empty()) {
                    self.stream_line(depth, &rest);
                }
                let ind = Self::indent(depth);
                let mark = if *failed { self.paint("31", "✗") } else { self.paint("32", "✓") };
                let detail = match kind {
                    BlockKind::Request => {
                        let status = match output.get("status").and_then(Value::as_u64) {
                            Some(s) => format!(
                                " → {s} {}",
                                output.get("status_text").and_then(Value::as_str).unwrap_or_default()
                            ),
                            None => String::new(),
                        };
                        format!(
                            "  {} {}{status}",
                            output.get("method").and_then(Value::as_str).unwrap_or_default(),
                            output.get("url").and_then(Value::as_str).unwrap_or_default()
                        )
                    }
                    BlockKind::Loop if port == "item" => format!(
                        "  item {}/{}",
                        output.get("index").and_then(Value::as_u64).map(|i| i + 1).unwrap_or(0),
                        output.get("count").and_then(Value::as_u64).unwrap_or(0)
                    ),
                    BlockKind::Loop if port == "done" => format!(
                        "  done after {} iteration{}",
                        output.get("iterations").and_then(Value::as_u64).unwrap_or(0),
                        plural(output.get("iterations").and_then(Value::as_u64).unwrap_or(0) as usize)
                    ),
                    BlockKind::Condition if !failed => format!("  → {port}"),
                    BlockKind::Output => format!("  = {}", short(output)),
                    BlockKind::Parallel => {
                        let n = self.branches.get(block).copied().unwrap_or(0);
                        format!("  {n} branch{}", if n == 1 { "" } else { "es" })
                    }
                    BlockKind::Join => format!(
                        "  {}/{} branches",
                        output.get("count").and_then(Value::as_u64).unwrap_or(0),
                        output.get("branches").and_then(Value::as_u64).unwrap_or(0)
                    ),
                    _ => String::new(),
                };
                let tag = branch.map(|b| self.paint("2", &format!(" [{b}]"))).unwrap_or_default();
                let time = if *duration_ms >= 1.0 {
                    self.paint("2", &format!(" ({})", merpati_core::util::human_ms(*duration_ms)))
                } else {
                    String::new()
                };
                self.out(format!("{ind}{mark} {label}{tag}{detail}{time}"));
                for t in tests {
                    let line = if t.skipped {
                        self.paint("2", &format!("{ind}    - {} (skipped)", t.name))
                    } else if t.passed {
                        format!("{ind}    {} {}", self.paint("32", "✓"), t.name)
                    } else {
                        format!(
                            "{ind}    {} {}{}",
                            self.paint("31", "✗"),
                            t.name,
                            t.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
                        )
                    };
                    self.out(line);
                }
                if let Some(err) = error
                    && *failed
                    && tests.iter().all(|t| t.passed || t.skipped)
                {
                    self.out(format!("{ind}    {}", self.paint("31", err)));
                }
                for l in self.pending.remove(block).unwrap_or_default() {
                    self.out(l);
                }
            }
            FlowEvent::Log { block, level, message } => {
                if matches!(level, LogLevel::Debug) && !self.verbose {
                    return;
                }
                let running = block.as_ref().and_then(|b| self.steps.values().find(|r| &r.block == b));
                let depth = running.map(|r| r.depth).unwrap_or(0);
                let tag = match level {
                    LogLevel::Warn => "warn ",
                    LogLevel::Error => "error ",
                    LogLevel::Debug => "debug ",
                    _ => "",
                };
                let ind = Self::indent(depth);
                let held = running.is_some();
                for (i, l) in message.lines().enumerate() {
                    let prefix = if i == 0 { tag } else { "" };
                    let line = self.paint("2", &format!("{ind}  │ {prefix}{l}"));
                    match block.as_ref().filter(|_| held) {
                        Some(b) => self.pending.entry(b.clone()).or_default().push(line),
                        None => self.out(line),
                    }
                }
            }
            FlowEvent::Error { block, message } => {
                if block.is_none() {
                    self.out(format!("  {}", self.paint("31", &format!("error: {message}"))));
                }
            }
            FlowEvent::Started { .. } | FlowEvent::Execution { .. } | FlowEvent::Finished { .. } => {}
        }
    }

    fn summary(&self, r: &FlowResult) {
        println!();
        if !r.outputs.is_empty() {
            println!("{}", self.bold("Outputs"));
            for (k, v) in &r.outputs {
                println!("  {k}: {}", short(v));
            }
            println!();
        }
        let unhandled: Vec<_> = r.failures.iter().filter(|f| !f.handled).collect();
        for f in &unhandled {
            println!("{}", self.paint("31", &format!("✗ {}: {}", f.label, f.message)));
        }
        for e in &r.errors {
            println!("{}", self.paint("31", &format!("✗ {e}")));
        }
        let tests_total = r.tests_passed + r.tests_failed;
        let line = format!(
            "{} {} steps, {} request{}, {}/{} test{} passed, {}",
            if r.success { "Flow passed:" } else { "Flow failed:" },
            r.steps.len(),
            r.requests,
            plural(r.requests),
            r.tests_passed,
            tests_total,
            plural(tests_total),
            merpati_core::util::human_ms(r.duration_ms)
        );
        println!("{}", if r.success { self.paint("32", &line) } else { self.paint("31", &line) });
    }
}

/// Compact JSON (strings unquoted), cut to one line.
fn short(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() > 200 {
        format!("{}…", one_line.chars().take(200).collect::<String>())
    } else {
        one_line
    }
}
