//! Application shell: workspace, tabs, background tasks, shortcuts and the
//! top-level layout (PRD section 11).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use egui::{Key, KeyboardShortcut, Modifiers, RichText};

use merpati_core::environment::Environment;
use merpati_core::history::HistoryEntry;
use merpati_core::model::{Collection, Folder, HttpRequest, Id, Item, RequestItem, RequestKind, WsRequest};
use merpati_core::script::ScriptEngine;
use merpati_core::settings::AppSettings;
use merpati_core::tokens::{OAuth2Token, TokenStore};
use merpati_core::variables::VariableScopes;
use merpati_http::{CancellationToken, EngineConfig, HttpEngine};
use merpati_runner::{ExecuteInput, Execution, RunEvent, Services};

use crate::dialogs::Dialog;
use crate::i18n::{tr, trf};
use crate::request_tab::RequestTab;
use crate::state::WsState;
use crate::widgets::VarCtx;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ItemRef {
    pub collection: Id,
    pub item: Id,
}

pub enum TabKind {
    Request(Box<RequestTab>),
    WebSocket(Box<crate::ws_tab::WsTab>),
    Container(crate::container_tab::ContainerTab),
    Environment(crate::env_tab::EnvTab),
    Runner(Box<crate::runner_tab::RunnerTab>),
    Mock(Box<crate::mock_tab::MockTab>),
    Spec(Box<crate::spec_tab::SpecTab>),
    Mqtt(Box<crate::mqtt_tab::MqttTab>),
    Grpc(Box<crate::grpc_tab::GrpcTab>),
    SocketIo(Box<crate::sio_tab::SioTab>),
    Mcp(Box<crate::mcp_tab::McpTab>),
    Ai(Box<crate::ai_tab::AiTab>),
    Webhook(Box<crate::webhook_tab::WebhookTab>),
    Flow(Box<crate::flow_tab::FlowTab>),
    Capture(Box<crate::capture_tab::CaptureTab>),
}

impl TabKind {
    /// The saved request a tab shows, if any.
    pub fn location(&self) -> Option<&ItemRef> {
        match self {
            TabKind::Request(t) => t.location.as_ref(),
            TabKind::WebSocket(t) => t.location.as_ref(),
            TabKind::Mqtt(t) => t.slot.location.as_ref(),
            TabKind::Grpc(t) => t.slot.location.as_ref(),
            TabKind::SocketIo(t) => t.slot.location.as_ref(),
            TabKind::Mcp(t) => t.slot.location.as_ref(),
            TabKind::Ai(t) => t.slot.location.as_ref(),
            _ => None,
        }
    }

    pub fn location_mut(&mut self) -> Option<&mut Option<ItemRef>> {
        match self {
            TabKind::Request(t) => Some(&mut t.location),
            TabKind::WebSocket(t) => Some(&mut t.location),
            TabKind::Mqtt(t) => Some(&mut t.slot.location),
            TabKind::Grpc(t) => Some(&mut t.slot.location),
            TabKind::SocketIo(t) => Some(&mut t.slot.location),
            TabKind::Mcp(t) => Some(&mut t.slot.location),
            TabKind::Ai(t) => Some(&mut t.slot.location),
            _ => None,
        }
    }

    /// The unsaved request a tab holds, if any.
    pub fn scratch(&self) -> Option<&RequestItem> {
        match self {
            TabKind::Request(t) => t.scratch.as_ref(),
            TabKind::WebSocket(t) => t.scratch.as_ref(),
            TabKind::Mqtt(t) => t.slot.scratch.as_ref(),
            TabKind::Grpc(t) => t.slot.scratch.as_ref(),
            TabKind::SocketIo(t) => t.slot.scratch.as_ref(),
            TabKind::Mcp(t) => t.slot.scratch.as_ref(),
            TabKind::Ai(t) => t.slot.scratch.as_ref(),
            _ => None,
        }
    }

    pub fn scratch_mut(&mut self) -> Option<&mut Option<RequestItem>> {
        match self {
            TabKind::Request(t) => Some(&mut t.scratch),
            TabKind::WebSocket(t) => Some(&mut t.scratch),
            TabKind::Mqtt(t) => Some(&mut t.slot.scratch),
            TabKind::Grpc(t) => Some(&mut t.slot.scratch),
            TabKind::SocketIo(t) => Some(&mut t.slot.scratch),
            TabKind::Mcp(t) => Some(&mut t.slot.scratch),
            TabKind::Ai(t) => Some(&mut t.slot.scratch),
            _ => None,
        }
    }

    /// Script state of protocol tabs (FR-S7).
    pub fn script_state_mut(&mut self) -> Option<&mut crate::proto_common::ScriptState> {
        match self {
            TabKind::Grpc(t) => Some(&mut t.scripts),
            TabKind::Mqtt(t) => Some(&mut t.scripts),
            TabKind::SocketIo(t) => Some(&mut t.scripts),
            TabKind::Mcp(t) => Some(&mut t.scripts),
            TabKind::Ai(t) => Some(&mut t.scripts),
            TabKind::WebSocket(t) => Some(&mut t.scripts),
            _ => None,
        }
    }

    /// The tab type for a request of this protocol.
    pub fn for_item(location: Option<ItemRef>, scratch: Option<RequestItem>, kind: &RequestKind) -> Option<TabKind> {
        use crate::proto_common::ItemSlot;
        let slot = ItemSlot { location, scratch };
        Some(match kind {
            RequestKind::Http(_) => TabKind::Request(Box::new(match slot.location {
                Some(l) => RequestTab::saved(l),
                None => RequestTab::scratch(slot.scratch?),
            })),
            RequestKind::WebSocket(_) => TabKind::WebSocket(Box::new(match slot.location {
                Some(l) => crate::ws_tab::WsTab::saved(l),
                None => crate::ws_tab::WsTab::scratch(slot.scratch?),
            })),
            RequestKind::Mqtt(_) => TabKind::Mqtt(Box::new(crate::mqtt_tab::MqttTab::new(slot))),
            RequestKind::Grpc(_) => TabKind::Grpc(Box::new(crate::grpc_tab::GrpcTab::new(slot))),
            RequestKind::SocketIo(_) => TabKind::SocketIo(Box::new(crate::sio_tab::SioTab::new(slot))),
            RequestKind::Mcp(_) => TabKind::Mcp(Box::new(crate::mcp_tab::McpTab::new(slot))),
            RequestKind::Ai(_) => TabKind::Ai(Box::new(crate::ai_tab::AiTab::new(slot))),
        })
    }
}

pub struct Tab {
    pub id: u64,
    pub kind: TabKind,
}

/// Console lines kept (the latest).
pub const MAX_CONSOLE: usize = 5000;

pub enum Msg {
    Request {
        tab: u64,
        exec: Box<Execution>,
        before: VariableScopes,
        after: VariableScopes,
        collection: Option<Id>,
        /// Environment active when the request was sent.
        env: Option<Id>,
        rerun: bool,
    },
    Run {
        tab: u64,
        event: Box<RunEvent>,
    },
    RunFinished {
        tab: u64,
        before: VariableScopes,
        after: VariableScopes,
        collection: Id,
        /// Environment active when the run started.
        env: Option<Id>,
        persist: bool,
    },
    Token {
        owner: String,
        result: Result<OAuth2Token, String>,
    },
    Ws {
        tab: u64,
        epoch: u64,
        event: merpati_http::ws::WsEvent,
    },
    Schema {
        tab: u64,
        result: Result<merpati_convert::graphql::Schema, String>,
    },
    Mock {
        tab: u64,
        result: Result<merpati_mock::MockServer, String>,
    },
    Perf {
        tab: u64,
        event: crate::runner_tab::PerfMsg,
    },
    Mqtt {
        tab: u64,
        epoch: u64,
        event: merpati_mqtt::MqttEvent,
        /// MQTT 5 properties of a message event.
        props: Option<merpati_mqtt::MessageProperties>,
    },
    Grpc {
        tab: u64,
        epoch: u64,
        event: crate::grpc_tab::GrpcMsg,
    },
    SocketIo {
        tab: u64,
        epoch: u64,
        event: merpati_socketio::SocketIoEvent,
    },
    Mcp {
        tab: u64,
        epoch: u64,
        event: crate::mcp_tab::McpMsg,
    },
    Ai {
        tab: u64,
        epoch: u64,
        event: crate::ai_tab::AiMsg,
    },
    Webhook {
        tab: u64,
        event: crate::webhook_tab::WebhookMsg,
    },
    Flow {
        tab: u64,
        event: crate::flow_tab::FlowMsg,
    },
    Capture {
        tab: u64,
        event: crate::capture_tab::CaptureMsg,
    },
    /// A protocol tab's script finished (FR-S7).
    ProtoScript {
        tab: u64,
        run: Box<crate::proto_common::ScriptRun>,
    },
    Toast(String, bool),
    Update(crate::update::UpdateMsg),
}

/// What a click in the tab bar asks for.
enum TabBarAct {
    Activate(usize),
    CloseOthers(usize),
    New,
}

/// Actions requested from inside a tab while it is being shown (the tab is
/// temporarily out of `tabs`, so they run after it is put back).
pub enum Deferred {
    Send(u64),
    RerunTests(u64),
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Collections,
    Environments,
    History,
    Apis,
    Flows,
}

pub struct MerpatiApp {
    pub rt: tokio::runtime::Runtime,
    pub settings: AppSettings,
    pub settings_path: PathBuf,
    pub ws: WsState,
    pub http: Arc<HttpEngine>,
    pub scripts: Arc<dyn ScriptEngine>,
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_tab_id: u64,
    pub sidebar_tab: SidebarTab,
    pub sidebar: crate::sidebar::SidebarState,
    pub console: Vec<merpati_runner::ConsoleLine>,
    pub console_open: bool,
    pub console_filter: crate::dialogs::ConsoleFilter,
    pub tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    pub dialog: Option<Dialog>,
    pub toasts: Vec<Toast>,
    pub ctx: egui::Context,
    pub pending_tokens: std::collections::HashSet<String>,
    last_settings_saved: AppSettings,
    pub automation: Option<crate::automation::Automation>,
    pub pending_runner_start: Option<u64>,
    pub deferred: Vec<Deferred>,
    /// Last session file contents, and when input last changed something.
    pub last_session: Vec<u8>,
    session_dirty: Option<Instant>,
    /// Content hash of each unsaved request tab when it was opened, to tell
    /// whether closing it loses edits.
    scratch_baseline: std::collections::HashMap<u64, u64>,
    /// Tab to close once the Save As dialog saved it (closing asked to save).
    pub close_after_save: Option<u64>,
    /// Update check and download state.
    pub update: crate::update::UpdateState,
    /// The tab the tab bar last scrolled into view, and the bar's width then.
    tab_in_view: Option<(u64, u32)>,
    /// Keeps trackpad scrolls on one axis per gesture.
    axis_lock: crate::scroll_lock::AxisLock,
    /// The native menu bar (installed by `new`, not in tests).
    #[cfg(target_os = "macos")]
    menu: Option<crate::menu::AppMenu>,
}

/// A new connection or call number. Session events carry the number of the
/// session that sent them (0: not tied to one), so events a closed session
/// sends late are ignored once the tab has connected again.
pub fn next_epoch() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Whether an event of session `epoch` is for the tab's current session.
fn current(tab_epoch: u64, epoch: u64) -> bool {
    epoch == 0 || epoch == tab_epoch
}

/// Hash of a request's content (name and settings included).
fn content_hash(item: &RequestItem) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(item).unwrap_or_default().hash(&mut h);
    h.finish()
}

fn default_workspace_dir() -> PathBuf {
    AppSettings::config_dir().join("workspaces").join("My Workspace")
}

/// "New …" menu entries for the non-HTTP protocols.
/// Icon for a protocol template name (see [`protocol_templates`]).
pub fn protocol_icon(name: &str) -> &'static str {
    use crate::icons;
    match name {
        "gRPC" => icons::LIGHTNING,
        "MQTT" => icons::WIFI_HIGH,
        "Socket.IO" => icons::PLUGS,
        "MCP" => icons::PUZZLE_PIECE,
        "AI request" => icons::SPARKLE,
        _ => icons::GLOBE,
    }
}

/// What the New menus create (the toolbar's New and, on macOS, File › New).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewItem {
    Request,
    WebSocket,
    /// The n-th entry of [`protocol_templates`].
    Protocol(usize),
    Webhook,
    Capture,
    Flow,
    Collection,
    Environment,
}

impl NewItem {
    /// Every item in menu order, with its icon and (untranslated) label.
    pub fn all() -> Vec<(NewItem, &'static str, &'static str)> {
        use crate::icons;
        let mut items = vec![
            (NewItem::Request, icons::GLOBE, "New Request"),
            (NewItem::WebSocket, icons::PLUGS_CONNECTED, "New WebSocket"),
        ];
        for (i, (label, name, _)) in protocol_templates().into_iter().enumerate() {
            items.push((NewItem::Protocol(i), protocol_icon(name), label));
        }
        items.extend([
            (NewItem::Webhook, icons::WEBHOOKS_LOGO, "New webhook listener"),
            (NewItem::Capture, icons::BROADCAST, "Capture traffic (proxy)"),
            (NewItem::Flow, icons::FLOW_ARROW, "New flow"),
            (NewItem::Collection, icons::STACK, "New Collection"),
            (NewItem::Environment, icons::GLOBE_SIMPLE, "New Environment"),
        ]);
        items
    }
}

pub fn protocol_templates() -> Vec<(&'static str, &'static str, RequestKind)> {
    use merpati_core::protocols::*;
    vec![
        ("New gRPC", "gRPC", RequestKind::Grpc(GrpcRequest { url: "localhost:50051".into(), ..Default::default() })),
        (
            "New MQTT",
            "MQTT",
            RequestKind::Mqtt(MqttRequest { url: "mqtt://localhost:1883".into(), ..Default::default() }),
        ),
        (
            "New Socket.IO",
            "Socket.IO",
            RequestKind::SocketIo(SocketIoRequest {
                url: "http://localhost:3000".into(),
                listeners: vec![SocketIoListener { event: "message".into(), enabled: true }],
                ..Default::default()
            }),
        ),
        ("New MCP", "MCP", RequestKind::Mcp(McpRequest::default())),
        (
            "New AI request",
            "AI request",
            RequestKind::Ai(AiRequest {
                model: "gpt-5-mini".into(),
                stream: true,
                messages: vec![AiMessage { role: "user".into(), content: "Hello!".into() }],
                auth: merpati_core::auth::Auth::bearer("{{vault:openai}}"),
                ..Default::default()
            }),
        ),
    ]
}

/// Replaces every occurrence of a secret (raw or percent-encoded) with `•••`.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        out = out.replace(s.as_str(), "•••");
        let encoded = url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        if encoded != *s {
            out = out.replace(&encoded, "•••");
        }
    }
    out
}

/// [`redact`] on every part of a request that may hold a literal secret
/// (pre-request scripts often copy one into a header).
pub fn redact_request(req: &mut HttpRequest, secrets: &[String]) {
    if secrets.is_empty() {
        return;
    }
    req.url = redact(&req.url, secrets);
    let body = &mut req.body;
    for kv in req.params.iter_mut().chain(&mut req.path_vars).chain(&mut req.headers).chain(&mut body.urlencoded) {
        kv.value = redact(&kv.value, secrets);
    }
    for f in &mut body.form_data {
        f.value = redact(&f.value, secrets);
    }
    body.raw = redact(&body.raw, secrets);
    body.graphql.query = redact(&body.graphql.query, secrets);
    body.graphql.variables = redact(&body.graphql.variables, secrets);
}

/// A copy of the collection containing only what one request needs (its
/// ancestors' auth, scripts and variables), so sending never clones a
/// collection with thousands of requests.
pub fn slim_collection(c: &Collection, item_id: &str) -> Collection {
    fn walk(items: &[Item], id: &str) -> Option<Vec<Item>> {
        for item in items {
            match item {
                Item::Request(r) if r.id == id => return Some(vec![Item::Request(r.clone())]),
                Item::Folder(f) => {
                    if let Some(children) = walk(&f.items, id) {
                        let mut copy = Folder { items: children, ..f.clone() };
                        copy.items.shrink_to_fit();
                        return Some(vec![Item::Folder(copy)]);
                    }
                }
                _ => {}
            }
        }
        None
    }
    let items = walk(&c.items, item_id).unwrap_or_default();
    Collection { items, ..Collection { items: Vec::new(), ..c.clone() } }
}

impl MerpatiApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        mut settings: AppSettings,
        settings_path: PathBuf,
        open_path: Option<PathBuf>,
    ) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        crate::theme::install_fonts(&cc.egui_ctx);
        crate::i18n::set_language(settings.ui.language);
        cc.egui_ctx.set_theme(match settings.ui.theme {
            merpati_core::settings::ThemePreference::System => egui::ThemePreference::System,
            merpati_core::settings::ThemePreference::Light => egui::ThemePreference::Light,
            merpati_core::settings::ThemePreference::Dark => egui::ThemePreference::Dark,
        });
        crate::theme::apply(&cc.egui_ctx, settings.ui.font_size);

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(32)
            .thread_name("merpati-rt")
            .enable_all()
            .build()
            .expect("tokio runtime");

        let candidates: Vec<PathBuf> = open_path
            .into_iter()
            .chain(settings.recent_workspaces.iter().cloned())
            .chain(std::iter::once(default_workspace_dir()))
            .collect();
        let mut ws = None;
        let mut startup_error = None;
        for path in candidates {
            if merpati_core::storage::WorkspaceStore::is_workspace(&path) {
                match WsState::open(&path, settings.history_limit) {
                    Ok(w) => {
                        ws = Some(w);
                        break;
                    }
                    Err(e) => startup_error = Some(format!("Could not open {}: {e}", path.display())),
                }
            }
        }
        let ws = ws.unwrap_or_else(|| {
            let dir = default_workspace_dir();
            WsState::create(&dir, "My Workspace", settings.history_limit)
                .or_else(|_| WsState::open(&dir, settings.history_limit))
                .expect("could not create the default workspace")
        });
        settings.push_recent(ws.root());
        // Settings from older versions may hold passwords in plain text.
        let moved_secrets = Self::vault_settings_secrets(&mut settings);
        let http = Arc::new(Self::engine(&settings, &ws));
        let scripts = Self::script_engine(&settings);
        let (tx, rx) = mpsc::channel();
        let mut app = MerpatiApp {
            rt,
            last_settings_saved: settings.clone(),
            settings,
            settings_path,
            ws,
            http,
            scripts,
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 1,
            sidebar_tab: SidebarTab::Collections,
            sidebar: Default::default(),
            console: Vec::new(),
            console_open: false,
            console_filter: Default::default(),
            tx,
            rx,
            dialog: None,
            toasts: Vec::new(),
            ctx: cc.egui_ctx.clone(),
            pending_tokens: Default::default(),
            automation: crate::automation::Automation::from_env(),
            pending_runner_start: None,
            last_session: Vec::new(),
            session_dirty: None,
            deferred: Vec::new(),
            scratch_baseline: Default::default(),
            close_after_save: None,
            update: Default::default(),
            tab_in_view: None,
            axis_lock: Default::default(),
            #[cfg(target_os = "macos")]
            menu: None,
        };
        if crate::update::check_on_startup(&app) {
            crate::update::check(&mut app, false);
        }
        #[cfg(target_os = "macos")]
        {
            // Tests run off the main thread, where AppKit has no menu bar.
            if !cfg!(test) {
                app.menu = Some(crate::menu::AppMenu::install(&cc.egui_ctx));
            }
            // ⌘Q is the menu's Quit, which closes like the window's button.
            cc.egui_ctx.options_mut(|o| o.quit_shortcuts.clear());
        }
        if let Some(log) = crate::crash::take_pending(&AppSettings::config_dir()) {
            app.toast(trf("Merpati closed unexpectedly last time. Details: {}", &[&log.display()]), true);
        }
        if moved_secrets && let Err(e) = app.settings.save(&app.settings_path) {
            tracing::warn!("could not save settings: {e}");
        }
        if let Some(e) = startup_error {
            app.toast(e, true);
        }
        for e in app.ws.data.load_errors.clone() {
            app.toast(e, true);
        }
        app.attach_external_secrets(false);
        crate::dialogs::restore_session(&mut app);
        if app.tabs.is_empty() {
            app.new_request_tab();
        }
        app
    }

    /// Opens the workspace's external secret links and warms their cache in
    /// the background, so `{{vault:…}}` lookups rarely wait for a provider.
    /// Without `force` nothing happens for workspaces that never configured a
    /// provider (building the client loads the system certificates).
    pub fn attach_external_secrets(&mut self, force: bool) {
        if self.ws.external.is_some()
            || (!force && !merpati_secrets::SecretsConfig::path(&self.ws.local_dir()).exists())
        {
            return;
        }
        let client = match merpati_secrets::client_for(&self.proxy_settings(), &self.settings.certificates) {
            Ok(c) => c,
            Err(e) => {
                self.toast(trf("External secrets: {}", &[&e.message]), true);
                return;
            }
        };
        match merpati_secrets::ExternalSecrets::open(&self.ws.local_dir(), self.ws.secrets.clone(), client) {
            Ok(ext) => {
                let names = ext.linked_names();
                if !names.is_empty() {
                    let warm = ext.clone();
                    let tx = self.tx.clone();
                    self.rt.spawn(async move {
                        for (name, e) in warm.prefetch(&names).await {
                            let _ = tx.send(Msg::Toast(format!("vault:{name}: {}", e.message), true));
                        }
                    });
                }
                self.ws.external = Some(ext);
            }
            Err(e) => self.toast(trf("External secrets: {}", &[&e.message]), true),
        }
    }

    /// Scripts run in worker processes started from this executable, so a
    /// script that crashes, hangs in native code or uses too much memory
    /// cannot take the app down.
    fn script_engine(settings: &AppSettings) -> Arc<dyn ScriptEngine> {
        let config = merpati_script::IsolationConfig {
            memory_limit_bytes: settings.script_memory_mb.saturating_mul(1024 * 1024),
            ..Default::default()
        };
        match merpati_script::IsolatedEngine::for_current_exe(config) {
            Ok(e) => Arc::new(e),
            Err(e) => {
                tracing::warn!("script worker processes unavailable ({e}); running scripts in the app");
                Arc::new(merpati_script::BoaEngine::new())
            }
        }
    }

    fn engine(settings: &AppSettings, ws: &WsState) -> HttpEngine {
        HttpEngine::new(Self::engine_config(settings, ws), ws.cookies.clone())
    }

    fn engine_config(settings: &AppSettings, ws: &WsState) -> EngineConfig {
        let mut cfg = EngineConfig::from_settings(settings);
        cfg.temp_dir = Some(std::env::temp_dir().join("merpati"));
        cfg.secret_resolver = Some(Self::vault_resolver(ws));
        cfg
    }

    /// Extra CA and the client certificate from Settings > Certificates for
    /// a WebSocket, Socket.IO or MQTT connection to `url` (the proxy comes
    /// from `settings.proxy`).
    pub fn tls_setup(&self, url: &str, verify: bool) -> Result<merpati_http::tls::TlsSetup, String> {
        let secrets = Self::vault_resolver(&self.ws);
        merpati_http::tls::TlsSetup::for_url(&self.settings.certificates, Some(&secrets), verify, url)
    }

    /// Resolves `{{vault:name}}` in the settings (client certificate
    /// passphrases, the proxy password): the settings' own secrets for
    /// `settings.` names, the workspace vault otherwise.
    pub fn vault_resolver(ws: &WsState) -> merpati_http::SecretResolver {
        let vault = crate::state::settings_vault(ws.vault());
        merpati_http::SecretResolver::new(move |s| {
            let scopes = merpati_core::variables::VariableScopes::default();
            merpati_core::variables::Resolver::new(&scopes).with_vault(&vault).resolve(s)
        })
    }

    /// Moves literal secrets of `settings` (proxy password, client
    /// certificate passphrases) to the settings' Keychain entry and leaves
    /// `{{vault:settings.<name>}}` references, so `settings.json` never
    /// holds them. Returns whether anything moved.
    pub fn vault_settings_secrets(settings: &mut AppSettings) -> bool {
        let store = crate::state::settings_secrets();
        let mut moved = false;
        let mut keep = |value: &mut String, name: &str| {
            if !value.is_empty() && !merpati_core::variables::is_pure_reference(value) {
                store.vault_set(name, value);
                *value = format!("{{{{vault:{}{name}}}}}", crate::state::SETTINGS_VAULT_PREFIX);
                moved = true;
            }
        };
        keep(&mut settings.proxy.password, "proxy-password");
        for c in &mut settings.certificates.client_certs {
            let host: String =
                c.host.chars().map(|ch| if ch.is_ascii_alphanumeric() || ch == '.' { ch } else { '-' }).collect();
            keep(&mut c.passphrase, &format!("client-cert-{}", host.trim_matches('-')));
        }
        moved
    }

    /// The proxy settings with the password itself, for the protocol clients.
    pub fn proxy_settings(&self) -> merpati_core::settings::ProxySettings {
        self.http.config().proxy
    }

    pub fn services(&self) -> Services {
        let mut defaults = self.settings.request.clone();
        defaults.timeout_ms = self.settings.request.timeout_ms;
        Services {
            http: self.http.clone(),
            scripts: self.scripts.clone(),
            tokens: self.ws.secrets.clone(),
            vault: Some(self.ws.vault()),
            defaults,
            script_timeout: Duration::from_millis(self.settings.script_timeout_ms.max(100)),
            runtime: self.rt.handle().clone(),
            packages: Some(merpati_core::script::PackageContext::for_workspace(
                self.ws.root(),
                &self.settings.package_registry,
            )),
        }
    }

    /// Something changed that the session file should reflect soon.
    pub fn mark_session_dirty(&mut self) {
        self.session_dirty.get_or_insert_with(Instant::now);
    }

    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast { text: text.into(), error, at: Instant::now() });
    }

    pub fn open_workspace(&mut self, path: PathBuf, create_name: Option<String>) {
        crate::dialogs::save_session(self);
        self.ws.flush_blocking();
        let result = match create_name {
            Some(name) => WsState::create(&path, &name, self.settings.history_limit),
            None => WsState::open(&path, self.settings.history_limit),
        };
        match result {
            Ok(ws) => self.switch_workspace(ws, &path),
            Err(e) => self.toast(trf("Could not open workspace: {}", &[&e]), true),
        }
    }

    fn switch_workspace(&mut self, ws: WsState, path: &std::path::Path) {
        // Close the old tabs properly: running requests, runs, mock servers,
        // listeners and proxies would otherwise keep going (and write their
        // results into the new workspace).
        while !self.tabs.is_empty() {
            self.close_tab(self.tabs.len() - 1);
        }
        self.ws = ws;
        self.http = Arc::new(Self::engine(&self.settings, &self.ws));
        self.attach_external_secrets(false);
        self.active = 0;
        self.settings.push_recent(path);
        crate::dialogs::restore_session(self);
        if self.tabs.is_empty() {
            self.new_request_tab();
        }
        for e in self.ws.data.load_errors.clone() {
            self.toast(e, true);
        }
    }

    pub fn add_tab(&mut self, kind: TabKind) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let at = if self.tabs.is_empty() { 0 } else { (self.active + 1).min(self.tabs.len()) };
        if let Some(item) = kind.scratch() {
            self.scratch_baseline.insert(id, content_hash(item));
        }
        self.tabs.insert(at, Tab { id, kind });
        self.active = at;
        id
    }

    pub fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// Whether closing tab `idx` loses work: a request that was never saved
    /// and was edited since its tab opened, or a spec with unsaved changes.
    /// Saved requests and flows are written automatically.
    pub fn tab_has_unsaved_work(&self, idx: usize) -> bool {
        let Some(tab) = self.tabs.get(idx) else { return false };
        if let TabKind::Spec(t) = &tab.kind {
            return t.dirty();
        }
        match tab.kind.scratch() {
            Some(item) if tab.kind.location().is_none() => {
                self.scratch_baseline.get(&tab.id) != Some(&content_hash(item))
            }
            _ => false,
        }
    }

    /// Closes tab `idx`, asking first when that would lose unsaved work.
    pub fn request_close_tab(&mut self, idx: usize) {
        if self.tab_has_unsaved_work(idx) {
            self.dialog = Some(Dialog::CloseUnsaved { tab: self.tabs[idx].id });
        } else {
            self.close_tab(idx);
        }
    }

    pub fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(idx);
        self.scratch_baseline.remove(&tab.id);
        match tab.kind {
            TabKind::Request(t) => {
                if let Some(r) = &t.running {
                    r.cancel.cancel();
                }
            }
            TabKind::Runner(t) => t.cancel_all(),
            TabKind::Mock(t) => t.stop(&self.rt),
            TabKind::Webhook(mut t) => t.stop(&self.rt),
            TabKind::Ai(mut t) => t.cancel(),
            TabKind::Flow(mut t) => t.stop(),
            TabKind::Capture(mut t) => t.stop(&self.rt),
            TabKind::WebSocket(t) => drop(t),
            _ => {}
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        } else if idx < self.active {
            self.active -= 1;
        }
    }

    /// Asks for a workspace folder and opens it.
    pub fn pick_workspace(&mut self) {
        let Some(dir) = rfd::FileDialog::new().set_title(tr("Open workspace folder")).pick_folder() else { return };
        if merpati_core::storage::WorkspaceStore::is_workspace(&dir) {
            self.open_workspace(dir, None);
        } else {
            self.toast(trf("{} is not a Merpati workspace (no merpati.json)", &[&dir.display()]), true);
        }
    }

    pub fn create_new(&mut self, what: NewItem) {
        match what {
            NewItem::Request => self.new_request_tab(),
            NewItem::WebSocket => self.new_ws_tab(),
            NewItem::Protocol(i) => {
                if let Some((_, name, kind)) = protocol_templates().into_iter().nth(i) {
                    self.new_proto_tab(name, kind);
                }
            }
            NewItem::Webhook => {
                self.add_tab(TabKind::Webhook(Box::default()));
            }
            NewItem::Capture => {
                self.add_tab(TabKind::Capture(Box::default()));
            }
            NewItem::Flow => {
                let n = self.sidebar.flows.as_ref().map(|f| f.len()).unwrap_or(0) + 1;
                let name = format!("{} {n}", tr("Flow"));
                self.add_tab(TabKind::Flow(Box::new(crate::flow_tab::FlowTab::blank(&name))));
            }
            NewItem::Collection => self.dialog = Some(Dialog::prompt_new_collection()),
            NewItem::Environment => self.dialog = Some(Dialog::prompt_new_environment()),
        }
    }

    pub fn new_request_tab(&mut self) {
        let item = RequestItem::new(tr("Untitled Request"), HttpRequest::default());
        let mut tab = RequestTab::scratch(item);
        tab.focus_url = true;
        self.add_tab(TabKind::Request(Box::new(tab)));
    }

    /// Opens an unsaved request of another protocol in a new tab.
    pub fn new_proto_tab(&mut self, name: &str, kind: RequestKind) {
        let mut item = RequestItem::new(name, HttpRequest::default());
        item.request = kind;
        match TabKind::for_item(None, Some(item.clone()), &item.request) {
            Some(k) => {
                self.add_tab(k);
            }
            None => self
                .toast(trf("{} requests are not supported in this build yet", &[&item.request.protocol_label()]), true),
        }
    }

    pub fn new_ws_tab(&mut self) {
        let mut item = RequestItem::new("WebSocket", HttpRequest::default());
        item.request = RequestKind::WebSocket(WsRequest::default());
        self.add_tab(TabKind::WebSocket(Box::new(crate::ws_tab::WsTab::scratch(item))));
    }

    /// Opens (or focuses) the tab for a saved request.
    pub fn open_item(&mut self, r: ItemRef) {
        let existing = self.tabs.iter().position(|t| t.kind.location() == Some(&r));
        if let Some(i) = existing {
            self.active = i;
            return;
        }
        let Some(c) = self.ws.data.collection(&r.collection) else { return };
        match c.find(&r.item) {
            Some(Item::Request(item)) => {
                let label = item.request.protocol_label();
                match TabKind::for_item(Some(r), None, &item.request) {
                    Some(kind) => {
                        self.add_tab(kind);
                    }
                    None => self.toast(trf("{} requests are not supported in this build yet", &[&label]), true),
                }
            }
            Some(Item::Folder(_)) => self.open_container(r.collection.clone(), Some(r.item.clone())),
            None => {}
        }
    }

    pub fn open_container(&mut self, collection: Id, folder: Option<Id>) {
        let existing = self
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, TabKind::Container(c) if c.collection == collection && c.folder == folder));
        match existing {
            Some(i) => self.active = i,
            None => {
                self.add_tab(TabKind::Container(crate::container_tab::ContainerTab::new(collection, folder)));
            }
        }
    }

    pub fn open_environment(&mut self, env: Option<Id>) {
        let existing = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Environment(e) if e.env == env));
        match existing {
            Some(i) => self.active = i,
            None => {
                self.add_tab(TabKind::Environment(crate::env_tab::EnvTab { env, search: String::new() }));
            }
        }
    }

    pub fn open_runner(&mut self, collection: Id, folder: Option<Id>) {
        let tab = crate::runner_tab::RunnerTab::new(&self.ws, collection, folder);
        self.add_tab(TabKind::Runner(Box::new(tab)));
    }

    pub fn open_mock(&mut self, collection: Id) {
        let existing = self.tabs.iter().position(|t| matches!(&t.kind, TabKind::Mock(m) if m.collection == collection));
        match existing {
            Some(i) => self.active = i,
            None => {
                self.add_tab(TabKind::Mock(Box::new(crate::mock_tab::MockTab::new(collection))));
            }
        }
    }

    /// The variable context for a request in `collection`.
    pub fn var_ctx(&self, collection: Option<&str>) -> VarCtx {
        VarCtx::new(self.ws.scopes_for(collection), self.ws.secret_keys(collection), self.ws.vault_names())
    }

    /// Starts sending the request of tab `id`.
    pub fn send(&mut self, id: u64) {
        let Some(idx) = self.tab_index(id) else { return };
        let services = self.services();
        let TabKind::Request(tab) = &mut self.tabs[idx].kind else { return };
        if tab.running.is_some() || tab.gql_sub.as_ref().map(|s| !s.done).unwrap_or(false) {
            return;
        }
        let item = match &tab.location {
            Some(loc) => self.ws.data.collection(&loc.collection).and_then(|c| c.find_request(&loc.item)).cloned(),
            None => tab.scratch.clone(),
        };
        if item.as_ref().map(crate::request_tab::is_subscription).unwrap_or(false) {
            // Subscriptions run over WebSocket from the tab itself.
            tab.click_send = true;
            return;
        }
        let (collection, request_id, collection_id, request) = match &tab.location {
            Some(loc) => {
                let Some(c) = self.ws.data.collection(&loc.collection) else { return };
                let Some(item) = c.find_request(&loc.item) else { return };
                let Some(http) = item.http() else { return };
                (slim_collection(c, &loc.item), loc.item.clone(), Some(loc.collection.clone()), http.clone())
            }
            None => {
                let Some(item) = &tab.scratch else { return };
                let Some(http) = item.http() else { return };
                let mut c = Collection::new("Scratch");
                c.items.push(Item::Request(item.clone()));
                (c, item.id.clone(), None, http.clone())
            }
        };
        if request.url.trim().is_empty() {
            return;
        }
        let before = self.ws.scopes_for(collection_id.as_deref());
        let env = self.ws.data.active_environment.clone();
        let cancel = CancellationToken::new();
        tab.start_running(cancel.clone());
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let base = self.ws.root().to_path_buf();
        self.rt.spawn(async move {
            let mut vars = before.clone();
            let input = ExecuteInput {
                collection: &collection,
                request_id: &request_id,
                request_override: Some(&request),
                iteration: 0,
                iteration_count: 1,
                base_dir: Some(&base),
            };
            let exec = merpati_runner::execute(&services, input, &mut vars, &cancel).await;
            let _ = tx.send(Msg::Request {
                tab: id,
                exec: Box::new(exec),
                before,
                after: vars,
                collection: collection_id,
                env,
                rerun: false,
            });
            ctx.request_repaint();
        });
    }

    /// Re-runs post-response scripts on the existing response (FR-S6).
    pub fn rerun_tests(&mut self, id: u64) {
        let Some(idx) = self.tab_index(id) else { return };
        let TabKind::Request(tab) = &self.tabs[idx].kind else { return };
        let Some(result) = &tab.result else { return };
        let (Some(prepared), Some(response)) = (result.exec.prepared.clone(), result.exec.response.clone()) else {
            return;
        };
        let (collection, request_id, collection_id) = match &tab.location {
            Some(loc) => {
                let Some(c) = self.ws.data.collection(&loc.collection) else { return };
                (slim_collection(c, &loc.item), loc.item.clone(), Some(loc.collection.clone()))
            }
            None => {
                let Some(item) = &tab.scratch else { return };
                let mut c = Collection::new("Scratch");
                c.items.push(Item::Request(item.clone()));
                (c, item.id.clone(), None)
            }
        };
        let before = self.ws.scopes_for(collection_id.as_deref());
        let env = self.ws.data.active_environment.clone();
        let services = self.services();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let mut vars = before.clone();
            let cancel = CancellationToken::new();
            let mut exec = merpati_runner::pipeline::rerun_post_response(
                &services,
                &collection,
                &request_id,
                &prepared,
                &response,
                &mut vars,
                &cancel,
            )
            .await;
            exec.prepared = Some(prepared);
            exec.response = Some(response);
            let _ = tx.send(Msg::Request {
                tab: id,
                exec: Box::new(exec),
                before,
                after: vars,
                collection: collection_id,
                env,
                rerun: true,
            });
            ctx.request_repaint();
        });
    }

    /// Gets a new OAuth 2.0 token for `auth`, whose variables are resolved
    /// with `scopes` and the workspace vault in the background (vault names
    /// linked to a secret manager may need the network).
    pub fn fetch_token(
        &mut self,
        auth: merpati_core::auth::Auth,
        scopes: merpati_core::variables::VariableScopes,
        owner: String,
    ) {
        if self.pending_tokens.contains(&owner) {
            return;
        }
        self.pending_tokens.insert(owner.clone());
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let http = self.http.clone();
        let vault = self.ws.vault();
        self.rt.spawn(async move {
            let resolved = tokio::task::spawn_blocking(move || {
                let resolver = merpati_core::variables::Resolver::new(&scopes).with_vault(vault.as_ref());
                auth.map_strings(|s| resolver.resolve(s))
            })
            .await;
            let result = match resolved {
                Err(e) => Err(e.to_string()),
                Ok(auth) => {
                    let opener = |url: &str| {
                        crate::widgets::open_web_link(url);
                    };
                    // The engine presents the token endpoint's client certificate (mutual TLS).
                    merpati_auth::oauth2::get_new_token(&auth, http.as_ref(), &opener).await.map_err(|e| e.to_string())
                }
            };
            let _ = tx.send(Msg::Token { owner, result });
            ctx.request_repaint();
        });
    }

    /// Keeps the latest [`MAX_CONSOLE`] console lines.
    fn trim_console(&mut self) {
        let extra = self.console.len().saturating_sub(MAX_CONSOLE);
        self.console.drain(..extra);
    }

    /// Handles pending background messages (tests drive the app with it).
    #[cfg(test)]
    pub fn pump_messages(&mut self) {
        self.handle_msgs();
    }

    fn handle_msgs(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Request { tab, exec, before, after, collection, env, rerun } => {
                    self.ws.apply_var_changes(collection.as_deref(), env.as_deref(), &before, &after);
                    self.console.extend(exec.console.iter().cloned());
                    self.trim_console();
                    if exec.response.is_some() {
                        self.ws.mark_cookies();
                    }
                    if !rerun && let Some(req) = &exec.request {
                        let secrets =
                            self.secret_values(collection.as_deref(), env.as_deref(), &before, exec.prepared.as_ref());
                        let mut request = req.clone();
                        redact_request(&mut request, &secrets);
                        let resolved = exec
                            .sent
                            .as_ref()
                            .map(|s| s.url.clone())
                            .or_else(|| exec.prepared.as_ref().map(|p| p.url.clone()))
                            .unwrap_or_default();
                        let entry = HistoryEntry {
                            id: merpati_core::util::new_id(),
                            timestamp: chrono::Utc::now().timestamp_millis(),
                            request,
                            resolved_url: redact(&resolved, &secrets),
                            status: exec.response.as_ref().map(|r| r.status),
                            elapsed_ms: exec.response.as_ref().map(|r| r.timings.total_ms).unwrap_or(0.0),
                            size: exec.response.as_ref().map(|r| r.total_size()).unwrap_or(0),
                            error: exec.error.as_ref().map(|e| redact(&e.message, &secrets)),
                            source_request_id: collection.as_ref().map(|_| exec.request_id.clone()),
                            name: Some(exec.request_name.clone()),
                        };
                        self.ws.add_history(entry);
                    }
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Request(t) = &mut self.tabs[i].kind
                    {
                        t.finish(*exec, rerun);
                    }
                }
                Msg::Run { tab, event } => {
                    if let RunEvent::Execution(exec) = event.as_ref() {
                        self.console.extend(exec.console.iter().cloned());
                        self.trim_console();
                    }
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Runner(t) = &mut self.tabs[i].kind
                    {
                        t.on_event(*event);
                    }
                }
                Msg::RunFinished { tab, before, after, collection, env, persist } => {
                    if persist {
                        self.ws.apply_var_changes(Some(&collection), env.as_deref(), &before, &after);
                    }
                    self.ws.mark_cookies();
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Runner(t) = &mut self.tabs[i].kind
                    {
                        t.finished();
                    }
                }
                Msg::Token { owner, result } => {
                    self.pending_tokens.remove(&owner);
                    match result {
                        Ok(token) => {
                            TokenStore::set(self.ws.secrets.as_ref(), &owner, Some(token));
                            self.toast(tr("New access token stored"), false);
                        }
                        Err(e) => self.toast(trf("OAuth 2.0: {}", &[&e]), true),
                    }
                }
                Msg::Ws { tab, epoch, event } => {
                    if let Some(i) = self.tab_index(tab) {
                        match &mut self.tabs[i].kind {
                            TabKind::WebSocket(t) if current(t.epoch, epoch) => t.on_event(event),
                            TabKind::Request(t) if current(t.ws_epoch, epoch) => t.on_ws_event(event),
                            _ => {}
                        }
                    }
                }
                Msg::Schema { tab, result } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Request(t) = &mut self.tabs[i].kind
                    {
                        t.graphql.loading = false;
                        match result {
                            Ok(s) => {
                                t.graphql.schema = Some(Arc::new(s));
                                t.graphql.error = None;
                                t.graphql.explorer_open = true;
                            }
                            Err(e) => t.graphql.error = Some(e),
                        }
                    }
                }
                Msg::Mock { tab, result } => {
                    let mut err = None;
                    if let Some(i) = self.tab_index(tab) {
                        if let TabKind::Mock(t) = &mut self.tabs[i].kind {
                            match result {
                                Ok(server) => t.started(server),
                                Err(e) => {
                                    t.starting = false;
                                    err = Some(e);
                                }
                            }
                        } else if let Ok(server) = result {
                            self.rt.spawn(server.stop());
                        }
                    }
                    if let Some(e) = err {
                        self.toast(trf("Mock server: {}", &[&e]), true);
                    }
                }
                Msg::Perf { tab, event } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Runner(t) = &mut self.tabs[i].kind
                    {
                        t.on_perf(event);
                    }
                }
                Msg::Mqtt { tab, epoch, event, props } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Mqtt(t) = &mut self.tabs[i].kind
                        && current(t.epoch, epoch)
                    {
                        t.on_event(event, props);
                    }
                }
                Msg::Grpc { tab, epoch, event } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Grpc(t) = &mut self.tabs[i].kind
                        && current(t.epoch, epoch)
                    {
                        t.on_msg(event);
                    }
                }
                Msg::SocketIo { tab, epoch, event } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::SocketIo(t) = &self.tabs[i].kind
                        && current(t.epoch, epoch)
                    {
                        let listened = crate::sio_tab::listened(self, t);
                        if let TabKind::SocketIo(t) = &mut self.tabs[i].kind {
                            t.on_event(event, &listened);
                        }
                    }
                }
                Msg::Mcp { tab, epoch, event } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Mcp(t) = &mut self.tabs[i].kind
                        && current(t.epoch, epoch)
                    {
                        t.on_msg(event);
                    }
                }
                Msg::Ai { tab, epoch, event } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Ai(t) = &mut self.tabs[i].kind
                        && current(t.epoch, epoch)
                    {
                        t.on_msg(event);
                    }
                }
                Msg::Capture { tab, event } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Capture(t) = &mut self.tabs[i].kind
                    {
                        t.on_msg(event);
                    }
                }
                Msg::Update(m) => crate::update::on_msg(self, m),
                Msg::Flow { tab, event } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Flow(t) = &mut self.tabs[i].kind
                    {
                        t.on_msg(event);
                    }
                }
                Msg::Webhook { tab, event } => {
                    if let Some(i) = self.tab_index(tab)
                        && let TabKind::Webhook(t) = &mut self.tabs[i].kind
                    {
                        t.on_msg(event);
                    }
                }
                Msg::ProtoScript { tab, run } => {
                    self.ws.apply_var_changes(
                        run.collection.as_deref(),
                        run.env.as_deref(),
                        &run.before,
                        &run.outcome.variables,
                    );
                    let at = chrono::Utc::now().timestamp_millis();
                    for c in &run.outcome.console {
                        self.console.push(merpati_runner::ConsoleLine {
                            kind: c.level.into(),
                            message: c.message.clone(),
                            source: run.name.clone(),
                            phase: run.event.label().into(),
                            at_ms: at,
                        });
                    }
                    for e in &run.outcome.errors {
                        self.console.push(merpati_runner::ConsoleLine {
                            kind: merpati_runner::ConsoleKind::Error,
                            message: format!("{}: {}", e.location, e.message),
                            source: run.name.clone(),
                            phase: run.event.label().into(),
                            at_ms: at,
                        });
                    }
                    self.trim_console();
                    if let Some(i) = self.tab_index(tab)
                        && let Some(s) = self.tabs[i].kind.script_state_mut()
                    {
                        s.on_run(&run);
                    }
                }
                Msg::Toast(t, e) => self.toast(t, e),
            }
        }
    }

    /// Values that must not be written to local files such as the history:
    /// secret variables, vault entries and sensitive auth parameters.
    fn secret_values(
        &self,
        collection: Option<&str>,
        env: Option<&str>,
        scopes: &VariableScopes,
        prepared: Option<&merpati_core::prepared::PreparedRequest>,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for key in self.ws.secret_keys_in(collection, env) {
            if let Some((v, _)) = scopes.get(&key) {
                out.push(v.into_owned());
            }
        }
        for name in self.ws.secrets.vault_names() {
            if let Some(v) = self.ws.secrets.vault_get(&name) {
                out.push(v);
            }
        }
        if let Some(ext) = &self.ws.external {
            out.extend(ext.linked_names().iter().filter_map(|n| ext.get_cached(n)));
        }
        if let Some(p) = prepared {
            for param in p.auth.kind.sensitive_params() {
                out.push(p.auth.get_string(param));
            }
        }
        out.retain(|v| v.len() >= 4);
        out.sort_by_key(|v| std::cmp::Reverse(v.len()));
        out
    }

    /// Carries out the menu bar items chosen since the last frame. Shortcut
    /// items become the key press they stand for, so `shortcuts` and text
    /// editing handle them as usual.
    #[cfg(target_os = "macos")]
    fn run_menu_commands(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        use crate::menu::Command;
        let Some(menu) = &self.menu else { return };
        for command in menu.take_commands() {
            if let Some(event) = crate::menu::as_input(command) {
                raw_input.events.push(event);
                continue;
            }
            match command {
                Command::New(what) => self.create_new(what),
                Command::Import => self.dialog = Some(Dialog::import()),
                Command::OpenWorkspace => self.pick_workspace(),
                Command::NewWorkspace => self.dialog = Some(Dialog::new_workspace()),
                Command::CheckForUpdates => crate::update::check(self, true),
                Command::Settings(section) => {
                    self.dialog = Some(Dialog::Settings { draft: Box::new(self.settings.clone()), section });
                }
                Command::Url(url) => {
                    crate::widgets::open_web_link(url);
                }
                Command::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Command::Key(..) | Command::Copy | Command::Cut | Command::Paste => {}
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let sc = |m: Modifiers, k: Key| KeyboardShortcut::new(m, k);
        let pressed = |s: KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(&s));
        // egui ignores extra Shift/Alt when matching a shortcut; ⇧⌘↩ belongs
        // to the tab (it sends a WebSocket message), so ⌘↩ must match exactly.
        let exact = |m: Modifiers, k: Key| {
            ctx.input_mut(|i| {
                let hit = i.events.iter().any(|e| match e {
                    egui::Event::Key { key, pressed: true, modifiers, .. } => *key == k && modifiers.matches_exact(m),
                    _ => false,
                });
                hit && i.consume_shortcut(&KeyboardShortcut::new(m, k))
            })
        };
        if exact(cmd, Key::Enter)
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            let id = tab.id;
            match &mut tab.kind {
                TabKind::Request(t) if t.running.is_some() => t.cancel(),
                TabKind::Request(_) => self.send(id),
                // ⌘↩ runs a flow too (or stops the running one).
                TabKind::Flow(t) if t.is_running() => t.stop(),
                TabKind::Flow(t) => t.auto_run = true,
                _ => {}
            }
        }
        if pressed(sc(cmd | Modifiers::SHIFT, Key::P)) {
            self.dialog = Some(Dialog::palette(false));
        }
        if pressed(sc(cmd, Key::K)) {
            self.dialog = Some(Dialog::palette(true));
        }
        if pressed(sc(cmd, Key::N)) || pressed(sc(cmd, Key::T)) {
            self.new_request_tab();
        }
        if pressed(sc(cmd, Key::W)) {
            let a = self.active;
            self.request_close_tab(a);
        }
        if pressed(sc(cmd, Key::B)) {
            self.settings.ui.sidebar_visible = !self.settings.ui.sidebar_visible;
        }
        if pressed(sc(cmd, Key::E)) {
            self.dialog = Some(Dialog::env_switcher());
        }
        if pressed(sc(cmd | Modifiers::ALT, Key::C)) {
            self.console_open = !self.console_open;
        }
        if pressed(sc(cmd, Key::S)) {
            self.save_active();
        }
        if pressed(sc(cmd, Key::Comma)) {
            self.dialog = Some(Dialog::settings(&self.settings));
        }
        // With Shift held, `[` and `]` usually arrive as `{` and `}`.
        let prev = pressed(sc(cmd | Modifiers::SHIFT, Key::OpenBracket))
            || pressed(sc(cmd | Modifiers::SHIFT, Key::OpenCurlyBracket));
        if prev && !self.tabs.is_empty() {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        }
        let next = pressed(sc(cmd | Modifiers::SHIFT, Key::CloseBracket))
            || pressed(sc(cmd | Modifiers::SHIFT, Key::CloseCurlyBracket));
        if next && !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    /// ⌘S: scratch requests ask where to save; saved ones flush to disk.
    pub fn save_active(&mut self) {
        if let Some(tab) = self.tabs.get(self.active) {
            let scratch = tab.kind.location().is_none() && tab.kind.scratch().is_some();
            if scratch {
                self.dialog = Some(Dialog::save_as(tab.id, &self.ws));
                return;
            }
        }
        // Spec files are only written on an explicit save.
        let spec = match self.tabs.get_mut(self.active).map(|t| &mut t.kind) {
            Some(TabKind::Spec(t)) => Some(t.save()),
            _ => None,
        };
        match spec {
            Some(Ok(())) => {}
            Some(Err(e)) => return self.toast(trf("Save failed: {}", &[&e]), true),
            None => self.ws.flush(),
        }
        self.toast(tr("Saved"), false);
    }

    /// Moves a scratch tab's request into a collection.
    /// Returns false (and keeps the request in the tab) when the target
    /// collection or folder does not exist any more.
    pub fn save_scratch_to(&mut self, tab_id: u64, collection: &str, folder: Option<&str>, name: &str) -> bool {
        let Some(idx) = self.tab_index(tab_id) else { return false };
        let Some(c) = self.ws.data.collection_mut(collection) else { return false };
        let Some(at) = c.children_mut(folder).map(|v| v.len()) else { return false };
        let item = self.tabs[idx].kind.scratch_mut().and_then(|s| s.take());
        let Some(mut item) = item else { return false };
        item.name = name.to_string();
        let item_id = item.id.clone();
        c.insert(folder, at, Item::Request(item));
        self.ws.mark_collection(collection);
        let loc = ItemRef { collection: collection.to_string(), item: item_id };
        if let Some(l) = self.tabs[idx].kind.location_mut() {
            *l = Some(loc);
        }
        self.sidebar.expand_to(collection, folder);
        true
    }

    pub fn new_collection(&mut self, name: &str) -> Id {
        let c = Collection::new(name);
        let id = self.ws.add_collection(c);
        self.sidebar.expanded.insert(id.clone());
        id
    }

    pub fn new_environment(&mut self, name: &str) {
        let id = self.ws.add_environment(Environment::new(name));
        self.open_environment(Some(id));
    }

    fn persist_settings(&mut self) {
        if self.settings != self.last_settings_saved {
            if let Err(e) = self.settings.save(&self.settings_path) {
                tracing::warn!("could not save settings: {e}");
            }
            self.last_settings_saved = self.settings.clone();
        }
    }

    pub fn apply_settings(&mut self, mut new: AppSettings) {
        // Passwords go to the Keychain; settings keep a reference.
        Self::vault_settings_secrets(&mut new);
        if new.script_memory_mb != self.settings.script_memory_mb {
            self.scripts = Self::script_engine(&new);
        }
        let engine_changed = new.proxy != self.settings.proxy
            || new.certificates != self.settings.certificates
            || new.max_body_in_memory_bytes != self.settings.max_body_in_memory_bytes;
        crate::i18n::set_language(new.ui.language);
        #[cfg(target_os = "macos")]
        if new.ui.language != self.settings.ui.language
            && let Some(menu) = &mut self.menu
        {
            menu.rebuild();
        }
        self.ctx.set_theme(match new.ui.theme {
            merpati_core::settings::ThemePreference::System => egui::ThemePreference::System,
            merpati_core::settings::ThemePreference::Light => egui::ThemePreference::Light,
            merpati_core::settings::ThemePreference::Dark => egui::ThemePreference::Dark,
        });
        crate::theme::apply(&self.ctx, new.ui.font_size);
        self.settings = new;
        if engine_changed {
            self.http.set_config(Self::engine_config(&self.settings, &self.ws));
        }
        self.persist_settings();
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        use crate::{icons, kit};
        let p = crate::theme::pal(ui);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.add(
                egui::Image::new(egui::include_image!("../assets/icon-256.png"))
                    .fit_to_exact_size(egui::vec2(22.0, 22.0))
                    .corner_radius(5),
            );
            ui.add_space(4.0);
            let name = self.ws.data.name.clone();
            let ws = kit::button_ex(ui, kit::Kind::Ghost, None, &format!("{name}  {}", icons::CARET_DOWN), false)
                .on_hover_text(self.ws.root().display().to_string());
            crate::widgets::a11y_name(&ws, &format!("{} {name}", tr("Workspace")));
            egui::Popup::menu(&ws).show(|ui| {
                if ui.button(kit::with_icon(icons::FOLDER_OPEN, tr("Open workspace…"))).clicked() {
                    ui.close();
                    self.pick_workspace();
                }
                if ui.button(kit::with_icon(icons::FOLDER_PLUS, tr("New workspace…"))).clicked() {
                    ui.close();
                    self.dialog = Some(Dialog::new_workspace());
                }
                // macOS has it in the app menu of the menu bar.
                #[cfg(not(target_os = "macos"))]
                if ui.button(kit::with_icon(icons::ARROWS_CLOCKWISE, tr("Check for updates…"))).clicked() {
                    ui.close();
                    crate::update::check(self, true);
                }
                if ui.button(kit::with_icon(icons::ARROW_SQUARE_OUT, tr("Reveal in file manager"))).clicked() {
                    ui.close();
                    let _ = open::that(self.ws.root());
                }
                let recents: Vec<PathBuf> =
                    self.settings.recent_workspaces.iter().filter(|p| p.as_path() != self.ws.root()).cloned().collect();
                if !recents.is_empty() {
                    ui.separator();
                    ui.label(RichText::new(tr("Recent workspaces")).small().weak());
                    for p in recents {
                        let label = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if ui
                            .button(kit::with_icon(icons::FOLDER_SIMPLE, &label))
                            .on_hover_text(p.display().to_string())
                            .clicked()
                        {
                            ui.close();
                            self.open_workspace(p, None);
                        }
                    }
                }
            });
            kit::vdivider(ui, 18.0);
            let new = kit::button_ex(ui, kit::Kind::Ghost, Some(icons::PLUS), tr("New"), false);
            egui::Popup::menu(&new).show(|ui| {
                ui.set_min_width(220.0);
                for (i, (what, icon, label)) in NewItem::all().into_iter().enumerate() {
                    // Separators before the local tools and before collections.
                    if i > 0 && matches!(what, NewItem::Webhook | NewItem::Collection) {
                        ui.separator();
                    }
                    if ui.button(kit::with_icon(icon, tr(label))).clicked() {
                        ui.close();
                        self.create_new(what);
                    }
                }
            });
            if kit::button_ex(ui, kit::Kind::Ghost, Some(icons::DOWNLOAD_SIMPLE), tr("Import"), false).clicked() {
                self.dialog = Some(Dialog::import());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if kit::icon_button(ui, icons::LOCK_KEY, tr("Vault")).clicked() {
                    self.dialog = Some(Dialog::vault());
                }
                if kit::icon_button(ui, icons::COOKIE, tr("Cookies")).clicked() {
                    self.dialog = Some(Dialog::cookies());
                }
                crate::env_tab::environment_selector(self, ui);
                // Search, centered in the space that is left.
                let free = ui.available_width();
                let w = free.clamp(0.0, 420.0);
                if w > 160.0 {
                    ui.add_space(((free - w) / 2.0).max(0.0));
                    let (rect, r) = ui.allocate_exact_size(egui::vec2(w, 30.0), egui::Sense::click());
                    let r = r.on_hover_text(format!("{} (⌘K)", tr("Search requests")));
                    crate::widgets::a11y_button(&r, tr("Search requests"));
                    let painter = ui.painter();
                    let fill = if r.hovered() { p.surface } else { p.hover };
                    painter.rect(
                        rect,
                        egui::CornerRadius::same(8),
                        fill,
                        egui::Stroke::new(1.0, p.border),
                        egui::StrokeKind::Inside,
                    );
                    painter.text(
                        egui::pos2(rect.left() + 14.0, rect.center().y),
                        egui::Align2::CENTER_CENTER,
                        icons::MAGNIFYING_GLASS,
                        crate::theme::icon_font(14.0),
                        p.text_faint,
                    );
                    painter.text(
                        egui::pos2(rect.left() + 28.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        tr("Search requests"),
                        egui::FontId::proportional(13.0),
                        p.text_faint,
                    );
                    painter.text(
                        egui::pos2(rect.right() - 12.0, rect.center().y),
                        egui::Align2::RIGHT_CENTER,
                        "⌘K",
                        egui::FontId::proportional(12.0),
                        p.text_faint,
                    );
                    if r.clicked() {
                        self.dialog = Some(Dialog::palette(true));
                    }
                }
            });
        });
    }

    /// The icon rail at the far left: sidebar sections and settings.
    fn rail(&mut self, ui: &mut egui::Ui) {
        use crate::{icons, kit};
        ui.vertical_centered(|ui| {
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.y = 4.0;
            for (tab, icon, label) in [
                (SidebarTab::Collections, icons::STACK, tr("Collections")),
                (SidebarTab::Environments, icons::GLOBE, tr("Environments")),
                (SidebarTab::History, icons::CLOCK_COUNTER_CLOCKWISE, tr("History")),
                (SidebarTab::Apis, icons::BRACKETS_CURLY, tr("APIs")),
                (SidebarTab::Flows, icons::FLOW_ARROW, tr("Flows")),
            ] {
                let on = self.settings.ui.sidebar_visible && self.sidebar_tab == tab;
                let r = kit::icon_button_ex(ui, icon, label, on, 36.0);
                crate::widgets::a11y_tab(&r, label, self.sidebar_tab == tab);
                if r.clicked() {
                    if on {
                        self.settings.ui.sidebar_visible = false;
                    } else {
                        self.sidebar_tab = tab;
                        self.settings.ui.sidebar_visible = true;
                    }
                }
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                if kit::icon_button_ex(ui, icons::GEAR_SIX, &format!("{} (⌘,)", tr("Settings")), false, 36.0).clicked()
                {
                    self.dialog = Some(Dialog::settings(&self.settings));
                }
                let errors = self.console.iter().filter(|l| l.kind == merpati_runner::ConsoleKind::Error).count();
                let r = kit::icon_button_ex(
                    ui,
                    icons::TERMINAL_WINDOW,
                    &format!("{} (⌥⌘C)", tr("Console")),
                    self.console_open,
                    36.0,
                );
                if errors > 0 {
                    let c = r.rect.right_top() + egui::vec2(-8.0, 8.0);
                    ui.painter().circle_filled(c, 4.0, crate::theme::pal(ui).err);
                }
                if r.clicked() {
                    self.console_open = !self.console_open;
                }
            });
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        use crate::{icons, kit, theme};
        let p = theme::pal(ui);
        let dark = ui.visuals().dark_mode;
        let mut close = None;
        let mut activate: Option<TabBarAct> = None;
        let mut reorder: Option<(usize, usize)> = None;
        const H: f32 = 38.0;
        let bar = ui.available_rect_before_wrap().with_max_y(ui.cursor().top() + H);
        // Scroll the active tab into view when it changes or the window is resized.
        let in_view = self.tabs.get(self.active).map(|t| (t.id, bar.width() as u32));
        let scroll_to = in_view.filter(|v| self.tab_in_view != Some(*v)).map(|v| v.0);
        self.tab_in_view = in_view;
        ui.painter().rect_filled(bar, 0.0, p.chrome);
        ui.painter().hline(bar.x_range(), bar.bottom() - 0.5, egui::Stroke::new(1.0, p.border));
        // The tabs scroll; the new-tab button follows them and stays in view
        // when they overflow.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            egui::ScrollArea::horizontal()
                .id_salt("tabs")
                .max_height(H)
                .max_width((bar.width() - 36.0).max(0.0))
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.add_space(4.0);
                        for (i, tab) in self.tabs.iter().enumerate() {
                            let (title, badge, dirty) = crate::dialogs::tab_title(tab, &self.ws);
                            let selected = i == self.active;
                            let title_short = crate::dialogs::ellipsize(&title, 26);
                            let lead = match &badge {
                                Some(b) => (b.clone(), theme::semibold(10.5), theme::method_color(b, dark)),
                                None => {
                                    (crate::dialogs::tab_icon(tab).to_string(), theme::icon_font(14.0), p.text_muted)
                                }
                            };
                            let lead_g = ui.painter().layout_no_wrap(lead.0, lead.1, lead.2);
                            let font = if selected { theme::medium(13.0) } else { egui::FontId::proportional(13.0) };
                            let title_g = ui.painter().layout_no_wrap(title_short, font, egui::Color32::PLACEHOLDER);
                            let w = 12.0 + lead_g.size().x + 6.0 + title_g.size().x + 8.0 + 18.0 + 8.0;
                            let (rect, r) =
                                ui.allocate_exact_size(egui::vec2(w.max(96.0), H), egui::Sense::click_and_drag());
                            if selected && scroll_to == Some(tab.id) {
                                r.scroll_to_me(None);
                            }
                            let hovered = ui.rect_contains_pointer(rect);
                            let painter = ui.painter();
                            if selected {
                                let body = rect.shrink2(egui::vec2(0.0, 0.0)).with_max_y(rect.bottom() + 1.0);
                                painter.rect_filled(body, egui::CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, p.canvas);
                                painter.rect_stroke(
                                    body,
                                    egui::CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 },
                                    egui::Stroke::new(1.0, p.border),
                                    egui::StrokeKind::Inside,
                                );
                                // Hide the bar's bottom line under the selected tab.
                                painter.hline(
                                    rect.x_range().shrink(1.0),
                                    rect.bottom() - 0.5,
                                    egui::Stroke::new(1.5, p.canvas),
                                );
                                painter.hline(
                                    rect.x_range().shrink(8.0),
                                    rect.top() + 1.0,
                                    egui::Stroke::new(2.0, p.accent),
                                );
                            } else if hovered {
                                painter.rect_filled(
                                    rect.shrink2(egui::vec2(2.0, 5.0)),
                                    egui::CornerRadius::same(6),
                                    p.hover,
                                );
                            }
                            let mut x = rect.left() + 12.0;
                            painter.galley(
                                egui::pos2(x, rect.center().y - lead_g.size().y / 2.0),
                                lead_g.clone(),
                                p.text_muted,
                            );
                            x += lead_g.size().x + 6.0;
                            let text_color = if selected || hovered { p.text } else { p.text_muted };
                            painter.galley(
                                egui::pos2(x, rect.center().y - title_g.size().y / 2.0),
                                title_g,
                                text_color,
                            );
                            // Close button (or the unsaved dot until hovered).
                            let x_rect = egui::Rect::from_center_size(
                                egui::pos2(rect.right() - 16.0, rect.center().y),
                                egui::vec2(18.0, 18.0),
                            );
                            let x_resp =
                                ui.interact(x_rect, ui.scope_id().with(("tab-close", tab.id)), egui::Sense::click());
                            crate::widgets::a11y_name(&x_resp, &format!("{} {title}", tr("Close")));
                            if dirty && !hovered {
                                ui.painter().circle_filled(x_rect.center(), 3.5, p.accent);
                            } else if hovered || selected {
                                if x_resp.hovered() {
                                    ui.painter().rect_filled(x_rect, egui::CornerRadius::same(4), p.subtle);
                                }
                                ui.painter().text(
                                    x_rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    icons::X,
                                    theme::icon_font(12.0),
                                    if x_resp.hovered() { p.text } else { p.text_muted },
                                );
                            }
                            if x_resp.clicked() {
                                close = Some(i);
                            }
                            let name = if dirty { format!("{title} ({})", tr("unsaved")) } else { title.clone() };
                            crate::widgets::a11y_tab(&r, &name, selected);
                            if r.clicked() && !x_resp.hovered() {
                                activate = Some(TabBarAct::Activate(i));
                            }
                            if r.middle_clicked() {
                                close = Some(i);
                            }
                            if r.dragged()
                                && let Some(pos) = ui.ctx().pointer_interact_pos()
                            {
                                if pos.x > r.rect.right() + 4.0 && i + 1 < self.tabs.len() {
                                    reorder = Some((i, i + 1));
                                } else if pos.x < r.rect.left() - 4.0 && i > 0 {
                                    reorder = Some((i, i - 1));
                                }
                            }
                            r.context_menu(|ui| {
                                if ui.button(kit::with_icon(icons::X, tr("Close"))).clicked() {
                                    close = Some(i);
                                    ui.close();
                                }
                                if ui.button(kit::with_icon(icons::X_CIRCLE, tr("Close other tabs"))).clicked() {
                                    activate = Some(TabBarAct::CloseOthers(i));
                                    ui.close();
                                }
                            });
                        }
                    });
                });
            ui.add_space(4.0);
            ui.allocate_ui_with_layout(
                egui::vec2(32.0, H),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    if kit::icon_button(ui, icons::PLUS, &format!("{} (⌘T)", tr("New Request"))).clicked() {
                        activate = Some(TabBarAct::New);
                    }
                },
            );
        });
        self.apply_tab_bar(reorder, activate, close);
    }

    /// Applies what the tab bar asked for after drawing it.
    fn apply_tab_bar(&mut self, reorder: Option<(usize, usize)>, activate: Option<TabBarAct>, close: Option<usize>) {
        if let Some((a, b)) = reorder {
            self.tabs.swap(a, b);
            if self.active == a {
                self.active = b;
            } else if self.active == b {
                self.active = a;
            }
        }
        match activate {
            Some(TabBarAct::New) => self.new_request_tab(),
            Some(TabBarAct::CloseOthers(keep)) => {
                let keep_id = self.tabs[keep].id;
                let ids: Vec<usize> = (0..self.tabs.len()).rev().filter(|&j| self.tabs[j].id != keep_id).collect();
                // Tabs with unsaved work stay open.
                let mut kept = 0;
                for j in ids {
                    if self.tab_has_unsaved_work(j) {
                        kept += 1;
                    } else {
                        self.close_tab(j);
                    }
                }
                self.active = self.tab_index(keep_id).unwrap_or(0);
                if kept > 0 {
                    self.toast(trf("{} tabs with unsaved changes were kept open.", &[&kept]), false);
                }
            }
            Some(TabBarAct::Activate(i)) => self.active = i,
            None => {}
        }
        if let Some(i) = close {
            self.request_close_tab(i);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        use crate::{icons, kit, theme};
        let p = theme::pal(ui);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let r = kit::icon_button_ex(
                ui,
                icons::SIDEBAR_SIMPLE,
                tr("Toggle sidebar (⌘B)"),
                self.settings.ui.sidebar_visible,
                22.0,
            );
            if r.clicked() {
                self.settings.ui.sidebar_visible = !self.settings.ui.sidebar_visible;
            }
            let errors = self.console.iter().filter(|l| l.kind == merpati_runner::ConsoleKind::Error).count();
            let label = if errors > 0 {
                format!("{}  {}  ·  {errors} {}", icons::TERMINAL_WINDOW, tr("Console"), icons::WARNING_CIRCLE)
            } else {
                format!("{}  {}", icons::TERMINAL_WINDOW, tr("Console"))
            };
            let color = if errors > 0 {
                p.err
            } else if self.console_open {
                p.text
            } else {
                p.text_muted
            };
            let r =
                ui.add(egui::Button::new(RichText::new(label).small().color(color)).frame(false)).on_hover_text("⌥⌘C");
            if r.clicked() {
                self.console_open = !self.console_open;
            }
            if let Some(e) = &self.ws.last_error {
                ui.label(RichText::new(trf("Save failed: {}", &[&e])).small().color(p.err));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let side = self.settings.ui.layout == merpati_core::settings::ResponseLayout::Side;
                let r = kit::icon_button_ex(
                    ui,
                    if side { icons::SPLIT_HORIZONTAL } else { icons::SPLIT_VERTICAL },
                    tr("Toggle response position"),
                    false,
                    22.0,
                );
                if r.clicked() {
                    self.settings.ui.layout = if side {
                        merpati_core::settings::ResponseLayout::Below
                    } else {
                        merpati_core::settings::ResponseLayout::Side
                    };
                }
                let name = format!("{} (⇧⌘P)", tr("Command palette"));
                if kit::icon_button_ex(ui, icons::COMMAND, &name, false, 22.0).clicked() {
                    self.dialog = Some(Dialog::palette(false));
                }
                crate::update::status_pill(self, ui);
                ui.add_space(6.0);
                let (icon, status) = if self.ws.has_pending() {
                    (icons::CIRCLE_NOTCH, tr("Saving…"))
                } else {
                    (icons::CHECK, tr("Saved"))
                };
                ui.label(RichText::new(format!("{icon} {status}")).small().color(p.text_faint));
                ui.add_space(6.0);
                let ws = self.ws.data.name.clone();
                ui.label(RichText::new(format!("{}  {ws}", icons::FOLDER_SIMPLE)).small().color(p.text_muted))
                    .on_hover_text(self.ws.root().display().to_string());
            });
        });
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        use crate::{icons, theme};
        self.toasts.retain(|t| t.at.elapsed() < Duration::from_secs(if t.error { 8 } else { 3 }));
        if self.toasts.is_empty() {
            return;
        }
        let p = theme::palette(ctx.global_style().visuals.dark_mode);
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -40.0))
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| {
                for t in &self.toasts {
                    let (icon, color) =
                        if t.error { (icons::WARNING_CIRCLE, p.err) } else { (icons::CHECK_CIRCLE, p.ok) };
                    egui::Frame::popup(ui.style())
                        .fill(p.surface)
                        .corner_radius(egui::CornerRadius::same(10))
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_max_width(400.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(icon).font(theme::icon_font(18.0)).color(color));
                                ui.add(egui::Label::new(RichText::new(&t.text).color(p.text)).wrap());
                            });
                        });
                    ui.add_space(6.0);
                }
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// The start page when no tab is open: quick actions, shortcuts and
    /// recently sent requests.
    fn home(&mut self, ui: &mut egui::Ui) {
        use crate::{icons, kit, theme};
        let p = theme::pal(ui);
        egui::ScrollArea::vertical().id_salt("home").auto_shrink([false; 2]).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.08).clamp(16.0, 72.0));
                ui.add(
                    egui::Image::new(egui::include_image!("../assets/icon-256.png"))
                        .fit_to_exact_size(egui::vec2(56.0, 56.0))
                        .corner_radius(12),
                );
                ui.add_space(10.0);
                ui.label(RichText::new("Merpati").font(theme::semibold(24.0)).color(p.text));
                ui.label(
                    RichText::new(tr("A fast, local-first API client. Start with one of these:")).color(p.text_muted),
                );
                ui.add_space(18.0);
            });
            let width = ui.available_width().min(760.0);
            let left = ((ui.available_width() - width) / 2.0).max(0.0);
            ui.horizontal(|ui| {
                ui.add_space(left);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    let mut actions: Vec<(&str, String, &str, u8)> = vec![
                        (icons::GLOBE, tr("HTTP request").to_string(), tr("REST, GraphQL, SOAP"), 0),
                        (icons::PLUGS_CONNECTED, tr("WebSocket").to_string(), tr("Messages both ways"), 1),
                    ];
                    for (label, name, _) in crate::app::protocol_templates() {
                        let _ = label;
                        actions.push((protocol_icon(name), name.to_string(), "", 2));
                    }
                    actions.push((
                        icons::DOWNLOAD_SIMPLE,
                        tr("Import").to_string(),
                        tr("Postman, OpenAPI, cURL, …"),
                        3,
                    ));
                    actions.push((icons::STACK, tr("New Collection").to_string(), tr("Organize requests"), 4));
                    actions.push((icons::FLOW_ARROW, tr("New flow").to_string(), tr("Chain requests visually"), 5));
                    let cols = if width > 600.0 { 3 } else { 2 };
                    let cell = (width - (cols as f32 - 1.0) * 10.0) / cols as f32;
                    let templates = crate::app::protocol_templates();
                    for chunk in actions.chunks(cols) {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 10.0;
                            for (icon, title, sub, kind) in chunk {
                                let (rect, r) = ui.allocate_exact_size(egui::vec2(cell, 64.0), egui::Sense::click());
                                crate::widgets::a11y_button(&r, title);
                                let hovered = r.hovered();
                                let painter = ui.painter();
                                painter.rect(
                                    rect,
                                    egui::CornerRadius::same(10),
                                    if hovered { p.hover } else { p.surface },
                                    egui::Stroke::new(1.0, if hovered { p.border_strong } else { p.border }),
                                    egui::StrokeKind::Inside,
                                );
                                let icon_rect = egui::Rect::from_center_size(
                                    egui::pos2(rect.left() + 30.0, rect.center().y),
                                    egui::vec2(34.0, 34.0),
                                );
                                painter.rect_filled(icon_rect, egui::CornerRadius::same(8), p.accent_soft);
                                painter.text(
                                    icon_rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    *icon,
                                    theme::icon_font(18.0),
                                    p.accent_text,
                                );
                                let tx = rect.left() + 58.0;
                                let sub = if sub.is_empty() { tr("Connect and test") } else { sub };
                                painter.text(
                                    egui::pos2(tx, rect.center().y - 9.0),
                                    egui::Align2::LEFT_CENTER,
                                    title,
                                    theme::medium(13.5),
                                    p.text,
                                );
                                painter.text(
                                    egui::pos2(tx, rect.center().y + 10.0),
                                    egui::Align2::LEFT_CENTER,
                                    sub,
                                    egui::FontId::proportional(12.0),
                                    p.text_muted,
                                );
                                if r.clicked() {
                                    match kind {
                                        0 => self.new_request_tab(),
                                        1 => self.new_ws_tab(),
                                        2 => {
                                            if let Some((_, name, k)) = templates.iter().find(|(_, n, _)| n == title) {
                                                self.new_proto_tab(name, k.clone());
                                            }
                                        }
                                        3 => self.dialog = Some(Dialog::import()),
                                        4 => self.dialog = Some(Dialog::prompt_new_collection()),
                                        _ => {
                                            let n = self.sidebar.flows.as_ref().map(|f| f.len()).unwrap_or(0) + 1;
                                            self.add_tab(TabKind::Flow(Box::new(crate::flow_tab::FlowTab::blank(
                                                &format!("{} {n}", tr("Flow")),
                                            ))));
                                        }
                                    }
                                }
                            }
                        });
                        ui.add_space(10.0);
                    }
                    ui.add_space(14.0);
                    ui.columns(2, |cols| {
                        cols[0].label(kit::section_title(tr("Keyboard shortcuts")));
                        cols[0].add_space(4.0);
                        for (keys, what) in [
                            ("⌘N", tr("New request")),
                            ("⌘↩", tr("Send request")),
                            ("⌘K", tr("Search requests")),
                            ("⌘E", tr("Switch environment")),
                            ("⇧⌘P", tr("Command palette")),
                            ("⌘B", tr("Toggle sidebar")),
                        ] {
                            cols[0].horizontal(|ui| {
                                ui.set_width(220.0);
                                ui.label(RichText::new(what).color(p.text_muted));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    kit::kbd(ui, keys)
                                });
                            });
                        }
                        cols[1].label(kit::section_title(tr("Recent requests")));
                        cols[1].add_space(4.0);
                        let recent: Vec<(String, String, String)> = self
                            .ws
                            .history
                            .iter()
                            .take(6)
                            .map(|h| (h.id.clone(), h.request.method.clone(), h.request.url.clone()))
                            .collect();
                        if recent.is_empty() {
                            cols[1].label(RichText::new(tr("Requests you send appear here.")).color(p.text_muted));
                        }
                        let dark = cols[1].visuals().dark_mode;
                        for (id, method, url) in recent {
                            let r = cols[1]
                                .horizontal(|ui| {
                                    ui.add_sized(
                                        [46.0, 18.0],
                                        egui::Label::new(crate::widgets::method_badge(&method, dark)),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(crate::dialogs::ellipsize(&url, 48)).color(p.text),
                                        )
                                        .truncate()
                                        .sense(egui::Sense::click()),
                                    )
                                })
                                .inner;
                            if r.clicked() {
                                self.open_history(&id);
                            }
                        }
                    });
                });
            });
        });
    }

    /// Opens a history entry in a new unsaved tab.
    pub fn open_history(&mut self, id: &str) {
        let Some(h) = self.ws.history.iter().find(|h| h.id == id).cloned() else { return };
        let name = h.name.clone().unwrap_or_else(|| h.request.url.clone());
        let item = RequestItem::new(name, h.request.clone());
        self.add_tab(TabKind::Request(Box::new(RequestTab::scratch(item))));
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        if self.tabs.is_empty() {
            self.home(ui);
            return;
        }
        let idx = self.active.min(self.tabs.len() - 1);
        self.active = idx;
        let tab_id = self.tabs[idx].id;
        // Temporarily take the tab out so it can borrow the app mutably.
        let mut tab = std::mem::replace(
            &mut self.tabs[idx],
            Tab { id: 0, kind: TabKind::Environment(crate::env_tab::EnvTab::default()) },
        );
        // Widget state (text-edit undo history, bulk edit, revealed secrets,
        // scroll positions) is kept per tab.
        ui.push_id(("tab", tab_id), |ui| match &mut tab.kind {
            TabKind::Request(t) => crate::request_tab::show(self, ui, tab_id, t),
            TabKind::WebSocket(t) => crate::ws_tab::show(self, ui, tab_id, t),
            TabKind::Container(t) => crate::container_tab::show(self, ui, t),
            TabKind::Environment(t) => crate::env_tab::show(self, ui, t),
            TabKind::Runner(t) => crate::runner_tab::show(self, ui, tab_id, t),
            TabKind::Mock(t) => crate::mock_tab::show(self, ui, tab_id, t),
            TabKind::Spec(t) => crate::spec_tab::show(self, ui, t),
            TabKind::Mqtt(t) => crate::mqtt_tab::show(self, ui, tab_id, t),
            TabKind::Grpc(t) => crate::grpc_tab::show(self, ui, tab_id, t),
            TabKind::SocketIo(t) => crate::sio_tab::show(self, ui, tab_id, t),
            TabKind::Mcp(t) => crate::mcp_tab::show(self, ui, tab_id, t),
            TabKind::Ai(t) => crate::ai_tab::show(self, ui, tab_id, t),
            TabKind::Webhook(t) => crate::webhook_tab::show(self, ui, tab_id, t),
            TabKind::Flow(t) => crate::flow_tab::show(self, ui, tab_id, t),
            TabKind::Capture(t) => crate::capture_tab::show(self, ui, tab_id, t),
        });
        // The tab may have been closed or moved while shown.
        if let Some(slot) = self.tabs.iter_mut().find(|t| t.id == 0) {
            *slot = tab;
        }
    }
}

impl eframe::App for MerpatiApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        #[cfg(target_os = "macos")]
        self.run_menu_commands(ctx, raw_input);
        self.axis_lock.apply(ctx, raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_msgs();
        self.shortcuts(&ctx);
        crate::automation::tick(self, &ctx);

        let p = crate::theme::pal(ui);
        let chrome = |m: egui::Margin| egui::Frame::new().fill(p.chrome).inner_margin(m);
        egui::Panel::top("top").exact_size(46.0).frame(chrome(egui::Margin::symmetric(12, 0))).show(ui, |ui| {
            self.top_bar(ui);
        });
        egui::Panel::bottom("status")
            .exact_size(26.0)
            .frame(chrome(egui::Margin::symmetric(8, 0)))
            .show(ui, |ui| self.status_bar(ui));
        if self.console_open {
            egui::Panel::bottom("console")
                .resizable(true)
                .default_size(220.0)
                .min_size(80.0)
                .frame(egui::Frame::new().fill(p.canvas).inner_margin(egui::Margin::symmetric(12, 8)))
                .show(ui, |ui| {
                    crate::dialogs::console_ui(self, ui);
                });
        }
        egui::Panel::left("rail")
            .exact_size(52.0)
            .resizable(false)
            .frame(chrome(egui::Margin::ZERO))
            .show(ui, |ui| self.rail(ui));
        let mut sidebar_visible = self.settings.ui.sidebar_visible;
        // At most a little over a quarter of the window, so small windows keep room for the editor.
        let sidebar_max = (ui.available_width() * 0.28).clamp(220.0, 520.0);
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(280.0_f32.min(sidebar_max))
            .size_range(200.0..=sidebar_max)
            .frame(chrome(egui::Margin { left: 10, right: 10, top: 10, bottom: 6 }))
            .show_collapsible(ui, &mut sidebar_visible, |ui| crate::sidebar::show(self, ui));
        self.settings.ui.sidebar_visible = sidebar_visible;
        egui::CentralPanel::no_frame().frame(egui::Frame::new().fill(p.canvas)).show(ui, |ui| {
            self.tab_bar(ui);
            egui::Frame::new()
                .inner_margin(egui::Margin { left: 18, right: 18, top: 12, bottom: 8 })
                .show(ui, |ui| self.central(ui));
        });
        for action in std::mem::take(&mut self.deferred) {
            match action {
                Deferred::Send(id) => self.send(id),
                Deferred::RerunTests(id) => self.rerun_tests(id),
            }
        }

        crate::dialogs::show(self, &ctx);
        crate::update::poll_requests(self, &ctx);
        crate::update::dialog(self, &ctx);
        self.toasts_ui(&ctx);

        // Autosave and periodic upkeep.
        if let Some(wait) = self.ws.autosave() {
            ctx.request_repaint_after(wait);
        }
        let running = self.tabs.iter().any(|t| match &t.kind {
            TabKind::Request(r) => r.running.is_some(),
            TabKind::Runner(r) => r.is_running(),
            TabKind::Mock(m) => m.server.is_some(),
            TabKind::Webhook(w) => w.is_running(),
            TabKind::Ai(a) => a.is_running(),
            TabKind::Flow(f) => f.is_running(),
            TabKind::Capture(c) => c.is_running(),
            _ => false,
        });
        if running {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        // Crash recovery: the open tabs (unsaved ones included) are written
        // shortly after input, not only on exit.
        if ctx.input(|i| !i.events.is_empty()) {
            self.mark_session_dirty();
        }
        if let Some(since) = self.session_dirty {
            let wait = Duration::from_secs(2).saturating_sub(since.elapsed());
            if wait.is_zero() {
                self.session_dirty = None;
                crate::dialogs::save_session(self);
            } else {
                ctx.request_repaint_after(wait);
            }
        }
        self.persist_settings();
    }

    #[cfg(feature = "glow")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }

    #[cfg(not(feature = "glow"))]
    fn on_exit(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
impl MerpatiApp {
    /// An app on a new workspace in `root` (in-memory secrets, no settings
    /// file), for tests.
    pub fn for_test(root: &std::path::Path) -> MerpatiApp {
        let mut settings = AppSettings::default();
        settings.proxy.use_system = false;
        let ws = WsState::for_test(root);
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("runtime");
        let http = Arc::new(Self::engine(&settings, &ws));
        let (tx, rx) = mpsc::channel();
        MerpatiApp {
            rt,
            last_settings_saved: settings.clone(),
            settings,
            settings_path: root.join("settings.json"),
            ws,
            http,
            scripts: Arc::new(merpati_script::BoaEngine::new()),
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 1,
            sidebar_tab: SidebarTab::Collections,
            sidebar: Default::default(),
            console: Vec::new(),
            console_open: false,
            console_filter: Default::default(),
            tx,
            rx,
            dialog: None,
            toasts: Vec::new(),
            ctx: egui::Context::default(),
            pending_tokens: Default::default(),
            automation: None,
            pending_runner_start: None,
            last_session: Vec::new(),
            session_dirty: None,
            deferred: Vec::new(),
            scratch_baseline: Default::default(),
            close_after_save: None,
            update: Default::default(),
            tab_in_view: None,
            axis_lock: Default::default(),
            #[cfg(target_os = "macos")]
            menu: None,
        }
    }

    /// Whether the automation script (if any) has run all its steps.
    pub fn automation_finished(&self) -> bool {
        self.automation.as_ref().is_none_or(crate::automation::Automation::finished)
    }

    /// Draws the active tab and the open dialog for one frame (1280×820,
    /// headless) with `events`, for tests.
    pub fn draw_frame(&mut self, events: Vec<egui::Event>) -> DrawnFrame {
        let ctx = self.ctx.clone();
        ctx.enable_accesskit();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default_margins().show(ui, |ui| self.central(ui));
            crate::dialogs::show(self, ui.ctx());
        });
        let nodes = out.platform_output.accesskit_update.take().map(|u| u.nodes).unwrap_or_default();
        let labels = nodes.iter().filter_map(|(_, n)| n.label().or(n.value()).map(str::to_string)).collect();
        let shapes = out.shapes.len();
        out.drop_without_applying_deltas();
        DrawnFrame { shapes, labels }
    }
}

/// What [`MerpatiApp::draw_frame`] drew: the number of shapes and the
/// names (or texts) screen readers get.
#[cfg(test)]
pub struct DrawnFrame {
    pub shapes: usize,
    pub labels: Vec<String>,
}

impl MerpatiApp {
    fn shutdown(&mut self) {
        crate::dialogs::save_session(self);
        self.ws.flush_blocking();
        self.persist_settings();
        for tab in &self.tabs {
            if let TabKind::Mock(m) = &tab.kind {
                m.stop(&self.rt);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_raw_and_encoded_secrets() {
        let secrets = vec!["s3cr3t/key".to_string()];
        let url = "https://api.test/x?key=s3cr3t/key&k2=s3cr3t%2Fkey";
        assert_eq!(redact(url, &secrets), "https://api.test/x?key=•••&k2=•••");
    }

    #[test]
    fn settings_passwords_live_in_the_keychain() {
        let mut s = AppSettings::default();
        s.proxy.password = "hunter2".into();
        s.certificates.client_certs.push(merpati_core::settings::ClientCertificate {
            host: "api.example.com".into(),
            passphrase: "p12-pass".into(),
            ..Default::default()
        });
        assert!(MerpatiApp::vault_settings_secrets(&mut s));
        assert_eq!(s.proxy.password, "{{vault:settings.proxy-password}}");
        assert_eq!(s.certificates.client_certs[0].passphrase, "{{vault:settings.client-cert-api.example.com}}");
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("hunter2") && !json.contains("p12-pass"), "{json}");
        assert!(!MerpatiApp::vault_settings_secrets(&mut s));

        // Clients get the password itself, in any workspace.
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let mut new = app.settings.clone();
        new.proxy = merpati_core::settings::ProxySettings {
            custom_enabled: true,
            custom_url: "http://proxy.test:3128".into(),
            username: "alice".into(),
            password: "s3cret".into(),
            ..Default::default()
        };
        app.apply_settings(new);
        assert_eq!(app.settings.proxy.password, "{{vault:settings.proxy-password}}");
        assert_eq!(app.proxy_settings().password, "s3cret");
        let saved = std::fs::read_to_string(&app.settings_path).unwrap();
        assert!(!saved.contains("s3cret"), "{saved}");
    }

    #[test]
    fn slim_collection_keeps_only_the_path() {
        let mut c = Collection::new("C");
        let mut f = Folder::new("F");
        let r = RequestItem::new("R", HttpRequest::new("GET", "http://x"));
        let id = r.id.clone();
        f.items.push(Item::Request(r));
        f.items.push(Item::Request(RequestItem::new("Other", HttpRequest::new("GET", "http://y"))));
        c.items.push(Item::Folder(f));
        c.items.push(Item::Request(RequestItem::new("Root", HttpRequest::new("GET", "http://z"))));
        let slim = slim_collection(&c, &id);
        assert_eq!(slim.request_count(), 1);
        assert!(slim.find(&id).is_some());
        assert_eq!(slim.ancestors(&id).unwrap().len(), 2);
    }

    fn scratch_tab(app: &mut MerpatiApp, name: &str) -> u64 {
        let item = RequestItem::new(name, HttpRequest::new("GET", format!("http://{name}.test")));
        app.add_tab(TabKind::Request(Box::new(RequestTab::scratch(item))))
    }

    fn tab_names(app: &MerpatiApp) -> Vec<String> {
        app.tabs.iter().map(|t| crate::dialogs::tab_title(t, &app.ws).0).collect()
    }

    /// "Close other tabs" from the context menu of the first tab.
    #[test]
    fn close_other_tabs_keeps_the_first_tab() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        for n in ["a", "b", "c"] {
            scratch_tab(&mut app, n);
        }
        app.apply_tab_bar(None, Some(TabBarAct::CloseOthers(0)), None);
        assert_eq!(tab_names(&app), ["a"]);
        assert_eq!(app.active, 0);
        app.apply_tab_bar(None, Some(TabBarAct::New), None);
        assert_eq!(tab_names(&app), ["a", "Untitled Request"]);
    }

    #[test]
    fn closing_asks_only_when_work_would_be_lost() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let edited = scratch_tab(&mut app, "edited");
        let untouched = scratch_tab(&mut app, "untouched");
        let i = app.tab_index(edited).unwrap();
        if let TabKind::Request(t) = &mut app.tabs[i].kind
            && let Some(item) = t.scratch.as_mut()
            && let RequestKind::Http(h) = &mut item.request
        {
            h.set_url("http://edited.test/typed".into());
        }
        // An untouched new tab just closes.
        app.request_close_tab(app.tab_index(untouched).unwrap());
        assert_eq!(tab_names(&app), ["edited"]);
        assert!(app.dialog.is_none());
        // An edited one asks first and stays open.
        app.request_close_tab(app.tab_index(edited).unwrap());
        assert!(matches!(app.dialog, Some(Dialog::CloseUnsaved { tab }) if tab == edited));
        assert_eq!(tab_names(&app), ["edited"]);
        // "Close other tabs" keeps it open too.
        app.dialog = None;
        let other = scratch_tab(&mut app, "other");
        app.apply_tab_bar(None, Some(TabBarAct::CloseOthers(app.tab_index(other).unwrap())), None);
        assert_eq!(tab_names(&app), ["edited", "other"]);
        assert_eq!(app.active, app.tab_index(other).unwrap());
        // Saving it from the prompt closes it afterwards.
        let cid = app.new_collection("C");
        app.close_after_save = Some(edited);
        assert!(app.save_scratch_to(edited, &cid, None, "Edited"));
        assert!(app.tab_index(edited).is_some(), "saving alone keeps the tab");
    }

    /// Runs the global shortcuts on one key press; returns true when the key
    /// is still there for the tab afterwards.
    fn press(app: &mut MerpatiApp, key: Key, modifiers: Modifiers) -> bool {
        let ctx = app.ctx.clone();
        let raw = egui::RawInput {
            events: vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }],
            ..Default::default()
        };
        let mut left = false;
        ctx.run_ui(raw, |ui| {
            app.shortcuts(ui.ctx());
            left = ui.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(modifiers, key)));
        })
        .drop_without_applying_deltas();
        left
    }

    /// ⇧⌘↩ sends a WebSocket message; the global ⌘↩ must not swallow it.
    #[test]
    fn shift_cmd_enter_reaches_the_websocket_tab() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        app.new_ws_tab();
        assert!(press(&mut app, Key::Enter, Modifiers::COMMAND | Modifiers::SHIFT));
        assert!(!press(&mut app, Key::Enter, Modifiers::COMMAND));
    }

    /// With Shift held the `[`/`]` keys arrive as `{`/`}` (macOS, US layout).
    #[test]
    fn shift_cmd_brackets_switch_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        for n in ["a", "b", "c"] {
            scratch_tab(&mut app, n);
        }
        assert_eq!(app.active, 2);
        press(&mut app, Key::OpenCurlyBracket, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(app.active, 1);
        press(&mut app, Key::CloseCurlyBracket, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(app.active, 2);
        press(&mut app, Key::CloseBracket, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(app.active, 0);
    }

    /// A pre-request script that copies a secret variable into a header (the
    /// usual `Bearer ' + pm.environment.get('token')`) must not put the
    /// secret into the history file; neither when the user switched
    /// environments while the request was in flight.
    #[test]
    fn history_does_not_keep_secret_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let mut env = Environment::new("Dev");
        env.variables.push(merpati_core::variables::Variable::secret("token", "s3cr3t-token"));
        let env = app.ws.add_environment(env);
        app.ws.set_active_environment(Some(env.clone()));
        let before = app.ws.scopes_for(None);
        app.ws.set_active_environment(None);
        let mut req = HttpRequest::new("GET", "");
        req.set_url("https://api.test/x?key=s3cr3t-token".into());
        req.headers.push(merpati_core::model::KeyValue::new("Authorization", "Bearer s3cr3t-token"));
        req.body = merpati_core::model::Body::raw_json("{\"t\": \"s3cr3t-token\"}");
        let exec = Execution {
            request: Some(req),
            sent: Some(merpati_http::SentRequest {
                url: "https://api.test/x?key=s3cr3t-token".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let msg = Msg::Request {
            tab: 0,
            exec: Box::new(exec),
            after: before.clone(),
            before,
            collection: None,
            env: Some(env),
            rerun: false,
        };
        app.tx.send(msg).unwrap();
        app.handle_msgs();
        let line = serde_json::to_string(&app.ws.history[0]).unwrap();
        assert!(!line.contains("s3cr3t-token"), "{line}");
    }

    /// Draws the tab area for one frame; returns the labelled widgets.
    fn frame(app: &mut MerpatiApp, events: Vec<egui::Event>) -> Vec<(String, egui::Pos2)> {
        let ctx = app.ctx.clone();
        ctx.enable_accesskit();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default_margins().show(ui, |ui| app.central(ui));
        });
        let nodes = out.platform_output.accesskit_update.take().map(|u| u.nodes).unwrap_or_default();
        out.drop_without_applying_deltas();
        nodes
            .iter()
            .filter_map(|(_, n)| {
                let b = n.bounds()?;
                let text = n.label().or(n.value())?.to_string();
                Some((text, egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32)))
            })
            .collect()
    }

    fn click(app: &mut MerpatiApp, label: &str) {
        let widgets = frame(app, Vec::new());
        let Some((_, pos)) = widgets.iter().find(|(l, _)| l == label) else { panic!("no {label:?} in {widgets:?}") };
        let pos = *pos;
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(app, vec![egui::Event::PointerMoved(pos), button(true)]);
        frame(app, vec![button(false)]);
    }

    /// Widget state (bulk edit mode and text, text-edit undo history, revealed
    /// secrets) belongs to one tab: turning on bulk edit of the params in one
    /// request must not show that request's params in another tab.
    #[test]
    fn widget_state_is_kept_per_tab() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        scratch_tab(&mut app, "a");
        scratch_tab(&mut app, "b");
        app.active = 0;
        click(&mut app, tr("Bulk Edit"));
        let labels: Vec<String> = frame(&mut app, Vec::new()).into_iter().map(|(l, _)| l).collect();
        assert!(labels.iter().any(|l| l == tr("Key-Value Edit")), "{labels:?}");
        app.active = 1;
        frame(&mut app, Vec::new());
        let labels: Vec<String> = frame(&mut app, Vec::new()).into_iter().map(|(l, _)| l).collect();
        assert!(labels.iter().any(|l| l == tr("Bulk Edit")), "{labels:?}");
        assert!(!labels.iter().any(|l| l == tr("Key-Value Edit")), "{labels:?}");
    }

    /// A listener that cannot start (port in use) says so; the error used to
    /// be shown only next to a selected request, which cannot exist then.
    #[test]
    fn webhook_start_error_is_shown() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let id = app.add_tab(TabKind::Webhook(Box::default()));
        app.tx
            .send(Msg::Webhook {
                tab: id,
                event: crate::webhook_tab::WebhookMsg::Started(Err("address already in use".into())),
            })
            .unwrap();
        app.handle_msgs();
        frame(&mut app, Vec::new());
        let labels: Vec<String> = frame(&mut app, Vec::new()).into_iter().map(|(l, _)| l).collect();
        assert!(labels.iter().any(|l| l.contains("address already in use")), "{labels:?}");
    }

    /// Switching workspaces stops what the old tabs were running, like
    /// closing them does (requests whose scripts would otherwise write
    /// variables and history into the new workspace, mock servers, ...).
    #[test]
    fn switching_workspace_stops_running_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(&dir.path().join("one"));
        scratch_tab(&mut app, "a");
        let cancel = CancellationToken::new();
        let TabKind::Request(t) = &mut app.tabs[0].kind else { panic!("request tab") };
        t.start_running(cancel.clone());
        let other = WsState::for_test(&dir.path().join("two"));
        let other_root = other.root().to_path_buf();
        app.switch_workspace(other, &other_root);
        assert_eq!(app.ws.root(), other_root);
        assert_eq!(tab_names(&app), ["Untitled Request"]);
        assert!(cancel.is_cancelled());
    }

    /// On-message scripts of a busy stream add console lines, test results
    /// and errors for every message; the console and the tab keep the latest.
    #[test]
    fn protocol_script_output_is_bounded() {
        use merpati_core::script::{ConsoleEntry, LogLevel, ScriptError, ScriptEvent, ScriptOutcome, TestResult};
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let id = app.add_tab(TabKind::Mqtt(Box::new(crate::mqtt_tab::MqttTab::new(Default::default()))));
        for i in 0..3000 {
            let test = TestResult { name: format!("t{i}"), passed: true, error: None, skipped: false };
            let error = ScriptError { location: "on-message".into(), message: format!("e{i}") };
            let outcome = ScriptOutcome {
                tests: vec![test.clone(), test],
                console: vec![ConsoleEntry { level: LogLevel::Log, message: format!("m{i}") }],
                errors: vec![error.clone(), error],
                ..Default::default()
            };
            let run = crate::proto_common::ScriptRun {
                event: ScriptEvent::OnMessage,
                outcome,
                before: VariableScopes::default(),
                collection: None,
                env: None,
                name: "mqtt".into(),
            };
            app.tx.send(Msg::ProtoScript { tab: id, run: Box::new(run) }).unwrap();
        }
        app.handle_msgs();
        assert_eq!(app.console.len(), MAX_CONSOLE);
        assert_eq!(app.console.last().map(|l| l.message.as_str()), Some("on-message: e2999"));
        let s = app.tabs[0].kind.script_state_mut().unwrap();
        assert_eq!(s.tests.len(), crate::proto_common::MAX_SCRIPT_RESULTS);
        assert_eq!(s.errors.len(), crate::proto_common::MAX_SCRIPT_RESULTS);
        assert_eq!(s.tests.back().map(|t| t.name.as_str()), Some("t2999"));
        assert_eq!(s.tests.front().map(|t| t.name.as_str()), Some("t500"));
    }

    /// Typed variables (Postman `type`): a collection variable declared as a
    /// number is a number in the request's scripts, and a number a script
    /// sets is stored with its type (`any` holding 5 for a new variable, as in
    /// Postman; a declared number stays a number).
    #[test]
    fn scripts_see_and_set_typed_variables() {
        use merpati_core::variables::{VarType, Variable};
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Some(mut s) = listener.incoming().flatten().next() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let mut dev = Environment::new("Dev");
        dev.variables.push(Variable { ty: VarType::Number, ..Variable::new("count", "1") });
        let env = app.ws.add_environment(dev);
        app.ws.set_active_environment(Some(env.clone()));
        let cid = app.new_collection("C");
        let mut req = RequestItem::new("typed", HttpRequest::new("GET", format!("http://127.0.0.1:{port}/")));
        req.scripts.pre_request = "pm.environment.set('seen', typeof pm.collectionVariables.get('limit'));\n\
                                   pm.environment.set('n', pm.collectionVariables.get('limit') / 2);\n\
                                   pm.environment.set('count', '7');"
            .into();
        let rid = req.id.clone();
        let c = app.ws.data.collection_mut(&cid).unwrap();
        c.variables.push(Variable { ty: VarType::Number, ..Variable::new("limit", "10") });
        c.items.push(Item::Request(req));
        app.open_item(ItemRef { collection: cid, item: rid });
        let tab = app.tabs[app.active].id;
        app.send(tab);
        let deadline = Instant::now() + Duration::from_secs(30);
        while matches!(&app.tabs[app.active].kind, TabKind::Request(t) if t.running.is_some()) {
            assert!(Instant::now() < deadline, "the request did not finish");
            std::thread::sleep(Duration::from_millis(20));
            app.handle_msgs();
        }
        let vars = &app.ws.data.environment(&env).unwrap().variables;
        let var = |k: &str| vars.iter().find(|v| v.key == k).unwrap_or_else(|| panic!("no {k} in {vars:?}"));
        assert_eq!(var("seen").effective(), "number");
        assert_eq!(var("n").effective(), "5");
        assert_eq!(var("n").ty, VarType::Any);
        assert_eq!(var("seen").ty, VarType::Default);
        assert_eq!((var("count").effective(), var("count").ty), ("7", VarType::Number));
    }

    /// Saving into a collection or folder deleted while the Save dialog was
    /// open keeps the unsaved request in its tab.
    #[test]
    fn save_as_into_a_missing_target_keeps_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let tab = scratch_tab(&mut app, "a");
        let cid = app.new_collection("C");
        assert!(!app.save_scratch_to(tab, "deleted-collection", None, "Saved"));
        assert!(!app.save_scratch_to(tab, &cid, Some("deleted-folder"), "Saved"));
        assert!(app.tabs[0].kind.scratch().is_some());
        assert!(app.tabs[0].kind.location().is_none());
        assert_eq!(app.ws.data.collection(&cid).unwrap().request_count(), 0);
        assert!(app.save_scratch_to(tab, &cid, None, "Saved"));
        assert!(app.tabs[0].kind.scratch().is_none());
        assert_eq!(tab_names(&app), ["Saved"]);
    }
}
