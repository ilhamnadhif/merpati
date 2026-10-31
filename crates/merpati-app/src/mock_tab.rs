//! Local mock server tab (FR-M1..M4).

use egui::{RichText, Ui};

use merpati_core::model::Id;
use merpati_mock::{FailureKind, FailureSimulation, MockConfig, MockLogEntry, MockServer, NetworkConditions};

use crate::app::{MerpatiApp, Msg};
use crate::i18n::{tr, trf};
use crate::proto_common::{self as pc, Conn, ReqRow};
use crate::{icons, kit, theme, widgets};

#[derive(Clone, Copy, PartialEq, Eq)]
enum FailKind {
    Status,
    Drop,
    Timeout,
}

pub struct MockTab {
    pub collection: Id,
    pub server: Option<MockServer>,
    pub starting: bool,
    port: u16,
    delay_ms: u64,
    fail_rate: f32,
    fail_status: u16,
    fail_kind: FailKind,
    network: NetworkConditions,
    use_env: bool,
    /// The selected log entry and where it was last seen: once the server's
    /// log is full it drops the oldest entries, so an index alone drifts.
    selected_log: Option<(usize, MockLogEntry)>,
}

impl MockTab {
    pub fn new(collection: Id) -> Self {
        MockTab {
            collection,
            server: None,
            starting: false,
            port: 3000,
            delay_ms: 0,
            fail_rate: 0.0,
            fail_status: 500,
            fail_kind: FailKind::Status,
            network: NetworkConditions::default(),
            use_env: true,
            selected_log: None,
        }
    }

    pub fn started(&mut self, server: MockServer) {
        self.starting = false;
        self.server = Some(server);
    }

    pub fn stop(&self, _rt: &tokio::runtime::Runtime) {
        // `MockServer` shuts down when dropped; `show` drops it on Stop.
    }
}

fn start(app: &mut MerpatiApp, tab_id: u64, tab: &mut MockTab, collection: merpati_core::model::Collection) {
    tab.starting = true;
    let mut vars = app.ws.scopes_for(Some(&tab.collection));
    if !tab.use_env {
        vars.environment.clear();
    }
    let config = MockConfig {
        port: tab.port,
        delay_ms: tab.delay_ms,
        variables: vars,
        network: tab.network,
        failure: (tab.fail_rate > 0.0).then_some(FailureSimulation {
            rate: tab.fail_rate,
            kind: match tab.fail_kind {
                FailKind::Status => FailureKind::Status(tab.fail_status),
                FailKind::Drop => FailureKind::Drop,
                FailKind::Timeout => FailureKind::Timeout,
            },
        }),
        ..Default::default()
    };
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    app.rt.spawn(async move {
        let result = MockServer::start(collection, config).await.map_err(|e| e.to_string());
        let _ = tx.send(Msg::Mock { tab: tab_id, result });
        ctx.request_repaint();
    });
}

/// Points `{{baseUrl}}` (of the active environment, else the collection) at `url`.
fn point_base_url(app: &mut MerpatiApp, collection: &Id, url: &str) {
    match app.ws.data.active_environment.clone() {
        Some(env_id) => {
            if let Some(env) = app.ws.data.environments.iter_mut().find(|e| e.id == env_id) {
                env.set_current("baseUrl", url);
            }
            app.ws.mark_env(&env_id);
        }
        None => {
            if let Some(c) = app.ws.data.collection_mut(collection) {
                match c.variables.iter_mut().find(|v| v.key == "baseUrl") {
                    Some(v) => v.current = Some(url.to_string()),
                    None => {
                        let mut v = merpati_core::variables::Variable::new("baseUrl", "");
                        v.current = Some(url.to_string());
                        c.variables.push(v);
                    }
                }
            }
            let cid = collection.clone();
            app.ws.mark_collection(&cid);
        }
    }
    app.toast(tr("baseUrl now points at the mock server"), false);
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut MockTab) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let Some(collection) = app.ws.data.collection(&tab.collection).cloned() else {
        kit::empty_state(
            ui,
            icons::CUBE,
            tr("This collection was deleted."),
            tr("Close this tab, or open a mock server from another collection."),
            |_| {},
        );
        return;
    };
    // Keep the running server in sync with edits to the collection.
    if let Some(s) = &tab.server {
        s.set_collection(collection.clone());
    }
    let (state, state_text) = if tab.server.is_some() {
        (Conn::Live, tr("Running"))
    } else if tab.starting {
        (Conn::Busy, tr("Starting…"))
    } else {
        (Conn::Idle, tr("Stopped"))
    };
    let mut do_start = false;
    let mut do_stop = false;
    let subtitle = trf("{} · examples saved on requests are served by method and path", &[&collection.name]);
    pc::view_title(
        ui,
        icons::CUBE,
        tr("Mock server"),
        Some(&subtitle),
        |ui| {
            pc::status_pill(ui, state, state_text);
        },
        |ui| match state {
            Conn::Live => do_stop = kit::secondary(ui, Some(icons::STOP), tr("Stop")).clicked(),
            Conn::Busy => {
                ui.add(egui::Spinner::new().size(18.0).color(p.accent));
            }
            _ => do_start = kit::primary_icon(ui, icons::PLAY, tr("Start")).clicked(),
        },
    );
    if let Some(server) = &tab.server {
        let url = server.url();
        ui.add_space(10.0);
        let mut point = false;
        ui.horizontal(|ui| {
            if pc::address_pill(ui, &url) {
                app.toast(tr("Copied"), false);
            }
            let env_name = app.ws.data.active_environment().map(|e| e.name.clone());
            let hint = match &env_name {
                Some(n) => trf("Set {{baseUrl}} in \"{}\" to this URL", &[n]),
                None => tr("Set collection {{baseUrl}} to this URL").to_string(),
            };
            point = kit::secondary(ui, Some(icons::CROSSHAIR), tr("Use as {{baseUrl}}")).on_hover_text(hint).clicked();
        });
        if point {
            point_base_url(app, &tab.collection, &url);
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
    ui.add_space(12.0);
    // Settings.
    let running = tab.server.is_some();
    kit::card(ui).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add_enabled_ui(!running, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            let muted = |ui: &mut Ui, t: &str| {
                ui.label(RichText::new(t).color(p.text_muted));
            };
            pc::flow_row(
                ui,
                tr("Server"),
                90.0,
                vec![
                    Box::new(|ui: &mut Ui| {
                        muted(ui, tr("Port"));
                        widgets::a11y_name(&ui.add(egui::DragValue::new(&mut tab.port).range(0..=65535)), tr("Port"));
                    }),
                    Box::new(|ui: &mut Ui| {
                        muted(ui, tr("Delay (ms)"));
                        widgets::a11y_name(
                            &ui.add(egui::DragValue::new(&mut tab.delay_ms).range(0..=60_000)),
                            tr("Delay (ms)"),
                        );
                    }),
                    Box::new(|ui: &mut Ui| {
                        ui.checkbox(&mut tab.use_env, tr("Resolve variables with the active environment"));
                    }),
                ],
            );
            let net_now = tab.network;
            let mut preset_pick = None;
            pc::flow_row(
                ui,
                tr("Network"),
                90.0,
                vec![
                    Box::new(|ui: &mut Ui| {
                        let preset =
                            NetworkConditions::PRESETS.iter().find(|(_, n)| *n == net_now).map(|(name, _)| *name);
                        let current = match (preset, net_now == NetworkConditions::default()) {
                            (_, true) => tr("No throttling").to_string(),
                            (Some(p), _) => p.to_string(),
                            (None, _) => tr("Custom").to_string(),
                        };
                        let r = egui::ComboBox::from_id_salt(("mock-net", tab_id))
                            .icon(crate::kit::combo_caret)
                            .width(130.0)
                            .selected_text(current)
                            .show_ui(ui, |ui| {
                                let none = NetworkConditions::default();
                                if ui.selectable_label(net_now == none, tr("No throttling")).clicked() {
                                    preset_pick = Some(none);
                                }
                                for (name, n) in NetworkConditions::PRESETS {
                                    if ui.selectable_label(net_now == n, name).clicked() {
                                        preset_pick = Some(n);
                                    }
                                }
                            });
                        widgets::a11y_name(&r.response, tr("Network"));
                    }),
                    Box::new(|ui: &mut Ui| {
                        muted(ui, tr("Latency (ms)"));
                        widgets::a11y_name(
                            &ui.add(egui::DragValue::new(&mut tab.network.latency_ms).range(0..=60_000)),
                            tr("Latency (ms)"),
                        );
                    }),
                    Box::new(|ui: &mut Ui| {
                        muted(ui, tr("Jitter (ms)"));
                        widgets::a11y_name(
                            &ui.add(egui::DragValue::new(&mut tab.network.jitter_ms).range(0..=10_000)),
                            tr("Jitter (ms)"),
                        );
                    }),
                    Box::new(|ui: &mut Ui| {
                        let bps = &mut tab.network.bandwidth_bytes_per_sec;
                        let mut limited = bps.is_some();
                        if ui.checkbox(&mut limited, tr("Bandwidth (KB/s)")).changed() {
                            *bps = limited.then_some(100_000);
                        }
                        if let Some(bps) = bps {
                            let mut kb = *bps / 1000;
                            if ui.add(egui::DragValue::new(&mut kb).range(1..=1_000_000)).changed() {
                                *bps = kb * 1000;
                            }
                        }
                    }),
                ],
            );
            if let Some(n) = preset_pick {
                tab.network = n;
            }
            pc::flow_row(
                ui,
                tr("Failures"),
                90.0,
                vec![
                    Box::new(|ui: &mut Ui| {
                        let mut pct = (tab.fail_rate * 100.0).round() as u32;
                        let r = ui.add(egui::DragValue::new(&mut pct).range(0..=100).suffix(" %"));
                        widgets::a11y_name(&r, tr("Failures"));
                        if r.on_hover_text(tr("Share of requests that fail")).changed() {
                            tab.fail_rate = pct as f32 / 100.0;
                        }
                    }),
                    Box::new(|ui: &mut Ui| {
                        ui.push_id("fail-kind", |ui| {
                            kit::segmented(
                                ui,
                                &mut tab.fail_kind,
                                &[
                                    (FailKind::Status, tr("Error status")),
                                    (FailKind::Drop, tr("Drop connection")),
                                    (FailKind::Timeout, tr("Timeout")),
                                ],
                            )
                        });
                        if tab.fail_kind == FailKind::Status {
                            ui.add(egui::DragValue::new(&mut tab.fail_status).range(100..=599))
                                .on_hover_text(tr("Status code"));
                        }
                    }),
                ],
            );
        });
        if running {
            ui.label(kit::hint(ui, tr("Stop the server to change these settings.")));
        }
    });
    ui.add_space(12.0);
    let routes = match &tab.server {
        Some(s) => s.routes(),
        None => merpati_mock::routes(&collection),
    };
    let logs = tab.server.as_ref().map(|s| s.logs()).unwrap_or_default();
    let selected = locate(&logs, &mut tab.selected_log);
    let mut clear_logs = false;
    ui.columns(2, |cols| {
        // Routes.
        let ui = &mut cols[0];
        ui.horizontal(|ui| {
            ui.set_height(26.0);
            ui.label(kit::section_title(tr("Routes")));
            if !routes.is_empty() {
                pc::count_badge(ui, routes.len());
            }
        });
        ui.add_space(4.0);
        if routes.is_empty() {
            kit::empty_state(
                ui,
                icons::BOOKMARK_SIMPLE,
                tr("No examples yet"),
                tr("Send a request and use “Save as example” on the response."),
                |_| {},
            );
        } else {
            egui::ScrollArea::vertical().id_salt("mock-routes").auto_shrink([false; 2]).show(ui, |ui| {
                for (i, r) in routes.iter().enumerate() {
                    ui.push_id(i, |ui| {
                        ui.horizontal(|ui| {
                            ui.set_height(28.0);
                            ui.allocate_ui_with_layout(
                                egui::vec2(58.0, 22.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| pc::method_pill(ui, &r.method),
                            );
                            ui.add(egui::Label::new(RichText::new(&r.path).monospace()).truncate())
                                .on_hover_text(&r.request_name);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                kit::pill(ui, &trf("{} examples", &[&r.examples.len()]), p.text_muted)
                                    .on_hover_text(r.examples.join(", "));
                                ui.add(egui::Label::new(RichText::new(&r.request_name).color(p.text_muted)).truncate());
                            });
                        });
                    });
                }
            });
        }
        // Logs.
        let ui = &mut cols[1];
        ui.horizontal(|ui| {
            ui.set_height(26.0);
            ui.label(kit::section_title(tr("Logs")));
            if !logs.is_empty() {
                pc::count_badge(ui, logs.len());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let clear = ui.add_enabled_ui(!logs.is_empty(), |ui| kit::ghost(ui, Some(icons::TRASH), tr("Clear")));
                clear_logs = clear.inner.clicked();
            });
        });
        ui.add_space(4.0);
        if logs.is_empty() {
            let body = if tab.server.is_some() {
                tr("Requests to the mock server appear here.")
            } else {
                tr("Start the server, then send requests to it.")
            };
            kit::empty_state(ui, icons::LIST, tr("No requests yet"), body, |_| {});
            return;
        }
        pc::request_header(ui, false, true, tr("Path"), tr("Time"));
        let list_h = if selected.is_some() { ui.available_height() * 0.45 } else { ui.available_height() };
        egui::ScrollArea::vertical()
            .id_salt("mock-logs")
            .stick_to_bottom(true)
            .max_height(list_h)
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (i, l) in logs.iter().enumerate() {
                    let time = pc::clock_s(l.timestamp_ms);
                    let status = if l.status == 0 { tr("dropped").to_string() } else { l.status.to_string() };
                    let query = if l.query.is_empty() { String::new() } else { format!("?{}", l.query) };
                    let row = ReqRow {
                        time: &time,
                        method: &l.method,
                        status: Some((status, theme::status_color(color_status(l.status), dark))),
                        path: &l.path,
                        detail: &query,
                        right: merpati_core::util::human_ms(l.duration_ms),
                    };
                    let r = pc::request_row(ui, &row, selected == Some(i), None);
                    let r = r.on_hover_text(l.matched.clone().unwrap_or_else(|| tr("no match").into()));
                    if r.clicked() {
                        tab.selected_log = if selected == Some(i) { None } else { Some((i, l.clone())) };
                    }
                }
            });
        if let Some(l) = selected.and_then(|i| logs.get(i)) {
            ui.add_space(8.0);
            egui::ScrollArea::vertical().id_salt("mock-log-detail").auto_shrink([false; 2]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(kit::section_title(tr("Matched")));
                    match &l.matched {
                        Some(m) => ui.label(RichText::new(m).color(p.text)),
                        None => ui.label(RichText::new(tr("no match")).color(p.warn)),
                    };
                });
                ui.add_space(6.0);
                pc::kv_view(
                    ui,
                    "mock-log-headers",
                    l.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                    tr("No headers"),
                );
                if !l.body.is_empty() {
                    ui.add_space(8.0);
                    crate::proto_common::text_view(ui, "mock-log-body", &l.body);
                }
            });
        }
    });
    if clear_logs && let Some(s) = &tab.server {
        s.clear_logs();
        tab.selected_log = None;
    }
    if do_stop && let Some(s) = tab.server.take() {
        app.rt.spawn(s.stop());
    }
    if do_start && tab.server.is_none() && !tab.starting {
        start(app, tab_id, tab, collection);
    }
}

/// Where the selected entry is now. Entries only move towards the front (the
/// oldest are dropped); the selection is cleared once its entry is gone.
fn locate(logs: &[MockLogEntry], selected: &mut Option<(usize, MockLogEntry)>) -> Option<usize> {
    let (hint, entry) = selected.as_mut()?;
    match (0..=(*hint).min(logs.len().saturating_sub(1))).rev().find(|&i| logs.get(i) == Some(&*entry)) {
        Some(i) => {
            *hint = i;
            Some(i)
        }
        None => {
            *selected = None;
            None
        }
    }
}

/// Status used to colour a log row (0 = dropped, shown like a server error).
fn color_status(status: u16) -> u16 {
    if status == 0 { 500 } else { status }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: i64) -> MockLogEntry {
        MockLogEntry {
            timestamp_ms: n,
            method: "GET".into(),
            path: format!("/{n}"),
            query: String::new(),
            headers: Vec::new(),
            body: String::new(),
            matched: None,
            status: 200,
            duration_ms: 1.0,
        }
    }

    /// The selected request stays selected while a full log drops its
    /// oldest entries (the index used to point at a newer request), and the
    /// selection goes once its entry is dropped too.
    #[test]
    fn selection_follows_its_entry_in_a_full_log() {
        let window = |from: i64| (from..from + 500).map(entry).collect::<Vec<_>>();
        let mut selected = Some((5, entry(5)));
        assert_eq!(locate(&window(0), &mut selected), Some(5));
        assert_eq!(locate(&window(3), &mut selected), Some(2));
        assert_eq!(selected.as_ref().map(|(_, e)| e.path.as_str()), Some("/5"));
        assert_eq!(locate(&window(5), &mut selected), Some(0));
        assert_eq!(locate(&window(6), &mut selected), None);
        assert!(selected.is_none());
        let mut selected = Some((0, entry(0)));
        assert_eq!(locate(&[], &mut selected), None);
    }

    #[test]
    fn log_rows_are_coloured_by_their_status() {
        assert_eq!(color_status(0), 500);
        assert_eq!(color_status(200), 200);
        assert_eq!(color_status(404), 404);
    }
}
