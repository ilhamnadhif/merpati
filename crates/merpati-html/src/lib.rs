//! Native renderer for Postman visualizations (FR-P4).
//!
//! `pm.visualizer.set(template, data)` gives a Handlebars template and its
//! data. [`parse`] renders the template (Handlebars.js semantics), parses the
//! resulting HTML with html5ever and lays it out with a CSS subset into a
//! [`Document`], which [`Document::show`] draws with egui:
//!
//! * elements: headings, paragraphs, `div`/`span`, `b`/`strong`, `i`/`em`,
//!   `u`, `s`, `small`, `code`, `pre`, `br`, `hr`, lists, tables (`thead`,
//!   `tbody`, `tfoot`, `colspan`, `rowspan`, `<col>` widths), links, images
//!   (`data:` URIs; remote images only through [`Host::remote_image`]),
//!   `blockquote`;
//! * CSS: `<style>` blocks and `style` attributes with type, class, id,
//!   attribute, descendant/child/sibling selectors and structural
//!   pseudo-classes (`:nth-child(even)` striping), and the properties
//!   `color`, `background(-color)`, `font-*`, `text-align`, `text-decoration`,
//!   `padding`, `margin`, `border*` (incl. `border-collapse`,
//!   `border-radius`), `width`, `height`, `min-/max-width/height`,
//!   `box-sizing`, `calc()`, `min()`, `max()` and `clamp()` of lengths and
//!   percentages, `overflow`, `display`, `white-space`, `vertical-align`,
//!   `list-style`; `@media` width queries against the viewport width
//!   ([`Document::from_html_at`], [`Document::restyle_needed`]);
//! * layout: block flow, `inline-block` (sized boxes such as badges and
//!   swatches stay in their line), flexbox (`flex-grow/-shrink/-basis`,
//!   `justify-content`, `align-items/-self`, `gap`, wrapping, columns), CSS
//!   grid (`grid-template-columns/rows` with `fr`, `minmax()`,
//!   `repeat(auto-fill | auto-fit, ...)`, areas, line numbers and spans,
//!   auto-placement, `justify-/align-items/-self`), floats with `clear` and
//!   float containment (`overflow`, `display: flow-root`, clearfix), and
//!   `position: relative | absolute | fixed | sticky` with `z-index` paint
//!   order. See the `render` module for the simplifications.
//!
//! Scripts are never executed: [`Document::uses_scripts`],
//! [`Document::external_resources`] and [`Document::unsupported`] tell the
//! app when to offer "Open in browser" for full fidelity
//! ([`Document::needs_browser`]).
//!
//! ```no_run
//! # fn ui(ui: &mut egui::Ui) {
//! let data = serde_json::json!({"users": [{"name": "Ada"}]});
//! let doc = merpati_html::parse("<ul>{{#each users}}<li>{{name}}</li>{{/each}}</ul>", &data).unwrap();
//! doc.show(ui, &mut |href: &str| println!("open {href}"));
//! # }
//! ```

mod css;
mod document;
mod dom;
mod grid;
mod layout;
mod render;
mod style;
mod template;

pub use document::{
    Block, BorderSide, BorderStyle, BoxStyle, Breadth, Cell, Clear, Content, Document, Float, Grid, GridLine,
    GridPlacement, GridTemplate, ImageRef, Inline, ItemAlign, Justify, Len, Overflow, Position, Repeat, Row, Sides,
    Table, TextAlign, TextRun, TextStyle, TrackSize, Tracks, VAlign,
};
pub use render::{Host, Options, Paper};
pub use template::render_template;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The Handlebars template could not be compiled or rendered.
    #[error("template error{}: {message}", position(*line, *column))]
    Template { message: String, line: Option<usize>, column: Option<usize> },
}

fn position(line: Option<usize>, column: Option<usize>) -> String {
    match (line, column) {
        (Some(l), Some(c)) => format!(" at line {l}, column {c}"),
        (Some(l), None) => format!(" at line {l}"),
        _ => String::new(),
    }
}

/// Renders a visualizer template with its data and lays out the HTML.
pub fn parse(template: &str, data: &serde_json::Value) -> Result<Document, Error> {
    let html = render_template(template, data)?;
    Ok(Document::from_html(&html))
}

/// A standalone page for "Open in browser": the rendered template plus a
/// `pm.getData(callback)` shim with the data, so scripted visualizations
/// (Chart.js, D3, ...) work without Postman.
pub fn browser_html(template: &str, data: &serde_json::Value) -> Result<String, Error> {
    let html = render_template(template, data)?;
    // `</script>` (and `<!--`) inside the JSON must not end the script element.
    let json =
        serde_json::to_string(data).unwrap_or_else(|_| "null".into()).replace("</", "<\\/").replace("<!--", "<\\!--");
    Ok(format!(
        "<!DOCTYPE html><meta charset=\"utf-8\"><script>window.pm = {{ getData: function (cb) {{ cb(null, {json}); }} }};</script>\n{html}"
    ))
}

impl Document {
    /// Lays out an HTML document (no templating). Never fails: like a
    /// browser, the parser recovers from any markup.
    pub fn from_html(html: &str) -> Document {
        layout::build(html.to_string(), None)
    }

    /// Like [`Document::from_html`] for a viewport `css_width` CSS px wide,
    /// so that `@media` width queries apply.
    pub fn from_html_at(html: &str, css_width: f32) -> Document {
        layout::build(html.to_string(), Some(css_width))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn browser_page_has_get_data_shim() {
        let page =
            super::browser_html("<p>{{x}}</p><script>pm.getData(f)</script>", &json!({"x": "</script>"})).unwrap();
        assert!(page.starts_with("<!DOCTYPE html>"));
        assert!(page.contains(r#"cb(null, {"x":"<\/script>"})"#), "{page}");
        assert!(page.ends_with("<p>&lt;/script&gt;</p><script>pm.getData(f)</script>"));
        // The shim does not end the script element early.
        assert_eq!(page.matches("</script>").count(), 2);
    }
}
