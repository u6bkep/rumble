//! Cpal-based audio backend.
//!
//! Used as the primary backend on macOS (CoreAudio) and Windows (WASAPI),
//! and as a fallback on Linux when no PulseAudio/PipeWire server is reachable.
//! On Linux with a sound server present, prefer [`super::pulse::PulseAudioBackend`].

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use cpal::{
    Device, Host, SampleFormat, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use tracing::{debug, error, info, warn};

use rumble_client_traits::audio::{AudioBackend, AudioCaptureStream, AudioPlaybackStream, FillBufferFn, OnFrameFn};
use rumble_protocol::AudioDeviceInfo;

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 1;
const FRAME_SIZE: usize = 960;

fn get_device_id(device: &Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

fn make_device_info(device: &Device, default_id: Option<&str>) -> Option<AudioDeviceInfo> {
    let id = get_device_id(device)?;
    let description = device.description().ok();
    let name = description
        .as_ref()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|| id.clone());
    let pipeline = description.and_then(|d| d.driver().map(|s| s.to_string()));
    let is_default = default_id == Some(id.as_str());
    Some(AudioDeviceInfo {
        id,
        name,
        pipeline,
        is_default,
    })
}

fn list_devices(devices: Option<impl Iterator<Item = Device>>, default_id: Option<&str>) -> Vec<AudioDeviceInfo> {
    let Some(devices) = devices else { return Vec::new() };
    devices
        .filter_map(|device| make_device_info(&device, default_id))
        .collect()
}

fn find_device_or_default(
    devices: impl Iterator<Item = Device>,
    device_id: Option<&str>,
    default: Option<Device>,
) -> anyhow::Result<Device> {
    if let Some(id) = device_id {
        if let Some(found) = devices.into_iter().find(|d| get_device_id(d).as_deref() == Some(id)) {
            return Ok(found);
        }
        warn!(requested = %id, "audio: requested device id not found, falling back to system default");
    }
    default.ok_or_else(|| anyhow::anyhow!("no default audio device available"))
}

fn sample_format_preference(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 5,
        SampleFormat::I16 => 4,
        SampleFormat::I32 => 3,
        SampleFormat::U16 => 2,
        SampleFormat::U8 => 1,
        _ => 0,
    }
}

/// Pick the best supported config for a device that matches our target rate / channels.
fn pick_config(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> Option<(StreamConfig, SampleFormat, u32, u16)> {
    let mut all: Vec<cpal::SupportedStreamConfigRange> = configs.collect();
    all.sort_by_key(|c| std::cmp::Reverse(sample_format_preference(c.sample_format())));

    for cfg in &all {
        if cfg.channels() == CHANNELS && cfg.min_sample_rate() <= SAMPLE_RATE && cfg.max_sample_rate() >= SAMPLE_RATE {
            let sc = StreamConfig {
                channels: CHANNELS,
                sample_rate: SAMPLE_RATE,
                buffer_size: cpal::BufferSize::Default,
            };
            return Some((sc, cfg.sample_format(), SAMPLE_RATE, CHANNELS));
        }
    }
    for cfg in &all {
        if cfg.min_sample_rate() <= SAMPLE_RATE && cfg.max_sample_rate() >= SAMPLE_RATE {
            let ch = cfg.channels();
            let sc = StreamConfig {
                channels: ch,
                sample_rate: SAMPLE_RATE,
                buffer_size: cpal::BufferSize::Default,
            };
            return Some((sc, cfg.sample_format(), SAMPLE_RATE, ch));
        }
    }
    if let Some(cfg) = all.first() {
        let rate = cfg.max_sample_rate();
        let ch = cfg.channels();
        let sc = StreamConfig {
            channels: ch,
            sample_rate: rate,
            buffer_size: cpal::BufferSize::Default,
        };
        return Some((sc, cfg.sample_format(), rate, ch));
    }
    None
}

fn resample(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if input.is_empty() || from_rate == to_rate {
        return input.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = ((input.len() as f64) / ratio).ceil() as usize;
    let mut output = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos as usize;
        let frac = src_pos - idx as f64;
        let a = input[idx.min(input.len() - 1)];
        let b = input[(idx + 1).min(input.len() - 1)];
        output.push(a + (b - a) * frac as f32);
    }
    output
}

struct InputProcessor {
    buffer: Vec<f32>,
    frame_size: usize,
    on_frame: OnFrameFn,
}

impl InputProcessor {
    fn new(frame_size: usize, on_frame: OnFrameFn) -> Self {
        Self {
            buffer: Vec::with_capacity(frame_size * 2),
            frame_size,
            on_frame,
        }
    }

    fn process(&mut self, samples: &[f32]) {
        self.buffer.extend_from_slice(samples);
        while self.buffer.len() >= self.frame_size {
            let frame: Vec<f32> = self.buffer.drain(..self.frame_size).collect();
            (self.on_frame)(&frame);
        }
    }
}

pub struct CpalAudioBackend {
    host: Host,
}

impl CpalAudioBackend {
    pub fn new() -> Self {
        let host = cpal::default_host();
        info!(host_id = ?host.id(), "audio: initialized cpal host");
        Self { host }
    }
}

impl Default for CpalAudioBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioBackend for CpalAudioBackend {
    type CaptureStream = CpalCaptureStream;
    type PlaybackStream = CpalPlaybackStream;

    fn list_input_devices(&self) -> Vec<AudioDeviceInfo> {
        let default_id = self.host.default_input_device().and_then(|d| get_device_id(&d));
        list_devices(self.host.input_devices().ok(), default_id.as_deref())
    }

    fn list_output_devices(&self) -> Vec<AudioDeviceInfo> {
        let default_id = self.host.default_output_device().and_then(|d| get_device_id(&d));
        list_devices(self.host.output_devices().ok(), default_id.as_deref())
    }

    fn open_input(
        &self,
        device_id: Option<&str>,
        on_frame: Box<dyn FnMut(&[f32]) + Send>,
    ) -> anyhow::Result<CpalCaptureStream> {
        let device = find_device_or_default(self.host.input_devices()?, device_id, self.host.default_input_device())?;

        let dev_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".into());
        info!(device = %dev_name, "audio: opening input device");

        let supported = device.supported_input_configs()?;
        let (stream_config, sample_format, actual_rate, actual_channels) = pick_config(supported)
            .ok_or_else(|| anyhow::anyhow!("no suitable input config for device {}", dev_name))?;

        debug!(
            ?sample_format,
            rate = actual_rate,
            channels = actual_channels,
            "audio: input config selected"
        );

        let is_active = Arc::new(AtomicBool::new(true));
        let processor = Arc::new(Mutex::new(InputProcessor::new(FRAME_SIZE, on_frame)));
        let err_fn = move |err| error!("audio: input stream error: {}", err);

        let stream = match sample_format {
            SampleFormat::F32 => {
                let ch = actual_channels;
                let rate = actual_rate;
                let is_active = is_active.clone();
                let processor = processor.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if !is_active.load(Ordering::Relaxed) {
                            return;
                        }
                        let samples = convert_input_f32(data, ch, rate);
                        if let Ok(mut proc) = processor.lock() {
                            proc.process(&samples);
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::I16 => {
                let ch = actual_channels;
                let rate = actual_rate;
                let is_active = is_active.clone();
                let processor = processor.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        if !is_active.load(Ordering::Relaxed) {
                            return;
                        }
                        let float_data: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                        let samples = convert_input_f32(&float_data, ch, rate);
                        if let Ok(mut proc) = processor.lock() {
                            proc.process(&samples);
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::U16 => {
                let ch = actual_channels;
                let rate = actual_rate;
                let is_active = is_active.clone();
                let processor = processor.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        if !is_active.load(Ordering::Relaxed) {
                            return;
                        }
                        let float_data: Vec<f32> =
                            data.iter().map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0).collect();
                        let samples = convert_input_f32(&float_data, ch, rate);
                        if let Ok(mut proc) = processor.lock() {
                            proc.process(&samples);
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::I32 => {
                let ch = actual_channels;
                let rate = actual_rate;
                let is_active = is_active.clone();
                let processor = processor.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[i32], _: &cpal::InputCallbackInfo| {
                        if !is_active.load(Ordering::Relaxed) {
                            return;
                        }
                        let float_data: Vec<f32> = data.iter().map(|&s| s as f32 / i32::MAX as f32).collect();
                        let samples = convert_input_f32(&float_data, ch, rate);
                        if let Ok(mut proc) = processor.lock() {
                            proc.process(&samples);
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::U8 => {
                let ch = actual_channels;
                let rate = actual_rate;
                let is_active = is_active.clone();
                let processor = processor.clone();
                device.build_input_stream(
                    &stream_config,
                    move |data: &[u8], _: &cpal::InputCallbackInfo| {
                        if !is_active.load(Ordering::Relaxed) {
                            return;
                        }
                        let float_data: Vec<f32> = data.iter().map(|&s| (s as f32 - 128.0) / 128.0).collect();
                        let samples = convert_input_f32(&float_data, ch, rate);
                        if let Ok(mut proc) = processor.lock() {
                            proc.process(&samples);
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            other => anyhow::bail!("unsupported input sample format: {:?}", other),
        };

        stream.play()?;
        info!("audio: input stream started");

        Ok(CpalCaptureStream {
            _stream: stream,
            is_active,
        })
    }

    fn open_output(
        &self,
        device_id: Option<&str>,
        fill_buffer: Box<dyn FnMut(&mut [f32]) + Send>,
    ) -> anyhow::Result<CpalPlaybackStream> {
        let device = find_device_or_default(
            self.host.output_devices()?,
            device_id,
            self.host.default_output_device(),
        )?;

        let dev_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".into());
        info!(device = %dev_name, "audio: opening output device");

        let supported = device.supported_output_configs()?;
        let (stream_config, sample_format, actual_rate, actual_channels) = pick_config(supported)
            .ok_or_else(|| anyhow::anyhow!("no suitable output config for device {}", dev_name))?;

        debug!(
            ?sample_format,
            rate = actual_rate,
            channels = actual_channels,
            "audio: output config selected"
        );

        let fill = Arc::new(Mutex::new(fill_buffer));
        let err_fn = move |err| error!("audio: output stream error: {}", err);

        let stream = match sample_format {
            SampleFormat::F32 => {
                let fill = fill.clone();
                let ch = actual_channels;
                let rate = actual_rate;
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        write_output_f32(data, &fill, ch, rate);
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::I16 => {
                let fill = fill.clone();
                let ch = actual_channels;
                let rate = actual_rate;
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        let mono_len = output_mono_len(data.len(), ch, rate);
                        let mut mono = vec![0.0f32; mono_len];
                        if let Ok(mut f) = fill.lock() {
                            f(&mut mono);
                        }
                        let expanded = expand_output(&mono, ch, rate);
                        for (out, &val) in data.iter_mut().zip(expanded.iter()) {
                            *out = (val * i16::MAX as f32) as i16;
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::U16 => {
                let fill = fill.clone();
                let ch = actual_channels;
                let rate = actual_rate;
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                        let mono_len = output_mono_len(data.len(), ch, rate);
                        let mut mono = vec![0.0f32; mono_len];
                        if let Ok(mut f) = fill.lock() {
                            f(&mut mono);
                        }
                        let expanded = expand_output(&mono, ch, rate);
                        for (out, &val) in data.iter_mut().zip(expanded.iter()) {
                            *out = ((val + 1.0) / 2.0 * u16::MAX as f32) as u16;
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::I32 => {
                let fill = fill.clone();
                let ch = actual_channels;
                let rate = actual_rate;
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                        let mono_len = output_mono_len(data.len(), ch, rate);
                        let mut mono = vec![0.0f32; mono_len];
                        if let Ok(mut f) = fill.lock() {
                            f(&mut mono);
                        }
                        let expanded = expand_output(&mono, ch, rate);
                        for (out, &val) in data.iter_mut().zip(expanded.iter()) {
                            *out = (val * i32::MAX as f32) as i32;
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::U8 => {
                let fill = fill.clone();
                let ch = actual_channels;
                let rate = actual_rate;
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [u8], _: &cpal::OutputCallbackInfo| {
                        let mono_len = output_mono_len(data.len(), ch, rate);
                        let mut mono = vec![0.0f32; mono_len];
                        if let Ok(mut f) = fill.lock() {
                            f(&mut mono);
                        }
                        let expanded = expand_output(&mono, ch, rate);
                        for (out, &val) in data.iter_mut().zip(expanded.iter()) {
                            *out = ((val * 128.0) + 128.0).clamp(0.0, 255.0) as u8;
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            other => anyhow::bail!("unsupported output sample format: {:?}", other),
        };

        stream.play()?;
        info!("audio: output stream started");

        Ok(CpalPlaybackStream { _stream: stream })
    }
}

fn convert_input_f32(data: &[f32], channels: u16, device_rate: u32) -> Vec<f32> {
    let mono = if channels == 1 {
        data.to_vec()
    } else {
        let ch = channels as usize;
        data.chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    };
    if device_rate == SAMPLE_RATE {
        mono
    } else {
        resample(&mono, device_rate, SAMPLE_RATE)
    }
}

fn output_mono_len(total_device_samples: usize, channels: u16, device_rate: u32) -> usize {
    let device_mono = total_device_samples / channels as usize;
    if device_rate == SAMPLE_RATE {
        device_mono
    } else {
        ((device_mono as f64) * (SAMPLE_RATE as f64 / device_rate as f64)).ceil() as usize
    }
}

fn expand_output(mono: &[f32], channels: u16, device_rate: u32) -> Vec<f32> {
    let resampled = if device_rate == SAMPLE_RATE {
        mono.to_vec()
    } else {
        resample(mono, SAMPLE_RATE, device_rate)
    };
    if channels == 1 {
        resampled
    } else {
        let ch = channels as usize;
        let mut out = Vec::with_capacity(resampled.len() * ch);
        for &s in &resampled {
            for _ in 0..ch {
                out.push(s);
            }
        }
        out
    }
}

fn write_output_f32(data: &mut [f32], fill: &Arc<Mutex<FillBufferFn>>, channels: u16, device_rate: u32) {
    if channels == 1 && device_rate == SAMPLE_RATE {
        if let Ok(mut f) = fill.lock() {
            f(data);
        } else {
            data.fill(0.0);
        }
    } else {
        let mono_len = output_mono_len(data.len(), channels, device_rate);
        let mut mono = vec![0.0f32; mono_len];
        if let Ok(mut f) = fill.lock() {
            f(&mut mono);
        }
        let expanded = expand_output(&mono, channels, device_rate);
        let copy_len = data.len().min(expanded.len());
        data[..copy_len].copy_from_slice(&expanded[..copy_len]);
        for s in &mut data[copy_len..] {
            *s = 0.0;
        }
    }
}

pub struct CpalCaptureStream {
    _stream: cpal::Stream,
    is_active: Arc<AtomicBool>,
}

impl AudioCaptureStream for CpalCaptureStream {
    fn set_active(&self, active: bool) {
        self.is_active.store(active, Ordering::Relaxed);
    }
}

// SAFETY: cpal::Stream is Send on all desktop platforms we target.
unsafe impl Send for CpalCaptureStream {}

pub struct CpalPlaybackStream {
    _stream: cpal::Stream,
}

impl AudioPlaybackStream for CpalPlaybackStream {}

// SAFETY: cpal::Stream is Send on all desktop platforms we target.
unsafe impl Send for CpalPlaybackStream {}
