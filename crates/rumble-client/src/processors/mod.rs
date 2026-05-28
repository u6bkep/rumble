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

/// Merge a persisted pipeline config with the registry to ensure it can
/// still be executed, while preserving any runtime composition the user
/// has done (add / remove / reorder).
///
/// The persisted order is authoritative — this function never reorders
/// the user's pipeline. It handles schema evolution along three axes:
/// - Processor types no longer registered are dropped (otherwise
///   `AudioPipeline::from_config` would fail outright).
/// - Built-in defaults that the user has never seen are appended to the
///   end so a newly-shipped processor still surfaces (in its disabled
///   default state for migrations, enabled state for fresh users).
/// - Per-processor settings fields added since are backfilled from each
///   processor's schema defaults, so every downstream reader (audio
///   task, UI display, UI event handlers) sees the same fully populated
///   object.
///
/// Existing values are preserved; only missing keys are filled.
pub fn merge_with_default_tx_pipeline(persisted: &PipelineConfig, registry: &ProcessorRegistry) -> PipelineConfig {
    let mut processors: Vec<ProcessorConfig> = persisted
        .processors
        .iter()
        .filter(|p| registry.has(&p.type_id))
        .cloned()
        .map(|mut config| {
            registry.backfill_settings(&config.type_id, &mut config.settings);
            config
        })
        .collect();

    // Append any built-in processor the user hasn't seen yet. Migration
    // semantics: the new processor lands at the end of the chain in its
    // documented default-enabled state — it's then up to the user to
    // reorder if they want it earlier.
    for (type_id, default_enabled) in DEFAULT_TX_PIPELINE {
        if processors.iter().any(|p| p.type_id == *type_id) {
            continue;
        }
        if let Some(mut config) = registry.default_config(type_id) {
            config.enabled = *default_enabled;
            processors.push(config);
        }
    }

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
    //! The function exists to handle two evolutions: a new built-in
    //! processor was added since the user last saved their config (it
    //! must surface), and a previously-shipped processor was removed
    //! from the registry (its persisted entry must not stall the
    //! pipeline). User intent — order, duplicates, custom settings —
    //! is authoritative and must round-trip unchanged.
    //!
    //! These tests lock those contracts in. Earlier versions of this
    //! function used a `HashMap<type_id, &ProcessorConfig>` keyed merge
    //! that silently rewrote the user's pipeline to the default order
    //! and dropped any non-default processors; if you regress here,
    //! that's the shape of the bug.
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

    /// An empty persisted config means "first run after upgrade" — the
    /// default pipeline must materialize.
    #[test]
    fn empty_persisted_yields_default_pipeline() {
        let registry = registry_with_builtins();
        let persisted = PipelineConfig {
            processors: vec![],
            ..Default::default()
        };

        let merged = merge_with_default_tx_pipeline(&persisted, &registry);

        let ids: Vec<&str> = merged.processors.iter().map(|p| p.type_id.as_str()).collect();
        assert_eq!(ids, [type_ids::GAIN, type_ids::DENOISE, type_ids::VAD]);
        let enabled: Vec<bool> = merged.processors.iter().map(|p| p.enabled).collect();
        assert_eq!(enabled, [true, true, false], "matches DEFAULT_TX_PIPELINE flags");
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

    /// A built-in processor that was added after the user's persisted
    /// config was written must appear in the merged result, appended to
    /// the end (so it doesn't disturb any reorder the user has done).
    #[test]
    fn missing_default_is_appended() {
        let registry = registry_with_builtins();
        // Persist only gain — denoise + vad are "new" relative to this
        // config.
        let persisted = PipelineConfig {
            processors: vec![cfg(type_ids::GAIN, true, json!({}))],
            ..Default::default()
        };

        let merged = merge_with_default_tx_pipeline(&persisted, &registry);

        let ids: Vec<&str> = merged.processors.iter().map(|p| p.type_id.as_str()).collect();
        assert_eq!(ids[0], type_ids::GAIN, "user's existing entry stays first");
        assert!(ids[1..].contains(&type_ids::DENOISE));
        assert!(ids[1..].contains(&type_ids::VAD));
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
