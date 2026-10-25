//! Postman-compatible JavaScript sandbox built on the pure-Rust `boa` engine.
//!
//! [`BoaEngine`] implements [`ScriptEngine`]. Every job runs on a worker
//! thread that owns a long-lived boa `Context`: the `pm` prelude is compiled
//! once per worker and bundled libraries (chai, lodash, moment, crypto-js,
//! tv4) and the `Intl` and Web Streams polyfills are loaded lazily on first
//! use, so the per-job cost stays small.
//! Jobs are isolated from each other by resetting all sandbox state and
//! removing globals created by user code after every job.
//!
//! Limits: boa has no way to interrupt running JavaScript from the outside.
//! Wall-clock timeouts and cancellation are enforced at every host call
//! (`pm.*`, `console.*`, timers, promise jobs) and by a watchdog that
//! abandons the worker if it does not answer in time. Tight loops that never
//! call into the host are stopped by boa's per-call-frame loop iteration limit
//! (see [`EngineConfig::loop_iteration_limit`]); there is no heap limit.
//!
//! [`IsolatedEngine`] runs the same engine in worker processes instead, so a
//! job stuck in native code or over its memory limit can be killed and a
//! crash inside boa (a native stack overflow) fails only that job; see
//! `isolated`. Binaries using it call [`maybe_run_worker`] first thing in
//! `main()`.
//!
//! Jobs with a [`merpati_core::script::ProtocolContext`] expose the request,
//! response and message of non-HTTP protocols; jobs with a
//! [`merpati_core::script::PackageContext`] can `pm.require` npm/JSR
//! packages and Package Library files (see `packages`).

mod executor;
mod host;
mod isolated;
mod packages;
mod tz;
mod worker;

pub use isolated::{IsolatedEngine, IsolationConfig, WORKER_FLAG, maybe_run_worker};

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use merpati_core::script::{ScriptEngine, ScriptError, ScriptJob, ScriptOutcome};

/// Tunables for the sandbox.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Maximum loop iterations per call frame before the script is aborted.
    /// Guards against tight infinite loops that never reach a host call.
    pub loop_iteration_limit: u64,
    /// Maximum nested function calls.
    pub recursion_limit: usize,
    /// VM stack size limit (values).
    pub stack_size_limit: usize,
    /// Native stack size of a worker thread.
    pub thread_stack_bytes: usize,
    /// Idle workers kept around for reuse.
    pub max_idle_workers: usize,
    /// Extra time the watchdog grants a worker past the job deadline before
    /// abandoning it.
    pub watchdog_grace: Duration,
    /// Largest single package module fetched or read from the cache.
    pub package_module_limit: u64,
    /// Largest total size of the modules one `pm.require` loads.
    pub package_graph_limit: u64,
    /// Per-module fetch timeout (also bounded by the job deadline).
    pub package_fetch_timeout: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            loop_iteration_limit: 50_000_000,
            recursion_limit: 2_000,
            stack_size_limit: 1024 * 64,
            thread_stack_bytes: 64 * 1024 * 1024,
            max_idle_workers: 1,
            watchdog_grace: Duration::from_millis(500),
            package_module_limit: 5 * 1024 * 1024,
            package_graph_limit: 30 * 1024 * 1024,
            package_fetch_timeout: Duration::from_secs(30),
        }
    }
}

struct WorkerRequest {
    job: ScriptJob,
    reply: mpsc::Sender<ScriptOutcome>,
}

struct WorkerHandle {
    tx: mpsc::Sender<WorkerRequest>,
}

struct Pool {
    config: EngineConfig,
    idle: Mutex<Vec<WorkerHandle>>,
}

impl Pool {
    fn spawn(self: &Arc<Self>) -> std::io::Result<WorkerHandle> {
        let (tx, rx) = mpsc::channel::<WorkerRequest>();
        let config = self.config.clone();
        std::thread::Builder::new()
            .name("merpati-script".into())
            .stack_size(config.thread_stack_bytes)
            .spawn(move || worker::worker_main(config, rx))?;
        Ok(WorkerHandle { tx })
    }

    fn take(self: &Arc<Self>) -> std::io::Result<WorkerHandle> {
        if let Some(w) = self.idle.lock().ok().and_then(|mut i| i.pop()) {
            return Ok(w);
        }
        self.spawn()
    }

    fn give_back(&self, w: WorkerHandle) {
        if let Ok(mut idle) = self.idle.lock()
            && idle.len() < self.config.max_idle_workers
        {
            idle.push(w);
        }
    }
}

/// The boa-backed [`ScriptEngine`]. Cheap to clone; clones share the pool.
#[derive(Clone)]
pub struct BoaEngine {
    pool: Arc<Pool>,
}

impl Default for BoaEngine {
    fn default() -> Self {
        BoaEngine::new()
    }
}

impl BoaEngine {
    pub fn new() -> Self {
        BoaEngine::with_config(EngineConfig::default())
    }

    pub fn with_config(config: EngineConfig) -> Self {
        BoaEngine { pool: Arc::new(Pool { config, idle: Mutex::new(Vec::new()) }) }
    }

    /// Starts a worker and compiles the prelude plus the assertion library in
    /// the background, so the first real job does not pay for it.
    pub fn warm_up(&self) {
        let Ok(worker) = self.pool.spawn() else { return };
        let (reply, rx) = mpsc::channel();
        if worker.tx.send(WorkerRequest { job: warm_up_job(), reply }).is_ok() {
            let pool = self.pool.clone();
            std::thread::spawn(move || {
                if rx.recv().is_ok() {
                    pool.give_back(worker);
                }
            });
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.pool.config
    }
}

/// Compiles the prelude and loads the assertion library.
fn warm_up_job() -> ScriptJob {
    let mut job = ScriptJob::new(
        merpati_core::script::ScriptEvent::PostResponse,
        vec![merpati_core::script::ScriptSource {
            location: "warm-up".into(),
            code: "pm.expect(1).to.equal(1);".into(),
        }],
        Default::default(),
    );
    job.timeout = Duration::from_secs(30);
    job
}

fn failure_outcome(
    request: merpati_core::script::ScriptRequest,
    variables: merpati_core::VariableScopes,
    message: String,
) -> ScriptOutcome {
    ScriptOutcome {
        request,
        variables,
        errors: vec![ScriptError { location: "script engine".into(), message }],
        ..Default::default()
    }
}

impl ScriptEngine for BoaEngine {
    fn name(&self) -> &'static str {
        "boa"
    }

    fn run(&self, job: ScriptJob) -> ScriptOutcome {
        let protocol_request = job.protocol.as_ref().map(|p| p.request.clone());
        let mut outcome = self.run_job(job);
        if outcome.protocol_request.is_none() {
            outcome.protocol_request = protocol_request;
        }
        outcome
    }
}

impl BoaEngine {
    fn run_job(&self, job: ScriptJob) -> ScriptOutcome {
        if !job.has_code() {
            return ScriptOutcome { request: job.request, variables: job.variables, ..Default::default() };
        }
        let start = Instant::now();
        let deadline = start + job.timeout;
        let cancel = job.cancel.clone();
        let timeout = job.timeout;
        let fallback_request = job.request.clone();
        let fallback_vars = job.variables.clone();

        let worker = match self.pool.take() {
            Ok(w) => w,
            Err(e) => {
                return failure_outcome(fallback_request, fallback_vars, format!("could not start script worker: {e}"));
            }
        };
        let (reply, rx) = mpsc::channel();
        if worker.tx.send(WorkerRequest { job, reply }).is_err() {
            return failure_outcome(fallback_request, fallback_vars, "script worker stopped unexpectedly".into());
        }
        let grace = self.pool.config.watchdog_grace;
        let hard_deadline = deadline + grace;
        let mut cancel_seen: Option<Instant> = None;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(outcome) => {
                    self.pool.give_back(worker);
                    return outcome;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return failure_outcome(fallback_request, fallback_vars, "script worker crashed".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // The worker notices deadlines and cancellation itself at
                    // every host call and answers with partial results; it is
                    // only abandoned when stuck in pure JavaScript.
                    let now = Instant::now();
                    if cancel.load(Ordering::Relaxed) && cancel_seen.is_none() {
                        cancel_seen = Some(now);
                    }
                    let cancelled_stuck = cancel_seen.is_some_and(|t| now >= t + grace);
                    if now >= hard_deadline || cancelled_stuck {
                        // Dropping the handle closes the worker's queue; the
                        // thread exits once the runaway script hits a limit.
                        drop(worker);
                        let message = if cancelled_stuck {
                            "Script execution was cancelled".to_string()
                        } else {
                            format!("Script execution timed out after {} ms", timeout.as_millis())
                        };
                        return failure_outcome(fallback_request, fallback_vars, message);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
