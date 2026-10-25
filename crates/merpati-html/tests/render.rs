//! Headless egui runs of the renderer: no pixels, but layout, painting,
//! virtualization and link clicks are exercised for real.

use egui::{Context, Event, Pos2, RawInput, Rect, Shape, pos2, vec2};
use merpati_html::{Document, Options, Paper, parse};
use serde_json::json;

struct Texts(Vec<(String, Rect)>);

fn collect(shape: &Shape, out: &mut Vec<(String, Rect)>) {
    match shape {
        Shape::Text(t) => {
            if t.override_text_color != Some(egui::Color32::TRANSPARENT) && !t.galley.text().is_empty() {
                out.push((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2())));
            }
        }
        Shape::Vec(v) => v.iter().for_each(|s| collect(s, out)),
        _ => {}
    }
}

fn input(events: Vec<Event>) -> RawInput {
    RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))), events, ..Default::default() }
}

/// Runs one frame and returns the painted texts.
fn frame(ctx: &Context, doc: &Document, events: Vec<Event>, clicked: &mut Vec<String>) -> Texts {
    let mut out = ctx.run_ui(input(events), |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            doc.show(ui, &mut |href: &str| clicked.push(href.to_string()));
        });
    });
    out.textures_delta.clear();
    let mut texts = Vec::new();
    for s in &out.shapes {
        collect(&s.shape, &mut texts);
    }
    Texts(texts)
}

impl Texts {
    fn find(&self, needle: &str) -> Option<&(String, Rect)> {
        self.0.iter().find(|(t, _)| t.contains(needle))
    }
}

#[test]
fn renders_text_and_clicks_links() {
    let doc = Document::from_html(
        "<h1>Report</h1><p>See <a href='https://example.com/docs'>the docs</a> for <b>details</b>.</p>",
    );
    let ctx = Context::default();
    let mut clicked = Vec::new();
    let mut texts = frame(&ctx, &doc, vec![], &mut clicked);
    for _ in 0..2 {
        texts = frame(&ctx, &doc, vec![], &mut clicked);
    }
    let (_, h1) = texts.find("Report").expect("heading painted").clone();
    let (para, rect) = texts.find("the docs").expect("paragraph painted").clone();
    assert_eq!(para, "See the docs for details.");
    assert!(h1.height() > rect.height() * 1.5, "h1 is bigger: {h1:?} vs {rect:?}");
    // Click roughly in the middle of "the docs" (characters 4..12 of 25).
    let x = rect.left() + rect.width() * 8.0 / 25.0;
    let p = pos2(x, rect.center().y);
    frame(&ctx, &doc, vec![Event::PointerMoved(p)], &mut clicked);
    let press = |pressed| Event::PointerButton {
        pos: p,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    frame(&ctx, &doc, vec![press(true)], &mut clicked);
    frame(&ctx, &doc, vec![press(false)], &mut clicked);
    assert_eq!(clicked, ["https://example.com/docs"]);

    // Clicking plain text does nothing.
    let q = pos2(rect.right() - 2.0, rect.center().y);
    frame(&ctx, &doc, vec![Event::PointerMoved(q)], &mut clicked);
    let press_q = |pressed| Event::PointerButton {
        pos: q,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    frame(&ctx, &doc, vec![press_q(true)], &mut clicked);
    frame(&ctx, &doc, vec![press_q(false)], &mut clicked);
    assert_eq!(clicked.len(), 1);
}

#[test]
fn large_tables_only_paint_visible_rows() {
    let rows: Vec<serde_json::Value> = (0..10_000).map(|i| json!({"id": i, "name": format!("user-{i}")})).collect();
    let doc = parse(
        "<table style='border-collapse: collapse'><thead><tr><th>Id</th><th>Name</th></tr></thead>\
         {{#each rows}}<tr><td style='border-bottom: 1px solid #ccc'>{{id}}</td><td>{{name}}</td></tr>{{/each}}</table>",
        &json!({ "rows": rows }),
    )
    .unwrap();
    let ctx = Context::default();
    let mut clicked = Vec::new();
    let start = std::time::Instant::now();
    let mut texts = frame(&ctx, &doc, vec![], &mut clicked);
    for _ in 0..3 {
        texts = frame(&ctx, &doc, vec![], &mut clicked);
    }
    eprintln!("4 frames of a 10k-row table: {:?}", start.elapsed());
    assert!(texts.find("user-0").is_some());
    assert!(texts.find("user-9999").is_none());
    let painted = texts.0.iter().filter(|(t, _)| t.starts_with("user-")).count();
    assert!(painted > 10 && painted < 200, "painted {painted} rows");

    // Scrolling far down shows later rows.
    let scroll = Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(0.0, -150_000.0),
        modifiers: Default::default(),
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Wheel,
    };
    frame(&ctx, &doc, vec![Event::PointerMoved(pos2(400.0, 300.0))], &mut clicked);
    frame(&ctx, &doc, vec![scroll], &mut clicked);
    for _ in 0..3 {
        texts = frame(&ctx, &doc, vec![], &mut clicked);
    }
    assert!(texts.find("user-0").is_none());
    let visible: Vec<&String> = texts.0.iter().map(|(t, _)| t).filter(|t| t.starts_with("user-")).collect();
    assert!(!visible.is_empty(), "rows after scrolling");
}

fn text_colors(doc: &Document, dark: bool, paper: Paper) -> std::collections::HashMap<String, egui::Color32> {
    let ctx = Context::default();
    ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
    let mut out = ctx.run_ui(input(vec![]), |ui| {
        egui::CentralPanel::default().show(ui, |ui| {
            doc.ui(ui, &mut |_: &str| {}, &Options { paper, ..Default::default() });
        });
    });
    out.textures_delta.clear();
    let mut colors = std::collections::HashMap::new();
    fn walk(s: &Shape, out: &mut std::collections::HashMap<String, egui::Color32>) {
        match s {
            Shape::Text(t) => {
                let c = t.galley.job.sections.first().map(|s| s.format.color).unwrap_or_default();
                out.insert(t.galley.text().to_string(), c);
            }
            Shape::Vec(v) => v.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    for s in &out.shapes {
        walk(&s.shape, &mut colors);
    }
    colors
}

fn lum(c: egui::Color32) -> u32 {
    (c.r() as u32 + c.g() as u32 + c.b() as u32) / 3
}

#[test]
fn auto_paper_follows_the_theme_only_for_unstyled_pages() {
    let plain = Document::from_html("<p>plain</p><p><a href='x'>link</a></p>");
    assert!(!plain.sets_backgrounds);
    assert!(lum(text_colors(&plain, true, Paper::Auto)["plain"]) > 128);
    assert!(lum(text_colors(&plain, false, Paper::Auto)["plain"]) < 128);
    let styled = Document::from_html("<p>plain</p><div style='background:#eef'>box</div>");
    assert!(styled.sets_backgrounds);
    // Designed for a white page: drawn on white (dark text) even in dark mode.
    assert!(lum(text_colors(&styled, true, Paper::Auto)["plain"]) < 128);
    assert!(lum(text_colors(&styled, true, Paper::Theme)["plain"]) > 128);
}

#[test]
fn dark_and_light_defaults() {
    let doc = Document::from_html(
        "<p>plain</p><div style='background: #ffffff'><p>on white</p></div><p style='color: #1c1d20'>dark ink</p>         <p style='color: #333'><span style='background: #27ae60; color: white'>badge</span></p>",
    );
    for dark in [true, false] {
        let colors = text_colors(&doc, dark, Paper::Theme);
        assert_eq!(colors["badge"], egui::Color32::WHITE, "white on the badge's own green background");
        let plain = colors["plain"];
        if dark {
            assert!(lum(plain) > 128, "light text on the dark panel: {plain:?}");
            assert!(lum(colors["dark ink"]) > 128, "unreadable author color replaced");
        } else {
            assert!(lum(plain) < 128, "dark text on the light panel: {plain:?}");
            assert_eq!(colors["dark ink"], egui::Color32::from_rgb(0x1c, 0x1d, 0x20));
        }
        assert!(lum(colors["on white"]) < 100, "dark text on a white box ({dark})");
    }
}

#[test]
fn every_feature_renders_without_panicking() {
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    let html = format!(
        "<style>.c {{ display:inline-block; width: 120px; border: 1px dashed red; border-radius: 6px; padding: 4px }}\
         td {{ border: 2px dotted blue }} th {{ border-bottom: 3px double #000 }}</style>\
         <h2 style='text-align:center'>Center</h2><p style='text-align:right'>right</p>\
         <p style='text-align: justify'>justified text that is long enough to wrap around in a narrow box maybe</p>\
         <ul><li>one</li><li style='list-style: square'>two</li></ul><ol type=A><li>x</li></ol>\
         <pre>a\n  b</pre><blockquote>q</blockquote><hr>\
         <div style='display:flex;flex-wrap:wrap;gap:4px'><div class=c>card 1</div><div class=c>card 2</div></div>\
         <p><img src='data:image/png;base64,{png}' width=20 height=20> <img src='https://x.test/a.png' alt='remote'> \
         <img src='rel.png'></p><p style='text-align:center'><img src='data:image/png;base64,{png}'></p>\
         <table width=300 border=1><tr><th colspan=2>head</th></tr><tr><td rowspan=2>a</td><td>b</td></tr>\
         <tr><td style='vertical-align: bottom'>c</td></tr></table>\
         <p>x<sub>2</sub> y<sup>3</sup> <mark>m</mark> <code>c</code> <kbd>k</kbd> <s>s</s></p>"
    );
    let doc = Document::from_html(&html);
    let ctx = Context::default();
    let mut clicked = Vec::new();
    for _ in 0..3 {
        let texts = frame(&ctx, &doc, vec![], &mut clicked);
        assert!(texts.find("Center").is_some());
        assert!(texts.find("[remote]").is_some(), "alt text for blocked remote images");
    }
}
