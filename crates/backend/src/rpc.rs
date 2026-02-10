//! Unix domain socket RPC server for external process control.
//!
//! Allows external processes to control a running Rumble client instance
//! via newline-delimited JSON commands over a Unix socket.

use crate::events::{Command, ConnectionState, State, VoiceMode};
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::{broadcast, mpsc},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Get the default socket path for the RPC server.
///
/// Uses `$XDG_RUNTIME_DIR/rumble/rpc.sock` if available, otherwise `/tmp/rumble/rpc.sock`.
pub fn default_socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    base.join("rumble").join("rpc.sock")
}

/// RPC request (newline-delimited JSON).
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "method")]
#[serde(rename_all = "snake_case")]
pub enum RpcRequest {
    // Queries
    GetState,
    GetStatus,

    // Connection
    Disconnect,

    // Rooms
    JoinRoom { room_id: String },
    CreateRoom { name: String, parent_id: Option<String> },
    DeleteRoom { room_id: String },
    RenameRoom { room_id: String, new_name: String },

    // Chat
    SendChat { text: String },

    // Audio
    SetMuted { muted: bool },
    SetDeafened { deafened: bool },
    SetVoiceMode { mode: String },
    StartTransmit,
    StopTransmit,
    MuteUser { user_id: u64 },
    UnmuteUser { user_id: u64 },
    SetUserVolume { user_id: u64, volume_db: f32 },

    // Files
    ShareFile { path: String },
    DownloadFile { magnet: String },
    PauseTransfer { infohash: String },
    ResumeTransfer { infohash: String },

    // Events
    Subscribe,
    Unsubscribe,
}

/// RPC response.
#[derive(Debug, serde::Serialize)]
pub struct RpcResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl RpcResponse {
    fn ok() -> Self {
        Self {
            status: "ok".to_string(),
            error: None,
            data: None,
        }
    }

    fn ok_with_data(data: serde_json::Value) -> Self {
        Self {
            status: "ok".to_string(),
            error: None,
            data: Some(data),
        }
    }

    fn error(msg: impl Into<String>) -> Self {
        Self {
            status: "error".to_string(),
            error: Some(msg.into()),
            data: None,
        }
    }
}

/// State event for subscriptions.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum StateEvent {
    StateChanged,
    ChatMessage { sender: String, text: String },
    UserJoined { user_id: u64, username: String },
    UserLeft { user_id: u64, username: String },
    ConnectionChanged { state: String },
}

/// RPC server that listens on a Unix domain socket.
pub struct RpcServer {
    socket_path: PathBuf,
    event_tx: broadcast::Sender<StateEvent>,
    _handle: tokio::task::JoinHandle<()>,
}

impl RpcServer {
    /// Start the RPC server on the given socket path.
    pub fn start(
        socket_path: PathBuf,
        state: Arc<RwLock<State>>,
        command_tx: mpsc::UnboundedSender<Command>,
    ) -> anyhow::Result<Self> {
        // Create parent directory if needed
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Remove stale socket file if it exists
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)?;
        }

        let listener = UnixListener::bind(&socket_path)?;
        info!(path = %socket_path.display(), "RPC server listening");

        let (event_tx, _) = broadcast::channel(256);
        let event_tx_clone = event_tx.clone();
        let socket_path_clone = socket_path.clone();

        let handle = tokio::spawn(async move {
            Self::accept_loop(listener, state, command_tx, event_tx_clone).await;
            debug!(path = %socket_path_clone.display(), "RPC server stopped");
        });

        Ok(Self {
            socket_path,
            event_tx,
            _handle: handle,
        })
    }

    /// Publish a state event to all subscribed clients.
    pub fn publish(&self, event: StateEvent) {
        // Ignore send errors (no subscribers is fine)
        let _ = self.event_tx.send(event);
    }

    async fn accept_loop(
        listener: UnixListener,
        state: Arc<RwLock<State>>,
        command_tx: mpsc::UnboundedSender<Command>,
        event_tx: broadcast::Sender<StateEvent>,
    ) {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let state = state.clone();
                    let command_tx = command_tx.clone();
                    let event_rx = event_tx.subscribe();
                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection(stream, state, command_tx, event_rx).await {
                            debug!("RPC connection ended: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("RPC accept error: {}", e);
                    break;
                }
            }
        }
    }

    async fn handle_connection(
        stream: tokio::net::UnixStream,
        state: Arc<RwLock<State>>,
        command_tx: mpsc::UnboundedSender<Command>,
        mut event_rx: broadcast::Receiver<StateEvent>,
    ) -> anyhow::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let mut subscribed = false;

        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Some(line) = line? else {
                        break; // EOF
                    };

                    let response = match serde_json::from_str::<RpcRequest>(&line) {
                        Ok(request) => {
                            let is_subscribe = matches!(request, RpcRequest::Subscribe);
                            let is_unsubscribe = matches!(request, RpcRequest::Unsubscribe);
                            let resp = Self::handle_request(request, &state, &command_tx);
                            if is_subscribe && resp.status == "ok" {
                                subscribed = true;
                            }
                            if is_unsubscribe {
                                subscribed = false;
                            }
                            resp
                        }
                        Err(e) => RpcResponse::error(format!("Invalid JSON: {}", e)),
                    };

                    let mut json = serde_json::to_string(&response)?;
                    json.push('\n');
                    writer.write_all(json.as_bytes()).await?;
                }
                event = event_rx.recv(), if subscribed => {
                    match event {
                        Ok(event) => {
                            let mut json = serde_json::to_string(&event)?;
                            json.push('\n');
                            writer.write_all(json.as_bytes()).await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("RPC event subscriber lagged by {} events", n);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_request(
        request: RpcRequest,
        state: &Arc<RwLock<State>>,
        command_tx: &mpsc::UnboundedSender<Command>,
    ) -> RpcResponse {
        match request {
            RpcRequest::GetState => {
                let state = state.read().unwrap();
                match serde_json::to_value(&*state) {
                    Ok(value) => RpcResponse::ok_with_data(value),
                    Err(e) => RpcResponse::error(format!("Serialization error: {}", e)),
                }
            }
            RpcRequest::GetStatus => {
                let state = state.read().unwrap();
                let status = match &state.connection {
                    ConnectionState::Disconnected => serde_json::json!({
                        "connected": false,
                        "state": "disconnected"
                    }),
                    ConnectionState::Connecting { server_addr } => serde_json::json!({
                        "connected": false,
                        "state": "connecting",
                        "server_addr": server_addr
                    }),
                    ConnectionState::Connected { server_name, user_id } => serde_json::json!({
                        "connected": true,
                        "state": "connected",
                        "server_name": server_name,
                        "user_id": user_id,
                        "room_id": state.my_room_id.map(|id| id.to_string()),
                        "users_count": state.users.len(),
                        "rooms_count": state.rooms.len(),
                        "muted": state.audio.self_muted,
                        "deafened": state.audio.self_deafened,
                        "transmitting": state.audio.is_transmitting,
                    }),
                    ConnectionState::ConnectionLost { error } => serde_json::json!({
                        "connected": false,
                        "state": "connection_lost",
                        "error": error
                    }),
                    ConnectionState::CertificatePending { cert_info } => serde_json::json!({
                        "connected": false,
                        "state": "certificate_pending",
                        "server_name": cert_info.server_name,
                    }),
                };
                RpcResponse::ok_with_data(status)
            }
            RpcRequest::Disconnect => {
                let _ = command_tx.send(Command::Disconnect);
                RpcResponse::ok()
            }
            RpcRequest::JoinRoom { room_id } => match room_id.parse::<Uuid>() {
                Ok(uuid) => {
                    let _ = command_tx.send(Command::JoinRoom { room_id: uuid });
                    RpcResponse::ok()
                }
                Err(e) => RpcResponse::error(format!("Invalid room UUID: {}", e)),
            },
            RpcRequest::CreateRoom { name, parent_id } => {
                let parent_uuid = match parent_id {
                    Some(id) => match id.parse::<Uuid>() {
                        Ok(uuid) => Some(uuid),
                        Err(e) => return RpcResponse::error(format!("Invalid parent UUID: {}", e)),
                    },
                    None => None,
                };
                let _ = command_tx.send(Command::CreateRoom {
                    name,
                    parent_id: parent_uuid,
                });
                RpcResponse::ok()
            }
            RpcRequest::DeleteRoom { room_id } => match room_id.parse::<Uuid>() {
                Ok(uuid) => {
                    let _ = command_tx.send(Command::DeleteRoom { room_id: uuid });
                    RpcResponse::ok()
                }
                Err(e) => RpcResponse::error(format!("Invalid room UUID: {}", e)),
            },
            RpcRequest::RenameRoom { room_id, new_name } => match room_id.parse::<Uuid>() {
                Ok(uuid) => {
                    let _ = command_tx.send(Command::RenameRoom {
                        room_id: uuid,
                        new_name,
                    });
                    RpcResponse::ok()
                }
                Err(e) => RpcResponse::error(format!("Invalid room UUID: {}", e)),
            },
            RpcRequest::SendChat { text } => {
                let _ = command_tx.send(Command::SendChat { text });
                RpcResponse::ok()
            }
            RpcRequest::SetMuted { muted } => {
                let _ = command_tx.send(Command::SetMuted { muted });
                RpcResponse::ok()
            }
            RpcRequest::SetDeafened { deafened } => {
                let _ = command_tx.send(Command::SetDeafened { deafened });
                RpcResponse::ok()
            }
            RpcRequest::SetVoiceMode { mode } => {
                let voice_mode = match mode.as_str() {
                    "push_to_talk" => VoiceMode::PushToTalk,
                    "continuous" => VoiceMode::Continuous,
                    _ => return RpcResponse::error(format!("Unknown voice mode: {}", mode)),
                };
                let _ = command_tx.send(Command::SetVoiceMode { mode: voice_mode });
                RpcResponse::ok()
            }
            RpcRequest::StartTransmit => {
                let _ = command_tx.send(Command::StartTransmit);
                RpcResponse::ok()
            }
            RpcRequest::StopTransmit => {
                let _ = command_tx.send(Command::StopTransmit);
                RpcResponse::ok()
            }
            RpcRequest::MuteUser { user_id } => {
                let _ = command_tx.send(Command::MuteUser { user_id });
                RpcResponse::ok()
            }
            RpcRequest::UnmuteUser { user_id } => {
                let _ = command_tx.send(Command::UnmuteUser { user_id });
                RpcResponse::ok()
            }
            RpcRequest::SetUserVolume { user_id, volume_db } => {
                let _ = command_tx.send(Command::SetUserVolume { user_id, volume_db });
                RpcResponse::ok()
            }
            RpcRequest::ShareFile { path } => {
                let _ = command_tx.send(Command::ShareFile {
                    path: PathBuf::from(path),
                });
                RpcResponse::ok()
            }
            RpcRequest::DownloadFile { magnet } => {
                let _ = command_tx.send(Command::DownloadFile { magnet });
                RpcResponse::ok()
            }
            RpcRequest::PauseTransfer { infohash } => {
                let _ = command_tx.send(Command::PauseTransfer { infohash });
                RpcResponse::ok()
            }
            RpcRequest::ResumeTransfer { infohash } => {
                let _ = command_tx.send(Command::ResumeTransfer { infohash });
                RpcResponse::ok()
            }
            RpcRequest::Subscribe => RpcResponse::ok_with_data(serde_json::json!({"subscribed": true})),
            RpcRequest::Unsubscribe => RpcResponse::ok_with_data(serde_json::json!({"subscribed": false})),
        }
    }
}

impl Drop for RpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        info!(path = %self.socket_path.display(), "RPC server socket removed");
    }
}
