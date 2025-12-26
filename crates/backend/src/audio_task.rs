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
    codec::{EncoderSettings, VoiceDecoder, VoiceEncoder, OPUS_FRAME_SIZE},
    events::{AudioSettings, AudioStats, State, VoiceMode},
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
    /// Set voice activation mode (PTT vs Continuous).
    SetVoiceMode { mode: VoiceMode },
    /// Set self-muted state.
    SetMuted { muted: bool },
    /// Set self-deafened state.
    SetDeafened { deafened: bool },
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
    /// Configurable delay in packets before starting playback.
    jitter_buffer_delay: u32,
    /// Statistics: packets lost (detected via sequence gaps).
    packets_lost: u64,
    /// Statistics: packets recovered via FEC.
    packets_recovered_fec: u64,
    /// Statistics: frames concealed via PLC.
    frames_concealed: u64,
    /// Statistics: bytes received.
    bytes_received: u64,
}

/// Maximum jitter buffer size (drop old packets beyond this).
const JITTER_BUFFER_MAX_PACKETS: usize = 20;

/// An encoded audio frame with metadata for statistics tracking.
struct EncodedFrame {
    data: Bytes,
    size_bytes: usize,
}

impl UserAudioState {
    fn new(jitter_buffer_delay: u32) -> Result<Self, String> {
        Ok(Self {
            decoder: VoiceDecoder::new().map_err(|e| e.to_string())?,
            jitter_buffer: BTreeMap::new(),
            next_play_seq: 0,
            started: false,
            last_received: Instant::now(),
            packets_received: 0,
            jitter_buffer_delay,
            packets_lost: 0,
            packets_recovered_fec: 0,
            frames_concealed: 0,
            bytes_received: 0,
        })
    }

    /// Insert a packet into the jitter buffer.
    fn insert_packet(&mut self, sequence: u32, opus_data: Vec<u8>) {
        self.last_received = Instant::now();
        self.packets_received += 1;
        self.bytes_received += opus_data.len() as u64;

        // If not started, set the initial sequence
        if !self.started && self.packets_received == 1 {
            self.next_play_seq = sequence;
        }

        // Detect sender restart: if the incoming sequence is earlier than next_play_seq,
        // the sender has restarted transmission. Reset our state.
        // We use wrapping subtraction to handle wrapping correctly.
        // If behind_by is less than half the sequence space, the packet is truly behind.
        // If behind_by is greater than half, it means the packet is actually ahead (wrapped).
        let behind_by = self.next_play_seq.wrapping_sub(sequence);
        const HALF_SEQ_SPACE: u32 = u32::MAX / 2;

        if self.started && behind_by > 0 && behind_by < HALF_SEQ_SPACE {
            // Sender has restarted - reset decoder and jitter buffer state
            debug!(
                "Detected sender restart: received seq {} but expected around {}, resetting",
                sequence, self.next_play_seq
            );
            self.jitter_buffer.clear();
            self.next_play_seq = sequence;
            self.started = false;
            // Reset decoder to clear any internal state
            if let Ok(new_decoder) = VoiceDecoder::new() {
                self.decoder = new_decoder;
            }
        }

        // Insert into buffer
        self.jitter_buffer.insert(sequence, opus_data);

        // Limit buffer size - remove oldest packets if too large
        while self.jitter_buffer.len() > JITTER_BUFFER_MAX_PACKETS {
            if let Some((oldest_seq, _)) = self.jitter_buffer.pop_first() {
                // If we removed a packet we haven't played yet, skip it
                if oldest_seq >= self.next_play_seq {
                    self.next_play_seq = oldest_seq.wrapping_add(1);
                }
            }
        }
    }

    /// Check if we have enough buffered to start playback.
    fn ready_to_play(&self) -> bool {
        self.packets_received >= self.jitter_buffer_delay
    }

    /// Get the next frame to play, decoding from jitter buffer.
    /// Returns decoded PCM samples, using FEC recovery or PLC if packet is missing.
    ///
    /// FEC (Forward Error Correction) recovery works by using data embedded in the
    /// *next* packet to reconstruct a lost packet. This provides better quality than
    /// pure PLC (Packet Loss Concealment) which just interpolates/generates comfort noise.
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
                    self.frames_concealed += 1;
                    self.decoder.conceal(OPUS_FRAME_SIZE).ok()
                }
            }
        } else {
            // Packet missing - track as lost
            self.packets_lost += 1;
            
            // Try FEC recovery using the next packet if available
            let next_seq = seq.wrapping_add(1);
            if let Some(next_opus_data) = self.jitter_buffer.get(&next_seq) {
                // We have the next packet - use its FEC data to recover this frame
                trace!("Missing packet seq {}, recovering with FEC from seq {}", seq, next_seq);
                match self.decoder.decode_fec(next_opus_data) {
                    Ok(pcm) => {
                        self.packets_recovered_fec += 1;
                        Some(pcm)
                    }
                    Err(e) => {
                        trace!("FEC recovery failed for seq {}: {}, falling back to PLC", seq, e);
                        self.frames_concealed += 1;
                        self.decoder.conceal(OPUS_FRAME_SIZE).ok()
                    }
                }
            } else {
                // No next packet available - fall back to pure PLC
                trace!("Missing packet seq {}, no FEC available, using PLC", seq);
                self.frames_concealed += 1;
                self.decoder.conceal(OPUS_FRAME_SIZE).ok()
            }
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

    // Voice mode and mute state (orthogonal controls)
    let mut voice_mode = VoiceMode::PushToTalk;
    let mut self_muted = false;
    let mut self_deafened = false;
    let mut ptt_active = false;
    
    // Per-user local mutes
    let mut muted_users: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // Audio I/O handles
    let mut audio_input: Option<AudioInput> = None;
    let mut audio_output: Option<AudioOutput> = None;

    // Per-user decoders and jitter buffers
    let mut user_audio: HashMap<u64, UserAudioState> = HashMap::new();

    // Selected devices
    let mut selected_input: Option<String> = None;
    let mut selected_output: Option<String> = None;
    
    // Current audio settings (start with defaults from state)
    let mut audio_settings = {
        let s = state.read().unwrap();
        s.audio.settings.clone()
    };

    // Channel for encoded audio frames to send
    let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel::<EncodedFrame>();

    // Sequence number for outgoing voice packets
    let mut send_sequence: u32 = 0;
    
    // Statistics tracking
    let mut packets_sent: u64 = 0;
    let mut bytes_sent: u64 = 0;
    
    // =========================================================================
    // Transmission State Machine
    // =========================================================================
    // 
    // Instead of scattered logic, we use a declarative approach:
    // 1. should_transmit() - pure function that determines desired state
    // 2. sync_transmission_state() - ensures actual state matches desired state
    //
    // Every handler that changes relevant state just calls sync_transmission_state()
    // after updating its piece, eliminating bugs from inconsistent logic.
    
    /// Determine if we should be transmitting based on current state.
    /// This is the SINGLE SOURCE OF TRUTH for transmission decisions.
    #[inline]
    fn should_transmit(
        voice_mode: VoiceMode,
        self_muted: bool,
        ptt_active: bool,
        connected: bool,
    ) -> bool {
        if !connected || self_muted {
            return false;
        }
        match voice_mode {
            VoiceMode::Continuous => true,
            VoiceMode::PushToTalk => ptt_active,
        }
    }

    // Interval for cleaning up stale talking_users
    let mut cleanup_interval = tokio::time::interval(Duration::from_millis(500));

    // Interval for mixing audio from all users' jitter buffers (every 20ms = one frame)
    let mut mix_interval = tokio::time::interval(Duration::from_millis(20));
    
    // Interval for updating statistics in state
    let mut stats_interval = tokio::time::interval(Duration::from_millis(500));

    info!("Audio task started");
    
    /// Macro to sync transmission state after any state change.
    /// This ensures transmission is started/stopped to match the desired state.
    macro_rules! sync_transmission {
        () => {{
            let want = should_transmit(voice_mode, self_muted, ptt_active, connection.is_some());
            let have = audio_input.is_some();
            
            if want && !have {
                start_transmission(&audio_system, &selected_input, &encoded_tx, &audio_settings, &mut audio_input, &state, &repaint);
            } else if !want && have {
                stop_transmission(&mut audio_input, &state, &repaint);
            }
        }};
    }

    loop {
        tokio::select! {
            // Handle commands
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    AudioCommand::ConnectionEstablished { connection: conn, my_user_id: uid } => {
                        info!("Audio task: connection established, user_id={}", uid);
                        connection = Some(conn);
                        my_user_id = uid;

                        // Start audio output for receiving (unless deafened)
                        if audio_output.is_none() && !self_deafened {
                            audio_output = start_audio_output(&audio_system, &selected_output, &mut user_audio);
                        }

                        // Sync transmission state
                        sync_transmission!();
                    }

                    AudioCommand::ConnectionClosed => {
                        info!("Audio task: connection closed");
                        connection = None;
                        my_user_id = 0;

                        // Stop transmission (sync will handle this since connected=false)
                        sync_transmission!();
                        
                        // Reset PTT state on disconnect
                        ptt_active = false;

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

                        // Restart input if currently transmitting (need to use new device)
                        if audio_input.is_some() {
                            stop_transmission(&mut audio_input, &state, &repaint);
                            sync_transmission!();
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
                        if connection.is_some() && !self_deafened {
                            audio_output = start_audio_output(&audio_system, &selected_output, &mut user_audio);
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
                        {
                            let mut s = state.write().unwrap();
                            s.audio.voice_mode = mode;
                        }
                        repaint();
                        sync_transmission!();
                    }
                    
                    AudioCommand::SetMuted { muted } => {
                        self_muted = muted;
                        {
                            let mut s = state.write().unwrap();
                            s.audio.self_muted = muted;
                        }
                        repaint();
                        sync_transmission!();
                    }
                    
                    AudioCommand::SetDeafened { deafened } => {
                        self_deafened = deafened;
                        // Deafen implies mute
                        if deafened && !self_muted {
                            self_muted = true;
                        }
                        {
                            let mut s = state.write().unwrap();
                            s.audio.self_deafened = deafened;
                            s.audio.self_muted = self_muted;
                        }
                        repaint();
                        
                        // Handle audio output based on deafen state
                        if deafened {
                            audio_output = None;
                            user_audio.clear();
                            {
                                let mut s = state.write().unwrap();
                                s.audio.talking_users.clear();
                            }
                        } else if connection.is_some() && audio_output.is_none() {
                            audio_output = start_audio_output(&audio_system, &selected_output, &mut user_audio);
                        }
                        
                        sync_transmission!();
                    }
                    
                    AudioCommand::MuteUser { user_id } => {
                        muted_users.insert(user_id);
                        {
                            let mut s = state.write().unwrap();
                            s.audio.muted_users.insert(user_id);
                        }
                        repaint();
                    }
                    
                    AudioCommand::UnmuteUser { user_id } => {
                        muted_users.remove(&user_id);
                        {
                            let mut s = state.write().unwrap();
                            s.audio.muted_users.remove(&user_id);
                        }
                        repaint();
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
                    
                    AudioCommand::UpdateSettings { settings } => {
                        info!("Audio task: updating settings");
                        audio_settings = settings.clone();
                        
                        // Update settings in shared state
                        {
                            let mut s = state.write().unwrap();
                            s.audio.settings = settings;
                        }
                        repaint();
                        
                        // If currently transmitting, stop and restart with new settings
                        if audio_input.is_some() {
                            stop_transmission(&mut audio_input, &state, &repaint);
                        }
                        sync_transmission!();
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
                        
                        // Update state
                        {
                            let mut s = state.write().unwrap();
                            s.audio.stats = AudioStats::default();
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
            Some(encoded_frame) = encoded_rx.recv() => {
                if let Some(conn) = &connection {
                    let datagram = VoiceDatagram {
                        sender_id: Some(my_user_id),
                        room_id: None, // TODO: set room ID
                        sequence: send_sequence,
                        timestamp_us: 0, // TODO: track timestamp
                        opus_data: encoded_frame.data.to_vec(),
                    };
                    send_sequence = send_sequence.wrapping_add(1);
                    let datagram_bytes = datagram.encode_to_vec();
                    
                    // Track statistics
                    packets_sent += 1;
                    bytes_sent += encoded_frame.size_bytes as u64;

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
                                // Don't play back our own audio or audio from muted users
                                if sender_id != my_user_id && !muted_users.contains(&sender_id) {
                                    handle_voice_datagram(
                                        sender_id,
                                        voice.sequence,
                                        voice.opus_data,
                                        &mut user_audio,
                                        &audio_settings,
                                        &state,
                                        &repaint,
                                    );
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
            
            // Periodic stats update
            _ = stats_interval.tick() => {
                update_stats(&user_audio, packets_sent, bytes_sent, &state, &repaint);
            }
        }
    }
}

/// Start audio transmission (input capture + encoding).
fn start_transmission(
    audio_system: &AudioSystem,
    selected_input: &Option<String>,
    encoded_tx: &mpsc::UnboundedSender<EncodedFrame>,
    audio_settings: &AudioSettings,
    audio_input: &mut Option<AudioInput>,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    if audio_input.is_some() {
        return; // Already transmitting
    }

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

    // Create encoder with current settings
    let encoder_settings = EncoderSettings {
        bitrate: audio_settings.bitrate,
        complexity: audio_settings.encoder_complexity,
        fec_enabled: audio_settings.fec_enabled,
        packet_loss_percent: audio_settings.packet_loss_percent,
        dtx_enabled: true,
        vbr_enabled: true,
    };
    
    let encoder = match VoiceEncoder::with_settings(encoder_settings) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to create Opus encoder: {}", e);
            return;
        }
    };
    let encoder_mutex = std::sync::Arc::new(std::sync::Mutex::new(encoder));

    // Create audio config with denoise setting
    let audio_config = if audio_settings.denoise_enabled {
        AudioConfig::default().with_denoise()
    } else {
        AudioConfig::default()
    };

    // Create audio input with encoding callback
    let encoded_tx = encoded_tx.clone();
    let input = AudioInput::new(&device, &audio_config, move |samples| {
        // Encode the audio frame
        if let Ok(mut enc) = encoder_mutex.lock() {
            match enc.encode(samples) {
                Ok(encoded) => {
                    let size_bytes = encoded.len();
                    let _ = encoded_tx.send(EncodedFrame {
                        data: Bytes::from(encoded),
                        size_bytes,
                    });
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
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    if audio_input.is_none() {
        return; // Not transmitting
    }

    // The encoder is owned by the AudioInput's callback closure,
    // so it will be dropped when we drop the AudioInput.
    *audio_input = None;

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
    audio_settings: &AudioSettings,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    // Get or create per-user audio state
    let jitter_delay = audio_settings.jitter_buffer_delay_packets;
    let user_state = user_audio.entry(sender_id).or_insert_with(|| {
        UserAudioState::new(jitter_delay).expect("create user audio state")
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

/// Update audio statistics in shared state.
fn update_stats(
    user_audio: &HashMap<u64, UserAudioState>,
    packets_sent: u64,
    bytes_sent: u64,
    state: &Arc<RwLock<State>>,
    _repaint: &Arc<dyn Fn() + Send + Sync>,
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
    
    // Update state
    {
        let mut s = state.write().unwrap();
        s.audio.stats.packets_sent = packets_sent;
        s.audio.stats.packets_received = total_packets_received;
        s.audio.stats.packets_lost = total_packets_lost;
        s.audio.stats.packets_recovered_fec = total_packets_recovered_fec;
        s.audio.stats.frames_concealed = total_frames_concealed;
        s.audio.stats.bytes_sent = bytes_sent;
        s.audio.stats.bytes_received = total_bytes_received;
        s.audio.stats.avg_frame_size_bytes = avg_frame_size;
        s.audio.stats.actual_bitrate_bps = actual_bitrate_bps;
        s.audio.stats.playback_buffer_packets = total_buffer_packets;
        s.audio.stats.last_update = Some(Instant::now());
    }
    
    // Note: We don't call repaint here to avoid excessive repaints
    // The stats will be visible on next UI-triggered repaint
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
        handle.send(AudioCommand::SetMuted { muted: true });
        handle.send(AudioCommand::SetDeafened { deafened: false });
        handle.send(AudioCommand::SetVoiceMode { mode: VoiceMode::Continuous });
    }

    #[test]
    fn test_user_audio_state_creation() {
        let state = UserAudioState::new(3);
        assert!(state.is_ok());
    }
    
    #[test]
    fn test_user_audio_state_sequence_restart_detection() {
        let mut state = UserAudioState::new(3).unwrap();
        
        // Simulate initial stream: packets 0, 1, 2, 3, 4
        for seq in 0..5 {
            state.insert_packet(seq, vec![0u8; 20]);
        }
        
        // Start playback (sets started = true)
        assert!(state.ready_to_play());
        let _ = state.get_next_frame();
        assert!(state.started);
        
        // Advance next_play_seq to simulate having played some frames
        state.next_play_seq = 100;
        
        // Now simulate sender restart: new packets starting at 0
        state.insert_packet(0, vec![0u8; 20]);
        
        // The state should have been reset
        assert_eq!(state.next_play_seq, 0, "next_play_seq should reset to new stream start");
        assert!(state.jitter_buffer.contains_key(&0), "jitter buffer should contain new packet");
        assert!(!state.started, "started should be reset for buffering");
    }
    
    #[test]
    fn test_user_audio_state_no_reset_for_normal_packets() {
        let mut state = UserAudioState::new(3).unwrap();
        
        // Simulate initial stream
        for seq in 0..5 {
            state.insert_packet(seq, vec![0u8; 20]);
        }
        let _ = state.get_next_frame();
        
        // next_play_seq should be 1 now (just played 0)
        assert_eq!(state.next_play_seq, 1);
        
        // Insert next expected packet - should NOT reset
        state.insert_packet(5, vec![0u8; 20]);
        assert_eq!(state.next_play_seq, 1, "next_play_seq should not change for normal packets");
    }

    // Helper: simulate should_transmit logic
    fn should_transmit(
        voice_mode: VoiceMode,
        self_muted: bool,
        ptt_active: bool,
        connected: bool,
    ) -> bool {
        if !connected || self_muted {
            return false;
        }
        match voice_mode {
            VoiceMode::Continuous => true,
            VoiceMode::PushToTalk => ptt_active,
        }
    }
    
    #[test]
    fn test_should_transmit_continuous_connected() {
        assert!(should_transmit(VoiceMode::Continuous, false, false, true));
    }
    
    #[test]
    fn test_should_transmit_continuous_muted() {
        assert!(!should_transmit(VoiceMode::Continuous, true, false, true));
    }
    
    #[test]
    fn test_should_transmit_continuous_disconnected() {
        assert!(!should_transmit(VoiceMode::Continuous, false, false, false));
    }
    
    #[test]
    fn test_should_transmit_ptt_active() {
        assert!(should_transmit(VoiceMode::PushToTalk, false, true, true));
    }
    
    #[test]
    fn test_should_transmit_ptt_inactive() {
        assert!(!should_transmit(VoiceMode::PushToTalk, false, false, true));
    }
    
    #[test]
    fn test_should_transmit_ptt_muted() {
        // Even if PTT is active, mute should prevent transmission
        assert!(!should_transmit(VoiceMode::PushToTalk, true, true, true));
    }
    
    /// Test sync_transmission logic: updating settings while in continuous mode
    #[test]
    fn test_sync_transmission_continuous_mode() {
        let voice_mode = VoiceMode::Continuous;
        let self_muted = false;
        let ptt_active = false;
        let connected = true;
        let mut audio_input_present = true;
        
        // Simulate stopping for settings update
        audio_input_present = false;
        
        // sync_transmission! would do:
        let want = should_transmit(voice_mode, self_muted, ptt_active, connected);
        let have = audio_input_present;
        
        if want && !have {
            audio_input_present = true; // start_transmission would be called
        } else if !want && have {
            audio_input_present = false; // stop_transmission would be called
        }
        
        assert!(audio_input_present, "Should restart transmission in continuous mode");
    }
    
    /// Test sync_transmission logic: muting stops transmission
    #[test]
    fn test_sync_transmission_mute() {
        let voice_mode = VoiceMode::Continuous;
        let self_muted = true; // NOW MUTED
        let ptt_active = false;
        let connected = true;
        let mut audio_input_present = true; // Currently transmitting
        
        let want = should_transmit(voice_mode, self_muted, ptt_active, connected);
        let have = audio_input_present;
        
        if want && !have {
            audio_input_present = true;
        } else if !want && have {
            audio_input_present = false; // stop_transmission should be called
        }
        
        assert!(!audio_input_present, "Should stop transmission when muted");
    }
    
    /// Test sync_transmission logic: unmuting resumes transmission
    #[test]
    fn test_sync_transmission_unmute() {
        let voice_mode = VoiceMode::Continuous;
        let self_muted = false; // UNMUTED
        let ptt_active = false;
        let connected = true;
        let mut audio_input_present = false; // Currently not transmitting (was muted)
        
        let want = should_transmit(voice_mode, self_muted, ptt_active, connected);
        let have = audio_input_present;
        
        if want && !have {
            audio_input_present = true; // start_transmission should be called
        } else if !want && have {
            audio_input_present = false;
        }
        
        assert!(audio_input_present, "Should resume transmission when unmuted in continuous mode");
    }
    
    /// Test sync_transmission logic: PTT release in PTT mode
    #[test]
    fn test_sync_transmission_ptt_release() {
        let voice_mode = VoiceMode::PushToTalk;
        let self_muted = false;
        let ptt_active = false; // PTT released
        let connected = true;
        let mut audio_input_present = true; // Was transmitting
        
        let want = should_transmit(voice_mode, self_muted, ptt_active, connected);
        let have = audio_input_present;
        
        if want && !have {
            audio_input_present = true;
        } else if !want && have {
            audio_input_present = false; // stop_transmission should be called
        }
        
        assert!(!audio_input_present, "Should stop transmission when PTT released");
    }
}
