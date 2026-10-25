//! Computed styles: user-agent defaults, presentational attributes, author
//! style sheets and inline `style`, with CSS inheritance.

use std::sync::LazyLock;

use egui::Color32;

use crate::css::{self, Decl, Len, Stylesheet, parse_color, parse_length, tokens};
use crate::dom::{Dom, NodeId};
use crate::grid::{self, GridPlacement, GridTemplate};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Display {
    #[default]
    Inline,
    Block,
    InlineBlock,
    ListItem,
    Table,
    TableRow,
    TableRowGroup,
    TableHeaderGroup,
    TableFooterGroup,
    TableCell,
    TableCaption,
    TableColumn,
    TableColumnGroup,
    Flex,
    Grid,
    None,
}

/// How an element lays out its children (the inner display type).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Inner {
    #[default]
    Flow,
    /// `display: flow-root` (contains floats).
    FlowRoot,
    Flex,
    Grid,
}

/// Computed `line-height`. Like in CSS, a number is inherited as a number
/// (a multiple of each descendant's own font size) while a length or
/// percentage is inherited as the absolute length it computed to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum LineHeight {
    #[default]
    Normal,
    Number(f32),
    /// CSS px.
    Px(f32),
}

impl LineHeight {
    /// The line height as a multiple of `font_size` (CSS px); `None` for `normal`.
    pub fn factor(self, font_size: f32) -> Option<f32> {
        let f = match self {
            LineHeight::Normal => return None,
            LineHeight::Number(n) => n,
            LineHeight::Px(_) if font_size <= 0.0 => return None,
            LineHeight::Px(p) => p / font_size,
        };
        Some(f.clamp(0.5, 50.0))
    }
}

/// `position`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Position {
    #[default]
    Static,
    Relative,
    /// Out of flow, placed in the padding box of the nearest positioned ancestor.
    Absolute,
    /// Out of flow, placed in the viewport (it does not scroll).
    Fixed,
    /// In the flow, shifted to stay inside the nearest scrollport while it scrolls.
    Sticky,
}

impl Overflow {
    fn max_by(self, other: Overflow) -> Overflow {
        if self as u8 >= other as u8 { self } else { other }
    }
}

impl Position {
    /// `absolute` or `fixed`: taken out of the normal flow.
    pub fn out_of_flow(self) -> bool {
        matches!(self, Position::Absolute | Position::Fixed)
    }
}

/// `float`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Float {
    #[default]
    None,
    Left,
    Right,
}

/// `clear`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Clear {
    #[default]
    None,
    Left,
    Right,
    Both,
}

/// `overflow` (the larger of `overflow-x` and `overflow-y`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overflow {
    #[default]
    Visible,
    /// `hidden` / `clip`: content is clipped to the padding box.
    Hidden,
    /// `auto` / `scroll`: a scroll area when the box has a definite height.
    Scroll,
}

/// Alignment of a box in its flex line or grid area (`align-items`,
/// `justify-self`, ...). `normal` is `Stretch`, `baseline` is `Start`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ItemAlign {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

/// Distribution of free space along a flex line or between grid tracks
/// (`justify-content`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Justify {
    /// `normal`: like `start` (`auto` grid tracks are stretched).
    #[default]
    Normal,
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
    Justify,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VAlign {
    Top,
    #[default]
    Middle,
    Bottom,
    Baseline,
    Sub,
    Super,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WhiteSpace {
    #[default]
    Normal,
    NoWrap,
    Pre,
    PreWrap,
    PreLine,
}

impl WhiteSpace {
    pub fn preserves_spaces(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap)
    }

    pub fn preserves_newlines(self) -> bool {
        matches!(self, WhiteSpace::Pre | WhiteSpace::PreWrap | WhiteSpace::PreLine)
    }

    pub fn wraps(self) -> bool {
        !matches!(self, WhiteSpace::NoWrap | WhiteSpace::Pre)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ListStyle {
    #[default]
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TextTransform {
    #[default]
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BorderStyle {
    #[default]
    None,
    Solid,
    Dashed,
    Dotted,
    Double,
}

/// One side of a border. `color: None` means the text color.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BorderSide {
    pub width: f32,
    pub style: BorderStyle,
    pub color: Option<Color32>,
}

impl BorderSide {
    pub fn visible(&self) -> bool {
        self.width > 0.0 && self.style != BorderStyle::None
    }

    /// Width that takes up space.
    pub fn used_width(&self) -> f32 {
        if self.visible() { self.width } else { 0.0 }
    }
}

/// top, right, bottom, left.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sides<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

impl<T: Copy> Sides<T> {
    pub fn all(v: T) -> Self {
        Sides { top: v, right: v, bottom: v, left: v }
    }

    fn set(&mut self, i: usize, v: T) {
        match i {
            0 => self.top = v,
            1 => self.right = v,
            2 => self.bottom = v,
            _ => self.left = v,
        }
    }

    /// CSS 1–4 value shorthand.
    fn from_list(v: &[T]) -> Option<Self> {
        Some(match v {
            [a] => Sides::all(*a),
            [a, b] => Sides { top: *a, right: *b, bottom: *a, left: *b },
            [a, b, c] => Sides { top: *a, right: *b, bottom: *c, left: *b },
            [a, b, c, d] => Sides { top: *a, right: *b, bottom: *c, left: *d },
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Computed {
    // -- inherited --
    pub color: Option<Color32>,
    /// CSS px.
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    pub monospace: bool,
    pub text_align: TextAlign,
    pub white_space: WhiteSpace,
    pub list_style: ListStyle,
    pub text_transform: TextTransform,
    pub border_collapse: bool,
    pub border_spacing: f32,
    pub hidden: bool,
    pub line_height: LineHeight,
    // -- propagated to descendants (text-decoration) --
    pub underline: bool,
    pub strike: bool,
    // -- not inherited --
    pub display: Display,
    pub background: Option<Color32>,
    pub margin: Sides<Len>,
    pub padding: Sides<Len>,
    pub border: Sides<BorderSide>,
    pub width: Len,
    pub max_width: Len,
    pub height: Len,
    pub radius: f32,
    /// `border-radius` as a percentage of the box size.
    pub radius_percent: f32,
    pub vertical_align: VAlign,
    pub flex_row: bool,
    pub flex_wrap: bool,
    /// `column-gap` (CSS px).
    pub gap: f32,
    pub row_gap: f32,
    pub inner: Inner,
    pub position: Position,
    /// `top`, `right`, `bottom`, `left`.
    pub inset: Sides<Len>,
    pub z_index: Option<i32>,
    pub float: Float,
    pub clear: Clear,
    pub overflow: Overflow,
    /// A clearfix rule (`::after { clear: both }`) applies.
    pub clearfix: bool,
    /// `box-sizing: border-box`.
    pub border_box: bool,
    pub min_width: Len,
    pub min_height: Len,
    pub max_height: Len,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: Len,
    pub justify_content: Justify,
    pub align_items: ItemAlign,
    pub justify_items: ItemAlign,
    pub align_self: Option<ItemAlign>,
    pub justify_self: Option<ItemAlign>,
    pub grid: GridTemplate,
    pub grid_area: GridPlacement,
}

impl Default for Computed {
    fn default() -> Self {
        Computed {
            color: None,
            font_size: 16.0,
            bold: false,
            italic: false,
            monospace: false,
            text_align: TextAlign::Left,
            white_space: WhiteSpace::Normal,
            list_style: ListStyle::Disc,
            text_transform: TextTransform::None,
            border_collapse: false,
            border_spacing: 2.0,
            hidden: false,
            line_height: LineHeight::Normal,
            underline: false,
            strike: false,
            display: Display::Inline,
            background: None,
            margin: Sides::all(Len::Px(0.0)),
            padding: Sides::all(Len::Px(0.0)),
            border: Sides::all(BorderSide::default()),
            width: Len::Auto,
            max_width: Len::Auto,
            height: Len::Auto,
            radius: 0.0,
            radius_percent: 0.0,
            vertical_align: VAlign::Baseline,
            flex_row: true,
            flex_wrap: false,
            gap: 0.0,
            row_gap: 0.0,
            inner: Inner::Flow,
            position: Position::Static,
            inset: Sides::all(Len::Auto),
            z_index: None,
            float: Float::None,
            clear: Clear::None,
            overflow: Overflow::Visible,
            clearfix: false,
            border_box: false,
            min_width: Len::Auto,
            min_height: Len::Auto,
            max_height: Len::Auto,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: Len::Auto,
            justify_content: Justify::Normal,
            align_items: ItemAlign::Stretch,
            justify_items: ItemAlign::Stretch,
            align_self: None,
            justify_self: None,
            grid: GridTemplate::default(),
            grid_area: GridPlacement::default(),
        }
    }
}

impl Computed {
    /// A child's starting point: inherited properties copied, the rest reset.
    pub fn inherit(&self) -> Computed {
        Computed {
            color: self.color,
            font_size: self.font_size,
            bold: self.bold,
            italic: self.italic,
            monospace: self.monospace,
            text_align: self.text_align,
            white_space: self.white_space,
            list_style: self.list_style,
            text_transform: self.text_transform,
            border_collapse: self.border_collapse,
            border_spacing: self.border_spacing,
            hidden: self.hidden,
            line_height: self.line_height,
            underline: self.underline,
            strike: self.strike,
            ..Computed::default()
        }
    }
}

const UA_CSS: &str = r#"
html, address, blockquote, body, center, dialog, div, figure, figcaption, footer, form, header, hr, legend,
listing, main, p, plaintext, pre, xmp, article, aside, h1, h2, h3, h4, h5, h6, hgroup, nav, section, dl, dt, dd,
ol, ul, menu, dir, details, summary, fieldset, optgroup { display: block }
head, script, style, title, meta, link, base, template, noscript, datalist, param, source, track, area, map,
input[type=hidden], [hidden] { display: none }
li { display: list-item }
table { display: table; border-spacing: 2px; border-collapse: separate; text-indent: 0 }
caption { display: table-caption; text-align: center }
colgroup { display: table-column-group }
col { display: table-column }
thead { display: table-header-group; vertical-align: middle }
tbody { display: table-row-group; vertical-align: middle }
tfoot { display: table-footer-group; vertical-align: middle }
tr { display: table-row; vertical-align: inherit }
td, th { display: table-cell; vertical-align: inherit; padding: 1px }
th { font-weight: bold; text-align: center }
body { margin: 8px }
p, blockquote, figure, dl, ul, ol, menu, dir, pre, xmp, listing, plaintext { margin-top: 1em; margin-bottom: 1em }
blockquote, figure { margin-left: 40px; margin-right: 40px }
dd { margin-left: 40px }
ul, ol, menu, dir { padding-left: 40px }
ul, menu, dir { list-style-type: disc }
ol { list-style-type: decimal }
ul ul, ul ol, ol ul, ol ol, ul menu, menu ul, ol menu { margin-top: 0; margin-bottom: 0 }
ul ul, ol ul, menu ul { list-style-type: circle }
ul ul ul, ul ol ul, ol ul ul, ol ol ul { list-style-type: square }
h1 { font-size: 2em; margin-top: 0.67em; margin-bottom: 0.67em; font-weight: bold }
h2 { font-size: 1.5em; margin-top: 0.83em; margin-bottom: 0.83em; font-weight: bold }
h3 { font-size: 1.17em; margin-top: 1em; margin-bottom: 1em; font-weight: bold }
h4 { margin-top: 1.33em; margin-bottom: 1.33em; font-weight: bold }
h5 { font-size: 0.83em; margin-top: 1.67em; margin-bottom: 1.67em; font-weight: bold }
h6 { font-size: 0.67em; margin-top: 2.33em; margin-bottom: 2.33em; font-weight: bold }
hr { margin: 0.5em 0; border-top: 1px solid gray }
pre, xmp, listing, plaintext { font-family: monospace; white-space: pre }
code, kbd, samp, tt, var { font-family: monospace }
b, strong { font-weight: bold }
i, em, cite, dfn, var, address { font-style: italic }
u, ins { text-decoration: underline }
a[href] { text-decoration: underline; color: -merpati-link }
s, strike, del { text-decoration: line-through }
small { font-size: smaller }
big { font-size: larger }
sub { vertical-align: sub; font-size: smaller }
sup { vertical-align: super; font-size: smaller }
mark { background-color: yellow; color: black }
center { text-align: center }
nobr { white-space: nowrap }
dt, summary { font-weight: bold }
fieldset { margin: 0 2px; padding: 0.35em 0.75em 0.625em; border: 2px solid #c0c0c0 }
button { display: inline-block; padding: 1px 6px; border: 1px solid #767676; background-color: #efefef; color: black }
"#;

static UA: LazyLock<Stylesheet> = LazyLock::new(|| {
    let mut s = Stylesheet::default();
    s.add(UA_CSS);
    s
});

/// Cascades and computes the style of `el`.
pub(crate) fn compute(dom: &Dom, el: NodeId, parent: &Computed, author: &Stylesheet) -> Computed {
    let mut decls: Vec<&Decl> = Vec::new();
    let ua = UA.matching(dom, el);
    decls.extend(ua.iter().flat_map(|r| r.decls.iter()));

    let presentational = presentational_hints(dom, el);
    decls.extend(presentational.iter());

    let rules = author.matching(dom, el);
    let inline = dom.attr(el, "style").map(css::parse_declarations).unwrap_or_default();
    for r in &rules {
        decls.extend(r.decls.iter().filter(|d| !d.important));
    }
    decls.extend(inline.iter().filter(|d| !d.important));
    for r in &rules {
        decls.extend(r.decls.iter().filter(|d| d.important));
    }
    decls.extend(inline.iter().filter(|d| d.important));

    let mut c = parent.inherit();
    // Font size first: `em` units of the other properties depend on it.
    for d in &decls {
        match d.name.as_str() {
            "font-size" => apply_font_size(&mut c, &d.value, parent),
            "font" => {
                for t in tokens(&d.value) {
                    let size = t.split('/').next().unwrap_or(t);
                    if size.starts_with(|ch: char| ch.is_ascii_digit() || ch == '.') || is_size_keyword(size) {
                        apply_font_size(&mut c, size, parent);
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    for d in &decls {
        apply(&mut c, d, parent);
    }
    c.clearfix = author.clearfix(dom, el);
    c
}

fn is_size_keyword(s: &str) -> bool {
    matches!(s, "xx-small" | "x-small" | "small" | "medium" | "large" | "x-large" | "xx-large" | "smaller" | "larger")
}

fn apply_font_size(c: &mut Computed, v: &str, parent: &Computed) {
    let v = v.trim().to_ascii_lowercase();
    let px = match v.as_str() {
        "xx-small" => 9.0,
        "x-small" => 10.0,
        "small" => 13.0,
        "medium" => 16.0,
        "large" => 18.0,
        "x-large" => 24.0,
        "xx-large" => 32.0,
        "xxx-large" => 48.0,
        "smaller" => parent.font_size / 1.2,
        "larger" => parent.font_size * 1.2,
        "inherit" => parent.font_size,
        other => match parse_length(other, parent.font_size) {
            Some(Len::Px(p)) => p,
            Some(Len::Percent(p)) => parent.font_size * p / 100.0,
            _ => return,
        },
    };
    c.font_size = px.clamp(1.0, 400.0);
}

/// HTML attributes that act like author CSS with the lowest priority.
fn presentational_hints(dom: &Dom, el: NodeId) -> Vec<Decl> {
    let mut out = Vec::new();
    let mut push = |name: &str, value: String| {
        out.push(Decl { name: name.into(), value, important: false });
    };
    let tag = dom.tag(el).unwrap_or("");
    let px = |v: &str| {
        let v = v.trim();
        if v.ends_with('%') || v.ends_with("px") { v.to_string() } else { format!("{v}px") }
    };
    if let Some(c) = dom.attr(el, "bgcolor") {
        push("background-color", c.to_string());
    }
    if matches!(tag, "table" | "td" | "th" | "col" | "img" | "hr" | "div" | "colgroup")
        && let Some(w) = dom.attr(el, "width")
    {
        push("width", px(w));
    }
    if matches!(tag, "img" | "td" | "th" | "tr")
        && let Some(h) = dom.attr(el, "height")
    {
        push("height", px(h));
    }
    if let Some(a) = dom.attr(el, "align") {
        let a = a.to_ascii_lowercase();
        if matches!(tag, "table" | "img") {
            if a == "center" {
                push("margin-left", "auto".into());
                push("margin-right", "auto".into());
            } else if a == "left" || a == "right" {
                push("float", a);
            }
        } else if matches!(a.as_str(), "left" | "right" | "center" | "justify") {
            push("text-align", a);
        }
    }
    if let Some(v) = dom.attr(el, "valign") {
        push("vertical-align", v.to_ascii_lowercase());
    }
    if tag == "font" {
        if let Some(c) = dom.attr(el, "color") {
            push("color", c.to_string());
        }
        if let Some(f) = dom.attr(el, "face") {
            push("font-family", f.to_string());
        }
        if let Some(s) = dom.attr(el, "size").and_then(|s| s.trim().parse::<i32>().ok()) {
            let size = ["x-small", "small", "medium", "large", "x-large", "xx-large", "xxx-large"];
            push("font-size", size[(s.clamp(1, 7) - 1) as usize].into());
        }
    }
    if tag == "table" {
        if let Some(b) = dom.attr(el, "border") {
            let w = b.trim().parse::<f32>().unwrap_or(1.0);
            if w > 0.0 {
                push("border", format!("{w}px solid gray"));
            }
        }
        if let Some(s) = dom.attr(el, "cellspacing") {
            push("border-spacing", px(s));
        }
    }
    if matches!(tag, "td" | "th") {
        // Attributes of the enclosing table apply to its cells.
        let table = ancestors(dom, el).find(|a| dom.tag(*a) == Some("table"));
        if let Some(t) = table {
            if let Some(b) = dom.attr(t, "border").and_then(|b| b.trim().parse::<f32>().ok())
                && b > 0.0
            {
                push("border", "1px solid gray".into());
            }
            if let Some(p) = dom.attr(t, "cellpadding") {
                push("padding", px(p));
            }
        }
        if dom.attr(el, "nowrap").is_some() {
            push("white-space", "nowrap".into());
        }
    }
    if tag == "ol"
        && let Some(t) = dom.attr(el, "type")
    {
        let style = match t {
            "a" => "lower-alpha",
            "A" => "upper-alpha",
            "i" => "lower-roman",
            "I" => "upper-roman",
            _ => "decimal",
        };
        push("list-style-type", style.into());
    }
    if tag == "pre" && dom.attr(el, "wrap").is_some() {
        push("white-space", "pre-wrap".into());
    }
    out
}

fn ancestors(dom: &Dom, el: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    std::iter::successors(dom.nodes[el].parent, |p| dom.nodes[*p].parent)
}

fn parse_border_side(v: &str, font_size: f32) -> Option<BorderSide> {
    let mut side = BorderSide { width: 3.0, style: BorderStyle::None, color: None };
    let lower = v.trim().to_ascii_lowercase();
    if lower == "none" || lower == "0" || lower == "hidden" {
        return Some(BorderSide::default());
    }
    for t in tokens(&lower) {
        if let Some(s) = parse_border_style(t) {
            side.style = s;
        } else if let Some(w) = parse_border_width(t, font_size) {
            side.width = w;
        } else if let Some(c) = parse_color(t) {
            side.color = Some(c);
        } else if t != "currentcolor" {
            return None;
        }
    }
    Some(side)
}

fn parse_border_style(t: &str) -> Option<BorderStyle> {
    Some(match t {
        "none" | "hidden" => BorderStyle::None,
        "solid" | "inset" | "outset" | "groove" | "ridge" => BorderStyle::Solid,
        "dashed" => BorderStyle::Dashed,
        "dotted" => BorderStyle::Dotted,
        "double" => BorderStyle::Double,
        _ => return None,
    })
}

fn parse_border_width(t: &str, font_size: f32) -> Option<f32> {
    match t {
        "thin" => Some(1.0),
        "medium" => Some(3.0),
        "thick" => Some(5.0),
        _ => match parse_length(t, font_size)? {
            Len::Px(p) => Some(p.max(0.0)),
            _ => None,
        },
    }
}

fn side_index(name: &str) -> Option<usize> {
    Some(match name {
        "top" => 0,
        "right" => 1,
        "bottom" => 2,
        "left" => 3,
        _ => return None,
    })
}

fn lengths(v: &str, font_size: f32) -> Option<Vec<Len>> {
    tokens(v).into_iter().map(|t| parse_length(t, font_size)).collect()
}

fn apply(c: &mut Computed, d: &Decl, parent: &Computed) {
    let v = d.value.trim();
    let lower = v.to_ascii_lowercase();
    let fs = c.font_size;
    if lower == "inherit" {
        inherit_property(c, &d.name, parent);
        return;
    }
    if lower == "initial" || lower == "unset" {
        if lower == "unset" && is_inherited(&d.name) {
            inherit_property(c, &d.name, parent);
        } else {
            let base = Computed { font_size: c.font_size, ..Computed::default() };
            inherit_property(c, &d.name, &base);
        }
        return;
    }
    match d.name.as_str() {
        "color" => {
            if lower == "-merpati-link" {
                // The theme's link color (decided when drawing).
                c.color = None;
            } else if lower != "currentcolor"
                && let Some(col) = parse_color(&lower)
            {
                c.color = Some(col);
            }
        }
        "background" | "background-color" => {
            if lower == "none" || lower == "transparent" || lower == "initial" {
                c.background = None;
            } else if let Some(col) = css::background_color(&lower) {
                c.background = if col.a() == 0 { None } else { Some(col) };
            }
        }
        "display" => {
            // Two-value syntax: `inline flex`, `block flow-root`, ...
            let words: Vec<&str> = lower.split_whitespace().collect();
            let key = match words.as_slice() {
                ["inline", "flex"] => "inline-flex",
                ["inline", "grid"] => "inline-grid",
                ["inline", "flow-root"] => "inline-block",
                ["inline", "flow"] => "inline",
                ["block", inner] => inner,
                [w, ..] => w,
                [] => "",
            };
            (c.display, c.inner) = match key {
                "none" => (Display::None, Inner::Flow),
                "block" | "contents" | "flow" => (Display::Block, Inner::Flow),
                "flow-root" => (Display::Block, Inner::FlowRoot),
                "flex" => (Display::Flex, Inner::Flex),
                "grid" => (Display::Grid, Inner::Grid),
                "inline" => (Display::Inline, Inner::Flow),
                "inline-block" | "inline-table" => (Display::InlineBlock, Inner::Flow),
                "inline-flex" => (Display::InlineBlock, Inner::Flex),
                "inline-grid" => (Display::InlineBlock, Inner::Grid),
                "list-item" => (Display::ListItem, Inner::Flow),
                "table" => (Display::Table, Inner::Flow),
                "table-row" => (Display::TableRow, Inner::Flow),
                "table-row-group" => (Display::TableRowGroup, Inner::Flow),
                "table-header-group" => (Display::TableHeaderGroup, Inner::Flow),
                "table-footer-group" => (Display::TableFooterGroup, Inner::Flow),
                "table-cell" => (Display::TableCell, Inner::Flow),
                "table-caption" => (Display::TableCaption, Inner::Flow),
                "table-column" => (Display::TableColumn, Inner::Flow),
                "table-column-group" => (Display::TableColumnGroup, Inner::Flow),
                _ => (c.display, c.inner),
            };
        }
        "visibility" => c.hidden = matches!(lower.as_str(), "hidden" | "collapse"),
        "font-weight" => {
            c.bold = match lower.as_str() {
                "bold" | "bolder" => true,
                "normal" | "lighter" => false,
                n => n.parse::<u32>().map(|w| w >= 600).unwrap_or(c.bold),
            }
        }
        "font-style" => c.italic = lower.starts_with("italic") || lower.starts_with("oblique"),
        "font-family" => c.monospace = is_monospace_family(&lower),
        "font" => {
            for t in tokens(&lower) {
                match t {
                    "bold" | "bolder" | "600" | "700" | "800" | "900" => c.bold = true,
                    "normal" | "lighter" => {}
                    "italic" | "oblique" => c.italic = true,
                    _ => {}
                }
            }
            if let Some(slash) = lower.split_whitespace().find(|t| t.contains('/'))
                && let Some(lh) = slash.split('/').nth(1)
            {
                c.line_height = parse_line_height(lh, fs).unwrap_or(c.line_height);
            }
            c.monospace = is_monospace_family(&lower);
        }
        "text-decoration" | "text-decoration-line" => {
            if lower.contains("none") {
                c.underline = false;
                c.strike = false;
            }
            if lower.contains("underline") {
                c.underline = true;
            }
            if lower.contains("line-through") {
                c.strike = true;
            }
        }
        "text-align" => {
            c.text_align = match lower.as_str() {
                "center" | "-webkit-center" | "-moz-center" => TextAlign::Center,
                "right" | "end" | "-webkit-right" => TextAlign::Right,
                "justify" => TextAlign::Justify,
                "left" | "start" | "-webkit-left" => TextAlign::Left,
                _ => c.text_align,
            }
        }
        "text-transform" => {
            c.text_transform = match lower.as_str() {
                "uppercase" => TextTransform::Uppercase,
                "lowercase" => TextTransform::Lowercase,
                "capitalize" => TextTransform::Capitalize,
                _ => TextTransform::None,
            }
        }
        "white-space" | "white-space-collapse" | "text-wrap" | "text-wrap-mode" => {
            c.white_space = match lower.as_str() {
                "nowrap" => WhiteSpace::NoWrap,
                "pre" => WhiteSpace::Pre,
                "pre-wrap" | "break-spaces" | "preserve" => WhiteSpace::PreWrap,
                "pre-line" | "preserve-breaks" => WhiteSpace::PreLine,
                "normal" | "wrap" | "collapse" => WhiteSpace::Normal,
                _ => c.white_space,
            }
        }
        "line-height" => c.line_height = parse_line_height(&lower, fs).unwrap_or(c.line_height),
        "list-style" | "list-style-type" => {
            for t in tokens(&lower) {
                let s = match t {
                    "disc" => ListStyle::Disc,
                    "circle" => ListStyle::Circle,
                    "square" => ListStyle::Square,
                    "decimal" => ListStyle::Decimal,
                    "decimal-leading-zero" => ListStyle::DecimalLeadingZero,
                    "lower-alpha" | "lower-latin" => ListStyle::LowerAlpha,
                    "upper-alpha" | "upper-latin" => ListStyle::UpperAlpha,
                    "lower-roman" => ListStyle::LowerRoman,
                    "upper-roman" => ListStyle::UpperRoman,
                    "none" => ListStyle::None,
                    _ => continue,
                };
                c.list_style = s;
            }
        }
        "border-collapse" => c.border_collapse = lower == "collapse",
        "border-spacing" => {
            if let Some(Len::Px(p)) = tokens(&lower).first().and_then(|t| parse_length(t, fs)) {
                c.border_spacing = p.max(0.0);
            }
        }
        "vertical-align" => {
            c.vertical_align = match lower.as_str() {
                "top" | "text-top" => VAlign::Top,
                "middle" => VAlign::Middle,
                "bottom" | "text-bottom" => VAlign::Bottom,
                "sub" => VAlign::Sub,
                "super" => VAlign::Super,
                _ => VAlign::Baseline,
            }
        }
        "margin" => {
            if let Some(s) = lengths(&lower, fs).and_then(|l| Sides::from_list(&l)) {
                c.margin = s;
            }
        }
        "padding" => {
            if let Some(s) = lengths(&lower, fs).and_then(|l| Sides::from_list(&l)) {
                c.padding = s;
            }
        }
        "margin-block" | "padding-block" | "margin-inline" | "padding-inline" => {
            let Some(l) = lengths(&lower, fs) else { return };
            let (a, b) = (l.first().copied().unwrap_or_default(), l.get(1).or(l.first()).copied().unwrap_or_default());
            let target = if d.name.starts_with("margin") { &mut c.margin } else { &mut c.padding };
            if d.name.ends_with("block") {
                target.top = a;
                target.bottom = b;
            } else {
                target.left = a;
                target.right = b;
            }
        }
        "border" => {
            if let Some(side) = parse_border_side(&lower, fs) {
                c.border = Sides::all(side);
            }
        }
        "border-width" => {
            let w: Option<Vec<f32>> = tokens(&lower).into_iter().map(|t| parse_border_width(t, fs)).collect();
            if let Some(s) = w.and_then(|w| Sides::from_list(&w)) {
                c.border.top.width = s.top;
                c.border.right.width = s.right;
                c.border.bottom.width = s.bottom;
                c.border.left.width = s.left;
            }
        }
        "border-style" => {
            let st: Option<Vec<BorderStyle>> = tokens(&lower).into_iter().map(parse_border_style).collect();
            if let Some(s) = st.and_then(|s| Sides::from_list(&s)) {
                c.border.top.style = s.top;
                c.border.right.style = s.right;
                c.border.bottom.style = s.bottom;
                c.border.left.style = s.left;
            }
        }
        "border-color" => {
            let cols: Vec<Option<Color32>> = tokens(&lower).into_iter().map(parse_color).collect();
            if let Some(s) = Sides::from_list(&cols) {
                c.border.top.color = s.top;
                c.border.right.color = s.right;
                c.border.bottom.color = s.bottom;
                c.border.left.color = s.left;
            }
        }
        "border-radius" => match tokens(&lower).first().and_then(|t| parse_length(t, fs)) {
            Some(Len::Px(p)) => {
                c.radius = p.max(0.0);
                c.radius_percent = 0.0;
            }
            Some(Len::Percent(p)) => {
                c.radius = 0.0;
                c.radius_percent = p.clamp(0.0, 50.0);
            }
            _ => {}
        },
        "width" => {
            if let Some(l) = parse_length(&lower, fs) {
                c.width = l;
            }
        }
        "max-width" => {
            c.max_width = if lower == "none" { Len::Auto } else { parse_length(&lower, fs).unwrap_or(c.max_width) };
        }
        "max-height" => {
            c.max_height = if lower == "none" { Len::Auto } else { parse_length(&lower, fs).unwrap_or(c.max_height) };
        }
        "min-width" | "min-height" => {
            let l = if lower == "auto" || lower.ends_with("content") {
                Some(Len::Auto)
            } else {
                parse_length(&lower, fs).filter(|l| *l != Len::Auto)
            };
            if let Some(l) = l {
                if d.name == "min-width" {
                    c.min_width = l;
                } else {
                    c.min_height = l;
                }
            }
        }
        "height" => {
            if let Some(l) = parse_length(&lower, fs) {
                c.height = l;
            }
        }
        "box-sizing" | "-webkit-box-sizing" | "-moz-box-sizing" => c.border_box = lower == "border-box",
        "position" => {
            c.position = match lower.as_str() {
                "relative" => Position::Relative,
                "absolute" => Position::Absolute,
                "fixed" => Position::Fixed,
                "sticky" | "-webkit-sticky" => Position::Sticky,
                "static" => Position::Static,
                _ => c.position,
            }
        }
        "top" | "right" | "bottom" | "left" => {
            if let Some(l) = parse_length(&lower, fs) {
                c.inset.set(side_index(&d.name).unwrap_or(0), l);
            }
        }
        "inset" => {
            if let Some(s) = lengths(&lower, fs).and_then(|l| Sides::from_list(&l)) {
                c.inset = s;
            }
        }
        "z-index" => {
            if lower == "auto" {
                c.z_index = None;
            } else if let Ok(z) = lower.parse::<i32>() {
                c.z_index = Some(z);
            }
        }
        "float" => {
            c.float = match lower.as_str() {
                "left" | "inline-start" => Float::Left,
                "right" | "inline-end" => Float::Right,
                "none" => Float::None,
                _ => c.float,
            }
        }
        "clear" => {
            c.clear = match lower.as_str() {
                "left" | "inline-start" => Clear::Left,
                "right" | "inline-end" => Clear::Right,
                "both" => Clear::Both,
                "none" => Clear::None,
                _ => c.clear,
            }
        }
        "overflow" | "overflow-x" | "overflow-y" => {
            // The more restrictive of the two axes wins.
            let o = lower
                .split_whitespace()
                .map(|t| match t {
                    "hidden" | "clip" => Overflow::Hidden,
                    "auto" | "scroll" | "overlay" => Overflow::Scroll,
                    _ => Overflow::Visible,
                })
                .max_by_key(|o| *o as u8)
                .unwrap_or_default();
            c.overflow = if d.name == "overflow" { o } else { o.max_by(c.overflow) };
        }
        "flex-direction" => c.flex_row = !lower.starts_with("column"),
        "flex-wrap" => c.flex_wrap = lower == "wrap" || lower == "wrap-reverse",
        "flex-flow" => {
            c.flex_row = !lower.contains("column");
            c.flex_wrap = lower.split_whitespace().any(|t| t == "wrap" || t == "wrap-reverse");
        }
        "flex-grow" => {
            if let Ok(g) = lower.parse::<f32>() {
                c.flex_grow = g.max(0.0);
            }
        }
        "flex-shrink" => {
            if let Ok(g) = lower.parse::<f32>() {
                c.flex_shrink = g.max(0.0);
            }
        }
        "flex-basis" => {
            if lower == "content" {
                c.flex_basis = Len::Auto;
            } else if let Some(l) = parse_length(&lower, fs) {
                c.flex_basis = l;
            }
        }
        "flex" => apply_flex(c, &lower, fs),
        "gap" | "grid-gap" => {
            let t = tokens(&lower);
            let px = |t: Option<&&str>| match t.and_then(|t| parse_length(t, fs)) {
                Some(Len::Px(p)) => Some(p.max(0.0)),
                _ => None,
            };
            if let Some(row) = px(t.first()) {
                c.row_gap = row;
                c.gap = px(t.get(1)).unwrap_or(row);
            }
        }
        "column-gap" | "grid-column-gap" | "row-gap" | "grid-row-gap" => {
            if let Some(Len::Px(p)) = parse_length(&lower, fs) {
                if d.name.contains("row") {
                    c.row_gap = p.max(0.0);
                } else {
                    c.gap = p.max(0.0);
                }
            }
        }
        "justify-content" => c.justify_content = parse_justify(&lower).unwrap_or(c.justify_content),
        "align-items" => c.align_items = parse_item_align(&lower).unwrap_or(c.align_items),
        "justify-items" => c.justify_items = parse_item_align(&lower).unwrap_or(c.justify_items),
        "align-self" => c.align_self = if lower == "auto" { None } else { parse_item_align(&lower).or(c.align_self) },
        "justify-self" => {
            c.justify_self = if lower == "auto" { None } else { parse_item_align(&lower).or(c.justify_self) }
        }
        "place-items" | "place-self" => {
            let t = tokens(&lower);
            let a = t.first().and_then(|v| parse_item_align(v));
            let j = t.get(1).and_then(|v| parse_item_align(v)).or(a);
            if d.name == "place-items" {
                c.align_items = a.unwrap_or(c.align_items);
                c.justify_items = j.unwrap_or(c.justify_items);
            } else {
                c.align_self = a.or(c.align_self);
                c.justify_self = j.or(c.justify_self);
            }
        }
        "place-content" => {
            let t = tokens(&lower);
            if let Some(j) = t.get(1).or(t.first()).and_then(|v| parse_justify(v)) {
                c.justify_content = j;
            }
        }
        "grid-template-columns" | "grid-template-rows" => {
            if let Some(list) = grid::parse_track_list(&lower, fs) {
                if d.name.ends_with("columns") {
                    c.grid.columns = list;
                } else {
                    c.grid.rows = list;
                }
            }
        }
        "grid-template-areas" => {
            if let Some(areas) = grid::parse_areas(v) {
                c.grid.areas = areas;
            }
        }
        "grid-template" | "grid" => apply_grid_template(c, v, fs),
        "grid-auto-columns" | "grid-auto-rows" => {
            if let Some(list) = grid::parse_auto_tracks(&lower, fs) {
                if d.name.ends_with("columns") {
                    c.grid.auto_columns = list;
                } else {
                    c.grid.auto_rows = list;
                }
            }
        }
        "grid-auto-flow" => c.grid.column_flow = lower.split_whitespace().any(|t| t == "column"),
        "grid-area" => c.grid_area = grid::parse_area(v.trim()),
        "grid-row" => (c.grid_area.row_start, c.grid_area.row_end) = grid::parse_line_pair(v.trim()),
        "grid-column" => (c.grid_area.column_start, c.grid_area.column_end) = grid::parse_line_pair(v.trim()),
        "grid-row-start" => c.grid_area.row_start = grid::parse_line(v.trim()),
        "grid-row-end" => c.grid_area.row_end = grid::parse_line(v.trim()),
        "grid-column-start" => c.grid_area.column_start = grid::parse_line(v.trim()),
        "grid-column-end" => c.grid_area.column_end = grid::parse_line(v.trim()),
        name => {
            // margin-top, padding-left, border-bottom, border-left-color, ...
            if let Some(side) = name.strip_prefix("margin-").and_then(side_index) {
                if let Some(l) = parse_length(&lower, fs) {
                    c.margin.set(side, l);
                }
            } else if let Some(side) = name.strip_prefix("padding-").and_then(side_index) {
                if let Some(l) = parse_length(&lower, fs) {
                    c.padding.set(side, l);
                }
            } else if let Some(rest) = name.strip_prefix("border-") {
                let (side, prop) = rest.split_once('-').unwrap_or((rest, ""));
                let Some(i) = side_index(side) else { return };
                let cur = match i {
                    0 => &mut c.border.top,
                    1 => &mut c.border.right,
                    2 => &mut c.border.bottom,
                    _ => &mut c.border.left,
                };
                match prop {
                    "" => {
                        if let Some(s) = parse_border_side(&lower, fs) {
                            *cur = s;
                        }
                    }
                    "width" => {
                        if let Some(w) = parse_border_width(&lower, fs) {
                            cur.width = w;
                        }
                    }
                    "style" => {
                        if let Some(s) = parse_border_style(&lower) {
                            cur.style = s;
                        }
                    }
                    "color" => cur.color = parse_color(&lower),
                    _ => {}
                }
            }
        }
    }
}

/// `em` and percentages are relative to the element's own font size.
fn parse_line_height(v: &str, font_size: f32) -> Option<LineHeight> {
    if v == "normal" {
        return Some(LineHeight::Normal);
    }
    if let Ok(n) = v.parse::<f32>() {
        return (n.is_finite() && n >= 0.0).then_some(LineHeight::Number(n));
    }
    let px = match parse_length(v, font_size)? {
        Len::Px(p) => p,
        Len::Percent(p) => font_size * p / 100.0,
        _ => return None,
    };
    (px.is_finite() && px >= 0.0).then_some(LineHeight::Px(px))
}

fn parse_item_align(v: &str) -> Option<ItemAlign> {
    // `safe center`, `unsafe end`: the keyword after the overflow position.
    let v = v.split_whitespace().last().unwrap_or("");
    Some(match v {
        "normal" | "stretch" => ItemAlign::Stretch,
        "start" | "flex-start" | "self-start" | "left" | "baseline" | "first" => ItemAlign::Start,
        "end" | "flex-end" | "self-end" | "right" => ItemAlign::End,
        "center" => ItemAlign::Center,
        _ => return None,
    })
}

fn parse_justify(v: &str) -> Option<Justify> {
    let v = v.split_whitespace().last().unwrap_or("");
    Some(match v {
        "normal" | "stretch" => Justify::Normal,
        "start" | "flex-start" | "left" => Justify::Start,
        "end" | "flex-end" | "right" => Justify::End,
        "center" => Justify::Center,
        "space-between" => Justify::SpaceBetween,
        "space-around" => Justify::SpaceAround,
        "space-evenly" => Justify::SpaceEvenly,
        _ => return None,
    })
}

/// `flex: none | auto | <grow> [<shrink>] [<basis>]`.
fn apply_flex(c: &mut Computed, v: &str, fs: f32) {
    let (grow, shrink, basis) = match v {
        "none" => (0.0, 0.0, Len::Auto),
        "auto" => (1.0, 1.0, Len::Auto),
        "initial" => (0.0, 1.0, Len::Auto),
        _ => {
            let mut nums = Vec::new();
            let mut basis = None;
            for t in tokens(v) {
                match t.parse::<f32>() {
                    Ok(n) if nums.len() < 2 && basis.is_none() => nums.push(n.max(0.0)),
                    _ => basis = parse_length(t, fs).or(basis),
                }
            }
            if nums.is_empty() && basis.is_none() {
                return;
            }
            // A single number is the grow factor with a zero basis (`flex: 1`).
            let basis = basis.unwrap_or(if nums.is_empty() { Len::Auto } else { Len::Px(0.0) });
            (nums.first().copied().unwrap_or(1.0), nums.get(1).copied().unwrap_or(1.0), basis)
        }
    };
    c.flex_grow = grow;
    c.flex_shrink = shrink;
    c.flex_basis = basis;
}

/// `grid-template: <rows> / <columns>`, optionally with area strings
/// (`"a a" 40px "b c" 1fr / auto 1fr`); `grid` with the same syntax.
fn apply_grid_template(c: &mut Computed, v: &str, fs: f32) {
    let lower = v.to_ascii_lowercase();
    if lower.trim() == "none" {
        c.grid = GridTemplate::default();
        return;
    }
    if lower.contains("auto-flow") {
        return;
    }
    let parts = css::split_top_level(v, '/');
    let (rows, cols) = (parts[0], parts.get(1).copied().unwrap_or(""));
    if rows.contains(['"', '\'']) {
        let Some(areas) = grid::parse_areas(rows) else { return };
        // Row sizes may follow each string.
        let mut sizes = Vec::new();
        let mut rest = rows;
        while let Some(start) = rest.find(['"', '\'']) {
            let q = rest.as_bytes()[start] as char;
            let Some(end) = rest[start + 1..].find(q) else { break };
            rest = &rest[start + 1 + end + 1..];
            let next = rest.find(['"', '\'']).unwrap_or(rest.len());
            let size = rest[..next].trim().to_ascii_lowercase();
            sizes.push(if size.is_empty() { None } else { grid::parse_track_size(&size, fs) });
        }
        c.grid.areas = areas;
        c.grid.rows = sizes.into_iter().map(|s| grid::Tracks::One(s.unwrap_or(grid::TrackSize::AUTO))).collect();
    } else if let Some(list) = grid::parse_track_list(&rows.to_ascii_lowercase(), fs) {
        c.grid.rows = list;
    }
    if let Some(list) = grid::parse_track_list(&cols.to_ascii_lowercase(), fs) {
        c.grid.columns = list;
    }
}

fn is_monospace_family(v: &str) -> bool {
    ["monospace", "courier", "consolas", "menlo", "monaco", "mono", "fixed", "terminal"].iter().any(|m| v.contains(m))
}

fn is_inherited(name: &str) -> bool {
    matches!(
        name,
        "color"
            | "font"
            | "font-size"
            | "font-weight"
            | "font-style"
            | "font-family"
            | "text-align"
            | "white-space"
            | "list-style"
            | "list-style-type"
            | "text-transform"
            | "border-collapse"
            | "border-spacing"
            | "visibility"
            | "line-height"
    )
}

fn inherit_property(c: &mut Computed, name: &str, from: &Computed) {
    match name {
        "color" => c.color = from.color,
        "font-size" => c.font_size = from.font_size,
        "font-weight" => c.bold = from.bold,
        "font-style" => c.italic = from.italic,
        "font-family" => c.monospace = from.monospace,
        "font" => {
            c.font_size = from.font_size;
            c.bold = from.bold;
            c.italic = from.italic;
            c.monospace = from.monospace;
        }
        "text-align" => c.text_align = from.text_align,
        "white-space" => c.white_space = from.white_space,
        "list-style" | "list-style-type" => c.list_style = from.list_style,
        "text-transform" => c.text_transform = from.text_transform,
        "border-collapse" => c.border_collapse = from.border_collapse,
        "border-spacing" => c.border_spacing = from.border_spacing,
        "visibility" => c.hidden = from.hidden,
        "line-height" => c.line_height = from.line_height,
        "background" | "background-color" => c.background = from.background,
        "display" => {
            c.display = from.display;
            c.inner = from.inner;
        }
        "margin" => c.margin = from.margin,
        "padding" => c.padding = from.padding,
        "border" => c.border = from.border,
        "width" => c.width = from.width,
        "max-width" => c.max_width = from.max_width,
        "height" => c.height = from.height,
        "vertical-align" => c.vertical_align = from.vertical_align,
        "position" => c.position = from.position,
        "float" => c.float = from.float,
        "clear" => c.clear = from.clear,
        "z-index" => c.z_index = from.z_index,
        "overflow" | "overflow-x" | "overflow-y" => c.overflow = from.overflow,
        "box-sizing" => c.border_box = from.border_box,
        "min-width" => c.min_width = from.min_width,
        "min-height" => c.min_height = from.min_height,
        "max-height" => c.max_height = from.max_height,
        "text-decoration" | "text-decoration-line" => {
            c.underline = from.underline;
            c.strike = from.strike;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style_of(html: &str, id: &str) -> Computed {
        let dom = Dom::parse(html);
        let mut sheet = Stylesheet::default();
        for s in dom.find_all("style") {
            sheet.add(&dom.text_content(s));
        }
        fn walk(dom: &Dom, n: NodeId, parent: &Computed, sheet: &Stylesheet, id: &str) -> Option<Computed> {
            let c = if dom.is_element(n) { compute(dom, n, parent, sheet) } else { parent.clone() };
            if dom.attr(n, "id") == Some(id) {
                return Some(c);
            }
            for ch in dom.nodes[n].children.clone() {
                if let Some(found) = walk(dom, ch, &c, sheet, id) {
                    return Some(found);
                }
            }
            None
        }
        walk(&dom, 0, &Computed::default(), &sheet, id).expect("element")
    }

    #[test]
    fn cascade_order_and_specificity() {
        let html = "<style>p { color: red } .x { color: green } #y { color: blue } p.x { font-weight: bold }\
                    .imp { color: purple !important }</style>\
                    <p id=a>a</p><p id=b class=x>b</p><p id=y class=x>c</p>\
                    <p id=d class='x imp' style='color: orange'>d</p><p id=e class=x style='color: orange'>e</p>";
        assert_eq!(style_of(html, "a").color, Some(Color32::RED));
        assert_eq!(style_of(html, "b").color, Some(Color32::from_rgb(0, 128, 0)));
        assert!(style_of(html, "b").bold);
        assert_eq!(style_of(html, "y").color, Some(Color32::BLUE));
        assert_eq!(style_of(html, "d").color, Some(Color32::from_rgb(128, 0, 128)));
        assert_eq!(style_of(html, "e").color, Some(Color32::from_rgb(255, 165, 0)));
    }

    #[test]
    fn inheritance_and_em() {
        let html = "<div style='font-size: 20px; color: #123456; padding: 1em; background: #eee'>\
                    <span id=s style='font-size: 1.5em; margin-left: 2em'>x</span></div>";
        let s = style_of(html, "s");
        assert_eq!(s.font_size, 30.0);
        assert_eq!(s.margin.left, Len::Px(60.0));
        assert_eq!(s.color, Some(Color32::from_rgb(0x12, 0x34, 0x56)));
        assert_eq!(s.background, None, "background is not inherited");
        assert_eq!(s.padding.left, Len::Px(0.0));
    }

    #[test]
    fn ua_defaults() {
        let html = "<h1 id=h>t</h1><ul><li id=li>x<ul><li id=inner>y</li></ul></li></ul><table><tr><th id=th>h</th></tr></table>";
        let h = style_of(html, "h");
        assert_eq!(h.font_size, 32.0);
        assert!(h.bold);
        assert_eq!(h.display, Display::Block);
        assert_eq!(h.margin.top, Len::Px(32.0 * 0.67));
        assert_eq!(style_of(html, "li").display, Display::ListItem);
        assert_eq!(style_of(html, "inner").list_style, ListStyle::Circle);
        let th = style_of(html, "th");
        assert!(th.bold);
        assert_eq!(th.text_align, TextAlign::Center);
        assert_eq!(th.vertical_align, VAlign::Middle);
    }

    #[test]
    fn borders_and_shorthands() {
        let html = "<style>td { border: 1px solid #ccc; border-bottom: 2px dashed red; padding: 4px 8px }</style>\
                    <table><tr><td id=c style='border-left-color: blue; margin: 1px 2px 3px'>x</td></tr></table>";
        let c = style_of(html, "c");
        assert_eq!(
            c.border.top,
            BorderSide { width: 1.0, style: BorderStyle::Solid, color: Some(Color32::from_gray(0xcc)) }
        );
        assert_eq!(c.border.bottom.style, BorderStyle::Dashed);
        assert_eq!(c.border.bottom.width, 2.0);
        assert_eq!(c.border.left.color, Some(Color32::BLUE));
        assert_eq!(
            c.padding,
            Sides { top: Len::Px(4.0), right: Len::Px(8.0), bottom: Len::Px(4.0), left: Len::Px(8.0) }
        );
        assert_eq!(c.margin.bottom, Len::Px(3.0));
        assert_eq!(c.margin.left, Len::Px(2.0));
    }

    #[test]
    fn presentational_attributes() {
        let html = "<table border=1 cellpadding=5 cellspacing=0 width='100%'><tr bgcolor=#ff0000>\
                    <td id=c align=right>x</td></tr></table><font id=f color=green size=5>y</font>";
        let c = style_of(html, "c");
        assert_eq!(c.padding.left, Len::Px(5.0));
        assert_eq!(c.border.top.width, 1.0);
        assert_eq!(c.text_align, TextAlign::Right);
        let f = style_of(html, "f");
        assert_eq!(f.font_size, 24.0);
        assert_eq!(f.color, Some(Color32::from_rgb(0, 128, 0)));
    }

    #[test]
    fn line_height_lengths_inherit_as_lengths() {
        let html = "<div style='font-size: 10px; line-height: 2em'><span id=a style='font-size: 20px'>a</span></div>\
                    <div style='font-size: 10px; line-height: 150%'><span id=b style='font-size: 20px'>b</span></div>\
                    <div style='font-size: 10px; line-height: 2'><span id=c style='font-size: 20px'>c</span></div>\
                    <div style='line-height: 3'><span id=d style='line-height: bogus'>d</span></div>";
        // A length (or percentage) computes to px against the element's own font size...
        let a = style_of(html, "a");
        assert_eq!(a.line_height, LineHeight::Px(20.0));
        assert_eq!(a.line_height.factor(a.font_size), Some(1.0));
        assert_eq!(style_of(html, "b").line_height, LineHeight::Px(15.0));
        // ...while a number stays a multiple of each descendant's font size.
        let c = style_of(html, "c");
        assert_eq!(c.line_height.factor(c.font_size), Some(2.0));
        // Invalid values are ignored.
        assert_eq!(style_of(html, "d").line_height, LineHeight::Number(3.0));
    }

    #[test]
    fn display_none_and_keywords() {
        let html = "<style>.hide { display: none } p { color: red } p.i { color: inherit }</style>\
                    <div style='color: blue'><p id=p class='i'>x</p></div><b id=b class=hide>z</b>";
        assert_eq!(style_of(html, "b").display, Display::None);
        assert_eq!(style_of(html, "p").color, Some(Color32::BLUE));
    }
}
