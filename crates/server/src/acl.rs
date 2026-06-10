//! ACL permission checking for the Rumble server.
//!
//! This module provides real implementations of ACL evaluation using
//! the persistence layer for group definitions and room ACL data.

use crate::{
    persistence::Persistence,
    state::{ClientHandle, Identity, Member, ServerState},
};
use rumble_protocol::permissions::{self, AclEntry, Permissions, RoomAclData};
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

use rumble_protocol::proto;

/// Check if a connected client has the required permission in a room.
/// Returns Ok(()) if allowed, Err(PermissionDenied) if not.
pub async fn check_permission(
    state: &ServerState,
    sender: &ClientHandle,
    room_uuid: Uuid,
    required: Permissions,
    persistence: &Arc<Persistence>,
) -> Result<(), proto::PermissionDenied> {
    let effective = evaluate_user_permissions(state, sender, room_uuid, persistence).await;
    require(effective, required, room_uuid)
}

/// Check if a roster member (client *or* participant) has the required
/// permission in a room.
pub async fn check_member_permission(
    state: &ServerState,
    member: &Member,
    room_uuid: Uuid,
    required: Permissions,
    persistence: &Arc<Persistence>,
) -> Result<(), proto::PermissionDenied> {
    let effective = evaluate_member_permissions(state, member, room_uuid, persistence).await;
    require(effective, required, room_uuid)
}

fn require(effective: Permissions, required: Permissions, room_uuid: Uuid) -> Result<(), proto::PermissionDenied> {
    if effective.contains(required) {
        Ok(())
    } else {
        Err(proto::PermissionDenied {
            required_permission: required.bits(),
            room_id: room_uuid.as_bytes().to_vec(),
            message: format!(
                "You do not have permission to perform this action (requires {:?})",
                required
            ),
        })
    }
}

/// Evaluate effective permissions for a connected client in a room.
pub async fn evaluate_user_permissions(
    state: &ServerState,
    sender: &ClientHandle,
    room_uuid: Uuid,
    persistence: &Arc<Persistence>,
) -> Permissions {
    let is_superuser = sender.is_superuser.load(std::sync::atomic::Ordering::Relaxed);
    evaluate_identity_permissions(state, is_superuser, &sender.identity, room_uuid, persistence).await
}

/// Evaluate effective permissions for a roster member (client or participant).
pub async fn evaluate_member_permissions(
    state: &ServerState,
    member: &Member,
    room_uuid: Uuid,
    persistence: &Arc<Persistence>,
) -> Permissions {
    evaluate_identity_permissions(state, member.is_superuser(), &member.identity, room_uuid, persistence).await
}

/// Core ACL evaluation, operating purely on identity (groups + verified
/// username) and superuser status. The implicit username-group is derived from
/// the member's *verified* identity name, never from a display name; members
/// with no verified identity (anonymous participants) get no implicit group.
async fn evaluate_identity_permissions(
    state: &ServerState,
    is_superuser: bool,
    identity: &Identity,
    room_uuid: Uuid,
    persistence: &Arc<Persistence>,
) -> Permissions {
    let persist = persistence;

    // Build user's group list: always in "default" + their explicitly assigned
    // groups + their verified username as an implicit group (if verified).
    let mut user_groups = vec!["default".to_string()];
    for g in identity.groups().await {
        if !user_groups.contains(&g) {
            user_groups.push(g);
        }
    }
    // Add verified username as implicit group. Anonymous participants have no
    // verified identity, so they never gain a username-keyed group.
    if let Some(username) = identity.verified_username().await
        && !user_groups.contains(&username)
    {
        user_groups.push(username);
    }

    // Build group permissions map from persistence
    let mut group_perms: HashMap<String, Permissions> = HashMap::new();
    for (name, pg) in persist.list_groups() {
        // Truncate undefined bits rather than dropping the whole group: with
        // from_bits a single unknown bit would silently zero a group's perms
        // (e.g. un-banning everyone in "banned"). Matches the ACL-entry path.
        group_perms.insert(name, Permissions::from_bits_truncate(pg.permissions));
    }

    // Build room chain from root to target room
    let owned_chain = build_room_chain_owned(state, room_uuid, persist).await;
    // Convert to borrowed references for effective_permissions()
    let ref_chain: Vec<(Uuid, Option<&RoomAclData>)> =
        owned_chain.iter().map(|(uuid, acl)| (*uuid, acl.as_ref())).collect();

    permissions::effective_permissions(&user_groups, &group_perms, &ref_chain, is_superuser)
}

/// Build the room chain from root to the target room with owned ACL data.
async fn build_room_chain_owned(
    state: &ServerState,
    target: Uuid,
    persist: &Persistence,
) -> Vec<(Uuid, Option<RoomAclData>)> {
    // Build the path from target to root by walking parent links
    let rooms = state.get_rooms().await;
    let mut path = Vec::new();
    let mut current = target;

    loop {
        path.push(current);
        if current == Uuid::nil() {
            break;
        }
        // Find parent of current room
        let parent = rooms.iter().find_map(|r| {
            let rid = r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
            if rid == current {
                r.parent_id.as_ref().and_then(rumble_protocol::uuid_from_room_id)
            } else {
                None
            }
        });
        match parent {
            Some(parent_uuid) => current = parent_uuid,
            None => break,
        }
    }

    // Reverse to get root-to-target order
    path.reverse();

    // Ensure root is in the chain
    if path.first() != Some(&Uuid::nil()) {
        path.insert(0, Uuid::nil());
    }

    // Deduplicate root if target IS root
    path.dedup();

    // Load ACL data for each room in the chain
    path.into_iter()
        .map(|room_uuid| {
            let acl_data = persist.get_room_acl(room_uuid.as_bytes()).map(|pacl| RoomAclData {
                inherit_acl: pacl.inherit_acl,
                entries: pacl
                    .entries
                    .iter()
                    .map(|e| AclEntry {
                        group: e.group.clone(),
                        grant: Permissions::from_bits_truncate(e.grant),
                        deny: Permissions::from_bits_truncate(e.deny),
                        apply_here: e.apply_here,
                        apply_subs: e.apply_subs,
                    })
                    .collect(),
            });
            (room_uuid, acl_data)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! Characterization tests for the *server ACL wiring* — the work this
    //! module does on top of `permissions::effective_permissions` (which has
    //! its own algorithm tests in `rumble-protocol`). What's pinned here:
    //! how the user's group list is assembled (default + identity groups +
    //! the implicit *verified-username* group), that an unverified display
    //! name never mints an implicit group, and that the room chain is walked
    //! from root to target so parent grants are inherited.
    use super::*;
    use crate::{
        persistence::{PersistedAclEntry, PersistedRoomAcl, Persistence},
        state::{Binding, Identity, Member, OwnerId, ServerState},
    };
    use rumble_protocol::{
        ROOT_ROOM_UUID,
        permissions::{DEFAULT_PERMISSIONS, Permissions},
    };

    fn persistence_with_default() -> Arc<Persistence> {
        let p = Persistence::in_memory().unwrap();
        p.create_group("default", DEFAULT_PERMISSIONS.bits()).unwrap();
        Arc::new(p)
    }

    /// An owned (controller-driven) participant carrying the given identity.
    fn member(uid: u64, display: &str, verified: Option<&str>, groups: &[&str]) -> Member {
        Member {
            user_id: uid,
            identity: Arc::new(Identity::participant(
                display.to_string(),
                verified.map(str::to_string),
                None,
                groups.iter().map(|s| s.to_string()).collect(),
            )),
            binding: Binding::Owned {
                owner: OwnerId::Plugin(1),
            },
        }
    }

    /// Put a single grant entry on the root room's ACL.
    fn grant_at_root(p: &Persistence, group: &str, grant: Permissions, apply_subs: bool) {
        p.set_room_acl(
            ROOT_ROOM_UUID.as_bytes(),
            &PersistedRoomAcl {
                inherit_acl: true,
                entries: vec![PersistedAclEntry {
                    group: group.to_string(),
                    grant: grant.bits(),
                    deny: 0,
                    apply_here: true,
                    apply_subs,
                }],
            },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn default_group_only_yields_default_permissions() {
        let state = ServerState::new();
        let persist = persistence_with_default();
        let m = member(1, "anon", None, &[]);

        let perms = evaluate_member_permissions(&state, &m, ROOT_ROOM_UUID, &persist).await;
        assert_eq!(perms, DEFAULT_PERMISSIONS);
    }

    #[tokio::test]
    async fn verified_username_acts_as_implicit_group() {
        let state = ServerState::new();
        let persist = persistence_with_default();
        // The root ACL grants MUTE_DEAFEN to a group named "alice".
        grant_at_root(&persist, "alice", Permissions::MUTE_DEAFEN, false);

        // alice, verified, picks the grant up via her implicit username-group.
        let alice = member(1, "alice", Some("alice"), &[]);
        let aperms = evaluate_member_permissions(&state, &alice, ROOT_ROOM_UUID, &persist).await;
        assert_eq!(aperms, DEFAULT_PERMISSIONS | Permissions::MUTE_DEAFEN);

        // bob, verified as someone else, does not.
        let bob = member(2, "bob", Some("bob"), &[]);
        let bperms = evaluate_member_permissions(&state, &bob, ROOT_ROOM_UUID, &persist).await;
        assert_eq!(bperms, DEFAULT_PERMISSIONS);
    }

    #[tokio::test]
    async fn unverified_display_name_is_not_an_implicit_group() {
        // Security property: an anonymous participant whose *display* name is
        // "alice" must NOT inherit the "alice" group's grants — only a
        // *verified* identity mints the implicit username-group.
        let state = ServerState::new();
        let persist = persistence_with_default();
        grant_at_root(&persist, "alice", Permissions::MUTE_DEAFEN, false);

        let impostor = member(1, "alice", None, &[]);
        let perms = evaluate_member_permissions(&state, &impostor, ROOT_ROOM_UUID, &persist).await;
        assert_eq!(
            perms, DEFAULT_PERMISSIONS,
            "an unverified display name must not mint an implicit group"
        );
    }

    #[tokio::test]
    async fn parent_grant_inherited_through_room_chain() {
        // A grant placed on root with apply_subs must reach a grandchild —
        // exercises the root→target parent-walk in build_room_chain_owned.
        let state = ServerState::new();
        let persist = persistence_with_default();
        grant_at_root(&persist, "default", Permissions::MUTE_DEAFEN, true);

        let child = state
            .create_room_with_parent("child".into(), Some(ROOT_ROOM_UUID))
            .await;
        let grandchild = state.create_room_with_parent("grandchild".into(), Some(child)).await;

        let m = member(1, "anon", None, &[]);
        let perms = evaluate_member_permissions(&state, &m, grandchild, &persist).await;

        // The room-scoped MUTE_DEAFEN grant is inherited down the chain; the
        // server-scoped SELF_REGISTER bit from the default group is stripped
        // because server-scoped permissions only apply at the root room.
        let expected = (DEFAULT_PERMISSIONS & rumble_protocol::permissions::ALL_ROOM_SCOPED) | Permissions::MUTE_DEAFEN;
        assert_eq!(
            perms, expected,
            "an apply_subs grant at root must be inherited by descendants"
        );
    }

    #[tokio::test]
    async fn explicit_group_membership_unions_in() {
        // An explicitly-assigned group's permissions union into the base.
        let state = ServerState::new();
        let persist = persistence_with_default();
        persist
            .create_group("moderators", Permissions::MUTE_DEAFEN.bits())
            .unwrap();

        let m = member(1, "anon", None, &["moderators"]);
        let perms = evaluate_member_permissions(&state, &m, ROOT_ROOM_UUID, &persist).await;
        assert_eq!(perms, DEFAULT_PERMISSIONS | Permissions::MUTE_DEAFEN);
    }
}
