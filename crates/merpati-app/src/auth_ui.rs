//! Auth editor shared by requests, folders and collections. Forms are built
//! from `merpati_auth::fields`, so every scheme gets an editor for free.

use egui::{Align2, Color32, CornerRadius, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use merpati_auth::{FieldKind, FieldSpec};
use merpati_core::auth::{Auth, AuthType};
use merpati_core::tokens::OAuth2Token;

use crate::i18n::{tr, trf};
use crate::request_tab::{banner, elide, form_divider, form_row};
use crate::theme;
use crate::widgets::{self, VarCtx};
use crate::{icons, kit};

pub struct AuthUiCtx<'a> {
    /// Collection level: `Inherit` is not offered.
    pub is_root: bool,
    /// Where the inherited auth comes from (name, type), for display.
    pub inherited: Option<(String, AuthType)>,
    /// Key used to store OAuth 2.0 tokens for this auth.
    pub owner_id: &'a str,
    pub token: Option<OAuth2Token>,
    pub fetching: bool,
    pub vars: &'a VarCtx,
}

pub enum AuthAction {
    None,
    GetToken,
    ClearToken,
}

/// What the signers in merpati-auth use when a parameter is missing (as in
/// imported collections). New configs get `f.default` written, which can
/// differ from this fallback.
fn missing_value(kind: AuthType, f: &FieldSpec) -> &'static str {
    match (kind, f.key) {
        // Signed into the URL or body unless set.
        (AuthType::OAuth1, "addParamsToHeader") => "false",
        // The redirect is received on a random 127.0.0.1 port.
        (AuthType::OAuth2, "redirect_uri") => "",
        _ => f.default,
    }
}

fn value_text(auth: &Auth, f: &FieldSpec) -> String {
    match auth.params.get(f.key) {
        Some(_) => auth.get_string(f.key),
        None => missing_value(auth.kind, f).to_string(),
    }
}

fn bool_value(auth: &Auth, f: &FieldSpec) -> bool {
    auth.get_bool(f.key, missing_value(auth.kind, f) == "true")
}

/// One sentence about each auth type, shown under the type picker.
fn describe(kind: AuthType) -> &'static str {
    match kind {
        AuthType::Inherit => tr("Use the auth of the parent folder or collection."),
        AuthType::None => tr("No authorization is sent with this request."),
        AuthType::ApiKey => tr("Send a key and value as a header or query parameter."),
        AuthType::Bearer => tr("Send a token in the Authorization header."),
        AuthType::Basic => tr("Send a username and password in the Authorization header."),
        AuthType::Digest => tr("Answer the server's challenge with a hashed password."),
        AuthType::Jwt => tr("Sign a JSON Web Token and send it with the request."),
        AuthType::OAuth1 => tr("Sign the request with OAuth 1.0 consumer and token credentials."),
        AuthType::OAuth2 => tr("Get an access token from an authorization server and send it."),
        AuthType::Hawk => tr("Sign the request with Hawk credentials."),
        AuthType::AwsV4 => tr("Sign the request with AWS Signature Version 4."),
        AuthType::Ntlm => tr("Windows NTLM challenge-response authentication."),
        AuthType::EdgeGrid => tr("Sign the request for Akamai EdgeGrid APIs."),
        AuthType::Asap => tr("Sign a token with the Atlassian S2S Authentication Protocol."),
    }
}

/// A field-like button that opens a menu (`Popup::menu` on the response).
pub(crate) fn select_button(ui: &mut Ui, icon: Option<&str>, text: &str, width: f32) -> egui::Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 30.0), Sense::click());
    let enabled = ui.is_enabled();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, enabled, text));
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        let stroke = if hovered { p.border_strong } else { p.border };
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::same(theme::RADIUS), p.surface, Stroke::new(1.0, stroke), StrokeKind::Inside);
        if resp.has_focus() {
            painter.rect_stroke(
                rect,
                CornerRadius::same(theme::RADIUS),
                Stroke::new(1.5, p.accent),
                StrokeKind::Inside,
            );
        }
        let mut x = rect.left() + 10.0;
        if let Some(icon) = icon {
            painter.text(
                pos2(x + 7.0, rect.center().y),
                Align2::CENTER_CENTER,
                icon,
                theme::icon_font(15.0),
                p.text_muted,
            );
            x += 22.0;
        }
        let g = elide(ui, text, theme::medium(13.0), p.text, rect.right() - x - 28.0);
        ui.painter().galley(pos2(x, rect.center().y - g.size().y / 2.0), g, p.text);
        ui.painter().text(
            pos2(rect.right() - 14.0, rect.center().y),
            Align2::CENTER_CENTER,
            icons::CARET_DOWN,
            theme::icon_font(13.0),
            p.text_muted,
        );
    }
    resp
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// An on/off switch. Returns a response that is `changed()` when toggled.
pub(crate) fn switch(ui: &mut Ui, on: &mut bool, name: &str) -> egui::Response {
    let p = theme::pal(ui);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(34.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let enabled = ui.is_enabled();
    let value = *on;
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, enabled, value, name));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let off = if resp.hovered() { p.text_faint } else { p.border_strong };
        let fill = mix(off, p.accent, t);
        let fill = if enabled { fill } else { fill.gamma_multiply(0.45) };
        ui.painter().rect_filled(rect, CornerRadius::same(10), fill);
        let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
        ui.painter().circle_filled(pos2(x, rect.center().y), 7.0, Color32::WHITE);
        if resp.has_focus() {
            ui.painter().rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(12),
                Stroke::new(1.5, p.accent),
                StrokeKind::Outside,
            );
        }
    }
    resp
}

/// A masked single-line input with a show/hide eye inside it. `framed`
/// false draws no frame (table cells).
pub(crate) fn secret_input(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    width: f32,
    framed: bool,
    hint: &str,
) -> egui::Response {
    let p = theme::pal(ui);
    let id = ui.make_persistent_id(id);
    let reveal_id = id.with("reveal");
    let mut reveal: bool = ui.data(|d| d.get_temp(reveal_id).unwrap_or(false));
    let mut edit = egui::TextEdit::singleline(text)
        .id(id)
        .password(!reveal)
        .hint_text(hint)
        .desired_width(width)
        .font(egui::TextStyle::Monospace);
    if framed {
        edit = edit.margin(egui::Margin { left: 8, right: 30, top: 5, bottom: 5 });
    } else {
        let h = ui.available_height().clamp(20.0, 34.0);
        edit = edit
            .frame(egui::Frame::NONE)
            .margin(egui::Margin { left: 6, right: 28, top: 2, bottom: 2 })
            .min_size(vec2(0.0, h))
            .align(Align2::LEFT_CENTER);
    }
    let r = ui.add(edit);
    let eye = Rect::from_center_size(pos2(r.rect.right() - 15.0, r.rect.center().y), vec2(22.0, 22.0));
    let e = ui.interact(eye, id.with("eye"), Sense::click());
    let label = if reveal { tr("Hide value") } else { tr("Show value") };
    widgets::a11y_button(&e, label);
    if e.hovered() {
        ui.painter().rect_filled(eye, CornerRadius::same(5), p.hover);
    }
    let color = if e.hovered() { p.text } else { p.text_muted };
    ui.painter().text(
        eye.center(),
        Align2::CENTER_CENTER,
        if reveal { icons::EYE_SLASH } else { icons::EYE },
        theme::icon_font(14.0),
        color,
    );
    if e.on_hover_text(label).clicked() {
        reveal = !reveal;
        ui.data_mut(|d| d.insert_temp(reveal_id, reveal));
    }
    r
}

fn field_ui(ui: &mut Ui, auth: &mut Auth, f: &FieldSpec, vars: &VarCtx) -> bool {
    let mut changed = false;
    form_row(ui, tr(f.label), tr(f.help), |ui| {
        let width = ui.available_width().min(560.0);
        match f.kind {
            FieldKind::Text => {
                let mut v = value_text(auth, f);
                if widgets::var_line(ui, f.key, &mut v, vars, "", width).changed() {
                    auth.set(f.key, v);
                    changed = true;
                }
            }
            FieldKind::Secret => {
                let mut v = value_text(auth, f);
                let is_ref = merpati_core::variables::is_pure_reference(&v);
                let r = if is_ref {
                    widgets::var_line(ui, f.key, &mut v, vars, "", width)
                } else {
                    secret_input(ui, f.key, &mut v, width, true, "")
                };
                if r.changed() {
                    auth.set(f.key, v);
                    changed = true;
                }
            }
            FieldKind::Multiline => {
                let mut v = value_text(auth, f);
                ui.set_width(width);
                let lang = if f.key.to_lowercase().contains("key") {
                    crate::highlight::Lang::Text
                } else {
                    crate::highlight::Lang::Json
                };
                if widgets::code_editor(ui, f.key, &mut v, lang, Some(vars), 3, false).changed() {
                    auth.set(f.key, v);
                    changed = true;
                }
            }
            FieldKind::Bool => {
                let mut b = bool_value(auth, f);
                ui.add_space(4.0);
                if switch(ui, &mut b, tr(f.label)).changed() {
                    auth.set(f.key, b);
                    changed = true;
                }
            }
            FieldKind::Select(options) => {
                let current = value_text(auth, f);
                let label = options
                    .iter()
                    .find(|(v, _)| *v == current)
                    .map(|(_, l)| *l)
                    .unwrap_or(current.as_str())
                    .to_string();
                let w = width.min(280.0);
                let r = select_button(ui, None, &label, w);
                egui::Popup::menu(&r).width(w).show(|ui| {
                    for (v, l) in options.iter() {
                        let mark = if current == *v { icons::CHECK } else { "    " };
                        if ui.button(format!("{mark}  {l}")).clicked() {
                            auth.set(f.key, v.to_string());
                            changed = true;
                            ui.close();
                        }
                    }
                });
            }
        }
    });
    changed
}

/// The auth type picker with a sentence about the chosen type.
fn type_picker(ui: &mut Ui, auth: &mut Auth, is_root: bool, width: f32) -> bool {
    let p = theme::pal(ui);
    let mut changed = false;
    ui.label(RichText::new(tr("Auth type")).font(theme::medium(12.0)).color(p.text_muted));
    ui.add_space(2.0);
    let r = select_button(ui, Some(icons::KEY), tr(auth.kind.label()), width);
    egui::Popup::menu(&r).width(width).show(|ui| {
        for t in AuthType::ALL {
            if is_root && t == AuthType::Inherit {
                continue;
            }
            let mark = if auth.kind == t { icons::CHECK } else { "    " };
            if ui.button(format!("{mark}  {}", tr(t.label()))).on_hover_text(describe(t)).clicked() {
                if auth.kind != t {
                    let mut fresh = Auth::new(t);
                    // Sensible defaults for new configs.
                    for f in merpati_auth::fields(t) {
                        if !f.default.is_empty() {
                            match f.kind {
                                FieldKind::Bool => fresh.set(f.key, f.default == "true"),
                                _ => fresh.set(f.key, f.default.to_string()),
                            }
                        }
                    }
                    *auth = fresh;
                    changed = true;
                }
                ui.close();
            }
            if t == AuthType::None {
                ui.separator();
            }
        }
    });
    ui.add_space(6.0);
    ui.label(RichText::new(describe(auth.kind)).small().color(p.text_muted));
    changed
}

/// Returns (changed, action).
pub fn auth_editor(ui: &mut Ui, auth: &mut Auth, ctx: AuthUiCtx<'_>) -> (bool, AuthAction) {
    let mut changed = false;
    let mut action = AuthAction::None;
    let width = ui.available_width();
    if width >= 760.0 {
        let left_w = 250.0;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(vec2(left_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(left_w);
                changed |= type_picker(ui, auth, ctx.is_root, left_w);
            });
            ui.add_space(20.0);
            let right_w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(right_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(right_w);
                auth_body(ui, auth, &ctx, &mut changed, &mut action);
            });
        });
    } else {
        changed |= type_picker(ui, auth, ctx.is_root, width.min(320.0));
        ui.add_space(14.0);
        auth_body(ui, auth, &ctx, &mut changed, &mut action);
    }
    (changed, action)
}

fn auth_body(ui: &mut Ui, auth: &mut Auth, ctx: &AuthUiCtx<'_>, changed: &mut bool, action: &mut AuthAction) {
    let p = theme::pal(ui);
    match auth.kind {
        AuthType::Inherit => {
            inherited_card(ui, ctx.inherited.as_ref());
            return;
        }
        AuthType::None => {
            banner(ui, icons::LOCK_SIMPLE, p.text_muted, tr("This request does not use any authorization."));
            return;
        }
        _ => {}
    }
    // ASAP used to store `issuer`/`subject`/`audience`; Postman's names win now.
    if auth.kind == AuthType::Asap {
        for (old, new) in [("issuer", "iss"), ("subject", "sub"), ("audience", "aud")] {
            if let Some(v) = auth.params.shift_remove(old)
                && !auth.params.contains_key(new)
            {
                auth.params.insert(new.to_string(), v);
                *changed = true;
            }
        }
    }
    let fields = merpati_auth::fields(auth.kind);
    let basic: Vec<&FieldSpec> = fields.iter().filter(|f| !f.is_advanced()).collect();
    let advanced: Vec<&FieldSpec> = fields.iter().filter(|f| f.is_advanced()).collect();
    if !basic.is_empty() {
        kit::card(ui).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, f) in basic.iter().enumerate() {
                if i > 0 {
                    form_divider(ui);
                }
                *changed |= field_ui(ui, auth, f, ctx.vars);
            }
        });
    }
    if !advanced.is_empty() {
        ui.add_space(12.0);
        let id = ui.make_persistent_id(("auth-adv", auth.kind.as_str()));
        if crate::request_tab::disclosure(ui, id, tr("Advanced"), Some(advanced.len()), false) {
            ui.label(kit::hint(ui, tr("Most servers work with the defaults; change these only when told to.")));
            ui.add_space(4.0);
            kit::card(ui).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                for (i, f) in advanced.iter().enumerate() {
                    if i > 0 {
                        form_divider(ui);
                    }
                    *changed |= field_ui(ui, auth, f, ctx.vars);
                }
            });
        }
    }
    if auth.kind == AuthType::OAuth2 {
        ui.add_space(16.0);
        token_card(ui, ctx, action);
    }
}

/// Where an inherited auth comes from.
fn inherited_card(ui: &mut Ui, inherited: Option<&(String, AuthType)>) {
    let p = theme::pal(ui);
    let dark = ui.visuals().dark_mode;
    egui::Frame::new()
        .fill(theme::soft(p.info, if dark { 0.10 } else { 0.06 }))
        .stroke(Stroke::new(1.0, theme::soft(p.info, if dark { 0.28 } else { 0.22 })))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                crate::request_tab::icon_tile(ui, icons::SHIELD_CHECK, p.info, 32.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    match inherited {
                        Some((name, kind)) => {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(trf("Inherited from “{}”", &[name])).font(theme::semibold(13.5)),
                                );
                                kit::pill(ui, tr(kind.label()), p.info);
                            });
                            ui.label(
                                RichText::new(trf("This request uses {} from “{}”.", &[&tr(kind.label()), name]))
                                    .color(p.text_muted),
                            );
                            ui.label(kit::hint(ui, tr("Change it in the folder or collection settings.")));
                        }
                        None => {
                            ui.label(RichText::new(tr("Nothing to inherit")).font(theme::semibold(13.5)));
                            ui.label(
                                RichText::new(tr("No auth is set on any parent; the request is sent without auth."))
                                    .color(p.text_muted),
                            );
                        }
                    }
                });
            });
        });
}

/// Seconds as `45 s`, `12 min`, `3 h`, `2 d`.
fn short_duration(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86_400 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86_400),
    }
}

/// The stored OAuth 2.0 token: state, value, expiry and the actions.
fn token_card(ui: &mut Ui, ctx: &AuthUiCtx<'_>, action: &mut AuthAction) {
    let p = theme::pal(ui);
    kit::card(ui).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.set_height(26.0);
            ui.label(RichText::new(icons::KEY).font(theme::icon_font(15.0)).color(p.text_muted));
            ui.label(RichText::new(tr("Current token")).font(theme::semibold(13.5)));
            let now = chrono::Utc::now().timestamp();
            let expired = ctx.token.as_ref().and_then(|t| t.expires_at).is_some_and(|e| e <= now);
            if ctx.fetching {
                kit::pill(ui, tr("Waiting for the authorization server…"), p.info);
                ui.add(egui::Spinner::new().size(14.0).color(p.info));
            } else if ctx.token.is_none() {
                kit::pill(ui, tr("No token"), p.text_muted);
            } else if expired {
                kit::pill(ui, &format!("{} {}", icons::WARNING_CIRCLE, tr("Expired")), p.err);
            } else {
                kit::pill(ui, &format!("{} {}", icons::CHECK, tr("Active")), p.ok);
            }
        });
        ui.add_space(6.0);
        match &ctx.token {
            Some(t) => {
                let kind = if t.token_type.is_empty() { "Bearer" } else { &t.token_type };
                ui.horizontal(|ui| {
                    egui::Frame::new()
                        .fill(p.chrome)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.set_width((ui.available_width() - 44.0).max(120.0));
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(kind).font(theme::medium(12.0)).color(p.text_muted));
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(crate::dialogs::ellipsize(&t.access_token, 48)).monospace(),
                                    )
                                    .truncate(),
                                );
                            });
                        });
                    if kit::icon_button(ui, icons::COPY, tr("Copy token")).clicked() {
                        ui.ctx().copy_text(t.access_token.clone());
                    }
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    if let Some(e) = t.expires_at {
                        let left = e - chrono::Utc::now().timestamp();
                        let text = if left <= 0 {
                            tr("Expired").to_string()
                        } else {
                            trf("Expires in {}", &[&short_duration(left)])
                        };
                        kit::metric(ui, icons::CLOCK, &text);
                    }
                    if t.refresh_token.is_some() {
                        kit::metric(ui, icons::ARROWS_CLOCKWISE, tr("Renewed automatically"))
                            .on_hover_text(tr("refresh token available: renewed automatically when it expires"));
                    }
                });
                if let Some(s) = t.scope.as_ref().filter(|s| !s.trim().is_empty()) {
                    ui.add_space(2.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(widgets::muted(ui, tr("Scopes")).small());
                        for scope in s.split_whitespace() {
                            kit::pill(ui, scope, p.text_muted);
                        }
                    });
                }
            }
            None => {
                ui.label(kit::hint(
                    ui,
                    tr("No token stored. Tokens are kept in the Keychain, never in collection files."),
                ));
            }
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!ctx.fetching, |ui| {
                if kit::secondary(ui, Some(icons::KEY), tr("Get New Access Token")).clicked() {
                    *action = AuthAction::GetToken;
                }
            });
            if ctx.token.is_some()
                && kit::button_ex(ui, kit::Kind::Ghost, Some(icons::TRASH), tr("Clear token"), false).clicked()
            {
                *action = AuthAction::ClearToken;
            }
        });
        ui.add_space(4.0);
        ui.label(RichText::new(trf("Token key: {}", &[&ctx.owner_id])).small().color(p.text_faint));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::model::HttpRequest;
    use merpati_core::prepared::{PrepareContext, prepare};
    use merpati_core::variables::{Resolver, VariableScopes};

    fn spec(kind: AuthType, key: &str) -> &'static FieldSpec {
        merpati_auth::fields(kind).iter().find(|f| f.key == key).unwrap()
    }

    /// Imported auth often leaves parameters out; the editor must then show
    /// what is actually sent, not the default a new config would get.
    #[test]
    fn missing_params_show_what_is_sent() {
        let mut auth = Auth::new(AuthType::OAuth1);
        auth.set("consumerKey", "ck");
        auth.set("consumerSecret", "cs");
        let http = HttpRequest { auth: auth.clone(), ..HttpRequest::new("GET", "https://api.test/x") };
        let scopes = VariableScopes::default();
        let defaults = merpati_core::settings::RequestDefaults::default();
        let ctx = PrepareContext {
            resolver: Resolver::new(&scopes),
            auth: &auth,
            auth_owner_id: None,
            defaults: &defaults,
            base_dir: None,
        };
        let signed = merpati_auth::preview(&prepare(&http, &ctx)).unwrap().request;
        let in_header =
            signed.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.starts_with("OAuth"));
        assert_eq!(bool_value(&auth, spec(AuthType::OAuth1, "addParamsToHeader")), in_header);

        // Without a callback URL the redirect is received on a random
        // 127.0.0.1 port, not on http://localhost:5555/callback.
        let auth = Auth::new(AuthType::OAuth2);
        assert_eq!(value_text(&auth, spec(AuthType::OAuth2, "redirect_uri")), "");
    }
}
