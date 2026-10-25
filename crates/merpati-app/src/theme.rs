//! Design tokens (colors, type, spacing) and the egui style built from them.
//!
//! Every screen draws with these tokens: [`Palette`] for colors (light and
//! dark), the font helpers ([`medium`], [`semibold`], [`mono`]) for type, and
//! the components in `kit.rs`. The accent is the coral red of the app icon.

use egui::{Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, Visuals};

/// Colors of one theme.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Content area (request editor, response).
    pub canvas: Color32,
    /// Window chrome: top bar, sidebar, tab strip, status bar.
    pub chrome: Color32,
    /// Raised surfaces: inputs, cards, dialogs, menus.
    pub surface: Color32,
    /// Hovered rows and quiet button fills.
    pub hover: Color32,
    /// Pressed state and subtle fills (segmented control track).
    pub subtle: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_faint: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    /// Text on an accent fill.
    pub on_accent: Color32,
    /// Background of selected rows and accent pills.
    pub accent_soft: Color32,
    /// Accent-colored text on light backgrounds.
    pub accent_text: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub info: Color32,
}

const LIGHT: Palette = Palette {
    canvas: Color32::from_rgb(255, 255, 255),
    chrome: Color32::from_rgb(247, 247, 248),
    surface: Color32::from_rgb(255, 255, 255),
    hover: Color32::from_rgb(240, 240, 243),
    subtle: Color32::from_rgb(234, 234, 238),
    border: Color32::from_rgb(228, 228, 233),
    border_strong: Color32::from_rgb(208, 208, 215),
    text: Color32::from_rgb(26, 26, 32),
    text_muted: Color32::from_rgb(104, 104, 116),
    text_faint: Color32::from_rgb(150, 150, 162),
    accent: Color32::from_rgb(225, 77, 42),
    accent_hover: Color32::from_rgb(204, 64, 31),
    on_accent: Color32::WHITE,
    accent_soft: Color32::from_rgb(253, 236, 230),
    accent_text: Color32::from_rgb(190, 58, 26),
    ok: Color32::from_rgb(22, 140, 78),
    warn: Color32::from_rgb(180, 112, 0),
    err: Color32::from_rgb(214, 48, 49),
    info: Color32::from_rgb(37, 99, 220),
};

const DARK: Palette = Palette {
    canvas: Color32::from_rgb(24, 25, 28),
    chrome: Color32::from_rgb(19, 20, 23),
    surface: Color32::from_rgb(32, 33, 37),
    hover: Color32::from_rgb(38, 39, 44),
    subtle: Color32::from_rgb(44, 45, 51),
    border: Color32::from_rgb(46, 47, 53),
    border_strong: Color32::from_rgb(62, 64, 71),
    text: Color32::from_rgb(232, 232, 236),
    text_muted: Color32::from_rgb(158, 159, 168),
    text_faint: Color32::from_rgb(110, 111, 120),
    accent: Color32::from_rgb(242, 103, 72),
    accent_hover: Color32::from_rgb(250, 125, 96),
    on_accent: Color32::WHITE,
    accent_soft: Color32::from_rgb(63, 36, 30),
    accent_text: Color32::from_rgb(255, 144, 118),
    ok: Color32::from_rgb(74, 205, 128),
    warn: Color32::from_rgb(240, 180, 72),
    err: Color32::from_rgb(248, 104, 100),
    info: Color32::from_rgb(110, 160, 255),
};

impl Palette {
    /// Barely visible fill for hovered table rows.
    pub fn faint_row(&self) -> Color32 {
        Color32::from_rgba_unmultiplied(self.hover.r(), self.hover.g(), self.hover.b(), 150)
    }
}

pub fn palette(dark: bool) -> Palette {
    if dark { DARK } else { LIGHT }
}

/// The palette of the theme `ui` is drawn with.
pub fn pal(ui: &egui::Ui) -> Palette {
    palette(ui.visuals().dark_mode)
}

/// Accent used for primary actions.
pub fn accent(dark: bool) -> Color32 {
    palette(dark).accent
}

pub fn method_color(method: &str, dark: bool) -> Color32 {
    let (d, l) = match method.to_ascii_uppercase().as_str() {
        "GET" => ((74, 205, 128), (22, 140, 78)),
        "POST" => ((246, 184, 70), (184, 112, 0)),
        "PUT" => ((100, 160, 255), (37, 99, 220)),
        "PATCH" => ((180, 140, 250), (124, 72, 210)),
        "DELETE" | "DEL" => ((248, 104, 100), (214, 48, 49)),
        // Protocol badges share the hues of the rarer methods.
        "HEAD" | "SIO" | "SOCKET.IO" => ((96, 205, 205), (14, 128, 132)),
        "OPTIONS" | "OPT" | "AI" => ((240, 125, 200), (180, 45, 135)),
        "WS" => ((110, 180, 255), (20, 110, 200)),
        "GQL" | "GRPC" => ((236, 100, 190), (196, 30, 140)),
        "MQTT" => ((180, 140, 250), (124, 72, 210)),
        "MCP" => ((246, 184, 70), (184, 112, 0)),
        _ => ((158, 159, 168), (104, 104, 116)),
    };
    let (r, g, b) = if dark { d } else { l };
    Color32::from_rgb(r, g, b)
}

pub fn status_color(status: u16, dark: bool) -> Color32 {
    match status {
        200..=299 => ok(dark),
        300..=399 => palette(dark).info,
        400..=499 => warn(dark),
        _ => err(dark),
    }
}

pub fn ok(dark: bool) -> Color32 {
    palette(dark).ok
}

pub fn warn(dark: bool) -> Color32 {
    palette(dark).warn
}

pub fn err(dark: bool) -> Color32 {
    palette(dark).err
}

pub fn muted(dark: bool) -> Color32 {
    palette(dark).text_muted
}

pub fn var_defined(dark: bool) -> Color32 {
    if dark { Color32::from_rgb(110, 200, 140) } else { Color32::from_rgb(20, 130, 70) }
}

pub fn var_undefined(dark: bool) -> Color32 {
    err(dark)
}

/// A color at `alpha` (0–1) opacity, for soft fills behind colored text.
pub fn soft(c: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (alpha.clamp(0.0, 1.0) * 255.0) as u8)
}

// ---------------------------------------------------------------------------
// Type
// ---------------------------------------------------------------------------

const MEDIUM: &str = "medium";
const SEMIBOLD: &str = "semibold";
const ICONS: &str = "icons";

/// Body text size for the user's font size setting.
fn body(font_size: f32) -> f32 {
    font_size + 0.5
}

/// Inter Medium (labels that need a little weight: tabs, buttons).
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MEDIUM.into()))
}

/// Inter SemiBold (titles, headings).
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// The icon font alone (icons also render inside ordinary text).
pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}

/// Inter (regular, medium, semibold), JetBrains Mono and the Phosphor icons,
/// with egui's default fonts as fallbacks for scripts and emoji the bundled
/// subsets leave out.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let font = |bytes: &'static [u8]| std::sync::Arc::new(egui::FontData::from_static(bytes));
    fonts.font_data.insert("Inter".into(), font(include_bytes!("../assets/fonts/Inter-Regular.ttf")));
    fonts.font_data.insert("Inter-Medium".into(), font(include_bytes!("../assets/fonts/Inter-Medium.ttf")));
    fonts.font_data.insert("Inter-SemiBold".into(), font(include_bytes!("../assets/fonts/Inter-SemiBold.ttf")));
    fonts.font_data.insert("JetBrainsMono".into(), font(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")));
    let mut phosphor = egui::FontData::from_static(include_bytes!("../assets/fonts/Phosphor.ttf"));
    // Icons sit a little high next to Inter's x-height.
    phosphor.tweak.y_offset_factor = 0.08;
    fonts.font_data.insert("Phosphor".into(), std::sync::Arc::new(phosphor));

    let fallbacks: Vec<String> = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mono_fallbacks: Vec<String> = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with = |first: &str| {
        let mut v = vec![first.to_string(), "Phosphor".to_string()];
        v.extend(fallbacks.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, with("Inter"));
    fonts.families.insert(FontFamily::Name(MEDIUM.into()), with("Inter-Medium"));
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), with("Inter-SemiBold"));
    let mut mono = vec!["JetBrainsMono".to_string()];
    mono.extend(mono_fallbacks);
    mono.push("Phosphor".into());
    fonts.families.insert(FontFamily::Monospace, mono);
    fonts.families.insert(FontFamily::Name(ICONS.into()), vec!["Phosphor".into(), "Inter".into()]);
    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

pub const RADIUS: u8 = 6;
pub const RADIUS_LG: u8 = 10;

pub fn apply(ctx: &egui::Context, font_size: f32) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            let dark = theme == egui::Theme::Dark;
            let p = palette(dark);
            let s = &mut style.spacing;
            s.item_spacing = egui::vec2(8.0, 6.0);
            s.button_padding = egui::vec2(10.0, 4.0);
            s.interact_size = egui::vec2(24.0, 26.0);
            s.window_margin = egui::Margin::same(16);
            s.menu_margin = egui::Margin::same(6);
            s.indent = 16.0;
            s.icon_width = 15.0;
            s.icon_width_inner = 9.0;
            s.icon_spacing = 6.0;
            s.combo_height = 320.0;
            s.tooltip_width = 360.0;
            s.menu_width = 220.0;
            s.scroll = egui::style::ScrollStyle::floating();
            s.scroll.bar_width = 8.0;
            s.scroll.floating_width = 3.0;
            s.scroll.floating_allocated_width = 0.0;

            let v: &mut Visuals = &mut style.visuals;
            v.panel_fill = p.canvas;
            v.window_fill = p.surface;
            v.extreme_bg_color = p.surface;
            v.text_edit_bg_color = Some(p.surface);
            v.faint_bg_color = if dark { Color32::from_rgb(28, 29, 33) } else { Color32::from_rgb(250, 250, 251) };
            v.code_bg_color = p.subtle;
            v.hyperlink_color = p.accent_text;
            v.warn_fg_color = p.warn;
            v.error_fg_color = p.err;
            v.override_text_color = None;
            v.weak_text_color = Some(p.text_muted);
            v.selection.bg_fill = if dark { soft(p.accent, 0.32) } else { soft(p.accent, 0.18) };
            // Also the border of a focused text field: accent in both themes.
            v.selection.stroke = Stroke::new(1.0, if dark { p.accent } else { p.accent_text });
            v.text_cursor.stroke = Stroke::new(1.5, p.accent);
            v.window_corner_radius = CornerRadius::same(RADIUS_LG);
            v.menu_corner_radius = CornerRadius::same(8);
            v.window_stroke = Stroke::new(1.0, p.border);
            v.window_shadow = Shadow {
                offset: [0, 10],
                blur: 30,
                spread: 0,
                color: Color32::from_black_alpha(if dark { 120 } else { 38 }),
            };
            v.popup_shadow = Shadow {
                offset: [0, 4],
                blur: 14,
                spread: 0,
                color: Color32::from_black_alpha(if dark { 100 } else { 28 }),
            };
            v.window_highlight_topmost = false;
            v.collapsing_header_frame = false;
            v.indent_has_left_vline = true;
            v.striped = false;
            v.slider_trailing_fill = true;
            v.handle_shape = egui::style::HandleShape::Circle;
            v.interact_cursor = Some(egui::CursorIcon::PointingHand);
            v.disabled_alpha = 0.45;

            let radius = CornerRadius::same(RADIUS);
            let w = &mut v.widgets;
            // Labels, separators, frames.
            w.noninteractive.bg_fill = p.surface;
            w.noninteractive.weak_bg_fill = p.surface;
            w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
            w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
            w.noninteractive.corner_radius = radius;
            // Buttons, inputs, checkboxes at rest: outlined surface.
            w.inactive.bg_fill = p.subtle;
            w.inactive.weak_bg_fill = p.surface;
            w.inactive.bg_stroke = Stroke::new(1.0, p.border_strong);
            w.inactive.fg_stroke = Stroke::new(1.0, p.text);
            w.inactive.corner_radius = radius;
            w.inactive.expansion = 0.0;
            w.hovered.bg_fill = p.subtle;
            w.hovered.weak_bg_fill = p.hover;
            w.hovered.bg_stroke =
                Stroke::new(1.0, if dark { p.border_strong } else { Color32::from_rgb(190, 190, 200) });
            w.hovered.fg_stroke = Stroke::new(1.0, p.text);
            w.hovered.corner_radius = radius;
            w.hovered.expansion = 0.0;
            w.active.bg_fill = p.subtle;
            w.active.weak_bg_fill = p.subtle;
            w.active.bg_stroke = Stroke::new(1.0, p.accent);
            w.active.fg_stroke = Stroke::new(1.0, p.text);
            w.active.corner_radius = radius;
            w.active.expansion = 0.0;
            w.open.bg_fill = p.subtle;
            w.open.weak_bg_fill = p.hover;
            w.open.bg_stroke = Stroke::new(1.0, p.border_strong);
            w.open.fg_stroke = Stroke::new(1.0, p.text);
            w.open.corner_radius = radius;

            for (ts, font) in style.text_styles.iter_mut() {
                *font = match ts {
                    egui::TextStyle::Heading => semibold(font_size + 5.0),
                    egui::TextStyle::Small => FontId::proportional(font_size - 1.5),
                    egui::TextStyle::Monospace => mono(font_size - 0.5),
                    egui::TextStyle::Button => FontId::proportional(body(font_size)),
                    _ => FontId::proportional(body(font_size)),
                };
            }
        });
    }
}
