//! Built-in audio processors for the Rumble client.
//!
//! This module contains implementations of [`pipeline::AudioProcessor`] for
//! common audio processing tasks. Each processor is registered with a
//! `builtin.` prefix in the processor registry.
//!
//! # Available Processors
//!
//! - [`GainProcessor`] (`builtin.gain`) - Volume adjustment
//! - [`DenoiseProcessor`] (`builtin.denoise`) - RNNoise-based noise suppression
//! - [`VadProcessor`] (`builtin.vad`) - Voice activity detection
//!
//! # Usage
//!
//! ```ignore
//! use backend::processors::{register_builtin_processors, GainProcessor};
//! use pipeline::ProcessorRegistry;
//!
//! let mut registry = ProcessorRegistry::new();
//! register_builtin_processors(&mut registry);
//! ```

mod gain;
mod denoise;
mod vad;

pub use gain::{GainProcessor, GainProcessorFactory};
pub use denoise::{DenoiseProcessor, DenoiseProcessorFactory};
pub use vad::{VadProcessor, VadProcessorFactory};

use pipeline::ProcessorRegistry;

/// Register all built-in processors with the registry.
pub fn register_builtin_processors(registry: &mut ProcessorRegistry) {
    registry.register(Box::new(GainProcessorFactory));
    registry.register(Box::new(DenoiseProcessorFactory));
    registry.register(Box::new(VadProcessorFactory));
}

/// Processor type IDs for built-in processors.
pub mod type_ids {
    pub const GAIN: &str = "builtin.gain";
    pub const DENOISE: &str = "builtin.denoise";
    pub const VAD: &str = "builtin.vad";
}
