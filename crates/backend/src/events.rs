//! Backend state and commands for UI integration.
//!
//! This module provides a state-driven API for communication between
//! the UI layer and the backend. The UI reads state and sends commands.
//! The backend updates state and calls the repaint callback.

use crate::audio::AudioDeviceInfo;
use api::proto::{RoomInfo, User};
use pipeline::{PipelineConfig, UserRxConfig};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};
use uuid::Uuid;

// =============================================================================
// Room Tree
// =============================================================================

/// A node in the room tree hierarchy.
#[derive(Debug, Clone)]
pub struct RoomTreeNode {
    /// The room UUID.
    pub id: Uuid,
    /// The room name.
    pub name: String,
    /// The parent room UUID (None for root rooms).
    pub parent_id: Option<Uuid>,
    /// UUIDs of child rooms, in sorted order by name.
    pub children: Vec<Uuid>,
}

/// Hierarchical tree structure of rooms.
///
/// This is rebuilt whenever the room list changes. The UI can use this
/// to efficiently render the room list as a tree without rebuilding
/// the hierarchy on each frame.
#[derive(Debug, Clone, Default)]
pub struct RoomTree {
    /// All nodes indexed by room UUID.
    pub nodes: HashMap<Uuid, RoomTreeNode>,
    /// Root room UUIDs (rooms with no parent), sorted by name.
    pub roots: Vec<Uuid>,
}

impl RoomTree {
    /// Create a new empty room tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the tree from a list of rooms.
    pub fn rebuild(&mut self, rooms: &[RoomInfo]) {
        self.nodes.clear();
        self.roots.clear();

        // First pass: create all nodes
        for room in rooms {
            let Some(room_uuid) = room.id.as_ref().and_then(api::uuid_from_room_id) else {
                continue;
            };
            let parent_uuid = room.parent_id.as_ref().and_then(api::uuid_from_room_id);

            self.nodes.insert(
                room_uuid,
                RoomTreeNode {
                    id: room_uuid,
                    name: room.name.clone(),
                    parent_id: parent_uuid,
                    children: Vec::new(),
                },
            );
        }

        // Second pass: build parent-child relationships and collect roots
        let uuids: Vec<Uuid> = self.nodes.keys().copied().collect();
        for uuid in uuids {
            let parent_id = self.nodes.get(&uuid).and_then(|n| n.parent_id);

            if let Some(parent_uuid) = parent_id {
                // Add as child of parent (if parent exists)
                if self.nodes.contains_key(&parent_uuid) {
                    if let Some(parent) = self.nodes.get_mut(&parent_uuid) {
                        parent.children.push(uuid);
                    }
                } else {
                    // Parent doesn't exist, treat as root
                    self.roots.push(uuid);
                }
            } else {
                // No parent, this is a root
                self.roots.push(uuid);
            }
        }

        // Sort roots by name
        self.roots.sort_by(|a, b| {
            let name_a = self.nodes.get(a).map(|n| n.name.as_str()).unwrap_or("");
            let name_b = self.nodes.get(b).map(|n| n.name.as_str()).unwrap_or("");
            name_a.cmp(name_b)
        });

        // Sort children of each node by name
        // First, collect the name for each UUID for sorting
        let names: HashMap<Uuid, String> = self.nodes.iter().map(|(id, node)| (*id, node.name.clone())).collect();

        for node in self.nodes.values_mut() {
            node.children.sort_by(|a, b| {
                let name_a = names.get(a).map(|s| s.as_str()).unwrap_or("");
                let name_b = names.get(b).map(|s| s.as_str()).unwrap_or("");
                name_a.cmp(name_b)
            });
        }
    }

    /// Get a node by UUID.
    pub fn get(&self, uuid: Uuid) -> Option<&RoomTreeNode> {
        self.nodes.get(&uuid)
    }

    /// Get mutable reference to a node by UUID.
    pub fn get_mut(&mut self, uuid: Uuid) -> Option<&mut RoomTreeNode> {
        self.nodes.get_mut(&uuid)
    }

    /// Check if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Get the number of rooms in the tree.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Get children of a room.
    pub fn children(&self, uuid: Uuid) -> &[Uuid] {
        self.nodes.get(&uuid).map(|n| n.children.as_slice()).unwrap_or(&[])
    }

    /// Check if a room has any children.
    pub fn has_children(&self, uuid: Uuid) -> bool {
        self.nodes.get(&uuid).map(|n| !n.children.is_empty()).unwrap_or(false)
    }

    /// Iterate over all ancestors of a room, from parent to root.
    pub fn ancestors(&self, uuid: Uuid) -> impl Iterator<Item = Uuid> + '_ {
        AncestorIterator {
            tree: self,
            current: self.nodes.get(&uuid).and_then(|n| n.parent_id),
        }
    }

    /// Check if `ancestor` is an ancestor of `descendant`.
    pub fn is_ancestor(&self, ancestor: Uuid, descendant: Uuid) -> bool {
        self.ancestors(descendant).any(|id| id == ancestor)
    }

    /// Get the depth of a room (0 for root rooms).
    pub fn depth(&self, uuid: Uuid) -> usize {
        self.ancestors(uuid).count()
    }
}

/// Iterator over ancestors of a room.
struct AncestorIterator<'a> {
    tree: &'a RoomTree,
    current: Option<Uuid>,
}

impl<'a> Iterator for AncestorIterator<'a> {
    type Item = Uuid;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.current?;
        self.current = self.tree.nodes.get(&current).and_then(|n| n.parent_id);
        Some(current)
    }
}

/// Re-export ROOT_ROOM_UUID for convenience
pub use api::ROOT_ROOM_UUID;

// =============================================================================
// Authentication Types
// =============================================================================

/// Callback for signing authentication challenges.
///
/// The UI provides this callback. It may:
/// - Sign using a local Ed25519 private key
/// - Sign using the SSH agent
///
/// Takes the payload to sign, returns the 64-byte signature or an error.
pub type SigningCallback = Arc<dyn Fn(&[u8]) -> Result<[u8; 64], String> + Send + Sync>;

// =============================================================================
// Audio Settings (Configurable)
// =============================================================================

/// Configurable audio pipeline settings.
///
/// These settings can be changed at runtime via the `UpdateAudioSettings` command.
///
/// Note: Audio processing (denoise, VAD, etc.) is configured via the TX pipeline,
/// not via these settings.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSettings {
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

/// Information about a pending certificate for user confirmation.
#[derive(Clone)]
pub struct PendingCertificate {
    /// The DER-encoded certificate data.
    pub certificate_der: Vec<u8>,
    /// SHA256 fingerprint of the certificate.
    pub fingerprint: [u8; 32],
    /// The server name/address.
    pub server_name: String,
    /// The server address we were trying to connect to.
    pub server_addr: String,
    /// Username for retry.
    pub username: String,
    /// Password for retry (if any).
    pub password: Option<String>,
    /// Public key for retry.
    pub public_key: [u8; 32],
    /// Signer callback for retry.
    pub signer: SigningCallback,
}

// Implement Debug manually since SigningCallback doesn't implement Debug
impl std::fmt::Debug for PendingCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingCertificate")
            .field("fingerprint", &self.fingerprint_short())
            .field("server_name", &self.server_name)
            .field("server_addr", &self.server_addr)
            .field("username", &self.username)
            .field("password", &self.password.is_some())
            .field("public_key", &format!("{:02x?}...", &self.public_key[..4]))
            .finish()
    }
}

impl PendingCertificate {
    /// Get a hex-encoded fingerprint string for display.
    pub fn fingerprint_hex(&self) -> String {
        self.fingerprint
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .chunks(2)
            .map(|c| c.join(""))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Get a shortened fingerprint for compact display.
    pub fn fingerprint_short(&self) -> String {
        self.fingerprint
            .iter()
            .take(8)
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(":")
    }
}

// Implement PartialEq manually since SigningCallback doesn't implement it
impl PartialEq for PendingCertificate {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && self.server_name == other.server_name
            && self.server_addr == other.server_addr
    }
}

impl Eq for PendingCertificate {}

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
    /// Server presented a self-signed/untrusted certificate that needs user confirmation.
    CertificatePending {
        /// Information about the certificate awaiting confirmation.
        cert_info: PendingCertificate,
    },
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
    /// Unique message ID (16-byte UUID).
    pub id: [u8; 16],
    pub sender: String,
    pub text: String,
    /// Wall-clock time when the message was received/created.
    pub timestamp: std::time::SystemTime,
    /// True if this is a local status message (not from the server).
    pub is_local: bool,
}

// =============================================================================
// File Message (for file sharing in chat)
// =============================================================================

/// A file share message embedded in chat.
///
/// File messages are sent as JSON-encoded text in chat messages.
/// The client detects file messages by parsing the text as JSON
/// and validating against the file message schema.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileMessage {
    /// Message type marker (always "file").
    #[serde(rename = "type")]
    pub msg_type: String,
    /// File information.
    pub file: FileInfo,
}

/// File information for a shared file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileInfo {
    /// Original filename.
    pub name: String,
    /// File size in bytes.
    pub size: u64,
    /// MIME type (e.g., "image/jpeg").
    pub mime: String,
    /// 40-character hex-encoded SHA-1 infohash.
    pub infohash: String,
}

impl FileMessage {
    /// Schema URL for file messages.
    pub const SCHEMA: &'static str = "https://rumble.example/schemas/file-message-v1.json";

    /// Create a new file message.
    pub fn new(name: String, size: u64, mime: String, infohash: String) -> Self {
        Self {
            msg_type: "file".to_string(),
            file: FileInfo {
                name,
                size,
                mime,
                infohash,
            },
        }
    }

    /// Try to parse a chat message text as a file message.
    ///
    /// Returns Some(FileMessage) if the text is valid JSON that matches
    /// the file message schema with no extraneous fields.
    pub fn parse(text: &str) -> Option<Self> {
        // Attempt JSON parse
        let value: serde_json::Value = serde_json::from_str(text).ok()?;

        // Check it's an object
        let obj = value.as_object()?;

        // Validate against the schema:
        // - Must have "type": "file"
        // - Must have "file" object with exactly: name, size, mime, infohash
        // - May optionally have "$schema" field
        // - No other fields allowed (prevents false positives on user-pasted JSON)

        // Check type field
        if obj.get("type")?.as_str()? != "file" {
            return None;
        }

        // Check file object exists
        let file_obj = obj.get("file")?.as_object()?;

        // Validate file object has exactly the required fields
        let required_fields = ["name", "size", "mime", "infohash"];
        for field in &required_fields {
            if !file_obj.contains_key(*field) {
                return None;
            }
        }

        // Check for extraneous fields in file object
        for key in file_obj.keys() {
            if !required_fields.contains(&key.as_str()) {
                return None;
            }
        }

        // Check for extraneous fields in root object
        for key in obj.keys() {
            if !["type", "file", "$schema"].contains(&key.as_str()) {
                return None;
            }
        }

        // Now we can safely deserialize
        serde_json::from_value(value).ok()
    }

    /// Generate the magnet link for this file.
    pub fn magnet_link(&self) -> String {
        format!("magnet:?xt=urn:btih:{}", self.file.infohash)
    }

    /// Serialize to JSON for sending as a chat message.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// =============================================================================
// File Transfer State
// =============================================================================

/// State of a file transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransferState {
    /// In queue, not started.
    #[default]
    Pending,
    /// Verifying existing data.
    Checking,
    /// Actively downloading.
    Downloading,
    /// Actively seeding (upload only).
    Seeding,
    /// Paused by user.
    Paused,
    /// Download completed, not seeding.
    Completed,
    /// Transfer failed with error.
    Error,
}

impl TransferState {
    /// Check if this state represents an active transfer.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            TransferState::Checking | TransferState::Downloading | TransferState::Seeding
        )
    }

    /// Check if the transfer is complete (downloaded or seeding).
    pub fn is_finished(&self) -> bool {
        matches!(self, TransferState::Seeding | TransferState::Completed)
    }
}

/// Information about a file transfer.
#[derive(Debug, Clone)]
pub struct FileTransferState {
    /// Infohash (20 bytes).
    pub infohash: [u8; 20],
    /// Original filename.
    pub name: String,
    /// File size in bytes.
    pub size: u64,
    /// MIME type (if known).
    pub mime: Option<String>,
    /// Progress (0.0 to 1.0).
    pub progress: f32,
    /// Download speed in bytes/sec.
    pub download_speed: u64,
    /// Upload speed in bytes/sec.
    pub upload_speed: u64,
    /// Number of connected peers.
    pub peers: u32,
    /// User IDs of known seeders (from tracker).
    pub seeders: Vec<u64>,
    /// Current state.
    pub state: TransferState,
    /// Error message (if state is Error).
    pub error: Option<String>,
    /// Magnet link for sharing.
    pub magnet: Option<String>,
    /// Local file path (if available).
    pub local_path: Option<std::path::PathBuf>,
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

    // File Transfers
    pub file_transfers: Vec<FileTransferState>,

    // Room tree (derived from rooms)
    /// Hierarchical tree structure of rooms, rebuilt when rooms change.
    pub room_tree: RoomTree,
}

impl State {
    /// Rebuild the room tree from the current rooms list.
    /// Call this after modifying `self.rooms`.
    pub fn rebuild_room_tree(&mut self) {
        self.room_tree.rebuild(&self.rooms);
    }

    /// Get users in a specific room.
    pub fn users_in_room(&self, room_uuid: Uuid) -> Vec<&User> {
        self.users
            .iter()
            .filter(|u| u.current_room.as_ref().and_then(api::uuid_from_room_id) == Some(room_uuid))
            .collect()
    }

    /// Check if a user is in a room.
    pub fn is_user_in_room(&self, user_id: u64, room_uuid: Uuid) -> bool {
        self.users.iter().any(|u| {
            u.user_id.as_ref().map(|id| id.value) == Some(user_id)
                && u.current_room.as_ref().and_then(api::uuid_from_room_id) == Some(room_uuid)
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
#[derive(Clone)]
pub enum Command {
    // Connection
    /// Connect to a server with Ed25519 authentication.
    Connect {
        addr: String,
        name: String,
        public_key: [u8; 32],     // Ed25519 public key
        signer: SigningCallback,  // Signs auth challenge
        password: Option<String>, // Server password (for unknown keys)
    },
    /// Disconnect from the current server.
    Disconnect,
    /// Accept the pending self-signed certificate and retry connection.
    /// The certificate will be added to the trusted store.
    AcceptCertificate,
    /// Reject the pending certificate and cancel the connection attempt.
    RejectCertificate,

    // Room/Chat
    /// Join a room by UUID.
    JoinRoom {
        room_id: Uuid,
    },
    /// Create a new room, optionally under a parent room.
    CreateRoom {
        name: String,
        parent_id: Option<Uuid>,
    },
    /// Delete a room by UUID.
    DeleteRoom {
        room_id: Uuid,
    },
    /// Rename a room.
    RenameRoom {
        room_id: Uuid,
        new_name: String,
    },
    /// Move a room to a new parent.
    MoveRoom {
        room_id: Uuid,
        new_parent_id: Uuid,
    },
    /// Send a chat message.
    SendChat {
        text: String,
    },
    /// Add a local status message (not sent to server).
    LocalMessage {
        text: String,
    },

    // Audio configuration (always available)
    /// Set the input (microphone) device by ID.
    SetInputDevice {
        device_id: Option<String>,
    },
    /// Set the output (speaker) device by ID.
    SetOutputDevice {
        device_id: Option<String>,
    },
    /// Refresh the list of available audio devices.
    RefreshAudioDevices,

    // Transmission control
    /// Set the voice activation mode (PTT vs Continuous).
    SetVoiceMode {
        mode: VoiceMode,
    },
    /// Set self-muted state (stops transmission).
    SetMuted {
        muted: bool,
    },
    /// Set self-deafened state (stops receiving audio; implies muted).
    SetDeafened {
        deafened: bool,
    },
    /// Mute a specific user locally (we won't hear them).
    MuteUser {
        user_id: u64,
    },
    /// Unmute a specific user locally.
    UnmuteUser {
        user_id: u64,
    },
    /// Start transmitting (only effective in PushToTalk mode when not muted).
    StartTransmit,
    /// Stop transmitting.
    StopTransmit,

    // Audio settings
    /// Update audio pipeline settings (denoise, bitrate, complexity, etc.).
    UpdateAudioSettings {
        settings: AudioSettings,
    },
    /// Reset audio statistics.
    ResetAudioStats,

    // Pipeline configuration
    /// Update the TX (transmit) pipeline configuration.
    UpdateTxPipeline {
        config: PipelineConfig,
    },
    /// Update the default RX (receive) pipeline configuration for all users.
    UpdateRxPipelineDefaults {
        config: PipelineConfig,
    },
    /// Update configuration for a specific user's RX pipeline.
    UpdateUserRxConfig {
        user_id: u64,
        config: UserRxConfig,
    },
    /// Remove per-user RX override, reverting to defaults.
    ClearUserRxOverride {
        user_id: u64,
    },
    /// Set per-user volume (convenience command, updates UserRxConfig).
    SetUserVolume {
        user_id: u64,
        volume_db: f32,
    },

    // Registration
    /// Register a user (binds their username to their public key).
    RegisterUser {
        user_id: u64,
    },
    /// Unregister a user.
    UnregisterUser {
        user_id: u64,
    },

    // File Sharing
    ShareFile {
        path: std::path::PathBuf,
    },
    DownloadFile {
        magnet: String,
    },
    /// Pause a file transfer by infohash (hex-encoded).
    PauseTransfer {
        infohash: String,
    },
    /// Resume a paused file transfer by infohash (hex-encoded).
    ResumeTransfer {
        infohash: String,
    },
    /// Cancel and remove a file transfer by infohash (hex-encoded).
    CancelTransfer {
        infohash: String,
    },
    /// Remove a completed/seeding transfer and optionally delete the local file.
    RemoveTransfer {
        infohash: String,
        delete_file: bool,
    },
    /// Save a completed file to a new location.
    SaveFileAs {
        infohash: String,
        destination: std::path::PathBuf,
    },
    /// Open a completed file with the system default application.
    OpenFile {
        infohash: String,
    },
}

// Implement Debug manually since SigningCallback doesn't implement Debug
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::Connect {
                addr,
                name,
                public_key,
                password,
                ..
            } => f
                .debug_struct("Connect")
                .field("addr", addr)
                .field("name", name)
                .field("public_key", &format!("{:02x?}...", &public_key[..4]))
                .field("password", &password.is_some())
                .finish(),
            Command::Disconnect => write!(f, "Disconnect"),
            Command::AcceptCertificate => write!(f, "AcceptCertificate"),
            Command::RejectCertificate => write!(f, "RejectCertificate"),
            Command::JoinRoom { room_id } => f.debug_struct("JoinRoom").field("room_id", room_id).finish(),
            Command::CreateRoom { name, parent_id } => f
                .debug_struct("CreateRoom")
                .field("name", name)
                .field("parent_id", parent_id)
                .finish(),
            Command::DeleteRoom { room_id } => f.debug_struct("DeleteRoom").field("room_id", room_id).finish(),
            Command::RenameRoom { room_id, new_name } => f
                .debug_struct("RenameRoom")
                .field("room_id", room_id)
                .field("new_name", new_name)
                .finish(),
            Command::MoveRoom { room_id, new_parent_id } => f
                .debug_struct("MoveRoom")
                .field("room_id", room_id)
                .field("new_parent_id", new_parent_id)
                .finish(),
            Command::SendChat { text } => f.debug_struct("SendChat").field("text", text).finish(),
            Command::LocalMessage { text } => f.debug_struct("LocalMessage").field("text", text).finish(),
            Command::SetInputDevice { device_id } => {
                f.debug_struct("SetInputDevice").field("device_id", device_id).finish()
            }
            Command::SetOutputDevice { device_id } => {
                f.debug_struct("SetOutputDevice").field("device_id", device_id).finish()
            }
            Command::RefreshAudioDevices => write!(f, "RefreshAudioDevices"),
            Command::SetVoiceMode { mode } => f.debug_struct("SetVoiceMode").field("mode", mode).finish(),
            Command::SetMuted { muted } => f.debug_struct("SetMuted").field("muted", muted).finish(),
            Command::SetDeafened { deafened } => f.debug_struct("SetDeafened").field("deafened", deafened).finish(),
            Command::MuteUser { user_id } => f.debug_struct("MuteUser").field("user_id", user_id).finish(),
            Command::UnmuteUser { user_id } => f.debug_struct("UnmuteUser").field("user_id", user_id).finish(),
            Command::StartTransmit => write!(f, "StartTransmit"),
            Command::StopTransmit => write!(f, "StopTransmit"),
            Command::UpdateAudioSettings { settings } => f
                .debug_struct("UpdateAudioSettings")
                .field("settings", settings)
                .finish(),
            Command::ResetAudioStats => write!(f, "ResetAudioStats"),
            Command::UpdateTxPipeline { .. } => write!(f, "UpdateTxPipeline {{ .. }}"),
            Command::UpdateRxPipelineDefaults { .. } => {
                write!(f, "UpdateRxPipelineDefaults {{ .. }}")
            }
            Command::UpdateUserRxConfig { user_id, .. } => {
                f.debug_struct("UpdateUserRxConfig").field("user_id", user_id).finish()
            }
            Command::ClearUserRxOverride { user_id } => {
                f.debug_struct("ClearUserRxOverride").field("user_id", user_id).finish()
            }
            Command::SetUserVolume { user_id, volume_db } => f
                .debug_struct("SetUserVolume")
                .field("user_id", user_id)
                .field("volume_db", volume_db)
                .finish(),
            Command::RegisterUser { user_id } => f.debug_struct("RegisterUser").field("user_id", user_id).finish(),
            Command::UnregisterUser { user_id } => f.debug_struct("UnregisterUser").field("user_id", user_id).finish(),
            Command::ShareFile { path } => f.debug_struct("ShareFile").field("path", path).finish(),
            Command::DownloadFile { magnet } => f.debug_struct("DownloadFile").field("magnet", magnet).finish(),
            Command::PauseTransfer { infohash } => f.debug_struct("PauseTransfer").field("infohash", infohash).finish(),
            Command::ResumeTransfer { infohash } => {
                f.debug_struct("ResumeTransfer").field("infohash", infohash).finish()
            }
            Command::CancelTransfer { infohash } => {
                f.debug_struct("CancelTransfer").field("infohash", infohash).finish()
            }
            Command::RemoveTransfer { infohash, delete_file } => f
                .debug_struct("RemoveTransfer")
                .field("infohash", infohash)
                .field("delete_file", delete_file)
                .finish(),
            Command::SaveFileAs { infohash, destination } => f
                .debug_struct("SaveFileAs")
                .field("infohash", infohash)
                .field("destination", destination)
                .finish(),
            Command::OpenFile { infohash } => f.debug_struct("OpenFile").field("infohash", infohash).finish(),
        }
    }
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
                    parent_id: None,
                },
                RoomInfo {
                    id: Some(room_id_from_uuid(room2_uuid)),
                    name: "Room2".to_string(),
                    parent_id: None,
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
            file_transfers: vec![],
            room_tree: RoomTree::default(),
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
                parent_id: None,
            }],
            ..Default::default()
        };

        let room = state.get_room(room_uuid);
        assert!(room.is_some());
        assert_eq!(room.unwrap().name, "Root");

        assert!(state.get_room(Uuid::new_v4()).is_none());
    }

    #[test]
    fn test_room_tree_rebuild() {
        let root_uuid = ROOT_ROOM_UUID;
        let child1_uuid = Uuid::new_v4();
        let child2_uuid = Uuid::new_v4();
        let grandchild_uuid = Uuid::new_v4();

        let rooms = vec![
            RoomInfo {
                id: Some(room_id_from_uuid(root_uuid)),
                name: "Root".to_string(),
                parent_id: None,
            },
            RoomInfo {
                id: Some(room_id_from_uuid(child1_uuid)),
                name: "Alpha Channel".to_string(),
                parent_id: Some(room_id_from_uuid(root_uuid)),
            },
            RoomInfo {
                id: Some(room_id_from_uuid(child2_uuid)),
                name: "Beta Channel".to_string(),
                parent_id: Some(room_id_from_uuid(root_uuid)),
            },
            RoomInfo {
                id: Some(room_id_from_uuid(grandchild_uuid)),
                name: "Private".to_string(),
                parent_id: Some(room_id_from_uuid(child1_uuid)),
            },
        ];

        let mut tree = RoomTree::new();
        tree.rebuild(&rooms);

        // Check basic structure
        assert_eq!(tree.len(), 4);
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0], root_uuid);

        // Check root has two children (sorted by name: Alpha, Beta)
        let root_node = tree.get(root_uuid).unwrap();
        assert_eq!(root_node.children.len(), 2);
        assert_eq!(root_node.children[0], child1_uuid); // Alpha
        assert_eq!(root_node.children[1], child2_uuid); // Beta

        // Check child1 has grandchild
        let child1_node = tree.get(child1_uuid).unwrap();
        assert_eq!(child1_node.children.len(), 1);
        assert_eq!(child1_node.children[0], grandchild_uuid);

        // Check child2 has no children
        let child2_node = tree.get(child2_uuid).unwrap();
        assert!(child2_node.children.is_empty());

        // Check ancestors
        let ancestors: Vec<Uuid> = tree.ancestors(grandchild_uuid).collect();
        assert_eq!(ancestors.len(), 2);
        assert_eq!(ancestors[0], child1_uuid);
        assert_eq!(ancestors[1], root_uuid);

        // Check depth
        assert_eq!(tree.depth(root_uuid), 0);
        assert_eq!(tree.depth(child1_uuid), 1);
        assert_eq!(tree.depth(grandchild_uuid), 2);

        // Check is_ancestor
        assert!(tree.is_ancestor(root_uuid, grandchild_uuid));
        assert!(tree.is_ancestor(child1_uuid, grandchild_uuid));
        assert!(!tree.is_ancestor(child2_uuid, grandchild_uuid));
    }
}
