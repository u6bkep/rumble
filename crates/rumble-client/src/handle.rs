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

/// Severity level for a [`BackendEvent::Toast`] notification.
///
/// Neutral type defined here so `rumble-client` doesn't depend on the
/// aetna UI library. Callers (aetna, tests) map this to their own
/// `ToastLevel` / colour token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Warn,
    Error,
}

/// One-way events that background tasks push to the UI.
///
/// Produced by internal client code via [`BackendHandle::push_event`];
/// consumed by the UI layer (aetna) by draining
/// [`BackendHandle::take_event_receiver`] once at startup and polling
/// it each frame.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    /// User-visible notification — wrap in a toast.
    Toast { level: NotificationLevel, text: String },
    /// A file transfer just changed lifecycle stage. Forwarded from
    /// the file-transfer plugin's [`PluginEvent::TransferStageChanged`]
    /// so the UI's media cache reacts on transition rather than
    /// polling `plugin.transfers()` every frame. Only emitted on
    /// stage *transitions* — intra-`Active` progress updates do not
    /// fire events (progress bars keep reading the snapshot).
    TransferStageChanged {
        id: rumble_client_traits::TransferId,
        direction: rumble_client_traits::TransferDirection,
        name: String,
        stage: rumble_client_traits::TransferStage,
    },
}

/// A handle to the backend that can be used from UI code.
///
/// This type manages the tokio runtime, async connection, audio subsystem,
/// and provides a state-driven interface for the UI. The UI reads state
/// via `state()` and sends commands via `send()`.
pub struct BackendHandle<P: Platform> {
    /// Shared state that the UI reads.
    ///
    /// The [`projection`][crate::projection] task is the sole
    /// writer of this `RwLock` in the active code path. The connection
    /// and audio tasks emit typed events on the `bus` instead of
    /// touching `state` directly; the projection folds those events
    /// into the snapshot. `state_mut()` exists as an escape hatch for
    /// the deprecated egui clients to clear one-shot fields.
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
    /// Typed per-domain event channels. The connection and audio
    /// tasks emit into the senders held here; subscribers (the
    /// projection task plus any UI consumer via
    /// `subscribe_*`) hold receivers. See [`crate::domain_events`].
    bus: crate::projection::EventBus,
    /// Background thread running the tokio runtime for connection task.
    /// Held in an `Option` so `Drop` can `take()` and `join()` it after
    /// signalling the connection task to send a graceful disconnect.
    runtime_thread: Option<std::thread::JoinHandle<()>>,
    /// Connection configuration (certificates, etc.). stored incase we want to inspect it later.
    _connect_config: ConnectConfig,
    /// Pre-generated sound effects library.
    sfx_library: crate::sfx::SfxLibrary,
    /// Sender half of the backend-event channel. Internal client code
    /// calls `push_event` to enqueue a [`BackendEvent`]; the UI drains
    /// the receiver each frame.
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    /// Receiver half, wrapped in an `Option` so `take_event_receiver`
    /// can hand ownership to the UI on first call.
    event_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<BackendEvent>>>,
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
        let (event_tx, event_rx) = mpsc::unbounded_channel::<BackendEvent>();

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

        // Typed per-domain event channels. The connection and audio
        // tasks emit into the senders held here; the projection task
        // (spawned below) subscribes to all of them and is the sole
        // writer of `State`. External subscribers attach via the
        // `subscribe_*` methods on `BackendHandle`.
        let bus = crate::projection::EventBus::new();

        // Spawn the audio task (runs on its own thread)
        let audio_task = spawn_audio_task::<P>(AudioTaskConfig {
            state: state.clone(),
            repaint: repaint_callback.clone(),
            bus: bus.clone(),
            audio_dumper,
            audio_backend,
        });

        // Clone handles for the connection task
        let state_for_task = state.clone();
        let config_for_task = connect_config.clone();
        let audio_task_for_connection = audio_task.clone();
        let command_tx_for_task = command_tx.clone();
        let file_transfer_for_task = file_transfer.clone();
        let event_tx_for_task = event_tx.clone();
        let bus_for_task = bus.clone();
        let bus_for_projection = bus.clone();
        let state_for_projection = state.clone();
        let repaint_for_projection = repaint_callback.clone();
        let audio_task_for_projection = audio_task.clone();
        let file_transfer_for_projection = file_transfer.clone();

        // Spawn background thread with tokio runtime for connection task.
        // The projection task is spawned on this same runtime so it
        // can subscribe to the broadcast channels in scope.
        let key_signer_for_task = key_signer.clone();
        let runtime_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async move {
                // Projection task: subscribes to every domain channel
                // and maintains `State`. Sole writer of the snapshot the
                // UI reads each frame.
                let _projection_handle = crate::projection::spawn_projection_task(
                    state_for_projection,
                    repaint_for_projection,
                    &bus_for_projection,
                    audio_task_for_projection,
                    file_transfer_for_projection,
                );
                run_connection_task::<P>(
                    command_rx,
                    command_tx_for_task,
                    state_for_task,
                    config_for_task,
                    key_signer_for_task,
                    audio_task_for_connection,
                    file_transfer_for_task,
                    event_tx_for_task,
                    bus_for_task,
                )
                .await;
            });
        });

        Self {
            state,
            command_tx,
            audio_task,
            file_transfer,
            bus,
            runtime_thread: Some(runtime_thread),
            _connect_config: connect_config,
            sfx_library: crate::sfx::SfxLibrary::new(),
            event_tx,
            event_rx: std::sync::Mutex::new(Some(event_rx)),
            _phantom: std::marker::PhantomData,
        }
    }

    // -------------------------------------------------------------
    // Typed event subscriptions
    // -------------------------------------------------------------
    //
    // Each `subscribe_*` returns a fresh `broadcast::Receiver` that
    // starts seeing events emitted *after* the call (broadcast
    // channels don't replay history to late subscribers). For
    // initial state, read [`Self::state`] once; for incremental
    // updates, subscribe and consume the receiver.
    //
    // Per-domain channels mean a consumer that cares only about
    // chat (or only about voice transitions, etc.) doesn't get
    // woken up for unrelated events.

    /// Subscribe to chat events ([`ChatEvent`][crate::ChatEvent]).
    ///
    /// Consumers: chat renderer's "what's new since last frame" diff,
    /// auto-download evaluator, DM notification toasts.
    pub fn subscribe_chat(&self) -> tokio::sync::broadcast::Receiver<crate::ChatEvent> {
        self.bus.chat.subscribe()
    }

    /// Subscribe to voice events ([`VoiceEvent`][crate::VoiceEvent]).
    ///
    /// Consumers: SFX pump (mute/unmute/talking transitions), VU meter
    /// (input level), stats display.
    pub fn subscribe_voice(&self) -> tokio::sync::broadcast::Receiver<crate::VoiceEvent> {
        self.bus.voice.subscribe()
    }

    /// Subscribe to connection lifecycle events
    /// ([`ConnectionEvent`][crate::ConnectionEvent]).
    ///
    /// Consumers: connect/disconnect SFX, reconnect-backoff logic,
    /// permission-denied toast, kick dialog.
    pub fn subscribe_connection(&self) -> tokio::sync::broadcast::Receiver<crate::ConnectionEvent> {
        self.bus.connection.subscribe()
    }

    /// Subscribe to room/server-state events
    /// ([`RoomEvent`][crate::RoomEvent]).
    ///
    /// Consumers: user-joined-your-room SFX cue, room-list refresh
    /// triggers, ACL-changed permission recompute.
    pub fn subscribe_room(&self) -> tokio::sync::broadcast::Receiver<crate::RoomEvent> {
        self.bus.room.subscribe()
    }

    /// Subscribe to file-transfer lifecycle events
    /// ([`TransferEvent`][crate::TransferEvent]).
    ///
    /// Consumers: media cache (decode-on-completion), chat card
    /// re-render trigger, auto-download accept.
    pub fn subscribe_transfer(&self) -> tokio::sync::broadcast::Receiver<crate::TransferEvent> {
        self.bus.transfer.subscribe()
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

    /// Push a [`BackendEvent`] onto the UI-facing event channel.
    ///
    /// Background tasks call this to surface errors, progress notices,
    /// or other non-blocking notifications to the UI. The UI drains the
    /// channel each frame via the receiver obtained from
    /// [`Self::take_event_receiver`].
    pub fn push_event(&self, event: BackendEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Hand ownership of the event receiver to the caller.
    ///
    /// Should be called once at UI startup. Subsequent calls return
    /// `None` (the receiver has already been moved out).
    pub fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<BackendEvent>> {
        match self.event_rx.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        }
    }
}

impl<P: Platform> Drop for BackendHandle<P> {
    fn drop(&mut self) {
        // Tell the connection task to flush a graceful Disconnect and exit,
        // then wait for its thread before we let the process tear down the
        // tokio runtime. Without this the runtime is dropped mid-shutdown
        // and the server only notices us missing after its idle timeout.
        let _ = self.command_tx.send(Command::Shutdown);
        if let Some(thread) = self.runtime_thread.take() {
            let _ = thread.join();
        }
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

/// Best-effort send of a `proto::Disconnect` envelope. Used during shutdown
/// so the server can evict us immediately instead of waiting on the QUIC
/// idle timeout. A failure here is non-fatal: the subsequent `transport.close()`
/// still trips the server's connection-loss path, just without the named reason.
async fn send_disconnect_envelope<T: rumble_client_traits::transport::Transport>(transport: &mut T, reason: &str) {
    let env = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::Disconnect(proto::Disconnect {
            reason: reason.to_string(),
        })),
    };
    if let Err(e) = send_envelope(transport, &env).await {
        debug!("Failed to send Disconnect envelope during shutdown: {}", e);
    }
}

/// The main connection task that handles QUIC communication (reliable streams only).
///
/// This task manages:
/// - QUIC connection lifecycle
/// - Reliable protocol messages (via streams)
/// - State synchronization
///
/// Internal feedback channel from upload-watcher tasks back into the
/// connection task. The watcher polls `ft.transfers()` and, once an
/// upload finishes successfully, asks the connection task to broadcast
/// the corresponding chat message. Failure does not flow through this
/// channel: the sender's SenderDraft card reads the error directly from
/// the plugin's TransferStatus. Lives entirely inside
/// [`run_connection_task`].
#[derive(Debug)]
enum DeferredAction {
    /// Upload finished successfully — broadcast the chat envelope and
    /// insert a SenderMirror of the message in local state so
    /// late-joining peers can fetch it via history sync.
    FinalizeShareUpload {
        message_id: [u8; 16],
        attachment: rumble_protocol::ChatAttachment,
        sender: String,
        timestamp: std::time::SystemTime,
    },
}

/// It notifies the audio task when a connection is established or closed.
#[allow(clippy::too_many_arguments)] // private helper; bundling its args would just push churn
async fn run_connection_task<P: Platform>(
    mut command_rx: mpsc::UnboundedReceiver<Command>,
    command_tx: mpsc::UnboundedSender<Command>,
    state: Arc<RwLock<State>>,
    config: ConnectConfig,
    key_signer: Arc<dyn KeySigning>,
    audio_task: AudioTaskHandle,
    file_transfer_slot: Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>>,
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    bus: crate::projection::EventBus,
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

    // Channel for upload-watcher tasks to feed back results that have to
    // be handled inside the connection task (e.g. broadcasting the
    // FileOffer envelope, which requires `transport`).
    let (deferred_tx, mut deferred_rx) = mpsc::unbounded_channel::<DeferredAction>();

    loop {
        tokio::select! {
            Some(action) = deferred_rx.recv() => {
                match action {
                    DeferredAction::FinalizeShareUpload { message_id, attachment, sender, timestamp } => {
                        let Some(t) = &mut transport else {
                            // Disconnected between upload start and completion.
                            // The file is in the server cache but nobody knows
                            // about it; the sender's SenderDraft card still
                            // shows complete from plugin state. Drop the
                            // broadcast — no peers to deliver to anyway.
                            warn!("FinalizeShareUpload: transport gone, dropping broadcast");
                            continue;
                        };
                        let timestamp_ms = timestamp
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64;
                        let env = proto::Envelope {
                            state_hash: Vec::new(),
                            payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                id: message_id.to_vec(),
                                sender: sender.clone(),
                                text: attachment.fallback_text.clone(),
                                timestamp_ms,
                                tree: None,
                                attachment: Some(rumble_protocol::chat_attachment_to_proto(&attachment)),
                            })),
                        };
                        if let Err(e) = send_envelope(t, &env).await {
                            error!("Failed to broadcast file share message: {}", e);
                            // Per design: don't try to surface this on the
                            // chat card — the user's SenderDraft card reads
                            // upload state from the plugin (which shows
                            // complete). A toast keeps them informed that
                            // nobody else got it.
                            let _ = event_tx.send(BackendEvent::Toast {
                                level: NotificationLevel::Error,
                                text: format!("Failed to broadcast share: {e}"),
                            });
                            continue;
                        }
                        // Mirror the broadcast into local chat_messages as a
                        // SenderMirror entry so history-sync requests from
                        // late peers receive it. The SenderDraft twin (same
                        // id) keeps showing the live transfer state on the
                        // sender's screen.
                        let _ = bus.chat.send(crate::ChatEvent::SenderMirrorInserted {
                            msg: crate::events::ChatMessage {
                                id: message_id,
                                sender,
                                sender_id: None,
                                text: attachment.fallback_text.clone(),
                                timestamp,
                                kind: Default::default(),
                                attachment: Some(attachment),
                                visibility: crate::events::ChatMessageVisibility::SenderMirror,
                            },
                        });
                    }
                }
            }
            cmd = command_rx.recv() => {
                let Some(cmd) = cmd else {
                    // Channel closed, handle is dropped, exit task. Normal
                    // shutdown should reach this via Command::Shutdown
                    // first; this branch is the belt-and-suspenders path
                    // for the case where every sender clone went away
                    // without one being sent.
                    debug!("Command channel closed, shutting down connection task");
                    if let Some(mut t) = transport.take() {
                        send_disconnect_envelope(&mut t, "client shutdown").await;
                        t.close().await;
                    }
                    return;
                };
                match cmd {
                    Command::Connect { addr, name, public_key, password } => {
                        client_name = name.clone();

                        // Update state to Connecting via projection.
                        let _ = bus.connection.send(crate::ConnectionEvent::ConnectStarted {
                            server_addr: addr.clone(),
                        });

                        // Create a captured cert holder for the verifier to store self-signed certs
                        let captured_cert = new_captured_cert();

                        // Attempt connection with Ed25519 auth
                        match connect_to_server::<P::Transport>(&addr, &name, &public_key, key_signer.as_ref(), password.as_deref(), &config, captured_cert.clone()).await {
                            Ok((mut new_transport, user_id, rooms, users, groups, session_info)) => {
                                // Connection-identity transition via the projection.
                                let _ = bus.connection.send(crate::ConnectionEvent::Connected {
                                    server_name: "Rumble Server".to_string(),
                                    user_id,
                                    session_public_key: session_info.session_public_key,
                                    session_id: session_info.session_id,
                                });
                                // Pre-compute our room from the connect-time
                                // user list (falling back to root if missing).
                                let my_room_uuid = users.iter()
                                    .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
                                    .and_then(|u| u.current_room.as_ref())
                                    .and_then(rumble_protocol::uuid_from_room_id)
                                    .unwrap_or(ROOT_ROOM_UUID);

                                // Per-room effective permissions for the
                                // FullStateReplaced payload (server-computed).
                                let mut per_room_permissions: HashMap<Uuid, u32> = HashMap::new();
                                for room in &rooms {
                                    if let Some(room_uuid) = room.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) {
                                        per_room_permissions.insert(room_uuid, room.effective_permissions);
                                    }
                                }

                                let _ = bus.room.send(crate::RoomEvent::FullStateReplaced {
                                    rooms,
                                    users,
                                    groups,
                                    per_room_permissions,
                                });
                                let _ = bus.room.send(crate::RoomEvent::SelfMovedToRoom { room_id: my_room_uuid });

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
                                let event_sink = plugin_event_sink(event_tx.clone(), bus.clone());
                                let ft_arc: Option<Arc<dyn FileTransferPlugin>> =
                                    P::create_file_transfer_plugin(opener, downloads_dir, Some(event_sink));

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
                                let audio_task_clone = audio_task.clone();
                                let command_tx_clone = command_tx.clone();
                                let ft_slot_for_recv = file_transfer_slot.clone();
                                let bus_for_recv = bus.clone();
                                tokio::spawn(async move {
                                    run_receiver_task(recv_stream, state_clone, audio_task_clone, command_tx_clone, ft_slot_for_recv, bus_for_recv).await;
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
                                    let _ = bus.connection.send(crate::ConnectionEvent::CertificatePending {
                                        cert_info: pending,
                                    });
                                } else if is_cert_error_message(&e) {
                                    // Cert verification error but we didn't capture the cert - shouldn't happen
                                    // but log and treat as connection failure
                                    error!("Certificate verification error but no cert captured: {}", e);
                                    let _ = bus.connection.send(crate::ConnectionEvent::ConnectionLost {
                                        error: format!("Certificate error: {}", e),
                                    });
                                } else {
                                    error!("Connection failed: {}", e);
                                    let _ = bus.connection.send(crate::ConnectionEvent::ConnectionLost {
                                        error: e.to_string(),
                                    });
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
                                let _ = bus.connection.send(crate::ConnectionEvent::ConnectStarted {
                                    server_addr: pending.server_addr.clone(),
                                });
                            }

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
                                    let _ = bus.connection.send(crate::ConnectionEvent::Connected {
                                        server_name: "Rumble Server".to_string(),
                                        user_id,
                                        session_public_key: session_info.session_public_key,
                                        session_id: session_info.session_id,
                                    });
                                    let my_room_uuid = users.iter()
                                        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
                                        .and_then(|u| u.current_room.as_ref())
                                        .and_then(rumble_protocol::uuid_from_room_id)
                                        .unwrap_or(ROOT_ROOM_UUID);
                                    let mut per_room_permissions: HashMap<Uuid, u32> = HashMap::new();
                                    for room in &rooms {
                                        if let Some(room_uuid) = room.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) {
                                            per_room_permissions.insert(room_uuid, room.effective_permissions);
                                        }
                                    }
                                    let _ = bus.room.send(crate::RoomEvent::FullStateReplaced {
                                        rooms,
                                        users,
                                        groups,
                                        per_room_permissions,
                                    });
                                    let _ = bus.room.send(crate::RoomEvent::SelfMovedToRoom { room_id: my_room_uuid });

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
                                    let event_sink = plugin_event_sink(event_tx.clone(), bus.clone());
                                    let ft_arc: Option<Arc<dyn FileTransferPlugin>> =
                                        P::create_file_transfer_plugin(opener, downloads_dir, Some(event_sink));

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
                                    let audio_task_clone = audio_task.clone();
                                    let command_tx_clone = command_tx.clone();
                                    let ft_slot_for_recv = file_transfer_slot.clone();
                                    let bus_for_recv = bus.clone();
                                    tokio::spawn(async move {
                                        run_receiver_task(recv_stream, state_clone, audio_task_clone, command_tx_clone, ft_slot_for_recv, bus_for_recv).await;
                                    });

                                    // Spawn stream dispatch task for server-initiated bi-directional streams
                                    tokio::spawn(async move {
                                        run_stream_dispatch(bi_handle, ft_for_dispatch).await;
                                    });
                                }
                                Err(e) => {
                                    error!("Connection failed after accepting certificate: {}", e);
                                    let _ = bus.connection.send(crate::ConnectionEvent::ConnectionLost {
                                        error: e.to_string(),
                                    });
                                }
                            }
                        } else {
                            warn!("AcceptCertificate received but no certificate pending");
                        }
                    }

                    Command::RejectCertificate => {
                        // Simply go back to disconnected state
                        if let ConnectionState::CertificatePending { cert_info } = &read_state(&state).connection {
                            info!("User rejected certificate for {}", cert_info.server_name);
                        }
                        let _ = bus.connection.send(crate::ConnectionEvent::Disconnected);
                    }

                    Command::Disconnect => {
                        // Notify audio task before closing
                        audio_task.send(AudioCommand::ConnectionClosed);

                        if let Some(mut t) = transport.take() {
                            send_disconnect_envelope(&mut t, "client disconnected").await;
                            t.close().await;
                        }
                        file_transfer = None;
                        publish_ft(&file_transfer);
                        // Wipe room/user state via the projection: emit a
                        // FullStateReplaced with empty payloads alongside
                        // the connection-identity Disconnected event.
                        let _ = bus.room.send(crate::RoomEvent::FullStateReplaced {
                            rooms: Vec::new(),
                            users: Vec::new(),
                            groups: Vec::new(),
                            per_room_permissions: HashMap::new(),
                        });
                        let _ = bus.connection.send(crate::ConnectionEvent::Disconnected);
                    }

                    Command::Shutdown => {
                        // Process shutdown — flush a graceful Disconnect to
                        // the server and exit the connection task so the
                        // BackendHandle's Drop can join this thread before
                        // the process exits. Without this the server only
                        // notices we're gone after its QUIC idle timeout.
                        audio_task.send(AudioCommand::ConnectionClosed);
                        if let Some(mut t) = transport.take() {
                            send_disconnect_envelope(&mut t, "client shutdown").await;
                            t.close().await;
                        }
                        return;
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
                            let message_id = uuid::Uuid::new_v4().into_bytes();
                            let timestamp = std::time::SystemTime::now();
                            let timestamp_ms = timestamp
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as i64;
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                    id: message_id.to_vec(),
                                    timestamp_ms,
                                    sender: client_name.clone(),
                                    text: text.clone(),
                                    tree: None,
                                    attachment: None,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send ChatMessage: {}", e);
                            }
                            // Insert locally — server no longer echoes chat
                            // back to the sender. Projection applies the push.
                            let my_uid = read_state(&state).my_user_id;
                            let _ = bus.chat.send(crate::ChatEvent::MessageAdded {
                                msg: crate::events::ChatMessage {
                                    id: message_id,
                                    sender: client_name.clone(),
                                    sender_id: my_uid,
                                    text,
                                    timestamp,
                                    kind: crate::events::ChatMessageKind::Room,
                                    attachment: None,
                                    visibility: crate::events::ChatMessageVisibility::Normal,
                                },
                            });
                        }
                    }

                    Command::SendTreeChat { text } => {
                        if let Some(t) = &mut transport {
                            let message_id = uuid::Uuid::new_v4().into_bytes();
                            let timestamp = std::time::SystemTime::now();
                            let timestamp_ms = timestamp
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as i64;
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::ChatMessage(proto::ChatMessage {
                                    id: message_id.to_vec(),
                                    timestamp_ms,
                                    sender: client_name.clone(),
                                    text: text.clone(),
                                    tree: Some(true),
                                    attachment: None,
                                })),
                            };
                            if let Err(e) = send_envelope(t, &env).await {
                                error!("Failed to send tree ChatMessage: {}", e);
                            }
                            let my_uid = read_state(&state).my_user_id;
                            let _ = bus.chat.send(crate::ChatEvent::MessageAdded {
                                msg: crate::events::ChatMessage {
                                    id: message_id,
                                    sender: client_name.clone(),
                                    sender_id: my_uid,
                                    text,
                                    timestamp,
                                    kind: crate::events::ChatMessageKind::Tree,
                                    attachment: None,
                                    visibility: crate::events::ChatMessageVisibility::Normal,
                                },
                            });
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
                            let my_uid = read_state(&state).my_user_id;
                            let _ = bus.chat.send(crate::ChatEvent::MessageAdded {
                                msg: crate::events::ChatMessage {
                                    id: message_id,
                                    sender: client_name.clone(),
                                    sender_id: my_uid,
                                    text,
                                    timestamp,
                                    kind: crate::events::ChatMessageKind::DirectMessage {
                                        other_user_id: target_user_id,
                                        other_username: target_username,
                                    },
                                    attachment: None,
                                    visibility: crate::events::ChatMessageVisibility::Normal,
                                },
                            });
                        }
                    }

                    Command::LocalMessage { text } => {
                        let _ = bus.chat.send(crate::ChatEvent::SystemNotice { text });
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
                        let Some(ft) = file_transfer.as_ref() else {
                            warn!("ShareFile: no file transfer plugin available");
                            let _ = event_tx.send(BackendEvent::Toast {
                                level: NotificationLevel::Error,
                                text: "File sharing is not currently available".to_string(),
                            });
                            continue;
                        };
                        match ft.share(path) {
                            Ok(offer) => {
                                info!("File share started: {} ({})", offer.name, offer.id.0);
                                let message_id = uuid::Uuid::new_v4().into_bytes();
                                let timestamp = std::time::SystemTime::now();
                                // The plugin owns its on-wire payload encoding;
                                // rumble-client just routes the attachment.
                                let attachment = ft.encode_attachment(&offer);
                                let summary = attachment.fallback_text.clone();
                                // Sender's SenderDraft card. Renders in-flight
                                // / failed / complete state by reading the
                                // plugin's TransferStatus by transfer_id.
                                // Excluded from history sync — the matching
                                // `SenderMirror` entry (inserted by
                                // FinalizeShareUpload once the broadcast goes
                                // out) carries that responsibility.
                                let my_uid = read_state(&state).my_user_id;
                                let _ = bus.chat.send(crate::ChatEvent::SenderDraftInserted {
                                    msg: crate::events::ChatMessage {
                                        id: message_id,
                                        sender: client_name.clone(),
                                        sender_id: my_uid,
                                        text: summary,
                                        timestamp,
                                        kind: Default::default(),
                                        attachment: Some(attachment.clone()),
                                        visibility: crate::events::ChatMessageVisibility::SenderDraft,
                                    },
                                });
                                // Watcher broadcasts once the upload succeeds.
                                // On failure it just returns — the card reads
                                // TransferStatus.error directly from the plugin.
                                let ft_for_watch = ft.clone();
                                let deferred_tx_for_watch = deferred_tx.clone();
                                let sender = client_name.clone();
                                tokio::spawn(watch_share_upload(
                                    ft_for_watch,
                                    offer.id.0.clone(),
                                    message_id,
                                    attachment,
                                    sender,
                                    timestamp,
                                    deferred_tx_for_watch,
                                ));
                            }
                            Err(e) => {
                                // Plugin couldn't even form a transfer entry
                                // (e.g. file unreadable). Surface as a system
                                // chat line — no card to attach to.
                                warn!("File share failed: {}", e);
                                let _ = event_tx.send(BackendEvent::Toast {
                                    level: NotificationLevel::Error,
                                    text: format!("File share failed: {e}"),
                                });
                            }
                        }
                    }

                    Command::DownloadFile { share_data } => {
                        if let Some(ref ft) = file_transfer {
                            match ft.download(&share_data) {
                                Ok(id) => info!("Download started: {}", id.0),
                                Err(e) => {
                                    warn!("Download failed: {}", e);
                                    let _ = event_tx.send(BackendEvent::Toast {
                                        level: NotificationLevel::Error,
                                        text: format!("Download failed: {e}"),
                                    });
                                }
                            }
                        } else {
                            warn!("DownloadFile: no file transfer plugin available");
                            let _ = event_tx.send(BackendEvent::Toast {
                                level: NotificationLevel::Error,
                                text: "File downloads are not currently available".to_string(),
                            });
                        }
                    }

                    Command::CancelTransfer { transfer_id } => {
                        if let Some(ref ft) = file_transfer {
                            let name = ft
                                .transfers()
                                .into_iter()
                                .find(|s| s.id.0 == transfer_id)
                                .map(|s| s.name)
                                .unwrap_or_else(|| transfer_id.clone());
                            let id = rumble_client_traits::file_transfer::TransferId(transfer_id);
                            if let Err(e) = ft.cancel(&id, false) {
                                warn!("Cancel failed for {}: {}", id.0, e);
                            } else {
                                let _ = event_tx.send(BackendEvent::Toast {
                                    level: NotificationLevel::Info,
                                    text: format!("Transfer cancelled: {name}"),
                                });
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
                                let _ = bus.chat.send(crate::ChatEvent::SystemNotice {
                                    text: "Requesting chat history from peers...".to_string(),
                                });
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
    audio_task: AudioTaskHandle,
    command_tx: mpsc::UnboundedSender<Command>,
    file_transfer_slot: Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>>,
    bus: crate::projection::EventBus,
) {
    loop {
        match recv.recv().await {
            Ok(Some(frame)) => {
                if let Ok(env) = proto::Envelope::decode(&*frame) {
                    handle_server_message(env, &state, &command_tx, &bus);
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
    let already_disconnected = matches!(read_state(&state).connection, ConnectionState::Disconnected);
    if !already_disconnected {
        let _ = bus.connection.send(crate::ConnectionEvent::ConnectionLost {
            error: "Connection closed".to_string(),
        });
        // Wipe room/user state via the projection so my_room_id / rooms /
        // users / room_tree all get cleared atomically.
        let _ = bus.room.send(crate::RoomEvent::FullStateReplaced {
            rooms: Vec::new(),
            users: Vec::new(),
            groups: Vec::new(),
            per_room_permissions: HashMap::new(),
        });
    }
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

/// Poll the file-transfer plugin until the named upload finishes
/// successfully and ask the connection task to broadcast its chat
/// message. Failure paths (plugin error, plugin dropped entry, cancel)
/// just terminate the watcher — the sender's SenderDraft card reads
/// state directly from the plugin's TransferStatus.
async fn watch_share_upload(
    ft: Arc<dyn FileTransferPlugin>,
    transfer_id: String,
    message_id: [u8; 16],
    attachment: rumble_protocol::ChatAttachment,
    sender: String,
    timestamp: std::time::SystemTime,
    deferred_tx: mpsc::UnboundedSender<DeferredAction>,
) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let transfers = ft.transfers();
        let Some(status) = transfers.into_iter().find(|s| s.id.0 == transfer_id) else {
            return;
        };
        use rumble_client_traits::file_transfer::TransferStage;
        match status.stage {
            TransferStage::Done { .. } => {
                let _ = deferred_tx.send(DeferredAction::FinalizeShareUpload {
                    message_id,
                    attachment,
                    sender,
                    timestamp,
                });
                return;
            }
            TransferStage::Failed { .. } => return,
            TransferStage::Active { .. } | TransferStage::Paused { .. } => {}
        }
    }
}

/// Build a [`PluginEventSink`] that translates plugin events onto the
/// [`BackendEvent`] channel. Defined here so the connection task can
/// build it inline without `rumble-client-traits` taking a dependency
/// on `BackendEvent`.
fn plugin_event_sink(
    event_tx: mpsc::UnboundedSender<BackendEvent>,
    bus: crate::projection::EventBus,
) -> rumble_client_traits::PluginEventSink {
    use rumble_client_traits::PluginEvent;
    Arc::new(move |event| {
        match event {
            PluginEvent::Notification { level, text } => {
                let level = match level {
                    rumble_client_traits::PluginNotificationLevel::Info => NotificationLevel::Info,
                    rumble_client_traits::PluginNotificationLevel::Warn => NotificationLevel::Warn,
                    rumble_client_traits::PluginNotificationLevel::Error => NotificationLevel::Error,
                };
                let _ = event_tx.send(BackendEvent::Toast { level, text });
            }
            PluginEvent::TransferStageChanged {
                id,
                direction,
                name,
                stage,
            } => {
                // mpsc UX-notification path (legacy): the UI's media
                // cache drains this each frame.
                let _ = event_tx.send(BackendEvent::TransferStageChanged {
                    id: id.clone(),
                    direction,
                    name: name.clone(),
                    stage: stage.clone(),
                });
                // Broadcast TransferEvent — same payload, but on the
                // domain bus so post-cutover subscribers can react
                // alongside the mpsc consumer.
                let _ = bus.transfer.send(crate::domain_events::TransferEvent::StageChanged {
                    id,
                    direction,
                    name,
                    stage,
                });
            }
        }
    })
}

/// Emit a system notice to the chat log. The projection task does the
/// actual `chat_messages.push` + trim + repaint.
fn add_local_message(bus: &crate::projection::EventBus, text: String) {
    let _ = bus.chat.send(crate::ChatEvent::SystemNotice { text });
}

/// Handle an incoming server message and translate it into the matching
/// domain-event emissions on the bus.
fn handle_server_message(
    env: proto::Envelope,
    state: &Arc<RwLock<State>>,
    command_tx: &mpsc::UnboundedSender<Command>,
    bus: &crate::projection::EventBus,
) {
    match env.payload {
        Some(Payload::CommandResult(cr)) => {
            // Display command results as local chat messages
            let prefix = if cr.success { "✔" } else { "✖" };
            add_local_message(bus, format!("{} {}", prefix, cr.message));
        }
        Some(Payload::ServerEvent(se)) => {
            if let Some(kind) = se.kind {
                match kind {
                    proto::server_event::Kind::ServerState(ss) => {
                        // Build per-room permissions map from server-computed
                        // values, then emit a single FullStateReplaced event.
                        // The projection handles state mutation, audio task
                        // notifications (RoomChanged + SetServerMuted), and
                        // permission recalc.
                        let mut per_room_permissions: HashMap<Uuid, u32> = HashMap::new();
                        for room in &ss.rooms {
                            if let Some(room_uuid) = room.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) {
                                per_room_permissions.insert(room_uuid, room.effective_permissions);
                            }
                        }
                        let _ = bus.room.send(crate::RoomEvent::FullStateReplaced {
                            rooms: ss.rooms,
                            users: ss.users,
                            groups: ss.groups,
                            per_room_permissions,
                        });
                    }
                    proto::server_event::Kind::StateUpdate(su) => {
                        apply_state_update(su, state, bus);
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
                            // Pre-dedup against current state so the
                            // projection has minimal work to do.
                            let existing_ids: std::collections::HashSet<[u8; 16]> = {
                                let s = read_state(state);
                                s.chat_messages.iter().map(|m| m.id).collect()
                            };
                            let to_add: Vec<crate::events::ChatMessage> = share
                                .content
                                .to_messages()
                                .into_iter()
                                .filter(|m| !existing_ids.contains(&m.id))
                                .collect();
                            if !to_add.is_empty() {
                                let _ = bus.chat.send(crate::ChatEvent::HistoryMerged { msgs: to_add });
                            }
                            return;
                        }

                        let kind = if cb.tree.unwrap_or(false) {
                            crate::events::ChatMessageKind::Tree
                        } else {
                            crate::events::ChatMessageKind::Room
                        };

                        let attachment = cb.attachment.map(rumble_protocol::chat_attachment_from_proto);

                        let _ = bus.chat.send(crate::ChatEvent::MessageAdded {
                            msg: crate::events::ChatMessage {
                                id,
                                sender: cb.sender,
                                // Server populates sender_id from the authenticated
                                // connection; 0 means a legacy server didn't set it.
                                sender_id: if cb.sender_id != 0 { Some(cb.sender_id) } else { None },
                                text: cb.text,
                                timestamp,
                                kind,
                                attachment,
                                visibility: crate::events::ChatMessageVisibility::Normal,
                            },
                        });
                    }
                    proto::server_event::Kind::DirectMessageReceived(dm) => {
                        // Incoming DM from another user (server no longer echoes to sender)
                        let id: [u8; 16] = dm.id.try_into().unwrap_or_else(|_| uuid::Uuid::new_v4().into_bytes());
                        let timestamp = if dm.timestamp_ms > 0 {
                            std::time::UNIX_EPOCH + std::time::Duration::from_millis(dm.timestamp_ms as u64)
                        } else {
                            std::time::SystemTime::now()
                        };

                        let _ = bus.chat.send(crate::ChatEvent::MessageAdded {
                            msg: crate::events::ChatMessage {
                                id,
                                sender: dm.sender_name.clone(),
                                sender_id: Some(dm.sender_id),
                                text: dm.text,
                                timestamp,
                                kind: crate::events::ChatMessageKind::DirectMessage {
                                    other_user_id: dm.sender_id,
                                    other_username: dm.sender_name,
                                },
                                attachment: None,
                                visibility: crate::events::ChatMessageVisibility::Normal,
                            },
                        });
                    }
                    proto::server_event::Kind::KeepAlive(_) => {
                        // Ignore keep-alive for now
                    }
                    proto::server_event::Kind::WelcomeMessage(wm) => {
                        let _ = bus.chat.send(crate::ChatEvent::SystemNotice { text: wm.text });
                    }
                }
            }
        }
        Some(Payload::PermissionDenied(pd)) => {
            warn!("Permission denied: {}", pd.message);
            let _ = bus
                .connection
                .send(crate::ConnectionEvent::PermissionDenied { message: pd.message });
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
                let _ = bus.connection.send(crate::ConnectionEvent::Kicked { reason });
                // The server will close the connection, so we don't need to disconnect explicitly
            } else {
                // Another user was kicked - they'll get a UserLeft event too
                info!("User {} was kicked by {}", uk.user_id, uk.kicked_by);
            }
        }
        _ => {}
    }
}

/// Translate a `StateUpdate` envelope into the matching `RoomEvent`
/// emission. The projection task in `projection.rs` is the sole
/// writer of room/user/group state and the sole dispatcher of audio-task
/// notifications and file-transfer-plugin room-id updates.
fn apply_state_update(update: proto::StateUpdate, state: &Arc<RwLock<State>>, bus: &crate::projection::EventBus) {
    use crate::RoomEvent;

    let Some(u) = update.update else {
        return;
    };
    match u {
        proto::state_update::Update::RoomCreated(rc) => {
            if let Some(room) = rc.room {
                let _ = bus.room.send(RoomEvent::RoomAdded { room });
            }
        }
        proto::state_update::Update::RoomDeleted(rd) => {
            if let Some(rid) = rd.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                let _ = bus.room.send(RoomEvent::RoomRemoved { room_id: rid });
            }
        }
        proto::state_update::Update::RoomRenamed(rr) => {
            if let Some(rid) = rr.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                let _ = bus.room.send(RoomEvent::RoomRenamed {
                    room_id: rid,
                    name: rr.new_name,
                });
            }
        }
        proto::state_update::Update::RoomMoved(rm) => {
            if let Some(rid) = rm.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                let new_parent = rm.new_parent_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r));
                let _ = bus.room.send(RoomEvent::RoomMoved {
                    room_id: rid,
                    new_parent,
                });
            }
        }
        proto::state_update::Update::RoomDescriptionChanged(rdc) => {
            if let Some(rid) = rdc.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                let description = if rdc.description.is_empty() {
                    None
                } else {
                    Some(rdc.description)
                };
                let _ = bus.room.send(RoomEvent::RoomDescriptionSet {
                    room_id: rid,
                    description,
                });
            }
        }
        proto::state_update::Update::UserJoined(uj) => {
            if let Some(user) = uj.user {
                let _ = bus.room.send(RoomEvent::UserJoined { user });
            }
        }
        proto::state_update::Update::UserLeft(ul) => {
            if let Some(uid) = ul.user_id {
                let _ = bus.room.send(RoomEvent::UserLeft { user_id: uid.value });
            }
        }
        proto::state_update::Update::UserMoved(um) => {
            if let (Some(uid), Some(to_room)) = (um.user_id, um.to_room_id) {
                let to_room_uuid = rumble_protocol::uuid_from_room_id(&to_room);
                let my_user_id = read_state(state).my_user_id;
                let _ = bus.room.send(RoomEvent::UserMoved {
                    user_id: uid.value,
                    room_id: to_room_uuid,
                });
                // If it's us moving and the target is set, follow up with
                // SelfMovedToRoom so the projection updates my_room_id +
                // notifies the audio task + nudges the file-transfer plugin.
                if my_user_id == Some(uid.value)
                    && let Some(room_uuid) = to_room_uuid
                {
                    let _ = bus.room.send(RoomEvent::SelfMovedToRoom { room_id: room_uuid });
                }
            }
        }
        proto::state_update::Update::UserStatusChanged(usc) => {
            // Build the full updated User by reading the current snapshot
            // and overlaying the changed fields. The projection takes the
            // full User and applies the side effect (SetServerMuted if it
            // affects us) inside its locked section.
            if let Some(uid) = usc.user_id {
                let snapshot = read_state(state);
                if let Some(existing) = snapshot
                    .users
                    .iter()
                    .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                {
                    let mut user = existing.clone();
                    user.is_muted = usc.is_muted;
                    user.is_deafened = usc.is_deafened;
                    user.server_muted = usc.server_muted;
                    user.is_elevated = usc.is_elevated;
                    drop(snapshot);
                    let _ = bus.room.send(RoomEvent::UserUpdated { user });
                }
            }
        }
        proto::state_update::Update::GroupChanged(gc) => {
            if gc.deleted {
                if let Some(group) = gc.group {
                    let _ = bus.room.send(RoomEvent::GroupRemoved { name: group.name });
                }
            } else if let Some(group) = gc.group {
                // Existing-vs-new is decided in the projection (which
                // holds the source of truth); we emit Modified and the
                // projection upserts.
                let _ = bus.room.send(RoomEvent::GroupModified { group });
            }
        }
        proto::state_update::Update::UserGroupChanged(ugc) => {
            // Same "merge into existing user" pattern as UserStatusChanged.
            let snapshot = read_state(state);
            if let Some(existing) = snapshot
                .users
                .iter()
                .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(ugc.user_id))
            {
                let mut user = existing.clone();
                if ugc.added {
                    if !user.groups.contains(&ugc.group) {
                        user.groups.push(ugc.group);
                    }
                } else {
                    user.groups.retain(|g| g != &ugc.group);
                }
                drop(snapshot);
                let _ = bus.room.send(RoomEvent::UserUpdated { user });
            }
        }
        proto::state_update::Update::RoomAclChanged(rac) => {
            if let Some(rid) = rac.room_id.and_then(|r| rumble_protocol::uuid_from_room_id(&r)) {
                let _ = bus.room.send(RoomEvent::RoomAclChanged {
                    room_id: rid,
                    inherit_acl: rac.inherit_acl,
                    acls: rac.entries,
                    // The server-computed effective bitmask for this
                    // editor isn't included in the proto envelope today;
                    // projection will recompute locally.
                    effective: 0,
                    per_room_recompute: HashMap::new(),
                });
            }
        }
    }
}
