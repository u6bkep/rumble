//! Pre-roll lookback ring for the TX VAD gate.
//!
//! While the VAD gate is closed (Continuous mode, `pipeline_result.suppress`),
//! the capture callback stashes the processed (post-pipeline) PCM frames here
//! instead of discarding them. When the gate opens, the ring is flushed
//! through the normal encode path *ahead of* the frame that opened the gate,
//! so speech onsets that scored below the trigger (unvoiced fricatives,
//! plosives, the `attack_ms` window) are transmitted retroactively instead of
//! clipped. In PushToTalk mode the gate is bypassed entirely
//! (`vad_gate_bypass`), so no frame is ever suppressed and the ring stays
//! empty — pre-roll is a Continuous/VAD-mode mechanism only.
//!
//! # Why raw PCM, not encoded packets
//!
//! Buffering pre-encode keeps a single encode path: flushed frames go through
//! the same connection-scoped Opus encoder, in capture order, immediately
//! before the gate-opening frame. Since the ring frames are *consecutive*
//! capture frames (contiguity is enforced on push), the encoder sees a
//! contiguous PCM stream across the flush — no encoder-state discontinuity is
//! introduced mid-spurt. Buffering encoded packets instead would require
//! encoding while suppressed (paying encode cost during silence, and emitting
//! bitstream that predicts from frames the receiver will never get when the
//! ring overwrites).
//!
//! # Timestamp semantics
//!
//! Each stored frame keeps the media-clock timestamp (`capture_frame_index`
//! × `FRAME_US`) it was captured with, and is sent with that original
//! timestamp; sequence numbers are assigned at send time in flush order, so
//! the flushed spurt is N consecutive sequence numbers covering N consecutive
//! media frames. The spurt's first timestamp therefore moves *back* by the
//! pre-roll length, and the receive side needs no special handling:
//!
//! - **Gap longer than the ring** (normal case): the first flushed frame
//!   still sits ≥ 2 media frames after the last played frame while the
//!   (EOS-discounted) sequence span is 1, so `UserAudioState`'s
//!   loss-vs-silence classifier reads the gap as intended silence and
//!   re-anchors playout at the first *flushed* frame — onset prime and
//!   re-buffering happen exactly as they would for a non-pre-rolled spurt,
//!   just anchored ≤ 100 ms earlier.
//! - **Gap that fits in the ring**: every suppressed frame is flushed, the
//!   stream is timestamp-contiguous with the previous spurt, and the receiver
//!   plays straight through with no re-anchor — a short VAD dropout becomes
//!   genuinely gapless (the encoder input was contiguous too).
//!
//! # Capacity bound (why 5 frames / 100 ms)
//!
//! Flushing N ring frames plus the gate-opening frame lands N+1 datagrams
//! back-to-back in the receiver's jitter buffer, usually within one mix tick.
//! While `started`, `UserAudioState::insert_packet` trims depth to
//! `target_frames + DRIFT_DROP_SLACK` (minimum 2 + 4 = 6), advancing the
//! playout cursor past trimmed frames — a burst larger than 6 would get its
//! *head* (the onset audio we're trying to save) silently dropped, and the
//! cursor force-advance would skip the silence classification that does the
//! onset re-anchor + decoder prime. 5 + 1 = 6 never trims, even with leftover
//! unplayed frames from the previous spurt (bounded by the same target, which
//! drains during the ≥ 6-frame gap that any non-contiguous flush implies). A
//! larger pre-roll needs flush pacing or an RX-side burst allowance — don't
//! raise [`PRE_ROLL_FRAMES`] without revisiting that analysis (there is a
//! compile-time assert next to the RX constants in `audio_task.rs`).
//!
//! # Hot-path discipline
//!
//! All slots are preallocated; `push` reuses each slot's sample buffer
//! (`clear` + `extend_from_slice`), so after the first lap around the ring
//! the audio callback does not allocate.

/// Number of suppressed 20 ms frames retained for retroactive transmission
/// (100 ms). See the module docs for why this must not exceed the receiver's
/// minimum burst absorption.
pub(crate) const PRE_ROLL_FRAMES: usize = 5;

struct Slot {
    timestamp_us: u64,
    samples: Vec<f32>,
}

/// Fixed-capacity ring of recently suppressed capture frames, oldest-first.
///
/// Contiguity invariant: the stored frames are always a run of consecutive
/// media-clock frames. A pushed frame that is not the immediate successor of
/// the newest stored frame (capture re-activation fast-forwarded the clock,
/// warm-up frames were discarded, …) resets the ring first, so a flush can
/// never emit stale audio from a previous capture run.
pub(crate) struct PreRollRing {
    slots: Vec<Slot>,
    /// Index of the oldest occupied slot.
    head: usize,
    /// Number of occupied slots.
    len: usize,
    /// Media-clock step between consecutive frames, in microseconds.
    frame_us: u64,
}

impl PreRollRing {
    pub(crate) fn new(capacity: usize, frame_us: u64) -> Self {
        Self {
            slots: (0..capacity)
                .map(|_| Slot {
                    timestamp_us: 0,
                    // Sized for a 20 ms stereo-ish frame; grows once if the
                    // device delivers bigger frames, then stays.
                    samples: Vec::with_capacity(1024),
                })
                .collect(),
            head: 0,
            len: 0,
            frame_us,
        }
    }

    fn newest_timestamp(&self) -> Option<u64> {
        (self.len > 0).then(|| self.slots[(self.head + self.len - 1) % self.slots.len()].timestamp_us)
    }

    /// Stash one suppressed frame. Evicts the oldest frame when full; resets
    /// the ring first if `timestamp_us` is not contiguous with the newest
    /// stored frame (see the struct docs).
    pub(crate) fn push(&mut self, timestamp_us: u64, samples: &[f32]) {
        let cap = self.slots.len();
        if cap == 0 {
            return;
        }
        if let Some(newest) = self.newest_timestamp()
            && newest + self.frame_us != timestamp_us
        {
            self.len = 0;
        }
        if self.len == cap {
            self.head = (self.head + 1) % cap;
            self.len -= 1;
        }
        let slot = &mut self.slots[(self.head + self.len) % cap];
        slot.timestamp_us = timestamp_us;
        slot.samples.clear();
        slot.samples.extend_from_slice(samples);
        self.len += 1;
    }

    /// Drain the buffered frames oldest-first into `emit(timestamp_us,
    /// samples)`, but only if the run ends exactly one frame before
    /// `current_timestamp_us` — i.e. the buffered audio is the immediate
    /// prefix of the frame that opened the gate. A non-contiguous ring is
    /// stale (capture was re-activated since it was filled) and is discarded
    /// instead. The ring is empty afterwards either way.
    pub(crate) fn flush_into(&mut self, current_timestamp_us: u64, mut emit: impl FnMut(u64, &[f32])) {
        if let Some(newest) = self.newest_timestamp()
            && newest + self.frame_us == current_timestamp_us
        {
            let cap = self.slots.len();
            for i in 0..self.len {
                let slot = &self.slots[(self.head + i) % cap];
                emit(slot.timestamp_us, &slot.samples);
            }
        }
        self.len = 0;
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processors::vad_shaper::{VadShaper, VadShaperSettings};

    const FRAME_US: u64 = 20_000;

    fn ts(frame: u64) -> u64 {
        frame * FRAME_US
    }

    /// A one-sample "frame" whose value identifies its media frame index, so
    /// tests can assert both ordering and payload identity.
    fn frame_payload(frame: u64) -> [f32; 1] {
        [frame as f32]
    }

    fn collect_flush(ring: &mut PreRollRing, current_frame: u64) -> Vec<(u64, Vec<f32>)> {
        let mut out = Vec::new();
        ring.flush_into(ts(current_frame), |t, s| out.push((t, s.to_vec())));
        out
    }

    /// Gate opens → the previously suppressed frames are emitted first, in
    /// capture order, each with its original media timestamp.
    #[test]
    fn flush_emits_suppressed_frames_in_order_with_original_timestamps() {
        let mut ring = PreRollRing::new(PRE_ROLL_FRAMES, FRAME_US);
        for f in 10..14 {
            ring.push(ts(f), &frame_payload(f));
        }
        let emitted = collect_flush(&mut ring, 14);
        assert_eq!(emitted.len(), 4);
        for (i, (t, samples)) in emitted.iter().enumerate() {
            let f = 10 + i as u64;
            assert_eq!(*t, ts(f), "timestamps preserved in order");
            assert_eq!(samples.as_slice(), &frame_payload(f), "payload matches frame");
        }
        // The ring is empty after a flush: no double emission.
        assert!(collect_flush(&mut ring, 15).is_empty());
    }

    /// The ring is bounded: pushing more than the capacity keeps only the
    /// newest `PRE_ROLL_FRAMES` frames (the lookback window slides).
    #[test]
    fn ring_bounded_keeps_newest_frames() {
        let mut ring = PreRollRing::new(PRE_ROLL_FRAMES, FRAME_US);
        for f in 0..20 {
            ring.push(ts(f), &frame_payload(f));
            assert!(ring.len() <= PRE_ROLL_FRAMES, "ring never exceeds capacity");
        }
        let emitted = collect_flush(&mut ring, 20);
        let frames: Vec<u64> = emitted.iter().map(|(t, _)| t / FRAME_US).collect();
        assert_eq!(frames, vec![15, 16, 17, 18, 19], "only the newest 100 ms survive");
    }

    /// A timestamp jump on push (capture re-activation fast-forwarded the
    /// media clock) invalidates the stale run; only the contiguous tail is
    /// kept.
    #[test]
    fn discontinuous_push_resets_stale_run() {
        let mut ring = PreRollRing::new(PRE_ROLL_FRAMES, FRAME_US);
        ring.push(ts(5), &frame_payload(5));
        ring.push(ts(6), &frame_payload(6));
        // Clock jumps (e.g. mute → unmute fast-forward).
        ring.push(ts(300), &frame_payload(300));
        let emitted = collect_flush(&mut ring, 301);
        let frames: Vec<u64> = emitted.iter().map(|(t, _)| t / FRAME_US).collect();
        assert_eq!(frames, vec![300], "pre-jump frames must not flush");
    }

    /// A flush whose current frame is not the immediate successor of the
    /// newest stored frame discards the (stale) ring instead of emitting it.
    #[test]
    fn flush_with_noncontiguous_current_frame_discards() {
        let mut ring = PreRollRing::new(PRE_ROLL_FRAMES, FRAME_US);
        ring.push(ts(5), &frame_payload(5));
        ring.push(ts(6), &frame_payload(6));
        // Gate opens much later (stored frames are not the onset's prefix).
        assert!(collect_flush(&mut ring, 100).is_empty());
        // And the stale frames are gone for good.
        assert!(collect_flush(&mut ring, 7).is_empty());
    }

    /// Zero-capacity ring is inert (defensive; not used in production).
    #[test]
    fn zero_capacity_ring_is_inert() {
        let mut ring = PreRollRing::new(0, FRAME_US);
        ring.push(ts(1), &frame_payload(1));
        assert!(collect_flush(&mut ring, 2).is_empty());
    }

    /// End-to-end gate simulation mirroring the capture-callback wiring:
    /// suppressed frames are pushed, the gate-opening frame triggers a flush.
    /// With `attack_ms > 0` the *decision* is delayed by the attack window,
    /// but no audio is lost — the attack-window frames come out of the ring.
    #[test]
    fn attack_delays_decision_without_losing_audio() {
        let mut shaper = VadShaper::new();
        // 40 ms attack = 2 × 20 ms frames at 48 kHz.
        let cfg = VadShaperSettings {
            trigger: 0.5,
            release: 0.3,
            attack_ms: 40,
            holdoff_ms: 100,
        };
        let mut ring = PreRollRing::new(PRE_ROLL_FRAMES, FRAME_US);
        let mut transmitted: Vec<u64> = Vec::new();

        // Frames 0..3 silent, speech starts at frame 3.
        for f in 0u64..8 {
            let score = if f >= 3 { 1.0 } else { 0.0 };
            let active = shaper.update(score, 960, 48000, &cfg);
            if !active {
                ring.push(ts(f), &frame_payload(f));
            } else {
                ring.flush_into(ts(f), |t, _| transmitted.push(t / FRAME_US));
                transmitted.push(f);
            }
        }

        // The gate opened on frame 4 (two voiced frames accumulate the 40 ms
        // attack), but the flush recovered every frame back to frame 0 — in
        // particular the voiced frame 3 and the attack frame, so the spurt
        // onset is intact and in order.
        assert_eq!(transmitted, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    /// Silence stays silent: if the gate never opens, nothing is ever
    /// emitted, and the ring stays bounded.
    #[test]
    fn no_gate_open_no_flush() {
        let mut shaper = VadShaper::new();
        let cfg = VadShaperSettings {
            trigger: 0.5,
            release: 0.3,
            attack_ms: 0,
            holdoff_ms: 100,
        };
        let mut ring = PreRollRing::new(PRE_ROLL_FRAMES, FRAME_US);
        let mut transmitted = 0usize;

        for f in 0u64..50 {
            let active = shaper.update(0.0, 960, 48000, &cfg);
            if !active {
                ring.push(ts(f), &frame_payload(f));
            } else {
                ring.flush_into(ts(f), |_, _| transmitted += 1);
                transmitted += 1;
            }
        }
        assert_eq!(transmitted, 0, "no frame may leave the client while suppressed");
        assert!(ring.len() <= PRE_ROLL_FRAMES);
    }
}
