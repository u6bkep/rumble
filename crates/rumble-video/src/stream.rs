//! High-level video stream: wraps an `MpvPlayer` with a worker
//! thread that pulls decoded frames into a CPU mailbox, plus the
//! control surface (pause/resume/seek/time-pos/duration) the UI
//! thread needs to drive playback.
//!
//! Threading model:
//! - The worker thread is the **only** caller of
//!   `MpvPlayer::wait_for_frame` and `MpvPlayer::render_sw`. libmpv
//!   tolerates calls from multiple threads but renders are single-
//!   threaded; the discipline is enforced by exposing only
//!   property/command setters on `VideoStream` itself.
//! - The UI thread reads the latest decoded frame via
//!   [`VideoStream::with_latest_frame`]. Latest-frame storage is a
//!   single `Mutex<FrameBuffer>` swapped under the lock. Stride and
//!   dimensions are fixed at `open()` time — mid-stream resolution
//!   changes (rare) are not handled.
//! - [`VideoStream::frame_seq`] is a monotonically-increasing
//!   counter the UI side checks each paint to skip GPU re-uploads
//!   when nothing has changed.
//!
//! Audio is left to libmpv: `ao=auto` on the player picks the
//! system audio output (PulseAudio / CoreAudio / WASAPI). We do
//! not bridge through cpal — fighting two audio stacks for a
//! peripheral feature is not worth it.

use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use crate::{Error, MpvPlayer};

/// Decoded RGBA frame in CPU memory. Bytes are laid out
/// `R, G, B, 0xff` per pixel — `Rgba8UnormSrgb`-ready, no further
/// conversion needed before `queue.write_texture`.
pub struct FrameBuffer {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Bytes between row y and row y+1. Always `width * 4` for
    /// streams created via [`VideoStream::open`] (no extra row
    /// padding), but kept explicit so callers don't have to assume.
    pub stride: usize,
}

/// Worker-shared state. Held in an `Arc` so the worker thread and
/// the UI-side `VideoStream` both reference the same mailbox.
struct StreamInner {
    /// Latest decoded frame. Worker swaps a freshly-rendered buffer
    /// into this slot; UI reads under the lock.
    latest: Mutex<FrameBuffer>,
    /// Bumped once per successful render. Consumers compare to
    /// their last-seen value to skip redundant uploads.
    seq: AtomicU64,
    /// Worker exit signal. Set by `Drop` on `VideoStream`; checked
    /// at the top of every worker loop iteration.
    shutdown: AtomicBool,
}

/// Live video stream: owns the libmpv player, the decode worker,
/// and the CPU-side frame mailbox. Drop-clean: terminates the
/// worker, then libmpv.
pub struct VideoStream {
    /// Shared with the worker so we can issue pause/resume/seek
    /// from the UI thread while the worker is mid-`wait_for_frame`.
    player: Arc<MpvPlayer>,
    inner: Arc<StreamInner>,
    /// `Option` so `Drop` can `take()` and `join()` without leaving
    /// an invalid `JoinHandle` behind.
    worker: Option<JoinHandle<()>>,
    width: u32,
    height: u32,
}

impl VideoStream {
    /// Open `path`, wait for libmpv to report video dimensions, and
    /// spawn the decode worker. The stream begins in the `playing`
    /// state with audio routed through libmpv's default output.
    /// Pass `looped = true` to set `loop-file=inf` so playback
    /// restarts at end-of-stream — useful for the integration tests
    /// and "preview" UX, less so for one-shot lightbox playback.
    pub fn open(path: &Path, looped: bool) -> Result<Self, Error> {
        let player = MpvPlayer::new()?;
        // Audio: let libmpv pick the platform-native output. The
        // player constructor already disables terminal/input/config
        // so we don't need any further audio plumbing.
        if looped {
            player.set_option_string("loop-file", "inf")?;
        }

        player.load_file(path)?;
        let (width, height) = player.dimensions()?;
        let stride = (width as usize) * 4;
        let frame = FrameBuffer {
            data: vec![0u8; stride * height as usize],
            width,
            height,
            stride,
        };

        let inner = Arc::new(StreamInner {
            latest: Mutex::new(frame),
            seq: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
        });
        let player = Arc::new(player);

        let worker_player = Arc::clone(&player);
        let worker_inner = Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            .name("rumble-video::decode".into())
            .spawn(move || worker_loop(worker_player, worker_inner, width, height, stride))
            .expect("rumble-video: failed to spawn decode worker");

        Ok(Self {
            player,
            inner,
            worker: Some(worker),
            width,
            height,
        })
    }

    /// Pause playback. Idempotent — pausing an already-paused
    /// stream is a no-op libmpv-side.
    pub fn pause(&self) -> Result<(), Error> {
        self.player.set_property_string("pause", "yes")
    }

    /// Resume playback. Inverse of [`Self::pause`].
    pub fn resume(&self) -> Result<(), Error> {
        self.player.set_property_string("pause", "no")
    }

    /// Seek to `target` seconds from the start of the file
    /// (absolute seek). Negative values clamp to zero; values past
    /// the duration land at end-of-file.
    pub fn seek(&self, target: Duration) -> Result<(), Error> {
        let secs = format!("{:.3}", target.as_secs_f64());
        self.player.command(&["seek", &secs, "absolute"])
    }

    /// Current playback position, or `None` while libmpv is still
    /// negotiating start-of-file (the property is unavailable for
    /// the first few milliseconds after `open()`).
    pub fn time_pos(&self) -> Result<Option<Duration>, Error> {
        Ok(self.player.get_property_f64("time-pos")?.map(Duration::from_secs_f64))
    }

    /// Total duration of the loaded file, or `None` for streams
    /// without a known duration (live streams; very rare for chat
    /// attachments).
    pub fn duration(&self) -> Result<Option<Duration>, Error> {
        Ok(self.player.get_property_f64("duration")?.map(Duration::from_secs_f64))
    }

    /// Decoded video size in pixels — fixed across the stream's
    /// lifetime. Sized at `open()` from libmpv's `dwidth` /
    /// `dheight` properties.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Monotonic counter incremented every time the worker swaps a
    /// new frame into the mailbox. UI consumers cache the last-seen
    /// value to skip redundant GPU uploads.
    pub fn frame_seq(&self) -> u64 {
        self.inner.seq.load(Ordering::Acquire)
    }

    /// Run `f` against the latest decoded frame under the mailbox
    /// lock. Hold the closure short — the worker stalls during
    /// frame swaps for the duration of any outstanding read.
    pub fn with_latest_frame<R>(&self, f: impl FnOnce(&FrameBuffer) -> R) -> R {
        let guard = self.inner.latest.lock().expect("video frame mutex poisoned");
        f(&*guard)
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        // Signal the worker to exit at its next loop iteration
        // (worst case ~50ms — the wait_for_frame timeout).
        self.inner.shutdown.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        // Player Arc drops here; mpv_terminate_destroy fires from
        // MpvPlayer::drop once the worker is no longer holding
        // its clone.
    }
}

/// Body of the decode worker. Loops until `shutdown`, blocks up to
/// 50ms in `wait_for_frame`, renders into a back buffer, swaps it
/// into the shared mailbox, bumps the seq counter, repeats.
fn worker_loop(player: Arc<MpvPlayer>, inner: Arc<StreamInner>, width: u32, height: u32, stride: usize) {
    let mut back = vec![0u8; stride * height as usize];
    while !inner.shutdown.load(Ordering::Acquire) {
        match player.wait_for_frame(Duration::from_millis(50)) {
            Ok(true) => {
                if let Err(e) = player.render_sw(&mut back, width, height, stride) {
                    tracing::warn!(error = %e, "video: render_sw failed");
                    continue;
                }
                {
                    let mut latest = inner.latest.lock().expect("video frame mutex poisoned");
                    std::mem::swap(&mut latest.data, &mut back);
                }
                inner.seq.fetch_add(1, Ordering::Release);
            }
            Ok(false) => {
                // Timeout — loop back to re-check shutdown. No new
                // frame; UI still shows the last one. End-of-stream
                // for a non-looped file lands here repeatedly until
                // the parent VideoStream is dropped.
            }
            Err(Error::Shutdown) => break,
            Err(e) => {
                tracing::debug!(error = %e, "video: worker exiting");
                break;
            }
        }
    }
}
