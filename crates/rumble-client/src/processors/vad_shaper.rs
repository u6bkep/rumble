//! Voice-activity gating state machine.
//!
//! Translates a per-frame "voice-likeness" score (dB level, ML probability —
//! whatever the caller produces) into a binary suppress/active decision,
//! applying hysteresis, a minimum attack duration, and a holdoff timer.
//!
//! This module is unit-agnostic: the caller chooses the score's units and
//! supplies matching thresholds. The energy VAD uses dB; the RNNoise VAD
//! uses a 0..1 probability. Wrapping logic and defaults belong to the
//! consuming processor, not here.

/// Per-call settings for [`VadShaper::update`].
///
/// `trigger` and `release` must be in the same units as the score; an
/// out-of-order pair (`release > trigger`) collapses to no-hysteresis,
/// matching the runtime clamp inside [`VadShaper::update`].
#[derive(Debug, Clone, Copy)]
pub struct VadShaperSettings {
    pub trigger: f32,
    pub release: f32,
    pub attack_ms: u32,
    pub holdoff_ms: u32,
}

/// Holdoff + attack + hysteresis state machine.
pub struct VadShaper {
    voice_active: bool,
    /// Consecutive samples above trigger while inactive; resets on any
    /// frame at or below trigger.
    attack_samples_accumulated: u32,
    /// Samples remaining until the active→inactive transition fires.
    holdoff_samples_remaining: u32,
}

impl VadShaper {
    pub fn new() -> Self {
        Self {
            voice_active: false,
            attack_samples_accumulated: 0,
            holdoff_samples_remaining: 0,
        }
    }

    pub fn reset(&mut self) {
        self.voice_active = false;
        self.attack_samples_accumulated = 0;
        self.holdoff_samples_remaining = 0;
    }

    pub fn is_active(&self) -> bool {
        self.voice_active
    }

    /// Advance the state machine by one input frame. Returns the new
    /// active state.
    ///
    /// `frame_samples` is the duration of the input the score covers,
    /// at `sample_rate`. The energy VAD passes the outer pipeline frame
    /// size; an ML-VAD wrapping a fixed-size network chunk (e.g. RNNoise's
    /// 480-sample frame) should pass that chunk size, calling `update`
    /// once per chunk.
    pub fn update(&mut self, score: f32, frame_samples: u32, sample_rate: u32, settings: &VadShaperSettings) -> bool {
        let trigger = settings.trigger;
        // A release threshold above the trigger would mean the
        // active→inactive transition can never happen; clamp so an
        // upside-down configuration collapses to the no-hysteresis case.
        let release = settings.release.min(trigger);
        let holdoff_samples = ms_to_samples(settings.holdoff_ms, sample_rate);
        let attack_samples_required = ms_to_samples(settings.attack_ms, sample_rate);

        if self.voice_active {
            if score > release {
                self.holdoff_samples_remaining = holdoff_samples;
            } else if self.holdoff_samples_remaining > 0 {
                self.holdoff_samples_remaining = self.holdoff_samples_remaining.saturating_sub(frame_samples);
            } else {
                self.voice_active = false;
                self.attack_samples_accumulated = 0;
            }
        } else if score > trigger {
            self.attack_samples_accumulated = self.attack_samples_accumulated.saturating_add(frame_samples);
            if self.attack_samples_accumulated >= attack_samples_required {
                self.voice_active = true;
                self.holdoff_samples_remaining = holdoff_samples;
                self.attack_samples_accumulated = 0;
            }
        } else {
            self.attack_samples_accumulated = 0;
        }
        self.voice_active
    }
}

impl Default for VadShaper {
    fn default() -> Self {
        Self::new()
    }
}

fn ms_to_samples(ms: u32, sample_rate: u32) -> u32 {
    (ms as u64 * sample_rate as u64 / 1000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(trigger: f32, release: f32, attack_ms: u32, holdoff_ms: u32) -> VadShaperSettings {
        VadShaperSettings {
            trigger,
            release,
            attack_ms,
            holdoff_ms,
        }
    }

    #[test]
    fn activates_immediately_with_zero_attack() {
        let mut s = VadShaper::new();
        assert!(s.update(1.0, 480, 48000, &settings(0.5, 0.3, 0, 100)));
        assert!(s.is_active());
    }

    #[test]
    fn rejects_short_transient() {
        // 50ms attack at 48kHz = 2400 samples; one 480-sample chunk is below.
        let mut s = VadShaper::new();
        let cfg = settings(0.5, 0.3, 50, 100);
        assert!(!s.update(1.0, 480, 48000, &cfg));
        assert!(!s.is_active());
        // Reset by a quiet frame.
        s.update(0.0, 480, 48000, &cfg);
        // Sustained loud across ≥5 chunks should activate.
        for _ in 0..5 {
            s.update(1.0, 480, 48000, &cfg);
        }
        assert!(s.is_active());
    }

    #[test]
    fn hysteresis_holds_between_thresholds() {
        let mut s = VadShaper::new();
        let cfg = settings(0.6, 0.3, 0, 20);
        s.update(1.0, 480, 48000, &cfg);
        assert!(s.is_active());
        // Score between release and trigger: stays active indefinitely.
        for _ in 0..100 {
            assert!(s.update(0.45, 480, 48000, &cfg));
        }
        // Drop below release; holdoff drains, then deactivates.
        for _ in 0..5 {
            s.update(0.0, 480, 48000, &cfg);
        }
        assert!(!s.update(0.0, 480, 48000, &cfg));
    }

    #[test]
    fn release_above_trigger_collapses_to_single_threshold() {
        let mut s = VadShaper::new();
        let cfg = settings(0.5, 0.9, 0, 0);
        s.update(1.0, 480, 48000, &cfg);
        assert!(s.is_active());
        // With release clamped down to trigger, score just below trigger deactivates.
        assert!(!s.update(0.4, 480, 48000, &cfg));
    }
}
