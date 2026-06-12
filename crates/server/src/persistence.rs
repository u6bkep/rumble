//! Server persistence layer using sled for registered users and known keys.
//!
//! This module provides persistent storage for:
//! - Registered users (public_key → user data)
//! - Known keys (keys that have connected before, bypass password)
//! - Rooms (uuid → room data)
//! - Permission groups (group name → permissions)
//! - User-group assignments (public_key → group names)
//! - Room ACLs (room UUID → ACL data)
//! - Sudo password (fixed key → bcrypt hash)

use anyhow::Result;
use rumble_protocol::permissions::{ADMIN_PERMISSIONS, DEFAULT_PERMISSIONS};
use serde::{Deserialize, Serialize};
use sled::Db;
use std::path::Path;
use tracing::{info, warn};

/// A persisted ban record, keyed by the banned user's public key (32 bytes).
///
/// `banned_at_secs` is wall-clock seconds since UNIX epoch at the moment the
/// ban was issued. `duration_seconds == 0` means permanent. Missing
/// `banned_at_secs` in old records is handled gracefully: if we cannot compute
/// an expiry we treat the ban as permanent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedBan {
    /// Unix timestamp (seconds) when the ban was issued. `None` in records
    /// written before this field was added — treat as permanent.
    pub banned_at_secs: Option<u64>,
    /// Duration in seconds; 0 means permanent.
    pub duration_seconds: u64,
    /// Human-readable reason (may be empty).
    pub reason: String,
    /// Display name of the issuing admin (informational).
    pub banned_by: String,
}

impl PersistedBan {
    /// Returns `true` if this ban has expired as of `now_secs` (seconds since
    /// UNIX epoch). Permanent bans (`duration_seconds == 0`) or bans with no
    /// recorded timestamp never expire.
    pub fn is_expired(&self, now_secs: u64) -> bool {
        if self.duration_seconds == 0 {
            return false;
        }
        match self.banned_at_secs {
            Some(at) => now_secs >= at.saturating_add(self.duration_seconds),
            // Unknown issue time → treat as permanent, do not expire
            None => false,
        }
    }
}

/// User registration data stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredUser {
    /// The user's registered username.
    pub username: String,
    /// Last room the user was in (UUID bytes).
    pub last_room: Option<[u8; 16]>,
}

/// Room data stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedRoom {
    /// The room's display name.
    pub name: String,
    /// Parent room UUID (None for root-level rooms).
    pub parent: Option<[u8; 16]>,
    /// Room description.
    pub description: String,
    /// Whether this room is permanent (survives server restart).
    pub permanent: bool,
}

/// A persisted permission group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedGroup {
    pub permissions: u32,
}

/// Persisted room ACL data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedRoomAcl {
    pub inherit_acl: bool,
    pub entries: Vec<PersistedAclEntry>,
}

/// A single persisted ACL entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedAclEntry {
    pub group: String,
    pub grant: u32,
    pub deny: u32,
    pub apply_here: bool,
    pub apply_subs: bool,
}

/// Server persistence layer using sled.
pub struct Persistence {
    #[allow(dead_code)]
    db: Db,
    /// Tree for registered users: public_key (32 bytes) → RegisteredUser
    registered_users: sled::Tree,
    /// Tree for known keys: public_key (32 bytes) → empty value
    known_keys: sled::Tree,
    /// Tree for rooms: uuid (16 bytes) → PersistedRoom
    rooms: sled::Tree,
    /// Tree for permission groups: group name (UTF-8) → PersistedGroup
    groups: sled::Tree,
    /// Tree for user-group assignments: public_key (32 bytes) → Vec<String>
    user_groups: sled::Tree,
    /// Tree for timed group-membership expiries. Key is `public_key (32 bytes)
    /// ++ group name (UTF-8)`, value is the expiry as 8-byte big-endian Unix
    /// seconds. A membership with no entry here is permanent — so existing
    /// databases (which predate this tree) treat every membership as permanent,
    /// exactly as before. Mirrors the ban model: the expiry record coexists with
    /// the `user_groups` membership, and both are removed together on expiry.
    group_expiries: sled::Tree,
    /// Tree for room ACLs: room UUID (16 bytes) → PersistedRoomAcl
    room_acls: sled::Tree,
    /// Tree for sudo password: fixed key b"sudo" → bcrypt hash string
    sudo_password: sled::Tree,
    /// Tree for controller participant defaults: controller public_key (32
    /// bytes) → default participant group name (UTF-8). Separate from the
    /// controller's own group assignment (which carries MANAGE_PARTICIPANTS),
    /// so anonymous participants never inherit the controller's authority.
    participant_defaults: sled::Tree,
    /// Tree for timed ban metadata: public_key (32 bytes) → PersistedBan.
    /// A record here coexists with the user being in the "banned" group.
    /// When a ban expires, both the group membership and this record are removed.
    pub bans: sled::Tree,
}

impl Persistence {
    /// Open the persistence layer at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db = sled::open(path)?;
        let registered_users = db.open_tree("registered_users")?;
        let known_keys = db.open_tree("known_keys")?;
        let rooms = db.open_tree("rooms")?;
        let groups = db.open_tree("groups")?;
        let user_groups = db.open_tree("user_groups")?;
        let group_expiries = db.open_tree("group_expiries")?;
        let room_acls = db.open_tree("room_acls")?;
        let sudo_password = db.open_tree("sudo_password")?;
        let participant_defaults = db.open_tree("participant_defaults")?;
        let bans = db.open_tree("bans")?;

        Ok(Self {
            db,
            registered_users,
            known_keys,
            rooms,
            groups,
            user_groups,
            group_expiries,
            room_acls,
            sudo_password,
            participant_defaults,
            bans,
        })
    }

    /// Create an in-memory persistence layer (for testing).
    pub fn in_memory() -> Result<Self> {
        let db = sled::Config::new().temporary(true).open()?;
        let registered_users = db.open_tree("registered_users")?;
        let known_keys = db.open_tree("known_keys")?;
        let rooms = db.open_tree("rooms")?;
        let groups = db.open_tree("groups")?;
        let user_groups = db.open_tree("user_groups")?;
        let group_expiries = db.open_tree("group_expiries")?;
        let room_acls = db.open_tree("room_acls")?;
        let sudo_password = db.open_tree("sudo_password")?;
        let participant_defaults = db.open_tree("participant_defaults")?;
        let bans = db.open_tree("bans")?;

        Ok(Self {
            db,
            registered_users,
            known_keys,
            rooms,
            groups,
            user_groups,
            group_expiries,
            room_acls,
            sudo_password,
            participant_defaults,
            bans,
        })
    }

    /// Get registered user by public key.
    pub fn get_registered_user(&self, public_key: &[u8; 32]) -> Option<RegisteredUser> {
        self.registered_users
            .get(public_key)
            .ok()
            .flatten()
            .and_then(|data| bincode::deserialize(&data).ok())
    }

    /// List every registered user as `(public_key, RegisteredUser)`. Used by the
    /// web admin to manage group memberships for users who may be offline.
    pub fn list_registered_users(&self) -> Vec<([u8; 32], RegisteredUser)> {
        self.registered_users
            .iter()
            .filter_map(|result| {
                result.ok().and_then(|(key, value)| {
                    let key: [u8; 32] = key.as_ref().try_into().ok()?;
                    let user: RegisteredUser = bincode::deserialize(&value).ok()?;
                    Some((key, user))
                })
            })
            .collect()
    }

    /// Register a user (bind username to public key).
    pub fn register_user(&self, public_key: &[u8; 32], user: RegisteredUser) -> Result<()> {
        let data = bincode::serialize(&user)?;
        self.registered_users.insert(public_key, data)?;
        Ok(())
    }

    /// Unregister a user.
    pub fn unregister_user(&self, public_key: &[u8; 32]) -> Result<()> {
        self.registered_users.remove(public_key)?;
        Ok(())
    }

    /// Check if username is taken by a different key.
    ///
    /// This returns true if:
    /// 1. The username is registered to a DIFFERENT public key, OR
    /// 2. The username matches a registered user's name and the provided key is NOT that registered user
    ///
    /// This ensures that registered usernames can only be used by their registered key.
    pub fn is_username_taken(&self, username: &str, public_key: &[u8; 32]) -> bool {
        for result in self.registered_users.iter() {
            if let Ok((key, value)) = result
                && let Ok(user) = bincode::deserialize::<RegisteredUser>(&value)
                && user.username.eq_ignore_ascii_case(username)
            {
                // This username is registered - only allow if it's the same key
                if key.as_ref() != public_key {
                    return true; // Username taken by different key
                }
                // Same key owns this username - not taken
                return false;
            }
        }
        false
    }

    /// Check if a public key is registered (has a bound username).
    pub fn is_registered(&self, public_key: &[u8; 32]) -> bool {
        self.registered_users.contains_key(public_key).unwrap_or(false)
    }

    /// Check if a public key is known (has connected before).
    pub fn is_known_key(&self, public_key: &[u8; 32]) -> bool {
        self.known_keys.contains_key(public_key).unwrap_or(false)
    }

    /// Mark a key as known.
    pub fn add_known_key(&self, public_key: &[u8; 32]) -> Result<()> {
        self.known_keys.insert(public_key, &[])?;
        Ok(())
    }

    /// Remove a known key.
    pub fn remove_known_key(&self, public_key: &[u8; 32]) -> Result<()> {
        self.known_keys.remove(public_key)?;
        Ok(())
    }

    /// Flush all pending writes to disk.
    pub fn flush(&self) -> Result<()> {
        self.db.flush()?;
        Ok(())
    }

    // =========================================================================
    // Generic raw tree access (used by ACL handlers, real impl in acl-server-core)
    // =========================================================================

    /// Store raw bytes in a named tree.
    pub fn store_raw(&self, tree_name: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let tree = self.db.open_tree(tree_name)?;
        tree.insert(key, value)?;
        Ok(())
    }

    /// Get raw bytes from a named tree.
    pub fn get_raw(&self, tree_name: &str, key: &[u8]) -> Option<Vec<u8>> {
        let tree = self.db.open_tree(tree_name).ok()?;
        tree.get(key).ok().flatten().map(|v| v.to_vec())
    }

    /// Remove an entry from a named tree.
    pub fn remove_raw(&self, tree_name: &str, key: &[u8]) -> Result<()> {
        let tree = self.db.open_tree(tree_name)?;
        tree.remove(key)?;
        Ok(())
    }

    /// Check if a username is registered (for group name collision check).
    pub fn is_username_registered(&self, username: &str) -> bool {
        for result in self.registered_users.iter() {
            if let Ok((_key, value)) = result
                && let Ok(user) = bincode::deserialize::<RegisteredUser>(&value)
                && user.username.eq_ignore_ascii_case(username)
            {
                return true;
            }
        }
        false
    }

    // =========================================================================
    // Room persistence
    // =========================================================================

    /// Get a room by UUID.
    pub fn get_room(&self, uuid: &[u8; 16]) -> Option<PersistedRoom> {
        self.rooms
            .get(uuid)
            .ok()
            .flatten()
            .and_then(|data| bincode::deserialize(&data).ok())
    }

    /// Save a room.
    pub fn save_room(&self, uuid: &[u8; 16], room: &PersistedRoom) -> Result<()> {
        let data = bincode::serialize(room)?;
        self.rooms.insert(uuid, data)?;
        Ok(())
    }

    /// Delete a room.
    pub fn delete_room(&self, uuid: &[u8; 16]) -> Result<()> {
        self.rooms.remove(uuid)?;
        Ok(())
    }

    /// Get all persisted rooms.
    pub fn get_all_rooms(&self) -> Vec<([u8; 16], PersistedRoom)> {
        self.rooms
            .iter()
            .filter_map(|result| {
                result.ok().and_then(|(key, value)| {
                    let uuid: [u8; 16] = key.as_ref().try_into().ok()?;
                    let room: PersistedRoom = bincode::deserialize(&value).ok()?;
                    Some((uuid, room))
                })
            })
            .collect()
    }

    /// Update a registered user's last room.
    pub fn update_user_last_room(&self, public_key: &[u8; 32], room_uuid: Option<[u8; 16]>) -> Result<()> {
        if let Some(mut user) = self.get_registered_user(public_key) {
            user.last_room = room_uuid;
            self.register_user(public_key, user)?;
        }
        Ok(())
    }

    // =========================================================================
    // Permission Groups
    // =========================================================================

    /// Create a permission group. Overwrites if it already exists.
    pub fn create_group(&self, name: &str, permissions: u32) -> Result<()> {
        let group = PersistedGroup { permissions };
        let data = bincode::serialize(&group)?;
        self.groups.insert(name.as_bytes(), data)?;
        Ok(())
    }

    /// Get a permission group by name.
    pub fn get_group(&self, name: &str) -> Option<PersistedGroup> {
        self.groups
            .get(name.as_bytes())
            .ok()
            .flatten()
            .and_then(|data| bincode::deserialize(&data).ok())
    }

    /// Delete a permission group.
    pub fn delete_group(&self, name: &str) -> Result<()> {
        self.groups.remove(name.as_bytes())?;
        Ok(())
    }

    /// Modify a group's permissions.
    pub fn modify_group(&self, name: &str, permissions: u32) -> Result<bool> {
        if self.groups.contains_key(name.as_bytes())? {
            self.create_group(name, permissions)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Atomically set (`enable`) or clear specific permission bit(s) on a
    /// group, leaving the rest of its bitmask untouched. The read-modify-write
    /// happens inside sled's `update_and_fetch` compare-and-swap loop, so two
    /// concurrent toggles of different bits both land.
    ///
    /// Returns `Ok(Some(new_permissions))` on success, `Ok(None)` if the group
    /// does not exist.
    pub fn toggle_group_permission(&self, name: &str, bits: u32, enable: bool) -> Result<Option<u32>> {
        let updated = self.groups.update_and_fetch(name.as_bytes(), |old| {
            let old = old?;
            let group: PersistedGroup = bincode::deserialize(old).ok()?;
            let permissions = if enable {
                group.permissions | bits
            } else {
                group.permissions & !bits
            };
            bincode::serialize(&PersistedGroup { permissions }).ok()
        })?;
        match updated {
            Some(data) => {
                let group: PersistedGroup =
                    bincode::deserialize(&data).map_err(|e| anyhow::anyhow!("corrupt group record: {e}"))?;
                Ok(Some(group.permissions))
            }
            None => Ok(None),
        }
    }

    /// List all groups.
    pub fn list_groups(&self) -> Vec<(String, PersistedGroup)> {
        self.groups
            .iter()
            .filter_map(|result| {
                result.ok().and_then(|(key, value)| {
                    let name = String::from_utf8(key.to_vec()).ok()?;
                    let group: PersistedGroup = bincode::deserialize(&value).ok()?;
                    Some((name, group))
                })
            })
            .collect()
    }

    /// Ensure default groups exist (called on startup).
    /// Creates "default" and "admin" groups if the groups tree is empty.
    pub fn ensure_default_groups(&self) -> Result<()> {
        if self.groups.is_empty() {
            info!("Creating default permission groups");
            self.create_group("default", DEFAULT_PERMISSIONS.bits())?;
            self.create_group("admin", ADMIN_PERMISSIONS.bits())?;
        }
        // Self-heal: ADMIN_PERMISSIONS used to be ALL, which includes the
        // BANNED auth-time marker — a DB seeded in that window bans every
        // admin at login. BANNED on the admin group is never meaningful (it
        // locks out exactly the people it grants everything to), so strip it.
        use rumble_protocol::permissions::Permissions;
        if let Some(g) = self.get_group("admin") {
            let perms = Permissions::from_bits_truncate(g.permissions);
            if perms.contains(Permissions::BANNED) {
                warn!("admin group carried the BANNED marker bit (seeding bug that bans admins at auth); clearing it");
                self.modify_group("admin", perms.difference(Permissions::BANNED).bits())?;
            }
        }
        Ok(())
    }

    // =========================================================================
    // User-Group Assignments
    // =========================================================================

    /// Set the complete group list for a user.
    pub fn set_user_groups(&self, public_key: &[u8; 32], groups: &[String]) -> Result<()> {
        let data = bincode::serialize(groups)?;
        self.user_groups.insert(public_key, data)?;
        Ok(())
    }

    /// Get the groups a user belongs to.
    pub fn get_user_groups(&self, public_key: &[u8; 32]) -> Vec<String> {
        self.user_groups
            .get(public_key)
            .ok()
            .flatten()
            .and_then(|data| bincode::deserialize::<Vec<String>>(&data).ok())
            .unwrap_or_default()
    }

    /// Set the default participant group for a controller (by its public key).
    /// Anonymous participants minted by this controller inherit this group.
    /// Passing `None` clears the setting.
    pub fn set_participant_default_group(&self, public_key: &[u8; 32], group: Option<&str>) -> Result<()> {
        match group {
            Some(g) => {
                self.participant_defaults.insert(public_key, g.as_bytes())?;
            }
            None => {
                self.participant_defaults.remove(public_key)?;
            }
        }
        Ok(())
    }

    /// Get the default participant group configured for a controller, if any.
    pub fn get_participant_default_group(&self, public_key: &[u8; 32]) -> Option<String> {
        self.participant_defaults
            .get(public_key)
            .ok()
            .flatten()
            .and_then(|data| String::from_utf8(data.to_vec()).ok())
    }

    /// Atomically mutate a user's group list inside sled's `update_and_fetch`
    /// compare-and-swap loop, so two concurrent membership changes for the
    /// same user (e.g. two admins toggling different groups) don't lose each
    /// other to a get→modify→insert race. A missing record mutates from the
    /// empty list. The mutator must be pure — sled may retry it.
    fn update_user_groups(&self, public_key: &[u8; 32], mutate: impl Fn(&mut Vec<String>)) -> Result<()> {
        self.user_groups.update_and_fetch(public_key, |old| {
            let mut groups: Vec<String> = old.and_then(|d| bincode::deserialize(d).ok()).unwrap_or_default();
            mutate(&mut groups);
            // Serializing Vec<String> can't realistically fail, but returning
            // None here would *delete* the record — fall back to the old value.
            bincode::serialize(&groups).ok().or_else(|| old.map(|d| d.to_vec()))
        })?;
        Ok(())
    }

    /// Add a user to a group (atomic; idempotent if already a member). The
    /// membership is permanent: adding clears any pre-existing expiry record
    /// for this `(key, group)`, so re-adding a previously-timed membership
    /// without an expiry makes it permanent (matching the `expires_at == 0`
    /// contract).
    pub fn add_user_to_group(&self, public_key: &[u8; 32], group: &str) -> Result<()> {
        self.add_user_to_group_with_expiry(public_key, group, 0)
    }

    /// Add a user to a group with an optional expiry (atomic; idempotent if
    /// already a member). `expires_at` is Unix seconds; `0` means permanent
    /// (no expiry record stored).
    pub fn add_user_to_group_with_expiry(&self, public_key: &[u8; 32], group: &str, expires_at: u64) -> Result<()> {
        self.update_user_groups(public_key, |groups| {
            if !groups.iter().any(|g| g == group) {
                groups.push(group.to_string());
            }
        })?;
        // Always (re)set the expiry so callers can extend, shorten, or clear it
        // on an existing membership.
        self.set_group_expiry(public_key, group, expires_at)?;
        Ok(())
    }

    /// Remove a user from a group (atomic; idempotent if not a member). Also
    /// drops any orphaned expiry record regardless of whether the membership
    /// was present, so timed-membership bookkeeping never leaks.
    pub fn remove_user_from_group(&self, public_key: &[u8; 32], group: &str) -> Result<()> {
        self.update_user_groups(public_key, |groups| groups.retain(|g| g != group))?;
        let _ = self.group_expiries.remove(Self::expiry_key(public_key, group));
        Ok(())
    }

    /// Remove a group from all users' group lists (used when deleting a group).
    pub fn remove_group_from_all_users(&self, group: &str) {
        for entry in self.user_groups.iter() {
            if let Ok((key, _)) = entry
                && let Ok(key_arr) = <[u8; 32]>::try_from(key.as_ref())
            {
                let _ = self.update_user_groups(&key_arr, |groups| groups.retain(|g| g != group));
                let _ = self.group_expiries.remove(Self::expiry_key(&key_arr, group));
            }
        }
    }

    // ---- Timed group memberships ----

    /// Build the `group_expiries` key for a `(public_key, group)` pair:
    /// `public_key (32 bytes) ++ group name (UTF-8)`.
    fn expiry_key(public_key: &[u8; 32], group: &str) -> Vec<u8> {
        let mut key = Vec::with_capacity(32 + group.len());
        key.extend_from_slice(public_key);
        key.extend_from_slice(group.as_bytes());
        key
    }

    /// Set (or clear) the expiry for a `(key, group)` membership. `expires_at`
    /// of `0` removes any expiry record, making the membership permanent.
    pub fn set_group_expiry(&self, public_key: &[u8; 32], group: &str, expires_at: u64) -> Result<()> {
        let key = Self::expiry_key(public_key, group);
        if expires_at == 0 {
            self.group_expiries.remove(key)?;
        } else {
            self.group_expiries.insert(key, &expires_at.to_be_bytes())?;
        }
        Ok(())
    }

    /// Get the expiry (Unix seconds) for a `(key, group)` membership, or `0` if
    /// the membership is permanent / has no expiry record.
    pub fn get_group_expiry(&self, public_key: &[u8; 32], group: &str) -> u64 {
        self.group_expiries
            .get(Self::expiry_key(public_key, group))
            .ok()
            .flatten()
            .and_then(|v| <[u8; 8]>::try_from(v.as_ref()).ok())
            .map(u64::from_be_bytes)
            .unwrap_or(0)
    }

    /// Return a user's currently-effective group list as of `now_secs`,
    /// transparently removing memberships whose expiry has passed (both the
    /// membership and the expiry record). This is the lazy enforcement path used
    /// at auth/permission-evaluation time so an expired membership can't be
    /// exploited in the window before the background sweep runs.
    pub fn active_user_groups(&self, public_key: &[u8; 32], now_secs: u64) -> Vec<String> {
        let groups = self.get_user_groups(public_key);
        let mut active = Vec::with_capacity(groups.len());
        for g in groups {
            let expiry = self.get_group_expiry(public_key, &g);
            if expiry != 0 && now_secs >= expiry {
                // Expired: drop the membership and its record lazily.
                let _ = self.remove_user_from_group(public_key, &g);
            } else {
                active.push(g);
            }
        }
        active
    }

    /// Iterate all timed group memberships and remove any whose expiry has
    /// passed as of `now_secs`. Returns the `(public_key, group)` pairs that were
    /// removed, so the caller can mirror the change onto live connections and
    /// broadcast it. Mirrors [`Self::sweep_expired_bans`].
    pub fn sweep_expired_memberships(&self, now_secs: u64) -> Vec<([u8; 32], String)> {
        let expired: Vec<([u8; 32], String)> = self
            .group_expiries
            .iter()
            .filter_map(|entry| {
                let (key, value) = entry.ok()?;
                let expires_at = <[u8; 8]>::try_from(value.as_ref()).ok().map(u64::from_be_bytes)?;
                if expires_at == 0 || now_secs < expires_at {
                    return None;
                }
                let raw = key.as_ref();
                if raw.len() < 32 {
                    return None;
                }
                let pubkey: [u8; 32] = raw[..32].try_into().ok()?;
                let group = String::from_utf8(raw[32..].to_vec()).ok()?;
                Some((pubkey, group))
            })
            .collect();

        for (key, group) in &expired {
            // remove_user_from_group also clears the expiry record.
            let _ = self.remove_user_from_group(key, group);
        }
        expired
    }

    // =========================================================================
    // Room ACLs
    // =========================================================================

    /// Set room ACL data.
    pub fn set_room_acl(&self, room_uuid: &[u8; 16], acl: &PersistedRoomAcl) -> Result<()> {
        let data = bincode::serialize(acl)?;
        self.room_acls.insert(room_uuid, data)?;
        Ok(())
    }

    /// Get room ACL data.
    pub fn get_room_acl(&self, room_uuid: &[u8; 16]) -> Option<PersistedRoomAcl> {
        self.room_acls
            .get(room_uuid)
            .ok()
            .flatten()
            .and_then(|data| bincode::deserialize(&data).ok())
    }

    /// Delete room ACL data.
    pub fn delete_room_acl(&self, room_uuid: &[u8; 16]) -> Result<()> {
        self.room_acls.remove(room_uuid)?;
        Ok(())
    }

    // =========================================================================
    // Bans
    // =========================================================================

    /// Persist a ban record for a user's public key.
    pub fn set_ban(&self, public_key: &[u8; 32], ban: &PersistedBan) -> Result<()> {
        let data = bincode::serialize(ban)?;
        self.bans.insert(public_key, data)?;
        Ok(())
    }

    /// Retrieve the ban record for a user's public key, if one exists.
    pub fn get_ban(&self, public_key: &[u8; 32]) -> Option<PersistedBan> {
        self.bans
            .get(public_key)
            .ok()
            .flatten()
            .and_then(|data| bincode::deserialize(&data).ok())
    }

    /// Remove the ban record for a user's public key.
    pub fn remove_ban(&self, public_key: &[u8; 32]) -> Result<()> {
        self.bans.remove(public_key)?;
        Ok(())
    }

    /// Iterate all ban records and remove any that have expired as of `now_secs`
    /// (seconds since UNIX epoch). For each expired ban, the user is also removed
    /// from the "banned" group.
    ///
    /// Returns the number of bans swept.
    pub fn sweep_expired_bans(&self, now_secs: u64) -> usize {
        let mut swept = 0usize;
        let expired_keys: Vec<[u8; 32]> = self
            .bans
            .iter()
            .filter_map(|entry| {
                let (key, value) = entry.ok()?;
                let ban: PersistedBan = bincode::deserialize(&value).ok()?;
                let key_arr: [u8; 32] = key.as_ref().try_into().ok()?;
                if ban.is_expired(now_secs) { Some(key_arr) } else { None }
            })
            .collect();

        for key in &expired_keys {
            let _ = self.bans.remove(key);
            let _ = self.remove_user_from_group(key, "banned");
            swept += 1;
        }
        swept
    }

    // =========================================================================
    // Sudo Password
    // =========================================================================

    /// Set the sudo password (stores bcrypt hash).
    pub fn set_sudo_password(&self, hash: &str) -> Result<()> {
        self.sudo_password.insert(b"sudo", hash.as_bytes())?;
        Ok(())
    }

    /// Get the sudo password hash.
    pub fn get_sudo_password(&self) -> Option<String> {
        self.sudo_password
            .get(b"sudo")
            .ok()
            .flatten()
            .and_then(|data| String::from_utf8(data.to_vec()).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_keys() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [1u8; 32];

        assert!(!persistence.is_known_key(&key));
        persistence.add_known_key(&key).unwrap();
        assert!(persistence.is_known_key(&key));
        persistence.remove_known_key(&key).unwrap();
        assert!(!persistence.is_known_key(&key));
    }

    #[test]
    fn test_registered_users() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [2u8; 32];

        assert!(persistence.get_registered_user(&key).is_none());

        let user = RegisteredUser {
            username: "alice".to_string(),
            last_room: None,
        };
        persistence.register_user(&key, user.clone()).unwrap();

        let retrieved = persistence.get_registered_user(&key).unwrap();
        assert_eq!(retrieved.username, "alice");

        persistence.unregister_user(&key).unwrap();
        assert!(persistence.get_registered_user(&key).is_none());
    }

    #[test]
    fn test_list_registered_users() {
        let persistence = Persistence::in_memory().unwrap();
        assert!(persistence.list_registered_users().is_empty());

        let key_a = [1u8; 32];
        let key_b = [2u8; 32];
        persistence
            .register_user(
                &key_a,
                RegisteredUser {
                    username: "alice".to_string(),
                    last_room: None,
                },
            )
            .unwrap();
        persistence
            .register_user(
                &key_b,
                RegisteredUser {
                    username: "bob".to_string(),
                    last_room: None,
                },
            )
            .unwrap();

        let mut listed = persistence.list_registered_users();
        listed.sort_by(|a, b| a.1.username.cmp(&b.1.username));
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].0, key_a);
        assert_eq!(listed[0].1.username, "alice");
        assert_eq!(listed[1].0, key_b);
        assert_eq!(listed[1].1.username, "bob");
    }

    #[test]
    fn test_username_taken() {
        let persistence = Persistence::in_memory().unwrap();
        let key1 = [1u8; 32];
        let key2 = [2u8; 32];

        let user = RegisteredUser {
            username: "alice".to_string(),

            last_room: None,
        };
        persistence.register_user(&key1, user).unwrap();

        // Same username, different key - should be taken
        assert!(persistence.is_username_taken("alice", &key2));
        assert!(persistence.is_username_taken("ALICE", &key2)); // Case insensitive

        // Same username, same key - not taken (it's their own)
        assert!(!persistence.is_username_taken("alice", &key1));

        // Different username - not taken
        assert!(!persistence.is_username_taken("bob", &key2));
    }

    #[test]
    fn test_is_registered() {
        let persistence = Persistence::in_memory().unwrap();
        let key1 = [1u8; 32];
        let key2 = [2u8; 32];

        // Initially not registered
        assert!(!persistence.is_registered(&key1));
        assert!(!persistence.is_registered(&key2));

        // Register key1
        let user = RegisteredUser {
            username: "alice".to_string(),

            last_room: None,
        };
        persistence.register_user(&key1, user).unwrap();

        // key1 is now registered, key2 is not
        assert!(persistence.is_registered(&key1));
        assert!(!persistence.is_registered(&key2));

        // Unregister key1
        persistence.unregister_user(&key1).unwrap();
        assert!(!persistence.is_registered(&key1));
    }

    #[test]
    fn test_registered_username_protected() {
        let persistence = Persistence::in_memory().unwrap();
        let registered_key = [1u8; 32];
        let unregistered_key = [2u8; 32];

        // Register "alice" to registered_key
        let user = RegisteredUser {
            username: "alice".to_string(),

            last_room: None,
        };
        persistence.register_user(&registered_key, user).unwrap();

        // An unregistered key trying to use "alice" should be blocked
        assert!(persistence.is_username_taken("alice", &unregistered_key));

        // The registered key can use "alice"
        assert!(!persistence.is_username_taken("alice", &registered_key));

        // Anyone can use an unregistered username like "bob"
        assert!(!persistence.is_username_taken("bob", &unregistered_key));
        assert!(!persistence.is_username_taken("bob", &registered_key));
    }

    #[test]
    fn test_room_persistence() {
        let persistence = Persistence::in_memory().unwrap();
        let uuid = [3u8; 16];

        // Initially no room
        assert!(persistence.get_room(&uuid).is_none());

        // Save a room
        let room = PersistedRoom {
            name: "General".to_string(),
            parent: None,
            description: "General discussion".to_string(),
            permanent: true,
        };
        persistence.save_room(&uuid, &room).unwrap();

        // Retrieve the room
        let retrieved = persistence.get_room(&uuid).unwrap();
        assert_eq!(retrieved.name, "General");
        assert_eq!(retrieved.description, "General discussion");
        assert!(retrieved.permanent);
        assert!(retrieved.parent.is_none());

        // Delete the room
        persistence.delete_room(&uuid).unwrap();
        assert!(persistence.get_room(&uuid).is_none());
    }

    #[test]
    fn test_room_with_parent() {
        let persistence = Persistence::in_memory().unwrap();
        let parent_uuid = [4u8; 16];
        let child_uuid = [5u8; 16];

        // Create parent room
        let parent = PersistedRoom {
            name: "Parent".to_string(),
            parent: None,
            description: "Parent room".to_string(),
            permanent: true,
        };
        persistence.save_room(&parent_uuid, &parent).unwrap();

        // Create child room with parent reference
        let child = PersistedRoom {
            name: "Child".to_string(),
            parent: Some(parent_uuid),
            description: "Child room".to_string(),
            permanent: true,
        };
        persistence.save_room(&child_uuid, &child).unwrap();

        // Verify child has parent reference
        let retrieved = persistence.get_room(&child_uuid).unwrap();
        assert_eq!(retrieved.parent, Some(parent_uuid));
    }

    #[test]
    fn test_get_all_rooms() {
        let persistence = Persistence::in_memory().unwrap();

        // Initially empty
        assert!(persistence.get_all_rooms().is_empty());

        // Add some rooms
        let uuid1 = [1u8; 16];
        let uuid2 = [2u8; 16];

        persistence
            .save_room(
                &uuid1,
                &PersistedRoom {
                    name: "Room 1".to_string(),
                    parent: None,
                    description: String::new(),
                    permanent: true,
                },
            )
            .unwrap();

        persistence
            .save_room(
                &uuid2,
                &PersistedRoom {
                    name: "Room 2".to_string(),
                    parent: None,
                    description: String::new(),
                    permanent: false,
                },
            )
            .unwrap();

        let rooms = persistence.get_all_rooms();
        assert_eq!(rooms.len(), 2);
    }

    #[test]
    fn test_update_user_last_room() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [6u8; 32];
        let room_uuid = [7u8; 16];

        // Register user without last room
        let user = RegisteredUser {
            username: "bob".to_string(),

            last_room: None,
        };
        persistence.register_user(&key, user).unwrap();

        // Update last room
        persistence.update_user_last_room(&key, Some(room_uuid)).unwrap();

        // Verify last room was updated
        let retrieved = persistence.get_registered_user(&key).unwrap();
        assert_eq!(retrieved.last_room, Some(room_uuid));

        // Clear last room
        persistence.update_user_last_room(&key, None).unwrap();
        let retrieved = persistence.get_registered_user(&key).unwrap();
        assert_eq!(retrieved.last_room, None);
    }

    #[test]
    fn test_update_last_room_unregistered_user() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [8u8; 32];
        let room_uuid = [9u8; 16];

        // Try to update last room for unregistered user - should not error
        persistence.update_user_last_room(&key, Some(room_uuid)).unwrap();

        // User should still not exist
        assert!(persistence.get_registered_user(&key).is_none());
    }

    #[test]
    fn test_groups_crud() {
        let persistence = Persistence::in_memory().unwrap();

        // Initially no groups
        assert!(persistence.list_groups().is_empty());
        assert!(persistence.get_group("admin").is_none());

        // Create groups
        persistence.create_group("admin", 0x3FFFFF).unwrap();
        persistence.create_group("default", 0x8001F).unwrap();

        let admin = persistence.get_group("admin").unwrap();
        assert_eq!(admin.permissions, 0x3FFFFF);

        let groups = persistence.list_groups();
        assert_eq!(groups.len(), 2);

        // Modify
        assert!(persistence.modify_group("admin", 0xFF).unwrap());
        assert_eq!(persistence.get_group("admin").unwrap().permissions, 0xFF);

        // Modify nonexistent
        assert!(!persistence.modify_group("nonexistent", 0).unwrap());

        // Delete
        persistence.delete_group("admin").unwrap();
        assert!(persistence.get_group("admin").is_none());
    }

    #[test]
    fn test_ensure_default_groups() {
        let persistence = Persistence::in_memory().unwrap();

        // First call creates groups
        persistence.ensure_default_groups().unwrap();
        let groups = persistence.list_groups();
        assert_eq!(groups.len(), 2);

        // Second call is idempotent (tree not empty)
        persistence.ensure_default_groups().unwrap();
        let groups = persistence.list_groups();
        assert_eq!(groups.len(), 2);
    }

    /// DBs seeded while ADMIN_PERMISSIONS was ALL carry the BANNED marker on
    /// the admin group, which bans every admin at auth. Startup must strip it.
    #[test]
    fn ensure_default_groups_heals_banned_admin_group() {
        use rumble_protocol::permissions::Permissions;
        let persistence = Persistence::in_memory().unwrap();
        // Simulate the poisoned seeding (old ADMIN_PERMISSIONS == ALL).
        persistence.create_group("default", DEFAULT_PERMISSIONS.bits()).unwrap();
        persistence
            .create_group("admin", rumble_protocol::permissions::ALL.bits())
            .unwrap();

        persistence.ensure_default_groups().unwrap();

        let admin = Permissions::from_bits_truncate(persistence.get_group("admin").unwrap().permissions);
        assert!(!admin.contains(Permissions::BANNED), "BANNED must be stripped");
        assert!(
            admin.contains(Permissions::SUDO),
            "capabilities must survive the fix-up"
        );
    }

    #[test]
    fn test_user_groups() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [10u8; 32];

        // Initially empty
        assert!(persistence.get_user_groups(&key).is_empty());

        // Add to groups
        persistence.add_user_to_group(&key, "admin").unwrap();
        persistence.add_user_to_group(&key, "moderator").unwrap();

        let groups = persistence.get_user_groups(&key);
        assert_eq!(groups.len(), 2);
        assert!(groups.contains(&"admin".to_string()));
        assert!(groups.contains(&"moderator".to_string()));

        // Idempotent add
        persistence.add_user_to_group(&key, "admin").unwrap();
        assert_eq!(persistence.get_user_groups(&key).len(), 2);

        // Remove
        persistence.remove_user_from_group(&key, "admin").unwrap();
        let groups = persistence.get_user_groups(&key);
        assert_eq!(groups.len(), 1);
        assert!(!groups.contains(&"admin".to_string()));

        // Set all at once
        persistence
            .set_user_groups(&key, &["a".to_string(), "b".to_string()])
            .unwrap();
        assert_eq!(persistence.get_user_groups(&key).len(), 2);
    }

    #[test]
    fn test_room_acls() {
        let persistence = Persistence::in_memory().unwrap();
        let uuid = [11u8; 16];

        assert!(persistence.get_room_acl(&uuid).is_none());

        let acl = PersistedRoomAcl {
            inherit_acl: false,
            entries: vec![PersistedAclEntry {
                group: "default".to_string(),
                grant: 0x004,
                deny: 0x040,
                apply_here: true,
                apply_subs: false,
            }],
        };
        persistence.set_room_acl(&uuid, &acl).unwrap();

        let retrieved = persistence.get_room_acl(&uuid).unwrap();
        assert!(!retrieved.inherit_acl);
        assert_eq!(retrieved.entries.len(), 1);
        assert_eq!(retrieved.entries[0].group, "default");
        assert_eq!(retrieved.entries[0].grant, 0x004);

        persistence.delete_room_acl(&uuid).unwrap();
        assert!(persistence.get_room_acl(&uuid).is_none());
    }

    #[test]
    fn test_sudo_password() {
        let persistence = Persistence::in_memory().unwrap();

        assert!(persistence.get_sudo_password().is_none());

        persistence.set_sudo_password("$2b$12$somehash").unwrap();
        assert_eq!(persistence.get_sudo_password().unwrap(), "$2b$12$somehash");
    }

    // =========================================================================
    // Ban expiry unit tests
    // =========================================================================

    #[test]
    fn test_persisted_ban_permanent_never_expires() {
        let ban = PersistedBan {
            banned_at_secs: Some(1_000),
            duration_seconds: 0, // permanent
            reason: String::new(),
            banned_by: String::new(),
        };
        assert!(!ban.is_expired(1_000));
        assert!(!ban.is_expired(u64::MAX));
    }

    #[test]
    fn test_persisted_ban_timed_expires() {
        let ban = PersistedBan {
            banned_at_secs: Some(1_000),
            duration_seconds: 3_600, // 1 hour
            reason: String::new(),
            banned_by: String::new(),
        };
        // Not yet expired
        assert!(!ban.is_expired(1_000));
        assert!(!ban.is_expired(4_599));
        // Exactly at expiry
        assert!(ban.is_expired(4_600));
        // Well past expiry
        assert!(ban.is_expired(u64::MAX));
    }

    #[test]
    fn test_persisted_ban_missing_timestamp_treated_as_permanent() {
        // Old records without a timestamp should never expire (treated as permanent).
        let ban = PersistedBan {
            banned_at_secs: None,
            duration_seconds: 60,
            reason: String::new(),
            banned_by: String::new(),
        };
        assert!(!ban.is_expired(u64::MAX));
    }

    #[test]
    fn test_ban_crud() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [42u8; 32];

        assert!(persistence.get_ban(&key).is_none());

        let ban = PersistedBan {
            banned_at_secs: Some(500),
            duration_seconds: 3_600,
            reason: "test".to_string(),
            banned_by: "admin".to_string(),
        };
        persistence.set_ban(&key, &ban).unwrap();

        let retrieved = persistence.get_ban(&key).unwrap();
        assert_eq!(retrieved.banned_at_secs, Some(500));
        assert_eq!(retrieved.duration_seconds, 3_600);
        assert_eq!(retrieved.reason, "test");

        persistence.remove_ban(&key).unwrap();
        assert!(persistence.get_ban(&key).is_none());
    }

    #[test]
    fn test_sweep_expired_bans_removes_expired_and_group() {
        let persistence = Persistence::in_memory().unwrap();
        persistence.ensure_default_groups().unwrap();
        let _ = persistence.create_group("banned", 0x400000); // BANNED flag

        let key_expired = [1u8; 32];
        let key_permanent = [2u8; 32];
        let key_not_yet = [3u8; 32];

        // Timed ban that has expired
        persistence.add_user_to_group(&key_expired, "banned").unwrap();
        persistence
            .set_ban(
                &key_expired,
                &PersistedBan {
                    banned_at_secs: Some(100),
                    duration_seconds: 60, // expires at 160
                    reason: String::new(),
                    banned_by: String::new(),
                },
            )
            .unwrap();

        // Permanent ban — must survive sweep
        persistence.add_user_to_group(&key_permanent, "banned").unwrap();
        persistence
            .set_ban(
                &key_permanent,
                &PersistedBan {
                    banned_at_secs: Some(100),
                    duration_seconds: 0,
                    reason: String::new(),
                    banned_by: String::new(),
                },
            )
            .unwrap();

        // Timed ban not yet expired
        persistence.add_user_to_group(&key_not_yet, "banned").unwrap();
        persistence
            .set_ban(
                &key_not_yet,
                &PersistedBan {
                    banned_at_secs: Some(100),
                    duration_seconds: 1_000, // expires at 1100
                    reason: String::new(),
                    banned_by: String::new(),
                },
            )
            .unwrap();

        let swept = persistence.sweep_expired_bans(200); // now = 200 → key_expired expired
        assert_eq!(swept, 1);

        // Expired ban removed from both the bans tree and the banned group
        assert!(persistence.get_ban(&key_expired).is_none());
        assert!(
            !persistence
                .get_user_groups(&key_expired)
                .contains(&"banned".to_string())
        );

        // Permanent and not-yet-expired bans must still be there
        assert!(persistence.get_ban(&key_permanent).is_some());
        assert!(
            persistence
                .get_user_groups(&key_permanent)
                .contains(&"banned".to_string())
        );
        assert!(persistence.get_ban(&key_not_yet).is_some());
        assert!(
            persistence
                .get_user_groups(&key_not_yet)
                .contains(&"banned".to_string())
        );
    }

    // =========================================================================
    // Timed group-membership expiry tests
    // =========================================================================

    #[test]
    fn test_membership_zero_expiry_is_permanent() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [20u8; 32];

        // expires_at: 0 → permanent, no expiry record stored.
        persistence.add_user_to_group_with_expiry(&key, "admin", 0).unwrap();
        assert_eq!(persistence.get_group_expiry(&key, "admin"), 0);

        // A sweep far in the future must not touch a permanent membership.
        let swept = persistence.sweep_expired_memberships(u64::MAX);
        assert!(swept.is_empty());
        assert!(persistence.get_user_groups(&key).contains(&"admin".to_string()));

        // active_user_groups also keeps it.
        assert!(
            persistence
                .active_user_groups(&key, u64::MAX)
                .contains(&"admin".to_string())
        );
    }

    #[test]
    fn test_membership_past_expiry_swept() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [21u8; 32];

        // expires at t=160.
        persistence.add_user_to_group_with_expiry(&key, "admin", 160).unwrap();
        assert_eq!(persistence.get_group_expiry(&key, "admin"), 160);

        // Before expiry: survives.
        assert!(persistence.sweep_expired_memberships(100).is_empty());
        assert!(persistence.get_user_groups(&key).contains(&"admin".to_string()));

        // After expiry: swept, membership and expiry record both gone.
        let swept = persistence.sweep_expired_memberships(200);
        assert_eq!(swept, vec![(key, "admin".to_string())]);
        assert!(!persistence.get_user_groups(&key).contains(&"admin".to_string()));
        assert_eq!(persistence.get_group_expiry(&key, "admin"), 0);
    }

    #[test]
    fn test_active_user_groups_lazily_drops_expired() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [22u8; 32];

        persistence.add_user_to_group(&key, "default").unwrap(); // permanent
        persistence.add_user_to_group_with_expiry(&key, "admin", 160).unwrap(); // timed

        // Before expiry both are active.
        let mut active = persistence.active_user_groups(&key, 100);
        active.sort();
        assert_eq!(active, vec!["admin".to_string(), "default".to_string()]);

        // At/after expiry, the timed group is dropped (and cleaned up in sled).
        let active = persistence.active_user_groups(&key, 160);
        assert_eq!(active, vec!["default".to_string()]);
        // The lazy drop persisted: the membership is gone from storage too.
        assert!(!persistence.get_user_groups(&key).contains(&"admin".to_string()));
    }

    #[test]
    fn test_remove_membership_clears_expiry_record() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [23u8; 32];

        persistence.add_user_to_group_with_expiry(&key, "admin", 999).unwrap();
        assert_eq!(persistence.get_group_expiry(&key, "admin"), 999);

        persistence.remove_user_from_group(&key, "admin").unwrap();
        assert_eq!(persistence.get_group_expiry(&key, "admin"), 0);

        // Re-adding as permanent does not resurrect the old expiry.
        persistence.add_user_to_group(&key, "admin").unwrap();
        assert_eq!(persistence.get_group_expiry(&key, "admin"), 0);
    }

    #[test]
    fn test_readd_permanent_clears_prior_expiry() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [24u8; 32];

        // First a timed grant, then a permanent grant of the same group.
        persistence.add_user_to_group_with_expiry(&key, "admin", 160).unwrap();
        persistence.add_user_to_group_with_expiry(&key, "admin", 0).unwrap();

        // Now permanent: a future sweep must not remove it.
        assert!(persistence.sweep_expired_memberships(u64::MAX).is_empty());
        assert!(persistence.get_user_groups(&key).contains(&"admin".to_string()));
    }

    #[test]
    fn test_old_format_membership_is_permanent() {
        // Simulate a database written before the group_expiries tree existed:
        // a membership stored only as a bincode Vec<String>, no expiry record.
        let persistence = Persistence::in_memory().unwrap();
        let key = [25u8; 32];

        persistence
            .set_user_groups(&key, &["admin".to_string(), "moderator".to_string()])
            .unwrap();

        // No expiry records exist for these old-format memberships.
        assert_eq!(persistence.get_group_expiry(&key, "admin"), 0);
        assert_eq!(persistence.get_group_expiry(&key, "moderator"), 0);

        // They load normally and are treated as permanent (survive any sweep and
        // any active_user_groups call).
        assert!(persistence.sweep_expired_memberships(u64::MAX).is_empty());
        let mut active = persistence.active_user_groups(&key, u64::MAX);
        active.sort();
        assert_eq!(active, vec!["admin".to_string(), "moderator".to_string()]);
    }

    #[test]
    fn test_delete_group_clears_expiry_records() {
        let persistence = Persistence::in_memory().unwrap();
        let key = [26u8; 32];

        persistence.add_user_to_group_with_expiry(&key, "temp", 500).unwrap();
        assert_eq!(persistence.get_group_expiry(&key, "temp"), 500);

        persistence.remove_group_from_all_users("temp");
        assert!(!persistence.get_user_groups(&key).contains(&"temp".to_string()));
        assert_eq!(persistence.get_group_expiry(&key, "temp"), 0);
    }

    #[test]
    fn test_auth_time_expired_ban_lifts_cleanly() {
        // Simulate what happens at auth time: if the ban record is expired,
        // we remove the group membership. Afterwards the user is no longer
        // in the "banned" group.
        let persistence = Persistence::in_memory().unwrap();
        persistence.ensure_default_groups().unwrap();
        let _ = persistence.create_group("banned", 0x400000);

        let key = [7u8; 32];
        persistence.add_user_to_group(&key, "banned").unwrap();
        persistence
            .set_ban(
                &key,
                &PersistedBan {
                    banned_at_secs: Some(0),
                    duration_seconds: 10, // expired at t=10
                    reason: String::new(),
                    banned_by: String::new(),
                },
            )
            .unwrap();

        let now_secs: u64 = 9999;
        let ban_record = persistence.get_ban(&key).unwrap();
        assert!(ban_record.is_expired(now_secs));

        // Lift the ban (as the auth handler does)
        persistence.remove_ban(&key).unwrap();
        persistence.remove_user_from_group(&key, "banned").unwrap();

        // Verify clean state
        assert!(persistence.get_ban(&key).is_none());
        assert!(!persistence.get_user_groups(&key).contains(&"banned".to_string()));
    }
}
