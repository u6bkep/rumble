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
    persistence: &Option<Arc<Persistence>>,
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
    persistence: &Option<Arc<Persistence>>,
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
    persistence: &Option<Arc<Persistence>>,
) -> Permissions {
    let is_superuser = sender.is_superuser.load(std::sync::atomic::Ordering::Relaxed);
    evaluate_identity_permissions(state, is_superuser, &sender.identity, room_uuid, persistence).await
}

/// Evaluate effective permissions for a roster member (client or participant).
pub async fn evaluate_member_permissions(
    state: &ServerState,
    member: &Member,
    room_uuid: Uuid,
    persistence: &Option<Arc<Persistence>>,
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
    persistence: &Option<Arc<Persistence>>,
) -> Permissions {
    let Some(persist) = persistence else {
        // No persistence = no ACL data, grant all permissions
        return Permissions::all();
    };

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
        if let Some(p) = Permissions::from_bits(pg.permissions) {
            group_perms.insert(name, p);
        }
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
