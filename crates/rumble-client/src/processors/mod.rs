//! Built-in audio processors for the Rumble client.
//!
//! This module contains implementations of [`rumble_audio::AudioProcessor`] for
//! common audio processing tasks. Each processor is registered with a
//! `builtin.` prefix in the processor registry.
//!
//! # Available Processors
//!
//! - [`GainProcessor`] (`builtin.gain`) — Volume adjustment
//! - [`DenoiseProcessor`] (`builtin.denoise`) — RNNoise denoise + ML voice gate
//! - [`VadProcessor`] (`builtin.vad`) — Energy-only voice activity detection
//!   (legacy; kept for users who want zero ML in the chain)
//!
//! # Usage
//!
//! ```ignore
//! use rumble_client::processors::{register_builtin_processors, GainProcessor};
//! use rumble_audio::ProcessorRegistry;
//!
//! let mut registry = ProcessorRegistry::new();
//! register_builtin_processors(&mut registry);
//! ```

mod denoise;
mod gain;
mod vad;
mod vad_shaper;

pub use denoise::{DenoiseProcessor, DenoiseProcessorFactory};
pub use gain::{GainProcessor, GainProcessorFactory};
pub use vad::{VadProcessor, VadProcessorFactory};

use rumble_audio::{PipelineConfig, ProcessorConfig, ProcessorRegistry};

/// Register all built-in processors with the registry.
pub fn register_builtin_processors(registry: &mut ProcessorRegistry) {
    registry.register(Box::new(GainProcessorFactory));
    registry.register(Box::new(DenoiseProcessorFactory));
    registry.register(Box::new(VadProcessorFactory));
}

/// Default TX pipeline processor configuration.
///
/// Each entry is (type_id, default_enabled).
/// Order matters - processors are applied in this order.
///
/// The denoise stage handles both noise suppression and voice gating (its
/// `vad_enabled` setting defaults to true for fresh configs), so new users
/// get RNNoise as their voice gate out of the box. The standalone energy
/// VAD stays registered and listed in the pipeline but disabled, available
/// for users who want an ML-free gate.
pub const DEFAULT_TX_PIPELINE: &[(&str, bool)] = &[
    (type_ids::GAIN, true),
    (type_ids::DENOISE, true),
    (type_ids::VAD, false),
];

/// Build the default TX pipeline configuration.
///
/// This creates a pipeline with all standard processors in the correct order,
/// using their default settings and the default enabled state for each.
pub fn build_default_tx_pipeline(registry: &ProcessorRegistry) -> PipelineConfig {
    let processors: Vec<ProcessorConfig> = DEFAULT_TX_PIPELINE
        .iter()
        .filter_map(|(type_id, default_enabled)| {
            registry.default_config(type_id).map(|mut config| {
                config.enabled = *default_enabled;
                config
            })
        })
        .collect();

    PipelineConfig {
        processors,
        ..Default::default()
    }
}

/// Make a persisted pipeline config executable against the current
/// registry, while preserving the user's composition (add / remove /
/// reorder) untouched.
///
/// A persisted config is the user's: this function never adds, removes
/// (beyond unregistered types), reorders, or re-enables stages. In
/// particular, built-ins shipped after the config was saved are NOT
/// appended — they stay discoverable through the settings UI's add-stage
/// menu, and only brand-new users (no persisted pipeline at all, see
/// [`build_default_tx_pipeline`]) get them by default. An empty persisted
/// list means the user removed every stage; it stays empty.
///
/// What it does fix up is the minimum needed for execution:
/// - Processor types no longer registered are dropped (otherwise
///   `AudioPipeline::from_config` would fail outright).
/// - Per-processor settings fields added since are backfilled from each
///   processor's schema defaults, so every downstream reader (audio
///   task, UI display, UI event handlers) sees the same fully populated
///   object.
///
/// Existing values are preserved; only missing keys are filled.
pub fn merge_with_default_tx_pipeline(persisted: &PipelineConfig, registry: &ProcessorRegistry) -> PipelineConfig {
    let processors: Vec<ProcessorConfig> = persisted
        .processors
        .iter()
        .filter(|p| registry.has(&p.type_id))
        .cloned()
        .map(|mut config| {
            registry.backfill_settings(&config.type_id, &mut config.settings);
            config
        })
        .collect();

    PipelineConfig {
        processors,
        frame_size: persisted.frame_size,
    }
}

/// Processor type IDs for built-in processors.
pub mod type_ids {
    pub const GAIN: &str = "builtin.gain";
    pub const DENOISE: &str = "builtin.denoise";
    pub const VAD: &str = "builtin.vad";
}

#[cfg(test)]
mod merge_tests {
    //! Regression coverage for `merge_with_default_tx_pipeline`.
    //!
    //! The contract: a persisted config belongs to the user and is
    //! touched as little as possible on upgrade. Only two fix-ups are
    //! allowed, both execution necessities: dropping processor types
    //! the registry no longer knows (they'd stall pipeline build) and
    //! backfilling settings keys added since (so all downstream readers
    //! agree on values). Everything else — order, duplicates, enabled
    //! flags, custom settings, and crucially the *set* of stages — must
    //! round-trip unchanged.
    //!
    //! Two historical bug shapes to not regress into: a
    //! `HashMap<type_id, &ProcessorConfig>` keyed merge that silently
    //! rewrote the user's pipeline to the default order and dropped
    //! non-default processors; and an append step that added newly
    //! shipped built-ins to existing configs *enabled*, silently
    //! changing upgrading users' audio processing and transmission
    //! gating (issue #45).
    use super::*;
    use rumble_audio::{PipelineConfig, ProcessorConfig};
    use serde_json::json;

    fn registry_with_builtins() -> ProcessorRegistry {
        let mut r = ProcessorRegistry::new();
        register_builtin_processors(&mut r);
        r
    }

    fn cfg(type_id: &str, enabled: bool, settings: serde_json::Value) -> ProcessorConfig {
        ProcessorConfig {
            type_id: type_id.to_string(),
            enabled,
            settings,
        }
    }

    /// An empty persisted list is a deliberate user state ("I removed
    /// every stage") and must stay empty. Fresh users are distinguished
    /// upstream by the *absence* of a persisted config
    /// (`settings.audio.tx_pipeline == None` → `build_default_tx_pipeline`),
    /// never by an empty list.
    #[test]
    fn empty_persisted_stays_empty() {
        let registry = registry_with_builtins();
        let persisted = PipelineConfig {
            processors: vec![],
            ..Default::default()
        };

        let merged = merge_with_default_tx_pipeline(&persisted, &registry);

        assert!(
            merged.processors.is_empty(),
            "a user who removed all stages must not have the defaults re-seeded"
        );
    }

    /// User reorder must survive a load. This is the original
    /// regression — old code keyed by `type_id` in a HashMap and
    /// re-emitted in the canonical order, silently undoing the user's
    /// composition.
    #[test]
    fn user_order_is_preserved() {
        let registry = registry_with_builtins();
        let persisted = PipelineConfig {
            // Reversed from the default order.
            processors: vec![
                cfg(type_ids::VAD, true, json!({})),
                cfg(type_ids::DENOISE, true, json!({})),
                cfg(type_ids::GAIN, true, json!({})),
            ],
            ..Default::default()
        };

        let merged = merge_with_default_tx_pipeline(&persisted, &registry);

        let ids: Vec<&str> = merged.processors.iter().map(|p| p.type_id.as_str()).collect();
        assert_eq!(ids, [type_ids::VAD, type_ids::DENOISE, type_ids::GAIN]);
    }

    /// Duplicate type_ids are legitimate (gain pre- and post-denoise is
    /// a real use case). They must all survive the merge.
    #[test]
    fn duplicate_type_ids_are_preserved() {
        let registry = registry_with_builtins();
        let persisted = PipelineConfig {
            processors: vec![
                cfg(type_ids::GAIN, true, json!({"gain_db": 6.0})),
                cfg(type_ids::DENOISE, true, json!({})),
                cfg(type_ids::GAIN, true, json!({"gain_db": -3.0})),
            ],
            ..Default::default()
        };

        let merged = merge_with_default_tx_pipeline(&persisted, &registry);

        let gain_count = merged.processors.iter().filter(|p| p.type_id == type_ids::GAIN).count();
        assert_eq!(gain_count, 2, "both gain stages must survive");
        // The intermediate denoise stage must keep its slot.
        assert_eq!(merged.processors[1].type_id, type_ids::DENOISE);
    }

    /// A persisted config for a type_id the registry no longer knows
    /// about must be dropped silently — `AudioPipeline::from_config`
    /// would otherwise fail to build the pipeline at all.
    #[test]
    fn unknown_type_id_is_dropped() {
        let registry = registry_with_builtins();
        let persisted = PipelineConfig {
            processors: vec![
                cfg(type_ids::GAIN, true, json!({})),
                cfg("removed.processor", true, json!({})),
                cfg(type_ids::DENOISE, true, json!({})),
            ],
            ..Default::default()
        };

        let merged = merge_with_default_tx_pipeline(&persisted, &registry);

        assert!(
            merged.processors.iter().all(|p| registry.has(&p.type_id)),
            "every surviving processor must be registered"
        );
        // The user's two known processors keep their relative order.
        assert_eq!(merged.processors[0].type_id, type_ids::GAIN);
        assert_eq!(merged.processors[1].type_id, type_ids::DENOISE);
    }

    /// A built-in processor that shipped after the user's persisted
    /// config was written must NOT be added to their pipeline — doing
    /// so silently changed upgrading users' audio processing (and, for
    /// gating stages like denoise, their transmission behavior). New
    /// stages are discoverable via the settings UI's add-stage menu
    /// instead (issue #45).
    #[test]
    fn missing_default_is_not_added() {
        let registry = registry_with_builtins();
        // Persist only gain — denoise + vad are "new" relative to this
        // config.
        let persisted = PipelineConfig {
            processors: vec![cfg(type_ids::GAIN, true, json!({}))],
            ..Default::default()
        };

        let merged = merge_with_default_tx_pipeline(&persisted, &registry);

        let ids: Vec<&str> = merged.processors.iter().map(|p| p.type_id.as_str()).collect();
        assert_eq!(ids, [type_ids::GAIN], "exactly the user's stages, nothing appended");
    }

    /// Settings keys missing from the persisted config must be filled
    /// from the schema default — without this, downstream readers
    /// (audio task / UI display / UI handlers) each fall back to their
    /// own private default and drift apart.
    #[test]
    fn missing_schema_fields_are_backfilled() {
        let registry = registry_with_builtins();
        // VAD with an empty settings object — every field is missing.
        let persisted = PipelineConfig {
            processors: vec![cfg(type_ids::VAD, true, json!({}))],
            ..Default::default()
        };

        let merged = merge_with_default_tx_pipeline(&persisted, &registry);

        let vad = &merged.processors[0];
        assert_eq!(vad.type_id, type_ids::VAD);
        // The VAD schema declares a `threshold_db` field with a numeric
        // default; after backfill, that key must be populated.
        assert!(
            vad.settings.get("threshold_db").is_some(),
            "VAD threshold_db must be backfilled from schema default"
        );
    }

    /// Round-trip: a config containing only known processors with full
    /// settings must come out the other side unchanged in shape and
    /// order. Backfill may add fields but must not reorder or drop.
    #[test]
    fn known_config_round_trips() {
        let registry = registry_with_builtins();
        // Seed each persisted entry with its schema defaults, so
        // backfill is a no-op and we can compare structures directly.
        let mut persisted = PipelineConfig::default();
        for type_id in [type_ids::DENOISE, type_ids::GAIN, type_ids::VAD] {
            persisted
                .processors
                .push(registry.default_config(type_id).expect("registered"));
        }

        let merged = merge_with_default_tx_pipeline(&persisted, &registry);

        assert_eq!(merged.processors.len(), persisted.processors.len());
        for (m, p) in merged.processors.iter().zip(persisted.processors.iter()) {
            assert_eq!(m.type_id, p.type_id);
            assert_eq!(m.enabled, p.enabled);
            assert_eq!(m.settings, p.settings);
        }
    }
}

#[cfg(test)]
mod pipeline_exec_tests {
    use super::*;
    use rumble_client_traits::codec::{OPUS_FRAME_SIZE, OPUS_SAMPLE_RATE};

    /// A pipeline of [Gain, VAD] must (a) apply the gain to the samples in
    /// chain order and (b) gate transmission via the aggregated suppress flag.
    ///
    /// This is the only test that drives a real processor *chain* end to end.
    /// It pins inter-processor ordering and the suppress-OR aggregation the
    /// audio task depends on — without constraining how any single processor
    /// is implemented internally.
    #[test]
    fn gain_then_vad_applies_gain_and_gates_on_level() {
        let gain_db = 6.0;
        let mut pipeline = rumble_audio::AudioPipeline::new(OPUS_FRAME_SIZE);
        pipeline.add(Box::new(GainProcessor::new(gain_db)));
        pipeline.add(Box::new(VadProcessor::with_settings(super::vad::VadSettings {
            threshold_db: -30.0,
            release_threshold_db: -30.0, // == threshold: disable hysteresis
            attack_ms: 0,                // gate per-frame: no attack delay...
            holdoff_ms: 0,               // ...and no release hold
        })));

        let gain = rumble_audio::db_to_linear(gain_db);

        // Loud frames sit above the VAD threshold after gain (→ pass); the
        // quiet frame sits below it (→ gated). Either way, the samples leaving
        // the pipeline must equal input * gain — proof the gain stage ran in
        // the chain ahead of the (non-mutating) VAD stage.
        for (amp, expect_suppress) in [(0.2f32, false), (0.001f32, true), (0.2f32, false)] {
            let mut samples = vec![amp; OPUS_FRAME_SIZE];
            let result = pipeline.process(&mut samples, OPUS_SAMPLE_RATE);

            assert_eq!(
                result.suppress, expect_suppress,
                "VAD gate decision for amplitude {amp}"
            );

            let expected = (amp * gain).clamp(-1.0, 1.0);
            assert!(
                samples.iter().all(|s| (s - expected).abs() < 1e-6),
                "gain must be applied in-chain: {amp} * {gain} = {expected}"
            );
        }
    }
}
