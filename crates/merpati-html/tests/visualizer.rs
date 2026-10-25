//! Layout-tree tests with templates typical for the Postman visualizer.

use egui::Color32;
use merpati_html::{Content, Document, Inline, Len, TextAlign, parse};
use serde_json::json;

fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn users() -> serde_json::Value {
    json!({"response": [
        {"name": "Ada Lovelace", "email": "ada@example.com", "active": true},
        {"name": "Alan <Turing>", "email": "alan@example.com", "active": false},
        {"name": "Grace Hopper", "email": "grace@example.com", "active": true}
    ]})
}

/// Postman's generated "table" visualization.
const POSTMAN_TABLE: &str = r#"
<style type="text/css">
    .tftable {font-size:14px;color:#333333;width:100%;border-width: 1px;border-color: #87ceeb;border-collapse: collapse;}
    .tftable th {font-size:18px;background-color:#87ceeb;border-width: 1px;padding: 8px;border-style: solid;border-color: #87ceeb;text-align:left;}
    .tftable tr {background-color:#ffffff;}
    .tftable td {font-size:14px;border-width: 1px;padding: 8px;border-style: solid;border-color: #87ceeb;}
    .tftable tr:hover {background-color:#e0ffff;}
</style>

<table class="tftable" border="1">
    <tr><th>Name</th><th>Email</th></tr>
    {{#each response}}
        <tr>
            <td>{{name}}</td>
            <td><a href="mailto:{{email}}">{{email}}</a></td>
        </tr>
    {{/each}}
</table>
"#;

#[test]
fn table_of_users_from_an_array() {
    let doc = parse(POSTMAN_TABLE, &users()).unwrap();
    assert!(!doc.needs_browser());
    let table_block = doc.root.find("table").expect("table");
    assert_eq!(table_block.style.width, Len::Percent(100.0));
    assert_eq!(table_block.classes, ["tftable"]);
    let t = table_block.table().unwrap();
    assert_eq!(t.ncols, 2);
    assert_eq!(t.rows.len(), 4);
    assert!(t.collapse);
    assert_eq!(t.spacing, 0.0);

    let header = &t.rows[0];
    assert!(header.cells.iter().all(|c| c.header));
    let th = &header.cells[0].block;
    assert_eq!(th.style.background, Some(rgb(0x87ceeb)));
    assert_eq!(th.style.text_align, TextAlign::Left);
    assert_eq!(th.style.padding.left, Len::Px(8.0));
    assert_eq!(th.style.border.bottom.width, 1.0);
    assert_eq!(th.style.border.bottom.color, Some(rgb(0x87ceeb)));
    let Inline::Text(run) = &th.inlines()[0] else { panic!() };
    assert_eq!(run.text, "Name");
    assert!(run.style.bold);
    assert_eq!(run.style.size, 18.0);
    assert_eq!(run.style.color, Some(rgb(0x333333)));

    // Escaped data comes back as text; whitespace from the template is collapsed.
    let names: Vec<String> = t.rows[1..].iter().map(|r| r.cells[0].block.text()).collect();
    assert_eq!(names, ["Ada Lovelace", "Alan <Turing>", "Grace Hopper"]);
    assert!(t.rows[1..].iter().all(|r| r.background == Some(Color32::WHITE)), "the :hover rule never applies");
    let Inline::Text(email) = &t.rows[1].cells[1].block.inlines()[0] else { panic!() };
    assert_eq!(email.link.as_deref(), Some("mailto:ada@example.com"));
    assert!(email.style.underline);
    assert_eq!(
        doc.text(),
        "Name\tEmail\nAda Lovelace\tada@example.com\nAlan <Turing>\talan@example.com\nGrace Hopper\tgrace@example.com"
    );
}

#[test]
fn list_counters_at_the_integer_limits() {
    let doc = Document::from_html(
        "<ol start=9223372036854775807><li>a</li><li>b</li></ol>\
         <ol reversed><li value=-9223372036854775808>c</li><li>d</li></ol>",
    );
    let outline = doc.outline();
    assert!(outline.contains("li [9223372036854775807.]: \"b\""), "{outline}");
    assert!(outline.contains("li [-9223372036854775808.]: \"d\""), "{outline}");
}

#[test]
fn nested_lists_get_markers() {
    let template = r#"
<ul>
  {{#each categories}}
    <li>{{name}}
      <ol>{{#each items}}<li>{{this}}</li>{{/each}}</ol>
      <ul>{{#each tags}}<li>{{this}}</li>{{/each}}</ul>
    </li>
  {{/each}}
</ul>
<ol start="3" type="i"><li>three</li><li>four</li></ol>"#;
    let data = json!({"categories": [
        {"name": "Fruit", "items": ["apple", "pear"], "tags": ["sweet"]},
        {"name": "Veg", "items": ["leek"], "tags": []}
    ]});
    let doc = parse(template, &data).unwrap();
    assert_eq!(
        doc.outline(),
        "body
  ul
    li [•]
      #anon: \"Fruit\"
      ol
        li [1.]: \"apple\"
        li [2.]: \"pear\"
      ul
        li [◦]: \"sweet\"
    li [•]
      #anon: \"Veg\"
      ol
        li [1.]: \"leek\"
      ul
  ol
    li [iii.]: \"three\"
    li [iv.]: \"four\"
"
    );
}

#[test]
fn styled_header_and_conditional_rows() {
    let template = r#"
<style>
  body { font-family: Arial, sans-serif; background: #fafafa; }
  .header { background: #2c3e50; color: white; padding: 12px 16px; border-radius: 4px; }
  .header h1 { margin: 0; font-size: 20px; }
  .paid td { background: #e8f5e9; }
  .due { background-color: #ffebee; }
  td.amount { text-align: right; font-family: monospace; }
  .hidden { display: none; }
</style>
<div class="header"><h1>{{title}}</h1><small>{{orders.length}} orders</small></div>
<table>
  <thead><tr><th>#</th><th>Customer</th><th>Amount</th></tr></thead>
  <tbody>
  {{#each orders}}
    <tr class="{{#if paid}}paid{{else}}due{{/if}}">
      <td>{{@index}}</td><td>{{customer}}{{#if vip}} <b>VIP</b>{{/if}}</td><td class="amount">{{amount}}</td>
    </tr>
  {{/each}}
  </tbody>
</table>
{{#unless orders}}<p>No orders</p>{{/unless}}
<p class="hidden">secret</p>"#;
    let data = json!({"title": "Orders", "orders": [
        {"customer": "Ada", "amount": "10.00", "paid": true, "vip": true},
        {"customer": "Bob", "amount": "7.50", "paid": false}
    ]});
    let doc = parse(template, &data).unwrap();
    assert_eq!(doc.background, Some(rgb(0xfafafa)));
    let header = doc.root.find("div").unwrap();
    assert_eq!(header.style.background, Some(rgb(0x2c3e50)));
    assert_eq!(header.style.radius, 4.0);
    assert_eq!(header.style.padding.left, Len::Px(16.0));
    let h1 = header.find("h1").unwrap();
    assert_eq!(h1.style.margin.top, Len::Px(0.0));
    let Inline::Text(title) = &h1.inlines()[0] else { panic!() };
    assert_eq!((title.text.as_str(), title.style.size, title.style.color), ("Orders", 20.0, Some(Color32::WHITE)));
    assert_eq!(header.children()[1].text(), "2 orders");

    let t = doc.root.find("table").unwrap().table().unwrap();
    assert_eq!(t.rows.len(), 3);
    assert!(t.rows[0].head);
    let paid = &t.rows[1];
    assert_eq!(paid.cells[0].block.style.background, Some(rgb(0xe8f5e9)));
    assert_eq!(paid.background, None);
    let due = &t.rows[2];
    assert_eq!(due.background, Some(rgb(0xffebee)));
    assert_eq!(due.cells[0].block.style.background, None);
    let amount = &paid.cells[2].block;
    assert_eq!(amount.style.text_align, TextAlign::Right);
    let Inline::Text(a) = &amount.inlines()[0] else { panic!() };
    assert!(a.style.monospace);
    // "Ada VIP": the bold run follows a collapsed space.
    let customer: Vec<(String, bool)> = paid.cells[1]
        .block
        .inlines()
        .iter()
        .map(|i| match i {
            Inline::Text(t) => (t.text.clone(), t.style.bold),
            _ => panic!(),
        })
        .collect();
    assert_eq!(customer, [("Ada ".to_string(), false), ("VIP".to_string(), true)]);
    assert!(!doc.text().contains("No orders"));
    assert!(!doc.text().contains("secret"));
}

#[test]
fn striped_rows_colspan_and_sections() {
    let template = r#"
<style>
  table { border-collapse: separate; border-spacing: 4px; }
  tbody tr:nth-child(even) { background: #f2f2f2 }
  tbody tr:nth-child(odd) td:first-child { font-weight: bold }
  th, td { border: 1px solid #ddd; padding: 4px }
</style>
<table>
  <colgroup><col style="width: 120px"><col></colgroup>
  <caption>Totals</caption>
  <tfoot><tr><td colspan="2">footer</td></tr></tfoot>
  <thead><tr><th>Key</th><th>Value</th></tr></thead>
  <tbody>
    {{#each rows}}<tr><td>{{k}}</td><td>{{v}}</td></tr>{{/each}}
  </tbody>
</table>"#;
    let data = json!({"rows": [{"k": "a", "v": 1}, {"k": "b", "v": 2}, {"k": "c", "v": 3}]});
    let doc = parse(template, &data).unwrap();
    let t = doc.root.find("table").unwrap().table().unwrap();
    assert!(!t.collapse);
    assert_eq!(t.spacing, 4.0);
    assert_eq!(t.columns, [Len::Px(120.0), Len::Auto]);
    assert_eq!(t.caption.as_ref().unwrap().text(), "Totals");
    let order: Vec<String> = t.rows.iter().map(|r| r.cells[0].block.text()).collect();
    assert_eq!(order, ["Key", "a", "b", "c", "footer"], "thead first, tfoot last");
    let stripes: Vec<Option<Color32>> = t.rows[1..4].iter().map(|r| r.background).collect();
    assert_eq!(stripes, [None, Some(rgb(0xf2f2f2)), None]);
    let bold: Vec<bool> = t.rows[1..4]
        .iter()
        .map(|r| matches!(&r.cells[0].block.inlines()[0], Inline::Text(t) if t.style.bold))
        .collect();
    assert_eq!(bold, [true, false, true]);
    let foot = &t.rows[4].cells[0];
    assert_eq!((foot.colspan, foot.col), (2, 0));
    assert_eq!(t.rows[1].cells[0].block.style.border.left.color, Some(rgb(0xdddddd)));
}

#[test]
fn rowspan_moves_cells_to_free_columns() {
    let doc = Document::from_html(
        "<table><tr><td rowspan=2>a</td><td>b</td><td>c</td></tr><tr><td>d</td><td>e</td></tr>\
         <tr><td colspan=3>f</td></tr></table>",
    );
    let t = doc.root.find("table").unwrap().table().unwrap();
    let cols: Vec<Vec<usize>> = t.rows.iter().map(|r| r.cells.iter().map(|c| c.col).collect()).collect();
    assert_eq!(cols, [vec![0, 1, 2], vec![1, 2], vec![0]]);
    assert_eq!(t.ncols, 3);
}

#[test]
fn chart_js_template_needs_a_browser() {
    let template = r#"
<canvas id="myChart" height="75"></canvas>
<script src="https://cdnjs.cloudflare.com/ajax/libs/Chart.js/2.5.0/Chart.min.js"></script>
<script>
    var ctx = document.getElementById("myChart");
    pm.getData(function (err, value) { new Chart(ctx, { type: "bar", data: value }); });
</script>"#;
    let doc = parse(template, &json!({"labels": ["a"]})).unwrap();
    assert!(doc.uses_scripts);
    assert_eq!(doc.external_resources, ["https://cdnjs.cloudflare.com/ajax/libs/Chart.js/2.5.0/Chart.min.js"]);
    assert_eq!(doc.unsupported, ["canvas"]);
    assert!(doc.needs_browser());
    assert!(doc.is_blank());
    assert!(doc.text().is_empty(), "script text is not shown");
}

#[test]
fn external_stylesheets_and_titles() {
    let doc = Document::from_html(
        "<html><head><title> My  report </title><link rel='stylesheet' href='https://cdn.example/bootstrap.css'>\
         <style>@import url(\"https://fonts.example/f.css\"); p { color: teal }</style></head>\
         <body><p>x</p><svg><circle r=4 /></svg><input value='typed'></body></html>",
    );
    assert_eq!(doc.title.as_deref(), Some("My report"));
    assert_eq!(doc.external_resources, ["https://cdn.example/bootstrap.css", "https://fonts.example/f.css"]);
    assert_eq!(doc.unsupported, ["svg", "input"]);
    assert!(!doc.uses_scripts);
    assert!(doc.needs_browser());
    assert!(doc.text().contains("typed"));
}

#[test]
fn whitespace_inline_styles_and_breaks() {
    let doc = Document::from_html(
        "<p>  Hello   <b>world</b> !\n  <i>it<em>alic</em></i> <u>u</u><s>s</s> <small>sm</small><br>  next <code>x = 1</code></p>",
    );
    assert_eq!(
        doc.outline(),
        "body
  p: \"Hello \"\"world\"<b>\" ! \"\"italic\"<i>\" \"\"u\"<u>\"s\"<s>\" \"\"sm\"<br>\"next \"\"x = 1\"<m>
"
    );
    let p = doc.root.find("p").unwrap();
    let Inline::Text(small) = &p.inlines()[8] else { panic!("{:?}", p.inlines()) };
    assert!((small.style.size - 16.0 / 1.2).abs() < 0.01);
}

#[test]
fn pre_keeps_formatting_of_json() {
    let data = json!({"json": "{\n  \"id\": 1,\n  \"tags\": [\"<a>\"]\n}"});
    let doc = parse("<h3>Raw</h3><pre>{{json}}</pre><p>after</p>", &data).unwrap();
    let pre = doc.root.find("pre").unwrap();
    assert!(!pre.style.wrap);
    let Inline::Text(t) = &pre.inlines()[0] else { panic!() };
    assert_eq!(t.text, "{\n  \"id\": 1,\n  \"tags\": [\"<a>\"]\n}");
    assert!(t.style.monospace);
    let h3 = doc.root.find("h3").unwrap();
    assert_eq!(h3.style.font_size, 16.0 * 1.17);
}

#[test]
fn links_and_images() {
    // 1x1 transparent PNG.
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    let html = format!(
        "<p><a href='https://example.com/docs'>Docs <b>here</b></a> <a href='javascript:alert(1)'>bad</a></p>\
         <p><img src='data:image/png;base64,{png}' alt='dot' width=10 height=10>\
         <a href='https://example.com'><img src='https://example.com/logo.png' alt='logo'></a></p>\
         <a href='https://example.com/block'><div>Block link</div></a>"
    );
    let doc = Document::from_html(&html);
    let p = &doc.root.children()[0];
    let links: Vec<Option<&str>> = p
        .inlines()
        .iter()
        .map(|i| match i {
            Inline::Text(t) => t.link.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(links, [Some("https://example.com/docs"), Some("https://example.com/docs"), None, None]);
    let imgs: Vec<&merpati_html::ImageRef> = doc.root.children()[1]
        .inlines()
        .iter()
        .filter_map(|i| match i {
            Inline::Image(img) => Some(img),
            _ => None,
        })
        .collect();
    assert_eq!(imgs.len(), 2);
    assert_eq!(imgs[0].mime.as_deref(), Some("image/png"));
    assert_eq!(imgs[0].data.as_ref().map(|d| d.len()), Some(68));
    assert_eq!((imgs[0].width, imgs[0].height), (Some(10.0), Some(10.0)));
    assert!(imgs[1].data.is_none());
    assert_eq!(imgs[1].link.as_deref(), Some("https://example.com"));
    let block_link = doc.root.find("div").unwrap();
    let Inline::Text(t) = &block_link.inlines()[0] else { panic!() };
    assert_eq!(t.link.as_deref(), Some("https://example.com/block"));
}

#[test]
fn blockquote_hr_and_rows() {
    let doc = Document::from_html(
        "<blockquote>quoted</blockquote><hr>\
         <div style='display:flex; gap: 8px'><div class=card>one</div><div class=card>two</div></div>\
         <div><div style='display:inline-block'>a</div><div style='display:inline-block'><p>b</p></div></div>\
         <span style='display:inline-block; background:#eee; padding: 4px'>badge</span>",
    );
    let bq = doc.root.find("blockquote").unwrap();
    assert_eq!(bq.style.margin.left, Len::Px(40.0));
    assert!(matches!(doc.root.find("hr").unwrap().content, Content::Rule));
    let flex = &doc.root.children()[2];
    let Content::Row { blocks, gap, wrap, flex, .. } = &flex.content else { panic!("{}", doc.outline()) };
    assert_eq!((blocks.len(), *gap, *wrap, *flex), (2, 8.0, false, true));
    // inline-block with block content -> row item; the text-only one stays inline.
    let mixed = &doc.root.children()[3];
    assert_eq!(mixed.children().len(), 2, "{}", doc.outline());
    assert!(matches!(mixed.children()[1].content, Content::Row { .. }));
    let badge = doc.root.children().last().unwrap();
    let Inline::Text(t) = &badge.inlines()[0] else { panic!("{}", doc.outline()) };
    assert_eq!(t.text, "\u{a0}badge\u{a0}", "padding approximated by spaces");
    assert_eq!(t.style.background, Some(Color32::from_gray(0xee)));
}

#[test]
fn template_errors_are_reported() {
    let err = parse("<ul>{{#each items}}<li>{{name}}</li></ul>", &json!({})).unwrap_err();
    assert!(err.to_string().starts_with("template error"), "{err}");
}

#[test]
fn large_tables_parse_quickly() {
    let rows: Vec<serde_json::Value> =
        (0..5000).map(|i| json!({"id": i, "name": format!("user {i}"), "email": format!("u{i}@x.test")})).collect();
    let template = "<style>tr:nth-child(even){background:#eee} td{padding:2px 6px}</style><table>\
                    {{#each rows}}<tr><td>{{id}}</td><td>{{name}}</td><td>{{email}}</td></tr>{{/each}}</table>";
    let start = std::time::Instant::now();
    let doc = parse(template, &json!({ "rows": rows })).unwrap();
    let elapsed = start.elapsed();
    let t = doc.root.find("table").unwrap().table().unwrap();
    assert_eq!(t.rows.len(), 5000);
    assert_eq!(t.rows[1].background, Some(Color32::from_gray(0xee)));
    eprintln!("5000-row table parsed in {elapsed:?}");
}
