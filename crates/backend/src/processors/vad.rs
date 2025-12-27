//! Voice Activity Detection (VAD) processor.
//!
//! This processor detects when voice is present in the audio stream and
//! sets the `suppress` flag when no voice is detected. When enabled in the
//! TX pipeline, it provides voice-activated transmission in Continuous mode.
//! It can also be used in PTT mode to avoid transmitting silence/noise.
//!
//! The current implementation uses energy-based detection with a configurable
//! threshold and holdoff timer to avoid cutting off speech endings.

use pipeline::{AudioProcessor, ProcessorFactory, ProcessorResult, calculate_rms_db};
use serde::{Deserialize, Serialize};

use super::type_ids;

/// Settings for the VAD processor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadSettings {
    /// Threshold in dB RMS. Frames below this are considered silence.
    /// Typical values: -40 to -30 dB for quiet environments,
    /// -50 to -40 dB for noisy environments.
    #[serde(default = "default_threshold_db")]
    pub threshold_db: f32,

    /// Holdoff time in milliseconds.
    /// After voice is detected, continue for this duration even if
    /// the signal drops below threshold. Prevents cutting off word endings.
    #[serde(default = "default_holdoff_ms")]
    pub holdoff_ms: u32,
}

fn default_threshold_db() -> f32 {
    -40.0
}

fn default_holdoff_ms() -> u32 {
    300
}

impl Default for VadSettings {
    fn default() -> Self {
        Self {
            threshold_db: default_threshold_db(),
            holdoff_ms: default_holdoff_ms(),
        }
    }
}

/// Voice Activity Detection processor.
///
/// Detects voice presence using energy-based analysis. When no voice is
/// detected, sets `suppress = true` in the result, signaling that the
/// frame should not be transmitted.
///
/// Features:
/// - Energy-based detection with configurable threshold
/// - Holdoff timer to avoid cutting off speech endings
/// - Smooth transitions to prevent abrupt start/stop
pub struct VadProcessor {
    settings: VadSettings,
    enabled: bool,

    // State
    /// Whether we're currently in "voice active" state
    voice_active: bool,
    /// Samples remaining in holdoff period (converted from ms at runtime)
    holdoff_samples_remaining: u32,
    /// Current sample rate (for holdoff calculation)
    current_sample_rate: u32,
}

impl VadProcessor {
    /// Create a new VAD processor with default settings.
    pub fn new() -> Self {
        Self::with_settings(VadSettings::default())
    }

    /// Create a new VAD processor with custom settings.
    pub fn with_settings(settings: VadSettings) -> Self {
        Self {
            settings,
            enabled: true,
            voice_active: false,
            holdoff_samples_remaining: 0,
            current_sample_rate: 48000,
        }
    }

    /// Get the current threshold in dB.
    pub fn threshold_db(&self) -> f32 {
        self.settings.threshold_db
    }

    /// Set the threshold in dB.
    pub fn set_threshold_db(&mut self, threshold_db: f32) {
        self.settings.threshold_db = threshold_db;
    }

    /// Get the holdoff time in milliseconds.
    pub fn holdoff_ms(&self) -> u32 {
        self.settings.holdoff_ms
    }

    /// Set the holdoff time in milliseconds.
    pub fn set_holdoff_ms(&mut self, holdoff_ms: u32) {
        self.settings.holdoff_ms = holdoff_ms;
    }

    /// Check if voice is currently detected as active.
    pub fn is_voice_active(&self) -> bool {
        self.voice_active
    }

    /// Convert holdoff_ms to samples at the given sample rate.
    fn holdoff_samples(&self, sample_rate: u32) -> u32 {
        (self.settings.holdoff_ms as u64 * sample_rate as u64 / 1000) as u32
    }
}

impl Default for VadProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioProcessor for VadProcessor {
    fn process(&mut self, samples: &mut [f32], sample_rate: u32) -> ProcessorResult {
        self.current_sample_rate = sample_rate;

        // Calculate RMS level in dB
        let level_db = calculate_rms_db(samples);

        // Check if current frame exceeds threshold
        let above_threshold = level_db > self.settings.threshold_db;

        if above_threshold {
            // Voice detected - activate and reset holdoff
            self.voice_active = true;
            self.holdoff_samples_remaining = self.holdoff_samples(sample_rate);
        } else if self.holdoff_samples_remaining > 0 {
            // Below threshold but in holdoff period
            let frame_samples = samples.len() as u32;
            self.holdoff_samples_remaining = self.holdoff_samples_remaining.saturating_sub(frame_samples);
        } else {
            // Below threshold and holdoff expired
            self.voice_active = false;
        }

        // Return result with level for UI metering
        ProcessorResult {
            suppress: !self.voice_active,
            level_db: Some(level_db),
        }
    }

    fn name(&self) -> &'static str {
        "Voice Activity Detection"
    }

    fn reset(&mut self) {
        self.voice_active = false;
        self.holdoff_samples_remaining = 0;
    }

    fn config(&self) -> serde_json::Value {
        serde_json::to_value(&self.settings).unwrap_or_default()
    }

    fn set_config(&mut self, config: &serde_json::Value) {
        if let Ok(settings) = serde_json::from_value::<VadSettings>(config.clone()) {
            self.settings = settings;
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset();
        }
    }
}

/// Factory for creating VadProcessor instances.
pub struct VadProcessorFactory;

impl ProcessorFactory for VadProcessorFactory {
    fn type_id(&self) -> &'static str {
        type_ids::VAD
    }

    fn display_name(&self) -> &'static str {
        "Voice Activity Detection"
    }

    fn create_default(&self) -> Box<dyn AudioProcessor> {
        Box::new(VadProcessor::new())
    }

    fn create_from_config(&self, config: &serde_json::Value) -> Result<Box<dyn AudioProcessor>, String> {
        let settings: VadSettings = serde_json::from_value(config.clone())
            .map_err(|e| format!("Invalid VAD settings: {}", e))?;

        Ok(Box::new(VadProcessor::with_settings(settings)))
    }

    fn settings_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "threshold_db": {
                    "type": "number",
                    "title": "Threshold (dB)",
                    "description": "Audio level threshold in dB RMS. Frames below this are considered silence.",
                    "default": -40.0,
                    "minimum": -60.0,
                    "maximum": 0.0
                },
                "holdoff_ms": {
                    "type": "integer",
                    "title": "Holdoff (ms)",
                    "description": "Time to continue transmitting after voice drops below threshold. Prevents cutting off word endings.",
                    "default": 300,
                    "minimum": 0,
                    "maximum": 2000
                }
            }
        })
    }

    fn description(&self) -> &'static str {
        "Detects voice activity and suppresses silence for automatic transmission control"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vad_creation() {
        let processor = VadProcessor::new();
        assert!(processor.is_enabled());
        assert!(!processor.is_voice_active());
        assert_eq!(processor.threshold_db(), -40.0);
        assert_eq!(processor.holdoff_ms(), 300);
    }

    #[test]
    fn test_vad_detects_silence() {
        let mut processor = VadProcessor::new();

        // Very quiet samples (essentially silence)
        let mut samples = vec![0.0001f32; 960];
        let result = processor.process(&mut samples, 48000);

        // Should be suppressed (no voice)
        assert!(result.suppress);
        assert!(!processor.is_voice_active());
    }

    #[test]
    fn test_vad_detects_voice() {
        let mut processor = VadProcessor::new();

        // Loud samples (voice-like level)
        let mut samples = vec![0.3f32; 960];
        let result = processor.process(&mut samples, 48000);

        // Should NOT be suppressed (voice detected)
        assert!(!result.suppress);
        assert!(processor.is_voice_active());
    }

    #[test]
    fn test_vad_holdoff() {
        let mut processor = VadProcessor::with_settings(VadSettings {
            threshold_db: -40.0,
            holdoff_ms: 100, // 100ms holdoff = ~4800 samples at 48kHz
        });

        // First: loud samples to activate
        let mut samples = vec![0.3f32; 960];
        processor.process(&mut samples, 48000);
        assert!(processor.is_voice_active());

        // Second: quiet samples, but still in holdoff
        let mut samples = vec![0.0001f32; 960];
        let result = processor.process(&mut samples, 48000);
        assert!(!result.suppress); // Still active due to holdoff
        assert!(processor.is_voice_active());

        // Process more quiet frames until holdoff expires
        for _ in 0..10 {
            let mut samples = vec![0.0001f32; 960];
            processor.process(&mut samples, 48000);
        }

        // Now should be inactive
        let mut samples = vec![0.0001f32; 960];
        let result = processor.process(&mut samples, 48000);
        assert!(result.suppress);
        assert!(!processor.is_voice_active());
    }

    #[test]
    fn test_vad_reset() {
        let mut processor = VadProcessor::new();

        // Activate voice
        let mut samples = vec![0.3f32; 960];
        processor.process(&mut samples, 48000);
        assert!(processor.is_voice_active());

        // Reset
        processor.reset();
        assert!(!processor.is_voice_active());
    }

    #[test]
    fn test_vad_level_reporting() {
        let mut processor = VadProcessor::new();

        let mut samples = vec![0.5f32; 960];
        let result = processor.process(&mut samples, 48000);

        // Should report level
        assert!(result.level_db.is_some());
        let level = result.level_db.unwrap();
        // 0.5 amplitude ~= -6dB
        assert!(level > -10.0 && level < 0.0);
    }

    #[test]
    fn test_factory_create() {
        let factory = VadProcessorFactory;
        let config = serde_json::json!({
            "threshold_db": -35.0,
            "holdoff_ms": 200
        });

        let processor = factory.create_from_config(&config).unwrap();
        assert_eq!(processor.name(), "Voice Activity Detection");
    }

    #[test]
    fn test_config_roundtrip() {
        let processor = VadProcessor::with_settings(VadSettings {
            threshold_db: -45.0,
            holdoff_ms: 500,
        });
        let config = processor.config();

        let mut processor2 = VadProcessor::new();
        processor2.set_config(&config);

        assert!((processor2.threshold_db() - (-45.0)).abs() < 0.001);
        assert_eq!(processor2.holdoff_ms(), 500);
    }
}
