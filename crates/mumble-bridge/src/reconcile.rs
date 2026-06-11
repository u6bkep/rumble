//! Full-state reconciliation: diff two full Rumble server states into the
//! ordered Mumble messages needed to bring connected Mumble clients up to
//! date.
//!
//! Every full `ServerState` (initial connect and reconnect refresh) is the
//! single reconciliation point for missed/dropped incremental `StateUpdate`s:
//! the bridge diffs the previously projected rooms/users against the new
//! snapshot and broadcasts the delta, pruning proxy-session mappings for
//! entities that no longer exist server-side.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet, VecDeque},
};

use rumble_protocol::proto;
use uuid::Uuid;

use crate::{
    mumble_proto::mumble,
    state::{ChannelMap, UserMap},
};

/// Rumble users that must never be projected to Mumble clients: the bridge's
/// own connection user, its registered virtual users (Mumble clients mirrored
/// onto the server), and virtual users whose registration is still in flight
/// (matched by username, mirroring `handle_state_update`).
#[derive(Debug, Default)]
pub struct SkipRules {
    /// The bridge's own user ID on the Rumble server.
    pub bridge_user_id: Option<u64>,
    /// Rumble IDs of this bridge's registered virtual users.
    pub virtual_user_ids: HashSet<u64>,
    /// Usernames with an in-flight `RegisterParticipant`.
    pub pending_usernames: HashSet<String>,
}

impl SkipRules {
    fn skips(&self, user: &proto::User) -> bool {
        let id = user_id(user);
        Some(id) == self.bridge_user_id
            || self.virtual_user_ids.contains(&id)
            || self.pending_usernames.contains(&user.username)
    }
}

/// A Mumble message produced by reconciliation, already in send order.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncMessage {
    UserRemove(mumble::UserRemove),
    UserState(mumble::UserState),
    ChannelRemove(mumble::ChannelRemove),
    ChannelState(mumble::ChannelState),
}

/// Result of diffing the old projected state against a new full snapshot.
#[derive(Debug, Default)]
pub struct ReconcileOutput {
    /// Messages to broadcast to all connected Mumble clients, in order:
    /// user removes, then channel adds/changes (parent before child), then
    /// user adds/changes, then channel removes (child before parent). Users
    /// are removed first so removed channels are vacated, channels are
    /// created before users are moved into them, and channels are removed
    /// last so surviving users have already been moved out.
    pub messages: Vec<SyncMessage>,
    /// Rumble user IDs whose proxy sessions were pruned; the caller must
    /// also drop their per-user outbound sequence counters.
    pub removed_user_ids: Vec<u64>,
}

fn user_id(user: &proto::User) -> u64 {
    user.user_id.as_ref().map(|id| id.value).unwrap_or(0)
}

fn room_uuid(room: &proto::RoomInfo) -> Option<Uuid> {
    room.id.as_ref().and_then(rumble_protocol::uuid_from_room_id)
}

fn parent_uuid(room: &proto::RoomInfo) -> Uuid {
    room.parent_id
        .as_ref()
        .and_then(rumble_protocol::uuid_from_room_id)
        .unwrap_or(rumble_protocol::ROOT_ROOM_UUID)
}

/// Depth of `uuid` in the old room tree (root = 0). Unknown ancestry stops
/// the walk; a parent cycle is capped at the room count.
fn old_depth(uuid: Uuid, old_by_uuid: &HashMap<Uuid, &proto::RoomInfo>) -> usize {
    let mut depth = 0;
    let mut cur = uuid;
    while cur != rumble_protocol::ROOT_ROOM_UUID && depth <= old_by_uuid.len() {
        match old_by_uuid.get(&cur) {
            Some(room) => cur = parent_uuid(room),
            None => break,
        }
        depth += 1;
    }
    depth
}

/// Diff the previously projected state (`old_*`) against a new full snapshot
/// (`new_*`), updating the bidirectional `channels`/`users` maps in place
/// (assigning IDs for new entities, pruning vanished ones) and returning the
/// ordered Mumble messages that bring every connected client up to date.
pub fn reconcile_server_state(
    old_rooms: &[proto::RoomInfo],
    old_users: &[proto::User],
    new_rooms: &[proto::RoomInfo],
    new_users: &[proto::User],
    channels: &mut ChannelMap,
    users: &mut UserMap,
    skip: &SkipRules,
) -> ReconcileOutput {
    let mut out = ReconcileOutput::default();

    let old_room_by_uuid: HashMap<Uuid, &proto::RoomInfo> =
        old_rooms.iter().filter_map(|r| room_uuid(r).map(|u| (u, r))).collect();
    let old_user_by_id: HashMap<u64, &proto::User> = old_users.iter().map(|u| (user_id(u), u)).collect();

    // --- Users: removals. Prune from the *map*, not just the old roster, so
    // stale proxy sessions that drifted out of the cache are cleaned up too.
    // Only users with a proxy session were ever announced to Mumble clients.
    let new_user_ids: HashSet<u64> = new_users.iter().filter(|u| !skip.skips(u)).map(user_id).collect();
    let mut vanished: Vec<(u64, u32)> = users.entries().filter(|(id, _)| !new_user_ids.contains(id)).collect();
    vanished.sort_unstable();
    for &(rumble_id, session) in &vanished {
        users.remove_by_rumble_id(rumble_id);
        out.removed_user_ids.push(rumble_id);
        out.messages.push(SyncMessage::UserRemove(mumble::UserRemove {
            session,
            actor: None,
            reason: Some("Left".to_string()),
            ban: None,
            ban_certificate: None,
            ban_ip: None,
        }));
    }

    // --- Channels: additions and changes, BFS from the root so a parent's
    // ChannelState is always emitted (and its ID assigned) before any child
    // references it. Mirrors `build_channel_states`.
    let mut children_of: HashMap<Uuid, Vec<usize>> = HashMap::new();
    let mut new_uuids: Vec<Option<Uuid>> = Vec::new();
    for (idx, room) in new_rooms.iter().enumerate() {
        let uuid = room_uuid(room);
        new_uuids.push(uuid);
        if let Some(uuid) = uuid
            && uuid != rumble_protocol::ROOT_ROOM_UUID
        {
            children_of.entry(parent_uuid(room)).or_default().push(idx);
        }
    }
    let mut queue = VecDeque::new();
    if let Some(root_idx) = new_uuids
        .iter()
        .position(|u| *u == Some(rumble_protocol::ROOT_ROOM_UUID))
    {
        queue.push_back(root_idx);
    } else if let Some(children) = children_of.get(&rumble_protocol::ROOT_ROOM_UUID) {
        queue.extend(children.iter().copied());
    }
    while let Some(idx) = queue.pop_front() {
        let room = &new_rooms[idx];
        let Some(uuid) = new_uuids[idx] else { continue };
        let channel_id = channels.get_or_insert(uuid);
        let parent = (uuid != rumble_protocol::ROOT_ROOM_UUID).then(|| channels.get_or_insert(parent_uuid(room)));
        let changed = match old_room_by_uuid.get(&uuid) {
            None => true,
            Some(old) => {
                old.name != room.name || old.description != room.description || parent_uuid(old) != parent_uuid(room)
            }
        };
        if changed {
            out.messages.push(SyncMessage::ChannelState(mumble::ChannelState {
                channel_id: Some(channel_id),
                parent,
                name: Some(room.name.clone()),
                description: room.description.clone(),
                ..Default::default()
            }));
        }
        if let Some(children) = children_of.get(&uuid) {
            queue.extend(children.iter().copied());
        }
    }

    // --- Users: additions and changes. After channel adds so a user's new
    // channel already exists client-side.
    for user in new_users.iter().filter(|u| !skip.skips(u)) {
        let rumble_id = user_id(user);
        let channel_id = user
            .current_room
            .as_ref()
            .and_then(rumble_protocol::uuid_from_room_id)
            .map(|uuid| channels.get_or_insert(uuid))
            .unwrap_or(0);
        let changed = match (users.get_mumble_session(rumble_id), old_user_by_id.get(&rumble_id)) {
            // No proxy session yet: never announced to Mumble clients.
            (None, _) => true,
            // Has a session but fell out of the cached roster: re-announce.
            (Some(_), None) => true,
            (Some(_), Some(old)) => {
                old.username != user.username
                    || old.current_room != user.current_room
                    || old.is_muted != user.is_muted
                    || old.is_deafened != user.is_deafened
            }
        };
        let session = users.get_or_insert(rumble_id);
        if changed {
            out.messages.push(SyncMessage::UserState(mumble::UserState {
                session: Some(session),
                name: Some(user.username.clone()),
                channel_id: Some(channel_id),
                self_mute: Some(user.is_muted),
                self_deaf: Some(user.is_deafened),
                ..Default::default()
            }));
        }
    }

    // --- Channels: removals, deepest (by the old tree) first so a Mumble
    // client never sees a parent vanish while a known child still references
    // it. The root channel (0) is never removed.
    let new_uuid_set: HashSet<Uuid> = new_uuids.iter().flatten().copied().collect();
    let mut removed: Vec<(usize, u32, Uuid)> = channels
        .rumble_uuids()
        .filter(|uuid| *uuid != rumble_protocol::ROOT_ROOM_UUID && !new_uuid_set.contains(uuid))
        .map(|uuid| {
            let channel_id = channels.get_mumble_id(&uuid).expect("uuid came from the map");
            (old_depth(uuid, &old_room_by_uuid), channel_id, uuid)
        })
        .collect();
    removed.sort_unstable_by_key(|&(depth, channel_id, _)| (Reverse(depth), channel_id));
    for (_, channel_id, uuid) in removed {
        channels.remove_by_uuid(&uuid);
        out.messages
            .push(SyncMessage::ChannelRemove(mumble::ChannelRemove { channel_id }));
    }

    out
}

#[cfg(test)]
mod tests {
    use rumble_protocol::{ROOT_ROOM_UUID, room_id_from_uuid};

    use super::*;

    fn u(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn user(id: u64, name: &str, room: Uuid, muted: bool, deafened: bool) -> proto::User {
        proto::User {
            user_id: Some(proto::UserId { value: id }),
            username: name.to_string(),
            current_room: Some(room_id_from_uuid(room)),
            is_muted: muted,
            is_deafened: deafened,
            ..Default::default()
        }
    }

    fn room(uuid: Uuid, name: &str, parent: Option<Uuid>, description: Option<&str>) -> proto::RoomInfo {
        proto::RoomInfo {
            id: Some(room_id_from_uuid(uuid)),
            name: name.to_string(),
            parent_id: parent.map(room_id_from_uuid),
            description: description.map(str::to_string),
            ..Default::default()
        }
    }

    fn root() -> proto::RoomInfo {
        room(ROOT_ROOM_UUID, "Root", None, None)
    }

    /// Apply `rooms`/`users` as the baseline state, populating the maps.
    fn seed(rooms: &[proto::RoomInfo], users_list: &[proto::User], channels: &mut ChannelMap, users: &mut UserMap) {
        reconcile_server_state(&[], &[], rooms, users_list, channels, users, &SkipRules::default());
    }

    fn user_states(out: &ReconcileOutput) -> Vec<&mumble::UserState> {
        out.messages
            .iter()
            .filter_map(|m| match m {
                SyncMessage::UserState(us) => Some(us),
                _ => None,
            })
            .collect()
    }

    fn channel_states(out: &ReconcileOutput) -> Vec<&mumble::ChannelState> {
        out.messages
            .iter()
            .filter_map(|m| match m {
                SyncMessage::ChannelState(cs) => Some(cs),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn vanished_user_emits_remove_and_prunes_map() {
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        let old_rooms = vec![root()];
        let old_users = vec![user(1, "alice", ROOT_ROOM_UUID, false, false)];
        seed(&old_rooms, &old_users, &mut channels, &mut users);
        let session = users.get_mumble_session(1).expect("alice has a proxy session");

        let out = reconcile_server_state(
            &old_rooms,
            &old_users,
            &[root()],
            &[],
            &mut channels,
            &mut users,
            &SkipRules::default(),
        );

        assert_eq!(out.removed_user_ids, vec![1]);
        assert!(users.get_mumble_session(1).is_none(), "map entry pruned");
        let removes: Vec<_> = out
            .messages
            .iter()
            .filter_map(|m| match m {
                SyncMessage::UserRemove(ur) => Some(ur),
                _ => None,
            })
            .collect();
        assert_eq!(removes.len(), 1);
        assert_eq!(removes[0].session, session);
    }

    #[test]
    fn stale_map_entry_not_in_roster_is_pruned() {
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        seed(&[root()], &[], &mut channels, &mut users);
        // Simulate a proxy session that leaked past a missed UserLeft.
        let session = users.get_or_insert(42);

        let out = reconcile_server_state(
            &[root()],
            &[],
            &[root()],
            &[],
            &mut channels,
            &mut users,
            &SkipRules::default(),
        );

        assert_eq!(out.removed_user_ids, vec![42]);
        assert!(users.get_mumble_session(42).is_none());
        assert!(
            out.messages
                .iter()
                .any(|m| matches!(m, SyncMessage::UserRemove(ur) if ur.session == session))
        );
    }

    #[test]
    fn new_user_emits_user_state() {
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        seed(&[root()], &[], &mut channels, &mut users);

        let new_users = vec![user(7, "bob", ROOT_ROOM_UUID, false, false)];
        let out = reconcile_server_state(
            &[root()],
            &[],
            &[root()],
            &new_users,
            &mut channels,
            &mut users,
            &SkipRules::default(),
        );

        let states = user_states(&out);
        assert_eq!(states.len(), 1);
        let session = users.get_mumble_session(7).expect("bob got a proxy session");
        assert_eq!(states[0].session, Some(session));
        assert_eq!(states[0].name.as_deref(), Some("bob"));
        assert_eq!(states[0].channel_id, Some(0));
        assert_eq!(states[0].self_mute, Some(false));
        assert!(out.removed_user_ids.is_empty());
    }

    #[test]
    fn moved_and_renamed_user_emits_full_user_state() {
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        let rooms = vec![root(), room(u(1), "Lobby", None, None)];
        let old_users = vec![user(3, "carol", ROOT_ROOM_UUID, false, false)];
        seed(&rooms, &old_users, &mut channels, &mut users);
        let session = users.get_mumble_session(3).unwrap();
        let lobby_channel = channels.get_mumble_id(&u(1)).unwrap();

        let new_users = vec![user(3, "carol2", u(1), true, false)];
        let out = reconcile_server_state(
            &rooms,
            &old_users,
            &rooms,
            &new_users,
            &mut channels,
            &mut users,
            &SkipRules::default(),
        );

        let states = user_states(&out);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].session, Some(session), "proxy session is stable");
        assert_eq!(states[0].name.as_deref(), Some("carol2"));
        assert_eq!(states[0].channel_id, Some(lobby_channel));
        assert_eq!(states[0].self_mute, Some(true));
        assert!(channel_states(&out).is_empty(), "rooms unchanged");
    }

    #[test]
    fn vanished_rooms_remove_children_before_parents() {
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        let old_rooms = vec![
            root(),
            room(u(1), "Parent", None, None),
            room(u(2), "Child", Some(u(1)), None),
        ];
        seed(&old_rooms, &[], &mut channels, &mut users);
        let parent_channel = channels.get_mumble_id(&u(1)).unwrap();
        let child_channel = channels.get_mumble_id(&u(2)).unwrap();

        let out = reconcile_server_state(
            &old_rooms,
            &[],
            &[root()],
            &[],
            &mut channels,
            &mut users,
            &SkipRules::default(),
        );

        let removes: Vec<u32> = out
            .messages
            .iter()
            .filter_map(|m| match m {
                SyncMessage::ChannelRemove(cr) => Some(cr.channel_id),
                _ => None,
            })
            .collect();
        assert_eq!(removes, vec![child_channel, parent_channel], "child removed first");
        assert!(channels.get_mumble_id(&u(1)).is_none(), "map entries pruned");
        assert!(channels.get_mumble_id(&u(2)).is_none());
    }

    #[test]
    fn new_nested_rooms_emit_parent_before_child() {
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        seed(&[root()], &[], &mut channels, &mut users);

        let new_rooms = vec![
            root(),
            // Child listed before parent to prove ordering comes from the tree.
            room(u(2), "Child", Some(u(1)), None),
            room(u(1), "Parent", None, None),
        ];
        let out = reconcile_server_state(
            &[root()],
            &[],
            &new_rooms,
            &[],
            &mut channels,
            &mut users,
            &SkipRules::default(),
        );

        let states = channel_states(&out);
        assert_eq!(states.len(), 2);
        let parent_channel = channels.get_mumble_id(&u(1)).unwrap();
        let child_channel = channels.get_mumble_id(&u(2)).unwrap();
        assert_eq!(states[0].channel_id, Some(parent_channel), "parent first");
        assert_eq!(states[0].parent, Some(0));
        assert_eq!(states[1].channel_id, Some(child_channel));
        assert_eq!(states[1].parent, Some(parent_channel), "child references known parent");
    }

    #[test]
    fn renamed_moved_and_redescribed_rooms_emit_channel_state() {
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        let old_rooms = vec![
            root(),
            room(u(1), "A", None, None),
            room(u(2), "B", None, None),
            room(u(3), "C", None, Some("old desc")),
        ];
        seed(&old_rooms, &[], &mut channels, &mut users);
        let (a, b, c) = (
            channels.get_mumble_id(&u(1)).unwrap(),
            channels.get_mumble_id(&u(2)).unwrap(),
            channels.get_mumble_id(&u(3)).unwrap(),
        );

        let new_rooms = vec![
            root(),
            room(u(1), "A-renamed", None, None),
            room(u(2), "B", Some(u(1)), None),
            room(u(3), "C", None, Some("new desc")),
        ];
        let out = reconcile_server_state(
            &old_rooms,
            &[],
            &new_rooms,
            &[],
            &mut channels,
            &mut users,
            &SkipRules::default(),
        );

        let states = channel_states(&out);
        assert_eq!(states.len(), 3);
        let by_id = |id: u32| states.iter().find(|cs| cs.channel_id == Some(id)).unwrap();
        assert_eq!(by_id(a).name.as_deref(), Some("A-renamed"));
        assert_eq!(by_id(b).parent, Some(a), "B moved under A");
        assert_eq!(by_id(c).description.as_deref(), Some("new desc"));
        assert!(out.messages.iter().all(|m| matches!(m, SyncMessage::ChannelState(_))));
    }

    #[test]
    fn own_user_virtual_users_and_pending_registrations_are_skipped() {
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        seed(&[root()], &[], &mut channels, &mut users);

        let skip = SkipRules {
            bridge_user_id: Some(99),
            virtual_user_ids: HashSet::from([50]),
            pending_usernames: HashSet::from(["mumbler".to_string()]),
        };
        // Old state contains the previous connection's virtual user (id 60,
        // now re-registering under a pending username) — it must not produce
        // a UserRemove either, since it never had a proxy session.
        let old_users = vec![user(60, "mumbler", ROOT_ROOM_UUID, false, false)];
        let new_users = vec![
            user(99, "MumbleBridge", ROOT_ROOM_UUID, false, false),
            user(50, "registered-virtual", ROOT_ROOM_UUID, false, false),
            user(61, "mumbler", ROOT_ROOM_UUID, false, false),
        ];
        let out = reconcile_server_state(
            &[root()],
            &old_users,
            &[root()],
            &new_users,
            &mut channels,
            &mut users,
            &skip,
        );

        assert!(out.messages.is_empty(), "got: {:?}", out.messages);
        assert!(out.removed_user_ids.is_empty());
        for id in [99, 50, 60, 61] {
            assert!(
                users.get_mumble_session(id).is_none(),
                "user {id} must not get a proxy session"
            );
        }
    }

    #[test]
    fn unchanged_state_emits_nothing() {
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        let rooms = vec![root(), room(u(1), "Lobby", None, Some("desc"))];
        let roster = vec![user(1, "alice", u(1), true, true)];
        seed(&rooms, &roster, &mut channels, &mut users);

        let out = reconcile_server_state(
            &rooms,
            &roster,
            &rooms,
            &roster,
            &mut channels,
            &mut users,
            &SkipRules::default(),
        );

        assert!(out.messages.is_empty(), "got: {:?}", out.messages);
        assert!(out.removed_user_ids.is_empty());
    }

    #[test]
    fn message_sections_are_ordered_for_safe_application() {
        // user 1 vanishes, room R(1) vanishes, room S(2) appears, user 2
        // moves from R into S: removes-users -> add-channels -> move-users
        // -> remove-channels so the client never sees a dangling reference.
        let (mut channels, mut users) = (ChannelMap::new(), UserMap::new());
        let old_rooms = vec![root(), room(u(1), "R", None, None)];
        let old_users = vec![user(1, "alice", u(1), false, false), user(2, "bob", u(1), false, false)];
        seed(&old_rooms, &old_users, &mut channels, &mut users);
        let r_channel = channels.get_mumble_id(&u(1)).unwrap();

        let new_rooms = vec![root(), room(u(2), "S", None, None)];
        let new_users = vec![user(2, "bob", u(2), false, false)];
        let out = reconcile_server_state(
            &old_rooms,
            &old_users,
            &new_rooms,
            &new_users,
            &mut channels,
            &mut users,
            &SkipRules::default(),
        );

        let pos = |pred: fn(&SyncMessage) -> bool| out.messages.iter().position(pred).unwrap();
        let user_remove = pos(|m| matches!(m, SyncMessage::UserRemove(_)));
        let channel_add = pos(|m| matches!(m, SyncMessage::ChannelState(_)));
        let user_move = pos(|m| matches!(m, SyncMessage::UserState(_)));
        let channel_remove = pos(|m| matches!(m, SyncMessage::ChannelRemove(_)));
        assert!(user_remove < channel_add, "vacate before mutating channels");
        assert!(channel_add < user_move, "new channel exists before bob moves in");
        assert!(user_move < channel_remove, "bob has left R before R is removed");

        let s_channel = channels.get_mumble_id(&u(2)).unwrap();
        let states = user_states(&out);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].channel_id, Some(s_channel));
        assert!(matches!(
            &out.messages[channel_remove],
            SyncMessage::ChannelRemove(cr) if cr.channel_id == r_channel
        ));
        assert_eq!(out.removed_user_ids, vec![1]);
    }
}
