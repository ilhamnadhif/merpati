//! CSS Grid: track lists, placement and track sizing.
//!
//! Supported: `grid-template-columns/rows` with lengths, percentages, `fr`,
//! `auto`, `min-content`, `max-content`, `minmax()`, `fit-content()` and
//! `repeat(n | auto-fill | auto-fit, ...)`; `grid-auto-columns/rows`;
//! `grid-auto-flow: row | column` (`dense` is ignored); `grid-template-areas`;
//! `grid-row`/`grid-column`/`grid-area` with line numbers (negative ones
//! count from the end of the explicit grid), `span n` and area names.
//!
//! Simplifications: line names other than area names are ignored; rows are
//! sized by their content as the grid is drawn, and `fr` rows are stretched
//! on the next frame (to the space left by a definite height, otherwise so
//! that `1fr` is the largest content height per `fr`); items spanning several
//! tracks spread their size evenly.

use crate::css::{Len, parse_length, split_top_level, tokens};

/// One end of a track size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Breadth {
    /// A length or percentage of the grid's content box.
    Len(Len),
    /// `fr` (only as the maximum).
    Fr(f32),
    Auto,
    MinContent,
    MaxContent,
    /// `fit-content(limit)` (only as the maximum).
    FitContent(Len),
}

/// A track size: `minmax(min, max)`; `100px` is `minmax(100px, 100px)` and
/// `1fr` is `minmax(auto, 1fr)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackSize {
    pub min: Breadth,
    pub max: Breadth,
}

impl TrackSize {
    pub const AUTO: TrackSize = TrackSize { min: Breadth::Auto, max: Breadth::Auto };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    Count(u16),
    AutoFill,
    AutoFit,
}

/// An entry of `grid-template-columns`/`rows`.
#[derive(Clone, Debug, PartialEq)]
pub enum Tracks {
    One(TrackSize),
    Repeat(Repeat, Vec<TrackSize>),
}

/// One end of an item's placement on an axis.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum GridLine {
    #[default]
    Auto,
    /// 1-based line number; negative numbers count from the last line.
    Line(i32),
    Span(u16),
    /// A `grid-template-areas` name (`header`, `header-start`, `header-end`).
    Area(String),
}

/// `grid-row-start`, `grid-row-end`, `grid-column-start`, `grid-column-end`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GridPlacement {
    pub row_start: GridLine,
    pub row_end: GridLine,
    pub column_start: GridLine,
    pub column_end: GridLine,
}

impl GridPlacement {
    pub fn is_auto(&self) -> bool {
        *self == GridPlacement::default()
    }
}

/// The grid container's template.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GridTemplate {
    pub columns: Vec<Tracks>,
    pub rows: Vec<Tracks>,
    /// Sizes of implicit tracks (cycled); empty means `auto`.
    pub auto_columns: Vec<TrackSize>,
    pub auto_rows: Vec<TrackSize>,
    /// `grid-template-areas`, row by row (`.` for unnamed cells).
    pub areas: Vec<Vec<String>>,
    /// `grid-auto-flow: column`.
    pub column_flow: bool,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Removes `[line names]`.
fn strip_line_names(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut depth = 0;
    for c in v.chars() {
        match c {
            '[' => depth += 1,
            ']' if depth > 0 => {
                depth -= 1;
                out.push(' ');
            }
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn parse_breadth(t: &str, font_size: f32) -> Option<Breadth> {
    let t = t.trim();
    Some(match t {
        "auto" => Breadth::Auto,
        "min-content" => Breadth::MinContent,
        "max-content" => Breadth::MaxContent,
        _ => {
            if let Some(fr) = t.strip_suffix("fr") {
                let f: f32 = fr.trim().parse().ok()?;
                return (f >= 0.0 && f.is_finite()).then_some(Breadth::Fr(f));
            }
            if let Some(arg) = t.strip_prefix("fit-content(").and_then(|r| r.strip_suffix(')')) {
                return Some(Breadth::FitContent(parse_length(arg, font_size).filter(|l| *l != Len::Auto)?));
            }
            match parse_length(t, font_size)? {
                Len::Auto => Breadth::Auto,
                l => Breadth::Len(l),
            }
        }
    })
}

/// A single track size (`100px`, `1fr`, `minmax(200px, 1fr)`, ...).
pub(crate) fn parse_track_size(t: &str, font_size: f32) -> Option<TrackSize> {
    let t = t.trim();
    if let Some(args) = t.strip_prefix("minmax(").and_then(|r| r.strip_suffix(')')) {
        let parts = split_top_level(args, ',');
        let [a, b] = parts.as_slice() else { return None };
        let min = match parse_breadth(a, font_size)? {
            // A flexible minimum is invalid; `auto` is the closest.
            Breadth::Fr(_) | Breadth::FitContent(_) => Breadth::Auto,
            m => m,
        };
        return Some(TrackSize { min, max: parse_breadth(b, font_size)? });
    }
    Some(match parse_breadth(t, font_size)? {
        Breadth::Fr(f) => TrackSize { min: Breadth::Auto, max: Breadth::Fr(f) },
        Breadth::FitContent(l) => TrackSize { min: Breadth::Auto, max: Breadth::FitContent(l) },
        b => TrackSize { min: b, max: b },
    })
}

/// `grid-template-columns`/`rows`. `None` for invalid values.
pub(crate) fn parse_track_list(v: &str, font_size: f32) -> Option<Vec<Tracks>> {
    let v = strip_line_names(v);
    let v = v.trim();
    if v == "none" || v.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut auto_repeats = 0;
    for t in tokens(v) {
        if let Some(args) = t.strip_prefix("repeat(").and_then(|r| r.strip_suffix(')')) {
            let (count, list) = args.split_once(',')?;
            let count = match count.trim() {
                "auto-fill" => Repeat::AutoFill,
                "auto-fit" => Repeat::AutoFit,
                n => Repeat::Count(n.parse::<u16>().ok().filter(|n| *n > 0)?.min(1000)),
            };
            let sizes: Option<Vec<TrackSize>> =
                tokens(list).into_iter().map(|t| parse_track_size(t, font_size)).collect();
            let sizes = sizes?;
            if sizes.is_empty() {
                return None;
            }
            if matches!(count, Repeat::AutoFill | Repeat::AutoFit) {
                auto_repeats += 1;
            }
            out.push(Tracks::Repeat(count, sizes));
        } else {
            out.push(Tracks::One(parse_track_size(t, font_size)?));
        }
    }
    // Only one automatic repetition is allowed.
    (auto_repeats <= 1).then_some(out)
}

/// `grid-auto-columns`/`rows`.
pub(crate) fn parse_auto_tracks(v: &str, font_size: f32) -> Option<Vec<TrackSize>> {
    tokens(v).into_iter().map(|t| parse_track_size(t, font_size)).collect()
}

/// `grid-template-areas: "a a" "b c"`. `None` for invalid values.
pub(crate) fn parse_areas(v: &str) -> Option<Vec<Vec<String>>> {
    if v.trim() == "none" {
        return Some(Vec::new());
    }
    let mut rows = Vec::new();
    let mut rest = v;
    while let Some(start) = rest.find(['"', '\'']) {
        let quote = rest.as_bytes()[start] as char;
        let after = &rest[start + 1..];
        let end = after.find(quote)?;
        // `..` is one unnamed cell, like `.`.
        let row: Vec<String> = after[..end]
            .split_whitespace()
            .map(|n| if n.chars().all(|c| c == '.') { ".".to_string() } else { n.to_string() })
            .collect();
        if row.is_empty() {
            return None;
        }
        rows.push(row);
        rest = &after[end + 1..];
    }
    let width = rows.first().map(Vec::len)?;
    rows.iter().all(|r| r.len() == width).then_some(rows)
}

pub(crate) fn parse_line(v: &str) -> GridLine {
    let toks: Vec<&str> = v.split_whitespace().collect();
    match toks.as_slice() {
        ["auto"] | [] => GridLine::Auto,
        ["span"] => GridLine::Span(1),
        ["span", n] | [n, "span"] => match n.parse::<u16>() {
            Ok(n) if n > 0 => GridLine::Span(n.min(1000)),
            // `span name` is not supported.
            _ => GridLine::Span(1),
        },
        [n] => match n.parse::<i32>() {
            Ok(0) => GridLine::Auto,
            Ok(n) => GridLine::Line(n.clamp(-1000, 1000)),
            Err(_) => GridLine::Area(n.to_string()),
        },
        // `2 name` and other forms with line names: the number wins.
        _ => toks.iter().find_map(|t| t.parse::<i32>().ok()).filter(|n| *n != 0).map_or(GridLine::Auto, GridLine::Line),
    }
}

/// `grid-row`/`grid-column`: `start / end`.
pub(crate) fn parse_line_pair(v: &str) -> (GridLine, GridLine) {
    let parts = split_top_level(v, '/');
    let start = parse_line(parts[0].trim());
    let end = match parts.get(1) {
        Some(e) => parse_line(e.trim()),
        // A lone area name spans the whole area.
        None if matches!(start, GridLine::Area(_)) => start.clone(),
        None => GridLine::Auto,
    };
    (start, end)
}

/// `grid-area`: a name or `row-start / column-start / row-end / column-end`.
pub(crate) fn parse_area(v: &str) -> GridPlacement {
    let parts: Vec<GridLine> = split_top_level(v, '/').into_iter().map(|p| parse_line(p.trim())).collect();
    let name = |i: usize| match &parts[0] {
        GridLine::Area(_) => parts.get(i).cloned().unwrap_or_else(|| parts[0].clone()),
        _ => parts.get(i).cloned().unwrap_or_default(),
    };
    GridPlacement { row_start: parts[0].clone(), column_start: name(1), row_end: name(2), column_end: name(3) }
}

// ---------------------------------------------------------------------------
// Explicit tracks
// ---------------------------------------------------------------------------

/// The explicit tracks for a content box `avail` wide (or tall); also the
/// range of tracks from an `auto-fit` repetition (collapsed when empty).
pub(crate) fn expand_tracks(
    list: &[Tracks],
    avail: f32,
    gap: f32,
    to_px: impl Fn(Len, f32) -> Option<f32>,
) -> (Vec<TrackSize>, Option<std::ops::Range<usize>>) {
    let mut out = Vec::new();
    let mut fit = None;
    // A track's size when deciding how many times an automatic repetition fits.
    let fixed = |t: &TrackSize| {
        let of = |b: Breadth| match b {
            Breadth::Len(l) if avail.is_finite() => to_px(l, avail),
            Breadth::Len(Len::Px(p)) => to_px(Len::Px(p), 0.0),
            _ => None,
        };
        match (of(t.max), of(t.min)) {
            (Some(max), Some(min)) => Some(max.max(min)),
            (max, min) => max.or(min),
        }
    };
    let others: Vec<f32> = list
        .iter()
        .flat_map(|e| match e {
            Tracks::One(t) => vec![fixed(t).unwrap_or(0.0)],
            Tracks::Repeat(Repeat::Count(n), ts) => {
                std::iter::repeat_n(ts.iter().map(|t| fixed(t).unwrap_or(0.0)), *n as usize).flatten().collect()
            }
            Tracks::Repeat(..) => Vec::new(),
        })
        .collect();
    for entry in list {
        match entry {
            Tracks::One(t) => out.push(*t),
            Tracks::Repeat(Repeat::Count(n), ts) => {
                for _ in 0..*n {
                    out.extend_from_slice(ts);
                }
            }
            Tracks::Repeat(kind, ts) => {
                let sizes: Option<Vec<f32>> = ts.iter().map(fixed).collect();
                let count = match sizes {
                    Some(sizes) if avail.is_finite() => {
                        let one: f32 = sizes.iter().sum::<f32>() + gap * ts.len() as f32;
                        let used: f32 = others.iter().sum::<f32>() + gap * others.len() as f32;
                        if one > 0.0 { ((avail - used + gap) / one).floor().clamp(1.0, 1000.0) as usize } else { 1 }
                    }
                    _ => 1,
                };
                let start = out.len();
                for _ in 0..count {
                    out.extend_from_slice(ts);
                }
                if *kind == Repeat::AutoFit {
                    fit = Some(start..out.len());
                }
            }
        }
    }
    (out, fit)
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// A placed item: row and column line indices (0-based, end exclusive).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Cell {
    pub row: (usize, usize),
    pub col: (usize, usize),
}

enum Axis {
    Definite(usize, usize),
    Auto(usize),
}

/// Lines of the named area on one axis: (start, end).
fn area_lines(areas: &[Vec<String>], name: &str, columns: bool) -> Option<(usize, usize)> {
    let name = name.strip_suffix("-start").or_else(|| name.strip_suffix("-end")).unwrap_or(name);
    let mut range: Option<(usize, usize)> = None;
    for (r, row) in areas.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if cell == name {
                let i = if columns { c } else { r };
                range = Some(match range {
                    Some((a, b)) => (a.min(i), b.max(i + 1)),
                    None => (i, i + 1),
                });
            }
        }
    }
    range
}

fn resolve_axis(start: &GridLine, end: &GridLine, explicit: usize, areas: &[Vec<String>], columns: bool) -> Axis {
    // Line number -> 0-based line index (negative from the end of the explicit grid).
    let line = |l: &GridLine, is_end: bool| -> Option<usize> {
        match l {
            GridLine::Line(n) if *n > 0 => Some(*n as usize - 1),
            GridLine::Line(n) => Some((explicit as i32 + 1 + n).max(0) as usize),
            GridLine::Area(name) => {
                let (a, b) = area_lines(areas, name, columns)?;
                Some(if name.ends_with("-end") || (is_end && !name.ends_with("-start")) { b } else { a })
            }
            _ => None,
        }
    };
    let span = |l: &GridLine| match l {
        GridLine::Span(n) => Some(*n as usize),
        _ => None,
    };
    match (line(start, false), line(end, true)) {
        (Some(s), Some(e)) if e > s => Axis::Definite(s, e),
        (Some(s), Some(e)) if e < s => Axis::Definite(e, s),
        (Some(s), _) => Axis::Definite(s, s + span(end).unwrap_or(1)),
        (None, Some(e)) => {
            let n = span(start).unwrap_or(1);
            let e = e.max(n);
            Axis::Definite(e - n, e)
        }
        (None, None) => Axis::Auto(span(start).or(span(end)).unwrap_or(1)),
    }
}

/// Occupancy of grid cells, rows grow on demand.
struct Occupancy {
    cols: usize,
    cells: Vec<bool>,
}

impl Occupancy {
    fn fits(&self, r: usize, c: (usize, usize), rows: usize) -> bool {
        if c.1 > self.cols {
            return false;
        }
        (r..r + rows).all(|r| (c.0..c.1).all(|c| !self.cells.get(r * self.cols + c).copied().unwrap_or(false)))
    }

    fn mark(&mut self, r: (usize, usize), c: (usize, usize)) {
        let need = r.1 * self.cols;
        if self.cells.len() < need {
            self.cells.resize(need, false);
        }
        for r in r.0..r.1 {
            for c in c.0..c.1.min(self.cols) {
                self.cells[r * self.cols + c] = true;
            }
        }
    }
}

/// Places items (in document order) on a grid with `explicit_cols` x
/// `explicit_rows` explicit tracks. Returns the cells and the final number
/// of columns and rows (explicit plus implicit).
pub(crate) fn place(
    items: &[&GridPlacement],
    explicit_cols: usize,
    explicit_rows: usize,
    areas: &[Vec<String>],
    column_flow: bool,
) -> (Vec<Cell>, usize, usize) {
    // Work in (major, minor) = (row, column) for row flow, transposed for column flow.
    let axes: Vec<(Axis, Axis)> = items
        .iter()
        .map(|p| {
            let rows = resolve_axis(&p.row_start, &p.row_end, explicit_rows, areas, false);
            let cols = resolve_axis(&p.column_start, &p.column_end, explicit_cols, areas, true);
            if column_flow { (cols, rows) } else { (rows, cols) }
        })
        .collect();
    let explicit_minor = if column_flow { explicit_rows } else { explicit_cols };
    let size = |a: &Axis| match a {
        Axis::Definite(s, e) => e - s,
        Axis::Auto(n) => *n,
    };
    let end = |a: &Axis| match a {
        Axis::Definite(_, e) => *e,
        Axis::Auto(n) => *n,
    };
    let minor = axes.iter().map(|(_, m)| end(m)).max().unwrap_or(0).max(explicit_minor).max(1);
    let mut occ = Occupancy { cols: minor, cells: Vec::new() };
    let mut out = vec![Cell::default(); items.len()];
    let mut done = vec![false; items.len()];
    // 1. Fully definite items.
    for (i, (maj, min)) in axes.iter().enumerate() {
        if let (Axis::Definite(a, b), Axis::Definite(c, d)) = (maj, min) {
            occ.mark((*a, *b), (*c, *d));
            out[i] = Cell { row: (*a, *b), col: (*c, *d) };
            done[i] = true;
        }
    }
    // 2. Items locked to a major track: earliest free minor position.
    for (i, (maj, min)) in axes.iter().enumerate() {
        if let (Axis::Definite(a, b), Axis::Auto(n)) = (maj, min) {
            let mut c = 0;
            while c + n <= minor && !occ.fits(*a, (c, c + n), b - a) {
                c += 1;
            }
            let c = c.min(minor.saturating_sub(*n));
            occ.mark((*a, *b), (c, c + n));
            out[i] = Cell { row: (*a, *b), col: (c, c + n) };
            done[i] = true;
        }
    }
    // 3. The rest, with a sparse cursor.
    let (mut cr, mut cc) = (0usize, 0usize);
    for (i, (maj, min)) in axes.iter().enumerate() {
        if done[i] {
            continue;
        }
        let rows = size(maj);
        match min {
            Axis::Definite(c0, c1) => {
                if *c0 < cc {
                    cr += 1;
                }
                while !occ.fits(cr, (*c0, *c1), rows) {
                    cr += 1;
                }
                occ.mark((cr, cr + rows), (*c0, *c1));
                out[i] = Cell { row: (cr, cr + rows), col: (*c0, *c1) };
                cc = *c1;
            }
            Axis::Auto(n) => loop {
                if cc + n > minor {
                    cr += 1;
                    cc = 0;
                    continue;
                }
                if occ.fits(cr, (cc, cc + n), rows) {
                    occ.mark((cr, cr + rows), (cc, cc + n));
                    out[i] = Cell { row: (cr, cr + rows), col: (cc, cc + n) };
                    cc += n;
                    break;
                }
                cc += 1;
            },
        }
    }
    let explicit_major = if column_flow { explicit_cols } else { explicit_rows };
    let major = out.iter().map(|c| c.row.1).max().unwrap_or(0).max(explicit_major);
    if column_flow {
        for c in &mut out {
            std::mem::swap(&mut c.row, &mut c.col);
        }
        (out, major, minor)
    } else {
        (out, minor, major)
    }
}

// ---------------------------------------------------------------------------
// Track sizing
// ---------------------------------------------------------------------------

/// An item's size contribution to the tracks `span`: min- and max-content
/// (margin box, in points).
pub(crate) struct Contribution {
    pub span: (usize, usize),
    pub min: f32,
    pub max: f32,
}

fn content_based(b: Breadth) -> bool {
    matches!(b, Breadth::Auto | Breadth::MinContent | Breadth::MaxContent | Breadth::FitContent(_))
}

/// Track sizes (points) for `avail` points of free space (`INFINITY` for the
/// max-content size, `0` for the min-content size). `to_px` resolves lengths
/// (with percentages of `avail`). Tracks in `collapsed` get no size.
/// `stretch_auto`: free space left over grows `auto` tracks (the default
/// `justify-content: normal`).
pub(crate) fn size_tracks(
    tracks: &[TrackSize],
    items: &[Contribution],
    avail: f32,
    gap: f32,
    collapsed: &[bool],
    stretch_auto: bool,
    to_px: impl Fn(Len, f32) -> Option<f32>,
) -> Vec<f32> {
    let n = tracks.len();
    let of = if avail.is_finite() { avail } else { 0.0 };
    let len = |b: Breadth| match b {
        Breadth::Len(l) => to_px(l, of),
        _ => None,
    };
    let mut base = vec![0.0f32; n];
    let mut limit = vec![f32::INFINITY; n];
    let flex: Vec<f32> = tracks.iter().map(|t| if let Breadth::Fr(f) = t.max { f } else { 0.0 }).collect();
    for (i, t) in tracks.iter().enumerate() {
        if collapsed.get(i).copied().unwrap_or(false) {
            limit[i] = 0.0;
            continue;
        }
        base[i] = len(t.min).unwrap_or(0.0).max(0.0);
        if let Some(l) = len(t.max) {
            limit[i] = l.max(base[i]);
        }
    }
    let is_collapsed = |i: usize| collapsed.get(i).copied().unwrap_or(false);
    // Single-track items first, then spanning items spread their excess.
    let mut content_limit = vec![0.0f32; n];
    let mut has_items = vec![false; n];
    for it in items.iter().filter(|it| it.span.1 == it.span.0 + 1) {
        let i = it.span.0;
        if i >= n || is_collapsed(i) {
            continue;
        }
        has_items[i] = true;
        let t = tracks[i];
        match t.min {
            Breadth::Auto | Breadth::MinContent => base[i] = base[i].max(it.min),
            Breadth::MaxContent => base[i] = base[i].max(it.max),
            _ => {}
        }
        let c = match t.max {
            Breadth::MinContent => it.min,
            Breadth::FitContent(l) => it.max.min(to_px(l, of).unwrap_or(f32::INFINITY)).max(it.min),
            _ => it.max,
        };
        content_limit[i] = content_limit[i].max(c);
    }
    for it in items.iter().filter(|it| it.span.1 > it.span.0 + 1) {
        let range = it.span.0.min(n)..it.span.1.min(n);
        let open: Vec<usize> = range.clone().filter(|i| !is_collapsed(*i) && content_based(tracks[*i].min)).collect();
        if open.is_empty() {
            continue;
        }
        let inner = gap * (range.len().saturating_sub(1)) as f32;
        let sum: f32 = range.clone().map(|i| base[i]).sum();
        if it.min > sum + inner {
            let add = (it.min - sum - inner) / open.len() as f32;
            for &i in &open {
                base[i] += add;
            }
        }
        let sum_max: f32 = range.clone().map(|i| base[i].max(content_limit[i])).sum();
        if it.max > sum_max + inner {
            let add = (it.max - sum_max - inner) / open.len() as f32;
            for &i in &open {
                content_limit[i] = base[i].max(content_limit[i]) + add;
                has_items[i] = true;
            }
        }
    }
    for i in 0..n {
        if content_based(tracks[i].max) || flex[i] > 0.0 {
            limit[i] = if has_items[i] { content_limit[i] } else { 0.0 };
            if let Breadth::FitContent(l) = tracks[i].max
                && let Some(l) = to_px(l, of)
            {
                limit[i] = limit[i].min(l);
            }
        }
        if is_collapsed(i) {
            limit[i] = 0.0;
        }
        limit[i] = limit[i].max(base[i]);
    }
    let visible = (0..n).filter(|i| !is_collapsed(*i)).count();
    let gaps = gap * visible.saturating_sub(1) as f32;
    let mut size = base.clone();
    // Grow inflexible tracks towards their limits.
    let fixed_sum = |size: &[f32]| (0..n).filter(|i| flex[*i] == 0.0).map(|i| size[i]).sum::<f32>();
    let flex_base: f32 = (0..n).filter(|i| flex[*i] > 0.0).map(|i| base[i]).sum();
    if avail.is_infinite() {
        for i in 0..n {
            if flex[i] == 0.0 {
                size[i] = limit[i];
            }
        }
    } else {
        let free = avail - gaps - fixed_sum(&size) - flex_base;
        let room: f32 = (0..n).filter(|i| flex[*i] == 0.0).map(|i| limit[i] - base[i]).sum();
        if free > 0.0 && room > 0.0 {
            let k = (free / room).min(1.0);
            for i in 0..n {
                if flex[i] == 0.0 {
                    size[i] = base[i] + (limit[i] - base[i]) * k;
                }
            }
        }
    }
    // Flexible tracks share what is left (`fr`), but never shrink below their base.
    let flexible: Vec<usize> = (0..n).filter(|i| flex[*i] > 0.0 && !is_collapsed(*i)).collect();
    if !flexible.is_empty() {
        if avail.is_infinite() {
            // Max-content: the largest per-fr size of any flexible track.
            let fr = flexible.iter().map(|&i| limit[i] / flex[i].max(1e-3)).fold(0.0f32, f32::max);
            for &i in &flexible {
                size[i] = (fr * flex[i]).max(base[i]);
            }
        } else {
            let mut left: Vec<usize> = flexible.clone();
            let mut space = avail - gaps - fixed_sum(&size);
            loop {
                let factors: f32 = left.iter().map(|&i| flex[i]).sum::<f32>().max(1.0);
                let fr = space.max(0.0) / factors;
                let small: Vec<usize> = left.iter().copied().filter(|&i| base[i] > fr * flex[i]).collect();
                if small.is_empty() {
                    for &i in &left {
                        size[i] = fr * flex[i];
                    }
                    break;
                }
                for &i in &small {
                    size[i] = base[i];
                    space -= base[i];
                }
                left.retain(|i| !small.contains(i));
                if left.is_empty() {
                    break;
                }
            }
        }
    } else if stretch_auto && avail.is_finite() {
        let used: f32 = size.iter().sum::<f32>() + gaps;
        let autos: Vec<usize> = (0..n).filter(|i| tracks[*i].max == Breadth::Auto && !is_collapsed(*i)).collect();
        if used < avail && !autos.is_empty() {
            let add = (avail - used) / autos.len() as f32;
            for i in autos {
                size[i] += add;
            }
        }
    }
    size
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(l: Len, of: f32) -> Option<f32> {
        l.resolve(of)
    }

    fn sizes(list: &str, avail: f32, gap: f32, items: &[Contribution]) -> Vec<f32> {
        let list = parse_track_list(list, 16.0).expect("track list");
        let (tracks, _) = expand_tracks(&list, avail, gap, px);
        size_tracks(&tracks, items, avail, gap, &[], true, px)
    }

    #[test]
    fn parses_track_lists() {
        let l = parse_track_list("[a] 100px 1fr [b c] minmax(200px, 2fr) repeat(2, auto 10%)", 16.0).unwrap();
        assert_eq!(l.len(), 4);
        assert_eq!(
            l[0],
            Tracks::One(TrackSize { min: Breadth::Len(Len::Px(100.0)), max: Breadth::Len(Len::Px(100.0)) })
        );
        assert_eq!(l[1], Tracks::One(TrackSize { min: Breadth::Auto, max: Breadth::Fr(1.0) }));
        assert_eq!(l[2], Tracks::One(TrackSize { min: Breadth::Len(Len::Px(200.0)), max: Breadth::Fr(2.0) }));
        let Tracks::Repeat(Repeat::Count(2), ts) = &l[3] else { panic!("{l:?}") };
        assert_eq!(ts[1].min, Breadth::Len(Len::Percent(10.0)));
        assert!(parse_track_list("repeat(auto-fill, 100px) repeat(auto-fit, 1fr)", 16.0).is_none());
        assert!(parse_track_list("12 parsecs", 16.0).is_none());
        assert_eq!(parse_areas("'a a .' \"b c ..\"").unwrap(), [["a", "a", "."], ["b", "c", "."]]);
        assert!(parse_areas("'a a' 'b'").is_none());
        assert_eq!(parse_line_pair("2 / span 3"), (GridLine::Line(2), GridLine::Span(3)));
        assert_eq!(parse_line_pair("main"), (GridLine::Area("main".into()), GridLine::Area("main".into())));
        assert_eq!(parse_area("1 / 2 / 3").column_end, GridLine::Auto);
    }

    #[test]
    fn auto_fill_counts_repetitions() {
        let list = parse_track_list("repeat(auto-fill, minmax(200px, 1fr))", 16.0).unwrap();
        assert_eq!(expand_tracks(&list, 900.0, 10.0, px).0.len(), 4);
        assert_eq!(expand_tracks(&list, 420.0, 10.0, px).0.len(), 2);
        assert_eq!(expand_tracks(&list, 100.0, 10.0, px).0.len(), 1);
        let list = parse_track_list("100px repeat(auto-fit, 100px)", 16.0).unwrap();
        let (t, fit) = expand_tracks(&list, 330.0, 10.0, px);
        assert_eq!((t.len(), fit), (3, Some(1..3)));
    }

    #[test]
    fn sizes_fixed_fr_and_auto_tracks() {
        assert_eq!(sizes("100px 1fr 2fr", 400.0, 0.0, &[]), [100.0, 100.0, 200.0]);
        assert_eq!(sizes("1fr 1fr", 210.0, 10.0, &[]), [100.0, 100.0]);
        // `1fr` is `minmax(auto, 1fr)`: content wider than the share keeps its size.
        let wide = [Contribution { span: (0, 1), min: 150.0, max: 300.0 }];
        assert_eq!(sizes("1fr 1fr", 200.0, 0.0, &wide), [150.0, 50.0]);
        // `auto` tracks fit their content, then share the free space.
        let items = [Contribution { span: (0, 1), min: 40.0, max: 80.0 }];
        assert_eq!(sizes("auto 100px", 300.0, 0.0, &items), [200.0, 100.0]);
        assert_eq!(sizes("max-content 1fr", 300.0, 0.0, &items), [80.0, 220.0]);
        assert_eq!(sizes("25% 50%", 400.0, 0.0, &[]), [100.0, 200.0]);
        // Max-content size of the grid.
        let list = parse_track_list("1fr 2fr", 16.0).unwrap();
        let (tracks, _) = expand_tracks(&list, f32::INFINITY, 0.0, px);
        let items =
            [Contribution { span: (0, 1), min: 10.0, max: 60.0 }, Contribution { span: (1, 2), min: 10.0, max: 60.0 }];
        assert_eq!(size_tracks(&tracks, &items, f32::INFINITY, 0.0, &[], true, px), [60.0, 120.0]);
    }

    #[test]
    fn places_items_row_major() {
        let auto = GridPlacement::default();
        let span2 = GridPlacement { column_start: GridLine::Span(2), ..Default::default() };
        let pinned =
            GridPlacement { row_start: GridLine::Line(1), column_start: GridLine::Line(3), ..Default::default() };
        let items = [&auto, &span2, &auto, &pinned, &auto];
        let (cells, cols, rows) = place(&items, 3, 0, &[], false);
        assert_eq!((cols, rows), (3, 3));
        let at: Vec<((usize, usize), (usize, usize))> = cells.iter().map(|c| (c.row, c.col)).collect();
        // The pinned item claims row 1, column 3 first, so the span-2 item moves down.
        assert_eq!(at, [((0, 1), (0, 1)), ((1, 2), (0, 2)), ((1, 2), (2, 3)), ((0, 1), (2, 3)), ((2, 3), (0, 1))]);
    }

    #[test]
    fn places_named_areas_and_negative_lines() {
        let areas = parse_areas("'head head' 'side main'").unwrap();
        let main = GridPlacement {
            row_start: GridLine::Area("main".into()),
            row_end: GridLine::Area("main".into()),
            column_start: GridLine::Area("main".into()),
            column_end: GridLine::Area("main".into()),
        };
        let full =
            GridPlacement { column_start: GridLine::Line(1), column_end: GridLine::Line(-1), ..Default::default() };
        let (cells, cols, rows) = place(&[&main, &full], 2, 2, &areas, false);
        assert_eq!((cols, rows), (2, 2));
        assert_eq!(cells[0], Cell { row: (1, 2), col: (1, 2) });
        assert_eq!(cells[1], Cell { row: (0, 1), col: (0, 2) });
    }
}
