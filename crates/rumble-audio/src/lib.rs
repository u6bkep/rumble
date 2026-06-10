//! Audio processing pipeline traits and registry.
//!
//! This crate provides the core abstractions for building pluggable audio
//! processing pipelines. It is intentionally minimal with few dependencies
//! to allow external crates to implement custom processors without pulling
//! in the full backend dependency tree.
//!
//! # Architecture
//!
//! The pipeline consists of a chain of [`AudioProcessor`] instances that
//! process audio frames in sequence. Each processor can:
//!
//! - Modify audio samples in-place (e.g., gain, denoise, compress)
//! - Analyze audio and report results (e.g., level metering)
//! - Signal that a frame should be suppressed (e.g., VAD, noise gate)
//!
//! Processors are created by [`ProcessorFactory`] instances registered in
//! the [`ProcessorRegistry`]. This allows runtime discovery and configuration
//! of available processor types.
//!
//! # Example
//!
//! ```ignore
//! use rumble_audio::{AudioPipeline, ProcessorRegistry, ProcessorConfig};
//!
//! // Create registry and register built-in processors
//! let mut registry = ProcessorRegistry::new();
//! registry.register(Box::new(GainProcessorFactory));
//!
//! // Build pipeline from config
//! let config = PipelineConfig {
//!     processors: vec![
//!         ProcessorConfig {
//!             type_id: "builtin.gain".to_string(),
//!             enabled: true,
//!             settings: serde_json::json!({ "gain_db": -6.0 }),
//!         },
//!     ],
//!     frame_size: 960,
//! };
//!
//! let mut pipeline = AudioPipeline::from_config(&config, &registry)?;
//!
//! // Process audio
//! let mut samples = vec![0.0f32; 960];
//! let result = pipeline.process(&mut samples, 48000);
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod outputs;
pub use outputs::{
    Anchor, MAX_OUTPUTS, Mark, OutputEntry, OutputFrame, OutputKind, OutputLayout, OutputSink, OutputSpec, Role, Zone,
};

// =============================================================================
// Processor Result
// =============================================================================

/// Result from processing an audio frame.
///
/// Processors return this to communicate both analysis results and
/// processing decisions to the pipeline.
#[derive(Debug, Clone, Default)]
pub struct ProcessorResult {
    /// Suppress this frame from transmission/playback.
    ///
    /// When any processor in the pipeline sets this to `true`, the frame
    /// will not be transmitted (TX) or played (RX). This is used by:
    /// - Voice Activity Detection (VAD) to suppress silence
    /// - Noise gates to suppress low-level noise
    ///
    /// Pipeline uses OR logic: any processor returning `true` suppresses the frame.
    pub suppress: bool,
    // Note: level metering used to live here as `level_db: Option<f32>`,
    // reduced "last-wins" by the pipeline. That coupled the meter to a
    // specific processor's identity — removing the level-reporting
    // processor killed the meter. Metering is now done by the audio task
    // itself at deterministic pre/post-pipeline taps and published via
    // `rumble_client::MeterHandle`; `ProcessorResult` carries only
    // control-flow signals.
}

impl ProcessorResult {
    /// Create a result indicating the frame should pass through.
    pub fn pass() -> Self {
        Self::default()
    }

    /// Create a result indicating the frame should be suppressed.
    pub fn suppressed() -> Self {
        Self { suppress: true }
    }
}

// =============================================================================
// Audio Processor Trait
// =============================================================================

/// A stage in the audio processing pipeline.
///
/// Processors operate on fixed-size frames of audio samples. The frame size
/// and sample rate are provided to allow processors to adapt to different
/// configurations.
///
/// # Implementation Notes
///
/// - Processors must be `Send` to allow use across threads
/// - The `process` method receives samples as `&mut [f32]` in the range [-1.0, 1.0]
/// - Processors should handle variable frame sizes gracefully
/// - State should be reset when `reset()` is called (e.g., on transmission start)
pub trait AudioProcessor: Send {
    /// Process a frame of audio samples in-place.
    ///
    /// # Arguments
    /// * `samples` - Audio samples in [-1.0, 1.0] range, modified in-place
    /// * `sample_rate` - Current sample rate in Hz (e.g., 48000)
    ///
    /// # Returns
    /// A [`ProcessorResult`] indicating whether to suppress the frame and
    /// any analysis results (e.g., level metering).
    fn process(&mut self, samples: &mut [f32], sample_rate: u32) -> ProcessorResult;

    /// Human-readable name for debugging and UI display.
    fn name(&self) -> &'static str;

    /// Reset internal state.
    ///
    /// Called when transmission starts/stops or when the pipeline is
    /// reconfigured. Processors should clear any accumulated state
    /// (e.g., filter history, envelope followers).
    fn reset(&mut self) {}

    /// Get current configuration as JSON.
    ///
    /// This should return the current settings in the same format
    /// accepted by the factory's `create_from_config`.
    fn config(&self) -> serde_json::Value;

    /// Update configuration at runtime.
    ///
    /// Processors should apply new settings immediately. Invalid settings
    /// should be silently ignored or clamped to valid ranges.
    fn set_config(&mut self, config: &serde_json::Value);

    /// Whether this processor is currently enabled.
    fn is_enabled(&self) -> bool;

    /// Enable or disable this processor.
    ///
    /// Disabled processors are skipped during pipeline execution but
    /// remain in the chain for potential re-enabling.
    fn set_enabled(&mut self, enabled: bool);

    /// Emit this processor's live output values into `sink`, keyed by the
    /// `key` of each [`OutputSpec`] its factory declared via
    /// [`ProcessorFactory::outputs`].
    ///
    /// Called once per processed frame on the audio thread — keep it cheap
    /// and allocation-free (read cached fields, don't recompute). Stages
    /// with no outputs leave this as the default no-op.
    fn write_outputs(&self, _sink: &mut OutputSink) {}
}

// =============================================================================
// Processor Factory Trait
// =============================================================================

/// Factory for creating processor instances.
///
/// Each processor type provides a factory that can create instances from
/// serialized configuration. Factories are registered in the
/// [`ProcessorRegistry`] for runtime discovery.
///
/// # Type IDs
///
/// Type IDs should be namespaced to avoid collisions:
/// - Built-in processors: `builtin.denoise`, `builtin.vad`, etc.
/// - Plugin processors: `myplugin.autotune`, `company.effect`, etc.
pub trait ProcessorFactory: Send + Sync {
    /// Unique identifier for this processor type.
    ///
    /// This is used in [`ProcessorConfig`] to specify which factory
    /// should create the processor instance.
    fn type_id(&self) -> &'static str;

    /// Human-readable name for UI display.
    fn display_name(&self) -> &'static str;

    /// Create a new processor instance with default settings.
    fn create_default(&self) -> Box<dyn AudioProcessor>;

    /// Create a processor instance from JSON configuration.
    ///
    /// # Arguments
    /// * `config` - JSON object containing processor-specific settings
    ///
    /// # Returns
    /// A new processor instance, or an error message if the config is invalid.
    fn create_from_config(&self, config: &serde_json::Value) -> Result<Box<dyn AudioProcessor>, String>;

    /// Get the JSON schema for this processor's settings.
    ///
    /// This can be used by UI code to generate appropriate controls.
    /// The schema should follow JSON Schema conventions.
    fn settings_schema(&self) -> serde_json::Value;

    /// Describe this processor's live output meters/indicators for the UI.
    ///
    /// Mirror of [`settings_schema`](Self::settings_schema) for *feedback*:
    /// the values these specs describe are emitted per frame by
    /// [`AudioProcessor::write_outputs`]. Returns an empty vec for stages
    /// with no live feedback (the default).
    fn outputs(&self) -> Vec<OutputSpec> {
        Vec::new()
    }

    /// Get the default settings for this processor as JSON.
    ///
    /// This is used when creating a new processor config with default values.
    /// The returned JSON should be valid input for `create_from_config`.
    fn default_settings(&self) -> serde_json::Value {
        // Default implementation: create a default processor and get its config
        self.create_default().config()
    }

    /// Short description of what this processor does.
    fn description(&self) -> &'static str {
        ""
    }
}

// =============================================================================
// Configuration Types
// =============================================================================

/// Configuration for a single processor instance.
///
/// This is a serializable representation of a processor's type and settings,
/// used for persistence and transmission of pipeline configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessorConfig {
    /// Processor type identifier (e.g., "builtin.denoise", "builtin.vad").
    pub type_id: String,

    /// Whether this processor is enabled.
    pub enabled: bool,

    /// Processor-specific settings as JSON.
    #[serde(default)]
    pub settings: serde_json::Value,
}

impl ProcessorConfig {
    /// Create a new config with default settings.
    pub fn new(type_id: impl Into<String>) -> Self {
        Self {
            type_id: type_id.into(),
            enabled: true,
            settings: serde_json::Value::Object(Default::default()),
        }
    }

    /// Create a new config with specific settings.
    pub fn with_settings(type_id: impl Into<String>, settings: serde_json::Value) -> Self {
        Self {
            type_id: type_id.into(),
            enabled: true,
            settings,
        }
    }

    /// Set enabled state.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// Complete pipeline configuration.
///
/// Defines the ordered list of processors and processing parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Ordered list of processor configs.
    ///
    /// Processors are executed in order. The output of each processor
    /// becomes the input to the next.
    pub processors: Vec<ProcessorConfig>,

    /// Frame size in samples.
    ///
    /// Default is 960 samples (20ms at 48kHz). Smaller values reduce
    /// latency but may reduce compression efficiency.
    #[serde(default = "default_frame_size")]
    pub frame_size: usize,
}

fn default_frame_size() -> usize {
    960 // 20ms at 48kHz
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            processors: Vec::new(),
            frame_size: default_frame_size(),
        }
    }
}

impl PipelineConfig {
    /// Create an empty pipeline config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a processor to the pipeline.
    pub fn with_processor(mut self, config: ProcessorConfig) -> Self {
        self.processors.push(config);
        self
    }

    /// Set the frame size.
    pub fn with_frame_size(mut self, frame_size: usize) -> Self {
        self.frame_size = frame_size;
        self
    }
}

/// Per-user RX configuration with optional overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRxConfig {
    /// If Some, use these pipeline settings; if None, use global defaults.
    #[serde(default)]
    pub pipeline_override: Option<PipelineConfig>,

    /// Per-user volume adjustment in dB.
    ///
    /// This is applied independently of the pipeline and is always available.
    /// 0.0 = unity gain, negative = quieter, positive = louder.
    #[serde(default)]
    pub volume_db: f32,
}

impl Default for UserRxConfig {
    fn default() -> Self {
        Self {
            pipeline_override: None,
            volume_db: 0.0,
        }
    }
}

// =============================================================================
// Processor Registry
// =============================================================================

/// Registry for processor factories.
///
/// The registry maintains a mapping from type IDs to factories, allowing
/// runtime discovery and instantiation of processor types.
pub struct ProcessorRegistry {
    factories: HashMap<String, Box<dyn ProcessorFactory>>,
}

impl Default for ProcessorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessorRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Register a processor factory.
    ///
    /// If a factory with the same type ID already exists, it is replaced.
    pub fn register(&mut self, factory: Box<dyn ProcessorFactory>) {
        let type_id = factory.type_id().to_string();
        self.factories.insert(type_id, factory);
    }

    /// Create a processor from configuration.
    ///
    /// # Returns
    /// A new processor instance with the specified settings, or an error
    /// if the type ID is unknown or the settings are invalid.
    pub fn create(&self, config: &ProcessorConfig) -> Result<Box<dyn AudioProcessor>, String> {
        let factory = self
            .factories
            .get(&config.type_id)
            .ok_or_else(|| format!("Unknown processor type: {}", config.type_id))?;

        let mut processor = factory.create_from_config(&config.settings)?;
        processor.set_enabled(config.enabled);
        Ok(processor)
    }

    /// Create a processor with default settings.
    pub fn create_default(&self, type_id: &str) -> Result<Box<dyn AudioProcessor>, String> {
        let factory = self
            .factories
            .get(type_id)
            .ok_or_else(|| format!("Unknown processor type: {}", type_id))?;

        Ok(factory.create_default())
    }

    /// List all registered processor types.
    ///
    /// Returns tuples of (type_id, display_name, description).
    pub fn list_available(&self) -> Vec<(&str, &str, &str)> {
        self.factories
            .values()
            .map(|f| (f.type_id(), f.display_name(), f.description()))
            .collect()
    }

    /// Check if a processor type is registered.
    pub fn has(&self, type_id: &str) -> bool {
        self.factories.contains_key(type_id)
    }

    /// Get the settings schema for a processor type.
    pub fn settings_schema(&self, type_id: &str) -> Option<serde_json::Value> {
        self.factories.get(type_id).map(|f| f.settings_schema())
    }

    /// Get the output descriptors for a processor type. Empty if the type
    /// is unregistered or declares no outputs.
    pub fn outputs(&self, type_id: &str) -> Vec<OutputSpec> {
        self.factories.get(type_id).map(|f| f.outputs()).unwrap_or_default()
    }

    /// Get the default settings for a processor type.
    pub fn default_settings(&self, type_id: &str) -> Option<serde_json::Value> {
        self.factories.get(type_id).map(|f| f.default_settings())
    }

    /// Create a ProcessorConfig with default settings for a processor type.
    ///
    /// Returns None if the type_id is not registered.
    pub fn default_config(&self, type_id: &str) -> Option<ProcessorConfig> {
        self.factories.get(type_id).map(|f| ProcessorConfig {
            type_id: type_id.to_string(),
            enabled: true,
            settings: f.default_settings(),
        })
    }

    /// Fill any missing top-level properties in `settings` from this
    /// processor's schema defaults. Existing values are preserved.
    ///
    /// Call this on persisted configs before handing them to the runtime
    /// or to the UI so that every reader sees a fully populated object.
    /// Missing fields would otherwise pick up whatever per-reader fallback
    /// happens to be in scope (serde defaults in the audio task, the
    /// schema default in the UI display, hardcoded `false`/`0` in UI
    /// event handlers), and those fallbacks can — and historically did —
    /// drift apart.
    ///
    /// No-op if the type_id is unregistered or its schema has no
    /// `properties` block.
    pub fn backfill_settings(&self, type_id: &str, settings: &mut serde_json::Value) {
        if let Some(schema) = self.settings_schema(type_id) {
            backfill_settings_from_schema(settings, &schema);
        }
    }
}

/// Fill missing top-level properties in `value` from `schema.properties[*].default`.
///
/// If `value` is not a JSON object it is replaced with one. Keys outside
/// `schema.properties` are preserved.
pub fn backfill_settings_from_schema(value: &mut serde_json::Value, schema: &serde_json::Value) {
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    if !value.is_object() {
        *value = serde_json::Value::Object(serde_json::Map::new());
    }
    let object = value.as_object_mut().expect("just ensured object");
    for (key, prop_schema) in properties {
        if object.contains_key(key) {
            continue;
        }
        if let Some(default) = prop_schema.get("default") {
            object.insert(key.clone(), default.clone());
        }
    }
}

// =============================================================================
// Audio Pipeline
// =============================================================================

/// A chain of audio processors.
///
/// The pipeline executes processors in order, passing the output of each
/// to the next. The aggregated `ProcessorResult` uses OR logic for
/// `suppress` (any processor can gate the frame).
pub struct AudioPipeline {
    processors: Vec<Box<dyn AudioProcessor>>,
    /// Output descriptors per processor, parallel to `processors` and
    /// captured at build time from the registry. Drives `write_outputs`'s
    /// slot assignment; kept in lock-step with [`OutputLayout::derive`].
    /// Processors added via [`add`](Self::add) (test/builder paths) declare
    /// no outputs.
    output_specs: Vec<Vec<OutputSpec>>,
    frame_size: usize,
}

impl AudioPipeline {
    /// Create an empty pipeline.
    pub fn new(frame_size: usize) -> Self {
        Self {
            processors: Vec::new(),
            output_specs: Vec::new(),
            frame_size,
        }
    }

    /// Create a pipeline from configuration.
    pub fn from_config(config: &PipelineConfig, registry: &ProcessorRegistry) -> Result<Self, String> {
        let mut processors = Vec::with_capacity(config.processors.len());
        let mut output_specs = Vec::with_capacity(config.processors.len());

        for proc_config in &config.processors {
            let processor = registry.create(proc_config)?;
            output_specs.push(registry.outputs(&proc_config.type_id));
            processors.push(processor);
        }

        Ok(Self {
            processors,
            output_specs,
            frame_size: config.frame_size,
        })
    }

    /// Add a processor to the end of the pipeline.
    ///
    /// The processor is added with no output descriptors — this builder
    /// path is for tests and manual assembly; production pipelines go
    /// through [`from_config`](Self::from_config), which captures outputs
    /// from the registry.
    pub fn add(&mut self, processor: Box<dyn AudioProcessor>) {
        self.processors.push(processor);
        self.output_specs.push(Vec::new());
    }

    /// Process a frame of audio samples.
    ///
    /// Runs all enabled processors in sequence, modifying samples in-place.
    /// The aggregated result's `suppress` is the OR of every enabled
    /// processor's `suppress`.
    pub fn process(&mut self, samples: &mut [f32], sample_rate: u32) -> ProcessorResult {
        let mut result = ProcessorResult::default();

        for processor in &mut self.processors {
            if processor.is_enabled() {
                let r = processor.process(samples, sample_rate);
                if r.suppress {
                    result.suppress = true;
                }
            }
        }

        result
    }

    /// Reset all processors in the pipeline.
    pub fn reset(&mut self) {
        for processor in &mut self.processors {
            processor.reset();
        }
    }

    /// Get the configured frame size.
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Get the number of processors in the pipeline.
    pub fn len(&self) -> usize {
        self.processors.len()
    }

    /// Check if the pipeline is empty.
    pub fn is_empty(&self) -> bool {
        self.processors.is_empty()
    }

    /// Iterate over processors.
    pub fn iter(&self) -> impl Iterator<Item = &dyn AudioProcessor> {
        self.processors.iter().map(|p| p.as_ref())
    }

    /// Iterate over processors mutably.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Box<dyn AudioProcessor>> {
        self.processors.iter_mut()
    }

    /// Update pipeline from a new configuration.
    ///
    /// This rebuilds the processor list from scratch.
    pub fn update_config(&mut self, config: &PipelineConfig, registry: &ProcessorRegistry) -> Result<(), String> {
        let new_pipeline = Self::from_config(config, registry)?;
        self.processors = new_pipeline.processors;
        self.output_specs = new_pipeline.output_specs;
        self.frame_size = new_pipeline.frame_size;
        Ok(())
    }

    /// Collect every enabled processor's live outputs into `frame`.
    ///
    /// Walks processors in order, giving each a contiguous slot window
    /// sized to its declared outputs. Disabled stages keep their window
    /// (so slot indices stay stable across enable toggles) but get zeroed
    /// rather than queried. Slot assignment mirrors [`OutputLayout::derive`]
    /// exactly, so a reader that derived its layout from the same config
    /// addresses the right values.
    ///
    /// Cheap and allocation-free — safe to call from the audio callback
    /// right after [`process`](Self::process), while the same lock is held.
    pub fn write_outputs(&self, frame: &mut OutputFrame) {
        let mut offset = 0usize;
        for (processor, specs) in self.processors.iter().zip(self.output_specs.iter()) {
            if specs.is_empty() {
                continue;
            }
            let n = specs.len();
            if offset + n > MAX_OUTPUTS {
                // Tail dropped — the reader's OutputLayout flags this via
                // `truncated`; nothing useful to do here on the hot path.
                break;
            }
            let slots = &mut frame.values[offset..offset + n];
            slots.fill(0.0);
            if processor.is_enabled() {
                let mut sink = OutputSink::new(specs, slots);
                processor.write_outputs(&mut sink);
            }
            offset += n;
        }
        frame.len = offset as u8;
    }
}

// =============================================================================
// Utility Functions
// =============================================================================

/// Convert linear amplitude to decibels.
///
/// Returns -infinity for zero or negative values.
pub fn linear_to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * linear.log10()
    }
}

/// Convert decibels to linear amplitude.
pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Calculate RMS level of samples in decibels.
pub fn calculate_rms_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return f32::NEG_INFINITY;
    }

    let sum_squares: f32 = samples.iter().map(|&s| s * s).sum();
    let rms = (sum_squares / samples.len() as f32).sqrt();
    linear_to_db(rms)
}

/// Calculate peak level of samples in decibels.
pub fn calculate_peak_db(samples: &[f32]) -> f32 {
    let peak = samples.iter().map(|&s| s.abs()).fold(0.0f32, f32::max);
    linear_to_db(peak)
}

/// Threshold below which [`soft_clip`] is fully transparent.
pub const SOFT_CLIP_THRESHOLD: f32 = 0.75;

/// Soft-knee peak limiter. Transparent below [`SOFT_CLIP_THRESHOLD`] (so any
/// in-range signal is untouched), then smoothly compresses overshoot — the
/// curve is C¹-continuous at the knee (unit slope) and asymptotes to ±1
/// without ever reaching it. Use this instead of a per-sample hard clamp,
/// which flattens loud content into a square wave (audible crackle).
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= SOFT_CLIP_THRESHOLD {
        return x;
    }
    let over = a - SOFT_CLIP_THRESHOLD;
    let headroom = 1.0 - SOFT_CLIP_THRESHOLD;
    // over/(over+headroom) → 0 at the knee (slope 1) and → 1 as over → ∞.
    let limited = SOFT_CLIP_THRESHOLD + headroom * (over / (over + headroom));
    limited.copysign(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_processor_result_default() {
        let result = ProcessorResult::default();
        assert!(!result.suppress);
    }

    // A processor that declares `n` outputs and emits each output's index
    // as its value, so a test can assert exactly which slot got what.
    struct CountingProcessor {
        keys: Vec<String>,
        enabled: bool,
    }
    impl AudioProcessor for CountingProcessor {
        fn process(&mut self, _samples: &mut [f32], _sample_rate: u32) -> ProcessorResult {
            ProcessorResult::pass()
        }
        fn name(&self) -> &'static str {
            "counting"
        }
        fn config(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn set_config(&mut self, _config: &serde_json::Value) {}
        fn is_enabled(&self) -> bool {
            self.enabled
        }
        fn set_enabled(&mut self, enabled: bool) {
            self.enabled = enabled;
        }
        fn write_outputs(&self, sink: &mut OutputSink) {
            for (i, k) in self.keys.iter().enumerate() {
                sink.set(k, i as f32 + 1.0);
            }
        }
    }

    struct CountingFactory {
        type_id: &'static str,
        keys: Vec<&'static str>,
    }
    impl ProcessorFactory for CountingFactory {
        fn type_id(&self) -> &'static str {
            self.type_id
        }
        fn display_name(&self) -> &'static str {
            "Counting"
        }
        fn create_default(&self) -> Box<dyn AudioProcessor> {
            Box::new(CountingProcessor {
                keys: self.keys.iter().map(|k| k.to_string()).collect(),
                enabled: true,
            })
        }
        fn create_from_config(&self, _config: &serde_json::Value) -> Result<Box<dyn AudioProcessor>, String> {
            Ok(self.create_default())
        }
        fn settings_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn outputs(&self) -> Vec<OutputSpec> {
            self.keys
                .iter()
                .map(|k| OutputSpec {
                    key: k.to_string(),
                    title: k.to_string(),
                    kind: OutputKind::Indicator,
                })
                .collect()
        }
    }

    fn counting_registry() -> ProcessorRegistry {
        let mut reg = ProcessorRegistry::new();
        // "silent" declares no outputs and must be skipped in slot assignment.
        reg.register(Box::new(CountingFactory {
            type_id: "silent",
            keys: vec![],
        }));
        reg.register(Box::new(CountingFactory {
            type_id: "two",
            keys: vec!["a", "b"],
        }));
        reg.register(Box::new(CountingFactory {
            type_id: "one",
            keys: vec!["c"],
        }));
        reg
    }

    // The whole transport hinges on writer and reader agreeing on slots
    // without a shared map. Build a pipeline, write a frame, derive the
    // layout independently, and assert each entry reads back the value its
    // producer wrote into that slot.
    #[test]
    fn write_outputs_slots_match_derived_layout() {
        let reg = counting_registry();
        let config = PipelineConfig {
            processors: vec![
                ProcessorConfig::new("silent"),
                ProcessorConfig::new("two"),
                ProcessorConfig::new("one"),
            ],
            frame_size: 960,
        };

        let pipeline = AudioPipeline::from_config(&config, &reg).unwrap();
        let mut frame = OutputFrame::default();
        pipeline.write_outputs(&mut frame);

        let layout = OutputLayout::derive(&config, &reg);
        assert!(!layout.truncated);
        // silent (skipped) → two: slots 0,1 → one: slot 2.
        assert_eq!(layout.entries.len(), 3);
        assert_eq!(frame.len, 3);

        let by_key: std::collections::HashMap<&str, &OutputEntry> =
            layout.entries.iter().map(|e| (e.spec.key.as_str(), e)).collect();
        // CountingProcessor wrote (output index + 1) into each slot.
        assert_eq!(frame.get(by_key["a"].slot), Some(1.0));
        assert_eq!(frame.get(by_key["b"].slot), Some(2.0));
        assert_eq!(frame.get(by_key["c"].slot), Some(1.0));
        // The skipped silent stage produced no entries.
        assert!(layout.entries.iter().all(|e| e.type_id != "silent"));
    }

    // A disabled stage keeps its slot window (so indices stay stable across
    // enable toggles) but its values are zeroed rather than queried.
    #[test]
    fn write_outputs_zeroes_disabled_stage_but_keeps_slots() {
        let reg = counting_registry();
        let config = PipelineConfig {
            processors: vec![ProcessorConfig::new("two").enabled(false), ProcessorConfig::new("one")],
            frame_size: 960,
        };
        let pipeline = AudioPipeline::from_config(&config, &reg).unwrap();
        let mut frame = OutputFrame::default();
        pipeline.write_outputs(&mut frame);

        let layout = OutputLayout::derive(&config, &reg);
        let by_key: std::collections::HashMap<&str, &OutputEntry> =
            layout.entries.iter().map(|e| (e.spec.key.as_str(), e)).collect();
        // Disabled "two" still occupies slots 0,1 but reads back zero...
        assert_eq!(frame.get(by_key["a"].slot), Some(0.0));
        assert_eq!(frame.get(by_key["b"].slot), Some(0.0));
        // ...and the enabled "one" downstream still lands at slot 2.
        assert_eq!(by_key["c"].slot, 2);
        assert_eq!(frame.get(by_key["c"].slot), Some(1.0));
    }

    #[test]
    fn test_backfill_fills_missing_fields_and_preserves_present_ones() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "a": { "type": "boolean", "default": true },
                "b": { "type": "integer", "default": 7 },
                "c": { "type": "number" }, // no default — must stay missing
            },
        });
        let mut value = serde_json::json!({ "b": 1, "extra": "keep me" });
        backfill_settings_from_schema(&mut value, &schema);
        assert_eq!(value["a"], serde_json::json!(true));
        assert_eq!(
            value["b"],
            serde_json::json!(1),
            "present value must not be overwritten"
        );
        assert!(value.get("c").is_none(), "properties without a default stay missing");
        assert_eq!(value["extra"], serde_json::json!("keep me"));
    }

    #[test]
    fn test_backfill_replaces_non_object_value() {
        let schema = serde_json::json!({
            "properties": { "x": { "default": 42 } },
        });
        let mut value = serde_json::Value::Null;
        backfill_settings_from_schema(&mut value, &schema);
        assert_eq!(value["x"], serde_json::json!(42));
    }

    #[test]
    fn test_processor_config_new() {
        let config = ProcessorConfig::new("builtin.gain");
        assert_eq!(config.type_id, "builtin.gain");
        assert!(config.enabled);
    }

    #[test]
    fn test_pipeline_config_default() {
        let config = PipelineConfig::default();
        assert!(config.processors.is_empty());
        assert_eq!(config.frame_size, 960);
    }

    #[test]
    fn test_linear_to_db() {
        assert!((linear_to_db(1.0) - 0.0).abs() < 0.001);
        assert!((linear_to_db(0.5) - (-6.02)).abs() < 0.1);
        assert!(linear_to_db(0.0).is_infinite());
    }

    #[test]
    fn test_db_to_linear() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 0.001);
        assert!((db_to_linear(-6.0) - 0.501).abs() < 0.01);
    }

    #[test]
    fn test_calculate_rms_db() {
        // Full scale sine approximation
        let samples: Vec<f32> = (0..960).map(|i| (i as f32 * 0.1).sin()).collect();
        let rms = calculate_rms_db(&samples);
        assert!(rms > -10.0 && rms < 0.0);
    }

    #[test]
    fn test_registry_operations() {
        let registry = ProcessorRegistry::new();
        assert!(!registry.has("builtin.gain"));
        assert!(registry.list_available().is_empty());
    }
}
