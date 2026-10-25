//! Running JavaScript for Script, Condition, Template and Loop blocks through
//! the shared [`ScriptEngine`](merpati_core::script::ScriptEngine).
//!
//! The flow context travels in the data scope under [`CONTEXT_KEY`] (values
//! keep their JSON type there) and the return value comes back as a local
//! variable, so only the engine's stable job/outcome contract is used.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use merpati_core::script::{ScriptError, ScriptJob, ScriptOutcome};
use merpati_runner::Services;
use tokio_util::sync::CancellationToken;

/// Data-scope key holding `{ blocks, labels, inputs, vars, loop, input, flow }`.
pub const CONTEXT_KEY: &str = "__merpati_flow";
/// Local variable carrying the JSON-encoded return value.
pub const RESULT_KEY: &str = "__merpati_result";
/// Local variable carrying the JSON-encoded `vars` object after the script.
pub const VARS_KEY: &str = "__merpati_vars";
/// Prefix of internal keys that never become flow variables.
pub const INTERNAL_PREFIX: &str = "__merpati_";

/// Wraps user code (a function body; `return` sets the result). Everything
/// before the user code stays on the first line so error line numbers match
/// what the user typed.
pub fn wrap(body: &str) -> String {
    let is_async = body.contains("await");
    let (open, close) = if is_async { ("await (async function () {", "})()") } else { ("(function () {", "})()") };
    format!(
        "const __mf = pm.iterationData.get(\"{CONTEXT_KEY}\") || {{}}; \
         const blocks = Object.assign({{}}, __mf.blocks || {{}}); \
         for (const [k, v] of Object.entries(__mf.labels || {{}})) {{ if (!(k in blocks) && blocks[v] !== undefined) blocks[k] = blocks[v]; }} \
         const inputs = __mf.inputs || {{}}, vars = __mf.vars || {{}}, flow = __mf.flow || {{}}; \
         const loop = __mf.loop === undefined ? null : __mf.loop, input = __mf.input === undefined ? null : __mf.input; \
         const __mr = {open}{body}\n{close}; \
         pm.variables.set(\"{RESULT_KEY}\", JSON.stringify(__mr === undefined ? null : __mr)); \
         pm.variables.set(\"{VARS_KEY}\", JSON.stringify(vars));"
    )
}

/// Wraps an expression so its value is the result.
pub fn wrap_expression(expr: &str) -> String {
    wrap(&format!("return ({}\n);", expr.trim().trim_end_matches(';')))
}

/// Runs a job on a blocking thread, honoring cancellation (same as the
/// request pipeline does).
pub async fn run_job(services: &Services, mut job: ScriptJob, cancel: &CancellationToken) -> ScriptOutcome {
    let flag = Arc::new(AtomicBool::new(false));
    job.cancel = flag.clone();
    job.timeout = services.script_timeout;
    let watcher = {
        let cancel = cancel.clone();
        let flag = flag.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            flag.store(true, Ordering::SeqCst);
        })
    };
    let engine = services.scripts.clone();
    let request = job.request.clone();
    let variables = job.variables.clone();
    let outcome = tokio::task::spawn_blocking(move || engine.run(job)).await;
    watcher.abort();
    match outcome {
        Ok(o) => o,
        Err(e) => ScriptOutcome {
            request,
            variables,
            errors: vec![ScriptError { location: "script engine".into(), message: format!("engine crashed: {e}") }],
            ..Default::default()
        },
    }
}
