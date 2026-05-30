//! Saved-server picker — the disconnected-state center area + the
//! add/edit form modal.
//!
//! Renders the saved-server list (sorted by last-used desc), the
//! "Connecting…" / "Connection lost" / "Cert pending" status banner
//! for the disconnected variants of `ConnectionState`, and the form
//! modal that handles both add and edit. Form text inputs are routed
//! internally via [`apply_event`][text_input::apply_event]; the App
//! receives high-level [`ServerPickerOutcome`] variants for the
//! lifecycle events it needs to cover (Connect, Save, Save & Connect,
//! Edit, Remove, BeginAdd) because those depend on the App's `Identity`
//! / `SettingsStore` / backend handle and aren't safe to inline here.

use std::time::{SystemTime, UNIX_EPOCH};

use damascene_core::prelude::*;
use rumble_client::{ConnectionState, State};
use rumble_desktop_shell::RecentServer;

// ============================================================
// State + form
// ============================================================

/// Picker-owned UI state. Today this is just the optional add/edit
/// form; the saved-server list itself reads from the App's
/// `SettingsStore` so a successful add immediately repaints with the
/// new entry on the next frame.
#[derive(Default)]
pub struct ServerPickerState {
    pub form: Option<ServerForm>,
}

impl ServerPickerState {
    /// Open a fresh add-server form pre-filled with the given default
    /// username and a localhost placeholder address.
    pub fn begin_add(&mut self, default_username: &str) {
        self.form = Some(ServerForm::for_add(default_username));
    }

    /// Open an edit form pre-populated from `server`. `idx` is the
    /// position in `Settings.recent_servers` so the eventual save
    /// can find the original entry.
    pub fn begin_edit(&mut self, idx: usize, server: &RecentServer) {
        self.form = Some(ServerForm::for_edit(idx, server));
    }

    pub fn close(&mut self) {
        self.form = None;
    }
}

/// Local state for the saved-server add/edit modal.
///
/// `editing_index` is the position in `Settings.recent_servers` when
/// editing an existing entry, or `None` when the form is being used to
/// add a new server. The text fields are rendered via the global
/// `Selection` held on `RumbleApp`, so this struct doesn't carry
/// per-input selection state.
#[derive(Debug, Clone)]
pub struct ServerForm {
    pub editing_index: Option<usize>,
    pub addr: String,
    pub label: String,
    pub username: String,
    /// Inline validation message (e.g. "Address is required").
    pub error: Option<String>,
}

impl ServerForm {
    fn for_add(default_username: &str) -> Self {
        Self {
            editing_index: None,
            addr: "127.0.0.1:5000".to_string(),
            label: String::new(),
            username: default_username.to_string(),
            error: None,
        }
    }

    fn for_edit(idx: usize, server: &RecentServer) -> Self {
        Self {
            editing_index: Some(idx),
            addr: server.addr.clone(),
            label: server.label.clone(),
            username: server.username.clone(),
            error: None,
        }
    }
}

// ============================================================
// Outcome
// ============================================================

/// Result of routing a single event into the server picker. Every
/// command-emitting variant carries the index or form action the App
/// needs to apply against its `Identity` / `SettingsStore` / backend.
pub enum ServerPickerOutcome {
    /// Event wasn't recognized — the App should keep checking.
    Ignored,
    /// Event was consumed but produced no App-side action (e.g. a
    /// text-input keypress, form cancel, escape).
    Handled,
    /// User clicked "Add server…" — App should open a fresh form via
    /// [`ServerPickerState::begin_add`].
    BeginAdd,
    /// User clicked Connect on saved-server `idx`.
    ConnectRecent(usize),
    /// User clicked Edit on saved-server `idx` — App should open an
    /// edit form via [`ServerPickerState::begin_edit`] (it has access
    /// to the `RecentServer` lookup; we don't pipe it through here).
    BeginEdit(usize),
    /// User clicked Remove on saved-server `idx`.
    DeleteRecent(usize),
    /// User clicked Save in the form. App validates + persists; the
    /// picker stays open until the App calls
    /// [`ServerPickerState::close`] (typically after a successful save).
    Save,
    /// User clicked "Save & Connect" in the form.
    SaveAndConnect,
}

// ============================================================
// Routed-key constants
// ============================================================

const KEY_ADD: &str = "server:add";
const KEY_CONNECT_PREFIX: &str = "server:connect:";
const KEY_EDIT_PREFIX: &str = "server:edit:";
const KEY_DELETE_PREFIX: &str = "server:delete:";

const KEY_FORM_DISMISS: &str = "server_form:dismiss";
const KEY_FORM_CANCEL: &str = "server_form:cancel";
const KEY_FORM_SAVE: &str = "server_form:save";
const KEY_FORM_CONNECT: &str = "server_form:connect";
const KEY_FORM_ADDR: &str = "server_form:addr";
const KEY_FORM_LABEL: &str = "server_form:label";
const KEY_FORM_USER: &str = "server_form:user";

// ============================================================
// Render
// ============================================================

/// Render the disconnected-state center area: header, optional status
/// banner, and the saved-server list (or empty-state placeholder).
pub fn render_center(state: &State, recent_servers: &[RecentServer]) -> El {
    // Connection-status banner above the list. Most disconnected
    // states have nothing to say (Disconnected idle); Connecting /
    // ConnectionLost / CertPending get a one-line status so the
    // center area still communicates progress while the list keeps
    // taking up the same space.
    let status_banner: Option<El> = match &state.connection {
        ConnectionState::Disconnected => None,
        ConnectionState::Connecting { server_addr } => Some(text(format!("Connecting to {server_addr}...")).muted()),
        ConnectionState::ConnectionLost { error } => Some(
            text(format!("Connection lost: {error}"))
                .text_color(tokens::DESTRUCTIVE)
                .wrap_text(),
        ),
        ConnectionState::CertificatePending { cert_info } => Some(
            text(format!("Certificate pending for {}", cert_info.server_name))
                .text_color(tokens::WARNING)
                .wrap_text(),
        ),
        ConnectionState::Connected { .. } => unreachable!("server picker only renders while disconnected"),
    };

    let header = row([
        text("Servers").title(),
        spacer(),
        button("Add server…").key(KEY_ADD).primary(),
    ])
    .gap(tokens::SPACE_2)
    .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
    .align(Align::Center)
    .width(Size::Fill(1.0));

    // Render the list sorted by last_used desc; the row's stable
    // identifier remains the unsorted index (`server:connect:{idx}`),
    // so settings.modify by index continues to point at the right row.
    let body: El = if recent_servers.is_empty() {
        column([
            text("No saved servers yet.").muted(),
            paragraph("Click \"Add server…\" to bookmark one.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .justify(Justify::Center)
        .padding(tokens::SPACE_7)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
    } else {
        let mut order: Vec<usize> = (0..recent_servers.len()).collect();
        order.sort_by(|&a, &b| recent_servers[b].last_used_unix.cmp(&recent_servers[a].last_used_unix));
        let rows: Vec<El> = order
            .into_iter()
            .map(|idx| server_row(idx, &recent_servers[idx]))
            .collect();
        scroll([item_group(rows)])
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    };

    let mut children: Vec<El> = vec![header, divider()];
    if let Some(banner) = status_banner {
        children.push(
            row([banner])
                .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
                .width(Size::Fill(1.0)),
        );
        children.push(divider());
    }
    children.push(body);

    column(children).width(Size::Fill(1.0)).height(Size::Fill(1.0))
}

fn server_row(idx: usize, server: &RecentServer) -> El {
    let title = if server.label.is_empty() {
        server.addr.clone()
    } else {
        server.label.clone()
    };

    // Subtitle holds whichever of (addr, username, last-used) we still
    // have something to show: addr only repeats when label was the
    // primary title; username always shows; last-used is omitted when
    // the bookmark has never been connected to (last_used == 0).
    let mut subtitle_parts: Vec<String> = Vec::new();
    if !server.label.is_empty() {
        subtitle_parts.push(server.addr.clone());
    }
    if !server.username.is_empty() {
        subtitle_parts.push(server.username.clone());
    }
    let rel = relative_time(server.last_used_unix);
    if !rel.is_empty() {
        subtitle_parts.push(rel);
    }
    let subtitle = subtitle_parts.join(" · ");

    item([
        item_content([item_title(title), item_description(subtitle)]),
        item_actions([
            button("Connect").key(format!("{KEY_CONNECT_PREFIX}{idx}")).primary(),
            button("Edit").key(format!("{KEY_EDIT_PREFIX}{idx}")).ghost(),
            button("Remove").key(format!("{KEY_DELETE_PREFIX}{idx}")).ghost(),
        ]),
    ])
}

/// Render the add/edit form modal. Returns `None` when no form is open.
pub fn render_form_modal(state: &ServerPickerState, selection: &Selection) -> Option<El> {
    let state_form = state.form.as_ref()?;
    let title = if state_form.editing_index.is_some() {
        "Edit server"
    } else {
        "Add server"
    };

    let mut fields: Vec<El> = vec![
        form_item([
            form_label("Address"),
            form_control(text_input(&state_form.addr, selection, KEY_FORM_ADDR).width(Size::Fill(1.0))),
            form_description("host:port — e.g. 127.0.0.1:5000"),
        ]),
        form_item([
            form_label("Label (optional)"),
            form_control(text_input(&state_form.label, selection, KEY_FORM_LABEL).width(Size::Fill(1.0))),
        ]),
        form_item([
            form_label("Username"),
            form_control(text_input(&state_form.username, selection, KEY_FORM_USER).width(Size::Fill(1.0))),
        ]),
    ];
    if let Some(err) = &state_form.error {
        fields.push(form_message(err.clone()));
    }

    let body: Vec<El> = vec![
        form(fields),
        row([
            button("Cancel").key(KEY_FORM_CANCEL),
            spacer(),
            button("Save").key(KEY_FORM_SAVE),
            button("Save & Connect").key(KEY_FORM_CONNECT).primary(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center),
    ];

    Some(modal("server_form", title, body))
}

// ============================================================
// Event handling
// ============================================================

/// Route a single event into the picker. The picker handles its own
/// text-input / cancel / dismiss / escape lifecycle internally;
/// everything that needs the App's `Identity` / `SettingsStore` /
/// backend (Connect, Save, Edit, Remove) flows back as a
/// [`ServerPickerOutcome`].
pub fn handle_event(state: &mut ServerPickerState, event: &UiEvent, selection: &mut Selection) -> ServerPickerOutcome {
    // List-row actions (clicking buttons on a saved-server card).
    if event.is_click_or_activate(KEY_ADD) {
        return ServerPickerOutcome::BeginAdd;
    }
    if let Some(idx) = parse_index_route(event, KEY_CONNECT_PREFIX) {
        return ServerPickerOutcome::ConnectRecent(idx);
    }
    if let Some(idx) = parse_index_route(event, KEY_EDIT_PREFIX) {
        return ServerPickerOutcome::BeginEdit(idx);
    }
    if let Some(idx) = parse_index_route(event, KEY_DELETE_PREFIX) {
        return ServerPickerOutcome::DeleteRecent(idx);
    }

    // Form lifecycle. Cancel / dismiss / Escape all close, and only
    // when a form is actually open — otherwise an unrelated Escape on
    // the disconnected screen would be a no-op consumed here, which
    // we don't want.
    let form_open = state.form.is_some();
    if event.is_click_or_activate(KEY_FORM_CANCEL)
        || (event.is_route(KEY_FORM_DISMISS) && event.kind == UiEventKind::Click)
        || (form_open && event.kind == UiEventKind::Escape)
    {
        state.form = None;
        return ServerPickerOutcome::Handled;
    }
    if event.is_click_or_activate(KEY_FORM_SAVE) {
        return ServerPickerOutcome::Save;
    }
    if event.is_click_or_activate(KEY_FORM_CONNECT) {
        return ServerPickerOutcome::SaveAndConnect;
    }

    // Form text inputs.
    if let Some(field) = form_field_from_target(event)
        && let Some(form) = state.form.as_mut()
    {
        let buf = match field {
            FormField::Addr => &mut form.addr,
            FormField::Label => &mut form.label,
            FormField::Username => &mut form.username,
        };
        text_input::apply_event(buf, selection, field.key(), event);
        return ServerPickerOutcome::Handled;
    }

    ServerPickerOutcome::Ignored
}

#[derive(Clone, Copy)]
enum FormField {
    Addr,
    Label,
    Username,
}

impl FormField {
    fn key(self) -> &'static str {
        match self {
            FormField::Addr => KEY_FORM_ADDR,
            FormField::Label => KEY_FORM_LABEL,
            FormField::Username => KEY_FORM_USER,
        }
    }
}

fn form_field_from_target(event: &UiEvent) -> Option<FormField> {
    match event.target_key()? {
        KEY_FORM_ADDR => Some(FormField::Addr),
        KEY_FORM_LABEL => Some(FormField::Label),
        KEY_FORM_USER => Some(FormField::Username),
        _ => None,
    }
}

fn parse_index_route(event: &UiEvent, prefix: &str) -> Option<usize> {
    if !matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
        return None;
    }
    event.route()?.strip_prefix(prefix)?.parse().ok()
}

// ============================================================
// Helpers
// ============================================================

/// Coarse "n minutes ago" / "n hours ago" / "n days ago" formatter for
/// the server-row subtitle. Returns an empty string when `then == 0`
/// (the sentinel for "never connected to this server").
fn relative_time(then: u64) -> String {
    if then == 0 {
        return String::new();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now <= then {
        return "just now".to_string();
    }
    let secs = now - then;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}
