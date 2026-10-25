//! Local Flows (FR-D4): compose requests, conditions, loops, parallel
//! branches, scripts and AI calls as blocks on a canvas, run them from the
//! app or the CLI (`merpati flow run`).
//!
//! * [`model`]: the flow document ([`Flow`], [`Block`], [`Edge`], per-kind
//!   configs and ports).
//! * [`storage`]: `<workspace>/flows/<slug>.flow.json` files.
//! * [`exec`]: [`run_flow`], driven by [`FlowEvent`]s, with cancellation and
//!   a step limit.
//! * [`validate`]: diagnostics per block for the canvas.
//!
//! # Blocks and ports
//!
//! | kind | inputs | outputs | output value |
//! |---|---|---|---|
//! | `start` | – | `next` | the flow inputs |
//! | `request` | `in` | `success`, `failure` | response (below) |
//! | `condition` | `in` | `true`, `false` | the boolean |
//! | `loop` | `in`, `break` | `item` (per element), `done` | `{item, index, count, first, last}`; on done `{count, iterations, broken}` |
//! | `parallel` | `in` | `branches` (one branch per edge) | the value it received |
//! | `join` | `in` | `next` | `{results, items, sources, count, branches}` (below) |
//! | `script` | `in` | `success`, `failure` | the script's `return` value |
//! | `ai` | `in` | `success`, `failure` | `{text, json, model, finish_reason, input_tokens, output_tokens, latency_ms}` (with `stream`, the text is also reported as it arrives) |
//! | `delay` | `in` | `next` | `{ms}` |
//! | `set_variable` | `in` | `next` | the value |
//! | `template` | `in` | `success`, `failure` | the value |
//! | `output` | `in` | `next` | the value (also stored as a named flow output) |
//! | `log` | `in` | `next` | the message |
//!
//! A Request block's output is `{status, status_text, ok, headers, body,
//! json, time_ms, size, method, url, tests, error, skipped}`; header names
//! are lower-case, `json` is the parsed body (or null) and `error` says why
//! the block failed (null on success). It ends on
//! `success` when the request was sent, the status matches
//! `success_status` (default: below 400), every test passed and no script
//! threw; otherwise on `failure`.
//!
//! # Execution
//!
//! Runs start at the Start block. A finished block activates the blocks
//! connected to the port it finished on, depth first; when a port fans
//! out, targets run top to bottom, then left to right on the canvas, then in
//! edge order. A block with several incoming edges runs once per
//! activation (use a Join to wait for parallel branches). A Loop runs its body — everything
//! reachable from `item` — to completion for each element, then fires
//! `done`; an edge into its `break` input stops it, and an edge from the
//! body back into the loop just ends that path. Loops are capped by
//! `max_iterations` (default 1000) and runs by `max_steps` (default 10,000
//! block executions).
//!
//! A block that fails while its `failure` port is connected is *handled*
//! and the run continues there. A failure with no failure edge (and any
//! failure of a block without a failure port) stops the run, unless
//! `settings.continue_on_failure` is set. A run succeeds when nothing failed
//! unhandled, no test failed, it was not cancelled, no Output block with
//! `fail` was reached, and — if failures were handled — at least one Output
//! was produced.
//!
//! # Parallel branches and Join
//!
//! A Parallel block starts one *branch* per edge leaving its `branches`
//! port, and the branches run at the same time: their requests, AI calls,
//! delays and scripts overlap, so two 300 ms requests take about 300 ms.
//! `settings.max_concurrent` caps how many branches of a Parallel block run
//! at once (0 = all); the others start in branch order as running ones
//! finish, with the same results. Each branch runs depth first like the rest
//! of the flow until it has nothing left to do; reaching a Join ends that
//! path of the branch. Events of the branches interleave (`step` ties a
//! block's events together, `branch` says which branch: `1`, `2`, `2.1` in
//! a nested Parallel, `1+2` after a Join that joined branches 1 and 2 only;
//! [`StepRecord::started_ms`] shows how they overlapped) and the
//! `max_steps` budget is shared.
//!
//! * **State.** A branch starts from a copy of the flow variables,
//!   environment, globals, collection variables and block results, and does
//!   not see what other branches change. When all branches are done their
//!   changes are applied in branch order (top to bottom, then left to right
//!   on the canvas): when two branches set the same variable, the later
//!   branch wins; a block that ran in several branches keeps the later
//!   branch's output and adds up its runs. Output blocks, tests, failures
//!   and the request count are recorded as they happen.
//! * **Join.** A Join waits for the branches that can reach it, following
//!   the connections from where each branch starts (any port, but not into
//!   a `break`); which branches those are depends only on the canvas, never
//!   on timing. It fires once with `{results, items, sources, count,
//!   branches}`: `results` maps the label (or id) of each block that led
//!   into the Join to its output
//!   (`{{blocks.Join.output.results.Users.json}}`), `items` holds the same
//!   values as a list in branch order, `sources` their block ids, `count`
//!   the number of arrivals and `branches` the number of branches. A block
//!   that arrives several times keeps its last value in `results`. A branch
//!   that can reach the Join but does not (another path, a failure) is just
//!   missing, so `count` can be lower than `branches`.
//!   * A Join every branch can reach waits for the whole group and fires
//!     after the changes are applied (Joins reached by the group fire top to
//!     bottom, where the Parallel block ran, so inside a loop body they are
//!     part of the iteration).
//!   * A Join only some branches can reach fires as soon as those have
//!     finished, while the other branches keep running. What follows it runs
//!     alongside them like one more branch: it starts from the state before
//!     the Parallel block plus the changes of the branches it waited for
//!     (applied in branch order), does not see the other branches' changes,
//!     and its own changes are applied after all branches (Joins top to
//!     bottom), so it wins over them.
//!
//!   A Join reached outside of a Parallel's branches does not wait: it
//!   passes each arrival on with `count` 1. Nested Parallel blocks work the
//!   same way; a Join reached in a branch belongs to the innermost group.
//! * **Failures.** A handled failure continues on the branch's failure
//!   path. An unhandled one stops the run: blocks still running in other
//!   branches are interrupted (they end on `failure` with a "stopped"
//!   error that is not counted as a failure) and no further Join fires.
//!   With `continue_on_failure` the other branches go on and the Join fires
//!   without the failed branch. Cancelling interrupts every branch.
//! * **Loops.** A Parallel block inside a loop body runs its group once per
//!   iteration; branches see the loop (`{{loop.item}}`) and may run loops
//!   of their own. A `break` of a loop around the group stops that loop once
//!   all branches are done and drops what the iteration still had to do,
//!   including Joins that wait for the whole group; a Join that already
//!   fired early (it waited only for other branches) has run.
//!
//! # Variables
//!
//! Each run has its own flow variables (initially `--var` values) layered
//! over the environment and globals: flow inputs and variables are the data
//! scope, so `{{name}}` finds them, `pm.variables.get` too. Request blocks run
//! the full request pipeline (collection/folder scripts, auth, variables)
//! like the Runner; `pm.variables.set` in their scripts sets flow
//! variables, `pm.environment.set` and `pm.globals.set` change the run's
//! environment and globals (passed in and updated by [`run_flow`]).
//!
//! # Templates
//!
//! Every text field is a template. Besides normal variables (`{{baseUrl}}`,
//! `{{$guid}}`, `{{vault:key}}`) these flow references are available:
//!
//! * `{{blocks.<label or id>.output.<path>}}`: output of a block that ran,
//!   e.g. `{{blocks.Login.output.json.token}}` or
//!   `{{blocks.list.output.json.items[0].id}}`. `output.` may be left out
//!   (`{{blocks.login.json.token}}`); `.port`, `.error`, `.duration_ms` and
//!   `.runs` describe the last run. Blocks are matched by id, then label
//!   (exact, case-insensitive, then slug: `get-users` for "Get users").
//! * `{{inputs.<name>}}`: flow inputs.
//! * `{{vars.<name>}}`: flow variables (same as `{{name}}`, but never a
//!   collection or environment variable).
//! * `{{loop.item}}`, `{{loop.index}}`, `{{loop.count}}`, `{{loop.first}}`,
//!   `{{loop.last}}`: the innermost loop iteration (`{{loop.item.id}}`).
//! * `{{input}}`: the output of the block whose edge activated this block.
//! * `{{flow.name}}`, `{{flow.id}}`.
//!
//! Paths use `.key`, `[0]`, `[-1]` (from the end), `["key with.dots"]` and
//! `.length`. Objects and arrays render as compact JSON. A field that is a
//! single reference (`{{blocks.list.output.json.items}}`) keeps the JSON
//! type where a value is expected (Set Variable, Output, Template, Loop
//! items, Condition operands) and is null when it does not resolve; inside
//! longer text, unresolved references stay as written. Output blocks that
//! run several times (in a loop) keep the last value.
//!
//! In JavaScript (Script, Condition expressions, Template expressions,
//! Loop item expressions) the same data is available as `blocks` (keyed by
//! id and label: `blocks["Get users"].output.json`), `inputs`, `vars`,
//! `loop`, `input` and `flow`, next to the `pm` API. Assigning
//! `vars.x = ...` sets a flow variable.

pub mod exec;
pub mod js;
pub mod model;
pub mod storage;
pub mod template;
pub mod validate;

pub use exec::{
    DEFAULT_MAX_STEPS, Failure, FlowEnv, FlowEvent, FlowResult, FlowRunOptions, StepRecord, resolve_inputs, run_flow,
};
pub use model::*;
pub use template::BlockRecord;
pub use validate::{Diagnostic, Severity, validate};
