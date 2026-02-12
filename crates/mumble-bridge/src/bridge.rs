use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use anyhow::Result;
use api::proto::{self, envelope::Payload};
use prost::Message;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{
    config::BridgeConfig,
    mumble_proto::{MessageType, mumble},
    mumble_server::MumbleOutbound,
    mumble_voice, rumble_client,
    state::BridgeState,
};

/// Acquire a read lock, recovering from poison if another thread panicked.
pub fn read_bridge(state: &RwLock<BridgeState>) -> std::sync::RwLockReadGuard<'_, BridgeState> {
    state.read().unwrap_or_else(|e| e.into_inner())
}

/// Acquire a write lock, recovering from poison if another thread panicked.
pub fn write_bridge(state: &RwLock<BridgeState>) -> std::sync::RwLockWriteGuard<'_, BridgeState> {
    state.write().unwrap_or_else(|e| e.into_inner())
}

/// Events from Mumble clients or the Rumble connection, funneled into the bridge.
#[derive(Debug)]
pub enum BridgeEvent {
    /// A Mumble client completed authentication.
    MumbleClientJoined { session: u32, username: String },
    /// A Mumble client disconnected.
    MumbleClientLeft { session: u32 },
    /// Register a sender for a Mumble client.
    MumbleClientSender {
        session: u32,
        sender: mpsc::UnboundedSender<MumbleOutbound>,
    },
    /// Mumble client sent a ping.
    MumblePing { session: u32, payload: Vec<u8> },
    /// Mumble client sent voice data.
    MumbleVoice { session: u32, data: Vec<u8> },
    /// Mumble client sent a text message.
    MumbleChat {
        session: u32,
        message: String,
        /// Target session IDs for private messages (Mumble TextMessage.session field).
        target_sessions: Vec<u32>,
        /// Target tree IDs for tree messages (Mumble TextMessage.tree_id field).
        target_tree_ids: Vec<u32>,
    },
    /// Mumble client changed channel.
    MumbleChannelChange { session: u32, channel_id: u32 },
    /// Mumble client changed mute/deaf state.
    MumbleMuteDeafChange {
        session: u32,
        is_muted: Option<bool>,
        is_deafened: Option<bool>,
    },
    /// Received a Rumble envelope from the server.
    RumbleEnvelope(proto::Envelope),
    /// Received a Rumble voice datagram.
    RumbleVoice(proto::VoiceDatagram),
}

/// Per-client sender handle.
pub(crate) struct ClientSender {
    pub tx: mpsc::UnboundedSender<MumbleOutbound>,
}

/// Persistent state that survives across reconnects.
pub struct BridgeLoopState {
    /// Outbound channels for each connected Mumble client.
    pub(crate) client_senders: HashMap<u32, ClientSender>,
    /// Per-Mumble-session sequence counters (Mumble->Rumble direction).
    pub(crate) mumble_to_rumble_seq: HashMap<u32, u32>,
    /// Per-Rumble-user outbound sequence counters (Rumble->Mumble direction).
    pub(crate) rumble_outbound_seq: HashMap<u64, u64>,
}

impl BridgeLoopState {
    pub fn new() -> Self {
        Self {
            client_senders: HashMap::new(),
            mumble_to_rumble_seq: HashMap::new(),
            rumble_outbound_seq: HashMap::new(),
        }
    }
}

/// Run the core bridge event loop.
///
/// Consumes events from both Mumble clients and the Rumble connection,
/// translating and forwarding messages between the two protocols.
/// Returns when the shutdown signal fires or the event channel closes.
/// `loop_state` persists across reconnects to preserve client senders and
/// sequence counters.
pub async fn run_bridge(
    config: Arc<BridgeConfig>,
    bridge_state: Arc<RwLock<BridgeState>>,
    mut bridge_rx: mpsc::UnboundedReceiver<BridgeEvent>,
    rumble_conn: quinn::Connection,
    rumble_send: &mut quinn::SendStream,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    loop_state: &mut BridgeLoopState,
) -> (Result<()>, mpsc::UnboundedReceiver<BridgeEvent>) {
    let client_senders = &mut loop_state.client_senders;
    let rumble_outbound_seq = &mut loop_state.rumble_outbound_seq;
    let mumble_to_rumble_seq = &mut loop_state.mumble_to_rumble_seq;
    // (virtual_user_id, mumble_session) pairs that need BridgeJoinRoom after registration
    let mut pending_join_rooms: Vec<(u64, u32)> = Vec::new();
    // Virtual user IDs that arrived late (Mumble client already left) and need cleanup
    let mut pending_unregister: Vec<u64> = Vec::new();

    info!("Bridge event loop started");

    let mut shutdown_requested = false;

    loop {
        let event = tokio::select! {
            event = bridge_rx.recv() => {
                match event {
                    Some(e) => e,
                    None => break,
                }
            }
            _ = shutdown_rx.changed() => {
                info!("Shutdown signal received");
                shutdown_requested = true;
                break;
            }
            reason = rumble_conn.closed() => {
                warn!(%reason, "Rumble QUIC connection closed");
                break;
            }
        };
        match event {
            BridgeEvent::MumbleClientJoined { session, username } => {
                info!(session, %username, "Mumble client joined bridge");

                // Notify all existing Mumble clients about the new user
                let user_state = mumble::UserState {
                    session: Some(session),
                    name: Some(username.clone()),
                    channel_id: Some(0),
                    ..Default::default()
                };
                broadcast_to_mumble_except(client_senders, session, MessageType::UserState, &user_state);

                // Register this Mumble client as a virtual user on the Rumble server
                {
                    let mut state = write_bridge(&bridge_state);
                    state.pending_registrations.push((username.clone(), session));
                }
                if let Err(e) = rumble_client::send_bridge_register_user(rumble_send, &username).await {
                    warn!(error = %e, %username, "Failed to send BridgeRegisterUser");
                }
            }

            BridgeEvent::MumbleClientLeft { session } => {
                client_senders.remove(&session);
                mumble_to_rumble_seq.remove(&session);

                let (username, virtual_user_id) = {
                    let mut state = write_bridge(&bridge_state);
                    let username = state.mumble_clients.get(&session).map(|c| c.username.clone());
                    let virtual_user_id = state.virtual_user_map.remove(&session);
                    if let Some(vid) = virtual_user_id {
                        state.reverse_virtual_user_map.remove(&vid);
                    }
                    // Also clean up any pending registration for this session
                    state.pending_registrations.retain(|(_, s)| *s != session);
                    (username, virtual_user_id)
                };
                info!(session, username = ?username, virtual_user_id = ?virtual_user_id, "Mumble client left bridge");

                // Unregister the virtual user on the Rumble server
                if let Some(vid) = virtual_user_id {
                    if let Err(e) = rumble_client::send_bridge_unregister_user(rumble_send, vid).await {
                        warn!(error = %e, vid, "Failed to send BridgeUnregisterUser");
                    }
                }

                // Notify remaining Mumble clients
                let remove = mumble::UserRemove {
                    session,
                    actor: None,
                    reason: Some("Disconnected".to_string()),
                    ban: None,
                    ban_certificate: None,
                    ban_ip: None,
                };
                broadcast_to_all_mumble(client_senders, MessageType::UserRemove, &remove);
            }

            BridgeEvent::MumbleClientSender { session, sender } => {
                client_senders.insert(session, ClientSender { tx: sender });
            }

            BridgeEvent::MumblePing { session, payload } => {
                if let Some(client) = client_senders.get(&session) {
                    // Parse the client's Ping, preserve timestamp, zero server stats
                    let response = if let Ok(ping) = mumble::Ping::decode(&*payload) {
                        mumble::Ping {
                            timestamp: ping.timestamp,
                            good: Some(0),
                            late: Some(0),
                            lost: Some(0),
                            resync: Some(0),
                            udp_packets: Some(0),
                            tcp_packets: Some(0),
                            udp_ping_avg: Some(0.0),
                            udp_ping_var: Some(0.0),
                            tcp_ping_avg: Some(0.0),
                            tcp_ping_var: Some(0.0),
                        }
                        .encode_to_vec()
                    } else {
                        payload
                    };
                    let _ = client.tx.send(MumbleOutbound::Protobuf {
                        msg_type: MessageType::Ping as u16,
                        payload: response,
                    });
                }
            }

            BridgeEvent::MumbleVoice { session, data } => {
                // Look up the virtual user ID for this Mumble session
                let virtual_user_id = {
                    let state = read_bridge(&bridge_state);
                    state.virtual_user_map.get(&session).copied()
                };

                // Drop voice if the virtual user hasn't been registered yet
                let virtual_user_id = match virtual_user_id {
                    Some(id) => id,
                    None => continue,
                };

                // Parse the Mumble voice packet and forward to Rumble as datagram
                if let Some(voice) = mumble_voice::parse_voice_packet(&data) {
                    // Use bridge-owned sequence counter instead of Mumble's sequence.
                    // Mumble sequences increment by iFramesPerPacket (e.g. 2) per packet,
                    // but Rumble's jitter buffer expects consecutive per-packet sequences.
                    let seq = mumble_to_rumble_seq.entry(session).or_insert(0);
                    *seq = seq.wrapping_add(1);
                    let datagram = proto::VoiceDatagram {
                        opus_data: voice.opus_data,
                        sequence: *seq,
                        timestamp_us: 0,
                        end_of_stream: voice.is_last,
                        sender_id: Some(virtual_user_id),
                        room_id: None,
                    };
                    let encoded = datagram.encode_to_vec();
                    if let Err(e) = rumble_conn.send_datagram(encoded.into()) {
                        warn!(error = %e, "Failed to send voice datagram to Rumble");
                    }
                }
            }

            BridgeEvent::MumbleChat {
                session,
                message,
                target_sessions,
                target_tree_ids,
            } => {
                // Look up the virtual user ID for per-user chat attribution
                let virtual_user_id = {
                    let state = read_bridge(&bridge_state);
                    state.virtual_user_map.get(&session).copied()
                };

                if !target_sessions.is_empty() {
                    // Private message targeting specific sessions.
                    // Resolve Mumble session to Rumble user ID while holding the lock,
                    // then drop the lock before the async send.
                    let target_rumble_id = {
                        let state = read_bridge(&bridge_state);
                        target_sessions.iter().find_map(|&ts| {
                            state
                                .virtual_user_map
                                .get(&ts)
                                .copied()
                                .or_else(|| state.users.get_rumble_id(ts))
                        })
                    };
                    if let Some(target_id) = target_rumble_id {
                        if let Err(e) = rumble_client::send_direct_message(rumble_send, target_id, &message).await {
                            warn!(error = %e, "Failed to send DM to Rumble");
                        }
                    }
                } else if !target_tree_ids.is_empty() {
                    // Tree message - send as tree chat to Rumble
                    let sender_name = {
                        let state = read_bridge(&bridge_state);
                        state
                            .mumble_clients
                            .get(&session)
                            .map(|c| c.username.clone())
                            .unwrap_or_else(|| format!("session-{}", session))
                    };
                    if let Err(e) = rumble_client::send_tree_chat(rumble_send, &sender_name, &message).await {
                        warn!(error = %e, "Failed to send tree chat to Rumble");
                    }
                } else if let Some(vid) = virtual_user_id {
                    // Normal channel message - send as the virtual user via bridge protocol
                    if let Err(e) = rumble_client::send_bridge_chat_message(rumble_send, vid, &message).await {
                        warn!(error = %e, "Failed to send BridgeChatMessage to Rumble");
                    }
                } else {
                    // Virtual user not registered yet, fall back to prefixed bridge chat
                    let username = {
                        let state = read_bridge(&bridge_state);
                        state
                            .mumble_clients
                            .get(&session)
                            .map(|c| c.username.clone())
                            .unwrap_or_else(|| format!("session-{}", session))
                    };
                    let prefixed = format!("[{}] {}", username, message);
                    if let Err(e) = rumble_client::send_chat(rumble_send, &config.bridge_name, &prefixed).await {
                        warn!(error = %e, "Failed to send chat to Rumble");
                    }
                }

                // Also broadcast to other Mumble clients
                let sender_channel_id = {
                    let state = read_bridge(&bridge_state);
                    state.mumble_clients.get(&session).map(|c| c.channel_id).unwrap_or(0)
                };
                let text_msg = mumble::TextMessage {
                    actor: Some(session),
                    channel_id: vec![sender_channel_id],
                    message: Some(message),
                    ..Default::default()
                };
                broadcast_to_mumble_except(client_senders, session, MessageType::TextMessage, &text_msg);
            }

            BridgeEvent::MumbleChannelChange { session, channel_id } => {
                let (virtual_user_id, room_uuid) = {
                    let mut state = write_bridge(&bridge_state);
                    if let Some(client) = state.mumble_clients.get_mut(&session) {
                        client.channel_id = channel_id;
                    }
                    let vid = state.virtual_user_map.get(&session).copied();
                    let room_uuid = state.channels.get_rumble_uuid(channel_id);
                    (vid, room_uuid)
                };

                // Move the virtual user to the corresponding Rumble room
                if let (Some(vid), Some(uuid)) = (virtual_user_id, room_uuid) {
                    let room_id = api::room_id_from_uuid(uuid);
                    if let Err(e) = rumble_client::send_bridge_join_room(rumble_send, vid, room_id).await {
                        warn!(error = %e, vid, "Failed to send BridgeJoinRoom");
                    }
                }

                let user_state = mumble::UserState {
                    session: Some(session),
                    channel_id: Some(channel_id),
                    ..Default::default()
                };
                broadcast_to_all_mumble(client_senders, MessageType::UserState, &user_state);
            }

            BridgeEvent::MumbleMuteDeafChange {
                session,
                is_muted,
                is_deafened,
            } => {
                let (virtual_user_id, final_muted, final_deafened) = {
                    let mut state = write_bridge(&bridge_state);
                    if let Some(client) = state.mumble_clients.get_mut(&session) {
                        if let Some(m) = is_muted {
                            client.is_muted = m;
                        }
                        if let Some(d) = is_deafened {
                            client.is_deafened = d;
                        }
                        // Enforce Mumble invariant: deaf implies mute, unmute clears deaf
                        if client.is_deafened {
                            client.is_muted = true;
                        }
                        if !client.is_muted {
                            client.is_deafened = false;
                        }
                    }
                    let vid = state.virtual_user_map.get(&session).copied();
                    let client = state.mumble_clients.get(&session);
                    let m = client.map(|c| c.is_muted).unwrap_or(false);
                    let d = client.map(|c| c.is_deafened).unwrap_or(false);
                    (vid, m, d)
                };

                if let Some(vid) = virtual_user_id {
                    if let Err(e) =
                        rumble_client::send_bridge_set_user_status(rumble_send, vid, final_muted, final_deafened).await
                    {
                        warn!(error = %e, vid, "Failed to send BridgeSetUserStatus");
                    }
                }

                // Broadcast the enforced mute/deaf state to other Mumble clients
                let user_state = mumble::UserState {
                    session: Some(session),
                    self_mute: Some(final_muted),
                    self_deaf: Some(final_deafened),
                    ..Default::default()
                };
                broadcast_to_mumble_except(client_senders, session, MessageType::UserState, &user_state);
            }

            BridgeEvent::RumbleEnvelope(env) => {
                handle_rumble_envelope(
                    env,
                    &bridge_state,
                    client_senders,
                    rumble_outbound_seq,
                    &mut pending_join_rooms,
                    &mut pending_unregister,
                );

                // Process any pending join-room requests from BridgeUserRegistered
                for (vid, session) in pending_join_rooms.drain(..) {
                    let (room_id, is_muted, is_deafened) = {
                        let state = read_bridge(&bridge_state);
                        let client = state.mumble_clients.get(&session);
                        let channel_id = client.map(|c| c.channel_id);
                        let room_id = channel_id
                            .and_then(|ch| state.channels.get_rumble_uuid(ch))
                            .map(api::room_id_from_uuid)
                            .unwrap_or_else(api::root_room_id);
                        let is_muted = client.map(|c| c.is_muted).unwrap_or(false);
                        let is_deafened = client.map(|c| c.is_deafened).unwrap_or(false);
                        (room_id, is_muted, is_deafened)
                    };
                    if let Err(e) = rumble_client::send_bridge_join_room(rumble_send, vid, room_id).await {
                        warn!(error = %e, vid, "Failed to send BridgeJoinRoom for new virtual user");
                    }
                    // Sync mute/deaf state for the newly registered virtual user
                    if is_muted || is_deafened {
                        if let Err(e) =
                            rumble_client::send_bridge_set_user_status(rumble_send, vid, is_muted, is_deafened).await
                        {
                            warn!(error = %e, vid, "Failed to sync mute/deaf state for new virtual user");
                        }
                    }
                }

                // Clean up orphaned virtual users (Mumble client left before registration completed)
                for vid in pending_unregister.drain(..) {
                    if let Err(e) = rumble_client::send_bridge_unregister_user(rumble_send, vid).await {
                        warn!(error = %e, vid, "Failed to send BridgeUnregisterUser for orphaned virtual user");
                    }
                }
            }

            BridgeEvent::RumbleVoice(datagram) => {
                handle_rumble_voice(datagram, &bridge_state, client_senders, rumble_outbound_seq);
            }
        }
    }

    // Only attempt graceful cleanup on intentional shutdown (signal),
    // not when the QUIC connection died underneath us.
    if shutdown_requested {
        let virtual_users: Vec<u64> = {
            let state = read_bridge(&bridge_state);
            state.virtual_user_map.values().copied().collect()
        };
        if !virtual_users.is_empty() {
            info!(count = virtual_users.len(), "Unregistering virtual users on shutdown");
            for vid in virtual_users {
                if let Err(e) = rumble_client::send_bridge_unregister_user(rumble_send, vid).await {
                    warn!(error = %e, vid, "Failed to unregister virtual user on shutdown");
                }
            }
        }
        rumble_conn.close(0u32.into(), b"bridge shutdown");
    }

    info!("Bridge event loop ended");
    (Ok(()), bridge_rx)
}

/// Handle a Rumble server envelope and forward relevant events to Mumble clients.
fn handle_rumble_envelope(
    env: proto::Envelope,
    bridge_state: &Arc<RwLock<BridgeState>>,
    client_senders: &HashMap<u32, ClientSender>,
    rumble_outbound_seq: &mut HashMap<u64, u64>,
    pending_join_rooms: &mut Vec<(u64, u32)>,
    pending_unregister: &mut Vec<u64>,
) {
    match env.payload {
        Some(Payload::ServerEvent(se)) => {
            if let Some(kind) = se.kind {
                match kind {
                    proto::server_event::Kind::ServerState(ss) => {
                        let mut state = write_bridge(&bridge_state);
                        state.rumble_rooms = ss.rooms.clone();
                        state.rumble_users = ss.users.clone();

                        for user in &ss.users {
                            let rumble_id = user.user_id.as_ref().map(|id| id.value).unwrap_or(0);
                            if Some(rumble_id) != state.bridge_user_id
                                && !state.reverse_virtual_user_map.contains_key(&rumble_id)
                            {
                                state.users.get_or_insert(rumble_id);
                            }
                        }
                        for room in &ss.rooms {
                            if let Some(uuid) = room.id.as_ref().and_then(api::uuid_from_room_id) {
                                state.channels.get_or_insert(uuid);
                            }
                        }
                        debug!(rooms = ss.rooms.len(), users = ss.users.len(), "Rumble state refreshed");
                    }

                    proto::server_event::Kind::StateUpdate(su) => {
                        handle_state_update(su, bridge_state, client_senders, rumble_outbound_seq);
                    }

                    proto::server_event::Kind::ChatBroadcast(cb) => {
                        let text = format!("{}: {}", cb.sender, cb.text);

                        // Resolve the sender username to a Mumble session ID for proper
                        // message attribution in Mumble clients.
                        let actor = {
                            let state = read_bridge(&bridge_state);
                            // Find the Rumble user_id for this sender by matching username
                            let rumble_user_id = state.rumble_users.iter().find_map(|u| {
                                if u.username == cb.sender {
                                    u.user_id.as_ref().map(|id| id.value)
                                } else {
                                    None
                                }
                            });
                            rumble_user_id.and_then(|rid| {
                                // Check if this is one of our virtual users (Mumble client)
                                state
                                    .reverse_virtual_user_map
                                    .get(&rid)
                                    .copied()
                                    // Otherwise look up as a remote Rumble user
                                    .or_else(|| state.users.get_mumble_session(rid))
                            })
                        };

                        let text_msg = mumble::TextMessage {
                            actor,
                            channel_id: vec![0],
                            message: Some(text),
                            ..Default::default()
                        };
                        broadcast_to_all_mumble(client_senders, MessageType::TextMessage, &text_msg);
                    }

                    proto::server_event::Kind::DirectMessageReceived(dm) => {
                        // A DM directed at one of our virtual users.
                        // Use target_user_id to find the correct Mumble session.
                        let target_session = {
                            let state = read_bridge(&bridge_state);
                            state.reverse_virtual_user_map.get(&dm.target_user_id).copied()
                        };

                        if let Some(session) = target_session {
                            let actor = {
                                let state = read_bridge(&bridge_state);
                                state.users.get_mumble_session(dm.sender_id)
                            };
                            let text = format!("[DM] {}: {}", dm.sender_name, dm.text);
                            let text_msg = mumble::TextMessage {
                                actor,
                                message: Some(text),
                                ..Default::default()
                            };
                            send_to_mumble_session(client_senders, session, MessageType::TextMessage, &text_msg);
                        } else {
                            debug!(
                                target_user_id = dm.target_user_id,
                                "DM target is not a virtual user on this bridge, ignoring"
                            );
                        }
                    }

                    proto::server_event::Kind::KeepAlive(_) => {}

                    proto::server_event::Kind::WelcomeMessage(wm) => {
                        info!("Received welcome message from Rumble server");
                        let mut state = write_bridge(&bridge_state);
                        state.welcome_message = Some(wm.text);
                    }
                }
            }
        }
        Some(Payload::BridgeUserRegistered(bur)) => {
            let mut state = write_bridge(&bridge_state);
            let pending_idx = state
                .pending_registrations
                .iter()
                .position(|(name, _)| name == &bur.username);
            if let Some(session) = pending_idx.map(|i| state.pending_registrations.remove(i).1) {
                state.virtual_user_map.insert(session, bur.user_id);
                state.reverse_virtual_user_map.insert(bur.user_id, session);
                info!(
                    session,
                    virtual_user_id = bur.user_id,
                    username = %bur.username,
                    "Virtual user registered"
                );

                // Place the virtual user in the correct room
                pending_join_rooms.push((bur.user_id, session));
            } else {
                // Mumble client disconnected while registration was pending.
                // The virtual user was created on the server but has no owner -- clean it up.
                warn!(
                    username = %bur.username,
                    user_id = bur.user_id,
                    "BridgeUserRegistered for departed client, scheduling cleanup"
                );
                pending_unregister.push(bur.user_id);
            }
        }
        Some(Payload::CommandResult(cr)) => {
            debug!(success = cr.success, message = %cr.message, "Rumble command result");
        }
        _ => {}
    }
}

/// Handle a Rumble StateUpdate and forward to Mumble clients.
fn handle_state_update(
    su: proto::StateUpdate,
    bridge_state: &Arc<RwLock<BridgeState>>,
    client_senders: &HashMap<u32, ClientSender>,
    rumble_outbound_seq: &mut HashMap<u64, u64>,
) {
    match su.update {
        Some(proto::state_update::Update::UserJoined(uj)) => {
            if let Some(user) = uj.user {
                let rumble_id = user.user_id.as_ref().map(|id| id.value).unwrap_or(0);

                let mut state = write_bridge(&bridge_state);
                if Some(rumble_id) == state.bridge_user_id {
                    return;
                }
                // Skip events for our own virtual users (already registered)
                if state.reverse_virtual_user_map.contains_key(&rumble_id) {
                    return;
                }
                // Also skip if this user matches a pending registration —
                // the UserJoined can arrive before BridgeUserRegistered,
                // and we don't want to broadcast the virtual user as a
                // duplicate to Mumble clients who already have their own session.
                if state
                    .pending_registrations
                    .iter()
                    .any(|(name, _)| name == &user.username)
                {
                    return;
                }

                let session = state.users.get_or_insert(rumble_id);
                let channel_id = user
                    .current_room
                    .as_ref()
                    .and_then(api::uuid_from_room_id)
                    .map(|uuid| state.channels.get_or_insert(uuid))
                    .unwrap_or(0);

                state.rumble_users.push(user.clone());

                let user_state = mumble::UserState {
                    session: Some(session),
                    name: Some(user.username.clone()),
                    channel_id: Some(channel_id),
                    self_mute: Some(user.is_muted),
                    self_deaf: Some(user.is_deafened),
                    ..Default::default()
                };
                drop(state);
                broadcast_to_all_mumble(client_senders, MessageType::UserState, &user_state);
                info!(rumble_id, session, "Rumble user joined -> Mumble UserState");
            }
        }

        Some(proto::state_update::Update::UserLeft(ul)) => {
            let rumble_id = ul.user_id.as_ref().map(|id| id.value).unwrap_or(0);

            let mut state = write_bridge(&bridge_state);
            // Skip events for our own virtual users
            if state.reverse_virtual_user_map.contains_key(&rumble_id) {
                return;
            }
            let session = state.users.get_mumble_session(rumble_id);
            state.users.remove_by_rumble_id(rumble_id);
            state
                .rumble_users
                .retain(|u| u.user_id.as_ref().map(|id| id.value).unwrap_or(0) != rumble_id);

            // Clean up outbound sequence counter for this user
            rumble_outbound_seq.remove(&rumble_id);

            if let Some(session) = session {
                let remove = mumble::UserRemove {
                    session,
                    actor: None,
                    reason: Some("Left".to_string()),
                    ban: None,
                    ban_certificate: None,
                    ban_ip: None,
                };
                drop(state);
                broadcast_to_all_mumble(client_senders, MessageType::UserRemove, &remove);
                info!(rumble_id, session, "Rumble user left -> Mumble UserRemove");
            }
        }

        Some(proto::state_update::Update::UserMoved(um)) => {
            let rumble_id = um.user_id.as_ref().map(|id| id.value).unwrap_or(0);
            let mut state = write_bridge(&bridge_state);
            // Skip events for our own virtual users
            if state.reverse_virtual_user_map.contains_key(&rumble_id) {
                return;
            }
            let session = state.users.get_mumble_session(rumble_id);
            let channel_id = um
                .to_room_id
                .as_ref()
                .and_then(api::uuid_from_room_id)
                .map(|uuid| state.channels.get_or_insert(uuid));

            if let (Some(session), Some(channel_id)) = (session, channel_id) {
                let user_state = mumble::UserState {
                    session: Some(session),
                    channel_id: Some(channel_id),
                    ..Default::default()
                };
                drop(state);
                broadcast_to_all_mumble(client_senders, MessageType::UserState, &user_state);
            }
        }

        Some(proto::state_update::Update::UserStatusChanged(usc)) => {
            let rumble_id = usc.user_id.as_ref().map(|id| id.value).unwrap_or(0);
            let state = read_bridge(&bridge_state);
            // Skip events for our own virtual users
            if state.reverse_virtual_user_map.contains_key(&rumble_id) {
                return;
            }
            let session = state.users.get_mumble_session(rumble_id);

            if let Some(session) = session {
                let user_state = mumble::UserState {
                    session: Some(session),
                    self_mute: Some(usc.is_muted),
                    self_deaf: Some(usc.is_deafened),
                    ..Default::default()
                };
                drop(state);
                broadcast_to_all_mumble(client_senders, MessageType::UserState, &user_state);
            }
        }

        Some(proto::state_update::Update::RoomCreated(rc)) => {
            if let Some(room) = rc.room {
                let mut state = write_bridge(&bridge_state);
                let uuid = room.id.as_ref().and_then(api::uuid_from_room_id);
                if let Some(uuid) = uuid {
                    let channel_id = state.channels.get_or_insert(uuid);
                    let parent = room
                        .parent_id
                        .as_ref()
                        .and_then(api::uuid_from_room_id)
                        .map(|p| state.channels.get_or_insert(p))
                        .unwrap_or(0);

                    state.rumble_rooms.push(room.clone());

                    let channel_state = mumble::ChannelState {
                        channel_id: Some(channel_id),
                        parent: Some(parent),
                        name: Some(room.name.clone()),
                        description: room.description.clone(),
                        ..Default::default()
                    };
                    drop(state);
                    broadcast_to_all_mumble(client_senders, MessageType::ChannelState, &channel_state);
                    info!(channel_id, "Rumble room created -> Mumble ChannelState");
                }
            }
        }

        Some(proto::state_update::Update::RoomDeleted(rd)) => {
            let uuid = rd.room_id.as_ref().and_then(api::uuid_from_room_id);
            if let Some(uuid) = uuid {
                let mut state = write_bridge(&bridge_state);
                let channel_id = state.channels.get_mumble_id(&uuid);
                state.channels.remove_by_uuid(&uuid);
                state
                    .rumble_rooms
                    .retain(|r| r.id.as_ref().and_then(api::uuid_from_room_id) != Some(uuid));

                if let Some(channel_id) = channel_id {
                    let remove = mumble::ChannelRemove { channel_id };
                    drop(state);
                    broadcast_to_all_mumble(client_senders, MessageType::ChannelRemove, &remove);
                    info!(channel_id, "Rumble room deleted -> Mumble ChannelRemove");
                }
            }
        }

        Some(proto::state_update::Update::RoomRenamed(rr)) => {
            let uuid = rr.room_id.as_ref().and_then(api::uuid_from_room_id);
            if let Some(uuid) = uuid {
                let state = read_bridge(&bridge_state);
                let channel_id = state.channels.get_mumble_id(&uuid);
                if let Some(channel_id) = channel_id {
                    let channel_state = mumble::ChannelState {
                        channel_id: Some(channel_id),
                        name: Some(rr.new_name.clone()),
                        ..Default::default()
                    };
                    drop(state);
                    broadcast_to_all_mumble(client_senders, MessageType::ChannelState, &channel_state);
                }
            }
        }

        Some(proto::state_update::Update::RoomMoved(rm)) => {
            let uuid = rm.room_id.as_ref().and_then(api::uuid_from_room_id);
            let new_parent_uuid = rm.new_parent_id.as_ref().and_then(api::uuid_from_room_id);
            if let (Some(uuid), Some(parent_uuid)) = (uuid, new_parent_uuid) {
                let state = read_bridge(&bridge_state);
                let channel_id = state.channels.get_mumble_id(&uuid);
                let parent_id = state.channels.get_mumble_id(&parent_uuid);
                if let (Some(channel_id), Some(parent_id)) = (channel_id, parent_id) {
                    let channel_state = mumble::ChannelState {
                        channel_id: Some(channel_id),
                        parent: Some(parent_id),
                        ..Default::default()
                    };
                    drop(state);
                    broadcast_to_all_mumble(client_senders, MessageType::ChannelState, &channel_state);
                }
            }
        }

        Some(proto::state_update::Update::RoomDescriptionChanged(rdc)) => {
            let uuid = rdc.room_id.as_ref().and_then(api::uuid_from_room_id);
            if let Some(uuid) = uuid {
                // Update cached room description
                {
                    let mut state = write_bridge(&bridge_state);
                    if let Some(room) = state
                        .rumble_rooms
                        .iter_mut()
                        .find(|r| r.id.as_ref().and_then(api::uuid_from_room_id) == Some(uuid))
                    {
                        room.description = if rdc.description.is_empty() {
                            None
                        } else {
                            Some(rdc.description.clone())
                        };
                    }
                }

                let state = read_bridge(&bridge_state);
                let channel_id = state.channels.get_mumble_id(&uuid);
                if let Some(channel_id) = channel_id {
                    let description = if rdc.description.is_empty() {
                        None
                    } else {
                        Some(rdc.description.clone())
                    };
                    let channel_state = mumble::ChannelState {
                        channel_id: Some(channel_id),
                        description,
                        ..Default::default()
                    };
                    drop(state);
                    broadcast_to_all_mumble(client_senders, MessageType::ChannelState, &channel_state);
                }
            }
        }

        None => {}
    }
}

/// Handle a Rumble voice datagram and forward to Mumble clients in the matching channel.
fn handle_rumble_voice(
    datagram: proto::VoiceDatagram,
    bridge_state: &Arc<RwLock<BridgeState>>,
    client_senders: &HashMap<u32, ClientSender>,
    outbound_seq: &mut HashMap<u64, u64>,
) {
    if datagram.opus_data.is_empty() && !datagram.end_of_stream {
        return;
    }

    let sender_id = match datagram.sender_id {
        Some(id) => id,
        None => return,
    };

    // Resolve the sender's Mumble session and echo-exclusion in a single lock.
    // Virtual users aren't in UserMap, so we get their session from reverse_virtual_user_map.
    // For virtual users we also set exclude_session to prevent echoing voice back to the sender.
    let (session, exclude_session, target_channel) = {
        let state = read_bridge(&bridge_state);

        let vu_session = state.reverse_virtual_user_map.get(&sender_id).copied();
        let session = vu_session.or_else(|| state.users.get_mumble_session(sender_id));
        let exclude_session = vu_session;

        let target_channel = datagram
            .room_id
            .as_ref()
            .and_then(|bytes| uuid::Uuid::from_slice(bytes).ok())
            .and_then(|uuid| state.channels.get_mumble_id(&uuid));

        (session, exclude_session, target_channel)
    };

    let session = match session {
        Some(s) => s,
        None => return,
    };

    // Increment per-sender outbound sequence
    let seq = outbound_seq.entry(sender_id).or_insert(0);
    *seq += 1;
    let current_seq = *seq;

    let voice_data =
        mumble_voice::encode_voice_packet(session, current_seq, &datagram.opus_data, datagram.end_of_stream);

    if let Some(target_channel) = target_channel {
        // Only relay to Mumble clients in the matching channel, skipping the
        // original sender's Mumble session to prevent echo
        let state = read_bridge(&bridge_state);
        for (&client_session, sender) in client_senders {
            if exclude_session == Some(client_session) {
                continue;
            }
            let client_channel = state.mumble_clients.get(&client_session).map(|c| c.channel_id);
            if client_channel == Some(target_channel) {
                let _ = sender.tx.send(MumbleOutbound::Voice(voice_data.clone()));
            }
        }
    } else {
        // No room_id on the datagram — fall back to broadcasting to all clients,
        // skipping the original sender's Mumble session to prevent echo
        for (&client_session, sender) in client_senders {
            if exclude_session == Some(client_session) {
                continue;
            }
            let _ = sender.tx.send(MumbleOutbound::Voice(voice_data.clone()));
        }
    }
}

/// Send a protobuf message to a single Mumble client by session ID.
fn send_to_mumble_session(
    senders: &HashMap<u32, ClientSender>,
    session: u32,
    msg_type: MessageType,
    msg: &impl Message,
) {
    if let Some(sender) = senders.get(&session) {
        let payload = msg.encode_to_vec();
        let _ = sender.tx.send(MumbleOutbound::Protobuf {
            msg_type: msg_type as u16,
            payload,
        });
    }
}

/// Broadcast a protobuf message to all connected Mumble clients.
fn broadcast_to_all_mumble(senders: &HashMap<u32, ClientSender>, msg_type: MessageType, msg: &impl Message) {
    let payload = msg.encode_to_vec();
    for sender in senders.values() {
        let _ = sender.tx.send(MumbleOutbound::Protobuf {
            msg_type: msg_type as u16,
            payload: payload.clone(),
        });
    }
}

/// Broadcast a protobuf message to all Mumble clients except the specified session.
fn broadcast_to_mumble_except(
    senders: &HashMap<u32, ClientSender>,
    exclude_session: u32,
    msg_type: MessageType,
    msg: &impl Message,
) {
    let payload = msg.encode_to_vec();
    for (&session, sender) in senders {
        if session == exclude_session {
            continue;
        }
        let _ = sender.tx.send(MumbleOutbound::Protobuf {
            msg_type: msg_type as u16,
            payload: payload.clone(),
        });
    }
}
