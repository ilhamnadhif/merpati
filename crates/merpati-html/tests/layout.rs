//! Box geometry of grids, floats, positioned boxes and flex rows, from
//! headless egui runs (`Document::layout_boxes`). 16 CSS px = 16 points, so
//! the expected numbers are CSS pixels.

use std::collections::HashMap;

use egui::{Color32, Context, Pos2, RawInput, Rect, Shape, pos2, vec2};
use merpati_html::{Block, Document, Options};

fn opts() -> Options {
    Options { base_size: Some(16.0), ..Default::default() }
}

struct Laid {
    doc: Document,
    boxes: HashMap<u32, Rect>,
}

impl Laid {
    fn new(html: &str, width: f32) -> Laid {
        let doc = Document::from_html(html);
        let boxes = doc.layout_boxes(vec2(width, 600.0), &opts());
        Laid { doc, boxes }
    }

    fn block(&self, class: &str) -> &Block {
        self.doc.root.find_class(class).unwrap_or_else(|| panic!("no .{class} in\n{}", self.doc.outline()))
    }

    /// (x, y, width, height) of the border box of the block with `class`.
    fn rect(&self, class: &str) -> (f32, f32, f32, f32) {
        let key = self.block(class).key;
        let r = self.boxes.get(&key).unwrap_or_else(|| panic!(".{class} was not drawn"));
        let round = |v: f32| (v * 10.0).round() / 10.0;
        (round(r.left()), round(r.top()), round(r.width()), round(r.height()))
    }
}

/// Painted shapes of one frame at `width`, flattened in paint order.
fn painted(html: &str, width: f32) -> Vec<Shape> {
    paint(html, vec2(width, 600.0), None)
}

/// Painted shapes with the document in a scroll area of `size` scrolled down
/// by `offset` (screen coordinates: the viewport starts at y = 0).
fn painted_scrolled(html: &str, size: egui::Vec2, offset: f32) -> Vec<Shape> {
    paint(html, size, Some(offset))
}

fn paint(html: &str, size: egui::Vec2, scroll: Option<f32>) -> Vec<Shape> {
    let doc = Document::from_html(html);
    let ctx = Context::default();
    let mut shapes = Vec::new();
    for _ in 0..4 {
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| match scroll {
                Some(offset) => {
                    egui::ScrollArea::vertical().vertical_scroll_offset(offset).show(ui, |ui| {
                        doc.ui(ui, &mut |_: &str| {}, &opts());
                    });
                }
                None => {
                    doc.ui(ui, &mut |_: &str| {}, &opts());
                }
            });
        });
        out.textures_delta.clear();
        fn flatten(s: &Shape, out: &mut Vec<Shape>) {
            match s {
                Shape::Vec(v) => v.iter().for_each(|x| flatten(x, out)),
                Shape::Noop => {}
                s => out.push(s.clone()),
            }
        }
        shapes.clear();
        for s in &out.shapes {
            flatten(&s.shape, &mut shapes);
        }
    }
    shapes
}

/// The first rectangle filled with `color`.
fn fill_rect(shapes: &[Shape], color: Color32) -> Rect {
    shapes[fill_index(shapes, color)].visual_bounding_rect()
}

/// Index in paint order of the first rectangle filled with `color`.
fn fill_index(shapes: &[Shape], color: Color32) -> usize {
    shapes
        .iter()
        .position(|s| matches!(s, Shape::Rect(r) if r.fill == color))
        .unwrap_or_else(|| panic!("no rectangle filled with {color:?}"))
}

fn text_rect(shapes: &[Shape], needle: &str) -> Rect {
    shapes
        .iter()
        .find_map(|s| match s {
            Shape::Text(t) if t.galley.text().contains(needle) => Some(t.galley.rect.translate(t.pos.to_vec2())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no text {needle:?}"))
}

const CARDS: &str = "<style>body { margin: 0 } \
    .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(200px, 1fr)); gap: 10px } \
    .card { height: 50px; background: #eef }</style>\
    <div class=grid><div class='card c1'>1</div><div class='card c2'>2</div><div class='card c3'>3</div>\
    <div class='card c4'>4</div><div class='card c5'>5</div></div>";

#[test]
fn grid_auto_fill_cards_follow_the_width() {
    // 650 wide: three 210px columns.
    let l = Laid::new(CARDS, 650.0);
    assert_eq!(l.rect("c1"), (0.0, 0.0, 210.0, 50.0));
    assert_eq!(l.rect("c2"), (220.0, 0.0, 210.0, 50.0));
    assert_eq!(l.rect("c3"), (440.0, 0.0, 210.0, 50.0));
    assert_eq!(l.rect("c4"), (0.0, 60.0, 210.0, 50.0));
    assert_eq!(l.rect("grid"), (0.0, 0.0, 650.0, 110.0));
    // 430 wide: two columns, three rows.
    let l = Laid::new(CARDS, 430.0);
    assert_eq!(l.rect("c2"), (220.0, 0.0, 210.0, 50.0));
    assert_eq!(l.rect("c3"), (0.0, 60.0, 210.0, 50.0));
    assert_eq!(l.rect("c5"), (0.0, 120.0, 210.0, 50.0));
    assert_eq!(l.rect("grid").3, 170.0);
}

#[test]
fn grid_auto_fit_stretches_the_few_items() {
    let html = "<style>body { margin: 0 } .g { display: grid; grid-template-columns: repeat(auto-fit, minmax(100px, 1fr)) }</style>\
                <div class=g><div class=a>a</div><div class=b>b</div></div>";
    // Four tracks fit; the two empty ones collapse and the items share the width.
    let l = Laid::new(html, 400.0);
    assert_eq!(l.rect("a").2, 200.0);
    assert_eq!(l.rect("b").0, 200.0);
}

#[test]
fn grid_spans_and_line_numbers() {
    let html = "<style>body { margin: 0 } \
        .g { display: grid; grid-template-columns: 100px 100px 100px; grid-auto-rows: 40px }</style>\
        <div class=g><div class=a style='grid-column: span 2'>a</div><div class=b>b</div>\
        <div class=c style='grid-row: span 2'>c</div><div class=d style='grid-column: 2 / 4'>d</div>\
        <div class=e>e</div><div class=f style='grid-column: 1 / -1'>f</div></div>";
    let l = Laid::new(html, 500.0);
    assert_eq!(l.rect("a"), (0.0, 0.0, 200.0, 40.0));
    assert_eq!(l.rect("b"), (200.0, 0.0, 100.0, 40.0));
    assert_eq!(l.rect("c"), (0.0, 40.0, 100.0, 80.0));
    assert_eq!(l.rect("d"), (100.0, 40.0, 200.0, 40.0));
    assert_eq!(l.rect("e"), (100.0, 80.0, 100.0, 40.0));
    assert_eq!(l.rect("f"), (0.0, 120.0, 300.0, 40.0));
}

#[test]
fn grid_template_areas_and_alignment() {
    let html = "<style>body { margin: 0 } \
        .page { display: grid; grid-template-columns: 120px 1fr; grid-template-rows: auto 100px; \
                grid-template-areas: 'head head' 'side main'; column-gap: 20px; row-gap: 5px } \
        header { grid-area: head; height: 30px } aside { grid-area: side } \
        main { grid-area: main; justify-self: center; align-self: end; width: 50px; height: 20px }</style>\
        <div class=page><main class=m>m</main><aside class=s>s</aside><header class=h>h</header></div>";
    let l = Laid::new(html, 400.0);
    assert_eq!(l.rect("h"), (0.0, 0.0, 400.0, 30.0));
    // `aside` stretches to the 100px row.
    assert_eq!(l.rect("s"), (0.0, 35.0, 120.0, 100.0));
    // `main` is centered in its 260px column and sits at the bottom of the row.
    assert_eq!(l.rect("m"), (140.0 + 105.0, 35.0 + 80.0, 50.0, 20.0));
}

#[test]
fn grid_auto_columns_size_to_content_and_stretch() {
    let html = "<style>body { margin: 0 } .g { display: grid; grid-template-columns: auto 1fr auto }\
                .x { width: 60px } .z { width: 40px }</style>\
                <div class=g><div class=x>x</div><div class=y>y</div><div class=z>z</div></div>";
    let l = Laid::new(html, 300.0);
    assert_eq!(l.rect("x").2, 60.0);
    assert_eq!(l.rect("y"), (60.0, 0.0, 200.0, l.rect("y").3));
    assert_eq!(l.rect("z").0, 260.0);
}

const FLOATS: &str = "<style>body { margin: 0 } p { margin: 0 } \
    .l { float: left; width: 100px; height: 50px; background: #fcc } \
    .r { float: right; width: 80px; height: 30px; background: #cfc } \
    .bfc { overflow: hidden; height: 20px } .c { clear: both; height: 20px }</style>\
    <div class=box><div class=l></div><div class=r></div><div class=bfc></div>\
    <p class=t>text beside the floats</p><div class=c></div></div>";

#[test]
fn floats_left_right_and_clear() {
    let l = Laid::new(FLOATS, 400.0);
    assert_eq!(l.rect("l"), (0.0, 0.0, 100.0, 50.0));
    assert_eq!(l.rect("r"), (320.0, 0.0, 80.0, 30.0));
    // A formatting context sits between the floats.
    assert_eq!(l.rect("bfc"), (100.0, 0.0, 220.0, 20.0));
    // A plain paragraph keeps its full-width box; its text avoids the floats.
    let p = l.rect("t");
    assert_eq!((p.0, p.1, p.2), (0.0, 20.0, 400.0));
    let text = text_rect(&painted(FLOATS, 400.0), "text beside");
    assert!(text.left() >= 100.0 && text.right() <= 320.5, "{text:?}");
    // `clear: both` goes below the taller float.
    assert_eq!(l.rect("c"), (0.0, 50.0, 400.0, 20.0));
    assert_eq!(l.rect("box").3, 70.0);
}

#[test]
fn floats_stack_and_wrap_and_containers_contain_them() {
    let html = "<style>body { margin: 0 } .f { float: left; width: 150px; height: 40px } \
        .tall { height: 60px } .hidden { overflow: hidden } .root { display: flow-root } \
        .cf::after { content: ''; display: table; clear: both }</style>\
        <div class=plain><div class='f f1 tall'></div><div class='f f2'></div><div class='f f3'></div></div>\
        <div class=hidden><div class='f h1'></div></div>\
        <div class=root><div class='f r1'></div></div>\
        <div class=cf><div class='f c1'></div></div>\
        <div class=after style='height: 10px'></div>";
    let l = Laid::new(html, 400.0);
    assert_eq!(l.rect("f1"), (0.0, 0.0, 150.0, 60.0));
    assert_eq!(l.rect("f2"), (150.0, 0.0, 150.0, 40.0));
    // No room for a third: below the shorter float, next to the taller one.
    assert_eq!(l.rect("f3"), (150.0, 40.0, 150.0, 40.0));
    // The plain container does not grow around its floats...
    assert_eq!(l.rect("plain").3, 0.0);
    // ...so the next one starts at the top, below the floats once it is a formatting context.
    let hidden = l.rect("hidden");
    assert_eq!((hidden.1, hidden.3), (80.0, 40.0));
    assert_eq!(l.rect("h1"), (0.0, 80.0, 150.0, 40.0));
    let root = l.rect("root");
    assert_eq!((root.1, root.3), (120.0, 40.0));
    // A clearfix container grows to contain its floats too.
    let cf = l.rect("cf");
    assert_eq!((cf.1, cf.3), (160.0, 40.0));
    assert_eq!(l.rect("after").1, 200.0);
}

/// Rectangles of the text shapes that contain `needle`, in paint order.
fn text_rects(shapes: &[Shape], needle: &str) -> Vec<Rect> {
    shapes
        .iter()
        .filter_map(|s| match s {
            Shape::Text(t) if t.galley.text().contains(needle) => Some(t.galley.rect.translate(t.pos.to_vec2())),
            _ => None,
        })
        .collect()
}

#[test]
fn text_flows_back_under_a_float_that_ends() {
    let html = format!(
        "<style>body {{ margin: 0 }} p {{ margin: 0; line-height: 20px }} \
         .f {{ float: left; width: 100px; height: 50px }} .r {{ float: right; width: 50px; height: 90px }}</style>\
         <div class=f></div><p class=t>{}</p><div class=r></div><p class=u>{}</p>",
        "word ".repeat(80),
        "more ".repeat(60)
    );
    let shapes = painted(&html, 400.0);
    let lines = text_rects(&shapes, "word");
    // Three 20px lines overlap the 50px float (the third ends at 60): they
    // start beside it; the lines below use the full width again.
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].left() >= 100.0 && lines[0].top() == 0.0 && lines[0].bottom() == 60.0, "{lines:?}");
    assert!(lines[1].left() < 1.0 && lines[1].top() == 60.0 && lines[1].right() > 300.0, "{lines:?}");
    // The paragraph box is as tall as all its lines.
    let l = Laid::new(&html, 400.0);
    let t = l.rect("t");
    assert_eq!((t.0, t.1, t.2, t.3), (0.0, 0.0, 400.0, lines[1].bottom()));
    // The next float only narrows the lines of the next paragraph beside it.
    assert_eq!(l.rect("r"), (350.0, t.3, 50.0, 90.0));
    let more = text_rects(&shapes, "more");
    assert_eq!(more.len(), 2, "{more:?}");
    assert!(more[0].right() <= 350.5 && more[0].top() == t.3 && more[0].bottom() == t.3 + 100.0, "{more:?}");
    assert!(more[1].right() > 360.0 && more[1].top() == t.3 + 100.0, "{more:?}");
}

#[test]
fn line_height_lengths_are_inherited_as_lengths() {
    // Like in browsers: `20px`, `1.25em` and `125%` (of 16px) reach the 32px
    // text as 20px, while the number `1.25` is applied to its font size.
    let html = "<style>body { margin: 0 } p { margin: 0 } b { font-size: 32px } \
        .px { line-height: 20px } .em { line-height: 1.25em } .pct { line-height: 125% } .n { line-height: 1.25 }</style>\
        <div class=px><p class=a><b>A</b></p></div><div class=em><p class=b><b>B</b></p></div>\
        <div class=pct><p class=c><b>C</b></p></div><div class=n><p class=d><b>D</b></p></div>\
        <div class=px><p class=e>E<br>E</p></div>";
    let l = Laid::new(html, 300.0);
    for class in ["a", "b", "c"] {
        assert_eq!(l.rect(class).3, 20.0, ".{class}");
    }
    assert_eq!(l.rect("d").3, 40.0);
    assert_eq!(l.rect("e").3, 40.0);
}

#[test]
fn inline_floats_are_hoisted_to_their_line() {
    let html = "<style>body { margin: 0 } h3 { margin: 0 } .badge { float: right; width: 40px; height: 16px }</style>\
                <h3 class=title>Title <span class=badge>new</span></h3>";
    let l = Laid::new(html, 300.0);
    assert_eq!(l.rect("badge"), (260.0, 0.0, 40.0, 16.0));
}

const POSITIONED: &str = "<style>body { margin: 0 } p { margin: 0 } \
    .rel { position: relative; margin-left: 20px; width: 300px; height: 200px; padding: 10px; border: 2px solid #000 } \
    .corner { position: absolute; top: 5px; right: 15px; width: 50px; height: 40px } \
    .footer { position: absolute; left: 0; right: 0; bottom: 0; height: 30px } \
    .auto { position: absolute; right: 0; bottom: 0 } \
    .shift { position: relative; top: 10px; left: 5px; height: 20px } .next { height: 20px } \
    .fixed { position: fixed; right: 0; bottom: 0; width: 10px; height: 10px }</style>\
    <div class=rel><div class=corner></div><div class=footer></div><p class=first>content</p>\
    <span class=auto>auto sized</span></div>\
    <div class=shift>shifted</div><div class=next>next</div><div class=fixed></div>";

#[test]
fn absolute_boxes_in_the_padding_box_of_a_relative_ancestor() {
    let l = Laid::new(POSITIONED, 500.0);
    assert_eq!(l.rect("rel"), (20.0, 0.0, 324.0, 224.0));
    // Padding box: (22, 2) - (342, 222).
    assert_eq!(l.rect("corner"), (277.0, 7.0, 50.0, 40.0));
    assert_eq!(l.rect("footer"), (22.0, 192.0, 320.0, 30.0));
    // Absolutely positioned boxes take no room in the flow.
    assert_eq!(l.rect("first").1, 12.0);
    // Shrink-to-fit width, placed from the bottom right.
    let auto = l.rect("auto");
    assert!(auto.2 > 20.0 && auto.2 < 200.0, "{auto:?}");
    assert_eq!((auto.0 + auto.2, auto.1 + auto.3), (342.0, 222.0));
    // `relative` moves the box but not the flow.
    assert_eq!(l.rect("shift"), (5.0, 234.0, 500.0, 20.0));
    assert_eq!(l.rect("next"), (0.0, 244.0, 500.0, 20.0));
    // `fixed`: in the first screen (500x600).
    assert_eq!(l.rect("fixed"), (490.0, 590.0, 10.0, 10.0));
}

const STICKY: &str = "<style>body { margin: 0 } \
    .sec { height: 400px } .head { position: sticky; top: 10px; height: 30px; background: #ff0000 } \
    .body { height: 300px; background: #00ff00 } .clip { overflow: hidden; height: 400px } \
    .inner { position: sticky; top: 0; height: 20px; background: #0000ff } \
    .fixed { position: fixed; right: 0; bottom: 0; width: 10px; height: 10px; background: #ff00ff } \
    .static { position: fixed; left: 20px; width: 10px; height: 10px; background: #00ffff }</style>\
    <div class=sec><div class=head></div><div class=body></div></div>\
    <div class=clip><div style='height: 50px'></div><div class=inner></div></div>\
    <div style='height: 2000px'></div><div class=fixed></div>";

#[test]
fn sticky_boxes_stick_to_the_scrolled_viewport() {
    let size = vec2(500.0, 300.0);
    let red = Color32::from_rgb(255, 0, 0);
    let at = |offset: f32| fill_rect(&painted_scrolled(STICKY, size, offset), red);
    // 10px below the top of the viewport from the start, while the section
    // scrolls under it...
    assert_eq!(at(0.0).top(), 10.0);
    assert_eq!(at(100.0).top(), 10.0);
    // ...until the end of its section pushes it out (section bottom 400 - 390).
    assert_eq!(at(390.0).top(), -20.0);
    assert_eq!(at(390.0).bottom(), 10.0);
    // The stuck header paints above the content that scrolls under it.
    let shapes = painted_scrolled(STICKY, size, 100.0);
    assert!(fill_index(&shapes, red) > fill_index(&shapes, Color32::from_rgb(0, 255, 0)));
    // `overflow: hidden` is the scroll container of its sticky boxes: it
    // does not scroll, so they stay in place.
    let blue = |offset: f32| fill_rect(&painted_scrolled(STICKY, size, offset), Color32::from_rgb(0, 0, 255));
    assert_eq!(blue(0.0).top(), 450.0);
    assert_eq!(blue(420.0).top(), 30.0);
    // Without scrolling the layout is unchanged.
    let l = Laid::new(STICKY, 500.0);
    assert_eq!(l.rect("head"), (0.0, 10.0, 500.0, 30.0));
    assert_eq!(l.rect("body").1, 30.0);
}

#[test]
fn fixed_boxes_stay_in_the_viewport() {
    let size = vec2(500.0, 300.0);
    let html = format!("<div style='height: 50px'></div><div class=static></div>{STICKY}");
    for offset in [0.0, 200.0, 1500.0] {
        let shapes = painted_scrolled(&html, size, offset);
        let fixed = fill_rect(&shapes, Color32::from_rgb(255, 0, 255));
        assert_eq!((fixed.left(), fixed.top()), (490.0, 290.0), "scrolled by {offset}");
        // `top: auto`: where it would be in the unscrolled document, also when scrolled.
        let s = fill_rect(&shapes, Color32::from_rgb(0, 255, 255));
        assert_eq!((s.left(), s.top()), (20.0, 50.0), "scrolled by {offset}");
    }
}

#[test]
fn sticky_table_headers_of_long_tables() {
    let rows: String = (0..200).map(|i| format!("<tr><td>row {i}</td></tr>")).collect();
    let html = format!(
        "<style>body {{ margin: 0 }} table {{ border-spacing: 0 }} \
         th {{ position: sticky; top: 0; background: #ff00ff; height: 30px }} td {{ height: 20px }}</style>\
         <div style='height: 100px'></div><table class=t><thead><tr><th class=h>Head</th></tr></thead>\
         <tbody>{rows}</tbody></table><div style='height: 1000px'></div>"
    );
    let l = Laid::new(&html, 500.0);
    let (table, head) = (l.rect("t"), l.rect("h"));
    assert_eq!(head.1, 100.0);
    let size = vec2(500.0, 300.0);
    let magenta = Color32::from_rgb(255, 0, 255);
    assert_eq!(fill_rect(&painted_scrolled(&html, size, 50.0), magenta).top(), 50.0);
    // Far down a virtualized table the header row is still drawn, at the top.
    let shapes = painted_scrolled(&html, size, 2000.0);
    assert_eq!(fill_rect(&shapes, magenta).top(), 0.0);
    assert!(text_rect(&shapes, "Head").top() >= 0.0);
    // At the end of the table it scrolls away with it.
    let end = table.1 + table.3;
    assert_eq!(fill_rect(&painted_scrolled(&html, size, end - 20.0), magenta).bottom(), 20.0, "{table:?} {head:?}");
}

#[test]
fn z_index_orders_painting() {
    let html = "<style>body { margin: 0 } div { height: 40px } \
        .flow { background: #808080 } \
        .a { position: absolute; top: 0; left: 0; width: 50px; z-index: 2; background: #ff0000 } \
        .b { position: absolute; top: 10px; left: 10px; width: 50px; z-index: 1; background: #00ff00 } \
        .under { position: absolute; top: 0; left: 0; width: 300px; z-index: -1; background: #0000ff } \
        .auto { position: absolute; top: 5px; left: 5px; width: 20px; background: #00ffff } \
        .sc { position: relative; z-index: 3; background: #ffff00 } \
        .badge { position: absolute; top: 0; right: 0; width: 10px; height: 10px; background: #800080 } \
        .later { margin-top: -20px; background: #ffa500 }</style>\
        <div class=a></div><div class=b></div><div class=under></div><div class=auto></div>\
        <div class=flow>flow</div><div class=sc><div class=badge></div></div><div class=later></div>";
    let shapes = painted(html, 400.0);
    let at = |hex: u32| fill_index(&shapes, Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8));
    let (under, flow, later) = (at(0x0000ff), at(0x808080), at(0xffa500));
    let (auto, b, a, sc, badge) = (at(0x00ffff), at(0x00ff00), at(0xff0000), at(0xffff00), at(0x800080));
    // Negative z-index below the flow, then the flow, then positioned boxes by z-index.
    assert!(under < flow && flow < later, "{under} {flow} {later}");
    assert!(later < auto && auto < b && b < a && a < sc, "{auto} {b} {a} {sc}");
    // The relatively positioned box with a z-index paints its absolute child above itself.
    assert!(sc < badge, "{sc} {badge}");
}

#[test]
fn flex_rows_grow_justify_and_align() {
    let html = "<style>body { margin: 0 } \
        .between { display: flex; justify-content: space-between; align-items: flex-start } \
        .grow { display: flex; gap: 10px } .center { display: flex; align-items: center } \
        .push { display: flex }</style>\
        <div class=between><div class=b1 style='width: 50px; height: 10px'></div>\
        <div class=b2 style='width: 50px; height: 20px'></div></div>\
        <div class=grow><div class=g1 style='flex: 1'>a</div><div class=g2 style='flex: 2'>b</div></div>\
        <div class=center style='height: 50px'><div class=c1 style='width: 10px; height: 10px'></div>\
        <div class=c2 style='width: 10px; height: 30px; align-self: stretch'></div></div>\
        <div class=push><div class=p1 style='width: 30px'>x</div><div class=p2 style='width: 30px; margin-left: auto'>y</div></div>";
    let l = Laid::new(html, 310.0);
    assert_eq!(l.rect("b1"), (0.0, 0.0, 50.0, 10.0));
    assert_eq!(l.rect("b2"), (260.0, 0.0, 50.0, 20.0));
    let (g1, g2) = (l.rect("g1"), l.rect("g2"));
    assert_eq!((g1.0, g1.2, g2.0, g2.2), (0.0, 100.0, 110.0, 200.0));
    // Items stretch to the line by default.
    assert_eq!(g1.3, g2.3);
    let c1 = l.rect("c1");
    assert_eq!((c1.1 - l.rect("center").1, c1.3), (20.0, 10.0));
    assert_eq!(l.rect("c2").3, 30.0, "an explicit height is not stretched");
    assert_eq!(l.rect("p2").0, 280.0);
}

#[test]
fn flex_bar_chart_with_percentage_heights() {
    let html = "<style>body { margin: 0 } .chart { display: flex; align-items: flex-end; height: 100px; gap: 4px } \
        .bar { width: 20px; background: #4caf50 }</style>\
        <div class=chart><div class='bar b40' style='height: 40%'></div><div class='bar b90' style='height: 90%'></div></div>";
    let l = Laid::new(html, 300.0);
    assert_eq!(l.rect("b40"), (0.0, 60.0, 20.0, 40.0));
    assert_eq!(l.rect("b90"), (24.0, 10.0, 20.0, 90.0));
}

#[test]
fn column_flex_has_gaps_and_cross_alignment() {
    let html = "<style>body { margin: 0 } .col { display: flex; flex-direction: column; gap: 8px; align-items: center } \
        .i { width: 40px; height: 10px }</style>\
        <div class=col><div class='i i1'></div><div class='i i2'></div></div>";
    let l = Laid::new(html, 200.0);
    assert_eq!(l.rect("i1"), (80.0, 0.0, 40.0, 10.0));
    assert_eq!(l.rect("i2"), (80.0, 18.0, 40.0, 10.0));
}

#[test]
fn box_sizing_min_max_and_calc() {
    let html = "<style>body { margin: 0 } div { height: 10px } \
        .bb { box-sizing: border-box; width: 100px; padding: 10px; border: 5px solid #000 } \
        .cb { width: 100px; padding: 10px; border: 5px solid #000 } \
        .max { max-width: 200px } .min { width: 10px; min-width: 50px } \
        .calc { width: calc(100% - 40px); margin-left: 20px } \
        .tall { min-height: 30px } .short { height: 50px; max-height: 25px }</style>\
        <div class=bb></div><div class=cb></div><div class=max></div><div class=min></div>\
        <div class=calc></div><div class=tall></div><div class=short></div>";
    let l = Laid::new(html, 400.0);
    assert_eq!(l.rect("bb").2, 100.0);
    assert_eq!(l.rect("cb").2, 130.0);
    assert_eq!(l.rect("bb").3, 30.0, "border-box height is never smaller than padding and borders");
    assert_eq!(l.rect("max").2, 200.0);
    assert_eq!(l.rect("min").2, 50.0);
    assert_eq!(l.rect("calc"), (20.0, l.rect("calc").1, 360.0, 10.0));
    assert_eq!(l.rect("tall").3, 30.0);
    assert_eq!(l.rect("short").3, 25.0);
}

#[test]
fn inline_blocks_with_a_size_stay_in_the_line() {
    let html = "<style>body { margin: 0 } p { margin: 0 } \
        .dot { display: inline-block; width: 10px; height: 10px; border-radius: 50%; background: #2e7d32 } \
        .pill { display: inline-block; padding: 2px 8px; border-radius: 9px; background: #1565c0; color: white }</style>\
        <p class=line>Status <span class=dot></span> online <span class=pill>v2</span></p>";
    let l = Laid::new(html, 400.0);
    let (line, dot, pill) = (l.rect("line"), l.rect("dot"), l.rect("pill"));
    assert_eq!((dot.2, dot.3), (10.0, 10.0));
    assert!(dot.0 > 20.0 && dot.1 >= line.1 && dot.1 + dot.3 <= line.1 + line.3, "{dot:?} in {line:?}");
    assert!(pill.0 > dot.0 + 10.0 && pill.1 < line.1 + line.3, "{pill:?} on the same line as {dot:?}");
    // The pill keeps its padding: 8px on each side of the text.
    let text = text_rect(&painted(html, 400.0), "v2");
    assert!((text.left() - pill.0 - 8.0).abs() < 1.0, "{text:?} {pill:?}");
}

#[test]
fn large_grids_and_positioned_tables_stay_fast() {
    use serde_json::json;
    let cards: Vec<serde_json::Value> = (0..10_000).map(|i| json!({"id": i})).collect();
    let rows: Vec<serde_json::Value> = (0..10_000).map(|i| json!({"id": i})).collect();
    let template = "<style>.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(120px, 1fr)); gap: 8px } \
        .card { border: 1px solid #ccc; padding: 4px } .wrap { position: relative } \
        .tag { position: absolute; top: 0; right: 0 } .side { float: right; width: 80px }</style>\
        <div class=wrap><span class=tag>10k</span><div class=side>side</div>\
        <table>{{#each rows}}<tr><td>row-{{id}}</td></tr>{{/each}}</table></div>\
        <div class=grid>{{#each cards}}<div class=card>card-{{id}}</div>{{/each}}</div>";
    let doc = merpati_html::parse(template, &json!({"cards": cards, "rows": rows})).unwrap();
    let ctx = Context::default();
    let start = std::time::Instant::now();
    let mut texts = 0;
    for _ in 0..4 {
        let input =
            RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                doc.show(ui, &mut |_: &str| {});
            });
        });
        out.textures_delta.clear();
        fn count(s: &Shape) -> usize {
            match s {
                Shape::Vec(v) => v.iter().map(count).sum(),
                Shape::Text(_) => 1,
                _ => 0,
            }
        }
        texts = out.shapes.iter().map(|s| count(&s.shape)).sum::<usize>();
    }
    let elapsed = start.elapsed();
    eprintln!("4 frames of a 10k-row table and a 10k-card grid: {elapsed:?}, {texts} texts painted");
    assert!(texts < 1000, "only the visible part is painted ({texts} texts)");
    assert!(elapsed.as_secs_f32() < 20.0, "{elapsed:?}");
}

#[test]
fn layout_boxes_are_relative_to_the_document() {
    let l = Laid::new("<div class=x style='height: 10px'></div>", 300.0);
    // The body's 8px margin.
    assert_eq!(l.rect("x"), (8.0, 8.0, 284.0, 10.0));
    let _ = pos2(0.0, 0.0);
}

#[test]
fn flex_items_are_the_child_elements() {
    let html = "<style>body { margin: 0 } .bar { display: flex; justify-content: space-between }</style>\
                <div class=bar><span class=l>Left</span> <a class=r href='x'>Right</a></div>";
    let l = Laid::new(html, 300.0);
    assert_eq!(l.rect("l").0, 0.0);
    let r = l.rect("r");
    assert_eq!(r.0 + r.2, 300.0);
}

/// Every block with `class`, in document order.
fn all_with_class<'a>(b: &'a Block, class: &str, out: &mut Vec<&'a Block>) {
    if b.classes.iter().any(|c| c == class) {
        out.push(b);
    }
    for i in b.inlines() {
        if let merpati_html::Inline::Block(x) = i {
            all_with_class(x, class, out);
        }
    }
    for c in b.children() {
        all_with_class(c, class, out);
    }
    if let Some(t) = b.table() {
        for c in t.rows.iter().flat_map(|r| &r.cells) {
            all_with_class(&c.block, class, out);
        }
    }
}

fn fixture(name: &str) -> String {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/templates/");
    let template = std::fs::read_to_string(format!("{dir}{name}.hbs")).expect("template");
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}{name}.json")).expect("data")).expect("json");
    merpati_html::render_template(&template, &data).expect("renders")
}

impl Laid {
    fn rects(&self, class: &str) -> Vec<(f32, f32, f32, f32)> {
        let mut blocks = Vec::new();
        all_with_class(&self.doc.root, class, &mut blocks);
        blocks
            .iter()
            .map(|b| {
                let r = self.boxes[&b.key];
                (r.left().round(), r.top().round(), r.width().round(), r.height().round())
            })
            .collect()
    }
}

#[test]
fn dashboard_template() {
    let html = fixture("dashboard");
    let wide = Laid::new(&html, 900.0);
    assert!(!wide.doc.needs_browser());
    // Four KPI cards on one line (auto-fit), two per line when narrow.
    let kpis = wide.rects("kpi");
    assert_eq!(kpis.len(), 4);
    assert!(kpis.iter().all(|k| k.1 == kpis[0].1 && k.2 == 208.0), "{kpis:?}");
    let narrow = Laid::new(&html, 420.0);
    let small = narrow.rects("kpi");
    assert_eq!((small[1].1, small[2].1 > small[1].1, small[2].0), (small[0].1, true, 16.0), "{small:?}");
    // Trend badges sit in the top-right corner of their card (inside the 1px border).
    for (kpi, trend) in kpis.iter().zip(wide.rects("trend")) {
        assert_eq!((trend.0 + trend.2, trend.1), (kpi.0 + kpi.2 - 11.0, kpi.1 + 11.0), "{trend:?} in {kpi:?}");
    }
    // 2fr / 1fr panels.
    let panels = wide.rects("panel");
    assert_eq!((panels[0].2, panels[1].2), (568.0, 284.0));
    // Progress bars: 100% of an 8px track, as wide as the percentage.
    let bars = wide.rects("bar");
    let inner = wide.doc.root.find_class("bar").unwrap().children()[0].key;
    let fill = wide.boxes[&inner];
    assert_eq!((fill.width().round(), fill.height().round()), ((bars[0].2 * 0.35).round(), 8.0));
    // Flex rows with spans: the percentage is right-aligned.
    let row = wide.rects("row")[0];
    let pct = wide.doc.root.find_class("row").unwrap().children()[1].key;
    assert_eq!(wide.boxes[&pct].right().round(), row.0 + row.2);
    let shapes = painted(&html, 900.0);
    for t in ["API health", "12.4k", "+8%", "POST /orders", "Database"] {
        text_rect(&shapes, t);
    }
}

#[test]
fn float_profiles_template() {
    let html = fixture("profiles");
    let l = Laid::new(&html, 900.0);
    let cards = l.rects("card");
    assert_eq!(cards.len(), 5);
    // Three cards per line, the rest below.
    assert!(cards[0].1 == cards[2].1 && cards[3].1 > cards[0].1 && cards[3].0 == cards[0].0, "{cards:?}");
    // The clearfix container holds the floats; the footer clears them.
    let wrap = l.rects("clearfix")[0];
    let last = cards[4];
    assert!(wrap.1 + wrap.3 >= last.1 + last.3, "{wrap:?} {last:?}");
    assert!(l.rect("footer").1 >= last.1 + last.3);
    // The note floats right of the intro text.
    let note = l.rect("note");
    assert_eq!((note.0 + note.2).round(), 900.0 - 12.0);
    let shapes = painted(&html, 900.0);
    let intro = text_rect(&shapes, "people signed up");
    assert!(intro.right() <= note.0 + 0.5, "{intro:?} {note:?}");
    // The ribbon is clipped by its card (`overflow: hidden`): it still paints.
    text_rect(&shapes, "ADMIN");
    // Narrow: the cards keep their percentage widths.
    let narrow = Laid::new(&html, 400.0);
    assert!(narrow.rects("card").iter().all(|c| (c.2 - (376.0f32 * 0.31).round()).abs() <= 1.0));
}

#[test]
fn weather_template() {
    let html = fixture("weather");
    let l = Laid::new(&html, 600.0);
    let hero = l.rect("hero");
    assert_eq!((hero.2, hero.3), (576.0, 160.0));
    let now = l.rect("now");
    assert_eq!((now.0, now.1 + now.3), (hero.0 + 16.0, hero.1 + 160.0 - 14.0));
    let cond = l.rect("cond");
    assert_eq!((cond.0 + cond.2, cond.1), (hero.0 + hero.2 - 16.0, hero.1 + 14.0));
    // Centered with `left: 50%` and a negative margin.
    let alert = l.rect("alert");
    assert_eq!(alert.0 + alert.2 / 2.0, hero.0 + hero.2 / 2.0);
    let days = l.rects("day");
    assert_eq!(days.len(), 5);
    assert!(days.iter().all(|d| d.2 == days[0].2 && d.3 == days[0].3), "{days:?}");
    assert_eq!(days[4].0 + days[4].2, hero.0 + hero.2);
    // `max-height` + `overflow: auto`: a scroll area of 90px plus padding.
    let pre = l.doc.root.find("pre").unwrap().key;
    assert_eq!(l.boxes[&pre].height().round(), 106.0);
    let shapes = painted(&html, 600.0);
    for t in ["24°", "Light rain", "Thunderstorm watch", "Mon", "\"city\""] {
        text_rect(&shapes, t);
    }
}

#[test]
fn min_max_clamp_widths() {
    let html = r#"<div class="wrap" style="width: 500px"><div class="a" style="width: min(100%, 300px)">a</div>
        <div class="b" style="width: clamp(100px, 50%, 200px)">b</div><div class="c" style="width: max(80%, 100px)">c</div></div>
        <div class="narrow" style="width: 200px"><div class="d" style="width: min(100%, 300px)">d</div></div>"#;
    let l = Laid::new(html, 800.0);
    assert_eq!(l.rect("a").2, 300.0);
    assert_eq!(l.rect("b").2, 200.0);
    assert_eq!(l.rect("c").2, 400.0);
    assert_eq!(l.rect("d").2, 200.0);
}

#[test]
fn media_queries_follow_the_viewport_width() {
    let html = r#"<style>body { margin: 0 } .card { width: 50% } @media (max-width: 600px) { .card { width: 100% } }
        @media (width >= 1200px) { .card { width: 25% } }</style><div class="card">c</div>"#;
    let at = |w: f32| {
        let doc = Document::from_html_at(html, w);
        let boxes = doc.layout_boxes(vec2(w, 600.0), &opts());
        boxes[&doc.root.find_class("card").unwrap().key].width().round()
    };
    assert_eq!(at(900.0), 450.0);
    assert_eq!(at(400.0), 400.0);
    assert_eq!(at(1600.0), 400.0);
    let doc = Document::from_html_at(html, 900.0);
    assert!(!doc.restyle_needed(950.0));
    assert!(doc.restyle_needed(500.0) && doc.restyle_needed(1300.0));
    assert!(!doc.restyled(500.0).restyle_needed(320.0));
    // Without a width, width queries do not apply (and are restyled once known).
    let doc = Document::from_html(html);
    assert!(!doc.restyle_needed(900.0) && doc.restyle_needed(400.0));
}

#[test]
fn fr_rows_share_the_height() {
    // `grid-auto-rows: 1fr` without a height: every row as tall as the tallest.
    let html = r#"<div style="display: grid; grid-template-columns: 100px 100px; grid-auto-rows: 1fr; gap: 0">
        <div class="a">short</div><div class="b">x</div>
        <div class="c">one<br>two<br>three<br>four</div><div class="d">y</div></div>"#;
    let l = Laid::new(html, 400.0);
    let (a, c) = (l.rect("a"), l.rect("c"));
    assert!(c.3 > 60.0, "{c:?}");
    assert_eq!(a.3, c.3, "{a:?} {c:?}");
    assert_eq!(c.1 - a.1, a.3);
    // A definite height is shared by factor; content taller than its share keeps its size.
    let html = r#"<div style="display: grid; height: 300px; grid-template-rows: 50px 1fr 2fr">
        <div class="h">head</div><div class="one">1</div><div class="two">2</div></div>"#;
    let l = Laid::new(html, 400.0);
    assert_eq!(l.rect("h").3, 50.0);
    assert_eq!(l.rect("one").3, 83.3);
    assert_eq!(l.rect("two").3, 166.7);
}

#[test]
fn nested_floats_lay_out_in_linear_time() {
    // Each float re-queued the paint entries of the floats inside it,
    // placeholders included, so the paint list doubled with every level.
    let html = "<div style='float: left'>x".repeat(16);
    let doc = Document::from_html(&html);
    let ctx = Context::default();
    let input =
        RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))), ..Default::default() };
    let mut out = ctx.run_ui(input, |ui| {
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            doc.ui(ui, &mut |_: &str| {}, &opts());
        });
    });
    out.textures_delta.clear();
    fn texts(s: &Shape) -> usize {
        match s {
            Shape::Vec(v) => v.iter().map(texts).sum(),
            Shape::Text(_) => 1,
            _ => 0,
        }
    }
    assert_eq!(out.shapes.iter().map(|s| texts(&s.shape)).sum::<usize>(), 16);
    assert!(out.shapes.len() < 1000, "{} paint entries for 16 floats", out.shapes.len());
}

#[test]
fn deeply_nested_markup_does_not_overflow_the_stack() {
    // Unclosed tags in a loop nest once per item; every pass over the tree
    // (styles, layout, painting, dropping) used to recurse that deep.
    let run = || {
        let doc = Document::from_html(&"<div>x".repeat(6_000));
        let boxes = doc.layout_boxes(vec2(800.0, 600.0), &opts());
        assert!(!boxes.is_empty());
        let doc = Document::from_html(&"<ul><li>y".repeat(2_000));
        assert!(doc.outline().contains('y'));
    };
    // Debug builds use several times the stack of release builds per level.
    std::thread::Builder::new().stack_size(64 << 20).spawn(run).unwrap().join().unwrap();
}

#[test]
fn huge_lengths_do_not_panic() {
    // 1e38em (or 1e38% of the width) used to overflow to infinity; the two
    // margins then summed to NaN and `f32::clamp` panicked in flex layout.
    let big = format!("1{}", "0".repeat(38));
    for unit in ["em", "%"] {
        let html = format!(
            "<div style='display: flex'><div class=item style='max-width: 100px; \
             margin-left: {big}{unit}; margin-right: -{big}{unit}'>x</div></div>"
        );
        let l = Laid::new(&html, 400.0);
        let (x, _, w, _) = l.rect("item");
        assert!(x.is_finite() && w.is_finite(), "{unit}: {:?}", l.rect("item"));
    }
}

#[test]
fn inline_blocks_with_large_negative_margins() {
    // A margin box narrower than nothing was handed to egui as a negative size.
    let html = "<p>a <span class=b style='display: inline-block; width: 50px; margin-left: -500px'>b</span> c</p>";
    let l = Laid::new(html, 400.0);
    assert_eq!(l.rect("b").2, 50.0);
}
