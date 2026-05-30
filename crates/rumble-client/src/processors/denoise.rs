//! RNNoise-based noise suppression and voice gating.
//!
//! Uses the nnnoiseless port of RNNoise to do two things in a single
//! forward pass per 10 ms chunk: produce a denoised audio frame and a
//! voice-probability score. The denoised samples optionally replace the
//! input; the probability optionally drives the [`VadShaper`] for
//! voice-activated transmission.
//!
//! Why one processor for both: RNNoise computes the VAD score from the
//! same internal features it uses to derive the denoise gains — the two
//! were never independent. Bundling them lets us pay for inference once
//! and gives users a single "speech cleanup" stage instead of two whose
//! ordering and pairing they'd otherwise have to reason about.
//!
//! Note: RNNoise operates on 10 ms frames (480 samples at 48 kHz), so
//! the processor buffers internally to handle arbitrary outer frame
//! sizes. The very first 10 ms output is muted to hide fade-in artifacts.

use nnnoiseless::DenoiseState;
use rumble_audio::{AudioProcessor, ProcessorFactory, ProcessorResult};
use serde::{Deserialize, Serialize};

use super::{
    type_ids,
    vad_shaper::{VadShaper, VadShaperSettings},
};

/// Frame size used by nnnoiseless (480 samples = 10 ms at 48 kHz).
const DENOISE_FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;

/// Settings for the denoise/voice-gate processor.
///
/// Field defaults live in `Default::default()` and **must match** the
/// `default` values in [`DenoiseProcessorFactory::settings_schema`] —
/// the schema is the single source of truth, and persisted configs are
/// backfilled against it (see `ProcessorRegistry::backfill_settings`)
/// before they reach this struct. The struct-level `#[serde(default)]`
/// is a defensive belt-and-braces fallback for the create-from-raw-JSON
/// paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DenoiseSettings {
    /// When true, replace the input audio with the denoised output.
    /// When false, samples pass through untouched but inference still
    /// runs if `vad_enabled` is true.
    pub denoise_enabled: bool,

    /// When true, drive a `suppress` decision off the RNNoise VAD
    /// probability.
    pub vad_enabled: bool,

    /// Voice probability above which transmission starts (0..1).
    pub vad_trigger: f32,

    /// Voice probability the score must drop below — together with
    /// holdoff expiring — for transmission to stop. Set lower than
    /// `vad_trigger` for hysteresis.
    pub vad_release: f32,

    /// Minimum sustained-over-trigger duration before activating.
    /// 0 = activate on first chunk above trigger.
    pub vad_attack_ms: u32,

    /// After dropping below release, continue transmitting for this
    /// long. Prevents cutting off word endings.
    pub vad_holdoff_ms: u32,
}

impl Default for DenoiseSettings {
    fn default() -> Self {
        Self {
            denoise_enabled: true,
            vad_enabled: true,
            vad_trigger: 0.5,
            vad_release: 0.35,
            vad_attack_ms: 0,
            vad_holdoff_ms: 300,
        }
    }
}

/// RNNoise denoiser + ML voice gate.
pub struct DenoiseProcessor {
    denoise_state: Box<DenoiseState<'static>>,
    /// Buffer of input samples waiting to be assembled into 10 ms chunks.
    input_buffer: Vec<f32>,
    /// Buffer of denoised samples waiting to be copied back to the caller.
    output_buffer: Vec<f32>,
    /// True once the first chunk has run; that chunk's output is
    /// substituted with silence to mask RNNoise fade-in artifacts.
    first_frame_done: bool,
    /// Cached VAD probability from the most-recent processed chunk.
    /// Surfaces via [`DenoiseProcessor::last_vad_probability`] for UI.
    last_vad_prob: f32,
    settings: DenoiseSettings,
    shaper: VadShaper,
    enabled: bool,
}

impl DenoiseProcessor {
    pub fn new() -> Self {
        Self::with_settings(DenoiseSettings::default())
    }

    pub fn with_settings(settings: DenoiseSettings) -> Self {
        Self {
            denoise_state: DenoiseState::new(),
            input_buffer: Vec::with_capacity(DENOISE_FRAME_SIZE * 2),
            output_buffer: Vec::with_capacity(DENOISE_FRAME_SIZE * 4),
            first_frame_done: false,
            last_vad_prob: 0.0,
            settings,
            shaper: VadShaper::new(),
            enabled: true,
        }
    }

    /// VAD probability from the most-recent 10 ms chunk processed.
    /// Useful for surfacing a live probability bar in the UI.
    pub fn last_vad_probability(&self) -> f32 {
        self.last_vad_prob
    }

    /// Whether the voice gate currently considers the speaker active.
    /// Only meaningful when `vad_enabled` is true.
    pub fn is_voice_active(&self) -> bool {
        self.shaper.is_active()
    }

    fn shaper_settings(&self) -> VadShaperSettings {
        VadShaperSettings {
            trigger: self.settings.vad_trigger,
            release: self.settings.vad_release,
            attack_ms: self.settings.vad_attack_ms,
            holdoff_ms: self.settings.vad_holdoff_ms,
        }
    }
}

impl Default for DenoiseProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioProcessor for DenoiseProcessor {
    fn process(&mut self, samples: &mut [f32], sample_rate: u32) -> ProcessorResult {
        let want_denoise = self.settings.denoise_enabled;
        let want_vad = self.settings.vad_enabled;

        // If neither output is wanted, skip inference and the buffering
        // dance entirely — cheap fast path for "stage enabled but doing
        // nothing." Returns pass through unchanged samples.
        if !want_denoise && !want_vad {
            return ProcessorResult::pass();
        }

        // Snapshot the pre-denoise input for the pass-through case
        // before the output_buffer drain overwrites samples. Only used
        // when we run inference for VAD but don't want denoised audio.
        let original: Option<Vec<f32>> = if !want_denoise { Some(samples.to_vec()) } else { None };

        self.input_buffer.extend_from_slice(samples);

        let shaper_settings = self.shaper_settings();

        while self.input_buffer.len() >= DENOISE_FRAME_SIZE {
            // Scale the leading 10 ms chunk into a stack scratch (RNNoise wants
            // i16-range floats), run inference, then drop the consumed input.
            // Stack arrays here avoid two heap allocs per chunk (2 chunks per
            // 20 ms frame) on the always-on capture path.
            let mut scaled = [0.0f32; DENOISE_FRAME_SIZE];
            for (dst, &src) in scaled.iter_mut().zip(&self.input_buffer[..DENOISE_FRAME_SIZE]) {
                *dst = src * 32767.0;
            }
            self.input_buffer.drain(..DENOISE_FRAME_SIZE);

            let mut out_buf = [0.0f32; DENOISE_FRAME_SIZE];
            let vad_prob = self.denoise_state.process_frame(&mut out_buf, &scaled);
            self.last_vad_prob = vad_prob;

            if want_vad {
                self.shaper
                    .update(vad_prob, DENOISE_FRAME_SIZE as u32, sample_rate, &shaper_settings);
            }

            if !self.first_frame_done {
                self.first_frame_done = true;
                self.output_buffer.extend(std::iter::repeat_n(0.0, DENOISE_FRAME_SIZE));
                continue;
            }

            for &sample in &out_buf {
                self.output_buffer.push(sample / 32767.0);
            }
        }

        // Drain the denoised buffer back into the caller's slice. We always
        // do this so the buffer doesn't grow unboundedly; if the caller
        // wanted pass-through, we then overwrite with the original input.
        let copy_len = samples.len().min(self.output_buffer.len());
        if copy_len > 0 {
            samples[..copy_len].copy_from_slice(&self.output_buffer[..copy_len]);
            self.output_buffer.drain(..copy_len);
        }
        if copy_len < samples.len() {
            samples[copy_len..].fill(0.0);
        }

        if let Some(original) = original {
            // VAD-only mode: restore the unmodified input. Inference still
            // ran above so the shaper gets updated and the buffers stay
            // healthy if denoise is toggled back on later.
            samples.copy_from_slice(&original);
        }

        ProcessorResult {
            suppress: want_vad && !self.shaper.is_active(),
        }
    }

    fn name(&self) -> &'static str {
        "Denoise + Voice Gate (RNNoise)"
    }

    fn reset(&mut self) {
        self.denoise_state = DenoiseState::new();
        self.input_buffer.clear();
        self.output_buffer.clear();
        self.first_frame_done = false;
        self.last_vad_prob = 0.0;
        self.shaper.reset();
    }

    fn config(&self) -> serde_json::Value {
        serde_json::to_value(&self.settings).unwrap_or_default()
    }

    fn set_config(&mut self, config: &serde_json::Value) {
        if let Ok(settings) = serde_json::from_value::<DenoiseSettings>(config.clone()) {
            // Flipping the gate must clear the shaper's attack/holdoff
            // counters — otherwise stale state from the previous mode
            // can mis-gate the first frame after the toggle.
            if settings.vad_enabled != self.settings.vad_enabled {
                self.shaper.reset();
            }
            self.settings = settings;
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if enabled {
            self.reset();
        }
    }
}

/// Factory for creating [`DenoiseProcessor`] instances.
pub struct DenoiseProcessorFactory;

impl ProcessorFactory for DenoiseProcessorFactory {
    fn type_id(&self) -> &'static str {
        type_ids::DENOISE
    }

    fn display_name(&self) -> &'static str {
        "Denoise + Voice Gate (RNNoise)"
    }

    fn create_default(&self) -> Box<dyn AudioProcessor> {
        Box::new(DenoiseProcessor::new())
    }

    fn create_from_config(&self, config: &serde_json::Value) -> Result<Box<dyn AudioProcessor>, String> {
        let settings: DenoiseSettings = serde_json::from_value(config.clone()).unwrap_or_default();
        Ok(Box::new(DenoiseProcessor::with_settings(settings)))
    }

    fn settings_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "denoise_enabled": {
                    "type": "boolean",
                    "title": "Apply noise suppression",
                    "description": "Replace the input audio with the denoised output.",
                    "default": true
                },
                "vad_enabled": {
                    "type": "boolean",
                    "title": "Gate transmission on voice",
                    "description": "Use the RNNoise voice probability to suppress silence and non-speech.",
                    "default": true
                },
                "vad_trigger": {
                    "type": "number",
                    "title": "Voice trigger (probability)",
                    "description": "Voice probability above which transmission starts.",
                    "default": 0.5,
                    "minimum": 0.0,
                    "maximum": 1.0
                },
                "vad_release": {
                    "type": "number",
                    "title": "Voice release (probability)",
                    "description": "Once active, stay active until probability drops below this. Set lower than the trigger for hysteresis.",
                    "default": 0.35,
                    "minimum": 0.0,
                    "maximum": 1.0
                },
                "vad_attack_ms": {
                    "type": "integer",
                    "title": "Voice attack (ms)",
                    "description": "Probability must stay above trigger for this long before activating.",
                    "default": 0,
                    "minimum": 0,
                    "maximum": 200
                },
                "vad_holdoff_ms": {
                    "type": "integer",
                    "title": "Voice holdoff (ms)",
                    "description": "Keep transmitting for this long after the probability drops below release.",
                    "default": 300,
                    "minimum": 0,
                    "maximum": 2000
                }
            }
        })
    }

    fn description(&self) -> &'static str {
        "RNNoise neural-network noise suppression with an integrated voice-activity gate"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_denoise_creation() {
        let processor = DenoiseProcessor::new();
        assert!(processor.is_enabled());
        assert_eq!(processor.name(), "Denoise + Voice Gate (RNNoise)");
    }

    #[test]
    fn test_denoise_process_single_frame() {
        let mut processor = DenoiseProcessor::with_settings(DenoiseSettings {
            denoise_enabled: true,
            vad_enabled: false, // isolate denoise behavior from VAD gating
            ..DenoiseSettings::default()
        });

        let mut samples = vec![0.1f32; 960];
        let result = processor.process(&mut samples, 48000);

        assert!(!result.suppress);
        // First chunk gets zeroed by fade-in suppression.
        assert!(samples[..480].iter().all(|&s| s.abs() < 0.001));
    }

    #[test]
    fn test_denoise_pass_through_when_disabled() {
        // denoise off, VAD off → stage is a no-op, samples untouched
        // and no inference is run.
        let mut processor = DenoiseProcessor::with_settings(DenoiseSettings {
            denoise_enabled: false,
            vad_enabled: false,
            ..DenoiseSettings::default()
        });
        let original = vec![0.25f32; 960];
        let mut samples = original.clone();
        let result = processor.process(&mut samples, 48000);
        assert!(!result.suppress);
        assert_eq!(samples, original);
    }

    #[test]
    fn test_vad_only_preserves_input_audio() {
        // VAD on, denoise off → input audio passes through unchanged,
        // but inference still runs (we can observe last_vad_probability
        // gets populated).
        let mut processor = DenoiseProcessor::with_settings(DenoiseSettings {
            denoise_enabled: false,
            vad_enabled: true,
            ..DenoiseSettings::default()
        });
        let original: Vec<f32> = (0..960).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
        let mut samples = original.clone();
        processor.process(&mut samples, 48000);
        // Input bytes unchanged.
        for (a, b) in original.iter().zip(samples.iter()) {
            assert!((a - b).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn test_denoise_reset() {
        let mut processor = DenoiseProcessor::new();
        let mut samples = vec![0.5f32; 960];
        processor.process(&mut samples, 48000);
        processor.reset();
        assert!(processor.input_buffer.is_empty());
        assert!(processor.output_buffer.is_empty());
        assert!(!processor.first_frame_done);
        assert!(!processor.is_voice_active());
    }

    /// Legacy persisted configs (which carried only an empty `{}`) must
    /// pick up the schema defaults rather than some separate "legacy"
    /// values — otherwise the runtime's gate state and what the UI paints
    /// drift apart, because the UI reads schema defaults. Backfill at
    /// load time (see `ProcessorRegistry::backfill_settings` and
    /// `merge_with_default_tx_pipeline`) makes this true at the JSON
    /// layer; here we assert that the serde fallback agrees, since it's
    /// the last line of defense for code paths that bypass backfill.
    #[test]
    fn test_legacy_empty_config_uses_schema_defaults() {
        let settings: DenoiseSettings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(settings.denoise_enabled);
        assert!(settings.vad_enabled, "missing fields must fall back to schema defaults");
    }

    #[test]
    fn test_factory_default_enables_vad() {
        let factory = DenoiseProcessorFactory;
        let default_settings = factory.default_settings();
        let settings: DenoiseSettings = serde_json::from_value(default_settings).unwrap();
        assert!(settings.denoise_enabled);
        assert!(settings.vad_enabled, "fresh configs must default vad_enabled=true");
    }

    #[test]
    fn test_factory_create_from_legacy_config() {
        let factory = DenoiseProcessorFactory;
        let processor = factory.create_from_config(&serde_json::json!({})).unwrap();
        assert_eq!(processor.name(), "Denoise + Voice Gate (RNNoise)");
    }

    #[test]
    fn test_config_roundtrip() {
        let processor = DenoiseProcessor::with_settings(DenoiseSettings {
            denoise_enabled: false,
            vad_enabled: true,
            vad_trigger: 0.7,
            vad_release: 0.4,
            vad_attack_ms: 30,
            vad_holdoff_ms: 250,
        });
        let json = processor.config();
        let parsed: DenoiseSettings = serde_json::from_value(json).unwrap();
        assert!(!parsed.denoise_enabled);
        assert!(parsed.vad_enabled);
        assert!((parsed.vad_trigger - 0.7).abs() < 1e-6);
        assert!((parsed.vad_release - 0.4).abs() < 1e-6);
        assert_eq!(parsed.vad_attack_ms, 30);
        assert_eq!(parsed.vad_holdoff_ms, 250);
    }
}
