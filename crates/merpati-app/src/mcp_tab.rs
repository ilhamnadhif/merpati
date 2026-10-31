//! MCP request tab (FR-X8): connect to a local (stdio) or remote (Streamable
//! HTTP) MCP server, browse tools/resources/prompts, call them, watch the
//! JSON-RPC traffic and export the server config for other MCP hosts.

use std::collections::VecDeque;
use std::time::Instant;

use egui::{RichText, Ui};

use merpati_core::model::{KeyValue, RequestKind};
use merpati_core::protocols::{McpCallKind, McpSavedCall, McpTransport};
use merpati_mcp::{
    Direction, ExportStyle, McpClient, McpConfig, McpEvent, Prompt, Resource, ResourceTemplate, ServerInfo, Tool,
};

use crate::app::{MerpatiApp, Msg};
use crate::highlight::Lang;
use crate::i18n::{tr, trf};
use crate::proto_common::{self as pc, Conn, ConnAction, Dir, ItemSlot, Log, ScriptState, header, resolver};
use crate::widgets::{self, KvOptions};
use crate::{icons, kit, theme};
use merpati_core::script::ScriptEvent;
use merpati_core::script::protocol as proto;

pub struct Catalog {
    pub info: ServerInfo,
    pub tools: Vec<Tool>,
    pub resources: Vec<Resource>,
    pub templates: Vec<ResourceTemplate>,
    pub prompts: Vec<Prompt>,
}

pub enum McpMsg {
    Connected(Result<(McpClient, Box<Catalog>), String>),
    Event(McpEvent),
    Result {
        title: String,
        text: String,
        is_error: bool,
        raw: serde_json::Value,
        ms: f64,
    },
    /// The server asks the user's model for a completion (needs approval).
    Sampling(Box<merpati_mcp::SamplingRequest>, SamplingReply),
    /// The server asks the user for input.
    Elicit(Box<merpati_mcp::ElicitationRequest>, ElicitReply),
    /// The approved sampling request `id` was answered (or failed).
    SamplingDone(u64, Result<String, String>),
}

pub type SamplingReply = tokio::sync::oneshot::Sender<Result<merpati_mcp::SamplingResult, merpati_mcp::McpError>>;
pub type ElicitReply = tokio::sync::oneshot::Sender<merpati_mcp::ElicitationResponse>;

struct PendingSampling {
    id: u64,
    request: Box<merpati_mcp::SamplingRequest>,
    reply: Option<SamplingReply>,
    /// Saved AI request used to answer.
    model: Option<String>,
    running: bool,
}

struct PendingElicit {
    request: Box<merpati_mcp::ElicitationRequest>,
    reply: Option<ElicitReply>,
    values: serde_json::Map<String, serde_json::Value>,
    problems: Vec<String>,
}

/// OAuth tokens and registered clients of MCP servers, in the workspace's
/// secret store (Keychain).
struct SecretOAuth(std::sync::Arc<merpati_core::secrets::SecretStore>);

impl merpati_mcp::OAuthStorage for SecretOAuth {
    fn load_tokens(&self, server: &str) -> Option<merpati_mcp::OAuthTokens> {
        self.0.get(&format!("mcp-oauth:tokens:{server}")).and_then(|s| serde_json::from_str(&s).ok())
    }

    fn save_tokens(&self, server: &str, tokens: Option<&merpati_mcp::OAuthTokens>) {
        let key = format!("mcp-oauth:tokens:{server}");
        match tokens.and_then(|t| serde_json::to_string(t).ok()) {
            Some(json) => self.0.set(&key, &json),
            None => self.0.remove(&key),
        }
    }

    fn load_client(&self, issuer: &str) -> Option<merpati_mcp::RegisteredClient> {
        self.0.get(&format!("mcp-oauth:client:{issuer}")).and_then(|s| serde_json::from_str(&s).ok())
    }

    fn save_client(&self, issuer: &str, client: Option<&merpati_mcp::RegisteredClient>) {
        let key = format!("mcp-oauth:client:{issuer}");
        match client.and_then(|c| serde_json::to_string(c).ok()) {
            Some(json) => self.0.set(&key, &json),
            None => self.0.remove(&key),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Tools,
    Resources,
    Prompts,
    Saved,
    Traffic,
    Scripts,
    Connection,
}

/// The result of the last call.
struct CallResult {
    title: String,
    text: String,
    is_error: bool,
    ms: Option<f64>,
}

pub struct McpTab {
    /// Number of the current connection (see `app::next_epoch`).
    pub epoch: u64,
    pub slot: ItemSlot,
    client: Option<McpClient>,
    connecting: bool,
    catalog: Option<Box<Catalog>>,
    view: View,
    selected: Option<(McpCallKind, String)>,
    args: String,
    result: Option<CallResult>,
    busy: bool,
    log: Log,
    export_style: ExportStyle,
    /// Requests from the server waiting for the user, answered oldest first.
    /// Dropping one (closing the tab, a closed connection) rejects it.
    sampling: VecDeque<PendingSampling>,
    elicit: VecDeque<PendingElicit>,
    next_sampling: u64,
    login: Option<tokio_util::sync::CancellationToken>,
    pub scripts: ScriptState,
    /// A call waiting for its pre-request script, then its post-response input.
    pending_call: Option<(McpCallKind, String, serde_json::Value)>,
    post: Option<(McpCallKind, String, serde_json::Value, serde_json::Value)>,
    /// Automation: `connect` or `call:<tool>:<json args>`.
    pub auto: Option<String>,
    /// Filter of the tools/resources/prompts list.
    search: String,
    since: Option<Instant>,
    /// Why the last connection failed.
    error: Option<String>,
}

impl McpTab {
    pub fn new(slot: ItemSlot) -> Self {
        let mut log =
            Log::with_empty("No traffic yet", "Connect to see the JSON-RPC messages exchanged with the server.");
        log.title = "";
        McpTab {
            epoch: 0,
            slot,
            client: None,
            connecting: false,
            catalog: None,
            view: View::Connection,
            selected: None,
            args: "{}".into(),
            result: None,
            busy: false,
            log,
            export_style: ExportStyle::default(),
            sampling: VecDeque::new(),
            elicit: VecDeque::new(),
            next_sampling: 0,
            login: None,
            scripts: ScriptState::default(),
            pending_call: None,
            post: None,
            auto: None,
            search: String::new(),
            since: None,
            error: None,
        }
    }

    fn state(&self) -> Conn {
        if self.client.is_some() {
            Conn::Live
        } else if self.connecting {
            Conn::Busy
        } else if self.error.is_some() {
            Conn::Failed
        } else {
            Conn::Idle
        }
    }

    pub fn on_msg(&mut self, m: McpMsg) {
        match m {
            McpMsg::Connected(Ok((client, catalog))) => {
                self.connecting = false;
                self.error = None;
                self.since = Some(Instant::now());
                let info = &catalog.info;
                self.log.system(trf(
                    "Connected to {} {} (protocol {})",
                    &[&info.name, &info.version, &info.protocol_version],
                ));
                self.client = Some(client);
                if self.view == View::Connection {
                    self.view = View::Tools;
                }
                self.catalog = Some(catalog);
            }
            McpMsg::Connected(Err(e)) => {
                self.connecting = false;
                self.error = Some(e.clone());
                self.log.error(e);
            }
            McpMsg::Event(e) => match e {
                McpEvent::Log { level, message } => {
                    self.log.push(Dir::In, trf("log/{}", &[&level]), message, crate::proto_common::now_ms())
                }
                McpEvent::Stderr(line) => self.log.push(Dir::System, "stderr", line, crate::proto_common::now_ms()),
                McpEvent::Notification { method, params } => {
                    self.log.push(Dir::In, method, params.to_string(), crate::proto_common::now_ms())
                }
                McpEvent::Traffic { direction, json, at_ms } => {
                    let dir = if direction == Direction::Incoming { Dir::In } else { Dir::Out };
                    let title = serde_json::from_str::<serde_json::Value>(&json)
                        .ok()
                        .map(|v| traffic_title(&v))
                        .unwrap_or_default();
                    self.log.push(dir, title, json, at_ms);
                }
                McpEvent::Closed(reason) => {
                    self.log.system(trf("Closed: {}", &[&reason]));
                    self.client = None;
                    self.catalog = None;
                    self.since = None;
                    self.drop_pending();
                }
            },
            McpMsg::Result { title, text, is_error, raw, ms } => {
                self.busy = false;
                if let Some((kind, target, args)) = self.pending_call.take() {
                    self.post = Some((kind, target, args, proto::mcp_response(raw, is_error, ms)));
                }
                self.result = Some(CallResult { title, text, is_error, ms: Some(ms) });
            }
            McpMsg::Sampling(request, reply) => {
                self.log.system(trf("{} asks to use your model", &[&request.server]));
                self.next_sampling += 1;
                let id = self.next_sampling;
                self.sampling.push_back(PendingSampling {
                    id,
                    request,
                    reply: Some(reply),
                    model: None,
                    running: false,
                });
            }
            McpMsg::Elicit(request, reply) => {
                let values = request
                    .fields()
                    .into_iter()
                    .filter_map(|f| f.default.clone().map(|d| (f.name.clone(), d)))
                    .collect();
                self.elicit.push_back(PendingElicit { request, reply: Some(reply), values, problems: Vec::new() });
            }
            McpMsg::SamplingDone(id, r) => {
                match &r {
                    Ok(_) => self.log.system(tr("Sampling answered")),
                    Err(e) => self.log.error(trf("Sampling failed: {}", &[&e])),
                }
                self.sampling.retain(|p| p.id != id);
            }
        }
    }

    /// Declines the sampling request shown (the oldest waiting).
    fn decline_sampling(&mut self) {
        if let Some(mut p) = self.sampling.pop_front()
            && let Some(r) = p.reply.take()
        {
            let _ = r.send(Err(merpati_mcp::McpError::rejected("The user declined the request")));
        }
    }

    /// Answers the elicitation shown (the oldest waiting).
    fn answer_elicit(&mut self, answer: merpati_mcp::ElicitationResponse) {
        if let Some(mut p) = self.elicit.pop_front()
            && let Some(r) = p.reply.take()
        {
            let _ = r.send(answer);
        }
    }

    /// Forgets the requests of a connection that ended; their (dropped)
    /// replies reject them.
    fn drop_pending(&mut self) {
        self.sampling.clear();
        self.elicit.clear();
    }
}

/// The log title of a JSON-RPC message: its method, or which request a
/// response answers (`result #3`, `error #3`).
fn traffic_title(v: &serde_json::Value) -> String {
    if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
        return m.to_string();
    }
    let id = v.get("id").map(|i| i.to_string()).unwrap_or_default();
    if v.get("error").is_some() {
        format!("error #{id}")
    } else if v.get("result").is_some() {
        format!("result #{id}")
    } else {
        String::new()
    }
}

fn connect(
    app: &mut MerpatiApp,
    tab_id: u64,
    tab: &mut McpTab,
    item_id: &str,
    req: &merpati_core::protocols::McpRequest,
    collection: Option<&str>,
) {
    let r = resolver(app, collection);
    let cfg = McpConfig::from_request(&with_inherited_auth(app, collection, item_id, req), &r);
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    tab.connecting = true;
    tab.error = None;
    tab.drop_pending();
    tab.log.system(trf("Connecting to {}…", &[&connecting_target(req)]));
    let login = tokio_util::sync::CancellationToken::new();
    tab.login = Some(login.clone());
    let http = app.http.default_client().ok();
    let storage: std::sync::Arc<dyn merpati_mcp::OAuthStorage> =
        std::sync::Arc::new(SecretOAuth(app.ws.secrets.clone()));
    let roots: Vec<merpati_mcp::Root> = merpati_mcp::Root::from_path(app.ws.root()).into_iter().collect();
    tab.epoch = crate::app::next_epoch();
    let epoch = tab.epoch;
    app.rt.spawn(async move {
        let tx_events = tx.clone();
        let ctx_events = ctx.clone();
        let (tx_s, ctx_s, tx_e, ctx_e) = (tx.clone(), ctx.clone(), tx.clone(), ctx.clone());
        let result = async {
            let mut builder = McpClient::builder(cfg)
                .on_event(move |e| {
                    let _ = tx_events.send(Msg::Mcp { tab: tab_id, epoch, event: McpMsg::Event(e) });
                    ctx_events.request_repaint();
                })
                .oauth(
                    merpati_mcp::OAuthOptions::new(|url| {
                        crate::widgets::open_web_link(url);
                    })
                    .with_storage(storage)
                    .with_cancel(login),
                )
                .sampling(move |req: merpati_mcp::SamplingRequest| {
                    let (reply, wait) = tokio::sync::oneshot::channel();
                    let _ = tx_s.send(Msg::Mcp { tab: tab_id, epoch, event: McpMsg::Sampling(Box::new(req), reply) });
                    ctx_s.request_repaint();
                    async move {
                        wait.await.unwrap_or_else(|_| Err(merpati_mcp::McpError::rejected("The request was dismissed")))
                    }
                })
                .elicitation(move |req: merpati_mcp::ElicitationRequest| {
                    let (reply, wait) = tokio::sync::oneshot::channel();
                    let _ = tx_e.send(Msg::Mcp { tab: tab_id, epoch, event: McpMsg::Elicit(Box::new(req), reply) });
                    ctx_e.request_repaint();
                    async move { Ok(wait.await.unwrap_or(merpati_mcp::ElicitationResponse::Cancel)) }
                })
                .roots(roots);
            if let Some(h) = http {
                builder = builder.http_client(h);
            }
            let client = builder.connect().await.map_err(|e| e.to_string())?;
            let info = client.server_info().clone();
            let tools =
                if info.has_capability("tools") { client.list_tools().await.unwrap_or_default() } else { Vec::new() };
            let (resources, templates) = if info.has_capability("resources") {
                (
                    client.list_resources().await.unwrap_or_default(),
                    client.list_resource_templates().await.unwrap_or_default(),
                )
            } else {
                (Vec::new(), Vec::new())
            };
            let prompts = if info.has_capability("prompts") {
                client.list_prompts().await.unwrap_or_default()
            } else {
                Vec::new()
            };
            Ok::<_, String>((client, Box::new(Catalog { info, tools, resources, templates, prompts })))
        }
        .await;
        let _ = tx.send(Msg::Mcp { tab: tab_id, epoch, event: McpMsg::Connected(result) });
        ctx.request_repaint();
    });
}

fn run_call(
    app: &MerpatiApp,
    tab_id: u64,
    client: McpClient,
    kind: McpCallKind,
    target: String,
    args: serde_json::Value,
) {
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    app.rt.spawn(async move {
        let started = std::time::Instant::now();
        let mut raw = serde_json::Value::Null;
        let (title, text, is_error) = match kind {
            McpCallKind::Tool => match client.call_tool(&target, args).await {
                Ok(r) => {
                    raw = r.raw.clone();
                    let mut text = r.text();
                    if let Some(s) = &r.structured {
                        text.push_str("\n\nstructuredContent:\n");
                        text.push_str(&serde_json::to_string_pretty(s).unwrap_or_default());
                    }
                    if text.trim().is_empty() {
                        text = serde_json::to_string_pretty(&r.raw).unwrap_or_default();
                    }
                    (format!("tools/call {target}"), text, r.is_error)
                }
                Err(e) => (format!("tools/call {target}"), e.to_string(), true),
            },
            McpCallKind::Resource => match client.read_resource(&target).await {
                Ok(v) => {
                    raw = v.clone();
                    (format!("resources/read {target}"), serde_json::to_string_pretty(&v).unwrap_or_default(), false)
                }
                Err(e) => (format!("resources/read {target}"), e.to_string(), true),
            },
            McpCallKind::Prompt => {
                let map = args.as_object().cloned().unwrap_or_default();
                match client.get_prompt(&target, map).await {
                    Ok(v) => {
                        raw = v.clone();
                        (format!("prompts/get {target}"), serde_json::to_string_pretty(&v).unwrap_or_default(), false)
                    }
                    Err(e) => (format!("prompts/get {target}"), e.to_string(), true),
                }
            }
        };
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let _ = tx.send(Msg::Mcp { tab: tab_id, epoch: 0, event: McpMsg::Result { title, text, is_error, raw, ms } });
        ctx.request_repaint();
    });
}

/// One entry of the tools/resources/prompts list.
struct Entry {
    kind: McpCallKind,
    /// Tool or prompt name, or resource URI (template).
    target: String,
    name: String,
    detail: Option<String>,
    tag: Option<String>,
}

fn entries(c: &Catalog, view: View) -> Vec<Entry> {
    match view {
        View::Tools => c
            .tools
            .iter()
            .map(|t| Entry {
                kind: McpCallKind::Tool,
                target: t.name.clone(),
                name: t.display_name().to_string(),
                detail: t.description.clone(),
                tag: t.title.as_ref().map(|_| t.name.clone()),
            })
            .collect(),
        View::Resources => c
            .resources
            .iter()
            .map(|r| Entry {
                kind: McpCallKind::Resource,
                target: r.uri.clone(),
                name: r.title.clone().unwrap_or_else(|| r.name.clone()),
                detail: Some(r.description.clone().unwrap_or_else(|| r.uri.clone())),
                tag: r.mime_type.clone(),
            })
            .chain(c.templates.iter().map(|t| Entry {
                kind: McpCallKind::Resource,
                target: t.uri_template.clone(),
                name: t.title.clone().unwrap_or_else(|| t.name.clone()),
                detail: Some(t.description.clone().unwrap_or_else(|| t.uri_template.clone())),
                tag: Some(tr("template").to_string()),
            }))
            .collect(),
        View::Prompts => c
            .prompts
            .iter()
            .map(|p| Entry {
                kind: McpCallKind::Prompt,
                target: p.name.clone(),
                name: p.title.clone().unwrap_or_else(|| p.name.clone()),
                detail: p.description.clone(),
                tag: (!p.arguments.is_empty()).then(|| trf("{} args", &[&p.arguments.len()])),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A card of the catalog list: name, a tag and two lines of description.
fn entry_card(ui: &mut Ui, e: &Entry, selected: bool) -> egui::Response {
    let p = theme::pal(ui);
    let w = ui.available_width();
    let painter = ui.painter().clone();
    let name = painter.layout_no_wrap(e.name.clone(), theme::medium(13.5), p.text);
    let detail = e.detail.as_ref().map(|d| {
        let mut job = egui::text::LayoutJob::single_section(
            d.replace('\n', " "),
            egui::TextFormat::simple(egui::FontId::proportional(12.0), p.text_muted),
        );
        job.wrap = egui::text::TextWrapping {
            max_width: w - 24.0,
            max_rows: 2,
            break_anywhere: false,
            overflow_character: Some('…'),
        };
        painter.layout_job(job)
    });
    let h = 16.0 + name.size().y + detail.as_ref().map_or(0.0, |d| d.size().y + 3.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
    widgets::a11y_button(&resp, &e.name);
    if ui.is_rect_visible(rect) {
        let (fill, stroke) = if selected {
            (p.accent_soft, egui::Stroke::new(1.0, theme::soft(p.accent, 0.45)))
        } else if resp.hovered() {
            (p.hover, egui::Stroke::new(1.0, p.border))
        } else {
            (p.surface, egui::Stroke::new(1.0, p.border))
        };
        painter.rect(rect, egui::CornerRadius::same(8), fill, stroke, egui::StrokeKind::Inside);
        let mut y = rect.top() + 8.0;
        painter.galley(egui::pos2(rect.left() + 12.0, y), name.clone(), p.text);
        if let Some(tag) = &e.tag {
            let g = painter.layout_no_wrap(tag.clone(), theme::mono(10.5), p.text_muted);
            let x = rect.left() + 12.0 + name.size().x + 8.0;
            if x + g.size().x < rect.right() - 8.0 {
                let pill = egui::Rect::from_min_size(
                    egui::pos2(x, y + name.size().y / 2.0 - 8.0),
                    egui::vec2(g.size().x + 10.0, 16.0),
                );
                painter.rect_filled(pill, egui::CornerRadius::same(8), if selected { p.surface } else { p.subtle });
                painter.galley(pill.center() - g.size() / 2.0, g, p.text_muted);
            }
        }
        y += name.size().y + 3.0;
        if let Some(d) = detail {
            painter.galley(egui::pos2(rect.left() + 12.0, y), d, p.text_muted);
        }
    }
    resp
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut McpTab) {
    let Some(original) = tab.slot.load(app) else {
        ui.label(tr("This request no longer exists."));
        return;
    };
    let mut item = original.clone();
    let collection = tab.slot.collection_id();
    let vars = app.var_ctx(collection.as_deref());
    // Where an inherited auth comes from (applied when connecting).
    let inherited = collection.as_deref().and_then(|c| app.ws.data.collection(c)).and_then(|c| {
        let chain = c.ancestors(&item.id)?;
        merpati_core::model::effective_from_chain(&chain).map(|e| (e.owner_name.to_string(), e.auth.kind))
    });
    let state = tab.state();
    let since = tab.since;
    let server = tab.catalog.as_ref().map(|c| {
        (
            format!("{} {}", c.info.title.clone().unwrap_or(c.info.name.clone()), c.info.version),
            c.info.protocol_version.clone(),
        )
    });
    header(ui, app, tab_id, &tab.slot, &mut item, |ui| {
        pc::conn_status(ui, state, since);
        if let Some((name, protocol)) = &server {
            kit::pill(ui, name, theme::pal(ui).text_muted).on_hover_text(trf("protocol {}", &[protocol]));
        }
    });
    let item_name = item.name.clone();
    let RequestKind::Mcp(req) = &mut item.request else { return };
    let mut action = None;
    let mut call: Option<(McpCallKind, String, serde_json::Value)> = None;
    let clicked = pc::connect_bar(
        ui,
        "MCP",
        126.0,
        |ui| {
            let is_http = matches!(req.transport, McpTransport::Http { .. });
            ui.scope(|ui| {
                pc::frameless(ui);
                let r = egui::ComboBox::from_id_salt("mcp-transport")
                    .icon(crate::kit::combo_caret)
                    .width(64.0)
                    .selected_text(RichText::new(if is_http { "HTTP" } else { "STDIO" }).font(theme::medium(12.5)))
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(is_http, tr("HTTP (remote)")).clicked() && !is_http {
                            req.transport = McpTransport::Http { url: String::new(), headers: Vec::new() };
                        }
                        if ui.selectable_label(!is_http, tr("STDIO (local command)")).clicked() && is_http {
                            req.transport = McpTransport::Stdio {
                                command: String::new(),
                                args: Vec::new(),
                                env: Vec::new(),
                                cwd: String::new(),
                            };
                        }
                    })
                    .response
                    .on_hover_text(tr("Transport"));
                widgets::a11y_name(&r, tr("Transport"));
            });
            kit::vdivider(ui, 22.0);
            let w = ui.available_width();
            let r = match &mut req.transport {
                McpTransport::Http { url, .. } => {
                    widgets::var_cell(ui, "mcp-url", url, &vars, "https://example.com/mcp", w)
                }
                McpTransport::Stdio { command, args, .. } => {
                    let mut line =
                        std::iter::once(command.clone()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
                    let r = widgets::var_cell(
                        ui,
                        "mcp-cmd",
                        &mut line,
                        &vars,
                        "npx -y @modelcontextprotocol/server-everything",
                        w,
                    );
                    if r.changed() {
                        let mut parts = shell_split(&line).into_iter();
                        *command = parts.next().unwrap_or_default();
                        *args = parts.collect();
                    }
                    r
                }
            };
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && state != Conn::Live {
                action = Some(ConnAction::Connect);
            }
            r.has_focus()
        },
        |ui| pc::connect_button(ui, state),
    );
    if clicked.is_some() {
        action = clicked;
    }
    if let (Some(e), Conn::Failed) = (&tab.error, state) {
        ui.add_space(8.0);
        let mut retry = false;
        let mut dismiss = false;
        pc::error_banner(ui, tr("Could not connect to the server"), e, |ui| {
            dismiss = kit::icon_button(ui, icons::X, tr("Dismiss")).clicked();
            retry = kit::secondary(ui, Some(icons::ARROW_CLOCKWISE), tr("Retry")).clicked();
        });
        if retry {
            action = Some(ConnAction::Connect);
        }
        if dismiss {
            tab.error = None;
        }
    }
    let counts = tab
        .catalog
        .as_ref()
        .map(|c| (c.tools.len(), c.resources.len() + c.templates.len(), c.prompts.len()))
        .unwrap_or_default();
    let items = vec![
        (View::Tools, trf("Tools{}", &[&widgets::count_label(counts.0)])),
        (View::Resources, trf("Resources{}", &[&widgets::count_label(counts.1)])),
        (View::Prompts, trf("Prompts{}", &[&widgets::count_label(counts.2)])),
        (View::Saved, trf("Saved calls{}", &[&widgets::count_label(req.saved_calls.len())])),
        (View::Traffic, trf("Traffic{}", &[&widgets::count_label(tab.log.lines.len())])),
        (View::Scripts, pc::scripts_label(&item.scripts)),
        (View::Connection, tr("Connection").to_string()),
    ];
    ui.add_space(8.0);
    pc::section_tabs(ui, "mcp-tabs", &mut tab.view, &items);
    ui.add_space(8.0);
    pending_ui(app, ui, tab_id, tab);
    let can_call = tab.client.is_some() && !tab.busy;
    let mut call_now = can_call && tab.selected.is_some() && pc::send_shortcut(ui);
    match tab.view {
        View::Traffic => tab.log.ui(ui, ("mcp-log", tab_id)),
        View::Scripts => {
            crate::proto_common::scripts_ui(
                ui,
                &mut item.scripts,
                &[(ScriptEvent::PreRequest, "Before call"), (ScriptEvent::PostResponse, "After result")],
                &mut tab.scripts,
                &vars,
            );
        }
        View::Connection => {
            egui::ScrollArea::vertical().id_salt("mcp-conn").show(ui, |ui| {
                let p = theme::pal(ui);
                if let Some(c) = &tab.catalog {
                    kit::card(ui).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icons::PUZZLE_PIECE).font(theme::icon_font(18.0)).color(p.accent_text));
                            ui.label(
                                RichText::new(c.info.title.clone().unwrap_or(c.info.name.clone()))
                                    .font(theme::semibold(14.0)),
                            );
                            kit::pill(ui, &c.info.version, p.text_muted);
                            kit::pill(ui, &trf("protocol {}", &[&c.info.protocol_version]), p.info);
                        });
                        ui.horizontal_wrapped(|ui| {
                            for cap in ["tools", "resources", "prompts", "logging", "completions"] {
                                if c.info.has_capability(cap) {
                                    kit::pill(ui, &format!("{} {cap}", icons::CHECK), p.ok);
                                }
                            }
                        });
                        if let Some(i) = &c.info.instructions {
                            ui.add_space(4.0);
                            ui.label(RichText::new(i).color(p.text_muted));
                        }
                    });
                    ui.add_space(14.0);
                }
                match &mut req.transport {
                    McpTransport::Http { headers, .. } => {
                        pc::form_grid("mcp-http").show(ui, |ui| {
                            use merpati_core::protocols::McpHttpTransport as T;
                            pc::form_label(ui, tr("Transport"));
                            ui.push_id("http-transport", |ui| {
                                kit::segmented(
                                    ui,
                                    &mut req.http_transport,
                                    &[(T::Auto, tr("Auto")), (T::Streamable, "Streamable HTTP"), (T::Sse, tr("HTTP+SSE (legacy)"))],
                                )
                            });
                            ui.end_row();
                        });
                        ui.label(kit::hint(ui, tr("Auto: Streamable HTTP, falling back to HTTP+SSE")));
                        ui.add_space(4.0);
                        ui.label(kit::hint(ui, tr("Servers that require OAuth open your browser to sign in; tokens are kept in the Keychain. Set the auth type to OAuth 2.0 to use a pre-registered client.")));
                        ui.add_space(12.0);
                        let mut h = headers.clone();
                        let opts = KvOptions { title: Some(tr("Headers")), allow_add: true, key_hint: tr("Key"), value_hint: tr("Value"), ..Default::default() };
                        if widgets::kv_table(ui, "mcp-headers", &mut h, &vars, opts) {
                            *headers = h;
                        }
                        ui.label(kit::hint(ui, tr("A Bearer token in Authorization is sent as the Authorization header.")));
                    }
                    McpTransport::Stdio { env, cwd, .. } => {
                        pc::form_grid("mcp-stdio").show(ui, |ui| {
                            pc::form_label(ui, tr("Working directory"));
                            ui.add(
                                egui::TextEdit::singleline(cwd)
                                    .hint_text(tr("the workspace folder"))
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(360.0),
                            );
                            ui.end_row();
                        });
                        ui.add_space(12.0);
                        let mut e: Vec<KeyValue> = env.clone();
                        let opts = KvOptions { title: Some(tr("Environment")), allow_add: true, key_hint: tr("Key"), value_hint: tr("Value"), ..Default::default() };
                        if widgets::kv_table(ui, "mcp-env", &mut e, &vars, opts) {
                            *env = e;
                        }
                    }
                }
                ui.add_space(14.0);
                pc::section(ui, tr("Authorization"), None);
                crate::auth_ui::auth_editor(
                    ui,
                    &mut req.auth,
                    crate::auth_ui::AuthUiCtx {
                        is_root: false,
                        inherited: inherited.clone(),
                        owner_id: &item.id,
                        token: None,
                        fetching: false,
                        vars: &vars,
                    },
                );
                ui.add_space(14.0);
                pc::section(ui, tr("Export config"), Some(tr("Use this server in other MCP hosts.")));
                ui.horizontal(|ui| {
                    let r = egui::ComboBox::from_id_salt("mcp-export")
                        .icon(crate::kit::combo_caret)
                        .width(240.0)
                        .selected_text(tab.export_style.label())
                        .show_ui(ui, |ui| {
                            for s in ExportStyle::ALL {
                                ui.selectable_value(&mut tab.export_style, s, s.label());
                            }
                        });
                    widgets::a11y_name(&r.response, tr("Export config"));
                    if kit::secondary(ui, Some(icons::COPY), tr("Copy config")).clicked() {
                        let v = merpati_mcp::export_config(&item_name, req, tab.export_style);
                        ui.ctx().copy_text(serde_json::to_string_pretty(&v).unwrap_or_default());
                        app.toast(tr("Copied"), false);
                    }
                });
            });
        }
        view => {
            let listing = view != View::Saved;
            if listing && tab.catalog.is_none() {
                let (title, body) = if state == Conn::Busy {
                    (tr("Connecting…"), tr("Starting the server and listing what it offers."))
                } else {
                    (tr("Not connected"), tr("Connect to list the server's tools, resources and prompts."))
                };
                kit::empty_state(ui, icons::PLUGS, title, body, |ui| {
                    if state == Conn::Busy {
                        ui.add(egui::Spinner::new().size(18.0).color(theme::pal(ui).accent));
                    }
                });
            } else {
                let list_w = (ui.available_width() * 0.36).clamp(220.0, 340.0);
                pc::list_panel(ui, ("mcp-list-panel", tab_id), list_w, |ui| {
                    if listing {
                        let c = tab.catalog.as_ref().expect("catalog");
                        let list = entries(c, view);
                        if list.is_empty() {
                            let what = match view {
                                View::Tools => tr("This server has no tools."),
                                View::Resources => tr("This server has no resources."),
                                _ => tr("This server has no prompts."),
                            };
                            ui.add_space(16.0);
                            ui.label(RichText::new(what).color(theme::pal(ui).text_muted));
                            return;
                        }
                        kit::search_field(ui, "mcp-search", &mut tab.search, tr("Filter"), ui.available_width());
                        ui.add_space(6.0);
                        let q = tab.search.to_lowercase();
                        egui::ScrollArea::vertical().id_salt(("mcp-list", view == View::Tools)).show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 6.0;
                            for e in list.iter().filter(|e| {
                                q.is_empty()
                                    || e.name.to_lowercase().contains(&q)
                                    || e.target.to_lowercase().contains(&q)
                                    || e.detail.as_ref().is_some_and(|d| d.to_lowercase().contains(&q))
                            }) {
                                let sel = tab.selected.as_ref() == Some(&(e.kind, e.target.clone()));
                                if entry_card(ui, e, sel).clicked() && !sel {
                                    tab.selected = Some((e.kind, e.target.clone()));
                                    tab.args = default_args(c, e.kind, &e.target);
                                    tab.result = None;
                                }
                            }
                        });
                    } else {
                        saved_list(ui, req, tab);
                    }
                });
                call_form(app, ui, tab, req, &vars, collection.as_deref(), &mut call, &mut call_now);
            }
        }
    }
    if let Some(a) = tab.auto.clone() {
        if a == "connect" {
            if tab.client.is_none() && !tab.connecting {
                action = Some(ConnAction::Connect);
            }
            tab.auto = None;
        } else if let Some(rest) = a.strip_prefix("call:")
            && tab.catalog.is_some()
            && !tab.busy
        {
            let (name, args) = rest.split_once(':').unwrap_or((rest, "{}"));
            tab.view = View::Tools;
            tab.selected = Some((McpCallKind::Tool, name.to_string()));
            tab.args = args.to_string();
            call = Some((McpCallKind::Tool, name.to_string(), serde_json::from_str(args).unwrap_or_default()));
            tab.auto = None;
        }
    }
    let req_clone = req.clone();
    let item_now = item.clone();
    tab.slot.store(app, &original, item);
    match action {
        Some(ConnAction::Connect) => connect(app, tab_id, tab, &item_now.id, &req_clone, collection.as_deref()),
        Some(ConnAction::Cancel) => {
            if let Some(l) = &tab.login {
                l.cancel();
            }
        }
        Some(ConnAction::Disconnect) => {
            if let Some(c) = tab.client.take() {
                app.rt.spawn(async move { c.close().await });
                tab.catalog = None;
                tab.since = None;
                tab.drop_pending();
                tab.log.system(tr("Disconnected"));
            }
        }
        None => {}
    }
    let saved = |kind: McpCallKind, target: &str, args: &serde_json::Value| merpati_core::protocols::McpSavedCall {
        id: String::new(),
        name: target.to_string(),
        kind,
        target: target.to_string(),
        arguments: args.to_string(),
    };
    if let (Some((kind, target, args)), Some(client)) = (call, tab.client.clone()) {
        tab.busy = true;
        tab.scripts.clear_results();
        let ctx = proto::mcp(&req_clone, Some(&saved(kind, &target, &args)));
        tab.pending_call = Some((kind, target.clone(), args.clone()));
        if crate::proto_common::run_scripts(app, tab_id, &tab.slot, &item_now, ScriptEvent::PreRequest, ctx) {
            tab.scripts.waiting = true;
        } else {
            run_call(app, tab_id, client, kind, target, args);
        }
    }
    if let Some(pre) = tab.scripts.take_pre() {
        match (pre, tab.pending_call.clone(), tab.client.clone()) {
            (Some(v), Some((kind, target, args)), Some(client)) => {
                let mut r = req_clone.clone();
                let mut c = saved(kind, &target, &args);
                proto::apply_mcp(&mut r, Some(&mut c), &v);
                let args = serde_json::from_str(&c.arguments).unwrap_or(args);
                tab.pending_call = Some((kind, c.target.clone(), args.clone()));
                run_call(app, tab_id, client, kind, c.target, args);
            }
            _ => {
                tab.busy = false;
                tab.pending_call = None;
                tab.log.system(tr("Skipped by pm.execution.skipRequest()"));
            }
        }
    }
    if let Some((kind, target, args, resp)) = tab.post.take() {
        let ctx = proto::mcp(&req_clone, Some(&saved(kind, &target, &args))).with_response(resp);
        crate::proto_common::run_scripts(app, tab_id, &tab.slot, &item_now, ScriptEvent::PostResponse, ctx);
    }
}

/// The arguments template of a tool or prompt.
fn default_args(c: &Catalog, kind: McpCallKind, target: &str) -> String {
    match kind {
        McpCallKind::Tool => c
            .tools
            .iter()
            .find(|t| t.name == target)
            .and_then(|t| serde_json::to_string_pretty(&merpati_mcp::args_template(&t.input_schema)).ok())
            .unwrap_or_else(|| "{}".into()),
        McpCallKind::Prompt => {
            let args: serde_json::Map<String, serde_json::Value> = c
                .prompts
                .iter()
                .find(|p| p.name == target)
                .map(|p| {
                    p.arguments.iter().map(|a| (a.name.clone(), serde_json::Value::String(String::new()))).collect()
                })
                .unwrap_or_default();
            serde_json::to_string_pretty(&serde_json::Value::Object(args)).unwrap_or_else(|_| "{}".into())
        }
        McpCallKind::Resource => "{}".into(),
    }
}

/// The call form for the selected tool, resource or prompt, and its result.
#[allow(clippy::too_many_arguments)]
fn call_form(
    app: &mut MerpatiApp,
    ui: &mut Ui,
    tab: &mut McpTab,
    req: &mut merpati_core::protocols::McpRequest,
    vars: &widgets::VarCtx,
    collection: Option<&str>,
    call: &mut Option<(McpCallKind, String, serde_json::Value)>,
    call_now: &mut bool,
) {
    let p = theme::pal(ui);
    let Some((kind, target)) = tab.selected.clone() else {
        let body = if tab.view == View::Saved {
            tr("Pick a saved call to load it.")
        } else {
            tr("Pick an item on the left to see it and call it.")
        };
        kit::empty_state(ui, icons::ARROW_LEFT, tr("Nothing selected"), body, |_| {});
        return;
    };
    egui::ScrollArea::vertical().id_salt("mcp-call").auto_shrink([false; 2]).show(ui, |ui| {
        let (icon, verb) = match kind {
            McpCallKind::Tool => (icons::PUZZLE_PIECE, tr("Call tool")),
            McpCallKind::Resource => (icons::FILE_TEXT, tr("Read")),
            McpCallKind::Prompt => (icons::CHAT_CIRCLE, tr("Get prompt")),
        };
        let mut description = None;
        let mut schema = None;
        let mut title = target.clone();
        if let Some(c) = &tab.catalog {
            match kind {
                McpCallKind::Tool => {
                    if let Some(t) = c.tools.iter().find(|t| t.name == target) {
                        description = t.description.clone();
                        schema = Some(t.input_schema.clone());
                        title = t.display_name().to_string();
                    }
                }
                McpCallKind::Prompt => {
                    if let Some(pr) = c.prompts.iter().find(|pr| pr.name == target) {
                        description = pr.description.clone();
                        title = pr.title.clone().unwrap_or(pr.name.clone());
                    }
                }
                McpCallKind::Resource => {
                    if let Some(r) = c.resources.iter().find(|r| r.uri == target) {
                        description = r.description.clone();
                        title = r.title.clone().unwrap_or(r.name.clone());
                    }
                }
            }
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon).font(theme::icon_font(18.0)).color(p.accent_text));
            ui.label(RichText::new(&title).font(theme::semibold(15.0)));
            if title != target {
                kit::pill(ui, &target, p.text_muted);
            }
        });
        if let Some(d) = description {
            ui.add_space(2.0);
            ui.add(egui::Label::new(RichText::new(d).color(p.text_muted)).wrap());
        }
        ui.add_space(10.0);
        let mut target_edit = target.clone();
        if kind == McpCallKind::Resource {
            pc::section(ui, "URI", None);
            ui.add(
                egui::TextEdit::singleline(&mut target_edit)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
            if target_edit != target {
                tab.selected = Some((kind, target_edit.clone()));
            }
        } else {
            ui.horizontal(|ui| {
                ui.label(kit::section_title(tr("Arguments")));
                ui.label(kit::hint(ui, tr("JSON object")));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(c) = &tab.catalog
                        && kit::ghost(ui, Some(icons::ARROW_COUNTER_CLOCKWISE), tr("Reset"))
                            .on_hover_text(tr("Fill with a template of the arguments"))
                            .clicked()
                    {
                        tab.args = default_args(c, kind, &target);
                    }
                });
            });
            ui.add_space(2.0);
            widgets::code_editor(ui, "mcp-args", &mut tab.args, Lang::Json, Some(vars), 6, false);
            if let Some(s) = schema {
                ui.add_space(6.0);
                pc::collapsible(ui, "mcp-input-schema", tr("Input schema"), None, |ui| {
                    crate::proto_common::text_view(
                        ui,
                        "mcp-schema-text",
                        &serde_json::to_string_pretty(&s).unwrap_or_default(),
                    );
                });
            }
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let ready = tab.client.is_some() && !tab.busy;
            let hint = if tab.client.is_none() { tr("Connect first") } else { tr("A call is running") };
            if pc::send_button(ui, icons::PLAY, verb, ready, hint) {
                *call_now = true;
            }
            if kit::ghost(ui, Some(icons::BOOKMARK_SIMPLE), tr("Save call")).clicked() {
                req.saved_calls.push(McpSavedCall {
                    id: merpati_core::util::new_id(),
                    name: target.clone(),
                    kind,
                    target: target_edit.clone(),
                    arguments: tab.args.clone(),
                });
            }
        });
        if *call_now {
            let r = resolver(app, collection);
            match serde_json::from_str::<serde_json::Value>(&r(&tab.args)) {
                Ok(v) => *call = Some((kind, target_edit.clone(), v)),
                Err(e) => {
                    tab.result = Some(CallResult {
                        title: tr("Invalid arguments").into(),
                        text: e.to_string(),
                        is_error: true,
                        ms: None,
                    })
                }
            }
        }
        ui.add_space(12.0);
        if tab.busy {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0).color(p.accent));
                ui.label(RichText::new(tr("Waiting for the server…")).color(p.text_muted));
            });
        } else if let Some(res) = &tab.result {
            ui.horizontal(|ui| {
                ui.label(kit::section_title(tr("Result")));
                if res.is_error {
                    kit::pill(ui, &format!("{} {}", icons::WARNING_CIRCLE, tr("Error")), p.err);
                } else {
                    kit::pill(ui, &format!("{} {}", icons::CHECK, tr("Success")), p.ok);
                }
                if let Some(ms) = res.ms {
                    kit::metric(ui, icons::TIMER, &merpati_core::util::human_ms(ms));
                }
                ui.add(egui::Label::new(RichText::new(&res.title).monospace().small().color(p.text_muted)).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if kit::icon_button(ui, icons::COPY, tr("Copy result")).clicked() {
                        ui.ctx().copy_text(res.text.clone());
                    }
                });
            });
            ui.add_space(4.0);
            crate::proto_common::text_view(ui, "mcp-result-text", &res.text);
        }
    });
}

fn saved_list(ui: &mut Ui, req: &mut merpati_core::protocols::McpRequest, tab: &mut McpTab) {
    let p = theme::pal(ui);
    if req.saved_calls.is_empty() {
        ui.add_space(16.0);
        ui.label(RichText::new(tr("Use “Save call” to keep a tool call with its arguments.")).color(p.text_muted));
        return;
    }
    let mut remove = None;
    egui::ScrollArea::vertical().id_salt("mcp-saved").show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        for (i, c) in req.saved_calls.iter().enumerate() {
            let kind = match c.kind {
                McpCallKind::Tool => tr("tool"),
                McpCallKind::Prompt => tr("prompt"),
                McpCallKind::Resource => tr("resource"),
            };
            let e = Entry {
                kind: c.kind,
                target: c.target.clone(),
                name: c.name.clone(),
                detail: (!c.arguments.trim().is_empty() && c.arguments.trim() != "{}")
                    .then(|| c.arguments.replace('\n', " ")),
                tag: Some(kind.to_string()),
            };
            let sel = tab.selected.as_ref() == Some(&(c.kind, c.target.clone()));
            let r = entry_card(ui, &e, sel);
            if r.clicked() {
                tab.selected = Some((c.kind, c.target.clone()));
                tab.args = if c.arguments.is_empty() { "{}".into() } else { c.arguments.clone() };
                tab.result = None;
            }
            r.context_menu(|ui| {
                if ui.button(kit::with_icon(icons::TRASH, tr("Remove"))).clicked() {
                    remove = Some(i);
                    ui.close();
                }
            });
        }
    });
    if let Some(i) = remove {
        req.saved_calls.remove(i);
    }
    ui.add_space(4.0);
    ui.label(kit::hint(ui, tr("Right-click a saved call to remove it.")));
}

/// Splits a command line on whitespace, honouring single and double quotes.
pub fn shell_split(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for ch in line.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                has = true;
            }
            None if ch.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None => cur.push(ch),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Approval panel for sampling requests and the form for elicitation.
fn pending_ui(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut McpTab) {
    let p = theme::pal(ui);
    // Automation: approve with the first saved AI request.
    let auto_approve = tab.auto.as_deref() == Some("approve") && tab.sampling.front().is_some_and(|p| !p.running);
    if auto_approve {
        tab.auto = None;
    }
    let waiting = tab.sampling.len().saturating_sub(1);
    if let Some(s) = tab.sampling.front_mut() {
        let mut decline = false;
        let mut approve = false;
        if auto_approve {
            s.model = app
                .ws
                .data
                .collections
                .iter()
                .flat_map(|c| c.requests())
                .find(|r| matches!(r.request, RequestKind::Ai(_)))
                .map(|r| r.id.clone());
            approve = s.model.is_some();
        }
        let title = format!("{} {}", s.request.server, tr("asks to use your AI model"));
        pc::notice_card(ui, icons::ROBOT, &title, |ui| {
            if let Some(sys) = &s.request.system_prompt {
                ui.label(
                    RichText::new(format!("{}: {}", tr("System"), crate::dialogs::ellipsize(sys, 300)))
                        .color(p.text_muted),
                );
            }
            for m in &s.request.messages {
                let text: Vec<String> = m
                    .content
                    .iter()
                    .map(|c| match c {
                        merpati_mcp::Content::Text { text } => text.clone(),
                        _ => format!("[{}]", tr("non-text content")),
                    })
                    .collect();
                ui.horizontal_wrapped(|ui| {
                    kit::pill(ui, &m.role, p.text_muted);
                    ui.label(RichText::new(crate::dialogs::ellipsize(&text.join(" "), 500)).monospace());
                });
            }
            if !s.request.model_hints.is_empty() {
                ui.label(kit::hint(ui, &format!("{}: {}", tr("Model hints"), s.request.model_hints.join(", "))));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr("Answer with")).color(p.text_muted));
                let ai: Vec<(String, String)> = app
                    .ws
                    .data
                    .collections
                    .iter()
                    .flat_map(|c| {
                        c.requests()
                            .into_iter()
                            .filter(|r| matches!(r.request, RequestKind::Ai(_)))
                            .map(move |r| (r.id.clone(), format!("{} / {}", c.name, r.name)))
                    })
                    .collect();
                let current = s
                    .model
                    .as_ref()
                    .and_then(|id| ai.iter().find(|(i, _)| i == id))
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| tr("choose an AI request").into());
                let r = egui::ComboBox::from_id_salt(("mcp-sampling-model", tab_id))
                    .icon(crate::kit::combo_caret)
                    .width(220.0)
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for (id, name) in &ai {
                            if ui.selectable_label(s.model.as_ref() == Some(id), name).clicked() {
                                s.model = Some(id.clone());
                            }
                        }
                    });
                widgets::a11y_name(&r.response, tr("Model"));
                if ai.is_empty() {
                    ui.label(kit::hint(ui, tr("Save an AI request first")));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if s.running {
                        ui.add(egui::Spinner::new().size(16.0).color(p.accent));
                        ui.label(RichText::new(tr("Answering…")).color(p.text_muted));
                    } else {
                        let ok = ui
                            .add_enabled_ui(s.model.is_some(), |ui| kit::primary_icon(ui, icons::CHECK, tr("Approve")));
                        if ok.inner.clicked() {
                            approve = true;
                        }
                        if kit::secondary(ui, Some(icons::X), tr("Decline")).clicked() {
                            decline = true;
                        }
                    }
                });
            });
            if waiting > 0 {
                ui.label(kit::hint(ui, &trf("{} more requests waiting", &[&waiting])));
            }
        });
        ui.add_space(8.0);
        if decline {
            tab.decline_sampling();
        } else if approve {
            answer_sampling(app, tab_id, tab);
        }
    }
    let waiting = tab.elicit.len().saturating_sub(1);
    if let Some(e) = tab.elicit.front_mut() {
        let mut outcome: Option<merpati_mcp::ElicitationResponse> = None;
        let title = format!("{} {}", e.request.server, tr("asks for input"));
        pc::notice_card(ui, icons::QUESTION, &title, |ui| {
            ui.label(&e.request.message);
            if let Some(url) = &e.request.url
                && ui.link(url).clicked()
            {
                crate::widgets::open_web_link(url);
            }
            ui.add_space(6.0);
            pc::form_grid(("mcp-elicit", tab_id)).show(ui, |ui| {
                for f in e.request.fields() {
                    let label = format!("{}{}", f.label(), if f.required { " *" } else { "" });
                    ui.add(egui::Label::new(RichText::new(label).color(p.text_muted)))
                        .on_hover_text(f.description.clone().unwrap_or_default());
                    let v = e.values.entry(f.name.clone()).or_insert(serde_json::Value::Null);
                    match &f.kind {
                        merpati_mcp::ElicitFieldKind::Boolean => {
                            let mut b = v.as_bool().unwrap_or(false);
                            if ui.checkbox(&mut b, "").changed() {
                                *v = serde_json::Value::Bool(b);
                            }
                        }
                        merpati_mcp::ElicitFieldKind::Number { integer, .. } => {
                            let mut text = if v.is_null() { String::new() } else { v.to_string() };
                            if ui.add(egui::TextEdit::singleline(&mut text).desired_width(160.0)).changed() {
                                *v = if *integer {
                                    text.trim()
                                        .parse::<i64>()
                                        .map(serde_json::Value::from)
                                        .unwrap_or(serde_json::Value::Null)
                                } else {
                                    text.trim()
                                        .parse::<f64>()
                                        .ok()
                                        .and_then(serde_json::Number::from_f64)
                                        .map(serde_json::Value::Number)
                                        .unwrap_or(serde_json::Value::Null)
                                };
                            }
                        }
                        merpati_mcp::ElicitFieldKind::Choice { values, labels } => {
                            let current = v.as_str().unwrap_or("").to_string();
                            let r = egui::ComboBox::from_id_salt(("elicit", &f.name))
                                .icon(crate::kit::combo_caret)
                                .width(200.0)
                                .selected_text(&current)
                                .show_ui(ui, |ui| {
                                    for (i, val) in values.iter().enumerate() {
                                        let label = labels.get(i).unwrap_or(val);
                                        if ui.selectable_label(&current == val, label).clicked() {
                                            *v = serde_json::Value::String(val.clone());
                                        }
                                    }
                                });
                            widgets::a11y_name(&r.response, f.label());
                        }
                        merpati_mcp::ElicitFieldKind::MultiChoice { values, labels, .. } => {
                            ui.horizontal_wrapped(|ui| {
                                let mut chosen: Vec<String> = v
                                    .as_array()
                                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                                    .unwrap_or_default();
                                for (i, val) in values.iter().enumerate() {
                                    let mut on = chosen.contains(val);
                                    if ui.checkbox(&mut on, labels.get(i).unwrap_or(val)).changed() {
                                        if on {
                                            chosen.push(val.clone())
                                        } else {
                                            chosen.retain(|c| c != val)
                                        }
                                        *v = serde_json::json!(chosen);
                                    }
                                }
                            });
                        }
                        _ => {
                            let mut text = v.as_str().unwrap_or("").to_string();
                            if ui.add(egui::TextEdit::singleline(&mut text).desired_width(280.0)).changed() {
                                *v = serde_json::Value::String(text);
                            }
                        }
                    }
                    ui.end_row();
                }
            });
            for problem in &e.problems {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::WARNING_CIRCLE).font(theme::icon_font(14.0)).color(p.err));
                    ui.label(RichText::new(problem).color(p.err));
                });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if kit::primary_icon(ui, icons::CHECK, tr("Submit")).clicked() {
                    let content: serde_json::Map<String, serde_json::Value> =
                        e.values.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), v.clone())).collect();
                    e.problems = e.request.validate(&content);
                    if e.problems.is_empty() {
                        outcome = Some(merpati_mcp::ElicitationResponse::Accept(content));
                    }
                }
                if kit::secondary(ui, None, tr("Decline")).clicked() {
                    outcome = Some(merpati_mcp::ElicitationResponse::Decline);
                }
                if kit::ghost(ui, None, tr("Cancel")).clicked() {
                    outcome = Some(merpati_mcp::ElicitationResponse::Cancel);
                }
            });
            if waiting > 0 {
                ui.label(kit::hint(ui, &trf("{} more requests waiting", &[&waiting])));
            }
        });
        ui.add_space(8.0);
        if let Some(o) = outcome {
            tab.answer_elicit(o);
        }
    }
}

/// Runs the approved sampling request through the chosen AI request.
fn answer_sampling(app: &mut MerpatiApp, tab_id: u64, tab: &mut McpTab) {
    let Some(p) = tab.sampling.front_mut() else { return };
    let Some(item) = p.model.as_ref().and_then(|id| {
        app.ws.data.collections.iter().find_map(|c| c.find_request(id).map(|i| (c.id.clone(), i.clone())))
    }) else {
        return;
    };
    let (cid, item) = item;
    let RequestKind::Ai(mut ai) = item.request else { return };
    ai.system = p.request.system_prompt.clone().unwrap_or_default();
    ai.messages = p
        .request
        .messages
        .iter()
        .map(|m| merpati_core::protocols::AiMessage {
            role: m.role.clone(),
            content: m
                .content
                .iter()
                .filter_map(|c| if let merpati_mcp::Content::Text { text } = c { Some(text.clone()) } else { None })
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .collect();
    ai.max_tokens = Some(p.request.max_tokens.min(u64::from(u32::MAX)) as u32);
    if let Some(t) = p.request.temperature {
        ai.temperature = Some(t as f32);
    }
    ai.mcp_servers.clear();
    let resolve = resolver(app, Some(&cid));
    for key in ["token", "value"] {
        let v = ai.auth.get_string(key);
        if !v.is_empty() {
            ai.auth.set(key, resolve(&v));
        }
    }
    let cfg = match merpati_ai::AiConfig::from_request(&ai, &resolve) {
        Ok(c) => c,
        Err(e) => {
            tab.log.error(trf("Sampling failed: {}", &[&e]));
            return;
        }
    };
    let Ok(http) = app.http.default_client() else { return };
    let Some(reply) = p.reply.take() else { return };
    p.running = true;
    let id = p.id;
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    app.rt.spawn(async move {
        let r = merpati_ai::run(cfg, &http, Vec::new(), |_| {}, merpati_ai::CancellationToken::new()).await;
        let done = match r {
            Ok(res) => {
                let _ = reply.send(Ok(merpati_mcp::SamplingResult::text(&res.model, &res.text)));
                Ok(res.text)
            }
            Err(e) => {
                let _ = reply.send(Err(merpati_mcp::McpError::rejected(e.to_string())));
                Err(e.to_string())
            }
        };
        let _ = tx.send(Msg::Mcp { tab: tab_id, epoch: 0, event: McpMsg::SamplingDone(id, done) });
        ctx.request_repaint();
    });
}

/// `req` with an inherited auth replaced by the collection's or folder's.
pub fn with_inherited_auth(
    app: &MerpatiApp,
    collection: Option<&str>,
    item_id: &str,
    req: &merpati_core::protocols::McpRequest,
) -> merpati_core::protocols::McpRequest {
    let mut req = req.clone();
    if req.auth.is_inherit() {
        req.auth = collection
            .and_then(|c| app.ws.data.collection(c))
            .and_then(|c| c.effective_auth(item_id).map(|e| e.auth.clone()))
            .unwrap_or_else(merpati_core::auth::Auth::none);
    }
    req
}

/// The server shown in the traffic log while connecting: as configured,
/// with `{{variables}}` unresolved so secrets passed to it stay out of the log.
fn connecting_target(req: &merpati_core::protocols::McpRequest) -> String {
    McpConfig::from_request(req, &|s: &str| s.to_string()).describe()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An MCP request set to "Inherit auth from parent" (the default) uses
    /// the collection's auth, like HTTP requests do.
    #[test]
    fn inherited_auth_reaches_the_server() {
        use merpati_core::model::{Item, RequestItem};
        use merpati_core::protocols::{McpRequest, McpTransport};
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let cid = app.new_collection("C");
        let req = McpRequest {
            transport: McpTransport::Http { url: "https://mcp.test/mcp".into(), headers: Vec::new() },
            ..Default::default()
        };
        assert!(req.auth.is_inherit());
        let mut item = RequestItem::new("server", merpati_core::model::HttpRequest::default());
        item.request = RequestKind::Mcp(req.clone());
        let item_id = item.id.clone();
        let c = app.ws.data.collection_mut(&cid).unwrap();
        c.auth = merpati_core::auth::Auth::bearer("{{token}}");
        c.variables.push(merpati_core::variables::Variable::new("token", "t0k"));
        c.items.push(Item::Request(item));
        let r = resolver(&app, Some(&cid));
        let cfg = McpConfig::from_request(&with_inherited_auth(&app, Some(&cid), &item_id, &req), &r);
        let merpati_mcp::Transport::Http { headers, .. } = &cfg.transport else { panic!("http") };
        assert!(headers.iter().any(|(k, v)| k == "Authorization" && v == "Bearer t0k"), "{headers:?}");
    }

    /// Secrets passed to a server (`--api-key {{vault:…}}`, `?token=…`) do
    /// not appear resolved in the traffic log.
    #[test]
    fn connecting_log_keeps_secrets_out() {
        use merpati_core::protocols::{McpRequest, McpTransport};
        let stdio = McpRequest {
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["server".into(), "--api-key".into(), "{{vault:key}}".into()],
                env: Vec::new(),
                cwd: String::new(),
            },
            ..Default::default()
        };
        assert_eq!(connecting_target(&stdio), "npx server --api-key {{vault:key}}");
    }

    fn sampling(server: &str) -> Box<merpati_mcp::SamplingRequest> {
        Box::new(merpati_mcp::SamplingRequest {
            server: server.into(),
            messages: Vec::new(),
            system_prompt: None,
            max_tokens: 100,
            temperature: None,
            stop_sequences: Vec::new(),
            model_hints: Vec::new(),
            model_preferences: None,
            include_context: None,
            metadata: None,
            raw: serde_json::Value::Null,
        })
    }

    fn elicitation(message: &str) -> Box<merpati_mcp::ElicitationRequest> {
        Box::new(merpati_mcp::ElicitationRequest {
            server: "s".into(),
            message: message.into(),
            requested_schema: serde_json::json!({"type": "object", "properties": {}}),
            mode: "form".into(),
            url: None,
            elicitation_id: None,
            raw: serde_json::Value::Null,
        })
    }

    /// A second sampling or elicitation request waits behind the first
    /// instead of replacing it (which rejected the first one unseen); they
    /// are answered in order, and closing the tab rejects what still waits.
    #[test]
    fn server_requests_queue_up_and_closing_rejects_them() {
        use tokio::sync::oneshot::{self, error::TryRecvError};
        let mut tab = McpTab::new(ItemSlot::default());
        let (r1, mut w1) = oneshot::channel();
        let (r2, mut w2) = oneshot::channel();
        tab.on_msg(McpMsg::Sampling(sampling("first"), r1));
        tab.on_msg(McpMsg::Sampling(sampling("second"), r2));
        let (e1, mut v1) = oneshot::channel();
        let (e2, mut v2) = oneshot::channel();
        tab.on_msg(McpMsg::Elicit(elicitation("one"), e1));
        tab.on_msg(McpMsg::Elicit(elicitation("two"), e2));
        assert!(matches!(w1.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(tab.sampling.iter().map(|p| p.request.server.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        tab.decline_sampling();
        assert!(matches!(w1.try_recv(), Ok(Err(_))));
        assert!(matches!(w2.try_recv(), Err(TryRecvError::Empty)));
        tab.answer_elicit(merpati_mcp::ElicitationResponse::Decline);
        assert_eq!(v1.try_recv().ok(), Some(merpati_mcp::ElicitationResponse::Decline));
        assert_eq!(tab.elicit.front().map(|p| p.request.message.as_str()), Some("two"));
        // A finished answer removes its own request, not the one shown.
        tab.on_msg(McpMsg::SamplingDone(99, Ok(String::new())));
        assert_eq!(tab.sampling.len(), 1);

        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let id = app.add_tab(crate::app::TabKind::Mcp(Box::new(tab)));
        app.close_tab(app.tab_index(id).unwrap());
        assert!(matches!(w2.try_recv(), Err(TryRecvError::Closed)));
        assert!(matches!(v2.try_recv(), Err(TryRecvError::Closed)));

        // So does a connection that closes.
        let mut tab = McpTab::new(ItemSlot::default());
        let (r3, mut w3) = oneshot::channel();
        tab.on_msg(McpMsg::Sampling(sampling("third"), r3));
        tab.on_msg(McpMsg::Event(McpEvent::Closed("gone".into())));
        assert!(tab.sampling.is_empty());
        assert!(matches!(w3.try_recv(), Err(TryRecvError::Closed)));
    }

    #[test]
    fn splits_commands() {
        assert_eq!(shell_split("npx -y \"@scope/pkg name\" 'a b'"), vec!["npx", "-y", "@scope/pkg name", "a b"]);
        assert!(shell_split("   ").is_empty());
    }
}
