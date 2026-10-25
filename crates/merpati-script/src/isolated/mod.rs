//! Process isolation: [`IsolatedEngine`] runs jobs in worker processes.
//!
//! The in-process [`BoaEngine`](crate::BoaEngine) cannot stop JavaScript
//! stuck in native code (catastrophic regex backtracking), has no heap limit,
//! and a native stack overflow inside boa (`JSON.stringify` of very deeply
//! nested data) aborts the whole process. [`IsolatedEngine`] runs every job
//! in a child process instead: the app's own executable started with
//! [`WORKER_FLAG`], whose `main()` hands over to [`maybe_run_worker`] first
//! thing. The child runs the job with an in-process `BoaEngine`, so script
//! semantics (host-call timeouts, loop limits, packages) are unchanged. The
//! app kills the child when the job runs past its timeout plus
//! [`IsolationConfig::kill_grace`], when it is cancelled and does not stop
//! within a short grace, or when the child uses more than
//! [`IsolationConfig::memory_limit_bytes`]; a crashing child only fails its
//! job.
//!
//! Protocol (see `wire`): newline-delimited JSON over the child's stdin and
//! stdout, which carries nothing else. The first line is the engine
//! configuration; the child compiles the prelude, answers `ready` and then
//! takes one job at a time, answering `done` with the outcome.
//! `pm.sendRequest` becomes a request to the app, which performs it with the
//! job's [`ScriptHttp`] on a helper thread and replies. Small in-memory
//! response bodies travel inline (base64); file bodies (and large in-memory
//! ones, spilled to a temporary file) are passed by path and stay owned by
//! the app until the job ends. Cancellation is forwarded, so a script stopped
//! at a host call keeps its partial results. The last few KB of the child's
//! stderr are kept for crash diagnostics.
//!
//! Idle workers are kept for reuse ([`IsolationConfig::max_idle_workers`]),
//! so a job costs a pipe round trip, not a process start; beyond the first,
//! workers idle for [`IsolationConfig::idle_timeout`] are shut down, so a
//! burst of parallel jobs (a performance test) does not leave processes
//! behind. A killed or
//! crashed worker is reaped and the next job starts a fresh one; a worker
//! whose memory grew past half the limit is not reused.
//!
//! Memory is the child's resident size (on macOS the larger of resident size
//! and physical footprint, which includes compressed memory; on Windows the
//! working set from `GetProcessMemoryInfo`), polled every 25 ms, so a fast
//! allocation can overshoot the limit briefly. Linux reads
//! `/proc/<pid>/statm`. Other systems have no memory limit; everything else
//! works the same.

mod child;
mod wire;

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use merpati_core::script::{ScriptEngine, ScriptHttp, ScriptJob, ScriptOutcome, ScriptResponse};

use self::wire::{FromWorker, HttpCall, ToWorker, WireConfig};
use crate::{EngineConfig, failure_outcome};

/// First argument that makes an executable a script worker; see
/// [`maybe_run_worker`].
pub const WORKER_FLAG: &str = "--merpati-script-worker";

const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// How long a cancelled job may take to stop at a host call before its
/// worker is killed.
const CANCEL_GRACE: Duration = Duration::from_millis(250);
/// Longest a new worker may take to start (process start and prelude).
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of a worker's stderr kept for diagnostics.
const STDERR_TAIL: usize = 4096;

const CANCELLED: &str = "Script execution was cancelled";
const CRASHED: &str = "The script engine stopped unexpectedly (for example a stack overflow from very deeply nested \
                       data); the app was not affected.";

/// Runs this process as a script worker if it was started as one by
/// [`IsolatedEngine`], i.e. with [`WORKER_FLAG`] as the first argument. Call
/// it first thing in `main()`, before anything that writes to stdout.
///
/// Returns at once in a normal start. In a worker it serves jobs and exits
/// the process when the app closes the worker's stdin.
pub fn maybe_run_worker() {
    if std::env::args_os().nth(1).is_some_and(|a| a == WORKER_FLAG) {
        child::main();
    }
}

/// Settings of [`IsolatedEngine`].
#[derive(Clone, Debug)]
pub struct IsolationConfig {
    /// Sandbox settings of the engine inside each worker process. Its
    /// `max_idle_workers` and `watchdog_grace` do not apply there: a worker
    /// runs one job at a time and the app is its watchdog.
    pub engine: EngineConfig,
    /// Memory a worker process may use before its job is stopped; 0 means no
    /// limit. Enforced on macOS, Linux and Windows.
    pub memory_limit_bytes: u64,
    /// Idle worker processes kept for reuse.
    pub max_idle_workers: usize,
    /// How long idle workers beyond the first are kept.
    pub idle_timeout: Duration,
    /// Extra time past the job timeout before a worker that does not answer
    /// (stuck in native code) is killed.
    pub kill_grace: Duration,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        IsolationConfig {
            engine: EngineConfig::default(),
            memory_limit_bytes: 1024 * 1024 * 1024,
            max_idle_workers: 8,
            idle_timeout: Duration::from_secs(30),
            kill_grace: Duration::from_secs(1),
        }
    }
}

/// A [`ScriptEngine`] that runs every job in a worker process, see the
/// module docs. Cheap to clone; clones share the worker pool, and dropping
/// the last one kills the idle workers.
#[derive(Clone)]
pub struct IsolatedEngine {
    pool: Arc<Pool>,
}

impl IsolatedEngine {
    /// `worker_exe` must call [`maybe_run_worker`] first thing in `main()`.
    pub fn new(worker_exe: PathBuf, config: IsolationConfig) -> Self {
        let idle = Mutex::new(Vec::new());
        IsolatedEngine { pool: Arc::new(Pool { exe: worker_exe, config, idle, reaping: AtomicBool::new(false) }) }
    }

    /// Workers are this executable, so its `main()` must call
    /// [`maybe_run_worker`] first thing.
    pub fn for_current_exe(config: IsolationConfig) -> std::io::Result<Self> {
        Ok(IsolatedEngine::new(std::env::current_exe()?, config))
    }

    /// Starts a worker in the background, so the first job does not wait for
    /// the process start and the prelude.
    pub fn warm_up(&self) {
        let pool = self.pool.clone();
        let _ = std::thread::Builder::new().name("merpati-script-warm-up".into()).spawn(move || {
            if let Ok(worker) = pool.spawn() {
                pool.give_back(worker);
            }
        });
    }

    pub fn config(&self) -> &IsolationConfig {
        &self.pool.config
    }
}

impl ScriptEngine for IsolatedEngine {
    fn name(&self) -> &'static str {
        "boa (isolated)"
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

impl IsolatedEngine {
    fn run_job(&self, job: ScriptJob) -> ScriptOutcome {
        if !job.has_code() {
            return ScriptOutcome { request: job.request, variables: job.variables, ..Default::default() };
        }
        let fail = |job: ScriptJob, message: String| failure_outcome(job.request, job.variables, message);
        // Temporary files with bodies for the worker, deleted when the job ends.
        let mut spilled = Vec::new();
        let line = match wire::job_line(&job, &mut spilled) {
            Ok(line) => line,
            Err(e) => return fail(job, format!("could not send the job to the script engine: {e}")),
        };
        let mut worker = match self.pool.take() {
            Ok(w) => w,
            Err(e) => return fail(job, format!("could not start script worker: {e}")),
        };
        worker.clear_stderr();
        let config = &self.pool.config;
        let mut sent_at: Option<Instant> = None;
        let mut respawned = false;
        let mut cancel_sent: Option<Instant> = None;
        let mut pending_http: Option<u64> = None;
        // `pm.sendRequest` responses, kept for their temporary files.
        let mut responses = Vec::new();
        loop {
            if sent_at.is_none() && worker.ready {
                if worker.send(&line).is_ok() {
                    sent_at = Some(Instant::now());
                } else if !respawned {
                    // An idle worker that died in the meantime: start afresh once.
                    respawned = true;
                    drop(worker);
                    worker = match self.pool.spawn() {
                        Ok(w) => w,
                        Err(e) => return fail(job, format!("could not start script worker: {e}")),
                    };
                } else {
                    let details = worker.exit_details();
                    return fail(job, format!("could not start script worker: {details}"));
                }
            }
            match worker.events.recv_timeout(POLL_INTERVAL) {
                Ok(Event::Msg(FromWorker::Ready)) => worker.ready = true,
                Ok(Event::Msg(FromWorker::Done(outcome))) => {
                    self.pool.give_back(worker);
                    return outcome.into_owned();
                }
                Ok(Event::Msg(FromWorker::Http(call))) => {
                    pending_http = Some(call.id);
                    worker.start_http(*call, job.http.clone());
                }
                Ok(Event::HttpDone { id, result }) => {
                    // Late answers to calls of an earlier job are dropped.
                    if pending_http == Some(id) {
                        pending_http = None;
                        if let Ok(line) = wire::reply_line(id, &result, &mut spilled) {
                            let _ = worker.send(&line);
                        }
                        responses.extend(result.ok());
                    }
                }
                Ok(Event::Eof) | Err(RecvTimeoutError::Disconnected) => {
                    let details = worker.exit_details();
                    let message = match (sent_at, details.is_empty()) {
                        (None, _) => format!("could not start script worker: {details}"),
                        (Some(_), true) => CRASHED.to_string(),
                        (Some(_), false) => format!("{CRASHED} Details: {details}"),
                    };
                    return fail(job, message);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }

            let now = Instant::now();
            if job.cancel.load(Ordering::Relaxed) {
                match (sent_at, cancel_sent) {
                    // Still starting: the worker stays usable.
                    (None, _) => {
                        self.pool.give_back(worker);
                        return fail(job, CANCELLED.into());
                    }
                    (Some(_), None) => {
                        if let Ok(line) = wire::line(&ToWorker::Cancel) {
                            let _ = worker.send(&line);
                        }
                        cancel_sent = Some(now);
                    }
                    (Some(_), Some(at)) if now >= at + CANCEL_GRACE => {
                        worker.kill("the job was cancelled and did not stop");
                        return fail(job, CANCELLED.into());
                    }
                    (Some(_), Some(_)) => {}
                }
            }
            let deadline = match sent_at {
                Some(at) => job.timeout.checked_add(config.kill_grace).and_then(|d| at.checked_add(d)),
                None => worker.started.checked_add(STARTUP_TIMEOUT),
            };
            if deadline.is_some_and(|d| now >= d) {
                return match sent_at {
                    Some(_) => {
                        worker.kill("the job ran past its timeout");
                        let message = format!("Script execution timed out after {} ms", job.timeout.as_millis());
                        fail(job, message)
                    }
                    None => {
                        worker.kill("it did not start in time");
                        fail(job, format!("could not start script worker: no answer within {STARTUP_TIMEOUT:?}"))
                    }
                };
            }
            let limit = config.memory_limit_bytes;
            if limit > 0 && process_memory(worker.child.id()).is_some_and(|used| used > limit) {
                worker.kill("the job used too much memory");
                return fail(job, format!("Script used more than {} MB of memory", limit.div_ceil(1024 * 1024)));
            }
        }
    }
}

struct Pool {
    exe: PathBuf,
    config: IsolationConfig,
    /// With when they became idle, most recent last. Dropping the pool drops
    /// these, which kills and reaps them.
    idle: Mutex<Vec<(Worker, Instant)>>,
    /// A thread is shutting down workers idle for too long.
    reaping: AtomicBool,
}

impl Pool {
    fn spawn(&self) -> std::io::Result<Worker> {
        let mut cmd = Command::new(&self.exe);
        cmd.arg(WORKER_FLAG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Out of the terminal's process group: Ctrl+C in a CLI reaches only
        // the app, which cancels its jobs itself.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        // CREATE_NO_WINDOW: no console window flashing up for the worker.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        let mut child = cmd.spawn()?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (events_tx, events) = mpsc::channel();
        let (stderr_done_tx, stderr_done) = mpsc::channel::<()>();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let (Some(stdin), Some(stdout), Some(stderr_pipe)) = pipes else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("worker pipes missing"));
        };
        // From here on dropping the worker kills the process.
        let mut worker = Worker {
            child,
            stdin,
            events,
            events_tx: events_tx.clone(),
            stderr: stderr.clone(),
            stderr_done,
            ready: false,
            started: Instant::now(),
        };
        let stray = stderr.clone();
        std::thread::Builder::new()
            .name("merpati-script-out".into())
            .spawn(move || read_events(stdout, events_tx, &stray))?;
        std::thread::Builder::new().name("merpati-script-err".into()).spawn(move || {
            read_stderr(stderr_pipe, &stderr);
            drop(stderr_done_tx);
        })?;
        worker.send(&wire::line(&ToWorker::Config(WireConfig::new(&self.config.engine)))?)?;
        tracing::debug!(pid = worker.child.id(), "started script worker");
        Ok(worker)
    }

    fn take(&self) -> std::io::Result<Worker> {
        loop {
            let idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner).pop();
            match idle {
                Some((mut worker, _)) => {
                    if worker.is_alive() {
                        return Ok(worker);
                    }
                }
                None => return self.spawn(),
            }
        }
    }

    fn give_back(self: &Arc<Self>, worker: Worker) {
        let limit = self.config.memory_limit_bytes;
        let bloated = limit > 0 && process_memory(worker.child.id()).is_some_and(|used| used > limit / 2);
        if !bloated {
            let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
            if idle.len() < self.config.max_idle_workers.max(1) {
                idle.push((worker, Instant::now()));
                if idle.len() > 1 && !self.reaping.swap(true, Ordering::AcqRel) {
                    self.start_reaper();
                }
                return;
            }
        }
        // Reaping takes a moment; do not hold up the caller.
        let _ = std::thread::Builder::new().name("merpati-script-reap".into()).spawn(move || drop(worker));
    }

    /// Shuts down workers idle for longer than `idle_timeout`, keeping the
    /// most recently used one; ends when only that one is left.
    fn start_reaper(self: &Arc<Self>) {
        let pool = Arc::downgrade(self);
        let every = (self.config.idle_timeout / 2).max(Duration::from_millis(50));
        let spawned = std::thread::Builder::new().name("merpati-script-idle".into()).spawn(move || {
            loop {
                std::thread::sleep(every);
                let Some(pool) = pool.upgrade() else { return };
                let (expired, done) = {
                    let mut idle = pool.idle.lock().unwrap_or_else(PoisonError::into_inner);
                    let keep_from = idle.len().saturating_sub(1);
                    let timeout = pool.config.idle_timeout;
                    let mut kept = Vec::new();
                    let mut expired = Vec::new();
                    for (i, (worker, since)) in idle.drain(..).enumerate() {
                        if i < keep_from && since.elapsed() >= timeout {
                            expired.push(worker);
                        } else {
                            kept.push((worker, since));
                        }
                    }
                    *idle = kept;
                    let done = idle.len() <= 1;
                    if done {
                        pool.reaping.store(false, Ordering::Release);
                    }
                    (expired, done)
                };
                drop(expired);
                if done {
                    return;
                }
            }
        });
        if spawned.is_err() {
            self.reaping.store(false, Ordering::Release);
        }
    }
}

enum Event {
    Msg(FromWorker<'static>),
    HttpDone {
        id: u64,
        result: Result<ScriptResponse, String>,
    },
    /// The worker closed its stdout: it exited or crashed.
    Eof,
}

/// The app's side of one worker process. Dropping it kills and reaps the
/// process.
struct Worker {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<Event>,
    /// For `pm.sendRequest` threads to report back.
    events_tx: Sender<Event>,
    /// The tail of the worker's stderr, and stdout lines that were not messages.
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Disconnects once the stderr pipe is drained.
    stderr_done: Receiver<()>,
    /// The worker answered `ready`.
    ready: bool,
    started: Instant,
}

impl Worker {
    fn send(&mut self, line: &[u8]) -> std::io::Result<()> {
        self.stdin.write_all(line)?;
        self.stdin.flush()
    }

    fn clear_stderr(&self) {
        self.stderr.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// Takes in what an idle worker sent; false if it is gone.
    fn is_alive(&mut self) -> bool {
        loop {
            match self.events.try_recv() {
                Ok(Event::Msg(FromWorker::Ready)) => self.ready = true,
                // A `pm.sendRequest` answer the last job no longer waited for.
                Ok(Event::HttpDone { .. }) => {}
                Ok(_) | Err(TryRecvError::Disconnected) => return false,
                Err(TryRecvError::Empty) => break,
            }
        }
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Performs `pm.sendRequest` on a helper thread, so the job's deadline,
    /// cancellation and memory are still watched meanwhile.
    fn start_http(&self, call: HttpCall<'static>, http: Option<Arc<dyn ScriptHttp>>) {
        let HttpCall { id, request, scopes } = call;
        let (request, scopes) = (request.into_owned(), scopes.into_owned());
        let Some(http) = http else {
            let result = Err("pm.sendRequest is not available in this context".into());
            let _ = self.events_tx.send(Event::HttpDone { id, result });
            return;
        };
        let events = self.events_tx.clone();
        let spawned = std::thread::Builder::new().name("merpati-script-http".into()).spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| http.send(request, &scopes)))
                .unwrap_or_else(|_| Err("pm.sendRequest failed unexpectedly".into()));
            let _ = events.send(Event::HttpDone { id, result });
        });
        if let Err(e) = spawned {
            let result = Err(format!("could not start pm.sendRequest: {e}"));
            let _ = self.events_tx.send(Event::HttpDone { id, result });
        }
    }

    fn kill(self, why: &str) {
        tracing::warn!(pid = self.child.id(), "killing script worker: {why}");
        drop(self);
    }

    /// Reaps a worker that stopped on its own and describes why: the last
    /// lines of its stderr, else its exit status.
    fn exit_details(mut self) -> String {
        let _ = self.child.kill();
        let status = self.child.wait().ok();
        let _ = self.stderr_done.recv_timeout(Duration::from_millis(250));
        let tail = self.stderr.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let text = String::from_utf8_lossy(&tail);
        tracing::warn!(pid = self.child.id(), ?status, "script worker stopped unexpectedly: {}", text.trim());
        let lines: Vec<&str> =
            text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("note: ")).collect();
        let details = lines[lines.len().saturating_sub(2)..].join("; ");
        let details = match status {
            Some(status) if details.is_empty() => status.to_string(),
            _ => details,
        };
        match details.char_indices().nth(300) {
            Some((cut, _)) => format!("{}...", &details[..cut]),
            None => details,
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_events(stdout: ChildStdout, events: Sender<Event>, stray: &Mutex<Vec<u8>>) {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        if !matches!(reader.read_until(b'\n', &mut line), Ok(n) if n > 0) {
            break;
        }
        match serde_json::from_slice::<FromWorker>(&line) {
            Ok(msg) => {
                if events.send(Event::Msg(msg)).is_err() {
                    return;
                }
            }
            // Not a message (nothing should print to stdout): keep it for diagnostics.
            Err(_) => push_tail(stray, &line),
        }
    }
    let _ = events.send(Event::Eof);
}

fn read_stderr(mut stderr: ChildStderr, tail: &Mutex<Vec<u8>>) {
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => push_tail(tail, &buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn push_tail(tail: &Mutex<Vec<u8>>, data: &[u8]) {
    let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
    tail.extend_from_slice(data);
    if tail.len() > 2 * STDERR_TAIL {
        let cut = tail.len() - STDERR_TAIL;
        tail.drain(..cut);
    }
}

/// Memory a process uses in bytes, `None` where unknown.
#[cfg(target_os = "macos")]
fn process_memory(pid: u32) -> Option<u64> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: both calls only write into the correctly sized, zeroed structs
    // passed to them and report failure through their return values.
    unsafe {
        let mut task: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut task).cast(), size);
        let resident = (n == size).then_some(task.pti_resident_size);
        let mut usage: libc::rusage_info_v2 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&raw mut usage).cast());
        let footprint = (rc == 0).then_some(usage.ri_phys_footprint);
        resident.max(footprint)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_memory(pid: u32) -> Option<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages.saturating_mul(u64::try_from(page).ok().filter(|&p| p > 0).unwrap_or(4096)))
}

/// The working set: the physical memory currently mapped into the process,
/// the Windows counterpart of the resident size. `PrivateUsage` (commit
/// charge) would also count memory that was allocated but never touched or
/// has been paged out.
#[cfg(windows)]
fn process_memory(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: the handle is checked before use and closed exactly once;
    // `K32GetProcessMemoryInfo` only writes into the struct whose size it is
    // given and reports failure through its return value.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        counters.cb = size;
        let ok = K32GetProcessMemoryInfo(process, &raw mut counters, size) != 0;
        CloseHandle(process);
        ok.then_some(counters.WorkingSetSize as u64)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android", windows)))]
fn process_memory(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn measures_process_memory() {
        let before = super::process_memory(std::process::id()).expect("memory of this process");
        let block = vec![1u8; 64 * 1024 * 1024];
        let after = super::process_memory(std::process::id()).unwrap();
        assert!(after >= before + 60 * 1024 * 1024, "{before} -> {after}");
        drop(block);
    }
}
