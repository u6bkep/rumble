//! Energy-based noise gate processor.
//!
//! This processor measures the RMS level of each frame and sets the
//! `suppress` flag when the level falls below a configurable threshold.
//! When enabled in the TX pipeline, it provides level-gated transmission
//! in Continuous mode. It can also be used in PTT mode to avoid
//! transmitting silence/noise.
//!
//! Historically this stage was called "Voice Activity Detection", but it
//! makes no attempt to distinguish speech from other sound — it is a
//! plain amplitude gate with hysteresis and attack/holdoff shaping. The
//! ML voice gate lives in the RNNoise stage (`super::denoise`).

use rumble_audio::{
    Anchor, AudioProcessor, OutputKind, OutputSink, OutputSpec, ProcessorFactory, ProcessorResult, Role, Zone,
    calculate_rms_db,
};
use serde::{Deserialize, Serialize};

use super::{
    type_ids,
    vad_shaper::{VadShaper, VadShaperSettings},
};

/// Floor of the gate's level meter in dB; also the silence clamp for the
/// published `level_db` output (RMS of digital silence is -inf, which
/// would render as "-inf dB"). Matches the UI's VU meter floor.
const METER_MIN_DB: f32 = -60.0;

/// Settings for the noise gate processor.
///
/// Field defaults live in `Default::default()` and **must match** the
/// `default` values in [`NoiseGateProcessorFactory::settings_schema`] —
/// the schema is the single source of truth, and persisted configs are
/// backfilled against it (see `ProcessorRegistry::backfill_settings`)
/// before they reach this struct. The struct-level `#[serde(default)]`
/// is a defensive belt-and-braces fallback for the create-from-raw-JSON
/// paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NoiseGateSettings {
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
    ///
    /// The attack window delays the *decision*, not the audio: frames
    /// suppressed while the window accumulates sit in the encode gate's
    /// pre-roll ring (`rumble_client::preroll`, 100 ms) and are transmitted
    /// retroactively when the gate opens. Attack durations beyond the
    /// pre-roll length lose the excess.
    pub attack_ms: u32,

    /// Holdoff time in milliseconds.
    /// After the signal drops below `release_threshold_db`, continue
    /// transmitting for this duration. Prevents cutting off word endings.
    pub holdoff_ms: u32,
}

impl Default for NoiseGateSettings {
    fn default() -> Self {
        Self {
            threshold_db: -40.0,
            release_threshold_db: -45.0,
            attack_ms: 0,
            holdoff_ms: 300,
        }
    }
}

/// Energy-based noise gate.
///
/// Measures frame RMS against a threshold. When the level is below it,
/// sets `suppress = true` in the result, signaling that the frame should
/// not be transmitted.
///
/// Features:
/// - Energy-based gating with configurable threshold
/// - Hysteresis via a separate release threshold
/// - Holdoff timer to avoid cutting off speech endings
pub struct NoiseGateProcessor {
    settings: NoiseGateSettings,
    enabled: bool,
    shaper: VadShaper,
    /// RMS level of the most-recent frame, cached for the live UI meter.
    /// This is the exact value the gate compares against its thresholds —
    /// measured at this stage's position in the chain, unlike the audio
    /// task's fixed pre/post taps.
    last_level_db: f32,
}

impl NoiseGateProcessor {
    /// Create a new noise gate with default settings.
    pub fn new() -> Self {
        Self::with_settings(NoiseGateSettings::default())
    }

    /// Create a new noise gate with custom settings.
    pub fn with_settings(settings: NoiseGateSettings) -> Self {
        Self {
            settings,
            enabled: true,
            shaper: VadShaper::new(),
            last_level_db: f32::NEG_INFINITY,
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

    /// Check if the gate is currently open (signal considered active).
    pub fn is_gate_open(&self) -> bool {
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

impl Default for NoiseGateProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioProcessor for NoiseGateProcessor {
    fn process(&mut self, samples: &mut [f32], sample_rate: u32) -> ProcessorResult {
        // The RMS level feeds the shaper's trigger/release decision and is
        // cached for the stage's live `level_db` output. Signal-level
        // metering at fixed tap points still lives in the audio task —
        // this value is the gate's *decision input*, on the same dB axis
        // as its threshold sliders.
        let level_db = calculate_rms_db(samples);
        self.last_level_db = level_db;
        let active = self
            .shaper
            .update(level_db, samples.len() as u32, sample_rate, &self.shaper_settings());
        ProcessorResult { suppress: !active }
    }

    fn name(&self) -> &'static str {
        "Noise Gate"
    }

    fn reset(&mut self) {
        self.shaper.reset();
        self.last_level_db = f32::NEG_INFINITY;
    }

    fn config(&self) -> serde_json::Value {
        serde_json::to_value(&self.settings).unwrap_or_default()
    }

    fn set_config(&mut self, config: &serde_json::Value) {
        if let Ok(settings) = serde_json::from_value::<NoiseGateSettings>(config.clone()) {
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

    fn write_outputs(&self, sink: &mut OutputSink) {
        // Cached from the most-recent frame; no recomputation here. Silence
        // (-inf dB) clamps to the meter floor so the readout stays finite.
        sink.set("level_db", self.last_level_db.max(METER_MIN_DB));
        sink.set("gate_open", if self.is_gate_open() { 1.0 } else { 0.0 });
    }
}

/// Factory for creating [`NoiseGateProcessor`] instances.
pub struct NoiseGateProcessorFactory;

impl ProcessorFactory for NoiseGateProcessorFactory {
    fn type_id(&self) -> &'static str {
        type_ids::NOISE_GATE
    }

    fn display_name(&self) -> &'static str {
        "Noise Gate"
    }

    fn create_default(&self) -> Box<dyn AudioProcessor> {
        Box::new(NoiseGateProcessor::new())
    }

    fn create_from_config(&self, config: &serde_json::Value) -> Result<Box<dyn AudioProcessor>, String> {
        let settings: NoiseGateSettings =
            serde_json::from_value(config.clone()).map_err(|e| format!("Invalid noise gate settings: {}", e))?;

        Ok(Box::new(NoiseGateProcessor::with_settings(settings)))
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
                    "description": "Signal must stay above the trigger threshold for this long before activating. Rejects short transients; the delayed audio is recovered from the 100 ms pre-roll buffer, not lost.",
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

    fn outputs(&self) -> Vec<OutputSpec> {
        vec![
            // Live input level on the same dB axis the trigger/release
            // sliders act on — the value the gate actually compares,
            // measured at this stage's slot in the chain. The coloured
            // bands ARE the gate thresholds: their edges track
            // `release_threshold_db`/`threshold_db`, so dragging a slider
            // grows or shrinks a zone directly. Below release the gate is
            // closed; between release and trigger is the hysteresis band
            // where it holds its state; above trigger it opens.
            OutputSpec {
                key: "level_db".to_string(),
                title: "Input level".to_string(),
                kind: OutputKind::Meter {
                    min: METER_MIN_DB,
                    max: 0.0,
                    unit: Some("dB".to_string()),
                    zones: vec![
                        Zone::new(
                            Anchor::fixed(METER_MIN_DB),
                            Anchor::setting("release_threshold_db"),
                            Role::Inactive,
                        )
                        .label("closed"),
                        Zone::new(
                            Anchor::setting("release_threshold_db"),
                            Anchor::setting("threshold_db"),
                            Role::Warning,
                        )
                        .label("hold"),
                        Zone::new(Anchor::setting("threshold_db"), Anchor::fixed(0.0), Role::Active).label("open"),
                    ],
                    marks: vec![],
                },
            },
            // Whether the shaper currently holds the gate open (after
            // attack/holdoff). A lamp, not a meter.
            OutputSpec {
                key: "gate_open".to_string(),
                title: "Gate open".to_string(),
                kind: OutputKind::Indicator,
            },
        ]
    }

    fn description(&self) -> &'static str {
        "Simple amplitude-based gate that suppresses transmission while the input level is below a threshold"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noise_gate_creation() {
        let processor = NoiseGateProcessor::new();
        assert!(processor.is_enabled());
        assert!(!processor.is_gate_open());
        assert_eq!(processor.threshold_db(), -40.0);
        assert_eq!(processor.release_threshold_db(), -45.0);
        assert_eq!(processor.attack_ms(), 0);
        assert_eq!(processor.holdoff_ms(), 300);
    }

    #[test]
    fn test_noise_gate_suppresses_silence() {
        let mut processor = NoiseGateProcessor::new();

        // Very quiet samples (essentially silence)
        let mut samples = vec![0.0001f32; 960];
        let result = processor.process(&mut samples, 48000);

        // Should be suppressed (below threshold)
        assert!(result.suppress);
        assert!(!processor.is_gate_open());
    }

    #[test]
    fn test_noise_gate_opens_on_signal() {
        let mut processor = NoiseGateProcessor::new();

        // Loud samples (speech-like level)
        let mut samples = vec![0.3f32; 960];
        let result = processor.process(&mut samples, 48000);

        // Should NOT be suppressed (above threshold)
        assert!(!result.suppress);
        assert!(processor.is_gate_open());
    }

    #[test]
    fn test_noise_gate_holdoff() {
        let mut processor = NoiseGateProcessor::with_settings(NoiseGateSettings {
            threshold_db: -40.0,
            release_threshold_db: -40.0, // disable hysteresis for this test
            attack_ms: 0,
            holdoff_ms: 100, // 100ms holdoff = ~4800 samples at 48kHz
        });

        // First: loud samples to activate
        let mut samples = vec![0.3f32; 960];
        processor.process(&mut samples, 48000);
        assert!(processor.is_gate_open());

        // Second: quiet samples, but still in holdoff
        let mut samples = vec![0.0001f32; 960];
        let result = processor.process(&mut samples, 48000);
        assert!(!result.suppress); // Still active due to holdoff
        assert!(processor.is_gate_open());

        // Process more quiet frames until holdoff expires
        for _ in 0..10 {
            let mut samples = vec![0.0001f32; 960];
            processor.process(&mut samples, 48000);
        }

        // Now should be inactive
        let mut samples = vec![0.0001f32; 960];
        let result = processor.process(&mut samples, 48000);
        assert!(result.suppress);
        assert!(!processor.is_gate_open());
    }

    #[test]
    fn test_noise_gate_reset() {
        let mut processor = NoiseGateProcessor::new();

        // Open the gate
        let mut samples = vec![0.3f32; 960];
        processor.process(&mut samples, 48000);
        assert!(processor.is_gate_open());

        // Reset
        processor.reset();
        assert!(!processor.is_gate_open());
    }

    /// The stage publishes its decision-input RMS and gate state as live
    /// outputs for the UI meter. Silence clamps to the meter floor so the
    /// readout never shows "-inf".
    #[test]
    fn test_noise_gate_outputs() {
        let factory = NoiseGateProcessorFactory;
        let specs = factory.outputs();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].key, "level_db");
        assert_eq!(specs[1].key, "gate_open");

        let mut processor = NoiseGateProcessor::new();

        // Before any frame: level clamps to the meter floor, gate closed.
        let mut slots = [123.0f32; 2];
        processor.write_outputs(&mut OutputSink::new(&specs, &mut slots));
        assert_eq!(slots[0], METER_MIN_DB);
        assert_eq!(slots[1], 0.0);

        // Loud frame: level is the frame's RMS (0.3 ≈ -10.5 dB), gate open.
        let mut samples = vec![0.3f32; 960];
        processor.process(&mut samples, 48000);
        let mut slots = [0.0f32; 2];
        processor.write_outputs(&mut OutputSink::new(&specs, &mut slots));
        assert!((slots[0] - calculate_rms_db(&samples)).abs() < 0.01);
        assert!(slots[0] > -20.0 && slots[0] < 0.0);
        assert_eq!(slots[1], 1.0);
    }

    #[test]
    fn test_factory_create() {
        let factory = NoiseGateProcessorFactory;
        let config = serde_json::json!({
            "threshold_db": -35.0,
            "holdoff_ms": 200
        });

        let processor = factory.create_from_config(&config).unwrap();
        assert_eq!(processor.name(), "Noise Gate");
    }

    #[test]
    fn test_config_roundtrip() {
        let processor = NoiseGateProcessor::with_settings(NoiseGateSettings {
            threshold_db: -45.0,
            release_threshold_db: -52.0,
            attack_ms: 40,
            holdoff_ms: 500,
        });
        let config = processor.config();

        let mut processor2 = NoiseGateProcessor::new();
        processor2.set_config(&config);

        assert!((processor2.threshold_db() - (-45.0)).abs() < 0.001);
        assert!((processor2.release_threshold_db() - (-52.0)).abs() < 0.001);
        assert_eq!(processor2.attack_ms(), 40);
        assert_eq!(processor2.holdoff_ms(), 500);
    }

    /// Old configs (saved before hysteresis/attack were added) should
    /// deserialize cleanly with sensible defaults for the new fields.
    #[test]
    fn test_legacy_config_compat() {
        let config = serde_json::json!({
            "threshold_db": -38.0,
            "holdoff_ms": 250
        });
        let mut processor = NoiseGateProcessor::new();
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
    fn test_hysteresis_holds_between_thresholds() {
        let mut processor = NoiseGateProcessor::with_settings(NoiseGateSettings {
            threshold_db: -20.0,
            release_threshold_db: -40.0,
            attack_ms: 0,
            holdoff_ms: 20, // short — would expire quickly if it actually counted down
        });

        // Activate with a loud frame (~0 dB).
        let mut loud = vec![0.5f32; 960];
        processor.process(&mut loud, 48000);
        assert!(processor.is_gate_open());

        // Frame between thresholds: ~-26 dB (above -40 release, below -20 trigger).
        // Should stay active across many frames without the holdoff expiring.
        for _ in 0..50 {
            let mut mid = vec![0.05f32; 960];
            let r = processor.process(&mut mid, 48000);
            assert!(!r.suppress, "frame above release threshold must not suppress");
            assert!(processor.is_gate_open());
        }

        // Now drop below release: holdoff begins, then deactivates.
        for _ in 0..10 {
            let mut quiet = vec![0.0001f32; 960];
            processor.process(&mut quiet, 48000);
        }
        let mut quiet = vec![0.0001f32; 960];
        let r = processor.process(&mut quiet, 48000);
        assert!(r.suppress);
        assert!(!processor.is_gate_open());
    }

    /// A loud frame shorter than `attack_ms` must not trigger activation;
    /// activation requires the level to stay above trigger across the
    /// full attack window.
    #[test]
    fn test_attack_rejects_short_transient() {
        // attack_ms = 50ms ≈ 2400 samples at 48kHz → needs ~3 frames of 960.
        let mut processor = NoiseGateProcessor::with_settings(NoiseGateSettings {
            threshold_db: -40.0,
            release_threshold_db: -45.0,
            attack_ms: 50,
            holdoff_ms: 300,
        });

        // Single loud frame (~20ms) — below the 50ms attack window.
        let mut loud = vec![0.3f32; 960];
        let r = processor.process(&mut loud, 48000);
        assert!(r.suppress, "single short transient must not activate");
        assert!(!processor.is_gate_open());

        // Followed by quiet: accumulator resets.
        let mut quiet = vec![0.0001f32; 960];
        processor.process(&mut quiet, 48000);
        assert!(!processor.is_gate_open());

        // Now sustained loud across multiple frames — should activate
        // once cumulative loud time crosses the attack window.
        for _ in 0..3 {
            let mut loud = vec![0.3f32; 960];
            processor.process(&mut loud, 48000);
        }
        assert!(processor.is_gate_open());
    }

    /// A release threshold configured above the trigger threshold would
    /// be unreachable for the inactive→active transition. The processor
    /// clamps it to the trigger, which collapses to the no-hysteresis
    /// (original) behavior.
    #[test]
    fn test_release_clamped_to_trigger() {
        let mut processor = NoiseGateProcessor::with_settings(NoiseGateSettings {
            threshold_db: -40.0,
            release_threshold_db: -20.0, // nonsensical: above trigger
            attack_ms: 0,
            holdoff_ms: 0,
        });

        let mut loud = vec![0.3f32; 960];
        processor.process(&mut loud, 48000);
        assert!(processor.is_gate_open());

        // With release clamped to -40, a frame just below trigger deactivates.
        let mut quiet = vec![0.0001f32; 960];
        let r = processor.process(&mut quiet, 48000);
        assert!(r.suppress);
        assert!(!processor.is_gate_open());
    }
}
