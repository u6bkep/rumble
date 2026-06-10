//! Gain processor for volume adjustment.
//!
//! This is a simple processor that applies a gain (volume adjustment) to
//! audio samples. It's commonly used for per-user volume control on the
//! receive path.

use rumble_audio::{AudioProcessor, ProcessorFactory, ProcessorResult, db_to_linear, soft_clip};
use serde::{Deserialize, Serialize};

use super::type_ids;

/// Settings for the gain processor.
///
/// `Default::default()` must match the schema defaults in
/// [`GainProcessorFactory::settings_schema`]; persisted configs are
/// backfilled against the schema before reaching this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GainSettings {
    /// Gain in decibels. 0 = unity, negative = quieter, positive = louder.
    pub gain_db: f32,
}

impl Default for GainSettings {
    fn default() -> Self {
        Self { gain_db: 0.0 }
    }
}

/// A simple gain (volume) processor.
///
/// Applies a constant gain to all samples. Useful for per-user volume
/// adjustment on the receive path.
pub struct GainProcessor {
    settings: GainSettings,
    /// Pre-computed linear gain factor
    gain_linear: f32,
    enabled: bool,
}

impl GainProcessor {
    /// Create a new gain processor with the specified gain in dB.
    pub fn new(gain_db: f32) -> Self {
        Self {
            settings: GainSettings { gain_db },
            gain_linear: db_to_linear(gain_db),
            enabled: true,
        }
    }

    /// Get the current gain in dB.
    pub fn gain_db(&self) -> f32 {
        self.settings.gain_db
    }

    /// Set the gain in dB.
    pub fn set_gain_db(&mut self, gain_db: f32) {
        self.settings.gain_db = gain_db;
        self.gain_linear = db_to_linear(gain_db);
    }
}

impl Default for GainProcessor {
    fn default() -> Self {
        Self::new(0.0)
    }
}

impl AudioProcessor for GainProcessor {
    fn process(&mut self, samples: &mut [f32], _sample_rate: u32) -> ProcessorResult {
        // Unity gain (the default) is a true no-op.
        if self.gain_linear == 1.0 {
            return ProcessorResult::pass();
        }

        // Only a boost can push an in-range signal past full scale, so only
        // boosts are limited. Soft-knee limit the boosted signal (transparent
        // below the knee, smooth compression above): the old code hard-clamped
        // to ±1.0 while claiming to soft clip, squaring off anything a
        // positive gain drove past full scale (harsh distortion). Attenuation
        // passes through unlimited — downstream (the playback mix limiter on
        // RX; the float pipeline on TX) tolerates the rare >1.0 sample.
        let limit = self.gain_linear > 1.0;
        for sample in samples.iter_mut() {
            *sample *= self.gain_linear;
            if limit {
                *sample = soft_clip(*sample);
            }
        }

        ProcessorResult::pass()
    }

    fn name(&self) -> &'static str {
        "Gain"
    }

    fn reset(&mut self) {
        // No state to reset
    }

    fn config(&self) -> serde_json::Value {
        serde_json::to_value(&self.settings).unwrap_or_default()
    }

    fn set_config(&mut self, config: &serde_json::Value) {
        if let Ok(settings) = serde_json::from_value::<GainSettings>(config.clone()) {
            self.settings = settings;
            self.gain_linear = db_to_linear(self.settings.gain_db);
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

/// Factory for creating GainProcessor instances.
pub struct GainProcessorFactory;

impl ProcessorFactory for GainProcessorFactory {
    fn type_id(&self) -> &'static str {
        type_ids::GAIN
    }

    fn display_name(&self) -> &'static str {
        "Gain"
    }

    fn create_default(&self) -> Box<dyn AudioProcessor> {
        Box::new(GainProcessor::default())
    }

    fn create_from_config(&self, config: &serde_json::Value) -> Result<Box<dyn AudioProcessor>, String> {
        let settings: GainSettings =
            serde_json::from_value(config.clone()).map_err(|e| format!("Invalid gain settings: {}", e))?;

        Ok(Box::new(GainProcessor::new(settings.gain_db)))
    }

    fn settings_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "gain_db": {
                    "type": "number",
                    "title": "Gain (dB)",
                    "description": "Volume adjustment in decibels. 0 = no change, negative = quieter, positive = louder.",
                    "default": 0.0,
                    "minimum": -60.0,
                    "maximum": 20.0
                }
            }
        })
    }

    fn description(&self) -> &'static str {
        "Adjusts audio volume by a fixed amount in decibels"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gain_unity() {
        let mut processor = GainProcessor::new(0.0);
        let mut samples = vec![0.5, -0.5, 1.0, -1.0];
        let original = samples.clone();

        processor.process(&mut samples, 48000);

        for (a, b) in samples.iter().zip(original.iter()) {
            assert!((a - b).abs() < 0.0001);
        }
    }

    #[test]
    fn test_gain_minus_6db() {
        let mut processor = GainProcessor::new(-6.0);
        let mut samples = vec![1.0, -1.0];

        processor.process(&mut samples, 48000);

        // -6dB is approximately 0.5 linear
        assert!((samples[0] - 0.501).abs() < 0.01);
        assert!((samples[1] - (-0.501)).abs() < 0.01);
    }

    #[test]
    fn test_gain_soft_clips_overshoot() {
        // New pinned behavior: a boost past full scale is SOFT-limited (the
        // old code hard-clamped to exactly 1.0, despite its "soft clip"
        // comment — a square-wave edge). The result must approach but never
        // reach full scale, and must match the shared limiter curve.
        let mut processor = GainProcessor::new(20.0); // +20dB = 10x
        let mut samples = vec![0.5];

        processor.process(&mut samples, 48000);

        assert!((samples[0] - soft_clip(5.0)).abs() < 1e-6);
        assert!(samples[0] < 1.0, "soft limiter must stay below full scale");
        assert!(samples[0] > 0.9, "overshoot should still land near full scale");
    }

    #[test]
    fn test_gain_attenuation_does_not_limit() {
        // Attenuation can't overshoot; peaks above the limiter knee must pass
        // through untouched rather than being compressed.
        let mut processor = GainProcessor::new(-1.0);
        let gain = db_to_linear(-1.0);
        let mut samples = vec![0.95f32];
        processor.process(&mut samples, 48000);
        assert!((samples[0] - 0.95 * gain).abs() < 1e-6);
    }

    #[test]
    fn test_gain_config_roundtrip() {
        let processor = GainProcessor::new(-12.0);
        let config = processor.config();

        let mut processor2 = GainProcessor::default();
        processor2.set_config(&config);

        assert!((processor2.gain_db() - (-12.0)).abs() < 0.001);
    }

    #[test]
    fn test_factory_create() {
        let factory = GainProcessorFactory;
        let config = serde_json::json!({ "gain_db": -3.0 });

        let processor = factory.create_from_config(&config).unwrap();
        assert_eq!(processor.name(), "Gain");
    }
}
