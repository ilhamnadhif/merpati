//! gRPC request tab (FR-X2): schema via server reflection or `.proto` files,
//! service/method picker, unary and streaming calls with JSON messages.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use egui::{RichText, Ui};

use merpati_core::model::RequestKind;
use merpati_core::protocols::GrpcSchemaSource;
use merpati_grpc::{Direction, GrpcCall, GrpcEvent, GrpcSchema, MethodKind, Target};

use crate::app::{MerpatiApp, Msg};
use crate::highlight::Lang;
use crate::i18n::{tr, trf};
use crate::proto_common::{self as pc, Conn, Dir, ItemSlot, Log, ScriptState, header, resolver};
use crate::widgets::{self, KvOptions};
use crate::{icons, kit, theme};
use merpati_core::script::ScriptEvent;
use merpati_core::script::protocol as proto;

pub enum GrpcMsg {
    Schema(Result<Arc<GrpcSchema>, String>),
    Event(GrpcEvent),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Message,
    Metadata,
    Auth,
    Schema,
    Scripts,
    Settings,
}

/// Script work waiting for the next frame (it needs the request).
enum Queued {
    Message(serde_json::Value),
    Response(serde_json::Value),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RespSection {
    Messages,
    Headers,
    Trailers,
}

pub struct GrpcTab {
    /// Number of the current call (see `app::next_epoch`).
    pub epoch: u64,
    pub slot: ItemSlot,
    schema: Option<Arc<GrpcSchema>>,
    /// Services and methods of the schema (listed once when it loads).
    methods: Vec<merpati_grpc::ServiceInfo>,
    schema_loading: bool,
    schema_error: Option<String>,
    call: Option<GrpcCall>,
    /// When the current call started.
    started: Option<Instant>,
    section: Section,
    resp_section: RespSection,
    log: Log,
    status: Option<(i32, String, String, f64)>,
    headers: Vec<(String, String)>,
    trailers: Vec<(String, String)>,
    /// Text typed in the method picker.
    method_search: String,
    /// Set by UI automation.
    pub auto_load: bool,
    pub auto_invoke: bool,
    pub scripts: ScriptState,
    received: Vec<serde_json::Value>,
    queue: Vec<Queued>,
}

impl GrpcTab {
    pub fn new(slot: ItemSlot) -> Self {
        let mut log = Log::with_empty("No response yet", "Pick a method and press Invoke to call it.");
        log.title = "";
        GrpcTab {
            epoch: 0,
            slot,
            schema: None,
            methods: Vec::new(),
            schema_loading: false,
            schema_error: None,
            call: None,
            started: None,
            section: Section::Message,
            resp_section: RespSection::Messages,
            log,
            status: None,
            headers: Vec::new(),
            trailers: Vec::new(),
            method_search: String::new(),
            auto_load: false,
            auto_invoke: false,
            scripts: ScriptState::default(),
            received: Vec::new(),
            queue: Vec::new(),
        }
    }

    /// Automation: switch to the Scripts section.
    pub fn show_scripts(&mut self) {
        self.section = Section::Scripts;
    }

    /// The loaded schema (for automation).
    pub fn schema(&self) -> Option<&Arc<GrpcSchema>> {
        self.schema.as_ref()
    }

    pub fn on_msg(&mut self, m: GrpcMsg) {
        match m {
            GrpcMsg::Schema(r) => {
                self.schema_loading = false;
                match r {
                    Ok(s) => {
                        let n: usize = s.services().iter().map(|s| s.methods.len()).sum();
                        self.log.system(trf("Schema loaded: {} services, {} methods", &[&s.services().len(), &n]));
                        for w in s.warnings() {
                            self.log.system(trf("warning: {}", &[&w]));
                        }
                        self.methods = s.services();
                        self.schema = Some(s);
                        self.schema_error = None;
                    }
                    Err(e) => {
                        self.log.error(trf("Schema: {}", &[&e]));
                        self.schema_error = Some(e);
                    }
                }
            }
            GrpcMsg::Event(e) => match e {
                GrpcEvent::Connecting => self.log.system(tr("Connecting…")),
                GrpcEvent::ResponseHeaders(h) => self.headers = h,
                GrpcEvent::Message { direction, json, at_ms } => {
                    let dir = if direction == Direction::Received { Dir::In } else { Dir::Out };
                    if direction == Direction::Received {
                        let m = proto::message(proto::json_or_text(&json), proto::Direction::Incoming, at_ms);
                        self.received.push(m.clone());
                        self.queue.push(Queued::Message(m));
                    }
                    self.log.push(dir, "", json, at_ms);
                }
                GrpcEvent::Trailers(t) => self.trailers = t,
                GrpcEvent::Finished { code, code_name, message, elapsed_ms } => {
                    let message_part = if message.is_empty() { String::new() } else { format!(": {message}") };
                    let took = merpati_core::util::human_ms(elapsed_ms);
                    self.log.system(trf("{} ({}){} in {}", &[&code_name, &code, &message_part, &took]));
                    let resp = proto::grpc_response(
                        code,
                        &message,
                        &self.headers,
                        &self.trailers,
                        std::mem::take(&mut self.received),
                        elapsed_ms,
                    );
                    self.queue.push(Queued::Response(resp));
                    self.status = Some((code, code_name, message, elapsed_ms));
                    self.call = None;
                    self.started = None;
                }
                GrpcEvent::Error(m) => {
                    self.log.error(m);
                }
            },
        }
    }
}

fn target_for(app: &MerpatiApp, tab: &GrpcTab, item: &merpati_core::model::RequestItem) -> Option<Target> {
    let RequestKind::Grpc(req) = &item.request else { return None };
    let collection = tab.slot.collection_id();
    let r = resolver(app, collection.as_deref());
    let mut target = Target::from_request(req, &r, app.settings.request.verify_ssl)
        .with_certificates(app.settings.certificates.clone(), Some(MerpatiApp::vault_resolver(&app.ws)))
        .with_proxy(app.proxy_settings());
    if req.auth.is_inherit()
        && let Some(eff) =
            collection.as_deref().and_then(|c| app.ws.data.collection(c)).and_then(|c| c.effective_auth(&item.id))
    {
        for (k, v) in merpati_grpc::auth_metadata(eff.auth, &r) {
            if !target.metadata.iter().any(|(mk, _)| mk.eq_ignore_ascii_case(&k)) {
                target.metadata.push((k, v));
            }
        }
    }
    Some(target)
}

fn load_schema(app: &mut MerpatiApp, tab_id: u64, tab: &mut GrpcTab, item: &merpati_core::model::RequestItem) {
    let RequestKind::Grpc(req) = &item.request else { return };
    let Some(target) = target_for(app, tab, item) else { return };
    tab.schema_loading = true;
    tab.schema_error = None;
    let source = req.schema.clone();
    let base = app.ws.root().to_path_buf();
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    app.rt.spawn(async move {
        let result = match source {
            GrpcSchemaSource::Reflection => merpati_grpc::load_via_reflection(&target).await.map_err(|e| e.to_string()),
            GrpcSchemaSource::ProtoFiles { files, import_paths } => {
                let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
                let imports: Vec<PathBuf> = import_paths.iter().map(PathBuf::from).collect();
                tokio::task::spawn_blocking(move || {
                    merpati_grpc::load_proto_files(&files, &imports, Some(&base)).map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            }
        };
        let _ = tx.send(Msg::Grpc { tab: tab_id, epoch: 0, event: GrpcMsg::Schema(result.map(Arc::new)) });
        ctx.request_repaint();
    });
}

/// Short label and color of a method kind.
fn kind_look(kind: MethodKind, p: &theme::Palette) -> (&'static str, egui::Color32) {
    match kind {
        MethodKind::Unary => (tr("unary"), p.info),
        MethodKind::ServerStreaming => (tr("server stream"), p.ok),
        MethodKind::ClientStreaming => (tr("client stream"), p.warn),
        MethodKind::Bidi => (tr("bidi stream"), p.accent_text),
    }
}

/// What the method picker asks for.
enum Pick {
    Method(String, String),
    LoadSchema,
}

/// The method picker of the bar: the selected method (or "Load schema"),
/// opening a searchable list of the services' methods with their kinds.
fn method_picker(ui: &mut Ui, tab: &mut GrpcTab, service: &str, method: &str) -> Option<Pick> {
    let p = theme::pal(ui);
    let mut pick = None;
    let w = ui.available_width().max(150.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::click());
    let label = if tab.schema_loading {
        tr("Loading schema…").to_string()
    } else if tab.schema.is_none() {
        tr("Load schema").to_string()
    } else if method.is_empty() {
        tr("Select a method").to_string()
    } else {
        format!("{} / {method}", short(service))
    };
    widgets::a11y_button(&resp, &label);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if resp.hovered() {
            painter.rect_filled(rect.shrink(1.0), egui::CornerRadius::same(5), p.hover);
        }
        let mut x = rect.left() + 8.0;
        let icon = if tab.schema.is_none() { icons::ARROWS_CLOCKWISE } else { icons::TREE_STRUCTURE };
        if tab.schema_loading {
            let t = ui.input(|i| i.time) as f32;
            let c = egui::pos2(x + 7.0, rect.center().y);
            let pts: Vec<egui::Pos2> = (0..=14)
                .map(|i| {
                    let a = t * 6.0 + i as f32 / 14.0 * 4.5;
                    c + 5.0 * egui::vec2(a.cos(), a.sin())
                })
                .collect();
            painter.line(pts, egui::Stroke::new(1.6, p.accent));
            ui.ctx().request_repaint();
        } else {
            painter.text(
                egui::pos2(x + 7.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                icon,
                theme::icon_font(14.0),
                p.text_muted,
            );
        }
        x += 20.0;
        let color = if method.is_empty() || tab.schema.is_none() { p.text_muted } else { p.text };
        let caret_w = if tab.schema.is_some() { 18.0 } else { 0.0 };
        let mut job =
            egui::text::LayoutJob::single_section(label.clone(), egui::TextFormat::simple(theme::medium(13.0), color));
        job.wrap = egui::text::TextWrapping {
            max_width: (rect.right() - caret_w - x - 4.0).max(10.0),
            max_rows: 1,
            break_anywhere: true,
            overflow_character: Some('…'),
        };
        let g = painter.layout_job(job);
        painter.galley(egui::pos2(x, rect.center().y - g.size().y / 2.0), g, color);
        if tab.schema.is_some() {
            painter.text(
                egui::pos2(rect.right() - 10.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                icons::CARET_DOWN,
                theme::icon_font(12.0),
                p.text_muted,
            );
        }
    }
    if tab.schema.is_none() {
        if resp.clicked() && !tab.schema_loading {
            pick = Some(Pick::LoadSchema);
        }
        resp.on_hover_text(tr("Server reflection or the .proto files in Schema"));
        return pick;
    }
    let popup = egui::Popup::from_toggle_button_response(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(380.0);
    popup.show(|ui| {
        ui.set_min_width(380.0);
        let r = kit::search_field(ui, "grpc-method-search", &mut tab.method_search, tr("Search methods"), 364.0);
        if !r.has_focus() && tab.method_search.is_empty() {
            r.request_focus();
        }
        ui.add_space(4.0);
        let q = tab.method_search.to_lowercase();
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            let mut any = false;
            for svc in &tab.methods {
                let methods: Vec<_> = svc
                    .methods
                    .iter()
                    .filter(|m| {
                        q.is_empty() || m.name.to_lowercase().contains(&q) || svc.name.to_lowercase().contains(&q)
                    })
                    .collect();
                if methods.is_empty() {
                    continue;
                }
                any = true;
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::CUBE).font(theme::icon_font(13.0)).color(p.text_faint));
                    ui.label(RichText::new(&svc.name).font(theme::medium(11.5)).color(p.text_muted));
                });
                for m in methods {
                    let selected = svc.name == service && m.name == method;
                    let (rect, r) =
                        ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::click());
                    widgets::a11y_button(&r, &format!("{} {}", svc.name, m.name));
                    let painter = ui.painter();
                    if selected {
                        painter.rect_filled(rect, egui::CornerRadius::same(6), p.accent_soft);
                    } else if r.hovered() {
                        painter.rect_filled(rect, egui::CornerRadius::same(6), p.hover);
                    }
                    let (kind, color) = kind_look(m.kind, &p);
                    let kg = painter.layout_no_wrap(kind.to_string(), theme::medium(10.5), color);
                    let pill = egui::Rect::from_min_size(
                        egui::pos2(rect.right() - kg.size().x - 20.0, rect.center().y - 9.0),
                        egui::vec2(kg.size().x + 12.0, 18.0),
                    );
                    painter.rect_filled(pill, egui::CornerRadius::same(9), theme::soft(color, 0.14));
                    painter.galley(pill.center() - kg.size() / 2.0, kg, color);
                    let ng = painter.layout_no_wrap(m.name.clone(), theme::medium(13.0), p.text);
                    painter.galley(egui::pos2(rect.left() + 10.0, rect.top() + 5.0), ng, p.text);
                    let io = format!("{} → {}", short(&m.input_type), short(&m.output_type));
                    let ig = painter.layout_no_wrap(io, theme::mono(11.0), p.text_muted);
                    painter.galley(egui::pos2(rect.left() + 10.0, rect.top() + 22.0), ig, p.text_muted);
                    if r.clicked() {
                        pick = Some(Pick::Method(svc.name.clone(), m.name.clone()));
                        ui.close();
                    }
                }
            }
            if !any {
                ui.add_space(8.0);
                ui.label(RichText::new(tr("No methods match")).color(p.text_muted));
            }
        });
        ui.add_space(4.0);
        ui.separator();
        if kit::ghost(ui, Some(icons::ARROWS_CLOCKWISE), tr("Reload schema")).clicked() {
            pick = Some(Pick::LoadSchema);
            ui.close();
        }
    });
    pick
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut GrpcTab) {
    let Some(original) = tab.slot.load(app) else {
        ui.label(tr("This request no longer exists."));
        return;
    };
    let mut item = original.clone();
    let collection = tab.slot.collection_id();
    let vars = app.var_ctx(collection.as_deref());
    let methods_n: usize = tab.methods.iter().map(|s| s.methods.len()).sum();
    let (schema_state, schema_text) = if tab.schema_loading {
        (Conn::Busy, tr("Loading schema…").to_string())
    } else if tab.schema.is_some() {
        (Conn::Live, trf("{} methods", &[&methods_n]))
    } else if tab.schema_error.is_some() {
        (Conn::Failed, tr("Schema error").to_string())
    } else {
        (Conn::Idle, tr("No schema").to_string())
    };
    let source = match &item.request {
        RequestKind::Grpc(r) if matches!(r.schema, GrpcSchemaSource::Reflection) => tr("server reflection"),
        _ => tr(".proto files"),
    };
    header(ui, app, tab_id, &tab.slot, &mut item, |ui| {
        pc::status_pill(ui, schema_state, &schema_text).on_hover_text(trf("Schema from {}", &[&source]));
    });
    let mut do_load = false;
    let mut do_invoke = false;
    let mut stream_send = false;
    {
        let RequestKind::Grpc(req) = &mut item.request else { return };
        let method_kind = tab.schema.as_ref().and_then(|s| s.method(&req.service, &req.method)).map(|m| m.kind);
        let running = tab.call.is_some();
        let ready = tab.schema.is_some() && !req.method.is_empty();
        let has_schema = tab.schema.is_some();
        let mut cancel = false;
        let mut invoke_clicked = false;
        let mut picked = None;
        pc::connect_bar(
            ui,
            "gRPC",
            132.0,
            |ui| {
                let picker_w = (ui.available_width() * 0.45).clamp(180.0, 340.0);
                let w = (ui.available_width() - picker_w - 10.0).max(120.0);
                let r = widgets::var_cell(ui, "grpc-url", &mut req.url, &vars, "localhost:50051", w);
                kit::vdivider(ui, 22.0);
                let (service, method) = (req.service.clone(), req.method.clone());
                picked = method_picker(ui, tab, &service, &method);
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if tab.schema.is_none() {
                        do_load = true;
                    } else {
                        do_invoke = !running;
                    }
                }
                r.has_focus()
            },
            |ui| {
                if running {
                    cancel = kit::secondary(ui, Some(icons::STOP), tr("Cancel")).clicked();
                } else {
                    let label = match method_kind {
                        Some(MethodKind::ClientStreaming) | Some(MethodKind::Bidi) => tr("Start stream"),
                        _ => tr("Invoke"),
                    };
                    let r = ui.add_enabled_ui(ready, |ui| kit::primary_icon(ui, icons::PLAY, label)).inner;
                    let hint = if has_schema { tr("Select a method") } else { tr("Load the schema first") };
                    if r.on_hover_text("⇧⌘↩").on_disabled_hover_text(hint).clicked() {
                        invoke_clicked = true;
                    }
                }
            },
        );
        do_invoke |= invoke_clicked;
        match picked {
            Some(Pick::Method(s, m)) => {
                if let Some(info) = tab.schema.as_ref().and_then(|sc| sc.method(&s, &m))
                    && (req.message.trim().is_empty() || req.method != m)
                {
                    req.message = info.input_template.clone();
                }
                req.service = s;
                req.method = m;
                tab.method_search.clear();
            }
            Some(Pick::LoadSchema) => do_load = true,
            None => {}
        }
        if cancel && let Some(call) = &tab.call {
            call.cancel();
        }
        if pc::send_shortcut(ui) {
            match &tab.call {
                Some(call) if call.can_send() => stream_send = true,
                None if tab.schema.is_some() && !req.method.is_empty() => do_invoke = true,
                _ => {}
            }
        }
        if let Some(e) = tab.schema_error.clone() {
            ui.add_space(8.0);
            let mut dismiss = false;
            pc::error_banner(ui, tr("Could not load the schema"), &e, |ui| {
                dismiss = kit::icon_button(ui, icons::X, tr("Dismiss")).clicked();
                if kit::secondary(ui, Some(icons::ARROW_CLOCKWISE), tr("Retry")).clicked() {
                    do_load = true;
                }
            });
            if dismiss {
                tab.schema_error = None;
            }
        }
        let metadata_n = req.metadata.iter().filter(|m| m.enabled).count();
        let items = vec![
            (Section::Message, tr("Message").to_string()),
            (Section::Metadata, trf("Metadata{}", &[&widgets::count_label(metadata_n)])),
            (Section::Auth, tr("Authorization").to_string()),
            (Section::Schema, tr("Schema").to_string()),
            (Section::Scripts, pc::scripts_label(&item.scripts)),
            (Section::Settings, tr("Settings").to_string()),
        ];
        ui.add_space(8.0);
        pc::section_tabs(ui, "grpc-tabs", &mut tab.section, &items);
        ui.add_space(8.0);
        let call = &tab.call;
        let schema = &tab.schema;
        let schema_loading = tab.schema_loading;
        let methods = &tab.methods;
        let section = tab.section;
        let scripts_state = &mut tab.scripts;
        let scripts = &mut item.scripts;
        let item_id = item.id.clone();
        let log = &mut tab.log;
        let resp_section = &mut tab.resp_section;
        let status = &tab.status;
        let headers = &tab.headers;
        let trailers = &tab.trailers;
        let started = tab.started;
        let mut end_stream = false;
        pc::split_view(
            ui,
            ("grpc-split", tab_id),
            0.5,
            |ui| match section {
                Section::Message => {
                    let p = theme::pal(ui);
                    let info = schema.as_ref().and_then(|s| s.method(&req.service, &req.method));
                    ui.horizontal(|ui| {
                        ui.set_height(30.0);
                        match &info {
                            Some(m) => {
                                let (kind, color) = kind_look(m.kind, &p);
                                kit::pill(ui, kind, color);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(format!("{} → {}", short(&m.input_type), short(&m.output_type)))
                                            .monospace()
                                            .color(p.text_muted),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(format!("{} → {}", m.input_type, m.output_type));
                            }
                            None => {
                                ui.label(kit::hint(ui, tr("Pick a method to get a template of its input message.")));
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if let Some(c) = call {
                                if c.can_send() && pc::send_button(ui, icons::PAPER_PLANE_RIGHT, tr("Send"), true, "") {
                                    stream_send = true;
                                }
                                if c.kind().is_client_streaming()
                                    && kit::secondary(ui, Some(icons::STOP), tr("End stream")).clicked()
                                {
                                    end_stream = true;
                                }
                            } else if let Some(m) = &info
                                && kit::ghost(ui, Some(icons::SPARKLE), tr("Use example"))
                                    .on_hover_text(tr("Fill with a template of the input message"))
                                    .clicked()
                            {
                                req.message = m.input_template.clone();
                            }
                        });
                    });
                    ui.add_space(6.0);
                    let r = pc::fill_editor(ui, "grpc-msg", &mut req.message, Lang::Json, &vars, 6);
                    pc::editor_placeholder(ui, &r, &req.message, tr("The request message as JSON"));
                }
                Section::Metadata => {
                    egui::ScrollArea::vertical().id_salt("grpc-md-scroll").show(ui, |ui| {
                        let mut md = req.metadata.clone();
                        let opts = KvOptions {
                            title: Some(tr("Metadata")),
                            allow_add: true,
                            show_description: true,
                            key_hint: tr("Key"),
                            value_hint: tr("Value"),
                            ..Default::default()
                        };
                        if widgets::kv_table(ui, "grpc-metadata", &mut md, &vars, opts) {
                            req.metadata = md;
                        }
                        ui.label(kit::hint(ui, tr("Keys ending in -bin take base64 values.")));
                    });
                }
                Section::Auth => {
                    egui::ScrollArea::vertical().id_salt("grpc-auth").show(ui, |ui| {
                        ui.label(kit::hint(ui, tr("Bearer, Basic and API key (header) are sent as metadata.")));
                        ui.add_space(4.0);
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
                Section::Schema => {
                    egui::ScrollArea::vertical().id_salt("grpc-schema").show(ui, |ui| {
                        schema_section(ui, req, schema.is_some(), schema_loading, methods, &mut do_load)
                    });
                }
                Section::Scripts => crate::proto_common::scripts_ui(
                    ui,
                    scripts,
                    &[
                        (ScriptEvent::PreRequest, "Before invoke"),
                        (ScriptEvent::OnMessage, "On message"),
                        (ScriptEvent::PostResponse, "After response"),
                    ],
                    scripts_state,
                    &vars,
                ),
                Section::Settings => {
                    egui::ScrollArea::vertical().id_salt("grpc-settings").show(ui, |ui| {
                        pc::form_grid("grpc-settings").show(ui, |ui| {
                            pc::form_label(ui, "TLS");
                            ui.push_id("tls", |ui| {
                                kit::segmented(
                                    ui,
                                    &mut req.tls,
                                    &[(None, tr("By scheme")), (Some(true), tr("On")), (Some(false), tr("Off"))],
                                )
                            });
                            ui.end_row();
                            pc::form_label(ui, tr("Verify SSL certificates"));
                            let mut v = req.verify_ssl.unwrap_or(true);
                            if ui.checkbox(&mut v, tr("When TLS is on")).changed() {
                                req.verify_ssl = Some(v);
                            }
                            ui.end_row();
                            pc::form_label(ui, tr("Deadline (ms, 0 = none)"));
                            let mut d = req.deadline_ms.unwrap_or(0);
                            if ui.add(egui::DragValue::new(&mut d).range(0..=3_600_000)).changed() {
                                req.deadline_ms = (d > 0).then_some(d);
                            }
                            ui.end_row();
                        });
                    });
                }
            },
            |ui| {
                let p = theme::pal(ui);
                let n = log.lines.iter().filter(|l| l.dir != Dir::System).count();
                let items = vec![
                    (RespSection::Messages, format!("{}{}", tr("Messages"), widgets::count_label(n))),
                    (RespSection::Headers, format!("{}{}", tr("Headers"), widgets::count_label(headers.len()))),
                    (RespSection::Trailers, trf("Trailers{}", &[&widgets::count_label(trailers.len())])),
                ];
                ui.horizontal(|ui| {
                    widgets::sub_tabs(ui, resp_section, &items);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(s) = started {
                            let ms = s.elapsed().as_secs_f64() * 1000.0;
                            kit::metric(ui, icons::TIMER, &merpati_core::util::human_ms(ms));
                            pc::status_pill(ui, Conn::Busy, tr("Calling…"));
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                        } else if let Some((code, name, message, elapsed)) = status {
                            kit::metric(ui, icons::TIMER, &merpati_core::util::human_ms(*elapsed));
                            let color = if *code == 0 { p.ok } else { p.err };
                            let r = kit::pill(ui, &format!("{code} {name}"), color);
                            if !message.is_empty() {
                                r.on_hover_text(message);
                            }
                        }
                    });
                });
                ui.add_space(6.0);
                match *resp_section {
                    RespSection::Messages => log.ui(ui, ("grpc-log", tab_id)),
                    RespSection::Headers => {
                        egui::ScrollArea::vertical().id_salt("grpc-headers").show(ui, |ui| {
                            pc::kv_view(
                                ui,
                                "grpc-headers",
                                headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                                tr("No response headers yet."),
                            )
                        });
                    }
                    RespSection::Trailers => {
                        egui::ScrollArea::vertical().id_salt("grpc-trailers").show(ui, |ui| {
                            pc::kv_view(
                                ui,
                                "grpc-trailers",
                                trailers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                                tr("No trailers yet."),
                            )
                        });
                    }
                }
            },
        );
        if end_stream && let Some(c) = &tab.call {
            c.end_stream();
        }
        if stream_send && let Some(c) = &tab.call {
            let r = resolver(app, collection.as_deref());
            if let Err(e) = c.send(&r(&req.message)) {
                tab.log.error(e.to_string());
            }
        }
    }
    do_load |= std::mem::take(&mut tab.auto_load);
    do_invoke |= std::mem::take(&mut tab.auto_invoke) && tab.schema.is_some();
    if do_load {
        load_schema(app, tab_id, tab, &item);
    }
    if do_invoke && tab.schema.is_some() && tab.call.is_none() {
        tab.scripts.clear_results();
        let pre = match &item.request {
            RequestKind::Grpc(r) => {
                crate::proto_common::run_scripts(app, tab_id, &tab.slot, &item, ScriptEvent::PreRequest, proto::grpc(r))
            }
            _ => false,
        };
        if pre {
            tab.scripts.waiting = true;
            tab.log.system(tr("Running the pre-invoke script…"));
        } else {
            invoke(app, tab_id, tab, &item);
        }
    }
    // The pre-invoke script finished: send what it left in pm.request.
    if let Some(pre) = tab.scripts.take_pre() {
        match pre {
            None => tab.log.system(tr("Skipped by pm.execution.skipRequest()")),
            Some(v) => {
                let mut sent = item.clone();
                if let RequestKind::Grpc(r) = &mut sent.request {
                    proto::apply_grpc(r, &v);
                }
                invoke(app, tab_id, tab, &sent);
            }
        }
    }
    if let RequestKind::Grpc(r) = &item.request {
        for q in std::mem::take(&mut tab.queue) {
            let (event, ctx) = match q {
                Queued::Message(m) => (ScriptEvent::OnMessage, proto::grpc(r).with_message(m)),
                Queued::Response(v) => (ScriptEvent::PostResponse, proto::grpc(r).with_response(v)),
            };
            crate::proto_common::run_scripts(app, tab_id, &tab.slot, &item, event, ctx);
        }
    }
    tab.slot.store(app, &original, item);
}

/// Where the schema comes from: server reflection or `.proto` files.
fn schema_section(
    ui: &mut Ui,
    req: &mut merpati_core::protocols::GrpcRequest,
    loaded: bool,
    loading: bool,
    methods: &[merpati_grpc::ServiceInfo],
    do_load: &mut bool,
) {
    let p = theme::pal(ui);
    let mut reflection = matches!(req.schema, GrpcSchemaSource::Reflection);
    ui.horizontal(|ui| {
        if kit::segmented(ui, &mut reflection, &[(true, tr("Server reflection")), (false, tr(".proto files"))]) {
            req.schema = if reflection {
                GrpcSchemaSource::Reflection
            } else {
                GrpcSchemaSource::ProtoFiles { files: Vec::new(), import_paths: Vec::new() }
            };
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if loading {
                ui.add(egui::Spinner::new().size(16.0).color(p.accent));
            } else {
                let label = if loaded { tr("Reload schema") } else { tr("Load schema") };
                if kit::secondary(ui, Some(icons::ARROWS_CLOCKWISE), label).clicked() {
                    *do_load = true;
                }
            }
        });
    });
    ui.add_space(8.0);
    match &mut req.schema {
        GrpcSchemaSource::Reflection => {
            ui.label(kit::hint(ui, tr("The server lists its services itself (grpc.reflection v1 or v1alpha).")));
        }
        GrpcSchemaSource::ProtoFiles { files, import_paths } => {
            let mut remove = None;
            let mut remove_import = None;
            if files.is_empty() {
                ui.label(kit::hint(ui, tr("No .proto files yet.")));
            }
            for (i, f) in files.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::FILE_CODE).font(theme::icon_font(15.0)).color(p.text_muted));
                    ui.add(egui::Label::new(RichText::new(f).monospace()).truncate());
                    if kit::icon_button(ui, icons::X, tr("Remove file")).clicked() {
                        remove = Some(i);
                    }
                });
            }
            for (i, d) in import_paths.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::FOLDER).font(theme::icon_font(15.0)).color(p.text_muted));
                    ui.add(egui::Label::new(RichText::new(d).monospace().color(p.text_muted)).truncate());
                    kit::pill(ui, tr("import path"), p.text_muted);
                    if kit::icon_button(ui, icons::X, tr("Remove import path")).clicked() {
                        remove_import = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                files.remove(i);
            }
            if let Some(i) = remove_import {
                import_paths.remove(i);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if kit::ghost(ui, Some(icons::FILE_PLUS), tr("Add .proto files")).clicked()
                    && let Some(paths) = rfd::FileDialog::new().add_filter("Protocol Buffers", &["proto"]).pick_files()
                {
                    files.extend(paths.into_iter().map(|p| p.display().to_string()));
                }
                if kit::ghost(ui, Some(icons::FOLDER_PLUS), tr("Add import path")).clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    import_paths.push(dir.display().to_string());
                }
            });
        }
    }
    if !methods.is_empty() {
        ui.add_space(12.0);
        pc::section(ui, tr("Services"), None);
        for svc in methods {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::CUBE).font(theme::icon_font(14.0)).color(p.text_muted));
                ui.label(RichText::new(&svc.name).monospace());
                pc::count_badge(ui, svc.methods.len());
            });
        }
    }
}

fn invoke(app: &mut MerpatiApp, tab_id: u64, tab: &mut GrpcTab, item: &merpati_core::model::RequestItem) {
    let (Some(schema), Some(target)) = (tab.schema.clone(), target_for(app, tab, item)) else { return };
    let RequestKind::Grpc(req) = &item.request else { return };
    let collection = tab.slot.collection_id();
    let r = resolver(app, collection.as_deref());
    tab.log.clear();
    tab.status = None;
    tab.headers.clear();
    tab.trailers.clear();
    tab.received.clear();
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    let first = Some(r(&req.message)).filter(|m| !m.trim().is_empty());
    tab.epoch = crate::app::next_epoch();
    let epoch = tab.epoch;
    match GrpcCall::start(app.rt.handle(), target, schema, &req.service, &req.method, first, move |event| {
        let _ = tx.send(Msg::Grpc { tab: tab_id, epoch, event: GrpcMsg::Event(event) });
        ctx.request_repaint();
    }) {
        Ok(call) => {
            tab.call = Some(call);
            tab.started = Some(Instant::now());
            tab.resp_section = RespSection::Messages;
        }
        Err(e) => tab.log.error(e.to_string()),
    }
}

fn short(service: &str) -> &str {
    service.rsplit('.').next().unwrap_or(service)
}
