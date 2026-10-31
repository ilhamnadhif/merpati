//! Shared components drawn with the design tokens of `theme.rs`: buttons,
//! icon buttons, tab strips, segmented controls, pills, keyboard hints,
//! empty states and cards. Screens use these instead of styling egui
//! widgets one by one, so every view looks and behaves the same.

use egui::{
    Align2, Color32, CornerRadius, FontId, Rect, Response, RichText, Role, Sense, Stroke, StrokeKind, Ui, Vec2,
    WidgetInfo, vec2,
};

use crate::theme::{self, RADIUS};

fn body_size(ui: &Ui) -> f32 {
    ui.style().text_styles.get(&egui::TextStyle::Body).map(|f| f.size).unwrap_or(13.5)
}

fn small_size(ui: &Ui) -> f32 {
    ui.style().text_styles.get(&egui::TextStyle::Small).map(|f| f.size).unwrap_or(11.5)
}

/// Icon followed by a label, for menus and ordinary buttons.
pub fn with_icon(icon: &str, text: &str) -> String {
    format!("{icon}  {text}")
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Filled accent: the one main action of a view.
    Primary,
    /// Outlined surface: ordinary actions.
    Secondary,
    /// No frame until hovered: toolbars and quiet actions.
    Ghost,
    /// Outlined, red text: destructive actions.
    Danger,
}

/// A button with an optional leading icon. `small` makes it 24 px high
/// instead of 30.
pub fn button_ex(ui: &mut Ui, kind: Kind, icon: Option<&str>, text: &str, small: bool) -> Response {
    let p = theme::pal(ui);
    let size = if small { small_size(ui) + 0.5 } else { body_size(ui) };
    let height = if small { 24.0 } else { 30.0 };
    let pad = if small { 8.0 } else { 12.0 };
    let enabled = ui.is_enabled();
    let text_color = match kind {
        Kind::Primary => p.on_accent,
        Kind::Danger => p.err,
        Kind::Secondary | Kind::Ghost => p.text,
    };
    let galley = (!text.is_empty())
        .then(|| ui.painter().layout_no_wrap(text.to_string(), theme::medium(size), Color32::PLACEHOLDER));
    let icon_color =
        if matches!(kind, Kind::Primary | Kind::Danger) || galley.is_none() { text_color } else { p.text_muted };
    let icon_galley =
        icon.map(|i| ui.painter().layout_no_wrap(i.to_string(), theme::icon_font(size + 2.0), Color32::PLACEHOLDER));
    let gap = if galley.is_some() && icon_galley.is_some() { 6.0 } else { 0.0 };
    let content_w =
        galley.as_ref().map_or(0.0, |g| g.size().x) + icon_galley.as_ref().map_or(0.0, |g| g.size().x) + gap;
    let width = if galley.is_none() { height } else { content_w + pad * 2.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(width.max(height), height), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let pressed = enabled && resp.is_pointer_button_down_on();
        let (fill, stroke) = match kind {
            Kind::Primary => (if hovered { p.accent_hover } else { p.accent }, Stroke::NONE),
            Kind::Secondary | Kind::Danger => (
                if pressed {
                    p.subtle
                } else if hovered {
                    p.hover
                } else {
                    p.surface
                },
                Stroke::new(1.0, if hovered { p.border_strong } else { p.border }),
            ),
            Kind::Ghost => (
                if pressed {
                    p.subtle
                } else if hovered {
                    p.hover
                } else {
                    Color32::TRANSPARENT
                },
                Stroke::NONE,
            ),
        };
        let painter = ui.painter();
        let radius = CornerRadius::same(RADIUS);
        painter.rect(rect, radius, fill, stroke, StrokeKind::Inside);
        if resp.has_focus() {
            painter.rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(RADIUS + 2),
                Stroke::new(1.5, p.accent),
                StrokeKind::Outside,
            );
        }
        let mut x = rect.center().x - content_w / 2.0;
        let fade = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };
        if let Some(g) = icon_galley {
            let pos = egui::pos2(x, rect.center().y - g.size().y / 2.0);
            x += g.size().x + gap;
            painter.galley(pos, g, fade(icon_color));
        }
        if let Some(g) = galley {
            let pos = egui::pos2(x, rect.center().y - g.size().y / 2.0);
            painter.galley(pos, g, fade(text_color));
        }
    }
    resp
}

pub fn primary(ui: &mut Ui, text: &str) -> Response {
    button_ex(ui, Kind::Primary, None, text, false)
}

pub fn primary_icon(ui: &mut Ui, icon: &str, text: &str) -> Response {
    button_ex(ui, Kind::Primary, Some(icon), text, false)
}

pub fn secondary(ui: &mut Ui, icon: Option<&str>, text: &str) -> Response {
    button_ex(ui, Kind::Secondary, icon, text, false)
}

pub fn ghost(ui: &mut Ui, icon: Option<&str>, text: &str) -> Response {
    button_ex(ui, Kind::Ghost, icon, text, true)
}

/// A square, frameless icon button with a tooltip (also its accessible name).
pub fn icon_button(ui: &mut Ui, icon: &str, tooltip: &str) -> Response {
    icon_button_ex(ui, icon, tooltip, false, 28.0)
}

/// An icon button showing an on/off state (sidebar toggle, wrap, ...).
pub fn icon_toggle(ui: &mut Ui, icon: &str, tooltip: &str, on: bool) -> Response {
    icon_button_ex(ui, icon, tooltip, on, 28.0)
}

pub fn icon_button_ex(ui: &mut Ui, icon: &str, tooltip: &str, on: bool, side: f32) -> Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    let resp = resp.on_hover_text(tooltip);
    let enabled = ui.is_enabled();
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, tooltip));
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let painter = ui.painter();
        let radius = CornerRadius::same(RADIUS);
        if on {
            painter.rect_filled(rect, radius, p.accent_soft);
        } else if resp.is_pointer_button_down_on() {
            painter.rect_filled(rect, radius, p.subtle);
        } else if hovered {
            painter.rect_filled(rect, radius, p.hover);
        }
        if resp.has_focus() {
            painter.rect_stroke(rect, radius, Stroke::new(1.5, p.accent), StrokeKind::Inside);
        }
        let color = if on {
            p.accent_text
        } else if hovered {
            p.text
        } else {
            p.text_muted
        };
        let color = if enabled { color } else { color.gamma_multiply(0.45) };
        painter.text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon_font((side * 0.58).round()), color);
    }
    resp
}

// ---------------------------------------------------------------------------
// Tabs and segmented controls
// ---------------------------------------------------------------------------

/// Splits `Headers (7)` into `("Headers", Some("7"))`.
fn split_count(label: &str) -> (&str, Option<&str>) {
    if let Some(open) = label.rfind(" (")
        && label.ends_with(')')
    {
        let inner = &label[open + 2..label.len() - 1];
        if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit() || c == '/') {
            return (&label[..open], Some(inner));
        }
    }
    (label, None)
}

/// A row of tabs with an underline under the selected one and a hairline
/// under the whole row. A trailing ` (n)` in a label is shown as a count
/// badge. Returns true when the selection changed.
pub fn tab_strip<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, items: &[(T, String)]) -> bool {
    tab_strip_ex(ui, current, items, &[])
}

/// [`tab_strip`] with a small green dot on the tabs in `dots` (a section
/// that has something set, like a body or auth). When the tabs don't fit
/// they move closer together, then scroll sideways (instead of widening the
/// view) with the edge that hides tabs faded.
pub fn tab_strip_ex<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, items: &[(T, String)], dots: &[T]) -> bool {
    let p = theme::pal(ui);
    let size = body_size(ui);
    let start_x = ui.cursor().min.x;
    let right = ui.max_rect().right();
    let tabs: Vec<_> = items
        .iter()
        .map(|(value, label)| {
            let selected = *current == *value;
            let (name, count) = split_count(label);
            let galley = ui.painter().layout_no_wrap(name.to_string(), theme::medium(size), Color32::PLACEHOLDER);
            let count_galley = count.map(|c| {
                ui.painter().layout_no_wrap(
                    c.to_string(),
                    theme::medium(small_size(ui) - 0.5),
                    if selected { p.accent_text } else { p.text_muted },
                )
            });
            (*value, label.as_str(), selected, galley, count_galley)
        })
        .collect();
    let text_w = |t: &(T, &str, bool, std::sync::Arc<egui::Galley>, Option<std::sync::Arc<egui::Galley>>)| {
        t.3.size().x + t.4.as_ref().map_or(0.0, |g| g.size().x + 12.0)
    };
    let total = |pad: f32| -> f32 {
        tabs.iter().map(|t| text_w(t) + 2.0 * pad).sum::<f32>() + 2.0 * tabs.len().saturating_sub(1) as f32
    };
    let pad = if total(10.0) <= ui.available_width() + 0.5 { 10.0 } else { 6.0 };
    let width =
        |t: &(T, &str, bool, std::sync::Arc<egui::Galley>, Option<std::sync::Arc<egui::Galley>>)| text_w(t) + 2.0 * pad;
    let total = total(pad);
    let mut changed = false;
    let mut draw = |ui: &mut Ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for t in &tabs {
                let (value, label, selected, galley, count_galley) = t;
                let (rect, resp) = ui.allocate_exact_size(vec2(width(t), 34.0), Sense::click());
                crate::widgets::a11y_tab(&resp, label, *selected);
                if resp.clicked() && !*selected {
                    *current = *value;
                    changed = true;
                }
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let painter = ui.painter();
                let text_color = if *selected || resp.hovered() { p.text } else { p.text_muted };
                let text_w = galley.size().x;
                let text_pos = egui::pos2(rect.left() + pad, rect.center().y - galley.size().y / 2.0);
                painter.galley(text_pos, galley.clone(), text_color);
                let mut end = rect.left() + pad + text_w;
                if let Some(g) = count_galley {
                    let pill = Rect::from_min_size(
                        egui::pos2(end + 6.0, rect.center().y - 8.0),
                        vec2(g.size().x + 10.0, 16.0),
                    );
                    painter.rect_filled(pill, CornerRadius::same(8), if *selected { p.accent_soft } else { p.subtle });
                    painter.galley(
                        egui::pos2(pill.left() + 5.0, pill.center().y - g.size().y / 2.0),
                        g.clone(),
                        Color32::PLACEHOLDER,
                    );
                    end = pill.right();
                }
                if dots.contains(value) {
                    painter.circle_filled(egui::pos2(end + 5.5, rect.center().y - 5.0), 2.5, p.ok);
                }
                if *selected {
                    let line = Rect::from_min_max(
                        egui::pos2(rect.left() + pad - 4.0, rect.bottom() - 2.0),
                        egui::pos2(rect.right() - pad + 4.0, rect.bottom()),
                    );
                    painter.rect_filled(line, CornerRadius::same(1), p.accent);
                }
                if resp.has_focus() {
                    painter.rect_stroke(
                        rect.shrink(2.0),
                        CornerRadius::same(RADIUS),
                        Stroke::new(1.5, p.accent),
                        StrokeKind::Inside,
                    );
                }
            }
        })
        .response
        .rect
        .bottom()
    };
    let bottom = if total <= ui.available_width() + 0.5 {
        draw(ui)
    } else {
        let id = ui.next_auto_id().with("tab-strip");
        let out = egui::ScrollArea::horizontal().id_salt(id).auto_shrink([false, true]).show(ui, |ui| draw(ui));
        let (view, offset) = (out.inner_rect, out.state.clamped_offset().x);
        let hidden_right = out.content_size.x - view.width() - offset;
        let fill = ui.visuals().panel_fill;
        if offset > 0.5 {
            fade(ui.painter(), Rect::from_min_size(view.left_top(), vec2(28.0, view.height() - 1.0)), fill, false);
        }
        if hidden_right > 0.5 {
            let edge =
                Rect::from_min_size(egui::pos2(view.right() - 28.0, view.top()), vec2(28.0, view.height() - 1.0));
            fade(ui.painter(), edge, fill, true);
        }
        out.inner
    };
    ui.painter().hline(start_x..=right, bottom - 0.5, Stroke::new(1.0, p.border));
    changed
}

/// Covers `rect` with `color`, from clear to opaque towards the right
/// (`to_right`) or the left: the edge of a strip that hides more.
fn fade(painter: &egui::Painter, rect: Rect, color: Color32, to_right: bool) {
    let (left, right) = if to_right { (Color32::TRANSPARENT, color) } else { (color, Color32::TRANSPARENT) };
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), left);
    mesh.colored_vertex(rect.right_top(), right);
    mesh.colored_vertex(rect.right_bottom(), right);
    mesh.colored_vertex(rect.left_bottom(), left);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(mesh);
}

/// A segmented control (`Pretty | Raw | Preview`). Returns true when the
/// selection changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, items: &[(T, &str)]) -> bool {
    let p = theme::pal(ui);
    let size = small_size(ui) + 0.5;
    let galleys: Vec<_> = items
        .iter()
        .map(|(_, label)| ui.painter().layout_no_wrap(label.to_string(), theme::medium(size), Color32::PLACEHOLDER))
        .collect();
    let seg_w: Vec<f32> = galleys.iter().map(|g| g.size().x + 20.0).collect();
    let total = seg_w.iter().sum::<f32>() + 4.0;
    // Segment ids come from this control's own (auto) id, so two controls
    // with the same labels in one view don't share clicks.
    let (rect, base) = ui.allocate_exact_size(vec2(total, 28.0), Sense::hover());
    let mut changed = false;
    ui.painter().rect_filled(rect, CornerRadius::same(7), p.subtle);
    let mut x = rect.left() + 2.0;
    for (i, ((value, label), galley)) in items.iter().zip(galleys).enumerate() {
        let seg = Rect::from_min_size(egui::pos2(x, rect.top() + 2.0), vec2(seg_w[i], rect.height() - 4.0));
        x += seg_w[i];
        let id = base.id.with(("segment", label, i));
        let resp = ui.interact(seg, id, Sense::click());
        let selected = *current == *value;
        crate::widgets::a11y_tab(&resp, label, selected);
        if resp.clicked() && !selected {
            *current = *value;
            changed = true;
        }
        let painter = ui.painter();
        if selected {
            painter.rect(seg, CornerRadius::same(5), p.surface, Stroke::new(1.0, p.border), StrokeKind::Inside);
        } else if resp.hovered() {
            painter.rect_filled(seg, CornerRadius::same(5), p.hover);
        }
        let color = if selected || resp.hovered() { p.text } else { p.text_muted };
        painter.galley(seg.center() - galley.size() / 2.0, galley, color);
    }
    changed
}

// ---------------------------------------------------------------------------
// Small pieces
// ---------------------------------------------------------------------------

/// A rounded label on a soft fill of `color` (methods, statuses, counts).
pub fn pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let dark = ui.visuals().dark_mode;
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::medium(small_size(ui)), color);
    let size = galley.size() + vec2(14.0, 6.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let fill = theme::soft(color, if dark { 0.18 } else { 0.11 });
        ui.painter().rect_filled(rect, CornerRadius::same((size.y / 2.0) as u8), fill);
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    resp
}

/// `200 OK` style status pill.
pub fn status_pill(ui: &mut Ui, status: u16, reason: &str) -> Response {
    let color = theme::status_color(status, ui.visuals().dark_mode);
    let text = if reason.is_empty() { status.to_string() } else { format!("{status} {reason}") };
    pill(ui, &text, color)
}

/// A muted metric label (`2 ms`, `956 B`) with an icon.
pub fn metric(ui: &mut Ui, icon: &str, text: &str) -> Response {
    let p = theme::pal(ui);
    ui.add(egui::Label::new(
        RichText::new(format!("{icon} {text}")).color(p.text_muted).font(theme::medium(small_size(ui) + 0.5)),
    ))
}

/// A keyboard shortcut hint (`⌘↩`).
pub fn kbd(ui: &mut Ui, keys: &str) -> Response {
    let p = theme::pal(ui);
    let galley = ui.painter().layout_no_wrap(keys.to_string(), FontId::proportional(small_size(ui)), p.text_muted);
    let size = galley.size() + vec2(10.0, 4.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect(rect, CornerRadius::same(4), p.subtle, Stroke::new(1.0, p.border), StrokeKind::Inside);
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text_muted);
    }
    resp
}

/// A filled dot (environment color, connection state).
pub fn dot(ui: &mut Ui, color: Color32, radius: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(radius * 2.0 + 2.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
    resp
}

/// The chevron of a drop-down, for `ComboBox::icon` (egui's default is a
/// filled triangle that doesn't match the line icons).
pub fn combo_caret(ui: &Ui, rect: Rect, visuals: &egui::style::WidgetVisuals, open: bool) {
    let c = rect.center();
    let (w, h) = (3.5, if open { -1.8 } else { 1.8 });
    let color = visuals.fg_stroke.color.gamma_multiply(0.8);
    let points = vec![egui::pos2(c.x - w, c.y - h), egui::pos2(c.x, c.y + h), egui::pos2(c.x + w, c.y - h)];
    ui.painter().add(egui::Shape::line(points, Stroke::new(1.4, color)));
}

/// Focus indication of an input or input group drawn by hand: a 1 px accent
/// border and a soft accent halo just outside it (the same signal everywhere,
/// and calmer than a thick red outline, which reads like an error).
pub fn focus_ring(ui: &Ui, rect: Rect, radius: u8) {
    let p = theme::pal(ui);
    let halo = theme::soft(p.accent, if ui.visuals().dark_mode { 0.30 } else { 0.22 });
    let painter = ui.painter();
    painter.rect_stroke(rect, CornerRadius::same(radius), Stroke::new(3.0, halo), StrokeKind::Outside);
    painter.rect_stroke(rect, CornerRadius::same(radius), Stroke::new(1.0, p.accent), StrokeKind::Inside);
}

/// A thin vertical divider between toolbar groups.
pub fn vdivider(ui: &mut Ui, height: f32) {
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, height), Sense::hover());
    ui.painter().vline(rect.center().x, rect.y_range(), Stroke::new(1.0, p.border));
}

/// A section title (`Query parameters`) in semibold, slightly smaller than body.
pub fn section_title(text: &str) -> RichText {
    RichText::new(text).font(theme::semibold(13.0))
}

/// Muted helper text under a field or section.
pub fn hint(ui: &Ui, text: &str) -> RichText {
    RichText::new(text).color(theme::pal(ui).text_muted).small()
}

/// A bordered card.
pub fn card(ui: &Ui) -> egui::Frame {
    let p = theme::pal(ui);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(14))
}

/// A centered empty state: an icon in a soft circle, a title, a line of
/// explanation, then whatever `actions` adds (buttons, shortcut hints).
pub fn empty_state(ui: &mut Ui, icon: &str, title: &str, body: &str, actions: impl FnOnce(&mut Ui)) {
    let p = theme::pal(ui);
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.28).clamp(12.0, 160.0));
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(56.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 28.0, p.subtle);
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon_font(26.0), p.text_muted);
        ui.add_space(10.0);
        ui.label(RichText::new(title).font(theme::semibold(15.0)).color(p.text));
        ui.add_space(2.0);
        ui.label(RichText::new(body).color(p.text_muted));
        ui.add_space(12.0);
        actions(ui);
    });
}

/// A single-line search field with a magnifier inside and a clear button.
pub fn search_field(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    hint_text: &str,
    width: f32,
) -> Response {
    let p = theme::pal(ui);
    let edit = egui::TextEdit::singleline(text)
        .id_salt(id)
        .hint_text(hint_text)
        .desired_width(width)
        .margin(egui::Margin { left: 26, right: 22, top: 4, bottom: 4 });
    let resp = ui.add(edit);
    let r = resp.rect;
    ui.painter().text(
        egui::pos2(r.left() + 13.0, r.center().y),
        Align2::CENTER_CENTER,
        crate::icons::MAGNIFYING_GLASS,
        theme::icon_font(14.0),
        p.text_faint,
    );
    if !text.is_empty() {
        let x_rect = Rect::from_center_size(egui::pos2(r.right() - 12.0, r.center().y), Vec2::splat(16.0));
        let x = ui.interact(x_rect, resp.id.with("clear"), Sense::click());
        let color = if x.hovered() { p.text } else { p.text_faint };
        ui.painter().text(
            x_rect.center(),
            Align2::CENTER_CENTER,
            crate::icons::X_CIRCLE,
            theme::icon_font(14.0),
            color,
        );
        if x.clicked() {
            text.clear();
        }
    }
    resp
}

/// `ScrollArea::show_rows` for rows of exactly `row_h` laid out edge to edge.
/// egui places the rows `row_h + item_spacing.y` apart, so the spacing is
/// zeroed around the area too: otherwise the rows slip by that much each
/// time one scrolls out of view and the bottom of the list is left empty.
pub fn show_rows<R>(
    area: egui::ScrollArea,
    ui: &mut Ui,
    row_h: f32,
    total: usize,
    add: impl FnOnce(&mut Ui, std::ops::Range<usize>) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        area.show_rows(ui, row_h, total, add)
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows drawn by `show_rows` (top, bottom) and the list's visible rect.
    fn drawn_rows(offset: Option<f32>) -> (Vec<(f32, f32)>, Rect) {
        let ctx = egui::Context::default();
        let mut rows = Vec::new();
        let mut inner = Rect::NOTHING;
        // The third frame knows the content size and has scrolled.
        for _ in 0..3 {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(300.0, 400.0))),
                ..Default::default()
            };
            let out = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default_margins().show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    let mut area = egui::ScrollArea::vertical().auto_shrink([false; 2]);
                    area = match offset {
                        Some(y) => area.vertical_scroll_offset(y),
                        None => area.stick_to_bottom(true),
                    };
                    rows.clear();
                    inner = show_rows(area, ui, 28.0, 100, |ui, range| {
                        // As the callers do.
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for _ in range {
                            let (rect, _) = ui.allocate_exact_size(vec2(100.0, 28.0), Sense::hover());
                            rows.push((rect.top(), rect.bottom()));
                        }
                    })
                    .inner_rect;
                });
            });
            out.drop_without_applying_deltas();
        }
        (rows, inner)
    }

    /// The rows in view fill the list, down to the last one at its bottom.
    #[test]
    fn rows_fill_the_list_down_to_the_bottom() {
        for offset in [Some(0.0), Some(500.0), Some(1234.0), None] {
            let (rows, inner) = drawn_rows(offset);
            let (top, bottom) = (rows.first().unwrap().0, rows.last().unwrap().1);
            assert!(top <= inner.top() + 0.5, "{offset:?}: first row at {top}, list from {}", inner.top());
            assert!(bottom >= inner.bottom() - 0.5, "{offset:?}: last row ends {bottom}, list to {}", inner.bottom());
            assert!(rows.windows(2).all(|w| w[0].1 == w[1].0), "{offset:?}: gaps between rows");
        }
        // At the end of the list its last row ends exactly at the bottom.
        let (rows, inner) = drawn_rows(None);
        assert!(
            (rows.last().unwrap().1 - inner.bottom()).abs() < 0.5,
            "{} vs {}",
            rows.last().unwrap().1,
            inner.bottom()
        );
    }

    #[test]
    fn counts_are_split_from_labels() {
        assert_eq!(split_count("Headers (7)"), ("Headers", Some("7")));
        assert_eq!(split_count("Test Results (3/3)"), ("Test Results", Some("3/3")));
        assert_eq!(split_count("Body"), ("Body", None));
        assert_eq!(split_count("Docs (beta)"), ("Docs (beta)", None));
        assert_eq!(with_icon("+", "New"), "+  New");
    }
}
