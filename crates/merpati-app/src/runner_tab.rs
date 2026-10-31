//! Collection Runner tab (FR-U1..U7): functional runs with data files and
//! custom order, and performance runs with virtual users.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use merpati_core::model::Id;
use merpati_http::CancellationToken;
use merpati_runner::data::DataRow;
use merpati_runner::perf::{PerfOptions, PerfReport, PerfSample};
use merpati_runner::{RequestResult, RunEvent, RunOptions, RunSummary};

use crate::app::{MerpatiApp, Msg};
use crate::i18n::{tr, trf};
use crate::request_tab::{banner, elide, fmt_ms, stat_cards};
use crate::state::WsState;
use crate::{icons, kit, theme, widgets};

/// Results kept in memory for display; everything is also spooled to disk.
const KEEP_RESULTS: usize = 2000;

pub enum PerfMsg {
    Sample(PerfSample),
    Done(Box<PerfReport>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Functional,
    Performance,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Passed,
    Failed,
}

pub struct RunnerTab {
    pub collection: Id,
    mode: Mode,
    /// (request id, name, selected) in run order.
    plan: Vec<(Id, String, String, bool)>,
    iterations: usize,
    delay_ms: u64,
    data_path: Option<PathBuf>,
    data: Vec<DataRow>,
    data_error: Option<String>,
    persist_vars: bool,
    save_cookies: bool,
    bail: bool,
    fail_rate: f32,
    cancel: Option<CancellationToken>,
    started: Option<Instant>,
    expected: usize,
    results: std::collections::VecDeque<RequestResult>,
    dropped: usize,
    summary: Option<RunSummary>,
    live: RunSummary,
    spool: Option<tempfile::NamedTempFile>,
    filter: Filter,
    selected: Option<usize>,
    /// Run order row being dragged by its handle.
    drag: Option<usize>,
    /// Width of the setup column.
    panel_w: f32,
    /// Narrow windows show the setup or the results, not both.
    show_results: bool,
    // Performance mode.
    vus: usize,
    duration_s: u64,
    ramp_s: u64,
    perf_samples: Vec<PerfSample>,
    perf_report: Option<Box<PerfReport>>,
}

impl RunnerTab {
    pub fn new(ws: &WsState, collection: Id, folder: Option<Id>) -> Self {
        let mut plan = Vec::new();
        if let Some(c) = ws.data.collection(&collection) {
            let opts = RunOptions { folder: folder.clone(), ..Default::default() };
            if let Ok(list) = merpati_runner::runner::plan(c, &opts) {
                // Every protocol runs: HTTP, gRPC, AI, MCP, and WebSocket,
                // Socket.IO and MQTT as sessions with a stop condition.
                for (r, _) in list {
                    plan.push((r.id.clone(), r.name.clone(), r.request.badge(), true));
                }
            }
        }
        RunnerTab {
            collection,
            mode: Mode::Functional,
            plan,
            iterations: 1,
            delay_ms: 0,
            data_path: None,
            data: Vec::new(),
            data_error: None,
            persist_vars: true,
            save_cookies: true,
            bail: false,
            fail_rate: 0.0,
            cancel: None,
            started: None,
            expected: 0,
            results: Default::default(),
            dropped: 0,
            summary: None,
            live: RunSummary::default(),
            spool: None,
            filter: Filter::All,
            selected: None,
            drag: None,
            panel_w: 340.0,
            show_results: false,
            vus: 10,
            duration_s: 30,
            ramp_s: 0,
            perf_samples: Vec::new(),
            perf_report: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.cancel.is_some()
    }

    pub fn cancel_all(&self) {
        if let Some(c) = &self.cancel {
            c.cancel();
        }
    }

    pub fn on_event(&mut self, e: RunEvent) {
        match e {
            RunEvent::RequestFinished(r) => {
                if let Some(spool) = &mut self.spool
                    && let Ok(line) = serde_json::to_string(&*r)
                {
                    let _ = writeln!(spool, "{line}");
                }
                self.live.executed += 1;
                if r.error.is_some() {
                    self.live.requests_failed += 1;
                }
                for t in &r.tests {
                    if t.skipped {
                        self.live.tests_skipped += 1;
                    } else if t.passed {
                        self.live.tests_passed += 1;
                    } else {
                        self.live.tests_failed += 1;
                    }
                }
                self.live.script_errors += r.script_errors.len();
                self.results.push_back(*r);
                if self.results.len() > KEEP_RESULTS {
                    self.results.pop_front();
                    self.dropped += 1;
                    self.selected = self.selected.and_then(|s| s.checked_sub(1));
                }
            }
            RunEvent::Finished(s) => self.summary = Some(s),
            _ => {}
        }
    }

    pub fn on_perf(&mut self, m: PerfMsg) {
        match m {
            PerfMsg::Sample(s) => self.perf_samples.push(s),
            PerfMsg::Done(r) => {
                self.perf_report = Some(r);
                self.cancel = None;
            }
        }
    }

    pub fn finished(&mut self) {
        self.cancel = None;
    }

    fn reset(&mut self) {
        self.results.clear();
        self.dropped = 0;
        self.summary = None;
        self.live = RunSummary::default();
        self.selected = None;
        self.perf_samples.clear();
        self.perf_report = None;
        self.spool = tempfile::NamedTempFile::new().ok();
    }
}

fn start(app: &mut MerpatiApp, tab_id: u64, tab: &mut RunnerTab) {
    let Some(c) = app.ws.data.collection(&tab.collection).cloned() else { return };
    let order: Vec<String> = tab.plan.iter().filter(|p| p.3).map(|p| p.0.clone()).collect();
    if order.is_empty() {
        app.toast(tr("Select at least one request to run"), true);
        return;
    }
    tab.reset();
    let before = app.ws.scopes_for(Some(&tab.collection));
    let env = app.ws.data.active_environment.clone();
    let mut services = app.services();
    if !tab.save_cookies {
        // Run against a copy of the jar so cookies set during the run are discarded.
        let jar = std::sync::Arc::new(merpati_http::CookieJar::new());
        for ck in app.ws.cookies.list() {
            let _ = jar.set(&ck.domain, &ck.to_set_cookie());
        }
        services.http = std::sync::Arc::new(merpati_http::HttpEngine::new(app.http.config(), jar));
    }
    let cancel = CancellationToken::new();
    tab.cancel = Some(cancel.clone());
    tab.started = Some(Instant::now());
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    let collection_id = tab.collection.clone();
    let persist = tab.persist_vars;
    match tab.mode {
        Mode::Functional => {
            let iterations = tab.iterations.max(1);
            tab.expected = iterations * order.len();
            let opts = RunOptions {
                iterations: Some(iterations),
                delay_ms: tab.delay_ms,
                data: tab.data.clone(),
                order: Some(order),
                bail: tab.bail,
                simulate_failure_rate: tab.fail_rate,
                base_dir: Some(app.ws.root().to_path_buf()),
                body_preview_bytes: 2048,
                keep_console: false,
                ..Default::default()
            };
            app.rt.spawn(async move {
                let mut vars = before.clone();
                let tx2 = tx.clone();
                let ctx2 = ctx.clone();
                let mut last_paint = Instant::now();
                let mut sink = move |e: RunEvent| {
                    let finished = matches!(e, RunEvent::Finished(_));
                    let _ = tx2.send(Msg::Run { tab: tab_id, event: Box::new(e) });
                    if finished || last_paint.elapsed() > Duration::from_millis(100) {
                        ctx2.request_repaint();
                        last_paint = Instant::now();
                    }
                };
                let result = merpati_runner::run_collection(&services, &c, &mut vars, opts, &mut sink, &cancel).await;
                if let Err(e) = result {
                    let _ = tx.send(Msg::Toast(format!("Run failed: {e}"), true));
                }
                let _ = tx.send(Msg::RunFinished {
                    tab: tab_id,
                    before,
                    after: vars,
                    collection: collection_id,
                    env,
                    persist,
                });
                ctx.request_repaint();
            });
        }
        Mode::Performance => {
            let opts = PerfOptions {
                virtual_users: tab.vus.max(1),
                duration: Duration::from_secs(tab.duration_s.max(1)),
                ramp_up: Duration::from_secs(tab.ramp_s),
                request_ids: Some(order),
            };
            app.rt.spawn(async move {
                let tx2 = tx.clone();
                let ctx2 = ctx.clone();
                let mut on_sample = move |s: &PerfSample| {
                    let _ = tx2.send(Msg::Perf { tab: tab_id, event: PerfMsg::Sample(s.clone()) });
                    ctx2.request_repaint();
                };
                let report =
                    merpati_runner::perf::run_perf(&services, &c, &before, opts, &mut on_sample, &cancel).await;
                let _ = tx.send(Msg::Perf { tab: tab_id, event: PerfMsg::Done(Box::new(report)) });
                ctx.request_repaint();
            });
        }
    }
}

fn export(tab: &RunnerTab, format: &str) -> Result<(), String> {
    let (Some(spool), Some(summary)) = (&tab.spool, &tab.summary) else { return Err("nothing to export".into()) };
    let ext = match format {
        "junit" => "xml",
        "html" => "html",
        _ => "json",
    };
    let Some(path) = rfd::FileDialog::new().set_file_name(format!("merpati-run.{ext}")).save_file() else {
        return Ok(());
    };
    let mut reporter: Box<dyn merpati_runner::report::Reporter> = match format {
        "junit" => Box::new(merpati_runner::report::JunitReporter::new(&path, &summary.collection)),
        "html" => Box::new(merpati_runner::report::HtmlReporter::new(&path)),
        _ => Box::new(merpati_runner::report::JsonReporter::new(&path)),
    };
    let text = std::fs::read_to_string(spool.path()).map_err(|e| e.to_string())?;
    for line in text.lines() {
        if let Ok(r) = serde_json::from_str::<RequestResult>(line) {
            reporter.on_event(&RunEvent::RequestFinished(Box::new(r)));
        }
    }
    reporter.finish(summary).map_err(|e| e.to_string())
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut RunnerTab) {
    let p = theme::pal(ui);
    let name = app.ws.data.collection(&tab.collection).map(|c| c.name.clone()).unwrap_or_default();
    view_title(ui, tab, &name, p.accent);
    ui.add_space(12.0);
    let mut run = false;
    let avail = ui.available_rect_before_wrap();
    if avail.width() < 820.0 {
        // Narrow: one pane at a time, switched with a segmented control.
        let has_results = !tab.results.is_empty() || tab.summary.is_some() || tab.perf_report.is_some();
        let results_label = if has_results && tab.mode == Mode::Functional {
            format!("{} {}", tr("Results"), tab.results.len())
        } else {
            tr("Results").to_string()
        };
        kit::segmented(ui, &mut tab.show_results, &[(false, tr("Setup")), (true, results_label.as_str())]);
        ui.add_space(10.0);
        let rest = ui.available_rect_before_wrap();
        if tab.show_results {
            ui.scope_builder(egui::UiBuilder::new().max_rect(rest), |ui| match tab.mode {
                Mode::Functional => functional_results(ui, tab),
                Mode::Performance => perf_results(ui, tab),
            });
        } else {
            ui.scope_builder(egui::UiBuilder::new().max_rect(rest), |ui| setup_panel(ui, tab, &mut run));
        }
        ui.advance_cursor_after_rect(rest);
        if app.pending_runner_start == Some(tab_id) {
            app.pending_runner_start = None;
            run = true;
        }
        if run {
            tab.show_results = true;
            start(app, tab_id, tab);
        }
        return;
    }
    // Setup column on the left (resizable by its edge), results on the right.
    let max_w = (avail.width() * 0.5).max(290.0);
    let panel_w = tab.panel_w.clamp(290.0, max_w);
    let left = Rect::from_min_size(avail.min, vec2(panel_w, avail.height()));
    let right = Rect::from_min_max(pos2(left.right() + 16.0, avail.top()), avail.max);
    ui.scope_builder(egui::UiBuilder::new().max_rect(left), |ui| setup_panel(ui, tab, &mut run));
    let handle = Rect::from_min_max(pos2(left.right() + 4.0, avail.top()), pos2(left.right() + 12.0, avail.bottom()));
    let h = ui.interact(handle, egui::Id::unique(("runner-split", tab_id)), Sense::drag());
    if h.hovered() || h.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        ui.painter().vline(handle.center().x, avail.y_range(), Stroke::new(2.0, p.accent));
    }
    if h.dragged() {
        tab.panel_w = (panel_w + h.drag_delta().x).clamp(290.0, max_w);
    }
    if h.double_clicked() {
        tab.panel_w = 340.0;
    }
    if app.pending_runner_start == Some(tab_id) {
        app.pending_runner_start = None;
        run = true;
    }
    if run {
        tab.show_results = true;
        start(app, tab_id, tab);
    }
    ui.scope_builder(egui::UiBuilder::new().max_rect(right), |ui| match tab.mode {
        Mode::Functional => functional_results(ui, tab),
        Mode::Performance => perf_results(ui, tab),
    });
    ui.advance_cursor_after_rect(avail);
}

/// Title row with the mode switch.
fn view_title(ui: &mut Ui, tab: &mut RunnerTab, collection: &str, color: Color32) {
    let running = tab.is_running();
    crate::request_tab::view_header(ui, icons::PLAY_CIRCLE, color, tr("Collection Runner"), collection, |ui| {
        ui.add_enabled_ui(!running, |ui| {
            kit::segmented(
                ui,
                &mut tab.mode,
                &[(Mode::Functional, tr("Functional")), (Mode::Performance, tr("Performance"))],
            );
        });
    });
}

/// The setup card: run order, options and the Run button.
fn setup_panel(ui: &mut Ui, tab: &mut RunnerTab, run: &mut bool) {
    let p = theme::pal(ui);
    let footer_h = 58.0;
    kit::card(ui).inner_margin(egui::Margin::ZERO).show(ui, |ui| {
        let h = ui.available_height();
        ui.set_min_height(h);
        ui.set_width(ui.available_width());
        egui::ScrollArea::vertical().id_salt("runner-cfg").auto_shrink([false, false]).max_height(h - footer_h).show(
            ui,
            |ui| {
                egui::Frame::new().inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    run_order(ui, tab);
                    ui.add_space(16.0);
                    match tab.mode {
                        Mode::Functional => functional_options(ui, tab),
                        Mode::Performance => perf_options(ui, tab),
                    }
                });
            },
        );
        // Footer with the main action.
        let top = ui.cursor().top();
        let r = ui.max_rect();
        ui.painter().hline(r.left()..=r.right(), top, Stroke::new(1.0, p.border));
        egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let selected = tab.plan.iter().filter(|p| p.3).count();
                let summary = match tab.mode {
                    Mode::Functional if tab.iterations <= 1 => trf("{} requests", &[&selected]),
                    Mode::Functional => trf("{} requests × {} iterations", &[&selected, &tab.iterations]),
                    Mode::Performance => trf("{} users · {} s", &[&tab.vus, &tab.duration_s]),
                };
                ui.add(egui::Label::new(RichText::new(summary).small().color(p.text_muted)).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if tab.is_running() {
                        if kit::button_ex(ui, kit::Kind::Danger, Some(icons::STOP), tr("Stop"), false).clicked() {
                            tab.cancel_all();
                        }
                    } else {
                        ui.add_enabled_ui(selected > 0, |ui| {
                            let label = match tab.mode {
                                Mode::Functional => tr("Run"),
                                Mode::Performance => tr("Start load test"),
                            };
                            if kit::primary_icon(ui, icons::PLAY, label).clicked() {
                                *run = true;
                            }
                        })
                        .response
                        .on_disabled_hover_text(tr("Select at least one request to run"));
                    }
                });
            });
        });
    });
}

/// The requests to run, in order: drag handle, checkbox, method, name.
fn run_order(ui: &mut Ui, tab: &mut RunnerTab) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let selected = tab.plan.iter().filter(|p| p.3).count();
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        ui.label(kit::section_title(tr("Run order")));
        kit::pill(ui, &format!("{selected}/{}", tab.plan.len()), if selected == 0 { p.err } else { p.text_muted });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if kit::ghost(ui, None, tr("None")).on_hover_text(tr("Deselect all")).clicked() {
                tab.plan.iter_mut().for_each(|p| p.3 = false);
            }
            if kit::ghost(ui, None, tr("All")).on_hover_text(tr("Select all")).clicked() {
                tab.plan.iter_mut().for_each(|p| p.3 = true);
            }
        });
    });
    ui.add_space(4.0);
    if tab.plan.is_empty() {
        banner(ui, icons::INFO, p.text_muted, tr("This collection has no requests to run."));
        return;
    }
    const ROW: f32 = 30.0;
    let list_top = ui.cursor().top();
    let width = ui.available_width();
    let n = tab.plan.len();
    let mut moved: Option<(usize, usize)> = None;
    let mut toggle_only: Option<usize> = None;
    let drop_at = tab.drag.and_then(|_| {
        ui.ctx().pointer_interact_pos().map(|pos| (((pos.y - list_top) / ROW).round().max(0.0) as usize).min(n))
    });
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for i in 0..n {
            let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW), Sense::click());
            let dragging = tab.drag == Some(i);
            let hovered = ui.rect_contains_pointer(rect);
            if dragging {
                ui.painter().rect_filled(rect, CornerRadius::same(6), p.accent_soft);
            } else if hovered {
                ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover);
            }
            let (_, name, badge, on) = &mut tab.plan[i];
            // Drag handle.
            let handle = Rect::from_min_size(rect.min, vec2(20.0, ROW));
            let h = ui.interact(handle, ui.make_persistent_id(("drag", i)), Sense::drag());
            widgets::a11y_name(&h, &trf("Move {}", &[&name]));
            if h.hovered() || h.dragged() {
                ui.ctx().set_cursor_icon(if h.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
            }
            let handle_color = if h.hovered() || dragging { p.text_muted } else { p.text_faint };
            ui.painter().text(
                handle.center(),
                Align2::CENTER_CENTER,
                icons::DOTS_SIX_VERTICAL,
                theme::icon_font(14.0),
                handle_color,
            );
            if h.drag_started() {
                tab.drag = Some(i);
            }
            if h.drag_stopped()
                && let Some(to) = drop_at
            {
                moved = Some((i, to));
            }
            // Checkbox.
            let c = Rect::from_center_size(pos2(rect.left() + 32.0, rect.center().y), vec2(18.0, ROW - 6.0));
            let r = ui.put(c, egui::Checkbox::without_text(on));
            widgets::a11y_name(&r, name);
            // Method and name.
            let badge_rect = Rect::from_min_size(pos2(rect.left() + 46.0, rect.top()), vec2(46.0, ROW));
            let badge_text = widgets::method_badge(badge, dark);
            let g = egui::WidgetText::from(badge_text).into_galley(
                ui,
                Some(egui::TextWrapMode::Extend),
                46.0,
                egui::TextStyle::Body,
            );
            ui.painter().galley(pos2(badge_rect.left(), rect.center().y - g.size().y / 2.0), g, p.text);
            let color = if *on { p.text } else { p.text_muted };
            let g = elide(ui, name, egui::FontId::proportional(13.0), color, rect.right() - badge_rect.right() - 8.0);
            ui.painter().galley(pos2(badge_rect.right(), rect.center().y - g.size().y / 2.0), g, color);
            if resp.clicked() {
                *on = !*on;
            }
            resp.context_menu(|ui| {
                if ui.button(kit::with_icon(icons::ARROW_UP, tr("Move up"))).clicked() && i > 0 {
                    moved = Some((i, i - 1));
                    ui.close();
                }
                if ui.button(kit::with_icon(icons::ARROW_DOWN, tr("Move down"))).clicked() && i + 1 < n {
                    moved = Some((i, i + 2));
                    ui.close();
                }
                if ui.button(kit::with_icon(icons::CHECK_SQUARE, tr("Run only this"))).clicked() {
                    toggle_only = Some(i);
                    ui.close();
                }
            });
        }
    });
    // Where the dragged row would land.
    if let (Some(from), Some(to)) = (tab.drag, drop_at)
        && to != from
        && to != from + 1
    {
        let y = list_top + to as f32 * ROW;
        ui.painter().hline(
            (ui.min_rect().left() + 4.0)..=(ui.min_rect().left() + width - 4.0),
            y,
            Stroke::new(2.0, p.accent),
        );
    }
    if moved.is_some() || !ui.input(|i| i.pointer.any_down()) {
        tab.drag = None;
    }
    if let Some((from, to)) = moved {
        let item = tab.plan.remove(from);
        let to = if to > from { to - 1 } else { to };
        tab.plan.insert(to.min(tab.plan.len()), item);
    }
    if let Some(i) = toggle_only {
        for (j, p) in tab.plan.iter_mut().enumerate() {
            p.3 = j == i;
        }
    }
    ui.add_space(4.0);
    ui.label(kit::hint(ui, tr("Drag the handles to change the order; right-click a request for more.")));
}

/// A compact option row for the narrow setup panel: label (and help) on the
/// left, a control of `control_w` on the right.
fn opt_row(ui: &mut Ui, label: &str, help: &str, control_w: f32, add: impl FnOnce(&mut Ui)) {
    let p = theme::pal(ui);
    ui.horizontal_top(|ui| {
        let label_w = (ui.available_width() - control_w - 12.0).max(80.0);
        ui.allocate_ui_with_layout(vec2(label_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(label_w);
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.add_space(3.0);
            ui.label(RichText::new(label).font(theme::medium(13.0)).color(p.text));
            if !help.is_empty() {
                ui.label(RichText::new(help).small().color(p.text_muted));
            }
        });
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), 26.0),
            egui::Layout::right_to_left(egui::Align::Center),
            add,
        );
    });
    ui.add_space(10.0);
}

fn number(
    ui: &mut Ui,
    name: &str,
    value: &mut impl egui::emath::Numeric,
    range: std::ops::RangeInclusive<f64>,
    suffix: &str,
) {
    let r = ui.add(egui::DragValue::new(value).range(range).speed(0.2).suffix(suffix).update_while_editing(false));
    widgets::a11y_name(&r, name);
}

fn functional_options(ui: &mut Ui, tab: &mut RunnerTab) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.set_height(22.0);
        ui.label(kit::section_title(tr("Options")));
    });
    ui.add_space(8.0);
    opt_row(ui, tr("Iterations"), tr("How many times to run the requests."), 90.0, |ui| {
        number(ui, tr("Iterations"), &mut tab.iterations, 1.0..=1_000_000.0, "");
    });
    opt_row(ui, tr("Delay"), tr("Pause between requests."), 90.0, |ui| {
        number(ui, tr("Delay"), &mut tab.delay_ms, 0.0..=600_000.0, " ms");
    });
    let mut pick = false;
    opt_row(ui, tr("Data file"), tr("CSV or JSON; one iteration per row."), 110.0, |ui| {
        let label = if tab.data_path.is_some() { tr("Change…") } else { tr("Select file") };
        pick = kit::button_ex(ui, kit::Kind::Secondary, Some(icons::UPLOAD_SIMPLE), label, true).clicked();
    });
    if pick && let Some(path) = rfd::FileDialog::new().add_filter("CSV or JSON", &["csv", "json"]).pick_file() {
        match merpati_runner::data::load(&path) {
            Ok(rows) => {
                tab.iterations = rows.len().max(1);
                tab.data = rows;
                tab.data_error = None;
            }
            Err(e) => tab.data_error = Some(e.to_string()),
        }
        tab.data_path = Some(path);
    }
    if let Some(path) = tab.data_path.clone() {
        let mut clear = false;
        egui::Frame::new()
            .fill(p.chrome)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::FILE_TEXT).font(theme::icon_font(15.0)).color(p.text_muted));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let file = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                        ui.add(egui::Label::new(RichText::new(file).font(theme::medium(12.5))).truncate())
                            .on_hover_text(path.display().to_string());
                        ui.label(RichText::new(trf("{} rows", &[&tab.data.len()])).small().color(p.text_muted));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        clear = widgets::small_icon_button(ui, icons::X, tr("Remove data file")).clicked();
                    });
                });
            });
        if clear {
            tab.data_path = None;
            tab.data.clear();
            tab.data_error = None;
        }
        ui.add_space(6.0);
    }
    if let Some(e) = &tab.data_error {
        banner(ui, icons::WARNING_CIRCLE, p.err, e);
        ui.add_space(6.0);
    }
    if !tab.data.is_empty() {
        let id = ui.make_persistent_id("data-preview");
        if crate::request_tab::disclosure(ui, id, tr("Preview data"), Some(tab.data.len().min(10)), false) {
            let keys: Vec<String> = tab.data.first().map(|r| r.keys().cloned().collect()).unwrap_or_default();
            if !keys.is_empty() {
                let rows: Vec<Vec<String>> = tab
                    .data
                    .iter()
                    .take(10)
                    .map(|r| {
                        keys.iter()
                            .map(|k| r.get(k).map(merpati_core::variables::value_to_string).unwrap_or_default())
                            .collect()
                    })
                    .collect();
                let cols: Vec<&str> = keys.iter().map(String::as_str).collect();
                let widths = vec![1.0 / cols.len() as f32; cols.len()];
                crate::request_tab::read_table(ui, "data-preview-table", &cols, &widths, &rows);
            }
        }
        ui.add_space(8.0);
    }
    opt_row(ui, tr("Keep variable values"), tr("Write variables changed by scripts back after the run."), 40.0, |ui| {
        crate::auth_ui::switch(ui, &mut tab.persist_vars, tr("Keep variable values after the run"));
    });
    opt_row(ui, tr("Save cookies"), tr("Keep cookies set during the run in the cookie jar."), 40.0, |ui| {
        crate::auth_ui::switch(ui, &mut tab.save_cookies, tr("Save cookies after the run"));
    });
    opt_row(ui, tr("Stop at the first failure"), tr("Stop when a request or a test fails."), 40.0, |ui| {
        crate::auth_ui::switch(ui, &mut tab.bail, tr("Stop at the first failure"));
    });
    opt_row(
        ui,
        tr("Simulate network failures"),
        tr("This fraction of requests fails with a simulated network error instead of being sent"),
        130.0,
        |ui| {
            let mut pct = (tab.fail_rate * 100.0).round();
            let r = ui.add(egui::Slider::new(&mut pct, 0.0..=100.0).suffix("%").show_value(true).step_by(5.0));
            widgets::a11y_name(&r, tr("Simulate network failures"));
            if r.changed() {
                tab.fail_rate = pct / 100.0;
            }
        },
    );
}

fn perf_options(ui: &mut Ui, tab: &mut RunnerTab) {
    ui.horizontal(|ui| {
        ui.set_height(22.0);
        ui.label(kit::section_title(tr("Load profile")));
    });
    ui.add_space(8.0);
    opt_row(ui, tr("Virtual users"), tr("Users sending requests at the same time."), 90.0, |ui| {
        number(ui, tr("Virtual users"), &mut tab.vus, 1.0..=1000.0, "");
    });
    opt_row(ui, tr("Duration"), tr("How long the test runs."), 90.0, |ui| {
        number(ui, tr("Duration"), &mut tab.duration_s, 1.0..=3600.0, " s");
    });
    opt_row(ui, tr("Ramp-up"), tr("Time to reach all virtual users."), 90.0, |ui| {
        number(ui, tr("Ramp-up"), &mut tab.ramp_s, 0.0..=3600.0, " s");
    });
    ui.label(kit::hint(
        ui,
        tr("Each virtual user loops over the selected requests with its own copy of the variables."),
    ));
}

/// A thin progress bar with a label on the left and one on the right.
fn progress(ui: &mut Ui, frac: f32, left: &str, right: &str) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(13.0).color(p.accent));
        let small = egui::FontId::proportional(crate::request_tab::small_size(ui));
        let right_w = ui.painter().layout_no_wrap(right.to_string(), small, p.text_muted).size().x;
        let left_w = (ui.available_width() - right_w - 12.0).max(40.0);
        ui.allocate_ui_with_layout(vec2(left_w, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(left).font(theme::medium(13.0))).truncate());
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(right).small().color(p.text_muted));
        });
    });
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(3), p.subtle);
    let w = (rect.width() * frac.clamp(0.0, 1.0)).max(6.0);
    ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(w, rect.height())), CornerRadius::same(3), p.accent);
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
}

fn functional_results(ui: &mut Ui, tab: &mut RunnerTab) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    if tab.results.is_empty() && !tab.is_running() && tab.summary.is_none() {
        kit::empty_state(
            ui,
            icons::PLAY_CIRCLE,
            tr("Ready to run"),
            tr("Choose the requests and options on the left, then press Run."),
            |_| {},
        );
        return;
    }
    let s = tab.summary.clone().unwrap_or_else(|| tab.live.clone());
    let failed = s.tests_failed + s.script_errors;
    // Summary cards.
    let skipped = s.skipped + s.tests_skipped;
    let req_hint = if skipped > 0 { trf("{} skipped", &[&skipped]) } else { tr("requests").to_string() };
    let duration = match (&tab.summary, tab.started) {
        (Some(sum), _) => fmt_ms(sum.total_time_ms),
        (None, Some(t)) => fmt_ms(t.elapsed().as_secs_f64() * 1000.0),
        _ => "–".into(),
    };
    let avg = tab.summary.as_ref().map(|sum| fmt_ms(sum.response_time_avg_ms())).unwrap_or_else(|| "–".into());
    stat_cards(
        ui,
        130.0,
        &[
            (tr("Requests"), s.executed.to_string(), None, req_hint),
            (tr("Passed"), s.tests_passed.to_string(), (s.tests_passed > 0).then_some(p.ok), tr("tests").into()),
            (tr("Failed"), failed.to_string(), (failed > 0).then_some(p.err), tr("tests and scripts").into()),
            (
                tr("Errors"),
                s.requests_failed.to_string(),
                (s.requests_failed > 0).then_some(p.err),
                tr("network").into(),
            ),
            (tr("Duration"), duration, None, tr("total").into()),
            (tr("Avg response"), avg, None, tr("per request").into()),
        ],
    );
    ui.add_space(12.0);
    if tab.is_running() {
        let done = tab.live.executed;
        let frac = if tab.expected > 0 { done as f32 / tab.expected as f32 } else { 0.0 };
        let elapsed = tab.started.map(|t| fmt_ms(t.elapsed().as_secs_f64() * 1000.0)).unwrap_or_default();
        progress(ui, frac, &trf("Running… {} of {}", &[&done, &tab.expected]), &elapsed);
    } else if let Some(sum) = &tab.summary {
        if sum.cancelled {
            banner(ui, icons::STOP, p.warn, tr("Run cancelled"));
        } else if sum.bailed {
            banner(ui, icons::WARNING, p.warn, tr("Stopped at the first failure"));
        } else if failed + sum.requests_failed == 0 && sum.tests_passed == 0 {
            banner(ui, icons::CHECK_CIRCLE, p.ok, &trf("{} requests sent, no tests ran.", &[&sum.executed]));
        } else if failed + sum.requests_failed == 0 {
            banner(
                ui,
                icons::CHECK_CIRCLE,
                p.ok,
                &trf("All {} tests passed in {} requests.", &[&sum.tests_passed, &sum.executed]),
            );
        } else {
            banner(
                ui,
                icons::X_CIRCLE,
                p.err,
                &trf("{} failures in {} requests.", &[&(failed + sum.requests_failed), &sum.executed]),
            );
        }
    }
    ui.add_space(12.0);
    let n_failed = tab.results.iter().filter(|r| r.failed()).count();
    let n_all = tab.results.len();
    ui.horizontal(|ui| {
        ui.set_height(28.0);
        let all = format!("{} {n_all}", tr("All"));
        let passed = format!("{} {}", tr("Passed"), n_all - n_failed);
        let failed = format!("{} {n_failed}", tr("Failed"));
        kit::segmented(
            ui,
            &mut tab.filter,
            &[(Filter::All, all.as_str()), (Filter::Passed, passed.as_str()), (Filter::Failed, failed.as_str())],
        );
        if tab.dropped > 0 {
            ui.label(
                widgets::muted(
                    ui,
                    trf(
                        "showing the latest {}; export for all {} results",
                        &[&KEEP_RESULTS, &(tab.dropped + tab.results.len())],
                    ),
                )
                .small(),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if tab.summary.is_some() {
                let r = kit::button_ex(
                    ui,
                    kit::Kind::Secondary,
                    Some(icons::EXPORT),
                    &format!("{}  {}", tr("Export"), icons::CARET_DOWN),
                    true,
                );
                egui::Popup::menu(&r).show(|ui| {
                    for (f, icon, label) in [
                        ("json", icons::BRACKETS_CURLY, "JSON"),
                        ("junit", icons::CODE, "JUnit XML"),
                        ("html", icons::GLOBE, tr("HTML report")),
                    ] {
                        if ui.button(kit::with_icon(icon, label)).clicked() {
                            if let Err(e) = export(tab, f) {
                                tracing::error!("export failed: {e}");
                            }
                            ui.close();
                        }
                    }
                });
            }
        });
    });
    ui.add_space(8.0);
    let filtered: Vec<usize> = tab
        .results
        .iter()
        .enumerate()
        .filter(|(_, r)| match tab.filter {
            Filter::All => true,
            Filter::Passed => !r.failed(),
            Filter::Failed => r.failed(),
        })
        .map(|(i, _)| i)
        .collect();
    // Arrow keys move the selection when no text field has focus.
    if let Some(sel) = tab.selected
        && !ui.ctx().egui_wants_keyboard_input()
        && let Some(pos) = filtered.iter().position(|&i| i == sel)
    {
        let (down, up, esc) = ui.input(|i| {
            (i.key_pressed(egui::Key::ArrowDown), i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::Escape))
        });
        if down && pos + 1 < filtered.len() {
            tab.selected = Some(filtered[pos + 1]);
        } else if up && pos > 0 {
            tab.selected = Some(filtered[pos - 1]);
        } else if esc {
            tab.selected = None;
        }
    }
    results_table(ui, tab, &filtered, dark);
    if let Some(r) = tab.selected.and_then(|i| tab.results.get(i)).cloned() {
        ui.add_space(10.0);
        if result_detail(ui, &r, dark) {
            tab.selected = None;
        }
    }
}

/// The results as a table: iteration, method, request, tests, status, time.
fn results_table(ui: &mut Ui, tab: &mut RunnerTab, filtered: &[usize], dark: bool) {
    let p = theme::pal(ui);
    const HEAD: f32 = 28.0;
    const ROW: f32 = 32.0;
    let width = ui.available_width();
    let height = if tab.selected.is_some() { (ui.available_height() * 0.45).max(140.0) } else { ui.available_height() };
    let (tests_w, status_w, time_w) = (84.0, 104.0, 76.0);
    let head = ui.allocate_exact_size(vec2(width, HEAD), Sense::hover()).0;
    ui.painter().rect_filled(head, CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, p.chrome);
    let right = head.right();
    let cols = [
        (head.left() + 10.0, Align2::LEFT_CENTER, tr("Request")),
        (right - time_w - status_w - tests_w + tests_w / 2.0, Align2::CENTER_CENTER, tr("Tests")),
        (right - time_w - status_w + 10.0, Align2::LEFT_CENTER, tr("Status")),
        (right - 12.0, Align2::RIGHT_CENTER, tr("Time")),
    ];
    for (x, align, label) in cols {
        ui.painter().text(pos2(x, head.center().y), align, label, theme::medium(11.5), p.text_muted);
    }
    let top = head.top();
    let mut clicked = None;
    let body_h = (height - HEAD).max(ROW * 2.0);
    let area = egui::ScrollArea::vertical().id_salt("run-results").auto_shrink([false, true]).max_height(body_h);
    let scroll = kit::show_rows(area, ui, ROW, filtered.len(), |ui, range| {
        for &i in &filtered[range] {
            let r = &tab.results[i];
            let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW), Sense::click());
            let painter = ui.painter();
            painter.hline(rect.x_range(), rect.top(), Stroke::new(1.0, p.border));
            if tab.selected == Some(i) {
                painter.rect_filled(rect.shrink2(vec2(1.0, 0.5)), 0.0, p.accent_soft);
            } else if resp.hovered() {
                painter.rect_filled(rect.shrink2(vec2(1.0, 0.5)), 0.0, p.faint_row());
            }
            let cy = rect.center().y;
            // Iteration and method.
            painter.text(
                pos2(rect.left() + 10.0, cy),
                Align2::LEFT_CENTER,
                format!("#{}", r.iteration + 1),
                egui::FontId::proportional(11.5),
                p.text_faint,
            );
            let badge = egui::WidgetText::from(widgets::method_badge(&r.method, dark)).into_galley(
                ui,
                Some(egui::TextWrapMode::Extend),
                50.0,
                egui::TextStyle::Body,
            );
            ui.painter().galley(pos2(rect.left() + 40.0, cy - badge.size().y / 2.0), badge, p.text);
            // Path and name.
            // The folders give way first, so the name stays readable.
            let name_x = rect.left() + 92.0;
            let name_w = (right - time_w - status_w - tests_w - name_x - 8.0).max(40.0);
            let line = |text: &str, font: egui::FontId, color, width: f32| {
                let mut job =
                    egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(font, color));
                job.wrap = egui::text::TextWrapping::truncate_at_width(width);
                ui.painter().layout_job(job)
            };
            let name = line(&r.name, theme::medium(13.0), p.text, name_w);
            let sep = line(" / ", egui::FontId::proportional(13.0), p.text_faint, f32::INFINITY);
            let sep_w = sep.size().x;
            let room = name_w - name.size().x - sep_w;
            let mut x = name_x;
            if !r.path.is_empty() && room >= 32.0 {
                let folders = line(&r.path.join(" / "), egui::FontId::proportional(13.0), p.text_muted, room);
                let w = folders.size().x;
                ui.painter().galley(pos2(x, cy - folders.size().y / 2.0), folders, p.text_muted);
                ui.painter().galley(pos2(x + w, cy - sep.size().y / 2.0), sep, p.text_faint);
                x += w + sep_w;
            }
            ui.painter().galley(pos2(x, cy - name.size().y / 2.0), name, p.text);
            // Tests.
            let tests_c = right - time_w - status_w - tests_w / 2.0;
            if !r.tests.is_empty() {
                let failed = r.tests_failed();
                let passed = r.tests.iter().filter(|t| t.passed && !t.skipped).count();
                let (text, color) = if failed > 0 {
                    (format!("{} {failed}/{}", icons::X, passed + failed), p.err)
                } else {
                    (format!("{} {passed}", icons::CHECK), p.ok)
                };
                let g = ui.painter().layout_no_wrap(text, theme::medium(12.0), color);
                ui.painter().galley(pos2(tests_c - g.size().x / 2.0, cy - g.size().y / 2.0), g, color);
            } else {
                ui.painter().text(
                    pos2(tests_c, cy),
                    Align2::CENTER_CENTER,
                    "–",
                    egui::FontId::proportional(12.0),
                    p.text_faint,
                );
            }
            // Status.
            let status_x = right - time_w - status_w + 10.0;
            let (label, color) = match (r.status, &r.error) {
                (_, Some(_)) => (tr("Error").to_string(), p.err),
                (Some(code), None) => (code.to_string(), theme::status_color(code, dark)),
                _ => (tr("Skipped").to_string(), p.text_muted),
            };
            let pill_w = ui.painter().layout_no_wrap(label.clone(), theme::medium(11.0), color).size().x + 14.0;
            crate::request_tab::paint_pill(ui, pos2(status_x + pill_w, cy), &label, color);
            // Time.
            ui.painter().text(
                pos2(right - 12.0, cy),
                Align2::RIGHT_CENTER,
                fmt_ms(r.response_time_ms),
                theme::mono(12.0),
                p.text_muted,
            );
            widgets::a11y_button(&resp, &format!("{} {}", r.method, r.name));
            if resp.clicked() {
                clicked = Some(i);
            }
        }
    });
    let bottom = scroll.inner_rect.bottom().max(head.bottom());
    let table = Rect::from_min_max(pos2(head.left(), top), pos2(head.right(), bottom));
    ui.painter().rect_stroke(table, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
    if filtered.is_empty() {
        ui.add_space(10.0);
        ui.label(widgets::muted(
            ui,
            if tab.is_running() { tr("Waiting for results…") } else { tr("No results match this filter.") },
        ));
    }
    if let Some(i) = clicked {
        tab.selected = if tab.selected == Some(i) { None } else { Some(i) };
    }
}

/// Details of one result. Returns true when closed.
fn result_detail(ui: &mut Ui, r: &RequestResult, dark: bool) -> bool {
    let p = theme::pal(ui);
    let mut close = false;
    kit::card(ui).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.set_height(26.0);
            ui.label(widgets::method_badge(&r.method, dark));
            ui.label(RichText::new(&r.name).font(theme::semibold(14.0)));
            match (r.status, &r.error) {
                (_, Some(_)) => {
                    kit::pill(ui, tr("Error"), p.err);
                }
                (Some(code), None) => {
                    kit::status_pill(ui, code, &r.status_text);
                }
                _ => {
                    kit::pill(ui, tr("Skipped"), p.text_muted);
                }
            }
            kit::metric(ui, icons::TIMER, &fmt_ms(r.response_time_ms));
            kit::metric(ui, icons::DOWNLOAD_SIMPLE, &merpati_core::util::human_bytes(r.response_size));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = kit::icon_button(ui, icons::X, tr("Close details")).clicked();
            });
        });
        ui.add(egui::Label::new(RichText::new(&r.url).monospace().color(p.text_muted)).selectable(true).truncate());
        ui.add_space(8.0);
        egui::ScrollArea::vertical().id_salt("run-detail").auto_shrink([false; 2]).show(ui, |ui| {
            if let Some(e) = &r.error {
                banner(ui, icons::WARNING_CIRCLE, p.err, e);
                ui.add_space(6.0);
            }
            for e in &r.script_errors {
                banner(ui, icons::WARNING_CIRCLE, p.err, &format!("{}: {}", e.location, e.message));
                ui.add_space(6.0);
            }
            if !r.tests.is_empty() {
                let tests: Vec<&merpati_core::script::TestResult> = r.tests.iter().collect();
                crate::request_tab::test_list(ui, &tests);
                ui.add_space(10.0);
            }
            if !r.response_headers.is_empty() {
                let id = ui.make_persistent_id("run-headers");
                if crate::request_tab::disclosure(ui, id, tr("Headers"), Some(r.response_headers.len()), false) {
                    let rows: Vec<Vec<String>> =
                        r.response_headers.iter().map(|(k, v)| vec![k.clone(), v.clone()]).collect();
                    crate::request_tab::read_table(
                        ui,
                        "run-headers-table",
                        &[tr("Key"), tr("Value")],
                        &[0.32, 0.68],
                        &rows,
                    );
                }
                ui.add_space(6.0);
            }
            if let Some(b) = &r.body_preview {
                let id = ui.make_persistent_id("run-body");
                if crate::request_tab::disclosure(ui, id, tr("Body"), None, true) {
                    egui::Frame::new()
                        .fill(p.chrome)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(egui::Margin::same(10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(egui::Label::new(RichText::new(b).monospace().size(11.5)).selectable(true).wrap());
                        });
                }
            }
        });
    });
    close
}

fn perf_results(ui: &mut Ui, tab: &mut RunnerTab) {
    let p = theme::pal(ui);
    let samples = &tab.perf_samples;
    if samples.is_empty() && tab.perf_report.is_none() {
        if tab.is_running() {
            progress(ui, 0.0, tr("Starting virtual users…"), "");
            return;
        }
        kit::empty_state(
            ui,
            icons::GAUGE,
            tr("Load test"),
            tr("Set virtual users and duration, then start the test to measure throughput and latency."),
            |_| {},
        );
        return;
    }
    egui::ScrollArea::vertical().id_salt("perf-results").auto_shrink([false; 2]).show(ui, |ui| {
        if let Some(r) = &tab.perf_report {
            let err_color = (r.errors > 0).then_some(p.err);
            stat_cards(
                ui,
                140.0,
                &[
                    (tr("Requests"), r.total.to_string(), None, trf("{} virtual users", &[&r.virtual_users])),
                    (
                        tr("Throughput"),
                        format!("{:.1}", r.throughput_rps),
                        Some(p.accent_text),
                        tr("requests per second").into(),
                    ),
                    (
                        tr("Error rate"),
                        format!("{:.2}%", r.error_rate * 100.0),
                        err_color,
                        trf("{} errors", &[&r.errors]),
                    ),
                    (tr("Avg latency"), fmt_ms(r.latency.avg_ms), None, trf("min {}", &[&fmt_ms(r.latency.min_ms)])),
                ],
            );
            if r.cancelled {
                ui.add_space(10.0);
                banner(
                    ui,
                    icons::STOP,
                    p.warn,
                    tr("The test was stopped before the end; numbers cover the time it ran."),
                );
            }
        } else if let Some(s) = samples.last() {
            let frac = s.second as f32 / tab.duration_s.max(1) as f32;
            progress(
                ui,
                frac,
                &trf(
                    "{} req/s · {} users · avg {}",
                    &[&format!("{:.1}", s.rps), &s.active_vus, &fmt_ms(s.avg_latency_ms)],
                ),
                &format!("{} / {} s", s.second.min(tab.duration_s), tab.duration_s),
            );
        }
        ui.add_space(16.0);
        ui.horizontal_top(|ui| {
            let total = ui.available_width();
            let side = tab.perf_report.is_some() && total > 760.0;
            let chart_w = if side { total * 0.62 } else { total };
            ui.allocate_ui_with_layout(vec2(chart_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(chart_w);
                crate::request_tab::detail_heading(ui, icons::CHART_BAR, tr("Throughput and latency"));
                kit::card(ui).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    timeline_chart(ui, &tab.perf_samples);
                });
            });
            if side && let Some(r) = &tab.perf_report {
                ui.add_space(12.0);
                let w = ui.available_width();
                ui.allocate_ui_with_layout(vec2(w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(w);
                    crate::request_tab::detail_heading(ui, icons::TIMER, tr("Latency percentiles"));
                    kit::card(ui).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        percentiles(ui, &r.latency);
                    });
                });
            }
        });
        if let Some(r) = &tab.perf_report {
            if ui.available_width() <= 760.0 {
                ui.add_space(16.0);
                crate::request_tab::detail_heading(ui, icons::TIMER, tr("Latency percentiles"));
                kit::card(ui).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    percentiles(ui, &r.latency);
                });
            }
            ui.add_space(16.0);
            crate::request_tab::detail_heading(ui, icons::LIST, tr("Per request"));
            let rows: Vec<Vec<String>> = r
                .per_request
                .iter()
                .map(|q| {
                    vec![
                        q.name.clone(),
                        q.total.to_string(),
                        q.errors.to_string(),
                        fmt_ms(q.latency.avg_ms),
                        fmt_ms(q.latency.p95_ms),
                        fmt_ms(q.latency.max_ms),
                    ]
                })
                .collect();
            crate::request_tab::read_table(
                ui,
                "perf-per-request",
                &[tr("Request"), tr("Total"), tr("Errors"), tr("Avg"), "p95", tr("Max")],
                &[0.35, 0.13, 0.13, 0.13, 0.13, 0.13],
                &rows,
            );
        }
    });
}

/// Latency percentiles as horizontal bars.
fn percentiles(ui: &mut Ui, l: &merpati_runner::perf::LatencyStats) {
    let p = theme::pal(ui);
    let max = l.max_ms.max(0.001);
    for (label, v, color) in [
        ("p50", l.p50_ms, p.ok),
        ("p90", l.p90_ms, p.info),
        ("p95", l.p95_ms, p.info),
        ("p99", l.p99_ms, p.warn),
        (tr("Max"), l.max_ms, p.err),
    ] {
        ui.horizontal(|ui| {
            ui.set_height(24.0);
            ui.add_sized(
                [36.0, 24.0],
                egui::Label::new(RichText::new(label).font(theme::medium(12.0)).color(p.text_muted)),
            );
            let bar_w = (ui.available_width() - 74.0).max(40.0);
            let (rect, _) = ui.allocate_exact_size(vec2(bar_w, 24.0), Sense::hover());
            let track = Rect::from_center_size(rect.center(), vec2(rect.width(), 8.0));
            ui.painter().rect_filled(track, CornerRadius::same(4), p.subtle);
            let w = (track.width() * (v / max) as f32).max(3.0);
            ui.painter().rect_filled(
                Rect::from_min_size(track.min, vec2(w, track.height())),
                CornerRadius::same(4),
                color,
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(fmt_ms(v)).font(theme::mono(12.0)));
            });
        });
    }
}

/// Requests per second (filled line) and average latency per second, with
/// grid lines, axis labels and a legend.
fn timeline_chart(ui: &mut Ui, samples: &[PerfSample]) {
    let p = theme::pal(ui);
    let lat_color = p.info;
    // Legend.
    ui.horizontal(|ui| {
        for (color, label) in [(p.accent, tr("Requests per second")), (lat_color, tr("Average latency"))] {
            let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            ui.painter().rect_filled(Rect::from_center_size(r.center(), vec2(10.0, 3.0)), CornerRadius::same(1), color);
            ui.label(RichText::new(label).small().color(p.text_muted));
            ui.add_space(8.0);
        }
    });
    ui.add_space(6.0);
    let height = 180.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let plot = Rect::from_min_max(rect.min + vec2(40.0, 6.0), rect.max - vec2(52.0, 20.0));
    let painter = ui.painter_at(rect);
    let max_rps = samples.iter().map(|s| s.rps).fold(1.0, f64::max) * 1.1;
    let max_lat = samples.iter().map(|s| s.avg_latency_ms).fold(1.0, f64::max) * 1.1;
    // Grid and axis labels.
    for k in 0..=3 {
        let f = k as f32 / 3.0;
        let y = plot.bottom() - plot.height() * f;
        painter.hline(plot.x_range(), y, Stroke::new(1.0, if k == 0 { p.border_strong } else { p.border }));
        painter.text(
            pos2(plot.left() - 6.0, y),
            Align2::RIGHT_CENTER,
            format!("{:.0}", max_rps * f as f64),
            egui::FontId::proportional(10.5),
            p.accent_text,
        );
        painter.text(
            pos2(plot.right() + 6.0, y),
            Align2::LEFT_CENTER,
            fmt_ms(max_lat * f as f64),
            egui::FontId::proportional(10.5),
            lat_color,
        );
    }
    if samples.len() < 2 {
        painter.text(
            plot.center(),
            Align2::CENTER_CENTER,
            tr("Collecting samples…"),
            egui::FontId::proportional(12.0),
            p.text_muted,
        );
        return;
    }
    let n = (samples.len() - 1) as f32;
    let x_of = |i: usize| plot.left() + plot.width() * i as f32 / n;
    let rps: Vec<egui::Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, s)| pos2(x_of(i), plot.bottom() - plot.height() * (s.rps / max_rps) as f32))
        .collect();
    let lat: Vec<egui::Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, s)| pos2(x_of(i), plot.bottom() - plot.height() * (s.avg_latency_ms / max_lat) as f32))
        .collect();
    // Soft area under the throughput line.
    let fill = theme::soft(p.accent, if ui.visuals().dark_mode { 0.16 } else { 0.10 });
    for w in rps.windows(2) {
        let quad = vec![w[0], w[1], pos2(w[1].x, plot.bottom()), pos2(w[0].x, plot.bottom())];
        painter.add(egui::Shape::convex_polygon(quad, fill, Stroke::NONE));
    }
    painter.add(egui::Shape::line(rps.clone(), Stroke::new(2.0, p.accent)));
    painter.add(egui::Shape::line(lat.clone(), Stroke::new(1.5, lat_color)));
    // Time axis.
    let last = samples.last().map(|s| s.second).unwrap_or(0);
    for (i, align) in [(0usize, Align2::LEFT_TOP), (samples.len() - 1, Align2::RIGHT_TOP)] {
        painter.text(
            pos2(x_of(i), plot.bottom() + 4.0),
            align,
            format!("{} s", if i == 0 { samples[0].second } else { last }),
            egui::FontId::proportional(10.5),
            p.text_muted,
        );
    }
    // Hover readout.
    if let Some(pos) = resp.hover_pos()
        && plot.x_range().contains(pos.x)
    {
        let i = (((pos.x - plot.left()) / plot.width()) * n).round() as usize;
        if let Some(s) = samples.get(i) {
            let x = x_of(i);
            painter.vline(x, plot.y_range(), Stroke::new(1.0, p.border_strong));
            painter.circle_filled(rps[i], 3.5, p.accent);
            painter.circle_filled(lat[i], 3.0, lat_color);
            resp.on_hover_text(format!(
                "{} s\n{:.1} req/s\n{} {}\n{} {}",
                s.second,
                s.rps,
                fmt_ms(s.avg_latency_ms),
                tr("avg"),
                s.active_vus,
                tr("users")
            ));
        }
    }
    let _ = Color32::TRANSPARENT;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(name: String) -> RunEvent {
        RunEvent::RequestFinished(Box::new(RequestResult { name, ..Default::default() }))
    }

    /// Only the latest results are kept; the selected one must stay selected
    /// while older ones are dropped from the front.
    #[test]
    fn selection_follows_results_when_old_ones_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let ws = WsState::for_test(dir.path());
        let mut tab = RunnerTab::new(&ws, "none".into(), None);
        for i in 0..KEEP_RESULTS {
            tab.on_event(result(format!("r{i}")));
        }
        tab.selected = Some(5);
        tab.on_event(result("next".into()));
        assert_eq!(tab.selected.and_then(|i| tab.results.get(i)).map(|r| r.name.as_str()), Some("r5"));
        tab.selected = Some(0);
        tab.on_event(result("next2".into()));
        assert_eq!(tab.selected, None);
    }
}
