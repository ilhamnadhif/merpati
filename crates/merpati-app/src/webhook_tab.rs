//! Local webhook listener (FR-X10): records incoming requests and can replay
//! them to another URL. A public URL needs a tunnel set up by the user.

use egui::{RichText, Ui};

use merpati_mock::listener::{ListenerConfig, ReplayResult, WebhookListener};

use crate::app::{MerpatiApp, Msg};
use crate::i18n::{tr, trf};
use crate::proto_common::{self as pc, Conn, ReqRow};
use crate::{icons, kit, theme};

pub enum WebhookMsg {
    Started(Result<WebhookListener, String>),
    Replayed(Result<ReplayResult, String>),
}

pub struct WebhookTab {
    listener: Option<WebhookListener>,
    starting: bool,
    port: u16,
    status: u16,
    body: String,
    selected: Option<u64>,
    replay_target: String,
    replay: Option<Result<ReplayResult, String>>,
    replaying: bool,
    /// Why the listener could not start.
    start_error: Option<String>,
    /// Automation: `start` or `select` (the latest request).
    pub auto: Option<String>,
}

impl Default for WebhookTab {
    fn default() -> Self {
        WebhookTab {
            listener: None,
            starting: false,
            port: 0,
            status: 200,
            body: "{\"ok\":true}".into(),
            selected: None,
            replay_target: "http://localhost:3000".into(),
            replay: None,
            replaying: false,
            start_error: None,
            auto: None,
        }
    }
}

impl WebhookTab {
    pub fn on_msg(&mut self, m: WebhookMsg) {
        match m {
            WebhookMsg::Started(r) => {
                self.starting = false;
                match r {
                    Ok(l) => self.listener = Some(l),
                    Err(e) => self.start_error = Some(e),
                }
            }
            WebhookMsg::Replayed(r) => {
                self.replaying = false;
                self.replay = Some(r)
            }
        }
    }

    pub fn set_port(&mut self, port: u16) {
        self.port = port;
    }

    pub fn is_running(&self) -> bool {
        self.listener.is_some()
    }

    pub fn stop(&mut self, rt: &tokio::runtime::Runtime) {
        if let Some(l) = self.listener.take() {
            rt.spawn(l.stop());
        }
    }

    fn state(&self) -> (Conn, &'static str) {
        if self.listener.is_some() {
            (Conn::Live, tr("Listening"))
        } else if self.starting {
            (Conn::Busy, tr("Starting…"))
        } else if self.start_error.is_some() {
            (Conn::Failed, tr("Not started"))
        } else {
            (Conn::Idle, tr("Stopped"))
        }
    }
}

fn start(app: &mut MerpatiApp, tab_id: u64, tab: &mut WebhookTab) {
    tab.starting = true;
    tab.start_error = None;
    let cfg = ListenerConfig {
        port: tab.port,
        response_status: tab.status,
        response_body: tab.body.clone(),
        ..Default::default()
    };
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    app.rt.spawn(async move {
        let r = WebhookListener::start(cfg).await.map_err(|e| e.to_string());
        let _ = tx.send(Msg::Webhook { tab: tab_id, event: WebhookMsg::Started(r) });
        ctx.request_repaint();
    });
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut WebhookTab) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let auto = tab.auto.take();
    if auto.as_deref() == Some("select")
        && let Some(l) = &tab.listener
    {
        tab.selected = l.events().last().map(|e| e.id);
    }
    let (state, state_text) = tab.state();
    let mut do_start = auto.as_deref() == Some("start") && tab.listener.is_none() && !tab.starting;
    let mut do_stop = false;
    pc::view_title(
        ui,
        icons::WEBHOOKS_LOGO,
        tr("Webhook listener"),
        Some(tr(
            "Records every request sent to the local URL. For a public URL, point a tunnel (ngrok, cloudflared, ...) at it.",
        )),
        |ui| {
            pc::status_pill(ui, state, state_text);
        },
        |ui| match state {
            Conn::Live => do_stop = kit::secondary(ui, Some(icons::STOP), tr("Stop")).clicked(),
            Conn::Busy => {
                ui.add(egui::Spinner::new().size(18.0).color(p.accent));
            }
            _ => do_start |= kit::primary_icon(ui, icons::PLAY, tr("Start")).clicked(),
        },
    );
    ui.add_space(12.0);
    // Settings.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_enabled_ui(tab.listener.is_none(), |ui| {
            ui.label(RichText::new(tr("Port")).color(p.text_muted));
            ui.add(egui::DragValue::new(&mut tab.port).range(0..=65535)).on_hover_text(tr("0 picks a free port"));
        });
        kit::vdivider(ui, 20.0);
        ui.label(RichText::new(tr("Respond with")).color(p.text_muted));
        ui.add(egui::DragValue::new(&mut tab.status).range(100..=599)).on_hover_text(tr("Status code"));
        let apply_w = if tab.listener.is_some() { 80.0 } else { 0.0 };
        ui.add(
            egui::TextEdit::singleline(&mut tab.body)
                .desired_width((ui.available_width() - apply_w).max(120.0))
                .hint_text(tr("Response body"))
                .font(egui::TextStyle::Monospace),
        );
        if let Some(l) = &tab.listener
            && kit::ghost(ui, Some(icons::CHECK), tr("Apply"))
                .on_hover_text(tr("Use this response from now on"))
                .clicked()
        {
            l.set_response(tab.status, vec![("Content-Type".into(), "application/json".into())], tab.body.clone());
        }
    });
    let events = tab.listener.as_ref().map(|l| l.events()).unwrap_or_default();
    if let Some(l) = &tab.listener {
        let url = l.url();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if pc::address_pill(ui, &url) {
                app.toast(tr("Copied"), false);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let clear = ui.add_enabled_ui(!events.is_empty(), |ui| kit::ghost(ui, Some(icons::TRASH), tr("Clear")));
                if clear.inner.clicked() {
                    l.clear();
                    tab.selected = None;
                }
                ui.label(RichText::new(trf("{} requests", &[&events.len()])).color(p.text_muted));
            });
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
    if let Some(e) = tab.start_error.clone() {
        ui.add_space(8.0);
        let mut retry = false;
        pc::error_banner(ui, tr("Could not start the listener"), &e, |ui| {
            retry = kit::secondary(ui, Some(icons::ARROW_CLOCKWISE), tr("Retry")).clicked();
        });
        do_start |= retry;
    }
    ui.add_space(10.0);
    let sep_y = ui.cursor().top();
    ui.painter().hline(ui.max_rect().x_range(), sep_y, egui::Stroke::new(1.0, p.border));
    ui.add_space(8.0);
    if events.is_empty() {
        match &tab.listener {
            Some(l) => {
                let url = l.url();
                let curl = format!(
                    "curl -X POST {url}/hook -H 'Content-Type: application/json' -d '{{\"hello\":\"merpati\"}}'"
                );
                kit::empty_state(
                    ui,
                    icons::HOURGLASS,
                    tr("Waiting for requests"),
                    tr("Requests sent to the address above appear here. Try:"),
                    |ui| {
                        let w = ui.available_width().min(560.0);
                        egui::Frame::new()
                            .fill(p.subtle)
                            .corner_radius(egui::CornerRadius::same(6))
                            .inner_margin(egui::Margin { left: 10, right: 2, top: 2, bottom: 2 })
                            .show(ui, |ui| {
                                ui.set_width(w - 12.0);
                                ui.horizontal(|ui| {
                                    ui.add(egui::Label::new(RichText::new(&curl).monospace().size(11.5)).truncate())
                                        .on_hover_text(&curl);
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if kit::icon_button_ex(ui, icons::COPY, tr("Copy command"), false, 24.0)
                                            .clicked()
                                        {
                                            ui.ctx().copy_text(curl.clone());
                                        }
                                    });
                                });
                            });
                    },
                );
            }
            None => kit::empty_state(
                ui,
                icons::WEBHOOKS_LOGO,
                tr("The listener is stopped"),
                tr("Start it to get a local URL that records every request sent to it."),
                |_| {},
            ),
        }
    } else {
        let list_w = (ui.available_width() * 0.42).clamp(280.0, 520.0);
        pc::list_panel(ui, ("wh-list", tab_id), list_w, |ui| {
            pc::request_header(ui, false, false, tr("Path"), tr("Size"));
            egui::ScrollArea::vertical().id_salt("wh-events").stick_to_bottom(true).auto_shrink([false; 2]).show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for e in &events {
                        let time = pc::clock_s(e.timestamp_ms);
                        let query = if e.query.is_empty() { String::new() } else { format!("?{}", e.query) };
                        let row = ReqRow {
                            time: &time,
                            method: &e.method,
                            status: None,
                            path: &e.path,
                            detail: &query,
                            right: merpati_core::util::human_bytes(e.body.len() as u64),
                        };
                        if pc::request_row(ui, &row, tab.selected == Some(e.id), None).clicked() {
                            tab.selected = Some(e.id);
                            tab.replay = None;
                        }
                    }
                },
            );
        });
        let Some(e) = tab.selected.and_then(|id| events.iter().find(|e| e.id == id)) else {
            kit::empty_state(
                ui,
                icons::ARROW_LEFT,
                tr("Select a request"),
                tr("Its headers and body appear here."),
                |_| {},
            );
            return run(app, tab_id, tab, do_start, do_stop);
        };
        let mut replay_now = false;
        egui::ScrollArea::vertical().id_salt("wh-detail").auto_shrink([false; 2]).show(ui, |ui| {
            ui.horizontal(|ui| {
                pc::method_pill(ui, &e.method);
                let full = if e.query.is_empty() { e.path.clone() } else { format!("{}?{}", e.path, e.query) };
                ui.add(egui::Label::new(RichText::new(&full).monospace().strong()).truncate());
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new(pc::clock_s(e.timestamp_ms)).color(p.text_muted).small());
                if let Some(a) = &e.remote_addr {
                    ui.label(RichText::new(trf("from {}", &[&a])).color(p.text_muted).small());
                }
                ui.label(
                    RichText::new(merpati_core::util::human_bytes(e.body.len() as u64)).color(p.text_muted).small(),
                );
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(kit::section_title(tr("Headers")));
                pc::count_badge(ui, e.headers.len());
            });
            ui.add_space(4.0);
            pc::kv_view(
                ui,
                ("wh-headers", e.id),
                e.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                tr("No headers"),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(kit::section_title(tr("Body")));
                if e.body_truncated {
                    kit::pill(ui, tr("truncated"), p.warn);
                }
            });
            ui.add_space(4.0);
            if e.body.is_empty() {
                ui.label(RichText::new(tr("No body")).color(p.text_muted));
            } else {
                let body = crate::proto_common::payload_text(&e.body);
                crate::proto_common::text_view(ui, ("wh-body-text", e.id), &body);
            }
            ui.add_space(14.0);
            kit::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                pc::section(ui, tr("Replay"), Some(tr("Send this request again to another URL, e.g. your local app.")));
                ui.horizontal(|ui| {
                    let busy = tab.replaying;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if busy {
                            ui.add(egui::Spinner::new().size(16.0).color(p.accent));
                        } else if kit::secondary(ui, Some(icons::ARROW_CLOCKWISE), tr("Replay")).clicked() {
                            replay_now = true;
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut tab.replay_target)
                                .desired_width(ui.available_width())
                                .hint_text("http://localhost:3000")
                                .font(egui::TextStyle::Monospace),
                        );
                    });
                });
                match &tab.replay {
                    Some(Ok(r)) => {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            kit::status_pill(ui, r.status, "");
                            kit::metric(ui, icons::TIMER, &merpati_core::util::human_ms(r.elapsed_ms));
                            kit::metric(
                                ui,
                                icons::DOWNLOAD_SIMPLE,
                                &merpati_core::util::human_bytes(r.body.len() as u64),
                            );
                        });
                        if !r.body.is_empty() {
                            crate::proto_common::text_view(
                                ui,
                                "wh-replay",
                                &crate::proto_common::payload_text(&r.body),
                            );
                        }
                    }
                    Some(Err(err)) => {
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(icons::WARNING_CIRCLE)
                                    .font(theme::icon_font(14.0))
                                    .color(theme::err(dark)),
                            );
                            ui.label(RichText::new(err).color(theme::err(dark)));
                        });
                    }
                    None => {}
                }
            });
        });
        if replay_now {
            tab.replaying = true;
            tab.replay = None;
            let event = e.clone();
            let target = tab.replay_target.clone();
            let http = app.http.default_client();
            let tx = app.tx.clone();
            let ctx = app.ctx.clone();
            app.rt.spawn(async move {
                let r = match http {
                    Ok(client) => merpati_mock::listener::replay(&event, &target, &client).await,
                    Err(e) => Err(e.message),
                };
                let _ = tx.send(Msg::Webhook { tab: tab_id, event: WebhookMsg::Replayed(r) });
                ctx.request_repaint();
            });
        }
    }
    run(app, tab_id, tab, do_start, do_stop);
}

/// Starts or stops the listener as asked this frame.
fn run(app: &mut MerpatiApp, tab_id: u64, tab: &mut WebhookTab, start_now: bool, stop_now: bool) {
    if stop_now {
        tab.stop(&app.rt);
    } else if start_now && tab.listener.is_none() && !tab.starting {
        start(app, tab_id, tab);
    }
}
