//! Host-side HTTP fetching of package modules: registry host only, bounded
//! size, timeouts and cancellation. Scripts never get network access.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use url::Url;

use super::spec::Registry;

const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_millis(300);

struct Net {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

static NET: OnceLock<Result<Net, String>> = OnceLock::new();

/// Started on first use, so jobs without packages never pay for it.
fn net() -> Result<&'static Net, String> {
    NET.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("merpati-packages")
            .enable_all()
            .build()
            .map_err(|e| format!("could not start the package fetcher: {e}"))?;
        let client = {
            let _guard = runtime.enter();
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(CONNECT_TIMEOUT)
                .user_agent(merpati_core::settings::USER_AGENT)
                .build()
                .map_err(|e| format!("could not create the package HTTP client: {e}"))?
        };
        Ok(Net { runtime, client })
    })
    .as_ref()
    .map_err(Clone::clone)
}

pub(crate) struct Limits {
    pub max_bytes: u64,
    pub timeout: Duration,
    pub deadline: Instant,
}

pub(crate) struct Fetched {
    /// URL after redirects.
    pub url: String,
    pub bytes: Vec<u8>,
}

enum Step {
    Redirect(String),
    Body(Vec<u8>),
    /// 5xx answer, retried once.
    ServerError(String),
}

fn too_large(url: &str, max: u64) -> String {
    format!("package module {url} is larger than the {} limit", super::store::size_text(max))
}

async fn fetch_once(client: &reqwest::Client, url: &str, max: u64) -> Result<Step, String> {
    let mut resp = client.get(url).send().await.map_err(|e| format!("could not fetch {url}: {e}"))?;
    let status = resp.status();
    if status.is_redirection() {
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| format!("{url}: redirect without a location"))?;
        let next =
            Url::parse(url).and_then(|u| u.join(loc)).map_err(|e| format!("{url}: bad redirect '{loc}': {e}"))?;
        return Ok(Step::Redirect(next.to_string()));
    }
    if !status.is_success() {
        let mut text = String::new();
        if let Ok(Some(chunk)) = resp.chunk().await {
            text = String::from_utf8_lossy(&chunk[..chunk.len().min(300)]).trim().to_string();
        }
        let detail = text.lines().next().map(|l| format!(": {l}")).unwrap_or_default();
        let msg = format!("{url}: the package registry answered {status}{detail}");
        return if status.is_server_error() { Ok(Step::ServerError(msg)) } else { Err(msg) };
    }
    if resp.content_length().is_some_and(|n| n > max) {
        return Err(too_large(url, max));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("could not read {url}: {e}"))? {
        if (body.len() + chunk.len()) as u64 > max {
            return Err(too_large(url, max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Step::Body(body))
}

/// Runs `fut` with a timeout, giving up early when `cancel` is set.
fn block_on<T>(
    net: &Net,
    fut: impl Future<Output = Result<T, String>>,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<T, String> {
    net.runtime.block_on(async {
        let fut = tokio::time::timeout(timeout, fut);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                r = &mut fut => return r.unwrap_or_else(|_| Err(format!("timed out after {} ms", timeout.as_millis()))),
                _ = tokio::time::sleep(Duration::from_millis(25)) => {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("Script execution was cancelled".to_string());
                    }
                }
            }
        }
    })
}

/// Fetches one module, following redirects only within the registry.
pub(crate) fn fetch(registry: &Registry, url: &str, limits: &Limits, cancel: &AtomicBool) -> Result<Fetched, String> {
    let net = net()?;
    let mut current = url.to_string();
    let mut retried = false;
    for _ in 0..=MAX_REDIRECTS + 1 {
        let parsed = Url::parse(&current).map_err(|e| format!("bad package URL {current}: {e}"))?;
        if !registry.owns(&parsed) {
            return Err(format!(
                "{url} redirects to {current}, which is not on the package registry ({})",
                registry.host()
            ));
        }
        let left = limits.deadline.saturating_duration_since(Instant::now()).min(limits.timeout);
        if left.is_zero() {
            return Err(format!("no time left to fetch {current}"));
        }
        let step = block_on(net, fetch_once(&net.client, &current, limits.max_bytes), left, cancel)
            .map_err(|e| if e.starts_with("timed out") { format!("fetching {current} {e}") } else { e })?;
        match step {
            Step::Redirect(next) => current = next,
            Step::Body(bytes) => return Ok(Fetched { url: current, bytes }),
            Step::ServerError(msg) if retried => return Err(msg),
            Step::ServerError(_) => {
                retried = true;
                std::thread::sleep(RETRY_DELAY.min(limits.deadline.saturating_duration_since(Instant::now())));
            }
        }
    }
    Err(format!("{url}: too many redirects"))
}
