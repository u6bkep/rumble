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

use crate::persistence::Persistence;
use crate::state::{ClientHandle, PendingAuth, ServerState, UserStatus, compute_server_state_hash};
use anyhow::Result;
use api::{
    ROOT_ROOM_UUID, build_auth_payload, encode_frame,
    proto::{self, ServerState as ProtoServerState, VoiceDatagram, envelope::Payload},
    room_id_from_uuid, uuid_from_room_id,
};
use ed25519_dalek::{Signature, VerifyingKey};
use prost::Message;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

/// Handle a decoded envelope from a client.
///
/// This is the main protocol handler. It processes the envelope payload
/// and updates server state accordingly.
///
/// # Arguments
/// * `env` - The decoded envelope
/// * `sender` - Handle to the client that sent this message
/// * `state` - Shared server state
/// * `persistence` - Optional persistence layer for registered users
///
/// # Returns
/// Ok(()) on success, Err on fatal errors that should close the connection.
pub async fn handle_envelope(
    env: proto::Envelope,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    match env.payload {
        Some(Payload::ClientHello(ch)) => {
            handle_client_hello(ch, sender, state, persistence).await?;
        }
        Some(Payload::Authenticate(auth)) => {
            handle_authenticate(auth, sender, state, persistence).await?;
        }
        Some(Payload::ChatMessage(msg)) => {
            // Require authentication for chat
            if !sender.authenticated.load(Ordering::SeqCst) {
                warn!(user_id = sender.user_id, "unauthenticated client tried to send chat");
                return Ok(());
            }
            handle_chat_message(msg, sender, state).await?;
        }
        Some(Payload::JoinRoom(jr)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                warn!(user_id = sender.user_id, "unauthenticated client tried to join room");
                return Ok(());
            }
            handle_join_room(jr, sender, state, persistence).await?;
        }
        Some(Payload::Disconnect(d)) => {
            handle_disconnect(d, sender).await?;
        }
        Some(Payload::CreateRoom(cr)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_create_room(cr, sender, state, persistence).await?;
        }
        Some(Payload::DeleteRoom(dr)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_delete_room(dr, sender, state, persistence).await?;
        }
        Some(Payload::RenameRoom(rr)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_rename_room(rr, sender, state).await?;
        }
        Some(Payload::RequestStateSync(rss)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_request_state_sync(rss, sender, state).await?;
        }
        Some(Payload::SetUserStatus(sus)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_set_user_status(sus, sender, state).await?;
        }
        Some(Payload::RegisterUser(ru)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_register_user(ru, sender, state, persistence).await?;
        }
        Some(Payload::UnregisterUser(uu)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_unregister_user(uu, sender, state, persistence).await?;
        }
        // Server-to-client messages or empty - ignore
        Some(Payload::ServerHello(_) | Payload::ServerEvent(_) | Payload::AuthFailed(_) | Payload::CommandResult(_)) | None => {}
    }
    Ok(())
}

/// Handle ClientHello - begin authentication handshake.
async fn handle_client_hello(
    ch: proto::ClientHello,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    info!("ClientHello from {}", ch.username);

    // 1. Validate public key length
    if ch.public_key.len() != 32 {
        return send_auth_failed(&sender, "Invalid public key length").await;
    }
    let public_key: [u8; 32] = ch.public_key.try_into().unwrap();
    
    // 2. Check registration constraints (if persistence is enabled)
    if let Some(ref persist) = persistence {
        // Check if username is taken by a DIFFERENT key
        // Note: If this key is registered, they can provide any name - we'll override it later
        // with their registered name. We only block if they're trying to use someone ELSE's
        // registered name.
        if persist.is_username_taken(&ch.username, &public_key) {
            return send_auth_failed(&sender, "Username is registered to a different key").await;
        }
    }
    
    // 3. Check password for unknown keys
    if let Some(ref persist) = persistence {
        let is_known = persist.is_known_key(&public_key);
        if !is_known {
            if let Ok(required) = std::env::var("RUMBLE_SERVER_PASSWORD") {
                if !required.is_empty() {
                    match &ch.password {
                        Some(pw) if pw == &required => { /* OK */ }
                        _ => return send_auth_failed(&sender, "Password required for new users").await,
                    }
                }
            }
        }
    } else {
        // No persistence - check password for everyone if set
        if let Ok(required) = std::env::var("RUMBLE_SERVER_PASSWORD") {
            if !required.is_empty() {
                match &ch.password {
                    Some(pw) if pw == &required => { /* OK */ }
                    _ => return send_auth_failed(&sender, "Password required").await,
                }
            }
        }
    }
    
    // 4. Generate nonce and store pending auth
    let nonce: [u8; 32] = rand::random();
    state.set_pending_auth(PendingAuth {
        nonce,
        user_id: sender.user_id,
        public_key,
        timestamp: Instant::now(),
        username: ch.username.clone(),
    });
    
    // 5. Store public key on client handle
    *sender.public_key.write().await = Some(public_key);

    // 6. Send ServerHello with nonce
    let reply = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ServerHello(proto::ServerHello {
            nonce: nonce.to_vec(),
            server_name: "Rumble Server".to_string(),
            user_id: sender.user_id,
        })),
    };
    let frame = encode_frame(&reply);
    debug!(
        bytes = frame.len(),
        user_id = sender.user_id,
        "server: sending ServerHello frame with nonce"
    );

    if let Err(e) = sender.send_frame(&frame).await {
        error!("failed to send ServerHello: {e:?}");
        return Err(e.into());
    }

    Ok(())
}

/// Handle Authenticate - verify signature and complete handshake.
async fn handle_authenticate(
    auth: proto::Authenticate,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    // 1. Get pending auth state
    let pending = match state.take_pending_auth(sender.user_id) {
        Some(p) => p,
        None => return send_auth_failed(&sender, "No pending authentication").await,
    };
    
    // 2. Check timestamp (±5 minutes)
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let diff_ms = (now_ms - auth.timestamp_ms).abs();
    if diff_ms > 5 * 60 * 1000 {
        return send_auth_failed(&sender, "Timestamp out of range").await;
    }
    
    // 3. Compute expected signature payload
    let cert_hash = state.server_cert_hash();
    let payload = build_auth_payload(
        &pending.nonce,
        auth.timestamp_ms,
        &pending.public_key,
        pending.user_id,
        &cert_hash,
    );
    
    // 4. Verify signature
    let signature: [u8; 64] = match auth.signature.try_into() {
        Ok(sig) => sig,
        Err(_) => return send_auth_failed(&sender, "Invalid signature length").await,
    };
    
    let verifying_key = match VerifyingKey::from_bytes(&pending.public_key) {
        Ok(key) => key,
        Err(_) => return send_auth_failed(&sender, "Invalid public key").await,
    };
    let sig = Signature::from_bytes(&signature);
    
    if verifying_key.verify_strict(&payload, &sig).is_err() {
        return send_auth_failed(&sender, "Invalid signature").await;
    }
    
    // 5. Authentication successful
    sender.authenticated.store(true, Ordering::SeqCst);
    
    // 6. Mark key as known (if persistence enabled)
    if let Some(ref persist) = persistence {
        if let Err(e) = persist.add_known_key(&pending.public_key) {
            warn!("Failed to mark key as known: {e}");
        }
    }
    
    // 7. Check for registered username (overrides client-provided)
    let final_username = if let Some(ref persist) = persistence {
        if let Some(registered) = persist.get_registered_user(&pending.public_key) {
            registered.username
        } else {
            pending.username.clone()
        }
    } else {
        pending.username.clone()
    };
    
    // 8. Set username
    sender.set_username(final_username.clone()).await;
    info!(user_id = sender.user_id, username = %final_username, "Authentication successful");
    
    // 9. Track session
    state.add_session(sender.user_id, pending.public_key);
    
    // 10. Determine initial room (restore last channel if registered, otherwise Root)
    let initial_room = if let Some(ref persist) = persistence {
        if let Some(registered) = persist.get_registered_user(&pending.public_key) {
            if let Some(last_channel_bytes) = registered.last_channel {
                let last_uuid = uuid::Uuid::from_bytes(last_channel_bytes);
                // Check if the room still exists
                let rooms = state.get_rooms().await;
                if rooms.iter().any(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(last_uuid)) {
                    info!(user_id = sender.user_id, room = %last_uuid, "Restoring user to last channel");
                    last_uuid
                } else {
                    ROOT_ROOM_UUID
                }
            } else {
                ROOT_ROOM_UUID
            }
        } else {
            ROOT_ROOM_UUID
        }
    } else {
        ROOT_ROOM_UUID
    };
    state.set_user_room(sender.user_id, initial_room).await;
    
    // 11. Send initial ServerState
    send_server_state_to_client(&sender, &state).await?;
    
    // 12. Broadcast that this user joined
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserJoined(proto::UserJoined {
            user: Some(proto::User {
                user_id: Some(proto::UserId { value: sender.user_id }),
                username: final_username,
                current_room: Some(room_id_from_uuid(initial_room)),
                is_muted: false,
                is_deafened: false,
            }),
        }),
    )
    .await?;

    Ok(())
}

/// Send AuthFailed message to client.
async fn send_auth_failed(sender: &ClientHandle, error: &str) -> Result<()> {
    warn!(user_id = sender.user_id, error, "Authentication failed");
    let reply = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::AuthFailed(proto::AuthFailed {
            error: error.to_string(),
        })),
    };
    let frame = encode_frame(&reply);
    let _ = sender.send_frame(&frame).await;
    // Finish the stream and wait for the peer to receive the data
    {
        let mut send = sender.send.lock().await;
        let _ = send.finish();
        // stopped() waits until the peer has consumed all data
        let _ = send.stopped().await;
    }
    // Close the connection
    sender.conn.close(quinn::VarInt::from_u32(1), b"auth failed");
    Ok(())
}

/// Handle RegisterUser request.
async fn handle_register_user(
    req: proto::RegisterUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    let Some(persist) = persistence else {
        return send_command_result(&sender, "RegisterUser", false, "Registration not enabled").await;
    };
    
    // Extract user_id from the UserId message
    let target_user_id = req.user_id.map(|u| u.value).unwrap_or(0);
    
    // Get the target user's public key from active sessions
    let target_key = match state.get_session_key(target_user_id) {
        Some(key) => key,
        None => {
            return send_command_result(&sender, "RegisterUser", false, "User not found").await;
        }
    };
    
    // Get the target user's current username
    let target_client = match state.get_client(target_user_id) {
        Some(c) => c,
        None => return send_command_result(&sender, "RegisterUser", false, "User not found").await,
    };
    let username = target_client.get_username().await;
    
    // Check if already registered
    if persist.get_registered_user(&target_key).is_some() {
        return send_command_result(&sender, "RegisterUser", false, "User already registered").await;
    }
    
    // Register
    if let Err(e) = persist.register_user(&target_key, crate::persistence::RegisteredUser {
        username: username.clone(),
        roles: vec![],
        last_channel: None,
    }) {
        return send_command_result(&sender, "RegisterUser", false, &format!("Registration failed: {e}")).await;
    }
    
    send_command_result(&sender, "RegisterUser", true, &format!("Registered '{}' successfully", username)).await
}

/// Handle UnregisterUser request.
async fn handle_unregister_user(
    req: proto::UnregisterUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    let Some(persist) = persistence else {
        return send_command_result(&sender, "UnregisterUser", false, "Registration not enabled").await;
    };
    
    // Extract user_id from the UserId message
    let target_user_id = req.user_id.map(|u| u.value).unwrap_or(0);
    
    // Get the target user's public key
    let target_key = match state.get_session_key(target_user_id) {
        Some(key) => key,
        None => {
            return send_command_result(&sender, "UnregisterUser", false, "User not found").await;
        }
    };
    
    // Get the username before unregistering
    let username = match state.get_client(target_user_id) {
        Some(c) => c.get_username().await,
        None => "user".to_string(),
    };
    
    // Unregister
    if let Err(e) = persist.unregister_user(&target_key) {
        return send_command_result(&sender, "UnregisterUser", false, &format!("Unregistration failed: {e}")).await;
    }
    
    send_command_result(&sender, "UnregisterUser", true, &format!("Unregistered '{}' successfully", username)).await
}

/// Send CommandResult message to client.
async fn send_command_result(sender: &ClientHandle, command: &str, success: bool, message: &str) -> Result<()> {
    let reply = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::CommandResult(proto::CommandResult {
            command: command.to_string(),
            success,
            message: message.to_string(),
        })),
    };
    let frame = encode_frame(&reply);
    let _ = sender.send_frame(&frame).await;
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
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    let new_room_uuid = jr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);
    
    // Check if room exists
    let rooms = state.get_rooms().await;
    let room = rooms.iter().find(|r| {
        r.id.as_ref().and_then(uuid_from_room_id) == Some(new_room_uuid)
    });
    
    let room_name = match room {
        Some(r) => r.name.clone(),
        None => {
            return send_command_result(&sender, "JoinRoom", false, "Room not found").await;
        }
    };
    
    let _old_room_uuid = state
        .get_user_room(sender.user_id)
        .await
        .unwrap_or(ROOT_ROOM_UUID);

    state.set_user_room(sender.user_id, new_room_uuid).await;

    // Save last channel for registered users
    if let Some(ref persist) = persistence {
        if let Some(public_key) = state.get_session_key(sender.user_id) {
            if persist.is_registered(&public_key) {
                if let Err(e) = persist.update_user_last_channel(&public_key, Some(new_room_uuid.into_bytes())) {
                    warn!("Failed to save user's last channel: {e}");
                }
            }
        }
    }

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
    
    // Send confirmation to the requesting client
    send_command_result(&sender, "JoinRoom", true, &format!("Joined '{}'", room_name)).await?;

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
async fn handle_create_room(
    cr: proto::CreateRoom,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    info!("CreateRoom: {}", cr.name);
    let room_name = cr.name.clone();
    let room_uuid = state.create_room(cr.name.clone()).await;

    // Persist the new channel
    if let Some(ref persist) = persistence {
        let channel = crate::persistence::PersistedChannel {
            name: room_name.clone(),
            parent: None,
            description: String::new(),
            permanent: true,
        };
        if let Err(e) = persist.save_channel(&room_uuid.into_bytes(), &channel) {
            warn!("Failed to persist channel: {e}");
        }
    }

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
    
    // Send confirmation to the requesting client
    send_command_result(&sender, "CreateRoom", true, &format!("Created room '{}'", room_name)).await?;
    Ok(())
}

/// Handle delete room request.
async fn handle_delete_room(
    dr: proto::DeleteRoom,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    let room_uuid = dr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);
    
    // Cannot delete the root room
    if room_uuid == ROOT_ROOM_UUID {
        return send_command_result(&sender, "DeleteRoom", false, "Cannot delete the Root room").await;
    }
    
    // Get room name before deleting
    let room_name = state.get_rooms().await
        .iter()
        .find(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(room_uuid))
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "room".to_string());
    
    info!("DeleteRoom: {}", room_uuid);
    let deleted = state.delete_room(room_uuid).await;
    
    if !deleted {
        return send_command_result(&sender, "DeleteRoom", false, "Room not found").await;
    }

    // Remove from persistence
    if let Some(ref persist) = persistence {
        if let Err(e) = persist.delete_channel(&room_uuid.into_bytes()) {
            warn!("Failed to remove channel from persistence: {e}");
        }
    }

    // Send incremental update to all clients
    broadcast_state_update(
        &state,
        proto::state_update::Update::RoomDeleted(proto::RoomDeleted {
            room_id: Some(room_id_from_uuid(room_uuid)),
            fallback_room_id: Some(room_id_from_uuid(ROOT_ROOM_UUID)),
        }),
    )
    .await?;
    
    // Send confirmation to the requesting client
    send_command_result(&sender, "DeleteRoom", true, &format!("Deleted room '{}'", room_name)).await?;
    Ok(())
}

/// Handle rename room request.
async fn handle_rename_room(rr: proto::RenameRoom, sender: Arc<ClientHandle>, state: Arc<ServerState>) -> Result<()> {
    let room_uuid = rr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);
    
    // Get old room name for the message
    let old_name = state.get_rooms().await
        .iter()
        .find(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(room_uuid))
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "room".to_string());
    
    info!("RenameRoom: {} -> {}", room_uuid, rr.new_name);
    let new_name = rr.new_name.clone();

    let renamed = state.rename_room(room_uuid, rr.new_name.clone()).await;
    
    if !renamed {
        return send_command_result(&sender, "RenameRoom", false, "Room not found").await;
    }

    // Send incremental update to all clients
    broadcast_state_update(
        &state,
        proto::state_update::Update::RoomRenamed(proto::RoomRenamed {
            room_id: Some(room_id_from_uuid(room_uuid)),
            new_name: rr.new_name,
        }),
    )
    .await?;
    
    // Send confirmation to the requesting client
    send_command_result(&sender, "RenameRoom", true, &format!("Renamed '{}' to '{}'", old_name, new_name)).await?;
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
    // Remove session
    state.remove_session(user_id);

    debug!(user_id, "server: cleaned up client");

    // Only broadcast if the client was authenticated
    if client_handle.authenticated.load(Ordering::SeqCst) {
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
