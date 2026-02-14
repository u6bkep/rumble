//! Permission flags and ACL evaluation for the Rumble ACL system.

use std::collections::HashMap;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Permissions: u32 {
        // Room-scoped
        const TRAVERSE      = 0x001;
        const ENTER         = 0x002;
        const SPEAK         = 0x004;
        const TEXT_MESSAGE   = 0x008;
        const SHARE_FILE    = 0x010;
        const MUTE_DEAFEN   = 0x020;
        const MOVE_USER     = 0x040;
        const MAKE_ROOM     = 0x080;
        const MODIFY_ROOM   = 0x100;
        const WRITE          = 0x200;

        // Server-scoped (only meaningful at root)
        const KICK           = 0x10000;
        const BAN            = 0x20000;
        const REGISTER       = 0x40000;
        const SELF_REGISTER  = 0x80000;
        const MANAGE_ACL     = 0x100000;
        const SUDO           = 0x200000;
    }
}

/// All room-scoped permission bits OR'd together.
pub const ALL_ROOM_SCOPED: Permissions = Permissions::TRAVERSE
    .union(Permissions::ENTER)
    .union(Permissions::SPEAK)
    .union(Permissions::TEXT_MESSAGE)
    .union(Permissions::SHARE_FILE)
    .union(Permissions::MUTE_DEAFEN)
    .union(Permissions::MOVE_USER)
    .union(Permissions::MAKE_ROOM)
    .union(Permissions::MODIFY_ROOM)
    .union(Permissions::WRITE);

/// All server-scoped permission bits OR'd together.
pub const ALL_SERVER_SCOPED: Permissions = Permissions::KICK
    .union(Permissions::BAN)
    .union(Permissions::REGISTER)
    .union(Permissions::SELF_REGISTER)
    .union(Permissions::MANAGE_ACL)
    .union(Permissions::SUDO);

/// All permission bits.
pub const ALL: Permissions = ALL_ROOM_SCOPED.union(ALL_SERVER_SCOPED);

/// Default permissions for new users (the "default" group).
pub const DEFAULT_PERMISSIONS: Permissions = Permissions::TRAVERSE
    .union(Permissions::ENTER)
    .union(Permissions::SPEAK)
    .union(Permissions::TEXT_MESSAGE)
    .union(Permissions::SHARE_FILE)
    .union(Permissions::SELF_REGISTER);

/// Admin permissions (all bits set).
pub const ADMIN_PERMISSIONS: Permissions = ALL;

/// A named permission group with a base permission set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionGroup {
    pub name: String,
    pub permissions: Permissions,
}

/// ACL data for a single room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomAclData {
    pub inherit_acl: bool,
    pub entries: Vec<AclEntry>,
}

/// A single ACL entry: grant/deny delta for a group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclEntry {
    pub group: String,
    pub grant: Permissions,
    pub deny: Permissions,
    pub apply_here: bool,
    pub apply_subs: bool,
}

/// Compute effective permissions for a user in a target room.
///
/// - `user_groups` - names of all groups the user belongs to (includes "default" and their username)
/// - `group_permissions` - map of group name to base permissions
/// - `room_chain` - ordered from root to target room, each with its ACL data
/// - `is_superuser` - if true, bypass all checks and return ALL
pub fn effective_permissions(
    user_groups: &[String],
    group_permissions: &HashMap<String, Permissions>,
    room_chain: &[(Uuid, Option<&RoomAclData>)],
    is_superuser: bool,
) -> Permissions {
    // 1. Superuser bypasses everything
    if is_superuser {
        return ALL;
    }

    // 2. Base = union of all user's group permissions
    let mut base = Permissions::empty();
    for group_name in user_groups {
        if let Some(&perms) = group_permissions.get(group_name) {
            base |= perms;
        }
    }

    // 3. Walk room chain root -> target
    let mut granted = base;
    let last_idx = room_chain.len().saturating_sub(1);

    for (idx, (_room_uuid, acl_data)) in room_chain.iter().enumerate() {
        let is_target = idx == last_idx;

        if let Some(acl) = acl_data {
            // Reset to base if inherit_acl is false
            if !acl.inherit_acl {
                granted = base;
            }

            // Apply ACL entries in order
            for entry in &acl.entries {
                // Skip if user is not in this group
                if !user_groups.contains(&entry.group) {
                    continue;
                }

                // Skip based on apply_here / apply_subs
                if is_target && !entry.apply_here {
                    continue;
                }
                if !is_target && !entry.apply_subs {
                    continue;
                }

                granted |= entry.grant;
                granted &= !entry.deny;
            }
        }

        // If TRAVERSE not in granted and this is not the target room, abort
        if !is_target && !granted.contains(Permissions::TRAVERSE) {
            return Permissions::empty();
        }
    }

    // 5. Server-scoped permissions only count when target is root
    if room_chain.len() > 1 {
        granted &= !ALL_SERVER_SCOPED;
    }

    granted
}

/// Check if the effective permissions contain the required permission(s).
pub fn has_permission(effective: Permissions, required: Permissions) -> bool {
    effective.contains(required)
}

// Implement Serialize/Deserialize for Permissions as a u32
impl Serialize for Permissions {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.bits().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Permissions {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bits = u32::deserialize(deserializer)?;
        Permissions::from_bits(bits)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid permission bits: 0x{bits:x}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_permissions() {
        assert!(DEFAULT_PERMISSIONS.contains(Permissions::TRAVERSE));
        assert!(DEFAULT_PERMISSIONS.contains(Permissions::ENTER));
        assert!(DEFAULT_PERMISSIONS.contains(Permissions::SPEAK));
        assert!(DEFAULT_PERMISSIONS.contains(Permissions::TEXT_MESSAGE));
        assert!(DEFAULT_PERMISSIONS.contains(Permissions::SHARE_FILE));
        assert!(DEFAULT_PERMISSIONS.contains(Permissions::SELF_REGISTER));
        assert!(!DEFAULT_PERMISSIONS.contains(Permissions::KICK));
        assert!(!DEFAULT_PERMISSIONS.contains(Permissions::BAN));
    }

    #[test]
    fn test_admin_permissions() {
        assert_eq!(ADMIN_PERMISSIONS, ALL);
        assert!(ADMIN_PERMISSIONS.contains(Permissions::KICK));
        assert!(ADMIN_PERMISSIONS.contains(Permissions::SUDO));
    }

    #[test]
    fn test_superuser_bypass() {
        let groups = vec!["default".to_string()];
        let group_perms = HashMap::new();
        let room_chain = vec![(Uuid::nil(), None)];

        let result = effective_permissions(&groups, &group_perms, &room_chain, true);
        assert_eq!(result, ALL);
    }

    #[test]
    fn test_basic_group_permissions() {
        let groups = vec!["default".to_string()];
        let mut group_perms = HashMap::new();
        group_perms.insert("default".to_string(), DEFAULT_PERMISSIONS);

        let room_chain = vec![(Uuid::nil(), None)];
        let result = effective_permissions(&groups, &group_perms, &room_chain, false);
        assert_eq!(result, DEFAULT_PERMISSIONS);
    }

    #[test]
    fn test_multi_group_union() {
        let groups = vec!["default".to_string(), "moderator".to_string()];
        let mut group_perms = HashMap::new();
        group_perms.insert("default".to_string(), DEFAULT_PERMISSIONS);
        group_perms.insert(
            "moderator".to_string(),
            Permissions::MUTE_DEAFEN | Permissions::MOVE_USER,
        );

        let room_chain = vec![(Uuid::nil(), None)];
        let result = effective_permissions(&groups, &group_perms, &room_chain, false);
        assert!(result.contains(DEFAULT_PERMISSIONS));
        assert!(result.contains(Permissions::MUTE_DEAFEN));
        assert!(result.contains(Permissions::MOVE_USER));
    }

    #[test]
    fn test_room_acl_deny() {
        let groups = vec!["default".to_string()];
        let mut group_perms = HashMap::new();
        group_perms.insert("default".to_string(), DEFAULT_PERMISSIONS);

        let acl = RoomAclData {
            inherit_acl: true,
            entries: vec![AclEntry {
                group: "default".to_string(),
                grant: Permissions::empty(),
                deny: Permissions::SPEAK | Permissions::MOVE_USER,
                apply_here: true,
                apply_subs: true,
            }],
        };

        let room_id = Uuid::new_v4();
        let room_chain = vec![(Uuid::nil(), None), (room_id, Some(&acl))];
        let result = effective_permissions(&groups, &group_perms, &room_chain, false);
        assert!(!result.contains(Permissions::SPEAK));
        assert!(result.contains(Permissions::ENTER));
    }

    #[test]
    fn test_traverse_denied_aborts() {
        let groups = vec!["default".to_string()];
        let mut group_perms = HashMap::new();
        group_perms.insert("default".to_string(), DEFAULT_PERMISSIONS);

        let deny_traverse = RoomAclData {
            inherit_acl: true,
            entries: vec![AclEntry {
                group: "default".to_string(),
                grant: Permissions::empty(),
                deny: Permissions::TRAVERSE,
                apply_here: true,
                apply_subs: true,
            }],
        };

        let mid_room = Uuid::new_v4();
        let target_room = Uuid::new_v4();
        let room_chain = vec![
            (Uuid::nil(), None),
            (mid_room, Some(&deny_traverse)),
            (target_room, None),
        ];
        let result = effective_permissions(&groups, &group_perms, &room_chain, false);
        assert_eq!(result, Permissions::empty());
    }

    #[test]
    fn test_inherit_acl_false_resets() {
        let groups = vec!["default".to_string()];
        let mut group_perms = HashMap::new();
        group_perms.insert("default".to_string(), DEFAULT_PERMISSIONS);

        // Parent grants MUTE_DEAFEN via ACL
        let parent_acl = RoomAclData {
            inherit_acl: true,
            entries: vec![AclEntry {
                group: "default".to_string(),
                grant: Permissions::MUTE_DEAFEN,
                deny: Permissions::empty(),
                apply_here: true,
                apply_subs: true,
            }],
        };

        // Child resets inheritance
        let child_acl = RoomAclData {
            inherit_acl: false,
            entries: vec![],
        };

        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let room_chain = vec![
            (Uuid::nil(), None),
            (parent, Some(&parent_acl)),
            (child, Some(&child_acl)),
        ];
        let result = effective_permissions(&groups, &group_perms, &room_chain, false);
        // Should be reset to base (DEFAULT_PERMISSIONS), not including MUTE_DEAFEN
        assert!(!result.contains(Permissions::MUTE_DEAFEN));
        assert!(result.contains(Permissions::SPEAK));
    }

    #[test]
    fn test_server_scoped_only_at_root() {
        let groups = vec!["admin".to_string()];
        let mut group_perms = HashMap::new();
        group_perms.insert("admin".to_string(), ALL);

        // At root: server-scoped permissions should be present
        let root_chain = vec![(Uuid::nil(), None)];
        let result = effective_permissions(&groups, &group_perms, &root_chain, false);
        assert!(result.contains(Permissions::KICK));
        assert!(result.contains(Permissions::BAN));

        // At a child room: server-scoped permissions should be stripped
        let child = Uuid::new_v4();
        let child_chain = vec![(Uuid::nil(), None), (child, None)];
        let result = effective_permissions(&groups, &group_perms, &child_chain, false);
        assert!(!result.contains(Permissions::KICK));
        assert!(!result.contains(Permissions::BAN));
        assert!(result.contains(Permissions::SPEAK));
    }

    #[test]
    fn test_has_permission() {
        let perms = Permissions::SPEAK | Permissions::ENTER;
        assert!(has_permission(perms, Permissions::SPEAK));
        assert!(has_permission(perms, Permissions::ENTER));
        assert!(!has_permission(perms, Permissions::KICK));
        assert!(!has_permission(perms, Permissions::SPEAK | Permissions::KICK));
    }

    #[test]
    fn test_apply_here_only() {
        let groups = vec!["default".to_string()];
        let mut group_perms = HashMap::new();
        group_perms.insert("default".to_string(), DEFAULT_PERMISSIONS);

        let acl = RoomAclData {
            inherit_acl: true,
            entries: vec![AclEntry {
                group: "default".to_string(),
                grant: Permissions::MUTE_DEAFEN,
                deny: Permissions::empty(),
                apply_here: true,
                apply_subs: false,
            }],
        };

        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();

        // When parent is an ancestor (not target), apply_here=true,apply_subs=false => skip
        let room_chain = vec![(Uuid::nil(), None), (parent, Some(&acl)), (child, None)];
        let result = effective_permissions(&groups, &group_perms, &room_chain, false);
        assert!(!result.contains(Permissions::MUTE_DEAFEN));

        // When parent is the target, apply_here=true => applies
        let room_chain_target = vec![(Uuid::nil(), None), (parent, Some(&acl))];
        let result = effective_permissions(&groups, &group_perms, &room_chain_target, false);
        assert!(result.contains(Permissions::MUTE_DEAFEN));
    }

    #[test]
    fn test_permissions_serde_roundtrip() {
        let perms = DEFAULT_PERMISSIONS;
        let json = serde_json::to_string(&perms).unwrap();
        let deserialized: Permissions = serde_json::from_str(&json).unwrap();
        assert_eq!(perms, deserialized);
    }
}
