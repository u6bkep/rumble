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
use api::permissions::{ADMIN_PERMISSIONS, DEFAULT_PERMISSIONS};
use serde::{Deserialize, Serialize};
use sled::Db;
use std::path::Path;
use tracing::info;

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
    /// Tree for room ACLs: room UUID (16 bytes) → PersistedRoomAcl
    room_acls: sled::Tree,
    /// Tree for sudo password: fixed key b"sudo" → bcrypt hash string
    sudo_password: sled::Tree,
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
        let room_acls = db.open_tree("room_acls")?;
        let sudo_password = db.open_tree("sudo_password")?;

        Ok(Self {
            db,
            registered_users,
            known_keys,
            rooms,
            groups,
            user_groups,
            room_acls,
            sudo_password,
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
        let room_acls = db.open_tree("room_acls")?;
        let sudo_password = db.open_tree("sudo_password")?;

        Ok(Self {
            db,
            registered_users,
            known_keys,
            rooms,
            groups,
            user_groups,
            room_acls,
            sudo_password,
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
            if let Ok((key, value)) = result {
                if let Ok(user) = bincode::deserialize::<RegisteredUser>(&value) {
                    if user.username.eq_ignore_ascii_case(username) {
                        // This username is registered - only allow if it's the same key
                        if key.as_ref() != public_key {
                            return true; // Username taken by different key
                        }
                        // Same key owns this username - not taken
                        return false;
                    }
                }
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
            if let Ok((_key, value)) = result {
                if let Ok(user) = bincode::deserialize::<RegisteredUser>(&value) {
                    if user.username.eq_ignore_ascii_case(username) {
                        return true;
                    }
                }
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

    /// Add a user to a group.
    pub fn add_user_to_group(&self, public_key: &[u8; 32], group: &str) -> Result<()> {
        let mut groups = self.get_user_groups(public_key);
        if !groups.iter().any(|g| g == group) {
            groups.push(group.to_string());
            self.set_user_groups(public_key, &groups)?;
        }
        Ok(())
    }

    /// Remove a user from a group.
    pub fn remove_user_from_group(&self, public_key: &[u8; 32], group: &str) -> Result<()> {
        let mut groups = self.get_user_groups(public_key);
        let before = groups.len();
        groups.retain(|g| g != group);
        if groups.len() != before {
            self.set_user_groups(public_key, &groups)?;
        }
        Ok(())
    }

    /// Remove a group from all users' group lists (used when deleting a group).
    pub fn remove_group_from_all_users(&self, group: &str) {
        for entry in self.user_groups.iter() {
            if let Ok((key, value)) = entry {
                if let Ok(mut groups) = bincode::deserialize::<Vec<String>>(&value) {
                    let before = groups.len();
                    groups.retain(|g| g != group);
                    if groups.len() != before {
                        if let Ok(key_arr) = <[u8; 32]>::try_from(key.as_ref()) {
                            let _ = self.set_user_groups(&key_arr, &groups);
                        }
                    }
                }
            }
        }
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
}
