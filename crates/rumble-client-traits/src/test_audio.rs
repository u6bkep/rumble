//! Headless mock [`AudioBackend`] for tests.
//!
//! `MockAudioBackend` lets tests inject f32 capture frames and observe what
//! the engine queues for playback, without opening a real audio device. The
//! engine sees the exact same `AudioBackend` trait surface as production —
//! the encoder, decoder, jitter buffer, and datagram path all run unchanged.
//!
//! # Usage
//!
//! ```ignore
//! let (backend, audio) = MockAudioBackend::new();
//! // hand `backend` to your `BackendHandle` constructor; keep `audio` for the test
//!
//! audio.inject_capture(vec![0.1f32; 960]); // one 20ms frame at 48 kHz mono
//! let played = audio.pump_playback(48_000); // pull one second of mixed output
//! ```

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use crate::audio::{AudioBackend, AudioCaptureStream, AudioPlaybackStream, FillBufferFn, OnFrameFn};
use rumble_protocol::AudioDeviceInfo;

/// Per-frame size the engine speaks (20ms at 48 kHz mono).
pub const MOCK_FRAME_SIZE: usize = 960;

/// Test-side handle for driving / observing a [`MockAudioBackend`].
///
/// Created together with the backend via [`MockAudioBackend::new`]. Cloneable
/// so multiple test threads can interact with the same backend.
#[derive(Clone)]
pub struct MockAudio {
    inject_tx: mpsc::Sender<Vec<f32>>,
    capture_active: Arc<AtomicBool>,
    playback: Arc<Mutex<Option<FillBufferFn>>>,
}

impl MockAudio {
    /// Push one capture frame to the engine. The frame is delivered to the
    /// engine's `on_frame` callback only while capture is active (i.e. after
    /// the audio task calls `set_active(true)`).
    ///
    /// The engine expects 960-sample 48 kHz mono frames; other sizes still
    /// flow through but may not encode cleanly.
    pub fn inject_capture(&self, frame: Vec<f32>) {
        let _ = self.inject_tx.send(frame);
    }

    /// Inject `count` 960-sample frames of silence — useful for warming up
    /// past the engine's stale-buffer drop (`frames_to_discard`).
    pub fn inject_silence(&self, count: usize) {
        for _ in 0..count {
            self.inject_capture(vec![0.0; MOCK_FRAME_SIZE]);
        }
    }

    /// Whether the engine has activated capture (i.e. transmit is ungated).
    /// True after `set_active(true)` and before `set_active(false)`.
    pub fn is_capture_active(&self) -> bool {
        self.capture_active.load(Ordering::Relaxed)
    }

    /// Pull `n` mixed playback samples from the engine. Internally calls the
    /// engine's `fill_buffer` callback once with an `n`-sized buffer (the same
    /// way cpal would in production). Returns silence (zeros) if no playback
    /// stream has been opened yet.
    pub fn pump_playback(&self, n: usize) -> Vec<f32> {
        let mut buf = vec![0.0; n];
        if let Ok(mut guard) = self.playback.lock()
            && let Some(fill) = guard.as_mut()
        {
            fill(&mut buf);
        }
        buf
    }

    /// Wait up to `timeout` for capture to be activated by the engine.
    pub fn wait_for_capture_active(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self.is_capture_active() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

/// Mock backend that implements [`AudioBackend`] without touching real audio
/// devices. Construct via [`MockAudioBackend::new`].
pub struct MockAudioBackend {
    inject_rx: Mutex<Option<mpsc::Receiver<Vec<f32>>>>,
    capture_active: Arc<AtomicBool>,
    playback: Arc<Mutex<Option<FillBufferFn>>>,
}

impl MockAudioBackend {
    /// Create a paired `(backend, handle)`. Hand the backend to the engine and
    /// keep the handle in the test for `inject_capture` / `pump_playback`.
    pub fn new() -> (Self, MockAudio) {
        let (inject_tx, inject_rx) = mpsc::channel();
        let capture_active = Arc::new(AtomicBool::new(false));
        let playback: Arc<Mutex<Option<FillBufferFn>>> = Arc::new(Mutex::new(None));

        let backend = Self {
            inject_rx: Mutex::new(Some(inject_rx)),
            capture_active: capture_active.clone(),
            playback: playback.clone(),
        };
        let audio = MockAudio {
            inject_tx,
            capture_active,
            playback,
        };
        (backend, audio)
    }
}

impl Default for MockAudioBackend {
    fn default() -> Self {
        Self::new().0
    }
}

impl AudioBackend for MockAudioBackend {
    type CaptureStream = MockCaptureStream;
    type PlaybackStream = MockPlaybackStream;

    fn list_input_devices(&self) -> Vec<AudioDeviceInfo> {
        vec![AudioDeviceInfo {
            id: "mock-input".into(),
            name: "Mock Input".into(),
            pipeline: None,
            is_default: true,
        }]
    }

    fn list_output_devices(&self) -> Vec<AudioDeviceInfo> {
        vec![AudioDeviceInfo {
            id: "mock-output".into(),
            name: "Mock Output".into(),
            pipeline: None,
            is_default: true,
        }]
    }

    fn open_input(&self, _device_id: Option<&str>, mut on_frame: OnFrameFn) -> anyhow::Result<MockCaptureStream> {
        let rx = self
            .inject_rx
            .lock()
            .map_err(|_| anyhow::anyhow!("MockAudioBackend mutex poisoned"))?
            .take()
            .ok_or_else(|| anyhow::anyhow!("MockAudioBackend::open_input called more than once"))?;

        let active = self.capture_active.clone();
        let active_for_worker = active.clone();
        let worker = thread::spawn(move || {
            // Block on the channel; deliver only while capture is active so
            // gating via `set_active` matches what cpal does in production.
            while let Ok(frame) = rx.recv() {
                if active_for_worker.load(Ordering::Relaxed) {
                    on_frame(&frame);
                }
            }
        });

        Ok(MockCaptureStream {
            active,
            _worker: Some(worker),
        })
    }

    fn open_output(&self, _device_id: Option<&str>, fill_buffer: FillBufferFn) -> anyhow::Result<MockPlaybackStream> {
        let mut guard = self
            .playback
            .lock()
            .map_err(|_| anyhow::anyhow!("MockAudioBackend mutex poisoned"))?;
        *guard = Some(fill_buffer);
        Ok(MockPlaybackStream {
            playback: self.playback.clone(),
        })
    }
}

/// Capture stream produced by [`MockAudioBackend::open_input`].
pub struct MockCaptureStream {
    active: Arc<AtomicBool>,
    // Worker thread is detached on drop — its sender side closes when
    // `MockAudioBackend` (and any `MockAudio` clones) are dropped, which lets
    // the worker exit cleanly.
    _worker: Option<thread::JoinHandle<()>>,
}

impl AudioCaptureStream for MockCaptureStream {
    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }
}

/// Playback stream produced by [`MockAudioBackend::open_output`].
pub struct MockPlaybackStream {
    playback: Arc<Mutex<Option<FillBufferFn>>>,
}

impl AudioPlaybackStream for MockPlaybackStream {}

impl Drop for MockPlaybackStream {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.playback.lock() {
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_frames_only_delivered_when_active() {
        let (backend, audio) = MockAudioBackend::new();
        let received = Arc::new(Mutex::new(Vec::<Vec<f32>>::new()));
        let received_for_callback = received.clone();
        let stream = backend
            .open_input(
                None,
                Box::new(move |samples| {
                    received_for_callback.lock().unwrap().push(samples.to_vec());
                }),
            )
            .expect("open mock input");

        // Inactive: frame dropped
        audio.inject_capture(vec![1.0; 4]);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(received.lock().unwrap().len(), 0);

        // Activate; frame delivered
        stream.set_active(true);
        audio.inject_capture(vec![0.5; 4]);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(received.lock().unwrap().len(), 1);
        assert_eq!(received.lock().unwrap()[0], vec![0.5; 4]);
    }

    #[test]
    fn pump_playback_calls_fill_buffer() {
        let (backend, audio) = MockAudioBackend::new();
        let _stream = backend
            .open_output(
                None,
                Box::new(|out: &mut [f32]| {
                    for (i, s) in out.iter_mut().enumerate() {
                        *s = i as f32;
                    }
                }),
            )
            .expect("open mock output");

        let pulled = audio.pump_playback(4);
        assert_eq!(pulled, vec![0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn pump_playback_returns_silence_with_no_stream() {
        let (_backend, audio) = MockAudioBackend::new();
        let pulled = audio.pump_playback(8);
        assert_eq!(pulled, vec![0.0; 8]);
    }
}
