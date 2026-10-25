//! Scripted UI automation for visual checks and benchmarks.
//!
//! Inactive unless `MERPATI_AUTOMATION` points to a JSON array of steps, e.g.
//! `[{"open": "List users"}, {"send": true}, {"wait_ms": 800},
//!   {"screenshot": "/tmp/a.png"}, {"quit": true}]`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::app::{ItemRef, MerpatiApp, SidebarTab, TabKind};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum Step {
    Open(String),
    /// Opens a new, empty request tab (its URL field gets the caret).
    NewRequest(bool),
    /// Shows the response body as `pretty`, `raw`, `preview` or `tree`.
    BodyView(String),
    /// Panics on the UI thread (checks the crash report).
    Panic(String),
    Send(bool),
    /// Presses the Send button of the active request tab.
    ClickSend(bool),
    WaitMs(u64),
    /// Waits until no request in the active tab is running.
    WaitIdle(bool),
    Screenshot(String),
    Section(String),
    RespSection(String),
    Sidebar(String),
    Dialog(String),
    CloseDialog(bool),
    Runner(String),
    RunnerStart(bool),
    Mock(String),
    Container(String),
    Environment(Option<String>),
    ActivateEnv(String),
    Layout(String),
    Theme(String),
    Language(String),
    Console(bool),
    Url(String),
    /// New unsaved gRPC request with this URL.
    NewGrpc(String),
    GrpcLoad(bool),
    /// `package.Service/Method`.
    GrpcMethod(String),
    GrpcInvoke(bool),
    /// New unsaved request from a "New …" template, by protocol name (`MCP`).
    NewProto(String),
    /// Replaces the request of the active (unsaved) tab, as `RequestKind` JSON.
    SetRequest(serde_json::Value),
    /// Sets the scripts of the active tab's unsaved request.
    SetScripts(merpati_core::model::Scripts),
    /// Protocol tab action: `connect`, `emit`, `send`, `start`, `select`, `call:<tool>:<json>`.
    Auto(String),
    /// New webhook listener tab on this port.
    NewWebhook(u16),
    /// New capture proxy tab on this port, started right away.
    NewCapture(u16),
    /// Opens the flow with this name (from `flows/`).
    OpenFlow(String),
    /// Runs the flow in the active tab.
    RunFlow(bool),
    /// Forks the collection with this name (as "<name> (fork)").
    Fork(String),
    /// Opens the pull (true) or merge (false) dialog of the fork with this name.
    ForkSync((String, bool)),
    /// Opens the first spec in `specs/` whose file name contains the text.
    OpenSpec(String),
    /// Sets a GraphQL body (scratch tabs).
    GraphqlQuery(String),
    Expand(String),
    Window([f32; 2]),
    /// Prints resident memory to stderr (benchmarks).
    ReportMemory(String),
    /// Presses the main button of the update pop-up (install and restart,
    /// download, or release page) once the startup check found an update
    /// (needs `MERPATI_UPDATE_URL`; waits up to 20 s).
    UpdateInstall(bool),
    Quit(bool),
}

pub struct Automation {
    steps: VecDeque<Step>,
    wait_until: Option<Instant>,
    pending_shot: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    started: Instant,
}

impl Automation {
    pub fn from_env() -> Option<Automation> {
        let path = std::env::var("MERPATI_AUTOMATION").ok()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let steps: Vec<Step> = match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("automation: {e}");
                return None;
            }
        };
        Some(Automation { steps: steps.into(), wait_until: None, pending_shot: None, started: Instant::now() })
    }
}

fn find_request(app: &MerpatiApp, name: &str) -> Option<ItemRef> {
    for c in &app.ws.data.collections {
        for r in c.requests() {
            if r.name == name || r.id == name {
                return Some(ItemRef { collection: c.id.clone(), item: r.id.clone() });
            }
        }
    }
    None
}

fn find_collection(app: &MerpatiApp, name: &str) -> Option<String> {
    app.ws.data.collections.iter().find(|c| c.name == name || c.id == name).map(|c| c.id.clone())
}

/// Physical footprint as shown by Activity Monitor (macOS `footprint`).
pub fn footprint_mb() -> Option<f64> {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("footprint").arg(&pid).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains("Footprint:"))?;
    let after = line.split("Footprint:").nth(1)?.trim();
    let mut parts = after.split_whitespace();
    let value: f64 = parts.next()?.parse().ok()?;
    Some(match parts.next()? {
        "KB" => value / 1024.0,
        "GB" => value * 1024.0,
        _ => value,
    })
}

pub fn resident_memory_mb() -> f64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

pub fn tick(app: &mut MerpatiApp, ctx: &egui::Context) {
    let Some(mut auto) = app.automation.take() else { return };
    ctx.request_repaint_after(Duration::from_millis(30));
    // Wait for a pending screenshot to be written.
    if let Some(flag) = &auto.pending_shot {
        if !flag.load(std::sync::atomic::Ordering::SeqCst) {
            app.automation = Some(auto);
            return;
        }
        auto.pending_shot = None;
    }
    if let Some(t) = auto.wait_until {
        if Instant::now() < t {
            app.automation = Some(auto);
            return;
        }
        auto.wait_until = None;
    }
    let Some(step) = auto.steps.pop_front() else {
        app.automation = Some(auto);
        return;
    };
    app.mark_session_dirty();
    match step {
        Step::Open(name) => match find_request(app, &name) {
            Some(r) => app.open_item(r),
            None => eprintln!("automation: request {name} not found"),
        },
        Step::Send(_) => {
            if let Some(t) = app.tabs.get(app.active) {
                let id = t.id;
                app.send(id);
            }
        }
        Step::ClickSend(_) => {
            if let Some(TabKind::Request(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind) {
                t.click_send = true;
            }
        }
        Step::WaitMs(ms) => auto.wait_until = Some(Instant::now() + Duration::from_millis(ms)),
        Step::WaitIdle(_) => {
            let busy = app.tabs.iter().any(|t| match &t.kind {
                TabKind::Request(r) => r.running.is_some(),
                TabKind::Runner(r) => r.is_running(),
                _ => false,
            });
            if busy {
                auto.steps.push_front(Step::WaitIdle(true));
                auto.wait_until = Some(Instant::now() + Duration::from_millis(50));
            }
        }
        Step::Screenshot(path) => {
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = done.clone();
            let repaint = ctx.clone();
            let cb = egui::ScreenshotCallback::new(move |image| {
                let [w, h] = image.size;
                let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                match image::RgbaImage::from_raw(w as u32, h as u32, bytes).map(|b| b.save(&path)) {
                    Some(Ok(())) => eprintln!("automation: saved {path}"),
                    other => eprintln!("automation: screenshot {path} failed: {other:?}"),
                }
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                repaint.request_repaint();
            });
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(cb));
            auto.pending_shot = Some(done);
        }
        Step::Section(s) => {
            if let Some(TabKind::Request(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind) {
                t.section = match s.as_str() {
                    "params" => crate::request_tab::Section::Params,
                    "auth" => crate::request_tab::Section::Auth,
                    "headers" => crate::request_tab::Section::Headers,
                    "body" => crate::request_tab::Section::Body,
                    "scripts" => crate::request_tab::Section::Scripts,
                    "settings" => crate::request_tab::Section::Settings,
                    _ => crate::request_tab::Section::Docs,
                };
            }
            if s == "scripts"
                && let Some(TabKind::Grpc(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind)
            {
                t.show_scripts();
            }
        }
        Step::RespSection(s) => {
            if let Some(TabKind::Request(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind) {
                t.resp_section = match s.as_str() {
                    "headers" => crate::request_tab::RespSection::Headers,
                    "cookies" => crate::request_tab::RespSection::Cookies,
                    "tests" => crate::request_tab::RespSection::Tests,
                    "details" => crate::request_tab::RespSection::Details,
                    "visualize" => crate::request_tab::RespSection::Visualize,
                    _ => crate::request_tab::RespSection::Body,
                };
            }
        }
        Step::Sidebar(s) => {
            app.sidebar_tab = match s.as_str() {
                "environments" => SidebarTab::Environments,
                "history" => SidebarTab::History,
                "apis" => SidebarTab::Apis,
                "flows" => SidebarTab::Flows,
                _ => SidebarTab::Collections,
            }
        }
        Step::Dialog(d) => {
            app.dialog = Some(match d.as_str() {
                "settings" => crate::dialogs::Dialog::settings(&app.settings),
                "import" => crate::dialogs::Dialog::import(),
                "cookies" => crate::dialogs::Dialog::cookies(),
                "vault" => crate::dialogs::Dialog::vault(),
                "requests" => crate::dialogs::Dialog::palette(true),
                "quicklook" => crate::dialogs::Dialog::quick_look(),
                _ => crate::dialogs::Dialog::palette(false),
            });
        }
        Step::CloseDialog(_) => app.dialog = None,
        Step::Runner(name) => {
            if let Some(c) = find_collection(app, &name) {
                app.open_runner(c, None);
            }
        }
        Step::RunnerStart(_) => {
            if let Some(tab) = app.tabs.get(app.active)
                && matches!(tab.kind, TabKind::Runner(_))
            {
                app.pending_runner_start = Some(tab.id);
            }
        }
        Step::Mock(name) => {
            if let Some(c) = find_collection(app, &name) {
                app.open_mock(c);
            }
        }
        Step::Container(name) => {
            if let Some(c) = find_collection(app, &name) {
                app.open_container(c, None);
            }
        }
        Step::Environment(name) => {
            let id = name.and_then(|n| app.ws.data.environments.iter().find(|e| e.name == n).map(|e| e.id.clone()));
            app.open_environment(id);
        }
        Step::ActivateEnv(name) => {
            let id = app.ws.data.environments.iter().find(|e| e.name == name).map(|e| e.id.clone());
            app.ws.set_active_environment(id);
        }
        Step::Layout(l) => {
            app.settings.ui.layout = if l == "side" {
                merpati_core::settings::ResponseLayout::Side
            } else {
                merpati_core::settings::ResponseLayout::Below
            }
        }
        Step::Theme(t) => {
            let mut s = app.settings.clone();
            s.ui.theme = match t.as_str() {
                "light" => merpati_core::settings::ThemePreference::Light,
                "dark" => merpati_core::settings::ThemePreference::Dark,
                _ => merpati_core::settings::ThemePreference::System,
            };
            app.apply_settings(s);
        }
        Step::Language(l) => {
            let mut s = app.settings.clone();
            s.ui.language =
                if l == "id" { merpati_core::settings::Language::Id } else { merpati_core::settings::Language::En };
            app.apply_settings(s);
        }
        Step::Console(on) => app.console_open = on,
        Step::Url(url) => {
            let loc = app.tabs.get(app.active).and_then(|t| t.kind.location().cloned());
            if let Some(loc) = loc {
                if let Some(h) = app
                    .ws
                    .data
                    .collection_mut(&loc.collection)
                    .and_then(|c| c.find_request_mut(&loc.item))
                    .and_then(|r| r.http_mut())
                {
                    h.set_url(url);
                    app.ws.mark_collection(&loc.collection);
                }
            } else if let Some(TabKind::Request(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind)
                && let Some(item) = t.scratch.as_mut().and_then(|i| i.http_mut())
            {
                item.set_url(url);
            }
        }
        Step::GraphqlQuery(q) => {
            if let Some(TabKind::Request(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind)
                && let Some(item) = t.scratch.as_mut().and_then(|i| i.http_mut())
            {
                item.method = "POST".into();
                item.body.mode = merpati_core::model::BodyMode::GraphQl;
                item.body.graphql.query = q;
                t.section = crate::request_tab::Section::Body;
            }
        }
        Step::OpenSpec(name) => {
            let found = crate::spec_tab::list_specs(app.ws.root())
                .into_iter()
                .find(|p| p.file_name().map(|f| f.to_string_lossy().contains(&name)).unwrap_or(false));
            if let Some(p) = found {
                crate::spec_tab::open_spec(app, p);
            }
        }
        Step::NewProto(name) => {
            if let Some((_, n, kind)) =
                crate::app::protocol_templates().into_iter().find(|(_, n, _)| n.eq_ignore_ascii_case(&name))
            {
                app.new_proto_tab(n, kind);
            }
        }
        Step::SetRequest(v) => match serde_json::from_value::<merpati_core::model::RequestKind>(v) {
            Ok(kind) => {
                if let Some(Some(item)) = app.tabs.get_mut(app.active).and_then(|t| t.kind.scratch_mut()) {
                    item.request = kind;
                }
            }
            Err(e) => eprintln!("automation: set_request: {e}"),
        },
        Step::SetScripts(scripts) => {
            if let Some(Some(item)) = app.tabs.get_mut(app.active).and_then(|t| t.kind.scratch_mut()) {
                item.scripts = scripts;
            }
        }
        Step::Auto(a) => match app.tabs.get_mut(app.active).map(|t| &mut t.kind) {
            Some(TabKind::Mcp(t)) => t.auto = Some(a),
            Some(TabKind::SocketIo(t)) => t.auto = Some(a),
            Some(TabKind::Ai(t)) => t.auto = Some(a),
            Some(TabKind::Webhook(t)) => t.auto = Some(a),
            Some(TabKind::Capture(t)) if a == "select" => t.select_latest(),
            Some(TabKind::Spec(t)) if a == "preview" => t.show_preview(),
            _ => {}
        },
        Step::NewCapture(port) => {
            let mut t = crate::capture_tab::CaptureTab::with_port(port);
            t.auto_start = true;
            app.add_tab(TabKind::Capture(Box::new(t)));
        }
        Step::OpenFlow(name) => {
            let found = merpati_flows::storage::list(app.ws.root()).0.into_iter().find(|f| f.flow.name == name);
            if let Some(f) = found {
                crate::flow_tab::open_flow(app, f.path);
            }
        }
        Step::RunFlow(_) => {
            if let Some(TabKind::Flow(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind) {
                t.auto_run = true;
            }
        }
        Step::Fork(name) => {
            if let Some(c) = find_collection(app, &name) {
                crate::fork_ui::create(app, &c, &format!("{name} (fork)"));
            }
        }
        Step::ForkSync((name, pull)) => {
            if let Some(c) = find_collection(app, &name)
                && let Some(s) = crate::fork_ui::ForkSync::new(app, &c, pull)
            {
                app.dialog = Some(crate::dialogs::Dialog::ForkSync(Box::new(s)));
            }
        }
        Step::NewWebhook(port) => {
            let mut t = crate::webhook_tab::WebhookTab::default();
            t.set_port(port);
            app.add_tab(TabKind::Webhook(Box::new(t)));
        }
        Step::NewGrpc(url) => {
            let req = merpati_core::protocols::GrpcRequest { url, ..Default::default() };
            app.new_proto_tab("gRPC", merpati_core::model::RequestKind::Grpc(req));
        }
        Step::GrpcLoad(_) => {
            if let Some(TabKind::Grpc(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind) {
                t.auto_load = true;
            }
        }
        Step::GrpcMethod(m) => {
            if let Some(TabKind::Grpc(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind) {
                let (svc, method) = m.split_once('/').unwrap_or((&m, ""));
                let template = t.schema().and_then(|s| s.method(svc, method)).map(|m| m.input_template);
                if let Some(item) = t.slot.scratch.as_mut()
                    && let merpati_core::model::RequestKind::Grpc(g) = &mut item.request
                {
                    g.service = svc.to_string();
                    g.method = method.to_string();
                    if let Some(tpl) = template {
                        g.message = tpl.replace("\"\"", "\"Merpati\"");
                    }
                }
            }
        }
        Step::GrpcInvoke(_) => {
            if let Some(TabKind::Grpc(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind) {
                t.auto_invoke = true;
            }
        }
        Step::Expand(name) => {
            if let Some(c) = find_collection(app, &name) {
                app.sidebar.expanded.insert(c.clone());
                if let Some(col) = app.ws.data.collection(&c) {
                    fn all_folders(items: &[merpati_core::model::Item], out: &mut Vec<String>) {
                        for i in items {
                            if let merpati_core::model::Item::Folder(f) = i {
                                out.push(f.id.clone());
                                all_folders(&f.items, out);
                            }
                        }
                    }
                    let mut f = Vec::new();
                    all_folders(&col.items, &mut f);
                    app.sidebar.expanded.extend(f);
                }
            }
        }
        Step::Window([w, h]) => ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h))),
        Step::ReportMemory(label) => {
            let since_start = crate::START.get().map(|s| s.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0);
            eprintln!(
                "automation: memory[{label}] = rss {:.1} MB, footprint {} MB, {:.0} ms since process start ({:.0} ms since first frame)",
                resident_memory_mb(),
                footprint_mb().map(|f| format!("{f:.0}")).unwrap_or_else(|| "?".into()),
                since_start,
                auto.started.elapsed().as_secs_f64() * 1000.0
            );
        }
        Step::NewRequest(_) => app.new_request_tab(),
        Step::Panic(message) => panic!("{message}"),
        Step::BodyView(v) => {
            if let Some(TabKind::Request(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind)
                && let Some(r) = t.result.as_mut()
            {
                use crate::body_view::BodyView;
                r.body.view = match v.as_str() {
                    "raw" => BodyView::Raw,
                    "preview" => BodyView::Preview,
                    "tree" => BodyView::Tree,
                    _ => BodyView::Pretty,
                };
            }
        }
        Step::UpdateInstall(_) => {
            if app.update.available.is_some() {
                crate::update::press_primary(app);
            } else if auto.started.elapsed() < Duration::from_secs(20) {
                // The startup check has not answered yet.
                auto.steps.push_front(Step::UpdateInstall(true));
                auto.wait_until = Some(Instant::now() + Duration::from_millis(100));
            } else {
                eprintln!("automation: no update to install");
            }
        }
        Step::Quit(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
    }
    app.automation = Some(auto);
}
