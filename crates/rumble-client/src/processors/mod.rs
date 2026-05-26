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

/// Merge a persisted pipeline config with the default to ensure all processors are present.
///
/// This handles schema evolution along two axes:
/// - Whole processors added since the persisted config was written are
///   inserted at their default position.
/// - Per-processor settings fields added since are backfilled from each
///   processor's schema defaults, so every downstream reader (audio task,
///   UI display, UI event handlers) sees the same fully populated object.
///
/// Existing values are preserved; only missing keys are filled.
pub fn merge_with_default_tx_pipeline(persisted: &PipelineConfig, registry: &ProcessorRegistry) -> PipelineConfig {
    // Build a map of existing processor configs by type_id
    let mut existing: std::collections::HashMap<&str, &ProcessorConfig> =
        persisted.processors.iter().map(|p| (p.type_id.as_str(), p)).collect();

    // Build the merged pipeline in the default order
    let processors: Vec<ProcessorConfig> = DEFAULT_TX_PIPELINE
        .iter()
        .filter_map(|(type_id, default_enabled)| {
            let mut config = if let Some(existing_config) = existing.remove(type_id) {
                existing_config.clone()
            } else {
                let mut config = registry.default_config(type_id)?;
                config.enabled = *default_enabled;
                config
            };
            registry.backfill_settings(type_id, &mut config.settings);
            Some(config)
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
