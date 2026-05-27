//! Opus voice codec implementation.
//!
//! Wraps the `opus` crate to provide [`NativeOpusCodec`], [`NativeOpusEncoder`],
//! and [`NativeOpusDecoder`] implementing the [`rumble_client_traits::codec`] traits.

use anyhow::Context;
use opus::{Application, Bitrate, Channels, Decoder, Encoder};
use rumble_client_traits::codec::{EncoderSettings, OPUS_SAMPLE_RATE, VoiceCodec, VoiceDecoder, VoiceEncoder};
use tracing::debug;

/// Number of channels (mono for voice).
const OPUS_CHANNELS: Channels = Channels::Mono;

/// Opus voice codec factory (zero-sized type).
pub struct NativeOpusCodec;

impl VoiceCodec for NativeOpusCodec {
    type Encoder = NativeOpusEncoder;
    type Decoder = NativeOpusDecoder;

    fn create_encoder(settings: &EncoderSettings) -> anyhow::Result<Self::Encoder> {
        NativeOpusEncoder::new(settings)
    }

    fn create_decoder() -> anyhow::Result<Self::Decoder> {
        NativeOpusDecoder::new()
    }
}

/// Opus encoder wrapper.
pub struct NativeOpusEncoder {
    encoder: Encoder,
}

impl NativeOpusEncoder {
    /// Create a new encoder with the given settings.
    fn new(settings: &EncoderSettings) -> anyhow::Result<Self> {
        let mut encoder = Encoder::new(OPUS_SAMPLE_RATE, OPUS_CHANNELS, Application::Voip)
            .map_err(|e| anyhow::anyhow!("encoder init failed: {}", e.description()))?;

        Self::configure(&mut encoder, settings)?;

        debug!(
            bitrate = settings.bitrate,
            complexity = settings.complexity,
            fec = settings.fec_enabled,
            "native codec: encoder initialized"
        );

        Ok(Self { encoder })
    }

    /// Apply settings to the underlying Opus encoder.
    fn configure(encoder: &mut Encoder, settings: &EncoderSettings) -> anyhow::Result<()> {
        encoder
            .set_bitrate(Bitrate::Bits(settings.bitrate))
            .map_err(|e| anyhow::anyhow!("set bitrate: {}", e.description()))?;
        encoder
            .set_complexity(settings.complexity)
            .map_err(|e| anyhow::anyhow!("set complexity: {}", e.description()))?;
        encoder
            .set_vbr(settings.vbr_enabled)
            .map_err(|e| anyhow::anyhow!("set vbr: {}", e.description()))?;
        encoder
            .set_dtx(settings.dtx_enabled)
            .map_err(|e| anyhow::anyhow!("set dtx: {}", e.description()))?;
        encoder
            .set_inband_fec(settings.fec_enabled)
            .map_err(|e| anyhow::anyhow!("set fec: {}", e.description()))?;
        encoder
            .set_packet_loss_perc(settings.packet_loss_percent)
            .map_err(|e| anyhow::anyhow!("set packet loss: {}", e.description()))?;
        encoder
            .set_signal(opus::Signal::Voice)
            .map_err(|e| anyhow::anyhow!("set signal: {}", e.description()))?;
        Ok(())
    }
}

impl VoiceEncoder for NativeOpusEncoder {
    fn encode(&mut self, pcm: &[f32], output: &mut [u8]) -> anyhow::Result<usize> {
        let len = self.encoder.encode_float(pcm, output).context("opus encode failed")?;
        Ok(len)
    }

    fn apply_settings(&mut self, settings: &EncoderSettings) -> anyhow::Result<()> {
        Self::configure(&mut self.encoder, settings)?;
        debug!(
            bitrate = settings.bitrate,
            complexity = settings.complexity,
            fec = settings.fec_enabled,
            "native codec: encoder settings updated"
        );
        Ok(())
    }
}

/// Opus decoder wrapper.
pub struct NativeOpusDecoder {
    decoder: Decoder,
}

impl NativeOpusDecoder {
    /// Create a new decoder at 48kHz mono.
    fn new() -> anyhow::Result<Self> {
        let decoder = Decoder::new(OPUS_SAMPLE_RATE, OPUS_CHANNELS)
            .map_err(|e| anyhow::anyhow!("decoder init failed: {}", e.description()))?;

        debug!("native codec: decoder initialized");

        Ok(Self { decoder })
    }
}

impl VoiceDecoder for NativeOpusDecoder {
    fn decode(&mut self, data: &[u8], output: &mut [f32]) -> anyhow::Result<usize> {
        let samples = self
            .decoder
            .decode_float(data, output, false)
            .context("opus decode failed")?;
        Ok(samples)
    }

    fn decode_plc(&mut self, output: &mut [f32]) -> anyhow::Result<usize> {
        let samples = self
            .decoder
            .decode_float(&[], output, false)
            .context("opus PLC decode failed")?;
        Ok(samples)
    }

    fn decode_fec(&mut self, data: &[u8], output: &mut [f32]) -> anyhow::Result<usize> {
        let samples = self
            .decoder
            .decode_float(data, output, true)
            .context("opus FEC decode failed")?;
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumble_client_traits::codec::OPUS_FRAME_SIZE;

    #[test]
    fn create_encoder() {
        let settings = EncoderSettings::default();
        let encoder = NativeOpusCodec::create_encoder(&settings);
        assert!(encoder.is_ok());
    }

    #[test]
    fn create_decoder() {
        let decoder = NativeOpusCodec::create_decoder();
        assert!(decoder.is_ok());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let settings = EncoderSettings::default();
        let mut encoder = NativeOpusCodec::create_encoder(&settings).unwrap();
        let mut decoder = NativeOpusCodec::create_decoder().unwrap();

        // Generate a sine wave
        let freq = 440.0f32;
        let sr = OPUS_SAMPLE_RATE as f32;
        let pcm: Vec<f32> = (0..OPUS_FRAME_SIZE)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin() * 0.5)
            .collect();

        let mut opus_buf = vec![0u8; 4000];
        let len = encoder.encode(&pcm, &mut opus_buf).unwrap();
        assert!(len > 0);

        let mut decoded = vec![0.0f32; OPUS_FRAME_SIZE];
        let samples = decoder.decode(&opus_buf[..len], &mut decoded).unwrap();
        assert_eq!(samples, OPUS_FRAME_SIZE);

        // Verify signal is not silence
        let max = decoded.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(max > 0.01, "decoded signal should not be silent (max: {})", max);
    }

    #[test]
    fn apply_settings() {
        let settings = EncoderSettings::default();
        let mut encoder = NativeOpusCodec::create_encoder(&settings).unwrap();

        let new_settings = EncoderSettings {
            bitrate: 32000,
            complexity: 5,
            fec_enabled: false,
            packet_loss_percent: 0,
            dtx_enabled: false,
            vbr_enabled: false,
        };

        assert!(encoder.apply_settings(&new_settings).is_ok());
    }

    #[test]
    fn decode_plc() {
        let mut decoder = NativeOpusCodec::create_decoder().unwrap();
        let mut output = vec![0.0f32; OPUS_FRAME_SIZE];
        let samples = decoder.decode_plc(&mut output).unwrap();
        assert_eq!(samples, OPUS_FRAME_SIZE);
    }
}
