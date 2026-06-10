//! Sender-free admin mutation cores.
//!
//! Each `apply_*` function performs the *post-authorization* mutation for one
//! admin operation: persist → mutate in-memory [`ServerState`] → broadcast the
//! change to connected clients (and force-disconnect for kick/ban). They take
//! no `ClientHandle` and run no permission checks, so the same logic is shared
//! by two callers:
//!
//! - the QUIC protocol handlers in [`crate::handlers`], which authorize via
//!   `acl::check_permission` against the requesting client and then delegate
//!   here, and
//! - the web control-plane in [`crate::web`], which authorizes via an admin
//!   session (the sudo password) and then delegates here.
//!
//! This keeps the mutation/broadcast invariants encoded exactly once.
//!
//! Convention: every function returns `Result<T, String>` where `Err(msg)`
//! carries a user-facing failure reason (e.g. "User not found"). Protocol
//! handlers map this onto `CommandResult { success, message }`; the web layer
//! maps `Err` onto an HTTP error body. `Ok` carries either a success message or
//! a value the caller needs (e.g. the new room UUID).

use crate::{
    handlers::{broadcast_state_update, cleanup_client},
    persistence::{PersistedAclEntry, PersistedBan, PersistedRoom, PersistedRoomAcl, Persistence, RegisteredUser},
    state::ServerState,
};
use rumble_protocol::{encode_frame, permissions::Permissions, proto, proto::envelope::Payload, room_id_from_uuid};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{error, info, warn};
use uuid::Uuid;

/// Built-in group names that may not be created or deleted.
const BUILTIN_GROUPS: [&str; 2] = ["default", "admin"];

// =============================================================================
// User moderation
// =============================================================================

/// Explain why `target_user_id` has no moderatable client connection. A
/// controller-driven participant (e.g. a bridged Mumble user) exists as a
/// [`crate::state::Member`] but has no `ClientHandle` of its own, so direct
/// `{action}` can't reach it — it must be moderated through its controller.
/// Anything else is genuinely absent.
fn not_a_client_reason(state: &Arc<ServerState>, target_user_id: u64, action: &str) -> String {
    if state.get_member(target_user_id).is_some() {
        format!("Cannot {action} a controller-driven participant directly; moderate it through its controller")
    } else {
        "User not found".to_string()
    }
}

/// Kick a connected user: notify them, close their connection, and broadcast
/// their departure. `actor` is the human-readable name recorded as the kicker.
///
/// Rejects kicking superusers and controller (bridge) connections. Self-kick
/// protection is the caller's concern (it is sender-relative).
pub(crate) async fn apply_kick(
    state: &Arc<ServerState>,
    target_user_id: u64,
    reason: &str,
    actor: &str,
) -> Result<String, String> {
    let target_client = state
        .get_client(target_user_id)
        .ok_or_else(|| not_a_client_reason(state, target_user_id, "kick"))?;

    if target_client.is_superuser.load(Ordering::Relaxed) {
        return Err("Cannot kick a superuser".to_string());
    }
    if target_client.is_controller.load(Ordering::SeqCst) {
        return Err("Cannot kick a bridge connection".to_string());
    }

    let target_username = target_client.get_username().await;
    info!(kicked_by = %actor, target = %target_username, reason = %reason, "KickUser");

    let kicked_env = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::UserKicked(proto::UserKicked {
            user_id: target_user_id,
            reason: reason.to_string(),
            kicked_by: actor.to_string(),
        })),
    };
    let frame = encode_frame(&kicked_env);
    let _ = target_client.send_frame(&frame).await;

    target_client.conn.close(quinn::VarInt::from_u32(2), b"kicked");
    cleanup_client(&target_client, state).await;

    Ok(format!("Kicked '{}'", target_username))
}

/// Ban a connected user: add them to the `banned` group (creating it if
/// needed), persist a timed ban record, then disconnect them. The `BANNED`
/// flag is checked at auth time to reject future connections.
///
/// `duration_seconds == 0` means a permanent ban.
pub(crate) async fn apply_ban(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    target_user_id: u64,
    duration_seconds: u64,
    reason: &str,
    actor: &str,
) -> Result<String, String> {
    let target_client = state
        .get_client(target_user_id)
        .ok_or_else(|| not_a_client_reason(state, target_user_id, "ban"))?;

    if target_client.is_superuser.load(Ordering::Relaxed) {
        return Err("Cannot ban a superuser".to_string());
    }
    if target_client.is_controller.load(Ordering::SeqCst) {
        return Err("Cannot ban a bridge connection".to_string());
    }

    let persist = persistence;

    // Ensure "banned" group exists with the BANNED permission flag.
    if persist.get_group("banned").is_none() {
        let _ = persist.create_group("banned", Permissions::BANNED.bits());
    }

    // Add target to "banned" group and persist a timed ban record.
    let target_key = state
        .get_user_public_key(target_user_id)
        .ok_or_else(|| "Cannot resolve user's public key".to_string())?;

    let _ = persist.add_user_to_group(&target_key, "banned");

    let banned_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ban_record = PersistedBan {
        banned_at_secs: Some(banned_at_secs),
        duration_seconds,
        reason: reason.to_string(),
        banned_by: actor.to_string(),
    };
    if let Err(e) = persist.set_ban(&target_key, &ban_record) {
        warn!("Failed to persist ban record: {e}");
    }

    // Update cached groups on the live connection.
    {
        let mut groups = target_client.identity.groups().await;
        if !groups.contains(&"banned".to_string()) {
            groups.push("banned".to_string());
            target_client.identity.set_groups(groups).await;
        }
    }

    let target_username = target_client.get_username().await;
    let duration_desc = if duration_seconds == 0 {
        "permanent".to_string()
    } else {
        format!("{}s", duration_seconds)
    };
    info!(banned_by = %actor, target = %target_username, reason = %reason, duration = %duration_desc, "BanUser");

    // Notify the target with a ban message, then disconnect.
    let ban_reason = if reason.is_empty() {
        format!("Banned by {} ({})", actor, duration_desc)
    } else {
        format!("Banned by {} ({}): {}", actor, duration_desc, reason)
    };
    let kicked_env = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::UserKicked(proto::UserKicked {
            user_id: target_user_id,
            reason: ban_reason,
            kicked_by: actor.to_string(),
        })),
    };
    let frame = encode_frame(&kicked_env);
    let _ = target_client.send_frame(&frame).await;

    target_client.conn.close(quinn::VarInt::from_u32(3), b"banned");
    cleanup_client(&target_client, state).await;

    Ok(format!("Banned '{}' ({})", target_username, duration_desc))
}

// =============================================================================
// User registration
// =============================================================================

/// Register the connected user `target_user_id` under their current username.
pub(crate) async fn apply_register_user(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    target_user_id: u64,
) -> Result<String, String> {
    let persist = persistence;

    let target_key = state
        .get_user_public_key(target_user_id)
        .ok_or_else(|| "User not found".to_string())?;
    let target_client = state
        .get_client(target_user_id)
        .ok_or_else(|| "User not found".to_string())?;
    let username = target_client.get_username().await;

    if persist.get_registered_user(&target_key).is_some() {
        return Err("User already registered".to_string());
    }

    // Username-as-group invariant: a username becomes an implicit ACL group, so
    // it must not collide with an existing permission group (builtin or custom).
    // Otherwise registering as e.g. "admin" would silently grant that group's
    // permissions. Symmetric to the check in apply_create_group.
    if BUILTIN_GROUPS.contains(&username.as_str()) || persist.get_group(&username).is_some() {
        return Err("Username conflicts with an existing permission group".to_string());
    }

    persist
        .register_user(
            &target_key,
            RegisteredUser {
                username: username.clone(),
                last_room: None,
            },
        )
        .map_err(|e| format!("Registration failed: {e}"))?;

    Ok(format!("Registered '{}' successfully", username))
}

/// Unregister the connected user `target_user_id`.
pub(crate) async fn apply_unregister_user(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    target_user_id: u64,
) -> Result<String, String> {
    let persist = persistence;

    let target_key = state
        .get_user_public_key(target_user_id)
        .ok_or_else(|| "User not found".to_string())?;
    let username = match state.get_client(target_user_id) {
        Some(c) => c.get_username().await,
        None => "user".to_string(),
    };

    persist
        .unregister_user(&target_key)
        .map_err(|e| format!("Unregistration failed: {e}"))?;

    Ok(format!("Unregistered '{}' successfully", username))
}

// =============================================================================
// Rooms
// =============================================================================

/// Create a room under `parent_uuid` (root if `None`) and broadcast it.
/// Returns the new room's UUID.
pub(crate) async fn apply_create_room(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    name: String,
    parent_uuid: Option<Uuid>,
    description: Option<String>,
) -> Result<Uuid, String> {
    info!("CreateRoom: {}", name);

    // Reject an unknown parent: creating a room under a non-existent parent
    // would persist an unreachable orphan (invisible to any parent-walking
    // room tree and with a truncated ACL chain).
    if let Some(parent) = parent_uuid
        && !state.room_exists(parent).await
    {
        return Err("Parent room does not exist".to_string());
    }

    let room_uuid = state
        .create_room_with_parent_desc(name.clone(), parent_uuid, description.clone())
        .await;

    {
        let persist = persistence;
        let room = PersistedRoom {
            name: name.clone(),
            parent: parent_uuid.map(|u| *u.as_bytes()),
            description: description.clone().unwrap_or_default(),
            permanent: true,
        };
        if let Err(e) = persist.save_room(&room_uuid.into_bytes(), &room) {
            warn!("Failed to persist room: {e}");
        }
    }

    let room_info = proto::RoomInfo {
        id: Some(room_id_from_uuid(room_uuid)),
        name,
        parent_id: parent_uuid.map(room_id_from_uuid),
        description,
        inherit_acl: true,
        acls: vec![],
        effective_permissions: 0,
    };
    broadcast_state_update(
        state,
        proto::state_update::Update::RoomCreated(proto::RoomCreated { room: Some(room_info) }),
    )
    .await
    .map_err(|e| format!("Failed to broadcast room creation: {e}"))?;

    Ok(room_uuid)
}

/// Delete a non-root room, moving its members to root, and broadcast the
/// removal. Returns the deleted room's name.
pub(crate) async fn apply_delete_room(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    room_uuid: Uuid,
) -> Result<String, String> {
    if room_uuid == rumble_protocol::ROOT_ROOM_UUID {
        return Err("Cannot delete the Root room".to_string());
    }

    let room_name = state
        .get_rooms()
        .await
        .iter()
        .find(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) == Some(room_uuid))
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "room".to_string());

    info!("DeleteRoom: {}", room_uuid);

    // Heal the tree before removing the room: reparent its direct children to
    // the deleted room's own parent (Root if it had none), so no subtree is
    // orphaned with a dangling parent_id. Each child is reparented as its own
    // move+persist+broadcast step — mirroring the MoveRoom handler — so the
    // per-step incremental state hash stays correct.
    if let Some((parent, children)) = state.room_parent_and_children(room_uuid).await {
        let new_parent = parent.unwrap_or(rumble_protocol::ROOT_ROOM_UUID);
        for child in children {
            state.move_room(child, new_parent).await;

            let child_bytes = child.into_bytes();
            if let Some(mut room) = persistence.get_room(&child_bytes) {
                room.parent = Some(new_parent.into_bytes());
                if let Err(e) = persistence.save_room(&child_bytes, &room) {
                    warn!("Failed to persist child reparent for {child}: {e}");
                }
            }

            broadcast_state_update(
                state,
                proto::state_update::Update::RoomMoved(proto::RoomMoved {
                    room_id: Some(room_id_from_uuid(child)),
                    new_parent_id: Some(room_id_from_uuid(new_parent)),
                }),
            )
            .await
            .map_err(|e| format!("Failed to broadcast child reparent: {e}"))?;
        }
    }

    // Capture who is in the room before deletion — delete_room moves them to
    // Root, where their SPEAK (and thus auto server-mute) must be re-evaluated.
    let displaced_members = state.get_room_members(room_uuid).await;

    if !state.delete_room(room_uuid).await {
        return Err("Room not found".to_string());
    }

    {
        let persist = persistence;
        if let Err(e) = persist.delete_room(&room_uuid.into_bytes()) {
            warn!("Failed to remove room from persistence: {e}");
        }
        if let Err(e) = persist.delete_room_acl(&room_uuid.into_bytes()) {
            warn!("Failed to remove room ACL data from persistence: {e}");
        }
    }

    broadcast_state_update(
        state,
        proto::state_update::Update::RoomDeleted(proto::RoomDeleted {
            room_id: Some(room_id_from_uuid(room_uuid)),
            fallback_room_id: Some(room_id_from_uuid(rumble_protocol::ROOT_ROOM_UUID)),
        }),
    )
    .await
    .map_err(|e| format!("Failed to broadcast room deletion: {e}"))?;

    // The displaced members are now in Root; re-evaluate their auto server-mute
    // against Root's SPEAK so a mute inherited from the (SPEAK-denied) deleted
    // room doesn't stick until they manually rejoin.
    for member_id in displaced_members {
        reevaluate_member_mute(state, persistence, member_id, rumble_protocol::ROOT_ROOM_UUID).await;
    }

    Ok(room_name)
}

// =============================================================================
// Permission groups
// =============================================================================

/// Create a permission group and broadcast it.
pub(crate) async fn apply_create_group(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    name: String,
    permissions: u32,
) -> Result<String, String> {
    let persist = persistence;

    if name.is_empty() {
        return Err("Group name cannot be empty".to_string());
    }
    // `:` is reserved: admin clients embed group names in `:`-separated
    // routing keys, where a colon inside the name silently corrupts
    // per-group operations.
    if name.contains(':') {
        return Err("Group name cannot contain ':'".to_string());
    }
    if BUILTIN_GROUPS.contains(&name.as_str()) {
        return Err("Cannot create built-in groups".to_string());
    }
    if persist.get_group(&name).is_some() {
        return Err("Group already exists".to_string());
    }
    // Username-as-group invariant: a group name must not collide with a
    // registered username.
    if persist.is_username_registered(&name) {
        return Err("Group name conflicts with a registered username".to_string());
    }

    persist
        .create_group(&name, permissions)
        .map_err(|e| format!("Failed: {e}"))?;

    info!(group = %name, permissions, "CreateGroup");

    let _ = broadcast_state_update(
        state,
        proto::state_update::Update::GroupChanged(proto::GroupChanged {
            group: Some(proto::GroupInfo {
                name: name.clone(),
                permissions,
                is_builtin: false,
            }),
            deleted: false,
        }),
    )
    .await;

    Ok(format!("Created group '{}'", name))
}

/// Delete a permission group, scrubbing it from every connected client and
/// every persisted user-group list, then broadcast the deletion.
pub(crate) async fn apply_delete_group(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    name: String,
) -> Result<String, String> {
    let persist = persistence;

    if BUILTIN_GROUPS.contains(&name.as_str()) {
        return Err("Cannot delete built-in groups".to_string());
    }

    info!(group = %name, "DeleteGroup");

    // Scrub from connected clients first, broadcasting each removal. Doing this
    // before the group delete keeps the crash window safe (group still in sled
    // → delete retryable).
    let clients = state.snapshot_clients();
    for client in &clients {
        let mut groups = client.identity.groups().await;
        if groups.contains(&name) {
            groups.retain(|g| g != &name);
            client.identity.set_groups(groups).await;
            let _ = broadcast_state_update(
                state,
                proto::state_update::Update::UserGroupChanged(proto::UserGroupChanged {
                    user_id: client.user_id,
                    group: name.clone(),
                    added: false,
                    expires_at: 0,
                }),
            )
            .await;
        }
    }

    persist.remove_group_from_all_users(&name);

    persist.delete_group(&name).map_err(|e| format!("Failed: {e}"))?;

    let _ = broadcast_state_update(
        state,
        proto::state_update::Update::GroupChanged(proto::GroupChanged {
            group: Some(proto::GroupInfo {
                name: name.clone(),
                permissions: 0,
                is_builtin: false,
            }),
            deleted: true,
        }),
    )
    .await;

    Ok(format!("Deleted group '{}'", name))
}

/// Change a group's permission bits and broadcast the change.
pub(crate) async fn apply_modify_group(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    name: String,
    permissions: u32,
) -> Result<String, String> {
    let persist = persistence;

    match persist.modify_group(&name, permissions) {
        Ok(false) => return Err("Group not found".to_string()),
        Err(e) => return Err(format!("Failed: {e}")),
        Ok(true) => {}
    }

    info!(group = %name, permissions, "ModifyGroup");

    let _ = broadcast_state_update(
        state,
        proto::state_update::Update::GroupChanged(proto::GroupChanged {
            group: Some(proto::GroupInfo {
                name: name.clone(),
                permissions,
                is_builtin: BUILTIN_GROUPS.contains(&name.as_str()),
            }),
            deleted: false,
        }),
    )
    .await;

    Ok(format!("Modified group '{}'", name))
}

/// Add or remove `target_user_id` to/from `group`. Group membership is keyed by
/// the user's long-term public key, so this resolves the key from the live
/// session and delegates to [`apply_set_user_group_by_key`]. Used by the QUIC
/// handler and the web by-id endpoint, both of which target a connected user.
pub(crate) async fn apply_set_user_group(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    target_user_id: u64,
    group: String,
    add: bool,
    expires_at: u64,
) -> Result<String, String> {
    let key = state
        .get_user_public_key(target_user_id)
        .ok_or_else(|| "User is not connected".to_string())?;
    apply_set_user_group_by_key(state, persistence, key, group, add, expires_at).await
}

/// Add or remove the identity owning `public_key` to/from `group`: persist the
/// change, mirror it onto every live connection authenticated with that key
/// (so their cached permissions update without a reconnect), and broadcast.
///
/// This is the canonical group-membership mutation — membership lives in
/// persistence keyed by the public key, and a live connection's cached
/// `identity.groups()` is a derived view kept in sync here. Works for offline
/// registered users too (no live connection to mirror onto, nothing to
/// broadcast).
pub(crate) async fn apply_set_user_group_by_key(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    public_key: [u8; 32],
    group: String,
    add: bool,
    expires_at: u64,
) -> Result<String, String> {
    let persist = persistence;

    // Validate the target group before mutating anything. Only the `add` path
    // needs this: removal is idempotent cleanup (stripping a possibly-stale
    // membership), but adding to a bogus group is a silent foot-gun.
    if add {
        // A registered username is an *implicit* identity group, not a
        // manageable one — granting it would hand the user every per-room ACL
        // entry targeting that identity.
        if persist.is_username_registered(&group) {
            return Err(format!("'{group}' is a registered username, not a group"));
        }
        // Reject typos: the group must actually exist (builtin or persisted).
        if !BUILTIN_GROUPS.contains(&group.as_str()) && persist.get_group(&group).is_none() {
            return Err(format!("Group '{group}' does not exist"));
        }
    }

    // Persist first and propagate failures: a swallowed write would leave only
    // a session-only grant (mirrored below) that vanishes on reconnect/restart
    // while the admin saw success.
    if add {
        persist
            .add_user_to_group(&public_key, &group)
            .map_err(|e| format!("Failed to persist group membership: {e}"))?;
    } else {
        persist
            .remove_user_from_group(&public_key, &group)
            .map_err(|e| format!("Failed to persist group membership: {e}"))?;
    }

    // Mirror onto any live connection(s) sharing this key, and broadcast the
    // change so other clients see the membership update.
    for client in state.snapshot_clients() {
        if state.get_user_public_key(client.user_id) != Some(public_key) {
            continue;
        }
        let mut groups = client.identity.groups().await;
        let before = groups.len();
        if add {
            if !groups.contains(&group) {
                groups.push(group.clone());
            }
        } else {
            groups.retain(|g| g != &group);
        }
        if groups.len() != before {
            client.identity.set_groups(groups).await;
        }
        let _ = broadcast_state_update(
            state,
            proto::state_update::Update::UserGroupChanged(proto::UserGroupChanged {
                user_id: client.user_id,
                group: group.clone(),
                added: add,
                expires_at,
            }),
        )
        .await;
    }

    let action = if add { "Added" } else { "Removed" };
    info!(group = %group, action, "SetUserGroup");

    Ok(format!(
        "{} user {} group '{}'",
        action,
        if add { "to" } else { "from" },
        group
    ))
}

// =============================================================================
// Room ACLs
// =============================================================================

/// Replace a room's ACL entries, persist them, broadcast the change, and
/// re-evaluate `SPEAK` for every member in the room (server-muting those who
/// lost it, unmuting those who regained it unless manually muted).
pub(crate) async fn apply_set_room_acl(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    room_uuid: Uuid,
    inherit_acl: bool,
    entries: Vec<proto::RoomAclEntry>,
) -> Result<String, String> {
    let persist = persistence;

    let persisted_acl = PersistedRoomAcl {
        inherit_acl,
        entries: entries
            .iter()
            .map(|e| PersistedAclEntry {
                group: e.group.clone(),
                grant: e.grant,
                deny: e.deny,
                apply_here: e.apply_here,
                apply_subs: e.apply_subs,
            })
            .collect(),
    };

    // Apply in-memory first; set_room_acl returns false if the room doesn't
    // exist. Bail before persisting/broadcasting so we don't leave a garbage
    // sled ACL record and announce an ACL for a room no client has.
    if !state.set_room_acl(room_uuid, inherit_acl, entries.clone()).await {
        return Err("Room not found".to_string());
    }

    if let Err(e) = persist.set_room_acl(room_uuid.as_bytes(), &persisted_acl) {
        error!(room = %room_uuid, "Failed to persist room ACL: {e:?}");
        return Err("Failed to persist ACL".to_string());
    }

    info!(room = %room_uuid, entries = entries.len(), "SetRoomAcl");

    broadcast_state_update(
        state,
        proto::state_update::Update::RoomAclChanged(proto::RoomAclChanged {
            room_id: Some(room_id_from_uuid(room_uuid)),
            inherit_acl,
            entries: entries.clone(),
        }),
    )
    .await
    .map_err(|e| format!("Failed to broadcast ACL change: {e}"))?;

    // Re-evaluate SPEAK for everyone currently in the room.
    for member_id in state.get_room_members(room_uuid).await {
        reevaluate_member_mute(state, persistence, member_id, room_uuid).await;
    }

    Ok("Room ACL updated".to_string())
}

/// Re-evaluate `member_id`'s `SPEAK` permission in `room` and update its
/// `server_muted` state, broadcasting any change. Server-mutes a member that
/// lacks SPEAK; unmutes one that regained it (unless manually muted). A no-op
/// for participants with no live connection. Used whenever a member's effective
/// SPEAK can change without a deliberate join: an ACL edit, or a forced move to
/// Root when their room is deleted (so a stale auto-mute doesn't persist).
async fn reevaluate_member_mute(state: &Arc<ServerState>, persistence: &Arc<Persistence>, member_id: u64, room: Uuid) {
    let Some(client) = state.get_client(member_id) else {
        return;
    };
    let speak_perms = crate::acl::evaluate_user_permissions(state, &client, room, persistence).await;
    let speak_denied = !speak_perms.contains(Permissions::SPEAK);
    let manually_muted = client.manually_server_muted.load(Ordering::Relaxed);
    let should_mute = speak_denied || manually_muted;
    let was_muted = client.server_muted.load(Ordering::Relaxed);
    if should_mute == was_muted {
        return;
    }
    client.server_muted.store(should_mute, Ordering::Relaxed);
    let status = state.get_user_status(member_id).await;
    let _ = broadcast_state_update(
        state,
        proto::state_update::Update::UserStatusChanged(proto::UserStatusChanged {
            user_id: Some(proto::UserId { value: member_id }),
            is_muted: status.is_muted,
            is_deafened: status.is_deafened,
            server_muted: should_mute,
            is_elevated: client.is_superuser.load(Ordering::Relaxed),
        }),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumble_protocol::ROOT_ROOM_UUID;
    use uuid::Uuid;

    fn test_env() -> (Arc<ServerState>, Arc<Persistence>) {
        let state = Arc::new(ServerState::new());
        let persist = Arc::new(Persistence::in_memory().expect("in-memory persistence"));
        (state, persist)
    }

    /// Deleting a mid-tree room reparents its direct children to the deleted
    /// room's own parent instead of orphaning them with a dangling parent_id
    /// (#30). Persisted parent is updated too, so the heal survives restart.
    #[tokio::test]
    async fn delete_room_reparents_children_to_grandparent() {
        let (state, persist) = test_env();
        let parent = apply_create_room(&state, &persist, "parent".into(), None, None)
            .await
            .unwrap();
        let mid = apply_create_room(&state, &persist, "mid".into(), Some(parent), None)
            .await
            .unwrap();
        let child = apply_create_room(&state, &persist, "child".into(), Some(mid), None)
            .await
            .unwrap();

        apply_delete_room(&state, &persist, mid).await.unwrap();

        assert!(!state.room_exists(mid).await, "deleted room is gone");
        let (child_parent, _) = state.room_parent_and_children(child).await.unwrap();
        assert_eq!(
            child_parent,
            Some(parent),
            "child must be reparented to the grandparent, not orphaned under the deleted room"
        );

        let persisted = persist.get_room(&child.into_bytes()).expect("child still persisted");
        assert_eq!(
            persisted.parent,
            Some(parent.into_bytes()),
            "reparent must be persisted so it survives restart"
        );
    }

    /// A top-level room's children fall back to Root when it is deleted.
    #[tokio::test]
    async fn delete_top_level_room_reparents_children_to_root() {
        let (state, persist) = test_env();
        let top = apply_create_room(&state, &persist, "top".into(), None, None)
            .await
            .unwrap();
        let child = apply_create_room(&state, &persist, "child".into(), Some(top), None)
            .await
            .unwrap();

        apply_delete_room(&state, &persist, top).await.unwrap();

        let (child_parent, _) = state.room_parent_and_children(child).await.unwrap();
        assert_eq!(child_parent, Some(ROOT_ROOM_UUID), "child falls back to Root");
    }

    /// Creating a room under a non-existent parent is rejected, not persisted as
    /// an unreachable orphan (#30).
    #[tokio::test]
    async fn create_room_rejects_unknown_parent() {
        let (state, persist) = test_env();
        let ghost = Uuid::new_v4();

        let err = apply_create_room(&state, &persist, "orphan".into(), Some(ghost), None)
            .await
            .unwrap_err();
        assert!(err.contains("Parent room does not exist"), "got: {err}");
        assert_eq!(state.get_rooms().await.len(), 1, "nothing created besides Root");
    }

    /// Adding a user to a typo'd / non-existent group is rejected instead of
    /// silently "succeeding" (#35).
    #[tokio::test]
    async fn set_user_group_rejects_unknown_group() {
        let (state, persist) = test_env();
        persist.ensure_default_groups().unwrap();
        let key = [7u8; 32];

        let err = apply_set_user_group_by_key(&state, &persist, key, "admni".into(), true, 0)
            .await
            .unwrap_err();
        assert!(err.contains("does not exist"), "got: {err}");
        assert!(
            persist.get_user_groups(&key).is_empty(),
            "no membership should have been written"
        );
    }

    /// A registered username is an implicit identity group and must not be
    /// grantable as a managed group (#35).
    #[tokio::test]
    async fn set_user_group_rejects_username_as_group() {
        let (state, persist) = test_env();
        persist.ensure_default_groups().unwrap();
        persist
            .register_user(
                &[1u8; 32],
                crate::persistence::RegisteredUser {
                    username: "alice".into(),
                    last_room: None,
                },
            )
            .unwrap();
        let key = [7u8; 32];

        let err = apply_set_user_group_by_key(&state, &persist, key, "alice".into(), true, 0)
            .await
            .unwrap_err();
        assert!(err.contains("registered username"), "got: {err}");
        assert!(persist.get_user_groups(&key).is_empty());
    }

    /// Adding an offline user to a real group persists the membership.
    #[tokio::test]
    async fn set_user_group_adds_existing_group() {
        let (state, persist) = test_env();
        persist.ensure_default_groups().unwrap();
        let key = [7u8; 32];

        apply_set_user_group_by_key(&state, &persist, key, "admin".into(), true, 0)
            .await
            .unwrap();
        assert_eq!(
            persist.get_user_groups(&key),
            vec!["admin".to_string()],
            "membership persisted"
        );
    }
}
