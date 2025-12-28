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
//! use backend::{BackendHandle, Command, State};
//!
//! // Create handle with a repaint callback
//! let handle = BackendHandle::new(|| {
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

use std::path::PathBuf;

// Audio subsystem
pub mod audio;
pub use audio::{
    AudioConfig, AudioDeviceInfo, AudioInput, AudioOutput, AudioSystem, CHANNELS, FRAME_SIZE,
    MAX_PLAYBACK_BUFFER_SAMPLES, SAMPLE_RATE, bytes_to_samples, samples_to_bytes,
};

// Opus codec for voice encoding/decoding
pub mod codec;
pub use codec::{
    CodecError, DecoderStats, EncoderSettings, EncoderStats, OPUS_FRAME_SIZE, OPUS_MAX_PACKET_SIZE,
    OPUS_SAMPLE_RATE, VoiceDecoder, VoiceEncoder, opus_version,
};

// Bounded voice channel for handling slow connections
pub mod bounded_voice;
pub use bounded_voice::{
    BoundedVoiceReceiver, BoundedVoiceSender, VoiceChannelConfig, VoiceChannelStats,
    bounded_voice_channel,
};

// Audio task (handles datagrams, cpal streams, Opus, jitter buffers)
pub mod audio_task;
pub use audio_task::{AudioCommand, AudioTaskConfig, AudioTaskHandle, spawn_audio_task};

// State and command types
pub mod events;
pub use events::{AudioSettings, AudioState, AudioStats, ChatMessage, Command, ConnectionState, State, VoiceMode, TransmissionMode};

// Backend handle
pub mod handle;
pub use handle::BackendHandle;

// Audio processing pipeline - processors
pub mod processors;
pub use processors::{
    register_builtin_processors,
    build_default_tx_pipeline,
    merge_with_default_tx_pipeline,
    GainProcessor, GainProcessorFactory,
    DenoiseProcessor, DenoiseProcessorFactory,
    VadProcessor, VadProcessorFactory,
};

// Re-export pipeline crate types
pub use pipeline::{
    AudioPipeline, AudioProcessor, ProcessorConfig, ProcessorFactory,
    ProcessorRegistry, ProcessorResult, PipelineConfig, UserRxConfig,
    calculate_rms_db, calculate_peak_db, db_to_linear, linear_to_db,
};

// Re-exports from api crate
pub use api::proto::VoiceDatagram;
pub use api::ROOT_ROOM_UUID;

/// Configuration for the backend client.
#[derive(Clone, Debug, Default)]
pub struct ConnectConfig {
    /// Additional certificate paths (DER format) to trust for server verification.
    /// These are added to the system root certificates (webpki_roots).
    /// Use this to add self-signed or development certificates.
    pub additional_certs: Vec<PathBuf>,
}

impl ConnectConfig {
    /// Create a new config with default settings (only webpki system roots trusted).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an additional certificate path to trust (DER format).
    pub fn with_cert(mut self, path: impl Into<PathBuf>) -> Self {
        self.additional_certs.push(path.into());
        self
    }
}
