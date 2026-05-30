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

use dashmap::DashMap;
use quinn::Connection;
use rumble_protocol::{
    ROOT_ROOM_UUID,
    proto::{RoomInfo, User, UserId},
    room_id_from_uuid, root_room_id, uuid_from_room_id,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use std::{sync::atomic::AtomicBool, time::Instant};

/// Per-user voice datagram rate limiter using a fixed time window.
///
/// Tracks bytes sent within a 1-second window. If the byte count exceeds
/// the limit, further datagrams are dropped until the window resets.
#[derive(Debug)]
pub struct VoiceRateLimit {
    /// Start of the current measurement window.
    window_start: Instant,
    /// Bytes counted in the current window.
    bytes_in_window: usize,
}

/// Maximum voice bytes per second per user (32 KB/s).
/// Generous for 64kbps Opus at 20ms frames (~8 KB/s pure opus + protobuf overhead).
const VOICE_RATE_LIMIT_BYTES_PER_SEC: usize = 32_000;

impl VoiceRateLimit {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            bytes_in_window: 0,
        }
    }

    /// Check if sending `bytes` is allowed. Returns true if within the limit.
    /// Automatically resets the window if the previous one has elapsed.
    fn check(&mut self, bytes: usize) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start).as_secs() >= 1 {
            // Window has elapsed, reset
            self.window_start = now;
            self.bytes_in_window = bytes;
            return true;
        }
        if self.bytes_in_window + bytes > VOICE_RATE_LIMIT_BYTES_PER_SEC {
            return false;
        }
        self.bytes_in_window += bytes;
        true
    }
}

/// A roster member's identity — the single home for display name, ACL groups,
/// and display marker. Orthogonal to how the member is connected or driven
/// (see [`Binding`]). A connected client and its [`Member`] entry hold the
/// *same* `Arc<Identity>`, so there is exactly one home for these fields.
#[derive(Debug)]
pub struct Identity {
    /// Display name (roster + chat attribution). Mutable over a session.
    display_name: RwLock<String>,
    /// Verified ACL-identity name. Drives the implicit username-group during
    /// ACL evaluation. `None` for anonymous participants (no verified
    /// identity), which is exactly why they never mint an implicit
    /// username-group — see `acl::evaluate_user_permissions`.
    verified_username: RwLock<Option<String>>,
    /// Freeform display marker set by a controller/plugin (e.g. "Mumble",
    /// "bot"). `None` for humans. Immutable after construction.
    pub label: Option<String>,
    /// Permission groups (loaded from persistence on auth for humans; set at
    /// mint time for participants).
    groups: RwLock<Vec<String>>,
}

impl Identity {
    /// A fresh, empty identity for a just-connected client (filled in at auth).
    pub fn empty() -> Self {
        Self {
            display_name: RwLock::new(String::new()),
            verified_username: RwLock::new(None),
            label: None,
            groups: RwLock::new(Vec::new()),
        }
    }

    /// Identity for a minted participant.
    pub fn participant(
        display_name: String,
        verified_username: Option<String>,
        label: Option<String>,
        groups: Vec<String>,
    ) -> Self {
        Self {
            display_name: RwLock::new(display_name),
            verified_username: RwLock::new(verified_username),
            label,
            groups: RwLock::new(groups),
        }
    }

    pub async fn display_name(&self) -> String {
        self.display_name.read().await.clone()
    }
    pub async fn set_display_name(&self, name: String) {
        *self.display_name.write().await = name;
    }
    pub async fn verified_username(&self) -> Option<String> {
        self.verified_username.read().await.clone()
    }
    pub async fn set_verified_username(&self, name: Option<String>) {
        *self.verified_username.write().await = name;
    }
    pub async fn groups(&self) -> Vec<String> {
        self.groups.read().await.clone()
    }
    pub async fn set_groups(&self, groups: Vec<String>) {
        *self.groups.write().await = groups;
    }
}

/// Who controls (and bounds the lifetime of) an owned participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerId {
    /// An in-process plugin, identified by its registration index.
    Plugin(u64),
    /// An out-of-process controller connection, by its user_id.
    Connection(u64),
}

/// How a roster [`Member`] is connected and driven.
#[derive(Debug, Clone)]
pub enum Binding {
    /// A live client connection. Self-owned; dies with the connection.
    Client(Arc<ClientHandle>),
    /// A participant with no connection of its own, driven by a controller.
    Owned { owner: OwnerId },
}

/// A roster member: identity plus binding. Covers humans and participants
/// alike — the unification of the old client table and virtual-user table.
#[derive(Debug, Clone)]
pub struct Member {
    pub user_id: u64,
    pub identity: Arc<Identity>,
    pub binding: Binding,
}

impl Member {
    /// True if this member is an out-of-process controller connection, which
    /// is hidden from the visible roster.
    pub fn is_controller(&self) -> bool {
        match &self.binding {
            Binding::Client(h) => h.is_controller.load(Ordering::SeqCst),
            Binding::Owned { .. } => false,
        }
    }

    /// True if this member is an elevated superuser (only connections can be).
    pub fn is_superuser(&self) -> bool {
        match &self.binding {
            Binding::Client(h) => h.is_superuser.load(Ordering::Relaxed),
            Binding::Owned { .. } => false,
        }
    }

    /// The connection handle if this member is itself a live client.
    pub fn client(&self) -> Option<&Arc<ClientHandle>> {
        match &self.binding {
            Binding::Client(h) => Some(h),
            Binding::Owned { .. } => None,
        }
    }
}

/// Handle to a connected client's control stream — connection runtime only.
/// Identity (name/groups/label) lives on the member's [`Identity`], shared by
/// `Arc` with this handle.
#[derive(Debug)]
pub struct ClientHandle {
    /// Send stream for the client's control channel.
    /// Locked only for the duration of a single message write.
    pub send: Mutex<quinn::SendStream>,
    /// Server-assigned user ID (immutable after creation).
    pub user_id: u64,
    /// The underlying QUIC connection (for datagrams).
    pub conn: Connection,
    /// The client's Ed25519 public key (set after authentication).
    pub public_key: Arc<RwLock<Option<[u8; 32]>>>,
    /// Whether authentication is complete.
    pub authenticated: Arc<AtomicBool>,
    /// Whether this connection has registered as a controller (ControllerHello).
    pub is_controller: AtomicBool,
    /// Whether this user has been elevated to superuser (session-only).
    pub is_superuser: AtomicBool,
    /// Whether this user is server-muted (effective state, checked on voice relay).
    pub server_muted: AtomicBool,
    /// Whether a moderator manually server-muted this user.
    pub manually_server_muted: AtomicBool,
    /// Shared identity — the same `Arc<Identity>` held by this client's [`Member`].
    pub identity: Arc<Identity>,
}

impl ClientHandle {
    /// Create a new client handle, sharing the given identity with its member.
    pub fn new(
        send: quinn::SendStream,
        user_id: u64,
        conn: Connection,
        public_key: Arc<RwLock<Option<[u8; 32]>>>,
        authenticated: Arc<AtomicBool>,
        identity: Arc<Identity>,
    ) -> Self {
        Self {
            send: Mutex::new(send),
            user_id,
            conn,
            public_key,
            authenticated,
            is_controller: AtomicBool::new(false),
            is_superuser: AtomicBool::new(false),
            server_muted: AtomicBool::new(false),
            manually_server_muted: AtomicBool::new(false),
            identity,
        }
    }

    /// Set the display name. Delegates to the shared identity.
    pub async fn set_username(&self, name: String) {
        self.identity.set_display_name(name).await;
    }

    /// Get the display name. Delegates to the shared identity.
    pub async fn get_username(&self) -> String {
        self.identity.display_name().await
    }

    /// Send a framed message to this client.
    /// Returns an error if the write fails.
    pub async fn send_frame(&self, frame: &[u8]) -> Result<(), quinn::WriteError> {
        let mut send = self.send.lock().await;
        tracing::info!("Sending frame of size {} to user {}", frame.len(), self.user_id);
        send.write_all(frame).await
    }
}

/// User status information (mute/deafen state).
#[derive(Debug, Clone, Copy, Default)]
pub struct UserStatus {
    /// User has muted themselves (not transmitting).
    pub is_muted: bool,
    /// User has deafened themselves (not receiving audio; implies muted).
    pub is_deafened: bool,
    /// User is server-muted (cannot transmit voice).
    pub server_muted: bool,
    /// User has elevated to superuser for this session.
    pub is_elevated: bool,
}

/// Inner state data protected by a single RwLock.
/// This consolidates rooms and memberships to avoid nested lock acquisition.
#[derive(Debug, Clone)]
pub struct StateData {
    /// Room definitions. The Root room always has UUID 0.
    pub rooms: Vec<RoomInfo>,
    /// Mapping of user_id to room UUID for room membership.
    pub memberships: Vec<(u64 /* user_id */, Uuid /* room_uuid */)>,
    /// Mapping of user_id to mute/deafen status.
    pub user_statuses: Vec<(u64 /* user_id */, UserStatus)>,
}

impl StateData {
    fn new() -> Self {
        Self {
            rooms: vec![RoomInfo {
                id: Some(root_room_id()),
                name: "Root".to_string(),
                parent_id: None,
                description: None,
                inherit_acl: true,
                acls: vec![],
                effective_permissions: 0,
            }],
            memberships: Vec::new(),
            user_statuses: Vec::new(),
        }
    }

    /// Create an empty `StateData` with no rooms or memberships.
    ///
    /// Used as a fallback when a synchronous snapshot cannot acquire the lock.
    pub fn empty() -> Self {
        Self {
            rooms: Vec::new(),
            memberships: Vec::new(),
            user_statuses: Vec::new(),
        }
    }
}

/// Compute a state hash from a ServerState message.
///
/// Re-exports the canonical hash function from the rumble-protocol crate.
pub fn compute_server_state_hash(server_state: &rumble_protocol::proto::ServerState) -> Vec<u8> {
    rumble_protocol::compute_server_state_hash(server_state)
}

/// Pending authentication state for a client.
#[derive(Debug)]
pub struct PendingAuth {
    /// Random challenge nonce (32 bytes).
    pub nonce: [u8; 32],
    /// Assigned user ID for this session.
    pub user_id: u64,
    /// Client's Ed25519 public key.
    pub public_key: [u8; 32],
    /// Timestamp when the auth started (for timeout).
    pub timestamp: Instant,
    /// Username claimed by the client.
    pub username: String,
}

/// Active session information for a connected client.
#[derive(Clone, Debug)]
pub struct SessionEntry {
    /// The user's long-term Ed25519 public key.
    pub user_public_key: [u8; 32],
    /// Ephemeral session public key used for P2P (32 bytes).
    pub session_public_key: [u8; 32],
    /// Stable session identifier derived from `session_public_key`.
    pub session_id: [u8; 32],
    /// Certificate issuance time (ms since epoch).
    pub issued_ms: i64,
    /// Certificate expiry time (ms since epoch).
    pub expires_ms: i64,
    /// Optional device label provided by the client.
    pub device: Option<String>,
}

/// The server's shared state.
///
/// This contains all mutable server state including:
/// - Connected clients (DashMap for concurrent access)
/// - Room definitions and user memberships (consolidated under one RwLock)
/// - Atomic user ID counter
/// - Persistence layer for registered users and known keys
/// - Pending authentication state
///
/// # Thread Safety
///
/// - Client lookup/iteration is lock-free via DashMap
/// - State mutations acquire a write lock on `state_data`
/// - User ID allocation is lock-free via AtomicU64
pub struct ServerState {
    /// All roster members, keyed by user_id — connected clients and
    /// controller-driven participants alike. DashMap allows lock-free reads
    /// and fine-grained locking per entry.
    members: DashMap<u64, Arc<Member>>,
    /// Consolidated state data (rooms + memberships).
    state_data: RwLock<StateData>,
    /// Counter for assigning unique user IDs.
    next_user_id: AtomicU64,
    /// Pending authentication state: user_id → PendingAuth.
    pending_auth: DashMap<u64, PendingAuth>,
    /// Active sessions: user_id → session entry (long-term + session keys).
    sessions: DashMap<u64, SessionEntry>,
    /// Reverse index: session_id → user_id for peer mapping.
    sessions_by_id: DashMap<[u8; 32], u64>,
    /// Server's TLS certificate DER bytes (for computing cert hash).
    server_cert_der: Vec<u8>,
    /// Per-user voice datagram rate limiters: user_id → VoiceRateLimit.
    voice_rate_limits: DashMap<u64, VoiceRateLimit>,
    /// Welcome message (MOTD) sent to clients after authentication.
    welcome_message: Option<String>,
}

impl ServerState {
    /// Create a new server state with the default Root room.
    pub fn new() -> Self {
        Self {
            members: DashMap::new(),
            state_data: RwLock::new(StateData::new()),
            next_user_id: AtomicU64::new(1),
            pending_auth: DashMap::new(),
            sessions: DashMap::new(),
            sessions_by_id: DashMap::new(),
            server_cert_der: Vec::new(),
            voice_rate_limits: DashMap::new(),
            welcome_message: None,
        }
    }

    /// Create a new server state with the given server certificate.
    pub fn with_cert(cert_der: Vec<u8>) -> Self {
        Self {
            members: DashMap::new(),
            state_data: RwLock::new(StateData::new()),
            next_user_id: AtomicU64::new(1),
            pending_auth: DashMap::new(),
            sessions: DashMap::new(),
            sessions_by_id: DashMap::new(),
            server_cert_der: cert_der,
            voice_rate_limits: DashMap::new(),
            welcome_message: None,
        }
    }

    /// Create a new server state with the given server certificate and welcome message.
    pub fn with_cert_and_welcome(cert_der: Vec<u8>, welcome_message: Option<String>) -> Self {
        let mut state = Self::with_cert(cert_der);
        state.welcome_message = welcome_message;
        state
    }

    /// Get the welcome message.
    pub fn welcome_message(&self) -> Option<&str> {
        self.welcome_message.as_deref()
    }

    /// Get the server's certificate hash.
    pub fn server_cert_hash(&self) -> [u8; 32] {
        rumble_protocol::compute_cert_hash(&self.server_cert_der)
    }

    /// Store pending authentication state.
    pub fn set_pending_auth(&self, pending: PendingAuth) {
        self.pending_auth.insert(pending.user_id, pending);
    }

    /// Get and remove pending authentication state.
    pub fn take_pending_auth(&self, user_id: u64) -> Option<PendingAuth> {
        self.pending_auth.remove(&user_id).map(|(_, v)| v)
    }

    /// Track an active session for a user.
    pub fn add_session(&self, user_id: u64, session: SessionEntry) {
        self.sessions.insert(user_id, session.clone());
        self.sessions_by_id.insert(session.session_id, user_id);
    }

    /// Remove an active session.
    pub fn remove_session(&self, user_id: u64) {
        if let Some((_, session)) = self.sessions.remove(&user_id) {
            self.sessions_by_id.remove(&session.session_id);
        }
    }

    /// Get the session entry for a user.
    pub fn get_session(&self, user_id: u64) -> Option<SessionEntry> {
        self.sessions.get(&user_id).map(|r| r.value().clone())
    }

    /// Get the user's long-term public key for a session.
    pub fn get_user_public_key(&self, user_id: u64) -> Option<[u8; 32]> {
        self.sessions.get(&user_id).map(|r| r.user_public_key)
    }

    /// Find a session entry by its session_id.
    pub fn get_session_by_id(&self, session_id: &[u8; 32]) -> Option<(u64, SessionEntry)> {
        self.sessions_by_id.get(session_id).and_then(|uid| {
            self.sessions
                .get(uid.value())
                .map(|s| (*uid.value(), s.value().clone()))
        })
    }

    /// Allocate the next user ID (lock-free).
    pub fn allocate_user_id(&self) -> u64 {
        self.next_user_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a new client.
    pub fn register_client(&self, handle: Arc<ClientHandle>) {
        let member = Arc::new(Member {
            user_id: handle.user_id,
            identity: handle.identity.clone(),
            binding: Binding::Client(handle.clone()),
        });
        self.members.insert(handle.user_id, member);
    }

    /// Remove a member (client or participant) by user_id.
    pub fn remove_client(&self, user_id: u64) {
        self.members.remove(&user_id);
    }

    /// Remove a member by the handle's user_id.
    pub fn remove_client_by_handle(&self, handle: &Arc<ClientHandle>) {
        self.members.remove(&handle.user_id);
    }

    /// Get the connection handle for a user_id, if that member is a live client.
    pub fn get_client(&self, user_id: u64) -> Option<Arc<ClientHandle>> {
        self.members.get(&user_id).and_then(|m| m.client().cloned())
    }

    /// Get a roster member (client or participant) by user_id.
    pub fn get_member(&self, user_id: u64) -> Option<Arc<Member>> {
        self.members.get(&user_id).map(|r| r.value().clone())
    }

    /// Get the number of connected clients (excludes participants).
    pub fn client_count(&self) -> usize {
        self.members.iter().filter(|m| m.client().is_some()).count()
    }

    /// Iterate over all live client connections. The closure receives each
    /// client handle. This is lock-free for reading.
    pub fn for_each_client<F>(&self, mut f: F)
    where
        F: FnMut(&Arc<ClientHandle>),
    {
        for entry in self.members.iter() {
            if let Some(handle) = entry.value().client() {
                f(handle);
            }
        }
    }

    /// Get a snapshot of all live client connections for iteration outside the
    /// map. Use this when you need to perform async operations on clients.
    pub fn snapshot_clients(&self) -> Vec<Arc<ClientHandle>> {
        self.members
            .iter()
            .filter_map(|r| r.value().client().cloned())
            .collect()
    }

    // =========================================================================
    // Participant / Member Methods
    // =========================================================================

    /// Register a controller-driven participant member.
    pub fn register_participant(&self, member: Arc<Member>) {
        self.members.insert(member.user_id, member);
    }

    /// True if `user_id` is a participant driven by the given owner.
    pub fn is_participant_of(&self, user_id: u64, owner: OwnerId) -> bool {
        self.members.get(&user_id).is_some_and(|m| match &m.binding {
            Binding::Owned { owner: o } => *o == owner,
            Binding::Client(_) => false,
        })
    }

    /// All participant user_ids driven by the given owner.
    pub fn members_by_owner(&self, owner: OwnerId) -> Vec<u64> {
        self.members
            .iter()
            .filter(|m| matches!(&m.binding, Binding::Owned { owner: o } if *o == owner))
            .map(|m| m.user_id)
            .collect()
    }

    /// Resolve a member to the connection that voice/chat for it should be
    /// delivered over: a live client delivers to itself; an owned participant
    /// delivers to its controller connection; a plugin-owned participant has
    /// no QUIC delivery target.
    pub fn delivery_client(&self, user_id: u64) -> Option<Arc<ClientHandle>> {
        let member = self.members.get(&user_id)?;
        match &member.binding {
            Binding::Client(h) => Some(h.clone()),
            Binding::Owned {
                owner: OwnerId::Connection(c),
            } => self.get_client(*c),
            Binding::Owned {
                owner: OwnerId::Plugin(_),
            } => None,
        }
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
        data.user_statuses.retain(|(uid, _)| *uid != user_id);
    }

    /// Set a user's mute/deafen status.
    pub async fn set_user_status(&self, user_id: u64, status: UserStatus) {
        let mut data = self.state_data.write().await;
        // Remove existing status entry if present
        data.user_statuses.retain(|(uid, _)| *uid != user_id);
        data.user_statuses.push((user_id, status));
    }

    /// Get a user's mute/deafen status.
    pub async fn get_user_status(&self, user_id: u64) -> UserStatus {
        let data = self.state_data.read().await;
        data.user_statuses
            .iter()
            .find(|(uid, _)| *uid == user_id)
            .map(|(_, status)| *status)
            .unwrap_or_default()
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

    /// Add a room with a specific UUID (for loading from persistence).
    /// Returns false if a room with that UUID already exists.
    pub async fn add_room_with_uuid(&self, uuid: Uuid, name: String) -> bool {
        self.add_room_with_uuid_and_parent(uuid, name, None).await
    }

    /// Add a room with a specific UUID and parent (for loading from persistence).
    /// Returns false if a room with that UUID already exists.
    pub async fn add_room_with_uuid_and_parent(&self, uuid: Uuid, name: String, parent: Option<Uuid>) -> bool {
        self.add_room_with_uuid_and_parent_desc(uuid, name, parent, None).await
    }

    /// Add a room with a specific UUID, parent, and description (for loading from persistence).
    /// Returns false if a room with that UUID already exists.
    pub async fn add_room_with_uuid_and_parent_desc(
        &self,
        uuid: Uuid,
        name: String,
        parent: Option<Uuid>,
        description: Option<String>,
    ) -> bool {
        let mut data = self.state_data.write().await;
        // Check if room already exists
        let exists = data
            .rooms
            .iter()
            .any(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(uuid));
        if exists {
            return false;
        }
        data.rooms.push(RoomInfo {
            id: Some(room_id_from_uuid(uuid)),
            name,
            parent_id: parent.map(room_id_from_uuid),
            description,
            inherit_acl: true,
            acls: vec![],
            effective_permissions: 0,
        });
        true
    }

    /// Create a new room and return its UUID.
    pub async fn create_room(&self, name: String) -> Uuid {
        self.create_room_with_parent(name, None).await
    }

    /// Create a new room with a parent and return its UUID.
    pub async fn create_room_with_parent(&self, name: String, parent: Option<Uuid>) -> Uuid {
        self.create_room_with_parent_desc(name, parent, None).await
    }

    /// Create a new room with a parent and description, returning its UUID.
    pub async fn create_room_with_parent_desc(
        &self,
        name: String,
        parent: Option<Uuid>,
        description: Option<String>,
    ) -> Uuid {
        let mut data = self.state_data.write().await;
        let new_uuid = Uuid::new_v4();
        data.rooms.push(RoomInfo {
            id: Some(room_id_from_uuid(new_uuid)),
            name,
            parent_id: parent.map(room_id_from_uuid),
            description,
            inherit_acl: true,
            acls: vec![],
            effective_permissions: 0,
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
        data.rooms
            .retain(|r| uuid_from_room_id(r.id.as_ref().unwrap_or(&root_room_id())) != Some(room_uuid));
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

    /// Move a room to a new parent. Returns true if the room was found and moved.
    pub async fn move_room(&self, room_uuid: Uuid, new_parent_uuid: Uuid) -> bool {
        let mut data = self.state_data.write().await;
        for r in data.rooms.iter_mut() {
            if uuid_from_room_id(r.id.as_ref().unwrap_or(&root_room_id())) == Some(room_uuid) {
                r.parent_id = Some(room_id_from_uuid(new_parent_uuid));
                return true;
            }
        }
        false
    }

    /// Update a room's ACL data in-memory. Returns true if the room was found.
    pub async fn set_room_acl(
        &self,
        room_uuid: Uuid,
        inherit_acl: bool,
        acls: Vec<rumble_protocol::proto::RoomAclEntry>,
    ) -> bool {
        let mut data = self.state_data.write().await;
        for r in data.rooms.iter_mut() {
            if uuid_from_room_id(r.id.as_ref().unwrap_or(&root_room_id())) == Some(room_uuid) {
                r.inherit_acl = inherit_acl;
                r.acls = acls;
                return true;
            }
        }
        false
    }

    /// Set a room's description. Returns true if the room was found.
    pub async fn set_room_description(&self, room_uuid: Uuid, description: String) -> bool {
        let mut data = self.state_data.write().await;
        for r in data.rooms.iter_mut() {
            if uuid_from_room_id(r.id.as_ref().unwrap_or(&root_room_id())) == Some(room_uuid) {
                r.description = if description.is_empty() {
                    None
                } else {
                    Some(description)
                };
                return true;
            }
        }
        false
    }

    /// Get current room list.
    pub async fn get_rooms(&self) -> Vec<RoomInfo> {
        self.state_data.read().await.rooms.clone()
    }

    /// Build the room list with ACL data populated from persistence.
    pub async fn build_room_list(
        &self,
        persistence: &Option<std::sync::Arc<crate::persistence::Persistence>>,
    ) -> Vec<RoomInfo> {
        let mut rooms = self.get_rooms().await;
        if let Some(persist) = persistence {
            for room in &mut rooms {
                if let Some(uuid) = room.id.as_ref().and_then(uuid_from_room_id)
                    && let Some(acl) = persist.get_room_acl(uuid.as_bytes())
                {
                    room.inherit_acl = acl.inherit_acl;
                    room.acls = acl
                        .entries
                        .iter()
                        .map(|e| rumble_protocol::proto::RoomAclEntry {
                            group: e.group.clone(),
                            grant: e.grant,
                            deny: e.deny,
                            apply_here: e.apply_here,
                            apply_subs: e.apply_subs,
                        })
                        .collect();
                }
            }
        }
        rooms
    }

    /// Get a snapshot of state data for building messages.
    /// Use this to avoid holding locks during I/O.
    pub async fn snapshot_state(&self) -> StateData {
        self.state_data.read().await.clone()
    }

    /// Synchronous best-effort snapshot of state data.
    ///
    /// Uses `try_read()` to avoid blocking. Returns an empty `StateData`
    /// if the write lock is currently held. This is intended for plugin
    /// context methods that cannot be async.
    pub fn snapshot_state_sync(&self) -> StateData {
        match self.state_data.try_read() {
            Ok(data) => data.clone(),
            Err(_) => StateData::empty(),
        }
    }

    /// Build the current user list, including virtual users.
    ///
    /// This is optimized to minimize lock contention:
    /// 1. Take a snapshot of memberships and statuses
    /// 2. Look up usernames from clients (lock-free DashMap access) or virtual users
    ///
    /// Bridge connections (is_bridge=true) are excluded from the user list
    /// since they are infrastructure, not visible users.
    pub async fn build_user_list(&self) -> Vec<User> {
        let data = self.state_data.read().await;
        let memberships = data.memberships.clone();
        let user_statuses = data.user_statuses.clone();
        drop(data); // Release the lock before async username lookups

        let mut users = Vec::with_capacity(memberships.len());
        for (uid, rid) in memberships {
            // Find the user's status (default to not muted/deafened)
            let status = user_statuses
                .iter()
                .find(|(id, _)| *id == uid)
                .map(|(_, s)| *s)
                .unwrap_or_default();

            let Some(member) = self.get_member(uid) else {
                continue;
            };
            // Controller connections are infrastructure, not visible users.
            if member.is_controller() {
                continue;
            }

            // Server-mute is connection runtime for clients; for participants
            // it lives in the membership status snapshot.
            let srv_muted = match &member.binding {
                Binding::Client(h) => h.server_muted.load(Ordering::Relaxed),
                Binding::Owned { .. } => status.server_muted,
            };

            users.push(User {
                user_id: Some(UserId { value: uid }),
                current_room: Some(room_id_from_uuid(rid)),
                username: member.identity.display_name().await,
                is_muted: status.is_muted,
                is_deafened: status.is_deafened,
                server_muted: srv_muted,
                is_elevated: member.is_superuser(),
                groups: member.identity.groups().await,
                label: member.identity.label.clone(),
            });
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

    // =========================================================================
    // Voice Rate Limiting
    // =========================================================================

    /// Check whether a voice datagram of `bytes` size is allowed for the given user.
    /// Returns `true` if allowed, `false` if the user has exceeded the rate limit.
    pub fn check_voice_rate(&self, user_id: u64, bytes: usize) -> bool {
        let mut entry = self
            .voice_rate_limits
            .entry(user_id)
            .or_insert_with(VoiceRateLimit::new);
        entry.check(bytes)
    }

    /// Remove rate limit state for a user (called on disconnect).
    pub fn remove_voice_rate(&self, user_id: u64) {
        self.voice_rate_limits.remove(&user_id);
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
    use rumble_protocol::is_root_room;

    /// Test that we can create server state and it has the Root room.
    #[tokio::test]
    async fn test_server_state_creation() {
        let state = ServerState::new();

        let rooms = state.get_rooms().await;
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].name, "Root");
        assert!(is_root_room(rooms[0].id.as_ref().unwrap()));
    }

    /// User ID allocation is lock-free and yields unique, increasing IDs.
    #[tokio::test]
    async fn test_user_id_allocation() {
        let state = ServerState::new();

        let id1 = state.allocate_user_id();
        let id2 = state.allocate_user_id();
        let id3 = state.allocate_user_id();

        // Contract: IDs are unique and strictly increasing. The exact base
        // value and step are implementation details — pinning them (1, 2, 3)
        // would fight a pooled/offset allocator change that is still correct.
        assert!(
            id1 < id2 && id2 < id3,
            "user IDs must be strictly increasing: {id1}, {id2}, {id3}"
        );
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

    /// Test adding a room with a specific UUID (for loading from persistence).
    #[tokio::test]
    async fn test_add_room_with_uuid() {
        let state = ServerState::new();
        let specific_uuid = Uuid::new_v4();

        // Add room with specific UUID
        let added = state
            .add_room_with_uuid(specific_uuid, "Persisted Room".to_string())
            .await;
        assert!(added);

        // Verify the room exists with the correct UUID
        let rooms = state.get_rooms().await;
        assert_eq!(rooms.len(), 2);

        let room = rooms.iter().find(|r| r.name == "Persisted Room").unwrap();
        assert_eq!(uuid_from_room_id(room.id.as_ref().unwrap()), Some(specific_uuid));

        // Try to add a room with the same UUID - should fail
        let added_again = state.add_room_with_uuid(specific_uuid, "Duplicate".to_string()).await;
        assert!(!added_again);

        // Room count should still be 2
        let rooms = state.get_rooms().await;
        assert_eq!(rooms.len(), 2);
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

    /// Build an owned participant member for the state-projection tests.
    fn participant(uid: u64, name: &str, label: Option<&str>, groups: &[&str]) -> Arc<Member> {
        Arc::new(Member {
            user_id: uid,
            identity: Arc::new(Identity::participant(
                name.to_string(),
                None,
                label.map(str::to_string),
                groups.iter().map(|s| s.to_string()).collect(),
            )),
            binding: Binding::Owned {
                owner: OwnerId::Plugin(1),
            },
        })
    }

    /// `build_user_list` projects roster members with a room membership into
    /// the wire `User` list, carrying username/room/groups/label and the
    /// mute/deafen/server-mute status. Members without a membership are
    /// omitted. (Controller-connection exclusion is not covered here — it
    /// requires a live client connection; see the integration tests.)
    #[tokio::test]
    async fn build_user_list_projects_members_rooms_and_status() {
        let state = ServerState::new();
        let room_a = state.create_room("Room A".to_string()).await;

        state.register_participant(participant(1, "alice", Some("Mumble"), &["mods"]));
        state.register_participant(participant(2, "bob", None, &[]));
        state.set_user_room(1, room_a).await;
        state.set_user_room(2, ROOT_ROOM_UUID).await;
        state
            .set_user_status(
                1,
                UserStatus {
                    is_muted: true,
                    is_deafened: false,
                    server_muted: true,
                    is_elevated: false,
                },
            )
            .await;

        // Registered but never placed in a room → must not surface.
        state.register_participant(participant(3, "ghost", None, &[]));

        let mut users = state.build_user_list().await;
        users.sort_by_key(|u| u.user_id.as_ref().unwrap().value);

        assert_eq!(users.len(), 2, "only members with a room membership are listed");

        let alice = &users[0];
        assert_eq!(alice.user_id.as_ref().unwrap().value, 1);
        assert_eq!(alice.username, "alice");
        assert_eq!(alice.current_room.as_ref().and_then(uuid_from_room_id), Some(room_a));
        assert_eq!(alice.groups, vec!["mods".to_string()]);
        assert_eq!(alice.label.as_deref(), Some("Mumble"));
        assert!(alice.is_muted && alice.server_muted && !alice.is_deafened);

        let bob = &users[1];
        assert_eq!(bob.username, "bob");
        assert_eq!(
            bob.current_room.as_ref().and_then(uuid_from_room_id),
            Some(ROOT_ROOM_UUID)
        );
        assert!(!bob.is_muted && !bob.server_muted);
        assert!(bob.groups.is_empty());
        assert_eq!(bob.label, None);
    }

    /// The server hashes a rebuilt `ServerState` on every state change and
    /// clients use that hash to detect divergence. Rebuilding the same logical
    /// state must produce an identical hash — the hash sorts rooms/users, so
    /// the nondeterministic `DashMap` iteration order must not leak through.
    #[tokio::test]
    async fn server_state_hash_is_stable_across_rebuilds() {
        let state = ServerState::new();
        let room = state.create_room("Room A".to_string()).await;
        for uid in 1..=5u64 {
            state.register_participant(participant(uid, &format!("u{uid}"), None, &[]));
            state
                .set_user_room(uid, if uid % 2 == 0 { room } else { ROOT_ROOM_UUID })
                .await;
        }

        let hash_now = || async {
            let rooms = state.build_room_list(&None).await;
            let users = state.build_user_list().await;
            compute_server_state_hash(&rumble_protocol::proto::ServerState {
                rooms,
                users,
                groups: vec![],
            })
        };

        let h1 = hash_now().await;
        let h2 = hash_now().await;
        assert!(!h1.is_empty());
        assert_eq!(h1, h2, "rebuilding the same logical state must hash identically");
    }
}
