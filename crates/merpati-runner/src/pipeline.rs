//! Executing a single request end to end.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use merpati_core::auth::Auth;
use merpati_core::model::{
    Ancestor, Body, BodyMode, Collection, FormField, HttpRequest, Item, KeyValue, RawLanguage, RequestKind,
};
use merpati_core::prepared::{PrepareContext, PreparedBody, PreparedRequest, prepare};
use merpati_core::response::HttpResponse;
use merpati_core::script::{
    LogLevel, NextRequest, ScriptEngine, ScriptError, ScriptEvent, ScriptInfo, ScriptJob, ScriptOutcome, ScriptRequest,
    ScriptResponse, ScriptSource, TestResult, Visualization,
};
use merpati_core::settings::RequestDefaults;
use merpati_core::tokens::TokenStore;
use merpati_core::variables::{Resolver, VariableScopes, VaultLookup};
use merpati_http::{HttpEngine, HttpError, SendOptions, SentRequest};

/// Everything an execution needs besides the request and the variables.
#[derive(Clone)]
pub struct Services {
    pub http: Arc<HttpEngine>,
    pub scripts: Arc<dyn ScriptEngine>,
    pub tokens: Arc<dyn TokenStore>,
    pub vault: Option<Arc<dyn VaultLookup>>,
    pub defaults: RequestDefaults,
    pub script_timeout: Duration,
    /// Runtime used by `pm.sendRequest` from script threads.
    pub runtime: tokio::runtime::Handle,
    /// Where `pm.require('npm:…')` and Package Library modules come from;
    /// `None` = built-in libraries only.
    pub packages: Option<merpati_core::script::PackageContext>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleKind {
    Log,
    Info,
    Warn,
    Error,
    Debug,
    /// A request/response line written by the pipeline itself.
    Network,
}

impl From<LogLevel> for ConsoleKind {
    fn from(l: LogLevel) -> Self {
        match l {
            LogLevel::Log => ConsoleKind::Log,
            LogLevel::Info => ConsoleKind::Info,
            LogLevel::Warn => ConsoleKind::Warn,
            LogLevel::Error => ConsoleKind::Error,
            LogLevel::Debug => ConsoleKind::Debug,
        }
    }
}

/// One line of the app console (FR-S5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConsoleLine {
    pub kind: ConsoleKind,
    pub message: String,
    /// Request the line belongs to.
    pub source: String,
    /// `pre-request`, `post-response` or `network`.
    pub phase: String,
    /// Unix milliseconds.
    pub at_ms: i64,
}

/// Script errors with the event they happened in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhaseError {
    pub event: ScriptEvent,
    pub location: String,
    pub message: String,
}

/// Result of one execution.
#[derive(Clone, Debug, Default)]
pub struct Execution {
    pub request_id: String,
    pub request_name: String,
    /// The request after pre-request scripts, before variable resolution.
    pub request: Option<HttpRequest>,
    /// The resolved request with auth applied.
    pub prepared: Option<PreparedRequest>,
    pub sent: Option<SentRequest>,
    pub response: Option<HttpResponse>,
    pub error: Option<HttpError>,
    pub tests: Vec<TestResult>,
    pub console: Vec<ConsoleLine>,
    pub script_errors: Vec<PhaseError>,
    pub next_request: Option<NextRequest>,
    /// `pm.execution.skipRequest()` was called.
    pub skipped: bool,
    pub visualization: Option<Visualization>,
    pub auth_notes: Vec<String>,
    /// Variables referenced but undefined.
    pub unresolved: Vec<String>,
}

impl Execution {
    pub fn tests_passed(&self) -> usize {
        self.tests.iter().filter(|t| t.passed && !t.skipped).count()
    }

    pub fn tests_failed(&self) -> usize {
        self.tests.iter().filter(|t| !t.passed && !t.skipped).count()
    }

    /// True if the request failed, a test failed or a script threw.
    pub fn has_failures(&self) -> bool {
        self.error.is_some() || self.tests_failed() > 0 || !self.script_errors.is_empty()
    }
}

pub struct ExecuteInput<'a> {
    /// Context for inherited auth, scripts and folder variables.
    pub collection: &'a Collection,
    /// Item id inside `collection`.
    pub request_id: &'a str,
    /// Unsaved edits from the UI; replaces the stored request.
    pub request_override: Option<&'a HttpRequest>,
    pub iteration: usize,
    pub iteration_count: usize,
    /// Base for relative file paths in bodies.
    pub base_dir: Option<&'a Path>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn location(a: &Ancestor<'_>) -> String {
    match a {
        Ancestor::Collection(c) => format!("collection \"{}\"", c.name),
        Ancestor::Folder(f) => format!("folder \"{}\"", f.name),
    }
}

/// Scripts for one event in execution order (collection, folders, request).
pub fn script_chain(
    chain: &[Ancestor<'_>],
    own: &merpati_core::model::Scripts,
    name: &str,
    event: ScriptEvent,
) -> Vec<ScriptSource> {
    let pick = |s: &merpati_core::model::Scripts| s.code(event).clone();
    let mut out: Vec<ScriptSource> = chain
        .iter()
        .map(|a| ScriptSource { location: location(a), code: pick(a.scripts()) })
        .filter(|s| !s.code.trim().is_empty())
        .collect();
    let code = pick(own);
    if !code.trim().is_empty() {
        out.push(ScriptSource { location: format!("request \"{name}\""), code });
    }
    out
}

/// The scopes a request sees: the collection variables, values and types,
/// with the folder variables of its ancestors layered on top (inner folders
/// win).
pub(crate) fn with_folder_vars(vars: &VariableScopes, chain: &[Ancestor<'_>]) -> VariableScopes {
    let mut s = vars.clone();
    for a in chain {
        if let Ancestor::Folder(f) = a {
            let types = merpati_core::variables::to_types(&f.variables);
            for (k, v) in merpati_core::variables::to_map(&f.variables) {
                match types.get(&k) {
                    Some(t) => s.types.collection.insert(k.clone(), *t),
                    None => s.types.collection.shift_remove(&k),
                };
                s.collection.insert(k, v);
            }
        }
    }
    s
}

/// Takes the scopes a script left (`after`) into `vars`, except that the
/// collection scope only gets the values and types the script changed
/// compared to `before` (what [`with_folder_vars`] gave it): folder
/// variables themselves are read-only.
pub(crate) fn merge_script_scopes(vars: &mut VariableScopes, before: &VariableScopes, mut after: VariableScopes) {
    for (k, v) in &after.collection {
        let ty = after.types.collection.get(k);
        if before.collection.get(k) != Some(v) || before.types.collection.get(k) != ty {
            vars.collection.insert(k.clone(), v.clone());
            match ty {
                Some(t) => vars.types.collection.insert(k.clone(), *t),
                None => vars.types.collection.shift_remove(k),
            };
        }
    }
    for k in before.collection.keys() {
        if !after.collection.contains_key(k) {
            vars.collection.shift_remove(k);
            vars.types.collection.shift_remove(k);
        }
    }
    after.collection = std::mem::take(&mut vars.collection);
    after.types.collection = std::mem::take(&mut vars.types.collection);
    *vars = after;
}

pub fn script_request(id: &str, name: &str, r: &HttpRequest) -> ScriptRequest {
    ScriptRequest {
        id: Some(id.to_string()),
        name: Some(name.to_string()),
        method: r.method.clone(),
        url: r.url.clone(),
        headers: r.headers.clone(),
        body: r.body.clone(),
    }
}

/// Applies `pm.request` mutations from a pre-request script.
fn apply_script_request(r: &mut HttpRequest, s: &ScriptRequest) {
    if r.method != s.method && !s.method.is_empty() {
        r.method = s.method.clone();
    }
    if r.url != s.url {
        r.set_url(s.url.clone());
    }
    if r.headers != s.headers {
        r.headers = s.headers.clone();
    }
    if r.body != s.body {
        r.body = s.body.clone();
    }
}

/// `pm.request` for post-response scripts: what was actually sent.
fn sent_script_request(id: &str, name: &str, p: &PreparedRequest, sent: Option<&SentRequest>) -> ScriptRequest {
    let headers = match sent {
        Some(s) => s.headers.iter().map(|(k, v)| KeyValue::new(k.clone(), v.clone())).collect(),
        None => p.headers.iter().map(|(k, v)| KeyValue::new(k.clone(), v.clone())).collect(),
    };
    let body = match &p.body {
        PreparedBody::None => Body::default(),
        PreparedBody::Bytes(b) => Body::raw_text(String::from_utf8_lossy(b).into_owned(), RawLanguage::Text),
        PreparedBody::File(path) => {
            Body { mode: BodyMode::Binary, binary_path: path.display().to_string(), ..Default::default() }
        }
        PreparedBody::Multipart(parts) => Body {
            mode: BodyMode::FormData,
            form_data: parts
                .iter()
                .map(|part| match &part.value {
                    merpati_core::prepared::MultipartValue::Text(t) => FormField::text(part.name.clone(), t.clone()),
                    merpati_core::prepared::MultipartValue::File(f) => {
                        FormField::file(part.name.clone(), f.display().to_string())
                    }
                })
                .collect(),
            ..Default::default()
        },
    };
    ScriptRequest {
        id: Some(id.to_string()),
        name: Some(name.to_string()),
        method: p.method.clone(),
        url: sent.map(|s| s.url.clone()).unwrap_or_else(|| p.url.clone()),
        headers,
        body,
    }
}

pub fn script_response(r: &HttpResponse) -> ScriptResponse {
    ScriptResponse {
        code: r.status,
        status: r.status_text.clone(),
        headers: r.headers.clone(),
        body: r.body.clone(),
        response_time_ms: r.timings.total_ms,
        response_size: r.total_size(),
    }
}

/// Runs a script job on a blocking thread, honoring cancellation.
pub(crate) async fn run_scripts(services: &Services, mut job: ScriptJob, cancel: &CancellationToken) -> ScriptOutcome {
    let flag = Arc::new(AtomicBool::new(false));
    job.cancel = flag.clone();
    job.timeout = services.script_timeout;
    let watcher = {
        let cancel = cancel.clone();
        let flag = flag.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            flag.store(true, Ordering::SeqCst);
        })
    };
    let engine = services.scripts.clone();
    let request = job.request.clone();
    let variables = job.variables.clone();
    let outcome = tokio::task::spawn_blocking(move || engine.run(job)).await;
    watcher.abort();
    match outcome {
        Ok(o) => o,
        Err(e) => ScriptOutcome {
            request,
            variables,
            errors: vec![ScriptError { location: "script engine".into(), message: format!("engine crashed: {e}") }],
            ..Default::default()
        },
    }
}

impl Execution {
    pub(crate) fn absorb(&mut self, event: ScriptEvent, outcome: &mut ScriptOutcome) {
        let phase = event.label();
        let at = now_ms();
        for c in outcome.console.drain(..) {
            self.console.push(ConsoleLine {
                kind: c.level.into(),
                message: c.message,
                source: self.request_name.clone(),
                phase: phase.into(),
                at_ms: at,
            });
        }
        for e in outcome.errors.drain(..) {
            self.console.push(ConsoleLine {
                kind: ConsoleKind::Error,
                message: format!("{}: {}", e.location, e.message),
                source: self.request_name.clone(),
                phase: phase.into(),
                at_ms: at,
            });
            self.script_errors.push(PhaseError { event, location: e.location, message: e.message });
        }
        self.tests.append(&mut outcome.tests);
        if outcome.next_request.is_some() {
            self.next_request = outcome.next_request.take();
        }
        if outcome.visualization.is_some() {
            self.visualization = outcome.visualization.take();
        }
    }

    pub(crate) fn network(&mut self, kind: ConsoleKind, message: String) {
        self.console.push(ConsoleLine {
            kind,
            message,
            source: self.request_name.clone(),
            phase: "network".into(),
            at_ms: now_ms(),
        });
    }
}

/// Executes one request item (HTTP, or gRPC/AI/MCP through
/// [`crate::protocol`]). `vars` is updated with every variable change made
/// by scripts (local variables are reset for each request).
pub async fn execute(
    services: &Services,
    input: ExecuteInput<'_>,
    vars: &mut VariableScopes,
    cancel: &CancellationToken,
) -> Execution {
    vars.local.clear();
    execute_keeping_locals(services, input, vars, cancel).await
}

/// [`execute`] without resetting local variables: in a collection run they
/// last for the whole run, as in Postman.
pub(crate) async fn execute_keeping_locals(
    services: &Services,
    input: ExecuteInput<'_>,
    vars: &mut VariableScopes,
    cancel: &CancellationToken,
) -> Execution {
    let collection = input.collection;
    let mut exec = Execution { request_id: input.request_id.to_string(), ..Default::default() };
    let Some(Item::Request(item)) = collection.find(input.request_id) else {
        exec.network(ConsoleKind::Error, format!("request {} not found", input.request_id));
        return exec;
    };
    exec.request_name = item.name.clone();
    let mut request = match (input.request_override, &item.request) {
        (Some(r), _) => r.clone(),
        (None, RequestKind::Http(r)) => r.clone(),
        (None, _) => {
            let info = ScriptInfo {
                request_name: item.name.clone(),
                request_id: item.id.clone(),
                iteration: input.iteration,
                iteration_count: input.iteration_count.max(1),
                collection_name: collection.name.clone(),
            };
            return crate::protocol::execute(services, &input, item, vars, info, cancel, exec).await;
        }
    };
    let chain = collection.ancestors(&item.id).unwrap_or_default();
    let info = ScriptInfo {
        request_name: item.name.clone(),
        request_id: item.id.clone(),
        iteration: input.iteration,
        iteration_count: input.iteration_count.max(1),
        collection_name: collection.name.clone(),
    };

    // Pre-request scripts.
    let pre = script_chain(&chain, &item.scripts, &item.name, ScriptEvent::PreRequest);
    if !pre.is_empty() {
        let before = with_folder_vars(vars, &chain);
        let mut job = ScriptJob::new(ScriptEvent::PreRequest, pre, script_request(&item.id, &item.name, &request));
        job.info = info.clone();
        job.variables = before.clone();
        job.cookies = services.http.cookies().for_url(&Resolver::new(vars).resolve(&request.url));
        job.http = Some(Arc::new(crate::script_http::PipelineHttp::new(services.clone())));
        job.packages = services.packages.clone();
        let mut outcome = run_scripts(services, job, cancel).await;
        exec.absorb(ScriptEvent::PreRequest, &mut outcome);
        merge_script_scopes(vars, &before, std::mem::take(&mut outcome.variables));
        apply_script_request(&mut request, &outcome.request);
        if outcome.skip_request {
            exec.skipped = true;
            exec.request = Some(request);
            exec.network(ConsoleKind::Info, "request skipped by pm.execution.skipRequest()".into());
            return exec;
        }
    }
    if cancel.is_cancelled() {
        exec.error = Some(HttpError::cancelled());
        exec.request = Some(request);
        return exec;
    }

    // Resolve variables and auth.
    let own_auth = request.auth.clone();
    let (auth, owner): (Auth, String) = if !own_auth.is_inherit() {
        (own_auth, item.id.clone())
    } else {
        match merpati_core::model::effective_from_chain(&chain) {
            Some(eff) => (eff.auth.clone(), eff.owner_id.to_string()),
            None => (Auth::none(), collection.id.clone()),
        }
    };
    let scopes_for_resolve = with_folder_vars(vars, &chain);
    let mut resolver = Resolver::new(&scopes_for_resolve);
    if let Some(v) = &services.vault {
        resolver = resolver.with_vault(v.as_ref());
    }
    let ctx = PrepareContext {
        resolver,
        auth: &auth,
        auth_owner_id: Some(&owner),
        defaults: &services.defaults,
        base_dir: input.base_dir,
    };
    let mut prepared = prepare(&request, &ctx);
    exec.unresolved = prepared.unresolved.clone();
    for name in &prepared.unresolved {
        exec.network(ConsoleKind::Warn, format!("variable \"{name}\" is not defined"));
    }
    exec.request = Some(request);

    // Token requests use the engine, so a client certificate configured for
    // the token endpoint's host is presented (mutual TLS).
    let auth_ctx = merpati_auth::AuthContext { tokens: services.tokens.as_ref(), http: services.http.as_ref() };
    // Fetching an OAuth 2.0 token can take long: honor cancellation and the
    // request timeout while waiting for it.
    let limit = prepared.settings.timeout;
    let applied = tokio::select! {
        r = merpati_auth::apply(&mut prepared, &auth_ctx) => r.map_err(|e| {
            HttpError::new(merpati_http::HttpErrorKind::Other, format!("Auth error: {e}"))
        }),
        _ = cancel.cancelled() => Err(HttpError::cancelled()),
        _ = sleep_or_pending(limit) => Err(HttpError::new(
            merpati_http::HttpErrorKind::Timeout,
            format!("Auth error: no OAuth 2.0 token within {} ms", limit.unwrap_or_default().as_millis()),
        )),
    };
    match applied {
        Ok(applied) => {
            for n in &applied.notes {
                exec.network(ConsoleKind::Info, n.clone());
            }
            exec.auth_notes = applied.notes;
        }
        Err(e) => {
            exec.network(ConsoleKind::Error, e.message.clone());
            exec.error = Some(e);
            exec.prepared = Some(prepared);
            return exec;
        }
    }

    // Send, answering Digest/NTLM challenges (NTLM bound to the TLS server certificate).
    let opts = SendOptions::with_cancel(cancel.clone());
    let mut attempts = 0;
    let result = loop {
        let r = services.http.send(&prepared, &opts).await;
        if let Ok(out) = &r
            && out.response.status == 401
            && attempts < 3
            && let Some(mut retry) = challenge_target(&prepared, out)
        {
            let cert = out.peer_certificate.as_deref();
            match merpati_auth::handle_challenge(&mut retry, out.response.status, &out.response.headers, cert) {
                Ok(true) => {
                    prepared = retry;
                    attempts += 1;
                    continue;
                }
                Ok(false) => {}
                Err(e) => exec.network(ConsoleKind::Warn, format!("Auth challenge: {e}")),
            }
        }
        break r;
    };
    match result {
        Ok(out) => {
            let r = &out.response;
            exec.network(
                ConsoleKind::Network,
                format!(
                    "{} {} → {} {} ({}, {})",
                    prepared.method,
                    out.sent.url,
                    r.status,
                    r.status_text,
                    merpati_core::util::human_ms(r.timings.total_ms),
                    merpati_core::util::human_bytes(r.total_size())
                ),
            );
            exec.sent = Some(out.sent);
            exec.response = Some(out.response);
        }
        Err(e) => {
            exec.network(ConsoleKind::Error, format!("{} {} → {}", prepared.method, prepared.url, e.message));
            exec.error = Some(e);
            exec.prepared = Some(prepared);
            return exec;
        }
    }

    // Post-response scripts and tests.
    let post = script_chain(&chain, &item.scripts, &item.name, ScriptEvent::PostResponse);
    if !post.is_empty() {
        let before = with_folder_vars(vars, &chain);
        let sreq = sent_script_request(&item.id, &item.name, &prepared, exec.sent.as_ref());
        let mut job = ScriptJob::new(ScriptEvent::PostResponse, post, sreq);
        job.packages = services.packages.clone();
        job.info = info;
        job.variables = before.clone();
        job.response = exec.response.as_ref().map(script_response);
        job.cookies = services.http.cookies().for_url(&prepared.url);
        job.http = Some(Arc::new(crate::script_http::PipelineHttp::new(services.clone())));
        let mut outcome = run_scripts(services, job, cancel).await;
        exec.absorb(ScriptEvent::PostResponse, &mut outcome);
        merge_script_scopes(vars, &before, outcome.variables);
    }
    exec.prepared = Some(prepared);
    exec
}

async fn sleep_or_pending(limit: Option<Duration>) {
    match limit {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

/// The request that answers a `401` challenge. After redirects the challenge
/// comes from the last hop, so the answer goes there (Digest signs the method
/// and request target); `None` when that hop is on another origin, where the
/// redirect handling does not send credentials either.
fn challenge_target(prepared: &PreparedRequest, out: &merpati_http::SendOutcome) -> Option<PreparedRequest> {
    let mut retry = prepared.clone();
    if out.response.redirects.is_empty() {
        return Some(retry);
    }
    let origin = |u: &str| reqwest::Url::parse(u).ok().map(|u| u.origin());
    let same_origin = origin(&prepared.url).is_some_and(|o| Some(o) == origin(&out.sent.url));
    if !same_origin && !prepared.settings.follow_auth_header {
        return None;
    }
    retry.url = out.sent.url.clone();
    if !out.sent.method.eq_ignore_ascii_case(&prepared.method) {
        // The redirect turned the request into a GET without a body.
        retry.method = out.sent.method.clone();
        retry.body = PreparedBody::None;
        retry.remove_header("content-type");
        retry.remove_header("content-length");
    }
    Some(retry)
}

/// Builds a one-request collection for requests that are not saved anywhere.
pub fn standalone(name: &str, request: HttpRequest) -> (Collection, String) {
    let mut c = Collection::new("Scratch");
    let item = merpati_core::model::RequestItem::new(name, request);
    let id = item.id.clone();
    c.items.push(Item::Request(item));
    (c, id)
}

/// Re-runs post-response scripts against an existing response without
/// sending the request again (FR-S6).
pub async fn rerun_post_response(
    services: &Services,
    collection: &Collection,
    request_id: &str,
    prepared: &PreparedRequest,
    response: &HttpResponse,
    vars: &mut VariableScopes,
    cancel: &CancellationToken,
) -> Execution {
    let mut exec = Execution { request_id: request_id.to_string(), ..Default::default() };
    let Some(Item::Request(item)) = collection.find(request_id) else { return exec };
    exec.request_name = item.name.clone();
    let chain = collection.ancestors(request_id).unwrap_or_default();
    let post = script_chain(&chain, &item.scripts, &item.name, ScriptEvent::PostResponse);
    let before = with_folder_vars(vars, &chain);
    let mut job =
        ScriptJob::new(ScriptEvent::PostResponse, post, sent_script_request(&item.id, &item.name, prepared, None));
    job.packages = services.packages.clone();
    job.info = ScriptInfo {
        request_name: item.name.clone(),
        request_id: item.id.clone(),
        iteration: 0,
        iteration_count: 1,
        collection_name: collection.name.clone(),
    };
    job.variables = before.clone();
    job.response = Some(script_response(response));
    job.http = Some(Arc::new(crate::script_http::PipelineHttp::new(services.clone())));
    let mut outcome = run_scripts(services, job, cancel).await;
    exec.absorb(ScriptEvent::PostResponse, &mut outcome);
    merge_script_scopes(vars, &before, outcome.variables);
    exec
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::model::Folder;
    use merpati_core::variables::{VarType, Variable};

    #[test]
    fn folder_variables_keep_their_types_and_stay_read_only() {
        let typed = |k: &str, v: &str, ty| Variable { ty, ..Variable::new(k, v) };
        let mut folder = Folder::new("F");
        folder.variables = vec![typed("n", "5", VarType::Number), Variable::new("s", "text")];
        let mut vars = VariableScopes::default();
        vars.collection.insert("s".into(), "1".into());
        vars.types.collection.insert("s".into(), VarType::Number);
        vars.collection.insert("c".into(), "true".into());
        vars.types.collection.insert("c".into(), VarType::Boolean);

        let before = with_folder_vars(&vars, &[Ancestor::Folder(&folder)]);
        assert_eq!((before.collection["n"].as_str(), before.types.collection.get("n")), ("5", Some(&VarType::Number)));
        // A string folder variable hides the collection's number type.
        assert_eq!((before.collection["s"].as_str(), before.types.collection.get("s")), ("text", None));

        // A script that only changes the type of `c` and sets `new`.
        let mut after = before.clone();
        after.types.collection.shift_remove("c");
        after.collection.insert("new".into(), "2".into());
        after.types.collection.insert("new".into(), VarType::Number);
        after.environment.insert("e".into(), "x".into());
        merge_script_scopes(&mut vars, &before, after);
        assert_eq!(vars.environment["e"], "x");
        assert_eq!(vars.collection.keys().collect::<Vec<_>>(), vec!["s", "c", "new"], "folder values stay out");
        assert_eq!(vars.collection["s"], "1");
        let types: Vec<(&str, VarType)> = vars.types.collection.iter().map(|(k, t)| (k.as_str(), *t)).collect();
        assert_eq!(types, vec![("s", VarType::Number), ("new", VarType::Number)]);
    }
}
