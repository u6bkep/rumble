//! Read-only monitoring endpoints: live state snapshot, group and room lists.
//! All require an admin session.

use super::{ApiResult, WebState, auth::Admin};
use axum::{Json, extract::State};
use rumble_protocol::uuid_from_room_id;
use rumble_web_types::{AclEntryDto, GroupDto, RegisteredUserDto, RoomDto, StateSnapshot, UserDto};
use std::{collections::HashSet, sync::atomic::Ordering};

const BUILTIN_GROUPS: [&str; 2] = ["default", "admin"];

/// Render a proto room into a wire DTO, including its ACL entries.
fn room_dto(r: &rumble_protocol::proto::RoomInfo) -> RoomDto {
    let id =
        r.id.as_ref()
            .and_then(uuid_from_room_id)
            .map(|u| u.to_string())
            .unwrap_or_default();
    let parent_id = r.parent_id.as_ref().and_then(uuid_from_room_id).map(|u| u.to_string());
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
    RoomDto {
        id,
        name: r.name.clone(),
        parent_id,
        description: r.description.clone(),
        inherit_acl: r.inherit_acl,
        acls,
        // Optimistic-concurrency token the ACL editor echoes back on save.
        // Computed from the same hydrated values the DTO carries, so what the
        // editor loads is exactly what the version covers.
        acl_version: rumble_protocol::room_acl_version(r.inherit_acl, &r.acls),
    }
}

/// `GET /api/state` — snapshot of connected users, rooms, and groups.
pub async fn state_snapshot(_admin: Admin, State(st): State<WebState>) -> Json<StateSnapshot> {
    // `build_room_list` hydrates each room's ACL entries from persistence, so
    // the ACL editor opens with authoritative rules (in-memory `get_rooms`
    // only carries ACLs touched since startup).
    let rooms: Vec<RoomDto> = st
        .state
        .build_room_list(&st.persistence)
        .await
        .iter()
        .map(room_dto)
        .collect();

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
    let rooms = st
        .state
        .build_room_list(&st.persistence)
        .await
        .iter()
        .map(room_dto)
        .collect();
    Ok(Json(rooms))
}
