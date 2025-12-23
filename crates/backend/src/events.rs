//! Backend events and commands for UI integration.
//!
//! This module provides a structured event-driven API for communication between
//! the UI layer and the backend. The UI sends commands and receives events without
//! needing to manage async tasks, channels, or the Client directly.

use api::proto::{RoomInfo, UserPresence};

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
        matches!(self, ConnectionStatus::Connecting | ConnectionStatus::Reconnecting { .. })
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
    SendChat {
        text: String,
    },
    /// Join a room by ID.
    JoinRoom {
        room_id: u64,
    },
    /// Create a new room.
    CreateRoom {
        name: String,
    },
    /// Delete a room by ID.
    DeleteRoom {
        room_id: u64,
    },
    /// Rename a room.
    RenameRoom {
        room_id: u64,
        new_name: String,
    },
    /// Send a voice frame (raw audio bytes for now, will be Opus later).
    SendVoice {
        audio_bytes: Vec<u8>,
    },
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
    ConnectFailed {
        error: String,
    },
    /// Graceful disconnection from the server.
    Disconnected {
        reason: Option<String>,
    },
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
    ConnectionStatusChanged {
        status: ConnectionStatus,
    },
    /// Room/user state has been updated.
    StateUpdated {
        state: ConnectionState,
    },
    /// Received a chat message.
    ChatReceived {
        sender: String,
        text: String,
    },
    /// Received voice audio from another user.
    VoiceReceived {
        sender_id: u64,
        audio_bytes: Vec<u8>,
    },
    /// An error occurred.
    Error {
        message: String,
    },
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
    /// Current room ID (if in a room).
    pub current_room_id: Option<u64>,
    /// List of rooms.
    pub rooms: Vec<RoomInfo>,
    /// List of user presences.
    pub users: Vec<UserPresence>,
}

impl ConnectionState {
    /// Get users in a specific room.
    pub fn users_in_room(&self, room_id: u64) -> Vec<&UserPresence> {
        self.users
            .iter()
            .filter(|u| u.room_id.as_ref().map(|r| r.value) == Some(room_id))
            .collect()
    }
    
    /// Check if a user is in a room.
    pub fn is_user_in_room(&self, user_id: u64, room_id: u64) -> bool {
        self.users.iter().any(|u| {
            u.user_id.as_ref().map(|id| id.value) == Some(user_id)
                && u.room_id.as_ref().map(|r| r.value) == Some(room_id)
        })
    }
    
    /// Get room by ID.
    pub fn get_room(&self, room_id: u64) -> Option<&RoomInfo> {
        self.rooms.iter().find(|r| r.id.as_ref().map(|id| id.value) == Some(room_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::proto::{RoomId, UserId};

    #[test]
    fn test_connection_state_users_in_room() {
        let state = ConnectionState {
            status: ConnectionStatus::Connected,
            connected: true,
            my_user_id: Some(1),
            my_client_name: "test".to_string(),
            current_room_id: Some(1),
            rooms: vec![
                RoomInfo { id: Some(RoomId { value: 1 }), name: "Root".to_string() },
                RoomInfo { id: Some(RoomId { value: 2 }), name: "Room2".to_string() },
            ],
            users: vec![
                UserPresence { user_id: Some(UserId { value: 1 }), room_id: Some(RoomId { value: 1 }), username: "user1".to_string() },
                UserPresence { user_id: Some(UserId { value: 2 }), room_id: Some(RoomId { value: 1 }), username: "user2".to_string() },
                UserPresence { user_id: Some(UserId { value: 3 }), room_id: Some(RoomId { value: 2 }), username: "user3".to_string() },
            ],
        };
        
        let users_in_room1 = state.users_in_room(1);
        assert_eq!(users_in_room1.len(), 2);
        
        let users_in_room2 = state.users_in_room(2);
        assert_eq!(users_in_room2.len(), 1);
        assert_eq!(users_in_room2[0].username, "user3");
    }
    
    #[test]
    fn test_connection_state_get_room() {
        let state = ConnectionState {
            rooms: vec![
                RoomInfo { id: Some(RoomId { value: 1 }), name: "Root".to_string() },
            ],
            ..Default::default()
        };
        
        let room = state.get_room(1);
        assert!(room.is_some());
        assert_eq!(room.unwrap().name, "Root");
        
        assert!(state.get_room(999).is_none());
    }
}
