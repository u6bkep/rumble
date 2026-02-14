//! ACL permission checking stubs.
//!
//! This module provides stub implementations of ACL functions.
//! The real implementations come from the `acl-server-core` branch.

use crate::state::{ClientHandle, ServerState};
use api::{permissions::Permissions, proto};
use uuid::Uuid;

/// Check if a user has the required permission in a room.
/// Returns Ok(()) if allowed, Err(PermissionDenied) if not.
pub async fn check_permission(
    _state: &ServerState,
    _sender: &ClientHandle,
    _room_uuid: Uuid,
    _required: Permissions,
) -> Result<(), proto::PermissionDenied> {
    // Stub: allow everything (real impl comes from acl-server-core branch)
    Ok(())
}

/// Evaluate effective permissions for a user in a room.
pub async fn evaluate_user_permissions(_state: &ServerState, _sender: &ClientHandle, _room_uuid: Uuid) -> Permissions {
    // Stub: return all permissions
    Permissions::all()
}
