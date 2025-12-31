//! Server persistence layer using sled for registered users and known keys.
//!
//! This module provides persistent storage for:
//! - Registered users (public_key → user data)
//! - Known keys (keys that have connected before, bypass password)
//! - Rooms (uuid → room data)

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sled::Db;
use std::path::Path;

/// User registration data stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredUser {
    /// The user's registered username.
    pub username: String,
    /// User roles for future ACL support.
    pub roles: Vec<String>,
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

/// Server persistence layer using sled.
pub struct Persistence {
    db: Db,
    /// Tree for registered users: public_key (32 bytes) → RegisteredUser
    registered_users: sled::Tree,
    /// Tree for known keys: public_key (32 bytes) → empty value
    known_keys: sled::Tree,
    /// Tree for rooms: uuid (16 bytes) → PersistedRoom
    rooms: sled::Tree,
}

impl Persistence {
    /// Open the persistence layer at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db = sled::open(path)?;
        let registered_users = db.open_tree("registered_users")?;
        let known_keys = db.open_tree("known_keys")?;
        let rooms = db.open_tree("rooms")?;
        
        Ok(Self {
            db,
            registered_users,
            known_keys,
            rooms,
        })
    }
    
    /// Create an in-memory persistence layer (for testing).
    pub fn in_memory() -> Result<Self> {
        let db = sled::Config::new().temporary(true).open()?;
        let registered_users = db.open_tree("registered_users")?;
        let known_keys = db.open_tree("known_keys")?;
        let rooms = db.open_tree("rooms")?;
        
        Ok(Self {
            db,
            registered_users,
            known_keys,
            rooms,
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
    pub fn update_user_last_room(
        &self,
        public_key: &[u8; 32],
        room_uuid: Option<[u8; 16]>,
    ) -> Result<()> {
        if let Some(mut user) = self.get_registered_user(public_key) {
            user.last_room = room_uuid;
            self.register_user(public_key, user)?;
        }
        Ok(())
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
            roles: vec!["user".to_string()],
            last_room: None,
        };
        persistence.register_user(&key, user.clone()).unwrap();

        let retrieved = persistence.get_registered_user(&key).unwrap();
        assert_eq!(retrieved.username, "alice");
        assert_eq!(retrieved.roles, vec!["user".to_string()]);

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
            roles: vec![],
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
            roles: vec![],
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
            roles: vec![],
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

        persistence.save_room(&uuid1, &PersistedRoom {
            name: "Room 1".to_string(),
            parent: None,
            description: String::new(),
            permanent: true,
        }).unwrap();

        persistence.save_room(&uuid2, &PersistedRoom {
            name: "Room 2".to_string(),
            parent: None,
            description: String::new(),
            permanent: false,
        }).unwrap();

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
            roles: vec![],
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
}
