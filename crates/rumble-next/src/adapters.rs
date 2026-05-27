//! Translate live `rumble_protocol::State` into the display models the
//! shell renders. Every function here is a pure read of `&State` plus
//! an optional selection snapshot — no mutation, no commands.

use std::collections::HashMap;

use prost::Message as _;
use rumble_client::{ChatMessage, ChatMessageKind, ConnectionState, RoomTree, RoomTreeNode, State};
use rumble_protocol::{proto, proto::RelayFileSharePayload, uuid_from_room_id};
use rumble_widgets::{TreeNode, TreeNodeId, UserState};
use uuid::Uuid;

use crate::data::{ChatEntry, ChatMsg, Media, SysMsg, SysTone};

/// What a visible tree row points at. The rumble-widgets tree only
/// stores `u64` ids, so we maintain a side map here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeRef {
    Room(Uuid),
    User(u64),
}

/// Encoded-in-the-id flag: high bit = room (Uuid), clear = user.
const ROOM_FLAG: u64 = 0x8000_0000_0000_0000;

pub fn room_node_id(uuid: Uuid) -> TreeNodeId {
    let (hi, _lo) = uuid.as_u64_pair();
    hi | ROOM_FLAG
}

pub fn user_node_id(user_id: u64) -> TreeNodeId {
    user_id & !ROOM_FLAG
}

pub fn is_room(id: TreeNodeId) -> bool {
    id & ROOM_FLAG != 0
}

/// Build the display tree + a `TreeNodeId -> NodeRef` side map in one
/// pass. Children order: nested rooms, then users (matching Mumble's
/// conventional layout).
pub fn build_tree(state: &State) -> (Vec<TreeNode>, HashMap<TreeNodeId, NodeRef>) {
    let mut id_map: HashMap<TreeNodeId, NodeRef> = HashMap::new();
    let users_by_room = users_by_room(state);
    let roots = state
        .room_tree
        .roots
        .iter()
        .filter_map(|uuid| {
            state
                .room_tree
                .get(*uuid)
                .map(|node| build_room(state, &state.room_tree, node, &users_by_room, &mut id_map))
        })
        .collect();
    (roots, id_map)
}

fn build_room(
    state: &State,
    tree: &RoomTree,
    node: &RoomTreeNode,
    users_by_room: &HashMap<Uuid, Vec<&proto::User>>,
    id_map: &mut HashMap<TreeNodeId, NodeRef>,
) -> TreeNode {
    let tn_id = room_node_id(node.id);
    id_map.insert(tn_id, NodeRef::Room(node.id));

    let mut children: Vec<TreeNode> = Vec::new();
    // Users in this room first (like Mumble).
    if let Some(users) = users_by_room.get(&node.id) {
        for u in users {
            let Some(uid) = u.user_id.as_ref().map(|id| id.value) else {
                continue;
            };
            let display = if state.my_user_id == Some(uid) {
                format!("{} (you)", u.username)
            } else {
                u.username.clone()
            };
            let state_flags = user_state(u, state);
            let user_id = user_node_id(uid);
            id_map.insert(user_id, NodeRef::User(uid));
            children.push(TreeNode::user(user_id, display, state_flags));
        }
    }
    // Then nested rooms.
    for child_uuid in &node.children {
        if let Some(child) = tree.get(*child_uuid) {
            children.push(build_room(state, tree, child, users_by_room, id_map));
        }
    }

    TreeNode::channel(tn_id, node.name.clone()).with_children(children)
}

fn users_by_room(state: &State) -> HashMap<Uuid, Vec<&proto::User>> {
    let mut map: HashMap<Uuid, Vec<&proto::User>> = HashMap::new();
    for u in &state.users {
        if let Some(room_id) = u.current_room.as_ref().and_then(uuid_from_room_id) {
            map.entry(room_id).or_default().push(u);
        }
    }
    for users in map.values_mut() {
        users.sort_by(|a, b| a.username.cmp(&b.username));
    }
    map
}

fn user_state(u: &proto::User, state: &State) -> UserState {
    let uid = u.user_id.as_ref().map(|id| id.value);
    UserState {
        talking: uid.map(|id| state.audio.talking_users.contains(&id)).unwrap_or(false),
        muted: u.is_muted,
        deafened: u.is_deafened,
        server_muted: u.server_muted,
        away: false,
    }
}

// ---------- Breadcrumbs ----------

/// Breadcrumb labels from the root to the given room.
pub fn crumbs_for_room(state: &State, room: Uuid) -> Vec<String> {
    let mut ancestors: Vec<Uuid> = state.room_tree.ancestors(room).collect();
    ancestors.reverse();
    ancestors.push(room);
    ancestors
        .into_iter()
        .filter_map(|id| state.room_tree.get(id).map(|n| n.name.clone()))
        .collect()
}

// ---------- Chat ----------

/// Convert the recent chat messages into the display model.
///
/// - System notices (`visibility == System`) become system entries with `Info` tone.
/// - Normal messages keep sender + body.
/// - Timestamps render according to the supplied `TimestampFormat`,
///   or are blank if the caller passes `None` (the user has them
///   hidden).
pub fn chat_entries(state: &State, format: Option<rumble_desktop_shell::TimestampFormat>) -> Vec<ChatEntry> {
    let my_username = state
        .my_user_id
        .and_then(|id| {
            state
                .users
                .iter()
                .find(|u| u.user_id.as_ref().map(|x| x.value) == Some(id))
        })
        .map(|u| u.username.as_str());
    state
        .chat_messages
        .iter()
        .map(|m| render_entry(m, format, my_username))
        .collect()
}

fn render_entry(
    m: &ChatMessage,
    format: Option<rumble_desktop_shell::TimestampFormat>,
    my_username: Option<&str>,
) -> ChatEntry {
    let t = format.map(|f| format_timestamp(m.timestamp, f)).unwrap_or_default();
    if m.visibility.is_system() {
        return ChatEntry::Sys(SysMsg {
            tone: tone_for_local(&m.text),
            t,
            text: m.text.clone(),
        });
    }
    let who = match &m.kind {
        ChatMessageKind::Room => m.sender.clone(),
        ChatMessageKind::Tree => format!("{} (tree)", m.sender),
        ChatMessageKind::DirectMessage { other_username, .. } => {
            format!("{} → {}", m.sender, other_username)
        }
    };
    let media = m.attachment.as_ref().and_then(|a| {
        let fo = RelayFileSharePayload::decode(a.payload.as_slice()).ok()?;
        Some(file_offer_media(&fo, m.sender.as_str(), my_username))
    });
    // When an attachment is present, the `text` field is just a
    // human-readable summary that the file card already conveys —
    // suppress the duplicate body line.
    let body = if media.is_some() { None } else { Some(m.text.clone()) };
    ChatEntry::Msg(ChatMsg { who, t, body, media })
}

fn file_offer_media(fo: &RelayFileSharePayload, sender: &str, my_username: Option<&str>) -> Media {
    Media::FileOffer {
        ext: file_extension_label(&fo.name, &fo.mime),
        name: fo.name.clone(),
        size: format_bytes(fo.size),
        transfer_id: fo.transfer_id.clone(),
        share_data: fo.share_data.clone(),
        is_own: my_username == Some(sender),
    }
}

fn file_extension_label(name: &str, mime: &str) -> String {
    if let Some(ext) = name.rsplit_once('.').map(|(_, e)| e)
        && !ext.is_empty()
        && ext.len() <= 5
    {
        return ext.to_ascii_uppercase();
    }
    if let Some((_, sub)) = mime.rsplit_once('/')
        && !sub.is_empty()
    {
        return sub.to_ascii_uppercase();
    }
    "FILE".to_string()
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn tone_for_local(text: &str) -> SysTone {
    let lc = text.to_ascii_lowercase();
    if lc.contains("disconnect") || lc.contains("error") || lc.contains("lost") || lc.contains("kicked") {
        SysTone::Disc
    } else if lc.contains("joined") || lc.contains("connected") {
        SysTone::Join
    } else {
        SysTone::Info
    }
}

/// Format a `SystemTime` according to the user's preferred display
/// shape. Date components are derived from the unix epoch via Howard
/// Hinnant's algorithm — no chrono dep needed for the small set of
/// formats we care about.
fn format_timestamp(ts: std::time::SystemTime, format: rumble_desktop_shell::TimestampFormat) -> String {
    use rumble_desktop_shell::TimestampFormat as F;
    let unix = ts
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if matches!(format, F::Relative) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(unix);
        return relative_time(now.saturating_sub(unix));
    }

    let local = unix + local_offset_seconds();
    let t = local.rem_euclid(86_400);
    let h24 = (t / 3600) as u32;
    let m = ((t % 3600) / 60) as u32;
    let (h12, ampm) = match h24 {
        0 => (12, "AM"),
        1..=11 => (h24, "AM"),
        12 => (12, "PM"),
        _ => (h24 - 12, "PM"),
    };

    match format {
        F::Time24h => format!("{h24:02}:{m:02}"),
        F::Time12h => format!("{h12}:{m:02} {ampm}"),
        F::DateTime24h => format!("{} {h24:02}:{m:02}", date_string(local)),
        F::DateTime12h => format!("{} {h12}:{m:02} {ampm}", date_string(local)),
        F::Relative => unreachable!(),
    }
}

/// Civil date for an epoch in seconds (positive, post-1970). Uses
/// the Hinnant date algorithm so we don't pull in chrono just for
/// the date suffix.
fn date_string(local_secs: i64) -> String {
    let days = local_secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn relative_time(delta: i64) -> String {
    if delta < 60 {
        "just now".into()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else if delta < 7 * 86_400 {
        format!("{}d ago", delta / 86_400)
    } else {
        format!("{}w ago", delta / (7 * 86_400))
    }
}

/// Local-time UTC offset in seconds. Read from the `TZ` environment
/// variable when it's in numeric `±HH:MM` form; otherwise falls back
/// to 0 (UTC). `rumble-egui` also shows timestamps in UTC, so the
/// fallback keeps the two clients visually consistent.
fn local_offset_seconds() -> i64 {
    std::env::var("TZ")
        .ok()
        .and_then(|tz| parse_numeric_tz(&tz))
        .unwrap_or(0)
}

fn parse_numeric_tz(tz: &str) -> Option<i64> {
    let tz = tz.trim();
    let (sign, rest) = match tz.as_bytes().first()? {
        b'+' => (1_i64, &tz[1..]),
        b'-' => (-1_i64, &tz[1..]),
        _ => return None,
    };
    let mut parts = rest.split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = match parts.next() {
        Some(m) => m.parse().ok()?,
        None => 0,
    };
    Some(sign * (hours * 3600 + minutes * 60))
}

// ---------- Self-state helpers ----------

pub fn my_display_name(state: &State) -> Option<String> {
    state
        .my_user_id
        .and_then(|id| state.get_user(id))
        .map(|u| u.username.clone())
}

/// Whether the server is muting our own user (auto on SPEAK-denied room
/// join, or via moderator action). The audio task gates capture on this
/// internally; the UI uses it to disable PTT and surface a "🔒" badge
/// instead of a misleading active-PTT light.
pub fn am_i_server_muted(state: &State) -> bool {
    state
        .my_user_id
        .and_then(|id| state.get_user(id))
        .map(|u| u.server_muted)
        .unwrap_or(false)
}

pub fn peers_in_current_room(state: &State) -> usize {
    state.my_room_id.map(|rid| state.users_in_room(rid).len()).unwrap_or(0)
}

pub fn connection_summary(state: &State) -> String {
    match &state.connection {
        ConnectionState::Disconnected => "● disconnected".to_string(),
        ConnectionState::Connecting { server_addr } => format!("● connecting to {server_addr}"),
        ConnectionState::Connected { server_name, .. } => format!("● connected · {server_name}"),
        ConnectionState::ConnectionLost { error } => format!("● lost: {error}"),
        ConnectionState::CertificatePending { .. } => "● waiting for certificate approval".to_string(),
    }
}
