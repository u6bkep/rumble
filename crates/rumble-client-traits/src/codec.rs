//! Voice codec abstraction for encoding and decoding audio.
//!
//! `EncoderSettings` and the `OPUS_*` constants live here (not in
//! `rumble-client::events`) so platform codec impls (`rumble-desktop`)
//! can consume them without a dep on the engine crate.

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

/// Encodes PCM audio into compressed frames.
pub trait VoiceEncoder: Send {
    /// Encode a frame of PCM samples into `output`. Returns the number of
    /// bytes written.
    fn encode(&mut self, pcm: &[f32], output: &mut [u8]) -> anyhow::Result<usize>;

    /// Apply new encoder settings (bitrate, complexity, etc.).
    fn apply_settings(&mut self, settings: &EncoderSettings) -> anyhow::Result<()>;
}

/// Decodes compressed frames back to PCM audio.
pub trait VoiceDecoder: Send {
    /// Decode a compressed frame into PCM samples. Returns the number of
    /// samples written.
    fn decode(&mut self, data: &[u8], output: &mut [f32]) -> anyhow::Result<usize>;

    /// Generate a packet-loss concealment frame (no data received).
    fn decode_plc(&mut self, output: &mut [f32]) -> anyhow::Result<usize>;

    /// Decode using Forward Error Correction data from a later packet.
    fn decode_fec(&mut self, data: &[u8], output: &mut [f32]) -> anyhow::Result<usize>;
}

/// Factory for creating voice encoder and decoder instances.
///
/// Implementations wrap a specific codec (e.g. Opus).
pub trait VoiceCodec: Send + 'static {
    type Encoder: VoiceEncoder;
    type Decoder: VoiceDecoder;

    /// Create a new encoder with the given settings.
    fn create_encoder(settings: &EncoderSettings) -> anyhow::Result<Self::Encoder>;

    /// Create a new decoder with default settings.
    fn create_decoder() -> anyhow::Result<Self::Decoder>;
}
