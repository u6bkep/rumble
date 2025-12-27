//! RNNoise-based noise suppression processor.
//!
//! This processor uses the nnnoiseless library (a Rust port of RNNoise) to
//! remove background noise from audio. It's particularly effective at
//! removing constant background noise like fans, air conditioning, and
//! keyboard sounds.
//!
//! Note: RNNoise operates on 10ms frames (480 samples at 48kHz), so this
//! processor buffers internally to handle arbitrary frame sizes.

use nnnoiseless::DenoiseState;
use pipeline::{AudioProcessor, ProcessorFactory, ProcessorResult};
use serde::{Deserialize, Serialize};

use super::type_ids;

/// Frame size used by nnnoiseless (480 samples = 10ms at 48kHz).
const DENOISE_FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;

/// Settings for the denoise processor.
///
/// Currently nnnoiseless has no configurable parameters, but we include
/// this struct for consistency and future extensibility.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DenoiseSettings {
    // No configurable parameters currently
    // Future: could add strength, attenuation floor, etc.
}

/// RNNoise-based noise suppression processor.
///
/// Uses the nnnoiseless library to suppress background noise. The processor
/// internally buffers audio to process in 10ms chunks as required by RNNoise.
///
/// Note: The first 10ms frame after reset is discarded due to RNNoise
/// fade-in artifacts.
pub struct DenoiseProcessor {
    /// RNNoise denoiser state.
    denoise_state: Box<DenoiseState<'static>>,
    /// Buffer for incomplete input frames.
    input_buffer: Vec<f32>,
    /// Buffer for denoised output waiting to be returned.
    output_buffer: Vec<f32>,
    /// Whether we've processed the first frame (which is discarded).
    first_frame_done: bool,
    /// Settings (currently unused but kept for consistency).
    settings: DenoiseSettings,
    /// Whether the processor is enabled.
    enabled: bool,
}

impl DenoiseProcessor {
    /// Create a new denoise processor.
    pub fn new() -> Self {
        Self {
            denoise_state: DenoiseState::new(),
            input_buffer: Vec::with_capacity(DENOISE_FRAME_SIZE * 2),
            output_buffer: Vec::with_capacity(DENOISE_FRAME_SIZE * 4),
            first_frame_done: false,
            settings: DenoiseSettings::default(),
            enabled: true,
        }
    }
}

impl Default for DenoiseProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioProcessor for DenoiseProcessor {
    fn process(&mut self, samples: &mut [f32], _sample_rate: u32) -> ProcessorResult {
        // Add incoming samples to input buffer
        self.input_buffer.extend_from_slice(samples);

        // Process complete 480-sample chunks
        while self.input_buffer.len() >= DENOISE_FRAME_SIZE {
            // Extract a chunk
            let chunk: Vec<f32> = self.input_buffer.drain(..DENOISE_FRAME_SIZE).collect();

            // Scale to i16 range for nnnoiseless (it expects samples in [-32768, 32767])
            let scaled: Vec<f32> = chunk.iter().map(|&s| s * 32767.0).collect();

            // Process through RNNoise
            let mut out_buf = [0.0f32; DENOISE_FRAME_SIZE];
            self.denoise_state.process_frame(&mut out_buf, &scaled);

            // Skip the first frame due to fade-in artifacts
            if !self.first_frame_done {
                self.first_frame_done = true;
                // Fill with zeros to maintain timing
                self.output_buffer.extend(std::iter::repeat(0.0).take(DENOISE_FRAME_SIZE));
                continue;
            }

            // Scale back to [-1.0, 1.0] and add to output buffer
            for &sample in &out_buf {
                self.output_buffer.push(sample / 32767.0);
            }
        }

        // Copy processed samples back to the input slice
        // We need to handle the case where output buffer might be smaller than input
        // due to the first-frame discard
        let copy_len = samples.len().min(self.output_buffer.len());
        if copy_len > 0 {
            let output: Vec<f32> = self.output_buffer.drain(..copy_len).collect();
            samples[..copy_len].copy_from_slice(&output);
        }

        // Zero any remaining samples if we don't have enough output
        if copy_len < samples.len() {
            samples[copy_len..].fill(0.0);
        }

        ProcessorResult::pass()
    }

    fn name(&self) -> &'static str {
        "Denoise (RNNoise)"
    }

    fn reset(&mut self) {
        // Reset the denoiser state
        self.denoise_state = DenoiseState::new();
        self.input_buffer.clear();
        self.output_buffer.clear();
        self.first_frame_done = false;
    }

    fn config(&self) -> serde_json::Value {
        serde_json::to_value(&self.settings).unwrap_or_default()
    }

    fn set_config(&mut self, config: &serde_json::Value) {
        if let Ok(settings) = serde_json::from_value::<DenoiseSettings>(config.clone()) {
            self.settings = settings;
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if enabled {
            // Reset state when re-enabled
            self.reset();
        }
    }
}

/// Factory for creating DenoiseProcessor instances.
pub struct DenoiseProcessorFactory;

impl ProcessorFactory for DenoiseProcessorFactory {
    fn type_id(&self) -> &'static str {
        type_ids::DENOISE
    }

    fn display_name(&self) -> &'static str {
        "Denoise (RNNoise)"
    }

    fn create_default(&self) -> Box<dyn AudioProcessor> {
        Box::new(DenoiseProcessor::new())
    }

    fn create_from_config(&self, config: &serde_json::Value) -> Result<Box<dyn AudioProcessor>, String> {
        // Currently no configurable parameters
        let _settings: DenoiseSettings = serde_json::from_value(config.clone())
            .unwrap_or_default();
        
        Ok(Box::new(DenoiseProcessor::new()))
    }

    fn settings_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "description": "RNNoise has no configurable parameters"
        })
    }

    fn description(&self) -> &'static str {
        "AI-powered noise suppression using RNNoise neural network"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_denoise_creation() {
        let processor = DenoiseProcessor::new();
        assert!(processor.is_enabled());
        assert_eq!(processor.name(), "Denoise (RNNoise)");
    }

    #[test]
    fn test_denoise_process_single_frame() {
        let mut processor = DenoiseProcessor::new();
        
        // Create a 960-sample frame (20ms at 48kHz)
        let mut samples = vec![0.1f32; 960];
        
        // Process - first 480 samples will be zeroed due to first-frame discard
        let result = processor.process(&mut samples, 48000);
        
        assert!(!result.suppress);
        // First chunk should be zeroed
        assert!(samples[..480].iter().all(|&s| s.abs() < 0.001));
    }

    #[test]
    fn test_denoise_reset() {
        let mut processor = DenoiseProcessor::new();
        
        // Process some audio
        let mut samples = vec![0.5f32; 960];
        processor.process(&mut samples, 48000);
        
        // Reset
        processor.reset();
        
        // Buffer should be cleared
        assert!(processor.input_buffer.is_empty());
        assert!(processor.output_buffer.is_empty());
        assert!(!processor.first_frame_done);
    }

    #[test]
    fn test_factory_create() {
        let factory = DenoiseProcessorFactory;
        let config = serde_json::json!({});
        
        let processor = factory.create_from_config(&config).unwrap();
        assert_eq!(processor.name(), "Denoise (RNNoise)");
    }
}
