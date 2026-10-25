//! Vault dialog: local secrets, names linked to external secret managers
//! (HashiCorp Vault, AWS Secrets Manager, Azure Key Vault, 1Password) and
//! the providers themselves.

use std::sync::Arc;

use egui::{Align2, CornerRadius, Key, Margin, RichText, Sense, Ui, pos2, vec2};
use parking_lot::Mutex;

use merpati_secrets::{
    AwsCredentialSource, AzureAuth, BrowseEntry, ExternalSecrets, KvVersion, LinkStatus, LinkedSecret, OnePasswordMode,
    ProviderConfig, ProviderKind, ProviderSettings, VaultAuth,
};

use crate::app::MerpatiApp;
use crate::dialogs::{
    Modal, Outcome, Tone, banner, body, compact_empty, fit_height, footer, hairline, input, line_galley, segmented,
    trailing,
};
use crate::i18n::{tr, trf};
use crate::{icons, kit, theme, widgets};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    Secrets,
    Providers,
}

/// Results of background calls, read by the dialog each frame.
#[derive(Default)]
struct Pending {
    busy: bool,
    message: Option<(String, bool)>,
    browse: Option<Vec<BrowseEntry>>,
}

#[derive(Default)]
struct LinkForm {
    name: String,
    provider: String,
    reference: String,
    ttl_secs: u64,
    browse_path: Vec<String>,
}

struct ProviderForm {
    config: ProviderConfig,
    /// Credential edits (empty = unchanged).
    credentials: Vec<(String, String)>,
    is_new: bool,
}

#[derive(Default)]
pub struct VaultDialog {
    section: Section,
    filter: String,
    new_name: String,
    new_value: String,
    link: Option<LinkForm>,
    provider: Option<ProviderForm>,
    pending: Arc<Mutex<Pending>>,
}

/// Runs `work` in the background; its result replaces `pending`.
fn run<F>(pending: &Arc<Mutex<Pending>>, app: &MerpatiApp, work: F)
where
    F: Future<Output = Pending> + Send + 'static,
{
    pending.lock().busy = true;
    let pending = pending.clone();
    let ctx = app.ctx.clone();
    app.rt.spawn(async move {
        let out = work.await;
        *pending.lock() = out;
        ctx.request_repaint();
    });
}

fn status_text(status: &LinkStatus) -> (String, bool) {
    match status {
        LinkStatus::NotLinked => (tr("local").into(), false),
        LinkStatus::NotFetched => (tr("not fetched yet").into(), false),
        LinkStatus::Fetching => (tr("fetching…").into(), false),
        LinkStatus::Cached { expires_in, .. } => (format!("{} · {}s", tr("cached"), expires_in.as_secs()), false),
        LinkStatus::Failed(e) => (e.message.clone(), true),
    }
}

fn kind_icon(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::HashicorpVault => icons::VAULT,
        ProviderKind::AwsSecretsManager => icons::DATABASE,
        ProviderKind::AzureKeyVault => icons::KEY,
        ProviderKind::OnePassword => icons::LOCK_KEY,
    }
}

/// A labelled row of a sub-form: muted label column, then the control.
fn form_row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(vec2(128.0, 30.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_width(128.0);
            ui.add(egui::Label::new(RichText::new(label).color(p.text_muted)).truncate());
        });
        add(ui);
    });
}

pub fn show(app: &mut MerpatiApp, ctx: &egui::Context, d: &mut VaultDialog) -> Outcome {
    // External providers are set up on first use.
    app.attach_external_secrets(true);
    let ext = app.ws.external.clone();
    let backend = app.ws.secrets.backend_description();
    let secret_count = app.ws.vault_names().len();
    let provider_count = ext.as_ref().map(|e| e.providers().len()).unwrap_or(0);
    let height = fit_height(ctx, 300.0, 380.0);
    Modal::new("vault", 820.0)
        .title(tr("Vault"))
        .subtitle(trf("Secrets are kept in {}, never in workspace files.", &[&backend]))
        .icon(icons::LOCK_KEY)
        .show(ctx, |ui| {
            body(ui, |ui| {
                let items = vec![
                    (Section::Secrets, format!("{}{}", tr("Secrets"), widgets::count_label(secret_count))),
                    (
                        Section::Providers,
                        format!("{}{}", tr("External providers"), widgets::count_label(provider_count)),
                    ),
                ];
                widgets::sub_tabs(ui, &mut d.section, &items);
                ui.add_space(12.0);
                egui::ScrollArea::vertical()
                    .id_salt(("vault-section", d.section == Section::Secrets))
                    .max_height(height)
                    .min_scrolled_height(height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| match (d.section, &ext) {
                        (Section::Secrets, _) => secrets(app, ui, d, ext.as_ref()),
                        (Section::Providers, Some(ext)) => providers(app, ui, d, ext),
                        (Section::Providers, None) => {
                            banner(ui, Tone::Err, tr("External secrets are unavailable for this workspace."));
                        }
                    });
                let (busy, message) = {
                    let p = d.pending.lock();
                    (p.busy, p.message.clone())
                };
                if busy || message.is_some() {
                    ui.add_space(10.0);
                }
                if busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(tr("Working…")).color(theme::pal(ui).text_muted));
                    });
                } else if let Some((m, err)) = message {
                    banner(ui, if err { Tone::Err } else { Tone::Ok }, &m);
                }
            });
            footer(ui, |ui| {
                let p = theme::pal(ui);
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(icons::INFO).font(theme::icon_font(14.0)).color(p.text_faint));
                ui.label(RichText::new(tr("Use a secret in any field as")).size(12.0).color(p.text_muted));
                kit::kbd(ui, "{{vault:name}}");
                trailing(ui, |ui| {
                    if kit::secondary(ui, None, tr("Close")).clicked() { Outcome::Close } else { Outcome::Keep }
                })
            })
        })
}

fn secrets(app: &mut MerpatiApp, ui: &mut Ui, d: &mut VaultDialog, ext: Option<&ExternalSecrets>) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        kit::search_field(ui, "vault-filter", &mut d.filter, tr("Filter secrets"), 260.0);
        trailing(ui, |ui| {
            if let Some(e) = ext
                && !e.providers().is_empty()
                && d.link.is_none()
                && kit::secondary(ui, Some(icons::LINK), tr("Link external secret…")).clicked()
            {
                d.link = Some(LinkForm { ttl_secs: 300, ..Default::default() });
            }
        });
    });
    ui.add_space(10.0);
    if let (Some(e), true) = (ext, d.link.is_some()) {
        link_form(app, ui, d, e);
        ui.add_space(12.0);
    }
    let names = app.ws.vault_names();
    let needle = d.filter.trim().to_lowercase();
    let shown: Vec<&String> = names.iter().filter(|n| n.to_lowercase().contains(&needle)).collect();
    let mut remove = None;
    let mut unlink = None;
    kit::card(ui).inner_margin(Margin::ZERO).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        if names.is_empty() {
            egui::Frame::new().inner_margin(Margin::symmetric(12, 20)).show(ui, |ui| {
                compact_empty(
                    ui,
                    icons::KEY,
                    tr("No secrets yet"),
                    tr("Add one below, then use it in any field without putting it in a file."),
                );
            });
        } else if shown.is_empty() {
            egui::Frame::new().inner_margin(Margin::symmetric(14, 14)).show(ui, |ui| {
                ui.label(RichText::new(tr("No secrets match the filter.")).color(p.text_muted));
            });
        }
        for (i, n) in shown.iter().enumerate() {
            if i > 0 {
                hairline(ui);
            }
            let linked = ext.and_then(|e| e.linked(n));
            egui::Frame::new().inner_margin(Margin { left: 14, right: 8, top: 5, bottom: 5 }).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_height(30.0);
                    ui.label(RichText::new(icons::KEY).font(theme::icon_font(15.0)).color(p.text_muted));
                    ui.label(RichText::new(format!("{{{{vault:{n}}}}}")).font(theme::mono(12.5)).color(p.text));
                    match (&linked, ext) {
                        (Some(l), Some(e)) => {
                            let provider = e.provider(&l.provider_id).map(|p| p.name).unwrap_or_default();
                            let (status, err) = status_text(&e.status(n));
                            kit::pill(ui, &format!("{} {provider}", icons::LINK_SIMPLE), p.accent_text)
                                .on_hover_text(&l.reference);
                            ui.label(RichText::new(status).size(12.0).color(if err { p.err } else { p.text_muted }));
                        }
                        _ => {
                            kit::pill(ui, tr("Local"), p.text_muted);
                        }
                    }
                    trailing(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if linked.is_some() {
                            if kit::icon_button(ui, icons::X_CIRCLE, tr("Unlink")).clicked() {
                                unlink = Some(n.to_string());
                            }
                        } else {
                            if kit::icon_button(ui, icons::TRASH, tr("Delete")).clicked() {
                                remove = Some(n.to_string());
                            }
                            if ext.is_some()
                                && kit::icon_button(ui, icons::LINK, tr("Link to an external secret")).clicked()
                            {
                                d.link = Some(LinkForm { name: n.to_string(), ttl_secs: 300, ..Default::default() });
                            }
                        }
                        if kit::icon_button(ui, icons::COPY, tr("Copy the reference")).clicked() {
                            ui.ctx().copy_text(format!("{{{{vault:{n}}}}}"));
                        }
                    });
                });
            });
        }
    });
    if let Some(n) = remove {
        app.ws.secrets.vault_remove(&n);
    }
    if let (Some(n), Some(e)) = (unlink, ext) {
        let _ = e.unlink(&n);
    }
    ui.add_space(16.0);
    ui.label(kit::section_title(tr("Add a secret")));
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        let r =
            ui.add(input(&mut d.new_name).hint_text(tr("Name")).font(egui::TextStyle::Monospace).desired_width(200.0));
        widgets::a11y_name(&r, tr("Name"));
        let w = ui.available_width() - 82.0;
        let v = ui.add(input(&mut d.new_value).hint_text(tr("Value")).password(true).desired_width(w));
        widgets::a11y_name(&v, tr("Value"));
        let enter = v.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let ok = !d.new_name.trim().is_empty();
        let add = ui.add_enabled_ui(ok, |ui| kit::secondary(ui, Some(icons::PLUS), tr("Add"))).inner.clicked();
        if (add || enter) && ok {
            app.ws.secrets.vault_set(d.new_name.trim(), &d.new_value);
            d.new_name.clear();
            d.new_value.clear();
        }
    });
}

fn link_form(app: &MerpatiApp, ui: &mut Ui, d: &mut VaultDialog, ext: &ExternalSecrets) {
    let providers = ext.providers();
    if providers.is_empty() {
        banner(ui, Tone::Info, tr("Add a provider under External providers first."));
        return;
    }
    let Some(form) = d.link.as_mut() else { return };
    if form.provider.is_empty() {
        form.provider = providers[0].id.clone();
    }
    let provider = providers.iter().find(|p| p.id == form.provider).cloned().unwrap_or_else(|| providers[0].clone());
    let mut save = false;
    let mut cancel = false;
    let mut browse: Option<String> = None;
    let mut preview = false;
    let p = theme::pal(ui);
    kit::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(icons::LINK).font(theme::icon_font(16.0)).color(p.accent_text));
            ui.label(
                RichText::new(tr("Link a vault name to an external secret")).font(theme::semibold(13.5)).color(p.text),
            );
            trailing(ui, |ui| {
                if kit::icon_button_ex(ui, icons::X, tr("Cancel"), false, 24.0).clicked() {
                    cancel = true;
                }
            });
        });
        ui.add_space(6.0);
        form_row(ui, tr("Name"), |ui| {
            ui.add(
                input(&mut form.name).hint_text("db_password").font(egui::TextStyle::Monospace).desired_width(260.0),
            );
        });
        form_row(ui, tr("Provider"), |ui| {
            egui::ComboBox::from_id_salt("vault-link-provider")
                .icon(crate::kit::combo_caret)
                .width(260.0)
                .selected_text(&provider.name)
                .show_ui(ui, |ui| {
                    for p in &providers {
                        let label = kit::with_icon(
                            kind_icon(p.settings.kind()),
                            &format!("{} ({})", p.name, p.settings.kind().label()),
                        );
                        if ui.selectable_label(p.id == form.provider, label).clicked() {
                            form.provider = p.id.clone();
                            form.browse_path.clear();
                        }
                    }
                });
        });
        form_row(ui, tr("Reference"), |ui| {
            let w = ui.available_width();
            ui.add(
                input(&mut form.reference)
                    .hint_text(provider.settings.reference_hint())
                    .desired_width(w)
                    .font(egui::TextStyle::Monospace),
            );
        });
        form_row(ui, tr("Cache"), |ui| {
            ui.spacing_mut().interact_size = vec2(96.0, 30.0);
            ui.add(egui::DragValue::new(&mut form.ttl_secs).range(0..=86_400).suffix(" s"));
            ui.label(kit::hint(ui, tr("0 fetches on every use")));
        });
        if let Err(e) = provider.settings.check_reference(&form.reference)
            && !form.reference.is_empty()
        {
            ui.add_space(4.0);
            banner(ui, Tone::Err, &e.message);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if kit::secondary(ui, Some(icons::FOLDER_OPEN), tr("Browse…")).clicked() {
                browse = Some(form.browse_path.last().cloned().unwrap_or_default());
            }
            if kit::secondary(ui, Some(icons::EYE), tr("Preview"))
                .on_hover_text(tr("Fetch once without showing the value"))
                .clicked()
            {
                preview = true;
            }
            trailing(ui, |ui| {
                let ok = !form.name.trim().is_empty() && !form.reference.trim().is_empty();
                if ui.add_enabled_ui(ok, |ui| kit::primary(ui, tr("Save link"))).inner.clicked() {
                    save = true;
                }
                if kit::secondary(ui, None, tr("Cancel")).clicked() {
                    cancel = true;
                }
            });
        });
        let entries = d.pending.lock().browse.clone();
        if let Some(entries) = entries {
            ui.add_space(10.0);
            egui::Frame::new()
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(4))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().id_salt("vault-browse").max_height(180.0).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        if !form.browse_path.is_empty() && browse_row(ui, icons::ARROW_UP, tr("Up"), false).clicked() {
                            form.browse_path.pop();
                            browse = Some(form.browse_path.last().cloned().unwrap_or_default());
                        }
                        if entries.is_empty() {
                            egui::Frame::new().inner_margin(Margin::same(8)).show(ui, |ui| {
                                ui.label(RichText::new(tr("Nothing here.")).color(p.text_muted));
                            });
                        }
                        for e in entries {
                            if let Some(path) = &e.path {
                                if browse_row(ui, icons::FOLDER_SIMPLE, &e.name, true).clicked() {
                                    form.browse_path.push(path.clone());
                                    browse = Some(path.clone());
                                }
                            } else {
                                let r = browse_row(ui, icons::KEY, &e.name, false);
                                if let Some(reference) = &e.reference {
                                    let use_rect =
                                        egui::Rect::from_min_max(pos2(r.rect.right() - 64.0, r.rect.top()), r.rect.max);
                                    if ui
                                        .put(use_rect.shrink2(vec2(4.0, 3.0)), |ui: &mut Ui| {
                                            kit::ghost(ui, None, tr("Use"))
                                        })
                                        .clicked()
                                        || r.double_clicked()
                                    {
                                        form.reference = reference.clone();
                                        if form.name.is_empty() {
                                            form.name = e.name.clone();
                                        }
                                    }
                                }
                            }
                        }
                    });
                });
        }
    });
    if let Some(path) = browse {
        let (ext, id) = (ext.clone(), provider.id.clone());
        run(&d.pending, app, async move {
            match ext.browse(&id, &path).await {
                Ok(list) => Pending { browse: Some(list), ..Default::default() },
                Err(e) => Pending { message: Some((e.message, true)), ..Default::default() },
            }
        });
    }
    if preview {
        let (ext, id, reference) = (ext.clone(), provider.id.clone(), form.reference.clone());
        let keep = d.pending.lock().browse.clone();
        run(&d.pending, app, async move {
            let message = match ext.resolve_reference(&id, &reference).await {
                Ok(v) => (format!("{} ({} {})", tr("Resolved"), v.chars().count(), tr("characters")), false),
                Err(e) => (e.message, true),
            };
            Pending { message: Some(message), browse: keep, ..Default::default() }
        });
    }
    if save {
        let link = LinkedSecret::new(form.name.trim(), &provider.id, form.reference.trim()).with_ttl(form.ttl_secs);
        match ext.link(link) {
            Ok(()) => {
                let (ext2, name) = (ext.clone(), form.name.trim().to_string());
                d.link = None;
                run(&d.pending, app, async move {
                    let errs = ext2.prefetch(std::slice::from_ref(&name)).await;
                    let message = match errs.into_iter().next() {
                        Some((_, e)) => (e.message, true),
                        None => (format!("{{{{vault:{name}}}}} {}", tr("is linked")), false),
                    };
                    Pending { message: Some(message), ..Default::default() }
                });
            }
            Err(e) => d.pending.lock().message = Some((e.message, true)),
        }
    } else if cancel {
        d.link = None;
        d.pending.lock().browse = None;
    }
}

/// A row of the provider browser: folders open, secrets can be used.
fn browse_row(ui: &mut Ui, icon: &str, label: &str, folder: bool) -> egui::Response {
    let p = theme::pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    widgets::a11y_button(&resp, label);
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), p.hover);
    }
    let cy = rect.center().y;
    ui.painter().text(pos2(rect.left() + 16.0, cy), Align2::CENTER_CENTER, icon, theme::icon_font(15.0), p.text_muted);
    let g = line_galley(ui, label, egui::FontId::proportional(13.0), rect.width() - 110.0);
    ui.painter().galley(pos2(rect.left() + 32.0, cy - g.size().y / 2.0), g, p.text);
    if folder {
        ui.painter().text(
            pos2(rect.right() - 14.0, cy),
            Align2::CENTER_CENTER,
            icons::CARET_RIGHT,
            theme::icon_font(13.0),
            p.text_faint,
        );
    }
    resp
}

fn providers(app: &mut MerpatiApp, ui: &mut Ui, d: &mut VaultDialog, ext: &ExternalSecrets) {
    let p = theme::pal(ui);
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(kit::hint(
                ui,
                tr("Providers keep their settings in .merpati/external-secrets.json; tokens and keys go to the secret store."),
            ))
            .wrap(),
        );
    });
    ui.add_space(8.0);
    let list = ext.providers();
    let mut remove = None;
    let show_list = !list.is_empty() || d.provider.is_none();
    if show_list {
        kit::card(ui).inner_margin(Margin::ZERO).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            if list.is_empty() && d.provider.is_none() {
                egui::Frame::new().inner_margin(Margin::symmetric(12, 20)).show(ui, |ui| {
                    compact_empty(
                        ui,
                        icons::VAULT,
                        tr("No external providers"),
                        tr("Connect HashiCorp Vault, AWS Secrets Manager, Azure Key Vault or 1Password."),
                    );
                });
            }
            for (i, pr) in list.iter().enumerate() {
                if i > 0 {
                    hairline(ui);
                }
                egui::Frame::new().inner_margin(Margin { left: 12, right: 8, top: 8, bottom: 8 }).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (tile, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                        ui.painter().rect_filled(tile, CornerRadius::same(8), p.accent_soft);
                        ui.painter().text(
                            tile.center(),
                            Align2::CENTER_CENTER,
                            kind_icon(pr.settings.kind()),
                            theme::icon_font(16.0),
                            p.accent_text,
                        );
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new(&pr.name).font(theme::medium(13.5)).color(p.text));
                            ui.label(RichText::new(pr.settings.kind().label()).size(12.0).color(p.text_muted));
                        });
                        trailing(ui, |ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if kit::icon_button(ui, icons::TRASH, tr("Remove")).clicked() {
                                remove = Some(pr.id.clone());
                            }
                            if kit::icon_button(ui, icons::PENCIL_SIMPLE, tr("Edit")).clicked() {
                                d.provider =
                                    Some(ProviderForm { config: pr.clone(), credentials: Vec::new(), is_new: false });
                            }
                        });
                    });
                });
            }
        });
        ui.add_space(10.0);
    }
    if let Some(id) = remove {
        match ext.remove_provider(&id) {
            Ok(unlinked) if !unlinked.is_empty() => {
                d.pending.lock().message = Some((format!("{}: {}", tr("Unlinked"), unlinked.join(", ")), false))
            }
            Ok(_) => {}
            Err(e) => d.pending.lock().message = Some((e.message, true)),
        }
    }
    if d.provider.is_none() {
        let add = kit::secondary(ui, Some(icons::PLUS), &format!("{}  {}", tr("Add provider"), icons::CARET_DOWN));
        egui::Popup::menu(&add).show(|ui| {
            for kind in ProviderKind::ALL {
                if ui.button(kit::with_icon(kind_icon(kind), kind.label())).clicked() {
                    d.provider = Some(ProviderForm {
                        config: ProviderConfig::new(kind.label(), kind.default_settings()),
                        credentials: Vec::new(),
                        is_new: true,
                    });
                    ui.close();
                }
            }
        });
    } else {
        provider_form(app, ui, d, ext);
    }
}

fn text_row(ui: &mut Ui, label: &str, value: &mut String, hint: &str) {
    form_row(ui, label, |ui| {
        let w = ui.available_width();
        ui.add(input(value).hint_text(hint).desired_width(w));
    });
}

fn provider_form(app: &MerpatiApp, ui: &mut Ui, d: &mut VaultDialog, ext: &ExternalSecrets) {
    let Some(form) = d.provider.as_mut() else { return };
    let mut save = false;
    let mut test = false;
    let mut cancel = false;
    let p = theme::pal(ui);
    kit::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let kind = form.config.settings.kind();
        ui.horizontal(|ui| {
            ui.label(RichText::new(kind_icon(kind)).font(theme::icon_font(16.0)).color(p.accent_text));
            let title = if form.is_new { trf("New {} provider", &[&kind.label()]) } else { form.config.name.clone() };
            ui.label(RichText::new(title).font(theme::semibold(13.5)).color(p.text));
            trailing(ui, |ui| {
                if kit::icon_button_ex(ui, icons::X, tr("Cancel"), false, 24.0).clicked() {
                    cancel = true;
                }
            });
        });
        ui.add_space(6.0);
        text_row(ui, tr("Name"), &mut form.config.name, "");
        match &mut form.config.settings {
            ProviderSettings::HashicorpVault(s) => {
                text_row(ui, tr("Address"), &mut s.address, "https://vault.example.com:8200");
                text_row(ui, tr("Namespace"), &mut s.namespace, tr("optional (Vault Enterprise)"));
                form_row(ui, tr("Authentication"), |ui| {
                    segmented(
                        ui,
                        &mut s.auth,
                        &[(VaultAuth::Token, "Token".into()), (VaultAuth::AppRole, "AppRole".into())],
                    );
                });
                if s.auth == VaultAuth::AppRole {
                    text_row(ui, "Role ID", &mut s.role_id, "");
                    text_row(ui, tr("AppRole mount"), &mut s.approle_mount, "approle");
                }
                text_row(ui, tr("KV mount"), &mut s.mount, tr("optional: first path segment"));
                form_row(ui, tr("KV version"), |ui| {
                    segmented(
                        ui,
                        &mut s.kv_version,
                        &[
                            (KvVersion::Auto, tr("Auto").into()),
                            (KvVersion::V1, "v1".into()),
                            (KvVersion::V2, "v2".into()),
                        ],
                    );
                });
            }
            ProviderSettings::AwsSecretsManager(s) => {
                text_row(ui, tr("Region"), &mut s.region, "eu-west-1");
                form_row(ui, tr("Credentials"), |ui| {
                    segmented(
                        ui,
                        &mut s.credentials,
                        &[
                            (AwsCredentialSource::DefaultChain, tr("Default chain (env, ~/.aws)").into()),
                            (AwsCredentialSource::Static, tr("Access keys").into()),
                        ],
                    );
                });
                if s.credentials == AwsCredentialSource::DefaultChain {
                    text_row(ui, tr("Profile"), &mut s.profile, "default");
                }
                text_row(ui, tr("Endpoint"), &mut s.endpoint, tr("optional (e.g. LocalStack)"));
            }
            ProviderSettings::AzureKeyVault(s) => {
                text_row(ui, tr("Vault URL"), &mut s.vault_url, "https://my-vault.vault.azure.net");
                form_row(ui, tr("Authentication"), |ui| {
                    segmented(
                        ui,
                        &mut s.auth,
                        &[
                            (AzureAuth::ClientSecret, tr("Service principal").into()),
                            (AzureAuth::AzureCli, "Azure CLI".into()),
                        ],
                    );
                });
                if s.auth == AzureAuth::ClientSecret {
                    text_row(ui, tr("Tenant ID"), &mut s.tenant_id, "");
                    text_row(ui, tr("Client ID"), &mut s.client_id, "");
                    text_row(ui, tr("Authority host"), &mut s.authority_host, "https://login.microsoftonline.com");
                } else {
                    text_row(ui, tr("az path"), &mut s.cli_path, tr("optional"));
                }
                text_row(ui, tr("API version"), &mut s.api_version, "7.5");
            }
            ProviderSettings::OnePassword(s) => {
                form_row(ui, tr("Mode"), |ui| {
                    segmented(
                        ui,
                        &mut s.mode,
                        &[
                            (OnePasswordMode::Cli, tr("op CLI").into()),
                            (OnePasswordMode::Connect, tr("Connect server").into()),
                        ],
                    );
                });
                match s.mode {
                    OnePasswordMode::Cli => {
                        text_row(ui, tr("Account"), &mut s.account, tr("optional: sign-in address or email"));
                        text_row(ui, tr("op path"), &mut s.cli_path, tr("optional"));
                    }
                    OnePasswordMode::Connect => {
                        text_row(ui, tr("Connect URL"), &mut s.connect_url, "http://localhost:8080")
                    }
                }
            }
        }
        for f in form.config.settings.credential_fields() {
            let set = ext.has_credential(&form.config.id, f.key);
            let idx = match form.credentials.iter().position(|(k, _)| k == f.key) {
                Some(i) => i,
                None => {
                    form.credentials.push((f.key.to_string(), String::new()));
                    form.credentials.len() - 1
                }
            };
            let label = format!("{}{}", tr(f.label), if f.required { " *" } else { "" });
            form_row(ui, &label, |ui| {
                let hint = if set { tr("set — type to replace") } else { tr("not set") };
                let w = ui.available_width();
                let r = ui.add(input(&mut form.credentials[idx].1).password(true).hint_text(hint).desired_width(w));
                widgets::a11y_name(&r, tr(f.label));
            });
        }
        if let Err(e) = form.config.settings.validate() {
            ui.add_space(4.0);
            banner(ui, Tone::Err, &e.message);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if kit::secondary(ui, Some(icons::PLUGS_CONNECTED), tr("Test connection")).clicked() {
                save = true;
                test = true;
            }
            trailing(ui, |ui| {
                if kit::primary(ui, tr("Save")).clicked() {
                    save = true;
                }
                if kit::secondary(ui, None, tr("Cancel")).clicked() {
                    cancel = true;
                }
            });
        });
    });
    if cancel {
        d.provider = None;
        return;
    }
    if !save {
        return;
    }
    let config = form.config.clone();
    if let Err(e) = ext.upsert_provider(config.clone()) {
        d.pending.lock().message = Some((e.message, true));
        return;
    }
    for (k, v) in form.credentials.drain(..).filter(|(_, v)| !v.is_empty()) {
        ext.set_credential(&config.id, &k, &v);
    }
    form.is_new = false;
    if test {
        let ext = ext.clone();
        run(&d.pending, app, async move {
            let message = match ext.test_provider(&config).await {
                Ok(summary) => (summary, false),
                Err(e) => (e.message, true),
            };
            Pending { message: Some(message), ..Default::default() }
        });
    } else {
        d.provider = None;
        d.pending.lock().message = Some((tr("Saved").into(), false));
    }
}
