//! Non-HTTP requests in the pipeline: gRPC calls, AI requests and MCP saved
//! calls run in the Collection Runner and CLI like HTTP requests (FR-X9), with
//! their scripts at the protocol's execution points (FR-S7): pre-request,
//! on-message (each gRPC response message) and post-response.
//!
//! WebSocket, Socket.IO and MQTT requests run as a scripted session until
//! their stop condition, see [`crate::session`].

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use merpati_core::model::{Ancestor, Collection, HttpRequest, RequestItem, RequestKind};
use merpati_core::protocols::{AiRequest, GrpcRequest, GrpcSchemaSource, McpCallKind, McpRequest};
use merpati_core::response::{HttpResponse, ResponseBody};
use merpati_core::script::protocol::{self as proto, Direction};
use merpati_core::script::{ProtocolContext, ScriptEvent, ScriptInfo, ScriptJob};
use merpati_core::variables::{Resolver, VariableScopes};
use merpati_http::{HttpError, HttpErrorKind};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::pipeline::{
    ConsoleKind, ExecuteInput, Execution, Services, merge_script_scopes, run_scripts, script_chain, with_folder_vars,
};

pub(crate) struct Ctx<'a> {
    pub(crate) services: &'a Services,
    pub(crate) collection: &'a Collection,
    pub(crate) chain: Vec<Ancestor<'a>>,
    pub(crate) item: &'a RequestItem,
    pub(crate) info: ScriptInfo,
    pub(crate) cancel: &'a CancellationToken,
    pub(crate) base_dir: Option<&'a std::path::Path>,
}

impl Ctx<'_> {
    /// Runs the scripts of `event`; returns the request JSON as left by the
    /// scripts (pre-request only).
    pub(crate) async fn scripts(
        &self,
        event: ScriptEvent,
        protocol: ProtocolContext,
        vars: &mut VariableScopes,
        exec: &mut Execution,
    ) -> Option<Value> {
        let scripts = script_chain(&self.chain, &self.item.scripts, &self.item.name, event);
        if scripts.is_empty() {
            return None;
        }
        let before = with_folder_vars(vars, &self.chain);
        let mut job = ScriptJob::for_protocol(event, scripts, protocol);
        job.info = self.info.clone();
        job.variables = before.clone();
        job.http = Some(Arc::new(crate::script_http::PipelineHttp::new(self.services.clone())));
        job.packages = self.services.packages.clone();
        let mut outcome = run_scripts(self.services, job, self.cancel).await;
        exec.absorb(event, &mut outcome);
        merge_script_scopes(vars, &before, std::mem::take(&mut outcome.variables));
        if outcome.skip_request {
            exec.skipped = true;
        }
        outcome.protocol_request
    }

    /// Extra CA and client certificate of the engine's certificate settings
    /// for a WebSocket, Socket.IO or MQTT connection to `url`.
    pub(crate) fn tls_setup(&self, url: &str, verify: bool) -> Result<merpati_http::tls::TlsSetup, String> {
        let engine = self.services.http.config();
        merpati_http::tls::TlsSetup::for_url(&engine.certificates, engine.secret_resolver.as_ref(), verify, url)
    }

    /// The engine's proxy settings, for WebSocket, Socket.IO, MQTT and gRPC
    /// connections (applied like for HTTP requests).
    pub(crate) fn proxy(&self) -> merpati_core::settings::ProxySettings {
        self.services.http.config().proxy
    }

    /// ` via proxy socks5://host:port` when a connection to `url` goes
    /// through a proxy (never with credentials), for the console.
    pub(crate) fn via_proxy(&self, url: &str, default_scheme: &str) -> String {
        merpati_http::proxy::proxy_for_url(&self.proxy(), url, default_scheme)
            .map(|p| format!(" via proxy {p}"))
            .unwrap_or_default()
    }

    /// Resolves `{{variables}}` (folder variables and vault included).
    pub(crate) fn resolver(&self, vars: &VariableScopes) -> impl Fn(&str) -> String + use<> {
        let scopes = self.scopes(vars);
        let vault = self.services.vault.clone();
        move |s: &str| {
            let mut r = Resolver::new(&scopes);
            if let Some(v) = &vault {
                r = r.with_vault(v.as_ref());
            }
            r.resolve(s)
        }
    }

    /// Variable scopes as seen by the request (folder variables layered on
    /// the collection ones).
    pub(crate) fn scopes(&self, vars: &VariableScopes) -> VariableScopes {
        with_folder_vars(vars, &self.chain)
    }
}

/// Executes a non-HTTP request item (called by [`crate::pipeline::execute`]).
pub(crate) async fn execute(
    services: &Services,
    input: &ExecuteInput<'_>,
    item: &RequestItem,
    vars: &mut VariableScopes,
    info: ScriptInfo,
    cancel: &CancellationToken,
    mut exec: Execution,
) -> Execution {
    let ctx = Ctx {
        services,
        collection: input.collection,
        chain: input.collection.ancestors(&item.id).unwrap_or_default(),
        item,
        info,
        cancel,
        base_dir: input.base_dir,
    };
    match &item.request {
        RequestKind::Grpc(r) => grpc(&ctx, r, vars, &mut exec).await,
        RequestKind::Ai(r) => ai(&ctx, r, vars, &mut exec).await,
        RequestKind::Mcp(r) => mcp(&ctx, r, vars, &mut exec).await,
        RequestKind::WebSocket(r) => crate::session::websocket(&ctx, r, vars, &mut exec).await,
        RequestKind::SocketIo(r) => crate::session::socketio(&ctx, r, vars, &mut exec).await,
        RequestKind::Mqtt(r) => crate::session::mqtt(&ctx, r, vars, &mut exec).await,
        // Sent by the pipeline itself.
        RequestKind::Http(_) => {}
    }
    exec
}

/// Shows the call in reports (method column and URL).
pub(crate) fn shown_request(method: &str, url: String) -> HttpRequest {
    HttpRequest::new(method, url)
}

pub(crate) fn response(
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body: String,
    ms: f64,
) -> HttpResponse {
    let mut r = HttpResponse {
        status,
        status_text,
        version: String::new(),
        headers,
        body: ResponseBody::Memory(Bytes::from(body)),
        ..Default::default()
    };
    r.timings.total_ms = ms;
    r
}

pub(crate) fn fail(exec: &mut Execution, message: String) {
    exec.network(ConsoleKind::Error, message.clone());
    exec.error = Some(HttpError::new(HttpErrorKind::Other, message));
}

/// HTTP-like status for reports: gRPC codes mapped to their usual HTTP
/// counterparts (the gRPC code itself is in the status text and scripts).
fn grpc_http_status(code: i32) -> u16 {
    match code {
        0 => 200,
        1 => 499,
        3 | 9 | 11 => 400,
        4 => 504,
        5 => 404,
        6 | 10 => 409,
        7 => 403,
        8 => 429,
        12 => 501,
        14 => 503,
        16 => 401,
        _ => 500,
    }
}

async fn grpc(ctx: &Ctx<'_>, r: &GrpcRequest, vars: &mut VariableScopes, exec: &mut Execution) {
    let mut req = r.clone();
    if let Some(v) = ctx.scripts(ScriptEvent::PreRequest, proto::grpc(&req), vars, exec).await {
        proto::apply_grpc(&mut req, &v);
    }
    exec.request = Some(shown_request("GRPC", format!("{}/{}/{}", req.url, req.service, req.method)));
    if exec.skipped || ctx.cancel.is_cancelled() {
        return;
    }
    let resolve = ctx.resolver(vars);
    let engine = ctx.services.http.config();
    let target = merpati_grpc::Target::from_request(&req, &resolve, ctx.services.defaults.verify_ssl)
        .with_certificates(engine.certificates, engine.secret_resolver)
        .with_proxy(engine.proxy);
    let schema = match &req.schema {
        GrpcSchemaSource::Reflection => merpati_grpc::load_via_reflection(&target).await,
        GrpcSchemaSource::ProtoFiles { files, import_paths } => {
            let files: Vec<_> = files.iter().map(|f| resolve(f).into()).collect();
            let imports: Vec<_> = import_paths.iter().map(|f| resolve(f).into()).collect();
            merpati_grpc::load_proto_files(&files, &imports, ctx.base_dir)
        }
    };
    let schema = match schema {
        Ok(s) => s,
        Err(e) => return fail(exec, format!("gRPC schema: {e}")),
    };
    let streaming_input = schema
        .method(&req.service, &req.method)
        .is_some_and(|m| matches!(m.kind, merpati_grpc::MethodKind::ClientStreaming | merpati_grpc::MethodKind::Bidi));
    // Client streams send the saved messages (or the editor message).
    let messages: Vec<String> = if streaming_input && !req.messages.is_empty() {
        req.messages.iter().map(|m| resolve(&m.body)).collect()
    } else {
        vec![resolve(if req.message.trim().is_empty() { "{}" } else { &req.message })]
    };
    exec.network(ConsoleKind::Info, format!("gRPC {}/{} → {}", req.service, req.method, req.url));
    let started = Instant::now();
    let call = tokio::select! {
        r = merpati_grpc::invoke(&target, &schema, &req.service, &req.method, &messages) => r,
        _ = ctx.cancel.cancelled() => {
            exec.error = Some(HttpError::cancelled());
            return;
        }
    };
    let res = match call {
        Ok(res) => res,
        Err(e) => return fail(exec, format!("gRPC: {e}")),
    };
    let now = chrono::Utc::now().timestamp_millis();
    let mut received = Vec::new();
    for m in &res.messages {
        let msg = proto::message(proto::json_or_text(m), Direction::Incoming, now);
        received.push(msg.clone());
        ctx.scripts(ScriptEvent::OnMessage, proto::grpc(&req).with_message(msg), vars, exec).await;
    }
    let ms = if res.elapsed_ms > 0.0 { res.elapsed_ms } else { started.elapsed().as_secs_f64() * 1000.0 };
    let resp_json = proto::grpc_response(res.code, &res.message, &res.headers, &res.trailers, received, ms);
    ctx.scripts(ScriptEvent::PostResponse, proto::grpc(&req).with_response(resp_json), vars, exec).await;
    let body = match res.messages.as_slice() {
        [one] => pretty(one),
        many => {
            let values: Vec<Value> = many.iter().map(|m| proto::json_or_text(m)).collect();
            serde_json::to_string_pretty(&values).unwrap_or_default()
        }
    };
    let mut headers = res.headers.clone();
    headers.extend(res.trailers.iter().cloned());
    let text = if res.message.is_empty() {
        format!("{} (gRPC {})", res.code_name, res.code)
    } else {
        format!("{} (gRPC {}): {}", res.code_name, res.code, res.message)
    };
    exec.response = Some(response(grpc_http_status(res.code), text, headers, body, ms));
}

fn pretty(json: &str) -> String {
    serde_json::from_str::<Value>(json).ok().and_then(|v| serde_json::to_string_pretty(&v).ok()).unwrap_or(json.into())
}

/// MCP servers attached to an AI request, looked up in the same collection.
async fn ai_tools(
    ctx: &Ctx<'_>,
    ids: &[String],
    vars: &VariableScopes,
    http: &reqwest::Client,
    exec: &mut Execution,
) -> (Vec<merpati_mcp::McpClient>, Vec<(merpati_mcp::McpClient, merpati_mcp::Tool)>) {
    let mut clients = Vec::new();
    let mut tools = Vec::new();
    for id in ids {
        let Some(RequestKind::Mcp(m)) = ctx.collection.find_request(id).map(|i| &i.request) else {
            exec.network(
                ConsoleKind::Warn,
                format!("MCP server {id} is not in this collection; its tools are left out"),
            );
            continue;
        };
        // `Inherit` takes the server's folder or collection auth, as for requests.
        let mut m = m.clone();
        if let Some(eff) = ctx.collection.effective_auth(id) {
            m.auth = eff.auth.clone();
        }
        let resolve = ctx.resolver(vars);
        let cfg = merpati_mcp::McpConfig::from_request(&m, &resolve);
        // Starting a server can take long: stop waiting when the run stops
        // (the caller closes the clients already connected).
        let connected = tokio::select! {
            r = merpati_mcp::McpClient::connect_with_http(cfg, http.clone(), |_| {}) => r,
            _ = ctx.cancel.cancelled() => break,
        };
        match connected {
            Ok(c) => {
                clients.push(c.clone());
                let listed = tokio::select! {
                    r = c.list_tools() => r,
                    _ = ctx.cancel.cancelled() => break,
                };
                match listed {
                    Ok(list) => tools.extend(list.into_iter().map(|t| (c.clone(), t))),
                    Err(e) => exec.network(ConsoleKind::Warn, format!("MCP tools/list: {e}")),
                }
            }
            Err(e) => exec.network(ConsoleKind::Warn, format!("MCP connect: {e}")),
        }
    }
    (clients, tools)
}

async fn ai(ctx: &Ctx<'_>, r: &AiRequest, vars: &mut VariableScopes, exec: &mut Execution) {
    let mut req = r.clone();
    if let Some(v) = ctx.scripts(ScriptEvent::PreRequest, proto::ai(&req), vars, exec).await {
        proto::apply_ai(&mut req, &v);
    }
    let base = if req.base_url.trim().is_empty() { merpati_ai::default_base_url(req.provider) } else { &req.base_url };
    exec.request = Some(shown_request("AI", format!("{base} · {}", req.model)));
    if exec.skipped || ctx.cancel.is_cancelled() {
        return;
    }
    let resolve = ctx.resolver(vars);
    // The API key is resolved here (vault references, variables).
    for key in ["token", "value"] {
        let v = req.auth.get_string(key);
        if !v.is_empty() {
            req.auth.set(key, resolve(&v));
        }
    }
    let cfg = match merpati_ai::AiConfig::from_request(&req, &resolve) {
        Ok(c) => c,
        Err(e) => return fail(exec, format!("AI: {e}")),
    };
    let http = match ctx.services.http.default_client() {
        Ok(c) => c,
        Err(e) => return fail(exec, e.message),
    };
    let (clients, tools) = ai_tools(ctx, &req.mcp_servers, vars, &http, exec).await;
    if ctx.cancel.is_cancelled() {
        for c in clients {
            c.close().await;
        }
        exec.error = Some(HttpError::cancelled());
        return;
    }
    exec.network(ConsoleKind::Info, format!("AI {} → {}", cfg.model, cfg.base_url));
    let tool_log = parking_lot::Mutex::new(Vec::new());
    let result = merpati_ai::run(
        cfg,
        &http,
        tools,
        |e| match e {
            merpati_ai::AiEvent::ToolCall { name, arguments } => {
                tool_log.lock().push(format!("tool {name} {arguments}"))
            }
            merpati_ai::AiEvent::ToolResult { name, result } => {
                tool_log.lock().push(format!("tool {name} → {}", crate::protocol::clip(&result, 300)))
            }
            _ => {}
        },
        ctx.cancel.clone(),
    )
    .await;
    for c in clients {
        c.close().await;
    }
    for line in tool_log.into_inner() {
        exec.network(ConsoleKind::Info, line);
    }
    let res = match result {
        Ok(r) => r,
        Err(merpati_ai::AiError::Cancelled) => {
            exec.error = Some(HttpError::cancelled());
            return;
        }
        Err(e) => {
            let status = match &e {
                merpati_ai::AiError::Provider { status, .. } => Some(*status),
                _ => None,
            };
            if let Some(status) = status {
                let resp_json = proto::ai_response("", &req.model, "error", json!({}), status, 0.0);
                ctx.scripts(ScriptEvent::PostResponse, proto::ai(&req).with_response(resp_json), vars, exec).await;
                exec.response = Some(response(status, e.to_string(), Vec::new(), e.to_string(), 0.0));
            }
            return fail(exec, format!("AI: {e}"));
        }
    };
    let usage = json!({"inputTokens": res.input_tokens, "outputTokens": res.output_tokens});
    let finish = res.finish_reason.clone().unwrap_or_default();
    let resp_json = proto::ai_response(&res.text, &res.model, &finish, usage, 200, res.latency_ms);
    ctx.scripts(ScriptEvent::PostResponse, proto::ai(&req).with_response(resp_json), vars, exec).await;
    let mut headers = vec![("model".to_string(), res.model.clone())];
    if !finish.is_empty() {
        headers.push(("finish-reason".into(), finish));
    }
    if let (Some(i), Some(o)) = (res.input_tokens, res.output_tokens) {
        headers.push(("usage".into(), format!("{i} in, {o} out")));
    }
    exec.response = Some(response(200, "OK".into(), headers, res.text, res.latency_ms));
}

async fn mcp(ctx: &Ctx<'_>, r: &McpRequest, vars: &mut VariableScopes, exec: &mut Execution) {
    let mut req = r.clone();
    if let Some(v) = ctx.scripts(ScriptEvent::PreRequest, proto::mcp(&req, req.saved_calls.first()), vars, exec).await {
        let mut first = req.saved_calls.first().cloned();
        proto::apply_mcp(&mut req, first.as_mut(), &v);
        if let (Some(f), Some(slot)) = (first, req.saved_calls.first_mut()) {
            *slot = f;
        }
    }
    let shown = match &req.transport {
        merpati_core::protocols::McpTransport::Http { url, .. } => url.clone(),
        merpati_core::protocols::McpTransport::Stdio { command, .. } => command.clone(),
    };
    exec.request = Some(shown_request("MCP", shown));
    if exec.skipped || ctx.cancel.is_cancelled() {
        return;
    }
    // `Inherit` takes the folder's or collection's auth, as for HTTP requests.
    req.auth = crate::session::effective_auth(ctx, &req.auth).0;
    let resolve = ctx.resolver(vars);
    let cfg = merpati_mcp::McpConfig::from_request(&req, &resolve);
    let http = match ctx.services.http.default_client() {
        Ok(c) => c,
        Err(e) => return fail(exec, e.message),
    };
    let started = Instant::now();
    // Starting a server can take long (`npx` downloads): stop waiting on cancel.
    let connected = tokio::select! {
        r = merpati_mcp::McpClient::connect_with_http(cfg, http, |_| {}) => r,
        _ = ctx.cancel.cancelled() => {
            exec.error = Some(HttpError::cancelled());
            return;
        }
    };
    let client = match connected {
        Ok(c) => c,
        Err(e) => return fail(exec, format!("MCP: {e}")),
    };
    let info = client.server_info().clone();
    exec.network(ConsoleKind::Info, format!("MCP connected to {} {}", info.name, info.version));
    let mut results = Vec::new();
    let mut any_error = false;
    if req.saved_calls.is_empty() {
        // Nothing to call: report the server's tools.
        let listed = tokio::select! {
            r = client.list_tools() => Some(r),
            _ = ctx.cancel.cancelled() => None,
        };
        match listed {
            None => {}
            Some(Ok(tools)) => {
                let names: Vec<Value> = tools.iter().map(|t| json!(t.name)).collect();
                let resp_json = proto::mcp_response(json!({"tools": names}), false, ms(started));
                ctx.scripts(
                    ScriptEvent::PostResponse,
                    proto::mcp(&req, None).with_response(resp_json.clone()),
                    vars,
                    exec,
                )
                .await;
                results.push(resp_json);
            }
            Some(Err(e)) => {
                client.close().await;
                return fail(exec, format!("MCP tools/list: {e}"));
            }
        }
    }
    for call in &req.saved_calls {
        if ctx.cancel.is_cancelled() {
            break;
        }
        let t = Instant::now();
        let args = if call.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&resolve(&call.arguments)).unwrap_or(json!({}))
        };
        let target = resolve(&call.target);
        let calling = async {
            match call.kind {
                McpCallKind::Tool => {
                    client.call_tool(&target, args).await.map(|r| (r.raw, r.is_error)).map_err(|e| e.to_string())
                }
                McpCallKind::Resource => {
                    client.read_resource(&target).await.map(|v| (v, false)).map_err(|e| e.to_string())
                }
                McpCallKind::Prompt => client
                    .get_prompt(&target, args.as_object().cloned().unwrap_or_default())
                    .await
                    .map(|v| (v, false))
                    .map_err(|e| e.to_string()),
            }
        };
        let outcome: Result<(Value, bool), String> = tokio::select! {
            r = calling => r,
            _ = ctx.cancel.cancelled() => break,
        };
        let (result, is_error) = match outcome {
            Ok(r) => r,
            Err(e) => (json!({"error": e}), true),
        };
        any_error |= is_error;
        exec.network(
            if is_error { ConsoleKind::Warn } else { ConsoleKind::Info },
            format!("MCP {} {target}{}", call_kind(call.kind), if is_error { " (error)" } else { "" }),
        );
        let resp_json = proto::mcp_response(result, is_error, ms(t));
        ctx.scripts(
            ScriptEvent::PostResponse,
            proto::mcp(&req, Some(call)).with_response(resp_json.clone()),
            vars,
            exec,
        )
        .await;
        results.push(resp_json);
    }
    client.close().await;
    if ctx.cancel.is_cancelled() {
        exec.error = Some(HttpError::cancelled());
        return;
    }
    let body = serde_json::to_string_pretty(&if results.len() == 1 { results.remove(0) } else { json!(results) })
        .unwrap_or_default();
    let (status, text) = if any_error { (500, "Error".to_string()) } else { (200, "OK".to_string()) };
    exec.response = Some(response(status, text, vec![("server".into(), info.name)], body, ms(started)));
}

fn call_kind(k: McpCallKind) -> &'static str {
    match k {
        McpCallKind::Tool => "tools/call",
        McpCallKind::Resource => "resources/read",
        McpCallKind::Prompt => "prompts/get",
    }
}

pub(crate) fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

pub(crate) fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}
