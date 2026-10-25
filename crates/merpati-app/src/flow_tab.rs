//! Flows canvas (FR-D4): blocks connected by edges, edited visually, run
//! with live per-block status, saved to `flows/<name>.flow.json`.
//!
//! The tab has a header (title, save state, zoom, Fit / Validate / Add block
//! / Run), the canvas (dot grid, block cards with ports, bezier
//! connections), the inspector on the right (flow settings, or the selected
//! block or connection as form sections) and the run log under the canvas.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontId, Key, Layout, Pos2, Rect, Response, RichText, Sense,
    Shape, Stroke, StrokeKind, Ui, UiBuilder, Vec2, vec2,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use merpati_core::model::RequestKind;
use merpati_core::protocols::AiProvider;
use merpati_core::util::human_ms;
use merpati_flows::{
    Block, BlockConfig, BlockKind, CompareOp, ConditionMode, Diagnostic, Edge, Flow, FlowEnv, FlowEvent, FlowInput,
    FlowResult, FlowRunOptions, ItemRef, LogLevelConfig, LoopMode, PORT_BRANCHES, RequestSource, Severity,
    TemplateMode, VariableScope,
};

use crate::app::{MerpatiApp, Msg, TabKind};
use crate::highlight::Lang;
use crate::i18n::{tr, trf};
use crate::theme::{self, Palette, RADIUS};
use crate::widgets::{self, KvOptions};
use crate::{icons, kit};

/// Block card layout in canvas units (screen pixels at 100 %).
const BLOCK_W: f32 = 200.0;
/// Tinted header with the kind icon, the name and the run badge.
const HEAD_H: f32 = 32.0;
/// Top of the first port row (under the summary line and a divider).
const PORTS_TOP: f32 = 60.0;
const PORT_ROW: f32 = 20.0;
/// Dot grid spacing; moved blocks snap to half of it.
const GRID: f32 = 20.0;
const MIN_ZOOM: f32 = 0.3;
const MAX_ZOOM: f32 = 2.0;
/// "Fit" never zooms out further than this, so blocks stay readable; a flow
/// too wide for the canvas is shown from its start instead.
const FIT_MIN_ZOOM: f32 = 0.7;
/// Below this zoom, port labels and summaries are left out.
const DETAIL_ZOOM: f32 = 0.55;
/// Run log lines shown (the newest ones).
const LOG_SHOWN: usize = 600;

pub enum FlowMsg {
    Event(FlowEvent),
    Done(Box<FlowResult>),
}

#[derive(Clone, PartialEq)]
enum Selection {
    Block(String),
    Edge(usize),
}

#[derive(Clone)]
enum BlockStatus {
    Running { since: Instant },
    Done { port: String, failed: bool, ms: f64, output: Arc<Value>, error: Option<String>, tests: (usize, usize) },
}

/// The answer of a streaming AI block so far.
struct LiveText {
    block: String,
    text: String,
    /// Run log line that grows with it.
    log: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogKind {
    /// A block finished normally.
    Step,
    Info,
    Debug,
    Warn,
    Error,
    /// The run succeeded.
    Done,
}

struct LogLine {
    /// Time since the run started.
    at: Duration,
    kind: LogKind,
    /// Display name of the block the line is about.
    block: Option<String>,
    text: String,
}

#[derive(Default)]
struct RunState {
    cancel: Option<CancellationToken>,
    started: Option<Instant>,
    status: HashMap<String, BlockStatus>,
    log: Vec<LogLine>,
    result: Option<Box<FlowResult>>,
    /// Steps still running → block id (parallel branches run several blocks
    /// at once, sometimes the same block twice).
    running: HashMap<usize, String>,
    /// Streamed AI text by step.
    live: HashMap<usize, LiveText>,
}

impl RunState {
    fn live_text(&self, block: &str) -> Option<&str> {
        self.live.values().filter(|l| l.block == block).map(|l| l.text.as_str()).next()
    }

    fn push(&mut self, kind: LogKind, block: Option<String>, text: impl Into<String>) -> usize {
        let at = self.started.map(|s| s.elapsed()).unwrap_or_default();
        self.log.push(LogLine { at, kind, block, text: text.into() });
        self.log.len() - 1
    }
}

pub struct FlowTab {
    pub flow: Flow,
    pub path: Option<PathBuf>,
    saved: Flow,
    last_edit: Option<Instant>,
    selected: Option<Selection>,
    connecting: Option<(String, String)>,
    pan: Vec2,
    zoom: f32,
    run: RunState,
    diagnostics: Vec<Diagnostic>,
    inputs: HashMap<String, String>,
    error: Option<String>,
    /// Automation: run on the next frame.
    pub auto_run: bool,
    /// Fit all blocks into view on the next frame.
    fit: bool,
    /// Size of the canvas when last drawn (new blocks go to its middle).
    view: Vec2,
    /// The user zoomed or panned since the last fit (no refit on resize).
    moved_view: bool,
    /// Inspector shown; `None` decides by the width of the tab.
    inspector: Option<bool>,
    /// The flow name being edited in the header.
    renaming: Option<String>,
    /// Canvas position of the last right click ("Add block here").
    menu_at: Option<(f32, f32)>,
}

impl FlowTab {
    pub fn new(flow: Flow, path: Option<PathBuf>) -> Self {
        FlowTab {
            saved: flow.clone(),
            flow,
            path,
            last_edit: None,
            selected: None,
            connecting: None,
            pan: Vec2::new(40.0, 40.0),
            zoom: 1.0,
            run: RunState::default(),
            diagnostics: Vec::new(),
            inputs: HashMap::new(),
            error: None,
            auto_run: false,
            fit: true,
            view: Vec2::ZERO,
            moved_view: false,
            inspector: None,
            renaming: None,
            menu_at: None,
        }
    }

    /// A new flow with a Start block.
    pub fn blank(name: &str) -> Self {
        let mut flow = Flow::new(name);
        if flow.start().is_none() {
            flow.add_block(Block::new(BlockKind::Start.default_config(), tr("Start"), (0.0, 120.0)));
        }
        let mut t = FlowTab::new(flow, None);
        t.last_edit = Some(Instant::now());
        t
    }

    pub fn dirty(&self) -> bool {
        self.flow != self.saved
    }

    pub fn is_running(&self) -> bool {
        self.run.cancel.is_some()
    }

    pub fn stop(&mut self) {
        if let Some(c) = self.run.cancel.take() {
            c.cancel();
        }
    }

    fn label_of(&self, block: &str) -> String {
        self.flow.block(block).map(|b| b.display_name().to_string()).unwrap_or_else(|| block.to_string())
    }

    pub fn on_msg(&mut self, m: FlowMsg) {
        match m {
            FlowMsg::Event(e) => match e {
                FlowEvent::Started { .. } => {}
                FlowEvent::BlockStarted { step, block, .. } => {
                    self.run.running.insert(step, block.clone());
                    if !matches!(self.run.status.get(&block), Some(BlockStatus::Running { .. })) {
                        self.run.status.insert(block, BlockStatus::Running { since: Instant::now() });
                    }
                }
                FlowEvent::Execution { .. } => {}
                FlowEvent::AiDelta { step, block, text } => match self.run.live.get_mut(&step) {
                    Some(live) => {
                        live.text.push_str(&text);
                        if let Some(line) = self.run.log.get_mut(live.log) {
                            line.text.push_str(&text);
                        }
                    }
                    None => {
                        let label = self.label_of(&block);
                        let log = self.run.push(LogKind::Info, Some(label), text.clone());
                        self.run.live.insert(step, LiveText { block, text, log });
                    }
                },
                FlowEvent::BlockFinished { step, block, port, output, duration_ms, failed, error, tests } => {
                    self.run.running.remove(&step);
                    self.run.live.remove(&step);
                    let passed = tests.iter().filter(|t| t.passed && !t.skipped).count();
                    let failed_tests = tests.iter().filter(|t| !t.passed && !t.skipped).count();
                    let label = self.label_of(&block);
                    match &error {
                        Some(e) => self.run.push(LogKind::Error, Some(label), e.clone()),
                        None => self.run.push(
                            if failed { LogKind::Error } else { LogKind::Step },
                            Some(label),
                            format!("→ {} · {}", tr(&port), human_ms(duration_ms)),
                        ),
                    };
                    // Still running in another branch.
                    if self.run.running.values().any(|b| *b == block) {
                        return;
                    }
                    self.run.status.insert(
                        block,
                        BlockStatus::Done {
                            port,
                            failed,
                            ms: duration_ms,
                            output,
                            error,
                            tests: (passed, failed_tests),
                        },
                    );
                }
                FlowEvent::Log { block, level, message } => {
                    let label = block.map(|b| self.label_of(&b));
                    let kind = match level {
                        merpati_core::script::LogLevel::Error => LogKind::Error,
                        merpati_core::script::LogLevel::Warn => LogKind::Warn,
                        merpati_core::script::LogLevel::Debug => LogKind::Debug,
                        merpati_core::script::LogLevel::Log | merpati_core::script::LogLevel::Info => LogKind::Info,
                    };
                    self.run.push(kind, label, message);
                }
                FlowEvent::Error { block, message } => {
                    let label = block.map(|b| self.label_of(&b));
                    self.run.push(LogKind::Error, label, message);
                }
                FlowEvent::Finished { .. } => {}
            },
            FlowMsg::Done(result) => {
                self.run.cancel = None;
                self.run.running.clear();
                self.run.live.clear();
                self.run.status.retain(|_, s| !matches!(s, BlockStatus::Running { .. }));
                let (kind, head) = if result.cancelled {
                    (LogKind::Warn, tr("Stopped"))
                } else if result.success {
                    (LogKind::Done, tr("Succeeded"))
                } else {
                    (LogKind::Error, tr("Failed"))
                };
                let summary = format!("{} · {}", steps_text(result.steps.len()), human_ms(result.duration_ms));
                self.run.push(kind, None, format!("{head} · {summary}"));
                self.run.result = Some(result);
            }
        }
    }

    fn touch(&mut self) {
        self.last_edit = Some(Instant::now());
        self.diagnostics.clear();
    }
}

// ---------------------------------------------------------------------------
// Block kinds: color, icon, description, summary
// ---------------------------------------------------------------------------

/// The color of a block kind (its icon tile and header tint).
fn kind_color(kind: BlockKind, dark: bool) -> Color32 {
    let p = theme::palette(dark);
    match kind {
        BlockKind::Start | BlockKind::Output => p.ok,
        BlockKind::Request => p.accent,
        BlockKind::Condition => p.warn,
        BlockKind::Loop => theme::method_color("PATCH", dark),
        BlockKind::Parallel | BlockKind::Join => theme::method_color("WS", dark),
        BlockKind::Script => theme::method_color("PUT", dark),
        BlockKind::Ai => theme::method_color("GQL", dark),
        BlockKind::SetVariable => theme::method_color("HEAD", dark),
        BlockKind::Template => theme::method_color("OPTIONS", dark),
        BlockKind::Delay | BlockKind::Log => p.text_muted,
    }
}

fn kind_icon(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Start => icons::PLAY_CIRCLE,
        BlockKind::Request => icons::GLOBE,
        BlockKind::Condition => icons::GIT_BRANCH,
        BlockKind::Loop => icons::ARROWS_CLOCKWISE,
        BlockKind::Parallel => icons::GIT_FORK,
        BlockKind::Join => icons::GIT_MERGE,
        BlockKind::Script => icons::CODE,
        BlockKind::Ai => icons::SPARKLE,
        BlockKind::Delay => icons::HOURGLASS,
        BlockKind::SetVariable => icons::BRACKETS_CURLY,
        BlockKind::Template => icons::TEXT_ALIGN_LEFT,
        BlockKind::Output => icons::EXPORT,
        BlockKind::Log => icons::TERMINAL_WINDOW,
    }
}

fn kind_description(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Start => tr("Where the flow begins"),
        BlockKind::Request => tr("Sends a saved or inline request"),
        BlockKind::Condition => tr("Continues on true or false"),
        BlockKind::Loop => tr("Repeats for each item or a number of times"),
        BlockKind::Parallel => tr("Runs branches at the same time"),
        BlockKind::Join => tr("Waits for every parallel branch"),
        BlockKind::Script => tr("Runs JavaScript"),
        BlockKind::Ai => tr("Asks an AI model"),
        BlockKind::Delay => tr("Waits before continuing"),
        BlockKind::SetVariable => tr("Stores a value in a variable"),
        BlockKind::Template => tr("Builds text or JSON from a template"),
        BlockKind::Output => tr("Names a result of the flow"),
        BlockKind::Log => tr("Writes a message to the run log"),
    }
}

/// The color of a port (its dot and the connections leaving it).
fn port_color(port: &str, p: &Palette, dark: bool) -> Color32 {
    match port {
        "success" | "true" => p.ok,
        "failure" | "false" => p.err,
        "item" => p.info,
        "break" => p.warn,
        PORT_BRANCHES => kind_color(BlockKind::Parallel, dark),
        _ => p.text_muted,
    }
}

/// One line describing the block's configuration.
fn summary(app: &MerpatiApp, flow: &Flow, b: &Block) -> String {
    match &b.config {
        BlockConfig::Start(_) => {
            if flow.inputs.is_empty() {
                tr("Entry point").into()
            } else {
                let names: Vec<&str> = flow.inputs.iter().map(|i| i.name.as_str()).collect();
                trf("inputs: {}", &[&names.join(", ")])
            }
        }
        BlockConfig::Request(c) => match &c.source {
            RequestSource::Saved(r) => request_name(app, r).unwrap_or_else(|| tr("(missing request)").into()),
            RequestSource::Inline { request, .. } => format!("{} {}", request.method, request.url),
        },
        BlockConfig::Condition(c) => match c.mode {
            ConditionMode::Compare if c.op.is_unary() => format!("{} {}", c.left, c.op.label()),
            ConditionMode::Compare => format!("{} {} {}", c.left, c.op.label(), c.right),
            ConditionMode::Expression => c.expression.clone(),
        },
        BlockConfig::Loop(c) => match c.mode {
            LoopMode::ForEach => format!("{} {}", tr("each of"), c.items),
            LoopMode::Count => format!("{} × {}", c.count, tr("times")),
        },
        BlockConfig::Script(c) => c.code.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string(),
        BlockConfig::Ai(c) => {
            if c.model.is_empty() {
                c.prompt.clone()
            } else {
                format!("{} · {}", c.model, c.prompt)
            }
        }
        BlockConfig::Delay(c) => format!("{} ms", c.ms),
        BlockConfig::SetVariable(c) => format!("{} = {}", c.name, c.value),
        BlockConfig::Template(c) => match c.mode {
            TemplateMode::Template => c.template.clone(),
            TemplateMode::Expression => c.expression.clone(),
        },
        BlockConfig::Output(c) => format!("{} = {}", c.name, c.value),
        BlockConfig::Log(c) => c.message.clone(),
        BlockConfig::Parallel(_) => trf("{} branches at once", &[&flow.edges_from(&b.id, PORT_BRANCHES).count()]),
        BlockConfig::Join(_) => trf("waits for {} incoming", &[&flow.edges_to(&b.id).count()]),
    }
}

/// The end of `s` on one line: at most `max` characters, plus "…" when cut.
fn tail(s: &str, max: usize) -> String {
    let one_line = s.trim_end().replace('\n', " ");
    let n = one_line.chars().count();
    if n <= max { one_line } else { format!("…{}", one_line.chars().skip(n - max).collect::<String>()) }
}

fn blocks_text(n: usize) -> String {
    if n == 1 { tr("1 block").to_string() } else { trf("{} blocks", &[&n]) }
}

fn connections_text(n: usize) -> String {
    if n == 1 { tr("1 connection").to_string() } else { trf("{} connections", &[&n]) }
}

fn steps_text(n: usize) -> String {
    if n == 1 { tr("1 step").to_string() } else { trf("{} steps", &[&n]) }
}

pub(crate) fn problems_text(n: usize) -> String {
    if n == 1 { tr("1 problem found").to_string() } else { trf("{} problems found", &[&n]) }
}

fn elapsed(since: Instant) -> String {
    format!("{:.1} s", since.elapsed().as_secs_f64())
}

/// `mm:ss.mmm` since the start of the run.
fn clock(d: Duration) -> String {
    let ms = d.as_millis();
    format!("{:02}:{:02}.{:03}", ms / 60_000, (ms / 1000) % 60, ms % 1000)
}

fn request_name(app: &MerpatiApp, r: &ItemRef) -> Option<String> {
    app.ws
        .data
        .collections
        .iter()
        .filter(|c| r.collection_id.is_empty() || c.id == r.collection_id)
        .find_map(|c| c.find_request(&r.request_id).map(|i| format!("{} / {}", c.name, i.name)))
}

fn reference_key(b: &Block) -> String {
    if b.label.trim().is_empty() { b.id.clone() } else { b.label.trim().to_string() }
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

fn block_height(kind: BlockKind) -> f32 {
    let rows = kind.input_ports().len().max(kind.output_ports().len()).max(1) as f32;
    PORTS_TOP + rows * PORT_ROW + 8.0
}

/// Rounds to the half grid, so moved blocks line up.
fn snap(v: f32) -> f32 {
    let step = GRID / 2.0;
    (v / step).round() * step
}

/// A font size at a zoom level, in half pixels (fewer glyph sizes to cache).
fn fs(size: f32, zoom: f32) -> f32 {
    ((size * zoom * 2.0).round() / 2.0).max(4.0)
}

struct Geo {
    origin: Pos2,
    zoom: f32,
}

impl Geo {
    fn to_screen(&self, p: (f32, f32)) -> Pos2 {
        self.origin + Vec2::new(p.0, p.1) * self.zoom
    }

    fn to_canvas(&self, p: Pos2) -> (f32, f32) {
        let v = (p - self.origin) / self.zoom;
        (v.x, v.y)
    }

    fn block_rect(&self, b: &Block) -> Rect {
        Rect::from_min_size(self.to_screen(b.position), Vec2::new(BLOCK_W, block_height(b.kind())) * self.zoom)
    }

    fn port_y(&self, r: Rect, i: usize) -> f32 {
        r.top() + (PORTS_TOP + PORT_ROW * (i as f32 + 0.5)) * self.zoom
    }

    fn in_port(&self, b: &Block, i: usize) -> Pos2 {
        let r = self.block_rect(b);
        Pos2::new(r.left(), self.port_y(r, i))
    }

    fn out_port(&self, b: &Block, i: usize) -> Pos2 {
        let r = self.block_rect(b);
        Pos2::new(r.right(), self.port_y(r, i))
    }
}

fn bezier(a: Pos2, b: Pos2, zoom: f32) -> [Pos2; 4] {
    let dx = ((b.x - a.x).abs() * 0.5).max(60.0 * zoom);
    [a, Pos2::new(a.x + dx, a.y), Pos2::new(b.x - dx, b.y), b]
}

fn bezier_at(pts: &[Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    (pts[0].to_vec2() * (u * u * u)
        + pts[1].to_vec2() * (3.0 * u * u * t)
        + pts[2].to_vec2() * (3.0 * u * t * t)
        + pts[3].to_vec2() * (t * t * t))
        .to_pos2()
}

/// Distance from `p` to the curve (measured on 32 straight pieces).
fn bezier_distance(pts: &[Pos2; 4], p: Pos2) -> f32 {
    let mut prev = pts[0];
    let mut best = f32::INFINITY;
    for i in 1..=32 {
        let q = bezier_at(pts, i as f32 / 32.0);
        let ab = q - prev;
        let t = ((p - prev).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
        best = best.min((prev + ab * t).distance(p));
        prev = q;
    }
    best
}

/// Text on one line, cut with "…" at `max_w`.
fn one_line(painter: &egui::Painter, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::single_section(text.replace('\n', " "), TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(1.0));
    painter.layout_job(job)
}

/// A small spinning arc.
fn spinner(ui: &Ui, painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    let t = ui.input(|i| i.time) as f32;
    let start = t * 5.0;
    let pts: Vec<Pos2> = (0..=24)
        .map(|i| {
            let a = start + i as f32 / 24.0 * 4.2;
            center + radius * vec2(a.cos(), a.sin())
        })
        .collect();
    painter.add(Shape::line(pts, Stroke::new((radius * 0.32).max(1.2), color)));
    ui.ctx().request_repaint_after(Duration::from_millis(30));
}

// ---------------------------------------------------------------------------
// Tab
// ---------------------------------------------------------------------------

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut FlowTab) {
    if std::mem::take(&mut tab.auto_run) && !tab.is_running() {
        start_run(app, tab_id, tab);
    }
    let width = ui.available_width();
    let inspector_open = tab.inspector.unwrap_or(width >= 820.0 || tab.selected.is_some());
    header(app, ui, tab_id, tab, inspector_open);
    ui.add_space(10.0);
    if inspector_open {
        let max_w = (width * 0.45).max(260.0);
        egui::Panel::right(egui::Id::unique(("flow-inspector", tab_id)))
            .resizable(true)
            .default_size(330.0_f32.min(max_w))
            .min_size(260.0)
            .max_size(max_w)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("flow-inspector-scroll")
                    .auto_shrink([false; 2])
                    .show(ui, |ui| inspector(app, ui, tab));
            });
    }
    let height = ui.available_height();
    egui::Panel::bottom(egui::Id::unique(("flow-log", tab_id)))
        .resizable(true)
        .show_separator_line(false)
        .default_size((height * 0.24).clamp(110.0, 190.0))
        .min_size(90.0)
        .max_size((height * 0.6).max(120.0))
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 0, top: 10, bottom: 0 }))
        .show(ui, |ui| run_log(ui, tab));
    canvas(app, ui, tab);
    autosave(app, ui, tab);
}

fn autosave(app: &mut MerpatiApp, ui: &Ui, tab: &mut FlowTab) {
    let Some(t) = tab.last_edit else { return };
    let wait = Duration::from_millis(700).saturating_sub(t.elapsed());
    if !wait.is_zero() {
        ui.ctx().request_repaint_after(wait);
        return;
    }
    tab.last_edit = None;
    if !tab.dirty() && tab.path.is_some() {
        return;
    }
    match merpati_flows::storage::save(app.ws.root(), &tab.flow) {
        Ok(path) => {
            tab.path = Some(path);
            tab.saved = tab.flow.clone();
            tab.error = None;
            app.sidebar.flows_dirty = true;
        }
        Err(e) => tab.error = Some(e.to_string()),
    }
}

fn header(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut FlowTab, inspector_open: bool) {
    let p = theme::pal(ui);
    let narrow = ui.available_width() < 780.0;
    ui.horizontal(|ui| {
        ui.set_height(30.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if tab.is_running() {
                if kit::secondary(ui, Some(icons::STOP), tr("Stop")).on_hover_text(tr("Stop the run")).clicked() {
                    tab.stop();
                }
            } else if kit::primary_icon(ui, icons::PLAY, tr("Run")).on_hover_text(tr("Run the flow")).clicked() {
                start_run(app, tab_id, tab);
            }
            let add = if narrow {
                kit::icon_button(ui, icons::PLUS, tr("Add block"))
            } else {
                kit::secondary(ui, Some(icons::PLUS), tr("Add block"))
            };
            egui::Popup::menu(&add).width(240.0).show(|ui| block_menu(ui, tab, None));
            let validate = if narrow {
                kit::icon_button(ui, icons::SHIELD_CHECK, tr("Validate"))
            } else {
                kit::secondary(ui, Some(icons::SHIELD_CHECK), tr("Validate"))
            };
            if validate.on_hover_text(tr("Check the flow for problems")).clicked() {
                validate_flow(app, tab);
            }
            let fit = if narrow {
                kit::icon_button(ui, icons::ARROWS_IN, tr("Fit"))
            } else {
                kit::secondary(ui, Some(icons::ARROWS_IN), tr("Fit"))
            };
            if fit.on_hover_text(tr("Show all blocks")).clicked() {
                tab.fit = true;
            }
            kit::vdivider(ui, 20.0);
            if kit::icon_toggle(ui, icons::SLIDERS_HORIZONTAL, tr("Inspector"), inspector_open).clicked() {
                tab.inspector = Some(!inspector_open);
            }
            if !narrow {
                let pill = kit::pill(ui, &format!("{:.0}%", tab.zoom * 100.0), p.text_muted);
                let reset = ui
                    .interact(pill.rect, ui.scope_id().with("flow-zoom-reset"), Sense::click())
                    .on_hover_text(tr("Zoom level (click for 100 %)"));
                widgets::a11y_button(&reset, tr("Reset zoom"));
                if reset.clicked() {
                    let anchor = tab.view / 2.0;
                    set_zoom(tab, 1.0, anchor);
                }
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(icons::FLOW_ARROW).font(theme::icon_font(16.0)).color(p.text_muted));
                if !narrow {
                    ui.label(RichText::new(tr("Flows")).color(p.text_muted));
                    ui.label(RichText::new(icons::CARET_RIGHT).font(theme::icon_font(11.0)).color(p.text_faint));
                }
                let title_w = (ui.available_width() - 96.0).max(60.0);
                match &mut tab.renaming {
                    Some(buf) => {
                        let r = ui.add(
                            egui::TextEdit::singleline(buf)
                                .font(theme::semibold(15.0))
                                .desired_width(title_w.min(320.0)),
                        );
                        r.request_focus();
                        let escape = ui.input(|i| i.key_pressed(Key::Escape));
                        if r.lost_focus() || escape {
                            if !escape && !buf.trim().is_empty() && buf.trim() != tab.flow.name {
                                tab.flow.name = buf.trim().to_string();
                                tab.touch();
                            }
                            tab.renaming = None;
                        }
                    }
                    None => {
                        let r = ui
                            .allocate_ui_with_layout(vec2(title_w, 30.0), Layout::left_to_right(Align::Center), |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&tab.flow.name).font(theme::semibold(15.0)).color(p.text),
                                    )
                                    .sense(Sense::click())
                                    .selectable(false)
                                    .truncate(),
                                )
                            })
                            .inner;
                        if r.on_hover_text(tr("Double-click to rename")).double_clicked() {
                            tab.renaming = Some(tab.flow.name.clone());
                        }
                    }
                }
                save_pill(ui, tab);
            });
        });
    });
}

fn save_pill(ui: &mut Ui, tab: &FlowTab) {
    let p = theme::pal(ui);
    if let Some(e) = &tab.error {
        kit::pill(ui, &format!("{} {}", icons::WARNING_CIRCLE, tr("Not saved")), p.err).on_hover_text(e);
    } else if tab.dirty() || tab.last_edit.is_some() {
        kit::pill(ui, &format!("{} {}", icons::CLOUD_ARROW_UP, tr("Saving…")), p.text_muted);
    } else {
        let where_ = tab.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        kit::pill(ui, &format!("{} {}", icons::CHECK, tr("Saved")), p.text_muted).on_hover_text(where_);
    }
}

/// Zooms to `zoom`, keeping the canvas point under `anchor` (relative to
/// the canvas' top left corner) in place.
fn set_zoom(tab: &mut FlowTab, zoom: f32, anchor: Vec2) {
    tab.moved_view = true;
    let old = tab.zoom;
    tab.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
    let before = (anchor - tab.pan) / old;
    tab.pan = anchor - before * tab.zoom;
}

fn validate_flow(app: &mut MerpatiApp, tab: &mut FlowTab) {
    tab.diagnostics = merpati_flows::validate(&tab.flow, Some(&app.ws.data.collections));
    if tab.diagnostics.is_empty() {
        app.toast(tr("No problems found"), false);
    } else {
        // The flow settings list the problems.
        tab.selected = None;
        tab.inspector = Some(true);
        app.toast(problems_text(tab.diagnostics.len()), true);
    }
}

/// The "Add block" menu: block kinds by group, with their icons.
fn block_menu(ui: &mut Ui, tab: &mut FlowTab, at: Option<(f32, f32)>) {
    let p = theme::pal(ui);
    ui.set_min_width(230.0);
    let groups: [(&str, &[BlockKind]); 3] = [
        (tr("Actions"), &[BlockKind::Request, BlockKind::Script, BlockKind::Ai]),
        (tr("Logic"), &[BlockKind::Condition, BlockKind::Loop, BlockKind::Parallel, BlockKind::Join, BlockKind::Delay]),
        (tr("Data"), &[BlockKind::SetVariable, BlockKind::Template, BlockKind::Output, BlockKind::Log]),
    ];
    if tab.flow.start().is_none() && block_menu_item(ui, BlockKind::Start).clicked() {
        add_block(tab, BlockKind::Start, at);
        ui.close();
    }
    for (i, (title, kinds)) in groups.iter().enumerate() {
        if i > 0 {
            ui.add_space(2.0);
            ui.separator();
        }
        ui.add_space(2.0);
        ui.label(RichText::new(*title).font(theme::medium(11.5)).color(p.text_faint));
        for kind in kinds.iter() {
            if block_menu_item(ui, *kind).clicked() {
                add_block(tab, *kind, at);
                ui.close();
            }
        }
    }
}

fn block_menu_item(ui: &mut Ui, kind: BlockKind) -> Response {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let w = ui.available_width().max(220.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    widgets::a11y_button(&resp, tr(kind.label()));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if resp.hovered() {
            painter.rect_filled(rect, CornerRadius::same(RADIUS), p.hover);
        }
        let tile = Rect::from_center_size(Pos2::new(rect.left() + 17.0, rect.center().y), Vec2::splat(20.0));
        painter.rect_filled(tile, CornerRadius::same(5), kind_color(kind, dark));
        painter.text(tile.center(), Align2::CENTER_CENTER, kind_icon(kind), theme::icon_font(12.5), p.surface);
        painter.text(
            Pos2::new(tile.right() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            tr(kind.label()),
            theme::medium(13.0),
            p.text,
        );
    }
    resp.on_hover_text(kind_description(kind))
}

/// Adds a block at `at` (canvas units) or in the middle of the view, and
/// selects it.
fn add_block(tab: &mut FlowTab, kind: BlockKind, at: Option<(f32, f32)>) -> String {
    let (x, y) = at.unwrap_or_else(|| {
        let c = (tab.view / 2.0 - tab.pan) / tab.zoom;
        (c.x - BLOCK_W / 2.0, c.y - block_height(kind) / 2.0)
    });
    let mut pos = (snap(x), snap(y));
    while tab.flow.blocks.iter().any(|b| (b.position.0 - pos.0).abs() < GRID && (b.position.1 - pos.1).abs() < GRID) {
        pos = (pos.0 + 2.0 * GRID, pos.1 + 2.0 * GRID);
    }
    let id = tab.flow.add_block(Block::new(kind.default_config(), "", pos));
    tab.selected = Some(Selection::Block(id.clone()));
    tab.touch();
    id
}

/// Adds a block to the right of `from` and connects `port` to it.
fn add_after(tab: &mut FlowTab, from: &str, port: &str, kind: BlockKind) {
    let Some(b) = tab.flow.block(from) else { return };
    let pos = (b.position.0 + BLOCK_W + 60.0, b.position.1);
    let id = add_block(tab, kind, Some(pos));
    tab.flow.edges.push(Edge::new(from, port, &id, "in"));
}

fn duplicate_block(tab: &mut FlowTab, id: &str) {
    let Some(b) = tab.flow.block(id).cloned() else { return };
    if b.kind() == BlockKind::Start {
        return;
    }
    let copy =
        Block::new(b.config.clone(), trf("{} copy", &[&b.display_name()]), (b.position.0 + 40.0, b.position.1 + 40.0));
    let nid = tab.flow.add_block(copy);
    tab.selected = Some(Selection::Block(nid));
    tab.touch();
}

fn start_run(app: &mut MerpatiApp, tab_id: u64, tab: &mut FlowTab) {
    let diags = merpati_flows::validate(&tab.flow, Some(&app.ws.data.collections));
    if diags.iter().any(|d| d.severity == Severity::Error) {
        tab.diagnostics = diags;
        tab.selected = None;
        tab.inspector = Some(true);
        app.toast(tr("Fix the problems listed in the inspector before running"), true);
        return;
    }
    tab.diagnostics = diags;
    let mut opts = FlowRunOptions::default();
    for input in &tab.flow.inputs {
        if let Some(text) = tab.inputs.get(&input.name).filter(|t| !t.trim().is_empty()) {
            let v = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()));
            opts.inputs.insert(input.name.clone(), v);
        }
    }
    let env = FlowEnv {
        services: app.services(),
        collections: app.ws.data.collections.clone(),
        base_dir: Some(app.ws.root().to_path_buf()),
    };
    let mut vars = app.ws.scopes_for(None);
    let flow = tab.flow.clone();
    let cancel = CancellationToken::new();
    tab.run = RunState { cancel: Some(cancel.clone()), started: Some(Instant::now()), ..Default::default() };
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    app.rt.spawn(async move {
        let events = tx.clone();
        let repaint = ctx.clone();
        let mut sink = move |e: FlowEvent| {
            let _ = events.send(Msg::Flow { tab: tab_id, event: FlowMsg::Event(e) });
            repaint.request_repaint();
        };
        let result = merpati_flows::run_flow(&env, &flow, &mut vars, opts, &mut sink, &cancel).await;
        let _ = tx.send(Msg::Flow { tab: tab_id, event: FlowMsg::Done(Box::new(result)) });
        ctx.request_repaint();
    });
}

// ---------------------------------------------------------------------------
// Canvas
// ---------------------------------------------------------------------------

/// What a block card shows.
struct CardLook<'a> {
    block: &'a Block,
    rect: Rect,
    zoom: f32,
    line: &'a str,
    selected: bool,
    hovered: bool,
    status: Option<&'a BlockStatus>,
    diag: Option<Severity>,
}

fn canvas(app: &MerpatiApp, ui: &mut Ui, tab: &mut FlowTab) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let rect = ui.available_rect_before_wrap();
    if !tab.moved_view && tab.view != Vec2::ZERO && (tab.view - rect.size()).length() > 1.0 {
        tab.fit = true;
    }
    tab.view = rect.size();
    let bg = ui.interact(rect, ui.scope_id().with("flow-canvas"), Sense::click_and_drag());
    widgets::a11y_name(&bg, tr("Flow canvas"));
    let painter = ui.painter_at(rect);
    let radius = CornerRadius::same(8);
    painter.rect_filled(rect, radius, p.chrome);

    // Zoom around the pointer with pinch or ⌘ + scroll; scroll or drag the
    // empty canvas to pan.
    if ui.rect_contains_pointer(rect) && !egui::Popup::is_any_open(ui.ctx()) {
        let zoom = ui.input(|i| i.zoom_delta());
        if (zoom - 1.0).abs() > f32::EPSILON {
            if let Some(ptr) = ui.input(|i| i.pointer.hover_pos()) {
                let z = tab.zoom * zoom;
                set_zoom(tab, z, ptr - rect.min);
            }
        } else {
            let scroll = ui.input(|i| i.smooth_scroll_delta);
            if scroll != Vec2::ZERO {
                tab.pan += scroll;
                tab.moved_view = true;
            }
        }
    }
    if bg.dragged() && tab.connecting.is_none() {
        tab.pan += bg.drag_delta();
        tab.moved_view = true;
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    }
    if std::mem::take(&mut tab.fit) {
        fit_view(tab, rect.size());
    }
    let geo = Geo { origin: rect.min + tab.pan, zoom: tab.zoom };
    let z = tab.zoom;

    // Dot grid.
    let step = GRID * z;
    if step >= 8.0 {
        let dot = if dark { p.border_strong } else { p.border_strong.gamma_multiply(0.95) };
        let r = (1.0 * z.sqrt()).clamp(0.7, 1.3);
        let ox = (geo.origin.x - rect.left()).rem_euclid(step);
        let oy = (geo.origin.y - rect.top()).rem_euclid(step);
        let mut y = rect.top() + oy;
        while y < rect.bottom() {
            let mut x = rect.left() + ox;
            while x < rect.right() {
                painter.circle_filled(Pos2::new(x, y), r, dot);
                x += step;
            }
            y += step;
        }
    }

    let pointer = ui.input(|i| i.pointer.hover_pos()).filter(|q| rect.contains(*q));
    let worst = |m: &mut HashMap<_, Severity>, k, s: Severity| {
        let e = m.entry(k).or_insert(s);
        if s == Severity::Error {
            *e = s;
        }
    };
    let mut diag_blocks: HashMap<String, Severity> = HashMap::new();
    let mut diag_edges: HashMap<usize, Severity> = HashMap::new();
    for d in &tab.diagnostics {
        if let Some(b) = &d.block {
            worst(&mut diag_blocks, b.clone(), d.severity);
        }
        if let Some(e) = d.edge {
            let e2 = &mut diag_edges;
            let entry = e2.entry(e).or_insert(d.severity);
            if d.severity == Severity::Error {
                *entry = d.severity;
            }
        }
    }
    let over_block = pointer.is_some_and(|q| tab.flow.blocks.iter().any(|b| geo.block_rect(b).expand(6.0).contains(q)));

    // Edges.
    let edge_pts: Vec<Option<[Pos2; 4]>> = tab
        .flow
        .edges
        .iter()
        .map(|e| {
            let (from, to) = (tab.flow.block(&e.from_block)?, tab.flow.block(&e.to_block)?);
            let oi = from.kind().output_ports().iter().position(|p| *p == e.from_port).unwrap_or(0);
            let ii = to.kind().input_ports().iter().position(|p| *p == e.to_port).unwrap_or(0);
            Some(bezier(geo.out_port(from, oi), geo.in_port(to, ii), z))
        })
        .collect();
    let hover_edge = match pointer {
        Some(q) if !over_block && tab.connecting.is_none() => edge_pts
            .iter()
            .enumerate()
            .filter_map(|(i, pts)| pts.map(|pts| (i, bezier_distance(&pts, q))))
            .filter(|(_, d)| *d < 7.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i),
        _ => None,
    };
    if hover_edge.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    for (i, e) in tab.flow.edges.iter().enumerate() {
        let Some(pts) = edge_pts[i] else { continue };
        let selected = tab.selected == Some(Selection::Edge(i));
        let hovered = hover_edge == Some(i);
        let failure = matches!(e.from_port.as_str(), "failure" | "false");
        let taken =
            matches!(tab.run.status.get(&e.from_block), Some(BlockStatus::Done { port, .. }) if *port == e.from_port);
        let (color, width) = if selected {
            (p.accent, 2.5)
        } else if let Some(s) = diag_edges.get(&i) {
            (if *s == Severity::Error { p.err } else { p.warn }, 2.0)
        } else if hovered {
            (theme::soft(p.accent, 0.75), 2.5)
        } else if taken {
            (if failure { p.err } else { p.ok }, 2.5)
        } else if failure {
            (theme::soft(p.err, 0.7), 1.75)
        } else {
            (p.text_faint, 1.75)
        };
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            pts,
            false,
            Color32::TRANSPARENT,
            Stroke::new(width * z.sqrt().clamp(0.7, 1.4), color),
        ));
    }

    // Blocks.
    let mut moved = false;
    let mut snap_block: Option<String> = None;
    let mut new_edge: Option<Edge> = None;
    let mut delete_block: Option<String> = None;
    let mut duplicate: Option<String> = None;
    let mut disconnect: Option<String> = None;
    let released = ui.input(|i| i.pointer.any_released());
    let ids: Vec<String> = tab.flow.blocks.iter().map(|b| b.id.clone()).collect();
    for id in &ids {
        let Some(b) = tab.flow.block(id).cloned() else { continue };
        let kind = b.kind();
        let r = geo.block_rect(&b);
        if !r.expand(24.0).intersects(rect) {
            continue;
        }
        let selected = tab.selected == Some(Selection::Block(b.id.clone()));
        let card = ui.interact(r, ui.scope_id().with(("flow-block", &b.id)), Sense::click_and_drag());
        widgets::a11y_button(&card, &format!("{} {}", tr(kind.label()), b.display_name()));
        if card.clicked() || card.drag_started() {
            tab.selected = Some(Selection::Block(b.id.clone()));
        }
        if card.dragged()
            && let Some(blk) = tab.flow.block_mut(&b.id)
        {
            let d = card.drag_delta() / z;
            blk.position.0 += d.x;
            blk.position.1 += d.y;
            moved = true;
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        } else if card.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
        if card.drag_stopped() {
            snap_block = Some(b.id.clone());
        }
        card.context_menu(|ui| {
            if kind != BlockKind::Start && ui.button(kit::with_icon(icons::COPY, tr("Duplicate"))).clicked() {
                duplicate = Some(b.id.clone());
                ui.close();
            }
            if ui.button(kit::with_icon(icons::LINK_SIMPLE, tr("Remove connections"))).clicked() {
                disconnect = Some(b.id.clone());
                ui.close();
            }
            if kind != BlockKind::Start {
                ui.separator();
                let del = RichText::new(kit::with_icon(icons::TRASH, tr("Delete"))).color(p.err);
                if ui.button(del).clicked() {
                    delete_block = Some(b.id.clone());
                    ui.close();
                }
            }
        });

        // A streaming AI block shows the end of its answer as it grows.
        let line = match tab.run.live_text(&b.id) {
            Some(text) => tail(text, 60),
            None => summary(app, &tab.flow, &b),
        };
        let status = tab.run.status.get(&b.id);
        if matches!(status, Some(BlockStatus::Running { .. })) {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let look = CardLook {
            block: &b,
            rect: r,
            zoom: z,
            line: &line,
            selected,
            hovered: card.hovered() || card.dragged(),
            status,
            diag: diag_blocks.get(&b.id).copied(),
        };
        draw_card(ui, &painter, &look);

        // Input ports.
        let detail = z >= DETAIL_ZOOM;
        let label_font = theme::medium(fs(11.0, z));
        for (i, port) in kind.input_ports().iter().enumerate() {
            let c = geo.in_port(&b, i);
            let connected = tab.flow.edges.iter().any(|e| e.to_block == b.id && e.to_port == *port);
            let hover = pointer.is_some_and(|q| q.distance(c) < 12.0 * z.max(0.8));
            let target = tab.connecting.as_ref().is_some_and(|(from, _)| *from != b.id);
            if target {
                painter.circle_filled(c, 10.0 * z, theme::soft(p.accent, if hover { 0.35 } else { 0.14 }));
            }
            let color = if target && hover { p.accent } else { port_color(port, &p, dark) };
            draw_port(&painter, &p, c, z, color, connected || (target && hover), hover && target);
            if detail {
                painter.text(c + vec2(11.0 * z, 0.0), Align2::LEFT_CENTER, tr(port), label_font.clone(), p.text_muted);
            }
            if let Some((from, from_port)) = &tab.connecting
                && hover
                && released
                && *from != b.id
            {
                new_edge = Some(Edge::new(from, from_port, &b.id, port));
            }
        }
        // Output ports: drag from them to connect.
        for (i, port) in kind.output_ports().iter().enumerate() {
            let c = geo.out_port(&b, i);
            let taken = matches!(status, Some(BlockStatus::Done { port: taken, .. }) if taken == port);
            let connected = tab.flow.edges.iter().any(|e| e.from_block == b.id && e.from_port == *port);
            let pr = ui.interact(
                Rect::from_center_size(c, Vec2::splat(18.0 * z.max(0.8))),
                ui.scope_id().with(("flow-port", &b.id, port)),
                Sense::drag(),
            );
            widgets::a11y_name(&pr, &format!("{} {} · {}", tr("Connect from"), b.display_name(), tr(port)));
            let active = tab.connecting.as_ref().is_some_and(|(f, fp)| *f == b.id && fp == port);
            let color = port_color(port, &p, dark);
            if taken {
                painter.circle_filled(c, 9.0 * z, theme::soft(color, 0.22));
            }
            draw_port(&painter, &p, c, z, color, connected || taken || active, pr.hovered() || active);
            if detail {
                painter.text(c - vec2(11.0 * z, 0.0), Align2::RIGHT_CENTER, tr(port), label_font.clone(), p.text_muted);
            }
            if pr.drag_started() {
                tab.connecting = Some((b.id.clone(), port.to_string()));
            }
            if pr.hovered() || pr.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
            }
        }
    }

    // The connection being drawn.
    if let (Some((from, port)), Some(q)) = (&tab.connecting, ui.input(|i| i.pointer.hover_pos()))
        && let Some(b) = tab.flow.block(from)
    {
        let oi = b.kind().output_ports().iter().position(|x| x == port).unwrap_or(0);
        let a = geo.out_port(b, oi);
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            bezier(a, q, z),
            false,
            Color32::TRANSPARENT,
            Stroke::new(2.0, p.accent),
        ));
        painter.circle_filled(q, 4.0, p.accent);
        ui.ctx().request_repaint();
    }
    if released {
        tab.connecting = None;
    }
    if let Some(e) = new_edge
        && !tab.flow.edges.contains(&e)
    {
        tab.flow.edges.push(e);
        tab.selected = Some(Selection::Edge(tab.flow.edges.len() - 1));
        tab.touch();
    }

    // The selected connection gets a delete button at its middle.
    let mut remove_edge = None;
    if let Some(Selection::Edge(i)) = tab.selected
        && let Some(Some(pts)) = edge_pts.get(i)
    {
        let mid = bezier_at(pts, 0.5);
        if rect.contains(mid) {
            let br = Rect::from_center_size(mid, Vec2::splat(22.0));
            let x = ui
                .interact(br, ui.scope_id().with("flow-edge-delete"), Sense::click())
                .on_hover_text(tr("Delete connection"));
            widgets::a11y_button(&x, tr("Delete connection"));
            painter.circle(mid, 11.0, p.surface, Stroke::new(1.0, if x.hovered() { p.err } else { p.border_strong }));
            painter.text(
                mid,
                Align2::CENTER_CENTER,
                icons::X,
                theme::icon_font(12.0),
                if x.hovered() { p.err } else { p.text_muted },
            );
            if x.clicked() {
                remove_edge = Some(i);
            }
        }
    }

    // Clicks on the empty canvas: select a connection or clear the selection.
    if bg.clicked() {
        tab.selected = hover_edge.map(Selection::Edge);
    }
    if bg.secondary_clicked() {
        tab.menu_at = ui.input(|i| i.pointer.interact_pos()).map(|q| geo.to_canvas(q));
        if let Some(i) = hover_edge {
            tab.selected = Some(Selection::Edge(i));
        }
    }
    bg.context_menu(|ui| {
        if let Some(Selection::Edge(i)) = tab.selected {
            let del = RichText::new(kit::with_icon(icons::TRASH, tr("Delete connection"))).color(p.err);
            if ui.button(del).clicked() {
                remove_edge = Some(i);
                ui.close();
            }
            ui.separator();
        }
        ui.menu_button(kit::with_icon(icons::PLUS, tr("Add block here")), |ui| {
            let at = tab.menu_at;
            block_menu(ui, tab, at);
        });
        if ui.button(kit::with_icon(icons::ARROWS_IN, tr("Fit"))).clicked() {
            tab.fit = true;
            ui.close();
        }
    });

    // Keyboard: delete, duplicate, deselect and nudge the selection.
    if !ui.ctx().egui_wants_keyboard_input() && app.dialog.is_none() && !egui::Popup::is_any_open(ui.ctx()) {
        let (del, esc, dup, nudge) = ui.input(|i| {
            let step = if i.modifiers.shift { GRID * 2.0 } else { GRID / 2.0 };
            let mut d = Vec2::ZERO;
            for (key, v) in [
                (Key::ArrowLeft, vec2(-step, 0.0)),
                (Key::ArrowRight, vec2(step, 0.0)),
                (Key::ArrowUp, vec2(0.0, -step)),
                (Key::ArrowDown, vec2(0.0, step)),
            ] {
                if i.key_pressed(key) {
                    d += v;
                }
            }
            (
                i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace),
                i.key_pressed(Key::Escape),
                i.modifiers.command && i.key_pressed(Key::D),
                d,
            )
        });
        match tab.selected.clone() {
            Some(Selection::Block(id)) => {
                let start = tab.flow.block(&id).is_some_and(|b| b.kind() == BlockKind::Start);
                if del && !start {
                    delete_block = Some(id.clone());
                }
                if dup {
                    duplicate = Some(id.clone());
                }
                if nudge != Vec2::ZERO
                    && let Some(b) = tab.flow.block_mut(&id)
                {
                    b.position = (snap(b.position.0 + nudge.x), snap(b.position.1 + nudge.y));
                    moved = true;
                }
            }
            Some(Selection::Edge(i)) if del => remove_edge = Some(i),
            _ => {}
        }
        if esc {
            tab.connecting = None;
            tab.selected = None;
        }
    }
    if let Some(i) = remove_edge.filter(|i| *i < tab.flow.edges.len()) {
        tab.flow.edges.remove(i);
        tab.selected = None;
        tab.touch();
    }
    if let Some(id) = delete_block {
        tab.flow.remove_block(&id);
        tab.selected = None;
        tab.touch();
    }
    if let Some(id) = disconnect {
        tab.flow.edges.retain(|e| e.from_block != id && e.to_block != id);
        tab.selected = Some(Selection::Block(id));
        tab.touch();
    }
    if let Some(id) = duplicate {
        duplicate_block(tab, &id);
    }
    if let Some(id) = snap_block
        && let Some(b) = tab.flow.block_mut(&id)
    {
        b.position = (snap(b.position.0), snap(b.position.1));
        moved = true;
    }
    if moved {
        tab.touch();
    }

    zoom_controls(ui, tab, rect);
    if tab.flow.blocks.len() <= 1 {
        empty_canvas(ui, tab, rect);
    }
    painter.rect_stroke(rect, radius, Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.allocate_rect(rect, Sense::hover());
}

fn draw_port(painter: &egui::Painter, p: &Palette, c: Pos2, z: f32, color: Color32, filled: bool, hovered: bool) {
    let r = if hovered { 6.0 } else { 4.5 } * z.clamp(0.75, 1.5);
    if hovered {
        painter.circle_filled(c, r + 4.0 * z, theme::soft(color, 0.2));
    }
    painter.circle(c, r, if filled { color } else { p.surface }, Stroke::new(1.75 * z.clamp(0.75, 1.3), color));
}

fn draw_card(ui: &Ui, painter: &egui::Painter, look: &CardLook) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let z = look.zoom;
    let r = look.rect;
    let kind = look.block.kind();
    let color = kind_color(kind, dark);
    let rad = (8.0 * z).round().clamp(3.0, 16.0) as u8;
    let cr = CornerRadius::same(rad);
    let shadow = egui::Shadow {
        offset: [0, (2.0 * z).round() as i8],
        blur: (10.0 * z).round() as u8,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 90 } else { 18 }),
    };
    painter.add(shadow.as_shape(r, cr));
    if look.selected {
        painter.rect_filled(r.expand(5.0), CornerRadius::same(rad + 5), theme::soft(p.accent, 0.12));
        painter.rect_stroke(
            r.expand(2.5),
            CornerRadius::same(rad + 2),
            Stroke::new(2.0, p.accent),
            StrokeKind::Outside,
        );
    }
    painter.rect_filled(r, cr, p.surface);
    let head = Rect::from_min_size(r.min, vec2(r.width(), HEAD_H * z));
    painter.rect_filled(
        head,
        CornerRadius { nw: rad, ne: rad, sw: 0, se: 0 },
        theme::soft(color, if dark { 0.13 } else { 0.08 }),
    );

    let t = 20.0 * z;
    let tile = Rect::from_min_size(Pos2::new(r.left() + 8.0 * z, head.center().y - t / 2.0), Vec2::splat(t));
    painter.rect_filled(tile, CornerRadius::same((5.0 * z).round() as u8), color);
    painter.text(tile.center(), Align2::CENTER_CENTER, kind_icon(kind), theme::icon_font(fs(12.0, z)), p.surface);

    let right = r.right() - 8.0 * z;
    let badge_left = draw_badge(ui, painter, look, Pos2::new(right, head.center().y)).unwrap_or(right + 6.0 * z);
    let tx = tile.right() + 8.0 * z;
    let g = one_line(painter, look.block.display_name(), theme::medium(fs(13.0, z)), p.text, badge_left - 6.0 * z - tx);
    painter.galley(Pos2::new(tx, head.center().y - g.size().y / 2.0), g, p.text);

    if z >= DETAIL_ZOOM {
        let text = if look.line.trim().is_empty() { "—" } else { look.line };
        let g = one_line(painter, text, theme::mono(fs(11.0, z)), p.text_muted, r.width() - 20.0 * z);
        let y = head.bottom() + (PORTS_TOP - HEAD_H - 4.0) * z / 2.0;
        painter.galley(Pos2::new(r.left() + 10.0 * z, y - g.size().y / 2.0), g, p.text_muted);
    }
    let dy = r.top() + (PORTS_TOP - 4.0) * z;
    painter.hline((r.left() + 1.0)..=(r.right() - 1.0), dy, Stroke::new(1.0, p.border));

    let border = match (look.status, look.diag) {
        (Some(BlockStatus::Running { .. }), _) => Stroke::new(1.5, p.accent),
        (Some(BlockStatus::Done { failed: true, .. }), _) => Stroke::new(1.5, p.err),
        (None, Some(Severity::Error)) => Stroke::new(1.5, p.err),
        (None, Some(Severity::Warning)) => Stroke::new(1.5, p.warn),
        _ if look.selected => Stroke::new(1.0, p.accent),
        _ if look.hovered => Stroke::new(1.0, p.text_faint),
        _ => Stroke::new(1.0, if dark { p.border_strong } else { p.border }),
    };
    painter.rect_stroke(r, cr, border, StrokeKind::Inside);
}

/// The run (or problem) badge at the right of a card's header. Returns its
/// left edge.
fn draw_badge(ui: &Ui, painter: &egui::Painter, look: &CardLook, anchor: Pos2) -> Option<f32> {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let z = look.zoom;
    let (icon, text, color) = match (look.status, look.diag) {
        (Some(BlockStatus::Running { since }), _) => (None, elapsed(*since), p.accent),
        (Some(BlockStatus::Done { failed: true, ms, .. }), _) => (Some(icons::X), human_ms(*ms), p.err),
        (Some(BlockStatus::Done { ms, tests, .. }), _) => {
            let tests =
                if tests.0 + tests.1 > 0 { format!(" · {}/{}", tests.0, tests.0 + tests.1) } else { String::new() };
            (Some(icons::CHECK), format!("{}{tests}", human_ms(*ms)), p.ok)
        }
        (None, Some(Severity::Error)) => (Some(icons::WARNING_CIRCLE), String::new(), p.err),
        (None, Some(Severity::Warning)) => (Some(icons::WARNING), String::new(), p.warn),
        _ => return None,
    };
    let text = if z < DETAIL_ZOOM { String::new() } else { text };
    let g = painter.layout_no_wrap(text, theme::medium(fs(11.0, z)), color);
    let icon_w = 11.0 * z;
    let gap = if g.size().x > 0.0 { 4.0 * z } else { 0.0 };
    let pad = 6.0 * z;
    let h = 18.0 * z;
    let w = pad * 2.0 + icon_w + gap + g.size().x;
    let rect = Rect::from_min_size(Pos2::new(anchor.x - w, anchor.y - h / 2.0), vec2(w, h));
    painter.rect_filled(rect, CornerRadius::same((h / 2.0) as u8), theme::soft(color, if dark { 0.2 } else { 0.13 }));
    let ic = Pos2::new(rect.left() + pad + icon_w / 2.0, rect.center().y);
    match icon {
        Some(i) => {
            painter.text(ic, Align2::CENTER_CENTER, i, theme::icon_font(fs(11.5, z)), color);
        }
        None => spinner(ui, painter, ic, icon_w * 0.42, color),
    }
    painter.galley(Pos2::new(ic.x + icon_w / 2.0 + gap, rect.center().y - g.size().y / 2.0), g, color);
    Some(rect.left())
}

/// Zoom out / in buttons at the bottom right of the canvas.
fn zoom_controls(ui: &mut Ui, tab: &mut FlowTab, rect: Rect) {
    let p = theme::pal(ui);
    let size = vec2(62.0, 32.0);
    let r = Rect::from_min_size(rect.right_bottom() - size - vec2(12.0, 12.0), size);
    if !rect.contains_rect(r) {
        return;
    }
    let painter = ui.painter();
    painter.add(
        egui::Shadow {
            offset: [0, 2],
            blur: 8,
            spread: 0,
            color: Color32::from_black_alpha(if ui.visuals().dark_mode { 80 } else { 16 }),
        }
        .as_shape(r, CornerRadius::same(8)),
    );
    painter.rect(r, CornerRadius::same(8), p.surface, Stroke::new(1.0, p.border), StrokeKind::Inside);
    let mut c = ui.new_child(UiBuilder::new().max_rect(r.shrink(3.0)).layout(Layout::left_to_right(Align::Center)));
    c.spacing_mut().item_spacing.x = 0.0;
    let anchor = rect.size() / 2.0;
    if kit::icon_button_ex(&mut c, icons::MINUS, tr("Zoom out"), false, 26.0).clicked() {
        set_zoom(tab, tab.zoom / 1.2, anchor);
    }
    if kit::icon_button_ex(&mut c, icons::PLUS, tr("Zoom in"), false, 26.0).clicked() {
        set_zoom(tab, tab.zoom * 1.2, anchor);
    }
}

/// A flow with only its Start block: what to do next, with quick buttons
/// that add a first block and connect it.
fn empty_canvas(ui: &mut Ui, tab: &mut FlowTab, rect: Rect) {
    let p = theme::pal(ui);
    let w = 420.0_f32.min(rect.width() - 32.0);
    let h = 132.0;
    if w < 240.0 || rect.height() < 300.0 {
        return;
    }
    let cy = (rect.center().y + 130.0).min(rect.bottom() - h / 2.0 - 56.0);
    let card = Rect::from_center_size(Pos2::new(rect.center().x, cy), vec2(w, h));
    ui.painter().rect(card, CornerRadius::same(10), p.surface, Stroke::new(1.0, p.border), StrokeKind::Inside);
    let mut c =
        ui.new_child(UiBuilder::new().max_rect(card.shrink2(vec2(16.0, 16.0))).layout(Layout::top_down(Align::Center)));
    c.label(RichText::new(tr("Build your flow")).font(theme::semibold(15.0)).color(p.text));
    c.add_space(2.0);
    c.label(
        RichText::new(tr(
            "Add a first step after Start, or drag from an output port to an input port to connect blocks.",
        ))
        .color(p.text_muted),
    );
    c.add_space(12.0);
    let kinds = [BlockKind::Request, BlockKind::Condition, BlockKind::Loop, BlockKind::Script, BlockKind::Ai];
    let widths: f32 = kinds
        .iter()
        .map(|k| {
            c.painter().layout_no_wrap(tr(k.label()).to_string(), theme::medium(12.0), p.text).size().x
                + 14.0
                + 6.0
                + 16.0
                + 6.0
        })
        .sum();
    let mut chosen = None;
    c.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(((ui.available_width() - widths) / 2.0).max(0.0));
        for k in kinds {
            if kit::button_ex(ui, kit::Kind::Secondary, Some(kind_icon(k)), tr(k.label()), true)
                .on_hover_text(kind_description(k))
                .clicked()
            {
                chosen = Some(k);
            }
        }
    });
    if let Some(kind) = chosen {
        match tab.flow.start().map(|b| b.id.clone()) {
            Some(start) => add_after(tab, &start, "next", kind),
            None => {
                add_block(tab, kind, None);
            }
        }
    }
}

/// Zoom (between [`FIT_MIN_ZOOM`] and 100 %) and pan so the blocks are in
/// view: centered when they fit, from the left (where flows start) when not.
fn fit_view(tab: &mut FlowTab, size: Vec2) {
    tab.moved_view = false;
    let mut min = Pos2::new(f32::INFINITY, f32::INFINITY);
    let mut max = Pos2::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    for b in &tab.flow.blocks {
        min = min.min(Pos2::new(b.position.0, b.position.1));
        max = max.max(Pos2::new(b.position.0 + BLOCK_W, b.position.1 + block_height(b.kind())));
    }
    if !min.x.is_finite() || size.x <= 0.0 {
        tab.zoom = 1.0;
        tab.pan = Vec2::new(40.0, 40.0);
        return;
    }
    let margin = 48.0;
    let content = max - min;
    let zoom = ((size.x - 2.0 * margin) / content.x).min((size.y - 2.0 * margin) / content.y);
    tab.zoom = zoom.clamp(FIT_MIN_ZOOM, 1.0);
    let used = content * tab.zoom;
    let axis = |avail: f32, used: f32| if used + 2.0 * margin <= avail { (avail - used) / 2.0 } else { margin };
    tab.pan = Vec2::new(axis(size.x, used.x), axis(size.y, used.y)) - min.to_vec2() * tab.zoom;
}

// ---------------------------------------------------------------------------
// Run log
// ---------------------------------------------------------------------------

fn run_log(ui: &mut Ui, tab: &mut FlowTab) {
    let p = theme::pal(ui);
    let mut clear = false;
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(icons::TERMINAL_WINDOW).font(theme::icon_font(15.0)).color(p.text_muted));
        ui.label(kit::section_title(tr("Run log")));
        if tab.is_running() {
            let since = tab.run.started.map(elapsed).unwrap_or_default();
            ui.add(egui::Spinner::new().size(13.0).color(p.accent));
            ui.label(RichText::new(trf("Running… {}", &[&since])).color(p.accent_text).font(theme::medium(12.5)));
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        } else if let Some(r) = &tab.run.result {
            let (text, color, icon) = if r.cancelled {
                (tr("Stopped"), p.warn, icons::STOP)
            } else if r.success {
                (tr("Succeeded"), p.ok, icons::CHECK_CIRCLE)
            } else {
                (tr("Failed"), p.err, icons::X_CIRCLE)
            };
            kit::pill(ui, &format!("{icon} {text}"), color);
            ui.label(
                RichText::new(format!("{} · {}", steps_text(r.steps.len()), human_ms(r.duration_ms)))
                    .color(p.text_muted)
                    .small(),
            );
            let tests = r.tests_passed + r.tests_failed;
            if tests > 0 {
                let c = if r.tests_failed > 0 { p.err } else { p.ok };
                kit::pill(ui, &trf("{}/{} tests", &[&r.tests_passed, &tests]), c);
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let can_clear = !tab.is_running() && (!tab.run.log.is_empty() || tab.run.result.is_some());
            if ui
                .add_enabled_ui(can_clear, |ui| {
                    kit::icon_button(ui, icons::BROOM, tr("Clear the log and block states"))
                })
                .inner
                .clicked()
            {
                clear = true;
            }
            if let Some(r) = &tab.run.result {
                for (k, v) in r.outputs.iter().take(3) {
                    if ui.available_width() < 140.0 {
                        break;
                    }
                    let max_w = (ui.available_width() - 70.0).min(320.0);
                    let full = match v {
                        Value::String(s) => s.clone(),
                        v => v.to_string(),
                    };
                    let text = crate::dialogs::ellipsize(&full.replace('\n', " "), 36);
                    egui::Frame::new()
                        .fill(p.subtle)
                        .corner_radius(CornerRadius::same(RADIUS))
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.set_max_width(max_w);
                            let mut job = LayoutJob::default();
                            job.append(k, 0.0, TextFormat::simple(theme::medium(12.0), p.text_muted));
                            job.append(" = ", 0.0, TextFormat::simple(theme::mono(11.5), p.text_faint));
                            job.append(&text, 0.0, TextFormat::simple(theme::mono(11.5), p.text));
                            ui.add(egui::Label::new(job).truncate());
                        })
                        .response
                        .on_hover_text(full);
                }
                if !r.outputs.is_empty() && ui.available_width() > 60.0 {
                    ui.label(RichText::new(tr("Outputs")).small().color(p.text_faint));
                }
            }
        });
    });
    if clear {
        tab.run = RunState::default();
    }
    ui.add_space(4.0);
    let frame = egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(10, 8));
    frame.show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        egui::ScrollArea::vertical().id_salt("flow-log-scroll").stick_to_bottom(true).auto_shrink([false; 2]).show(
            ui,
            |ui| {
                if tab.run.log.is_empty() {
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new(icons::PLAY_CIRCLE).font(theme::icon_font(15.0)).color(p.text_faint));
                        let text = if tab.is_running() {
                            tr("Waiting for the first block…")
                        } else {
                            tr("Run the flow to see each step, its timing and its messages here.")
                        };
                        ui.add(egui::Label::new(RichText::new(text).color(p.text_muted)).wrap());
                    });
                    return;
                }
                ui.spacing_mut().item_spacing.y = 3.0;
                let skip = tab.run.log.len().saturating_sub(LOG_SHOWN);
                if skip > 0 {
                    ui.label(RichText::new(trf("{} earlier lines not shown", &[&skip])).small().color(p.text_faint));
                }
                for line in &tab.run.log[skip..] {
                    log_row(ui, &p, line);
                }
            },
        );
    });
}

fn log_row(ui: &mut Ui, p: &Palette, line: &LogLine) {
    let (icon, color) = match line.kind {
        LogKind::Step => (icons::CHECK, p.ok),
        LogKind::Info => (icons::INFO, p.info),
        LogKind::Debug => (icons::BUG, p.text_faint),
        LogKind::Warn => (icons::WARNING, p.warn),
        LogKind::Error => (icons::X_CIRCLE, p.err),
        LogKind::Done => (icons::CHECK_CIRCLE, p.ok),
    };
    let text_color = match line.kind {
        LogKind::Error => p.err,
        LogKind::Debug => p.text_muted,
        _ => p.text,
    };
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(clock(line.at)).font(theme::mono(11.0)).color(p.text_faint));
        ui.label(RichText::new(icon).font(theme::icon_font(13.5)).color(color));
        let mut job = LayoutJob::default();
        if let Some(b) = &line.block {
            let chip = TextFormat { background: p.subtle, ..TextFormat::simple(theme::medium(12.0), p.text) };
            job.append(&format!(" {b} "), 0.0, chip);
            job.append("  ", 0.0, TextFormat::simple(theme::medium(12.0), p.text_muted));
        }
        let font = if matches!(line.kind, LogKind::Done) { theme::medium(12.5) } else { theme::mono(12.0) };
        job.append(&line.text, 0.0, TextFormat::simple(font, text_color));
        ui.add(egui::Label::new(job).wrap());
    });
}

// ---------------------------------------------------------------------------
// Inspector
// ---------------------------------------------------------------------------

/// Space and a hairline between inspector sections.
fn section_gap(ui: &mut Ui) {
    let p = theme::pal(ui);
    ui.add_space(14.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.border));
    ui.add_space(14.0);
}

fn section_head(ui: &mut Ui, title: &str, help: Option<&str>) {
    ui.label(kit::section_title(title));
    if let Some(h) = help {
        ui.add_space(1.0);
        ui.label(kit::hint(ui, h));
    }
    ui.add_space(6.0);
}

fn field_label(ui: &mut Ui, text: &str) {
    let p = theme::pal(ui);
    ui.add_space(6.0);
    ui.label(RichText::new(text).font(theme::medium(12.5)).color(p.text_muted));
    ui.add_space(1.0);
}

/// A label on the left and a control on the right.
fn setting_row(ui: &mut Ui, label: &str, control: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(26.0);
        ui.label(label);
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
}

/// The top of the inspector: a tinted icon, a small line above the title,
/// and actions on the right.
fn inspector_head(ui: &mut Ui, icon: &str, color: Color32, eyebrow: &str, title: &str, actions: impl FnOnce(&mut Ui)) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    ui.horizontal(|ui| {
        ui.set_height(38.0);
        let (tile, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        ui.painter().rect_filled(tile, CornerRadius::same(8), theme::soft(color, if dark { 0.2 } else { 0.12 }));
        ui.painter().text(tile.center(), Align2::CENTER_CENTER, icon, theme::icon_font(19.0), color);
        ui.add_space(2.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            actions(ui);
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(1.0);
                ui.add(egui::Label::new(RichText::new(eyebrow).small().color(p.text_muted)).truncate());
                ui.add(egui::Label::new(RichText::new(title).font(theme::semibold(15.0)).color(p.text)).truncate());
            });
        });
    });
}

/// A tinted box with an icon and a message (run state of a block).
fn banner(ui: &mut Ui, color: Color32, icon: Option<&str>, title: &str, detail: Option<&str>) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    egui::Frame::new()
        .fill(theme::soft(color, if dark { 0.12 } else { 0.07 }))
        .stroke(Stroke::new(1.0, theme::soft(color, 0.25)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                match icon {
                    Some(i) => {
                        ui.label(RichText::new(i).font(theme::icon_font(16.0)).color(color));
                    }
                    None => {
                        ui.add(egui::Spinner::new().size(14.0).color(color));
                    }
                }
                ui.label(RichText::new(title).font(theme::medium(13.0)).color(p.text));
            });
            if let Some(d) = detail {
                ui.add_space(4.0);
                ui.label(RichText::new(d).font(theme::mono(12.0)).color(if color == p.err {
                    p.err
                } else {
                    p.text_muted
                }));
            }
        });
}

fn problem_row(ui: &mut Ui, severity: Severity, who: Option<&str>, message: &str) -> Response {
    let p = theme::pal(ui);
    let (icon, color) = if severity == Severity::Error { (icons::X_CIRCLE, p.err) } else { (icons::WARNING, p.warn) };
    let bg = ui.painter().add(Shape::Noop);
    let row = ui
        .horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add_space(4.0);
            ui.label(RichText::new(icon).font(theme::icon_font(14.5)).color(color));
            let mut job = LayoutJob::default();
            if let Some(w) = who {
                job.append(w, 0.0, TextFormat::simple(theme::medium(13.0), p.text));
                job.append("  ", 0.0, TextFormat::simple(theme::medium(13.0), p.text));
            }
            job.append(message, 0.0, TextFormat::simple(FontId::proportional(13.0), p.text_muted));
            ui.add(egui::Label::new(job).wrap().selectable(false));
        })
        .response;
    let rect = row.rect.expand2(vec2(0.0, 3.0));
    let resp = ui.interact(rect, row.id.with("problem"), Sense::click());
    if resp.hovered() {
        ui.painter().set(bg, Shape::rect_filled(rect, CornerRadius::same(RADIUS), p.hover));
    }
    ui.add_space(2.0);
    resp
}

fn inspector(app: &mut MerpatiApp, ui: &mut Ui, tab: &mut FlowTab) {
    ui.add_space(2.0);
    match tab.selected.clone() {
        Some(Selection::Edge(i)) => edge_inspector(ui, tab, i),
        Some(Selection::Block(id)) => block_inspector(app, ui, tab, &id),
        None => flow_settings(ui, tab),
    }
    ui.add_space(20.0);
}

#[derive(PartialEq)]
enum HeadAction {
    None,
    Close,
    Duplicate,
    Delete,
}

fn block_inspector(app: &mut MerpatiApp, ui: &mut Ui, tab: &mut FlowTab, id: &str) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let Some(mut block) = tab.flow.block(id).cloned() else {
        tab.selected = None;
        return;
    };
    let kind = block.kind();
    let mut act = HeadAction::None;
    inspector_head(ui, kind_icon(kind), kind_color(kind, dark), tr(kind.label()), block.display_name(), |ui| {
        if kit::icon_button(ui, icons::X, tr("Close")).clicked() {
            act = HeadAction::Close;
        }
        if kind != BlockKind::Start {
            if kit::icon_button(ui, icons::TRASH, tr("Delete block")).clicked() {
                act = HeadAction::Delete;
            }
            if kit::icon_button(ui, icons::COPY, tr("Duplicate")).clicked() {
                act = HeadAction::Duplicate;
            }
        }
    });
    match act {
        HeadAction::None => {}
        HeadAction::Close => {
            tab.selected = None;
            return;
        }
        HeadAction::Duplicate => {
            duplicate_block(tab, id);
            return;
        }
        HeadAction::Delete => {
            tab.flow.remove_block(id);
            tab.selected = None;
            tab.touch();
            return;
        }
    }
    ui.add_space(12.0);

    // Run state first: it is what one looks for while a flow runs.
    match tab.run.status.get(&block.id) {
        Some(BlockStatus::Running { since }) => {
            banner(ui, p.accent, None, &trf("Running… {}", &[&elapsed(*since)]), None);
            ui.add_space(4.0);
        }
        Some(BlockStatus::Done { port, failed, ms, error, tests, .. }) => {
            let (color, icon) = if *failed { (p.err, icons::X_CIRCLE) } else { (p.ok, icons::CHECK_CIRCLE) };
            let mut title =
                format!("{} → {} · {}", if *failed { tr("Failed") } else { tr("Succeeded") }, tr(port), human_ms(*ms));
            if tests.0 + tests.1 > 0 {
                title.push_str(&format!(" · {}", trf("{}/{} tests", &[&tests.0, &(tests.0 + tests.1)])));
            }
            banner(ui, color, Some(icon), &title, error.as_deref());
            ui.add_space(4.0);
        }
        None => {}
    }

    section_head(ui, tr("Block"), None);
    field_label(ui, tr("Label"));
    ui.add(egui::TextEdit::singleline(&mut block.label).hint_text(tr(kind.label())).desired_width(f32::INFINITY));
    field_label(ui, tr("Reference"));
    let reference = format!("{{{{blocks.{}.output}}}}", reference_key(&block));
    egui::Frame::new()
        .fill(p.subtle)
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(egui::Margin { left: 8, right: 2, top: 2, bottom: 2 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if kit::icon_button_ex(ui, icons::COPY, tr("Copy"), false, 24.0).clicked() {
                        ui.ctx().copy_text(reference.clone());
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&reference).font(theme::mono(12.0)).color(p.text))
                                .truncate(),
                        );
                    });
                });
            });
        });
    ui.label(kit::hint(ui, tr("Later blocks read this block's output with this reference.")));

    section_gap(ui);
    let vars = app.var_ctx(None);
    config_editor(app, ui, &mut block, &vars);

    let problems: Vec<&Diagnostic> =
        tab.diagnostics.iter().filter(|d| d.block.as_deref() == Some(block.id.as_str())).collect();
    if !problems.is_empty() {
        section_gap(ui);
        section_head(ui, tr("Problems"), None);
        for d in problems {
            problem_row(ui, d.severity, None, &d.message);
        }
    }
    if let Some(text) = tab.run.live_text(&block.id) {
        section_gap(ui);
        section_head(ui, tr("Answer so far"), None);
        crate::proto_common::text_view(ui, ("flow-live", &block.id), &crate::dialogs::ellipsize(text, 20_000));
    }
    if let Some(BlockStatus::Done { output, .. }) = tab.run.status.get(&block.id) {
        section_gap(ui);
        section_head(ui, tr("Output"), Some(tr("What the block produced in the last run.")));
        let text = serde_json::to_string_pretty(output.as_ref()).unwrap_or_default();
        crate::proto_common::text_view(ui, ("flow-output", &block.id), &crate::dialogs::ellipsize(&text, 20_000));
    }
    if let Some(slot) = tab.flow.block_mut(&block.id)
        && *slot != block
    {
        *slot = block;
        tab.touch();
    }
}

fn edge_inspector(ui: &mut Ui, tab: &mut FlowTab, i: usize) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let Some(e) = tab.flow.edges.get(i).cloned() else {
        tab.selected = None;
        return;
    };
    let name = |id: &str| tab.flow.block(id).map(|b| b.display_name().to_string()).unwrap_or_default();
    let title = format!("{} → {}", name(&e.from_block), name(&e.to_block));
    let mut close = false;
    inspector_head(ui, icons::LINK_SIMPLE, port_color(&e.from_port, &p, dark), tr("Connection"), &title, |ui| {
        close = kit::icon_button(ui, icons::X, tr("Close")).clicked();
    });
    if close {
        tab.selected = None;
        return;
    }
    ui.add_space(12.0);
    let mut select = None;
    for (head, block, port) in [(tr("From"), &e.from_block, &e.from_port), (tr("To"), &e.to_block, &e.to_port)] {
        field_label(ui, head);
        if endpoint_row(ui, tab.flow.block(block), port).clicked() {
            select = Some(block.clone());
        }
    }
    ui.add_space(4.0);
    ui.label(kit::hint(
        ui,
        &trf("“{}” runs when “{}” finishes on “{}”.", &[&name(&e.to_block), &name(&e.from_block), &tr(&e.from_port)]),
    ));
    ui.add_space(12.0);
    let mut delete = false;
    ui.horizontal(|ui| {
        delete = kit::button_ex(ui, kit::Kind::Danger, Some(icons::TRASH), tr("Delete connection"), false).clicked();
        kit::kbd(ui, "⌫");
    });
    if delete {
        tab.flow.edges.remove(i);
        tab.selected = None;
        tab.touch();
    } else if let Some(b) = select {
        tab.selected = Some(Selection::Block(b));
    }
}

/// One end of a connection: the block (icon and name) and the port.
fn endpoint_row(ui: &mut Ui, block: Option<&Block>, port: &str) -> Response {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 36.0), Sense::click());
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(8),
        if resp.hovered() { p.hover } else { p.surface },
        Stroke::new(1.0, p.border),
        StrokeKind::Inside,
    );
    let (icon, color, name) = match block {
        Some(b) => (kind_icon(b.kind()), kind_color(b.kind(), dark), b.display_name().to_string()),
        None => (icons::QUESTION, p.text_muted, tr("(missing block)").to_string()),
    };
    let tile = Rect::from_center_size(Pos2::new(rect.left() + 20.0, rect.center().y), Vec2::splat(22.0));
    painter.rect_filled(tile, CornerRadius::same(6), color);
    painter.text(tile.center(), Align2::CENTER_CENTER, icon, theme::icon_font(13.0), p.surface);
    let pc = port_color(port, &p, dark);
    let pg = painter.layout_no_wrap(tr(port).to_string(), theme::medium(11.5), pc);
    let pill = Rect::from_min_size(
        Pos2::new(rect.right() - 10.0 - pg.size().x - 14.0, rect.center().y - 10.0),
        vec2(pg.size().x + 14.0, 20.0),
    );
    painter.rect_filled(pill, CornerRadius::same(10), theme::soft(pc, if dark { 0.18 } else { 0.11 }));
    painter.galley(pill.center() - pg.size() / 2.0, pg, pc);
    let g = one_line(painter, &name, theme::medium(13.0), p.text, pill.left() - 8.0 - (tile.right() + 10.0));
    painter.galley(Pos2::new(tile.right() + 10.0, rect.center().y - g.size().y / 2.0), g, p.text);
    widgets::a11y_button(&resp, &name);
    resp
}

fn flow_settings(ui: &mut Ui, tab: &mut FlowTab) {
    let p = theme::pal(ui);
    let edges = tab.flow.edges.len();
    let blocks = tab.flow.blocks.len();
    inspector_head(
        ui,
        icons::FLOW_ARROW,
        p.accent,
        &format!("{} · {}", blocks_text(blocks), connections_text(edges)),
        tr("Flow settings"),
        |_| {},
    );
    ui.add_space(12.0);
    let mut flow = tab.flow.clone();

    section_head(ui, tr("Description"), None);
    ui.add(
        egui::TextEdit::multiline(&mut flow.description)
            .hint_text(tr("What the flow does and what it needs"))
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );

    section_gap(ui);
    section_head(ui, tr("Inputs"), Some(tr("Values the flow starts with; blocks read them as {{inputs.name}}.")));
    let mut remove = None;
    for (i, input) in flow.inputs.iter_mut().enumerate() {
        ui.push_id(i, |ui| {
            egui::Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if widgets::small_icon_button(ui, icons::TRASH, tr("Remove input")).clicked() {
                                remove = Some(i);
                            }
                            ui.checkbox(&mut input.required, tr("Required"));
                            ui.add(
                                egui::TextEdit::singleline(&mut input.name)
                                    .hint_text(tr("name"))
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(ui.available_width()),
                            );
                        });
                    });
                    ui.add_space(4.0);
                    let label_w = 76.0;
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(vec2(label_w, 24.0), Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new(tr("Default")).color(p.text_muted).small());
                        });
                        let mut default = if input.default.is_null() {
                            String::new()
                        } else {
                            serde_json::to_string(&input.default).unwrap_or_default()
                        };
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut default)
                                    .hint_text(tr("JSON value"))
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(ui.available_width()),
                            )
                            .changed()
                        {
                            input.default = if default.trim().is_empty() {
                                Value::Null
                            } else {
                                serde_json::from_str(&default).unwrap_or(Value::String(default.clone()))
                            };
                        }
                    });
                    let value = tab.inputs.entry(input.name.clone()).or_default();
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(vec2(label_w, 24.0), Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new(tr("Next run")).color(p.text_muted).small());
                        });
                        ui.add(
                            egui::TextEdit::singleline(value)
                                .hint_text(tr("value for the next run"))
                                .desired_width(ui.available_width()),
                        );
                    });
                });
        });
        ui.add_space(6.0);
    }
    if let Some(i) = remove {
        flow.inputs.remove(i);
    }
    if kit::ghost(ui, Some(icons::PLUS), tr("Add input")).clicked() {
        flow.inputs.push(FlowInput::new(format!("input{}", flow.inputs.len() + 1), Value::Null));
    }

    section_gap(ui);
    section_head(ui, tr("Run settings"), None);
    setting_row(ui, tr("Limit steps"), |ui| {
        if let Some(n) = &mut flow.settings.max_steps {
            let r = ui.add(egui::DragValue::new(n).range(1..=1_000_000));
            widgets::a11y_name(&r, tr("Max steps"));
        }
        let mut on = flow.settings.max_steps.is_some();
        if ui.checkbox(&mut on, "").changed() {
            flow.settings.max_steps = on.then_some(merpati_flows::DEFAULT_MAX_STEPS);
        }
    });
    ui.label(kit::hint(ui, tr("Stops a run after this many block runs (protects against endless loops).")));
    ui.add_space(6.0);
    setting_row(ui, tr("Parallel branches at once"), |ui| {
        let r = ui.add(egui::DragValue::new(&mut flow.settings.max_concurrent).range(0..=64));
        widgets::a11y_name(&r, tr("Parallel branches at once"));
    });
    ui.label(kit::hint(ui, tr("Branches of a Parallel block that run at the same time; 0 runs them all at once.")));
    ui.add_space(6.0);
    ui.checkbox(
        &mut flow.settings.continue_on_failure,
        tr("Continue after a failed block without a failure connection"),
    );

    if flow != tab.flow {
        tab.flow = flow;
        tab.touch();
    }

    if !tab.diagnostics.is_empty() {
        section_gap(ui);
        let errors = tab.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
        ui.horizontal(|ui| {
            ui.label(kit::section_title(tr("Problems")));
            kit::pill(ui, &tab.diagnostics.len().to_string(), if errors > 0 { p.err } else { p.warn });
        });
        ui.label(kit::hint(ui, tr("Click a problem to select its block.")));
        ui.add_space(6.0);
        let mut select = None;
        for d in &tab.diagnostics {
            let who = d.block.as_deref().and_then(|b| tab.flow.block(b)).map(|b| b.display_name().to_string());
            if problem_row(ui, d.severity, who.as_deref(), &d.message).clicked() {
                select = d
                    .block
                    .clone()
                    .map(Selection::Block)
                    .or_else(|| d.edge.filter(|e| *e < tab.flow.edges.len()).map(Selection::Edge));
            }
        }
        if select.is_some() {
            tab.selected = select;
        }
    }

    section_gap(ui);
    section_head(ui, tr("Keyboard"), None);
    for (keys, what) in [
        ("⌫", tr("Delete the selection")),
        ("⌘D", tr("Duplicate the block")),
        ("← ↑ → ↓", tr("Move the block (⇧ for bigger steps)")),
        ("Esc", tr("Clear the selection")),
        ("⌘ + scroll", tr("Zoom around the pointer")),
    ] {
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(vec2(86.0, 22.0), Layout::left_to_right(Align::Center), |ui| {
                kit::kbd(ui, keys);
            });
            ui.label(RichText::new(what).color(p.text_muted).small());
        });
    }
}

/// Picker for saved requests of the given protocols ("Collection / name").
fn request_picker(app: &MerpatiApp, ui: &mut Ui, id: &str, current: &mut ItemRef, want_ai: bool) -> bool {
    let mut changed = false;
    let shown = request_name(app, current).unwrap_or_else(|| tr("Choose a request").into());
    egui::ComboBox::from_id_salt(id)
        .icon(crate::kit::combo_caret)
        .width(ui.available_width())
        .selected_text(crate::dialogs::ellipsize(&shown, 44))
        .show_ui(ui, |ui| {
            for c in &app.ws.data.collections {
                for r in c.requests() {
                    let ok = if want_ai {
                        matches!(r.request, RequestKind::Ai(_))
                    } else {
                        !matches!(r.request, RequestKind::Ai(_))
                    };
                    if !ok {
                        continue;
                    }
                    let sel = current.request_id == r.id;
                    if ui.selectable_label(sel, format!("{} / {} {}", c.name, r.request.badge(), r.name)).clicked() {
                        *current = ItemRef::new(&c.id, &r.id);
                        changed = true;
                    }
                }
            }
        });
    changed
}

/// A field label and a full-width `{{variable}}` line.
fn var_field(ui: &mut Ui, vars: &widgets::VarCtx, label: &str, id: &str, value: &mut String, hint: &str) {
    field_label(ui, label);
    widgets::var_line(ui, id, value, vars, hint, ui.available_width());
}

fn text_field(ui: &mut Ui, label: &str, value: &mut String, hint: &str) {
    field_label(ui, label);
    ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(f32::INFINITY));
}

fn config_editor(app: &MerpatiApp, ui: &mut Ui, block: &mut Block, vars: &widgets::VarCtx) {
    let id = block.id.clone();
    let key = reference_key(block);
    match &mut block.config {
        BlockConfig::Start(_) => {
            section_head(
                ui,
                tr("Start"),
                Some(tr(
                    "The flow starts here. Declare its inputs in the flow settings (click an empty spot of the canvas).",
                )),
            );
        }
        BlockConfig::Request(c) => {
            section_head(ui, tr("Request"), None);
            let mut saved = matches!(c.source, RequestSource::Saved(_));
            if kit::segmented(ui, &mut saved, &[(true, tr("Saved request")), (false, tr("Inline request"))]) {
                c.source = if saved { RequestSource::Saved(ItemRef::default()) } else { RequestSource::default() };
            }
            match &mut c.source {
                RequestSource::Saved(r) => {
                    field_label(ui, tr("Request"));
                    request_picker(app, ui, &format!("flow-req-{id}"), r, false);
                    ui.label(kit::hint(
                        ui,
                        tr("Runs with the collection's auth, scripts and variables (HTTP, gRPC, MCP)."),
                    ));
                }
                RequestSource::Inline { request, .. } => {
                    field_label(ui, tr("URL"));
                    ui.horizontal(|ui| {
                        widgets::method_picker(ui, &mut request.method);
                        let mut url = request.url.clone();
                        if widgets::var_line(
                            ui,
                            ("flow-url", &id),
                            &mut url,
                            vars,
                            "https://api.example.com/items",
                            ui.available_width(),
                        )
                        .changed()
                        {
                            request.set_url(url);
                        }
                    });
                    field_label(ui, tr("Headers"));
                    let mut h = request.headers.clone();
                    if widgets::kv_table(
                        ui,
                        ("flow-headers", &id),
                        &mut h,
                        vars,
                        KvOptions { allow_add: true, ..Default::default() },
                    ) {
                        request.headers = h;
                    }
                    field_label(ui, tr("Body (JSON)"));
                    let mut raw = request.body.raw.clone();
                    if widgets::code_editor(ui, ("flow-body", &id), &mut raw, Lang::Json, Some(vars), 3, false)
                        .changed()
                    {
                        request.body = if raw.trim().is_empty() {
                            merpati_core::model::Body::default()
                        } else {
                            merpati_core::model::Body::raw_json(raw)
                        };
                    }
                }
            }
            var_field(ui, vars, tr("Success status"), &format!("flow-st-{id}"), &mut c.success_status, "2xx");
            ui.label(kit::hint(
                ui,
                tr("Statuses that count as success, like 2xx or 200-299. Empty: any status below 400."),
            ));
        }
        BlockConfig::Condition(c) => {
            section_head(ui, tr("Condition"), Some(tr("Continues on “true” or “false”.")));
            kit::segmented(
                ui,
                &mut c.mode,
                &[(ConditionMode::Compare, tr("Compare")), (ConditionMode::Expression, tr("JavaScript"))],
            );
            match c.mode {
                ConditionMode::Compare => {
                    var_field(
                        ui,
                        vars,
                        tr("Value"),
                        &format!("flow-left-{id}"),
                        &mut c.left,
                        "{{blocks.list.output.status}}",
                    );
                    field_label(ui, tr("Operator"));
                    egui::ComboBox::from_id_salt(("flow-op", &id))
                        .icon(crate::kit::combo_caret)
                        .width(ui.available_width())
                        .selected_text(tr(c.op.label()))
                        .show_ui(ui, |ui| {
                            for op in CompareOp::ALL {
                                ui.selectable_value(&mut c.op, op, tr(op.label()));
                            }
                        });
                    if !c.op.is_unary() {
                        var_field(ui, vars, tr("Compared with"), &format!("flow-right-{id}"), &mut c.right, "200");
                    }
                }
                ConditionMode::Expression => {
                    field_label(ui, tr("Expression"));
                    widgets::code_editor(ui, ("flow-expr", &id), &mut c.expression, Lang::JavaScript, None, 3, false);
                    ui.label(kit::hint(ui, tr("input.status === 200 && input.json.items.length > 0")));
                }
            }
        }
        BlockConfig::Loop(c) => {
            section_head(
                ui,
                tr("Loop"),
                Some(tr(
                    "Blocks connected to “item” run once per element ({{loop.item}}, {{loop.index}}); “done” runs after the last one.",
                )),
            );
            kit::segmented(ui, &mut c.mode, &[(LoopMode::ForEach, tr("For each")), (LoopMode::Count, tr("Count"))]);
            match c.mode {
                LoopMode::ForEach => var_field(
                    ui,
                    vars,
                    tr("Items"),
                    &format!("flow-items-{id}"),
                    &mut c.items,
                    "{{blocks.list.output.json.items}}",
                ),
                LoopMode::Count => var_field(ui, vars, tr("Times"), &format!("flow-count-{id}"), &mut c.count, "5"),
            }
            ui.add_space(6.0);
            setting_row(ui, tr("Max iterations"), |ui| {
                let mut n = c.max_iterations.unwrap_or(merpati_flows::DEFAULT_MAX_ITERATIONS);
                let r = ui.add(egui::DragValue::new(&mut n).range(1..=1_000_000));
                widgets::a11y_name(&r, tr("Max iterations"));
                if r.changed() {
                    c.max_iterations = Some(n);
                }
            });
        }
        BlockConfig::Script(c) => {
            section_head(
                ui,
                tr("Script"),
                Some(tr("blocks, inputs, input, vars, loop, flow and pm are available; return sets the output.")),
            );
            widgets::code_editor(ui, ("flow-script", &id), &mut c.code, Lang::JavaScript, None, 10, false);
        }
        BlockConfig::Ai(c) => {
            section_head(ui, tr("AI"), None);
            let mut saved = c.request.is_some();
            if ui.checkbox(&mut saved, tr("Based on a saved AI request")).changed() {
                c.request = saved.then(ItemRef::default);
            }
            if let Some(r) = &mut c.request {
                field_label(ui, tr("Request"));
                request_picker(app, ui, &format!("flow-ai-{id}"), r, true);
            } else {
                let current = c.provider.unwrap_or_default();
                field_label(ui, tr("Provider"));
                egui::ComboBox::from_id_salt(("flow-ai-provider", &id))
                    .icon(crate::kit::combo_caret)
                    .width(ui.available_width())
                    .selected_text(current.label())
                    .show_ui(ui, |ui| {
                        for p in AiProvider::ALL {
                            if ui.selectable_label(current == p, p.label()).clicked() {
                                c.provider = Some(p);
                            }
                        }
                    });
                var_field(ui, vars, tr("Model"), &format!("flow-ai-model-{id}"), &mut c.model, "gpt-5-mini");
                var_field(
                    ui,
                    vars,
                    tr("Base URL"),
                    &format!("flow-ai-url-{id}"),
                    &mut c.base_url,
                    merpati_ai::default_base_url(current),
                );
                var_field(ui, vars, tr("API key"), &format!("flow-ai-key-{id}"), &mut c.api_key, "{{vault:openai}}");
            }
            field_label(ui, tr("System prompt"));
            widgets::code_editor(ui, ("flow-ai-system", &id), &mut c.system, Lang::Text, Some(vars), 2, false);
            field_label(ui, tr("Prompt"));
            widgets::code_editor(ui, ("flow-ai-prompt", &id), &mut c.prompt, Lang::Text, Some(vars), 4, false);
            ui.add_space(4.0);
            ui.checkbox(&mut c.require_json, tr("Fail unless the answer is JSON"));
            ui.checkbox(&mut c.stream, tr("Stream the answer (shown live while the flow runs)"));
        }
        BlockConfig::Parallel(_) => {
            section_head(
                ui,
                tr("Parallel"),
                Some(tr(
                    "Every block connected to “branches” starts a branch, and the branches run at the same time. Each branch starts from a copy of the variables; their changes are applied in branch order (top to bottom) once all are done.",
                )),
            );
            ui.label(kit::hint(
                ui,
                tr("Connect the ends of the branches to a Join to continue once all of them have finished."),
            ));
        }
        BlockConfig::Join(_) => {
            section_head(
                ui,
                tr("Join"),
                Some(tr(
                    "Waits until every branch of the Parallel block has finished, then continues once with the results of all branches.",
                )),
            );
            for (r, what) in [
                (format!("{{{{blocks.{key}.output.results.<block>}}}}"), tr("output of a block by its name")),
                (format!("{{{{blocks.{key}.output.items}}}}"), tr("all outputs as a list")),
                (format!("{{{{blocks.{key}.output.count}}}}"), tr("number of branches that arrived")),
            ] {
                ui.add_space(4.0);
                ui.label(RichText::new(r).font(theme::mono(12.0)));
                ui.label(kit::hint(ui, what));
            }
        }
        BlockConfig::Delay(c) => {
            section_head(ui, tr("Delay"), None);
            var_field(ui, vars, tr("Milliseconds"), &format!("flow-ms-{id}"), &mut c.ms, "1000");
        }
        BlockConfig::SetVariable(c) => {
            section_head(ui, tr("Set variable"), None);
            text_field(ui, tr("Name"), &mut c.name, "token");
            var_field(
                ui,
                vars,
                tr("Value"),
                &format!("flow-val-{id}"),
                &mut c.value,
                "{{blocks.login.output.json.token}}",
            );
            field_label(ui, tr("Scope"));
            kit::segmented(
                ui,
                &mut c.scope,
                &[
                    (VariableScope::Flow, tr("Flow")),
                    (VariableScope::Environment, tr("Environment")),
                    (VariableScope::Global, tr("Global")),
                ],
            );
        }
        BlockConfig::Template(c) => {
            section_head(ui, tr("Template"), None);
            kit::segmented(
                ui,
                &mut c.mode,
                &[(TemplateMode::Template, tr("Template")), (TemplateMode::Expression, tr("JavaScript"))],
            );
            ui.add_space(6.0);
            match c.mode {
                TemplateMode::Template => {
                    widgets::code_editor(ui, ("flow-tpl", &id), &mut c.template, Lang::Json, Some(vars), 4, false);
                    ui.checkbox(&mut c.parse_json, tr("Parse the result as JSON"));
                }
                TemplateMode::Expression => {
                    widgets::code_editor(ui, ("flow-tpl-js", &id), &mut c.expression, Lang::JavaScript, None, 4, false);
                }
            }
        }
        BlockConfig::Output(c) => {
            section_head(ui, tr("Output"), Some(tr("Named results appear in the run log and in `merpati flow run`.")));
            text_field(ui, tr("Name"), &mut c.name, "summary");
            var_field(
                ui,
                vars,
                tr("Value"),
                &format!("flow-outv-{id}"),
                &mut c.value,
                "{{blocks.summary.output.text}}",
            );
            ui.add_space(4.0);
            ui.checkbox(&mut c.fail, tr("Mark the run as failed when reached"));
        }
        BlockConfig::Log(c) => {
            section_head(ui, tr("Log"), None);
            var_field(ui, vars, tr("Message"), &format!("flow-log-msg-{id}"), &mut c.message, "{{loop.item.name}}");
            field_label(ui, tr("Level"));
            kit::segmented(
                ui,
                &mut c.level,
                &[
                    (LogLevelConfig::Info, tr("Info")),
                    (LogLevelConfig::Log, tr("Log")),
                    (LogLevelConfig::Warn, tr("Warn")),
                    (LogLevelConfig::Error, tr("Error")),
                    (LogLevelConfig::Debug, tr("Debug")),
                ],
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Opening, sidebar list
// ---------------------------------------------------------------------------

/// Opens the flow at `path` (or focuses its tab).
pub fn open_flow(app: &mut MerpatiApp, path: PathBuf) {
    if let Some(i) = app.tabs.iter().position(|t| matches!(&t.kind, TabKind::Flow(f) if f.path.as_ref() == Some(&path)))
    {
        app.active = i;
        return;
    }
    match merpati_flows::storage::load(&path) {
        Ok(flow) => {
            app.add_tab(TabKind::Flow(Box::new(FlowTab::new(flow, Some(path)))));
        }
        Err(e) => app.toast(e.to_string(), true),
    }
}

/// What the sidebar list shows of a flow besides its name.
#[derive(Clone, Default)]
struct FlowMeta {
    blocks: usize,
    description: String,
}

enum ListAct {
    New,
    Open(PathBuf),
    Run(PathBuf),
    Rename(String, String),
    Duplicate(PathBuf),
    Delete(String, String, PathBuf),
}

/// A sidebar row: icon, name, a muted detail on the right, and action
/// buttons while hovered (or while its menu is open).
pub(crate) struct SidebarRow<'a> {
    pub icon: &'a str,
    pub icon_color: Color32,
    /// Spinner instead of the icon.
    pub busy: bool,
    pub name: &'a str,
    pub detail: &'a str,
    pub active: bool,
    /// Unsaved changes (a dot after the name).
    pub dirty: bool,
    /// Icon and accessible name of an extra hover action (Run).
    pub action: Option<(&'a str, &'a str)>,
}

pub(crate) struct SidebarRowOut {
    pub row: Response,
    /// The "More actions" button, with the id its menu must use.
    pub more: Option<(Response, egui::Id)>,
    pub action: Option<Response>,
}

pub(crate) fn sidebar_row(
    ui: &mut Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    spec: &SidebarRow,
) -> SidebarRowOut {
    let p = theme::pal(ui);
    let w = ui.available_width();
    let (rect, row) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
    widgets::a11y_button(&row, spec.name);
    let popup_id = ui.scope_id().with(("sidebar-row-menu", key));
    let menu_open = egui::Popup::is_id_open(ui.ctx(), popup_id);
    let hovered = ui.rect_contains_pointer(rect) || menu_open;
    let painter = ui.painter().clone();
    let fill = if spec.active {
        Some(p.accent_soft)
    } else if hovered {
        Some(p.hover)
    } else {
        None
    };
    if let Some(f) = fill {
        painter.rect_filled(rect, CornerRadius::same(RADIUS), f);
    }
    let ic = Pos2::new(rect.left() + 16.0, rect.center().y);
    if spec.busy {
        spinner(ui, &painter, ic, 5.5, p.accent);
    } else {
        let color = if spec.active { p.accent_text } else { spec.icon_color };
        painter.text(ic, Align2::CENTER_CENTER, spec.icon, theme::icon_font(15.0), color);
    }
    let buttons_w = if hovered { 4.0 + 26.0 * (1.0 + spec.action.is_some() as u8 as f32) } else { 0.0 };
    let detail = if hovered || spec.detail.is_empty() {
        None
    } else {
        Some(painter.layout_no_wrap(spec.detail.to_string(), FontId::proportional(11.5), p.text_faint))
    };
    let detail_w = detail.as_ref().map_or(0.0, |g| g.size().x + 10.0);
    let name_x = rect.left() + 32.0;
    let dot_w = if spec.dirty { 12.0 } else { 0.0 };
    let max_w = rect.right() - 8.0 - buttons_w.max(detail_w) - dot_w - name_x;
    let font = if spec.active { theme::medium(13.5) } else { FontId::proportional(13.5) };
    let g = one_line(&painter, spec.name, font, p.text, max_w);
    let name_w = g.size().x;
    painter.galley(Pos2::new(name_x, rect.center().y - g.size().y / 2.0), g, p.text);
    if spec.dirty {
        painter.circle_filled(Pos2::new(name_x + name_w + 8.0, rect.center().y), 3.0, p.accent);
    }
    if let Some(g) = detail {
        painter.galley(Pos2::new(rect.right() - 8.0 - g.size().x, rect.center().y - g.size().y / 2.0), g, p.text_faint);
    }
    let mut out = SidebarRowOut { row, more: None, action: None };
    if hovered {
        let br =
            Rect::from_min_max(Pos2::new(rect.right() - buttons_w, rect.top()), rect.right_bottom() - vec2(2.0, 0.0));
        let mut c = ui.new_child(UiBuilder::new().max_rect(br).layout(Layout::right_to_left(Align::Center)));
        c.spacing_mut().item_spacing.x = 0.0;
        let more = kit::icon_button_ex(&mut c, icons::DOTS_THREE, tr("More actions"), menu_open, 26.0);
        out.more = Some((more, popup_id));
        if let Some((icon, name)) = spec.action {
            out.action = Some(kit::icon_button_ex(&mut c, icon, name, false, 26.0));
        }
    }
    out
}

/// A centered confirmation dialog; `Some(true)` when confirmed, `Some(false)`
/// when dismissed.
pub(crate) fn confirm_dialog(ctx: &egui::Context, id: &str, title: &str, body: &str, confirm: &str) -> Option<bool> {
    let style = ctx.global_style();
    let p = theme::palette(style.visuals.dark_mode);
    let mut answer = None;
    let resp = egui::Modal::new(egui::Id::unique(id))
        .frame(
            egui::Frame::popup(&style)
                .inner_margin(egui::Margin::same(20))
                .corner_radius(CornerRadius::same(theme::RADIUS_LG)),
        )
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 18.0, theme::soft(p.err, 0.12));
                ui.painter().text(r.center(), Align2::CENTER_CENTER, icons::TRASH, theme::icon_font(18.0), p.err);
                ui.vertical(|ui| {
                    ui.label(RichText::new(title).font(theme::semibold(15.0)).color(p.text));
                    ui.add_space(2.0);
                    ui.label(RichText::new(body).color(p.text_muted));
                });
            });
            ui.add_space(18.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if kit::button_ex(ui, kit::Kind::Danger, Some(icons::TRASH), confirm, false).clicked() {
                    answer = Some(true);
                }
                if kit::secondary(ui, None, tr("Cancel")).clicked() {
                    answer = Some(false);
                }
            });
        });
    if answer.is_none() && resp.should_close() {
        answer = Some(false);
    }
    answer
}

/// The "Flows" sidebar list.
pub fn sidebar_list(app: &mut MerpatiApp, ui: &mut Ui) {
    let p = theme::pal(ui);
    let meta_id = egui::Id::unique("flow-list-meta");
    let stale = ui.data(|d| d.get_temp::<Arc<HashMap<PathBuf, FlowMeta>>>(meta_id)).is_none();
    if app.sidebar.flows_dirty || app.sidebar.flows.is_none() || stale {
        let (flows, errors) = merpati_flows::storage::list(app.ws.root());
        for e in errors {
            app.toast(e, true);
        }
        let meta: HashMap<PathBuf, FlowMeta> = flows
            .iter()
            .map(|f| {
                (f.path.clone(), FlowMeta { blocks: f.flow.blocks.len(), description: f.flow.description.clone() })
            })
            .collect();
        ui.data_mut(|d| d.insert_temp(meta_id, Arc::new(meta)));
        app.sidebar.flows = Some(flows.into_iter().map(|f| (f.flow.id, f.flow.name, f.path)).collect());
        app.sidebar.flows_dirty = false;
    }
    let meta: Arc<HashMap<PathBuf, FlowMeta>> = ui.data(|d| d.get_temp(meta_id)).unwrap_or_default();
    let flows = app.sidebar.flows.clone().unwrap_or_default();
    let mut act: Option<ListAct> = None;
    let delete_id = egui::Id::unique("flow-list-delete");
    let rename_id = egui::Id::unique("flow-list-rename");

    if flows.is_empty() {
        kit::empty_state(
            ui,
            icons::FLOW_ARROW,
            tr("No flows yet"),
            tr("Chain requests, conditions, loops and AI steps, then run them in one go."),
            |ui| {
                if kit::primary_icon(ui, icons::PLUS, tr("New flow")).clicked() {
                    act = Some(ListAct::New);
                }
            },
        );
    } else {
        let filter_id = egui::Id::unique("flow-list-filter");
        let mut filter: String = ui.data(|d| d.get_temp(filter_id)).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let w = ui.available_width() - 32.0;
            let r = kit::search_field(ui, "flow-filter", &mut filter, tr("Filter flows"), w);
            widgets::a11y_name(&r, tr("Filter flows"));
            if kit::icon_button(ui, icons::PLUS, tr("New flow")).clicked() {
                act = Some(ListAct::New);
            }
        });
        ui.data_mut(|d| d.insert_temp(filter_id, filter.clone()));
        ui.add_space(6.0);
        let active_path = app.tabs.get(app.active).and_then(|t| match &t.kind {
            TabKind::Flow(f) => f.path.clone(),
            _ => None,
        });
        let open: HashMap<PathBuf, (bool, bool)> = app
            .tabs
            .iter()
            .filter_map(|t| match &t.kind {
                TabKind::Flow(f) => f.path.clone().map(|p| (p, (f.is_running(), f.dirty()))),
                _ => None,
            })
            .collect();
        let mut renaming: Option<(String, String)> = ui.data(|d| d.get_temp(rename_id));
        let q = filter.trim().to_lowercase();
        egui::ScrollArea::vertical().id_salt("flow-list").auto_shrink([false; 2]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            let mut shown = 0;
            for (id, name, path) in &flows {
                if !q.is_empty() && !name.to_lowercase().contains(&q) {
                    continue;
                }
                shown += 1;
                if let Some((rid, buf)) = &mut renaming
                    && rid == id
                {
                    let r = ui.add(egui::TextEdit::singleline(buf).desired_width(ui.available_width()));
                    r.request_focus();
                    let escape = ui.input(|i| i.key_pressed(Key::Escape));
                    if r.lost_focus() || escape {
                        if !escape && !buf.trim().is_empty() && buf.trim() != name {
                            act = Some(ListAct::Rename(id.clone(), buf.trim().to_string()));
                        }
                        ui.data_mut(|d| {
                            d.remove_temp::<(String, String)>(rename_id);
                        });
                    }
                    continue;
                }
                let m = meta.get(path).cloned().unwrap_or_default();
                let (busy, dirty) = open.get(path).copied().unwrap_or_default();
                let detail = blocks_text(m.blocks);
                let out = sidebar_row(
                    ui,
                    id,
                    &SidebarRow {
                        icon: icons::FLOW_ARROW,
                        icon_color: p.text_muted,
                        busy,
                        name,
                        detail: &detail,
                        active: active_path.as_ref() == Some(path),
                        dirty,
                        action: Some((icons::PLAY, tr("Run"))),
                    },
                );
                let row =
                    if m.description.trim().is_empty() { out.row } else { out.row.on_hover_text(m.description.trim()) };
                if row.clicked() {
                    act = Some(ListAct::Open(path.clone()));
                }
                if row.double_clicked() {
                    ui.data_mut(|d| d.insert_temp(rename_id, (id.clone(), name.clone())));
                }
                if out.action.is_some_and(|r| r.clicked()) {
                    act = Some(ListAct::Run(path.clone()));
                }
                let mut menu = |ui: &mut Ui| {
                    if ui.button(kit::with_icon(icons::FOLDER_OPEN, tr("Open"))).clicked() {
                        act = Some(ListAct::Open(path.clone()));
                        ui.close();
                    }
                    if ui.button(kit::with_icon(icons::PLAY, tr("Run"))).clicked() {
                        act = Some(ListAct::Run(path.clone()));
                        ui.close();
                    }
                    if ui.button(kit::with_icon(icons::PENCIL_SIMPLE, tr("Rename"))).clicked() {
                        ui.data_mut(|d| d.insert_temp(rename_id, (id.clone(), name.clone())));
                        ui.close();
                    }
                    if ui.button(kit::with_icon(icons::COPY, tr("Duplicate"))).clicked() {
                        act = Some(ListAct::Duplicate(path.clone()));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(RichText::new(kit::with_icon(icons::TRASH, tr("Delete…"))).color(p.err)).clicked() {
                        act = Some(ListAct::Delete(id.clone(), name.clone(), path.clone()));
                        ui.close();
                    }
                };
                if let Some((more, popup_id)) = &out.more {
                    egui::Popup::menu(more).id(*popup_id).show(&mut menu);
                }
                row.context_menu(&mut menu);
            }
            if shown == 0 {
                ui.add_space(8.0);
                ui.label(RichText::new(trf("No flows match “{}”", &[&filter.trim()])).color(p.text_muted));
            }
            ui.add_space(12.0);
            ui.label(kit::hint(
                ui,
                tr("Flows are saved in flows/ inside the workspace and also run with `merpati flow run`."),
            ));
        });
    }

    // Delete confirmation.
    if let Some((id, name, path)) = ui.data(|d| d.get_temp::<(String, String, PathBuf)>(delete_id)) {
        match confirm_dialog(
            ui.ctx(),
            "flow-delete-confirm",
            &trf("Delete “{}”?", &[&name]),
            tr("The flow file is removed from the workspace. This cannot be undone."),
            tr("Delete flow"),
        ) {
            Some(true) => {
                ui.data_mut(|d| d.remove_temp::<(String, String, PathBuf)>(delete_id));
                if let Some(i) = app.tabs.iter().position(
                    |t| matches!(&t.kind, TabKind::Flow(f) if f.path.as_ref() == Some(&path) || f.flow.id == id),
                ) {
                    app.close_tab(i);
                }
                match merpati_flows::storage::delete(app.ws.root(), &id) {
                    Ok(_) => app.sidebar.flows_dirty = true,
                    Err(e) => app.toast(e.to_string(), true),
                }
            }
            Some(false) => {
                ui.data_mut(|d| d.remove_temp::<(String, String, PathBuf)>(delete_id));
            }
            None => {}
        }
    }

    match act {
        None => {}
        Some(ListAct::New) => {
            let n = app.sidebar.flows.as_ref().map(|f| f.len()).unwrap_or(0) + 1;
            let tab = FlowTab::blank(&format!("{} {n}", tr("Flow")));
            app.add_tab(TabKind::Flow(Box::new(tab)));
        }
        Some(ListAct::Open(path)) => open_flow(app, path),
        Some(ListAct::Run(path)) => {
            open_flow(app, path.clone());
            if let Some(TabKind::Flow(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind)
                && t.path.as_ref() == Some(&path)
            {
                t.auto_run = true;
            }
        }
        Some(ListAct::Rename(id, name)) => match merpati_flows::storage::rename(app.ws.root(), &id, &name) {
            Ok(path) => {
                for t in app.tabs.iter_mut() {
                    if let TabKind::Flow(f) = &mut t.kind
                        && f.flow.id == id
                    {
                        f.flow.name = name.clone();
                        f.saved.name = name.clone();
                        f.path = Some(path.clone());
                    }
                }
                app.sidebar.flows_dirty = true;
            }
            Err(e) => app.toast(e.to_string(), true),
        },
        Some(ListAct::Duplicate(path)) => match merpati_flows::storage::load(&path) {
            Ok(mut flow) => {
                flow.id = merpati_core::util::new_id();
                flow.name = trf("{} copy", &[&flow.name]);
                match merpati_flows::storage::save(app.ws.root(), &flow) {
                    Ok(p) => {
                        app.sidebar.flows_dirty = true;
                        open_flow(app, p);
                    }
                    Err(e) => app.toast(e.to_string(), true),
                }
            }
            Err(e) => app.toast(e.to_string(), true),
        },
        Some(ListAct::Delete(id, name, path)) => {
            ui.data_mut(|d| d.insert_temp(delete_id, (id, name, path)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(step: usize, block: &str) -> FlowMsg {
        FlowMsg::Event(FlowEvent::BlockStarted {
            step,
            block: block.into(),
            label: block.into(),
            kind: BlockKind::Delay,
            depth: 1,
            iteration: None,
            branch: Some(step.to_string()),
        })
    }

    fn finished(step: usize, block: &str) -> FlowMsg {
        FlowMsg::Event(FlowEvent::BlockFinished {
            step,
            block: block.into(),
            port: "next".into(),
            output: Arc::new(Value::Null),
            duration_ms: 1.0,
            failed: false,
            error: None,
            tests: Vec::new(),
        })
    }

    fn running(tab: &FlowTab, block: &str) -> bool {
        matches!(tab.run.status.get(block), Some(BlockStatus::Running { .. }))
    }

    #[test]
    fn parallel_blocks_run_at_once_and_ai_text_grows() {
        let mut flow = Flow::new("t");
        flow.add_block(Block::with_id("a", "A", BlockKind::Delay.default_config()));
        flow.add_block(Block::with_id("b", "B", BlockKind::Ai.default_config()));
        let mut tab = FlowTab::new(flow, None);
        // Two branches: "a" runs twice (steps 1 and 3), "b" once.
        for m in [started(1, "a"), started(2, "b"), started(3, "a")] {
            tab.on_msg(m);
        }
        assert!(running(&tab, "a") && running(&tab, "b"));
        for piece in ["Hel", "lo"] {
            tab.on_msg(FlowMsg::Event(FlowEvent::AiDelta { step: 2, block: "b".into(), text: piece.into() }));
        }
        assert_eq!(tab.run.live_text("b"), Some("Hello"));
        let last = tab.run.log.last().map(|l| (l.block.as_deref(), l.text.as_str()));
        assert_eq!(last, Some((Some("B"), "Hello")));
        tab.on_msg(finished(1, "a"));
        assert!(running(&tab, "a"), "step 3 still runs");
        tab.on_msg(finished(3, "a"));
        assert!(matches!(tab.run.status.get("a"), Some(BlockStatus::Done { .. })));
        tab.on_msg(finished(2, "b"));
        assert!(!running(&tab, "b"));
        assert_eq!(tab.run.live_text("b"), None);
        let answers = tab.run.log.iter().filter(|l| l.block.as_deref() == Some("B") && l.text == "Hello").count();
        assert_eq!(answers, 1, "the answer stays in the log");
        assert_eq!(tail("first line\nand the end", 7), "…the end");
    }

    #[test]
    fn fit_keeps_blocks_readable() {
        let mut flow = Flow::new("wide");
        for i in 0..10 {
            flow.add_block(Block::with_id(format!("b{i}"), "", BlockKind::Delay.default_config()));
            flow.blocks[i].position = (i as f32 * 280.0, 0.0);
        }
        let mut tab = FlowTab::new(flow, None);
        fit_view(&mut tab, Vec2::new(900.0, 500.0));
        assert!((tab.zoom - FIT_MIN_ZOOM).abs() < 1e-6, "zoom {}", tab.zoom);
        // Too wide: shown from the first block, vertically centered.
        assert!((tab.pan.x - 48.0).abs() < 1e-3);
        let small = Flow::new("small");
        let mut tab = FlowTab::new(small, None);
        tab.flow.add_block(Block::with_id("s", "", BlockKind::Start.default_config()));
        fit_view(&mut tab, Vec2::new(900.0, 500.0));
        assert_eq!(tab.zoom, 1.0);
        let h = block_height(BlockKind::Start);
        assert!((tab.pan.x - (900.0 - BLOCK_W) / 2.0).abs() < 1e-3 && (tab.pan.y - (500.0 - h) / 2.0).abs() < 1e-3);
        assert_eq!(clock(Duration::from_millis(61_234)), "01:01.234");
    }
}
