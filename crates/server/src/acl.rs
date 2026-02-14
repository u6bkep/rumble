//! ACL (Access Control List) evaluation and permission checking.
//!
//! This module provides the server-side permission evaluation logic,
//! wrapping the core evaluation function from the api crate with
//! server-specific state lookups.

use std::{collections::HashMap, sync::Arc};

use api::{
    ROOT_ROOM_UUID,
    permissions::{AclEntry, Permissions, RoomAclData, effective_permissions, has_permission},
    uuid_from_room_id,
};
use uuid::Uuid;

use crate::{
    persistence::Persistence,
    state::{ClientHandle, ServerState},
};

/// Check whether a user has the required permission in a room.
///
/// Returns `Ok(())` if permitted, or `Err(PermissionDenied)` with details.
///
/// Checks superuser status first (bypasses all ACL), then evaluates
/// effective permissions through the room chain.
pub async fn check_permission(
    state: &ServerState,
    sender: &Arc<ClientHandle>,
    persistence: &Persistence,
    room_uuid: Uuid,
    required: Permissions,
) -> Result<(), PermissionDenied> {
    // Superuser bypasses everything
    if sender.is_superuser.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }

    let effective = evaluate_user_permissions(state, sender, persistence, room_uuid).await;

    if has_permission(effective, required) {
        Ok(())
    } else {
        Err(PermissionDenied {
            required_permission: required.bits(),
            room_uuid,
            message: format!("Missing permission 0x{:x} in room {}", required.bits(), room_uuid),
        })
    }
}

/// Compute the effective permissions for a user in a specific room.
///
/// Builds the room chain from root to target, loads group permissions
/// and room ACLs from persistence, then delegates to the core
/// `effective_permissions()` function from the api crate.
pub async fn evaluate_user_permissions(
    state: &ServerState,
    sender: &Arc<ClientHandle>,
    persistence: &Persistence,
    room_uuid: Uuid,
) -> Permissions {
    let is_superuser = sender.is_superuser.load(std::sync::atomic::Ordering::SeqCst);

    // Build user's group list: always includes "default" + their explicit groups
    let explicit_groups = sender.groups.read().await;
    let mut user_groups: Vec<String> = vec!["default".to_string()];
    for g in explicit_groups.iter() {
        if g != "default" {
            user_groups.push(g.clone());
        }
    }

    // Add username-as-group (implicit group for per-user ACL overrides)
    let username = sender.get_username().await;
    if !username.is_empty() {
        user_groups.push(username);
    }

    // Load group permissions from persistence
    let mut group_permissions: HashMap<String, Permissions> = HashMap::new();
    for group_name in &user_groups {
        if let Some(pg) = persistence.get_group(group_name) {
            if let Some(perms) = Permissions::from_bits(pg.permissions) {
                group_permissions.insert(group_name.clone(), perms);
            }
        }
    }

    // Build room chain from root to target
    let room_chain = build_room_chain(state, room_uuid).await;

    // Load room ACLs for each room in the chain
    let mut acl_data_storage: Vec<Option<RoomAclData>> = Vec::with_capacity(room_chain.len());
    for &chain_room_uuid in &room_chain {
        let acl = persistence
            .get_room_acl(&chain_room_uuid.into_bytes())
            .map(|persisted| RoomAclData {
                inherit_acl: persisted.inherit_acl,
                entries: persisted
                    .entries
                    .into_iter()
                    .map(|e| AclEntry {
                        group: e.group,
                        grant: Permissions::from_bits_truncate(e.grant),
                        deny: Permissions::from_bits_truncate(e.deny),
                        apply_here: e.apply_here,
                        apply_subs: e.apply_subs,
                    })
                    .collect(),
            });
        acl_data_storage.push(acl);
    }

    // Build the room chain with references to ACL data
    let chain_with_acls: Vec<(Uuid, Option<&RoomAclData>)> = room_chain
        .iter()
        .zip(acl_data_storage.iter())
        .map(|(&uuid, acl_opt)| (uuid, acl_opt.as_ref()))
        .collect();

    effective_permissions(&user_groups, &group_permissions, &chain_with_acls, is_superuser)
}

/// Build the room chain from root to the target room.
///
/// Walks parent_id pointers from target to root, then reverses.
/// If the target room is the root, returns a single-element chain.
async fn build_room_chain(state: &ServerState, target_room: Uuid) -> Vec<Uuid> {
    if target_room == ROOT_ROOM_UUID {
        return vec![ROOT_ROOM_UUID];
    }

    let rooms = state.get_rooms().await;

    // Build a map of UUID -> parent UUID for quick lookup
    let mut parent_map: HashMap<Uuid, Option<Uuid>> = HashMap::new();
    for room in &rooms {
        if let Some(ref room_id) = room.id {
            if let Some(uuid) = uuid_from_room_id(room_id) {
                let parent = room.parent_id.as_ref().and_then(uuid_from_room_id);
                parent_map.insert(uuid, parent);
            }
        }
    }

    // Walk from target to root
    let mut chain = Vec::new();
    let mut current = target_room;
    let mut visited = std::collections::HashSet::new();

    loop {
        if !visited.insert(current) {
            // Cycle detected, break
            break;
        }
        chain.push(current);

        if current == ROOT_ROOM_UUID {
            break;
        }

        match parent_map.get(&current) {
            Some(Some(parent)) => current = *parent,
            _ => {
                // No parent found, assume root is implicit parent
                chain.push(ROOT_ROOM_UUID);
                break;
            }
        }
    }

    // Reverse so it goes root -> target
    chain.reverse();
    chain
}

/// Permission denied error with details.
#[derive(Debug, Clone)]
pub struct PermissionDenied {
    pub required_permission: u32,
    pub room_uuid: Uuid,
    pub message: String,
}
