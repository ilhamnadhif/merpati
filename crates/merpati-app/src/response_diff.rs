//! "Compare" section of the response panel: the previous response of the
//! tab against the current one. A summary (status, time, size, headers)
//! and a line diff (Myers) of the bodies or the headers; JSON is compared
//! pretty-printed with sorted keys, so key order does not count.

use std::collections::{HashMap, HashSet};

use egui::{Align2, Color32, CornerRadius, RichText, Sense, Stroke, Ui, pos2, vec2};
use merpati_core::response::{BodyLanguage, HttpResponse};

use crate::i18n::{tr, trf};
use crate::{icons, kit, theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Same,
    Removed,
    Added,
}

/// One line of a diff: `a`/`b` are line indices in the old/new text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub op: Op,
    pub a: usize,
    pub b: usize,
}

/// Line diff of `a` and `b` (Myers, after trimming the common start and
/// end). `None` when more than `max_d` lines differ.
pub fn diff<T: Eq + std::hash::Hash>(a: &[T], b: &[T], max_d: usize) -> Option<Vec<Edit>> {
    // Lines as numbers, so comparing is cheap.
    let mut ids: HashMap<&T, u32> = HashMap::new();
    let mut intern = |l| {
        let n = ids.len() as u32;
        *ids.entry(l).or_insert(n)
    };
    let a_ids: Vec<u32> = a.iter().map(&mut intern).collect();
    let b_ids: Vec<u32> = b.iter().map(&mut intern).collect();
    let prefix = a_ids.iter().zip(&b_ids).take_while(|(x, y)| x == y).count();
    let suffix = a_ids[prefix..].iter().rev().zip(b_ids[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    let mid_a = &a_ids[prefix..a_ids.len() - suffix];
    let mid_b = &b_ids[prefix..b_ids.len() - suffix];
    let mut out: Vec<Edit> = (0..prefix).map(|i| Edit { op: Op::Same, a: i, b: i }).collect();
    for e in myers(mid_a, mid_b, max_d)? {
        out.push(Edit { op: e.op, a: e.a + prefix, b: e.b + prefix });
    }
    let (a0, b0) = (a.len() - suffix, b.len() - suffix);
    out.extend((0..suffix).map(|i| Edit { op: Op::Same, a: a0 + i, b: b0 + i }));
    Some(out)
}

/// Myers' O(ND) diff with the trace kept for backtracking.
fn myers(a: &[u32], b: &[u32], max_d: usize) -> Option<Vec<Edit>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 || m == 0 {
        let mut out: Vec<Edit> = (0..a.len()).map(|i| Edit { op: Op::Removed, a: i, b: 0 }).collect();
        out.extend((0..b.len()).map(|j| Edit { op: Op::Added, a: a.len(), b: j }));
        return Some(out);
    }
    let limit = ((n + m) as usize).min(max_d) as isize;
    let off = limit + 1;
    let mut v = vec![0i32; (2 * limit + 3) as usize];
    // trace[d]: v[-d..=d] before step d.
    let mut trace: Vec<Vec<i32>> = Vec::new();
    let mut end = None;
    'outer: for d in 0..=limit {
        trace.push(v[(off - d) as usize..=(off + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let i = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[i - 1] < v[i + 1]) { v[i + 1] } else { v[i - 1] + 1 } as isize;
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[i] = x as i32;
            if x >= n && y >= m {
                end = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let d_end = end?;
    let mut out = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=d_end).rev() {
        let t = &trace[d as usize];
        let get = |k: isize| t[(k + d) as usize] as isize;
        let k = x - y;
        let prev_k = if k == -d || (k != d && get(k - 1) < get(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = get(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            out.push(Edit { op: Op::Same, a: x as usize, b: y as usize });
        }
        if x == prev_x {
            out.push(Edit { op: Op::Added, a: x as usize, b: prev_y as usize });
        } else {
            out.push(Edit { op: Op::Removed, a: prev_x as usize, b: y as usize });
        }
        x = prev_x;
        y = prev_y;
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        out.push(Edit { op: Op::Same, a: x as usize, b: y as usize });
    }
    out.reverse();
    Some(out)
}

/// JSON pretty-printed with the keys of every object sorted.
pub fn canonical_json(v: &serde_json::Value) -> String {
    fn sorted(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                serde_json::Value::Object(keys.into_iter().map(|k| (k.clone(), sorted(&m[k]))).collect())
            }
            serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string_pretty(&sorted(v)).unwrap_or_default()
}

/// Bodies are read up to this size for comparing.
const MAX_BODY: u64 = 16 * 1024 * 1024;
/// Lines compared per side.
pub const MAX_LINES: usize = 20_000;
/// Differing lines beyond which the bodies are shown as replaced.
const MAX_EDITS: usize = 2_000;
/// Characters shown per line.
const MAX_LINE_CHARS: usize = 2_000;

/// The text of a body as compared: JSON canonical, XML/HTML pretty.
fn body_text(resp: &HttpResponse) -> Result<String, String> {
    let lang = resp.language();
    if matches!(lang, BodyLanguage::Binary | BodyLanguage::Image) {
        return Err(tr("Binary bodies are compared by size only.").to_string());
    }
    if resp.body.len() > MAX_BODY {
        return Err(trf(
            "Bodies larger than {} are compared by size only.",
            &[&merpati_core::util::human_bytes(MAX_BODY)],
        ));
    }
    let bytes = resp.body.read_all().map_err(|e| e.to_string())?;
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        return Ok(canonical_json(&v));
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(match lang {
        BodyLanguage::Xml | BodyLanguage::Html => crate::highlight::pretty_xml(&text),
        _ => text,
    })
}

/// Headers as `name: value` lines, names lowercased and sorted.
fn header_lines(resp: &HttpResponse) -> Vec<String> {
    let mut lines: Vec<(String, &str)> =
        resp.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.as_str())).collect();
    lines.sort_by(|x, y| x.0.cmp(&y.0));
    lines.into_iter().map(|(k, v)| format!("{k}: {v}")).collect()
}

/// Number of header names whose values differ (added and removed count).
pub fn changed_headers(a: &HttpResponse, b: &HttpResponse) -> usize {
    fn group(r: &HttpResponse) -> HashMap<String, Vec<&str>> {
        let mut m: HashMap<String, Vec<&str>> = HashMap::new();
        for (k, v) in &r.headers {
            m.entry(k.to_ascii_lowercase()).or_default().push(v.as_str());
        }
        m
    }
    let (ga, gb) = (group(a), group(b));
    let names: HashSet<&String> = ga.keys().chain(gb.keys()).collect();
    names.into_iter().filter(|n| ga.get(*n) != gb.get(*n)).count()
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Body,
    Headers,
}

/// A computed comparison of one mode.
struct Computed {
    mode: Mode,
    old: Vec<String>,
    new: Vec<String>,
    edits: Vec<Edit>,
    added: usize,
    removed: usize,
    /// Too many differences to align: shown as replaced.
    replaced: bool,
    truncated: bool,
    note: Option<String>,
    /// Widest line in characters.
    widest: usize,
    /// Unchanged runs the user opened (by their first edit index).
    opened: HashSet<usize>,
}

fn split_lines(text: &str) -> (Vec<String>, bool) {
    let mut lines: Vec<String> = text.lines().take(MAX_LINES + 1).map(str::to_string).collect();
    let truncated = lines.len() > MAX_LINES;
    lines.truncate(MAX_LINES);
    (lines, truncated)
}

fn compute(mode: Mode, a: &HttpResponse, b: &HttpResponse) -> Computed {
    let mut note = None;
    let (old, new, truncated) = match mode {
        Mode::Headers => (header_lines(a), header_lines(b), false),
        Mode::Body => match (body_text(a), body_text(b)) {
            (Ok(x), Ok(y)) => {
                let (old, t1) = split_lines(&x);
                let (new, t2) = split_lines(&y);
                (old, new, t1 || t2)
            }
            (Err(e), _) | (_, Err(e)) => {
                note = Some(e);
                (Vec::new(), Vec::new(), false)
            }
        },
    };
    let (edits, replaced) = match diff(&old, &new, MAX_EDITS) {
        Some(e) => (e, false),
        None => {
            let mut e: Vec<Edit> = (0..old.len()).map(|i| Edit { op: Op::Removed, a: i, b: 0 }).collect();
            e.extend((0..new.len()).map(|j| Edit { op: Op::Added, a: old.len(), b: j }));
            (e, true)
        }
    };
    let added = edits.iter().filter(|e| e.op == Op::Added).count();
    let removed = edits.iter().filter(|e| e.op == Op::Removed).count();
    let widest = old.iter().chain(&new).map(|l| l.chars().count().min(MAX_LINE_CHARS)).max().unwrap_or(0);
    Computed { mode, old, new, edits, added, removed, replaced, truncated, note, widest, opened: HashSet::new() }
}

/// A row of the diff list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// Index into the edits.
    Line(usize),
    /// `len` unchanged lines starting at edit `start`, folded.
    Fold { start: usize, len: usize },
}

/// Lines of context kept around changes.
const CONTEXT: usize = 3;

/// Folds long unchanged runs, keeping [`CONTEXT`] lines around changes.
pub fn rows(ops: &[Op], opened: &HashSet<usize>) -> Vec<Row> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ops.len() {
        if ops[i] != Op::Same {
            out.push(Row::Line(i));
            i += 1;
            continue;
        }
        let start = i;
        while i < ops.len() && ops[i] == Op::Same {
            i += 1;
        }
        let end = i;
        let lead = if start == 0 { 0 } else { CONTEXT };
        let trail = if end == ops.len() { 0 } else { CONTEXT };
        let len = end - start;
        if len > lead + trail + 1 && !opened.contains(&(start + lead)) {
            out.extend((start..start + lead).map(Row::Line));
            out.push(Row::Fold { start: start + lead, len: len - lead - trail });
            out.extend((end - trail..end).map(Row::Line));
        } else {
            out.extend((start..end).map(Row::Line));
        }
    }
    out
}

/// What the Compare section keeps between frames.
#[derive(Default)]
pub struct CompareState {
    pub mode: Mode,
    computed: Option<Computed>,
}

impl CompareState {
    /// Forgets the computed diff (a response arrived).
    pub fn reset(&mut self) {
        self.computed = None;
    }
}

/// The response kept for comparing and when it arrived.
pub struct Snapshot {
    pub resp: HttpResponse,
    pub at: chrono::DateTime<chrono::Local>,
}

/// What the Compare section asks of the tab.
#[derive(Default)]
pub struct CompareActs {
    pub toggle_pin: bool,
    /// Make room for the diff.
    pub maximize: bool,
}

pub fn show(ui: &mut Ui, state: &mut CompareState, prev: &Snapshot, cur: &Snapshot, pinned: bool) -> CompareActs {
    let p = theme::pal(ui);
    let mut acts = CompareActs::default();
    if state.computed.as_ref().map(|c| c.mode) != Some(state.mode) {
        state.computed = Some(compute(state.mode, &prev.resp, &cur.resp));
    }
    let Some(c) = state.computed.as_mut() else { return acts };
    let headers_changed = changed_headers(&prev.resp, &cur.resp);
    // Toolbar: what to compare, line counts, keep the baseline.
    ui.horizontal(|ui| {
        ui.set_height(30.0);
        ui.spacing_mut().item_spacing.x = 8.0;
        let headers_label = if headers_changed > 0 {
            format!("{} · {headers_changed}", tr("Headers"))
        } else {
            tr("Headers").to_string()
        };
        kit::segmented(ui, &mut state.mode, &[(Mode::Body, tr("Body")), (Mode::Headers, &headers_label)]);
        if c.added + c.removed > 0 {
            ui.label(RichText::new(format!("+{}", c.added)).font(theme::medium(12.5)).color(p.ok));
            ui.label(RichText::new(format!("−{}", c.removed)).font(theme::medium(12.5)).color(p.err));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let tip = if pinned {
                tr("Baseline kept: new responses are compared with it. Click to compare with the previous response again.")
            } else {
                tr("Keep this baseline: compare new responses with it instead of the previous one")
            };
            if kit::icon_toggle(ui, icons::PUSH_PIN, tip, pinned).clicked() {
                acts.toggle_pin = true;
            }
        });
    });
    ui.add_space(6.0);
    summary(ui, prev, cur, headers_changed);
    ui.add_space(8.0);
    if let Some(note) = &c.note {
        crate::request_tab::banner(ui, icons::INFO, p.info, note);
        return acts;
    }
    if c.added + c.removed == 0 {
        let text = match c.mode {
            Mode::Body => tr("The body is the same as in the previous response."),
            Mode::Headers => tr("The headers are the same as in the previous response."),
        };
        crate::request_tab::banner(ui, icons::CHECK_CIRCLE, p.ok, text);
        return acts;
    }
    if c.truncated {
        crate::request_tab::banner(
            ui,
            icons::INFO,
            p.info,
            &trf("Only the first {} lines of each body are compared.", &[&MAX_LINES]),
        );
        ui.add_space(6.0);
    }
    if c.replaced {
        crate::request_tab::banner(
            ui,
            icons::INFO,
            p.info,
            tr("The bodies differ in too many lines to line them up; showing the whole body as replaced."),
        );
        ui.add_space(6.0);
    }
    if ui.available_height() < 90.0 {
        // No room for the lines: say how many changed and offer room.
        let text = trf("{} lines changed. Maximize the response to see them.", &[&(c.added + c.removed)]);
        crate::request_tab::banner_with(ui, icons::GIT_DIFF, p.info, &text, |ui| {
            if kit::ghost(ui, Some(icons::ARROWS_OUT), tr("Maximize response")).clicked() {
                acts.maximize = true;
            }
        });
        return acts;
    }
    diff_list(ui, c, prev, cur);
    acts
}

/// Status, time, size and headers of both responses side by side.
fn summary(ui: &mut Ui, prev: &Snapshot, cur: &Snapshot, headers_changed: usize) {
    let p = theme::pal(ui);
    let (a, b) = (&prev.resp, &cur.resp);
    let fmt_ms = crate::request_tab::fmt_ms;
    let bytes = |n: u64| merpati_core::util::human_bytes(n);
    let delta_ms = b.timings.total_ms - a.timings.total_ms;
    let delta_size = b.total_size() as i64 - a.total_size() as i64;
    // Faster or smaller is green; a change under 10% stays muted.
    let tone = |delta: f64, base: f64| {
        if base > 0.0 && (delta / base).abs() < 0.1 {
            p.text_muted
        } else if delta < 0.0 {
            p.ok
        } else {
            p.warn
        }
    };
    if ui.available_height() < 340.0 || ui.available_width() < 560.0 {
        // Little room: one wrapped line, so the diff keeps its space.
        let dark = ui.visuals().dark_mode;
        let size = crate::request_tab::small_size(ui) + 0.5;
        let mut job = egui::text::LayoutJob::default();
        let mut add = |text: &str, color: Color32, strong: bool| {
            let font = if strong { theme::medium(size) } else { egui::FontId::proportional(size) };
            job.append(text, 0.0, egui::TextFormat::simple(font, color));
        };
        let status = |r: &HttpResponse| format!("{} {}", r.status, r.status_text).trim().to_string();
        add(&status(a), theme::status_color(a.status, dark), true);
        add(" → ", p.text_faint, false);
        add(&status(b), theme::status_color(b.status, dark), true);
        add("  ·  ", p.text_faint, false);
        add(&format!("{} → ", fmt_ms(a.timings.total_ms)), p.text_muted, false);
        add(&fmt_ms(b.timings.total_ms), p.text, true);
        if delta_ms.abs() >= 0.5 {
            let sign = if delta_ms < 0.0 { "−" } else { "+" };
            add(&format!(" ({sign}{})", fmt_ms(delta_ms.abs())), tone(delta_ms, a.timings.total_ms), false);
        }
        add("  ·  ", p.text_faint, false);
        add(&format!("{} → ", bytes(a.total_size())), p.text_muted, false);
        add(&bytes(b.total_size()), p.text, true);
        if delta_size != 0 {
            let sign = if delta_size < 0 { "−" } else { "+" };
            let text = format!(" ({sign}{})", bytes(delta_size.unsigned_abs()));
            add(&text, tone(delta_size as f64, a.total_size() as f64), false);
        }
        add("  ·  ", p.text_faint, false);
        add(&format!("{} ", tr("Headers")), p.text_muted, false);
        if headers_changed == 0 {
            add(tr("same"), p.text_muted, false);
        } else if headers_changed == 1 {
            add(tr("1 changed"), p.warn, true);
        } else {
            add(&trf("{} changed", &[&headers_changed]), p.warn, true);
        }
        job.wrap.max_width = ui.available_width();
        ui.add(egui::Label::new(job).wrap());
        return;
    }
    let label = |ui: &mut Ui, text: &str| {
        ui.label(RichText::new(text).small().color(p.text_muted));
    };
    let arrow = |ui: &mut Ui| {
        ui.label(RichText::new(icons::ARROW_RIGHT).font(theme::icon_font(12.0)).color(p.text_faint));
    };
    let group = |ui: &mut Ui, i: usize| match i {
        0 => {
            label(ui, tr("Status"));
            kit::status_pill(ui, a.status, &a.status_text);
            arrow(ui);
            kit::status_pill(ui, b.status, &b.status_text);
        }
        1 => {
            label(ui, tr("Time"));
            ui.label(RichText::new(fmt_ms(a.timings.total_ms)).color(p.text_muted));
            arrow(ui);
            ui.label(RichText::new(fmt_ms(b.timings.total_ms)).font(theme::medium(13.0)));
            if delta_ms.abs() >= 0.5 {
                let sign = if delta_ms < 0.0 { "−" } else { "+" };
                ui.label(
                    RichText::new(format!("{sign}{}", fmt_ms(delta_ms.abs())))
                        .small()
                        .color(tone(delta_ms, a.timings.total_ms)),
                );
            }
        }
        2 => {
            label(ui, tr("Size"));
            ui.label(RichText::new(bytes(a.total_size())).color(p.text_muted));
            arrow(ui);
            ui.label(RichText::new(bytes(b.total_size())).font(theme::medium(13.0)));
            if delta_size != 0 {
                let sign = if delta_size < 0 { "−" } else { "+" };
                ui.label(
                    RichText::new(format!("{sign}{}", bytes(delta_size.unsigned_abs())))
                        .small()
                        .color(tone(delta_size as f64, a.total_size() as f64)),
                );
            }
        }
        _ => {
            label(ui, tr("Headers"));
            if headers_changed == 0 {
                ui.label(RichText::new(tr("same")).color(p.text_muted));
            } else {
                let text = if headers_changed == 1 {
                    tr("1 changed").to_string()
                } else {
                    trf("{} changed", &[&headers_changed])
                };
                ui.label(RichText::new(text).font(theme::medium(13.0)).color(p.warn));
            }
        }
    };
    kit::card(ui).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
        // The groups go in rows that fit, using their widths of the last
        // frame (a group never breaks, which would push the panel wider).
        let id = ui.make_persistent_id("compare-summary");
        let widths: [f32; 4] = ui.data(|d| d.get_temp(id)).unwrap_or([220.0; 4]);
        const SEP: f32 = 9.0 + 16.0;
        let avail = ui.available_width();
        let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
        let mut used = 0.0;
        for (i, w) in widths.iter().enumerate() {
            let row = rows.last_mut().expect("a row");
            let need = if row.is_empty() { *w } else { w + SEP };
            if !row.is_empty() && used + need > avail {
                rows.push(vec![i]);
                used = *w;
            } else {
                row.push(i);
                used += need;
            }
        }
        let mut measured = widths;
        for row in rows {
            ui.horizontal(|ui| {
                for (n, i) in row.into_iter().enumerate() {
                    if n > 0 {
                        kit::vdivider(ui, 18.0);
                    }
                    measured[i] = ui.horizontal(|ui| group(ui, i)).response.rect.width();
                }
            });
        }
        if measured.iter().zip(&widths).any(|(m, w)| (m - w).abs() > 0.5) {
            ui.data_mut(|d| d.insert_temp(id, measured));
            ui.ctx().request_discard("compare summary widths");
        }
    });
}

/// The diff lines with line numbers, colored by change, unchanged runs
/// folded into a clickable row.
fn diff_list(ui: &mut Ui, c: &mut Computed, prev: &Snapshot, cur: &Snapshot) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    let ops: Vec<Op> = c.edits.iter().map(|e| e.op).collect();
    let rows = rows(&ops, &c.opened);
    let mono_size = crate::request_tab::mono_size(ui) - 0.5;
    let font = theme::mono(mono_size);
    let char_w = ui.painter().layout_no_wrap("0".into(), font.clone(), p.text).size().x;
    let digits = (c.old.len().max(c.new.len()).max(1) as f32).log10().floor() + 1.0;
    let num_w = (digits * char_w + 12.0).max(28.0);
    let gutter = num_w * 2.0 + 18.0;
    let row_h = (mono_size * 1.45).round().max(17.0);
    let content_w = gutter + c.widest as f32 * char_w + 24.0;
    let time = |s: &Snapshot| s.at.format("%H:%M:%S").to_string();
    egui::Frame::new()
        .fill(p.canvas)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(1))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Which side is which.
            ui.horizontal(|ui| {
                ui.set_height(26.0);
                ui.add_space(10.0);
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new("−").font(theme::mono(12.0)).color(p.err));
                ui.label(RichText::new(trf("Previous · {}", &[&time(prev)])).small().color(p.text_muted))
                    .on_hover_text(RichText::new(&prev.resp.url).monospace());
                ui.add_space(8.0);
                ui.label(RichText::new("+").font(theme::mono(12.0)).color(p.ok));
                ui.label(RichText::new(trf("Current · {}", &[&time(cur)])).small().color(p.text_muted))
                    .on_hover_text(RichText::new(&cur.resp.url).monospace());
            });
            crate::dialogs::hairline(ui);
            let mut open = None;
            kit::show_rows(
                egui::ScrollArea::both().id_salt("resp-compare").auto_shrink([false; 2]),
                ui,
                row_h,
                rows.len(),
                |ui, range| {
                    let width = ui.available_width().max(content_w);
                    for row in &rows[range] {
                        let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                        let painter = ui.painter();
                        match *row {
                            Row::Fold { start, len } => {
                                let hovered = resp.hovered();
                                painter.rect_filled(rect, 0.0, if hovered { p.hover } else { p.chrome });
                                let text = if len == 1 {
                                    tr("1 unchanged line").to_string()
                                } else {
                                    trf("{} unchanged lines", &[&len])
                                };
                                let label = format!("{}  {text}", icons::DOTS_THREE);
                                painter.text(
                                    pos2(rect.left() + gutter, rect.center().y),
                                    Align2::LEFT_CENTER,
                                    label,
                                    egui::FontId::proportional(mono_size),
                                    if hovered { p.text } else { p.text_muted },
                                );
                                let resp = resp.on_hover_text(tr("Show these lines"));
                                crate::widgets::a11y_button(&resp, &text);
                                if hovered {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                if resp.clicked() {
                                    open = Some(start);
                                }
                            }
                            Row::Line(i) => {
                                let e = c.edits[i];
                                let (sign, color, bg, text) = match e.op {
                                    Op::Same => (" ", p.text_muted, Color32::TRANSPARENT, &c.new[e.b]),
                                    Op::Removed => {
                                        ("−", p.err, theme::soft(p.err, if dark { 0.16 } else { 0.09 }), &c.old[e.a])
                                    }
                                    Op::Added => {
                                        ("+", p.ok, theme::soft(p.ok, if dark { 0.16 } else { 0.09 }), &c.new[e.b])
                                    }
                                };
                                if bg != Color32::TRANSPARENT {
                                    painter.rect_filled(rect, 0.0, bg);
                                }
                                let num = |n: usize| (n + 1).to_string();
                                let (old_no, new_no) = match e.op {
                                    Op::Same => (num(e.a), num(e.b)),
                                    Op::Removed => (num(e.a), String::new()),
                                    Op::Added => (String::new(), num(e.b)),
                                };
                                let cy = rect.center().y;
                                let gutter_font = theme::mono(mono_size - 1.0);
                                painter.text(
                                    pos2(rect.left() + num_w, cy),
                                    Align2::RIGHT_CENTER,
                                    old_no,
                                    gutter_font.clone(),
                                    p.text_faint,
                                );
                                painter.text(
                                    pos2(rect.left() + num_w * 2.0, cy),
                                    Align2::RIGHT_CENTER,
                                    new_no,
                                    gutter_font,
                                    p.text_faint,
                                );
                                painter.text(
                                    pos2(rect.left() + num_w * 2.0 + 10.0, cy),
                                    Align2::CENTER_CENTER,
                                    sign,
                                    font.clone(),
                                    color,
                                );
                                let shown: std::borrow::Cow<str> = if text.chars().count() > MAX_LINE_CHARS {
                                    format!("{}…", text.chars().take(MAX_LINE_CHARS).collect::<String>()).into()
                                } else {
                                    text.as_str().into()
                                };
                                let text_color = if e.op == Op::Same { p.text_muted } else { p.text };
                                painter.text(
                                    pos2(rect.left() + gutter, cy),
                                    Align2::LEFT_CENTER,
                                    shown,
                                    font.clone(),
                                    text_color,
                                );
                                resp.context_menu(|ui| {
                                    if ui.button(kit::with_icon(icons::COPY, tr("Copy line"))).clicked() {
                                        ui.ctx().copy_text(text.clone());
                                        ui.close();
                                    }
                                });
                            }
                        }
                    }
                },
            );
            if let Some(start) = open {
                c.opened.insert(start);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    /// Applies a diff to `a` and checks that it yields `b`; returns the
    /// number of changed lines.
    fn check(a: &[&str], b: &[&str]) -> usize {
        let edits = diff(a, b, 10_000).unwrap();
        let mut rebuilt_a = Vec::new();
        let mut rebuilt_b = Vec::new();
        for e in &edits {
            match e.op {
                Op::Same => {
                    assert_eq!(a[e.a], b[e.b]);
                    rebuilt_a.push(a[e.a]);
                    rebuilt_b.push(b[e.b]);
                }
                Op::Removed => rebuilt_a.push(a[e.a]),
                Op::Added => rebuilt_b.push(b[e.b]),
            }
        }
        assert_eq!(rebuilt_a, a);
        assert_eq!(rebuilt_b, b);
        edits.iter().filter(|e| e.op != Op::Same).count()
    }

    #[test]
    fn diffs_are_minimal_and_complete() {
        assert_eq!(check(&lines("a\nb\nc"), &lines("a\nb\nc")), 0);
        assert_eq!(check(&lines("a\nb\nc"), &lines("a\nx\nc")), 2);
        assert_eq!(check(&lines("a\nb\nc\nd"), &lines("a\nc\nd\ne")), 2);
        assert_eq!(check(&[], &lines("a\nb")), 2);
        assert_eq!(check(&lines("a\nb"), &[]), 2);
        assert_eq!(check(&lines("x\na\nb\nc\ny"), &lines("a\nb\nq\nc")), 3);
        // The classic example (D = 5).
        assert_eq!(check(&lines("a\nb\nc\na\nb\nb\na"), &lines("c\nb\na\nb\na\nc")), 5);
    }

    #[test]
    fn many_differences_give_up() {
        let a: Vec<String> = (0..500).map(|i| format!("a{i}")).collect();
        let b: Vec<String> = (0..500).map(|i| format!("b{i}")).collect();
        assert!(diff(&a, &b, 100).is_none());
        assert_eq!(diff(&a, &b, 1000).unwrap().len(), 1000);
    }

    /// A large body with a few changes stays quick (prefix/suffix trimming
    /// and a small D).
    #[test]
    fn large_bodies_with_few_changes() {
        let a: Vec<String> = (0..20_000).map(|i| format!("line {i}")).collect();
        let mut b = a.clone();
        b[10] = "changed".into();
        b[15_000] = "changed too".into();
        b.insert(9_000, "new".into());
        let start = std::time::Instant::now();
        let edits = diff(&a, &b, MAX_EDITS).unwrap();
        assert!(start.elapsed().as_secs_f64() < 5.0);
        assert_eq!(edits.iter().filter(|e| e.op == Op::Added).count(), 3);
        assert_eq!(edits.iter().filter(|e| e.op == Op::Removed).count(), 2);
    }

    #[test]
    fn json_key_order_does_not_count() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"b": 1, "a": {"y": 2, "x": [1, {"d": 1, "c": 2}]}}"#).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(r#"{"a": {"x": [1, {"c": 2, "d": 1}], "y": 2}, "b": 1}"#).unwrap();
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert!(canonical_json(&a).starts_with("{\n  \"a\""));
    }

    #[test]
    fn unchanged_runs_fold() {
        use Op::*;
        let mut ops = vec![Same; 10];
        ops.push(Added);
        ops.extend(vec![Same; 10]);
        ops.push(Removed);
        ops.extend(vec![Same; 5]);
        let r = rows(&ops, &HashSet::new());
        // 7 leading lines folded, 3 kept; 4 folded between the changes; 2 folded at the end.
        assert_eq!(r[0], Row::Fold { start: 0, len: 7 });
        assert_eq!(r[1..5], [Row::Line(7), Row::Line(8), Row::Line(9), Row::Line(10)]);
        assert!(r.contains(&Row::Fold { start: 14, len: 4 }));
        assert_eq!(*r.last().unwrap(), Row::Fold { start: 25, len: 2 });
        // Opening a fold shows its lines.
        let opened: HashSet<usize> = [14].into_iter().collect();
        let r2 = rows(&ops, &opened);
        assert!(!r2.contains(&Row::Fold { start: 14, len: 4 }));
        assert_eq!(r2.len(), r.len() + 3);
        // Short runs stay.
        assert_eq!(rows(&[Same, Added, Same], &HashSet::new()).len(), 3);
    }

    #[test]
    fn header_changes_count_names() {
        let a = HttpResponse {
            headers: vec![("Date".into(), "1".into()), ("X-A".into(), "a".into()), ("Set-Cookie".into(), "x".into())],
            ..Default::default()
        };
        let b = HttpResponse {
            headers: vec![("date".into(), "2".into()), ("x-a".into(), "a".into()), ("X-New".into(), "n".into())],
            ..Default::default()
        };
        assert_eq!(changed_headers(&a, &b), 3);
        assert_eq!(header_lines(&b), vec!["date: 2", "x-a: a", "x-new: n"]);
    }
}
