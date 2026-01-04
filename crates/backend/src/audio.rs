//! Audio capture and playback using cpal.
//!
//! This module provides audio device enumeration, input capture, and output playback
//! for voice communication. Audio samples are captured as f32 PCM and should be
//! encoded with Opus before transmission (see the `codec` module).

use cpal::{
    Device, Host, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tracing::{debug, error, info, warn};

/// Audio sample rate used for voice communication.
/// 48kHz is the native rate for Opus codec.
pub const SAMPLE_RATE: u32 = 48000;

/// Number of channels (mono for voice).
pub const CHANNELS: u16 = 1;

/// Frame size in samples for Opus encoding.
///
/// At 48kHz, common Opus frame sizes are:
/// - 120 samples (2.5ms)
/// - 240 samples (5ms)
/// - 480 samples (10ms)
/// - 960 samples (20ms) <- **default, good balance**
/// - 1920 samples (40ms)
/// - 2880 samples (60ms)
///
/// 20ms (960 samples) is recommended for VoIP as it provides a good
/// balance between latency and compression efficiency.
pub const FRAME_SIZE: usize = 960;

/// Information about an audio device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    /// Stable identifier for the device (used for selection).
    /// This is the device name, which should be stable across sessions.
    pub id: String,
    /// Device name for display (same as id for now, but could differ).
    pub name: String,
    /// Whether this is the default device.
    pub is_default: bool,
}

/// Get device name using the new API, falling back gracefully.
fn get_device_name(device: &Device) -> Option<String> {
    // Try the new description() API first, fall back to deprecated name()
    // #[allow(deprecated)]
    // device.name().ok()
    device
        .description()
        .map(|desc| desc.name().to_string())
        .ok()
}

/// Audio subsystem handle.
pub struct AudioSystem {
    host: Host,
}

impl Default for AudioSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSystem {
    /// Create a new audio system using the default host.
    pub fn new() -> Self {
        let host = cpal::default_host();
        info!(host_id = ?host.id(), "audio: initialized audio host");
        Self { host }
    }

    /// List available input (microphone) devices.
    pub fn list_input_devices(&self) -> Vec<AudioDeviceInfo> {
        let default_name = self
            .host
            .default_input_device()
            .and_then(|d| get_device_name(&d));

        self.host
            .input_devices()
            .map(|devices| {
                devices
                    .filter_map(|device| {
                        let name = get_device_name(&device)?;
                        let is_default = default_name.as_ref() == Some(&name);
                        Some(AudioDeviceInfo {
                            id: name.clone(),
                            name,
                            is_default,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List available output (speaker/headphone) devices.
    pub fn list_output_devices(&self) -> Vec<AudioDeviceInfo> {
        let default_name = self
            .host
            .default_output_device()
            .and_then(|d| get_device_name(&d));

        self.host
            .output_devices()
            .map(|devices| {
                devices
                    .filter_map(|device| {
                        let name = get_device_name(&device)?;
                        let is_default = default_name.as_ref() == Some(&name);
                        Some(AudioDeviceInfo {
                            id: name.clone(),
                            name,
                            is_default,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get the default input device.
    pub fn default_input_device(&self) -> Option<Device> {
        self.host.default_input_device()
    }

    /// Get the default output device.
    pub fn default_output_device(&self) -> Option<Device> {
        self.host.default_output_device()
    }

    /// Get an input device by its ID (device name).
    pub fn get_input_device_by_id(&self, id: &str) -> Option<Device> {
        self.host
            .input_devices()
            .ok()?
            .find(|d| get_device_name(d).as_deref() == Some(id))
    }

    /// Get an output device by its ID (device name).
    pub fn get_output_device_by_id(&self, id: &str) -> Option<Device> {
        self.host
            .output_devices()
            .ok()?
            .find(|d| get_device_name(d).as_deref() == Some(id))
    }
}

/// Audio configuration for capture/playback.
#[derive(Clone, Debug)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_size: usize,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            frame_size: FRAME_SIZE,
        }
    }
}

/// Returns a preference score for a sample format (higher is better).
/// F32 is preferred as it's our internal format, followed by I16 which is common.
fn sample_format_preference(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 5, // Best - native format, no conversion needed
        SampleFormat::I16 => 4, // Good - common format, simple conversion
        SampleFormat::I32 => 3, // OK - higher precision but needs scaling
        SampleFormat::U16 => 2, // Less common, needs offset conversion
        SampleFormat::U8 => 1,  // Low quality, needs offset conversion
        _ => 0,                 // Unknown/unsupported formats
    }
}

/// Processes audio input samples, accumulating them into frames.
///
/// This struct handles:
/// - Accumulating samples until we have enough for a frame
/// - Outputting frames (typically 960 samples / 20ms at 48kHz) for processing
///
/// Note: Audio processing (denoise, VAD, etc.) is handled by the pipeline,
/// not by this low-level input processor.
struct InputProcessor<F: FnMut(&[f32])> {
    /// Buffer to accumulate incoming samples.
    sample_buffer: Vec<f32>,
    /// Output frame size (typically 960 for Opus).
    frame_size: usize,
    /// Callback for completed frames.
    on_frame: F,
}

impl<F: FnMut(&[f32])> InputProcessor<F> {
    fn new(frame_size: usize, on_frame: F) -> Self {
        Self {
            sample_buffer: Vec::with_capacity(frame_size * 2),
            frame_size,
            on_frame,
        }
    }

    /// Process incoming audio samples (in [-1.0, 1.0] range).
    fn process_samples(&mut self, samples: &[f32]) {
        self.sample_buffer.extend_from_slice(samples);

        // Output full frames
        while self.sample_buffer.len() >= self.frame_size {
            let frame: Vec<f32> = self.sample_buffer.drain(..self.frame_size).collect();
            (self.on_frame)(&frame);
        }
    }
}

/// Handle for audio input (microphone capture).
pub struct AudioInput {
    stream: Stream,
    is_capturing: Arc<AtomicBool>,
}

impl AudioInput {
    /// Create and start an audio input stream.
    ///
    /// # Arguments
    /// * `device` - The input device to use
    /// * `config` - Audio configuration
    /// * `on_frame` - Callback invoked with each audio frame (FRAME_SIZE samples)
    pub fn new<F>(device: &Device, config: &AudioConfig, on_frame: F) -> Result<Self, String>
    where
        F: FnMut(&[f32]) + Send + 'static,
    {
        // Use a fixed buffer size matching our frame size for lower latency
        // and more predictable timing. 960 samples = 20ms at 48kHz.
        let stream_config = StreamConfig {
            channels: config.channels,
            sample_rate: config.sample_rate,
            buffer_size: cpal::BufferSize::Fixed(config.frame_size as u32),
        };

        let is_capturing = Arc::new(AtomicBool::new(true));

        let frame_size = config.frame_size;

        let err_fn = |err| error!("audio input error: {}", err);

        // Check supported formats and pick the best one based on preference
        let supported = device
            .supported_input_configs()
            .map_err(|e| format!("Failed to get supported input configs: {}", e))?;

        let format = supported
            .filter(|c| c.channels() == config.channels)
            .filter(|c| {
                c.min_sample_rate() <= config.sample_rate
                    && c.max_sample_rate() >= config.sample_rate
            })
            .max_by_key(|c| sample_format_preference(c.sample_format()));

        let stream = if let Some(supported_config) = format {
            let sample_format = supported_config.sample_format();
            debug!(sample_format = ?sample_format, "audio: using supported format");

            // TODO: make this generic over sample formats. see https://github.com/RustAudio/cpal/blob/5d571761f09474dc85a45639fe06b8ead40104b1/examples/beep.rs#L91
            match sample_format {
                SampleFormat::F32 => {
                    let is_capturing_clone = is_capturing.clone();
                    let processor = Arc::new(Mutex::new(InputProcessor::new(
                        frame_size,
                        on_frame,
                    )));
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            if !is_capturing_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            if let Ok(mut proc) = processor.lock() {
                                proc.process_samples(data);
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                SampleFormat::I16 => {
                    let is_capturing_clone = is_capturing.clone();
                    let processor = Arc::new(Mutex::new(InputProcessor::new(
                        frame_size,
                        on_frame,
                    )));
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[i16], _: &cpal::InputCallbackInfo| {
                            if !is_capturing_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            // Convert i16 to f32
                            let float_data: Vec<f32> =
                                data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();

                            if let Ok(mut proc) = processor.lock() {
                                proc.process_samples(&float_data);
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                SampleFormat::U16 => {
                    let is_capturing_clone = is_capturing.clone();
                    let processor = Arc::new(Mutex::new(InputProcessor::new(
                        frame_size,
                        on_frame,
                    )));
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[u16], _: &cpal::InputCallbackInfo| {
                            if !is_capturing_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            // Convert u16 to f32
                            let float_data: Vec<f32> = data
                                .iter()
                                .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
                                .collect();

                            if let Ok(mut proc) = processor.lock() {
                                proc.process_samples(&float_data);
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                SampleFormat::U8 => {
                    let is_capturing_clone = is_capturing.clone();
                    let processor = Arc::new(Mutex::new(InputProcessor::new(
                        frame_size,
                        on_frame,
                    )));
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[u8], _: &cpal::InputCallbackInfo| {
                            if !is_capturing_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            // Convert u8 to f32 (128 is silence)
                            let float_data: Vec<f32> =
                                data.iter().map(|&s| (s as f32 - 128.0) / 128.0).collect();

                            if let Ok(mut proc) = processor.lock() {
                                proc.process_samples(&float_data);
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                _ => return Err(format!("Unsupported sample format: {:?}", sample_format)),
            }
        } else {
            // Try default f32 format
            warn!("audio: no matching format found, trying f32 default");
            let is_capturing_clone = is_capturing.clone();
            let processor = Arc::new(Mutex::new(InputProcessor::new(
                frame_size,
                on_frame,
            )));
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    if !is_capturing_clone.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Ok(mut proc) = processor.lock() {
                        proc.process_samples(data);
                    }
                },
                err_fn,
                None,
            )
        }
        .map_err(|e| format!("Failed to build input stream: {}", e))?;

        stream
            .play()
            .map_err(|e| format!("Failed to start input stream: {}", e))?;
        info!("audio: input stream started");

        Ok(Self {
            stream,
            is_capturing,
        })
    }

    /// Pause audio capture.
    pub fn pause(&self) {
        self.is_capturing.store(false, Ordering::Relaxed);
        let _ = self.stream.pause();
    }

    /// Resume audio capture.
    pub fn resume(&self) {
        self.is_capturing.store(true, Ordering::Relaxed);
        let _ = self.stream.play();
    }

    /// Check if currently capturing.
    pub fn is_capturing(&self) -> bool {
        self.is_capturing.load(Ordering::Relaxed)
    }
}

/// Maximum number of samples to buffer for playback before dropping old samples.
/// At 48kHz mono, this controls the latency/jitter tradeoff:
/// - 4800 samples = 100ms (good for low-latency local networks)
/// - 9600 samples = 200ms (reasonable for typical internet)
/// - 14400 samples = 300ms (handles more jitter but adds delay)
///
/// We use 200ms as a balance between low latency and handling network jitter.
pub const MAX_PLAYBACK_BUFFER_SAMPLES: usize = 9600;

/// Handle for audio output (speaker playback).
pub struct AudioOutput {
    stream: Stream,
    is_playing: Arc<AtomicBool>,
    /// Ring buffer for samples to play (front = oldest, back = newest).
    /// Samples are pushed to back, popped from front.
    playback_buffer: Arc<Mutex<VecDeque<f32>>>,
    /// Maximum buffer size before dropping old samples.
    max_buffer_size: usize,
    /// Counter for dropped samples (for statistics/debugging).
    dropped_samples: Arc<std::sync::atomic::AtomicUsize>,
    /// Counter for underrun samples (buffer was empty when we needed samples).
    underrun_samples: Arc<std::sync::atomic::AtomicUsize>,
}

impl AudioOutput {
    /// Create and start an audio output stream.
    ///
    /// # Arguments
    /// * `device` - The output device to use
    /// * `config` - Audio configuration
    pub fn new(device: &Device, config: &AudioConfig) -> Result<Self, String> {
        // Use a fixed buffer size matching our frame size for lower latency
        // and more predictable timing. 960 samples = 20ms at 48kHz.
        let stream_config = StreamConfig {
            channels: config.channels,
            sample_rate: config.sample_rate,
            buffer_size: cpal::BufferSize::Fixed(config.frame_size as u32),
        };

        let is_playing = Arc::new(AtomicBool::new(true));
        let playback_buffer = Arc::new(Mutex::new(VecDeque::with_capacity(config.frame_size * 10)));
        let underrun_samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let is_playing_clone = is_playing.clone();
        let playback_buffer_clone = playback_buffer.clone();
        let underrun_samples_clone = underrun_samples.clone();

        let err_fn = |err| error!("audio output error: {}", err);

        // Check supported formats and pick the best one based on preference
        let supported = device
            .supported_output_configs()
            .map_err(|e| format!("Failed to get supported output configs: {}", e))?;

        let format = supported
            .filter(|c| c.channels() == config.channels)
            .filter(|c| {
                c.min_sample_rate() <= config.sample_rate
                    && c.max_sample_rate() >= config.sample_rate
            })
            .max_by_key(|c| sample_format_preference(c.sample_format()));

        let stream = if let Some(supported_config) = format {
            let sample_format = supported_config.sample_format();
            debug!(sample_format = ?sample_format, "audio: using output format");

            match sample_format {
                SampleFormat::F32 => {
                    // Track underrun state for logging transitions
                    let in_underrun = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)); // Start in underrun (no audio yet)
                    let in_underrun_clone = in_underrun.clone();
                    
                    device.build_output_stream(
                        &stream_config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            if !is_playing_clone.load(Ordering::Relaxed) {
                                data.fill(0.0);
                                return;
                            }
                            let mut buffer = playback_buffer_clone.lock().unwrap();
                            let buffer_len_before = buffer.len();
                            let was_in_underrun = in_underrun_clone.load(Ordering::Relaxed);
                            let mut samples_played = 0usize;
                            let mut underrun_samples_this_callback = 0usize;
                            
                            for sample in data.iter_mut() {
                                // pop_front = oldest sample first (FIFO)
                                if let Some(s) = buffer.pop_front() {
                                    *sample = s;
                                    samples_played += 1;
                                } else {
                                    *sample = 0.0;
                                    underrun_samples_this_callback += 1;
                                }
                            }
                            
                            // Track underrun statistics
                            if underrun_samples_this_callback > 0 {
                                underrun_samples_clone.fetch_add(underrun_samples_this_callback, Ordering::Relaxed);
                            }
                            
                            // Log state transitions (useful for debugging audio issues)
                            let now_in_underrun = underrun_samples_this_callback > 0;
                            if now_in_underrun && !was_in_underrun {
                                // Entered underrun state - buffer ran dry
                                debug!(
                                    "audio: ENTER underrun - buffer had {} samples, played {}, missing {}",
                                    buffer_len_before, samples_played, underrun_samples_this_callback
                                );
                            } else if !now_in_underrun && was_in_underrun && samples_played > 0 {
                                // Recovered from underrun - buffer has audio again
                                debug!(
                                    "audio: EXIT underrun - buffer now has {} samples after playing {}",
                                    buffer.len(), samples_played
                                );
                            }
                            
                            in_underrun_clone.store(now_in_underrun, Ordering::Relaxed);
                        },
                        err_fn,
                        None,
                    )
                }
                SampleFormat::I16 => device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        if !is_playing_clone.load(Ordering::Relaxed) {
                            data.fill(0);
                            return;
                        }
                        let mut buffer = playback_buffer_clone.lock().unwrap();
                        for sample in data.iter_mut() {
                            let f = buffer.pop_front().unwrap_or(0.0);
                            *sample = (f * i16::MAX as f32) as i16;
                        }
                    },
                    err_fn,
                    None,
                ),
                SampleFormat::U16 => device.build_output_stream(
                    &stream_config,
                    move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                        if !is_playing_clone.load(Ordering::Relaxed) {
                            data.fill(u16::MAX / 2);
                            return;
                        }
                        let mut buffer = playback_buffer_clone.lock().unwrap();
                        for sample in data.iter_mut() {
                            let f = buffer.pop_front().unwrap_or(0.0);
                            *sample = ((f + 1.0) / 2.0 * u16::MAX as f32) as u16;
                        }
                    },
                    err_fn,
                    None,
                ),
                SampleFormat::U8 => {
                    device.build_output_stream(
                        &stream_config,
                        move |data: &mut [u8], _: &cpal::OutputCallbackInfo| {
                            if !is_playing_clone.load(Ordering::Relaxed) {
                                data.fill(128); // Silence for U8
                                return;
                            }
                            let mut buffer = playback_buffer_clone.lock().unwrap();
                            for sample in data.iter_mut() {
                                let f = buffer.pop_front().unwrap_or(0.0);
                                // Convert f32 [-1, 1] to u8 [0, 255] with 128 as center
                                *sample = ((f * 128.0) + 128.0).clamp(0.0, 255.0) as u8;
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                _ => return Err(format!("Unsupported sample format: {:?}", sample_format)),
            }
        } else {
            // Try default f32 format
            warn!("audio: no matching output format found, trying f32 default");
            device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !is_playing_clone.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }
                    let mut buffer = playback_buffer_clone.lock().unwrap();
                    for sample in data.iter_mut() {
                        *sample = buffer.pop_front().unwrap_or(0.0);
                    }
                },
                err_fn,
                None,
            )
        }
        .map_err(|e| format!("Failed to build output stream: {}", e))?;

        stream
            .play()
            .map_err(|e| format!("Failed to start output stream: {}", e))?;
        info!("audio: output stream started");

        Ok(Self {
            stream,
            is_playing,
            playback_buffer,
            max_buffer_size: MAX_PLAYBACK_BUFFER_SAMPLES,
            dropped_samples: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            underrun_samples,
        })
    }

    /// Create an audio output stream with a custom max buffer size.
    ///
    /// Use this if you need to tune the latency/drop tradeoff.
    /// A larger buffer allows more network jitter but increases latency.
    /// A smaller buffer reduces latency but drops more samples on jitter.
    pub fn with_max_buffer(
        device: &Device,
        config: &AudioConfig,
        max_buffer_size: usize,
    ) -> Result<Self, String> {
        let mut output = Self::new(device, config)?;
        output.max_buffer_size = max_buffer_size;
        Ok(output)
    }

    /// Queue audio samples for playback.
    ///
    /// Samples are added to the back of the buffer (newest).
    /// Playback reads from the front (oldest) - FIFO order.
    /// If the buffer would exceed `max_buffer_size`, oldest samples are dropped.
    pub fn queue_samples(&self, samples: &[f32]) {
        let mut buffer = self.playback_buffer.lock().unwrap();

        // Add new samples to the back
        buffer.extend(samples.iter().copied());

        // If we exceeded the max size, drop oldest samples from the front
        if buffer.len() > self.max_buffer_size {
            let samples_to_drop = buffer.len() - self.max_buffer_size;
            buffer.drain(..samples_to_drop);
            self.dropped_samples
                .fetch_add(samples_to_drop, Ordering::Relaxed);

            debug!(
                dropped = samples_to_drop,
                buffer_size = buffer.len(),
                "audio: dropped old samples to maintain buffer limit"
            );
        }
    }

    /// Get the number of samples that have been dropped.
    pub fn dropped_samples(&self) -> usize {
        self.dropped_samples.load(Ordering::Relaxed)
    }

    /// Get the number of underrun samples (buffer was empty when playback needed samples).
    pub fn underrun_samples(&self) -> usize {
        self.underrun_samples.load(Ordering::Relaxed)
    }

    /// Get the number of samples currently buffered.
    pub fn buffered_samples(&self) -> usize {
        self.playback_buffer.lock().unwrap().len()
    }

    /// Pause audio playback.
    pub fn pause(&self) {
        self.is_playing.store(false, Ordering::Relaxed);
        let _ = self.stream.pause();
    }

    /// Resume audio playback.
    pub fn resume(&self) {
        self.is_playing.store(true, Ordering::Relaxed);
        let _ = self.stream.play();
    }

    /// Check if currently playing.
    pub fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::Relaxed)
    }

    /// Clear the playback buffer.
    pub fn clear_buffer(&self) {
        self.playback_buffer.lock().unwrap().clear();
    }
}

/// Convert f32 samples to bytes for transmission.
pub fn samples_to_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|&s| s.to_le_bytes()).collect()
}

/// Convert bytes back to f32 samples.
pub fn bytes_to_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| {
            let arr: [u8; 4] = chunk.try_into().unwrap();
            f32::from_le_bytes(arr)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_samples_bytes_roundtrip() {
        let samples: Vec<f32> = vec![0.0, 0.5, -0.5, 1.0, -1.0];
        let bytes = samples_to_bytes(&samples);
        let recovered = bytes_to_samples(&bytes);
        assert_eq!(samples, recovered);
    }

    #[test]
    fn test_audio_system_creation() {
        let system = AudioSystem::new();
        // Just verify we can list devices without panicking
        let _inputs = system.list_input_devices();
        let _outputs = system.list_output_devices();
    }
}
