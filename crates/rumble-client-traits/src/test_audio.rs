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
    },
    time::Duration,
};

use crate::audio::{AudioBackend, AudioCaptureStream, AudioDeviceInfo, AudioPlaybackStream, FillBufferFn, OnFrameFn};

/// Per-frame size the engine speaks (20ms at 48 kHz mono).
pub const MOCK_FRAME_SIZE: usize = 960;

/// Test-side handle for driving / observing a [`MockAudioBackend`].
///
/// Created together with the backend via [`MockAudioBackend::new`]. Cloneable
/// so multiple test threads can interact with the same backend.
#[derive(Clone)]
pub struct MockAudio {
    capture_active: Arc<AtomicBool>,
    capture_callback: Arc<Mutex<Option<OnFrameFn>>>,
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
        if !self.capture_active.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut guard) = self.capture_callback.lock()
            && let Some(on_frame) = guard.as_mut()
        {
            on_frame(&frame);
        }
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

    /// Pull `n` mixed playback samples from the engine. Calls the engine's
    /// `fill_buffer` callback in 20 ms device-sized blocks, yielding briefly
    /// between blocks — the way a real output device pulls incrementally at its
    /// clock rate, giving the engine's device-paced producer time to refill the
    /// playback ring between callbacks. (Pulling all `n` at once would outrun the
    /// producer and read mostly silence.) Returns silence (zeros) if no playback
    /// stream has been opened yet.
    pub fn pump_playback(&self, n: usize) -> Vec<f32> {
        const BLOCK: usize = 960; // 20 ms at 48 kHz
        let mut buf = vec![0.0; n];
        let mut off = 0;
        while off < n {
            let end = (off + BLOCK).min(n);
            // Re-acquire the lock per block and release it before sleeping, so the
            // engine can (re)open/close the playback stream between blocks instead
            // of blocking on this lock for the whole pump. If the stream is gone,
            // stop filling — the remainder stays silence.
            {
                let Ok(mut guard) = self.playback.lock() else { break };
                let Some(fill) = guard.as_mut() else { break };
                fill(&mut buf[off..end]);
            }
            off = end;
            if off < n {
                // Let the producer's ~10 ms refill tick run before the next block.
                std::thread::sleep(Duration::from_millis(12));
            }
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
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

/// Mock backend that implements [`AudioBackend`] without touching real audio
/// devices. Construct via [`MockAudioBackend::new`].
pub struct MockAudioBackend {
    capture_active: Arc<AtomicBool>,
    capture_callback: Arc<Mutex<Option<OnFrameFn>>>,
    playback: Arc<Mutex<Option<FillBufferFn>>>,
}

impl MockAudioBackend {
    /// Create a paired `(backend, handle)`. Hand the backend to the engine and
    /// keep the handle in the test for `inject_capture` / `pump_playback`.
    pub fn new() -> (Self, MockAudio) {
        let capture_active = Arc::new(AtomicBool::new(false));
        let capture_callback: Arc<Mutex<Option<OnFrameFn>>> = Arc::new(Mutex::new(None));
        let playback: Arc<Mutex<Option<FillBufferFn>>> = Arc::new(Mutex::new(None));

        let backend = Self {
            capture_active: capture_active.clone(),
            capture_callback: capture_callback.clone(),
            playback: playback.clone(),
        };
        let audio = MockAudio {
            capture_active,
            capture_callback,
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

    fn open_input(&self, _device_id: Option<&str>, on_frame: OnFrameFn) -> anyhow::Result<MockCaptureStream> {
        let mut guard = self
            .capture_callback
            .lock()
            .map_err(|_| anyhow::anyhow!("MockAudioBackend mutex poisoned"))?;
        *guard = Some(on_frame);

        Ok(MockCaptureStream {
            active: self.capture_active.clone(),
            capture_callback: self.capture_callback.clone(),
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
    capture_callback: Arc<Mutex<Option<OnFrameFn>>>,
}

impl AudioCaptureStream for MockCaptureStream {
    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }
}

impl Drop for MockCaptureStream {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
        if let Ok(mut guard) = self.capture_callback.lock() {
            *guard = None;
        }
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
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(received.lock().unwrap().len(), 0);

        // Activate; frame delivered
        stream.set_active(true);
        audio.inject_capture(vec![0.5; 4]);
        std::thread::sleep(Duration::from_millis(20));
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
