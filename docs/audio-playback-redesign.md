# Audio Playback Redesign

Design note on the receive→playback path: what the current implementation got right,
the structural flaw behind the cpal/pulse buffer underruns, and the shape of a rebuild.
Companion to `docs/audio-subsystem.md` (which documents the system *as built*); this doc
is about *why* and *how to change it*.

The code referenced here lives in `crates/rumble-client/src/audio_task.rs` unless noted,
with device I/O in `crates/rumble-desktop/src/audio/{cpal,pulse}.rs`.

**Implementation status.** Steps 1, 2, 3, 4, and 6 have landed; step 5 is deferred.
- **Step 1 (done)** — the `Arc<Mutex<VecDeque<f32>>>` playback buffer is now an `rtrb`
  lock-free SPSC ring (`Producer` on the audio task, `Consumer` in the device callback). The
  real-time output callback no longer takes a general-purpose lock.
- **Step 2 (done)** — the producer is consumer-paced. A fast poll (`produce_interval`, 10 ms)
  refills the ring only to `PLAYBACK_TARGET_SAMPLES`, so production rate tracks the device's
  drain rate rather than the wall-clock timer — closing the drift. The 20 ms `mix_interval` is
  gone; `fill_playback_ring` / `mix_one_frame` / `push_frame` replace `mix_and_play_audio`.
- **Step 4 (done)** — `UserAudioState` is now a **timestamp-driven jitter buffer**, keyed by
  media frame index (`timestamp_us / FRAME_US`) instead of sequence. The TX side stamps a
  continuous media clock (`capture_frame_index`, advancing through silence) onto every datagram;
  the bridge and the server's plugin/echo relay stamp it too, so there is one receiver path, no
  legacy fallback. A gap is classified from the next frame's (seq, ts): sequence advancing as far
  as the media clock ⇒ loss (FEC/PLC); media clock jumping further ⇒ silence ⇒ re-anchor. This
  replaces the EOS-arrival-gap + sequence-restart heuristics (EOS is now advisory) and lets the
  onset prime fire on a missed EOS. The playout depth is adaptive (RFC 3550 jitter estimate,
  `target_frames` in `[MIN_TARGET_FRAMES, MAX_TARGET_FRAMES]`).
- **Step 3 (done, drop/insert variant)** — drift is bounded by the adaptive buffer: most
  sender-faster drift is absorbed for free when a spurt boundary re-anchors, and sustained drift
  during long continuous speech is bounded by dropping the oldest unplayed frame past
  `DRIFT_DROP_SLACK`. This keeps frames at 960 samples (trivial mixing). A *smooth* fractional
  resampler (WSOLA / continuous-phase) is the future refinement over single-frame drop/insert.
- **Step 6 (done)** — the mix sums into headroom and applies a soft-knee limiter (`soft_clip`,
  transparent below `SOFT_CLIP_THRESHOLD`) instead of a per-sample hard clamp, fixing the loud
  multi-speaker crackle. The `overlapping_speakers` characterization test is no longer `#[ignore]`.

- **Step 5 (deferred)** — moving decode/mix onto a thread separate from the network select loop.
  It is an optimization, not a correctness fix, and step 2 already removed the coupling's main
  harm (the producer is paced by the device, not stalled by network handling). Deferred as the
  lowest value / highest concurrency-risk item.

---

## The path as built

Receive → playback flows through these stages:

1. **Datagram recv** (`run_audio_task` select arm) → `handle_voice_datagram` →
   `UserAudioState::insert_packet` into a per-peer `BTreeMap<seq, opus>` jitter buffer.
2. **Mix tick** — a `tokio::time::interval(20ms)` (`mix_interval`) fires `mix_and_play_audio`
   → `mix_peer_frames`: for every ready peer, `decode_next_into` one 960-sample frame, run the
   RX pipeline, apply volume, sum-and-clamp into one frame, append to `playback_buffer`.
3. **`playback_buffer: Arc<Mutex<VecDeque<f32>>>`** — the single rendezvous between producer
   and consumer.
4. **Device callback** drains it: `PlaybackFiller::fill`, invoked from the pulse playback
   thread (`pulse.rs`) or the cpal output callback (`cpal.rs`).

The recent underrun work added the `PlaybackFiller` priming state machine (build a 60 ms
cushion, re-prime on starvation), the `MAX_PLAYBACK_BUFFER_SAMPLES` overflow drain, and the
`playback_producer_active` flag to keep the underrun stat honest. Those are symptom controls,
not a fix. The root cause is structural.

---

## Root cause: two clocks reconciled by a lossy elastic buffer

The producer (mix tick) is driven by a **`tokio::time::interval` — a software wall-clock
timer.** The consumer (device callback) drains at the **true hardware sample clock.** These
are two independent oscillators, and `playback_buffer` is the only thing between them. This is
the classic two-clock problem, and the design has no mechanism to reconcile them.

- **Wall clock ≠ audio clock.** Even if tokio fired at exactly 20 ms, "50 frames/sec by the
  system timer" is not the same rate as "48000 samples/sec by the DAC crystal." They drift. If
  the DAC consumes marginally faster than the timer produces, the buffer trends toward empty →
  **underrun (click)**. Marginally slower → the `9600`-sample cap drops about-to-play samples →
  **skip**. The drift is slow and continuous, so the glitches are *periodic* — exactly the
  underrun signature observed.
- **The timer shares a thread with everything else.** `mix_interval` is one arm of the same
  `tokio::select!` that also handles command processing, datagram recv, stats, and cleanup, all
  on a single current-thread runtime. A heavy datagram decode or a burst of commands delays the
  tick; `interval`'s default `MissedTickBehavior::Burst` then fires catch-up ticks back-to-back,
  shoving bursts of frames into the buffer.
- **Both corrections are lossy.** Underrun → insert silence + re-prime (another 60 ms of
  latency). Overflow → `drain(..excess)` drops samples. Neither closes the loop on the drift;
  they only bound each excursion. The 60 ms cushion masks short-term jitter, but once sustained
  drift eats it, you underrun, re-prime, and repeat — a glitch cadence proportional to the drift
  rate.

The correct model for playback is: **the device pulls, and everything upstream is paced by that
pull.** The device callback is the only clock that matters. The current design inverts this — a
free-running timer *pushes* into a buffer the device happens to drain — which is precisely what
forces the elastic-buffer-plus-lossy-correction band-aids.

### Three structural problems stacked on top

1. **Real-time-unsafe mutex in the device callback.** `PlaybackFiller::fill` takes
   `playback_buffer.lock()` on the *real-time audio thread*. The mix tick holds that same
   `std::sync::Mutex` while `buf.extend(...)` possibly reallocates the `VecDeque`. If the device
   callback lands while the mix tick holds the lock, the RT thread blocks — that *is* an underrun
   glitch. A latent priority-inversion hazard independent of the drift. Standard fix: a lock-free
   SPSC ring; the RT side never takes a general-purpose lock.

2. **Two elastic buffers in series, neither closed-loop.** The jitter buffer (fixed 3-packet /
   60 ms delay) and `playback_buffer` (3-frame cushion) are independent stages, and *neither*
   adapts to the consumer. The jitter delay is a hardcoded `jitter_buffer_delay_packets` that
   never responds to measured network jitter — a fixed prebuffer, not an adaptive jitter buffer.

3. **Sequence-counted, not timestamp-driven — so EOS becomes load-bearing.** `timestamp_us` is
   sent as `0` and ignored; the receiver reconstructs stream structure from sequence numbers +
   arrival wall-clock. Because of that, a lot of complexity exists to *guess* stream boundaries:
   DTX frames are sent CBR-style (consecutive sequence) purely so the receiver never sees a
   phantom gap (≈12 kbps wasted); and `insert_packet` carries three separate "new spurt"
   heuristics — explicit EOS, 60 ms gap-resume (lost EOS), and backward sequence-jump restart —
   each priming the decoder. EOS travels over unreliable datagrams and *is* frequently lost, so
   the gap heuristic does real work.

---

## What worked (keep it)

- **Per-peer long-lived decoder invariant.** Correct, well-documented, and the right call —
  re-init per spurt is the crackle bug it explicitly prevents. (See the invariant section of
  `docs/audio-subsystem.md`.)
- **Connection-scoped encoder + input stream** with `set_active()` gating instead of stream
  teardown. Avoids ALSA re-enumeration on every PTT cycle.
- **The Platform / `AudioBackend` / `VoiceCodec` trait split.** Lets `MockAudioBackend` and
  `MarkerDecoder` drive the real code paths in tests; this is why the jitter-buffer behavior is
  so testable.
- **Characterization tests that pin *observable* behavior** (played-frame sequence,
  loss/FEC/conceal counters) rather than internals, plus the real-Opus onset-pop and burst-loss
  tests measuring sample-to-sample discontinuity. This is what makes a rewrite tractable: a
  redesign that preserves the played-frame sequence and loss accounting keeps most tests green.
- **Snapshot channels for meter/stats off the state lock.** Good separation.
- **FEC-then-PLC loss handling** and decode-into-caller-buffer to avoid per-frame allocations.

## What didn't (change it)

- Wall-clock-timer producer vs hardware-clock consumer (the root cause).
- `std::sync::Mutex<VecDeque>` shared with the RT callback.
- Fixed, non-adaptive jitter buffer; sequence-based instead of timestamp-based.
- Unreliable EOS made load-bearing → DTX-as-CBR hack + triple spurt-detection heuristics.
- Decode/mix sharing a thread with network recv and command processing.
- The `PlaybackFiller` priming / overflow-drain / `producer_active` machinery — all compensation
  for the missing clock sync, not a feature.

---

## Redesign shape

**Principle: one clock. The device pulls; the producer is paced by the consumer's drain, never
by a wall timer.**

1. **Lock-free SPSC ring between mix and device** (`rtrb`). *(Done.)* The device callback does
   only a wait-free read of N samples, filling silence on empty — no `Mutex`, no allocation, no
   blocking on the RT thread. Kills the priority-inversion hazard outright.

2. **Pace the producer by the ring, not by `tokio::time::interval`.** *(Done, polled variant.)*
   Rather than waking on a consumer signal, a short poll (`produce_interval`) refills the ring to a
   fixed `PLAYBACK_TARGET_SAMPLES` each tick; because it fills to a fixed depth, the frames produced
   equal what the device drained, so production rate self-regulates to consumption rate and drift
   can't accumulate. The poll interval is a safety cadence, not the rate. (A consumer-notify variant
   would drop the poll, at the cost of a wake from the RT thread — deferred.) The target is set
   equal to the consumer's prime so the network cushion stays in the jitter buffer, not decoded
   ahead into the ring.

3. **Add a fractional resampler in the mix stage for true drift compensation.** The sender's
   48 kHz crystal ≠ the local DAC's. Hold the jitter buffer at a target depth and nudge the
   resample ratio by a few PPM when it trends long/short over seconds (NetEq-style). This is what
   makes it glitch-free *indefinitely* — a fixed cushion + hard clamp fundamentally cannot.

4. **Timestamp-driven, adaptive jitter buffer.** Populate and use a media timestamp (or derive
   `seq × 20 ms`). See the clock model below for what this does and does not buy. Size the target
   depth from measured inter-arrival jitter (e.g. p95 + decode margin, bounded), trading latency
   for loss dynamically.

5. **Move decode/mix off the network select loop.** Datagram recv feeds per-peer jitter buffers;
   the render-paced producer owns decode+mix. Network jitter then never stalls audio production,
   and vice versa.

6. **Replace sum-and-clamp mix with headroom + a soft limiter.** The `overlapping_speakers_clip`
   test is already `#[ignore]`'d documenting that two loud speakers hard-clamp into crackle. A
   small fixed headroom + soft-knee limiter fixes that and lets the test be un-ignored.

The encoder side and the trait abstractions stay nearly as-is — the rot is entirely in the RX
producer/consumer clocking and the sequence-vs-timestamp choice.

---

## The clock model (why timestamps help — and where they don't)

A common objection: we have no control over the remote system clock, and absolute offset between
two machines can drift for minutes. True — but an RTP-style **media timestamp is not a wall-clock
timestamp**, and it is never compared to the local clock.

It is a **sample counter**: each packet carries "this frame begins at sample offset T in *my own*
stream," T incrementing by 960 per 20 ms frame from an arbitrary sender-chosen origin. We only
ever do arithmetic *between the sender's own timestamps* — `ts(B) − ts(A)` — never
`our_now − their_ts`. So absolute offset and minutes of drift are simply never in any equation.

There are two orthogonal problems, solved by different mechanisms:

### Problem A — structure within the stream (timestamps solve this)

Is a packet we expected but didn't get a **loss** (conceal) or **intended silence** (play
nothing)? Is an arriving packet a continuation or a **new talkspurt** (re-anchor, re-prime)? The
clean encoding is RTP's: **sequence** increments +1 per packet (sole job: detect loss),
**timestamp** increments by samples-elapsed (sole job: spacing/silence). Then:

- seq +5, timestamp +5 frames → 4 packets **lost**, conceal.
- seq +1, timestamp +5 frames → sender **skipped 4 frames of silence** (DTX), play nothing — not
  loss.

The second case is impossible to distinguish with sequence numbers alone, which is exactly why
the current code sends CBR silence. Timestamps give real DTX savings *and* unambiguous loss
detection at once. None of this touches the remote clock.

### Problem B — crystal drift between the two machines (timestamps do NOT solve this)

Handled separately, and *also* without trusting the remote clock: the control signal is your own
jitter-buffer depth. If the sender's crystal is slightly slow, packets arrive a hair slower than
the DAC drains, so buffer occupancy trends toward empty regardless of any timestamp. The
controller watches that trend over seconds and nudges the resampler. The input is buffer depth,
measured locally; the remote timestamp is never an input. So "minutes of offset" is irrelevant —
only the *rate* mismatch matters, and it shows up as buffer drift you can see locally.

You need both: buffer-depth control handles *rate* but can't tell silence from loss; timestamps
handle *structure* but say nothing about rate.

---

## The underrun-instant decision (what timestamps can't do)

The sharpest case: the buffer is empty, you must emit a sample *now*, and the next packet — the
one whose timestamp would classify the gap — hasn't arrived. At that instant the timestamp cannot
help; the deciding information does not exist yet. The decision is irreducibly a guess, in three
tiers:

**Tier 1 — the instant: guess, cheaply.** PLC. Not because you know it's loss, but because PLC is
right for a *short* gap of either cause: continuing speech → PLC is correct; silence-onset →
Opus PLC decays toward comfort-noise/silence within a frame or two, nearly inaudible. Being wrong
over 1–2 frames costs almost nothing. Timestamps change nothing here.

**Tier 2 — bounded conceal, then give up: the timeout is irreducible.** After a few PLC frames
with nothing arriving, fade to silence and stop. This *is* the 60 ms-style wall-clock timeout, and
it **survives the redesign**. Absence of data is the only signal, and absence carries no
timestamp — you cannot distinguish "sender stopped, EOS lost" from "long network stall" except by
waiting and giving up. A timeout capping PLC is fundamental, not a wart.

**Tier 3 — when data resumes: timestamps earn their keep.** The next packet's timestamp classifies
the gap *retroactively and definitively*:

- timestamp **contiguous** with the last played frame → genuine **loss**; the PLC was correct;
  count it.
- timestamp **jumped forward** by the gap duration → the sender was **silent** (DTX); concealment
  covered nothing real; don't count loss, and treat this as a new talkspurt onset (re-prime,
  re-anchor).

This is what the current code reconstructs with proxies ("backward jump = restart, forward gap =
loss, 60 ms arrival gap = new spurt, EOS marker"). One timestamp delta replaces the pile, and it
matters for honest stats, clean re-anchoring, and not corrupting decoder continuity by concealing
*across* a talkspurt boundary.

### Reducing how often Tier 1 is hit at all

Two local levers, separate from timestamps:

1. **Adaptive buffer depth.** Size the target above measured jitter so normal jitter never drains
   to empty *during* a talkspurt. Then buffer-empty becomes weak probabilistic evidence of "stream
   likely ended" rather than "just jitter" — not proof, but it biases the Tier-1 guess correctly
   and cuts false PLC. This is the latency-vs-glitch knob.

2. **Lookahead for FEC.** Opus in-band FEC recovers packet N from a copy in N+1 — but only if N+1
   has *already arrived*, i.e. only with buffer ahead of the playout point. At the bleeding edge of
   an empty buffer there is no lookahead, so FEC can't fire and you're stuck with PLC. A deeper
   buffer routes more gaps through clean FEC recovery instead of the lossy PLC guess.

---

## Summary

- The underruns are not a tuning bug; they are the inevitable result of a wall-clock-timer
  producer racing a hardware-clock consumer across a lossy elastic buffer.
- Fix the clocking first: device-pulls, producer-paced-by-the-ring (lock-free SPSC), plus a
  PPM resampler driven by local buffer depth. This is what actually removes the glitch class.
- Adopt media timestamps to replace the EOS + gap-heuristic pile and drop the DTX-as-CBR tax —
  understanding that they classify gaps *after* the fact, not at the underrun instant, and that a
  bounded-PLC-then-silence timeout remains fundamental.
- Keep the decoder-lifetime invariant, the trait split, and the characterization tests; they make
  the rebuild safe.
