use std::sync::{Arc, RwLock};

use anyhow::Result;
use prost::Message;
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::TcpListener,
    sync::mpsc,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tracing::{info, warn};

use crate::{
    bridge::{BridgeEvent, read_bridge, write_bridge},
    config::BridgeConfig,
    mumble_framing::{self, write_message, write_udp_tunnel},
    mumble_proto::{MessageType, mumble},
    state::{BridgeState, MumbleClient},
};

/// Messages that can be sent TO a Mumble client.
#[derive(Debug, Clone)]
pub enum MumbleOutbound {
    /// Send a protobuf message to the client.
    Protobuf { msg_type: u16, payload: Vec<u8> },
    /// Send raw voice data (UDPTunnel).
    Voice(Vec<u8>),
}

/// Start the Mumble TLS listener.
pub async fn run_mumble_server(
    config: Arc<BridgeConfig>,
    tls_acceptor: TlsAcceptor,
    bridge_state: Arc<RwLock<BridgeState>>,
    bridge_tx: mpsc::UnboundedSender<BridgeEvent>,
) -> Result<()> {
    let listen_addr = format!("0.0.0.0:{}", config.mumble_port);
    let listener = TcpListener::bind(&listen_addr).await?;
    info!(addr = %listen_addr, "Mumble server listening");

    loop {
        let (tcp_stream, peer_addr) = listener.accept().await?;
        info!(%peer_addr, "New Mumble TCP connection");

        let acceptor = tls_acceptor.clone();
        let state = bridge_state.clone();
        let btx = bridge_tx.clone();
        let cfg = config.clone();

        tokio::spawn(async move {
            match acceptor.accept(tcp_stream).await {
                Ok(tls_stream) => {
                    info!(%peer_addr, "TLS handshake complete");
                    if let Err(e) = handle_mumble_client(tls_stream, state, btx, cfg).await {
                        warn!(%peer_addr, error = %e, "Mumble client handler error");
                    }
                }
                Err(e) => {
                    warn!(%peer_addr, error = %e, "TLS handshake failed");
                }
            }
        });
    }
}

/// Handle a single Mumble client connection.
async fn handle_mumble_client(
    tls_stream: TlsStream<tokio::net::TcpStream>,
    bridge_state: Arc<RwLock<BridgeState>>,
    bridge_tx: mpsc::UnboundedSender<BridgeEvent>,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(tls_stream);
    let mut reader = reader;

    // Phase 1: Authentication handshake
    let (username, session) = mumble_auth_handshake(&mut reader, &mut writer, &bridge_state, &config).await?;
    info!(session, %username, "Mumble client authenticated");

    // Register with bridge
    bridge_tx.send(BridgeEvent::MumbleClientJoined {
        session,
        username: username.clone(),
    })?;

    // Create outbound channel for this client
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<MumbleOutbound>();

    // Register the outbound sender
    bridge_tx.send(BridgeEvent::MumbleClientSender {
        session,
        sender: out_tx,
    })?;

    // Spawn writer task
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let result = match msg {
                MumbleOutbound::Protobuf { msg_type, payload } => {
                    write_raw_frame(&mut writer, msg_type, &payload).await
                }
                MumbleOutbound::Voice(data) => write_udp_tunnel(&mut writer, &data).await,
            };
            if let Err(e) = result {
                warn!(error = %e, "Error writing to Mumble client");
                break;
            }
        }
    });

    // Phase 2: Message loop
    let result = mumble_message_loop(&mut reader, session, &bridge_tx).await;

    // Cleanup
    writer_handle.abort();
    bridge_tx.send(BridgeEvent::MumbleClientLeft { session })?;

    {
        let mut state = write_bridge(&bridge_state);
        state.mumble_clients.remove(&session);
    }

    info!(session, %username, "Mumble client disconnected");
    result
}

/// Perform the Mumble authentication handshake.
///
/// Waits for Version + Authenticate from the client, then sends back the full
/// channel/user state and ServerSync.
async fn mumble_auth_handshake(
    reader: &mut ReadHalf<TlsStream<tokio::net::TcpStream>>,
    writer: &mut WriteHalf<TlsStream<tokio::net::TcpStream>>,
    bridge_state: &Arc<RwLock<BridgeState>>,
    config: &BridgeConfig,
) -> Result<(String, u32)> {
    // Wait for Version and Authenticate messages
    let username = loop {
        let msg = mumble_framing::read_message(reader).await?;
        match MessageType::from_u16(msg.msg_type) {
            Some(MessageType::Version) => {
                if let Ok(v) = mumble::Version::decode(&*msg.payload) {
                    info!(
                        release = v.release.as_deref().unwrap_or("unknown"),
                        os = v.os.as_deref().unwrap_or("unknown"),
                        "Mumble client version"
                    );
                }
            }
            Some(MessageType::Authenticate) => {
                let auth = mumble::Authenticate::decode(&*msg.payload)?;
                let name = auth.username.unwrap_or_default();
                if name.is_empty() {
                    let reject = mumble::Reject {
                        r#type: Some(mumble::reject::RejectType::InvalidUsername.into()),
                        reason: Some("Username cannot be empty".to_string()),
                    };
                    write_message(writer, MessageType::Reject, &reject).await?;
                    anyhow::bail!("Rejected: empty username");
                }
                break name;
            }
            other => {
                warn!(?other, msg_type = msg.msg_type, "Unexpected message during auth");
            }
        }
    };

    // Allocate session and register client
    let session;
    let channels;
    let users;
    {
        let mut state = write_bridge(&bridge_state);
        session = state.allocate_mumble_session();
        state.mumble_clients.insert(
            session,
            MumbleClient {
                session,
                username: username.clone(),
                channel_id: 0, // Start in root channel
                is_muted: false,
                is_deafened: false,
            },
        );

        // Snapshot current state for sending to the client
        channels = build_channel_states(&mut state);
        users = build_user_states(&state, session);
    }

    // Send server Version
    let version = mumble::Version {
        version_v2: Some(0x00010500), // 1.5.0
        release: Some("Rumble Mumble Bridge 0.1.0".to_string()),
        os: Some(std::env::consts::OS.to_string()),
        os_version: Some(String::new()),
        version_v1: None,
    };
    write_message(writer, MessageType::Version, &version).await?;

    // Send CryptSetup with random keys (TCP-only, but avoids known-zero crypto material)
    let crypt = mumble::CryptSetup {
        key: Some(rand::random::<[u8; 16]>().to_vec()),
        client_nonce: Some(rand::random::<[u8; 16]>().to_vec()),
        server_nonce: Some(rand::random::<[u8; 16]>().to_vec()),
    };
    write_message(writer, MessageType::CryptSetup, &crypt).await?;

    // Send CodecVersion with opus = true
    let codec = mumble::CodecVersion {
        alpha: -2147483637, // CELT 0.7.0
        beta: 0,
        prefer_alpha: false,
        opus: Some(true),
    };
    write_message(writer, MessageType::CodecVersion, &codec).await?;

    // Send ChannelState for each channel
    for ch in &channels {
        write_message(writer, MessageType::ChannelState, ch).await?;
    }

    // Send UserState for each existing user
    for us in &users {
        write_message(writer, MessageType::UserState, us).await?;
    }

    // Send UserState for the connecting client itself
    let self_user = mumble::UserState {
        session: Some(session),
        name: Some(username.clone()),
        channel_id: Some(0), // Root channel
        ..Default::default()
    };
    write_message(writer, MessageType::UserState, &self_user).await?;

    // Use the Rumble server's welcome message if available, otherwise fall back to config
    let welcome_text = {
        let state = read_bridge(&bridge_state);
        state
            .welcome_message
            .clone()
            .unwrap_or_else(|| config.welcome_text.clone())
    };

    // Send ServerSync
    let sync = mumble::ServerSync {
        session: Some(session),
        max_bandwidth: Some(config.max_bandwidth),
        welcome_text: Some(welcome_text.clone()),
        permissions: Some(0x07FFFFFF), // All permissions
    };
    write_message(writer, MessageType::ServerSync, &sync).await?;

    // Send ServerConfig
    let server_config = mumble::ServerConfig {
        max_bandwidth: Some(config.max_bandwidth),
        welcome_text: Some(welcome_text),
        allow_html: Some(true),
        message_length: Some(5000),
        image_message_length: Some(131072),
        max_users: Some(100),
        recording_allowed: Some(false),
    };
    write_message(writer, MessageType::ServerConfig, &server_config).await?;

    Ok((username, session))
}

/// Build ChannelState messages for all known rooms.
///
/// Channels are returned in BFS order (root first, then children) to match
/// the Mumble reference server behavior. Mumble clients expect parent channels
/// to be sent before their children.
fn build_channel_states(state: &mut BridgeState) -> Vec<mumble::ChannelState> {
    use std::collections::{HashMap, VecDeque};

    // First pass: assign Mumble IDs and build a parent->children map
    let mut children_of: HashMap<uuid::Uuid, Vec<usize>> = HashMap::new();
    let mut room_uuids: Vec<Option<uuid::Uuid>> = Vec::new();

    for (idx, room) in state.rumble_rooms.iter().enumerate() {
        let uuid = room.id.as_ref().and_then(api::uuid_from_room_id);
        room_uuids.push(uuid);
        if let Some(uuid) = uuid {
            // Ensure channel IDs are assigned
            state.channels.get_or_insert(uuid);

            let parent_uuid = room
                .parent_id
                .as_ref()
                .and_then(api::uuid_from_room_id)
                .unwrap_or(api::ROOT_ROOM_UUID);

            if uuid != api::ROOT_ROOM_UUID {
                children_of.entry(parent_uuid).or_default().push(idx);
            }
        }
    }

    // BFS from root to produce parent-before-child ordering
    let mut channels = Vec::new();
    let mut queue = VecDeque::new();

    // Find the root room index
    let root_idx = room_uuids.iter().position(|u| *u == Some(api::ROOT_ROOM_UUID));

    if let Some(root_idx) = root_idx {
        queue.push_back(root_idx);
    } else {
        // No root room in state — synthesize one
        channels.push(mumble::ChannelState {
            channel_id: Some(0),
            parent: None,
            name: Some("Root".to_string()),
            ..Default::default()
        });
        // Still enqueue children of ROOT_ROOM_UUID
        if let Some(children) = children_of.get(&api::ROOT_ROOM_UUID) {
            for &idx in children {
                queue.push_back(idx);
            }
        }
    }

    while let Some(idx) = queue.pop_front() {
        let room = &state.rumble_rooms[idx];
        let uuid = match room_uuids[idx] {
            Some(u) => u,
            None => continue,
        };

        let channel_id = state.channels.get_or_insert(uuid);
        let parent = if uuid == api::ROOT_ROOM_UUID {
            None
        } else {
            let parent_uuid = room
                .parent_id
                .as_ref()
                .and_then(api::uuid_from_room_id)
                .unwrap_or(api::ROOT_ROOM_UUID);
            Some(state.channels.get_or_insert(parent_uuid))
        };

        channels.push(mumble::ChannelState {
            channel_id: Some(channel_id),
            parent,
            name: Some(room.name.clone()),
            description: room.description.clone(),
            ..Default::default()
        });

        // Enqueue children of this room
        if let Some(children) = children_of.get(&uuid) {
            for &child_idx in children {
                queue.push_back(child_idx);
            }
        }
    }

    channels
}

/// Build UserState messages for all known Rumble users.
fn build_user_states(state: &BridgeState, exclude_session: u32) -> Vec<mumble::UserState> {
    let mut users = Vec::new();

    for user in &state.rumble_users {
        let rumble_id = user.user_id.as_ref().map(|id| id.value).unwrap_or(0);

        // Skip the bridge's own user
        if Some(rumble_id) == state.bridge_user_id {
            continue;
        }

        let session = state.users.get_mumble_session(rumble_id);
        if let Some(session) = session {
            let channel_id = user
                .current_room
                .as_ref()
                .and_then(api::uuid_from_room_id)
                .and_then(|uuid| state.channels.get_mumble_id(&uuid));

            users.push(mumble::UserState {
                session: Some(session),
                name: Some(user.username.clone()),
                channel_id,
                self_mute: Some(user.is_muted),
                self_deaf: Some(user.is_deafened),
                ..Default::default()
            });
        }
    }

    // Also include other Mumble clients
    for client in state.mumble_clients.values() {
        if client.session == exclude_session {
            continue;
        }
        users.push(mumble::UserState {
            session: Some(client.session),
            name: Some(client.username.clone()),
            channel_id: Some(client.channel_id),
            self_mute: Some(client.is_muted),
            self_deaf: Some(client.is_deafened),
            ..Default::default()
        });
    }

    users
}

/// Process messages from a connected Mumble client.
async fn mumble_message_loop(
    reader: &mut ReadHalf<TlsStream<tokio::net::TcpStream>>,
    session: u32,
    bridge_tx: &mpsc::UnboundedSender<BridgeEvent>,
) -> Result<()> {
    loop {
        let msg = mumble_framing::read_message(reader).await?;

        match MessageType::from_u16(msg.msg_type) {
            Some(MessageType::Ping) => {
                // Echo ping back via the bridge (which will send it to the client's outbound channel)
                bridge_tx.send(BridgeEvent::MumblePing {
                    session,
                    payload: msg.payload,
                })?;
            }
            Some(MessageType::UdpTunnel) => {
                // Voice data - forward to bridge for relay
                bridge_tx.send(BridgeEvent::MumbleVoice {
                    session,
                    data: msg.payload,
                })?;
            }
            Some(MessageType::TextMessage) => {
                if let Ok(text_msg) = mumble::TextMessage::decode(&*msg.payload) {
                    if let Some(message) = text_msg.message {
                        bridge_tx.send(BridgeEvent::MumbleChat {
                            session,
                            message,
                            target_sessions: text_msg.session,
                            target_tree_ids: text_msg.tree_id,
                        })?;
                    }
                }
            }
            Some(MessageType::UserState) => {
                // Client might be changing channel or mute/deaf state
                if let Ok(user_state) = mumble::UserState::decode(&*msg.payload) {
                    if let Some(channel_id) = user_state.channel_id {
                        bridge_tx.send(BridgeEvent::MumbleChannelChange { session, channel_id })?;
                    }
                    if user_state.self_mute.is_some() || user_state.self_deaf.is_some() {
                        bridge_tx.send(BridgeEvent::MumbleMuteDeafChange {
                            session,
                            is_muted: user_state.self_mute,
                            is_deafened: user_state.self_deaf,
                        })?;
                    }
                }
            }
            Some(msg_type) => {
                // Log but ignore unhandled message types
                tracing::trace!(?msg_type, session, "Ignoring Mumble message");
            }
            None => {
                warn!(msg_type = msg.msg_type, "Unknown Mumble message type");
            }
        }
    }
}

/// Write a raw framed message (type + payload already encoded).
async fn write_raw_frame(
    writer: &mut WriteHalf<TlsStream<tokio::net::TcpStream>>,
    msg_type: u16,
    payload: &[u8],
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut frame = Vec::with_capacity(6 + payload.len());
    frame.extend_from_slice(&msg_type.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    writer.write_all(&frame).await?;
    Ok(())
}
