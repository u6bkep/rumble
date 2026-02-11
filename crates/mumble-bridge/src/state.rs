use std::collections::HashMap;

use uuid::Uuid;

/// Bidirectional mapping between Rumble room UUIDs and Mumble channel u32 IDs.
#[derive(Debug, Default)]
pub struct ChannelMap {
    /// Rumble UUID -> Mumble channel ID
    rumble_to_mumble: HashMap<Uuid, u32>,
    /// Mumble channel ID -> Rumble UUID
    mumble_to_rumble: HashMap<u32, Uuid>,
    /// Next available Mumble channel ID.
    next_id: u32,
}

impl ChannelMap {
    pub fn new() -> Self {
        Self {
            rumble_to_mumble: HashMap::new(),
            mumble_to_rumble: HashMap::new(),
            // Channel 0 is always the root channel
            next_id: 1,
        }
    }

    /// Get or create a Mumble channel ID for a Rumble room UUID.
    /// Root room (UUID nil) always maps to channel 0.
    pub fn get_or_insert(&mut self, uuid: Uuid) -> u32 {
        if uuid == api::ROOT_ROOM_UUID {
            self.rumble_to_mumble.insert(uuid, 0);
            self.mumble_to_rumble.insert(0, uuid);
            return 0;
        }
        if let Some(&id) = self.rumble_to_mumble.get(&uuid) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.rumble_to_mumble.insert(uuid, id);
        self.mumble_to_rumble.insert(id, uuid);
        id
    }

    pub fn get_mumble_id(&self, uuid: &Uuid) -> Option<u32> {
        self.rumble_to_mumble.get(uuid).copied()
    }

    pub fn get_rumble_uuid(&self, mumble_id: u32) -> Option<Uuid> {
        self.mumble_to_rumble.get(&mumble_id).copied()
    }

    pub fn remove_by_uuid(&mut self, uuid: &Uuid) {
        if let Some(id) = self.rumble_to_mumble.remove(uuid) {
            self.mumble_to_rumble.remove(&id);
        }
    }
}

/// Bidirectional mapping between Rumble user IDs (u64) and Mumble session IDs (u32).
#[derive(Debug, Default)]
pub struct UserMap {
    /// Rumble user_id -> Mumble session
    rumble_to_mumble: HashMap<u64, u32>,
    /// Mumble session -> Rumble user_id
    mumble_to_rumble: HashMap<u32, u64>,
    /// Next available Mumble session ID.
    next_session: u32,
}

impl UserMap {
    pub fn new() -> Self {
        Self {
            rumble_to_mumble: HashMap::new(),
            mumble_to_rumble: HashMap::new(),
            // Session ID partitioning:
            //   1-999:       reserved
            //   1000-99999:  Mumble client sessions (BridgeState.next_mumble_session)
            //   100000+:     Rumble user proxy sessions (UserMap.next_session)
            next_session: 100_000,
        }
    }

    /// Get or create a Mumble session ID for a Rumble user ID.
    pub fn get_or_insert(&mut self, rumble_id: u64) -> u32 {
        if let Some(&session) = self.rumble_to_mumble.get(&rumble_id) {
            return session;
        }
        let session = self.next_session;
        self.next_session += 1;
        self.rumble_to_mumble.insert(rumble_id, session);
        self.mumble_to_rumble.insert(session, rumble_id);
        session
    }

    pub fn get_mumble_session(&self, rumble_id: u64) -> Option<u32> {
        self.rumble_to_mumble.get(&rumble_id).copied()
    }

    pub fn get_rumble_id(&self, session: u32) -> Option<u64> {
        self.mumble_to_rumble.get(&session).copied()
    }

    pub fn remove_by_rumble_id(&mut self, rumble_id: u64) {
        if let Some(session) = self.rumble_to_mumble.remove(&rumble_id) {
            self.mumble_to_rumble.remove(&session);
        }
    }
}

/// Information about a connected Mumble client.
#[derive(Debug, Clone)]
pub struct MumbleClient {
    /// The Mumble session ID assigned to this client.
    pub session: u32,
    /// The username the Mumble client authenticated with.
    pub username: String,
    /// The Mumble channel ID the client is currently in.
    pub channel_id: u32,
}

/// Shared bridge state, protected by RwLock externally.
#[derive(Debug)]
pub struct BridgeState {
    /// Channel (room) mapping.
    pub channels: ChannelMap,
    /// User mapping for Rumble users (remote users seen via Rumble state).
    pub users: UserMap,
    /// Connected Mumble clients, keyed by their Mumble session ID.
    pub mumble_clients: HashMap<u32, MumbleClient>,
    /// Next Mumble session ID for local Mumble clients.
    pub next_mumble_session: u32,
    /// Cached Rumble rooms (from latest ServerState/StateUpdate).
    pub rumble_rooms: Vec<api::proto::RoomInfo>,
    /// Cached Rumble users (from latest ServerState/StateUpdate).
    pub rumble_users: Vec<api::proto::User>,
    /// Our bridge user ID on the Rumble server.
    pub bridge_user_id: Option<u64>,
    /// Whether we are connected to the Rumble server.
    pub rumble_connected: bool,

    // -- Virtual user tracking for bridge protocol --
    /// Pending registrations: (username, mumble_session) pairs awaiting BridgeUserRegistered.
    /// Uses a Vec to support multiple simultaneous registrations with the same username.
    pub pending_registrations: Vec<(String, u32)>,
    /// Mumble session -> Rumble virtual user ID (for registered virtual users).
    pub virtual_user_map: HashMap<u32, u64>,
    /// Rumble virtual user ID -> Mumble session (reverse lookup).
    pub reverse_virtual_user_map: HashMap<u64, u32>,
}

impl BridgeState {
    pub fn new() -> Self {
        Self {
            channels: ChannelMap::new(),
            users: UserMap::new(),
            mumble_clients: HashMap::new(),
            next_mumble_session: 1000, // Start Mumble client sessions at 1000 to avoid overlap
            rumble_rooms: Vec::new(),
            rumble_users: Vec::new(),
            bridge_user_id: None,
            rumble_connected: false,
            pending_registrations: Vec::new(),
            virtual_user_map: HashMap::new(),
            reverse_virtual_user_map: HashMap::new(),
        }
    }

    /// Allocate a new session ID for a Mumble client.
    pub fn allocate_mumble_session(&mut self) -> u32 {
        let session = self.next_mumble_session;
        self.next_mumble_session += 1;
        session
    }
}
