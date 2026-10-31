//! Reusable widgets: variable-aware text fields, key/value tables, the code
//! editor and small helpers.

use egui::text::{CCursor, CCursorRange};
use egui::{Key, Modifiers, RichText, TextEdit, Ui};

use merpati_core::model::KeyValue;
use merpati_core::variables::{Scope, TokenKind, VariableScopes, tokenize};

use crate::highlight::{self, HlKey, Lang, VarNames};
use crate::i18n::tr;
use crate::theme;

/// Variables visible to the request being edited, for highlighting,
/// tooltips and autocomplete.
#[derive(Default)]
pub struct VarCtx {
    pub names: VarNames,
    pub scopes: VariableScopes,
    /// Keys whose value must be masked in tooltips.
    pub secret_keys: std::collections::HashSet<String>,
    pub vault_names: Vec<String>,
}

impl VarCtx {
    pub fn new(
        scopes: VariableScopes,
        secret_keys: std::collections::HashSet<String>,
        vault_names: Vec<String>,
    ) -> Self {
        let mut names: Vec<String> = scopes.all_names().into_iter().map(|(k, _, _)| k).collect();
        names.extend(vault_names.iter().map(|v| format!("vault:{v}")));
        VarCtx { names: VarNames::new(names), scopes, secret_keys, vault_names }
    }

    pub fn describe(&self, name: &str) -> (String, Option<Scope>) {
        let name = name.trim();
        if let Some(v) = name.strip_prefix("vault:") {
            return if self.vault_names.iter().any(|n| n == v) {
                ("••••••".into(), Some(Scope::Vault))
            } else {
                (format!("vault secret \"{v}\" not found"), None)
            };
        }
        if let Some((v, scope)) = self.scopes.get(name) {
            let shown = if self.secret_keys.contains(name) { "••••••".to_string() } else { v.into_owned() };
            return (shown, Some(scope));
        }
        if name.starts_with('$') && merpati_core::dynamic::NAMES.contains(&name) {
            return ("dynamic value generated when sending".into(), Some(Scope::Dynamic));
        }
        ("not defined in any scope".into(), None)
    }

    pub fn completions(&self, prefix: &str) -> Vec<String> {
        let p = prefix.to_lowercase();
        let mut out: Vec<String> = self
            .scopes
            .all_names()
            .into_iter()
            .map(|(k, _, _)| k)
            .chain(self.vault_names.iter().map(|v| format!("vault:{v}")))
            .filter(|k| k.to_lowercase().starts_with(&p))
            .collect();
        if prefix.starts_with('$') {
            out.extend(
                merpati_core::dynamic::NAMES.iter().filter(|n| n.to_lowercase().starts_with(&p)).map(|s| s.to_string()),
            );
        }
        out.sort();
        out.dedup();
        out.truncate(12);
        out
    }
}

fn is_dark(ui: &Ui) -> bool {
    ui.visuals().dark_mode
}

/// Byte offset of a char index.
fn byte_at(text: &str, char_idx: usize) -> usize {
    text.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(text.len())
}

/// Shows a tooltip describing the `{{variable}}` under the pointer.
fn variable_tooltip(ui: &Ui, out: &egui::text_edit::TextEditOutput, text: &str, vars: &VarCtx) {
    let Some(pos) = out.response.hover_pos() else { return };
    let cursor = out.galley.cursor_from_pos(pos - out.galley_pos);
    let at = byte_at(text, cursor.index.0);
    for t in tokenize(text) {
        if t.kind == TokenKind::Variable && t.range.start <= at && at < t.range.end {
            let name = text[t.name_range.clone()].trim().to_string();
            let (value, scope) = vars.describe(&name);
            let dark = is_dark(ui);
            out.response.response.clone().on_hover_ui_at_pointer(|ui| {
                ui.set_max_width(360.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&name).monospace().strong());
                    match scope {
                        Some(s) => ui.label(RichText::new(s.label()).small().color(theme::var_defined(dark))),
                        None => ui.label(RichText::new(tr("undefined")).small().color(theme::var_undefined(dark))),
                    };
                });
                ui.label(RichText::new(value).monospace());
            });
            break;
        }
    }
}

/// Autocomplete popup for `{{` references. Returns true if text changed.
fn variable_completion(
    ui: &mut Ui,
    id: egui::Id,
    out: &egui::text_edit::TextEditOutput,
    text: &mut String,
    vars: &VarCtx,
) -> bool {
    if !out.response.has_focus() {
        return false;
    }
    let Some(range) = out.cursor_range else { return false };
    let cursor_b = byte_at(text, range.primary.index.0);
    let before = &text[..cursor_b];
    let Some(open) = before.rfind("{{") else { return false };
    let prefix = &before[open + 2..];
    if prefix.contains("}}") || prefix.contains(' ') && !prefix.trim().is_empty() {
        return false;
    }
    let items = vars.completions(prefix.trim());
    if items.is_empty() || items.len() == 1 && items[0] == prefix.trim() {
        return false;
    }
    let mut chosen: Option<String> = None;
    let accept_first = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab));
    if accept_first {
        chosen = items.first().cloned();
    }
    let pos = out.galley_pos + out.galley.pos_from_cursor(range.primary).left_bottom().to_vec2() + egui::vec2(0.0, 2.0);
    egui::Area::new(id.with("var-complete")).order(egui::Order::Foreground).fixed_pos(pos).show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_min_width(180.0);
            for (i, item) in items.iter().enumerate() {
                let (value, scope) = vars.describe(item);
                let label = format!("{item}  ·  {}", scope.map(|s| s.label()).unwrap_or(""));
                let r = ui.selectable_label(i == 0, RichText::new(label).monospace().size(11.5));
                let r = r.on_hover_text(value);
                if r.clicked() {
                    chosen = Some(item.clone());
                }
            }
            ui.label(RichText::new(tr("Tab ↹ to accept")).small().weak());
        });
    });
    if let Some(name) = chosen {
        let after = &text[cursor_b..];
        let close = if after.starts_with("}}") { "" } else { "}}" };
        let new_text = format!("{}{{{{{}{}{}", &text[..open], name, close, after);
        let new_cursor = text[..open].chars().count() + 2 + name.chars().count() + 2;
        *text = new_text;
        let mut state = out.state.clone();
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(new_cursor))));
        state.store(ui.ctx(), out.response.id);
        return true;
    }
    false
}

/// Single-line text field with `{{variable}}` highlighting, hover values and
/// autocomplete.
/// Spoken name for a widget whose visible text does not describe it (icon
/// buttons, unlabeled fields). Keeps the widget's role and value.
pub fn a11y_name(r: &egui::Response, name: &str) {
    r.ctx.accesskit_node_builder(r.id, |n| n.set_label(name));
}

/// Tells screen readers the key that presses a widget (`"Enter"`, `"Escape"`).
pub fn a11y_shortcut(r: &egui::Response, keys: &str) {
    r.ctx.accesskit_node_builder(r.id, |n| n.set_keyboard_shortcut(keys));
}

/// Makes a custom clickable area (tree row) a named button for screen readers.
pub fn a11y_button(r: &egui::Response, name: &str) {
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
}

/// Exposes a clickable area as a tab (selected or not) for screen readers.
pub fn a11y_tab(r: &egui::Response, name: &str, selected: bool) {
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Tab, true, name));
    r.ctx.accesskit_node_builder(r.id, |n| n.set_selected(selected));
}

/// A small frameless icon button; `name` is its tooltip and accessible name.
pub fn small_icon_button(ui: &mut Ui, icon: &str, name: &str) -> egui::Response {
    crate::kit::icon_button_ex(ui, icon, name, false, 22.0)
}

pub fn var_line(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    vars: &VarCtx,
    hint: &str,
    width: f32,
) -> egui::Response {
    var_line_ex(ui, id, text, vars, hint, width, true)
}

/// [`var_line`] without its own frame, for table cells and grouped inputs.
pub fn var_cell(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    vars: &VarCtx,
    hint: &str,
    width: f32,
) -> egui::Response {
    var_line_ex(ui, id, text, vars, hint, width, false)
}

fn var_line_ex(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    vars: &VarCtx,
    hint: &str,
    width: f32,
    frame: bool,
) -> egui::Response {
    let id = ui.make_persistent_id(id);
    let dark = is_dark(ui);
    let font_size = ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5);
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let job = highlight::cached(
            ui.ctx(),
            HlKey {
                text: buf.as_str(),
                lang: Lang::Text,
                dark,
                search: "",
                vars: Some(&vars.names),
                font_size,
                wrap: false,
            },
        );
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let mut edit = TextEdit::singleline(text)
        .id(id)
        .hint_text(hint)
        .desired_width(width)
        .font(egui::TextStyle::Monospace)
        .margin(if frame { egui::Margin::symmetric(8, 5) } else { egui::Margin::symmetric(6, 4) })
        .layouter(&mut layouter);
    if !frame {
        // Fill the cell or group height with the text centered in it.
        let h = ui.available_height().clamp(20.0, 34.0);
        edit = edit.frame(egui::Frame::NONE).min_size(egui::vec2(0.0, h)).align(egui::Align2::LEFT_CENTER);
    }
    let out = edit.show(ui);
    variable_tooltip(ui, &out, text, vars);
    if !hint.is_empty() {
        a11y_name(&out.response.response, hint);
    }
    let mut response = out.response.response.clone();
    if variable_completion(ui, id, &out, text, vars) {
        response.mark_changed();
    }
    response
}

/// Multi-line code editor with highlighting (and variable overlay when
/// `vars` is given).
pub fn code_editor(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    lang: Lang,
    vars: Option<&VarCtx>,
    min_rows: usize,
    readonly: bool,
) -> egui::Response {
    code_editor_with(ui, id, text, lang, vars, min_rows, readonly, None)
}

/// One completion: `(label, insert, detail)`.
pub type CompletionItem = (String, String, String);

/// Completion provider: given the text and the cursor (byte offset), returns
/// the byte where the replaced word starts and the items.
pub type CompletionFn<'a> = &'a dyn Fn(&str, usize) -> Option<(usize, Vec<CompletionItem>)>;

#[allow(clippy::too_many_arguments)]
pub fn code_editor_with(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    lang: Lang,
    vars: Option<&VarCtx>,
    min_rows: usize,
    readonly: bool,
    completions: Option<CompletionFn<'_>>,
) -> egui::Response {
    let id = ui.make_persistent_id(id);
    let dark = is_dark(ui);
    let font_size = ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5);
    let names = vars.map(|v| &v.names);

    // Completions are computed from last frame's cursor so Tab can be
    // consumed before the editor turns it into an indent.
    let mut pending: Option<(usize, usize, Vec<CompletionItem>)> = None;
    if let (Some(provider), Some(state)) = (completions, egui::text_edit::TextEditState::load(ui.ctx(), id)) {
        let focused = ui.ctx().memory(|m| m.has_focus(id));
        if focused && let Some(range) = state.cursor.char_range() {
            let cursor_b = byte_at(text, range.primary.index.0);
            if let Some((start, items)) = provider(text, cursor_b).filter(|(_, items)| !items.is_empty()) {
                pending = Some((start, cursor_b, items));
            }
        }
    }
    let mut accept: Option<String> = None;
    if let Some((_, _, items)) = &pending
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab))
    {
        accept = items.first().map(|i| i.1.clone());
    }

    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let mut job = highlight::cached(
            ui.ctx(),
            HlKey { text: buf.as_str(), lang, dark, search: "", vars: names, font_size, wrap: true },
        );
        job.wrap.max_width = wrap_width;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let edit = TextEdit::multiline(text)
        .id(id)
        .code_editor()
        .desired_rows(min_rows)
        .desired_width(f32::INFINITY)
        .lock_focus(true)
        .interactive(!readonly)
        .layouter(&mut layouter);
    let out = edit.show(ui);
    if let Some(v) = vars {
        variable_tooltip(ui, &out, text, v);
    }
    let mut response = out.response.response.clone();
    // Callers with a more specific name set theirs after this.
    a11y_name(&response, tr("Code editor"));
    if let Some((start, end, items)) = pending {
        if accept.is_none()
            && out.response.has_focus()
            && let Some(range) = out.cursor_range
        {
            let pos = out.galley_pos
                + out.galley.pos_from_cursor(range.primary).left_bottom().to_vec2()
                + egui::vec2(0.0, 2.0);
            egui::Area::new(id.with("complete")).order(egui::Order::Foreground).fixed_pos(pos).show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(220.0);
                    for (i, (label, insert, detail)) in items.iter().take(10).enumerate() {
                        let r = ui
                            .selectable_label(i == 0, RichText::new(label).monospace().size(11.5))
                            .on_hover_text(detail);
                        if r.clicked() {
                            accept = Some(insert.clone());
                        }
                    }
                    ui.label(RichText::new(tr("Tab ↹ to accept")).small().weak());
                });
            });
        }
        if let Some(insert) = accept
            && start <= end
            && end <= text.len()
        {
            text.replace_range(start..end, &insert);
            let new_cursor = text[..start].chars().count() + insert.chars().count();
            if let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), id) {
                state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(new_cursor))));
                state.store(ui.ctx(), id);
            }
            ui.ctx().memory_mut(|m| m.request_focus(id));
            response.mark_changed();
        }
    }
    response
}

/// Byte offset where the word ending at the end of `before` starts (after
/// the last char that is not part of a word, whatever its UTF-8 length).
pub fn word_start(before: &str, is_word: impl Fn(char) -> bool) -> usize {
    before.char_indices().rev().find(|(_, c)| !is_word(*c)).map(|(i, c)| i + c.len_utf8()).unwrap_or(0)
}

/// Completions for the `pm` API in scripts.
pub fn pm_completions(text: &str, cursor: usize) -> Option<(usize, Vec<CompletionItem>)> {
    let before = &text[..cursor];
    let start = word_start(before, |c| c.is_alphanumeric() || c == '_' || c == '.');
    let word = &before[start..];
    if !word.starts_with("pm.") || word.len() < 3 {
        return None;
    }
    let items: Vec<CompletionItem> = crate::snippets::PM_COMPLETIONS
        .iter()
        .filter(|(sig, _)| sig.starts_with(word) && sig.len() > word.len())
        .map(|(sig, doc)| {
            let insert = match sig.find('(') {
                Some(i) => format!("{}(", &sig[..i]),
                None => sig.to_string(),
            };
            (sig.to_string(), insert, doc.to_string())
        })
        .collect();
    (!items.is_empty()).then_some((start, items))
}

pub const COMMON_HEADERS: &[&str] = &[
    "Accept",
    "Accept-Charset",
    "Accept-Encoding",
    "Accept-Language",
    "Authorization",
    "Cache-Control",
    "Connection",
    "Content-Disposition",
    "Content-Encoding",
    "Content-Language",
    "Content-Length",
    "Content-Type",
    "Cookie",
    "Date",
    "DNT",
    "ETag",
    "Expect",
    "Forwarded",
    "From",
    "Host",
    "If-Match",
    "If-Modified-Since",
    "If-None-Match",
    "If-Range",
    "If-Unmodified-Since",
    "Origin",
    "Pragma",
    "Prefer",
    "Range",
    "Referer",
    "TE",
    "Upgrade",
    "User-Agent",
    "Via",
    "X-API-Key",
    "X-Correlation-ID",
    "X-CSRF-Token",
    "X-Forwarded-For",
    "X-Forwarded-Host",
    "X-Forwarded-Proto",
    "X-Request-ID",
    "X-Requested-With",
];

/// A suggestion dropdown under a text edit (header names, content types).
fn suggestions(ui: &mut Ui, out: &egui::text_edit::TextEditOutput, text: &mut String, list: &[&str]) -> bool {
    if !out.response.has_focus() || text.is_empty() {
        return false;
    }
    let lower = text.to_lowercase();
    let items: Vec<&str> =
        list.iter().copied().filter(|h| h.to_lowercase().starts_with(&lower) && h.len() > text.len()).take(8).collect();
    if items.is_empty() {
        return false;
    }
    let mut chosen = None;
    if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
        chosen = items.first().map(|s| s.to_string());
    }
    let pos = out.response.rect.left_bottom() + egui::vec2(0.0, 2.0);
    egui::Area::new(out.response.id.with("suggest")).order(egui::Order::Foreground).fixed_pos(pos).show(
        ui.ctx(),
        |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(out.response.rect.width().max(160.0));
                for (i, item) in items.iter().enumerate() {
                    if ui.selectable_label(i == 0, RichText::new(*item).monospace().size(11.5)).clicked() {
                        chosen = Some(item.to_string());
                    }
                }
            });
        },
    );
    if let Some(c) = chosen {
        *text = c;
        let mut state = out.state.clone();
        state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(text.chars().count()))));
        state.store(ui.ctx(), out.response.id);
        return true;
    }
    false
}

#[derive(Clone, Copy, Default)]
pub struct KvOptions<'a> {
    /// Title shown on the table's toolbar row (left of "Bulk Edit").
    pub title: Option<&'a str>,
    pub key_suggestions: Option<&'a [&'a str]>,
    pub show_description: bool,
    pub key_hint: &'a str,
    pub value_hint: &'a str,
    pub readonly_keys: bool,
    pub allow_add: bool,
}

/// Editable key/value table with enable checkboxes and an always-present
/// empty row for adding entries. Returns true when anything changed.
pub fn kv_table(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    rows: &mut Vec<KeyValue>,
    vars: &VarCtx,
    opts: KvOptions<'_>,
) -> bool {
    // Scope every row widget under the table id so two tables on one page
    // never share widget state.
    let salt = egui::Id::unique(&id);
    ui.push_id(salt, |ui| kv_table_inner(ui, salt, rows, vars, opts)).inner
}

fn kv_table_inner(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    rows: &mut Vec<KeyValue>,
    vars: &VarCtx,
    opts: KvOptions<'_>,
) -> bool {
    use egui::{CornerRadius, Rect, Stroke, StrokeKind, pos2, vec2};
    let p = theme::pal(ui);
    let id = ui.make_persistent_id(id);
    let mut changed = false;
    let bulk_id = id.with("bulk");
    let mut bulk: bool = ui.data(|d| d.get_temp(bulk_id).unwrap_or(false));
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        if let Some(title) = opts.title {
            ui.label(crate::kit::section_title(title));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (icon, label) = if bulk {
                (crate::icons::TABLE, tr("Key-Value Edit"))
            } else {
                (crate::icons::TEXT_ALIGN_LEFT, tr("Bulk Edit"))
            };
            if crate::kit::ghost(ui, Some(icon), label).clicked() {
                bulk = !bulk;
                if bulk {
                    ui.data_mut(|d| d.insert_temp(id.with("bulk-text"), bulk_format(rows)));
                }
                ui.data_mut(|d| d.insert_temp(bulk_id, bulk));
            }
        });
    });
    ui.add_space(4.0);
    if bulk {
        let stored: String = ui.data(|d| d.get_temp(id.with("bulk-text")).unwrap_or_default());
        let mut text = bulk_text(stored, rows);
        let r = code_editor(ui, id.with("bulk-editor"), &mut text, Lang::Text, Some(vars), 6, false);
        ui.label(RichText::new(tr("One entry per line as key:value. Prefix with // to disable.")).small().weak());
        if r.changed() {
            // Keep descriptions of keys that still exist.
            let mut merged = bulk_parse(&text);
            for m in &mut merged {
                if let Some(old) = rows.iter().find(|o| o.key == m.key) {
                    m.description = old.description.clone();
                }
            }
            *rows = merged;
            changed = true;
            ui.data_mut(|d| d.insert_temp(id.with("bulk-text"), text));
        }
        return changed;
    }

    // A bordered grid: header row, one row per entry and an empty row that
    // adds one. Cells are frameless inputs; the focused one gets a ring.
    const CHECK: f32 = 34.0;
    const DEL: f32 = 32.0;
    const HEAD: f32 = 28.0;
    const ROW: f32 = 32.0;
    let width = ui.available_width().max(240.0);
    let left = ui.cursor().left();
    let top = ui.cursor().top();
    let cols = width - CHECK - DEL;
    // The description column takes what is left between the value and the delete column.
    let (key_w, val_w) = if opts.show_description { (cols * 0.3, cols * 0.42) } else { (cols * 0.38, cols * 0.62) };
    let xs = [left + CHECK, left + CHECK + key_w, left + CHECK + key_w + val_w, left + width - DEL];
    let mono = theme::mono(ui.style().text_styles.get(&egui::TextStyle::Monospace).map(|f| f.size).unwrap_or(12.5));

    let head = Rect::from_min_size(pos2(left, top), vec2(width, HEAD));
    ui.painter().rect_filled(head, CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, p.chrome);
    let head_font = theme::medium(11.5);
    let mut labels = vec![(xs[0], tr("Key")), (xs[1], tr("Value"))];
    if opts.show_description {
        labels.push((xs[2], tr("Description")));
    }
    for (x, label) in labels {
        ui.painter().text(
            pos2(x + 10.0, head.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            head_font.clone(),
            p.text_muted,
        );
    }
    let mut y = head.bottom();
    let mut remove = None;
    let n = rows.len();
    for i in 0..=n {
        let is_new = i == n;
        if is_new && !opts.allow_add {
            break;
        }
        let mut row = if is_new { KeyValue::default() } else { rows[i].clone() };
        let mut row_changed = false;
        let row_rect = Rect::from_min_size(pos2(left, y), vec2(width, ROW));
        ui.painter().hline(left..=left + width, y, Stroke::new(1.0, p.border));
        let hovered = ui.rect_contains_pointer(row_rect);
        if hovered && !is_new {
            ui.painter().rect_filled(row_rect.shrink2(vec2(1.0, 0.5)), 0.0, p.faint_row());
        }
        let cell = |x0: f32, x1: f32| Rect::from_min_max(pos2(x0 + 4.0, y + 3.0), pos2(x1 - 4.0, y + ROW - 3.0));
        ui.push_id(i, |ui| {
            if !is_new {
                let c =
                    Rect::from_center_size(pos2(left + CHECK / 2.0 + 2.0, row_rect.center().y), vec2(18.0, ROW - 6.0));
                let r = ui.put(c, egui::Checkbox::without_text(&mut row.enabled));
                a11y_name(&r, &format!("{} {}", tr("Enabled"), i + 1));
                if r.changed() {
                    row_changed = true;
                }
            }
            let faded = !is_new && !row.enabled;
            let key_rect = cell(xs[0], xs[1]);
            let key_out = ui
                .scope_builder(egui::UiBuilder::new().max_rect(key_rect), |ui| {
                    let mut edit = TextEdit::singleline(&mut row.key)
                        .hint_text(if is_new { opts.key_hint } else { "" })
                        .desired_width(key_rect.width())
                        .font(mono.clone())
                        .frame(egui::Frame::NONE)
                        .margin(egui::Margin::symmetric(6, 2))
                        .min_size(vec2(0.0, key_rect.height()))
                        .align(egui::Align2::LEFT_CENTER)
                        .interactive(!opts.readonly_keys || is_new);
                    if faded {
                        edit = edit.text_color(p.text_faint);
                    }
                    edit.show(ui)
                })
                .inner;
            a11y_name(&key_out.response.response, &format!("{} {}", tr("Key"), i + 1));
            if key_out.response.response.has_focus() {
                ui.painter().rect_stroke(
                    key_rect,
                    CornerRadius::same(5),
                    Stroke::new(1.5, p.accent),
                    StrokeKind::Outside,
                );
            }
            if key_out.response.changed() {
                row_changed = true;
            }
            if let Some(list) = opts.key_suggestions
                && suggestions(ui, &key_out, &mut row.key, list)
            {
                row_changed = true;
            }
            let val_rect = cell(xs[1], if opts.show_description { xs[2] } else { xs[3] });
            let val_resp = ui
                .scope_builder(egui::UiBuilder::new().max_rect(val_rect), |ui| {
                    var_cell(ui, "v", &mut row.value, vars, if is_new { opts.value_hint } else { "" }, val_rect.width())
                })
                .inner;
            a11y_name(&val_resp, &format!("{} {}", tr("Value"), i + 1));
            if val_resp.has_focus() {
                ui.painter().rect_stroke(
                    val_rect,
                    CornerRadius::same(5),
                    Stroke::new(1.5, p.accent),
                    StrokeKind::Outside,
                );
            }
            if val_resp.changed() {
                row_changed = true;
            }
            if opts.show_description {
                let d_rect = cell(xs[2], xs[3]);
                let r = ui.put(
                    d_rect,
                    TextEdit::singleline(&mut row.description)
                        .frame(egui::Frame::NONE)
                        .margin(egui::Margin::symmetric(6, 2))
                        .min_size(vec2(0.0, d_rect.height()))
                        .align(egui::Align2::LEFT_CENTER)
                        .hint_text(if is_new { tr("Description") } else { "" })
                        .desired_width(d_rect.width()),
                );
                a11y_name(&r, &format!("{} {}", tr("Description"), i + 1));
                if r.has_focus() {
                    ui.painter().rect_stroke(
                        d_rect,
                        CornerRadius::same(5),
                        Stroke::new(1.5, p.accent),
                        StrokeKind::Outside,
                    );
                }
                if r.changed() {
                    row_changed = true;
                }
            }
            if !is_new && hovered {
                let d = Rect::from_center_size(pos2(xs[3] + DEL / 2.0, row_rect.center().y), vec2(24.0, 24.0));
                let r = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(d), |ui| {
                        crate::kit::icon_button_ex(ui, crate::icons::TRASH, tr("Remove"), false, 24.0)
                    })
                    .inner;
                a11y_name(&r, &format!("{} {}", tr("Remove"), i + 1));
                if r.clicked() {
                    remove = Some(i);
                }
            }
        });
        y += ROW;
        if row_changed {
            changed = true;
            if is_new {
                if !row.key.is_empty() || !row.value.is_empty() {
                    row.enabled = true;
                    rows.push(row);
                }
            } else {
                rows[i] = row;
            }
        }
    }
    let table = Rect::from_min_max(pos2(left, top), pos2(left + width, y));
    for x in &xs[..if opts.show_description { 3 } else { 2 }] {
        ui.painter().vline(*x, table.y_range(), Stroke::new(1.0, p.border));
    }
    ui.painter().rect_stroke(table, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.advance_cursor_after_rect(table);
    if let Some(i) = remove {
        rows.remove(i);
        changed = true;
    }
    changed
}

/// Bulk edit text: one `key:value` line per row, disabled rows prefixed
/// with `//`.
fn bulk_format(rows: &[KeyValue]) -> String {
    rows.iter()
        .map(|r| format!("{}{}:{}", if r.enabled { "" } else { "//" }, r.key, r.value))
        .collect::<Vec<_>>()
        .join("\n")
}

fn bulk_parse(text: &str) -> Vec<KeyValue> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (enabled, l) = match l.trim_start().strip_prefix("//") {
                Some(rest) => (false, rest),
                None => (true, l),
            };
            let (k, v) = l.split_once(':').unwrap_or((l, ""));
            let mut kv = KeyValue::new(k.trim(), v.trim_start());
            kv.enabled = enabled;
            kv
        })
        .collect()
}

/// The bulk edit text to show: the text being edited, unless the rows were
/// changed elsewhere since (then the rows, so they are not overwritten).
fn bulk_text(stored: String, rows: &[KeyValue]) -> String {
    let same = |a: &KeyValue, b: &KeyValue| a.enabled == b.enabled && a.key == b.key && a.value == b.value;
    let parsed = bulk_parse(&stored);
    if parsed.len() == rows.len() && parsed.iter().zip(rows).all(|(a, b)| same(a, b)) {
        stored
    } else {
        bulk_format(rows)
    }
}

pub const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Method picker that also accepts custom methods.
pub fn method_picker(ui: &mut Ui, method: &mut String) -> bool {
    let dark = is_dark(ui);
    let mut changed = false;
    let text = RichText::new(method.as_str()).font(theme::semibold(13.0)).color(theme::method_color(method, dark));
    let r = egui::ComboBox::from_id_salt("method")
        .icon(crate::kit::combo_caret)
        .width(96.0)
        .selected_text(text)
        .show_ui(ui, |ui| {
            for m in METHODS {
                let label = RichText::new(*m).font(theme::semibold(13.0)).color(theme::method_color(m, dark));
                if ui.selectable_label(method == m, label).clicked() {
                    *method = m.to_string();
                    changed = true;
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new(tr("Custom")).small());
                let mut custom = if METHODS.contains(&method.as_str()) { String::new() } else { method.clone() };
                if ui.add(TextEdit::singleline(&mut custom).desired_width(80.0).hint_text("PROPFIND")).changed() {
                    *method = custom.to_uppercase();
                    changed = true;
                }
            });
        });
    a11y_name(&r.response, tr("Method"));
    changed
}

pub fn method_badge(method: &str, dark: bool) -> RichText {
    let short = match method {
        "DELETE" => "DEL",
        "OPTIONS" => "OPT",
        "PATCH" => "PATCH",
        m => m,
    };
    RichText::new(short).font(theme::semibold(10.5)).color(theme::method_color(method, dark))
}

/// A row of tabs (`Params | Auth | Headers ...`) with an underlined
/// selection; a trailing ` (n)` becomes a count badge.
pub fn sub_tabs<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, items: &[(T, String)]) {
    crate::kit::tab_strip(ui, current, items);
}

pub fn count_label(n: usize) -> String {
    if n == 0 { String::new() } else { format!(" ({n})") }
}

pub fn muted(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(theme::muted(is_dark(ui)))
}

/// Masked secret input with a reveal toggle (an eye inside the field).
pub fn secret_field(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    width: f32,
) -> egui::Response {
    crate::auth_ui::secret_input(ui, id, text, width, true, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pm_completion_prefixes() {
        let text = "const x = pm.envi";
        let (start, items) = pm_completions(text, text.len()).unwrap();
        assert_eq!(&text[start..], "pm.envi");
        assert!(items.iter().any(|i| i.1 == "pm.environment.get("));
        assert!(pm_completions("foo.bar", 7).is_none());
        let text = "pm.response.to.have.status(200)";
        assert!(pm_completions(text, text.len()).is_none());
    }

    /// The provider runs every frame while a script editor has focus, so a
    /// multi-byte character right before the cursor must not panic.
    #[test]
    fn pm_completion_after_multibyte_chars() {
        for text in ["console.log(\"✅", "a — b —", "x = “", "// 日本語。"] {
            assert!(pm_completions(text, text.len()).is_none(), "{text}");
        }
        let text = "—pm.envi";
        let (start, items) = pm_completions(text, text.len()).unwrap();
        assert_eq!(&text[start..], "pm.envi");
        assert!(!items.is_empty());
        // The GraphQL query editor uses the same helper.
        let gql = |c: char| c.is_alphanumeric() || c == '_';
        assert_eq!(word_start("{ “user", gql), "{ “".len());
        assert_eq!(word_start("…", gql), "…".len());
        assert_eq!(word_start("user", gql), 0);
    }

    /// Params can change while their bulk editor is open (the URL bar edits
    /// them); the editor must then show them instead of overwriting them
    /// with its old text on the next keystroke.
    #[test]
    fn bulk_text_follows_rows_changed_elsewhere() {
        let mut rows = vec![KeyValue::new("a", "1")];
        let typed = "a:1\n".to_string();
        assert_eq!(bulk_text(typed.clone(), &rows), typed);
        rows.push(KeyValue::new("page", "2"));
        rows[0].enabled = false;
        let text = bulk_text(typed, &rows);
        assert_eq!(text, "//a:1\npage:2");
        assert_eq!(bulk_parse(&text), rows);
    }
}

/// Opens a link from a template, spec, collection or server in the browser:
/// only `http(s):` and `mailto:` links, so a crafted link cannot launch a
/// local file or app (`file:`, custom URL schemes). Returns whether it was opened.
pub fn open_web_link(url: &str) -> bool {
    let scheme = url.trim().split_once(':').map(|(s, _)| s.to_ascii_lowercase());
    let allowed = matches!(scheme.as_deref(), Some("http" | "https" | "mailto"));
    if allowed {
        let _ = open::that(url.trim());
    } else {
        tracing::warn!("not opening a link with an unsupported scheme");
    }
    allowed
}

#[cfg(test)]
mod link_tests {
    #[test]
    fn only_web_links_are_opened() {
        // Not opened (an `open` call here would launch them).
        for url in [
            "file:///System/Applications/Calculator.app",
            "/Applications/Calculator.app",
            "x-apple.systempreferences:",
            "javascript:alert(1)",
            "vscode://file/etc/passwd",
            "",
        ] {
            assert!(!super::open_web_link(url), "{url}");
        }
    }
}
