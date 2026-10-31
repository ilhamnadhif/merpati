//! Left sidebar: collections tree (FR-C1, FR-C5), environments and history
//! (FR-R9).

use std::collections::HashSet;

use egui::{RichText, Ui};

use merpati_core::model::{Collection, Folder, HttpRequest, Id, Item, RequestItem, RequestKind, WsRequest};
use merpati_core::storage::Workspace;

use crate::app::{ItemRef, MerpatiApp, SidebarTab, TabKind};
use crate::dialogs::Dialog;
use crate::i18n::{tr, trf};
use crate::request_tab::RequestTab;
use crate::widgets;
use crate::{icons, kit};

#[derive(Default)]
pub struct SidebarState {
    pub filter: String,
    pub expanded: HashSet<Id>,
    pub renaming: Option<(Id, Id, String)>,
    pub history_filter: String,
    pub focus_search: bool,
    /// Flows in `flows/` (id, name, path); reloaded when `flows_dirty`.
    pub flows: Option<Vec<(Id, String, std::path::PathBuf)>>,
    pub flows_dirty: bool,
    /// Scrolls the tree to this offset in the next frame (automation).
    pub scroll_to: Option<f32>,
}

impl SidebarState {
    pub fn expand_to(&mut self, collection: &str, folder: Option<&str>) {
        self.expanded.insert(collection.to_string());
        if let Some(f) = folder {
            self.expanded.insert(f.to_string());
        }
    }
}

#[derive(Clone, Debug)]
struct DragItem {
    collection: Id,
    item: Id,
}

pub enum ExportFormat {
    Postman,
    OpenApi,
}

enum Act {
    Open(ItemRef),
    OpenContainer(Id, Option<Id>),
    NewRequest(Id, Option<Id>),
    NewWs(Id, Option<Id>),
    NewProto(Id, Option<Id>, String, Box<RequestKind>),
    NewFolder(Id, Option<Id>),
    CommitRename(Id, Id, String),
    Duplicate(Id, Id),
    DuplicateCollection(Id),
    Delete(Id, Option<Id>),
    Run(Id, Option<Id>),
    Mock(Id),
    Export(Id, ExportFormat),
    Move {
        from: Id,
        item: Id,
        to: Id,
        parent: Option<Id>,
        index: usize,
    },
    CopyCurl(Id, Id),
    /// None = preview, Some(markdown) = export.
    Docs(Id, Option<bool>),
    Fork(Id),
    /// Pull parent changes (true) or merge into the parent (false).
    ForkSync(Id, bool),
}

fn matches_filter(item: &Item, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    match item {
        Item::Folder(f) => f.name.to_lowercase().contains(q) || f.items.iter().any(|i| matches_filter(i, q)),
        Item::Request(r) => {
            r.name.to_lowercase().contains(q)
                || r.request.url().to_lowercase().contains(q)
                || r.http()
                    .map(|h| h.body.raw.to_lowercase().contains(q) || h.body.graphql.query.to_lowercase().contains(q))
                    .unwrap_or(false)
                || r.scripts.pre_request.to_lowercase().contains(q)
                || r.scripts.post_response.to_lowercase().contains(q)
                || r.description.to_lowercase().contains(q)
        }
    }
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui) {
    let title = match app.sidebar_tab {
        SidebarTab::Collections => tr("Collections"),
        SidebarTab::Environments => tr("Environments"),
        SidebarTab::History => tr("History"),
        SidebarTab::Apis => tr("APIs"),
        SidebarTab::Flows => tr("Flows"),
    };
    ui.horizontal(|ui| {
        ui.set_height(28.0);
        ui.label(RichText::new(title).font(crate::theme::semibold(14.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            match app.sidebar_tab {
                SidebarTab::Collections => {
                    if kit::icon_button(ui, icons::DOWNLOAD_SIMPLE, tr("Import")).clicked() {
                        app.dialog = Some(Dialog::import());
                    }
                    if kit::icon_button(ui, icons::PLUS, tr("New Collection")).clicked() {
                        app.dialog = Some(Dialog::prompt_new_collection());
                    }
                }
                SidebarTab::Environments => {
                    if kit::icon_button(ui, icons::PLUS, tr("New Environment")).clicked() {
                        app.dialog = Some(Dialog::prompt_new_environment());
                    }
                }
                SidebarTab::History => {
                    if !app.ws.history.is_empty() && kit::icon_button(ui, icons::BROOM, tr("Clear")).clicked() {
                        app.ws.clear_history();
                    }
                }
                SidebarTab::Apis | SidebarTab::Flows => {}
            }
        });
    });
    ui.add_space(4.0);
    match app.sidebar_tab {
        SidebarTab::Collections => collections(app, ui),
        SidebarTab::Environments => crate::env_tab::sidebar_list(app, ui),
        SidebarTab::History => history(app, ui),
        SidebarTab::Apis => crate::spec_tab::sidebar_list(app, ui),
        SidebarTab::Flows => crate::flow_tab::sidebar_list(app, ui),
    }
}

/// The expand/collapse caret of a tree row.
fn caret(ui: &mut Ui, open: bool, name: &str) -> egui::Response {
    let icon = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
    kit::icon_button_ex(ui, icon, &format!("{} {name}", if open { tr("Collapse") } else { tr("Expand") }), false, 18.0)
}

fn active_ref(app: &MerpatiApp) -> Option<ItemRef> {
    app.tabs.get(app.active).and_then(|t| t.kind.location().cloned())
}

fn collections(app: &mut MerpatiApp, ui: &mut Ui) {
    let mut acts: Vec<Act> = Vec::new();
    let r = kit::search_field(ui, "tree-search", &mut app.sidebar.filter, tr("Filter requests"), ui.available_width());
    widgets::a11y_name(&r, tr("Search requests"));
    if app.sidebar.focus_search {
        r.request_focus();
        app.sidebar.focus_search = false;
    }
    ui.add_space(6.0);
    let active = active_ref(app);
    let filter = app.sidebar.filter.to_lowercase();
    let dark = ui.visuals().dark_mode;
    if app.ws.data.collections.is_empty() {
        kit::empty_state(
            ui,
            icons::STACK,
            tr("No collections yet"),
            tr("Group requests into collections to save and share them."),
            |ui| {
                if kit::primary_icon(ui, icons::PLUS, tr("New Collection")).clicked() {
                    app.dialog = Some(Dialog::prompt_new_collection());
                }
                ui.add_space(4.0);
                if kit::secondary(ui, Some(icons::DOWNLOAD_SIMPLE), tr("Import")).clicked() {
                    app.dialog = Some(Dialog::import());
                }
            },
        );
    }
    let data: &Workspace = &app.ws.data;
    let fork_parents: std::collections::HashMap<&str, String> = app
        .ws
        .forks
        .iter()
        .map(|f| (f.fork.as_str(), data.collection(&f.parent).map(|c| c.name.clone()).unwrap_or_default()))
        .collect();
    let state = &mut app.sidebar;
    // Only the visible rows are laid out, so a 10,000-request tree costs the
    // same per frame as a small one.
    let rows = flatten(data, state, &filter);
    const ROW: f32 = 28.0;
    let p = crate::theme::pal(ui);
    let mut area = egui::ScrollArea::vertical().id_salt("tree").auto_shrink([false; 2]);
    if let Some(y) = state.scroll_to.take() {
        area = area.vertical_scroll_offset(y);
    }
    kit::show_rows(area, ui, ROW, rows.len(), |ui, range| {
        for row in &rows[range] {
            let bg = ui.painter().add(egui::Shape::Noop);
            let line = ui.horizontal(|ui| {
                ui.set_height(ROW);
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.add_space(4.0 + row.depth as f32 * 14.0);
                match row.kind {
                    RowKind::Collection => {
                        let parent = fork_parents.get(row.collection.id.as_str()).map(String::as_str);
                        collection_row(ui, state, row.collection, parent, row.open, &mut acts)
                    }
                    RowKind::Folder(f) => folder_row(ui, state, row.collection, f, row.open, &mut acts),
                    RowKind::Request(r) => request_row(
                        ui,
                        state,
                        row.collection,
                        r,
                        row.parent,
                        row.index,
                        active.as_ref(),
                        dark,
                        &mut acts,
                    ),
                    RowKind::Empty => {
                        ui.add_space(18.0);
                        ui.label(widgets::muted(ui, tr("Empty collection")));
                        if kit::ghost(ui, Some(icons::PLUS), tr("Add request")).clicked() {
                            acts.push(Act::NewRequest(row.collection.id.clone(), None));
                        }
                    }
                }
            });
            // Row background (hover, open request) and indent guides.
            let full = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), line.response.rect.y_range());
            let is_active = matches!((row.kind, &active), (RowKind::Request(r), Some(a)) if a.item == r.id && a.collection == row.collection.id);
            let fill = if is_active {
                Some(p.accent_soft)
            } else if ui.rect_contains_pointer(full) {
                Some(p.hover)
            } else {
                None
            };
            if let Some(fill) = fill {
                ui.painter().set(
                    bg,
                    egui::Shape::rect_filled(full.shrink2(egui::vec2(0.0, 1.0)), egui::CornerRadius::same(6), fill),
                );
            }
            for d in 1..=row.depth {
                let x = full.left() + 4.0 + (d as f32 - 1.0) * 14.0 + 9.0;
                ui.painter().vline(x, full.y_range(), egui::Stroke::new(1.0, p.border));
            }
        }
    });
    apply(app, acts);
}

fn rename_field(ui: &mut Ui, state: &mut SidebarState, acts: &mut Vec<Act>) -> bool {
    let Some((cid, target, buf)) = &mut state.renaming else { return false };
    let r = ui.add(egui::TextEdit::singleline(buf).desired_width(ui.available_width() - 8.0));
    r.request_focus();
    let done = r.lost_focus();
    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if done || escape {
        if !escape && !buf.trim().is_empty() {
            acts.push(Act::CommitRename(cid.clone(), target.clone(), buf.trim().to_string()));
        }
        state.renaming = None;
    }
    true
}

#[derive(Clone, Copy)]
enum RowKind<'a> {
    Collection,
    Folder(&'a Folder),
    Request(&'a RequestItem),
    Empty,
}

struct Row<'a> {
    depth: usize,
    collection: &'a Collection,
    kind: RowKind<'a>,
    parent: Option<&'a str>,
    index: usize,
    open: bool,
}

/// Visible rows of the tree in display order.
fn flatten<'a>(data: &'a Workspace, state: &SidebarState, filter: &str) -> Vec<Row<'a>> {
    fn walk<'a>(
        c: &'a Collection,
        items: &'a [Item],
        parent: Option<&'a str>,
        depth: usize,
        state: &SidebarState,
        filter: &str,
        out: &mut Vec<Row<'a>>,
    ) {
        for (index, item) in items.iter().enumerate() {
            if !matches_filter(item, filter) {
                continue;
            }
            match item {
                Item::Folder(f) => {
                    let open = state.expanded.contains(&f.id) || !filter.is_empty();
                    out.push(Row { depth, collection: c, kind: RowKind::Folder(f), parent, index, open });
                    if open {
                        walk(c, &f.items, Some(&f.id), depth + 1, state, filter, out);
                    }
                }
                Item::Request(r) => {
                    out.push(Row { depth, collection: c, kind: RowKind::Request(r), parent, index, open: false });
                }
            }
        }
    }
    let mut out = Vec::new();
    for c in &data.collections {
        if !filter.is_empty()
            && !c.name.to_lowercase().contains(filter)
            && !c.items.iter().any(|i| matches_filter(i, filter))
        {
            continue;
        }
        let open = state.expanded.contains(&c.id) || !filter.is_empty();
        out.push(Row { depth: 0, collection: c, kind: RowKind::Collection, parent: None, index: 0, open });
        if open {
            if c.items.is_empty() {
                out.push(Row { depth: 1, collection: c, kind: RowKind::Empty, parent: None, index: 0, open: false });
            }
            walk(c, &c.items, None, 1, state, filter, &mut out);
        }
    }
    out
}

fn collection_row(
    ui: &mut Ui,
    state: &mut SidebarState,
    c: &Collection,
    fork_parent: Option<&str>,
    open: bool,
    acts: &mut Vec<Act>,
) {
    let a = caret(ui, open, &c.name);
    if a.clicked() {
        toggle(state, &c.id);
    }
    if matches!(&state.renaming, Some((_, t, _)) if t == &c.id) {
        rename_field(ui, state, acts);
        return;
    }
    ui.label(RichText::new(icons::STACK).font(crate::theme::icon_font(15.0)).color(crate::theme::pal(ui).text_muted));
    let r = ui.add(
        egui::Label::new(RichText::new(&c.name).font(crate::theme::medium(13.5)))
            .sense(egui::Sense::click())
            .selectable(false)
            .truncate(),
    );
    widgets::a11y_button(&r, &format!("{} {}", tr("Collection"), c.name));
    if r.clicked() {
        toggle(state, &c.id);
    }
    if r.double_clicked() {
        acts.push(Act::OpenContainer(c.id.clone(), None));
    }
    if let Some(p) = fork_parent {
        kit::pill(ui, tr("fork"), crate::theme::pal(ui).info).on_hover_text(format!("{} {p}", tr("Forked from")));
    }
    // Dropping onto the collection header moves the item to the root.
    if let Some(p) = r.dnd_release_payload::<DragItem>() {
        acts.push(Act::Move {
            from: p.collection.clone(),
            item: p.item.clone(),
            to: c.id.clone(),
            parent: None,
            index: usize::MAX,
        });
    }
    r.context_menu(|ui| {
        container_menu(ui, state, c, None, &c.name, acts);
        ui.separator();
        if ui.button(trf("{} (Postman v2.1)", &[&tr("Export")])).clicked() {
            acts.push(Act::Export(c.id.clone(), ExportFormat::Postman));
            ui.close();
        }
        if ui.button(trf("{} (OpenAPI 3)", &[&tr("Export")])).clicked() {
            acts.push(Act::Export(c.id.clone(), ExportFormat::OpenApi));
            ui.close();
        }
        if ui.button(tr("Preview documentation")).clicked() {
            acts.push(Act::Docs(c.id.clone(), None));
            ui.close();
        }
        if ui.button(tr("Export documentation (HTML)")).clicked() {
            acts.push(Act::Docs(c.id.clone(), Some(false)));
            ui.close();
        }
        if ui.button(tr("Export documentation (Markdown)")).clicked() {
            acts.push(Act::Docs(c.id.clone(), Some(true)));
            ui.close();
        }
        if ui.button(tr("Duplicate")).clicked() {
            acts.push(Act::DuplicateCollection(c.id.clone()));
            ui.close();
        }
        if ui.button(format!("{}…", tr("Fork"))).clicked() {
            acts.push(Act::Fork(c.id.clone()));
            ui.close();
        }
        if let Some(p) = fork_parent {
            if ui.button(format!("{} “{p}”…", tr("Pull changes from"))).clicked() {
                acts.push(Act::ForkSync(c.id.clone(), true));
                ui.close();
            }
            if ui.button(format!("{} “{p}”…", tr("Merge into"))).clicked() {
                acts.push(Act::ForkSync(c.id.clone(), false));
                ui.close();
            }
        }
        if ui.button(tr("Mock server")).clicked() {
            acts.push(Act::Mock(c.id.clone()));
            ui.close();
        }
    });
}

fn folder_row(ui: &mut Ui, state: &mut SidebarState, c: &Collection, f: &Folder, open: bool, acts: &mut Vec<Act>) {
    let a = caret(ui, open, &f.name);
    let mut clicked = a.clicked();
    if matches!(&state.renaming, Some((_, t, _)) if t == &f.id) {
        rename_field(ui, state, acts);
        return;
    }
    let drag_id = egui::Id::unique(("drag", &c.id, &f.id));
    let payload = DragItem { collection: c.id.clone(), item: f.id.clone() };
    let resp = ui
        .dnd_drag_source(drag_id, payload, |ui| {
            let icon = if open { icons::FOLDER_OPEN } else { icons::FOLDER_SIMPLE };
            ui.label(RichText::new(icon).font(crate::theme::icon_font(15.0)).color(crate::theme::pal(ui).text_muted));
            ui.add(egui::Label::new(&f.name).selectable(false).truncate());
        })
        .response;
    let row = resp.interact(egui::Sense::click());
    widgets::a11y_button(&row, &format!("{} {}", tr("Folder"), f.name));
    clicked |= row.clicked();
    if clicked {
        toggle(state, &f.id);
    }
    if row.double_clicked() {
        acts.push(Act::OpenContainer(c.id.clone(), Some(f.id.clone())));
    }
    drop_target(ui, &row, c, Some(&f.id), usize::MAX, acts, true);
    row.context_menu(|ui| container_menu(ui, state, c, Some(f), &f.name, acts));
}

#[allow(clippy::too_many_arguments)]
fn request_row(
    ui: &mut Ui,
    state: &mut SidebarState,
    c: &Collection,
    r: &RequestItem,
    parent: Option<&str>,
    index: usize,
    active: Option<&ItemRef>,
    dark: bool,
    acts: &mut Vec<Act>,
) {
    ui.add_space(18.0);
    if matches!(&state.renaming, Some((_, t, _)) if t == &r.id) {
        rename_field(ui, state, acts);
        return;
    }
    let is_active = active.map(|a| a.item == r.id).unwrap_or(false);
    let drag_id = egui::Id::unique(("drag", &c.id, &r.id));
    let payload = DragItem { collection: c.id.clone(), item: r.id.clone() };
    let inner = ui.dnd_drag_source(drag_id, payload, |ui| {
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(40.0, 18.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(egui::Label::new(widgets::method_badge(&r.request.badge(), dark)).selectable(false));
                },
            );
            let text = if is_active {
                RichText::new(&r.name).font(crate::theme::medium(13.5))
            } else {
                RichText::new(&r.name)
            };
            ui.add(egui::Label::new(text).selectable(false).truncate());
        });
    });
    let resp = inner.response.interact(egui::Sense::click());
    widgets::a11y_button(&resp, &format!("{} {}", r.request.badge(), r.name));
    if resp.clicked() {
        acts.push(Act::Open(ItemRef { collection: c.id.clone(), item: r.id.clone() }));
    }
    let resp = resp.on_hover_text(r.request.url());
    drop_target(ui, &resp, c, parent, index, acts, false);
    resp.context_menu(|ui| {
        if ui.button(tr("Open")).clicked() {
            acts.push(Act::Open(ItemRef { collection: c.id.clone(), item: r.id.clone() }));
            ui.close();
        }
        if ui.button(tr("Rename")).clicked() {
            state.renaming = Some((c.id.clone(), r.id.clone(), r.name.clone()));
            ui.close();
        }
        if ui.button(tr("Duplicate")).clicked() {
            acts.push(Act::Duplicate(c.id.clone(), r.id.clone()));
            ui.close();
        }
        if r.http().is_some() && ui.button(tr("Copy as cURL")).clicked() {
            acts.push(Act::CopyCurl(c.id.clone(), r.id.clone()));
            ui.close();
        }
        if ui.button(RichText::new(tr("Delete")).color(crate::theme::err(dark))).clicked() {
            acts.push(Act::Delete(c.id.clone(), Some(r.id.clone())));
            ui.close();
        }
    });
}

fn container_menu(
    ui: &mut Ui,
    state: &mut SidebarState,
    c: &Collection,
    folder: Option<&Folder>,
    name: &str,
    acts: &mut Vec<Act>,
) {
    let fid = folder.map(|f| f.id.clone());
    if ui.button(tr("Add request")).clicked() {
        acts.push(Act::NewRequest(c.id.clone(), fid.clone()));
        ui.close();
    }
    if ui.button(tr("Add WebSocket request")).clicked() {
        acts.push(Act::NewWs(c.id.clone(), fid.clone()));
        ui.close();
    }
    for (label, name, kind) in crate::app::protocol_templates() {
        if ui.button(label.replace("New", "Add")).clicked() {
            acts.push(Act::NewProto(c.id.clone(), fid.clone(), name.to_string(), Box::new(kind)));
            ui.close();
        }
    }
    if ui.button(tr("Add folder")).clicked() {
        acts.push(Act::NewFolder(c.id.clone(), fid.clone()));
        ui.close();
    }
    ui.separator();
    if ui
        .button(kit::with_icon(icons::PLAY, if folder.is_some() { tr("Run folder") } else { tr("Run collection") }))
        .clicked()
    {
        acts.push(Act::Run(c.id.clone(), fid.clone()));
        ui.close();
    }
    if ui.button(kit::with_icon(icons::GEAR_SIX, tr("Settings"))).clicked() {
        acts.push(Act::OpenContainer(c.id.clone(), fid.clone()));
        ui.close();
    }
    if ui.button(tr("Rename")).clicked() {
        state.renaming = Some((c.id.clone(), fid.clone().unwrap_or_else(|| c.id.clone()), name.to_string()));
        ui.close();
    }
    if let Some(f) = folder
        && ui.button(tr("Duplicate")).clicked()
    {
        acts.push(Act::Duplicate(c.id.clone(), f.id.clone()));
        ui.close();
    }
    if ui.button(RichText::new(tr("Delete")).color(crate::theme::err(ui.visuals().dark_mode))).clicked() {
        acts.push(Act::Delete(c.id.clone(), fid));
        ui.close();
    }
}

fn toggle(state: &mut SidebarState, id: &str) {
    if !state.expanded.remove(id) {
        state.expanded.insert(id.to_string());
    }
}

/// Handles drops on a row: onto a folder moves into it, onto a request moves
/// before it.
fn drop_target(
    ui: &Ui,
    resp: &egui::Response,
    c: &Collection,
    parent: Option<&str>,
    index: usize,
    acts: &mut Vec<Act>,
    into_folder: bool,
) {
    if let Some(hover) = resp.dnd_hover_payload::<DragItem>()
        && hover.item != parent.unwrap_or("")
    {
        let stroke = egui::Stroke::new(2.0, crate::theme::accent(ui.visuals().dark_mode));
        if into_folder {
            ui.painter().rect_stroke(resp.rect, 3.0, stroke, egui::StrokeKind::Inside);
        } else {
            ui.painter().hline(resp.rect.x_range(), resp.rect.top(), stroke);
        }
    }
    if let Some(p) = resp.dnd_release_payload::<DragItem>() {
        acts.push(Act::Move {
            from: p.collection.clone(),
            item: p.item.clone(),
            to: c.id.clone(),
            parent: parent.map(str::to_string),
            index,
        });
    }
}

fn apply(app: &mut MerpatiApp, acts: Vec<Act>) {
    for act in acts {
        match act {
            Act::Open(r) => app.open_item(r),
            Act::OpenContainer(c, f) => app.open_container(c, f),
            Act::NewRequest(cid, parent) => {
                let item = RequestItem::new("New Request", HttpRequest::default());
                add_item(app, &cid, parent.as_deref(), Item::Request(item), true);
            }
            Act::NewWs(cid, parent) => {
                let mut item = RequestItem::new("New WebSocket", HttpRequest::default());
                item.request = RequestKind::WebSocket(WsRequest::default());
                add_item(app, &cid, parent.as_deref(), Item::Request(item), true);
            }
            Act::NewProto(cid, parent, name, kind) => {
                let mut item = RequestItem::new(name, HttpRequest::default());
                item.request = *kind;
                add_item(app, &cid, parent.as_deref(), Item::Request(item), true);
            }
            Act::NewFolder(cid, parent) => {
                let f = Folder::new("New Folder");
                let fid = f.id.clone();
                add_item(app, &cid, parent.as_deref(), Item::Folder(f), false);
                app.sidebar.renaming = Some((cid, fid, "New Folder".into()));
            }
            Act::CommitRename(cid, target, name) => {
                if let Some(c) = app.ws.data.collection_mut(&cid) {
                    if target == cid {
                        c.name = name;
                    } else if let Some(item) = c.find_mut(&target) {
                        item.set_name(name);
                    }
                    app.ws.mark_collection(&cid);
                }
            }
            Act::Duplicate(cid, item_id) => {
                if let Some(c) = app.ws.data.collection_mut(&cid)
                    && let Some(item) = c.find(&item_id).cloned()
                {
                    let parent = c.parent_of(&item_id).map(str::to_string);
                    let mut copy = item.duplicate();
                    copy.set_name(trf("{} Copy", &[&item.name()]));
                    let idx = c
                        .children_mut(parent.as_deref())
                        .and_then(|v| v.iter().position(|i| i.id() == item_id))
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    c.insert(parent.as_deref(), idx, copy);
                    app.ws.mark_collection(&cid);
                }
            }
            Act::DuplicateCollection(cid) => {
                if let Some(c) = app.ws.data.collection(&cid).cloned() {
                    let mut copy = c.clone();
                    copy.id = merpati_core::util::new_id();
                    copy.name = trf("{} Copy", &[&c.name]);
                    copy.items = copy.items.iter().map(|i| i.duplicate()).collect();
                    app.ws.add_collection(copy);
                }
            }
            Act::Delete(cid, item) => {
                let name = match &item {
                    Some(i) => app.ws.data.collection(&cid).and_then(|c| c.find(i)).map(|i| i.name().to_string()),
                    None => app.ws.data.collection(&cid).map(|c| c.name.clone()),
                };
                if let Some(name) = name {
                    app.dialog = Some(Dialog::confirm_delete(cid, item, name));
                }
            }
            Act::Run(cid, folder) => app.open_runner(cid, folder),
            Act::Mock(cid) => app.open_mock(cid),
            Act::Fork(cid) => {
                let name = app.ws.data.collection(&cid).map(|c| format!("{} (fork)", c.name)).unwrap_or_default();
                app.dialog = Some(Dialog::Prompt { kind: crate::dialogs::Prompt::Fork(cid), text: name });
            }
            Act::ForkSync(cid, pull) => {
                if let Some(s) = crate::fork_ui::ForkSync::new(app, &cid, pull) {
                    app.dialog = Some(Dialog::ForkSync(Box::new(s)));
                }
            }
            Act::Export(cid, format) => crate::dialogs::export_collection(app, &cid, format),
            Act::Move { from, item, to, parent, index } => move_item(app, &from, &item, &to, parent.as_deref(), index),
            Act::Docs(cid, mode) => match mode {
                None => {
                    if let Some(c) = app.ws.data.collection(&cid).cloned() {
                        crate::dialogs::preview_docs(app, &c);
                    }
                }
                Some(md) => crate::dialogs::export_docs(app, &cid, md),
            },
            Act::CopyCurl(cid, rid) => {
                if let Some(code) =
                    crate::dialogs::codegen_for(app, &cid, &rid, merpati_convert::codegen::Language::Curl)
                {
                    app.ctx.copy_text(code);
                    app.toast(tr("Copied"), false);
                }
            }
        }
    }
}

fn add_item(app: &mut MerpatiApp, cid: &str, parent: Option<&str>, item: Item, open: bool) {
    let item_id = item.id().to_string();
    let name = item.name().to_string();
    if let Some(c) = app.ws.data.collection_mut(cid) {
        let at = c.children_mut(parent).map(|v| v.len()).unwrap_or(0);
        c.insert(parent, at, item);
        app.ws.mark_collection(cid);
        app.sidebar.expand_to(cid, parent);
        if open {
            app.open_item(ItemRef { collection: cid.to_string(), item: item_id.clone() });
            app.sidebar.renaming = Some((cid.to_string(), item_id, name));
        }
    }
}

fn move_item(app: &mut MerpatiApp, from: &str, item: &str, to: &str, parent: Option<&str>, index: usize) {
    if from == to {
        if let Some(c) = app.ws.data.collection_mut(from) {
            let idx = if index == usize::MAX { c.children_mut(parent).map(|v| v.len()).unwrap_or(0) } else { index };
            if c.move_item(item, parent, idx) {
                app.ws.mark_collection(from);
            }
        }
        return;
    }
    let Some(moved) = app.ws.data.collection_mut(from).and_then(|c| c.remove(item)) else { return };
    let ok = app
        .ws
        .data
        .collection_mut(to)
        .map(|c| {
            let idx = if index == usize::MAX { c.children_mut(parent).map(|v| v.len()).unwrap_or(0) } else { index };
            c.insert(parent, idx, moved.clone())
        })
        .unwrap_or(false);
    if ok {
        app.ws.mark_collection(from);
        app.ws.mark_collection(to);
        // Keep open tabs pointing at the right collection.
        for tab in &mut app.tabs {
            let loc = tab.kind.location_mut().and_then(|l| l.as_mut());
            if let Some(l) = loc
                && l.collection == from
                && (l.item == item || app.ws.data.collection(to).and_then(|c| c.find(&l.item)).is_some())
            {
                l.collection = to.to_string();
            }
        }
    } else if let Some(c) = app.ws.data.collection_mut(from) {
        c.items.push(moved);
    }
}

fn history(app: &mut MerpatiApp, ui: &mut Ui) {
    let dark = ui.visuals().dark_mode;
    kit::search_field(ui, "history-search", &mut app.sidebar.history_filter, tr("Search"), ui.available_width());
    ui.add_space(4.0);
    let q = app.sidebar.history_filter.clone();
    let mut open = None;
    let mut remove = None;
    let today = chrono::Local::now().date_naive();
    egui::ScrollArea::vertical().id_salt("history").auto_shrink([false; 2]).show(ui, |ui| {
        let mut last_day = None;
        for h in app.ws.history.iter().filter(|h| h.matches(&q)) {
            let dt = chrono::DateTime::from_timestamp_millis(h.timestamp).map(|d| d.with_timezone(&chrono::Local));
            let day = dt.map(|d| d.date_naive());
            if day != last_day {
                let label = match day {
                    Some(d) if d == today => tr("Today").to_string(),
                    Some(d) if Some(d) == today.pred_opt() => tr("Yesterday").to_string(),
                    Some(d) => d.format("%d %b %Y").to_string(),
                    None => String::new(),
                };
                ui.add_space(6.0);
                ui.label(
                    RichText::new(label.to_uppercase())
                        .font(crate::theme::semibold(10.5))
                        .color(crate::theme::pal(ui).text_faint),
                );
                last_day = day;
            }
            let url = if h.resolved_url.is_empty() { &h.request.url } else { &h.resolved_url };
            let r = history_row(ui, h, url, dark);
            let r = r.on_hover_ui(|ui| {
                ui.label(RichText::new(url).monospace());
                if let Some(n) = &h.name {
                    ui.label(n);
                }
                ui.label(format!(
                    "{} · {} · {}",
                    h.status.map(|s| s.to_string()).or(h.error.clone()).unwrap_or_default(),
                    merpati_core::util::human_ms(h.elapsed_ms),
                    dt.map(|d| d.format("%H:%M:%S").to_string()).unwrap_or_default()
                ));
            });
            if r.clicked() {
                open = Some(h.clone());
            }
            r.context_menu(|ui| {
                if ui.button(tr("Delete")).clicked() {
                    remove = Some(h.id.clone());
                    ui.close();
                }
            });
        }
        if app.ws.history.is_empty() {
            ui.label(widgets::muted(ui, tr("Requests you send appear here.")));
        }
    });
    if let Some(h) = open {
        let name = h.name.clone().unwrap_or_else(|| h.request.url.clone());
        let item = RequestItem::new(name, h.request.clone());
        app.add_tab(TabKind::Request(Box::new(RequestTab::scratch(item))));
    }
    if let Some(id) = remove {
        app.ws.remove_history(&id);
    }
}

/// One History row: method, the URL without its scheme (cut with "…" to the
/// row), and the status code on the right; painted so a long URL never
/// widens the sidebar.
fn history_row(ui: &mut Ui, h: &merpati_core::history::HistoryEntry, url: &str, dark: bool) -> egui::Response {
    const ROW: f32 = 28.0;
    const METHOD_W: f32 = 46.0;
    let p = crate::theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW), egui::Sense::click());
    let label = format!("{} {url}", h.request.method);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let painter = ui.painter_at(rect);
    if resp.hovered() {
        painter.rect_filled(rect.shrink2(egui::vec2(0.0, 1.0)), egui::CornerRadius::same(6), p.hover);
    }
    let y = rect.center().y;
    let method = widgets::method_badge(&h.request.method, dark);
    let method_font = crate::theme::semibold(10.5);
    painter.text(
        egui::pos2(rect.left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        method.text(),
        method_font,
        crate::theme::method_color(&h.request.method, dark),
    );
    let small =
        ui.style().text_styles.get(&egui::TextStyle::Small).cloned().unwrap_or(egui::FontId::proportional(11.5));
    let mut right = rect.right() - 6.0;
    let status = match (h.status, &h.error) {
        (Some(code), _) => Some((code.to_string(), crate::theme::status_color(code, dark))),
        (None, Some(_)) => Some((tr("Error").to_string(), crate::theme::err(dark))),
        _ => None,
    };
    if let Some((text, color)) = status {
        let g = painter.layout_no_wrap(text, small.clone(), color);
        right -= g.size().x;
        painter.galley(egui::pos2(right, y - g.size().y / 2.0), g, color);
        right -= 10.0;
    }
    let shown = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let left = rect.left() + 6.0 + METHOD_W;
    let mut job = egui::text::LayoutJob::simple_singleline(shown.to_string(), small, p.text);
    job.wrap = egui::text::TextWrapping::truncate_at_width((right - left).max(10.0));
    let g = painter.layout_job(job);
    painter.galley(egui::pos2(left, y - g.size().y / 2.0), g, p.text);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inline rename that follows "Add …" starts from the new item's own
    /// name, so leaving the field unchanged keeps it.
    #[test]
    fn new_items_keep_their_name_after_the_inline_rename() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        let cid = app.new_collection("C");
        let (_, name, kind) = crate::app::protocol_templates().remove(0);
        apply(
            &mut app,
            vec![Act::NewWs(cid.clone(), None), Act::NewProto(cid.clone(), None, name.to_string(), Box::new(kind))],
        );
        let c = app.ws.data.collection(&cid).unwrap();
        let names: Vec<String> = c.items.iter().map(|i| i.name().to_string()).collect();
        assert_eq!(names, ["New WebSocket", name]);
        let (_, target, buf) = app.sidebar.renaming.clone().unwrap();
        assert_eq!(target, c.items[1].id());
        assert_eq!(buf, name);
    }
}
