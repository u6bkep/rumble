//! Desktop audio backend.
//!
//! On Linux, prefers PulseAudio (which talks to PipeWire-pulse on PipeWire
//! systems and to legacy pulseaudio otherwise) for proper per-source
//! selection. Falls back to cpal-via-ALSA if no Pulse server is reachable.
//! On macOS and Windows, uses cpal (CoreAudio / WASAPI).

pub mod cpal;
#[cfg(all(target_os = "linux", feature = "pulse"))]
pub mod pulse;

pub use cpal::{CpalAudioBackend, CpalCaptureStream, CpalPlaybackStream};
#[cfg(all(target_os = "linux", feature = "pulse"))]
pub use pulse::{PulseAudioBackend, PulseCaptureStream, PulsePlaybackStream};

use rumble_client_traits::audio::{AudioBackend, AudioCaptureStream, AudioDeviceInfo, AudioPlaybackStream};
#[cfg(all(target_os = "linux", feature = "pulse"))]
use tracing::info;

/// Runtime-selected desktop audio backend.
///
/// Construction picks Pulse if reachable on Linux; otherwise cpal. The
/// choice is fixed for the lifetime of the value — drop and rebuild to
/// re-detect (e.g. after a sound server restart).
pub enum DesktopAudioBackend {
    #[cfg(all(target_os = "linux", feature = "pulse"))]
    Pulse(PulseAudioBackend),
    Cpal(CpalAudioBackend),
}

impl DesktopAudioBackend {
    pub fn new() -> Self {
        #[cfg(all(target_os = "linux", feature = "pulse"))]
        match PulseAudioBackend::new() {
            Ok(b) => return Self::Pulse(b),
            Err(e) => info!(error = %e, "audio: Pulse unavailable, falling back to cpal"),
        }
        Self::Cpal(CpalAudioBackend::new())
    }
}

impl Default for DesktopAudioBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Capture stream produced by [`DesktopAudioBackend`].
pub enum DesktopCaptureStream {
    #[cfg(all(target_os = "linux", feature = "pulse"))]
    Pulse(PulseCaptureStream),
    Cpal(CpalCaptureStream),
}

impl AudioCaptureStream for DesktopCaptureStream {
    fn set_active(&self, active: bool) {
        match self {
            #[cfg(all(target_os = "linux", feature = "pulse"))]
            Self::Pulse(s) => s.set_active(active),
            Self::Cpal(s) => s.set_active(active),
        }
    }
}

/// Playback stream produced by [`DesktopAudioBackend`].
pub enum DesktopPlaybackStream {
    #[cfg(all(target_os = "linux", feature = "pulse"))]
    Pulse(PulsePlaybackStream),
    Cpal(CpalPlaybackStream),
}

impl AudioPlaybackStream for DesktopPlaybackStream {}

impl AudioBackend for DesktopAudioBackend {
    type CaptureStream = DesktopCaptureStream;
    type PlaybackStream = DesktopPlaybackStream;

    fn list_input_devices(&self) -> Vec<AudioDeviceInfo> {
        match self {
            #[cfg(all(target_os = "linux", feature = "pulse"))]
            Self::Pulse(b) => b.list_input_devices(),
            Self::Cpal(b) => b.list_input_devices(),
        }
    }

    fn list_output_devices(&self) -> Vec<AudioDeviceInfo> {
        match self {
            #[cfg(all(target_os = "linux", feature = "pulse"))]
            Self::Pulse(b) => b.list_output_devices(),
            Self::Cpal(b) => b.list_output_devices(),
        }
    }

    fn open_input(
        &self,
        device_id: Option<&str>,
        on_frame: Box<dyn FnMut(&[f32]) + Send>,
    ) -> anyhow::Result<DesktopCaptureStream> {
        match self {
            #[cfg(all(target_os = "linux", feature = "pulse"))]
            Self::Pulse(b) => Ok(DesktopCaptureStream::Pulse(b.open_input(device_id, on_frame)?)),
            Self::Cpal(b) => Ok(DesktopCaptureStream::Cpal(b.open_input(device_id, on_frame)?)),
        }
    }

    fn open_output(
        &self,
        device_id: Option<&str>,
        fill_buffer: Box<dyn FnMut(&mut [f32]) + Send>,
    ) -> anyhow::Result<DesktopPlaybackStream> {
        match self {
            #[cfg(all(target_os = "linux", feature = "pulse"))]
            Self::Pulse(b) => Ok(DesktopPlaybackStream::Pulse(b.open_output(device_id, fill_buffer)?)),
            Self::Cpal(b) => Ok(DesktopPlaybackStream::Cpal(b.open_output(device_id, fill_buffer)?)),
        }
    }
}
