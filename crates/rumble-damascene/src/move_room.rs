//! "Move to…" destination picker for rooms.
//!
//! Opened from a room's context menu (gated on `MODIFY_ROOM`, same as
//! the drag-reparent path). Lists every valid destination parent —
//! walked in room-tree render order so the hierarchy reads naturally —
//! and fires the same `Command::MoveRoom` the drag-and-drop flow uses.
//!
//! The validation ([`is_valid_move_target`] / [`can_move_room`]) is the
//! single source of truth shared with the drag path in
//! [`crate::room_tree`]: no self-moves, no moves into the room's own
//! subtree (a parent cycle bricks the server-side ACL walk, so the
//! server rejects it too — we just don't offer it), `MODIFY_ROOM` on
//! the room being moved and `MAKE_ROOM` on the destination, mirroring
//! the server's `handle_move_room` checks. "Root level" is represented
//! by the Root room itself (UUID nil): `MoveRoom.new_parent_id` is not
//! optional on the wire, and the server treats parent == Root as
//! top-of-tree.

use damascene_core::prelude::*;
use rumble_client::{Command, ROOT_ROOM_UUID, State};
use rumble_protocol::permissions::Permissions;
use uuid::Uuid;

// ============================================================
// State
// ============================================================

/// Ephemeral state of the open "Move to…" modal.
#[derive(Clone, Debug)]
pub struct MoveToState {
    /// Room being moved.
    pub room_id: Uuid,
    /// Snapshot of its name at open time (for the title).
    pub room_name: String,
    /// Destination row the user has highlighted; `None` until a pick.
    pub selected: Option<Uuid>,
}

impl MoveToState {
    /// Open the picker for `room_id`. Returns `None` when the room is
    /// missing from the current state or the local user may not move
    /// it — callers gate the menu entry the same way, this is the
    /// click-time re-check.
    pub fn open_for(app_state: &State, room_id: Uuid) -> Option<Self> {
        if !can_move_room(app_state, room_id) {
            return None;
        }
        let node = app_state.room_tree.nodes.get(&room_id)?;
        Some(Self {
            room_id,
            room_name: node.name.clone(),
            selected: None,
        })
    }
}

/// What an event routed into the open modal means for the caller.
pub enum MoveToOutcome {
    /// Consumed (selection change, ignored confirm, …); keep the modal.
    Handled,
    /// Cancel / Escape / scrim — close the modal.
    Close,
    /// Confirm — fire the command and close the modal.
    Move(Command),
}

// ============================================================
// Validation (shared with the drag path)
// ============================================================

/// Per-room permission bits: the server-computed per-room entry when
/// present, else the session-wide effective permissions.
fn room_perms(app_state: &State, room_id: Uuid) -> Permissions {
    let bits = app_state
        .per_room_permissions
        .get(&room_id)
        .copied()
        .unwrap_or(app_state.effective_permissions);
    Permissions::from_bits_truncate(bits)
}

/// May the local user move `room_id` at all? Mirrors the server's
/// `MODIFY_ROOM`-on-the-moved-room check; the Root room additionally
/// can't be moved (everything lives under it).
pub fn can_move_room(app_state: &State, room_id: Uuid) -> bool {
    if room_id == ROOT_ROOM_UUID {
        return false;
    }
    app_state.room_tree.nodes.contains_key(&room_id)
        && room_perms(app_state, room_id).contains(Permissions::MODIFY_ROOM)
}

/// True when `target` lies inside `source`'s subtree (including
/// `source` itself). Moving there would create a parent cycle.
fn in_subtree(app_state: &State, source: Uuid, target: Uuid) -> bool {
    if source == target {
        return true;
    }
    let Some(node) = app_state.room_tree.nodes.get(&source) else {
        return false;
    };
    node.children.iter().any(|&child| in_subtree(app_state, child, target))
}

/// Is `target` a legal new parent for `source`? One source of truth
/// for both the picker list and the drag-drop path: rejects self /
/// subtree destinations (cycles), the current parent (no-op), and
/// destinations the user lacks `MAKE_ROOM` on (the server checks the
/// same bit on the new parent).
pub fn is_valid_move_target(app_state: &State, source: Uuid, target: Uuid) -> bool {
    if in_subtree(app_state, source, target) {
        return false;
    }
    let Some(source_node) = app_state.room_tree.nodes.get(&source) else {
        return false;
    };
    if !app_state.room_tree.nodes.contains_key(&target) {
        return false;
    }
    if source_node.parent_id == Some(target) {
        return false; // already there
    }
    room_perms(app_state, target).contains(Permissions::MAKE_ROOM)
}

/// A candidate destination row: room id, display name, and tree depth
/// (for indentation).
pub struct Destination {
    pub id: Uuid,
    pub name: String,
    pub depth: usize,
}

/// All valid destinations for moving `source`, in room-tree render
/// order (roots first, children depth-first under their parent).
pub fn destinations(app_state: &State, source: Uuid) -> Vec<Destination> {
    fn walk(app_state: &State, source: Uuid, room_id: Uuid, depth: usize, out: &mut Vec<Destination>) {
        let Some(node) = app_state.room_tree.nodes.get(&room_id) else {
            return;
        };
        // Don't descend into the source's own subtree at all — every
        // room below it is an invalid (cycle-forming) destination.
        if room_id == source {
            return;
        }
        if is_valid_move_target(app_state, source, room_id) {
            out.push(Destination {
                id: room_id,
                name: node.name.clone(),
                depth,
            });
        }
        for &child in &node.children {
            walk(app_state, source, child, depth + 1, out);
        }
    }

    let mut out = Vec::new();
    walk(app_state, source, ROOT_ROOM_UUID, 0, &mut out);
    out
}

// ============================================================
// Render
// ============================================================

const KEY_DISMISS: &str = "move_to:dismiss";
const KEY_CANCEL: &str = "move_to:cancel";
const KEY_CONFIRM: &str = "move_to:confirm";
const DEST_KEY_PREFIX: &str = "move_to:dest:";

fn dest_key(id: Uuid) -> String {
    format!("{DEST_KEY_PREFIX}{id}")
}

fn parse_dest_key(key: &str) -> Option<Uuid> {
    key.strip_prefix(DEST_KEY_PREFIX).and_then(|s| Uuid::parse_str(s).ok())
}

/// Render the open picker. Destinations are recomputed every frame so
/// permission / tree changes while the modal is up stay honest.
pub fn render(state: &MoveToState, app_state: &State) -> El {
    let dests = destinations(app_state, state.room_id);

    let list: El = if dests.is_empty() {
        column([
            text("No destinations available.").muted(),
            paragraph("You need permission to create rooms under a destination to move this room there.")
                .muted()
                .font_size(tokens::TEXT_XS.size),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .padding(tokens::SPACE_5)
        .width(Size::Fill(1.0))
    } else {
        let rows: Vec<El> = dests
            .iter()
            .map(|dest| {
                let is_selected = state.selected == Some(dest.id);
                let mut row_el = row([
                    spacer().width(Size::Fixed(dest.depth as f32 * tokens::SPACE_4)),
                    icon(IconName::Folder)
                        .icon_size(tokens::ICON_SM)
                        .text_color(tokens::MUTED_FOREGROUND),
                    text(dest.name.clone()).font_size(tokens::TEXT_SM.size),
                ])
                .key(dest_key(dest.id))
                .gap(tokens::SPACE_2)
                .align(Align::Center)
                .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
                .radius(tokens::RADIUS_SM)
                .width(Size::Fill(1.0))
                .cursor(Cursor::Pointer)
                .focusable();
                if is_selected {
                    row_el = row_el.fill(tokens::ACCENT);
                }
                row_el
            })
            .collect();
        scroll(rows)
            .gap(tokens::SPACE_1)
            .padding(Sides::all(tokens::SPACE_1))
            .width(Size::Fill(1.0))
            .height(Size::Fixed(240.0))
    };

    let body: Vec<El> = vec![
        paragraph("Pick the new parent room. Moving to \"Root\" places it at the top level.")
            .muted()
            .font_size(tokens::TEXT_XS.size),
        list.stroke(tokens::BORDER).radius(tokens::RADIUS_SM),
        row([
            button("Cancel").key(KEY_CANCEL),
            spacer(),
            button("Move").key(KEY_CONFIRM).primary(),
        ])
        .gap(tokens::SPACE_2)
        .width(Size::Fill(1.0))
        .align(Align::Center),
    ];

    modal("move_to", format!("Move \"{}\" to…", state.room_name), body)
}

// ============================================================
// Events
// ============================================================

/// Route an event into the open modal. Returns `None` when the event
/// isn't for this modal (the caller keeps routing).
pub fn handle_event(state: &mut MoveToState, event: &UiEvent, app_state: &State) -> Option<MoveToOutcome> {
    if (event.is_route(KEY_DISMISS) && event.kind == UiEventKind::Click)
        || event.is_click_or_activate(KEY_CANCEL)
        || event.kind == UiEventKind::Escape
    {
        return Some(MoveToOutcome::Close);
    }

    if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
        && let Some(key) = event.route()
        && let Some(dest) = parse_dest_key(key)
    {
        state.selected = Some(dest);
        return Some(MoveToOutcome::Handled);
    }

    if event.is_click_or_activate(KEY_CONFIRM) {
        // Re-validate at click time: permissions or the tree may have
        // shifted since the row was picked (render gates the list, this
        // is the second half of the render-time + click-time contract).
        let Some(target) = state.selected else {
            return Some(MoveToOutcome::Handled); // nothing picked yet
        };
        if !can_move_room(app_state, state.room_id) || !is_valid_move_target(app_state, state.room_id, target) {
            state.selected = None;
            return Some(MoveToOutcome::Handled);
        }
        return Some(MoveToOutcome::Move(Command::MoveRoom {
            room_id: state.room_id,
            new_parent_id: target,
        }));
    }

    None
}
