//! Backend state and commands for UI integration.
//!
//! This module provides a state-driven API for communication between
//! the UI layer and the backend. The UI reads state and sends commands.
//! The backend updates state and calls the repaint callback.

use crate::audio::AudioDeviceInfo;
use api::proto::{RoomInfo, User};
use pipeline::{PipelineConfig, UserRxConfig};
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use uuid::Uuid;

/// Re-export ROOT_ROOM_UUID for convenience
pub use api::ROOT_ROOM_UUID;

// =============================================================================
// Audio Settings (Configurable)
// =============================================================================

/// Configurable audio pipeline settings.
///
/// These settings can be changed at runtime via the `UpdateAudioSettings` command.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSettings {
    /// Enable RNNoise denoising on microphone input.
    pub denoise_enabled: bool,
    
    /// Opus encoder bitrate in bits per second.
    /// Common values: 24000 (low), 32000 (medium), 64000 (high), 96000 (very high).
    /// Range: 6000 - 510000.
    pub bitrate: i32,
    
    /// Opus encoder complexity (0-10).
    /// Higher values = better quality but more CPU usage.
    /// Recommended: 5 for mobile, 10 for desktop.
    pub encoder_complexity: i32,
    
    /// Number of packets to buffer before starting playback.
    /// Higher values = more latency but smoother playback under jitter.
    /// At 20ms per frame: 2 packets = 40ms, 3 packets = 60ms, 5 packets = 100ms.
    pub jitter_buffer_delay_packets: u32,
    
    /// Enable Forward Error Correction for packet loss recovery.
    pub fec_enabled: bool,
    
    /// Expected packet loss percentage (0-100) for FEC tuning.
    /// Higher values add more redundancy at the cost of bitrate.
    pub packet_loss_percent: i32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            denoise_enabled: true,
            bitrate: 64000,
            encoder_complexity: 10,
            jitter_buffer_delay_packets: 3,
            fec_enabled: true,
            packet_loss_percent: 5,
        }
    }
}

impl AudioSettings {
    /// Bitrate presets for easy selection.
    pub const BITRATE_LOW: i32 = 24000;
    pub const BITRATE_MEDIUM: i32 = 32000;
    pub const BITRATE_HIGH: i32 = 64000;
    pub const BITRATE_VERY_HIGH: i32 = 96000;
    
    /// Get a human-readable description of the current bitrate.
    pub fn bitrate_label(&self) -> &'static str {
        match self.bitrate {
            b if b <= 24000 => "Low (24 kbps)",
            b if b <= 32000 => "Medium (32 kbps)",
            b if b <= 64000 => "High (64 kbps)",
            _ => "Very High (96+ kbps)",
        }
    }
}

// =============================================================================
// Audio Statistics (Observable)
// =============================================================================

/// Runtime audio statistics for monitoring and debugging.
///
/// These are read-only values updated by the audio pipeline.
#[derive(Debug, Clone, Default)]
pub struct AudioStats {
    /// Number of voice packets sent.
    pub packets_sent: u64,
    
    /// Number of voice packets received.
    pub packets_received: u64,
    
    /// Number of packets lost (detected via sequence gaps).
    pub packets_lost: u64,
    
    /// Number of packets recovered via FEC.
    pub packets_recovered_fec: u64,
    
    /// Number of frames concealed via PLC (packet loss concealment).
    pub frames_concealed: u64,
    
    /// Total bytes of Opus data sent.
    pub bytes_sent: u64,
    
    /// Total bytes of Opus data received.
    pub bytes_received: u64,
    
    /// Average encoded frame size in bytes (rolling average).
    pub avg_frame_size_bytes: f32,
    
    /// Estimated actual bitrate in bits per second (rolling average).
    pub actual_bitrate_bps: f32,
    
    /// Current playback buffer level in packets (for jitter buffer monitoring).
    pub playback_buffer_packets: u32,
    
    /// Number of buffer underruns (playback starvation events).
    pub buffer_underruns: u64,
    
    /// Timestamp of last stats update.
    pub last_update: Option<Instant>,
}

impl AudioStats {
    /// Calculate packet loss percentage.
    pub fn packet_loss_percent(&self) -> f32 {
        let total = self.packets_received + self.packets_lost;
        if total == 0 {
            0.0
        } else {
            (self.packets_lost as f32 / total as f32) * 100.0
        }
    }
    
    /// Calculate FEC recovery percentage (of lost packets).
    pub fn fec_recovery_percent(&self) -> f32 {
        if self.packets_lost == 0 {
            0.0
        } else {
            (self.packets_recovered_fec as f32 / self.packets_lost as f32) * 100.0
        }
    }
}

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
// Voice Mode (how voice is activated)
// =============================================================================

/// Voice activation mode (how transmission is triggered).
/// 
/// Note: This is separate from mute state. A user can be in Continuous mode
/// but still muted - mute is an orthogonal toggle.
/// 
/// Note: Voice Activity Detection (VAD) is not a voice mode but a pipeline
/// processor. To achieve "voice activated" transmission, use Continuous mode
/// with the VAD processor enabled in the TX pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceMode {
    /// Only transmit while PTT key is held.
    #[default]
    PushToTalk,
    /// Always transmitting when connected (unless muted or suppressed by pipeline).
    /// When VAD processor is enabled, this provides voice-activated behavior.
    Continuous,
}

impl VoiceMode {
    /// Check if this mode requires the audio pipeline to run continuously.
    /// 
    /// In PTT mode, the pipeline only runs when PTT is pressed.
    /// In Continuous mode, it runs continuously.
    pub fn requires_continuous_capture(&self) -> bool {
        matches!(self, VoiceMode::Continuous)
    }
}

/// For backwards compatibility during migration
pub type TransmissionMode = VoiceMode;

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
    /// Current voice activation mode (PTT or Continuous).
    pub voice_mode: VoiceMode,
    /// Whether self is muted (not transmitting).
    pub self_muted: bool,
    /// Whether self is deafened (not receiving audio).
    pub self_deafened: bool,
    /// User IDs that are locally muted (we don't hear them).
    pub muted_users: HashSet<u64>,
    /// Whether we are actually transmitting right now.
    pub is_transmitting: bool,
    /// User IDs of users currently transmitting voice.
    pub talking_users: HashSet<u64>,
    /// Configurable audio pipeline settings (legacy, for Opus encoder config).
    pub settings: AudioSettings,
    /// Runtime audio statistics.
    pub stats: AudioStats,
    
    // Pipeline configuration
    /// TX (transmit) pipeline configuration.
    pub tx_pipeline: PipelineConfig,
    /// Default RX (receive) pipeline configuration for all users.
    pub rx_pipeline_defaults: PipelineConfig,
    /// Per-user RX configuration overrides.
    pub per_user_rx: HashMap<u64, UserRxConfig>,
    
    /// Current audio input level in dB (from TX pipeline, for UI metering).
    pub input_level_db: Option<f32>,
}

impl AudioState {
    /// Create a new AudioState with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a specific user is locally muted.
    pub fn is_user_muted(&self, user_id: u64) -> bool {
        self.muted_users.contains(&user_id)
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
    /// Set the voice activation mode (PTT vs Continuous).
    SetVoiceMode { mode: VoiceMode },
    /// Set self-muted state (stops transmission).
    SetMuted { muted: bool },
    /// Set self-deafened state (stops receiving audio; implies muted).
    SetDeafened { deafened: bool },
    /// Mute a specific user locally (we won't hear them).
    MuteUser { user_id: u64 },
    /// Unmute a specific user locally.
    UnmuteUser { user_id: u64 },
    /// Start transmitting (only effective in PushToTalk mode when not muted).
    StartTransmit,
    /// Stop transmitting.
    StopTransmit,
    
    // Audio settings
    /// Update audio pipeline settings (denoise, bitrate, complexity, etc.).
    UpdateAudioSettings { settings: AudioSettings },
    /// Reset audio statistics.
    ResetAudioStats,
    
    // Pipeline configuration
    /// Update the TX (transmit) pipeline configuration.
    UpdateTxPipeline { config: PipelineConfig },
    /// Update the default RX (receive) pipeline configuration for all users.
    UpdateRxPipelineDefaults { config: PipelineConfig },
    /// Update configuration for a specific user's RX pipeline.
    UpdateUserRxConfig { user_id: u64, config: UserRxConfig },
    /// Remove per-user RX override, reverting to defaults.
    ClearUserRxOverride { user_id: u64 },
    /// Set per-user volume (convenience command, updates UserRxConfig).
    SetUserVolume { user_id: u64, volume_db: f32 },
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
                    is_muted: false,
                    is_deafened: false,
                },
                User {
                    user_id: Some(UserId { value: 2 }),
                    current_room: Some(room_id_from_uuid(room1_uuid)),
                    username: "user2".to_string(),
                    is_muted: false,
                    is_deafened: false,
                },
                User {
                    user_id: Some(UserId { value: 3 }),
                    current_room: Some(room_id_from_uuid(room2_uuid)),
                    username: "user3".to_string(),
                    is_muted: false,
                    is_deafened: false,
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
