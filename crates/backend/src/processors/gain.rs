//! Gain processor for volume adjustment.
//!
//! This is a simple processor that applies a gain (volume adjustment) to
//! audio samples. It's commonly used for per-user volume control on the
//! receive path.

use pipeline::{AudioProcessor, ProcessorFactory, ProcessorResult, db_to_linear};
use serde::{Deserialize, Serialize};

use super::type_ids;

/// Settings for the gain processor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GainSettings {
    /// Gain in decibels. 0 = unity, negative = quieter, positive = louder.
    #[serde(default)]
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
        // Apply gain to all samples
        for sample in samples.iter_mut() {
            *sample *= self.gain_linear;
            // Soft clip to prevent harsh distortion
            *sample = sample.clamp(-1.0, 1.0);
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
    fn test_gain_clipping() {
        let mut processor = GainProcessor::new(20.0); // +20dB = 10x
        let mut samples = vec![0.5];

        processor.process(&mut samples, 48000);

        // Should clip to 1.0
        assert_eq!(samples[0], 1.0);
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
