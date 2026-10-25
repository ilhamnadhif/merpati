//! Drawing a [`Document`] with egui.
//!
//! Layout happens while drawing, in one pass over the tree, with small caches
//! kept in egui memory between frames (text widths, block and row heights).
//!
//! * Block flow with collapsing vertical margins, `box-sizing`,
//!   `min-/max-width`, `height`, `min-/max-height` and `overflow` (clipped;
//!   `auto`/`scroll` boxes with a definite height get a scroll area).
//! * Floats are placed at the left or right edge of their block formatting
//!   context, below earlier floats when they do not fit; `clear` moves a block
//!   below them, and boxes that establish a formatting context (`overflow`
//!   other than `visible`, `display: flow-root`, floats, flex and grid items,
//!   inline-blocks, table cells) or use the clearfix idiom contain them.
//!   The lines of a text paragraph are shortened only while they are beside
//!   a float and use the full width again below it; boxes that avoid floats
//!   (formatting contexts) sit beside them as a whole. Simplification: a
//!   paragraph with inline images or inline-blocks next to a float is laid
//!   out in the space beside it for its whole height.
//! * Flex rows (`flex-grow`/`-shrink`/`-basis`, `justify-content`,
//!   `align-items`/`-self`, auto margins, wrapping) and grids (see
//!   [`crate::grid`]); column flexboxes are laid out as one-column grids.
//! * `position: relative` offsets a box after layout; `absolute` boxes are
//!   placed in the padding box of the nearest positioned ancestor once it is
//!   laid out and `fixed` ones in the viewport (the visible part of the
//!   scroll area showing the document), so they do not scroll. `sticky`
//!   boxes (also table cells, e.g. headers) are shifted every frame to keep
//!   their `top`/`bottom`/`left`/`right` distance from the edges of the
//!   nearest scrollport (the document's scroll area, an `overflow` box with a
//!   scroll area, or a non-scrolling `overflow` box, where they stay put),
//!   within their containing block as laid out in the previous frame.
//!   Positioned boxes paint after the normal flow by `z-index` (negative
//!   values below it); a relatively positioned box with a `z-index`, and a
//!   sticky box, is a stacking context for the absolutely positioned boxes
//!   inside it.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use egui::epaint::ClippedShape;
use egui::layers::ShapeIdx;
use egui::text::{LayoutJob, LayoutSection, TextFormat, TextWrapping};
use egui::text_selection::LabelSelectionState;
use egui::{
    Align, Color32, CursorIcon, FontFamily, FontId, Id, Label, Layout, Painter, Pos2, Rect, Response, Sense, Shape,
    Stroke, TextWrapMode, Ui, UiBuilder, Vec2, pos2, vec2,
};

use crate::css::Len;
use crate::document::{Block, BoxStyle, Cell, Content, Document, Grid, ImageRef, Inline, Table, TextStyle};
use crate::grid::{self, Breadth, Contribution, TrackSize};
use crate::style::{BorderSide, BorderStyle, Clear, Float, ItemAlign, Justify, Overflow, Position, TextAlign, VAlign};

/// What the renderer needs from the application.
pub trait Host {
    /// A link was clicked (`href` as written in the page).
    fn open_link(&mut self, href: &str);

    /// Bytes of a remote (`http(s)://`) image. Called every frame while the
    /// image is visible; return `None` while it loads or when remote images
    /// are not allowed (the default), and the `alt` text is shown instead.
    fn remote_image(&mut self, _url: &str) -> Option<egui::load::Bytes> {
        None
    }
}

impl<F: FnMut(&str)> Host for F {
    fn open_link(&mut self, href: &str) {
        self(href)
    }
}

/// Rendering options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Size in egui points of 16 CSS px (the browser default font size).
    /// Default: the `Body` text style.
    pub base_size: Option<f32>,
    /// Font family for bold text. Default: a family named `bold` when the
    /// app registered one, otherwise bold is drawn with a slight overstrike.
    pub bold_family: Option<FontFamily>,
    /// Text can be selected and copied.
    pub selectable: bool,
    /// Tables with more rows than this only lay out the visible rows.
    pub virtualize_rows: usize,
    /// The page behind the document (a `body` background wins).
    pub paper: Paper,
    /// Salt for the egui ids of the document (show two documents at once).
    pub id_salt: Option<Id>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            base_size: None,
            bold_family: None,
            selectable: true,
            virtualize_rows: 60,
            paper: Paper::Auto,
            id_salt: None,
        }
    }
}

impl Options {
    /// The width available in `ui` in CSS px: the viewport width for
    /// `@media` queries (see [`Document::restyle_needed`]).
    pub fn css_width(&self, ui: &Ui) -> f32 {
        let base_size = self.base_size.unwrap_or_else(|| ui.style().text_styles[&egui::TextStyle::Body].size);
        ui.available_width() / (base_size / 16.0).max(0.1)
    }
}

/// What the document is drawn on.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Paper {
    /// White, like a browser, when the page sets background colors of its
    /// own (it was designed for a white page); otherwise the theme's panel
    /// color, so unstyled pages follow dark mode.
    #[default]
    Auto,
    /// The theme's panel color.
    Theme,
    /// White.
    White,
    /// Nothing is painted; the color is what is behind the document (used
    /// to pick readable text colors).
    Transparent(Color32),
}

impl Document {
    /// The color the document is drawn on, and whether it must be painted.
    pub fn paper_color(&self, ui: &Ui, paper: Paper) -> (Color32, bool) {
        let panel = ui.visuals().panel_fill;
        let base = match paper {
            Paper::Auto if self.sets_backgrounds => Color32::WHITE,
            Paper::Auto | Paper::Theme => panel,
            Paper::White => Color32::WHITE,
            Paper::Transparent(c) => {
                return (self.background.map(|b| over(c, b)).unwrap_or(c), self.background.is_some());
            }
        };
        (self.background.map(|b| over(base, b)).unwrap_or(base), base != panel || self.background.is_some())
    }

    /// Shows the document in a scroll area that fills the available space.
    pub fn show(&self, ui: &mut Ui, host: &mut dyn Host) -> Response {
        self.show_with(ui, host, &Options::default())
    }

    pub fn show_with(&self, ui: &mut Ui, host: &mut dyn Host, opts: &Options) -> Response {
        let salt = opts.id_salt.unwrap_or_else(|| ui.scope_id().with("merpati_html"));
        egui::ScrollArea::both()
            .id_salt(salt)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let (paper, paint) = self.paper_color(ui, opts.paper);
                if paint {
                    let clip = ui.clip_rect();
                    ui.painter().rect_filled(clip, 0.0, paper);
                }
                self.ui(ui, host, opts)
            })
            .inner
    }

    /// Lays out the document in `ui` without a scroll area (the caller
    /// provides one; only the visible part of large tables is drawn).
    pub fn ui(&self, ui: &mut Ui, host: &mut dyn Host, opts: &Options) -> Response {
        self.ui_impl(ui, host, opts, false).0
    }

    /// Lays the document out offscreen in a viewport of `size` points (a few
    /// passes, so cached sizes settle) and returns the border box of every
    /// block that was drawn, by [`Block::key`], relative to the top-left
    /// corner of the document. For tests and tools.
    pub fn layout_boxes(&self, size: Vec2, opts: &Options) -> HashMap<u32, Rect> {
        let ctx = egui::Context::default();
        let mut boxes = HashMap::new();
        for _ in 0..3 {
            let input =
                egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                    let origin = ui.cursor().min.to_vec2();
                    let (_, recorded) = self.ui_impl(ui, &mut |_: &str| {}, opts, true);
                    boxes = recorded.into_iter().map(|(k, r)| (k, r.translate(-origin))).collect();
                });
            });
            out.textures_delta.clear();
        }
        boxes
    }

    fn ui_impl(
        &self,
        ui: &mut Ui,
        host: &mut dyn Host,
        opts: &Options,
        record: bool,
    ) -> (Response, HashMap<u32, Rect>) {
        let base_id = opts.id_salt.unwrap_or_else(|| ui.scope_id().with("merpati_html")).with("doc");
        let base_size = opts.base_size.unwrap_or_else(|| ui.style().text_styles[&egui::TextStyle::Body].size);
        let scale = (base_size / 16.0).max(0.1);
        let visuals = ui.visuals().clone();
        let (page_bg, _) = self.paper_color(ui, opts.paper);
        let bold_family = opts.bold_family.clone().or_else(|| {
            let bold = FontFamily::Name("bold".into());
            ui.fonts(|f| f.families().contains(&bold)).then_some(bold)
        });
        let ppp = ui.ctx().pixels_per_point();
        let mut cache: DocCache = ui.ctx().data_mut(|d| d.remove_temp(base_id)).unwrap_or_default();
        let key = (scale.to_bits(), ppp.to_bits(), self.hash);
        if cache.key != key {
            cache = DocCache { key, ..Default::default() };
        }
        let mut r = Renderer {
            host,
            opts,
            scale,
            dark: visuals.dark_mode,
            theme_text: visuals.text_color(),
            link: visuals.hyperlink_color,
            bg: vec![page_bg],
            bold_family,
            cache,
            repaint: false,
            floats: vec![FloatCtx::new()],
            abs_frames: Vec::new(),
            contexts: vec![Vec::new()],
            behind: None,
            behind_shapes: Vec::new(),
            measuring: 0,
            definite_h: None,
            icb: Rect::NOTHING,
            viewport: Rect::NOTHING,
            scrollport: Rect::NOTHING,
            fixed: Vec::new(),
            sticky_frames: Vec::new(),
            record: record.then(HashMap::new),
        };
        let response = ui
            .scope(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let width = ui.available_width();
                // The initial containing block: the document's first screen.
                r.icb = Rect::from_min_size(ui.cursor().min, vec2(width, ui.clip_rect().height().max(0.0)));
                // What is visible of the document (the scroll area showing it).
                let clip = ui.clip_rect();
                r.viewport = if clip.is_finite() && clip.is_positive() { clip } else { r.icb };
                r.scrollport = r.viewport;
                let top = r.collapsed_top(&self.root, width);
                ui.add_space(top);
                r.block_box(ui, &self.root, Fit { root: true, ..Default::default() });
                ui.add_space(r.collapsed_bottom(&self.root, width));
                // Fixed boxes join the root's positioned boxes; those found
                // while painting positioned boxes are painted in a later round.
                for _ in 0..8 {
                    let fixed = std::mem::take(&mut r.fixed);
                    let viewport = r.viewport;
                    r.resolve_abs(fixed, viewport, viewport, page_bg);
                    let layers = r.contexts.pop().unwrap_or_default();
                    if layers.is_empty() {
                        break;
                    }
                    r.paint_layers(ui, layers, true);
                    r.contexts.push(Vec::new());
                }
                if let Some(idx) = r.behind {
                    ui.painter().set(idx, Shape::Vec(std::mem::take(&mut r.behind_shapes)));
                }
            })
            .response;
        if r.repaint {
            ui.ctx().request_repaint();
        }
        let cache = std::mem::take(&mut r.cache);
        ui.ctx().data_mut(|d| d.insert_temp(base_id, cache));
        (response, r.record.unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Caches kept in egui memory between frames
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct TableCache {
    /// (min, max) border-box widths per column, in points.
    cols: Vec<(f32, f32)>,
    /// Fixed column widths (points) and percentages.
    fixed: Vec<Option<Len>>,
    /// Column widths used last frame (to detect relayouts).
    widths: Vec<f32>,
    /// Row heights (0 = not measured yet).
    heights: Vec<f32>,
    /// Content heights of the cells of each row.
    cell_heights: Vec<Vec<f32>>,
}

#[derive(Clone, Default)]
struct GridCache {
    /// Explicit (columns, rows) the placement was made for.
    explicit: (usize, usize),
    cells: Vec<grid::Cell>,
    cols: usize,
    rows: usize,
    /// Column widths and row heights of the last frame (0 = not drawn yet).
    widths: Vec<f32>,
    heights: Vec<f32>,
    /// Row heights from content alone, before `fr` rows were stretched.
    content: Vec<f32>,
    /// Height of `1fr` used for flexible rows in the last frame.
    fr_unit: f32,
}

#[derive(Clone, Default)]
struct DocCache {
    key: (u32, u32, u64),
    /// (min, max) widths of blocks by key.
    measures: HashMap<u32, (f32, f32)>,
    /// Block heights by key: (available width, height).
    heights: HashMap<u32, (f32, f32)>,
    /// Border-box heights by key: (border-box width, height) of boxes laid
    /// out at a width decided by their container.
    natural: HashMap<u32, (f32, f32)>,
    /// Margin-box sizes of floats: (available width, size).
    floats: HashMap<u32, (f32, Vec2)>,
    tables: HashMap<u32, TableCache>,
    grids: HashMap<u32, GridCache>,
    /// Room of sticky boxes in their containing blocks.
    sticky: HashMap<u32, StickyRoom>,
}

/// Last frame's layout of a `position: sticky` box: how far it may move up,
/// down, left and right without leaving its containing block, and its
/// border-box height.
#[derive(Clone, Copy, Default, PartialEq)]
struct StickyRoom {
    up: f32,
    down: f32,
    left: f32,
    right: f32,
    height: f32,
}

impl StickyRoom {
    /// Room of the border box `r` (with margins `[top, right, bottom, left]`)
    /// in the content box `cb`.
    fn new(r: Rect, margins: [f32; 4], cb: Rect) -> Self {
        StickyRoom {
            up: (r.top() - margins[0] - cb.top()).max(0.0),
            down: (cb.bottom() - r.bottom() - margins[2]).max(0.0),
            left: (r.left() - margins[3] - cb.left()).max(0.0),
            right: (cb.right() - r.right() - margins[1]).max(0.0),
            height: r.height(),
        }
    }

    fn differs(&self, other: &StickyRoom) -> bool {
        let a = [self.up, self.down, self.left, self.right, self.height];
        let b = [other.up, other.down, other.left, other.right, other.height];
        a.iter().zip(b).any(|(x, y)| (x - y).abs() > 0.5)
    }
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

fn luminance(c: Color32) -> f32 {
    let f = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
}

fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn is_light(c: Color32) -> bool {
    luminance(c) > 0.35
}

/// `top` composited over the opaque `bottom`.
fn over(bottom: Color32, top: Color32) -> Color32 {
    let a = top.a() as f32 / 255.0;
    if a >= 1.0 {
        return top;
    }
    let [tr, tg, tb, _] = top.to_srgba_unmultiplied();
    let mix = |t: u8, b: u8| (t as f32 * a + b as f32 * (1.0 - a)).round() as u8;
    Color32::from_rgb(mix(tr, bottom.r()), mix(tg, bottom.g()), mix(tb, bottom.b()))
}

// ---------------------------------------------------------------------------
// Paint order helpers
// ---------------------------------------------------------------------------

fn next_shape(ui: &Ui) -> ShapeIdx {
    let layer = ui.layer_id();
    ui.ctx().graphics_mut(|g| g.entry(layer).next_idx())
}

/// Takes the shapes painted in `ui`'s layer since `start` out of the layer
/// (they are painted again later with [`repaint_shapes`]). Empty entries
/// (taken earlier by nested boxes) stay behind, so nesting does not
/// multiply them.
fn take_shapes(ui: &Ui, start: ShapeIdx) -> Vec<ClippedShape> {
    let layer = ui.layer_id();
    ui.ctx().graphics_mut(|g| {
        let list = g.entry(layer);
        let end = list.next_idx().0;
        let shapes: Vec<ClippedShape> =
            list.all_entries().skip(start.0).filter(|c| !matches!(c.shape, Shape::Noop)).cloned().collect();
        for i in start.0..end {
            list.reset_shape(ShapeIdx(i));
        }
        shapes
    })
}

fn repaint_shapes(ui: &Ui, shapes: Vec<ClippedShape>) {
    if shapes.is_empty() {
        return;
    }
    let layer = ui.layer_id();
    ui.ctx().graphics_mut(|g| {
        let list = g.entry(layer);
        for s in shapes {
            let idx = list.add(s.clip_rect, Shape::Noop);
            list.mutate_shape(idx, |c| *c = s);
        }
    });
}

/// A length in points (`px` scaled, percentages of `of`).
fn len_at(l: Len, of: f32, scale: f32) -> Option<f32> {
    match l {
        Len::Auto => None,
        Len::Px(p) => Some(p * scale),
        Len::Percent(p) => Some(of * p / 100.0),
        Len::Calc(px, p) => Some(px * scale + of * p / 100.0),
        Len::Clamp(_) => l.resolve_clamp(of, scale),
    }
}

/// (offset of the first item, extra space between items) for `justify`.
fn distribute(justify: Justify, free: f32, n: usize) -> (f32, f32) {
    if free <= 0.0 || n == 0 {
        return (0.0, 0.0);
    }
    let n = n as f32;
    match justify {
        Justify::Normal | Justify::Start => (0.0, 0.0),
        Justify::End => (free, 0.0),
        Justify::Center => (free / 2.0, 0.0),
        Justify::SpaceBetween if n > 1.0 => (0.0, free / (n - 1.0)),
        Justify::SpaceBetween => (0.0, 0.0),
        Justify::SpaceAround => (free / (2.0 * n), free / n),
        Justify::SpaceEvenly => (free / (n + 1.0), free / (n + 1.0)),
    }
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// Whether a child of `b` is a sticky box (its room is measured in `b`).
fn has_sticky_child(b: &Block) -> bool {
    let sticky = |c: &Block| c.style.position == Position::Sticky;
    match &b.content {
        Content::Inline(items) => items.iter().any(|i| matches!(i, Inline::Block(c) if sticky(c))),
        _ => b.children().iter().any(sticky),
    }
}

/// `TextFormat::expand_bg` of sections drawn with faux bold.
const FAUX_BOLD_MARK: f32 = 1.0001;

/// Content measured (at most) to size grid columns.
const MEASURE_ITEMS: usize = 400;

/// Runs of lines a paragraph is split into to flow around floats (at most).
const FLOW_PIECES: usize = 64;

/// A link in a layout job: char range and `href`.
type Link = (usize, usize, String);

/// Splits a layout job (and its links) at char `at`.
fn split_job(job: LayoutJob, links: Vec<Link>, at: usize) -> ((LayoutJob, Vec<Link>), (LayoutJob, Vec<Link>)) {
    let byte = job.text.char_indices().nth(at).map_or(job.text.len(), |(i, _)| i);
    let mut head = LayoutJob { text: job.text[..byte].to_string(), sections: Vec::new(), ..job.clone() };
    let mut tail = LayoutJob { text: job.text[byte..].to_string(), sections: Vec::new(), ..job.clone() };
    for sec in job.sections {
        let (a, z) = (sec.byte_range.start.0, sec.byte_range.end.0);
        if a < byte {
            head.sections.push(LayoutSection { byte_range: a.into()..z.min(byte).into(), ..sec.clone() });
        }
        if z > byte {
            let range = (a.max(byte) - byte).into()..(z - byte).into();
            tail.sections.push(LayoutSection { leading_space: 0.0, byte_range: range, ..sec });
        }
    }
    let head_links = links.iter().filter(|l| l.0 < at).map(|(a, z, h)| (*a, (*z).min(at), h.clone())).collect();
    let tail_links = links.into_iter().filter(|l| l.1 > at).map(|(a, z, h)| (a.max(at) - at, z - at, h)).collect();
    ((head, head_links), (tail, tail_links))
}

fn has_faux_bold(job: &LayoutJob) -> bool {
    job.sections.iter().any(|s| s.format.expand_bg == FAUX_BOLD_MARK)
}

/// The floats of a block formatting context.
struct FloatCtx {
    /// Margin boxes of the floats that may still affect content below.
    left: Vec<Rect>,
    right: Vec<Rect>,
    /// Top of the last float: later floats are never placed higher.
    top: f32,
    /// Lowest float bottom on each side.
    bottom_left: f32,
    bottom_right: f32,
    /// Shapes of the floats, painted when the context ends (above the
    /// backgrounds of the blocks next to them).
    shapes: Vec<ClippedShape>,
}

impl FloatCtx {
    fn new() -> Self {
        FloatCtx {
            left: Vec::new(),
            right: Vec::new(),
            top: f32::NEG_INFINITY,
            bottom_left: f32::NEG_INFINITY,
            bottom_right: f32::NEG_INFINITY,
            shapes: Vec::new(),
        }
    }

    fn bottom(&self) -> f32 {
        self.bottom_left.max(self.bottom_right)
    }

    /// The horizontal room in `[x0, x1]` between the floats overlapping
    /// `y0..y1`.
    fn band(&self, y0: f32, y1: f32, x0: f32, x1: f32) -> (f32, f32) {
        let (mut l, mut r) = (x0, x1);
        let hits = |f: &&Rect| f.top() < y1 && f.bottom() > y0 && f.right() > x0 && f.left() < x1;
        for f in self.left.iter().filter(hits) {
            l = l.max(f.right());
        }
        for f in self.right.iter().filter(hits) {
            r = r.min(f.left());
        }
        (l, r.max(l))
    }

    /// The next float bottom below `y`.
    fn next_bottom(&self, y: f32) -> Option<f32> {
        self.left.iter().chain(&self.right).map(Rect::bottom).filter(|b| *b > y).min_by(f32::total_cmp)
    }

    fn clear_y(&self, clear: Clear) -> Option<f32> {
        let y = match clear {
            Clear::None => return None,
            Clear::Left => self.bottom_left,
            Clear::Right => self.bottom_right,
            Clear::Both => self.bottom(),
        };
        y.is_finite().then_some(y)
    }

    /// Room at `y` for a box `need` points wide, moving down past floats
    /// until it fits: (y, left, right).
    fn fit(&self, mut y: f32, h: f32, need: f32, x0: f32, x1: f32) -> (f32, f32, f32) {
        loop {
            let (l, r) = self.band(y, y + h.max(1.0), x0, x1);
            if r - l + 0.5 >= need || (l <= x0 && r >= x1) {
                return (y, l, r);
            }
            match self.next_bottom(y) {
                Some(b) => y = b,
                None => return (y, l, r),
            }
        }
    }

    fn add(&mut self, float: Float, rect: Rect) {
        self.top = rect.top();
        if float == Float::Right {
            self.bottom_right = self.bottom_right.max(rect.bottom());
            self.right.push(rect);
        } else {
            self.bottom_left = self.bottom_left.max(rect.bottom());
            self.left.push(rect);
        }
    }

    /// Forgets floats that end above `y` (content only moves down).
    fn prune(&mut self, y: f32) {
        self.left.retain(|f| f.bottom() > y);
        self.right.retain(|f| f.bottom() > y);
    }
}

/// An absolutely positioned box waiting for its containing block.
struct AbsPending<'d> {
    block: &'d Block,
    /// Where it would have been in the flow (for `auto` offsets).
    static_pos: Pos2,
}

enum LayerKind<'d> {
    /// An absolutely positioned box, laid out when painted.
    Abs { block: &'d Block, cb: Rect, static_pos: Pos2, clip: Rect, bg: Color32 },
    /// A relatively positioned box with a `z-index`, already laid out, and the
    /// positioned boxes of its stacking context.
    Shapes(Vec<ClippedShape>, Vec<Layer<'d>>),
}

/// Something painted after the normal flow.
struct Layer<'d> {
    z: i32,
    /// Tree order.
    order: u32,
    kind: LayerKind<'d>,
}

/// How the caller places a box.
#[derive(Clone, Copy, Default)]
struct Fit {
    /// The document's root box.
    root: bool,
    /// Contains its floats and its children's margins (flex and grid items,
    /// inline-blocks, floats, absolutely positioned boxes).
    bfc: bool,
    /// Border-box width decided by the caller.
    width: Option<f32>,
    /// Definite border-box height decided by the caller.
    height: Option<f32>,
    /// The caller placed the box: horizontal margins are already applied.
    no_margins: bool,
    /// Return absolutely positioned descendants instead of placing them (the
    /// caller may still stretch the box).
    defer_abs: bool,
}

/// A drawn box.
struct BoxOut<'d> {
    /// Border box.
    rect: Rect,
    /// Border-box height from the content and the box's own height
    /// properties.
    natural: f32,
    /// The shape slot of the background and borders (see [`Renderer::restretch`]).
    placeholder: ShapeIdx,
    painter: Painter,
    content_min: Pos2,
    /// Absolutely positioned descendants (with [`Fit::defer_abs`]).
    abs: Option<Vec<AbsPending<'d>>>,
    /// Clip for absolutely positioned descendants.
    clip: Rect,
    /// Background behind the content.
    bg: Color32,
}

/// A table cell drawn whose row height is not known yet.
struct CellOut<'d> {
    /// Left, right and top of the border box in the flow.
    rect: Rect,
    cell: &'d Cell,
    /// Its absolutely positioned descendants.
    frame: Option<Vec<AbsPending<'d>>>,
    bg: Color32,
    /// Sticky offset.
    shift: Vec2,
    /// Shapes of a sticky cell's content (painted with its box after the flow).
    content: Option<Vec<ClippedShape>>,
}

/// A grid item drawn whose rows are not all sized yet.
struct OpenItem<'d> {
    block: &'d Block,
    out: BoxOut<'d>,
    row0: usize,
    row1: usize,
    /// Vertical margins and alignment offset.
    extra: f32,
    align: ItemAlign,
}

/// Parameters of a flex row or a run of inline-blocks.
#[derive(Clone, Copy)]
struct RowSpec {
    gap: f32,
    row_gap: f32,
    wrap: bool,
    justify: Justify,
    align: ItemAlign,
    flex: bool,
}

struct Renderer<'a, 'd> {
    host: &'a mut dyn Host,
    opts: &'a Options,
    /// Points per CSS px.
    scale: f32,
    dark: bool,
    theme_text: Color32,
    link: Color32,
    /// Effective (opaque) background stack.
    bg: Vec<Color32>,
    bold_family: Option<FontFamily>,
    cache: DocCache,
    repaint: bool,
    /// Floats of the enclosing block formatting contexts.
    floats: Vec<FloatCtx>,
    /// Absolutely positioned boxes waiting for their containing block, one
    /// frame per positioned box being drawn (the root's first).
    abs_frames: Vec<Vec<AbsPending<'d>>>,
    /// Boxes to paint after the normal flow, per stacking context.
    contexts: Vec<Vec<Layer<'d>>>,
    /// Shape slot below the normal flow, for negative `z-index` boxes.
    behind: Option<ShapeIdx>,
    behind_shapes: Vec<Shape>,
    /// Laying out invisibly to measure heights: nothing is registered.
    measuring: u32,
    /// Content-box height of the box being drawn, when definite (for
    /// percentage heights of its children).
    definite_h: Option<f32>,
    /// The initial containing block.
    icb: Rect,
    /// The visible part of the document: containing block of `fixed` boxes.
    viewport: Rect,
    /// The visible area of the nearest scroll container (for `sticky`).
    scrollport: Rect,
    /// `position: fixed` boxes, placed in the viewport after the flow.
    fixed: Vec<AbsPending<'d>>,
    /// Sticky boxes drawn in each box being drawn that has some: key,
    /// border box (without the sticky offset) and margins.
    sticky_frames: Vec<Vec<(u32, Rect, [f32; 4])>>,
    /// Border boxes drawn, by key ([`Document::layout_boxes`]).
    record: Option<HashMap<u32, Rect>>,
}

/// Scaled box metrics of a block, in points.
#[derive(Clone, Copy, Default)]
struct Metrics {
    border: [f32; 4],
    padding: [f32; 4],
}

impl Metrics {
    fn horizontal(&self) -> f32 {
        self.border[1] + self.border[3] + self.padding[1] + self.padding[3]
    }

    fn vertical(&self) -> f32 {
        self.border[0] + self.border[2] + self.padding[0] + self.padding[2]
    }
}

impl<'d> Renderer<'_, 'd> {
    fn px(&self, v: f32) -> f32 {
        v * self.scale
    }

    fn len(&self, l: Len, of: f32) -> Option<f32> {
        len_at(l, of, self.scale)
    }

    fn metrics(&self, s: &BoxStyle, of: f32) -> Metrics {
        let b = |side: &BorderSide| self.px(side.used_width());
        let p = |l: Len| self.len(l, of).unwrap_or(0.0).max(0.0);
        Metrics {
            border: [b(&s.border.top), b(&s.border.right), b(&s.border.bottom), b(&s.border.left)],
            padding: [p(s.padding.top), p(s.padding.right), p(s.padding.bottom), p(s.padding.left)],
        }
    }

    /// A `width`-like length as a border-box width (`box-sizing`).
    fn border_len(&self, s: &BoxStyle, l: Len, of: f32, m: &Metrics) -> Option<f32> {
        let v = self.len(l, of)?;
        Some(if s.border_box { v.max(m.horizontal()) } else { v.max(0.0) + m.horizontal() })
    }

    /// A border-box width within `min-width` and `max-width`.
    fn clamp_width(&self, s: &BoxStyle, of: f32, m: &Metrics, w: f32) -> f32 {
        let mut w = w;
        if let Some(max) = self.border_len(s, s.max_width, of, m) {
            w = w.min(max);
        }
        if let Some(min) = self.border_len(s, s.min_width, of, m) {
            w = w.max(min);
        }
        w.max(m.horizontal())
    }

    /// The border-box width from `width`, if not `auto`.
    fn border_width(&self, s: &BoxStyle, of: f32, m: &Metrics) -> Option<f32> {
        self.border_len(s, s.width, of, m).map(|w| self.clamp_width(s, of, m, w))
    }

    /// A `height`-like length as a border-box height; percentages need a
    /// definite containing block height.
    fn border_height(&self, s: &BoxStyle, l: Len, m: &Metrics) -> Option<f32> {
        let v = match l {
            Len::Px(p) => self.px(p),
            Len::Percent(_) | Len::Calc(..) | Len::Clamp(_) => self.len(l, self.definite_h?)?,
            Len::Auto => return None,
        };
        Some(if s.border_box { v.max(m.vertical()) } else { v.max(0.0) + m.vertical() })
    }

    /// The padding box of a border box.
    fn padding_box(&self, r: Rect, s: &BoxStyle) -> Rect {
        let b = &s.border;
        Rect::from_min_max(
            r.min + vec2(self.px(b.left.used_width()), self.px(b.top.used_width())),
            r.max - vec2(self.px(b.right.used_width()), self.px(b.bottom.used_width())),
        )
    }

    fn current_bg(&self) -> Color32 {
        *self.bg.last().unwrap_or(&Color32::WHITE)
    }

    /// Default text color for the current background.
    fn default_text(&self) -> Color32 {
        self.default_text_on(self.current_bg())
    }

    fn default_text_on(&self, bg: Color32) -> Color32 {
        if is_light(bg) != self.dark && contrast(self.theme_text, bg) >= 3.0 {
            self.theme_text
        } else if is_light(bg) {
            Color32::from_gray(28)
        } else {
            Color32::from_gray(225)
        }
    }

    /// Author colors stay unless they would be (nearly) invisible.
    fn readable(&self, c: Color32) -> Color32 {
        self.readable_on(c, self.current_bg())
    }

    fn readable_on(&self, c: Color32, bg: Color32) -> Color32 {
        let solid = over(bg, c);
        if contrast(solid, bg) < 1.6 { self.default_text_on(bg) } else { solid }
    }

    fn link_color_on(&self, bg: Color32) -> Color32 {
        if contrast(self.link, bg) >= 3.0 {
            self.link
        } else if is_light(bg) {
            Color32::from_rgb(6, 69, 173)
        } else {
            Color32::from_rgb(110, 180, 255)
        }
    }

    // -- margins ----------------------------------------------------------------

    fn margin_top(&self, b: &Block, of: f32) -> f32 {
        self.len(b.style.margin.top, of).unwrap_or(0.0)
    }

    fn margin_bottom(&self, b: &Block, of: f32) -> f32 {
        self.len(b.style.margin.bottom, of).unwrap_or(0.0)
    }

    /// Top margin including the first child's when they collapse.
    fn collapsed_top(&self, b: &Block, of: f32) -> f32 {
        let own = self.margin_top(b, of);
        if !self.top_collapses(b, of) {
            return own;
        }
        match &b.content {
            Content::Blocks(children) => match children.iter().find(|c| c.in_flow()) {
                Some(first) => own.max(self.collapsed_top(first, of)),
                None => own,
            },
            _ => own,
        }
    }

    fn collapsed_bottom(&self, b: &Block, of: f32) -> f32 {
        let own = self.margin_bottom(b, of);
        if !self.bottom_collapses(b, of) {
            return own;
        }
        match &b.content {
            Content::Blocks(children) => match children.iter().rev().find(|c| c.in_flow()) {
                Some(last) => own.max(self.collapsed_bottom(last, of)),
                None => own,
            },
            _ => own,
        }
    }

    fn top_collapses(&self, b: &Block, of: f32) -> bool {
        !b.is_bfc() && !b.style.border.top.visible() && self.len(b.style.padding.top, of).unwrap_or(0.0) <= 0.0
    }

    fn bottom_collapses(&self, b: &Block, of: f32) -> bool {
        !b.is_bfc()
            && b.style.height == Len::Auto
            && !b.style.border.bottom.visible()
            && self.len(b.style.padding.bottom, of).unwrap_or(0.0) <= 0.0
    }

    // -- out-of-flow boxes ------------------------------------------------------------

    /// An absolutely positioned box found in the flow at `at`.
    fn register_abs(&mut self, b: &'d Block, at: Pos2) {
        if self.measuring > 0 {
            return;
        }
        if b.style.position == Position::Fixed {
            // The static position as it would be with the document unscrolled.
            let at = self.viewport.min + (at - self.icb.min);
            self.fixed.push(AbsPending { block: b, static_pos: at });
        } else if let Some(f) = self.abs_frames.last_mut() {
            f.push(AbsPending { block: b, static_pos: at });
        }
    }

    /// How far a `position: sticky` box whose border box in the flow starts
    /// at `r.min` (`r.width()` wide) moves to keep its `top`/`bottom`/
    /// `left`/`right` distance from the edges of the nearest scrollport,
    /// without leaving its containing block (last frame's room; nothing
    /// moves before the box was laid out once).
    fn sticky_offset(&self, b: &Block, r: Rect) -> Vec2 {
        let Some(room) = self.cache.sticky.get(&b.key) else { return Vec2::ZERO };
        let port = self.scrollport;
        if !port.is_finite() {
            return Vec2::ZERO;
        }
        let s = &b.style;
        let r = Rect::from_min_size(r.min, vec2(r.width(), room.height));
        let mut d = Vec2::ZERO;
        // `top` wins over `bottom` and `left` over `right`.
        if let Some(bottom) = self.len(s.inset.bottom, port.height()) {
            let over = r.bottom() - (port.bottom() - bottom);
            if over > 0.0 {
                d.y = -over.min(room.up);
            }
        }
        if let Some(top) = self.len(s.inset.top, port.height()) {
            let under = port.top() + top - r.top();
            if under > 0.0 {
                d.y = under.min(room.down);
            }
        }
        if let Some(right) = self.len(s.inset.right, port.width()) {
            let over = r.right() - (port.right() - right);
            if over > 0.0 {
                d.x = -over.min(room.left);
            }
        }
        if let Some(left) = self.len(s.inset.left, port.width()) {
            let under = port.left() + left - r.left();
            if under > 0.0 {
                d.x = under.min(room.right);
            }
        }
        d
    }

    /// Stores the room of the sticky boxes drawn in the content box `cb`.
    fn sticky_room(&mut self, boxes: Vec<(u32, Rect, [f32; 4])>, cb: Rect) {
        for (key, r, margins) in boxes {
            let room = StickyRoom::new(r, margins, cb);
            if self.cache.sticky.insert(key, room).is_none_or(|old| old.differs(&room)) {
                self.repaint = true;
            }
        }
    }

    /// Absolutely positioned boxes whose containing block (padding box `cb`)
    /// is done: they are painted after the flow.
    fn resolve_abs(&mut self, frame: Vec<AbsPending<'d>>, cb: Rect, clip: Rect, bg: Color32) {
        let Some(ctx) = self.contexts.last_mut() else { return };
        for p in frame {
            ctx.push(Layer {
                z: p.block.style.z_index.unwrap_or(0),
                order: p.block.key,
                kind: LayerKind::Abs { block: p.block, cb, static_pos: p.static_pos, clip, bg },
            });
        }
    }

    /// Paints a stacking context's positioned boxes by `z-index`.
    fn paint_layers(&mut self, ui: &mut Ui, mut layers: Vec<Layer<'d>>, root: bool) {
        layers.sort_by_key(|l| (l.z, l.order));
        for layer in layers {
            let behind = root && layer.z < 0;
            let start = next_shape(ui);
            match layer.kind {
                LayerKind::Shapes(shapes, children) => {
                    repaint_shapes(ui, shapes);
                    self.paint_layers(ui, children, false);
                }
                LayerKind::Abs { block, cb, static_pos, clip, bg } => {
                    self.contexts.push(Vec::new());
                    self.bg.push(bg);
                    self.abs_box(ui, block, cb, static_pos, clip);
                    self.bg.pop();
                    let children = self.contexts.pop().unwrap_or_default();
                    self.paint_layers(ui, children, false);
                }
            }
            if behind {
                self.behind_shapes.extend(take_shapes(ui, start).into_iter().map(|s| s.shape));
            }
        }
    }

    /// Lays out an absolutely positioned box in the padding box `cb`.
    fn abs_box(&mut self, ui: &mut Ui, b: &'d Block, cb: Rect, at: Pos2, clip: Rect) {
        let s = &b.style;
        let (cw, ch) = (cb.width(), cb.height());
        let m = self.metrics(s, cw);
        let left = self.len(s.inset.left, cw);
        let right = self.len(s.inset.right, cw);
        let top = self.len(s.inset.top, ch);
        let bottom = self.len(s.inset.bottom, ch);
        let margin = |l: Len| len_at(l, cw, self.scale).unwrap_or(0.0);
        let (mut ml, mr, mut mt, mb) =
            (margin(s.margin.left), margin(s.margin.right), margin(s.margin.top), margin(s.margin.bottom));
        let explicit = self.border_width(s, cw, &m);
        let width = match (explicit, left, right) {
            (Some(w), ..) => w,
            (None, Some(l), Some(r)) => self.clamp_width(s, cw, &m, cw - l - r - ml - mr),
            _ => {
                let room = (cw - left.unwrap_or(0.0) - right.unwrap_or(0.0)).max(0.0);
                let (min, max) = self.measure(ui, b);
                self.clamp_width(s, cw, &m, max.min(room).max(min) - ml - mr)
            }
        };
        if let (Some(l), Some(r), Some(_)) = (left, right, explicit)
            && s.margin.left == Len::Auto
            && s.margin.right == Len::Auto
        {
            ml = ((cw - l - r - width) / 2.0).max(0.0);
        }
        let saved = self.definite_h.replace(ch);
        let spec_h = self.border_height(s, s.height, &m);
        let stretched = match (spec_h, top, bottom) {
            (None, Some(t), Some(bt)) => Some((ch - t - bt - mt - mb).max(m.vertical())),
            _ => None,
        };
        if let (Some(h), Some(t), Some(bt)) = (spec_h, top, bottom)
            && s.margin.top == Len::Auto
            && s.margin.bottom == Len::Auto
        {
            mt = ((ch - t - bt - h) / 2.0).max(0.0);
        }
        let x = match (left, right) {
            (Some(l), _) => cb.left() + l + ml,
            (None, Some(r)) => cb.right() - r - mr - width,
            _ => at.x + ml,
        };
        let y = match (top, bottom) {
            (Some(t), _) => cb.top() + t + mt,
            (None, Some(bt)) => {
                let h = spec_h.or(stretched).unwrap_or_else(|| self.natural_height(ui, b, width));
                cb.bottom() - bt - mb - h
            }
            _ => at.y + mt,
        };
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(("abs", b.key))
                .max_rect(Rect::from_min_size(pos2(x, y), vec2(width, f32::INFINITY)))
                .layout(Layout::top_down(Align::Min)),
        );
        child.set_clip_rect(clip);
        child.spacing_mut().item_spacing = Vec2::ZERO;
        let fit = Fit { bfc: true, width: Some(width), height: stretched, no_margins: true, ..Default::default() };
        let out = self.block_box(&mut child, b, fit);
        self.definite_h = saved;
        // Absolutely positioned boxes count for scrolling (only down and to
        // the right); fixed ones stay in the viewport.
        if s.position != Position::Fixed {
            let doc = ui.min_rect();
            ui.expand_to_include_rect(Rect::from_min_max(doc.min, out.rect.max.max(doc.min)));
        }
    }

    /// Lays out a float at `y` (or lower) in the current formatting context.
    fn float_box(&mut self, ui: &mut Ui, b: &'d Block, y: f32) {
        let left = ui.max_rect().left();
        let avail = ui.available_width();
        let right = left + avail;
        let s = &b.style;
        let m = self.metrics(s, avail);
        let margin = |l: Len| len_at(l, avail, self.scale).unwrap_or(0.0);
        let (ml, mr, mt, mb) =
            (margin(s.margin.left), margin(s.margin.right), margin(s.margin.top), margin(s.margin.bottom));
        let width = match self.border_width(s, avail, &m) {
            Some(w) => w,
            None => {
                // Shrink to fit.
                let (min, max) = self.measure(ui, b);
                self.clamp_width(s, avail, &m, max.min(avail).max(min) - ml - mr)
            }
        };
        let outer = width + ml + mr;
        let (y, l, r) = {
            let ctx = self.floats.last_mut().expect("float context");
            ctx.prune(y);
            let mut y = y.max(ctx.top);
            if let Some(c) = ctx.clear_y(s.clear) {
                y = y.max(c);
            }
            ctx.fit(y, 1.0, outer, left, right)
        };
        // A float wider than the room overflows to the right.
        let x = if s.float == Float::Right { (r - outer).max(left) } else { l };
        let clip = ui.clip_rect();
        let cached = self.cache.floats.get(&b.key).filter(|(w, _)| (*w - avail).abs() < 0.5).map(|(_, s)| *s);
        let size = match cached {
            // Off screen with a known size: only reserve the space.
            Some(size) if !b.fixed && (y > clip.bottom() + 400.0 || y + size.y < clip.top() - 400.0) => size,
            _ => {
                let start = next_shape(ui);
                let mut child = ui.new_child(
                    UiBuilder::new()
                        .id_salt(("fl", b.key))
                        .max_rect(Rect::from_min_size(pos2(x + ml, y + mt), vec2(width, f32::INFINITY)))
                        .layout(Layout::top_down(Align::Min)),
                );
                child.spacing_mut().item_spacing = Vec2::ZERO;
                let fit = Fit { bfc: true, width: Some(width), no_margins: true, ..Default::default() };
                let out = self.block_box(&mut child, b, fit);
                if self.measuring == 0 {
                    let shapes = take_shapes(ui, start);
                    if let Some(ctx) = self.floats.last_mut() {
                        ctx.shapes.extend(shapes);
                    }
                }
                let size = vec2(outer, out.rect.height() + mt + mb);
                self.cache.floats.insert(b.key, (avail, size));
                size
            }
        };
        if let Some(ctx) = self.floats.last_mut() {
            ctx.add(s.float, Rect::from_min_size(pos2(x, y), size));
        }
    }

    /// Whether floats are near `y` in the current formatting context.
    fn floats_below(&self, y: f32) -> bool {
        self.floats.last().is_some_and(|f| f.bottom() > y)
    }

    /// Ends a formatting context in `ui`: grows it to contain the floats and
    /// paints them.
    fn contain_floats(&mut self, ui: &mut Ui, ctx: FloatCtx) {
        let bottom = ctx.bottom();
        if bottom.is_finite() && bottom > ui.min_rect().bottom() {
            ui.expand_to_include_y(bottom);
        }
        repaint_shapes(ui, ctx.shapes);
    }

    /// Lays out a box that must not overlap floats (a formatting context:
    /// tables, flex and grid containers, `overflow: hidden`, ...) in the
    /// room beside them, or below them when it does not fit.
    fn beside_floats(&mut self, ui: &mut Ui, b: &'d Block) {
        let left = ui.max_rect().left();
        let avail = ui.available_width();
        let right = left + avail;
        let m = self.metrics(&b.style, avail);
        let margins =
            self.len(b.style.margin.left, avail).unwrap_or(0.0) + self.len(b.style.margin.right, avail).unwrap_or(0.0);
        let need = match self.border_width(&b.style, avail, &m) {
            Some(w) => w + margins,
            None => self.measure(ui, b).0,
        };
        let y0 = ui.cursor().top();
        let (y, l, r) = match self.floats.last() {
            Some(ctx) => ctx.fit(y0, 1.0, need, left, right),
            None => (y0, left, right),
        };
        if y > y0 {
            ui.add_space(y - y0);
        }
        if l <= left && r >= right {
            self.block_box(ui, b, Fit::default());
            return;
        }
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(("bf", b.key))
                .max_rect(Rect::from_min_size(pos2(l, y), vec2((r - l).max(0.0), f32::INFINITY)))
                .layout(Layout::top_down(Align::Min)),
        );
        child.spacing_mut().item_spacing = Vec2::ZERO;
        self.block_box(&mut child, b, Fit::default());
        let used = child.min_rect();
        ui.allocate_rect(
            Rect::from_min_max(pos2(left, y), pos2(right.max(used.right()), used.bottom().max(y))),
            Sense::hover(),
        );
    }

    /// Boxes that are laid out beside floats as a whole (instead of only
    /// their lines avoiding them).
    fn avoids_floats(b: &Block) -> bool {
        b.is_bfc() || matches!(b.content, Content::Table(_) | Content::Row { .. } | Content::Grid(_))
    }

    // -- blocks -------------------------------------------------------------------

    /// Children of a block in normal flow, with collapsed vertical margins.
    /// The parent already added the first child's top margin when they
    /// collapse (and adds the last child's bottom margin).
    fn blocks(&mut self, ui: &mut Ui, children: &'d [Block], skip_first_top: bool, skip_last_bottom: bool) {
        let width = ui.available_width();
        let left = ui.max_rect().left();
        let clip = ui.clip_rect();
        let mut prev_bottom: Option<f32> = None;
        let last = children.iter().rposition(Block::in_flow);
        for (i, child) in children.iter().enumerate() {
            if child.style.position.out_of_flow() {
                let y = ui.cursor().top() + prev_bottom.unwrap_or(0.0);
                self.register_abs(child, pos2(left, y));
                continue;
            }
            if child.style.float != Float::None {
                let y = ui.cursor().top() + prev_bottom.unwrap_or(0.0);
                self.float_box(ui, child, y);
                continue;
            }
            let top = self.collapsed_top(child, width);
            let mut gap = match prev_bottom {
                Some(p) => p.max(top),
                None if skip_first_top => 0.0,
                None => top,
            };
            // Clearance: below the floats on the cleared side.
            if let Some(cy) = self.floats.last().and_then(|f| f.clear_y(child.style.clear)) {
                let y = ui.cursor().top() + gap;
                if cy > y {
                    gap += cy - y;
                }
            }
            ui.add_space(gap);
            let cached = self.cache.heights.get(&child.key).filter(|(w, _)| (*w - width).abs() < 0.5).map(|x| x.1);
            let y = ui.cursor().top();
            let floats = self.floats_below(y);
            // Fixed and sticky boxes may be visible away from their place in the flow.
            let pinned = child.fixed || child.style.position == Position::Sticky;
            match cached {
                // Off screen with a known height: only reserve the space.
                Some(h)
                    if !floats
                        && !child.loose_floats
                        && !pinned
                        && (y > clip.bottom() + 400.0 || y + h < clip.top() - 400.0) =>
                {
                    ui.allocate_exact_size(vec2(width, h), Sense::hover());
                }
                _ => {
                    if floats && Self::avoids_floats(child) {
                        self.beside_floats(ui, child);
                    } else {
                        self.block_box(ui, child, Fit::default());
                    }
                    let h = ui.cursor().top() - y;
                    self.cache.heights.insert(child.key, (width, h));
                }
            }
            let bottom = self.collapsed_bottom(child, width);
            if Some(i) == last {
                if !skip_last_bottom {
                    ui.add_space(bottom);
                }
            } else {
                prev_bottom = Some(bottom);
            }
        }
    }

    /// Draws a block's box (background, borders, marker) and content. The
    /// caller handles vertical margins.
    fn block_box(&mut self, ui: &mut Ui, b: &'d Block, fit: Fit) -> BoxOut<'d> {
        let avail = ui.available_width();
        let s = &b.style;
        let m = self.metrics(s, avail);
        let margins = !fit.no_margins;
        let auto_l = margins && s.margin.left == Len::Auto;
        let auto_r = margins && s.margin.right == Len::Auto;
        let mut ml = if margins { self.len(s.margin.left, avail).unwrap_or(0.0) } else { 0.0 };
        let mr = if margins { self.len(s.margin.right, avail).unwrap_or(0.0) } else { 0.0 };
        let width = match fit.width {
            Some(w) => w,
            None => match self.border_width(s, avail, &m) {
                Some(w) => w,
                None => {
                    let full = (avail - ml - mr).max(0.0);
                    let w = match &b.content {
                        // Tables shrink to fit.
                        Content::Table(_) => {
                            let (_, max) = self.measure(ui, b);
                            (max - ml - mr).min(full)
                        }
                        _ => full,
                    };
                    self.clamp_width(s, avail, &m, w)
                }
            },
        }
        .max(m.horizontal());
        if auto_l && auto_r && !fit.root {
            ml = ((avail - width) / 2.0).max(0.0);
        } else if auto_l && !fit.root {
            ml = (avail - width - mr).max(0.0);
        }
        let left = ui.max_rect().left();
        let top = ui.cursor().top();
        let live = self.measuring == 0;
        // `relative` offsets move the box, not the space it takes; so does
        // sticking to the edge of the scrollport.
        let offset = match s.position {
            Position::Relative => {
                let dx = self.len(s.inset.left, avail).or_else(|| self.len(s.inset.right, avail).map(|r| -r));
                let of = self.definite_h.unwrap_or(0.0);
                let dy = self.len(s.inset.top, of).or_else(|| self.len(s.inset.bottom, of).map(|b| -b));
                vec2(dx.unwrap_or(0.0), dy.unwrap_or(0.0))
            }
            Position::Sticky if live && !fit.root => {
                self.sticky_offset(b, Rect::from_min_size(pos2(left + ml, top), vec2(width, 0.0)))
            }
            _ => Vec2::ZERO,
        };
        let box_min = pos2(left + ml, top) + offset;
        let content_min = box_min + vec2(m.border[3] + m.padding[3], m.border[0] + m.padding[0]);
        let content_w = (width - m.horizontal()).max(0.0);
        let spec_h = fit.height.or_else(|| self.border_height(s, s.height, &m));
        let min_h = self.border_height(s, s.min_height, &m).unwrap_or(0.0);
        let max_h = self.border_height(s, s.max_height, &m).unwrap_or(f32::INFINITY);

        let positioned = live && (fit.root || s.position != Position::Static);
        // Sticky boxes paint above the flow that scrolls under them.
        let stacking = live
            && !fit.root
            && !fit.defer_abs
            && s.position != Position::Static
            && (s.z_index.is_some() || s.position == Position::Sticky);
        let sticky_cb = live && has_sticky_child(b);
        if sticky_cb {
            self.sticky_frames.push(Vec::new());
        }
        let paint_start = stacking.then(|| next_shape(ui));
        let placeholder = ui.painter().add(Shape::Noop);
        if fit.root && live {
            self.behind = Some(ui.painter().add(Shape::Noop));
        }
        if positioned {
            self.abs_frames.push(Vec::new());
        }
        if stacking {
            self.contexts.push(Vec::new());
        }
        let pushed = match s.background {
            Some(bg) => {
                self.bg.push(over(self.current_bg(), bg));
                true
            }
            None => false,
        };
        let inner_bg = self.current_bg();
        let bfc = fit.bfc || fit.root || b.is_bfc();
        if bfc {
            self.floats.push(FloatCtx::new());
        }
        let parent_h = self.definite_h;
        self.definite_h = spec_h.map(|h| (h.min(max_h).max(min_h) - m.vertical()).max(0.0));
        let layout = Layout::top_down(Align::Min);
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(("b", b.key))
                .max_rect(Rect::from_min_size(content_min, vec2(content_w, f32::INFINITY)))
                .layout(layout),
        );
        child.spacing_mut().item_spacing = Vec2::ZERO;
        let padding_box = |h: f32| {
            Rect::from_min_size(
                content_min - vec2(m.padding[3], m.padding[0]),
                vec2(content_w + m.padding[1] + m.padding[3], h),
            )
        };
        // Visible height of an `overflow` box, when definite.
        let clip_h = spec_h.or(max_h.is_finite().then_some(max_h)).map(|h| (h - m.vertical()).max(0.0));
        let contains = fit.bfc || b.is_bfc();
        // A box with `overflow` is the scroll container of its sticky
        // descendants, even when it cannot scroll.
        let parent_port = self.scrollport;
        if !fit.root && s.overflow != Overflow::Visible {
            self.scrollport = match (s.overflow, clip_h) {
                (Overflow::Scroll, Some(h)) => Rect::from_min_size(content_min, vec2(content_w, h)),
                (_, h) => padding_box(h.map_or(f32::INFINITY, |h| h + m.padding[0] + m.padding[2])),
            };
        }
        // The scrolled content (sticky boxes stay inside it).
        let mut scrolled: Option<Rect> = None;
        match (s.overflow, clip_h) {
            (Overflow::Scroll, Some(h)) => {
                child.shrink_clip_rect(padding_box(h + m.padding[0] + m.padding[2]));
                // The edge fade would use the panel color, not the box's background.
                child.spacing_mut().scroll.fade.strength = 0.0;
                egui::ScrollArea::both().id_salt(("ov", b.key)).max_height(h).auto_shrink([false, true]).show(
                    &mut child,
                    |ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        self.content(ui, b, content_w, true);
                        let r = ui.min_rect();
                        scrolled = Some(Rect::from_min_size(r.min, vec2(r.width().max(content_w), r.height())));
                    },
                );
            }
            (Overflow::Hidden, h) => {
                let h = h.map_or(f32::INFINITY, |h| h + m.padding[0] + m.padding[2]);
                child.shrink_clip_rect(padding_box(h));
                self.content(&mut child, b, content_w, contains);
            }
            _ => self.content(&mut child, b, content_w, contains),
        }
        self.scrollport = parent_port;
        if bfc {
            let ctx = self.floats.pop().unwrap_or_else(FloatCtx::new);
            self.contain_floats(&mut child, ctx);
        } else if s.clearfix
            && let Some(y) = self.floats.last().and_then(|f| f.clear_y(Clear::Both))
            && y > child.min_rect().bottom()
        {
            child.expand_to_include_y(y);
        }
        self.definite_h = parent_h;
        let content = child.min_rect();
        let content_h = content.height().max(0.0);
        let natural = spec_h.unwrap_or(content_h + m.vertical()).min(max_h).max(min_h).max(m.vertical());
        let box_rect = Rect::from_min_size(box_min, vec2(width, natural));
        if pushed {
            self.bg.pop();
        }
        if sticky_cb {
            let boxes = self.sticky_frames.pop().unwrap_or_default();
            let cb = Rect::from_min_size(content_min, vec2(content_w, (natural - m.vertical()).max(0.0)));
            self.sticky_room(boxes, scrolled.unwrap_or(cb));
        }
        let mut shapes = self.box_shapes(box_rect, s);
        if let Some(marker) = &b.marker {
            self.marker_shape(ui.ctx(), marker, s, content_min, &mut shapes);
        }
        ui.painter().set(placeholder, Shape::Vec(shapes));
        // The parent makes room for the box where it would be without the offset.
        let alloc = Rect::from_min_max(
            pos2(left, top),
            pos2((box_rect.right() + mr).max(content.right()) - offset.x, box_rect.bottom() - offset.y),
        );
        ui.allocate_rect(alloc, Sense::hover());
        if live {
            if let Some(rec) = &mut self.record {
                rec.insert(b.key, box_rect);
            }
            if s.position == Position::Sticky && !fit.root {
                let (mt, mb) = (self.margin_top(b, avail), self.margin_bottom(b, avail));
                if let Some(frame) = self.sticky_frames.last_mut() {
                    frame.push((b.key, box_rect.translate(-offset), [mt, mr, mb, ml]));
                }
            }
            if let Some(w) = fit.width
                && let Some((pw, ph)) = self.cache.natural.insert(b.key, (w, natural))
                && (pw - w).abs() < 0.5
                && (ph - natural).abs() > 0.5
            {
                self.repaint = true;
            }
        }
        let clip = if s.overflow != Overflow::Visible {
            self.padding_box(box_rect, s).intersect(ui.clip_rect())
        } else {
            ui.clip_rect()
        };
        let mut abs = None;
        if positioned {
            let frame = self.abs_frames.pop().unwrap_or_default();
            if fit.defer_abs {
                abs = Some(frame);
            } else {
                let cb = if fit.root { self.icb } else { self.padding_box(box_rect, s) };
                self.resolve_abs(frame, cb, clip, inner_bg);
            }
        }
        if let Some(start) = paint_start {
            let children = self.contexts.pop().unwrap_or_default();
            let shapes = take_shapes(ui, start);
            if let Some(ctx) = self.contexts.last_mut() {
                ctx.push(Layer { z: s.z_index.unwrap_or(0), order: b.key, kind: LayerKind::Shapes(shapes, children) });
            }
        }
        BoxOut {
            rect: box_rect,
            natural,
            placeholder,
            painter: ui.painter().clone(),
            content_min,
            abs,
            clip,
            bg: inner_bg,
        }
    }

    /// Gives a drawn box a taller border box (stretched flex and grid items).
    fn restretch(&mut self, b: &Block, out: &mut BoxOut<'d>, height: f32) {
        out.rect.set_height(height);
        let mut shapes = self.box_shapes(out.rect, &b.style);
        if let Some(marker) = &b.marker {
            let ctx = out.painter.ctx().clone();
            self.marker_shape(&ctx, marker, &b.style, out.content_min, &mut shapes);
        }
        out.painter.set(out.placeholder, Shape::Vec(shapes));
        if self.measuring == 0
            && let Some(rec) = &mut self.record
        {
            rec.insert(b.key, out.rect);
        }
    }

    /// Places the absolutely positioned descendants of a box drawn with
    /// [`Fit::defer_abs`].
    fn finish_box(&mut self, b: &Block, out: &mut BoxOut<'d>) {
        if let Some(frame) = out.abs.take() {
            let cb = self.padding_box(out.rect, &b.style);
            self.resolve_abs(frame, cb, out.clip, out.bg);
        }
    }

    /// Border-box height of `b` laid out at border-box width `w`: cached, or
    /// laid out invisibly once.
    fn natural_height(&mut self, ui: &mut Ui, b: &'d Block, w: f32) -> f32 {
        if let Some((cw, h)) = self.cache.natural.get(&b.key)
            && (cw - w).abs() < 0.5
        {
            return *h;
        }
        self.measuring += 1;
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(("m", b.key))
                .max_rect(Rect::from_min_size(ui.cursor().min, vec2(w, f32::INFINITY)))
                .layout(Layout::top_down(Align::Min))
                .invisible(),
        );
        child.spacing_mut().item_spacing = Vec2::ZERO;
        let out =
            self.block_box(&mut child, b, Fit { bfc: true, width: Some(w), no_margins: true, ..Default::default() });
        self.measuring -= 1;
        self.cache.natural.insert(b.key, (w, out.natural));
        out.natural
    }

    fn marker_shape(&mut self, ctx: &egui::Context, marker: &str, s: &BoxStyle, at: Pos2, out: &mut Vec<Shape>) {
        let size = self.px(s.font_size).max(6.0);
        let font = FontId::proportional(size);
        let color = s.color.map(|c| self.readable(c)).unwrap_or_else(|| self.default_text());
        let gap = size * 0.5;
        let row_h = ctx.fonts_mut(|f| f.row_height(&font));
        // Bullets are drawn as shapes: not every font has the glyphs.
        let r = size * 0.17;
        let center = pos2(at.x - gap - r, at.y + row_h * 0.55);
        match marker {
            "•" => out.push(Shape::circle_filled(center, r, color)),
            "◦" => out.push(Shape::circle_stroke(center, r * 0.9, Stroke::new((size / 14.0).max(1.0), color))),
            "▪" => out.push(Shape::rect_filled(Rect::from_center_size(center, Vec2::splat(r * 1.8)), 0.0, color)),
            _ => {
                let galley = ctx.fonts_mut(|f| f.layout_no_wrap(marker.to_string(), font, color));
                let pos = pos2(at.x - gap - galley.size().x, at.y);
                out.push(Shape::galley(pos, galley, color));
            }
        }
    }

    fn box_shapes(&self, r: Rect, s: &BoxStyle) -> Vec<Shape> {
        let mut out = Vec::new();
        let radius =
            if s.radius_percent > 0.0 { r.size().min_elem() * s.radius_percent / 100.0 } else { self.px(s.radius) };
        if let Some(bg) = s.background {
            out.push(Shape::rect_filled(r, radius, bg));
        }
        let default = self.default_text();
        let color = |side: &BorderSide| side.color.unwrap_or(default);
        let b = &s.border;
        let uniform = [b.top, b.right, b.bottom, b.left].iter().all(|x| x == &b.top) && b.top.visible();
        if uniform && radius > 0.0 {
            let w = self.px(b.top.width);
            out.push(Shape::rect_stroke(
                r.shrink(w / 2.0),
                radius,
                Stroke::new(w, color(&b.top)),
                egui::StrokeKind::Middle,
            ));
            return out;
        }
        let sides = [
            (&b.top, [r.left_top(), r.right_top()], vec2(0.0, 1.0)),
            (&b.right, [r.right_top(), r.right_bottom()], vec2(-1.0, 0.0)),
            (&b.bottom, [r.left_bottom(), r.right_bottom()], vec2(0.0, -1.0)),
            (&b.left, [r.left_top(), r.left_bottom()], vec2(1.0, 0.0)),
        ];
        for (side, [a, z], inward) in sides {
            if !side.visible() {
                continue;
            }
            let w = self.px(side.width).max(1.0 / 2.0);
            let off = inward * (w / 2.0);
            let (a, z) = (a + off, z + off);
            let stroke = Stroke::new(w, color(side));
            match side.style {
                BorderStyle::Dashed => {
                    out.extend(Shape::dashed_line(&[a, z], stroke, 4.0 * w.max(1.0), 3.0 * w.max(1.0)))
                }
                BorderStyle::Dotted => out.extend(Shape::dotted_line(&[a, z], stroke.color, 2.0 * w.max(1.0), w / 2.0)),
                BorderStyle::Double if w >= 3.0 => {
                    let t = w / 3.0;
                    let thin = Stroke::new(t, stroke.color);
                    let o1 = inward * (-w / 2.0 + t / 2.0);
                    let o2 = inward * (w / 2.0 - t / 2.0);
                    out.push(Shape::line_segment([a + o1, z + o1], thin));
                    out.push(Shape::line_segment([a + o2, z + o2], thin));
                }
                _ => out.push(Shape::line_segment([a, z], stroke)),
            }
        }
        out
    }

    /// Draws a block's content (not its box) into `ui`. `contained`: the box
    /// is a formatting context (its children's margins stay inside).
    fn content(&mut self, ui: &mut Ui, b: &'d Block, width: f32, contained: bool) {
        match &b.content {
            Content::Blocks(children) => {
                let skip_top = !contained && self.top_collapses(b, width);
                let skip_bottom = !contained && self.bottom_collapses(b, width);
                self.blocks(ui, children, skip_top, skip_bottom);
            }
            Content::Inline(items) => self.inline_content(ui, b, items),
            Content::Table(t) => self.table(ui, b, t),
            Content::Rule => {}
            Content::Row { blocks, gap, wrap, row_gap, justify, align, flex } => {
                let spec = RowSpec {
                    gap: *gap,
                    row_gap: *row_gap,
                    wrap: *wrap,
                    justify: *justify,
                    align: *align,
                    flex: *flex,
                };
                self.row(ui, blocks, spec);
            }
            Content::Grid(g) => self.grid(ui, b, g),
        }
    }

    // -- inline content -------------------------------------------------------------

    /// A paragraph, next to the floats of its formatting context if any.
    fn inline_content(&mut self, ui: &mut Ui, b: &'d Block, items: &'d [Inline]) {
        let y0 = ui.cursor().top();
        if !self.floats_below(y0) {
            self.paragraph(ui, items, &b.style);
            return;
        }
        let x0 = ui.max_rect().left();
        let x1 = x0 + ui.available_width();
        let line = self.px(b.style.font_size.max(1.0)) * b.style.line_height.unwrap_or(1.25);
        let s = &b.style;
        let m = self.metrics(s, 0.0);
        let own =
            m.horizontal() + self.len(s.margin.left, 0.0).unwrap_or(0.0) + self.len(s.margin.right, 0.0).unwrap_or(0.0);
        let need = (self.measure(ui, b).0 - own).max(0.0);
        if s.wrap && items.iter().all(|i| matches!(i, Inline::Text(_) | Inline::Break)) {
            self.flow_text(ui, b, items, line, need);
            return;
        }
        let (y, l, r) = match self.floats.last() {
            Some(ctx) => ctx.fit(y0, line, need, x0, x1),
            None => (y0, x0, x1),
        };
        if y > y0 {
            ui.add_space(y - y0);
        }
        if l <= x0 && r >= x1 {
            self.paragraph(ui, items, &b.style);
            return;
        }
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(("p", b.key))
                .max_rect(Rect::from_min_size(pos2(l, y), vec2((r - l).max(0.0), f32::INFINITY)))
                .layout(Layout::top_down(Align::Min)),
        );
        child.spacing_mut().item_spacing = Vec2::ZERO;
        self.paragraph(&mut child, items, &b.style);
        let used = child.min_rect();
        ui.allocate_rect(
            Rect::from_min_max(pos2(x0, y), pos2(x1.max(used.right()), used.bottom().max(y))),
            Sense::hover(),
        );
    }

    /// Text beside floats: each run of lines is laid out in the room next to
    /// the floats it overlaps, and the lines below a float that ends earlier
    /// use the full width again (line boxes are only shortened while beside
    /// a float). `line`: estimated line height; `need`: the longest word.
    fn flow_text(&mut self, ui: &mut Ui, b: &'d Block, items: &'d [Inline], line: f32, need: f32) {
        let x0 = ui.max_rect().left();
        let x1 = x0 + ui.available_width();
        let y0 = ui.cursor().top();
        let refs: Vec<&Inline> = items.iter().collect();
        let (mut job, mut links, _) = self.job(&refs, &b.style);
        let (mut y, mut right) = (y0, x1);
        for piece in 0..FLOW_PIECES {
            let Some(ctx) = self.floats.last() else { break };
            let (top, l, r) = ctx.fit(y, line, need, x0, x1);
            let w = (r - l).max(0.0);
            // The first line whose room differs from this one's starts the next run.
            let split = if piece + 1 < FLOW_PIECES {
                let mut probe = job.clone();
                probe.wrap = TextWrapping::wrap_at_width(w);
                let galley = ui.fonts_mut(|f| f.layout_job(probe));
                let mut chars = 0;
                let mut split = None;
                for (i, row) in galley.rows.iter().enumerate() {
                    if i > 0 {
                        let (rl, rr) = ctx.band(top + row.min_y(), top + row.max_y(), x0, x1);
                        if (rl - l).abs() > 0.5 || (rr - r).abs() > 0.5 {
                            split = Some(chars);
                            break;
                        }
                    }
                    chars += row.char_count_including_newline().0;
                }
                split.filter(|c| *c > 0)
            } else {
                None
            };
            let (head, tail) = match split {
                Some(at) => {
                    let (head, tail) = split_job(job, links, at);
                    (head, Some(tail))
                }
                None => ((job, links), None),
            };
            let mut child = ui.new_child(
                UiBuilder::new()
                    .id_salt(("p", b.key, piece))
                    .max_rect(Rect::from_min_size(pos2(l, top), vec2(w, f32::INFINITY)))
                    .layout(Layout::top_down(Align::Min)),
            );
            child.spacing_mut().item_spacing = Vec2::ZERO;
            let bold = has_faux_bold(&head.0);
            self.text_block(&mut child, head.0, &head.1, bold, &b.style);
            let used = child.min_rect();
            right = right.max(used.right());
            // Always move down, so every run takes at least one line.
            y = used.bottom().max(top + 1.0);
            match tail {
                Some(rest) if !rest.0.text.is_empty() => (job, links) = rest,
                _ => break,
            }
        }
        ui.allocate_rect(Rect::from_min_max(pos2(x0, y0), pos2(right, y.max(y0))), Sense::hover());
    }

    fn text_format(&self, s: &TextStyle, link: bool) -> TextFormat {
        let size = self.px(s.size).max(6.0);
        let family = if s.monospace {
            FontFamily::Monospace
        } else if s.bold && self.bold_family.is_some() {
            self.bold_family.clone().unwrap_or_default()
        } else {
            FontFamily::Proportional
        };
        let bg = s.background.map(|b| over(self.current_bg(), b)).unwrap_or(self.current_bg());
        let color = match s.color {
            Some(c) => self.readable_on(c, bg),
            None if link => self.link_color_on(bg),
            None => self.default_text_on(bg),
        };
        let mut f = TextFormat { font_id: FontId::new(size, family), color, italics: s.italic, ..Default::default() };
        if let Some(bg) = s.background {
            f.background = bg;
        }
        if s.underline {
            f.underline = Stroke::new((size / 14.0).max(1.0), color);
        }
        if s.strike {
            f.strikethrough = Stroke::new((size / 14.0).max(1.0), color);
        }
        f.valign = match s.valign {
            VAlign::Super => Align::TOP,
            _ => Align::BOTTOM,
        };
        if let Some(lh) = s.line_height {
            f.line_height = Some((lh * size).round());
        }
        if self.faux_bold(s) {
            // Invisible difference that keeps bold sections from being merged
            // with neighbours, so the overstrike can find them.
            f.expand_bg = FAUX_BOLD_MARK;
        }
        f
    }

    fn faux_bold(&self, s: &TextStyle) -> bool {
        s.bold && (s.monospace || self.bold_family.is_none())
    }

    /// Builds the layout job of a run of text items; returns the link ranges
    /// (char offsets) and whether any section needs faux bold.
    fn job(&self, items: &[&Inline], style: &BoxStyle) -> (LayoutJob, Vec<Link>, bool) {
        let mut job = LayoutJob::default();
        let mut links = Vec::new();
        let mut chars = 0usize;
        let mut bold = false;
        for it in items {
            match it {
                Inline::Text(t) => {
                    let f = self.text_format(&t.style, t.link.is_some());
                    let n = t.text.chars().count();
                    if let Some(l) = &t.link {
                        links.push((chars, chars + n, l.clone()));
                    }
                    bold |= self.faux_bold(&t.style);
                    job.append(&t.text, 0.0, f);
                    chars += n;
                }
                Inline::Break => {
                    let size = self.px(style.font_size).max(6.0);
                    let line_height = style.line_height.map(|lh| (lh * size).round());
                    let f = TextFormat { font_id: FontId::proportional(size), line_height, ..Default::default() };
                    job.append("\n", 0.0, f);
                    chars += 1;
                }
                Inline::Image(_) | Inline::Block(_) => {}
            }
        }
        job.break_on_newline = true;
        (job, links, bold)
    }

    /// (outer width, border-box width) of an inline-block in a line `avail` wide.
    fn inline_block_width(&mut self, ui: &Ui, b: &Block, avail: f32) -> (f32, f32) {
        let s = &b.style;
        let m = self.metrics(s, avail);
        let margins = self.len(s.margin.left, avail).unwrap_or(0.0) + self.len(s.margin.right, avail).unwrap_or(0.0);
        let w = match self.border_width(s, avail, &m) {
            Some(w) => w,
            None => {
                let (min, max) = self.measure(ui, b);
                self.clamp_width(s, avail, &m, max.min(avail).max(min) - margins)
            }
        };
        (w + margins, w)
    }

    fn inline_block(&mut self, ui: &mut Ui, b: &'d Block, avail: f32) {
        let (outer, w) = self.inline_block_width(ui, b, avail);
        // The height is needed up front to center the box on the line.
        let h = self.natural_height(ui, b, w);
        // Negative margins can make the margin box narrower than nothing.
        ui.allocate_ui_with_layout(vec2(outer.max(0.0), h), Layout::top_down(Align::Min), |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            let fit = Fit { bfc: true, width: Some(w), ..Default::default() };
            let mut out = self.block_box(ui, b, fit);
            self.finish_box(b, &mut out);
        });
    }

    fn paragraph(&mut self, ui: &mut Ui, items: &'d [Inline], style: &BoxStyle) {
        let has_images = items.iter().any(|i| matches!(i, Inline::Image(_) | Inline::Block(_)));
        if !has_images {
            let refs: Vec<&Inline> = items.iter().collect();
            let (job, links, bold) = self.job(&refs, style);
            self.text_block(ui, job, &links, bold, style);
            return;
        }
        let only_one = items.len() == 1;
        if only_one && style.text_align != TextAlign::Left {
            let align = if style.text_align == TextAlign::Right { Align::Max } else { Align::Center };
            ui.with_layout(Layout::top_down(align), |ui| match &items[0] {
                Inline::Image(img) => self.image(ui, img),
                Inline::Block(b) => {
                    let avail = ui.available_width();
                    self.inline_block(ui, b, avail);
                }
                _ => {}
            });
            return;
        }
        let avail = ui.available_width();
        let mut line_h = self.px(style.font_size).max(6.0) * style.line_height.unwrap_or(1.25);
        // Boxes are centered on the line (like `vertical-align: middle`),
        // images sit on the baseline.
        let boxes = items.iter().any(|i| matches!(i, Inline::Block(_)));
        if boxes {
            for it in items {
                if let Inline::Block(b) = it {
                    let (_, w) = self.inline_block_width(ui, b, avail);
                    line_h = line_h.max(self.natural_height(ui, b, w));
                }
            }
        }
        let cross = if boxes { Align::Center } else { Align::Max };
        let layout = Layout::left_to_right(cross).with_main_wrap(true);
        ui.allocate_ui_with_layout(vec2(avail, line_h), layout, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            let mut run: Vec<&Inline> = Vec::new();
            for it in items {
                match it {
                    Inline::Image(img) => {
                        if !run.is_empty() {
                            self.text_label(ui, &run, style, false);
                            run.clear();
                        }
                        self.image(ui, img);
                    }
                    Inline::Block(b) => {
                        if !run.is_empty() {
                            self.text_label(ui, &run, style, false);
                            run.clear();
                        }
                        self.inline_block(ui, b, avail);
                    }
                    Inline::Break => {
                        if !run.is_empty() {
                            self.text_label(ui, &run, style, false);
                            run.clear();
                        }
                        ui.end_row();
                    }
                    Inline::Text(_) => run.push(it),
                }
            }
            if !run.is_empty() {
                self.text_label(ui, &run, style, false);
            }
        });
    }

    /// Text-only lines, aligned with `text-align`.
    fn text_block(&mut self, ui: &mut Ui, job: LayoutJob, links: &[Link], bold: bool, style: &BoxStyle) {
        let align = match style.text_align {
            TextAlign::Center => Align::Center,
            TextAlign::Right => Align::Max,
            _ => Align::Min,
        };
        let justify = style.text_align == TextAlign::Justify;
        let layout = Layout::top_down(align).with_cross_justify(justify);
        ui.scope_builder(UiBuilder::new().layout(layout), |ui| {
            self.job_label(ui, job, links, bold, style, justify);
        });
    }

    fn text_label(&mut self, ui: &mut Ui, items: &[&Inline], style: &BoxStyle, justify: bool) {
        let (job, links, bold) = self.job(items, style);
        self.job_label(ui, job, &links, bold, style, justify);
    }

    fn job_label(
        &mut self,
        ui: &mut Ui,
        mut job: LayoutJob,
        links: &[Link],
        bold: bool,
        style: &BoxStyle,
        justify: bool,
    ) {
        if job.text.is_empty() {
            return;
        }
        job.justify = justify;
        let wrap = if style.wrap { TextWrapMode::Wrap } else { TextWrapMode::Extend };
        let selectable = self.opts.selectable && self.measuring == 0;
        let mut label = Label::new(job).wrap_mode(wrap).selectable(selectable);
        if !links.is_empty() && !selectable {
            label = label.sense(Sense::click());
        }
        let (pos, galley, response) = label.layout_in_ui(ui);
        if !ui.is_rect_visible(response.rect) {
            return;
        }
        let fallback = self.default_text();
        if selectable {
            LabelSelectionState::label_text_selection(ui, &response, pos, galley.clone(), fallback, Stroke::NONE);
        } else {
            ui.painter().galley(pos, galley.clone(), fallback);
        }
        if bold {
            // Overstrike bold sections, keeping the exact same layout.
            let mut job2 = (*galley.job).clone();
            for sec in &mut job2.sections {
                if sec.format.expand_bg == FAUX_BOLD_MARK {
                    sec.format.background = Color32::TRANSPARENT;
                    sec.format.underline = Stroke::NONE;
                    sec.format.strikethrough = Stroke::NONE;
                } else {
                    sec.format.color = Color32::TRANSPARENT;
                    sec.format.background = Color32::TRANSPARENT;
                    sec.format.underline = Stroke::NONE;
                    sec.format.strikethrough = Stroke::NONE;
                }
            }
            let size = job2
                .sections
                .iter()
                .filter(|s| s.format.color != Color32::TRANSPARENT)
                .map(|s| s.format.font_id.size)
                .fold(0.0f32, f32::max);
            let overlay = ui.fonts_mut(|f| f.layout_job(job2));
            let offset = (size * 0.06).clamp(0.5, 1.6);
            ui.painter().galley(pos + vec2(offset, 0.0), overlay, Color32::TRANSPARENT);
        }
        if links.is_empty() {
            return;
        }
        let pointer =
            response.hover_pos().or_else(|| if response.clicked() { response.interact_pointer_pos() } else { None });
        let Some(p) = pointer else { return };
        let local = p - pos;
        let over_row = galley.rows.iter().any(|r| r.rect().expand(1.0).contains(pos2(local.x, local.y)));
        if !over_row {
            return;
        }
        let idx = galley.cursor_from_pos(local).index.0;
        let Some((_, _, href)) = links.iter().find(|(a, b, _)| *a <= idx && idx < *b) else { return };
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        let href = href.clone();
        if response.clicked() {
            self.host.open_link(&href);
        }
        response.on_hover_text_at_pointer(href);
    }

    fn image(&mut self, ui: &mut Ui, img: &ImageRef) {
        let source = if let Some(data) = &img.data {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            data.hash(&mut h);
            let ext = img.mime.as_deref().and_then(|m| m.strip_prefix("image/")).unwrap_or("png");
            let ext = ext.split('+').next().unwrap_or(ext);
            Some(egui::ImageSource::Bytes {
                uri: format!("bytes://merpati-html/{:016x}.{ext}", h.finish()).into(),
                bytes: egui::load::Bytes::Shared(data.clone()),
            })
        } else if img.src.starts_with("http://") || img.src.starts_with("https://") {
            self.host.remote_image(&img.src).map(|bytes| egui::ImageSource::Bytes {
                uri: format!("bytes://merpati-html/remote/{}", img.src).into(),
                bytes,
            })
        } else {
            None
        };
        let max_w = ui.available_size_before_wrap().x.max(16.0);
        let want = |w: Option<f32>, h: Option<f32>, natural: Vec2| -> Vec2 {
            let (w, h) = (w.map(|v| v * self.scale), h.map(|v| v * self.scale));
            let natural = natural * self.scale;
            let mut size = match (w, h) {
                (Some(w), Some(h)) => vec2(w, h),
                (Some(w), None) if natural.x > 0.0 => vec2(w, w * natural.y / natural.x),
                (None, Some(h)) if natural.y > 0.0 => vec2(h * natural.x / natural.y, h),
                (Some(w), None) => vec2(w, w),
                (None, Some(h)) => vec2(h, h),
                (None, None) => natural,
            };
            if size.x > max_w && size.x > 0.0 {
                size = vec2(max_w, size.y * max_w / size.x);
            }
            size
        };
        let Some(source) = source else {
            self.alt(ui, img);
            return;
        };
        let image = egui::Image::new(source);
        match image.load_for_size(ui.ctx(), vec2(max_w, f32::INFINITY)) {
            Ok(egui::load::TexturePoll::Ready { texture }) => {
                let size = want(img.width, img.height, texture.size);
                let sense = if img.link.is_some() { Sense::click() } else { Sense::hover() };
                let (rect, response) = ui.allocate_exact_size(size, sense);
                if ui.is_rect_visible(rect) {
                    image.paint_at(ui, rect);
                }
                if let Some(link) = &img.link {
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        self.host.open_link(link);
                    }
                }
            }
            Ok(egui::load::TexturePoll::Pending { size }) => {
                let size = want(img.width, img.height, size.unwrap_or(vec2(16.0, 16.0)));
                ui.allocate_exact_size(size, Sense::hover());
                self.repaint = true;
            }
            Err(_) => self.alt(ui, img),
        }
    }

    fn alt(&mut self, ui: &mut Ui, img: &ImageRef) {
        let text = if img.alt.trim().is_empty() { "[image]".to_string() } else { format!("[{}]", img.alt.trim()) };
        let size = self.px(14.0).max(6.0);
        let color = self.default_text().gamma_multiply(0.7);
        let galley = ui.fonts_mut(|f| f.layout_no_wrap(text, FontId::proportional(size), color));
        let sense = if img.link.is_some() { Sense::click() } else { Sense::hover() };
        let mut size = galley.size() + vec2(8.0, 4.0);
        if let (Some(w), Some(h)) = (img.width, img.height) {
            size =
                size.max(vec2(self.px(w), self.px(h)).min(vec2(ui.available_size_before_wrap().x.max(16.0), 4000.0)));
        }
        let (rect, response) = ui.allocate_exact_size(size, sense);
        let painter = ui.painter();
        painter.rect_stroke(rect, 2.0, Stroke::new(1.0, color.gamma_multiply(0.5)), egui::StrokeKind::Inside);
        painter.galley(rect.left_top() + vec2(4.0, 2.0), galley, color);
        if let Some(link) = &img.link
            && response.clicked()
        {
            self.host.open_link(link);
        }
        if !img.src.is_empty() {
            response.on_hover_text(&img.src);
        }
    }

    // -- measuring --------------------------------------------------------------------

    /// (min-content, max-content) border-box width of a block plus its
    /// horizontal margins, in points.
    fn measure(&mut self, ui: &Ui, b: &Block) -> (f32, f32) {
        if let Some(m) = self.cache.measures.get(&b.key) {
            return *m;
        }
        let s = &b.style;
        let m = self.metrics(s, 0.0);
        let margins = self.len(s.margin.left, 0.0).unwrap_or(0.0) + self.len(s.margin.right, 0.0).unwrap_or(0.0);
        let extra = m.horizontal() + margins;
        let result = if let Some(w) = s.width.fixed().and_then(|w| self.border_len(s, Len::Px(w), 0.0, &m)) {
            let w = self.clamp_width(s, 0.0, &m, w) + margins;
            (w, w)
        } else {
            let (mut min, mut max) = match &b.content {
                Content::Inline(items) => self.measure_inlines(ui, items, s),
                Content::Blocks(children) => {
                    children.iter().filter(|c| !c.style.position.out_of_flow()).fold((0.0f32, 0.0f32), |acc, c| {
                        let (a, z) = self.measure(ui, c);
                        (acc.0.max(a), acc.1.max(z))
                    })
                }
                Content::Row { blocks, gap, wrap, .. } => {
                    let gap = self.px(*gap);
                    let mut min = 0.0f32;
                    let mut sum = 0.0f32;
                    for (i, c) in blocks.iter().filter(|c| !c.style.position.out_of_flow()).enumerate() {
                        let (a, z) = self.measure(ui, c);
                        min = if *wrap { min.max(a) } else { min + a };
                        sum += z + if i > 0 { gap } else { 0.0 };
                    }
                    (min, sum)
                }
                Content::Grid(g) => self.measure_grid(ui, g),
                Content::Table(t) => {
                    let (min, max) = self.table_widths(ui, b, t);
                    let caption = t.caption.as_ref().map(|c| self.measure(ui, c)).unwrap_or((0.0, 0.0));
                    (min.max(caption.0), max.max(caption.1))
                }
                Content::Rule => (0.0, 0.0),
            };
            // `min-width`/`max-width` as content-box widths.
            if let Some(mw) = s.max_width.fixed().and_then(|w| self.border_len(s, Len::Px(w), 0.0, &m)) {
                max = max.min(mw - m.horizontal());
            }
            if let Some(mw) = s.min_width.fixed().and_then(|w| self.border_len(s, Len::Px(w), 0.0, &m)) {
                min = min.max(mw - m.horizontal());
                max = max.max(min);
            }
            (min + extra, max.max(min) + extra)
        };
        self.cache.measures.insert(b.key, result);
        result
    }

    fn measure_inlines(&mut self, ui: &Ui, items: &[Inline], style: &BoxStyle) -> (f32, f32) {
        let refs: Vec<&Inline> = items.iter().filter(|i| matches!(i, Inline::Text(_) | Inline::Break)).collect();
        let mut max = 0.0f32;
        if !refs.is_empty() {
            let (mut job, _, _) = self.job(&refs, style);
            job.wrap = TextWrapping::no_max_width();
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            max = galley.size().x;
        }
        let mut boxes = 0.0f32;
        let mut widest = 0.0f32;
        for it in items {
            match it {
                Inline::Image(img) => {
                    let w = img
                        .width
                        .map(|w| self.px(w))
                        .unwrap_or_else(|| self.px(if img.alt.is_empty() { 60.0 } else { 100.0 }));
                    boxes += w;
                    widest = widest.max(w);
                }
                Inline::Block(b) => {
                    let (a, z) = self.measure(ui, b);
                    boxes += z;
                    widest = widest.max(a);
                }
                _ => {}
            }
        }
        max += boxes;
        if !style.wrap {
            return (max, max);
        }
        // Longest word of each run.
        let mut min = widest;
        for it in items {
            if let Inline::Text(t) = it {
                let Some(word) = t.text.split_whitespace().max_by_key(|w| w.chars().count()) else { continue };
                let f = self.text_format(&t.style, t.link.is_some());
                let galley = ui.fonts_mut(|fo| fo.layout_no_wrap(word.to_string(), f.font_id.clone(), f.color));
                min = min.max(galley.size().x + if self.faux_bold(&t.style) { 1.0 } else { 0.0 });
            }
        }
        (min.min(max), max)
    }

    // -- rows (flex / inline-block) ------------------------------------------------------

    fn row(&mut self, ui: &mut Ui, blocks: &'d [Block], spec: RowSpec) {
        let avail = ui.available_width();
        let gap = self.px(spec.gap);
        let row_gap = self.px(spec.row_gap);
        let origin = ui.cursor().min;
        let mut items: Vec<&'d Block> = Vec::with_capacity(blocks.len());
        for b in blocks {
            if b.style.position.out_of_flow() {
                self.register_abs(b, origin);
            } else {
                items.push(b);
            }
        }
        // Outer (margin-box) widths with their lower and upper bounds.
        let n = items.len();
        let mut widths: Vec<f32> = Vec::with_capacity(n);
        let mut floors: Vec<f32> = Vec::with_capacity(n);
        let mut ceils: Vec<f32> = Vec::with_capacity(n);
        let mut margins: Vec<f32> = Vec::with_capacity(n);
        // Flex base sizes (before `min-`/`max-width`): growing starts from them.
        let mut bases: Vec<f32> = Vec::with_capacity(n);
        for b in &items {
            let s = &b.style;
            let (min, max) = self.measure(ui, b);
            let m = self.metrics(s, avail);
            let mg = self.len(s.margin.left, avail).unwrap_or(0.0) + self.len(s.margin.right, avail).unwrap_or(0.0);
            let basis =
                if spec.flex && s.flex_basis != Len::Auto { self.border_len(s, s.flex_basis, avail, &m) } else { None };
            let explicit = self.border_len(s, s.width, avail, &m);
            let size = basis.or(explicit);
            let hi = self.border_len(s, s.max_width, avail, &m).map_or(f32::INFINITY, |v| v + mg);
            let lo = match self.border_len(s, s.min_width, avail, &m) {
                Some(v) => v + mg,
                None if s.overflow != Overflow::Visible => m.horizontal() + mg,
                // The automatic minimum: the content, or a smaller `width`.
                None => min.min(explicit.map_or(f32::INFINITY, |v| v + mg)),
            }
            .min(hi);
            let basis = size.map_or(max, |v| v + mg);
            // Not `clamp`: it panics when a bound is NaN.
            widths.push(basis.max(lo).min(hi).min(avail).max(lo.min(avail)));
            floors.push(lo.min(avail));
            ceils.push(hi);
            margins.push(mg);
            bases.push(basis);
        }
        let mut lines: Vec<(usize, usize)> = Vec::new();
        if spec.wrap {
            let (mut start, mut x) = (0, 0.0f32);
            for (i, w) in widths.iter().enumerate() {
                if i > start && x + w > avail + 0.5 {
                    lines.push((start, i));
                    start = i;
                    x = 0.0;
                }
                x += w + gap;
            }
            if start < n {
                lines.push((start, n));
            }
        } else if n > 0 {
            lines.push((0, n));
        }
        // Grow or shrink each line to the available width.
        for &(a, z) in &lines {
            let total: f32 = widths[a..z].iter().sum::<f32>() + gap * (z - a - 1) as f32;
            let free = avail - total;
            if spec.flex && free > 0.0 {
                let grow: f32 = items[a..z].iter().map(|b| b.style.flex_grow).sum();
                if grow > 0.0 {
                    // Items that do not grow keep their size; the others share
                    // the rest starting from their base sizes.
                    let grows = |i: usize| items[i].style.flex_grow > 0.0;
                    let used: f32 = (a..z).map(|i| if grows(i) { bases[i] } else { widths[i] }).sum();
                    let free = (avail - used - gap * (z - a - 1) as f32).max(0.0);
                    for i in (a..z).filter(|i| grows(*i)) {
                        let w = bases[i] + free * items[i].style.flex_grow / grow;
                        widths[i] = w.min(ceils[i]).max(floors[i]);
                    }
                }
            } else if free < 0.0 && (spec.flex || !spec.wrap) {
                let weights: Vec<f32> = (a..z)
                    .map(|i| {
                        let k = if spec.flex { items[i].style.flex_shrink } else { 1.0 };
                        (widths[i] - floors[i]).max(0.0) * k
                    })
                    .collect();
                let shrinkable: f32 = weights.iter().sum();
                if shrinkable > 0.0 {
                    let k = (-free / shrinkable).min(1.0);
                    for (i, w) in (a..z).zip(weights) {
                        widths[i] -= w * k;
                    }
                }
            }
        }
        let line_cross = if lines.len() == 1 && spec.flex { self.definite_h } else { None };
        let align_of = |b: &Block| if spec.flex { b.style.align_self.unwrap_or(spec.align) } else { spec.align };
        let mut y = 0.0f32;
        let mut right = 0.0f32;
        for (li, &(a, z)) in lines.iter().enumerate() {
            let used: f32 = widths[a..z].iter().sum::<f32>() + gap * (z - a).saturating_sub(1) as f32;
            let free = (avail - used).max(0.0);
            // Auto margins take the free space before `justify-content`.
            let autos: usize = if spec.flex {
                items[a..z]
                    .iter()
                    .map(|b| {
                        usize::from(b.style.margin.left == Len::Auto) + usize::from(b.style.margin.right == Len::Auto)
                    })
                    .sum()
            } else {
                0
            };
            let (mut x, between) = if autos > 0 { (0.0, 0.0) } else { distribute(spec.justify, free, z - a) };
            let share = if autos > 0 { free / autos as f32 } else { 0.0 };
            // Centered and end-aligned items need their heights first.
            let mut naturals = vec![0.0f32; z - a];
            let mut cross = line_cross.unwrap_or(0.0);
            if items[a..z].iter().any(|b| matches!(align_of(b), ItemAlign::Center | ItemAlign::End)) {
                for i in a..z {
                    let b = items[i];
                    let h = self.natural_height(ui, b, (widths[i] - margins[i]).max(0.0))
                        + self.margin_top(b, avail)
                        + self.margin_bottom(b, avail);
                    naturals[i - a] = h;
                    if line_cross.is_none() {
                        cross = cross.max(h);
                    }
                }
            }
            let mut drawn = Vec::with_capacity(z - a);
            for i in a..z {
                let b = items[i];
                if spec.flex && b.style.margin.left == Len::Auto {
                    x += share;
                }
                let top = self.margin_top(b, avail);
                let bottom = self.margin_bottom(b, avail);
                let dy = match align_of(b) {
                    ItemAlign::Center => ((cross - naturals[i - a]) / 2.0).max(0.0),
                    ItemAlign::End => (cross - naturals[i - a]).max(0.0),
                    _ => 0.0,
                };
                let rect = Rect::from_min_size(origin + vec2(x, y + top + dy), vec2(widths[i], f32::INFINITY));
                let mut child = ui.new_child(
                    UiBuilder::new().id_salt(("r", b.key)).max_rect(rect).layout(Layout::top_down(Align::Min)),
                );
                child.spacing_mut().item_spacing = Vec2::ZERO;
                let fit = Fit {
                    bfc: true,
                    width: Some((widths[i] - margins[i]).max(0.0)),
                    defer_abs: true,
                    ..Default::default()
                };
                let out = self.block_box(&mut child, b, fit);
                right = right.max(x + child.min_rect().width());
                x += widths[i] + gap + between;
                if spec.flex && b.style.margin.right == Len::Auto {
                    x += share;
                }
                drawn.push((b, out, top + bottom + dy));
            }
            let mut line_h = line_cross.unwrap_or(0.0);
            for (_, out, extra) in &drawn {
                line_h = line_h.max(out.rect.height() + extra);
            }
            for (b, mut out, extra) in drawn {
                if spec.flex && align_of(b) == ItemAlign::Stretch && b.style.height == Len::Auto {
                    let target = line_h - extra;
                    if target > out.rect.height() + 0.5 {
                        self.restretch(b, &mut out, target);
                    }
                }
                self.finish_box(b, &mut out);
            }
            y += line_h;
            if li + 1 < lines.len() {
                y += row_gap;
            }
        }
        let rect = Rect::from_min_size(origin, vec2(right.max(0.0), y));
        ui.allocate_rect(rect, Sense::hover());
    }

    // -- grids ---------------------------------------------------------------------------

    /// Explicit column and row tracks, and the `auto-fit` range.
    fn grid_tracks(
        &self,
        g: &Grid,
        width: f32,
        height: f32,
    ) -> (Vec<TrackSize>, Option<std::ops::Range<usize>>, Vec<TrackSize>) {
        let scale = self.scale;
        let to_px = move |l: Len, of: f32| len_at(l, of, scale);
        let t = &g.template;
        let (cols, fit) = grid::expand_tracks(&t.columns, width, self.px(g.column_gap), to_px);
        let (rows, _) = grid::expand_tracks(&t.rows, height, self.px(g.row_gap), to_px);
        (cols, fit, rows)
    }

    /// The size of the implicit track `i` (counted from the end of the explicit grid).
    fn implicit(list: &[TrackSize], i: usize) -> TrackSize {
        if list.is_empty() { TrackSize::AUTO } else { list[i % list.len()] }
    }

    fn grid_placement(g: &Grid, items: &[&Block], explicit: (usize, usize)) -> GridCache {
        let placements: Vec<&grid::GridPlacement> = items.iter().map(|i| &i.style.grid_area).collect();
        let t = &g.template;
        let (cells, cols, rows) = grid::place(&placements, explicit.0, explicit.1, &t.areas, t.column_flow);
        GridCache { explicit, cells, cols, rows, ..Default::default() }
    }

    fn measure_grid(&mut self, ui: &Ui, g: &Grid) -> (f32, f32) {
        let items: Vec<&Block> = g.items.iter().filter(|i| !i.style.position.out_of_flow()).collect();
        let (mut cols, _, rows) = self.grid_tracks(g, f32::INFINITY, f32::INFINITY);
        let t = &g.template;
        let explicit = (cols.len().max(t.areas.first().map_or(0, Vec::len)), rows.len().max(t.areas.len()));
        let placed = Self::grid_placement(g, &items, explicit);
        let n_explicit = cols.len();
        while cols.len() < placed.cols {
            cols.push(Self::implicit(&t.auto_columns, cols.len() - n_explicit));
        }
        let contribs: Vec<Contribution> = items
            .iter()
            .zip(&placed.cells)
            .take(MEASURE_ITEMS)
            .map(|(it, c)| {
                let (min, max) = self.measure(ui, it);
                Contribution { span: c.col, min, max }
            })
            .collect();
        let scale = self.scale;
        let to_px = move |l: Len, of: f32| len_at(l, of, scale);
        let gap = self.px(g.column_gap);
        let gaps = gap * cols.len().saturating_sub(1) as f32;
        let max: f32 = grid::size_tracks(&cols, &contribs, f32::INFINITY, gap, &[], false, to_px).iter().sum();
        let min: f32 = grid::size_tracks(&cols, &contribs, 0.0, gap, &[], false, to_px).iter().sum();
        (min + gaps, (max + gaps).max(min + gaps))
    }

    /// (border-box width, horizontal offset) of a grid item in an area `area_w` wide.
    fn grid_item_width(&mut self, ui: &Ui, it: &Block, area_w: f32, justify: ItemAlign) -> (f32, f32) {
        let s = &it.style;
        let m = self.metrics(s, area_w);
        let ml = self.len(s.margin.left, area_w).unwrap_or(0.0);
        let mr = self.len(s.margin.right, area_w).unwrap_or(0.0);
        let room = (area_w - ml - mr).max(0.0);
        let explicit = self.border_width(s, area_w, &m);
        let w = match explicit {
            Some(w) => w,
            None if justify == ItemAlign::Stretch => self.clamp_width(s, area_w, &m, room),
            None => {
                let (min, max) = self.measure(ui, it);
                self.clamp_width(s, area_w, &m, max.min(area_w).max(min) - ml - mr)
            }
        };
        let justify = match (s.margin.left == Len::Auto, s.margin.right == Len::Auto) {
            (true, true) => ItemAlign::Center,
            (true, false) => ItemAlign::End,
            _ => justify,
        };
        let free = (room - w).max(0.0);
        let dx = ml
            + match justify {
                ItemAlign::Center => free / 2.0,
                ItemAlign::End => free,
                _ => 0.0,
            };
        (w, dx)
    }

    fn grid(&mut self, ui: &mut Ui, b: &'d Block, g: &'d Grid) {
        let avail = ui.available_width();
        let origin = ui.cursor().min;
        let cgap = self.px(g.column_gap);
        let rgap = self.px(g.row_gap);
        let mut items: Vec<&'d Block> = Vec::with_capacity(g.items.len());
        for it in &g.items {
            if it.style.position.out_of_flow() {
                self.register_abs(it, origin);
            } else {
                items.push(it);
            }
        }
        if items.is_empty() {
            return;
        }
        let height = self.definite_h.unwrap_or(f32::INFINITY);
        let (mut cols, fit, row_tracks) = self.grid_tracks(g, avail, height);
        let t = &g.template;
        let explicit = (cols.len().max(t.areas.first().map_or(0, Vec::len)), row_tracks.len().max(t.areas.len()));
        let mut cache = match self.cache.grids.remove(&b.key) {
            Some(c) if c.explicit == explicit && c.cells.len() == items.len() => c,
            _ => Self::grid_placement(g, &items, explicit),
        };
        let (ncols, nrows) = (cache.cols, cache.rows);
        let n_explicit = cols.len();
        while cols.len() < ncols {
            cols.push(Self::implicit(&t.auto_columns, cols.len() - n_explicit));
        }
        // Empty `auto-fit` tracks collapse.
        let collapsed: Vec<bool> = (0..ncols)
            .map(|i| {
                fit.as_ref().is_some_and(|r| r.contains(&i)) && !cache.cells.iter().any(|c| c.col.0 <= i && i < c.col.1)
            })
            .collect();
        let content_sized = |ts: &TrackSize| {
            let c = |b: Breadth| {
                matches!(b, Breadth::Auto | Breadth::MinContent | Breadth::MaxContent | Breadth::FitContent(_))
            };
            c(ts.min) || c(ts.max)
        };
        let mut contribs = Vec::new();
        for (it, cell) in items.iter().zip(&cache.cells).take(MEASURE_ITEMS) {
            if (cell.col.0..cell.col.1.min(ncols)).any(|i| content_sized(&cols[i])) {
                let (min, max) = self.measure(ui, it);
                contribs.push(Contribution { span: cell.col, min, max });
            }
        }
        let scale = self.scale;
        let to_px = move |l: Len, of: f32| len_at(l, of, scale);
        let widths =
            grid::size_tracks(&cols, &contribs, avail, cgap, &collapsed, g.justify_content == Justify::Normal, to_px);
        let visible = collapsed.iter().filter(|c| !**c).count();
        let used = widths.iter().sum::<f32>() + cgap * visible.saturating_sub(1) as f32;
        let (start, between) = distribute(g.justify_content, avail - used, visible);
        let mut xs = Vec::with_capacity(ncols);
        let mut x = start;
        let mut first = true;
        for i in 0..ncols {
            if !collapsed[i] {
                if !first {
                    x += cgap + between;
                }
                first = false;
            }
            xs.push(x);
            x += widths[i];
        }
        let grid_right = x;
        let span_w = |c: (usize, usize)| {
            let end = c.1.min(ncols).max(c.0 + 1);
            xs[end - 1] + widths[end - 1] - xs[c.0]
        };
        if cache.widths.len() != widths.len() || cache.widths.iter().zip(&widths).any(|(a, b)| (a - b).abs() > 0.5) {
            cache.heights = vec![0.0; nrows];
            cache.content = vec![0.0; nrows];
            cache.fr_unit = 0.0;
            cache.widths = widths.clone();
        }
        cache.heights.resize(nrows, 0.0);
        cache.content.resize(nrows, 0.0);
        let mut by_row: Vec<Vec<usize>> = vec![Vec::new(); nrows];
        for (k, c) in cache.cells.iter().enumerate() {
            if c.row.0 < nrows {
                by_row[c.row.0].push(k);
            }
        }
        // (base, limit) of a row: fixed sizes, otherwise sized by content.
        let row_limits = |r: usize| {
            let tr =
                if r < row_tracks.len() { row_tracks[r] } else { Self::implicit(&t.auto_rows, r - row_tracks.len()) };
            let fixed = |b: Breadth| match b {
                Breadth::Len(Len::Px(p)) => Some(p * scale),
                Breadth::Len(l) if height.is_finite() => len_at(l, height, scale),
                _ => None,
            };
            let base = fixed(tr.min).unwrap_or(0.0);
            (base, fixed(tr.max).unwrap_or(f32::INFINITY).max(base))
        };
        let row_flex = |r: usize| {
            let tr =
                if r < row_tracks.len() { row_tracks[r] } else { Self::implicit(&t.auto_rows, r - row_tracks.len()) };
            if let Breadth::Fr(f) = tr.max { f } else { 0.0 }
        };
        let fr_unit = cache.fr_unit;
        let estimate = {
            let mut known: Vec<f32> = cache.heights.iter().copied().filter(|h| *h > 0.0).collect();
            known.sort_by(f32::total_cmp);
            known.get(known.len() / 2).copied().unwrap_or(self.px(24.0))
        };
        let align_of = |it: &Block| it.style.align_self.unwrap_or(g.align_items);
        let justify_of = |it: &Block| it.style.justify_self.unwrap_or(g.justify_items);
        let clip = ui.clip_rect();
        let mut y = origin.y;
        let mut row_top = vec![0.0f32; nrows];
        let mut open: Vec<OpenItem<'d>> = Vec::new();
        for r in 0..nrows {
            row_top[r] = y;
            let (base, limit) = row_limits(r);
            let known = cache.heights[r];
            let guess = if known > 0.0 { known } else { estimate.max(base) };
            let single = by_row[r].iter().all(|&k| cache.cells[k].row.1 == r + 1);
            let pinned = by_row[r].iter().any(|&k| items[k].fixed || items[k].style.position == Position::Sticky);
            // Rows far off screen are not drawn: last frame's (or estimated) height.
            if open.is_empty() && single && !pinned && (y > clip.bottom() + 400.0 || y + guess < clip.top() - 400.0) {
                y += guess + rgap;
                continue;
            }
            let mut cross: Option<f32> = None;
            if by_row[r].iter().any(|&k| matches!(align_of(items[k]), ItemAlign::Center | ItemAlign::End)) {
                let mut h = base;
                for &k in &by_row[r] {
                    let (it, cell) = (items[k], cache.cells[k]);
                    if cell.row.1 == r + 1 {
                        let (w, _) = self.grid_item_width(ui, it, span_w(cell.col), justify_of(it));
                        h = h.max(
                            self.natural_height(ui, it, w) + self.margin_top(it, avail) + self.margin_bottom(it, avail),
                        );
                    }
                }
                cross = Some(h.min(limit));
            }
            for &k in &by_row[r] {
                let (it, cell) = (items[k], cache.cells[k]);
                let (w, dx) = self.grid_item_width(ui, it, span_w(cell.col), justify_of(it));
                let mt = self.margin_top(it, avail);
                let mb = self.margin_bottom(it, avail);
                let align = align_of(it);
                let dy = match (align, cross) {
                    (ItemAlign::Center | ItemAlign::End, Some(h)) if cell.row.1 == r + 1 => {
                        let free = (h - self.natural_height(ui, it, w) - mt - mb).max(0.0);
                        if align == ItemAlign::Center { free / 2.0 } else { free }
                    }
                    _ => 0.0,
                };
                let pos = pos2(origin.x + xs[cell.col.0] + dx, y + mt + dy);
                let mut child = ui.new_child(
                    UiBuilder::new()
                        .id_salt(("g", it.key))
                        .max_rect(Rect::from_min_size(pos, vec2(w, f32::INFINITY)))
                        .layout(Layout::top_down(Align::Min)),
                );
                child.spacing_mut().item_spacing = Vec2::ZERO;
                let fit = Fit { bfc: true, width: Some(w), no_margins: true, defer_abs: true, ..Default::default() };
                let out = self.block_box(&mut child, it, fit);
                open.push(OpenItem { block: it, out, row0: r, row1: cell.row.1, extra: mt + mb + dy, align });
            }
            let mut h = base;
            for o in open.iter().filter(|o| o.row1 == r + 1) {
                h = h.max(o.out.rect.height() + o.extra - (y - row_top[o.row0]));
            }
            let h = h.min(limit);
            cache.content[r] = h;
            // Flexible rows share `1fr` (from last frame's content heights).
            let h = h.max(fr_unit * row_flex(r)).min(limit.max(h));
            if (h - known).abs() > 0.5 {
                if known > 0.0 {
                    self.repaint = true;
                }
                cache.heights[r] = h;
            }
            y += h;
            let mut k = 0;
            while k < open.len() {
                if open[k].row1 != r + 1 {
                    k += 1;
                    continue;
                }
                let mut o = open.swap_remove(k);
                if o.align == ItemAlign::Stretch && o.block.style.height == Len::Auto {
                    let target = y - row_top[o.row0] - o.extra;
                    if target > o.out.rect.height() + 0.5 {
                        self.restretch(o.block, &mut o.out, target);
                    }
                }
                self.finish_box(o.block, &mut o.out);
            }
            y += rgap;
        }
        if nrows > 0 {
            y -= rgap;
        }
        // `1fr` for the next frame: with a definite height the space left by
        // the other rows, otherwise the largest content height per `fr`, so
        // `1fr` rows come out equally tall.
        let flex: Vec<f32> = (0..nrows).map(row_flex).collect();
        let total: f32 = flex.iter().sum();
        if total > 0.0 {
            let unit = if height.is_finite() {
                let fixed: f32 = (0..nrows).filter(|&r| flex[r] == 0.0).map(|r| cache.heights[r]).sum();
                let gaps = rgap * nrows.saturating_sub(1) as f32;
                (height - fixed - gaps).max(0.0) / total
            } else {
                (0..nrows).filter(|&r| flex[r] > 0.0).map(|r| cache.content[r] / flex[r]).fold(0.0, f32::max)
            };
            if (unit - cache.fr_unit).abs() > 0.5 {
                cache.fr_unit = unit;
                self.repaint = true;
            }
        }
        self.cache.grids.insert(b.key, cache);
        let rect = Rect::from_min_max(origin, pos2(origin.x + grid_right.max(0.0), y.max(origin.y)));
        ui.allocate_rect(rect, Sense::hover());
    }

    // -- tables -------------------------------------------------------------------------

    fn table_cache(&mut self, ui: &Ui, b: &Block, t: &Table) -> TableCache {
        if let Some(c) = self.cache.tables.remove(&b.key) {
            return c;
        }
        // Measure up to MEASURE_ROWS rows (all head rows) to size the columns.
        const MEASURE_ROWS: usize = 400;
        let n = t.ncols;
        let mut cols = vec![(0.0f32, 0.0f32); n];
        let mut fixed: Vec<Option<Len>> =
            (0..n).map(|i| t.columns.get(i).copied().filter(|l| *l != Len::Auto)).collect();
        let mut spanned: Vec<(&Cell, f32, f32)> = Vec::new();
        for (ri, row) in t.rows.iter().enumerate() {
            if ri >= MEASURE_ROWS && !row.head {
                continue;
            }
            for cell in &row.cells {
                if cell.col >= n {
                    continue;
                }
                let (min, max) = self.measure(ui, &cell.block);
                if cell.colspan == 1 {
                    let c = &mut cols[cell.col];
                    c.0 = c.0.max(min);
                    c.1 = c.1.max(max);
                    if fixed[cell.col].is_none() && cell.block.style.width != Len::Auto {
                        let s = &cell.block.style;
                        fixed[cell.col] = Some(match s.width {
                            Len::Px(p) if s.border_box => Len::Px(p),
                            Len::Px(p) => Len::Px(p + self.metrics(s, 0.0).horizontal() / self.scale),
                            other => other,
                        });
                    }
                } else {
                    spanned.push((cell, min, max));
                }
            }
        }
        // Spanning cells widen their columns when needed.
        let spacing = self.px(t.spacing);
        for (cell, min, max) in spanned {
            let end = (cell.col + cell.colspan).min(n);
            let range = cell.col..end;
            let k = range.len() as f32;
            let inner = spacing * (k - 1.0);
            let (sum_min, sum_max): (f32, f32) =
                cols[range.clone()].iter().fold((0.0, 0.0), |a, c| (a.0 + c.0, a.1 + c.1));
            if min > sum_min + inner {
                let add = (min - sum_min - inner) / k;
                for c in &mut cols[range.clone()] {
                    c.0 += add;
                    c.1 = c.1.max(c.0);
                }
            }
            if max > sum_max + inner {
                let add = (max - sum_max - inner) / k;
                for c in &mut cols[range] {
                    c.1 += add;
                }
            }
        }
        TableCache { cols, fixed, ..Default::default() }
    }

    /// (min, max) width of the table grid (without the table's own box).
    fn table_widths(&mut self, ui: &Ui, b: &Block, t: &Table) -> (f32, f32) {
        let cache = self.table_cache(ui, b, t);
        let spacing = self.px(t.spacing) * (t.ncols as f32 + 1.0);
        let mut min = spacing;
        let mut max = spacing;
        for (i, (a, z)) in cache.cols.iter().enumerate() {
            match cache.fixed[i] {
                Some(Len::Px(p)) => {
                    let w = self.px(p).max(*a);
                    min += w;
                    max += w;
                }
                _ => {
                    min += a;
                    max += z;
                }
            }
        }
        self.cache.tables.insert(b.key, cache);
        (min, max)
    }

    fn column_widths(&self, cache: &TableCache, avail: f32, fill: bool) -> Vec<f32> {
        let n = cache.cols.len();
        let mut widths = vec![0.0f32; n];
        let mut flexible: Vec<usize> = Vec::new();
        let mut used = 0.0;
        for (i, w) in widths.iter_mut().enumerate() {
            let (min, _) = cache.cols[i];
            match cache.fixed[i] {
                Some(Len::Px(p)) => {
                    *w = self.px(p).max(min);
                    used += *w;
                }
                Some(l @ (Len::Percent(_) | Len::Calc(..) | Len::Clamp(_))) => {
                    *w = self.len(l, avail).unwrap_or(0.0).max(min);
                    used += *w;
                }
                _ => flexible.push(i),
            }
        }
        let rest = (avail - used).max(0.0);
        let sum_min: f32 = flexible.iter().map(|i| cache.cols[*i].0).sum();
        let sum_max: f32 = flexible.iter().map(|i| cache.cols[*i].1).sum();
        for &i in &flexible {
            let (min, max) = cache.cols[i];
            widths[i] = if sum_max <= rest {
                if fill && sum_max > 0.0 {
                    max + (rest - sum_max) * max / sum_max
                } else if fill {
                    rest / flexible.len() as f32
                } else {
                    max
                }
            } else if sum_min >= rest {
                min
            } else {
                min + (max - min) * (rest - sum_min) / (sum_max - sum_min).max(1e-3)
            };
        }
        widths
    }

    fn table(&mut self, ui: &mut Ui, b: &'d Block, t: &'d Table) {
        if let Some(caption) = &t.caption {
            self.block_box(ui, caption, Fit::default());
        }
        if t.rows.is_empty() || t.ncols == 0 {
            return;
        }
        let avail = ui.available_width();
        let mut cache = self.table_cache(ui, b, t);
        let spacing = self.px(t.spacing);
        let n = t.ncols;
        let grid_w = (avail - spacing * (n as f32 + 1.0)).max(0.0);
        let fill = b.style.width != Len::Auto;
        let widths = self.column_widths(&cache, grid_w, fill);
        if cache.widths.len() != widths.len() || cache.widths.iter().zip(&widths).any(|(a, b)| (a - b).abs() > 0.5) {
            cache.heights = vec![0.0; t.rows.len()];
            cache.cell_heights = vec![Vec::new(); t.rows.len()];
            cache.widths = widths.clone();
        }
        if cache.heights.len() != t.rows.len() {
            cache.heights.resize(t.rows.len(), 0.0);
            cache.cell_heights.resize(t.rows.len(), Vec::new());
        }
        let mut xs = Vec::with_capacity(n + 1);
        let origin = ui.cursor().min;
        let mut x = origin.x + spacing;
        for w in &widths {
            xs.push(x);
            x += w + spacing;
        }
        xs.push(x);
        let table_right = x;

        let estimate = {
            let known: Vec<f32> = cache.heights.iter().copied().filter(|h| *h > 0.0).collect();
            if known.is_empty() {
                self.px(16.0) * 1.3 + self.px(4.0)
            } else {
                let mut k = known;
                k.sort_by(f32::total_cmp);
                k[k.len() / 2]
            }
        };
        let virtualize = t.rows.len() > self.opts.virtualize_rows;
        let clip = ui.clip_rect();
        let margin = 300.0;
        let mut y = origin.y + spacing;
        // Sticky cells drawn: key and border box in the flow.
        let mut sticky = Vec::new();
        for (ri, row) in t.rows.iter().enumerate() {
            let known = cache.heights[ri];
            let h = if known > 0.0 { known } else { estimate };
            // Rows with sticky (e.g. header) cells or fixed boxes may be visible anyway.
            let pinned = row.cells.iter().any(|c| c.block.fixed || c.block.style.position == Position::Sticky);
            if virtualize && !pinned && (y > clip.bottom() + margin || y + h < clip.top() - margin) {
                y += h + spacing;
                continue;
            }
            let row_h = self.table_row(ui, ri, row, &xs, y, spacing, &mut cache, &mut sticky);
            if (row_h - known).abs() > 0.5 {
                if known > 0.0 || virtualize {
                    self.repaint = true;
                }
                cache.heights[ri] = row_h;
            }
            y += row_h + spacing;
        }
        let rect = Rect::from_min_max(origin, pos2(table_right, y));
        ui.allocate_rect(rect, Sense::hover());
        self.cache.tables.insert(b.key, cache);
        // Sticky cells stay inside the table.
        self.sticky_room(sticky.into_iter().map(|(k, r)| (k, r, [0.0; 4])).collect(), rect);
    }

    #[allow(clippy::too_many_arguments)]
    fn table_row(
        &mut self,
        ui: &mut Ui,
        ri: usize,
        row: &'d crate::document::Row,
        xs: &[f32],
        y: f32,
        spacing: f32,
        cache: &mut TableCache,
        sticky: &mut Vec<(u32, Rect)>,
    ) -> f32 {
        let placeholder = ui.painter().add(Shape::Noop);
        let row_bg = row.background;
        let prev_heights = cache.cell_heights[ri].clone();
        let known_row = cache.heights[ri];
        let mut heights = Vec::with_capacity(row.cells.len());
        let mut row_h = 0.0f32;
        let mut boxes: Vec<CellOut<'d>> = Vec::new();
        for (ci, cell) in row.cells.iter().enumerate() {
            let n = xs.len() - 1;
            if cell.col >= n {
                heights.push(0.0);
                continue;
            }
            let end = (cell.col + cell.colspan).min(n);
            let x0 = xs[cell.col];
            let x1 = xs[end] - spacing;
            let w = (x1 - x0).max(0.0);
            let s = &cell.block.style;
            let m = self.metrics(s, w);
            let content_w = (w - m.horizontal()).max(0.0);
            // Vertical alignment uses last frame's heights.
            let span_h = if cell.rowspan > 1 {
                (ri..(ri + cell.rowspan).min(cache.heights.len())).map(|r| cache.heights[r]).sum::<f32>()
                    + spacing * (cell.rowspan - 1) as f32
            } else {
                known_row
            };
            let prev = prev_heights.get(ci).copied().unwrap_or(0.0);
            let dy = if span_h > 0.0 && prev > 0.0 {
                let free = (span_h - prev - m.vertical()).max(0.0);
                match s.vertical_align {
                    VAlign::Top => 0.0,
                    VAlign::Bottom => free,
                    _ => free / 2.0,
                }
            } else {
                0.0
            };
            // A sticky cell (a header row) moves with its content, above the rows below.
            let pinned = self.measuring == 0 && s.position == Position::Sticky;
            let shift = if pinned {
                self.sticky_offset(&cell.block, Rect::from_min_max(pos2(x0, y), pos2(x1, y)))
            } else {
                Vec2::ZERO
            };
            let content_min = pos2(x0 + m.border[3] + m.padding[3], y + m.border[0] + m.padding[0] + dy) + shift;
            let start = pinned.then(|| next_shape(ui));
            let cell_bg = s.background.or(row_bg);
            if let Some(bg) = cell_bg {
                self.bg.push(over(self.current_bg(), bg));
            }
            let inner_bg = self.current_bg();
            let positioned = self.measuring == 0 && s.position != Position::Static;
            if positioned {
                self.abs_frames.push(Vec::new());
            }
            let mut child = ui.new_child(
                UiBuilder::new()
                    .id_salt(("c", cell.block.key))
                    .max_rect(Rect::from_min_size(content_min, vec2(content_w, f32::INFINITY)))
                    .layout(Layout::top_down(Align::Min)),
            );
            child.spacing_mut().item_spacing = Vec2::ZERO;
            // A table cell is a formatting context of its own.
            self.floats.push(FloatCtx::new());
            self.content(&mut child, &cell.block, content_w, true);
            let floats = self.floats.pop().unwrap_or_else(FloatCtx::new);
            self.contain_floats(&mut child, floats);
            if cell_bg.is_some() {
                self.bg.pop();
            }
            let frame = positioned.then(|| self.abs_frames.pop().unwrap_or_default());
            let ch = child.min_rect().height().max(0.0);
            heights.push(ch);
            if cell.rowspan == 1 {
                row_h = row_h.max(ch + m.vertical());
            }
            let content = start.map(|start| take_shapes(ui, start));
            boxes.push(CellOut {
                rect: Rect::from_min_max(pos2(x0, y), pos2(x1, y)),
                cell,
                frame,
                bg: inner_bg,
                shift,
                content,
            });
        }
        if row_h <= 0.0 {
            row_h = self.px(4.0);
        }
        let mut shapes = Vec::new();
        for CellOut { rect: r, cell, frame, bg, shift, content } in boxes {
            let h = if cell.rowspan > 1 {
                let following: f32 = (ri + 1..(ri + cell.rowspan).min(cache.heights.len()))
                    .map(|i| if cache.heights[i] > 0.0 { cache.heights[i] } else { row_h })
                    .sum();
                row_h + following + spacing * (cell.rowspan - 1) as f32
            } else {
                row_h
            };
            let mut rect = Rect::from_min_size(r.min, vec2(r.width(), h));
            let mut style = cell.block.style.clone();
            if style.background.is_none() {
                style.background = row_bg;
            }
            if let Some(content) = content {
                sticky.push((cell.block.key, rect));
                rect = rect.translate(shift);
                let clip = ui.clip_rect();
                let mut layer: Vec<ClippedShape> =
                    self.box_shapes(rect, &style).into_iter().map(|shape| ClippedShape::new(clip, shape)).collect();
                layer.extend(content);
                if let Some(ctx) = self.contexts.last_mut() {
                    let z = style.z_index.unwrap_or(0);
                    ctx.push(Layer { z, order: cell.block.key, kind: LayerKind::Shapes(layer, Vec::new()) });
                }
            } else {
                shapes.extend(self.box_shapes(rect, &style));
            }
            if self.measuring == 0
                && let Some(rec) = &mut self.record
            {
                rec.insert(cell.block.key, rect);
            }
            if let Some(frame) = frame {
                let cb = self.padding_box(rect, &style);
                self.resolve_abs(frame, cb, ui.clip_rect(), bg);
            }
        }
        ui.painter().set(placeholder, Shape::Vec(shapes));
        if prev_heights != heights {
            // Vertical alignment needs the new heights: draw once more.
            self.repaint = true;
            cache.cell_heights[ri] = heights;
        }
        row_h
    }
}
