//! HTTP (and GraphQL) request tab: URL bar, request editor sections and the
//! response panel (PRD 5.1, 5.3, 6.1, 7.1).

use std::sync::Arc;
use std::time::Instant;

use egui::{Align2, Color32, CornerRadius, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use merpati_core::auth::{Auth, AuthType};
use merpati_core::model::{
    Body, BodyMode, Example, FormField, FormFieldKind, HttpRequest, HttpVersion, KeyValue, RawLanguage, RequestItem,
    RequestSettings,
};
use merpati_core::script::{ScriptEvent, TestResult};
use merpati_core::tokens::TokenStore;
use merpati_core::variables::Resolver;
use merpati_http::CancellationToken;
use merpati_runner::Execution;

use crate::app::{ItemRef, MerpatiApp};
use crate::body_view::BodyState;
use crate::highlight::Lang;
use crate::i18n::{tr, trf};
use crate::theme;
use crate::widgets::{self, KvOptions, VarCtx};
use crate::{icons, kit};

pub struct Running {
    pub cancel: CancellationToken,
    pub started: Instant,
}

pub struct ResultView {
    pub exec: Execution,
    pub body: BodyState,
    /// When the response arrived (shown when comparing).
    pub at: chrono::DateTime<chrono::Local>,
}

/// How much of the tab the response panel takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PanelMode {
    /// The split set by the divider ([`RequestTab::split`]).
    #[default]
    Normal,
    /// The request editor is folded to its section tabs (below) or hidden
    /// (side by side).
    Maximized,
    /// Only the response's header bar is shown.
    Minimized,
}

#[derive(Default)]
pub struct GraphQlState {
    pub schema: Option<Arc<merpati_convert::graphql::Schema>>,
    pub loading: bool,
    pub error: Option<String>,
    pub explorer_open: bool,
    pub filter: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Params,
    Auth,
    Headers,
    Body,
    Scripts,
    Settings,
    Docs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RespSection {
    Body,
    Headers,
    Cookies,
    Tests,
    Details,
    Visualize,
    /// The previous response against this one.
    Compare,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TestFilter {
    All,
    Passed,
    Failed,
    Skipped,
}

pub struct RequestTab {
    pub location: Option<ItemRef>,
    pub scratch: Option<RequestItem>,
    pub section: Section,
    pub script_event: ScriptEvent,
    pub resp_section: RespSection,
    pub running: Option<Running>,
    pub result: Option<Box<ResultView>>,
    pub graphql: GraphQlState,
    pub test_filter: TestFilter,
    renaming: Option<String>,
    /// Fraction of the tab given to the response panel.
    pub split: f32,
    /// Set by UI automation to press Send through the normal button path.
    pub click_send: bool,
    /// Put the caret in the URL field on the next frame (new request tabs).
    pub focus_url: bool,
    /// A running GraphQL subscription (graphql-transport-ws over WebSocket).
    pub gql_sub: Option<GqlSubscription>,
    /// Number of the subscription's connection (see `app::next_epoch`).
    pub ws_epoch: u64,
    /// Rendered `pm.visualizer.set` output, keyed by a hash of its input.
    viz: Option<(u64, Result<merpati_html::Document, String>)>,
    /// Maximized or minimized response panel.
    pub panel: PanelMode,
    /// The mode the last frame was laid out with.
    drawn_panel: PanelMode,
    /// The response before the current one (Compare).
    pub prev: Option<Box<crate::response_diff::Snapshot>>,
    /// Keep `prev` when new responses arrive.
    pub prev_pinned: bool,
    compare: crate::response_diff::CompareState,
    /// Set as variable, generated tests, confirmations.
    tools: crate::response_actions::Tools,
    /// A body value action requested by UI automation.
    pending_value: Option<crate::response_actions::ValueAction>,
}

pub struct GqlSubscription {
    session: merpati_http::ws::WsSession,
    subscribe: String,
    pub status: String,
    pub messages: Vec<(i64, String)>,
    started: Instant,
    /// The server completed, errored or the connection closed.
    pub done: bool,
}

/// True if the request is a GraphQL subscription operation.
pub fn is_subscription(item: &RequestItem) -> bool {
    item.http()
        .map(|h| h.body.mode == BodyMode::GraphQl && starts_with_subscription(&h.body.graphql.query))
        .unwrap_or(false)
}

fn starts_with_subscription(query: &str) -> bool {
    let mut q = query.trim_start();
    // Skip comments.
    while let Some(rest) = q.strip_prefix('#') {
        q = rest.split_once('\n').map(|(_, r)| r).unwrap_or("").trim_start();
    }
    q.starts_with("subscription")
}

impl RequestTab {
    /// WebSocket events of a GraphQL subscription.
    pub fn on_ws_event(&mut self, e: merpati_http::ws::WsEvent) {
        use merpati_http::ws::{WsDirection, WsEvent, WsPayload};
        let Some(sub) = &mut self.gql_sub else { return };
        match e {
            WsEvent::Connecting => sub.status = "Connecting…".into(),
            WsEvent::Connected { protocol, .. } => {
                sub.status =
                    format!("Connected ({}), initializing…", protocol.unwrap_or_else(|| "no subprotocol".into()));
                sub.session.send_text(serde_json::json!({"type": "connection_init", "payload": {}}).to_string());
            }
            WsEvent::Message { direction: WsDirection::Incoming, payload: WsPayload::Text(t), at_ms } => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
                match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "connection_ack" => {
                        sub.session.send_text(sub.subscribe.clone());
                        sub.status = "Subscribed".into();
                    }
                    "next" | "data" => {
                        let payload = v.get("payload").cloned().unwrap_or(v.clone());
                        sub.messages.push((at_ms, serde_json::to_string_pretty(&payload).unwrap_or(t)));
                    }
                    "error" => {
                        sub.status = "Error".into();
                        sub.messages.push((at_ms, serde_json::to_string_pretty(&v).unwrap_or(t)));
                    }
                    "complete" => {
                        sub.status = "Completed by server".into();
                        sub.done = true;
                        sub.session.close(1000, "");
                    }
                    "ping" => {
                        sub.session.send_text(serde_json::json!({"type": "pong"}).to_string());
                    }
                    "ka" | "pong" => {}
                    _ => sub.messages.push((at_ms, t)),
                }
            }
            WsEvent::Message { .. } => {}
            WsEvent::Error(m) => {
                sub.status = format!("Error: {m}");
                sub.done = true;
            }
            WsEvent::Closed { code, reason, .. } => {
                sub.done = true;
                if sub.status.starts_with("Completed") || sub.status.starts_with("Error") {
                    return;
                }
                sub.status = format!(
                    "Closed{}{}",
                    code.map(|c| format!(" ({c})")).unwrap_or_default(),
                    if reason.is_empty() { String::new() } else { format!(": {reason}") }
                );
            }
        }
    }
}

impl RequestTab {
    fn base(location: Option<ItemRef>, scratch: Option<RequestItem>) -> Self {
        RequestTab {
            location,
            scratch,
            section: Section::Params,
            script_event: ScriptEvent::PostResponse,
            resp_section: RespSection::Body,
            running: None,
            result: None,
            graphql: GraphQlState::default(),
            test_filter: TestFilter::All,
            renaming: None,
            split: 0.5,
            click_send: false,
            focus_url: false,
            gql_sub: None,
            ws_epoch: 0,
            viz: None,
            panel: PanelMode::Normal,
            drawn_panel: PanelMode::Normal,
            prev: None,
            prev_pinned: false,
            compare: Default::default(),
            tools: Default::default(),
            pending_value: None,
        }
    }

    pub fn scratch(item: RequestItem) -> Self {
        Self::base(None, Some(item))
    }

    pub fn saved(r: ItemRef) -> Self {
        Self::base(Some(r), None)
    }

    pub fn start_running(&mut self, cancel: CancellationToken) {
        self.gql_sub = None;
        // A minimized panel opens again to show the new response.
        if self.panel == PanelMode::Minimized {
            self.panel = PanelMode::Normal;
        }
        self.running = Some(Running { cancel, started: Instant::now() });
    }

    pub fn cancel(&self) {
        if let Some(r) = &self.running {
            r.cancel.cancel();
        }
    }

    pub fn finish(&mut self, exec: Execution, rerun: bool) {
        self.running = None;
        if rerun {
            if let Some(r) = &mut self.result {
                r.exec.tests = exec.tests;
                r.exec.script_errors = exec.script_errors;
                r.exec.console.extend(exec.console);
                r.exec.visualization = exec.visualization.or(r.exec.visualization.take());
                self.resp_section = RespSection::Tests;
            }
            return;
        }
        let had_tests = !exec.tests.is_empty();
        // The new body keeps how the last one was viewed (view, wrap, filter).
        let mut body = BodyState::default();
        if let Some(old) = &self.result {
            body.view = old.body.view;
            body.wrap = old.body.wrap;
            body.filter = old.body.filter.clone();
            body.filter_open = old.body.filter_open;
        }
        // The response being replaced becomes the one to compare with,
        // unless a baseline is kept.
        if let Some(old) = self.result.take()
            && let Some(resp) = old.exec.response
            && !(self.prev_pinned && self.prev.is_some())
        {
            self.prev = Some(Box::new(crate::response_diff::Snapshot { resp, at: old.at }));
        }
        self.compare.reset();
        self.result = Some(Box::new(ResultView { exec, body, at: chrono::Local::now() }));
        if self.resp_section == RespSection::Tests && !had_tests {
            self.resp_section = RespSection::Body;
        }
    }

    /// UI automation of the response panel: `maximize`, `minimize`,
    /// `restore`, `compare`, `compare_headers`, `generate_tests`,
    /// `add_generated_tests` (presses "Add tests" in the preview),
    /// `set_var:<path>`, `add_test:<path>` (a path of the JSON body).
    pub fn automation(&mut self, action: &str) {
        use crate::response_actions::ValueAction;
        match action {
            "maximize" => self.panel = PanelMode::Maximized,
            "minimize" => self.panel = PanelMode::Minimized,
            "restore" => self.panel = PanelMode::Normal,
            "compare" | "compare_headers" => {
                self.resp_section = RespSection::Compare;
                self.compare.mode = if action == "compare" {
                    crate::response_diff::Mode::Body
                } else {
                    crate::response_diff::Mode::Headers
                };
            }
            "add_generated_tests" => self.tools.auto_insert = true,
            "generate_tests" => {
                if let Some(resp) = self.result.as_ref().and_then(|r| r.exec.response.as_ref()) {
                    self.tools.generate(resp);
                }
            }
            other => {
                let Some((kind, path)) = other.split_once(':') else { return };
                let json: Option<serde_json::Value> = self
                    .result
                    .as_ref()
                    .and_then(|r| r.exec.response.as_ref())
                    .and_then(|r| r.body.read_all().ok())
                    .and_then(|b| serde_json::from_slice(&b).ok());
                let Some(value) =
                    json.as_ref().and_then(|j| crate::test_gen::lookup(j, &crate::test_gen::parse_path(path))).cloned()
                else {
                    return;
                };
                let path = path.to_string();
                self.pending_value = match kind {
                    "set_var" => Some(ValueAction::SetVariable { path, value }),
                    "add_test" => Some(ValueAction::AddTest { path, value }),
                    _ => None,
                };
            }
        }
    }
}

/// Context computed before editing to avoid borrowing the workspace twice.
struct EditCtx {
    vars: VarCtx,
    inherited: Option<(String, AuthType)>,
    breadcrumb: Vec<String>,
    token: Option<merpati_core::tokens::OAuth2Token>,
    fetching: bool,
    max_pretty: u64,
    side_layout: bool,
}

#[derive(Default)]
struct Actions {
    send: bool,
    cancel: bool,
    save: bool,
    codegen: bool,
    fetch_schema: bool,
    get_token: bool,
    clear_token: bool,
    save_example: bool,
    rerun_tests: bool,
    duplicate: bool,
    /// What a pasted cURL command could not import exactly.
    curl_warnings: Vec<String>,
    /// "Set as variable" / "Add test" from the body.
    value_action: Option<crate::response_actions::ValueAction>,
    /// Open the "Generate tests" preview.
    generate_tests: bool,
    /// Copy the request as sent (resolved) as a cURL command.
    copy_curl: bool,
}

fn get_item(app: &MerpatiApp, tab: &RequestTab) -> Option<RequestItem> {
    match &tab.location {
        Some(loc) => app.ws.data.collection(&loc.collection)?.find_request(&loc.item).cloned(),
        None => tab.scratch.clone(),
    }
}

fn put_item(app: &mut MerpatiApp, tab: &mut RequestTab, item: RequestItem) {
    match &tab.location {
        Some(loc) => {
            let cid = loc.collection.clone();
            if let Some(target) =
                app.ws.data.collection_mut(&loc.collection).and_then(|c| c.find_request_mut(&loc.item))
            {
                *target = item;
                app.ws.mark_collection(&cid);
            }
        }
        None => tab.scratch = Some(item),
    }
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut RequestTab) {
    let Some(original) = get_item(app, tab) else {
        ui.label(tr("This request no longer exists."));
        return;
    };
    let mut item = original.clone();
    let collection_id = tab.location.as_ref().map(|l| l.collection.clone());
    let (inherited, breadcrumb) = match &tab.location {
        Some(loc) => match app.ws.data.collection(&loc.collection) {
            Some(c) => {
                let chain = c.ancestors(&loc.item).unwrap_or_default();
                let inherited =
                    merpati_core::model::effective_from_chain(&chain).map(|e| (e.owner_name.to_string(), e.auth.kind));
                (inherited, chain.iter().map(|a| a.name().to_string()).collect())
            }
            None => (None, Vec::new()),
        },
        None => (None, vec![tr("Unsaved").to_string()]),
    };
    let token = TokenStore::get(app.ws.secrets.as_ref(), &item.id);
    let ectx = EditCtx {
        vars: app.var_ctx(collection_id.as_deref()),
        inherited,
        breadcrumb,
        fetching: app.pending_tokens.contains(&item.id),
        token,
        max_pretty: app.settings.max_pretty_bytes,
        side_layout: app.settings.ui.layout == merpati_core::settings::ResponseLayout::Side,
    };
    let mut actions = Actions::default();

    header(ui, tab, &mut item, &ectx, &mut actions);
    ui.add_space(4.0);

    let resp_panel = |ui: &mut Ui, tab: &mut RequestTab, actions: &mut Actions| {
        let rect = ui.max_rect();
        response_panel(ui, tab, &ectx, actions);
        rect
    };
    let avail = ui.available_rect_before_wrap();
    let mut after = crate::response_actions::After::default();
    let panel_rect = if ectx.side_layout {
        let w = match tab.panel {
            PanelMode::Normal => (avail.width() * tab.split).clamp(260.0, (avail.width() - 280.0).max(260.0)),
            PanelMode::Maximized => avail.width(),
            PanelMode::Minimized => MINIMIZED_RAIL,
        };
        let rect = egui::Panel::right(egui::Id::unique(("resp-side", tab_id)))
            .exact_size(w)
            .resizable(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 10, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| resp_panel(ui, tab, &mut actions))
            .inner;
        let handle = egui::Rect::from_min_max(
            egui::pos2(rect.left() - 10.0, rect.top()),
            egui::pos2(rect.left() - 4.0, rect.bottom()),
        );
        if tab.panel != PanelMode::Maximized {
            split_handle(ui, egui::Id::unique(("split-side", tab_id)), handle, true, avail, tab);
        }
        rect
    } else {
        let h = match tab.panel {
            PanelMode::Normal => (avail.height() * tab.split).clamp(120.0, (avail.height() - 140.0).max(120.0)),
            // The request keeps its section tabs.
            PanelMode::Maximized => (avail.height() - 44.0).max(120.0),
            PanelMode::Minimized => MINIMIZED_BAR,
        };
        let rect = egui::Panel::bottom(egui::Id::unique(("resp-below", tab_id)))
            .exact_size(h)
            .resizable(false)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 0, top: 8, bottom: 0 }))
            .show(ui, |ui| resp_panel(ui, tab, &mut actions))
            .inner;
        let handle = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() - 8.0),
            egui::pos2(rect.right(), rect.top() - 2.0),
        );
        split_handle(ui, egui::Id::unique(("split-below", tab_id)), handle, false, avail, tab);
        rect
    };
    if !(ectx.side_layout && tab.panel == PanelMode::Maximized) {
        request_sections(ui, tab, &mut item, &ectx, &mut actions);
    }
    // Over the response, or the whole tab when the response is a narrow rail.
    let notice_rect = if panel_rect.width() < 320.0 { avail } else { panel_rect };
    let scripts_shown = tab.section == Section::Scripts
        && tab.script_event == ScriptEvent::PostResponse
        && tab.panel != PanelMode::Maximized;
    crate::response_actions::notice(ui, &mut tab.tools, notice_rect, scripts_shown, &mut after);

    // Body value actions (context menu of the body, or automation).
    if let Some(a) = actions.value_action.take().or_else(|| tab.pending_value.take())
        && crate::response_actions::start(&mut tab.tools, a, &mut item)
    {
        after.changed = true;
    }
    if actions.generate_tests
        && let Some(resp) = tab.result.as_ref().and_then(|r| r.exec.response.as_ref())
    {
        tab.tools.generate(resp);
    }
    let dialogs = crate::response_actions::dialogs(app, ui.ctx(), &mut tab.tools, &mut item, collection_id.as_deref());
    after.changed |= dialogs.changed;
    if dialogs.rerun_tests {
        actions.rerun_tests = true;
        tab.resp_section = RespSection::Tests;
        if tab.panel == PanelMode::Minimized {
            tab.panel = PanelMode::Normal;
        }
    }
    if after.rerun_tests {
        actions.rerun_tests = true;
    }
    if after.open_scripts || dialogs.open_scripts {
        tab.section = Section::Scripts;
        tab.script_event = ScriptEvent::PostResponse;
        if tab.panel == PanelMode::Maximized {
            tab.panel = PanelMode::Normal;
        }
    }
    if actions.copy_curl
        && let Some(prepared) = tab.result.as_ref().and_then(|r| r.exec.prepared.as_ref())
    {
        let code = merpati_convert::codegen::generate(merpati_convert::codegen::Language::Curl, prepared);
        ui.ctx().copy_text(code);
        app.toast(tr("Copied the request as sent, as a cURL command"), false);
    }

    if item != original {
        put_item(app, tab, item.clone());
    }
    // A new panel size lays the frame out again before it is shown, so
    // lists sized by the panel don't show a frame of the old size.
    if tab.panel != tab.drawn_panel {
        tab.drawn_panel = tab.panel;
        ui.ctx().request_discard("response panel resized");
    }

    // Deferred actions that need the whole app.
    if actions.send {
        if is_subscription(&item) {
            if tab.gql_sub.as_ref().map(|s| s.done).unwrap_or(true) {
                start_subscription(app, tab_id, tab, &item, collection_id.as_deref());
            }
        } else {
            app.deferred.push(crate::app::Deferred::Send(tab_id));
        }
    }
    if !actions.curl_warnings.is_empty() {
        app.toast(format!("{}\n{}", tr("Imported from cURL with warnings:"), actions.curl_warnings.join("\n")), true);
    }
    if actions.cancel {
        tab.cancel();
        if let Some(sub) = tab.gql_sub.take() {
            sub.session.close(1000, "");
        }
    }
    if actions.save {
        if tab.location.is_none() {
            app.dialog = Some(crate::dialogs::Dialog::save_as(tab_id, &app.ws));
        } else {
            app.ws.flush();
            app.toast(tr("Saved"), false);
        }
    }
    if actions.codegen {
        app.dialog = Some(crate::dialogs::Dialog::codegen(app, &item, collection_id.as_deref()));
    }
    if actions.get_token
        && let Some(http) = item.http()
    {
        let scopes = app.ws.scopes_for(collection_id.as_deref());
        app.fetch_token(http.auth.clone(), scopes, item.id.clone());
    }
    if actions.clear_token {
        TokenStore::set(app.ws.secrets.as_ref(), &item.id, None);
    }
    if actions.fetch_schema {
        fetch_schema(app, tab_id, tab, &item, collection_id.as_deref());
    }
    if actions.rerun_tests {
        app.deferred.push(crate::app::Deferred::RerunTests(tab_id));
    }
    if actions.save_example
        && let Some(result) = &tab.result
        && let Some(resp) = &result.exec.response
    {
        let mut req = item.http().cloned().unwrap_or_default();
        req.settings = RequestSettings::default();
        let body = if resp.body.len() <= 4 * 1024 * 1024 { resp.body.text_lossy() } else { String::new() };
        let ex = Example {
            name: format!("{} {}", resp.status, resp.status_text),
            request: Some(req),
            status: resp.status,
            status_text: resp.status_text.clone(),
            headers: resp
                .headers
                .iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.as_str(),
                        "content-length" | "transfer-encoding" | "connection" | "date" | "content-encoding"
                    )
                })
                .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
                .collect(),
            body,
            language: Some(resp.language().as_str().to_string()),
            ..Default::default()
        };
        item.examples.push(ex);
        put_item(app, tab, item.clone());
        app.toast(tr("Saved as example"), false);
        tab.section = Section::Docs;
    }
    if actions.duplicate {
        if let Some(loc) = &tab.location {
            if let Some(c) = app.ws.data.collection_mut(&loc.collection) {
                let parent = c.parent_of(&loc.item).map(str::to_string);
                let mut copy = merpati_core::model::Item::Request(item.clone()).duplicate();
                copy.set_name(trf("{} Copy", &[&item.name]));
                let new_id = copy.id().to_string();
                let idx = c
                    .children_mut(parent.as_deref())
                    .and_then(|v| v.iter().position(|i| i.id() == loc.item))
                    .map(|i| i + 1)
                    .unwrap_or(0);
                c.insert(parent.as_deref(), idx, copy);
                let cid = loc.collection.clone();
                app.ws.mark_collection(&cid);
                app.open_item(ItemRef { collection: cid, item: new_id });
            }
        } else {
            let mut copy = item.clone();
            copy.id = merpati_core::util::new_id();
            copy.name = trf("{} Copy", &[&item.name]);
            app.add_tab(crate::app::TabKind::Request(Box::new(RequestTab::scratch(copy))));
        }
    }
}

/// Height of the minimized response panel (its header bar).
const MINIMIZED_BAR: f32 = 44.0;
/// Width of the minimized response panel beside the request.
const MINIMIZED_RAIL: f32 = 46.0;

/// A draggable divider between the request editor and the response panel.
/// Dragging a maximized or minimized panel brings back the split at the
/// pointer; a double click restores it (or resets the split to half).
fn split_handle(ui: &mut Ui, id: egui::Id, rect: egui::Rect, vertical: bool, avail: Rect, tab: &mut RequestTab) {
    let total = if vertical { avail.width() } else { avail.height() };
    let r = ui.interact(rect, id, egui::Sense::click_and_drag());
    let hovered = r.hovered() || r.dragged();
    if hovered {
        ui.ctx().set_cursor_icon(if vertical {
            egui::CursorIcon::ResizeHorizontal
        } else {
            egui::CursorIcon::ResizeVertical
        });
    }
    let split = &mut tab.split;
    if r.dragged() && total > 0.0 {
        if tab.panel != PanelMode::Normal {
            if let Some(pos) = r.interact_pointer_pos() {
                let from_end = if vertical { avail.right() - pos.x } else { avail.bottom() - pos.y };
                *split = (from_end / total).clamp(0.15, 0.85);
            }
            tab.panel = PanelMode::Normal;
        } else {
            let d = r.drag_delta();
            let delta = if vertical { -d.x } else { -d.y };
            *split = (*split + delta / total).clamp(0.15, 0.85);
        }
    }
    if r.double_clicked() {
        if tab.panel == PanelMode::Normal {
            *split = 0.5;
        } else {
            tab.panel = PanelMode::Normal;
        }
    }
    let stroke = if hovered {
        egui::Stroke::new(2.0, crate::theme::accent(ui.visuals().dark_mode))
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    if vertical {
        ui.painter().vline(rect.left() + 1.0, rect.y_range(), stroke);
    } else {
        ui.painter().hline(rect.x_range(), rect.top() + 1.0, stroke);
    }
}

/// Applies an edit of the URL field: a pasted cURL command becomes the whole
/// request (returning what it could not import exactly), anything else is
/// the URL.
fn url_edited(http: &mut HttpRequest, url: String) -> Vec<String> {
    let trimmed = url.trim_start();
    if (trimmed.starts_with("curl ") || trimmed.starts_with("curl\t"))
        && let Ok((parsed, warnings)) = merpati_convert::curl::parse_with_warnings(trimmed)
    {
        *http = parsed;
        return warnings;
    }
    http.set_url(url);
    Vec::new()
}

fn header(ui: &mut Ui, tab: &mut RequestTab, item: &mut RequestItem, ectx: &EditCtx, actions: &mut Actions) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.set_height(30.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        for part in &ectx.breadcrumb {
            ui.label(RichText::new(part.as_str()).color(p.text_muted));
            ui.label(RichText::new(icons::CARET_RIGHT).font(theme::icon_font(11.0)).color(p.text_faint));
        }
        match &mut tab.renaming {
            Some(buf) => {
                let r = ui.add(egui::TextEdit::singleline(buf).desired_width(260.0));
                r.request_focus();
                if r.lost_focus() {
                    if !buf.trim().is_empty() {
                        item.name = buf.trim().to_string();
                    }
                    tab.renaming = None;
                }
            }
            None => {
                let r = ui.add(
                    egui::Label::new(RichText::new(&item.name).font(theme::semibold(15.0)).color(p.text))
                        .sense(egui::Sense::click())
                        .selectable(false),
                );
                if r.on_hover_text(tr("Double-click to rename")).double_clicked() {
                    tab.renaming = Some(item.name.clone());
                }
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let more = kit::icon_button(ui, icons::DOTS_THREE, tr("More actions"));
            egui::Popup::menu(&more).show(|ui| {
                if ui.button(kit::with_icon(icons::CODE, tr("Code snippet"))).clicked() {
                    actions.codegen = true;
                    ui.close();
                }
                if ui.button(kit::with_icon(icons::COPY, tr("Duplicate"))).clicked() {
                    actions.duplicate = true;
                    ui.close();
                }
            });
            if kit::secondary(ui, Some(icons::FLOPPY_DISK), tr("Save")).on_hover_text("⌘S").clicked() {
                actions.save = true;
            }
            if kit::icon_button(ui, icons::CODE, tr("Code snippet")).clicked() {
                actions.codegen = true;
            }
        });
    });
    ui.add_space(6.0);
    let Some(http) = item.http_mut() else { return };
    // Method, URL and Send as one group.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let send_w = 104.0;
        let group_w = (ui.available_width() - send_w - 8.0).max(200.0);
        let running = tab.running.is_some() || tab.gql_sub.as_ref().map(|s| !s.done).unwrap_or(false);
        let frame = egui::Frame::new()
            .fill(p.surface)
            .stroke(egui::Stroke::new(1.0, p.border_strong))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(2, 2));
        let group = frame.show(ui, |ui| {
            ui.set_width(group_w - 4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.scope(|ui| {
                    let w = &mut ui.visuals_mut().widgets;
                    for v in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
                        v.bg_stroke = egui::Stroke::NONE;
                    }
                    w.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                    widgets::method_picker(ui, &mut http.method);
                });
                kit::vdivider(ui, 22.0);
                let mut url = http.url.clone();
                let r = widgets::var_cell(
                    ui,
                    "url",
                    &mut url,
                    &ectx.vars,
                    tr("Enter a URL and press Send"),
                    ui.available_width(),
                );
                if std::mem::take(&mut tab.focus_url) {
                    r.request_focus();
                }
                if r.changed() {
                    actions.curl_warnings = url_edited(http, url);
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    actions.send = true;
                }
                r.has_focus()
            })
            .inner
        });
        if group.inner {
            kit::focus_ring(ui, group.response.rect, 8);
        }
        let h = group.response.rect.height();
        ui.allocate_ui_with_layout(
            egui::vec2(send_w, h),
            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
            |ui| {
                if running {
                    if kit::button_ex(ui, kit::Kind::Secondary, Some(icons::STOP), tr("Cancel"), false).clicked() {
                        actions.cancel = true;
                    }
                } else {
                    let send = kit::primary_icon(ui, icons::PAPER_PLANE_RIGHT, tr("Send"));
                    if send.on_hover_text("⌘↩").clicked() || std::mem::take(&mut tab.click_send) {
                        actions.send = true;
                    }
                }
            },
        );
    });
}

fn request_sections(ui: &mut Ui, tab: &mut RequestTab, item: &mut RequestItem, ectx: &EditCtx, actions: &mut Actions) {
    let id = item.id.clone();
    let Some(http) = item.http_mut() else { return };
    let enabled_params = http.params.iter().filter(|p| p.enabled).count();
    let enabled_headers = http.headers.iter().filter(|p| p.enabled).count();
    // A dot after "Body" and "Authorization" tells that they are set.
    let mut dots = Vec::new();
    if http.body.mode != BodyMode::None {
        dots.push(Section::Body);
    }
    if !matches!(http.auth.kind, AuthType::Inherit | AuthType::None) {
        dots.push(Section::Auth);
    }
    let overridden = settings_overridden(&http.settings);
    let script_count = [!item.scripts.pre_request.trim().is_empty(), !item.scripts.post_response.trim().is_empty()]
        .iter()
        .filter(|b| **b)
        .count();
    let items = vec![
        (Section::Params, format!("{}{}", tr("Params"), widgets::count_label(enabled_params))),
        (Section::Auth, tr("Authorization").to_string()),
        (Section::Headers, format!("{}{}", tr("Headers"), widgets::count_label(enabled_headers))),
        (Section::Body, tr("Body").to_string()),
        (Section::Scripts, format!("{}{}", tr("Scripts"), widgets::count_label(script_count))),
        (Section::Settings, format!("{}{}", tr("Settings"), widgets::count_label(overridden))),
        (Section::Docs, format!("{}{}", tr("Docs"), widgets::count_label(item.examples.len()))),
    ];
    ui.add_space(4.0);
    // The sections keep the width of the editor even when the tab row is
    // wider than a narrow window.
    let area = ui.available_rect_before_wrap();
    // With the response maximized only the section tabs show; picking one
    // brings the editor back.
    if tab.panel == PanelMode::Maximized {
        let mut picked = false;
        ui.horizontal(|ui| {
            let strip_w = (ui.available_width() - 120.0).max(120.0);
            ui.allocate_ui_with_layout(vec2(strip_w, 34.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                picked = kit::tab_strip_ex(ui, &mut tab.section, &items, &dots);
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if kit::ghost(ui, Some(icons::CARET_DOWN), tr("Show editor")).clicked() {
                    picked = true;
                }
            });
        });
        if picked {
            tab.panel = PanelMode::Normal;
        }
        return;
    }
    kit::tab_strip_ex(ui, &mut tab.section, &items, &dots);
    ui.add_space(10.0);
    let rect = Rect::from_min_max(pos2(area.left(), ui.cursor().top()), pos2(area.right(), area.bottom()));
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        egui::ScrollArea::vertical().id_salt(("req-sections", &id)).auto_shrink([false; 2]).show(ui, |ui| {
            match tab.section {
                Section::Scripts => scripts_editor(ui, tab, &mut item.scripts, &ectx.vars, actions),
                Section::Docs => docs_editor(ui, item),
                section => {
                    let http = item.request.as_http_mut().expect("http request");
                    sections_http(ui, tab, section, http, &id, ectx, actions);
                }
            }
        });
    });
}

fn sections_http(
    ui: &mut Ui,
    tab: &mut RequestTab,
    section: Section,
    http: &mut HttpRequest,
    id: &str,
    ectx: &EditCtx,
    actions: &mut Actions,
) {
    match section {
        Section::Params => {
            let mut params = http.params.clone();
            if widgets::kv_table(
                ui,
                "params",
                &mut params,
                &ectx.vars,
                KvOptions {
                    title: Some(tr("Query parameters")),
                    show_description: true,
                    allow_add: true,
                    key_hint: tr("Key"),
                    value_hint: tr("Value"),
                    ..Default::default()
                },
            ) {
                http.params = params;
                http.sync_url_from_params();
            }
            if !http.path_vars.is_empty() {
                ui.add_space(14.0);
                let mut vars = http.path_vars.clone();
                if widgets::kv_table(
                    ui,
                    "path-vars",
                    &mut vars,
                    &ectx.vars,
                    KvOptions {
                        title: Some(tr("Path variables")),
                        show_description: true,
                        readonly_keys: true,
                        allow_add: false,
                        ..Default::default()
                    },
                ) {
                    http.path_vars = vars;
                }
            }
        }
        Section::Auth => {
            let (changed, action) = crate::auth_ui::auth_editor(
                ui,
                &mut http.auth,
                crate::auth_ui::AuthUiCtx {
                    is_root: false,
                    inherited: ectx.inherited.clone(),
                    owner_id: id,
                    token: ectx.token.clone(),
                    fetching: ectx.fetching,
                    vars: &ectx.vars,
                },
            );
            let _ = changed;
            match action {
                crate::auth_ui::AuthAction::GetToken => actions.get_token = true,
                crate::auth_ui::AuthAction::ClearToken => actions.clear_token = true,
                crate::auth_ui::AuthAction::None => {}
            }
        }
        Section::Headers => {
            let mut headers = http.headers.clone();
            if widgets::kv_table(
                ui,
                "headers",
                &mut headers,
                &ectx.vars,
                KvOptions {
                    title: Some(tr("Request headers")),
                    key_suggestions: Some(widgets::COMMON_HEADERS),
                    show_description: true,
                    allow_add: true,
                    key_hint: tr("Key"),
                    value_hint: tr("Value"),
                    ..Default::default()
                },
            ) {
                http.headers = headers;
            }
            ui.add_space(16.0);
            auto_headers(ui, http);
        }
        Section::Body => body_editor(ui, tab, http, ectx, actions),
        Section::Settings => settings_editor(ui, &mut http.settings),
        Section::Scripts | Section::Docs => {}
    }
}

/// Headers the client adds by itself, in a collapsible, muted table. Default
/// headers can be switched off; calculated ones are shown with a lock.
fn auto_headers(ui: &mut Ui, http: &mut HttpRequest) {
    let p = theme::pal(ui);
    let defaults = merpati_core::settings::RequestDefaults::default();
    // (name, value, Some(on) when it can be switched off)
    let mut rows: Vec<(String, String, Option<bool>)> = Vec::new();
    for (k, v) in defaults.default_headers() {
        let disabled = http.settings.disabled_default_headers.iter().any(|d| d.eq_ignore_ascii_case(k));
        rows.push((k.to_string(), v, Some(!disabled)));
    }
    let ct = match http.body.mode {
        BodyMode::Raw => Some(http.body.raw_language.content_type().to_string()),
        BodyMode::UrlEncoded => Some("application/x-www-form-urlencoded".into()),
        BodyMode::FormData => Some("multipart/form-data; boundary=<calculated>".into()),
        BodyMode::GraphQl => Some("application/json".into()),
        _ => None,
    };
    for (k, v) in [
        ("Host", "<calculated when sent>".to_string()),
        ("Accept-Encoding", "gzip, deflate, br".into()),
        ("Content-Length", "<calculated>".into()),
    ]
    .into_iter()
    .chain(ct.map(|c| ("Content-Type", c)))
    {
        rows.push((k.to_string(), v, None));
    }
    let id = ui.make_persistent_id("auto-headers");
    let open = disclosure(ui, id, tr("Auto-generated headers"), Some(rows.len()), false);
    if !open {
        ui.label(kit::hint(ui, tr("Sent with every request; headers you set above override them.")));
        return;
    }
    ui.add_space(6.0);
    const CHECK: f32 = 34.0;
    const HEAD: f32 = 28.0;
    const ROW: f32 = 30.0;
    let width = ui.available_width().max(240.0);
    let left = ui.cursor().left();
    let top = ui.cursor().top();
    let key_w = (width - CHECK) * 0.36;
    let xs = [left + CHECK, left + CHECK + key_w];
    let mono = theme::mono(mono_size(ui));
    let head = Rect::from_min_size(pos2(left, top), vec2(width, HEAD));
    ui.painter().rect_filled(head, CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, p.chrome);
    for (x, label) in [(xs[0], tr("Key")), (xs[1], tr("Value"))] {
        ui.painter().text(
            pos2(x + 10.0, head.center().y),
            Align2::LEFT_CENTER,
            label,
            theme::medium(11.5),
            p.text_muted,
        );
    }
    let mut y = head.bottom();
    for (i, (k, v, on)) in rows.into_iter().enumerate() {
        let row = Rect::from_min_size(pos2(left, y), vec2(width, ROW));
        ui.painter().hline(left..=left + width, y, Stroke::new(1.0, p.border));
        let faded = on == Some(false);
        let color = if faded { p.text_faint } else { p.text_muted };
        match on {
            Some(mut on) => {
                let c = Rect::from_center_size(pos2(left + CHECK / 2.0 + 2.0, row.center().y), vec2(18.0, ROW - 6.0));
                let r = ui.put(c, egui::Checkbox::without_text(&mut on));
                widgets::a11y_name(&r, &format!("{} {k}", tr("Send")));
                if r.changed() {
                    if on {
                        http.settings.disabled_default_headers.retain(|d| !d.eq_ignore_ascii_case(&k));
                    } else {
                        http.settings.disabled_default_headers.push(k.to_lowercase());
                    }
                }
            }
            None => {
                let c = Rect::from_center_size(pos2(left + CHECK / 2.0 + 2.0, row.center().y), vec2(18.0, 18.0));
                ui.painter().text(
                    c.center(),
                    Align2::CENTER_CENTER,
                    icons::LOCK_SIMPLE,
                    theme::icon_font(13.0),
                    p.text_faint,
                );
                ui.interact(c, id.with(("lock", i)), Sense::hover())
                    .on_hover_text(tr("Calculated when the request is sent"));
            }
        }
        let g = elide(ui, &k, mono.clone(), color, key_w - 20.0);
        ui.painter().galley(pos2(xs[0] + 10.0, row.center().y - g.size().y / 2.0), g, color);
        let g = elide(ui, &v, mono.clone(), color, left + width - xs[1] - 20.0);
        ui.painter().galley(pos2(xs[1] + 10.0, row.center().y - g.size().y / 2.0), g, color);
        y += ROW;
    }
    let table = Rect::from_min_max(pos2(left, top), pos2(left + width, y));
    for x in xs {
        ui.painter().vline(x, table.y_range(), Stroke::new(1.0, p.border));
    }
    ui.painter().rect_stroke(table, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.advance_cursor_after_rect(table);
    ui.add_space(6.0);
    ui.label(kit::hint(
        ui,
        tr("Headers you set explicitly override these. Cookies from the jar are added automatically."),
    ));
}

/// Formats JSON that may contain `{{variables}}` outside of strings.
pub fn beautify_json_with_vars(text: &str) -> Option<String> {
    let mut placeholders: Vec<(String, String)> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escape = false;
    let mut i = 0;
    let b = text.as_bytes();
    while i < b.len() {
        let c = b[i];
        if !in_string
            && text[i..].starts_with("{{")
            && let Some(end) = text[i..].find("}}")
        {
            let var = &text[i..i + end + 2];
            let ph = format!("\"__merpati_var_{}__\"", placeholders.len());
            placeholders.push((ph.clone(), var.to_string()));
            out.push_str(&ph);
            i += end + 2;
            continue;
        }
        if in_string {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_string = false;
            }
        } else if c == b'"' {
            in_string = true;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let mut pretty = serde_json::to_string_pretty(&v).ok()?;
    for (ph, var) in placeholders {
        pretty = pretty.replace(&ph, &var);
    }
    Some(pretty)
}

/// Label of a body type in the type selector; the long urlencoded name is
/// shortened when space is tight.
fn body_mode_label(mode: BodyMode, narrow: bool) -> &'static str {
    match mode {
        BodyMode::None => tr("none"),
        BodyMode::UrlEncoded if narrow => "urlencoded",
        m => m.label(),
    }
}

fn body_editor(ui: &mut Ui, tab: &mut RequestTab, http: &mut HttpRequest, ectx: &EditCtx, actions: &mut Actions) {
    let width = ui.available_width();
    let raw = http.body.mode == BodyMode::Raw;
    // The raw language tools share the row when there is room for them.
    let tools_inline = width >= 760.0;
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        let items: Vec<(BodyMode, &str)> =
            BodyMode::ALL.iter().map(|m| (*m, body_mode_label(*m, width < 720.0))).collect();
        kit::segmented(ui, &mut http.body.mode, &items);
        if raw && tools_inline {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| raw_tools(ui, &mut http.body));
        }
    });
    if raw && !tools_inline {
        ui.add_space(4.0);
        ui.horizontal(|ui| raw_tools(ui, &mut http.body));
    }
    ui.add_space(8.0);
    let body: &mut Body = &mut http.body;
    match body.mode {
        BodyMode::None => {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                let p = theme::pal(ui);
                let (rect, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 22.0, p.subtle);
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    icons::FILE_TEXT,
                    theme::icon_font(20.0),
                    p.text_muted,
                );
                ui.add_space(6.0);
                ui.label(RichText::new(tr("This request does not have a body")).font(theme::semibold(13.5)));
                ui.label(kit::hint(ui, tr("Choose a body type above to send data with the request.")));
            });
        }
        BodyMode::Raw => {
            let lang = Lang::from_raw(body.raw_language);
            let rows = editor_rows(ui, 8);
            widgets::code_editor(ui, "raw-body", &mut body.raw, lang, Some(&ectx.vars), rows, false);
        }
        BodyMode::UrlEncoded => {
            let mut rows = body.urlencoded.clone();
            if widgets::kv_table(
                ui,
                "urlencoded",
                &mut rows,
                &ectx.vars,
                KvOptions {
                    title: Some(tr("Form fields")),
                    show_description: true,
                    allow_add: true,
                    key_hint: tr("Key"),
                    value_hint: tr("Value"),
                    ..Default::default()
                },
            ) {
                body.urlencoded = rows;
            }
            ui.add_space(6.0);
            ui.label(kit::hint(ui, tr("Sent as application/x-www-form-urlencoded.")));
        }
        BodyMode::FormData => {
            ui.horizontal(|ui| {
                ui.set_height(26.0);
                ui.label(kit::section_title(tr("Form fields")));
            });
            ui.add_space(4.0);
            let mut rows = body.form_data.clone();
            if form_data_table(ui, "formdata", &mut rows, &ectx.vars) {
                body.form_data = rows;
            }
            ui.add_space(6.0);
            ui.label(kit::hint(ui, tr("Sent as multipart/form-data. Switch a field to File to upload files.")));
        }
        BodyMode::Binary => binary_body(ui, body),
        BodyMode::GraphQl => graphql_editor(ui, tab, body, ectx, actions),
    }
}

/// Number of text rows that fill the space left in the section (at least
/// `min`), so editors use the panel instead of a fixed height.
fn editor_rows(ui: &Ui, min: usize) -> usize {
    let row = mono_size(ui) * 1.36;
    (((ui.available_height() - 14.0) / row).floor() as usize).clamp(min, 60)
}

/// Raw body language picker, JSON validity and Beautify.
fn raw_tools(ui: &mut Ui, body: &mut Body) {
    let lang_label = format!("{}  {}", body.raw_language.label(), icons::CARET_DOWN);
    let r = kit::button_ex(ui, kit::Kind::Secondary, Some(icons::CODE), &lang_label, true)
        .on_hover_text(tr("Language of the raw body (sets the Content-Type)"));
    egui::Popup::menu(&r).show(|ui| {
        for l in RawLanguage::ALL {
            let mark = if body.raw_language == l { icons::CHECK } else { "    " };
            if ui.button(format!("{mark}  {}", l.label())).clicked() {
                body.raw_language = l;
                ui.close();
            }
        }
    });
    if body.raw_language == RawLanguage::Json {
        let valid = body.raw.trim().is_empty() || beautify_json_with_vars(&body.raw).is_some();
        if kit::ghost(ui, Some(icons::SPARKLE), tr("Beautify")).clicked()
            && let Some(p) = beautify_json_with_vars(&body.raw)
        {
            body.raw = p;
        }
        if !valid {
            let p = theme::pal(ui);
            kit::pill(ui, &format!("{} {}", icons::WARNING_CIRCLE, tr("Invalid JSON")), p.err)
                .on_hover_text(tr("The body is not valid JSON; it is sent as typed."));
        }
    }
}

/// multipart/form-data fields in the visual language of `widgets::kv_table`:
/// enable box, key, Text/File type, value (or files), content type.
fn form_data_table(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    rows: &mut Vec<FormField>,
    vars: &VarCtx,
) -> bool {
    let salt = egui::Id::unique(&id);
    ui.push_id(salt, |ui| form_data_table_inner(ui, salt, rows, vars)).inner
}

fn form_data_table_inner(ui: &mut Ui, id: egui::Id, rows: &mut Vec<FormField>, vars: &VarCtx) -> bool {
    use egui::TextEdit;
    let p = theme::pal(ui);
    let mut changed = false;
    const CHECK: f32 = 34.0;
    const DEL: f32 = 32.0;
    const HEAD: f32 = 28.0;
    const ROW: f32 = 32.0;
    const KIND: f32 = 84.0;
    let width = ui.available_width().max(320.0);
    let left = ui.cursor().left();
    let top = ui.cursor().top();
    let cols = width - CHECK - DEL - KIND;
    let (key_w, val_w) = (cols * 0.3, cols * 0.46);
    let xs = [
        left + CHECK,
        left + CHECK + key_w,
        left + CHECK + key_w + KIND,
        left + CHECK + key_w + KIND + val_w,
        left + width - DEL,
    ];
    let mono = theme::mono(mono_size(ui));
    let head = Rect::from_min_size(pos2(left, top), vec2(width, HEAD));
    ui.painter().rect_filled(head, CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, p.chrome);
    for (x, label) in [(xs[0], tr("Key")), (xs[1], tr("Type")), (xs[2], tr("Value")), (xs[3], tr("Content-Type"))] {
        ui.painter().text(
            pos2(x + 10.0, head.center().y),
            Align2::LEFT_CENTER,
            label,
            theme::medium(11.5),
            p.text_muted,
        );
    }
    let mut y = head.bottom();
    let mut remove = None;
    let n = rows.len();
    let ring = |ui: &Ui, r: Rect| {
        ui.painter().rect_stroke(r, CornerRadius::same(5), Stroke::new(1.5, p.accent), StrokeKind::Outside);
    };
    for i in 0..=n {
        let is_new = i == n;
        let mut row = if is_new { FormField::text("", "") } else { rows[i].clone() };
        let mut row_changed = false;
        let row_rect = Rect::from_min_size(pos2(left, y), vec2(width, ROW));
        ui.painter().hline(left..=left + width, y, Stroke::new(1.0, p.border));
        let hovered = ui.rect_contains_pointer(row_rect);
        if hovered && !is_new {
            ui.painter().rect_filled(row_rect.shrink2(vec2(1.0, 0.5)), 0.0, p.faint_row());
        }
        let cell = |x0: f32, x1: f32| Rect::from_min_max(pos2(x0 + 4.0, y + 3.0), pos2(x1 - 4.0, y + ROW - 3.0));
        ui.push_id(i, |ui| {
            if !is_new {
                let c =
                    Rect::from_center_size(pos2(left + CHECK / 2.0 + 2.0, row_rect.center().y), vec2(18.0, ROW - 6.0));
                let r = ui.put(c, egui::Checkbox::without_text(&mut row.enabled));
                widgets::a11y_name(&r, &format!("{} {}", tr("Enabled"), i + 1));
                row_changed |= r.changed();
            }
            let faded = !is_new && !row.enabled;
            let text_color = if faded { p.text_faint } else { p.text };
            // Key
            let key_rect = cell(xs[0], xs[1]);
            let r = ui.put(
                key_rect,
                TextEdit::singleline(&mut row.key)
                    .hint_text(if is_new { tr("Key") } else { "" })
                    .font(mono.clone())
                    .text_color(text_color)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::symmetric(6, 2))
                    .min_size(vec2(0.0, key_rect.height()))
                    .align(Align2::LEFT_CENTER)
                    .desired_width(key_rect.width()),
            );
            widgets::a11y_name(&r, &format!("{} {}", tr("Key"), i + 1));
            if r.has_focus() {
                ring(ui, key_rect);
            }
            row_changed |= r.changed();
            // Type
            let kind_rect = cell(xs[1], xs[2]);
            let (icon, label) = match row.kind {
                FormFieldKind::Text => (icons::TEXT_AA, tr("Text")),
                FormFieldKind::File => (icons::FILE, tr("File")),
            };
            let r = ui
                .scope_builder(egui::UiBuilder::new().max_rect(kind_rect), |ui| {
                    ui.centered_and_justified(|ui| {
                        kit::button_ex(
                            ui,
                            kit::Kind::Ghost,
                            Some(icon),
                            &format!("{label} {}", icons::CARET_DOWN),
                            true,
                        )
                    })
                    .inner
                })
                .inner;
            egui::Popup::menu(&r).show(|ui| {
                for (k, icon, label) in
                    [(FormFieldKind::Text, icons::TEXT_AA, tr("Text")), (FormFieldKind::File, icons::FILE, tr("File"))]
                {
                    if ui.button(kit::with_icon(icon, label)).clicked() {
                        if row.kind != k {
                            row.kind = k;
                            row_changed = true;
                        }
                        ui.close();
                    }
                }
            });
            // Value
            let val_rect = cell(xs[2], xs[3]);
            match row.kind {
                FormFieldKind::Text => {
                    let r = ui
                        .scope_builder(egui::UiBuilder::new().max_rect(val_rect), |ui| {
                            widgets::var_cell(
                                ui,
                                "v",
                                &mut row.value,
                                vars,
                                if is_new { tr("Value") } else { "" },
                                val_rect.width(),
                            )
                        })
                        .inner;
                    widgets::a11y_name(&r, &format!("{} {}", tr("Value"), i + 1));
                    if r.has_focus() {
                        ring(ui, val_rect);
                    }
                    row_changed |= r.changed();
                }
                FormFieldKind::File => {
                    let names = row
                        .files
                        .iter()
                        .map(|f| {
                            std::path::Path::new(f)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| f.clone())
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let r = ui
                        .scope_builder(egui::UiBuilder::new().max_rect(val_rect), |ui| {
                            ui.horizontal_centered(|ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                if row.files.is_empty() {
                                    kit::button_ex(
                                        ui,
                                        kit::Kind::Ghost,
                                        Some(icons::UPLOAD_SIMPLE),
                                        tr("Select files"),
                                        true,
                                    )
                                } else {
                                    let name_w = (ui.available_width() - 26.0).max(40.0);
                                    let g = elide(
                                        ui,
                                        &format!("{}  {names}", icons::FILE),
                                        mono.clone(),
                                        text_color,
                                        name_w - 8.0,
                                    );
                                    let (rect, resp) = ui.allocate_exact_size(vec2(name_w, 22.0), Sense::click());
                                    if resp.hovered() {
                                        ui.painter().rect_filled(rect, CornerRadius::same(4), p.hover);
                                    }
                                    ui.painter().galley(
                                        pos2(rect.left() + 4.0, rect.center().y - g.size().y / 2.0),
                                        g,
                                        text_color,
                                    );
                                    let clear = kit::icon_button_ex(ui, icons::X, tr("Clear files"), false, 22.0);
                                    if clear.clicked() {
                                        row.files.clear();
                                        row_changed = true;
                                    }
                                    resp.on_hover_text(row.files.join("\n"))
                                }
                            })
                            .inner
                        })
                        .inner;
                    if r.clicked()
                        && let Some(files) = rfd::FileDialog::new().pick_files()
                    {
                        row.files = files.into_iter().map(|p| p.display().to_string()).collect();
                        row_changed = true;
                    }
                }
            }
            // Content type
            let ct_rect = cell(xs[3], xs[4]);
            let mut ct = row.content_type.clone().unwrap_or_default();
            let r = ui.put(
                ct_rect,
                TextEdit::singleline(&mut ct)
                    .hint_text(if is_new || row.content_type.is_none() { tr("Auto") } else { "" })
                    .font(mono.clone())
                    .text_color(text_color)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::symmetric(6, 2))
                    .min_size(vec2(0.0, ct_rect.height()))
                    .align(Align2::LEFT_CENTER)
                    .desired_width(ct_rect.width()),
            );
            widgets::a11y_name(&r, &format!("{} {}", tr("Content-Type"), i + 1));
            if r.has_focus() {
                ring(ui, ct_rect);
            }
            if r.changed() {
                row.content_type = (!ct.is_empty()).then_some(ct);
                row_changed = true;
            }
            if !is_new && hovered {
                let d = Rect::from_center_size(pos2(xs[4] + DEL / 2.0, row_rect.center().y), vec2(24.0, 24.0));
                let r = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(d), |ui| {
                        kit::icon_button_ex(ui, icons::TRASH, tr("Remove"), false, 24.0)
                    })
                    .inner;
                if r.clicked() {
                    remove = Some(i);
                }
            }
        });
        y += ROW;
        if row_changed {
            changed = true;
            if is_new {
                if !row.key.is_empty()
                    || !row.value.is_empty()
                    || !row.files.is_empty()
                    || row.kind == FormFieldKind::File
                {
                    row.enabled = true;
                    rows.push(row);
                }
            } else {
                rows[i] = row;
            }
        }
    }
    let table = Rect::from_min_max(pos2(left, top), pos2(left + width, y));
    for x in &xs[..4] {
        ui.painter().vline(*x, table.y_range(), Stroke::new(1.0, p.border));
    }
    ui.painter().rect_stroke(table, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.advance_cursor_after_rect(table);
    let _ = id;
    if let Some(i) = remove {
        rows.remove(i);
        changed = true;
    }
    changed
}

/// A file picked as the whole request body.
fn binary_body(ui: &mut Ui, body: &mut Body) {
    let p = theme::pal(ui);
    let path = body.binary_path.trim().to_string();
    let meta = (!path.is_empty()).then(|| std::fs::metadata(&path).ok()).flatten();
    kit::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
            let color = if path.is_empty() || meta.is_some() { p.text_muted } else { p.err };
            ui.painter().rect_filled(rect, CornerRadius::same(8), p.subtle);
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, icons::FILE, theme::icon_font(20.0), color);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if path.is_empty() {
                    ui.label(RichText::new(tr("No file selected")).font(theme::medium(13.5)));
                    ui.label(kit::hint(ui, tr("The file's bytes are sent as the request body.")));
                } else {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone());
                    ui.label(RichText::new(name).font(theme::medium(13.5)));
                    match &meta {
                        Some(m) => ui.label(kit::hint(ui, &merpati_core::util::human_bytes(m.len()))),
                        None => ui.label(RichText::new(tr("File not found")).small().color(p.err)),
                    };
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !path.is_empty() && kit::icon_button(ui, icons::X, tr("Remove file")).clicked() {
                    body.binary_path.clear();
                }
                let label = if path.is_empty() { tr("Choose file…") } else { tr("Change…") };
                if kit::secondary(ui, Some(icons::UPLOAD_SIMPLE), label).clicked()
                    && let Some(f) = rfd::FileDialog::new().pick_file()
                {
                    body.binary_path = f.display().to_string();
                }
            });
        });
        ui.add_space(10.0);
        ui.add(
            egui::TextEdit::singleline(&mut body.binary_path)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace)
                .margin(egui::Margin::symmetric(8, 5))
                .hint_text("/path/to/file"),
        );
    });
}

fn graphql_editor(ui: &mut Ui, tab: &mut RequestTab, body: &mut Body, ectx: &EditCtx, actions: &mut Actions) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.set_min_height(28.0);
        if tab.graphql.loading {
            ui.add(egui::Spinner::new().size(14.0).color(p.accent));
            ui.label(widgets::muted(ui, tr("Fetching schema…")));
        } else if kit::button_ex(ui, kit::Kind::Secondary, Some(icons::ARROWS_CLOCKWISE), tr("Fetch schema"), true)
            .on_hover_text(tr("Run an introspection query against the URL"))
            .clicked()
        {
            actions.fetch_schema = true;
        }
        if let Some(schema) = &tab.graphql.schema {
            let types = schema.types.len();
            kit::pill(ui, &format!("{} {}", icons::CHECK, trf("{} types", &[&types])), p.ok);
        }
        if let Some(e) = &tab.graphql.error {
            ui.add(
                egui::Label::new(RichText::new(format!("{} {e}", icons::WARNING_CIRCLE)).small().color(p.err))
                    .truncate(),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if tab.graphql.schema.is_some()
                && kit::icon_toggle(ui, icons::TREE_STRUCTURE, tr("Schema explorer"), tab.graphql.explorer_open)
                    .clicked()
            {
                tab.graphql.explorer_open = !tab.graphql.explorer_open;
            }
            if kit::ghost(ui, Some(icons::SPARKLE), tr("Prettify")).clicked() {
                body.graphql.query = prettify_graphql(&body.graphql.query);
            }
        });
    });
    ui.add_space(6.0);
    let rows = editor_rows(ui, 10).saturating_sub(1);
    let schema = tab.graphql.schema.clone();
    let show_explorer = tab.graphql.explorer_open && schema.is_some();
    ui.columns(if show_explorer { 3 } else { 2 }, |cols| {
        cols[0].label(kit::section_title(tr("Query")));
        let provider = |text: &str, cursor: usize| -> Option<(usize, Vec<widgets::CompletionItem>)> {
            let schema = schema.as_ref()?;
            let word_start = widgets::word_start(&text[..cursor], |c| c.is_alphanumeric() || c == '_');
            if word_start == cursor {
                return None;
            }
            let items: Vec<widgets::CompletionItem> = merpati_convert::graphql::suggest(schema, text, cursor)
                .into_iter()
                .map(|s| (s.label, s.insert, s.detail))
                .collect();
            Some((word_start, items))
        };
        widgets::code_editor_with(
            &mut cols[0],
            "gql-query",
            &mut body.graphql.query,
            Lang::GraphQl,
            Some(&ectx.vars),
            rows,
            false,
            Some(&provider),
        );
        cols[1].label(kit::section_title(tr("GraphQL Variables")));
        widgets::code_editor(
            &mut cols[1],
            "gql-vars",
            &mut body.graphql.variables,
            Lang::Json,
            Some(&ectx.vars),
            rows,
            false,
        );
        if show_explorer {
            let schema = schema.unwrap();
            let ui = &mut cols[2];
            ui.label(kit::section_title(tr("Schema")));
            kit::search_field(ui, "gql-filter", &mut tab.graphql.filter, tr("Filter fields"), ui.available_width());
            ui.add_space(4.0);
            let filter = tab.graphql.filter.to_lowercase();
            egui::ScrollArea::vertical().id_salt("gql-explorer").max_height(rows as f32 * 17.0).show(ui, |ui| {
                for (op, label) in [("query", "Query"), ("mutation", "Mutation"), ("subscription", "Subscription")] {
                    let Some(root_type) = schema.root(op).and_then(|r| schema.get(r)) else { continue };
                    let id = ui.make_persistent_id(("gql-root", op));
                    if !disclosure(ui, id, label, Some(root_type.fields.len()), op == "query") && filter.is_empty() {
                        continue;
                    }
                    for field in &root_type.fields {
                        if !filter.is_empty() && !field.name.to_lowercase().contains(&filter) {
                            continue;
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if widgets::small_icon_button(ui, icons::PLUS, tr("Insert an operation for this field"))
                                .clicked()
                            {
                                body.graphql.query = merpati_convert::graphql::skeleton(&schema, op, &field.name);
                                body.graphql.variables =
                                    merpati_convert::graphql::skeleton_variables(&schema, op, &field.name);
                            }
                            let name = RichText::new(&field.name).monospace();
                            let name = if field.deprecated { name.strikethrough().color(p.text_faint) } else { name };
                            ui.add(egui::Label::new(name).selectable(false));
                            ui.add(
                                egui::Label::new(RichText::new(field.ty.display()).monospace().color(p.text_muted))
                                    .selectable(false)
                                    .truncate(),
                            );
                        })
                        .response
                        .on_hover_text(field.description.clone().unwrap_or_default());
                    }
                    ui.add_space(4.0);
                }
            });
        }
    });
}

fn prettify_graphql(q: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for token in q.replace('{', " { ").replace('}', " } ").split_whitespace() {
        match token {
            "{" => {
                out.push_str(" {\n");
                depth += 1;
                out.push_str(&"  ".repeat(depth));
            }
            "}" => {
                depth = depth.saturating_sub(1);
                while out.ends_with(' ') {
                    out.pop();
                }
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&"  ".repeat(depth));
                out.push_str("}\n");
                out.push_str(&"  ".repeat(depth));
            }
            t => {
                if !out.is_empty() && !out.ends_with(' ') && !out.ends_with('\n') && !out.ends_with("  ") {
                    if t.starts_with('(') || t.starts_with(':') || out.ends_with('(') {
                    } else {
                        out.push('\n');
                        out.push_str(&"  ".repeat(depth));
                    }
                }
                out.push_str(t);
                if t.ends_with(':') || t.ends_with('(') || t.ends_with(',') {
                    out.push(' ');
                }
            }
        }
    }
    out.trim().to_string()
}

fn scripts_editor(
    ui: &mut Ui,
    tab: &mut RequestTab,
    scripts: &mut merpati_core::model::Scripts,
    vars: &VarCtx,
    actions: &mut Actions,
) {
    let has_response = tab.result.as_ref().is_some_and(|r| r.exec.response.is_some());
    let post = tab.script_event == ScriptEvent::PostResponse;
    script_event_bar_with(
        ui,
        &mut tab.script_event,
        scripts,
        tr("JavaScript with the Postman-compatible pm API"),
        if post { 150.0 } else { 0.0 },
        |ui| {
            if !post {
                return;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = ui
                    .add_enabled_ui(has_response, |ui| kit::ghost(ui, Some(icons::SPARKLE), tr("Generate tests")))
                    .inner;
                let r = if has_response {
                    r.on_hover_text(tr("Write tests for the status, time, content type and body shape of the response"))
                } else {
                    r.on_disabled_hover_text(tr("Send the request first: tests are generated from its response."))
                };
                if r.clicked() {
                    actions.generate_tests = true;
                }
            });
        },
    );
    script_panel(ui, tab.script_event, scripts, vars);
}

/// Pre-request / Post-response switch with a dot on the scripts that have
/// code, and a line of help. Shared with the folder and collection settings.
pub(crate) fn script_event_bar(
    ui: &mut Ui,
    event: &mut ScriptEvent,
    scripts: &merpati_core::model::Scripts,
    help: &str,
) {
    script_event_bar_with(ui, event, scripts, help, 0.0, |_| {});
}

/// [`script_event_bar`] with controls (`trailing_w` wide) on the right of
/// the row.
fn script_event_bar_with(
    ui: &mut Ui,
    event: &mut ScriptEvent,
    scripts: &merpati_core::model::Scripts,
    help: &str,
    trailing_w: f32,
    trailing: impl FnOnce(&mut Ui),
) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        let pre = tr("Pre-request");
        let post = tr("Post-response");
        let r = ui.scope(|ui| {
            kit::segmented(ui, event, &[(ScriptEvent::PreRequest, pre), (ScriptEvent::PostResponse, post)]);
        });
        // Dots on segments with code (segments are text width + 20).
        let seg = |text: &str| {
            ui.painter()
                .layout_no_wrap(text.to_string(), theme::medium(small_size(ui) + 0.5), Color32::PLACEHOLDER)
                .size()
                .x
                + 20.0
        };
        let (w_pre, w_post) = (seg(pre), seg(post));
        let rect = r.response.rect;
        for (has, right) in [
            (!scripts.pre_request.trim().is_empty(), rect.left() + 2.0 + w_pre),
            (!scripts.post_response.trim().is_empty(), rect.left() + 2.0 + w_pre + w_post),
        ] {
            if has {
                ui.painter().circle_filled(pos2(right - 6.0, rect.top() + 7.0), 2.5, p.ok);
            }
        }
        ui.add_space(4.0);
        if trailing_w == 0.0 {
            ui.add(egui::Label::new(kit::hint(ui, help)).truncate());
        } else {
            // The help gives way to the trailing controls.
            let help_w = ui.available_width() - trailing_w - 8.0;
            if help_w > 60.0 {
                ui.allocate_ui_with_layout(
                    vec2(help_w, 30.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(kit::hint(ui, help)).truncate());
                    },
                );
            }
            trailing(ui);
        }
    });
    ui.add_space(8.0);
}

/// Script editor with snippets, shared with folder/collection tabs.
pub fn script_panel(ui: &mut Ui, event: ScriptEvent, scripts: &mut merpati_core::model::Scripts, vars: &VarCtx) {
    let code = match event {
        ScriptEvent::PreRequest => &mut scripts.pre_request,
        ScriptEvent::PostResponse => &mut scripts.post_response,
        ScriptEvent::OnMessage => &mut scripts.on_message,
    };
    let width = ui.available_width();
    let rows = editor_rows(ui, 10);
    let editor_id = ("script", event == ScriptEvent::PreRequest);
    let editor = |ui: &mut Ui, code: &mut String| {
        widgets::code_editor_with(
            ui,
            editor_id,
            code,
            Lang::JavaScript,
            Some(vars),
            rows,
            false,
            Some(&widgets::pm_completions),
        )
    };
    if width >= 640.0 {
        // Editor on the left, snippets in a side panel of the same height.
        let side_w = (width * 0.28).clamp(220.0, 280.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let editor_w = width - side_w - 12.0;
            let r = ui.allocate_ui_with_layout(vec2(editor_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(editor_w);
                editor(ui, code);
            });
            let h = r.response.rect.height();
            snippets_panel(ui, event, code, side_w, h);
        });
    } else {
        let r = kit::button_ex(ui, kit::Kind::Ghost, Some(icons::LIGHTNING), tr("Snippets"), true);
        egui::Popup::menu(&r).width(260.0).show(|ui| {
            for s in crate::snippets::SNIPPETS.iter().filter(|s| event == ScriptEvent::PostResponse || !s.post_only) {
                if ui.button(s.title).on_hover_text(RichText::new(s.code).monospace()).clicked() {
                    insert_snippet(code, s.code);
                    ui.close();
                }
            }
        });
        ui.add_space(4.0);
        editor(ui, code);
    }
}

fn insert_snippet(code: &mut String, snippet: &str) {
    if !code.is_empty() && !code.ends_with('\n') {
        code.push('\n');
    }
    code.push_str(snippet);
}

/// The snippet list and pm API reference beside a script editor.
fn snippets_panel(ui: &mut Ui, event: ScriptEvent, code: &mut String, width: f32, height: f32) {
    let p = theme::pal(ui);
    let filter_id = ui.make_persistent_id(("snippet-filter", event == ScriptEvent::PreRequest));
    let mut filter: String = ui.data(|d| d.get_temp(filter_id).unwrap_or_default());
    egui::Frame::new()
        .fill(p.chrome)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(width - 20.0);
            ui.set_height((height - 20.0).max(120.0));
            ui.vertical(|ui| snippets_list(ui, event, code, filter_id, &mut filter));
        });
}

fn snippets_list(ui: &mut Ui, event: ScriptEvent, code: &mut String, filter_id: egui::Id, filter: &mut String) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(icons::LIGHTNING).font(theme::icon_font(14.0)).color(p.accent_text));
        ui.label(RichText::new(tr("Snippets")).font(theme::semibold(13.0)));
    });
    ui.add_space(2.0);
    if kit::search_field(ui, filter_id.with("field"), filter, tr("Filter snippets"), ui.available_width()).changed() {
        ui.data_mut(|d| d.insert_temp(filter_id, filter.clone()));
    }
    ui.add_space(4.0);
    let needle = filter.to_lowercase();
    egui::ScrollArea::vertical().id_salt(("snips", event == ScriptEvent::PreRequest)).auto_shrink([false; 2]).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut any = false;
            for s in crate::snippets::SNIPPETS
                .iter()
                .filter(|s| event == ScriptEvent::PostResponse || !s.post_only)
                .filter(|s| needle.is_empty() || s.title.to_lowercase().contains(&needle))
            {
                any = true;
                if list_row(ui, icons::PLUS, s.title, false).on_hover_text(RichText::new(s.code).monospace()).clicked()
                {
                    insert_snippet(code, s.code);
                }
            }
            if !any {
                ui.label(kit::hint(ui, tr("No snippets match.")));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::BOOK_OPEN).font(theme::icon_font(13.0)).color(p.text_muted));
                ui.label(RichText::new(tr("pm API")).font(theme::medium(12.0)).color(p.text_muted));
            });
            ui.add_space(4.0);
            for (sig, doc) in crate::snippets::PM_COMPLETIONS {
                if needle.is_empty() || sig.to_lowercase().contains(&needle) {
                    ui.add(egui::Label::new(RichText::new(*sig).monospace().size(10.5).color(p.text_muted)).truncate())
                        .on_hover_text(*doc);
                    ui.add_space(3.0);
                }
            }
        },
    );
}

/// A clickable row of a side list: icon, text, hover fill (accent fill when
/// `selected`).
pub(crate) fn list_row(ui: &mut Ui, icon: &str, text: &str, selected: bool) -> egui::Response {
    let p = theme::pal(ui);
    let h = 26.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    widgets::a11y_button(&resp, text);
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        if selected {
            ui.painter().rect_filled(rect, CornerRadius::same(6), p.accent_soft);
        } else if hovered {
            ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover);
        }
        let icon_color = if hovered || selected { p.accent_text } else { p.text_faint };
        ui.painter().text(
            pos2(rect.left() + 12.0, rect.center().y),
            Align2::CENTER_CENTER,
            icon,
            theme::icon_font(13.0),
            icon_color,
        );
        let g = elide(ui, text, egui::FontId::proportional(small_size(ui) + 0.5), p.text, rect.width() - 32.0);
        ui.painter().galley(pos2(rect.left() + 26.0, rect.center().y - g.size().y / 2.0), g, p.text);
    }
    resp
}

/// Number of settings this request overrides.
fn settings_overridden(s: &RequestSettings) -> usize {
    [
        s.follow_redirects.is_some(),
        s.follow_original_method.is_some(),
        s.follow_auth_header.is_some(),
        s.verify_ssl.is_some(),
        s.encode_url.is_some(),
        s.disable_cookies.is_some(),
        s.max_redirects.is_some(),
        s.timeout_ms.is_some(),
        s.http_version.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count()
}

/// An on/off setting that can also follow the app default.
fn tri_state(ui: &mut Ui, label: &str, help: &str, value: &mut Option<bool>, default: bool) {
    ui.push_id(label, |ui| {
        form_row(ui, label, help, |ui| {
            let default_label = if default { tr("Default (on)") } else { tr("Default (off)") };
            kit::segmented(ui, value, &[(None, default_label), (Some(true), tr("On")), (Some(false), tr("Off"))]);
        });
    });
}

/// A number setting; empty means the default.
fn number_setting<T: std::str::FromStr + ToString + Copy>(
    ui: &mut Ui,
    label: &str,
    help: &str,
    value: &mut Option<T>,
    default: &str,
    unit: &str,
) {
    form_row(ui, label, help, |ui| {
        ui.horizontal(|ui| {
            let mut v = value.map(|v| v.to_string()).unwrap_or_default();
            if ui
                .add(
                    egui::TextEdit::singleline(&mut v)
                        .hint_text(default)
                        .desired_width(90.0)
                        .margin(egui::Margin::symmetric(8, 5)),
                )
                .changed()
            {
                *value = v.trim().parse().ok();
            }
            if !unit.is_empty() {
                ui.label(widgets::muted(ui, unit));
            }
        });
    });
}

fn settings_editor(ui: &mut Ui, s: &mut RequestSettings) {
    let d = merpati_core::settings::RequestDefaults::default();
    let width = ui.available_width().min(860.0);
    ui.allocate_ui_with_layout(vec2(width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_width(width);
        ui.horizontal(|ui| {
            ui.set_height(26.0);
            ui.label(kit::hint(ui, tr("These settings apply to this request only and override the app settings.")));
            if settings_overridden(s) > 0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if kit::ghost(ui, Some(icons::ARROW_COUNTER_CLOCKWISE), tr("Reset to defaults")).clicked() {
                        let disabled = std::mem::take(&mut s.disabled_default_headers);
                        *s = RequestSettings { disabled_default_headers: disabled, ..Default::default() };
                    }
                });
            }
        });
        ui.add_space(8.0);
        settings_group(ui, tr("Redirects"), |ui| {
            tri_state(
                ui,
                tr("Follow redirects"),
                tr("Follow 3xx responses to their Location."),
                &mut s.follow_redirects,
                d.follow_redirects,
            );
            form_divider(ui);
            tri_state(
                ui,
                tr("Follow original HTTP method"),
                tr("Redirect with the original method instead of switching to GET."),
                &mut s.follow_original_method,
                false,
            );
            form_divider(ui);
            tri_state(
                ui,
                tr("Keep Authorization header on redirect"),
                tr("Also send it when the redirect goes to another host."),
                &mut s.follow_auth_header,
                false,
            );
            form_divider(ui);
            number_setting(
                ui,
                tr("Maximum redirects"),
                tr("Stop after this many redirects."),
                &mut s.max_redirects,
                &d.max_redirects.to_string(),
                "",
            );
        });
        ui.add_space(14.0);
        settings_group(ui, tr("Connection"), |ui| {
            tri_state(
                ui,
                tr("Verify SSL certificates"),
                tr("Reject servers whose TLS certificate is not trusted."),
                &mut s.verify_ssl,
                d.verify_ssl,
            );
            form_divider(ui);
            number_setting(
                ui,
                tr("Request timeout"),
                tr("Cancel the request after this long; 0 waits forever."),
                &mut s.timeout_ms,
                tr("default"),
                "ms",
            );
            form_divider(ui);
            form_row(ui, tr("HTTP version"), tr("HTTP/2 is used only with servers that support it."), |ui| {
                let mut v = if s.http_version == Some(HttpVersion::Auto) { None } else { s.http_version };
                if kit::segmented(
                    ui,
                    &mut v,
                    &[(None, tr("Auto")), (Some(HttpVersion::Http1), "HTTP/1.x"), (Some(HttpVersion::Http2), "HTTP/2")],
                ) {
                    s.http_version = v;
                }
            });
        });
        ui.add_space(14.0);
        settings_group(ui, tr("Request"), |ui| {
            tri_state(
                ui,
                tr("Encode URL automatically"),
                tr("Percent-encode reserved characters in the path and query."),
                &mut s.encode_url,
                d.encode_url,
            );
            form_divider(ui);
            tri_state(
                ui,
                tr("Disable cookie jar"),
                tr("Do not send stored cookies or keep the ones this response sets."),
                &mut s.disable_cookies,
                false,
            );
        });
        ui.add_space(8.0);
    });
}

fn docs_editor(ui: &mut Ui, item: &mut RequestItem) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        ui.label(kit::section_title(tr("Description")));
    });
    ui.add_space(2.0);
    widgets::code_editor(ui, "desc", &mut item.description, Lang::Text, None, 5, false);
    ui.label(kit::hint(ui, tr("Shown in the generated collection documentation.")));
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        ui.label(kit::section_title(tr("Examples")));
        if !item.examples.is_empty() {
            kit::pill(ui, &item.examples.len().to_string(), p.text_muted);
        }
    });
    ui.add_space(4.0);
    if item.examples.is_empty() {
        banner(
            ui,
            icons::BOOKMARK_SIMPLE,
            p.text_muted,
            tr("Use “Save as example” on a response to document it and to serve it from a mock server."),
        );
    }
    let dark = ui.visuals().dark_mode;
    let mut remove = None;
    for (i, ex) in item.examples.iter_mut().enumerate() {
        ui.push_id(&ex.id, |ui| {
            kit::card(ui).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let id = ui.make_persistent_id("ex-open");
                let mut open: bool = ui.data(|d| d.get_temp(id).unwrap_or(false));
                let head = ui.horizontal(|ui| {
                    ui.set_height(28.0);
                    let caret = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
                    ui.add(
                        egui::Label::new(RichText::new(caret).font(theme::icon_font(12.0)).color(p.text_muted))
                            .selectable(false),
                    );
                    kit::pill(ui, &ex.status.to_string(), theme::status_color(ex.status, dark));
                    ui.add(
                        egui::Label::new(RichText::new(&ex.name).font(theme::medium(13.5)))
                            .selectable(false)
                            .truncate(),
                    );
                    if let Some(l) = &ex.language {
                        ui.label(widgets::muted(ui, l.to_uppercase()).small());
                    }
                });
                let toggle = ui.interact(head.response.rect, id.with("head"), Sense::click());
                widgets::a11y_button(&toggle, &ex.name);
                if toggle.clicked() {
                    open = !open;
                    ui.data_mut(|d| d.insert_temp(id, open));
                }
                if open {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(widgets::muted(ui, tr("Name")));
                        ui.add(
                            egui::TextEdit::singleline(&mut ex.name)
                                .desired_width((ui.available_width() - 200.0).max(120.0))
                                .margin(egui::Margin::symmetric(8, 5)),
                        );
                        ui.label(widgets::muted(ui, tr("Status")));
                        let mut code = ex.status.to_string();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut code)
                                    .desired_width(50.0)
                                    .margin(egui::Margin::symmetric(8, 5)),
                            )
                            .changed()
                            && let Ok(c) = code.parse()
                        {
                            ex.status = c;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if kit::icon_button(ui, icons::TRASH, tr("Delete example")).clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                    ui.add_space(6.0);
                    let lang = match ex.language.as_deref() {
                        Some("json") => Lang::Json,
                        Some("xml") | Some("html") => Lang::Xml,
                        _ => Lang::Text,
                    };
                    widgets::code_editor(ui, "ex-body", &mut ex.body, lang, None, 6, false);
                }
            });
        });
        ui.add_space(6.0);
    }
    if let Some(i) = remove {
        item.examples.remove(i);
    }
}

fn subscription_panel(ui: &mut Ui, sub: &GqlSubscription, actions: &mut Actions) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.set_height(34.0);
        icon_tile(ui, icons::BROADCAST, if sub.done { p.text_muted } else { p.ok }, 28.0);
        ui.label(RichText::new(tr("Subscription")).font(theme::semibold(14.0)));
        let color = if sub.status.starts_with("Error") {
            p.err
        } else if sub.done {
            p.text_muted
        } else {
            p.ok
        };
        kit::pill(ui, &sub.status, color);
        kit::metric(ui, icons::CHATS, &trf("{} events", &[&sub.messages.len()]));
        kit::metric(ui, icons::TIMER, &merpati_core::util::human_ms(sub.started.elapsed().as_secs_f64() * 1000.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (icon, label) = if sub.done { (icons::X, tr("Close")) } else { (icons::STOP, tr("Stop")) };
            if kit::secondary(ui, Some(icon), label).clicked() {
                actions.cancel = true;
            }
        });
    });
    ui.add_space(6.0);
    if sub.messages.is_empty() {
        kit::empty_state(
            ui,
            icons::BROADCAST,
            tr("Waiting for events"),
            tr("Events the server sends appear here."),
            |_| {},
        );
        return;
    }
    egui::ScrollArea::vertical().id_salt("gql-sub").stick_to_bottom(true).auto_shrink([false; 2]).show(ui, |ui| {
        for (at, text) in &sub.messages {
            let t = chrono::DateTime::from_timestamp_millis(*at)
                .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S%.3f").to_string())
                .unwrap_or_default();
            kit::card(ui).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(format!("{} {t}", icons::ARROW_DOWN)).small().color(p.text_muted));
                ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true));
            });
            ui.add_space(6.0);
        }
    });
}

/// Maximize and minimize buttons of the response panel (added right to
/// left: minimize ends up rightmost).
fn panel_controls(ui: &mut Ui, panel: &mut PanelMode, side: bool) {
    let (min_icon, min_tip) =
        if side { (icons::CARET_RIGHT, tr("Minimize response")) } else { (icons::CARET_DOWN, tr("Minimize response")) };
    if kit::icon_button(ui, min_icon, min_tip).clicked() {
        *panel = PanelMode::Minimized;
    }
    let maximized = *panel == PanelMode::Maximized;
    let (icon, tip) =
        if maximized { (icons::ARROWS_IN, tr("Restore layout")) } else { (icons::ARROWS_OUT, tr("Maximize response")) };
    if kit::icon_toggle(ui, icon, tip, maximized).clicked() {
        *panel = if maximized { PanelMode::Normal } else { PanelMode::Maximized };
    }
}

/// [`panel_controls`] in the top right corner, over the content (empty
/// and loading states, which have no header row).
fn corner_controls(ui: &mut Ui, panel: &mut PanelMode, side: bool) {
    let r = ui.max_rect();
    let rect = Rect::from_min_max(pos2(r.right() - 64.0, r.top()), pos2(r.right(), r.top() + 28.0));
    let mut child =
        ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::right_to_left(egui::Align::Center)));
    child.spacing_mut().item_spacing.x = 4.0;
    panel_controls(&mut child, panel, side);
}

/// The minimized response panel: a bar with the outcome and the buttons
/// to bring it back (a narrow rail when the response is beside the request).
fn minimized_panel(ui: &mut Ui, tab: &mut RequestTab, side: bool) {
    let p = theme::pal(ui);
    let rect = ui.max_rect();
    let bar = ui.interact(rect, ui.make_persistent_id("resp-min-bar"), Sense::click());
    if bar.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let bar = bar.on_hover_text(tr("Show the response"));
    widgets::a11y_button(&bar, tr("Show the response"));
    let mut restore = bar.clicked();
    let resp = tab.result.as_ref().and_then(|r| r.exec.response.as_ref());
    let failed = tab.result.as_ref().is_some_and(|r| r.exec.error.is_some());
    if side {
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            if kit::icon_button(ui, icons::CARET_LEFT, tr("Restore layout")).clicked() {
                restore = true;
            }
            if kit::icon_button(ui, icons::ARROWS_OUT, tr("Maximize response")).clicked() {
                tab.panel = PanelMode::Maximized;
            }
            ui.add_space(4.0);
            if tab.running.is_some() {
                ui.add(egui::Spinner::new().size(14.0).color(p.accent));
            } else if let Some(resp) = resp {
                let color = theme::status_color(resp.status, ui.visuals().dark_mode);
                ui.label(RichText::new(resp.status.to_string()).font(theme::semibold(11.5)).color(color))
                    .on_hover_text(format!("{} {}", resp.status, resp.status_text));
            } else if failed {
                ui.label(RichText::new(icons::WARNING_CIRCLE).font(theme::icon_font(15.0)).color(p.err));
            }
        });
    } else {
        ui.horizontal(|ui| {
            ui.set_height(30.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add(egui::Label::new(RichText::new(tr("Response")).font(theme::semibold(13.5))).selectable(false));
            if tab.running.is_some() {
                ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                ui.label(widgets::muted(ui, tr("Sending request…")));
            } else if let Some(resp) = resp {
                kit::status_pill(ui, resp.status, &resp.status_text);
                kit::metric(ui, icons::TIMER, &merpati_core::util::human_ms(resp.timings.total_ms));
                kit::metric(ui, icons::DOWNLOAD_SIMPLE, &merpati_core::util::human_bytes(resp.total_size()));
                if let Some(r) = &tab.result
                    && !r.exec.tests.is_empty()
                {
                    let (passed, failed, _) = test_counts(&r.exec.tests);
                    let color = if failed > 0 { p.err } else { p.ok };
                    ui.label(
                        RichText::new(format!("{} {passed}/{}", icons::TEST_TUBE, passed + failed))
                            .font(theme::medium(small_size(ui) + 0.5))
                            .color(color),
                    )
                    .on_hover_text(tr("Test Results"));
                }
            } else if let Some(err) = tab.result.as_ref().and_then(|r| r.exec.error.as_ref()) {
                kit::pill(ui, &format!("{} {}", icons::WARNING_CIRCLE, tr(err.kind_label())), p.err);
            } else {
                ui.label(widgets::muted(ui, tr("No response yet")));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if kit::icon_button(ui, icons::CARET_UP, tr("Restore layout")).clicked() {
                    restore = true;
                }
                if kit::icon_button(ui, icons::ARROWS_OUT, tr("Maximize response")).clicked() {
                    tab.panel = PanelMode::Maximized;
                }
            });
        });
    }
    if restore {
        tab.panel = PanelMode::Normal;
    }
}

fn response_panel(ui: &mut Ui, tab: &mut RequestTab, ectx: &EditCtx, actions: &mut Actions) {
    if tab.panel == PanelMode::Minimized {
        minimized_panel(ui, tab, ectx.side_layout);
        return;
    }
    if let Some(sub) = &tab.gql_sub {
        subscription_panel(ui, sub, actions);
        return;
    }
    if tab.running.is_some() || tab.result.is_none() {
        corner_controls(ui, &mut tab.panel, ectx.side_layout);
    }
    if let Some(r) = &tab.running {
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            ui.add(egui::Spinner::new().size(22.0).color(theme::pal(ui).accent));
            ui.add_space(6.0);
            ui.label(RichText::new(tr("Sending request…")).font(theme::medium(13.5)));
            ui.label(widgets::muted(ui, merpati_core::util::human_ms(r.started.elapsed().as_secs_f64() * 1000.0)));
            ui.add_space(6.0);
            if crate::kit::secondary(ui, Some(crate::icons::STOP), tr("Cancel")).clicked() {
                actions.cancel = true;
            }
        });
        return;
    }
    let Some(result) = &mut tab.result else {
        crate::kit::empty_state(
            ui,
            crate::icons::PAPER_PLANE_RIGHT,
            tr("No response yet"),
            tr("Enter a URL and press Send to see the response here."),
            |ui| {
                ui.horizontal(|ui| {
                    let w = 140.0;
                    ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                    crate::kit::kbd(ui, "⌘↩");
                    ui.label(widgets::muted(ui, tr("to send")));
                });
            },
        );
        return;
    };
    let exec = &result.exec;
    let tests_total = exec.tests.len();
    let tests_passed = exec.tests.iter().filter(|t| t.passed && !t.skipped).count();
    let resp_headers = exec.response.as_ref().map(|r| r.headers.len()).unwrap_or(0);
    let cookies = exec.response.as_ref().map(|r| r.headers_all("set-cookie").count()).unwrap_or(0);
    let tests_label = if tests_total == 0 && exec.script_errors.is_empty() {
        tr("Test Results").to_string()
    } else {
        format!("{} ({tests_passed}/{tests_total})", tr("Test Results"))
    };
    let items = vec![
        (RespSection::Body, tr("Body").to_string()),
        (RespSection::Headers, format!("{}{}", tr("Headers"), widgets::count_label(resp_headers))),
        (RespSection::Cookies, format!("{}{}", tr("Cookies"), widgets::count_label(cookies))),
        (RespSection::Tests, tests_label),
        (RespSection::Details, tr("Details").to_string()),
    ];
    let mut items = items;
    if exec.visualization.is_some() {
        items.push((RespSection::Visualize, tr("Visualize").to_string()));
    } else if tab.resp_section == RespSection::Visualize {
        tab.resp_section = RespSection::Body;
    }
    let comparable = tab.prev.is_some() && exec.response.is_some();
    if comparable {
        items.push((RespSection::Compare, tr("Compare").to_string()));
    } else if tab.resp_section == RespSection::Compare {
        tab.resp_section = RespSection::Body;
    }
    // Tabs and the status row share a line when both fit (the tabs'
    // width as `kit::tab_strip` measures it, the status row's estimated).
    let tabs_w: f32 = items
        .iter()
        .map(|(_, label)| {
            let font = theme::medium(body_size(ui));
            let w = ui.painter().layout_no_wrap(label.clone(), font, Color32::PLACEHOLDER).size().x;
            w + 22.0
        })
        .sum();
    let status_w = if exec.response.is_some() { 360.0 } else { 220.0 };
    let narrow = ui.available_width() < tabs_w + status_w;
    let panel = &mut tab.panel;
    let side = ectx.side_layout;
    let mut pin_baseline = false;
    let mut status_row = |ui: &mut Ui, actions: &mut Actions| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            panel_controls(ui, panel, side);
            kit::vdivider(ui, 18.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            if let Some(resp) = &exec.response {
                let more = crate::kit::icon_button(ui, crate::icons::DOTS_THREE, tr("More actions"));
                egui::Popup::menu(&more).show(|ui| {
                    if ui.button(crate::kit::with_icon(crate::icons::BOOKMARK_SIMPLE, tr("Save as example"))).clicked()
                    {
                        actions.save_example = true;
                        ui.close();
                    }
                    if ui
                        .button(crate::kit::with_icon(crate::icons::SPARKLE, tr("Generate tests")))
                        .on_hover_text(tr(
                            "Write tests for the status, time, content type and body shape of the response",
                        ))
                        .clicked()
                    {
                        actions.generate_tests = true;
                        ui.close();
                    }
                    if ui
                        .button(crate::kit::with_icon(crate::icons::GIT_DIFF, tr("Keep as compare baseline")))
                        .on_hover_text(tr("Compare the next responses with this one"))
                        .clicked()
                    {
                        pin_baseline = true;
                        ui.close();
                    }
                    if exec.prepared.is_some()
                        && ui
                            .button(crate::kit::with_icon(crate::icons::TERMINAL, tr("Copy as cURL (as sent)")))
                            .on_hover_text(tr(
                                "The request that produced this response, with variables and auth applied",
                            ))
                            .clicked()
                    {
                        actions.copy_curl = true;
                        ui.close();
                    }
                    if ui
                        .button(crate::kit::with_icon(crate::icons::ARROW_CLOCKWISE, tr("Re-run tests")))
                        .on_hover_text(tr("Run post-response scripts again on this response"))
                        .clicked()
                    {
                        actions.rerun_tests = true;
                        ui.close();
                    }
                    if exec.visualization.is_some()
                        && ui
                            .button(crate::kit::with_icon(
                                crate::icons::ARROW_SQUARE_OUT,
                                tr("Open visualization in browser"),
                            ))
                            .clicked()
                    {
                        open_visualization(exec);
                        ui.close();
                    }
                });
                let size = merpati_core::util::human_bytes(resp.total_size());
                let time = merpati_core::util::human_ms(resp.timings.total_ms);
                crate::kit::metric(ui, crate::icons::DOWNLOAD_SIMPLE, &size).on_hover_ui(|ui| {
                    ui.label(trf("Headers: {}", &[&merpati_core::util::human_bytes(resp.header_size)]));
                    let body = merpati_core::util::human_bytes(resp.body.len());
                    ui.label(if resp.body.is_file() {
                        trf("Body: {} (stored on disk)", &[&body])
                    } else {
                        trf("Body: {}", &[&body])
                    });
                });
                crate::kit::metric(ui, crate::icons::TIMER, &time).on_hover_ui(|ui| {
                    ui.label(trf("Time to first byte: {}", &[&merpati_core::util::human_ms(resp.timings.ttfb_ms)]));
                    ui.label(trf(
                        "Download: {}",
                        &[&merpati_core::util::human_ms(resp.timings.total_ms - resp.timings.ttfb_ms)],
                    ));
                    ui.label(trf("Total: {}", &[&merpati_core::util::human_ms(resp.timings.total_ms)]));
                });
                crate::kit::status_pill(ui, resp.status, &resp.status_text).on_hover_text(format!(
                    "{}{}",
                    resp.version,
                    resp.remote_addr.as_ref().map(|a| format!(" · {a}")).unwrap_or_default()
                ));
            } else if let Some(err) = &exec.error {
                let p = theme::pal(ui);
                kit::pill(ui, &format!("{} {}", icons::WARNING_CIRCLE, tr(err.kind_label())), p.err);
            } else if exec.skipped {
                let p = theme::pal(ui);
                kit::pill(ui, tr("Skipped"), p.text_muted);
            }
        });
    };
    if narrow {
        ui.horizontal(|ui| status_row(ui, actions));
        widgets::sub_tabs(ui, &mut tab.resp_section, &items);
    } else {
        ui.horizontal(|ui| {
            widgets::sub_tabs(ui, &mut tab.resp_section, &items);
            status_row(ui, actions);
        });
    }
    if pin_baseline && let Some(resp) = &exec.response {
        tab.prev = Some(Box::new(crate::response_diff::Snapshot { resp: resp.clone(), at: result.at }));
        tab.prev_pinned = true;
        tab.compare.reset();
        tab.resp_section = RespSection::Compare;
    }
    ui.add_space(8.0);
    match tab.resp_section {
        RespSection::Compare => {
            if let (Some(prev), Some(resp)) = (&tab.prev, &exec.response) {
                let cur = crate::response_diff::Snapshot { resp: resp.clone(), at: result.at };
                let acts = crate::response_diff::show(ui, &mut tab.compare, prev, &cur, tab.prev_pinned);
                if acts.toggle_pin {
                    tab.prev_pinned = !tab.prev_pinned;
                }
                if acts.maximize {
                    tab.panel = PanelMode::Maximized;
                }
            }
        }
        RespSection::Body => {
            if let Some(resp) = &exec.response {
                if let Some(a) = crate::body_view::show(ui, &mut result.body, resp, ectx.max_pretty) {
                    actions.value_action = Some(a.into());
                }
            } else if let Some(err) = &exec.error {
                error_state(ui, err, actions);
            } else if exec.skipped {
                kit::empty_state(
                    ui,
                    icons::ARROW_RIGHT,
                    tr("Request skipped"),
                    tr("The request was skipped by pm.execution.skipRequest()."),
                    |_| {},
                );
            }
        }
        RespSection::Headers => match &exec.response {
            Some(resp) => response_headers(ui, resp),
            None => no_response(ui),
        },
        RespSection::Cookies => match &exec.response {
            Some(resp) => response_cookies(ui, resp),
            None => no_response(ui),
        },
        RespSection::Tests => tests_view(
            ui,
            tab_tests_filter(&mut tab.test_filter),
            exec,
            &mut tab.section,
            &mut tab.script_event,
            actions,
        ),
        RespSection::Details => details_view(ui, exec),
        RespSection::Visualize => visualize_view(ui, &mut tab.viz, exec),
    }
}

/// Placeholder for response sections when the request failed before a
/// response arrived.
fn no_response(ui: &mut Ui) {
    kit::empty_state(
        ui,
        icons::WARNING_CIRCLE,
        tr("No response"),
        tr("The request failed before a response arrived; see Body for the error."),
        |_| {},
    );
}

/// What to try next for a failed request.
fn error_hint(kind: merpati_http::error::HttpErrorKind) -> Option<&'static str> {
    use merpati_http::error::HttpErrorKind as K;
    Some(match kind {
        K::InvalidUrl => tr("Check the URL: variables must resolve to a full http(s) address."),
        K::InvalidHeader => tr("A header name or value contains characters that are not allowed."),
        K::Dns => tr("Check the host name and your network connection."),
        K::ConnectionRefused => tr("Check that the server is running and that the host and port are right."),
        K::ConnectionReset => tr("The server closed the connection. Try again, or check the server logs."),
        K::Timeout => tr("The server did not answer in time. Try again or raise the timeout in Settings."),
        K::Certificate => tr("For self-signed certificates, turn off SSL verification in the request Settings."),
        K::Tls => tr("Check that the server speaks HTTPS on this port."),
        K::Proxy => tr("Check the proxy settings."),
        K::TooManyRedirects => tr("Raise the maximum redirects in Settings, or turn off following redirects."),
        K::File => tr("Check that the body files still exist and can be read."),
        _ => return None,
    })
}

/// The request could not be sent: icon, cause, next step, details.
fn error_state(ui: &mut Ui, err: &merpati_http::error::HttpError, actions: &mut Actions) {
    let p = theme::pal(ui);
    let detail_id = ui.make_persistent_id("err-detail");
    let mut show_detail: bool = ui.data(|d| d.get_temp(detail_id).unwrap_or(false));
    egui::ScrollArea::vertical().id_salt("err-scroll").auto_shrink([false; 2]).show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.12).clamp(8.0, 60.0));
            let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 28.0, theme::soft(p.err, 0.12));
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                icons::WARNING_CIRCLE,
                theme::icon_font(26.0),
                p.err,
            );
            ui.add_space(10.0);
            ui.label(RichText::new(tr("Could not send request")).font(theme::semibold(15.0)));
            ui.add_space(4.0);
            kit::pill(ui, tr(err.kind_label()), p.err);
            ui.add_space(6.0);
            let w = ui.available_width().min(560.0);
            ui.allocate_ui_with_layout(vec2(w, 0.0), egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.label(RichText::new(&err.message).color(p.text));
                if let Some(hint) = error_hint(err.kind) {
                    ui.add_space(2.0);
                    ui.label(RichText::new(hint).color(p.text_muted));
                }
            });
            ui.add_space(12.0);
            let has_detail = err.detail != err.message;
            let w = if has_detail { 260.0 } else { 120.0 };
            ui.allocate_ui_with_layout(vec2(w, 30.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if kit::secondary(ui, Some(icons::ARROW_CLOCKWISE), tr("Try again")).clicked() {
                    actions.send = true;
                }
                if has_detail {
                    let label = if show_detail { tr("Hide details") } else { tr("Show details") };
                    if kit::ghost(ui, Some(icons::TERMINAL), label).clicked() {
                        show_detail = !show_detail;
                        ui.data_mut(|d| d.insert_temp(detail_id, show_detail));
                    }
                }
            });
            if show_detail && has_detail {
                ui.add_space(10.0);
                let w = ui.available_width().min(680.0);
                ui.allocate_ui_with_layout(vec2(w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    egui::Frame::new()
                        .fill(p.chrome)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(
                                egui::Label::new(RichText::new(&err.detail).monospace().size(11.5).color(p.text_muted))
                                    .selectable(true),
                            );
                        });
                });
            }
        });
    });
}

/// Response headers as a read-only table with copy actions.
fn response_headers(ui: &mut Ui, resp: &merpati_core::response::HttpResponse) {
    if resp.headers.is_empty() {
        kit::empty_state(ui, icons::LIST, tr("No headers"), tr("The response has no headers."), |_| {});
        return;
    }
    let count =
        if resp.headers.len() == 1 { tr("1 header").to_string() } else { trf("{} headers", &[&resp.headers.len()]) };
    table_toolbar(ui, &count, || resp.headers.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n"));
    let rows: Vec<Vec<String>> = resp.headers.iter().map(|(k, v)| vec![k.clone(), v.clone()]).collect();
    egui::ScrollArea::vertical().id_salt("resp-headers").auto_shrink([false; 2]).show(ui, |ui| {
        read_table(ui, "resp-headers", &[tr("Key"), tr("Value")], &[0.32, 0.68], &rows);
    });
}

/// A muted count on the left and "Copy all" on the right, above a table.
fn table_toolbar(ui: &mut Ui, count: &str, all_text: impl FnOnce() -> String) {
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        ui.label(widgets::muted(ui, count).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if kit::ghost(ui, Some(icons::COPY), tr("Copy all")).clicked() {
                ui.ctx().copy_text(all_text());
            }
        });
    });
    ui.add_space(4.0);
}

/// `Set-Cookie` headers split into name, value and attributes.
fn parse_set_cookie(c: &str) -> Vec<String> {
    let first = c.split(';').next().unwrap_or("");
    let (n, v) = first.split_once('=').unwrap_or((first, ""));
    let attrs = c.split(';').skip(1).map(str::trim).filter(|a| !a.is_empty()).collect::<Vec<_>>().join(" · ");
    vec![n.trim().to_string(), v.trim().to_string(), attrs]
}

fn response_cookies(ui: &mut Ui, resp: &merpati_core::response::HttpResponse) {
    let rows: Vec<Vec<String>> = resp.headers_all("set-cookie").map(parse_set_cookie).collect();
    if rows.is_empty() {
        kit::empty_state(ui, icons::COOKIE, tr("No cookies"), tr("No cookies were set by this response."), |_| {});
        return;
    }
    let count = if rows.len() == 1 { tr("1 cookie").to_string() } else { trf("{} cookies", &[&rows.len()]) };
    table_toolbar(ui, &count, || resp.headers_all("set-cookie").collect::<Vec<_>>().join("\n"));
    egui::ScrollArea::vertical().id_salt("resp-cookies").auto_shrink([false; 2]).show(ui, |ui| {
        read_table(ui, "resp-cookies", &[tr("Name"), tr("Value"), tr("Attributes")], &[0.24, 0.38, 0.38], &rows);
    });
}

/// `pm.visualizer.set` output rendered in the app (FR-P4); pages that need
/// scripts (charts) can still be opened in the browser.
fn visualize_view(ui: &mut Ui, cache: &mut Option<(u64, Result<merpati_html::Document, String>)>, exec: &Execution) {
    use std::hash::{Hash, Hasher};
    let Some(v) = &exec.visualization else { return };
    let p = theme::pal(ui);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.template.hash(&mut h);
    v.data.to_string().hash(&mut h);
    let key = h.finish();
    if cache.as_ref().map(|(k, _)| *k) != Some(key) {
        *cache = Some((key, merpati_html::parse(&v.template, &v.data).map_err(|e| e.to_string())));
    }
    let Some((_, doc)) = cache.as_mut() else { return };
    match doc {
        Ok(doc) => {
            // `@media` width queries follow the panel width.
            let width = merpati_html::Options::default().css_width(ui);
            if doc.restyle_needed(width) {
                *doc = doc.restyled(width);
            }
            if doc.needs_browser() {
                let mut open = false;
                banner_with(
                    ui,
                    icons::INFO,
                    p.info,
                    tr(
                        "This visualization uses scripts or external resources; open it in the browser for the full page.",
                    ),
                    |ui| open = kit::ghost(ui, Some(icons::ARROW_SQUARE_OUT), tr("Open in browser")).clicked(),
                );
                if open {
                    open_visualization(exec);
                }
                ui.add_space(8.0);
            }
            doc.show(ui, &mut |href: &str| {
                widgets::open_web_link(href);
            });
        }
        Err(e) => {
            banner(ui, icons::WARNING_CIRCLE, p.err, &format!("{}: {e}", tr("Visualizer template")));
        }
    }
}

fn tab_tests_filter(f: &mut TestFilter) -> &mut TestFilter {
    f
}

fn event_label(e: ScriptEvent) -> &'static str {
    match e {
        ScriptEvent::PreRequest => tr("Pre-request"),
        ScriptEvent::PostResponse => tr("Post-response"),
        ScriptEvent::OnMessage => tr("On message"),
    }
}

/// Pass/fail/skip counts of a list of tests.
pub(crate) fn test_counts(tests: &[TestResult]) -> (usize, usize, usize) {
    let skipped = tests.iter().filter(|t| t.skipped).count();
    let passed = tests.iter().filter(|t| t.passed && !t.skipped).count();
    (passed, tests.len() - passed - skipped, skipped)
}

/// Pills with the pass/fail/skip counts (zero counts are muted).
pub(crate) fn test_count_pills(ui: &mut Ui, passed: usize, failed: usize, skipped: usize) {
    let p = theme::pal(ui);
    let muted_if_zero = |n: usize, c: Color32| if n == 0 { p.text_muted } else { c };
    kit::pill(ui, &format!("{} {}", icons::CHECK, trf("{} passed", &[&passed])), muted_if_zero(passed, p.ok));
    kit::pill(ui, &format!("{} {}", icons::X, trf("{} failed", &[&failed])), muted_if_zero(failed, p.err));
    if skipped > 0 {
        kit::pill(ui, &format!("{} {}", icons::MINUS, trf("{} skipped", &[&skipped])), p.text_muted);
    }
}

/// A thin bar split into passed, failed and skipped parts.
pub(crate) fn test_ratio_bar(ui: &mut Ui, passed: usize, failed: usize, skipped: usize, width: f32) {
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let total = (passed + failed + skipped).max(1) as f32;
    ui.painter().rect_filled(rect, CornerRadius::same(3), p.subtle);
    let mut x = rect.left();
    let painter = ui.painter_at(rect);
    for (n, c) in [(passed, p.ok), (failed, p.err), (skipped, p.text_faint)] {
        if n == 0 {
            continue;
        }
        let w = rect.width() * n as f32 / total;
        painter.rect_filled(Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height())), CornerRadius::same(3), c);
        x += w;
    }
}

fn tests_view(
    ui: &mut Ui,
    filter: &mut TestFilter,
    exec: &Execution,
    section: &mut Section,
    script_event: &mut ScriptEvent,
    actions: &mut Actions,
) {
    let p = theme::pal(ui);
    if exec.tests.is_empty() && exec.script_errors.is_empty() && exec.response.is_none() {
        kit::empty_state(
            ui,
            icons::TEST_TUBE,
            tr("Tests did not run"),
            tr("Post-response tests run only when a response arrives."),
            |_| {},
        );
        return;
    }
    if exec.tests.is_empty() && exec.script_errors.is_empty() {
        kit::empty_state(
            ui,
            icons::TEST_TUBE,
            tr("No tests yet"),
            tr("Add pm.test(...) calls in the Post-response script to check the response."),
            |ui| {
                // Two buttons, centered.
                let w = 300.0;
                ui.allocate_ui_with_layout(vec2(w, 30.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if kit::secondary(ui, Some(icons::SPARKLE), tr("Generate tests"))
                        .on_hover_text(tr(
                            "Write tests for the status, time, content type and body shape of the response",
                        ))
                        .clicked()
                    {
                        actions.generate_tests = true;
                    }
                    if kit::secondary(ui, Some(icons::PLUS), tr("Write a test")).clicked() {
                        *section = Section::Scripts;
                        *script_event = ScriptEvent::PostResponse;
                    }
                });
            },
        );
        return;
    }
    let (passed, failed, skipped) = test_counts(&exec.tests);
    ui.horizontal(|ui| {
        ui.set_height(28.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        test_count_pills(ui, passed, failed, skipped);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if kit::ghost(ui, Some(icons::ARROW_CLOCKWISE), tr("Re-run tests"))
                .on_hover_text(tr("Run post-response scripts again on this response"))
                .clicked()
            {
                actions.rerun_tests = true;
            }
        });
    });
    ui.add_space(6.0);
    test_ratio_bar(ui, passed, failed, skipped, ui.available_width());
    ui.add_space(10.0);
    if !exec.tests.is_empty() {
        let all = format!("{} {}", tr("All"), exec.tests.len());
        let pass = format!("{} {passed}", tr("Passed"));
        let fail = format!("{} {failed}", tr("Failed"));
        let skip = format!("{} {skipped}", tr("Skipped"));
        kit::segmented(
            ui,
            filter,
            &[
                (TestFilter::All, all.as_str()),
                (TestFilter::Passed, pass.as_str()),
                (TestFilter::Failed, fail.as_str()),
                (TestFilter::Skipped, skip.as_str()),
            ],
        );
        ui.add_space(8.0);
    }
    egui::ScrollArea::vertical().id_salt("tests").auto_shrink([false; 2]).show(ui, |ui| {
        for e in &exec.script_errors {
            banner(
                ui,
                icons::WARNING_CIRCLE,
                p.err,
                &format!("{}\n{}", trf("Script error in {} ({})", &[&event_label(e.event), &e.location]), e.message),
            );
            ui.add_space(8.0);
        }
        let shown: Vec<&TestResult> = exec
            .tests
            .iter()
            .filter(|t| match filter {
                TestFilter::All => true,
                TestFilter::Passed => t.passed && !t.skipped,
                TestFilter::Failed => !t.passed && !t.skipped,
                TestFilter::Skipped => t.skipped,
            })
            .collect();
        if shown.is_empty() && !exec.tests.is_empty() {
            ui.add_space(8.0);
            ui.label(widgets::muted(ui, tr("No tests match this filter.")));
            return;
        }
        test_list(ui, &shown);
    });
}

/// Tests as rows in one bordered list.
pub(crate) fn test_list(ui: &mut Ui, tests: &[&TestResult]) {
    if tests.is_empty() {
        return;
    }
    let p = theme::pal(ui);
    let top = ui.cursor().min;
    let width = ui.available_width();
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (i, t) in tests.iter().enumerate() {
            if i > 0 {
                let y = ui.cursor().min.y;
                ui.painter().hline(top.x..=top.x + width, y, Stroke::new(1.0, p.border));
            }
            test_row(ui, t, width);
        }
    });
    let rect = Rect::from_min_max(top, pos2(top.x + width, ui.min_rect().bottom()));
    ui.painter().rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
}

/// One test: status icon, name, PASS/FAIL/SKIP pill, failure message under it.
fn test_row(ui: &mut Ui, t: &TestResult, width: f32) {
    let p = theme::pal(ui);
    let (icon, color, label) = if t.skipped {
        (icons::MINUS, p.text_muted, tr("SKIP"))
    } else if t.passed {
        (icons::CHECK_CIRCLE, p.ok, tr("PASS"))
    } else {
        (icons::X_CIRCLE, p.err, tr("FAIL"))
    };
    let text_x = 36.0;
    let text_w = (width - text_x - 76.0).max(60.0);
    let name = ui.painter().layout(t.name.clone(), egui::FontId::proportional(body_size(ui)), p.text, text_w);
    let err = t
        .error
        .as_ref()
        .filter(|e| !e.is_empty())
        .map(|e| ui.painter().layout(e.clone(), theme::mono(mono_size(ui) - 0.5), p.text_muted, text_w));
    let h = (9.0 + name.size().y + err.as_ref().map_or(0.0, |g| g.size().y + 3.0) + 9.0).max(34.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect.shrink(1.0), CornerRadius::same(7), p.faint_row());
    }
    let first_line = rect.top() + 9.0 + name.rows.first().map_or(name.size().y, |r| r.height()) / 2.0;
    ui.painter().text(pos2(rect.left() + 18.0, first_line), Align2::CENTER_CENTER, icon, theme::icon_font(16.0), color);
    ui.painter().galley(pos2(rect.left() + text_x, rect.top() + 9.0), name.clone(), p.text);
    if let Some(g) = err {
        ui.painter().galley(pos2(rect.left() + text_x, rect.top() + 9.0 + name.size().y + 3.0), g, p.text_muted);
    }
    paint_pill(ui, pos2(rect.right() - 12.0, first_line), label, color);
    widgets::a11y_button(&resp, &format!("{label} {}", t.name));
    resp.context_menu(|ui| {
        if ui.button(kit::with_icon(icons::COPY, tr("Copy test name"))).clicked() {
            ui.ctx().copy_text(t.name.clone());
            ui.close();
        }
        if let Some(e) = &t.error
            && ui.button(kit::with_icon(icons::COPY, tr("Copy message"))).clicked()
        {
            ui.ctx().copy_text(e.clone());
            ui.close();
        }
    });
}

/// Paints a pill whose right edge is centered vertically on `right_center`.
pub(crate) fn paint_pill(ui: &Ui, right_center: egui::Pos2, text: &str, color: Color32) -> Rect {
    let dark = ui.visuals().dark_mode;
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::medium(small_size(ui) - 0.5), color);
    let size = galley.size() + vec2(14.0, 6.0);
    let rect = Rect::from_min_size(pos2(right_center.x - size.x, right_center.y - size.y / 2.0), size);
    ui.painter().rect_filled(
        rect,
        CornerRadius::same((size.y / 2.0) as u8),
        theme::soft(color, if dark { 0.18 } else { 0.11 }),
    );
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    rect
}

/// Milliseconds with a precision that fits their size.
pub(crate) fn fmt_ms(ms: f64) -> String {
    if ms < 1.0 {
        format!("{ms:.2} ms")
    } else if ms < 10.0 {
        format!("{ms:.1} ms")
    } else {
        merpati_core::util::human_ms(ms)
    }
}

/// A small heading inside a view: icon and section title.
pub(crate) fn detail_heading(ui: &mut Ui, icon: &str, text: &str) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.set_height(24.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new(icon).font(theme::icon_font(14.0)).color(p.text_muted));
        ui.label(kit::section_title(text));
    });
    ui.add_space(4.0);
}

/// A label/value line inside a details card.
fn info_row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    let p = theme::pal(ui);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(140.0, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_width(140.0);
            ui.label(RichText::new(label).color(p.text_muted));
        });
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), 22.0),
            egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
            add,
        );
    });
}

/// Waterfall of the response timing phases.
fn timing_bars(ui: &mut Ui, ttfb: f64, total: f64) {
    let p = theme::pal(ui);
    let total = total.max(ttfb).max(0.001);
    let phases = [
        (
            tr("Waiting (TTFB)"),
            0.0,
            ttfb,
            p.info,
            tr("From sending the request to the first response byte: connecting, TLS and the server's work."),
        ),
        (tr("Content download"), ttfb, (total - ttfb).max(0.0), p.ok, tr("Receiving the response body.")),
    ];
    let label_w = 150.0;
    let value_w = 76.0;
    for (label, start, dur, color, help) in phases {
        ui.horizontal(|ui| {
            ui.set_height(26.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.allocate_ui_with_layout(vec2(label_w, 26.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_width(label_w);
                ui.label(RichText::new(label).color(p.text));
            });
            let bar_w = (ui.available_width() - value_w - 8.0).max(60.0);
            let (rect, resp) = ui.allocate_exact_size(vec2(bar_w, 26.0), Sense::hover());
            let track = Rect::from_center_size(rect.center(), vec2(rect.width(), 8.0));
            ui.painter().rect_filled(track, CornerRadius::same(4), p.subtle);
            let x0 = track.left() + track.width() * (start / total) as f32;
            let w = (track.width() * (dur / total) as f32).max(3.0).min(track.right() - x0);
            ui.painter().rect_filled(
                Rect::from_min_size(pos2(x0, track.top()), vec2(w, track.height())),
                CornerRadius::same(4),
                color,
            );
            resp.on_hover_text(help);
            ui.allocate_ui_with_layout(vec2(value_w, 26.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(fmt_ms(dur)).font(theme::mono(mono_size(ui))).color(p.text));
            });
        });
    }
    form_divider(ui);
    ui.horizontal(|ui| {
        ui.set_height(24.0);
        ui.label(RichText::new(tr("Total")).font(theme::medium(13.5)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(fmt_ms(total)).font(theme::semibold(14.0)));
        });
    });
}

fn details_view(ui: &mut Ui, exec: &Execution) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    egui::ScrollArea::vertical().id_salt("details").auto_shrink([false; 2]).show(ui, |ui| {
        let w = ui.available_width().min(920.0);
        ui.allocate_ui_with_layout(vec2(w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(w);
            if !exec.unresolved.is_empty() {
                banner(ui, icons::WARNING, p.warn, &trf("Undefined variables: {}", &[&exec.unresolved.join(", ")]));
                ui.add_space(12.0);
            }
            if let Some(resp) = &exec.response {
                detail_heading(ui, icons::TIMER, tr("Timing"));
                kit::card(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    timing_bars(ui, resp.timings.ttfb_ms, resp.timings.total_ms);
                });
                ui.add_space(16.0);
                detail_heading(ui, icons::DOWNLOAD_SIMPLE, tr("Response"));
                kit::card(ui).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 8.0;
                    info_row(ui, tr("Status"), |ui| {
                        kit::status_pill(ui, resp.status, &resp.status_text);
                    });
                    info_row(ui, tr("HTTP version"), |ui| {
                        ui.label(RichText::new(&resp.version).monospace());
                    });
                    if let Some(addr) = &resp.remote_addr {
                        info_row(ui, tr("Remote address"), |ui| {
                            ui.add(egui::Label::new(RichText::new(addr).monospace()).selectable(true));
                        });
                    }
                    info_row(ui, tr("Security"), |ui| {
                        if resp.url.starts_with("https://") {
                            ui.label(
                                RichText::new(format!("{} {}", icons::LOCK_SIMPLE, tr("Encrypted with TLS (HTTPS)")))
                                    .color(p.ok),
                            );
                        } else {
                            ui.label(
                                RichText::new(format!("{} {}", icons::WARNING, tr("Not encrypted (plain HTTP)")))
                                    .color(p.text_muted),
                            );
                        }
                    });
                    info_row(ui, tr("Response size"), |ui| {
                        ui.label(merpati_core::util::human_bytes(resp.total_size()));
                        ui.label(widgets::muted(
                            ui,
                            trf(
                                "headers {} · body {}",
                                &[
                                    &merpati_core::util::human_bytes(resp.header_size),
                                    &merpati_core::util::human_bytes(resp.body.len()),
                                ],
                            ),
                        ));
                    });
                    info_row(ui, tr("URL"), |ui| {
                        ui.add(egui::Label::new(RichText::new(&resp.url).monospace()).selectable(true).wrap());
                    });
                });
                if !resp.redirects.is_empty() {
                    ui.add_space(16.0);
                    detail_heading(ui, icons::ARROW_BEND_UP_LEFT, tr("Redirects"));
                    kit::card(ui).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for (i, r) in resp.redirects.iter().chain(std::iter::once(&resp.url)).enumerate() {
                            ui.horizontal(|ui| {
                                let last = i == resp.redirects.len();
                                let n =
                                    RichText::new(format!("{}", i + 1)).font(theme::medium(11.5)).color(p.text_muted);
                                ui.add_sized([18.0, 20.0], egui::Label::new(n));
                                ui.add(egui::Label::new(RichText::new(r).monospace()).selectable(true).truncate());
                                if last {
                                    kit::status_pill(ui, resp.status, "");
                                }
                            });
                        }
                    });
                }
            }
            if let Some(sent) = &exec.sent {
                ui.add_space(16.0);
                detail_heading(ui, icons::PAPER_PLANE_RIGHT, tr("Request sent"));
                ui.horizontal(|ui| {
                    ui.label(widgets::method_badge(&sent.method, dark));
                    ui.add(egui::Label::new(RichText::new(&sent.url).monospace()).selectable(true).wrap());
                });
                if let Some(b) = sent.body_size {
                    ui.label(kit::hint(ui, &trf("Body: {}", &[&merpati_core::util::human_bytes(b)])));
                }
                ui.add_space(6.0);
                let rows: Vec<Vec<String>> = sent.headers.iter().map(|(k, v)| vec![k.clone(), v.clone()]).collect();
                read_table(ui, "sent-headers", &[tr("Key"), tr("Value")], &[0.32, 0.68], &rows);
            }
            if !exec.auth_notes.is_empty() {
                ui.add_space(16.0);
                detail_heading(ui, icons::KEY, tr("Authorization"));
                for n in &exec.auth_notes {
                    banner(ui, icons::INFO, p.info, n);
                    ui.add_space(4.0);
                }
            }
            if !exec.console.is_empty() {
                ui.add_space(16.0);
                detail_heading(ui, icons::TERMINAL, tr("Console"));
                egui::Frame::new()
                    .fill(p.chrome)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 3.0;
                        for l in &exec.console {
                            let color = match l.kind {
                                merpati_runner::ConsoleKind::Warn => p.warn,
                                merpati_runner::ConsoleKind::Error => p.err,
                                _ => p.text,
                            };
                            ui.horizontal_top(|ui| {
                                ui.label(RichText::new(&l.phase).monospace().size(11.0).color(p.text_faint));
                                ui.add(
                                    egui::Label::new(RichText::new(&l.message).monospace().size(11.5).color(color))
                                        .selectable(true)
                                        .wrap(),
                                );
                            });
                        }
                    });
            }
            ui.add_space(8.0);
        });
    });
}

fn open_visualization(exec: &Execution) {
    let Some(v) = &exec.visualization else { return };
    let Ok(html) = merpati_html::browser_html(&v.template, &v.data) else { return };
    let path = std::env::temp_dir().join(format!("merpati-visualize-{}.html", std::process::id()));
    if std::fs::write(&path, html).is_ok() {
        let _ = open::that(&path);
    }
}

/// Starts a GraphQL subscription over WebSocket (graphql-transport-ws; the
/// legacy graphql-ws protocol is accepted too).
fn start_subscription(
    app: &mut MerpatiApp,
    tab_id: u64,
    tab: &mut RequestTab,
    item: &RequestItem,
    collection_id: Option<&str>,
) {
    let Some(http) = item.http() else { return };
    let scopes = app.ws.scopes_for(collection_id);
    let vault = app.ws.vault();
    let resolver = Resolver::new(&scopes).with_vault(vault.as_ref());
    let auth = if http.auth.is_inherit() {
        collection_id
            .and_then(|c| app.ws.data.collection(c))
            .and_then(|c| c.effective_auth(&item.id).map(|e| e.auth.clone()))
            .unwrap_or_else(Auth::none)
    } else {
        http.auth.clone()
    };
    let ctx = merpati_core::prepared::PrepareContext {
        resolver,
        auth: &auth,
        auth_owner_id: Some(&item.id),
        defaults: &app.settings.request,
        base_dir: None,
    };
    let mut prepared = merpati_core::prepared::prepare(http, &ctx);
    if let Some(p) = merpati_auth::preview(&prepared) {
        prepared = p.request;
    }
    let mut url = prepared.url.clone();
    if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{rest}");
    } else if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{rest}");
    }
    let gql = &http.body.graphql;
    let variables: serde_json::Value =
        serde_json::from_str(&resolver.resolve(&gql.variables)).unwrap_or(serde_json::json!({}));
    let mut payload = serde_json::json!({"query": resolver.resolve(&gql.query), "variables": variables});
    if let Some(op) = gql.operation_name.as_ref().filter(|o| !o.is_empty()) {
        payload["operationName"] = serde_json::Value::String(op.clone());
    }
    let subscribe = serde_json::json!({"id": "1", "type": "subscribe", "payload": payload}).to_string();
    let headers = prepared
        .headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("content-type") && !k.eq_ignore_ascii_case("accept"))
        .cloned()
        .collect();
    let tls = match app.tls_setup(&url, prepared.settings.verify_ssl) {
        Ok(t) => t,
        Err(e) => {
            app.toast(e, true);
            return;
        }
    };
    let cfg = merpati_http::ws::WsConfig {
        url,
        headers,
        subprotocols: vec!["graphql-transport-ws".into(), "graphql-ws".into()],
        verify_ssl: prepared.settings.verify_ssl,
        tls,
        proxy: app.proxy_settings(),
        ..Default::default()
    };
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    tab.ws_epoch = crate::app::next_epoch();
    let epoch = tab.ws_epoch;
    let session = merpati_http::ws::WsSession::connect(app.rt.handle(), cfg, move |event| {
        let _ = tx.send(crate::app::Msg::Ws { tab: tab_id, epoch, event });
        ctx.request_repaint();
    });
    tab.result = None;
    tab.gql_sub = Some(GqlSubscription {
        session,
        subscribe,
        status: "Connecting…".into(),
        messages: Vec::new(),
        started: Instant::now(),
        done: false,
    });
}

fn fetch_schema(
    app: &mut MerpatiApp,
    tab_id: u64,
    tab: &mut RequestTab,
    item: &RequestItem,
    collection_id: Option<&str>,
) {
    let Some(http) = item.http() else { return };
    let mut req = http.clone();
    req.body = Body::default();
    req.body.mode = BodyMode::GraphQl;
    req.body.graphql.query = merpati_convert::graphql::INTROSPECTION_QUERY.to_string();
    req.method = "POST".into();
    let scopes = app.ws.scopes_for(collection_id);
    let auth = if req.auth.is_inherit() {
        collection_id
            .and_then(|c| app.ws.data.collection(c))
            .and_then(|c| c.effective_auth(&item.id).map(|e| e.auth.clone()))
            .unwrap_or_else(Auth::none)
    } else {
        req.auth.clone()
    };
    let services = app.services();
    tab.graphql.loading = true;
    tab.graphql.error = None;
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    let owner = item.id.clone();
    app.rt.spawn(async move {
        let resolver = Resolver::new(&scopes).with_vault(services.vault.as_deref().unwrap());
        let pctx = merpati_core::prepared::PrepareContext {
            resolver,
            auth: &auth,
            auth_owner_id: Some(&owner),
            defaults: &services.defaults,
            base_dir: None,
        };
        let mut prepared = merpati_core::prepared::prepare(&req, &pctx);
        let result = async {
            merpati_auth::apply(
                &mut prepared,
                &merpati_auth::AuthContext { tokens: services.tokens.as_ref(), http: services.http.as_ref() },
            )
            .await
            .map_err(|e| e.to_string())?;
            let out =
                services.http.send(&prepared, &merpati_http::SendOptions::default()).await.map_err(|e| e.message)?;
            let json: serde_json::Value =
                serde_json::from_slice(&out.response.body.read_all().map_err(|e| e.to_string())?)
                    .map_err(|e| format!("introspection response is not JSON: {e}"))?;
            merpati_convert::graphql::parse_introspection(&json).map_err(|e| e.to_string())
        }
        .await;
        let _ = tx.send(crate::app::Msg::Schema { tab: tab_id, result });
        ctx.request_repaint();
    });
}

// ---------------------------------------------------------------------------
// Building blocks shared by the request, runner, environment and container
// views (candidates for kit.rs).
// ---------------------------------------------------------------------------

pub(crate) fn body_size(ui: &Ui) -> f32 {
    ui.style().text_styles.get(&egui::TextStyle::Body).map(|f| f.size).unwrap_or(13.5)
}

pub(crate) fn small_size(ui: &Ui) -> f32 {
    ui.style().text_styles.get(&egui::TextStyle::Small).map(|f| f.size).unwrap_or(11.5)
}

pub(crate) fn mono_size(ui: &Ui) -> f32 {
    ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5)
}

/// One line of text cut with an ellipsis at `width`.
pub(crate) fn elide(
    ui: &Ui,
    text: &str,
    font: egui::FontId,
    color: Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    ui.painter().layout_job(job)
}

/// A collapsible section header (caret, title, optional count). Returns
/// whether it is open.
pub(crate) fn disclosure(ui: &mut Ui, id: egui::Id, title: &str, count: Option<usize>, default_open: bool) -> bool {
    let p = theme::pal(ui);
    let mut open: bool = ui.data(|d| d.get_temp(id).unwrap_or(default_open));
    let row = ui.horizontal(|ui| {
        ui.set_height(26.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        let caret = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
        ui.add(
            egui::Label::new(RichText::new(caret).font(theme::icon_font(12.0)).color(p.text_muted)).selectable(false),
        );
        ui.add(egui::Label::new(kit::section_title(title)).selectable(false));
        if let Some(n) = count {
            kit::pill(ui, &n.to_string(), p.text_muted);
        }
    });
    let r = ui.interact(row.response.rect, id.with("toggle"), Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, title));
    r.ctx.accesskit_node_builder(r.id, |n| n.set_expanded(open));
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if r.clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(id, open));
    }
    open
}

/// A message on a soft fill of `color` with an icon (info, warning, error).
pub(crate) fn banner(ui: &mut Ui, icon: &str, color: Color32, text: &str) {
    banner_with(ui, icon, color, text, |_| {});
}

/// [`banner`] with actions on the right.
pub(crate) fn banner_with(ui: &mut Ui, icon: &str, color: Color32, text: &str, actions: impl FnOnce(&mut Ui)) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    // The actions' width is measured on the previous frame so the text can
    // wrap in the space left of them.
    let width_id = ui.scope_id().with(("banner-actions", text));
    let actions_w: f32 = ui.data(|d| d.get_temp(width_id).unwrap_or(0.0));
    egui::Frame::new()
        .fill(theme::soft(color, if dark { 0.12 } else { 0.07 }))
        .stroke(Stroke::new(1.0, theme::soft(color, if dark { 0.30 } else { 0.24 })))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(RichText::new(icon).font(theme::icon_font(16.0)).color(color));
                // The actions' row always takes one item spacing, even when empty.
                let text_w = (ui.available_width() - actions_w - 8.0).max(80.0);
                ui.allocate_ui_with_layout(vec2(text_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(text_w);
                    ui.add_space(1.0);
                    ui.label(RichText::new(text).color(p.text));
                });
                let r = ui.horizontal(actions);
                let w = r.response.rect.width();
                if (w - actions_w).abs() > 0.5 {
                    ui.data_mut(|d| d.insert_temp(width_id, w));
                    ui.ctx().request_repaint();
                }
            });
        });
}

/// An icon on a soft rounded square (view headers, list items).
pub(crate) fn icon_tile(ui: &mut Ui, icon: &str, color: Color32, side: f32) -> egui::Response {
    let dark = ui.visuals().dark_mode;
    let (rect, resp) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same((side / 4.0) as u8),
        theme::soft(color, if dark { 0.16 } else { 0.10 }),
    );
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon_font((side * 0.55).round()), color);
    resp
}

/// Title row of a view: icon tile, title and subtitle, actions on the right
/// (added right to left).
pub(crate) fn view_header(
    ui: &mut Ui,
    icon: &str,
    color: Color32,
    title: &str,
    subtitle: &str,
    actions: impl FnOnce(&mut Ui),
) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.set_height(40.0);
        ui.spacing_mut().item_spacing.x = 10.0;
        icon_tile(ui, icon, color, 36.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.add(egui::Label::new(RichText::new(title).font(theme::semibold(16.0)).color(p.text)).truncate());
            if !subtitle.is_empty() {
                ui.add(egui::Label::new(RichText::new(subtitle).small().color(p.text_muted)).truncate());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            actions(ui);
        });
    });
}

/// A row in a settings form: label (and help under it) in a column on the
/// left, the control on the right. Stacks when the space is narrow.
pub(crate) fn form_row<R>(ui: &mut Ui, label: &str, help: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::pal(ui);
    let width = ui.available_width();
    let label_ui = |ui: &mut Ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(RichText::new(label).font(theme::medium(13.0)).color(p.text));
        if !help.is_empty() {
            ui.label(RichText::new(help).small().color(p.text_muted));
        }
    };
    if width < 480.0 {
        return ui
            .vertical(|ui| {
                label_ui(ui);
                ui.add_space(4.0);
                add(ui)
            })
            .inner;
    }
    let label_w = (width * 0.36).clamp(160.0, 260.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(label_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(label_w);
            ui.add_space(4.0);
            label_ui(ui);
        });
        ui.add_space(12.0);
        ui.vertical(|ui| add(ui)).inner
    })
    .inner
}

/// A hairline between form rows inside a card.
pub(crate) fn form_divider(ui: &mut Ui) {
    let p = theme::pal(ui);
    ui.add_space(6.0);
    let y = ui.cursor().top();
    let r = ui.max_rect();
    ui.painter().hline(r.left()..=r.right(), y, Stroke::new(1.0, p.border));
    ui.add_space(7.0);
}

/// A titled card of form rows.
pub(crate) fn settings_group(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_height(22.0);
        ui.label(kit::section_title(title));
    });
    ui.add_space(4.0);
    kit::card(ui).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

/// A read-only table in the style of `widgets::kv_table`: header row,
/// hairlines, wrapped cells, hover highlight, a copy button on the hovered
/// row and a context menu. `widths` are column fractions.
pub(crate) fn read_table(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    columns: &[&str],
    widths: &[f32],
    rows: &[Vec<String>],
) {
    let p = theme::pal(ui);
    let id = ui.make_persistent_id(id);
    const HEAD: f32 = 28.0;
    const COPY: f32 = 34.0;
    let width = ui.available_width().max(220.0);
    let left = ui.cursor().left();
    let top = ui.cursor().top();
    let inner = width - COPY;
    let mut xs = vec![left];
    for w in widths {
        let last = *xs.last().unwrap_or(&left);
        xs.push(last + inner * w);
    }
    let mono = theme::mono(mono_size(ui));
    let head = Rect::from_min_size(pos2(left, top), vec2(width, HEAD));
    ui.painter().rect_filled(head, CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, p.chrome);
    for (c, label) in columns.iter().enumerate() {
        ui.painter().text(
            pos2(xs[c] + 10.0, head.center().y),
            Align2::LEFT_CENTER,
            *label,
            theme::medium(11.5),
            p.text_muted,
        );
    }
    let mut y = head.bottom();
    for (i, row) in rows.iter().enumerate() {
        let galleys: Vec<_> = row
            .iter()
            .enumerate()
            .take(columns.len())
            .map(|(c, text)| {
                let w = (xs[c + 1] - xs[c] - 20.0).max(20.0);
                let color = if c == 0 {
                    p.text
                } else if c + 1 == columns.len() && columns.len() > 2 {
                    p.text_muted
                } else {
                    p.text
                };
                (ui.painter().layout(text.clone(), mono.clone(), color, w), color)
            })
            .collect();
        let h = (galleys.iter().map(|(g, _)| g.size().y).fold(0.0, f32::max) + 14.0).max(30.0);
        let row_rect = Rect::from_min_size(pos2(left, y), vec2(width, h));
        ui.painter().hline(left..=left + width, y, Stroke::new(1.0, p.border));
        let resp = ui.interact(row_rect, id.with(i), Sense::click());
        let hovered = ui.rect_contains_pointer(row_rect);
        if hovered {
            ui.painter().rect_filled(row_rect.shrink2(vec2(1.0, 0.5)), 0.0, p.faint_row());
        }
        for (c, (g, color)) in galleys.into_iter().enumerate() {
            ui.painter().galley(pos2(xs[c] + 10.0, y + 7.0), g, color);
        }
        let row_text = if row.len() == 2 { format!("{}: {}", row[0], row[1]) } else { row.join("\t") };
        if hovered {
            let d = Rect::from_center_size(pos2(left + width - COPY / 2.0, y + 15.0), vec2(24.0, 24.0));
            let r = ui
                .scope_builder(egui::UiBuilder::new().max_rect(d), |ui| {
                    kit::icon_button_ex(ui, icons::COPY, tr("Copy"), false, 24.0)
                })
                .inner;
            if r.clicked() {
                ui.ctx().copy_text(row_text.clone());
            }
        }
        resp.context_menu(|ui| {
            if row.len() > 1 && ui.button(kit::with_icon(icons::COPY, tr("Copy value"))).clicked() {
                ui.ctx().copy_text(row[1].clone());
                ui.close();
            }
            if ui.button(kit::with_icon(icons::COPY, tr("Copy name"))).clicked() {
                ui.ctx().copy_text(row[0].clone());
                ui.close();
            }
            if ui.button(kit::with_icon(icons::CLIPBOARD_TEXT, tr("Copy row"))).clicked() {
                ui.ctx().copy_text(row_text.clone());
                ui.close();
            }
        });
        y += h;
    }
    let table = Rect::from_min_max(pos2(left, top), pos2(left + width, y));
    for x in &xs[1..columns.len()] {
        ui.painter().vline(*x, table.y_range(), Stroke::new(1.0, p.border));
    }
    ui.painter().rect_stroke(table, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.advance_cursor_after_rect(table);
}

/// Number cards laid out in rows of equal width (at least `min` wide).
pub(crate) fn stat_cards(ui: &mut Ui, min: f32, cards: &[(&str, String, Option<Color32>, String)]) {
    if cards.is_empty() {
        return;
    }
    let gap = 10.0;
    let avail = ui.available_width();
    let per_row = (((avail + gap) / (min + gap)).floor() as usize).clamp(1, cards.len());
    let rows = cards.len().div_ceil(per_row);
    // Spread the cards evenly over the rows.
    let per_row = cards.len().div_ceil(rows);
    let w = ((avail - gap * (per_row as f32 - 1.0)) / per_row as f32).floor();
    for (r, chunk) in cards.chunks(per_row).enumerate() {
        if r > 0 {
            ui.add_space(gap);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (label, value, color, hint) in chunk {
                stat_card(ui, w, label, value, *color, hint);
            }
        });
    }
}

/// A number card: label, big value (colored when given) and a hint line.
pub(crate) fn stat_card(ui: &mut Ui, width: f32, label: &str, value: &str, color: Option<Color32>, hint: &str) {
    let p = theme::pal(ui);
    kit::card(ui).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
        // Inner margins and the 1 px stroke on both sides.
        ui.set_width(width - 30.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(egui::Label::new(RichText::new(label).small().color(p.text_muted)).truncate());
            ui.add(
                egui::Label::new(RichText::new(value).font(theme::semibold(20.0)).color(color.unwrap_or(p.text)))
                    .truncate(),
            );
            if !hint.is_empty() {
                ui.add(egui::Label::new(RichText::new(hint).small().color(p.text_faint)).truncate());
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_subscriptions() {
        assert!(starts_with_subscription("  subscription OnMsg { msg }"));
        assert!(starts_with_subscription("# comment\nsubscription { a }"));
        assert!(!starts_with_subscription("query { a }"));
    }

    #[test]
    fn beautify_keeps_variables() {
        let out = beautify_json_with_vars("{\"id\": {{id}}, \"name\":\"{{name}}\", \"a\":[1,2]}").unwrap();
        assert!(out.contains("\"id\": {{id}}"));
        assert!(out.contains("\"name\": \"{{name}}\""));
        assert!(beautify_json_with_vars("{bad").is_none());
    }

    /// A pasted cURL command becomes the request and reports what it could
    /// not import exactly (shown as a toast); other text is the URL.
    #[test]
    fn pasted_curl_reports_its_warnings() {
        let mut http = HttpRequest::default();
        let warnings = url_edited(&mut http, "curl --data-urlencode name@notes.txt https://api.test/x".into());
        assert_eq!(http.method, "POST");
        assert!(http.url.starts_with("https://api.test/x"), "{}", http.url);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("notes.txt"), "{warnings:?}");
        assert!(url_edited(&mut http, "curl https://api.test/y".into()).is_empty());
        assert!(url_edited(&mut http, "https://api.test/{{path}}".into()).is_empty());
        assert_eq!(http.url, "https://api.test/{{path}}");
    }
}
