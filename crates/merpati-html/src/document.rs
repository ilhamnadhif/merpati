//! The layout tree: what the renderer draws, independent of egui state.

use std::fmt::Write as _;
use std::sync::Arc;

use egui::Color32;

pub use crate::css::Len;
pub use crate::grid::{Breadth, GridLine, GridPlacement, GridTemplate, Repeat, TrackSize, Tracks};
pub use crate::style::{
    BorderSide, BorderStyle, Clear, Float, ItemAlign, Justify, Overflow, Position, Sides, TextAlign, VAlign,
};

/// A rendered visualization, ready to be shown with [`Document::show`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    /// The `<body>` (or `<html>`) box.
    pub root: Block,
    /// `<title>`.
    pub title: Option<String>,
    /// Page background from `html`/`body` styles.
    pub background: Option<Color32>,
    /// The page contains `<script>` elements (never executed here).
    pub uses_scripts: bool,
    /// External scripts and style sheets (`<script src>`, `<link rel=stylesheet>`,
    /// `@import`), e.g. a CDN copy of Chart.js.
    pub external_resources: Vec<String>,
    /// Element names that were present but cannot be rendered natively
    /// (`canvas`, `svg`, `iframe`, form controls, ...).
    pub unsupported: Vec<String>,
    /// The HTML after templating (what a browser would get).
    pub html: String,
    /// The page gives boxes, rows or cells background colors of its own, so
    /// it was designed for a white page (see [`crate::Paper::Auto`]).
    pub sets_backgrounds: bool,
    pub(crate) block_count: u32,
    /// Hash of `html`, keys the renderer's caches.
    pub(crate) hash: u64,
    /// Viewport width (CSS px) the styles were computed for.
    pub(crate) viewport: Option<f32>,
    /// Width ranges of the page's `@media` queries.
    pub(crate) media: Vec<crate::css::WidthQuery>,
}

impl Document {
    /// Whether `@media` width queries give other results at `css_width` than
    /// at the width this document was styled for (see [`Document::restyled`]).
    pub fn restyle_needed(&self, css_width: f32) -> bool {
        self.media.iter().any(|q| q.matches(css_width) != self.viewport.is_some_and(|w| q.matches(w)))
    }

    /// The same page styled for a viewport `css_width` CSS px wide.
    pub fn restyled(&self, css_width: f32) -> Document {
        crate::layout::build(self.html.clone(), Some(css_width))
    }

    /// Whether a browser would show noticeably more than the native renderer:
    /// scripts, external resources or unsupported elements.
    pub fn needs_browser(&self) -> bool {
        self.uses_scripts || !self.external_resources.is_empty() || !self.unsupported.is_empty()
    }

    /// Whether nothing visible was produced (e.g. a script-only page).
    pub fn is_blank(&self) -> bool {
        self.root.is_blank()
    }

    /// All text in document order, blocks separated by newlines, cells by tabs.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.root.collect_text(&mut out);
        out.trim().to_string()
    }

    /// An indented dump of the layout tree, for tests and debugging.
    pub fn outline(&self) -> String {
        let mut out = String::new();
        self.root.outline(0, &mut out);
        out
    }
}

/// A block-level box.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Block {
    /// Element name; empty for anonymous boxes (inline content between blocks).
    pub tag: String,
    /// `class` attribute words.
    pub classes: Vec<String>,
    pub style: BoxStyle,
    /// List item marker (`•`, `3.`).
    pub marker: Option<String>,
    pub content: Content,
    /// Preorder index, unique within the document (used for egui ids).
    pub key: u32,
    /// Floats in this subtree that are not contained by it (they affect
    /// content after this block).
    pub(crate) loose_floats: bool,
    /// A `position: fixed` box is in this subtree: it is laid out even when
    /// the rest is off screen, since the box stays in the viewport.
    pub(crate) fixed: bool,
}

impl Block {
    pub fn children(&self) -> &[Block] {
        match &self.content {
            Content::Blocks(b) | Content::Row { blocks: b, .. } => b,
            Content::Grid(g) => &g.items,
            _ => &[],
        }
    }

    /// In the normal flow: neither floated nor absolutely positioned.
    pub fn in_flow(&self) -> bool {
        self.style.float == Float::None && !self.style.position.out_of_flow()
    }

    /// Establishes a block formatting context of its own (contains its
    /// floats, its margins do not collapse with its children's).
    pub fn is_bfc(&self) -> bool {
        let s = &self.style;
        s.overflow != Overflow::Visible || s.flow_root || s.float != Float::None || s.position.out_of_flow()
    }

    /// Depth-first search for the first block with this class.
    pub fn find_class(&self, class: &str) -> Option<&Block> {
        self.find_by(&|b| b.classes.iter().any(|c| c == class))
    }

    fn find_by(&self, f: &dyn Fn(&Block) -> bool) -> Option<&Block> {
        if f(self) {
            return Some(self);
        }
        let inline = self.inlines().iter().find_map(|i| match i {
            Inline::Block(b) => b.find_by(f),
            _ => None,
        });
        inline.or_else(|| self.children().iter().find_map(|c| c.find_by(f))).or_else(|| {
            let t = self.table()?;
            t.caption
                .as_ref()
                .and_then(|c| c.find_by(f))
                .or_else(|| t.rows.iter().flat_map(|r| &r.cells).find_map(|c| c.block.find_by(f)))
        })
    }

    pub fn inlines(&self) -> &[Inline] {
        match &self.content {
            Content::Inline(i) => i,
            _ => &[],
        }
    }

    pub fn table(&self) -> Option<&Table> {
        match &self.content {
            Content::Table(t) => Some(t),
            _ => None,
        }
    }

    /// Depth-first search for the first block with this tag.
    pub fn find(&self, tag: &str) -> Option<&Block> {
        self.find_by(&|b| b.tag == tag)
    }

    /// Plain text of this block.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out.trim().to_string()
    }

    /// Any block, row or cell background in this subtree.
    pub(crate) fn has_backgrounds(&self) -> bool {
        if self.style.background.is_some() {
            return true;
        }
        match &self.content {
            Content::Blocks(b) | Content::Row { blocks: b, .. } => b.iter().any(Block::has_backgrounds),
            Content::Grid(g) => g.items.iter().any(Block::has_backgrounds),
            Content::Inline(items) => items.iter().any(|i| matches!(i, Inline::Block(b) if b.has_backgrounds())),
            Content::Table(t) => {
                t.caption.as_ref().is_some_and(Block::has_backgrounds)
                    || t.rows
                        .iter()
                        .any(|r| r.background.is_some() || r.cells.iter().any(|c| c.block.has_backgrounds()))
            }
            _ => false,
        }
    }

    pub fn is_blank(&self) -> bool {
        match &self.content {
            Content::Blocks(b) | Content::Row { blocks: b, .. } => {
                b.iter().all(Block::is_blank) && self.style.background.is_none() && !self.style.has_border()
            }
            Content::Grid(g) => {
                g.items.iter().all(Block::is_blank) && self.style.background.is_none() && !self.style.has_border()
            }
            Content::Inline(i) => i.is_empty(),
            Content::Table(_) | Content::Rule => false,
        }
    }

    fn collect_text(&self, out: &mut String) {
        if let Some(m) = &self.marker {
            out.push_str(m);
            out.push(' ');
        }
        match &self.content {
            Content::Blocks(b) | Content::Row { blocks: b, .. } => {
                for c in b {
                    c.collect_text(out);
                }
            }
            Content::Grid(g) => {
                for c in &g.items {
                    c.collect_text(out);
                }
            }
            Content::Inline(items) => {
                for i in items {
                    match i {
                        Inline::Text(t) => out.push_str(&t.text),
                        Inline::Break => out.push('\n'),
                        Inline::Image(img) => out.push_str(&img.alt),
                        Inline::Block(b) => out.push_str(&b.text()),
                    }
                }
                out.push('\n');
            }
            Content::Table(t) => {
                if let Some(c) = &t.caption {
                    c.collect_text(out);
                }
                for r in &t.rows {
                    let cells: Vec<String> = r.cells.iter().map(|c| c.block.text()).collect();
                    out.push_str(&cells.join("\t"));
                    out.push('\n');
                }
            }
            Content::Rule => out.push_str("---\n"),
        }
    }

    fn outline(&self, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        let name = if self.tag.is_empty() { "#anon" } else { &self.tag };
        let _ = write!(out, "{pad}{name}");
        if !self.classes.is_empty() {
            let _ = write!(out, ".{}", self.classes.join("."));
        }
        if let Some(m) = &self.marker {
            let _ = write!(out, " [{m}]");
        }
        if let Some(bg) = self.style.background {
            let _ = write!(out, " bg={}", hex(bg));
        }
        if self.style.text_align != TextAlign::Left {
            let _ = write!(out, " align={:?}", self.style.text_align);
        }
        if self.style.float != Float::None {
            let _ = write!(out, " float={:?}", self.style.float);
        }
        if self.style.position != Position::Static {
            let _ = write!(out, " {:?}", self.style.position);
        }
        match &self.content {
            Content::Blocks(b) => {
                out.push('\n');
                for c in b {
                    c.outline(depth + 1, out);
                }
            }
            Content::Row { blocks, .. } => {
                out.push_str(" (row)\n");
                for c in blocks {
                    c.outline(depth + 1, out);
                }
            }
            Content::Grid(g) => {
                out.push_str(" (grid)\n");
                for c in &g.items {
                    c.outline(depth + 1, out);
                }
            }
            Content::Inline(items) if items.is_empty() => out.push('\n'),
            Content::Inline(items) => {
                out.push_str(": ");
                for i in items {
                    match i {
                        Inline::Text(t) => {
                            let mut flags = String::new();
                            let s = &t.style;
                            for (on, f) in [
                                (s.bold, "b"),
                                (s.italic, "i"),
                                (s.underline, "u"),
                                (s.strike, "s"),
                                (s.monospace, "m"),
                            ] {
                                if on {
                                    flags.push_str(f);
                                }
                            }
                            if let Some(c) = s.color {
                                flags.push_str(&format!(" {}", hex(c)));
                            }
                            if let Some(l) = &t.link {
                                flags.push_str(&format!(" ->{l}"));
                            }
                            if flags.is_empty() {
                                let _ = write!(out, "{:?}", t.text);
                            } else {
                                let _ = write!(out, "{:?}<{}>", t.text, flags.trim());
                            }
                        }
                        Inline::Break => out.push_str("<br>"),
                        Inline::Image(img) => {
                            let _ = write!(out, "<img {}>", img.alt);
                        }
                        Inline::Block(b) => {
                            let _ = write!(out, "<{} {:?}>", if b.tag.is_empty() { "#anon" } else { &b.tag }, b.text());
                        }
                    }
                }
                out.push('\n');
            }
            Content::Table(t) => {
                let _ = writeln!(out, " table {}x{}", t.rows.len(), t.ncols);
                if let Some(c) = &t.caption {
                    c.outline(depth + 1, out);
                }
                for r in &t.rows {
                    let _ = write!(out, "{pad}  tr{}", if r.head { " (head)" } else { "" });
                    if let Some(bg) = r.background {
                        let _ = write!(out, " bg={}", hex(bg));
                    }
                    out.push('\n');
                    for c in &r.cells {
                        let mut tag = if c.header { "th".to_string() } else { "td".to_string() };
                        if c.colspan > 1 {
                            tag.push_str(&format!(" colspan={}", c.colspan));
                        }
                        let _ = writeln!(out, "{pad}    {tag}: {}", c.block.text().replace('\n', " / "));
                    }
                }
            }
            Content::Rule => out.push_str(" (rule)\n"),
        }
    }
}

fn hex(c: Color32) -> String {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    if a == 255 { format!("#{r:02x}{g:02x}{b:02x}") } else { format!("#{r:02x}{g:02x}{b:02x}{a:02x}") }
}

/// Box properties of a block. Lengths are CSS px (scaled when drawn).
#[derive(Clone, Debug, PartialEq)]
pub struct BoxStyle {
    pub margin: Sides<Len>,
    pub padding: Sides<Len>,
    pub border: Sides<BorderSide>,
    pub background: Option<Color32>,
    pub width: Len,
    pub min_width: Len,
    pub max_width: Len,
    pub height: Len,
    pub min_height: Len,
    pub max_height: Len,
    /// `box-sizing: border-box`: `width`/`height` include padding and borders.
    pub border_box: bool,
    pub radius: f32,
    /// `border-radius` as a percentage of the box size.
    pub radius_percent: f32,
    pub text_align: TextAlign,
    pub vertical_align: VAlign,
    /// `white-space` allows wrapping.
    pub wrap: bool,
    /// Line height as a multiple of the font size.
    pub line_height: Option<f32>,
    /// Text color for markers and `currentColor` borders.
    pub color: Option<Color32>,
    /// Font size in CSS px (markers, empty lines).
    pub font_size: f32,
    pub position: Position,
    /// `top`, `right`, `bottom`, `left`.
    pub inset: Sides<Len>,
    pub z_index: Option<i32>,
    pub float: Float,
    pub clear: Clear,
    pub overflow: Overflow,
    /// `display: flow-root`.
    pub flow_root: bool,
    /// A clearfix rule (`::after { clear: both }`) applies: the box grows
    /// to contain the floats before its end.
    pub clearfix: bool,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Len,
    pub align_self: Option<ItemAlign>,
    pub justify_self: Option<ItemAlign>,
    pub grid_area: GridPlacement,
}

impl Default for BoxStyle {
    fn default() -> Self {
        BoxStyle {
            margin: Sides::default(),
            padding: Sides::default(),
            border: Sides::default(),
            background: None,
            width: Len::Auto,
            min_width: Len::Auto,
            max_width: Len::Auto,
            height: Len::Auto,
            min_height: Len::Auto,
            max_height: Len::Auto,
            border_box: false,
            radius: 0.0,
            radius_percent: 0.0,
            text_align: TextAlign::default(),
            vertical_align: VAlign::default(),
            wrap: false,
            line_height: None,
            color: None,
            font_size: 0.0,
            position: Position::Static,
            inset: Sides::all(Len::Auto),
            z_index: None,
            float: Float::None,
            clear: Clear::None,
            overflow: Overflow::Visible,
            flow_root: false,
            clearfix: false,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: Len::Auto,
            align_self: None,
            justify_self: None,
            grid_area: GridPlacement::default(),
        }
    }
}

impl BoxStyle {
    pub fn has_border(&self) -> bool {
        [self.border.top, self.border.right, self.border.bottom, self.border.left].iter().any(BorderSide::visible)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Blocks(Vec<Block>),
    /// An inline formatting context (a paragraph).
    Inline(Vec<Inline>),
    Table(Box<Table>),
    /// `<hr>`.
    Rule,
    /// Children laid out left to right (`display: flex` rows and runs of
    /// `inline-block` boxes).
    Row {
        blocks: Vec<Block>,
        /// Between items (CSS px).
        gap: f32,
        wrap: bool,
        /// Between wrapped lines (CSS px).
        row_gap: f32,
        justify: Justify,
        /// `align-items`.
        align: ItemAlign,
        /// A flex container (items grow and shrink); otherwise a run of
        /// inline-blocks.
        flex: bool,
    },
    /// `display: grid`.
    Grid(Box<Grid>),
}

/// A grid container's items and template.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grid {
    pub items: Vec<Block>,
    pub template: GridTemplate,
    /// CSS px.
    pub column_gap: f32,
    pub row_gap: f32,
    pub justify_items: ItemAlign,
    pub align_items: ItemAlign,
    pub justify_content: Justify,
}

impl Default for Content {
    fn default() -> Self {
        Content::Blocks(Vec::new())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inline {
    Text(TextRun),
    Break,
    Image(ImageRef),
    /// An `inline-block` box with a size or border of its own (a badge, a
    /// legend swatch) inside a line.
    Block(Box<Block>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    pub text: String,
    pub style: TextStyle,
    pub link: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextStyle {
    /// `None`: the theme's (or the background-appropriate) text color.
    pub color: Option<Color32>,
    pub background: Option<Color32>,
    /// CSS px.
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub monospace: bool,
    /// `sub`/`super`.
    pub valign: VAlign,
    /// Line height as a multiple of `size` (`None`: `normal`).
    pub line_height: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageRef {
    pub src: String,
    pub alt: String,
    /// CSS px from attributes or CSS.
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub link: Option<String>,
    /// Decoded bytes of a `data:` URI.
    pub data: Option<Arc<[u8]>>,
    pub mime: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub caption: Option<Block>,
    /// Widths from `<col>`/`<colgroup>`.
    pub columns: Vec<Len>,
    /// `thead` rows first, then the bodies, then `tfoot`.
    pub rows: Vec<Row>,
    pub ncols: usize,
    pub collapse: bool,
    /// `border-spacing` (CSS px; 0 when collapsed).
    pub spacing: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub background: Option<Color32>,
    pub cells: Vec<Cell>,
    /// From `<thead>`.
    pub head: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cell {
    /// `<th>`.
    pub header: bool,
    /// Grid column of the cell's first column (after colspans/rowspans).
    pub col: usize,
    pub colspan: usize,
    pub rowspan: usize,
    /// The cell's box (padding, borders, background) and content.
    pub block: Block,
}
