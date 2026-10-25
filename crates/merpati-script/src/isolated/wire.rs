//! Messages between the app and a worker process: one JSON object per line
//! in each direction. The app serialises the parts of a job it keeps in place
//! (`Cow::Borrowed`); everything is deserialised as owned values.

use std::borrow::Cow;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use merpati_core::response::ResponseBody;
use merpati_core::script::{
    PackageContext, ProtocolContext, ScriptCookie, ScriptEvent, ScriptHttp, ScriptInfo, ScriptJob, ScriptOutcome,
    ScriptRequest, ScriptResponse, ScriptSource,
};
use merpati_core::variables::VariableScopes;
use serde::{Deserialize, Serialize};
use tempfile::TempPath;

use crate::EngineConfig;

/// Changed whenever a message changes shape. App and worker are normally
/// the same executable; this catches a mismatched `worker_exe`.
pub(crate) const PROTOCOL_VERSION: u32 = 2;

/// In-memory bodies up to this size travel inline (base64). Larger ones are
/// written to a temporary file, which the worker only reads when a script
/// asks for the body.
const INLINE_BODY_LIMIT: usize = 256 * 1024;

/// App -> worker.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToWorker<'a> {
    /// The first line after the worker started.
    Config(WireConfig),
    Job(Box<WireJob<'a>>),
    /// Answer to [`HttpCall`].
    HttpReply {
        id: u64,
        result: Result<WireResponse<'a>, Cow<'a, str>>,
    },
    /// Stop the running job; it keeps its partial results like an in-process cancellation.
    Cancel,
}

/// Worker -> app.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FromWorker<'a> {
    /// Started and the prelude is compiled: jobs can be sent.
    Ready,
    Http(Box<HttpCall<'a>>),
    Done(Box<Cow<'a, ScriptOutcome>>),
}

/// A `pm.sendRequest` call, answered with [`ToWorker::HttpReply`].
#[derive(Serialize, Deserialize)]
pub(crate) struct HttpCall<'a> {
    pub id: u64,
    pub request: Cow<'a, ScriptRequest>,
    pub scopes: Cow<'a, VariableScopes>,
}

/// The [`EngineConfig`] settings that apply inside a worker process.
#[derive(Serialize, Deserialize)]
pub(crate) struct WireConfig {
    pub protocol: u32,
    pub loop_iteration_limit: u64,
    pub recursion_limit: usize,
    pub stack_size_limit: usize,
    pub thread_stack_bytes: usize,
    pub package_module_limit: u64,
    pub package_graph_limit: u64,
    pub package_fetch_timeout_ms: u64,
}

impl WireConfig {
    pub(crate) fn new(config: &EngineConfig) -> Self {
        // Destructured so a new setting cannot be forgotten here.
        let EngineConfig {
            loop_iteration_limit,
            recursion_limit,
            stack_size_limit,
            thread_stack_bytes,
            // The worker runs one job at a time and the app is its watchdog.
            max_idle_workers: _,
            watchdog_grace: _,
            package_module_limit,
            package_graph_limit,
            package_fetch_timeout,
        } = config;
        WireConfig {
            protocol: PROTOCOL_VERSION,
            loop_iteration_limit: *loop_iteration_limit,
            recursion_limit: *recursion_limit,
            stack_size_limit: *stack_size_limit,
            thread_stack_bytes: *thread_stack_bytes,
            package_module_limit: *package_module_limit,
            package_graph_limit: *package_graph_limit,
            package_fetch_timeout_ms: millis(*package_fetch_timeout),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WireJob<'a> {
    pub event: ScriptEvent,
    pub scripts: Cow<'a, [ScriptSource]>,
    pub info: Cow<'a, ScriptInfo>,
    pub request: Cow<'a, ScriptRequest>,
    pub response: Option<WireResponse<'a>>,
    pub variables: Cow<'a, VariableScopes>,
    pub cookies: Cow<'a, [ScriptCookie]>,
    /// Whether `pm.sendRequest` is available.
    pub http: bool,
    pub timeout_us: u64,
    pub protocol: Option<Cow<'a, ProtocolContext>>,
    pub packages: Option<Cow<'a, PackageContext>>,
}

impl WireJob<'_> {
    pub(crate) fn timeout(&self) -> Duration {
        Duration::from_micros(self.timeout_us)
    }

    /// The job inside the worker, with its own `http` and `cancel`.
    pub(crate) fn into_job(
        self,
        http: Option<Arc<dyn ScriptHttp>>,
        cancel: Arc<AtomicBool>,
    ) -> Result<ScriptJob, String> {
        let timeout = self.timeout();
        Ok(ScriptJob {
            event: self.event,
            scripts: self.scripts.into_owned(),
            info: self.info.into_owned(),
            request: self.request.into_owned(),
            response: self.response.map(WireResponse::into_response).transpose()?,
            variables: self.variables.into_owned(),
            cookies: self.cookies.into_owned(),
            http,
            timeout,
            cancel,
            protocol: self.protocol.map(Cow::into_owned),
            packages: self.packages.map(Cow::into_owned),
        })
    }
}

/// A [`ScriptResponse`] on the wire.
#[derive(Serialize, Deserialize)]
pub(crate) struct WireResponse<'a> {
    pub code: u16,
    pub status: Cow<'a, str>,
    pub headers: Cow<'a, [(String, String)]>,
    pub body: WireBody,
    /// `None` stands for NaN, which JSON cannot carry.
    pub response_time_ms: Option<f64>,
    pub response_size: u64,
}

impl<'a> WireResponse<'a> {
    fn new(r: &'a ScriptResponse, spilled: &mut Vec<TempPath>) -> Self {
        WireResponse {
            code: r.code,
            status: Cow::Borrowed(&r.status),
            headers: Cow::Borrowed(&r.headers),
            body: WireBody::new(&r.body, spilled),
            response_time_ms: Some(r.response_time_ms),
            response_size: r.response_size,
        }
    }

    pub(crate) fn into_response(self) -> Result<ScriptResponse, String> {
        Ok(ScriptResponse {
            code: self.code,
            status: self.status.into_owned(),
            headers: self.headers.into_owned(),
            body: self.body.into_body()?,
            response_time_ms: self.response_time_ms.unwrap_or(f64::NAN),
            response_size: self.response_size,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireBody {
    /// Base64.
    Memory(String),
    /// A temporary file owned by the app, which keeps it until the job ends.
    File { path: PathBuf, len: u64 },
}

impl WireBody {
    fn new(body: &ResponseBody, spilled: &mut Vec<TempPath>) -> Self {
        match body {
            ResponseBody::File { path, len } if is_portable(path) => {
                WireBody::File { path: path.to_path_buf(), len: *len }
            }
            ResponseBody::Memory(bytes) if bytes.len() > INLINE_BODY_LIMIT => match spill(bytes) {
                Ok(file) if is_portable(&file) => {
                    let wire = WireBody::File { path: file.to_path_buf(), len: bytes.len() as u64 };
                    spilled.push(file);
                    wire
                }
                _ => WireBody::Memory(BASE64.encode(bytes)),
            },
            ResponseBody::Memory(bytes) => WireBody::Memory(BASE64.encode(bytes)),
            // A path JSON cannot carry: send the content instead.
            ResponseBody::File { .. } => WireBody::Memory(BASE64.encode(body.read_all().unwrap_or_default())),
        }
    }

    fn into_body(self) -> Result<ResponseBody, String> {
        match self {
            WireBody::Memory(b64) => {
                BASE64.decode(b64).map(|b| ResponseBody::Memory(b.into())).map_err(|e| format!("invalid body: {e}"))
            }
            WireBody::File { path, len } => {
                let mut path = TempPath::try_from_path(path).map_err(|e| format!("invalid body path: {e}"))?;
                // The app deletes the file when the job ends.
                path.disable_cleanup(true);
                Ok(ResponseBody::File { path: Arc::new(path), len })
            }
        }
    }
}

/// Paths are sent as JSON strings, so they must be valid Unicode.
fn is_portable(path: &Path) -> bool {
    path.to_str().is_some()
}

fn spill(bytes: &[u8]) -> std::io::Result<TempPath> {
    let mut file = tempfile::Builder::new().prefix("merpati-body-").tempfile()?;
    file.write_all(bytes)?;
    Ok(file.into_temp_path())
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// One message as a line.
pub(crate) fn line<T: Serialize>(msg: &T) -> serde_json::Result<Vec<u8>> {
    let mut buf = serde_json::to_vec(msg)?;
    buf.push(b'\n');
    Ok(buf)
}

/// The job message. Large bodies are written to temporary files that are
/// added to `spilled` and must outlive the job.
pub(crate) fn job_line(job: &ScriptJob, spilled: &mut Vec<TempPath>) -> serde_json::Result<Vec<u8>> {
    // Destructured so a new job field cannot be forgotten here.
    let ScriptJob {
        event,
        scripts,
        info,
        request,
        response,
        variables,
        cookies,
        http,
        timeout,
        // Polled by the app, which forwards it as `ToWorker::Cancel`.
        cancel: _,
        protocol,
        packages,
    } = job;
    line(&ToWorker::Job(Box::new(WireJob {
        event: *event,
        scripts: Cow::Borrowed(scripts),
        info: Cow::Borrowed(info),
        request: Cow::Borrowed(request),
        response: response.as_ref().map(|r| WireResponse::new(r, spilled)),
        variables: Cow::Borrowed(variables),
        cookies: Cow::Borrowed(cookies),
        http: http.is_some(),
        timeout_us: u64::try_from(timeout.as_micros()).unwrap_or(u64::MAX),
        protocol: protocol.as_ref().map(Cow::Borrowed),
        packages: packages.as_ref().map(Cow::Borrowed),
    })))
}

/// The answer to a `pm.sendRequest` call; see [`job_line`] for `spilled`.
pub(crate) fn reply_line(
    id: u64,
    result: &Result<ScriptResponse, String>,
    spilled: &mut Vec<TempPath>,
) -> serde_json::Result<Vec<u8>> {
    let result = match result {
        Ok(r) => Ok(WireResponse::new(r, spilled)),
        Err(e) => Err(Cow::Borrowed(e.as_str())),
    };
    line(&ToWorker::HttpReply { id, result })
}
