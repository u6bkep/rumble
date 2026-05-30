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

    // These are characterization tests for the Opus wiring. They pin the
    // codec's *observable* behavior — a full-frame count, a faithfully
    // reproduced tone in steady state, well-formed (finite, unclipped) output
    // on every path — robustly enough to survive an opus-rs version bump, but
    // tightly enough to catch a buffer-handling regression in the surrounding
    // code (wrong slice, reused/stale buffer, channel mishandling, silence).

    /// Single-bin Goertzel power: the energy of `samples` at `freq` Hz.
    fn goertzel_power(samples: &[f32], freq: f32, sample_rate: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in samples {
            let s = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        s1 * s1 + s2 * s2 - coeff * s1 * s2
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// One 20 ms frame of a continuous sine, phase-continuous from `start_sample`.
    fn sine_frame(freq: f32, start_sample: usize, amp: f32) -> Vec<f32> {
        let sr = OPUS_SAMPLE_RATE as f32;
        (0..OPUS_FRAME_SIZE)
            .map(|i| (2.0 * std::f32::consts::PI * freq * (start_sample + i) as f32 / sr).sin() * amp)
            .collect()
    }

    fn well_formed(frame: &[f32]) -> bool {
        frame.len() == OPUS_FRAME_SIZE && frame.iter().all(|s| s.is_finite() && s.abs() <= 1.0)
    }

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
    fn encode_decode_roundtrip_preserves_tone() {
        let settings = EncoderSettings::default();
        let mut encoder = NativeOpusCodec::create_encoder(&settings).unwrap();
        let mut decoder = NativeOpusCodec::create_decoder().unwrap();

        // A single-frame round-trip is dominated by Opus's algorithmic delay,
        // so push a continuous tone through several frames and characterize a
        // steady-state frame once the codec has warmed up.
        let freq = 440.0f32;
        let amp = 0.5f32;
        const FRAMES: usize = 12;

        let mut last = vec![0.0f32; OPUS_FRAME_SIZE];
        for f in 0..FRAMES {
            let pcm = sine_frame(freq, f * OPUS_FRAME_SIZE, amp);
            let mut opus_buf = vec![0u8; 4000];
            let len = encoder.encode(&pcm, &mut opus_buf).unwrap();
            assert!(len > 0, "encoder must emit a packet");

            let mut decoded = vec![0.0f32; OPUS_FRAME_SIZE];
            let n = decoder.decode(&opus_buf[..len], &mut decoded).unwrap();
            assert_eq!(n, OPUS_FRAME_SIZE, "decode must yield a full 20ms frame");
            last = decoded;
        }

        // Steady-state frame: well-formed, the right tone, the right loudness.
        assert!(well_formed(&last), "decoded samples must be finite and unclipped");

        let sr = OPUS_SAMPLE_RATE as f32;
        let on = goertzel_power(&last, freq, sr);
        let off = goertzel_power(&last, freq * 3.0, sr); // 1320 Hz: not present
        assert!(
            on > off * 20.0,
            "energy at {freq}Hz ({on}) should dominate an absent band ({off})"
        );

        // RMS of a full sine is amp/sqrt(2); allow a wide band for lossy coding.
        let expected = amp / std::f32::consts::SQRT_2;
        let r = rms(&last);
        assert!(
            r > expected * 0.5 && r < expected * 1.5,
            "decoded RMS {r} should be near {expected} (silence or gain bug otherwise)"
        );
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
    fn decode_plc_produces_wellformed_frame() {
        let settings = EncoderSettings::default();
        let mut encoder = NativeOpusCodec::create_encoder(&settings).unwrap();
        let mut decoder = NativeOpusCodec::create_decoder().unwrap();

        // Prime the decoder with real audio, then conceal a lost frame.
        for f in 0..3 {
            let pcm = sine_frame(440.0, f * OPUS_FRAME_SIZE, 0.5);
            let mut buf = vec![0u8; 4000];
            let len = encoder.encode(&pcm, &mut buf).unwrap();
            let mut out = vec![0.0f32; OPUS_FRAME_SIZE];
            decoder.decode(&buf[..len], &mut out).unwrap();
        }

        let mut plc = vec![0.0f32; OPUS_FRAME_SIZE];
        let n = decoder.decode_plc(&mut plc).unwrap();
        assert_eq!(n, OPUS_FRAME_SIZE, "PLC must emit a full frame");
        assert!(well_formed(&plc), "concealed frame must be finite and unclipped");
    }

    #[test]
    fn decode_fec_recovers_wellformed_frame() {
        // In-band FEC must be enabled (with a non-zero expected loss) for the
        // encoder to embed redundancy that the next packet can recover.
        let settings = EncoderSettings {
            fec_enabled: true,
            packet_loss_percent: 20,
            ..EncoderSettings::default()
        };
        let mut encoder = NativeOpusCodec::create_encoder(&settings).unwrap();
        let mut decoder = NativeOpusCodec::create_decoder().unwrap();

        // Encode a continuous tone and capture each packet.
        let mut packets = Vec::new();
        for f in 0..4 {
            let pcm = sine_frame(440.0, f * OPUS_FRAME_SIZE, 0.5);
            let mut buf = vec![0u8; 4000];
            let len = encoder.encode(&pcm, &mut buf).unwrap();
            packets.push(buf[..len].to_vec());
        }

        // Decode 0 and 1 normally, "lose" 2, recover it from 3's in-band FEC.
        let mut out = vec![0.0f32; OPUS_FRAME_SIZE];
        decoder.decode(&packets[0], &mut out).unwrap();
        decoder.decode(&packets[1], &mut out).unwrap();

        let mut fec = vec![0.0f32; OPUS_FRAME_SIZE];
        let n = decoder.decode_fec(&packets[3], &mut fec).unwrap();
        assert_eq!(n, OPUS_FRAME_SIZE, "FEC recovery must emit a full frame");
        assert!(well_formed(&fec), "recovered frame must be finite and unclipped");
    }
}
