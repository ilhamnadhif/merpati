//! Response body viewer (FR-P1, FR-P2).
//!
//! Small bodies are shown in a read-only, highlighted, selectable editor
//! with a gutter of line numbers and fold carets: JSON folds by brackets,
//! XML, HTML, YAML and other text by indentation. Large bodies are shown
//! through a virtual row viewer that only reads and lays out the visible
//! rows; for bodies kept in a temporary file the row index is built on a
//! background thread, so a 100 MB response never blocks the UI or loads
//! fully into memory. Both keep a fixed content width, so scrolling down
//! never changes the horizontal scroll range.
//!
//! JSON bodies can be filtered with JSONPath (Pretty and Tree views), and a
//! right click on a JSON value copies it or its path, or asks the request tab
//! to save it in a variable or assert it in a test ([`BodyAction`]).

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use egui::{Align, Align2, Color32, CornerRadius, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use parking_lot::Mutex;
use serde_json::Value;

use merpati_core::jsonpath::{self, Segment};
use merpati_core::response::{BodyLanguage, HttpResponse, ResponseBody};

use crate::highlight::{self, HlKey, Lang};
use crate::i18n::{tr, trf};
use crate::{icons, kit, theme};

/// Rows longer than this are split for display.
const MAX_ROW_BYTES: usize = 2048;
/// Displayed text up to this size uses the selectable editor (with folding).
const EDITOR_LIMIT: usize = 256 * 1024;
/// A JSONPath result prints at most this much text.
const FILTER_TEXT_LIMIT: usize = 16 * 1024 * 1024;
/// The tree view lists at most this many children of a node or filter matches.
const TREE_LIMIT: usize = 1000;
const NO_REGION: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyView {
    Pretty,
    Raw,
    Preview,
    Tree,
}

/// Something the request tab should do for the user.
#[derive(Clone, Debug, PartialEq)]
pub enum BodyAction {
    /// Save a response value into a variable (the request tab asks name and scope).
    SetVariable { path: String, value: serde_json::Value },
    /// Append `pm.test` asserting this value to the post-response script.
    AddTest { path: String, value: serde_json::Value },
}

/// Start offsets of display rows.
#[derive(Default)]
pub struct LineIndex {
    pub starts: Vec<u64>,
    pub total: u64,
    /// Characters in the longest row (a tab counts as 4): the content width
    /// of the row viewer, known before any row is drawn.
    pub max_cols: usize,
}

/// Columns a byte adds to its row: continuation bytes of a UTF-8 character add none.
fn byte_cols(c: u8) -> usize {
    match c {
        b'\t' => 4,
        c if c & 0xC0 == 0x80 => 0,
        _ => 1,
    }
}

impl LineIndex {
    pub fn from_bytes(b: &[u8]) -> LineIndex {
        let mut starts = vec![0u64];
        let mut row_start = 0usize;
        let (mut cols, mut max_cols) = (0usize, 0usize);
        for (i, &c) in b.iter().enumerate() {
            if c == b'\n' {
                starts.push(i as u64 + 1);
                row_start = i + 1;
                max_cols = max_cols.max(cols);
                cols = 0;
                continue;
            }
            if i - row_start >= MAX_ROW_BYTES && (c & 0xC0) != 0x80 {
                starts.push(i as u64);
                row_start = i;
                max_cols = max_cols.max(cols);
                cols = 0;
            }
            cols += byte_cols(c);
        }
        LineIndex { starts, total: b.len() as u64, max_cols: max_cols.max(cols) }
    }

    pub fn from_file(path: &std::path::Path) -> std::io::Result<LineIndex> {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut starts = vec![0u64];
        let mut pos = 0u64;
        let mut row_start = 0u64;
        let (mut cols, mut max_cols) = (0usize, 0usize);
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
                    max_cols = max_cols.max(cols);
                    cols = 0;
                    continue;
                }
                if at - row_start >= MAX_ROW_BYTES as u64 && (c & 0xC0) != 0x80 {
                    starts.push(at);
                    row_start = at;
                    max_cols = max_cols.max(cols);
                    cols = 0;
                }
                cols += byte_cols(c);
            }
            pos += n as u64;
        }
        if starts.last() == Some(&pos) && pos > 0 {
            starts.pop();
        }
        Ok(LineIndex { starts, total: pos, max_cols: max_cols.max(cols) })
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

// ---------------------------------------------------------------------------
// Folding
// ---------------------------------------------------------------------------

/// What a folded block holds, shown muted after the folded line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Count {
    Keys(usize),
    Items(usize),
    Children(usize),
    Lines(usize),
}

fn count_label(c: Count) -> String {
    match c {
        Count::Keys(1) => tr("1 key").to_string(),
        Count::Keys(n) => trf("{} keys", &[&n]),
        Count::Items(1) => tr("1 item").to_string(),
        Count::Items(n) => trf("{} items", &[&n]),
        Count::Children(1) => tr("1 child").to_string(),
        Count::Children(n) => trf("{} children", &[&n]),
        Count::Lines(1) => tr("1 line").to_string(),
        Count::Lines(n) => trf("{} lines", &[&n]),
    }
}

/// A block that can be folded: from the line that opens it to its last line.
#[derive(Clone, Debug, PartialEq)]
struct Region {
    start: u32,
    end: u32,
    /// Nesting level (0: outermost).
    depth: u16,
    /// The last line closes the block (`}`, `]`, `</tag>`) and is shown on
    /// the folded line: `"a": { … },`.
    tail: bool,
    count: Count,
}

/// The fold regions of a text.
struct Folds {
    line_starts: Vec<usize>,
    /// Sorted by start line.
    regions: Vec<Region>,
    /// Region starting at each line (`NO_REGION`: none).
    at_line: Vec<u32>,
}

impl Folds {
    fn compute(text: &str, lang: Lang) -> Folds {
        let mut line_starts = vec![0];
        line_starts.extend(text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
        let lines: Vec<&str> = (0..line_starts.len()).map(|i| line_text(text, &line_starts, i)).collect();
        let mut regions = match lang {
            Lang::Json => bracket_regions(&lines),
            _ => indent_regions(&lines, lang),
        };
        regions.sort_by_key(|r| r.start);
        let mut at_line = vec![NO_REGION; lines.len()];
        for (i, r) in regions.iter().enumerate() {
            at_line[r.start as usize] = i as u32;
        }
        Folds { line_starts, regions, at_line }
    }

    fn region_at(&self, line: usize) -> Option<u32> {
        self.at_line.get(line).copied().filter(|&r| r != NO_REGION)
    }

    fn line_of(&self, byte: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= byte).saturating_sub(1)
    }

    /// Unfolds every folded region that hides `line`; true if any was folded.
    fn reveal(&self, line: usize, collapsed: &mut [bool]) -> bool {
        let mut changed = false;
        for (i, r) in self.regions.iter().enumerate() {
            if collapsed[i] && (r.start as usize) < line && line <= r.end as usize {
                collapsed[i] = false;
                changed = true;
            }
        }
        changed
    }
}

/// Line `i` without its line break.
fn line_text<'t>(text: &'t str, starts: &[usize], i: usize) -> &'t str {
    let s = starts[i];
    let e = starts.get(i + 1).map(|e| e - 1).unwrap_or(text.len());
    &text[s..e.max(s)]
}

/// Pretty-printed JSON: a line ending in `{` or `[` opens a block that the
/// line starting with the matching `}` or `]` closes.
fn bracket_regions(lines: &[&str]) -> Vec<Region> {
    // (line, opener, direct children)
    let mut stack: Vec<(u32, u8, usize)> = Vec::new();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        let Some(&first) = t.as_bytes().first() else { continue };
        if first == b'}' || first == b']' {
            if let Some((start, open, count)) = stack.pop()
                && count > 0
            {
                let count = if open == b'{' { Count::Keys(count) } else { Count::Items(count) };
                out.push(Region { start, end: i as u32, depth: stack.len() as u16, tail: true, count });
            }
            continue;
        }
        if let Some(top) = stack.last_mut() {
            top.2 += 1;
        }
        let last = t.as_bytes()[t.len() - 1];
        if last == b'{' || last == b'[' {
            stack.push((i as u32, last, 0));
        }
    }
    out
}

/// Indented text: a line opens a block when the next lines are indented
/// deeper. A closing line at the opening line's indent (`</tag>`, `}`) ends
/// the block. YAML list items (`- a`) count as one level deeper than their
/// dash, so a list right under its key still folds.
fn indent_regions(lines: &[&str], lang: Lang) -> Vec<Region> {
    struct Open {
        line: u32,
        indent: usize,
        count: usize,
        dashes: bool,
    }
    let indent_of = |l: &str| -> usize {
        let mut n = 0;
        for c in l.chars() {
            match c {
                ' ' => n += 1,
                '\t' => n += 4,
                _ => break,
            }
        }
        let t = l.trim_start();
        if lang == Lang::Yaml && (t.starts_with("- ") || t == "-") { n + 1 } else { n }
    };
    let closer = |t: &str| {
        lang != Lang::Yaml && (t.starts_with("</") || t.starts_with('}') || t.starts_with(']') || t.starts_with(')'))
    };
    let make = |o: &Open, end: usize, tail: bool, depth: usize| Region {
        start: o.line,
        end: end as u32,
        depth: depth as u16,
        tail,
        count: match lang {
            Lang::Xml => Count::Children(o.count),
            Lang::Yaml if o.dashes => Count::Items(o.count),
            Lang::Yaml => Count::Keys(o.count),
            _ => Count::Lines(end - o.line as usize - usize::from(tail)),
        },
    };
    let mut stack: Vec<Open> = Vec::new();
    let mut out = Vec::new();
    let mut prev = 0usize;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        let d = indent_of(l);
        let mut closed_here = false;
        while stack.last().is_some_and(|top| top.indent >= d) {
            let Some(top) = stack.pop() else { break };
            let tail = closer(t) && top.indent == d;
            let end = if tail { i } else { prev };
            if top.count > 0 && end > top.line as usize {
                out.push(make(&top, end, tail, stack.len()));
            }
            closed_here |= tail;
        }
        if !closed_here {
            if let Some(top) = stack.last_mut() {
                if top.count == 0 {
                    top.dashes = t.starts_with("- ") || t == "-";
                }
                top.count += 1;
            }
            stack.push(Open { line: i as u32, indent: d, count: 0, dashes: false });
        }
        prev = i;
    }
    while let Some(top) = stack.pop() {
        if top.count > 0 && prev > top.line as usize {
            out.push(make(&top, prev, false, stack.len()));
        }
    }
    out
}

/// The text as shown: folded regions replaced by one line.
struct Display {
    text: Arc<String>,
    /// Per shown line; empty when nothing is folded (line `i` is line `i`).
    lines: Vec<DLine>,
}

struct DLine {
    /// Line of the unfolded text.
    orig: u32,
    /// Byte offset in the shown text.
    start: usize,
    /// The folded region this line stands for.
    collapsed: Option<u32>,
    /// Byte offset of the `…` of a folded line.
    ellipsis: usize,
}

impl Display {
    fn new(text: &Arc<String>, folds: Option<&Folds>, collapsed: &[bool]) -> Display {
        let Some(folds) = folds.filter(|_| collapsed.iter().any(|&c| c)) else {
            return Display { text: text.clone(), lines: Vec::new() };
        };
        let n = folds.line_starts.len();
        let mut out = String::with_capacity(text.len());
        let mut lines = Vec::new();
        let mut l = 0;
        while l < n {
            let start = out.len();
            let ri = folds.at_line[l];
            if ri != NO_REGION && collapsed[ri as usize] {
                let r = &folds.regions[ri as usize];
                out.push_str(line_text(text, &folds.line_starts, l).trim_end());
                out.push(' ');
                let ellipsis = out.len();
                out.push('…');
                if r.tail {
                    out.push(' ');
                    out.push_str(line_text(text, &folds.line_starts, r.end as usize).trim());
                }
                lines.push(DLine { orig: l as u32, start, collapsed: Some(ri), ellipsis });
                l = r.end as usize + 1;
            } else {
                out.push_str(line_text(text, &folds.line_starts, l));
                lines.push(DLine { orig: l as u32, start, collapsed: None, ellipsis: 0 });
                l += 1;
            }
            if l < n {
                out.push('\n');
            }
        }
        Display { text: Arc::new(out), lines }
    }

    /// Unfolded line number of shown line `line`.
    fn orig(&self, line: usize) -> usize {
        self.lines.get(line).map_or(line, |l| l.orig as usize)
    }

    fn collapsed_at(&self, line: usize) -> Option<u32> {
        self.lines.get(line).and_then(|l| l.collapsed)
    }

    /// Character offset in the shown text of byte `byte` of the unfolded text.
    fn char_of(&self, doc: &Doc, byte: usize) -> usize {
        let Some(folds) = doc.folds.as_ref().filter(|_| !self.lines.is_empty()) else {
            return doc.text[..byte.min(doc.text.len())].chars().count();
        };
        let line = folds.line_of(byte);
        let d = self.lines.partition_point(|l| l.orig as usize <= line).saturating_sub(1);
        let dl = &self.lines[d];
        let mut at = dl.start;
        if dl.orig as usize == line {
            let within = byte - folds.line_starts[line];
            at += match dl.collapsed {
                Some(_) => within.min(dl.ellipsis - dl.start),
                None => within,
            };
        }
        let at = at.min(self.text.len());
        let at = (0..=at).rev().find(|&i| self.text.is_char_boundary(i)).unwrap_or(0);
        self.text[..at].chars().count()
    }
}

/// Fold state of one text.
struct FoldView {
    key: u64,
    collapsed: Vec<bool>,
    display: Option<Arc<Display>>,
}

impl FoldView {
    fn display(&mut self, doc: &Doc) -> Arc<Display> {
        self.display
            .get_or_insert_with(|| Arc::new(Display::new(&doc.text, doc.folds.as_deref(), &self.collapsed)))
            .clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FoldCmd {
    ExpandAll,
    CollapseAll,
    /// Fold every block nested this deep or deeper.
    Level(usize),
}

// ---------------------------------------------------------------------------
// Prepared content
// ---------------------------------------------------------------------------

enum Source {
    Text(Arc<String>),
    File(ResponseBody),
}

/// Where the JSON printed in a [`Doc`] sits in the body, to name the value
/// on a line.
struct DocJson {
    root: Arc<Value>,
    base: Base,
}

enum Base {
    /// The text is the whole body.
    Root,
    /// The text is the value at this path.
    At(Vec<Segment<'static>>),
    /// The text is an array of the values at these paths.
    Each(Arc<Vec<Vec<Segment<'static>>>>),
}

impl DocJson {
    /// Path and value of what is printed on line `line`.
    fn target(&self, line: usize) -> Option<(String, Value)> {
        let mut rel = Vec::new();
        match &self.base {
            Base::Root => {
                let v = value_at_line(&self.root, line, &mut rel)?;
                Some((jsonpath::format_path(&rel), v.clone()))
            }
            Base::At(base) => {
                let start = jsonpath::resolve(&self.root, base)?;
                let v = value_at_line(start, line, &mut rel)?;
                Some((join_path(base, &rel), v.clone()))
            }
            Base::Each(paths) => {
                let items = paths
                    .iter()
                    .enumerate()
                    .filter_map(|(k, p)| jsonpath::resolve(&self.root, p).map(|v| (Segment::Index(k), v)));
                let v = in_children(items, None, line, &mut rel)?;
                let Some(Segment::Index(k)) = rel.first() else { return None };
                Some((join_path(&paths[*k], &rel[1..]), v.clone()))
            }
        }
    }
}

fn join_path(base: &[Segment], rest: &[Segment]) -> String {
    let mut s = jsonpath::format_path(base);
    for seg in rest {
        match seg {
            Segment::Key(k) => jsonpath::push_key(&mut s, k),
            Segment::Index(i) => jsonpath::push_index(&mut s, *i),
        }
    }
    s
}

/// Lines `v` takes when pretty-printed.
fn json_lines(v: &Value) -> usize {
    match v {
        Value::Object(m) if !m.is_empty() => 2 + m.values().map(json_lines).sum::<usize>(),
        Value::Array(a) if !a.is_empty() => 2 + a.iter().map(json_lines).sum::<usize>(),
        _ => 1,
    }
}

/// The value printed on line `target` of `v` pretty-printed (a container
/// for its closing line), and its path below `v`.
fn value_at_line<'v>(v: &'v Value, target: usize, path: &mut Vec<Segment<'v>>) -> Option<&'v Value> {
    if target == 0 {
        return Some(v);
    }
    match v {
        Value::Object(m) => {
            in_children(m.iter().map(|(k, c)| (Segment::Key(Cow::Borrowed(k.as_str())), c)), Some(v), target, path)
        }
        Value::Array(a) => {
            in_children(a.iter().enumerate().map(|(i, c)| (Segment::Index(i), c)), Some(v), target, path)
        }
        _ => None,
    }
}

fn in_children<'v>(
    kids: impl Iterator<Item = (Segment<'v>, &'v Value)>,
    parent: Option<&'v Value>,
    target: usize,
    path: &mut Vec<Segment<'v>>,
) -> Option<&'v Value> {
    if target == 0 {
        return parent;
    }
    let mut cur = 1;
    for (seg, child) in kids {
        let n = json_lines(child);
        if target < cur + n {
            path.push(seg);
            return value_at_line(child, target - cur, path);
        }
        cur += n;
    }
    if target == cur { parent } else { None }
}

/// A text shown by the Pretty or Raw view.
#[derive(Clone)]
struct Doc {
    text: Arc<String>,
    lang: Lang,
    lines: usize,
    /// Row index when the text is too large for the editor.
    index: Option<Arc<LineIndex>>,
    folds: Option<Arc<Folds>>,
    json: Option<Arc<DocJson>>,
    /// Hash of the text: fold state follows the text across view switches.
    key: u64,
}

impl Doc {
    fn new(text: String, lang: Lang, json: Option<DocJson>, foldable: bool) -> Doc {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        lang.hash(&mut h);
        foldable.hash(&mut h);
        let key = h.finish();
        let lines = text.bytes().filter(|&b| b == b'\n').count() + 1;
        let (index, folds) = if text.len() > EDITOR_LIMIT {
            (Some(Arc::new(LineIndex::from_bytes(text.as_bytes()))), None)
        } else {
            (None, foldable.then(|| Arc::new(Folds::compute(&text, lang))))
        };
        Doc { text: Arc::new(text), lang, lines, index, folds, json: json.map(Arc::new), key }
    }

    fn foldable(&self) -> bool {
        self.folds.as_ref().is_some_and(|f| !f.regions.is_empty())
    }
}

#[derive(Clone)]
enum Content {
    Empty,
    Doc(Doc),
    /// A body in a temporary file, indexed on a background thread.
    File {
        body: ResponseBody,
        lang: Lang,
        index: Arc<Mutex<Option<Arc<LineIndex>>>>,
    },
}

struct Prepared {
    view: BodyView,
    note: Option<String>,
    is_image: bool,
    is_html: bool,
    is_binary: bool,
    /// Pixel size of an image body.
    image_size: Option<[u32; 2]>,
    /// The body as JSON (JSON bodies within the pretty-print limit).
    json: Option<Arc<Value>>,
    /// Show `json` as a tree.
    tree: bool,
    content: Content,
}

/// The result of the JSONPath filter for one expression.
struct FilterResult {
    expr: String,
    error: Option<jsonpath::ParseError>,
    /// The last valid result, kept while a path is being typed.
    ok: Option<FilterOk>,
}

#[derive(Clone)]
struct FilterOk {
    total: usize,
    /// Matches printed (all unless the text limit was reached).
    shown: usize,
    paths: Arc<Vec<Vec<Segment<'static>>>>,
    /// The matches pretty-printed; `None` without matches.
    doc: Option<Doc>,
}

fn run_filter(root: &Arc<Value>, expr: &str, prev: Option<FilterOk>) -> FilterResult {
    let path = match jsonpath::JsonPath::parse(expr) {
        Ok(p) => p,
        Err(e) => return FilterResult { expr: expr.to_string(), error: Some(e), ok: prev },
    };
    let matches = path.find(root);
    let total = matches.len();
    let owned = |p: &[Segment]| p.iter().cloned().map(Segment::into_owned).collect::<Vec<_>>();
    let mut paths = Vec::new();
    let mut text = String::new();
    let base = match matches.as_slice() {
        [] => None,
        [one] => {
            highlight::write_json_pretty(&mut text, one.value, 0);
            paths.push(owned(&one.path));
            Some(Base::At(owned(&one.path)))
        }
        many => {
            text.push_str("[\n");
            for (i, m) in many.iter().enumerate() {
                if text.len() > FILTER_TEXT_LIMIT {
                    break;
                }
                if i > 0 {
                    text.push_str(",\n");
                }
                text.push_str("  ");
                highlight::write_json_pretty(&mut text, m.value, 1);
                paths.push(owned(&m.path));
            }
            text.push_str("\n]");
            None
        }
    };
    let shown = paths.len();
    let paths = Arc::new(paths);
    let doc = (total > 0).then(|| {
        let base = base.unwrap_or_else(|| Base::Each(paths.clone()));
        Doc::new(text, Lang::Json, Some(DocJson { root: root.clone(), base }), true)
    });
    FilterResult { expr: expr.to_string(), error: None, ok: Some(FilterOk { total, shown, paths, doc }) }
}

/// The value under a right click in the Pretty view.
#[derive(Clone)]
struct MenuTarget {
    key: u64,
    line: usize,
    path: String,
    value: Value,
}

pub struct BodyState {
    pub view: BodyView,
    pub search: String,
    pub wrap: bool,
    /// JSONPath filter of JSON bodies (Pretty and Tree views).
    pub filter: String,
    pub filter_open: bool,
    prepared: Option<Prepared>,
    /// The body parsed as JSON, shared by the views (`Some(None)`: not JSON).
    parsed: Option<Option<Arc<Value>>>,
    filter_result: Option<FilterResult>,
    focus_filter: bool,
    /// Scroll to the `match_cursor`-th search match.
    jump: bool,
    match_cursor: usize,
    /// Tree view: nodes opened or closed by the user (by JSON path).
    tree_open: HashMap<String, bool>,
    /// Tree view: levels open by default (`usize::MAX`: all), set by the fold actions.
    tree_level: Option<usize>,
    /// Pretty view: fold state of the last texts shown.
    folds: Vec<FoldView>,
    menu: Option<MenuTarget>,
    /// Row viewer: widest row drawn (wide glyphs beyond the estimate).
    rows_width: f32,
    pending_fold: Option<FoldCmd>,
    pending_scroll: Option<egui::Vec2>,
}

impl Default for BodyState {
    fn default() -> Self {
        BodyState {
            view: BodyView::Pretty,
            search: String::new(),
            wrap: false,
            filter: String::new(),
            filter_open: false,
            prepared: None,
            parsed: None,
            filter_result: None,
            focus_filter: false,
            jump: false,
            match_cursor: 0,
            tree_open: Default::default(),
            tree_level: None,
            folds: Vec::new(),
            menu: None,
            rows_width: 0.0,
            pending_fold: None,
            pending_scroll: None,
        }
    }
}

impl BodyState {
    pub fn reset(&mut self) {
        self.prepared = None;
        self.jump = false;
        self.match_cursor = 0;
        self.menu = None;
    }

    /// Opens the JSONPath filter with `expr` (automation).
    pub fn set_filter(&mut self, expr: &str) {
        self.filter_open = true;
        self.filter = expr.to_string();
    }

    /// `expand`, `collapse`, `level1`, `level2`, … (automation).
    pub fn fold(&mut self, what: &str) {
        self.pending_fold = Some(match what {
            "expand" => FoldCmd::ExpandAll,
            "collapse" => FoldCmd::CollapseAll,
            l => FoldCmd::Level(l.trim_start_matches("level").parse().unwrap_or(1)),
        });
    }

    /// Searches for `text` and scrolls to the next match (automation).
    pub fn find_next(&mut self, text: &str) {
        if self.search != text {
            self.search = text.to_string();
            self.match_cursor = 0;
        }
        self.match_cursor += 1;
        self.jump = true;
    }

    /// Scrolls the body to this offset (automation).
    pub fn scroll_body(&mut self, offset: [f32; 2]) {
        self.pending_scroll = Some(vec2(offset[0], offset[1]));
    }

    fn prepare(&mut self, resp: &HttpResponse, max_pretty: u64, ctx: &egui::Context) {
        if self.prepared.as_ref().is_none_or(|p| p.view != self.view) {
            self.prepared = Some(prepare(resp, self.view, max_pretty, ctx, &mut self.parsed));
        }
    }

    /// The filter applies: open, not empty, on a JSON body in Pretty or Tree.
    fn filter_active(&self) -> bool {
        self.filter_open
            && !self.filter.trim().is_empty()
            && self
                .prepared
                .as_ref()
                .is_some_and(|p| p.json.is_some() && matches!(p.view, BodyView::Pretty | BodyView::Tree))
    }

    fn update_filter(&mut self) {
        let expr = self.filter.trim();
        if expr.is_empty() {
            self.filter_result = None;
            return;
        }
        let Some(root) = self.prepared.as_ref().and_then(|p| p.json.clone()) else { return };
        if !self.filter_open || self.filter_result.as_ref().is_some_and(|f| f.expr == expr) {
            return;
        }
        let prev = self.filter_result.take().and_then(|f| f.ok);
        self.filter_result = Some(run_filter(&root, expr, prev));
    }

    fn filter_ok(&self) -> Option<&FilterOk> {
        if !self.filter_active() {
            return None;
        }
        self.filter_result.as_ref()?.ok.as_ref()
    }

    /// The text the Pretty or Raw view shows (the filter result when filtering).
    fn current_doc(&self) -> Option<Doc> {
        if let Some(ok) = self.filter_ok() {
            return ok.doc.clone();
        }
        match &self.prepared.as_ref()?.content {
            Content::Doc(d) => Some(d.clone()),
            _ => None,
        }
    }

    fn fold_view(&mut self, doc: &Doc) -> &mut FoldView {
        if let Some(i) = self.folds.iter().position(|f| f.key == doc.key) {
            return &mut self.folds[i];
        }
        if self.folds.len() >= 4 {
            self.folds.remove(0);
        }
        let n = doc.folds.as_ref().map_or(0, |f| f.regions.len());
        self.folds.push(FoldView { key: doc.key, collapsed: vec![false; n], display: None });
        let last = self.folds.len() - 1;
        &mut self.folds[last]
    }

    fn apply_fold(&mut self, cmd: FoldCmd) {
        if self.prepared.as_ref().is_some_and(|p| p.tree) {
            self.tree_open.clear();
            self.tree_level = Some(match cmd {
                FoldCmd::ExpandAll => usize::MAX,
                FoldCmd::CollapseAll => 0,
                FoldCmd::Level(n) => n,
            });
            return;
        }
        let Some(doc) = self.current_doc() else { return };
        let Some(folds) = doc.folds.clone() else { return };
        let fv = self.fold_view(&doc);
        for (c, r) in fv.collapsed.iter_mut().zip(&folds.regions) {
            *c = match cmd {
                FoldCmd::ExpandAll => false,
                FoldCmd::CollapseAll => true,
                FoldCmd::Level(n) => r.depth as usize >= n,
            };
        }
        fv.display = None;
    }

    fn toggle_region(&mut self, doc: &Doc, region: u32) {
        let fv = self.fold_view(doc);
        if let Some(c) = fv.collapsed.get_mut(region as usize) {
            *c = !*c;
            fv.display = None;
        }
    }
}

fn is_yaml(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.contains("yaml") || ct.contains("yml")
}

fn parse_json(parsed: &mut Option<Option<Arc<Value>>>, bytes: &[u8]) -> Option<Arc<Value>> {
    parsed.get_or_insert_with(|| serde_json::from_slice(bytes).ok().map(Arc::new)).clone()
}

fn prepare(
    resp: &HttpResponse,
    view: BodyView,
    max_pretty: u64,
    ctx: &egui::Context,
    parsed: &mut Option<Option<Arc<Value>>>,
) -> Prepared {
    let language = resp.language();
    let lang = if is_yaml(resp.content_type()) { Lang::Yaml } else { Lang::from_language(language) };
    let len = resp.body.len();
    let mut p = Prepared {
        view,
        note: None,
        is_image: language == BodyLanguage::Image,
        is_html: language == BodyLanguage::Html,
        is_binary: language == BodyLanguage::Binary,
        image_size: None,
        json: None,
        tree: false,
        content: Content::Empty,
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
            if language == BodyLanguage::Json {
                p.json = parse_json(parsed, &bytes);
            }
            if view == BodyView::Tree && language == BodyLanguage::Json {
                if p.json.is_some() {
                    p.tree = true;
                    return p;
                }
                p.note = Some(tr("The body is not valid JSON, so it cannot be shown as a tree.").to_string());
            }
            let (text, json) = match (language, &p.json) {
                (BodyLanguage::Json, Some(v)) => {
                    (highlight::pretty_json_value(v), Some(DocJson { root: v.clone(), base: Base::Root }))
                }
                (BodyLanguage::Xml | BodyLanguage::Html, _) => {
                    (highlight::pretty_xml(&String::from_utf8_lossy(&bytes)), None)
                }
                _ => (String::from_utf8_lossy(&bytes).into_owned(), None),
            };
            p.content = Content::Doc(Doc::new(text, lang, json, true));
        }
        (_, true) => {
            let bytes = resp.body.read_all().unwrap_or_default();
            p.content = Content::Doc(Doc::new(String::from_utf8_lossy(&bytes).into_owned(), lang, None, false));
        }
        (_, false) => {
            if view != BodyView::Raw {
                p.note = Some(trf(
                    "The body is {}; showing the raw view. Pretty printing is limited to {} (Settings).",
                    &[&merpati_core::util::human_bytes(len), &merpati_core::util::human_bytes(max_pretty)],
                ));
            }
            let lang = if len > 20 * 1024 * 1024 { Lang::Text } else { lang };
            match &resp.body {
                ResponseBody::File { path, .. } => {
                    let index = Arc::new(Mutex::new(None));
                    p.content = Content::File { body: resp.body.clone(), lang, index: index.clone() };
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
                    p.content = Content::Doc(Doc::new(String::from_utf8_lossy(b).into_owned(), lang, None, false));
                }
            }
        }
    }
    p
}

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

/// What the current view offers.
#[derive(Default)]
struct Caps {
    filter: bool,
    fold: bool,
    wrap: bool,
}

fn view_caps(state: &BodyState) -> Caps {
    let Some(p) = &state.prepared else { return Caps::default() };
    Caps {
        filter: p.json.is_some() && matches!(p.view, BodyView::Pretty | BodyView::Tree),
        fold: p.tree || state.current_doc().is_some_and(|d| d.foldable()),
        // Only the editor wraps; the row viewer of large texts doesn't.
        wrap: !p.tree && state.current_doc().is_some_and(|d| d.index.is_none()),
    }
}

fn mono_size(ui: &Ui) -> f32 {
    ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5)
}

fn save_body(resp: &HttpResponse) {
    if let Some(path) = rfd::FileDialog::new().set_file_name(default_file_name(resp)).save_file()
        && let Err(e) = resp.body.save_to(&path)
    {
        tracing::error!("save body: {e}");
    }
}

/// Copies what the view shows (formatted, or the filter result).
fn copy_body(ui: &Ui, state: &BodyState, resp: &HttpResponse) {
    if let Some(doc) = state.current_doc() {
        ui.ctx().copy_text(doc.text.to_string());
    } else if resp.body.len() <= 50 * 1024 * 1024 {
        ui.ctx().copy_text(resp.body.text_lossy());
    }
}

fn search_box(ui: &mut Ui, state: &mut BodyState, width: f32) {
    let r = kit::search_field(ui, "body-search", &mut state.search, tr("Search"), width);
    if r.changed() {
        state.match_cursor = 0;
    }
    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if (kit::icon_button(ui, icons::ARROW_DOWN, tr("Next match")).clicked() || enter) && !state.search.is_empty() {
        state.match_cursor += 1;
        state.jump = true;
        if enter {
            // Enter again goes on to the next match.
            r.request_focus();
        }
    }
}

/// View switch, search and actions. Narrow panels move the less used
/// actions into the ⋯ menu, and very narrow ones put search on its own row.
fn toolbar(ui: &mut Ui, state: &mut BodyState, resp: &HttpResponse, caps: &Caps) -> Option<FoldCmd> {
    let avail = ui.available_width();
    let compact = avail < 660.0;
    let stacked = avail < 440.0;
    let mut cmd = None;
    ui.horizontal(|ui| {
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
        let icons_right = if compact {
            1 + usize::from(caps.filter)
        } else {
            2 + usize::from(caps.wrap) + usize::from(caps.filter) + if caps.fold { 3 } else { 0 }
        };
        if !stacked {
            ui.add_space(4.0);
            let w = (ui.available_width() - icons_right as f32 * 30.0 - 60.0).clamp(110.0, 220.0);
            search_box(ui, state, w);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if !compact {
                if kit::icon_button(ui, icons::DOWNLOAD_SIMPLE, tr("Save response to file")).clicked() {
                    save_body(resp);
                }
                if kit::icon_button(ui, icons::COPY, tr("Copy")).clicked() {
                    copy_body(ui, state, resp);
                }
                if caps.wrap && kit::icon_toggle(ui, icons::TEXT_ALIGN_LEFT, tr("Wrap"), state.wrap).clicked() {
                    state.wrap = !state.wrap;
                }
                if caps.fold || caps.filter {
                    kit::vdivider(ui, 18.0);
                }
            }
            if compact || caps.fold {
                let more = kit::icon_button(ui, icons::DOTS_THREE, tr("More actions"));
                egui::Popup::menu(&more).show(|ui| {
                    ui.set_min_width(210.0);
                    if caps.fold {
                        if compact {
                            if ui.button(kit::with_icon(icons::ARROWS_OUT_LINE_VERTICAL, tr("Expand all"))).clicked() {
                                cmd = Some(FoldCmd::ExpandAll);
                            }
                            if ui.button(kit::with_icon(icons::ARROWS_IN_LINE_VERTICAL, tr("Collapse all"))).clicked() {
                                cmd = Some(FoldCmd::CollapseAll);
                            }
                        }
                        for level in [1, 2] {
                            let label = trf("Collapse to level {}", &[&level]);
                            if ui.button(kit::with_icon(icons::LIST, &label)).clicked() {
                                cmd = Some(FoldCmd::Level(level));
                            }
                        }
                    }
                    if compact {
                        if caps.fold {
                            ui.separator();
                        }
                        if caps.wrap {
                            let icon = if state.wrap { icons::CHECK } else { icons::TEXT_ALIGN_LEFT };
                            if ui.button(kit::with_icon(icon, tr("Wrap lines"))).clicked() {
                                state.wrap = !state.wrap;
                            }
                        }
                        if ui.button(kit::with_icon(icons::COPY, tr("Copy"))).clicked() {
                            copy_body(ui, state, resp);
                            ui.close();
                        }
                        if ui.button(kit::with_icon(icons::DOWNLOAD_SIMPLE, tr("Save response to file"))).clicked() {
                            ui.close();
                            save_body(resp);
                        }
                    }
                    if cmd.is_some() {
                        ui.close();
                    }
                });
            }
            if !compact && caps.fold {
                if kit::icon_button(ui, icons::ARROWS_IN_LINE_VERTICAL, tr("Collapse all")).clicked() {
                    cmd = Some(FoldCmd::CollapseAll);
                }
                if kit::icon_button(ui, icons::ARROWS_OUT_LINE_VERTICAL, tr("Expand all")).clicked() {
                    cmd = Some(FoldCmd::ExpandAll);
                }
            }
            if caps.filter
                && kit::icon_toggle(ui, icons::FUNNEL, tr("Filter with JSONPath"), state.filter_open).clicked()
            {
                state.filter_open = !state.filter_open;
                state.focus_filter = state.filter_open;
            }
        });
    });
    if stacked {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let w = ui.available_width() - 38.0;
            search_box(ui, state, w);
        });
    }
    cmd
}

/// The JSONPath field, with the number of matches or why the path is invalid.
fn filter_bar(ui: &mut Ui, state: &mut BodyState) {
    let p = theme::pal(ui);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let slot = (ui.available_width() * 0.34).clamp(96.0, 280.0);
        let width = (ui.available_width() - slot - 8.0).max(120.0);
        let edit = egui::TextEdit::singleline(&mut state.filter)
            .id_salt("body-jsonpath")
            .hint_text(RichText::new("$.data[*].title").font(FontId::monospace(mono_size(ui))).color(p.text_faint))
            .font(FontId::monospace(mono_size(ui)))
            .desired_width(width)
            .margin(egui::Margin { left: 26, right: 22, top: 4, bottom: 4 });
        let field = ui.add(edit);
        let r = field.rect;
        let active = !state.filter.trim().is_empty();
        ui.painter().text(
            pos2(r.left() + 13.0, r.center().y),
            Align2::CENTER_CENTER,
            icons::FUNNEL,
            theme::icon_font(14.0),
            if active { p.accent_text } else { p.text_faint },
        );
        if !state.filter.is_empty() {
            let x_rect = Rect::from_center_size(pos2(r.right() - 12.0, r.center().y), egui::Vec2::splat(16.0));
            let x = ui.interact(x_rect, field.id.with("clear"), Sense::click()).on_hover_text(tr("Clear"));
            let color = if x.hovered() { p.text } else { p.text_faint };
            ui.painter().text(x_rect.center(), Align2::CENTER_CENTER, icons::X_CIRCLE, theme::icon_font(14.0), color);
            if x.clicked() {
                state.filter.clear();
                field.request_focus();
            }
        }
        if std::mem::take(&mut state.focus_filter) {
            field.request_focus();
        }
        if (field.has_focus() || field.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.filter_open = false;
        }
        state.update_filter();
        ui.add_space(8.0);
        let Some(result) = &state.filter_result else { return };
        if let Some(e) = &result.error {
            let mut job = egui::text::LayoutJob::default();
            let size = ui.style().text_styles.get(&egui::TextStyle::Small).map(|f| f.size).unwrap_or(11.5) + 0.5;
            job.append(icons::WARNING_CIRCLE, 0.0, egui::TextFormat::simple(theme::icon_font(size + 1.0), p.err));
            job.append(
                &format!("  {}", tr(e.message)),
                0.0,
                egui::TextFormat::simple(FontId::proportional(size), p.text_muted),
            );
            ui.add(egui::Label::new(job).truncate())
                .on_hover_text(trf("{} (at character {})", &[&tr(e.message), &(e.offset + 1)]));
        } else if let Some(ok) = &result.ok {
            let label = match ok.total {
                0 => tr("No matches").to_string(),
                1 => tr("1 match").to_string(),
                n => trf("{} matches", &[&n]),
            };
            let pill = kit::pill(ui, &label, if ok.total == 0 { p.text_muted } else { p.info });
            if ok.shown < ok.total {
                pill.on_hover_text(trf("Showing the first {} matches.", &[&ok.shown]));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

/// Toolbar + body. `max_pretty` comes from the settings. Returns what the
/// user asked the request tab to do with a value.
pub fn show(ui: &mut Ui, state: &mut BodyState, resp: &HttpResponse, max_pretty: u64) -> Option<BodyAction> {
    state.prepare(resp, max_pretty, ui.ctx());
    state.update_filter();
    let cmd = toolbar(ui, state, resp, &view_caps(state));
    state.prepare(resp, max_pretty, ui.ctx());
    if view_caps(state).filter && state.filter_open {
        filter_bar(ui, state);
    }
    state.update_filter();
    if let Some(cmd) = cmd.or(state.pending_fold.take()) {
        state.apply_fold(cmd);
    }
    ui.add_space(8.0);

    let pal = theme::pal(ui);
    let p = state.prepared.as_ref()?;
    if let Some(note) = &p.note {
        crate::request_tab::banner(ui, icons::INFO, pal.info, note);
        ui.add_space(8.0);
    }
    if p.is_binary && state.view != BodyView::Raw {
        binary_state(ui, resp);
        return None;
    }
    if state.view == BodyView::Preview {
        if p.is_image {
            image_preview(ui, resp, p.image_size);
            return None;
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
    let (tree, json, content) = (p.tree, p.json.clone(), p.content.clone());
    let filter = state.filter_ok().cloned();
    if filter.as_ref().is_some_and(|f| f.total == 0) {
        kit::empty_state(
            ui,
            icons::FUNNEL,
            tr("No matches"),
            tr("Nothing matches this JSONPath. Change the path, or press Esc to see the whole body."),
            |_| {},
        );
        return None;
    }
    if tree && let Some(root) = json {
        let search = state.search.clone();
        let (roots, more) = match &filter {
            Some(f) => {
                let roots = f
                    .paths
                    .iter()
                    .take(TREE_LIMIT)
                    .filter_map(|path| {
                        let p = jsonpath::format_path(path);
                        jsonpath::resolve(&root, path).map(|value| TreeRoot { key: Some(p.clone()), path: p, value })
                    })
                    .collect::<Vec<_>>();
                (roots, f.total.saturating_sub(TREE_LIMIT))
            }
            None => (vec![TreeRoot { key: None, path: "$".into(), value: &root }], 0),
        };
        return json_tree_view(ui, state, &roots, more, &search);
    }
    let doc = match (filter, content) {
        (Some(f), _) => f.doc,
        (None, Content::Doc(d)) => Some(d),
        (None, Content::File { body, lang, index }) => {
            let ix = index.lock().clone();
            match ix {
                Some(ix) => rows_viewer(ui, state, &Source::File(body), &ix, lang, "file"),
                None => {
                    ui.add_space(24.0);
                    ui.vertical_centered(|ui| {
                        ui.add(egui::Spinner::new().size(20.0).color(pal.accent));
                        ui.add_space(6.0);
                        ui.label(RichText::new(tr("Indexing large response…")).color(pal.text_muted));
                    });
                }
            }
            None
        }
        (None, Content::Empty) => None,
    }?;
    let filtered = state.filter_ok().is_some();
    if let Some(ix) = doc.index.clone() {
        let salt = if filtered { "filter" } else { "text" };
        rows_viewer(ui, state, &Source::Text(doc.text.clone()), &ix, doc.lang, salt);
        return None;
    }
    doc_editor(ui, state, &doc, filtered)
}

/// The frame around the text and tree views.
fn code_frame(ui: &Ui) -> egui::Frame {
    let p = theme::pal(ui);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(1))
}

/// A shown line within the visible part of a galley.
struct VisLine {
    line: usize,
    first: Rect,
    last: Rect,
    /// Character offset of the line in the galley.
    char_start: usize,
}

/// The lines of `galley` (placed at `gpos`) that start inside `clip`.
fn visible_lines(galley: &egui::Galley, gpos: Pos2, clip: Rect) -> Vec<VisLine> {
    let mut out = Vec::new();
    let mut cur: Option<VisLine> = None;
    let (mut line, mut chars, mut at_start) = (0usize, 0usize, true);
    for row in &galley.rows {
        let r = row.rect().translate(gpos.to_vec2());
        if r.top() > clip.bottom() {
            break;
        }
        if at_start {
            out.extend(cur.take());
            if r.bottom() >= clip.top() {
                cur = Some(VisLine { line, first: r, last: r, char_start: chars });
            }
        } else if let Some(c) = &mut cur {
            c.last = r;
        }
        chars += row.char_count_excluding_newline().0 + usize::from(row.ends_with_newline);
        at_start = row.ends_with_newline;
        if row.ends_with_newline {
            line += 1;
        }
    }
    out.extend(cur);
    out
}

/// The shown line at height `y` of the galley.
fn line_at_y(galley: &egui::Galley, gpos: Pos2, y: f32) -> Option<usize> {
    let mut line = 0;
    for row in &galley.rows {
        let r = row.rect().translate(gpos.to_vec2());
        if y < r.bottom() {
            return (y >= r.top() - 2.0).then_some(line);
        }
        if row.ends_with_newline {
            line += 1;
        }
    }
    None
}

/// The selectable text view with line numbers and folding.
fn doc_editor(ui: &mut Ui, state: &mut BodyState, doc: &Doc, filtered: bool) -> Option<BodyAction> {
    let dark = ui.visuals().dark_mode;
    let pal = theme::pal(ui);
    let font_size = mono_size(ui);
    let font = FontId::monospace(font_size);
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
    let search = state.search.clone();
    let wrap = state.wrap;
    let jump = std::mem::take(&mut state.jump);

    // Jumping to a match unfolds the blocks around it.
    let mut scroll_to = None;
    if jump
        && !search.is_empty()
        && let Some(byte) = nth_match_byte(&doc.text, &search, state.match_cursor)
    {
        if let Some(folds) = &doc.folds {
            let fv = state.fold_view(doc);
            if folds.reveal(folds.line_of(byte), &mut fv.collapsed) {
                fv.display = None;
            }
        }
        scroll_to = Some(byte);
    }
    let display = state.fold_view(doc).display(doc);
    let scroll_char = scroll_to.map(|b| display.char_of(doc, b));
    let any_folded = !display.lines.is_empty();

    let gutter_w = doc.lines.to_string().len().max(2) as f32 * char_w + 30.0;
    let text_x = gutter_w + 10.0;
    let lang = doc.lang;
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let mut job = highlight::cached(
            ui.ctx(),
            HlKey { text: buf.as_str(), lang, dark, search: &search, vars: None, font_size, wrap },
        );
        job.wrap.max_width = if wrap { wrap_width } else { f32::INFINITY };
        ui.fonts_mut(|f| f.layout_job(job))
    };
    // The whole text's width is the content width: it doesn't change while
    // scrolling, so neither does the horizontal scroll range.
    let text_w = if wrap {
        0.0
    } else {
        let s: &str = display.text.as_str();
        layouter(ui, &s, f32::INFINITY).size().x + if any_folded { 120.0 } else { 24.0 }
    };

    let salt = if filtered { "body-filter-scroll" } else { "body-scroll" };
    let pending_scroll = state.pending_scroll.take();
    let menu = state.menu.clone().filter(|m| m.key == doc.key);
    let mut new_menu: Option<Option<MenuTarget>> = None;
    let mut toggle = None;
    let mut action = None;
    code_frame(ui).show(ui, |ui| {
        let mut area = egui::ScrollArea::both().auto_shrink([false; 2]).id_salt(salt);
        if let Some(off) = pending_scroll {
            area = area.scroll_offset(off);
        }
        area.show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add_space(6.0);
            let out = ui
                .horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(text_x);
                    let mut s: &str = display.text.as_str();
                    let mut edit = egui::TextEdit::multiline(&mut s)
                        .id_salt((salt, "text"))
                        .code_editor()
                        .frame(egui::Frame::NONE)
                        .margin(egui::Margin::ZERO)
                        .desired_width(if wrap { ui.available_width() } else { f32::INFINITY })
                        .layouter(&mut layouter);
                    if !wrap {
                        edit = edit.min_size(vec2(text_w, 0.0));
                    }
                    edit.show(ui)
                })
                .inner;
            ui.add_space(8.0);

            let galley = out.galley.clone();
            let gpos = out.galley_pos;
            let clip = ui.clip_rect();
            let vis = visible_lines(&galley, gpos, clip);
            let gid = ui.scope_id().with((salt, "gutter"));
            let painter = ui.painter().clone();

            // The value under a right click (JSON only).
            let text_resp = out.response.response.clone();
            if text_resp.secondary_clicked()
                && let Some(json) = &doc.json
            {
                new_menu = Some(
                    text_resp.interact_pointer_pos().and_then(|pos| line_at_y(&galley, gpos, pos.y)).and_then(|line| {
                        json.target(display.orig(line)).map(|(path, value)| MenuTarget {
                            key: doc.key,
                            line,
                            path,
                            value,
                        })
                    }),
                );
            }
            let menu = match &new_menu {
                Some(m) => m.clone(),
                None => menu.clone(),
            };
            if let Some(m) = &menu {
                let open = text_resp.context_menu(|ui| action = value_menu(ui, &m.path, &m.value, None));
                if open.is_some()
                    && let Some(v) = vis.iter().find(|v| v.line == m.line)
                {
                    let band = Rect::from_x_y_ranges(clip.x_range(), v.first.top()..=v.last.bottom());
                    painter.rect_filled(band, 0.0, theme::soft(pal.accent, if dark { 0.14 } else { 0.08 }));
                }
            }

            // Folded lines: a chip on the ellipsis and the count after the line.
            for v in &vis {
                let Some(dl) = display.lines.get(v.line) else { continue };
                let Some(ri) = dl.collapsed else { continue };
                let Some(region) = doc.folds.as_ref().and_then(|f| f.regions.get(ri as usize)) else { continue };
                let pre = display.text[dl.start..dl.ellipsis].chars().count();
                let a = galley.pos_from_cursor(egui::text::CCursor::new(v.char_start + pre));
                let b = galley.pos_from_cursor(egui::text::CCursor::new(v.char_start + pre + 1));
                let chip =
                    Rect::from_min_max(pos2(a.left() - 3.0, a.top() + 1.0), pos2(b.left() + 3.0, a.bottom() - 1.0))
                        .translate(gpos.to_vec2());
                let chip_resp =
                    ui.interact(chip, gid.with(("chip", v.line)), Sense::click()).on_hover_text(tr("Expand"));
                painter.rect_filled(
                    chip,
                    CornerRadius::same(4),
                    theme::soft(pal.accent, if chip_resp.hovered() { 0.30 } else { 0.14 }),
                );
                let label = count_label(region.count);
                let small = FontId::proportional(font_size - 1.5);
                let g = painter.layout_no_wrap(label, small, Color32::PLACEHOLDER);
                let at = pos2(v.last.right() + 10.0, v.last.center().y - g.size().y / 2.0);
                let count_rect = Rect::from_min_size(at, g.size()).expand(2.0);
                let count_resp = ui.interact(count_rect, gid.with(("count", v.line)), Sense::click());
                painter.galley(at, g, if count_resp.hovered() { pal.text_muted } else { pal.text_faint });
                if chip_resp.clicked() || count_resp.clicked() {
                    toggle = Some(ri);
                }
                if chip_resp.hovered() || count_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
            }

            // The gutter stays at the left edge when scrolling sideways.
            let gutter = Rect::from_min_max(clip.left_top(), pos2(clip.left() + gutter_w, clip.bottom()));
            let gutter_resp = ui.interact(gutter, gid, Sense::click_and_drag());
            let hover_y = gutter_resp.hover_pos().map(|p| p.y);
            painter.rect_filled(gutter, CornerRadius { nw: 7, sw: 7, ne: 0, se: 0 }, pal.surface);
            painter.vline(gutter.right() - 0.5, gutter.y_range(), Stroke::new(1.0, pal.border));
            let number_font = FontId::monospace(font_size - 1.0);
            let mut hovered_region = None;
            for v in &vis {
                let orig = display.orig(v.line);
                let region = match display.collapsed_at(v.line) {
                    Some(r) => Some((r, true)),
                    None => doc.folds.as_ref().and_then(|f| f.region_at(orig)).map(|r| (r, false)),
                };
                let hovered = hover_y.is_some_and(|y| y >= v.first.top() && y < v.last.bottom());
                let y = v.first.center().y;
                painter.text(
                    pos2(gutter.right() - 22.0, y),
                    Align2::RIGHT_CENTER,
                    (orig + 1).to_string(),
                    number_font.clone(),
                    if hovered { pal.text_muted } else { pal.text_faint },
                );
                if let Some((r, folded)) = region {
                    let caret = if folded { icons::CARET_RIGHT } else { icons::CARET_DOWN };
                    let color = if hovered {
                        pal.text
                    } else if folded {
                        pal.accent_text
                    } else {
                        pal.text_faint
                    };
                    painter.text(
                        pos2(gutter.right() - 11.0, y),
                        Align2::CENTER_CENTER,
                        caret,
                        theme::icon_font(11.0),
                        color,
                    );
                    if hovered {
                        hovered_region = Some(r);
                    }
                }
            }
            if hovered_region.is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                if gutter_resp.clicked() {
                    toggle = hovered_region;
                }
            }

            if let Some(ch) = scroll_char {
                let r = galley.pos_from_cursor(egui::text::CCursor::new(ch)).translate(gpos.to_vec2());
                let text_left = clip.left() + gutter_w + 8.0;
                // Only move sideways when the match is out of sight.
                let x = if r.left() >= text_left && r.right() + 8.0 <= clip.right() {
                    clip.x_range()
                } else {
                    egui::Rangef::new(r.left() - (clip.width() - gutter_w) / 2.0, r.left() + gutter_w)
                };
                ui.scroll_to_rect(Rect::from_x_y_ranges(x, r.y_range()), Some(Align::Center));
            }
        });
    });
    if let Some(m) = new_menu {
        state.menu = m;
    }
    if let Some(r) = toggle {
        state.toggle_region(doc, r);
    }
    action
}

/// Copy, path and request-tab actions for a JSON value.
fn value_menu(ui: &mut Ui, path: &str, value: &Value, key: Option<&str>) -> Option<BodyAction> {
    let p = theme::pal(ui);
    let mut action = None;
    ui.set_min_width(220.0);
    ui.add(egui::Label::new(RichText::new(path).monospace().small().color(p.text_muted)).truncate());
    ui.separator();
    if ui.button(kit::with_icon(icons::COPY, tr("Copy value"))).clicked() {
        let text = match value {
            Value::String(s) => s.clone(),
            v => serde_json::to_string_pretty(v).unwrap_or_default(),
        };
        ui.ctx().copy_text(text);
        ui.close();
    }
    if ui.button(kit::with_icon(icons::PATH, tr("Copy path"))).clicked() {
        ui.ctx().copy_text(path.to_string());
        ui.close();
    }
    if let Some(k) = key
        && ui.button(kit::with_icon(icons::KEY, tr("Copy key"))).clicked()
    {
        ui.ctx().copy_text(k.to_string());
        ui.close();
    }
    ui.separator();
    if ui.button(kit::with_icon(icons::BRACKETS_CURLY, tr("Set as variable…"))).clicked() {
        action = Some(BodyAction::SetVariable { path: path.to_string(), value: value.clone() });
        ui.close();
    }
    if ui.button(kit::with_icon(icons::TEST_TUBE, tr("Add test for this value"))).clicked() {
        action = Some(BodyAction::AddTest { path: path.to_string(), value: value.clone() });
        ui.close();
    }
    action
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
        _ if is_yaml(resp.content_type()) => "yaml",
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

/// Large texts: only the visible rows are read and laid out. Every row is
/// drawn at a fixed place in a content as wide as the longest row of the
/// whole body, so scrolling down never changes the horizontal range.
fn rows_viewer(ui: &mut Ui, state: &mut BodyState, source: &Source, index: &LineIndex, lang: Lang, salt: &str) {
    let dark = ui.visuals().dark_mode;
    let font_size = mono_size(ui);
    let font = FontId::monospace(font_size);
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 1.0;
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
    let rows = index.rows();
    let pal = theme::pal(ui);
    let search = state.search.clone();
    ui.label(
        RichText::new(trf("{} rows · {}", &[&rows, &merpati_core::util::human_bytes(index.total)]))
            .small()
            .color(pal.text_muted),
    );
    ui.add_space(4.0);
    let gutter_w = rows.to_string().len().max(2) as f32 * char_w + 18.0;
    let text_x = gutter_w + 10.0;
    let text_w = (index.max_cols as f32 * char_w).max(state.rows_width) + 24.0;
    let content_w = text_x + text_w;
    let mut area = egui::ScrollArea::both().auto_shrink([false; 2]).id_salt(("rows-scroll", salt));
    if std::mem::take(&mut state.jump)
        && let Some(r) = find_match_row(source, index, &search, state.match_cursor)
    {
        area = area.vertical_scroll_offset((r as f32 * row_h - 80.0).max(0.0));
    }
    if let Some(off) = state.pending_scroll.take() {
        area = area.scroll_offset(off);
    }
    let mut widest = state.rows_width;
    code_frame(ui).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        area.show_rows(ui, row_h, rows, |ui, range| {
            ui.set_min_width(content_w);
            let clip = ui.clip_rect();
            let texts = read_rows(source, index, range.clone());
            let mut placed = Vec::with_capacity(texts.len());
            for (row, text) in range.zip(texts) {
                let (rect, _) = ui.allocate_exact_size(vec2(content_w, row_h), Sense::hover());
                let job = highlight::highlight(HlKey {
                    text: &text,
                    lang,
                    dark,
                    search: &search,
                    vars: None,
                    font_size,
                    wrap: false,
                });
                let text_rect = Rect::from_min_size(pos2(rect.left() + text_x, rect.top()), vec2(text_w, row_h));
                let label = ui
                    .scope_builder(
                        egui::UiBuilder::new().max_rect(text_rect).layout(Layout::left_to_right(Align::Center)),
                        |ui| ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend)),
                    )
                    .inner;
                widest = widest.max(label.rect.width());
                placed.push((row, rect));
            }
            let gutter = Rect::from_min_max(clip.left_top(), pos2(clip.left() + gutter_w, clip.bottom()));
            ui.interact(gutter, ui.scope_id().with("rows-gutter"), Sense::click_and_drag());
            let painter = ui.painter();
            painter.rect_filled(gutter, CornerRadius { nw: 7, sw: 7, ne: 0, se: 0 }, pal.surface);
            painter.vline(gutter.right() - 0.5, gutter.y_range(), Stroke::new(1.0, pal.border));
            for (row, rect) in placed {
                painter.text(
                    pos2(gutter.right() - 8.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    (row + 1).to_string(),
                    FontId::monospace(font_size - 1.0),
                    pal.text_faint,
                );
            }
        });
    });
    // Rows with glyphs wider than the estimate only ever widen the content.
    if widest > state.rows_width + 0.5 && widest > index.max_cols as f32 * char_w {
        state.rows_width = widest;
    }
}

/// Byte offset of the n-th (1-based, wrapping) case-insensitive match.
fn nth_match_byte(text: &str, search: &str, nth: usize) -> Option<usize> {
    // Lowercase char by char, remembering where in the original text each
    // byte of the lowercased text came from.
    let mut lower = String::with_capacity(text.len());
    let mut origin: Vec<u32> = Vec::with_capacity(text.len());
    for (bi, c) in text.char_indices() {
        for l in c.to_lowercase() {
            lower.push(l);
            origin.extend(std::iter::repeat_n(bi as u32, l.len_utf8()));
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
    origin.get(byte).map(|&b| b as usize)
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
        if kit::secondary(ui, Some(icons::DOWNLOAD_SIMPLE), tr("Save response to file")).clicked() {
            save_body(resp);
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

// ---------------------------------------------------------------------------
// Tree view
// ---------------------------------------------------------------------------

/// A top-level node of the tree: the body, or one filter match.
struct TreeRoot<'a> {
    /// Shown before the value (the path of a filter match).
    key: Option<String>,
    path: String,
    value: &'a Value,
}

/// One visible row of the JSON tree.
struct TreeRow<'a> {
    depth: usize,
    /// Object key or array index.
    key: Option<String>,
    /// `key` is an index or a path, not an object key.
    is_index: bool,
    value: &'a Value,
    path: String,
    open: bool,
    /// "… n more" row after the first 1000 children.
    more: Option<usize>,
}

fn more_row<'a>(n: usize, depth: usize) -> TreeRow<'a> {
    TreeRow { depth, key: None, is_index: false, value: &Value::Null, path: String::new(), open: false, more: Some(n) }
}

#[allow(clippy::too_many_arguments)]
fn flatten_tree<'a>(
    opened: &HashMap<String, bool>,
    level: usize,
    v: &'a Value,
    key: Option<String>,
    is_index: bool,
    path: String,
    depth: usize,
    out: &mut Vec<TreeRow<'a>>,
) {
    let container = matches!(v, Value::Object(_) | Value::Array(_));
    let open = container && opened.get(&path).copied().unwrap_or(depth < level);
    out.push(TreeRow { depth, key, is_index, value: v, path: path.clone(), open, more: None });
    if !open {
        return;
    }
    match v {
        Value::Object(map) => {
            for (i, (k, child)) in map.iter().enumerate() {
                if i >= TREE_LIMIT {
                    out.push(more_row(map.len() - TREE_LIMIT, depth + 1));
                    break;
                }
                let mut child_path = path.clone();
                jsonpath::push_key(&mut child_path, k);
                flatten_tree(opened, level, child, Some(k.clone()), false, child_path, depth + 1, out);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                if i >= TREE_LIMIT {
                    out.push(more_row(items.len() - TREE_LIMIT, depth + 1));
                    break;
                }
                let mut child_path = path.clone();
                jsonpath::push_index(&mut child_path, i);
                flatten_tree(opened, level, child, Some(i.to_string()), true, child_path, depth + 1, out);
            }
        }
        _ => {}
    }
}

/// The JSON body as a tree: carets, indent guides, type-colored values,
/// counts on containers; click a container to fold it, right-click a value
/// for its actions.
fn json_tree_view(
    ui: &mut Ui,
    state: &mut BodyState,
    roots: &[TreeRoot],
    more: usize,
    search: &str,
) -> Option<BodyAction> {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let colors = highlight::json_colors(dark);
    let level = state.tree_level.unwrap_or(2);
    let mut rows = Vec::new();
    for r in roots {
        flatten_tree(&state.tree_open, level, r.value, r.key.clone(), r.key.is_some(), r.path.clone(), 0, &mut rows);
    }
    if more > 0 {
        rows.push(more_row(more, 0));
    }
    let font_size = mono_size(ui);
    let font = FontId::monospace(font_size);
    let row_h = 22.0;
    const INDENT: f32 = 16.0;
    let needle = search.to_lowercase();
    let mut toggle: Option<(String, bool)> = None;
    let mut action = None;
    let mut area = egui::ScrollArea::both().auto_shrink([false; 2]).id_salt("json-tree");
    if let Some(off) = state.pending_scroll.take() {
        area = area.scroll_offset(off);
    }
    code_frame(ui).inner_margin(egui::Margin::symmetric(4, 6)).show(ui, |ui| {
        kit::show_rows(area, ui, row_h, rows.len(), |ui, range| {
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
                        FontId::proportional(font_size),
                        p.text_muted,
                    );
                    continue;
                }
                let container = matches!(row.value, Value::Object(_) | Value::Array(_));
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
                    Value::Object(m) => {
                        let preview = if row.open {
                            String::new()
                        } else {
                            let keys: Vec<&str> = m.keys().take(4).map(String::as_str).collect();
                            format!(" {}{}", keys.join(", "), if m.len() > 4 { ", …" } else { "" })
                        };
                        (
                            count_label(Count::Keys(m.len())),
                            if row.open { "{".to_string() } else { format!("{{{preview} }}") },
                            colors.punct,
                        )
                    }
                    Value::Array(a) => (
                        count_label(Count::Items(a.len())),
                        if row.open { "[".to_string() } else { "[…]".to_string() },
                        colors.punct,
                    ),
                    Value::String(_) => (String::new(), row.value.to_string(), colors.string),
                    Value::Number(_) => (String::new(), row.value.to_string(), colors.number),
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
                        egui::TextFormat::simple(FontId::proportional(font_size - 1.0), p.text_faint),
                    );
                }
                job.wrap = egui::text::TextWrapping::truncate_at_width((rect.right() - x0 - 24.0).max(40.0));
                let hovered = resp.hovered() || resp.context_menu_opened();
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
                    let key = row.key.as_deref().filter(|_| !row.is_index);
                    if let Some(a) = value_menu(ui, &row.path, row.value, key) {
                        action = Some(a);
                    }
                });
            }
        });
    });
    if let Some((path, open)) = toggle {
        state.tree_open.insert(path, open);
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn nth_match_char(text: &str, search: &str, nth: usize) -> Option<usize> {
        nth_match_byte(text, search, nth).map(|b| text[..b].chars().count())
    }

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

    /// The row viewer's content width comes from the longest row of the
    /// whole body, counted in characters (not bytes), before any row is drawn.
    #[test]
    fn index_knows_the_widest_row() {
        let ix = LineIndex::from_bytes("ab\nhéllo wörld\n\tx\n".as_bytes());
        assert_eq!(ix.max_cols, 11);
        let ix = LineIndex::from_bytes(b"\t\tx");
        assert_eq!(ix.max_cols, 9);
        let long = vec![b'x'; MAX_ROW_BYTES * 2 + 10];
        assert_eq!(LineIndex::from_bytes(&long).max_cols, MAX_ROW_BYTES);
        let path = tempfile_dir().join("w.txt");
        std::fs::write(&path, "short\nthe longest row ★\nmid\n").unwrap();
        assert_eq!(LineIndex::from_file(&path).unwrap().max_cols, 17);
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
        assert_eq!(nth_match_byte("é é", "É", 2), Some(3));
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

    fn sample() -> Value {
        json!({
            "status": "ok",
            "data": [
                {"id": 1, "address": {"city": "Jakarta", "zip": "10110"}, "tags": ["a", "b"]},
                {"id": 2, "address": {}, "tags": []}
            ],
            "meta": {"total": 2}
        })
    }

    fn regions_of(text: &str, lang: Lang) -> Vec<(u32, u32, u16, bool, Count)> {
        Folds::compute(text, lang).regions.iter().map(|r| (r.start, r.end, r.depth, r.tail, r.count)).collect()
    }

    #[test]
    fn json_blocks_fold_with_counts() {
        let text = highlight::pretty_json_value(&sample());
        // 0 {  1 "status"  2 "data": [  3 {  4 "id"  5 "address": {  6 city  7 zip  8 },  9 "tags": [
        // 10 "a"  11 "b"  12 ]  13 },  14 {  15 "id"  16 "address": {},  17 "tags": []  18 }  19 ],
        // 20 "meta": {  21 "total"  22 }  23 }
        assert_eq!(
            regions_of(&text, Lang::Json),
            vec![
                (0, 23, 0, true, Count::Keys(3)),
                (2, 19, 1, true, Count::Items(2)),
                (3, 13, 2, true, Count::Keys(3)),
                (5, 8, 3, true, Count::Keys(2)),
                (9, 12, 3, true, Count::Items(2)),
                (14, 18, 2, true, Count::Keys(3)),
                (20, 22, 1, true, Count::Keys(1)),
            ]
        );
    }

    #[test]
    fn folded_lines_read_like_the_block() {
        let doc = Doc::new(highlight::pretty_json_value(&sample()), Lang::Json, None, true);
        let folds = doc.folds.clone().unwrap();
        let mut collapsed = vec![false; folds.regions.len()];
        // Fold "address" of the first item and the "meta" object.
        collapsed[3] = true;
        collapsed[6] = true;
        let d = Display::new(&doc.text, Some(&folds), &collapsed);
        let lines: Vec<&str> = d.text.lines().collect();
        assert_eq!(lines[5], "      \"address\": { … },");
        assert_eq!(lines[6], "      \"tags\": [");
        assert_eq!(lines[17], "  \"meta\": { … }");
        assert_eq!(lines.len(), 24 - 3 - 2);
        assert_eq!(d.orig(5), 5);
        assert_eq!(d.orig(6), 9);
        assert_eq!(d.collapsed_at(5), Some(3));
        assert_eq!(&d.text[d.lines[5].ellipsis..d.lines[5].ellipsis + 3], "…");
        // Folding everything leaves one line.
        let d = Display::new(&doc.text, Some(&folds), &vec![true; folds.regions.len()]);
        assert_eq!(d.text.as_str(), "{ … }");
        // Nothing folded: the text itself.
        let d = Display::new(&doc.text, Some(&folds), &vec![false; folds.regions.len()]);
        assert!(Arc::ptr_eq(&d.text, &doc.text));
    }

    /// A search match inside a folded block unfolds it, and the jump lands
    /// on the right character of the shown text.
    #[test]
    fn jumping_to_a_hidden_match_unfolds_it() {
        let doc = Doc::new(highlight::pretty_json_value(&sample()), Lang::Json, None, true);
        let folds = doc.folds.clone().unwrap();
        let mut collapsed = vec![false; folds.regions.len()];
        collapsed[1] = true; // "data"
        collapsed[3] = true; // "address" inside it
        let byte = nth_match_byte(&doc.text, "jakarta", 1).unwrap();
        assert!(folds.reveal(folds.line_of(byte), &mut collapsed));
        assert!(!collapsed[1] && !collapsed[3]);
        let d = Display::new(&doc.text, Some(&folds), &collapsed);
        let at = d.char_of(&doc, byte);
        assert!(d.text.chars().skip(at).collect::<String>().starts_with("Jakarta"));
        // With "meta" folded the match before it keeps its place.
        let mut collapsed = vec![false; folds.regions.len()];
        collapsed[6] = true;
        let d = Display::new(&doc.text, Some(&folds), &collapsed);
        let byte = nth_match_byte(&doc.text, "\"zip\"", 1).unwrap();
        assert!(d.text.chars().skip(d.char_of(&doc, byte)).collect::<String>().starts_with("\"zip\""));
    }

    #[test]
    fn xml_and_yaml_fold_by_indentation() {
        let xml = highlight::pretty_xml("<a><b x=\"1\">t</b><c/><d><e>1</e><e>2</e></d></a>");
        // 0 <a>  1 <b x="1">t</b>  2 <c/>  3 <d>  4 <e>1</e>  5 <e>2</e>  6 </d>  7 </a>
        assert_eq!(
            regions_of(&xml, Lang::Xml),
            vec![(0, 7, 0, true, Count::Children(3)), (3, 6, 1, true, Count::Children(2))]
        );
        let html = highlight::pretty_xml("<ul><li>One</li><li>Two <b>2</b></li></ul>");
        assert_eq!(
            regions_of(&html, Lang::Xml),
            vec![(0, 6, 0, true, Count::Children(2)), (2, 5, 1, true, Count::Children(2))]
        );
        let yaml = "info:\n  title: x\n  contact:\n    name: m\nservers:\n  - url: a\n    desc: b\n  - url: c\ntags:\n- name: u\n- name: v\nend: 1\n";
        assert_eq!(
            regions_of(yaml, Lang::Yaml),
            vec![
                (0, 3, 0, false, Count::Keys(2)),
                (2, 3, 1, false, Count::Keys(1)),
                (4, 7, 0, false, Count::Items(2)),
                (5, 6, 1, false, Count::Keys(1)),
                (8, 10, 0, false, Count::Items(2)),
            ]
        );
        let doc = Doc::new(yaml.to_string(), Lang::Yaml, None, true);
        let folds = doc.folds.clone().unwrap();
        let mut collapsed = vec![false; folds.regions.len()];
        collapsed[2] = true;
        let d = Display::new(&doc.text, Some(&folds), &collapsed);
        assert!(d.text.contains("servers: …\ntags:"));
        // Plain text with a closing brace keeps it on the folded line.
        let js = "function f() {\n  a();\n  b();\n}\nx();";
        assert_eq!(regions_of(js, Lang::JavaScript), vec![(0, 3, 0, true, Count::Lines(2))]);
        assert!(regions_of("a\nb\nc", Lang::Text).is_empty());
    }

    /// Right click names the value on the line, also in a filter result.
    #[test]
    fn lines_map_to_values_and_paths() {
        let root = Arc::new(sample());
        let doc = DocJson { root: root.clone(), base: Base::Root };
        assert_eq!(doc.target(0), Some(("$".to_string(), (*root).clone())));
        assert_eq!(doc.target(1), Some(("$.status".to_string(), json!("ok"))));
        assert_eq!(doc.target(4), Some(("$.data[0].id".to_string(), json!(1))));
        assert_eq!(doc.target(6), Some(("$.data[0].address.city".to_string(), json!("Jakarta"))));
        // A closing line names its container.
        assert_eq!(doc.target(8).map(|t| t.0), Some("$.data[0].address".to_string()));
        assert_eq!(doc.target(16), Some(("$.data[1].address".to_string(), json!({}))));
        assert_eq!(doc.target(21), Some(("$.meta.total".to_string(), json!(2))));
        assert_eq!(doc.target(23).map(|t| t.0), Some("$".to_string()));
        assert_eq!(doc.target(24), None);

        // Several matches print as an array of values.
        let result = run_filter(&root, "$.data[*].address", None);
        let ok = result.ok.unwrap();
        assert_eq!((ok.total, ok.shown), (2, 2));
        let fdoc = ok.doc.unwrap();
        assert_eq!(fdoc.text.as_str(), "[\n  {\n    \"city\": \"Jakarta\",\n    \"zip\": \"10110\"\n  },\n  {}\n]");
        let json = fdoc.json.unwrap();
        assert_eq!(json.target(0), None);
        assert_eq!(json.target(1).map(|t| t.0), Some("$.data[0].address".to_string()));
        assert_eq!(json.target(3), Some(("$.data[0].address.zip".to_string(), json!("10110"))));
        assert_eq!(json.target(5), Some(("$.data[1].address".to_string(), json!({}))));
        assert_eq!(json.target(6), None);

        // One match prints the value itself.
        let ok = run_filter(&root, "$.data[0].tags", None).ok.unwrap();
        let fdoc = ok.doc.unwrap();
        assert_eq!(fdoc.text.as_str(), "[\n  \"a\",\n  \"b\"\n]");
        assert_eq!(fdoc.json.unwrap().target(2), Some(("$.data[0].tags[1]".to_string(), json!("b"))));

        // An invalid path keeps the last result and says why.
        let prev = run_filter(&root, "$.meta", None).ok;
        let bad = run_filter(&root, "$.meta[", prev);
        assert_eq!(bad.error.map(|e| e.message), Some(jsonpath::ERR_END));
        assert_eq!(bad.ok.map(|o| o.total), Some(1));
        let none = run_filter(&root, "$.nothing", None).ok.unwrap();
        assert_eq!(none.total, 0);
        assert!(none.doc.is_none());
    }

    #[test]
    fn fold_commands_and_state() {
        let body = serde_json::to_vec(&sample()).unwrap();
        let resp = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: ResponseBody::Memory(body.into()),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut state = BodyState::default();
        state.prepare(&resp, 1 << 20, &ctx);
        let doc = state.current_doc().unwrap();
        assert!(view_caps(&state).fold && view_caps(&state).filter);
        state.apply_fold(FoldCmd::Level(1));
        let d = state.fold_view(&doc).display(&doc);
        assert_eq!(d.text.as_str(), "{\n  \"status\": \"ok\",\n  \"data\": [ … ],\n  \"meta\": { … }\n}");
        state.apply_fold(FoldCmd::CollapseAll);
        assert_eq!(state.fold_view(&doc).display(&doc).text.as_str(), "{ … }");
        state.apply_fold(FoldCmd::ExpandAll);
        assert!(Arc::ptr_eq(&state.fold_view(&doc).display(&doc).text, &doc.text));
        state.toggle_region(&doc, 0);
        assert_eq!(state.fold_view(&doc).display(&doc).text.as_str(), "{ … }");

        // The filter replaces the text while open; the fold state of the body stays.
        state.set_filter("$..city");
        state.update_filter();
        assert_eq!(state.current_doc().unwrap().text.as_str(), "\"Jakarta\"");
        state.filter_open = false;
        assert_eq!(state.current_doc().unwrap().key, doc.key);
        assert_eq!(state.fold_view(&doc).display(&doc).text.as_str(), "{ … }");

        // The tree view uses levels.
        state.view = BodyView::Tree;
        state.prepare(&resp, 1 << 20, &ctx);
        state.apply_fold(FoldCmd::Level(1));
        assert_eq!(state.tree_level, Some(1));

        // A YAML body is highlighted and folded as YAML.
        let yaml = HttpResponse {
            headers: vec![("Content-Type".into(), "application/yaml".into())],
            body: ResponseBody::Memory(b"a:\n  b: 1\n".to_vec().into()),
            ..Default::default()
        };
        let mut state = BodyState::default();
        state.prepare(&yaml, 1 << 20, &ctx);
        let doc = state.current_doc().unwrap();
        assert_eq!(doc.lang, Lang::Yaml);
        assert!(doc.foldable());
        assert!(!view_caps(&state).filter);
    }

    /// The JSONPath error messages shown in the filter bar are translated.
    #[test]
    fn jsonpath_errors_are_translated() {
        let table = include_str!("../i18n/id.tsv");
        for m in jsonpath::ERROR_MESSAGES {
            assert!(table.lines().any(|l| l.split('\t').next() == Some(m)), "no Indonesian entry for {m:?}");
        }
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("merpati-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
