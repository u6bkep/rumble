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

use crate::{
    ConnectConfig,
    audio_dump::AudioDumper,
    audio_task::{AudioCommand, AudioTaskConfig, AudioTaskHandle, spawn_audio_task},
    events::{AudioState, Command, ConnectionState, PendingCertificate, State, VoiceMode},
};
use ed25519_dalek::SigningKey;
use prost::Message;
use rumble_client_traits::{
    AudioBackend, FileTransferPlugin, KeySigning, Platform, StreamHeader,
    auth::{send_envelope, wait_for_auth_result, wait_for_server_hello},
    cert::{CapturedCert, is_cert_error_message, new_captured_cert, take_captured_cert},
    file_transfer::{TransferId, TransferStatus},
    transport::{BiStreamHandle, TlsConfig, Transport, TransportRecvStream},
};
use rumble_protocol::{
    ROOT_ROOM_UUID, build_auth_payload, build_session_cert_payload, compute_cert_hash, compute_session_id,
    proto::{self, envelope::Payload},
    room_id_from_uuid,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// A handle to the backend that can be used from UI code.
///
/// This type manages the tokio runtime, async connection, audio subsystem,
/// and provides a state-driven interface for the UI. The UI reads state
/// via `state()` and sends commands via `send()`.
pub struct BackendHandle<P: Platform> {
    /// Shared state that the UI reads.
    state: Arc<RwLock<State>>,
    /// Channel to send commands to the connection task.
    command_tx: mpsc::UnboundedSender<Command>,
    /// Handle to send commands to the audio task.
    audio_task: AudioTaskHandle,
    /// Active file transfer plugin, when connected. The connection
    /// task installs this when the QUIC handshake completes and
    /// clears it on disconnect / connection loss. Read by the UI
    /// to enumerate / cancel / locate transfers without going
    /// through the command channel.
    file_transfer: Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>>,
    /// Background thread running the tokio runtime for connection task.
    _runtime_thread: std::thread::JoinHandle<()>,
    /// Connection configuration (certificates, etc.). stored incase we want to inspect it later.
    _connect_config: ConnectConfig,
    /// Pre-generated sound effects library.
    sfx_library: crate::sfx::SfxLibrary,
    /// Marker for the platform type parameter.
    _phantom: std::marker::PhantomData<P>,
}

impl<P: Platform> BackendHandle<P> {
    /// Create a new backend handle with the given repaint callback and key
    /// signer. The signer is invoked during the QUIC handshake to produce
    /// auth signatures; see [`KeySigning`].
    pub fn new<F>(repaint_callback: F, key_signer: Arc<dyn KeySigning>) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self::with_config_and_dumper(
            repaint_callback,
            ConnectConfig::new(),
            key_signer,
            None,
            P::AudioBackend::default(),
        )
    }

    /// Create a new backend handle with a repaint callback, connect config,
    /// and key signer.
    pub fn with_config<F>(repaint_callback: F, connect_config: ConnectConfig, key_signer: Arc<dyn KeySigning>) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self::with_config_and_dumper(
            repaint_callback,
            connect_config,
            key_signer,
            None,
            P::AudioBackend::default(),
        )
    }

    /// Create a new backend handle with audio dumping enabled.
    ///
    /// Audio dumping writes raw audio data to files for debugging:
    /// - `mic_raw.pcm` - Raw microphone input (f32 samples)
    /// - `tx_opus.bin` - Encoded opus packets being sent
    /// - `rx_opus.bin` - Received opus packets
    /// - `rx_decoded.pcm` - Decoded audio before mixing (f32 samples)
    pub fn with_audio_dumper<F>(
        repaint_callback: F,
        connect_config: ConnectConfig,
        key_signer: Arc<dyn KeySigning>,
        audio_dumper: AudioDumper,
    ) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self::with_config_and_dumper(
            repaint_callback,
            connect_config,
            key_signer,
            Some(audio_dumper),
            P::AudioBackend::default(),
        )
    }

    /// Create a new backend handle with a caller-supplied audio backend.
    ///
    /// Tests use this to inject a `MockAudioBackend` so the engine runs
    /// against fake capture/playback instead of opening a real device.
    pub fn with_audio_backend<F>(
        repaint_callback: F,
        connect_config: ConnectConfig,
        key_signer: Arc<dyn KeySigning>,
        audio_backend: P::AudioBackend,
    ) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self::with_config_and_dumper(repaint_callback, connect_config, key_signer, None, audio_backend)
    }

    /// Internal constructor with optional audio dumper.
    fn with_config_and_dumper<F>(
        repaint_callback: F,
        connect_config: ConnectConfig,
        key_signer: Arc<dyn KeySigning>,
        audio_dumper: Option<AudioDumper>,
        audio_backend: P::AudioBackend,
    ) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        // Check for audio dump env var if no explicit dumper provided
        let audio_dumper =
            audio_dumper.or_else(|| crate::audio_dump::AudioDumpConfig::from_env().map(AudioDumper::new));

        let repaint_callback = Arc::new(repaint_callback);
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        // Snapshot device lists from the supplied backend before handing it
        // to the audio task.
        let input_devices = audio_backend.list_input_devices();
        let output_devices = audio_backend.list_output_devices();

        // Initialize state with audio info
        let state = State {
            connection: ConnectionState::Disconnected,
            rooms: Vec::new(),
            users: Vec::new(),
            my_user_id: None,
            my_room_id: None,
            my_session_public_key: None,
            my_session_id: None,
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
            effective_permissions: 0,
            per_room_permissions: HashMap::new(),
            permission_denied: None,
            kicked: None,
            group_definitions: vec![],
        };

        let state = Arc::new(RwLock::new(state));
        let file_transfer: Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>> = Arc::new(RwLock::new(None));

        // Spawn the audio task (runs on its own thread)
        let audio_task = spawn_audio_task::<P>(AudioTaskConfig {
            state: state.clone(),
            repaint: repaint_callback.clone(),
            audio_dumper,
            audio_backend,
        });

        // Clone handles for the connection task
        let state_for_task = state.clone();
        let repaint_for_task = repaint_callback.clone();
        let config_for_task = connect_config.clone();
        let audio_task_for_connection = audio_task.clone();
        let command_tx_for_task = command_tx.clone();
        let file_transfer_for_task = file_transfer.clone();

        // Spawn background thread with tokio runtime for connection task
        let key_signer_for_task = key_signer.clone();
        let runtime_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async move {
                run_connection_task::<P>(
                    command_rx,
                    command_tx_for_task,
                    state_for_task,
                    repaint_for_task,
                    config_for_task,
                    key_signer_for_task,
                    audio_task_for_connection,
                    file_transfer_for_task,
                )
                .await;
            });
        });

        Self {
            state,
            command_tx,
            audio_task,
            file_transfer,
            _runtime_thread: runtime_thread,
            _connect_config: connect_config,
            sfx_library: crate::sfx::SfxLibrary::new(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get the current state for rendering.
    ///
    /// This returns a clone of the state. The UI should call this
    /// in its render loop to get the latest state.
    pub fn state(&self) -> State {
        read_state(&self.state).clone()
    }

    /// Get a mutable write guard to the state for clearing one-shot fields.
    pub fn state_mut(&self) -> RwLockWriteGuard<'_, State> {
        write_state(&self.state)
    }

    /// Get a reference to the shared state Arc (for RPC server).
    pub fn state_arc(&self) -> &Arc<RwLock<State>> {
        &self.state
    }

    /// Get a reference to the command sender (for RPC server).
    pub fn command_sender(&self) -> &mpsc::UnboundedSender<Command> {
        &self.command_tx
    }

    /// Start the RPC server on the given Unix socket path.
    #[cfg(unix)]
    pub fn start_rpc_server(&self, socket_path: std::path::PathBuf) -> anyhow::Result<crate::rpc::RpcServer> {
        crate::rpc::RpcServer::start(socket_path, self.state.clone(), self.command_tx.clone())
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

    /// Snapshot the current set of file transfers (uploads + downloads).
    /// Returns an empty vec when no plugin is installed (i.e. while
    /// disconnected). The plugin's own internal lock is held briefly.
    pub fn transfers(&self) -> Vec<TransferStatus> {
        let guard = match self.file_transfer.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.as_ref().map(|ft| ft.transfers()).unwrap_or_default()
    }

    /// Cancel an in-flight transfer. `delete_files=true` also removes
    /// any partially-downloaded data from disk.
    pub fn cancel_transfer(&self, id: &TransferId, delete_files: bool) -> anyhow::Result<()> {
        let guard = match self.file_transfer.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match guard.as_ref() {
            Some(ft) => ft.cancel(id, delete_files),
            None => anyhow::bail!("file transfer plugin not available (not connected)"),
        }
    }

    /// Local file path for a completed transfer, if known. Used by the
    /// UI's "show in folder" / "open" affordances on the transfers
    /// panel and the chat-card download button.
    pub fn transfer_file_path(&self, id: &TransferId) -> Option<std::path::PathBuf> {
        let guard = match self.file_transfer.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.as_ref().and_then(|ft| ft.get_file_path(id).ok())
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
#[allow(clippy::too_many_arguments)] // private helper; bundling its args would just push churn
async fn run_connection_task<P: Platform>(
    mut command_rx: mpsc::UnboundedReceiver<Command>,
    command_tx: mpsc::UnboundedSender<Command>,
    state: Arc<RwLock<State>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    config: ConnectConfig,
    key_signer: Arc<dyn KeySigning>,
    audio_task: AudioTaskHandle,
    file_transfer_slot: Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>>,
) {
    // Connection state
    let mut transport: Option<P::Transport> = None;
    let mut client_name = String::new();
    let mut _session_identity: Option<SessionIdentity> = None;
    let mut file_transfer: Option<Arc<dyn FileTransferPlugin>> = None;
    // Write the active plugin into the shared slot so the UI thread
    // can call `transfers()` / `cancel()` / `get_file_path()` directly
    // without going through the command channel.
    let publish_ft = |ft: &Option<Arc<dyn FileTransferPlugin>>| {
        let mut slot = match file_transfer_slot.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *slot = ft.clone();
    };

    loop {
        tokio::select! {
            cmd = command_rx.recv() => {
                let Some(cmd) = cmd else {
                    // Channel closed, handle is dropped, exit task
                    debug!("Command channel closed, shutting down connection task");
                    if let Some(t) = transport.take() {
                        t.close().await;
                    }
                    return;
                };
                match cmd {
                    Command::Connect { addr, name, public_key, password } => {
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
                        match connect_to_server::<P::Transport>(&addr, &name, &public_key, key_signer.as_ref(), password.as_deref(), &config, captured_cert.clone()).await {
                            Ok((mut new_transport, user_id, rooms, users, groups, session_info)) => {
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
                                        .and_then(rumble_protocol::uuid_from_room_id)
                                        .or(Some(ROOT_ROOM_UUID));
                                    s.my_session_public_key = Some(session_info.session_public_key);
                                    s.my_session_id = Some(session_info.session_id);
                                    s.rooms = rooms;
                                    s.users = users;
                                    s.group_definitions = groups;
                                    s.rebuild_room_tree();
                                }
                                recalculate_effective_permissions(&state);
                                repaint();

                                // Notify audio task of new connection
                                audio_task.send(AudioCommand::ConnectionEstablished {
                                    datagram: Arc::new(new_transport.datagram_handle()),
                                    my_user_id: user_id,
                                });

                                _session_identity = Some(session_info);

                                // Split off the receive stream for the receiver task
                                let recv_stream = new_transport.take_recv();

                                // Get bi-stream handles: one for dispatch, one for the relay plugin
                                let bi_handle = new_transport.bi_stream_handle();
                                let opener_handle = new_transport.bi_stream_handle();

                                transport = Some(new_transport);

                                // Create the file transfer relay plugin (if platform supports it)
                                let downloads_dir = config
                                    .download_dir
                                    .clone()
                                    .unwrap_or_else(|| std::env::temp_dir().join("rumble_downloads"));
                                let opener: Arc<dyn rumble_client_traits::StreamOpener> =
                                    Arc::new(rumble_client_traits::BiStreamOpener::new(opener_handle));
                                let ft_arc: Option<Arc<dyn FileTransferPlugin>> =
                                    P::create_file_transfer_plugin(opener, downloads_dir);

                                // Set the initial room ID on the relay plugin
                                if let Some(ft) = &ft_arc
                                    && let Some(room_uuid) = read_state(&state).my_room_id {
                                        ft.set_room_id(room_uuid.to_string());
                                    }

                                // Keep a reference for room-change updates,
                                // and publish to the shared slot so UI threads
                                // can call into the plugin.
                                file_transfer = ft_arc.clone();
                                publish_ft(&file_transfer);

                                let ft_for_dispatch: Option<Arc<dyn FileTransferPlugin>> = ft_arc.clone();

                                // Spawn receiver task for reliable messages
                                let state_clone = state.clone();
                                let repaint_clone = repaint.clone();
                                let audio_task_clone = audio_task.clone();
                                let command_tx_clone = command_tx.clone();
                                let ft_for_recv = ft_arc;
                                let ft_slot_for_recv = file_transfer_slot.clone();
                                tokio::spawn(async move {
                                    run_receiver_task(recv_stream, state_clone, repaint_clone, audio_task_clone, command_tx_clone, ft_for_recv, ft_slot_for_recv).await;
                                });

                                // Spawn stream dispatch task for server-initiated bi-directional streams
                                tokio::spawn(async move {
                                    run_stream_dispatch(bi_handle, ft_for_dispatch).await;
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
                                    };
                                    {
                                        let mut s = write_state(&state);
                                        s.connection = ConnectionState::CertificatePending { cert_info: pending };
                                    }
                                    repaint();
                                } else if is_cert_error_message(&e) {
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
                            match connect_to_server::<P::Transport>(
                                &pending.server_addr,
                                &pending.username,
                                &pending.public_key,
                                key_signer.as_ref(),
                                pending.password.as_deref(),
                                &new_config,
                                captured_cert,
                            ).await {
                                Ok((mut new_transport, user_id, rooms, users, groups, session_info)) => {
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
                                            .and_then(rumble_protocol::uuid_from_room_id)
                                            .or(Some(ROOT_ROOM_UUID));
                                        s.my_session_public_key = Some(session_info.session_public_key);
                                        s.my_session_id = Some(session_info.session_id);
                                        s.rooms = rooms;
                                        s.users = users;
                                        s.group_definitions = groups;
                                        s.rebuild_room_tree();
                                    }
                                    recalculate_effective_permissions(&state);
                                    repaint();

                                    // Notify audio task
                                    audio_task.send(AudioCommand::ConnectionEstablished {
                                        datagram: Arc::new(new_transport.datagram_handle()),
                                        my_user_id: user_id,
                                    });

                                    client_name = pending.username;
                                    _session_identity = Some(session_info);

                                    // Split off the receive stream for the receiver task
                                    let recv_stream = new_transport.take_recv();

                                    // Get bi-stream handles: one for dispatch, one for the relay plugin
                                    let bi_handle = new_transport.bi_stream_handle();
                                    let opener_handle = new_transport.bi_stream_handle();

                                    transport = Some(new_transport);

                                    // Create the file transfer relay plugin (if platform supports it)
                                    let downloads_dir = config
                                        .download_dir
                                        .clone()
                                        .unwrap_or_else(|| std::env::temp_dir().join("rumble_downloads"));
                                    let opener: Arc<dyn rumble_client_traits::StreamOpener> =
                                        Arc::new(rumble_client_traits::BiStreamOpener::new(opener_handle));
                                    let ft_arc: Option<Arc<dyn FileTransferPlugin>> =
                                        P::create_file_transfer_plugin(opener, downloads_dir);

                                    // Set the initial room ID on the relay plugin
                                    if let Some(ft) = &ft_arc
                                        && let Some(room_uuid) = read_state(&state).my_room_id {
                                            ft.set_room_id(room_uuid.to_string());
                                        }

                                    // Keep a reference for room-change updates,
                                    // and publish to the shared slot so UI threads
                                    // can call into the plugin.
                                    file_transfer = ft_arc.clone();
                                    publish_ft(&file_transfer);

                                    let ft_for_dispatch: Option<Arc<dyn FileTransferPlugin>> = ft_arc.clone();

                                    // Spawn receiver task
                                    let state_clone = state.clone();
                                    let repaint_clone = repaint.clone();
                                    let audio_task_clone = audio_task.clone();
                                    let command_tx_clone = command_tx.clone();
                                    let ft_for_recv = ft_arc;
                                    let ft_slot_for_recv = file_transfer_slot.clone();
                                    tokio::spawn(async move {
                                        run_receiver_task(recv_stream, state_clone, repaint_clone, audio_task_clone, command_tx_clone, ft_for_recv, ft_slot_for_recv).await;
                                    });

                                    // Spawn stream dispatch task for server-initiated bi-directional streams
                                    tokio::spawn(async move {
                                        run_stream_dispatch(bi_handle, ft_for_dispatch).await;
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

                        if let Some(t) = transport.take() {
                            t.close().await;
                        }
                        file_transfer = None;
                        publish_ft(&file_transfer);
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
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::JoinRoom(proto::JoinRoom {
                                    room_id: Some(room_id_from_uuid(room_id)),
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send JoinRoom: {}", e);
                            }
                        }
                    }

                    Command::CreateRoom { name, parent_id } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::CreateRoom(proto::CreateRoom {
                                    name,
                                    parent_id: parent_id.map(room_id_from_uuid),
                                    description: None,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send CreateRoom: {}", e);
                            }
                        }
                    }

                    Command::DeleteRoom { room_id } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::DeleteRoom(proto::DeleteRoom {
                                    room_id: Some(room_id_from_uuid(room_id)),
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send DeleteRoom: {}", e);
                            }
                        }
                    }

                    Command::RenameRoom { room_id, new_name } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::RenameRoom(proto::RenameRoom {
                                    room_id: Some(room_id_from_uuid(room_id)),
                                    new_name,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send RenameRoom: {}", e);
                            }
                        }
                    }

                    Command::MoveRoom { room_id, new_parent_id } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::MoveRoom(proto::MoveRoom {
                                    room_id: Some(room_id_from_uuid(room_id)),
                                    new_parent_id: Some(room_id_from_uuid(new_parent_id)),
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send MoveRoom: {}", e);
                            }
                        }
                    }

                    Command::SetRoomDescription { room_id, description } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::SetRoomDescription(proto::SetRoomDescription {
                                    room_id: Some(room_id_from_uuid(room_id)),
                                    description,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send SetRoomDescription: {}", e);
                            }
                        }
                    }

                    Command::SendChat { text } => {
                        if let Some(t) = &mut transport {
                            let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
                            let timestamp_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as i64;
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                    id: message_id,
                                    timestamp_ms,
                                    sender: client_name.clone(),
                                    text,
                                    tree: None,
                                    attachment: None,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send ChatMessage: {}", e);
                            }
                        }
                    }

                    Command::SendTreeChat { text } => {
                        if let Some(t) = &mut transport {
                            let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
                            let timestamp_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as i64;
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                    id: message_id,
                                    timestamp_ms,
                                    sender: client_name.clone(),
                                    text,
                                    tree: Some(true),
                                    attachment: None,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send tree ChatMessage: {}", e);
                            }
                        }
                    }

                    Command::SendDirectMessage { target_user_id, target_username, text } => {
                        if let Some(t) = &mut transport {
                            let message_id = uuid::Uuid::new_v4().into_bytes();
                            let timestamp = std::time::SystemTime::now();
                            let timestamp_ms = timestamp
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as i64;
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::DirectMessage(proto::DirectMessage {
                                    target_user_id,
                                    text: text.clone(),
                                    id: message_id.to_vec(),
                                    timestamp_ms,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send DirectMessage: {}", e);
                            }
                            // Add local message so the sender sees their own DM
                            let mut s = write_state(&state);
                            s.chat_messages.push(crate::events::ChatMessage {
                                id: message_id,
                                sender: client_name.clone(),
                                text,
                                timestamp,
                                is_local: false,
                                kind: crate::events::ChatMessageKind::DirectMessage {
                                    other_user_id: target_user_id,
                                    other_username: target_username,
                                },
                                attachment: None,
                            });
                            if s.chat_messages.len() > 100 {
                                s.chat_messages.remove(0);
                            }
                            drop(s);
                            repaint();
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
                            kind: Default::default(),
                            attachment: None,
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
                        if let Some(t) = &mut transport {
                            let is_deafened = read_state(&state).audio.self_deafened;
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::SetUserStatus(proto::SetUserStatus {
                                    is_muted: muted,
                                    is_deafened,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send SetUserStatus: {}", e);
                            }
                        }
                    }

                    Command::SetDeafened { deafened } => {
                        // Send status update to server
                        // Note: deafen implies mute
                        if let Some(t) = &mut transport {
                            let is_muted = read_state(&state).audio.self_muted || deafened;
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::SetUserStatus(proto::SetUserStatus {
                                    is_muted,
                                    is_deafened: deafened,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
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
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::RegisterUser(proto::RegisterUser {
                                    user_id: Some(proto::UserId { value: user_id }),
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send RegisterUser: {}", e);
                            }
                        }
                    }

                    Command::UnregisterUser { user_id } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::UnregisterUser(proto::UnregisterUser {
                                    user_id: Some(proto::UserId { value: user_id }),
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send UnregisterUser: {}", e);
                            }
                        }
                    }

                    Command::ShareFile { path } => {
                        if let Some(ref ft) = file_transfer {
                            match ft.share(path) {
                                Ok(offer) => {
                                    info!("File shared via relay: {} ({})", offer.name, offer.id.0);
                                    if let Some(t) = &mut transport {
                                        let attachment = proto::ChatAttachment {
                                            kind: Some(proto::chat_attachment::Kind::FileOffer(proto::FileOffer {
                                                schema_version: 1,
                                                transfer_id: offer.id.0.clone(),
                                                name: offer.name.clone(),
                                                size: offer.size,
                                                mime: offer.mime.clone(),
                                                share_data: offer.share_data,
                                            })),
                                        };
                                        let summary = format!(
                                            "shared file \"{}\" ({})",
                                            offer.name,
                                            format_bytes(offer.size),
                                        );
                                        let env = proto::Envelope {
                                            state_hash: Vec::new(),
                                            payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                                id: uuid::Uuid::new_v4().into_bytes().to_vec(),
                                                sender: client_name.clone(),
                                                text: summary,
                                                timestamp_ms: std::time::SystemTime::now()
                                                    .duration_since(std::time::UNIX_EPOCH)
                                                    .unwrap_or_default()
                                                    .as_millis() as i64,
                                                tree: None,
                                                attachment: Some(attachment),
                                            })),
                                        };
                                        if let Err(e) = send_envelope(t, &env).await {
                                            error!("Failed to send file share message: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("File share failed: {}", e);
                                    add_local_message(&state, format!("File share failed: {}", e), &repaint);
                                }
                            }
                        } else {
                            warn!("ShareFile: no file transfer plugin available");
                            add_local_message(&state, "File sharing is not currently available".to_string(), &repaint);
                        }
                    }

                    Command::DownloadFile { share_data } => {
                        if let Some(ref ft) = file_transfer {
                            match ft.download(&share_data) {
                                Ok(id) => info!("Download started: {}", id.0),
                                Err(e) => {
                                    warn!("Download failed: {}", e);
                                    add_local_message(&state, format!("Download failed: {}", e), &repaint);
                                }
                            }
                        } else {
                            warn!("DownloadFile: no file transfer plugin available");
                            add_local_message(&state, "File downloads are not currently available".to_string(), &repaint);
                        }
                    }

                    Command::CancelTransfer { transfer_id } => {
                        if let Some(ref ft) = file_transfer {
                            let id = rumble_client_traits::file_transfer::TransferId(transfer_id);
                            if let Err(e) = ft.cancel(&id, false) {
                                warn!("Cancel failed for {}: {}", id.0, e);
                            }
                        }
                    }

                    Command::RequestChatHistory => {
                        // Send a chat history request to the room
                        if let Some(t) = &mut transport {
                            let request = crate::events::ChatHistoryRequestMessage::new();
                            let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
                            let timestamp_ms = SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as i64;

                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                    id: message_id,
                                    timestamp_ms,
                                    sender: client_name.clone(),
                                    text: request.to_json(),
                                    tree: None,
                                    attachment: None,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
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
                                    kind: Default::default(),
                                    attachment: None,
                                });
                                repaint();
                            }
                        }
                    }

                    Command::ShareChatHistory => {
                        if let Some(t) = &mut transport {
                            let messages = {
                                let s = read_state(&state);
                                s.chat_messages.clone()
                            };
                            let content = crate::events::ChatHistoryContent::from_messages(&messages);
                            if content.messages.is_empty() {
                                debug!("ShareChatHistory: nothing to share");
                            } else {
                                let share = crate::events::ChatHistoryShareMessage::new(content);
                                let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
                                let timestamp_ms = SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as i64;
                                let env = proto::Envelope {
                                    state_hash: Vec::new(),
                                    payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                        id: message_id,
                                        timestamp_ms,
                                        sender: client_name.clone(),
                                        text: share.to_json(),
                                        tree: None,
                                        attachment: None,
                                    })),
                                };
                                if let Err(e) = send_envelope(t, &env).await {
                                    error!("Failed to send chat history response: {}", e);
                                } else {
                                    debug!("Shared {} chat messages", share.content.messages.len());
                                }
                            }
                        }
                    }

                    // ACL Commands
                    Command::KickUser { target_user_id, reason } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: vec![],
                                payload: Some(Payload::KickUser(proto::KickUser {
                                    target_user_id,
                                    reason,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send KickUser: {}", e);
                            }
                        }
                    }
                    Command::BanUser {
                        target_user_id,
                        reason,
                        duration_seconds,
                    } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: vec![],
                                payload: Some(Payload::BanUser(proto::BanUser {
                                    target_user_id,
                                    reason,
                                    duration_seconds: duration_seconds.unwrap_or(0),
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send BanUser: {}", e);
                            }
                        }
                    }
                    Command::SetServerMute { target_user_id, muted } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: vec![],
                                payload: Some(Payload::SetServerMute(proto::SetServerMute {
                                    target_user_id,
                                    muted,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send SetServerMute: {}", e);
                            }
                        }
                    }
                    Command::Elevate { password } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: vec![],
                                payload: Some(Payload::Elevate(proto::Elevate { password })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send Elevate: {}", e);
                            }
                        }
                    }
                    Command::CreateGroup { name, permissions } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: vec![],
                                payload: Some(Payload::CreateGroup(proto::CreateGroup {
                                    name,
                                    permissions,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send CreateGroup: {e}");
                            }
                        }
                    }
                    Command::DeleteGroup { name } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: vec![],
                                payload: Some(Payload::DeleteGroup(proto::DeleteGroup { name })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send DeleteGroup: {e}");
                            }
                        }
                    }
                    Command::ModifyGroup { name, permissions } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: vec![],
                                payload: Some(Payload::ModifyGroup(proto::ModifyGroup {
                                    name,
                                    permissions,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send ModifyGroup: {e}");
                            }
                        }
                    }
                    Command::SetUserGroup {
                        target_user_id,
                        group,
                        add,
                        expires_at,
                    } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: vec![],
                                payload: Some(Payload::SetUserGroup(proto::SetUserGroup {
                                    target_user_id,
                                    group,
                                    add,
                                    expires_at,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send SetUserGroup: {e}");
                            }
                        }
                    }
                    Command::SetRoomAcl {
                        room_id,
                        inherit_acl,
                        entries,
                    } => {
                        if let Some(t) = &mut transport {
                            let env = proto::Envelope {
                                state_hash: vec![],
                                payload: Some(Payload::SetRoomAcl(proto::SetRoomAcl {
                                    room_id: room_id.as_bytes().to_vec(),
                                    inherit_acl,
                                    entries,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send SetRoomAcl: {e}");
                            }
                        }
                    }
                    // PlaySfx is intercepted in BackendHandle::send() and never reaches here
                    Command::PlaySfx { .. } => {}
                }
            }
        }
    }
}

/// Ephemeral session identity for this connection.
struct SessionIdentity {
    #[allow(dead_code)]
    signing_key: SigningKey,
    session_public_key: [u8; 32],
    session_id: [u8; 32],
    _issued_ms: i64,
    _expires_ms: i64,
}

/// Connect to a server and perform handshake with Ed25519 authentication.
///
/// Uses the Transport trait for QUIC connection and protocol framing.
/// Returns the connected transport plus handshake results.
async fn connect_to_server<T: Transport>(
    addr: &str,
    client_name: &str,
    public_key: &[u8; 32],
    key_signer: &dyn KeySigning,
    password: Option<&str>,
    config: &ConnectConfig,
    captured_cert: CapturedCert,
) -> anyhow::Result<(
    T,
    u64, // user_id
    Vec<proto::RoomInfo>,
    Vec<proto::User>,
    Vec<proto::GroupInfo>,
    SessionIdentity,
)> {
    info!(server_addr = %addr, client_name, "Connecting to server");

    // Build TlsConfig from ConnectConfig. We pass raw file bytes through —
    // the Transport impl handles PEM/DER detection (see TlsConfig docs).
    let mut additional_ca_certs = Vec::new();

    for cert_path in &config.additional_certs {
        match std::fs::read(cert_path) {
            Ok(cert_bytes) => {
                info!("Loaded certificate file {:?}", cert_path);
                additional_ca_certs.push(cert_bytes);
            }
            Err(e) => {
                error!("Failed to load cert from {:?}: {}", cert_path, e);
            }
        }
    }

    // Add user-accepted certificates (from interactive prompts). Always DER.
    for cert_der in &config.accepted_certs {
        additional_ca_certs.push(cert_der.clone());
    }

    let tls_config = TlsConfig {
        accept_invalid_certs: false,
        additional_ca_certs,
        accepted_fingerprints: Vec::new(),
        captured_cert: Some(captured_cert),
    };

    // Connect via Transport trait (handles QUIC handshake + bi-stream setup)
    let mut transport = T::connect(addr, tls_config).await?;
    info!("Connected to server via Transport");

    // Step 1: Send ClientHello with public key
    let hello = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ClientHello(proto::ClientHello {
            username: client_name.to_string(),
            public_key: public_key.to_vec(),
            password: password.map(|s| s.to_string()),
        })),
    };
    send_envelope(&mut transport, &hello).await?;
    debug!("Sent ClientHello");

    // Step 2: Wait for ServerHello with nonce
    let (nonce, user_id) = wait_for_server_hello(&mut transport).await?;
    info!(user_id, "Received ServerHello with nonce");

    // Step 3: Compute server certificate hash from transport
    let server_cert_hash = if let Some(cert_der) = transport.peer_certificate_der() {
        compute_cert_hash(&cert_der)
    } else {
        warn!("Could not get server certificate for hash computation");
        [0u8; 32]
    };

    // Step 4: Generate session keypair and certificate signed by long-term key
    let session_secret: [u8; 32] = rand::random();
    let session_signing = ed25519_dalek::SigningKey::from_bytes(&session_secret);
    let session_public_bytes: [u8; 32] = session_signing.verifying_key().to_bytes();

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let expires_ms = timestamp_ms + 24 * 60 * 60 * 1000; // 24h validity for session cert

    let cert_payload = build_session_cert_payload(&session_public_bytes, timestamp_ms, expires_ms, Some(client_name));
    let session_signature = key_signer
        .sign(public_key, &cert_payload)
        .await
        .map_err(|e| anyhow::anyhow!("Signing session cert failed: {}", e))?;

    // Step 5: Compute signature payload for handshake
    let payload = build_auth_payload(&nonce, timestamp_ms, public_key, user_id, &server_cert_hash);

    // Step 6: Sign the handshake payload
    let signature = key_signer
        .sign(public_key, &payload)
        .await
        .map_err(|e| anyhow::anyhow!("Signing handshake payload failed: {}", e))?;

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
    send_envelope(&mut transport, &auth).await?;
    debug!("Sent Authenticate");

    // Step 8: Wait for ServerState or AuthFailed
    let (rooms, users, groups) = wait_for_auth_result(&mut transport).await?;

    let session_identity = SessionIdentity {
        signing_key: session_signing,
        session_public_key: session_public_bytes,
        session_id: compute_session_id(&session_public_bytes),
        _issued_ms: timestamp_ms,
        _expires_ms: expires_ms,
    };

    Ok((transport, user_id, rooms, users, groups, session_identity))
}

/// Background task that receives reliable messages from the server.
///
/// Uses `TransportRecvStream` for framed message reception. Connection loss
/// is detected when `recv()` returns `None` or an error.
async fn run_receiver_task(
    mut recv: impl TransportRecvStream,
    state: Arc<RwLock<State>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    audio_task: AudioTaskHandle,
    command_tx: mpsc::UnboundedSender<Command>,
    file_transfer: Option<Arc<dyn FileTransferPlugin>>,
    file_transfer_slot: Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>>,
) {
    loop {
        match recv.recv().await {
            Ok(Some(frame)) => {
                if let Ok(env) = proto::Envelope::decode(&*frame) {
                    handle_server_message(env, &state, &repaint, &audio_task, &command_tx, &file_transfer);
                }
            }
            Ok(None) => {
                info!("Server closed the receive stream");
                break;
            }
            Err(e) => {
                warn!("Read error on receive stream: {}", e);
                break;
            }
        }
    }

    // Notify audio task
    audio_task.send(AudioCommand::ConnectionClosed);

    // Clear the shared file-transfer slot so UI accessors stop returning
    // statuses for a connection that no longer exists.
    {
        let mut slot = match file_transfer_slot.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *slot = None;
    }

    // Update state only if not already disconnected (explicit disconnect sets Disconnected)
    {
        let mut s = write_state(&state);
        if !matches!(s.connection, ConnectionState::Disconnected) {
            s.connection = ConnectionState::ConnectionLost {
                error: "Connection closed".to_string(),
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

/// Background task that accepts server-initiated bi-directional streams
/// and dispatches them to the appropriate plugin based on `StreamHeader`.
async fn run_stream_dispatch<H: BiStreamHandle>(bi_handle: H, file_transfer: Option<Arc<dyn FileTransferPlugin>>) {
    loop {
        match bi_handle.accept_bi().await {
            Ok(Some((send, mut recv))) => {
                // Read the StreamHeader to determine which plugin should handle this stream.
                let header = match StreamHeader::read_from(&mut recv).await {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("failed to read stream header: {e}");
                        continue;
                    }
                };

                debug!(plugin = %header.plugin, "dispatching incoming stream");

                match header.plugin.as_str() {
                    "file-relay" => {
                        if let Some(ref ft) = file_transfer {
                            let ft = ft.clone();
                            tokio::spawn(async move {
                                ft.on_incoming_stream(Box::new(send), Box::new(recv)).await;
                            });
                        } else {
                            warn!("received file-relay stream but no file transfer plugin configured");
                        }
                    }
                    other => {
                        warn!(plugin = other, "no handler for incoming stream plugin");
                    }
                }
            }
            Ok(None) => {
                // Connection closed.
                debug!("stream dispatch: connection closed");
                break;
            }
            Err(e) => {
                warn!("stream dispatch accept error: {e}");
                break;
            }
        }
    }
}

/// Add a local status message to the chat.
fn add_local_message(state: &Arc<RwLock<State>>, text: String, repaint: &Arc<dyn Fn() + Send + Sync>) {
    let mut s = write_state(state);
    s.chat_messages.push(crate::events::ChatMessage {
        id: uuid::Uuid::new_v4().into_bytes(),
        sender: String::new(),
        text,
        timestamp: std::time::SystemTime::now(),
        is_local: true,
        kind: Default::default(),
        attachment: None,
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
    command_tx: &mpsc::UnboundedSender<Command>,
    file_transfer: &Option<Arc<dyn FileTransferPlugin>>,
) {
    match env.payload {
        Some(Payload::CommandResult(cr)) => {
            // Display command results as local chat messages
            let prefix = if cr.success { "✔" } else { "✖" };
            add_local_message(state, format!("{} {}", prefix, cr.message), repaint);
        }
        Some(Payload::ServerEvent(se)) => {
            if let Some(kind) = se.kind {
                match kind {
                    proto::server_event::Kind::ServerState(ss) => {
                        // Full state replacement
                        let mut s = write_state(state);

                        // Extract per-room effective permissions from server-computed values
                        s.per_room_permissions.clear();
                        for room in &ss.rooms {
                            if let Some(room_uuid) = room.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) {
                                s.per_room_permissions.insert(room_uuid, room.effective_permissions);
                            }
                        }

                        s.rooms = ss.rooms;
                        s.users = ss.users.clone();
                        s.group_definitions = ss.groups;
                        s.rebuild_room_tree();

                        // Update effective_permissions from per-room data for current room
                        if let Some(my_room) = s.my_room_id
                            && let Some(&perms) = s.per_room_permissions.get(&my_room)
                        {
                            s.effective_permissions = perms;
                        }

                        // Sync our own server-muted state to the audio task on
                        // initial connect — without this, joining a SPEAK-denied
                        // room before the first frame would leave the mic
                        // capturing even though the server is dropping packets.
                        let my_server_muted = s.my_user_id.and_then(|id| {
                            ss.users
                                .iter()
                                .find(|u| u.user_id.as_ref().map(|x| x.value) == Some(id))
                                .map(|u| u.server_muted)
                        });

                        // Notify audio task about users in our room (for proactive decoder creation)
                        if let Some(my_room_id) = &s.my_room_id {
                            let my_user_id = s.my_user_id;
                            let user_ids_in_room: Vec<u64> = ss
                                .users
                                .iter()
                                .filter_map(|u| {
                                    let user_id = u.user_id.as_ref().map(|id| id.value)?;
                                    let user_room =
                                        u.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
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
                        if let Some(muted) = my_server_muted {
                            audio_task.send(AudioCommand::SetServerMuted { muted });
                        }
                        // Still do client-side recalculation as fallback
                        recalculate_effective_permissions(state);
                        repaint();
                    }
                    proto::server_event::Kind::StateUpdate(su) => {
                        apply_state_update(su, state, repaint, audio_task, file_transfer);
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

                        // Check if this is a chat history response — merge messages
                        if let Some(share) = crate::events::ChatHistoryShareMessage::parse(&cb.text) {
                            debug!(
                                "Received chat history from {}: {} messages",
                                cb.sender,
                                share.content.messages.len()
                            );
                            let incoming = share.content.to_messages();
                            let mut s = write_state(state);
                            // Collect existing IDs to skip duplicates
                            let existing_ids: std::collections::HashSet<[u8; 16]> =
                                s.chat_messages.iter().map(|m| m.id).collect();
                            let mut added = 0usize;
                            for msg in incoming {
                                if !existing_ids.contains(&msg.id) {
                                    s.chat_messages.push(msg);
                                    added += 1;
                                }
                            }
                            if added > 0 {
                                // Sort merged history by timestamp
                                s.chat_messages.sort_by_key(|m| m.timestamp);
                                // Trim to 100 most recent
                                let len = s.chat_messages.len();
                                if len > 100 {
                                    s.chat_messages.drain(0..len - 100);
                                }
                                drop(s);
                                repaint();
                            }
                            return;
                        }

                        let kind = if cb.tree.unwrap_or(false) {
                            crate::events::ChatMessageKind::Tree
                        } else {
                            crate::events::ChatMessageKind::Room
                        };

                        let attachment = cb.attachment.and_then(rumble_protocol::chat_attachment_from_proto);

                        let mut s = write_state(state);
                        s.chat_messages.push(crate::events::ChatMessage {
                            id,
                            sender: cb.sender,
                            text: cb.text,
                            timestamp,
                            is_local: false,
                            kind,
                            attachment,
                        });
                        // Keep only recent messages
                        if s.chat_messages.len() > 100 {
                            s.chat_messages.remove(0);
                        }
                        drop(s);
                        repaint();
                    }
                    proto::server_event::Kind::DirectMessageReceived(dm) => {
                        // Incoming DM from another user (server no longer echoes to sender)
                        let id: [u8; 16] = dm.id.try_into().unwrap_or_else(|_| uuid::Uuid::new_v4().into_bytes());
                        let timestamp = if dm.timestamp_ms > 0 {
                            std::time::UNIX_EPOCH + std::time::Duration::from_millis(dm.timestamp_ms as u64)
                        } else {
                            std::time::SystemTime::now()
                        };

                        let mut s = write_state(state);
                        s.chat_messages.push(crate::events::ChatMessage {
                            id,
                            sender: dm.sender_name.clone(),
                            text: dm.text,
                            timestamp,
                            is_local: false,
                            kind: crate::events::ChatMessageKind::DirectMessage {
                                other_user_id: dm.sender_id,
                                other_username: dm.sender_name,
                            },
                            attachment: None,
                        });
                        if s.chat_messages.len() > 100 {
                            s.chat_messages.remove(0);
                        }
                        drop(s);
                        repaint();
                    }
                    proto::server_event::Kind::KeepAlive(_) => {
                        // Ignore keep-alive for now
                    }
                    proto::server_event::Kind::WelcomeMessage(wm) => {
                        let mut s = write_state(state);
                        s.chat_messages.push(crate::events::ChatMessage {
                            id: uuid::Uuid::new_v4().into_bytes(),
                            sender: "Server".to_string(),
                            text: wm.text,
                            timestamp: std::time::SystemTime::now(),
                            is_local: true,
                            kind: Default::default(),
                            attachment: None,
                        });
                        if s.chat_messages.len() > 100 {
                            s.chat_messages.remove(0);
                        }
                        drop(s);
                        repaint();
                    }
                }
            }
        }
        Some(Payload::PermissionDenied(pd)) => {
            warn!("Permission denied: {}", pd.message);
            let mut s = write_state(state);
            s.permission_denied = Some(pd.message);
            drop(s);
            repaint();
        }
        Some(Payload::UserKicked(uk)) => {
            let my_user_id = read_state(state).my_user_id;
            if my_user_id == Some(uk.user_id) {
                // We were kicked
                let reason = if uk.reason.is_empty() {
                    format!("Kicked by {}", uk.kicked_by)
                } else {
                    format!("Kicked by {}: {}", uk.kicked_by, uk.reason)
                };
                warn!("{}", reason);
                let mut s = write_state(state);
                s.kicked = Some(reason);
                drop(s);
                // The server will close the connection, so we don't need to disconnect explicitly
            } else {
                // Another user was kicked - they'll get a UserLeft event too
                info!("User {} was kicked by {}", uk.user_id, uk.kicked_by);
            }
            repaint();
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
    file_transfer: &Option<Arc<dyn FileTransferPlugin>>,
) {
    if let Some(u) = update.update {
        let mut s = write_state(state);
        match u {
            proto::state_update::Update::RoomCreated(rc) => {
                if let Some(room) = rc.room {
                    s.rooms.push(room);
                    s.rebuild_room_tree();
                    drop(s);
                    recalculate_effective_permissions(state);
                    repaint();
                    return;
                }
            }
            proto::state_update::Update::RoomDeleted(rd) => {
                if let Some(rid) = rd.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                    s.rooms
                        .retain(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) != Some(rid));
                    s.rebuild_room_tree();
                    drop(s);
                    recalculate_effective_permissions(state);
                    repaint();
                    return;
                }
            }
            proto::state_update::Update::RoomRenamed(rr) => {
                if let Some(rid) = rr.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                    if let Some(room) = s
                        .rooms
                        .iter_mut()
                        .find(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) == Some(rid))
                    {
                        room.name = rr.new_name;
                    }
                    s.rebuild_room_tree();
                }
            }
            proto::state_update::Update::RoomMoved(rm) => {
                if let Some(rid) = rm.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                    if let Some(room) = s
                        .rooms
                        .iter_mut()
                        .find(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) == Some(rid))
                    {
                        room.parent_id = rm.new_parent_id;
                    }
                    s.rebuild_room_tree();
                }
            }
            proto::state_update::Update::RoomDescriptionChanged(rdc) => {
                if let Some(rid) = rdc.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                    let desc = if rdc.description.is_empty() {
                        None
                    } else {
                        Some(rdc.description)
                    };
                    if let Some(room) = s
                        .rooms
                        .iter_mut()
                        .find(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) == Some(rid))
                    {
                        room.description = desc;
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
                        let user_room = user.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id);
                        let notify_audio = user_id_value.is_some() && my_room_id.is_some() && user_room == my_room_id;

                        s.users.push(user);

                        if notify_audio && let Some(uid) = user_id_value {
                            drop(s);
                            audio_task.send(AudioCommand::UserJoinedRoom { user_id: uid });
                            repaint();
                            return;
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
                        .map(|u| u.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id) == my_room_id)
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
                if let (Some(uid), Some(to_room)) = (um.user_id, um.to_room_id.clone()) {
                    let to_room_clone = to_room.clone();
                    let to_room_id = rumble_protocol::uuid_from_room_id(&to_room_clone);
                    let my_room_id = s.my_room_id;
                    let my_user_id = s.my_user_id;

                    // Look up where the user was before (from_room is implicit in User.current_room)
                    let from_room_id = s
                        .users
                        .iter()
                        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                        .and_then(|u| u.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id));

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

                        // Update the relay plugin's room ID
                        if let Some(ft) = file_transfer {
                            ft.set_room_id(to_room_id.map(|r| r.to_string()).unwrap_or_default());
                        }

                        // We changed rooms - rebuild decoder list
                        if let Some(new_room_id) = to_room_id {
                            let user_ids_in_room: Vec<u64> = s
                                .users
                                .iter()
                                .filter_map(|u| {
                                    let user_id = u.user_id.as_ref().map(|id| id.value)?;
                                    let user_room =
                                        u.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
                                    if user_room == new_room_id && Some(user_id) != my_user_id {
                                        Some(user_id)
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            drop(s);
                            audio_task.send(AudioCommand::RoomChanged { user_ids_in_room });
                            recalculate_effective_permissions(state);
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
                    let my_user_id = s.my_user_id;
                    if let Some(user) = s
                        .users
                        .iter_mut()
                        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                    {
                        let prev_server_muted = user.server_muted;
                        user.is_muted = usc.is_muted;
                        user.is_deafened = usc.is_deafened;
                        user.server_muted = usc.server_muted;
                        user.is_elevated = usc.is_elevated;
                        // Propagate our own server_muted flips to the audio
                        // task so it stops capturing instead of just sending
                        // packets the server will drop.
                        if my_user_id == Some(uid.value) && prev_server_muted != usc.server_muted {
                            drop(s);
                            audio_task.send(AudioCommand::SetServerMuted {
                                muted: usc.server_muted,
                            });
                            repaint();
                            return;
                        }
                    }
                }
            }
            proto::state_update::Update::GroupChanged(gc) => {
                if gc.deleted {
                    if let Some(group) = &gc.group {
                        s.group_definitions.retain(|g| g.name != group.name);
                    }
                } else if let Some(group) = gc.group {
                    if let Some(existing) = s.group_definitions.iter_mut().find(|g| g.name == group.name) {
                        *existing = group;
                    } else {
                        s.group_definitions.push(group);
                    }
                }
                drop(s);
                recalculate_effective_permissions(state);
                repaint();
                return;
            }
            proto::state_update::Update::UserGroupChanged(ugc) => {
                // Update user's groups list in our local state
                let my_user_id = s.my_user_id;
                if let Some(user) = s
                    .users
                    .iter_mut()
                    .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(ugc.user_id))
                {
                    if ugc.added {
                        if !user.groups.contains(&ugc.group) {
                            user.groups.push(ugc.group);
                        }
                    } else {
                        user.groups.retain(|g| g != &ugc.group);
                    }
                }
                if my_user_id == Some(ugc.user_id) {
                    drop(s);
                    recalculate_effective_permissions(state);
                    repaint();
                    return;
                }
            }
            proto::state_update::Update::RoomAclChanged(rac) => {
                if let Some(rid) = rac.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                    // Update the room's ACL data in our local state
                    if let Some(room) = s
                        .rooms
                        .iter_mut()
                        .find(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) == Some(rid))
                    {
                        room.inherit_acl = rac.inherit_acl;
                        room.acls = rac.entries;
                    }
                    drop(s);
                    // Recalculate permissions since ACLs changed
                    recalculate_effective_permissions(state);
                    repaint();
                    return;
                }
            }
        }
        drop(s);
        repaint();
    }
}

/// Recalculate the effective permissions for the current user in all rooms.
///
/// Updates both `effective_permissions` (for current room) and `per_room_permissions`
/// (for all rooms). This acquires the state lock internally, so the caller must NOT hold it.
fn recalculate_effective_permissions(state: &Arc<RwLock<State>>) {
    let s = state.read().unwrap();
    let my_user_id = match s.my_user_id {
        Some(id) => id,
        None => return,
    };

    // Build user's group list
    let mut user_groups = vec!["default".to_string()];
    if let Some(me) = s
        .users
        .iter()
        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(my_user_id))
    {
        for g in &me.groups {
            if !user_groups.contains(g) {
                user_groups.push(g.clone());
            }
        }
        // Add username as implicit group
        if !user_groups.contains(&me.username) {
            user_groups.push(me.username.clone());
        }
    }

    // Build group permissions map
    let mut group_perms = std::collections::HashMap::new();
    for gd in &s.group_definitions {
        group_perms.insert(
            gd.name.clone(),
            rumble_protocol::permissions::Permissions::from_bits_truncate(gd.permissions),
        );
    }

    // Check superuser status
    let is_elevated = s
        .users
        .iter()
        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(my_user_id))
        .map(|u| u.is_elevated)
        .unwrap_or(false);

    let my_room_id = s.my_room_id;

    // Collect all room UUIDs
    let room_uuids: Vec<Uuid> = s
        .rooms
        .iter()
        .filter_map(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id))
        .collect();

    // Clone rooms for chain building (we'll release the read lock)
    let rooms_snapshot = s.rooms.clone();
    drop(s);

    // Compute per-room effective permissions
    let mut per_room = HashMap::new();
    for room_uuid in &room_uuids {
        let room_chain = build_client_room_chain(&rooms_snapshot, *room_uuid);
        let ref_chain: Vec<(Uuid, Option<&rumble_protocol::permissions::RoomAclData>)> =
            room_chain.iter().map(|(uuid, acl)| (*uuid, acl.as_ref())).collect();
        let effective =
            rumble_protocol::permissions::effective_permissions(&user_groups, &group_perms, &ref_chain, is_elevated);
        per_room.insert(*room_uuid, effective.bits());
    }

    let mut s = state.write().unwrap();
    s.per_room_permissions = per_room;

    // Update the current room's effective_permissions for backward compatibility
    if let Some(my_room) = my_room_id
        && let Some(&perms) = s.per_room_permissions.get(&my_room)
    {
        s.effective_permissions = perms;
    }
}

/// Build the room chain from root to target room, with ACL data for each room.
fn build_client_room_chain(
    rooms: &[rumble_protocol::proto::RoomInfo],
    target: Uuid,
) -> Vec<(Uuid, Option<rumble_protocol::permissions::RoomAclData>)> {
    use rumble_protocol::permissions::{AclEntry, Permissions, RoomAclData};

    let mut path = Vec::new();
    let mut current = target;

    loop {
        path.push(current);
        if current == rumble_protocol::ROOT_ROOM_UUID {
            break;
        }
        let parent = rooms.iter().find_map(|r| {
            let rid = r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
            if rid == current {
                r.parent_id.as_ref().and_then(rumble_protocol::uuid_from_room_id)
            } else {
                None
            }
        });
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }

    path.reverse();
    if path.first() != Some(&rumble_protocol::ROOT_ROOM_UUID) {
        path.insert(0, rumble_protocol::ROOT_ROOM_UUID);
    }
    path.dedup();

    path.into_iter()
        .map(|room_uuid| {
            let acl_data = rooms.iter().find_map(|r| {
                let rid = r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
                if rid == room_uuid && !r.acls.is_empty() {
                    Some(RoomAclData {
                        inherit_acl: r.inherit_acl,
                        entries: r
                            .acls
                            .iter()
                            .map(|e| AclEntry {
                                group: e.group.clone(),
                                grant: Permissions::from_bits_truncate(e.grant),
                                deny: Permissions::from_bits_truncate(e.deny),
                                apply_here: e.apply_here,
                                apply_subs: e.apply_subs,
                            })
                            .collect(),
                    })
                } else {
                    None
                }
            });
            (room_uuid, acl_data)
        })
        .collect()
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
