//! Mutating admin endpoints. Each requires an admin session ([`super::auth::Admin`])
//! and delegates to the shared [`crate::ops`] cores, so the web and QUIC paths
//! apply identical mutations and broadcasts.

use super::{ApiErrorResponse, ApiResult, WebState, auth::Admin};
use crate::ops;
use axum::{
    Json,
    extract::{Path, State},
};
use rumble_protocol::proto;
use rumble_web_types::{
    BanRequest, CreateGroupRequest, CreateRoomRequest, KickRequest, ModifyGroupRequest, OkMessage, SetRoomAclRequest,
    SetUserGroupRequest, ToggleGroupPermissionRequest,
};
use uuid::Uuid;

/// Name recorded as the actor for web-initiated moderation actions.
const WEB_ACTOR: &str = "web-admin";

fn ok(message: String) -> ApiResult<OkMessage> {
    Ok(Json(OkMessage { message }))
}

/// Map an `ops::*` `Result<_, String>` failure onto a 400 response.
fn map_op<T>(r: Result<T, String>) -> Result<T, ApiErrorResponse> {
    r.map_err(ApiErrorResponse::bad_request)
}

fn parse_room_uuid(s: &str) -> Result<Uuid, ApiErrorResponse> {
    Uuid::parse_str(s).map_err(|_| ApiErrorResponse::bad_request("Invalid room UUID"))
}

// --- Groups -----------------------------------------------------------------

pub async fn create_group(
    _admin: Admin,
    State(st): State<WebState>,
    Json(req): Json<CreateGroupRequest>,
) -> ApiResult<OkMessage> {
    let msg = map_op(ops::apply_create_group(&st.state, &st.persistence, req.name, req.permissions).await)?;
    ok(msg)
}

pub async fn modify_group(
    _admin: Admin,
    State(st): State<WebState>,
    Path(name): Path<String>,
    Json(req): Json<ModifyGroupRequest>,
) -> ApiResult<OkMessage> {
    let msg = map_op(ops::apply_modify_group(&st.state, &st.persistence, name, req.permissions).await)?;
    ok(msg)
}

/// `POST /api/groups/{name}/permissions` — set or clear specific permission
/// bit(s) atomically against the group's current state. Per-switch toggles use
/// this instead of [`modify_group`] so two admins editing different bits
/// concurrently don't clobber each other (#38).
pub async fn toggle_group_permission(
    _admin: Admin,
    State(st): State<WebState>,
    Path(name): Path<String>,
    Json(req): Json<ToggleGroupPermissionRequest>,
) -> ApiResult<OkMessage> {
    let msg = map_op(ops::apply_toggle_group_permission(&st.state, &st.persistence, name, req.bits, req.enable).await)?;
    ok(msg)
}

pub async fn delete_group(_admin: Admin, State(st): State<WebState>, Path(name): Path<String>) -> ApiResult<OkMessage> {
    let msg = map_op(ops::apply_delete_group(&st.state, &st.persistence, name).await)?;
    ok(msg)
}

// --- Rooms ------------------------------------------------------------------

pub async fn create_room(
    _admin: Admin,
    State(st): State<WebState>,
    Json(req): Json<CreateRoomRequest>,
) -> ApiResult<OkMessage> {
    let parent_uuid = match req.parent_id.as_ref() {
        Some(p) => Some(parse_room_uuid(p)?),
        None => None,
    };
    let uuid =
        map_op(ops::apply_create_room(&st.state, &st.persistence, req.name, parent_uuid, req.description).await)?;
    ok(uuid.to_string())
}

pub async fn delete_room(_admin: Admin, State(st): State<WebState>, Path(uuid): Path<String>) -> ApiResult<OkMessage> {
    let room_uuid = parse_room_uuid(&uuid)?;
    let name = map_op(ops::apply_delete_room(&st.state, &st.persistence, room_uuid).await)?;
    ok(format!("Deleted room '{}'", name))
}

pub async fn set_room_acl(
    _admin: Admin,
    State(st): State<WebState>,
    Path(uuid): Path<String>,
    Json(req): Json<SetRoomAclRequest>,
) -> ApiResult<OkMessage> {
    let room_uuid = parse_room_uuid(&uuid)?;
    let entries: Vec<proto::RoomAclEntry> = req
        .entries
        .into_iter()
        .map(|e| proto::RoomAclEntry {
            group: e.group,
            grant: e.grant,
            deny: e.deny,
            apply_here: e.apply_here,
            apply_subs: e.apply_subs,
        })
        .collect();
    let msg = map_op(
        ops::apply_set_room_acl(
            &st.state,
            &st.persistence,
            room_uuid,
            req.inherit_acl,
            entries,
            req.base_version,
        )
        .await,
    )?;
    ok(msg)
}

// --- User moderation & registration -----------------------------------------

pub async fn kick_user(
    _admin: Admin,
    State(st): State<WebState>,
    Path(id): Path<u64>,
    Json(req): Json<KickRequest>,
) -> ApiResult<OkMessage> {
    let msg = map_op(ops::apply_kick(&st.state, id, &req.reason, WEB_ACTOR).await)?;
    ok(msg)
}

pub async fn ban_user(
    _admin: Admin,
    State(st): State<WebState>,
    Path(id): Path<u64>,
    Json(req): Json<BanRequest>,
) -> ApiResult<OkMessage> {
    let msg = map_op(
        ops::apply_ban(
            &st.state,
            &st.persistence,
            id,
            req.duration_seconds,
            &req.reason,
            WEB_ACTOR,
        )
        .await,
    )?;
    ok(msg)
}

pub async fn register_user(_admin: Admin, State(st): State<WebState>, Path(id): Path<u64>) -> ApiResult<OkMessage> {
    let msg = map_op(ops::apply_register_user(&st.state, &st.persistence, id).await)?;
    ok(msg)
}

pub async fn unregister_user(_admin: Admin, State(st): State<WebState>, Path(id): Path<u64>) -> ApiResult<OkMessage> {
    let msg = map_op(ops::apply_unregister_user(&st.state, &st.persistence, id).await)?;
    ok(msg)
}

pub async fn set_user_group(
    _admin: Admin,
    State(st): State<WebState>,
    Path(id): Path<u64>,
    Json(req): Json<SetUserGroupRequest>,
) -> ApiResult<OkMessage> {
    let msg =
        map_op(ops::apply_set_user_group(&st.state, &st.persistence, id, req.group, req.add, req.expires_at).await)?;
    ok(msg)
}

/// `POST /api/registered-users/{key}/groups` — add/remove a registered user
/// (identified by its URL-safe-base64 public key) to/from a group. Unlike
/// [`set_user_group`], the target need not be connected.
pub async fn set_registered_user_group(
    _admin: Admin,
    State(st): State<WebState>,
    Path(key_b64): Path<String>,
    Json(req): Json<SetUserGroupRequest>,
) -> ApiResult<OkMessage> {
    let key = decode_public_key(&key_b64)?;
    let msg = map_op(
        ops::apply_set_user_group_by_key(&st.state, &st.persistence, key, req.group, req.add, req.expires_at).await,
    )?;
    ok(msg)
}

/// Decode a URL-safe-base64 (no padding) 32-byte Ed25519 public key, the form
/// used in the registered-user DTO and route path.
fn decode_public_key(s: &str) -> Result<[u8; 32], ApiErrorResponse> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, s)
        .map_err(|_| ApiErrorResponse::bad_request("Invalid public key"))?;
    bytes
        .try_into()
        .map_err(|_| ApiErrorResponse::bad_request("Public key must be 32 bytes"))
}
