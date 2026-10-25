//! Collection Runner (FR-U1..U6).
//!
//! Results are streamed to the caller through [`RunEvent`]s; the runner only
//! keeps aggregate counters, so memory stays flat no matter how many
//! iterations run (FR-U5). Callers decide what to retain.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use merpati_core::model::{Collection, Item, RequestItem};
use merpati_core::script::{NextRequest, TestResult};
use merpati_core::variables::VariableScopes;

use crate::data::DataRow;
use crate::pipeline::{ConsoleLine, ExecuteInput, Execution, PhaseError, Services, execute_keeping_locals};

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// Number of iterations; defaults to the number of data rows, or 1.
    pub iterations: Option<usize>,
    /// Pause between requests.
    pub delay_ms: u64,
    pub data: Vec<DataRow>,
    /// Only run this folder (id or name); a request id or name that matches
    /// no folder runs just that request (newman `--folder`).
    pub folder: Option<String>,
    /// Custom order/subset of request ids (FR-U6). Overrides `folder`.
    pub order: Option<Vec<String>>,
    /// Stop the whole run at the first failing request.
    pub bail: bool,
    pub base_dir: Option<PathBuf>,
    /// Keep up to this many bytes of each response body in the results.
    pub body_preview_bytes: usize,
    /// Keep console lines in the results.
    pub keep_console: bool,
    /// Fraction (0..=1) of requests that fail with a simulated network error
    /// instead of being sent, to test how a flow copes with failures (FR-U6).
    pub simulate_failure_rate: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestResult {
    pub iteration: usize,
    /// Execution number within the iteration.
    pub index: usize,
    pub request_id: String,
    pub name: String,
    /// Folder names from the collection root to the request.
    pub path: Vec<String>,
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub status_text: String,
    pub response_time_ms: f64,
    pub response_size: u64,
    pub tests: Vec<TestResult>,
    pub error: Option<String>,
    pub script_errors: Vec<PhaseError>,
    pub skipped: bool,
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<(String, String)>,
    pub body_preview: Option<String>,
    pub console: Vec<ConsoleLine>,
}

impl RequestResult {
    pub fn tests_failed(&self) -> usize {
        self.tests.iter().filter(|t| !t.passed && !t.skipped).count()
    }

    pub fn failed(&self) -> bool {
        self.error.is_some() || self.tests_failed() > 0 || !self.script_errors.is_empty()
    }

    pub fn from_execution(
        exec: &Execution,
        iteration: usize,
        index: usize,
        path: Vec<String>,
        opts: &RunOptions,
    ) -> Self {
        let resp = exec.response.as_ref();
        let (method, url) = match (&exec.sent, &exec.prepared, &exec.request) {
            (Some(s), _, _) => (s.method.clone(), s.url.clone()),
            (None, Some(p), _) => (p.method.clone(), p.url.clone()),
            (None, None, Some(r)) => (r.method.clone(), r.url.clone()),
            _ => (String::new(), String::new()),
        };
        RequestResult {
            iteration,
            index,
            request_id: exec.request_id.clone(),
            name: exec.request_name.clone(),
            path,
            method,
            url,
            status: resp.map(|r| r.status),
            status_text: resp.map(|r| r.status_text.clone()).unwrap_or_default(),
            response_time_ms: resp.map(|r| r.timings.total_ms).unwrap_or(0.0),
            response_size: resp.map(|r| r.total_size()).unwrap_or(0),
            tests: exec.tests.clone(),
            error: exec.error.as_ref().map(|e| e.message.clone()),
            script_errors: exec.script_errors.clone(),
            skipped: exec.skipped,
            request_headers: exec.sent.as_ref().map(|s| s.headers.clone()).unwrap_or_default(),
            response_headers: resp.map(|r| r.headers.clone()).unwrap_or_default(),
            body_preview: match (resp, opts.body_preview_bytes) {
                (Some(r), n) if n > 0 => r.body.read_range(0, n).ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
                _ => None,
            },
            console: if opts.keep_console { exec.console.clone() } else { Vec::new() },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub collection: String,
    pub iterations: usize,
    pub executed: usize,
    pub skipped: usize,
    /// Requests that failed at the network level.
    pub requests_failed: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    pub tests_skipped: usize,
    pub script_errors: usize,
    pub total_time_ms: f64,
    /// Requests that got a response (the ones the response times cover).
    #[serde(default)]
    pub responses: usize,
    pub response_time_total_ms: f64,
    pub response_time_min_ms: f64,
    pub response_time_max_ms: f64,
    pub total_bytes: u64,
    pub started_at: String,
    pub finished_at: String,
    pub cancelled: bool,
    pub bailed: bool,
}

impl RunSummary {
    pub fn response_time_avg_ms(&self) -> f64 {
        if self.responses == 0 { 0.0 } else { self.response_time_total_ms / self.responses as f64 }
    }

    pub fn success(&self) -> bool {
        self.requests_failed == 0 && self.tests_failed == 0 && self.script_errors == 0 && !self.cancelled
    }

    fn add(&mut self, r: &RequestResult) {
        self.executed += 1;
        if r.skipped {
            self.skipped += 1;
        }
        if r.error.is_some() {
            self.requests_failed += 1;
        }
        for t in &r.tests {
            if t.skipped {
                self.tests_skipped += 1;
            } else if t.passed {
                self.tests_passed += 1;
            } else {
                self.tests_failed += 1;
            }
        }
        self.script_errors += r.script_errors.len();
        if r.status.is_some() {
            let t = r.response_time_ms;
            if self.responses == 0 || t < self.response_time_min_ms {
                self.response_time_min_ms = t;
            }
            self.responses += 1;
            self.response_time_max_ms = self.response_time_max_ms.max(t);
            self.response_time_total_ms += t;
            self.total_bytes += r.response_size;
        }
    }
}

#[derive(Clone, Debug)]
pub enum RunEvent {
    Started {
        iterations: usize,
        requests: usize,
    },
    IterationStarted {
        iteration: usize,
    },
    RequestStarted {
        iteration: usize,
        request_id: String,
        name: String,
    },
    RequestFinished(Box<RequestResult>),
    /// The full execution, for callers that want the response (e.g. the UI
    /// keeping the latest one). Sent right before `RequestFinished`.
    Execution(Box<Execution>),
    Finished(RunSummary),
}

fn simulated_failure(item: &RequestItem) -> Execution {
    let message = "Simulated network failure (runner failure simulation)".to_string();
    Execution {
        request_id: item.id.clone(),
        request_name: item.name.clone(),
        request: item.http().cloned(),
        error: Some(merpati_http::HttpError::new(merpati_http::HttpErrorKind::ConnectionReset, message.clone())),
        console: vec![crate::pipeline::ConsoleLine {
            kind: crate::pipeline::ConsoleKind::Warn,
            message,
            source: item.name.clone(),
            phase: "network".into(),
            at_ms: chrono::Utc::now().timestamp_millis(),
        }],
        ..Default::default()
    }
}

/// Requests to run, depth first, with their folder path.
pub fn plan<'a>(c: &'a Collection, opts: &RunOptions) -> Result<Vec<(&'a RequestItem, Vec<String>)>, String> {
    fn walk<'a>(items: &'a [Item], path: &mut Vec<String>, out: &mut Vec<(&'a RequestItem, Vec<String>)>) {
        for item in items {
            match item {
                Item::Request(r) => out.push((r, path.clone())),
                Item::Folder(f) => {
                    path.push(f.name.clone());
                    walk(&f.items, path, out);
                    path.pop();
                }
            }
        }
    }
    let mut all = Vec::new();
    walk(&c.items, &mut Vec::new(), &mut all);
    if let Some(order) = &opts.order {
        return order
            .iter()
            .map(|id| all.iter().find(|(r, _)| &r.id == id).cloned().ok_or_else(|| format!("request {id} not found")))
            .collect();
    }
    if let Some(folder) = &opts.folder {
        fn find<'a>(items: &'a [Item], key: &str) -> Option<&'a merpati_core::model::Folder> {
            for item in items {
                if let Item::Folder(f) = item {
                    if f.id == key || f.name == key {
                        return Some(f);
                    }
                    if let Some(x) = find(&f.items, key) {
                        return Some(x);
                    }
                }
            }
            None
        }
        let Some(f) = find(&c.items, folder) else {
            // Like newman's `--folder`, a request (id or name) runs on its own.
            return all
                .into_iter()
                .find(|(r, _)| &r.id == folder || &r.name == folder)
                .map(|r| vec![r])
                .ok_or_else(|| format!("folder or request \"{folder}\" not found"));
        };
        let prefix =
            c.ancestors(&f.id).unwrap_or_default().iter().skip(1).map(|a| a.name().to_string()).collect::<Vec<_>>();
        let mut out = Vec::new();
        let mut path = prefix;
        path.push(f.name.clone());
        walk(&f.items, &mut path, &mut out);
        return Ok(out);
    }
    Ok(all)
}

/// Where `pm.execution.setNextRequest(target)` jumps to, as postman-runtime
/// does it (`prepareLookupHash` in `lib/runner/extensions/waterfall.command.js`):
/// an item id wins over a name, and among items sharing a name the last one
/// in the run order is picked (later names overwrite earlier ones in its
/// lookup table). `None` ends the iteration.
fn next_position(list: &[(&RequestItem, Vec<String>)], target: &str) -> Option<usize> {
    list.iter().position(|(r, _)| r.id == target).or_else(|| list.iter().rposition(|(r, _)| r.name == target))
}

/// Runs a collection. `vars` holds globals/collection/environment state and
/// is updated as scripts change variables.
pub async fn run_collection(
    services: &Services,
    collection: &Collection,
    vars: &mut VariableScopes,
    opts: RunOptions,
    sink: &mut (dyn FnMut(RunEvent) + Send),
    cancel: &CancellationToken,
) -> Result<RunSummary, String> {
    let list = plan(collection, &opts)?;
    let iterations = opts.iterations.unwrap_or(if opts.data.is_empty() { 1 } else { opts.data.len() }).max(1);
    let started = Instant::now();
    let mut summary = RunSummary {
        collection: collection.name.clone(),
        iterations,
        started_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };
    sink(RunEvent::Started { iterations, requests: list.len() });
    // Guards against setNextRequest loops that never end.
    let max_steps = 10_000usize.max(list.len() * 100);

    // Local variables (`pm.variables.set`) last for the whole run, as in Postman.
    vars.local.clear();
    'run: for iteration in 0..iterations {
        // Iterations past the end of the data reuse the last row (postman-runtime).
        vars.data = match opts.data.get(iteration).or(opts.data.last()) {
            Some(row) => row.clone(),
            None => Default::default(),
        };
        sink(RunEvent::IterationStarted { iteration });
        let mut i = 0;
        let mut steps = 0;
        while i < list.len() {
            if cancel.is_cancelled() {
                summary.cancelled = true;
                break 'run;
            }
            let (item, path) = &list[i];
            sink(RunEvent::RequestStarted { iteration, request_id: item.id.clone(), name: item.name.clone() });
            let input = ExecuteInput {
                collection,
                request_id: &item.id,
                request_override: None,
                iteration,
                iteration_count: iterations,
                base_dir: opts.base_dir.as_deref(),
            };
            let exec = if opts.simulate_failure_rate > 0.0 && rand::random::<f32>() < opts.simulate_failure_rate {
                simulated_failure(item)
            } else {
                execute_keeping_locals(services, input, vars, cancel).await
            };
            if matches!(&exec.error, Some(e) if e.kind == merpati_http::HttpErrorKind::Cancelled) {
                summary.cancelled = true;
            }
            let result = RequestResult::from_execution(&exec, iteration, steps, path.clone(), &opts);
            summary.add(&result);
            let failed = result.failed();
            let next = exec.next_request.clone();
            sink(RunEvent::Execution(Box::new(exec)));
            sink(RunEvent::RequestFinished(Box::new(result)));
            if summary.cancelled {
                break 'run;
            }
            if opts.bail && failed {
                summary.bailed = true;
                break 'run;
            }
            steps += 1;
            if steps >= max_steps {
                break;
            }
            i = match next {
                None => i + 1,
                Some(NextRequest::Stop) => break,
                Some(NextRequest::Named(name)) => match next_position(&list, &name) {
                    Some(pos) => pos,
                    None => break,
                },
            };
            if opts.delay_ms > 0 && i < list.len() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(opts.delay_ms)) => {}
                    _ = cancel.cancelled() => {
                        summary.cancelled = true;
                        break 'run;
                    }
                }
            }
        }
    }
    vars.data = Default::default();
    vars.local.clear();
    summary.total_time_ms = started.elapsed().as_secs_f64() * 1000.0;
    summary.finished_at = chrono::Utc::now().to_rfc3339();
    sink(RunEvent::Finished(summary.clone()));
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(status: Option<u16>, ms: f64, error: Option<&str>) -> RequestResult {
        RequestResult { status, response_time_ms: ms, error: error.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn response_time_stats_cover_every_response() {
        let mut s = RunSummary::default();
        // A session/AI request can fail after a response arrived; its time
        // counts in total, min and max, so it counts in the average too.
        s.add(&result(Some(200), 0.0, None));
        s.add(&result(Some(502), 50.0, Some("closed abnormally")));
        s.add(&result(Some(200), 100.0, None));
        s.add(&result(None, 0.0, Some("connection refused")));
        assert_eq!(s.requests_failed, 2);
        assert_eq!(s.response_time_min_ms, 0.0);
        assert_eq!(s.response_time_max_ms, 100.0);
        assert_eq!(s.response_time_avg_ms(), 50.0);
        assert_eq!(RunSummary::default().response_time_avg_ms(), 0.0);
    }
}
