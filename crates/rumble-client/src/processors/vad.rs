//! Voice Activity Detection (VAD) processor.
//!
//! This processor detects when voice is present in the audio stream and
//! sets the `suppress` flag when no voice is detected. When enabled in the
//! TX pipeline, it provides voice-activated transmission in Continuous mode.
//! It can also be used in PTT mode to avoid transmitting silence/noise.
//!
//! The current implementation uses energy-based detection with a configurable
//! threshold and holdoff timer to avoid cutting off speech endings.

use rumble_audio::{AudioProcessor, ProcessorFactory, ProcessorResult, calculate_rms_db};
use serde::{Deserialize, Serialize};

use super::{
    type_ids,
    vad_shaper::{VadShaper, VadShaperSettings},
};

/// Settings for the VAD processor.
///
/// Field defaults live in `Default::default()` and **must match** the
/// `default` values in [`VadProcessorFactory::settings_schema`] — the
/// schema is the single source of truth, and persisted configs are
/// backfilled against it (see `ProcessorRegistry::backfill_settings`)
/// before they reach this struct. The struct-level `#[serde(default)]`
/// is a defensive belt-and-braces fallback for the create-from-raw-JSON
/// paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VadSettings {
    /// Trigger threshold in dB RMS. Signal must exceed this (for at least
    /// `attack_ms`) to activate transmission.
    /// Typical values: -40 to -30 dB for quiet environments,
    /// -50 to -40 dB for noisy environments.
    pub threshold_db: f32,

    /// Release (hold) threshold in dB RMS. Once active, the signal must
    /// drop below this — *and* the holdoff timer must expire — before
    /// transmission stops. Should be set lower than `threshold_db` to
    /// provide hysteresis (typical: 5–10 dB below the trigger). If
    /// configured above `threshold_db` it is clamped at runtime, which
    /// disables hysteresis.
    pub release_threshold_db: f32,

    /// Minimum attack duration in milliseconds. The signal must remain
    /// above `threshold_db` for this long before activating. Rejects
    /// short transients (keyboard clicks, pops). 0 = activate on first
    /// frame above threshold (original behavior).
    pub attack_ms: u32,

    /// Holdoff time in milliseconds.
    /// After the signal drops below `release_threshold_db`, continue
    /// transmitting for this duration. Prevents cutting off word endings.
    pub holdoff_ms: u32,
}

impl Default for VadSettings {
    fn default() -> Self {
        Self {
            threshold_db: -40.0,
            release_threshold_db: -45.0,
            attack_ms: 0,
            holdoff_ms: 300,
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
    shaper: VadShaper,
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
            shaper: VadShaper::new(),
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

    /// Get the release (hold) threshold in dB.
    pub fn release_threshold_db(&self) -> f32 {
        self.settings.release_threshold_db
    }

    /// Set the release (hold) threshold in dB.
    pub fn set_release_threshold_db(&mut self, release_threshold_db: f32) {
        self.settings.release_threshold_db = release_threshold_db;
    }

    /// Get the minimum attack duration in milliseconds.
    pub fn attack_ms(&self) -> u32 {
        self.settings.attack_ms
    }

    /// Set the minimum attack duration in milliseconds.
    pub fn set_attack_ms(&mut self, attack_ms: u32) {
        self.settings.attack_ms = attack_ms;
    }

    /// Check if voice is currently detected as active.
    pub fn is_voice_active(&self) -> bool {
        self.shaper.is_active()
    }

    fn shaper_settings(&self) -> VadShaperSettings {
        VadShaperSettings {
            trigger: self.settings.threshold_db,
            release: self.settings.release_threshold_db,
            attack_ms: self.settings.attack_ms,
            holdoff_ms: self.settings.holdoff_ms,
        }
    }
}

impl Default for VadProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioProcessor for VadProcessor {
    fn process(&mut self, samples: &mut [f32], sample_rate: u32) -> ProcessorResult {
        // RMS is still needed internally to feed the shaper's
        // trigger/release decision, but is no longer reported back to
        // the pipeline — metering taps live in the audio task instead.
        let level_db = calculate_rms_db(samples);
        let active = self
            .shaper
            .update(level_db, samples.len() as u32, sample_rate, &self.shaper_settings());
        ProcessorResult { suppress: !active }
    }

    fn name(&self) -> &'static str {
        "Voice Activity Detection"
    }

    fn reset(&mut self) {
        self.shaper.reset();
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
        let settings: VadSettings =
            serde_json::from_value(config.clone()).map_err(|e| format!("Invalid VAD settings: {}", e))?;

        Ok(Box::new(VadProcessor::with_settings(settings)))
    }

    fn settings_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "threshold_db": {
                    "type": "number",
                    "title": "Trigger threshold (dB)",
                    "description": "Audio level required to start transmitting.",
                    "default": -40.0,
                    "minimum": -60.0,
                    "maximum": 0.0
                },
                "release_threshold_db": {
                    "type": "number",
                    "title": "Release threshold (dB)",
                    "description": "Once active, stay active until level drops below this. Set lower than the trigger threshold for hysteresis.",
                    "default": -45.0,
                    "minimum": -60.0,
                    "maximum": 0.0
                },
                "attack_ms": {
                    "type": "integer",
                    "title": "Attack (ms)",
                    "description": "Signal must stay above the trigger threshold for this long before activating. Rejects short transients.",
                    "default": 0,
                    "minimum": 0,
                    "maximum": 200
                },
                "holdoff_ms": {
                    "type": "integer",
                    "title": "Holdoff (ms)",
                    "description": "Time to keep transmitting after the level drops below the release threshold. Prevents cutting off word endings.",
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
        assert_eq!(processor.release_threshold_db(), -45.0);
        assert_eq!(processor.attack_ms(), 0);
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
            release_threshold_db: -40.0, // disable hysteresis for this test
            attack_ms: 0,
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

    // Note: VAD used to report `result.level_db` so the UI meter could
    // read it. Metering has moved into the audio task (RMS taps before
    // and after the pipeline), so VAD now only reports `suppress`. The
    // shaper still uses an internal RMS computation for its trigger
    // decision — see `test_vad_suppress_with_loud_signal`.

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
            release_threshold_db: -52.0,
            attack_ms: 40,
            holdoff_ms: 500,
        });
        let config = processor.config();

        let mut processor2 = VadProcessor::new();
        processor2.set_config(&config);

        assert!((processor2.threshold_db() - (-45.0)).abs() < 0.001);
        assert!((processor2.release_threshold_db() - (-52.0)).abs() < 0.001);
        assert_eq!(processor2.attack_ms(), 40);
        assert_eq!(processor2.holdoff_ms(), 500);
    }

    /// Old configs (saved before hysteresis/attack were added) should
    /// deserialize cleanly with sensible defaults for the new fields.
    #[test]
    fn test_vad_legacy_config_compat() {
        let config = serde_json::json!({
            "threshold_db": -38.0,
            "holdoff_ms": 250
        });
        let mut processor = VadProcessor::new();
        processor.set_config(&config);

        assert!((processor.threshold_db() - (-38.0)).abs() < 0.001);
        assert_eq!(processor.holdoff_ms(), 250);
        assert_eq!(processor.release_threshold_db(), -45.0);
        assert_eq!(processor.attack_ms(), 0);
    }

    /// With hysteresis configured, once active the level can drop below
    /// the trigger but stay above the release threshold and remain active
    /// indefinitely (holdoff never starts counting down).
    #[test]
    fn test_vad_hysteresis_holds_between_thresholds() {
        let mut processor = VadProcessor::with_settings(VadSettings {
            threshold_db: -20.0,
            release_threshold_db: -40.0,
            attack_ms: 0,
            holdoff_ms: 20, // short — would expire quickly if it actually counted down
        });

        // Activate with a loud frame (~0 dB).
        let mut loud = vec![0.5f32; 960];
        processor.process(&mut loud, 48000);
        assert!(processor.is_voice_active());

        // Frame between thresholds: ~-26 dB (above -40 release, below -20 trigger).
        // Should stay active across many frames without the holdoff expiring.
        for _ in 0..50 {
            let mut mid = vec![0.05f32; 960];
            let r = processor.process(&mut mid, 48000);
            assert!(!r.suppress, "frame above release threshold must not suppress");
            assert!(processor.is_voice_active());
        }

        // Now drop below release: holdoff begins, then deactivates.
        for _ in 0..10 {
            let mut quiet = vec![0.0001f32; 960];
            processor.process(&mut quiet, 48000);
        }
        let mut quiet = vec![0.0001f32; 960];
        let r = processor.process(&mut quiet, 48000);
        assert!(r.suppress);
        assert!(!processor.is_voice_active());
    }

    /// A loud frame shorter than `attack_ms` must not trigger activation;
    /// activation requires the level to stay above trigger across the
    /// full attack window.
    #[test]
    fn test_vad_attack_rejects_short_transient() {
        // attack_ms = 50ms ≈ 2400 samples at 48kHz → needs ~3 frames of 960.
        let mut processor = VadProcessor::with_settings(VadSettings {
            threshold_db: -40.0,
            release_threshold_db: -45.0,
            attack_ms: 50,
            holdoff_ms: 300,
        });

        // Single loud frame (~20ms) — below the 50ms attack window.
        let mut loud = vec![0.3f32; 960];
        let r = processor.process(&mut loud, 48000);
        assert!(r.suppress, "single short transient must not activate");
        assert!(!processor.is_voice_active());

        // Followed by quiet: accumulator resets.
        let mut quiet = vec![0.0001f32; 960];
        processor.process(&mut quiet, 48000);
        assert!(!processor.is_voice_active());

        // Now sustained loud across multiple frames — should activate
        // once cumulative loud time crosses the attack window.
        for _ in 0..3 {
            let mut loud = vec![0.3f32; 960];
            processor.process(&mut loud, 48000);
        }
        assert!(processor.is_voice_active());
    }

    /// A release threshold configured above the trigger threshold would
    /// be unreachable for the inactive→active transition. The processor
    /// clamps it to the trigger, which collapses to the no-hysteresis
    /// (original) behavior.
    #[test]
    fn test_vad_release_clamped_to_trigger() {
        let mut processor = VadProcessor::with_settings(VadSettings {
            threshold_db: -40.0,
            release_threshold_db: -20.0, // nonsensical: above trigger
            attack_ms: 0,
            holdoff_ms: 0,
        });

        let mut loud = vec![0.3f32; 960];
        processor.process(&mut loud, 48000);
        assert!(processor.is_voice_active());

        // With release clamped to -40, a frame just below trigger deactivates.
        let mut quiet = vec![0.0001f32; 960];
        let r = processor.process(&mut quiet, 48000);
        assert!(r.suppress);
        assert!(!processor.is_voice_active());
    }
}
