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

use crate::{
    persistence::Persistence,
    state::{
        ClientHandle, PeerCapabilitiesEntry, PendingAuth, ServerState, SessionEntry, UserStatus,
        compute_server_state_hash,
    },
};
use anyhow::Result;
use api::{
    ROOT_ROOM_UUID, build_auth_payload, build_session_cert_payload, compute_session_id, encode_frame,
    proto::{self, ServerState as ProtoServerState, VoiceDatagram, envelope::Payload},
    room_id_from_uuid, uuid_from_room_id,
};
use ed25519_dalek::{Signature, VerifyingKey};
use prost::Message;
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
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
            handle_rename_room(rr, sender, state, persistence).await?;
        }
        Some(Payload::MoveRoom(mr)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_move_room(mr, sender, state, persistence).await?;
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
        Some(Payload::TrackerAnnounce(ta)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_tracker_announce(ta, sender, state).await?;
        }
        Some(Payload::TrackerScrape(ts)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_tracker_scrape(ts, sender, state).await?;
        }
        Some(Payload::PeerCapabilities(pc)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_peer_capabilities(pc, sender, state).await?;
        }
        Some(Payload::P2pVoiceStatus(vs)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_p2p_voice_status(vs, sender, state).await?;
        }
        // Bridge messages (70-79)
        Some(Payload::BridgeHello(bh)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_bridge_hello(bh, sender, state).await?;
        }
        Some(Payload::BridgeRegisterUser(bru)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_bridge.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_bridge_register_user(bru, sender, state).await?;
        }
        Some(Payload::BridgeUnregisterUser(buu)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_bridge.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_bridge_unregister_user(buu, sender, state).await?;
        }
        Some(Payload::BridgeJoinRoom(bjr)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_bridge.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_bridge_join_room(bjr, sender, state).await?;
        }
        Some(Payload::BridgeSetUserStatus(bss)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_bridge.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_bridge_set_user_status(bss, sender, state).await?;
        }
        Some(Payload::BridgeChatMessage(bcm)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_bridge.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_bridge_chat_message(bcm, sender, state).await?;
        }
        // Server-to-client messages or empty - ignore
        Some(
            Payload::ServerHello(_)
            | Payload::ServerEvent(_)
            | Payload::AuthFailed(_)
            | Payload::CommandResult(_)
            | Payload::PeerAnnounce(_)
            | Payload::RelayAllocation(_)
            | Payload::BridgeUserRegistered(_),
        )
        | None => {}
        _ => {
            warn!("Received unknown or unhandled message type");
        }
    }
    Ok(())
}

async fn handle_tracker_announce(
    msg: proto::TrackerAnnounce,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    let info_hash: [u8; 20] = msg
        .info_hash
        .clone()
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("Invalid info_hash length {}, expected 20", v.len()))?;
    let peer_id: [u8; 20] = msg
        .peer_id
        .clone()
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("Invalid peer_id length {}, expected 20", v.len()))?;

    // Use the authenticated user ID from the sender, not from the message
    // This prevents spoofing
    let user_id = sender.user_id;

    info!(
        "Received TrackerAnnounce from user={} info_hash={} needs_relay={}",
        user_id,
        hex::encode(&info_hash),
        msg.needs_relay
    );

    // Use the client's IP address from the connection
    let ip = sender.conn.remote_address().ip();

    let event = proto::tracker_announce::Event::try_from(msg.event).ok();

    // Generate relay token BEFORE announce if client needs relay
    let relay_port = state.relay_port();
    let relay_token = if msg.needs_relay && relay_port > 0 {
        Some(state.relay_tokens.generate_token(user_id))
    } else {
        if msg.needs_relay && relay_port == 0 {
            debug!("Client requested relay but relay service is not enabled");
        }
        None
    };

    let (complete, incomplete, peers) = state
        .tracker
        .announce(
            info_hash,
            peer_id,
            user_id,
            ip,
            msg.port as u16,
            msg.uploaded,
            msg.downloaded,
            msg.left,
            event,
            msg.needs_relay,
            relay_token,
        )
        .await;

    // Check if any peers need relay - if so, client needs to know the relay port
    let has_relay_peers = peers.iter().any(|p| p.needs_relay && p.relay_token.is_some());

    // Include relay info if:
    // 1. Client requested relay mode (they need their own token)
    // 2. OR there are relay peers (client needs to know relay port to reach them)
    let relay = if relay_port > 0 && (msg.needs_relay || has_relay_peers) {
        let token = if msg.needs_relay {
            relay_token.map(|t| hex::encode(t)).unwrap_or_default()
        } else {
            // Client doesn't need a token for themselves, but we still tell them the port
            String::new()
        };
        Some(proto::RelayInfo {
            relay_token: token,
            relay_port: relay_port as u32,
        })
    } else {
        None
    };

    let response = proto::TrackerAnnounceResponse {
        interval: 1800,
        min_interval: 60,
        complete,
        incomplete,
        peers: peers
            .into_iter()
            .map(|p| proto::PeerInfo {
                peer_id: p.peer_id.to_vec(),
                user_id: p.user_id,
                ip: p.ip.to_string(),
                port: p.port as u32,
                supports_relay: p.needs_relay,
                relay_token: p.relay_token.map(|t| hex::encode(t)),
            })
            .collect(),
        request_id: msg.request_id,
        relay,
    };

    let envelope = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::TrackerAnnounceResponse(response)),
    };

    let frame = api::encode_frame(&envelope);
    sender.send_frame(&frame).await?;
    Ok(())
}

async fn handle_tracker_scrape(
    msg: proto::TrackerScrape,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    let info_hashes: Vec<[u8; 20]> = msg
        .info_hashes
        .iter()
        .filter_map(|h| h.clone().try_into().ok())
        .collect();

    let stats = state.tracker.scrape(info_hashes).await;

    let mut files = std::collections::HashMap::new();
    for (hash, (complete, downloaded, incomplete)) in stats {
        let hash_hex = hex::encode(hash);
        files.insert(
            hash_hex,
            proto::ScrapeStats {
                complete,
                downloaded,
                incomplete,
            },
        );
    }

    let response = proto::TrackerScrapeResponse { files };

    let envelope = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::TrackerScrapeResponse(response)),
    };

    let frame = api::encode_frame(&envelope);
    sender.send_frame(&frame).await?;
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
                        _ => {
                            return send_auth_failed(&sender, "Password required for new users").await;
                        }
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
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
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

    // 5. Verify and record session certificate
    let Some(cert) = auth.session_cert else {
        return send_auth_failed(&sender, "Missing session certificate").await;
    };

    if cert.session_public_key.len() != 32 {
        return send_auth_failed(&sender, "Invalid session public key length").await;
    }
    let session_public_key: [u8; 32] = match cert.session_public_key.as_slice().try_into() {
        Ok(k) => k,
        Err(_) => return send_auth_failed(&sender, "Invalid session public key").await,
    };

    // Basic time validity check
    if cert.expires_ms <= cert.issued_ms {
        return send_auth_failed(&sender, "Session certificate expiry invalid").await;
    }
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    if cert.expires_ms < now_ms {
        return send_auth_failed(&sender, "Session certificate expired").await;
    }

    // Verify certificate signature with the user's long-term key
    let cert_payload = build_session_cert_payload(
        &session_public_key,
        cert.issued_ms,
        cert.expires_ms,
        cert.device.as_deref(),
    );
    let cert_sig: [u8; 64] = match cert.user_signature.as_slice().try_into() {
        Ok(s) => s,
        Err(_) => return send_auth_failed(&sender, "Invalid session certificate signature length").await,
    };
    let cert_sig = Signature::from_bytes(&cert_sig);
    if verifying_key.verify_strict(&cert_payload, &cert_sig).is_err() {
        return send_auth_failed(&sender, "Invalid session certificate signature").await;
    }

    let session_id = compute_session_id(&session_public_key);
    let session_entry = SessionEntry {
        user_public_key: pending.public_key,
        session_public_key,
        session_id,
        issued_ms: cert.issued_ms,
        expires_ms: cert.expires_ms,
        device: cert.device.clone(),
    };

    // 6. Authentication successful
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

    // 9. Track session (user + session keys)
    state.add_session(sender.user_id, session_entry);

    // 10. Determine initial room (restore last room if registered, otherwise Root)
    let initial_room = if let Some(ref persist) = persistence {
        if let Some(registered) = persist.get_registered_user(&pending.public_key) {
            if let Some(last_room_bytes) = registered.last_room {
                let last_uuid = uuid::Uuid::from_bytes(last_room_bytes);
                // Check if the room still exists
                let rooms = state.get_rooms().await;
                if rooms
                    .iter()
                    .any(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(last_uuid))
                {
                    info!(user_id = sender.user_id, room = %last_uuid, "Restoring user to last room");
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
    let target_key = match state.get_user_public_key(target_user_id) {
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
    if let Err(e) = persist.register_user(
        &target_key,
        crate::persistence::RegisteredUser {
            username: username.clone(),
            roles: vec![],
            last_room: None,
        },
    ) {
        return send_command_result(&sender, "RegisterUser", false, &format!("Registration failed: {e}")).await;
    }

    send_command_result(
        &sender,
        "RegisterUser",
        true,
        &format!("Registered '{}' successfully", username),
    )
    .await
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
    let target_key = match state.get_user_public_key(target_user_id) {
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

    send_command_result(
        &sender,
        "UnregisterUser",
        true,
        &format!("Unregistered '{}' successfully", username),
    )
    .await
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

    let sender_room = state.get_user_room(sender.user_id).await.unwrap_or(ROOT_ROOM_UUID);

    // Use provided message ID and timestamp, or generate new ones if not provided
    let message_id = if msg.id.len() == 16 {
        msg.id
    } else {
        uuid::Uuid::new_v4().into_bytes().to_vec()
    };

    let timestamp_ms = if msg.timestamp_ms > 0 {
        msg.timestamp_ms
    } else {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    };

    let broadcast = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ServerEvent(proto::ServerEvent {
            kind: Some(proto::server_event::Kind::ChatBroadcast(proto::ChatBroadcast {
                id: message_id,
                timestamp_ms,
                sender: msg.sender,
                text: msg.text,
            })),
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
    let room = rooms
        .iter()
        .find(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(new_room_uuid));

    let room_name = match room {
        Some(r) => r.name.clone(),
        None => {
            return send_command_result(&sender, "JoinRoom", false, "Room not found").await;
        }
    };

    // let _old_room_uuid = state
    //     .get_user_room(sender.user_id)
    //     .await
    //     .unwrap_or(ROOT_ROOM_UUID);

    state.set_user_room(sender.user_id, new_room_uuid).await;

    // Save last room for registered users
    if let Some(ref persist) = persistence {
        if let Some(public_key) = state.get_user_public_key(sender.user_id) {
            if persist.is_registered(&public_key) {
                if let Err(e) = persist.update_user_last_room(&public_key, Some(new_room_uuid.into_bytes())) {
                    warn!("Failed to save user's last room: {e}");
                }
            }
        }
    }

    // Send incremental update about user moving rooms (from_room is implicit)
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserMoved(proto::UserMoved {
            user_id: Some(proto::UserId { value: sender.user_id }),
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

    // Extract parent UUID if provided
    let parent_uuid = cr.parent_id.as_ref().and_then(uuid_from_room_id);

    // Create the room with parent
    let room_uuid = state.create_room_with_parent(cr.name.clone(), parent_uuid).await;

    // Persist the new room
    if let Some(ref persist) = persistence {
        let room = crate::persistence::PersistedRoom {
            name: room_name.clone(),
            parent: parent_uuid.map(|u| *u.as_bytes()),
            description: String::new(),
            permanent: true,
        };
        if let Err(e) = persist.save_room(&room_uuid.into_bytes(), &room) {
            warn!("Failed to persist room: {e}");
        }
    }

    // Send incremental update to all clients
    let room_info = proto::RoomInfo {
        id: Some(room_id_from_uuid(room_uuid)),
        name: cr.name,
        parent_id: parent_uuid.map(room_id_from_uuid),
    };
    broadcast_state_update(
        &state,
        proto::state_update::Update::RoomCreated(proto::RoomCreated { room: Some(room_info) }),
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
    let room_name = state
        .get_rooms()
        .await
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
        if let Err(e) = persist.delete_room(&room_uuid.into_bytes()) {
            warn!("Failed to remove room from persistence: {e}");
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
async fn handle_rename_room(
    rr: proto::RenameRoom,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    let room_uuid = rr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);

    // Get old room name for the message
    let old_name = state
        .get_rooms()
        .await
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

    // Persist the room rename
    if let Some(ref persist) = persistence {
        let room_uuid_bytes = room_uuid.into_bytes();
        if let Some(mut room) = persist.get_room(&room_uuid_bytes) {
            room.name = new_name.clone();
            if let Err(e) = persist.save_room(&room_uuid_bytes, &room) {
                warn!("Failed to persist room rename: {e}");
            }
        }
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
    send_command_result(
        &sender,
        "RenameRoom",
        true,
        &format!("Renamed '{}' to '{}'", old_name, new_name),
    )
    .await?;
    Ok(())
}

/// Handle move room request.
async fn handle_move_room(
    mr: proto::MoveRoom,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    let room_uuid = mr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);

    let new_parent_uuid = mr
        .new_parent_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);

    // Get room name for the message
    let room_name = state
        .get_rooms()
        .await
        .iter()
        .find(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(room_uuid))
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "room".to_string());

    let new_parent_name = state
        .get_rooms()
        .await
        .iter()
        .find(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(new_parent_uuid))
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "Root".to_string());

    info!("MoveRoom: {} -> parent {}", room_uuid, new_parent_uuid);

    let moved = state.move_room(room_uuid, new_parent_uuid).await;

    if !moved {
        return send_command_result(&sender, "MoveRoom", false, "Room not found").await;
    }

    // Persist the room move
    if let Some(ref persist) = persistence {
        let room_uuid_bytes = room_uuid.into_bytes();
        if let Some(mut room) = persist.get_room(&room_uuid_bytes) {
            room.parent = Some(new_parent_uuid.into_bytes());
            if let Err(e) = persist.save_room(&room_uuid_bytes, &room) {
                warn!("Failed to persist room move: {e}");
            }
        }
    }

    // Send incremental update to all clients
    broadcast_state_update(
        &state,
        proto::state_update::Update::RoomMoved(proto::RoomMoved {
            room_id: mr.room_id,
            new_parent_id: mr.new_parent_id,
        }),
    )
    .await?;

    // Send confirmation to the requesting client
    send_command_result(
        &sender,
        "MoveRoom",
        true,
        &format!("Moved '{}' into '{}'", room_name, new_parent_name),
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
        debug!("  actual: {:02x?}...", &rss.actual_hash[..rss.actual_hash.len().min(8)]);
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
pub async fn broadcast_state_update(state: &Arc<ServerState>, update: proto::state_update::Update) -> Result<()> {
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
///
/// If the client is a bridge, also cleans up all its virtual users.
pub async fn cleanup_client(client_handle: &Arc<ClientHandle>, state: &Arc<ServerState>) {
    let user_id = client_handle.user_id;
    let is_bridge = client_handle.is_bridge.load(Ordering::SeqCst);

    // If this is a bridge, clean up all virtual users it owned
    if is_bridge {
        let virtual_user_ids = state.get_virtual_users_for_bridge(user_id);
        for vu_id in virtual_user_ids {
            state.remove_virtual_user(vu_id);
            state.remove_user_membership(vu_id).await;
            if let Err(e) = broadcast_state_update(
                state,
                proto::state_update::Update::UserLeft(proto::UserLeft {
                    user_id: Some(proto::UserId { value: vu_id }),
                }),
            )
            .await
            {
                error!(virtual_user_id = vu_id, "failed to broadcast virtual user leave: {e:?}");
            }
            debug!(bridge_id = user_id, virtual_user_id = vu_id, "cleaned up virtual user");
        }
    }

    // Broadcast peer removal before removing session data
    if client_handle.authenticated.load(Ordering::SeqCst) {
        broadcast_peer_removal(user_id, state).await;
    }

    // Remove client from DashMap (lock-free)
    state.remove_client_by_handle(client_handle);
    // Remove membership
    state.remove_user_membership(user_id).await;
    // Remove session (also removes peer_capabilities)
    state.remove_session(user_id);

    debug!(user_id, "server: cleaned up client");

    // Only broadcast if the client was authenticated and not a bridge
    // (bridge user was already removed from visible state in BridgeHello)
    if client_handle.authenticated.load(Ordering::SeqCst) && !is_bridge {
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
pub async fn handle_datagrams(conn: quinn::Connection, state: Arc<ServerState>, sender_user_id: u64) {
    loop {
        match conn.read_datagram().await {
            Ok(datagram) => {
                // Decode the VoiceDatagram protobuf
                match VoiceDatagram::decode(datagram.as_ref()) {
                    Ok(mut voice_dgram) => {
                        // Determine the effective sender for this datagram.
                        // For bridge connections, trust the sender_id if it matches
                        // a virtual user owned by this bridge.
                        let is_bridge = state
                            .get_client(sender_user_id)
                            .is_some_and(|c| c.is_bridge.load(Ordering::SeqCst));

                        let effective_sender = if is_bridge {
                            // Bridge: use the datagram's sender_id if it's a valid virtual user
                            match voice_dgram.sender_id {
                                Some(claimed_id) if state.is_virtual_user_of(claimed_id, sender_user_id) => claimed_id,
                                _ => {
                                    debug!(
                                        bridge_id = sender_user_id,
                                        claimed_sender = ?voice_dgram.sender_id,
                                        "server: bridge datagram with invalid sender_id, dropping"
                                    );
                                    continue;
                                }
                            }
                        } else {
                            // Normal client: always use the connection's user_id
                            sender_user_id
                        };

                        // Check if sender is muted — drop voice silently
                        let status = state.get_user_status(effective_sender).await;
                        if status.is_muted {
                            debug!(sender = effective_sender, "server: dropping voice from muted user");
                            continue;
                        }

                        debug!(
                            sender = effective_sender,
                            seq = voice_dgram.sequence,
                            data_len = voice_dgram.opus_data.len(),
                            "server: received voice datagram"
                        );

                        // Take a snapshot of room memberships to avoid holding locks during relay
                        let room_memberships = state.snapshot_room_memberships().await;

                        // Find effective sender's room from the snapshot
                        let sender_room = room_memberships.iter().find_map(|(rid, users)| {
                            if users.contains(&effective_sender) {
                                Some(*rid)
                            } else {
                                None
                            }
                        });

                        // Only relay if sender is actually in a room
                        let Some(actual_room) = sender_room else {
                            debug!(
                                sender = effective_sender,
                                "server: sender not in any room, dropping datagram"
                            );
                            continue;
                        };

                        // Set sender_id and room_id (server-authoritative)
                        voice_dgram.sender_id = Some(effective_sender);
                        voice_dgram.room_id = Some(actual_room.as_bytes().to_vec());

                        // Re-encode with corrected sender_id and room_id
                        let relay_bytes = voice_dgram.encode_to_vec();

                        // Get recipients from the snapshot (no lock needed)
                        let recipients = room_memberships.get(&actual_room).map(|v| v.as_slice()).unwrap_or(&[]);

                        // Track which connections already received this datagram to avoid
                        // sending duplicates when multiple virtual users from the same
                        // bridge are in the room.
                        let mut sent_to = std::collections::HashSet::new();

                        // Relay to each recipient (lock-free client lookup via DashMap)
                        for &recipient_id in recipients {
                            // Skip the effective sender
                            if recipient_id == effective_sender {
                                continue;
                            }

                            // Virtual users don't have their own connections;
                            // voice for them goes through their bridge's connection.
                            // Look up the actual client to send to.
                            let (target_conn_id, target_client) = if let Some(vu) = state.get_virtual_user(recipient_id)
                            {
                                // Send to the bridge that owns this virtual user
                                (vu.bridge_owner_id, state.get_client(vu.bridge_owner_id))
                            } else {
                                (recipient_id, state.get_client(recipient_id))
                            };

                            // Skip if we already sent to this connection
                            if !sent_to.insert(target_conn_id) {
                                continue;
                            }

                            if let Some(client) = target_client {
                                if let Err(e) = client.conn.send_datagram(relay_bytes.clone().into()) {
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

// =============================================================================
// Bridge Handlers
// =============================================================================

/// Handle BridgeHello - mark connection as a bridge and hide its user entry.
async fn handle_bridge_hello(bh: proto::BridgeHello, sender: Arc<ClientHandle>, state: Arc<ServerState>) -> Result<()> {
    if sender.is_bridge.load(Ordering::SeqCst) {
        return send_command_result(&sender, "BridgeHello", false, "Already in bridge mode").await;
    }

    info!(
        user_id = sender.user_id,
        bridge_name = %bh.bridge_name,
        "BridgeHello: marking connection as bridge"
    );

    sender.is_bridge.store(true, Ordering::SeqCst);

    // Remove the bridge's own user from memberships so it doesn't appear in the user list.
    // The bridge itself is infrastructure, not a visible user.
    state.remove_user_membership(sender.user_id).await;

    // Broadcast UserLeft for the bridge user so existing clients remove it
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserLeft(proto::UserLeft {
            user_id: Some(proto::UserId { value: sender.user_id }),
        }),
    )
    .await?;

    send_command_result(&sender, "BridgeHello", true, "Bridge mode activated").await
}

/// Handle BridgeRegisterUser - create a virtual user owned by this bridge.
async fn handle_bridge_register_user(
    bru: proto::BridgeRegisterUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    let virtual_user_id = state.allocate_user_id();

    info!(
        bridge_id = sender.user_id,
        virtual_user_id,
        username = %bru.username,
        "BridgeRegisterUser: creating virtual user"
    );

    // Register the virtual user in state
    state.register_virtual_user(virtual_user_id, bru.username.clone(), sender.user_id);

    // Place virtual user in root room by default
    state.set_user_room(virtual_user_id, ROOT_ROOM_UUID).await;

    // Broadcast UserJoined for the virtual user
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserJoined(proto::UserJoined {
            user: Some(proto::User {
                user_id: Some(proto::UserId { value: virtual_user_id }),
                username: bru.username.clone(),
                current_room: Some(room_id_from_uuid(ROOT_ROOM_UUID)),
                is_muted: false,
                is_deafened: false,
            }),
        }),
    )
    .await?;

    // Send the assigned user_id back to the bridge
    let reply = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeUserRegistered(proto::BridgeUserRegistered {
            user_id: virtual_user_id,
            username: bru.username,
        })),
    };
    let frame = encode_frame(&reply);
    let _ = sender.send_frame(&frame).await;

    Ok(())
}

/// Handle BridgeUnregisterUser - remove a virtual user owned by this bridge.
async fn handle_bridge_unregister_user(
    buu: proto::BridgeUnregisterUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    // Verify the bridge owns this virtual user
    if !state.is_virtual_user_of(buu.user_id, sender.user_id) {
        warn!(
            bridge_id = sender.user_id,
            virtual_user_id = buu.user_id,
            "BridgeUnregisterUser: bridge does not own this virtual user"
        );
        return send_command_result(&sender, "BridgeUnregisterUser", false, "Not your virtual user").await;
    }

    info!(
        bridge_id = sender.user_id,
        virtual_user_id = buu.user_id,
        "BridgeUnregisterUser: removing virtual user"
    );

    // Remove from state
    state.remove_virtual_user(buu.user_id);
    state.remove_user_membership(buu.user_id).await;

    // Broadcast UserLeft
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserLeft(proto::UserLeft {
            user_id: Some(proto::UserId { value: buu.user_id }),
        }),
    )
    .await?;

    send_command_result(&sender, "BridgeUnregisterUser", true, "Virtual user removed").await
}

/// Handle BridgeJoinRoom - move a virtual user to a room.
async fn handle_bridge_join_room(
    bjr: proto::BridgeJoinRoom,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    // Verify ownership
    if !state.is_virtual_user_of(bjr.user_id, sender.user_id) {
        return send_command_result(&sender, "BridgeJoinRoom", false, "Not your virtual user").await;
    }

    let room_uuid = bjr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);

    // Verify room exists
    let rooms = state.get_rooms().await;
    if !rooms
        .iter()
        .any(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(room_uuid))
    {
        return send_command_result(&sender, "BridgeJoinRoom", false, "Room not found").await;
    }

    state.set_user_room(bjr.user_id, room_uuid).await;

    // Broadcast UserMoved
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserMoved(proto::UserMoved {
            user_id: Some(proto::UserId { value: bjr.user_id }),
            to_room_id: Some(room_id_from_uuid(room_uuid)),
        }),
    )
    .await?;

    send_command_result(&sender, "BridgeJoinRoom", true, "Virtual user moved").await
}

/// Handle BridgeSetUserStatus - update a virtual user's mute/deaf status.
async fn handle_bridge_set_user_status(
    bss: proto::BridgeSetUserStatus,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    if !state.is_virtual_user_of(bss.user_id, sender.user_id) {
        return send_command_result(&sender, "BridgeSetUserStatus", false, "Not your virtual user").await;
    }

    let status = UserStatus {
        is_muted: bss.is_muted,
        is_deafened: bss.is_deafened,
    };
    state.set_user_status(bss.user_id, status).await;

    // Broadcast UserStatusChanged
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserStatusChanged(proto::UserStatusChanged {
            user_id: Some(proto::UserId { value: bss.user_id }),
            is_muted: bss.is_muted,
            is_deafened: bss.is_deafened,
        }),
    )
    .await?;

    Ok(())
}

/// Handle BridgeChatMessage - send chat on behalf of a virtual user.
async fn handle_bridge_chat_message(
    bcm: proto::BridgeChatMessage,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    if !state.is_virtual_user_of(bcm.user_id, sender.user_id) {
        return send_command_result(&sender, "BridgeChatMessage", false, "Not your virtual user").await;
    }

    let vu = match state.get_virtual_user(bcm.user_id) {
        Some(vu) => vu,
        None => return Ok(()),
    };

    let sender_room = state.get_user_room(bcm.user_id).await.unwrap_or(ROOT_ROOM_UUID);

    info!(
        virtual_user_id = bcm.user_id,
        username = %vu.username,
        "BridgeChatMessage"
    );

    let message_id = uuid::Uuid::new_v4().into_bytes().to_vec();
    let timestamp_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;

    let broadcast = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ServerEvent(proto::ServerEvent {
            kind: Some(proto::server_event::Kind::ChatBroadcast(proto::ChatBroadcast {
                id: message_id,
                timestamp_ms,
                sender: vu.username,
                text: bcm.text,
            })),
        })),
    };
    let frame = encode_frame(&broadcast);

    // Send to all clients in the same room
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

// =============================================================================
// P2P Control Plane Handlers
// =============================================================================

/// Handle PeerCapabilities message from a client.
/// This stores the client's P2P capabilities and broadcasts their info to other peers.
async fn handle_peer_capabilities(
    msg: proto::PeerCapabilities,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    let user_id = sender.user_id;

    info!(
        user_id,
        supports_file_transfer = msg.supports_file_transfer,
        supports_p2p_voice = msg.supports_p2p_voice,
        prefer_relay = msg.prefer_relay,
        multiaddrs_count = msg.multiaddrs.len(),
        "Received PeerCapabilities"
    );

    // Store the capabilities
    let capabilities = PeerCapabilitiesEntry {
        supports_file_transfer: msg.supports_file_transfer,
        supports_p2p_voice: msg.supports_p2p_voice,
        prefer_relay: msg.prefer_relay,
        libp2p_peer_id: msg.libp2p_peer_id.clone(),
        multiaddrs: msg.multiaddrs.clone(),
        bandwidth_tier: msg.bandwidth_tier,
    };
    state.set_peer_capabilities(user_id, capabilities);

    // Get the session entry to build the PeerAnnounce
    let Some(session) = state.get_session(user_id) else {
        warn!(user_id, "No session found for user when handling PeerCapabilities");
        return Ok(());
    };

    // Broadcast PeerAnnounce to all other connected clients
    let announce = proto::PeerAnnounce {
        user_id: Some(proto::UserId { value: user_id }),
        session_id: session.session_id.to_vec(),
        libp2p_peer_id: msg.libp2p_peer_id,
        multiaddrs: msg.multiaddrs,
        supports_relay: msg.prefer_relay, // If they prefer relay, they support it
        supports_p2p_voice: msg.supports_p2p_voice,
        is_removal: false,
    };

    let envelope = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::PeerAnnounce(announce)),
    };
    let frame = encode_frame(&envelope);

    // Broadcast to all other clients
    broadcast_to_others(&state, user_id, &frame).await;

    // Also send existing peers to the new client
    send_existing_peers_to_client(&sender, &state).await;

    Ok(())
}

/// Handle P2PVoiceStatus message from a client.
/// This tracks which peers a client has active P2P voice connections to.
async fn handle_p2p_voice_status(
    msg: proto::P2pVoiceStatus,
    sender: Arc<ClientHandle>,
    _state: Arc<ServerState>,
) -> Result<()> {
    let user_id = sender.user_id;

    debug!(
        user_id,
        p2p_mode_active = msg.p2p_mode_active,
        connected_sessions_count = msg.connected_sessions.len(),
        "Received P2PVoiceStatus"
    );

    // For now, just log this. In the future, we can use this to:
    // - Track which clients are using P2P vs relay for voice
    // - Adjust topology hints
    // - Route relay traffic only when needed

    // TODO: Store P2P voice status for topology decisions

    Ok(())
}

/// Send PeerAnnounce for all existing peers to a newly connected client.
async fn send_existing_peers_to_client(client: &Arc<ClientHandle>, state: &Arc<ServerState>) {
    let peers = state.get_all_peer_capabilities();

    for (peer_user_id, session, caps) in peers {
        // Don't announce the client to themselves
        if peer_user_id == client.user_id {
            continue;
        }

        let announce = proto::PeerAnnounce {
            user_id: Some(proto::UserId { value: peer_user_id }),
            session_id: session.session_id.to_vec(),
            libp2p_peer_id: caps.libp2p_peer_id,
            multiaddrs: caps.multiaddrs,
            supports_relay: caps.prefer_relay,
            supports_p2p_voice: caps.supports_p2p_voice,
            is_removal: false,
        };

        let envelope = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::PeerAnnounce(announce)),
        };
        let frame = encode_frame(&envelope);

        if let Err(e) = client.send_frame(&frame).await {
            debug!(
                user_id = client.user_id,
                peer_user_id,
                error = ?e,
                "Failed to send PeerAnnounce to client"
            );
        }
    }
}

/// Broadcast a removal announcement when a peer disconnects.
pub async fn broadcast_peer_removal(user_id: u64, state: &Arc<ServerState>) {
    // Get session info before it's removed
    let Some(session) = state.get_session(user_id) else {
        return;
    };

    let announce = proto::PeerAnnounce {
        user_id: Some(proto::UserId { value: user_id }),
        session_id: session.session_id.to_vec(),
        libp2p_peer_id: Vec::new(),
        multiaddrs: Vec::new(),
        supports_relay: false,
        supports_p2p_voice: false,
        is_removal: true,
    };

    let envelope = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::PeerAnnounce(announce)),
    };
    let frame = encode_frame(&envelope);

    broadcast_to_others(state, user_id, &frame).await;
}

/// Broadcast a frame to all connected clients except the sender.
async fn broadcast_to_others(state: &Arc<ServerState>, exclude_user_id: u64, frame: &[u8]) {
    for client in state.snapshot_clients() {
        if client.user_id == exclude_user_id {
            continue;
        }
        if let Err(e) = client.send_frame(frame).await {
            debug!(
                user_id = client.user_id,
                error = ?e,
                "Failed to broadcast frame to client"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    // Handler tests will require mock clients, which we'll add in a future step.
    // For now, the state module tests cover the core logic.
}
