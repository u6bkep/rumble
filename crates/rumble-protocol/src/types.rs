//! Shared state, command and configuration types for the Rumble client.
//!
//! These types were originally defined in `rumble_client::events` and other backend
//! modules. They are pure-data types with no platform-specific dependencies
//! (no cpal, opus, etc.), so they live here in the `rumble-protocol` crate for
//! broader reuse.

use crate::proto::{RoomInfo, User};
use rumble_audio::{PipelineConfig, UserRxConfig};
use std::{
    collections::{HashMap, HashSet},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

// =============================================================================
// Utility
// =============================================================================

/// Get current time as milliseconds since UNIX epoch, with a safe fallback.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// =============================================================================
// Serde Helpers
// =============================================================================

pub(crate) fn serialize_system_time<S: serde::Serializer>(
    time: &std::time::SystemTime,
    s: S,
) -> Result<S::Ok, S::Error> {
    let millis = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    s.serialize_u64(millis)
}

pub(crate) fn serialize_id_hex<S: serde::Serializer>(id: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(id))
}

pub(crate) fn serialize_opt_instant<S: serde::Serializer>(_: &Option<Instant>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_none()
}

pub(crate) fn serialize_session_key<S: serde::Serializer>(key: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
    match key {
        Some(k) => s.serialize_some(&hex::encode(k)),
        None => s.serialize_none(),
    }
}

// =============================================================================
// Room Tree
// =============================================================================

/// A node in the room tree hierarchy.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RoomTreeNode {
    /// The room UUID.
    pub id: Uuid,
    /// The room name.
    pub name: String,
    /// The parent room UUID (None for root rooms).
    pub parent_id: Option<Uuid>,
    /// UUIDs of child rooms, in sorted order by name.
    pub children: Vec<Uuid>,
    /// Optional description for the room.
    pub description: Option<String>,
}

/// Hierarchical tree structure of rooms.
///
/// This is rebuilt whenever the room list changes. The UI can use this
/// to efficiently render the room list as a tree without rebuilding
/// the hierarchy on each frame.
#[derive(Debug, Clone, Default, serde::Serialize)]
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
            let Some(room_uuid) = room.id.as_ref().and_then(crate::uuid_from_room_id) else {
                continue;
            };
            let parent_uuid = room.parent_id.as_ref().and_then(crate::uuid_from_room_id);

            self.nodes.insert(
                room_uuid,
                RoomTreeNode {
                    id: room_uuid,
                    name: room.name.clone(),
                    parent_id: parent_uuid,
                    children: Vec::new(),
                    description: room.description.clone(),
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

// =============================================================================
// Audio Settings (Configurable)
// =============================================================================

/// Configurable audio pipeline settings.
///
/// These settings can be changed at runtime via the `UpdateAudioSettings` command.
///
/// Note: Audio processing (denoise, VAD, etc.) is configured via the TX pipeline,
/// not via these settings.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize)]
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
    #[serde(serialize_with = "serialize_opt_instant")]
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
#[derive(Clone, Debug)]
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

// Equality is by fingerprint + address tuple, not the full byte-for-byte cert.
impl PartialEq for PendingCertificate {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && self.server_name == other.server_name
            && self.server_addr == other.server_addr
    }
}

impl Eq for PendingCertificate {}

/// Connection lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// Not connected to any server.
    #[default]
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

impl serde::Serialize for ConnectionState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        match self {
            ConnectionState::Disconnected => {
                map.serialize_entry("state", "disconnected")?;
            }
            ConnectionState::Connecting { server_addr } => {
                map.serialize_entry("state", "connecting")?;
                map.serialize_entry("server_addr", server_addr)?;
            }
            ConnectionState::Connected { server_name, user_id } => {
                map.serialize_entry("state", "connected")?;
                map.serialize_entry("server_name", server_name)?;
                map.serialize_entry("user_id", user_id)?;
            }
            ConnectionState::ConnectionLost { error } => {
                map.serialize_entry("state", "connection_lost")?;
                map.serialize_entry("error", error)?;
            }
            ConnectionState::CertificatePending { cert_info } => {
                map.serialize_entry("state", "certificate_pending")?;
                map.serialize_entry("server_name", &cert_info.server_name)?;
                map.serialize_entry("fingerprint", &cert_info.fingerprint_hex())?;
            }
        }
        map.end()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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
// Audio Device Info
// =============================================================================

/// Information about an audio device.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AudioDeviceInfo {
    /// Stable, host-unique identifier (used for selection + persistence).
    ///
    /// On ALSA this is the cpal `pcm_id` (e.g. `pipewire`, `pulse`,
    /// `front:CARD=Generic_1,DEV=0`). On other hosts it's whatever
    /// `Device::id()` returns. The same physical card can appear under
    /// multiple ALSA endpoints, so two entries can share `name` but
    /// always have distinct `id`s.
    pub id: String,
    /// Human-readable name (e.g. `"AT2020USB+, USB Audio"`).
    /// Comes from `Device::description().name()`. Not unique on its own.
    pub name: String,
    /// Routing / driver tag the user can use to disambiguate same-named
    /// entries — typically the ALSA pcm pipeline (`pipewire`, `pulse`,
    /// `front:CARD=...`, `dsnoop:CARD=...`) or the host driver name
    /// on other platforms. `None` if the host doesn't expose one.
    #[serde(default)]
    pub pipeline: Option<String>,
    /// Whether this is the default device.
    pub is_default: bool,
}

// =============================================================================
// Audio State
// =============================================================================

/// Audio subsystem state.
#[derive(Debug, Clone, Default, serde::Serialize)]
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

/// The kind of chat message (room, DM, tree broadcast).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatMessageKind {
    /// Normal room chat message.
    #[default]
    Room,
    /// Direct (private) message. Contains the other user's ID and name.
    DirectMessage {
        /// The user ID of the other party (sender for incoming, target for outgoing).
        other_user_id: u64,
        /// The username of the other party.
        other_username: String,
    },
    /// Tree broadcast message (sent to a room and all descendants).
    Tree,
}

/// Plugin-namespaced sidecar attached to a chat message. The server
/// forwards this unchanged; receivers dispatch by `namespace` to find a
/// matching plugin renderer/handler. Clients without a matching plugin
/// display `fallback_text`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChatAttachment {
    /// Reverse-DNS plugin identifier (e.g. "rumble.file_transfer.relay").
    pub namespace: String,
    /// Schema version for the payload format, plugin-defined.
    pub schema_version: u32,
    /// Opaque plugin-encoded payload (consumed by the matching plugin).
    pub payload: Vec<u8>,
    /// Text shown when no plugin matches `namespace`.
    pub fallback_text: String,
}

pub fn chat_attachment_from_proto(att: crate::proto::ChatAttachment) -> ChatAttachment {
    ChatAttachment {
        namespace: att.namespace,
        schema_version: att.schema_version,
        payload: att.payload,
        fallback_text: att.fallback_text,
    }
}

pub fn chat_attachment_to_proto(att: &ChatAttachment) -> crate::proto::ChatAttachment {
    crate::proto::ChatAttachment {
        namespace: att.namespace.clone(),
        schema_version: att.schema_version,
        payload: att.payload.clone(),
        fallback_text: att.fallback_text.clone(),
    }
}

/// A chat message.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    /// Unique message ID (16-byte UUID).
    #[serde(serialize_with = "serialize_id_hex")]
    pub id: [u8; 16],
    pub sender: String,
    /// Server-assigned `user_id` of the authenticated sender. `None` for
    /// system/local messages and for messages received from peers running
    /// older clients/servers that don't populate the field. Identity
    /// matching ("is this my own message?") must prefer `sender_id` over
    /// `sender` because usernames are not unique and are client-supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<u64>,
    pub text: String,
    /// Wall-clock time when the message was received/created.
    #[serde(serialize_with = "serialize_system_time")]
    pub timestamp: std::time::SystemTime,
    /// True if this is a local status message (not from the server).
    pub is_local: bool,
    /// The kind of message (room chat, DM, or tree broadcast).
    #[serde(default)]
    pub kind: ChatMessageKind,
    /// Optional plugin-namespaced attachment. Receivers without a
    /// matching plugin fall back to `attachment.fallback_text` (or the
    /// message's `text` if no attachment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<ChatAttachment>,
    /// Client-only: sender's view-only message. Never goes on the wire,
    /// never included in chat-history-sync shares. Used for the
    /// sender's in-flight file-share card (driven by plugin transfer
    /// state).
    #[serde(skip)]
    pub local_only: bool,
    /// Client-only: sender's local copy of a broadcast they sent.
    /// NOT rendered to the sender (they have the local_only card for
    /// the same thing). IS included in history-sync shares so late
    /// peers can fetch it from the sender; the flag itself is stripped
    /// on egress.
    #[serde(skip)]
    pub remote_only: bool,
}

// =============================================================================
// Chat History Sync Messages
// =============================================================================

/// MIME type for chat history files.
pub const CHAT_HISTORY_MIME: &str = "application/x-rumble-chat-history";

/// A chat history request message.
///
/// Sent to request chat history from peers in the room.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatHistoryRequestMessage {
    /// Message type marker (always "chat-history-request").
    #[serde(rename = "type")]
    pub msg_type: String,
}

impl ChatHistoryRequestMessage {
    pub fn new() -> Self {
        Self {
            msg_type: "chat-history-request".to_string(),
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let obj = value.as_object()?;

        if obj.get("type")?.as_str()? != "chat-history-request" {
            return None;
        }

        // Only allow type and optional $schema fields
        for key in obj.keys() {
            if !["type", "$schema"].contains(&key.as_str()) {
                return None;
            }
        }

        Some(Self::new())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl Default for ChatHistoryRequestMessage {
    fn default() -> Self {
        Self::new()
    }
}

/// A single message in the chat history export format.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatHistoryEntry {
    /// Hex-encoded 16-byte UUID.
    pub id: String,
    /// Sender username.
    pub sender: String,
    /// Server-assigned `user_id` of the authenticated sender. Absent on
    /// entries received from older peers; absence falls back to legacy
    /// username-based identity matching on the receiving client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<u64>,
    /// Message text.
    pub text: String,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
    /// Optional attachment (file offer, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<ChatAttachment>,
}

/// Chat history file content.
///
/// Serialized as JSON and transferred via P2P file sharing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatHistoryContent {
    /// Schema version for forward compatibility.
    pub version: u32,
    /// Messages in chronological order.
    pub messages: Vec<ChatHistoryEntry>,
}

impl ChatHistoryContent {
    /// Current schema version.
    pub const VERSION: u32 = 1;

    /// Create a new chat history content from chat messages.
    ///
    /// Filters out:
    /// - `is_local` system messages (preamble lines, "Connect first" notices)
    /// - `local_only` sender-private cards (in-flight file shares)
    ///
    /// `remote_only` messages ARE included — that's the entire reason for
    /// the flag. The flag itself is sender-local and not serialized over
    /// the wire; peers receive a clean copy and render normally.
    pub fn from_messages(messages: &[ChatMessage]) -> Self {
        let entries = messages
            .iter()
            .filter(|m| !m.is_local && !m.local_only)
            .map(|m| ChatHistoryEntry {
                id: hex::encode(m.id),
                sender: m.sender.clone(),
                sender_id: m.sender_id,
                text: m.text.clone(),
                timestamp: m
                    .timestamp
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                attachment: m.attachment.clone(),
            })
            .collect();

        Self {
            version: Self::VERSION,
            messages: entries,
        }
    }

    /// Parse chat history from JSON.
    pub fn parse(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }

    /// Serialize to JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Convert entries back to ChatMessage structs for merging.
    pub fn to_messages(&self) -> Vec<ChatMessage> {
        self.messages
            .iter()
            .filter_map(|e| {
                let id_bytes = hex::decode(&e.id).ok()?;
                if id_bytes.len() != 16 {
                    return None;
                }
                let mut id = [0u8; 16];
                id.copy_from_slice(&id_bytes);

                let timestamp = std::time::UNIX_EPOCH + std::time::Duration::from_millis(e.timestamp);

                Some(ChatMessage {
                    id,
                    sender: e.sender.clone(),
                    sender_id: e.sender_id,
                    text: e.text.clone(),
                    timestamp,
                    is_local: false,
                    kind: ChatMessageKind::default(),
                    attachment: e.attachment.clone(),
                    local_only: false,
                    remote_only: false,
                })
            })
            .collect()
    }
}

/// A chat history response message — sent in-band over the room chat
/// channel in reply to a [`ChatHistoryRequestMessage`]. The payload is
/// the sender's recent non-local message history; receivers merge it
/// into their local log and suppress the raw JSON from display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatHistoryShareMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub content: ChatHistoryContent,
}

impl ChatHistoryShareMessage {
    pub fn new(content: ChatHistoryContent) -> Self {
        Self {
            msg_type: "chat-history-response".to_string(),
            content,
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let obj = value.as_object()?;
        if obj.get("type")?.as_str()? != "chat-history-response" {
            return None;
        }
        serde_json::from_value(value).ok()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// =============================================================================
// Sound Effect Kind
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SfxKind {
    UserJoin,
    UserLeave,
    Connect,
    Disconnect,
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    Message,
    PrivateMessage,
    SelfChannelJoin,
    SelfChannelMoved,
    ServerMute,
    Kicked,
}

impl SfxKind {
    pub fn all() -> &'static [SfxKind] {
        &[
            SfxKind::UserJoin,
            SfxKind::UserLeave,
            SfxKind::Connect,
            SfxKind::Disconnect,
            SfxKind::Mute,
            SfxKind::Unmute,
            SfxKind::Deafen,
            SfxKind::Undeafen,
            SfxKind::Message,
            SfxKind::PrivateMessage,
            SfxKind::SelfChannelJoin,
            SfxKind::SelfChannelMoved,
            SfxKind::ServerMute,
            SfxKind::Kicked,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            SfxKind::UserJoin => "User Join",
            SfxKind::UserLeave => "User Leave",
            SfxKind::Connect => "Connect",
            SfxKind::Disconnect => "Disconnect",
            SfxKind::Mute => "Mute",
            SfxKind::Unmute => "Unmute",
            SfxKind::Deafen => "Deafen",
            SfxKind::Undeafen => "Undeafen",
            SfxKind::Message => "Message",
            SfxKind::PrivateMessage => "Private Message",
            SfxKind::SelfChannelJoin => "Channel Switch",
            SfxKind::SelfChannelMoved => "Moved by Another",
            SfxKind::ServerMute => "Server Muted",
            SfxKind::Kicked => "Kicked",
        }
    }
}

// =============================================================================
// Opus Encoder Settings & Constants
// =============================================================================

/// Sample rate for Opus encoding/decoding (48kHz).
pub const OPUS_SAMPLE_RATE: u32 = 48000;

/// Frame size in samples for 20ms at 48kHz.
///
/// Opus supports frame sizes of 2.5, 5, 10, 20, 40, or 60 ms.
/// 20ms is a good balance between latency and compression efficiency.
/// At 48kHz: 20ms = 0.020 * 48000 = 960 samples.
pub const OPUS_FRAME_SIZE: usize = 960;

/// Maximum size of an encoded Opus frame in bytes.
///
/// For 20ms mono at 128kbps, max is about 320 bytes.
/// We use 4000 bytes to be safe with higher bitrates.
pub const OPUS_MAX_PACKET_SIZE: usize = 4000;

/// Default target bitrate for voice (in bits per second).
/// 64 kbps provides good quality for voice.
pub const OPUS_DEFAULT_BITRATE: i32 = 64000;

/// Default encoder complexity (0-10).
/// 10 is highest quality, most CPU intensive.
pub const OPUS_DEFAULT_COMPLEXITY: i32 = 10;

/// Default expected packet loss percentage for FEC configuration.
/// 5% is a reasonable default for internet voice chat.
pub const OPUS_DEFAULT_PACKET_LOSS_PERC: i32 = 5;

/// Maximum size in bytes for a DTX (discontinuous transmission) silence frame.
/// When Opus DTX is enabled and the encoder detects silence, it produces very
/// small frames (typically 1-2 bytes). We use <=2 bytes as the threshold to
/// identify these DTX frames for the purpose of skipping/keepalive logic.
pub const DTX_FRAME_MAX_SIZE: usize = 2;

/// Configurable settings for the Opus encoder.
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderSettings {
    /// Target bitrate in bits per second.
    /// Range: 6000 - 510000. Recommended: 24000 - 96000 for voice.
    pub bitrate: i32,

    /// Encoder complexity (0-10).
    /// Higher values = better quality but more CPU usage.
    pub complexity: i32,

    /// Enable Forward Error Correction for packet loss recovery.
    pub fec_enabled: bool,

    /// Expected packet loss percentage (0-100) for FEC tuning.
    pub packet_loss_percent: i32,

    /// Enable discontinuous transmission (silence compression).
    pub dtx_enabled: bool,

    /// Enable variable bitrate for better quality/bandwidth trade-off.
    pub vbr_enabled: bool,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate: OPUS_DEFAULT_BITRATE,
            complexity: OPUS_DEFAULT_COMPLEXITY,
            fec_enabled: true,
            packet_loss_percent: OPUS_DEFAULT_PACKET_LOSS_PERC,
            dtx_enabled: true,
            vbr_enabled: true,
        }
    }
}

// =============================================================================
// Main State Struct
// =============================================================================

/// The complete client state exposed to the UI.
///
/// The UI renders based on this state. User actions result in Commands
/// sent to the backend, which updates this state and calls the repaint callback.
#[derive(Debug, Clone, Default, serde::Serialize)]
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
    /// Ephemeral session public key for this connection.
    #[serde(serialize_with = "serialize_session_key")]
    pub my_session_public_key: Option<[u8; 32]>,
    /// Stable session identifier derived from the session public key.
    #[serde(serialize_with = "serialize_session_key")]
    pub my_session_id: Option<[u8; 32]>,

    // Audio
    /// Audio subsystem state.
    pub audio: AudioState,

    // Chat (recent messages, not persisted)
    /// Recent chat messages.
    pub chat_messages: Vec<ChatMessage>,

    // Room tree (derived from rooms)
    /// Hierarchical tree structure of rooms, rebuilt when rooms change.
    pub room_tree: RoomTree,

    // ACL state
    /// Effective permissions for our current room (bitmask from PermissionsInfo).
    pub effective_permissions: u32,
    /// Per-room effective permissions (room UUID -> permission bitmask).
    /// Populated from server-computed values in ServerState and updated on ACL changes.
    pub per_room_permissions: HashMap<Uuid, u32>,
    /// Last permission denied message (for UI toast display). Cleared after reading.
    pub permission_denied: Option<String>,
    /// Kick reason if we were kicked (for disconnect dialog). Cleared after reading.
    pub kicked: Option<String>,
    /// Server-defined permission group definitions (synced from ServerState).
    pub group_definitions: Vec<crate::proto::GroupInfo>,
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
            .filter(|u| u.current_room.as_ref().and_then(crate::uuid_from_room_id) == Some(room_uuid))
            .collect()
    }

    /// Check if a user is in a room.
    pub fn is_user_in_room(&self, user_id: u64, room_uuid: Uuid) -> bool {
        self.users.iter().any(|u| {
            u.user_id.as_ref().map(|id| id.value) == Some(user_id)
                && u.current_room.as_ref().and_then(crate::uuid_from_room_id) == Some(room_uuid)
        })
    }

    /// Get room by UUID.
    pub fn get_room(&self, room_uuid: Uuid) -> Option<&RoomInfo> {
        self.rooms
            .iter()
            .find(|r| r.id.as_ref().and_then(crate::uuid_from_room_id) == Some(room_uuid))
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
        password: Option<String>, // Server password (for unknown keys)
    },
    /// Disconnect from the current server.
    Disconnect,
    /// Send a graceful disconnect to the server and terminate the
    /// connection task. Used during process shutdown so the server
    /// removes us from its state immediately instead of waiting for
    /// the QUIC idle timeout.
    Shutdown,
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
    /// Set a room's description.
    SetRoomDescription {
        room_id: Uuid,
        description: String,
    },
    /// Send a chat message.
    SendChat {
        text: String,
    },
    /// Send a tree chat message (broadcast to room and all descendants).
    SendTreeChat {
        text: String,
    },
    /// Send a direct (private) message to a specific user.
    SendDirectMessage {
        target_user_id: u64,
        target_username: String,
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
    /// Accept an incoming file offer and start downloading.
    /// `share_data` is the opaque payload from the offer's attachment.
    DownloadFile {
        share_data: String,
    },
    /// Cancel an in-flight share or download. The transfer is removed
    /// from the plugin; partial data on disk is left in place (the
    /// plugin's `delete_files` flag is currently kept off so the user
    /// can still access an interrupted download manually).
    CancelTransfer {
        transfer_id: String,
    },

    // Sound Effects
    /// Play a sound effect.
    PlaySfx {
        kind: SfxKind,
        volume: f32,
    },

    // Chat History Sync
    /// Request chat history from peers in the current room.
    RequestChatHistory,
    /// Internal: Share chat history in response to a request.
    /// This is triggered by receiving a ChatHistoryRequestMessage.
    #[doc(hidden)]
    ShareChatHistory,

    // ACL Commands
    /// Kick a user from the server.
    KickUser {
        target_user_id: u64,
        reason: String,
    },
    /// Ban a user from the server (adds to "banned" group and kicks).
    BanUser {
        target_user_id: u64,
        reason: String,
        /// Duration in seconds; None = permanent.
        duration_seconds: Option<u64>,
    },
    /// Set server mute on another user.
    SetServerMute {
        target_user_id: u64,
        muted: bool,
    },
    /// Elevate to superuser (sudo).
    Elevate {
        password: String,
    },
    /// Create a new permission group.
    CreateGroup {
        name: String,
        permissions: u32,
    },
    /// Delete a permission group.
    DeleteGroup {
        name: String,
    },
    /// Modify an existing permission group.
    ModifyGroup {
        name: String,
        permissions: u32,
    },
    /// Add or remove a user from a group.
    SetUserGroup {
        target_user_id: u64,
        group: String,
        add: bool,
        expires_at: u64,
    },
    /// Set room ACL entries.
    SetRoomAcl {
        room_id: Uuid,
        inherit_acl: bool,
        entries: Vec<crate::proto::RoomAclEntry>,
    },
}

// Hand-written Debug — hides password presence as a boolean and abbreviates
// the public key for log readability.
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
            Command::Shutdown => write!(f, "Shutdown"),
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
            Command::SetRoomDescription { room_id, description } => f
                .debug_struct("SetRoomDescription")
                .field("room_id", room_id)
                .field("description", description)
                .finish(),
            Command::SendChat { text } => f.debug_struct("SendChat").field("text", text).finish(),
            Command::SendTreeChat { text } => f.debug_struct("SendTreeChat").field("text", text).finish(),
            Command::SendDirectMessage {
                target_user_id,
                target_username,
                text,
            } => f
                .debug_struct("SendDirectMessage")
                .field("target_user_id", target_user_id)
                .field("target_username", target_username)
                .field("text", text)
                .finish(),
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
            Command::DownloadFile { share_data } => f
                .debug_struct("DownloadFile")
                .field("share_data_len", &share_data.len())
                .finish(),
            Command::CancelTransfer { transfer_id } => f
                .debug_struct("CancelTransfer")
                .field("transfer_id", transfer_id)
                .finish(),
            Command::PlaySfx { kind, volume } => f
                .debug_struct("PlaySfx")
                .field("kind", kind)
                .field("volume", volume)
                .finish(),
            Command::RequestChatHistory => write!(f, "RequestChatHistory"),
            Command::ShareChatHistory => write!(f, "ShareChatHistory"),
            Command::KickUser { target_user_id, reason } => f
                .debug_struct("KickUser")
                .field("target_user_id", target_user_id)
                .field("reason", reason)
                .finish(),
            Command::BanUser {
                target_user_id,
                reason,
                duration_seconds,
            } => f
                .debug_struct("BanUser")
                .field("target_user_id", target_user_id)
                .field("reason", reason)
                .field("duration_seconds", duration_seconds)
                .finish(),
            Command::SetServerMute { target_user_id, muted } => f
                .debug_struct("SetServerMute")
                .field("target_user_id", target_user_id)
                .field("muted", muted)
                .finish(),
            Command::Elevate { .. } => write!(f, "Elevate {{ .. }}"),
            Command::CreateGroup { name, permissions } => f
                .debug_struct("CreateGroup")
                .field("name", name)
                .field("permissions", permissions)
                .finish(),
            Command::DeleteGroup { name } => f.debug_struct("DeleteGroup").field("name", name).finish(),
            Command::ModifyGroup { name, permissions } => f
                .debug_struct("ModifyGroup")
                .field("name", name)
                .field("permissions", permissions)
                .finish(),
            Command::SetUserGroup {
                target_user_id,
                group,
                add,
                expires_at,
            } => f
                .debug_struct("SetUserGroup")
                .field("target_user_id", target_user_id)
                .field("group", group)
                .field("add", add)
                .field("expires_at", expires_at)
                .finish(),
            Command::SetRoomAcl {
                room_id,
                inherit_acl,
                entries,
            } => f
                .debug_struct("SetRoomAcl")
                .field("room_id", room_id)
                .field("inherit_acl", inherit_acl)
                .field("entries_count", &entries.len())
                .finish(),
        }
    }
}
