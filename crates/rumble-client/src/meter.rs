//! Live audio metering types.
//!
//! Metering rides the [`crate::snapshot`] transport — a single-writer /
//! multi-reader latest-value channel — rather than the projection event
//! bus. The audio task publishes a fresh [`MeterSnapshot`] per capture
//! frame (~50 Hz); the UI samples it once per repaint via
//! `BackendHandle::meter()`. Keeping it off the bus means a meter tick
//! doesn't take the `State` lock, wake the projection task, or need a
//! per-event repaint carve-out.

/// A measured audio level, or "not yet measured."
///
/// `Option<f32>` would carry the same data, but the named variants make
/// callers handle "no measurement source attached" explicitly instead of
/// silently treating it as silence (which the old `Option<f32>` meter
/// path did — drawing an empty bar but printing "—" — two states that
/// diverge if a caller forgets to special-case `None`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Level {
    /// The audio task has not yet delivered a capture frame, or the
    /// input device is not open. UI renders this as "no signal,"
    /// distinct from silence.
    #[default]
    Unmeasured,
    /// RMS dB of the most recent frame. The floor is whatever
    /// `rumble_audio::calculate_rms_db` produces for digital silence
    /// (a very large negative value); callers should clamp for display.
    Db(f32),
}

impl Level {
    /// Extract the dB value, if measured.
    pub fn as_db(self) -> Option<f32> {
        match self {
            Level::Unmeasured => None,
            Level::Db(db) => Some(db),
        }
    }

    /// Map onto `[0.0, 1.0]` linearly between `min_db` and `max_db`.
    /// Unmeasured returns `0.0` so an empty bar paints by default.
    pub fn normalized(self, min_db: f32, max_db: f32) -> f32 {
        let span = (max_db - min_db).max(f32::EPSILON);
        match self {
            Level::Unmeasured => 0.0,
            Level::Db(db) => ((db - min_db) / span).clamp(0.0, 1.0),
        }
    }
}

/// Live metering data published by the audio task once per capture
/// frame. Sampled by the UI each repaint.
///
/// Only the two level taps live here. Transmit state is intentionally
/// *not* mirrored onto the snapshot: it's an edge-triggered fact owned
/// by `AudioState::is_transmitting` (driven by `VoiceEvent::Transmitting`
/// Changed), and its main consumer — transmit-start SFX — needs the edge,
/// which a sampled snapshot can't represent without the reader
/// re-deriving it.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeterSnapshot {
    /// RMS dB of the raw capture frame, before any TX processor runs.
    /// "Is the mic alive?" — surfaced by the Devices tab meter.
    pub input_pre: Level,
    /// RMS dB after the TX pipeline. "What level is being transmitted?"
    /// — surfaced by the Processing tab meter.
    pub input_post: Level,
}
