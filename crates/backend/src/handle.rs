//! Backend handle for UI integration.
//!
//! The `BackendHandle` provides a clean state-driven interface for UI code
//! to interact with the backend. It manages the async runtime, client connection,
//! audio subsystem, and provides:
//!
//! - A shared `State` object the UI reads for rendering
//! - A `send()` method for commands
//! - A repaint callback for state change notifications
//!
//! # Architecture
//!
//! The backend spawns two independent background tasks:
//!
//! 1. **Connection Task** (tokio thread):
//!    - Manages QUIC connection lifecycle (reliable streams only)
//!    - Sends/receives protocol messages
//!    - Updates connection and room state
//!    - Passes Connection handle to Audio Task on connect
//!
//! 2. **Audio Task** (separate thread):
//!    - Owns QUIC datagram send/receive
//!    - Manages cpal audio streams
//!    - Runs Opus encoder (capture) and per-user decoders (playback)
//!    - Manages per-user jitter buffers
//!    - Updates `talking_users` in shared state
//!
//! # Usage
//!
//! ```ignore
//! // Create with repaint callback
//! let handle = BackendHandle::new(|| ctx.request_repaint());
//!
//! // Send commands
//! handle.send(Command::Connect { ... });
//!
//! // Read state for rendering
//! let state = handle.state();
//! ```

#[cfg(feature = "p2p")]
use crate::p2p::{P2PManager, build_p2p_magnet, parse_p2p_magnet};
use crate::{
    ConnectConfig,
    audio::AudioSystem,
    audio_dump::AudioDumper,
    audio_task::{AudioCommand, AudioTaskConfig, AudioTaskHandle, spawn_audio_task},
    cert_verifier::{
        CapturedCert, InteractiveCertVerifier, is_cert_verification_error, new_captured_cert, take_captured_cert,
    },
    events::{AudioState, Command, ConnectionState, PendingCertificate, SigningCallback, State, VoiceMode},
};
use api::{
    ROOT_ROOM_UUID, build_auth_payload, build_session_cert_payload, compute_cert_hash, compute_session_id,
    encode_frame,
    proto::{self, envelope::Payload},
    room_id_from_uuid, try_decode_frame,
};
use bytes::BytesMut;
use ed25519_dalek::SigningKey;
#[cfg(feature = "p2p")]
use libp2p::{Multiaddr, identity};
use prost::Message;
use quinn::{Endpoint, crypto::rustls::QuicClientConfig};
use std::{
    collections::{HashMap, HashSet},
    net::ToSocketAddrs,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Detect MIME type from a filename's extension.
fn mime_from_extension(filename: &str) -> Option<String> {
    let ext = filename.rsplit('.').next()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        // Images
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        // Audio
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "opus" => "audio/opus",
        "m4a" => "audio/mp4",
        // Video
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        // Documents
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "md" => "text/markdown",
        // Archives
        "zip" => "application/zip",
        "gz" | "gzip" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "xz" => "application/x-xz",
        // Other
        "exe" => "application/octet-stream",
        "wasm" => "application/wasm",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Map a raw error message to a user-friendly description.
fn friendly_download_error(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("connection refused") {
        "Could not reach any peers sharing this file".to_string()
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "Download timed out \u{2014} peers may be offline".to_string()
    } else if lower.contains("no such file") || lower.contains("not found") {
        "File not found \u{2014} the magnet link may be invalid or expired".to_string()
    } else if lower.contains("no peers") || lower.contains("no seeds") {
        "No peers available \u{2014} nobody is currently sharing this file".to_string()
    } else if lower.contains("invalid magnet") || lower.contains("invalid info hash") {
        "Invalid magnet link \u{2014} please check the link and try again".to_string()
    } else if lower.contains("disk") || lower.contains("no space") || lower.contains("permission denied") {
        "Disk error \u{2014} check available space and file permissions".to_string()
    } else if lower.contains("dns") || lower.contains("resolve") {
        "Network error \u{2014} could not resolve tracker address".to_string()
    } else {
        format!("Download failed: {}", raw)
    }
}

/// A handle to the backend that can be used from UI code.
///
/// This type manages the tokio runtime, async connection, audio subsystem,
/// and provides a state-driven interface for the UI. The UI reads state
/// via `state()` and sends commands via `send()`.
pub struct BackendHandle {
    /// Shared state that the UI reads.
    state: Arc<RwLock<State>>,
    /// Channel to send commands to the connection task.
    command_tx: mpsc::UnboundedSender<Command>,
    /// Handle to send commands to the audio task.
    audio_task: AudioTaskHandle,
    /// Background thread running the tokio runtime for connection task.
    _runtime_thread: std::thread::JoinHandle<()>,
    /// Connection configuration (certificates, etc.). stored incase we want to inspect it later.
    _connect_config: ConnectConfig,
    /// Pre-generated sound effects library.
    sfx_library: crate::sfx::SfxLibrary,
}

impl BackendHandle {
    /// Create a new backend handle with the given repaint callback.
    ///
    /// The repaint callback is called whenever state changes, allowing
    /// the UI to request a repaint.
    pub fn new<F>(repaint_callback: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self::with_config_and_dumper(repaint_callback, ConnectConfig::new(), None)
    }

    /// Create a new backend handle with a repaint callback and connect config.
    pub fn with_config<F>(repaint_callback: F, connect_config: ConnectConfig) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self::with_config_and_dumper(repaint_callback, connect_config, None)
    }

    /// Create a new backend handle with audio dumping enabled.
    ///
    /// Audio dumping writes raw audio data to files for debugging:
    /// - `mic_raw.pcm` - Raw microphone input (f32 samples)
    /// - `tx_opus.bin` - Encoded opus packets being sent
    /// - `rx_opus.bin` - Received opus packets
    /// - `rx_decoded.pcm` - Decoded audio before mixing (f32 samples)
    pub fn with_audio_dumper<F>(repaint_callback: F, connect_config: ConnectConfig, audio_dumper: AudioDumper) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self::with_config_and_dumper(repaint_callback, connect_config, Some(audio_dumper))
    }

    /// Internal constructor with optional audio dumper.
    fn with_config_and_dumper<F>(
        repaint_callback: F,
        connect_config: ConnectConfig,
        audio_dumper: Option<AudioDumper>,
    ) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        // Check for audio dump env var if no explicit dumper provided
        let audio_dumper =
            audio_dumper.or_else(|| crate::audio_dump::AudioDumpConfig::from_env().map(AudioDumper::new));

        let repaint_callback = Arc::new(repaint_callback);
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        // Initialize audio system (on main thread) and get device lists
        let audio_system = AudioSystem::new();
        let input_devices = audio_system.list_input_devices();
        let output_devices = audio_system.list_output_devices();

        // Initialize state with audio info
        let state = State {
            connection: ConnectionState::Disconnected,
            rooms: Vec::new(),
            users: Vec::new(),
            my_user_id: None,
            my_room_id: None,
            my_session_public_key: None,
            my_session_id: None,
            p2p_peers: HashMap::new(),
            audio: AudioState {
                input_devices,
                output_devices,
                selected_input: None,
                selected_output: None,
                voice_mode: VoiceMode::PushToTalk,
                self_muted: false,
                self_deafened: false,
                muted_users: HashSet::new(),
                is_transmitting: false,
                talking_users: HashSet::new(),
                settings: Default::default(),
                stats: Default::default(),
                tx_pipeline: Default::default(),
                rx_pipeline_defaults: Default::default(),
                per_user_rx: Default::default(),
                input_level_db: None,
            },
            chat_messages: Vec::new(),
            room_tree: Default::default(),
            file_transfers: Vec::new(),
            file_transfer_settings: Default::default(),
        };

        let state = Arc::new(RwLock::new(state));

        // Spawn the audio task (runs on its own thread)
        let audio_task = spawn_audio_task(AudioTaskConfig {
            state: state.clone(),
            repaint: repaint_callback.clone(),
            audio_dumper,
        });

        // Clone handles for the connection task
        let state_for_task = state.clone();
        let repaint_for_task = repaint_callback.clone();
        let config_for_task = connect_config.clone();
        let audio_task_for_connection = audio_task.clone();
        let command_tx_for_task = command_tx.clone();

        // Spawn background thread with tokio runtime for connection task
        let runtime_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async move {
                run_connection_task(
                    command_rx,
                    command_tx_for_task,
                    state_for_task,
                    repaint_for_task,
                    config_for_task,
                    audio_task_for_connection,
                )
                .await;
            });
        });

        Self {
            state,
            command_tx,
            audio_task,
            _runtime_thread: runtime_thread,
            _connect_config: connect_config,
            sfx_library: crate::sfx::SfxLibrary::new(),
        }
    }

    /// Get the current state for rendering.
    ///
    /// This returns a clone of the state. The UI should call this
    /// in its render loop to get the latest state.
    pub fn state(&self) -> State {
        read_state(&self.state).clone()
    }

    /// Send a command to the backend.
    ///
    /// Commands are fire-and-forget. The backend will update state
    /// asynchronously and call the repaint callback.
    pub fn send(&self, command: Command) {
        // Route audio-related commands to the audio task
        match &command {
            Command::RefreshAudioDevices => {
                self.audio_task.send(AudioCommand::RefreshDevices);
                return;
            }
            Command::SetInputDevice { device_id } => {
                self.audio_task.send(AudioCommand::SetInputDevice {
                    device_id: device_id.clone(),
                });
                return;
            }
            Command::SetOutputDevice { device_id } => {
                self.audio_task.send(AudioCommand::SetOutputDevice {
                    device_id: device_id.clone(),
                });
                return;
            }
            Command::SetVoiceMode { mode } => {
                self.audio_task.send(AudioCommand::SetVoiceMode { mode: *mode });
                return;
            }
            Command::SetMuted { muted } => {
                self.audio_task.send(AudioCommand::SetMuted { muted: *muted });
                // Also notify connection task to inform server
                // (don't return - let it fall through to send to connection task)
            }
            Command::SetDeafened { deafened } => {
                self.audio_task.send(AudioCommand::SetDeafened { deafened: *deafened });
                // Also notify connection task to inform server
                // (don't return - let it fall through to send to connection task)
            }
            Command::MuteUser { user_id } => {
                self.audio_task.send(AudioCommand::MuteUser { user_id: *user_id });
                return;
            }
            Command::UnmuteUser { user_id } => {
                self.audio_task.send(AudioCommand::UnmuteUser { user_id: *user_id });
                return;
            }
            Command::StartTransmit => {
                self.audio_task.send(AudioCommand::StartTransmit);
                return;
            }
            Command::StopTransmit => {
                self.audio_task.send(AudioCommand::StopTransmit);
                return;
            }
            Command::UpdateAudioSettings { settings } => {
                self.audio_task.send(AudioCommand::UpdateSettings {
                    settings: settings.clone(),
                });
                return;
            }
            Command::ResetAudioStats => {
                self.audio_task.send(AudioCommand::ResetStats);
                return;
            }
            Command::UpdateTxPipeline { config } => {
                self.audio_task
                    .send(AudioCommand::UpdateTxPipeline { config: config.clone() });
                return;
            }
            Command::UpdateRxPipelineDefaults { config } => {
                self.audio_task
                    .send(AudioCommand::UpdateRxPipelineDefaults { config: config.clone() });
                return;
            }
            Command::UpdateUserRxConfig { user_id, config } => {
                self.audio_task.send(AudioCommand::UpdateUserRxConfig {
                    user_id: *user_id,
                    config: config.clone(),
                });
                return;
            }
            Command::ClearUserRxOverride { user_id } => {
                self.audio_task
                    .send(AudioCommand::ClearUserRxOverride { user_id: *user_id });
                return;
            }
            Command::SetUserVolume { user_id, volume_db } => {
                self.audio_task.send(AudioCommand::SetUserVolume {
                    user_id: *user_id,
                    volume_db: *volume_db,
                });
                return;
            }
            Command::PlaySfx { kind, volume } => {
                if let Some(samples) = self.sfx_library.get(*kind) {
                    let scaled: Vec<f32> = samples.iter().map(|s| s * volume).collect();
                    self.audio_task.send(AudioCommand::PlaySfx { samples: scaled });
                }
                return;
            }
            _ => {}
        }

        // Forward non-audio commands to connection task
        let _ = self.command_tx.send(command);
    }

    /// Check if we are currently connected.
    pub fn is_connected(&self) -> bool {
        read_state(&self.state).connection.is_connected()
    }

    /// Get our user ID if connected.
    pub fn my_user_id(&self) -> Option<u64> {
        read_state(&self.state).my_user_id
    }

    /// Get our current room ID if in a room.
    pub fn my_room_id(&self) -> Option<Uuid> {
        read_state(&self.state).my_room_id
    }
}

/// Acquire a read lock on the state, recovering from lock poisoning.
pub(crate) fn read_state(state: &Arc<RwLock<State>>) -> RwLockReadGuard<'_, State> {
    match state.read() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("State RwLock was poisoned (read), recovering");
            poisoned.into_inner()
        }
    }
}

/// Acquire a write lock on the state, recovering from lock poisoning.
pub(crate) fn write_state(state: &Arc<RwLock<State>>) -> RwLockWriteGuard<'_, State> {
    match state.write() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("State RwLock was poisoned (write), recovering");
            poisoned.into_inner()
        }
    }
}

/// The main connection task that handles QUIC communication (reliable streams only).
///
/// This task manages:
/// - QUIC connection lifecycle
/// - Reliable protocol messages (via streams)
/// - State synchronization
///
/// It notifies the audio task when a connection is established or closed.
async fn run_connection_task(
    mut command_rx: mpsc::UnboundedReceiver<Command>,
    command_tx: mpsc::UnboundedSender<Command>,
    state: Arc<RwLock<State>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    config: ConnectConfig,
    audio_task: AudioTaskHandle,
) {
    // Connection state
    let mut connection: Option<quinn::Connection> = None;
    let mut send_stream: Option<quinn::SendStream> = None;
    let mut client_name = String::new();
    let mut torrent_manager: Option<Arc<crate::torrent::TorrentManager>> = None;
    let mut shared_infohashes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut _session_identity: Option<SessionIdentity> = None;
    #[cfg(feature = "p2p")]
    let mut p2p_manager: Option<Arc<P2PManager>> = None;
    let mut transfer_update_interval = tokio::time::interval(std::time::Duration::from_millis(500));

    loop {
        tokio::select! {
            _ = transfer_update_interval.tick() => {
                if let Some(tm) = &torrent_manager {
                    let transfers = tm.session().with_torrents(|iter| {
                        let mut transfers = Vec::new();
                        for (_id, handle) in iter {
                            let stats = handle.stats();
                            let name = handle.name().unwrap_or_else(|| "Unknown".to_string());
                            let progress = if stats.total_bytes > 0 {
                                stats.progress_bytes as f32 / stats.total_bytes as f32
                            } else {
                                0.0
                            };

                            // Map librqbit state to our TransferState
                            let state = match stats.state {
                                librqbit::TorrentStatsState::Initializing => crate::events::TransferState::Checking,
                                librqbit::TorrentStatsState::Paused => crate::events::TransferState::Paused,
                                librqbit::TorrentStatsState::Error => crate::events::TransferState::Error,
                                librqbit::TorrentStatsState::Live => {
                                    if stats.finished {
                                        crate::events::TransferState::Seeding
                                    } else {
                                        crate::events::TransferState::Downloading
                                    }
                                }
                            };

                            let info_hash = handle.info_hash();
                            let infohash_hex = hex::encode(info_hash.0);
                            let magnet = format!("magnet:?xt=urn:btih:{}", &infohash_hex);

                            // Get local file path for completed downloads
                            let local_path = if stats.finished {
                                tm.get_file_path(&infohash_hex).ok()
                            } else {
                                None
                            };

                            // Extract speed and peer info from live stats
                            let (download_speed, upload_speed, peers) = stats.live.as_ref()
                                .map(|l| {
                                    let dl = l.download_speed.as_bytes();
                                    let ul = l.upload_speed.as_bytes();
                                    let peer_stats = &l.snapshot.peer_stats;
                                    let peer_count = peer_stats.live + peer_stats.queued + peer_stats.connecting;
                                    (dl, ul, peer_count)
                                })
                                .unwrap_or((0, 0, 0));

                            // Get per-peer details from the live state
                            let peer_details = handle.live()
                                .map(|live| {
                                    use librqbit::http_api_types::{PeerStatsFilter, PeerStatsSnapshot};
                                    let filter = PeerStatsFilter::default(); // Default filters to live peers
                                    let snapshot: PeerStatsSnapshot = live.per_peer_stats_snapshot(filter);

                                    snapshot.peers.into_iter().map(|(addr_str, peer_stats)| {
                                        // Parse the address to check if it's a relay proxy
                                        let addr: Option<std::net::SocketAddr> = addr_str.parse().ok();
                                        let is_relay = addr.as_ref()
                                            .map(|a| tm.is_relay_proxy(a))
                                            .unwrap_or(false);

                                        // Determine connection type
                                        // Note: ConnectionKind is not publicly exported from librqbit,
                                        // so we serialize to JSON and check the string value
                                        let connection_type = if is_relay {
                                            crate::events::PeerConnectionType::Relay
                                        } else if let Some(ref conn_kind) = peer_stats.conn_kind {
                                            // ConnectionKind serializes to "tcp", "utp", or "socks"
                                            let kind_str = serde_json::to_string(conn_kind)
                                                .unwrap_or_default()
                                                .trim_matches('"')
                                                .to_string();
                                            match kind_str.as_str() {
                                                "tcp" => crate::events::PeerConnectionType::Direct,
                                                "utp" => crate::events::PeerConnectionType::Utp,
                                                "socks" => crate::events::PeerConnectionType::Socks,
                                                _ => crate::events::PeerConnectionType::Direct,
                                            }
                                        } else {
                                            crate::events::PeerConnectionType::Direct
                                        };

                                        // Map peer state
                                        let peer_state = match peer_stats.state {
                                            "live" => crate::events::PeerState::Live,
                                            "connecting" => crate::events::PeerState::Connecting,
                                            "queued" => crate::events::PeerState::Queued,
                                            _ => crate::events::PeerState::Dead,
                                        };

                                        // Get display address - use original addr for relay, else the connected addr
                                        let display_addr = if is_relay {
                                            addr.and_then(|a| tm.get_relay_original_addr(&a))
                                                .map(|a| a.to_string())
                                                .unwrap_or(addr_str.clone())
                                        } else {
                                            addr_str.clone()
                                        };

                                        crate::events::TransferPeerInfo {
                                            address: display_addr,
                                            connection_type,
                                            state: peer_state,
                                            downloaded_bytes: peer_stats.counters.fetched_bytes,
                                            uploaded_bytes: peer_stats.counters.uploaded_bytes,
                                        }
                                    }).collect::<Vec<_>>()
                                })
                                .unwrap_or_default();

                            // Get error message if in error state, with user-friendly mapping
                            let error = if matches!(state, crate::events::TransferState::Error) {
                                stats
                                    .error
                                    .as_deref()
                                    .map(friendly_download_error)
                            } else {
                                None
                            };

                            let mime = mime_from_extension(&name);

                            transfers.push(crate::events::FileTransferState {
                                infohash: info_hash.0,
                                name,
                                size: stats.total_bytes,
                                mime,
                                progress,
                                download_speed,
                                upload_speed,
                                peers,
                                seeders: Vec::new(), // TODO: populate from tracker responses
                                state,
                                error,
                                magnet: Some(magnet),
                                local_path,
                                peer_details,
                            });
                        }
                        transfers
                    });

                    {
                        let mut s = write_state(&state);
                        s.file_transfers = transfers;
                    }
                    repaint();
                }
            }
            cmd = command_rx.recv() => {
                let Some(cmd) = cmd else {
                    // Channel closed, handle is dropped, exit task
                    debug!("Command channel closed, shutting down connection task");
                    if let Some(conn) = connection.take() {
                        conn.close(quinn::VarInt::from_u32(0), b"shutdown");
                    }
                    #[cfg(feature = "p2p")]
                    if let Some(p2p) = p2p_manager.take() {
                        p2p.shutdown().await;
                    }
                    return;
                };
                match cmd {
                    Command::Connect { addr, name, public_key, signer, password } => {
                        client_name = name.clone();

                        // Update state to Connecting
                        {
                            let mut s = write_state(&state);
                            s.connection = ConnectionState::Connecting { server_addr: addr.clone() };
                        }
                        repaint();

                        // Create a captured cert holder for the verifier to store self-signed certs
                        let captured_cert = new_captured_cert();

                        // Attempt connection with Ed25519 auth
                        match connect_to_server(&addr, &name, &public_key, &signer, password.as_deref(), &config, captured_cert.clone()).await {
                            Ok((conn, send, recv, recv_buf, user_id, rooms, users, session_info)) => {
                                // Update state to Connected
                                {
                                    let mut s = write_state(&state);
                                    s.connection = ConnectionState::Connected {
                                        server_name: "Rumble Server".to_string(),
                                        user_id,
                                    };
                                    s.my_user_id = Some(user_id);
                                    // Find our user in the users list and get their current room
                                    s.my_room_id = users.iter()
                                        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
                                        .and_then(|u| u.current_room.as_ref())
                                        .and_then(api::uuid_from_room_id)
                                        .or(Some(ROOT_ROOM_UUID));
                                    s.my_session_public_key = Some(session_info.session_public_key);
                                    s.my_session_id = Some(session_info.session_id);
                                    s.rooms = rooms;
                                    s.users = users;
                                    s.rebuild_room_tree();
                                }
                                repaint();

                                // Notify audio task of new connection
                                audio_task.send(AudioCommand::ConnectionEstablished {
                                    connection: conn.clone(),
                                    my_user_id: user_id,
                                });

                                connection = Some(conn.clone());
                                send_stream = Some(send);
                                _session_identity = Some(session_info);

                                #[cfg(feature = "p2p")]
                                {
                                    p2p_manager = start_p2p_manager_from_session(
                                        &_session_identity.as_ref().unwrap().signing_key,
                                        &config,
                                    )
                                    .await;

                                    // Send PeerCapabilities to server
                                    if let Some(ref p2p) = p2p_manager {
                                        send_peer_capabilities(
                                            send_stream.as_mut().unwrap(),
                                            p2p.clone(),
                                        )
                                        .await;
                                    }
                                }

                                // Initialize TorrentManager
                                let temp_dir = config.download_dir.clone()
                                    .unwrap_or_else(|| std::env::temp_dir().join("rumble_downloads"));
                                match crate::torrent::TorrentManager::new(conn.clone(), temp_dir).await {
                                    Ok(tm) => {
                                        tm.set_needs_relay(config.prefer_relay);
                                        torrent_manager = Some(Arc::new(tm));
                                    }
                                    Err(e) => {
                                        error!("Failed to initialize TorrentManager: {}", e);
                                    }
                                }

                                // Spawn receiver task for reliable messages
                                let state_clone = state.clone();
                                let repaint_clone = repaint.clone();
                                let audio_task_clone = audio_task.clone();
                                let torrent_manager_clone = torrent_manager.clone();
                                let command_tx_clone = command_tx.clone();
                                let shared_infohashes_clone = shared_infohashes.clone();
                                tokio::spawn(async move {
                                    run_receiver_task(conn, recv, recv_buf, state_clone, repaint_clone, audio_task_clone, torrent_manager_clone, command_tx_clone, shared_infohashes_clone).await;
                                });
                            }
                            Err(e) => {
                                // Check if the verifier captured a self-signed certificate
                                if let Some(cert_info) = take_captured_cert(&captured_cert) {
                                    info!("Self-signed certificate detected, prompting user for acceptance");
                                    let pending = PendingCertificate {
                                        certificate_der: cert_info.certificate_der,
                                        fingerprint: cert_info.fingerprint,
                                        server_name: cert_info.server_name,
                                        server_addr: addr.clone(),
                                        username: name.clone(),
                                        password: password.clone(),
                                        public_key,
                                        signer: signer.clone(),
                                    };
                                    {
                                        let mut s = write_state(&state);
                                        s.connection = ConnectionState::CertificatePending { cert_info: pending };
                                    }
                                    repaint();
                                } else if is_cert_verification_error(&e) {
                                    // Cert verification error but we didn't capture the cert - shouldn't happen
                                    // but log and treat as connection failure
                                    error!("Certificate verification error but no cert captured: {}", e);
                                    {
                                        let mut s = write_state(&state);
                                        s.connection = ConnectionState::ConnectionLost {
                                            error: format!("Certificate error: {}", e)
                                        };
                                    }
                                    repaint();
                                } else {
                                    error!("Connection failed: {}", e);
                                    {
                                        let mut s = write_state(&state);
                                        s.connection = ConnectionState::ConnectionLost { error: e.to_string() };
                                    }
                                    repaint();
                                }
                            }
                        }
                    }

                    Command::AcceptCertificate => {
                        // Get the pending certificate info from state
                        let pending_info = {
                            let s = read_state(&state);
                            if let ConnectionState::CertificatePending { cert_info } = &s.connection {
                                Some(cert_info.clone())
                            } else {
                                None
                            }
                        };

                        if let Some(pending) = pending_info {
                            info!("User accepted certificate for {}", pending.server_name);

                            // Update state to Connecting
                            {
                                let mut s = write_state(&state);
                                s.connection = ConnectionState::Connecting { server_addr: pending.server_addr.clone() };
                            }
                            repaint();

                            // Create a new config with the certificate added
                            let mut new_config = config.clone();
                            new_config.accepted_certs.push(pending.certificate_der.clone());

                            // Create a new captured cert holder (shouldn't capture again since cert is now trusted)
                            let captured_cert = new_captured_cert();

                            // Retry connection with the certificate trusted
                            match connect_to_server(
                                &pending.server_addr,
                                &pending.username,
                                &pending.public_key,
                                &pending.signer,
                                pending.password.as_deref(),
                                &new_config,
                                captured_cert,
                            ).await {
                                Ok((conn, send, recv, recv_buf, user_id, rooms, users, session_info)) => {
                                    // Success! Update state
                                    {
                                        let mut s = write_state(&state);
                                        s.connection = ConnectionState::Connected {
                                            server_name: "Rumble Server".to_string(),
                                            user_id,
                                        };
                                        s.my_user_id = Some(user_id);
                                        s.my_room_id = users.iter()
                                            .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
                                            .and_then(|u| u.current_room.as_ref())
                                            .and_then(api::uuid_from_room_id)
                                            .or(Some(ROOT_ROOM_UUID));
                                        s.my_session_public_key = Some(session_info.session_public_key);
                                        s.my_session_id = Some(session_info.session_id);
                                        s.rooms = rooms;
                                        s.users = users;
                                        s.rebuild_room_tree();
                                    }
                                    repaint();

                                    // Notify audio task
                                    audio_task.send(AudioCommand::ConnectionEstablished {
                                        connection: conn.clone(),
                                        my_user_id: user_id,
                                    });

                                    connection = Some(conn.clone());
                                    send_stream = Some(send);
                                    client_name = pending.username;
                                    _session_identity = Some(session_info);

                                    #[cfg(feature = "p2p")]
                                    {
                                        p2p_manager = start_p2p_manager_from_session(
                                            &_session_identity.as_ref().unwrap().signing_key,
                                            &new_config,
                                        )
                                        .await;

                                        // Send PeerCapabilities to server
                                        if let Some(ref p2p) = p2p_manager {
                                            send_peer_capabilities(
                                                send_stream.as_mut().unwrap(),
                                                p2p.clone(),
                                            )
                                            .await;
                                        }
                                    }

                                    // Initialize TorrentManager
                                    let temp_dir = new_config.download_dir.clone()
                                        .unwrap_or_else(|| std::env::temp_dir().join("rumble_downloads"));
                                    match crate::torrent::TorrentManager::new(conn.clone(), temp_dir).await {
                                        Ok(tm) => {
                                            tm.set_needs_relay(new_config.prefer_relay);
                                            torrent_manager = Some(Arc::new(tm));
                                        }
                                        Err(e) => {
                                            error!("Failed to initialize TorrentManager: {}", e);
                                        }
                                    }

                                    // Spawn receiver task
                                    let state_clone = state.clone();
                                    let repaint_clone = repaint.clone();
                                    let audio_task_clone = audio_task.clone();
                                    let torrent_manager_clone = torrent_manager.clone();
                                    let command_tx_clone = command_tx.clone();
                                    let shared_infohashes_clone = shared_infohashes.clone();
                                    tokio::spawn(async move {
                                        run_receiver_task(conn, recv, recv_buf, state_clone, repaint_clone, audio_task_clone, torrent_manager_clone, command_tx_clone, shared_infohashes_clone).await;
                                    });
                                }
                                Err(e) => {
                                    error!("Connection failed after accepting certificate: {}", e);
                                    {
                                        let mut s = write_state(&state);
                                        s.connection = ConnectionState::ConnectionLost { error: e.to_string() };
                                    }
                                    repaint();
                                }
                            }
                        } else {
                            warn!("AcceptCertificate received but no certificate pending");
                        }
                    }

                    Command::RejectCertificate => {
                        // Simply go back to disconnected state
                        {
                            let s = read_state(&state);
                            if let ConnectionState::CertificatePending { cert_info } = &s.connection {
                                info!("User rejected certificate for {}", cert_info.server_name);
                            }
                        }
                        {
                            let mut s = write_state(&state);
                            s.connection = ConnectionState::Disconnected;
                        }
                        repaint();
                    }

                    Command::Disconnect => {
                        // Notify audio task before closing
                        audio_task.send(AudioCommand::ConnectionClosed);

                        if let Some(conn) = connection.take() {
                            conn.close(quinn::VarInt::from_u32(0), b"disconnect");
                        }
                        send_stream = None;
                        torrent_manager = None;
                        #[cfg(feature = "p2p")]
                        if let Some(p2p) = p2p_manager.take() {
                            p2p.shutdown().await;
                        }
                        {
                            let mut s = write_state(&state);
                            s.connection = ConnectionState::Disconnected;
                            s.my_user_id = None;
                            s.my_room_id = None;
                            s.my_session_public_key = None;
                            s.my_session_id = None;
                            s.rooms.clear();
                            s.users.clear();
                            s.rebuild_room_tree();
                        }
                        repaint();
                    }

                    Command::JoinRoom { room_id } => {
                        if let Some(send) = &mut send_stream {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::JoinRoom(proto::JoinRoom {
                                    room_id: Some(room_id_from_uuid(room_id)),
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send JoinRoom: {}", e);
                            }
                        }
                    }

                    Command::CreateRoom { name, parent_id } => {
                        if let Some(send) = &mut send_stream {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::CreateRoom(proto::CreateRoom {
                                    name,
                                    parent_id: parent_id.map(room_id_from_uuid),
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send CreateRoom: {}", e);
                            }
                        }
                    }

                    Command::DeleteRoom { room_id } => {
                        if let Some(send) = &mut send_stream {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::DeleteRoom(proto::DeleteRoom {
                                    room_id: Some(room_id_from_uuid(room_id)),
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send DeleteRoom: {}", e);
                            }
                        }
                    }

                    Command::RenameRoom { room_id, new_name } => {
                        if let Some(send) = &mut send_stream {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::RenameRoom(proto::RenameRoom {
                                    room_id: Some(room_id_from_uuid(room_id)),
                                    new_name,
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send RenameRoom: {}", e);
                            }
                        }
                    }

                    Command::MoveRoom { room_id, new_parent_id } => {
                        if let Some(send) = &mut send_stream {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::MoveRoom(proto::MoveRoom {
                                    room_id: Some(room_id_from_uuid(room_id)),
                                    new_parent_id: Some(room_id_from_uuid(new_parent_id)),
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send MoveRoom: {}", e);
                            }
                        }
                    }

                    Command::SendChat { text } => {
                        if let Some(send) = &mut send_stream {
                            let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
                            let timestamp_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as i64;
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                    id: message_id,
                                    timestamp_ms,
                                    sender: client_name.clone(),
                                    text,
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send ChatMessage: {}", e);
                            }
                        }
                    }

                    Command::LocalMessage { text } => {
                        let mut s = write_state(&state);
                        s.chat_messages.push(crate::events::ChatMessage {
                            id: uuid::Uuid::new_v4().into_bytes(),
                            sender: String::new(),
                            text,
                            timestamp: std::time::SystemTime::now(),
                            is_local: true,
                        });
                        // Keep only recent messages
                        if s.chat_messages.len() > 100 {
                            s.chat_messages.remove(0);
                        }
                        drop(s);
                        repaint();
                    }

                    // Audio commands are routed to audio task in BackendHandle::send()
                    Command::SetMuted { muted } => {
                        // Send status update to server
                        if let Some(send) = &mut send_stream {
                            let s = read_state(&state);
                            let is_deafened = s.audio.self_deafened;
                            drop(s);
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::SetUserStatus(proto::SetUserStatus {
                                    is_muted: muted,
                                    is_deafened,
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send SetUserStatus: {}", e);
                            }
                        }
                    }

                    Command::SetDeafened { deafened } => {
                        // Send status update to server
                        // Note: deafen implies mute
                        if let Some(send) = &mut send_stream {
                            let s = read_state(&state);
                            let is_muted = s.audio.self_muted || deafened;
                            drop(s);
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::SetUserStatus(proto::SetUserStatus {
                                    is_muted,
                                    is_deafened: deafened,
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send SetUserStatus: {}", e);
                            }
                        }
                    }

                    // Other audio commands are routed to audio task in BackendHandle::send()
                    Command::StartTransmit
                    | Command::StopTransmit
                    | Command::SetInputDevice { .. }
                    | Command::SetOutputDevice { .. }
                    | Command::SetVoiceMode { .. }
                    | Command::MuteUser { .. }
                    | Command::UnmuteUser { .. }
                    | Command::RefreshAudioDevices
                    | Command::UpdateAudioSettings { .. }
                    | Command::ResetAudioStats
                    | Command::UpdateTxPipeline { .. }
                    | Command::UpdateRxPipelineDefaults { .. }
                    | Command::UpdateUserRxConfig { .. }
                    | Command::ClearUserRxOverride { .. }
                    | Command::SetUserVolume { .. } => {
                        debug!("Audio command received in connection task - should be routed to audio task");
                    }

                    Command::RegisterUser { user_id } => {
                        if let Some(send) = &mut send_stream {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::RegisterUser(proto::RegisterUser {
                                    user_id: Some(proto::UserId { value: user_id }),
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send RegisterUser: {}", e);
                            }
                        }
                    }

                    Command::UnregisterUser { user_id } => {
                        if let Some(send) = &mut send_stream {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::UnregisterUser(proto::UnregisterUser {
                                    user_id: Some(proto::UserId { value: user_id }),
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send UnregisterUser: {}", e);
                            }
                        }
                    }

                    Command::ShareFile { path } => {
                        #[cfg(feature = "p2p")]
                        if let Some(p2p) = &p2p_manager {
                            let state = state.clone();
                            let repaint = repaint.clone();
                            let client = client_name.clone();
                            let path = path.clone();
                            let mut send_opt = send_stream.as_mut();

                            match p2p.share_file(path.clone()).await {
                                Ok(shared) => {
                                    shared_infohashes.insert(hex::encode(shared.id));
                                    let addrs = p2p.listen_addrs().await;
                                    let magnet = build_p2p_magnet(p2p.peer_id(), &shared, &addrs);
                                    let addr_strings: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();

                                    let msg = crate::events::P2pFileMessage::new(
                                        shared.name.clone(),
                                        shared.size,
                                        hex::encode(shared.id),
                                        p2p.peer_id().to_string(),
                                        addr_strings,
                                    );

                                    if let Some(send) = send_opt.as_mut() {
                                        let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
                                        let timestamp_ms = SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_millis() as i64;

                                        let env = proto::Envelope {
                                            state_hash: Vec::new(),
                                            payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                                id: message_id,
                                                timestamp_ms,
                                                sender: client.clone(),
                                                text: msg.to_json(),
                                            })),
                                        };
                                        let frame = encode_frame(&env);
                                        if let Err(e) = send.write_all(&frame).await {
                                            error!("Failed to send p2p file share message: {}", e);
                                        }
                                    }

                                    let mut s = write_state(&state);
                                    s.chat_messages.push(crate::events::ChatMessage {
                                        id: uuid::Uuid::new_v4().into_bytes(),
                                        sender: "System".to_string(),
                                        text: format!("Sharing {} via P2P (magnet: {})", shared.name, magnet),
                                        timestamp: SystemTime::now(),
                                        is_local: true,
                                    });
                                    repaint();
                                    continue;
                                }
                                Err(e) => {
                                    error!("Failed to share file via P2P: {}", e);
                                    let mut s = write_state(&state);
                                    s.chat_messages.push(crate::events::ChatMessage {
                                        id: uuid::Uuid::new_v4().into_bytes(),
                                        sender: "System".to_string(),
                                        text: format!("Failed to share file via P2P: {}", e),
                                        timestamp: SystemTime::now(),
                                        is_local: true,
                                    });
                                    repaint();
                                    // fall through to torrent path as a fallback
                                }
                            }
                        }

                        if let (Some(tm), Some(send)) = (&torrent_manager, &mut send_stream) {
                            let tm = tm.clone();
                            let path = path.clone();
                            let state = state.clone();
                            let repaint = repaint.clone();
                            let client = client_name.clone();

                            // Share file and get info
                            match tm.share_file(path).await {
                                Ok(file_info) => {
                                    info!("Shared file: {} ({})", file_info.name, file_info.magnet);
                                    shared_infohashes.insert(file_info.infohash.clone());

                                    // Create file message JSON
                                    let file_message = crate::events::FileMessage::new(
                                        file_info.name.clone(),
                                        file_info.size,
                                        file_info.mime.clone(),
                                        file_info.infohash.clone(),
                                    );
                                    let text = file_message.to_json();
                                    let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
                                    let timestamp_ms = SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_millis() as i64;

                                    // Send to server as chat message
                                    let env = proto::Envelope {
                                        state_hash: Vec::new(),
                                        payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                            id: message_id.clone(),
                                            timestamp_ms,
                                            sender: client.clone(),
                                            text: text.clone(),
                                        })),
                                    };
                                    let frame = encode_frame(&env);
                                    if let Err(e) = send.write_all(&frame).await {
                                        error!("Failed to send file share message: {}", e);
                                    }

                                    // Add local confirmation with magnet link
                                    let mut s = write_state(&state);
                                    s.chat_messages.push(crate::events::ChatMessage {
                                        id: uuid::Uuid::new_v4().into_bytes(),
                                        sender: "System".to_string(),
                                        text: format!("Sharing {} ({} bytes)\nMagnet: {}", file_info.name, file_info.size, file_info.magnet),
                                        timestamp: SystemTime::now(),
                                        is_local: true,
                                    });
                                    repaint();
                                }
                                Err(e) => {
                                    error!("Failed to share file: {}", e);
                                    let mut s = write_state(&state);
                                    s.chat_messages.push(crate::events::ChatMessage {
                                        id: uuid::Uuid::new_v4().into_bytes(),
                                        sender: "System".to_string(),
                                        text: format!("Failed to share file: {}", e),
                                        timestamp: SystemTime::now(),
                                        is_local: true,
                                    });
                                    repaint();
                                }
                            }
                        }
                    }

                    Command::DownloadFile { magnet } => {
                        #[cfg(feature = "p2p")]
                        if let Some(p2p) = &p2p_manager {
                            if let Ok((peer_id, file_id, addrs)) = parse_p2p_magnet(&magnet) {
                                let p2p = p2p.clone();
                                let state = state.clone();
                                let repaint = repaint.clone();
                                let download_dir = config
                                    .download_dir
                                    .clone()
                                    .unwrap_or_else(|| std::env::temp_dir().join("rumble_downloads"));

                                tokio::spawn(async move {
                                    for addr in &addrs {
                                        p2p.dial(addr.clone()).await;
                                    }

                                    match p2p.fetch_file(peer_id, file_id).await {
                                        Ok((name, data)) => {
                                            // Check if this is a chat history file
                                            if name == "chat-history.json" {
                                                // Parse and merge chat history
                                                if let Ok(json_str) = std::str::from_utf8(&data) {
                                                    if let Some(history) = crate::events::ChatHistoryContent::parse(json_str) {
                                                        let received_messages = history.to_messages();
                                                        let msg_count = received_messages.len();

                                                        let mut s = write_state(&state);
                                                        // Merge: add messages with new UUIDs
                                                        let existing_ids: std::collections::HashSet<[u8; 16]> =
                                                            s.chat_messages.iter().map(|m| m.id).collect();

                                                        let mut added = 0;
                                                        for msg in received_messages {
                                                            if !existing_ids.contains(&msg.id) {
                                                                s.chat_messages.push(msg);
                                                                added += 1;
                                                            }
                                                        }

                                                        // Sort by timestamp
                                                        s.chat_messages.sort_by_key(|m| m.timestamp);

                                                        // Add system message about the sync
                                                        s.chat_messages.push(crate::events::ChatMessage {
                                                            id: uuid::Uuid::new_v4().into_bytes(),
                                                            sender: "System".to_string(),
                                                            text: format!(
                                                                "Synced chat history: {} new messages (of {} received)",
                                                                added, msg_count
                                                            ),
                                                            timestamp: SystemTime::now(),
                                                            is_local: true,
                                                        });
                                                        repaint();
                                                        return; // Done with chat history
                                                    }
                                                }
                                                error!("Failed to parse chat history JSON");
                                                return; // Don't process as regular file
                                            }

                                            // Regular file download
                                            if let Err(e) = tokio::fs::create_dir_all(&download_dir).await {
                                                error!("Failed to create download dir: {}", e);
                                            }

                                            let dest = download_dir.join(&name);
                                            match tokio::fs::write(&dest, &data).await {
                                                Ok(_) => {
                                                    let mut s = write_state(&state);
                                                    s.chat_messages.push(crate::events::ChatMessage {
                                                        id: uuid::Uuid::new_v4().into_bytes(),
                                                        sender: "System".to_string(),
                                                        text: format!(
                                                            "Downloaded {} via P2P to {}",
                                                            name,
                                                            dest.display()
                                                        ),
                                                        timestamp: SystemTime::now(),
                                                        is_local: true,
                                                    });
                                                    repaint();
                                                }
                                                Err(e) => {
                                                    error!("Failed to save P2P download: {}", e);
                                                    let mut s = write_state(&state);
                                                    s.chat_messages.push(crate::events::ChatMessage {
                                                        id: uuid::Uuid::new_v4().into_bytes(),
                                                        sender: "System".to_string(),
                                                        text: format!("Failed to save P2P download: {}", e),
                                                        timestamp: SystemTime::now(),
                                                        is_local: true,
                                                    });
                                                    repaint();
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            error!("Failed to download via P2P: {}", e);
                                            let msg = friendly_download_error(&e.to_string());
                                            let mut s = write_state(&state);
                                            s.chat_messages.push(crate::events::ChatMessage {
                                                id: uuid::Uuid::new_v4().into_bytes(),
                                                sender: "System".to_string(),
                                                text: msg,
                                                timestamp: SystemTime::now(),
                                                is_local: true,
                                            });
                                            repaint();
                                        }
                                    }
                                });

                                continue;
                            }
                        }

                        if let Some(tm) = &torrent_manager {
                            let tm = tm.clone();
                            let magnet = magnet.clone();
                            let state = state.clone();
                            let repaint = repaint.clone();
                            tokio::spawn(async move {
                                match tm.download_file(magnet).await {
                                    Ok(_) => {
                                        info!("Started download");
                                        let mut s = write_state(&state);
                                        s.chat_messages.push(crate::events::ChatMessage {
                                            id: uuid::Uuid::new_v4().into_bytes(),
                                            sender: "System".to_string(),
                                            text: "Started download".to_string(),
                                            timestamp: SystemTime::now(),
                                            is_local: true,
                                        });
                                        repaint();
                                    }
                                    Err(e) => {
                                        error!("Failed to download file: {}", e);
                                        let msg = friendly_download_error(&e.to_string());
                                        let mut s = write_state(&state);
                                        s.chat_messages.push(crate::events::ChatMessage {
                                            id: uuid::Uuid::new_v4().into_bytes(),
                                            sender: "System".to_string(),
                                            text: msg,
                                            timestamp: SystemTime::now(),
                                            is_local: true,
                                        });
                                        repaint();
                                    }
                                }
                            });
                        }
                    }

                    Command::PauseTransfer { infohash } => {
                        if let Some(tm) = &torrent_manager {
                            let tm = tm.clone();
                            let infohash = infohash.clone();
                            tokio::spawn(async move {
                                if let Err(e) = tm.pause_transfer(&infohash).await {
                                    error!("Failed to pause transfer: {}", e);
                                }
                            });
                        }
                    }

                    Command::ResumeTransfer { infohash } => {
                        if let Some(tm) = &torrent_manager {
                            let tm = tm.clone();
                            let infohash = infohash.clone();
                            tokio::spawn(async move {
                                if let Err(e) = tm.resume_transfer(&infohash).await {
                                    error!("Failed to resume transfer: {}", e);
                                }
                            });
                        }
                    }

                    Command::CancelTransfer { infohash } => {
                        if let Some(tm) = &torrent_manager {
                            let tm = tm.clone();
                            let infohash = infohash.clone();
                            tokio::spawn(async move {
                                if let Err(e) = tm.cancel_transfer(&infohash, false).await {
                                    error!("Failed to cancel transfer: {}", e);
                                }
                            });
                        }
                    }

                    Command::RemoveTransfer { infohash, delete_file } => {
                        if let Some(tm) = &torrent_manager {
                            let tm = tm.clone();
                            let infohash = infohash.clone();
                            tokio::spawn(async move {
                                if let Err(e) = tm.cancel_transfer(&infohash, delete_file).await {
                                    error!("Failed to remove transfer: {}", e);
                                }
                            });
                        }
                    }

                    Command::SaveFileAs { infohash, destination } => {
                        if let Some(tm) = &torrent_manager {
                            // Get source path and copy to destination
                            match tm.get_file_path(&infohash) {
                                Ok(source) => {
                                    let dest = destination.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = tokio::fs::copy(&source, &dest).await {
                                            error!("Failed to save file: {}", e);
                                        } else {
                                            info!("Saved file to: {:?}", dest);
                                        }
                                    });
                                }
                                Err(e) => {
                                    error!("Failed to get file path for save: {}", e);
                                }
                            }
                        }
                    }

                    Command::OpenFile { infohash } => {
                        if let Some(tm) = &torrent_manager {
                            match tm.get_file_path(&infohash) {
                                Ok(path) => {
                                    if let Err(e) = open::that(&path) {
                                        error!("Failed to open file: {}", e);
                                    } else {
                                        info!("Opened file: {:?}", path);
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to get file path for open: {}", e);
                                }
                            }
                        }
                    }

                    Command::UpdateFileTransferSettings { settings } => {
                        debug!("Updating file transfer settings: auto_download_enabled={}", settings.auto_download_enabled);
                        let mut s = write_state(&state);
                        s.file_transfer_settings = settings;
                        drop(s);
                        repaint();
                    }

                    Command::RequestChatHistory => {
                        // Send a chat history request to the room
                        if let Some(send) = &mut send_stream {
                            let request = crate::events::ChatHistoryRequestMessage::new();
                            let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
                            let timestamp_ms = SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as i64;

                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                    id: message_id,
                                    timestamp_ms,
                                    sender: client_name.clone(),
                                    text: request.to_json(),
                                })),
                            };
                            let frame = encode_frame(&env);
                            if let Err(e) = send.write_all(&frame).await {
                                error!("Failed to send chat history request: {}", e);
                            } else {
                                info!("Sent chat history request to room");
                                let mut s = write_state(&state);
                                s.chat_messages.push(crate::events::ChatMessage {
                                    id: uuid::Uuid::new_v4().into_bytes(),
                                    sender: "System".to_string(),
                                    text: "Requesting chat history from peers...".to_string(),
                                    timestamp: SystemTime::now(),
                                    is_local: true,
                                });
                                repaint();
                            }
                        }
                    }

                    Command::ShareChatHistory => {
                        // Share our chat history via P2P in response to a request
                        #[cfg(feature = "p2p")]
                        if let Some(p2p) = &p2p_manager {
                            // Serialize chat history (excluding local messages)
                            let history = {
                                let s = read_state(&state);
                                crate::events::ChatHistoryContent::from_messages(&s.chat_messages)
                            };

                            if history.messages.is_empty() {
                                debug!("No chat history to share");
                                continue;
                            }

                            let json = history.to_json();
                            let json_bytes = json.as_bytes();

                            // Write to temp file
                            let temp_path = std::env::temp_dir().join(format!(
                                "rumble-chat-history-{}.json",
                                uuid::Uuid::new_v4()
                            ));

                            if let Err(e) = std::fs::write(&temp_path, json_bytes) {
                                error!("Failed to write chat history temp file: {}", e);
                                continue;
                            }

                            // Share via P2P
                            match p2p.share_file(temp_path.clone()).await {
                                Ok(shared) => {
                                    let addrs = p2p.listen_addrs().await;
                                    let addr_strings: Vec<String> =
                                        addrs.iter().map(|a| a.to_string()).collect();

                                    // Create file message with chat history MIME type
                                    let msg = crate::events::P2pFileMessage::new(
                                        "chat-history.json".to_string(),
                                        shared.size,
                                        hex::encode(shared.id),
                                        p2p.peer_id().to_string(),
                                        addr_strings,
                                    );

                                    // Send as chat message
                                    if let Some(send) = &mut send_stream {
                                        let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
                                        let timestamp_ms = SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_millis() as i64;

                                        let env = proto::Envelope {
                                            state_hash: Vec::new(),
                                            payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                                id: message_id,
                                                timestamp_ms,
                                                sender: client_name.clone(),
                                                text: msg.to_json(),
                                            })),
                                        };
                                        let frame = encode_frame(&env);
                                        if let Err(e) = send.write_all(&frame).await {
                                            error!("Failed to send chat history share message: {}", e);
                                        } else {
                                            info!(
                                                "Shared chat history ({} messages, {} bytes)",
                                                history.messages.len(),
                                                shared.size
                                            );
                                        }
                                    }

                                    // Clean up temp file after a delay (let P2P transfer complete)
                                    let temp_path_clone = temp_path.clone();
                                    tokio::spawn(async move {
                                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                                        let _ = std::fs::remove_file(temp_path_clone);
                                    });
                                }
                                Err(e) => {
                                    error!("Failed to share chat history via P2P: {}", e);
                                    let _ = std::fs::remove_file(&temp_path);
                                }
                            }
                        }

                        #[cfg(not(feature = "p2p"))]
                        {
                            debug!("Chat history sharing requires P2P feature");
                        }
                    }

                    // PlaySfx is intercepted in BackendHandle::send() and never reaches here
                    Command::PlaySfx { .. } => {}
                }
            }
        }
    }
}

/// Ephemeral session identity used for libp2p PeerId binding.
struct SessionIdentity {
    signing_key: SigningKey,
    session_public_key: [u8; 32],
    session_id: [u8; 32],
    issued_ms: i64,
    expires_ms: i64,
}

#[cfg(feature = "p2p")]
async fn start_p2p_manager_from_session(
    session_signing: &SigningKey,
    config: &ConnectConfig,
) -> Option<Arc<P2PManager>> {
    let secret = identity::ed25519::SecretKey::try_from_bytes(session_signing.to_bytes()).ok()?;
    let kp = identity::Keypair::from(identity::ed25519::Keypair::from(secret));

    let listen_addrs: Vec<Multiaddr> = if config.p2p_listen_addrs.is_empty() {
        vec!["/ip4/0.0.0.0/tcp/0".parse().expect("p2p default listen addr")]
    } else {
        config.p2p_listen_addrs.clone()
    };

    match P2PManager::spawn(kp, listen_addrs, config.p2p_relay.clone()).await {
        Ok(manager) => Some(Arc::new(manager)),
        Err(err) => {
            error!(%err, "failed to start p2p manager");
            None
        }
    }
}

/// Send PeerCapabilities message to the server after P2P manager is initialized.
#[cfg(feature = "p2p")]
async fn send_peer_capabilities(send: &mut quinn::SendStream, p2p: Arc<P2PManager>) {
    // Wait a moment for listen addresses to be discovered
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let addrs = p2p.listen_addrs().await;
    let peer_id_bytes = p2p.peer_id().to_bytes();

    let capabilities = proto::PeerCapabilities {
        supports_file_transfer: true,
        supports_p2p_voice: false, // Not yet implemented
        prefer_relay: false,
        libp2p_peer_id: peer_id_bytes,
        multiaddrs: addrs.into_iter().map(|a| a.to_string()).collect(),
        bandwidth_tier: 0, // Auto
    };

    let envelope = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::PeerCapabilities(capabilities)),
    };
    let frame = encode_frame(&envelope);

    if let Err(e) = send.write_all(&frame).await {
        warn!("Failed to send PeerCapabilities: {}", e);
    } else {
        info!("Sent PeerCapabilities to server");
    }
}

/// Connect to a server and perform handshake with Ed25519 authentication.
///
/// Returns the negotiated connection handles plus the generated session
/// identity (ephemeral keypair) used for P2P features.
async fn connect_to_server(
    addr: &str,
    client_name: &str,
    public_key: &[u8; 32],
    signer: &SigningCallback,
    password: Option<&str>,
    config: &ConnectConfig,
    captured_cert: CapturedCert,
) -> anyhow::Result<(
    quinn::Connection,
    quinn::SendStream,
    quinn::RecvStream,
    BytesMut, // remaining buffer after handshake
    u64,      // user_id
    Vec<proto::RoomInfo>,
    Vec<proto::User>,
    SessionIdentity,
)> {
    use std::net::SocketAddr;
    use url::Url;

    const DEFAULT_PORT: u16 = 5000;

    info!(server_addr = %addr, client_name, "Connecting to server");

    // Parse address using URL crate with rumble:// scheme
    // Supports: "rumble://host:port", "rumble://host", "host:port", "host", IP addresses
    let addr_as_url = if addr.contains("://") {
        addr.to_string()
    } else {
        format!("rumble://{}", addr)
    };

    let url = Url::parse(&addr_as_url).map_err(|e| anyhow::anyhow!("Invalid server address: {}", e))?;

    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("No host in server address"))?;
    let port = url.port().unwrap_or(DEFAULT_PORT);

    let socket_addr: SocketAddr = format!("{}:{}", host, port)
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("Failed to resolve address: {}", e))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("No addresses found for hostname"))?;

    let endpoint = make_client_endpoint(socket_addr, config, captured_cert)?;

    let conn = endpoint.connect(socket_addr, "localhost")?.await?;
    info!(remote = %conn.remote_address(), "Connected to server");

    let (mut send, mut recv) = conn.open_bi().await?;
    info!("Opened bi stream");

    // Step 1: Send ClientHello with public key
    let hello = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ClientHello(proto::ClientHello {
            username: client_name.to_string(),
            public_key: public_key.to_vec(),
            password: password.map(|s| s.to_string()),
        })),
    };
    let frame = encode_frame(&hello);
    send.write_all(&frame).await?;
    debug!("Sent ClientHello");

    // Step 2: Wait for ServerHello with nonce
    let mut buf = BytesMut::new();
    let (nonce, user_id) = wait_for_server_hello(&mut recv, &mut buf).await?;
    info!(user_id, "Received ServerHello with nonce");

    // Step 3: Compute server certificate hash
    let server_cert_hash = compute_server_cert_hash(&conn);

    // Step 4: Generate session keypair and certificate signed by long-term key
    let session_secret: [u8; 32] = rand::random();
    let session_signing = ed25519_dalek::SigningKey::from_bytes(&session_secret);
    let session_public_bytes: [u8; 32] = session_signing.verifying_key().to_bytes();

    let timestamp_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let expires_ms = timestamp_ms + 24 * 60 * 60 * 1000; // 24h validity for session cert

    let cert_payload = build_session_cert_payload(&session_public_bytes, timestamp_ms, expires_ms, Some(client_name));
    let session_signature = signer(&cert_payload).map_err(|e| anyhow::anyhow!("Signing failed: {}", e))?;

    // Step 5: Compute signature payload for handshake
    let payload = build_auth_payload(&nonce, timestamp_ms, public_key, user_id, &server_cert_hash);

    // Step 6: Sign the handshake payload
    let signature = signer(&payload).map_err(|e| anyhow::anyhow!("Signing failed: {}", e))?;

    // Step 7: Send Authenticate (includes session certificate)
    let auth = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::Authenticate(proto::Authenticate {
            signature: signature.to_vec(),
            timestamp_ms,
            session_cert: Some(proto::SessionCertificate {
                session_public_key: session_public_bytes.to_vec(),
                issued_ms: timestamp_ms,
                expires_ms,
                device: Some(client_name.to_string()),
                user_signature: session_signature.to_vec(),
            }),
        })),
    };
    let frame = encode_frame(&auth);
    send.write_all(&frame).await?;
    debug!("Sent Authenticate");

    // Step 8: Wait for ServerState or AuthFailed
    let (rooms, users) = wait_for_auth_result(&mut recv, &mut buf).await?;

    let session_identity = SessionIdentity {
        signing_key: session_signing,
        session_public_key: session_public_bytes,
        session_id: compute_session_id(&session_public_bytes),
        issued_ms: timestamp_ms,
        expires_ms,
    };

    Ok((conn, send, recv, buf, user_id, rooms, users, session_identity))
}

/// Wait for ServerHello message and extract nonce and user_id.
async fn wait_for_server_hello(recv: &mut quinn::RecvStream, buf: &mut BytesMut) -> anyhow::Result<([u8; 32], u64)> {
    loop {
        let mut chunk = [0u8; 4096];
        match recv.read(&mut chunk).await? {
            Some(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(frame) = try_decode_frame(buf) {
                    if let Ok(env) = proto::Envelope::decode(&*frame) {
                        match env.payload {
                            Some(Payload::ServerHello(sh)) => {
                                if sh.nonce.len() != 32 {
                                    return Err(anyhow::anyhow!("Invalid nonce length in ServerHello"));
                                }
                                let nonce: [u8; 32] = sh.nonce.try_into().unwrap();
                                return Ok((nonce, sh.user_id));
                            }
                            Some(Payload::AuthFailed(af)) => {
                                return Err(anyhow::anyhow!("Authentication failed: {}", af.error));
                            }
                            _ => {}
                        }
                    }
                }
            }
            None => {
                return Err(anyhow::anyhow!("Server closed connection during handshake"));
            }
        }
    }
}

/// Wait for authentication result (ServerState or AuthFailed).
async fn wait_for_auth_result(
    recv: &mut quinn::RecvStream,
    buf: &mut BytesMut,
) -> anyhow::Result<(Vec<proto::RoomInfo>, Vec<proto::User>)> {
    loop {
        let mut chunk = [0u8; 4096];
        match recv.read(&mut chunk).await? {
            Some(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(frame) = try_decode_frame(buf) {
                    if let Ok(env) = proto::Envelope::decode(&*frame) {
                        match env.payload {
                            Some(Payload::AuthFailed(af)) => {
                                return Err(anyhow::anyhow!("Authentication failed: {}", af.error));
                            }
                            Some(Payload::ServerEvent(se)) => {
                                if let Some(proto::server_event::Kind::ServerState(ss)) = se.kind {
                                    return Ok((ss.rooms, ss.users));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            None => {
                return Err(anyhow::anyhow!("Server closed connection during authentication"));
            }
        }
    }
}

/// Compute the SHA256 hash of the server's TLS certificate from the connection.
fn compute_server_cert_hash(conn: &quinn::Connection) -> [u8; 32] {
    // Get peer certificates from the connection
    if let Some(peer_identity) = conn.peer_identity() {
        if let Some(certs) = peer_identity.downcast_ref::<Vec<rustls::pki_types::CertificateDer<'_>>>() {
            if let Some(cert) = certs.first() {
                return compute_cert_hash(cert.as_ref());
            }
        }
    }
    // Fallback: return zeros (should not happen with proper TLS)
    warn!("Could not get server certificate for hash computation");
    [0u8; 32]
}

/// Background task that receives reliable messages from the server.
///
/// This task handles:
/// - Server events (state updates, user joins/leaves, chat messages)
/// - Connection loss detection
///
/// Voice datagrams are handled by the audio task, not here.
async fn run_receiver_task(
    conn: quinn::Connection,
    mut recv: quinn::RecvStream,
    mut buf: BytesMut,
    state: Arc<RwLock<State>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    audio_task: AudioTaskHandle,
    torrent_manager: Option<Arc<crate::torrent::TorrentManager>>,
    command_tx: mpsc::UnboundedSender<Command>,
    shared_infohashes: std::collections::HashSet<String>,
) {
    loop {
        let mut chunk = [0u8; 4096];
        match recv.read(&mut chunk).await {
            Ok(Some(n)) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(frame) = try_decode_frame(&mut buf) {
                    if let Ok(env) = proto::Envelope::decode(&*frame) {
                        handle_server_message(
                            env,
                            &state,
                            &repaint,
                            &audio_task,
                            &torrent_manager,
                            &command_tx,
                            &shared_infohashes,
                        );
                    }
                }
            }
            Ok(None) => {
                // Stream closed normally
                info!("Server closed the receive stream");
                break;
            }
            Err(e) => {
                warn!("Read error on receive stream: {}", e);
                break;
            }
        }
    }

    // Connection closed or error - wait for the actual connection close
    let error = conn.closed().await;

    warn!("Connection closed: {}", error);

    // Notify audio task
    audio_task.send(AudioCommand::ConnectionClosed);

    // Update state only if not already disconnected (explicit disconnect sets Disconnected)
    {
        let mut s = write_state(&state);
        if !matches!(s.connection, ConnectionState::Disconnected) {
            s.connection = ConnectionState::ConnectionLost {
                error: error.to_string(),
            };
            s.my_user_id = None;
            s.my_room_id = None;
            s.my_session_public_key = None;
            s.my_session_id = None;
            s.rooms.clear();
            s.users.clear();
            s.rebuild_room_tree();
        }
    }
    repaint();
}

/// Add a local status message to the chat.
fn add_local_message(state: &Arc<RwLock<State>>, text: String, repaint: &Arc<dyn Fn() + Send + Sync>) {
    let mut s = write_state(&state);
    s.chat_messages.push(crate::events::ChatMessage {
        id: uuid::Uuid::new_v4().into_bytes(),
        sender: String::new(),
        text,
        timestamp: std::time::SystemTime::now(),
        is_local: true,
    });
    // Keep only recent messages
    if s.chat_messages.len() > 100 {
        s.chat_messages.remove(0);
    }
    drop(s);
    repaint();
}

/// Handle an incoming server message and update state accordingly.
fn handle_server_message(
    env: proto::Envelope,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
    audio_task: &AudioTaskHandle,
    torrent_manager: &Option<Arc<crate::torrent::TorrentManager>>,
    command_tx: &mpsc::UnboundedSender<Command>,
    shared_infohashes: &std::collections::HashSet<String>,
) {
    match env.payload {
        Some(Payload::TrackerAnnounceResponse(_)) => {
            // TrackerAnnounceResponse is handled inline in the announce loop
            // This branch handles any stray responses (shouldn't happen)
        }
        Some(Payload::CommandResult(cr)) => {
            // Display command results as local chat messages
            let prefix = if cr.success { "✓" } else { "✗" };
            add_local_message(state, format!("{} {}", prefix, cr.message), repaint);
        }
        Some(Payload::ServerEvent(se)) => {
            if let Some(kind) = se.kind {
                match kind {
                    proto::server_event::Kind::ServerState(ss) => {
                        // Full state replacement
                        let mut s = write_state(&state);
                        s.rooms = ss.rooms;
                        s.users = ss.users.clone();
                        s.rebuild_room_tree();

                        // Notify audio task about users in our room (for proactive decoder creation)
                        if let Some(my_room_id) = &s.my_room_id {
                            let my_user_id = s.my_user_id;
                            let user_ids_in_room: Vec<u64> = ss
                                .users
                                .iter()
                                .filter_map(|u| {
                                    let user_id = u.user_id.as_ref().map(|id| id.value)?;
                                    let user_room = u.current_room.as_ref().and_then(api::uuid_from_room_id)?;
                                    if user_room == *my_room_id && Some(user_id) != my_user_id {
                                        Some(user_id)
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            drop(s);
                            audio_task.send(AudioCommand::RoomChanged { user_ids_in_room });
                        } else {
                            drop(s);
                        }
                        repaint();
                    }
                    proto::server_event::Kind::StateUpdate(su) => {
                        apply_state_update(su, state, repaint, audio_task);
                    }
                    proto::server_event::Kind::ChatBroadcast(cb) => {
                        // Extract message ID or generate a fallback
                        let id: [u8; 16] = cb.id.try_into().unwrap_or_else(|_| uuid::Uuid::new_v4().into_bytes());

                        // Convert timestamp_ms to SystemTime, fall back to now
                        let timestamp = if cb.timestamp_ms > 0 {
                            std::time::UNIX_EPOCH + std::time::Duration::from_millis(cb.timestamp_ms as u64)
                        } else {
                            std::time::SystemTime::now()
                        };

                        // Check if this is a chat history request
                        if crate::events::ChatHistoryRequestMessage::parse(&cb.text).is_some() {
                            debug!("Received chat history request from {}", cb.sender);
                            // Trigger sharing our chat history
                            let _ = command_tx.send(Command::ShareChatHistory);
                            // Don't add this message to chat - it's a protocol message
                            return;
                        }

                        // Check if this is a P2P chat history file
                        if let Some(p2p_msg) = crate::events::P2pFileMessage::parse(&cb.text) {
                            if p2p_msg.file.name == "chat-history.json" {
                                debug!("Received chat history file from {}", cb.sender);
                                // Send command to download and merge history
                                let _ = command_tx.send(Command::DownloadFile {
                                    magnet: p2p_msg.magnet_link(),
                                });
                                // Don't add this message to chat - it's a protocol message
                                return;
                            }
                        }

                        // Check if this is a file message that should be auto-downloaded
                        // Skip auto-download if we're already seeding this file
                        let mut should_auto_download = None;
                        if let Some(file_msg) = crate::events::FileMessage::parse(&cb.text) {
                            let already_shared = shared_infohashes.contains(&file_msg.file.infohash);

                            let s = read_state(&state);
                            if !already_shared
                                && s.file_transfer_settings
                                    .should_auto_download(&file_msg.file.mime, file_msg.file.size)
                            {
                                // Check we're not already downloading this file
                                let infohash_bytes = hex::decode(&file_msg.file.infohash).ok();
                                let already_downloading = infohash_bytes
                                    .as_ref()
                                    .map(|ih| {
                                        if ih.len() == 20 {
                                            let mut arr = [0u8; 20];
                                            arr.copy_from_slice(ih);
                                            s.file_transfers.iter().any(|t| t.infohash == arr)
                                        } else {
                                            false
                                        }
                                    })
                                    .unwrap_or(false);

                                if !already_downloading {
                                    should_auto_download = Some(file_msg.magnet_link());
                                    debug!(
                                        "Auto-downloading file: {} ({} bytes, {})",
                                        file_msg.file.name, file_msg.file.size, file_msg.file.mime
                                    );
                                }
                            }
                            drop(s);
                        }

                        let mut s = write_state(&state);
                        s.chat_messages.push(crate::events::ChatMessage {
                            id,
                            sender: cb.sender,
                            text: cb.text,
                            timestamp,
                            is_local: false,
                        });
                        // Keep only recent messages
                        if s.chat_messages.len() > 100 {
                            s.chat_messages.remove(0);
                        }
                        drop(s);
                        repaint();

                        // Trigger auto-download if conditions were met
                        if let (Some(magnet), Some(tm)) = (should_auto_download, torrent_manager) {
                            let tm = tm.clone();
                            let state = state.clone();
                            let repaint = repaint.clone();
                            tokio::spawn(async move {
                                match tm.download_file(magnet).await {
                                    Ok(_) => {
                                        info!("Auto-download started");
                                        let mut s = write_state(&state);
                                        s.chat_messages.push(crate::events::ChatMessage {
                                            id: uuid::Uuid::new_v4().into_bytes(),
                                            sender: "System".to_string(),
                                            text: "Auto-download started".to_string(),
                                            timestamp: std::time::SystemTime::now(),
                                            is_local: true,
                                        });
                                        repaint();
                                    }
                                    Err(e) => {
                                        error!("Auto-download failed: {}", e);
                                        let mut s = write_state(&state);
                                        s.chat_messages.push(crate::events::ChatMessage {
                                            id: uuid::Uuid::new_v4().into_bytes(),
                                            sender: "System".to_string(),
                                            text: format!("Auto-download failed: {}", e),
                                            timestamp: std::time::SystemTime::now(),
                                            is_local: true,
                                        });
                                        repaint();
                                    }
                                }
                            });
                        }
                    }
                    proto::server_event::Kind::KeepAlive(_) => {
                        // Ignore keep-alive for now
                    }
                }
            }
        }
        Some(Payload::PeerAnnounce(pa)) => {
            handle_peer_announce(pa, state, repaint);
        }
        Some(Payload::RelayAllocation(ra)) => {
            handle_relay_allocation(ra, state, repaint);
        }
        _ => {}
    }
}

/// Apply a state update to the current state.
fn apply_state_update(
    update: proto::StateUpdate,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
    audio_task: &AudioTaskHandle,
) {
    if let Some(u) = update.update {
        let mut s = write_state(&state);
        match u {
            proto::state_update::Update::RoomCreated(rc) => {
                if let Some(room) = rc.room {
                    s.rooms.push(room);
                    s.rebuild_room_tree();
                }
            }
            proto::state_update::Update::RoomDeleted(rd) => {
                if let Some(rid) = rd.room_id.and_then(|r| api::uuid_from_room_id(&r)) {
                    s.rooms
                        .retain(|r| r.id.as_ref().and_then(api::uuid_from_room_id) != Some(rid));
                    s.rebuild_room_tree();
                }
            }
            proto::state_update::Update::RoomRenamed(rr) => {
                if let Some(rid) = rr.room_id.and_then(|r| api::uuid_from_room_id(&r)) {
                    if let Some(room) = s
                        .rooms
                        .iter_mut()
                        .find(|r| r.id.as_ref().and_then(api::uuid_from_room_id) == Some(rid))
                    {
                        room.name = rr.new_name;
                    }
                    s.rebuild_room_tree();
                }
            }
            proto::state_update::Update::RoomMoved(rm) => {
                if let Some(rid) = rm.room_id.and_then(|r| api::uuid_from_room_id(&r)) {
                    if let Some(room) = s
                        .rooms
                        .iter_mut()
                        .find(|r| r.id.as_ref().and_then(api::uuid_from_room_id) == Some(rid))
                    {
                        room.parent_id = rm.new_parent_id;
                    }
                    s.rebuild_room_tree();
                }
            }
            proto::state_update::Update::UserJoined(uj) => {
                if let Some(user) = uj.user {
                    // Only add if user doesn't already exist (avoid duplicates from
                    // receiving our own UserJoined broadcast after initial ServerState)
                    let user_id_value = user.user_id.as_ref().map(|id| id.value);
                    let already_exists = s
                        .users
                        .iter()
                        .any(|u| u.user_id.as_ref().map(|id| id.value) == user_id_value);
                    if !already_exists {
                        // Check if this user is joining our room - if so, notify audio task
                        let my_room_id = s.my_room_id;
                        let user_room = user.current_room.as_ref().and_then(api::uuid_from_room_id);
                        let notify_audio = user_id_value.is_some() && my_room_id.is_some() && user_room == my_room_id;

                        s.users.push(user);

                        if notify_audio {
                            if let Some(uid) = user_id_value {
                                drop(s);
                                audio_task.send(AudioCommand::UserJoinedRoom { user_id: uid });
                                repaint();
                                return;
                            }
                        }
                    }
                }
            }
            proto::state_update::Update::UserLeft(ul) => {
                if let Some(uid) = ul.user_id {
                    // Check if the leaving user was in our room
                    let my_room_id = s.my_room_id;
                    let was_in_our_room = s
                        .users
                        .iter()
                        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                        .map(|u| u.current_room.as_ref().and_then(api::uuid_from_room_id) == my_room_id)
                        .unwrap_or(false);

                    s.users
                        .retain(|u| u.user_id.as_ref().map(|id| id.value) != Some(uid.value));

                    // Notify audio task if user was in our room
                    if was_in_our_room && my_room_id.is_some() {
                        drop(s);
                        audio_task.send(AudioCommand::UserLeftRoom { user_id: uid.value });
                        repaint();
                        return;
                    }
                }
            }
            proto::state_update::Update::UserMoved(um) => {
                if let (Some(uid), Some(to_room)) = (um.user_id.clone(), um.to_room_id.clone()) {
                    let to_room_clone = to_room.clone();
                    let to_room_id = api::uuid_from_room_id(&to_room_clone);
                    let my_room_id = s.my_room_id;
                    let my_user_id = s.my_user_id;

                    // Look up where the user was before (from_room is implicit in User.current_room)
                    let from_room_id = s
                        .users
                        .iter()
                        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                        .and_then(|u| u.current_room.as_ref().and_then(api::uuid_from_room_id));

                    // Now update the user's current room
                    if let Some(user) = s
                        .users
                        .iter_mut()
                        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                    {
                        user.current_room = Some(to_room);
                    }

                    // Check if this is us moving
                    if my_user_id == Some(uid.value) {
                        s.my_room_id = to_room_id;

                        // We changed rooms - rebuild decoder list
                        if let Some(new_room_id) = to_room_id {
                            let user_ids_in_room: Vec<u64> = s
                                .users
                                .iter()
                                .filter_map(|u| {
                                    let user_id = u.user_id.as_ref().map(|id| id.value)?;
                                    let user_room = u.current_room.as_ref().and_then(api::uuid_from_room_id)?;
                                    if user_room == new_room_id && Some(user_id) != my_user_id {
                                        Some(user_id)
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            drop(s);
                            audio_task.send(AudioCommand::RoomChanged { user_ids_in_room });
                            repaint();
                            return;
                        }
                    } else {
                        // Another user moved - check if they joined/left our room
                        let joined_our_room = my_room_id.is_some() && to_room_id == my_room_id;
                        let left_our_room = my_room_id.is_some() && from_room_id == my_room_id;

                        if joined_our_room {
                            drop(s);
                            audio_task.send(AudioCommand::UserJoinedRoom { user_id: uid.value });
                            repaint();
                            return;
                        } else if left_our_room {
                            drop(s);
                            audio_task.send(AudioCommand::UserLeftRoom { user_id: uid.value });
                            repaint();
                            return;
                        }
                    }
                }
            }
            proto::state_update::Update::UserStatusChanged(usc) => {
                if let Some(uid) = usc.user_id {
                    if let Some(user) = s
                        .users
                        .iter_mut()
                        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                    {
                        user.is_muted = usc.is_muted;
                        user.is_deafened = usc.is_deafened;
                    }
                }
            }
        }
        drop(s);
        repaint();
    }
}

/// Handle a PeerAnnounce message from the server.
/// This updates our knowledge of P2P peers.
fn handle_peer_announce(
    announce: proto::PeerAnnounce,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    let user_id = announce.user_id.as_ref().map(|u| u.value).unwrap_or(0);

    if announce.is_removal {
        // Peer left - remove from our list
        let mut s = write_state(&state);
        s.p2p_peers.remove(&user_id);
        info!(user_id, "P2P peer removed");
        drop(s);
        repaint();
        return;
    }

    // Convert session_id to fixed array
    let session_id: [u8; 32] = announce.session_id.try_into().unwrap_or_else(|_| [0u8; 32]);

    let peer_info = crate::events::PeerInfo {
        user_id,
        session_id,
        libp2p_peer_id: announce.libp2p_peer_id,
        multiaddrs: announce.multiaddrs,
        supports_relay: announce.supports_relay,
        supports_p2p_voice: announce.supports_p2p_voice,
    };

    let mut s = write_state(&state);
    s.p2p_peers.insert(user_id, peer_info);
    info!(
        user_id,
        multiaddrs_count = s.p2p_peers.get(&user_id).map(|p| p.multiaddrs.len()).unwrap_or(0),
        "P2P peer announced"
    );
    drop(s);
    repaint();
}

/// Handle a RelayAllocation message from the server.
/// This provides us with relay details for NAT traversal.
fn handle_relay_allocation(
    allocation: proto::RelayAllocation,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) {
    info!(
        relay_multiaddr = %allocation.relay_multiaddr,
        expires_ms = allocation.expires_ms,
        rate_limit = allocation.rate_limit_bytes_per_sec,
        "Received relay allocation"
    );

    // TODO: Store relay allocation for use with P2P connections
    // For now, just log it. When P2P voice is implemented, we'll use this
    // to configure the libp2p swarm with the relay address.

    let _ = (state, repaint); // Suppress unused warnings for now
}

/// Create a QUIC client endpoint with custom certificate verification.
///
/// This endpoint uses an interactive certificate verifier that:
/// 1. Accepts certificates from standard webpki roots
/// 2. Accepts certificates from configured additional_certs paths
/// 3. Accepts certificates from the accepted_certs list (user-accepted)
/// 4. For other self-signed certs, stores the cert in `captured_cert` and returns an error
///    that allows the UI to prompt the user for acceptance
fn make_client_endpoint(
    remote_addr: std::net::SocketAddr,
    config: &ConnectConfig,
    captured_cert: CapturedCert,
) -> anyhow::Result<Endpoint> {
    // Bind to the same address family as the remote address
    let bind_addr: std::net::SocketAddr = if remote_addr.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    let mut endpoint = Endpoint::client(bind_addr)?;

    // Start with webpki system roots
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Load additional configured certificates from files (supports both PEM and DER formats)
    for cert_path in &config.additional_certs {
        match std::fs::read(cert_path) {
            Ok(cert_bytes) => {
                // Try to detect if it's PEM format (starts with "-----BEGIN")
                if cert_bytes.starts_with(b"-----BEGIN") {
                    // Parse as PEM - may contain multiple certificates
                    let mut reader = std::io::BufReader::new(cert_bytes.as_slice());
                    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
                        rustls_pemfile::certs(&mut reader).filter_map(|r| r.ok()).collect();
                    for cert in certs {
                        let _ = root_store.add(cert);
                    }
                    info!("Loaded PEM certificate(s) from {:?}", cert_path);
                } else {
                    // Assume DER format
                    let cert = rustls::pki_types::CertificateDer::from(cert_bytes);
                    let _ = root_store.add(cert);
                    info!("Loaded DER certificate from {:?}", cert_path);
                }
            }
            Err(e) => {
                error!("Failed to load cert from {:?}: {}", cert_path, e);
            }
        }
    }

    // Add user-accepted certificates (from interactive prompts)
    for cert_der in &config.accepted_certs {
        let cert = rustls::pki_types::CertificateDer::from(cert_der.clone());
        if let Err(e) = root_store.add(cert) {
            warn!("Failed to add accepted certificate to root store: {:?}", e);
        } else {
            debug!("Added user-accepted certificate to trust store");
        }
    }

    // Create the crypto provider
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

    // Create the custom interactive verifier with the shared captured cert storage
    let verifier = Arc::new(InteractiveCertVerifier::new(
        root_store,
        provider.clone(),
        captured_cert,
    ));

    // Build client config with the custom verifier using dangerous() API
    let mut client_cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();

    client_cfg.alpn_protocols = vec![b"rumble".to_vec()];
    let rustls_config = Arc::new(client_cfg);
    let crypto = QuicClientConfig::try_from(rustls_config)?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));

    // Configure transport
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into().unwrap()));
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport_config.datagram_receive_buffer_size(Some(65536));
    client_config.transport_config(Arc::new(transport_config));

    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}
