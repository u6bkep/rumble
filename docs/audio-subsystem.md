# Audio Subsystem

Reference for rumble's voice path: capture → encode → send, and receive → jitter buffer → decode → mix → playback. The bulk of the logic lives in the **Audio Task** (`crates/rumble-client/src/audio_task.rs`), a dedicated thread/runtime owned by `BackendHandle`. The codec is abstracted behind `rumble-client-traits` and implemented natively in `crates/rumble-desktop/src/codec.rs` (libopus). The TX/RX processor framework (gain, denoise, VAD) is the `rumble-audio` crate. Device I/O is the `AudioBackend` trait, implemented for desktop in `crates/rumble-desktop/src/audio/`.

---

## Format

- **48 kHz, mono, 20 ms frames = 960 samples.** Constants in `rumble-client-traits/src/codec.rs`: `OPUS_SAMPLE_RATE = 48000`, `OPUS_FRAME_SIZE = 960`, `OPUS_MAX_PACKET_SIZE = 4000`, `OPUS_DEFAULT_BITRATE = 64000`. Mono is fixed in the codec impl (`codec.rs`: `OPUS_CHANNELS = Channels::Mono`).
- **Encode** happens in the capture callback (`audio_task.rs` `start_transmission`, the closure passed to `open_input`), one Opus packet per processed 960-sample frame.
- **Decode** happens per remote peer in `UserAudioState::decode_next_into` (`audio_task.rs`), driven by the playback producer as it tops up the device ring (see Playback path).
- Encoder application is `Voip`, VBR + DTX + inband-FEC enabled (`NativeOpusEncoder::configure`). 50 frames/sec → stats compute bitrate as `avg_frame_bytes * 50 * 8`.

---

## The Audio Task

Spawned by `spawn_audio_task::<P>()` (`audio_task.rs`, called from `handle.rs:309`) on its own OS thread running a **current-thread tokio runtime** — keeps blocking audio I/O off the main async executor. Driven by a single `tokio::select!` loop (`run_audio_task`) over:

- `command_rx` — `AudioCommand`s from `BackendHandle` (device/mute/PTT/room/settings).
- `encoded_rx` — `CaptureMessage` (`EncodedFrame` / `EndOfStream`) produced by the capture callback; the loop wraps each into a `VoiceDatagram` and sends it.
- `recv_datagram()` — incoming voice datagrams (only polled when a connection exists).
- `produce_interval` (10 ms) — top the playback ring back up to its target depth: decode/mix/limit one frame per ready peer at a time until the ring is at `PLAYBACK_TARGET_SAMPLES` (consumer-paced; see Playback path).
- `cleanup_interval` (500 ms) — expire `talking_users` UI state (does **not** drop decoders).
- `stats_interval` (500 ms) — roll up `AudioStats` into the stats snapshot.

```
 capture cb ──encode──> encoded_tx ─┐
 (cpal/pulse thread)                │ mpsc
                                    ▼
                         [select loop] ──VoiceDatagram──> QUIC datagram (send)
                                    ▲
   QUIC datagram (recv) ───────────┘
        │
        ▼ handle_voice_datagram
   per-peer jitter buffer (BTreeMap<media-frame, packet>)
        │ produce_interval (10ms) → fill_playback_ring → decode_next_into → RX pipeline → volume → soft-limit mix
        ▼
   playback ring (rtrb SPSC f32) ──fill cb──> cpal/pulse output
```

### TX path

- The **input stream is connection-scoped**: created once on `ConnectionEstablished`, destroyed on disconnect / device / settings / TX-pipeline change. Mute, server-mute, and PTT do **not** recreate it — they toggle `AudioCaptureStream::set_active()` via the `sync_transmission!` macro. This avoids ALSA device re-enumeration on every PTT cycle.
- The **encoder is also connection-scoped** (`Arc<Mutex<Option<Enc<P>>>>`), created on `ConnectionEstablished`, persisting across all talk spurts so Opus keeps its DTX/VBR state. Settings changes call `apply_settings` in place rather than rebuilding.
- Capture callback per frame: discard first 3 frames (stale hardware ringbuffer); measure pre-pipeline RMS; run the TX `AudioPipeline`; measure post-pipeline RMS; if `suppress` (ignored in PTT mode — see Transmission Gating) → emit `EndOfStream` (on the active→suppressed edge), stash the processed frame in the **pre-roll ring**, and return; else flush the pre-roll ring (see VAD Pre-Roll below) and encode/send `EncodedFrame`.
- Outgoing `VoiceDatagram` (proto `api.proto`) carries `sender_id`, a **client-owned `sequence`** (`send_sequence`, incremented per sent packet including DTX frames), `opus_data`, `end_of_stream`, and a **media `timestamp_us`**. The timestamp is a continuous media clock (`capture_frame_index` × `FRAME_US`) that advances per captured frame *including silence* — VAD-suppressed frames advance it, and a mute/PTT gap fast-forwards it to real elapsed time on the next activation. So a silence gap shows up as a timestamp jump, which is what lets the receiver tell silence from loss. `room_id` is left unset (server infers room). The Mumble bridge and the server's plugin/echo relay also stamp a media timestamp, so every producer emits one.

### RX path

`handle_voice_datagram::<P::Codec>` (`audio_task.rs`): drops own/locally-muted senders, lazily creates a `UserAudioState` if none exists, then `insert_packet(seq, timestamp_us, opus)` into that peer's jitter buffer (keyed by media frame index). `end_of_stream` datagrams call `mark_end_of_stream` (advisory). The 10 ms `produce_interval` then runs `fill_playback_ring`, which for each ready peer decodes one frame (`decode_next_into`), runs the per-peer RX pipeline, applies per-user volume (dB), and sums into the mix; the summed peers + any queued SFX are passed through a soft-knee limiter (`soft_clip`) rather than a hard clamp, and pushed to the per-stream `rtrb` playback ring (capacity `MAX_PLAYBACK_BUFFER_SAMPLES = 9600`, filled only to `PLAYBACK_TARGET_SAMPLES`).

### Playback path (device-clocked ring)

The hand-off from the audio task to the output device is a **lock-free SPSC ring** (`rtrb`): the audio task holds the `Producer`, the device callback owns the `Consumer` (inside `PlaybackFiller`). The real-time callback only does a wait-free read — no `Mutex`, no allocation.

- **Consumer-paced production.** There is no free-running mix timer feeding the ring. `produce_interval` (10 ms) is a poll; on each tick `fill_playback_ring` produces frames only until the ring is back at `PLAYBACK_TARGET_SAMPLES`. Because it fills to a fixed depth, the number of frames produced equals what the device drained — so production rate tracks the device's audio clock, not wall-clock, which is what eliminated the buffer-underrun drift. With no output stream open (deafened / device failed), the producer is `None` and nothing is produced.
- **`PlaybackFiller`** primes a small cushion (`PLAYBACK_PRIME_SAMPLES`) before playout and re-primes on a starvation that occurs *while a stream is live* (`producer_active`), so the device callback never clicks on a thin buffer and the underrun stat stays honest (a cushion drain after a spurt ends is not counted).
- The producer/stream pair is created and torn down together per output stream (`open_playback!`), since each ring has exactly one producer and one consumer.

---

## ⚠️ Per-Peer Opus Decoder Lifetime (critical invariant)

**Each remote peer gets exactly ONE long-lived Opus decoder that persists for the entire session.** It lives in `UserAudioState.decoder` keyed by `user_id` in `HashMap<u64, UserAudioState<Dec<P>>>` (`user_audio` in `run_audio_task`).

A decoder is **only** dropped when the peer genuinely goes away:
- `AudioCommand::UserLeftRoom` / `AudioCommand::PeerLeft` — peer removed.
- `AudioCommand::RoomChanged` — all decoders cleared, rebuilt for the new room's users.
- `AudioCommand::ConnectionClosed` / deafen — full `user_audio.clear()`.

It is created proactively on `UserJoinedRoom` / `RoomChanged`, or lazily on first datagram (`handle_voice_datagram`) via `P::Codec::create_decoder()`.

**It must NOT be re-created per packet, per talk spurt, or on silence/EOS.** The buffer explicitly preserves the decoder across:
- silence → new spurt (re-anchors playout; primes via `needs_prime` — a discarded PLC frame),
- detected sender restart (media-clock jump backward),
- the stale-stream timeout.

`cleanup_stale_users` clears only the `talking_users` UI set, never the decoder map. Re-initializing decoders per packet/talkspurt produces `native codec: decoder initialized` log spam (emitted by `NativeOpusDecoder::new` in `codec.rs`) and audible crackle/pop at speech onset, because the decoder loses its internal continuity state. Treat any per-spurt decoder construction as a bug.

---

## Jitter Buffer (timestamp-driven)

Per-peer, in `UserAudioState` (`audio_task.rs`). Keyed by the sender's **media clock**, not by sequence — see `docs/audio-playback-redesign.md` for the rationale.

- `jitter_buffer: BTreeMap<u64, BufferedPacket>` keyed by media frame index (`timestamp_us / FRAME_US`); each `BufferedPacket` keeps the sender `seq` alongside the Opus payload. `next_play_frame` is the playout cursor on that clock.
- **Adaptive startup depth:** playback begins once the buffered depth reaches `target_frames`, which adapts to an RFC 3550 interarrival-jitter estimate (`update_jitter`), clamped to `[MIN_TARGET_FRAMES (2), MAX_TARGET_FRAMES (12)]`. The floor is `AudioSettings::jitter_buffer_delay_packets` (default 3). `started` clears on re-anchor so each new spurt re-buffers.
- **Loss vs silence classification (`decode_next_into`):** if the frame at the cursor is present → decode. If missing, look at the next buffered frame `(seq_n, frame_n)`: if the sequence advanced as many steps as the media clock (`seq_span == media_span`) the gap is **loss** — recover from the immediate successor's in-band **FEC** (`decode_fec`), else **PLC** (`decode_plc`); if the media clock jumped further (`seq_span < media_span`) the sender was **silent** → **re-anchor** to `frame_n` (re-buffer + prime), not concealed. A trailing gap with nothing buffered ahead is concealed but not counted as confirmed loss. Stats: `packets_lost`, `packets_recovered_fec`, `frames_concealed`.
- **Drift handling:** sender-faster clock drift is mostly absorbed for free at spurt re-anchors; sustained drift in long continuous speech is bounded by dropping the oldest unplayed frame once depth exceeds `target_frames + DRIFT_DROP_SLACK`. Hard cap `JITTER_BUFFER_MAX_PACKETS = 20` is the backstop.
- **PLC priming:** on re-anchor a discarded PLC frame warms the decoder (`needs_prime`) to avoid onset clicks — this now fires on a missed EOS too, since the timestamp jump alone triggers the re-anchor.
- **Stale-stream guard:** if no packet for `STREAM_STALE_THRESHOLD` (200 ms) and no EOS, the stream is treated as ended so PLC doesn't run forever.
- **Sender restart:** a media-clock jump backward by more than `RESTART_BACKWARD_FRAMES` clears the buffer and re-anchors (reconnect / new capture epoch), preserving the decoder.

EOS is **advisory**: `mark_end_of_stream` stops PLC promptly once the buffer drains, but a lost EOS is covered by both the stale guard and the silence classification. (DTX silence frames are still sent as-is today; the timestamp would also permit skipping them — a future TX optimization.)

---

## Pluggable Processor Pipeline (`rumble-audio`)

`rumble-audio/src/lib.rs` defines the framework; concrete processors live in `crates/rumble-client/src/processors/`.

- **`AudioProcessor` trait:** `process(&mut [f32], sample_rate) -> ProcessorResult` operating in-place on `[-1.0, 1.0]` frames. `ProcessorResult` carries only `suppress` (a control flag) — level metering is **not** done by processors anymore (see Metering).
- **`AudioPipeline`:** ordered `Vec<Box<dyn AudioProcessor>>`. `process` runs each enabled stage in sequence; `suppress` is the **OR** of all stages (any stage can gate the frame).
- **`ProcessorRegistry` + `ProcessorFactory`:** type-ID-keyed (`builtin.gain`, `builtin.denoise`, `builtin.vad`) factories; pipelines are built from serializable `PipelineConfig`/`ProcessorConfig` (JSON settings). `register_builtin_processors` (`processors/mod.rs`) registers the three built-ins.

Built-in processors:

| type_id | impl | role |
|---|---|---|
| `builtin.gain` | `processors/gain.rs` | dB volume adjust |
| `builtin.denoise` | `processors/denoise.rs` | RNNoise (nnnoiseless) denoise **+** ML voice gate; when `vad_enabled` (default true) it drives `suppress` from the RNNoise VAD score |
| `builtin.vad` | `processors/vad.rs` | legacy energy-only VAD (`threshold_db`/`release_threshold_db` + hangover); registered but **disabled** by default |

Default TX pipeline (`DEFAULT_TX_PIPELINE`, `processors/mod.rs`): `gain` (on) → `denoise` (on) → `vad` (off). So fresh users get RNNoise as their gate, with the energy VAD available ML-free.

- **TX pipeline** is one shared pipeline (built from `tx_pipeline_config`) run in the capture callback.
- **RX pipeline** is per-peer (`UserAudioState.rx_pipeline`), built from `rx_pipeline_defaults` or a per-user `UserRxConfig::pipeline_override`; on RX, `suppress` drops that playback frame (noise gate). Per-user `volume_db` is applied separately, outside the pipeline.

---

## Transmission Gating: VAD vs Push-to-Talk

VAD is **not** a voice mode — it is a pipeline processor. The two layers are orthogonal:

1. **Voice mode** (`VoiceMode::PushToTalk | Continuous`, `events.rs`) plus mute/server-mute decide whether the capture stream is *active* at all — `should_capture()` + `sync_transmission!`. In PTT mode capture is active only while `ptt_active`; in Continuous it's always active when connected and unmuted.
2. When capture is active, the **TX pipeline's `suppress`** (typically from denoise/VAD) gates each individual frame: suppressed frames aren't encoded/sent, and the active→suppressed edge emits an `EndOfStream`. **Exception: in PushToTalk mode the suppress flag is ignored** (`vad_gate_bypass`) — the held key is the user's explicit voice-activity signal (the Mumble/Discord convention), so whispers, breaths, and non-voice audio transmit while held. Denoise still processes the samples; only the gate decision is overridden, and it still drives the meters/gate lamp.

`SetMuted`/`SetServerMuted`/`SetDeafened` are tracked as separate flags so server-mute toggles never clobber the user's own self-mute. Deafen implies mute and tears down the playback stream + clears decoders.

### VAD Pre-Roll (onset recovery)

Frames suppressed by the gate are not discarded outright: the capture callback keeps the last 5 processed frames (100 ms) in a pre-roll ring (`crates/rumble-client/src/preroll.rs`). When the gate opens, the ring is flushed through the normal encode path *ahead of* the gate-opening frame, each frame with its **original media timestamp**, so low-scoring speech onsets (fricatives, plosives) and the `attack_ms` window are transmitted retroactively instead of clipped — `attack_ms` delays the gate *decision*, not the audio (up to the 100 ms pre-roll bound). On the receive side this needs no special handling: the spurt simply anchors up to 100 ms earlier (silence classification + re-anchor as usual), and a suppressed gap short enough to fit the ring is timestamp-contiguous and plays through gaplessly. The 5-frame capacity is deliberately the largest burst the RX jitter buffer absorbs without trimming (`MIN_TARGET_FRAMES + DRIFT_DROP_SLACK = 6` ≥ 5 ring frames + 1 current); a compile-time assert in `audio_task.rs` pins this. PTT mode never suppresses, so the ring stays empty there.

---

## Device Handling & Hotplug

- `AudioBackend` trait (`rumble-client-traits`) → `DesktopAudioBackend` (`rumble-desktop/src/audio/mod.rs`), which **prefers PulseAudio** on Linux (feature `pulse`, talks to PipeWire-pulse) and falls back to **cpal/ALSA**; cpal on macOS/Windows.
- `AudioCommand::RefreshDevices` enumerates input/output devices and emits `VoiceEvent::DevicesEnumerated`. `SetInputDevice`/`SetOutputDevice` restart the relevant stream on the new device.
- The cpal backend (`audio/cpal.rs`) picks the best supported config matching 48 kHz/mono, and otherwise **resamples** (linear interp, `resample()`) and down/up-mixes channels. `InputProcessor` accumulates samples into exact 960-sample frames before invoking `on_frame`. `set_active(false)` makes the capture callback early-return.
- There is **no explicit device-hotplug/auto-reconnect** mechanism in this code: the backend choice is fixed for the value's lifetime (drop+rebuild to re-detect a sound-server restart, per the `mod.rs` doc comment), and recovery from a yanked device relies on the user re-selecting via `SetInputDevice`/`SetOutputDevice` or `RefreshDevices`. Stream errors are only logged (`err_fn`).

---

## Metering & Stats (snapshots to the UI)

Two single-writer/multi-reader latest-value snapshot channels (`crates/rumble-client/src/snapshot.rs`), sampled by the UI on repaint — deliberately **off** the projection event bus so they don't take the `State` lock per tick.

- **`MeterSnapshot`** (`meter.rs`): published once per capture frame (~50 Hz) from the capture callback. Two taps — `input_pre` (raw mic RMS, "is the mic alive") and `input_post` (after TX pipeline, "what's being transmitted"), each a `Level::{Unmeasured, Db(f32)}`. Read via `BackendHandle::meter()`. Transmit state is intentionally not on the snapshot — it's an edge-triggered `VoiceEvent::TransmittingChanged` folded into `AudioState::is_transmitting`.
- **`AudioStats`** (`events.rs`): rolled up every 500 ms in `update_stats` — packets sent/received/lost, FEC recoveries, frames concealed, bytes, avg frame size, derived bitrate, total buffered packets. Read via `BackendHandle::stats()`.

`talking_users` (the per-user "speaking" indicator) is separate: `handle_voice_datagram` emits `VoiceEvent::UserStartedTalking`, and `cleanup_stale_users` (300 ms stale threshold) emits `UserStoppedTalking`; the projection task folds these into `State`.

Optional `AudioDumper` (`audio_dump.rs`) taps raw mic, TX opus, RX opus, RX decoded, and final playback when enabled (debugging).
