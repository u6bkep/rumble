//! Shared serde DTOs for the Rumble server's web admin REST API.
//!
//! These types define the JSON wire shape exchanged between the server's web
//! control-plane (`server::web`) and the wasm admin UI (`rumble-admin-web`).
//! They are deliberately explicit hand-written structs rather than the
//! prost-generated protocol types: the HTTP surface is decoupled from the QUIC
//! wire format, and both ends depend on this one crate so they cannot drift.
//!
//! UUIDs are carried as plain lowercase-hyphenated strings; permission masks
//! are raw `u32` bitfields (see `rumble_protocol::permissions::Permissions`).

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

// =============================================================================
// Generic envelopes
// =============================================================================

/// A successful action with a human-readable message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OkMessage {
    pub message: String,
}

/// A failed action with a user-facing reason. Returned as the JSON body of 4xx
/// responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

// =============================================================================
// Auth & bootstrap
// =============================================================================

/// `POST /api/login` — authenticate with the server's sudo password. On success
/// the server sets an HttpOnly session cookie.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

/// `GET /api/session` — reports whether the caller holds a live session and
/// whether the server still needs first-run bootstrap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub authenticated: bool,
    /// True when no sudo password is configured yet, so the bootstrap flow is
    /// available.
    pub needs_bootstrap: bool,
}

/// `POST /api/bootstrap` — first-run setup, available only while no sudo
/// password is configured. Guarded by the one-time setup token printed to the
/// server log at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapRequest {
    /// The one-time token printed to the server log at startup.
    pub setup_token: String,
    /// The sudo password to set (also the web admin login credential).
    pub sudo_password: String,
    /// Base64 Ed25519 public key to seed into the `admin` group. Optional.
    #[serde(default)]
    pub admin_public_key_b64: Option<String>,
}

// =============================================================================
// Permission groups
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDto {
    pub name: String,
    pub permissions: u32,
    pub is_builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
    pub permissions: u32,
}

/// `PATCH /api/groups/{name}` — absolute write: replaces the group's whole
/// permission bitmask. Use [`ToggleGroupPermissionRequest`] for individual
/// switch toggles so concurrent edits to different bits don't clobber each
/// other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModifyGroupRequest {
    pub permissions: u32,
}

/// `POST /api/groups/{name}/permissions` — set or clear specific permission
/// bit(s) without overwriting the rest of the group's bitmask. `enable` is the
/// desired state (idempotent on retry), applied atomically server-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToggleGroupPermissionRequest {
    /// Permission bit(s) to set or clear. Must be non-zero, known flags only.
    pub bits: u32,
    /// Desired state: true = set the bits, false = clear them.
    pub enable: bool,
}

/// `POST /api/users/{id}/groups` (by live user id) or
/// `POST /api/registered-users/{key}/groups` (by registered public key) — add
/// or remove a user to/from a group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetUserGroupRequest {
    pub group: String,
    pub add: bool,
    #[serde(default)]
    pub expires_at: u64,
}

// =============================================================================
// Rooms & ACLs
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomDto {
    /// Room UUID as a lowercase-hyphenated string.
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub description: Option<String>,
    pub inherit_acl: bool,
    /// The room's own ACL entries, so the admin UI can open the ACL editor
    /// without a second round-trip. Empty when the room has no local rules.
    #[serde(default)]
    pub acls: Vec<AclEntryDto>,
    /// Content-derived version of `(inherit_acl, acls)`, computed server-side
    /// (`rumble_protocol::room_acl_version`). The ACL editor echoes it back in
    /// [`SetRoomAclRequest::base_version`] so a save from a stale snapshot is
    /// rejected instead of clobbering another admin's concurrent edit.
    #[serde(default)]
    pub acl_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRoomRequest {
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclEntryDto {
    pub group: String,
    pub grant: u32,
    pub deny: u32,
    pub apply_here: bool,
    pub apply_subs: bool,
}

/// `PUT /api/rooms/{uuid}/acl` — replace a room's ACL entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRoomAclRequest {
    pub inherit_acl: bool,
    #[serde(default)]
    pub entries: Vec<AclEntryDto>,
    /// The [`RoomDto::acl_version`] the editor loaded before editing. The
    /// server rejects the save when the room's ACL has changed since (another
    /// admin saved meanwhile). 0 = unversioned legacy write, accepted
    /// unconditionally.
    #[serde(default)]
    pub base_version: u64,
}

// =============================================================================
// Moderation
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KickRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BanRequest {
    /// `0` means a permanent ban.
    #[serde(default)]
    pub duration_seconds: u64,
    #[serde(default)]
    pub reason: String,
}

// =============================================================================
// Monitoring
// =============================================================================

/// A connected user as seen by the admin monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserDto {
    pub user_id: u64,
    pub username: String,
    pub room_id: Option<String>,
    pub is_muted: bool,
    pub is_deafened: bool,
    pub server_muted: bool,
    pub is_elevated: bool,
    pub groups: Vec<String>,
    /// True when this connection's public key has a persisted registration.
    /// A guest (unregistered) connection has no persistent identity, so its
    /// group memberships cannot be managed.
    #[serde(default)]
    pub is_registered: bool,
}

/// A registered (persisted) user as seen by the admin monitor. Unlike
/// [`UserDto`], these exist independent of any live connection — they are the
/// persistent identities whose group memberships an admin manages. Keyed by the
/// long-term Ed25519 public key rather than a session `user_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredUserDto {
    /// URL-safe base64 (no padding) of the 32-byte Ed25519 public key. Used as
    /// the path segment for `POST /api/registered-users/{key}/groups`.
    pub public_key: String,
    pub username: String,
    pub groups: Vec<String>,
    /// True when this identity currently has a live connection.
    pub online: bool,
}

/// `GET /api/state` — a snapshot of live server state for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub client_count: usize,
    pub users: Vec<UserDto>,
    pub rooms: Vec<RoomDto>,
    pub groups: Vec<GroupDto>,
    /// All persisted user registrations, with their group memberships. Empty
    /// when persistence is disabled.
    #[serde(default)]
    pub registered_users: Vec<RegisteredUserDto>,
}
