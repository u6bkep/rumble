//! Backend state and commands for UI integration.
//!
//! This module provides a state-driven API for communication between
//! the UI layer and the backend. The UI reads state and sends commands.
//! The backend updates state and calls the repaint callback.

use crate::audio::AudioDeviceInfo;
use api::proto::{RoomInfo, User};
use std::collections::HashSet;
use uuid::Uuid;

/// Re-export ROOT_ROOM_UUID for convenience
pub use api::ROOT_ROOM_UUID;

// =============================================================================
// Connection State
// =============================================================================

/// Connection lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Not connected to any server.
    Disconnected,
    /// Attempting to establish a connection.
    Connecting { server_addr: String },
    /// Successfully connected and authenticated.
    Connected { server_name: String, user_id: u64 },
    /// Connection was unexpectedly lost.
    ConnectionLost { error: String },
}

impl Default for ConnectionState {
    fn default() -> Self {
        ConnectionState::Disconnected
    }
}

impl ConnectionState {
    /// Check if we are currently connected.
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected { .. })
    }

    /// Check if we are attempting to connect.
    pub fn is_connecting(&self) -> bool {
        matches!(self, ConnectionState::Connecting { .. })
    }

    /// Get the user ID if connected.
    pub fn user_id(&self) -> Option<u64> {
        match self {
            ConnectionState::Connected { user_id, .. } => Some(*user_id),
            _ => None,
        }
    }
}

// =============================================================================
// Transmission Mode
// =============================================================================

/// Voice transmission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransmissionMode {
    /// Only transmit while PTT key is held.
    #[default]
    PushToTalk,
    /// Always transmitting when connected.
    Continuous,
    /// Not transmitting (muted).
    Muted,
    // Future: VoiceActivated { threshold: f32 }
}

// =============================================================================
// Audio State
// =============================================================================

/// Audio subsystem state.
#[derive(Debug, Clone, Default)]
pub struct AudioState {
    /// Available input (microphone) devices.
    pub input_devices: Vec<AudioDeviceInfo>,
    /// Available output (speaker/headphone) devices.
    pub output_devices: Vec<AudioDeviceInfo>,
    /// Currently selected input device ID (None = system default).
    pub selected_input: Option<String>,
    /// Currently selected output device ID (None = system default).
    pub selected_output: Option<String>,
    /// Current transmission mode.
    pub transmission_mode: TransmissionMode,
    /// Whether we are actually transmitting right now.
    pub is_transmitting: bool,
    /// User IDs of users currently transmitting voice.
    pub talking_users: HashSet<u64>,
}

impl AudioState {
    /// Create a new AudioState with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the selected input device info, if any.
    pub fn selected_input_device(&self) -> Option<&AudioDeviceInfo> {
        match &self.selected_input {
            Some(id) => self.input_devices.iter().find(|d| &d.id == id),
            None => self.input_devices.iter().find(|d| d.is_default),
        }
    }

    /// Get the selected output device info, if any.
    pub fn selected_output_device(&self) -> Option<&AudioDeviceInfo> {
        match &self.selected_output {
            Some(id) => self.output_devices.iter().find(|d| &d.id == id),
            None => self.output_devices.iter().find(|d| d.is_default),
        }
    }
}

// =============================================================================
// Chat Message
// =============================================================================

/// A chat message.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub sender: String,
    pub text: String,
    pub timestamp: std::time::Instant,
}

// =============================================================================
// Main State Struct
// =============================================================================

/// The complete client state exposed to the UI.
///
/// The UI renders based on this state. User actions result in Commands
/// sent to the backend, which updates this state and calls the repaint callback.
#[derive(Debug, Clone, Default)]
pub struct State {
    // Connection
    /// Current connection state.
    pub connection: ConnectionState,
    
    // Server state (when connected)
    /// List of rooms on the server.
    pub rooms: Vec<RoomInfo>,
    /// List of users on the server.
    pub users: Vec<User>,
    /// Our user ID (if connected).
    pub my_user_id: Option<u64>,
    /// Our current room ID (if in a room).
    pub my_room_id: Option<Uuid>,
    
    // Audio
    /// Audio subsystem state.
    pub audio: AudioState,
    
    // Chat (recent messages, not persisted)
    /// Recent chat messages.
    pub chat_messages: Vec<ChatMessage>,
}

impl State {
    /// Get users in a specific room.
    pub fn users_in_room(&self, room_uuid: Uuid) -> Vec<&User> {
        self.users
            .iter()
            .filter(|u| {
                u.current_room
                    .as_ref()
                    .and_then(api::uuid_from_room_id)
                    == Some(room_uuid)
            })
            .collect()
    }

    /// Check if a user is in a room.
    pub fn is_user_in_room(&self, user_id: u64, room_uuid: Uuid) -> bool {
        self.users.iter().any(|u| {
            u.user_id.as_ref().map(|id| id.value) == Some(user_id)
                && u.current_room
                    .as_ref()
                    .and_then(api::uuid_from_room_id)
                    == Some(room_uuid)
        })
    }

    /// Get room by UUID.
    pub fn get_room(&self, room_uuid: Uuid) -> Option<&RoomInfo> {
        self.rooms
            .iter()
            .find(|r| r.id.as_ref().and_then(api::uuid_from_room_id) == Some(room_uuid))
    }
    
    /// Get a user by ID.
    pub fn get_user(&self, user_id: u64) -> Option<&User> {
        self.users
            .iter()
            .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
    }
}

// =============================================================================
// Commands
// =============================================================================

/// Commands that can be sent from the UI to the backend.
///
/// Commands are fire-and-forget. The UI sends a command, and the backend
/// updates state asynchronously.
#[derive(Debug, Clone)]
pub enum Command {
    // Connection
    /// Connect to a server.
    Connect {
        addr: String,
        name: String,
        password: Option<String>,
    },
    /// Disconnect from the current server.
    Disconnect,

    // Room/Chat
    /// Join a room by UUID.
    JoinRoom { room_id: Uuid },
    /// Create a new room.
    CreateRoom { name: String },
    /// Delete a room by UUID.
    DeleteRoom { room_id: Uuid },
    /// Rename a room.
    RenameRoom { room_id: Uuid, new_name: String },
    /// Send a chat message.
    SendChat { text: String },

    // Audio configuration (always available)
    /// Set the input (microphone) device by ID.
    SetInputDevice { device_id: Option<String> },
    /// Set the output (speaker) device by ID.
    SetOutputDevice { device_id: Option<String> },
    /// Refresh the list of available audio devices.
    RefreshAudioDevices,

    // Transmission control
    /// Set the transmission mode.
    SetTransmissionMode { mode: TransmissionMode },
    /// Start transmitting (only effective in PushToTalk mode).
    StartTransmit,
    /// Stop transmitting.
    StopTransmit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::{proto::UserId, room_id_from_uuid};

    #[test]
    fn test_state_users_in_room() {
        let room1_uuid = ROOT_ROOM_UUID;
        let room2_uuid = Uuid::new_v4();

        let state = State {
            connection: ConnectionState::Connected {
                server_name: "Test".to_string(),
                user_id: 1,
            },
            my_user_id: Some(1),
            my_room_id: Some(room1_uuid),
            rooms: vec![
                RoomInfo {
                    id: Some(room_id_from_uuid(room1_uuid)),
                    name: "Root".to_string(),
                },
                RoomInfo {
                    id: Some(room_id_from_uuid(room2_uuid)),
                    name: "Room2".to_string(),
                },
            ],
            users: vec![
                User {
                    user_id: Some(UserId { value: 1 }),
                    current_room: Some(room_id_from_uuid(room1_uuid)),
                    username: "user1".to_string(),
                },
                User {
                    user_id: Some(UserId { value: 2 }),
                    current_room: Some(room_id_from_uuid(room1_uuid)),
                    username: "user2".to_string(),
                },
                User {
                    user_id: Some(UserId { value: 3 }),
                    current_room: Some(room_id_from_uuid(room2_uuid)),
                    username: "user3".to_string(),
                },
            ],
            audio: AudioState::default(),
            chat_messages: vec![],
        };

        let users_in_room1 = state.users_in_room(room1_uuid);
        assert_eq!(users_in_room1.len(), 2);

        let users_in_room2 = state.users_in_room(room2_uuid);
        assert_eq!(users_in_room2.len(), 1);
        assert_eq!(users_in_room2[0].username, "user3");
    }

    #[test]
    fn test_state_get_room() {
        let room_uuid = ROOT_ROOM_UUID;
        let state = State {
            rooms: vec![RoomInfo {
                id: Some(room_id_from_uuid(room_uuid)),
                name: "Root".to_string(),
            }],
            ..Default::default()
        };

        let room = state.get_room(room_uuid);
        assert!(room.is_some());
        assert_eq!(room.unwrap().name, "Root");

        assert!(state.get_room(Uuid::new_v4()).is_none());
    }
}
