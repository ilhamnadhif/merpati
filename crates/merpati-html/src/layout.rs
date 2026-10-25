//! DOM + styles -> layout tree ([`Document`]).

use std::sync::Arc;

use base64::Engine as _;
use egui::Color32;

use crate::css::{Len, Stylesheet};
use crate::document::{
    Block, BoxStyle, Cell, Content, Document, Grid, ImageRef, Inline, Row, Table, TextRun, TextStyle,
};
use crate::dom::{Dom, NodeData, NodeId};
use crate::grid::{Breadth, GridTemplate, TrackSize, Tracks};
use crate::style::{
    self, Computed, Display, Float, Inner, ItemAlign, Justify, ListStyle, Position, TextTransform, VAlign,
};

/// Elements a browser renders that we cannot.
const UNSUPPORTED: &[&str] =
    &["canvas", "svg", "iframe", "video", "audio", "object", "embed", "math", "input", "select", "textarea"];

/// Inline formatting inherited from enclosing inline elements.
#[derive(Clone, Default)]
struct InlineCtx {
    link: Option<String>,
    background: Option<Color32>,
    valign: VAlign,
}

/// Inline content being collected, before whitespace processing.
enum Pending {
    Text { text: String, style: TextStyle, link: Option<String>, collapse: bool },
    Break,
    Image(ImageRef),
    Block(Box<Block>),
}

/// A block-level item of a flow plus whether it came from an inline-block
/// or is out of the flow (floated or absolutely positioned).
struct FlowItem {
    block: Block,
    inline_block: bool,
    out_of_flow: bool,
}

#[derive(Default)]
struct Flow {
    items: Vec<FlowItem>,
    inline: Vec<Pending>,
}

struct ListCounter {
    next: i64,
    step: i64,
}

struct Builder<'a> {
    dom: &'a Dom,
    sheet: Stylesheet,
    next_key: u32,
    lists: Vec<ListCounter>,
    unsupported: Vec<String>,
}

/// `width`: viewport width in CSS px for `@media` width queries.
pub(crate) fn build(html: String, width: Option<f32>) -> Document {
    let dom = Dom::parse(&html);
    let mut doc = Document { html: String::new(), viewport: width, ..Default::default() };

    let mut sheet = Stylesheet::default();
    sheet.width = width;
    let mut order = Vec::new();
    preorder(&dom, 0, &mut order);
    for &id in &order {
        match dom.tag(id) {
            Some("style") if !inside(&dom, id, "template") => sheet.add(&dom.text_content(id)),
            Some("script") => {
                doc.uses_scripts = true;
                if let Some(src) = dom.attr(id, "src") {
                    push_unique(&mut doc.external_resources, src.trim());
                }
            }
            Some("link") => {
                let rel = dom.attr(id, "rel").unwrap_or("").to_ascii_lowercase();
                if rel.split_whitespace().any(|r| r == "stylesheet")
                    && let Some(href) = dom.attr(id, "href")
                {
                    push_unique(&mut doc.external_resources, href.trim());
                }
            }
            Some("title") if doc.title.is_none() => {
                let t = collapse_ws(&dom.text_content(id));
                let t = t.trim();
                if !t.is_empty() {
                    doc.title = Some(t.to_string());
                }
            }
            _ => {}
        }
    }
    for i in std::mem::take(&mut sheet.imports) {
        push_unique(&mut doc.external_resources, &i);
    }
    doc.media = std::mem::take(&mut sheet.queries);

    let html_el = dom.find("html");
    let body = dom.find("body");
    let initial = Computed::default();
    let html_style = html_el.map(|h| style::compute(&dom, h, &initial, &sheet)).unwrap_or_default();
    let mut b = Builder { dom: &dom, sheet, next_key: 0, lists: Vec::new(), unsupported: Vec::new() };
    doc.root = match body {
        Some(body) => {
            let body_style = style::compute(&dom, body, &html_style, &b.sheet);
            doc.background = body_style.background.or(html_style.background);
            let mut root = b.element_block(body, &body_style);
            // The page background is painted by the renderer behind everything.
            if html_style.background.is_none() {
                root.style.background = None;
            }
            root
        }
        None => Block::default(),
    };
    doc.sets_backgrounds = doc.background.is_some() || doc.root.has_backgrounds();
    mark_fixed(&mut doc.root);
    doc.unsupported = b.unsupported;
    doc.block_count = b.next_key;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&html, &mut h);
    // Restyled copies of one page must not share the renderer's caches.
    std::hash::Hash::hash(&width.map(f32::to_bits), &mut h);
    doc.hash = std::hash::Hasher::finish(&h);
    doc.html = html;
    doc
}

/// Sets [`Block::fixed`] in the subtree; returns whether it contains a
/// `position: fixed` box.
fn mark_fixed(b: &mut Block) -> bool {
    let mut fixed = b.style.position == Position::Fixed;
    match &mut b.content {
        Content::Blocks(blocks) | Content::Row { blocks, .. } => {
            for c in blocks {
                fixed |= mark_fixed(c);
            }
        }
        Content::Grid(g) => {
            for c in &mut g.items {
                fixed |= mark_fixed(c);
            }
        }
        Content::Inline(items) => {
            for i in items {
                if let Inline::Block(c) = i {
                    fixed |= mark_fixed(c);
                }
            }
        }
        Content::Table(t) => {
            for c in
                t.caption.iter_mut().chain(t.rows.iter_mut().flat_map(|r| r.cells.iter_mut().map(|c| &mut c.block)))
            {
                fixed |= mark_fixed(c);
            }
        }
        Content::Rule => {}
    }
    b.fixed = fixed;
    fixed
}

fn preorder(dom: &Dom, id: NodeId, out: &mut Vec<NodeId>) {
    out.push(id);
    for c in &dom.nodes[id].children {
        preorder(dom, *c, out);
    }
}

fn inside(dom: &Dom, id: NodeId, tag: &str) -> bool {
    std::iter::successors(dom.nodes[id].parent, |p| dom.nodes[*p].parent).any(|p| dom.tag(p) == Some(tag))
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !s.is_empty() && !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if is_html_ws(c) {
            if !space {
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

fn is_html_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}')
}

fn transform(text: &str, t: TextTransform, word_start: &mut bool) -> String {
    match t {
        TextTransform::None => text.to_string(),
        TextTransform::Uppercase => text.to_uppercase(),
        TextTransform::Lowercase => text.to_lowercase(),
        TextTransform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            for c in text.chars() {
                if *word_start && c.is_alphanumeric() {
                    out.extend(c.to_uppercase());
                    *word_start = false;
                } else {
                    out.push(c);
                    if c.is_whitespace() {
                        *word_start = true;
                    } else if c.is_alphanumeric() {
                        *word_start = false;
                    }
                }
            }
            out
        }
    }
}

fn box_style(c: &Computed) -> BoxStyle {
    let mut border = c.border;
    for side in [&mut border.top, &mut border.right, &mut border.bottom, &mut border.left] {
        if side.color.is_none() {
            side.color = c.color;
        }
    }
    BoxStyle {
        margin: c.margin,
        padding: c.padding,
        border,
        background: c.background,
        width: c.width,
        min_width: c.min_width,
        max_width: c.max_width,
        height: c.height,
        min_height: c.min_height,
        max_height: c.max_height,
        border_box: c.border_box,
        radius: c.radius,
        radius_percent: c.radius_percent,
        text_align: c.text_align,
        vertical_align: c.vertical_align,
        wrap: c.white_space.wraps(),
        line_height: c.line_height.factor(c.font_size),
        color: c.color,
        font_size: c.font_size,
        position: c.position,
        inset: c.inset,
        z_index: c.z_index,
        // `position: absolute` wins over `float`.
        float: if c.position.out_of_flow() { Float::None } else { c.float },
        clear: c.clear,
        overflow: c.overflow,
        flow_root: c.inner == Inner::FlowRoot,
        clearfix: c.clearfix,
        flex_grow: c.flex_grow,
        flex_shrink: c.flex_shrink,
        flex_basis: c.flex_basis,
        align_self: c.align_self,
        justify_self: c.justify_self,
        grid_area: c.grid_area.clone(),
    }
}

/// Box style of an anonymous block: only the inherited, paragraph-level parts.
fn anon_style(c: &Computed) -> BoxStyle {
    BoxStyle {
        text_align: c.text_align,
        wrap: c.white_space.wraps(),
        line_height: c.line_height.factor(c.font_size),
        color: c.color,
        font_size: c.font_size,
        ..Default::default()
    }
}

fn text_style(c: &Computed, ictx: &InlineCtx) -> TextStyle {
    TextStyle {
        color: c.color,
        background: ictx.background,
        size: c.font_size,
        bold: c.bold,
        italic: c.italic,
        underline: c.underline,
        strike: c.strike,
        monospace: c.monospace,
        valign: ictx.valign,
        line_height: c.line_height.factor(c.font_size),
    }
}

fn is_block_level(d: Display) -> bool {
    !matches!(d, Display::Inline | Display::InlineBlock | Display::None)
}

fn is_table_part(d: Display) -> bool {
    matches!(
        d,
        Display::TableRow
            | Display::TableRowGroup
            | Display::TableHeaderGroup
            | Display::TableFooterGroup
            | Display::TableCell
            | Display::TableColumn
            | Display::TableColumnGroup
            | Display::TableCaption
    )
}

/// Floats and absolutely positioned boxes are block-level ("blockified").
fn blockify(mut c: Computed) -> Computed {
    c.display = match (c.display, c.inner) {
        (Display::Inline | Display::InlineBlock, Inner::Flex) => Display::Flex,
        (Display::Inline | Display::InlineBlock, Inner::Grid) => Display::Grid,
        (Display::Inline | Display::InlineBlock, _) => Display::Block,
        (d, _) => d,
    };
    c
}

/// Floats in `children` that escape their parent.
fn loose_floats(children: &[Block]) -> bool {
    children.iter().any(|c| (c.style.float != Float::None) || (c.loose_floats && !c.is_bfc()))
}

fn roman(mut n: i64, upper: bool) -> String {
    if n <= 0 || n >= 4000 {
        return n.to_string();
    }
    let table = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut out = String::new();
    for (v, s) in table {
        while n >= v {
            out.push_str(s);
            n -= v;
        }
    }
    if upper { out.to_uppercase() } else { out }
}

fn alpha(n: i64, upper: bool) -> String {
    if n <= 0 {
        return n.to_string();
    }
    let mut n = n;
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
    }
    let s: String = out.into_iter().rev().collect();
    if upper { s.to_uppercase() } else { s }
}

fn marker(style: ListStyle, n: i64) -> Option<String> {
    Some(match style {
        ListStyle::None => return None,
        ListStyle::Disc => "•".into(),
        ListStyle::Circle => "◦".into(),
        ListStyle::Square => "▪".into(),
        ListStyle::Decimal => format!("{n}."),
        ListStyle::DecimalLeadingZero => format!("{n:02}."),
        ListStyle::LowerAlpha => format!("{}.", alpha(n, false)),
        ListStyle::UpperAlpha => format!("{}.", alpha(n, true)),
        ListStyle::LowerRoman => format!("{}.", roman(n, false)),
        ListStyle::UpperRoman => format!("{}.", roman(n, true)),
    })
}

/// Decodes `data:[mime][;base64],payload`.
pub(crate) fn decode_data_uri(src: &str) -> Option<(Option<String>, Vec<u8>)> {
    let rest = src.trim().strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let base64 = meta.split(';').any(|p| p.eq_ignore_ascii_case("base64"));
    let mime = meta.split(';').next().filter(|m| !m.is_empty()).map(|m| m.to_ascii_lowercase());
    let bytes = if base64 {
        let clean: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
        let clean = percent_encoding::percent_decode_str(&clean).decode_utf8_lossy().into_owned();
        base64::engine::general_purpose::STANDARD
            .decode(clean.trim_end_matches('='))
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(clean.trim_end_matches('=')))
            .ok()?
    } else {
        percent_encoding::percent_decode_str(payload).collect()
    };
    Some((mime, bytes))
}

impl Builder<'_> {
    fn key(&mut self) -> u32 {
        let k = self.next_key;
        self.next_key += 1;
        k
    }

    fn note_unsupported(&mut self, tag: &str) {
        push_unique(&mut self.unsupported, tag);
    }

    fn classes(&self, el: NodeId) -> Vec<String> {
        self.dom.attr(el, "class").map(|c| c.split_whitespace().map(str::to_string).collect()).unwrap_or_default()
    }

    /// The box of a block-level element with its content.
    fn element_block(&mut self, el: NodeId, c: &Computed) -> Block {
        let key = self.key();
        let tag = self.dom.tag(el).unwrap_or("").to_string();
        let mut block =
            Block { tag: tag.clone(), classes: self.classes(el), style: box_style(c), key, ..Default::default() };
        if c.display == Display::Table {
            block.content = Content::Table(Box::new(self.table(el, c)));
            return block;
        }
        if tag == "hr" {
            block.content = Content::Rule;
            return block;
        }
        let list = matches!(tag.as_str(), "ul" | "ol" | "menu" | "dir");
        if list {
            let reversed = tag == "ol" && self.dom.attr(el, "reversed").is_some();
            let count = self.dom.element_children(el).filter(|ch| self.dom.tag(*ch) == Some("li")).count() as i64;
            let start = self.dom.attr(el, "start").and_then(|s| s.trim().parse::<i64>().ok());
            let next = start.unwrap_or(if reversed { count } else { 1 });
            self.lists.push(ListCounter { next, step: if reversed { -1 } else { 1 } });
        }
        let items = matches!(c.inner, Inner::Flex | Inner::Grid);
        block.content = self.flow_children(el, c, &InlineCtx::default(), items);
        if list {
            self.lists.pop();
        }
        if let Content::Row { blocks, .. } = &mut block.content
            && (c.inner == Inner::Grid || !c.flex_row)
        {
            let mut items = std::mem::take(blocks);
            let grid = if c.inner == Inner::Grid {
                Grid {
                    items,
                    template: c.grid.clone(),
                    column_gap: c.gap,
                    row_gap: c.row_gap,
                    justify_items: c.justify_items,
                    align_items: c.align_items,
                    justify_content: c.justify_content,
                }
            } else {
                // A column flexbox is laid out as a one-column grid: `gap`
                // between items, `align-items` across.
                for it in &mut items {
                    it.style.justify_self = it.style.align_self.take();
                    it.style.grid_area = Default::default();
                }
                let column = TrackSize { min: Breadth::Auto, max: Breadth::Fr(1.0) };
                Grid {
                    items,
                    template: GridTemplate { columns: vec![Tracks::One(column)], ..Default::default() },
                    column_gap: 0.0,
                    row_gap: c.row_gap,
                    justify_items: c.align_items,
                    align_items: ItemAlign::Start,
                    justify_content: Justify::Normal,
                }
            };
            block.content = Content::Grid(Box::new(grid));
        }
        if let Content::Blocks(children) = &block.content {
            block.loose_floats = loose_floats(children);
        }
        block
    }

    fn flow_children(&mut self, el: NodeId, c: &Computed, ictx: &InlineCtx, flex: bool) -> Content {
        let mut flow = Flow::default();
        let mut word_start = true;
        for ch in self.dom.nodes[el].children.clone() {
            self.flow_node(ch, c, ictx, &mut flow, &mut word_start);
        }
        self.finish_flow(flow, c, flex)
    }

    fn finish_flow(&mut self, mut flow: Flow, c: &Computed, flex: bool) -> Content {
        if flow.items.is_empty() && !flex {
            return Content::Inline(clean_inlines(std::mem::take(&mut flow.inline)));
        }
        self.flush(&mut flow, c);
        let items = flow.items;
        if flex {
            let blocks = items
                .into_iter()
                .map(|i| {
                    // `float` does not apply to flex and grid items.
                    let mut b = i.block;
                    b.style.float = Float::None;
                    b
                })
                .collect();
            return Content::Row {
                blocks,
                gap: c.gap,
                wrap: c.flex_wrap,
                row_gap: c.row_gap,
                justify: c.justify_content,
                align: c.align_items,
                flex: true,
            };
        }
        // Consecutive inline-block boxes are laid out in a row, aligned by `text-align`.
        let justify = match c.text_align {
            style::TextAlign::Center => Justify::Center,
            style::TextAlign::Right => Justify::End,
            _ => Justify::Start,
        };
        let mut out: Vec<Block> = Vec::new();
        let mut run: Vec<Block> = Vec::new();
        let flush_run = |run: &mut Vec<Block>, out: &mut Vec<Block>, b: &mut Self| {
            if run.is_empty() {
                return;
            }
            let key = b.key();
            out.push(Block {
                tag: String::new(),
                style: anon_style(c),
                key,
                content: Content::Row {
                    blocks: std::mem::take(run),
                    gap: 0.0,
                    wrap: true,
                    row_gap: 0.0,
                    justify,
                    align: ItemAlign::Start,
                    flex: false,
                },
                ..Default::default()
            });
        };
        for item in items {
            if item.out_of_flow {
                // Out of flow: does not end a run of inline-blocks.
                out.push(item.block);
            } else if item.inline_block {
                run.push(item.block);
            } else {
                flush_run(&mut run, &mut out, self);
                out.push(item.block);
            }
        }
        flush_run(&mut run, &mut out, self);
        Content::Blocks(out)
    }

    /// Moves collected inline content into an anonymous block.
    fn flush(&mut self, flow: &mut Flow, c: &Computed) {
        if flow.inline.is_empty() {
            return;
        }
        let items = clean_inlines(std::mem::take(&mut flow.inline));
        if items.is_empty() {
            return;
        }
        let key = self.key();
        let block = Block {
            tag: String::new(),
            style: anon_style(c),
            key,
            content: Content::Inline(items),
            ..Default::default()
        };
        flow.items.push(FlowItem { block, inline_block: false, out_of_flow: false });
    }

    fn push_text(&mut self, raw: &str, c: &Computed, ictx: &InlineCtx, flow: &mut Flow, word_start: &mut bool) {
        if c.hidden {
            return;
        }
        let ws = c.white_space;
        let text = if ws.preserves_spaces() {
            raw.replace("\r\n", "\n").replace('\t', "    ")
        } else if ws.preserves_newlines() {
            let lines: Vec<String> = raw.replace("\r\n", "\n").split('\n').map(collapse_ws).collect();
            lines.join("\n")
        } else {
            collapse_ws(raw)
        };
        if text.is_empty() {
            return;
        }
        let text = transform(&text, c.text_transform, word_start);
        let style = text_style(c, ictx);
        let collapse = !ws.preserves_spaces();
        if let Some(Pending::Text { text: prev, style: ps, link, collapse: pc }) = flow.inline.last_mut()
            && *ps == style
            && *link == ictx.link
            && *pc == collapse
        {
            prev.push_str(&text);
            return;
        }
        flow.inline.push(Pending::Text { text, style, link: ictx.link.clone(), collapse });
    }

    fn flow_node(&mut self, id: NodeId, parent: &Computed, ictx: &InlineCtx, flow: &mut Flow, ws: &mut bool) {
        let dom = self.dom;
        match &dom.nodes[id].data {
            NodeData::Text(t) => {
                self.push_text(t, parent, ictx, flow, ws);
                return;
            }
            NodeData::Element { .. } => {}
            _ => return,
        }
        let tag = dom.tag(id).unwrap_or("");
        if UNSUPPORTED.contains(&tag) {
            self.note_unsupported(tag);
        }
        let mut c = style::compute(dom, id, parent, &self.sheet);
        if c.display == Display::None {
            return;
        }
        // Each child element of a flex or grid container is an item of its own.
        if matches!(parent.inner, Inner::Flex | Inner::Grid) && !is_table_part(c.display) {
            c = blockify(c);
        }
        // Floats and absolutely positioned boxes leave the line they are in;
        // inline content around them stays one paragraph.
        let out_of_flow = (c.float != Float::None || c.position.out_of_flow()) && !is_table_part(c.display);
        match tag {
            "br" => {
                flow.inline.push(Pending::Break);
                return;
            }
            "img" => {
                if c.hidden {
                    return;
                }
                let img = self.image(id, &c, ictx);
                if is_block_level(c.display) || out_of_flow {
                    if !out_of_flow {
                        self.flush(flow, parent);
                    }
                    let key = self.key();
                    let block = Block {
                        tag: "img".into(),
                        style: box_style(&c),
                        key,
                        content: Content::Inline(vec![Inline::Image(img)]),
                        ..Default::default()
                    };
                    flow.items.push(FlowItem { block, inline_block: false, out_of_flow });
                } else {
                    flow.inline.push(Pending::Image(img));
                }
                return;
            }
            "canvas" | "svg" | "iframe" | "video" | "audio" | "object" | "embed" | "math" => return,
            "input" => {
                let ty = dom.attr(id, "type").unwrap_or("text").to_ascii_lowercase();
                let text = match ty.as_str() {
                    "checkbox" | "radio" => {
                        if dom.attr(id, "checked").is_some() {
                            "[x]".to_string()
                        } else {
                            "[ ]".to_string()
                        }
                    }
                    "submit" | "button" | "reset" => dom.attr(id, "value").unwrap_or("Submit").to_string(),
                    "password" => "•".repeat(dom.attr(id, "value").map(|v| v.chars().count()).unwrap_or(0)),
                    _ => dom.attr(id, "value").or(dom.attr(id, "placeholder")).unwrap_or("").to_string(),
                };
                let ictx = InlineCtx { background: c.background.or(ictx.background), ..ictx.clone() };
                self.push_text(
                    &format!(" {text} "),
                    &Computed { white_space: style::WhiteSpace::Pre, ..c.clone() },
                    &ictx,
                    flow,
                    ws,
                );
                return;
            }
            "select" => {
                let options = dom.find_all("option");
                let mine: Vec<NodeId> = options.into_iter().filter(|o| is_descendant(dom, *o, id)).collect();
                let chosen = mine.iter().find(|o| dom.attr(**o, "selected").is_some()).or(mine.first());
                let text = chosen.map(|o| collapse_ws(&dom.text_content(*o)).trim().to_string()).unwrap_or_default();
                self.push_text(&format!("{text} ▾"), &c, ictx, flow, ws);
                return;
            }
            _ => {}
        }

        let own_link = (tag == "a")
            .then(|| dom.attr(id, "href").map(str::trim))
            .flatten()
            .filter(|h| !h.to_ascii_lowercase().starts_with("javascript:"));
        if out_of_flow {
            if c.hidden {
                return;
            }
            let c = blockify(c);
            let mut block = self.element_block(id, &c);
            if let Some(link) = own_link.or(ictx.link.as_deref()) {
                set_link(&mut block, link);
            }
            flow.items.push(FlowItem { block, inline_block: false, out_of_flow: true });
            return;
        }
        let display = c.display;
        let inline_block_box =
            display == Display::InlineBlock && (c.inner != Inner::Flow || self.has_block_content(id, &c));
        if display == Display::InlineBlock && !inline_block_box && self.boxed_inline(id, &c) {
            if c.hidden {
                return;
            }
            let mut block = self.element_block(id, &c);
            if let Some(link) = own_link.or(ictx.link.as_deref()) {
                set_link(&mut block, link);
            }
            flow.inline.push(Pending::Block(Box::new(block)));
            return;
        }
        if is_block_level(display) || inline_block_box {
            self.flush(flow, parent);
            if c.hidden {
                return;
            }
            let mut block = self.element_block(id, &c);
            if display == Display::ListItem {
                let n = match self.lists.last_mut() {
                    Some(counter) => {
                        if let Some(v) = dom.attr(id, "value").and_then(|v| v.trim().parse::<i64>().ok()) {
                            counter.next = v;
                        }
                        let n = counter.next;
                        counter.next = counter.next.saturating_add(counter.step);
                        n
                    }
                    None => 1,
                };
                block.marker = marker(c.list_style, n);
            }
            // `<a>` wrapping blocks: the link applies to the text inside.
            if let Some(link) = own_link.or(ictx.link.as_deref()) {
                set_link(&mut block, link);
            }
            flow.items.push(FlowItem { block, inline_block: inline_block_box, out_of_flow: false });
            return;
        }

        // Inline element (or inline-block with inline content).
        let mut inner = ictx.clone();
        if tag == "a"
            && let Some(href) = dom.attr(id, "href")
        {
            let href = href.trim();
            if !href.to_ascii_lowercase().starts_with("javascript:") {
                inner.link = Some(href.to_string());
            }
        }
        if let Some(bg) = c.background {
            inner.background = Some(bg);
        }
        if matches!(c.vertical_align, VAlign::Sub | VAlign::Super) {
            inner.valign = c.vertical_align;
        }
        if tag == "q" {
            self.push_text("“", &c, &inner, flow, ws);
        }
        if display == Display::InlineBlock {
            // Padding approximated by spaces around the text.
            let pad = c.padding.left.resolve(0.0).unwrap_or(0.0) >= 4.0 && c.background.is_some();
            if pad {
                self.push_text("\u{a0}", &c, &inner, flow, ws);
            }
            for ch in dom.nodes[id].children.clone() {
                self.flow_node(ch, &c, &inner, flow, ws);
            }
            if pad {
                self.push_text("\u{a0}", &c, &inner, flow, ws);
            }
        } else {
            for ch in dom.nodes[id].children.clone() {
                self.flow_node(ch, &c, &inner, flow, ws);
            }
        }
        if tag == "q" {
            self.push_text("”", &c, &inner, flow, ws);
        }
    }

    /// An inline-block without block content that still needs a box of its
    /// own: an explicit size, a border, rounded corners, or a background but
    /// no text (a legend swatch).
    fn boxed_inline(&self, id: NodeId, c: &Computed) -> bool {
        let b = &c.border;
        c.width != Len::Auto
            || c.height != Len::Auto
            || [b.top, b.right, b.bottom, b.left].iter().any(|s| s.visible())
            || c.radius > 0.0
            || c.radius_percent > 0.0
            || (c.background.is_some() && self.dom.text_content(id).trim().is_empty())
    }

    /// Whether an element contains block-level children (for inline-block).
    fn has_block_content(&self, id: NodeId, c: &Computed) -> bool {
        self.dom.element_children(id).any(|ch| {
            let cs = style::compute(self.dom, ch, c, &self.sheet);
            is_block_level(cs.display)
        })
    }

    fn image(&mut self, id: NodeId, c: &Computed, ictx: &InlineCtx) -> ImageRef {
        let dom = self.dom;
        let src = dom.attr(id, "src").unwrap_or("").trim().to_string();
        let alt = dom.attr(id, "alt").unwrap_or("").to_string();
        let size = |l: Len| match l {
            Len::Px(p) => Some(p),
            _ => None,
        };
        let (mime, data) = match decode_data_uri(&src) {
            Some((mime, bytes)) => (mime, Some(Arc::<[u8]>::from(bytes))),
            None => (None, None),
        };
        ImageRef { src, alt, width: size(c.width), height: size(c.height), link: ictx.link.clone(), data, mime }
    }

    fn table(&mut self, el: NodeId, c: &Computed) -> Table {
        let dom = self.dom;
        let mut t = Table {
            collapse: c.border_collapse,
            spacing: if c.border_collapse { 0.0 } else { c.border_spacing },
            ..Default::default()
        };
        let mut head: Vec<Row> = Vec::new();
        let mut body: Vec<Row> = Vec::new();
        let mut foot: Vec<Row> = Vec::new();
        // Remaining rowspan per column, for column placement.
        let mut occupied: Vec<usize> = Vec::new();
        let mut rows_in_order: Vec<(usize, Row)> = Vec::new();
        for ch in dom.element_children(el).collect::<Vec<_>>() {
            let cs = style::compute(dom, ch, c, &self.sheet);
            match cs.display {
                Display::None => {}
                Display::TableCaption => {
                    if t.caption.is_none() {
                        t.caption = Some(self.element_block(ch, &cs));
                    }
                }
                Display::TableColumnGroup => {
                    let cols: Vec<NodeId> = dom.element_children(ch).filter(|x| dom.tag(*x) == Some("col")).collect();
                    if cols.is_empty() {
                        let span = span_attr(dom, ch);
                        t.columns.extend(std::iter::repeat_n(cs.width, span));
                    }
                    for col in cols {
                        let ccs = style::compute(dom, col, &cs, &self.sheet);
                        t.columns.extend(std::iter::repeat_n(ccs.width, span_attr(dom, col)));
                    }
                }
                Display::TableColumn => t.columns.extend(std::iter::repeat_n(cs.width, span_attr(dom, ch))),
                Display::TableRowGroup | Display::TableHeaderGroup | Display::TableFooterGroup => {
                    let section = match cs.display {
                        Display::TableHeaderGroup => 0,
                        Display::TableFooterGroup => 2,
                        _ => 1,
                    };
                    for tr in dom.element_children(ch).collect::<Vec<_>>() {
                        let rs = style::compute(dom, tr, &cs, &self.sheet);
                        if rs.display == Display::TableRow {
                            let mut row = self.row(tr, &rs, &mut occupied);
                            row.background = row.background.or(cs.background);
                            row.head = section == 0;
                            rows_in_order.push((section, row));
                        }
                    }
                }
                Display::TableRow => {
                    let row = self.row(ch, &cs, &mut occupied);
                    rows_in_order.push((1, row));
                }
                _ => {}
            }
        }
        for (section, row) in rows_in_order {
            match section {
                0 => head.push(row),
                2 => foot.push(row),
                _ => body.push(row),
            }
        }
        t.rows = head.into_iter().chain(body).chain(foot).collect();
        t.ncols = t
            .rows
            .iter()
            .flat_map(|r| r.cells.iter().map(|c| c.col + c.colspan))
            .max()
            .unwrap_or(0)
            .max(t.columns.len());
        t
    }

    fn row(&mut self, tr: NodeId, rs: &Computed, occupied: &mut Vec<usize>) -> Row {
        let dom = self.dom;
        let mut row = Row { background: rs.background, ..Default::default() };
        let mut col = 0usize;
        let mut taken: Vec<(usize, usize, usize)> = Vec::new(); // (col, colspan, rowspan)
        for cell in dom.element_children(tr).collect::<Vec<_>>() {
            let cs = style::compute(dom, cell, rs, &self.sheet);
            if cs.display != Display::TableCell {
                continue;
            }
            while occupied.get(col).copied().unwrap_or(0) > 0 {
                col += 1;
            }
            let colspan =
                dom.attr(cell, "colspan").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).clamp(1, 1000);
            let rowspan =
                dom.attr(cell, "rowspan").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).clamp(1, 65534);
            let block = self.element_block(cell, &cs);
            row.cells.push(Cell { header: dom.tag(cell) == Some("th"), col, colspan, rowspan, block });
            taken.push((col, colspan, rowspan));
            col += colspan;
        }
        // Age the rowspans of earlier rows, then add this row's.
        for o in occupied.iter_mut() {
            *o = o.saturating_sub(1);
        }
        for (c, span, rows) in taken {
            if rows > 1 {
                if occupied.len() < c + span {
                    occupied.resize(c + span, 0);
                }
                for o in &mut occupied[c..c + span] {
                    *o = (*o).max(rows - 1);
                }
            }
        }
        row
    }
}

fn span_attr(dom: &Dom, el: NodeId) -> usize {
    dom.attr(el, "span").and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1).clamp(1, 1000)
}

fn is_descendant(dom: &Dom, node: NodeId, ancestor: NodeId) -> bool {
    std::iter::successors(dom.nodes[node].parent, |p| dom.nodes[*p].parent).any(|p| p == ancestor)
}

fn set_link(block: &mut Block, link: &str) {
    match &mut block.content {
        Content::Inline(items) => {
            for i in items {
                match i {
                    Inline::Text(t) if t.link.is_none() => t.link = Some(link.to_string()),
                    Inline::Image(img) if img.link.is_none() => img.link = Some(link.to_string()),
                    Inline::Block(b) => set_link(b, link),
                    _ => {}
                }
            }
        }
        Content::Blocks(b) | Content::Row { blocks: b, .. } => {
            for c in b {
                set_link(c, link);
            }
        }
        Content::Grid(g) => {
            for c in &mut g.items {
                set_link(c, link);
            }
        }
        _ => {}
    }
}

/// Collapses whitespace between runs, trims line starts/ends and merges runs.
fn clean_inlines(items: Vec<Pending>) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    // Whether the last emitted character was collapsible whitespace (or a line start).
    let mut at_space = true;
    for it in items {
        match it {
            Pending::Break => {
                trim_trailing(&mut out);
                out.push(Inline::Break);
                at_space = true;
            }
            Pending::Image(img) => {
                out.push(Inline::Image(img));
                at_space = false;
            }
            Pending::Block(b) => {
                out.push(Inline::Block(b));
                at_space = false;
            }
            Pending::Text { text, style, link, collapse } => {
                let text = if collapse && at_space { text.trim_start_matches(' ').to_string() } else { text };
                if text.is_empty() {
                    continue;
                }
                at_space = if collapse { text.ends_with(' ') } else { text.ends_with('\n') };
                if let Some(Inline::Text(prev)) = out.last_mut()
                    && prev.style == style
                    && prev.link == link
                {
                    prev.text.push_str(&text);
                    continue;
                }
                out.push(Inline::Text(TextRun { text, style, link }));
            }
        }
    }
    trim_trailing(&mut out);
    // A paragraph of only whitespace (or nothing) is dropped.
    let visible = out.iter().any(|i| match i {
        Inline::Text(t) => !t.text.trim().is_empty() || t.style.background.is_some(),
        _ => true,
    });
    if visible { out } else { Vec::new() }
}

fn trim_trailing(out: &mut Vec<Inline>) {
    while let Some(Inline::Text(t)) = out.last_mut() {
        if t.text.ends_with('\n') {
            // The newline before `</pre>` does not start a visible line.
            t.text.pop();
        }
        let trimmed = t.text.trim_end_matches(' ').len();
        t.text.truncate(trimmed);
        if t.text.is_empty() {
            out.pop();
        } else {
            break;
        }
    }
}
