//! The request pipeline shared by the app, the Collection Runner and the CLI
//! (PRD section 10: "Runner and CLI use the same path as the UI").
//!
//! One execution: pre-request scripts (collection, folders, request) ->
//! variable resolution -> auth -> send (with Digest/NTLM challenge retries) ->
//! post-response scripts and tests.

pub mod data;
pub mod perf;
pub mod pipeline;
mod protocol;
pub mod report;
pub mod runner;
pub mod script_http;
mod session;

pub use pipeline::{ConsoleKind, ConsoleLine, ExecuteInput, Execution, Services, execute};
pub use runner::{RequestResult, RunEvent, RunOptions, RunSummary, run_collection};
