//! A worker thread owning one boa `Context` that runs jobs one at a time.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::task::{Context as TaskContext, Poll, Waker};
use std::time::{Duration, Instant};

use boa_engine::context::ContextBuilder;
use boa_engine::{Context, JsError, JsObject, JsResult, JsString, JsValue, Script, Source, js_string};
use merpati_core::script::{ScriptError, ScriptJob, ScriptOutcome};

use crate::executor::Executor;
use crate::host::{self, Interrupt, JobData};
use crate::packages::{PackageLimits, PackageLoader, WorkerShared};
use crate::{EngineConfig, WorkerRequest};

const PRELUDE: &str = include_str!("../js/prelude.js");
const CHAI_POSTMAN: &str = include_str!("../js/chai_postman.js");

/// "Clock cycles" between checks of the deadline while user code runs.
const BUDGET: u32 = 20_000;
const SCRIPT_CACHE_LIMIT: usize = 512;
/// Path given to user scripts so stack frames in them can be recognised.
const USER_SCRIPT_PATH: &str = "user-script";
/// How long a script past its deadline may run on, waiting for a host call
/// to stop it cleanly, before it is aborted.
const SOFT_STOP_GRACE: Duration = Duration::from_millis(40);

pub(crate) fn worker_main(config: EngineConfig, rx: Receiver<WorkerRequest>) {
    let mut state: Option<Worker> = None;
    for WorkerRequest { job, reply } in rx {
        let fallback_request = job.request.clone();
        let fallback_vars = job.variables.clone();
        if state.is_none() {
            match Worker::new(&config) {
                Ok(w) => state = Some(w),
                Err(e) => {
                    let _ = reply.send(internal_failure(
                        fallback_request,
                        fallback_vars,
                        format!("could not start the script engine: {e}"),
                    ));
                    continue;
                }
            }
        }
        let worker = state.as_mut().expect("worker initialised above");
        let result = catch_unwind(AssertUnwindSafe(|| worker.run(job)));
        let discard;
        let outcome = match result {
            Ok(outcome) => {
                discard = worker.poisoned;
                outcome
            }
            Err(panic) => {
                discard = true;
                let msg = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".into());
                internal_failure(fallback_request, fallback_vars, format!("internal script engine error: {msg}"))
            }
        };
        // The caller may have given up on us (watchdog); the queue closes
        // when its handle is dropped, which ends this loop.
        let _ = reply.send(outcome);
        if discard {
            // Dropped after replying: tearing down a large heap takes time.
            state = None;
        }
    }
}

fn internal_failure(
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

enum EvalResult {
    Done(JsResult<JsValue>),
    Aborted(Interrupt),
}

struct Hooks {
    mp: JsObject,
    begin: JsObject,
    begin_script: JsObject,
    end_script: JsObject,
    record_error: JsObject,
    next_timer: JsObject,
    clear_timers: JsObject,
    finish: JsObject,
    take_tainted: JsObject,
}

pub(crate) struct Worker {
    ctx: Context,
    executor: Rc<Executor>,
    loader: Rc<PackageLoader>,
    package_limits: PackageLimits,
    hooks: Hooks,
    fire_timers: Script,
    cache: HashMap<String, Script>,
    /// Set when the context can no longer be trusted (a run was aborted
    /// mid-instruction); the worker is rebuilt before the next job.
    pub(crate) poisoned: bool,
}

fn get_fn(obj: &JsObject, name: &str, ctx: &mut Context) -> JsResult<JsObject> {
    obj.get(JsString::from(name), ctx)?
        .as_callable()
        .ok_or_else(|| boa_engine::JsNativeError::typ().with_message(format!("prelude hook {name} missing")).into())
}

fn js_err_to_string(e: JsError, ctx: &mut Context) -> String {
    match e.try_native(ctx) {
        Ok(native) => native.to_string(),
        Err(_) => match e.into_opaque(ctx) {
            Ok(v) => v.to_string(ctx).map(|s| s.to_std_string_escaped()).unwrap_or_else(|_| "unknown error".into()),
            Err(e) => e.to_string(),
        },
    }
}

impl Worker {
    fn new(config: &EngineConfig) -> Result<Worker, String> {
        let executor = Rc::new(Executor::default());
        let loader = Rc::new(PackageLoader::default());
        let mut ctx = ContextBuilder::new()
            .job_executor(executor.clone())
            .module_loader(loader.clone())
            .build()
            .map_err(|e| e.to_string())?;
        ctx.insert_data(WorkerShared { executor: executor.clone(), loader: loader.clone() });
        {
            let limits = ctx.runtime_limits_mut();
            limits.set_loop_iteration_limit(config.loop_iteration_limit);
            limits.set_recursion_limit(config.recursion_limit);
            limits.set_stack_size_limit(config.stack_size_limit);
        }
        host::register(&mut ctx).map_err(|e| e.to_string())?;
        let src = format!("{PRELUDE}\n;\n{CHAI_POSTMAN}\n;\n__mp_snapshot();\n");
        ctx.eval(Source::from_bytes(src.as_bytes()))
            .map_err(|e| format!("prelude failed: {}", js_err_to_string(e, &mut ctx)))?;
        let mp = ctx
            .global_object()
            .get(js_string!("__mp"), &mut ctx)
            .map_err(|e| e.to_string())?
            .as_object()
            .ok_or("prelude did not define __mp")?;
        let mut hook = |name: &str| get_fn(&mp, name, &mut ctx).map_err(|e| e.to_string());
        let hooks = Hooks {
            begin: hook("begin")?,
            begin_script: hook("beginScript")?,
            end_script: hook("endScript")?,
            record_error: hook("recordError")?,
            next_timer: hook("nextTimer")?,
            clear_timers: hook("clearTimers")?,
            finish: hook("finish")?,
            take_tainted: hook("takeTainted")?,
            mp: mp.clone(),
        };
        loader.set_import_hook(get_fn(&mp, "importValue", &mut ctx).map_err(|e| e.to_string())?);
        let fire_timers =
            Script::parse(Source::from_bytes("__mp.fireTimers();"), None, &mut ctx).map_err(|e| e.to_string())?;
        let package_limits = PackageLimits {
            module_bytes: config.package_module_limit,
            graph_bytes: config.package_graph_limit,
            fetch_timeout: config.package_fetch_timeout,
        };
        Ok(Worker { ctx, executor, loader, package_limits, hooks, fire_timers, cache: HashMap::new(), poisoned: false })
    }

    fn call(&mut self, f: &JsObject, args: &[JsValue]) -> JsResult<JsValue> {
        let this = JsValue::from(self.hooks.mp.clone());
        f.call(&this, args, &mut self.ctx)
    }

    fn record_error(&mut self, location: &str, message: &str) {
        let f = self.hooks.record_error.clone();
        let _ = self.call(&f, &[JsString::from(location).into(), JsString::from(message).into()]);
    }

    /// Compiles (or fetches from the cache) a user script wrapped in
    /// `__mp.exec(function () { ... })`. Returns the script and the length of
    /// the wrapper prefix on line 1.
    fn compile(&mut self, code: &str) -> Result<(Script, usize), String> {
        let is_async = code.contains("await");
        let prefix = if is_async { "__mp.exec(async function () {" } else { "__mp.exec(function () {" };
        let key = format!("{}{code}", if is_async { 'a' } else { 's' });
        if let Some(s) = self.cache.get(&key) {
            return Ok((s.clone(), prefix.len()));
        }
        let wrapped = format!("{prefix}{code}\n}});");
        let source = Source::from_bytes(wrapped.as_bytes()).with_path(std::path::Path::new(USER_SCRIPT_PATH));
        match Script::parse(source, None, &mut self.ctx) {
            Ok(script) => {
                if self.cache.len() >= SCRIPT_CACHE_LIMIT {
                    self.cache.clear();
                }
                self.cache.insert(key, script.clone());
                Ok((script, prefix.len()))
            }
            Err(e) => Err(syntax_message(&js_err_to_string(e, &mut self.ctx), prefix.len())),
        }
    }

    /// Evaluates a script, yielding regularly to check the deadline.
    ///
    /// Past the deadline (or on cancellation) every host call throws an
    /// uncatchable error, which unwinds cleanly and keeps partial results.
    /// Code that makes no host calls is aborted after a short grace period;
    /// that leaves the VM mid-execution and poisons the context.
    fn eval_budgeted(&mut self, script: &Script, deadline: Instant, cancel: &AtomicBool) -> EvalResult {
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);
        let mut fut = pin!(script.evaluate_async_with_budget(&mut self.ctx, BUDGET));
        let mut stop_seen: Option<(Instant, Interrupt)> = None;
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(r) => return EvalResult::Done(r),
                Poll::Pending => {
                    let now = Instant::now();
                    match stop_seen {
                        Some((at, why)) if now >= at + SOFT_STOP_GRACE => return EvalResult::Aborted(why),
                        Some(_) => {}
                        None if cancel.load(Ordering::Relaxed) => stop_seen = Some((now, Interrupt::Cancelled)),
                        None if now >= deadline => stop_seen = Some((now, Interrupt::Timeout)),
                        None => {}
                    }
                }
            }
        }
    }

    /// Runs promise jobs and timers until nothing is pending.
    fn drive_async(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), DriveStop> {
        loop {
            if let Err(e) = self.ctx.run_jobs() {
                return Err(DriveStop::Error(js_err_to_string(e, &mut self.ctx)));
            }
            let f = self.hooks.next_timer.clone();
            let next = match self.call(&f, &[]) {
                Ok(v) => v.as_number().unwrap_or(-1.0),
                Err(e) => return Err(DriveStop::Error(js_err_to_string(e, &mut self.ctx))),
            };
            if next < 0.0 {
                return Ok(());
            }
            let wake_at = Instant::now() + Duration::from_millis(next.ceil().max(0.0) as u64);
            while Instant::now() < wake_at {
                if cancel.load(Ordering::Relaxed) {
                    return Err(DriveStop::Interrupt(Interrupt::Cancelled));
                }
                if Instant::now() >= deadline {
                    return Err(DriveStop::Interrupt(Interrupt::Timeout));
                }
                let left = wake_at.saturating_duration_since(Instant::now()).min(Duration::from_millis(10));
                std::thread::sleep(left);
            }
            let script = self.fire_timers.clone();
            match self.eval_budgeted(&script, deadline, cancel) {
                EvalResult::Done(Ok(_)) => {}
                EvalResult::Done(Err(e)) => return Err(DriveStop::Error(js_err_to_string(e, &mut self.ctx))),
                EvalResult::Aborted(i) => {
                    self.poisoned = true;
                    return Err(DriveStop::Interrupt(i));
                }
            }
        }
    }

    pub(crate) fn run(&mut self, job: ScriptJob) -> ScriptOutcome {
        let started = Instant::now();
        let deadline = started + job.timeout;
        let cancel = job.cancel.clone();
        let fallback_request = job.request.clone();
        let fallback_vars = job.variables.clone();
        let timeout_ms = job.timeout.as_millis();

        let input = serde_json::json!({
            "event": job.event.postman_name(),
            "info": job.info,
            "request": job.request,
            "response": job.response.as_ref().map(|r| serde_json::json!({
                "code": r.code,
                "status": r.status,
                "headers": r.headers,
                "responseTime": r.response_time_ms,
                "responseSize": r.response_size,
                "bodySize": r.body.len(),
            })),
            "variables": job.variables,
            "cookies": job.cookies,
            "locations": job.scripts.iter().map(|s| s.location.clone()).collect::<Vec<_>>(),
            "protocol": job.protocol,
            "packages": job.packages.is_some(),
        });
        self.executor.clear();
        self.ctx.insert_data(JobData {
            deadline,
            timeout_ms,
            cancel: cancel.clone(),
            response_body: job.response.as_ref().map(|r| r.body.clone()),
            http: job.http.clone(),
            interrupted: Default::default(),
            packages: job.packages.map(std::sync::Arc::new),
            package_limits: self.package_limits,
            pkg_session: Default::default(),
        });

        let begin = self.hooks.begin.clone();
        if let Err(e) = self.call(&begin, &[JsString::from(input.to_string()).into()]) {
            let msg = js_err_to_string(e, &mut self.ctx);
            self.end_job_data();
            self.poisoned = true;
            return internal_failure(fallback_request, fallback_vars, format!("could not prepare the sandbox: {msg}"));
        }

        let interrupt_message = |i: Interrupt| match i {
            Interrupt::Timeout => format!("Script execution timed out after {timeout_ms} ms"),
            Interrupt::Cancelled => "Script execution was cancelled".to_string(),
        };

        let mut stopped = false;
        for source in &job.scripts {
            if source.code.trim().is_empty() {
                continue;
            }
            let (script, prefix_len) = match self.compile(&source.code) {
                Ok(compiled) => compiled,
                Err(msg) => {
                    self.record_error(&source.location, &msg);
                    continue;
                }
            };
            let f = self.hooks.begin_script.clone();
            let _ = self.call(&f, &[JsString::from(source.location.as_str()).into(), JsValue::from(prefix_len as i32)]);
            match self.eval_budgeted(&script, deadline, &cancel) {
                EvalResult::Done(Ok(_)) => {}
                EvalResult::Done(Err(e)) => {
                    let msg = self.describe_engine_error(e, &interrupt_message);
                    self.record_error(&source.location, &msg);
                    stopped = self.interrupted().is_some();
                }
                EvalResult::Aborted(i) => {
                    self.poisoned = true;
                    self.end_job_data();
                    return aborted_outcome(fallback_request, fallback_vars, &source.location, interrupt_message(i));
                }
            }
            if !stopped {
                match self.drive_async(deadline, &cancel) {
                    Ok(()) => {}
                    Err(DriveStop::Interrupt(i)) if self.poisoned => {
                        self.end_job_data();
                        return aborted_outcome(
                            fallback_request,
                            fallback_vars,
                            &source.location,
                            interrupt_message(i),
                        );
                    }
                    Err(DriveStop::Interrupt(i)) => {
                        self.record_error(&source.location, &interrupt_message(i));
                        stopped = true;
                    }
                    Err(DriveStop::Error(msg)) => {
                        let msg = match self.interrupted() {
                            Some(i) => interrupt_message(i),
                            None => friendly_limit_message(&msg),
                        };
                        self.record_error(&source.location, &msg);
                        stopped = self.interrupted().is_some();
                    }
                }
            }
            // The bookkeeping hooks make no host calls, so they still work
            // after the deadline has passed.
            let f = self.hooks.end_script.clone();
            let _ = self.call(&f, &[]);
            if stopped {
                let f = self.hooks.clear_timers.clone();
                let _ = self.call(&f, &[]);
                break;
            }
        }
        if let Some(msg) = self.end_job_data() {
            self.record_error("packages", &msg);
        }
        self.executor.clear();

        let finish = self.hooks.finish.clone();
        let out = match self.call(&finish, &[]) {
            Ok(v) => v.as_string().map(|s| s.to_std_string_escaped()),
            Err(e) => {
                let msg = js_err_to_string(e, &mut self.ctx);
                self.poisoned = true;
                return internal_failure(
                    fallback_request,
                    fallback_vars,
                    format!("could not collect script results: {msg}"),
                );
            }
        };
        // The job changed shared objects in a way the prelude cannot undo
        // (frozen built-ins, non-configurable properties): start afresh.
        let take_tainted = self.hooks.take_tainted.clone();
        if !matches!(self.call(&take_tainted, &[]), Ok(v) if v.as_boolean() == Some(false)) {
            self.poisoned = true;
        }
        match out.as_deref().map(serde_json::from_str::<ScriptOutcome>) {
            Some(Ok(outcome)) => outcome,
            Some(Err(e)) => {
                self.poisoned = true;
                internal_failure(fallback_request, fallback_vars, format!("invalid script results: {e}"))
            }
            None => {
                self.poisoned = true;
                internal_failure(fallback_request, fallback_vars, "script results missing".into())
            }
        }
    }

    /// Removes the job data, writing new lockfile pins; returns a message
    /// when the lockfile could not be written.
    fn end_job_data(&mut self) -> Option<String> {
        self.loader.end_job();
        let data = self.ctx.remove_data::<JobData>()?;
        let session = data.pkg_session.borrow_mut().take()?;
        let result = session.borrow_mut().flush();
        result.err()
    }

    fn interrupted(&self) -> Option<Interrupt> {
        self.ctx.get_data::<JobData>().and_then(|d| d.interrupted.get())
    }

    fn describe_engine_error(&mut self, e: JsError, interrupt_message: &dyn Fn(Interrupt) -> String) -> String {
        if let Some(i) = self.interrupted() {
            return interrupt_message(i);
        }
        friendly_limit_message(&js_err_to_string(e, &mut self.ctx))
    }
}

/// Results after an aborted evaluation: the context is no longer trusted, so
/// only the original request and variables are returned.
fn aborted_outcome(
    request: merpati_core::script::ScriptRequest,
    variables: merpati_core::VariableScopes,
    location: &str,
    message: String,
) -> ScriptOutcome {
    ScriptOutcome {
        request,
        variables,
        errors: vec![ScriptError { location: location.to_string(), message }],
        ..Default::default()
    }
}

enum DriveStop {
    Interrupt(Interrupt),
    Error(String),
}

/// Cleans up a parse error: drops boa's "(native)" marker and corrects the
/// column on the first line for the wrapper prepended to user code.
fn syntax_message(msg: &str, prefix_len: usize) -> String {
    let msg = msg.trim_end().trim_end_matches("(native)").trim_end();
    if let Some(idx) = msg.find("at line 1, col ") {
        let rest = &msg[idx + "at line 1, col ".len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(col) = digits.parse::<usize>() {
            let fixed = col.saturating_sub(prefix_len).max(1);
            return format!("{}at line 1, col {fixed}{}", &msg[..idx], &rest[digits.len()..]);
        }
    }
    msg.to_string()
}

fn friendly_limit_message(msg: &str) -> String {
    if msg.contains("maximum number of iteration loops") {
        "RangeError: Script stopped after too many loop iterations (possible infinite loop)".into()
    } else if msg.contains("maximum number of recursive calls") || msg.contains("maximum stack size") {
        "RangeError: Maximum call stack size exceeded".into()
    } else {
        msg.to_string()
    }
}
