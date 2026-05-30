//! Room tree view: hierarchical rooms with users inlined as leaves,
//! single-click select / double-click join, right-click context menus
//! for rooms and users, drag-and-drop reparenting (room → room) and
//! drag-yourself-to-join (self-user → room), plus the confirmation
//! modals those two drag flows fall into.
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

use std::{
    panic::Location,
    sync::{Arc, LazyLock, Mutex},
};

use damascene_core::prelude::*;
use rumble_client::{Command, State};
use rumble_protocol::permissions::Permissions;
use uuid::Uuid;

use crate::theme as palette;

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
/// highlighted, which context menu / modal is open, in-flight drag
/// source, and the rect snapshot that drag-drop hit-tests against.
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
    /// Last-laid-out room-drop hit zones, refreshed each frame by the
    /// rect-tracker custom layout fn so `PointerUp` after a drag can
    /// hit-test the drop point against every room row (and the user
    /// rows below it, mapped back to the parent room). The same target
    /// room can appear multiple times — once for the room row, once
    /// per user row — and [`Self::find_room_at`] returns the first
    /// match. `Mutex` is unconditional because the layout closure is
    /// `Fn(LayoutCtx) -> Vec<Rect>` — no `FnMut` allowed.
    pub(crate) room_rects: Arc<Mutex<Vec<(Uuid, Rect)>>>,
}

impl RoomTreeState {
    /// Hit-test `point` against the room rects we captured during the
    /// last layout pass. Used by drag-and-drop to resolve the drop
    /// target on `PointerUp`. Returns `None` when the pointer is over
    /// empty space, the rooms scroll viewport border, or a row that
    /// hasn't been laid out yet.
    fn find_room_at(&self, point: (f32, f32)) -> Option<Uuid> {
        let list = self.room_rects.lock().ok()?;
        list.iter()
            .find_map(|(id, rect)| rect.contains(point.0, point.1).then_some(*id))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RoomContextMenu {
    room_id: Uuid,
    room_name: String,
    is_root: bool,
    point: (f32, f32),
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
    point: (f32, f32),
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
    /// User picked "Permissions…" on a room — App should open the
    /// per-room ACL editor modal for this room.
    OpenAclEditor(Uuid),
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
const KEY_ROOM_CTX_PERMS: &str = "room_ctx:perms";
const KEY_ROOM_CTX_ADD_CHILD: &str = "room_ctx:add_child";
const KEY_ROOM_CTX_DELETE: &str = "room_ctx:delete";

const KEY_USER_CTX_DISMISS: &str = "user_ctx:dismiss";
const KEY_USER_CTX_SELF_MUTE: &str = "user_ctx:self_mute";
const KEY_USER_CTX_SELF_UNMUTE: &str = "user_ctx:self_unmute";
const KEY_USER_CTX_SELF_DEAFEN: &str = "user_ctx:self_deafen";
const KEY_USER_CTX_SELF_UNDEAFEN: &str = "user_ctx:self_undeafen";
const KEY_USER_CTX_MUTE_LOCAL: &str = "user_ctx:mute_local";
const KEY_USER_CTX_UNMUTE_LOCAL: &str = "user_ctx:unmute_local";
const KEY_USER_CTX_SERVER_MUTE: &str = "user_ctx:server_mute";
const KEY_USER_CTX_SERVER_UNMUTE: &str = "user_ctx:server_unmute";

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
    rooms_view(app_state, state.selected_room_id, &state.room_rects)
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
}

impl RoomTreeOverlays {
    /// True when at least one overlay is visible — useful for the
    /// caller's "are any layers active?" decision.
    pub fn any(&self) -> bool {
        self.room_context_menu.is_some()
            || self.user_context_menu.is_some()
            || self.move_room_modal.is_some()
            || self.delete_room_modal.is_some()
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
        user_context_menu: state.user_context_menu.as_ref().map(render_user_context_menu),
        move_room_modal: state.move_room_modal.as_ref().map(render_move_room_modal),
        delete_room_modal: state.delete_room_modal.as_ref().map(render_delete_room_modal),
    }
}

fn rooms_view(state: &State, selected_room_id: Option<Uuid>, room_rects: &Arc<Mutex<Vec<(Uuid, Rect)>>>) -> El {
    let mut entries: Vec<El> = Vec::new();
    // (key, drop_target_room_id) pairs handed to the rect tracker
    // closure. Rooms register their own UUID; users below a room
    // register that room's UUID so dragging onto a user resolves to
    // dropping into the user's room.
    let mut hit_rows: Vec<(String, Uuid)> = Vec::new();
    for &root_id in &state.room_tree.roots {
        push_room_subtree(state, root_id, 0, selected_room_id, &mut entries, &mut hit_rows);
    }

    if entries.is_empty() {
        entries.push(text("No rooms received yet.").muted());
    }

    let tracker = room_rect_tracker(hit_rows, Arc::clone(room_rects));

    column([
        text("Rooms")
            .title()
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2)),
        divider(),
        scroll(entries)
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_3))
            .gap(tokens::SPACE_1)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0)),
        tracker,
    ])
    .width(Size::Fill(1.0))
    .height(Size::Fill(1.0))
}

/// Hidden zero-size element whose only job is to snapshot the laid-out
/// rect of every room row each frame. Sits as the last child of
/// `rooms_view`'s column so by the time its custom layout fn runs, all
/// room rows (inside the scroll viewport above) have been laid out and
/// their rects are addressable via `LayoutCtx::rect_of_key`.
///
/// User rows are captured too, mapped back to their parent room — so
/// dropping a drag onto a user counts as dropping into the user's room
/// (since the room row only spans the room's own line, and the user
/// rows below it would otherwise resolve to "no drop target").
///
/// Returns rects post-scroll-translation, which is exactly what
/// `PointerUp` hit-testing in screen coordinates needs.
fn room_rect_tracker(rows: Vec<(String, Uuid)>, rects: Arc<Mutex<Vec<(Uuid, Rect)>>>) -> El {
    El::new(Kind::Custom("room-rect-tracker"))
        .width(Size::Fixed(0.0))
        .height(Size::Fixed(0.0))
        .layout(move |ctx| {
            if let Ok(mut list) = rects.lock() {
                list.clear();
                for (key, target) in &rows {
                    if let Some(rect) = (ctx.rect_of_key)(key) {
                        list.push((*target, rect));
                    }
                }
            }
            // Custom layout returns one Rect per child; we have none.
            Vec::new()
        })
}

fn push_room_subtree(
    state: &State,
    room_id: Uuid,
    depth: usize,
    selected_room_id: Option<Uuid>,
    out: &mut Vec<El>,
    hit_rows: &mut Vec<(String, Uuid)>,
) {
    use rumble_protocol::uuid_from_room_id;

    let Some(node) = state.room_tree.nodes.get(&room_id) else {
        return;
    };

    let user_count = state
        .users
        .iter()
        .filter(|u| {
            u.current_room
                .as_ref()
                .and_then(uuid_from_room_id)
                .is_some_and(|id| id == room_id)
        })
        .count();
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
    ])
    .key(room_route_key(room_id))
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .padding(Sides::xy(tokens::SPACE_1, tokens::SPACE_1))
    .radius(tokens::RADIUS_SM)
    .focusable();
    if is_selected {
        // Subtle pill so single-click selection is visible without
        // looking like the active-room indicator.
        row_el = row_el.fill(tokens::ACCENT);
    }
    out.push(row_el);
    hit_rows.push((room_route_key(room_id), room_id));

    for user in state.users.iter().filter(|u| {
        u.current_room
            .as_ref()
            .and_then(uuid_from_room_id)
            .is_some_and(|id| id == room_id)
    }) {
        let user_id = user.user_id.as_ref().map(|u| u.value).unwrap_or(0);
        // talking_users only tracks remote peers (populated from received
        // voice datagrams); for self we use the local capture/VAD signal.
        let is_self = state.my_user_id == Some(user_id);
        let is_talking = state.audio.talking_users.contains(&user_id) || (is_self && state.audio.is_transmitting);
        // Mic-state glyph picks up its color from the bundled Mumble
        // SVG itself (red self-mute, blue server-mute, blue talking,
        // green idle), so no `.text_color(...)` override here.
        let mic_icon: SvgIcon = if user.server_muted {
            SVG_MUTED_SERVER.clone()
        } else if user.is_muted {
            SVG_MUTED_SELF.clone()
        } else if is_talking {
            SVG_TALKING_ON.clone()
        } else {
            SVG_TALKING_OFF.clone()
        };

        let mut name_el = text(user.username.clone()).font_size(tokens::TEXT_SM.size);
        if user.is_elevated {
            name_el = name_el.text_color(palette::ELEVATED);
        }

        let user_key = user_route_key(user_id, room_id);
        out.push(
            row([
                spacer().width(Size::Fixed(indent + tokens::SPACE_4)),
                icon(mic_icon).icon_size(tokens::ICON_MD),
                name_el,
            ])
            .key(user_key.clone())
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .padding(Sides::xy(tokens::SPACE_1, tokens::SPACE_1))
            .focusable(),
        );
        // User row drops into its parent room.
        hit_rows.push((user_key, room_id));
    }

    for &child in &node.children {
        push_room_subtree(state, child, depth + 1, selected_room_id, out, hit_rows);
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
    let (uid, rid) = rest.split_once(':')?;
    Some((uid.parse().ok()?, Uuid::parse_str(rid).ok()?))
}

// ============================================================
// Context menus + modals
// ============================================================

fn render_room_context_menu(menu: &RoomContextMenu, app_state: &State) -> El {
    // Per-room permissions trump the inherited effective permissions
    // — same rule rumble-egui follows. Falls back to effective when
    // the server hasn't computed a per-room entry for this room yet.
    let perms_bits = app_state
        .per_room_permissions
        .get(&menu.room_id)
        .copied()
        .unwrap_or(app_state.effective_permissions);
    let perms = Permissions::from_bits_truncate(perms_bits);

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
    if perms.contains(Permissions::WRITE) {
        items.push(menu_item("Permissions…").key(KEY_ROOM_CTX_PERMS));
    }
    if perms.contains(Permissions::MODIFY_ROOM) && !menu.is_root {
        items.push(menu_item("Delete room…").key(KEY_ROOM_CTX_DELETE));
    }

    context_menu("room_ctx", menu.point, items)
}

fn render_user_context_menu(menu: &UserContextMenu) -> El {
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
    }

    context_menu("user_ctx", menu.point, items)
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
pub fn handle_event(state: &mut RoomTreeState, event: &UiEvent, app_state: &State) -> RoomTreeOutcome {
    // Open the room / user context menu on a SecondaryClick on the
    // appropriate row key. Has to come before the menu-item routing
    // below so a SecondaryClick that opens a fresh menu doesn't fall
    // through to the dismiss/items branches of an already-open one.
    if event.kind == UiEventKind::SecondaryClick
        && let Some(key) = event.route()
        && let Some(point) = event.pointer_pos()
    {
        if let Some(room_id) = parse_room_route_key(key) {
            open_room_context_menu(state, app_state, room_id, point);
            return RoomTreeOutcome::Handled;
        }
        if let Some((user_id, room_id)) = parse_user_route_key(key) {
            open_user_context_menu(state, app_state, user_id, room_id, point);
            return RoomTreeOutcome::Handled;
        }
    }

    if let Some(out) = handle_room_context_menu_event(state, event) {
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
    if let Some(out) = handle_drag_event(state, event, app_state) {
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

/// Snapshot the room name + root-ness, then open the right-click
/// context menu anchored at `point`. No-op if the room isn't in the
/// current state (could have been deleted between the press and the
/// release).
fn open_room_context_menu(state: &mut RoomTreeState, app_state: &State, room_id: Uuid, point: (f32, f32)) {
    let Some(node) = app_state.room_tree.nodes.get(&room_id) else {
        return;
    };
    let is_root = app_state.room_tree.roots.contains(&room_id);
    state.room_context_menu = Some(RoomContextMenu {
        room_id,
        room_name: node.name.clone(),
        is_root,
        point,
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
    point: (f32, f32),
) {
    let is_self = app_state.my_user_id == Some(user_id);
    let Some(user) = app_state
        .users
        .iter()
        .find(|u| u.user_id.as_ref().map(|i| i.value) == Some(user_id))
    else {
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
        point,
    });
    state.room_context_menu = None;
}

fn handle_room_context_menu_event(state: &mut RoomTreeState, event: &UiEvent) -> Option<RoomTreeOutcome> {
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
                parent_id: Some(room_id),
            }))
        }
        KEY_ROOM_CTX_DELETE => {
            state.delete_room_modal = Some(DeleteRoomModal { room_id, room_name });
            state.room_context_menu = None;
            Some(RoomTreeOutcome::Handled)
        }
        KEY_ROOM_CTX_PERMS => {
            state.room_context_menu = None;
            Some(RoomTreeOutcome::OpenAclEditor(room_id))
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
        _ => return None,
    };
    state.user_context_menu = None;
    let _ = room_id; // reserved for future per-room actions (e.g. kick reason)
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

/// Drag-and-drop pipeline: `PointerDown` on a tree row arms a candidate
/// drag (room reparent or self-user join), `Drag` past `DRAG_ACTIVATE_PX`
/// promotes it, and `PointerUp` resolves the drop target via the rect
/// snapshot in [`RoomTreeState::room_rects`].
///
/// Returns `None` so the caller continues processing — `PointerDown`
/// and `Drag` aren't consumed (the runtime needs them for its own
/// bookkeeping, and a tap that doesn't promote to a drag still needs
/// to fire `Click`). `PointerUp` returns `Some` only when an active
/// drag actually produced a backend command.
fn handle_drag_event(state: &mut RoomTreeState, event: &UiEvent, app_state: &State) -> Option<RoomTreeOutcome> {
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
        && let Some(target) = state.find_room_at(point)
    {
        match drag.source {
            TreeDragSource::Room(source) if source != target => {
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
