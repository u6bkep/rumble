//! Pre-connect view: pick a saved server or add a new one.
//!
//! Two modes share one card:
//! - **List mode** (`Selection::None`): one row per `RecentServer` plus
//!   a "+ New server" row. Clicking a row enters form mode.
//! - **Form mode** (`Selection::Saved(i)` / `Selection::New`): editable
//!   server / username / password / label, an "auto-connect on launch"
//!   toggle, and Connect / Cancel / Forget actions.
//!
//! On Connect we update `Settings::recent_servers` (insert or refresh
//! the matching entry by `addr`) and bump `last_used_unix`. On
//! AcceptCertificate we persist the cert into `accepted_certificates`
//! so subsequent connects skip the prompt entirely.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::backend::UiBackend;
use eframe::egui::{self, Align, Layout, Margin, RichText, Ui};
use rumble_desktop_shell::{AcceptedCertificate, RecentServer, SettingsStore};
use rumble_protocol::{Command, ConnectionState, State};
use rumble_widgets::{ButtonArgs, GroupBox, PressableRole, SurfaceFrame, SurfaceKind, TextInput, TextRole, UiExt};

use crate::identity::Identity;

/// What the connect card is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Render the list of saved servers + a "new" entry.
    None,
    /// Render the form populated from `recent_servers[idx]`.
    Saved { idx: usize },
    /// Render an empty form for a brand-new server.
    New,
}

impl Default for Selection {
    fn default() -> Self {
        Self::None
    }
}

/// In-memory edit buffer for the active form. Flushed back into the
/// settings store on Connect; never persisted directly.
#[derive(Debug, Clone, Default)]
pub struct ServerDraft {
    pub addr: String,
    pub label: String,
    pub username: String,
    pub password: String,
    pub auto_connect: bool,
}

pub struct ConnectForm {
    pub selection: Selection,
    pub editing: ServerDraft,
    /// Set to true after we've initialised `selection` from the
    /// settings store on the first frame. Avoids overwriting the
    /// user's later choices on every render.
    initialised: bool,
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            selection: Selection::default(),
            editing: ServerDraft::default(),
            initialised: false,
        }
    }
}

impl ConnectForm {
    /// First-frame setup: pick a sensible default selection given the
    /// current settings store. With no saved servers we drop straight
    /// into the new-server form so the first-time user sees a familiar
    /// prompt; with saved servers we show the list.
    fn init_if_needed(&mut self, settings: &SettingsStore) {
        if self.initialised {
            return;
        }
        self.initialised = true;
        if settings.settings().recent_servers.is_empty() {
            self.selection = Selection::New;
            self.editing = new_draft_default();
        }
    }
}

fn new_draft_default() -> ServerDraft {
    ServerDraft {
        addr: "[::1]:5000".into(),
        label: String::new(),
        username: whoami_username(),
        password: String::new(),
        auto_connect: false,
    }
}

fn whoami_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "rumbler".to_string())
}

pub fn render<B: UiBackend>(
    ui: &mut Ui,
    state: &State,
    form: &mut ConnectForm,
    settings: &mut SettingsStore,
    identity: &Identity,
    backend: &B,
) {
    form.init_if_needed(settings);

    let rect = ui.available_rect_before_wrap();
    let center_w = 520.0_f32.min(rect.width() - 24.0);

    ui.with_layout(Layout::top_down(Align::Center), |ui| {
        ui.add_space((rect.height() * 0.10).min(60.0));
        ui.allocate_ui_with_layout(egui::Vec2::new(center_w, 0.0), Layout::top_down(Align::Min), |ui| {
            connect_card(ui, state, form, settings, identity, backend);
        });
    });
}

fn connect_card<B: UiBackend>(
    ui: &mut Ui,
    state: &State,
    form: &mut ConnectForm,
    settings: &mut SettingsStore,
    identity: &Identity,
    backend: &B,
) {
    SurfaceFrame::new(SurfaceKind::Panel)
        .inner_margin(Margin::same(20))
        .show(ui, |ui| {
            ui.label(
                RichText::new("Connect to a Rumble server")
                    .font(ui.theme().font(TextRole::Heading))
                    .strong(),
            );
            ui.add_space(10.0);

            match form.selection.clone() {
                Selection::None => {
                    server_list(ui, form, settings);
                }
                Selection::Saved { idx } => {
                    server_form(ui, state, form, settings, identity, backend, Some(idx));
                }
                Selection::New => {
                    server_form(ui, state, form, settings, identity, backend, None);
                }
            }

            // Cert prompt is independent of list/form mode — surface it
            // whenever the backend asks for a trust decision so the user
            // never has to wonder why a connect appears stuck.
            if let ConnectionState::CertificatePending { cert_info } = &state.connection {
                ui.add_space(12.0);
                cert_prompt_card(ui, cert_info, settings, backend);
            }

            ui.add_space(10.0);
            let public_key = identity
                .public_key()
                .map(|key| {
                    let hex = hex::encode(key);
                    format!("Public key: {}…", &hex[..16])
                })
                .unwrap_or_else(|| "Public key: not set up".to_string());
            ui.label(
                RichText::new(public_key)
                    .color(ui.theme().tokens().text_muted)
                    .font(ui.theme().font(TextRole::Mono)),
            );
        });
}

fn server_list(ui: &mut Ui, form: &mut ConnectForm, settings: &SettingsStore) {
    GroupBox::new("Saved servers")
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            let recents = &settings.settings().recent_servers;
            if recents.is_empty() {
                ui.label(
                    RichText::new("No saved servers yet.")
                        .color(ui.theme().tokens().text_muted)
                        .font(ui.theme().font(TextRole::Body)),
                );
                ui.add_space(6.0);
            } else {
                // Sort indices by last-used desc so the freshest server
                // is on top, but iterate by sorted order to keep the
                // visual stable. Stored order doesn't matter for UX.
                let mut order: Vec<usize> = (0..recents.len()).collect();
                order.sort_by_key(|&i| std::cmp::Reverse(recents[i].last_used_unix));

                for i in order {
                    let server = &recents[i];
                    if server_row(ui, server).clicked() {
                        form.selection = Selection::Saved { idx: i };
                        form.editing = ServerDraft {
                            addr: server.addr.clone(),
                            label: server.label.clone(),
                            username: server.username.clone(),
                            password: String::new(),
                            auto_connect: settings.settings().auto_connect_addr.as_deref()
                                == Some(server.addr.as_str()),
                        };
                    }
                    ui.add_space(2.0);
                }
                ui.add_space(4.0);
            }

            // Always-available "new server" entry at the bottom.
            if ButtonArgs::new("+ New server")
                .role(PressableRole::Default)
                .show(ui)
                .clicked()
            {
                form.selection = Selection::New;
                form.editing = new_draft_default();
            }
        });
}

/// One row in the saved-server list. Returns the click response so
/// the caller can react.
fn server_row(ui: &mut Ui, server: &RecentServer) -> egui::Response {
    let title = if server.label.is_empty() {
        server.addr.clone()
    } else {
        server.label.clone()
    };
    let subtitle = if server.label.is_empty() {
        format!(
            "user: {} · {}",
            or_unknown(&server.username),
            relative_time(server.last_used_unix)
        )
    } else {
        format!(
            "{} · user: {} · {}",
            server.addr,
            or_unknown(&server.username),
            relative_time(server.last_used_unix)
        )
    };

    let resp = ButtonArgs::new(title.clone()).role(PressableRole::Default).show(ui);
    ui.label(
        RichText::new(subtitle)
            .color(ui.theme().tokens().text_muted)
            .font(ui.theme().font(TextRole::Label)),
    );
    resp
}

fn or_unknown(s: &str) -> &str {
    if s.is_empty() { "—" } else { s }
}

fn server_form<B: UiBackend>(
    ui: &mut Ui,
    state: &State,
    form: &mut ConnectForm,
    settings: &mut SettingsStore,
    identity: &Identity,
    backend: &B,
    saved_idx: Option<usize>,
) {
    let title = if saved_idx.is_some() {
        if form.editing.label.is_empty() {
            form.editing.addr.clone()
        } else {
            form.editing.label.clone()
        }
    } else {
        "New server".into()
    };
    let connecting = state.connection.is_connecting();
    let any_recents = !settings.settings().recent_servers.is_empty();

    GroupBox::new(title)
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            // Address is locked once a server is saved — changing it
            // would orphan the trust-store entry and pop a fresh cert
            // prompt. Render as a read-only label in that case so the
            // user can still see what they're connecting to.
            labeled(ui, "Server", |ui| {
                if saved_idx.is_some() {
                    ui.label(RichText::new(&form.editing.addr).font(ui.theme().font(TextRole::Mono)));
                } else {
                    TextInput::new(&mut form.editing.addr)
                        .placeholder("host:port")
                        .desired_width(300.0)
                        .show(ui);
                }
            });
            ui.add_space(4.0);
            labeled(ui, "Username", |ui| {
                TextInput::new(&mut form.editing.username)
                    .placeholder("your username")
                    .desired_width(300.0)
                    .show(ui);
            });
            ui.add_space(4.0);
            labeled(ui, "Password", |ui| {
                TextInput::new(&mut form.editing.password)
                    .placeholder("only for first-join")
                    .password(true)
                    .desired_width(300.0)
                    .show(ui);
            });
            ui.add_space(4.0);
            labeled(ui, "Label", |ui| {
                TextInput::new(&mut form.editing.label)
                    .placeholder("optional friendly name")
                    .desired_width(300.0)
                    .show(ui);
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(98.0); // line up with the labelled inputs
                let mut auto = form.editing.auto_connect;
                if ui.checkbox(&mut auto, "Connect to this server on launch").changed() {
                    form.editing.auto_connect = auto;
                }
            });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let status_text = match &state.connection {
                    ConnectionState::Disconnected => String::new(),
                    ConnectionState::Connecting { server_addr } => format!("Connecting to {server_addr}…"),
                    ConnectionState::ConnectionLost { error } => format!("Lost: {error}"),
                    ConnectionState::CertificatePending { .. } => "Waiting for certificate approval…".into(),
                    ConnectionState::Connected { server_name, .. } => format!("Connected to {server_name}"),
                };
                ui.label(
                    RichText::new(status_text)
                        .color(ui.theme().tokens().text_muted)
                        .font(ui.theme().font(TextRole::Body)),
                );

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let connect_label = if connecting { "Connecting…" } else { "Connect" };
                    let identity_ready = identity.public_key().is_some() && !identity.needs_unlock();
                    let valid = form_valid(&form.editing) && identity_ready;
                    if ButtonArgs::new(connect_label)
                        .role(PressableRole::Primary)
                        .disabled(connecting || !valid)
                        .min_width(120.0)
                        .show(ui)
                        .on_disabled_hover_text(if identity.public_key().is_none() {
                            "Set up an identity key before connecting"
                        } else if identity.needs_unlock() {
                            "Unlock your encrypted identity before connecting"
                        } else {
                            "Enter a server and username"
                        })
                        .clicked()
                    {
                        commit_and_connect(form, settings, identity, backend);
                    }

                    if let Some(idx) = saved_idx {
                        if ButtonArgs::new("Forget").role(PressableRole::Danger).show(ui).clicked() {
                            forget_server(form, settings, idx);
                        }
                    } else if any_recents
                        && ButtonArgs::new("Cancel")
                            .role(PressableRole::Default)
                            .show(ui)
                            .clicked()
                    {
                        form.selection = Selection::None;
                    }
                });
            });
        });
}

fn cert_prompt_card<B: UiBackend>(
    ui: &mut Ui,
    cert_info: &rumble_protocol::PendingCertificate,
    settings: &mut SettingsStore,
    backend: &B,
) {
    GroupBox::new("Untrusted certificate")
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.label(RichText::new(format!("Server: {}", cert_info.server_name)).font(ui.theme().font(TextRole::Body)));
            ui.label(
                RichText::new(format!("Fingerprint: {}", cert_info.fingerprint_hex()))
                    .font(ui.theme().font(TextRole::Mono))
                    .color(ui.theme().tokens().text_muted),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ButtonArgs::new("Reject")
                    .role(PressableRole::Default)
                    .show(ui)
                    .clicked()
                {
                    backend.send(Command::RejectCertificate);
                }
                if ButtonArgs::new("Trust and connect")
                    .role(PressableRole::Primary)
                    .show(ui)
                    .clicked()
                {
                    persist_accepted_cert(settings, cert_info);
                    backend.send(Command::AcceptCertificate);
                }
            });
        });
}

/// Push the freshly-trusted cert into the settings store. We dedup by
/// `(server_name, fingerprint_hex)` so accepting the same cert twice
/// (e.g. after a transient connection failure) doesn't grow the file.
fn persist_accepted_cert(settings: &mut SettingsStore, cert_info: &rumble_protocol::PendingCertificate) {
    let server_name = cert_info.server_name.clone();
    let fingerprint = cert_info.fingerprint_hex();
    let der = cert_info.certificate_der.clone();
    settings.modify(|s| {
        let already = s
            .accepted_certificates
            .iter()
            .any(|c| c.server_name == server_name && c.fingerprint_hex == fingerprint);
        if !already {
            s.accepted_certificates
                .push(AcceptedCertificate::from_der(server_name, fingerprint, &der));
        }
    });
}

/// Save the form to recent_servers (insert or refresh) and dispatch
/// `Command::Connect`. `auto_connect_addr` is set or cleared to match
/// the form's checkbox: turning the toggle on for one server clears
/// any previous choice.
fn commit_and_connect<B: UiBackend>(
    form: &mut ConnectForm,
    settings: &mut SettingsStore,
    identity: &Identity,
    backend: &B,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let draft = form.editing.clone();
    let addr = draft.addr.trim().to_string();
    let username = draft.username.trim().to_string();
    let label = draft.label.trim().to_string();
    let auto_connect = draft.auto_connect;

    let mut saved_idx = None;
    settings.modify(|s| {
        let existing = s.recent_servers.iter().position(|r| r.addr == addr);
        match existing {
            Some(i) => {
                let entry = &mut s.recent_servers[i];
                entry.label = label.clone();
                entry.username = username.clone();
                entry.last_used_unix = now;
                saved_idx = Some(i);
            }
            None => {
                s.recent_servers.push(RecentServer {
                    addr: addr.clone(),
                    label: label.clone(),
                    username: username.clone(),
                    last_used_unix: now,
                });
                saved_idx = Some(s.recent_servers.len() - 1);
            }
        }
        if auto_connect {
            s.auto_connect_addr = Some(addr.clone());
        } else if s.auto_connect_addr.as_deref() == Some(addr.as_str()) {
            s.auto_connect_addr = None;
        }
    });

    if let Some(i) = saved_idx {
        form.selection = Selection::Saved { idx: i };
    }

    let password = (!draft.password.is_empty()).then_some(draft.password);
    let Some(public_key) = identity.public_key() else {
        return;
    };
    backend.send(Command::Connect {
        addr,
        name: username,
        public_key,
        password,
    });
}

fn forget_server(form: &mut ConnectForm, settings: &mut SettingsStore, idx: usize) {
    let removed_addr = settings.settings().recent_servers.get(idx).map(|r| r.addr.clone());
    settings.modify(|s| {
        if idx < s.recent_servers.len() {
            s.recent_servers.remove(idx);
        }
        if let Some(addr) = &removed_addr
            && s.auto_connect_addr.as_deref() == Some(addr.as_str())
        {
            s.auto_connect_addr = None;
        }
    });
    form.selection = if settings.settings().recent_servers.is_empty() {
        Selection::New
    } else {
        Selection::None
    };
    form.editing = new_draft_default();
}

fn labeled(ui: &mut Ui, label: &str, mut content: impl FnMut(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(egui::Vec2::new(90.0, 0.0), Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(label)
                    .color(ui.theme().tokens().text_muted)
                    .font(ui.theme().font(TextRole::Label)),
            );
        });
        ui.add_space(8.0);
        content(ui);
    });
}

fn form_valid(draft: &ServerDraft) -> bool {
    !draft.addr.trim().is_empty() && !draft.username.trim().is_empty()
}

/// Best-effort relative timestamp ("just now", "5m ago", "yesterday",
/// "Mar 12"). We don't need a real i18n stack here — the connect view
/// is a brief glance, not a journal.
fn relative_time(unix: u64) -> String {
    if unix == 0 {
        return "never".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let delta = now.saturating_sub(unix);
    if delta < 60 {
        "just now".into()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else if delta < 7 * 86_400 {
        format!("{}d ago", delta / 86_400)
    } else {
        // Older than a week — fall back to a stable, locale-free tag.
        format!("{}w ago", delta / (7 * 86_400))
    }
}
