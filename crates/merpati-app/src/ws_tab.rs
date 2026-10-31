//! WebSocket request tab (FR-X3): connect with headers, send text/binary,
//! timestamped message log, saved messages.

use std::collections::VecDeque;
use std::time::Instant;

use base64::Engine;
use egui::{RichText, Ui};

use merpati_core::model::{RequestItem, RequestKind, WsMessageKind, WsSavedMessage};
use merpati_core::prepared::{PrepareContext, prepare};
use merpati_core::protocols::SessionRunner;
use merpati_core::variables::Resolver;
use merpati_http::ws::{WsConfig, WsDirection, WsEvent, WsPayload, WsSession};

use crate::app::{ItemRef, MerpatiApp, Msg};
use crate::highlight::Lang;
use crate::i18n::{tr, trf};
use crate::proto_common::{self as pc, Conn, ConnAction, Dir, Log, LogLine, RowAction};
use crate::widgets::{self, KvOptions};
use crate::{icons, kit, theme};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Message,
    Params,
    Headers,
    Auth,
    Saved,
    Scripts,
}

#[derive(Clone, PartialEq)]
enum State {
    Disconnected,
    Connecting,
    Connected { protocol: Option<String>, status: u16 },
}

pub struct WsTab {
    /// Number of the current connection (see `app::next_epoch`).
    pub epoch: u64,
    pub location: Option<ItemRef>,
    pub scratch: Option<RequestItem>,
    session: Option<WsSession>,
    conn: State,
    log: Log,
    composer: String,
    binary: bool,
    section: Section,
    pub scripts: crate::proto_common::ScriptState,
    inbox: VecDeque<serde_json::Value>,
    /// When the connection opened, and how long the handshake took.
    since: Option<Instant>,
    handshake_ms: Option<f64>,
    /// Why the last connection failed (cleared when connecting again).
    error: Option<String>,
}

impl WsTab {
    fn base(location: Option<ItemRef>, scratch: Option<RequestItem>) -> Self {
        WsTab {
            epoch: 0,
            location,
            scratch,
            session: None,
            conn: State::Disconnected,
            log: Log::with_empty("No messages yet", "Connect, then send a message to see the conversation here."),
            composer: String::new(),
            binary: false,
            section: Section::Message,
            scripts: Default::default(),
            inbox: VecDeque::new(),
            since: None,
            handshake_ms: None,
            error: None,
        }
    }

    fn slot(&self) -> crate::proto_common::ItemSlot {
        crate::proto_common::ItemSlot { location: self.location.clone(), scratch: None }
    }

    pub fn scratch(item: RequestItem) -> Self {
        Self::base(None, Some(item))
    }

    pub fn saved(r: ItemRef) -> Self {
        Self::base(Some(r), None)
    }

    pub fn on_event(&mut self, e: WsEvent) {
        match e {
            WsEvent::Connecting => {
                self.conn = State::Connecting;
                self.error = None;
            }
            WsEvent::Connected { status, protocol, elapsed_ms, .. } => {
                self.conn = State::Connected { protocol: protocol.clone(), status };
                self.since = Some(Instant::now());
                self.handshake_ms = Some(elapsed_ms);
                self.error = None;
                let protocol = protocol.map(|p| trf(", protocol {}", &[&p])).unwrap_or_default();
                let took = merpati_core::util::human_ms(elapsed_ms);
                self.log.system(trf("Connected ({}) in {}{}", &[&status, &took, &protocol]));
            }
            WsEvent::Message { direction, payload, at_ms } => {
                let (text, binary) = match &payload {
                    WsPayload::Text(t) => (t.clone(), false),
                    WsPayload::Binary(_) => (payload.display(), true),
                };
                if direction == WsDirection::Incoming {
                    use merpati_core::script::protocol as proto;
                    let mut m = proto::message(proto::json_or_text(&text), proto::Direction::Incoming, at_ms);
                    m["binary"] = serde_json::json!(binary);
                    crate::proto_common::queue_message(&mut self.inbox, m);
                }
                let dir = if direction == WsDirection::Incoming { Dir::In } else { Dir::Out };
                let size = payload.len();
                self.log.push_line(LogLine {
                    dir,
                    title: String::new(),
                    text,
                    at_ms,
                    size,
                    error: false,
                    binary,
                    meta: String::new(),
                });
            }
            WsEvent::Error(msg) => {
                if !matches!(self.conn, State::Connected { .. }) {
                    self.error = Some(msg.clone());
                }
                self.log.error(trf("Error: {}", &[&msg]))
            }
            WsEvent::Closed { code, reason, by_peer } => {
                self.conn = State::Disconnected;
                self.session = None;
                self.since = None;
                let mut text = if by_peer { tr("Disconnected by the server") } else { tr("Disconnected") }.to_string();
                if let Some(c) = code {
                    text.push_str(&trf(" (code {})", &[&c]));
                }
                if !reason.is_empty() {
                    text.push_str(&format!(": {reason}"));
                }
                self.log.system(text);
            }
        }
    }

    fn state(&self) -> Conn {
        match self.conn {
            State::Connected { .. } => Conn::Live,
            State::Connecting => Conn::Busy,
            State::Disconnected if self.error.is_some() => Conn::Failed,
            State::Disconnected => Conn::Idle,
        }
    }
}

fn get_item(app: &MerpatiApp, tab: &WsTab) -> Option<RequestItem> {
    match &tab.location {
        Some(loc) => app.ws.data.collection(&loc.collection)?.find_request(&loc.item).cloned(),
        None => tab.scratch.clone(),
    }
}

/// Collapsible "Runner" settings of a WebSocket, Socket.IO or MQTT request:
/// when the Collection Runner and `merpati run` end the session.
pub(crate) fn runner_section(ui: &mut Ui, id: &str, runner: &mut SessionRunner, until_hint: &str) {
    let badge = (!runner.is_default()).then(|| tr("customized"));
    pc::collapsible(ui, (id, "runner"), tr("Runner"), badge, |ui| {
        ui.label(kit::hint(
            ui,
            tr("Collection Runner and merpati run: connect, send the saved messages in order, then stop at the first of:"),
        ));
        ui.add_space(6.0);
        pc::form_grid((id, "runner-grid")).show(ui, |ui| {
            let mut limit = runner.max_messages.is_some();
            if ui.checkbox(&mut limit, tr("Incoming messages")).changed() {
                runner.max_messages = limit.then_some(1);
            }
            match &mut runner.max_messages {
                Some(n) => ui.add(egui::DragValue::new(n).range(0..=1_000_000)),
                None => ui.label(widgets::muted(ui, tr("no limit"))),
            };
            ui.end_row();
            pc::form_label(ui, tr("Message that ends the session"));
            let mut until = runner.until_event.clone().unwrap_or_default();
            if ui.add(egui::TextEdit::singleline(&mut until).hint_text(until_hint).desired_width(260.0)).changed() {
                runner.until_event = (!until.trim().is_empty()).then_some(until);
            }
            ui.end_row();
            pc::form_label(ui, tr("Silence after the last message (ms, 0 = off)"));
            crate::widgets::a11y_name(
                &ui.add(egui::DragValue::new(&mut runner.wait_ms).range(0..=3_600_000).speed(10)),
                tr("Silence after the last message (ms, 0 = off)"),
            );
            ui.end_row();
            pc::form_label(ui, tr("Session timeout (ms, 0 = none)"));
            crate::widgets::a11y_name(
                &ui.add(egui::DragValue::new(&mut runner.timeout_ms).range(0..=86_400_000).speed(100)),
                tr("Session timeout (ms, 0 = none)"),
            );
            ui.end_row();
            if id == "sio" {
                pc::form_label(ui, tr("Answer server acknowledgements with"));
                let mut reply = runner.ack_reply.clone().unwrap_or_default();
                let edit = egui::TextEdit::singleline(&mut reply)
                    .hint_text(tr("JSON array of arguments; empty = no answer"))
                    .desired_width(260.0);
                if ui.add(edit).changed() {
                    runner.ack_reply = (!reply.trim().is_empty()).then_some(reply);
                }
                ui.end_row();
            }
        });
        if !runner.is_default() {
            ui.add_space(4.0);
            if kit::ghost(ui, Some(icons::ARROW_COUNTER_CLOCKWISE), tr("Reset to defaults")).clicked() {
                *runner = SessionRunner::default();
            }
        }
    });
}

fn parse_binary(text: &str) -> Option<Vec<u8>> {
    let t = text.trim();
    let hex: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    if hex.len().is_multiple_of(2) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect();
    }
    base64::engine::general_purpose::STANDARD.decode(t).ok()
}

fn connect(app: &mut MerpatiApp, tab_id: u64, tab: &mut WsTab, item: &RequestItem) {
    let RequestKind::WebSocket(ws) = &item.request else { return };
    let collection_id = tab.location.as_ref().map(|l| l.collection.clone());
    let scopes = app.ws.scopes_for(collection_id.as_deref());
    let vault = app.ws.vault();
    let resolver = Resolver::new(&scopes).with_vault(vault.as_ref());
    // Reuse the HTTP preparation for variables, query params and auth headers.
    let auth = if ws.auth.is_inherit() {
        collection_id
            .as_deref()
            .and_then(|c| app.ws.data.collection(c))
            .and_then(|c| c.effective_auth(&item.id).map(|e| e.auth.clone()))
            .unwrap_or_else(merpati_core::auth::Auth::none)
    } else {
        ws.auth.clone()
    };
    let mut http = merpati_core::model::HttpRequest::new("GET", ws.url.clone());
    http.params = ws.params.clone();
    http.sync_url_from_params();
    http.headers = ws.headers.clone();
    let mut defaults = app.settings.request.clone();
    defaults.send_user_agent = true;
    let ctx =
        PrepareContext { resolver, auth: &auth, auth_owner_id: Some(&item.id), defaults: &defaults, base_dir: None };
    let mut prepared = prepare(&http, &ctx);
    if let Some(p) = merpati_auth::preview(&prepared) {
        prepared = p.request;
    }
    let mut url = prepared.url.clone();
    if let Some(rest) = url.strip_prefix("http://") {
        url = format!("ws://{rest}");
    } else if let Some(rest) = url.strip_prefix("https://") {
        url = format!("wss://{rest}");
    }
    let headers = prepared.headers.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("accept")).cloned().collect();
    let verify_ssl = ws.verify_ssl.unwrap_or(app.settings.request.verify_ssl);
    let tls = match app.tls_setup(&url, verify_ssl) {
        Ok(t) => t,
        Err(e) => {
            app.toast(e, true);
            return;
        }
    };
    let cfg = WsConfig {
        url,
        headers,
        subprotocols: ws.subprotocols.clone(),
        verify_ssl,
        tls,
        proxy: app.proxy_settings(),
        ..Default::default()
    };
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    tab.conn = State::Connecting;
    tab.error = None;
    tab.epoch = crate::app::next_epoch();
    let epoch = tab.epoch;
    tab.session = Some(WsSession::connect(app.rt.handle(), cfg, move |event| {
        let _ = tx.send(Msg::Ws { tab: tab_id, epoch, event });
        ctx.request_repaint();
    }));
}

/// Sends `text` (resolved) as a text or binary message.
fn send_message(
    app: &mut MerpatiApp,
    tab: &WsTab,
    scopes: &merpati_core::variables::VariableScopes,
    text: &str,
    binary: bool,
) {
    let resolved = resolve_composer(app, scopes, text);
    let Some(s) = &tab.session else { return };
    if binary {
        match parse_binary(&resolved) {
            Some(b) => {
                s.send_binary(b);
            }
            None => app.toast(tr("Binary messages must be hex or base64"), true),
        }
    } else {
        s.send_text(resolved);
    }
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut WsTab) {
    let Some(original) = get_item(app, tab) else {
        ui.label(tr("This request no longer exists."));
        return;
    };
    let mut item = original.clone();
    let collection_id = tab.location.as_ref().map(|l| l.collection.clone());
    let vars = app.var_ctx(collection_id.as_deref());
    let state = tab.state();
    let slot = tab.slot();
    let since = tab.since;
    let detail = match &tab.conn {
        State::Connected { protocol, status } => {
            let mut d = format!("HTTP {status}");
            if let Some(ms) = tab.handshake_ms {
                d.push_str(&format!(" · {}", trf("handshake {}", &[&merpati_core::util::human_ms(ms)])));
            }
            if let Some(p) = protocol {
                d.push_str(&format!(" · {p}"));
            }
            d
        }
        _ => String::new(),
    };
    pc::header(ui, app, tab_id, &slot, &mut item, |ui| {
        let r = pc::conn_status(ui, state, since);
        if !detail.is_empty() {
            r.on_hover_text(&detail);
        }
    });
    let RequestKind::WebSocket(ws) = &mut item.request else { return };
    let mut action = None;
    let clicked = pc::connect_bar(
        ui,
        "WS",
        126.0,
        |ui| {
            let mut url = ws.url.clone();
            let r =
                widgets::var_cell(ui, "ws-url", &mut url, &vars, "ws://localhost:8080/socket", ui.available_width());
            if r.changed() {
                ws.params = merpati_core::urlutil::merge_params_from_url(&url, &ws.params);
                ws.url = url;
            }
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
        pc::error_banner(ui, tr("Could not connect"), e, |ui| {
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
    let connected = state == Conn::Live;
    let mut send_now: Option<(String, bool)> = None;
    if connected && pc::send_shortcut(ui) {
        send_now = Some((tab.composer.clone(), tab.binary));
    }
    let enabled_params = ws.params.iter().filter(|p| p.enabled).count();
    let enabled_headers = ws.headers.iter().filter(|p| p.enabled).count();
    let auth_label = if ws.auth.is_inherit() || ws.auth.kind == merpati_core::auth::AuthType::None {
        tr("Authorization").to_string()
    } else {
        format!("{} ●", tr("Authorization"))
    };
    let items = vec![
        (Section::Message, tr("Message").to_string()),
        (Section::Params, format!("{}{}", tr("Params"), widgets::count_label(enabled_params))),
        (Section::Headers, format!("{}{}", tr("Headers"), widgets::count_label(enabled_headers))),
        (Section::Auth, auth_label),
        (Section::Saved, format!("{}{}", tr("Saved messages"), widgets::count_label(ws.messages.len()))),
        (Section::Scripts, pc::scripts_label(&item.scripts)),
    ];
    ui.add_space(8.0);
    pc::section_tabs(ui, "ws-tabs", &mut tab.section, &items);
    ui.add_space(8.0);
    let item_id = item.id.clone();
    let log = &mut tab.log;
    let section = &mut tab.section;
    let composer = &mut tab.composer;
    let binary = &mut tab.binary;
    let scripts_state = &mut tab.scripts;
    let scripts = &mut item.scripts;
    pc::split_view(
        ui,
        ("ws-split", tab_id),
        0.55,
        |ui| match *section {
            Section::Message => {
                ui.horizontal(|ui| {
                    ui.set_height(30.0);
                    kit::segmented(ui, binary, &[(false, tr("Text")), (true, tr("Binary"))]);
                    if *binary {
                        ui.label(kit::hint(ui, tr("hex or base64, e.g. 01 02 ff or AQL/")));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if pc::send_button(ui, icons::PAPER_PLANE_RIGHT, tr("Send"), connected, tr("Connect first")) {
                            send_now = Some((composer.clone(), *binary));
                        }
                        if pc::save_message_button(ui, !composer.trim().is_empty()) {
                            ws.messages.push(WsSavedMessage {
                                id: merpati_core::util::new_id(),
                                name: composer.lines().next().unwrap_or("").chars().take(30).collect(),
                                body: composer.clone(),
                                kind: if *binary { WsMessageKind::Binary } else { WsMessageKind::Text },
                            });
                        }
                    });
                });
                ui.add_space(6.0);
                let lang = if *binary { Lang::Text } else { Lang::Json };
                let r = pc::fill_editor(ui, "ws-composer", composer, lang, &vars, 4);
                let hint = if *binary {
                    tr("Bytes as hex (01 02 ff) or base64")
                } else {
                    tr("Type a message: JSON or text, with {{variables}}")
                };
                pc::editor_placeholder(ui, &r, composer, hint);
            }
            Section::Params => {
                egui::ScrollArea::vertical().id_salt("ws-params-scroll").show(ui, |ui| {
                    let mut params = ws.params.clone();
                    let opts = KvOptions {
                        title: Some(tr("Query parameters")),
                        allow_add: true,
                        key_hint: tr("Key"),
                        value_hint: tr("Value"),
                        ..Default::default()
                    };
                    if widgets::kv_table(ui, "ws-params", &mut params, &vars, opts) {
                        ws.params = params;
                        ws.url = merpati_core::urlutil::replace_query(&ws.url, &ws.params);
                    }
                });
            }
            Section::Headers => {
                egui::ScrollArea::vertical().id_salt("ws-headers-scroll").show(ui, |ui| {
                    let mut headers = ws.headers.clone();
                    let opts = KvOptions {
                        title: Some(tr("Headers")),
                        key_suggestions: Some(widgets::COMMON_HEADERS),
                        allow_add: true,
                        key_hint: tr("Key"),
                        value_hint: tr("Value"),
                        ..Default::default()
                    };
                    if widgets::kv_table(ui, "ws-headers", &mut headers, &vars, opts) {
                        ws.headers = headers;
                    }
                    ui.add_space(12.0);
                    pc::section(ui, tr("Subprotocols"), Some(tr("Sent as Sec-WebSocket-Protocol, comma separated.")));
                    let mut text = ws.subprotocols.join(", ");
                    let edit = egui::TextEdit::singleline(&mut text)
                        .hint_text(tr("graphql-transport-ws, mqtt"))
                        .font(egui::TextStyle::Monospace)
                        .desired_width(360.0);
                    if ui.add(edit).changed() {
                        ws.subprotocols =
                            text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    }
                });
            }
            Section::Auth => {
                egui::ScrollArea::vertical().id_salt("ws-auth-scroll").show(ui, |ui| {
                    crate::auth_ui::auth_editor(
                        ui,
                        &mut ws.auth,
                        crate::auth_ui::AuthUiCtx {
                            is_root: false,
                            inherited: None,
                            owner_id: &item_id,
                            token: None,
                            fetching: false,
                            vars: &vars,
                        },
                    );
                });
            }
            Section::Scripts => crate::proto_common::scripts_ui(
                ui,
                scripts,
                &[
                    (merpati_core::script::ScriptEvent::PreRequest, "Before connect"),
                    (merpati_core::script::ScriptEvent::OnMessage, "On message"),
                ],
                scripts_state,
                &vars,
            ),
            Section::Saved => {
                egui::ScrollArea::vertical().id_salt("ws-saved").show(ui, |ui| {
                    if ws.messages.is_empty() {
                        kit::empty_state(
                            ui,
                            icons::BOOKMARK_SIMPLE,
                            tr("No saved messages"),
                            tr("Save messages from the composer to reuse them; they are stored with the request."),
                            |_| {},
                        );
                    } else {
                        let p = theme::pal(ui);
                        let acted = pc::saved_list(
                            ui,
                            "ws-saved-list",
                            &mut ws.messages,
                            connected,
                            tr("Send now"),
                            |ui, m| {
                                pc::name_field(ui, &mut m.name, 150.0);
                                if m.kind == WsMessageKind::Binary {
                                    kit::pill(ui, tr("Binary"), p.info);
                                }
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(m.body.replace('\n', " ")).monospace().color(p.text_muted),
                                    )
                                    .truncate(),
                                );
                            },
                        );
                        match acted {
                            Some((i, RowAction::Use)) => {
                                *composer = ws.messages[i].body.clone();
                                *binary = ws.messages[i].kind == WsMessageKind::Binary;
                                *section = Section::Message;
                            }
                            Some((i, RowAction::Send)) => {
                                send_now =
                                    Some((ws.messages[i].body.clone(), ws.messages[i].kind == WsMessageKind::Binary));
                            }
                            Some((i, RowAction::Remove)) => {
                                ws.messages.remove(i);
                            }
                            None => {}
                        }
                    }
                    ui.add_space(12.0);
                    runner_section(ui, "ws", &mut ws.runner, tr("message text, or its JSON type/event"));
                });
            }
        },
        |ui| log.ui(ui, ("ws-log", tab_id)),
    );
    if let Some((text, binary)) = send_now {
        send_message(app, tab, &vars.scopes, &text, binary);
    }
    if item != original {
        match &tab.location {
            Some(loc) => {
                let cid = loc.collection.clone();
                if let Some(target) =
                    app.ws.data.collection_mut(&loc.collection).and_then(|c| c.find_request_mut(&loc.item))
                {
                    *target = item.clone();
                    app.ws.mark_collection(&cid);
                }
            }
            None => tab.scratch = Some(item.clone()),
        }
    }
    use merpati_core::script::{ScriptEvent, protocol as proto};
    match action {
        Some(ConnAction::Connect) => {
            tab.scripts.clear_results();
            let pre = match &item.request {
                RequestKind::WebSocket(w) => crate::proto_common::run_scripts(
                    app,
                    tab_id,
                    &slot,
                    &item,
                    ScriptEvent::PreRequest,
                    proto::websocket(w),
                ),
                _ => false,
            };
            if pre {
                tab.scripts.waiting = true;
            } else {
                connect(app, tab_id, tab, &item);
            }
        }
        Some(ConnAction::Cancel) => {
            if let Some(s) = &tab.session {
                s.close(1000, "");
            }
            tab.session = None;
            tab.conn = State::Disconnected;
        }
        Some(ConnAction::Disconnect) => {
            if let Some(s) = &tab.session {
                s.close(1000, "");
            }
        }
        None => {}
    }
    // Pre-request finished (a skipped request just doesn't connect).
    if let Some(Some(v)) = tab.scripts.take_pre() {
        let mut sent = item.clone();
        if let RequestKind::WebSocket(w) = &mut sent.request {
            proto::apply_websocket(w, &v);
        }
        connect(app, tab_id, tab, &sent);
    }
    if let RequestKind::WebSocket(w) = &item.request {
        for m in std::mem::take(&mut tab.inbox) {
            if !crate::proto_common::run_scripts(
                app,
                tab_id,
                &slot,
                &item,
                ScriptEvent::OnMessage,
                proto::websocket(w).with_message(m),
            ) {
                break;
            }
        }
    }
}

/// The message to send, with `{{variables}}` resolved.
fn resolve_composer(app: &MerpatiApp, scopes: &merpati_core::variables::VariableScopes, text: &str) -> String {
    let vault = app.ws.vault();
    Resolver::new(scopes).with_vault(vault.as_ref()).resolve(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `{{vault:…}}` in a message resolves like it does in the URL and
    /// headers instead of being sent literally.
    #[test]
    fn composer_resolves_vault_references() {
        let dir = tempfile::tempdir().unwrap();
        let app = MerpatiApp::for_test(dir.path());
        app.ws.secrets.vault_set("ws-key", "s3cret");
        let scopes = app.ws.scopes_for(None);
        assert_eq!(resolve_composer(&app, &scopes, "{\"key\": \"{{vault:ws-key}}\"}"), "{\"key\": \"s3cret\"}");
    }

    fn incoming(tab: &mut WsTab, text: String) {
        tab.on_event(WsEvent::Message { direction: WsDirection::Incoming, payload: WsPayload::Text(text), at_ms: 0 });
    }

    /// The log keeps the latest messages; the selected one stays selected as
    /// older ones are dropped, and messages waiting for on-message scripts
    /// (a tab in the background is not drawn) do not pile up without limit.
    #[test]
    fn full_log_keeps_the_selection_and_bounded_inbox() {
        let mut tab = WsTab::scratch(RequestItem::new("ws", merpati_core::model::HttpRequest::default()));
        let cap = tab.log.cap;
        for i in 0..cap {
            incoming(&mut tab, format!("m{i}"));
        }
        tab.log.selected = Some(5);
        incoming(&mut tab, "next".into());
        assert_eq!(tab.log.selected.and_then(|i| tab.log.lines.get(i)).map(|l| l.text.as_str()), Some("m5"));
        for i in 0..cap {
            incoming(&mut tab, format!("more{i}"));
        }
        assert_eq!(tab.log.lines.len(), cap);
        assert!(tab.inbox.len() <= crate::proto_common::MAX_INBOX);
    }

    /// A failed connection is shown as such (with its cause) until the next
    /// attempt; errors on an open connection do not mark it failed.
    #[test]
    fn failed_connection_state() {
        let mut tab = WsTab::scratch(RequestItem::new("ws", merpati_core::model::HttpRequest::default()));
        assert_eq!(tab.state(), Conn::Idle);
        tab.on_event(WsEvent::Connecting);
        assert_eq!(tab.state(), Conn::Busy);
        tab.on_event(WsEvent::Error("connection refused".into()));
        tab.on_event(WsEvent::Closed { code: None, reason: String::new(), by_peer: false });
        assert_eq!(tab.state(), Conn::Failed);
        assert_eq!(tab.error.as_deref(), Some("connection refused"));
        tab.on_event(WsEvent::Connecting);
        assert_eq!(tab.state(), Conn::Busy);
    }

    /// A log of 20,000 messages (some long, multi-line JSON) draws only the
    /// rows in view: a frame takes milliseconds, not one widget per message.
    #[test]
    fn big_log_draws_only_visible_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let mut item = RequestItem::new("ws", merpati_core::model::HttpRequest::default());
        item.request = RequestKind::WebSocket(Default::default());
        let id = app.add_tab(crate::app::TabKind::WebSocket(Box::new(WsTab::scratch(item))));
        let crate::app::TabKind::WebSocket(tab) = &mut app.tabs[0].kind else { panic!("ws tab") };
        tab.log.cap = 20_000;
        let json = serde_json::to_string_pretty(&serde_json::json!({"items": vec![1; 200]})).unwrap();
        for i in 0..20_000 {
            incoming(tab, if i % 10 == 0 { json.clone() } else { format!("message {i}") });
        }
        tab.log.selected = Some(19_990);
        app.draw_frame(Vec::new());
        let started = std::time::Instant::now();
        let frame = app.draw_frame(Vec::new());
        let took = started.elapsed();
        assert!(frame.labels.len() < 1_000, "{} widgets", frame.labels.len());
        assert!(frame.shapes < 2_000, "{} shapes", frame.shapes);
        assert!(took < std::time::Duration::from_millis(500), "{took:?}");
        // The newest rows are in view (the list sticks to the bottom), and
        // the selected message is shown in full.
        assert!(frame.labels.iter().any(|l| l == "message 19999"), "{:?}", frame.labels);
        assert!(app.tab_index(id).is_some());

        // Searching a big log is fine too.
        let crate::app::TabKind::WebSocket(tab) = &mut app.tabs[0].kind else { panic!("ws tab") };
        tab.log.search = "message 1999".into();
        app.draw_frame(Vec::new());
        let frame = app.draw_frame(Vec::new());
        assert!(frame.labels.iter().any(|l| l == "message 19998"), "{:?}", frame.labels);
        assert!(!frame.labels.iter().any(|l| l == "message 19989"));
    }
}
