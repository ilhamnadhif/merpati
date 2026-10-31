//! Shared pieces of the protocol tabs (WebSocket, MQTT, Socket.IO, gRPC, MCP,
//! AI) and the local server tabs (webhook, mock, capture): where the request
//! lives, the header and connection bar, status pills, variable resolution
//! and the message log.

use std::collections::VecDeque;
use std::sync::Arc;

use egui::{Align2, Color32, CornerRadius, Rect, RichText, Sense, Stroke, StrokeKind, Ui, vec2};

use merpati_core::model::RequestItem;
use merpati_core::variables::{Resolver, VariableScopes};

use crate::app::{ItemRef, MerpatiApp};
use crate::i18n::{tr, trf};
use crate::{icons, kit, theme, widgets};

/// A request either saved in a collection or kept in the tab (unsaved).
#[derive(Default)]
pub struct ItemSlot {
    pub location: Option<ItemRef>,
    pub scratch: Option<RequestItem>,
}

impl ItemSlot {
    pub fn load(&self, app: &MerpatiApp) -> Option<RequestItem> {
        match &self.location {
            Some(loc) => app.ws.data.collection(&loc.collection)?.find_request(&loc.item).cloned(),
            None => self.scratch.clone(),
        }
    }

    /// Writes `item` back if it differs from `original`.
    pub fn store(&mut self, app: &mut MerpatiApp, original: &RequestItem, item: RequestItem) {
        if &item == original {
            return;
        }
        match &self.location {
            Some(loc) => {
                let cid = loc.collection.clone();
                if let Some(target) =
                    app.ws.data.collection_mut(&loc.collection).and_then(|c| c.find_request_mut(&loc.item))
                {
                    *target = item;
                    app.ws.mark_collection(&cid);
                }
            }
            None => self.scratch = Some(item),
        }
    }

    pub fn collection_id(&self) -> Option<String> {
        self.location.as_ref().map(|l| l.collection.clone())
    }
}

// ---------------------------------------------------------------------------
// Header, connection bar and status
// ---------------------------------------------------------------------------

/// Color of a protocol badge (`WS`, `SIO`, `MQTT`, `gRPC`, `MCP`, `AI`).
pub fn proto_color(badge: &str, dark: bool) -> Color32 {
    theme::method_color(badge, dark)
}

/// State of a connection, a call or a local server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conn {
    /// Not connected / stopped.
    Idle,
    /// Connecting, starting, running a call.
    Busy,
    /// Connected / listening.
    Live,
    /// The last attempt failed.
    Failed,
}

/// A rounded status label with a dot (a spinner while busy): `● Connected`.
pub fn status_pill(ui: &mut Ui, state: Conn, text: &str) -> egui::Response {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let color = match state {
        Conn::Idle => p.text_muted,
        Conn::Busy => p.info,
        Conn::Live => p.ok,
        Conn::Failed => p.err,
    };
    let size = ui.style().text_styles.get(&egui::TextStyle::Small).map(|f| f.size).unwrap_or(11.5);
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::medium(size), color);
    let h = 22.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 32.0, h), Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, text));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let fill = theme::soft(color, if dark { 0.16 } else { 0.10 });
        painter.rect_filled(rect, CornerRadius::same((h / 2.0) as u8), fill);
        let c = egui::pos2(rect.left() + 13.0, rect.center().y);
        match state {
            Conn::Busy => {
                let t = ui.input(|i| i.time) as f32;
                let start = t * std::f32::consts::TAU * 0.9;
                let points: Vec<egui::Pos2> = (0..=16)
                    .map(|i| {
                        let a = start + i as f32 / 16.0 * std::f32::consts::TAU * 0.72;
                        c + 4.5 * vec2(a.cos(), a.sin())
                    })
                    .collect();
                painter.line(points, Stroke::new(1.6, color));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
            }
            Conn::Idle => {
                painter.circle_stroke(c, 3.5, Stroke::new(1.4, color));
            }
            Conn::Live => {
                painter.circle_filled(c, 6.0, theme::soft(color, 0.22));
                painter.circle_filled(c, 3.5, color);
            }
            Conn::Failed => {
                painter.circle_filled(c, 3.5, color);
            }
        }
        painter.galley(egui::pos2(rect.left() + 24.0, rect.center().y - galley.size().y / 2.0), galley, color);
    }
    resp
}

/// `12 s`, `4:05`, `1:02:03`: how long a connection has been open.
pub fn uptime(since: std::time::Instant) -> String {
    let s = since.elapsed().as_secs();
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{}:{:02}", s / 60, s % 60)
    } else {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    }
}

/// The standard status pill of a connection, with how long it has been open
/// (redrawn every second while connected).
pub fn conn_status(ui: &mut Ui, state: Conn, since: Option<std::time::Instant>) -> egui::Response {
    let text = match state {
        Conn::Idle => tr("Disconnected").to_string(),
        Conn::Busy => tr("Connecting…").to_string(),
        Conn::Live => match since {
            Some(s) => {
                ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
                format!("{} · {}", tr("Connected"), uptime(s))
            }
            None => tr("Connected").to_string(),
        },
        Conn::Failed => tr("Connection failed").to_string(),
    };
    status_pill(ui, state, &text)
}

/// Breadcrumb, name (double-click to rename), status widgets and Save, like
/// the HTTP request header.
pub fn header(
    ui: &mut Ui,
    app: &mut MerpatiApp,
    tab_id: u64,
    slot: &ItemSlot,
    item: &mut RequestItem,
    status: impl FnOnce(&mut Ui),
) {
    let p = theme::pal(ui);
    let crumbs: Vec<String> = match &slot.location {
        Some(loc) => app
            .ws
            .data
            .collection(&loc.collection)
            .and_then(|c| c.ancestors(&loc.item))
            .map(|chain| chain.iter().map(|a| a.name().to_string()).collect())
            .unwrap_or_default(),
        None => vec![tr("Unsaved").to_string()],
    };
    let mut save = false;
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 30.0),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            let label = if slot.location.is_none() { tr("Save…") } else { tr("Save") };
            if kit::secondary(ui, Some(icons::FLOPPY_DISK), label).on_hover_text("⌘S").clicked() {
                save = true;
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                // Narrow windows: the name and status only (the path is in the tooltip).
                let path = crumbs.join(" › ");
                if ui.available_width() > 620.0 {
                    for part in &crumbs {
                        ui.add(egui::Label::new(RichText::new(part.as_str()).color(p.text_muted)).selectable(false));
                        ui.label(RichText::new(icons::CARET_RIGHT).font(theme::icon_font(11.0)).color(p.text_faint));
                    }
                }
                let rename_id = egui::Id::unique(("proto-rename", tab_id));
                let renaming: Option<String> = ui.data(|d| d.get_temp(rename_id));
                match renaming {
                    Some(mut buf) => {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut buf)
                                .desired_width(260.0)
                                .font(egui::FontId::proportional(14.0)),
                        );
                        r.request_focus();
                        if r.lost_focus() {
                            let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
                            if !cancelled && !buf.trim().is_empty() {
                                item.name = buf.trim().to_string();
                            }
                            ui.data_mut(|d| d.remove::<String>(rename_id));
                        } else {
                            ui.data_mut(|d| d.insert_temp(rename_id, buf));
                        }
                    }
                    None => {
                        let r = ui.add(
                            egui::Label::new(RichText::new(&item.name).font(theme::semibold(15.0)).color(p.text))
                                .sense(Sense::click())
                                .selectable(false)
                                .truncate(),
                        );
                        let hint = format!("{path} › {}\n{}", item.name, tr("Double-click to rename"));
                        if r.on_hover_text(hint).double_clicked() {
                            ui.data_mut(|d| d.insert_temp(rename_id, item.name.clone()));
                        }
                    }
                }
                ui.add_space(4.0);
                status(ui);
            });
        },
    );
    if save {
        if slot.location.is_none() {
            app.dialog = Some(crate::dialogs::Dialog::save_as(tab_id, &app.ws));
        } else {
            app.ws.flush();
            app.toast(tr("Saved"), false);
        }
    }
    ui.add_space(6.0);
}

/// The grouped bar under the header: `[badge | fields…] [button]`, like the
/// method + URL + Send group of HTTP requests. `fields` returns whether one
/// of its inputs has focus (the group then shows a focus ring); `button`
/// draws the action in a slot of `button_w` and its result is returned.
pub fn connect_bar<B>(
    ui: &mut Ui,
    badge: &str,
    button_w: f32,
    fields: impl FnOnce(&mut Ui) -> bool,
    button: impl FnOnce(&mut Ui) -> B,
) -> B {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let group_w = (ui.available_width() - button_w - 8.0).max(200.0);
        let frame = egui::Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border_strong))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(2, 2));
        let group = frame.show(ui, |ui| {
            ui.set_width(group_w - 4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let color = proto_color(badge, dark);
                let galley = ui.painter().layout_no_wrap(badge.to_string(), theme::semibold(12.5), color);
                let (rect, _) = ui.allocate_exact_size(vec2(galley.size().x + 26.0, 26.0), Sense::hover());
                ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
                kit::vdivider(ui, 22.0);
                fields(ui)
            })
            .inner
        });
        if group.inner {
            crate::kit::focus_ring(ui, group.response.rect, 8);
        }
        let h = group.response.rect.height();
        // Right-aligned, like the Save button above it.
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), h),
            egui::Layout::right_to_left(egui::Align::Center),
            button,
        )
        .inner
    })
    .inner
}

/// Makes combo boxes inside a [`connect_bar`] frameless.
pub fn frameless(ui: &mut Ui) {
    let w = &mut ui.visuals_mut().widgets;
    for v in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
        v.bg_stroke = Stroke::NONE;
    }
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
}

/// A frameless single-line field for a [`connect_bar`] (a namespace, a
/// working directory…).
pub fn bar_field(ui: &mut Ui, id: &str, text: &mut String, hint: &str, width: f32) -> egui::Response {
    let h = ui.available_height().clamp(20.0, 34.0);
    let r = ui.add(
        egui::TextEdit::singleline(text)
            .id_salt(id)
            .hint_text(hint)
            .desired_width(width)
            .font(egui::TextStyle::Monospace)
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::symmetric(6, 4))
            .min_size(vec2(0.0, h))
            .align(egui::Align2::LEFT_CENTER),
    );
    widgets::a11y_name(&r, hint);
    r
}

/// Connect / Cancel / Disconnect for a [`connect_bar`]. Returns the clicked
/// action.
pub fn connect_button(ui: &mut Ui, state: Conn) -> Option<ConnAction> {
    match state {
        Conn::Idle | Conn::Failed => {
            let r = kit::primary_icon(ui, icons::PLUGS_CONNECTED, tr("Connect"));
            r.on_hover_text(tr("Or press ↩ in the address field")).clicked().then_some(ConnAction::Connect)
        }
        Conn::Busy => {
            let r = kit::secondary(ui, Some(icons::X), tr("Cancel"));
            r.clicked().then_some(ConnAction::Cancel)
        }
        Conn::Live => {
            let r = kit::secondary(ui, Some(icons::PLUGS), tr("Disconnect"));
            r.clicked().then_some(ConnAction::Disconnect)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnAction {
    Connect,
    Cancel,
    Disconnect,
}

/// The title row of a tool view (webhook listener, mock server, capture):
/// an icon in a soft square, a title, a status pill, and actions on the right.
pub fn view_title(
    ui: &mut Ui,
    icon: &str,
    title: &str,
    subtitle: Option<&str>,
    status: impl FnOnce(&mut Ui),
    actions: impl FnOnce(&mut Ui),
) {
    let p = theme::pal(ui);
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 36.0),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            actions(ui);
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let dark = ui.visuals().dark_mode;
                let (rect, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(9),
                    theme::soft(p.accent, if dark { 0.16 } else { 0.10 }),
                );
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon_font(20.0), p.accent_text);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.label(RichText::new(title).font(theme::semibold(16.0)).color(p.text));
                        status(ui);
                    });
                    if let Some(s) = subtitle {
                        ui.add(egui::Label::new(RichText::new(s).color(p.text_muted).small()).truncate())
                            .on_hover_text(s);
                    }
                });
            });
        },
    );
}

/// A local address with a copy button: `🌐 http://127.0.0.1:4010  ⧉`.
pub fn address_pill(ui: &mut Ui, url: &str) -> bool {
    let p = theme::pal(ui);
    let mut copied = false;
    egui::Frame::new()
        .fill(p.subtle)
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(egui::Margin { left: 10, right: 2, top: 1, bottom: 1 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(icons::GLOBE_SIMPLE).font(theme::icon_font(14.0)).color(p.text_muted));
                ui.add(egui::Label::new(RichText::new(url).monospace().color(p.text)).selectable(true));
                if kit::icon_button_ex(ui, icons::COPY, tr("Copy address"), false, 24.0).clicked() {
                    ui.ctx().copy_text(url.to_string());
                    copied = true;
                }
            });
        });
    copied
}

/// A method label on a soft fill of its color.
pub fn method_pill(ui: &mut Ui, method: &str) -> egui::Response {
    let dark = ui.visuals().dark_mode;
    let short = match method {
        "DELETE" => "DEL",
        "OPTIONS" => "OPT",
        "" => "—",
        m => m,
    };
    kit::pill(ui, short, theme::method_color(method, dark))
}

/// One row of a request table (webhook listener, mock server, capture).
pub struct ReqRow<'a> {
    pub time: &'a str,
    pub method: &'a str,
    /// Status text and color (`200`, `failed`, `tunnel`).
    pub status: Option<(String, Color32)>,
    pub path: &'a str,
    /// Muted text after the path (query, host).
    pub detail: &'a str,
    /// Right-aligned muted text (size, duration).
    pub right: String,
}

const REQ_ROW_H: f32 = 30.0;
const COL_TIME: f32 = 72.0;
const COL_METHOD: f32 = 62.0;
const COL_STATUS: f32 = 56.0;

/// Column titles over [`request_row`]s.
pub fn request_header(ui: &mut Ui, check: bool, status: bool, path: &str, right: &str) {
    let p = theme::pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let painter = ui.painter();
    let font = theme::medium(11.0);
    let mut x = rect.left() + 8.0 + if check { 24.0 } else { 0.0 };
    let y = rect.center().y;
    let col = |text: &str, x: f32| {
        painter.text(egui::pos2(x, y), Align2::LEFT_CENTER, text, font.clone(), p.text_faint);
    };
    col(tr("Time"), x);
    x += COL_TIME;
    col(tr("Method"), x);
    x += COL_METHOD;
    if status {
        col(tr("Status"), x);
        x += COL_STATUS;
    }
    col(path, x);
    painter.text(egui::pos2(rect.right() - 10.0, y), Align2::RIGHT_CENTER, right, font, p.text_faint);
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.border));
}

/// A painted request row with optional checkbox; clicking the row selects it.
pub fn request_row(ui: &mut Ui, row: &ReqRow<'_>, selected: bool, check: Option<&mut bool>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), REQ_ROW_H), Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::Button, true, selected, format!("{} {}", row.method, row.path))
    });
    let has_check = check.is_some();
    let mut check_box = None;
    if let Some(on) = check {
        let r = Rect::from_center_size(egui::pos2(rect.left() + 17.0, rect.center().y), vec2(16.0, 16.0));
        let c = ui.interact(r.expand(3.0), resp.id.with("check"), Sense::click());
        widgets::a11y_name(&c, tr("Select"));
        if c.clicked() {
            *on = !*on;
        }
        check_box = Some((r, *on, c.hovered()));
    }
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let painter = ui.painter_at(rect.intersect(ui.clip_rect()));
    let bg = rect.shrink2(vec2(0.0, 1.0));
    if selected {
        painter.rect_filled(bg, CornerRadius::same(6), p.accent_soft);
    } else if resp.hovered() {
        painter.rect_filled(bg, CornerRadius::same(6), p.faint_row());
    }
    if let Some((r, on, hovered)) = check_box {
        if on {
            painter.rect_filled(r, CornerRadius::same(4), p.accent);
            painter.text(r.center(), Align2::CENTER_CENTER, icons::CHECK, theme::icon_font(11.0), p.on_accent);
        } else {
            let stroke = if hovered { p.text_muted } else { p.border_strong };
            painter.rect(r, CornerRadius::same(4), p.surface, Stroke::new(1.0, stroke), StrokeKind::Inside);
        }
    }
    let cy = rect.center().y;
    let mut x = rect.left() + 8.0 + if has_check { 24.0 } else { 0.0 };
    let mono = ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5);
    painter.text(egui::pos2(x, cy), Align2::LEFT_CENTER, row.time, theme::mono(mono - 1.0), p.text_faint);
    x += COL_TIME;
    let method = match row.method {
        "DELETE" => "DEL",
        "OPTIONS" => "OPT",
        "" => "CONNECT",
        m => m,
    };
    let mc = theme::method_color(row.method, dark);
    let g = painter.layout_no_wrap(method.to_string(), theme::semibold(10.5), mc);
    let pill = Rect::from_min_size(egui::pos2(x, cy - 9.0), vec2(g.size().x + 12.0, 18.0));
    painter.rect_filled(pill, CornerRadius::same(9), theme::soft(mc, if dark { 0.18 } else { 0.11 }));
    painter.galley(pill.center() - g.size() / 2.0, g, mc);
    x += COL_METHOD;
    if let Some((text, color)) = &row.status {
        let g = painter.layout_no_wrap(text.clone(), theme::medium(11.0), *color);
        let pill = Rect::from_min_size(egui::pos2(x, cy - 9.0), vec2(g.size().x + 12.0, 18.0));
        painter.rect_filled(pill, CornerRadius::same(9), theme::soft(*color, if dark { 0.18 } else { 0.11 }));
        painter.galley(pill.center() - g.size() / 2.0, g, *color);
        x += COL_STATUS;
    }
    let mut right = rect.right() - 10.0;
    if !row.right.is_empty() {
        let g = painter.layout_no_wrap(row.right.clone(), egui::FontId::proportional(mono - 1.5), p.text_faint);
        right -= g.size().x;
        painter.galley(egui::pos2(right, cy - g.size().y / 2.0), g, p.text_faint);
        right -= 12.0;
    }
    if right > x + 10.0 {
        let mut job = egui::text::LayoutJob::default();
        job.append(row.path, 0.0, egui::TextFormat::simple(theme::mono(mono - 0.5), p.text));
        if !row.detail.is_empty() {
            job.append(row.detail, 0.0, egui::TextFormat::simple(theme::mono(mono - 0.5), p.text_muted));
        }
        job.wrap = egui::text::TextWrapping {
            max_width: right - x,
            max_rows: 1,
            break_anywhere: true,
            overflow_character: Some('…'),
        };
        let g = painter.layout_job(job);
        painter.galley(egui::pos2(x, cy - g.size().y / 2.0), g, p.text);
    }
    resp
}

/// `14:02:31` in local time.
pub fn clock_s(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

/// A banner for an error: an icon, a title, the cause and an optional action.
pub fn error_banner(ui: &mut Ui, title: &str, cause: &str, action: impl FnOnce(&mut Ui)) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    egui::Frame::new()
        .fill(theme::soft(p.err, if dark { 0.12 } else { 0.07 }))
        .stroke(Stroke::new(1.0, theme::soft(p.err, 0.35)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::WARNING_CIRCLE).font(theme::icon_font(18.0)).color(p.err));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(title).font(theme::semibold(13.0)).color(p.err));
                    ui.add(egui::Label::new(RichText::new(cause).color(p.text)).wrap());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), action);
            });
        });
}

/// A notice for something waiting for the user (server requests,
/// acknowledgements): accent-tinted card with an icon and a title.
pub fn notice_card<R>(ui: &mut Ui, icon: &str, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    egui::Frame::new()
        .fill(theme::soft(p.accent, if dark { 0.10 } else { 0.05 }))
        .stroke(Stroke::new(1.0, theme::soft(p.accent, 0.35)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).font(theme::icon_font(16.0)).color(p.accent_text));
                ui.label(RichText::new(title).font(theme::semibold(13.5)).color(p.text));
            });
            ui.add_space(4.0);
            add(ui)
        })
        .inner
}

/// A two-column form: muted labels on the left, controls on the right.
pub fn form_grid(id: impl std::hash::Hash + std::fmt::Debug) -> egui::Grid {
    egui::Grid::new(id).num_columns(2).spacing([18.0, 10.0]).min_col_width(150.0)
}

/// One group of controls in a [`flow_row`] (kept on one line).
pub type Chunk<'a> = Box<dyn FnOnce(&mut Ui) + 'a>;

/// A form row: a fixed-width muted label, then groups of controls that flow
/// onto more lines when the window is narrow. Groups are measured on the
/// previous frame, so a group never splits and never widens the page.
pub fn flow_row(ui: &mut Ui, label: &str, label_w: f32, chunks: Vec<Chunk<'_>>) {
    let id = ui.make_persistent_id(("flow-row", label));
    let widths: Vec<f32> = ui.data(|d| d.get_temp(id).unwrap_or_default());
    let mut measured = vec![0.0; chunks.len()];
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(label_w, 26.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_width(label_w);
            form_label(ui, label);
        });
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let avail = ui.available_width();
            let gap = ui.spacing().item_spacing.x + 8.0;
            let mut chunks = chunks.into_iter().enumerate().peekable();
            while chunks.peek().is_some() {
                ui.horizontal(|ui| {
                    ui.set_min_height(26.0);
                    let mut used = 0.0;
                    while let Some((i, _)) = chunks.peek() {
                        let w = widths.get(*i).copied().unwrap_or(0.0);
                        if used > 0.0 && used + w > avail {
                            break;
                        }
                        let Some((i, chunk)) = chunks.next() else { break };
                        if used > 0.0 {
                            ui.add_space(8.0);
                        }
                        let r = ui.horizontal(|ui| chunk(ui)).response;
                        measured[i] = r.rect.width();
                        used += r.rect.width() + gap;
                    }
                });
            }
        });
    });
    if measured != widths {
        ui.data_mut(|d| d.insert_temp(id, measured));
        ui.ctx().request_repaint();
    }
}

/// The label of a [`form_grid`] row.
pub fn form_label(ui: &mut Ui, text: &str) {
    let p = theme::pal(ui);
    ui.add(egui::Label::new(RichText::new(text).color(p.text_muted)).selectable(false));
}

/// A section title with an optional hint under it.
pub fn section(ui: &mut Ui, title: &str, hint: Option<&str>) {
    ui.label(kit::section_title(title));
    if let Some(h) = hint {
        ui.label(kit::hint(ui, h));
    }
    ui.add_space(2.0);
}

/// Key/value rows (headers, trailers, metadata) as a bordered table.
pub fn kv_view<'a>(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    rows: impl Iterator<Item = (&'a str, &'a str)>,
    empty: &str,
) {
    let p = theme::pal(ui);
    let rows: Vec<(&str, &str)> = rows.collect();
    if rows.is_empty() {
        ui.add_space(8.0);
        ui.label(RichText::new(empty).color(p.text_muted));
        return;
    }
    egui::Frame::new()
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(0, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let key_w = (ui.available_width() * 0.32).clamp(110.0, 260.0);
            for (i, (k, v)) in rows.iter().enumerate() {
                if i > 0 {
                    let y = ui.cursor().top();
                    ui.painter().hline(ui.max_rect().x_range(), y, Stroke::new(1.0, p.border));
                }
                ui.push_id((&id, i), |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.allocate_ui_with_layout(
                            vec2(key_w, 22.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.set_min_width(key_w);
                                ui.add(egui::Label::new(RichText::new(*k).monospace().color(p.text_muted)).truncate())
                                    .on_hover_text(*k);
                            },
                        );
                        ui.add(egui::Label::new(RichText::new(*v).monospace().color(p.text)).wrap().selectable(true));
                    });
                });
            }
        });
}

/// What was clicked in a [`saved_list`] row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowAction {
    Use,
    Send,
    Remove,
}

/// Saved messages or calls as a bordered list: `row` draws the left part of
/// each row (name, tags, summary); the actions (load into the composer, send
/// now when `can_send`, remove) are on the right.
pub fn saved_list<T>(
    ui: &mut Ui,
    id: &str,
    items: &mut [T],
    can_send: bool,
    send_label: &str,
    mut row: impl FnMut(&mut Ui, &mut T),
) -> Option<(usize, RowAction)> {
    let p = theme::pal(ui);
    let mut action = None;
    egui::Frame::new()
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, item) in items.iter_mut().enumerate() {
                if i > 0 {
                    let y = ui.cursor().top();
                    ui.painter().hline(ui.max_rect().expand2(vec2(8.0, 0.0)).x_range(), y, Stroke::new(1.0, p.border));
                }
                ui.push_id((id, i), |ui| {
                    ui.allocate_ui_with_layout(
                        vec2(ui.available_width(), 36.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if kit::icon_button(ui, icons::TRASH, tr("Remove")).clicked() {
                                action = Some((i, RowAction::Remove));
                            }
                            let send = ui.add_enabled_ui(can_send, |ui| {
                                kit::icon_button(ui, icons::PAPER_PLANE_RIGHT, send_label)
                            });
                            if send.inner.on_disabled_hover_text(tr("Connect first")).clicked() {
                                action = Some((i, RowAction::Send));
                            }
                            if kit::icon_button(ui, icons::ARROW_BEND_UP_LEFT, tr("Load into the composer")).clicked() {
                                action = Some((i, RowAction::Use));
                            }
                            ui.add_space(6.0);
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                row(ui, item);
                            });
                        },
                    );
                });
            }
        });
    action
}

/// "Save message" for a composer toolbar (icon only when space is short).
pub fn save_message_button(ui: &mut Ui, enabled: bool) -> bool {
    ui.add_enabled_ui(enabled, |ui| {
        let r = if ui.available_width() > 560.0 {
            kit::ghost(ui, Some(icons::BOOKMARK_SIMPLE), tr("Save message"))
        } else {
            kit::icon_button(ui, icons::BOOKMARK_SIMPLE, tr("Save message"))
        };
        r.on_hover_text(tr("Keep this message with the request")).clicked()
    })
    .inner
}

/// A name field for list rows: frameless until hovered or focused.
pub fn name_field(ui: &mut Ui, text: &mut String, width: f32) -> egui::Response {
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.weak_bg_fill = Color32::TRANSPARENT;
        ui.visuals_mut().text_edit_bg_color = Some(Color32::TRANSPARENT);
        ui.add(
            egui::TextEdit::singleline(text)
                .desired_width(width)
                .font(theme::medium(13.0))
                .margin(egui::Margin::symmetric(4, 3))
                .hint_text(tr("Name")),
        )
    })
    .inner
}

/// A section that opens and closes on click, with a caret and a title.
pub fn collapsible(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    title: &str,
    badge: Option<&str>,
    add: impl FnOnce(&mut Ui),
) {
    let p = theme::pal(ui);
    let id = ui.make_persistent_id(id);
    let mut open: bool = ui.data(|d| d.get_temp(id).unwrap_or(false));
    let r = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let caret = if open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
            ui.label(RichText::new(caret).font(theme::icon_font(12.0)).color(p.text_muted));
            ui.label(kit::section_title(title));
            if let Some(b) = badge {
                kit::pill(ui, b, p.accent_text);
            }
        })
        .response
        .interact(Sense::click());
    widgets::a11y_button(&r, title);
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if r.clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(id, open));
    }
    if open {
        ui.add_space(4.0);
        ui.indent(id.with("body"), add);
    }
}

/// A placeholder painted in an empty code editor (they have no hint text).
pub fn editor_placeholder(ui: &Ui, editor: &egui::Response, text: &str, hint: &str) {
    if !text.is_empty() || editor.has_focus() {
        return;
    }
    let p = theme::pal(ui);
    let size = ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5);
    let pad = ui.spacing().button_padding;
    ui.painter().text(
        editor.rect.left_top() + vec2(pad.x + 1.0, pad.y + 2.0),
        Align2::LEFT_TOP,
        hint,
        theme::mono(size),
        p.text_faint,
    );
}

/// The section tabs of a view; they scroll sideways instead of widening the
/// page when the window is too narrow for all of them.
pub fn section_tabs<T: PartialEq + Copy>(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    current: &mut T,
    items: &[(T, String)],
) {
    egui::ScrollArea::horizontal()
        .id_salt(id)
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| widgets::sub_tabs(ui, current, items));
}

/// A list on the left of a list/detail view, with a hairline between them.
pub fn list_panel(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, width: f32, add: impl FnOnce(&mut Ui)) {
    let rect = egui::Panel::left(egui::Id::unique(id))
        .exact_size(width)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 21, top: 0, bottom: 0 }))
        .show(ui, add)
        .response
        .rect;
    let p = theme::pal(ui);
    ui.painter().vline(rect.right() - 10.5, rect.y_range(), Stroke::new(1.0, p.border));
}

/// A code editor filling the space left; longer text scrolls inside it
/// instead of pushing the panes below away.
pub fn fill_editor(
    ui: &mut Ui,
    id: &str,
    text: &mut String,
    lang: crate::highlight::Lang,
    vars: &widgets::VarCtx,
    min_rows: usize,
) -> egui::Response {
    let rows = fill_rows(ui, min_rows);
    egui::ScrollArea::vertical()
        .id_salt((id, "scroll"))
        .max_height(ui.available_height().max(60.0))
        .auto_shrink([false, true])
        .show(ui, |ui| widgets::code_editor(ui, id, text, lang, Some(vars), rows, false))
        .inner
}

/// Rows of a code editor that fill the space left (at least `min`).
pub fn fill_rows(ui: &Ui, min: usize) -> usize {
    let size = ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5);
    (((ui.available_height() - 14.0) / (size * 1.36)).floor() as usize).clamp(min, 60)
}

/// A request pane on top and a resizable pane below (the message log, the
/// response), split like the HTTP request editor. The split is kept per `id`.
pub fn split_view(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    default: f32,
    top: impl FnOnce(&mut Ui),
    bottom: impl FnOnce(&mut Ui),
) {
    let id = ui.make_persistent_id(id);
    let mut split: f32 = ui.data(|d| d.get_temp(id).unwrap_or(default));
    let avail = ui.available_rect_before_wrap();
    let h = (avail.height() * split).clamp(140.0, (avail.height() - 120.0).max(140.0));
    let rect = egui::Panel::bottom(id.with("panel"))
        .exact_size(h)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: 0, right: 0, top: 10, bottom: 0 }))
        .show(ui, bottom)
        .response
        .rect;
    // The draggable divider on top of the lower pane.
    let handle = Rect::from_min_max(rect.left_top() - vec2(0.0, 3.0), egui::pos2(rect.right(), rect.top() + 4.0));
    let r = ui.interact(handle, id.with("handle"), Sense::drag());
    let hovered = r.hovered() || r.dragged();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    if r.dragged() && avail.height() > 0.0 {
        split = (split - r.drag_delta().y / avail.height()).clamp(0.15, 0.85);
        ui.data_mut(|d| d.insert_temp(id, split));
    }
    if r.double_clicked() {
        ui.data_mut(|d| d.insert_temp(id, default));
    }
    let p = theme::pal(ui);
    let stroke = if hovered { Stroke::new(2.0, p.accent) } else { Stroke::new(1.0, p.border) };
    ui.painter().hline(rect.x_range(), rect.top(), stroke);
    top(ui);
}

/// Resolves `{{variables}}` with the scopes of `collection` (plus vault),
/// owning its data so it can be moved into async tasks.
pub fn resolver(app: &MerpatiApp, collection: Option<&str>) -> impl Fn(&str) -> String + Send + Sync + 'static {
    let scopes: VariableScopes = app.ws.scopes_for(collection);
    let vault: Arc<dyn merpati_core::variables::VaultLookup> = app.ws.vault();
    move |s: &str| Resolver::new(&scopes).with_vault(vault.as_ref()).resolve(s)
}

/// True when ⇧⌘↩ (the main action of a protocol tab: send, publish, emit,
/// invoke, call) was pressed; ⌘↩ alone belongs to the app.
pub fn send_shortcut(ui: &Ui) -> bool {
    ui.input_mut(|i| {
        i.consume_shortcut(&egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Key::Enter,
        ))
    })
}

/// The main action button of a composer with its shortcut hint before it
/// (in a right-to-left row). Check [`send_shortcut`] separately so the
/// shortcut works from every section.
pub fn send_button(ui: &mut Ui, icon: &str, label: &str, enabled: bool, disabled_hint: &str) -> bool {
    let r = ui.add_enabled_ui(enabled, |ui| kit::primary_icon(ui, icon, label)).inner;
    let r = if enabled { r.on_hover_text("⇧⌘↩") } else { r.on_disabled_hover_text(disabled_hint) };
    if enabled {
        kit::kbd(ui, "⇧⌘↩");
    }
    r.clicked()
}

// ---------------------------------------------------------------------------
// Message log
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
    System,
}

pub struct LogLine {
    pub dir: Dir,
    /// Topic, event name or method.
    pub title: String,
    pub text: String,
    pub at_ms: i64,
    pub size: usize,
    pub error: bool,
    /// A binary payload, shown as text (hex or base64).
    pub binary: bool,
    /// Short facts shown after the title (`QoS 1 · retained`).
    pub meta: String,
}

/// Which lines the log shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFilter {
    #[default]
    All,
    Sent,
    Received,
}

/// A protocol tab's message log: the latest `cap` lines, one row each in a
/// list that lays out only the visible rows (logs hold thousands), and the
/// selected message in full below it.
pub struct Log {
    /// Add lines with [`Log::push`]/[`Log::push_line`].
    pub lines: VecDeque<LogLine>,
    pub search: String,
    pub filter: LogFilter,
    pub selected: Option<usize>,
    pub cap: usize,
    /// Title and explanation shown while the log is empty.
    pub empty: (&'static str, &'static str),
    /// Title of the toolbar (translated when drawn); empty for none.
    pub title: &'static str,
    /// Lines ever pushed; the first line kept is number `pushed - lines.len()`.
    pushed: u64,
    found: Found,
    /// Rows the list had last frame.
    rows_shown: usize,
    /// The selected message as shown (pretty JSON), by line number.
    shown: Option<(u64, String, bool)>,
    /// Scroll offset and height of the list last frame (keyboard navigation).
    view: (f32, f32),
}

/// The lines matching a search (by number), found incrementally so a long
/// log is not searched again every frame.
#[derive(Default)]
struct Found {
    query: String,
    filter: LogFilter,
    numbers: VecDeque<u64>,
    /// The next line number to check.
    next: u64,
}

impl Default for Log {
    fn default() -> Self {
        Log {
            lines: VecDeque::new(),
            search: String::new(),
            filter: LogFilter::All,
            selected: None,
            cap: 5000,
            empty: ("No messages yet", "Connect and send a message to see the conversation here."),
            title: "Messages",
            pushed: 0,
            found: Found::default(),
            rows_shown: 0,
            shown: None,
            view: (0.0, 0.0),
        }
    }
}

/// Incoming messages kept for on-message scripts until the tab is drawn
/// again (a tab in the background is not), so busy streams stay bounded.
pub const MAX_INBOX: usize = 5000;

pub fn queue_message(inbox: &mut VecDeque<serde_json::Value>, m: serde_json::Value) {
    inbox.push_back(m);
    if inbox.len() > MAX_INBOX {
        inbox.pop_front();
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

const ROW_H: f32 = 28.0;

impl Log {
    /// A log with its own empty-state text.
    pub fn with_empty(title: &'static str, body: &'static str) -> Log {
        Log { empty: (title, body), ..Default::default() }
    }

    pub fn push(&mut self, dir: Dir, title: impl Into<String>, text: impl Into<String>, at_ms: i64) {
        let text = text.into();
        let size = text.len();
        self.push_line(LogLine {
            dir,
            title: title.into(),
            text,
            at_ms,
            size,
            error: false,
            binary: false,
            meta: String::new(),
        });
    }

    pub fn push_line(&mut self, line: LogLine) {
        self.lines.push_back(line);
        self.pushed += 1;
        if self.lines.len() > self.cap {
            self.lines.pop_front();
            self.selected = self.selected.and_then(|s| s.checked_sub(1));
        }
    }

    pub fn system(&mut self, text: impl Into<String>) {
        self.push(Dir::System, "", text, now_ms());
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.push(Dir::System, "", text, now_ms());
        if let Some(l) = self.lines.back_mut() {
            l.error = true;
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.selected = None;
        self.shown = None;
    }

    /// Number of the first line kept.
    fn first(&self) -> u64 {
        self.pushed.saturating_sub(self.lines.len() as u64)
    }

    /// Indexes of the lines matching `search` and `filter`, or `None` when
    /// every line is shown.
    fn matching(&mut self) -> Option<Vec<usize>> {
        let q = self.search.to_lowercase();
        if q.is_empty() && self.filter == LogFilter::All {
            return None;
        }
        let first = self.first();
        let filter = self.filter;
        let found = &mut self.found;
        if found.query != q || found.filter != filter {
            *found = Found { query: q, filter, numbers: VecDeque::new(), next: first };
        }
        while found.numbers.front().is_some_and(|&n| n < first) {
            found.numbers.pop_front();
        }
        for n in found.next.max(first)..self.pushed {
            let Some(l) = self.lines.get((n - first) as usize) else { break };
            let dir_ok = match filter {
                LogFilter::All => true,
                LogFilter::Sent => l.dir == Dir::Out,
                LogFilter::Received => l.dir == Dir::In,
            };
            let text_ok = found.query.is_empty()
                || l.text.to_lowercase().contains(&found.query)
                || l.title.to_lowercase().contains(&found.query);
            if dir_ok && text_ok {
                found.numbers.push_back(n);
            }
        }
        found.next = self.pushed;
        Some(found.numbers.iter().map(|n| (n - first) as usize).collect())
    }

    pub fn ui(&mut self, ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug) {
        self.ui_with(ui, id, |_| {});
    }

    /// The log, with `status` widgets after its title.
    pub fn ui_with(&mut self, ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, status: impl FnOnce(&mut Ui)) {
        let p = theme::pal(ui);
        // Toolbar: title and count, then filter, search and clear.
        ui.horizontal(|ui| {
            ui.set_height(28.0);
            if !self.title.is_empty() {
                ui.label(kit::section_title(tr(self.title)));
                if !self.lines.is_empty() {
                    count_badge(ui, self.lines.len());
                }
            }
            status(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let clear =
                    ui.add_enabled_ui(!self.lines.is_empty(), |ui| kit::ghost(ui, Some(icons::TRASH), tr("Clear")));
                if clear.inner.on_hover_text(tr("Clear the log")).clicked() {
                    self.clear();
                }
                let w = (ui.available_width() - 250.0).clamp(110.0, 200.0);
                kit::search_field(ui, (&id, "search"), &mut self.search, tr("Search messages"), w);
                let all = tr("All");
                let sent = format!("{} {}", icons::ARROW_UP, tr("Sent"));
                let received = format!("{} {}", icons::ARROW_DOWN, tr("Received"));
                ui.push_id((&id, "filter"), |ui| {
                    kit::segmented(
                        ui,
                        &mut self.filter,
                        &[(LogFilter::All, all), (LogFilter::Sent, &sent), (LogFilter::Received, &received)],
                    );
                });
            });
        });
        ui.add_space(4.0);
        let rows = self.matching();
        let count = rows.as_ref().map_or(self.lines.len(), Vec::len);
        if count < self.rows_shown {
            // Fewer rows (a search): the list scrolls back into range at the
            // end of this frame and shows them in the next.
            ui.ctx().request_repaint();
        }
        self.rows_shown = count;
        if self.lines.is_empty() {
            kit::empty_state(ui, icons::CHATS, tr(self.empty.0), tr(self.empty.1), |_| {});
            return;
        }
        if count == 0 {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(icons::MAGNIFYING_GLASS).font(theme::icon_font(22.0)).color(p.text_faint));
                ui.label(RichText::new(tr("No messages match")).color(p.text_muted));
            });
            return;
        }
        // Keyboard: ↑/↓ move the selection, Esc closes it.
        let row_of = |i: usize| rows.as_ref().map_or(Some(i), |r| r.iter().position(|&x| x == i));
        let mut moved = None;
        if ui.memory(|m| m.focused().is_none())
            && let Some(sel) = self.selected
            && let Some(row) = row_of(sel)
        {
            let (up, down, esc) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.key_pressed(egui::Key::Escape),
                )
            });
            let target = if up {
                row.checked_sub(1)
            } else if down {
                Some(row + 1).filter(|r| *r < count)
            } else {
                None
            };
            if let Some(t) = target {
                self.selected = Some(rows.as_ref().map_or(t, |r| r[t]));
                moved = Some(t);
            }
            if esc {
                self.selected = None;
            }
        }
        let mut area = egui::ScrollArea::vertical().id_salt(&id).stick_to_bottom(true).auto_shrink([false; 2]);
        if self.selected.is_some_and(|i| i < self.lines.len()) {
            // Room for the selected message below.
            area = area.max_height((ui.available_height() * 0.5).max(ROW_H * 4.0));
        }
        if let Some(t) = moved {
            let (offset, height) = self.view;
            // Rows are `item_spacing.y` apart.
            let top = t as f32 * (ROW_H + ui.spacing().item_spacing.y);
            if top < offset {
                area = area.vertical_scroll_offset(top);
            } else if top + ROW_H > offset + height {
                area = area.vertical_scroll_offset(top + ROW_H - height);
            }
        }
        let mut clicked = None;
        let selected = self.selected;
        let out = area.show_rows(ui, ROW_H, count, |ui, range| {
            for row in range {
                let i = rows.as_ref().map_or(row, |r| r[row]);
                if let Some(l) = self.lines.get(i)
                    && log_row(ui, l, selected == Some(i)).clicked()
                {
                    clicked = Some(i);
                }
            }
        });
        self.view = (out.state.clamped_offset().y, out.inner_rect.height());
        if let Some(i) = clicked {
            self.selected = if self.selected == Some(i) { None } else { Some(i) };
        }
        let Some(sel) = self.selected.filter(|&i| i < self.lines.len()) else { return };
        let number = self.first() + sel as u64;
        if self.shown.as_ref().is_none_or(|(n, _, _)| *n != number) {
            let text = &self.lines[sel].text;
            let (shown, json) = match serde_json::from_str::<serde_json::Value>(text) {
                Ok(v) if (v.is_object() || v.is_array()) && text.len() < 2_000_000 => {
                    (serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.clone()), true)
                }
                _ => (text.clone(), false),
            };
            self.shown = Some((number, shown, json));
        }
        let l = &self.lines[sel];
        let Some((_, shown, json)) = &self.shown else { return };
        ui.add_space(6.0);
        let mut close = false;
        ui.horizontal(|ui| {
            ui.set_height(28.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            let (icon, color, label) = dir_look(&p, l);
            dir_dot(ui, icon, color);
            ui.label(RichText::new(label).font(theme::medium(13.0)).color(p.text));
            ui.label(RichText::new(clock(l.at_ms)).monospace().color(p.text_muted));
            if !l.title.is_empty() {
                kit::pill(ui, &crate::dialogs::ellipsize(&l.title, 60), p.text_muted);
            }
            if !l.meta.is_empty() {
                ui.label(RichText::new(&l.meta).small().color(p.text_muted));
            }
            if l.size > 0 && l.dir != Dir::System {
                ui.label(RichText::new(size_text(l)).small().color(p.text_muted));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = kit::icon_button(ui, icons::X, tr("Close (Esc)")).clicked();
                if kit::icon_button(ui, icons::COPY, tr("Copy message")).clicked() {
                    ui.ctx().copy_text(shown.clone());
                }
            });
        });
        egui::ScrollArea::vertical().id_salt((&id, "message")).auto_shrink([false; 2]).show(ui, |ui| {
            if *json {
                text_view(ui, (&id, "message-text", number), shown);
            } else {
                egui::Frame::new()
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(theme::RADIUS))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let mut rt = RichText::new(shown.as_str()).monospace();
                        if l.error {
                            rt = rt.color(p.err);
                        }
                        ui.add(egui::Label::new(rt).wrap().selectable(true));
                    });
            }
        });
        if close {
            self.selected = None;
        }
    }
}

/// A small count badge (like the ones in tab labels).
pub fn count_badge(ui: &mut Ui, n: usize) {
    let p = theme::pal(ui);
    let size = ui.style().text_styles.get(&egui::TextStyle::Small).map(|f| f.size).unwrap_or(11.5);
    let g = ui.painter().layout_no_wrap(n.to_string(), theme::medium(size - 0.5), p.text_muted);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 12.0, 17.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(8), p.subtle);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, p.text_muted);
}

/// `14:02:31.123` in local time.
fn clock(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S%.3f").to_string())
        .unwrap_or_default()
}

fn size_text(l: &LogLine) -> String {
    let size = merpati_core::util::human_bytes(l.size as u64);
    if l.binary { format!("{size} · bin") } else { size }
}

/// Icon, color and spoken direction of a log line.
fn dir_look(p: &theme::Palette, l: &LogLine) -> (&'static str, Color32, &'static str) {
    match l.dir {
        Dir::In => (icons::ARROW_DOWN, p.ok, tr("Received")),
        Dir::Out => (icons::ARROW_UP, p.info, tr("Sent")),
        Dir::System if l.error => (icons::WARNING_CIRCLE, p.err, tr("Error")),
        Dir::System => (icons::INFO, p.text_muted, tr("Event")),
    }
}

/// The direction icon in a soft circle.
fn dir_dot(ui: &mut Ui, icon: &str, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 10.0, theme::soft(color, 0.14));
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon_font(12.0), color);
}

/// One row of the log (painted, for speed): direction, time, title, the
/// start of the message and its size. Clicking selects it.
fn log_row(ui: &mut Ui, l: &LogLine, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let preview = preview_text(&l.text);
    response.widget_info(|| {
        let name = if l.title.is_empty() { preview.clone() } else { format!("{} {preview}", l.title) };
        egui::WidgetInfo::selected(egui::Role::Button, true, selected, name)
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let painter = ui.painter_at(rect.intersect(ui.clip_rect()));
    let bg = rect.shrink2(vec2(0.0, 1.0));
    if selected {
        painter.rect_filled(bg, CornerRadius::same(6), p.accent_soft);
    } else if response.hovered() {
        painter.rect_filled(bg, CornerRadius::same(6), p.faint_row());
    } else if l.error {
        painter.rect_filled(bg, CornerRadius::same(6), theme::soft(p.err, if dark { 0.08 } else { 0.05 }));
    }
    let cy = rect.center().y;
    let mut x = rect.left() + 8.0;
    let (icon, color, _) = dir_look(&p, l);
    painter.circle_filled(egui::pos2(x + 9.0, cy), 9.0, theme::soft(color, if selected { 0.22 } else { 0.14 }));
    painter.text(egui::pos2(x + 9.0, cy), Align2::CENTER_CENTER, icon, theme::icon_font(11.5), color);
    x += 26.0;
    let mono = ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5);
    let t = painter.layout_no_wrap(clock(l.at_ms), theme::mono(mono - 1.0), p.text_faint);
    painter.galley(egui::pos2(x, cy - t.size().y / 2.0), t.clone(), p.text_faint);
    x += t.size().x + 12.0;
    // Size on the right.
    let mut right = rect.right() - 10.0;
    if l.size > 0 && l.dir != Dir::System {
        let s = painter.layout_no_wrap(size_text(l), egui::FontId::proportional(mono - 1.5), p.text_faint);
        right -= s.size().x;
        painter.galley(egui::pos2(right, cy - s.size().y / 2.0), s, p.text_faint);
        right -= 14.0;
    }
    if !l.title.is_empty() {
        let max = ((right - x) * 0.4).clamp(40.0, 240.0);
        let g = truncated(&painter, &l.title, theme::mono(mono - 1.0), p.text, max);
        let pill = Rect::from_min_size(egui::pos2(x, cy - 10.0), vec2(g.size().x + 14.0, 20.0));
        painter.rect_filled(pill, CornerRadius::same(10), if selected { p.surface } else { p.subtle });
        painter.galley(egui::pos2(pill.left() + 7.0, cy - g.size().y / 2.0), g, p.text);
        x = pill.right() + 8.0;
    }
    if !l.meta.is_empty() {
        let g = painter.layout_no_wrap(l.meta.clone(), egui::FontId::proportional(mono - 1.5), p.text_muted);
        if x + g.size().x < right - 40.0 {
            painter.galley(egui::pos2(x, cy - g.size().y / 2.0), g.clone(), p.text_muted);
            x += g.size().x + 10.0;
        }
    }
    if right > x + 8.0 {
        let color = if l.error {
            p.err
        } else if l.dir == Dir::System {
            p.text_muted
        } else {
            p.text
        };
        let font = if l.dir == Dir::System { egui::FontId::proportional(mono) } else { theme::mono(mono) };
        let g = truncated(&painter, &preview, font, color, right - x);
        painter.galley(egui::pos2(x, cy - g.size().y / 2.0), g, color);
    }
    response
}

/// The start of a message on one line: control characters become spaces
/// and runs of spaces (pretty JSON indentation) collapse into one.
fn preview_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(400));
    let mut space = false;
    for c in text.chars() {
        if out.len() >= 400 {
            break;
        }
        if c.is_whitespace() || c.is_control() {
            if !space && !out.is_empty() {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

/// One line of text cut to `max` width with an ellipsis.
fn truncated(painter: &egui::Painter, text: &str, font: egui::FontId, color: Color32, max: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping {
        max_width: max.max(10.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    painter.layout_job(job)
}

/// Pretty JSON when the payload is JSON, otherwise text (lossy) or hex.
pub fn payload_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) if v.is_object() || v.is_array() => {
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| s.to_string())
            }
            _ => s.to_string(),
        },
        Err(_) => bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
    }
}

/// Username/password fields stored as Basic auth (MQTT credentials).
pub fn basic_credentials(ui: &mut Ui, auth: &mut merpati_core::auth::Auth, vars: &widgets::VarCtx) {
    use merpati_core::auth::{Auth, AuthType};
    let mut on = auth.kind == AuthType::Basic;
    if ui.checkbox(&mut on, tr("Use username and password")).changed() {
        *auth = if on { Auth::basic("", "") } else { Auth::default() };
    }
    if auth.kind != AuthType::Basic {
        return;
    }
    let mut user = auth.get_string("username");
    let mut pass = auth.get_string("password");
    ui.add_space(4.0);
    form_grid("mqtt-credentials").show(ui, |ui| {
        form_label(ui, tr("Username"));
        if widgets::var_line(ui, "mqtt-user", &mut user, vars, "", 260.0).changed() {
            auth.set("username", user.clone());
        }
        ui.end_row();
        form_label(ui, tr("Password"));
        let r = if merpati_core::variables::is_pure_reference(&pass) {
            widgets::var_line(ui, "mqtt-pass", &mut pass, vars, "", 260.0)
        } else {
            widgets::secret_field(ui, "mqtt-pass", &mut pass, 286.0)
        };
        if r.changed() {
            auth.set("password", pass.clone());
        }
        ui.end_row();
    });
}

/// Read-only, selectable text; JSON keeps its indentation and is highlighted.
pub fn text_view(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, text: &str) {
    let is_json = matches!(serde_json::from_str::<serde_json::Value>(text), Ok(v) if v.is_object() || v.is_array());
    let lang = if is_json { crate::highlight::Lang::Json } else { crate::highlight::Lang::Text };
    let mut t = text.to_string();
    widgets::code_editor(ui, id, &mut t, lang, None, 1, true);
}

// ---------------------------------------------------------------------------
// Protocol scripts (FR-S7)
// ---------------------------------------------------------------------------

use merpati_core::script::{ProtocolContext, ScriptEvent, ScriptJob, ScriptOutcome, TestResult};

/// One finished protocol script run, delivered to its tab.
pub struct ScriptRun {
    pub event: ScriptEvent,
    pub outcome: ScriptOutcome,
    pub before: VariableScopes,
    pub collection: Option<String>,
    /// Environment active when the script started.
    pub env: Option<String>,
    pub name: String,
}

/// Test results and script errors a protocol tab keeps (the latest; on-message
/// scripts of a busy stream add some for every message), like the console.
pub const MAX_SCRIPT_RESULTS: usize = 5000;

/// Script editor selection, test results and the pending pre-request result
/// of a protocol tab.
pub struct ScriptState {
    pub editing: ScriptEvent,
    pub tests: VecDeque<TestResult>,
    pub errors: VecDeque<String>,
    /// Pre-request finished: the request JSON to use, or `None` when the
    /// script called `pm.execution.skipRequest()`.
    pre: Option<Option<serde_json::Value>>,
    pub waiting: bool,
}

impl Default for ScriptState {
    fn default() -> Self {
        ScriptState {
            editing: ScriptEvent::PreRequest,
            tests: VecDeque::new(),
            errors: VecDeque::new(),
            pre: None,
            waiting: false,
        }
    }
}

impl ScriptState {
    pub fn clear_results(&mut self) {
        self.tests.clear();
        self.errors.clear();
    }

    pub fn on_run(&mut self, run: &ScriptRun) {
        self.tests.extend(run.outcome.tests.iter().cloned());
        self.errors.extend(run.outcome.errors.iter().map(|e| format!("{}: {}", e.location, e.message)));
        let extra = self.tests.len().saturating_sub(MAX_SCRIPT_RESULTS);
        self.tests.drain(..extra);
        let extra = self.errors.len().saturating_sub(MAX_SCRIPT_RESULTS);
        self.errors.drain(..extra);
        if run.event == ScriptEvent::PreRequest {
            self.waiting = false;
            self.pre = Some(if run.outcome.skip_request { None } else { run.outcome.protocol_request.clone() });
        }
    }

    /// The pre-request result, once (see [`ScriptState::pre`]).
    pub fn take_pre(&mut self) -> Option<Option<serde_json::Value>> {
        self.pre.take()
    }
}

fn script_sources(
    app: &MerpatiApp,
    slot: &ItemSlot,
    item: &RequestItem,
    event: ScriptEvent,
) -> Vec<merpati_core::script::ScriptSource> {
    let collection = slot.location.as_ref().and_then(|l| app.ws.data.collection(&l.collection));
    let chain = collection.and_then(|c| c.ancestors(&item.id)).unwrap_or_default();
    merpati_runner::pipeline::script_chain(&chain, &item.scripts, &item.name, event)
}

/// Runs the item's scripts for `event` (collection and folder scripts first)
/// on a worker thread; the result arrives as [`crate::app::Msg::ProtoScript`].
/// Returns false when there is nothing to run.
pub fn run_scripts(
    app: &MerpatiApp,
    tab_id: u64,
    slot: &ItemSlot,
    item: &RequestItem,
    event: ScriptEvent,
    ctx: ProtocolContext,
) -> bool {
    let scripts = script_sources(app, slot, item, event);
    if scripts.is_empty() {
        return false;
    }
    let collection = slot.collection_id();
    let before = app.ws.scopes_for(collection.as_deref());
    let env = app.ws.data.active_environment.clone();
    let services = app.services();
    let mut job = ScriptJob::for_protocol(event, scripts, ctx);
    job.variables = before.clone();
    job.info.request_name = item.name.clone();
    job.info.request_id = item.id.clone();
    job.info.iteration_count = 1;
    job.info.collection_name =
        collection.as_deref().and_then(|c| app.ws.data.collection(c)).map(|c| c.name.clone()).unwrap_or_default();
    job.packages = services.packages.clone();
    job.timeout = services.script_timeout;
    job.http = Some(Arc::new(merpati_runner::script_http::PipelineHttp::new(services.clone())));
    let engine = services.scripts.clone();
    let tx = app.tx.clone();
    let ctx = app.ctx.clone();
    let name = item.name.clone();
    app.rt.spawn_blocking(move || {
        let outcome = engine.run(job);
        let run = ScriptRun { event, outcome, before, collection, env, name };
        let _ = tx.send(crate::app::Msg::ProtoScript { tab: tab_id, run: Box::new(run) });
        ctx.request_repaint();
    });
    true
}

/// The tab label of the Scripts section: `Scripts (n)` with the number of
/// scripts written.
pub fn scripts_label(scripts: &merpati_core::model::Scripts) -> String {
    let n = [&scripts.pre_request, &scripts.post_response, &scripts.on_message]
        .iter()
        .filter(|s| !s.trim().is_empty())
        .count();
    format!("{}{}", tr("Scripts"), widgets::count_label(n))
}

/// Script editors for the given events plus the latest test results.
pub fn scripts_ui(
    ui: &mut Ui,
    scripts: &mut merpati_core::model::Scripts,
    events: &[(ScriptEvent, &str)],
    state: &mut ScriptState,
    vars: &widgets::VarCtx,
) {
    let p = theme::pal(ui);
    if !events.iter().any(|(e, _)| *e == state.editing) {
        state.editing = events[0].0;
    }
    ui.horizontal(|ui| {
        let labels: Vec<String> = events
            .iter()
            .map(|(event, label)| {
                let dot = if scripts.code(*event).trim().is_empty() { "" } else { " •" };
                format!("{}{dot}", tr(label))
            })
            .collect();
        let items: Vec<(ScriptEvent, &str)> = events.iter().zip(&labels).map(|((e, _), l)| (*e, l.as_str())).collect();
        kit::segmented(ui, &mut state.editing, &items);
        ui.label(kit::hint(ui, tr("JavaScript with pm.request, pm.response and pm.message")));
    });
    ui.add_space(4.0);
    if !state.tests.is_empty() || !state.errors.is_empty() {
        let passed = state.tests.iter().filter(|t| t.passed && !t.skipped).count();
        let failed = state.tests.iter().filter(|t| !t.passed && !t.skipped).count();
        ui.horizontal(|ui| {
            ui.label(kit::section_title(tr("Test results")));
            kit::pill(ui, &format!("{} {}", icons::CHECK, trf("{} passed", &[&passed])), p.ok);
            if failed > 0 {
                kit::pill(ui, &format!("{} {}", icons::X, trf("{} failed", &[&failed])), p.err);
            }
            if !state.errors.is_empty() {
                kit::pill(ui, &trf("{} errors", &[&state.errors.len()]), p.err);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if kit::ghost(ui, Some(icons::TRASH), tr("Clear")).clicked() {
                    state.clear_results();
                }
            });
        });
        egui::ScrollArea::vertical().id_salt("proto-tests").max_height(140.0).show(ui, |ui| {
            for t in &state.tests {
                let (icon, color) = if t.skipped {
                    (icons::MINUS, p.text_muted)
                } else if t.passed {
                    (icons::CHECK_CIRCLE, p.ok)
                } else {
                    (icons::X_CIRCLE, p.err)
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(icon).font(theme::icon_font(14.0)).color(color));
                    ui.label(&t.name);
                    if let Some(e) = &t.error {
                        ui.label(RichText::new(e).color(p.text_muted));
                    }
                });
            }
            for e in &state.errors {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(icons::WARNING_CIRCLE).font(theme::icon_font(14.0)).color(p.err));
                    ui.label(RichText::new(e).color(p.err));
                });
            }
        });
        ui.add_space(4.0);
    }
    if state.waiting {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(14.0).color(p.accent));
            ui.label(RichText::new(tr("Running the pre-request script…")).color(p.text_muted));
        });
    }
    crate::request_tab::script_panel(ui, state.editing, scripts, vars);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(ctx: &egui::Context, log: &mut Log) -> (std::time::Duration, Vec<String>) {
        ctx.enable_accesskit();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let mut out = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default_margins().show(ui, |ui| log.ui(ui, "log"));
        });
        let took = started.elapsed();
        let nodes = out.platform_output.accesskit_update.take().map(|u| u.nodes).unwrap_or_default();
        out.drop_without_applying_deltas();
        (took, nodes.iter().filter_map(|(_, n)| n.label().or(n.value()).map(str::to_string)).collect())
    }

    /// The MQTT, Socket.IO, gRPC and MCP logs lay out only the rows in view,
    /// one line each, so a log of 20,000 messages (some long and multi-line)
    /// draws in milliseconds; the selected message is shown in full.
    #[test]
    fn big_log_draws_only_visible_rows() {
        let mut log = Log { cap: 20_000, ..Default::default() };
        let json = serde_json::to_string_pretty(&serde_json::json!({"items": vec![1; 500]})).unwrap();
        for i in 0..20_000 {
            let text = if i % 7 == 0 { json.clone() } else { format!("message {i}") };
            log.push(if i % 2 == 0 { Dir::In } else { Dir::Out }, "topic/a", text, 0);
        }
        log.selected = Some(19_998);
        let ctx = egui::Context::default();
        draw(&ctx, &mut log);
        let (took, labels) = draw(&ctx, &mut log);
        assert!(labels.len() < 1_000, "{} widgets", labels.len());
        assert!(took < std::time::Duration::from_millis(500), "{took:?}");
        // The newest rows are in view (the list sticks to the bottom).
        assert!(labels.iter().any(|l| l == "topic/a message 19997"), "{labels:?}");
        assert!(!labels.iter().any(|l| l == "topic/a message 1"), "{labels:?}");
        // The selected message in full below the list.
        assert!(labels.iter().any(|l| l == "message 19998"), "{labels:?}");
    }

    /// Searching keeps up with new lines and with the oldest being dropped.
    #[test]
    fn search_follows_the_log() {
        let mut log = Log { cap: 10, ..Default::default() };
        for i in 0..10 {
            log.push(Dir::In, "", format!("m{i}"), 0);
        }
        log.search = "M1".into();
        assert_eq!(log.matching(), Some(vec![1]));
        for i in 10..15 {
            log.push(Dir::In, "", format!("m{i}"), 0);
        }
        // m1 dropped; m10..m14 added (now at indexes 5..9).
        assert_eq!(log.matching(), Some(vec![5, 6, 7, 8, 9]));
        log.search = "m13".into();
        assert_eq!(log.matching(), Some(vec![8]));
        log.clear();
        assert_eq!(log.matching(), Some(vec![]));
        log.push(Dir::In, "m13", "", 0);
        assert_eq!(log.matching(), Some(vec![0]));
        log.search.clear();
        assert_eq!(log.matching(), None);
    }

    /// The Sent/Received filter combines with the search.
    #[test]
    fn filter_by_direction() {
        let mut log = Log::default();
        log.push(Dir::Out, "", "ping", 0);
        log.push(Dir::In, "", "pong", 0);
        log.system("connected");
        log.push(Dir::In, "", "ping back", 0);
        log.filter = LogFilter::Received;
        assert_eq!(log.matching(), Some(vec![1, 3]));
        log.search = "ping".into();
        assert_eq!(log.matching(), Some(vec![3]));
        log.filter = LogFilter::Sent;
        assert_eq!(log.matching(), Some(vec![0]));
        log.filter = LogFilter::All;
        log.search.clear();
        assert_eq!(log.matching(), None);
    }
}
