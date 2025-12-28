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
    audio::AudioSystem,
    audio_task::{spawn_audio_task, AudioCommand, AudioTaskConfig, AudioTaskHandle},
    events::{AudioState, Command, ConnectionState, State, VoiceMode},
    ConnectConfig,
};
use api::{
    encode_frame,
    proto::{self, envelope::Payload},
    room_id_from_uuid, try_decode_frame, ROOT_ROOM_UUID,
};
use bytes::BytesMut;
use prost::Message;
use quinn::{crypto::rustls::QuicClientConfig, Endpoint};
use std::{
    collections::HashSet,
    net::ToSocketAddrs,
    sync::{Arc, RwLock},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

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
        Self::with_config(repaint_callback, ConnectConfig::new())
    }

    /// Create a new backend handle with a repaint callback and connect config.
    pub fn with_config<F>(repaint_callback: F, connect_config: ConnectConfig) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
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
        };

        let state = Arc::new(RwLock::new(state));

        // Spawn the audio task (runs on its own thread)
        let audio_task = spawn_audio_task(AudioTaskConfig {
            state: state.clone(),
            repaint: repaint_callback.clone(),
        });

        // Clone handles for the connection task
        let state_for_task = state.clone();
        let repaint_for_task = repaint_callback.clone();
        let config_for_task = connect_config.clone();
        let audio_task_for_connection = audio_task.clone();

        // Spawn background thread with tokio runtime for connection task
        let runtime_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async move {
                run_connection_task(
                    command_rx,
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
        }
    }

    /// Get the current state for rendering.
    ///
    /// This returns a clone of the state. The UI should call this
    /// in its render loop to get the latest state.
    pub fn state(&self) -> State {
        self.state.read().unwrap().clone()
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
                self.audio_task.send(AudioCommand::UpdateTxPipeline {
                    config: config.clone(),
                });
                return;
            }
            Command::UpdateRxPipelineDefaults { config } => {
                self.audio_task.send(AudioCommand::UpdateRxPipelineDefaults {
                    config: config.clone(),
                });
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
                self.audio_task.send(AudioCommand::ClearUserRxOverride {
                    user_id: *user_id,
                });
                return;
            }
            Command::SetUserVolume { user_id, volume_db } => {
                self.audio_task.send(AudioCommand::SetUserVolume {
                    user_id: *user_id,
                    volume_db: *volume_db,
                });
                return;
            }
            _ => {}
        }

        // Forward non-audio commands to connection task
        let _ = self.command_tx.send(command);
    }

    /// Check if we are currently connected.
    pub fn is_connected(&self) -> bool {
        self.state.read().unwrap().connection.is_connected()
    }

    /// Get our user ID if connected.
    pub fn my_user_id(&self) -> Option<u64> {
        self.state.read().unwrap().my_user_id
    }

    /// Get our current room ID if in a room.
    pub fn my_room_id(&self) -> Option<Uuid> {
        self.state.read().unwrap().my_room_id
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
    state: Arc<RwLock<State>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    config: ConnectConfig,
    audio_task: AudioTaskHandle,
) {
    // Connection state
    let mut connection: Option<quinn::Connection> = None;
    let mut send_stream: Option<quinn::SendStream> = None;
    let mut client_name = String::new();

    loop {
        tokio::select! {
            cmd = command_rx.recv() => {
                let Some(cmd) = cmd else {
                    // Channel closed, handle is dropped, exit task
                    debug!("Command channel closed, shutting down connection task");
                    if let Some(conn) = connection.take() {
                        conn.close(quinn::VarInt::from_u32(0), b"shutdown");
                    }
                    return;
                };
                match cmd {
                    Command::Connect { addr, name, password } => {
                        client_name = name.clone();
                        
                        // Update state to Connecting
                        {
                            let mut s = state.write().unwrap();
                            s.connection = ConnectionState::Connecting { server_addr: addr.clone() };
                        }
                        repaint();

                        // Attempt connection
                        match connect_to_server(&addr, &name, password.as_deref(), &config).await {
                            Ok((conn, send, recv, recv_buf, user_id, rooms, users)) => {
                                // Update state to Connected
                                {
                                    let mut s = state.write().unwrap();
                                    s.connection = ConnectionState::Connected {
                                        server_name: "Rumble Server".to_string(),
                                        user_id,
                                    };
                                    s.my_user_id = Some(user_id);
                                    s.my_room_id = Some(ROOT_ROOM_UUID);
                                    s.rooms = rooms;
                                    s.users = users;
                                }
                                repaint();
                                
                                // Notify audio task of new connection
                                audio_task.send(AudioCommand::ConnectionEstablished {
                                    connection: conn.clone(),
                                    my_user_id: user_id,
                                });
                                
                                connection = Some(conn.clone());
                                send_stream = Some(send);
                                
                                // Spawn receiver task for reliable messages
                                let state_clone = state.clone();
                                let repaint_clone = repaint.clone();
                                let audio_task_clone = audio_task.clone();
                                tokio::spawn(async move {
                                    run_receiver_task(conn, recv, recv_buf, state_clone, repaint_clone, audio_task_clone).await;
                                });
                            }
                            Err(e) => {
                                error!("Connection failed: {}", e);
                                {
                                    let mut s = state.write().unwrap();
                                    s.connection = ConnectionState::ConnectionLost { error: e.to_string() };
                                }
                                repaint();
                            }
                        }
                    }
                    
                    Command::Disconnect => {
                        // Notify audio task before closing
                        audio_task.send(AudioCommand::ConnectionClosed);
                        
                        if let Some(conn) = connection.take() {
                            conn.close(quinn::VarInt::from_u32(0), b"disconnect");
                        }
                        send_stream = None;
                        {
                            let mut s = state.write().unwrap();
                            s.connection = ConnectionState::Disconnected;
                            s.my_user_id = None;
                            s.my_room_id = None;
                            s.rooms.clear();
                            s.users.clear();
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
                    
                    Command::CreateRoom { name } => {
                        if let Some(send) = &mut send_stream {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::CreateRoom(proto::CreateRoom { name })),
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
                    
                    Command::SendChat { text } => {
                        if let Some(send) = &mut send_stream {
                            let env = proto::Envelope {
                                state_hash: Vec::new(),
                                payload: Some(Payload::ChatMessage(proto::ChatMessage {
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
                    
                    // Audio commands are routed to audio task in BackendHandle::send()
                    Command::SetMuted { muted } => {
                        // Send status update to server
                        if let Some(send) = &mut send_stream {
                            let s = state.read().unwrap();
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
                            let s = state.read().unwrap();
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
                }
            }
        }
    }
}

/// Connect to a server and perform handshake.
async fn connect_to_server(
    addr: &str,
    client_name: &str,
    password: Option<&str>,
    config: &ConnectConfig,
) -> anyhow::Result<(
    quinn::Connection,
    quinn::SendStream,
    quinn::RecvStream,
    BytesMut, // remaining buffer after handshake
    u64, // user_id
    Vec<proto::RoomInfo>,
    Vec<proto::User>,
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
    
    let url = Url::parse(&addr_as_url)
        .map_err(|e| anyhow::anyhow!("Invalid server address: {}", e))?;
    
    let host = url.host_str()
        .ok_or_else(|| anyhow::anyhow!("No host in server address"))?;
    let port = url.port().unwrap_or(DEFAULT_PORT);
    
    let socket_addr: SocketAddr = format!("{}:{}", host, port)
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("Failed to resolve address: {}", e))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("No addresses found for hostname"))?;

    let endpoint = make_client_endpoint(socket_addr, config)?;

    let conn = endpoint.connect(socket_addr, "localhost")?.await?;
    info!(remote = %conn.remote_address(), "Connected to server");

    let (mut send, mut recv) = conn.open_bi().await?;
    info!("Opened bi stream");

    // Send ClientHello
    let hello = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ClientHello(proto::ClientHello {
            client_name: client_name.to_string(),
            password: password.unwrap_or("").to_string(),
        })),
    };
    let frame = encode_frame(&hello);
    send.write_all(&frame).await?;

    // Also send JoinRoom for Root room
    let join_env = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::JoinRoom(proto::JoinRoom {
            room_id: Some(room_id_from_uuid(ROOT_ROOM_UUID)),
        })),
    };
    let join_frame = encode_frame(&join_env);
    send.write_all(&join_frame).await?;

    // Wait for ServerHello and initial state
    let mut buf = BytesMut::new();
    let mut user_id = 0u64;
    #[allow(unused_assignments)]
    let mut rooms: Vec<proto::RoomInfo> = Vec::new();
    #[allow(unused_assignments)]
    let mut users: Vec<proto::User> = Vec::new();

    'wait: loop {
        let mut chunk = [0u8; 4096];
        match recv.read(&mut chunk).await? {
            Some(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(frame) = try_decode_frame(&mut buf) {
                    if let Ok(env) = proto::Envelope::decode(&*frame) {
                        match env.payload {
                            Some(Payload::ServerHello(sh)) => {
                                info!(server_name = %sh.server_name, user_id = sh.user_id, "Received ServerHello");
                                user_id = sh.user_id;
                            }
                            Some(Payload::ServerEvent(se)) => {
                                if let Some(proto::server_event::Kind::ServerState(ss)) = se.kind {
                                    rooms = ss.rooms;
                                    users = ss.users;
                                    // Once we have both ServerHello and initial state, we're done
                                    if user_id != 0 {
                                        break 'wait;
                                    }
                                }
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

    Ok((conn, send, recv, buf, user_id, rooms, users))
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
) {
    loop {
        let mut chunk = [0u8; 4096];
        match recv.read(&mut chunk).await {
            Ok(Some(n)) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(frame) = try_decode_frame(&mut buf) {
                    if let Ok(env) = proto::Envelope::decode(&*frame) {
                        handle_server_message(env, &state, &repaint, &audio_task);
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
        let mut s = state.write().unwrap();
        if !matches!(s.connection, ConnectionState::Disconnected) {
            s.connection = ConnectionState::ConnectionLost { 
                error: error.to_string() 
            };
            s.my_user_id = None;
            s.my_room_id = None;
            s.rooms.clear();
            s.users.clear();
        }
    }
    repaint();
}

/// Handle an incoming server message and update state accordingly.
fn handle_server_message(
    env: proto::Envelope,
    state: &Arc<RwLock<State>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
    audio_task: &AudioTaskHandle,
) {
    if let Some(Payload::ServerEvent(se)) = env.payload {
        if let Some(kind) = se.kind {
            match kind {
                proto::server_event::Kind::ServerState(ss) => {
                    // Full state replacement
                    let mut s = state.write().unwrap();
                    s.rooms = ss.rooms;
                    s.users = ss.users.clone();
                    
                    // Notify audio task about users in our room (for proactive decoder creation)
                    if let Some(my_room_id) = &s.my_room_id {
                        let my_user_id = s.my_user_id;
                        let user_ids_in_room: Vec<u64> = ss.users.iter()
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
                    let mut s = state.write().unwrap();
                    s.chat_messages.push(crate::events::ChatMessage {
                        sender: cb.sender,
                        text: cb.text,
                        timestamp: std::time::Instant::now(),
                    });
                    // Keep only recent messages
                    if s.chat_messages.len() > 100 {
                        s.chat_messages.remove(0);
                    }
                    drop(s);
                    repaint();
                }
                proto::server_event::Kind::KeepAlive(_) => {
                    // Ignore keep-alive for now
                }
            }
        }
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
        let mut s = state.write().unwrap();
        match u {
            proto::state_update::Update::RoomCreated(rc) => {
                if let Some(room) = rc.room {
                    s.rooms.push(room);
                }
            }
            proto::state_update::Update::RoomDeleted(rd) => {
                if let Some(rid) = rd.room_id.and_then(|r| api::uuid_from_room_id(&r)) {
                    s.rooms.retain(|r| {
                        r.id.as_ref().and_then(api::uuid_from_room_id) != Some(rid)
                    });
                }
            }
            proto::state_update::Update::RoomRenamed(rr) => {
                if let Some(rid) = rr.room_id.and_then(|r| api::uuid_from_room_id(&r)) {
                    if let Some(room) = s.rooms.iter_mut().find(|r| {
                        r.id.as_ref().and_then(api::uuid_from_room_id) == Some(rid)
                    }) {
                        room.name = rr.new_name;
                    }
                }
            }
            proto::state_update::Update::UserJoined(uj) => {
                if let Some(user) = uj.user {
                    // Only add if user doesn't already exist (avoid duplicates from
                    // receiving our own UserJoined broadcast after initial ServerState)
                    let user_id_value = user.user_id.as_ref().map(|id| id.value);
                    let already_exists = s.users.iter().any(|u| {
                        u.user_id.as_ref().map(|id| id.value) == user_id_value
                    });
                    if !already_exists {
                        // Check if this user is joining our room - if so, notify audio task
                        let my_room_id = s.my_room_id;
                        let user_room = user.current_room.as_ref().and_then(api::uuid_from_room_id);
                        let notify_audio = user_id_value.is_some() 
                            && my_room_id.is_some() 
                            && user_room == my_room_id;
                        
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
                    let was_in_our_room = s.users.iter().find(|u| {
                        u.user_id.as_ref().map(|id| id.value) == Some(uid.value)
                    }).map(|u| {
                        u.current_room.as_ref().and_then(api::uuid_from_room_id) == my_room_id
                    }).unwrap_or(false);
                    
                    s.users.retain(|u| {
                        u.user_id.as_ref().map(|id| id.value) != Some(uid.value)
                    });
                    
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
                    let from_room_id = s.users.iter()
                        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                        .and_then(|u| u.current_room.as_ref().and_then(api::uuid_from_room_id));
                    
                    // Now update the user's current room
                    if let Some(user) = s.users.iter_mut().find(|u| {
                        u.user_id.as_ref().map(|id| id.value) == Some(uid.value)
                    }) {
                        user.current_room = Some(to_room);
                    }
                    
                    // Check if this is us moving
                    if my_user_id == Some(uid.value) {
                        s.my_room_id = to_room_id;
                        
                        // We changed rooms - rebuild decoder list
                        if let Some(new_room_id) = to_room_id {
                            let user_ids_in_room: Vec<u64> = s.users.iter()
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
                    if let Some(user) = s.users.iter_mut().find(|u| {
                        u.user_id.as_ref().map(|id| id.value) == Some(uid.value)
                    }) {
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

/// Create a QUIC client endpoint.
fn make_client_endpoint(remote_addr: std::net::SocketAddr, config: &ConnectConfig) -> anyhow::Result<Endpoint> {
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

    // Load additional configured certificates
    for cert_path in &config.additional_certs {
        match std::fs::read(cert_path) {
            Ok(cert_bytes) => {
                let cert = rustls::pki_types::CertificateDer::from(cert_bytes);
                let _ = root_store.add(cert);
                info!("Loaded additional cert from {:?}", cert_path);
            }
            Err(e) => {
                error!("Failed to load cert from {:?}: {}", cert_path, e);
            }
        }
    }

    let mut client_cfg = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::aws_lc_rs::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(root_store)
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
