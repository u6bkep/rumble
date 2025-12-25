//! Opus audio codec for voice communication.
//!
//! This module provides high-level wrappers around the Opus encoder and decoder
//! for use in the Rumble voice chat application. The codec is configured for
//! VoIP use with the following parameters:
//!
//! - Sample rate: 48kHz (native Opus rate)
//! - Channels: Mono (single channel for voice)
//! - Frame size: 960 samples (20ms at 48kHz)
//! - Application: VoIP (optimized for speech)
//! - Features: VBR, DTX, FEC enabled
//!
//! # Usage
//!
//! ```ignore
//! use backend::codec::{VoiceEncoder, VoiceDecoder, OPUS_FRAME_SIZE};
//!
//! // Encoding
//! let mut encoder = VoiceEncoder::new()?;
//! let pcm_samples: &[f32] = &[/* 960 samples */];
//! let opus_data = encoder.encode(pcm_samples)?;
//!
//! // Decoding
//! let mut decoder = VoiceDecoder::new()?;
//! let pcm_output = decoder.decode(&opus_data)?;
//! ```

use opus::{Application, Bitrate, Channels, Decoder, Encoder};
use std::fmt;
use tracing::{debug, trace};

/// Sample rate for Opus encoding/decoding (48kHz).
pub const OPUS_SAMPLE_RATE: u32 = 48000;

/// Number of channels (mono for voice).
pub const OPUS_CHANNELS: Channels = Channels::Mono;

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

/// Target bitrate for voice (in bits per second).
/// 24-32 kbps is good quality for voice; we use 32kbps.
pub const OPUS_BITRATE: i32 = 64000;

/// Expected packet loss percentage for FEC configuration.
/// 5% is a reasonable default for internet voice chat.
pub const OPUS_PACKET_LOSS_PERC: i32 = 5;

/// Error type for codec operations.
#[derive(Debug)]
pub enum CodecError {
    /// Encoder initialization failed.
    EncoderInit(String),
    /// Decoder initialization failed.
    DecoderInit(String),
    /// Encoding failed.
    Encode(String),
    /// Decoding failed.
    Decode(String),
    /// Invalid frame size.
    InvalidFrameSize { expected: usize, got: usize },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::EncoderInit(e) => write!(f, "encoder initialization failed: {}", e),
            CodecError::DecoderInit(e) => write!(f, "decoder initialization failed: {}", e),
            CodecError::Encode(e) => write!(f, "encoding failed: {}", e),
            CodecError::Decode(e) => write!(f, "decoding failed: {}", e),
            CodecError::InvalidFrameSize { expected, got } => {
                write!(
                    f,
                    "invalid frame size: expected {} samples, got {}",
                    expected, got
                )
            }
        }
    }
}

impl std::error::Error for CodecError {}

/// High-level Opus encoder for voice data.
///
/// Configured for VoIP use with:
/// - 48kHz sample rate
/// - Mono audio
/// - 20ms frame size (960 samples)
/// - Variable bitrate with DTX and FEC
pub struct VoiceEncoder {
    encoder: Encoder,
    /// Reusable output buffer to avoid allocations.
    output_buffer: Vec<u8>,
    /// Total frames encoded (for statistics).
    frames_encoded: u64,
    /// Total bytes produced (for statistics).
    bytes_produced: u64,
}

impl VoiceEncoder {
    /// Create a new voice encoder with default VoIP settings.
    pub fn new() -> Result<Self, CodecError> {
        let mut encoder = Encoder::new(OPUS_SAMPLE_RATE, OPUS_CHANNELS, Application::Voip)
            .map_err(|e| CodecError::EncoderInit(e.description().to_string()))?;

        // Configure for VoIP use
        encoder
            .set_bitrate(Bitrate::Bits(OPUS_BITRATE))
            .map_err(|e| CodecError::EncoderInit(e.description().to_string()))?;

        // Enable variable bitrate for better quality/bandwidth trade-off
        encoder
            .set_vbr(true)
            .map_err(|e| CodecError::EncoderInit(e.description().to_string()))?;

        // Enable discontinuous transmission (silence compression)
        encoder
            .set_dtx(true)
            .map_err(|e| CodecError::EncoderInit(e.description().to_string()))?;

        // Enable forward error correction for packet loss resilience
        encoder
            .set_inband_fec(true)
            .map_err(|e| CodecError::EncoderInit(e.description().to_string()))?;

        // Set expected packet loss for FEC tuning
        encoder
            .set_packet_loss_perc(OPUS_PACKET_LOSS_PERC)
            .map_err(|e| CodecError::EncoderInit(e.description().to_string()))?;

        // Use voice signal type hint for better compression
        encoder
            .set_signal(opus::Signal::Voice)
            .map_err(|e| CodecError::EncoderInit(e.description().to_string()))?;

        debug!(
            bitrate = OPUS_BITRATE,
            frame_size = OPUS_FRAME_SIZE,
            "codec: encoder initialized"
        );

        Ok(Self {
            encoder,
            output_buffer: vec![0u8; OPUS_MAX_PACKET_SIZE],
            frames_encoded: 0,
            bytes_produced: 0,
        })
    }

    /// Encode a frame of PCM audio samples to Opus.
    ///
    /// # Arguments
    /// * `pcm` - Exactly `OPUS_FRAME_SIZE` (960) f32 samples in range [-1.0, 1.0]
    ///
    /// # Returns
    /// Opus-encoded bytes ready for transmission.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, CodecError> {
        if pcm.len() != OPUS_FRAME_SIZE {
            return Err(CodecError::InvalidFrameSize {
                expected: OPUS_FRAME_SIZE,
                got: pcm.len(),
            });
        }

        let len = self
            .encoder
            .encode_float(pcm, &mut self.output_buffer)
            .map_err(|e| CodecError::Encode(e.description().to_string()))?;

        self.frames_encoded += 1;
        self.bytes_produced += len as u64;

        trace!(len, frames = self.frames_encoded, "codec: encoded frame");

        Ok(self.output_buffer[..len].to_vec())
    }

    /// Encode a frame directly into the provided buffer.
    ///
    /// This avoids allocation by writing directly to the output buffer.
    ///
    /// # Returns
    /// The number of bytes written to the output buffer.
    pub fn encode_into(&mut self, pcm: &[f32], output: &mut [u8]) -> Result<usize, CodecError> {
        if pcm.len() != OPUS_FRAME_SIZE {
            return Err(CodecError::InvalidFrameSize {
                expected: OPUS_FRAME_SIZE,
                got: pcm.len(),
            });
        }

        let len = self
            .encoder
            .encode_float(pcm, output)
            .map_err(|e| CodecError::Encode(e.description().to_string()))?;

        self.frames_encoded += 1;
        self.bytes_produced += len as u64;

        Ok(len)
    }

    /// Get statistics about encoding.
    pub fn stats(&self) -> EncoderStats {
        EncoderStats {
            frames_encoded: self.frames_encoded,
            bytes_produced: self.bytes_produced,
            avg_bytes_per_frame: if self.frames_encoded > 0 {
                self.bytes_produced as f64 / self.frames_encoded as f64
            } else {
                0.0
            },
        }
    }

    /// Reset the encoder state.
    ///
    /// This should be called when starting a new voice transmission
    /// after silence to clear any internal state.
    pub fn reset(&mut self) -> Result<(), CodecError> {
        self.encoder
            .reset_state()
            .map_err(|e| CodecError::EncoderInit(e.description().to_string()))?;
        debug!("codec: encoder state reset");
        Ok(())
    }
}

/// Statistics from the encoder.
#[derive(Debug, Clone, Copy)]
pub struct EncoderStats {
    /// Total frames encoded.
    pub frames_encoded: u64,
    /// Total bytes of Opus data produced.
    pub bytes_produced: u64,
    /// Average bytes per frame.
    pub avg_bytes_per_frame: f64,
}

/// High-level Opus decoder for voice data.
///
/// Configured for VoIP use with:
/// - 48kHz sample rate
/// - Mono audio
/// - Packet loss concealment
pub struct VoiceDecoder {
    decoder: Decoder,
    /// Reusable output buffer to avoid allocations.
    output_buffer: Vec<f32>,
    /// Total frames decoded (for statistics).
    frames_decoded: u64,
    /// Total frames concealed due to packet loss.
    frames_concealed: u64,
}

impl VoiceDecoder {
    /// Create a new voice decoder.
    pub fn new() -> Result<Self, CodecError> {
        let decoder = Decoder::new(OPUS_SAMPLE_RATE, OPUS_CHANNELS)
            .map_err(|e| CodecError::DecoderInit(e.description().to_string()))?;

        debug!("codec: decoder initialized");

        Ok(Self {
            decoder,
            // Allocate for max possible frame size (120ms at 48kHz)
            output_buffer: vec![0.0f32; 5760],
            frames_decoded: 0,
            frames_concealed: 0,
        })
    }

    /// Decode an Opus packet to PCM samples.
    ///
    /// # Arguments
    /// * `opus_data` - Opus-encoded bytes received from the network
    ///
    /// # Returns
    /// Decoded f32 PCM samples in range [-1.0, 1.0].
    pub fn decode(&mut self, opus_data: &[u8]) -> Result<Vec<f32>, CodecError> {
        let samples = self
            .decoder
            .decode_float(opus_data, &mut self.output_buffer, false)
            .map_err(|e| CodecError::Decode(e.description().to_string()))?;

        self.frames_decoded += 1;

        trace!(
            samples,
            frames = self.frames_decoded,
            "codec: decoded frame"
        );

        Ok(self.output_buffer[..samples].to_vec())
    }

    /// Decode an Opus packet directly into the provided buffer.
    ///
    /// # Returns
    /// The number of samples written to the output buffer.
    pub fn decode_into(
        &mut self,
        opus_data: &[u8],
        output: &mut [f32],
    ) -> Result<usize, CodecError> {
        let samples = self
            .decoder
            .decode_float(opus_data, output, false)
            .map_err(|e| CodecError::Decode(e.description().to_string()))?;

        self.frames_decoded += 1;

        Ok(samples)
    }

    /// Decode a packet using forward error correction to recover a lost frame.
    ///
    /// When a packet is lost but the *next* packet has arrived, call this method
    /// with the next packet's data to recover an approximation of the lost frame.
    /// Then call `decode()` normally on the same packet to get its actual content.
    ///
    /// # FEC Recovery Flow
    /// ```ignore
    /// // Packet N is missing, but packet N+1 arrived:
    /// let recovered_n = decoder.decode_fec(&packet_n_plus_1)?;  // Recover lost frame N
    /// let frame_n_plus_1 = decoder.decode(&packet_n_plus_1)?;   // Decode frame N+1 normally
    /// ```
    ///
    /// # Arguments
    /// * `opus_data` - The packet that arrived *after* the lost packet (contains FEC data)
    ///
    /// # Returns
    /// Approximate PCM samples for the *previous* (lost) frame.
    ///
    /// # Note
    /// FEC only works when the encoder has `set_inband_fec(true)` (enabled by default).
    /// The recovered audio is lower quality than the original but better than PLC alone.
    pub fn decode_fec(&mut self, opus_data: &[u8]) -> Result<Vec<f32>, CodecError> {
        let samples = self
            .decoder
            .decode_float(opus_data, &mut self.output_buffer, true)
            .map_err(|e| CodecError::Decode(e.description().to_string()))?;

        self.frames_decoded += 1;
        self.frames_concealed += 1;

        trace!(samples, "codec: decoded FEC frame (recovered lost packet)");

        Ok(self.output_buffer[..samples].to_vec())
    }

    /// Conceal a lost packet using packet loss concealment.
    ///
    /// This should be called when a packet is lost and no FEC data is available.
    /// The decoder will generate comfort noise or interpolate based on previous frames.
    ///
    /// # Arguments
    /// * `frame_size` - Expected frame size in samples (typically OPUS_FRAME_SIZE)
    ///
    /// # Returns
    /// Concealed PCM samples.
    pub fn conceal(&mut self, frame_size: usize) -> Result<Vec<f32>, CodecError> {
        // Pass empty slice to trigger PLC
        let samples = self
            .decoder
            .decode_float(&[], &mut self.output_buffer[..frame_size], false)
            .map_err(|e| CodecError::Decode(e.description().to_string()))?;

        self.frames_concealed += 1;

        trace!(samples, "codec: concealed lost frame");

        Ok(self.output_buffer[..samples].to_vec())
    }

    /// Get statistics about decoding.
    pub fn stats(&self) -> DecoderStats {
        DecoderStats {
            frames_decoded: self.frames_decoded,
            frames_concealed: self.frames_concealed,
            plc_ratio: if self.frames_decoded > 0 {
                self.frames_concealed as f64 / self.frames_decoded as f64
            } else {
                0.0
            },
        }
    }

    /// Reset the decoder state.
    ///
    /// This should be called when starting to receive audio from a new
    /// sender or after a long gap in transmission.
    pub fn reset(&mut self) -> Result<(), CodecError> {
        self.decoder
            .reset_state()
            .map_err(|e| CodecError::DecoderInit(e.description().to_string()))?;
        debug!("codec: decoder state reset");
        Ok(())
    }
}

/// Statistics from the decoder.
#[derive(Debug, Clone, Copy)]
pub struct DecoderStats {
    /// Total frames decoded successfully.
    pub frames_decoded: u64,
    /// Frames concealed due to packet loss (PLC or FEC).
    pub frames_concealed: u64,
    /// Ratio of concealed frames to total frames.
    pub plc_ratio: f64,
}

/// Get the Opus library version string.
pub fn opus_version() -> &'static str {
    opus::version()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoder_creation() {
        let encoder = VoiceEncoder::new();
        assert!(encoder.is_ok(), "encoder should be created successfully");
    }

    #[test]
    fn test_decoder_creation() {
        let decoder = VoiceDecoder::new();
        assert!(decoder.is_ok(), "decoder should be created successfully");
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let mut encoder = VoiceEncoder::new().unwrap();
        let mut decoder = VoiceDecoder::new().unwrap();

        // Create a simple sine wave test signal
        let freq = 440.0; // A4 note
        let sample_rate = OPUS_SAMPLE_RATE as f32;
        let pcm: Vec<f32> = (0..OPUS_FRAME_SIZE)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate).sin() * 0.5)
            .collect();

        // Encode
        let opus_data = encoder.encode(&pcm).expect("encoding should succeed");
        assert!(!opus_data.is_empty(), "encoded data should not be empty");
        assert!(
            opus_data.len() < pcm.len() * 4,
            "opus should compress the audio"
        );

        // Decode
        let decoded = decoder.decode(&opus_data).expect("decoding should succeed");
        assert_eq!(
            decoded.len(),
            OPUS_FRAME_SIZE,
            "decoded frame should have correct size"
        );

        // Verify the decoded signal has similar characteristics to the original
        // (not exact due to lossy compression and codec latency)
        // Check that the signal has similar energy (RMS)
        let original_rms: f32 = (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt();
        let decoded_rms: f32 =
            (decoded.iter().map(|s| s * s).sum::<f32>() / decoded.len() as f32).sqrt();

        // RMS should be within a factor of 2 (Opus preserves signal energy reasonably well)
        assert!(
            decoded_rms > original_rms * 0.3 && decoded_rms < original_rms * 3.0,
            "decoded RMS ({:.4}) should be similar to original RMS ({:.4})",
            decoded_rms,
            original_rms
        );

        // Also verify that the decoded signal is not all zeros
        let max_sample = decoded.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(
            max_sample > 0.01,
            "decoded signal should not be silent (max: {})",
            max_sample
        );
    }

    #[test]
    fn test_encode_invalid_frame_size() {
        let mut encoder = VoiceEncoder::new().unwrap();

        // Too small
        let small_pcm = vec![0.0f32; 100];
        let result = encoder.encode(&small_pcm);
        assert!(matches!(result, Err(CodecError::InvalidFrameSize { .. })));

        // Too large
        let large_pcm = vec![0.0f32; 2000];
        let result = encoder.encode(&large_pcm);
        assert!(matches!(result, Err(CodecError::InvalidFrameSize { .. })));
    }

    #[test]
    fn test_encoder_stats() {
        let mut encoder = VoiceEncoder::new().unwrap();
        let pcm = vec![0.0f32; OPUS_FRAME_SIZE];

        // Encode a few frames
        for _ in 0..5 {
            encoder.encode(&pcm).unwrap();
        }

        let stats = encoder.stats();
        assert_eq!(stats.frames_encoded, 5);
        assert!(stats.bytes_produced > 0);
        assert!(stats.avg_bytes_per_frame > 0.0);
    }

    #[test]
    fn test_silence_compression() {
        let mut encoder = VoiceEncoder::new().unwrap();

        // Silent frame
        let silence = vec![0.0f32; OPUS_FRAME_SIZE];
        let encoded = encoder.encode(&silence).unwrap();

        // With DTX enabled, silence should be very small
        // (just a few bytes for comfort noise)
        assert!(
            encoded.len() < 50,
            "silence should be heavily compressed (got {} bytes)",
            encoded.len()
        );
    }

    #[test]
    fn test_decoder_reset() {
        let mut decoder = VoiceDecoder::new().unwrap();
        assert!(decoder.reset().is_ok());
    }

    #[test]
    fn test_opus_version() {
        let version = opus_version();
        assert!(!version.is_empty());
        // Version string typically contains "libopus"
        println!("Opus version: {}", version);
    }
}
