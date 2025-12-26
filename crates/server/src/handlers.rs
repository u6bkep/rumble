//! Message handling and protocol logic.
//!
//! This module contains the core message handling functions for the server,
//! separated from network I/O for testability.
//!
//! # Locking Behavior
//!
//! Handlers are designed to minimize lock contention:
//! - State reads take snapshots before performing I/O
//! - The voice path uses snapshots to avoid holding locks during relay
//! - Client iteration is lock-free via DashMap

use crate::state::{ClientHandle, ServerState, UserStatus, compute_server_state_hash};
use anyhow::Result;
use api::{
    ROOT_ROOM_UUID, encode_frame,
    proto::{self, ServerState as ProtoServerState, VoiceDatagram, envelope::Payload},
    room_id_from_uuid, uuid_from_room_id,
};
use prost::Message;
use std::sync::Arc;
use tracing::{debug, error, info};

/// Handle a decoded envelope from a client.
///
/// This is the main protocol handler. It processes the envelope payload
/// and updates server state accordingly.
///
/// # Arguments
/// * `env` - The decoded envelope
/// * `sender` - Handle to the client that sent this message
/// * `state` - Shared server state
///
/// # Returns
/// Ok(()) on success, Err on fatal errors that should close the connection.
pub async fn handle_envelope(
    env: proto::Envelope,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    match env.payload {
        Some(Payload::ClientHello(ch)) => {
            handle_client_hello(ch, sender, state).await?;
        }
        Some(Payload::ChatMessage(msg)) => {
            handle_chat_message(msg, sender, state).await?;
        }
        Some(Payload::JoinRoom(jr)) => {
            handle_join_room(jr, sender, state).await?;
        }
        Some(Payload::Disconnect(d)) => {
            handle_disconnect(d, sender).await?;
        }
        Some(Payload::CreateRoom(cr)) => {
            handle_create_room(cr, state).await?;
        }
        Some(Payload::DeleteRoom(dr)) => {
            handle_delete_room(dr, state).await?;
        }
        Some(Payload::RenameRoom(rr)) => {
            handle_rename_room(rr, state).await?;
        }
        Some(Payload::RequestStateSync(rss)) => {
            handle_request_state_sync(rss, sender, state).await?;
        }
        Some(Payload::SetUserStatus(sus)) => {
            handle_set_user_status(sus, sender, state).await?;
        }
        // Server-to-client messages or empty - ignore
        Some(Payload::ServerHello(_) | Payload::ServerEvent(_)) | None => {}
    }
    Ok(())
}

/// Handle ClientHello - authenticate and send initial state.
async fn handle_client_hello(
    ch: proto::ClientHello,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    info!("ClientHello from {}", ch.client_name);

    // Check password if required
    if let Ok(required) = std::env::var("RUMBLE_SERVER_PASSWORD") {
        if !required.is_empty() && ch.password != required {
            error!("authentication failed for {}", ch.client_name);
            let mut send = sender.send.lock().await;
            send.reset(quinn::VarInt::from_u32(0)).ok();
            return Ok(());
        }
    }

    // Set username (uses RwLock, only happens once)
    sender.set_username(ch.client_name.clone()).await;

    // Auto-join Root room
    state.set_user_room(sender.user_id, ROOT_ROOM_UUID).await;

    // Send ServerHello with assigned user_id
    let reply = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ServerHello(proto::ServerHello {
            server_name: "Rumble Server".to_string(),
            user_id: sender.user_id,
        })),
    };
    let frame = encode_frame(&reply);
    debug!(
        bytes = frame.len(),
        user_id = sender.user_id,
        "server: sending ServerHello frame with user_id"
    );

    // Send using the new helper method
    if let Err(e) = sender.send_frame(&frame).await {
        error!("failed to send ServerHello: {e:?}");
        return Err(e.into());
    }

    // Send initial ServerState
    send_server_state_to_client(&sender, &state).await?;

    // Broadcast that this user joined
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserJoined(proto::UserJoined {
            user: Some(proto::User {
                user_id: Some(proto::UserId { value: sender.user_id }),
                username: ch.client_name,
                current_room: Some(room_id_from_uuid(ROOT_ROOM_UUID)),
                is_muted: false,
                is_deafened: false,
            }),
        }),
    )
    .await?;

    Ok(())
}

/// Handle chat message - broadcast to room members.
async fn handle_chat_message(
    msg: proto::ChatMessage,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    info!("chat from {}: {}", msg.sender, msg.text);

    let sender_room = state
        .get_user_room(sender.user_id)
        .await
        .unwrap_or(ROOT_ROOM_UUID);

    let broadcast = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ServerEvent(proto::ServerEvent {
            kind: Some(proto::server_event::Kind::ChatBroadcast(
                proto::ChatBroadcast {
                    sender: msg.sender,
                    text: msg.text,
                },
            )),
        })),
    };
    let frame = encode_frame(&broadcast);

    // Snapshot clients first, then iterate without holding any state locks
    let clients = state.snapshot_clients();
    for h in clients {
        let user_room = state.get_user_room(h.user_id).await;
        if user_room != Some(sender_room) {
            continue;
        }
        if let Err(e) = h.send_frame(&frame).await {
            error!("broadcast write failed: {e:?}");
        }
    }

    Ok(())
}

/// Handle join room request.
async fn handle_join_room(
    jr: proto::JoinRoom,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    let new_room_uuid = jr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);
    let _old_room_uuid = state
        .get_user_room(sender.user_id)
        .await
        .unwrap_or(ROOT_ROOM_UUID);

    state.set_user_room(sender.user_id, new_room_uuid).await;

    // Send incremental update about user moving rooms (from_room is implicit)
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserMoved(proto::UserMoved {
            user_id: Some(proto::UserId {
                value: sender.user_id,
            }),
            to_room_id: Some(room_id_from_uuid(new_room_uuid)),
        }),
    )
    .await?;

    Ok(())
}

/// Handle disconnect request.
async fn handle_disconnect(d: proto::Disconnect, sender: Arc<ClientHandle>) -> Result<()> {
    info!("peer requested disconnect: {}", d.reason);
    let mut send = sender.send.lock().await;
    let _ = send.finish();
    Ok(())
}

/// Handle create room request.
async fn handle_create_room(cr: proto::CreateRoom, state: Arc<ServerState>) -> Result<()> {
    info!("CreateRoom: {}", cr.name);
    let room_uuid = state.create_room(cr.name.clone()).await;

    // Send incremental update to all clients
    let room_info = proto::RoomInfo {
        id: Some(room_id_from_uuid(room_uuid)),
        name: cr.name,
    };
    broadcast_state_update(
        &state,
        proto::state_update::Update::RoomCreated(proto::RoomCreated {
            room: Some(room_info),
        }),
    )
    .await?;
    Ok(())
}

/// Handle delete room request.
async fn handle_delete_room(dr: proto::DeleteRoom, state: Arc<ServerState>) -> Result<()> {
    let room_uuid = dr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);
    info!("DeleteRoom: {}", room_uuid);
    state.delete_room(room_uuid).await;

    // Send incremental update to all clients
    broadcast_state_update(
        &state,
        proto::state_update::Update::RoomDeleted(proto::RoomDeleted {
            room_id: Some(room_id_from_uuid(room_uuid)),
            fallback_room_id: Some(room_id_from_uuid(ROOT_ROOM_UUID)),
        }),
    )
    .await?;
    Ok(())
}

/// Handle rename room request.
async fn handle_rename_room(rr: proto::RenameRoom, state: Arc<ServerState>) -> Result<()> {
    let room_uuid = rr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);
    info!("RenameRoom: {} -> {}", room_uuid, rr.new_name);

    state.rename_room(room_uuid, rr.new_name.clone()).await;

    // Send incremental update to all clients
    broadcast_state_update(
        &state,
        proto::state_update::Update::RoomRenamed(proto::RoomRenamed {
            room_id: Some(room_id_from_uuid(room_uuid)),
            new_name: rr.new_name,
        }),
    )
    .await?;
    Ok(())
}

/// Handle a request for full state resync.
///
/// This is sent by clients when they detect a state hash mismatch.
/// We simply send them the current full state.
async fn handle_request_state_sync(
    rss: proto::RequestStateSync,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    info!(
        user_id = sender.user_id,
        expected_hash_len = rss.expected_hash.len(),
        actual_hash_len = rss.actual_hash.len(),
        "RequestStateSync: client detected hash mismatch, sending full state"
    );

    // Log the hashes for debugging (first 8 bytes as hex)
    if !rss.expected_hash.is_empty() {
        debug!(
            "  expected: {:02x?}...",
            &rss.expected_hash[..rss.expected_hash.len().min(8)]
        );
    }
    if !rss.actual_hash.is_empty() {
        debug!(
            "  actual: {:02x?}...",
            &rss.actual_hash[..rss.actual_hash.len().min(8)]
        );
    }

    // Send the current state to this client
    send_server_state_to_client(&sender, &state).await?;
    Ok(())
}

/// Handle user status update (mute/deafen).
async fn handle_set_user_status(
    sus: proto::SetUserStatus,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    info!(
        user_id = sender.user_id,
        is_muted = sus.is_muted,
        is_deafened = sus.is_deafened,
        "SetUserStatus"
    );

    // Update the user's status in state
    let status = UserStatus {
        is_muted: sus.is_muted,
        is_deafened: sus.is_deafened,
    };
    state.set_user_status(sender.user_id, status).await;

    // Broadcast the status change to all clients
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserStatusChanged(proto::UserStatusChanged {
            user_id: Some(proto::UserId { value: sender.user_id }),
            is_muted: sus.is_muted,
            is_deafened: sus.is_deafened,
        }),
    )
    .await?;
    Ok(())
}

/// Send current server state to a single client.
async fn send_server_state_to_client(client: &ClientHandle, state: &ServerState) -> Result<()> {
    let rooms = state.get_rooms().await;
    let users = state.build_user_list().await;

    // Build the ServerState message
    let server_state = ProtoServerState {
        rooms: rooms.clone(),
        users: users.clone(),
    };

    // Compute the state hash
    let state_hash = compute_server_state_hash(&server_state);

    let env = proto::Envelope {
        state_hash,
        payload: Some(Payload::ServerEvent(proto::ServerEvent {
            kind: Some(proto::server_event::Kind::ServerState(server_state)),
        })),
    };
    let frame = encode_frame(&env);

    info!(
        rooms = rooms.len(),
        users = users.len(),
        "server: sending initial ServerState with state_hash"
    );
    client.send_frame(&frame).await?;

    Ok(())
}

/// Broadcast current server state to all connected clients.
pub async fn broadcast_server_state(state: &Arc<ServerState>) -> Result<()> {
    let rooms = state.get_rooms().await;
    let users = state.build_user_list().await;

    // Build the ServerState message
    let server_state = ProtoServerState { rooms, users };

    // Compute the state hash
    let state_hash = compute_server_state_hash(&server_state);

    let env = proto::Envelope {
        state_hash,
        payload: Some(Payload::ServerEvent(proto::ServerEvent {
            kind: Some(proto::server_event::Kind::ServerState(server_state)),
        })),
    };
    let frame = encode_frame(&env);

    // Snapshot clients, then send without holding state locks
    let clients = state.snapshot_clients();
    for h in clients {
        let _ = h.send_frame(&frame).await;
    }

    Ok(())
}

/// Broadcast an incremental state update to all connected clients.
///
/// This sends a StateUpdate message containing:
/// - The specific change that occurred
/// - The expected hash AFTER applying this change
///
/// Clients apply the update locally and verify their computed hash matches.
/// If there's a mismatch, the client will request a full resync.
pub async fn broadcast_state_update(
    state: &Arc<ServerState>,
    update: proto::state_update::Update,
) -> Result<()> {
    // First, compute what the state hash should be after this update
    let rooms = state.get_rooms().await;
    let users = state.build_user_list().await;
    let server_state = ProtoServerState { rooms, users };
    let expected_hash = compute_server_state_hash(&server_state);

    let state_update = proto::StateUpdate {
        expected_hash: expected_hash.clone(),
        update: Some(update),
    };

    let env = proto::Envelope {
        state_hash: expected_hash,
        payload: Some(Payload::ServerEvent(proto::ServerEvent {
            kind: Some(proto::server_event::Kind::StateUpdate(state_update)),
        })),
    };
    let frame = encode_frame(&env);

    debug!("server: broadcasting incremental StateUpdate");

    // Snapshot clients, then send without holding state locks
    let clients = state.snapshot_clients();
    for h in clients {
        let _ = h.send_frame(&frame).await;
    }

    Ok(())
}

/// Clean up a client: remove from clients list, remove memberships, broadcast update.
pub async fn cleanup_client(client_handle: &Arc<ClientHandle>, state: &Arc<ServerState>) {
    let user_id = client_handle.user_id;

    // Remove client from DashMap (lock-free)
    state.remove_client_by_handle(client_handle);
    // Remove membership
    state.remove_user_membership(user_id).await;

    debug!(user_id, "server: cleaned up client");

    // Send incremental update about user leaving
    if let Err(e) = broadcast_state_update(
        state,
        proto::state_update::Update::UserLeft(proto::UserLeft {
            user_id: Some(proto::UserId { value: user_id }),
        }),
    )
    .await
    {
        error!("failed to broadcast state update after disconnect: {e:?}");
    }
}

/// Handle incoming QUIC datagrams for voice relay.
///
/// Datagrams are relayed to all other clients in the same room.
/// The sender_user_id is determined by the connection, not by the datagram content.
///
/// # Locking Behavior
///
/// This handler is optimized for the audio path:
/// - Uses snapshot of room memberships to avoid holding locks during relay
/// - Client iteration is lock-free via DashMap
/// - Datagram sends don't hold any locks
pub async fn handle_datagrams(
    conn: quinn::Connection,
    state: Arc<ServerState>,
    sender_user_id: u64,
) {
    loop {
        match conn.read_datagram().await {
            Ok(datagram) => {
                // Decode the VoiceDatagram protobuf
                match VoiceDatagram::decode(datagram.as_ref()) {
                    Ok(mut voice_dgram) => {
                        debug!(
                            sender = sender_user_id,
                            seq = voice_dgram.sequence,
                            data_len = voice_dgram.opus_data.len(),
                            "server: received voice datagram"
                        );

                        // Take a snapshot of room memberships to avoid holding locks during relay
                        let room_memberships = state.snapshot_room_memberships().await;

                        // Find sender's room from the snapshot
                        let sender_room = room_memberships.iter().find_map(|(rid, users)| {
                            if users.contains(&sender_user_id) {
                                Some(*rid)
                            } else {
                                None
                            }
                        });

                        // Only relay if sender is actually in a room
                        let Some(actual_room) = sender_room else {
                            debug!(
                                sender = sender_user_id,
                                "server: sender not in any room, dropping datagram"
                            );
                            continue;
                        };

                        // Set sender_id and room_id (server-authoritative, client values ignored)
                        voice_dgram.sender_id = Some(sender_user_id);
                        voice_dgram.room_id = Some(actual_room.as_bytes().to_vec());

                        // Re-encode with corrected sender_id and room_id
                        let relay_bytes = voice_dgram.encode_to_vec();

                        // Get recipients from the snapshot (no lock needed)
                        let recipients = room_memberships
                            .get(&actual_room)
                            .map(|v| v.as_slice())
                            .unwrap_or(&[]);

                        // Relay to each recipient (lock-free client lookup via DashMap)
                        for &recipient_id in recipients {
                            // Skip the sender
                            if recipient_id == sender_user_id {
                                continue;
                            }

                            // Get client handle (lock-free)
                            if let Some(client) = state.get_client(recipient_id) {
                                // Send datagram (no lock needed, datagrams are connectionless)
                                if let Err(e) =
                                    client.conn.send_datagram(relay_bytes.clone().into())
                                {
                                    debug!(
                                        user_id = recipient_id,
                                        error = ?e,
                                        "server: failed to relay voice datagram"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        debug!(error = ?e, "server: failed to decode voice datagram");
                    }
                }
            }
            Err(e) => {
                // Connection closed or error
                debug!(error = ?e, "server: datagram receive ended");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Handler tests will require mock clients, which we'll add in a future step.
    // For now, the state module tests cover the core logic.
}
