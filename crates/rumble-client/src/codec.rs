//! Opus codec constants and utilities for voice communication.
//!
//! The concrete `VoiceEncoder` / `VoiceDecoder` implementations have moved
//! to `rumble-desktop::codec` behind the `VoiceCodec` trait. This module
//! retains shared constants and small helpers used across the backend.

pub use crate::events::{
    DTX_FRAME_MAX_SIZE, EncoderSettings, OPUS_DEFAULT_BITRATE, OPUS_DEFAULT_COMPLEXITY, OPUS_DEFAULT_PACKET_LOSS_PERC,
    OPUS_FRAME_SIZE, OPUS_MAX_PACKET_SIZE, OPUS_SAMPLE_RATE,
};

/// Check if an encoded Opus frame is a DTX (discontinuous transmission) silence frame.
///
/// When DTX is enabled and the encoder detects silence, it produces very small
/// frames (typically 1-2 bytes). These can be skipped to save bandwidth, with
/// periodic keepalives to maintain the connection.
#[inline]
pub fn is_dtx_frame(encoded: &[u8]) -> bool {
    encoded.len() <= DTX_FRAME_MAX_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_dtx_frame() {
        // DTX frames are ≤2 bytes
        assert!(is_dtx_frame(&[]), "empty frame should be DTX");
        assert!(is_dtx_frame(&[0x00]), "1-byte frame should be DTX");
        assert!(is_dtx_frame(&[0x00, 0x01]), "2-byte frame should be DTX");
        assert!(!is_dtx_frame(&[0x00, 0x01, 0x02]), "3-byte frame should not be DTX");
        assert!(!is_dtx_frame(&[0x00; 100]), "100-byte frame should not be DTX");
    }
}
