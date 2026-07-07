//! Read-only monitoring endpoints: live state snapshot, group and room lists.
//! All require an admin session.

use super::{ApiResult, WebState, auth::Admin};
use axum::{Json, extract::State};
use rumble_protocol::{ROOT_ROOM_UUID, Room};
use rumble_web_types::{AclEntryDto, GroupDto, RegisteredUserDto, RoomDto, StateSnapshot, UserDto};
use std::{collections::HashSet, sync::atomic::Ordering};

const BUILTIN_GROUPS: [&str; 2] = ["default", "admin"];

/// Render a domain room into a wire DTO, including its ACL entries.
fn room_dto(r: &Room) -> RoomDto {
    // A top-level room parents to Root; surface that as no parent (the DTO's
    // `parent_id` is optional), matching the pre-domain wire behavior.
    let parent_id = (r.parent != ROOT_ROOM_UUID).then(|| r.parent.to_string());
    let acls = r
        .acls
        .iter()
        .map(|e| AclEntryDto {
            group: e.group.clone(),
            grant: e.grant,
            deny: e.deny,
            apply_here: e.apply_here,
            apply_subs: e.apply_subs,
        })
        .collect();
    // Optimistic-concurrency token the ACL editor echoes back on save. Computed
    // from the same values the DTO carries via the room's wire encoding, so what
    // the editor loads is exactly what the version covers.
    let acl_version = rumble_protocol::room_acl_version(r.inherit_acl, &r.to_proto().acls);
    RoomDto {
        id: r.id.to_string(),
        name: r.name.clone(),
        parent_id,
        description: r.description.clone(),
        inherit_acl: r.inherit_acl,
        acls,
        acl_version,
    }
}

/// `GET /api/state` — snapshot of connected users, rooms, and groups.
pub async fn state_snapshot(_admin: Admin, State(st): State<WebState>) -> Json<StateSnapshot> {
    // `get_rooms()` carries each room's authoritative ACL entries (hydrated at
    // startup, kept current by the `set_room_acl` write-through), so the ACL
    // editor opens with the real rules.
    let rooms: Vec<RoomDto> = st.state.get_rooms().await.iter().map(room_dto).collect();

    let groups: Vec<GroupDto> = st
        .persistence
        .list_groups()
        .into_iter()
        .map(|(name, g)| GroupDto {
            is_builtin: BUILTIN_GROUPS.contains(&name.as_str()),
            name,
            permissions: g.permissions,
        })
        .collect();

    let mut users = Vec::new();
    for client in st.state.snapshot_clients() {
        if !client.authenticated.load(Ordering::SeqCst) {
            continue;
        }
        let user_id = client.user_id;
        let status = st.state.get_user_status(user_id).await;
        let room_id = st.state.get_user_room(user_id).await.map(|u| u.to_string());
        let is_registered = match st.state.get_user_public_key(user_id) {
            Some(key) => st.persistence.is_registered(&key),
            None => false,
        };
        users.push(UserDto {
            user_id,
            username: client.get_username().await,
            room_id,
            is_muted: status.is_muted,
            is_deafened: status.is_deafened,
            server_muted: client.server_muted.load(Ordering::Relaxed),
            is_elevated: client.is_superuser.load(Ordering::Relaxed),
            groups: client.identity.groups().await,
            is_registered,
        });
    }

    // Registered (persisted) users, keyed by public key — the population whose
    // group memberships the admin manages, online or not. The set of currently
    // connected keys marks which are live.
    let online_keys: HashSet<[u8; 32]> = st
        .state
        .snapshot_clients()
        .iter()
        .filter_map(|c| st.state.get_user_public_key(c.user_id))
        .collect();
    let registered_users: Vec<RegisteredUserDto> = st
        .persistence
        .list_registered_users()
        .into_iter()
        .map(|(key, user)| RegisteredUserDto {
            public_key: base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, key),
            username: user.username,
            groups: st.persistence.get_user_groups(&key),
            online: online_keys.contains(&key),
        })
        .collect();

    Json(StateSnapshot {
        client_count: st.state.client_count(),
        users,
        rooms,
        groups,
        registered_users,
    })
}

/// `GET /api/groups` — list permission groups.
pub async fn list_groups(_admin: Admin, State(st): State<WebState>) -> ApiResult<Vec<GroupDto>> {
    let groups: Vec<GroupDto> = st
        .persistence
        .list_groups()
        .into_iter()
        .map(|(name, g)| GroupDto {
            is_builtin: BUILTIN_GROUPS.contains(&name.as_str()),
            name,
            permissions: g.permissions,
        })
        .collect();
    Ok(Json(groups))
}

/// `GET /api/rooms` — list rooms.
pub async fn list_rooms(_admin: Admin, State(st): State<WebState>) -> ApiResult<Vec<RoomDto>> {
    let rooms = st.state.get_rooms().await.iter().map(room_dto).collect();
    Ok(Json(rooms))
}
