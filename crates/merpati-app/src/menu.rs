//! Native macOS menu bar (muda). Menu items with key equivalents receive
//! those keys before egui, so an item hands egui the same input its shortcut
//! would have produced (a key press, or Copy/Cut/Paste): shortcuts keep one
//! handler on every platform (`MerpatiApp::shortcuts` and text editing).

use std::collections::HashMap;
use std::sync::mpsc;

use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use crate::app::NewItem;
use crate::dialogs::SettingsPage;
use crate::i18n::tr;

/// What a menu item does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    /// The key press of a shortcut the app already handles.
    Key(egui::Key, egui::Modifiers),
    Copy,
    Cut,
    Paste,
    New(NewItem),
    Import,
    OpenWorkspace,
    NewWorkspace,
    CheckForUpdates,
    Settings(SettingsPage),
    Url(&'static str),
    Quit,
}

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const RELEASES: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases");
const ISSUES: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/issues/new");

/// ⌘ as egui-winit reports it on macOS.
const CMD: egui::Modifiers = egui::Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };

fn with(base: egui::Modifiers, shift: bool, alt: bool) -> egui::Modifiers {
    egui::Modifiers { shift, alt, ..base }
}

pub struct AppMenu {
    _menu: Menu,
    commands: HashMap<MenuId, Command>,
    events: mpsc::Receiver<MenuEvent>,
}

impl AppMenu {
    /// Builds the menu bar, makes it the application's main menu and routes
    /// clicks to the app. Call once, on the main thread, after launch.
    pub fn install(ctx: &egui::Context) -> AppMenu {
        // The handler can be set once per process; clicks wake egui.
        let (tx, events) = mpsc::channel();
        let ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = tx.send(event);
            ctx.request_repaint();
        }));
        let (menu, commands) = build();
        AppMenu { _menu: menu, commands, events }
    }

    /// Builds the menu again in the current language.
    pub fn rebuild(&mut self) {
        let (menu, commands) = build();
        self._menu = menu;
        self.commands = commands;
    }

    /// Commands of the menu items chosen since the last call.
    pub fn take_commands(&self) -> Vec<Command> {
        self.events.try_iter().filter_map(|e| self.commands.get(e.id()).copied()).collect()
    }
}

/// The menu bar, installed as the application's main menu, and what each
/// item does.
fn build() -> (Menu, HashMap<MenuId, Command>) {
    let mut commands = HashMap::new();
    let mut item = |text: &str, accel: Option<Accelerator>, command: Command| -> MenuItem {
        let it = MenuItem::new(text, true, accel);
        commands.insert(it.id().clone(), command);
        it
    };
    let cmd = |code: Code| Some(Accelerator::new(Modifiers::META, code));
    let shift_cmd = |code: Code| Some(Accelerator::new(Modifiers::META | Modifiers::SHIFT, code));
    let alt_cmd = |code: Code| Some(Accelerator::new(Modifiers::META | Modifiers::ALT, code));
    let sep = PredefinedMenuItem::separator;

    let about = PredefinedMenuItem::about(
        Some(tr("About Merpati")),
        Some(AboutMetadata {
            name: Some("Merpati".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            comments: Some(tr("A lightweight API client written in Rust").into()),
            copyright: Some("© 2026 Tommy Lumos Empire and the Merpati contributors".into()),
            website: Some(REPOSITORY.into()),
            ..Default::default()
        }),
    );
    let updates = item(tr("Check for Updates…"), None, Command::CheckForUpdates);
    let settings = item(tr("Settings…"), cmd(Code::Comma), Command::Key(egui::Key::Comma, CMD));
    let quit = item(tr("Quit Merpati"), cmd(Code::KeyQ), Command::Quit);
    let app_menu = Submenu::with_items(
        "Merpati",
        true,
        &[
            &about,
            &updates,
            &sep(),
            &settings,
            &sep(),
            &PredefinedMenuItem::services(Some(tr("Services"))),
            &sep(),
            &PredefinedMenuItem::hide(Some(tr("Hide Merpati"))),
            &PredefinedMenuItem::hide_others(Some(tr("Hide Others"))),
            &PredefinedMenuItem::show_all(Some(tr("Show All"))),
            &sep(),
            &quit,
        ],
    )
    .expect("app menu");

    let new_request = item(tr("New Request"), cmd(Code::KeyN), Command::Key(egui::Key::N, CMD));
    let new_items: Vec<MenuItem> = NewItem::all()
        .into_iter()
        .filter(|(what, ..)| *what != NewItem::Request)
        .map(|(what, _, label)| item(tr(label), None, Command::New(what)))
        .collect();
    let mut new_refs: Vec<&dyn muda::IsMenuItem> = Vec::new();
    for it in &new_items {
        new_refs.push(it);
    }
    let new_menu = Submenu::with_items(tr("New"), true, &new_refs).expect("new menu");
    let import = item(tr("Import…"), cmd(Code::KeyO), Command::Import);
    let open_ws = item(tr("Open Workspace…"), None, Command::OpenWorkspace);
    let new_ws = item(tr("New Workspace…"), None, Command::NewWorkspace);
    let save = item(tr("Save"), cmd(Code::KeyS), Command::Key(egui::Key::S, CMD));
    let close = item(tr("Close Tab"), cmd(Code::KeyW), Command::Key(egui::Key::W, CMD));
    let send = item(tr("Send Request"), cmd(Code::Enter), Command::Key(egui::Key::Enter, CMD));
    let file_menu = Submenu::with_items(
        tr("File"),
        true,
        &[&new_request, &new_menu, &sep(), &import, &open_ws, &new_ws, &sep(), &save, &close, &sep(), &send],
    )
    .expect("file menu");

    let undo = item(tr("Undo"), cmd(Code::KeyZ), Command::Key(egui::Key::Z, CMD));
    let redo = item(tr("Redo"), shift_cmd(Code::KeyZ), Command::Key(egui::Key::Z, with(CMD, true, false)));
    let cut = item(tr("Cut"), cmd(Code::KeyX), Command::Cut);
    let copy = item(tr("Copy"), cmd(Code::KeyC), Command::Copy);
    let paste = item(tr("Paste"), cmd(Code::KeyV), Command::Paste);
    let select_all = item(tr("Select All"), cmd(Code::KeyA), Command::Key(egui::Key::A, CMD));
    let edit_menu = Submenu::with_items(tr("Edit"), true, &[&undo, &redo, &sep(), &cut, &copy, &paste, &select_all])
        .expect("edit menu");

    let palette =
        item(tr("Command Palette"), shift_cmd(Code::KeyP), Command::Key(egui::Key::P, with(CMD, true, false)));
    let search = item(tr("Search Requests"), cmd(Code::KeyK), Command::Key(egui::Key::K, CMD));
    let env = item(tr("Switch Environment"), cmd(Code::KeyE), Command::Key(egui::Key::E, CMD));
    let sidebar = item(tr("Show/Hide Sidebar"), cmd(Code::KeyB), Command::Key(egui::Key::B, CMD));
    let console =
        item(tr("Show/Hide Console"), alt_cmd(Code::KeyC), Command::Key(egui::Key::C, with(CMD, false, true)));
    let view_menu = Submenu::with_items(
        tr("View"),
        true,
        &[
            &palette,
            &search,
            &env,
            &sep(),
            &sidebar,
            &console,
            &sep(),
            &PredefinedMenuItem::fullscreen(Some(tr("Enter Full Screen"))),
        ],
    )
    .expect("view menu");

    let prev_tab = item(
        tr("Show Previous Tab"),
        shift_cmd(Code::BracketLeft),
        Command::Key(egui::Key::OpenBracket, with(CMD, true, false)),
    );
    let next_tab = item(
        tr("Show Next Tab"),
        shift_cmd(Code::BracketRight),
        Command::Key(egui::Key::CloseBracket, with(CMD, true, false)),
    );
    let window_menu = Submenu::with_items(
        tr("Window"),
        true,
        &[
            &PredefinedMenuItem::minimize(Some(tr("Minimize"))),
            &PredefinedMenuItem::maximize(Some(tr("Zoom"))),
            &sep(),
            &prev_tab,
            &next_tab,
            &sep(),
            &PredefinedMenuItem::bring_all_to_front(Some(tr("Bring All to Front"))),
        ],
    )
    .expect("window menu");

    let shortcuts = item(tr("Keyboard Shortcuts"), None, Command::Settings(SettingsPage::Shortcuts));
    let github = item(tr("Merpati on GitHub"), None, Command::Url(REPOSITORY));
    let notes = item(tr("Release Notes"), None, Command::Url(RELEASES));
    let issue = item(tr("Report an Issue…"), None, Command::Url(ISSUES));
    let help_menu =
        Submenu::with_items(tr("Help"), true, &[&shortcuts, &sep(), &github, &notes, &issue]).expect("help menu");

    let menu =
        Menu::with_items(&[&app_menu, &file_menu, &edit_menu, &view_menu, &window_menu, &help_menu]).expect("menu bar");
    menu.init_for_nsapp();
    window_menu.set_as_windows_menu_for_nsapp();
    help_menu.set_as_help_menu_for_nsapp();
    (menu, commands)
}

/// The egui input a command stands for, when it is plain input.
pub fn as_input(command: Command) -> Option<egui::Event> {
    Some(match command {
        Command::Key(key, modifiers) => {
            egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
        }
        Command::Copy => egui::Event::Copy,
        Command::Cut => egui::Event::Cut,
        Command::Paste => {
            let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
            egui::Event::Paste(text)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_items_become_key_presses() {
        let ev = as_input(Command::Key(egui::Key::S, CMD)).unwrap();
        assert!(
            matches!(ev, egui::Event::Key { key: egui::Key::S, pressed: true, modifiers, .. } if modifiers.command)
        );
        assert!(as_input(Command::Copy).is_some_and(|e| e == egui::Event::Copy));
        assert!(as_input(Command::CheckForUpdates).is_none());
    }
}
