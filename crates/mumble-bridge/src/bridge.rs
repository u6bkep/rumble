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
    mumble_voice,
    state::BridgeState,
};

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
    MumbleChat { session: u32, message: String },
    /// Mumble client changed channel.
    MumbleChannelChange { session: u32, channel_id: u32 },
    /// Received a Rumble envelope from the server.
    RumbleEnvelope(proto::Envelope),
    /// Received a Rumble voice datagram.
    RumbleVoice(proto::VoiceDatagram),
}

/// Per-client sender handle.
struct ClientSender {
    tx: mpsc::UnboundedSender<MumbleOutbound>,
}

/// Run the core bridge event loop.
///
/// Consumes events from both Mumble clients and the Rumble connection,
/// translating and forwarding messages between the two protocols.
pub async fn run_bridge(
    config: Arc<BridgeConfig>,
    bridge_state: Arc<RwLock<BridgeState>>,
    mut bridge_rx: mpsc::UnboundedReceiver<BridgeEvent>,
    rumble_conn: quinn::Connection,
    rumble_send: &mut quinn::SendStream,
) -> Result<()> {
    let mut client_senders: HashMap<u32, ClientSender> = HashMap::new();

    info!("Bridge event loop started");

    while let Some(event) = bridge_rx.recv().await {
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
                broadcast_to_mumble_except(&client_senders, session, MessageType::UserState, &user_state);
            }

            BridgeEvent::MumbleClientLeft { session } => {
                client_senders.remove(&session);

                let username = {
                    let state = bridge_state.read().unwrap();
                    state.mumble_clients.get(&session).map(|c| c.username.clone())
                };
                info!(session, username = ?username, "Mumble client left bridge");

                // Notify remaining Mumble clients
                let remove = mumble::UserRemove {
                    session,
                    actor: None,
                    reason: Some("Disconnected".to_string()),
                    ban: None,
                };
                broadcast_to_all_mumble(&client_senders, MessageType::UserRemove, &remove);
            }

            BridgeEvent::MumbleClientSender { session, sender } => {
                client_senders.insert(session, ClientSender { tx: sender });
            }

            BridgeEvent::MumblePing { session, payload } => {
                if let Some(client) = client_senders.get(&session) {
                    let _ = client.tx.send(MumbleOutbound::Protobuf {
                        msg_type: MessageType::Ping as u16,
                        payload,
                    });
                }
            }

            BridgeEvent::MumbleVoice { session: _, data } => {
                // Parse the Mumble voice packet and forward to Rumble as datagram
                if let Some(voice) = mumble_voice::parse_voice_packet(&data) {
                    let datagram = proto::VoiceDatagram {
                        opus_data: voice.opus_data,
                        sequence: 0,
                        timestamp_us: 0,
                        end_of_stream: voice.is_last,
                        sender_id: None,
                        room_id: None,
                    };
                    let encoded = datagram.encode_to_vec();
                    if let Err(e) = rumble_conn.send_datagram(encoded.into()) {
                        warn!(error = %e, "Failed to send voice datagram to Rumble");
                    }
                }
            }

            BridgeEvent::MumbleChat { session, message } => {
                let username = {
                    let state = bridge_state.read().unwrap();
                    state
                        .mumble_clients
                        .get(&session)
                        .map(|c| c.username.clone())
                        .unwrap_or_else(|| format!("session-{}", session))
                };

                // Forward to Rumble with prefix
                let prefixed = format!("[{}] {}", username, message);
                if let Err(e) = crate::rumble_client::send_chat(rumble_send, &config.bridge_name, &prefixed).await {
                    warn!(error = %e, "Failed to send chat to Rumble");
                }

                // Also broadcast to other Mumble clients
                let text_msg = mumble::TextMessage {
                    actor: Some(session),
                    channel_id: vec![0],
                    message: Some(message),
                    ..Default::default()
                };
                broadcast_to_mumble_except(&client_senders, session, MessageType::TextMessage, &text_msg);
            }

            BridgeEvent::MumbleChannelChange { session, channel_id } => {
                {
                    let mut state = bridge_state.write().unwrap();
                    if let Some(client) = state.mumble_clients.get_mut(&session) {
                        client.channel_id = channel_id;
                    }
                }

                let user_state = mumble::UserState {
                    session: Some(session),
                    channel_id: Some(channel_id),
                    ..Default::default()
                };
                broadcast_to_all_mumble(&client_senders, MessageType::UserState, &user_state);
            }

            BridgeEvent::RumbleEnvelope(env) => {
                handle_rumble_envelope(env, &bridge_state, &client_senders);
            }

            BridgeEvent::RumbleVoice(datagram) => {
                handle_rumble_voice(datagram, &bridge_state, &client_senders);
            }
        }
    }

    info!("Bridge event loop ended");
    Ok(())
}

/// Handle a Rumble server envelope and forward relevant events to Mumble clients.
fn handle_rumble_envelope(
    env: proto::Envelope,
    bridge_state: &Arc<RwLock<BridgeState>>,
    client_senders: &HashMap<u32, ClientSender>,
) {
    match env.payload {
        Some(Payload::ServerEvent(se)) => {
            if let Some(kind) = se.kind {
                match kind {
                    proto::server_event::Kind::ServerState(ss) => {
                        let mut state = bridge_state.write().unwrap();
                        state.rumble_rooms = ss.rooms.clone();
                        state.rumble_users = ss.users.clone();

                        for user in &ss.users {
                            let rumble_id = user.user_id.as_ref().map(|id| id.value).unwrap_or(0);
                            if Some(rumble_id) != state.bridge_user_id {
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
                        handle_state_update(su, bridge_state, client_senders);
                    }

                    proto::server_event::Kind::ChatBroadcast(cb) => {
                        let text = format!("{}: {}", cb.sender, cb.text);
                        let text_msg = mumble::TextMessage {
                            actor: None,
                            channel_id: vec![0],
                            message: Some(text),
                            ..Default::default()
                        };
                        broadcast_to_all_mumble(client_senders, MessageType::TextMessage, &text_msg);
                    }

                    proto::server_event::Kind::KeepAlive(_) => {}
                }
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
) {
    match su.update {
        Some(proto::state_update::Update::UserJoined(uj)) => {
            if let Some(user) = uj.user {
                let rumble_id = user.user_id.as_ref().map(|id| id.value).unwrap_or(0);

                let mut state = bridge_state.write().unwrap();
                if Some(rumble_id) == state.bridge_user_id {
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

            let mut state = bridge_state.write().unwrap();
            let session = state.users.get_mumble_session(rumble_id);
            state.users.remove_by_rumble_id(rumble_id);
            state
                .rumble_users
                .retain(|u| u.user_id.as_ref().map(|id| id.value).unwrap_or(0) != rumble_id);

            if let Some(session) = session {
                let remove = mumble::UserRemove {
                    session,
                    actor: None,
                    reason: Some("Left".to_string()),
                    ban: None,
                };
                drop(state);
                broadcast_to_all_mumble(client_senders, MessageType::UserRemove, &remove);
                info!(rumble_id, session, "Rumble user left -> Mumble UserRemove");
            }
        }

        Some(proto::state_update::Update::UserMoved(um)) => {
            let rumble_id = um.user_id.as_ref().map(|id| id.value).unwrap_or(0);
            let state = bridge_state.read().unwrap();
            let session = state.users.get_mumble_session(rumble_id);
            let channel_id = um
                .to_room_id
                .as_ref()
                .and_then(api::uuid_from_room_id)
                .and_then(|uuid| state.channels.get_mumble_id(&uuid));

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
            let state = bridge_state.read().unwrap();
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
                let mut state = bridge_state.write().unwrap();
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
                let mut state = bridge_state.write().unwrap();
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
                let state = bridge_state.read().unwrap();
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
                let state = bridge_state.read().unwrap();
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

        None => {}
    }
}

/// Handle a Rumble voice datagram and forward to all Mumble clients.
fn handle_rumble_voice(
    datagram: proto::VoiceDatagram,
    bridge_state: &Arc<RwLock<BridgeState>>,
    client_senders: &HashMap<u32, ClientSender>,
) {
    if datagram.opus_data.is_empty() && !datagram.end_of_stream {
        return;
    }

    let sender_id = match datagram.sender_id {
        Some(id) => id,
        None => return,
    };

    let session = {
        let state = bridge_state.read().unwrap();
        state.users.get_mumble_session(sender_id)
    };

    let session = match session {
        Some(s) => s,
        None => return,
    };

    let voice_data = mumble_voice::encode_voice_packet(session, &datagram.opus_data, datagram.end_of_stream);

    for sender in client_senders.values() {
        let _ = sender.tx.send(MumbleOutbound::Voice(voice_data.clone()));
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
