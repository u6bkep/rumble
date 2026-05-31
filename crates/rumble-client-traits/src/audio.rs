//! Audio backend abstraction for capture and playback.

/// Information about an audio device returned by the platform's
/// [`AudioBackend`].
///
/// Lives here (rather than in `rumble-client::events`) because platform
/// impl crates (`rumble-desktop`, future WASM impl) need to produce
/// these without taking a dep on the engine crate. Both
/// `rumble-client::events::AudioState` and the platform impls speak
/// this type via this trait crate.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AudioDeviceInfo {
    /// Stable, host-unique identifier (used for selection + persistence).
    ///
    /// On ALSA this is the cpal `pcm_id` (e.g. `pipewire`, `pulse`,
    /// `front:CARD=Generic_1,DEV=0`). On other hosts it's whatever
    /// `Device::id()` returns. The same physical card can appear under
    /// multiple ALSA endpoints, so two entries can share `name` but
    /// always have distinct `id`s.
    pub id: String,
    /// Human-readable name (e.g. `"AT2020USB+, USB Audio"`).
    /// Comes from `Device::description().name()`. Not unique on its own.
    pub name: String,
    /// Routing / driver tag the user can use to disambiguate same-named
    /// entries — typically the ALSA pcm pipeline (`pipewire`, `pulse`,
    /// `front:CARD=...`, `dsnoop:CARD=...`) or the host driver name
    /// on other platforms. `None` if the host doesn't expose one.
    #[serde(default)]
    pub pipeline: Option<String>,
    /// Whether this is the default device.
    pub is_default: bool,
}

/// Callback invoked with each captured PCM frame (48 kHz mono, 960 samples).
pub type OnFrameFn = Box<dyn FnMut(&[f32]) + Send>;

/// Callback invoked to fill an output PCM buffer.
pub type FillBufferFn = Box<dyn FnMut(&mut [f32]) + Send>;

/// A live audio capture stream that can be paused/resumed.
///
/// Not `Send`: cpal's `Stream` is `!Send` on Windows (WASAPI's COM
/// model pins it to the creating thread). The audio task owns streams
/// as locals on its dedicated thread (see `spawn_audio_task`), so
/// they never need to cross threads in practice.
pub trait AudioCaptureStream {
    /// Enable or disable capture. When inactive, the stream should produce
    /// silence (or simply not invoke the callback).
    fn set_active(&self, active: bool);

    /// Whether the stream's underlying device/IO is still alive.
    ///
    /// Returns `false` once the platform has observed the device vanish
    /// (unplug, sound-server crash) or its IO thread/callback die. The audio
    /// task polls this so it can tear the stream down, tell the UI, and try
    /// to re-open the device — backends with no liveness signal keep the
    /// default `true`.
    fn is_healthy(&self) -> bool {
        true
    }
}

/// A live audio playback stream. See `AudioCaptureStream` for why
/// this trait doesn't require `Send`.
pub trait AudioPlaybackStream {
    /// Whether the stream's underlying device/IO is still alive. See
    /// [`AudioCaptureStream::is_healthy`].
    fn is_healthy(&self) -> bool {
        true
    }
}

/// Platform audio I/O: device enumeration, capture, and playback.
///
/// Implementations use the platform's native audio API (e.g. cpal on
/// desktop, Web Audio on browser).
pub trait AudioBackend: Send + Default + 'static {
    type CaptureStream: AudioCaptureStream;
    type PlaybackStream: AudioPlaybackStream;

    /// List available audio input (microphone) devices.
    fn list_input_devices(&self) -> Vec<AudioDeviceInfo>;

    /// List available audio output (speaker) devices.
    fn list_output_devices(&self) -> Vec<AudioDeviceInfo>;

    /// Open an input device for capture.
    ///
    /// `device_id` selects a specific device; `None` uses the default.
    /// `on_frame` is called with 960-sample f32 PCM frames at 48 kHz mono.
    fn open_input(&self, device_id: Option<&str>, on_frame: OnFrameFn) -> anyhow::Result<Self::CaptureStream>;

    /// Open an output device for playback.
    ///
    /// `device_id` selects a specific device; `None` uses the default.
    /// `fill_buffer` is called to fill the output buffer with f32 PCM samples.
    fn open_output(&self, device_id: Option<&str>, fill_buffer: FillBufferFn) -> anyhow::Result<Self::PlaybackStream>;
}
