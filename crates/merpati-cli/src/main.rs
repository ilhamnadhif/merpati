//! `merpati`: run collections in CI, start mock servers, import/export and
//! convert collections, generate code and load test, without the app.
//!
//! Exit codes: 0 success, 1 failed requests/tests (0 with `run -x`), 2 usage
//! or load errors.

mod certs;
mod cmds;
mod flow;
mod jar;
mod run;
mod source;
mod util;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "merpati", version, about = "Merpati API client — command line", propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a collection (newman-compatible flags).
    Run(run::RunArgs),
    /// Run a collection repeatedly on a schedule.
    Schedule(run::ScheduleArgs),
    /// Start a mock server from a collection's examples.
    Mock(cmds::MockArgs),
    /// Import a file into a workspace (Postman, OpenAPI, Insomnia, HAR, cURL, ...), or a Smithy
    /// model from several files or a directory.
    Import(cmds::ImportArgs),
    /// Export a collection, environment or globals from a workspace.
    Export(cmds::ExportArgs),
    /// Convert any importable file to Postman or OpenAPI.
    Convert(cmds::ConvertArgs),
    /// Check a file or workspace for errors and undefined variables.
    Validate(cmds::ValidateArgs),
    /// Generate a code snippet for a request.
    Codegen(cmds::CodegenArgs),
    /// Create an empty workspace.
    Init(cmds::InitArgs),
    /// Load test requests with virtual users.
    Perf(cmds::PerfArgs),
    /// Generate documentation (static HTML site or Markdown) for a collection.
    Docs(cmds::DocsArgs),
    /// Run, list and validate Local Flows of a workspace.
    Flow(flow::FlowArgs),
}

fn main() -> ExitCode {
    // Script jobs run in worker processes started from this executable.
    merpati_script::maybe_run_worker();
    let cli = match Cli::try_parse_from(util::join_bail_modifiers(std::env::args_os().collect())) {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() { 2 } else { 0 };
            let _ = e.print();
            return ExitCode::from(code);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MERPATI_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
        )
        .with_writer(std::io::stderr)
        .init();
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: could not start the async runtime: {e}");
            return ExitCode::from(2);
        }
    };
    let result = rt.block_on(async move {
        match cli.command {
            Command::Run(a) => run::run_cmd(a).await,
            Command::Schedule(a) => run::schedule_cmd(a).await,
            Command::Mock(a) => cmds::mock(a).await,
            Command::Import(a) => cmds::import(a),
            Command::Export(a) => cmds::export(a),
            Command::Convert(a) => cmds::convert(a),
            Command::Validate(a) => cmds::validate(a),
            Command::Codegen(a) => cmds::codegen(a).await,
            Command::Init(a) => cmds::init(a),
            Command::Perf(a) => cmds::perf(a).await,
            Command::Docs(a) => cmds::docs(a),
            Command::Flow(a) => flow::flow_cmd(a).await,
        }
    });
    // Script workers may still be winding down; do not wait for them.
    rt.shutdown_timeout(std::time::Duration::from_millis(200));
    match result {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}
