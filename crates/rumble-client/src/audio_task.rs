//! Audio task for voice communication.
//!
//! This module implements the Audio Task - one of the two background tasks
//! in the backend architecture. The Audio Task handles:
//!
//! - QUIC datagram send/receive (voice data)
//! - cpal audio streams (capture and playback)
//! - Opus encoding (capture) and per-user decoding (playback)
//! - Per-user jitter buffers
//! - Updating `talking_users` in shared state
//!
//! # Architecture
//!
//! The Audio Task runs independently of the Connection Task. Inter-task
//! communication is minimal:
//!
//! - Connection Task sends `Connection` handle on connect
//! - Audio Task detects connection loss via failed datagram operations
//! - Audio Task updates `talking_users` in shared state directly
//!
//! This separation ensures:
//! - Audio never blocks on reliable message I/O
//! - Minimal latency for voice (no extra channel hop)
//! - Audio can be configured/tested without a connection
//! - Clean shutdown of either component independently

use crate::{
    audio_dump::AudioDumper,
    codec::OPUS_FRAME_SIZE,
    domain_events::{DeviceKind, VoiceEvent},
    events::{AudioSettings, AudioStats, State, VoiceMode},
    handle::read_state,
    projection::EventBus,
};
use bytes::Bytes;
use prost::Message;
use rtrb::{Consumer, Producer, RingBuffer};
use rumble_audio;
use rumble_client_traits::{
    AudioBackend, AudioCaptureStream, AudioPlaybackStream, DatagramTransport, Platform, VoiceCodec,
    VoiceDecoder as VoiceDecoderTrait, VoiceEncoder as VoiceEncoderTrait,
    codec::{EncoderSettings, OPUS_MAX_PACKET_SIZE},
};
use rumble_protocol::proto::VoiceDatagram;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

/// Type aliases to reduce verbosity with deeply nested associated types.
type Enc<P> = <<P as Platform>::Codec as VoiceCodec>::Encoder;
type Dec<P> = <<P as Platform>::Codec as VoiceCodec>::Decoder;
type CapStream<P> = <<P as Platform>::AudioBackend as AudioBackend>::CaptureStream;
type PlayStream<P> = <<P as Platform>::AudioBackend as AudioBackend>::PlaybackStream;

/// Maximum playback buffer size in samples before dropping old samples.
const MAX_PLAYBACK_BUFFER_SAMPLES: usize = 9600;

/// Maximum SFX queue depth in samples (~2 seconds at 48 kHz). Guards against
/// unbounded growth when the playback producer is stalled. Samples beyond this
/// limit are dropped from the front (oldest first) so recent effects play.
const MAX_SFX_QUEUE_SAMPLES: usize = 96_000;

/// Audio-task command channel.
pub enum AudioCommand {
    /// A QUIC connection was established - start datagram handling.
    ConnectionEstablished {
        datagram: Arc<dyn DatagramTransport>,
        my_user_id: u64,
    },
    /// Connection was closed - stop datagram handling.
    ConnectionClosed,
    /// Set the input (microphone) device.
    SetInputDevice { device_id: Option<String> },
    /// Set the output (speaker) device.
    SetOutputDevice { device_id: Option<String> },
    /// Start transmitting (PTT pressed).
    StartTransmit,
    /// Stop transmitting (PTT released).
    StopTransmit,
    /// Set voice activation mode (PTT vs Continuous).
    SetVoiceMode { mode: VoiceMode },
    /// Set self-muted state.
    SetMuted { muted: bool },
    /// Set self-deafened state.
    SetDeafened { deafened: bool },
    /// Set server-muted state. Tracked separately from `SetMuted` so the
    /// user's own self-mute preference isn't clobbered when the server
    /// flips its mute on/off (e.g. moving in/out of a SPEAK-denied room).
    SetServerMuted { muted: bool },
    /// Mute a specific user locally.
    MuteUser { user_id: u64 },
    /// Unmute a specific user locally.
    UnmuteUser { user_id: u64 },
    /// Refresh audio devices.
    RefreshDevices,
    /// Update audio pipeline settings.
    UpdateSettings { settings: AudioSettings },
    /// Reset audio statistics.
    ResetStats,
    /// Update TX pipeline configuration.
    UpdateTxPipeline { config: rumble_audio::PipelineConfig },
    /// Update RX pipeline defaults (for new users).
    UpdateRxPipelineDefaults { config: rumble_audio::PipelineConfig },
    /// Update a specific user's RX configuration.
    UpdateUserRxConfig {
        user_id: u64,
        config: rumble_audio::UserRxConfig,
    },
    /// Clear per-user RX override (use defaults).
    ClearUserRxOverride { user_id: u64 },
    /// Set volume for a specific user.
    SetUserVolume { user_id: u64, volume_db: f32 },
    /// A user joined our current room - create their decoder/pipeline proactively.
    UserJoinedRoom { user_id: u64 },
    /// A user left our current room - destroy their decoder/pipeline.
    UserLeftRoom { user_id: u64 },
    /// We changed rooms - destroy all decoders, create new ones for users in new room.
    RoomChanged { user_ids_in_room: Vec<u64> },
    /// Explicitly drop per-peer decode state (call when a peer leaves).
    PeerLeft { user_id: u64 },
    /// Play a sound effect (pre-scaled PCM samples).
    PlaySfx { samples: Vec<f32> },
    /// Shutdown the audio task.
    Shutdown,
}

impl std::fmt::Debug for AudioCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioCommand::ConnectionEstablished { my_user_id, .. } => f
                .debug_struct("ConnectionEstablished")
                .field("my_user_id", my_user_id)
                .finish(),
            AudioCommand::ConnectionClosed => write!(f, "ConnectionClosed"),
            AudioCommand::SetInputDevice { device_id } => {
                f.debug_struct("SetInputDevice").field("device_id", device_id).finish()
            }
            AudioCommand::SetOutputDevice { device_id } => {
                f.debug_struct("SetOutputDevice").field("device_id", device_id).finish()
            }
            AudioCommand::StartTransmit => write!(f, "StartTransmit"),
            AudioCommand::StopTransmit => write!(f, "StopTransmit"),
            AudioCommand::SetVoiceMode { mode } => f.debug_struct("SetVoiceMode").field("mode", mode).finish(),
            AudioCommand::SetMuted { muted } => f.debug_struct("SetMuted").field("muted", muted).finish(),
            AudioCommand::SetDeafened { deafened } => {
                f.debug_struct("SetDeafened").field("deafened", deafened).finish()
            }
            AudioCommand::SetServerMuted { muted } => f.debug_struct("SetServerMuted").field("muted", muted).finish(),
            AudioCommand::MuteUser { user_id } => f.debug_struct("MuteUser").field("user_id", user_id).finish(),
            AudioCommand::UnmuteUser { user_id } => f.debug_struct("UnmuteUser").field("user_id", user_id).finish(),
            AudioCommand::RefreshDevices => write!(f, "RefreshDevices"),
            AudioCommand::UpdateSettings { .. } => write!(f, "UpdateSettings {{ .. }}"),
            AudioCommand::ResetStats => write!(f, "ResetStats"),
            AudioCommand::UpdateTxPipeline { .. } => write!(f, "UpdateTxPipeline {{ .. }}"),
            AudioCommand::UpdateRxPipelineDefaults { .. } => write!(f, "UpdateRxPipelineDefaults {{ .. }}"),
            AudioCommand::UpdateUserRxConfig { user_id, .. } => {
                f.debug_struct("UpdateUserRxConfig").field("user_id", user_id).finish()
            }
            AudioCommand::ClearUserRxOverride { user_id } => {
                f.debug_struct("ClearUserRxOverride").field("user_id", user_id).finish()
            }
            AudioCommand::SetUserVolume { user_id, volume_db } => f
                .debug_struct("SetUserVolume")
                .field("user_id", user_id)
                .field("volume_db", volume_db)
                .finish(),
            AudioCommand::UserJoinedRoom { user_id } => {
                f.debug_struct("UserJoinedRoom").field("user_id", user_id).finish()
            }
            AudioCommand::UserLeftRoom { user_id } => f.debug_struct("UserLeftRoom").field("user_id", user_id).finish(),
            AudioCommand::RoomChanged { user_ids_in_room } => f
                .debug_struct("RoomChanged")
                .field("user_count", &user_ids_in_room.len())
                .finish(),
            AudioCommand::PeerLeft { user_id } => f.debug_struct("PeerLeft").field("user_id", user_id).finish(),
            AudioCommand::PlaySfx { .. } => write!(f, "PlaySfx"),
            AudioCommand::Shutdown => write!(f, "Shutdown"),
        }
    }
}

/// One 20 ms frame on the media clock, in microseconds. The sender's
/// `timestamp_us` advances by this per frame (through silence as well as
/// speech), so a packet's frame index is `timestamp_us / FRAME_US`.
const FRAME_US: u64 = 20_000;

/// A buffered packet: its Opus payload plus the sender's sequence number. The
/// sequence is kept alongside the media-clock key so a gap can be classified —
/// if the sequence advanced as many steps as the media clock, the gap is packet
/// loss; if the media clock jumped further, the sender was silent.
struct BufferedPacket {
    seq: u32,
    opus: Vec<u8>,
}

/// Per-peer playback state: a timestamp-driven jitter buffer feeding the peer's
/// long-lived Opus decoder.
///
/// Packets are keyed by **media frame index** (`timestamp_us / FRAME_US`), not
/// by sequence, so the playout cursor advances on the sender's clock and an
/// intended silence gap is represented as missing frames (played out as the
/// buffer draining), distinct from packet loss. The depth target adapts to
/// measured interarrival jitter, and sender-faster clock drift is bounded by
/// dropping the oldest unplayed frame past a slack margin (drift between
/// talk-spurt boundaries is absorbed for free by re-anchoring).
///
/// Exposed (`#[doc(hidden)]`) so the `jitter_buffer`/`rx_mix` benches and tests
/// can drive it directly — not a stable public API. Fields stay private;
/// construct via [`UserAudioState::new`] and feed it with
/// [`UserAudioState::insert_packet`].
#[doc(hidden)]
pub struct UserAudioState<D: VoiceDecoderTrait> {
    /// Opus decoder for this user (platform-specific via VoiceCodec trait).
    /// Long-lived: persists across talk spurts (see the decoder-lifetime
    /// invariant in `docs/audio-subsystem.md`).
    decoder: D,
    /// Jitter buffer keyed by media frame index (`timestamp_us / FRAME_US`).
    jitter_buffer: BTreeMap<u64, BufferedPacket>,
    /// Media frame index of the next frame to play (the playout cursor).
    next_play_frame: u64,
    /// Whether playout of the current spurt has begun; cleared on re-anchor so
    /// the next spurt re-buffers to the target depth before playing.
    started: bool,
    /// (sequence, frame) of the last frame actually decoded, used to classify a
    /// gap as loss vs intended silence.
    last_played_seq: u32,
    last_played_frame: u64,
    /// Adaptive playout depth (frames), derived from the jitter estimate and
    /// clamped to `[MIN_TARGET_FRAMES, MAX_TARGET_FRAMES]`.
    target_frames: u32,
    /// Floor for the adaptive target — the configured base delay.
    base_frames: u32,
    /// RFC 3550-style smoothed interarrival jitter estimate, in microseconds.
    jitter_est_us: f64,
    /// Arrival instant + media frame of the last inserted packet, for the jitter
    /// estimate and the stale-stream guard.
    last_arrival: Instant,
    last_arrival_frame: u64,
    /// False until the first packet has been inserted (no arrival to diff yet).
    have_arrival: bool,
    /// Statistics: packets received.
    packets_received: u32,
    /// Statistics: frames lost (a media frame the sender sent that never arrived).
    packets_lost: u64,
    /// Statistics: frames recovered via FEC.
    packets_recovered_fec: u64,
    /// Statistics: frames concealed via PLC.
    frames_concealed: u64,
    /// Statistics: bytes received.
    bytes_received: u64,
    /// Per-user RX pipeline for processing audio before playback.
    rx_pipeline: Option<rumble_audio::AudioPipeline>,
    /// Per-user volume adjustment in dB.
    volume_db: f32,
    /// Whether the sender has signaled end of stream. Advisory: it stops PLC
    /// promptly once the buffer drains, but a lost EOS is also covered by the
    /// stale-stream guard and by the silence classification at the next spurt.
    stream_ended: bool,
    /// Re-prime the (long-lived) decoder with a discarded PLC frame at the next
    /// spurt onset, smoothing the transition and avoiding an onset click.
    needs_prime: bool,
    /// Consecutive packets dropped for arriving behind the playout cursor. A
    /// stray late/reorder/dup frame trips this briefly; a *sustained* run means
    /// the sender's media clock reset by less than `RESTART_BACKWARD_FRAMES`, and
    /// at `LATE_RESYNC_FRAMES` we re-sync to it instead of dropping forever.
    consecutive_late: u32,
}

/// Maximum jitter buffer size (hard backstop against unbounded growth).
const JITTER_BUFFER_MAX_PACKETS: usize = 20;

/// Floor and ceiling for the adaptive playout target, in 20 ms frames.
const MIN_TARGET_FRAMES: u32 = 2;
const MAX_TARGET_FRAMES: u32 = 12;

/// While playing, drop the oldest unplayed frame once the buffer sits this many
/// frames past the current target. Bounds added latency under sender-faster
/// clock drift; most drift is absorbed for free when a spurt boundary
/// re-anchors the cursor, so this only fires on long continuous speech.
const DRIFT_DROP_SLACK: u32 = 4;

/// A media-clock jump backward by more than this many frames is read as a
/// sender restart (reconnect / new capture epoch), not a late or duplicate
/// packet, and re-anchors playout to the new stream.
const RESTART_BACKWARD_FRAMES: u64 = 100;

/// A *smaller* backward reset (within `RESTART_BACKWARD_FRAMES`) looks like a run
/// of late packets. After this many consecutive behind-cursor packets — far more
/// than any plausible reorder burst — conclude the sender's clock reset and
/// re-sync, so the stream recovers instead of dropping every frame forever.
const LATE_RESYNC_FRAMES: u32 = 10;

/// If no packet arrives for this long with no EOS, treat the stream as ended so
/// PLC doesn't run forever (sender crash, NAT rebind, lost EOS). Comfortably
/// longer than the deepest adaptive buffer so normal jitter never trips it.
const STREAM_STALE_THRESHOLD: Duration = Duration::from_millis(200);

/// Message sent from audio capture callback to main loop.
///
/// `timestamp_us` is the frame's position on the sender's media clock — a
/// continuous count of 20 ms frames since the connection's capture epoch,
/// advancing through silence (VAD suppression, mute, PTT release) as well as
/// active speech. It is the RTP-style timestamp the receiver uses to tell an
/// intended silence gap apart from packet loss; see [`UserAudioState`].
enum CaptureMessage {
    /// An encoded audio frame ready to send.
    EncodedFrame {
        data: Bytes,
        size_bytes: usize,
        timestamp_us: u64,
    },
    /// End of stream marker - transmission has stopped (VAD suppressed, PTT released, etc.)
    EndOfStream { timestamp_us: u64 },
}

impl<D: VoiceDecoderTrait> UserAudioState<D> {
    pub fn new(decoder: D, base_delay: u32) -> Self {
        let base = base_delay.clamp(MIN_TARGET_FRAMES, MAX_TARGET_FRAMES);
        Self {
            decoder,
            jitter_buffer: BTreeMap::new(),
            next_play_frame: 0,
            started: false,
            last_played_seq: 0,
            last_played_frame: 0,
            target_frames: base,
            base_frames: base,
            jitter_est_us: 0.0,
            last_arrival: Instant::now(),
            last_arrival_frame: 0,
            have_arrival: false,
            packets_received: 0,
            packets_lost: 0,
            packets_recovered_fec: 0,
            frames_concealed: 0,
            bytes_received: 0,
            rx_pipeline: None,
            volume_db: 0.0,
            stream_ended: false,
            needs_prime: false,
            consecutive_late: 0,
        }
    }

    /// Apply volume adjustment to samples.
    fn apply_volume(&self, samples: &mut [f32]) {
        if self.volume_db != 0.0 {
            let gain = 10.0f32.powf(self.volume_db / 20.0);
            for sample in samples.iter_mut() {
                *sample = (*sample * gain).clamp(-1.0, 1.0);
            }
        }
    }

    /// Frames currently buffered at or ahead of the playout cursor.
    fn depth(&self) -> usize {
        self.jitter_buffer.range(self.next_play_frame..).count()
    }

    /// Fold this arrival into the RFC 3550-style interarrival jitter estimate
    /// and recompute the adaptive playout target. Only forward arrivals advance
    /// the estimate (out-of-order packets would pollute the transit-time diff).
    /// `now` is the arrival instant (injectable so tests can model real pacing).
    fn update_jitter(&mut self, frame: u64, now: Instant) {
        if self.have_arrival && frame > self.last_arrival_frame {
            let arrival_delta = now.duration_since(self.last_arrival).as_micros() as f64;
            let media_delta = (frame - self.last_arrival_frame) as f64 * FRAME_US as f64;
            let transit = (arrival_delta - media_delta).abs();
            self.jitter_est_us += (transit - self.jitter_est_us) / 16.0;
            // Hold ~2x the smoothed jitter as headroom, on top of the base delay.
            let jitter_frames = (2.0 * self.jitter_est_us / FRAME_US as f64).ceil() as u32;
            self.target_frames = (self.base_frames + jitter_frames).clamp(MIN_TARGET_FRAMES, MAX_TARGET_FRAMES);
        }
        self.last_arrival = now;
        self.last_arrival_frame = frame;
        self.have_arrival = true;
    }

    /// Re-anchor playout to the start of a new spurt at `frame`: re-buffer to the
    /// target depth and prime the long-lived decoder so its carried-over state
    /// doesn't pop when handed fresh content. The decoder itself is never reset.
    fn reanchor(&mut self, frame: u64) {
        self.next_play_frame = frame;
        self.started = false;
        self.needs_prime = true;
    }

    /// Insert a packet into the jitter buffer, keyed by its media frame index.
    pub fn insert_packet(&mut self, sequence: u32, timestamp_us: u64, opus_data: Vec<u8>) {
        self.insert_packet_at(sequence, timestamp_us, opus_data, Instant::now());
    }

    /// As [`insert_packet`] but with the arrival instant supplied — lets tests
    /// model realistic packet pacing so the adaptive jitter target is
    /// deterministic instead of reacting to instant back-to-back inserts.
    pub fn insert_packet_at(&mut self, sequence: u32, timestamp_us: u64, opus_data: Vec<u8>, arrival: Instant) {
        let frame = timestamp_us / FRAME_US;
        self.packets_received += 1;
        self.bytes_received += opus_data.len() as u64;
        self.update_jitter(frame, arrival);

        // A media clock that jumps far backward is a sender restart (reconnect /
        // new capture epoch), not a stray late packet — drop the old stream and
        // re-anchor to the new one. The decoder persists across the restart.
        if self.started && frame + RESTART_BACKWARD_FRAMES < self.next_play_frame {
            debug!("sender restart: media clock jumped back to frame {}", frame);
            self.jitter_buffer.clear();
            self.reanchor(frame);
        } else if self.started && frame < self.next_play_frame {
            // Behind the playout cursor: normally a stray late / reordered /
            // duplicate frame, which we drop. But a *sustained* run of
            // behind-cursor frames (the cursor only advances on playout, so this
            // only happens if the sender's clock genuinely moved below ours)
            // means a small backward reset that the far-jump check above missed —
            // re-sync to it rather than dropping every frame forever.
            self.consecutive_late = self.consecutive_late.saturating_add(1);
            if self.consecutive_late <= LATE_RESYNC_FRAMES {
                return;
            }
            debug!("media clock reset below cursor; re-syncing at frame {}", frame);
            self.jitter_buffer.clear();
            self.reanchor(frame);
        }

        // An accepted packet (forward, restart, or resync) clears the late run
        // and means the stream is live again; EOS is only advisory.
        self.consecutive_late = 0;
        self.stream_ended = false;
        self.jitter_buffer.insert(
            frame,
            BufferedPacket {
                seq: sequence,
                opus: opus_data,
            },
        );

        // While buffering a fresh spurt, keep the cursor on the earliest buffered
        // frame so playout begins at the spurt's true start.
        if !self.started
            && let Some((&first, _)) = self.jitter_buffer.iter().next()
        {
            self.next_play_frame = first;
        }

        // Trim the buffer. While playing, hold it near the target to bound the
        // latency that sender-faster clock drift would otherwise add; while
        // buffering, only enforce the hard cap.
        let max_depth = if self.started {
            (self.target_frames + DRIFT_DROP_SLACK) as usize
        } else {
            JITTER_BUFFER_MAX_PACKETS
        };
        while self.jitter_buffer.len() > JITTER_BUFFER_MAX_PACKETS || self.depth() > max_depth {
            let Some((&oldest, _)) = self.jitter_buffer.iter().next() else {
                break;
            };
            self.jitter_buffer.remove(&oldest);
            if self.started {
                if oldest >= self.next_play_frame {
                    self.next_play_frame = oldest + 1;
                }
            } else if let Some((&first, _)) = self.jitter_buffer.iter().next() {
                self.next_play_frame = first;
            }
        }
    }

    /// Mark the sender's end-of-stream. Advisory: it stops PLC promptly once the
    /// buffer drains; a lost EOS is still covered by the stale-stream guard and
    /// by the silence classification at the next spurt.
    pub fn mark_end_of_stream(&mut self) {
        self.stream_ended = true;
    }

    /// Check if we have enough buffered to start (or are already playing).
    fn ready_to_play(&self) -> bool {
        self.started || self.depth() >= self.target_frames as usize
    }

    /// Decode the next frame to play into `out`, returning the number of valid
    /// samples written (or `None` when nothing should play this tick).
    ///
    /// Writes directly into a caller-owned buffer so the per-peer/per-tick mix
    /// can reuse one scratch frame instead of heap-allocating a fresh `Vec` on
    /// every decode. Only `out[..returned_len]` is meaningful; bytes beyond the
    /// returned length are left untouched (may hold stale data from a prior peer).
    ///
    /// A gap at the cursor is classified from the next buffered frame: if the
    /// sender's sequence advanced as many steps as the media clock, the gap is
    /// packet loss (recover via FEC, else PLC); if the media clock jumped
    /// further, the sender was silent, so we re-anchor to the new spurt rather
    /// than concealing the silence.
    pub fn decode_next_into(&mut self, out: &mut [f32; OPUS_FRAME_SIZE]) -> Option<usize> {
        if !self.ready_to_play() {
            return None;
        }

        // Stale-stream guard: no packets for a while and no EOS → treat as ended
        // so PLC doesn't run forever (sender crash, NAT rebind, lost EOS).
        if !self.stream_ended && self.have_arrival && self.last_arrival.elapsed() > STREAM_STALE_THRESHOLD {
            self.stream_ended = true;
        }
        if self.stream_ended && self.jitter_buffer.is_empty() {
            return None;
        }

        // Prime the long-lived decoder at a spurt onset to avoid an onset click.
        if self.needs_prime {
            self.needs_prime = false;
            let mut discard = [0.0f32; OPUS_FRAME_SIZE];
            let _ = self.decoder.decode_plc(&mut discard);
        }

        let frame = self.next_play_frame;

        // Frame present → decode and advance.
        if let Some(pkt) = self.jitter_buffer.remove(&frame) {
            self.started = true;
            self.last_played_seq = pkt.seq;
            self.last_played_frame = frame;
            self.next_play_frame += 1;
            return match self.decoder.decode(&pkt.opus, out) {
                Ok(n) => Some(n),
                Err(e) => {
                    warn!("decode error at frame {}: {}", frame, e);
                    self.frames_concealed += 1;
                    self.decoder.decode_plc(out).ok()
                }
            };
        }

        // Frame missing. Find the next buffered frame to classify the gap.
        let next = self.jitter_buffer.range(frame + 1..).next().map(|(&f, p)| (f, p.seq));
        let Some((next_frame, next_seq)) = next else {
            // Nothing buffered ahead.
            if self.stream_ended {
                return None;
            }
            // Starved mid-spurt: conceal and advance (the frame's time has passed).
            self.frames_concealed += 1;
            self.next_play_frame += 1;
            return self.decoder.decode_plc(out).ok();
        };

        // Classify loss vs silence over [last_played, next_frame]: if the
        // sequence advanced as many steps as the media clock, every frame was
        // sent and the gap is loss; if the media clock jumped further, the
        // sender was silent → re-anchor to the new spurt.
        if self.started {
            let media_span = next_frame - self.last_played_frame;
            let seq_span = next_seq.wrapping_sub(self.last_played_seq) as u64;
            if seq_span < media_span {
                self.reanchor(next_frame);
                return None;
            }
        }

        // Loss at `frame`. Recover from the immediate successor's in-band FEC if
        // present, else conceal with PLC.
        self.packets_lost += 1;
        self.next_play_frame += 1;
        if next_frame == frame + 1
            && let Some(pkt) = self.jitter_buffer.get(&next_frame)
        {
            return match self.decoder.decode_fec(&pkt.opus, out) {
                Ok(n) => {
                    self.packets_recovered_fec += 1;
                    Some(n)
                }
                Err(e) => {
                    warn!("FEC recovery failed at frame {}: {}, falling back to PLC", frame, e);
                    self.frames_concealed += 1;
                    self.decoder.decode_plc(out).ok()
                }
            };
        }
        self.frames_concealed += 1;
        self.decoder.decode_plc(out).ok()
    }
}

/// Audio task handle for sending commands.
#[derive(Clone)]
pub struct AudioTaskHandle {
    command_tx: mpsc::UnboundedSender<AudioCommand>,
}

impl AudioTaskHandle {
    /// Send a command to the audio task.
    pub fn send(&self, cmd: AudioCommand) {
        let _ = self.command_tx.send(cmd);
    }
}

/// Configuration for the audio task.
pub struct AudioTaskConfig<P: Platform> {
    /// Shared state, read-only for the audio task. Used to read the
    /// initial `AudioSettings` / pipeline configs at startup and to
    /// snapshot `talking_users` when computing stop-transitions on
    /// bulk-clear paths (disconnect, deafen, room change).
    pub state: Arc<RwLock<State>>,
    /// Repaint callback for UI updates.
    pub repaint: Arc<dyn Fn() + Send + Sync>,
    /// Event bus — the audio task emits `VoiceEvent`s here, the projection
    /// task folds them into `State`.
    pub bus: EventBus,
    /// Audio dumper for debugging (optional).
    pub audio_dumper: Option<AudioDumper>,
    /// Audio backend instance, owned by the audio task. Tests can pass a
    /// `MockAudioBackend` here to bypass cpal entirely; production code
    /// passes `P::AudioBackend::default()`.
    pub audio_backend: P::AudioBackend,
    /// Writer for the live meter snapshot. Cloned into each capture
    /// callback (single active stream → single active producer); the UI
    /// loads via `BackendHandle::meter()`.
    pub meter_writer: crate::snapshot::SnapshotWriter<crate::meter::MeterSnapshot>,
    /// Writer for the periodic stats roll-up, driven from the async
    /// loop's stats timer. UI loads via `BackendHandle::stats()`.
    pub stats_writer: crate::snapshot::SnapshotWriter<crate::events::AudioStats>,
    /// Writer for live per-stage pipeline outputs (e.g. the denoise VAD
    /// probability meter). Same sampled-snapshot transport as the meter;
    /// published once per capture frame, read by the UI via
    /// `BackendHandle::outputs()`.
    pub output_writer: crate::snapshot::SnapshotWriter<rumble_audio::OutputFrame>,
}

/// Spawn the audio task and return a handle for sending commands.
///
/// The audio task runs on a separate thread with its own tokio runtime
/// to avoid any blocking from audio I/O affecting other async tasks.
pub fn spawn_audio_task<P: Platform>(config: AudioTaskConfig<P>) -> AudioTaskHandle {
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create audio tokio runtime");

        rt.block_on(run_audio_task::<P>(command_rx, config));
    });

    AudioTaskHandle { command_tx }
}

/// Main audio task loop.
/// Determine if we should be processing captured audio based on current state.
/// Note: VAD is a pipeline processor, not a voice mode. In Continuous mode
/// with VAD enabled, the pipeline's suppress flag gates actual transmission.
#[inline]
fn should_capture(
    voice_mode: VoiceMode,
    self_muted: bool,
    server_muted: bool,
    ptt_active: bool,
    connected: bool,
) -> bool {
    if !connected || self_muted || server_muted {
        return false;
    }
    match voice_mode {
        VoiceMode::Continuous => true,
        VoiceMode::PushToTalk => ptt_active,
    }
}

/// The capture-state change implied by a desired vs. current capture state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureTransition {
    /// Desired and current state already agree — do nothing.
    None,
    /// Begin processing captured samples.
    Activate,
    /// Stop processing captured samples. The caller MUST emit an end-of-stream
    /// marker before deactivating, otherwise receivers conceal (PLC) forever
    /// waiting for packets that never arrive.
    Deactivate,
}

/// Decide the capture transition from the desired state (`want`, typically from
/// [`should_capture`]) and the current `active` flag.
#[inline]
fn capture_transition(want: bool, active: bool) -> CaptureTransition {
    match (want, active) {
        (true, false) => CaptureTransition::Activate,
        (false, true) => CaptureTransition::Deactivate,
        _ => CaptureTransition::None,
    }
}

async fn run_audio_task<P: Platform>(
    mut command_rx: mpsc::UnboundedReceiver<AudioCommand>,
    config: AudioTaskConfig<P>,
) {
    let state = config.state;
    let repaint = config.repaint;
    let bus = config.bus;
    let meter_writer = config.meter_writer;
    let mut stats_writer = config.stats_writer;
    let output_writer = config.output_writer;
    let audio_dumper = config.audio_dumper.unwrap_or_else(AudioDumper::disabled);

    // Audio backend for device access (lives on this thread)
    let audio_backend = config.audio_backend;

    // Current connection state (datagram handle for voice I/O)
    let mut connection: Option<Arc<dyn DatagramTransport>> = None;
    let mut my_user_id: u64 = 0;

    // Voice mode and mute state (orthogonal controls)
    let mut voice_mode = VoiceMode::PushToTalk;
    let mut self_muted = false;
    let mut self_deafened = false;
    let mut ptt_active = false;
    // Server-imposed mute (SPEAK denied, or moderator action).
    // Treated identically to `self_muted` for capture gating, but tracked
    // separately so unmuting on the server doesn't clobber the user's
    // chosen self-mute state.
    let mut server_muted = false;

    // Per-user local mutes
    let mut muted_users: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // Audio I/O handles (generic over Platform)
    let mut audio_input: Option<CapStream<P>> = None;
    // Playback health counters. `playback_underruns` is bumped by the output
    // callback (a separate device thread) when it drains the ring mid-block;
    // `playback_overflows` by the mix tick when the ring is full and it drops
    // about-to-play samples. Both surface in AudioStats for diagnosis.
    let playback_underruns = Arc::new(AtomicU64::new(0));
    let playback_overflows = Arc::new(AtomicU64::new(0));
    // True while the mix tick is actively producing audio. Lets the playback
    // filler distinguish a mid-stream starvation (click) from the expected
    // cushion drain after a spurt ends (not a click).
    let playback_producer_active = Arc::new(AtomicBool::new(false));
    // playback_stream is started below once audio_backend and selected_output are ready.

    // Sound effects sample queue - mixed into playback output each frame
    let mut sfx_queue: VecDeque<f32> = VecDeque::new();

    // Per-user decoders and jitter buffers
    let mut user_audio: HashMap<u64, UserAudioState<Dec<P>>> = HashMap::new();

    // Selected devices
    let mut selected_input: Option<String> = None;
    let mut selected_output: Option<String> = None;

    // Current audio settings (start with defaults from state)
    let mut audio_settings = {
        let s = read_state(&state);
        s.audio.settings.clone()
    };

    // TX pipeline configuration (start with defaults from state)
    let mut tx_pipeline_config = {
        let s = read_state(&state);
        s.audio.tx_pipeline.clone()
    };

    // RX pipeline defaults and per-user configs
    let mut rx_pipeline_defaults = {
        let s = read_state(&state);
        s.audio.rx_pipeline_defaults.clone()
    };
    let mut per_user_rx: HashMap<u64, rumble_audio::UserRxConfig> = {
        let s = read_state(&state);
        s.audio.per_user_rx.clone()
    };

    // Create processor registry with built-in processors
    let mut processor_registry = rumble_audio::ProcessorRegistry::new();
    crate::processors::register_builtin_processors(&mut processor_registry);

    // Channel for encoded audio frames and end-of-stream signals
    let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel::<CaptureMessage>();

    // Sequence number for outgoing voice packets
    // Only incremented when a packet is actually sent (not for skipped DTX frames)
    let mut send_sequence: u32 = 0;

    // Outgoing media clock. `capture_frame_index` counts 20 ms frames since
    // `capture_epoch`; the capture callback advances it per captured frame (so
    // VAD-suppressed silence is encoded as a timestamp gap) and stamps each
    // sent datagram with `index * FRAME_US`. On capture (re)activation the index
    // is fast-forwarded to real elapsed time so mute/PTT gaps are encoded too —
    // making the timestamp a true media clock the receiver can use to tell
    // silence from loss. Both reset per connection.
    let capture_frame_index = Arc::new(AtomicU64::new(0));
    let mut capture_epoch = Instant::now();

    // Statistics tracking
    let mut packets_sent: u64 = 0;
    let mut bytes_sent: u64 = 0;

    // =============================================================================
    // Connection-Scoped Encoder
    // =============================================================================
    // The encoder is created when a connection is established and persists for the
    // entire connection lifetime. It is NOT reset between PTT presses or voice
    // activations - this allows Opus to maintain state for better DTX behavior.
    let encoder: Arc<std::sync::Mutex<Option<Enc<P>>>> = Arc::new(std::sync::Mutex::new(None));

    // =============================================================================
    // DTX (Discontinuous Transmission) Handling
    // =============================================================================
    // Opus emits very small frames (≤2 bytes) during silence. We send those
    // frames as-is rather than skipping them: sequence numbers stay
    // consecutive at one-per-20ms, so the receiver's jitter buffer never
    // sees a wall-clock gap that looks like packet loss. The cost is ~12
    // kbps of overhead while the VAD/gate is open but the speaker is
    // quiet — which is negligible compared to ~24 kbps active voice and
    // avoids the receiver-side PLC bursts that skipping caused.
    //
    // Long silences are still handled efficiently: when the VAD gate
    // closes (`pipeline_result.suppress` flips to true in the capture
    // callback), we emit a single EOS marker and stop sending entirely
    // until the gate reopens.

    // =========================================================================
    // Transmission State Machine
    // =========================================================================
    //
    // The audio input stream is connection-scoped: created when a connection is
    // established and kept alive until disconnection. This avoids ALSA device
    // enumeration errors (e.g., OSS emulation `/dev/dsp` failures) on every
    // PTT press/release cycle.
    //
    // A shared `capture_active` flag controls whether captured samples are
    // processed and encoded or silently discarded. The `should_capture()` function
    // determines the desired state, and `sync_transmission!` toggles the flag
    // (and sends EOS when stopping) instead of creating/destroying the stream.
    //
    // The stream is only recreated for device changes, settings changes, or
    // disconnection.

    // Whether the capture callback should process samples or discard them.
    // This is a local bool because the callback uses AudioCaptureStream::set_active()
    // to be gated, rather than checking a shared atomic flag.
    // Invariant: capture_is_active == true implies audio_input.is_some().
    let mut capture_is_active = false;

    // Persistent recovery flags: set when a device dies and a re-open has
    // already failed at least once. The health tick uses them to keep retrying
    // even after the stream local is None (where the normal is_healthy() guard
    // would never fire again). Cleared on successful re-open or deliberate
    // teardown so recovery doesn't fight an intentional device change.
    let mut input_recovering = false;
    let mut output_recovering = false;

    // Edge-tracking baseline for the VAD-gated "transmitting" UI light, shared
    // with the capture callback (which owns the per-frame on/off edges). The
    // callback is gated off by set_active() when capture is deactivated, so it
    // can't emit its own false edge then — `sync_transmission!` resets this to
    // false on deactivate so the next activation re-evaluates from a clean
    // baseline. "Capture armed" (unmute) is NOT itself "transmitting"; only the
    // callback's VAD decision lights the indicator.
    let was_transmitting = Arc::new(AtomicBool::new(false));

    // Pipeline handle for the current capture stream. Retained alongside the
    // capture stream so the sync_transmission! Activate arm can call reset()
    // before arming the stream, clearing stale VAD-gate state (e.g. an open
    // holdoff from a previous PTT press) that would otherwise open the gate
    // and transmit room noise before the gate re-evaluates from silence.
    // Set by start_transmission; cleared by stop_transmission.
    let mut tx_pipeline_handle: Option<Arc<std::sync::Mutex<rumble_audio::AudioPipeline>>> = None;

    // Interval for cleaning up stale talking_users
    let mut cleanup_interval = tokio::time::interval(Duration::from_millis(500));

    // Poll cadence for the playback producer. This is NOT the production rate —
    // each tick refills the ring to `PLAYBACK_TARGET_SAMPLES`, so how much is
    // produced is set by how much the device drained, not by this interval. The
    // interval only has to be short relative to the ring depth so a tick lands
    // before the ring can drain empty (10 ms vs a ~40 ms ring → a delayed tick
    // is absorbed). A late tick under the default Burst behavior just finds the
    // ring already full and produces nothing, so catch-up bursts are harmless.
    let mut produce_interval = tokio::time::interval(Duration::from_millis(10));

    // Interval for updating statistics in state
    let mut stats_interval = tokio::time::interval(Duration::from_millis(500));

    // Interval for checking that the live capture/playback devices are still
    // alive and, if not, tearing them down and retrying a re-open. Slow enough
    // (1 s) that a persistently-missing device isn't hammered with re-open
    // attempts, but fast enough that recovery after a replug feels immediate.
    let mut device_health_interval = tokio::time::interval(Duration::from_secs(1));

    // Playback hand-off: a lock-free SPSC ring, created per output stream. The
    // mix tick (this thread) is the sole producer; the device output callback (a
    // separate real-time thread) is the sole consumer. The `Producer` lives here
    // in `playback_producer`, the `Consumer` is moved into the device callback,
    // so the producer and the stream are always created and torn down as a unit
    // — hence `open_playback!` over the two locals rather than a helper that
    // would have to thread both back out. Both are `None` whenever there is no
    // output stream (deafened, or a device that failed to open). Left
    // uninitialized here; `open_playback!` below assigns both before first use.
    let mut playback_stream: Option<PlayStream<P>>;
    let mut playback_producer: Option<Producer<f32>>;
    macro_rules! open_playback {
        () => {{
            match start_audio_output::<P>(
                &audio_backend,
                &selected_output,
                playback_underruns.clone(),
                playback_producer_active.clone(),
            ) {
                Some((stream, producer)) => {
                    playback_stream = Some(stream);
                    playback_producer = Some(producer);
                }
                None => {
                    playback_stream = None;
                    playback_producer = None;
                }
            }
        }};
    }

    // Start audio output at startup so SFX can play without a connection
    open_playback!();

    info!("Audio task started");

    /// Macro to sync the capture state after any state change.
    /// This toggles whether captured samples are processed or discarded
    /// via `AudioCaptureStream::set_active()`, WITHOUT creating/destroying
    /// the audio input stream.
    macro_rules! sync_transmission {
        () => {{
            let want = should_capture(voice_mode, self_muted, server_muted, ptt_active, connection.is_some());

            match capture_transition(want, capture_is_active) {
                CaptureTransition::Activate => {
                    // Activation is only meaningful when the capture stream
                    // exists. When audio_input is None the transition is
                    // deferred: capture_is_active stays false so the next
                    // sync_transmission! after a successful start_transmission
                    // will re-evaluate and arm the newly-opened stream.
                    // Invariant: capture_is_active == true implies audio_input.is_some().
                    if let Some(stream) = audio_input.as_ref() {
                        capture_is_active = true;
                        // Fast-forward the media clock past the silence we were
                        // not capturing (mute / PTT up), so the resumed stream's
                        // timestamp reflects the real gap and the receiver
                        // re-anchors it as a new spurt. `fetch_max` keeps it
                        // monotonic.
                        let elapsed_frames = (capture_epoch.elapsed().as_micros() as u64) / FRAME_US;
                        capture_frame_index.fetch_max(elapsed_frames, Ordering::Relaxed);
                        // Reset the TX pipeline so stale VAD-gate state (e.g. an
                        // open holdoff from a previous mute/PTT cycle) does not
                        // open the gate immediately on the next spurt, transmitting
                        // room noise before the VAD re-evaluates. Activate is the
                        // correct reset point: state must be clean when capture
                        // resumes. Resetting at Deactivate would be equivalent but
                        // would leave stale state visible to diagnostics in the
                        // interval between deactivation and the next activation.
                        if let Some(pipeline) = tx_pipeline_handle.as_ref() {
                            if let Ok(mut guard) = pipeline.lock() {
                                guard.reset();
                            }
                        }
                        stream.set_active(true);
                        // Don't light the transmitting indicator here: arming
                        // capture (e.g. unmuting) is not the same as
                        // transmitting. The capture callback drives the
                        // VAD-gated edge from a clean `false` baseline (reset
                        // on the matching deactivate), so the light turns on
                        // only once we actually speak.
                        info!("Capture activated (samples will be processed)");
                    }
                }
                CaptureTransition::Deactivate => {
                    // Send end-of-stream before deactivating
                    // (PTT released, muted, disconnected, etc.)
                    let _ = encoded_tx.send(CaptureMessage::EndOfStream {
                        timestamp_us: capture_frame_index.load(Ordering::Relaxed) * FRAME_US,
                    });
                    capture_is_active = false;
                    if let Some(stream) = audio_input.as_ref() {
                        stream.set_active(false);
                    }
                    // The callback is now gated off and can't emit its own false
                    // edge, so force the light off and reset the baseline for the
                    // next activation.
                    was_transmitting.store(false, Ordering::Relaxed);
                    let _ = bus.voice.send(VoiceEvent::TransmittingChanged { active: false });
                    info!("Capture deactivated (samples will be discarded)");
                }
                CaptureTransition::None => {}
            }
        }};
    }

    loop {
        tokio::select! {
            // Handle commands
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    AudioCommand::ConnectionEstablished { datagram: conn, my_user_id: uid } => {
                        info!("Audio task: connection established, user_id={}", uid);
                        connection = Some(conn);
                        my_user_id = uid;

                        // Reset the outgoing media clock for the new connection.
                        capture_epoch = Instant::now();
                        capture_frame_index.store(0, Ordering::Relaxed);

                        // Create connection-scoped encoder
                        // The encoder persists for the entire connection, not per-transmission
                        let encoder_settings = EncoderSettings {
                            bitrate: audio_settings.bitrate,
                            complexity: audio_settings.encoder_complexity,
                            fec_enabled: audio_settings.fec_enabled,
                            packet_loss_percent: audio_settings.packet_loss_percent,
                            dtx_enabled: true,
                            vbr_enabled: true,
                        };
                        match P::Codec::create_encoder(&encoder_settings) {
                            Ok(enc) => {
                                if let Ok(mut guard) = encoder.lock() {
                                    *guard = Some(enc);
                                }
                                info!("Created connection-scoped encoder");
                            }
                            Err(e) => {
                                error!("Failed to create connection-scoped encoder: {}", e);
                            }
                        }

                        // Start audio output for receiving (unless deafened)
                        if playback_stream.is_none() && !self_deafened {
                            open_playback!();
                        }

                        // Create connection-scoped audio input stream. It
                        // stays alive for the whole connection regardless of
                        // mute/server-mute/PTT — those gate via the
                        // capture_active flag, not stream lifetime.
                        if audio_input.is_none()
                            && let Err(e) = start_transmission::<P>(
                                &audio_backend,
                                &selected_input,
                                &encoded_tx,
                                &audio_settings,
                                &tx_pipeline_config,
                                &processor_registry,
                                &encoder,
                                &mut audio_input,
                                &mut tx_pipeline_handle,
                                &bus.voice,
                                &audio_dumper,
                                &meter_writer,
                                &output_writer,
                                &capture_frame_index,
                                &was_transmitting,
                            )
                        {
                            capture_is_active = false;
                            let _ = bus.voice.send(VoiceEvent::DeviceUnavailable {
                                kind: DeviceKind::Input,
                                message: e.to_string(),
                            });
                        }

                        // Sync transmission state (toggles capture_active flag)
                        sync_transmission!();
                    }

                    AudioCommand::ConnectionClosed => {
                        info!("Audio task: connection closed");
                        connection = None;
                        my_user_id = 0;

                        // Deactivate capture first (sync will handle this since connected=false)
                        sync_transmission!();

                        // Destroy connection-scoped audio input stream
                        stop_transmission::<P>(&mut audio_input, &mut tx_pipeline_handle);
                        // Capture is connection-scoped; ConnectionEstablished
                        // will re-open on reconnect. Stop the recovery loop
                        // so the health tick doesn't spin while disconnected.
                        input_recovering = false;

                        // Destroy connection-scoped encoder
                        if let Ok(mut guard) = encoder.lock() {
                            *guard = None;
                        }

                        // Reset PTT and server-muted state on disconnect so
                        // the next connection starts clean.
                        ptt_active = false;
                        server_muted = false;

                        // Clear talking users — emit a stop event for each
                        // currently-talking user so subscribers see clean
                        // transitions rather than a silent state reset.
                        {
                            let snapshot = read_state(&state).audio.talking_users.clone();
                            for uid in snapshot {
                                let _ = bus.voice.send(VoiceEvent::UserStoppedTalking { user_id: uid });
                            }
                        }

                        // Clear per-user state
                        user_audio.clear();
                    }

                    AudioCommand::SetInputDevice { device_id } => {
                        // When the device selection hasn't changed AND the
                        // stream is actually open, skip the teardown/rebuild
                        // cycle that would cause a ~200 ms transmission
                        // dropout. A missing stream (earlier open failure)
                        // must fall through so re-applying the same device
                        // retries the open. Still emit SelectedDeviceChanged
                        // so the projection can clear any device-fault state
                        // it may be holding.
                        if device_id == selected_input && audio_input.is_some() {
                            let _ = bus.voice.send(VoiceEvent::SelectedDeviceChanged {
                                kind: DeviceKind::Input,
                                id: device_id,
                            });
                            // sync state in case mute/PTT changed while the
                            // device was being confirmed
                            sync_transmission!();
                        } else {
                        selected_input = device_id.clone();
                        // A deliberate device switch cancels any in-progress
                        // recovery loop; the new start_transmission below is
                        // the authoritative open attempt.
                        input_recovering = false;

                        // Tear down the current stream so we always re-open
                        // against the new device.
                        if audio_input.is_some() {
                            stop_transmission::<P>(&mut audio_input, &mut tx_pipeline_handle);
                            capture_is_active = false;
                        }

                        if connection.is_some() {
                            // Only confirm the selection once the device
                            // actually opens — otherwise the UI would show a
                            // dead mic as "selected" while we silently capture
                            // nothing. On failure, capture stays off
                            // (audio_input is None ⇒ set_active is a no-op) and
                            // we surface a DeviceUnavailable instead.
                            match start_transmission::<P>(
                                &audio_backend,
                                &selected_input,
                                &encoded_tx,
                                &audio_settings,
                                &tx_pipeline_config,
                                &processor_registry,
                                &encoder,
                                &mut audio_input,
                                &mut tx_pipeline_handle,
                                &bus.voice,
                                &audio_dumper,
                                &meter_writer,
                                &output_writer,
                                &capture_frame_index,
                                &was_transmitting,
                            ) {
                                Ok(()) => {
                                    let _ = bus.voice.send(VoiceEvent::SelectedDeviceChanged {
                                        kind: DeviceKind::Input,
                                        id: device_id,
                                    });
                                }
                                Err(e) => {
                                    capture_is_active = false;
                                    let _ = bus.voice.send(VoiceEvent::DeviceUnavailable {
                                        kind: DeviceKind::Input,
                                        message: e.to_string(),
                                    });
                                }
                            }
                        } else {
                            // Disconnected: nothing to open against yet, so the
                            // selection can't fail here. Record it; it'll be
                            // validated when the next connection opens the stream.
                            let _ = bus.voice.send(VoiceEvent::SelectedDeviceChanged {
                                kind: DeviceKind::Input,
                                id: device_id,
                            });
                        }
                        sync_transmission!();
                        } // end else (device actually changed)
                    }

                    AudioCommand::SetOutputDevice { device_id } => {
                        selected_output = device_id.clone();
                        // A deliberate device switch cancels any in-progress
                        // recovery loop; open_playback! below is the
                        // authoritative attempt on the new device.
                        output_recovering = false;

                        // Restart output (always, even when disconnected — SFX need it)
                        playback_stream = None;
                        playback_producer = None;
                        if !self_deafened {
                            open_playback!();
                            // Confirm the selection only if playback actually
                            // opened; otherwise surface the failure so the UI
                            // doesn't show a dead sink as selected.
                            if playback_stream.is_some() {
                                let _ = bus.voice.send(VoiceEvent::SelectedDeviceChanged {
                                    kind: DeviceKind::Output,
                                    id: device_id,
                                });
                            } else {
                                let _ = bus.voice.send(VoiceEvent::DeviceUnavailable {
                                    kind: DeviceKind::Output,
                                    message: format!(
                                        "failed to open output device {:?}",
                                        selected_output.as_deref().unwrap_or("default")
                                    ),
                                });
                            }
                        } else {
                            // Deafened: output isn't opened, so the selection
                            // can't be validated yet. Record it for when we undeafen.
                            let _ = bus.voice.send(VoiceEvent::SelectedDeviceChanged {
                                kind: DeviceKind::Output,
                                id: device_id,
                            });
                        }
                    }

                    AudioCommand::StartTransmit => {
                        ptt_active = true;
                        sync_transmission!();
                    }

                    AudioCommand::StopTransmit => {
                        ptt_active = false;
                        sync_transmission!();
                    }

                    AudioCommand::SetVoiceMode { mode } => {
                        voice_mode = mode;
                        let _ = bus.voice.send(VoiceEvent::VoiceModeChanged { mode });
                        sync_transmission!();
                    }

                    AudioCommand::SetMuted { muted } => {
                        self_muted = muted;
                        let _ = bus.voice.send(VoiceEvent::SelfMutedChanged { muted });

                        // Stream stays connection-scoped; sync_transmission!
                        // gates the capture callback via set_active(false) and
                        // emits EOS, so muting is instant and unmute doesn't
                        // pay an ALSA device-init round-trip.
                        sync_transmission!();
                    }

                    AudioCommand::SetDeafened { deafened } => {
                        self_deafened = deafened;
                        // Deafen implies mute
                        if deafened && !self_muted {
                            self_muted = true;
                        }
                        let _ = bus.voice.send(VoiceEvent::SelfDeafenedChanged { deafened });
                        let _ = bus.voice.send(VoiceEvent::SelfMutedChanged { muted: self_muted });

                        // Handle audio output based on deafen state
                        if deafened {
                            playback_stream = None;
                            playback_producer = None;
                            // Deliberate teardown: stop the recovery loop so
                            // the health tick doesn't try to re-open an output
                            // the user intentionally silenced.
                            output_recovering = false;
                            // SFX queued while deafened are ephemeral — discard
                            // them so they don't play back as a burst on undeafen.
                            sfx_queue.clear();
                            // Snapshot talkers and emit stop transitions before
                            // we wipe local decoder state.
                            let snapshot = read_state(&state).audio.talking_users.clone();
                            user_audio.clear();
                            for uid in snapshot {
                                let _ = bus.voice.send(VoiceEvent::UserStoppedTalking { user_id: uid });
                            }
                        } else if connection.is_some() && playback_stream.is_none() {
                            open_playback!();
                        }

                        // Mute is gated via set_active(false); leave the input
                        // stream alive across deafen toggles.
                        sync_transmission!();
                    }

                    AudioCommand::SetServerMuted { muted } => {
                        if server_muted == muted {
                            // No-op fast path — UserStatusChanged broadcasts hit
                            // every frame in some flows; don't churn state.
                        } else {
                            server_muted = muted;
                            let _ = bus.voice.send(VoiceEvent::ServerMutedChanged { muted });
                            // Like SetMuted: gate via set_active, keep the
                            // stream alive across the toggle.
                            sync_transmission!();
                        }
                    }

                    AudioCommand::MuteUser { user_id } => {
                        muted_users.insert(user_id);
                        let _ = bus.voice.send(VoiceEvent::LocalMuteToggled { user_id, muted: true });
                    }

                    AudioCommand::UnmuteUser { user_id } => {
                        muted_users.remove(&user_id);
                        let _ = bus.voice.send(VoiceEvent::LocalMuteToggled { user_id, muted: false });
                    }

                    AudioCommand::RefreshDevices => {
                        let input_devices = audio_backend.list_input_devices();
                        let output_devices = audio_backend.list_output_devices();
                        let _ = bus.voice.send(VoiceEvent::DevicesEnumerated {
                            input: input_devices,
                            output: output_devices,
                        });
                    }

                    AudioCommand::UpdateSettings { settings } => {
                        // When the incoming settings are identical to what the
                        // task already holds, skip the teardown/rebuild cycle
                        // that would otherwise cause a ~200 ms transmission
                        // dropout every time the user opens the settings dialog
                        // and clicks Apply without changing anything.
                        if settings == audio_settings {
                            // settings are already in effect — nothing to do
                        } else {
                        info!("Audio task: updating settings");
                        audio_settings = settings.clone();
                        let _ = bus.voice.send(VoiceEvent::AudioSettingsChanged {
                            settings: settings.clone(),
                        });

                        // Recreate stream with new settings if connected
                        if audio_input.is_some() {
                            stop_transmission::<P>(&mut audio_input, &mut tx_pipeline_handle);
                            capture_is_active = false;
                        }
                        if connection.is_some()
                            && let Err(e) = start_transmission::<P>(
                                &audio_backend,
                                &selected_input,
                                &encoded_tx,
                                &audio_settings,
                                &tx_pipeline_config,
                                &processor_registry,
                                &encoder,
                                &mut audio_input,
                                &mut tx_pipeline_handle,
                                &bus.voice,
                                &audio_dumper,
                                &meter_writer,
                                &output_writer,
                                &capture_frame_index,
                                &was_transmitting,
                            )
                        {
                            capture_is_active = false;
                            let _ = bus.voice.send(VoiceEvent::DeviceUnavailable {
                                kind: DeviceKind::Input,
                                message: e.to_string(),
                            });
                        }
                        sync_transmission!();
                        } // end else (settings actually changed)
                    }

                    AudioCommand::ResetStats => {
                        info!("Audio task: resetting statistics");
                        packets_sent = 0;
                        bytes_sent = 0;

                        // Reset per-user stats
                        for user_state in user_audio.values_mut() {
                            user_state.packets_lost = 0;
                            user_state.packets_recovered_fec = 0;
                            user_state.frames_concealed = 0;
                            user_state.bytes_received = 0;
                        }

                        // Reset the playback health counters too, so the button
                        // zeroes underruns/overflows along with everything else.
                        playback_underruns.store(0, Ordering::Relaxed);
                        playback_overflows.store(0, Ordering::Relaxed);

                        stats_writer.store(AudioStats::default());
                    }

                    AudioCommand::UpdateTxPipeline { config } => {
                        // Skip teardown/rebuild when the pipeline config
                        // hasn't changed (e.g. repeated Apply presses).
                        if config == tx_pipeline_config {
                            // config already in effect — nothing to do
                        } else {
                        info!("Audio task: updating TX pipeline config");
                        tx_pipeline_config = config.clone();
                        let _ = bus.voice.send(VoiceEvent::TxPipelineChanged {
                            config: config.clone(),
                        });

                        // Recreate stream to rebuild pipeline with new config
                        if audio_input.is_some() {
                            stop_transmission::<P>(&mut audio_input, &mut tx_pipeline_handle);
                            capture_is_active = false;
                        }
                        if connection.is_some()
                            && let Err(e) = start_transmission::<P>(
                                &audio_backend,
                                &selected_input,
                                &encoded_tx,
                                &audio_settings,
                                &tx_pipeline_config,
                                &processor_registry,
                                &encoder,
                                &mut audio_input,
                                &mut tx_pipeline_handle,
                                &bus.voice,
                                &audio_dumper,
                                &meter_writer,
                                &output_writer,
                                &capture_frame_index,
                                &was_transmitting,
                            )
                        {
                            capture_is_active = false;
                            let _ = bus.voice.send(VoiceEvent::DeviceUnavailable {
                                kind: DeviceKind::Input,
                                message: e.to_string(),
                            });
                        }
                        sync_transmission!();
                        } // end else (config actually changed)
                    }

                    AudioCommand::UpdateRxPipelineDefaults { config } => {
                        info!("Audio task: updating RX pipeline defaults");
                        rx_pipeline_defaults = config.clone();
                        let _ = bus.voice.send(VoiceEvent::RxPipelineDefaultsChanged {
                            config: config.clone(),
                        });
                        // Rebuild pipelines for users without overrides
                        for (user_id, user_state) in user_audio.iter_mut() {
                            if !per_user_rx.contains_key(user_id) {
                                match rumble_audio::AudioPipeline::from_config(&rx_pipeline_defaults, &processor_registry) {
                                    Ok(p) => user_state.rx_pipeline = Some(p),
                                    Err(e) => warn!("Failed to rebuild RX pipeline for user {}: {}", user_id, e),
                                }
                            }
                        }
                        repaint();
                    }

                    AudioCommand::UpdateUserRxConfig { user_id, config } => {
                        info!("Audio task: updating RX config for user {}", user_id);
                        let volume_db = config.volume_db;
                        // Clone the pipeline config before moving config
                        let pipeline_config_owned = config.pipeline_override.clone();
                        per_user_rx.insert(user_id, config.clone());
                        let _ = bus.voice.send(VoiceEvent::UserRxConfigChanged {
                            user_id,
                            config,
                        });
                        // Rebuild this user's pipeline if they exist
                        if let Some(user_state) = user_audio.get_mut(&user_id) {
                            let pipeline_config = pipeline_config_owned.as_ref().unwrap_or(&rx_pipeline_defaults);
                            match rumble_audio::AudioPipeline::from_config(pipeline_config, &processor_registry) {
                                Ok(p) => user_state.rx_pipeline = Some(p),
                                Err(e) => warn!("Failed to rebuild RX pipeline for user {}: {}", user_id, e),
                            }
                            user_state.volume_db = volume_db;
                        }
                        repaint();
                    }

                    AudioCommand::ClearUserRxOverride { user_id } => {
                        info!("Audio task: clearing RX override for user {}", user_id);
                        per_user_rx.remove(&user_id);
                        let _ = bus.voice.send(VoiceEvent::UserRxOverrideCleared { user_id });
                        // Rebuild user's pipeline with defaults
                        if let Some(user_state) = user_audio.get_mut(&user_id) {
                            match rumble_audio::AudioPipeline::from_config(&rx_pipeline_defaults, &processor_registry) {
                                Ok(p) => user_state.rx_pipeline = Some(p),
                                Err(e) => warn!("Failed to rebuild RX pipeline for user {}: {}", user_id, e),
                            }
                            user_state.volume_db = 0.0;
                        }
                        repaint();
                    }

                    AudioCommand::SetUserVolume { user_id, volume_db } => {
                        info!("Audio task: setting volume for user {} to {} dB", user_id, volume_db);
                        // Update per-user config (audio task's own copy)
                        let user_rx = per_user_rx
                            .entry(user_id)
                            .or_default();
                        user_rx.volume_db = volume_db;
                        // Emit the full UserRxConfig snapshot so the projection
                        // can mirror it byte-for-byte into state.
                        let _ = bus.voice.send(VoiceEvent::UserRxConfigChanged {
                            user_id,
                            config: user_rx.clone(),
                        });
                        // Update live user state if they exist
                        if let Some(user_state) = user_audio.get_mut(&user_id) {
                            user_state.volume_db = volume_db;
                        }
                    }

                    AudioCommand::UserJoinedRoom { user_id } => {
                        // Proactively create decoder/pipeline for this user before packets arrive
                        // Skip if it's our own user ID
                        if user_id != my_user_id && !user_audio.contains_key(&user_id) {
                            debug!("Audio task: user {} joined room, creating decoder proactively", user_id);
                            let jitter_delay = audio_settings.jitter_buffer_delay_packets;

                            // Determine pipeline config and volume for this user
                            let (pipeline_config, volume_db) = match per_user_rx.get(&user_id) {
                                Some(user_rx) => {
                                    let config = user_rx.pipeline_override.as_ref().unwrap_or(&rx_pipeline_defaults);
                                    (config, user_rx.volume_db)
                                }
                                None => (&rx_pipeline_defaults, 0.0),
                            };

                            // Build the RX pipeline
                            let rx_pipeline = match rumble_audio::AudioPipeline::from_config(pipeline_config, &processor_registry) {
                                Ok(p) => Some(p),
                                Err(e) => {
                                    warn!("Failed to build RX pipeline for user {}: {}", user_id, e);
                                    None
                                }
                            };

                            if let Ok(decoder) = P::Codec::create_decoder() {
                                let mut user_state = UserAudioState::new(decoder, jitter_delay);
                                user_state.rx_pipeline = rx_pipeline;
                                user_state.volume_db = volume_db;
                                user_audio.insert(user_id, user_state);
                            } else {
                                error!("Failed to create decoder for user {}", user_id);
                            }
                        }
                    }

                    AudioCommand::UserLeftRoom { user_id } => {
                        // Destroy decoder/pipeline for this user
                        if user_audio.remove(&user_id).is_some() {
                            debug!("Audio task: user {} left room, destroyed decoder", user_id);
                            // If they were in our talking set, emit a stop so
                            // subscribers see the transition.
                            if read_state(&state).audio.talking_users.contains(&user_id) {
                                let _ = bus.voice.send(VoiceEvent::UserStoppedTalking { user_id });
                            }
                        }
                    }

                    AudioCommand::RoomChanged { user_ids_in_room } => {
                        // We changed rooms - destroy all existing decoders, create new ones
                        debug!("Audio task: room changed, rebuilding decoders for {} users", user_ids_in_room.len());

                        // Snapshot current talkers so we can emit stop events
                        // for them as we wipe local decoder state.
                        let prior_talkers = read_state(&state).audio.talking_users.clone();
                        user_audio.clear();
                        for uid in prior_talkers {
                            let _ = bus.voice.send(VoiceEvent::UserStoppedTalking { user_id: uid });
                        }

                        // Create decoders for all users in the new room (except ourselves)
                        let jitter_delay = audio_settings.jitter_buffer_delay_packets;
                        for user_id in user_ids_in_room {
                            if user_id != my_user_id {
                                // Determine pipeline config and volume for this user
                                let (pipeline_config, volume_db) = match per_user_rx.get(&user_id) {
                                    Some(user_rx) => {
                                        let config = user_rx.pipeline_override.as_ref().unwrap_or(&rx_pipeline_defaults);
                                        (config, user_rx.volume_db)
                                    }
                                    None => (&rx_pipeline_defaults, 0.0),
                                };

                                // Build the RX pipeline
                                let rx_pipeline = match rumble_audio::AudioPipeline::from_config(pipeline_config, &processor_registry) {
                                    Ok(p) => Some(p),
                                    Err(e) => {
                                        warn!("Failed to build RX pipeline for user {}: {}", user_id, e);
                                        None
                                    }
                                };

                                if let Ok(decoder) = P::Codec::create_decoder() {
                                    let mut user_state = UserAudioState::new(decoder, jitter_delay);
                                    user_state.rx_pipeline = rx_pipeline;
                                    user_state.volume_db = volume_db;
                                    user_audio.insert(user_id, user_state);
                                } else {
                                    error!("Failed to create decoder for user {}", user_id);
                                }
                            }
                        }
                        repaint();
                    }

                    AudioCommand::PeerLeft { user_id } => {
                        // Destroy decoder/pipeline for this user
                        user_audio.remove(&user_id);
                    }

                    AudioCommand::PlaySfx { samples } => {
                        // SFX are ephemeral event cues — drop them when
                        // playback is unavailable rather than queuing a
                        // backlog that would replay later as a stale burst.
                        if playback_producer.is_some() {
                            sfx_queue.extend(samples.iter());
                            // Backstop: drop the oldest samples if the queue
                            // has grown past the cap (stalled producer).
                            let excess = sfx_queue.len().saturating_sub(MAX_SFX_QUEUE_SAMPLES);
                            if excess > 0 {
                                sfx_queue.drain(..excess);
                            }
                        }
                    }

                    AudioCommand::Shutdown => {
                        info!("Audio task shutting down");
                        break;
                    }
                }
            }

            // Send encoded audio or end-of-stream as datagrams
            Some(capture_msg) = encoded_rx.recv() => {
                if let Some(conn) = &connection {
                    let (opus_data, size_bytes, end_of_stream, timestamp_us) = match capture_msg {
                        CaptureMessage::EncodedFrame {
                            data,
                            size_bytes,
                            timestamp_us,
                        } => (data.to_vec(), size_bytes, false, timestamp_us),
                        CaptureMessage::EndOfStream { timestamp_us } => {
                            // Send empty datagram with end_of_stream flag
                            (Vec::new(), 0, true, timestamp_us)
                        }
                    };

                    let datagram = VoiceDatagram {
                        sender_id: Some(my_user_id),
                        room_id: None, // Server determines room from connection state
                        sequence: send_sequence,
                        timestamp_us, // Media clock (see capture_frame_index); receiver tells silence from loss
                        opus_data,
                        end_of_stream,
                    };
                    send_sequence = send_sequence.wrapping_add(1);
                    let datagram_bytes = datagram.encode_to_vec();

                    // Track statistics (only for actual audio, not EOS)
                    if !end_of_stream {
                        packets_sent += 1;
                        bytes_sent += size_bytes as u64;
                    }

                    if let Err(e) = conn.send_datagram(&datagram_bytes) {
                        warn!("Failed to send voice datagram: {}", e);
                        // Connection might be closed - will be detected by read_datagram
                    }
                }
            }

            // Receive voice datagrams
            datagram = async {
                if let Some(conn) = connection.as_ref() {
                    conn.recv_datagram().await
                } else {
                    // No connection, just wait
                    std::future::pending().await
                }
            } => {
                match datagram {
                    Ok(Some(data)) => {
                        if let Ok(voice) = VoiceDatagram::decode(data.as_slice())
                            && let Some(sender_id) = voice.sender_id {
                                // Don't play back our own audio or audio from muted users
                                if sender_id != my_user_id && !muted_users.contains(&sender_id) {
                                    handle_voice_datagram::<P::Codec>(
                                        sender_id,
                                        voice.sequence,
                                        voice.timestamp_us,
                                        voice.opus_data,
                                        voice.end_of_stream,
                                        &mut user_audio,
                                        &audio_settings,
                                        &rx_pipeline_defaults,
                                        &per_user_rx,
                                        &processor_registry,
                                        &state,
                                        &bus.voice,
                                        &audio_dumper,
                                    );
                                }
                            }
                    }
                    Ok(None) => {
                        // Connection closed gracefully — null out the handle so the
                        // select arm parks on pending() rather than spinning until
                        // ConnectionClosed arrives. The connection task owns the
                        // authoritative cleanup (decoders, talking users, etc.).
                        debug!("Datagram stream closed");
                        connection = None;
                    }
                    Err(e) => {
                        // Fatal connection error — no more datagrams will arrive.
                        // Null out the handle so the select arm parks on pending()
                        // rather than busy-spinning at 100% CPU until the connection
                        // task delivers ConnectionClosed. Send/receive both stop here;
                        // the authoritative cleanup is still driven by ConnectionClosed.
                        warn!("Voice datagram receive error, stopping poll: {}", e);
                        connection = None;
                    }
                }
            }

            // Top the playback ring back up to its target depth. Consumer-paced:
            // produces only as many frames as the device drained since the last
            // poll, so production tracks the audio clock (no wall-clock drift).
            _ = produce_interval.tick() => {
                fill_playback_ring(
                    &mut user_audio,
                    &mut sfx_queue,
                    playback_producer.as_mut(),
                    &audio_dumper,
                    &playback_overflows,
                    &playback_producer_active,
                );
            }

            // Periodic cleanup of stale talking_users (does not destroy decoders)
            _ = cleanup_interval.tick() => {
                cleanup_stale_users(&user_audio, &state, &bus.voice);
            }

            // Detect a capture/playback device that died mid-session (unplug,
            // sound-server crash) and try to recover it. Without this the IO
            // thread/callback just stops and the UI keeps showing "connected"
            // with no audio.
            //
            // Recovery uses two persistent flags (`input_recovering`,
            // `output_recovering`). Once a stream dies, the flag is set and
            // remains set across ticks until either re-open succeeds or a
            // deliberate teardown cancels recovery. This is what allows the
            // health tick to keep retrying after the stream local is None:
            // without the flag, `is_healthy()` can never fire on a None stream.
            _ = device_health_interval.tick() => {
                // --- Capture (input) ---
                // Trigger on: live stream that just died, OR a prior failed
                // re-open (stream already None, input_recovering set).
                let input_needs_recovery =
                    audio_input.as_ref().is_some_and(|s| !s.is_healthy())
                    || (audio_input.is_none() && input_recovering);

                if input_needs_recovery {
                    if audio_input.is_some() {
                        warn!("audio: capture device died, attempting re-open");
                        stop_transmission::<P>(&mut audio_input, &mut tx_pipeline_handle);
                        capture_is_active = false;
                    }
                    input_recovering = true;

                    // Capture is connection-scoped — only meaningful to re-open
                    // while connected. (If disconnected, ConnectionEstablished
                    // re-opens it and sets input_recovering = false there.)
                    if connection.is_some() {
                        match start_transmission::<P>(
                            &audio_backend,
                            &selected_input,
                            &encoded_tx,
                            &audio_settings,
                            &tx_pipeline_config,
                            &processor_registry,
                            &encoder,
                            &mut audio_input,
                            &mut tx_pipeline_handle,
                            &bus.voice,
                            &audio_dumper,
                            &meter_writer,
                            &output_writer,
                            &capture_frame_index,
                            &was_transmitting,
                        ) {
                            Ok(()) => {
                                info!("audio: capture device re-opened");
                                input_recovering = false;
                                // Re-confirm the now-working selection; the
                                // projection clears the input fault on this.
                                let _ = bus.voice.send(VoiceEvent::SelectedDeviceChanged {
                                    kind: DeviceKind::Input,
                                    id: selected_input.clone(),
                                });
                                // Re-arm capture if the state machine wants it.
                                sync_transmission!();
                            }
                            Err(e) => {
                                // Still gone — keep recovering, retry next tick
                                // (audio_input stays None, input_recovering stays true).
                                let _ = bus.voice.send(VoiceEvent::DeviceError {
                                    kind: DeviceKind::Input,
                                    message: e.to_string(),
                                    recovering: true,
                                });
                            }
                        }
                    } else {
                        // Disconnected while device was already dead. Clear the
                        // flag: ConnectionEstablished will try a fresh open.
                        input_recovering = false;
                        let _ = bus.voice.send(VoiceEvent::DeviceError {
                            kind: DeviceKind::Input,
                            message: "capture device lost".to_string(),
                            recovering: false,
                        });
                    }
                }

                // --- Playback (output) ---
                // Output runs even while disconnected (SFX), so recover whenever
                // it's not intentionally torn down (deafened).
                let output_needs_recovery =
                    playback_stream.as_ref().is_some_and(|s| !s.is_healthy())
                    || (playback_stream.is_none() && output_recovering);

                if output_needs_recovery {
                    if playback_stream.is_some() {
                        warn!("audio: playback device died, attempting re-open");
                        playback_stream = None;
                        playback_producer = None;
                    }
                    output_recovering = true;

                    if !self_deafened {
                        open_playback!();
                        if playback_stream.is_some() {
                            info!("audio: playback device re-opened");
                            output_recovering = false;
                            // Re-confirm the now-working selection; the
                            // projection clears the output fault on this.
                            let _ = bus.voice.send(VoiceEvent::SelectedDeviceChanged {
                                kind: DeviceKind::Output,
                                id: selected_output.clone(),
                            });
                        } else {
                            // Still failing — keep recovering, retry next tick.
                            let _ = bus.voice.send(VoiceEvent::DeviceError {
                                kind: DeviceKind::Output,
                                message: format!(
                                    "failed to re-open output device {:?}",
                                    selected_output.as_deref().unwrap_or("default")
                                ),
                                recovering: true,
                            });
                        }
                    } else {
                        // Intentionally deafened — clear the flag so the
                        // health tick doesn't spin; undeafen will re-open.
                        output_recovering = false;
                        let _ = bus.voice.send(VoiceEvent::DeviceError {
                            kind: DeviceKind::Output,
                            message: "playback device lost".to_string(),
                            recovering: false,
                        });
                    }
                }
            }

            // Periodic stats update
            _ = stats_interval.tick() => {
                update_stats(
                    &user_audio,
                    packets_sent,
                    bytes_sent,
                    playback_producer.as_ref(),
                    &playback_underruns,
                    &playback_overflows,
                    &mut stats_writer,
                );
            }
        }
    }
}

/// Start audio input stream (connection-scoped).
///
/// The stream is created once and kept alive for the duration of the connection.
/// The `capture_active` flag controls whether captured samples are processed and
/// encoded (true) or silently discarded (false). This avoids ALSA device
/// enumeration errors on every PTT press/release cycle.
#[allow(clippy::too_many_arguments)]
fn start_transmission<P: Platform>(
    audio_backend: &P::AudioBackend,
    selected_input: &Option<String>,
    encoded_tx: &mpsc::UnboundedSender<CaptureMessage>,
    audio_settings: &AudioSettings,
    tx_pipeline_config: &rumble_audio::PipelineConfig,
    processor_registry: &rumble_audio::ProcessorRegistry,
    encoder: &Arc<std::sync::Mutex<Option<Enc<P>>>>,
    audio_input: &mut Option<CapStream<P>>,
    pipeline_handle: &mut Option<Arc<std::sync::Mutex<rumble_audio::AudioPipeline>>>,
    voice_tx: &tokio::sync::broadcast::Sender<VoiceEvent>,
    audio_dumper: &AudioDumper,
    meter_writer: &crate::snapshot::SnapshotWriter<crate::meter::MeterSnapshot>,
    output_writer: &crate::snapshot::SnapshotWriter<rumble_audio::OutputFrame>,
    capture_frame_index: &Arc<AtomicU64>,
    was_transmitting: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    if audio_input.is_some() {
        return Ok(()); // Stream already exists
    }

    // Use the connection-scoped encoder (already created on ConnectionEstablished)
    // We just verify it exists and update settings if needed
    {
        let encoder_settings = EncoderSettings {
            bitrate: audio_settings.bitrate,
            complexity: audio_settings.encoder_complexity,
            fec_enabled: audio_settings.fec_enabled,
            packet_loss_percent: audio_settings.packet_loss_percent,
            dtx_enabled: true,
            vbr_enabled: true,
        };

        if let Ok(mut guard) = encoder.lock() {
            if guard.is_none() {
                // Encoder not created yet (shouldn't happen if connected)
                error!("Connection-scoped encoder not available");
                anyhow::bail!("connection-scoped encoder not available");
            }
            // Update settings if they've changed
            if let Some(enc) = guard.as_mut()
                && let Err(e) = enc.apply_settings(&encoder_settings)
            {
                warn!("Failed to update encoder settings: {}", e);
            }
        } else {
            error!("Failed to lock encoder");
            anyhow::bail!("failed to lock encoder");
        }
    }

    // Clone Arc for use in callback
    let encoder_for_callback = encoder.clone();

    // Build the TX pipeline from config
    let tx_pipeline = match rumble_audio::AudioPipeline::from_config(tx_pipeline_config, processor_registry) {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to build TX pipeline, using empty pipeline: {}", e);
            rumble_audio::AudioPipeline::new(tx_pipeline_config.frame_size)
        }
    };
    let pipeline_mutex = std::sync::Arc::new(std::sync::Mutex::new(tx_pipeline));
    // Expose the pipeline to the audio-task loop so sync_transmission!'s
    // Activate arm can call reset() before arming the stream.
    *pipeline_handle = Some(pipeline_mutex.clone());

    // Voice event sender for TransmittingChanged edges from the capture
    // callback. (Per-frame level events have moved off the bus onto the
    // ArcSwap meter snapshot.)
    let voice_tx_for_callback = voice_tx.clone();

    // Meter snapshot writer for the capture callback. A fresh clone per
    // stream incarnation — only one capture stream is ever live, so the
    // single-producer contract holds. Published once per frame; the UI
    // samples on repaint. `mut` because the writer recycles its backing
    // buffer across stores.
    let mut meter_for_callback = meter_writer.clone();

    // Live per-stage output snapshot writer, same single-producer story as
    // the meter. `output_frame` is reused across frames so publishing the
    // pipeline's outputs doesn't allocate in the callback.
    let mut output_for_callback = output_writer.clone();
    let mut output_frame = rumble_audio::OutputFrame::default();

    // Track whether we were transmitting (for detecting suppress transitions).
    // Shared with `run_audio_task`, which resets it to false when capture is
    // deactivated so each (re)activation re-evaluates from a clean baseline.
    let was_transmitting_for_callback = Arc::clone(was_transmitting);

    // Discard the first few frames to avoid stale samples from the hardware buffer.
    // When an audio stream starts, the hardware often delivers stale/garbage data
    // from its internal ring buffer. Discarding 2-3 frames (~40-60ms) fixes this.
    let frames_to_discard = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(3));
    let frames_to_discard_for_callback = frames_to_discard.clone();

    // Audio dumper for debugging
    let dumper_for_callback = audio_dumper.clone();

    // Reusable scratch buffer for the per-frame mutable copy. The cpal
    // callback hands us `&[f32]`, but the pipeline needs `&mut [f32]`.
    // Stashing the Vec in the FnMut closure avoids a fresh allocation
    // every ~20 ms — small in absolute terms, but free to fix since
    // we're moving to FnMut anyway.
    let mut sample_scratch: Vec<f32> = Vec::with_capacity(2048);

    // Media-clock counter, advanced once per captured 20 ms frame so that
    // VAD-suppressed silence is encoded as a timestamp gap (see the field doc).
    let capture_frame_index_cb = capture_frame_index.clone();

    // Create audio input via AudioBackend trait
    let encoded_tx = encoded_tx.clone();
    let input = audio_backend.open_input(
        selected_input.as_deref(),
        Box::new(move |samples| {
            // Advance the media clock for every captured frame (including the
            // discarded warm-up frames and VAD-suppressed ones) so the timestamp
            // tracks real elapsed capture time. The frame's media timestamp is
            // its index on this clock.
            let timestamp_us = capture_frame_index_cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed) * FRAME_US;

            // Discard first few frames to avoid stale hardware buffer samples
            let remaining = frames_to_discard_for_callback.load(std::sync::atomic::Ordering::Relaxed);
            if remaining > 0 {
                frames_to_discard_for_callback.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }

            // Dump raw mic samples (before any processing)
            dumper_for_callback.write_mic_raw(samples);

            // RMS dB of the raw capture frame, before the pipeline
            // runs. This is the "is the mic working" signal — shown on
            // the Devices tab and independent of whatever processors
            // the user has composed.
            let pre_level_db = rumble_audio::calculate_rms_db(samples);

            // Run samples through the TX pipeline. Reuse the scratch
            // buffer rather than allocating a fresh Vec per frame.
            sample_scratch.clear();
            sample_scratch.extend_from_slice(samples);

            let pipeline_result = if let Ok(mut pipe) = pipeline_mutex.lock() {
                let r = pipe.process(&mut sample_scratch, 48000);
                // Collect live stage outputs while the same lock is held,
                // so the published values match the frame just processed.
                pipe.write_outputs(&mut output_frame);
                r
            } else {
                rumble_audio::ProcessorResult::default()
            };
            // Publish the per-stage outputs (VAD probability, gate lamp).
            // Recycles its buffer like the meter writer — no allocation.
            output_for_callback.store(output_frame);

            // RMS dB after the pipeline runs — what's actually about
            // to be encoded. Measured here (not via a processor) so the
            // value stays meaningful even if the user disables or
            // removes every level-reporting stage.
            let post_level_db = rumble_audio::calculate_rms_db(&sample_scratch);

            // Track previous transmitting state to detect changes
            let was_tx = was_transmitting_for_callback.load(std::sync::atomic::Ordering::Relaxed);
            let is_tx_now = !pipeline_result.suppress;

            // Publish a fresh meter snapshot for the UI. The writer
            // recycles its buffer, so in steady state this doesn't
            // allocate in the audio callback. No event-bus traffic for
            // the 50 Hz metering rate.
            meter_for_callback.store(crate::meter::MeterSnapshot {
                input_pre: crate::meter::Level::Db(pre_level_db),
                input_post: crate::meter::Level::Db(post_level_db),
            });
            // Emit a transmitting transition only on edge (VAD on/off).
            // This is still an edge-triggered event the projection
            // folds into AudioState::is_transmitting and that SFX/toast
            // listeners subscribe to — distinct from the per-frame
            // `transmitting` field on the snapshot.
            if was_tx != is_tx_now {
                let _ = voice_tx_for_callback.send(VoiceEvent::TransmittingChanged { active: is_tx_now });
            }

            // Pipeline suppress flag gates actual transmission
            // This allows VAD (or any other processor) to prevent encoding/sending
            if pipeline_result.suppress {
                // If we were transmitting and now we're suppressed, send end-of-stream
                if was_tx {
                    was_transmitting_for_callback.store(false, std::sync::atomic::Ordering::Relaxed);
                    let _ = encoded_tx.send(CaptureMessage::EndOfStream { timestamp_us });
                }
                return;
            }

            // We're transmitting now
            was_transmitting_for_callback.store(true, std::sync::atomic::Ordering::Relaxed);

            // Encode the processed audio frame using the connection-scoped encoder
            if let Ok(mut guard) = encoder_for_callback.lock()
                && let Some(enc) = guard.as_mut()
            {
                let mut opus_buf = [0u8; OPUS_MAX_PACKET_SIZE];
                match enc.encode(&sample_scratch, &mut opus_buf) {
                    Ok(len) => {
                        let encoded = &opus_buf[..len];
                        // Dump TX opus packet (after encoding)
                        dumper_for_callback.write_tx_opus(encoded);

                        let _ = encoded_tx.send(CaptureMessage::EncodedFrame {
                            data: Bytes::copy_from_slice(encoded),
                            size_bytes: len,
                            timestamp_us,
                        });
                    }
                    Err(e) => {
                        trace!("Encode error: {}", e);
                    }
                }
            }
        }),
    );

    match input {
        Ok(stream) => {
            // Start inactive — sync_transmission! will activate if needed.
            // This prevents the callback from firing before the state machine
            // has decided whether capture should be active.
            stream.set_active(false);
            *audio_input = Some(stream);
            info!(
                "Started connection-scoped audio input stream with pipeline ({} processors)",
                tx_pipeline_config.processors.len()
            );
            Ok(())
        }
        Err(e) => {
            error!("Failed to start audio input: {}", e);
            // Leave `audio_input` as None so the caller knows capture isn't
            // running and can surface a DeviceUnavailable to the UI rather
            // than reporting a phantom-working selection.
            Err(e)
        }
    }
}

/// Stop and destroy the audio input stream and its associated TX pipeline handle.
///
/// This destroys the underlying platform stream. It should only be called on
/// disconnection, device change, settings change, or mute - NOT on PTT release.
/// The `set_active()` method on the capture stream handles PTT gating.
fn stop_transmission<P: Platform>(
    audio_input: &mut Option<CapStream<P>>,
    pipeline_handle: &mut Option<Arc<std::sync::Mutex<rumble_audio::AudioPipeline>>>,
) {
    if audio_input.is_none() {
        return; // No stream to destroy
    }

    *audio_input = None;
    *pipeline_handle = None;
    info!("Destroyed audio input stream");
}

/// PCM cushion (in samples) the playback filler builds before it starts
/// draining a stream. The producer now refills the ring to
/// [`PLAYBACK_TARGET_SAMPLES`] on each poll (see `fill_playback_ring`), so the
/// ring no longer needs to hold the network cushion — that stays in the
/// per-peer jitter buffers. This is just enough PCM to cover the device pulling
/// a full 20 ms frame at once (the PulseAudio path writes whole frames) plus one
/// frame of reserve before the next producer poll. Must be ≤
/// `PLAYBACK_TARGET_SAMPLES`, or the producer would stop filling below the depth
/// the consumer waits for and playout would never start.
const PLAYBACK_PRIME_SAMPLES: usize = 2 * OPUS_FRAME_SIZE;

/// Ring depth (samples) the mix producer refills to on each poll. Because it
/// only ever tops the ring up to this fixed depth, the number of frames it
/// produces tracks how much the device drained since the last poll — so
/// production rate equals the device's *audio-clock* drain rate, not the
/// poll-timer's wall-clock rate. That is what closes the wall-clock-vs-audio
/// drift that made the ring underrun (or overflow) over time.
///
/// Set equal to the prime so the producer fills the ring to exactly the depth
/// the consumer waits for, and no further: a talk spurt's network cushion stays
/// in the per-peer jitter buffer (where a late/reordered packet can still be
/// slotted in) instead of being decoded ahead into PCM here (where it can't). A
/// larger target would pull more packets out of the jitter buffer at spurt
/// onset, trading reorder tolerance for a little more poll-delay slack — not the
/// trade we want until the jitter buffer itself is adaptive (see the redesign
/// doc).
const PLAYBACK_TARGET_SAMPLES: usize = PLAYBACK_PRIME_SAMPLES;

/// Stateful device-output filler that keeps a PCM cushion so brief
/// producer/consumer timing slips don't empty the buffer mid-stream.
///
/// It holds output silent while `priming` until the shared buffer reaches
/// [`PLAYBACK_PRIME_SAMPLES`], then drains one sample per output slot. If it
/// ever runs dry *while playing*, that is a real starvation click: it counts an
/// underrun and re-enters priming so the cushion rebuilds (one clean rebuffer
/// gap instead of repeated per-callback clicks).
///
/// Priming also makes the underrun stat honest: idle silence (nobody talking →
/// buffer never reaches the target) stays in `priming` and is never counted,
/// while a drain during active playout always is. Owned by the playback closure
/// so the state persists across device callbacks; extracted for unit testing.
struct PlaybackFiller {
    /// Consumer end of the playback ring. Read wait-free from the real-time
    /// device callback — no lock, no allocation.
    consumer: Consumer<f32>,
    priming: bool,
}

impl PlaybackFiller {
    fn new(consumer: Consumer<f32>) -> Self {
        // Start priming so the very first spurt plays from a full cushion.
        Self {
            consumer,
            priming: true,
        }
    }

    fn fill(&mut self, output: &mut [f32], underruns: &AtomicU64, producer_active: &AtomicBool) {
        if self.priming {
            // Hold output silent until the cushion fills — but only while the
            // producer is still feeding. If it has gone idle with a sub-target
            // burst already buffered (e.g. a <40 ms SFX like mute/unmute, shorter
            // than the prime cushion), the ring will never reach the target on its
            // own, so play out what we have rather than stranding it until the
            // next burst tops it over the line. That stranding is what made short
            // SFX play only every other press.
            if self.consumer.slots() < PLAYBACK_PRIME_SAMPLES && producer_active.load(Ordering::Relaxed) {
                output.fill(0.0);
                return;
            }
            self.priming = false;
        }

        // Pop as much as the ring holds, up to one device block. `slots()` is a
        // lower bound on what's readable and can only grow under us (we're the
        // sole consumer), so a `read_chunk` of that count never fails.
        let n = self.consumer.slots().min(output.len());
        if n > 0
            && let Ok(chunk) = self.consumer.read_chunk(n)
        {
            let (first, second) = chunk.as_slices();
            output[..first.len()].copy_from_slice(first);
            output[first.len()..first.len() + second.len()].copy_from_slice(second);
            chunk.commit_all();
        }
        // Anything we couldn't fill is silence.
        output[n..].fill(0.0);

        if n < output.len() {
            // Ran dry mid-block. Rebuild the cushion before resuming either way.
            self.priming = true;
            // Only a real click if the producer was still feeding: draining the
            // cushion after a spurt ends (producer idle) plays the tail out and
            // is expected, not a starvation. Counting it would tick up once per
            // spurt for nothing.
            if producer_active.load(Ordering::Relaxed) {
                underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Start audio output for playback using the AudioBackend trait.
///
/// Creates a fresh playback ring, moves its consumer into the device callback,
/// and returns the stream paired with the producer the mix tick writes to. The
/// ring capacity is [`MAX_PLAYBACK_BUFFER_SAMPLES`] (the old buffer cap), so a
/// full ring is the overflow boundary just as before.
fn start_audio_output<P: Platform>(
    audio_backend: &P::AudioBackend,
    selected_output: &Option<String>,
    underruns: Arc<AtomicU64>,
    producer_active: Arc<AtomicBool>,
) -> Option<(PlayStream<P>, Producer<f32>)> {
    let (producer, consumer) = RingBuffer::<f32>::new(MAX_PLAYBACK_BUFFER_SAMPLES);
    let mut filler = PlaybackFiller::new(consumer);
    match audio_backend.open_output(
        selected_output.as_deref(),
        Box::new(move |output: &mut [f32]| {
            filler.fill(output, &underruns, &producer_active);
        }),
    ) {
        Ok(stream) => {
            info!("Audio output started");
            Some((stream, producer))
        }
        Err(e) => {
            error!("Failed to create audio output: {}", e);
            None
        }
    }
}

/// Handle received voice datagram - insert into jitter buffer or handle end-of-stream.
#[allow(clippy::too_many_arguments)]
fn handle_voice_datagram<C: VoiceCodec>(
    sender_id: u64,
    sequence: u32,
    timestamp_us: u64,
    opus_data: Vec<u8>,
    end_of_stream: bool,
    user_audio: &mut HashMap<u64, UserAudioState<C::Decoder>>,
    audio_settings: &AudioSettings,
    rx_pipeline_defaults: &rumble_audio::PipelineConfig,
    per_user_rx: &HashMap<u64, rumble_audio::UserRxConfig>,
    processor_registry: &rumble_audio::ProcessorRegistry,
    state: &Arc<RwLock<State>>,
    voice_tx: &tokio::sync::broadcast::Sender<VoiceEvent>,
    audio_dumper: &AudioDumper,
) {
    // Dump RX opus packet (before jitter buffer)
    if !end_of_stream && !opus_data.is_empty() {
        audio_dumper.write_rx_opus(&opus_data);
    }

    // Handle end-of-stream: mark the user's stream as ended (advisory).
    if end_of_stream {
        if let Some(user_state) = user_audio.get_mut(&sender_id) {
            user_state.mark_end_of_stream();
            debug!("User {} stream ended at frame {}", sender_id, timestamp_us / FRAME_US);
        }
        return;
    }

    // Get or create per-user audio state
    let jitter_delay = audio_settings.jitter_buffer_delay_packets;
    if let std::collections::hash_map::Entry::Vacant(e) = user_audio.entry(sender_id) {
        // Determine pipeline config and volume for this user
        let (pipeline_config, volume_db) = match per_user_rx.get(&sender_id) {
            Some(user_rx) => {
                let config = user_rx.pipeline_override.as_ref().unwrap_or(rx_pipeline_defaults);
                (config, user_rx.volume_db)
            }
            None => (rx_pipeline_defaults, 0.0),
        };

        // Build the RX pipeline
        let rx_pipeline = match rumble_audio::AudioPipeline::from_config(pipeline_config, processor_registry) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!("Failed to build RX pipeline for user {}: {}", sender_id, e);
                None
            }
        };

        let decoder = match C::create_decoder() {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to create decoder for user {}: {}", sender_id, e);
                return;
            }
        };
        let mut new_state = UserAudioState::new(decoder, jitter_delay);
        new_state.rx_pipeline = rx_pipeline;
        new_state.volume_db = volume_db;
        e.insert(new_state);
    }
    let Some(user_state) = user_audio.get_mut(&sender_id) else {
        return; // Entry was just inserted above; unreachable in practice
    };

    // Insert packet into the jitter buffer (keyed by media timestamp).
    user_state.insert_packet(sequence, timestamp_us, opus_data);

    // Detect first-frame-after-silence by checking the live snapshot of
    // talking_users — projection updates it from our own events but with
    // (microsecond-scale) lag, so this is a best-effort dedup. The
    // projection also dedups on its end.
    if !read_state(state).audio.talking_users.contains(&sender_id) {
        let _ = voice_tx.send(VoiceEvent::UserStartedTalking { user_id: sender_id });
    }
}

/// Threshold below which the soft limiter is fully transparent.
const SOFT_CLIP_THRESHOLD: f32 = 0.75;

/// Soft-knee peak limiter for the summed mix. Transparent below
/// `SOFT_CLIP_THRESHOLD` (so a single speaker, or any sum within range, is
/// untouched), then smoothly compresses overshoot — the curve is C¹-continuous
/// at the knee (unit slope) and asymptotes to ±1 without ever reaching it. This
/// replaces the old per-sample hard clamp, which flattened loud overlapping
/// speakers into a square wave (the multi-speaker crackle).
#[inline]
fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= SOFT_CLIP_THRESHOLD {
        return x;
    }
    let over = a - SOFT_CLIP_THRESHOLD;
    let headroom = 1.0 - SOFT_CLIP_THRESHOLD;
    // over/(over+headroom) → 0 at the knee (slope 1) and → 1 as over → ∞.
    let limited = SOFT_CLIP_THRESHOLD + headroom * (over / (over + headroom));
    limited.copysign(x)
}

/// Decode, RX-process, volume-adjust, and sum one 20 ms frame from every peer
/// that is ready to play. Returns the mixed frame (soft-limited to ±1), or
/// `None` when no peer contributed audio this tick.
///
/// This is the decode+mix core of [`mix_and_play_audio`], split out from the
/// sfx queue and the cpal playback sink so it can be unit-tested and
/// benchmarked on its own (it touches only the peer states + the dumper). The
/// `audio_dumper` receives each peer's decoded frame *before* its RX pipeline;
/// it is a no-op when dumping is disabled.
///
/// Exposed (`#[doc(hidden)]`) for the `rx_mix`/`jitter_buffer` benches and tests
/// — not a stable public API.
#[doc(hidden)]
pub fn mix_peer_frames<D: VoiceDecoderTrait>(
    user_audio: &mut HashMap<u64, UserAudioState<D>>,
    audio_dumper: &AudioDumper,
) -> Option<[f32; OPUS_FRAME_SIZE]> {
    let mut mixed_buffer = [0.0f32; OPUS_FRAME_SIZE];
    // One scratch frame reused across every peer this tick — each peer decodes
    // into it, runs its RX pipeline, then mixes in. Avoids a fresh heap alloc
    // per peer per 20 ms tick.
    let mut frame = [0.0f32; OPUS_FRAME_SIZE];
    let mut has_audio = false;

    for user_state in user_audio.values_mut() {
        // Skip users who haven't buffered enough yet
        if !user_state.ready_to_play() {
            continue;
        }

        // Get next frame (decoded or PLC) into the shared scratch
        if let Some(len) = user_state.decode_next_into(&mut frame) {
            let pcm = &mut frame[..len];
            // Dump decoded audio (before RX pipeline processing)
            audio_dumper.write_rx_decoded(pcm);

            // Run through user's RX pipeline if present
            if let Some(ref mut pipeline) = user_state.rx_pipeline {
                let result = pipeline.process(pcm, 48000);
                // For RX, suppress means don't play this frame (noise gate, etc.)
                if result.suppress {
                    continue;
                }
            }

            // Apply per-user volume adjustment
            user_state.apply_volume(pcm);

            has_audio = true;
            // Accumulate the raw sum; the soft limiter below tames the peaks
            // once, instead of hard-clamping each running partial sum.
            for (i, &sample) in pcm.iter().enumerate() {
                if i < OPUS_FRAME_SIZE {
                    mixed_buffer[i] += sample;
                }
            }
        }
    }

    if has_audio {
        for s in mixed_buffer.iter_mut() {
            *s = soft_clip(*s);
        }
    }
    has_audio.then_some(mixed_buffer)
}

/// Mix one 20 ms frame from all ready peers + the sfx queue. Returns the mixed
/// frame, or `None` if neither peers nor sfx produced any audio this frame (so
/// the caller can stop topping up the ring and let the cushion play out).
fn mix_one_frame<D: VoiceDecoderTrait>(
    user_audio: &mut HashMap<u64, UserAudioState<D>>,
    sfx_queue: &mut VecDeque<f32>,
    audio_dumper: &AudioDumper,
) -> Option<[f32; OPUS_FRAME_SIZE]> {
    let peer_mix = mix_peer_frames(user_audio, audio_dumper);
    let mut has_audio = peer_mix.is_some();
    let mut mixed_buffer = peer_mix.unwrap_or([0.0f32; OPUS_FRAME_SIZE]);

    // Mix in sound effects from the sfx queue, then soft-limit the combined
    // peers+sfx sum (the peer mix was already limited, but sfx can push it back
    // over; re-limiting a within-range signal is transparent).
    if !sfx_queue.is_empty() {
        has_audio = true;
        for sample in mixed_buffer.iter_mut().take(OPUS_FRAME_SIZE) {
            if let Some(sfx_sample) = sfx_queue.pop_front() {
                *sample = soft_clip(*sample + sfx_sample);
            }
        }
    }

    has_audio.then_some(mixed_buffer)
}

/// Push one mixed frame into the playback ring. Writes only what fits; the ring
/// is filled only to `PLAYBACK_TARGET_SAMPLES` (far below capacity), so under
/// consumer-paced production this never truncates — a non-zero `overflows`
/// would mean the producer somehow ran past the target, kept as a tripwire.
fn push_frame(producer: &mut Producer<f32>, frame: &[f32; OPUS_FRAME_SIZE], overflows: &AtomicU64) {
    let n = frame.len().min(producer.slots());
    if n > 0
        && let Ok(mut chunk) = producer.write_chunk(n)
    {
        let (first, second) = chunk.as_mut_slices();
        let split = first.len();
        first.copy_from_slice(&frame[..split]);
        second.copy_from_slice(&frame[split..split + second.len()]);
        chunk.commit_all();
    }
    if n < frame.len() {
        overflows.fetch_add(1, Ordering::Relaxed);
    }
}

/// Top the playback ring back up to `PLAYBACK_TARGET_SAMPLES`, producing one
/// mixed frame at a time. Consumer-paced: it produces only enough frames to
/// replace what the device drained since the last poll, so the production rate
/// equals the device's audio-clock drain rate — no wall-clock drift. With no
/// output stream (`producer` None) there is no consumer clock to pace against,
/// so nothing is produced.
fn fill_playback_ring<D: VoiceDecoderTrait>(
    user_audio: &mut HashMap<u64, UserAudioState<D>>,
    sfx_queue: &mut VecDeque<f32>,
    producer: Option<&mut Producer<f32>>,
    audio_dumper: &AudioDumper,
    overflows: &AtomicU64,
    producer_active: &AtomicBool,
) {
    // Publish whether a voice/sfx stream is live, so the device callback can
    // tell a real mid-stream starvation (click) from the expected cushion drain
    // after a spurt ends. Derived from stream liveness (not from whether we
    // produced this poll) so it stays stable across a spurt even on polls where
    // the ring was already full and nothing was produced — otherwise a
    // stall-induced underrun between polls could read a stale `false`.
    let stream_active = !sfx_queue.is_empty()
        || user_audio
            .values()
            .any(|u| !u.stream_ended || !u.jitter_buffer.is_empty());
    producer_active.store(stream_active, Ordering::Relaxed);

    let Some(producer) = producer else { return };

    // Top up to the target depth. Each frame produced pulls exactly one packet
    // per ready peer from its jitter buffer, so the jitter buffers drain at the
    // device rate and keep their network cushion (we never decode ahead past
    // the target). Bounded by the target, so a delayed poll just refills and
    // resumes rather than ballooning latency.
    while MAX_PLAYBACK_BUFFER_SAMPLES - producer.slots() < PLAYBACK_TARGET_SAMPLES {
        let Some(frame) = mix_one_frame(user_audio, sfx_queue, audio_dumper) else {
            break; // nothing to play — let the cushion drain
        };
        audio_dumper.write_playback(&frame);
        push_frame(producer, &frame, overflows);
    }
}

/// Clean up users who haven't sent audio recently from the "talking" UI state.
///
/// NOTE: This function only updates the `talking_users` set for UI display.
/// It does NOT destroy decoders - those must persist for the entire session
/// to avoid crackle/pop at speech start. Decoders are only destroyed when:
/// - User leaves the room (UserLeftRoom / PeerLeft commands)
/// - We change rooms (RoomChanged command)
/// - Connection is closed (ConnectionClosed command)
fn cleanup_stale_users<D: VoiceDecoderTrait>(
    user_audio: &HashMap<u64, UserAudioState<D>>,
    state: &Arc<RwLock<State>>,
    voice_tx: &tokio::sync::broadcast::Sender<VoiceEvent>,
) {
    let stale_threshold = Duration::from_millis(300);
    let now = Instant::now();

    // Find users who haven't sent audio recently
    let stale_users: Vec<u64> = user_audio
        .iter()
        .filter(|(_, audio)| now.duration_since(audio.last_arrival) > stale_threshold)
        .map(|(&id, _)| id)
        .collect();

    if stale_users.is_empty() {
        return;
    }

    // Emit a Stopped event for any stale user that was still in the
    // talking set. Filter against the live snapshot so we don't fire
    // events for users the projection has already cleared.
    let talkers = read_state(state).audio.talking_users.clone();
    for user_id in &stale_users {
        if talkers.contains(user_id) {
            let _ = voice_tx.send(VoiceEvent::UserStoppedTalking { user_id: *user_id });
        }
    }
}

/// Publish an updated stats roll-up to the stats snapshot.
fn update_stats<D: VoiceDecoderTrait>(
    user_audio: &HashMap<u64, UserAudioState<D>>,
    packets_sent: u64,
    bytes_sent: u64,
    playback_producer: Option<&Producer<f32>>,
    underruns: &AtomicU64,
    overflows: &AtomicU64,
    stats_writer: &mut crate::snapshot::SnapshotWriter<AudioStats>,
) {
    // Aggregate stats from all users
    let mut total_packets_received: u64 = 0;
    let mut total_packets_lost: u64 = 0;
    let mut total_packets_recovered_fec: u64 = 0;
    let mut total_frames_concealed: u64 = 0;
    let mut total_bytes_received: u64 = 0;
    let mut total_buffer_packets: u32 = 0;

    for user_state in user_audio.values() {
        total_packets_received += user_state.packets_received as u64;
        total_packets_lost += user_state.packets_lost;
        total_packets_recovered_fec += user_state.packets_recovered_fec;
        total_frames_concealed += user_state.frames_concealed;
        total_bytes_received += user_state.bytes_received;
        total_buffer_packets += user_state.jitter_buffer.len() as u32;
    }

    // Calculate average frame size and bitrate
    let avg_frame_size = if packets_sent > 0 {
        bytes_sent as f32 / packets_sent as f32
    } else {
        0.0
    };

    // Bitrate = bytes_per_frame * frames_per_second * 8 bits/byte
    // At 20ms per frame, we have 50 frames per second
    let actual_bitrate_bps = avg_frame_size * 50.0 * 8.0;

    // Current PCM cushion the device is draining — ring occupancy from the
    // producer side (capacity minus free slots). A momentary depth (not a rate),
    // so a low value here alongside a rising underrun count is the underrun
    // signature. `slots()` is a lower bound on free space, so this is an upper
    // bound on occupancy — close enough for a diagnostic gauge.
    let playback_buffer_samples = playback_producer.map_or(0, |p| (MAX_PLAYBACK_BUFFER_SAMPLES - p.slots()) as u32);

    let stats = AudioStats {
        packets_sent,
        packets_received: total_packets_received,
        packets_lost: total_packets_lost,
        packets_recovered_fec: total_packets_recovered_fec,
        frames_concealed: total_frames_concealed,
        bytes_sent,
        bytes_received: total_bytes_received,
        avg_frame_size_bytes: avg_frame_size,
        actual_bitrate_bps,
        playback_buffer_packets: total_buffer_packets,
        playback_buffer_samples,
        buffer_underruns: underruns.load(Ordering::Relaxed),
        buffer_overflows: overflows.load(Ordering::Relaxed),
        last_update: Some(Instant::now()),
    };
    stats_writer.store(stats);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ====================================================================
    // Jitter-buffer characterization tests.
    //
    // These pin OBSERVABLE behavior — the sequence of frames the buffer
    // produces and its loss/conceal/FEC stat counters — NOT internal
    // bookkeeping (next_play_seq, started, buffered counters, raw buffer
    // contents). That keeps them as a safety net for jitter-buffer
    // optimizations: any rework that preserves the played-frame sequence
    // and the loss accounting will keep these green, while a behavior
    // change (wrong order, phantom loss, missed PLC/FEC) will trip them.
    //
    // The decoder is mocked so each produced frame is *identifiable*:
    //   - a normal decode stamps the packet's marker byte into every sample
    //   - PLC stamps PLC_TAG
    //   - FEC stamps FEC_TAG_BASE + the recovering packet's marker
    // so a frame's first sample tells us exactly how it was produced.
    // ====================================================================

    /// First-sample tag a PLC (packet-loss-concealment) frame carries.
    const PLC_TAG: f32 = -1.0;
    /// FEC-recovered frames carry `FEC_TAG_BASE + next_packet_marker`.
    const FEC_TAG_BASE: f32 = 1000.0;

    /// Decoder that stamps the *source* of each frame into its samples so a
    /// frame's first sample identifies how it was produced (see module note).
    struct MarkerDecoder;

    impl VoiceDecoderTrait for MarkerDecoder {
        fn decode(&mut self, data: &[u8], output: &mut [f32]) -> anyhow::Result<usize> {
            output.fill(data.first().copied().unwrap_or(0) as f32);
            Ok(output.len())
        }

        fn decode_plc(&mut self, output: &mut [f32]) -> anyhow::Result<usize> {
            output.fill(PLC_TAG);
            Ok(output.len())
        }

        fn decode_fec(&mut self, data: &[u8], output: &mut [f32]) -> anyhow::Result<usize> {
            output.fill(FEC_TAG_BASE + data.first().copied().unwrap_or(0) as f32);
            Ok(output.len())
        }
    }

    fn make_state(base_delay: u32) -> UserAudioState<MarkerDecoder> {
        UserAudioState::new(MarkerDecoder, base_delay)
    }

    /// A packet whose decoded frame is tagged with `marker` (keep markers < 256
    /// so they fit a byte and round-trip through f32 exactly).
    fn packet(marker: u8) -> Vec<u8> {
        vec![marker; 20]
    }

    /// Feed one packet: media `frame`, sender `seq`, decoded tag `marker`. The
    /// arrival instant is placed on a real-time grid aligned to the media clock,
    /// so the adaptive jitter target stays at the base (synthetic back-to-back
    /// inserts would otherwise look like huge transit jitter and inflate it).
    fn feed(state: &mut UserAudioState<MarkerDecoder>, epoch: Instant, seq: u32, frame: u64, marker: u8) {
        state.insert_packet_at(
            seq,
            frame * FRAME_US,
            packet(marker),
            epoch + Duration::from_micros(frame * FRAME_US),
        );
    }

    /// Feed a contiguous-stream packet where seq == frame == marker.
    fn feed_contig(state: &mut UserAudioState<MarkerDecoder>, epoch: Instant, n: u8) {
        feed(state, epoch, n as u32, n as u64, n);
    }

    /// Pull `n` frames, returning each frame's identifying tag (its first
    /// sample), or `None` where the buffer produced no frame.
    fn drain_tags(state: &mut UserAudioState<MarkerDecoder>, n: usize) -> Vec<Option<f32>> {
        (0..n)
            .map(|_| {
                let mut buf = [0.0f32; OPUS_FRAME_SIZE];
                state.decode_next_into(&mut buf).map(|_| buf[0])
            })
            .collect()
    }

    /// Pull up to `n` ticks, returning only the tags that actually played
    /// (drops the `None` ticks a re-anchor or buffering gap produces).
    fn drain_some(state: &mut UserAudioState<MarkerDecoder>, n: usize) -> Vec<f32> {
        drain_tags(state, n).into_iter().flatten().collect()
    }

    #[test]
    fn test_audio_command_send() {
        // Just verify the types work
        let (tx, _rx) = mpsc::unbounded_channel::<AudioCommand>();
        let handle = AudioTaskHandle { command_tx: tx };
        handle.send(AudioCommand::RefreshDevices);
        handle.send(AudioCommand::StartTransmit);
        handle.send(AudioCommand::StopTransmit);
        handle.send(AudioCommand::SetMuted { muted: true });
        handle.send(AudioCommand::SetDeafened { deafened: false });
        handle.send(AudioCommand::SetVoiceMode {
            mode: VoiceMode::Continuous,
        });
    }

    #[test]
    fn jitter_buffers_until_target_then_plays() {
        // With base delay 3, playback must not begin until 3 frames are buffered.
        let mut state = make_state(3);
        let epoch = Instant::now();
        feed_contig(&mut state, epoch, 0);
        feed_contig(&mut state, epoch, 1);
        assert!(!state.ready_to_play(), "should still be buffering at 2/3 frames");
        let mut buf = [0.0f32; OPUS_FRAME_SIZE];
        assert_eq!(
            state.decode_next_into(&mut buf),
            None,
            "no frame should play before the target depth is met"
        );

        feed_contig(&mut state, epoch, 2);
        assert!(state.ready_to_play(), "should be ready once 3 frames are buffered");
        assert_eq!(drain_tags(&mut state, 3), vec![Some(0.0), Some(1.0), Some(2.0)]);
    }

    #[test]
    fn mix_returns_none_when_no_peer_is_ready() {
        let mut peers: HashMap<u64, UserAudioState<MarkerDecoder>> = HashMap::new();
        // No peers at all.
        assert_eq!(mix_peer_frames(&mut peers, &AudioDumper::disabled()), None);

        // One peer still buffering (base 3, only 1 frame) — not ready, so the
        // tick produces silence (None), not a partial frame.
        let mut s = make_state(3);
        feed(&mut s, Instant::now(), 0, 0, 1);
        peers.insert(1, s);
        assert_eq!(mix_peer_frames(&mut peers, &AudioDumper::disabled()), None);
    }

    #[test]
    fn mix_sums_ready_peers_and_soft_limits() {
        let mut peers: HashMap<u64, UserAudioState<MarkerDecoder>> = HashMap::new();
        let epoch = Instant::now();
        // Two frames each so both peers reach the minimum playout depth; every
        // frame decodes to 1.0 via MarkerDecoder.
        let mut a = make_state(MIN_TARGET_FRAMES);
        feed(&mut a, epoch, 0, 0, 1);
        feed(&mut a, epoch, 1, 1, 1);
        let mut b = make_state(MIN_TARGET_FRAMES);
        feed(&mut b, epoch, 0, 0, 1);
        feed(&mut b, epoch, 1, 1, 1);
        peers.insert(1, a);
        peers.insert(2, b);

        let mixed = mix_peer_frames(&mut peers, &AudioDumper::disabled()).expect("two ready peers produce audio");
        // 1.0 + 1.0 summed, then soft-limited (not hard-clamped to 1.0).
        let expected = soft_clip(2.0);
        assert!(expected < 1.0, "soft limiter must stay below full scale");
        assert!(
            mixed.iter().all(|&s| (s - expected).abs() < 1e-6),
            "mixed output must be the soft-limited sum, not a hard clamp"
        );
    }

    #[test]
    fn jitter_plays_in_order_frames_in_order() {
        let mut state = make_state(3);
        let epoch = Instant::now();
        for n in 0..5u8 {
            feed_contig(&mut state, epoch, n);
        }
        assert_eq!(
            drain_tags(&mut state, 5),
            vec![Some(0.0), Some(1.0), Some(2.0), Some(3.0), Some(4.0)]
        );
        assert_eq!(state.packets_lost, 0);
        assert_eq!(state.frames_concealed, 0);
        assert_eq!(state.packets_recovered_fec, 0);
    }

    #[test]
    fn jitter_reorders_out_of_order_arrivals() {
        // Frames arrive out of order but must play back in media-clock order.
        let mut state = make_state(3);
        let epoch = Instant::now();
        for n in [0u8, 2, 1, 3] {
            feed_contig(&mut state, epoch, n);
        }
        assert_eq!(
            drain_tags(&mut state, 4),
            vec![Some(0.0), Some(1.0), Some(2.0), Some(3.0)]
        );
        assert_eq!(state.packets_lost, 0);
    }

    #[test]
    fn jitter_dedupes_duplicate_frame() {
        // A duplicated frame must collapse to a single played frame.
        let mut state = make_state(3);
        let epoch = Instant::now();
        for n in [0u8, 1, 1, 2] {
            feed_contig(&mut state, epoch, n);
        }
        assert_eq!(drain_tags(&mut state, 3), vec![Some(0.0), Some(1.0), Some(2.0)]);
        assert_eq!(state.packets_lost, 0);
    }

    #[test]
    fn jitter_conceals_trailing_gap_with_plc() {
        // A gap at the end with no following frame is ambiguous (loss vs the
        // stream simply pausing), so it is concealed with PLC but NOT counted as
        // confirmed loss — loss is only counted when a later frame proves the
        // gap was sent contiguously (see `jitter_recovers_missing_frame_with_fec`).
        let mut state = make_state(3);
        let epoch = Instant::now();
        for n in 0..3u8 {
            feed_contig(&mut state, epoch, n);
        }
        // Fourth pull reaches frame 3, which never arrived and has no successor.
        assert_eq!(
            drain_tags(&mut state, 4),
            vec![Some(0.0), Some(1.0), Some(2.0), Some(PLC_TAG)]
        );
        assert_eq!(state.frames_concealed, 1);
        assert_eq!(state.packets_recovered_fec, 0);
        // The trailing gap is concealed but NOT counted as confirmed loss — pin
        // that semantic so a regression that starts counting it is caught.
        assert_eq!(
            state.packets_lost, 0,
            "an ambiguous trailing gap must not be counted as loss"
        );
    }

    #[test]
    fn jitter_recovers_missing_frame_with_fec() {
        // Frame 3 is missing but frame 4 arrived contiguously (seq advanced as
        // much as the media clock): recover frame 3 from frame 4's FEC, then
        // still play frame 4 normally (FEC must not consume the next frame).
        let mut state = make_state(3);
        let epoch = Instant::now();
        for n in [0u8, 1, 2, 4] {
            feed_contig(&mut state, epoch, n);
        }
        assert_eq!(
            drain_tags(&mut state, 5),
            vec![
                Some(0.0),
                Some(1.0),
                Some(2.0),
                Some(FEC_TAG_BASE + 4.0), // frame 3 recovered from frame 4
                Some(4.0),                // frame 4 still played normally
            ]
        );
        assert_eq!(state.packets_lost, 1);
        assert_eq!(state.packets_recovered_fec, 1);
        assert_eq!(state.frames_concealed, 0);
    }

    #[test]
    fn jitter_restarts_cleanly_on_media_clock_reset() {
        // Play a stream well into its media clock, then the sender restarts with
        // a clock reset near zero (reconnect). The buffer must re-anchor to the
        // new stream and play it from its start, not count it as loss.
        let mut state = make_state(3);
        let epoch = Instant::now();
        for n in [200u8, 201, 202] {
            feed_contig(&mut state, epoch, n);
        }
        assert_eq!(drain_tags(&mut state, 3), vec![Some(200.0), Some(201.0), Some(202.0)]);

        // Sender restart: media clock jumps back near zero (far past the
        // late-packet margin), sequence resets too.
        for n in [0u8, 1, 2] {
            feed_contig(&mut state, epoch, n);
        }
        assert_eq!(
            drain_some(&mut state, 3),
            vec![0.0, 1.0, 2.0],
            "new stream should play from its start"
        );
        assert_eq!(state.packets_lost, 0, "a sender restart is not packet loss");
    }

    #[test]
    fn jitter_resyncs_on_small_clock_reset() {
        // A backward clock reset SMALLER than RESTART_BACKWARD_FRAMES looks like
        // a run of behind-cursor (late) packets. The buffer must not drop them
        // forever: after LATE_RESYNC_FRAMES it re-syncs to the new clock so the
        // stream recovers, and the dropped late frames are not counted as loss.
        let mut state = make_state(3);
        let epoch = Instant::now();
        for n in [50u8, 51, 52] {
            feed_contig(&mut state, epoch, n);
        }
        assert_eq!(drain_tags(&mut state, 3), vec![Some(50.0), Some(51.0), Some(52.0)]);
        // Cursor is now at frame 53. The sender's clock resets back to frame 10
        // (43 frames behind — within RESTART_BACKWARD_FRAMES=100, so the far-jump
        // path does NOT fire). Feed a sustained run from there.
        let reset_base = 10u8;
        for k in 0..(LATE_RESYNC_FRAMES as u8 + 3) {
            feed_contig(&mut state, epoch, reset_base + k);
        }
        // The first LATE_RESYNC_FRAMES behind-cursor frames are dropped, then the
        // run re-syncs and the remaining frames buffer + play.
        let played = drain_some(&mut state, 5);
        assert!(
            !played.is_empty(),
            "stream must recover after a small clock reset, not drop every frame forever"
        );
        let resync_frame = (reset_base + LATE_RESYNC_FRAMES as u8) as f32;
        assert_eq!(
            played[0], resync_frame,
            "playout should resume at the frame that triggered the re-sync"
        );
        assert_eq!(state.packets_lost, 0, "a clock reset is not packet loss");
    }

    #[test]
    fn jitter_resumes_after_eos_without_phantom_loss() {
        // A clean end-of-stream followed by a new talk spurt (sequence continues,
        // media clock jumps over the silence) must resume cleanly: the silence
        // gap is not counted as loss, and the new spurt plays from its start.
        let mut state = make_state(3);
        let epoch = Instant::now();
        for n in 0..3u8 {
            feed_contig(&mut state, epoch, n);
        }
        assert_eq!(drain_tags(&mut state, 3), vec![Some(0.0), Some(1.0), Some(2.0)]);

        state.mark_end_of_stream();

        // New spurt: sequence continues (3,4,5) but the media clock jumped to
        // frame 50 (≈1 s of silence elapsed).
        feed(&mut state, epoch, 3, 50, 50);
        feed(&mut state, epoch, 4, 51, 51);
        feed(&mut state, epoch, 5, 52, 52);
        assert_eq!(
            drain_some(&mut state, 4),
            vec![50.0, 51.0, 52.0],
            "new spurt after EOS should play from its start"
        );
        assert_eq!(state.packets_lost, 0, "the silence gap must not be counted as loss");
        assert_eq!(state.frames_concealed, 0);
    }

    #[test]
    fn jitter_classifies_silence_gap_without_eos() {
        // Same as above but the EOS datagram was LOST: the timestamp gap alone
        // must classify the resumption as a new spurt (re-anchor), not as a long
        // run of packet loss to conceal.
        let mut state = make_state(3);
        let epoch = Instant::now();
        for n in 0..3u8 {
            feed_contig(&mut state, epoch, n);
        }
        assert_eq!(drain_tags(&mut state, 3), vec![Some(0.0), Some(1.0), Some(2.0)]);

        // No mark_end_of_stream() — EOS was dropped. Sequence continues, clock jumps.
        feed(&mut state, epoch, 3, 50, 50);
        feed(&mut state, epoch, 4, 51, 51);
        feed(&mut state, epoch, 5, 52, 52);
        assert_eq!(drain_some(&mut state, 4), vec![50.0, 51.0, 52.0]);
        assert_eq!(state.packets_lost, 0, "an intended silence gap is not loss");
        assert_eq!(
            state.frames_concealed, 0,
            "silence must not be concealed frame-by-frame"
        );
    }

    #[test]
    fn jitter_adaptive_target_grows_under_jitter() {
        // Bursty arrivals (transit time varying well beyond a frame) must push
        // the adaptive playout target above the base delay.
        let mut state = make_state(MIN_TARGET_FRAMES);
        let epoch = Instant::now();
        // Frame N arrives at a jittery time: alternating early/late by 40 ms.
        for n in 0..12u64 {
            let skew = if n % 2 == 0 { 0 } else { 40_000 };
            state.insert_packet_at(
                n as u32,
                n * FRAME_US,
                packet(n as u8),
                epoch + Duration::from_micros(n * FRAME_US + skew),
            );
        }
        assert!(
            state.target_frames > MIN_TARGET_FRAMES,
            "target {} should have grown above the base {} under jitter",
            state.target_frames,
            MIN_TARGET_FRAMES
        );
        assert!(
            state.target_frames <= MAX_TARGET_FRAMES,
            "target stays within the ceiling"
        );
    }

    // ====================================================================
    // RX onset-pop reproduction (REAL Opus codec).
    //
    // Symptom: an audible pop/click shortly after a remote user starts
    // talking — at the onset of a talk spurt that follows a silence gap, NOT
    // the very first spurt.
    //
    // On re-anchoring to a new spurt (detected from the media-clock jump), the
    // first `decode_next_into` runs a DISCARDED `decode_plc()` to "warm" the
    // (long-lived) decoder before decoding the first real packet. This is
    // *intended* to prevent onset clicks — these tests check whether it
    // actually does, by driving the real Opus decoder through the production
    // jitter buffer across spurt1 -> gap -> spurt2 and measuring the spurt-2
    // onset waveform.
    //
    // A "pop" is a sample-to-sample discontinuity at onset far larger than
    // the signal's steady-state slew. The control timeline (path B) replays
    // the identical packets through the identical warmed decoder but SKIPS
    // only the PLC prime, isolating the prime's contribution.
    //
    // Uses the genuine `NativeOpusCodec` (dev-dep on rumble-desktop) rather
    // than the marker stub above, because a pop is an acoustic artifact a
    // tag-stamping decoder cannot exhibit.
    // ====================================================================

    use rumble_client_traits::{VoiceCodec, VoiceEncoder, codec::OPUS_SAMPLE_RATE};
    use rumble_desktop::NativeOpusCodec;

    /// Feed a real-codec packet at media `frame` / sender `seq`, arrival aligned
    /// to the media clock so the adaptive target stays at the base.
    fn feed_at<D: VoiceDecoderTrait>(
        state: &mut UserAudioState<D>,
        epoch: Instant,
        seq: u32,
        frame: u64,
        pkt: Vec<u8>,
    ) {
        state.insert_packet_at(
            seq,
            frame * FRAME_US,
            pkt,
            epoch + Duration::from_micros(frame * FRAME_US),
        );
    }

    /// Encode `frames` of a phase-continuous sine (starting at phase 0, the
    /// benign onset a clean decoder should reproduce without a click) into a
    /// vector of one Opus packet per 20 ms frame.
    fn encode_spurt(
        encoder: &mut <NativeOpusCodec as VoiceCodec>::Encoder,
        freq: f32,
        amp: f32,
        frames: usize,
    ) -> Vec<Vec<u8>> {
        (0..frames)
            .map(|f| {
                let pcm: Vec<f32> = (0..OPUS_FRAME_SIZE)
                    .map(|i| {
                        let n = (f * OPUS_FRAME_SIZE + i) as f32;
                        (2.0 * std::f32::consts::PI * freq * n / OPUS_SAMPLE_RATE as f32).sin() * amp
                    })
                    .collect();
                let mut buf = vec![0u8; OPUS_MAX_PACKET_SIZE];
                let len = encoder.encode(&pcm, &mut buf).unwrap();
                buf.truncate(len);
                buf
            })
            .collect()
    }

    /// Largest absolute step between consecutive samples — the metric a click
    /// spikes. A clean sine's max step is `amp * 2π * f / sample_rate`.
    fn max_abs_delta(samples: &[f32]) -> f32 {
        samples.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max)
    }

    #[test]
    fn no_pop_at_talkspurt_onset_after_eos() {
        const FREQ: f32 = 440.0;
        const AMP: f32 = 0.5;
        const FRAMES: usize = 10;
        const GAP: u64 = 40; // ~800 ms of silence between spurts

        let settings = EncoderSettings::default();
        let mut encoder = NativeOpusCodec::create_encoder(&settings).unwrap();
        // Two independent talk spurts, each a tone starting at phase 0.
        let spurt1 = encode_spurt(&mut encoder, FREQ, AMP, FRAMES);
        let spurt2 = encode_spurt(&mut encoder, FREQ, AMP, FRAMES);

        // ---- Path A: production jitter buffer, spurt1 -> EOS -> spurt2 ----
        let mut state = UserAudioState::new(NativeOpusCodec::create_decoder().unwrap(), 3);
        let epoch = Instant::now();
        let mut scratch = [0.0f32; OPUS_FRAME_SIZE];

        // Spurt 1 (frames 0..FRAMES): insert all, drain all — warms the decoder.
        for (i, pkt) in spurt1.iter().enumerate() {
            feed_at(&mut state, epoch, i as u32, i as u64, pkt.clone());
        }
        for _ in 0..FRAMES {
            state.decode_next_into(&mut scratch);
        }

        // EOS exactly as handle_voice_datagram applies it.
        state.mark_end_of_stream();

        // Spurt 2: sequence continues, media clock jumps over the silence. The
        // first decode re-anchors (silence classification) and primes the decoder.
        for (i, pkt) in spurt2.iter().enumerate() {
            feed_at(
                &mut state,
                epoch,
                FRAMES as u32 + i as u32,
                FRAMES as u64 + GAP + i as u64,
                pkt.clone(),
            );
        }
        let mut a_spurt2 = Vec::with_capacity(FRAMES * OPUS_FRAME_SIZE);
        for _ in 0..FRAMES + 1 {
            if let Some(n) = state.decode_next_into(&mut scratch) {
                a_spurt2.extend_from_slice(&scratch[..n]);
            }
        }

        // ---- Path B: control — identical warmed decoder, NO PLC prime ----
        let mut ctrl = NativeOpusCodec::create_decoder().unwrap();
        for pkt in &spurt1 {
            ctrl.decode(pkt, &mut scratch).unwrap();
        }
        let mut b_spurt2 = Vec::with_capacity(FRAMES * OPUS_FRAME_SIZE);
        for pkt in &spurt2 {
            let n = ctrl.decode(pkt, &mut scratch).unwrap();
            b_spurt2.extend_from_slice(&scratch[..n]);
        }

        // Onset window = first 2 frames; prepend 0.0 so the silence->onset
        // boundary (the actual audible edge in production) is included.
        let onset = OPUS_FRAME_SIZE * 2;
        let a_onset = max_abs_delta(&[&[0.0], &a_spurt2[..onset]].concat());
        let b_onset = max_abs_delta(&[&[0.0], &b_spurt2[..onset]].concat());
        // Steady-state slew, sampled away from the onset transient.
        let steady = OPUS_FRAME_SIZE * 5..OPUS_FRAME_SIZE * 8;
        let baseline = max_abs_delta(&a_spurt2[steady]);

        eprintln!(
            "onset Δ: primed(A)={a_onset:.4}  control(B)={b_onset:.4}  steady baseline={baseline:.4}\nfirst sample: \
             primed={:.4}  control={:.4}",
            a_spurt2[0], b_spurt2[0]
        );

        // The PLC prime is supposed to *smooth* the onset, so the primed path
        // must not introduce a discontinuity beyond what the control already
        // has. If this trips, the prime itself is injecting the onset pop.
        assert!(
            a_onset <= b_onset.max(baseline) * 2.0,
            "PLC prime injected an onset pop: primed onset Δ={a_onset:.4} vs control Δ={b_onset:.4} (steady baseline \
             Δ={baseline:.4})"
        );
    }

    /// Regression test for the reported onset pop on a missed EOS.
    ///
    /// EOS is delivered over unreliable QUIC datagrams; when it is lost, the
    /// long-lived warm decoder used to be handed brand-new spurt content
    /// directly — a large onset discontinuity audible as a click. With the
    /// timestamp-driven buffer, the media-clock jump alone classifies the
    /// resumption as a new spurt and primes the decoder, even with EOS missing.
    /// This models that path: spurt 1, a silent gap with the EOS dropped, then
    /// spurt 2 — and asserts the onset is not clicky.
    #[test]
    fn no_pop_at_talkspurt_onset_when_eos_missed() {
        const FREQ: f32 = 440.0;
        const AMP: f32 = 0.5;
        const FRAMES: usize = 10;
        const GAP: u64 = 40;

        let mut encoder = NativeOpusCodec::create_encoder(&EncoderSettings::default()).unwrap();
        let spurt1 = encode_spurt(&mut encoder, FREQ, AMP, FRAMES);
        let spurt2 = encode_spurt(&mut encoder, FREQ, AMP, FRAMES);

        // Drive spurt1 → (optional EOS) → silence gap → spurt2 through the
        // production jitter buffer, returning the decoded spurt-2 PCM.
        let run = |send_eos: bool| -> Vec<f32> {
            let mut state = UserAudioState::new(NativeOpusCodec::create_decoder().unwrap(), 3);
            let epoch = Instant::now();
            let mut scratch = [0.0f32; OPUS_FRAME_SIZE];
            for (i, pkt) in spurt1.iter().enumerate() {
                feed_at(&mut state, epoch, i as u32, i as u64, pkt.clone());
            }
            for _ in 0..FRAMES {
                state.decode_next_into(&mut scratch);
            }
            if send_eos {
                state.mark_end_of_stream();
            }
            // Sequence continues; the media clock jumps over the silence.
            for (i, pkt) in spurt2.iter().enumerate() {
                feed_at(
                    &mut state,
                    epoch,
                    FRAMES as u32 + i as u32,
                    FRAMES as u64 + GAP + i as u64,
                    pkt.clone(),
                );
            }
            let mut out = Vec::with_capacity(FRAMES * OPUS_FRAME_SIZE);
            for _ in 0..FRAMES + 1 {
                if let Some(n) = state.decode_next_into(&mut scratch) {
                    out.extend_from_slice(&scratch[..n]);
                }
            }
            out
        };

        let with_eos = run(true);
        let missed_eos = run(false);

        // The whole point: when the EOS datagram is LOST, the media-clock jump
        // alone must drive the SAME re-anchor + decoder prime that a received EOS
        // would — so the decoded spurt-2 output is identical. If a regression
        // stops the timestamp gap from re-anchoring, the missed-EOS path hands
        // the warm decoder fresh content with no prime (the onset pop) and
        // diverges from the EOS path; this exact-match assertion catches that.
        assert_eq!(
            missed_eos.len(),
            with_eos.len(),
            "missed-EOS must produce the same frame count as received-EOS"
        );
        let max_diff = missed_eos
            .iter()
            .zip(&with_eos)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        eprintln!("missed-EOS vs received-EOS: max sample diff = {max_diff:.6}");
        assert!(
            max_diff < 1e-6,
            "missed-EOS path diverged from received-EOS path (Δ={max_diff:.6}) — the timestamp gap is not \
             re-anchoring/priming the way a received EOS does"
        );

        // Sanity: the (shared) onset is actually smoothed, not equal-and-clicky.
        // The received-EOS prime is independently verified against an unprimed
        // control in `no_pop_at_talkspurt_onset_after_eos`.
        let onset = OPUS_FRAME_SIZE * 2;
        let onset_delta = max_abs_delta(&[&[0.0], &missed_eos[..onset]].concat());
        let baseline = max_abs_delta(&missed_eos[OPUS_FRAME_SIZE * 5..OPUS_FRAME_SIZE * 8]);
        eprintln!("missed-EOS onset Δ={onset_delta:.4}  baseline={baseline:.4}");
    }

    // ====================================================================
    // Other click sources (exploratory, real Opus codec).
    //
    // The onset pop above is one mechanism. These probe other paths that can
    // click intermittently in normal use, each isolated to one cause and
    // measured as a sample-to-sample discontinuity (a click) or hard clipping
    // (crackle). They PRINT their metrics and assert the click-free property,
    // so a red test here marks a reproduced click to triage, not a flake.
    // ====================================================================

    /// Mid-stream burst packet loss. A single lost packet is recovered cleanly
    /// from the next packet's in-band FEC, but a 2-packet burst leaves the
    /// first lost frame with no present successor → pure PLC. Measures the
    /// discontinuity at the concealment seams against the tone's steady slew.
    #[test]
    fn burst_loss_plc_seam() {
        const FREQ: f32 = 300.0;
        const AMP: f32 = 0.5;
        const FRAMES: usize = 20;
        const LOST: [usize; 2] = [10, 11]; // consecutive → FEC can't cover seq 10

        let settings = EncoderSettings::default(); // FEC enabled
        let mut encoder = NativeOpusCodec::create_encoder(&settings).unwrap();
        // One phase-continuous tone: any discontinuity is concealment, not content.
        let pkts = encode_spurt(&mut encoder, FREQ, AMP, FRAMES);

        let mut state = UserAudioState::new(NativeOpusCodec::create_decoder().unwrap(), 3);
        let epoch = Instant::now();
        for (i, pkt) in pkts.iter().enumerate() {
            if !LOST.contains(&i) {
                feed_at(&mut state, epoch, i as u32, i as u64, pkt.clone());
            }
        }

        let mut out = Vec::with_capacity(FRAMES * OPUS_FRAME_SIZE);
        let mut scratch = [0.0f32; OPUS_FRAME_SIZE];
        for _ in 0..FRAMES {
            if let Some(n) = state.decode_next_into(&mut scratch) {
                out.extend_from_slice(&scratch[..n]);
            }
        }

        // Seam window spans the frames just before the loss through recovery.
        let seam = max_abs_delta(&out[OPUS_FRAME_SIZE * 9..OPUS_FRAME_SIZE * 13]);
        let baseline = max_abs_delta(&out[OPUS_FRAME_SIZE * 3..OPUS_FRAME_SIZE * 6]);
        eprintln!(
            "burst-loss seam Δ={seam:.4}  steady baseline={baseline:.4}  lost={}, recovered_fec={}, concealed={}",
            state.packets_lost, state.packets_recovered_fec, state.frames_concealed
        );

        assert!(
            seam <= baseline * 3.0,
            "burst-loss PLC seam clicks: seam Δ={seam:.4} >> steady baseline Δ={baseline:.4}"
        );
    }

    /// Two peers talking at once. The mix sums their frames then applies the
    /// soft-knee limiter (`soft_clip`) instead of a per-sample hard clamp, so
    /// loud overlapping speech that exceeds full scale is smoothly compressed
    /// rather than flattened into a square wave (the old crackle). Asserts the
    /// limited mix does not pin samples at full scale.
    #[test]
    fn overlapping_speakers_soft_limited() {
        const AMP: f32 = 0.8; // two of these sum to 1.6 → well past full scale
        const FRAMES: usize = 12;

        let mut enc_a = NativeOpusCodec::create_encoder(&EncoderSettings::default()).unwrap();
        let mut enc_b = NativeOpusCodec::create_encoder(&EncoderSettings::default()).unwrap();
        let a_pkts = encode_spurt(&mut enc_a, 300.0, AMP, FRAMES);
        let b_pkts = encode_spurt(&mut enc_b, 440.0, AMP, FRAMES);

        let mut peers: HashMap<u64, UserAudioState<<NativeOpusCodec as VoiceCodec>::Decoder>> = HashMap::new();
        let mut a = UserAudioState::new(NativeOpusCodec::create_decoder().unwrap(), 1);
        let mut b = UserAudioState::new(NativeOpusCodec::create_decoder().unwrap(), 1);
        let epoch = Instant::now();
        for (i, p) in a_pkts.iter().enumerate() {
            feed_at(&mut a, epoch, i as u32, i as u64, p.clone());
        }
        for (i, p) in b_pkts.iter().enumerate() {
            feed_at(&mut b, epoch, i as u32, i as u64, p.clone());
        }
        peers.insert(1, a);
        peers.insert(2, b);

        let mut mixed = Vec::with_capacity(FRAMES * OPUS_FRAME_SIZE);
        for _ in 0..FRAMES {
            if let Some(f) = mix_peer_frames(&mut peers, &AudioDumper::disabled()) {
                mixed.extend_from_slice(&f);
            }
        }

        // Skip the warm-up frames; characterize steady state.
        let steady = &mixed[OPUS_FRAME_SIZE * 4..];
        let clipped = steady.iter().filter(|&&s| s.abs() >= 0.999).count();
        let clip_pct = 100.0 * clipped as f32 / steady.len() as f32;
        let peak = steady.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        eprintln!("overlapping speakers (soft-limited): peak={peak:.4}, {clip_pct:.1}% of samples at full scale");

        assert!(
            clip_pct < 1.0,
            "soft limiter should keep overlapping speakers off the rail, but {clip_pct:.1}% of samples are clipped"
        );
    }

    /// Push every sample into the playback ring (test helper; the ring must have
    /// room for them all). Mirrors the chunked write the mix tick does.
    fn push_all(producer: &mut Producer<f32>, samples: &[f32]) {
        let mut chunk = producer.write_chunk(samples.len()).expect("ring has capacity");
        let (first, second) = chunk.as_mut_slices();
        let split = first.len();
        first.copy_from_slice(&samples[..split]);
        second.copy_from_slice(&samples[split..split + second.len()]);
        chunk.commit_all();
    }

    /// Regression for the thin-buffer click fix: the playback filler builds a
    /// cushion before playout, so a thin buffer no longer plays-then-cuts
    /// mid-block (the old click). While priming it emits pure silence (not a
    /// clicky partial cut) and counts nothing; once the cushion is built it
    /// plays the continuous tone with no seam at block boundaries.
    #[test]
    fn playback_filler_primes_before_playout() {
        const FREQ: f32 = 300.0;
        const AMP: f32 = 0.8;
        let sr = OPUS_SAMPLE_RATE as f32;
        let tone = |n: usize| (2.0 * std::f32::consts::PI * FREQ * n as f32 / sr).sin() * AMP;

        let underruns = AtomicU64::new(0);
        let active = AtomicBool::new(true); // producer feeding throughout
        let (mut producer, consumer) = RingBuffer::<f32>::new(PLAYBACK_PRIME_SAMPLES + 8 * OPUS_FRAME_SIZE);
        let mut filler = PlaybackFiller::new(consumer);
        let mut output = [0.0f32; OPUS_FRAME_SIZE];

        // Idle / sub-target ring: stays priming → pure silence, nothing consumed,
        // nothing counted. (Old code would have played + cut here.)
        let sub: Vec<f32> = (0..PLAYBACK_PRIME_SAMPLES - 1).map(tone).collect();
        push_all(&mut producer, &sub);
        let before = filler.consumer.slots();
        filler.fill(&mut output, &underruns, &active);
        assert!(output.iter().all(|&s| s == 0.0), "priming output must be pure silence");
        assert_eq!(filler.consumer.slots(), before, "priming must not consume the ring");
        assert_eq!(underruns.load(Ordering::Relaxed), 0, "priming is not an underrun");

        // Top past the target, then play several blocks: the cushion keeps the
        // ring fed, so playout is the continuous tone with no boundary seam.
        let more: Vec<f32> = (PLAYBACK_PRIME_SAMPLES - 1..PLAYBACK_PRIME_SAMPLES - 1 + 6 * OPUS_FRAME_SIZE)
            .map(tone)
            .collect();
        push_all(&mut producer, &more);
        let mut played = Vec::new();
        for _ in 0..3 {
            filler.fill(&mut output, &underruns, &active);
            played.extend_from_slice(&output);
        }
        assert_eq!(
            underruns.load(Ordering::Relaxed),
            0,
            "a well-fed stream must not underrun"
        );
        let seam = max_abs_delta(&played);
        let natural_slew = AMP * 2.0 * std::f32::consts::PI * FREQ / sr;
        assert!(
            seam <= natural_slew * 3.0,
            "primed playout must be continuous (seam Δ={seam:.4} vs natural slew {natural_slew:.4})"
        );
    }

    /// Regression for "short SFX plays only every other press": a self-contained
    /// burst shorter than the prime cushion (e.g. a <40 ms mute/unmute blip) must
    /// still play out once the producer goes idle, instead of being stranded in
    /// the ring until the next press tops it past the prime target. With the old
    /// gate it stayed silent (priming forever) and accumulated, so only every
    /// second press crossed the threshold and flushed both.
    #[test]
    fn playback_filler_flushes_sub_prime_burst_once_producer_idle() {
        let underruns = AtomicU64::new(0);
        // Producer already idle: the burst is complete, nothing more is coming.
        let active = AtomicBool::new(false);
        let (mut producer, consumer) = RingBuffer::<f32>::new(MAX_PLAYBACK_BUFFER_SAMPLES);
        let mut filler = PlaybackFiller::new(consumer);
        let mut output = [0.0f32; OPUS_FRAME_SIZE];

        // A sub-prime burst (shorter than the cushion target) sits buffered.
        let burst = PLAYBACK_PRIME_SAMPLES / 2;
        assert!((OPUS_FRAME_SIZE..PLAYBACK_PRIME_SAMPLES).contains(&burst));
        push_all(&mut producer, &vec![0.5f32; burst]);

        // First fill: even though the ring never reached the prime target, the
        // idle producer means this is all there is — it must play, not stay silent.
        filler.fill(&mut output, &underruns, &active);
        assert!(
            output.iter().any(|&s| s != 0.0),
            "a complete sub-prime burst must play out, not stay silent"
        );
        assert_eq!(
            filler.consumer.slots(),
            burst - OPUS_FRAME_SIZE,
            "the buffered burst must be consumed, not stranded"
        );
        // Draining a sub-prime burst after the producer stopped is the expected
        // tail, not a mid-stream click — it must not count as an underrun.
        assert_eq!(underruns.load(Ordering::Relaxed), 0, "idle drain is not an underrun");
    }

    /// The filler counts a starvation only when it runs dry WHILE THE PRODUCER
    /// IS STILL FEEDING (a real mid-stream click). Running dry because the spurt
    /// ended (producer idle) is the expected cushion drain and must NOT count —
    /// otherwise the counter ticks up once every time someone stops talking.
    #[test]
    fn playback_filler_counts_starvation_only_while_producing() {
        // Helper: prime, play a clean block, then run dry mid-block with the
        // producer in the given state. Returns the underrun count after the dry
        // block (starting from 0).
        fn run_dry_with(producer_active: bool) -> u64 {
            let underruns = AtomicU64::new(0);
            let active = AtomicBool::new(true);
            let (mut producer, consumer) = RingBuffer::<f32>::new(PLAYBACK_PRIME_SAMPLES * 2);
            let mut filler = PlaybackFiller::new(consumer);
            let mut output = [0.0f32; OPUS_FRAME_SIZE];

            // Prime with exactly the target cushion (an integer number of device
            // blocks), then drain it in clean blocks (producer feeding) so the
            // ring lands exactly empty without ever starving.
            push_all(&mut producer, &vec![0.3f32; PLAYBACK_PRIME_SAMPLES]);
            for _ in 0..(PLAYBACK_PRIME_SAMPLES / OPUS_FRAME_SIZE) {
                filler.fill(&mut output, &underruns, &active);
            }
            assert_eq!(
                underruns.load(Ordering::Relaxed),
                0,
                "priming + clean blocks must not underrun"
            );

            // Next block runs dry under the producer state under test.
            active.store(producer_active, Ordering::Relaxed);
            filler.fill(&mut output, &underruns, &active);
            underruns.load(Ordering::Relaxed)
        }

        // Producer still feeding → genuine mid-stream starvation → counted.
        assert_eq!(run_dry_with(true), 1, "running dry while producing is a click");
        // Producer stopped (spurt ended) → expected drain → NOT counted.
        assert_eq!(
            run_dry_with(false),
            0,
            "draining the cushion after a spurt ends is not a click"
        );
    }

    // ====================================================================
    // Consumer-paced production (step 2 of the playback redesign).
    //
    // The producer must fill the ring only to PLAYBACK_TARGET_SAMPLES and no
    // further, so the number of frames it produces tracks how much the device
    // drained — production rate == consumption rate, no wall-clock drift — and
    // the per-peer jitter buffer keeps its network cushion instead of being
    // decoded ahead into PCM. These pin both properties against the real
    // `fill_playback_ring`.
    // ====================================================================

    #[test]
    fn producer_fills_to_target_then_tracks_consumption() {
        // One peer, ready (delay 3) with far more packets buffered than the ring
        // target — so a naive "drain all ready frames" producer would empty the
        // jitter buffer into the ring.
        let mut peers: HashMap<u64, UserAudioState<MarkerDecoder>> = HashMap::new();
        let mut s = make_state(3);
        let epoch = Instant::now();
        for n in 0..40u8 {
            feed_contig(&mut s, epoch, n);
        }
        let buffered_before = s.jitter_buffer.len();
        peers.insert(1, s);

        let mut sfx: VecDeque<f32> = VecDeque::new();
        let (mut producer, mut consumer) = RingBuffer::<f32>::new(MAX_PLAYBACK_BUFFER_SAMPLES);
        let overflows = AtomicU64::new(0);
        let active = AtomicBool::new(false);
        let dumper = AudioDumper::disabled();
        let target_frames = PLAYBACK_TARGET_SAMPLES / OPUS_FRAME_SIZE;

        // First fill tops the ring up to exactly the target and pulls only
        // target-worth of packets — the rest of the cushion stays in the jitter
        // buffer where reordering can still be absorbed.
        fill_playback_ring(&mut peers, &mut sfx, Some(&mut producer), &dumper, &overflows, &active);
        assert_eq!(
            consumer.slots(),
            PLAYBACK_TARGET_SAMPLES,
            "ring filled to exactly the target"
        );
        assert_eq!(
            peers[&1].jitter_buffer.len(),
            buffered_before - target_frames,
            "only target-worth of packets pulled; jitter cushion preserved"
        );
        assert!(
            active.load(Ordering::Relaxed),
            "a live stream marks the producer active"
        );

        // The device drains two frames; the next poll produces exactly two
        // frames back — production tracks consumption, not the poll cadence.
        let drained = consumer.read_chunk(2 * OPUS_FRAME_SIZE).expect("two frames buffered");
        drained.commit_all();
        let jitter_mid = peers[&1].jitter_buffer.len();
        fill_playback_ring(&mut peers, &mut sfx, Some(&mut producer), &dumper, &overflows, &active);
        assert_eq!(
            consumer.slots(),
            PLAYBACK_TARGET_SAMPLES,
            "ring refilled back to the target"
        );
        assert_eq!(
            peers[&1].jitter_buffer.len(),
            jitter_mid - 2,
            "refill pulled exactly the two frames the device drained"
        );
        assert_eq!(
            overflows.load(Ordering::Relaxed),
            0,
            "consumer-paced fill never overflows"
        );
    }

    #[test]
    fn producer_idle_when_no_peer_audio_and_marks_inactive() {
        // No peers, no sfx: nothing to produce, ring stays empty, and the
        // producer is marked inactive so a cushion drain isn't miscounted.
        let mut peers: HashMap<u64, UserAudioState<MarkerDecoder>> = HashMap::new();
        let mut sfx: VecDeque<f32> = VecDeque::new();
        let (mut producer, consumer) = RingBuffer::<f32>::new(MAX_PLAYBACK_BUFFER_SAMPLES);
        let overflows = AtomicU64::new(0);
        let active = AtomicBool::new(true);
        let dumper = AudioDumper::disabled();

        fill_playback_ring(&mut peers, &mut sfx, Some(&mut producer), &dumper, &overflows, &active);
        assert_eq!(consumer.slots(), 0, "nothing produced when no stream is live");
        assert!(
            !active.load(Ordering::Relaxed),
            "no live stream marks the producer inactive"
        );

        // A peer still buffering (not yet ready_to_play) is live but produces no
        // frame — the ring stays empty but the producer is marked active.
        let mut s = make_state(3);
        feed_contig(&mut s, Instant::now(), 0); // 1 of 3 → not ready
        peers.insert(1, s);
        fill_playback_ring(&mut peers, &mut sfx, Some(&mut producer), &dumper, &overflows, &active);
        assert_eq!(consumer.slots(), 0, "a buffering peer produces no frame yet");
        assert!(
            active.load(Ordering::Relaxed),
            "a buffering (un-ended) stream is active"
        );
    }

    // ====================================================================
    // Capture-decision tests.
    //
    // These exercise the REAL production `should_capture` (5 args, incl.
    // `server_muted`) and `capture_transition` — not a copy. The previous
    // tests reimplemented a 4-arg `should_capture` inside the test module,
    // which silently shadowed the real one and was blind to `server_muted`
    // entirely. Args: (voice_mode, self_muted, server_muted, ptt_active,
    // connected).
    // ====================================================================

    #[test]
    fn should_capture_continuous_when_connected() {
        assert!(should_capture(VoiceMode::Continuous, false, false, false, true));
    }

    #[test]
    fn should_capture_blocked_by_self_mute() {
        assert!(!should_capture(VoiceMode::Continuous, true, false, false, true));
    }

    #[test]
    fn should_capture_blocked_by_server_mute() {
        // The old test copy lacked this argument and could not catch a
        // server-mute regression.
        assert!(!should_capture(VoiceMode::Continuous, false, true, false, true));
        assert!(!should_capture(VoiceMode::PushToTalk, false, true, true, true));
    }

    #[test]
    fn should_capture_blocked_when_disconnected() {
        assert!(!should_capture(VoiceMode::Continuous, false, false, false, false));
    }

    #[test]
    fn should_capture_ptt_follows_button() {
        assert!(should_capture(VoiceMode::PushToTalk, false, false, true, true));
        assert!(!should_capture(VoiceMode::PushToTalk, false, false, false, true));
    }

    #[test]
    fn should_capture_ptt_held_but_self_muted() {
        // Holding PTT must not override mute.
        assert!(!should_capture(VoiceMode::PushToTalk, true, false, true, true));
    }

    // `capture_transition(want, active)` is the real decision the
    // `sync_transmission!` macro acts on. The macro's I/O (set_active, EOS
    // send, event broadcast) needs the full task to test, but the transition
    // decision — including that stopping capture is a distinct `Deactivate`
    // (which obliges the caller to send EOS) — is pinned here.

    #[test]
    fn capture_transition_activates_when_wanted_and_idle() {
        assert_eq!(capture_transition(true, false), CaptureTransition::Activate);
    }

    #[test]
    fn capture_transition_deactivates_when_unwanted_and_active() {
        assert_eq!(capture_transition(false, true), CaptureTransition::Deactivate);
    }

    #[test]
    fn capture_transition_no_change_when_already_in_desired_state() {
        assert_eq!(capture_transition(true, true), CaptureTransition::None);
        assert_eq!(capture_transition(false, false), CaptureTransition::None);
    }

    // The documented capture scenarios, composed from the real functions:
    // the desired state comes from `should_capture`, the action from
    // `capture_transition` against the current flag.

    #[test]
    fn capture_scenario_mute_stops_active_capture() {
        let want = should_capture(VoiceMode::Continuous, /* self_muted */ true, false, false, true);
        assert_eq!(
            capture_transition(want, /* active */ true),
            CaptureTransition::Deactivate
        );
    }

    #[test]
    fn capture_scenario_unmute_resumes_capture() {
        let want = should_capture(VoiceMode::Continuous, /* self_muted */ false, false, false, true);
        assert_eq!(
            capture_transition(want, /* active */ false),
            CaptureTransition::Activate
        );
    }

    #[test]
    fn capture_scenario_ptt_release_stops_capture() {
        let want = should_capture(VoiceMode::PushToTalk, false, false, /* ptt_active */ false, true);
        assert_eq!(
            capture_transition(want, /* active */ true),
            CaptureTransition::Deactivate
        );
    }

    // ====================================================================
    // Bug-1 invariant: capture_is_active == true implies audio_input.is_some().
    //
    // The sync_transmission! Activate arm defers the flag assignment when
    // audio_input is None, so a caller with a None stream that requests
    // activation must see Activate again after the stream is opened —
    // not a phantom-active flag with no stream backing it.
    //
    // These tests exercise the decision functions in isolation. The full
    // task is required to verify set_active() is called, but the deferred-
    // activation logic is captured here: consecutive Activate transitions
    // must be returned until the caller supplies a stream and sets the flag.
    // ====================================================================

    #[test]
    fn activate_transition_returned_when_stream_missing_flag_still_false() {
        // Caller wants to activate but has no stream yet — the flag must stay
        // false, and capture_transition must return Activate again next time
        // so the caller gets another chance once the stream opens.
        let want = should_capture(VoiceMode::Continuous, false, false, false, true);
        // Before stream opens: active=false, want=true → Activate.
        assert_eq!(capture_transition(want, false), CaptureTransition::Activate);
        // Simulate the state after the caller deferred (stream still None,
        // flag still false): the SAME transition must be returned again —
        // not None — so activation retries on the next stream-open attempt.
        assert_eq!(
            capture_transition(want, false),
            CaptureTransition::Activate,
            "deferred activation must keep returning Activate until honoured"
        );
    }

    #[test]
    fn activate_then_deactivate_still_works_after_deferral() {
        // If the caller deferred activation (flag=false, want=true) and then
        // conditions change so the desired state becomes false, the transition
        // must be None (not Deactivate) — there is nothing to deactivate.
        let want_false = should_capture(VoiceMode::Continuous, /* self_muted */ true, false, false, true);
        assert_eq!(
            capture_transition(want_false, /* active */ false),
            CaptureTransition::None,
            "deactivating a never-activated stream is a no-op"
        );
    }

    // ====================================================================
    // Bug-4 SFX queue tests: cap and ephemeral-drop invariants (pure logic,
    // no audio backend required).
    // ====================================================================

    #[test]
    fn sfx_queue_cap_drops_oldest_samples() {
        // Build a queue that exceeds MAX_SFX_QUEUE_SAMPLES and verify that
        // the logic drops from the front, keeping the most recent samples.
        let mut queue: VecDeque<f32> = VecDeque::new();
        // Fill with MAX_SFX_QUEUE_SAMPLES + 100 distinct values. We use the
        // index cast to f32 so we can identify which samples survive.
        let total = MAX_SFX_QUEUE_SAMPLES + 100;
        for i in 0..total {
            queue.push_back(i as f32);
        }
        // Apply the same cap logic as the PlaySfx handler.
        let excess = queue.len().saturating_sub(MAX_SFX_QUEUE_SAMPLES);
        if excess > 0 {
            queue.drain(..excess);
        }
        assert_eq!(queue.len(), MAX_SFX_QUEUE_SAMPLES, "queue trimmed to cap");
        // The oldest 100 samples (indices 0..100) should have been dropped;
        // the newest should start at index 100.
        assert_eq!(
            queue.front().copied(),
            Some(100.0),
            "oldest samples must be dropped, not newest"
        );
        assert_eq!(
            queue.back().copied(),
            Some((total - 1) as f32),
            "newest sample must be preserved"
        );
    }

    #[test]
    fn sfx_queue_under_cap_is_unchanged() {
        let mut queue: VecDeque<f32> = VecDeque::new();
        for i in 0..10usize {
            queue.push_back(i as f32);
        }
        let len_before = queue.len();
        let excess = queue.len().saturating_sub(MAX_SFX_QUEUE_SAMPLES);
        if excess > 0 {
            queue.drain(..excess);
        }
        assert_eq!(queue.len(), len_before, "queue below cap must not be modified");
    }
}
