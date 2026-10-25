//! Environments and globals (FR-V1..V4): selector, sidebar list and the
//! variable editor (also used for collection and folder variables).

use egui::{Align2, CornerRadius, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use merpati_core::model::Id;
use merpati_core::variables::{VarKind, Variable};

use crate::app::MerpatiApp;
use crate::dialogs::Dialog;
use crate::i18n::{tr, trf};
use crate::request_tab::{elide, mono_size};
use crate::{icons, kit, theme, widgets};

#[derive(Default)]
pub struct EnvTab {
    /// `None` = globals.
    pub env: Option<Id>,
    pub search: String,
}

pub fn environment_selector(app: &mut MerpatiApp, ui: &mut Ui) {
    use crate::{icons, kit};
    let p = crate::theme::pal(ui);
    let active = app.ws.data.active_environment().map(|e| e.name.clone());
    if kit::icon_button(ui, icons::EYE, tr("Environment quick look")).clicked() {
        app.dialog = Some(Dialog::quick_look());
    }
    let label =
        format!("{}  {}", active.clone().unwrap_or_else(|| tr("No environment").to_string()), icons::CARET_DOWN);
    let r = kit::button_ex(ui, kit::Kind::Secondary, Some(icons::GLOBE), &label, false).on_hover_text("⌘E");
    crate::widgets::a11y_name(
        &r,
        &format!("{}: {}", tr("Environment"), active.as_deref().unwrap_or(tr("No environment"))),
    );
    if active.is_some() {
        // A dot on the globe marks an active environment.
        ui.painter().circle_filled(r.rect.left_top() + egui::vec2(20.0, 9.0), 3.5, p.ok);
    }
    let mut choice: Option<Option<Id>> = None;
    let mut manage = false;
    egui::Popup::menu(&r).width(240.0).show(|ui| {
        let check = |on: bool| if on { icons::CHECK } else { "    " };
        if ui.button(format!("{}  {}", check(app.ws.data.active_environment.is_none()), tr("No environment"))).clicked()
        {
            choice = Some(None);
        }
        for e in &app.ws.data.environments {
            let on = app.ws.data.active_environment.as_deref() == Some(&e.id);
            if ui.button(format!("{}  {}", check(on), e.name)).clicked() {
                choice = Some(Some(e.id.clone()));
            }
        }
        ui.separator();
        if ui.button(kit::with_icon(icons::SLIDERS_HORIZONTAL, tr("Manage environments"))).clicked() {
            manage = true;
        }
    });
    if let Some(c) = choice {
        app.ws.set_active_environment(c);
    }
    if manage {
        app.sidebar_tab = crate::app::SidebarTab::Environments;
        app.settings.ui.sidebar_visible = true;
    }
}

/// Something to do with an environment, from its menu.
enum EnvAct {
    Open(Option<Id>),
    Activate(Option<Id>),
    Rename(Id, String),
    Duplicate(Id),
    Export(Id),
    Delete(Id),
}

fn apply(app: &mut MerpatiApp, act: EnvAct) {
    match act {
        EnvAct::Open(id) => app.open_environment(id),
        EnvAct::Activate(id) => app.ws.set_active_environment(id),
        EnvAct::Rename(id, name) => app.dialog = Some(Dialog::rename_env(id, name)),
        EnvAct::Duplicate(id) => {
            if let Some(e) = app.ws.data.environment(&id).cloned() {
                let mut copy = e.clone();
                copy.id = merpati_core::util::new_id();
                copy.name = trf("{} Copy", &[&e.name]);
                app.ws.add_environment(copy);
            }
        }
        EnvAct::Export(id) => crate::dialogs::export_environment(app, &id),
        EnvAct::Delete(id) => {
            let name = app.ws.data.environment(&id).map(|e| e.name.clone()).unwrap_or_default();
            app.dialog = Some(Dialog::confirm_delete_env(id, name));
        }
    }
}

/// Menu entries of an environment (row menu, context menu, tab header).
fn env_menu(ui: &mut Ui, id: &Id, name: &str, active: bool, acts: &mut Vec<EnvAct>) {
    let p = theme::pal(ui);
    let (icon, label) =
        if active { (icons::CIRCLE, tr("Deactivate")) } else { (icons::CHECK_CIRCLE, tr("Set active")) };
    if ui.button(kit::with_icon(icon, label)).clicked() {
        acts.push(EnvAct::Activate(if active { None } else { Some(id.clone()) }));
        ui.close();
    }
    if ui.button(kit::with_icon(icons::PENCIL_SIMPLE, tr("Rename"))).clicked() {
        acts.push(EnvAct::Rename(id.clone(), name.to_string()));
        ui.close();
    }
    if ui.button(kit::with_icon(icons::COPY, tr("Duplicate"))).clicked() {
        acts.push(EnvAct::Duplicate(id.clone()));
        ui.close();
    }
    if ui.button(kit::with_icon(icons::EXPORT, &trf("{} (Postman)", &[&tr("Export")]))).clicked() {
        acts.push(EnvAct::Export(id.clone()));
        ui.close();
    }
    ui.separator();
    if ui.button(RichText::new(kit::with_icon(icons::TRASH, tr("Delete"))).color(p.err)).clicked() {
        acts.push(EnvAct::Delete(id.clone()));
        ui.close();
    }
}

/// The environment open in the active tab (`Some(None)` = globals).
fn open_in_tab(app: &MerpatiApp) -> Option<Option<Id>> {
    match app.tabs.get(app.active).map(|t| &t.kind) {
        Some(crate::app::TabKind::Environment(t)) => Some(t.env.clone()),
        _ => None,
    }
}

/// One row of the sidebar list. Returns the row response (click = open).
fn env_row(
    ui: &mut Ui,
    icon: &str,
    icon_color: egui::Color32,
    name: &str,
    count: usize,
    selected: bool,
    strong: bool,
) -> (egui::Response, Rect) {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    widgets::a11y_button(&resp, name);
    let hovered = ui.rect_contains_pointer(rect);
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(6), p.accent_soft);
    } else if hovered {
        ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover);
    }
    ui.painter().text(
        pos2(rect.left() + 16.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        theme::icon_font(15.0),
        icon_color,
    );
    let font = if strong || selected { theme::medium(13.5) } else { egui::FontId::proportional(13.5) };
    let g = elide(ui, name, font, p.text, rect.width() - 34.0 - 40.0);
    ui.painter().galley(pos2(rect.left() + 32.0, rect.center().y - g.size().y / 2.0), g, p.text);
    if !hovered {
        ui.painter().text(
            pos2(rect.right() - 10.0, rect.center().y),
            Align2::RIGHT_CENTER,
            count.to_string(),
            egui::FontId::proportional(11.5),
            p.text_faint,
        );
    }
    (resp, rect)
}

pub fn sidebar_list(app: &mut MerpatiApp, ui: &mut Ui) {
    let p = theme::pal(ui);
    let open = open_in_tab(app);
    let mut acts: Vec<EnvAct> = Vec::new();
    ui.spacing_mut().item_spacing.y = 2.0;
    let globals = app.ws.data.globals.len();
    let (r, _) = env_row(ui, icons::GLOBE_SIMPLE, p.text_muted, tr("Globals"), globals, open == Some(None), false);
    if r.on_hover_text(tr("Variables available in every request and environment")).clicked() {
        acts.push(EnvAct::Open(None));
    }
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.label(RichText::new(tr("Environments")).font(theme::medium(11.5)).color(p.text_muted));
    });
    ui.add_space(2.0);
    if app.ws.data.environments.is_empty() {
        ui.add_space(8.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(tr("No environments yet")).font(theme::medium(13.0)));
            ui.label(kit::hint(ui, tr("Keep values like base URLs and tokens per stage.")));
            ui.add_space(6.0);
            if kit::secondary(ui, Some(icons::PLUS), tr("New Environment")).clicked() {
                app.dialog = Some(Dialog::prompt_new_environment());
            }
        });
    }
    for e in &app.ws.data.environments {
        let active = app.ws.data.active_environment.as_deref() == Some(&e.id);
        let selected = open.as_ref() == Some(&Some(e.id.clone()));
        let (icon, color) = if active { (icons::CHECK_CIRCLE, p.ok) } else { (icons::CIRCLE, p.text_faint) };
        let (resp, rect) = env_row(ui, icon, color, &e.name, e.variables.len(), selected, active);
        // The circle switches the active environment.
        let dot = Rect::from_center_size(pos2(rect.left() + 16.0, rect.center().y), vec2(22.0, 22.0));
        let d = ui.interact(dot, ui.make_persistent_id(("env-active", &e.id)), Sense::click());
        widgets::a11y_button(&d, if active { tr("Deactivate") } else { tr("Set active") });
        if d.hovered() && !active {
            ui.painter().text(dot.center(), Align2::CENTER_CENTER, icons::CHECK_CIRCLE, theme::icon_font(15.0), p.ok);
        }
        if d.on_hover_text(if active { tr("Active — click to deactivate") } else { tr("Set active") }).clicked() {
            acts.push(EnvAct::Activate(if active { None } else { Some(e.id.clone()) }));
        }
        if ui.rect_contains_pointer(rect) {
            let m = Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(24.0, 24.0));
            let r = ui
                .scope_builder(egui::UiBuilder::new().max_rect(m), |ui| {
                    kit::icon_button_ex(ui, icons::DOTS_THREE, tr("More actions"), false, 24.0)
                })
                .inner;
            egui::Popup::menu(&r).show(|ui| env_menu(ui, &e.id, &e.name, active, &mut acts));
        }
        if resp.clicked() {
            acts.push(EnvAct::Open(Some(e.id.clone())));
        }
        if resp.double_clicked() {
            acts.push(EnvAct::Activate(Some(e.id.clone())));
        }
        resp.context_menu(|ui| env_menu(ui, &e.id, &e.name, active, &mut acts));
    }
    for a in acts {
        apply(app, a);
    }
}

/// Editable variable table in the style of `widgets::kv_table`: enable box,
/// name, type (default/secret), initial and current value, row actions.
/// Returns true when changed.
pub fn variables_table(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    vars: &mut Vec<Variable>,
    filter: &str,
) -> bool {
    let salt = egui::Id::unique(&id);
    ui.push_id(salt, |ui| variables_table_inner(ui, vars, filter)).inner
}

fn variables_table_inner(ui: &mut Ui, vars: &mut Vec<Variable>, filter: &str) -> bool {
    let p = theme::pal(ui);
    let mut changed = false;
    let mut remove = None;
    const CHECK: f32 = 34.0;
    const KIND: f32 = 96.0;
    const ACT: f32 = 60.0;
    const HEAD: f32 = 28.0;
    const ROW: f32 = 32.0;
    let width = ui.available_width().max(360.0);
    let left = ui.cursor().left();
    let top = ui.cursor().top();
    let cols = width - CHECK - KIND - ACT;
    let key_w = cols * 0.28;
    let val_w = (cols - key_w) / 2.0;
    let xs = [
        left + CHECK,
        left + CHECK + key_w,
        left + CHECK + key_w + KIND,
        left + CHECK + key_w + KIND + val_w,
        left + width - ACT,
    ];
    let mono = theme::mono(mono_size(ui));
    let head = Rect::from_min_size(pos2(left, top), vec2(width, HEAD));
    ui.painter().rect_filled(head, CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, p.chrome);
    let heads = [
        (xs[0], xs[1], tr("Variable"), ""),
        (xs[1], xs[2], tr("Type"), tr("Secret values are masked and never written to the workspace files")),
        (xs[2], xs[3], tr("Initial value"), tr("Shared: saved to the workspace files (secrets excepted)")),
        (xs[3], xs[4], tr("Current value"), tr("Local: used when sending, never committed")),
    ];
    for (i, (x0, x1, label, help)) in heads.iter().enumerate() {
        ui.painter().text(
            pos2(x0 + 10.0, head.center().y),
            Align2::LEFT_CENTER,
            *label,
            theme::medium(11.5),
            p.text_muted,
        );
        if !help.is_empty() {
            let r = Rect::from_min_max(pos2(*x0, head.top()), pos2(*x1, head.bottom()));
            ui.interact(r, ui.make_persistent_id(("var-head", i)), Sense::hover()).on_hover_text(*help);
        }
    }
    let mut y = head.bottom();
    let needle = filter.to_lowercase();
    let mut shown = 0;
    let n = vars.len();
    let ring = |ui: &Ui, r: Rect| {
        ui.painter().rect_stroke(r, CornerRadius::same(5), Stroke::new(1.5, p.accent), StrokeKind::Outside);
    };
    for i in 0..=n {
        let is_new = i == n;
        if !is_new && !needle.is_empty() && !vars[i].key.to_lowercase().contains(&needle) {
            continue;
        }
        if !is_new {
            shown += 1;
        }
        let mut v = if is_new { Variable::new("", "") } else { vars[i].clone() };
        let mut row_changed = false;
        let row_rect = Rect::from_min_size(pos2(left, y), vec2(width, ROW));
        ui.painter().hline(left..=left + width, y, Stroke::new(1.0, p.border));
        let hovered = ui.rect_contains_pointer(row_rect);
        if hovered && !is_new {
            ui.painter().rect_filled(row_rect.shrink2(vec2(1.0, 0.5)), 0.0, p.faint_row());
        }
        let cell = |x0: f32, x1: f32| Rect::from_min_max(pos2(x0 + 4.0, y + 3.0), pos2(x1 - 4.0, y + ROW - 3.0));
        ui.push_id(i, |ui| {
            if !is_new {
                let c =
                    Rect::from_center_size(pos2(left + CHECK / 2.0 + 2.0, row_rect.center().y), vec2(18.0, ROW - 6.0));
                let r = ui.put(c, egui::Checkbox::without_text(&mut v.enabled));
                widgets::a11y_name(&r, &format!("{} {}", tr("Enabled"), v.key));
                row_changed |= r.changed();
            }
            let faded = !is_new && !v.enabled;
            let color = if faded { p.text_faint } else { p.text };
            // Name
            let key_rect = cell(xs[0], xs[1]);
            let r = ui.put(
                key_rect,
                frameless(&mut v.key, key_rect, &mono, if is_new { tr("Add a new variable") } else { "" }, color),
            );
            widgets::a11y_name(&r, tr("Variable"));
            if r.has_focus() {
                ring(ui, key_rect);
            }
            row_changed |= r.changed();
            // Type
            let secret = v.kind == VarKind::Secret;
            let kind_rect = cell(xs[1], xs[2]);
            let (icon, label) =
                if secret { (icons::LOCK_SIMPLE, tr("secret")) } else { (icons::TEXT_AA, tr("default")) };
            let r = ui
                .scope_builder(egui::UiBuilder::new().max_rect(kind_rect), |ui| {
                    ui.centered_and_justified(|ui| {
                        kit::button_ex(
                            ui,
                            kit::Kind::Ghost,
                            Some(icon),
                            &format!("{label} {}", icons::CARET_DOWN),
                            true,
                        )
                    })
                    .inner
                })
                .inner;
            egui::Popup::menu(&r).show(|ui| {
                for (k, icon, label) in [
                    (VarKind::Default, icons::TEXT_AA, tr("default")),
                    (VarKind::Secret, icons::LOCK_SIMPLE, tr("secret")),
                ] {
                    if ui.button(kit::with_icon(icon, label)).clicked() {
                        if v.kind != k {
                            v.kind = k;
                            row_changed = true;
                        }
                        ui.close();
                    }
                }
            });
            // Initial value
            let init_rect = cell(xs[2], xs[3]);
            let r = if secret {
                ui.scope_builder(egui::UiBuilder::new().max_rect(init_rect), |ui| {
                    crate::auth_ui::secret_input(ui, "init", &mut v.value, init_rect.width(), false, "")
                })
                .inner
            } else {
                ui.put(init_rect, frameless(&mut v.value, init_rect, &mono, "", color))
            };
            widgets::a11y_name(&r, tr("Initial value"));
            if r.has_focus() {
                ring(ui, init_rect);
            }
            row_changed |= r.changed();
            // Current value
            let cur_rect = cell(xs[3], xs[4]);
            let mut cur = v.current.clone().unwrap_or_default();
            let hint = if secret || is_new { String::new() } else { v.value.clone() };
            let r = if secret {
                ui.scope_builder(egui::UiBuilder::new().max_rect(cur_rect), |ui| {
                    crate::auth_ui::secret_input(ui, "cur", &mut cur, cur_rect.width(), false, "")
                })
                .inner
            } else {
                ui.put(cur_rect, frameless(&mut cur, cur_rect, &mono, RichText::new(&hint).font(mono.clone()), color))
            };
            widgets::a11y_name(&r, tr("Current value"));
            if r.has_focus() {
                ring(ui, cur_rect);
            }
            if r.changed() {
                v.current = if cur.is_empty() { None } else { Some(cur) };
                row_changed = true;
            }
            if !is_new && hovered {
                let m = Rect::from_center_size(pos2(xs[4] + 16.0, row_rect.center().y), vec2(24.0, 24.0));
                let r = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(m), |ui| {
                        kit::icon_button_ex(ui, icons::DOTS_THREE, tr("More actions"), false, 24.0)
                    })
                    .inner;
                egui::Popup::menu(&r).show(|ui| {
                    ui.add_enabled_ui(v.current.is_some(), |ui| {
                        if ui
                            .button(kit::with_icon(icons::ARROW_LINE_DOWN, tr("Persist current value as initial")))
                            .clicked()
                        {
                            if let Some(c) = v.current.take() {
                                v.value = c;
                                row_changed = true;
                            }
                            ui.close();
                        }
                        if ui
                            .button(kit::with_icon(icons::ARROW_COUNTER_CLOCKWISE, tr("Reset current value")))
                            .clicked()
                        {
                            v.current = None;
                            row_changed = true;
                            ui.close();
                        }
                    });
                });
                let d = Rect::from_center_size(pos2(xs[4] + 44.0, row_rect.center().y), vec2(24.0, 24.0));
                let r = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(d), |ui| {
                        kit::icon_button_ex(ui, icons::TRASH, tr("Delete"), false, 24.0)
                    })
                    .inner;
                if r.clicked() {
                    remove = Some(i);
                }
            }
        });
        y += ROW;
        if row_changed {
            changed = true;
            if is_new {
                if !v.key.is_empty() {
                    vars.push(v);
                }
            } else {
                vars[i] = v;
            }
        }
    }
    let table = Rect::from_min_max(pos2(left, top), pos2(left + width, y));
    for x in &xs {
        ui.painter().vline(*x, table.y_range(), Stroke::new(1.0, p.border));
    }
    ui.painter().rect_stroke(table, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.advance_cursor_after_rect(table);
    if !needle.is_empty() && shown == 0 {
        ui.add_space(8.0);
        ui.label(widgets::muted(ui, trf("No variables match “{}”.", &[&filter])));
    }
    if let Some(i) = remove {
        vars.remove(i);
        changed = true;
    }
    changed
}

/// A frameless single-line cell editor.
fn frameless<'t>(
    text: &'t mut String,
    rect: Rect,
    font: &egui::FontId,
    hint: impl Into<egui::WidgetText>,
    color: egui::Color32,
) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text)
        .hint_text(hint)
        .font(font.clone())
        .text_color(color)
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::symmetric(6, 2))
        .min_size(vec2(0.0, rect.height()))
        .align(Align2::LEFT_CENTER)
        .desired_width(rect.width())
}

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab: &mut EnvTab) {
    let p = theme::pal(ui);
    let (title, count) = match &tab.env {
        None => (tr("Globals").to_string(), app.ws.data.globals.len()),
        Some(id) => match app.ws.data.environment(id) {
            Some(e) => (e.name.clone(), e.variables.len()),
            None => {
                kit::empty_state(
                    ui,
                    icons::GLOBE,
                    tr("This environment was deleted."),
                    tr("Pick another one in the Environments list."),
                    |_| {},
                );
                return;
            }
        },
    };
    let active = tab.env.as_ref().is_some_and(|id| app.ws.data.active_environment.as_deref() == Some(id.as_str()));
    let mut acts: Vec<EnvAct> = Vec::new();
    let (icon, color, subtitle) = match &tab.env {
        None => (icons::GLOBE_SIMPLE, p.info, trf("Global variables · {} variables", &[&count])),
        Some(_) => {
            (icons::GLOBE, if active { p.ok } else { p.text_muted }, trf("Environment · {} variables", &[&count]))
        }
    };
    crate::request_tab::view_header(ui, icon, color, &title, &subtitle, |ui| {
        if let Some(id) = &tab.env {
            let more = kit::icon_button(ui, icons::DOTS_THREE, tr("More actions"));
            egui::Popup::menu(&more).show(|ui| env_menu(ui, id, &title, active, &mut acts));
            if active {
                kit::pill(ui, &format!("{} {}", icons::CHECK_CIRCLE, tr("Active")), p.ok)
                    .on_hover_text(tr("Requests resolve variables from this environment"));
            } else if kit::secondary(ui, Some(icons::CHECK_CIRCLE), tr("Set active")).clicked() {
                acts.push(EnvAct::Activate(Some(id.clone())));
            }
        }
    });
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.set_height(30.0);
        let w = ui.available_width().min(280.0);
        kit::search_field(ui, "env-filter", &mut tab.search, tr("Filter variables"), w);
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(kit::hint(
                ui,
                tr("Initial values are saved in the workspace and can be committed to Git; current values and secrets stay on this machine."),
            ))
            .truncate(),
        )
        .on_hover_text(tr("Initial values are saved in the workspace and can be committed to Git; current values and secrets stay on this machine."));
    });
    ui.add_space(10.0);
    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| match &tab.env {
        None => {
            let mut vars = app.ws.data.globals.clone();
            if variables_table(ui, "globals", &mut vars, &tab.search) {
                app.ws.data.globals = vars;
                app.ws.mark_globals();
            }
        }
        Some(id) => {
            let id = id.clone();
            let Some(env) = app.ws.data.environments.iter_mut().find(|e| e.id == id) else { return };
            let mut vars = env.variables.clone();
            if variables_table(ui, ("env", &id), &mut vars, &tab.search) {
                env.variables = vars;
                app.ws.mark_env(&id);
            }
        }
    });
    for a in acts {
        apply(app, a);
    }
}
