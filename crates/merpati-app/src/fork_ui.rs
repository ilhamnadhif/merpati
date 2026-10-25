//! Review dialog for pulling a fork's parent changes or merging a fork back.

use egui::{Align2, Color32, CornerRadius, FontId, Margin, RichText, Sense, Stroke, Ui, pos2, vec2};

use merpati_core::fork::{self, ChangeKind, DiffOp, Plan};
use merpati_core::model::Id;

use crate::app::MerpatiApp;
use crate::dialogs::{
    Modal, Outcome, body, compact_empty, fit_height, footer, line_galley, segmented, split, trailing,
};
use crate::i18n::{tr, trf};
use crate::{icons, kit, theme};

pub struct ForkSync {
    pub fork: Id,
    /// Pull parent → fork, or merge fork → parent.
    pub pull: bool,
    pub plan: Plan,
    pub selected: usize,
}

impl ForkSync {
    pub fn new(app: &MerpatiApp, fork_id: &str, pull: bool) -> Option<ForkSync> {
        let link = app.ws.fork_of(fork_id)?;
        let parent = app.ws.data.collection(&link.parent)?;
        let fork = app.ws.data.collection(fork_id)?;
        let plan = if pull { fork::plan_pull(link, parent, fork) } else { fork::plan_merge(link, parent, fork) };
        Some(ForkSync { fork: fork_id.to_string(), pull, plan, selected: 0 })
    }
}

/// Creates a fork of `parent` named `name`.
pub fn create(app: &mut MerpatiApp, parent: &str, name: &str) {
    let Some(p) = app.ws.data.collection(parent) else { return };
    let (fork, link) = fork::create_fork(p, name, chrono::Utc::now().timestamp_millis());
    app.ws.add_collection(fork);
    app.ws.forks.push(link);
    app.ws.save_forks();
    app.toast(tr("Fork created"), false);
}

/// Label and color of a change kind.
fn kind_style(kind: ChangeKind, ui: &Ui) -> (&'static str, Color32) {
    let p = theme::pal(ui);
    match kind {
        ChangeKind::Added => (tr("added"), p.ok),
        ChangeKind::Modified => (tr("changed"), p.info),
        ChangeKind::Removed => (tr("removed"), p.err),
        ChangeKind::Conflict => (tr("conflict"), p.warn),
    }
}

pub fn show(app: &mut MerpatiApp, ctx: &egui::Context, s: &mut ForkSync) -> Outcome {
    let Some(link) = app.ws.fork_of(&s.fork).cloned() else { return Outcome::Close };
    let parent_name = app.ws.data.collection(&link.parent).map(|c| c.name.clone()).unwrap_or_default();
    let fork_name = app.ws.data.collection(&s.fork).map(|c| c.name.clone()).unwrap_or_default();
    let (source, target) = if s.pull { (&parent_name, &fork_name) } else { (&fork_name, &parent_name) };
    let (title, icon) = if s.pull {
        (trf("Pull changes from “{}”", &[&parent_name]), icons::GIT_FORK)
    } else {
        (trf("Merge into “{}”", &[&parent_name]), icons::GIT_MERGE)
    };
    if s.plan.is_empty() {
        return Modal::new("fork-sync", 460.0).title(title).icon(icon).show(ctx, |ui| {
            body(ui, |ui| {
                ui.add_space(8.0);
                compact_empty(ui, icons::CHECK_CIRCLE, tr("Already up to date."), tr("There are no changes to apply."));
                ui.add_space(8.0);
            });
            footer(ui, |ui| {
                trailing(ui, |ui| {
                    if kit::secondary(ui, None, tr("Close")).clicked() { Outcome::Close } else { Outcome::Keep }
                })
            })
        });
    }
    let total = s.plan.entries.len();
    let conflicts = s.plan.conflicts();
    let subtitle = if conflicts > 0 {
        trf(
            "{}, {}: choose a side to include them",
            &[
                &crate::dialogs::plural(total, "{} change", "{} changes"),
                &crate::dialogs::plural(conflicts, "{} conflict", "{} conflicts"),
            ],
        )
    } else {
        trf("{} from “{}” to “{}”", &[&crate::dialogs::plural(total, "{} change", "{} changes"), source, target])
    };
    let height = fit_height(ctx, 200.0, 520.0);
    let mut apply = false;
    let out = Modal::new("fork-sync", 1000.0).title(title).subtitle(subtitle).icon(icon).sticky().show(ctx, |ui| {
        let current = s.selected;
        let mut select = None;
        let entries = &mut s.plan.entries;
        let shown =
            entries.get(current).map(|e| (e.name.clone(), e.kind, e.target_text.clone(), e.source_text.clone()));
        split(
            ui,
            height,
            380.0,
            |ui| {
                egui::ScrollArea::vertical().id_salt("fork-list").auto_shrink([false; 2]).show(ui, |ui| {
                    egui::Frame::new().inner_margin(Margin::same(8)).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for (i, e) in entries.iter_mut().enumerate() {
                            ui.push_id(i, |ui| {
                                if change_row(ui, e, i == current) {
                                    select = Some(i);
                                }
                                if e.kind == ChangeKind::Conflict && e.apply {
                                    ui.horizontal(|ui| {
                                        ui.add_space(34.0);
                                        segmented(
                                            ui,
                                            &mut e.take_source,
                                            &[(true, trf("Use {}", &[source])), (false, trf("Keep {}", &[target]))],
                                        );
                                    });
                                    ui.add_space(4.0);
                                }
                            });
                        }
                    });
                });
            },
            |ui| {
                egui::Frame::new().inner_margin(Margin::same(16)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if let Some((name, kind, old, new)) = &shown {
                        let p = theme::pal(ui);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(name).font(theme::semibold(14.5)).color(p.text));
                            let (label, color) = kind_style(*kind, ui);
                            kit::pill(ui, label, color);
                            trailing(ui, |ui| {
                                ui.spacing_mut().item_spacing.x = 10.0;
                                ui.label(RichText::new(format!("+ {source}")).font(theme::mono(12.0)).color(p.ok));
                                ui.label(RichText::new(format!("− {target}")).font(theme::mono(12.0)).color(p.err));
                            });
                        });
                        ui.add_space(8.0);
                        diff_view(ui, old, new, height - 80.0);
                    }
                });
            },
        );
        if let Some(i) = select {
            s.selected = i;
        }
        let n = s.plan.entries.iter().filter(|e| e.apply).count();
        footer(ui, |ui| {
            let p = theme::pal(ui);
            ui.label(RichText::new(trf("{} of {} selected", &[&n, &total])).size(12.0).color(p.text_muted));
            let all = n == total;
            if kit::ghost(ui, None, if all { tr("Select none") } else { tr("Select all") }).clicked() {
                for e in &mut s.plan.entries {
                    e.apply = !all;
                }
            }
            trailing(ui, |ui| {
                let label = format!("{} ({n})", tr("Apply"));
                if ui.add_enabled_ui(n > 0, |ui| kit::primary(ui, &label)).inner.clicked() {
                    apply = true;
                }
                if kit::secondary(ui, None, tr("Cancel")).clicked() { Outcome::Close } else { Outcome::Keep }
            })
        })
    });
    if !apply {
        return out;
    }
    let Some(mut link) = app.ws.fork_of(&s.fork).cloned() else { return Outcome::Close };
    let (Some(parent), Some(fork_c)) =
        (app.ws.data.collection(&link.parent).cloned(), app.ws.data.collection(&s.fork).cloned())
    else {
        return Outcome::Close;
    };
    if s.pull {
        let mut updated = fork_c;
        fork::apply_pull(&mut link, &s.plan, &parent, &mut updated);
        if let Some(c) = app.ws.data.collection_mut(&s.fork) {
            *c = updated;
        }
        app.ws.mark_collection(&s.fork);
    } else {
        let mut updated = parent;
        fork::apply_merge(&mut link, &s.plan, &fork_c, &mut updated);
        let pid = link.parent.clone();
        if let Some(c) = app.ws.data.collection_mut(&pid) {
            *c = updated;
        }
        app.ws.mark_collection(&pid);
    }
    if let Some(l) = app.ws.forks.iter_mut().find(|l| l.fork == s.fork) {
        *l = link;
    }
    app.ws.save_forks();
    app.toast(tr("Changes applied"), false);
    Outcome::Close
}

/// One change: include checkbox, kind label, icon and name. Returns true
/// when the row (not the checkbox) was clicked.
fn change_row(ui: &mut Ui, e: &mut merpati_core::fork::PlanEntry, selected: bool) -> bool {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.add_space(4.0);
        let r = ui.checkbox(&mut e.apply, "").on_hover_text(tr("Include this change"));
        crate::widgets::a11y_name(&r, &e.name);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
        crate::widgets::a11y_button(&resp, &e.name);
        let radius = CornerRadius::same(theme::RADIUS);
        if selected {
            ui.painter().rect_filled(rect, radius, p.accent_soft);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, radius, p.hover);
        }
        let cy = rect.center().y;
        let (label, color) = kind_style(e.kind, ui);
        let g = ui.painter().layout_no_wrap(label.to_string(), theme::medium(11.0), color);
        let pill = egui::Rect::from_min_size(pos2(rect.left() + 8.0, cy - 9.0), vec2(g.size().x + 12.0, 18.0));
        ui.painter().rect_filled(pill, CornerRadius::same(9), theme::soft(color, if dark { 0.18 } else { 0.11 }));
        ui.painter().galley(pill.center() - g.size() / 2.0, g, color);
        let x = pill.right() + 10.0;
        let icon = if e.id == fork::ROOT {
            icons::SLIDERS_HORIZONTAL
        } else if e.is_folder {
            icons::FOLDER_SIMPLE
        } else {
            icons::FILE
        };
        ui.painter().text(pos2(x + 7.0, cy), Align2::CENTER_CENTER, icon, theme::icon_font(14.0), p.text_muted);
        let name = line_galley(ui, &e.name, FontId::proportional(13.0), rect.right() - x - 30.0);
        ui.painter().galley(
            pos2(x + 22.0, cy - name.size().y / 2.0),
            name,
            if e.apply { p.text } else { p.text_muted },
        );
        clicked = resp.clicked();
    });
    clicked
}

fn diff_view(ui: &mut Ui, old: &str, new: &str, height: f32) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let lines = fork::line_diff(old, new);
    egui::Frame::new()
        .fill(p.canvas)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(1))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::both().id_salt("fork-diff").max_height(height.max(120.0)).auto_shrink([false; 2]).show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let width = ui.available_width();
                    for (op, line) in lines {
                        let (sign, color, bg) = match op {
                            DiffOp::Same => (" ", p.text, Color32::TRANSPARENT),
                            DiffOp::Removed => ("−", p.err, theme::soft(p.err, if dark { 0.14 } else { 0.08 })),
                            DiffOp::Added => ("+", p.ok, theme::soft(p.ok, if dark { 0.14 } else { 0.08 })),
                        };
                        // Not wrapped: wrapping labels trim leading spaces (the indentation).
                        egui::Frame::NONE.fill(bg).inner_margin(Margin::symmetric(10, 1)).show(ui, |ui| {
                            ui.set_min_width(width - 20.0);
                            let text_color = if op == DiffOp::Same { p.text_muted } else { color };
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("{sign}  {line}")).font(theme::mono(12.0)).color(text_color),
                                )
                                .extend(),
                            );
                        });
                    }
                },
            );
        });
}
