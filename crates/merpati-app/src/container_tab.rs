//! Collection and folder settings (FR-C2): overview, auth, scripts and
//! variables that requests inside inherit.

use egui::{RichText, Ui};

use merpati_core::auth::AuthType;
use merpati_core::model::{Id, Item};
use merpati_core::script::ScriptEvent;
use merpati_core::tokens::TokenStore;

use crate::app::MerpatiApp;
use crate::i18n::{tr, trf};
use crate::request_tab::{form_row, settings_group, stat_cards, view_header};
use crate::{icons, kit, theme, widgets};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Overview,
    Auth,
    Scripts,
    Variables,
}

pub struct ContainerTab {
    pub collection: Id,
    pub folder: Option<Id>,
    section: Section,
    event: ScriptEvent,
    filter: String,
}

impl ContainerTab {
    pub fn new(collection: Id, folder: Option<Id>) -> Self {
        ContainerTab {
            collection,
            folder,
            section: Section::Overview,
            event: ScriptEvent::PreRequest,
            filter: String::new(),
        }
    }
}

fn count_folders(items: &[Item]) -> usize {
    items
        .iter()
        .map(|i| match i {
            Item::Folder(f) => 1 + count_folders(&f.items),
            Item::Request(_) => 0,
        })
        .sum()
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab: &mut ContainerTab) {
    let p = theme::pal(ui);
    let vars = app.var_ctx(Some(&tab.collection));
    let owner_id = tab.folder.clone().unwrap_or_else(|| tab.collection.clone());
    let token = TokenStore::get(app.ws.secrets.as_ref(), &owner_id);
    let fetching = app.pending_tokens.contains(&owner_id);
    let Some(c) = app.ws.data.collection(&tab.collection) else {
        kit::empty_state(
            ui,
            icons::STACK,
            tr("This collection was deleted."),
            tr("Close this tab or pick another collection."),
            |_| {},
        );
        return;
    };
    let collection_name = c.name.clone();
    let inherited = tab.folder.as_ref().and_then(|f| {
        let chain = c.ancestors(f)?;
        merpati_core::model::effective_from_chain(&chain).map(|e| (e.owner_name.to_string(), e.auth.kind))
    });
    let (request_count, folder_count) = match &tab.folder {
        None => (c.request_count(), count_folders(&c.items)),
        Some(f) => match c.find(f) {
            Some(Item::Folder(folder)) => {
                let tmp = merpati_core::model::Collection {
                    items: folder.items.clone(),
                    ..merpati_core::model::Collection::new("")
                };
                (tmp.request_count(), count_folders(&folder.items))
            }
            _ => (0, 0),
        },
    };
    let mut get_token = false;
    let mut clear_token = false;
    let mut run = false;
    let mut mock = false;
    let cid = tab.collection.clone();
    let Some(c) = app.ws.data.collection_mut(&cid) else { return };
    let mut changed = false;
    // Borrow the common fields of either a collection or a folder.
    let (name, description, auth, scripts, variables) = match &tab.folder {
        None => (&mut c.name, &mut c.description, &mut c.auth, &mut c.scripts, &mut c.variables),
        Some(fid) => match c.find_mut(fid) {
            Some(Item::Folder(f)) => (&mut f.name, &mut f.description, &mut f.auth, &mut f.scripts, &mut f.variables),
            _ => {
                kit::empty_state(
                    ui,
                    icons::FOLDER_SIMPLE,
                    tr("This folder was deleted."),
                    tr("Close this tab or pick another folder."),
                    |_| {},
                );
                return;
            }
        },
    };
    let is_folder = tab.folder.is_some();
    let (icon, subtitle) = if is_folder {
        (icons::FOLDER_SIMPLE, trf("Folder in {} · {} requests", &[&collection_name, &request_count]))
    } else {
        (icons::STACK, trf("Collection · {} requests", &[&request_count]))
    };
    view_header(ui, icon, p.accent, name, &subtitle, |ui| {
        if kit::primary_icon(ui, icons::PLAY, tr("Run")).on_hover_text(tr("Open the Collection Runner")).clicked() {
            run = true;
        }
        if !is_folder && kit::secondary(ui, Some(icons::CUBE), tr("Mock server")).clicked() {
            mock = true;
        }
    });
    ui.add_space(10.0);
    let script_count = [&scripts.pre_request, &scripts.post_response].iter().filter(|s| !s.trim().is_empty()).count();
    let items = vec![
        (Section::Overview, tr("Overview").to_string()),
        (Section::Auth, tr("Authorization").to_string()),
        (Section::Variables, format!("{}{}", tr("Variables"), widgets::count_label(variables.len()))),
        (Section::Scripts, format!("{}{}", tr("Scripts"), widgets::count_label(script_count))),
    ];
    let auth_set = !matches!(auth.kind, AuthType::Inherit | AuthType::None);
    crate::kit::tab_strip_ex(ui, &mut tab.section, &items, if auth_set { &[Section::Auth] } else { &[] });
    ui.add_space(12.0);
    egui::ScrollArea::vertical().auto_shrink([false; 2]).id_salt("container").show(ui, |ui| match tab.section {
        Section::Overview => {
            let width = ui.available_width().min(900.0);
            ui.allocate_ui_with_layout(egui::vec2(width, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(width);
                // Numbers at a glance.
                let auth_label = if auth.kind == AuthType::Inherit { tr("Inherited") } else { tr(auth.kind.label()) };
                stat_cards(
                    ui,
                    150.0,
                    &[
                        (tr("Requests"), request_count.to_string(), None, String::new()),
                        (tr("Folders"), folder_count.to_string(), None, String::new()),
                        (tr("Variables"), variables.len().to_string(), None, String::new()),
                        (tr("Authorization"), auth_label.to_string(), None, String::new()),
                    ],
                );
                ui.add_space(16.0);
                settings_group(ui, tr("General"), |ui| {
                    form_row(
                        ui,
                        tr("Name"),
                        if is_folder {
                            tr("Shown in the sidebar and in run reports.")
                        } else {
                            tr("Also the name of the collection file.")
                        },
                        |ui| {
                            changed |= ui
                                .add(
                                    egui::TextEdit::singleline(name)
                                        .desired_width(ui.available_width().min(420.0))
                                        .margin(egui::Margin::symmetric(8, 5)),
                                )
                                .changed();
                        },
                    );
                });
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.set_height(22.0);
                    ui.label(kit::section_title(tr("Description")));
                });
                ui.add_space(4.0);
                changed |= widgets::code_editor(ui, "desc", description, crate::highlight::Lang::Text, None, 10, false)
                    .changed();
                ui.label(kit::hint(ui, tr("Shown in the generated collection documentation.")));
            });
        }
        Section::Auth => {
            ui.label(kit::hint(ui, tr("Requests that use “Inherit auth from parent” get this auth.")));
            ui.add_space(10.0);
            let (ch, action) = crate::auth_ui::auth_editor(
                ui,
                auth,
                crate::auth_ui::AuthUiCtx {
                    is_root: tab.folder.is_none(),
                    inherited: inherited.clone(),
                    owner_id: &owner_id,
                    token: token.clone(),
                    fetching,
                    vars: &vars,
                },
            );
            changed |= ch;
            match action {
                crate::auth_ui::AuthAction::GetToken => get_token = true,
                crate::auth_ui::AuthAction::ClearToken => clear_token = true,
                crate::auth_ui::AuthAction::None => {}
            }
        }
        Section::Scripts => {
            crate::request_tab::script_event_bar(
                ui,
                &mut tab.event,
                scripts,
                tr("Runs for every request inside, before the request's own script."),
            );
            let before = scripts.clone();
            crate::request_tab::script_panel(ui, tab.event, scripts, &vars);
            changed |= *scripts != before;
        }
        Section::Variables => {
            ui.horizontal(|ui| {
                ui.set_height(30.0);
                let w = ui.available_width().min(280.0);
                kit::search_field(ui, "container-filter", &mut tab.filter, tr("Filter variables"), w);
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(kit::hint(
                        ui,
                        if is_folder {
                            tr("Folder variables override collection variables for requests in this folder.")
                        } else {
                            tr("Collection variables are available to every request in the collection.")
                        },
                    ))
                    .truncate(),
                );
            });
            ui.add_space(10.0);
            changed |= crate::env_tab::variables_table(ui, "container-vars", variables, &tab.filter);
            if variables.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(tr("Type a name in the last row to add a variable.")).small().color(p.text_muted),
                );
            }
        }
    });
    let resolved_auth = if get_token { Some(auth.clone()) } else { None };
    if changed {
        app.ws.mark_collection(&cid);
    }
    if let Some(a) = resolved_auth {
        let scopes = app.ws.scopes_for(Some(&cid));
        app.fetch_token(a, scopes, owner_id.clone());
    }
    if clear_token {
        TokenStore::set(app.ws.secrets.as_ref(), &owner_id, None);
    }
    if run {
        app.open_runner(cid.clone(), tab.folder.clone());
    }
    if mock {
        app.open_mock(cid);
    }
}
