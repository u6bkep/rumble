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
    audio::{AudioConfig, AudioInput, AudioOutput, AudioSystem, FRAME_SIZE},
    codec::{VoiceDecoder, VoiceEncoder, OPUS_FRAME_SIZE},
    events::{State, TransmissionMode},
};
use api::proto::VoiceDatagram;
use bytes::Bytes;
use prost::Message;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

/// Commands sent to the audio task.
#[derive(Debug)]
pub enum AudioCommand {
    /// A QUIC connection was established - start datagram handling.
    ConnectionEstablished {
        connection: quinn::Connection,
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
    /// Set transmission mode.
    SetTransmissionMode { mode: TransmissionMode },
    /// Refresh audio devices.
    RefreshDevices,
    /// Shutdown the audio task.
    Shutdown,
}

/// Per-user audio state for playback with jitter buffer.
///
/// The jitter buffer stores incoming Opus packets by sequence number,
/// allowing reordering and providing a delay to absorb network jitter.
struct UserAudioState {
    /// Opus decoder for this user.
    decoder: VoiceDecoder,
    /// Jitter buffer: maps sequence number to Opus packet data.
    /// Uses BTreeMap to maintain ordering by sequence number.
    jitter_buffer: BTreeMap<u32, Vec<u8>>,
    /// Next sequence number we expect to play.
    next_play_seq: u32,
    /// Whether we've started playing (have received first packet).
    started: bool,
    /// Last time we received audio from this user.
    last_received: Instant,
    /// Number of packets received (for initial buffering).
    packets_received: u32,
}

/// Number of packets to buffer before starting playback.
/// At 20ms per frame, 3 packets = 60ms initial delay.
const JITTER_BUFFER_DELAY_PACKETS: u32 = 3;

/// Maximum jitter buffer size (drop old packets beyond this).
const JITTER_BUFFER_MAX_PACKETS: usize = 20;

impl UserAudioState {
    fn new() -> Result<Self, String> {
        Ok(Self {
            decoder: VoiceDecoder::new().map_err(|e| e.to_string())?,
            jitter_buffer: BTreeMap::new(),
            next_play_seq: 0,
            started: false,
            last_received: Instant::now(),
            packets_received: 0,
        })
    }

    /// Insert a packet into the jitter buffer.
    fn insert_packet(&mut self, sequence: u32, opus_data: Vec<u8>) {
        self.last_received = Instant::now();
        self.packets_received += 1;

        // If not started, set the initial sequence
        if !self.started && self.packets_received == 1 {
            self.next_play_seq = sequence;
        }

        // Insert into buffer
        self.jitter_buffer.insert(sequence, opus_data);

        // Limit buffer size - remove oldest packets if too large
        while self.jitter_buffer.len() > JITTER_BUFFER_MAX_PACKETS {
            if let Some(&oldest_seq) = self.jitter_buffer.keys().next() {
                self.jitter_buffer.remove(&oldest_seq);
                // If we removed a packet we haven't played yet, skip it
                if oldest_seq >= self.next_play_seq {
                    self.next_play_seq = oldest_seq.wrapping_add(1);
                }
            }
        }
    }

    /// Check if we have enough buffered to start playback.
    fn ready_to_play(&self) -> bool {
        self.packets_received >= JITTER_BUFFER_DELAY_PACKETS
    }

    /// Get the next frame to play, decoding from jitter buffer.
    /// Returns decoded PCM samples, using PLC if packet is missing.
    fn get_next_frame(&mut self) -> Option<Vec<f32>> {
        if !self.ready_to_play() {
            return None;
        }
        self.started = true;

        let seq = self.next_play_seq;
        self.next_play_seq = self.next_play_seq.wrapping_add(1);

        if let Some(opus_data) = self.jitter_buffer.remove(&seq) {
            // Packet present - decode it
            match self.decoder.decode(&opus_data) {
                Ok(pcm) => Some(pcm),
                Err(e) => {
                    trace!("Decode error for seq {}: {}", seq, e);
                    // Try PLC on decode error
                    self.decoder.conceal(OPUS_FRAME_SIZE).ok()
                }
            }
        } else {
            // Packet missing - use Opus PLC (Packet Loss Concealment)
            trace!("Missing packet seq {}, using PLC", seq);
            self.decoder.conceal(OPUS_FRAME_SIZE).ok()
        }
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
pub struct AudioTaskConfig {
    /// Shared state for updating talking_users.
    pub state: Arc<RwLock<State>>,
    /// Repaint callback for UI updates.
    pub repaint: Arc<dyn Fn() + Send + Sync>,
}

/// Spawn the audio task and return a handle for sending commands.
///
/// The audio task runs on a separate thread with its own tokio runtime
/// to avoid any blocking from audio I/O affecting other async tasks.
pub fn spawn_audio_task(config: AudioTaskConfig) -> AudioTaskHandle {
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create audio tokio runtime");

        rt.block_on(run_audio_task(command_rx, config));
    });

    AudioTaskHandle { command_tx }
}

/// Main audio task loop.
async fn run_audio_task(mut command_rx: mpsc::UnboundedReceiver<AudioCommand>, config: AudioTaskConfig) {
    let state = config.state;
    let repaint = config.repaint;

    // Audio system for device access (not Send, so lives on this thread)
    let audio_system = AudioSystem::new();

    // Current connection state
    let mut connection: Option<quinn::Connection> = None;
    let mut my_user_id: u64 = 0;

    // Transmission state
    let mut transmission_mode = TransmissionMode::PushToTalk;
    let mut ptt_active = false;

    // Audio I/O handles
    let mut audio_input: Option<AudioInput> = None;
    let mut audio_output: Option<AudioOutput> = None;

    // Opus encoder for outgoing audio
    let mut encoder: Option<VoiceEncoder> = None;

    // Per-user decoders and jitter buffers
    let mut user_audio: HashMap<u64, UserAudioState> = HashMap::new();

    // Selected devices
    let mut selected_input: Option<String> = None;
    let mut selected_output: Option<String> = None;

    // Channel for encoded audio frames to send
    let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel::<Bytes>();

    // Sequence number for outgoing voice packets
    let mut send_sequence: u32 = 0;

    // Interval for cleaning up stale talking_users
    let mut cleanup_interval = tokio::time::interval(Duration::from_millis(500));

    // Interval for mixing audio from all users' jitter buffers (every 20ms = one frame)
    let mut mix_interval = tokio::time::interval(Duration::from_millis(20));

    info!("Audio task started");

    loop {
        tokio::select! {
            // Handle commands
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    AudioCommand::ConnectionEstablished { connection: conn, my_user_id: uid } => {
                        info!("Audio task: connection established, user_id={}", uid);
                        connection = Some(conn);
                        my_user_id = uid;

                        // Start audio output for receiving
                        if audio_output.is_none() {
                            audio_output = start_audio_output(&audio_system, &selected_output, &mut user_audio);
                        }

                        // If in continuous mode, start transmitting
                        if transmission_mode == TransmissionMode::Continuous {
                            start_transmission(&audio_system, &selected_input, &encoded_tx, &mut audio_input, &mut encoder, &state, &repaint);
                        }
                    }

                    AudioCommand::ConnectionClosed => {
                        info!("Audio task: connection closed");
                        connection = None;
                        my_user_id = 0;

                        // Stop transmission
                        stop_transmission(&mut audio_input, &mut encoder, &state, &repaint);

                        // Clear talking users
                        {
                            let mut s = state.write().unwrap();
                            s.audio.talking_users.clear();
                        }
                        repaint();

                        // Clear per-user state
                        user_audio.clear();
                    }

                    AudioCommand::SetInputDevice { device_id } => {
                        selected_input = device_id.clone();
                        {
                            let mut s = state.write().unwrap();
                            s.audio.selected_input = device_id;
                        }
                        repaint();

                        // Restart input if currently transmitting
                        if audio_input.is_some() {
                            stop_transmission(&mut audio_input, &mut encoder, &state, &repaint);
                            start_transmission(&audio_system, &selected_input, &encoded_tx, &mut audio_input, &mut encoder, &state, &repaint);
                        }
                    }

                    AudioCommand::SetOutputDevice { device_id } => {
                        selected_output = device_id.clone();
                        {
                            let mut s = state.write().unwrap();
                            s.audio.selected_output = device_id;
                        }
                        repaint();

                        // Restart output
                        audio_output = None;
                        if connection.is_some() {
                            audio_output = start_audio_output(&audio_system, &selected_output, &mut user_audio);
                        }
                    }

                    AudioCommand::StartTransmit => {
                        if transmission_mode == TransmissionMode::PushToTalk && !ptt_active {
                            ptt_active = true;
                            if connection.is_some() {
                                start_transmission(&audio_system, &selected_input, &encoded_tx, &mut audio_input, &mut encoder, &state, &repaint);
                            }
                        }
                    }

                    AudioCommand::StopTransmit => {
                        if transmission_mode == TransmissionMode::PushToTalk && ptt_active {
                            ptt_active = false;
                            stop_transmission(&mut audio_input, &mut encoder, &state, &repaint);
                        }
                    }

                    AudioCommand::SetTransmissionMode { mode } => {
                        let old_mode = transmission_mode;
                        transmission_mode = mode;

                        {
                            let mut s = state.write().unwrap();
                            s.audio.transmission_mode = mode;
                        }
                        repaint();

                        // Handle mode transitions
                        if connection.is_some() {
                            match (old_mode, mode) {
                                // Switching to Continuous: start transmitting
                                (_, TransmissionMode::Continuous) => {
                                    start_transmission(&audio_system, &selected_input, &encoded_tx, &mut audio_input, &mut encoder, &state, &repaint);
                                }
                                // Switching to Muted: stop transmitting
                                (_, TransmissionMode::Muted) => {
                                    stop_transmission(&mut audio_input, &mut encoder, &state, &repaint);
                                }
                                // Switching to PTT: stop unless PTT is pressed
                                (_, TransmissionMode::PushToTalk) => {
                                    if !ptt_active {
                                        stop_transmission(&mut audio_input, &mut encoder, &state, &repaint);
                                    }
                                }
                            }
                        }
                    }

                    AudioCommand::RefreshDevices => {
                        let input_devices = audio_system.list_input_devices();
                        let output_devices = audio_system.list_output_devices();
                        {
                            let mut s = state.write().unwrap();
                            s.audio.input_devices = input_devices;
                            s.audio.output_devices = output_devices;
                        }
                        repaint();
                    }

                    AudioCommand::Shutdown => {
                        info!("Audio task shutting down");
                        break;
                    }
                }
            }

            // Send encoded audio as datagrams
            Some(encoded_data) = encoded_rx.recv() => {
                if let Some(conn) = &connection {
                    let datagram = VoiceDatagram {
                        sender_id: Some(my_user_id),
                        room_id: None, // TODO: set room ID
                        sequence: send_sequence,
                        timestamp_us: 0, // TODO: track timestamp
                        opus_data: encoded_data.to_vec(),
                    };
                    send_sequence = send_sequence.wrapping_add(1);
                    let datagram_bytes = datagram.encode_to_vec();

                    if let Err(e) = conn.send_datagram(Bytes::from(datagram_bytes)) {
                        warn!("Failed to send voice datagram: {}", e);
                        // Connection might be closed - will be detected by read_datagram
                    }
                }
            }

            // Receive voice datagrams
            datagram = async {
                if let Some(conn) = &connection {
                    conn.read_datagram().await
                } else {
                    // No connection, just wait
                    std::future::pending().await
                }
            } => {
                match datagram {
                    Ok(data) => {
                        if let Ok(voice) = VoiceDatagram::decode(data.as_ref()) {
                            if let Some(sender_id) = voice.sender_id {
                                // Don't play back our own audio
                                if sender_id != my_user_id {
                                    handle_voice_datagram(sender_id, voice.sequence, voice.opus_data, &mut user_audio, &state, &repaint);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Connection error - the connection task will handle cleanup
                        debug!("Datagram read error: {}", e);
                    }
                }
            }

            // Mix audio from all users' jitter buffers every 20ms
            _ = mix_interval.tick() => {
                mix_and_play_audio(&mut user_audio, &audio_output);
            }

            // Periodic cleanup of stale talking_users
            _ = cleanup_interval.tick() => {
                cleanup_stale_users(&mut user_audio, &state, &repaint);
            }
        }
    }
}

/// Start audio transmission (input capture + encoding).
fn start_transmission(
    audio_system: &AudioSystem,
    selected_input: &Option<String>,
    encoded_tx: &mpsc::UnboundedSender<Bytes>,
    audio_input: &mut Option<AudioInput>,
    encoder: &mut Option<VoiceEncoder>,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    if audio_input.is_some() {
        return; // Already transmitting
    }

    // Create encoder
    let enc = match VoiceEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to create Opus encoder: {}", e);
            return;
        }
    };
    *encoder = Some(enc);

    // Get input device
    let device = match selected_input {
        Some(id) => audio_system.get_input_device_by_id(id),
        None => audio_system.default_input_device(),
    };

    let device = match device {
        Some(d) => d,
        None => {
            error!("No input device available");
            return;
        }
    };

    // Create audio input with encoding callback
    let encoded_tx = encoded_tx.clone();
    let encoder_mutex = std::sync::Arc::new(std::sync::Mutex::new(
        VoiceEncoder::new().expect("create encoder"),
    ));

    let input = AudioInput::new(&device, &AudioConfig::default(), move |samples| {
        // Encode the audio frame
        if let Ok(mut enc) = encoder_mutex.lock() {
            match enc.encode(samples) {
                Ok(encoded) => {
                    let _ = encoded_tx.send(Bytes::from(encoded));
                }
                Err(e) => {
                    trace!("Encode error: {}", e);
                }
            }
        }
    });

    match input {
        Ok(input) => {
            *audio_input = Some(input);
            {
                let mut s = state.write().unwrap();
                s.audio.is_transmitting = true;
            }
            repaint();
            info!("Started audio transmission");
        }
        Err(e) => {
            error!("Failed to start audio input: {}", e);
        }
    }
}

/// Stop audio transmission.
fn stop_transmission(
    audio_input: &mut Option<AudioInput>,
    encoder: &mut Option<VoiceEncoder>,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    if audio_input.is_none() {
        return; // Not transmitting
    }

    *audio_input = None;
    *encoder = None;

    {
        let mut s = state.write().unwrap();
        s.audio.is_transmitting = false;
    }
    repaint();
    info!("Stopped audio transmission");
}

/// Start audio output for playback.
fn start_audio_output(
    audio_system: &AudioSystem,
    selected_output: &Option<String>,
    user_audio: &mut HashMap<u64, UserAudioState>,
) -> Option<AudioOutput> {
    let device = match selected_output {
        Some(id) => audio_system.get_output_device_by_id(id),
        None => audio_system.default_output_device(),
    };

    let device = match device {
        Some(d) => d,
        None => {
            error!("No output device available");
            return None;
        }
    };

    // Clear any existing user audio state
    user_audio.clear();

    // Create and return the audio output
    match AudioOutput::new(&device, &AudioConfig::default()) {
        Ok(output) => {
            info!("Audio output started");
            Some(output)
        }
        Err(e) => {
            error!("Failed to create audio output: {}", e);
            None
        }
    }
}

/// Handle received voice datagram - insert into jitter buffer.
fn handle_voice_datagram(
    sender_id: u64,
    sequence: u32,
    opus_data: Vec<u8>,
    user_audio: &mut HashMap<u64, UserAudioState>,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    // Get or create per-user audio state
    let user_state = user_audio.entry(sender_id).or_insert_with(|| {
        UserAudioState::new().expect("create user audio state")
    });

    // Insert packet into jitter buffer
    user_state.insert_packet(sequence, opus_data);
    user_state.last_received = Instant::now();

    // Update talking_users
    let mut needs_repaint = false;
    {
        let mut s = state.write().unwrap();
        if !s.audio.talking_users.contains(&sender_id) {
            s.audio.talking_users.insert(sender_id);
            needs_repaint = true;
        }
    }
    if needs_repaint {
        repaint();
    }
}

/// Mix audio from all users' jitter buffers and queue for playback.
fn mix_and_play_audio(
    user_audio: &mut HashMap<u64, UserAudioState>,
    audio_output: &Option<AudioOutput>,
) {
    let output = match audio_output {
        Some(o) => o,
        None => return,
    };

    // Collect decoded frames from all users who are ready
    let mut mixed_buffer = [0.0f32; FRAME_SIZE];
    let mut has_audio = false;

    for user_state in user_audio.values_mut() {
        // Skip users who haven't buffered enough yet
        if !user_state.ready_to_play() {
            continue;
        }

        // Get next frame (decoded or PLC)
        if let Some(pcm) = user_state.get_next_frame() {
            has_audio = true;
            // Mix by summing with clamping to [-1.0, 1.0]
            for (i, &sample) in pcm.iter().enumerate() {
                if i < FRAME_SIZE {
                    mixed_buffer[i] = (mixed_buffer[i] + sample).clamp(-1.0, 1.0);
                }
            }
        }
    }

    // Queue mixed audio if we have any
    if has_audio {
        output.queue_samples(&mixed_buffer);
    }
}

/// Clean up users who haven't sent audio recently.
fn cleanup_stale_users(
    user_audio: &mut HashMap<u64, UserAudioState>,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    let stale_threshold = Duration::from_millis(300);
    let now = Instant::now();

    let stale_users: Vec<u64> = user_audio
        .iter()
        .filter(|(_, audio)| now.duration_since(audio.last_received) > stale_threshold)
        .map(|(&id, _)| id)
        .collect();

    if stale_users.is_empty() {
        return;
    }

    // Remove from user_audio
    for user_id in &stale_users {
        user_audio.remove(user_id);
    }

    // Remove from talking_users
    let mut needs_repaint = false;
    {
        let mut s = state.write().unwrap();
        for user_id in &stale_users {
            if s.audio.talking_users.remove(user_id) {
                needs_repaint = true;
            }
        }
    }

    if needs_repaint {
        repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_command_send() {
        // Just verify the types work
        let (tx, _rx) = mpsc::unbounded_channel::<AudioCommand>();
        let handle = AudioTaskHandle { command_tx: tx };
        handle.send(AudioCommand::RefreshDevices);
        handle.send(AudioCommand::StartTransmit);
        handle.send(AudioCommand::StopTransmit);
    }

    #[test]
    fn test_user_audio_state_creation() {
        let state = UserAudioState::new();
        assert!(state.is_ok());
    }
}
