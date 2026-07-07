//! Hand-written domain model types for state that outlives a single
//! wire message.
//!
//! The prost-generated structs in [`crate::proto`] are wire types: every
//! message-typed field is `Option<T>` (proto3 presence), ids are
//! `Vec<u8>`-backed wrapper messages, and "no value" sentinels (`0`,
//! empty bytes) are implicit. Using them as the domain model forces
//! every consumer to re-unwrap and re-validate the same invariants.
//!
//! The types here encode those invariants **once**, at the wire
//! boundary:
//!
//! - Ids are plain [`Uuid`] / `u64` — never `Option<RoomId>`.
//! - [`Room::parent`] is non-optional. The Root room self-parents
//!   (`parent == id == nil`); every other room has a real parent, and
//!   "top-level" is exactly `parent == ROOT_ROOM_UUID`. A parentless
//!   non-root room is unrepresentable.
//! - [`RegisteredUser::public_key`] is `[u8; 32]`, validated once.
//! - [`GroupMembership::expires_at`] is `Option<u64>` instead of the
//!   wire's `0 == permanent` sentinel.
//!
//! Conversions **from** proto are fallible where the wire could carry
//! garbage (missing/malformed id ⇒ the record is dropped by the
//! boundary, matching the old `and_then(uuid_from_room_id)` filtering).
//! Notably, a malformed id must never *default* — defaulting to nil
//! would alias the Root room. Conversions **to** proto are infallible.
//!
//! Invariant for consumers: the client `State` and the server
//! `StateData` hold only these types; prost types appear solely in
//! transport/handler code that converts at the edge.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ROOT_ROOM_UUID, proto, room_id_from_uuid, uuid_from_room_id};

// =============================================================================
// Room
// =============================================================================

/// A room in the server's tree. See the module docs for the parent
/// invariant: Root self-parents at nil, everything else has a real
/// parent, top-level means `parent == ROOT_ROOM_UUID`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Room {
    pub id: Uuid,
    pub name: String,
    /// Parent room UUID. Never dangling on the server; the Root room
    /// self-parents (`parent == nil`). Tree walks terminate on
    /// `id == nil`, not on a missing link.
    pub parent: Uuid,
    /// Human-readable description; `None` when unset.
    pub description: Option<String>,
    /// Whether ACL evaluation inherits entries from the parent chain.
    pub inherit_acl: bool,
    /// Per-room ACL delta entries, in evaluation order.
    pub acls: Vec<RoomAclEntry>,
    /// Effective permission bitmask for the *receiving* client,
    /// computed per-client by the server; excluded from the state hash.
    pub effective_permissions: u32,
}

impl Room {
    pub fn is_root(&self) -> bool {
        self.id == ROOT_ROOM_UUID
    }

    /// Whether this room sits directly under Root (or *is* Root, which
    /// self-parents).
    pub fn is_top_level(&self) -> bool {
        self.parent == ROOT_ROOM_UUID
    }

    /// Convert from the wire type. `None` when the wire record has a
    /// missing or malformed `id` — the record is dropped, never
    /// defaulted (a nil default would alias Root). A missing or
    /// malformed `parent_id` means "top-level" and becomes Root, which
    /// is also how Root itself acquires its self-parent.
    pub fn from_proto(p: proto::RoomInfo) -> Option<Self> {
        let id = p.id.as_ref().and_then(uuid_from_room_id)?;
        let parent = p
            .parent_id
            .as_ref()
            .and_then(uuid_from_room_id)
            .unwrap_or(ROOT_ROOM_UUID);
        Some(Room {
            id,
            name: p.name,
            parent,
            description: p.description,
            inherit_acl: p.inherit_acl,
            acls: p.acls.into_iter().map(RoomAclEntry::from).collect(),
            effective_permissions: p.effective_permissions,
        })
    }

    pub fn to_proto(&self) -> proto::RoomInfo {
        proto::RoomInfo {
            id: Some(room_id_from_uuid(self.id)),
            name: self.name.clone(),
            // Root's self-parent round-trips as an explicit nil parent;
            // readers treat nil-parent and absent-parent identically.
            parent_id: Some(room_id_from_uuid(self.parent)),
            description: self.description.clone(),
            inherit_acl: self.inherit_acl,
            acls: self.acls.iter().map(RoomAclEntry::to_proto).collect(),
            effective_permissions: self.effective_permissions,
        }
    }
}

/// One grant/deny ACL delta on a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomAclEntry {
    pub group: String,
    pub grant: u32,
    pub deny: u32,
    pub apply_here: bool,
    pub apply_subs: bool,
}

impl From<proto::RoomAclEntry> for RoomAclEntry {
    fn from(p: proto::RoomAclEntry) -> Self {
        RoomAclEntry {
            group: p.group,
            grant: p.grant,
            deny: p.deny,
            apply_here: p.apply_here,
            apply_subs: p.apply_subs,
        }
    }
}

impl RoomAclEntry {
    pub fn to_proto(&self) -> proto::RoomAclEntry {
        proto::RoomAclEntry {
            group: self.group.clone(),
            grant: self.grant,
            deny: self.deny,
            apply_here: self.apply_here,
            apply_subs: self.apply_subs,
        }
    }
}

// =============================================================================
// User
// =============================================================================

/// A connected user (or controller-owned participant).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub user_id: u64,
    pub username: String,
    /// The room the user is in. Always a real room: the server places
    /// every user in Root at connect, so there is no room-less state.
    pub current_room: Uuid,
    /// Self-muted (not transmitting).
    pub is_muted: bool,
    /// Self-deafened (not receiving; implies muted).
    pub is_deafened: bool,
    /// Server-muted (cannot transmit).
    pub server_muted: bool,
    /// Elevated to superuser via sudo this session.
    pub is_elevated: bool,
    /// Permission groups (populated for self always; for all users when
    /// the viewer holds MANAGE_ACL).
    pub groups: Vec<String>,
    /// Display marker minted by a controller/plugin (e.g. "Mumble",
    /// "bot"). `None` for humans — this Option is semantic, not wire
    /// ceremony.
    pub label: Option<String>,
}

impl User {
    /// Convert from the wire type. `None` when `user_id` is absent —
    /// such a record is undeliverable garbage and is dropped. A missing
    /// `current_room` defaults to Root, mirroring where the server
    /// places every fresh connection.
    pub fn from_proto(p: proto::User) -> Option<Self> {
        let user_id = p.user_id.as_ref()?.value;
        let current_room = p
            .current_room
            .as_ref()
            .and_then(uuid_from_room_id)
            .unwrap_or(ROOT_ROOM_UUID);
        Some(User {
            user_id,
            username: p.username,
            current_room,
            is_muted: p.is_muted,
            is_deafened: p.is_deafened,
            server_muted: p.server_muted,
            is_elevated: p.is_elevated,
            groups: p.groups,
            label: p.label,
        })
    }

    pub fn to_proto(&self) -> proto::User {
        proto::User {
            user_id: Some(proto::UserId { value: self.user_id }),
            username: self.username.clone(),
            current_room: Some(room_id_from_uuid(self.current_room)),
            is_muted: self.is_muted,
            is_deafened: self.is_deafened,
            server_muted: self.server_muted,
            is_elevated: self.is_elevated,
            groups: self.groups.clone(),
            label: self.label.clone(),
        }
    }
}

// =============================================================================
// Groups / slash commands
// =============================================================================

/// A server-wide permission group definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    /// Permission bitmask granted by membership (see
    /// [`crate::permissions::Permissions`]).
    pub permissions: u32,
    pub is_builtin: bool,
}

impl From<proto::GroupInfo> for Group {
    fn from(p: proto::GroupInfo) -> Self {
        Group {
            name: p.name,
            permissions: p.permissions,
            is_builtin: p.is_builtin,
        }
    }
}

impl Group {
    pub fn to_proto(&self) -> proto::GroupInfo {
        proto::GroupInfo {
            name: self.name.clone(),
            permissions: self.permissions,
            is_builtin: self.is_builtin,
        }
    }
}

/// A slash command advertised by the server for composer autocomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommand {
    /// Keyword without the leading slash (e.g. "echo").
    pub name: String,
    pub description: String,
}

impl From<proto::SlashCommand> for SlashCommand {
    fn from(p: proto::SlashCommand) -> Self {
        SlashCommand {
            name: p.name,
            description: p.description,
        }
    }
}

impl SlashCommand {
    pub fn to_proto(&self) -> proto::SlashCommand {
        proto::SlashCommand {
            name: self.name.clone(),
            description: self.description.clone(),
        }
    }
}

// =============================================================================
// Registered users (admin roster)
// =============================================================================

/// A single group membership. `expires_at == None` is permanent (the
/// wire encodes permanence as `0`; the Option makes the sentinel
/// explicit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMembership {
    pub group: String,
    pub expires_at: Option<u64>,
}

impl From<proto::GroupMembership> for GroupMembership {
    fn from(p: proto::GroupMembership) -> Self {
        GroupMembership {
            group: p.group,
            expires_at: (p.expires_at != 0).then_some(p.expires_at),
        }
    }
}

impl GroupMembership {
    pub fn to_proto(&self) -> proto::GroupMembership {
        proto::GroupMembership {
            group: self.group.clone(),
            expires_at: self.expires_at.unwrap_or(0),
        }
    }
}

/// A registered identity (possibly offline) with its group memberships —
/// the admin roster entry pulled via `RequestRegisteredUsers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredUser {
    /// Ed25519 public key, validated to exactly 32 bytes at the wire
    /// boundary.
    pub public_key: [u8; 32],
    pub username: String,
    pub groups: Vec<GroupMembership>,
    /// Whether this identity has a live connection right now.
    pub online: bool,
}

impl RegisteredUser {
    /// Convert from the wire type. `None` when the public key is not
    /// exactly 32 bytes — an unaddressable record, dropped.
    pub fn from_proto(p: proto::RegisteredUserInfo) -> Option<Self> {
        let public_key: [u8; 32] = p.public_key.as_slice().try_into().ok()?;
        Some(RegisteredUser {
            public_key,
            username: p.username,
            groups: p.groups.into_iter().map(GroupMembership::from).collect(),
            online: p.online,
        })
    }

    pub fn to_proto(&self) -> proto::RegisteredUserInfo {
        proto::RegisteredUserInfo {
            public_key: self.public_key.to_vec(),
            username: self.username.clone(),
            groups: self.groups.iter().map(GroupMembership::to_proto).collect(),
            online: self.online,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_missing_id_is_dropped_not_defaulted() {
        let p = proto::RoomInfo {
            id: None,
            name: "ghost".into(),
            ..Default::default()
        };
        assert!(Room::from_proto(p).is_none());
    }

    #[test]
    fn room_missing_parent_becomes_root_child() {
        let id = Uuid::new_v4();
        let p = proto::RoomInfo {
            id: Some(room_id_from_uuid(id)),
            name: "lobby".into(),
            parent_id: None,
            ..Default::default()
        };
        let r = Room::from_proto(p).unwrap();
        assert_eq!(r.parent, ROOT_ROOM_UUID);
        assert!(r.is_top_level());
        assert!(!r.is_root());
    }

    #[test]
    fn root_self_parent_round_trips() {
        let root = Room {
            id: ROOT_ROOM_UUID,
            name: "Root".into(),
            parent: ROOT_ROOM_UUID,
            description: None,
            inherit_acl: true,
            acls: vec![],
            effective_permissions: 0,
        };
        let back = Room::from_proto(root.to_proto()).unwrap();
        assert!(back.is_root());
        assert_eq!(back.parent, ROOT_ROOM_UUID);
        assert_eq!(back, root);
    }

    #[test]
    fn user_missing_id_is_dropped() {
        let p = proto::User {
            user_id: None,
            username: "ghost".into(),
            ..Default::default()
        };
        assert!(User::from_proto(p).is_none());
    }

    #[test]
    fn membership_expiry_sentinel_maps_to_option() {
        let permanent = GroupMembership::from(proto::GroupMembership {
            group: "admin".into(),
            expires_at: 0,
        });
        assert_eq!(permanent.expires_at, None);
        let timed = GroupMembership::from(proto::GroupMembership {
            group: "banned".into(),
            expires_at: 12345,
        });
        assert_eq!(timed.expires_at, Some(12345));
        assert_eq!(timed.to_proto().expires_at, 12345);
        assert_eq!(permanent.to_proto().expires_at, 0);
    }

    #[test]
    fn registered_user_bad_key_dropped() {
        let p = proto::RegisteredUserInfo {
            public_key: vec![0u8; 31],
            username: "short".into(),
            groups: vec![],
            online: false,
        };
        assert!(RegisteredUser::from_proto(p).is_none());
    }
}
