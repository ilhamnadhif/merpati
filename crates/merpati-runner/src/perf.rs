//! Performance testing (FR-U7): virtual users loop over requests for a fixed
//! duration; latency is recorded in a fixed-size log-scale histogram so memory
//! does not grow with the number of requests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use merpati_core::model::Collection;
use merpati_core::variables::VariableScopes;

use crate::pipeline::{ExecuteInput, Services, execute};
use crate::runner::{RunOptions, plan};

#[derive(Clone, Debug)]
pub struct PerfOptions {
    pub virtual_users: usize,
    pub duration: Duration,
    /// VUs start evenly spread over this period.
    pub ramp_up: Duration,
    /// Requests each VU loops over, in order; all requests when `None`.
    pub request_ids: Option<Vec<String>>,
}

impl Default for PerfOptions {
    fn default() -> Self {
        PerfOptions { virtual_users: 10, duration: Duration::from_secs(30), ramp_up: Duration::ZERO, request_ids: None }
    }
}

/// One second of the run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PerfSample {
    /// Seconds since the start (1 = the first second).
    pub second: u64,
    pub requests: u64,
    pub rps: f64,
    pub avg_latency_ms: f64,
    pub errors: u64,
    pub active_vus: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LatencyStats {
    pub min_ms: f64,
    pub max_ms: f64,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestStats {
    pub request_id: String,
    pub name: String,
    pub total: u64,
    pub errors: u64,
    pub latency: LatencyStats,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PerfReport {
    pub virtual_users: usize,
    pub duration_ms: f64,
    pub total: u64,
    /// Network errors plus responses with status >= 400.
    pub errors: u64,
    pub error_rate: f64,
    pub tests_failed: u64,
    pub throughput_rps: f64,
    pub latency: LatencyStats,
    pub per_request: Vec<RequestStats>,
    pub timeline: Vec<PerfSample>,
    pub cancelled: bool,
}

/// Log-scale histogram with ~2% relative precision from 1 µs to ~10 min.
#[derive(Clone)]
pub struct Histogram {
    buckets: Vec<u64>,
    count: u64,
    sum_us: f64,
    min_us: f64,
    max_us: f64,
}

const GROWTH: f64 = 1.02;
const BUCKETS: usize = 1100;

impl Default for Histogram {
    fn default() -> Self {
        Histogram { buckets: vec![0; BUCKETS], count: 0, sum_us: 0.0, min_us: f64::MAX, max_us: 0.0 }
    }
}

impl Histogram {
    fn index(us: f64) -> usize {
        if us <= 1.0 {
            return 0;
        }
        ((us.ln() / GROWTH.ln()) as usize + 1).min(BUCKETS - 1)
    }

    /// Upper bound of a bucket, in µs.
    fn upper(idx: usize) -> f64 {
        if idx == 0 { 1.0 } else { GROWTH.powi(idx as i32) }
    }

    pub fn record_ms(&mut self, ms: f64) {
        let us = (ms * 1000.0).max(0.0);
        self.buckets[Self::index(us)] += 1;
        self.count += 1;
        self.sum_us += us;
        self.min_us = self.min_us.min(us);
        self.max_us = self.max_us.max(us);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Value at quantile `q` (0..=1) in ms, clamped to the observed range.
    pub fn quantile_ms(&self, q: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let target = ((q * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (i, c) in self.buckets.iter().enumerate() {
            seen += c;
            if seen >= target {
                return Self::upper(i).clamp(self.min_us, self.max_us) / 1000.0;
            }
        }
        self.max_us / 1000.0
    }

    pub fn stats(&self) -> LatencyStats {
        if self.count == 0 {
            return LatencyStats::default();
        }
        LatencyStats {
            min_ms: self.min_us / 1000.0,
            max_ms: self.max_us / 1000.0,
            avg_ms: self.sum_us / self.count as f64 / 1000.0,
            p50_ms: self.quantile_ms(0.50),
            p90_ms: self.quantile_ms(0.90),
            p95_ms: self.quantile_ms(0.95),
            p99_ms: self.quantile_ms(0.99),
        }
    }
}

#[derive(Default)]
struct PerRequest {
    name: String,
    total: u64,
    errors: u64,
    hist: Histogram,
}

#[derive(Default)]
struct Agg {
    overall: Histogram,
    total: u64,
    errors: u64,
    tests_failed: u64,
    per_request: HashMap<String, PerRequest>,
    // Current one-second window.
    window_requests: u64,
    window_latency_ms: f64,
    window_errors: u64,
    active_vus: usize,
}

/// Runs a load test against the selected requests of `c`.
pub async fn run_perf(
    services: &Services,
    c: &Collection,
    vars: &VariableScopes,
    opts: PerfOptions,
    on_sample: &mut (dyn FnMut(&PerfSample) + Send),
    cancel: &CancellationToken,
) -> PerfReport {
    let plan_opts = RunOptions { order: opts.request_ids.clone(), ..Default::default() };
    let ids: Vec<(String, String)> = match plan(c, &plan_opts) {
        Ok(list) => list.into_iter().map(|(r, _)| (r.id.clone(), r.name.clone())).collect(),
        Err(_) => Vec::new(),
    };
    let vus = opts.virtual_users.max(1);
    let mut report = PerfReport { virtual_users: vus, ..Default::default() };
    if ids.is_empty() {
        return report;
    }
    let agg = Arc::new(Mutex::new(Agg::default()));
    {
        let mut a = agg.lock();
        for (id, name) in &ids {
            a.per_request.insert(id.clone(), PerRequest { name: name.clone(), ..Default::default() });
        }
    }
    let collection = Arc::new(c.clone());
    let ids = Arc::new(ids);
    let start = Instant::now();
    let deadline = start + opts.duration;
    // Cancels in-flight requests shortly after the deadline (or on cancel).
    let stop = cancel.child_token();

    let mut tasks = Vec::with_capacity(vus);
    for vu in 0..vus {
        let services = services.clone();
        let collection = collection.clone();
        let ids = ids.clone();
        let agg = agg.clone();
        let stop = stop.clone();
        let mut vars = vars.clone();
        let delay = if vus > 1 { opts.ramp_up.mul_f64(vu as f64 / vus as f64) } else { Duration::ZERO };
        tasks.push(tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = stop.cancelled() => return,
            }
            agg.lock().active_vus += 1;
            let mut iteration = 0;
            'outer: loop {
                for (id, _) in ids.iter() {
                    if Instant::now() >= deadline || stop.is_cancelled() {
                        break 'outer;
                    }
                    let input = ExecuteInput {
                        collection: &collection,
                        request_id: id,
                        request_override: None,
                        iteration,
                        iteration_count: 0,
                        base_dir: None,
                    };
                    let exec = execute(&services, input, &mut vars, &stop).await;
                    if exec.skipped {
                        continue;
                    }
                    if matches!(&exec.error, Some(e) if e.kind == merpati_http::HttpErrorKind::Cancelled) {
                        break 'outer;
                    }
                    let latency = exec.response.as_ref().map(|r| r.timings.total_ms).unwrap_or(0.0);
                    let is_error =
                        exec.error.is_some() || exec.response.as_ref().map(|r| r.status >= 400).unwrap_or(true);
                    let mut a = agg.lock();
                    a.total += 1;
                    a.window_requests += 1;
                    a.tests_failed += exec.tests_failed() as u64;
                    if is_error {
                        a.errors += 1;
                        a.window_errors += 1;
                    }
                    if exec.response.is_some() {
                        a.overall.record_ms(latency);
                        a.window_latency_ms += latency;
                    }
                    if let Some(p) = a.per_request.get_mut(id) {
                        p.total += 1;
                        if is_error {
                            p.errors += 1;
                        }
                        if exec.response.is_some() {
                            p.hist.record_ms(latency);
                        }
                    }
                }
                iteration += 1;
            }
            agg.lock().active_vus -= 1;
        }));
    }

    let mut second = 0u64;
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(1), Duration::from_secs(1));
    loop {
        let finished = tasks.iter().all(|t| t.is_finished());
        if finished {
            break;
        }
        tokio::select! {
            _ = ticker.tick() => {}
            _ = cancel.cancelled() => {
                report.cancelled = true;
                stop.cancel();
                break;
            }
        }
        second += 1;
        let sample = {
            let mut a = agg.lock();
            let s = PerfSample {
                second,
                requests: a.window_requests,
                rps: a.window_requests as f64,
                avg_latency_ms: if a.window_requests == 0 {
                    0.0
                } else {
                    a.window_latency_ms / a.window_requests as f64
                },
                errors: a.window_errors,
                active_vus: a.active_vus,
            };
            a.window_requests = 0;
            a.window_latency_ms = 0.0;
            a.window_errors = 0;
            s
        };
        on_sample(&sample);
        report.timeline.push(sample);
        // Give in-flight requests a short grace period after the deadline.
        if Instant::now() >= deadline + Duration::from_secs(5) {
            stop.cancel();
        }
    }
    for t in tasks {
        let _ = t.await;
    }

    let elapsed = start.elapsed().min(opts.duration.max(Duration::from_millis(1)) + Duration::from_secs(5));
    let a = agg.lock();
    report.duration_ms = elapsed.as_secs_f64() * 1000.0;
    report.total = a.total;
    report.errors = a.errors;
    report.tests_failed = a.tests_failed;
    report.error_rate = if a.total == 0 { 0.0 } else { a.errors as f64 / a.total as f64 };
    report.throughput_rps = if elapsed.is_zero() { 0.0 } else { a.total as f64 / elapsed.as_secs_f64() };
    report.latency = a.overall.stats();
    report.per_request = ids
        .iter()
        .filter_map(|(id, _)| {
            a.per_request.get(id).map(|p| RequestStats {
                request_id: id.clone(),
                name: p.name.clone(),
                total: p.total,
                errors: p.errors,
                latency: p.hist.stats(),
            })
        })
        .collect();
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_quantiles() {
        let mut h = Histogram::default();
        for i in 1..=1000 {
            h.record_ms(i as f64);
        }
        let s = h.stats();
        assert_eq!(h.count(), 1000);
        assert!((s.min_ms - 1.0).abs() < 1e-9);
        assert!((s.max_ms - 1000.0).abs() < 1e-9);
        assert!((s.avg_ms - 500.5).abs() < 1e-6);
        for (q, want) in [(s.p50_ms, 500.0), (s.p90_ms, 900.0), (s.p99_ms, 990.0)] {
            assert!((q - want).abs() / want < 0.03, "{q} vs {want}");
        }
        assert!(s.p50_ms <= s.p90_ms && s.p90_ms <= s.p95_ms && s.p95_ms <= s.p99_ms && s.p99_ms <= s.max_ms);
        assert_eq!(Histogram::default().stats(), LatencyStats::default());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_test_against_local_server() {
        use axum::Router;
        use axum::routing::get;
        use merpati_core::model::{HttpRequest, RequestItem};

        let app = Router::new()
            .route("/ok", get(|| async { "ok" }))
            .route("/fail", get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "no") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut c = Collection::new("Load");
        let ok = RequestItem::new("Ok", HttpRequest::new("GET", format!("http://{addr}/ok")));
        let fail = RequestItem::new("Fail", HttpRequest::new("GET", format!("http://{addr}/fail")));
        let (ok_id, fail_id) = (ok.id.clone(), fail.id.clone());
        c.items.push(ok.into());
        c.items.push(fail.into());
        // Direct connections: the timing must not depend on the machine's
        // proxy settings (reading them first can be slow in fresh binaries).
        let config = merpati_http::EngineConfig { proxy: Default::default(), ..Default::default() };
        let services = Services {
            http: Arc::new(merpati_http::HttpEngine::new(config, Arc::new(merpati_http::CookieJar::new()))),
            scripts: Arc::new(merpati_core::script::NoopEngine),
            tokens: Arc::new(merpati_core::tokens::MemoryTokenStore::default()),
            vault: None,
            defaults: Default::default(),
            script_timeout: Duration::from_secs(5),
            runtime: tokio::runtime::Handle::current(),
            packages: None,
        };
        let opts = PerfOptions {
            virtual_users: 3,
            duration: Duration::from_millis(1500),
            ramp_up: Duration::from_millis(300),
            request_ids: None,
        };
        let mut samples = 0;
        let report =
            run_perf(&services, &c, &VariableScopes::default(), opts, &mut |_| samples += 1, &CancellationToken::new())
                .await;
        assert!(report.total > 10, "{report:?}");
        assert_eq!(report.per_request.len(), 2);
        let ok_stats = report.per_request.iter().find(|r| r.request_id == ok_id).unwrap();
        let fail_stats = report.per_request.iter().find(|r| r.request_id == fail_id).unwrap();
        assert_eq!(ok_stats.errors, 0);
        assert_eq!(fail_stats.errors, fail_stats.total);
        assert!((report.error_rate - 0.5).abs() < 0.1, "{}", report.error_rate);
        assert!(report.throughput_rps > 0.0);
        assert!(report.latency.p50_ms <= report.latency.p99_ms);
        assert!(samples >= 1 && samples == report.timeline.len());
        assert!(report.timeline.iter().any(|s| s.active_vus > 0));

        // Only the selected request runs; cancellation stops early.
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c2.cancel();
        });
        let opts = PerfOptions {
            virtual_users: 2,
            duration: Duration::from_secs(30),
            ramp_up: Duration::ZERO,
            request_ids: Some(vec![ok_id.clone()]),
        };
        let started = Instant::now();
        let report = run_perf(&services, &c, &VariableScopes::default(), opts, &mut |_| {}, &cancel).await;
        assert!(report.cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(report.per_request.len(), 1);
        assert_eq!(report.errors, 0);
    }
}
