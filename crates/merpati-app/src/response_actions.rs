//! Actions on a response that change the request or the variables: "Set as
//! variable" and "Add test" (from the body's context menu), "Generate
//! tests" (preview dialog), and the notice that confirms them.

use std::time::{Duration, Instant};

use egui::{Align2, CornerRadius, RichText, Stroke, Ui, vec2};
use serde_json::Value;

use merpati_core::model::RequestItem;
use merpati_core::variables::{VarType, Variable};

use crate::app::MerpatiApp;
use crate::dialogs::{Modal, Outcome, body, field_label, footer, trailing};
use crate::i18n::{tr, trf};
use crate::test_gen::{self, Seg, VarScope};
use crate::{icons, kit, theme, widgets};

/// Something asked of a value of the response body.
#[derive(Clone, Debug)]
pub enum ValueAction {
    SetVariable { path: String, value: Value },
    AddTest { path: String, value: Value },
}

impl From<crate::body_view::BodyAction> for ValueAction {
    fn from(a: crate::body_view::BodyAction) -> Self {
        match a {
            crate::body_view::BodyAction::SetVariable { path, value } => ValueAction::SetVariable { path, value },
            crate::body_view::BodyAction::AddTest { path, value } => ValueAction::AddTest { path, value },
        }
    }
}

pub struct SetVarDialog {
    path: Vec<Seg>,
    value: Value,
    name: String,
    /// Index into the scopes offered (see [`scopes`]).
    scope: usize,
    secret: bool,
    also_script: bool,
}

pub struct GenDialog {
    code: String,
    count: usize,
    status: String,
}

/// A short confirmation at the bottom of the response panel, with actions.
pub struct Notice {
    text: String,
    at: Instant,
    open_scripts: bool,
    run_tests: bool,
}

/// Dialogs and notice of one request tab.
#[derive(Default)]
pub struct Tools {
    pub set_var: Option<SetVarDialog>,
    pub generator: Option<GenDialog>,
    pub notice: Option<Notice>,
    /// UI automation: press "Add tests" in the open preview.
    pub auto_insert: bool,
}

/// What the tab should do after the dialogs ran.
#[derive(Default)]
pub struct After {
    pub rerun_tests: bool,
    pub open_scripts: bool,
    /// The request's scripts changed.
    pub changed: bool,
}

impl Tools {
    pub fn notify(&mut self, text: impl Into<String>, open_scripts: bool, run_tests: bool) {
        self.notice = Some(Notice { text: text.into(), at: Instant::now(), open_scripts, run_tests });
    }

    /// Opens the "Generate tests" preview for `resp`.
    pub fn generate(&mut self, resp: &merpati_core::response::HttpResponse) {
        let json = (resp.language() == merpati_core::response::BodyLanguage::Json
            && resp.body.len() <= 8 * 1024 * 1024)
            .then(|| resp.body.read_all().ok())
            .flatten()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let facts = test_gen::ResponseFacts {
            status: resp.status,
            status_text: &resp.status_text,
            time_ms: resp.timings.total_ms,
            content_type: resp.content_type(),
            json,
            body_empty: resp.body.is_empty(),
        };
        let (code, count) = test_gen::generate(&facts);
        let status = format!("{} {}", resp.status, resp.status_text).trim().to_string();
        self.generator = Some(GenDialog { code, count, status });
    }
}

/// Scopes "Set as variable" can write to, with their labels.
fn scopes(app: &MerpatiApp, collection: Option<&str>) -> Vec<(VarScope, String)> {
    let mut out = Vec::new();
    if let Some(env) = app.ws.data.active_environment() {
        out.push((VarScope::Environment(env.id.clone()), env.name.clone()));
    }
    if let Some(c) = collection.and_then(|c| app.ws.data.collection(c)) {
        out.push((VarScope::Collection(c.id.clone()), tr("Collection").to_string()));
    }
    out.push((VarScope::Globals, tr("Globals").to_string()));
    out
}

fn vars_mut<'a>(app: &'a mut MerpatiApp, scope: &VarScope) -> Option<&'a mut Vec<Variable>> {
    match scope {
        VarScope::Environment(id) => {
            app.ws.data.environments.iter_mut().find(|e| &e.id == id).map(|e| &mut e.variables)
        }
        VarScope::Collection(id) => app.ws.data.collection_mut(id).map(|c| &mut c.variables),
        VarScope::Globals => Some(&mut app.ws.data.globals),
    }
}

fn vars<'a>(app: &'a MerpatiApp, scope: &VarScope) -> Option<&'a [Variable]> {
    match scope {
        VarScope::Environment(id) => app.ws.data.environment(id).map(|e| e.variables.as_slice()),
        VarScope::Collection(id) => app.ws.data.collection(id).map(|c| c.variables.as_slice()),
        VarScope::Globals => Some(app.ws.data.globals.as_slice()),
    }
}

/// Words that suggest a value should be kept secret.
fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "apikey", "api_key", "api-key", "auth", "session", "cookie", "private"]
        .iter()
        .any(|w| n.contains(w))
}

/// Sets `name` to `value` in `scope` the way scripts do: as the current
/// value, which stays on this computer (secret ones in the secret store).
pub fn set_variable(app: &mut MerpatiApp, scope: &VarScope, name: &str, value: &Value, secret: bool) -> bool {
    let Some(list) = vars_mut(app, scope) else { return false };
    match list.iter_mut().rev().find(|v| v.key == name) {
        Some(v) => {
            let declared = matches!(v.ty, VarType::String | VarType::Number | VarType::Boolean).then_some(v.ty);
            let (text, ty) = VarType::cast(declared, value);
            v.current = Some(text);
            v.ty = ty;
            v.enabled = true;
        }
        None => {
            let (text, ty) = VarType::cast(None, value);
            let mut v = if secret { Variable::secret(name, "") } else { Variable::new(name, "") };
            v.current = Some(text);
            v.ty = ty;
            list.push(v);
        }
    }
    match scope {
        VarScope::Environment(id) => app.ws.mark_env(id),
        VarScope::Collection(id) => app.ws.mark_collection(id),
        VarScope::Globals => app.ws.mark_globals(),
    }
    true
}

/// Handles a body value action: "Add test" right away, "Set as variable"
/// through its dialog.
pub fn start(tools: &mut Tools, action: ValueAction, item: &mut RequestItem) -> bool {
    match action {
        ValueAction::SetVariable { path, value } => {
            let path = test_gen::parse_path(&path);
            let name = test_gen::suggested_name(&path);
            let secret = looks_secret(&name);
            tools.set_var = Some(SetVarDialog { path, value, name, scope: 0, secret, also_script: false });
            false
        }
        ValueAction::AddTest { path, value } => {
            let segs = test_gen::parse_path(&path);
            let code = test_gen::value_test(&segs, &value);
            let script = &mut item.scripts.post_response;
            if script.contains(code.trim_end()) {
                tools.notify(tr("This test is already in the Post-response script."), true, false);
                return false;
            }
            test_gen::append_code(script, &code);
            tools.notify(
                trf("Test added: {}", &[&test_name(&code).unwrap_or_else(|| test_gen::display_path(&segs))]),
                true,
                true,
            );
            true
        }
    }
}

/// The name of the first `pm.test` in `code`.
fn test_name(code: &str) -> Option<String> {
    let rest = code.split_once("pm.test(")?.1;
    let end = rest.find(", () =>")?;
    serde_json::from_str(&rest[..end]).ok()
}

/// Draws the open dialog of the tab, if any.
pub fn dialogs(
    app: &mut MerpatiApp,
    ctx: &egui::Context,
    tools: &mut Tools,
    item: &mut RequestItem,
    collection: Option<&str>,
) -> After {
    let mut after = After::default();
    if tools.set_var.is_some() {
        set_var_dialog(app, ctx, tools, item, collection, &mut after);
    }
    let auto_insert = std::mem::take(&mut tools.auto_insert);
    if let Some(d) = &mut tools.generator {
        let mut insert = auto_insert;
        let subtitle =
            trf("From the {} response · {}", &[&d.status, &crate::dialogs::plural(d.count, "{} test", "{} tests")]);
        let out = Modal::new("generate-tests", 720.0)
            .title(tr("Generate tests"))
            .subtitle(subtitle)
            .icon(icons::TEST_TUBE)
            .sticky()
            .show(ctx, |ui| {
                body(ui, |ui| {
                    ui.label(kit::hint(ui, tr("Review or edit the tests, then add them to the Post-response script.")));
                    ui.add_space(8.0);
                    let rows = ((crate::dialogs::fit_height(ctx, 300.0, 460.0)) / 18.0) as usize;
                    egui::ScrollArea::vertical().id_salt("gen-tests-code").max_height(rows as f32 * 18.0 + 16.0).show(
                        ui,
                        |ui| {
                            widgets::code_editor_with(
                                ui,
                                "gen-tests",
                                &mut d.code,
                                crate::highlight::Lang::JavaScript,
                                None,
                                rows.max(8),
                                false,
                                Some(&widgets::pm_completions),
                            );
                        },
                    );
                });
                footer(ui, |ui| {
                    ui.label(kit::hint(ui, tr("The tests run on this response right after they are added.")));
                    trailing(ui, |ui| {
                        if kit::primary_icon(ui, icons::PLUS, tr("Add tests")).clicked() {
                            insert = true;
                        }
                        if kit::secondary(ui, None, tr("Cancel")).clicked() {
                            return Outcome::Close;
                        }
                        Outcome::Keep
                    })
                })
            });
        if insert {
            let code = std::mem::take(&mut d.code);
            if !code.trim().is_empty() {
                test_gen::append_code(&mut item.scripts.post_response, &code);
                after.changed = true;
                after.rerun_tests = true;
                tools.notify(tr("Tests added to the Post-response script"), true, false);
            }
            tools.generator = None;
        } else if matches!(out, Outcome::Close) {
            tools.generator = None;
        }
    }
    after
}

fn set_var_dialog(
    app: &mut MerpatiApp,
    ctx: &egui::Context,
    tools: &mut Tools,
    item: &mut RequestItem,
    collection: Option<&str>,
    after: &mut After,
) {
    let Some(d) = &mut tools.set_var else { return };
    let options = scopes(app, collection);
    d.scope = d.scope.min(options.len().saturating_sub(1));
    let scope = options[d.scope].0.clone();
    let name = d.name.trim().to_string();
    let existing = vars(app, &scope).and_then(|v| v.iter().rev().find(|v| v.key == name)).cloned();
    let valid = !name.is_empty() && !name.contains('{') && !name.contains('}');
    let line = test_gen::set_variable_line(&scope, &name, &d.path);
    let mut apply = false;
    let out = Modal::new("set-variable", 540.0)
        .title(tr("Set as variable"))
        .subtitle(test_gen::display_path(&d.path))
        .icon(icons::BRACKETS_CURLY)
        .show(ctx, |ui| {
            let p = theme::pal(ui);
            body(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                field_label(ui, tr("Name"));
                let r = ui.add(crate::dialogs::input(&mut d.name).desired_width(ui.available_width()));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && valid {
                    apply = true;
                }
                if let Some(v) = &existing {
                    let text = if v.is_secret() {
                        tr("A secret variable with this name exists; its value is replaced and stays in the secret store.")
                    } else {
                        tr("A variable with this name exists; its current value is replaced.")
                    };
                    ui.label(kit::hint(ui, text));
                }
                ui.add_space(6.0);
                field_label(ui, tr("Scope"));
                let labels: Vec<String> = options
                    .iter()
                    .map(|(s, label)| match s {
                        VarScope::Environment(_) => format!("{}  {label}", icons::GLOBE),
                        VarScope::Collection(_) => format!("{}  {label}", icons::STACK),
                        VarScope::Globals => format!("{}  {label}", icons::GLOBE_SIMPLE),
                    })
                    .collect();
                let items: Vec<(usize, &str)> = labels.iter().enumerate().map(|(i, l)| (i, l.as_str())).collect();
                kit::segmented(ui, &mut d.scope, &items);
                ui.add_space(6.0);
                field_label(ui, tr("Value"));
                let (kind, text) = match &d.value {
                    Value::String(s) => ("string", s.clone()),
                    Value::Number(_) => ("number", d.value.to_string()),
                    Value::Bool(_) => ("boolean", d.value.to_string()),
                    Value::Null => ("null", "null".to_string()),
                    Value::Array(_) => ("array", serde_json::to_string(&d.value).unwrap_or_default()),
                    Value::Object(_) => ("object", serde_json::to_string(&d.value).unwrap_or_default()),
                };
                let masked = d.secret || existing.as_ref().is_some_and(|v| v.is_secret());
                let shown = if masked {
                    "•".repeat(text.chars().count().clamp(6, 24))
                } else {
                    crate::dialogs::ellipsize(&text, 400)
                };
                egui::Frame::new()
                    .fill(p.chrome)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(egui::Margin::symmetric(10, 7))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal_top(|ui| {
                            let w = ui.available_width() - 70.0;
                            ui.allocate_ui_with_layout(vec2(w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                                ui.set_width(w);
                                ui.add(egui::Label::new(RichText::new(shown).monospace().color(p.text)).wrap());
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                kit::pill(ui, kind, p.text_muted);
                            });
                        });
                    });
                if existing.is_none() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        crate::dialogs::toggle(ui, &mut d.secret, tr("Secret"));
                        ui.label(RichText::new(tr("Secret")).color(p.text));
                        ui.label(kit::hint(ui, tr("Masked in the app, kept in the secret store")));
                    });
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    crate::dialogs::toggle(ui, &mut d.also_script, tr("Also set it on every run"));
                    ui.label(RichText::new(tr("Also set it on every run")).color(p.text));
                });
                ui.label(kit::hint(ui, tr("Adds this line to the Post-response script:")));
                egui::Frame::new()
                    .fill(if d.also_script { theme::soft(p.ok, 0.08) } else { p.chrome })
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(egui::Margin::symmetric(10, 7))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let color = if d.also_script { p.text } else { p.text_muted };
                        ui.add(egui::Label::new(RichText::new(&line).monospace().size(11.5).color(color)).wrap());
                    });
            });
            footer(ui, |ui| {
                ui.label(kit::hint(ui, tr("Saved as the current value, which stays on this computer.")));
                trailing(ui, |ui| {
                    ui.add_enabled_ui(valid, |ui| {
                        if kit::primary(ui, tr("Set variable")).clicked() {
                            apply = true;
                        }
                    });
                    if kit::secondary(ui, None, tr("Cancel")).clicked() {
                        return Outcome::Close;
                    }
                    Outcome::Keep
                })
            })
        });
    if apply && valid {
        let secret = d.secret && existing.is_none();
        let (value, also, label) = (d.value.clone(), d.also_script, options[d.scope].1.clone());
        if set_variable(app, &scope, &name, &value, secret) {
            if also && !item.scripts.post_response.contains(&line) {
                test_gen::append_code(&mut item.scripts.post_response, &line);
                after.changed = true;
            }
            app.toast(trf("Variable “{}” set in {}", &[&name, &label]), false);
        }
        tools.set_var = None;
    } else if matches!(out, Outcome::Close) {
        tools.set_var = None;
    }
}

/// How long a notice stays (unless hovered).
const NOTICE_TIME: Duration = Duration::from_secs(8);

/// The notice floating at the bottom of the response panel (`rect`).
/// `scripts_shown`: the post-response script is open already (no "Open
/// Scripts" button).
pub fn notice(ui: &mut Ui, tools: &mut Tools, rect: egui::Rect, scripts_shown: bool, after: &mut After) {
    let Some(n) = &tools.notice else { return };
    let p = theme::pal(ui);
    let id = ui.make_persistent_id("resp-notice");
    let hovered_before: bool = ui.data(|d| d.get_temp(id).unwrap_or(false));
    if !hovered_before && n.at.elapsed() > NOTICE_TIME {
        tools.notice = None;
        return;
    }
    let mut close = false;
    let area = egui::Area::new(id)
        // Under dialogs and menus, over the panels.
        .order(egui::Order::Middle)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_pos(egui::pos2(rect.center().x, rect.bottom() - 12.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.border_strong))
                .corner_radius(CornerRadius::same(10))
                .shadow(ui.visuals().popup_shadow)
                .inner_margin(egui::Margin { left: 12, right: 6, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.set_max_width((rect.width() - 40.0).max(240.0));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new(icons::CHECK_CIRCLE).font(theme::icon_font(16.0)).color(p.ok));
                        ui.add(egui::Label::new(RichText::new(&n.text).color(p.text)).truncate());
                        if n.run_tests && kit::ghost(ui, Some(icons::PLAY), tr("Run tests")).clicked() {
                            after.rerun_tests = true;
                            close = true;
                        }
                        if n.open_scripts
                            && !scripts_shown
                            && kit::ghost(ui, Some(icons::CODE), tr("Open Scripts")).clicked()
                        {
                            after.open_scripts = true;
                            close = true;
                        }
                        if kit::icon_button_ex(ui, icons::X, tr("Close"), false, 24.0).clicked() {
                            close = true;
                        }
                    });
                });
        });
    let hovered = area.response.contains_pointer();
    ui.data_mut(|d| d.insert_temp(id, hovered));
    if close {
        tools.notice = None;
    } else if !hovered {
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
    if hovered && let Some(n) = &mut tools.notice {
        // Hovering keeps it; the timer starts again when the pointer leaves.
        n.at = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_names_are_read_back() {
        let code = test_gen::value_test(&test_gen::parse_path("$.a"), &serde_json::json!(1));
        assert_eq!(test_name(&code).as_deref(), Some("a is 1"));
        assert!(looks_secret("access_token"));
        assert!(!looks_secret("id"));
    }

    /// "Set as variable" writes the current value only: nothing reaches the
    /// shared file, and secret values go to the secret store.
    #[test]
    fn set_variable_writes_current_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = MerpatiApp::for_test(dir.path());
        assert!(set_variable(&mut app, &VarScope::Globals, "id", &serde_json::json!(42), false));
        let v = app.ws.data.globals.iter().find(|v| v.key == "id").unwrap();
        assert_eq!(v.value, "");
        assert_eq!(v.current.as_deref(), Some("42"));
        assert_eq!(v.ty, VarType::Any);
        assert!(set_variable(&mut app, &VarScope::Globals, "token", &serde_json::json!("s3cr3t"), true));
        let t = app.ws.data.globals.iter().find(|v| v.key == "token").unwrap();
        assert!(t.is_secret());
        assert_eq!(t.value, "");
        // An existing variable keeps its kind and gets a new current value.
        assert!(set_variable(&mut app, &VarScope::Globals, "token", &serde_json::json!("0th3r-v4lue"), false));
        let t = app.ws.data.globals.iter().find(|v| v.key == "token").unwrap();
        assert!(t.is_secret());
        assert_eq!(t.current.as_deref(), Some("0th3r-v4lue"));
        assert_eq!(app.ws.data.globals.len(), 2);
        app.ws.flush_blocking();
        let files: Vec<_> = walk(dir.path());
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap_or_default();
            assert!(!text.contains("s3cr3t") && !text.contains("0th3r-v4lue"), "{} has a secret", f.display());
        }
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }
}
