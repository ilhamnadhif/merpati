//! Modal dialogs, the console panel, session persistence and export helpers.
//!
//! Every dialog is drawn with [`Modal`]: a title row with a close button, a
//! padded [`body`] and a [`footer`] whose buttons sit on the right (main
//! action rightmost). Escape closes any dialog.

use std::path::PathBuf;
use std::sync::Arc;

use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Galley, Key, Layout, Margin, Modifiers, Rect, Response, RichText,
    Sense, Stroke, StrokeKind, TextFormat, Ui, UiBuilder, Vec2, pos2, vec2,
};
use serde::{Deserialize, Serialize};

use merpati_convert::codegen::Language;
use merpati_core::model::{HttpVersion, Id, Item, RequestItem};
use merpati_core::postman::ExportOptions;
use merpati_core::prepared::{PrepareContext, prepare};
use merpati_core::settings::{AppSettings, ClientCertificate, Language as UiLanguage, ResponseLayout, ThemePreference};
use merpati_core::variables::Resolver;

use crate::app::{ItemRef, MerpatiApp, Tab, TabKind};
use crate::i18n::{tr, trf};
use crate::state::WsState;
use crate::{icons, kit, theme, widgets};

pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

/// Title, method badge and unsaved marker for a tab.
pub fn tab_title(tab: &Tab, ws: &WsState) -> (String, Option<String>, bool) {
    let lookup = |r: &ItemRef| {
        ws.data
            .collection(&r.collection)
            .and_then(|c| c.find_request(&r.item))
            .map(|i| (i.name.clone(), i.request.badge()))
    };
    let deleted = || (tr("(deleted)").to_string(), None, false);
    match &tab.kind {
        TabKind::Request(t) => match (&t.location, &t.scratch) {
            (Some(r), _) => lookup(r).map(|(n, b)| (n, Some(b), false)).unwrap_or_else(deleted),
            (None, Some(i)) => (i.name.clone(), Some(i.request.badge()), true),
            _ => ("Request".into(), None, false),
        },
        TabKind::WebSocket(t) => match (&t.location, &t.scratch) {
            (Some(r), _) => lookup(r).map(|(n, _)| (n, Some("WS".into()), false)).unwrap_or_else(deleted),
            (None, Some(i)) => (i.name.clone(), Some("WS".into()), true),
            _ => ("WebSocket".into(), None, false),
        },
        TabKind::Container(c) => {
            let name = ws
                .data
                .collection(&c.collection)
                .map(|col| match &c.folder {
                    Some(f) => col.find(f).map(|i| i.name().to_string()).unwrap_or_default(),
                    None => col.name.clone(),
                })
                .unwrap_or_default();
            (name, None, false)
        }
        TabKind::Environment(e) => match &e.env {
            None => (tr("Globals").to_string(), None, false),
            Some(id) => (ws.data.environment(id).map(|e| e.name.clone()).unwrap_or_default(), None, false),
        },
        TabKind::Runner(r) => {
            (ws.data.collection(&r.collection).map(|c| c.name.clone()).unwrap_or_default(), None, false)
        }
        TabKind::Spec(t) => (t.title(), None, t.dirty()),
        TabKind::Capture(c) => (format!("{}{}", tr("Capture"), if c.is_running() { " ●" } else { "" }), None, false),
        TabKind::Flow(f) => (f.flow.name.clone(), None, f.dirty()),
        TabKind::Webhook(w) => (format!("Webhook{}", if w.is_running() { " ●" } else { "" }), None, false),
        other @ (TabKind::Mqtt(_) | TabKind::Grpc(_) | TabKind::SocketIo(_) | TabKind::Mcp(_) | TabKind::Ai(_)) => {
            match (other.location(), other.scratch()) {
                (Some(r), _) => lookup(r).map(|(n, b)| (n, Some(b), false)).unwrap_or_else(deleted),
                (None, Some(i)) => (i.name.clone(), Some(i.request.badge()), true),
                _ => ("Request".into(), None, false),
            }
        }
        TabKind::Mock(m) => (
            format!("Mock · {}", ws.data.collection(&m.collection).map(|c| c.name.clone()).unwrap_or_default()),
            None,
            false,
        ),
    }
}

/// Icon shown in front of the title of tabs without a method badge.
pub fn tab_icon(tab: &Tab) -> &'static str {
    match &tab.kind {
        TabKind::Container(c) if c.folder.is_some() => icons::FOLDER_SIMPLE,
        TabKind::Container(_) => icons::STACK,
        TabKind::Environment(e) if e.env.is_none() => icons::GLOBE_SIMPLE,
        TabKind::Environment(_) => icons::GLOBE,
        TabKind::Runner(_) => icons::PLAY_CIRCLE,
        TabKind::Spec(_) => icons::FILE_CODE,
        TabKind::Capture(_) => icons::BROADCAST,
        TabKind::Flow(_) => icons::FLOW_ARROW,
        TabKind::Webhook(_) => icons::WEBHOOKS_LOGO,
        TabKind::Mock(_) => icons::CUBE,
        _ => icons::FILE,
    }
}

// ---------------------------------------------------------------------------
// Dialog chrome: frame, header, body, footer and the small parts dialogs share
// ---------------------------------------------------------------------------

/// Corner radius of dialogs.
const RADIUS_DIALOG: u8 = 12;
/// Horizontal padding of dialog content.
const PAD: i8 = 20;

/// What a dialog wants after a frame.
pub enum Outcome {
    Keep,
    Close,
}

/// A dialog window. Configure it, then [`Modal::show`] its content, made of
/// [`body`], [`footer`] and (for two-pane dialogs) [`split`].
pub struct Modal {
    id: &'static str,
    width: f32,
    title: Option<String>,
    subtitle: Option<String>,
    icon: Option<&'static str>,
    sticky: bool,
    top: bool,
}

impl Modal {
    pub fn new(id: &'static str, width: f32) -> Modal {
        Modal { id, width, title: None, subtitle: None, icon: None, sticky: false, top: false }
    }

    /// Title row with a close button. Dialogs without one draw their own top.
    pub fn title(mut self, title: impl Into<String>) -> Modal {
        self.title = Some(title.into());
        self
    }

    /// Muted line under the title.
    pub fn subtitle(mut self, subtitle: impl Into<String>) -> Modal {
        self.subtitle = Some(subtitle.into());
        self
    }

    /// Icon tile in front of the title.
    pub fn icon(mut self, icon: &'static str) -> Modal {
        self.icon = Some(icon);
        self
    }

    /// A click outside does not close it (forms whose input would be lost).
    pub fn sticky(mut self) -> Modal {
        self.sticky = true;
        self
    }

    /// Pinned near the top instead of centered, so a search field stays put
    /// while the list under it grows and shrinks.
    pub fn top(mut self) -> Modal {
        self.top = true;
        self
    }

    pub fn show(self, ctx: &egui::Context, add: impl FnOnce(&mut Ui) -> Outcome) -> Outcome {
        let style = ctx.global_style();
        let dark = style.visuals.dark_mode;
        let p = theme::palette(dark);
        let screen = ctx.content_rect();
        let width = self.width.min(screen.width() - 32.0).max(260.0);
        let id = egui::Id::unique(("merpati-dialog", self.id));
        let mut area = egui::Modal::default_area(id);
        if self.top {
            area = area.anchor(Align2::CENTER_TOP, vec2(0.0, (screen.height() * 0.12).clamp(20.0, 120.0)));
        }
        let frame = egui::Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, if dark { p.border_strong } else { p.border }))
            .corner_radius(CornerRadius::same(RADIUS_DIALOG))
            .shadow(style.visuals.window_shadow);
        let mut close = false;
        let resp = egui::Modal::new(id)
            .area(area)
            .frame(frame)
            .backdrop_color(Color32::from_black_alpha(if dark { 150 } else { 72 }))
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing.y = 0.0;
                if let Some(title) = &self.title {
                    close = header(ui, title, self.subtitle.as_deref(), self.icon);
                }
                add(ui)
            });
        let escape =
            resp.is_top_modal && !resp.any_popup_open && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        if close || escape || (!self.sticky && resp.backdrop_response.clicked()) {
            return Outcome::Close;
        }
        resp.inner
    }
}

/// Title row: icon tile, title and subtitle, close button. Returns true when
/// the close button was pressed.
fn header(ui: &mut Ui, title: &str, subtitle: Option<&str>, icon: Option<&str>) -> bool {
    let p = theme::pal(ui);
    let mut close = false;
    egui::Frame::new().inner_margin(Margin { left: PAD, right: 14, top: 14, bottom: 13 }).show(ui, |ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
        let cy = rect.center().y;
        let mut x = rect.left();
        if let Some(icon) = icon {
            let tile = Rect::from_min_size(pos2(x, cy - 16.0), Vec2::splat(32.0));
            ui.painter().rect_filled(tile, CornerRadius::same(8), p.accent_soft);
            ui.painter().text(tile.center(), Align2::CENTER_CENTER, icon, theme::icon_font(17.0), p.accent_text);
            x += 44.0;
        }
        let close_rect = Rect::from_center_size(pos2(rect.right() - 14.0, cy), Vec2::splat(28.0));
        let text_w = (close_rect.left() - 12.0 - x).max(40.0);
        let tg = line_galley(ui, title, theme::semibold(16.5), text_w);
        match subtitle {
            None => {
                ui.painter().galley(pos2(x, cy - tg.size().y / 2.0), tg, p.text);
            }
            Some(s) => {
                let sg = line_galley(ui, s, FontId::proportional(12.5), text_w);
                let top = cy - (tg.size().y + 1.0 + sg.size().y) / 2.0;
                let sub_y = top + tg.size().y + 1.0;
                ui.painter().galley(pos2(x, top), tg, p.text);
                ui.painter().galley(pos2(x, sub_y), sg, p.text_muted);
            }
        }
        let name = format!("{} (Esc)", tr("Close"));
        if ui.put(close_rect, |ui: &mut Ui| kit::icon_button(ui, icons::X, &name)).clicked() {
            close = true;
        }
    });
    hairline(ui);
    close
}

/// The padded content of a dialog.
pub fn body<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .inner_margin(Margin { left: PAD, right: PAD, top: 16, bottom: 18 })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = ui.ctx().global_style().spacing.item_spacing;
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// The button bar of a dialog. Hints go first (left); the buttons go in
/// [`trailing`], main action first so that it ends up rightmost.
pub fn footer<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    hairline(ui);
    let r = RADIUS_DIALOG - 1;
    egui::Frame::new()
        .fill(side_fill(ui))
        .corner_radius(CornerRadius { nw: 0, ne: 0, sw: r, se: r })
        .inner_margin(Margin::symmetric(PAD, 12))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = ui.ctx().global_style().spacing.item_spacing;
            let w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(w, 30.0), Layout::left_to_right(Align::Center), add).inner
        })
        .inner
}

/// The right-aligned end of a row; widgets added first end up rightmost.
pub fn trailing<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(Layout::right_to_left(Align::Center), add).inner
}

/// Two panes of `height`: a navigation list on a tinted background (`left`,
/// `left_w` wide) and the content (`right`), each in its own child ui.
pub fn split(ui: &mut Ui, height: f32, left_w: f32, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let left_rect = Rect::from_min_size(rect.min, vec2(left_w, height));
    let right_rect = Rect::from_min_max(pos2(left_rect.right() + 1.0, rect.top()), rect.max);
    ui.painter().rect_filled(left_rect, 0.0, side_fill(ui));
    ui.painter().vline(left_rect.right() + 0.5, rect.y_range(), Stroke::new(1.0, p.border));
    let spacing = ui.ctx().global_style().spacing.item_spacing;
    let mut l = ui.new_child(UiBuilder::new().max_rect(left_rect).layout(Layout::top_down(Align::Min)));
    l.set_clip_rect(left_rect);
    l.spacing_mut().item_spacing = spacing;
    left(&mut l);
    let mut r = ui.new_child(UiBuilder::new().max_rect(right_rect).layout(Layout::top_down(Align::Min)));
    r.set_clip_rect(right_rect);
    r.spacing_mut().item_spacing = spacing;
    right(&mut r);
}

/// A 1 px line across the dialog.
pub(crate) fn hairline(ui: &mut Ui) {
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.border);
}

/// Background of footers and navigation panes: a step away from the dialog
/// surface in both themes.
pub(crate) fn side_fill(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode { ui.visuals().faint_bg_color } else { theme::pal(ui).chrome }
}

/// Space a dialog body may use before the window cuts it off.
pub(crate) fn fit_height(ctx: &egui::Context, reserved: f32, max: f32) -> f32 {
    (ctx.content_rect().height() - reserved).max(140.0).min(max)
}

/// One line of text laid out to at most `max_w`, with `…` when cut. Drawn
/// with the color passed to `painter.galley`.
pub(crate) fn line_galley(ui: &Ui, text: &str, font: FontId, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_string(), TextFormat::simple(font, Color32::PLACEHOLDER));
    job.wrap =
        TextWrapping { max_width: max_w.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

/// `↑↓ Navigate  ↩ Open  esc Close` style hints.
fn key_hints(ui: &mut Ui, hints: &[(&str, &str)]) {
    let p = theme::pal(ui);
    ui.spacing_mut().item_spacing.x = 5.0;
    for (i, (keys, what)) in hints.iter().enumerate() {
        if i > 0 {
            ui.add_space(8.0);
        }
        kit::kbd(ui, keys);
        ui.label(RichText::new(*what).size(12.0).color(p.text_muted));
    }
}

/// A label above a form field.
pub(crate) fn field_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::medium(12.5)).color(theme::pal(ui).text));
}

/// A single-line text input as tall as a button.
pub(crate) fn input(text: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).margin(Margin::symmetric(8, 6))
}

/// A masked input with a show/hide button (the passphrase of a certificate,
/// a proxy password).
pub(crate) fn secret_input(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    width: f32,
) -> Response {
    let id = ui.make_persistent_id(id);
    let mut reveal: bool = ui.data(|d| d.get_temp(id).unwrap_or(false));
    let r = ui.add(input(text).password(!reveal).desired_width(width - 36.0).font(egui::TextStyle::Monospace));
    let (icon, name) = if reveal { (icons::EYE_SLASH, tr("Hide")) } else { (icons::EYE, tr("Show")) };
    if kit::icon_button(ui, icon, name).clicked() {
        reveal = !reveal;
        ui.data_mut(|d| d.insert_temp(id, reveal));
    }
    r
}

/// An on/off switch, named `name` for screen readers.
pub(crate) fn toggle(ui: &mut Ui, on: &mut bool, name: &str) -> Response {
    let p = theme::pal(ui);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(36.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let enabled = ui.is_enabled();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, enabled, *on, name));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let off = if ui.visuals().dark_mode { p.border_strong } else { Color32::from_rgb(214, 214, 221) };
        let track = off.lerp_to_gamma(p.accent, t);
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(10), track);
        let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
        painter.circle_filled(pos2(x, rect.center().y + 0.5), 8.0, Color32::from_black_alpha(36));
        painter.circle_filled(pos2(x, rect.center().y), 8.0, Color32::WHITE);
        if resp.has_focus() {
            painter.rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(12),
                Stroke::new(1.5, p.accent),
                StrokeKind::Outside,
            );
        }
    }
    resp
}

/// The tone of a [`banner`].
#[derive(Clone, Copy)]
pub(crate) enum Tone {
    Info,
    Ok,
    #[allow(dead_code)]
    Warn,
    Err,
}

/// A message with an icon on a soft fill (errors, notes, results).
pub(crate) fn banner(ui: &mut Ui, tone: Tone, text: &str) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let (icon, color) = match tone {
        Tone::Info => (icons::INFO, p.info),
        Tone::Ok => (icons::CHECK_CIRCLE, p.ok),
        Tone::Warn => (icons::WARNING, p.warn),
        Tone::Err => (icons::WARNING_CIRCLE, p.err),
    };
    egui::Frame::new()
        .fill(theme::soft(color, if dark { 0.13 } else { 0.07 }))
        .stroke(Stroke::new(1.0, theme::soft(color, if dark { 0.32 } else { 0.24 })))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(RichText::new(icon).font(theme::icon_font(15.0)).color(color));
                ui.add(egui::Label::new(RichText::new(text).color(p.text)).wrap());
            });
        });
}

/// A row of a navigation or picker list: optional icon, label, optional
/// muted text on the right. Selected rows get the accent tint.
pub(crate) fn nav_item(ui: &mut Ui, icon: Option<&str>, label: &str, trail: Option<&str>, selected: bool) -> Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
    widgets::a11y_tab(&resp, label, selected);
    if ui.is_rect_visible(rect) {
        let radius = CornerRadius::same(theme::RADIUS);
        if selected {
            ui.painter().rect_filled(rect, radius, p.accent_soft);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, radius, p.hover);
        }
        let cy = rect.center().y;
        let mut x = rect.left() + 10.0;
        if let Some(icon) = icon {
            let color = if selected { p.accent_text } else { p.text_muted };
            ui.painter().text(pos2(x + 8.0, cy), Align2::CENTER_CENTER, icon, theme::icon_font(16.0), color);
            x += 28.0;
        }
        let mut right = rect.right() - 10.0;
        if let Some(t) = trail {
            let g = line_galley(ui, t, FontId::proportional(11.5), 110.0);
            let w = g.size().x;
            ui.painter().galley(pos2(right - w, cy - g.size().y / 2.0), g, p.text_muted);
            right -= w + 8.0;
        }
        let font = if selected { theme::medium(13.0) } else { FontId::proportional(13.0) };
        let g = line_galley(ui, label, font, right - x);
        ui.painter().galley(pos2(x, cy - g.size().y / 2.0), g, p.text);
        if resp.has_focus() {
            ui.painter().rect_stroke(rect, radius, Stroke::new(1.5, p.accent), StrokeKind::Inside);
        }
    }
    resp
}

/// Small centered "nothing here" note for panes too small for
/// `kit::empty_state`.
pub(crate) fn compact_empty(ui: &mut Ui, icon: &str, title: &str, text: &str) {
    let p = theme::pal(ui);
    ui.vertical_centered(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(RichText::new(icon).font(theme::icon_font(22.0)).color(p.text_faint));
        ui.label(RichText::new(title).font(theme::medium(13.5)).color(p.text));
        if !text.is_empty() {
            ui.label(RichText::new(text).size(12.5).color(p.text_muted));
        }
    });
}

/// Rows of a settings group: a label and help text on the left, the control
/// on the right, hairlines between rows.
struct Rows<'a> {
    ui: &'a mut Ui,
    n: usize,
    enabled: bool,
}

impl Rows<'_> {
    /// Greys out the rows added after this (and their controls) when false.
    fn enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    fn separator(&mut self) {
        if self.n > 0 {
            let p = theme::pal(self.ui);
            let y = self.ui.cursor().top();
            self.ui.painter().hline(self.ui.max_rect().x_range(), y, Stroke::new(1.0, p.border));
        }
        self.n += 1;
    }

    fn row(&mut self, label: &str, help: &str, add: impl FnOnce(&mut Ui)) {
        self.separator();
        let ui = &mut *self.ui;
        let p = theme::pal(ui);
        let w = ui.available_width();
        // The label column takes what the control leaves (measured on the
        // previous pass); text fields fill a little under half the row.
        let id = ui.make_persistent_id(("settings-row", label));
        let measured: Option<f32> = ui.data(|d| d.get_temp(id));
        let control_w = measured.unwrap_or(w * 0.46).min(w - 180.0).max(36.0);
        let label_w = w - control_w - 24.0;
        let lg = ui.painter().layout(label.to_string(), FontId::proportional(13.5), Color32::PLACEHOLDER, label_w);
        let hg = (!help.is_empty())
            .then(|| ui.painter().layout(help.to_string(), FontId::proportional(12.0), Color32::PLACEHOLDER, label_w));
        let text_h = lg.size().y + hg.as_ref().map_or(0.0, |g| g.size().y + 2.0);
        let pad = if help.is_empty() { 14.0 } else { 22.0 };
        let (rect, _) = ui.allocate_exact_size(vec2(w, text_h.max(30.0) + pad), Sense::hover());
        let fade = |c: Color32| if self.enabled { c } else { c.gamma_multiply(0.5) };
        let y = rect.center().y - text_h / 2.0;
        let label_h = lg.size().y;
        ui.painter().galley(pos2(rect.left(), y), lg, fade(p.text));
        if let Some(hg) = hg {
            ui.painter().galley(pos2(rect.left(), y + label_h + 2.0), hg, fade(p.text_muted));
        }
        let control = Rect::from_min_max(pos2(rect.right() - control_w, rect.top()), rect.max);
        let mut child = ui.new_child(UiBuilder::new().max_rect(control).layout(Layout::right_to_left(Align::Center)));
        child.spacing_mut().interact_size = vec2(110.0, 30.0);
        child.spacing_mut().item_spacing = vec2(8.0, 6.0);
        if !self.enabled {
            child.disable();
        }
        add(&mut child);
        let used = child.min_rect().width();
        if measured.is_none_or(|m| (m - used).abs() > 0.5) {
            ui.data_mut(|d| d.insert_temp(id, used));
            ui.ctx().request_discard("settings row width");
        }
    }

    /// Free content across the whole group (lists, sub-forms).
    fn custom(&mut self, add: impl FnOnce(&mut Ui)) {
        self.separator();
        let enabled = self.enabled;
        egui::Frame::new().inner_margin(Margin::symmetric(0, 12)).show(self.ui, |ui| {
            ui.spacing_mut().item_spacing = ui.ctx().global_style().spacing.item_spacing;
            ui.set_width(ui.available_width());
            if !enabled {
                ui.disable();
            }
            add(ui);
        });
    }
}

/// A titled group of settings rows in a bordered card.
fn group(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Rows)) {
    let p = theme::pal(ui);
    ui.label(RichText::new(title).font(theme::semibold(13.0)).color(p.text));
    ui.add_space(2.0);
    kit::card(ui).inner_margin(Margin::symmetric(14, 0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut rows = Rows { ui, n: 0, enabled: true };
        add(&mut rows);
    });
    ui.add_space(18.0);
}

/// A segmented control from owned labels.
pub(crate) fn segmented<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, items: &[(T, String)]) -> bool {
    let items: Vec<(T, &str)> = items.iter().map(|(v, l)| (*v, l.as_str())).collect();
    kit::segmented(ui, current, &items)
}

/// Keyboard chips for `⌘T / ⌘W` style shortcuts, right to left.
fn shortcut_chips(ui: &mut Ui, keys: &str) {
    let p = theme::pal(ui);
    ui.spacing_mut().item_spacing.x = 4.0;
    let parts: Vec<&str> = keys.split(" / ").collect();
    for (i, k) in parts.iter().enumerate().rev() {
        kit::kbd(ui, k);
        if i > 0 {
            ui.label(RichText::new("/").color(p.text_faint));
        }
    }
}

// ---------------------------------------------------------------------------
// Console
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsoleFilter {
    #[default]
    All,
    Logs,
    Network,
    Errors,
}

pub fn console_ui(app: &mut MerpatiApp, ui: &mut Ui) {
    use merpati_runner::ConsoleKind as K;
    let p = theme::pal(ui);
    let search_id = egui::Id::unique("console-search");
    let mut search: String = ui.data(|d| d.get_temp(search_id).unwrap_or_default());
    let problems = app.console.iter().filter(|l| matches!(l.kind, K::Error | K::Warn)).count();
    let wide = ui.available_width() > 760.0;
    let mut copy = false;
    ui.horizontal(|ui| {
        ui.set_height(30.0);
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(icons::TERMINAL_WINDOW).font(theme::icon_font(16.0)).color(p.text_muted));
        ui.label(RichText::new(tr("Console")).font(theme::semibold(13.0)).color(p.text));
        kit::vdivider(ui, 18.0);
        let errors = if problems > 0 { format!("{}  {problems}", tr("Errors")) } else { tr("Errors").to_string() };
        segmented(
            ui,
            &mut app.console_filter,
            &[
                (ConsoleFilter::All, tr("All").to_string()),
                (ConsoleFilter::Logs, tr("Logs").to_string()),
                (ConsoleFilter::Network, tr("Network").to_string()),
                (ConsoleFilter::Errors, errors),
            ],
        );
        trailing(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if kit::icon_button(ui, icons::X, &format!("{} (⌥⌘C)", tr("Close console"))).clicked() {
                app.console_open = false;
            }
            let has_lines = !app.console.is_empty();
            if ui
                .add_enabled_ui(has_lines, |ui| kit::icon_button(ui, icons::TRASH, tr("Clear console")))
                .inner
                .clicked()
            {
                app.console.clear();
            }
            if ui.add_enabled_ui(has_lines, |ui| kit::icon_button(ui, icons::COPY, tr("Copy lines"))).inner.clicked() {
                copy = true;
            }
            if wide {
                ui.add_space(6.0);
                kit::search_field(ui, "console-filter", &mut search, tr("Filter"), 200.0);
            }
        });
    });
    ui.data_mut(|d| d.insert_temp(search_id, search.clone()));
    ui.add_space(4.0);
    let filter = app.console_filter;
    let needle = search.trim().to_lowercase();
    let rows: Vec<&merpati_runner::ConsoleLine> = app
        .console
        .iter()
        .filter(|l| match filter {
            ConsoleFilter::All => true,
            ConsoleFilter::Logs => !matches!(l.kind, K::Network),
            ConsoleFilter::Network => matches!(l.kind, K::Network),
            ConsoleFilter::Errors => matches!(l.kind, K::Error | K::Warn),
        })
        .filter(|l| {
            needle.is_empty() || l.message.to_lowercase().contains(&needle) || l.source.to_lowercase().contains(&needle)
        })
        .collect();
    if copy {
        let text = rows.iter().map(|l| format!("[{}] {}: {}", l.phase, l.source, l.message)).collect::<Vec<_>>();
        ui.ctx().copy_text(text.join("\n"));
    }
    let frame = egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(1));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_height(ui.available_height());
        if rows.is_empty() {
            ui.add_space((ui.available_height() / 2.0 - 34.0).max(4.0));
            if app.console.is_empty() {
                compact_empty(
                    ui,
                    icons::TERMINAL_WINDOW,
                    tr("No console output yet"),
                    tr("console.log() from scripts and every request you send show up here."),
                );
            } else {
                compact_empty(ui, icons::FUNNEL, tr("Nothing matches this filter"), "");
            }
            return;
        }
        let row_h = 24.0;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().id_salt("console").stick_to_bottom(true).auto_shrink([false; 2]).show_rows(
                ui,
                row_h,
                rows.len(),
                |ui, range| {
                    for l in &rows[range] {
                        console_row(ui, l, row_h, wide);
                    }
                },
            );
        });
    });
}

fn console_row(ui: &mut Ui, l: &merpati_runner::ConsoleLine, h: f32, wide: bool) {
    use merpati_runner::ConsoleKind as K;
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if !ui.is_rect_visible(rect) {
        return;
    }
    // Network lines end with `→ 404 Not Found (…)`: the icon takes the status color.
    let status = l
        .message
        .rsplit_once(" → ")
        .and_then(|(_, rest)| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|s| (100..600).contains(s));
    let (icon, color, tinted) = match l.kind {
        K::Error => (icons::X_CIRCLE, p.err, true),
        K::Warn => (icons::WARNING, p.warn, true),
        K::Network => (icons::ARROWS_DOWN_UP, status.map(|s| theme::status_color(s, dark)).unwrap_or(p.info), false),
        K::Info => (icons::INFO, p.text_muted, false),
        K::Debug => (icons::BUG, p.text_faint, false),
        K::Log => (icons::CARET_RIGHT, p.text_faint, false),
    };
    let painter = ui.painter();
    if tinted {
        painter.rect_filled(rect, 0.0, theme::soft(color, if dark { 0.10 } else { 0.06 }));
    } else if resp.hovered() {
        painter.rect_filled(rect, 0.0, p.faint_row());
    }
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::soft(p.border, 0.7)));
    let cy = rect.center().y;
    let mut x = rect.left() + 10.0;
    let time = chrono::DateTime::from_timestamp_millis(l.at_ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_default();
    painter.text(pos2(x, cy), Align2::LEFT_CENTER, time, theme::mono(11.0), p.text_faint);
    x += 66.0;
    painter.text(pos2(x + 7.0, cy), Align2::CENTER_CENTER, icon, theme::icon_font(14.0), color);
    x += 22.0;
    if wide {
        let g = line_galley(ui, &l.source, FontId::proportional(12.0), 140.0);
        ui.painter().galley(pos2(x, cy - g.size().y / 2.0), g, p.text_muted);
        x += 152.0;
    }
    let first = l.message.lines().next().unwrap_or("");
    let multiline = l.message.trim_end().contains('\n');
    let shown = if multiline { format!("{first} …") } else { first.to_string() };
    let g = line_galley(ui, &shown, theme::mono(12.0), rect.right() - x - 10.0);
    let elided = g.elided || multiline;
    let text_color = if tinted { color } else { p.text };
    ui.painter().galley(pos2(x, cy - g.size().y / 2.0), g, text_color);
    let resp = if elided { resp.on_hover_text(RichText::new(&l.message).monospace()) } else { resp };
    resp.context_menu(|ui| {
        if ui.button(kit::with_icon(icons::COPY, tr("Copy message"))).clicked() {
            ui.ctx().copy_text(l.message.clone());
            ui.close();
        }
    });
}

// ---------------------------------------------------------------------------
// Session (open tabs), restored after restart or a crash
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum SavedTab {
    Request {
        location: Option<(Id, Id)>,
        scratch: Option<RequestItem>,
    },
    Websocket {
        location: Option<(Id, Id)>,
        scratch: Option<RequestItem>,
    },
    Container {
        collection: Id,
        folder: Option<Id>,
    },
    Environment {
        env: Option<Id>,
    },
    Spec {
        path: PathBuf,
        /// Edits not saved to the file yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unsaved: Option<String>,
    },
    Flow {
        path: PathBuf,
    },
    /// Any other protocol tab; the tab type follows the request.
    Proto {
        location: Option<(Id, Id)>,
        scratch: Option<RequestItem>,
    },
}

#[derive(Serialize, Deserialize, Default)]
struct Session {
    tabs: Vec<SavedTab>,
    active: usize,
}

fn session_path(app: &MerpatiApp) -> PathBuf {
    app.ws.local_dir().join("session.json")
}

/// Unsaved tabs are written without literal credentials (only `{{…}}`
/// references survive), like collection files.
fn scrub(item: Option<&RequestItem>) -> Option<RequestItem> {
    let mut item = item?.clone();
    let auth = item.request.auth_mut();
    let sensitive = auth.kind.sensitive_params();
    for (k, v) in auth.params.iter_mut() {
        if sensitive.contains(&k.as_str())
            && let serde_json::Value::String(s) = v
            && !merpati_core::variables::is_pure_reference(s)
        {
            s.clear();
        }
    }
    Some(item)
}

/// Writes the open tabs (unless unchanged since the last write).
pub fn save_session(app: &mut MerpatiApp) {
    let saved = app.tabs.iter().map(|t| match &t.kind {
        TabKind::Request(r) => Some(SavedTab::Request {
            location: r.location.as_ref().map(|l| (l.collection.clone(), l.item.clone())),
            scratch: scrub(r.scratch.as_ref()),
        }),
        TabKind::WebSocket(r) => Some(SavedTab::Websocket {
            location: r.location.as_ref().map(|l| (l.collection.clone(), l.item.clone())),
            scratch: scrub(r.scratch.as_ref()),
        }),
        TabKind::Container(c) => {
            Some(SavedTab::Container { collection: c.collection.clone(), folder: c.folder.clone() })
        }
        TabKind::Environment(e) => Some(SavedTab::Environment { env: e.env.clone() }),
        TabKind::Spec(t) => {
            Some(SavedTab::Spec { path: t.path.clone(), unsaved: t.unsaved_text().map(str::to_string) })
        }
        TabKind::Flow(f) => f.path.clone().map(|path| SavedTab::Flow { path }),
        other => other
            .location()
            .map(|l| (l.collection.clone(), l.item.clone()))
            .map(Some)
            .or_else(|| other.scratch().map(|_| None))
            .map(|location| SavedTab::Proto { location, scratch: scrub(other.scratch()) }),
    });
    // `active` indexes the saved tabs: the active tab, or the closest saved
    // one before it when it is not saved itself.
    let mut tabs = Vec::new();
    let mut active = 0;
    for (i, t) in saved.enumerate() {
        if let Some(t) = t {
            if i <= app.active {
                active = tabs.len();
            }
            tabs.push(t);
        }
    }
    let s = Session { tabs, active };
    if let Ok(json) = serde_json::to_vec_pretty(&s)
        && json != app.last_session
        && merpati_core::storage::write_atomic(&session_path(app), &json).is_ok()
    {
        app.last_session = json;
    }
}

pub fn restore_session(app: &mut MerpatiApp) {
    let Ok(bytes) = std::fs::read(session_path(app)) else { return };
    let Ok(s) = serde_json::from_slice::<Session>(&bytes) else { return };
    // Saved tabs that cannot be restored are skipped, so the active one is
    // found by tab id rather than by position.
    let mut active_id = None;
    for (pos, t) in s.tabs.into_iter().enumerate() {
        let before = app.tabs.len();
        match t {
            SavedTab::Request { location: Some((c, i)), .. } => {
                if app.ws.data.collection(&c).and_then(|col| col.find(&i)).is_some() {
                    app.add_tab(TabKind::Request(Box::new(crate::request_tab::RequestTab::saved(ItemRef {
                        collection: c,
                        item: i,
                    }))));
                }
            }
            SavedTab::Request { location: None, scratch: Some(item) } => {
                app.add_tab(TabKind::Request(Box::new(crate::request_tab::RequestTab::scratch(item))));
            }
            SavedTab::Websocket { location: Some((c, i)), .. } => {
                if app.ws.data.collection(&c).and_then(|col| col.find(&i)).is_some() {
                    app.add_tab(TabKind::WebSocket(Box::new(crate::ws_tab::WsTab::saved(ItemRef {
                        collection: c,
                        item: i,
                    }))));
                }
            }
            SavedTab::Websocket { location: None, scratch: Some(item) } => {
                app.add_tab(TabKind::WebSocket(Box::new(crate::ws_tab::WsTab::scratch(item))));
            }
            SavedTab::Container { collection, folder } => {
                if app.ws.data.collection(&collection).is_some() {
                    app.open_container(collection, folder);
                }
            }
            SavedTab::Environment { env }
                if env.as_ref().map(|e| app.ws.data.environment(e).is_some()).unwrap_or(true) =>
            {
                app.open_environment(env);
            }
            SavedTab::Spec { path, unsaved } if path.is_file() => {
                crate::spec_tab::open_spec(app, path.clone());
                if let Some(text) = unsaved
                    && let Some(TabKind::Spec(t)) = app.tabs.get_mut(app.active).map(|t| &mut t.kind)
                    && t.path == path
                {
                    t.restore_unsaved(text);
                }
            }
            SavedTab::Flow { path } if path.is_file() => crate::flow_tab::open_flow(app, path),
            SavedTab::Proto { location, scratch } => {
                let location = location.map(|(collection, item)| ItemRef { collection, item });
                let kind = match (&location, &scratch) {
                    (Some(l), _) => app
                        .ws
                        .data
                        .collection(&l.collection)
                        .and_then(|c| c.find_request(&l.item))
                        .map(|r| r.request.clone()),
                    (None, Some(item)) => Some(item.request.clone()),
                    (None, None) => None,
                };
                if let Some(tab) = kind.and_then(|k| TabKind::for_item(location, scratch, &k)) {
                    app.add_tab(tab);
                }
            }
            _ => {}
        }
        if pos <= s.active && app.tabs.len() > before {
            active_id = app.tabs.get(app.active).map(|t| t.id);
        }
    }
    app.active = active_id.and_then(|id| app.tab_index(id)).unwrap_or(0).min(app.tabs.len().saturating_sub(1));
}

// ---------------------------------------------------------------------------
// Export and code generation helpers
// ---------------------------------------------------------------------------

pub fn export_collection(app: &mut MerpatiApp, cid: &str, format: crate::sidebar::ExportFormat) {
    let Some(c) = app.ws.data.collection(cid).cloned() else { return };
    let slug = merpati_core::util::slugify(&c.name);
    let (value, name, note) = match format {
        crate::sidebar::ExportFormat::Postman => {
            let (v, report) = merpati_core::postman::export_collection(&c, ExportOptions::default());
            let mut note = String::new();
            if report.redacted > 0 {
                note = format!("{} secret values were left out of the export.", report.redacted);
            }
            for w in report.warnings {
                note.push_str(&format!(" {w}"));
            }
            (v, format!("{slug}.postman_collection.json"), note)
        }
        crate::sidebar::ExportFormat::OpenApi => {
            (merpati_convert::openapi::export(&c), format!("{slug}.openapi.json"), String::new())
        }
    };
    if let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file() {
        match std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap_or_default()) {
            Ok(()) => app.toast(trf("Exported to {}. {}", &[&path.display(), &note]), false),
            Err(e) => app.toast(trf("Export failed: {}", &[&e]), true),
        }
    }
}

fn docs_options(app: &MerpatiApp, c: &merpati_core::model::Collection) -> merpati_convert::docs::DocsOptions {
    let mut variables = app.ws.scopes_for(app.ws.data.collection(&c.id).map(|x| x.id.as_str()));
    // Secret values never go into documentation.
    let mut secret_vars: Vec<merpati_core::variables::Variable> = app.ws.data.globals.clone();
    if let Some(env) = app.ws.data.active_environment() {
        secret_vars.extend(env.variables.iter().cloned());
    }
    secret_vars.extend(c.variables.iter().cloned());
    merpati_convert::docs::redact_secret_variables(&mut variables, &secret_vars);
    merpati_convert::docs::DocsOptions {
        variables,
        snippet_languages: vec![Language::Curl, Language::Python, Language::JavaScriptFetch],
        title: Some(c.name.clone()),
    }
}

/// Generates static HTML documentation for a collection and opens it.
pub fn preview_docs(app: &mut MerpatiApp, c: &merpati_core::model::Collection) {
    let opts = docs_options(app, c);
    let dir = std::env::temp_dir().join(format!("merpati-docs-{}", merpati_core::util::slugify(&c.name)));
    match merpati_convert::docs::write_html_site(c, &opts, &dir) {
        Ok(index) => {
            let _ = open::that(index);
        }
        Err(e) => app.toast(trf("Docs: {}", &[&e]), true),
    }
}

/// Writes documentation (HTML site or Markdown) to a chosen location.
pub fn export_docs(app: &mut MerpatiApp, cid: &str, markdown: bool) {
    let Some(c) = app.ws.data.collection(cid).cloned() else { return };
    let opts = docs_options(app, &c);
    if markdown {
        let name = format!("{}.md", merpati_core::util::slugify(&c.name));
        if let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file() {
            match std::fs::write(&path, merpati_convert::docs::to_markdown(&c, &opts)) {
                Ok(()) => app.toast(trf("Documentation written to {}", &[&path.display()]), false),
                Err(e) => app.toast(trf("Docs: {}", &[&e]), true),
            }
        }
    } else if let Some(dir) = rfd::FileDialog::new().set_title("Folder for the documentation site").pick_folder() {
        match merpati_convert::docs::write_html_site(&c, &opts, &dir) {
            Ok(index) => {
                app.toast(trf("Documentation written to {}", &[&index.display()]), false);
                let _ = open::that(index);
            }
            Err(e) => app.toast(trf("Docs: {}", &[&e]), true),
        }
    }
}

pub fn export_environment(app: &mut MerpatiApp, id: &str) {
    let Some(e) = app.ws.data.environment(id).cloned() else { return };
    let (v, report) = merpati_core::postman::export_environment(&e, ExportOptions::default());
    let name = format!("{}.postman_environment.json", merpati_core::util::slugify(&e.name));
    if let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file() {
        match std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default()) {
            Ok(()) => app.toast(
                if report.redacted > 0 {
                    trf("Exported; {} secret values left out", &[&report.redacted])
                } else {
                    tr("Exported").to_string()
                },
                false,
            ),
            Err(e) => app.toast(trf("Export failed: {}", &[&e]), true),
        }
    }
}

/// Generates a snippet for a request item with current variables applied.
pub fn codegen_item(app: &MerpatiApp, item: &RequestItem, collection: Option<&str>, lang: Language) -> String {
    let Some(http) = item.http() else { return String::new() };
    let scopes = {
        let mut s = app.ws.scopes_for(collection);
        if let Some(c) = collection.and_then(|c| app.ws.data.collection(c))
            && let Some(chain) = c.ancestors(&item.id)
        {
            for a in chain {
                if let merpati_core::model::Ancestor::Folder(f) = a {
                    // Folder variables override the collection's, types included.
                    let values = merpati_core::variables::to_map(&f.variables);
                    s.types.collection.retain(|k, _| !values.contains_key(k));
                    s.types.collection.extend(merpati_core::variables::to_types(&f.variables));
                    s.collection.extend(values);
                }
            }
        }
        s
    };
    let auth = if http.auth.is_inherit() {
        collection
            .and_then(|c| app.ws.data.collection(c))
            .and_then(|c| c.effective_auth(&item.id).map(|e| e.auth.clone()))
            .unwrap_or_else(merpati_core::auth::Auth::none)
    } else {
        http.auth.clone()
    };
    let vault = app.ws.vault();
    let resolver = Resolver::new(&scopes).with_vault(vault.as_ref());
    let ctx = PrepareContext {
        resolver,
        auth: &auth,
        auth_owner_id: Some(&item.id),
        defaults: &app.settings.request,
        base_dir: Some(app.ws.root()),
    };
    let mut prepared = prepare(http, &ctx);
    if let Some(p) = merpati_auth::preview(&prepared) {
        prepared = p.request;
    }
    merpati_convert::codegen::generate_with(
        lang,
        &prepared,
        merpati_convert::codegen::Options { skip_default_headers: true },
    )
}

pub fn codegen_for(app: &MerpatiApp, cid: &str, rid: &str, lang: Language) -> Option<String> {
    let item = app.ws.data.collection(cid)?.find_request(rid)?.clone();
    Some(codegen_item(app, &item, Some(cid), lang))
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

pub enum Prompt {
    NewCollection,
    NewEnvironment,
    RenameEnvironment(Id),
    /// Fork this collection.
    Fork(Id),
}

pub enum DeleteTarget {
    Item { collection: Id, item: Option<Id> },
    Environment(Id),
}

/// Pages of the Settings dialog.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub enum SettingsPage {
    #[default]
    General,
    Requests,
    Scripts,
    Proxy,
    Certificates,
    Shortcuts,
    About,
}

impl SettingsPage {
    const ALL: [SettingsPage; 7] = [
        SettingsPage::General,
        SettingsPage::Requests,
        SettingsPage::Scripts,
        SettingsPage::Proxy,
        SettingsPage::Certificates,
        SettingsPage::Shortcuts,
        SettingsPage::About,
    ];

    fn icon(self) -> &'static str {
        match self {
            SettingsPage::General => icons::SLIDERS_HORIZONTAL,
            SettingsPage::Requests => icons::PAPER_PLANE_RIGHT,
            SettingsPage::Scripts => icons::CODE,
            SettingsPage::Proxy => icons::GLOBE_SIMPLE,
            SettingsPage::Certificates => icons::SHIELD_CHECK,
            SettingsPage::Shortcuts => icons::KEYBOARD,
            SettingsPage::About => icons::INFO,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SettingsPage::General => tr("General"),
            SettingsPage::Requests => tr("Requests"),
            SettingsPage::Scripts => tr("Scripts"),
            SettingsPage::Proxy => tr("Proxy"),
            SettingsPage::Certificates => tr("Certificates"),
            SettingsPage::Shortcuts => tr("Shortcuts"),
            SettingsPage::About => tr("About"),
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            SettingsPage::General => tr("Appearance, language, updates and history."),
            SettingsPage::Requests => tr("Defaults for every request you send."),
            SettingsPage::Scripts => tr("Limits for scripts and the JavaScript packages they can load."),
            SettingsPage::Proxy => tr("Send requests through a proxy server."),
            SettingsPage::Certificates => tr("Trust extra certificate authorities and send client certificates."),
            SettingsPage::Shortcuts => tr("Keys for the things you do most."),
            SettingsPage::About => "",
        }
    }
}

pub enum Dialog {
    Palette {
        query: String,
        requests_only: bool,
        selected: usize,
    },
    EnvSwitcher {
        query: String,
        /// Highlighted row; starts on the active environment.
        selected: Option<usize>,
    },
    QuickLook,
    Settings {
        draft: Box<AppSettings>,
        section: SettingsPage,
    },
    Vault(Box<crate::vault_ui::VaultDialog>),
    Cookies {
        domain: Option<String>,
        new_cookie: String,
        error: Option<String>,
        new_domain: String,
    },
    Import {
        text: String,
        error: Option<String>,
        preview: Option<merpati_convert::Imported>,
        source: String,
    },
    Codegen {
        item: Box<RequestItem>,
        collection: Option<Id>,
        lang: Language,
        code: String,
    },
    SaveAs {
        tab: u64,
        collection: Option<Id>,
        folder: Option<Id>,
        name: String,
        /// `name` was filled in from the tab (once: the user may empty it).
        named: bool,
        new_collection: String,
    },
    NewWorkspace {
        name: String,
        path: Option<PathBuf>,
    },
    Prompt {
        kind: Prompt,
        text: String,
    },
    Confirm {
        target: DeleteTarget,
        name: String,
    },
    ForkSync(Box<crate::fork_ui::ForkSync>),
    /// Closing a tab with unsaved work: save, discard or keep it open.
    CloseUnsaved {
        tab: u64,
    },
}

impl Dialog {
    pub fn palette(requests_only: bool) -> Dialog {
        Dialog::Palette { query: String::new(), requests_only, selected: 0 }
    }
    pub fn env_switcher() -> Dialog {
        Dialog::EnvSwitcher { query: String::new(), selected: None }
    }
    pub fn quick_look() -> Dialog {
        Dialog::QuickLook
    }
    pub fn settings(s: &AppSettings) -> Dialog {
        Dialog::Settings { draft: Box::new(s.clone()), section: SettingsPage::default() }
    }
    pub fn vault() -> Dialog {
        Dialog::Vault(Box::default())
    }
    pub fn cookies() -> Dialog {
        Dialog::Cookies { domain: None, new_cookie: String::new(), error: None, new_domain: String::new() }
    }
    pub fn import() -> Dialog {
        Dialog::Import { text: String::new(), error: None, preview: None, source: String::new() }
    }
    pub fn codegen(app: &MerpatiApp, item: &RequestItem, collection: Option<&str>) -> Dialog {
        let lang = Language::Curl;
        let code = codegen_item(app, item, collection, lang);
        Dialog::Codegen { item: Box::new(item.clone()), collection: collection.map(str::to_string), lang, code }
    }
    /// The name is taken from the tab when the dialog is first shown (the tab
    /// may be out of `app.tabs` while it asks for the dialog).
    pub fn save_as(tab: u64, ws: &WsState) -> Dialog {
        Dialog::SaveAs {
            tab,
            collection: ws.data.collections.first().map(|c| c.id.clone()),
            folder: None,
            name: String::new(),
            named: false,
            new_collection: String::new(),
        }
    }
    pub fn new_workspace() -> Dialog {
        Dialog::NewWorkspace { name: "My Workspace".into(), path: None }
    }
    pub fn prompt_new_collection() -> Dialog {
        Dialog::Prompt { kind: Prompt::NewCollection, text: "New Collection".into() }
    }
    pub fn prompt_new_environment() -> Dialog {
        Dialog::Prompt { kind: Prompt::NewEnvironment, text: "New Environment".into() }
    }
    pub fn rename_env(id: Id, name: String) -> Dialog {
        Dialog::Prompt { kind: Prompt::RenameEnvironment(id), text: name }
    }
    pub fn confirm_delete(collection: Id, item: Option<Id>, name: String) -> Dialog {
        Dialog::Confirm { target: DeleteTarget::Item { collection, item }, name }
    }
    pub fn confirm_delete_env(id: Id, name: String) -> Dialog {
        Dialog::Confirm { target: DeleteTarget::Environment(id), name }
    }
}

pub fn show(app: &mut MerpatiApp, ctx: &egui::Context) {
    let Some(mut dialog) = app.dialog.take() else { return };
    let outcome = match &mut dialog {
        Dialog::Palette { query, requests_only, selected } => palette(app, ctx, query, *requests_only, selected),
        Dialog::EnvSwitcher { query, selected } => env_switcher(app, ctx, query, selected),
        Dialog::QuickLook => quick_look(app, ctx),
        Dialog::Settings { draft, section } => settings(app, ctx, draft, section),
        Dialog::Vault(d) => crate::vault_ui::show(app, ctx, d),
        Dialog::Cookies { domain, new_cookie, error, new_domain } => {
            cookies(app, ctx, domain, new_cookie, error, new_domain)
        }
        Dialog::Import { text, error, preview, source } => import(app, ctx, text, error, preview, source),
        Dialog::Codegen { item, collection, lang, code } => codegen(app, ctx, item, collection.as_deref(), lang, code),
        Dialog::SaveAs { tab, collection, folder, name, named, new_collection } => {
            save_as(app, ctx, *tab, collection, folder, name, named, new_collection)
        }
        Dialog::NewWorkspace { name, path } => new_workspace(app, ctx, name, path),
        Dialog::Prompt { kind, text } => prompt(app, ctx, kind, text),
        Dialog::Confirm { target, name } => confirm(app, ctx, target, name),
        Dialog::ForkSync(s) => crate::fork_ui::show(app, ctx, s),
        Dialog::CloseUnsaved { tab } => close_unsaved(app, ctx, *tab),
    };
    if matches!(outcome, Outcome::Keep) && app.dialog.is_none() {
        app.dialog = Some(dialog);
    }
}

// ---------------------------------------------------------------------------
// Command palette, request search and environment switcher
// ---------------------------------------------------------------------------

/// Result groups, in the order they are listed when nothing is typed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Group {
    Create,
    Actions,
    Requests,
    Environments,
    Runner,
    Preferences,
}

impl Group {
    fn label(self) -> &'static str {
        match self {
            Group::Create => tr("Create"),
            Group::Actions => tr("Actions"),
            Group::Requests => tr("Requests"),
            Group::Environments => tr("Environments"),
            Group::Runner => tr("Run collection"),
            Group::Preferences => tr("Preferences"),
        }
    }
}

/// What a result row shows in front of its name.
enum Lead {
    Method(String),
    Icon(&'static str),
}

struct PaletteEntry {
    group: Group,
    lead: Lead,
    label: String,
    /// Muted text after the name (folder path, variable count).
    detail: String,
    /// Matched but not shown (URL, synonyms).
    extra: String,
    keys: &'static str,
    /// Shown with a check mark (active environment, current theme).
    current: bool,
    action: PaletteAction,
}

impl PaletteEntry {
    fn new(group: Group, icon: &'static str, label: impl Into<String>, action: PaletteAction) -> PaletteEntry {
        PaletteEntry {
            group,
            lead: Lead::Icon(icon),
            label: label.into(),
            detail: String::new(),
            extra: String::new(),
            keys: "",
            current: false,
            action,
        }
    }

    fn keys(mut self, keys: &'static str) -> PaletteEntry {
        self.keys = keys;
        self
    }

    fn detail(mut self, detail: impl Into<String>) -> PaletteEntry {
        self.detail = detail.into();
        self
    }

    fn extra(mut self, extra: impl Into<String>) -> PaletteEntry {
        self.extra = extra.into();
        self
    }

    fn current(mut self, current: bool) -> PaletteEntry {
        self.current = current;
        self
    }
}

enum PaletteAction {
    Open(ItemRef),
    NewRequest,
    NewWs,
    NewProto(usize),
    NewWebhook,
    NewCapture,
    NewFlow,
    NewCollection,
    NewEnvironment,
    Import,
    Settings,
    ToggleSidebar,
    ToggleConsole,
    Cookies,
    Vault,
    QuickLook,
    SwitchEnvironment,
    Env(Option<Id>),
    Run(Id),
    Theme(ThemePreference),
    Language(UiLanguage),
    OpenWorkspace,
    NewWorkspace,
    CheckUpdates,
}

fn fuzzy(hay: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let h = hay.to_lowercase();
    let n = needle.to_lowercase();
    if let Some(p) = h.find(&n) {
        return Some(1000 - p as i32);
    }
    let mut it = h.chars();
    let mut score = 0;
    for c in n.chars() {
        loop {
            match it.next() {
                Some(x) if x == c => {
                    score += 1;
                    break;
                }
                Some(_) => score -= 1,
                None => return None,
            }
        }
    }
    Some(score)
}

/// Which characters of `text` match `query` (the first substring match, or
/// the characters of a subsequence match), for highlighting.
fn match_marks(text: &str, query: &str) -> Vec<bool> {
    let lower: Vec<char> = text.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect();
    let mut marks = vec![false; lower.len()];
    let q: Vec<char> = query.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect();
    if q.is_empty() || q.len() > lower.len() {
        return marks;
    }
    if let Some(start) = (0..=lower.len() - q.len()).find(|&i| lower[i..i + q.len()] == q[..]) {
        marks[start..start + q.len()].iter_mut().for_each(|m| *m = true);
        return marks;
    }
    let mut qi = 0;
    for (i, c) in lower.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            marks[i] = true;
            qi += 1;
        }
    }
    if qi < q.len() {
        marks.iter_mut().for_each(|m| *m = false);
    }
    marks
}

fn score(e: &PaletteEntry, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let name = fuzzy(&e.label, query).map(|s| s + 2000);
    let rest = fuzzy(&format!("{} {}", e.detail, e.extra), query);
    // Requests are what people search for most: they win ties.
    let bias = if e.group == Group::Requests { 8 } else { 0 };
    name.max(rest).map(|s| s + bias)
}

/// Matching entries in display order: groups by their best match (then by
/// their usual order), rows by score.
fn rank(entries: &[PaletteEntry], query: &str) -> Vec<usize> {
    let mut scored: Vec<(i32, usize)> =
        entries.iter().enumerate().filter_map(|(i, e)| score(e, query).map(|s| (s, i))).collect();
    let mut best: std::collections::HashMap<Group, i32> = std::collections::HashMap::new();
    for (s, i) in &scored {
        let b = best.entry(entries[*i].group).or_insert(i32::MIN);
        *b = (*b).max(*s);
    }
    scored.sort_by(|a, b| {
        let (ga, gb) = (entries[a.1].group, entries[b.1].group);
        best[&gb].cmp(&best[&ga]).then(ga.cmp(&gb)).then(b.0.cmp(&a.0)).then(a.1.cmp(&b.1))
    });
    scored.truncate(120);
    scored.into_iter().map(|(_, i)| i).collect()
}

fn request_entries(app: &MerpatiApp, out: &mut Vec<PaletteEntry>) {
    fn walk(c: &merpati_core::model::Collection, items: &[Item], path: &str, out: &mut Vec<PaletteEntry>) {
        for i in items {
            match i {
                Item::Folder(f) => walk(c, &f.items, &format!("{path} › {}", f.name), out),
                Item::Request(r) => out.push(PaletteEntry {
                    group: Group::Requests,
                    lead: Lead::Method(r.request.badge()),
                    label: r.name.clone(),
                    detail: path.to_string(),
                    extra: r.request.url().to_string(),
                    keys: "",
                    current: false,
                    action: PaletteAction::Open(ItemRef { collection: c.id.clone(), item: r.id.clone() }),
                }),
            }
        }
    }
    for c in &app.ws.data.collections {
        walk(c, &c.items, &c.name, out);
    }
}

fn environment_entries(app: &MerpatiApp, out: &mut Vec<PaletteEntry>) {
    let active = app.ws.data.active_environment.as_deref();
    out.push(
        PaletteEntry::new(Group::Environments, icons::X_CIRCLE, tr("No environment"), PaletteAction::Env(None))
            .extra("environment")
            .current(active.is_none()),
    );
    for e in &app.ws.data.environments {
        let n = e.variables.iter().filter(|v| v.enabled).count();
        let vars = if n == 1 { tr("1 variable").to_string() } else { trf("{} variables", &[&n]) };
        out.push(
            PaletteEntry::new(
                Group::Environments,
                icons::GLOBE,
                e.name.clone(),
                PaletteAction::Env(Some(e.id.clone())),
            )
            .detail(vars)
            .extra("environment")
            .current(active == Some(e.id.as_str())),
        );
    }
}

fn palette_entries(app: &MerpatiApp, requests_only: bool) -> Vec<PaletteEntry> {
    let mut out = Vec::new();
    if !requests_only {
        use PaletteAction as A;
        let c = Group::Create;
        out.push(PaletteEntry::new(c, icons::GLOBE, tr("New Request"), A::NewRequest).keys("⌘N").extra("http"));
        out.push(PaletteEntry::new(c, icons::PLUGS_CONNECTED, tr("New WebSocket"), A::NewWs));
        for (i, (label, name, _)) in crate::app::protocol_templates().into_iter().enumerate() {
            out.push(PaletteEntry::new(c, crate::app::protocol_icon(name), tr(label), A::NewProto(i)));
        }
        out.push(PaletteEntry::new(c, icons::WEBHOOKS_LOGO, tr("New webhook listener"), A::NewWebhook));
        out.push(PaletteEntry::new(c, icons::BROADCAST, tr("Capture traffic (proxy)"), A::NewCapture));
        out.push(PaletteEntry::new(c, icons::FLOW_ARROW, tr("New flow"), A::NewFlow));
        out.push(PaletteEntry::new(c, icons::STACK, tr("New Collection"), A::NewCollection));
        out.push(PaletteEntry::new(c, icons::GLOBE_SIMPLE, tr("New Environment"), A::NewEnvironment));
        out.push(PaletteEntry::new(c, icons::DOWNLOAD_SIMPLE, tr("Import"), A::Import).extra("postman openapi curl"));
        let a = Group::Actions;
        out.push(PaletteEntry::new(a, icons::GLOBE, tr("Switch environment"), A::SwitchEnvironment).keys("⌘E"));
        out.push(PaletteEntry::new(a, icons::EYE, tr("Quick look"), A::QuickLook).extra("variables"));
        out.push(PaletteEntry::new(a, icons::SIDEBAR_SIMPLE, tr("Toggle sidebar"), A::ToggleSidebar).keys("⌘B"));
        out.push(PaletteEntry::new(a, icons::TERMINAL_WINDOW, tr("Toggle console"), A::ToggleConsole).keys("⌥⌘C"));
        out.push(PaletteEntry::new(a, icons::COOKIE, tr("Manage cookies"), A::Cookies));
        out.push(PaletteEntry::new(a, icons::LOCK_KEY, tr("Manage vault"), A::Vault).extra("secrets"));
        out.push(PaletteEntry::new(a, icons::FOLDER_OPEN, tr("Open workspace…"), A::OpenWorkspace));
        out.push(PaletteEntry::new(a, icons::FOLDER_PLUS, tr("New workspace…"), A::NewWorkspace));
        out.push(PaletteEntry::new(a, icons::GEAR_SIX, tr("Settings"), A::Settings).keys("⌘,").extra("preferences"));
        out.push(PaletteEntry::new(a, icons::ARROWS_CLOCKWISE, tr("Check for updates…"), A::CheckUpdates));
        let mut envs = Vec::new();
        environment_entries(app, &mut envs);
        out.extend(envs);
        for col in &app.ws.data.collections {
            out.push(
                PaletteEntry::new(Group::Runner, icons::PLAY_CIRCLE, col.name.clone(), A::Run(col.id.clone()))
                    .detail(tr("Collection runner"))
                    .extra("run runner"),
            );
        }
        let p = Group::Preferences;
        let theme = app.settings.ui.theme;
        for (t, icon, name) in [
            (ThemePreference::System, icons::CIRCLE_HALF, tr("System")),
            (ThemePreference::Light, icons::SUN, tr("Light")),
            (ThemePreference::Dark, icons::MOON, tr("Dark")),
        ] {
            out.push(
                PaletteEntry::new(p, icon, trf("Theme: {}", &[&name]), A::Theme(t)).extra("theme").current(theme == t),
            );
        }
        let lang = app.settings.ui.language;
        for (l, name) in [(UiLanguage::En, "English"), (UiLanguage::Id, "Bahasa Indonesia")] {
            out.push(
                PaletteEntry::new(p, icons::TRANSLATE, trf("Language: {}", &[&name]), A::Language(l))
                    .extra("language bahasa")
                    .current(lang == l),
            );
        }
    }
    request_entries(app, &mut out);
    out
}

/// Consumes ↑, ↓ and ↩ before the search field sees them.
fn nav_keys(ui: &mut Ui) -> (bool, bool, bool) {
    ui.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::ArrowUp),
            i.consume_key(Modifiers::NONE, Key::ArrowDown),
            i.consume_key(Modifiers::NONE, Key::Enter),
        )
    })
}

/// The large search field at the top of the palette and switcher.
fn search_header(ui: &mut Ui, id: &str, query: &mut String, hint: &str) -> Response {
    let p = theme::pal(ui);
    let r = egui::Frame::new()
        .inner_margin(Margin { left: 18, right: 14, top: 10, bottom: 10 })
        .show(ui, |ui| {
            let w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(w, 36.0), Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.label(RichText::new(icons::MAGNIFYING_GLASS).font(theme::icon_font(19.0)).color(p.text_muted));
                let edit = egui::TextEdit::singleline(query)
                    .id(egui::Id::unique(id))
                    .hint_text(RichText::new(hint).color(p.text_faint))
                    .font(FontId::proportional(16.5))
                    .frame(egui::Frame::NONE)
                    .margin(Margin::ZERO)
                    .min_size(vec2(0.0, 36.0))
                    .align(Align2::LEFT_CENTER)
                    .desired_width(ui.available_width() - 48.0);
                let r = ui.add(edit);
                trailing(ui, |ui| kit::kbd(ui, "esc"));
                r
            })
            .inner
        })
        .inner;
    hairline(ui);
    r.request_focus();
    widgets::a11y_name(&r, hint);
    r
}

/// The `⌘N`-style chip painted at the right edge of a row; returns its width.
fn paint_kbd(ui: &Ui, right: f32, cy: f32, keys: &str) -> f32 {
    let p = theme::pal(ui);
    let g = ui.painter().layout_no_wrap(keys.to_string(), FontId::proportional(11.5), p.text_muted);
    let rect = Rect::from_min_max(pos2(right - g.size().x - 12.0, cy - 10.0), pos2(right, cy + 10.0));
    ui.painter().rect(rect, CornerRadius::same(4), p.subtle, Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, p.text_muted);
    rect.width()
}

fn palette_row(ui: &mut Ui, e: &PaletteEntry, query: &str, selected: bool) -> Response {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    widgets::a11y_button(&resp, &e.label);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let radius = CornerRadius::same(7);
    if selected {
        ui.painter().rect_filled(rect, radius, p.accent_soft);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, radius, p.hover);
    }
    let cy = rect.center().y;
    let mut x = rect.left() + 12.0;
    match &e.lead {
        Lead::Method(m) => {
            let short = match m.as_str() {
                "DELETE" => "DEL",
                "OPTIONS" => "OPT",
                other => other,
            };
            let color = theme::method_color(m, dark);
            ui.painter().text(pos2(x, cy), Align2::LEFT_CENTER, short, theme::semibold(10.5), color);
            x += 46.0;
        }
        Lead::Icon(icon) => {
            let color = if selected { p.accent_text } else { p.text_muted };
            ui.painter().text(pos2(x + 8.0, cy), Align2::CENTER_CENTER, *icon, theme::icon_font(16.0), color);
            x += 30.0;
        }
    }
    let mut right = rect.right() - 12.0;
    if e.current {
        ui.painter().text(pos2(right - 8.0, cy), Align2::CENTER_CENTER, icons::CHECK, theme::icon_font(15.0), p.accent);
        right -= 26.0;
    }
    if !e.keys.is_empty() {
        right -= paint_kbd(ui, right, cy, e.keys) + 10.0;
    } else if selected && !e.current {
        right -= paint_kbd(ui, right, cy, "↩") + 10.0;
    }
    // Name with the matched characters in the accent color, then the muted
    // detail when there is room.
    let marks = match_marks(&e.label, query);
    let mut job = LayoutJob::default();
    let plain = TextFormat::simple(theme::medium(13.5), p.text);
    let hit = TextFormat::simple(theme::semibold(13.5), p.accent_text);
    let mut run = String::new();
    let mut run_hit = false;
    for (c, m) in e.label.chars().zip(marks.iter().copied()) {
        if m != run_hit && !run.is_empty() {
            job.append(&std::mem::take(&mut run), 0.0, if run_hit { hit.clone() } else { plain.clone() });
        }
        run_hit = m;
        run.push(c);
    }
    if !run.is_empty() {
        job.append(&run, 0.0, if run_hit { hit } else { plain });
    }
    let name_max = (right - x - 8.0).max(40.0);
    job.wrap = TextWrapping { max_width: name_max, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let name = ui.painter().layout_job(job);
    let name_w = name.size().x;
    ui.painter().galley(pos2(x, cy - name.size().y / 2.0), name, p.text);
    x += name_w + 10.0;
    if !e.detail.is_empty() && right - x > 48.0 {
        let g = line_galley(ui, &e.detail, FontId::proportional(12.5), right - x - 8.0);
        ui.painter().galley(pos2(x, cy - g.size().y / 2.0 + 0.5), g, p.text_muted);
    }
    resp
}

/// The grouped result list. Returns the entry that was clicked.
fn result_list(
    ui: &mut Ui,
    entries: &[PaletteEntry],
    order: &[usize],
    query: &str,
    selected: &mut usize,
    scroll: bool,
    max_h: f32,
) -> Option<usize> {
    let p = theme::pal(ui);
    let mut chosen = None;
    let first_group = order.first().map(|i| entries[*i].group);
    let headers = order.iter().any(|i| Some(entries[*i].group) != first_group);
    let moved = ui.input(|i| i.pointer.delta() != Vec2::ZERO);
    egui::ScrollArea::vertical().id_salt("results").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
        egui::Frame::new().inner_margin(Margin::symmetric(8, 6)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut last = None;
            for (row, &i) in order.iter().enumerate() {
                let e = &entries[i];
                if headers && last != Some(e.group) {
                    ui.add_space(if last.is_some() { 10.0 } else { 4.0 });
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(RichText::new(e.group.label()).font(theme::medium(11.5)).color(p.text_faint));
                    });
                    ui.add_space(4.0);
                    last = Some(e.group);
                }
                let r = palette_row(ui, e, query, row == *selected);
                if row == *selected && scroll {
                    r.scroll_to_me(None);
                }
                if r.hovered() && moved {
                    *selected = row;
                }
                if r.clicked() {
                    chosen = Some(i);
                }
            }
        });
    });
    chosen
}

/// "Nothing found" under a search field.
fn no_results(ui: &mut Ui, title: &str, text: &str) {
    let p = theme::pal(ui);
    ui.vertical_centered(|ui| {
        ui.add_space(34.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(46.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 23.0, p.subtle);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            icons::MAGNIFYING_GLASS,
            theme::icon_font(21.0),
            p.text_muted,
        );
        ui.add_space(12.0);
        ui.label(RichText::new(title).font(theme::semibold(14.5)).color(p.text));
        ui.add_space(4.0);
        ui.label(RichText::new(text).color(p.text_muted));
        ui.add_space(34.0);
    });
}

/// Moves the highlighted row with ↑/↓ (wrapping around).
fn step(selected: &mut usize, len: usize, up: bool, down: bool) {
    if len == 0 {
        *selected = 0;
        return;
    }
    if down {
        *selected = (*selected + 1) % len;
    }
    if up {
        *selected = (*selected + len - 1) % len;
    }
    *selected = (*selected).min(len - 1);
}

fn palette(
    app: &mut MerpatiApp,
    ctx: &egui::Context,
    query: &mut String,
    requests_only: bool,
    selected: &mut usize,
) -> Outcome {
    let mut entries = palette_entries(app, requests_only);
    let q = query.trim().to_string();
    let order = rank(&entries, &q);
    let no_requests = !entries.iter().any(|e| e.group == Group::Requests);
    let max_h = fit_height(ctx, 260.0, 420.0);
    let mut chosen = None;
    let outcome = Modal::new("palette", 660.0).top().show(ctx, |ui| {
        let (up, down, enter) = nav_keys(ui);
        let hint = if requests_only { tr("Search requests…") } else { tr("Search requests and commands…") };
        if search_header(ui, "palette-search", query, hint).changed() {
            *selected = 0;
        }
        step(selected, order.len(), up, down);
        if order.is_empty() {
            if q.is_empty() && no_requests {
                no_results(ui, tr("No requests yet"), tr("Create one with ⌘N or import a collection."));
            } else {
                no_results(ui, &trf("No results for “{}”", &[&q]), tr("Check the spelling or try another word."));
            }
        } else {
            chosen = result_list(ui, &entries, &order, &q, selected, up || down, max_h);
        }
        if enter {
            chosen = order.get(*selected).copied();
        }
        footer(ui, |ui| {
            key_hints(ui, &[("↑↓", tr("Navigate")), ("↩", tr("Open")), ("esc", tr("Close"))]);
            trailing(ui, |ui| {
                let n = order.len();
                let text = if n == 1 { tr("1 result").to_string() } else { trf("{} results", &[&n]) };
                ui.label(RichText::new(text).size(12.0).color(theme::pal(ui).text_faint));
            });
        });
        if chosen.is_some() { Outcome::Close } else { Outcome::Keep }
    });
    if let Some(i) = chosen {
        let action = entries.swap_remove(i).action;
        run_palette_action(app, ctx, action);
    }
    outcome
}

fn run_palette_action(app: &mut MerpatiApp, ctx: &egui::Context, action: PaletteAction) {
    match action {
        PaletteAction::Open(r) => app.open_item(r),
        PaletteAction::NewRequest => app.new_request_tab(),
        PaletteAction::NewWs => app.new_ws_tab(),
        PaletteAction::NewProto(i) => {
            if let Some((_, name, kind)) = crate::app::protocol_templates().into_iter().nth(i) {
                app.new_proto_tab(name, kind);
            }
        }
        PaletteAction::NewCapture => {
            app.add_tab(TabKind::Capture(Box::default()));
        }
        PaletteAction::NewFlow => {
            app.add_tab(TabKind::Flow(Box::new(crate::flow_tab::FlowTab::blank(tr("Flow")))));
        }
        PaletteAction::NewWebhook => {
            app.add_tab(TabKind::Webhook(Box::default()));
        }
        PaletteAction::NewCollection => app.dialog = Some(Dialog::prompt_new_collection()),
        PaletteAction::NewEnvironment => app.dialog = Some(Dialog::prompt_new_environment()),
        PaletteAction::Import => app.dialog = Some(Dialog::import()),
        PaletteAction::Settings => app.dialog = Some(Dialog::settings(&app.settings)),
        PaletteAction::ToggleSidebar => app.settings.ui.sidebar_visible = !app.settings.ui.sidebar_visible,
        PaletteAction::ToggleConsole => app.console_open = !app.console_open,
        PaletteAction::Cookies => app.dialog = Some(Dialog::cookies()),
        PaletteAction::Vault => app.dialog = Some(Dialog::vault()),
        PaletteAction::QuickLook => app.dialog = Some(Dialog::quick_look()),
        PaletteAction::SwitchEnvironment => app.dialog = Some(Dialog::env_switcher()),
        PaletteAction::Env(e) => app.ws.set_active_environment(e),
        PaletteAction::Run(c) => app.open_runner(c, None),
        PaletteAction::OpenWorkspace => {
            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                app.open_workspace(dir, None);
            }
        }
        PaletteAction::NewWorkspace => app.dialog = Some(Dialog::new_workspace()),
        PaletteAction::CheckUpdates => crate::update::request_check(ctx),
        PaletteAction::Theme(t) => {
            let mut s = app.settings.clone();
            s.ui.theme = t;
            app.apply_settings(s);
        }
        PaletteAction::Language(l) => {
            let mut s = app.settings.clone();
            s.ui.language = l;
            app.apply_settings(s);
        }
    }
}

fn env_switcher(
    app: &mut MerpatiApp,
    ctx: &egui::Context,
    query: &mut String,
    selected: &mut Option<usize>,
) -> Outcome {
    let mut entries = Vec::new();
    environment_entries(app, &mut entries);
    let q = query.trim().to_string();
    let order = rank(&entries, &q);
    let mut sel = selected.unwrap_or_else(|| order.iter().position(|i| entries[*i].current).unwrap_or(0));
    let max_h = fit_height(ctx, 260.0, 360.0);
    let mut chosen = None;
    let mut edit = false;
    let out = Modal::new("env-switch", 500.0).top().show(ctx, |ui| {
        let (up, down, enter) = nav_keys(ui);
        if search_header(ui, "env-search", query, tr("Switch environment…")).changed() {
            sel = 0;
        }
        step(&mut sel, order.len(), up, down);
        if order.is_empty() {
            no_results(ui, &trf("No results for “{}”", &[&q]), tr("Check the spelling or try another word."));
        } else {
            chosen = result_list(ui, &entries, &order, &q, &mut sel, up || down, max_h);
        }
        if enter {
            chosen = order.get(sel).copied();
        }
        footer(ui, |ui| {
            key_hints(ui, &[("↑↓", tr("Navigate")), ("↩", tr("Activate"))]);
            trailing(ui, |ui| {
                if kit::ghost(ui, Some(icons::PENCIL_SIMPLE), tr("Edit variables")).clicked() {
                    edit = true;
                }
            });
        });
        if chosen.is_some() || edit { Outcome::Close } else { Outcome::Keep }
    });
    *selected = Some(sel);
    if let Some(i) = chosen
        && let PaletteAction::Env(e) = entries.swap_remove(i).action
    {
        app.ws.set_active_environment(e);
    }
    if edit {
        app.open_environment(app.ws.data.active_environment.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Quick look
// ---------------------------------------------------------------------------

/// A card listing variables: header with an Edit button, then key/value rows.
/// Returns true when Edit was pressed.
fn variables_card(
    ui: &mut Ui,
    icon: &str,
    title: &str,
    tag: Option<&str>,
    vars: &[merpati_core::variables::Variable],
) -> bool {
    let p = theme::pal(ui);
    let mut edit = false;
    kit::card(ui).inner_margin(Margin::ZERO).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::Frame::new().inner_margin(Margin { left: 14, right: 8, top: 8, bottom: 8 }).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).font(theme::icon_font(16.0)).color(p.text_muted));
                ui.label(RichText::new(title).font(theme::semibold(13.5)).color(p.text));
                if let Some(tag) = tag {
                    kit::pill(ui, tag, p.ok);
                }
                trailing(ui, |ui| {
                    if kit::ghost(ui, Some(icons::PENCIL_SIMPLE), tr("Edit")).clicked() {
                        edit = true;
                    }
                });
            });
        });
        let shown: Vec<_> = vars.iter().filter(|v| v.enabled).collect();
        let w = ui.available_width();
        if shown.is_empty() {
            hairline(ui);
            egui::Frame::new().inner_margin(Margin::symmetric(14, 12)).show(ui, |ui| {
                ui.label(RichText::new(tr("No variables")).color(p.text_muted));
            });
        }
        let key_w = (w * 0.38).min(220.0);
        for v in shown {
            hairline(ui);
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
            if resp.hovered() {
                ui.painter().rect_filled(rect, 0.0, p.faint_row());
            }
            let cy = rect.center().y;
            let kg = line_galley(ui, &v.key, theme::mono(12.0), key_w - 20.0);
            ui.painter().galley(pos2(rect.left() + 14.0, cy - kg.size().y / 2.0), kg, p.text);
            let vx = rect.left() + key_w;
            if v.is_secret() {
                ui.painter().text(
                    pos2(vx, cy),
                    Align2::LEFT_CENTER,
                    icons::LOCK_SIMPLE,
                    theme::icon_font(13.0),
                    p.text_faint,
                );
                ui.painter().text(
                    pos2(vx + 18.0, cy),
                    Align2::LEFT_CENTER,
                    "••••••••",
                    theme::mono(12.0),
                    p.text_muted,
                );
            } else {
                let value = v.effective();
                let g = line_galley(ui, value, theme::mono(12.0), rect.right() - vx - 14.0);
                let elided = g.elided;
                ui.painter().galley(pos2(vx, cy - g.size().y / 2.0), g, p.text_muted);
                if elided {
                    let _ = resp.on_hover_text(RichText::new(value).monospace());
                }
            }
        }
    });
    edit
}

fn quick_look(app: &mut MerpatiApp, ctx: &egui::Context) -> Outcome {
    let mut open_env = None;
    let mut choose = false;
    let max_h = fit_height(ctx, 220.0, 520.0);
    let out = Modal::new("quick-look", 620.0)
        .title(tr("Quick look"))
        .subtitle(tr("Variables in scope right now"))
        .icon(icons::EYE)
        .show(ctx, |ui| {
            body(ui, |ui| {
                egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                    match app.ws.data.active_environment() {
                        Some(e) => {
                            if variables_card(ui, icons::GLOBE, &e.name, Some(tr("Active")), &e.variables) {
                                open_env = Some(Some(e.id.clone()));
                            }
                        }
                        None => {
                            kit::card(ui).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    let p = theme::pal(ui);
                                    ui.label(
                                        RichText::new(icons::GLOBE).font(theme::icon_font(16.0)).color(p.text_faint),
                                    );
                                    ui.label(RichText::new(tr("No environment is active")).color(p.text_muted));
                                    trailing(ui, |ui| {
                                        if kit::secondary(ui, None, tr("Choose…")).clicked() {
                                            choose = true;
                                        }
                                    });
                                });
                            });
                        }
                    }
                    ui.add_space(12.0);
                    if variables_card(ui, icons::GLOBE_SIMPLE, tr("Globals"), None, &app.ws.data.globals) {
                        open_env = Some(None);
                    }
                });
            });
            footer(ui, |ui| {
                let p = theme::pal(ui);
                ui.label(RichText::new(icons::LOCK_SIMPLE).font(theme::icon_font(13.0)).color(p.text_faint));
                ui.label(RichText::new(tr("Secret values stay hidden.")).size(12.0).color(p.text_muted));
                trailing(ui, |ui| {
                    if kit::secondary(ui, None, tr("Close")).clicked() { Outcome::Close } else { Outcome::Keep }
                })
            })
        });
    if choose {
        app.dialog = Some(Dialog::env_switcher());
        return Outcome::Close;
    }
    if let Some(e) = open_env {
        app.open_environment(e);
        return Outcome::Close;
    }
    out
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

fn settings(app: &mut MerpatiApp, ctx: &egui::Context, draft: &mut AppSettings, page: &mut SettingsPage) -> Outcome {
    let mut save = false;
    let changed = *draft != app.settings;
    let height = fit_height(ctx, 190.0, 540.0);
    let current = *page;
    let out = Modal::new("settings", 860.0).title(tr("Settings")).icon(icons::GEAR_SIX).sticky().show(ctx, |ui| {
        split(
            ui,
            height,
            200.0,
            |ui| {
                egui::Frame::new().inner_margin(Margin::same(10)).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for pg in SettingsPage::ALL {
                        if nav_item(ui, Some(pg.icon()), pg.label(), None, *page == pg).clicked() {
                            *page = pg;
                        }
                    }
                });
            },
            |ui| {
                egui::ScrollArea::vertical().id_salt(("settings-page", current)).auto_shrink([false; 2]).show(
                    ui,
                    |ui| {
                        egui::Frame::new().inner_margin(Margin { left: 26, right: 26, top: 20, bottom: 8 }).show(
                            ui,
                            |ui| {
                                ui.set_width(ui.available_width());
                                settings_page(app, ui, draft, current);
                            },
                        );
                    },
                );
            },
        );
        footer(ui, |ui| {
            let p = theme::pal(ui);
            if changed {
                kit::dot(ui, p.accent, 3.5);
                ui.label(RichText::new(tr("Unsaved changes")).size(12.0).color(p.text_muted));
            } else {
                ui.label(RichText::new(tr("Changes apply when you save.")).size(12.0).color(p.text_faint));
            }
            trailing(ui, |ui| {
                if kit::primary(ui, tr("Save")).clicked() {
                    save = true;
                }
                if kit::secondary(ui, None, tr("Cancel")).clicked() { Outcome::Close } else { Outcome::Keep }
            })
        })
    });
    if save {
        app.apply_settings(draft.clone());
        return Outcome::Close;
    }
    out
}

fn settings_page(app: &MerpatiApp, ui: &mut Ui, draft: &mut AppSettings, page: SettingsPage) {
    let p = theme::pal(ui);
    if page != SettingsPage::About {
        ui.label(RichText::new(page.label()).font(theme::semibold(17.0)).color(p.text));
        ui.add_space(2.0);
        ui.label(RichText::new(page.blurb()).color(p.text_muted));
        ui.add_space(18.0);
    }
    match page {
        SettingsPage::General => general_page(ui, draft),
        SettingsPage::Requests => requests_page(ui, draft),
        SettingsPage::Scripts => scripts_page(app, ui, draft),
        SettingsPage::Proxy => proxy_page(ui, draft),
        SettingsPage::Certificates => certificates_page(ui, draft),
        SettingsPage::Shortcuts => shortcuts_page(ui),
        SettingsPage::About => about_page(app, ui),
    }
}

/// A number field with a unit, for settings rows.
fn number<N: egui::emath::Numeric>(
    ui: &mut Ui,
    name: &str,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
    unit: &str,
) {
    let speed = (range.end().to_f64() - range.start().to_f64()) / 2000.0;
    let r = ui.add(egui::DragValue::new(value).range(range).suffix(unit).speed(speed.clamp(0.05, 50.0)));
    crate::widgets::a11y_name(&r, name);
}

/// A size in MB stored as bytes.
fn megabytes(ui: &mut Ui, name: &str, bytes: &mut u64, max: u32) {
    let mut mb = (*bytes / (1024 * 1024)) as u32;
    let before = mb;
    number(ui, name, &mut mb, 1..=max, " MB");
    if mb != before {
        *bytes = mb as u64 * 1024 * 1024;
    }
}

fn general_page(ui: &mut Ui, draft: &mut AppSettings) {
    group(ui, tr("Appearance"), |g| {
        g.row(tr("Theme"), tr("Follow the system, or always use light or dark."), |ui| {
            segmented(
                ui,
                &mut draft.ui.theme,
                &[
                    (ThemePreference::System, format!("{}  {}", icons::CIRCLE_HALF, tr("System"))),
                    (ThemePreference::Light, format!("{}  {}", icons::SUN, tr("Light"))),
                    (ThemePreference::Dark, format!("{}  {}", icons::MOON, tr("Dark"))),
                ],
            );
        });
        g.row(tr("Language"), tr("The language of menus and messages."), |ui| {
            segmented(
                ui,
                &mut draft.ui.language,
                &[(UiLanguage::En, "English".to_string()), (UiLanguage::Id, "Bahasa Indonesia".to_string())],
            );
        });
        g.row(tr("Font size"), tr("Text size of the whole interface."), |ui| {
            let p = theme::pal(ui);
            ui.label(RichText::new(format!("{} px", draft.ui.font_size)).font(theme::medium(12.5)).color(p.text_muted));
            ui.spacing_mut().slider_width = 150.0;
            let r = ui.add(egui::Slider::new(&mut draft.ui.font_size, 10.0..=20.0).step_by(0.5).show_value(false));
            crate::widgets::a11y_name(&r, tr("Font size"));
            widgets::a11y_name(&r, tr("Font size"));
        });
        g.row(tr("Response panel"), tr("Where the response appears next to the request."), |ui| {
            segmented(
                ui,
                &mut draft.ui.layout,
                &[
                    (ResponseLayout::Below, format!("{}  {}", icons::ROWS, tr("Below"))),
                    (ResponseLayout::Side, format!("{}  {}", icons::COLUMNS, tr("Side by side"))),
                ],
            );
        });
    });
    group(ui, tr("Updates"), |g| {
        let label = tr("Check for updates on startup");
        g.row(
            label,
            tr("Merpati asks the release feed for the latest version and shows what is new. Nothing about you or your work is sent."),
            |ui| {
                toggle(ui, &mut draft.updates.check_on_startup, label);
            },
        );
        g.row(&trf("Merpati {}", &[&crate::update::CURRENT]), tr("The version installed on this computer."), |ui| {
            if kit::secondary(ui, Some(icons::ARROWS_CLOCKWISE), tr("Check now")).clicked() {
                crate::update::request_check(ui.ctx());
            }
        });
    });
    group(ui, tr("History"), |g| {
        g.row(tr("History entries"), tr("The oldest entries are removed when History is full."), |ui| {
            number(ui, tr("History entries"), &mut draft.history_limit, 10..=100_000, "");
        });
    });
}

fn requests_page(ui: &mut Ui, draft: &mut AppSettings) {
    let r = &mut draft.request;
    group(ui, tr("Sending"), |g| {
        g.row(tr("Request timeout"), tr("How long to wait for a response; 0 waits forever."), |ui| {
            number(ui, tr("Request timeout"), &mut r.timeout_ms, 0..=3_600_000, " ms");
        });
        g.row(tr("HTTP version"), tr("Auto lets client and server agree on one."), |ui| {
            segmented(
                ui,
                &mut r.http_version,
                &[
                    (HttpVersion::Auto, tr("Auto").to_string()),
                    (HttpVersion::Http1, "HTTP/1.1".to_string()),
                    (HttpVersion::Http2, "HTTP/2".to_string()),
                ],
            );
        });
        g.row(tr("Encode URL automatically"), tr("Percent-encode characters that are not allowed in a URL."), |ui| {
            toggle(ui, &mut r.encode_url, tr("Encode URL automatically"));
        });
    });
    group(ui, tr("Redirects"), |g| {
        g.row(tr("Follow redirects"), tr("Go on to the address a 3xx response points to."), |ui| {
            toggle(ui, &mut r.follow_redirects, tr("Follow redirects"));
        });
        g.enabled(r.follow_redirects);
        g.row(tr("Maximum redirects"), "", |ui| {
            number(ui, tr("Maximum redirects"), &mut r.max_redirects, 0..=100, "");
        });
    });
    group(ui, tr("Security"), |g| {
        g.row(
            tr("Verify SSL certificates"),
            tr("Turn this off only for local servers with self-signed certificates."),
            |ui| {
                toggle(ui, &mut r.verify_ssl, tr("Verify SSL certificates"));
            },
        );
    });
    group(ui, tr("Default headers"), |g| {
        let agent = format!("User-Agent: {}", merpati_core::settings::USER_AGENT);
        g.row(tr("Send User-Agent header"), &agent, |ui| {
            toggle(ui, &mut r.send_user_agent, tr("Send User-Agent header"));
        });
        g.row(tr("Send no-cache header"), "Cache-Control: no-cache", |ui| {
            toggle(ui, &mut r.send_no_cache, tr("Send no-cache header"));
        });
    });
    group(ui, tr("Responses"), |g| {
        g.row(tr("Keep bodies in memory up to"), tr("Larger bodies are kept in a temporary file."), |ui| {
            megabytes(ui, tr("Keep bodies in memory up to"), &mut draft.max_body_in_memory_bytes, 1024);
        });
        g.row(tr("Pretty-print bodies up to"), tr("Larger bodies are shown as plain text."), |ui| {
            megabytes(ui, tr("Pretty-print bodies up to"), &mut draft.max_pretty_bytes, 200);
        });
    });
}

fn scripts_page(app: &MerpatiApp, ui: &mut Ui, draft: &mut AppSettings) {
    group(ui, tr("Limits"), |g| {
        g.row(tr("Script timeout"), tr("Scripts that run longer are stopped."), |ui| {
            number(ui, tr("Script timeout"), &mut draft.script_timeout_ms, 100..=600_000, " ms");
        });
        g.row(
            tr("Script memory limit"),
            tr("Scripts run in a separate process that is stopped when it uses more memory than this; 0 means no limit."),
            |ui| {
                number(ui, tr("Script memory limit"), &mut draft.script_memory_mb, 0..=65_536, " MB");
            },
        );
        g.row(tr("Script engine"), "", |ui| {
            ui.label(RichText::new(app.scripts.name().to_string()).color(theme::pal(ui).text_muted));
        });
    });
    group(ui, tr("Package registry"), |g| {
        g.row(
            tr("Registry URL"),
            tr("esm.sh-compatible; serves pm.require('npm:…') and pm.require('jsr:…')"),
            |ui| {
                let w = ui.available_width();
                ui.add(input(&mut draft.package_registry).hint_text("https://esm.sh").desired_width(w));
            },
        );
    });
    let root = app.ws.root().to_path_buf();
    let dir = root.join("packages");
    group(ui, tr("Package Library"), |g| {
        g.custom(|ui| {
            let p = theme::pal(ui);
            ui.add(egui::Label::new(kit::hint(ui, tr("JavaScript modules in packages/ (CommonJS or ESM), loaded with pm.require('@team/name') or pm.require('name'). They are part of the workspace and can be committed."))).wrap());
            ui.add_space(4.0);
            let mut files: Vec<std::path::PathBuf> = Vec::new();
            fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
                for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, out);
                    } else if p.extension().is_some_and(|x| x == "js" || x == "mjs" || x == "cjs") {
                        out.push(p);
                    }
                }
            }
            walk(&dir, &mut files);
            files.sort();
            if files.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::PACKAGE).font(theme::icon_font(15.0)).color(p.text_faint));
                    ui.label(RichText::new(tr("No packages yet.")).color(p.text_muted));
                });
            }
            for f in &files {
                let rel = f.strip_prefix(&dir).unwrap_or(f).display().to_string();
                let name = rel
                    .trim_end_matches(".js")
                    .trim_end_matches(".mjs")
                    .trim_end_matches(".cjs")
                    .trim_end_matches("/index");
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::PACKAGE).font(theme::icon_font(15.0)).color(p.text_muted));
                    ui.label(RichText::new(format!("pm.require('{name}')")).font(theme::mono(12.0)).color(p.text));
                    trailing(ui, |ui| {
                        if kit::icon_button_ex(ui, icons::ARROW_SQUARE_OUT, tr("Open"), false, 24.0).clicked() {
                            let _ = open::that(f);
                        }
                    });
                });
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if kit::secondary(ui, Some(icons::PLUS), tr("New package")).clicked() {
                    let mut n = 1;
                    let mut path = dir.join("my-helpers.js");
                    while path.exists() {
                        n += 1;
                        path = dir.join(format!("my-helpers-{n}.js"));
                    }
                    let body = "// Shared helpers: pm.require('my-helpers') in any script.\nmodule.exports = {\n  isoNow: () => new Date().toISOString(),\n};\n";
                    if std::fs::create_dir_all(&dir).is_ok()
                        && merpati_core::storage::write_atomic(&path, body.as_bytes()).is_ok()
                    {
                        let _ = open::that(&path);
                    }
                }
                if kit::ghost(ui, Some(icons::FOLDER_OPEN), tr("Open folder")).clicked() {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = open::that(&dir);
                }
            });
            let lock = root.join("packages.lock.json");
            if let Ok(text) = std::fs::read_to_string(&lock) {
                let v = serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
                let count = |k: &str| v.get(k).and_then(|m| m.as_object()).map(|o| o.len()).unwrap_or(0);
                ui.add_space(6.0);
                ui.label(kit::hint(
                    ui,
                    &trf(
                        "packages.lock.json: {} {} · {} {}",
                        &[&count("packages"), &tr("packages"), &count("modules"), &tr("pinned modules")],
                    ),
                ));
                for (spec, url) in v.get("packages").and_then(|m| m.as_object()).into_iter().flatten() {
                    ui.label(
                        RichText::new(format!("{spec} → {}", url.as_str().unwrap_or("")))
                            .font(theme::mono(11.0))
                            .color(p.text_muted),
                    );
                }
            }
        });
    });
}

fn proxy_page(ui: &mut Ui, draft: &mut AppSettings) {
    let px = &mut draft.proxy;
    group(ui, tr("System proxy"), |g| {
        g.row(
            tr("Use system proxy"),
            tr("Honours HTTP_PROXY / HTTPS_PROXY / NO_PROXY and the OS proxy settings, including a proxy auto-config (PAC) script."),
            |ui| {
                toggle(ui, &mut px.use_system, tr("Use system proxy"));
            },
        );
    });
    group(ui, tr("Custom proxy"), |g| {
        g.row(tr("Use custom proxy"), tr("Send requests through a proxy you set here."), |ui| {
            toggle(ui, &mut px.custom_enabled, tr("Use custom proxy"));
        });
        g.enabled(px.custom_enabled);
        g.row(tr("Proxy URL"), "", |ui| {
            let w = ui.available_width();
            ui.add(
                input(&mut px.custom_url)
                    .hint_text(tr("http://proxy:3128 or socks5://127.0.0.1:1080"))
                    .desired_width(w),
            );
        });
        g.row(tr("Username"), "", |ui| {
            let w = ui.available_width();
            ui.add(input(&mut px.username).desired_width(w));
        });
        g.row(tr("Password"), tr("Stored in the Keychain, not in the settings file."), |ui| {
            if merpati_core::variables::is_pure_reference(&px.password) {
                if kit::ghost(ui, Some(icons::PENCIL_SIMPLE), tr("Change")).clicked() {
                    px.password.clear();
                }
                ui.label(
                    RichText::new(format!("{}  {}", icons::LOCK_SIMPLE, tr("Stored in the Keychain")))
                        .color(theme::pal(ui).text_muted),
                );
            } else {
                let w = ui.available_width();
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    secret_input(ui, "proxy-pass", &mut px.password, w);
                });
            }
        });
        g.row(tr("Bypass"), tr("Hosts reached directly, separated by commas."), |ui| {
            let w = ui.available_width();
            ui.add(input(&mut px.bypass).hint_text(tr("localhost, *.internal")).desired_width(w));
        });
        g.row(
            tr("Auto-config (PAC) URL"),
            tr("A PAC script picks the proxy for each request instead of the proxy URL."),
            |ui| {
                let w = ui.available_width();
                ui.add(input(&mut px.pac_url).hint_text("http://wpad.example/proxy.pac").desired_width(w));
            },
        );
    });
}

/// A muted label in a fixed-width column, for small forms.
fn cell_label(ui: &mut Ui, text: &str, width: f32) {
    let p = theme::pal(ui);
    ui.allocate_ui_with_layout(vec2(width, 30.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_width(width);
        ui.add(egui::Label::new(RichText::new(text).color(p.text_muted)).truncate());
    });
}

/// A file path field with a Choose button.
fn path_field(ui: &mut Ui, value: &mut String, name: &str, width: f32) {
    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let r =
            ui.add(input(value).font(egui::TextStyle::Monospace).hint_text(tr("No file")).desired_width(width - 36.0));
        widgets::a11y_name(&r, name);
        if kit::icon_button_ex(ui, icons::FOLDER_OPEN, tr("Choose file…"), false, 30.0).clicked()
            && let Some(p) = rfd::FileDialog::new().pick_file()
        {
            *value = p.display().to_string();
        }
    });
}

fn certificates_page(ui: &mut Ui, draft: &mut AppSettings) {
    let certs = &mut draft.certificates;
    group(ui, tr("Certificate authorities"), |g| {
        g.row(
            tr("Trust an extra CA certificate (PEM)"),
            tr("Trusted in addition to the system certificate store."),
            |ui| {
                toggle(ui, &mut certs.ca_enabled, tr("Trust an extra CA certificate (PEM)"));
            },
        );
        g.enabled(certs.ca_enabled);
        g.row(tr("CA file"), "", |ui| {
            let w = ui.available_width();
            path_field(ui, &mut certs.ca_path, tr("CA file"), w);
        });
    });
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(tr("Client certificates")).font(theme::semibold(13.0)).color(p.text));
        trailing(ui, |ui| {
            if kit::ghost(ui, Some(icons::PLUS), tr("Add certificate")).clicked() {
                certs.client_certs.push(ClientCertificate { enabled: true, ..Default::default() });
            }
        });
    });
    ui.add_space(2.0);
    if certs.client_certs.is_empty() {
        kit::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(6.0);
            compact_empty(
                ui,
                icons::SHIELD_CHECK,
                tr("No client certificates"),
                tr("Add one to send it to servers that ask for it (mutual TLS)."),
            );
            ui.add_space(6.0);
        });
    }
    let mut remove = None;
    for (i, c) in certs.client_certs.iter_mut().enumerate() {
        ui.push_id(i, |ui| {
            kit::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let w = ui.available_width();
                ui.horizontal(|ui| {
                    toggle(ui, &mut c.enabled, tr("Enabled"));
                    let r = ui.add(
                        input(&mut c.host)
                            .hint_text(tr("api.example.com:443, *.example.com or *"))
                            .desired_width(w - 90.0),
                    );
                    widgets::a11y_name(&r, tr("Host"));
                    trailing(ui, |ui| {
                        if kit::icon_button(ui, icons::TRASH, tr("Remove certificate")).clicked() {
                            remove = Some(i);
                        }
                    });
                });
                ui.add_space(4.0);
                let label_w = 96.0;
                for (label, value) in
                    [("CRT (PEM)", &mut c.cert_path), ("KEY (PEM)", &mut c.key_path), ("PFX / P12", &mut c.pfx_path)]
                {
                    ui.horizontal(|ui| {
                        cell_label(ui, label, label_w);
                        path_field(ui, value, label, w - label_w - 8.0);
                    });
                }
                ui.horizontal(|ui| {
                    cell_label(ui, tr("Passphrase"), label_w);
                    if merpati_core::variables::is_pure_reference(&c.passphrase) {
                        ui.label(RichText::new(format!("{}  {}", icons::LOCK_SIMPLE, tr("Stored in the Keychain"))).color(p.text_muted));
                        if kit::ghost(ui, Some(icons::PENCIL_SIMPLE), tr("Change")).clicked() {
                            c.passphrase.clear();
                        }
                    } else {
                        secret_input(ui, ("cert-pass", i), &mut c.passphrase, w - label_w - 8.0);
                    }
                });
                ui.add_space(2.0);
                ui.label(kit::hint(ui, tr("A PKCS#12 bundle replaces the PEM files. The passphrase is stored in the Keychain, not in the settings file.")));
            });
        });
        ui.add_space(8.0);
    }
    if let Some(i) = remove {
        certs.client_certs.remove(i);
    }
}

fn shortcuts_page(ui: &mut Ui) {
    group(ui, tr("Requests"), |g| {
        for (action, keys) in
            [("Send request", "⌘↩"), ("New request", "⌘N"), ("Save", "⌘S"), ("Accept variable suggestion", "Tab")]
        {
            g.row(tr(action), "", |ui| shortcut_chips(ui, keys));
        }
    });
    group(ui, tr("Navigation"), |g| {
        for (action, keys) in [
            ("Search requests", "⌘K"),
            ("Command palette", "⇧⌘P"),
            ("Switch environment", "⌘E"),
            ("New tab / close tab", "⌘T / ⌘W"),
            ("Next / previous tab", "⇧⌘] / ⇧⌘["),
            ("Toggle sidebar", "⌘B"),
            ("Console", "⌥⌘C"),
            ("Settings", "⌘,"),
        ] {
            g.row(tr(action), "", |ui| shortcut_chips(ui, keys));
        }
    });
    ui.label(kit::hint(ui, tr("⌘ is Ctrl on Windows and Linux.")));
}

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

fn about_page(app: &MerpatiApp, ui: &mut Ui) {
    let p = theme::pal(ui);
    ui.vertical_centered(|ui| {
        ui.add_space(10.0);
        ui.add(
            egui::Image::new(egui::include_image!("../assets/icon-256.png"))
                .fit_to_exact_size(vec2(76.0, 76.0))
                .corner_radius(17),
        );
        ui.add_space(10.0);
        ui.label(RichText::new("Merpati").font(theme::semibold(22.0)).color(p.text));
        ui.add_space(4.0);
        kit::pill(ui, &trf("Version {}", &[&crate::update::CURRENT]), p.accent_text);
        ui.add_space(10.0);
        ui.label(RichText::new(tr("A lightweight API client written in Rust. No account, no telemetry: data stays in your workspace folder.")).color(p.text_muted));
        ui.add_space(14.0);
        // Centered row of links: measure once, then indent by half the slack.
        let links = [
            (icons::ARROW_SQUARE_OUT, tr("Source code"), REPOSITORY.to_string()),
            (icons::FILE_TEXT, tr("Release notes"), format!("{REPOSITORY}/releases")),
            (icons::BUG, tr("Report an issue"), format!("{REPOSITORY}/issues")),
        ];
        let id = ui.scope_id().with("about-links");
        let row_w: f32 = ui.data(|d| d.get_temp(id).unwrap_or(430.0));
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - row_w) / 2.0).max(0.0));
            let start = ui.cursor().left();
            for (icon, label, url) in &links {
                if kit::secondary(ui, Some(*icon), label).on_hover_text(url).clicked() {
                    widgets::open_web_link(url);
                }
            }
            let w = ui.min_rect().right() - start;
            ui.data_mut(|d| d.insert_temp(id, w));
        });
        ui.add_space(22.0);
    });
    group(ui, tr("System"), |g| {
        let path = app.settings_path.display().to_string();
        g.row(tr("Settings file"), "", |ui| {
            if kit::icon_button(ui, icons::COPY, tr("Copy path")).clicked() {
                ui.ctx().copy_text(path.clone());
            }
            let w = (ui.available_width() - 60.0).max(80.0);
            ui.add_sized(
                [w, 20.0],
                egui::Label::new(RichText::new(&path).font(theme::mono(11.5)).color(theme::pal(ui).text_muted))
                    .truncate(),
            )
            .on_hover_text(&path);
        });
        g.row(tr("Secrets"), "", |ui| {
            ui.label(RichText::new(app.ws.secrets.backend_description()).color(theme::pal(ui).text_muted));
        });
        g.row(tr("Script engine"), "", |ui| {
            ui.label(RichText::new(app.scripts.name().to_string()).color(theme::pal(ui).text_muted));
        });
        g.row(tr("License"), "", |ui| {
            ui.label(RichText::new(env!("CARGO_PKG_LICENSE")).color(theme::pal(ui).text_muted));
        });
    });
}

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

fn cookies(
    app: &mut MerpatiApp,
    ctx: &egui::Context,
    domain: &mut Option<String>,
    new_cookie: &mut String,
    error: &mut Option<String>,
    new_domain: &mut String,
) -> Outcome {
    let jar = app.ws.cookies.clone();
    let all = jar.list();
    let mut domains = jar.domains();
    if let Some(d) = domain.as_ref()
        && !domains.contains(d)
    {
        domains.push(d.clone());
    }
    if domain.is_none() {
        *domain = domains.first().cloned();
    }
    let mut changed = false;
    let mut picked: Option<String> = None;
    let subtitle = match (all.len(), domains.len()) {
        (0, _) => tr("Cookies set by responses are kept here and sent with matching requests.").to_string(),
        (n, d) => format!("{} · {}", plural(n, "{} cookie", "{} cookies"), plural(d, "{} domain", "{} domains")),
    };
    let height = fit_height(ctx, 200.0, 440.0);
    let out =
        Modal::new("cookies", 880.0).title(tr("Cookies")).subtitle(subtitle).icon(icons::COOKIE).show(ctx, |ui| {
            let current = domain.clone();
            split(
                ui,
                height,
                230.0,
                |ui| {
                    egui::Frame::new().inner_margin(Margin::same(10)).show(ui, |ui| {
                        let p = theme::pal(ui);
                        ui.horizontal(|ui| {
                            ui.add_space(4.0);
                            ui.label(RichText::new(tr("Domains")).font(theme::medium(11.5)).color(p.text_faint));
                        });
                        let list_h = ui.available_height() - 44.0;
                        egui::ScrollArea::vertical()
                            .id_salt("cookie-domains")
                            .max_height(list_h)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                if domains.is_empty() {
                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        ui.add_space(4.0);
                                        ui.label(RichText::new(tr("No domains yet")).color(p.text_muted));
                                    });
                                }
                                for d in &domains {
                                    let n = all.iter().filter(|c| &c.domain == d).count().to_string();
                                    if nav_item(
                                        ui,
                                        Some(icons::GLOBE_SIMPLE),
                                        d,
                                        Some(n.as_str()),
                                        current.as_deref() == Some(d),
                                    )
                                    .clicked()
                                    {
                                        picked = Some(d.clone());
                                    }
                                }
                            });
                        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                            let r = ui.add(
                                input(new_domain).hint_text(tr("Add a domain…")).desired_width(ui.available_width()),
                            );
                            widgets::a11y_name(&r, tr("Add a domain"));
                            if r.lost_focus()
                                && ui.input(|i| i.key_pressed(Key::Enter))
                                && !new_domain.trim().is_empty()
                            {
                                picked = Some(new_domain.trim().to_string());
                                new_domain.clear();
                            }
                        });
                    });
                },
                |ui| {
                    egui::Frame::new().inner_margin(Margin { left: 20, right: 20, top: 16, bottom: 14 }).show(
                        ui,
                        |ui| {
                            ui.set_width(ui.available_width());
                            let Some(d) = current.clone() else {
                                ui.add_space(ui.available_height() / 2.0 - 50.0);
                                compact_empty(
                                    ui,
                                    icons::COOKIE,
                                    tr("No cookies yet"),
                                    tr("Cookies set by responses appear here. Add a domain to set one by hand."),
                                );
                                return;
                            };
                            changed |= cookie_table(ui, &jar, &all, &d, new_cookie, error);
                        },
                    );
                },
            );
            footer(ui, |ui| {
                let p = theme::pal(ui);
                ui.label(RichText::new(icons::INFO).font(theme::icon_font(14.0)).color(p.text_faint));
                ui.label(
                    RichText::new(tr("Stored in this workspace, never committed.")).size(12.0).color(p.text_muted),
                );
                trailing(ui, |ui| {
                    if kit::secondary(ui, None, tr("Close")).clicked() { Outcome::Close } else { Outcome::Keep }
                })
            })
        });
    if let Some(d) = picked {
        *domain = Some(d);
        *error = None;
    }
    if changed {
        app.ws.mark_cookies();
    }
    out
}

/// The cookies of one domain with their actions and the "add" field.
/// Returns true when the jar changed.
fn cookie_table(
    ui: &mut Ui,
    jar: &merpati_http::cookies::CookieJar,
    all: &[merpati_http::cookies::CookieInfo],
    d: &str,
    new_cookie: &mut String,
    error: &mut Option<String>,
) -> bool {
    let p = theme::pal(ui);
    let mut changed = false;
    let list: Vec<_> = all.iter().filter(|c| c.domain == d).collect();
    ui.horizontal(|ui| {
        ui.label(RichText::new(d).font(theme::semibold(15.0)).color(p.text));
        ui.label(RichText::new(plural(list.len(), "{} cookie", "{} cookies")).color(p.text_muted));
        trailing(ui, |ui| {
            let r = ui.add_enabled_ui(!list.is_empty(), |ui| {
                kit::button_ex(ui, kit::Kind::Danger, Some(icons::TRASH), tr("Clear all"), true)
            });
            if r.inner.clicked() {
                jar.clear_domain(d);
                changed = true;
            }
        });
    });
    ui.add_space(8.0);
    let table_h = (ui.available_height() - 96.0).max(80.0);
    kit::card(ui).inner_margin(Margin::ZERO).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let w = ui.available_width();
        let cols = [0.0, 0.26, 0.62, 0.74];
        let x_of = |rect: Rect, f: f32| rect.left() + 12.0 + (w - 70.0) * f;
        // Header
        let (head, _) = ui.allocate_exact_size(vec2(w, 28.0), Sense::hover());
        ui.painter().rect_filled(head, CornerRadius { nw: 7, ne: 7, sw: 0, se: 0 }, side_fill(ui));
        for (f, t) in cols.iter().zip([tr("Name"), tr("Value"), tr("Path"), tr("Expires")]) {
            ui.painter().text(
                pos2(x_of(head, *f), head.center().y),
                Align2::LEFT_CENTER,
                t,
                theme::medium(12.0),
                p.text_muted,
            );
        }
        hairline(ui);
        egui::ScrollArea::vertical().id_salt("cookie-rows").max_height(table_h).auto_shrink([false, true]).show(
            ui,
            |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if list.is_empty() {
                    egui::Frame::new().inner_margin(Margin::symmetric(12, 14)).show(ui, |ui| {
                        ui.label(RichText::new(tr("No cookies for this domain. Add one below.")).color(p.text_muted));
                    });
                }
                for (i, c) in list.iter().enumerate() {
                    if i > 0 {
                        hairline(ui);
                    }
                    let (rect, resp) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, 0.0, p.faint_row());
                    }
                    let cy = rect.center().y;
                    let cell = |f0: f32, f1: f32| x_of(rect, f1) - x_of(rect, f0) - 10.0;
                    let name = line_galley(ui, &c.name, theme::mono(12.0), cell(cols[0], cols[1]));
                    ui.painter().galley(pos2(x_of(rect, cols[0]), cy - name.size().y / 2.0), name, p.text);
                    let value = line_galley(ui, &c.value, theme::mono(12.0), cell(cols[1], cols[2]));
                    ui.painter().galley(pos2(x_of(rect, cols[1]), cy - value.size().y / 2.0), value, p.text_muted);
                    let path = line_galley(ui, &c.path, theme::mono(12.0), cell(cols[2], cols[3]));
                    ui.painter().galley(pos2(x_of(rect, cols[2]), cy - path.size().y / 2.0), path, p.text_muted);
                    let expires = c
                        .expires
                        .and_then(|e| chrono::DateTime::from_timestamp(e, 0))
                        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| tr("Session").to_string());
                    let eg = line_galley(
                        ui,
                        &expires,
                        FontId::proportional(12.0),
                        rect.right() - 70.0 - x_of(rect, cols[3]),
                    );
                    ui.painter().galley(pos2(x_of(rect, cols[3]), cy - eg.size().y / 2.0), eg, p.text_muted);
                    let _ = resp.on_hover_text(RichText::new(c.to_set_cookie()).monospace());
                    let actions = Rect::from_min_max(pos2(rect.right() - 62.0, rect.top()), rect.max);
                    let mut child =
                        ui.new_child(UiBuilder::new().max_rect(actions).layout(Layout::right_to_left(Align::Center)));
                    child.spacing_mut().item_spacing.x = 2.0;
                    if kit::icon_button_ex(&mut child, icons::TRASH, tr("Delete cookie"), false, 26.0).clicked() {
                        jar.remove(&c.domain, &c.path, &c.name);
                        changed = true;
                    }
                    if kit::icon_button_ex(&mut child, icons::PENCIL_SIMPLE, tr("Edit cookie"), false, 26.0).clicked() {
                        *new_cookie = c.to_set_cookie();
                        *error = None;
                    }
                }
            },
        );
    });
    ui.add_space(12.0);
    field_label(ui, tr("Add or replace a cookie"));
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 30.0), Layout::right_to_left(Align::Center), |ui| {
        let add = kit::secondary(ui, Some(icons::PLUS), tr("Add")).clicked();
        let w = ui.available_width();
        let r = ui.add(
            input(new_cookie)
                .font(egui::TextStyle::Monospace)
                .hint_text(tr("name=value; Path=/; Expires=…"))
                .desired_width(w),
        );
        widgets::a11y_name(&r, tr("Cookie"));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if (add || enter) && !new_cookie.trim().is_empty() {
            match jar.set(d, new_cookie) {
                Ok(()) => {
                    new_cookie.clear();
                    *error = None;
                    changed = true;
                }
                Err(e) => *error = Some(e),
            }
        }
    });
    if let Some(e) = error {
        ui.add_space(4.0);
        banner(ui, Tone::Err, e);
    }
    changed
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Points along a rounded rectangle, for a dashed outline.
fn rounded_outline(rect: Rect, r: f32) -> Vec<egui::Pos2> {
    let mut pts = Vec::new();
    let corners = [
        (pos2(rect.right() - r, rect.top() + r), -90.0_f32),
        (pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (pos2(rect.left() + r, rect.top() + r), 180.0),
    ];
    for (c, start) in corners {
        for k in 0..=6 {
            let a = (start + 90.0 * k as f32 / 6.0).to_radians();
            pts.push(c + vec2(a.cos(), a.sin()) * r);
        }
    }
    pts.push(pts[0]);
    pts
}

/// Reads a file to import.
fn read_import(path: &std::path::Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

/// `1 cookie` / `3 cookies` (the Indonesian table has both forms).
pub(crate) fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { trf(one, &[&n]) } else { trf(many, &[&n]) }
}

fn import(
    app: &mut MerpatiApp,
    ctx: &egui::Context,
    text: &mut String,
    error: &mut Option<String>,
    preview: &mut Option<merpati_convert::Imported>,
    source: &mut String,
) -> Outcome {
    let mut do_import = false;
    // A file dropped anywhere on the window while the dialog is open.
    let dropped = ctx.input(|i| i.raw.dropped_files.first().cloned());
    if let Some(f) = dropped {
        let read = read_import(f.path())
            .or_else(|e| f.bytes().map(|b| String::from_utf8_lossy(&b).into_owned()).map_err(|_| e));
        match read {
            Ok(t) => {
                *text = t;
                *source = f.path().display().to_string();
                *preview = None;
                *error = None;
            }
            Err(e) => *error = Some(e),
        }
    }
    let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
    let out = Modal::new("import", 700.0)
        .title(tr("Import"))
        .subtitle(tr("Bring in collections, environments and requests from other tools."))
        .icon(icons::DOWNLOAD_SIMPLE)
        .sticky()
        .show(ctx, |ui| {
            body(ui, |ui| {
                let p = theme::pal(ui);
                // Drop zone (also a button that opens the file picker).
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 112.0), Sense::click());
                widgets::a11y_button(&resp, tr("Choose file…"));
                let hot = hovering || resp.hovered();
                ui.painter().rect_filled(rect, CornerRadius::same(10), if hot { p.accent_soft } else { side_fill(ui) });
                let stroke = Stroke::new(1.2, if hot { p.accent } else { p.border_strong });
                ui.painter().extend(egui::Shape::dashed_line(
                    &rounded_outline(rect.shrink(0.5), 10.0),
                    stroke,
                    6.0,
                    4.0,
                ));
                let cy = rect.center().y;
                ui.painter().text(
                    pos2(rect.center().x, cy - 26.0),
                    Align2::CENTER_CENTER,
                    icons::FILE_ARROW_UP,
                    theme::icon_font(26.0),
                    if hot { p.accent_text } else { p.text_muted },
                );
                let title =
                    if hovering { tr("Drop to import") } else { tr("Drop a file here, or click to choose one") };
                ui.painter().text(
                    pos2(rect.center().x, cy + 6.0),
                    Align2::CENTER_CENTER,
                    title,
                    theme::medium(13.5),
                    p.text,
                );
                ui.painter().text(
                    pos2(rect.center().x, cy + 26.0),
                    Align2::CENTER_CENTER,
                    "Postman · OpenAPI / Swagger · Insomnia · HAR · Hoppscotch · Thunder Client · WSDL · cURL",
                    FontId::proportional(11.5),
                    p.text_muted,
                );
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_file()
                {
                    match read_import(&path) {
                        Ok(t) => {
                            *text = t;
                            *source = path.display().to_string();
                            *preview = None;
                            *error = None;
                        }
                        Err(e) => *error = Some(e),
                    }
                }
                if !source.is_empty() {
                    ui.add_space(8.0);
                    let mut clear = false;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new(icons::FILE_TEXT).font(theme::icon_font(15.0)).color(p.text_muted));
                        let path = std::path::Path::new(source.as_str());
                        let name =
                            path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| source.clone());
                        ui.label(RichText::new(name).font(theme::medium(13.0)).color(p.text));
                        trailing(ui, |ui| {
                            if kit::icon_button_ex(ui, icons::X, tr("Clear"), false, 22.0).clicked() {
                                clear = true;
                            }
                            if let Some(dir) = path.parent() {
                                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(dir.display().to_string()).size(12.0).color(p.text_faint),
                                        )
                                        .truncate(),
                                    );
                                });
                            }
                        });
                    });
                    if clear {
                        source.clear();
                        text.clear();
                    }
                }
                ui.add_space(12.0);
                // "or paste" divider
                ui.horizontal(|ui| {
                    let label = tr("or paste the content");
                    let g = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(11.5), p.text_faint);
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), g.size().y), Sense::hover());
                    let gw = g.size().x;
                    let mid = r.center();
                    ui.painter().hline(r.left()..=mid.x - gw / 2.0 - 10.0, mid.y, Stroke::new(1.0, p.border));
                    ui.painter().hline(mid.x + gw / 2.0 + 10.0..=r.right(), mid.y, Stroke::new(1.0, p.border));
                    ui.painter().galley(mid - g.size() / 2.0, g, p.text_faint);
                });
                ui.add_space(8.0);
                let trimmed = text.trim_start();
                let lang = if trimmed.starts_with('{') || trimmed.starts_with('[') {
                    crate::highlight::Lang::Json
                } else if trimmed.starts_with('<') {
                    crate::highlight::Lang::Xml
                } else if trimmed.starts_with("openapi") || trimmed.starts_with("swagger") {
                    crate::highlight::Lang::Yaml
                } else {
                    crate::highlight::Lang::Text
                };
                let r = egui::ScrollArea::vertical()
                    .id_salt("import-scroll")
                    .max_height(fit_height(ui.ctx(), 520.0, 180.0))
                    .show(ui, |ui| widgets::code_editor(ui, "import-text", text, lang, None, 6, false))
                    .inner;
                if text.is_empty() {
                    ui.painter().text(
                        r.rect.left_top() + vec2(9.0, 7.0),
                        Align2::LEFT_TOP,
                        tr("A Postman or OpenAPI file, a HAR export or a cURL command…"),
                        theme::mono(12.0),
                        p.text_faint,
                    );
                }
                if r.changed() || (preview.is_none() && error.is_none() && !text.trim().is_empty()) {
                    match merpati_convert::import_auto(text) {
                        Ok(pr) => {
                            *preview = Some(pr);
                            *error = None;
                        }
                        Err(e) => {
                            *preview = None;
                            *error = Some(e.to_string());
                        }
                    }
                }
                if text.trim().is_empty() {
                    *preview = None;
                    if source.is_empty() {
                        *error = None;
                    }
                }
                if let Some(e) = error {
                    ui.add_space(10.0);
                    banner(ui, Tone::Err, &trf("This can't be imported: {}", &[&e]));
                }
                if let Some(pr) = preview {
                    ui.add_space(10.0);
                    import_summary(ui, text, pr);
                }
            });
            footer(ui, |ui| {
                let p = theme::pal(ui);
                ui.label(
                    RichText::new(tr("Imported items are added to this workspace.")).size(12.0).color(p.text_muted),
                );
                trailing(ui, |ui| {
                    let r = ui.add_enabled_ui(preview.is_some(), |ui| {
                        kit::primary_icon(ui, icons::DOWNLOAD_SIMPLE, tr("Import"))
                    });
                    if r.inner.clicked() {
                        do_import = true;
                    }
                    if kit::secondary(ui, None, tr("Cancel")).clicked() { Outcome::Close } else { Outcome::Keep }
                })
            })
        });
    if do_import && let Some(p) = preview.take() {
        let n = p.collections.len();
        for mut c in p.collections {
            if app.ws.data.collection(&c.id).is_some() {
                c.id = merpati_core::util::new_id();
            }
            let id = app.ws.add_collection(c);
            app.sidebar.expanded.insert(id);
        }
        for mut e in p.environments {
            if app.ws.data.environment(&e.id).is_some() {
                e.id = merpati_core::util::new_id();
            }
            app.ws.add_environment(e);
        }
        if !p.globals.is_empty() {
            for g in p.globals {
                app.ws.data.globals.retain(|x| x.key != g.key);
                app.ws.data.globals.push(g);
            }
            app.ws.mark_globals();
        }
        app.sidebar_tab = crate::app::SidebarTab::Collections;
        app.toast(trf("Imported {} collection(s)", &[&n]), false);
        return Outcome::Close;
    }
    out
}

/// "Ready to import" card: detected format, what will be added, warnings.
fn import_summary(ui: &mut Ui, text: &str, pr: &merpati_convert::Imported) {
    let p = theme::pal(ui);
    kit::card(ui).inner_margin(Margin::symmetric(14, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(icons::CHECK_CIRCLE).font(theme::icon_font(17.0)).color(p.ok));
            ui.label(RichText::new(tr("Ready to import")).font(theme::semibold(13.5)).color(p.text));
            if let Some(fmt) = merpati_convert::detect(text) {
                trailing(ui, |ui| kit::pill(ui, fmt.label(), p.accent_text));
            }
        });
        ui.add_space(6.0);
        let reqs: usize = pr.collections.iter().map(|c| c.request_count()).sum();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            let stats = [
                (icons::STACK, pr.collections.len(), "{} collection", "{} collections"),
                (icons::FILE, reqs, "{} request", "{} requests"),
                (icons::GLOBE, pr.environments.len(), "{} environment", "{} environments"),
                (icons::GLOBE_SIMPLE, pr.globals.len(), "{} global variable", "{} global variables"),
            ];
            for (i, (icon, n, one, many)) in stats.into_iter().enumerate() {
                if i >= 2 && n == 0 {
                    continue;
                }
                ui.label(RichText::new(format!("{icon}  {}", plural(n, one, many))).color(p.text_muted));
            }
        });
        if !pr.warnings.is_empty() {
            ui.add_space(8.0);
            for w in pr.warnings.iter().take(6) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(icons::WARNING).font(theme::icon_font(13.0)).color(p.warn));
                    ui.add(egui::Label::new(RichText::new(w).size(12.0).color(p.text_muted)).wrap());
                });
            }
            if pr.warnings.len() > 6 {
                ui.label(kit::hint(ui, &trf("{} more warnings", &[&(pr.warnings.len() - 6)])));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Code snippet
// ---------------------------------------------------------------------------

fn codegen(
    app: &mut MerpatiApp,
    ctx: &egui::Context,
    item: &RequestItem,
    collection: Option<&str>,
    lang: &mut Language,
    code: &mut String,
) -> Outcome {
    let mut regenerate = false;
    let height = fit_height(ctx, 200.0, 500.0);
    let copied_id = egui::Id::unique("codegen-copied");
    let now = ctx.input(|i| i.time);
    let copied_at: Option<f64> = ctx.data(|d| d.get_temp(copied_id));
    let copied = copied_at.is_some_and(|t| now - t < 1.6);
    if copied {
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
    }
    let out = Modal::new("codegen", 920.0)
        .title(tr("Code snippet"))
        .subtitle(item.name.clone())
        .icon(icons::CODE)
        .show(ctx, |ui| {
            let current = *lang;
            split(
                ui,
                height,
                220.0,
                |ui| {
                    egui::ScrollArea::vertical().id_salt("codegen-langs").auto_shrink([false; 2]).show(ui, |ui| {
                        egui::Frame::new().inner_margin(Margin::same(10)).show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            for l in Language::ALL {
                                let (name, variant) = l.label().split_once(" - ").unwrap_or((l.label(), ""));
                                let trail = (!variant.is_empty()).then_some(variant);
                                if nav_item(ui, None, name, trail, *lang == *l).clicked() && *lang != *l {
                                    *lang = *l;
                                    regenerate = true;
                                }
                            }
                        });
                    });
                },
                |ui| {
                    egui::Frame::new().inner_margin(Margin::same(16)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let hl = match current.syntax() {
                            "http" => crate::highlight::Lang::Text,
                            _ => crate::highlight::Lang::JavaScript,
                        };
                        let mut shown = code.clone();
                        let rows = ((height - 40.0) / 17.0) as usize;
                        egui::ScrollArea::vertical().id_salt("codegen-code").max_height(height - 32.0).show(ui, |ui| {
                            widgets::code_editor(ui, ("codegen", current.id()), &mut shown, hl, None, rows, true);
                        });
                    });
                },
            );
            footer(ui, |ui| {
                let p = theme::pal(ui);
                ui.label(RichText::new(icons::WARNING).font(theme::icon_font(14.0)).color(p.warn));
                ui.add(
                    egui::Label::new(
                        RichText::new(tr(
                            "Variables are resolved with the active environment; secrets appear in clear text.",
                        ))
                        .size(12.0)
                        .color(p.text_muted),
                    )
                    .truncate(),
                );
                trailing(ui, |ui| {
                    let (icon, label) = if copied { (icons::CHECK, tr("Copied")) } else { (icons::COPY, tr("Copy")) };
                    if kit::primary_icon(ui, icon, label).clicked() {
                        ui.ctx().copy_text(code.clone());
                        ui.ctx().data_mut(|d| d.insert_temp(copied_id, now));
                    }
                    if kit::secondary(ui, None, tr("Close")).clicked() { Outcome::Close } else { Outcome::Keep }
                })
            })
        });
    if regenerate {
        *code = codegen_item(app, item, collection, *lang);
    }
    out
}

// ---------------------------------------------------------------------------
// Save request
// ---------------------------------------------------------------------------

/// A row of the collection/folder picker.
fn tree_row(ui: &mut Ui, depth: usize, icon: &str, label: &str, selected: bool) -> Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    widgets::a11y_tab(&resp, label, selected);
    if ui.is_rect_visible(rect) {
        let radius = CornerRadius::same(theme::RADIUS);
        if selected {
            ui.painter().rect_filled(rect, radius, p.accent_soft);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, radius, p.hover);
        }
        let cy = rect.center().y;
        let x = rect.left() + 10.0 + 18.0 * depth as f32;
        let color = if selected { p.accent_text } else { p.text_muted };
        ui.painter().text(pos2(x + 8.0, cy), Align2::CENTER_CENTER, icon, theme::icon_font(15.0), color);
        let font = if depth == 0 { theme::medium(13.0) } else { FontId::proportional(13.0) };
        let g = line_galley(ui, label, font, rect.right() - x - 54.0);
        ui.painter().galley(pos2(x + 24.0, cy - g.size().y / 2.0), g, p.text);
        if selected {
            ui.painter().text(
                pos2(rect.right() - 16.0, cy),
                Align2::CENTER_CENTER,
                icons::CHECK,
                theme::icon_font(14.0),
                p.accent,
            );
        }
    }
    resp
}

#[allow(clippy::too_many_arguments)]
fn save_as(
    app: &mut MerpatiApp,
    ctx: &egui::Context,
    tab: u64,
    collection: &mut Option<Id>,
    folder: &mut Option<Id>,
    name: &mut String,
    named: &mut bool,
    new_collection: &mut String,
) -> Outcome {
    if !*named && let Some(i) = app.tab_index(tab) {
        *name = app.tabs[i].kind.scratch().map(|s| s.name.clone()).unwrap_or_default();
        *named = true;
    }
    let mut save = false;
    let mut create = false;
    // Where it goes, for the footer: "Collection › Folder".
    let target = collection.as_ref().and_then(|c| app.ws.data.collection(c)).map(|c| {
        let mut path = c.name.clone();
        if let Some(f) = folder.as_ref()
            && let Some(chain) = c.ancestors(f)
        {
            for a in chain {
                if let merpati_core::model::Ancestor::Folder(x) = a {
                    path.push_str(&format!(" › {}", x.name));
                }
            }
            if let Some(it) = c.find(f) {
                path.push_str(&format!(" › {}", it.name()));
            }
        }
        path
    });
    let valid = collection.is_some() && !name.trim().is_empty();
    let list_h = fit_height(ctx, 420.0, 260.0);
    let out =
        Modal::new("save-as", 520.0).title(tr("Save request")).icon(icons::FLOPPY_DISK).sticky().show(ctx, |ui| {
            body(ui, |ui| {
                field_label(ui, tr("Name"));
                let r = ui.add(input(name).desired_width(f32::INFINITY));
                widgets::a11y_name(&r, tr("Name"));
                if ui.memory(|m| m.focused().is_none()) {
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && valid {
                    save = true;
                }
                ui.add_space(12.0);
                field_label(ui, tr("Save to"));
                kit::card(ui).inner_margin(Margin::same(6)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt("save-tree")
                        .max_height(list_h)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            if app.ws.data.collections.is_empty() {
                                egui::Frame::new().inner_margin(Margin::symmetric(8, 10)).show(ui, |ui| {
                                    ui.label(
                                        RichText::new(tr("No collections yet. Create one below."))
                                            .color(theme::pal(ui).text_muted),
                                    );
                                });
                            }
                            for c in &app.ws.data.collections {
                                let selected = collection.as_deref() == Some(&c.id) && folder.is_none();
                                if tree_row(ui, 0, icons::STACK, &c.name, selected).clicked() {
                                    *collection = Some(c.id.clone());
                                    *folder = None;
                                }
                                fn folders(
                                    ui: &mut Ui,
                                    c: &merpati_core::model::Collection,
                                    items: &[Item],
                                    depth: usize,
                                    collection: &mut Option<Id>,
                                    folder: &mut Option<Id>,
                                ) {
                                    for i in items {
                                        if let Item::Folder(f) = i {
                                            let selected = folder.as_deref() == Some(&f.id);
                                            let icon = if selected { icons::FOLDER_OPEN } else { icons::FOLDER_SIMPLE };
                                            if tree_row(ui, depth, icon, &f.name, selected).clicked() {
                                                *collection = Some(c.id.clone());
                                                *folder = Some(f.id.clone());
                                            }
                                            folders(ui, c, &f.items, depth + 1, collection, folder);
                                        }
                                    }
                                }
                                folders(ui, c, &c.items, 1, collection, folder);
                            }
                        });
                });
                ui.add_space(8.0);
                ui.allocate_ui_with_layout(
                    vec2(ui.available_width(), 30.0),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        let ok = !new_collection.trim().is_empty();
                        let b = ui.add_enabled_ui(ok, |ui| {
                            kit::secondary(ui, Some(icons::FOLDER_PLUS), tr("Create collection"))
                        });
                        let w = ui.available_width();
                        let r = ui.add(input(new_collection).hint_text(tr("New collection name")).desired_width(w));
                        widgets::a11y_name(&r, tr("New collection name"));
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        if (b.inner.clicked() || enter) && ok {
                            create = true;
                        }
                    },
                );
            });
            footer(ui, |ui| {
                let p = theme::pal(ui);
                if let Some(t) = &target {
                    ui.label(RichText::new(icons::FOLDER_SIMPLE).font(theme::icon_font(14.0)).color(p.text_faint));
                    let w = (ui.available_width() - 190.0).max(40.0);
                    ui.allocate_ui_with_layout(vec2(w, 20.0), Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(t).size(12.0).color(p.text_muted)).truncate());
                    });
                }
                trailing(ui, |ui| {
                    if ui.add_enabled_ui(valid, |ui| kit::primary(ui, tr("Save"))).inner.clicked() {
                        save = true;
                    }
                    if kit::secondary(ui, None, tr("Cancel")).clicked() { Outcome::Close } else { Outcome::Keep }
                })
            })
        });
    if create {
        let id = app.new_collection(new_collection.trim());
        *collection = Some(id);
        *folder = None;
        new_collection.clear();
    }
    let out = if save { Outcome::Close } else { out };
    if save && let Some(c) = collection.clone() {
        if app.save_scratch_to(tab, &c, folder.as_deref(), name.trim()) {
            app.toast(tr("Saved"), false);
            // Saving was asked for while closing the tab.
            if app.close_after_save == Some(tab)
                && let Some(i) = app.tab_index(tab)
            {
                app.close_tab(i);
            }
        } else {
            app.toast(tr("The selected collection or folder no longer exists."), true);
        }
    }
    if matches!(out, Outcome::Close) {
        app.close_after_save = None;
    }
    out
}

// ---------------------------------------------------------------------------
// Alerts: unsaved changes, delete
// ---------------------------------------------------------------------------

/// The top of an alert: a tinted icon, a title and a sentence.
fn alert_body(ui: &mut Ui, icon: &str, color: Color32, title: &str, text: &str) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    egui::Frame::new().inner_margin(Margin { left: 22, right: 22, top: 22, bottom: 20 }).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 20.0, theme::soft(color, if dark { 0.18 } else { 0.12 }));
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon_font(20.0), color);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.add_space(1.0);
                ui.add(egui::Label::new(RichText::new(title).font(theme::semibold(15.5)).color(p.text)).wrap());
                ui.add(egui::Label::new(RichText::new(text).color(p.text_muted)).wrap());
            });
        });
    });
}

/// What to do with a tab whose unsaved work closing it would lose.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloseChoice {
    Save,
    Discard,
}

fn close_unsaved(app: &mut MerpatiApp, ctx: &egui::Context, tab: u64) -> Outcome {
    let Some(idx) = app.tab_index(tab) else { return Outcome::Close };
    let (name, _, _) = tab_title(&app.tabs[idx], &app.ws);
    let mut choice = None;
    // Enter saves and Escape cancels (the modal closes on Escape).
    let out = Modal::new("close-unsaved", 460.0).show(ctx, |ui| {
        let warn = theme::pal(ui).warn;
        alert_body(
            ui,
            icons::WARNING,
            warn,
            &trf("Save changes to “{}”?", &[&name]),
            tr("Your changes will be lost if you close the tab without saving them."),
        );
        footer(ui, |ui| {
            let discard = kit::button_ex(ui, kit::Kind::Danger, None, tr("Don't save"), false);
            widgets::a11y_name(&discard, &trf("Don't save \"{}\"", &[&name]));
            if discard.clicked() {
                choice = Some(CloseChoice::Discard);
            }
            trailing(ui, |ui| {
                let save = kit::primary(ui, tr("Save…")).on_hover_text(tr("Enter"));
                widgets::a11y_name(&save, &trf("Save \"{}\"…", &[&name]));
                widgets::a11y_shortcut(&save, "Enter");
                let cancel = kit::secondary(ui, None, tr("Cancel")).on_hover_text(tr("Esc"));
                widgets::a11y_name(&cancel, &trf("Cancel closing \"{}\"", &[&name]));
                widgets::a11y_shortcut(&cancel, "Escape");
                // Save… is the default button: focused unless another one is (Tab
                // moves between them, Enter/Space presses the focused one).
                let focus = ui.memory(|m| m.focused());
                let other = focus.is_some_and(|f| f == discard.id || f == cancel.id);
                if !other && focus != Some(save.id) {
                    save.request_focus();
                }
                if save.clicked() || (!other && ui.input(|i| i.key_pressed(Key::Enter))) {
                    choice = Some(CloseChoice::Save);
                }
                if cancel.clicked() { Outcome::Close } else { Outcome::Keep }
            })
        })
    });
    match choice {
        Some(CloseChoice::Discard) => app.close_tab(idx),
        Some(CloseChoice::Save) => {
            if let TabKind::Spec(t) = &mut app.tabs[idx].kind {
                match t.save() {
                    Ok(()) => app.close_tab(idx),
                    Err(e) => app.toast(trf("Save failed: {}", &[&e]), true),
                }
            } else {
                app.close_after_save = Some(tab);
                app.dialog = Some(Dialog::save_as(tab, &app.ws));
            }
        }
        None => return out,
    }
    Outcome::Close
}

fn confirm(app: &mut MerpatiApp, ctx: &egui::Context, target: &DeleteTarget, name: &str) -> Outcome {
    let mut yes = false;
    let (title, text) = match target {
        DeleteTarget::Item { item: None, .. } => (
            trf("Delete collection “{}”?", &[&name]),
            tr("The collection, its folders and its requests are removed from the workspace folder."),
        ),
        DeleteTarget::Item { collection, item: Some(id) } => {
            let folder = app
                .ws
                .data
                .collection(collection)
                .and_then(|c| c.find(id))
                .is_some_and(|i| matches!(i, Item::Folder(_)));
            if folder {
                (
                    trf("Delete folder “{}”?", &[&name]),
                    tr("The folder and the requests in it are removed from the workspace folder."),
                )
            } else {
                (trf("Delete request “{}”?", &[&name]), tr("The request is removed from the workspace folder."))
            }
        }
        DeleteTarget::Environment(_) => (
            trf("Delete environment “{}”?", &[&name]),
            tr("The environment and its variables are removed from the workspace folder."),
        ),
    };
    let out = Modal::new("confirm", 460.0).show(ctx, |ui| {
        let err = theme::pal(ui).err;
        alert_body(ui, icons::TRASH, err, &title, text);
        footer(ui, |ui| {
            trailing(ui, |ui| {
                if kit::button_ex(ui, kit::Kind::Danger, Some(icons::TRASH), tr("Delete"), false).clicked() {
                    yes = true;
                }
                let cancel = kit::secondary(ui, None, tr("Cancel"));
                if cancel.clicked() || yes { Outcome::Close } else { Outcome::Keep }
            })
        })
    });
    if yes {
        match target {
            DeleteTarget::Item { collection, item: None } => app.ws.delete_collection(collection),
            DeleteTarget::Item { collection, item: Some(id) } => {
                if let Some(c) = app.ws.data.collection_mut(collection) {
                    c.remove(id);
                }
                let cid = collection.clone();
                app.ws.mark_collection(&cid);
            }
            DeleteTarget::Environment(id) => app.ws.delete_environment(id),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// New workspace, prompt
// ---------------------------------------------------------------------------

fn new_workspace(app: &mut MerpatiApp, ctx: &egui::Context, name: &mut String, path: &mut Option<PathBuf>) -> Outcome {
    let mut create = false;
    let existing = path.as_ref().is_some_and(|p| merpati_core::storage::WorkspaceStore::is_workspace(p));
    let out = Modal::new("new-ws", 520.0)
        .title(tr("New workspace"))
        .subtitle(tr("A folder of plain files you can keep in Git."))
        .icon(icons::FOLDER_PLUS)
        .sticky()
        .show(ctx, |ui| {
            body(ui, |ui| {
                let p = theme::pal(ui);
                field_label(ui, tr("Name"));
                let r = ui.add(input(name).desired_width(f32::INFINITY));
                widgets::a11y_name(&r, tr("Name"));
                ui.add_space(12.0);
                field_label(ui, tr("Location"));
                ui.allocate_ui_with_layout(
                    vec2(ui.available_width(), 30.0),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        if kit::secondary(ui, Some(icons::FOLDER_OPEN), tr("Choose folder…")).clicked()
                            && let Some(dir) = rfd::FileDialog::new().pick_folder()
                        {
                            *path = Some(dir);
                        }
                        let w = ui.available_width();
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            egui::Frame::new()
                                .fill(side_fill(ui))
                                .stroke(Stroke::new(1.0, p.border))
                                .corner_radius(CornerRadius::same(theme::RADIUS))
                                .inner_margin(Margin::symmetric(8, 6))
                                .show(ui, |ui| {
                                    ui.set_width(w - 18.0);
                                    match path {
                                        Some(pth) => ui.add(
                                            egui::Label::new(
                                                RichText::new(pth.display().to_string())
                                                    .font(theme::mono(12.0))
                                                    .color(p.text),
                                            )
                                            .truncate(),
                                        ),
                                        None => ui.label(RichText::new(tr("No folder selected")).color(p.text_faint)),
                                    };
                                });
                        });
                    },
                );
                ui.add_space(10.0);
                if existing {
                    banner(ui, Tone::Info, tr("This folder already holds a workspace; it will be opened as it is."));
                } else {
                    ui.label(kit::hint(
                        ui,
                        tr("A workspace is a folder of plain JSON files; put it in a Git repository to share it."),
                    ));
                }
            });
            footer(ui, |ui| {
                trailing(ui, |ui| {
                    let label = if existing { tr("Open") } else { tr("Create") };
                    if ui.add_enabled_ui(path.is_some(), |ui| kit::primary(ui, label)).inner.clicked() {
                        create = true;
                    }
                    if kit::secondary(ui, None, tr("Cancel")).clicked() { Outcome::Close } else { Outcome::Keep }
                })
            })
        });
    if create && let Some(p) = path.clone() {
        if existing {
            app.open_workspace(p, None);
        } else {
            app.open_workspace(p, Some(name.clone()));
        }
        return Outcome::Close;
    }
    out
}

fn prompt(app: &mut MerpatiApp, ctx: &egui::Context, kind: &Prompt, text: &mut String) -> Outcome {
    let (title, icon, action) = match kind {
        Prompt::NewCollection => (tr("New Collection"), icons::STACK, tr("Create")),
        Prompt::NewEnvironment => (tr("New Environment"), icons::GLOBE_SIMPLE, tr("Create")),
        Prompt::RenameEnvironment(_) => (tr("Rename environment"), icons::PENCIL_SIMPLE, tr("Rename")),
        Prompt::Fork(_) => (tr("Fork collection"), icons::GIT_FORK, tr("Fork")),
    };
    let mut ok = false;
    let modal = Modal::new("prompt", 440.0).title(title).icon(icon).sticky();
    let valid = !text.trim().is_empty();
    let out = modal.show(ctx, |ui| {
        body(ui, |ui| {
            field_label(ui, tr("Name"));
            let id = egui::Id::unique("prompt-text");
            let first = !ui.memory(|m| m.has_focus(id));
            let r = ui.add(input(text).id(id).desired_width(f32::INFINITY));
            widgets::a11y_name(&r, tr("Name"));
            r.request_focus();
            if first && let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), id) {
                // Start with the whole name selected, ready to be typed over.
                let all = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(text.chars().count()),
                );
                state.cursor.set_char_range(Some(all));
                state.store(ui.ctx(), id);
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && valid {
                ok = true;
            }
            if let Prompt::Fork(_) = kind {
                ui.add_space(2.0);
                ui.label(kit::hint(ui, tr("A copy that can pull changes from the original and merge back.")));
            }
        });
        footer(ui, |ui| {
            trailing(ui, |ui| {
                if ui.add_enabled_ui(valid, |ui| kit::primary(ui, action)).inner.clicked() {
                    ok = true;
                }
                if kit::secondary(ui, None, tr("Cancel")).clicked() || ok { Outcome::Close } else { Outcome::Keep }
            })
        })
    });
    if ok && !text.trim().is_empty() {
        let name = text.trim().to_string();
        match kind {
            Prompt::NewCollection => {
                app.new_collection(&name);
                app.sidebar_tab = crate::app::SidebarTab::Collections;
            }
            Prompt::NewEnvironment => app.new_environment(&name),
            Prompt::Fork(id) => crate::fork_ui::create(app, id, &name),
            Prompt::RenameEnvironment(id) => {
                if let Some(e) = app.ws.data.environments.iter_mut().find(|e| &e.id == id) {
                    e.name = name;
                }
                let id = id.clone();
                app.ws.mark_env(&id);
            }
        }
        return Outcome::Close;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::model::HttpRequest;

    fn scratch(app: &mut MerpatiApp, name: &str) {
        let item = RequestItem::new(name, HttpRequest::new("GET", format!("http://{name}.test")));
        app.add_tab(TabKind::Request(Box::new(crate::request_tab::RequestTab::scratch(item))));
    }

    fn active_title(app: &MerpatiApp) -> String {
        tab_title(&app.tabs[app.active], &app.ws).0
    }

    /// Tabs that are not written to the session (webhook listener) and saved
    /// requests deleted meanwhile must not shift the restored active tab.
    #[test]
    fn session_restores_the_active_tab() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        app.add_tab(TabKind::Webhook(Box::default()));
        scratch(&mut app, "a");
        scratch(&mut app, "b");
        app.active = 1;
        save_session(&mut app);
        app.tabs.clear();
        restore_session(&mut app);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(active_title(&app), "a");

        // A saved request before the active tab is deleted before restoring.
        let cid = app.new_collection("C");
        let gone = RequestItem::new("gone", HttpRequest::new("GET", "http://gone.test"));
        let gone_id = gone.id.clone();
        app.ws.data.collection_mut(&cid).unwrap().items.push(Item::Request(gone));
        app.tabs.clear();
        app.active = 0;
        app.open_item(ItemRef { collection: cid.clone(), item: gone_id.clone() });
        scratch(&mut app, "c");
        scratch(&mut app, "d");
        app.active = 1;
        save_session(&mut app);
        app.ws.data.collection_mut(&cid).unwrap().remove(&gone_id);
        app.tabs.clear();
        restore_session(&mut app);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(active_title(&app), "c");
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    /// The Save dialog takes the tab's name once; emptying the field to type
    /// another name used to fill it in again on the next frame.
    #[test]
    fn save_as_fills_in_the_name_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        scratch(&mut app, "first");
        app.dialog = Some(Dialog::save_as(app.tabs[0].id, &app.ws));
        let name = |app: &mut MerpatiApp| match &mut app.dialog {
            Some(Dialog::SaveAs { name, .. }) => std::mem::take(name),
            _ => panic!("no Save dialog"),
        };
        app.draw_frame(Vec::new());
        assert_eq!(name(&mut app), "first");
        app.draw_frame(Vec::new());
        app.draw_frame(Vec::new());
        assert_eq!(name(&mut app), "");
    }

    /// The prompt for closing unsaved work: its buttons are named for screen
    /// readers, Escape keeps the tab open, Enter saves.
    #[test]
    fn close_unsaved_prompt_keys_and_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        scratch(&mut app, "edited");
        let id = app.tabs[0].id;
        app.dialog = Some(Dialog::CloseUnsaved { tab: id });
        // The first frame of a new window only measures it.
        app.draw_frame(Vec::new());
        let labels = app.draw_frame(Vec::new()).labels;
        for name in ["Save \"edited\"…", "Don't save \"edited\"", "Cancel closing \"edited\""] {
            assert!(labels.iter().any(|l| l == name), "no {name:?} in {labels:?}");
        }
        app.draw_frame(vec![key(egui::Key::Escape)]);
        assert!(app.dialog.is_none());
        assert_eq!(app.tabs.len(), 1);

        // A request that was never saved asks where to save it, then closes.
        app.dialog = Some(Dialog::CloseUnsaved { tab: id });
        app.draw_frame(Vec::new());
        app.draw_frame(vec![key(egui::Key::Enter)]);
        assert!(matches!(app.dialog, Some(Dialog::SaveAs { tab, .. }) if tab == id));
        assert_eq!(app.close_after_save, Some(id));
        assert_eq!(app.tabs.len(), 1);

        // A spec is written and closed right away.
        app.dialog = None;
        let path = dir.path().join("api.yaml");
        std::fs::write(&path, crate::spec_tab::OPENAPI_TEMPLATE).unwrap();
        crate::spec_tab::open_spec(&mut app, path.clone());
        let spec = app.tabs[app.active].id;
        app.dialog = Some(Dialog::CloseUnsaved { tab: spec });
        if let TabKind::Spec(t) = &mut app.tabs[app.active].kind {
            t.restore_unsaved(format!("{}# edited\n", crate::spec_tab::OPENAPI_TEMPLATE));
        }
        app.draw_frame(vec![key(egui::Key::Enter)]);
        assert!(app.tab_index(spec).is_none());
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("# edited\n"));
    }
}
