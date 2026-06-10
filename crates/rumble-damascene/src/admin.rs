//! Server-administration UI: permission groups and their members.
//!
//! Rendered as the **Admin** tab of the Settings dialog when the
//! connected user has `MANAGE_ACL` at the root. The tab owns its own
//! ephemeral UI state (search query, which group is expanded, popover
//! and dialog flags). Commands fire immediately — there is no
//! pending-state / Save round-trip for admin actions, matching the
//! egui client's semantics.
//!
//! Visual model: each group is one accordion card. The header shows
//! `[built-in]` badge, name, member count, and a short permission
//! summary. Expanding reveals a chip list of members with `×` to
//! remove and a `+ add user` popover, a grid of switches for the
//! group's base permission bitfield (room-scoped + server-scoped
//! sections, with a `+ all` shorthand), and a footer counting how
//! many rooms reference the group in an ACL.
//!
//! Per the design discussion: base permissions are first-class and
//! always editable; they show up as synthetic `(base)` inheritance
//! rows in the per-room ACL editor (see `room_acl.rs`).

use damascene_core::prelude::*;

use rumble_client::{Command, State};
use rumble_protocol::{
    permissions::{ALL_ROOM_SCOPED, ALL_SERVER_SCOPED, Permissions},
    proto::{GroupInfo, User},
};

// ============================================================
// Constants
// ============================================================

/// All permission flags we expose in the base-perms editor, ordered
/// the way they're rendered. `BANNED` is excluded — it's an internal
/// sentinel set by the server's ban logic, not user-tunable.
const ROOM_PERMS: &[(Permissions, &str, &str)] = &[
    (Permissions::TRAVERSE, "Traverse", "Walk through to children"),
    (Permissions::ENTER, "Enter", "Join this room"),
    (Permissions::SPEAK, "Speak", "Use voice here"),
    (Permissions::TEXT_MESSAGE, "Text", "Send chat messages"),
    (Permissions::SHARE_FILE, "Files", "Upload / share files"),
    (Permissions::MUTE_DEAFEN, "Mute/Deafen", "Mute or deafen others"),
    (Permissions::MOVE_USER, "Move users", "Move users between rooms"),
    (Permissions::MAKE_ROOM, "Make room", "Create sub-rooms"),
    (Permissions::MODIFY_ROOM, "Modify room", "Edit room settings"),
    (Permissions::WRITE, "Write ACL", "Edit this room's ACL"),
];

const SERVER_PERMS: &[(Permissions, &str, &str)] = &[
    (Permissions::KICK, "Kick", "Disconnect a user from the server"),
    (Permissions::BAN, "Ban", "Persist a connection ban by key"),
    (
        Permissions::REGISTER,
        "Register others",
        "Add another user's key to the registry",
    ),
    (Permissions::SELF_REGISTER, "Self register", "Register one's own key"),
    (
        Permissions::MANAGE_ACL,
        "Manage ACLs",
        "Edit groups, members, and the Admin tab",
    ),
    (Permissions::SUDO, "Sudo", "Elevate past the ACL system"),
    (
        Permissions::MANAGE_PARTICIPANTS,
        "Manage participants",
        "Register as a controller for proxied participants (bridges, bots)",
    ),
];

// ============================================================
// State
// ============================================================

#[derive(Debug, Default)]
pub struct AdminState {
    /// At most one group is expanded at a time — matches the accordion
    /// widget's `Option<V>` controlled-open pattern and keeps the tab
    /// readable when there are many groups.
    pub expanded_group: Option<String>,

    /// Search filter applied to both group names and member usernames.
    /// Empty string means "show everything".
    pub search_query: String,

    /// New-group dialog. `None` when closed.
    pub new_group: Option<NewGroupForm>,

    /// Group name pending delete confirmation.
    pub pending_delete: Option<String>,

    /// Open `+ add user` popover, keyed by group name. The query field
    /// lives here so it survives the modal closing → reopening, but is
    /// reset when a different group's popover opens.
    pub add_user_popover: Option<AddUserPopover>,

    /// Open ⋯ row action menu, keyed by group name. We don't carry
    /// any state beyond the open group — the menu items are pure
    /// commands.
    pub action_menu: Option<String>,
}

#[derive(Debug, Default)]
pub struct NewGroupForm {
    pub name: String,
    /// Inline validation message (empty name, duplicate name).
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct AddUserPopover {
    pub group_name: String,
    pub query: String,
}

// ============================================================
// Outcome
// ============================================================

/// Result of routing a single event through the admin tab.
pub enum AdminOutcome {
    /// Event wasn't recognised — caller continues checking other handlers.
    Ignored,
    /// Event was consumed; no commands to fire (popover toggle, expand, etc.).
    Handled,
    /// Event consumed and the App should fire each command in order.
    Dispatch(Vec<Command>),
}

impl AdminOutcome {
    fn one(cmd: Command) -> Self {
        AdminOutcome::Dispatch(vec![cmd])
    }
}

// ============================================================
// Routed-key constants
// ============================================================
//
// Group names appear inline in many keys. They are not validated for
// `:` here — the rumble server treats group names as opaque strings —
// but rumble's existing UI and CLI flow only produces alphanumeric
// names. If that ever breaks we can hex-encode names in keys.

const KEY_SEARCH: &str = "admin:search";
const KEY_NEW_GROUP_OPEN: &str = "admin:newgroup:open";
const KEY_NEW_GROUP_NAME: &str = "admin:newgroup:name";
const KEY_NEW_GROUP_CREATE: &str = "admin:newgroup:create";
const KEY_NEW_GROUP_CANCEL: &str = "admin:newgroup:cancel";
const KEY_NEW_GROUP_DISMISS: &str = "admin:newgroup:dismiss";

const ACCORDION_KEY: &str = "admin:group";

fn key_action_trigger(group: &str) -> String {
    format!("admin:g:{group}:action")
}
fn key_action_delete(group: &str) -> String {
    format!("admin:g:{group}:action:delete")
}
fn key_action_dismiss(group: &str) -> String {
    format!("admin:g:{group}:action:dismiss")
}
fn key_remove_member(group: &str, user_id: u64) -> String {
    format!("admin:g:{group}:remove:{user_id}")
}
fn key_add_user_open(group: &str) -> String {
    format!("admin:g:{group}:adduser:open")
}
fn key_add_user_query(group: &str) -> String {
    format!("admin:g:{group}:adduser:query")
}
fn key_add_user_dismiss(group: &str) -> String {
    format!("admin:g:{group}:adduser:dismiss")
}
fn key_add_user_pick(group: &str, user_id: u64) -> String {
    format!("admin:g:{group}:adduser:pick:{user_id}")
}
fn key_perm_toggle(group: &str, bits: u32) -> String {
    format!("admin:g:{group}:perm:{bits:08x}")
}
fn key_perm_all(group: &str) -> String {
    format!("admin:g:{group}:perm:all")
}
fn key_perm_clear(group: &str) -> String {
    format!("admin:g:{group}:perm:clear")
}
fn key_delete_confirm(group: &str) -> String {
    format!("admin:delete:{group}:confirm")
}
fn key_delete_cancel(group: &str) -> String {
    format!("admin:delete:{group}:cancel")
}
fn key_delete_dismiss(group: &str) -> String {
    format!("admin:delete:{group}:dismiss")
}

/// Inverse of [`key_remove_member`] / [`key_add_user_pick`] /
/// [`key_perm_toggle`]. Returns `(group_name, action, payload)` where
/// payload is the trailing segment (e.g. user id or perm bits in hex)
/// when present.
fn parse_group_route(route: &str) -> Option<(&str, &str, Option<&str>)> {
    let rest = route.strip_prefix("admin:g:")?;
    let (group, tail) = rest.split_once(':')?;
    let (action, payload) = match tail.split_once(':') {
        Some((a, p)) => (a, Some(p)),
        None => (tail, None),
    };
    Some((group, action, payload))
}

// ============================================================
// Public render API
// ============================================================

/// Render the body of the Admin tab — i.e. the content that lives
/// inside the Settings modal's scroll area. Floating layers (popovers
/// and dialogs) are returned separately by [`render_overlays`].
pub fn render(state: &AdminState, app_state: &State, selection: &Selection) -> El {
    let header = render_header(state, selection);
    let groups = render_group_list(state, app_state, selection);

    column([header, groups]).gap(tokens::SPACE_3).width(Size::Fill(1.0))
}

/// Floating layers (action menu, "+ add user" popover, new-group and
/// delete confirmation dialogs). Returned as an `AdminOverlays` so the
/// settings layer composer can decide z-order.
#[derive(Default)]
pub struct AdminOverlays {
    pub action_menu: Option<El>,
    pub add_user_popover: Option<El>,
    pub new_group_dialog: Option<El>,
    pub delete_dialog: Option<El>,
}

impl AdminOverlays {
    pub fn any(&self) -> bool {
        self.action_menu.is_some()
            || self.add_user_popover.is_some()
            || self.new_group_dialog.is_some()
            || self.delete_dialog.is_some()
    }

    /// Flatten the four optional layers into the form `overlays(...)`
    /// expects: a single `Option<El>` per slot, in paint order
    /// (popover and menu below dialogs).
    pub fn into_layers(self) -> [Option<El>; 4] {
        [
            self.action_menu,
            self.add_user_popover,
            self.delete_dialog,
            self.new_group_dialog,
        ]
    }
}

pub fn render_overlays(state: &AdminState, app_state: &State, selection: &Selection) -> AdminOverlays {
    let mut out = AdminOverlays::default();

    if let Some(group) = state.action_menu.as_deref() {
        let is_builtin = app_state
            .group_definitions
            .iter()
            .any(|g| g.name == group && g.is_builtin);
        out.action_menu = Some(render_action_menu(group, is_builtin));
    }
    if let Some(popover) = state.add_user_popover.as_ref() {
        out.add_user_popover = Some(render_add_user_popover(popover, app_state, selection));
    }
    if let Some(form) = state.new_group.as_ref() {
        out.new_group_dialog = Some(render_new_group_dialog(form, selection));
    }
    if let Some(group) = state.pending_delete.as_deref() {
        out.delete_dialog = Some(render_delete_dialog(group));
    }

    out
}

// ============================================================
// Header
// ============================================================

fn render_header(state: &AdminState, selection: &Selection) -> El {
    let intro = paragraph(
        "Named sets of users. Rooms grant permissions to groups; groups also carry a base permission set applied \
         before any room ACL fires. Built-in groups (default, admin) cannot be deleted.",
    )
    .muted()
    .font_size(tokens::TEXT_XS.size);

    let search = text_input_with(
        &state.search_query,
        selection,
        KEY_SEARCH,
        TextInputOpts {
            placeholder: Some("Search groups or users…"),
            ..Default::default()
        },
    )
    .width(Size::Fixed(240.0));

    let new_group_btn = button("+ New group").key(KEY_NEW_GROUP_OPEN).primary();

    let toolbar = row([search, spacer(), new_group_btn])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0));

    column([intro, toolbar]).gap(tokens::SPACE_2).width(Size::Fill(1.0))
}

// ============================================================
// Group list
// ============================================================

fn render_group_list(state: &AdminState, app_state: &State, selection: &Selection) -> El {
    let query = state.search_query.trim().to_lowercase();
    let groups: Vec<&GroupInfo> = filtered_groups(&app_state.group_definitions, &app_state.users, &query);

    if groups.is_empty() {
        return paragraph(if query.is_empty() {
            "No groups defined yet."
        } else {
            "No groups match the search."
        })
        .muted()
        .font_size(tokens::TEXT_XS.size);
    }

    let usage = count_group_usage(app_state);

    let items: Vec<El> = groups
        .iter()
        .map(|g| {
            let open = state.expanded_group.as_deref() == Some(g.name.as_str());
            render_group_card(
                state,
                app_state,
                selection,
                g,
                open,
                *usage.get(g.name.as_str()).unwrap_or(&0),
            )
        })
        .collect();

    accordion(items)
}

fn filtered_groups<'a>(groups: &'a [GroupInfo], users: &[User], query: &str) -> Vec<&'a GroupInfo> {
    if query.is_empty() {
        return groups.iter().collect();
    }
    groups
        .iter()
        .filter(|g| {
            if g.name.to_lowercase().contains(query) {
                return true;
            }
            // Match against member usernames so the search bar can also
            // jump to "which group is alice in".
            users
                .iter()
                .any(|u| u.groups.contains(&g.name) && u.username.to_lowercase().contains(query))
        })
        .collect()
}

/// Build a quick map from group name to number of rooms whose ACL
/// references it. Linear in the room ACL surface, but the room set is
/// small and this only runs on a render frame for the Admin tab.
fn count_group_usage(state: &State) -> std::collections::HashMap<String, usize> {
    let mut out: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for room in &state.rooms {
        for entry in &room.acls {
            *out.entry(entry.group.clone()).or_default() += 1;
        }
    }
    out
}

fn render_group_card(
    state: &AdminState,
    app_state: &State,
    selection: &Selection,
    group: &GroupInfo,
    open: bool,
    used_in_rooms: usize,
) -> El {
    let is_builtin = group.is_builtin;
    let perms = Permissions::from_bits_truncate(group.permissions);
    let members: Vec<&User> = app_state
        .users
        .iter()
        .filter(|u| u.groups.contains(&group.name))
        .collect();

    let header = render_group_header(group, perms, &members, is_builtin, open);

    let mut children: Vec<El> = vec![header];
    if open {
        let body = accordion_content([
            render_members_section(state, app_state, selection, group, is_builtin, &members),
            divider(),
            render_base_perms_section(group, perms, is_builtin),
            text(format!(
                "Used in {used_in_rooms} room{}",
                if used_in_rooms == 1 { "" } else { "s" }
            ))
            .muted()
            .font_size(tokens::TEXT_XS.size),
        ]);
        children.push(body);
    }

    column(children).gap(0.0).width(Size::Fill(1.0)).height(Size::Hug)
}

fn render_group_header(group: &GroupInfo, perms: Permissions, members: &[&User], is_builtin: bool, open: bool) -> El {
    let chevron = if open { "chevron-down" } else { "chevron-right" };

    let mut name_row: Vec<El> = Vec::new();
    if is_builtin {
        name_row.push(badge("built-in"));
    }
    name_row.push(text(&group.name).semibold());

    let count_label = format!("{} member{}", members.len(), if members.len() == 1 { "" } else { "s" });

    let summary = format_permission_summary(perms);

    let action_btn = icon_button("more-horizontal").key(key_action_trigger(&group.name));

    row([
        icon(chevron).icon_size(tokens::ICON_XS).color(tokens::MUTED_FOREGROUND),
        row(name_row).gap(tokens::SPACE_2).align(Align::Center).width(Size::Hug),
        text(count_label).muted().font_size(tokens::TEXT_XS.size),
        spacer(),
        text(summary)
            .muted()
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
        action_btn,
    ])
    .key(accordion_item_key(ACCORDION_KEY, &group.name))
    .focusable()
    .cursor(Cursor::Pointer)
    .fill(tokens::CARD)
    .radius(tokens::RADIUS_SM)
    .gap(tokens::SPACE_3)
    .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1))
    .height(Size::Fixed(40.0))
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

// ============================================================
// Members
// ============================================================

fn render_members_section(
    _state: &AdminState,
    _app_state: &State,
    _selection: &Selection,
    group: &GroupInfo,
    is_builtin: bool,
    members: &[&User],
) -> El {
    // `_state` / `_app_state` / `_selection` are reserved for the
    // upcoming "current user highlighting" + popover wiring; threading
    // them in now keeps the call site stable as those land.
    let mut chips: Vec<El> = members
        .iter()
        .map(|u| {
            let uid = u.user_id.as_ref().map(|i| i.value).unwrap_or(0);
            chip(&u.username, uid, &group.name, is_builtin)
        })
        .collect();

    // The "+ add user" button doubles as the popover trigger.
    if !is_builtin {
        chips.push(button("+ add user").key(key_add_user_open(&group.name)).secondary());
    }

    let members_block = if chips.is_empty() {
        text(if is_builtin {
            "Membership is computed and cannot be edited"
        } else {
            "No members yet"
        })
        .muted()
        .mono()
        .font_size(tokens::TEXT_XS.size)
    } else {
        // wrap the chips in a row that flows — for now use a wrapping
        // row by leaning on stack-as-flow. damascene doesn't have a
        // dedicated flow widget, but a `row` with `Align::Start` and
        // many small children behaves reasonably in this width.
        row(chips)
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0))
    };

    column([section_heading("Members"), members_block])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
}

fn chip(username: &str, user_id: u64, group: &str, is_builtin: bool) -> El {
    let initial = username
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .next()
        .unwrap_or('?');
    let mut parts: Vec<El> = vec![
        avatar_initials(initial.to_string()),
        text(username).font_size(tokens::TEXT_XS.size),
    ];
    if !is_builtin {
        // Match the chip's pill radius so the × button's fill follows
        // the parent curve instead of stamping a flat corner into it.
        parts.push(
            button("×")
                .key(key_remove_member(group, user_id))
                .secondary()
                .radius(tokens::RADIUS_PILL),
        );
    }
    row(parts)
        .gap(tokens::SPACE_1)
        .align(Align::Center)
        .fill(tokens::MUTED)
        .radius(tokens::RADIUS_PILL)
        .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
        .width(Size::Hug)
}

// ============================================================
// Base permissions
// ============================================================

fn render_base_perms_section(group: &GroupInfo, perms: Permissions, _is_builtin: bool) -> El {
    let all_bits = (ALL_ROOM_SCOPED | ALL_SERVER_SCOPED).bits();
    let everything_on = (perms.bits() & all_bits) == all_bits;

    let header_actions = row([button(if everything_on { "Clear all" } else { "+ all" })
        .key(if everything_on {
            key_perm_clear(&group.name)
        } else {
            key_perm_all(&group.name)
        })
        .secondary()])
    .gap(tokens::SPACE_2);

    let title = row([section_heading("Base permissions"), spacer(), header_actions])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0));

    let room_rows: Vec<El> = ROOM_PERMS
        .iter()
        .map(|(p, label, desc)| perm_row(&group.name, *p, label, desc, perms.contains(*p)))
        .collect();
    let server_rows: Vec<El> = SERVER_PERMS
        .iter()
        .map(|(p, label, desc)| perm_row(&group.name, *p, label, desc, perms.contains(*p)))
        .collect();

    column([
        title,
        text("Room-scoped").muted().font_size(tokens::TEXT_XS.size),
        column(room_rows).gap(tokens::SPACE_1).width(Size::Fill(1.0)),
        text("Server-scoped").muted().font_size(tokens::TEXT_XS.size),
        column(server_rows).gap(tokens::SPACE_1).width(Size::Fill(1.0)),
    ])
    .gap(tokens::SPACE_2)
    .width(Size::Fill(1.0))
}

fn perm_row(group: &str, perm: Permissions, label: &str, description: &str, enabled: bool) -> El {
    row([
        column([
            text(label).font_size(tokens::TEXT_SM.size),
            text(description).muted().font_size(tokens::TEXT_XS.size),
        ])
        .gap(0.0)
        .width(Size::Fill(1.0)),
        switch(enabled).key(key_perm_toggle(group, perm.bits())),
    ])
    .gap(tokens::SPACE_3)
    .align(Align::Center)
    .width(Size::Fill(1.0))
}

// ============================================================
// Overlays — action menu, add-user popover, dialogs
// ============================================================

fn render_action_menu(group: &str, is_builtin: bool) -> El {
    let mut items: Vec<El> = vec![
        text(format!("Group: {group}"))
            .semibold()
            .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1)),
        divider(),
    ];
    if is_builtin {
        items.push(
            text("Built-in groups cannot be deleted")
                .muted()
                .font_size(tokens::TEXT_XS.size)
                .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1)),
        );
    } else {
        items.push(menu_item("Delete group…").key(key_action_delete(group)));
    }
    dropdown(format!("admin:g:{group}:menu"), key_action_trigger(group), items)
}

fn render_add_user_popover(popover_state: &AddUserPopover, app_state: &State, selection: &Selection) -> El {
    let group = &popover_state.group_name;
    let query = popover_state.query.trim().to_lowercase();
    let mut candidates: Vec<&User> = app_state
        .users
        .iter()
        .filter(|u| !u.groups.contains(group))
        .filter(|u| query.is_empty() || u.username.to_lowercase().contains(&query))
        .collect();
    candidates.sort_by_key(|u| u.username.to_lowercase());

    let search = text_input_with(
        &popover_state.query,
        selection,
        &key_add_user_query(group),
        TextInputOpts {
            placeholder: Some("Filter users…"),
            ..Default::default()
        },
    )
    .width(Size::Fill(1.0));

    let list: Vec<El> = if candidates.is_empty() {
        vec![
            text(if app_state.users.is_empty() {
                "No connected users"
            } else if query.is_empty() {
                "Everyone is already a member"
            } else {
                "No match"
            })
            .muted()
            .font_size(tokens::TEXT_XS.size)
            .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_2)),
        ]
    } else {
        candidates
            .iter()
            .map(|u| {
                let uid = u.user_id.as_ref().map(|i| i.value).unwrap_or(0);
                menu_item(u.username.clone()).key(key_add_user_pick(group, uid))
            })
            .collect()
    };

    let panel = popover_panel(
        std::iter::once(search)
            .chain(std::iter::once(divider()))
            .chain(list)
            .collect::<Vec<_>>(),
    );
    popover(
        format!("admin:g:{group}:adduser"),
        Anchor::below_key(key_add_user_open(group)),
        panel,
    )
}

fn render_new_group_dialog(form: &NewGroupForm, selection: &Selection) -> El {
    let mut body: Vec<El> = vec![
        paragraph("Pick a name. New groups start with no permissions and no members; edit those after it's created.")
            .muted()
            .font_size(tokens::TEXT_XS.size),
        field_row(
            "Name",
            text_input_with(
                &form.name,
                selection,
                KEY_NEW_GROUP_NAME,
                TextInputOpts {
                    placeholder: Some("e.g. moderators"),
                    ..Default::default()
                },
            )
            .width(Size::Fill(1.0)),
        ),
    ];
    if let Some(err) = form.error.as_deref() {
        body.push(alert([alert_description(err)]).destructive());
    }
    body.push(divider());
    body.push(
        row([
            button("Cancel").key(KEY_NEW_GROUP_CANCEL),
            spacer(),
            button("Create group").key(KEY_NEW_GROUP_CREATE).primary(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center),
    );

    overlay([
        scrim(KEY_NEW_GROUP_DISMISS),
        modal_panel("New group", body).block_pointer(),
    ])
}

fn render_delete_dialog(group: &str) -> El {
    overlay([
        scrim(key_delete_dismiss(group)),
        modal_panel(
            "Delete group",
            [
                paragraph(format!(
                    "Permanently delete group \"{group}\"? Any ACL entries referencing it will be left dangling until \
                     manually cleaned up."
                ))
                .text_color(tokens::WARNING),
                divider(),
                row([
                    button("Cancel").key(key_delete_cancel(group)),
                    spacer(),
                    button("Delete").key(key_delete_confirm(group)).destructive(),
                ])
                .gap(tokens::SPACE_2)
                .width(Size::Fill(1.0))
                .align(Align::Center),
            ],
        )
        .block_pointer(),
    ])
}

// ============================================================
// Event handling
// ============================================================

/// Route a single event into the admin tab. The caller drains the
/// outcome and dispatches commands; popovers / dialogs are opened by
/// mutating `state` and remain open until a dismiss event arrives.
pub fn handle_event(
    state: &mut AdminState,
    event: &UiEvent,
    app_state: &State,
    selection: &mut Selection,
) -> AdminOutcome {
    // Text inputs route their own events; classify them first so
    // typed characters don't fall through to other handlers.
    if let Some(out) = handle_search_event(state, event, selection) {
        return out;
    }
    if let Some(out) = handle_new_group_event(state, event, app_state, selection) {
        return out;
    }
    if let Some(out) = handle_add_user_query_event(state, event, selection) {
        return out;
    }
    if let Some(out) = handle_delete_dialog_event(state, event) {
        return out;
    }
    if let Some(out) = handle_action_menu_event(state, event) {
        return out;
    }

    // Accordion toggle.
    if damascene_core::widgets::accordion::apply_event(&mut state.expanded_group, event, ACCORDION_KEY, |raw| {
        Some(raw.to_string())
    }) {
        return AdminOutcome::Handled;
    }

    // Click handlers for everything else.
    if !matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
        return AdminOutcome::Ignored;
    }
    let Some(route) = event.route() else {
        return AdminOutcome::Ignored;
    };

    if route == KEY_NEW_GROUP_OPEN {
        state.new_group = Some(NewGroupForm::default());
        return AdminOutcome::Handled;
    }

    if let Some((group, action, payload)) = parse_group_route(route) {
        // Snapshot group name as owned because we mutate state.
        let group = group.to_string();
        return handle_group_action(state, app_state, &group, action, payload);
    }

    AdminOutcome::Ignored
}

fn handle_search_event(state: &mut AdminState, event: &UiEvent, selection: &mut Selection) -> Option<AdminOutcome> {
    if damascene_core::widgets::text_input::apply_event(&mut state.search_query, selection, KEY_SEARCH, event) {
        Some(AdminOutcome::Handled)
    } else {
        None
    }
}

fn handle_new_group_event(
    state: &mut AdminState,
    event: &UiEvent,
    app_state: &State,
    selection: &mut Selection,
) -> Option<AdminOutcome> {
    state.new_group.as_ref()?;

    if event.is_route(KEY_NEW_GROUP_DISMISS) && event.kind == UiEventKind::Click {
        state.new_group = None;
        return Some(AdminOutcome::Handled);
    }
    if event.kind == UiEventKind::Escape {
        state.new_group = None;
        return Some(AdminOutcome::Handled);
    }

    if let Some(form) = state.new_group.as_mut()
        && damascene_core::widgets::text_input::apply_event(&mut form.name, selection, KEY_NEW_GROUP_NAME, event)
    {
        form.error = None;
        return Some(AdminOutcome::Handled);
    }

    if event.is_click_or_activate(KEY_NEW_GROUP_CANCEL) {
        state.new_group = None;
        return Some(AdminOutcome::Handled);
    }

    if event.is_click_or_activate(KEY_NEW_GROUP_CREATE) {
        let Some(form) = state.new_group.as_mut() else {
            return Some(AdminOutcome::Handled);
        };
        let name = form.name.trim().to_string();
        if name.is_empty() {
            form.error = Some("Name cannot be empty".into());
            return Some(AdminOutcome::Handled);
        }
        if app_state.group_definitions.iter().any(|g| g.name == name) {
            form.error = Some("A group with that name already exists".into());
            return Some(AdminOutcome::Handled);
        }
        state.new_group = None;
        state.expanded_group = Some(name.clone());
        return Some(AdminOutcome::one(Command::CreateGroup { name, permissions: 0 }));
    }

    None
}

fn handle_add_user_query_event(
    state: &mut AdminState,
    event: &UiEvent,
    selection: &mut Selection,
) -> Option<AdminOutcome> {
    let popover = state.add_user_popover.as_mut()?;
    let key = key_add_user_query(&popover.group_name);
    if damascene_core::widgets::text_input::apply_event(&mut popover.query, selection, &key, event) {
        return Some(AdminOutcome::Handled);
    }
    let dismiss_key = key_add_user_dismiss(&popover.group_name);
    if event.is_route(&dismiss_key) && event.kind == UiEventKind::Click {
        state.add_user_popover = None;
        return Some(AdminOutcome::Handled);
    }
    if event.kind == UiEventKind::Escape {
        state.add_user_popover = None;
        return Some(AdminOutcome::Handled);
    }
    None
}

fn handle_delete_dialog_event(state: &mut AdminState, event: &UiEvent) -> Option<AdminOutcome> {
    let group = state.pending_delete.clone()?;
    let dismiss_key = key_delete_dismiss(&group);
    let cancel_key = key_delete_cancel(&group);
    let confirm_key = key_delete_confirm(&group);

    if event.kind == UiEventKind::Escape || event.is_click_or_activate(&cancel_key) {
        state.pending_delete = None;
        return Some(AdminOutcome::Handled);
    }
    if event.is_route(&dismiss_key) && event.kind == UiEventKind::Click {
        state.pending_delete = None;
        return Some(AdminOutcome::Handled);
    }
    if event.is_click_or_activate(&confirm_key) {
        state.pending_delete = None;
        return Some(AdminOutcome::one(Command::DeleteGroup { name: group }));
    }
    None
}

fn handle_action_menu_event(state: &mut AdminState, event: &UiEvent) -> Option<AdminOutcome> {
    let group = state.action_menu.clone()?;
    let dismiss_key = key_action_dismiss(&group);
    if event.is_route(&dismiss_key) && event.kind == UiEventKind::Click {
        state.action_menu = None;
        return Some(AdminOutcome::Handled);
    }
    if event.kind == UiEventKind::Escape {
        state.action_menu = None;
        return Some(AdminOutcome::Handled);
    }
    if !matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
        return None;
    }
    let route = event.route()?;
    let delete_key = key_action_delete(&group);
    if route == delete_key {
        state.action_menu = None;
        state.pending_delete = Some(group);
        return Some(AdminOutcome::Handled);
    }
    None
}

fn handle_group_action(
    state: &mut AdminState,
    app_state: &State,
    group: &str,
    action: &str,
    payload: Option<&str>,
) -> AdminOutcome {
    let group_info = app_state.group_definitions.iter().find(|g| g.name == group);
    match action {
        "action" => {
            // ⋯ trigger toggles the row action menu.
            state.action_menu = match state.action_menu.as_deref() {
                Some(g) if g == group => None,
                _ => Some(group.to_string()),
            };
            AdminOutcome::Handled
        }
        "remove" => {
            let Some(uid_str) = payload else {
                return AdminOutcome::Handled;
            };
            let Ok(uid) = uid_str.parse::<u64>() else {
                return AdminOutcome::Handled;
            };
            AdminOutcome::one(Command::SetUserGroup {
                target_user_id: uid,
                group: group.to_string(),
                add: false,
                expires_at: 0,
            })
        }
        "adduser" => match payload {
            Some("open") => {
                state.add_user_popover = Some(AddUserPopover {
                    group_name: group.to_string(),
                    query: String::new(),
                });
                AdminOutcome::Handled
            }
            Some(rest) => {
                if let Some(uid_str) = rest.strip_prefix("pick:")
                    && let Ok(uid) = uid_str.parse::<u64>()
                {
                    state.add_user_popover = None;
                    return AdminOutcome::one(Command::SetUserGroup {
                        target_user_id: uid,
                        group: group.to_string(),
                        add: true,
                        expires_at: 0,
                    });
                }
                AdminOutcome::Handled
            }
            None => AdminOutcome::Handled,
        },
        "perm" => {
            let Some(g) = group_info else {
                return AdminOutcome::Handled;
            };
            let current = g.permissions;
            let new_bits = match payload {
                Some("all") => (ALL_ROOM_SCOPED | ALL_SERVER_SCOPED).bits(),
                Some("clear") => 0,
                Some(hex) => {
                    let Ok(bit) = u32::from_str_radix(hex, 16) else {
                        return AdminOutcome::Handled;
                    };
                    current ^ bit
                }
                None => return AdminOutcome::Handled,
            };
            if new_bits == current {
                AdminOutcome::Handled
            } else {
                AdminOutcome::one(Command::ModifyGroup {
                    name: group.to_string(),
                    permissions: new_bits,
                })
            }
        }
        _ => AdminOutcome::Ignored,
    }
}

// ============================================================
// Permission summary formatting
// ============================================================

/// All flags we display, in the order the summary lists them.
const SUMMARY_FLAGS: &[(Permissions, &str)] = &[
    (Permissions::TRAVERSE, "Traverse"),
    (Permissions::ENTER, "Enter"),
    (Permissions::SPEAK, "Speak"),
    (Permissions::TEXT_MESSAGE, "Text"),
    (Permissions::SHARE_FILE, "Files"),
    (Permissions::MUTE_DEAFEN, "Mute/Deafen"),
    (Permissions::MOVE_USER, "Move"),
    (Permissions::MAKE_ROOM, "Make room"),
    (Permissions::MODIFY_ROOM, "Modify room"),
    (Permissions::WRITE, "Write ACL"),
    (Permissions::KICK, "Kick"),
    (Permissions::BAN, "Ban"),
    (Permissions::REGISTER, "Register"),
    (Permissions::SELF_REGISTER, "Self register"),
    (Permissions::MANAGE_ACL, "Manage ACL"),
    (Permissions::SUDO, "Sudo"),
    (Permissions::MANAGE_PARTICIPANTS, "Manage participants"),
];

/// Short permission summary used in the accordion header. Renders
/// `+ all` when every (non-BANNED) bit is set, else a comma-separated
/// list of the bits, truncated to four flags + "N permissions" once
/// the list grows past that.
pub fn format_permission_summary(perms: Permissions) -> String {
    let mask = ALL_ROOM_SCOPED | ALL_SERVER_SCOPED;
    if (perms & mask) == mask {
        return "+ all".to_string();
    }
    let names: Vec<&str> = SUMMARY_FLAGS
        .iter()
        .filter(|(p, _)| perms.contains(*p))
        .map(|(_, n)| *n)
        .collect();
    if names.is_empty() {
        "— no base perms".to_string()
    } else if names.len() <= 4 {
        names.join(", ")
    } else {
        format!("{} permissions", names.len())
    }
}

// ============================================================
// Local view helpers
// ============================================================

fn section_heading(label: impl Into<String>) -> El {
    text(label).semibold().font_size(tokens::TEXT_SM.size)
}
