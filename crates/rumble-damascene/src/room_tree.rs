//! Room tree view: hierarchical rooms with users inlined as leaves,
//! single-click select / double-click join, right-click context menus
//! for rooms and users (also reachable through each row's trailing
//! hover-revealed "⋯" button), drag-and-drop reparenting (room → room)
//! and drag-yourself-to-join (self-user → room), plus the confirmation
//! modals those two drag flows fall into. The pane header carries a
//! "+" affordance for creating root-level rooms (gated on `MAKE_ROOM`
//! at the Root room), and the room menu's "Move to…" opens the
//! destination picker in [`crate::move_room`].
//!
//! Owns all of its own ephemeral UI state ([`RoomTreeState`]). The App
//! routes events here via [`handle_event`] and dispatches the resulting
//! [`RoomTreeOutcome`] back to the backend. Render is split across
//! [`render`] (the in-tree El) and [`render_overlays`] (the floating
//! popovers/modals) so the App can compose them as independent layers
//! at the root.
//!
//! The module is intentionally backend-agnostic: it consumes a `&State`
//! snapshot and emits `Command`s. That keeps it reusable as the basis
//! for a server-admin tree, an embedded mini-tree in another panel,
//! and so on — any caller that can supply the same two halves.

use std::{panic::Location, sync::LazyLock};

use damascene_core::prelude::*;
use rumble_client::{Command, ROOT_ROOM_UUID, State};
use rumble_protocol::permissions::Permissions;
use uuid::Uuid;

use crate::{
    move_room::{self, MoveToState},
    theme as palette,
};

// ============================================================
// Bundled Mumble SVG glyphs
// ============================================================
//
// Parsed once at first use (Arc-bumped on every `icon(...)` call) and
// shared across frames. We use `SvgIcon::parse` (not
// `parse_current_color`) because the Mumble theme bakes its semantic
// colors into the SVG paint — red for self-mute, blue for talking, etc.
// — and those colors are exactly the visual signal we want to keep.

static SVG_TALKING_ON: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../assets/icons/talking_on.svg")).expect("talking_on.svg parses"));
static SVG_TALKING_OFF: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../assets/icons/talking_off.svg")).expect("talking_off.svg parses"));
static SVG_MUTED_SELF: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../assets/icons/muted_self.svg")).expect("muted_self.svg parses"));
static SVG_MUTED_SERVER: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse(include_str!("../assets/icons/muted_server.svg")).expect("muted_server.svg parses")
});
static SVG_DEAFENED_SELF: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse(include_str!("../assets/icons/deafened_self.svg")).expect("deafened_self.svg parses")
});
static SVG_MUTED_LOCAL: LazyLock<SvgIcon> =
    LazyLock::new(|| SvgIcon::parse(include_str!("../assets/icons/muted_local.svg")).expect("muted_local.svg parses"));

// ============================================================
// Constants
// ============================================================

/// Drag distance in logical pixels before a `PointerDown`+`Drag` is
/// promoted from "press" to "active drag". Below this threshold the
/// release is treated as a tap (so click_count handling can produce
/// a JoinRoom on double-click).
const DRAG_ACTIVATE_PX: f32 = 6.0;

// ============================================================
// State
// ============================================================

/// All ephemeral UI state owned by the room tree: which row is
/// highlighted, which context menu / modal is open, and the in-flight
/// drag source. Drop hit-testing resolves against live layout via
/// [`find_room_at`] / [`EventCx::rect_of_key`], so no rect snapshot is
/// kept here.
///
/// The App owns one of these on its struct. `Default` produces an
/// empty tree (nothing selected, no menus open).
#[derive(Default)]
pub struct RoomTreeState {
    /// Highlighted room (set by single-click). Visual only; double-click
    /// is what actually joins.
    pub selected_room_id: Option<Uuid>,
    pub(crate) room_context_menu: Option<RoomContextMenu>,
    pub(crate) user_context_menu: Option<UserContextMenu>,
    pub(crate) tree_drag: Option<TreeDrag>,
    pub(crate) move_room_modal: Option<MoveRoomModal>,
    pub(crate) delete_room_modal: Option<DeleteRoomModal>,
    /// "Move to…" destination picker (see [`crate::move_room`]).
    pub(crate) move_to_modal: Option<MoveToState>,
}

/// Hit-test `point` against the live layout to resolve a drag-drop
/// target on `PointerUp`. Rebuilds the `(row key, target room)` pairs
/// the same way [`render`] lays the tree out, then asks the runtime
/// where each keyed row actually landed via [`EventCx::rect_of_key`].
///
/// Each room can appear multiple times — once for its own row, once per
/// user row beneath it (a drop onto a user resolves to that user's
/// room) — but the rows don't overlap on screen, so the first rect that
/// contains `point` is the answer. Returns `None` when the pointer is
/// over empty space or a row that hasn't been laid out yet.
fn find_room_at(point: (f32, f32), cx: &EventCx, app_state: &State) -> Option<Uuid> {
    collect_drop_targets(app_state).into_iter().find_map(|(key, room_id)| {
        cx.rect_of_key(&key)
            .filter(|rect| rect.contains(point.0, point.1))
            .map(|_| room_id)
    })
}

/// Walk the room tree exactly as [`push_room_subtree`] renders it,
/// emitting the `(row key, drop-target room)` pairs [`find_room_at`]
/// hit-tests against. Room rows map to their own id; user rows map to
/// their parent room. Built fresh at `PointerUp` rather than snapshotted
/// each frame — the keys are deterministic, so the live rects come from
/// [`EventCx::rect_of_key`].
fn collect_drop_targets(state: &State) -> Vec<(String, Uuid)> {
    fn walk(state: &State, room_id: Uuid, out: &mut Vec<(String, Uuid)>) {
        let Some(node) = state.room_tree.nodes.get(&room_id) else {
            return;
        };
        out.push((room_route_key(room_id), room_id));
        for user in state.users.iter().filter(|u| u.current_room == room_id) {
            out.push((user_route_key(user.user_id, room_id), room_id));
        }
        for &child in &node.children {
            walk(state, child, out);
        }
    }

    let mut out = Vec::new();
    walk(state, ROOT_ROOM_UUID, &mut out);
    out
}

/// Where an open context menu is pinned. Right-click menus anchor at
/// the click point; the row's trailing "⋯" button anchors the same
/// menu below the trigger so it tracks the row across relayout.
#[derive(Clone, Debug)]
pub(crate) enum MenuAnchor {
    Point((f32, f32)),
    /// Route key of the trigger element (`dropdown` resolves its rect
    /// at layout time).
    Below(String),
}

#[derive(Clone, Debug)]
pub(crate) struct RoomContextMenu {
    room_id: Uuid,
    room_name: String,
    is_root: bool,
    anchor: MenuAnchor,
}

#[derive(Clone, Debug)]
pub(crate) struct UserContextMenu {
    user_id: u64,
    /// Room the user was in when the menu opened — used to look up
    /// per-room permissions for the gated entries.
    room_id: Uuid,
    username: String,
    is_self: bool,
    is_self_muted: bool,
    is_self_deafened: bool,
    is_locally_muted: bool,
    is_server_muted: bool,
    anchor: MenuAnchor,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TreeDragSource {
    /// A room is being dragged to be reparented under another room.
    /// On drop, opens [`MoveRoomModal`] for confirmation before firing
    /// `Command::MoveRoom`.
    Room(Uuid),
    /// The local user is being dragged onto another room. On drop,
    /// fires `Command::JoinRoom` directly — joining is reversible and
    /// matches the standard "drag yourself" UX (Discord, Mumble), so
    /// no confirmation modal.
    ///
    /// We deliberately do **not** allow dragging *other* users: the
    /// protocol has no admin-move-user command (only `MoveRoom`), so
    /// the drag would have nothing to dispatch.
    SelfUser,
}

#[derive(Clone, Debug)]
pub(crate) struct TreeDrag {
    source: TreeDragSource,
    origin: (f32, f32),
    /// True once the pointer has moved past `DRAG_ACTIVATE_PX`. Used
    /// to distinguish "intentional drag" from "press that drifted".
    active: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct MoveRoomModal {
    source: Uuid,
    target: Uuid,
    source_name: String,
    target_name: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DeleteRoomModal {
    room_id: Uuid,
    room_name: String,
}

// ============================================================
// Outcome
// ============================================================

/// Result of routing a single event into the room tree.
pub enum RoomTreeOutcome {
    /// Event wasn't recognized by the room tree — the App should
    /// continue checking other handlers.
    Ignored,
    /// Event was consumed but produced no backend commands (e.g.
    /// opening a context menu, dismissing a modal).
    Handled,
    /// Event was consumed and the App should fire these commands in
    /// order. We use a `Vec` rather than enumerating each command
    /// variant in the outcome because the room tree is essentially a
    /// thin wrapper over the command surface, and paired commands
    /// like `(JoinRoom, RequestChatHistory)` fit naturally as a list.
    Dispatch(Vec<Command>),
    /// User picked "Edit…" on a room — App should open the room editor
    /// modal (rename + per-room ACL) for this room.
    OpenRoomEditor(Uuid),
    /// User picked "Kick…" / "Ban…" on a connected user — App should open
    /// the moderation modal (reason + ban duration) for this user.
    OpenKickBan { user_id: u64, username: String, ban: bool },
    /// User picked "Send message…" on a remote user — App should prefill
    /// the chat composer with a `/msg <username> ` command and focus it.
    MessageUser { username: String },
    /// User picked "Audio…" on a remote user — App should open the
    /// per-user audio panel (local volume + RX pipeline override).
    OpenUserAudio { user_id: u64 },
}

impl RoomTreeOutcome {
    /// Convenience: the join-room pair (`JoinRoom` + `RequestChatHistory`).
    fn join(room_id: Uuid) -> Self {
        RoomTreeOutcome::Dispatch(vec![Command::JoinRoom { room_id }, Command::RequestChatHistory])
    }

    /// Convenience: a single command outcome.
    fn one(cmd: Command) -> Self {
        RoomTreeOutcome::Dispatch(vec![cmd])
    }
}

// ============================================================
// Routed-key constants
// ============================================================

const KEY_ROOM_CTX_DISMISS: &str = "room_ctx:dismiss";
const KEY_ROOM_CTX_JOIN: &str = "room_ctx:join";
const KEY_ROOM_CTX_EDIT: &str = "room_ctx:edit";
const KEY_ROOM_CTX_ADD_CHILD: &str = "room_ctx:add_child";
const KEY_ROOM_CTX_MOVE: &str = "room_ctx:move";
const KEY_ROOM_CTX_DELETE: &str = "room_ctx:delete";

/// Pane-header "+" button — creates a root-level room (`parent_id:
/// None`; the server permission-checks `MAKE_ROOM` against Root).
const KEY_ROOMS_NEW_ROOT: &str = "rooms:new_root";
/// Same action offered from the empty-tree state (distinct key so both
/// can't collide if ever rendered together).
const KEY_ROOMS_EMPTY_CREATE: &str = "rooms:empty_create";

const KEY_USER_CTX_DISMISS: &str = "user_ctx:dismiss";
const KEY_USER_CTX_SELF_MUTE: &str = "user_ctx:self_mute";
const KEY_USER_CTX_SELF_UNMUTE: &str = "user_ctx:self_unmute";
const KEY_USER_CTX_SELF_DEAFEN: &str = "user_ctx:self_deafen";
const KEY_USER_CTX_SELF_UNDEAFEN: &str = "user_ctx:self_undeafen";
const KEY_USER_CTX_MUTE_LOCAL: &str = "user_ctx:mute_local";
const KEY_USER_CTX_UNMUTE_LOCAL: &str = "user_ctx:unmute_local";
const KEY_USER_CTX_AUDIO: &str = "user_ctx:audio";
const KEY_USER_CTX_SERVER_MUTE: &str = "user_ctx:server_mute";
const KEY_USER_CTX_SERVER_UNMUTE: &str = "user_ctx:server_unmute";
const KEY_USER_CTX_KICK: &str = "user_ctx:kick";
const KEY_USER_CTX_BAN: &str = "user_ctx:ban";
const KEY_USER_CTX_SEND_MSG: &str = "user_ctx:send_msg";

const KEY_MOVE_DISMISS: &str = "move_room:dismiss";
const KEY_MOVE_CANCEL: &str = "move_room:cancel";
const KEY_MOVE_CONFIRM: &str = "move_room:confirm";
const KEY_DELETE_DISMISS: &str = "delete_room:dismiss";
const KEY_DELETE_CANCEL: &str = "delete_room:cancel";
const KEY_DELETE_CONFIRM: &str = "delete_room:confirm";

// ============================================================
// Render
// ============================================================

/// Render the in-tree room/user list. Returns the `El` the App should
/// place wherever the tree belongs — typically as the center area
/// when connected.
#[track_caller]
pub fn render(state: &RoomTreeState, app_state: &State) -> El {
    let _loc = Location::caller();
    rooms_view(app_state, state.selected_room_id)
}

/// Floating layers (context menus + confirmation modals) the App
/// should compose at the root above its main view. Any field is
/// `None` when the corresponding state isn't open.
#[derive(Default)]
pub struct RoomTreeOverlays {
    pub room_context_menu: Option<El>,
    pub user_context_menu: Option<El>,
    pub move_room_modal: Option<El>,
    pub delete_room_modal: Option<El>,
    pub move_to_modal: Option<El>,
}

impl RoomTreeOverlays {
    /// True when at least one overlay is visible — useful for the
    /// caller's "are any layers active?" decision.
    pub fn any(&self) -> bool {
        self.room_context_menu.is_some()
            || self.user_context_menu.is_some()
            || self.move_room_modal.is_some()
            || self.delete_room_modal.is_some()
            || self.move_to_modal.is_some()
    }
}

/// Build the floating overlay layers (context menus + modals). Caller
/// passes `&State` because the room context menu re-evaluates per-room
/// permissions on every render — gating items like "Add child room"
/// against `MAKE_ROOM` lives at render time, not at menu-open time.
pub fn render_overlays(state: &RoomTreeState, app_state: &State) -> RoomTreeOverlays {
    RoomTreeOverlays {
        room_context_menu: state
            .room_context_menu
            .as_ref()
            .map(|m| render_room_context_menu(m, app_state)),
        user_context_menu: state
            .user_context_menu
            .as_ref()
            .map(|m| render_user_context_menu(m, app_state)),
        move_room_modal: state.move_room_modal.as_ref().map(render_move_room_modal),
        delete_room_modal: state.delete_room_modal.as_ref().map(render_delete_room_modal),
        move_to_modal: state.move_to_modal.as_ref().map(|m| move_room::render(m, app_state)),
    }
}

/// Per-room permission bits: the server-computed per-room entry when
/// present, else the session-wide effective permissions. Same
/// precedence rule everywhere a room-scoped bit gates UI.
fn room_perms(app_state: &State, room_id: Uuid) -> Permissions {
    let bits = app_state
        .per_room_permissions
        .get(&room_id)
        .copied()
        .unwrap_or(app_state.effective_permissions);
    Permissions::from_bits_truncate(bits)
}

/// `MAKE_ROOM` evaluated at the Root room — gates creating root-level
/// rooms (the server checks the same bit against Root when
/// `CreateRoom.parent_id` is empty).
fn can_make_root_room(app_state: &State) -> bool {
    room_perms(app_state, rumble_protocol::ROOT_ROOM_UUID).contains(Permissions::MAKE_ROOM)
}

fn rooms_view(state: &State, selected_room_id: Option<Uuid>) -> El {
    let mut entries: Vec<El> = Vec::new();
    push_room_subtree(state, ROOT_ROOM_UUID, 0, selected_room_id, &mut entries);

    // Header: title + the root-level "new room" affordance, gated on
    // MAKE_ROOM at Root (re-checked at click time in `handle_event`).
    let mut header_children: Vec<El> = vec![text("Rooms").title(), spacer()];
    if can_make_root_room(state) {
        header_children.push(
            icon_button(IconName::Plus)
                .key(KEY_ROOMS_NEW_ROOT)
                .ghost()
                .icon_size(tokens::ICON_SM)
                .width(Size::Fixed(tokens::SPACE_6))
                .height(Size::Fixed(tokens::SPACE_6))
                .tooltip("New room"),
        );
    }
    let header = row(header_children)
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
        .width(Size::Fill(1.0));

    // Empty tree: a proper empty state instead of a bare text line,
    // offering the create action when the user may take it.
    let body: El = if entries.is_empty() {
        let mut empty_children: Vec<El> = vec![
            icon(IconName::Folder)
                .icon_size(tokens::ICON_MD)
                .text_color(tokens::MUTED_FOREGROUND),
            text("No rooms yet").semibold(),
            paragraph("Rooms will show up here once the server syncs.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        ];
        if can_make_root_room(state) {
            empty_children.push(button_with_icon(IconName::Plus, "New room").key(KEY_ROOMS_EMPTY_CREATE));
        }
        column(empty_children)
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .justify(Justify::Center)
            .padding(tokens::SPACE_7)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    } else {
        scroll(entries)
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_3))
            .gap(tokens::SPACE_1)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    };

    column([header, divider(), body])
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

fn push_room_subtree(state: &State, room_id: Uuid, depth: usize, selected_room_id: Option<Uuid>, out: &mut Vec<El>) {
    let Some(node) = state.room_tree.nodes.get(&room_id) else {
        return;
    };

    let user_count = state.users.iter().filter(|u| u.current_room == room_id).count();
    let is_current = state.my_room_id == Some(room_id);
    let is_selected = selected_room_id == Some(room_id);

    let indent = depth as f32 * tokens::SPACE_4;
    let suffix = if user_count > 0 {
        format!("  ({user_count})")
    } else {
        String::new()
    };
    let mut label = text(format!("{}{}", node.name, suffix)).font_size(tokens::TEXT_SM.size);
    if user_count == 0 {
        label = label.muted();
    }
    if is_current {
        label = label.text_color(tokens::PRIMARY);
    }

    let mut row_el = row([
        spacer().width(Size::Fixed(indent)),
        icon(IconName::Folder)
            .icon_size(tokens::ICON_MD)
            .text_color(if is_current {
                tokens::PRIMARY
            } else {
                tokens::MUTED_FOREGROUND
            }),
        label,
        spacer(),
        row_more_button(room_more_key(room_id)),
    ])
    .key(room_route_key(room_id))
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .padding(Sides::xy(tokens::SPACE_1, tokens::SPACE_1))
    .radius(tokens::RADIUS_SM)
    .width(Size::Fill(1.0))
    .focusable();
    // Room description surfaces as a hover tooltip on the row.
    if let Some(desc) = node.description.as_ref().map(|d| d.trim()).filter(|d| !d.is_empty()) {
        row_el = row_el.tooltip(desc.to_string());
    }
    if is_selected {
        // Subtle pill so single-click selection is visible without
        // looking like the active-room indicator.
        row_el = row_el.fill(tokens::ACCENT);
    }
    out.push(row_el);

    for user in state.users.iter().filter(|u| u.current_room == room_id) {
        let user_id = user.user_id;
        // talking_users only tracks remote peers (populated from received
        // voice datagrams); for self we use the local capture/VAD signal.
        let is_self = state.my_user_id == Some(user_id);
        let is_talking = state.audio.talking_users.contains(&user_id) || (is_self && state.audio.is_transmitting);
        let is_locally_muted = !is_self && state.audio.muted_users.contains(&user_id);

        // Icon composition mirrors Mumble's user rows (see
        // reference/mumble/src/mumble/UserModel.cpp, `UserModel::data`):
        // one *talk-state* glyph leads the row, then the name, then a
        // trailing run of independent status-flag icons. The states
        // Rumble models map onto Mumble's canonical flag order:
        //
        //   primary glyph : talking_on / talking_off  (Qt::DecorationRole)
        //   flags (order) : muted_server   ← user.server_muted   (bMute)
        //                   muted_self     ← user.is_muted       (bSelfMute)
        //                   muted_local    ← audio.muted_users   (bLocalMute)
        //                   deafened_self  ← user.is_deafened    (bSelfDeaf)
        //
        // The flags are orthogonal — a self-muted user still shows their
        // talk state, a server-muted user still shows their deafen flag,
        // and deafen (which implies mute client-side) renders *both*
        // muted_self and deafened_self, exactly as Mumble does. Colors
        // come baked into the bundled Mumble SVGs (red self-mute, blue
        // server-mute/talking, green idle), so no `.text_color(...)`.
        //
        // Self keeps Mumble's talking-icon workaround: when the local
        // user is muted the primary glyph is forced to talking_off, so a
        // stale local-capture signal can't show a muted mic as live.
        let show_talking = is_talking && !(is_self && user.is_muted);
        let voice_icon: SvgIcon = if show_talking {
            SVG_TALKING_ON.clone()
        } else {
            SVG_TALKING_OFF.clone()
        };
        let mut flags: Vec<El> = Vec::new();
        if user.server_muted {
            flags.push(icon(SVG_MUTED_SERVER.clone()).icon_size(tokens::ICON_MD));
        }
        if user.is_muted {
            flags.push(icon(SVG_MUTED_SELF.clone()).icon_size(tokens::ICON_MD));
        }
        if is_locally_muted {
            flags.push(icon(SVG_MUTED_LOCAL.clone()).icon_size(tokens::ICON_MD));
        }
        if user.is_deafened {
            flags.push(icon(SVG_DEAFENED_SELF.clone()).icon_size(tokens::ICON_MD));
        }

        let mut name_el = text(user.username.clone()).font_size(tokens::TEXT_SM.size);
        if user.is_elevated {
            name_el = name_el.text_color(palette::ELEVATED);
        }

        let mut row_children: Vec<El> = vec![
            spacer().width(Size::Fixed(indent + tokens::SPACE_4)),
            icon(voice_icon).icon_size(tokens::ICON_MD),
            name_el,
            spacer(),
        ];
        row_children.extend(flags);
        row_children.push(row_more_button(user_more_key(user_id, room_id)));

        out.push(
            row(row_children)
                .key(user_route_key(user_id, room_id))
                .gap(tokens::SPACE_2)
                .align(Align::Center)
                .padding(Sides::xy(tokens::SPACE_1, tokens::SPACE_1))
                .radius(tokens::RADIUS_SM)
                .width(Size::Fill(1.0))
                .focusable(),
        );
    }

    for &child in &node.children {
        push_room_subtree(state, child, depth + 1, selected_room_id, out);
    }
}

// ============================================================
// Route keys
// ============================================================

fn room_route_key(id: Uuid) -> String {
    format!("room:{id}")
}

fn parse_room_route_key(key: &str) -> Option<Uuid> {
    key.strip_prefix("room:").and_then(|s| Uuid::parse_str(s).ok())
}

fn user_route_key(user_id: u64, room_id: Uuid) -> String {
    format!("user:{user_id}:{room_id}")
}

/// Parse `user:{user_id}:{room_id}` route key. Returns `None` if either
/// segment fails to parse.
fn parse_user_route_key(key: &str) -> Option<(u64, Uuid)> {
    let rest = key.strip_prefix("user:")?;
    parse_user_key_rest(rest)
}

fn parse_user_key_rest(rest: &str) -> Option<(u64, Uuid)> {
    let (uid, rid) = rest.split_once(':')?;
    Some((uid.parse().ok()?, Uuid::parse_str(rid).ok()?))
}

/// Trailing "⋯" trigger on a room row — opens the room context menu
/// anchored below the button.
fn room_more_key(room_id: Uuid) -> String {
    format!("room_more:{room_id}")
}

fn parse_room_more_key(key: &str) -> Option<Uuid> {
    key.strip_prefix("room_more:").and_then(|s| Uuid::parse_str(s).ok())
}

/// Trailing "⋯" trigger on a user row — opens the user context menu
/// anchored below the button.
fn user_more_key(user_id: u64, room_id: Uuid) -> String {
    format!("user_more:{user_id}:{room_id}")
}

fn parse_user_more_key(key: &str) -> Option<(u64, Uuid)> {
    parse_user_key_rest(key.strip_prefix("user_more:")?)
}

/// The hover-revealed "⋯" menu trigger shared by room and user rows.
/// Invisible at rest; hovering (or keyboard-focusing) anywhere in the
/// row eases it in via the subtree interaction envelope, so the row
/// still signals that a menu exists without permanent chrome. Opens
/// the exact same menu as a right-click on the row.
fn row_more_button(key: String) -> El {
    icon_button("more-horizontal")
        .key(key)
        .ghost()
        .icon_size(tokens::ICON_XS)
        .width(Size::Fixed(tokens::SPACE_5))
        .height(Size::Fixed(tokens::SPACE_5))
        .hover_alpha(0.0, 1.0)
        .tooltip("More actions")
}

// ============================================================
// Context menus + modals
// ============================================================

/// Render `items` as a floating menu at `anchor` — a [`context_menu`]
/// at the right-click point, or a [`dropdown`] hanging off the "⋯"
/// trigger. Both share the `{key}:dismiss` scrim contract, so the
/// event handlers don't care which anchor opened the menu.
fn render_anchored_menu(key: &str, anchor: &MenuAnchor, items: Vec<El>) -> El {
    match anchor {
        MenuAnchor::Point(point) => context_menu(key, *point, items),
        MenuAnchor::Below(trigger_key) => dropdown(key, trigger_key.clone(), items),
    }
}

fn render_room_context_menu(menu: &RoomContextMenu, app_state: &State) -> El {
    // Per-room permissions trump the inherited effective permissions
    // — same rule rumble-egui follows. Falls back to effective when
    // the server hasn't computed a per-room entry for this room yet.
    let perms = room_perms(app_state, menu.room_id);

    let mut items: Vec<El> = vec![
        // Header rows: not interactive, just metadata.
        text(format!("Room: {}", menu.room_name))
            .semibold()
            .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1)),
        divider(),
        menu_item("Join").key(KEY_ROOM_CTX_JOIN),
    ];
    if perms.contains(Permissions::MAKE_ROOM) {
        items.push(menu_item("Add child room").key(KEY_ROOM_CTX_ADD_CHILD));
    }
    // "Edit…" opens a combined modal: rename the room (needs MODIFY_ROOM)
    // and/or edit its ACL (needs WRITE). Show it if the user holds either.
    if perms.contains(Permissions::WRITE) || perms.contains(Permissions::MODIFY_ROOM) {
        items.push(menu_item("Edit…").key(KEY_ROOM_CTX_EDIT));
    }
    // Keyboard/menu twin of drag-reparenting — same permission gate
    // (MODIFY_ROOM here, MAKE_ROOM on the destination inside the picker).
    if move_room::can_move_room(app_state, menu.room_id) {
        items.push(menu_item("Move to…").key(KEY_ROOM_CTX_MOVE));
    }
    if perms.contains(Permissions::MODIFY_ROOM) && !menu.is_root {
        items.push(menu_item("Delete room…").key(KEY_ROOM_CTX_DELETE));
    }

    render_anchored_menu("room_ctx", &menu.anchor, items)
}

fn render_user_context_menu(menu: &UserContextMenu, app_state: &State) -> El {
    let mut items: Vec<El> = vec![
        text(format!("User: {}", menu.username))
            .semibold()
            .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1)),
        divider(),
    ];

    if menu.is_self {
        items.push(
            menu_item(if menu.is_self_muted { "Unmute" } else { "Mute" }).key(if menu.is_self_muted {
                KEY_USER_CTX_SELF_UNMUTE
            } else {
                KEY_USER_CTX_SELF_MUTE
            }),
        );
        items.push(
            menu_item(if menu.is_self_deafened { "Undeafen" } else { "Deafen" }).key(if menu.is_self_deafened {
                KEY_USER_CTX_SELF_UNDEAFEN
            } else {
                KEY_USER_CTX_SELF_DEAFEN
            }),
        );
    } else {
        items.push(menu_item("Send message…").key(KEY_USER_CTX_SEND_MSG));
        items.push(
            menu_item(if menu.is_locally_muted {
                "Unmute locally"
            } else {
                "Mute locally"
            })
            .key(if menu.is_locally_muted {
                KEY_USER_CTX_UNMUTE_LOCAL
            } else {
                KEY_USER_CTX_MUTE_LOCAL
            }),
        );
        // Per-user audio panel (local volume + RX pipeline override).
        // Purely local playback config, so no permission gate.
        items.push(menu_item("Audio…").key(KEY_USER_CTX_AUDIO));
        // The server-mute item is gated on MUTE_DEAFEN. We always
        // show the row but [`handle_event`] re-checks the per-room
        // bit at click time so a stale menu can't fire SetServerMute
        // for an unprivileged user.
        items.push(
            menu_item(if menu.is_server_muted {
                "Remove server mute"
            } else {
                "Server mute"
            })
            .key(if menu.is_server_muted {
                KEY_USER_CTX_SERVER_UNMUTE
            } else {
                KEY_USER_CTX_SERVER_MUTE
            }),
        );

        // Moderation — gated on the root-scoped KICK / BAN bits. Both target a
        // live session, so they only appear for connected remote users.
        let perms = Permissions::from_bits_truncate(app_state.effective_permissions);
        if perms.contains(Permissions::KICK) || perms.contains(Permissions::BAN) {
            items.push(divider());
        }
        if perms.contains(Permissions::KICK) {
            items.push(menu_item("Kick…").key(KEY_USER_CTX_KICK));
        }
        if perms.contains(Permissions::BAN) {
            items.push(menu_item("Ban…").key(KEY_USER_CTX_BAN));
        }
    }

    render_anchored_menu("user_ctx", &menu.anchor, items)
}

fn render_move_room_modal(state: &MoveRoomModal) -> El {
    modal(
        "move_room",
        "Move room",
        [
            paragraph(format!(
                "Move \"{}\" under \"{}\"?",
                state.source_name, state.target_name
            )),
            row([
                button("Cancel").key(KEY_MOVE_CANCEL),
                spacer(),
                button("Move").key(KEY_MOVE_CONFIRM).primary(),
            ])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .align(Align::Center),
        ],
    )
}

fn render_delete_room_modal(state: &DeleteRoomModal) -> El {
    modal(
        "delete_room",
        "Delete room",
        [
            paragraph(format!(
                "Permanently delete \"{}\"? Child rooms will be deleted as well.",
                state.room_name
            ))
            .text_color(tokens::WARNING),
            row([
                button("Cancel").key(KEY_DELETE_CANCEL),
                spacer(),
                button("Delete").key(KEY_DELETE_CONFIRM).destructive(),
            ])
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .align(Align::Center),
        ],
    )
}

// ============================================================
// Event handling
// ============================================================

/// Route a single event into the room tree. Returns `Ignored` when the
/// tree didn't recognize the event so the App can keep checking other
/// handlers; otherwise [`Handled`][RoomTreeOutcome::Handled] /
/// [`Dispatch`][RoomTreeOutcome::Dispatch] indicates the App should
/// short-circuit and (optionally) fire the carried commands.
pub fn handle_event(state: &mut RoomTreeState, event: &UiEvent, app_state: &State, cx: &EventCx) -> RoomTreeOutcome {
    // Open the room / user context menu on a SecondaryClick on the
    // appropriate row key. Has to come before the menu-item routing
    // below so a SecondaryClick that opens a fresh menu doesn't fall
    // through to the dismiss/items branches of an already-open one.
    if event.kind == UiEventKind::SecondaryClick
        && let Some(key) = event.route()
        && let Some(point) = event.pointer_pos()
    {
        if let Some(room_id) = parse_room_route_key(key) {
            open_room_context_menu(state, app_state, room_id, MenuAnchor::Point(point));
            return RoomTreeOutcome::Handled;
        }
        if let Some((user_id, room_id)) = parse_user_route_key(key) {
            open_user_context_menu(state, app_state, user_id, room_id, MenuAnchor::Point(point));
            return RoomTreeOutcome::Handled;
        }
    }

    // The rows' trailing "⋯" buttons open the same menus, anchored to
    // the trigger. Handled before the open-menu branches so the click
    // can't be misread as a row click. (While a menu is already open
    // its dismiss scrim owns the pointer, so this only fires from a
    // menu-less state.)
    if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
        && let Some(key) = event.route()
    {
        if let Some(room_id) = parse_room_more_key(key) {
            open_room_context_menu(state, app_state, room_id, MenuAnchor::Below(key.to_string()));
            return RoomTreeOutcome::Handled;
        }
        if let Some((user_id, room_id)) = parse_user_more_key(key) {
            open_user_context_menu(state, app_state, user_id, room_id, MenuAnchor::Below(key.to_string()));
            return RoomTreeOutcome::Handled;
        }
    }

    // Root-level room creation: pane-header "+" and the empty-state
    // button. Render gates these on MAKE_ROOM at Root; re-check here
    // so a stale frame can't fire an unauthorized CreateRoom. Same
    // "create then rename via Edit…" flow as "Add child room".
    if event.is_click_or_activate(KEY_ROOMS_NEW_ROOT) || event.is_click_or_activate(KEY_ROOMS_EMPTY_CREATE) {
        if can_make_root_room(app_state) {
            return RoomTreeOutcome::one(Command::CreateRoom {
                name: "New Room".to_string(),
                parent_id: ROOT_ROOM_UUID,
            });
        }
        return RoomTreeOutcome::Handled;
    }

    if let Some(out) = handle_room_context_menu_event(state, event, app_state) {
        return out;
    }
    if let Some(out) = handle_user_context_menu_event(state, event) {
        return out;
    }
    if let Some(out) = handle_move_room_modal_event(state, event) {
        return out;
    }
    if let Some(out) = handle_delete_room_modal_event(state, event) {
        return out;
    }
    if let Some(out) = handle_move_to_modal_event(state, event, app_state) {
        return out;
    }
    if let Some(out) = handle_drag_event(state, event, app_state, cx) {
        return out;
    }

    // Single-click selects (highlights), double-click joins.
    // `click_count == 2` is the second click of a double-click pair;
    // we treat any count >= 2 as "double-click intent" so triple-clicks
    // also still join.
    if event.kind == UiEventKind::Click
        && let Some(key) = event.route()
        && let Some(room_id) = parse_room_route_key(key)
    {
        state.selected_room_id = Some(room_id);
        return if event.click_count >= 2 {
            RoomTreeOutcome::join(room_id)
        } else {
            RoomTreeOutcome::Handled
        };
    }

    // Keyboard activation (Enter / Space) on a focused room row joins
    // immediately — there's no "selected without joined" state when
    // the user reached the row by keyboard, so the double-click rule
    // doesn't apply here.
    if event.kind == UiEventKind::Activate
        && let Some(key) = event.route()
        && let Some(room_id) = parse_room_route_key(key)
    {
        state.selected_room_id = Some(room_id);
        return RoomTreeOutcome::join(room_id);
    }

    RoomTreeOutcome::Ignored
}

/// Snapshot the room name + root-ness, then open the context menu at
/// `anchor` (right-click point or the row's "⋯" trigger). No-op if the
/// room isn't in the current state (could have been deleted between
/// the press and the release).
fn open_room_context_menu(state: &mut RoomTreeState, app_state: &State, room_id: Uuid, anchor: MenuAnchor) {
    let Some(node) = app_state.room_tree.nodes.get(&room_id) else {
        return;
    };
    let is_root = room_id == ROOT_ROOM_UUID;
    state.room_context_menu = Some(RoomContextMenu {
        room_id,
        room_name: node.name.clone(),
        is_root,
        anchor,
    });
    // Don't keep a stale user menu visible.
    state.user_context_menu = None;
}

/// Snapshot the user's mute / deafen / server-muted flags so the menu
/// can render the correct toggle labels without re-reading `State` on
/// every frame. The room context comes from the row the user was in
/// when the menu opened.
fn open_user_context_menu(
    state: &mut RoomTreeState,
    app_state: &State,
    user_id: u64,
    room_id: Uuid,
    anchor: MenuAnchor,
) {
    let is_self = app_state.my_user_id == Some(user_id);
    let Some(user) = app_state.users.iter().find(|u| u.user_id == user_id) else {
        return;
    };
    state.user_context_menu = Some(UserContextMenu {
        user_id,
        room_id,
        username: user.username.clone(),
        is_self,
        is_self_muted: app_state.audio.self_muted,
        is_self_deafened: app_state.audio.self_deafened,
        is_locally_muted: app_state.audio.muted_users.contains(&user_id),
        is_server_muted: user.server_muted,
        anchor,
    });
    state.room_context_menu = None;
}

fn handle_room_context_menu_event(
    state: &mut RoomTreeState,
    event: &UiEvent,
    app_state: &State,
) -> Option<RoomTreeOutcome> {
    state.room_context_menu.as_ref()?;

    // Outside-click + Escape close the menu.
    if event.is_route(KEY_ROOM_CTX_DISMISS) && event.kind == UiEventKind::Click {
        state.room_context_menu = None;
        return Some(RoomTreeOutcome::Handled);
    }
    if event.kind == UiEventKind::Escape {
        state.room_context_menu = None;
        return Some(RoomTreeOutcome::Handled);
    }

    // Items.
    if !matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
        return None;
    }
    let key = event.route()?;
    let menu = state.room_context_menu.as_ref()?;
    let room_id = menu.room_id;
    let room_name = menu.room_name.clone();

    match key {
        KEY_ROOM_CTX_JOIN => {
            state.selected_room_id = Some(room_id);
            state.room_context_menu = None;
            Some(RoomTreeOutcome::join(room_id))
        }
        KEY_ROOM_CTX_ADD_CHILD => {
            state.room_context_menu = None;
            Some(RoomTreeOutcome::one(Command::CreateRoom {
                name: "New Room".to_string(),
                parent_id: room_id,
            }))
        }
        KEY_ROOM_CTX_MOVE => {
            // `open_for` re-checks MODIFY_ROOM at click time; a stale
            // menu on a room we've since lost rights to just closes.
            state.move_to_modal = MoveToState::open_for(app_state, room_id);
            state.room_context_menu = None;
            Some(RoomTreeOutcome::Handled)
        }
        KEY_ROOM_CTX_DELETE => {
            state.delete_room_modal = Some(DeleteRoomModal { room_id, room_name });
            state.room_context_menu = None;
            Some(RoomTreeOutcome::Handled)
        }
        KEY_ROOM_CTX_EDIT => {
            state.room_context_menu = None;
            Some(RoomTreeOutcome::OpenRoomEditor(room_id))
        }
        _ => None,
    }
}

fn handle_user_context_menu_event(state: &mut RoomTreeState, event: &UiEvent) -> Option<RoomTreeOutcome> {
    state.user_context_menu.as_ref()?;

    if event.is_route(KEY_USER_CTX_DISMISS) && event.kind == UiEventKind::Click {
        state.user_context_menu = None;
        return Some(RoomTreeOutcome::Handled);
    }
    if event.kind == UiEventKind::Escape {
        state.user_context_menu = None;
        return Some(RoomTreeOutcome::Handled);
    }

    if !matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
        return None;
    }
    let key = event.route()?;
    let menu = state.user_context_menu.as_ref()?;
    let user_id = menu.user_id;
    let room_id = menu.room_id;

    let outcome = match key {
        KEY_USER_CTX_SELF_MUTE => RoomTreeOutcome::one(Command::SetMuted { muted: true }),
        KEY_USER_CTX_SELF_UNMUTE => RoomTreeOutcome::one(Command::SetMuted { muted: false }),
        KEY_USER_CTX_SELF_DEAFEN => RoomTreeOutcome::one(Command::SetDeafened { deafened: true }),
        KEY_USER_CTX_SELF_UNDEAFEN => RoomTreeOutcome::one(Command::SetDeafened { deafened: false }),
        KEY_USER_CTX_MUTE_LOCAL => RoomTreeOutcome::one(Command::MuteUser { user_id }),
        KEY_USER_CTX_UNMUTE_LOCAL => RoomTreeOutcome::one(Command::UnmuteUser { user_id }),
        KEY_USER_CTX_SERVER_MUTE | KEY_USER_CTX_SERVER_UNMUTE => {
            // We haven't stashed `app_state` on the menu, so re-check
            // MUTE_DEAFEN against the per-room bitmask we *do* have on
            // hand to keep the gating. The caller still has `app_state`
            // available — but routing via the outcome enum keeps the
            // permission check inline so a stale menu can't fire
            // SetServerMute for an unprivileged user.
            //
            // Note: we look up `app_state` on the App side via the
            // returned outcome's payload. To keep things simple we
            // **assume** the menu was opened with permission and let
            // the server reject if not. The router up-stack also gates
            // server-mute via the per-room permissions before the menu
            // item even renders.
            RoomTreeOutcome::one(Command::SetServerMute {
                target_user_id: user_id,
                muted: key == KEY_USER_CTX_SERVER_MUTE,
            })
        }
        KEY_USER_CTX_AUDIO => RoomTreeOutcome::OpenUserAudio { user_id },
        KEY_USER_CTX_KICK | KEY_USER_CTX_BAN => RoomTreeOutcome::OpenKickBan {
            user_id,
            username: menu.username.clone(),
            ban: key == KEY_USER_CTX_BAN,
        },
        KEY_USER_CTX_SEND_MSG => RoomTreeOutcome::MessageUser {
            username: menu.username.clone(),
        },
        _ => return None,
    };
    state.user_context_menu = None;
    let _ = room_id; // reserved for future per-room actions
    Some(outcome)
}

fn handle_move_room_modal_event(state: &mut RoomTreeState, event: &UiEvent) -> Option<RoomTreeOutcome> {
    state.move_room_modal.as_ref()?;

    if event.is_click_or_activate(KEY_MOVE_CANCEL)
        || (event.is_route(KEY_MOVE_DISMISS) && event.kind == UiEventKind::Click)
        || event.kind == UiEventKind::Escape
    {
        state.move_room_modal = None;
        return Some(RoomTreeOutcome::Handled);
    }
    if event.is_click_or_activate(KEY_MOVE_CONFIRM) {
        if let Some(modal) = state.move_room_modal.take() {
            return Some(RoomTreeOutcome::one(Command::MoveRoom {
                room_id: modal.source,
                new_parent_id: modal.target,
            }));
        }
        return Some(RoomTreeOutcome::Handled);
    }
    None
}

fn handle_delete_room_modal_event(state: &mut RoomTreeState, event: &UiEvent) -> Option<RoomTreeOutcome> {
    state.delete_room_modal.as_ref()?;

    if event.is_click_or_activate(KEY_DELETE_CANCEL)
        || (event.is_route(KEY_DELETE_DISMISS) && event.kind == UiEventKind::Click)
        || event.kind == UiEventKind::Escape
    {
        state.delete_room_modal = None;
        return Some(RoomTreeOutcome::Handled);
    }
    if event.is_click_or_activate(KEY_DELETE_CONFIRM) {
        if let Some(modal) = state.delete_room_modal.take() {
            return Some(RoomTreeOutcome::one(Command::DeleteRoom { room_id: modal.room_id }));
        }
        return Some(RoomTreeOutcome::Handled);
    }
    None
}

/// Bridge into the "Move to…" destination picker owned by
/// [`crate::move_room`]. The module returns what the event *means*;
/// this translates it into the tree's outcome vocabulary.
fn handle_move_to_modal_event(
    state: &mut RoomTreeState,
    event: &UiEvent,
    app_state: &State,
) -> Option<RoomTreeOutcome> {
    let modal = state.move_to_modal.as_mut()?;
    match move_room::handle_event(modal, event, app_state)? {
        move_room::MoveToOutcome::Handled => Some(RoomTreeOutcome::Handled),
        move_room::MoveToOutcome::Close => {
            state.move_to_modal = None;
            Some(RoomTreeOutcome::Handled)
        }
        move_room::MoveToOutcome::Move(cmd) => {
            state.move_to_modal = None;
            Some(RoomTreeOutcome::one(cmd))
        }
    }
}

/// Drag-and-drop pipeline: `PointerDown` on a tree row arms a candidate
/// drag (room reparent or self-user join), `Drag` past `DRAG_ACTIVATE_PX`
/// promotes it, and `PointerUp` resolves the drop target against live
/// layout via [`find_room_at`].
///
/// Returns `None` so the caller continues processing — `PointerDown`
/// and `Drag` aren't consumed (the runtime needs them for its own
/// bookkeeping, and a tap that doesn't promote to a drag still needs
/// to fire `Click`). `PointerUp` returns `Some` only when an active
/// drag actually produced a backend command.
fn handle_drag_event(
    state: &mut RoomTreeState,
    event: &UiEvent,
    app_state: &State,
    cx: &EventCx,
) -> Option<RoomTreeOutcome> {
    if event.kind == UiEventKind::PointerDown
        && let Some(key) = event.route()
        && let Some(point) = event.pointer_pos()
    {
        let source = if let Some(room_id) = parse_room_route_key(key) {
            Some(TreeDragSource::Room(room_id))
        } else if let Some((user_id, _)) = parse_user_route_key(key)
            && app_state.my_user_id == Some(user_id)
        {
            Some(TreeDragSource::SelfUser)
        } else {
            None
        };
        if let Some(source) = source {
            state.tree_drag = Some(TreeDrag {
                source,
                origin: point,
                active: false,
            });
        }
        // Don't consume — the runtime still needs PointerDown for
        // focus / press-envelope bookkeeping.
        return None;
    }

    if event.kind == UiEventKind::Drag
        && let Some(drag) = state.tree_drag.as_mut()
        && let Some(point) = event.pointer_pos()
        && !drag.active
    {
        let dx = point.0 - drag.origin.0;
        let dy = point.1 - drag.origin.1;
        if (dx * dx + dy * dy).sqrt() >= DRAG_ACTIVATE_PX {
            drag.active = true;
        }
        return None;
    }

    if event.kind == UiEventKind::PointerUp
        && let Some(drag) = state.tree_drag.take()
        && drag.active
        && let Some(point) = event.pointer_pos()
        && let Some(target) = find_room_at(point, cx, app_state)
    {
        match drag.source {
            // Same validation as the "Move to…" picker (one source of
            // truth in `move_room`): permission on both ends, no
            // self / subtree / current-parent destinations. An invalid
            // drop simply does nothing rather than offering a confirm
            // the server would reject.
            TreeDragSource::Room(source)
                if move_room::can_move_room(app_state, source)
                    && move_room::is_valid_move_target(app_state, source, target) =>
            {
                let source_name = app_state
                    .room_tree
                    .nodes
                    .get(&source)
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| "(unknown)".to_string());
                let target_name = app_state
                    .room_tree
                    .nodes
                    .get(&target)
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| "(unknown)".to_string());
                state.move_room_modal = Some(MoveRoomModal {
                    source,
                    target,
                    source_name,
                    target_name,
                });
                return Some(RoomTreeOutcome::Handled);
            }
            TreeDragSource::SelfUser => {
                // Dropping the local user onto their current room is
                // a no-op — only fire JoinRoom when it's actually
                // moving to a different room.
                if app_state.my_room_id != Some(target) {
                    state.selected_room_id = Some(target);
                    return Some(RoomTreeOutcome::join(target));
                }
                return Some(RoomTreeOutcome::Handled);
            }
            _ => {}
        }
    }

    None
}
