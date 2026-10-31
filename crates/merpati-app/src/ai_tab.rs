//! AI request tab (FR-X9): chat requests to OpenAI, Anthropic, Gemini or an
//! OpenAI-compatible endpoint, streamed, with tools from MCP requests.

use std::time::Instant;

use egui::{Color32, CornerRadius, RichText, Stroke, Ui};

use merpati_ai::merpati_mcp::{McpClient, McpConfig};
use merpati_ai::{AiConfig, AiEvent, AiResponse, CancellationToken};
use merpati_core::auth::{Auth, AuthType};
use merpati_core::model::RequestKind;
use merpati_core::protocols::{AiMessage, AiProvider, AiRequest};

use crate::app::{MerpatiApp, Msg};
use crate::highlight::Lang;
use crate::i18n::{tr, trf};
use crate::proto_common::{self as pc, Conn, ItemSlot, ScriptState, header, resolver};
use crate::widgets::{self, VarCtx};
use crate::{icons, kit, theme};
use merpati_core::script::ScriptEvent;
use merpati_core::script::protocol as proto;

pub enum AiMsg {
    Event(AiEvent),
    Models(Result<Vec<String>, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Messages,
    Settings,
    Auth,
    Tools,
    Scripts,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Chat,
    Raw,
}

pub struct AiTab {
    /// Number of the current send (see `app::next_epoch`).
    pub epoch: u64,
    pub slot: ItemSlot,
    section: Section,
    view: View,
    cancel: Option<CancellationToken>,
    text: String,
    /// Tool calls and results of the last run, in order: (call, name, body).
    steps: Vec<(bool, String, String)>,
    usage: Option<(u64, u64)>,
    done: Option<AiResponse>,
    error: Option<String>,
    models: Vec<String>,
    fetching_models: bool,
    /// When the current send started.
    started: Option<Instant>,
    /// Automation: `send`.
    pub auto: Option<String>,
    pub scripts: ScriptState,
    /// Post-response script input, run on the next frame.
    post: Option<serde_json::Value>,
}

impl AiTab {
    pub fn new(slot: ItemSlot) -> Self {
        AiTab {
            epoch: 0,
            slot,
            section: Section::Messages,
            view: View::Chat,
            cancel: None,
            text: String::new(),
            steps: Vec::new(),
            usage: None,
            done: None,
            error: None,
            models: Vec::new(),
            fetching_models: false,
            started: None,
            auto: None,
            scripts: ScriptState::default(),
            post: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.cancel.is_some()
    }

    pub fn cancel(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
        self.started = None;
    }

    pub fn on_msg(&mut self, m: AiMsg) {
        match m {
            AiMsg::Event(e) => match e {
                AiEvent::Started => {}
                AiEvent::Delta(d) => self.text.push_str(&d),
                AiEvent::ToolCall { name, arguments } => {
                    self.steps.push((true, name, serde_json::to_string_pretty(&arguments).unwrap_or_default()))
                }
                AiEvent::ToolResult { name, result } => self.steps.push((false, name, result)),
                AiEvent::Usage { input_tokens, output_tokens } => self.usage = Some((input_tokens, output_tokens)),
                AiEvent::Done(r) => {
                    self.cancel = None;
                    self.started = None;
                    if self.text.is_empty() {
                        self.text = r.text.clone();
                    }
                    let usage = serde_json::json!({"inputTokens": r.input_tokens, "outputTokens": r.output_tokens});
                    let finish = r.finish_reason.clone().unwrap_or_default();
                    self.post = Some(proto::ai_response(&self.text, &r.model, &finish, usage, 200, r.latency_ms));
                    self.done = Some(r);
                }
                AiEvent::Error(e) => {
                    self.cancel = None;
                    self.started = None;
                    self.error = Some(e);
                }
            },
            AiMsg::Models(r) => {
                self.fetching_models = false;
                match r {
                    Ok(m) => self.models = m,
                    Err(e) => self.error = Some(trf("Could not list models: {}", &[&e])),
                }
            }
        }
    }
}

/// The API key (Bearer token) with variables resolved.
fn api_key(req: &AiRequest, resolve: &dyn Fn(&str) -> String) -> Auth {
    let mut auth = req.auth.clone();
    for key in ["token", "value"] {
        let v = auth.get_string(key);
        if !v.is_empty() {
            auth.set(key, resolve(&v));
        }
    }
    auth
}

/// Who wrote a chat bubble.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Who {
    User,
    Assistant,
    Tool,
}

/// A chat bubble: the user's on the right, the others on the left. `add`
/// draws the content; returns its result and whether the bubble has focus
/// (drawn with a ring).
fn bubble<R>(ui: &mut Ui, who: Who, label: &str, add: impl FnOnce(&mut Ui) -> (R, bool)) -> R {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let max_w = (ui.available_width() * 0.82).max(240.0).min(ui.available_width());
    let layout = if who == Who::User {
        egui::Layout::right_to_left(egui::Align::Min)
    } else {
        egui::Layout::left_to_right(egui::Align::Min)
    };
    let (fill, stroke) = match who {
        Who::User => (if dark { theme::soft(p.accent, 0.16) } else { p.accent_soft }, Stroke::NONE),
        Who::Assistant => (p.surface, Stroke::new(1.0, p.border)),
        Who::Tool => (p.chrome, Stroke::new(1.0, p.border)),
    };
    let (icon, icon_color) = match who {
        Who::User => (icons::USER, p.accent_text),
        Who::Assistant => (icons::SPARKLE, p.info),
        Who::Tool => (icons::PUZZLE_PIECE, p.text_muted),
    };
    ui.with_layout(layout, |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        // Avatar.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 13.0, theme::soft(icon_color, 0.14));
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, theme::icon_font(14.0), icon_color);
        let inner = egui::Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius { nw: 12, ne: 12, sw: 12, se: 12 })
            .inner_margin(egui::Margin { left: 12, right: 12, top: 8, bottom: 10 })
            .show(ui, |ui| {
                ui.set_max_width(max_w - 34.0);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.label(RichText::new(label).font(theme::medium(11.5)).color(p.text_muted));
                    add(ui)
                })
                .inner
            });
        let (r, focused) = inner.inner;
        if focused {
            crate::kit::focus_ring(ui, inner.response.rect, 12);
        }
        r
    })
    .inner
}

/// A code editor without its own frame (inside a bubble or card).
fn bare_editor(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    vars: &VarCtx,
    rows: usize,
) -> egui::Response {
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.text_edit_bg_color = Some(Color32::TRANSPARENT);
        v.widgets.inactive.bg_stroke = Stroke::NONE;
        v.widgets.hovered.bg_stroke = Stroke::NONE;
        v.widgets.active.bg_stroke = Stroke::NONE;
        v.selection.stroke = Stroke::NONE;
        ui.spacing_mut().button_padding = egui::vec2(0.0, 2.0);
        widgets::code_editor(ui, id, text, Lang::Text, Some(vars), rows, false)
    })
    .inner
}

/// Three dots that pulse while the answer streams.
fn typing_dots(ui: &mut Ui) {
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 16.0), egui::Sense::hover());
    let t = ui.input(|i| i.time) as f32;
    for i in 0..3 {
        let phase = (t * 4.0 - i as f32 * 0.7).sin() * 0.5 + 0.5;
        let c = egui::pos2(rect.left() + 6.0 + i as f32 * 11.0, rect.center().y - phase * 2.5);
        ui.painter().circle_filled(c, 3.0, theme::soft(p.text_muted, 0.35 + phase * 0.6));
    }
    ui.ctx().request_repaint();
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut AiTab) {
    let Some(original) = tab.slot.load(app) else {
        ui.label(tr("This request no longer exists."));
        return;
    };
    let mut item = original.clone();
    let collection = tab.slot.collection_id();
    let vars = app.var_ctx(collection.as_deref());
    let running = tab.is_running();
    let status = if running {
        Some((Conn::Busy, tr("Generating…").to_string()))
    } else if tab.error.is_some() {
        Some((Conn::Failed, tr("Failed").to_string()))
    } else if tab.done.is_some() {
        Some((Conn::Live, tr("Completed").to_string()))
    } else {
        None
    };
    header(ui, app, tab_id, &tab.slot, &mut item, |ui| {
        if let Some((s, t)) = &status {
            pc::status_pill(ui, *s, t);
        }
    });
    let RequestKind::Ai(req) = &mut item.request else { return };
    let mut send = false;
    let mut stop = false;
    let mut fetch_models = false;
    let suggestions: Vec<String> = if tab.models.is_empty() {
        merpati_ai::suggested_models(req.provider).iter().map(|s| s.to_string()).collect()
    } else {
        tab.models.clone()
    };
    let fetching = tab.fetching_models;
    let mut clear_models = false;
    let send_clicked = pc::connect_bar(
        ui,
        "AI",
        104.0,
        |ui| {
            ui.scope(|ui| {
                pc::frameless(ui);
                let r = egui::ComboBox::from_id_salt(("ai-provider", tab_id))
                    .icon(crate::kit::combo_caret)
                    .width((ui.available_width() * 0.3).clamp(96.0, 150.0))
                    .selected_text(RichText::new(req.provider.label()).font(theme::medium(12.5)))
                    .show_ui(ui, |ui| {
                        for p in AiProvider::ALL {
                            if ui.selectable_label(req.provider == p, p.label()).clicked() && req.provider != p {
                                req.provider = p;
                                req.model.clear();
                                clear_models = true;
                            }
                        }
                    })
                    .response
                    .on_hover_text(tr("Provider"));
                widgets::a11y_name(&r, tr("Provider"));
            });
            kit::vdivider(ui, 22.0);
            let w = (ui.available_width() - 30.0).max(100.0);
            let r = widgets::var_cell(ui, "ai-model", &mut req.model, &vars, tr("model"), w);
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !running {
                send = true;
            }
            let more = kit::icon_button_ex(ui, icons::CARET_DOWN, tr("Choose a model"), false, 26.0);
            egui::Popup::menu(&more).show(|ui| {
                ui.set_min_width(240.0);
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for m in &suggestions {
                        let mark = if &req.model == m { icons::CHECK } else { "    " };
                        if ui.button(format!("{mark}  {m}")).clicked() {
                            req.model = m.clone();
                            ui.close();
                        }
                    }
                });
                ui.separator();
                if fetching {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(tr("Fetching models…"));
                    });
                } else if ui.button(kit::with_icon(icons::ARROWS_CLOCKWISE, tr("Fetch models from provider"))).clicked()
                {
                    fetch_models = true;
                    ui.close();
                }
            });
            r.has_focus()
        },
        |ui| {
            if running {
                stop = kit::secondary(ui, Some(icons::STOP), tr("Stop")).clicked();
                false
            } else {
                kit::primary_icon(ui, icons::PAPER_PLANE_RIGHT, tr("Send")).on_hover_text("⇧⌘↩").clicked()
            }
        },
    );
    send |= send_clicked;
    if clear_models {
        tab.models.clear();
    }
    if stop {
        tab.cancel();
    }
    if !running && pc::send_shortcut(ui) {
        send = true;
    }
    let items = vec![
        (Section::Messages, trf("Conversation{}", &[&widgets::count_label(req.messages.len())])),
        (Section::Settings, tr("Settings").to_string()),
        (Section::Auth, tr("Authorization").to_string()),
        (Section::Tools, trf("MCP tools{}", &[&widgets::count_label(req.mcp_servers.len())])),
        (Section::Scripts, pc::scripts_label(&item.scripts)),
    ];
    ui.add_space(8.0);
    pc::section_tabs(ui, "ai-tabs", &mut tab.section, &items);
    ui.add_space(8.0);
    match tab.section {
        Section::Messages => conversation(ui, tab, req, &vars, &mut send),
        Section::Settings => {
            egui::ScrollArea::vertical().id_salt("ai-settings").show(ui, |ui| {
                pc::form_grid("ai-settings").show(ui, |ui| {
                    pc::form_label(ui, tr("Base URL"));
                    widgets::var_line(
                        ui,
                        "ai-base",
                        &mut req.base_url,
                        &vars,
                        merpati_ai::default_base_url(req.provider),
                        360.0,
                    );
                    ui.end_row();
                    pc::form_label(ui, tr("Stream"));
                    ui.checkbox(&mut req.stream, tr("Stream the response as it is generated"));
                    ui.end_row();
                    pc::form_label(ui, tr("Temperature"));
                    ui.horizontal(|ui| {
                        let mut on = req.temperature.is_some();
                        if ui.checkbox(&mut on, "").changed() {
                            req.temperature = on.then_some(1.0);
                        }
                        if let Some(t) = &mut req.temperature {
                            widgets::a11y_name(
                                &ui.add(egui::Slider::new(t, 0.0..=2.0).step_by(0.05)),
                                tr("Temperature"),
                            );
                        } else {
                            ui.label(widgets::muted(ui, tr("provider default")));
                        }
                    });
                    ui.end_row();
                    pc::form_label(ui, tr("Max tokens"));
                    ui.horizontal(|ui| {
                        let mut on = req.max_tokens.is_some();
                        if ui.checkbox(&mut on, "").changed() {
                            req.max_tokens = on.then_some(1024);
                        }
                        if let Some(t) = &mut req.max_tokens {
                            widgets::a11y_name(&ui.add(egui::DragValue::new(t).range(1..=1_000_000)), tr("Max tokens"));
                        } else {
                            ui.label(widgets::muted(ui, tr("provider default")));
                        }
                    });
                    ui.end_row();
                });
                ui.add_space(14.0);
                pc::section(ui, tr("Extra body"), Some(tr("JSON merged into the provider request")));
                let r = widgets::code_editor(ui, "ai-extra", &mut req.extra, Lang::Json, Some(&vars), 4, false);
                pc::editor_placeholder(ui, &r, &req.extra, "{ \"top_p\": 0.9 }");
            });
        }
        Section::Auth => {
            egui::ScrollArea::vertical().id_salt("ai-auth").show(ui, |ui| {
                ui.label(kit::hint(
                    ui,
                    tr("API key, sent as the provider expects. Keep it in the vault, e.g. {{vault:openai}}."),
                ));
                ui.add_space(6.0);
                if req.auth.kind != AuthType::Bearer && req.auth.kind != AuthType::ApiKey {
                    req.auth = Auth::bearer("");
                }
                let key = if req.auth.kind == AuthType::Bearer { "token" } else { "value" };
                let mut v = req.auth.get_string(key);
                pc::form_grid("ai-auth").show(ui, |ui| {
                    pc::form_label(ui, tr("API key"));
                    let r = if v.is_empty() || merpati_core::variables::is_pure_reference(&v) {
                        widgets::var_line(ui, "ai-key", &mut v, &vars, "{{vault:openai}}", 320.0)
                    } else {
                        widgets::secret_field(ui, "ai-key", &mut v, 346.0)
                    };
                    if r.changed() {
                        req.auth.set(key, v.clone());
                    }
                    ui.end_row();
                });
                if req.provider == AiProvider::OpenAiCompatible {
                    ui.add_space(4.0);
                    ui.label(kit::hint(ui, tr("Local servers such as Ollama or LM Studio usually need no key.")));
                }
            });
        }
        Section::Scripts => crate::proto_common::scripts_ui(
            ui,
            &mut item.scripts,
            &[(ScriptEvent::PreRequest, "Before send"), (ScriptEvent::PostResponse, "After response")],
            &mut tab.scripts,
            &vars,
        ),
        Section::Tools => {
            egui::ScrollArea::vertical().id_salt("ai-tools").show(ui, |ui| {
                pc::section(
                    ui,
                    tr("MCP tools"),
                    Some(tr(
                        "Tools of the selected MCP requests are offered to the model; calls run through those servers.",
                    )),
                );
                ui.add_space(4.0);
                let p = theme::pal(ui);
                let mut any = false;
                for c in &app.ws.data.collections {
                    for r in c.requests() {
                        if !matches!(r.request, RequestKind::Mcp(_)) {
                            continue;
                        }
                        any = true;
                        let mut on = req.mcp_servers.contains(&r.id);
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut on, "").changed() {
                                if on {
                                    req.mcp_servers.push(r.id.clone());
                                } else {
                                    req.mcp_servers.retain(|id| id != &r.id);
                                }
                            }
                            ui.label(
                                RichText::new(icons::PUZZLE_PIECE).font(theme::icon_font(14.0)).color(p.text_muted),
                            );
                            ui.label(RichText::new(&r.name).font(theme::medium(13.0)));
                            ui.label(RichText::new(&c.name).color(p.text_muted));
                        });
                    }
                }
                if !any {
                    kit::empty_state(
                        ui,
                        icons::PUZZLE_PIECE,
                        tr("No MCP servers yet"),
                        tr("No MCP requests saved in this workspace yet."),
                        |_| {},
                    );
                }
            });
        }
    }
    if tab.auto.as_deref() == Some("send") {
        send = !tab.is_running();
        tab.auto = None;
    }
    let req_clone = req.clone();
    let item_now = item.clone();
    tab.slot.store(app, &original, item);
    if fetch_models {
        let r = resolver(app, collection.as_deref());
        let auth = api_key(&req_clone, &r);
        let key = match auth.kind {
            AuthType::Bearer => Some(auth.get_string("token")),
            AuthType::ApiKey => Some(auth.get_string("value")),
            _ => None,
        };
        let base = r(&req_clone.base_url);
        let provider = req_clone.provider;
        match app.http.default_client() {
            Ok(http) => {
                tab.fetching_models = true;
                let tx = app.tx.clone();
                let ctx = app.ctx.clone();
                app.rt.spawn(async move {
                    let res = merpati_ai::list_models(provider, &base, key.as_deref(), &http)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Ai { tab: tab_id, epoch: 0, event: AiMsg::Models(res) });
                    ctx.request_repaint();
                });
            }
            Err(e) => tab.error = Some(e.message),
        }
    }
    if send && !tab.is_running() {
        tab.scripts.clear_results();
        tab.section = Section::Messages;
        if crate::proto_common::run_scripts(
            app,
            tab_id,
            &tab.slot,
            &item_now,
            ScriptEvent::PreRequest,
            proto::ai(&req_clone),
        ) {
            tab.scripts.waiting = true;
        } else {
            start(app, tab_id, tab, req_clone.clone(), collection.as_deref());
        }
    }
    if let Some(pre) = tab.scripts.take_pre() {
        match pre {
            None => tab.error = Some(tr("Skipped by pm.execution.skipRequest()").into()),
            Some(v) => {
                let mut sent = req_clone.clone();
                proto::apply_ai(&mut sent, &v);
                start(app, tab_id, tab, sent, collection.as_deref());
            }
        }
    }
    if let Some(v) = tab.post.take() {
        crate::proto_common::run_scripts(
            app,
            tab_id,
            &tab.slot,
            &item_now,
            ScriptEvent::PostResponse,
            proto::ai(&req_clone).with_response(v),
        );
    }
}

/// The conversation: system prompt, the request's messages as editable
/// bubbles, then the tool calls and the answer of the last run.
fn conversation(ui: &mut Ui, tab: &mut AiTab, req: &mut AiRequest, vars: &VarCtx, send: &mut bool) {
    let p = theme::pal(ui);
    let running = tab.is_running();
    // Toolbar: view switch and the numbers of the last run.
    ui.horizontal(|ui| {
        ui.set_height(28.0);
        kit::segmented(ui, &mut tab.view, &[(View::Chat, tr("Chat")), (View::Raw, tr("Raw response"))]);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(r) = &tab.done {
                kit::metric(ui, icons::TIMER, &merpati_core::util::human_ms(r.latency_ms)).on_hover_ui(|ui| {
                    if let Some(f) = r.first_token_ms {
                        ui.label(trf("first token {}", &[&merpati_core::util::human_ms(f)]));
                    }
                    ui.label(trf("Total: {}", &[&merpati_core::util::human_ms(r.latency_ms)]));
                });
                if let Some(o) = r.output_tokens {
                    kit::pill(ui, &trf("{} out", &[&o]), p.info).on_hover_text(tr("Output tokens"));
                }
                if let Some(i) = r.input_tokens {
                    kit::pill(ui, &trf("{} in", &[&i]), p.text_muted).on_hover_text(tr("Input tokens"));
                }
                if let Some(f) = &r.finish_reason {
                    kit::pill(ui, f, if f == "stop" || f == "end_turn" || f == "STOP" { p.ok } else { p.warn })
                        .on_hover_text(tr("Finish reason"));
                }
                ui.add(egui::Label::new(RichText::new(&r.model).monospace().small().color(p.text_muted)).truncate());
            } else if let Some(s) = tab.started {
                kit::metric(ui, icons::TIMER, &merpati_core::util::human_ms(s.elapsed().as_secs_f64() * 1000.0));
                if let Some((i, o)) = tab.usage {
                    kit::pill(ui, &trf("{} out", &[&o]), p.info);
                    kit::pill(ui, &trf("{} in", &[&i]), p.text_muted);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        });
    });
    ui.add_space(8.0);
    if tab.view == View::Raw {
        let raw = tab.done.as_ref().map(|r| serde_json::to_string_pretty(&r.raw).unwrap_or_default());
        match raw {
            Some(raw) => {
                egui::ScrollArea::vertical().id_salt("ai-raw").auto_shrink([false; 2]).show(ui, |ui| {
                    crate::proto_common::text_view(ui, "ai-raw", &raw);
                });
            }
            None => kit::empty_state(
                ui,
                icons::BRACKETS_CURLY,
                tr("No response yet"),
                tr("The provider's response appears here after a send."),
                |_| {},
            ),
        }
        return;
    }
    egui::ScrollArea::vertical().id_salt("ai-chat").stick_to_bottom(running).auto_shrink([false; 2]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 10.0;
        // System prompt.
        egui::Frame::new()
            .fill(p.chrome)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin { left: 12, right: 12, top: 8, bottom: 8 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::GEAR_SIX).font(theme::icon_font(13.0)).color(p.text_muted));
                    ui.label(RichText::new(tr("System prompt")).font(theme::medium(11.5)).color(p.text_muted));
                });
                let r = bare_editor(ui, "ai-system", &mut req.system, vars, 1);
                if req.system.is_empty() && !r.has_focus() {
                    ui.painter().text(
                        r.rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        tr("Optional instructions for the model"),
                        theme::mono(12.0),
                        p.text_faint,
                    );
                }
            });
        // Messages.
        let mut remove = None;
        let mut flip = None;
        let n = req.messages.len();
        for (i, m) in req.messages.iter_mut().enumerate() {
            let who = if m.role == "assistant" { Who::Assistant } else { Who::User };
            let label = if who == Who::User { tr("User") } else { tr("Assistant") };
            ui.push_id(("ai-msg", i), |ui| {
                bubble(ui, who, label, |ui| {
                    let rows = if i + 1 == n { 2 } else { 1 };
                    let r = bare_editor(ui, ("ai-msg", i), &mut m.content, vars, rows);
                    if m.content.is_empty() && !r.has_focus() {
                        ui.painter().text(
                            r.rect.left_top() + egui::vec2(0.0, 4.0),
                            egui::Align2::LEFT_TOP,
                            tr("Write a message…"),
                            theme::mono(12.0),
                            p.text_faint,
                        );
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        let other = if who == Who::User { tr("Make assistant") } else { tr("Make user") };
                        if kit::icon_button_ex(ui, icons::ARROWS_LEFT_RIGHT, other, false, 22.0).clicked() {
                            flip = Some(i);
                        }
                        if kit::icon_button_ex(ui, icons::TRASH, tr("Remove message"), false, 22.0).clicked() {
                            remove = Some(i);
                        }
                    });
                    ((), r.has_focus())
                });
            });
        }
        if let Some(i) = flip {
            let m = &mut req.messages[i];
            m.role = if m.role == "assistant" { "user".into() } else { "assistant".into() };
        }
        if let Some(i) = remove {
            req.messages.remove(i);
        }
        ui.horizontal(|ui| {
            if kit::ghost(ui, Some(icons::PLUS), tr("User message")).clicked() {
                req.messages.push(AiMessage { role: "user".into(), content: String::new() });
            }
            if kit::ghost(ui, Some(icons::PLUS), tr("Assistant message")).clicked() {
                req.messages.push(AiMessage { role: "assistant".into(), content: String::new() });
            }
            if !running {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(tr("to send")).color(p.text_muted));
                    kit::kbd(ui, "⇧⌘↩");
                });
            }
        });
        // The last run.
        if !tab.steps.is_empty() || running || !tab.text.is_empty() || tab.error.is_some() {
            ui.add_space(2.0);
            let y = ui.cursor().top();
            ui.painter().hline(ui.max_rect().x_range(), y, Stroke::new(1.0, p.border));
            ui.add_space(2.0);
        }
        for (i, (is_call, name, body)) in tab.steps.iter().enumerate() {
            let label = if *is_call { trf("Tool call · {}", &[name]) } else { trf("Tool result · {}", &[name]) };
            ui.push_id(("ai-step", i), |ui| {
                bubble(ui, Who::Tool, &label, |ui| {
                    let short = crate::dialogs::ellipsize(&body.replace('\n', " "), 160);
                    ui.add(egui::Label::new(RichText::new(short).monospace().color(p.text_muted)).wrap());
                    pc::collapsible(ui, ("ai-step-full", i), tr("Details"), None, |ui| {
                        crate::proto_common::text_view(ui, ("ai-step-text", i), body);
                    });
                    ((), false)
                });
            });
        }
        if running || !tab.text.is_empty() {
            let mut add = false;
            bubble(ui, Who::Assistant, tr("Assistant"), |ui| {
                if !tab.text.is_empty() {
                    ui.add(egui::Label::new(RichText::new(&tab.text).size(14.0)).selectable(true).wrap());
                }
                if running {
                    typing_dots(ui);
                } else {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if kit::ghost(ui, Some(icons::ARROW_BEND_UP_LEFT), tr("Add to conversation"))
                            .on_hover_text(tr("Append the answer as an assistant message"))
                            .clicked()
                        {
                            add = true;
                        }
                        if kit::icon_button_ex(ui, icons::COPY, tr("Copy answer"), false, 24.0).clicked() {
                            ui.ctx().copy_text(tab.text.clone());
                        }
                    });
                }
                ((), false)
            });
            if add {
                req.messages.push(AiMessage { role: "assistant".into(), content: tab.text.clone() });
                req.messages.push(AiMessage { role: "user".into(), content: String::new() });
                tab.text.clear();
                tab.steps.clear();
                tab.done = None;
            }
        }
        if let Some(e) = tab.error.clone() {
            let mut retry = false;
            pc::error_banner(ui, tr("The model could not answer"), &e, |ui| {
                retry = kit::secondary(ui, Some(icons::ARROW_CLOCKWISE), tr("Retry")).clicked();
            });
            if retry {
                *send = true;
            }
        }
        if !running && tab.text.is_empty() && tab.error.is_none() && tab.done.is_none() {
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(icons::SPARKLE).font(theme::icon_font(20.0)).color(p.text_faint));
                ui.label(RichText::new(tr("Send the request to see the model's answer.")).color(p.text_muted));
            });
        }
    });
}

pub fn start(app: &mut MerpatiApp, tab_id: u64, tab: &mut AiTab, mut req: AiRequest, collection: Option<&str>) {
    let r = resolver(app, collection);
    req.auth = api_key(&req, &r);
    tab.text.clear();
    tab.steps.clear();
    tab.usage = None;
    tab.done = None;
    tab.error = None;
    let cfg = match AiConfig::from_request(&req, &r) {
        Ok(c) => c,
        Err(e) => {
            tab.error = Some(e.to_string());
            return;
        }
    };
    let http = match app.http.default_client() {
        Ok(c) => c,
        Err(e) => {
            tab.error = Some(e.message);
            return;
        }
    };
    // MCP servers whose tools are offered, with their own variable scopes.
    let mut servers = Vec::new();
    for id in &req.mcp_servers {
        for c in &app.ws.data.collections {
            if let Some(item) = c.find_request(id)
                && let RequestKind::Mcp(m) = &item.request
            {
                let resolve = resolver(app, Some(&c.id));
                let m = crate::mcp_tab::with_inherited_auth(app, Some(&c.id), &item.id, m);
                servers.push((item.name.clone(), McpConfig::from_request(&m, &resolve)));
            }
        }
    }
    let cancel = CancellationToken::new();
    tab.cancel = Some(cancel.clone());
    tab.started = Some(Instant::now());
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    tab.epoch = crate::app::next_epoch();
    let epoch = tab.epoch;
    app.rt.spawn(async move {
        let emit = {
            let tx = tx.clone();
            let ctx = ctx.clone();
            move |e: AiEvent| {
                let _ = tx.send(Msg::Ai { tab: tab_id, epoch, event: AiMsg::Event(e) });
                ctx.request_repaint();
            }
        };
        let mut clients = Vec::new();
        let mut tools = Vec::new();
        for (name, cfg) in servers {
            match McpClient::connect_with_http(cfg, http.clone(), |_| {}).await {
                Ok(client) => {
                    match client.list_tools().await {
                        Ok(list) => tools.extend(list.into_iter().map(|t| (client.clone(), t))),
                        Err(e) => {
                            emit(AiEvent::ToolResult { name: format!("{name} (tools/list)"), result: e.to_string() })
                        }
                    }
                    clients.push(client);
                }
                Err(e) => emit(AiEvent::ToolResult { name: format!("{name} (connect)"), result: e.to_string() }),
            }
        }
        let _ = merpati_ai::run(cfg, &http, tools, emit, cancel).await;
        for c in clients {
            c.close().await;
        }
    });
}
