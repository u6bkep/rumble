//! Backend library for the Rumble voice chat client.
//!
//! This crate provides the core client functionality:
//! - Connection management via QUIC
//! - Audio capture and playback
//! - State-driven API for UI integration
//!
//! # Architecture
//!
//! The backend exposes a **state-driven API** to the UI. The UI does not poll for
//! individual events; instead:
//!
//! 1. The backend exposes a `State` object representing the complete client state
//! 2. The UI renders based on this state
//! 3. User actions result in **commands** sent to the backend
//! 4. The backend updates state and notifies the UI via a **repaint callback**
//! 5. The UI re-renders from the new state
//!
//! # Usage
//!
//! ```ignore
//! use rumble_client::{handle::BackendHandle, Command, State};
//!
//! // Create handle with a repaint callback (generic over Platform)
//! let handle = BackendHandle::<MyPlatform>::new(|| {
//!     // Request UI repaint
//! });
//!
//! // Send commands
//! handle.send(Command::Connect {
//!     addr: "127.0.0.1:5000".to_string(),
//!     name: "user".to_string(),
//!     password: None,
//! });
//!
//! // Read state for rendering
//! let state = handle.state();
//! match &state.connection {
//!     ConnectionState::Connected { .. } => { /* render connected UI */ }
//!     _ => { /* render disconnected UI */ }
//! }
//! ```
//!
//! ## Invariants / gotchas
//! - Remote Opus decoders must be **per-peer and long-lived** (persist across talk spurts).
//!   They should be cleared only when the peer leaves (or as a long-TTL fallback), otherwise
//!   you will hear a crackle at speech start and see repeated `decoder initialized` logs.

use std::path::PathBuf;

// Audio constants and types (concrete I/O moved to rumble-desktop)
pub mod audio;
pub use audio::{AudioDeviceInfo, CHANNELS, SAMPLE_RATE};

// Opus codec constants and utilities (concrete encoder/decoder moved to rumble-desktop)
pub mod codec;
pub use codec::{EncoderSettings, OPUS_FRAME_SIZE, OPUS_MAX_PACKET_SIZE, OPUS_SAMPLE_RATE, is_dtx_frame};

// Bounded voice channel for handling slow connections
pub mod bounded_voice;
pub use bounded_voice::{
    BoundedVoiceReceiver, BoundedVoiceSender, VoiceChannelConfig, VoiceChannelStats, bounded_voice_channel,
};

// Audio task (handles datagrams, cpal streams, Opus, jitter buffers)
pub mod audio_task;
pub use audio_task::{AudioCommand, AudioTaskConfig, AudioTaskHandle, spawn_audio_task};

// Audio dumping for debugging
pub mod audio_dump;
pub use audio_dump::{AudioDumpConfig, AudioDumper};

// Certificate-handling types live in rumble-client-traits; re-export for
// callers that have historically imported them as `rumble_client::...`.
pub use rumble_client_traits::cert::{
    CapturedCert, ServerCertInfo, compute_sha256_fingerprint, is_cert_error_message, new_captured_cert,
    peek_captured_cert, take_captured_cert,
};

// State and command types
pub mod events;
// Replace the brittle, hand-maintained re-export list with a wildcard re-export to avoid
// build breaks when items in `events` are renamed/moved.
pub use events::*;

// Typed per-domain event channels emitted by the connection/audio
// tasks. Consumed by `projection::spawn_projection_task` (the sole
// `State` writer, phase 2) and by external subscribers via
// `BackendHandle::subscribe_*` methods.
pub mod domain_events;
pub use domain_events::{
    ChatEvent, ConnectionEvent, DeviceKind, EVENT_BROADCAST_CAPACITY, RoomEvent, TransferEvent, VoiceEvent,
};

// Projection task: consumes every domain channel and (phase 2) is
// the sole writer of `Arc<RwLock<State>>`.
pub mod projection;
pub use projection::{BusReceivers, EventBus, spawn_projection_task};

// Backend handle (generic over Platform)
pub mod handle;
pub use handle::{BackendEvent, NotificationLevel};

// RPC server for external process control (Unix only — uses Unix domain sockets)
#[cfg(unix)]
pub mod rpc;

// Waveform synthesizer for sound effects
pub mod synth;

// Sound effect definitions and library
pub mod sfx;
pub use sfx::{SfxKind, SfxLibrary};

// Single-writer / multi-reader "latest value wins" transport for
// sampled, lossy-tolerant signals (meter levels, stats roll-up). Kept
// off the projection event bus so high-frequency updates don't pressure
// the broadcast channel or force a per-event repaint carve-out.
pub mod snapshot;
pub use snapshot::{Snapshot, SnapshotWriter};

// Live audio metering value types, published over a `snapshot` channel.
pub mod meter;
pub use meter::{Level, MeterSnapshot};

// TX pre-roll ring: retains VAD-suppressed capture frames so the encode gate
// can flush them retroactively when the gate opens (speech-onset clipping fix).
pub(crate) mod preroll;

// Audio processing pipeline - processors
pub mod processors;
pub use processors::{
    DenoiseProcessor, DenoiseProcessorFactory, GainProcessor, GainProcessorFactory, NoiseGateProcessor,
    NoiseGateProcessorFactory, build_default_tx_pipeline, merge_with_default_tx_pipeline, register_builtin_processors,
};

// Re-export pipeline crate types
pub use rumble_audio::{
    Anchor, AudioPipeline, AudioProcessor, Mark, OutputEntry, OutputFrame, OutputKind, OutputLayout, OutputSpec,
    PipelineConfig, ProcessorConfig, ProcessorFactory, ProcessorRegistry, ProcessorResult, Role, UserRxConfig, Zone,
    calculate_peak_db, calculate_rms_db, db_to_linear, linear_to_db,
};

// Re-exports from rumble-protocol crate
pub use rumble_protocol::{ROOT_ROOM_UUID, proto::VoiceDatagram};

/// Configuration for the backend client.
#[derive(Clone, Debug, Default)]
pub struct ConnectConfig {
    /// Additional certificate paths (DER or PEM format) to trust for server verification.
    /// These are added to the system root certificates (webpki_roots).
    /// Use this to add self-signed or development certificates from files.
    pub additional_certs: Vec<PathBuf>,

    /// Certificates that have been accepted by the user during this session.
    /// These are DER-encoded certificate bytes that will be added to the trust store.
    /// This is typically populated when the user accepts a self-signed certificate prompt.
    pub accepted_certs: Vec<Vec<u8>>,

    /// Directory to store downloaded files.
    /// If None, defaults to system temp dir + "rumble_downloads".
    pub download_dir: Option<PathBuf>,

    /// If true, always use relay mode for file transfers.
    /// This is useful when behind NAT or when you want to hide your IP.
    pub prefer_relay: bool,

    /// Initial file-transfer download speed limit in bytes/sec; 0 = unlimited.
    /// Live updates go through [`Command::SetFileTransferSpeedLimits`].
    pub download_speed_limit: u64,

    /// Initial file-transfer upload speed limit in bytes/sec; 0 = unlimited.
    /// Live updates go through [`Command::SetFileTransferSpeedLimits`].
    pub upload_speed_limit: u64,
}

impl ConnectConfig {
    /// Create a new config with default settings (only webpki system roots trusted).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an additional certificate path to trust (DER or PEM format).
    pub fn with_cert(mut self, path: impl Into<PathBuf>) -> Self {
        self.additional_certs.push(path.into());
        self
    }

    /// Set the download directory.
    pub fn with_download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.download_dir = Some(path.into());
        self
    }

    /// Enable relay mode for file transfers (useful behind NAT).
    pub fn with_prefer_relay(mut self, prefer: bool) -> Self {
        self.prefer_relay = prefer;
        self
    }

    /// Set the initial file-transfer speed limits (bytes/sec; 0 = unlimited).
    pub fn with_speed_limits(mut self, download_bps: u64, upload_bps: u64) -> Self {
        self.download_speed_limit = download_bps;
        self.upload_speed_limit = upload_bps;
        self
    }
}
