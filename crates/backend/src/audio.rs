//! Audio capture and playback using cpal.
//!
//! This module provides audio device enumeration, input capture, and output playback
//! for voice communication. Raw audio samples are used (no encoding yet).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, SampleFormat, Stream, StreamConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

/// Audio sample rate used for voice communication.
/// 48kHz is the native rate for Opus codec.
pub const SAMPLE_RATE: u32 = 48000;

/// Number of channels (mono for voice).
pub const CHANNELS: u16 = 1;

/// Frame size in samples.
/// For raw audio over QUIC datagrams (max ~1200 bytes), we use 240 samples (5ms).
/// This gives us 240 * 4 = 960 bytes of audio data per frame.
/// When Opus encoding is added, this can be increased to 960 samples (20ms).
pub const FRAME_SIZE: usize = 240;

/// Information about an audio device.
#[derive(Clone, Debug)]
pub struct AudioDeviceInfo {
    /// Device name for display.
    pub name: String,
    /// Internal index for device selection.
    pub index: usize,
    /// Whether this is the default device.
    pub is_default: bool,
}

/// Get device name using the new API, falling back gracefully.
fn get_device_name(device: &Device) -> Option<String> {
    // Try the new description() API first, fall back to deprecated name()
    // #[allow(deprecated)]
    // device.name().ok()
    device.description().map(|desc| desc.name().to_string()).ok()
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
        let default_name = self.host
            .default_input_device()
            .and_then(|d| get_device_name(&d));

        self.host
            .input_devices()
            .map(|devices| {
                devices
                    .enumerate()
                    .filter_map(|(index, device)| {
                        let name = get_device_name(&device)?;
                        let is_default = default_name.as_ref() == Some(&name);
                        Some(AudioDeviceInfo { name, index, is_default })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List available output (speaker/headphone) devices.
    pub fn list_output_devices(&self) -> Vec<AudioDeviceInfo> {
        let default_name = self.host
            .default_output_device()
            .and_then(|d| get_device_name(&d));

        self.host
            .output_devices()
            .map(|devices| {
                devices
                    .enumerate()
                    .filter_map(|(index, device)| {
                        let name = get_device_name(&device)?;
                        let is_default = default_name.as_ref() == Some(&name);
                        Some(AudioDeviceInfo { name, index, is_default })
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

    /// Get an input device by index.
    pub fn get_input_device(&self, index: usize) -> Option<Device> {
        self.host.input_devices().ok()?.nth(index)
    }

    /// Get an output device by index.
    pub fn get_output_device(&self, index: usize) -> Option<Device> {
        self.host.output_devices().ok()?.nth(index)
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

/// Handle for audio input (microphone capture).
pub struct AudioInput {
    stream: Stream,
    is_capturing: Arc<AtomicBool>,
    /// Collected samples buffer, filled by the audio callback.
    #[allow(dead_code)]
    samples_buffer: Arc<Mutex<Vec<f32>>>,
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
        let stream_config = StreamConfig {
            channels: config.channels,
            sample_rate: config.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let is_capturing = Arc::new(AtomicBool::new(true));
        let samples_buffer = Arc::new(Mutex::new(Vec::with_capacity(config.frame_size * 2)));
        
        let is_capturing_clone = is_capturing.clone();
        let samples_buffer_clone = samples_buffer.clone();
        let frame_size = config.frame_size;
        let on_frame = Arc::new(Mutex::new(on_frame));

        let err_fn = |err| error!("audio input error: {}", err);

        // Check supported formats and pick the best one
        let supported = device.supported_input_configs()
            .map_err(|e| format!("Failed to get supported input configs: {}", e))?;
        
        let format = supported
            .filter(|c| c.channels() == config.channels)
            .filter(|c| c.min_sample_rate() <= config.sample_rate && c.max_sample_rate() >= config.sample_rate)
            .next();

        let stream = if let Some(supported_config) = format {
            let sample_format = supported_config.sample_format();
            debug!(sample_format = ?sample_format, "audio: using supported format");
            
            // TODO: make this generic over sample formats. see https://github.com/RustAudio/cpal/blob/5d571761f09474dc85a45639fe06b8ead40104b1/examples/beep.rs#L91
            match sample_format {
                SampleFormat::F32 => {
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[f32], _: &cpal::InputCallbackInfo| {
                            if !is_capturing_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            let mut buffer = samples_buffer_clone.lock().unwrap();
                            buffer.extend_from_slice(data);
                            
                            // When we have enough samples, call the callback
                            while buffer.len() >= frame_size {
                                let frame: Vec<f32> = buffer.drain(..frame_size).collect();
                                if let Ok(mut cb) = on_frame.lock() {
                                    cb(&frame);
                                }
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                SampleFormat::I16 => {
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[i16], _: &cpal::InputCallbackInfo| {
                            if !is_capturing_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            // Convert i16 to f32
                            let float_data: Vec<f32> = data.iter()
                                .map(|&s| s as f32 / i16::MAX as f32)
                                .collect();
                            
                            let mut buffer = samples_buffer_clone.lock().unwrap();
                            buffer.extend_from_slice(&float_data);
                            
                            while buffer.len() >= frame_size {
                                let frame: Vec<f32> = buffer.drain(..frame_size).collect();
                                if let Ok(mut cb) = on_frame.lock() {
                                    cb(&frame);
                                }
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                SampleFormat::U16 => {
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[u16], _: &cpal::InputCallbackInfo| {
                            if !is_capturing_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            // Convert u16 to f32
                            let float_data: Vec<f32> = data.iter()
                                .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
                                .collect();
                            
                            let mut buffer = samples_buffer_clone.lock().unwrap();
                            buffer.extend_from_slice(&float_data);
                            
                            while buffer.len() >= frame_size {
                                let frame: Vec<f32> = buffer.drain(..frame_size).collect();
                                if let Ok(mut cb) = on_frame.lock() {
                                    cb(&frame);
                                }
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                SampleFormat::U8 => {
                    device.build_input_stream(
                        &stream_config,
                        move |data: &[u8], _: &cpal::InputCallbackInfo| {
                            if !is_capturing_clone.load(Ordering::Relaxed) {
                                return;
                            }
                            // Convert u8 to f32 (128 is silence)
                            let float_data: Vec<f32> = data.iter()
                                .map(|&s| (s as f32 - 128.0) / 128.0)
                                .collect();
                            
                            let mut buffer = samples_buffer_clone.lock().unwrap();
                            buffer.extend_from_slice(&float_data);
                            
                            while buffer.len() >= frame_size {
                                let frame: Vec<f32> = buffer.drain(..frame_size).collect();
                                if let Ok(mut cb) = on_frame.lock() {
                                    cb(&frame);
                                }
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
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    if !is_capturing_clone.load(Ordering::Relaxed) {
                        return;
                    }
                    let mut buffer = samples_buffer_clone.lock().unwrap();
                    buffer.extend_from_slice(data);
                    
                    while buffer.len() >= frame_size {
                        let frame: Vec<f32> = buffer.drain(..frame_size).collect();
                        if let Ok(mut cb) = on_frame.lock() {
                            cb(&frame);
                        }
                    }
                },
                err_fn,
                None,
            )
        }.map_err(|e| format!("Failed to build input stream: {}", e))?;

        stream.play().map_err(|e| format!("Failed to start input stream: {}", e))?;
        info!("audio: input stream started");

        Ok(Self {
            stream,
            is_capturing,
            samples_buffer,
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

/// Handle for audio output (speaker playback).
pub struct AudioOutput {
    stream: Stream,
    is_playing: Arc<AtomicBool>,
    /// Ring buffer for samples to play.
    playback_buffer: Arc<Mutex<Vec<f32>>>,
}

impl AudioOutput {
    /// Create and start an audio output stream.
    /// 
    /// # Arguments
    /// * `device` - The output device to use
    /// * `config` - Audio configuration
    pub fn new(device: &Device, config: &AudioConfig) -> Result<Self, String> {
        let stream_config = StreamConfig {
            channels: config.channels,
            sample_rate: config.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let is_playing = Arc::new(AtomicBool::new(true));
        let playback_buffer = Arc::new(Mutex::new(Vec::with_capacity(config.frame_size * 10)));
        
        let is_playing_clone = is_playing.clone();
        let playback_buffer_clone = playback_buffer.clone();

        let err_fn = |err| error!("audio output error: {}", err);

        // Check supported formats
        let supported = device.supported_output_configs()
            .map_err(|e| format!("Failed to get supported output configs: {}", e))?;
        
        let format = supported
            .filter(|c| c.channels() == config.channels)
            .filter(|c| c.min_sample_rate() <= config.sample_rate && c.max_sample_rate() >= config.sample_rate)
            .next();

        let stream = if let Some(supported_config) = format {
            let sample_format = supported_config.sample_format();
            debug!(sample_format = ?sample_format, "audio: using output format");
            
            match sample_format {
                SampleFormat::F32 => {
                    device.build_output_stream(
                        &stream_config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            if !is_playing_clone.load(Ordering::Relaxed) {
                                data.fill(0.0);
                                return;
                            }
                            let mut buffer = playback_buffer_clone.lock().unwrap();
                            for sample in data.iter_mut() {
                                *sample = buffer.pop().unwrap_or(0.0);
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                SampleFormat::I16 => {
                    device.build_output_stream(
                        &stream_config,
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                            if !is_playing_clone.load(Ordering::Relaxed) {
                                data.fill(0);
                                return;
                            }
                            let mut buffer = playback_buffer_clone.lock().unwrap();
                            for sample in data.iter_mut() {
                                let f = buffer.pop().unwrap_or(0.0);
                                *sample = (f * i16::MAX as f32) as i16;
                            }
                        },
                        err_fn,
                        None,
                    )
                }
                SampleFormat::U16 => {
                    device.build_output_stream(
                        &stream_config,
                        move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                            if !is_playing_clone.load(Ordering::Relaxed) {
                                data.fill(u16::MAX / 2);
                                return;
                            }
                            let mut buffer = playback_buffer_clone.lock().unwrap();
                            for sample in data.iter_mut() {
                                let f = buffer.pop().unwrap_or(0.0);
                                *sample = ((f + 1.0) / 2.0 * u16::MAX as f32) as u16;
                            }
                        },
                        err_fn,
                        None,
                    )
                }
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
                                let f = buffer.pop().unwrap_or(0.0);
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
                        *sample = buffer.pop().unwrap_or(0.0);
                    }
                },
                err_fn,
                None,
            )
        }.map_err(|e| format!("Failed to build output stream: {}", e))?;

        stream.play().map_err(|e| format!("Failed to start output stream: {}", e))?;
        info!("audio: output stream started");

        Ok(Self {
            stream,
            is_playing,
            playback_buffer,
        })
    }

    /// Queue audio samples for playback.
    pub fn queue_samples(&self, samples: &[f32]) {
        let mut buffer = self.playback_buffer.lock().unwrap();
        // Prepend samples (buffer is read from back)
        let mut new_buffer = samples.to_vec();
        new_buffer.reverse();
        new_buffer.extend(buffer.drain(..));
        *buffer = new_buffer;
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
    samples.iter()
        .flat_map(|&s| s.to_le_bytes())
        .collect()
}

/// Convert bytes back to f32 samples.
pub fn bytes_to_samples(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4)
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
