//! Response body viewer (FR-P1, FR-P2).
//!
//! Small bodies are shown in a read-only, highlighted, selectable editor.
//! Large bodies are shown through a virtual row viewer that only reads and
//! lays out the visible rows; for bodies kept in a temporary file the row
//! index is built on a background thread, so a 100 MB response never blocks
//! the UI or loads fully into memory.

use std::sync::Arc;

use egui::{Align2, Color32, CornerRadius, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use parking_lot::Mutex;

use merpati_core::response::{BodyLanguage, HttpResponse, ResponseBody};

use crate::highlight::{self, HlKey, Lang};
use crate::i18n::{tr, trf};
use crate::{icons, kit, theme};

/// Rows longer than this are split for display.
const MAX_ROW_BYTES: usize = 2048;
/// Displayed text up to this size uses the selectable editor.
const EDITOR_LIMIT: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyView {
    Pretty,
    Raw,
    Preview,
    Tree,
}

/// Start offsets of display rows.
#[derive(Default)]
pub struct LineIndex {
    pub starts: Vec<u64>,
    pub total: u64,
}

impl LineIndex {
    pub fn from_bytes(b: &[u8]) -> LineIndex {
        let mut starts = vec![0u64];
        let mut row_start = 0usize;
        for (i, &c) in b.iter().enumerate() {
            if c == b'\n' {
                starts.push(i as u64 + 1);
                row_start = i + 1;
            } else if i - row_start >= MAX_ROW_BYTES && (c & 0xC0) != 0x80 {
                starts.push(i as u64);
                row_start = i;
            }
        }
        LineIndex { starts, total: b.len() as u64 }
    }

    pub fn from_file(path: &std::path::Path) -> std::io::Result<LineIndex> {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut starts = vec![0u64];
        let mut pos = 0u64;
        let mut row_start = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            for (i, &c) in buf[..n].iter().enumerate() {
                let at = pos + i as u64;
                if c == b'\n' {
                    starts.push(at + 1);
                    row_start = at + 1;
                } else if at - row_start >= MAX_ROW_BYTES as u64 && (c & 0xC0) != 0x80 {
                    starts.push(at);
                    row_start = at;
                }
            }
            pos += n as u64;
        }
        if starts.last() == Some(&pos) && pos > 0 {
            starts.pop();
        }
        Ok(LineIndex { starts, total: pos })
    }

    pub fn rows(&self) -> usize {
        self.starts.len()
    }

    pub fn range(&self, row: usize) -> (u64, u64) {
        let s = self.starts[row];
        let e = self.starts.get(row + 1).copied().unwrap_or(self.total);
        (s, e)
    }
}

enum Source {
    Text(Arc<String>),
    File(ResponseBody),
}

struct Prepared {
    view: BodyView,
    lang: Lang,
    source: Option<Source>,
    index: Arc<Mutex<Option<Arc<LineIndex>>>>,
    json: Option<Arc<serde_json::Value>>,
    note: Option<String>,
    is_image: bool,
    is_html: bool,
    is_binary: bool,
    /// Pixel size of an image body.
    image_size: Option<[u32; 2]>,
}

pub struct BodyState {
    pub view: BodyView,
    pub search: String,
    pub wrap: bool,
    prepared: Option<Prepared>,
    /// Row to scroll to (search navigation).
    jump: Option<usize>,
    match_cursor: usize,
    /// Tree view: nodes opened or closed by the user (by JSON path).
    tree_open: std::collections::HashMap<String, bool>,
    /// Tree view: "Expand all" / "Collapse all" override the default.
    tree_all: Option<bool>,
}

impl Default for BodyState {
    fn default() -> Self {
        BodyState {
            view: BodyView::Pretty,
            search: String::new(),
            wrap: false,
            prepared: None,
            jump: None,
            match_cursor: 0,
            tree_open: Default::default(),
            tree_all: None,
        }
    }
}

impl BodyState {
    pub fn reset(&mut self) {
        self.prepared = None;
        self.jump = None;
        self.match_cursor = 0;
    }
}

fn prepare(resp: &HttpResponse, view: BodyView, max_pretty: u64, ctx: &egui::Context) -> Prepared {
    let language = resp.language();
    let lang = Lang::from_language(language);
    let len = resp.body.len();
    let index = Arc::new(Mutex::new(None));
    let mut p = Prepared {
        view,
        lang,
        source: None,
        index: index.clone(),
        json: None,
        note: None,
        is_image: language == BodyLanguage::Image,
        is_html: language == BodyLanguage::Html,
        is_binary: language == BodyLanguage::Binary,
        image_size: None,
    };
    if language == BodyLanguage::Binary && view != BodyView::Raw {
        return p;
    }
    if language == BodyLanguage::Image && view == BodyView::Preview && !resp.body.is_file() {
        let bytes = resp.body.read_all().unwrap_or_default();
        p.image_size = image::ImageReader::new(std::io::Cursor::new(&bytes[..]))
            .with_guessed_format()
            .ok()
            .and_then(|r| r.into_dimensions().ok())
            .map(|(w, h)| [w, h]);
        return p;
    }
    if view == BodyView::Tree && language != BodyLanguage::Json {
        p.note = Some(tr("The tree view shows JSON bodies; this body is shown formatted instead.").to_string());
    }
    let small = len <= max_pretty && !resp.body.is_file();
    match (view, small) {
        (BodyView::Pretty | BodyView::Tree | BodyView::Preview, true) => {
            let bytes = resp.body.read_all().unwrap_or_default();
            if view == BodyView::Tree && language == BodyLanguage::Json {
                p.json = serde_json::from_slice(&bytes).ok().map(Arc::new);
                if p.json.is_none() {
                    p.note = Some(tr("The body is not valid JSON, so it cannot be shown as a tree.").to_string());
                }
                return p;
            }
            let text = match language {
                BodyLanguage::Json => {
                    highlight::pretty_json(&bytes).unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned())
                }
                BodyLanguage::Xml | BodyLanguage::Html => highlight::pretty_xml(&String::from_utf8_lossy(&bytes)),
                _ => String::from_utf8_lossy(&bytes).into_owned(),
            };
            set_text(&mut p, text);
        }
        (_, true) => {
            let bytes = resp.body.read_all().unwrap_or_default();
            set_text(&mut p, String::from_utf8_lossy(&bytes).into_owned());
        }
        (_, false) => {
            if view != BodyView::Raw {
                p.note = Some(trf(
                    "The body is {}; showing the raw view. Pretty printing is limited to {} (Settings).",
                    &[&merpati_core::util::human_bytes(len), &merpati_core::util::human_bytes(max_pretty)],
                ));
            }
            p.lang = if len > 20 * 1024 * 1024 { Lang::Text } else { lang };
            match &resp.body {
                ResponseBody::File { path, .. } => {
                    p.source = Some(Source::File(resp.body.clone()));
                    let path = path.to_path_buf();
                    let ctx = ctx.clone();
                    std::thread::spawn(move || {
                        if let Ok(ix) = LineIndex::from_file(&path) {
                            *index.lock() = Some(Arc::new(ix));
                        }
                        ctx.request_repaint();
                    });
                }
                ResponseBody::Memory(b) => {
                    let text = String::from_utf8_lossy(b).into_owned();
                    *index.lock() = Some(Arc::new(LineIndex::from_bytes(text.as_bytes())));
                    p.source = Some(Source::Text(Arc::new(text)));
                }
            }
        }
    }
    p
}

fn set_text(p: &mut Prepared, text: String) {
    if text.len() > EDITOR_LIMIT {
        *p.index.lock() = Some(Arc::new(LineIndex::from_bytes(text.as_bytes())));
    }
    p.source = Some(Source::Text(Arc::new(text)));
}

/// Toolbar + body. `max_pretty` comes from the settings.
pub fn show(ui: &mut Ui, state: &mut BodyState, resp: &HttpResponse, max_pretty: u64) {
    let dark = ui.visuals().dark_mode;
    ui.horizontal(|ui| {
        use crate::{icons, kit};
        let mut view = state.view;
        kit::segmented(
            ui,
            &mut view,
            &[
                (BodyView::Pretty, tr("Pretty")),
                (BodyView::Raw, tr("Raw")),
                (BodyView::Preview, tr("Preview")),
                (BodyView::Tree, tr("Tree")),
            ],
        );
        if view != state.view {
            state.view = view;
            state.reset();
        }
        ui.add_space(4.0);
        let r = kit::search_field(ui, "body-search", &mut state.search, tr("Search"), 200.0);
        if r.changed() {
            state.match_cursor = 0;
        }
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (kit::icon_button(ui, icons::ARROW_DOWN, tr("Next match")).clicked() || enter) && !state.search.is_empty() {
            state.match_cursor += 1;
            state.jump = Some(usize::MAX);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if kit::icon_button(ui, icons::DOWNLOAD_SIMPLE, tr("Save response to file")).clicked()
                && let Some(path) = rfd::FileDialog::new().set_file_name(default_file_name(resp)).save_file()
                && let Err(e) = resp.body.save_to(&path)
            {
                tracing::error!("save body: {e}");
            }
            if kit::icon_button(ui, icons::COPY, tr("Copy")).clicked() && resp.body.len() <= 50 * 1024 * 1024 {
                let text = match &state.prepared {
                    Some(Prepared { source: Some(Source::Text(t)), .. }) => t.as_str().to_string(),
                    _ => resp.body.text_lossy(),
                };
                ui.ctx().copy_text(text);
            }
            if state.view == BodyView::Tree {
                if kit::icon_button(ui, icons::ARROWS_IN, tr("Collapse all")).clicked() {
                    state.tree_all = Some(false);
                    state.tree_open.clear();
                }
                if kit::icon_button(ui, icons::ARROWS_OUT, tr("Expand all")).clicked() {
                    state.tree_all = Some(true);
                    state.tree_open.clear();
                }
            } else if kit::icon_toggle(ui, icons::TEXT_ALIGN_LEFT, tr("Wrap"), state.wrap).clicked() {
                state.wrap = !state.wrap;
            }
        });
    });
    ui.add_space(6.0);
    if state.prepared.as_ref().map(|p| p.view != state.view).unwrap_or(true) {
        state.prepared = Some(prepare(resp, state.view, max_pretty, ui.ctx()));
    }
    let jump = state.jump.take();
    let match_cursor = state.match_cursor;
    let p = state.prepared.as_ref().unwrap();
    let pal = theme::pal(ui);
    if let Some(note) = &p.note {
        crate::request_tab::banner(ui, icons::INFO, pal.info, note);
        ui.add_space(8.0);
    }
    if p.is_binary && state.view != BodyView::Raw {
        binary_state(ui, resp);
        return;
    }
    if state.view == BodyView::Preview {
        if p.is_image {
            image_preview(ui, resp, p.image_size);
            return;
        }
        if p.is_html {
            let mut open = false;
            crate::request_tab::banner_with(
                ui,
                icons::GLOBE,
                pal.info,
                tr("HTML preview opens in your browser."),
                |ui| open = kit::ghost(ui, Some(icons::ARROW_SQUARE_OUT), tr("Open in browser")).clicked(),
            );
            if open {
                let path = std::env::temp_dir().join(format!("merpati-preview-{}.html", std::process::id()));
                if resp.body.save_to(&path).is_ok() {
                    let _ = open::that(&path);
                }
            }
            ui.add_space(8.0);
        }
    }
    if let Some(json) = p.json.clone() {
        let search = state.search.clone();
        json_tree_view(ui, state, &json, &search);
        return;
    }
    let search = state.search.clone();
    let index = p.index.lock().clone();
    let lang = p.lang;
    match (&p.source, index) {
        (None, _) => {}
        (Some(Source::Text(text)), None) => {
            let font_size = ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5);
            let wrap = state.wrap;
            let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
                let mut job = highlight::cached(
                    ui.ctx(),
                    HlKey { text: buf.as_str(), lang, dark, search: &search, vars: None, font_size, wrap },
                );
                job.wrap.max_width = if wrap { wrap_width } else { f32::INFINITY };
                ui.fonts_mut(|f| f.layout_job(job))
            };
            let mut s: &str = text.as_str();
            let jump_to_match = jump.is_some();
            egui::ScrollArea::both().auto_shrink([false; 2]).id_salt("body-scroll").show(ui, |ui| {
                let out = egui::TextEdit::multiline(&mut s)
                    .code_editor()
                    .desired_width(if wrap { ui.available_width() } else { f32::INFINITY })
                    .layouter(&mut layouter)
                    .show(ui);
                if jump_to_match
                    && !search.is_empty()
                    && let Some(chars) = nth_match_char(text, &search, match_cursor)
                {
                    let rect = out.galley.pos_from_cursor(egui::text::CCursor::new(chars));
                    ui.scroll_to_rect(rect.translate(out.galley_pos.to_vec2()), Some(egui::Align::Center));
                }
            });
        }
        (Some(source), Some(index)) => {
            rows_viewer(ui, jump, match_cursor, source, &index, lang, &search, dark);
        }
        (Some(Source::File(_)), None) => {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.add(egui::Spinner::new().size(20.0).color(pal.accent));
                ui.add_space(6.0);
                ui.label(RichText::new(tr("Indexing large response…")).color(pal.text_muted));
            });
        }
    }
}

/// Cache key of an image preview (egui keeps loaded images by URI).
fn image_uri(url: &str, bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    bytes.hash(&mut h);
    format!("bytes://response-{:016x}.img", h.finish())
}

fn default_file_name(resp: &HttpResponse) -> String {
    let ext = match resp.language() {
        BodyLanguage::Json => "json",
        BodyLanguage::Xml => "xml",
        BodyLanguage::Html => "html",
        BodyLanguage::JavaScript => "js",
        BodyLanguage::Css => "css",
        BodyLanguage::Text => "txt",
        _ => "bin",
    };
    format!("response.{ext}")
}

fn read_rows(source: &Source, index: &LineIndex, rows: std::ops::Range<usize>) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let start = index.range(rows.start).0;
    let end = index.range(rows.end - 1).1;
    let bytes: Vec<u8> = match source {
        Source::Text(t) => t.as_bytes()[start as usize..end as usize].to_vec(),
        Source::File(body) => body.read_range(start, (end - start) as usize).unwrap_or_default(),
    };
    rows.map(|r| {
        let (s, e) = index.range(r);
        let (s, e) = ((s - start) as usize, ((e - start) as usize).min(bytes.len()));
        let slice = &bytes[s.min(e)..e];
        let slice = slice.strip_suffix(b"\n").unwrap_or(slice);
        let slice = slice.strip_suffix(b"\r").unwrap_or(slice);
        String::from_utf8_lossy(slice).into_owned()
    })
    .collect()
}

#[allow(clippy::too_many_arguments)]
fn rows_viewer(
    ui: &mut Ui,
    jump: Option<usize>,
    match_cursor: usize,
    source: &Source,
    index: &LineIndex,
    lang: Lang,
    search: &str,
    dark: bool,
) {
    let font_size = ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5);
    let row_h = ui.fonts_mut(|f| f.row_height(&egui::FontId::monospace(font_size))) + 1.0;
    let rows = index.rows();
    let pal = theme::pal(ui);
    ui.label(
        RichText::new(trf("{} rows · {}", &[&rows, &merpati_core::util::human_bytes(index.total)]))
            .small()
            .color(pal.text_muted),
    );
    let mut area = egui::ScrollArea::both().auto_shrink([false; 2]).id_salt("rows-scroll");
    if let Some(target) = jump {
        let row = if target == usize::MAX { find_match_row(source, index, search, match_cursor) } else { Some(target) };
        if let Some(r) = row {
            area = area.vertical_scroll_offset((r as f32 * row_h - 80.0).max(0.0));
        }
    }
    let gutter = format!("{}", rows).len();
    area.show_rows(ui, row_h, rows, |ui, range| {
        let texts = read_rows(source, index, range.clone());
        for (row, text) in range.zip(texts) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(
                    RichText::new(format!("{:>gutter$}", row + 1)).monospace().size(font_size).color(pal.text_faint),
                );
                let job =
                    highlight::highlight(HlKey { text: &text, lang, dark, search, vars: None, font_size, wrap: false });
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
            });
        }
    });
}

/// Char index of the n-th (1-based, wrapping) case-insensitive match.
fn nth_match_char(text: &str, search: &str, nth: usize) -> Option<usize> {
    // Lowercase char by char, remembering which original char each byte of
    // the lowercased text came from.
    let mut lower = String::with_capacity(text.len());
    let mut origin: Vec<u32> = Vec::with_capacity(text.len());
    for (ci, c) in text.chars().enumerate() {
        for l in c.to_lowercase() {
            lower.push(l);
            origin.extend(std::iter::repeat_n(ci as u32, l.len_utf8()));
        }
    }
    let needle: String = search.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return None;
    }
    let matches: Vec<usize> = lower.match_indices(&needle).map(|(i, _)| i).collect();
    if matches.is_empty() {
        return None;
    }
    let byte = matches[(nth.max(1) - 1) % matches.len()];
    origin.get(byte).map(|&c| c as usize)
}

/// Row of the n-th search match (wrapping), scanning the body in chunks.
fn find_match_row(source: &Source, index: &LineIndex, search: &str, nth: usize) -> Option<usize> {
    if search.is_empty() {
        return None;
    }
    let needle = search.to_lowercase();
    let mut found = Vec::new();
    let chunk_rows = 4096;
    let mut row = 0;
    while row < index.rows() && found.len() < nth.max(1) + 1 {
        let end = (row + chunk_rows).min(index.rows());
        for (i, text) in read_rows(source, index, row..end).into_iter().enumerate() {
            if text.to_lowercase().contains(&needle) {
                found.push(row + i);
            }
        }
        row = end;
        if found.len() >= 100_000 {
            break;
        }
    }
    if found.is_empty() {
        return None;
    }
    Some(found[(nth.max(1) - 1) % found.len()])
}

/// A binary body: what it is and how to get it.
fn binary_state(ui: &mut Ui, resp: &HttpResponse) {
    let size = merpati_core::util::human_bytes(resp.body.len());
    let ct = resp.content_type().to_string();
    let body = if ct.is_empty() { size } else { format!("{ct} · {size}") };
    kit::empty_state(ui, icons::FILE, tr("Binary response"), &body, |ui| {
        if kit::secondary(ui, Some(icons::DOWNLOAD_SIMPLE), tr("Save response to file")).clicked()
            && let Some(path) = rfd::FileDialog::new().set_file_name(default_file_name(resp)).save_file()
            && let Err(e) = resp.body.save_to(&path)
        {
            tracing::error!("save body: {e}");
        }
    });
}

/// An image body on a checkerboard (shows transparency), with its size.
fn image_preview(ui: &mut Ui, resp: &HttpResponse, size: Option<[u32; 2]>) {
    let p = theme::pal(ui);
    let bytes = resp.body.read_all().unwrap_or_default();
    let uri = image_uri(&resp.url, &bytes);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let ct = resp.content_type().split(';').next().unwrap_or("").trim().to_string();
        if !ct.is_empty() {
            kit::pill(ui, &ct, p.text_muted);
        }
        if let Some([w, h]) = size {
            kit::metric(ui, icons::ARROWS_OUT, &format!("{w} × {h} px"));
        }
        kit::metric(ui, icons::DOWNLOAD_SIMPLE, &merpati_core::util::human_bytes(bytes.len() as u64));
    });
    ui.add_space(8.0);
    egui::ScrollArea::both().auto_shrink([false; 2]).id_salt("image-preview").show(ui, |ui| {
        let avail = ui.available_width();
        let (img_w, img_h) = match size {
            Some([w, h]) => {
                let scale = ((avail - 32.0) / w as f32).min(1.0);
                (w as f32 * scale, h as f32 * scale)
            }
            None => (avail - 32.0, 240.0),
        };
        let frame = vec2(avail, img_h + 32.0);
        let (rect, _) = ui.allocate_exact_size(frame, Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(8), p.chrome);
        // Checkerboard behind the picture.
        let img_rect = Rect::from_center_size(rect.center(), vec2(img_w, img_h));
        let cell = 10.0;
        let dark_cell = if ui.visuals().dark_mode { Color32::from_gray(44) } else { Color32::from_gray(232) };
        let light_cell = if ui.visuals().dark_mode { Color32::from_gray(36) } else { Color32::WHITE };
        painter.rect_filled(img_rect, 0.0, light_cell);
        let cols = (img_w / cell).ceil() as i32;
        let rows = (img_h / cell).ceil() as i32;
        if cols * rows < 40_000 {
            for r in 0..rows {
                for c in 0..cols {
                    if (r + c) % 2 == 0 {
                        let min = img_rect.min + vec2(c as f32 * cell, r as f32 * cell);
                        let cr = Rect::from_min_size(min, vec2(cell, cell)).intersect(img_rect);
                        painter.rect_filled(cr, 0.0, dark_cell);
                    }
                }
            }
        }
        egui::Image::from_bytes(uri, bytes.to_vec()).paint_at(ui, img_rect);
        ui.painter().rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    });
}

/// One visible row of the JSON tree.
struct TreeRow<'a> {
    depth: usize,
    /// Object key or array index.
    key: Option<String>,
    is_index: bool,
    value: &'a serde_json::Value,
    path: String,
    open: bool,
    /// "… n more" row after the first 1000 children.
    more: Option<usize>,
}

fn flatten_tree<'a>(
    state: &BodyState,
    v: &'a serde_json::Value,
    key: Option<String>,
    is_index: bool,
    path: String,
    depth: usize,
    out: &mut Vec<TreeRow<'a>>,
) {
    let container = matches!(v, serde_json::Value::Object(_) | serde_json::Value::Array(_));
    let open = container && state.tree_open.get(&path).copied().unwrap_or(state.tree_all.unwrap_or(depth < 2));
    out.push(TreeRow { depth, key, is_index, value: v, path: path.clone(), open, more: None });
    if !open {
        return;
    }
    let more_row = |n: usize, depth: usize| TreeRow {
        depth,
        key: None,
        is_index: false,
        value: &serde_json::Value::Null,
        path: String::new(),
        open: false,
        more: Some(n),
    };
    match v {
        serde_json::Value::Object(map) => {
            for (i, (k, child)) in map.iter().enumerate() {
                if i >= 1000 {
                    out.push(more_row(map.len() - 1000, depth + 1));
                    break;
                }
                let child_path = if k.chars().all(|c| c.is_alphanumeric() || c == '_') && !k.is_empty() {
                    format!("{path}.{k}")
                } else {
                    format!("{path}[{}]", serde_json::Value::String(k.clone()))
                };
                flatten_tree(state, child, Some(k.clone()), false, child_path, depth + 1, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                if i >= 1000 {
                    out.push(more_row(items.len() - 1000, depth + 1));
                    break;
                }
                flatten_tree(state, child, Some(i.to_string()), true, format!("{path}[{i}]"), depth + 1, out);
            }
        }
        _ => {}
    }
}

/// The JSON body as a tree: carets, indent guides, type-colored values,
/// counts on containers; click a container to fold it, right-click to copy.
fn json_tree_view(ui: &mut Ui, state: &mut BodyState, json: &serde_json::Value, search: &str) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let colors = highlight::json_colors(dark);
    let mut rows = Vec::new();
    flatten_tree(state, json, None, false, "$".into(), 0, &mut rows);
    let font_size = ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5);
    let font = egui::FontId::monospace(font_size);
    let row_h = 22.0;
    const INDENT: f32 = 16.0;
    let needle = search.to_lowercase();
    let mut toggle: Option<(String, bool)> = None;
    let frame = egui::Frame::new()
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(4, 6));
    frame.show(ui, |ui| {
        egui::ScrollArea::both().auto_shrink([false; 2]).id_salt("json-tree").show_rows(
            ui,
            row_h,
            rows.len(),
            |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let width = ui.available_width().max(200.0);
                for row in &rows[range] {
                    let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                    let painter = ui.painter();
                    let x0 = rect.left() + 6.0 + row.depth as f32 * INDENT;
                    // Indent guides under each open ancestor.
                    for d in 0..row.depth {
                        let x = rect.left() + 6.0 + d as f32 * INDENT + 7.0;
                        painter.vline(x, rect.y_range(), Stroke::new(1.0, p.border));
                    }
                    if let Some(n) = row.more {
                        painter.text(
                            pos2(x0 + 18.0, rect.center().y),
                            Align2::LEFT_CENTER,
                            trf("… {} more", &[&n]),
                            egui::FontId::proportional(font_size),
                            p.text_muted,
                        );
                        continue;
                    }
                    let container = matches!(row.value, serde_json::Value::Object(_) | serde_json::Value::Array(_));
                    let mut job = egui::text::LayoutJob::default();
                    let fmt = |c: Color32| egui::TextFormat::simple(font.clone(), c);
                    let key_matches =
                        row.key.as_ref().is_some_and(|k| !needle.is_empty() && k.to_lowercase().contains(&needle));
                    if let Some(k) = &row.key {
                        let mut f = fmt(if row.is_index { p.text_faint } else { colors.key });
                        if key_matches {
                            f.background = highlight::search_background(dark);
                        }
                        job.append(k, 0.0, f);
                        job.append(": ", 0.0, fmt(colors.punct));
                    }
                    let (summary, value_text, value_color) = match row.value {
                        serde_json::Value::Object(m) => {
                            let preview = if row.open {
                                String::new()
                            } else {
                                let keys: Vec<&str> = m.keys().take(4).map(String::as_str).collect();
                                format!(" {}{}", keys.join(", "), if m.len() > 4 { ", …" } else { "" })
                            };
                            (
                                trf("{} keys", &[&m.len()]),
                                if row.open { "{".to_string() } else { format!("{{{preview} }}") },
                                colors.punct,
                            )
                        }
                        serde_json::Value::Array(a) => (
                            trf("{} items", &[&a.len()]),
                            if row.open { "[".to_string() } else { "[…]".to_string() },
                            colors.punct,
                        ),
                        serde_json::Value::String(_) => (String::new(), row.value.to_string(), colors.string),
                        serde_json::Value::Number(_) => (String::new(), row.value.to_string(), colors.number),
                        _ => (String::new(), row.value.to_string(), colors.keyword),
                    };
                    let mut vf = fmt(value_color);
                    if !container && !needle.is_empty() && value_text.to_lowercase().contains(&needle) {
                        vf.background = highlight::search_background(dark);
                    }
                    job.append(&value_text, 0.0, vf);
                    if !summary.is_empty() {
                        job.append("  ", 0.0, fmt(p.text_faint));
                        job.append(
                            &summary,
                            0.0,
                            egui::TextFormat::simple(egui::FontId::proportional(font_size - 1.0), p.text_faint),
                        );
                    }
                    job.wrap = egui::text::TextWrapping::truncate_at_width((rect.right() - x0 - 24.0).max(40.0));
                    let hovered = resp.hovered();
                    if hovered {
                        ui.painter().rect_filled(rect, CornerRadius::same(4), p.faint_row());
                    }
                    if container {
                        let caret = if row.open { icons::CARET_DOWN } else { icons::CARET_RIGHT };
                        ui.painter().text(
                            pos2(x0 + 7.0, rect.center().y),
                            Align2::CENTER_CENTER,
                            caret,
                            theme::icon_font(11.0),
                            if hovered { p.text } else { p.text_muted },
                        );
                    }
                    let galley = ui.painter().layout_job(job);
                    ui.painter().galley(pos2(x0 + 18.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
                    if container && resp.clicked() {
                        toggle = Some((row.path.clone(), !row.open));
                    }
                    let resp = if container { resp } else { resp.on_hover_text(RichText::new(&row.path).monospace()) };
                    resp.context_menu(|ui| {
                        if ui.button(kit::with_icon(icons::COPY, tr("Copy path"))).clicked() {
                            ui.ctx().copy_text(row.path.clone());
                            ui.close();
                        }
                        if ui.button(kit::with_icon(icons::COPY, tr("Copy value"))).clicked() {
                            let text = match row.value {
                                serde_json::Value::String(s) => s.clone(),
                                v => serde_json::to_string_pretty(v).unwrap_or_default(),
                            };
                            ui.ctx().copy_text(text);
                            ui.close();
                        }
                        if let Some(k) = &row.key
                            && ui.button(kit::with_icon(icons::COPY, tr("Copy key"))).clicked()
                        {
                            ui.ctx().copy_text(k.clone());
                            ui.close();
                        }
                    });
                }
            },
        );
    });
    if let Some((path, open)) = toggle {
        state.tree_open.insert(path, open);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_splits_long_rows_and_lines() {
        let ix = LineIndex::from_bytes(b"ab\ncd\n");
        assert_eq!(ix.rows(), 3);
        let long = vec![b'x'; MAX_ROW_BYTES * 2 + 10];
        let ix = LineIndex::from_bytes(&long);
        assert_eq!(ix.rows(), 3);
        let dir = tempfile_dir();
        let path = dir.join("b.txt");
        std::fs::write(&path, b"one\ntwo\nthree").unwrap();
        let ix = LineIndex::from_file(&path).unwrap();
        assert_eq!(ix.rows(), 3);
        let src = Source::Text(Arc::new("one\ntwo\nthree".into()));
        assert_eq!(read_rows(&src, &ix, 1..3), vec!["two", "three"]);
        assert_eq!(find_match_row(&src, &ix, "THR", 1), Some(2));
    }

    /// Lowercasing changes the UTF-8 length of some characters (`İ` becomes
    /// two chars), so offsets in the lowercased text do not fit the original.
    #[test]
    fn search_jump_with_case_changing_chars() {
        assert_eq!(nth_match_char("{\"a\": 1, \"A\": 2}", "a", 2), Some(10));
        assert_eq!(nth_match_char("İİİİ a", "a", 1), Some(5));
        assert_eq!(nth_match_char("\"İstanbul\" ankara", "ank", 1), Some(11));
        // KELVIN SIGN (3 bytes) lowercases to ASCII `k`.
        assert_eq!(nth_match_char("\u{212A}\u{212A} é k", "k", 3), Some(5));
        assert_eq!(nth_match_char("İ", "i̇", 1), Some(0));
        assert_eq!(nth_match_char("abc", "x", 1), None);
    }

    /// A new image response of the same size from the same URL (a random
    /// avatar, a captcha) must not show the previously loaded picture.
    #[test]
    fn image_previews_of_different_bytes_differ() {
        let url = "https://img.test/random.png";
        assert_ne!(image_uri(url, &[1, 2, 3, 4]), image_uri(url, &[4, 3, 2, 1]));
        assert_ne!(image_uri("https://a.test/x", b"same"), image_uri("https://b.test/x", b"same"));
        assert_eq!(image_uri(url, b"img"), image_uri(url, b"img"));
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("merpati-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
