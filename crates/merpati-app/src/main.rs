#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai_tab;
mod app;
mod auth_ui;
mod automation;
mod body_view;
mod capture_tab;
mod container_tab;
mod crash;
mod dialogs;
mod env_tab;
mod flow_tab;
mod fork_ui;
mod grpc_tab;
mod highlight;
mod i18n;
mod icons;
mod kit;
mod mcp_tab;
#[cfg(target_os = "macos")]
mod menu;
mod mock_tab;
mod mqtt_tab;
mod proto_common;
mod request_tab;
mod response_actions;
mod response_diff;
mod runner_tab;
mod scroll_lock;
mod sidebar;
mod sio_tab;
mod snippets;
mod spec_tab;
mod state;
mod test_gen;
mod theme;
mod update;
mod vault_ui;
mod webhook_tab;
mod widgets;
mod ws_tab;

use merpati_core::settings::AppSettings;

/// Process start, for startup-time measurements.
pub static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() -> eframe::Result {
    // Script jobs run in worker processes started from this executable.
    merpati_script::maybe_run_worker();
    START.get_or_init(std::time::Instant::now);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,merpati=info".into()),
        )
        .init();
    merpati_http::tls::install_provider();
    crash::install(&AppSettings::config_dir());

    let settings_path = AppSettings::default_path();
    let settings = AppSettings::load(&settings_path);
    let open_path = std::env::args().nth(1).map(std::path::PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Merpati")
            // Linux: matches merpati.desktop, so the window gets its icon and
            // groups with the launcher.
            .with_app_id("merpati")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([760.0, 480.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).unwrap_or_default()),
        persist_window: true,
        // The app installs its own menu bar (menu.rs) instead of winit's,
        // which names the app after its executable ("merpati-app").
        #[cfg(target_os = "macos")]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::macos::EventLoopBuilderExtMacOS as _;
            builder.with_default_menu(false);
        })),
        ..Default::default()
    };
    eframe::run_native(
        "Merpati",
        options,
        Box::new(move |cc| Ok(Box::new(app::MerpatiApp::new(cc, settings, settings_path, open_path)))),
    )
}
