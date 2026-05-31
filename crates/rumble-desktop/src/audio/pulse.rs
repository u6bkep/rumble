//! PulseAudio backend (libpulse / libpulse-simple).
//!
//! Preferred backend on Linux. Talks Pulse protocol, which works against
//! both PipeWire (via `pipewire-pulse`) and legacy PulseAudio servers.
//! Source/sink selection is by Pulse name (e.g. `alsa_input.usb-AT2020USB+_…`),
//! not ALSA PCM id.
//!
//! Streams use libpulse-simple in dedicated threads. Format is pinned to
//! 48 kHz mono `FloatNE` so no resampling/conversion runs in this layer.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use libpulse_binding::{
    callbacks::ListResult,
    context::{
        Context, FlagSet as ContextFlagSet, State as ContextState,
        introspect::{ServerInfo, SinkInfo, SourceInfo},
    },
    def::BufferAttr,
    mainloop::standard::{IterateResult, Mainloop},
    proplist::{Proplist, properties},
    sample::{Format, Spec},
    stream::Direction,
};
use libpulse_simple_binding::Simple;
use tracing::{debug, error, info, warn};

use rumble_client_traits::audio::{
    AudioBackend, AudioCaptureStream, AudioDeviceInfo, AudioPlaybackStream, FillBufferFn, OnFrameFn,
};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u8 = 1;
const FRAME_SAMPLES: usize = 960; // 20 ms @ 48 kHz
const FRAME_BYTES: usize = FRAME_SAMPLES * 4; // f32 = 4 bytes
const APP_NAME: &str = "rumble";

/// PulseAudio-backed implementation of `AudioBackend`.
///
/// Holds no persistent server connection; each enumeration spins up a
/// short-lived mainloop. Streams own their own libpulse-simple connections.
pub struct PulseAudioBackend {
    _private: (),
}

impl PulseAudioBackend {
    /// Connect once to verify a Pulse server is reachable. Returns Err if
    /// no server is available — caller falls back to cpal.
    pub fn new() -> anyhow::Result<Self> {
        // Round-trip enumeration confirms a server exists and responds.
        let _ = enumerate(false)?;
        info!("audio: PulseAudio backend ready");
        Ok(Self { _private: () })
    }
}

impl Default for PulseAudioBackend {
    fn default() -> Self {
        // Default is only ever called by AudioBackend's bound — DesktopAudioBackend
        // performs the runtime check. If something tries Default directly and Pulse
        // isn't reachable, we panic; that's a programming error.
        Self::new().expect("PulseAudioBackend::default called without a Pulse server")
    }
}

impl AudioBackend for PulseAudioBackend {
    type CaptureStream = PulseCaptureStream;
    type PlaybackStream = PulsePlaybackStream;

    fn list_input_devices(&self) -> Vec<AudioDeviceInfo> {
        match enumerate(false) {
            Ok((sources, _, default_source, _)) => label_default(sources, default_source.as_deref()),
            Err(e) => {
                warn!(error = %e, "audio: failed to list Pulse sources");
                Vec::new()
            }
        }
    }

    fn list_output_devices(&self) -> Vec<AudioDeviceInfo> {
        match enumerate(true) {
            Ok((_, sinks, _, default_sink)) => label_default(sinks, default_sink.as_deref()),
            Err(e) => {
                warn!(error = %e, "audio: failed to list Pulse sinks");
                Vec::new()
            }
        }
    }

    fn open_input(
        &self,
        device_id: Option<&str>,
        on_frame: Box<dyn FnMut(&[f32]) + Send>,
    ) -> anyhow::Result<PulseCaptureStream> {
        let spec = stream_spec();
        let attr = buffer_attr_capture();
        let simple = Simple::new(
            None,
            APP_NAME,
            Direction::Record,
            device_id,
            "voice capture",
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|e| anyhow::anyhow!("Pulse open input failed: {e}"))?;

        info!(source = ?device_id, "audio: PulseAudio input opened");

        let active = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        // Cleared by the capture thread when its read loop exits for any reason
        // other than a requested stop (device unplug, server crash). The audio
        // task polls `is_healthy()` to notice and attempt a re-open.
        let alive = Arc::new(AtomicBool::new(true));
        let on_frame = Arc::new(Mutex::new(on_frame));
        let handle = spawn_capture_thread(simple, active.clone(), stop.clone(), alive.clone(), on_frame);

        Ok(PulseCaptureStream {
            active,
            stop,
            alive,
            handle: Some(handle),
        })
    }

    fn open_output(
        &self,
        device_id: Option<&str>,
        fill_buffer: Box<dyn FnMut(&mut [f32]) + Send>,
    ) -> anyhow::Result<PulsePlaybackStream> {
        let spec = stream_spec();
        let attr = buffer_attr_playback();
        let simple = Simple::new(
            None,
            APP_NAME,
            Direction::Playback,
            device_id,
            "voice playback",
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|e| anyhow::anyhow!("Pulse open output failed: {e}"))?;

        info!(sink = ?device_id, "audio: PulseAudio output opened");

        let stop = Arc::new(AtomicBool::new(false));
        // See the capture path: cleared if the write loop exits on a device
        // error so the audio task can re-open.
        let alive = Arc::new(AtomicBool::new(true));
        let fill_buffer = Arc::new(Mutex::new(fill_buffer));
        let handle = spawn_playback_thread(simple, stop.clone(), alive.clone(), fill_buffer);

        Ok(PulsePlaybackStream {
            stop,
            alive,
            handle: Some(handle),
        })
    }
}

// ---------------------------------------------------------------------------
// Stream specs & buffer attrs
// ---------------------------------------------------------------------------

fn stream_spec() -> Spec {
    Spec {
        format: Format::FLOAT32NE,
        channels: CHANNELS,
        rate: SAMPLE_RATE,
    }
}

/// Capture buffer attrs: short fragments to keep latency low and let
/// `read()` return roughly every 20 ms (one Opus frame).
fn buffer_attr_capture() -> BufferAttr {
    BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: FRAME_BYTES as u32,
    }
}

/// Playback buffer attrs: similar — let pulse pick reasonable defaults for
/// most fields, only tighten `tlength` (target buffer length) so we wake up
/// often enough to keep the jitter buffer fed.
fn buffer_attr_playback() -> BufferAttr {
    BufferAttr {
        maxlength: u32::MAX,
        tlength: (FRAME_BYTES * 4) as u32, // ~80 ms target
        prebuf: u32::MAX,
        minreq: FRAME_BYTES as u32,
        fragsize: u32::MAX,
    }
}

// ---------------------------------------------------------------------------
// Threaded I/O loops
// ---------------------------------------------------------------------------

fn spawn_capture_thread(
    simple: Simple,
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    on_frame: Arc<Mutex<OnFrameFn>>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("rumble-pulse-capture".into())
        .spawn(move || {
            let mut samples = vec![0.0f32; FRAME_SAMPLES];
            while !stop.load(Ordering::Relaxed) {
                // SAFETY: bytes alias the f32 buffer; pulse-simple only reads/writes
                // raw bytes, and we're the sole owner.
                let bytes: &mut [u8] =
                    unsafe { std::slice::from_raw_parts_mut(samples.as_mut_ptr().cast::<u8>(), FRAME_BYTES) };
                if let Err(e) = simple.read(bytes) {
                    error!(error = %e, "audio: Pulse capture read failed");
                    // Device died / server gone: flag unhealthy so the audio
                    // task re-opens. (A requested stop never reaches here.)
                    alive.store(false, Ordering::Relaxed);
                    break;
                }
                if !active.load(Ordering::Relaxed) {
                    continue;
                }
                if let Ok(mut cb) = on_frame.lock() {
                    cb(&samples);
                }
            }
            debug!("audio: Pulse capture thread exiting");
        })
        .expect("spawn capture thread")
}

fn spawn_playback_thread(
    simple: Simple,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    fill_buffer: Arc<Mutex<FillBufferFn>>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("rumble-pulse-playback".into())
        .spawn(move || {
            let mut samples = vec![0.0f32; FRAME_SAMPLES];
            while !stop.load(Ordering::Relaxed) {
                if let Ok(mut cb) = fill_buffer.lock() {
                    cb(&mut samples);
                } else {
                    samples.fill(0.0);
                }
                let bytes: &[u8] = unsafe { std::slice::from_raw_parts(samples.as_ptr().cast::<u8>(), FRAME_BYTES) };
                if let Err(e) = simple.write(bytes) {
                    error!(error = %e, "audio: Pulse playback write failed");
                    // Device died / server gone: flag unhealthy so the audio
                    // task re-opens. (A requested stop never reaches here.)
                    alive.store(false, Ordering::Relaxed);
                    break;
                }
            }
            // Best-effort drain so the last queued frames actually play out.
            let _ = simple.drain();
            debug!("audio: Pulse playback thread exiting");
        })
        .expect("spawn playback thread")
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

pub struct PulseCaptureStream {
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioCaptureStream for PulseCaptureStream {
    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    fn is_healthy(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl Drop for PulseCaptureStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // The blocking read returns within ~20 ms; join is bounded.
            let _ = h.join();
        }
    }
}

pub struct PulsePlaybackStream {
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioPlaybackStream for PulsePlaybackStream {
    fn is_healthy(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl Drop for PulsePlaybackStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Probe + introspection (one-shot mainloop)
// ---------------------------------------------------------------------------

/// Tuple of `(sources, sinks, default_source_name, default_sink_name)`.
type EnumerateResult = (
    Vec<AudioDeviceInfo>,
    Vec<AudioDeviceInfo>,
    Option<String>,
    Option<String>,
);

/// Run a one-shot mainloop, return (sources, sinks, default_source_name, default_sink_name).
///
/// `need_sinks`: whether to also fetch the sink list. Sources are always
/// fetched because we need them for the default-source label.
fn enumerate(need_sinks: bool) -> anyhow::Result<EnumerateResult> {
    let mut props = Proplist::new().ok_or_else(|| anyhow::anyhow!("Proplist::new failed"))?;
    let _ = props.set_str(properties::APPLICATION_NAME, APP_NAME);

    let mut mainloop = Mainloop::new().ok_or_else(|| anyhow::anyhow!("Mainloop::new failed"))?;
    let mut context = Context::new_with_proplist(&mainloop, APP_NAME, &props)
        .ok_or_else(|| anyhow::anyhow!("Context::new failed"))?;

    context
        .connect(None, ContextFlagSet::NOAUTOSPAWN, None)
        .map_err(|e| anyhow::anyhow!("Pulse context connect: {e:?}"))?;

    // Wait until context is ready (or fails).
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match mainloop.iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                anyhow::bail!("Pulse mainloop iterate failed during connect");
            }
            IterateResult::Success(_) => {}
        }
        match context.get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                anyhow::bail!("Pulse context failed to connect");
            }
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("Pulse context connect timed out");
        }
    }

    let sources = Arc::new(Mutex::new(Vec::<AudioDeviceInfo>::new()));
    let sinks = Arc::new(Mutex::new(Vec::<AudioDeviceInfo>::new()));
    let default_source: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let default_sink: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sources_done = Arc::new(AtomicBool::new(false));
    let sinks_done = Arc::new(AtomicBool::new(!need_sinks));
    let server_done = Arc::new(AtomicBool::new(false));

    let introspect = context.introspect();

    {
        let default_source = default_source.clone();
        let default_sink = default_sink.clone();
        let server_done = server_done.clone();
        introspect.get_server_info(move |info: &ServerInfo| {
            *default_source.lock().unwrap() = info.default_source_name.as_ref().map(|s| s.to_string());
            *default_sink.lock().unwrap() = info.default_sink_name.as_ref().map(|s| s.to_string());
            server_done.store(true, Ordering::Release);
        });
    }

    {
        let sources = sources.clone();
        let sources_done = sources_done.clone();
        introspect.get_source_info_list(move |result: ListResult<&SourceInfo>| match result {
            ListResult::Item(s) => {
                // Skip monitor sources (those mirror an output sink — not useful as a mic).
                if s.monitor_of_sink.is_some() {
                    return;
                }
                if let Some(info) = source_info_to_device(s) {
                    sources.lock().unwrap().push(info);
                }
            }
            ListResult::End | ListResult::Error => {
                sources_done.store(true, Ordering::Release);
            }
        });
    }

    if need_sinks {
        let sinks = sinks.clone();
        let sinks_done = sinks_done.clone();
        introspect.get_sink_info_list(move |result: ListResult<&SinkInfo>| match result {
            ListResult::Item(s) => {
                if let Some(info) = sink_info_to_device(s) {
                    sinks.lock().unwrap().push(info);
                }
            }
            ListResult::End | ListResult::Error => {
                sinks_done.store(true, Ordering::Release);
            }
        });
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !(sources_done.load(Ordering::Acquire)
        && sinks_done.load(Ordering::Acquire)
        && server_done.load(Ordering::Acquire))
    {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                anyhow::bail!("Pulse mainloop iterate failed during introspection");
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("Pulse introspection timed out");
        }
    }

    context.disconnect();

    let sources = Arc::try_unwrap(sources).unwrap().into_inner().unwrap();
    let sinks = Arc::try_unwrap(sinks).unwrap().into_inner().unwrap();
    let default_source = Arc::try_unwrap(default_source).unwrap().into_inner().unwrap();
    let default_sink = Arc::try_unwrap(default_sink).unwrap().into_inner().unwrap();

    Ok((sources, sinks, default_source, default_sink))
}

fn source_info_to_device(s: &SourceInfo) -> Option<AudioDeviceInfo> {
    let id = s.name.as_ref()?.to_string();
    let name = s
        .description
        .as_ref()
        .map(|d| d.to_string())
        .unwrap_or_else(|| id.clone());
    let pipeline = Some(format!("pulse:source:{}", &id));
    Some(AudioDeviceInfo {
        id,
        name,
        pipeline,
        is_default: false, // filled in by label_default once we know the default name
    })
}

fn sink_info_to_device(s: &SinkInfo) -> Option<AudioDeviceInfo> {
    let id = s.name.as_ref()?.to_string();
    let name = s
        .description
        .as_ref()
        .map(|d| d.to_string())
        .unwrap_or_else(|| id.clone());
    let pipeline = Some(format!("pulse:sink:{}", &id));
    Some(AudioDeviceInfo {
        id,
        name,
        pipeline,
        is_default: false,
    })
}

fn label_default(mut devices: Vec<AudioDeviceInfo>, default_name: Option<&str>) -> Vec<AudioDeviceInfo> {
    if let Some(default) = default_name {
        for d in &mut devices {
            d.is_default = d.id == default;
        }
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: connect to whatever Pulse server is on this machine and
    /// list both sources and sinks. Skipped on machines with no server.
    #[test]
    #[ignore = "requires a running PulseAudio/PipeWire server"]
    fn enumerate_smoke() {
        let backend = PulseAudioBackend::new().expect("Pulse server reachable");
        let inputs = backend.list_input_devices();
        let outputs = backend.list_output_devices();
        eprintln!("sources ({}):", inputs.len());
        for d in &inputs {
            eprintln!("  default={} id={:?} name={:?}", d.is_default, d.id, d.name);
        }
        eprintln!("sinks ({}):", outputs.len());
        for d in &outputs {
            eprintln!("  default={} id={:?} name={:?}", d.is_default, d.id, d.name);
        }
        assert!(!inputs.is_empty(), "expected at least one Pulse source");
        assert!(!outputs.is_empty(), "expected at least one Pulse sink");
    }
}
