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

/// Picker-owned UI state: the optional add/edit form, the open "…"
/// row-overflow menu, and the pending remove confirmation. The
/// saved-server list itself reads from the App's `SettingsStore` so a
/// successful add immediately repaints with the new entry on the next
/// frame.
#[derive(Default)]
pub struct ServerPickerState {
    pub form: Option<ServerForm>,
    /// Address of the saved server whose "…" overflow menu is open,
    /// or `None` when no row menu is showing. Addresses (not list
    /// positions) identify rows — see [`ServerPickerOutcome`].
    pub row_menu: Option<String>,
    /// Address of the saved server awaiting remove confirmation.
    /// While set, the confirm dialog renders and the actual
    /// [`ServerPickerOutcome::DeleteRecent`] only fires on confirm.
    pub pending_remove: Option<String>,
}

impl ServerPickerState {
    /// Open a fresh add-server form pre-filled with the given default
    /// username and a localhost placeholder address.
    pub fn begin_add(&mut self, default_username: &str) {
        self.form = Some(ServerForm::for_add(default_username));
    }

    /// Open an edit form pre-populated from `server`. The entry's
    /// address is remembered so the eventual save can find the
    /// original entry even if the list is reordered meanwhile.
    pub fn begin_edit(&mut self, server: &RecentServer) {
        self.form = Some(ServerForm::for_edit(server));
    }

    pub fn close(&mut self) {
        self.form = None;
    }
}

/// Local state for the saved-server add/edit modal.
///
/// `editing_addr` is the address of the `Settings.recent_servers`
/// entry being edited, or `None` when the form is being used to add a
/// new server. Addresses are unique within the list (saves dedupe by
/// addr), so this survives reorders and deletions where a positional
/// index would not. The text fields are rendered via the global
/// `Selection` held on `RumbleApp`, so this struct doesn't carry
/// per-input selection state.
#[derive(Debug, Clone)]
pub struct ServerForm {
    pub editing_addr: Option<String>,
    pub addr: String,
    pub label: String,
    pub username: String,
    /// Inline validation message (e.g. "Address is required").
    pub error: Option<String>,
}

impl ServerForm {
    fn for_add(default_username: &str) -> Self {
        Self {
            editing_addr: None,
            addr: "127.0.0.1:5000".to_string(),
            label: String::new(),
            username: default_username.to_string(),
            error: None,
        }
    }

    fn for_edit(server: &RecentServer) -> Self {
        Self {
            editing_addr: Some(server.addr.clone()),
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
/// command-emitting variant carries the server address or form action
/// the App needs to apply against its `Identity` / `SettingsStore` /
/// backend. Addresses (not list positions) identify rows so a stale
/// click — e.g. the second half of a double-click landing after the
/// first already removed an entry and shifted the list — can never
/// resolve to a different server.
pub enum ServerPickerOutcome {
    /// Event wasn't recognized — the App should keep checking.
    Ignored,
    /// Event was consumed but produced no App-side action (e.g. a
    /// text-input keypress, form cancel, escape).
    Handled,
    /// User clicked "Add server…" — App should open a fresh form via
    /// [`ServerPickerState::begin_add`].
    BeginAdd,
    /// User clicked Connect on the saved server with this address.
    ConnectRecent(String),
    /// User picked "Edit…" from the row's overflow menu — App should
    /// open an edit form via [`ServerPickerState::begin_edit`] (it has
    /// access to the `RecentServer` lookup; we don't pipe it through
    /// here).
    BeginEdit(String),
    /// User confirmed removal of the saved server with this address in
    /// the remove-confirmation dialog. (Picking "Remove…" from the row
    /// menu only opens the dialog; it never fires this directly.)
    DeleteRecent(String),
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
/// "…" overflow-menu trigger on a row (`server:menu:{addr}`).
const KEY_MENU_PREFIX: &str = "server:menu:";
/// Edit / Remove items inside the open row menu.
const KEY_EDIT_PREFIX: &str = "server:edit:";
const KEY_DELETE_PREFIX: &str = "server:delete:";
/// The row-overflow dropdown itself. Only one is open at a time, so a
/// fixed key is fine; the anchored trigger key carries the address.
/// `dropdown()` derives its scrim route as `{key}:dismiss`.
const KEY_ROW_MENU: &str = "server:row_menu";
const KEY_ROW_MENU_DISMISS: &str = "server:row_menu:dismiss";

/// Remove-confirmation dialog. `modal()` derives `{key}:dismiss`.
const KEY_REMOVE_MODAL: &str = "server_remove";
const KEY_REMOVE_DISMISS: &str = "server_remove:dismiss";
const KEY_REMOVE_CANCEL: &str = "server_remove:cancel";
const KEY_REMOVE_CONFIRM: &str = "server_remove:confirm";

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
    // identifier is the server address (`server:connect:{addr}`) —
    // unique within the list and immune to rows shifting position
    // when an entry is removed or the sort order changes.
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
        let rows: Vec<El> = order.into_iter().map(|idx| server_row(&recent_servers[idx])).collect();
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

fn server_row(server: &RecentServer) -> El {
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

    // Connect stays the row's primary action; Edit / Remove live
    // behind the "…" overflow trigger (shadcn row-actions idiom) so
    // the destructive path is never one stray click away. The trigger
    // is a focusable icon_button, so the menu stays keyboard-reachable
    // (Tab to the trigger, Enter/Space to open, then Tab/Enter within
    // the menu items).
    item([
        item_content([item_title(title), item_description(subtitle)]),
        item_actions([
            button("Connect")
                .key(format!("{KEY_CONNECT_PREFIX}{}", server.addr))
                .primary(),
            icon_button("more-horizontal")
                .key(format!("{KEY_MENU_PREFIX}{}", server.addr))
                .ghost()
                .tooltip("More actions"),
        ]),
    ])
}

// ============================================================
// Overlays — row overflow menu + remove confirmation
// ============================================================

/// Floating layers owned by the picker (beyond the add/edit form
/// modal): the "…" row-actions dropdown and the remove-confirmation
/// dialog. Returned separately so the App's layer composer controls
/// z-order (dialog above menu).
#[derive(Default)]
pub struct ServerPickerOverlays {
    pub row_menu: Option<El>,
    pub remove_dialog: Option<El>,
}

impl ServerPickerOverlays {
    pub fn any(&self) -> bool {
        self.row_menu.is_some() || self.remove_dialog.is_some()
    }
}

/// Render the picker's floating layers. `recent_servers` is consulted
/// for display names and to drop a menu whose row vanished from the
/// list (the dropdown anchors to the row's trigger key, which no
/// longer exists in the laid-out tree in that case).
pub fn render_overlays(state: &ServerPickerState, recent_servers: &[RecentServer]) -> ServerPickerOverlays {
    let mut out = ServerPickerOverlays::default();

    if let Some(addr) = state.row_menu.as_deref()
        && recent_servers.iter().any(|s| s.addr == addr)
    {
        out.row_menu = Some(dropdown(
            KEY_ROW_MENU,
            format!("{KEY_MENU_PREFIX}{addr}"),
            [
                menu_item("Edit…").key(format!("{KEY_EDIT_PREFIX}{addr}")),
                menu_item("Remove…").key(format!("{KEY_DELETE_PREFIX}{addr}")),
            ],
        ));
    }

    if let Some(addr) = state.pending_remove.as_deref() {
        // Prefer the bookmark's label for the prompt; fall back to the
        // address (also covers an entry deleted out from under the
        // dialog — confirming then is a harmless no-op removal).
        let name = recent_servers
            .iter()
            .find(|s| s.addr == addr)
            .map(|s| {
                if s.label.is_empty() {
                    s.addr.clone()
                } else {
                    s.label.clone()
                }
            })
            .unwrap_or_else(|| addr.to_string());
        out.remove_dialog = Some(modal(
            KEY_REMOVE_MODAL,
            "Remove server",
            [
                paragraph(format!("Remove \"{name}\" from your saved servers?")).text_color(tokens::WARNING),
                row([
                    button("Cancel").key(KEY_REMOVE_CANCEL),
                    spacer(),
                    button("Remove").key(KEY_REMOVE_CONFIRM).destructive(),
                ])
                .gap(tokens::SPACE_2)
                .width(Size::Fill(1.0))
                .align(Align::Center),
            ],
        ));
    }

    out
}

/// Render the add/edit form modal. Returns `None` when no form is open.
pub fn render_form_modal(state: &ServerPickerState, selection: &Selection) -> Option<El> {
    let state_form = state.form.as_ref()?;
    let title = if state_form.editing_addr.is_some() {
        "Edit server"
    } else {
        "Add server"
    };

    let mut fields: Vec<El> = vec![
        form_item([
            form_label("Address"),
            form_control(text_input(KEY_FORM_ADDR, &state_form.addr, selection).width(Size::Fill(1.0))),
            form_description("host:port — e.g. 127.0.0.1:5000"),
        ]),
        form_item([
            form_label("Label (optional)"),
            form_control(text_input(KEY_FORM_LABEL, &state_form.label, selection).width(Size::Fill(1.0))),
        ]),
        form_item([
            form_label("Username"),
            form_control(text_input(KEY_FORM_USER, &state_form.username, selection).width(Size::Fill(1.0))),
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
    if let Some(addr) = parse_addr_route(event, KEY_CONNECT_PREFIX) {
        return ServerPickerOutcome::ConnectRecent(addr);
    }
    // "…" trigger toggles the row overflow menu.
    if let Some(addr) = parse_addr_route(event, KEY_MENU_PREFIX) {
        state.row_menu = match state.row_menu.as_deref() {
            Some(open) if open == addr => None,
            _ => Some(addr),
        };
        return ServerPickerOutcome::Handled;
    }
    // Open row menu: Edit / Remove items, click-outside, Escape. The
    // Edit/Remove routes only exist while the menu element is up, but
    // gating on `row_menu` keeps Escape from leaking to lower layers.
    if state.row_menu.is_some() {
        if (event.is_route(KEY_ROW_MENU_DISMISS) && event.kind == UiEventKind::Click)
            || event.kind == UiEventKind::Escape
        {
            state.row_menu = None;
            return ServerPickerOutcome::Handled;
        }
        if let Some(addr) = parse_addr_route(event, KEY_EDIT_PREFIX) {
            state.row_menu = None;
            return ServerPickerOutcome::BeginEdit(addr);
        }
        if let Some(addr) = parse_addr_route(event, KEY_DELETE_PREFIX) {
            // Removal is destructive — stage it behind the confirm
            // dialog instead of deleting on the spot.
            state.row_menu = None;
            state.pending_remove = Some(addr);
            return ServerPickerOutcome::Handled;
        }
    }
    // Remove-confirmation dialog lifecycle.
    if state.pending_remove.is_some() {
        if event.is_click_or_activate(KEY_REMOVE_CANCEL)
            || (event.is_route(KEY_REMOVE_DISMISS) && event.kind == UiEventKind::Click)
            || event.kind == UiEventKind::Escape
        {
            state.pending_remove = None;
            return ServerPickerOutcome::Handled;
        }
        if event.is_click_or_activate(KEY_REMOVE_CONFIRM)
            && let Some(addr) = state.pending_remove.take()
        {
            return ServerPickerOutcome::DeleteRecent(addr);
        }
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
        text_input::apply_event(buf, selection, event, field.key());
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

/// Extract the server address from a `server:{action}:{addr}` routed
/// key. Addresses may themselves contain `:` (host:port), which is
/// fine — everything after the action prefix is the address.
fn parse_addr_route(event: &UiEvent, prefix: &str) -> Option<String> {
    if !matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
        return None;
    }
    let addr = event.route()?.strip_prefix(prefix)?;
    if addr.is_empty() {
        return None;
    }
    Some(addr.to_string())
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

#[cfg(test)]
mod tests {
    use damascene_core::{event::UiEvent, selection::Selection};

    use super::*;

    const ADDR: &str = "127.0.0.1:5000";

    fn click(state: &mut ServerPickerState, route: &str) -> ServerPickerOutcome {
        let event = UiEvent::synthetic_click(route);
        handle_event(state, &event, &mut Selection::default())
    }

    /// The "…" trigger toggles the row menu open and closed.
    #[test]
    fn overflow_trigger_toggles_row_menu() {
        let mut state = ServerPickerState::default();
        let trigger = format!("{KEY_MENU_PREFIX}{ADDR}");
        assert!(matches!(click(&mut state, &trigger), ServerPickerOutcome::Handled));
        assert_eq!(state.row_menu.as_deref(), Some(ADDR));
        assert!(matches!(click(&mut state, &trigger), ServerPickerOutcome::Handled));
        assert_eq!(state.row_menu, None, "second trigger click must close the menu");
    }

    /// "Remove…" in the menu must only stage the confirmation — the
    /// destructive DeleteRecent fires exclusively from the dialog's
    /// confirm button.
    #[test]
    fn remove_requires_confirmation() {
        let mut state = ServerPickerState {
            row_menu: Some(ADDR.to_string()),
            ..Default::default()
        };
        let out = click(&mut state, &format!("{KEY_DELETE_PREFIX}{ADDR}"));
        assert!(matches!(out, ServerPickerOutcome::Handled), "menu pick must not delete");
        assert_eq!(state.row_menu, None);
        assert_eq!(state.pending_remove.as_deref(), Some(ADDR));

        let out = click(&mut state, KEY_REMOVE_CONFIRM);
        assert!(matches!(out, ServerPickerOutcome::DeleteRecent(addr) if addr == ADDR));
        assert_eq!(state.pending_remove, None);
    }

    /// Cancel (and the scrim dismiss route) close the dialog without
    /// deleting anything.
    #[test]
    fn remove_cancel_keeps_server() {
        let mut state = ServerPickerState {
            pending_remove: Some(ADDR.to_string()),
            ..Default::default()
        };
        assert!(matches!(
            click(&mut state, KEY_REMOVE_CANCEL),
            ServerPickerOutcome::Handled
        ));
        assert_eq!(state.pending_remove, None);

        state.pending_remove = Some(ADDR.to_string());
        assert!(matches!(
            click(&mut state, KEY_REMOVE_DISMISS),
            ServerPickerOutcome::Handled
        ));
        assert_eq!(state.pending_remove, None);
    }

    /// "Edit…" flows back as BeginEdit and closes the menu.
    #[test]
    fn edit_from_menu_begins_edit() {
        let mut state = ServerPickerState {
            row_menu: Some(ADDR.to_string()),
            ..Default::default()
        };
        let out = click(&mut state, &format!("{KEY_EDIT_PREFIX}{ADDR}"));
        assert!(matches!(out, ServerPickerOutcome::BeginEdit(addr) if addr == ADDR));
        assert_eq!(state.row_menu, None);
    }
}
