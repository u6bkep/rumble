//! Backend events and commands for UI integration.
//!
//! This module provides a structured event-driven API for communication between
//! the UI layer and the backend. The UI sends commands and receives events without
//! needing to manage async tasks, channels, or the Client directly.

use crate::audio::AudioDeviceInfo;
use api::{
    proto::{RoomInfo, UserPresence},
    uuid_from_room_id,
};
use uuid::Uuid;

/// Re-export ROOT_ROOM_UUID for convenience
pub use api::ROOT_ROOM_UUID;

/// Connection lifecycle status.
///
/// This enum represents the various states the connection can be in,
/// allowing the UI to provide appropriate feedback to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    /// Not connected to any server.
    #[default]
    Disconnected,
    /// Attempting to establish a connection.
    Connecting,
    /// Successfully connected and authenticated.
    Connected,
    /// Connection was lost, attempting to reconnect.
    Reconnecting {
        /// Current reconnection attempt number (1-indexed).
        attempt: u32,
        /// Maximum number of attempts before giving up (0 = unlimited).
        max_attempts: u32,
    },
    /// Connection was lost and reconnection failed or was not attempted.
    ConnectionLost,
}

impl ConnectionStatus {
    /// Check if we are currently connected.
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionStatus::Connected)
    }

    /// Check if we are attempting to connect or reconnect.
    pub fn is_connecting(&self) -> bool {
        matches!(
            self,
            ConnectionStatus::Connecting | ConnectionStatus::Reconnecting { .. }
        )
    }

    /// Check if the connection has failed.
    pub fn is_failed(&self) -> bool {
        matches!(self, ConnectionStatus::ConnectionLost)
    }
}

/// Commands that can be sent from the UI to the backend.
#[derive(Debug, Clone)]
pub enum BackendCommand {
    /// Connect to a server.
    Connect {
        addr: String,
        name: String,
        password: Option<String>,
        config: crate::ConnectConfig,
    },
    /// Disconnect from the current server.
    Disconnect,
    /// Send a chat message.
    SendChat { text: String },
    /// Join a room by UUID.
    JoinRoom { room_uuid: Uuid },
    /// Create a new room.
    CreateRoom { name: String },
    /// Delete a room by UUID.
    DeleteRoom { room_uuid: Uuid },
    /// Rename a room.
    RenameRoom { room_uuid: Uuid, new_name: String },

    // Audio control commands - audio is handled entirely by the backend
    /// Start capturing audio from the microphone and transmitting to the server.
    /// This is typically triggered by push-to-talk key press.
    StartVoiceCapture,
    /// Stop capturing audio.
    /// This is typically triggered by push-to-talk key release.
    StopVoiceCapture,
    /// Start audio playback (receiving and playing voice from other users).
    StartPlayback,
    /// Stop audio playback.
    StopPlayback,
    /// Set the input (microphone) device by ID.
    /// Use `None` for the system default device.
    SetInputDevice { device_id: Option<String> },
    /// Set the output (speaker) device by ID.
    /// Use `None` for the system default device.
    SetOutputDevice { device_id: Option<String> },
    /// Set the input volume multiplier (0.0 = muted, 1.0 = normal, 2.0 = boosted).
    SetInputVolume { volume: f32 },
    /// Set the output volume multiplier (0.0 = muted, 1.0 = normal, 2.0 = boosted).
    SetOutputVolume { volume: f32 },
    /// Refresh the list of available audio devices.
    RefreshAudioDevices,
}

/// Events that the backend sends to the UI.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    /// Successfully connected to the server.
    Connected {
        /// Our user ID assigned by the server.
        user_id: u64,
        /// Our client name.
        client_name: String,
    },
    /// Connection attempt failed.
    ConnectFailed { error: String },
    /// Graceful disconnection from the server.
    Disconnected { reason: Option<String> },
    /// Connection was unexpectedly lost.
    ///
    /// This is different from `Disconnected` which indicates a graceful shutdown.
    /// The UI may want to show a different message or trigger reconnection.
    ConnectionLost {
        /// Error message describing why the connection was lost.
        error: String,
        /// Whether the backend will attempt to reconnect.
        will_reconnect: bool,
    },
    /// Connection status has changed.
    ///
    /// Emitted when entering Connecting, Reconnecting, etc. states.
    /// This allows the UI to show intermediate connection states.
    ConnectionStatusChanged { status: ConnectionStatus },
    /// Room/user state has been updated.
    StateUpdated { state: ConnectionState },
    /// Received a chat message.
    ChatReceived { sender: String, text: String },
    /// Voice activity detected for a user.
    ///
    /// This event notifies the UI when a user starts or stops talking.
    /// The actual audio playback is handled internally by the backend.
    VoiceActivity {
        /// User ID of the speaker.
        user_id: u64,
        /// Whether the user is currently talking.
        is_talking: bool,
    },
    /// Audio state has changed.
    ///
    /// Sent when audio devices, settings, or capture/playback state changes.
    /// This contains the complete audio state so the UI can update accordingly.
    AudioStateChanged {
        /// Current audio state.
        state: AudioState,
    },
    /// An error occurred.
    Error { message: String },
}

/// Audio subsystem state.
///
/// This contains all audio-related state that the UI might need to display.
/// The backend owns this state and emits `AudioStateChanged` events when it changes.
#[derive(Debug, Clone, Default)]
pub struct AudioState {
    /// Available input (microphone) devices.
    pub input_devices: Vec<AudioDeviceInfo>,
    /// Available output (speaker/headphone) devices.
    pub output_devices: Vec<AudioDeviceInfo>,
    /// Currently selected input device ID (None = system default).
    pub selected_input_id: Option<String>,
    /// Currently selected output device ID (None = system default).
    pub selected_output_id: Option<String>,
    /// Input volume multiplier (0.0 = muted, 1.0 = normal, 2.0 = boosted).
    pub input_volume: f32,
    /// Output volume multiplier (0.0 = muted, 1.0 = normal, 2.0 = boosted).
    pub output_volume: f32,
    /// Whether voice capture is currently active.
    pub is_capturing: bool,
    /// Whether audio playback is currently active.
    pub is_playing: bool,
}

impl AudioState {
    /// Create a new AudioState with default values.
    pub fn new() -> Self {
        Self {
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            selected_input_id: None,
            selected_output_id: None,
            input_volume: 1.0,
            output_volume: 1.0,
            is_capturing: false,
            is_playing: false,
        }
    }

    /// Get the selected input device info, if any.
    pub fn selected_input_device(&self) -> Option<&AudioDeviceInfo> {
        match &self.selected_input_id {
            Some(id) => self.input_devices.iter().find(|d| &d.id == id),
            None => self.input_devices.iter().find(|d| d.is_default),
        }
    }

    /// Get the selected output device info, if any.
    pub fn selected_output_device(&self) -> Option<&AudioDeviceInfo> {
        match &self.selected_output_id {
            Some(id) => self.output_devices.iter().find(|d| &d.id == id),
            None => self.output_devices.iter().find(|d| d.is_default),
        }
    }
}

/// Current connection state as seen by the backend.
#[derive(Debug, Clone, Default)]
pub struct ConnectionState {
    /// Current connection status.
    pub status: ConnectionStatus,
    /// Whether we are connected to a server (convenience, same as status.is_connected()).
    pub connected: bool,
    /// Our user ID (if connected and assigned).
    pub my_user_id: Option<u64>,
    /// Our client name.
    pub my_client_name: String,
    /// Current room UUID (if in a room).
    pub current_room_id: Option<Uuid>,
    /// List of rooms.
    pub rooms: Vec<RoomInfo>,
    /// List of user presences.
    pub users: Vec<UserPresence>,
    /// Audio subsystem state.
    pub audio: AudioState,
}

impl ConnectionState {
    /// Get users in a specific room.
    pub fn users_in_room(&self, room_uuid: Uuid) -> Vec<&UserPresence> {
        self.users
            .iter()
            .filter(|u| u.room_id.as_ref().and_then(uuid_from_room_id) == Some(room_uuid))
            .collect()
    }

    /// Check if a user is in a room.
    pub fn is_user_in_room(&self, user_id: u64, room_uuid: Uuid) -> bool {
        self.users.iter().any(|u| {
            u.user_id.as_ref().map(|id| id.value) == Some(user_id)
                && u.room_id.as_ref().and_then(uuid_from_room_id) == Some(room_uuid)
        })
    }

    /// Get room by UUID.
    pub fn get_room(&self, room_uuid: Uuid) -> Option<&RoomInfo> {
        self.rooms
            .iter()
            .find(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(room_uuid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::{proto::UserId, room_id_from_uuid};
    use uuid::Uuid;

    #[test]
    fn test_connection_state_users_in_room() {
        let room1_uuid = ROOT_ROOM_UUID;
        let room2_uuid = Uuid::new_v4();

        let state = ConnectionState {
            status: ConnectionStatus::Connected,
            connected: true,
            my_user_id: Some(1),
            my_client_name: "test".to_string(),
            current_room_id: Some(room1_uuid),
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
                UserPresence {
                    user_id: Some(UserId { value: 1 }),
                    room_id: Some(room_id_from_uuid(room1_uuid)),
                    username: "user1".to_string(),
                },
                UserPresence {
                    user_id: Some(UserId { value: 2 }),
                    room_id: Some(room_id_from_uuid(room1_uuid)),
                    username: "user2".to_string(),
                },
                UserPresence {
                    user_id: Some(UserId { value: 3 }),
                    room_id: Some(room_id_from_uuid(room2_uuid)),
                    username: "user3".to_string(),
                },
            ],
        };

        let users_in_room1 = state.users_in_room(room1_uuid);
        assert_eq!(users_in_room1.len(), 2);

        let users_in_room2 = state.users_in_room(room2_uuid);
        assert_eq!(users_in_room2.len(), 1);
        assert_eq!(users_in_room2[0].username, "user3");
    }

    #[test]
    fn test_connection_state_get_room() {
        let room_uuid = ROOT_ROOM_UUID;
        let state = ConnectionState {
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
