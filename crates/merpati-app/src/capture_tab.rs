//! Traffic capture through a local proxy (PRD section 4, "Penangkapan lalu
//! lintas lewat proxy"): point a browser, app or device at the proxy, watch
//! requests live, then save them to a collection or export HAR.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use egui::{RichText, Ui};
use parking_lot::Mutex;

use merpati_core::model::Id;
use merpati_core::secrets::{FileBackend, KeychainBackend, SecretStore};
use merpati_proxy::{
    CaPem, CaStore, CaptureEntry, CaptureFilter, CaptureProxy, CertificateAuthority, EntryKind, EntryState,
    ImportOptions, ProxyConfig,
};

use crate::app::{MerpatiApp, Msg};
use crate::i18n::{tr, trf};
use crate::proto_common::{self as pc, Conn, ReqRow};
use crate::{icons, kit, theme, widgets};

pub enum CaptureMsg {
    Started(Result<CaptureProxy, String>),
    Trusted(bool),
}

/// The capture CA lives in the Keychain (one for the app, not per workspace),
/// so trusting it once covers every workspace.
struct KeychainCa(SecretStore);

impl KeychainCa {
    fn open() -> KeychainCa {
        let file = || FileBackend { path: merpati_core::settings::AppSettings::config_dir().join("capture-ca.json") };
        let store = match std::env::var("MERPATI_SECRETS").as_deref() {
            Ok("file") => SecretStore::new(Box::new(file())),
            Ok("memory") => SecretStore::memory(),
            _ if KeychainBackend::available() => {
                SecretStore::new(Box::new(KeychainBackend::for_workspace("capture-proxy")))
            }
            _ => SecretStore::new(Box::new(file())),
        };
        KeychainCa(store)
    }
}

impl CaStore for KeychainCa {
    fn load(&self) -> Result<Option<CaPem>, String> {
        Ok(match (self.0.get("ca_cert"), self.0.get("ca_key")) {
            (Some(cert_pem), Some(key_pem)) => Some(CaPem { cert_pem, key_pem }),
            _ => None,
        })
    }

    fn save(&self, pem: &CaPem) -> Result<(), String> {
        self.0.set("ca_cert", &pem.cert_pem);
        self.0.set("ca_key", &pem.key_pem);
        Ok(())
    }
}

pub struct CaptureTab {
    proxy: Option<CaptureProxy>,
    starting: bool,
    port: u16,
    intercept: bool,
    verify_upstream: bool,
    allow_lan: bool,
    passthrough: String,
    include_hosts: String,
    exclude_hosts: String,
    url_contains: String,
    search: String,
    selected: Option<u64>,
    checked: BTreeSet<u64>,
    target: Option<Id>,
    ca: Option<Arc<CertificateAuthority>>,
    trusted: Arc<Mutex<Option<bool>>>,
    error: Option<String>,
    show_ca: bool,
    /// Automation: start on the next frame.
    pub auto_start: bool,
}

impl Default for CaptureTab {
    fn default() -> Self {
        CaptureTab {
            proxy: None,
            starting: false,
            port: merpati_proxy::DEFAULT_PORT,
            intercept: true,
            verify_upstream: true,
            allow_lan: false,
            passthrough: String::new(),
            include_hosts: String::new(),
            exclude_hosts: String::new(),
            url_contains: String::new(),
            search: String::new(),
            selected: None,
            checked: BTreeSet::new(),
            target: None,
            ca: None,
            trusted: Arc::new(Mutex::new(None)),
            error: None,
            show_ca: false,
            auto_start: false,
        }
    }
}

fn list(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

impl CaptureTab {
    pub fn with_port(port: u16) -> Self {
        CaptureTab { port, ..Default::default() }
    }

    /// Automation: select the newest entry.
    pub fn select_latest(&mut self) {
        self.selected = self
            .proxy
            .as_ref()
            .and_then(|p| p.entries().iter().rev().find(|e| e.kind != EntryKind::Tunnel).map(|e| e.id));
    }

    pub fn on_msg(&mut self, m: CaptureMsg) {
        match m {
            CaptureMsg::Started(r) => {
                self.starting = false;
                match r {
                    Ok(p) => {
                        self.error = None;
                        self.proxy = Some(p);
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            CaptureMsg::Trusted(t) => *self.trusted.lock() = Some(t),
        }
    }

    pub fn is_running(&self) -> bool {
        self.proxy.is_some()
    }

    pub fn stop(&mut self, rt: &tokio::runtime::Runtime) {
        if let Some(p) = self.proxy.take() {
            rt.spawn(p.stop());
        }
    }

    fn filter(&self) -> CaptureFilter {
        CaptureFilter {
            include_hosts: list(&self.include_hosts),
            exclude_hosts: list(&self.exclude_hosts),
            url_contains: self.url_contains.trim().to_string(),
            ..Default::default()
        }
    }

    fn ensure_ca(&mut self) -> Result<Arc<CertificateAuthority>, String> {
        if let Some(ca) = &self.ca {
            return Ok(ca.clone());
        }
        let (ca, _created) = CertificateAuthority::load_or_generate(&KeychainCa::open()).map_err(|e| e.to_string())?;
        let ca = Arc::new(ca);
        self.ca = Some(ca.clone());
        Ok(ca)
    }
}

fn check_trust(app: &MerpatiApp, tab_id: u64, ca: Arc<CertificateAuthority>) {
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    app.rt.spawn_blocking(move || {
        let _ = tx.send(Msg::Capture { tab: tab_id, event: CaptureMsg::Trusted(ca.is_trusted_by_system()) });
        ctx.request_repaint();
    });
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut CaptureTab) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    if std::mem::take(&mut tab.auto_start) && tab.proxy.is_none() && !tab.starting {
        start(app, tab_id, tab);
    }
    let recording = tab.proxy.as_ref().map(|p| p.is_recording());
    let (state, state_text) = match recording {
        Some(true) => (Conn::Live, tr("Recording")),
        Some(false) => (Conn::Idle, tr("Paused")),
        None if tab.starting => (Conn::Busy, tr("Starting…")),
        None if tab.error.is_some() => (Conn::Failed, tr("Not started")),
        None => (Conn::Idle, tr("Stopped")),
    };
    let mut do_start = false;
    let mut do_stop = false;
    pc::view_title(
        ui,
        icons::BROADCAST,
        tr("Capture traffic"),
        Some(tr(
            "Set this proxy in a browser, app or device to record its requests. HTTPS needs the Merpati certificate to be trusted.",
        )),
        |ui| {
            pc::status_pill(ui, state, state_text);
        },
        |ui| {
            if tab.proxy.is_some() {
                do_stop = kit::secondary(ui, Some(icons::STOP), tr("Stop")).clicked();
            } else if tab.starting {
                ui.add(egui::Spinner::new().size(18.0).color(p.accent));
            } else {
                do_start = kit::primary_icon(ui, icons::PLAY, tr("Start proxy")).clicked();
            }
        },
    );
    if do_stop {
        tab.stop(&app.rt);
    }
    if do_start {
        start(app, tab_id, tab);
    }
    if let Some(proxy) = &tab.proxy {
        let url = proxy.url();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if pc::address_pill(ui, &url) {
                app.toast(tr("Copied"), false);
            }
            let rec = proxy.is_recording();
            let (icon, label) = if rec { (icons::PAUSE, tr("Pause")) } else { (icons::PLAY, tr("Resume")) };
            if kit::secondary(ui, Some(icon), label)
                .on_hover_text(if rec { tr("Stop recording; traffic still flows") } else { tr("Record traffic again") })
                .clicked()
            {
                proxy.set_recording(!rec);
            }
            if proxy.is_intercepting()
                && kit::icon_toggle(ui, icons::SHIELD_CHECK, tr("Certificate"), tab.show_ca).clicked()
            {
                tab.show_ca = !tab.show_ca;
            }
            let rejected = proxy.tls_failed_hosts();
            if !rejected.is_empty() {
                kit::pill(ui, &format!("{} {}", icons::WARNING, tr("Certificate rejected")), p.warn)
                    .on_hover_text(format!("{}: {}", tr("Certificate rejected by"), rejected.join(", ")));
            }
        });
        ui.ctx().request_repaint_after(Duration::from_millis(400));
    }
    if let Some(e) = tab.error.clone() {
        ui.add_space(8.0);
        let mut dismiss = false;
        pc::error_banner(ui, tr("Could not start the proxy"), &e, |ui| {
            dismiss = kit::icon_button(ui, icons::X, tr("Dismiss")).clicked();
        });
        if dismiss {
            tab.error = None;
        }
    }
    ui.add_space(12.0);
    options(ui, tab);
    if tab.show_ca {
        ui.add_space(10.0);
        ca_panel(app, ui, tab_id, tab);
    }
    ui.add_space(12.0);
    let entries: Vec<Arc<CaptureEntry>> = tab.proxy.as_ref().map(|p| p.entries()).unwrap_or_default();
    actions(app, ui, tab, &entries);
    ui.add_space(6.0);
    if entries.is_empty() {
        match &tab.proxy {
            Some(proxy) => {
                let addr = proxy.url();
                kit::empty_state(
                    ui,
                    icons::HOURGLASS,
                    tr("Waiting for traffic"),
                    &trf("Set {} as the HTTP and HTTPS proxy of a browser, app or device.", &[&addr]),
                    |_| {},
                );
            }
            None => kit::empty_state(
                ui,
                icons::BROADCAST,
                tr("The proxy is stopped"),
                tr("Start it, then point a browser, app or device at it to record their requests."),
                |_| {},
            ),
        }
        return;
    }
    let q = tab.search.to_lowercase();
    let shown: Vec<&Arc<CaptureEntry>> = entries
        .iter()
        .filter(|e| q.is_empty() || e.url.to_lowercase().contains(&q) || e.method.to_lowercase() == q)
        .collect();
    let list_w = (ui.available_width() * 0.5).clamp(320.0, 640.0);
    pc::list_panel(ui, ("capture-list", tab_id), list_w, |ui| {
        pc::request_header(ui, true, true, "URL", tr("Size"));
        if shown.is_empty() {
            ui.add_space(16.0);
            ui.label(RichText::new(tr("No requests match")).color(p.text_muted));
            return;
        }
        egui::ScrollArea::vertical().id_salt("capture-list").stick_to_bottom(true).auto_shrink([false; 2]).show_rows(
            ui,
            30.0,
            shown.len(),
            |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for e in &shown[range] {
                    let time = e.started_at.with_timezone(&chrono::Local).format("%H:%M:%S").to_string();
                    let status = match (e.status, e.state, e.kind) {
                        (_, _, EntryKind::Tunnel) => Some((tr("tunnel").to_string(), p.text_muted)),
                        (Some(s), _, _) => Some((s.to_string(), theme::status_color(s, dark))),
                        (None, EntryState::Failed, _) => Some((tr("failed").to_string(), p.err)),
                        (None, _, _) => Some(("···".to_string(), p.text_muted)),
                    };
                    let path = if e.kind == EntryKind::Tunnel { String::new() } else { e.path().to_string() };
                    let size = if e.kind == EntryKind::Tunnel {
                        String::new()
                    } else {
                        merpati_core::util::human_bytes(e.response_body.size)
                    };
                    let row =
                        ReqRow { time: &time, method: &e.method, status, path: &e.host, detail: &path, right: size };
                    let mut on = tab.checked.contains(&e.id);
                    let was = on;
                    let check = (e.kind != EntryKind::Tunnel).then_some(&mut on);
                    if pc::request_row(ui, &row, tab.selected == Some(e.id), check).clicked() {
                        tab.selected = Some(e.id);
                    }
                    if on != was {
                        if on {
                            tab.checked.insert(e.id);
                        } else {
                            tab.checked.remove(&e.id);
                        }
                    }
                }
            },
        );
    });
    match tab.selected.and_then(|id| entries.iter().find(|e| e.id == id)) {
        Some(e) => detail(ui, e, dark),
        None => kit::empty_state(
            ui,
            icons::ARROW_LEFT,
            tr("Select a request"),
            tr("Its headers, bodies and timings appear here."),
            |_| {},
        ),
    }
}

/// Proxy options (fixed while it runs) and capture filters.
fn options(ui: &mut Ui, tab: &mut CaptureTab) {
    let p = theme::pal(ui);
    let running = tab.proxy.is_some();
    let mut apply = false;
    // Open while stopped; folds into a summary once the proxy runs (it can
    // then only change its filters), leaving room for the traffic.
    let open_id = ui.make_persistent_id(("capture-options-open", running));
    let mut open: bool = ui.data(|d| d.get_temp(open_id).unwrap_or(!running));
    let filters =
        [&tab.include_hosts, &tab.exclude_hosts, &tab.url_contains].iter().filter(|f| !f.trim().is_empty()).count();
    let summary = format!(
        "{} {} · {} · {}",
        tr("Port"),
        tab.port,
        if tab.intercept { tr("Decrypt HTTPS") } else { tr("HTTPS passed through") },
        if filters == 0 { tr("no filters").to_string() } else { trf("{} filters", &[&filters]) }
    );
    let header = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let caret = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
            ui.label(RichText::new(caret).font(theme::icon_font(12.0)).color(p.text_muted));
            ui.label(kit::section_title(tr("Options")));
            if !open {
                ui.label(RichText::new(&summary).color(p.text_muted).small());
            }
        })
        .response
        .interact(egui::Sense::click());
    widgets::a11y_button(&header, tr("Options"));
    if header.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if header.clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(open_id, open));
    }
    if !open {
        return;
    }
    ui.add_space(4.0);
    kit::card(ui).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 8.0;
        let muted = |ui: &mut Ui, t: &str| {
            ui.label(RichText::new(t).color(p.text_muted));
        };
        pc::flow_row(
            ui,
            tr("Proxy"),
            110.0,
            vec![
                Box::new(|ui: &mut Ui| {
                    muted(ui, tr("Port"));
                    ui.add_enabled(!running, egui::DragValue::new(&mut tab.port).range(1..=65535));
                }),
                Box::new(|ui: &mut Ui| {
                    ui.add_enabled(!running, egui::Checkbox::new(&mut tab.intercept, tr("Decrypt HTTPS")));
                }),
                Box::new(|ui: &mut Ui| {
                    ui.add_enabled(
                        !running,
                        egui::Checkbox::new(&mut tab.verify_upstream, tr("Verify server certificates")),
                    );
                }),
                Box::new(|ui: &mut Ui| {
                    ui.add_enabled(
                        !running,
                        egui::Checkbox::new(&mut tab.allow_lan, tr("Allow other devices on the network")),
                    )
                    .on_hover_text(tr("Listens on all interfaces; anyone on the network can use the proxy"));
                }),
            ],
        );
        let has_proxy = tab.proxy.is_some();
        pc::flow_row(
            ui,
            tr("Record"),
            110.0,
            vec![
                Box::new(|ui: &mut Ui| {
                    muted(ui, tr("Only hosts"));
                    ui.add(
                        egui::TextEdit::singleline(&mut tab.include_hosts)
                            .hint_text("*.example.com")
                            .desired_width(150.0),
                    );
                }),
                Box::new(|ui: &mut Ui| {
                    muted(ui, tr("Skip hosts"));
                    ui.add(
                        egui::TextEdit::singleline(&mut tab.exclude_hosts)
                            .hint_text("*.apple.com")
                            .desired_width(150.0),
                    );
                }),
                Box::new(|ui: &mut Ui| {
                    muted(ui, tr("URL contains"));
                    ui.add(egui::TextEdit::singleline(&mut tab.url_contains).hint_text("/api/").desired_width(110.0));
                }),
                Box::new(|ui: &mut Ui| {
                    if has_proxy && kit::ghost(ui, Some(icons::FUNNEL), tr("Apply filters")).clicked() {
                        apply = true;
                    }
                }),
            ],
        );
        pc::flow_row(
            ui,
            tr("Never decrypt"),
            110.0,
            vec![Box::new(|ui: &mut Ui| {
                ui.add_enabled(
                    !running,
                    egui::TextEdit::singleline(&mut tab.passthrough)
                        .hint_text("bank.example.com, *.icloud.com")
                        .desired_width(300.0),
                )
                .on_hover_text(tr("Hosts whose HTTPS is passed through untouched"));
            })],
        );
        if running {
            ui.label(kit::hint(ui, tr("Stop the proxy to change its port and HTTPS options.")));
        }
    });
    if apply && let Some(proxy) = &tab.proxy {
        proxy.set_filter(tab.filter());
    }
}

fn start(app: &mut MerpatiApp, tab_id: u64, tab: &mut CaptureTab) {
    let ca = if tab.intercept {
        match tab.ensure_ca() {
            Ok(ca) => Some(ca),
            Err(e) => {
                tab.error = Some(format!("{}: {e}", tr("Certificate")));
                return;
            }
        }
    } else {
        None
    };
    let host = if tab.allow_lan { std::net::Ipv4Addr::UNSPECIFIED } else { std::net::Ipv4Addr::LOCALHOST };
    let config = ProxyConfig {
        bind: std::net::SocketAddr::from((host, tab.port)),
        allow_lan: tab.allow_lan,
        intercept_tls: ca.is_some(),
        ca: ca.clone(),
        verify_upstream_tls: tab.verify_upstream,
        passthrough_hosts: list(&tab.passthrough),
        filter: tab.filter(),
        ..Default::default()
    };
    tab.starting = true;
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    app.rt.spawn(async move {
        let r = CaptureProxy::start(config).await.map_err(|e| e.to_string());
        let _ = tx.send(Msg::Capture { tab: tab_id, event: CaptureMsg::Started(r) });
        ctx.request_repaint();
    });
    if let Some(ca) = ca {
        check_trust(app, tab_id, ca);
    }
}

fn ca_panel(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut CaptureTab) {
    let p = theme::pal(ui);
    let Ok(ca) = tab.ensure_ca() else { return };
    kit::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(icons::SHIELD_CHECK).font(theme::icon_font(18.0)).color(p.accent_text));
            ui.label(RichText::new(tr("Merpati capture certificate")).font(theme::semibold(14.0)));
            match *tab.trusted.lock() {
                Some(true) => kit::pill(ui, &format!("{} {}", icons::CHECK, tr("Trusted by this Mac")), p.ok),
                Some(false) => kit::pill(ui, &format!("{} {}", icons::WARNING, tr("Not trusted yet")), p.warn)
                    .on_hover_text(tr("Not trusted yet: HTTPS clients will reject the proxy")),
                None => pc::status_pill(ui, Conn::Busy, tr("Checking trust…")),
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if kit::icon_button(ui, icons::X, tr("Close")).clicked() {
                    tab.show_ca = false;
                }
            });
        });
        ui.add_space(6.0);
        pc::form_grid("capture-ca").show(ui, |ui| {
            pc::form_label(ui, "SHA-256");
            ui.add(egui::Label::new(RichText::new(ca.fingerprint_sha256()).monospace().size(11.0)).wrap());
            ui.end_row();
            pc::form_label(ui, tr("Valid until"));
            ui.label(ca.not_after().format("%Y-%m-%d").to_string());
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if kit::secondary(ui, Some(icons::DOWNLOAD_SIMPLE), &format!("{}…", tr("Save certificate"))).clicked()
                && let Some(path) = rfd::FileDialog::new().set_file_name("merpati-capture-ca.pem").save_file()
            {
                match std::fs::write(&path, ca.cert_pem()) {
                    Ok(()) => {
                        let text = ca.trust_instructions(&path.display().to_string());
                        ui.ctx().copy_text(text);
                        app.toast(tr("Certificate saved; trust instructions copied to the clipboard"), false);
                    }
                    Err(e) => app.toast(e.to_string(), true),
                }
            }
            if kit::ghost(ui, Some(icons::ARROWS_CLOCKWISE), tr("Check again")).clicked() {
                *tab.trusted.lock() = None;
                check_trust(app, tab_id, ca.clone());
            }
        });
        ui.label(kit::hint(
            ui,
            tr("Devices on the network can also download it from http://merpati.proxy/ through the proxy."),
        ));
    });
}

fn actions(app: &mut MerpatiApp, ui: &mut Ui, tab: &mut CaptureTab, entries: &[Arc<CaptureEntry>]) {
    let p = theme::pal(ui);
    // Narrow windows: saving to a collection moves to a second row.
    let narrow = ui.available_width() < 820.0;
    ui.horizontal(|ui| {
        ui.set_height(30.0);
        let w = if narrow { (ui.available_width() - 300.0).clamp(140.0, 220.0) } else { 220.0 };
        let r = kit::search_field(ui, "capture-search", &mut tab.search, tr("Search URLs"), w);
        widgets::a11y_name(&r, tr("Search captured requests"));
        ui.label(RichText::new(trf("{} requests", &[&entries.len()])).color(p.text_muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let export =
                ui.add_enabled_ui(!entries.is_empty(), |ui| kit::ghost(ui, Some(icons::EXPORT), tr("Export HAR")));
            if export.inner.clicked()
                && let Some(path) = rfd::FileDialog::new().set_file_name("capture.har").save_file()
            {
                let har = merpati_proxy::to_har(entries.iter().map(|e| e.as_ref()));
                match serde_json::to_vec_pretty(&har)
                    .map_err(|e| e.to_string())
                    .and_then(|b| std::fs::write(&path, b).map_err(|e| e.to_string()))
                {
                    Ok(()) => app.toast(tr("HAR exported"), false),
                    Err(e) => app.toast(e, true),
                }
            }
            if !entries.is_empty()
                && kit::ghost(ui, Some(icons::TRASH), tr("Clear")).clicked()
                && let Some(proxy) = &tab.proxy
            {
                proxy.clear();
                tab.checked.clear();
                tab.selected = None;
            }
            if !narrow {
                kit::vdivider(ui, 20.0);
                save_to_collection(app, ui, tab, entries);
            }
        });
    });
    if narrow {
        ui.horizontal(|ui| {
            ui.set_height(30.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                save_to_collection(app, ui, tab, entries)
            });
        });
    }
}

/// The target collection and "Save n to collection" (right to left).
fn save_to_collection(app: &mut MerpatiApp, ui: &mut Ui, tab: &mut CaptureTab, entries: &[Arc<CaptureEntry>]) {
    let n = tab.checked.len();
    let can = n > 0 && tab.target.is_some();
    let hint = if n == 0 { tr("Tick requests in the list first") } else { tr("Choose a collection first") };
    let save = ui.add_enabled_ui(can, |ui| {
        let label = if n == 0 { tr("Save to collection").to_string() } else { trf("Save {} to collection", &[&n]) };
        kit::secondary(ui, Some(icons::FOLDER_PLUS), &label)
    });
    if save.inner.on_disabled_hover_text(hint).clicked()
        && let Some(cid) = tab.target.clone()
    {
        let chosen: Vec<Arc<CaptureEntry>> = entries.iter().filter(|e| tab.checked.contains(&e.id)).cloned().collect();
        let items = merpati_proxy::to_request_items(chosen.iter().map(|e| e.as_ref()), &ImportOptions::default());
        let count = items.len();
        if let Some(c) = app.ws.data.collection_mut(&cid) {
            c.items.extend(items.into_iter().map(Into::into));
            app.ws.mark_collection(&cid);
            tab.checked.clear();
            app.toast(format!("{count} {}", tr("requests saved")), false);
        }
    }
    let current = tab
        .target
        .as_ref()
        .and_then(|id| app.ws.data.collection(id))
        .map(|c| c.name.clone())
        .unwrap_or_else(|| tr("choose collection").into());
    egui::ComboBox::from_id_salt("capture-target")
        .icon(crate::kit::combo_caret)
        .width(160.0)
        .selected_text(current)
        .show_ui(ui, |ui| {
            for c in &app.ws.data.collections {
                ui.selectable_value(&mut tab.target, Some(c.id.clone()), &c.name);
            }
        });
}

fn detail(ui: &mut Ui, e: &CaptureEntry, dark: bool) {
    let p = theme::pal(ui);
    egui::ScrollArea::vertical().id_salt("capture-detail").auto_shrink([false; 2]).show(ui, |ui| {
        ui.horizontal(|ui| {
            pc::method_pill(ui, if e.method.is_empty() { "CONNECT" } else { e.method.as_str() });
            if let Some(s) = e.status {
                kit::status_pill(ui, s, e.status_text());
            } else if e.state == EntryState::Failed {
                kit::pill(ui, tr("failed"), p.err);
            }
        });
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(&e.url).monospace().strong()).wrap().selectable(true));
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            kit::pill(ui, &e.http_version, p.text_muted);
            if e.tls_intercepted {
                kit::pill(ui, &format!("{} {}", icons::LOCK_SIMPLE, tr("decrypted")), p.ok);
            }
            if let Some(t) = e.timings.total {
                kit::metric(ui, icons::TIMER, &format!("{} ms", t.as_millis()));
            }
            if let Some(t) = e.timings.ttfb {
                kit::metric(ui, icons::HOURGLASS, &format!("TTFB {} ms", t.as_millis()));
            }
            kit::metric(ui, icons::DOWNLOAD_SIMPLE, &merpati_core::util::human_bytes(e.response_body.size));
            if let Some(s) = &e.stream {
                kit::metric(ui, icons::ARROW_UP, &merpati_core::util::human_bytes(s.bytes_up));
                kit::metric(ui, icons::ARROW_DOWN, &merpati_core::util::human_bytes(s.bytes_down));
            }
        });
        if let Some(err) = &e.error {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(icons::WARNING_CIRCLE).font(theme::icon_font(14.0)).color(theme::err(dark)));
                ui.label(RichText::new(err).color(theme::err(dark)));
            });
        }
        if let Some(n) = &e.note {
            ui.add_space(4.0);
            ui.label(kit::hint(ui, n));
        }
        section(ui, tr("Request headers"), e.request_headers.len());
        pc::kv_view(
            ui,
            ("cap-req-h", e.id),
            e.request_headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            tr("No headers"),
        );
        let req = e.decoded_request_body();
        if !req.data.is_empty() {
            section(ui, tr("Request body"), 0);
            crate::proto_common::text_view(
                ui,
                ("cap-req", e.id),
                &req.text().map(|t| t.into_owned()).unwrap_or_else(|| tr("(binary)").into()),
            );
        }
        section(ui, tr("Response headers"), e.response_headers.len());
        pc::kv_view(
            ui,
            ("cap-resp-h", e.id),
            e.response_headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            tr("No response yet"),
        );
        let resp = e.decoded_response_body();
        if !resp.data.is_empty() {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(kit::section_title(tr("Response body")));
                if resp.truncated {
                    kit::pill(ui, tr("truncated"), p.warn);
                }
            });
            ui.add_space(4.0);
            let text =
                resp.text().map(|t| crate::dialogs::ellipsize(&t, 100_000)).unwrap_or_else(|| tr("(binary)").into());
            crate::proto_common::text_view(ui, ("cap-resp", e.id), &text);
        }
        if !e.ws_messages.is_empty() {
            section(ui, tr("Messages"), e.ws_messages.len());
            for m in &e.ws_messages {
                ui.horizontal(|ui| {
                    let (icon, color) =
                        if m.from_client { (icons::ARROW_UP, p.info) } else { (icons::ARROW_DOWN, p.ok) };
                    ui.label(RichText::new(icon).font(theme::icon_font(13.0)).color(color));
                    ui.add(
                        egui::Label::new(
                            RichText::new(crate::dialogs::ellipsize(&m.text(), 200)).monospace().size(11.5),
                        )
                        .truncate(),
                    );
                });
            }
        }
    });
}

/// A titled section of the detail pane, with a count badge.
fn section(ui: &mut Ui, title: &str, count: usize) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(kit::section_title(title));
        if count > 0 {
            pc::count_badge(ui, count);
        }
    });
    ui.add_space(4.0);
}
