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
    AddControllerRequest, BanRequest, CreateGroupRequest, CreateRoomRequest, KickRequest, ModifyGroupRequest,
    OkMessage, SetParticipantGroupRequest, SetRoomAclRequest, SetSudoPasswordRequest, SetUserGroupRequest,
    ToggleGroupPermissionRequest,
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

// --- Server administration (sudo password, controllers) ----------------------

/// `POST /api/sudo-password` — replace the sudo elevation password (also the
/// web-admin login credential). Requires an existing admin session; over the
/// local admin socket, local access itself is the credential, which is what
/// lets `server set-sudo-password` recover a lost password without downtime.
pub async fn set_sudo_password(
    _admin: Admin,
    State(st): State<WebState>,
    Json(req): Json<SetSudoPasswordRequest>,
) -> ApiResult<OkMessage> {
    if req.password.is_empty() {
        return Err(ApiErrorResponse::bad_request("Password cannot be empty"));
    }
    // bcrypt hashing is CPU-bound and deliberately slow — keep it off the
    // async workers that also relay voice.
    let hash = tokio::task::spawn_blocking(move || bcrypt::hash(&req.password, bcrypt::DEFAULT_COST))
        .await
        .map_err(|e| ApiErrorResponse::internal(format!("Hashing task failed: {e}")))?
        .map_err(|e| ApiErrorResponse::internal(format!("Failed to hash password: {e}")))?;
    st.persistence
        .set_sudo_password(&hash)
        .map_err(|e| ApiErrorResponse::internal(format!("Failed to set sudo password: {e}")))?;
    let _ = st.persistence.flush();
    // A sudo password closes first-run bootstrap, so the setup token (and the
    // file publishing it) is dead — clean it up like a completed bootstrap does.
    if let Some(path) = st.setup_token_file.as_ref()
        && std::fs::remove_file(path).is_ok()
    {
        tracing::info!("deleted setup token file {} (bootstrap closed)", path.display());
    }
    ok("Sudo password set".to_string())
}

/// `POST /api/controllers` — authorize a controller key (Mumble bridge, bots):
/// ensures the `controllers` group exists and adds the key to it.
pub async fn add_controller(
    _admin: Admin,
    State(st): State<WebState>,
    Json(req): Json<AddControllerRequest>,
) -> ApiResult<OkMessage> {
    let key = decode_standard_public_key(&req.public_key_b64)?;
    let msg = map_op(ops::apply_add_controller(&st.state, &st.persistence, key).await)?;
    ok(msg)
}

/// `PUT /api/controllers/{key}/participant-group` — set (or clear with `null`)
/// the default group that this controller's anonymous participants inherit.
pub async fn set_participant_group(
    _admin: Admin,
    State(st): State<WebState>,
    Path(key_b64): Path<String>,
    Json(req): Json<SetParticipantGroupRequest>,
) -> ApiResult<OkMessage> {
    let key = decode_public_key(&key_b64)?;
    if let Some(group) = req.group.as_deref()
        && st.persistence.get_group(group).is_none()
    {
        return Err(ApiErrorResponse::bad_request(format!("Group '{group}' does not exist")));
    }
    st.persistence
        .set_participant_default_group(&key, req.group.as_deref())
        .map_err(|e| ApiErrorResponse::internal(format!("Failed to persist participant group: {e}")))?;
    let _ = st.persistence.flush();
    ok(match req.group {
        Some(g) => format!("Controller participants now inherit group '{g}'"),
        None => "Controller participant group cleared".to_string(),
    })
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

/// Decode a standard-base64 32-byte Ed25519 public key — the form keys appear
/// in everywhere outside URLs (bridge logs, identity exports).
fn decode_standard_public_key(s: &str) -> Result<[u8; 32], ApiErrorResponse> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s.trim())
        .map_err(|_| ApiErrorResponse::bad_request("Invalid base64 public key"))?;
    bytes
        .try_into()
        .map_err(|_| ApiErrorResponse::bad_request("Public key must be 32 bytes"))
}
