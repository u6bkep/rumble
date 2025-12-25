//! Server state management and core types.
//!
//! This module contains the server's shared state and client handle types,
//! designed to be testable in isolation without requiring network connections.
//!
//! # Locking Strategy
//!
//! The server state uses a carefully designed locking strategy to minimize
//! contention and avoid blocking the audio path:
//!
//! - **`next_user_id`**: `AtomicU64` for lock-free ID allocation
//! - **`clients`**: `DashMap` for lock-free per-client access
//! - **`state_data`**: `RwLock<StateData>` consolidates rooms and memberships
//!   into a single lock, avoiding deadlocks from nested lock acquisition
//!
//! The voice/datagram path avoids holding any locks during I/O operations
//! by taking snapshots of needed data first.

use api::{
    ROOT_ROOM_UUID,
    proto::{RoomInfo, User, UserId},
    room_id_from_uuid, root_room_id, uuid_from_room_id,
};
use dashmap::DashMap;
use quinn::Connection;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

/// Handle to a connected client's control stream.
///
/// This represents a single client connection and provides access to:
/// - The send side of their control stream (locked per-message)
/// - Their username (immutable after set)
/// - Their server-assigned user_id
/// - Their QUIC connection for datagram sending
#[derive(Debug)]
pub struct ClientHandle {
    /// Send stream for the client's control channel.
    /// Locked only for the duration of a single message write.
    pub send: Mutex<quinn::SendStream>,
    /// The client's display name (set once from ClientHello, then immutable).
    /// Use `set_username` and `get_username` for access.
    username: RwLock<String>,
    /// Server-assigned user ID (immutable after creation).
    pub user_id: u64,
    /// The underlying QUIC connection (for datagrams).
    pub conn: Connection,
}

impl ClientHandle {
    /// Create a new client handle.
    pub fn new(send: quinn::SendStream, user_id: u64, conn: Connection) -> Self {
        Self {
            send: Mutex::new(send),
            username: RwLock::new(String::new()),
            user_id,
            conn,
        }
    }

    /// Set the username (should only be called once during ClientHello).
    pub async fn set_username(&self, name: String) {
        let mut guard = self.username.write().await;
        *guard = name;
    }

    /// Get the current username.
    pub async fn get_username(&self) -> String {
        self.username.read().await.clone()
    }

    /// Send a framed message to this client.
    /// Returns an error if the write fails.
    pub async fn send_frame(&self, frame: &[u8]) -> Result<(), quinn::WriteError> {
        let mut send = self.send.lock().await;
        send.write_all(frame).await
    }
}

/// Inner state data protected by a single RwLock.
/// This consolidates rooms and memberships to avoid nested lock acquisition.
#[derive(Debug, Clone)]
pub struct StateData {
    /// Room definitions. The Root room always has UUID 0.
    pub rooms: Vec<RoomInfo>,
    /// Mapping of user_id to room UUID for room membership.
    pub memberships: Vec<(u64 /* user_id */, Uuid /* room_uuid */)>,
}

impl StateData {
    fn new() -> Self {
        Self {
            rooms: vec![RoomInfo {
                id: Some(root_room_id()),
                name: "Root".to_string(),
            }],
            memberships: Vec::new(),
        }
    }
}

/// Compute a state hash from a ServerState message.
///
/// Re-exports the canonical hash function from the API crate.
pub fn compute_server_state_hash(server_state: &api::proto::ServerState) -> Vec<u8> {
    api::compute_server_state_hash(server_state)
}

/// The server's shared state.
///
/// This contains all mutable server state including:
/// - Connected clients (DashMap for concurrent access)
/// - Room definitions and user memberships (consolidated under one RwLock)
/// - Atomic user ID counter
///
/// # Thread Safety
///
/// - Client lookup/iteration is lock-free via DashMap
/// - State mutations acquire a write lock on `state_data`
/// - User ID allocation is lock-free via AtomicU64
pub struct ServerState {
    /// All connected clients, keyed by user_id.
    /// DashMap allows lock-free reads and fine-grained locking per entry.
    clients: DashMap<u64, Arc<ClientHandle>>,
    /// Consolidated state data (rooms + memberships).
    state_data: RwLock<StateData>,
    /// Counter for assigning unique user IDs.
    next_user_id: AtomicU64,
}

impl ServerState {
    /// Create a new server state with the default Root room.
    pub fn new() -> Self {
        Self {
            clients: DashMap::new(),
            state_data: RwLock::new(StateData::new()),
            next_user_id: AtomicU64::new(1),
        }
    }

    /// Allocate the next user ID (lock-free).
    pub fn allocate_user_id(&self) -> u64 {
        self.next_user_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a new client.
    pub fn register_client(&self, handle: Arc<ClientHandle>) {
        self.clients.insert(handle.user_id, handle);
    }

    /// Remove a client by user_id.
    pub fn remove_client(&self, user_id: u64) {
        self.clients.remove(&user_id);
    }

    /// Remove a client by Arc pointer comparison (for backward compatibility).
    pub fn remove_client_by_handle(&self, handle: &Arc<ClientHandle>) {
        self.clients.remove(&handle.user_id);
    }

    /// Get a client by user_id.
    pub fn get_client(&self, user_id: u64) -> Option<Arc<ClientHandle>> {
        self.clients.get(&user_id).map(|r| r.value().clone())
    }

    /// Get the number of connected clients.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Iterate over all clients. The closure receives each client handle.
    /// This is lock-free for reading.
    pub fn for_each_client<F>(&self, mut f: F)
    where
        F: FnMut(&Arc<ClientHandle>),
    {
        for entry in self.clients.iter() {
            f(entry.value());
        }
    }

    /// Get a snapshot of all clients for iteration outside the map.
    /// Use this when you need to perform async operations on clients.
    pub fn snapshot_clients(&self) -> Vec<Arc<ClientHandle>> {
        self.clients.iter().map(|r| r.value().clone()).collect()
    }

    /// Add a user to a room, replacing any existing membership.
    pub async fn set_user_room(&self, user_id: u64, room_uuid: Uuid) {
        let mut data = self.state_data.write().await;
        data.memberships.retain(|(uid, _)| *uid != user_id);
        data.memberships.push((user_id, room_uuid));
    }

    /// Remove a user from all rooms.
    pub async fn remove_user_membership(&self, user_id: u64) {
        let mut data = self.state_data.write().await;
        data.memberships.retain(|(uid, _)| *uid != user_id);
    }

    /// Get the room a user is currently in (read-only, fast).
    pub async fn get_user_room(&self, user_id: u64) -> Option<Uuid> {
        let data = self.state_data.read().await;
        data.memberships
            .iter()
            .find(|(uid, _)| *uid == user_id)
            .map(|(_, rid)| *rid)
    }

    /// Get all users in a specific room.
    pub async fn get_room_members(&self, room_uuid: Uuid) -> Vec<u64> {
        let data = self.state_data.read().await;
        data.memberships
            .iter()
            .filter(|(_, rid)| *rid == room_uuid)
            .map(|(uid, _)| *uid)
            .collect()
    }

    /// Create a new room and return its UUID.
    pub async fn create_room(&self, name: String) -> Uuid {
        let mut data = self.state_data.write().await;
        let new_uuid = Uuid::new_v4();
        data.rooms.push(RoomInfo {
            id: Some(room_id_from_uuid(new_uuid)),
            name,
        });
        new_uuid
    }

    /// Delete a room by UUID. Users in the room are moved to Root.
    /// Returns true if the room was found and deleted.
    pub async fn delete_room(&self, room_uuid: Uuid) -> bool {
        // Don't allow deleting the root room
        if room_uuid == ROOT_ROOM_UUID {
            return false;
        }

        let mut data = self.state_data.write().await;
        let before_len = data.rooms.len();
        data.rooms.retain(|r| {
            uuid_from_room_id(r.id.as_ref().unwrap_or(&root_room_id())) != Some(room_uuid)
        });
        let deleted = data.rooms.len() < before_len;

        if deleted {
            // Move users from deleted room to Root
            for (_, rid) in data.memberships.iter_mut() {
                if *rid == room_uuid {
                    *rid = ROOT_ROOM_UUID;
                }
            }
        }

        deleted
    }

    /// Rename a room. Returns true if the room was found.
    pub async fn rename_room(&self, room_uuid: Uuid, new_name: String) -> bool {
        let mut data = self.state_data.write().await;
        for r in data.rooms.iter_mut() {
            if uuid_from_room_id(r.id.as_ref().unwrap_or(&root_room_id())) == Some(room_uuid) {
                r.name = new_name;
                return true;
            }
        }
        false
    }

    /// Get current room list.
    pub async fn get_rooms(&self) -> Vec<RoomInfo> {
        self.state_data.read().await.rooms.clone()
    }

    /// Get a snapshot of state data for building messages.
    /// Use this to avoid holding locks during I/O.
    pub async fn snapshot_state(&self) -> StateData {
        self.state_data.read().await.clone()
    }

    /// Build the current user list.
    ///
    /// This is optimized to minimize lock contention:
    /// 1. Take a snapshot of memberships
    /// 2. Look up usernames from clients (lock-free DashMap access)
    pub async fn build_user_list(&self) -> Vec<User> {
        let data = self.state_data.read().await;
        let memberships = data.memberships.clone();
        drop(data); // Release the lock before async username lookups

        let mut users = Vec::with_capacity(memberships.len());
        for (uid, rid) in memberships {
            if let Some(client) = self.get_client(uid) {
                let username = client.get_username().await;
                users.push(User {
                    user_id: Some(UserId { value: uid }),
                    current_room: Some(room_id_from_uuid(rid)),
                    username,
                });
            }
        }
        users
    }

    /// Get room membership snapshot for voice relay.
    /// Returns a map of room_uuid -> list of user_ids.
    /// This allows the voice path to determine recipients without holding locks.
    pub async fn snapshot_room_memberships(&self) -> std::collections::HashMap<Uuid, Vec<u64>> {
        let data = self.state_data.read().await;
        let mut map = std::collections::HashMap::new();
        for (uid, rid) in &data.memberships {
            map.entry(*rid).or_insert_with(Vec::new).push(*uid);
        }
        map
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::is_root_room;

    /// Test that we can create server state and it has the Root room.
    #[tokio::test]
    async fn test_server_state_creation() {
        let state = ServerState::new();

        let rooms = state.get_rooms().await;
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].name, "Root");
        assert!(is_root_room(rooms[0].id.as_ref().unwrap()));
    }

    /// Test user ID allocation is sequential and lock-free.
    #[tokio::test]
    async fn test_user_id_allocation() {
        let state = ServerState::new();

        // No longer async - allocation is lock-free
        let id1 = state.allocate_user_id();
        let id2 = state.allocate_user_id();
        let id3 = state.allocate_user_id();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    /// Test room creation generates UUIDs.
    #[tokio::test]
    async fn test_room_creation() {
        let state = ServerState::new();

        let room_uuid = state.create_room("Test Room".to_string()).await;
        // Room should have a valid non-root UUID
        assert_ne!(room_uuid, ROOT_ROOM_UUID);

        let rooms = state.get_rooms().await;
        assert_eq!(rooms.len(), 2);
        assert!(rooms.iter().any(|r| r.name == "Test Room"));
    }

    /// Test room deletion moves users to Root.
    #[tokio::test]
    async fn test_room_deletion_moves_users() {
        let state = ServerState::new();

        // Create a room and put a user in it
        let room_uuid = state.create_room("Temp Room".to_string()).await;
        state.set_user_room(1, room_uuid).await;

        // Verify user is in the new room
        assert_eq!(state.get_user_room(1).await, Some(room_uuid));

        // Delete the room
        let deleted = state.delete_room(room_uuid).await;
        assert!(deleted);

        // User should be moved to Root
        assert_eq!(state.get_user_room(1).await, Some(ROOT_ROOM_UUID));

        // Room should be gone
        let rooms = state.get_rooms().await;
        assert_eq!(rooms.len(), 1);
    }

    /// Test that Root room cannot be deleted.
    #[tokio::test]
    async fn test_cannot_delete_root() {
        let state = ServerState::new();

        let deleted = state.delete_room(ROOT_ROOM_UUID).await;
        assert!(!deleted);

        let rooms = state.get_rooms().await;
        assert_eq!(rooms.len(), 1);
    }

    /// Test room membership tracking.
    #[tokio::test]
    async fn test_room_membership() {
        let state = ServerState::new();

        // Create rooms
        let room_a = state.create_room("Room A".to_string()).await;
        let room_b = state.create_room("Room B".to_string()).await;

        // Add users to rooms
        state.set_user_room(1, room_a).await;
        state.set_user_room(2, room_a).await;
        state.set_user_room(3, room_b).await;

        // Check memberships
        let room_a_members = state.get_room_members(room_a).await;
        assert_eq!(room_a_members.len(), 2);
        assert!(room_a_members.contains(&1));
        assert!(room_a_members.contains(&2));

        let room_b_members = state.get_room_members(room_b).await;
        assert_eq!(room_b_members.len(), 1);
        assert!(room_b_members.contains(&3));

        // Move user 1 to room B
        state.set_user_room(1, room_b).await;

        let room_a_members = state.get_room_members(room_a).await;
        assert_eq!(room_a_members.len(), 1);
        assert!(!room_a_members.contains(&1));

        let room_b_members = state.get_room_members(room_b).await;
        assert_eq!(room_b_members.len(), 2);
        assert!(room_b_members.contains(&1));
    }

    /// Test room renaming.
    #[tokio::test]
    async fn test_room_rename() {
        let state = ServerState::new();

        let room_uuid = state.create_room("Old Name".to_string()).await;
        let renamed = state.rename_room(room_uuid, "New Name".to_string()).await;
        assert!(renamed);

        let rooms = state.get_rooms().await;
        let room = rooms
            .iter()
            .find(|r| uuid_from_room_id(r.id.as_ref().unwrap()) == Some(room_uuid));
        assert_eq!(room.unwrap().name, "New Name");
    }

    /// Test renaming non-existent room returns false.
    #[tokio::test]
    async fn test_rename_nonexistent_room() {
        let state = ServerState::new();

        let fake_uuid = Uuid::new_v4();
        let renamed = state.rename_room(fake_uuid, "Whatever".to_string()).await;
        assert!(!renamed);
    }

    /// Test DashMap-based client registration and lookup.
    #[tokio::test]
    async fn test_client_registration() {
        let state = ServerState::new();

        // Create a mock endpoint for testing (we can't easily create real quinn streams)
        // For now, just test that the DashMap operations work correctly
        assert_eq!(state.client_count(), 0);

        // Test snapshot when empty
        let clients = state.snapshot_clients();
        assert!(clients.is_empty());
    }

    /// Test lock-free iteration over clients.
    #[tokio::test]
    async fn test_for_each_client() {
        let state = ServerState::new();

        // Without actual clients, just verify the method works
        let mut count = 0;
        state.for_each_client(|_| {
            count += 1;
        });
        assert_eq!(count, 0);
    }

    /// Test snapshot_state returns consistent data.
    #[tokio::test]
    async fn test_snapshot_state() {
        let state = ServerState::new();

        // Create some rooms
        let room1 = state.create_room("Room 1".to_string()).await;
        let room2 = state.create_room("Room 2".to_string()).await;

        // Add memberships
        state.set_user_room(1, room1).await;
        state.set_user_room(2, room2).await;

        // Get snapshot
        let snapshot = state.snapshot_state().await;

        assert_eq!(snapshot.rooms.len(), 3); // Root + 2 created
        assert_eq!(snapshot.memberships.len(), 2);
    }

    /// Test snapshot_room_memberships for voice relay.
    #[tokio::test]
    async fn test_snapshot_room_memberships() {
        let state = ServerState::new();

        // Create rooms and add users
        let room_a = state.create_room("Room A".to_string()).await;
        state.set_user_room(1, room_a).await;
        state.set_user_room(2, room_a).await;
        state.set_user_room(3, ROOT_ROOM_UUID).await; // Root

        let map = state.snapshot_room_memberships().await;

        assert_eq!(map.get(&room_a).unwrap().len(), 2);
        assert_eq!(map.get(&ROOT_ROOM_UUID).unwrap().len(), 1);
    }
}
