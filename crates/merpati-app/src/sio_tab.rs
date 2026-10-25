//! Socket.IO request tab (FR-X4): namespaces, event listeners, emitting
//! events with JSON, text and binary arguments and acknowledgements.

use std::time::Instant;

use egui::{RichText, Ui};

use merpati_core::model::RequestKind;
use merpati_core::protocols::{SocketIoArg, SocketIoArgKind, SocketIoListener, SocketIoMessage};
use merpati_socketio::{
    ArgError, Direction, EmitArg, SharedCookies, SocketIoConfig, SocketIoEvent, SocketIoSession, SocketIoTransport,
    SocketIoVersion,
};

use crate::app::{MerpatiApp, Msg};
use crate::highlight::Lang;
use crate::i18n::{tr, trf};
use crate::proto_common::{self as pc, Conn, ConnAction, Dir, ItemSlot, Log, RowAction, ScriptState, header, resolver};
use crate::widgets::{self, KvOptions, VarCtx};
use crate::{icons, kit, theme};
use merpati_core::script::ScriptEvent;
use merpati_core::script::protocol as proto;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Emit,
    Listeners,
    Saved,
    Headers,
    Settings,
    Scripts,
}

pub struct SioTab {
    /// Number of the current connection (see `app::next_epoch`).
    pub epoch: u64,
    pub slot: ItemSlot,
    session: Option<SocketIoSession>,
    connected: bool,
    section: Section,
    event: String,
    args: Vec<SocketIoArg>,
    ack: bool,
    only_listened: bool,
    /// Reply per pending server ack id: a JSON argument list, or one text or
    /// binary argument.
    ack_replies: std::collections::HashMap<u64, SocketIoArg>,
    log: Log,
    /// Automation: `connect` or `emit`.
    pub auto: Option<String>,
    pub scripts: ScriptState,
    inbox: std::collections::VecDeque<serde_json::Value>,
    since: Option<Instant>,
    /// Why the last connection failed.
    error: Option<String>,
}

/// An event to emit: name, arguments and whether to ask for an ack.
type Emit = (String, Vec<SocketIoArg>, bool);

impl SioTab {
    pub fn new(slot: ItemSlot) -> Self {
        SioTab {
            epoch: 0,
            slot,
            session: None,
            connected: false,
            section: Section::Emit,
            event: "message".into(),
            args: vec![SocketIoArg::new(SocketIoArgKind::Json, "\"hello\"")],
            ack: false,
            only_listened: false,
            ack_replies: Default::default(),
            log: Log::with_empty("No events yet", "Connect, then emit an event; events from the server appear here."),
            auto: None,
            scripts: ScriptState::default(),
            inbox: Default::default(),
            since: None,
            error: None,
        }
    }

    fn state(&self) -> Conn {
        if self.connected {
            Conn::Live
        } else if self.session.is_some() {
            Conn::Busy
        } else if self.error.is_some() {
            Conn::Failed
        } else {
            Conn::Idle
        }
    }

    pub fn on_event(&mut self, e: SocketIoEvent, listened: &[String]) {
        match e {
            SocketIoEvent::Connecting => {
                self.error = None;
                self.log.system(tr("Connecting…"))
            }
            SocketIoEvent::Connected { sid, namespace } => {
                self.connected = true;
                self.since = Some(Instant::now());
                self.error = None;
                self.log.system(trf("Connected to {} (sid {})", &[&namespace, &sid]));
            }
            SocketIoEvent::Event { name, args, direction, at_ms } => {
                if direction == Direction::Incoming && self.only_listened && !listened.iter().any(|l| l == &name) {
                    return;
                }
                let dir = if direction == Direction::Incoming { Dir::In } else { Dir::Out };
                if direction == Direction::Incoming {
                    let data = args.first().cloned().unwrap_or(serde_json::Value::Null);
                    let mut m = proto::message(data, proto::Direction::Incoming, at_ms);
                    m["event"] = serde_json::json!(name);
                    m["args"] = serde_json::json!(args);
                    crate::proto_common::queue_message(&mut self.inbox, m);
                }
                self.log.push(dir, name, serde_json::to_string(&args).unwrap_or_default(), at_ms);
            }
            SocketIoEvent::Ack { id, args, at_ms } => {
                self.log.push(Dir::In, trf("ack #{}", &[&id]), serde_json::to_string(&args).unwrap_or_default(), at_ms)
            }
            SocketIoEvent::Error(m) => {
                if !self.connected {
                    self.error = Some(m.clone());
                }
                self.log.error(m)
            }
            SocketIoEvent::Disconnected { reason } => {
                if !self.connected && self.error.is_none() && !reason.is_empty() {
                    self.error = Some(reason.clone());
                }
                self.connected = false;
                self.session = None;
                self.since = None;
                self.log.system(trf("Disconnected: {}", &[&reason]));
            }
        }
    }
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut SioTab) {
    let Some(original) = tab.slot.load(app) else {
        ui.label(tr("This request no longer exists."));
        return;
    };
    let mut item = original.clone();
    let collection = tab.slot.collection_id();
    let vars = app.var_ctx(collection.as_deref());
    let state = tab.state();
    let since = tab.since;
    let transport = tab.session.as_ref().and_then(|s| s.transport());
    header(ui, app, tab_id, &tab.slot, &mut item, |ui| {
        pc::conn_status(ui, state, since);
        if let Some(t) = transport {
            kit::pill(ui, t.label(), theme::pal(ui).text_muted).on_hover_text(tr("Transport in use"));
        }
    });
    let RequestKind::SocketIo(req) = &mut item.request else { return };
    let mut action = None;
    let clicked = pc::connect_bar(
        ui,
        "SIO",
        126.0,
        |ui| {
            let w = (ui.available_width() - 130.0).max(120.0);
            let r = widgets::var_cell(ui, "sio-url", &mut req.url, &vars, "http://localhost:3000", w);
            kit::vdivider(ui, 22.0);
            let ns = pc::bar_field(ui, "sio-ns", &mut req.namespace, tr("/namespace"), ui.available_width());
            let enter = (r.lost_focus() || ns.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if enter && state != Conn::Live {
                action = Some(ConnAction::Connect);
            }
            r.has_focus() || ns.has_focus()
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
    let can_emit = connected && !tab.event.trim().is_empty();
    let mut emit_now: Option<Emit> = None;
    if can_emit && pc::send_shortcut(ui) {
        emit_now = Some((tab.event.clone(), tab.args.clone(), tab.ack));
    }
    let listeners = req.listeners.iter().filter(|l| l.enabled).count();
    let headers = req.headers.iter().filter(|h| h.enabled).count() + req.params.iter().filter(|h| h.enabled).count();
    let items = vec![
        (Section::Emit, tr("Emit").to_string()),
        (Section::Listeners, trf("Events{}", &[&widgets::count_label(listeners)])),
        (Section::Saved, format!("{}{}", tr("Saved messages"), widgets::count_label(req.messages.len()))),
        (Section::Headers, format!("{}{}", tr("Headers"), widgets::count_label(headers))),
        (Section::Settings, tr("Settings").to_string()),
        (Section::Scripts, pc::scripts_label(&item.scripts)),
    ];
    ui.add_space(8.0);
    pc::section_tabs(ui, "sio-tabs", &mut tab.section, &items);
    ui.add_space(8.0);
    // Server events waiting for our acknowledgement.
    let pending = tab.session.as_ref().map(|s| s.pending_acks()).unwrap_or_default();
    let mut send_ack = None;
    if !pending.is_empty() {
        pc::notice_card(ui, icons::HOURGLASS, tr("The server is waiting for acknowledgements"), |ui| {
            for a in &pending {
                ui.push_id(("sio-ack", a.id), |ui| {
                    ui.horizontal(|ui| {
                        kit::pill(ui, &format!("#{}", a.id), theme::pal(ui).text_muted);
                        ui.label(RichText::new(&a.event).monospace());
                        let reply = tab
                            .ack_replies
                            .entry(a.id)
                            .or_insert_with(|| SocketIoArg::new(SocketIoArgKind::Json, "[]"));
                        kind_combo(ui, &mut reply.kind).on_hover_text(match reply.kind {
                            SocketIoArgKind::Json => {
                                tr("Arguments as a JSON array, a single JSON value, or plain text.")
                            }
                            _ => tr("Sent as one argument"),
                        });
                        if reply.kind == SocketIoArgKind::File {
                            choose_file(ui, &mut reply.value);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if kit::secondary(ui, Some(icons::CHECK), tr("Send ack")).clicked() {
                                send_ack = Some(a.id);
                            }
                            ui.add(
                                egui::TextEdit::singleline(&mut reply.value)
                                    .desired_width(ui.available_width())
                                    .font(egui::TextStyle::Monospace),
                            );
                        });
                    });
                });
            }
        });
        ui.add_space(8.0);
    }
    if let Some(id) = send_ack {
        answer_ack(app, tab, id, collection.as_deref());
    }
    {
        let log = &mut tab.log;
        let section = &mut tab.section;
        let event = &mut tab.event;
        let args = &mut tab.args;
        let ack = &mut tab.ack;
        let only_listened = &mut tab.only_listened;
        let scripts_state = &mut tab.scripts;
        let scripts = &mut item.scripts;
        let item_id = item.id.clone();
        pc::split_view(
            ui,
            ("sio-split", tab_id),
            0.5,
            |ui| match *section {
                Section::Emit => {
                    let p = theme::pal(ui);
                    ui.horizontal(|ui| {
                        ui.set_height(30.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let hint = if connected { tr("Enter an event name first") } else { tr("Connect first") };
                            let ok = connected && !event.trim().is_empty();
                            if pc::send_button(ui, icons::PAPER_PLANE_RIGHT, tr("Emit"), ok, hint) {
                                emit_now = Some((event.clone(), args.clone(), *ack));
                            }
                            if pc::save_message_button(ui, !event.trim().is_empty()) {
                                req.messages.push(SocketIoMessage {
                                    name: event.clone(),
                                    event: event.clone(),
                                    arguments: args.clone(),
                                    ack: *ack,
                                    ..Default::default()
                                });
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.label(RichText::new(tr("Event")).color(p.text_muted));
                                ui.add(
                                    egui::TextEdit::singleline(event)
                                        .desired_width((ui.available_width() - 130.0).clamp(120.0, 300.0))
                                        .hint_text("message")
                                        .font(egui::TextStyle::Monospace),
                                );
                                ui.add_space(4.0);
                                ui.checkbox(ack, tr("Expect ack"))
                                    .on_hover_text(tr("Ask the server to acknowledge; its answer appears in the log"));
                            });
                        });
                    });
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().id_salt("sio-emit").show(ui, |ui| {
                        args_editor(ui, args, &vars);
                    });
                }
                Section::Listeners => {
                    egui::ScrollArea::vertical().id_salt("sio-listeners").show(ui, |ui| {
                        pc::section(
                            ui,
                            tr("Event listeners"),
                            Some(tr("Incoming events with these names are expected; the log can show only them.")),
                        );
                        ui.checkbox(only_listened, tr("Only show incoming events listed here"));
                        ui.add_space(6.0);
                        let p = theme::pal(ui);
                        let mut remove = None;
                        if !req.listeners.is_empty() {
                            egui::Frame::new()
                                .stroke(egui::Stroke::new(1.0, p.border))
                                .corner_radius(egui::CornerRadius::same(8))
                                .inner_margin(egui::Margin::symmetric(8, 2))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    for (i, l) in req.listeners.iter_mut().enumerate() {
                                        if i > 0 {
                                            let y = ui.cursor().top();
                                            ui.painter().hline(
                                                ui.max_rect().expand2(egui::vec2(8.0, 0.0)).x_range(),
                                                y,
                                                egui::Stroke::new(1.0, p.border),
                                            );
                                        }
                                        ui.push_id(i, |ui| {
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(ui.available_width(), 34.0),
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if kit::icon_button(ui, icons::TRASH, tr("Remove event")).clicked()
                                                    {
                                                        remove = Some(i);
                                                    }
                                                    ui.with_layout(
                                                        egui::Layout::left_to_right(egui::Align::Center),
                                                        |ui| {
                                                            ui.checkbox(&mut l.enabled, "")
                                                                .on_hover_text(tr("Listen to this event"));
                                                            ui.add(
                                                                egui::TextEdit::singleline(&mut l.event)
                                                                    .desired_width(ui.available_width())
                                                                    .hint_text(tr("event name"))
                                                                    .font(egui::TextStyle::Monospace),
                                                            );
                                                        },
                                                    );
                                                },
                                            );
                                        });
                                    }
                                });
                        }
                        if let Some(i) = remove {
                            req.listeners.remove(i);
                        }
                        ui.add_space(6.0);
                        if kit::ghost(ui, Some(icons::PLUS), tr("Add event")).clicked() {
                            req.listeners.push(SocketIoListener { event: "message".into(), enabled: true });
                        }
                    });
                }
                Section::Saved => {
                    egui::ScrollArea::vertical().id_salt("sio-saved").show(ui, |ui| {
                        if req.messages.is_empty() {
                            kit::empty_state(
                                ui,
                                icons::BOOKMARK_SIMPLE,
                                tr("No saved messages"),
                                tr("Save events from the Emit section to reuse them; they are stored with the request."),
                                |_| {},
                            );
                        } else {
                            let p = theme::pal(ui);
                            let acted = pc::saved_list(ui, "sio-saved-list", &mut req.messages, connected, tr("Emit now"), |ui, m| {
                                pc::name_field(ui, &mut m.name, 140.0);
                                kit::pill(ui, &crate::dialogs::ellipsize(&m.event, 30), p.text_muted);
                                if m.ack {
                                    ui.label(RichText::new(tr("ack")).small().color(p.text_muted));
                                }
                                let summary = args_summary(&m.argument_list());
                                ui.add(egui::Label::new(RichText::new(summary).monospace().color(p.text_muted)).truncate());
                            });
                            match acted {
                                Some((i, RowAction::Use)) => {
                                    let m = req.messages[i].clone();
                                    *args = m.argument_list();
                                    *event = m.event;
                                    *ack = m.ack;
                                    *section = Section::Emit;
                                }
                                Some((i, RowAction::Send)) => {
                                    let m = &req.messages[i];
                                    emit_now = Some((m.event.clone(), m.argument_list(), m.ack));
                                }
                                Some((i, RowAction::Remove)) => {
                                    req.messages.remove(i);
                                }
                                None => {}
                            }
                        }
                        ui.add_space(12.0);
                        crate::ws_tab::runner_section(ui, "sio", &mut req.runner, tr("event name"));
                    });
                }
                Section::Headers => {
                    egui::ScrollArea::vertical().id_salt("sio-headers").show(ui, |ui| {
                        let mut h = req.headers.clone();
                        let opts = KvOptions {
                            title: Some(tr("Headers")),
                            allow_add: true,
                            key_suggestions: Some(widgets::COMMON_HEADERS),
                            key_hint: tr("Key"),
                            value_hint: tr("Value"),
                            ..Default::default()
                        };
                        if widgets::kv_table(ui, "sio-headers", &mut h, &vars, opts) {
                            req.headers = h;
                        }
                        ui.add_space(12.0);
                        let mut p = req.params.clone();
                        let opts = KvOptions {
                            title: Some(tr("Query parameters")),
                            allow_add: true,
                            key_hint: tr("Key"),
                            value_hint: tr("Value"),
                            ..Default::default()
                        };
                        if widgets::kv_table(ui, "sio-params", &mut p, &vars, opts) {
                            req.params = p;
                        }
                    });
                }
                Section::Scripts => crate::proto_common::scripts_ui(
                    ui,
                    scripts,
                    &[(ScriptEvent::PreRequest, "Before connect"), (ScriptEvent::OnMessage, "On message")],
                    scripts_state,
                    &vars,
                ),
                Section::Settings => {
                    egui::ScrollArea::vertical().id_salt("sio-settings").show(ui, |ui| {
                        pc::form_grid("sio-settings").show(ui, |ui| {
                            pc::form_label(ui, tr("Transport"));
                            ui.push_id("transport", |ui| {
                                kit::segmented(
                                    ui,
                                    &mut req.transport,
                                    &[
                                        (SocketIoTransport::Auto, tr("Auto")),
                                        (SocketIoTransport::WebSocket, "WebSocket"),
                                        (SocketIoTransport::Polling, tr("Polling")),
                                    ],
                                )
                            });
                            ui.end_row();
                            ui.label("");
                            ui.label(kit::hint(
                                ui,
                                tr("Auto: long-polling first, then upgrade to WebSocket, like the official client"),
                            ));
                            ui.end_row();
                            pc::form_label(ui, tr("Server version"));
                            ui.push_id("version", |ui| {
                                kit::segmented(
                                    ui,
                                    &mut req.version,
                                    &[(SocketIoVersion::V4, "v3 / v4"), (SocketIoVersion::V2, "v2")],
                                )
                            });
                            ui.end_row();
                            pc::form_label(ui, tr("Handshake path"));
                            ui.add(
                                egui::TextEdit::singleline(&mut req.path)
                                    .hint_text("/socket.io/")
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(240.0),
                            );
                            ui.end_row();
                            pc::form_label(ui, tr("Verify SSL certificates"));
                            let mut v = req.verify_ssl.unwrap_or(true);
                            if ui.checkbox(&mut v, tr("For https:// servers")).changed() {
                                req.verify_ssl = Some(v);
                            }
                            ui.end_row();
                        });
                        ui.add_space(14.0);
                        pc::section(
                            ui,
                            tr("Handshake auth"),
                            Some(tr("A JSON object sent with the connection (socket.handshake.auth).")),
                        );
                        widgets::code_editor(ui, "sio-auth", &mut req.auth_payload, Lang::Json, Some(&vars), 3, false);
                        ui.add_space(14.0);
                        pc::section(ui, tr("Authorization"), None);
                        crate::auth_ui::auth_editor(
                            ui,
                            &mut req.auth,
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
            },
            |ui| log.ui(ui, ("sio-log", tab_id)),
        );
    }
    match tab.auto.as_deref() {
        Some("connect") => {
            if tab.session.is_none() {
                action = Some(ConnAction::Connect);
            }
            tab.auto = None;
        }
        Some("emit") if tab.connected => {
            emit_now = Some((tab.event.clone(), tab.args.clone(), tab.ack));
            tab.auto = None;
        }
        _ => {}
    }
    if let Some(e) = emit_now {
        emit(app, tab, collection.as_deref(), e);
    }
    let req_clone = req.clone();
    let item_now = item.clone();
    tab.slot.store(app, &original, item);
    match action {
        Some(ConnAction::Connect) => {
            tab.scripts.clear_results();
            tab.error = None;
            if crate::proto_common::run_scripts(
                app,
                tab_id,
                &tab.slot,
                &item_now,
                ScriptEvent::PreRequest,
                proto::socketio(&req_clone),
            ) {
                tab.scripts.waiting = true;
            } else {
                start(app, tab_id, tab, &req_clone, collection.as_deref());
            }
        }
        Some(ConnAction::Cancel) | Some(ConnAction::Disconnect) => {
            if let Some(s) = &tab.session {
                s.disconnect();
            }
        }
        None => {}
    }
    if let Some(pre) = tab.scripts.take_pre() {
        match pre {
            Some(v) => {
                let mut r = req_clone.clone();
                proto::apply_socketio(&mut r, &v);
                start(app, tab_id, tab, &r, collection.as_deref());
            }
            None => tab.log.system(tr("Skipped by pm.execution.skipRequest()")),
        }
    }
    for m in std::mem::take(&mut tab.inbox) {
        if !crate::proto_common::run_scripts(
            app,
            tab_id,
            &tab.slot,
            &item_now,
            ScriptEvent::OnMessage,
            proto::socketio(&req_clone).with_message(m),
        ) {
            break;
        }
    }
}

/// `req` with `Inherit` auth replaced by its folder's or collection's, as
/// for HTTP requests.
fn with_inherited_auth(
    app: &MerpatiApp,
    collection: Option<&str>,
    item_id: &str,
    req: &merpati_core::protocols::SocketIoRequest,
) -> merpati_core::protocols::SocketIoRequest {
    let mut req = req.clone();
    if req.auth.is_inherit() {
        req.auth = collection
            .and_then(|c| app.ws.data.collection(c))
            .and_then(|c| c.effective_auth(item_id).map(|e| e.auth.clone()))
            .unwrap_or_else(merpati_core::auth::Auth::none);
    }
    req
}

fn start(
    app: &mut MerpatiApp,
    tab_id: u64,
    tab: &mut SioTab,
    req_clone: &merpati_core::protocols::SocketIoRequest,
    collection: Option<&str>,
) {
    {
        let r = resolver(app, collection);
        let item_id = tab.slot.location.as_ref().map(|l| l.item.clone()).unwrap_or_default();
        let req = with_inherited_auth(app, collection, &item_id, req_clone);
        let mut cfg = SocketIoConfig::from_request(&req, &r, app.settings.request.verify_ssl);
        cfg.cookies = Some(SharedCookies::new(app.ws.cookies.clone()));
        cfg.proxy = app.proxy_settings();
        match app.tls_setup(&cfg.url, cfg.verify_ssl) {
            Ok(tls) => cfg.tls = tls,
            Err(e) => return app.toast(e, true),
        }
        let tx = app.tx.clone();
        let ctx = app.ctx.clone();
        tab.epoch = crate::app::next_epoch();
        let epoch = tab.epoch;
        tab.session = Some(SocketIoSession::connect(app.rt.handle(), cfg, move |event| {
            let _ = tx.send(Msg::SocketIo { tab: tab_id, epoch, event });
            ctx.request_repaint();
        }));
    }
}

const KINDS: [SocketIoArgKind; 4] =
    [SocketIoArgKind::Json, SocketIoArgKind::Text, SocketIoArgKind::Binary, SocketIoArgKind::File];

fn kind_label(kind: SocketIoArgKind) -> &'static str {
    match kind {
        SocketIoArgKind::Json => "JSON",
        SocketIoArgKind::Text => tr("Text"),
        SocketIoArgKind::Binary => tr("Binary"),
        SocketIoArgKind::File => tr("File"),
    }
}

fn kind_combo(ui: &mut Ui, kind: &mut SocketIoArgKind) -> egui::Response {
    egui::ComboBox::from_id_salt("kind")
        .icon(crate::kit::combo_caret)
        .width(90.0)
        .selected_text(kind_label(*kind))
        .show_ui(ui, |ui| {
            for k in KINDS {
                ui.selectable_value(kind, k, kind_label(k));
            }
        })
        .response
}

fn choose_file(ui: &mut Ui, path: &mut String) {
    if kit::secondary(ui, Some(icons::FOLDER_OPEN), tr("Choose file…")).clicked()
        && let Some(p) = rfd::FileDialog::new().pick_file()
    {
        *path = p.display().to_string();
    }
}

/// What is wrong with an argument as typed; values with variables are
/// checked when sent, files when they are read.
fn arg_problem(a: &SocketIoArg) -> Option<&'static str> {
    if a.value.contains("{{") {
        return None;
    }
    match a.kind {
        SocketIoArgKind::Json if serde_json::from_str::<serde_json::Value>(&a.value).is_err() => {
            Some(tr("Invalid JSON"))
        }
        SocketIoArgKind::Binary if SocketIoArg::decode_binary(&a.value).is_none() => {
            Some(tr("Binary messages must be hex or base64"))
        }
        _ => None,
    }
}

/// The emit arguments, one card each with its kind.
fn args_editor(ui: &mut Ui, args: &mut Vec<SocketIoArg>, vars: &VarCtx) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.label(kit::section_title(tr("Arguments")));
        if !args.is_empty() {
            pc::count_badge(ui, args.len());
        }
        ui.label(kit::hint(ui, tr("JSON, text or binary (an attachment, a Buffer on the server).")));
    });
    ui.add_space(4.0);
    let mut remove = None;
    for (i, a) in args.iter_mut().enumerate() {
        ui.push_id(("sio-arg", i), |ui| {
            egui::Frame::new()
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 11.0, p.subtle);
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            (i + 1).to_string(),
                            theme::medium(11.5),
                            p.text_muted,
                        );
                        let items: Vec<(SocketIoArgKind, &str)> = KINDS.iter().map(|k| (*k, kind_label(*k))).collect();
                        kit::segmented(ui, &mut a.kind, &items);
                        if let Some(problem) = arg_problem(a) {
                            kit::pill(ui, &format!("{} {problem}", icons::WARNING_CIRCLE), p.err);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if kit::icon_button(ui, icons::TRASH, tr("Remove argument")).clicked() {
                                remove = Some(i);
                            }
                        });
                    });
                    ui.add_space(4.0);
                    let width = ui.available_width();
                    match a.kind {
                        SocketIoArgKind::Json => {
                            widgets::code_editor(ui, "value", &mut a.value, Lang::Json, Some(vars), 2, false);
                        }
                        SocketIoArgKind::Text => {
                            widgets::code_editor(ui, "value", &mut a.value, Lang::Text, Some(vars), 2, false);
                        }
                        SocketIoArgKind::Binary => {
                            widgets::var_line(
                                ui,
                                "value",
                                &mut a.value,
                                vars,
                                tr("hex or base64, e.g. 01 02 ff or AQL/"),
                                width,
                            );
                        }
                        SocketIoArgKind::File => {
                            ui.horizontal(|ui| {
                                choose_file(ui, &mut a.value);
                                let hint = tr("File path (absolute or relative to the workspace folder)");
                                widgets::var_line(ui, "value", &mut a.value, vars, hint, ui.available_width());
                            });
                        }
                    }
                });
        });
        ui.add_space(6.0);
    }
    if let Some(i) = remove {
        args.remove(i);
    }
    ui.horizontal(|ui| {
        if kit::ghost(ui, Some(icons::PLUS), tr("Add argument")).clicked() {
            args.push(SocketIoArg::default());
        }
        if args.is_empty() {
            ui.label(kit::hint(ui, tr("No arguments: the event is emitted without data.")));
        }
    });
}

/// One line per saved message: JSON as written, text quoted, binary marked.
fn args_summary(args: &[SocketIoArg]) -> String {
    let parts: Vec<String> = args
        .iter()
        .map(|a| match a.kind {
            SocketIoArgKind::Json => a.value.trim().replace('\n', " "),
            SocketIoArgKind::Text => serde_json::Value::String(a.value.clone()).to_string(),
            SocketIoArgKind::Binary => format!("{} {}", tr("(binary)"), a.value.trim()),
            SocketIoArgKind::File => {
                let path = a.value.trim();
                format!("{} {}", tr("(binary)"), path.rsplit(['/', '\\']).next().unwrap_or(path))
            }
        })
        .collect();
    parts.join(", ")
}

/// A translated message for an argument that cannot be sent.
fn arg_error_text(e: &ArgError) -> String {
    let n = e.index() + 1;
    match e {
        ArgError::InvalidJson { message, .. } => {
            trf("Argument {}: {}", &[&n, &format!("{} ({message})", tr("Invalid JSON"))])
        }
        ArgError::InvalidBinary { .. } => trf("Argument {}: {}", &[&n, &tr("Binary messages must be hex or base64")]),
        ArgError::File { path, message, .. } => trf("Argument {}: cannot read {}: {}", &[&n, path, message]),
    }
}

/// Emits an event; arguments that cannot be sent are reported.
fn emit(app: &mut MerpatiApp, tab: &SioTab, collection: Option<&str>, (event, args, ack): Emit) {
    let Some(s) = &tab.session else { return };
    let r = resolver(app, collection);
    match merpati_socketio::resolve_args(&args, &r, Some(app.ws.root())) {
        Ok(args) => {
            s.emit_args(&event, args, ack);
        }
        Err(e) => app.toast(arg_error_text(&e), true),
    }
}

/// Answers the server's acknowledgement request `id` with its reply.
fn answer_ack(app: &mut MerpatiApp, tab: &mut SioTab, id: u64, collection: Option<&str>) {
    let (Some(s), Some(reply)) = (&tab.session, tab.ack_replies.get(&id)) else { return };
    let r = resolver(app, collection);
    let value = r(&reply.value);
    let args = match reply.kind {
        SocketIoArgKind::Json => Ok(merpati_socketio::parse_args(&value).into_iter().map(EmitArg::Json).collect()),
        kind => merpati_socketio::resolve_arg(0, kind, &value, Some(app.ws.root())).map(|a| vec![a]),
    };
    match args {
        Ok(args) => {
            let shown: Vec<serde_json::Value> = args.iter().map(EmitArg::to_json).collect();
            if s.ack_args(id, args) {
                let text = serde_json::to_string(&shown).unwrap_or_default();
                tab.log.push(Dir::Out, trf("ack #{}", &[&id]), text, crate::proto_common::now_ms());
            }
        }
        Err(e) => app.toast(arg_error_text(&e), true),
    }
}

/// Event names the request listens to (for filtering).
pub fn listened(app: &MerpatiApp, tab: &SioTab) -> Vec<String> {
    match tab.slot.load(app).map(|i| i.request) {
        Some(RequestKind::SocketIo(r)) => r.listeners.into_iter().filter(|l| l.enabled).map(|l| l.event).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_auth_reaches_the_handshake() {
        use merpati_core::model::{Item, RequestItem, RequestKind};
        use merpati_core::protocols::SocketIoRequest;
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let cid = app.new_collection("C");
        let req = SocketIoRequest { url: "http://sio.test".into(), ..Default::default() };
        assert!(req.auth.is_inherit());
        let mut item = RequestItem::new("socket", merpati_core::model::HttpRequest::default());
        item.request = RequestKind::SocketIo(req.clone());
        let item_id = item.id.clone();
        let c = app.ws.data.collection_mut(&cid).unwrap();
        c.auth = merpati_core::auth::Auth::bearer("{{token}}");
        c.variables.push(merpati_core::variables::Variable::new("token", "t0k"));
        c.items.push(Item::Request(item));
        let r = resolver(&app, Some(&cid));
        let cfg = SocketIoConfig::from_request(&with_inherited_auth(&app, Some(&cid), &item_id, &req), &r, true);
        assert!(cfg.headers.iter().any(|(k, v)| k == "Authorization" && v == "Bearer t0k"), "{:?}", cfg.headers);
    }

    /// Connecting, a failure with its cause, and a new attempt.
    #[test]
    fn connection_states() {
        let mut tab = SioTab::new(ItemSlot::default());
        assert_eq!(tab.state(), Conn::Idle);
        tab.on_event(SocketIoEvent::Error("connection refused".into()), &[]);
        assert_eq!(tab.state(), Conn::Failed);
        tab.on_event(SocketIoEvent::Connected { sid: "s1".into(), namespace: "/".into() }, &[]);
        assert_eq!(tab.state(), Conn::Live);
        assert!(tab.error.is_none());
    }
}
