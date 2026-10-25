//! Running flows.
//!
//! A block that finishes on a port activates the blocks connected to that
//! port, depth first; when a port fans out, targets run top to bottom, then
//! left to right (canvas position), then in edge order. A block runs once per
//! activation. A Loop runs its body (everything reachable from `item`) to
//! completion for each element before the next one, then fires `done`.
//! Without Parallel blocks execution is sequential and deterministic.
//!
//! Every line of execution is a `Run`: an activation stack of frames (the
//! root frame plus one per running loop) and its own variables and block
//! records. A Parallel block starts one branch `Run` per edge leaving its
//! `branches` port (at most `max_concurrent` at a time), from copies of the
//! current state, and polls them all on the same task, so their requests,
//! AI calls, delays and scripts overlap; a Join that only some branches can
//! reach starts one more `Run` (a continuation) once those are done. What
//! all of them share lives in `Shared`: the event sink, the step budget, the
//! result being built and the stop/cancel state. See `Run::run_group` for
//! how branches end, merge back and fire Joins, and why the outcome does not
//! depend on timing.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use indexmap::IndexMap;
use merpati_ai::merpati_mcp::{McpClient, McpConfig};
use merpati_ai::{AiConfig, AiError, AiEvent};
use merpati_core::auth::Auth;
use merpati_core::model::{Collection, HttpRequest, Item, RequestItem, RequestKind, Scripts};
use merpati_core::protocols::{AiMessage, AiRequest};
use merpati_core::script::{LogLevel, ScriptEvent, ScriptInfo, ScriptJob, ScriptRequest, ScriptSource, TestResult};
use merpati_core::variables::{self, ScopeTypes, VarType, VariableScopes, value_to_string};
use merpati_runner::{ConsoleKind, ExecuteInput, Execution, Services};
use serde::Serialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::js;
use crate::model::*;
use crate::template::{
    BlockRecord, RefCtx, Renderer, collect_refs, compare, flow_root, strip_code_fence, to_number, truthy,
};

/// Default limit of block executions per run.
pub const DEFAULT_MAX_STEPS: usize = 10_000;
/// Response bodies larger than this are cut in block outputs.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// What a run needs besides the flow.
#[derive(Clone)]
pub struct FlowEnv {
    pub services: Services,
    /// Collections whose requests Request/AI blocks may reference (usually
    /// every collection of the workspace).
    pub collections: Vec<Collection>,
    /// Base for relative file paths in request bodies.
    pub base_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub struct FlowRunOptions {
    /// Values for the flow's declared inputs (others are passed through).
    pub inputs: IndexMap<String, Value>,
    /// Initial flow variables.
    pub variables: IndexMap<String, Value>,
    /// Overrides `FlowSettings::max_steps`.
    pub max_steps: Option<usize>,
}

/// Progress of a run, delivered in order. Events of parallel branches
/// interleave; `step` ties a block's events together.
#[derive(Clone, Debug)]
pub enum FlowEvent {
    Started {
        flow_id: String,
        flow_name: String,
    },
    BlockStarted {
        /// 1-based execution number; matches the `BlockFinished` that follows.
        step: usize,
        block: String,
        label: String,
        kind: BlockKind,
        /// Nesting level: enclosing loops plus enclosing parallel branches.
        depth: usize,
        /// Index of the innermost enclosing loop iteration.
        iteration: Option<usize>,
        /// Position among parallel branches (`1`, `2`, `2.1` in a nested
        /// Parallel, `1+2` after a Join that waited only for branches 1 and
        /// 2); None outside of branches.
        branch: Option<String>,
    },
    /// The full request execution of a Request block (response, console,
    /// tests), sent right before its `BlockFinished`.
    Execution {
        step: usize,
        block: String,
        execution: Box<Execution>,
    },
    /// A piece of the answer of an AI block that streams, sent while the
    /// block runs. The pieces of one step, concatenated, are the answer so
    /// far (tool-call rounds are separated by a blank line).
    AiDelta {
        step: usize,
        block: String,
        text: String,
    },
    BlockFinished {
        step: usize,
        block: String,
        /// The port the block finished on (`failure` for failed blocks).
        port: String,
        output: Arc<Value>,
        duration_ms: f64,
        failed: bool,
        error: Option<String>,
        tests: Vec<TestResult>,
    },
    Log {
        block: Option<String>,
        level: LogLevel,
        message: String,
    },
    /// An unhandled failure or a run-level problem.
    Error {
        block: Option<String>,
        message: String,
    },
    Finished {
        outputs: IndexMap<String, Value>,
        duration_ms: f64,
        success: bool,
    },
}

/// One block execution, for reports.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StepRecord {
    pub step: usize,
    pub block: String,
    pub label: String,
    pub kind: BlockKind,
    pub port: String,
    pub failed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the block started, in ms since the run started (parallel
    /// branches overlap).
    pub started_ms: f64,
    pub duration_ms: f64,
    /// Enclosing loops plus enclosing parallel branches.
    pub depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iteration: Option<usize>,
    /// Position among parallel branches (see [`FlowEvent::BlockStarted`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// HTTP status of Request blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestResult>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Failure {
    pub block: String,
    pub label: String,
    pub message: String,
    /// A `failure` edge took over.
    pub handled: bool,
}

/// Result of a run.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FlowResult {
    pub flow_id: String,
    pub flow_name: String,
    pub success: bool,
    pub cancelled: bool,
    /// Named results from Output blocks.
    pub outputs: IndexMap<String, Value>,
    /// Block executions in step order (the order they started).
    pub steps: Vec<StepRecord>,
    pub requests: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    pub tests_skipped: usize,
    pub failures: Vec<Failure>,
    /// Run-level errors (missing inputs, step limit, ...).
    pub errors: Vec<String>,
    /// Final flow variables.
    pub variables: IndexMap<String, Value>,
    /// Final collection variables per collection id (only collections used).
    pub collection_variables: IndexMap<String, IndexMap<String, String>>,
    /// Latest record of every block that ran, by block id.
    pub blocks: IndexMap<String, BlockRecord>,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: f64,
}

impl FlowResult {
    pub fn unhandled_failures(&self) -> usize {
        self.failures.iter().filter(|f| !f.handled).count()
    }
}

/// Merges declared input defaults with provided values. Errors list the
/// required inputs that have no value.
pub fn resolve_inputs(flow: &Flow, provided: &IndexMap<String, Value>) -> Result<IndexMap<String, Value>, Vec<String>> {
    let mut out = IndexMap::new();
    let mut errors = Vec::new();
    for i in &flow.inputs {
        match provided.get(&i.name) {
            Some(v) => {
                out.insert(i.name.clone(), v.clone());
            }
            None if !i.default.is_null() => {
                out.insert(i.name.clone(), i.default.clone());
            }
            None if i.required => errors.push(format!("input \"{}\" is required", i.name)),
            None => {}
        }
    }
    for (k, v) in provided {
        if !out.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    if errors.is_empty() { Ok(out) } else { Err(errors) }
}

/// Parses a `success_status` spec (`2xx`, `200-299`, `201`, comma
/// separated) into inclusive ranges. Empty → no ranges (status < 400).
pub fn parse_status_spec(spec: &str) -> Result<Vec<(u16, u16)>, String> {
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let bad = || format!("invalid status \"{part}\" (use 2xx, 200-299 or 201)");
        let lower = part.to_ascii_lowercase();
        if let Some(d) = lower.strip_suffix("xx") {
            let d: u16 = d.parse().map_err(|_| bad())?;
            if !(1..=5).contains(&d) {
                return Err(bad());
            }
            out.push((d * 100, d * 100 + 99));
        } else if let Some((a, b)) = lower.split_once('-') {
            let a: u16 = a.trim().parse().map_err(|_| bad())?;
            let b: u16 = b.trim().parse().map_err(|_| bad())?;
            if a > b {
                return Err(bad());
            }
            out.push((a, b));
        } else {
            let n: u16 = lower.parse().map_err(|_| bad())?;
            out.push((n, n));
        }
    }
    Ok(out)
}

fn status_ok(status: u16, ranges: &[(u16, u16)]) -> bool {
    if ranges.is_empty() { status < 400 } else { ranges.iter().any(|(a, b)| (*a..=*b).contains(&status)) }
}

#[derive(Clone, Debug)]
struct Activation {
    block: String,
    port: String,
    payload: Arc<Value>,
    /// The block whose edge activated this one (None for Start and for a
    /// Join firing after its parallel group).
    from: Option<String>,
    /// A Join firing after its parallel group; `payload` is its output.
    join: bool,
}

impl Activation {
    fn new(block: &str, port: &str, payload: Arc<Value>, from: Option<&str>) -> Self {
        Activation {
            block: block.to_string(),
            port: port.to_string(),
            payload,
            from: from.map(str::to_string),
            join: false,
        }
    }
}

struct LoopState {
    block: String,
    items: Vec<Value>,
    index: usize,
    broken: bool,
    /// `{ item, index, count, first, last }` of the current iteration.
    ctx: Value,
}

enum FrameKind {
    Root,
    Loop(LoopState),
}

struct Frame {
    kind: FrameKind,
    /// Pending activations; the last one runs next.
    stack: Vec<Activation>,
}

/// A loop running around a parallel branch (in the run that started it).
#[derive(Clone)]
struct OuterLoop {
    block: String,
    index: usize,
    count: usize,
    ctx: Value,
}

/// The innermost loop around a block, wherever it runs.
struct LoopView<'x> {
    index: usize,
    count: usize,
    ctx: &'x Value,
}

/// What a branch hands back to its Parallel block when it is done.
#[derive(Default)]
struct GroupOut {
    /// `(join, from block, payload)` for every Join activation, in order.
    arrivals: Vec<(String, String, Arc<Value>)>,
    /// Loops around the group that a `break` edge in the branch stopped.
    breaks: Vec<String>,
}

/// What reached a Join: `(from block, value)` in branch order.
type Arrivals = Vec<(String, Arc<Value>)>;

/// The state branches start from, to tell their changes apart.
struct Snapshot {
    globals: IndexMap<String, String>,
    environment: IndexMap<String, String>,
    collection: IndexMap<String, String>,
    /// Types of `globals`, `environment` and `collection`.
    types: ScopeTypes,
    flow_vars: IndexMap<String, Value>,
    collection_vars: IndexMap<String, IndexMap<String, String>>,
    collection_types: IndexMap<String, IndexMap<String, VarType>>,
    /// `runs` of every block record.
    runs: HashMap<String, usize>,
}

struct Outcome {
    port: &'static str,
    output: Value,
    error: Option<String>,
    tests: Vec<TestResult>,
    status: Option<u16>,
    execution: Option<Box<Execution>>,
}

impl Outcome {
    fn ok(port: &'static str, output: Value) -> Self {
        Outcome { port, output, error: None, tests: Vec::new(), status: None, execution: None }
    }

    fn fail(message: impl Into<String>) -> Self {
        let message = message.into();
        Outcome {
            port: PORT_FAILURE,
            output: json!({ "error": message }),
            error: Some(message),
            tests: Vec::new(),
            status: None,
            execution: None,
        }
    }
}

struct JsOutcome {
    value: Result<Value, String>,
    tests: Vec<TestResult>,
}

fn log_level(k: ConsoleKind) -> LogLevel {
    match k {
        ConsoleKind::Log => LogLevel::Log,
        ConsoleKind::Info => LogLevel::Info,
        ConsoleKind::Warn => LogLevel::Warn,
        ConsoleKind::Error => LogLevel::Error,
        ConsoleKind::Debug | ConsoleKind::Network => LogLevel::Debug,
    }
}

fn tests_summary(tests: &[TestResult]) -> Option<String> {
    let failed: Vec<&TestResult> = tests.iter().filter(|t| !t.passed && !t.skipped).collect();
    if failed.is_empty() {
        return None;
    }
    let names: Vec<String> = failed
        .iter()
        .map(|t| match &t.error {
            Some(e) => format!("\"{}\" ({e})", t.name),
            None => format!("\"{}\"", t.name),
        })
        .collect();
    Some(format!("{} test{} failed: {}", failed.len(), if failed.len() == 1 { "" } else { "s" }, names.join(", ")))
}

/// The output of a Request block.
fn request_output(exec: &Execution, ok: bool) -> Value {
    let (method, url) = match (&exec.sent, &exec.prepared, &exec.request) {
        (Some(s), _, _) => (s.method.clone(), s.url.clone()),
        (None, Some(p), _) => (p.method.clone(), p.url.clone()),
        (None, None, Some(r)) => (r.method.clone(), r.url.clone()),
        _ => (String::new(), String::new()),
    };
    let tests: Vec<Value> = exec
        .tests
        .iter()
        .map(|t| json!({"name": t.name, "passed": t.passed, "skipped": t.skipped, "error": t.error}))
        .collect();
    let error = exec.error.as_ref().map(|e| e.message.clone());
    let Some(r) = &exec.response else {
        return json!({
            "status": null, "status_text": "", "ok": ok, "headers": {}, "body": "", "json": null,
            "time_ms": 0.0, "size": 0, "method": method, "url": url, "tests": tests, "error": error,
            "skipped": exec.skipped,
        });
    };
    let mut headers = serde_json::Map::new();
    for (k, v) in &r.headers {
        let k = k.to_ascii_lowercase();
        match headers.get_mut(&k) {
            Some(Value::String(prev)) => {
                prev.push_str(", ");
                prev.push_str(v);
            }
            _ => {
                headers.insert(k, Value::String(v.clone()));
            }
        }
    }
    let size = r.body.len();
    let truncated = size > MAX_BODY_BYTES as u64;
    let bytes = if truncated {
        r.body.read_range(0, MAX_BODY_BYTES).unwrap_or_default()
    } else {
        r.body.read_all().map(|b| b.to_vec()).unwrap_or_default()
    };
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let trimmed = body.trim_start();
    let parsed =
        if !truncated && (trimmed.starts_with('{') || trimmed.starts_with('[') || r.content_type().contains("json")) {
            serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
    let mut out = json!({
        "status": r.status,
        "status_text": r.status_text,
        "ok": ok,
        "headers": headers,
        "body": body,
        "json": parsed,
        "time_ms": r.timings.total_ms,
        "size": r.total_size(),
        "method": method,
        "url": if r.url.is_empty() { url } else { r.url.clone() },
        "tests": tests,
        "error": error,
        "skipped": exec.skipped,
    });
    if truncated {
        out["body_truncated"] = Value::Bool(true);
    }
    out
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the main run and all its parallel branches share. Locks are only
/// taken for short synchronous updates, never across an await.
struct Shared<'a> {
    env: &'a FlowEnv,
    flow: &'a Flow,
    /// The caller's token.
    cancel: &'a CancellationToken,
    /// Child of `cancel`, also cancelled when the run stops (an unhandled
    /// failure, the step limit, a fatal error), so blocks still running in
    /// other branches end at once. Blocks wait on this one.
    halt: CancellationToken,
    inputs: IndexMap<String, Value>,
    max_steps: usize,
    started: Instant,
    sink: Mutex<&'a mut (dyn FnMut(FlowEvent) + Send)>,
    state: Mutex<State>,
    inline_cache: Mutex<HashMap<String, Arc<(Collection, String)>>>,
    refs_cache: Mutex<HashMap<String, Vec<String>>>,
}

#[derive(Default)]
struct State {
    /// Steps, failures, tests, outputs, errors and request count so far.
    result: FlowResult,
    /// Steps started (the shared `max_steps` budget).
    step: usize,
    stop: bool,
    cancelled: bool,
    explicit_fail: bool,
}

impl Shared<'_> {
    fn emit(&self, e: FlowEvent) {
        let mut sink = lock(&self.sink);
        (*sink)(e);
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn stopped(&self) -> bool {
        self.state().stop
    }

    /// No new blocks start, running ones are interrupted.
    fn stop_run(&self) {
        self.state().stop = true;
        self.halt.cancel();
    }

    /// Records a cancellation by the caller; true when there was one.
    fn note_cancel(&self) -> bool {
        if !self.cancel.is_cancelled() {
            return false;
        }
        let mut st = self.state();
        st.cancelled = true;
        st.stop = true;
        true
    }

    /// Error of a block interrupted by `halt`.
    fn interrupted(&self) -> String {
        if self.cancel.is_cancelled() { "cancelled".into() } else { "stopped: another branch stopped the run".into() }
    }
}

/// One line of execution: the main run, or a branch of a Parallel block with
/// its own activation stack and its own copies of the variables and block
/// records (merged back when the branches are done).
struct Run<'r, 'a> {
    sh: &'r Shared<'a>,
    /// Globals, environment and the collection scope of blocks without a
    /// collection.
    vars: VariableScopes,
    flow_vars: IndexMap<String, Value>,
    records: HashMap<String, BlockRecord>,
    collection_vars: IndexMap<String, IndexMap<String, String>>,
    /// Types of `collection_vars` (names absent are plain strings).
    collection_types: IndexMap<String, IndexMap<String, VarType>>,
    used_collections: Vec<String>,
    frames: Vec<Frame>,
    /// Loops running around the Parallel block that started this branch,
    /// outermost first.
    outer_loops: Vec<OuterLoop>,
    /// Nesting level this run starts at (0 for the main run).
    base_depth: usize,
    /// `1`, `2.1`, ...; None for the main run.
    branch: Option<String>,
    /// Some for branches: what the Parallel block collects afterwards.
    group: Option<GroupOut>,
}

/// Runs a flow. `vars` holds the globals and environment (and the collection
/// scope for blocks that do not belong to a collection) and is updated with
/// the changes blocks made. Events go to `sink` in order.
pub async fn run_flow(
    env: &FlowEnv,
    flow: &Flow,
    vars: &mut VariableScopes,
    opts: FlowRunOptions,
    sink: &mut (dyn FnMut(FlowEvent) + Send),
    cancel: &CancellationToken,
) -> FlowResult {
    let started = Instant::now();
    sink(FlowEvent::Started { flow_id: flow.id.clone(), flow_name: flow.name.clone() });
    let max_steps = opts.max_steps.or(flow.settings.max_steps).unwrap_or(DEFAULT_MAX_STEPS).max(1);
    let (inputs, input_errors) = match resolve_inputs(flow, &opts.inputs) {
        Ok(i) => (i, Vec::new()),
        Err(errors) => (IndexMap::new(), errors),
    };
    let shared = Shared {
        env,
        flow,
        cancel,
        halt: cancel.child_token(),
        inputs,
        max_steps,
        started,
        sink: Mutex::new(sink),
        state: Mutex::new(State {
            result: FlowResult {
                flow_id: flow.id.clone(),
                flow_name: flow.name.clone(),
                started_at: chrono::Utc::now().to_rfc3339(),
                ..Default::default()
            },
            ..Default::default()
        }),
        inline_cache: Mutex::default(),
        refs_cache: Mutex::default(),
    };
    let mut run = Run {
        sh: &shared,
        vars: vars.clone(),
        flow_vars: opts.variables,
        records: HashMap::new(),
        collection_vars: env.collections.iter().map(|c| (c.id.clone(), variables::to_map(&c.variables))).collect(),
        collection_types: env.collections.iter().map(|c| (c.id.clone(), variables::to_types(&c.variables))).collect(),
        used_collections: Vec::new(),
        frames: Vec::new(),
        outer_loops: Vec::new(),
        base_depth: 0,
        branch: None,
        group: None,
    };
    if !input_errors.is_empty() {
        for e in input_errors {
            run.fatal(None, e);
        }
    } else if let Some(start) = flow.start() {
        run.frames.push(Frame {
            kind: FrameKind::Root,
            stack: vec![Activation::new(&start.id, PORT_IN, Arc::new(Value::Null), None)],
        });
        run.drive().await;
    } else {
        run.fatal(None, "the flow has no Start block".into());
    }
    *vars = std::mem::take(&mut run.vars);
    run.finish(started)
}

/// [`Run::drive`] behind a pointer: branches run it recursively (a branch
/// can hold a Parallel block of its own).
fn drive_boxed<'x>(run: &'x mut Run<'_, '_>) -> Pin<Box<dyn Future<Output = ()> + Send + 'x>> {
    Box::pin(run.drive())
}

/// Drives a member of a parallel group to its end and hands it back.
fn drive_owned<'r, 'a: 'r>(
    index: usize,
    mut run: Run<'r, 'a>,
) -> Pin<Box<dyn Future<Output = (usize, Run<'r, 'a>)> + Send + 'r>> {
    Box::pin(async move {
        drive_boxed(&mut run).await;
        (index, run)
    })
}

/// Applies the changes `new` made to `base` onto `target`.
fn merge_map<V: PartialEq + Clone>(
    target: &mut IndexMap<String, V>,
    base: &IndexMap<String, V>,
    new: &IndexMap<String, V>,
) {
    for k in base.keys() {
        if !new.contains_key(k) {
            target.shift_remove(k);
        }
    }
    for (k, v) in new {
        if base.get(k) != Some(v) {
            target.insert(k.clone(), v.clone());
        }
    }
}

type Typed<'x> = (&'x IndexMap<String, String>, &'x IndexMap<String, VarType>);

/// [`merge_map`] for a variable scope and its types: a value and its type
/// always come from the same branch.
fn merge_typed(
    (target, target_types): (&mut IndexMap<String, String>, &mut IndexMap<String, VarType>),
    (base, base_types): Typed<'_>,
    (new, new_types): Typed<'_>,
) {
    for k in base.keys() {
        if !new.contains_key(k) {
            target.shift_remove(k);
            target_types.shift_remove(k);
        }
    }
    for (k, v) in new {
        let ty = new_types.get(k);
        if base.get(k) != Some(v) || base_types.get(k) != ty {
            target.insert(k.clone(), v.clone());
            match ty {
                Some(t) => target_types.insert(k.clone(), *t),
                None => target_types.shift_remove(k),
            };
        }
    }
}

/// Sets a variable to `value`, recording its type the way a script's
/// `set(name, value)` does.
fn set_typed(
    values: &mut IndexMap<String, String>,
    types: &mut IndexMap<String, VarType>,
    name: String,
    value: &Value,
) {
    let (text, ty) = VarType::cast(None, value);
    if ty == VarType::Default {
        types.shift_remove(&name);
    } else {
        types.insert(name.clone(), ty);
    }
    values.insert(name, text);
}

/// Where each block's connections lead, without the edges into `break`
/// ports (a break ends the path).
type Adjacency<'a> = HashMap<&'a str, Vec<&'a str>>;

fn forward_edges(flow: &Flow) -> Adjacency<'_> {
    let mut adj: Adjacency = HashMap::new();
    for e in flow.edges.iter().filter(|e| e.to_port != PORT_BREAK) {
        adj.entry(e.from_block.as_str()).or_default().push(e.to_block.as_str());
    }
    adj
}

/// Blocks reachable from `from`, itself included.
fn reach<'a>(adj: &Adjacency<'a>, from: &'a str) -> HashSet<&'a str> {
    let mut seen = HashSet::from([from]);
    let mut queue = vec![from];
    while let Some(b) = queue.pop() {
        for &to in adj.get(b).map(Vec::as_slice).unwrap_or_default() {
            if seen.insert(to) {
                queue.push(to);
            }
        }
    }
    seen
}

/// One line of execution in a parallel group: a branch, or what runs after
/// an early Join (a continuation).
struct Member<'r, 'a> {
    /// For continuations: the Join and which firing of it (1, 2, ...).
    join: Option<(String, usize)>,
    /// Blocks it can reach from where it starts.
    reach: HashSet<&'a str>,
    /// The state it started from; None for branches (the group's start).
    base: Option<Snapshot>,
    state: MemberState<'r, 'a>,
    /// Join activations it collected that no early Join took yet:
    /// `(join, from block, payload)`.
    arrivals: Vec<(String, String, Arc<Value>)>,
}

enum MemberState<'r, 'a> {
    /// A branch waiting for a free slot (`max_concurrent`).
    Waiting,
    Running,
    Done(Box<Run<'r, 'a>>),
}

/// A Join only some branches of the group (the first members) can reach;
/// it fires without waiting for the others.
fn is_early(branches: &[Member<'_, '_>], join: &str) -> bool {
    !branches.iter().all(|m| m.reach.contains(join))
}

/// The topmost early Join that can fire now: it holds arrivals, and neither
/// a member that has not finished nor another early Join holding arrivals
/// can still lead to it.
fn ready_join<'a>(flow: &'a Flow, members: &[Member<'_, 'a>], count: usize, adj: &Adjacency<'a>) -> Option<&'a str> {
    let mut pending: Vec<&'a Block> = Vec::new();
    for (join, _, _) in members.iter().flat_map(|m| &m.arrivals) {
        if let Some(b) = flow.block(join)
            && !pending.iter().any(|p| p.id == b.id)
            && is_early(&members[..count], join)
        {
            pending.push(b);
        }
    }
    pending.sort_by(|a, b| {
        let (pa, pb) = (a.position, b.position);
        pa.1.total_cmp(&pb.1).then(pa.0.total_cmp(&pb.0)).then(a.id.cmp(&b.id))
    });
    let unfinished = |j: &str| members.iter().any(|m| !matches!(m.state, MemberState::Done(_)) && m.reach.contains(j));
    pending
        .iter()
        .map(|b| b.id.as_str())
        .find(|j| !unfinished(j) && !pending.iter().any(|o| o.id != *j && reach(adj, &o.id).contains(j)))
}

/// Members in merge order: branches in branch order, then continuations by
/// Join position (top to bottom, then left to right), Join id and firing.
fn merge_order(flow: &Flow, members: &[Member<'_, '_>]) -> Vec<usize> {
    let pos = |id: &str| flow.block(id).map(|b| b.position).unwrap_or((f32::MAX, f32::MAX));
    let mut order: Vec<usize> = (0..members.len()).collect();
    order.sort_by(|&a, &b| match (&members[a].join, &members[b].join) {
        (Some((ja, fa)), Some((jb, fb))) => {
            let (pa, pb) = (pos(ja), pos(jb));
            pa.1.total_cmp(&pb.1).then(pa.0.total_cmp(&pb.0)).then(ja.cmp(jb)).then(fa.cmp(fb))
        }
        (ja, jb) => ja.is_some().cmp(&jb.is_some()).then(a.cmp(&b)),
    });
    order
}

/// Output of a Join: `results` keyed by the label (or id) of the block each
/// arrival came from, `items` (the same values in branch order), `sources`
/// (their block ids), `count` and `branches` (branches in the group).
fn join_output(flow: &Flow, arrivals: &[(String, Arc<Value>)], branches: usize) -> Value {
    let mut results = serde_json::Map::new();
    let mut owners: HashMap<String, &str> = HashMap::new();
    let mut items = Vec::with_capacity(arrivals.len());
    let mut sources = Vec::with_capacity(arrivals.len());
    for (from, v) in arrivals {
        let label = flow.block(from).map(|b| b.label.trim()).filter(|l| !l.is_empty()).unwrap_or(from.as_str());
        // Two blocks with the same label: the second one goes by its id.
        let key = match owners.get(label) {
            Some(owner) if *owner != from.as_str() => from.as_str(),
            _ => label,
        };
        owners.insert(key.to_string(), from.as_str());
        results.insert(key.to_string(), (**v).clone());
        items.push((**v).clone());
        sources.push(Value::String(from.clone()));
    }
    json!({
        "results": results,
        "items": items,
        "sources": sources,
        "count": arrivals.len(),
        "branches": branches,
    })
}

impl<'r, 'a> Run<'r, 'a> {
    fn flow(&self) -> &'a Flow {
        self.sh.flow
    }

    fn env(&self) -> &'a FlowEnv {
        self.sh.env
    }

    fn emit(&self, e: FlowEvent) {
        self.sh.emit(e);
    }

    fn fatal(&self, block: Option<&str>, message: String) {
        self.sh.state().result.errors.push(message.clone());
        self.emit(FlowEvent::Error { block: block.map(str::to_string), message });
        self.sh.stop_run();
    }

    fn log(&self, block: Option<&str>, level: LogLevel, message: String) {
        self.emit(FlowEvent::Log { block: block.map(str::to_string), level, message });
    }

    async fn drive(&mut self) {
        loop {
            if self.group.is_some() {
                // Branches share one task: let the others take a step too,
                // so a busy branch does not hold back their requests.
                tokio::task::yield_now().await;
            }
            if self.sh.stopped() || self.sh.note_cancel() {
                break;
            }
            let Some(top) = self.frames.last_mut() else { break };
            if let Some(act) = top.stack.pop() {
                self.activate(act).await;
            } else if matches!(top.kind, FrameKind::Root) {
                break;
            } else {
                self.advance_loop();
            }
        }
    }

    fn depth(&self) -> usize {
        self.base_depth + self.frames.iter().filter(|f| matches!(f.kind, FrameKind::Loop(_))).count()
    }

    fn innermost_loop(&self) -> Option<LoopView<'_>> {
        self.frames
            .iter()
            .rev()
            .find_map(|f| match &f.kind {
                FrameKind::Loop(l) => Some(LoopView { index: l.index, count: l.items.len(), ctx: &l.ctx }),
                FrameKind::Root => None,
            })
            .or_else(|| self.outer_loops.last().map(|l| LoopView { index: l.index, count: l.count, ctx: &l.ctx }))
    }

    fn loop_active(&self, id: &str) -> bool {
        self.frames.iter().any(|f| matches!(&f.kind, FrameKind::Loop(l) if l.block == id))
            || self.outer_loops.iter().any(|l| l.block == id)
    }

    /// Counts a step and announces it; `None` when the run is stopping or
    /// the step limit is hit.
    fn begin_step(&mut self, block: &Block, depth: usize, iteration: Option<usize>) -> Option<usize> {
        let step = {
            let mut st = self.sh.state();
            if st.stop {
                return None;
            }
            (st.step < self.sh.max_steps).then(|| {
                st.step += 1;
                st.step
            })
        };
        let Some(step) = step else {
            let msg =
                format!("stopped after {} steps (max_steps); a loop or cycle probably never ends", self.sh.max_steps);
            self.fatal(Some(&block.id), msg);
            return None;
        };
        self.emit(FlowEvent::BlockStarted {
            step,
            block: block.id.clone(),
            label: block.display_name().to_string(),
            kind: block.kind(),
            depth,
            iteration,
            branch: self.branch.clone(),
        });
        Some(step)
    }

    /// Reports a finished block. Returns the port and output the run goes on
    /// with, or None when this path ends (a failure without a failure edge,
    /// or the run is stopping).
    fn finish_step(
        &mut self,
        block: &Block,
        step: usize,
        out: Outcome,
        duration_ms: f64,
        depth: usize,
    ) -> Option<(&'static str, Arc<Value>)> {
        let iteration = self.innermost_loop().map(|l| l.index);
        let failed = out.port == PORT_FAILURE;
        if let Some(execution) = out.execution {
            self.emit(FlowEvent::Execution { step, block: block.id.clone(), execution });
        }
        {
            let mut st = self.sh.state();
            for t in &out.tests {
                if t.skipped {
                    st.result.tests_skipped += 1;
                } else if t.passed {
                    st.result.tests_passed += 1;
                } else {
                    st.result.tests_failed += 1;
                }
            }
        }
        let output = Arc::new(out.output);
        let rec = self.records.entry(block.id.clone()).or_default();
        rec.output = output.clone();
        rec.port = out.port.to_string();
        rec.error = out.error.clone();
        rec.duration_ms = duration_ms;
        rec.runs += 1;
        self.emit(FlowEvent::BlockFinished {
            step,
            block: block.id.clone(),
            port: out.port.to_string(),
            output: output.clone(),
            duration_ms,
            failed,
            error: out.error.clone(),
            tests: out.tests.clone(),
        });
        self.sh.state().result.steps.push(StepRecord {
            step,
            block: block.id.clone(),
            label: block.display_name().to_string(),
            kind: block.kind(),
            port: out.port.to_string(),
            failed,
            error: out.error.clone(),
            started_ms: (self.sh.started.elapsed().as_secs_f64() * 1000.0 - duration_ms).max(0.0),
            duration_ms,
            depth,
            iteration,
            branch: self.branch.clone(),
            status: out.status,
            tests: out.tests,
        });
        if self.sh.halt.is_cancelled() {
            // Cancelled, or stopped by another branch: nothing goes on.
            self.sh.note_cancel();
            return None;
        }
        if failed {
            let message = out.error.unwrap_or_else(|| "failed".into());
            let handled =
                block.kind().has_failure_port() && self.flow().edges_from(&block.id, PORT_FAILURE).next().is_some();
            self.sh.state().result.failures.push(Failure {
                block: block.id.clone(),
                label: block.display_name().to_string(),
                message: message.clone(),
                handled,
            });
            if !handled {
                self.emit(FlowEvent::Error { block: Some(block.id.clone()), message });
                if !self.flow().settings.continue_on_failure {
                    self.sh.stop_run();
                }
                return None;
            }
        }
        Some((out.port, output))
    }

    fn finish_and_continue(&mut self, block: &Block, step: usize, out: Outcome, duration_ms: f64, depth: usize) {
        if let Some((port, output)) = self.finish_step(block, step, out, duration_ms, depth) {
            self.push_edges(&block.id, port, output);
        }
    }

    /// Edges leaving `block.port` in run order: top to bottom, then left to
    /// right, then file order.
    fn sorted_edges(&self, block: &str, port: &str) -> Vec<&'a Edge> {
        let flow = self.flow();
        let mut targets: Vec<usize> = flow.edges_from(block, port).map(|(i, _)| i).collect();
        let pos = |id: &str| flow.block(id).map(|b| b.position).unwrap_or((f32::MAX, f32::MAX));
        targets.sort_by(|ia, ib| {
            let (pa, pb) = (pos(&flow.edges[*ia].to_block), pos(&flow.edges[*ib].to_block));
            pa.1.total_cmp(&pb.1).then(pa.0.total_cmp(&pb.0)).then(ia.cmp(ib))
        });
        targets.into_iter().map(|i| &flow.edges[i]).collect()
    }

    fn push_edges(&mut self, block: &str, port: &str, payload: Arc<Value>) {
        let targets = self.sorted_edges(block, port);
        let Some(top) = self.frames.last_mut() else { return };
        for e in targets.iter().rev() {
            top.stack.push(Activation::new(&e.to_block, &e.to_port, payload.clone(), Some(block)));
        }
    }

    async fn activate(&mut self, act: Activation) {
        let flow = self.flow();
        let Some(block) = flow.block(&act.block) else {
            self.fatal(None, format!("connection to unknown block \"{}\"", act.block));
            return;
        };
        match &block.config {
            BlockConfig::Loop(cfg) => {
                if act.port == PORT_BREAK {
                    self.break_loop(block);
                } else if !self.loop_active(&block.id) {
                    self.start_loop(block, cfg, &act).await;
                }
                // An edge from the body back into its own loop just ends the
                // iteration path; the loop advances when the body is done.
                return;
            }
            BlockConfig::Join(_) if !act.join => {
                if let Some(group) = &mut self.group {
                    // The Join fires once the whole parallel group is done.
                    let from = act.from.unwrap_or_default();
                    group.arrivals.push((block.id.clone(), from, act.payload));
                    return;
                }
                // Outside of a parallel group a Join passes each arrival on.
            }
            _ => {}
        }
        let depth = self.depth();
        let iteration = self.innermost_loop().map(|l| l.index);
        let Some(step) = self.begin_step(block, depth, iteration) else { return };
        let started = Instant::now();
        let out = self.run_block(block, &act, step).await;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let Some((port, output)) = self.finish_step(block, step, out, ms, depth) else { return };
        if block.kind() == BlockKind::Parallel {
            self.run_group(block, output).await;
        } else {
            self.push_edges(&block.id, port, output);
        }
    }

    // -- Parallel branches ------------------------------------------------

    /// How many branches of `parallel` may run at the same time (0 = all).
    fn branch_limit(&self, _parallel: &Block) -> usize {
        self.flow().settings.max_concurrent
    }

    /// A run starting at `act` from a copy of this run's variables and block
    /// records, as a member of a parallel group.
    fn fork(&self, act: Activation, outer: &[OuterLoop], base_depth: usize, branch: String) -> Run<'r, 'a> {
        Run {
            sh: self.sh,
            vars: self.vars.clone(),
            flow_vars: self.flow_vars.clone(),
            records: self.records.clone(),
            collection_vars: self.collection_vars.clone(),
            collection_types: self.collection_types.clone(),
            used_collections: self.used_collections.clone(),
            frames: vec![Frame { kind: FrameKind::Root, stack: vec![act] }],
            outer_loops: outer.to_vec(),
            base_depth,
            branch: Some(branch),
            group: Some(GroupOut::default()),
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            globals: self.vars.globals.clone(),
            environment: self.vars.environment.clone(),
            collection: self.vars.collection.clone(),
            types: self.vars.types.clone(),
            flow_vars: self.flow_vars.clone(),
            collection_vars: self.collection_vars.clone(),
            collection_types: self.collection_types.clone(),
            runs: self.records.iter().map(|(k, r)| (k.clone(), r.runs)).collect(),
        }
    }

    /// Runs the branches of a Parallel block at the same time and waits for
    /// all of them (and for what runs after their early Joins).
    ///
    /// Each branch starts at one target of `branches`, with copies of this
    /// run's variables (flow, environment, globals, collection) and block
    /// records; branches do not see each other's changes. At most
    /// [`FlowSettings::max_concurrent`] branches run at once; the others start
    /// in branch order as running ones finish. A branch ends when it has
    /// nothing left to do: a Join activation is collected instead of run, a
    /// failure without a failure edge ends the branch (and the whole run
    /// unless `continue_on_failure`), a `break` of a loop around the group
    /// takes effect afterwards.
    ///
    /// Joins are told apart by the connections, not by what happens at run
    /// time, so the outcome never depends on timing:
    ///
    /// * A Join every branch can reach (from where it starts, along any
    ///   connection except into a `break` port) waits for the whole group.
    ///   It fires once, here, after the merge below, top to bottom, with the
    ///   arrivals of all members ([`join_output`]).
    /// * A Join only some branches can reach fires *early*: as soon as every
    ///   member that can reach it has finished (and no other early Join
    ///   holding arrivals can still lead to it), while the other branches go
    ///   on. What follows it runs as one more member of the group (a
    ///   *continuation*, labelled with the branches it joins: `1+2`), from
    ///   the group's starting state plus the changes of the members that can
    ///   reach the Join, applied in member order. A Join reached again after
    ///   it fired collects the new arrivals and fires again when ready.
    ///
    /// When everything is done the changes of every member are applied in
    /// member order: branches in branch order (top to bottom, then left to
    /// right, like a fan-out), then continuations by Join position. So when
    /// two branches set the same variable the later branch wins, and a
    /// continuation wins over the branches; block records add up their run
    /// counts and keep the later member's output. A `break` takes effect
    /// after that and does not undo Joins that already fired early. Once the
    /// run is stopping no branch starts and no early Join fires.
    async fn run_group(&mut self, parallel: &Block, payload: Arc<Value>) {
        let edges = self.sorted_edges(&parallel.id, PORT_BRANCHES);
        if edges.is_empty() {
            return;
        }
        let flow = self.flow();
        let mut outer = self.outer_loops.clone();
        for f in &self.frames {
            if let FrameKind::Loop(l) = &f.kind {
                outer.push(OuterLoop {
                    block: l.block.clone(),
                    index: l.index,
                    count: l.items.len(),
                    ctx: l.ctx.clone(),
                });
            }
        }
        let base = self.snapshot();
        let base_depth = self.depth() + 1;
        let label = |i: usize| match &self.branch {
            Some(b) => format!("{b}.{}", i + 1),
            None => (i + 1).to_string(),
        };
        let adj = forward_edges(flow);
        let count = edges.len();
        let mut members: Vec<Member<'r, 'a>> = edges
            .iter()
            .map(|e| Member {
                join: None,
                reach: reach(&adj, &e.to_block),
                base: None,
                state: MemberState::Waiting,
                arrivals: Vec::new(),
            })
            .collect();
        let limit = match self.branch_limit(parallel) {
            0 => count,
            n => n,
        };
        let mut running = FuturesUnordered::new();
        let (mut next, mut active) = (0, 0);
        let mut fired: HashMap<String, usize> = HashMap::new();
        loop {
            while next < count && active < limit && !self.sh.stopped() {
                let e = edges[next];
                let act = Activation::new(&e.to_block, &e.to_port, payload.clone(), Some(&parallel.id));
                running.push(drive_owned(next, self.fork(act, &outer, base_depth, label(next))));
                members[next].state = MemberState::Running;
                next += 1;
                active += 1;
            }
            let Some((i, mut run)) = running.next().await else { break };
            if i < count {
                active -= 1;
            }
            members[i].arrivals = run.group.as_mut().map(|g| std::mem::take(&mut g.arrivals)).unwrap_or_default();
            members[i].state = MemberState::Done(Box::new(run));
            while !self.sh.stopped() {
                let Some(join) = ready_join(flow, &members, count, &adj) else { break };
                let firing = fired.entry(join.to_string()).or_default();
                *firing += 1;
                let (run, start) = self.continuation(&mut members, count, &base, join, &outer, &label);
                members.push(Member {
                    join: Some((join.to_string(), *firing)),
                    reach: reach(&adj, join),
                    base: Some(start),
                    state: MemberState::Running,
                    arrivals: Vec::new(),
                });
                running.push(drive_owned(members.len() - 1, run));
            }
        }

        let mut joins: Vec<(String, Arrivals)> = Vec::new();
        let mut breaks: Vec<String> = Vec::new();
        for i in merge_order(flow, &members) {
            let arrivals = std::mem::take(&mut members[i].arrivals);
            let m = &members[i];
            let MemberState::Done(run) = &m.state else { continue };
            self.absorb(m.base.as_ref().unwrap_or(&base), run);
            for id in run.group.as_ref().map(|g| g.breaks.as_slice()).unwrap_or_default() {
                if !breaks.contains(id) {
                    breaks.push(id.clone());
                }
            }
            // Early Joins left with arrivals (the run is stopping) do not fire.
            for (join, from, value) in arrivals {
                if is_early(&members[..count], &join) {
                    continue;
                }
                match joins.iter_mut().find(|(j, _)| *j == join) {
                    Some((_, list)) => list.push((from, value)),
                    None => joins.push((join, vec![(from, value)])),
                }
            }
        }
        let pos = |id: &str| flow.block(id).map(|b| b.position).unwrap_or((f32::MAX, f32::MAX));
        joins.sort_by(|(a, _), (b, _)| {
            let (pa, pb) = (pos(a), pos(b));
            pa.1.total_cmp(&pb.1).then(pa.0.total_cmp(&pb.0))
        });
        if let Some(top) = self.frames.last_mut() {
            for (join, arrivals) in joins.iter().rev() {
                top.stack.push(Activation {
                    block: join.clone(),
                    port: PORT_IN.into(),
                    payload: Arc::new(join_output(flow, arrivals, count)),
                    from: None,
                    join: true,
                });
            }
        }
        // A break drops what this iteration still had to do, Joins included.
        for id in breaks {
            if let Some(b) = flow.block(&id) {
                self.break_loop(b);
            }
        }
    }

    /// The run that goes on after the early Join `join` fires: the group's
    /// starting state plus the changes of the finished members that can reach
    /// the Join (in member order), starting at the Join with the arrivals it
    /// has not fired with yet. Also returns the state it starts from.
    fn continuation(
        &self,
        members: &mut [Member<'r, 'a>],
        count: usize,
        base: &Snapshot,
        join: &str,
        outer: &[OuterLoop],
        label: &dyn Fn(usize) -> String,
    ) -> (Run<'r, 'a>, Snapshot) {
        let flow = self.flow();
        let order = merge_order(flow, members);
        let placeholder = Activation::new(join, PORT_IN, Arc::new(Value::Null), None);
        let feeders: Vec<String> = (0..count).filter(|i| members[*i].reach.contains(join)).map(label).collect();
        let mut run = self.fork(placeholder, outer, self.depth(), feeders.join("+"));
        let mut arrivals: Arrivals = Vec::new();
        for i in order {
            let m = &mut members[i];
            let MemberState::Done(done) = &m.state else { continue };
            if m.reach.contains(join) {
                run.absorb(m.base.as_ref().unwrap_or(base), done);
            }
            let (here, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut m.arrivals).into_iter().partition(|a| a.0 == join);
            m.arrivals = rest;
            arrivals.extend(here.into_iter().map(|(_, from, value)| (from, value)));
        }
        let start = run.snapshot();
        run.frames = vec![Frame {
            kind: FrameKind::Root,
            stack: vec![Activation {
                block: join.to_string(),
                port: PORT_IN.into(),
                payload: Arc::new(join_output(flow, &arrivals, count)),
                from: None,
                join: true,
            }],
        }];
        (run, start)
    }

    /// Applies what `b` changed (relative to `base`, where it started) to
    /// this run.
    fn absorb(&mut self, base: &Snapshot, b: &Run<'r, 'a>) {
        let (types, new) = (&mut self.vars.types, &b.vars.types);
        let globals = (&base.globals, &base.types.globals);
        merge_typed((&mut self.vars.globals, &mut types.globals), globals, (&b.vars.globals, &new.globals));
        let env = (&base.environment, &base.types.environment);
        merge_typed((&mut self.vars.environment, &mut types.environment), env, (&b.vars.environment, &new.environment));
        let coll = (&base.collection, &base.types.collection);
        merge_typed((&mut self.vars.collection, &mut types.collection), coll, (&b.vars.collection, &new.collection));
        merge_map(&mut self.flow_vars, &base.flow_vars, &b.flow_vars);
        let (empty, no_types) = (IndexMap::new(), IndexMap::new());
        for (id, vars) in &b.collection_vars {
            let before = base.collection_vars.get(id).unwrap_or(&empty);
            let before_types = base.collection_types.get(id).unwrap_or(&no_types);
            let new_types = b.collection_types.get(id).unwrap_or(&no_types);
            let target = (
                self.collection_vars.entry(id.clone()).or_default(),
                self.collection_types.entry(id.clone()).or_default(),
            );
            merge_typed(target, (before, before_types), (vars, new_types));
        }
        for id in &b.used_collections {
            if !self.used_collections.contains(id) {
                self.used_collections.push(id.clone());
            }
        }
        for (id, rec) in &b.records {
            let before = base.runs.get(id).copied().unwrap_or(0);
            if rec.runs > before {
                let runs = self.records.get(id).map(|r| r.runs).unwrap_or(0) + rec.runs - before;
                self.records.insert(id.clone(), BlockRecord { runs, ..rec.clone() });
            }
        }
    }

    // -- Loops ------------------------------------------------------------

    fn loop_ctx(items: &[Value], index: usize) -> Value {
        json!({
            "item": items[index],
            "index": index,
            "count": items.len(),
            "first": index == 0,
            "last": index + 1 == items.len(),
        })
    }

    async fn start_loop(&mut self, block: &Block, cfg: &LoopConfig, act: &Activation) {
        let depth = self.depth();
        let iteration = self.innermost_loop().map(|l| l.index);
        let Some(step) = self.begin_step(block, depth, iteration) else { return };
        let started = Instant::now();
        let max = cfg.max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);
        let items = self.loop_items(block, cfg, act, max).await;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let items = match items {
            Ok(items) if items.len() > max => Err(format!(
                "{} iterations exceed the limit of {max} (raise max_iterations to allow more)",
                items.len()
            )),
            other => other,
        };
        match items {
            Err(e) => self.finish_and_continue(block, step, Outcome::fail(e), ms, depth),
            Ok(items) if items.is_empty() => {
                let out = json!({"count": 0, "iterations": 0, "broken": false});
                self.finish_and_continue(block, step, Outcome::ok(PORT_DONE, out), ms, depth);
            }
            Ok(items) => {
                let ctx = Self::loop_ctx(&items, 0);
                self.frames.push(Frame {
                    kind: FrameKind::Loop(LoopState {
                        block: block.id.clone(),
                        items,
                        index: 0,
                        broken: false,
                        ctx: ctx.clone(),
                    }),
                    stack: Vec::new(),
                });
                self.finish_and_continue(block, step, Outcome::ok(PORT_ITEM, ctx), ms, depth);
            }
        }
    }

    /// Called when the top loop frame has no pending work.
    fn advance_loop(&mut self) {
        let flow = self.flow();
        let Some(Frame { kind: FrameKind::Loop(l), .. }) = self.frames.last_mut() else { return };
        let Some(block) = flow.block(&l.block) else {
            self.frames.pop();
            return;
        };
        if !l.broken && l.index + 1 < l.items.len() {
            l.index += 1;
            l.ctx = Self::loop_ctx(&l.items, l.index);
            let (ctx, index) = (l.ctx.clone(), l.index);
            let depth = self.depth() - 1;
            let Some(step) = self.begin_step(block, depth, Some(index)) else { return };
            self.finish_and_continue(block, step, Outcome::ok(PORT_ITEM, ctx), 0.0, depth);
        } else {
            let out = json!({
                "count": l.items.len(),
                "iterations": l.index + 1,
                "broken": l.broken,
            });
            self.frames.pop();
            let depth = self.depth();
            let iteration = self.innermost_loop().map(|l| l.index);
            let Some(step) = self.begin_step(block, depth, iteration) else { return };
            self.finish_and_continue(block, step, Outcome::ok(PORT_DONE, out), 0.0, depth);
        }
    }

    fn break_loop(&mut self, block: &Block) {
        let pos = self.frames.iter().rposition(|f| matches!(&f.kind, FrameKind::Loop(l) if l.block == block.id));
        match pos {
            Some(p) => {
                self.frames.truncate(p + 1);
                if let Some(f) = self.frames.last_mut() {
                    f.stack.clear();
                    if let FrameKind::Loop(l) = &mut f.kind {
                        l.broken = true;
                    }
                }
            }
            // The loop runs around this branch's parallel group: it stops
            // once all branches are done.
            None if self.outer_loops.iter().any(|l| l.block == block.id) => {
                if let Some(g) = &mut self.group {
                    g.breaks.push(block.id.clone());
                }
            }
            None => self.log(
                Some(&block.id),
                LogLevel::Warn,
                format!("break ignored: loop \"{}\" is not running", block.display_name()),
            ),
        }
    }

    async fn loop_items(
        &mut self,
        block: &Block,
        cfg: &LoopConfig,
        act: &Activation,
        max: usize,
    ) -> Result<Vec<Value>, String> {
        let value = match cfg.mode {
            LoopMode::Count => {
                let v = self.render_value(&cfg.count, &act.payload, None);
                let n = to_number(&v).ok_or_else(|| format!("count \"{}\" is not a number", value_to_string(&v)))?;
                if n < 0.0 {
                    return Err(format!("count {n} is negative"));
                }
                let n = n.floor() as usize;
                if n > max {
                    return Err(format!(
                        "{n} iterations exceed the limit of {max} (raise max_iterations to allow more)"
                    ));
                }
                return Ok((0..n).map(Value::from).collect());
            }
            LoopMode::ForEach if cfg.items.contains("{{") => match self.render_value(&cfg.items, &act.payload, None) {
                Value::String(s) => serde_json::from_str::<Value>(&s)
                    .map_err(|_| format!("items is not a JSON array: {}", truncate(&s, 80)))?,
                v => v,
            },
            LoopMode::ForEach => {
                let r = self.run_js(block, act, &js::wrap_expression(&cfg.items)).await;
                r.value?
            }
        };
        match value {
            Value::Array(a) => Ok(a),
            Value::Object(m) => Ok(m.into_iter().map(|(_, v)| v).collect()),
            Value::Null => Err("items resolved to nothing; check the reference".into()),
            other => Err(format!("items must be an array, got {}", type_name(&other))),
        }
    }

    // -- Scopes and templates --------------------------------------------

    /// Scopes for a block: run globals/environment, the collection scope of
    /// `collection` (or the run's own), flow inputs and variables as data.
    fn scopes(&self, collection: Option<&str>) -> VariableScopes {
        let mut data = self.sh.inputs.clone();
        for (k, v) in &self.flow_vars {
            data.insert(k.clone(), v.clone());
        }
        let mut scopes = VariableScopes {
            globals: self.vars.globals.clone(),
            collection: match collection {
                Some(id) => self.collection_vars.get(id).cloned().unwrap_or_default(),
                None => self.vars.collection.clone(),
            },
            environment: self.vars.environment.clone(),
            environment_name: self.vars.environment_name.clone(),
            data,
            local: IndexMap::new(),
            types: self.vars.types.clone(),
        };
        if let Some(id) = collection {
            scopes.types.collection = self.collection_types.get(id).cloned().unwrap_or_default();
        }
        scopes.types.local.clear();
        scopes
    }

    fn ref_ctx<'s>(&'s self, payload: &'s Value) -> RefCtx<'s> {
        RefCtx {
            flow: self.sh.flow,
            records: &self.records,
            inputs: &self.sh.inputs,
            vars: &self.flow_vars,
            loop_ctx: self.innermost_loop().map(|l| l.ctx),
            input: payload,
        }
    }

    fn render_value(&self, template: &str, payload: &Value, collection: Option<&str>) -> Value {
        let scopes = self.scopes(collection);
        let refs = self.ref_ctx(payload);
        let r = Renderer { refs: &refs, scopes: &scopes, vault: self.env().services.vault.as_deref() };
        r.render_value(template)
    }

    fn render(&self, template: &str, payload: &Value) -> String {
        let scopes = self.scopes(None);
        let refs = self.ref_ctx(payload);
        let r = Renderer { refs: &refs, scopes: &scopes, vault: self.env().services.vault.as_deref() };
        r.render(template)
    }

    /// Copies variable changes made by scripts back into the run. Local
    /// variables become flow variables.
    fn absorb_scopes(&mut self, scopes: VariableScopes, collection: Option<&str>) {
        let VariableScopes { globals, environment, collection: values, local, types, .. } = scopes;
        self.vars.globals = globals;
        self.vars.environment = environment;
        self.vars.types.globals = types.globals;
        self.vars.types.environment = types.environment;
        match collection {
            Some(id) => {
                self.collection_vars.insert(id.to_string(), values);
                self.collection_types.insert(id.to_string(), types.collection);
                if !self.used_collections.iter().any(|c| c == id) {
                    self.used_collections.push(id.to_string());
                }
            }
            None => {
                self.vars.collection = values;
                self.vars.types.collection = types.collection;
            }
        }
        // Keeping their type: a number set with `pm.variables.set` stays one.
        for (k, v) in local {
            if !k.starts_with(js::INTERNAL_PREFIX) {
                let ty = types.local.get(&k).copied().unwrap_or_default();
                self.flow_vars.insert(k, ty.to_json(&v));
            }
        }
    }

    fn position(&self) -> (usize, usize) {
        self.innermost_loop().map(|l| (l.index, l.count)).unwrap_or((0, 1))
    }

    // -- Blocks -----------------------------------------------------------

    async fn run_block(&mut self, block: &Block, act: &Activation, step: usize) -> Outcome {
        match &block.config {
            BlockConfig::Start(_) => {
                Outcome::ok(PORT_NEXT, Value::Object(self.sh.inputs.clone().into_iter().collect()))
            }
            BlockConfig::Request(cfg) => self.exec_request(block, cfg, act).await,
            BlockConfig::Condition(cfg) => self.exec_condition(block, cfg, act).await,
            BlockConfig::Loop(_) => Outcome::fail("loop handled elsewhere"),
            BlockConfig::Parallel(_) => Outcome::ok(PORT_BRANCHES, (*act.payload).clone()),
            BlockConfig::Join(_) if act.join => Outcome::ok(PORT_NEXT, (*act.payload).clone()),
            BlockConfig::Join(_) => {
                let from = act.from.clone().unwrap_or_default();
                Outcome::ok(PORT_NEXT, join_output(self.flow(), &[(from, act.payload.clone())], 1))
            }
            BlockConfig::Script(cfg) => {
                let r = self.run_js(block, act, &js::wrap(&cfg.code)).await;
                let tests_msg = tests_summary(&r.tests);
                let mut out = match (r.value, tests_msg) {
                    (Ok(v), None) => Outcome::ok(PORT_SUCCESS, v),
                    (Ok(_), Some(m)) | (Err(m), _) => Outcome::fail(m),
                };
                out.tests = r.tests;
                out
            }
            BlockConfig::Ai(cfg) => self.exec_ai(block, cfg, act, step).await,
            BlockConfig::Delay(cfg) => {
                let v = self.render_value(&cfg.ms, &act.payload, None);
                let Some(ms) = to_number(&v).filter(|n| *n >= 0.0) else {
                    return Outcome::fail(format!("delay \"{}\" is not a non-negative number", value_to_string(&v)));
                };
                let Ok(duration) = Duration::try_from_secs_f64(ms / 1000.0) else {
                    return Outcome::fail(format!("delay {ms} ms is too long"));
                };
                tokio::select! {
                    _ = tokio::time::sleep(duration) => Outcome::ok(PORT_NEXT, json!({"ms": ms})),
                    _ = self.sh.halt.cancelled() => {
                        self.sh.note_cancel();
                        Outcome::fail(self.sh.interrupted())
                    }
                }
            }
            BlockConfig::SetVariable(cfg) => {
                let name = cfg.name.trim().to_string();
                if name.is_empty() {
                    return Outcome::fail("the variable name is empty");
                }
                let value = self.render_value(&cfg.value, &act.payload, None);
                match cfg.scope {
                    VariableScope::Flow => {
                        self.flow_vars.insert(name, value.clone());
                    }
                    // Typed like `pm.environment.set(name, value)`.
                    VariableScope::Environment => {
                        set_typed(&mut self.vars.environment, &mut self.vars.types.environment, name, &value);
                    }
                    VariableScope::Global => {
                        set_typed(&mut self.vars.globals, &mut self.vars.types.globals, name, &value);
                    }
                }
                Outcome::ok(PORT_NEXT, value)
            }
            BlockConfig::Template(cfg) => match cfg.mode {
                TemplateMode::Template => {
                    let v = self.render_value(&cfg.template, &act.payload, None);
                    match (cfg.parse_json, v) {
                        (true, Value::String(s)) => match serde_json::from_str::<Value>(&s) {
                            Ok(j) => Outcome::ok(PORT_SUCCESS, j),
                            Err(e) => Outcome::fail(format!("the template is not valid JSON: {e}")),
                        },
                        (_, v) => Outcome::ok(PORT_SUCCESS, v),
                    }
                }
                TemplateMode::Expression => {
                    let r = self.run_js(block, act, &js::wrap_expression(&cfg.expression)).await;
                    match r.value {
                        Ok(v) => Outcome::ok(PORT_SUCCESS, v),
                        Err(e) => Outcome::fail(e),
                    }
                }
            },
            BlockConfig::Output(cfg) => {
                let value = self.render_value(&cfg.value, &act.payload, None);
                let name = cfg.name.trim();
                let name = if name.is_empty() { block.display_name().to_string() } else { name.to_string() };
                let failed = {
                    let mut st = self.sh.state();
                    st.result.outputs.insert(name.clone(), value.clone());
                    if cfg.fail {
                        st.explicit_fail = true;
                        let msg = format!("output \"{name}\" marks the run as failed");
                        st.result.errors.push(msg.clone());
                        Some(msg)
                    } else {
                        None
                    }
                };
                if let Some(message) = failed {
                    self.emit(FlowEvent::Error { block: Some(block.id.clone()), message });
                }
                Outcome::ok(PORT_NEXT, value)
            }
            BlockConfig::Log(cfg) => {
                let message = self.render(&cfg.message, &act.payload);
                let level = match cfg.level {
                    LogLevelConfig::Info => LogLevel::Info,
                    LogLevelConfig::Log => LogLevel::Log,
                    LogLevelConfig::Warn => LogLevel::Warn,
                    LogLevelConfig::Error => LogLevel::Error,
                    LogLevelConfig::Debug => LogLevel::Debug,
                };
                self.log(Some(&block.id), level, message.clone());
                Outcome::ok(PORT_NEXT, Value::String(message))
            }
        }
    }

    async fn exec_condition(&mut self, block: &Block, cfg: &ConditionConfig, act: &Activation) -> Outcome {
        let result = match cfg.mode {
            ConditionMode::Compare => {
                let left = self.render_value(&cfg.left, &act.payload, None);
                let right =
                    if cfg.op.is_unary() { Value::Null } else { self.render_value(&cfg.right, &act.payload, None) };
                compare(cfg.op, &left, &right)
            }
            ConditionMode::Expression => {
                let r = self.run_js(block, act, &js::wrap_expression(&cfg.expression)).await;
                r.value.map(|v| truthy(&v))
            }
        };
        match result {
            Ok(b) => Outcome::ok(if b { PORT_TRUE } else { PORT_FALSE }, Value::Bool(b)),
            Err(e) => Outcome::fail(e),
        }
    }

    /// Runs wrapped JavaScript (see [`js::wrap`]) for a block.
    async fn run_js(&mut self, block: &Block, act: &Activation, code: &str) -> JsOutcome {
        let flow = self.flow();
        let mut labels = serde_json::Map::new();
        for b in &flow.blocks {
            if !b.label.is_empty() && self.records.contains_key(&b.id) && !labels.contains_key(&b.label) {
                labels.insert(b.label.clone(), Value::String(b.id.clone()));
            }
        }
        let ctx = {
            let refs = self.ref_ctx(&act.payload);
            json!({
                "blocks": refs.blocks_value(),
                "labels": labels,
                "inputs": Value::Object(self.sh.inputs.clone().into_iter().collect()),
                "vars": Value::Object(self.flow_vars.clone().into_iter().collect()),
                "loop": refs.loop_ctx.cloned().unwrap_or(Value::Null),
                "input": *act.payload,
                "flow": {"id": flow.id, "name": flow.name},
            })
        };
        let mut scopes = self.scopes(None);
        scopes.data.insert(js::CONTEXT_KEY.into(), ctx);
        let source = ScriptSource { location: format!("block \"{}\"", block.display_name()), code: code.to_string() };
        let mut job = ScriptJob::new(ScriptEvent::PreRequest, vec![source], ScriptRequest::default());
        let services = &self.env().services;
        job.packages = services.packages.clone();
        let (iteration, iteration_count) = self.position();
        job.info = ScriptInfo {
            request_name: block.display_name().to_string(),
            request_id: block.id.clone(),
            iteration,
            iteration_count,
            collection_name: flow.name.clone(),
        };
        job.variables = scopes;
        job.http = Some(Arc::new(merpati_runner::script_http::PipelineHttp::new(services.clone())));
        let outcome = js::run_job(services, job, &self.sh.halt).await;
        self.sh.note_cancel();
        for c in &outcome.console {
            self.log(Some(&block.id), c.level, c.message.clone());
        }
        let mut variables = outcome.variables;
        let result = variables.local.shift_remove(js::RESULT_KEY);
        if let Some(Ok(Value::Object(m))) =
            variables.local.shift_remove(js::VARS_KEY).map(|s| serde_json::from_str::<Value>(&s))
        {
            self.flow_vars = m.into_iter().collect();
        }
        self.absorb_scopes(variables, None);
        let value = if self.sh.halt.is_cancelled() && !outcome.errors.is_empty() {
            Err(self.sh.interrupted())
        } else if let Some(e) = outcome.errors.first() {
            Err(if e.message.is_empty() { "script error".to_string() } else { e.message.clone() })
        } else {
            Ok(result.and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null))
        };
        JsOutcome { value, tests: outcome.tests }
    }

    /// The collection holding a saved item.
    fn find_saved(&self, r: &ItemRef) -> Option<(&'a Collection, &'a RequestItem)> {
        let env = self.env();
        if !r.collection_id.is_empty() {
            let c = env.collections.iter().find(|c| c.id == r.collection_id)?;
            return c.find_request(&r.request_id).map(|i| (c, i));
        }
        env.collections.iter().find_map(|c| c.find_request(&r.request_id).map(|i| (c, i)))
    }

    /// A collection holding just the inline request (or the context
    /// collection with the request added at its root), cached per block.
    fn inline_collection(
        &self,
        block: &Block,
        request: &HttpRequest,
        collection_id: &str,
        scripts: &Scripts,
    ) -> Result<Arc<(Collection, String)>, String> {
        if let Some(c) = lock(&self.sh.inline_cache).get(&block.id) {
            return Ok(c.clone());
        }
        let mut item = RequestItem::new(block.display_name(), request.clone());
        item.id = format!("flow-block-{}", block.id);
        item.scripts = scripts.clone();
        let id = item.id.clone();
        let flow = self.flow();
        let collection = if collection_id.is_empty() {
            let mut c = Collection::new(flow.name.clone());
            c.id = format!("flow-{}", flow.id);
            c.items.push(Item::Request(item));
            c
        } else {
            let mut c = self
                .env()
                .collections
                .iter()
                .find(|c| c.id == collection_id)
                .cloned()
                .ok_or_else(|| format!("collection {collection_id} not found"))?;
            c.items.push(Item::Request(item));
            c
        };
        let arc = Arc::new((collection, id));
        lock(&self.sh.inline_cache).insert(block.id.clone(), arc.clone());
        Ok(arc)
    }

    /// Flow references used by a request item, its folders and collection.
    fn request_refs(&self, block: &Block, collection: &Collection, request_id: &str) -> Vec<String> {
        if let Some(r) = lock(&self.sh.refs_cache).get(&block.id) {
            return r.clone();
        }
        let mut names = Vec::new();
        if let Some(item) = collection.find_request(request_id) {
            collect_refs(&serde_json::to_value(item).unwrap_or_default(), &mut names);
        }
        for a in collection.ancestors(request_id).unwrap_or_default() {
            collect_refs(&serde_json::to_value(a.auth()).unwrap_or_default(), &mut names);
            collect_refs(&serde_json::to_value(a.scripts()).unwrap_or_default(), &mut names);
            collect_refs(&serde_json::to_value(a.variables()).unwrap_or_default(), &mut names);
        }
        lock(&self.sh.refs_cache).insert(block.id.clone(), names.clone());
        names
    }

    async fn exec_request(&mut self, block: &Block, cfg: &RequestConfig, act: &Activation) -> Outcome {
        let env = self.env();
        let ranges = match parse_status_spec(&cfg.success_status) {
            Ok(r) => r,
            Err(e) => return Outcome::fail(e),
        };
        let holder: Arc<(Collection, String)>;
        let (collection, request_id, scope_id): (&Collection, String, Option<String>) = match &cfg.source {
            RequestSource::Saved(r) => match self.find_saved(r) {
                Some((c, item)) => {
                    if !matches!(item.request, RequestKind::Http(_)) {
                        return Outcome::fail(format!(
                            "\"{}\" is a {} request; Request blocks run HTTP requests",
                            item.name,
                            item.request.protocol_label()
                        ));
                    }
                    (c, item.id.clone(), Some(c.id.clone()))
                }
                None => return Outcome::fail(format!("saved request {} not found", r.request_id)),
            },
            RequestSource::Inline { request, collection_id, scripts } => {
                match self.inline_collection(block, request, collection_id, scripts) {
                    Ok(h) => holder = h,
                    Err(e) => return Outcome::fail(e),
                }
                let scope = (!collection_id.is_empty()).then(|| collection_id.clone());
                (&holder.0, holder.1.clone(), scope)
            }
        };
        let names = self.request_refs(block, collection, &request_id);
        let mut scopes = self.scopes(scope_id.as_deref());
        {
            let refs = self.ref_ctx(&act.payload);
            for n in &names {
                let bare = !crate::template::is_explicit_flow_ref(n);
                if bare && (scopes.get(n).is_some() || flow_root(n).is_none()) {
                    continue;
                }
                if let Some(v) = refs.eval(n) {
                    scopes.data.insert(n.clone(), v);
                }
            }
        }
        let (iteration, iteration_count) = self.position();
        let input = ExecuteInput {
            collection,
            request_id: &request_id,
            request_override: None,
            iteration,
            iteration_count,
            base_dir: env.base_dir.as_deref(),
        };
        let exec = merpati_runner::execute(&env.services, input, &mut scopes, &self.sh.halt).await;
        for k in names.iter() {
            scopes.data.shift_remove(k);
        }
        self.absorb_scopes(scopes, scope_id.as_deref());
        self.sh.state().result.requests += 1;
        for line in &exec.console {
            self.log(Some(&block.id), log_level(line.kind), line.message.clone());
        }
        let interrupted = self.sh.halt.is_cancelled()
            && matches!(&exec.error, Some(e) if e.kind == merpati_http::HttpErrorKind::Cancelled);
        self.sh.note_cancel();
        let status = exec.response.as_ref().map(|r| r.status);
        let status_good = match status {
            Some(s) => status_ok(s, &ranges),
            None => exec.skipped && exec.error.is_none(),
        };
        let error = if interrupted {
            Some(self.sh.interrupted())
        } else if let Some(e) = &exec.error {
            Some(e.message.clone())
        } else if !status_good {
            let r = exec.response.as_ref();
            Some(format!(
                "HTTP {} {}{}",
                status.map(|s| s.to_string()).unwrap_or_default(),
                r.map(|r| r.status_text.as_str()).unwrap_or(""),
                if cfg.success_status.trim().is_empty() {
                    String::new()
                } else {
                    format!(" (expected {})", cfg.success_status.trim())
                }
            ))
        } else if let Some(m) = tests_summary(&exec.tests) {
            Some(m)
        } else {
            exec.script_errors.first().map(|e| format!("{}: {}", e.location, e.message))
        };
        let ok = error.is_none();
        let mut output = request_output(&exec, ok);
        output["error"] = error.clone().map(Value::String).unwrap_or(Value::Null);
        Outcome {
            port: if ok { PORT_SUCCESS } else { PORT_FAILURE },
            output,
            error,
            tests: exec.tests.clone(),
            status,
            execution: Some(Box::new(exec)),
        }
    }

    async fn exec_ai(&mut self, block: &Block, cfg: &AiBlockConfig, act: &Activation, step: usize) -> Outcome {
        let env = self.env();
        let (mut req, scope_id): (AiRequest, Option<String>) = match &cfg.request {
            Some(r) => match self.find_saved(r) {
                Some((c, item)) => match &item.request {
                    RequestKind::Ai(a) => (a.clone(), Some(c.id.clone())),
                    _ => return Outcome::fail(format!("\"{}\" is not an AI request", item.name)),
                },
                None => return Outcome::fail(format!("saved AI request {} not found", r.request_id)),
            },
            None => (AiRequest { provider: cfg.provider.unwrap_or_default(), ..Default::default() }, None),
        };
        if let Some(p) = cfg.provider {
            req.provider = p;
        }
        if !cfg.base_url.trim().is_empty() {
            req.base_url = cfg.base_url.clone();
        }
        if !cfg.model.trim().is_empty() {
            req.model = cfg.model.clone();
        }
        if !cfg.api_key.trim().is_empty() {
            req.auth = Auth::bearer(cfg.api_key.clone());
        }
        if !cfg.system.trim().is_empty() {
            req.system = cfg.system.clone();
        }
        if !cfg.prompt.trim().is_empty() {
            req.messages.push(AiMessage { role: "user".into(), content: cfg.prompt.clone() });
        }
        if cfg.temperature.is_some() {
            req.temperature = cfg.temperature;
        }
        if cfg.max_tokens.is_some() {
            req.max_tokens = cfg.max_tokens;
        }
        req.stream = cfg.stream;

        let (ai_cfg, mcp) = {
            let scopes = self.scopes(scope_id.as_deref());
            let refs = self.ref_ctx(&act.payload);
            let r = Renderer { refs: &refs, scopes: &scopes, vault: env.services.vault.as_deref() };
            let resolve = |s: &str| r.render(s);
            let mut auth = req.auth.clone();
            for key in ["token", "value"] {
                let v = auth.get_string(key);
                if !v.is_empty() {
                    auth.set(key, resolve(&v));
                }
            }
            req.auth = auth;
            let ai_cfg = AiConfig::from_request(&req, &resolve);
            let mut mcp: Vec<(String, McpConfig)> = Vec::new();
            for id in &req.mcp_servers {
                for c in &env.collections {
                    if let Some(item) = c.find_request(id)
                        && let RequestKind::Mcp(m) = &item.request
                    {
                        // `Inherit` takes the folder's or collection's auth, as for requests.
                        let mut m = m.clone();
                        if let Some(eff) = c.effective_auth(id) {
                            m.auth = eff.auth.clone();
                        }
                        mcp.push((item.name.clone(), McpConfig::from_request(&m, &resolve)));
                    }
                }
            }
            (ai_cfg, mcp)
        };
        let ai_cfg = match ai_cfg {
            Ok(c) => c,
            Err(e) => return Outcome::fail(e.to_string()),
        };
        let http = match env.services.http.default_client() {
            Ok(c) => c,
            Err(e) => return Outcome::fail(e.message),
        };
        let mut clients: Vec<McpClient> = Vec::new();
        let mut tools = Vec::new();
        let mut interrupted = false;
        for (name, c) in mcp {
            // Starting a server and listing its tools can take a while
            // (`npx` downloads, slow servers): stop waiting when the run stops.
            let connected = tokio::select! {
                r = McpClient::connect_with_http(c, http.clone(), |_| {}) => r,
                _ = self.sh.halt.cancelled() => {
                    interrupted = true;
                    break;
                }
            };
            match connected {
                Ok(client) => {
                    clients.push(client.clone());
                    let listed = tokio::select! {
                        r = client.list_tools() => r,
                        _ = self.sh.halt.cancelled() => {
                            interrupted = true;
                            break;
                        }
                    };
                    match listed {
                        Ok(list) => tools.extend(list.into_iter().map(|t| (client.clone(), t))),
                        Err(e) => self.log(Some(&block.id), LogLevel::Warn, format!("{name}: tools/list failed: {e}")),
                    }
                }
                Err(e) => self.log(Some(&block.id), LogLevel::Warn, format!("{name}: could not connect: {e}")),
            }
        }
        if interrupted {
            for c in clients {
                c.close().await;
            }
            self.sh.note_cancel();
            return Outcome::fail(self.sh.interrupted());
        }
        // Text pieces and tool calls are reported as they happen.
        let sh = self.sh;
        let (id, stream) = (block.id.clone(), cfg.stream);
        let on_event = move |e: AiEvent| match e {
            AiEvent::Delta(text) if stream => sh.emit(FlowEvent::AiDelta { step, block: id.clone(), text }),
            AiEvent::ToolCall { name, arguments } => sh.emit(FlowEvent::Log {
                block: Some(id.clone()),
                level: LogLevel::Info,
                message: format!("tool call {name} {arguments}"),
            }),
            AiEvent::ToolResult { name, result } => sh.emit(FlowEvent::Log {
                block: Some(id.clone()),
                level: LogLevel::Info,
                message: format!("tool result {name}: {}", truncate(&result, 200)),
            }),
            _ => {}
        };
        let result = merpati_ai::run(ai_cfg, &http, tools, on_event, self.sh.halt.clone()).await;
        for c in clients {
            c.close().await;
        }
        match result {
            Ok(resp) => {
                let parsed = serde_json::from_str::<Value>(strip_code_fence(&resp.text)).ok();
                let output = json!({
                    "text": resp.text,
                    "json": parsed.clone().unwrap_or(Value::Null),
                    "model": resp.model,
                    "finish_reason": resp.finish_reason,
                    "input_tokens": resp.input_tokens,
                    "output_tokens": resp.output_tokens,
                    "latency_ms": resp.latency_ms,
                });
                if cfg.require_json && parsed.is_none() {
                    let mut out = Outcome::fail("the answer is not valid JSON");
                    out.output = output;
                    return out;
                }
                Outcome::ok(PORT_SUCCESS, output)
            }
            Err(AiError::Cancelled) => {
                self.sh.note_cancel();
                Outcome::fail(self.sh.interrupted())
            }
            Err(e) => Outcome::fail(e.to_string()),
        }
    }

    fn finish(self, started: Instant) -> FlowResult {
        let (mut r, cancelled, explicit_fail) = {
            let mut st = self.sh.state();
            (std::mem::take(&mut st.result), st.cancelled, st.explicit_fail)
        };
        r.cancelled = cancelled;
        if cancelled {
            r.errors.push("the run was cancelled".into());
        }
        let handled = r.failures.iter().filter(|f| f.handled).count();
        if handled > 0 && r.outputs.is_empty() {
            r.errors.push(format!(
                "{handled} failure{} handled by a failure path, but the flow produced no output",
                if handled == 1 { " was" } else { "s were" }
            ));
        }
        r.success = r.errors.is_empty() && r.unhandled_failures() == 0 && r.tests_failed == 0 && !explicit_fail;
        // Parallel branches finish out of order; report steps as they started.
        r.steps.sort_by_key(|s| s.step);
        r.variables = self.flow_vars;
        for id in &self.used_collections {
            if let Some(v) = self.collection_vars.get(id) {
                r.collection_variables.insert(id.clone(), v.clone());
            }
        }
        for b in &self.sh.flow.blocks {
            if let Some(rec) = self.records.get(&b.id) {
                r.blocks.insert(b.id.clone(), rec.clone());
            }
        }
        r.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        r.finished_at = chrono::Utc::now().to_rfc3339();
        self.sh.emit(FlowEvent::Finished {
            outputs: r.outputs.clone(),
            duration_ms: r.duration_ms,
            success: r.success,
        });
        r
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_specs() {
        assert_eq!(parse_status_spec("").unwrap(), vec![]);
        assert_eq!(parse_status_spec("2xx, 304").unwrap(), vec![(200, 299), (304, 304)]);
        assert_eq!(parse_status_spec("200-204").unwrap(), vec![(200, 204)]);
        assert!(parse_status_spec("9xx").is_err());
        assert!(parse_status_spec("abc").is_err());
        assert!(status_ok(399, &[]));
        assert!(!status_ok(404, &[]));
        assert!(status_ok(404, &parse_status_spec("2xx,404").unwrap()));
    }

    #[test]
    fn inputs_merge_defaults_and_required() {
        let mut f = Flow::new("x");
        f.inputs = vec![
            FlowInput::new("a", "1"),
            FlowInput { name: "b".into(), required: true, ..Default::default() },
            FlowInput { name: "c".into(), ..Default::default() },
        ];
        assert_eq!(resolve_inputs(&f, &IndexMap::new()).unwrap_err(), vec!["input \"b\" is required".to_string()]);
        let mut p = IndexMap::new();
        p.insert("b".to_string(), json!("2"));
        p.insert("extra".to_string(), json!("3"));
        let got = resolve_inputs(&f, &p).unwrap();
        assert_eq!(got.get("a"), Some(&json!("1")));
        assert_eq!(got.get("b"), Some(&json!("2")));
        assert!(!got.contains_key("c"));
        assert_eq!(got.get("extra"), Some(&json!("3")));
    }
}
