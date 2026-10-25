//! The worker process: reads jobs from stdin, runs them with an in-process
//! [`BoaEngine`] and writes the outcomes to stdout.
//!
//! Threads: the main thread runs one job at a time; a reader thread takes
//! the commands from stdin, so `pm.sendRequest` answers and cancellation
//! arrive while a job runs, and ends the process when stdin closes.

use std::borrow::Cow;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use merpati_core::script::{ScriptEngine, ScriptHttp, ScriptJob, ScriptRequest, ScriptResponse};
use merpati_core::variables::VariableScopes;

use super::wire::{self, FromWorker, HttpCall, PROTOCOL_VERSION, ToWorker, WireConfig};
use crate::{BoaEngine, EngineConfig};

/// The in-process watchdog never gives up on a job here: the app kills the
/// whole process instead, which also stops native code.
const NO_WATCHDOG: Duration = Duration::from_secs(24 * 3600);
/// `pm.sendRequest` waits this much past the job deadline, so the script
/// is interrupted with the usual timeout error once it gives up.
const HTTP_DEADLINE_SLACK: Duration = Duration::from_millis(50);
const HTTP_POLL: Duration = Duration::from_millis(20);

type HttpReply = (u64, Result<ScriptResponse, String>);

pub(crate) fn main() -> ! {
    let code = match serve() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("merpati script worker: {e}");
            2
        }
    };
    std::process::exit(code)
}

fn serve() -> Result<(), String> {
    let mut line = Vec::new();
    if std::io::stdin().lock().read_until(b'\n', &mut line).map_err(|e| e.to_string())? == 0 {
        return Ok(());
    }
    let config = match serde_json::from_slice::<ToWorker>(&line) {
        Ok(ToWorker::Config(c)) => engine_config(c)?,
        Ok(_) => return Err("expected the configuration first".into()),
        Err(e) => return Err(format!("invalid configuration: {e}")),
    };
    let engine = BoaEngine::with_config(config);
    engine.run(crate::warm_up_job());
    let (jobs_tx, jobs) = mpsc::channel();
    std::thread::Builder::new()
        .name("merpati-script-stdin".into())
        .spawn(move || read_commands(jobs_tx))
        .map_err(|e| e.to_string())?;
    send(&FromWorker::Ready)?;
    for job in jobs {
        let outcome = engine.run(job);
        send(&FromWorker::Done(Box::new(Cow::Borrowed(&outcome))))?;
    }
    Ok(())
}

fn engine_config(c: WireConfig) -> Result<EngineConfig, String> {
    if c.protocol != PROTOCOL_VERSION {
        return Err(format!(
            "incompatible script worker (protocol {PROTOCOL_VERSION}, the app speaks protocol {})",
            c.protocol
        ));
    }
    Ok(EngineConfig {
        loop_iteration_limit: c.loop_iteration_limit,
        recursion_limit: c.recursion_limit,
        stack_size_limit: c.stack_size_limit,
        thread_stack_bytes: c.thread_stack_bytes,
        max_idle_workers: 1,
        watchdog_grace: NO_WATCHDOG,
        package_module_limit: c.package_module_limit,
        package_graph_limit: c.package_graph_limit,
        package_fetch_timeout: Duration::from_millis(c.package_fetch_timeout_ms),
    })
}

/// Writes one message. Lines are written whole under the stdout lock, so the
/// main thread and `pm.sendRequest` on the engine's thread do not interleave.
fn send(msg: &FromWorker) -> Result<(), String> {
    let buf = wire::line(msg).map_err(|e| e.to_string())?;
    let mut out = std::io::stdout().lock();
    out.write_all(&buf).and_then(|()| out.flush()).map_err(|e| format!("could not write to the app: {e}"))
}

fn fail(message: &str) -> ! {
    eprintln!("merpati script worker: {message}");
    std::process::exit(2)
}

fn read_commands(jobs: Sender<ScriptJob>) {
    let mut input = std::io::stdin().lock();
    let mut line = Vec::new();
    // Cancel flag and `pm.sendRequest` answers of the current job.
    let mut current: Option<(Arc<AtomicBool>, Sender<HttpReply>)> = None;
    loop {
        line.clear();
        // The app shut the worker down or exited: stop, even mid-job.
        if !matches!(input.read_until(b'\n', &mut line), Ok(n) if n > 0) {
            std::process::exit(0);
        }
        match serde_json::from_slice::<ToWorker>(&line) {
            Ok(ToWorker::Job(wire)) => {
                let cancel = Arc::new(AtomicBool::new(false));
                let (reply_tx, replies) = mpsc::channel();
                let http = wire.http.then(|| {
                    let http = ChildHttp {
                        replies: Mutex::new(replies),
                        cancel: cancel.clone(),
                        deadline: Instant::now().checked_add(wire.timeout() + HTTP_DEADLINE_SLACK),
                    };
                    Arc::new(http) as Arc<dyn ScriptHttp>
                });
                let job = wire.into_job(http, cancel.clone()).unwrap_or_else(|e| fail(&format!("invalid job: {e}")));
                current = Some((cancel, reply_tx));
                if jobs.send(job).is_err() {
                    fail("the job runner stopped");
                }
            }
            Ok(ToWorker::HttpReply { id, result }) => {
                if let Some((_, replies)) = &current {
                    let _ = replies.send((id, result.map_err(Cow::into_owned).and_then(|r| r.into_response())));
                }
            }
            Ok(ToWorker::Cancel) => {
                if let Some((cancel, _)) = &current {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            Ok(ToWorker::Config(_)) => {}
            Err(e) => fail(&format!("invalid message: {e}")),
        }
    }
}

/// `pm.sendRequest` inside the worker: the app, which owns the job's real
/// [`ScriptHttp`], performs the request and sends the response back.
struct ChildHttp {
    replies: Mutex<Receiver<HttpReply>>,
    cancel: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

impl ScriptHttp for ChildHttp {
    fn send(&self, request: ScriptRequest, scopes: &VariableScopes) -> Result<ScriptResponse, String> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let call = HttpCall { id, request: Cow::Owned(request), scopes: Cow::Borrowed(scopes) };
        send(&FromWorker::Http(Box::new(call)))?;
        let replies = self.replies.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            // A stopped job stops waiting (the app may still be waiting for
            // the server); the caller then reports the interruption.
            if self.cancel.load(Ordering::Relaxed) || self.deadline.is_some_and(|d| Instant::now() >= d) {
                return Err("pm.sendRequest was interrupted".into());
            }
            match replies.recv_timeout(HTTP_POLL) {
                Ok((got, result)) if got == id => return result,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("the app stopped answering".into()),
            }
        }
    }
}
