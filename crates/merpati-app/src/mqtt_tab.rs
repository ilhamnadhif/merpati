//! MQTT request tab (FR-X5): connect to a broker (3.1.1 or 5), manage
//! subscriptions, publish and watch the message log.

use std::time::Instant;

use egui::{RichText, Ui};

use merpati_core::model::RequestKind;
use merpati_core::protocols::{MqttMessage, MqttSubscription, MqttVersion};
use merpati_mqtt::{Direction, MessageProperties, MqttConfig, MqttEvent, MqttSession, Publish};

use crate::app::{MerpatiApp, Msg};
use crate::highlight::Lang;
use crate::i18n::{tr, trf};
use crate::proto_common::{
    self as pc, Conn, ConnAction, Dir, ItemSlot, Log, LogLine, RowAction, ScriptState, basic_credentials, header,
    payload_text, resolver,
};
use crate::{icons, kit, theme, widgets};
use merpati_core::script::ScriptEvent;
use merpati_core::script::protocol as proto;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Publish,
    Subscriptions,
    Saved,
    Connection,
    Scripts,
}

pub struct MqttTab {
    /// Number of the current connection (see `app::next_epoch`).
    pub epoch: u64,
    pub slot: ItemSlot,
    session: Option<MqttSession>,
    connected: bool,
    connecting: bool,
    section: Section,
    topic: String,
    payload: String,
    qos: u8,
    retain: bool,
    /// MQTT 5 user properties of the next publish.
    user_props: Vec<merpati_core::model::KeyValue>,
    log: Log,
    pub scripts: ScriptState,
    /// Incoming messages waiting for the on-message script.
    inbox: std::collections::VecDeque<serde_json::Value>,
    since: Option<Instant>,
    /// Why the last connection failed.
    error: Option<String>,
}

impl MqttTab {
    pub fn new(slot: ItemSlot) -> Self {
        MqttTab {
            epoch: 0,
            slot,
            session: None,
            connected: false,
            connecting: false,
            section: Section::Publish,
            topic: String::new(),
            payload: String::new(),
            qos: 0,
            retain: false,
            user_props: Vec::new(),
            log: Log::with_empty(
                "No messages yet",
                "Connect to the broker; messages on your subscriptions appear here.",
            ),
            scripts: ScriptState::default(),
            inbox: Default::default(),
            since: None,
            error: None,
        }
    }

    fn state(&self) -> Conn {
        if self.connected {
            Conn::Live
        } else if self.connecting {
            Conn::Busy
        } else if self.error.is_some() {
            Conn::Failed
        } else {
            Conn::Idle
        }
    }

    /// An event with its MQTT 5 properties (user properties of messages).
    pub fn on_event(&mut self, e: MqttEvent, props: Option<MessageProperties>) {
        let user_props = props.map(|p| p.user_properties).unwrap_or_default();
        match e {
            MqttEvent::Connecting => {
                self.connecting = true;
                self.error = None;
                self.log.system(tr("Connecting…"));
            }
            MqttEvent::Connected { session_present } => {
                self.connecting = false;
                self.connected = true;
                self.since = Some(Instant::now());
                self.error = None;
                self.log.system(trf("Connected{}", &[&if session_present { tr(" (session present)") } else { "" }]));
            }
            MqttEvent::Subscribed { topic, granted_qos } => {
                let qos = granted_qos.map(|q| format!(" (QoS {q})")).unwrap_or_default();
                self.log.system(trf("Subscribed to {}{}", &[&topic, &qos]))
            }
            MqttEvent::Unsubscribed { topic } => self.log.system(trf("Unsubscribed from {}", &[&topic])),
            MqttEvent::Message { topic, payload, qos, retain, direction, at_ms } => {
                let dir = if direction == Direction::Incoming { Dir::In } else { Dir::Out };
                let mut meta = Vec::new();
                if qos > 0 {
                    meta.push(format!("QoS {qos}"));
                }
                if retain {
                    meta.push(tr("retained").to_string());
                }
                if !user_props.is_empty() {
                    meta.push(trf("{} props", &[&user_props.len()]));
                }
                let size = payload.len();
                let mut text = payload_text(&payload);
                if direction == Direction::Incoming {
                    let mut m = proto::message(proto::json_or_text(&text), proto::Direction::Incoming, at_ms);
                    m["topic"] = serde_json::json!(topic);
                    m["qos"] = serde_json::json!(qos);
                    m["retain"] = serde_json::json!(retain);
                    m["userProperties"] = user_props
                        .iter()
                        .map(|(k, v)| serde_json::json!({"key": k, "value": v}))
                        .collect::<Vec<_>>()
                        .into();
                    crate::proto_common::queue_message(&mut self.inbox, m);
                }
                if !user_props.is_empty() {
                    text.push_str(&format!("\n\n{}", tr("User properties:")));
                    for (k, v) in &user_props {
                        text.push_str(&format!("\n  {k}: {v}"));
                    }
                }
                self.log.push_line(LogLine {
                    dir,
                    title: topic,
                    text,
                    at_ms,
                    size,
                    error: false,
                    binary: std::str::from_utf8(&payload).is_err(),
                    meta: meta.join(" · "),
                });
            }
            MqttEvent::Error(m) => {
                if !self.connected {
                    self.error = Some(m.clone());
                }
                self.log.error(m)
            }
            MqttEvent::Disconnected { reason } => {
                if self.connecting && !reason.is_empty() && self.error.is_none() {
                    self.error = Some(reason.clone());
                }
                self.connected = false;
                self.connecting = false;
                self.session = None;
                self.since = None;
                self.log.system(if reason.is_empty() {
                    tr("Disconnected").to_string()
                } else {
                    trf("Disconnected: {}", &[&reason])
                });
            }
        }
    }
}

/// Enabled, named user properties with variables resolved.
fn publish_properties(
    rows: &[merpati_core::model::KeyValue],
    resolve: &dyn Fn(&str) -> String,
) -> Vec<(String, String)> {
    rows.iter()
        .filter(|p| p.enabled && !p.key.trim().is_empty())
        .map(|p| (resolve(&p.key), resolve(&p.value)))
        .collect()
}

/// What to publish: topic, payload, QoS, retain and user properties.
type Outgoing = (String, String, u8, bool, Vec<merpati_core::model::KeyValue>);

fn publish(app: &MerpatiApp, tab: &MqttTab, collection: Option<&str>, out: Outgoing) {
    let (topic, payload, qos, retain, props) = out;
    let r = resolver(app, collection);
    let Some(s) = &tab.session else { return };
    let publish = Publish { topic: r(&topic), payload: r(&payload).into_bytes(), qos, retain };
    let user_properties = publish_properties(&props, &r);
    if user_properties.is_empty() {
        s.publish(publish);
    } else {
        s.publish_with_properties(publish, MessageProperties { user_properties });
    }
}

const QOS: [(u8, &str); 3] = [(0, "0"), (1, "1"), (2, "2")];

pub fn show(app: &mut MerpatiApp, ui: &mut Ui, tab_id: u64, tab: &mut MqttTab) {
    let Some(original) = tab.slot.load(app) else {
        ui.label(tr("This request no longer exists."));
        return;
    };
    let mut item = original.clone();
    let collection = tab.slot.collection_id();
    let vars = app.var_ctx(collection.as_deref());
    let state = tab.state();
    let since = tab.since;
    let v5 = matches!(&item.request, RequestKind::Mqtt(r) if r.version == MqttVersion::V5);
    header(ui, app, tab_id, &tab.slot, &mut item, |ui| {
        pc::conn_status(ui, state, since);
        let p = theme::pal(ui);
        kit::pill(ui, if v5 { "MQTT 5" } else { "MQTT 3.1.1" }, p.text_muted);
    });
    let RequestKind::Mqtt(req) = &mut item.request else { return };
    let mut action = None;
    let clicked = pc::connect_bar(
        ui,
        "MQTT",
        126.0,
        |ui| {
            let r =
                widgets::var_cell(ui, "mqtt-url", &mut req.url, &vars, "mqtt://localhost:1883", ui.available_width());
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && state != Conn::Live {
                action = Some(ConnAction::Connect);
            }
            r.has_focus()
        },
        |ui| pc::connect_button(ui, state),
    );
    if clicked.is_some() {
        action = clicked;
    }
    if let (Some(e), Conn::Failed) = (&tab.error, state) {
        ui.add_space(8.0);
        let mut retry = false;
        let mut dismiss = false;
        pc::error_banner(ui, tr("Could not connect to the broker"), e, |ui| {
            dismiss = kit::icon_button(ui, icons::X, tr("Dismiss")).clicked();
            retry = kit::secondary(ui, Some(icons::ARROW_CLOCKWISE), tr("Retry")).clicked();
        });
        if retry {
            action = Some(ConnAction::Connect);
        }
        if dismiss {
            tab.error = None;
        }
    }
    let connected = state == Conn::Live;
    let mut outgoing: Option<Outgoing> = None;
    let can_publish = connected && !tab.topic.trim().is_empty();
    if can_publish && pc::send_shortcut(ui) {
        outgoing = Some((tab.topic.clone(), tab.payload.clone(), tab.qos, tab.retain, tab.user_props.clone()));
    }
    let active_subs = req.subscriptions.iter().filter(|s| s.enabled).count();
    let items = vec![
        (Section::Publish, tr("Publish").to_string()),
        (Section::Subscriptions, trf("Subscriptions{}", &[&widgets::count_label(active_subs)])),
        (Section::Saved, format!("{}{}", tr("Saved messages"), widgets::count_label(req.messages.len()))),
        (Section::Connection, tr("Connection").to_string()),
        (Section::Scripts, pc::scripts_label(&item.scripts)),
    ];
    ui.add_space(8.0);
    pc::section_tabs(ui, "mqtt-tabs", &mut tab.section, &items);
    ui.add_space(8.0);
    let mut sub_action: Option<(bool, String, u8)> = None;
    {
        let log = &mut tab.log;
        let t = &mut tab.topic;
        let payload = &mut tab.payload;
        let qos = &mut tab.qos;
        let retain = &mut tab.retain;
        let user_props = &mut tab.user_props;
        let section = &mut tab.section;
        let scripts_state = &mut tab.scripts;
        let scripts = &mut item.scripts;
        pc::split_view(
            ui,
            ("mqtt-split", tab_id),
            0.5,
            |ui| match *section {
                Section::Publish => {
                    let p = theme::pal(ui);
                    ui.horizontal(|ui| {
                        ui.set_height(30.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if pc::send_button(
                                ui,
                                icons::PAPER_PLANE_RIGHT,
                                tr("Publish"),
                                connected && !t.trim().is_empty(),
                                if connected { tr("Enter a topic first") } else { tr("Connect first") },
                            ) {
                                outgoing = Some((t.clone(), payload.clone(), *qos, *retain, user_props.clone()));
                            }
                            if pc::save_message_button(ui, !t.trim().is_empty()) {
                                req.messages.push(MqttMessage {
                                    name: t.clone(),
                                    topic: t.clone(),
                                    payload: payload.clone(),
                                    qos: *qos,
                                    retain: *retain,
                                    user_properties: user_props.clone(),
                                    ..Default::default()
                                });
                            }
                            kit::vdivider(ui, 20.0);
                            ui.checkbox(retain, tr("Retain")).on_hover_text(tr(
                                "The broker keeps the last retained message of a topic for new subscribers",
                            ));
                            ui.push_id("pub-qos", |ui| kit::segmented(ui, qos, &QOS));
                            ui.label(RichText::new("QoS").color(p.text_muted));
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.label(RichText::new(tr("Topic")).color(p.text_muted));
                                widgets::var_line(
                                    ui,
                                    "mqtt-topic",
                                    t,
                                    &vars,
                                    "sensors/kitchen/temp",
                                    ui.available_width(),
                                );
                            });
                        });
                    });
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().id_salt("mqtt-publish").show(ui, |ui| {
                        let rows = if req.version == MqttVersion::V5 {
                            pc::fill_rows(ui, 4).saturating_sub(7).max(4)
                        } else {
                            pc::fill_rows(ui, 4)
                        };
                        let r = widgets::code_editor(ui, "mqtt-payload", payload, Lang::Json, Some(&vars), rows, false);
                        pc::editor_placeholder(ui, &r, payload, tr("Payload: JSON or text, with {{variables}}"));
                        if req.version == MqttVersion::V5 {
                            ui.add_space(10.0);
                            let opts = widgets::KvOptions {
                                title: Some(tr("User properties")),
                                key_suggestions: None,
                                show_description: false,
                                key_hint: tr("Name"),
                                value_hint: tr("Value"),
                                readonly_keys: false,
                                allow_add: true,
                            };
                            widgets::kv_table(ui, "mqtt-user-props", user_props, &vars, opts);
                        }
                    });
                }
                Section::Subscriptions => {
                    egui::ScrollArea::vertical().id_salt("mqtt-subs").show(ui, |ui| {
                        pc::section(
                            ui,
                            tr("Subscriptions"),
                            Some(tr("Enabled subscriptions are made automatically after connecting. Wildcards + and # are allowed.")),
                        );
                        ui.add_space(4.0);
                        if req.subscriptions.is_empty() {
                            ui.label(kit::hint(ui, tr("No subscriptions yet.")));
                        } else {
                            let p = theme::pal(ui);
                            let mut remove = None;
                            egui::Frame::new()
                                .stroke(egui::Stroke::new(1.0, p.border))
                                .corner_radius(egui::CornerRadius::same(8))
                                .inner_margin(egui::Margin::symmetric(8, 2))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    for (i, s) in req.subscriptions.iter_mut().enumerate() {
                                        if i > 0 {
                                            let y = ui.cursor().top();
                                            ui.painter().hline(
                                                ui.max_rect().expand2(egui::vec2(8.0, 0.0)).x_range(),
                                                y,
                                                egui::Stroke::new(1.0, p.border),
                                            );
                                        }
                                        ui.push_id(i, |ui| {
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(ui.available_width(), 36.0),
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.spacing_mut().item_spacing.x = 6.0;
                                                    if kit::icon_button(ui, icons::TRASH, tr("Remove subscription")).clicked() {
                                                        remove = Some(i);
                                                    }
                                                    if connected {
                                                        if kit::ghost(ui, Some(icons::MINUS), tr("Unsubscribe")).clicked() {
                                                            sub_action = Some((false, s.topic.clone(), s.qos));
                                                        }
                                                        if kit::ghost(ui, Some(icons::PLUS), tr("Subscribe")).clicked() {
                                                            sub_action = Some((true, s.topic.clone(), s.qos));
                                                        }
                                                    }
                                                    ui.push_id("qos", |ui| kit::segmented(ui, &mut s.qos, &QOS));
                                                    ui.label(RichText::new("QoS").color(p.text_muted));
                                                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                                        ui.checkbox(&mut s.enabled, "")
                                                            .on_hover_text(tr("Subscribe after connecting"));
                                                        ui.add(
                                                            egui::TextEdit::singleline(&mut s.topic)
                                                                .desired_width(ui.available_width())
                                                                .hint_text("sensors/#")
                                                                .font(egui::TextStyle::Monospace),
                                                        );
                                                    });
                                                },
                                            );
                                        });
                                    }
                                });
                            if let Some(i) = remove {
                                req.subscriptions.remove(i);
                            }
                        }
                        ui.add_space(6.0);
                        if kit::ghost(ui, Some(icons::PLUS), tr("Add subscription")).clicked() {
                            req.subscriptions.push(MqttSubscription { topic: "#".into(), qos: 0, enabled: true });
                        }
                    });
                }
                Section::Saved => {
                    egui::ScrollArea::vertical().id_salt("mqtt-saved").show(ui, |ui| {
                        if req.messages.is_empty() {
                            kit::empty_state(
                                ui,
                                icons::BOOKMARK_SIMPLE,
                                tr("No saved messages"),
                                tr("Save messages from the Publish section to reuse them."),
                                |_| {},
                            );
                        } else {
                            let p = theme::pal(ui);
                            let acted = pc::saved_list(
                                ui,
                                "mqtt-saved-list",
                                &mut req.messages,
                                connected,
                                tr("Publish now"),
                                |ui, m| {
                                    pc::name_field(ui, &mut m.name, 140.0);
                                    kit::pill(ui, &crate::dialogs::ellipsize(&m.topic, 40), p.text_muted);
                                    if m.qos > 0 {
                                        ui.label(RichText::new(format!("QoS {}", m.qos)).small().color(p.text_muted));
                                    }
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(m.payload.replace('\n', " ")).monospace().color(p.text_muted),
                                        )
                                        .truncate(),
                                    );
                                },
                            );
                            match acted {
                                Some((i, RowAction::Use)) => {
                                    let m = req.messages[i].clone();
                                    *t = m.topic;
                                    *payload = m.payload;
                                    *qos = m.qos;
                                    *retain = m.retain;
                                    *user_props = m.user_properties;
                                    *section = Section::Publish;
                                }
                                Some((i, RowAction::Send)) => {
                                    let m = &req.messages[i];
                                    outgoing = Some((
                                        m.topic.clone(),
                                        m.payload.clone(),
                                        m.qos,
                                        m.retain,
                                        m.user_properties.clone(),
                                    ));
                                }
                                Some((i, RowAction::Remove)) => {
                                    req.messages.remove(i);
                                }
                                None => {}
                            }
                        }
                        ui.add_space(12.0);
                        crate::ws_tab::runner_section(ui, "mqtt", &mut req.runner, tr("topic filter, e.g. status/+"));
                    });
                }
                Section::Scripts => crate::proto_common::scripts_ui(
                    ui,
                    scripts,
                    &[(ScriptEvent::PreRequest, "Before connect"), (ScriptEvent::OnMessage, "On message")],
                    scripts_state,
                    &vars,
                ),
                Section::Connection => {
                    egui::ScrollArea::vertical().id_salt("mqtt-conn").show(ui, |ui| {
                        pc::form_grid("mqtt-conn").show(ui, |ui| {
                            pc::form_label(ui, tr("Version"));
                            ui.push_id("version", |ui| {
                                kit::segmented(
                                    ui,
                                    &mut req.version,
                                    &[(MqttVersion::V311, "3.1.1"), (MqttVersion::V5, "5.0")],
                                )
                            });
                            ui.end_row();
                            pc::form_label(ui, tr("Client ID"));
                            ui.add(
                                egui::TextEdit::singleline(&mut req.client_id)
                                    .hint_text(tr("generated when empty"))
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(260.0),
                            );
                            ui.end_row();
                            pc::form_label(ui, tr("Keep alive (s)"));
                            crate::widgets::a11y_name(
                                &ui.add(egui::DragValue::new(&mut req.keep_alive_secs).range(0..=65535)),
                                tr("Keep alive (s)"),
                            );
                            ui.end_row();
                            pc::form_label(ui, tr("Clean session"));
                            ui.checkbox(&mut req.clean_session, tr("Start without stored subscriptions"));
                            ui.end_row();
                            pc::form_label(ui, tr("Verify SSL certificates"));
                            let mut verify = req.verify_ssl.unwrap_or(true);
                            if ui.checkbox(&mut verify, tr("For mqtts:// and wss:// brokers")).changed() {
                                req.verify_ssl = Some(verify);
                            }
                            ui.end_row();
                        });
                        ui.add_space(14.0);
                        pc::section(ui, tr("Credentials"), None);
                        basic_credentials(ui, &mut req.auth, &vars);
                        ui.add_space(14.0);
                        pc::section(
                            ui,
                            tr("Last will"),
                            Some(tr("Published by the broker when this client disconnects unexpectedly.")),
                        );
                        let mut has_will = req.last_will.is_some();
                        if ui.checkbox(&mut has_will, tr("Send a last will")).changed() {
                            req.last_will = if has_will { Some(MqttMessage::default()) } else { None };
                        }
                        if let Some(w) = &mut req.last_will {
                            ui.add_space(4.0);
                            pc::form_grid("mqtt-will").show(ui, |ui| {
                                pc::form_label(ui, tr("Topic"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut w.topic)
                                        .hint_text("clients/merpati/status")
                                        .font(egui::TextStyle::Monospace)
                                        .desired_width(260.0),
                                );
                                ui.end_row();
                                pc::form_label(ui, tr("Payload"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut w.payload)
                                        .hint_text("offline")
                                        .font(egui::TextStyle::Monospace)
                                        .desired_width(260.0),
                                );
                                ui.end_row();
                                pc::form_label(ui, "QoS");
                                ui.horizontal(|ui| {
                                    ui.push_id("will-qos", |ui| kit::segmented(ui, &mut w.qos, &QOS));
                                    ui.checkbox(&mut w.retain, tr("Retain"));
                                });
                                ui.end_row();
                            });
                        }
                    });
                }
            },
            |ui| log.ui(ui, ("mqtt-log", tab_id)),
        );
    }
    if let Some(out) = outgoing {
        publish(app, tab, collection.as_deref(), out);
    }
    if let (Some((subscribe, topic, qos)), Some(sess)) = (sub_action, &tab.session) {
        // Topics may hold variables, as when connecting.
        let topic = resolver(app, collection.as_deref())(&topic);
        if subscribe {
            sess.subscribe(&topic, qos);
        } else {
            sess.unsubscribe(&topic);
        }
    }
    let req_clone = req.clone();
    let item_now = item.clone();
    tab.slot.store(app, &original, item);
    match action {
        Some(ConnAction::Connect) => {
            tab.scripts.clear_results();
            if crate::proto_common::run_scripts(
                app,
                tab_id,
                &tab.slot,
                &item_now,
                ScriptEvent::PreRequest,
                proto::mqtt(&req_clone),
            ) {
                tab.scripts.waiting = true;
            } else {
                start(app, tab_id, tab, &req_clone, collection.as_deref());
            }
        }
        Some(ConnAction::Cancel) | Some(ConnAction::Disconnect) => {
            if let Some(s) = &tab.session {
                s.disconnect();
            }
            if action == Some(ConnAction::Cancel) {
                tab.session = None;
                tab.connecting = false;
            }
        }
        None => {}
    }
    if let Some(pre) = tab.scripts.take_pre() {
        match pre {
            Some(v) => {
                let mut r = req_clone.clone();
                proto::apply_mqtt(&mut r, &v);
                start(app, tab_id, tab, &r, collection.as_deref());
            }
            None => tab.log.system(tr("Skipped by pm.execution.skipRequest()")),
        }
    }
    for m in std::mem::take(&mut tab.inbox) {
        if !crate::proto_common::run_scripts(
            app,
            tab_id,
            &tab.slot,
            &item_now,
            ScriptEvent::OnMessage,
            proto::mqtt(&req_clone).with_message(m),
        ) {
            break;
        }
    }
}

fn start(
    app: &mut MerpatiApp,
    tab_id: u64,
    tab: &mut MqttTab,
    req_clone: &merpati_core::protocols::MqttRequest,
    collection: Option<&str>,
) {
    {
        let r = resolver(app, collection);
        let mut cfg = MqttConfig::from_request(req_clone, &r, app.settings.request.verify_ssl);
        cfg.proxy = app.proxy_settings();
        match app.tls_setup(&cfg.url, cfg.verify_ssl) {
            Ok(tls) => cfg.tls = tls,
            Err(e) => return app.toast(e, true),
        }
        let tx = app.tx.clone();
        let ctx = app.ctx.clone();
        tab.epoch = crate::app::next_epoch();
        let epoch = tab.epoch;
        let session = MqttSession::connect_with_properties(app.rt.handle(), cfg, move |event, props| {
            let _ = tx.send(Msg::Mqtt { tab: tab_id, epoch, event, props });
            ctx.request_repaint();
        });
        for s in req_clone.subscriptions.iter().filter(|s| s.enabled && !s.topic.trim().is_empty()) {
            session.subscribe(&r(&s.topic), s.qos);
        }
        tab.session = Some(session);
        tab.connecting = true;
        tab.error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::model::KeyValue;

    #[test]
    fn mqtt5_user_properties_are_shown_and_given_to_scripts() {
        let mut tab = MqttTab::new(ItemSlot::default());
        let props =
            MessageProperties { user_properties: vec![("trace".into(), "abc".into()), ("k".into(), "v".into())] };
        let message = MqttEvent::Message {
            topic: "t/1".into(),
            payload: b"{\"n\":1}".to_vec(),
            qos: 1,
            retain: false,
            direction: Direction::Incoming,
            at_ms: 0,
        };
        tab.on_event(message, Some(props));
        let line = tab.log.lines.back().unwrap();
        assert!(line.text.contains("trace: abc") && line.text.contains("k: v"), "{}", line.text);
        assert_eq!(line.title, "t/1");
        assert!(line.meta.contains("QoS 1"), "{}", line.meta);
        let m = tab.inbox.back().unwrap();
        assert_eq!(m["userProperties"][0], serde_json::json!({"key": "trace", "value": "abc"}));

        let rows = vec![KeyValue::new("a", "{{x}}"), KeyValue::new("", "skip"), KeyValue::new("off", "1").disabled()];
        let resolve = |s: &str| s.replace("{{x}}", "42");
        assert_eq!(publish_properties(&rows, &resolve), vec![("a".to_string(), "42".to_string())]);
    }
}
