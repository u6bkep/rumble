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
    acl,
    persistence::Persistence,
    plugin::{ServerCtx, ServerPlugin},
    state::{
        Binding, ClientHandle, Identity, Member, OwnerId, PendingAuth, ServerState, SessionEntry, UserStatus,
        VoiceFrame, compute_server_state_hash,
    },
};
use anyhow::Result;
use ed25519_dalek::{Signature, VerifyingKey};
use prost::Message;
use rumble_protocol::{
    ROOT_ROOM_UUID, build_auth_payload, build_session_cert_payload, compute_session_id, encode_frame,
    permissions::Permissions,
    proto::{self, ServerState as ProtoServerState, VoiceDatagram, envelope::Payload},
    room_id_from_uuid, uuid_from_room_id,
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Get current time as milliseconds since UNIX epoch, with a safe fallback.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Parse a chat line into a `(command, args)` pair if it is a slash command.
///
/// Returns the lowercased command keyword (no leading slash) and the trimmed
/// remainder of the line. `None` for plain text, a bare `/`, or `/ ...`.
fn parse_slash_command(text: &str) -> Option<(String, &str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (cmd, args) = match rest.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (rest, ""),
    };
    if cmd.is_empty() {
        return None;
    }
    Some((cmd.to_ascii_lowercase(), args))
}

/// Handle a decoded envelope from a client.
///
/// This is the main protocol handler. It processes the envelope payload
/// and updates server state accordingly. Plugins get first look at every
/// message; if a plugin returns `Ok(true)`, the message is considered
/// handled and the built-in match is skipped.
///
/// # Arguments
/// * `env` - The decoded envelope
/// * `sender` - Handle to the client that sent this message
/// * `state` - Shared server state
/// * `persistence` - Optional persistence layer for registered users
/// * `plugins` - Registered server plugins
/// * `ctx` - Plugin server context
///
/// # Returns
/// Ok(()) on success, Err on fatal errors that should close the connection.
pub async fn handle_envelope(
    env: proto::Envelope,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
    plugins: &[Arc<dyn ServerPlugin>],
    ctx: &ServerCtx,
) -> Result<()> {
    // Slash-command dispatch: a chat line beginning with `/<cmd>` whose command
    // is declared by a plugin is routed to that plugin's `on_command` and
    // consumed (never broadcast as chat). Only authenticated native chat is
    // eligible. Checked before `on_message` so command lines aren't also
    // observed as ordinary chat.
    if let Some(Payload::ChatMessage(msg)) = &env.payload
        && sender.authenticated.load(Ordering::SeqCst)
        && let Some((cmd, args)) = parse_slash_command(&msg.text)
    {
        for plugin in plugins.iter() {
            if plugin.commands().iter().any(|c| c.name.eq_ignore_ascii_case(&cmd)) {
                plugin.on_command(&cmd, args, &sender, ctx).await?;
                return Ok(());
            }
        }
    }

    // Try plugins first
    for plugin in plugins.iter() {
        if plugin.on_message(&env, &sender, ctx).await? {
            return Ok(());
        }
    }

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
            handle_chat_message(msg, sender, state, persistence.clone()).await?;
        }
        Some(Payload::DirectMessage(dm)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_direct_message(dm, sender, state).await?;
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
        Some(Payload::SetRoomDescription(srd)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_set_room_description(srd, sender, state, persistence).await?;
        }
        Some(Payload::RequestStateSync(rss)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_request_state_sync(rss, sender, state, persistence).await?;
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
        // Controller / participant messages (70-79)
        Some(Payload::ControllerHello(ch)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_controller_hello(ch, sender, state, persistence).await?;
        }
        Some(Payload::RegisterParticipant(rp)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_controller.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_register_participant(rp, sender, state, persistence).await?;
        }
        Some(Payload::UnregisterParticipant(up)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_controller.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_unregister_participant(up, sender, state).await?;
        }
        Some(Payload::MoveParticipant(mp)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_controller.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_move_participant(mp, sender, state).await?;
        }
        Some(Payload::SetParticipantStatus(sps)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_controller.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_set_participant_status(sps, sender, state).await?;
        }
        Some(Payload::ParticipantChat(pc)) => {
            if !sender.authenticated.load(Ordering::SeqCst) || !sender.is_controller.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_participant_chat(pc, sender, state, persistence).await?;
        }
        // ACL messages (80-90)
        Some(Payload::KickUser(ku)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_kick_user(ku, sender, state, persistence).await?;
        }
        Some(Payload::BanUser(bu)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_ban_user(bu, sender, state, persistence).await?;
        }
        Some(Payload::SetServerMute(ssm)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_set_server_mute(ssm, sender, state, persistence.clone()).await?;
        }
        Some(Payload::Elevate(elev)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_elevate(elev, sender, state, persistence).await?;
        }
        Some(Payload::CreateGroup(cg)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_create_group(cg, sender, state, persistence).await?;
        }
        Some(Payload::DeleteGroup(dg)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_delete_group(dg, sender, state, persistence).await?;
        }
        Some(Payload::ModifyGroup(mg)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_modify_group(mg, sender, state, persistence).await?;
        }
        Some(Payload::SetUserGroup(sug)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_set_user_group(sug, sender, state, persistence).await?;
        }
        Some(Payload::SetRoomAcl(sra)) => {
            if !sender.authenticated.load(Ordering::SeqCst) {
                return Ok(());
            }
            handle_set_room_acl(sra, sender, state, persistence).await?;
        }
        // Server-to-client messages or empty - ignore
        Some(
            Payload::ServerHello(_)
            | Payload::ServerEvent(_)
            | Payload::AuthFailed(_)
            | Payload::CommandResult(_)
            | Payload::ParticipantRegistered(_)
            | Payload::PermissionDenied(_)
            | Payload::UserKicked(_),
        )
        | None => {}
    }
    Ok(())
}

/// Handle ClientHello - begin authentication handshake.
async fn handle_client_hello(
    ch: proto::ClientHello,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    info!("ClientHello from {}", ch.username);

    // 1. Validate public key length
    if ch.public_key.len() != 32 {
        return send_auth_failed(&sender, "Invalid public key length").await;
    }
    let public_key: [u8; 32] = ch.public_key.try_into().unwrap();

    // 2. Check registration constraints.
    // Check if username is taken by a DIFFERENT key.
    // Note: If this key is registered, they can provide any name - we'll override it later
    // with their registered name. We only block if they're trying to use someone ELSE's
    // registered name.
    if persistence.is_username_taken(&ch.username, &public_key) {
        return send_auth_failed(&sender, "Username is registered to a different key").await;
    }

    // 3. Check password for unknown keys
    {
        let is_known = persistence.is_known_key(&public_key);
        if !is_known
            && let Ok(required) = std::env::var("RUMBLE_SERVER_PASSWORD")
            && !required.is_empty()
        {
            match &ch.password {
                Some(pw) if pw == &required => { /* OK */ }
                _ => {
                    return send_auth_failed(&sender, "Password required for new users").await;
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
    persistence: Arc<Persistence>,
) -> Result<()> {
    // 1. Get pending auth state
    let pending = match state.take_pending_auth(sender.user_id) {
        Some(p) => p,
        None => return send_auth_failed(&sender, "No pending authentication").await,
    };

    // 2. Check timestamp (±5 minutes). Use saturating arithmetic: timestamp_ms
    // is an attacker-controlled i64, and a raw subtraction (or .abs() on
    // i64::MIN) would overflow and panic in debug builds.
    let now_ms = now_ms();
    let diff_ms = now_ms.saturating_sub(auth.timestamp_ms).saturating_abs();
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

    // 4b. Check BANNED permission via ACL evaluation, honouring ban expiry.
    {
        let persist = &persistence;
        // If a timed ban record exists and has expired, lift it proactively so
        // the group check below sees a clean state.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Some(ban_record) = persist.get_ban(&pending.public_key)
            && ban_record.is_expired(now_secs)
        {
            info!("ban expired at auth time, lifting ban");
            let _ = persist.remove_ban(&pending.public_key);
            let _ = persist.remove_user_from_group(&pending.public_key, "banned");
        }

        // Load user's groups to check for BANNED flag
        let mut check_groups = vec!["default".to_string()];
        let user_groups = persist.get_user_groups(&pending.public_key);
        for g in &user_groups {
            if !check_groups.contains(g) {
                check_groups.push(g.clone());
            }
        }
        // Add username-as-group
        if let Some(registered) = persist.get_registered_user(&pending.public_key)
            && !check_groups.contains(&registered.username)
        {
            check_groups.push(registered.username);
        }
        // Build group permissions map
        let mut group_perms = std::collections::HashMap::new();
        for (name, pg) in persist.list_groups() {
            // Truncate undefined bits rather than dropping the whole group, so a
            // stray unknown bit can't silently nullify the "banned" group and
            // let banned users through this auth-time check.
            group_perms.insert(
                name,
                rumble_protocol::permissions::Permissions::from_bits_truncate(pg.permissions),
            );
        }
        // Evaluate at root - if BANNED flag is present, reject
        let root_chain = vec![(ROOT_ROOM_UUID, None)];
        let effective =
            rumble_protocol::permissions::effective_permissions(&check_groups, &group_perms, &root_chain, false);
        if effective.contains(rumble_protocol::permissions::Permissions::BANNED) {
            return send_auth_failed(&sender, "You are banned from this server").await;
        }
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

    // 6b. Mark key as known.
    if let Err(e) = persistence.add_known_key(&pending.public_key) {
        warn!("Failed to mark key as known: {e}");
    }

    // 6c. Load the user's assigned groups and verified identity from
    // persistence. The implicit username-group is NOT baked into the assigned
    // groups here; ACL evaluation derives it from the verified identity name
    // (see acl::evaluate_identity_permissions), so members without a verified
    // identity never gain a username-keyed group.
    let verified_username = {
        let persist = &persistence;
        let mut groups = vec!["default".to_string()];
        if let Some(user_groups_data) = persist.get_raw("user_groups", &pending.public_key)
            && let Ok(stored_groups) = bincode::deserialize::<Vec<String>>(&user_groups_data)
        {
            for g in stored_groups {
                if !groups.contains(&g) {
                    groups.push(g);
                }
            }
        }
        sender.identity.set_groups(groups).await;
        persist.get_registered_user(&pending.public_key).map(|r| r.username)
    };
    sender.identity.set_verified_username(verified_username.clone()).await;

    // 7. Display name: the registered username overrides the client-provided one.
    let final_username = verified_username.clone().unwrap_or_else(|| pending.username.clone());

    // 8. Set username
    sender.set_username(final_username.clone()).await;
    info!(user_id = sender.user_id, username = %final_username, "Authentication successful");

    // 9. Track session (user + session keys)
    state.add_session(sender.user_id, session_entry);

    // 10. Determine initial room (restore last room if registered, otherwise Root)
    let initial_room = {
        let persist = &persistence;
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
    };
    state.set_user_room(sender.user_id, initial_room).await;

    // 11. Send initial ServerState
    send_server_state_to_client(&sender, &state, &persistence).await?;

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
                server_muted: false,
                is_elevated: false,
                groups: sender.identity.groups().await,
                label: None,
            }),
        }),
    )
    .await?;

    // 13. Send welcome message if configured
    if let Some(welcome_text) = state.welcome_message() {
        let welcome_env = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::ServerEvent(proto::ServerEvent {
                kind: Some(proto::server_event::Kind::WelcomeMessage(proto::WelcomeMessage {
                    text: welcome_text.to_string(),
                })),
            })),
        };
        let frame = encode_frame(&welcome_env);
        if let Err(e) = sender.send_frame(&frame).await {
            warn!(user_id = sender.user_id, error = ?e, "Failed to send welcome message");
        }
    }

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

/// Send PermissionDenied message to client.
pub(crate) async fn send_permission_denied(sender: &ClientHandle, denied: proto::PermissionDenied) -> Result<()> {
    let env = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::PermissionDenied(denied)),
    };
    let frame = encode_frame(&env);
    let _ = sender.send_frame(&frame).await;
    Ok(())
}

/// Handle RegisterUser request.
async fn handle_register_user(
    req: proto::RegisterUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Extract user_id from the UserId message
    let target_user_id = req.user_id.map(|u| u.value).unwrap_or(0);

    // Permission check: SELF_REGISTER if registering self, REGISTER if registering others
    let required = if target_user_id == sender.user_id {
        Permissions::SELF_REGISTER
    } else {
        Permissions::REGISTER
    };
    if let Err(denied) = acl::check_permission(&state, &sender, Uuid::nil(), required, &persistence).await {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    match crate::ops::apply_register_user(&state, &persistence, target_user_id).await {
        Ok(msg) => send_command_result(&sender, "RegisterUser", true, &msg).await,
        Err(msg) => send_command_result(&sender, "RegisterUser", false, &msg).await,
    }
}

/// Handle UnregisterUser request.
async fn handle_unregister_user(
    req: proto::UnregisterUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Extract user_id from the UserId message
    let target_user_id = req.user_id.map(|u| u.value).unwrap_or(0);

    // Permission check: SELF_REGISTER if unregistering self, REGISTER if unregistering others
    let required = if target_user_id == sender.user_id {
        Permissions::SELF_REGISTER
    } else {
        Permissions::REGISTER
    };
    if let Err(denied) = acl::check_permission(&state, &sender, Uuid::nil(), required, &persistence).await {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    match crate::ops::apply_unregister_user(&state, &persistence, target_user_id).await {
        Ok(msg) => send_command_result(&sender, "UnregisterUser", true, &msg).await,
        Err(msg) => send_command_result(&sender, "UnregisterUser", false, &msg).await,
    }
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
///
/// When the `tree` flag is set, the message is broadcast to the sender's room
/// AND all descendant rooms in the hierarchy.
async fn handle_chat_message(
    msg: proto::ChatMessage,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    let is_tree = msg.tree.unwrap_or(false);
    // Identity comes from the authenticated connection, NOT the client's
    // `msg.sender` field — otherwise a client could broadcast as anyone.
    info!("chat from uid={}: {} (tree={})", sender.user_id, msg.text, is_tree);

    // Use provided message ID and timestamp, or generate new ones if not provided
    let message_id = if msg.id.len() == 16 {
        msg.id
    } else {
        uuid::Uuid::new_v4().into_bytes().to_vec()
    };
    let timestamp_ms = if msg.timestamp_ms > 0 {
        msg.timestamp_ms
    } else {
        now_ms()
    };

    broadcast_chat_as(
        &state,
        &persistence,
        sender.user_id,
        msg.text,
        is_tree,
        message_id,
        timestamp_ms,
        msg.attachment,
    )
    .await
}

/// Broadcast a chat message authored by `sender_id` (a real client *or* a
/// participant) to its *current* room — and descendants if `tree`. Thin wrapper
/// over [`broadcast_chat_in_room`] that resolves the author's room.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn broadcast_chat_as(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    sender_id: u64,
    text: String,
    is_tree: bool,
    id: Vec<u8>,
    timestamp_ms: i64,
    attachment: Option<proto::ChatAttachment>,
) -> Result<()> {
    let sender_room = state.get_user_room(sender_id).await.unwrap_or(ROOT_ROOM_UUID);
    broadcast_chat_in_room(
        state,
        persistence,
        sender_id,
        sender_room,
        text,
        is_tree,
        id,
        timestamp_ms,
        attachment,
    )
    .await
}

/// Broadcast a chat message authored by `sender_id` into an explicit `room`
/// (and descendants if `tree`), rather than the author's current room. This is
/// the single source of truth for chat fan-out:
///
/// - ACL (`TEXT_MESSAGE`) is enforced in `room` via the member's identity, so
///   participants are subject to the same permission machinery as humans.
/// - Each recipient is resolved to its *delivery connection* (a participant's
///   chat reaches its controller), deduped per connection, so a controller with
///   several participants in the room receives the message once. This is what
///   makes inbound chat reach bridged users.
/// - The author's own delivery connection is never echoed: a client inserts its
///   own copy locally, and a controller already knows the message it sent.
///
/// [`broadcast_chat_as`] targets the author's own room (the common case);
/// plugins use this directly to reply into a room the bot is not a member of.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn broadcast_chat_in_room(
    state: &Arc<ServerState>,
    persistence: &Arc<Persistence>,
    sender_id: u64,
    room: Uuid,
    text: String,
    is_tree: bool,
    id: Vec<u8>,
    timestamp_ms: i64,
    attachment: Option<proto::ChatAttachment>,
) -> Result<()> {
    let Some(member) = state.get_member(sender_id) else {
        return Ok(());
    };

    // Permission check: TEXT_MESSAGE in the target room.
    if let Err(denied) =
        acl::check_member_permission(state, &member, room, Permissions::TEXT_MESSAGE, persistence).await
    {
        // Only a connected client can be told why; participants are driven by
        // their controller, which is responsible for its own UX.
        if let Some(client) = member.client() {
            send_permission_denied(client, denied).await?;
        }
        return Ok(());
    }

    let broadcast = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ServerEvent(proto::ServerEvent {
            kind: Some(proto::server_event::Kind::ChatBroadcast(proto::ChatBroadcast {
                id,
                timestamp_ms,
                sender: member.identity.display_name().await,
                sender_id,
                text,
                tree: if is_tree { Some(true) } else { None },
                attachment,
            })),
        })),
    };
    let frame = encode_frame(&broadcast);

    let target_rooms = if is_tree {
        collect_descendant_rooms(state, room).await
    } else {
        let mut set = std::collections::HashSet::new();
        set.insert(room);
        set
    };

    let author_conn = state.delivery_client(sender_id).map(|c| c.user_id);
    let memberships = state.snapshot_room_memberships().await;
    let mut sent_to = std::collections::HashSet::new();
    for (member_room, members) in &memberships {
        if !target_rooms.contains(member_room) {
            continue;
        }
        for &uid in members {
            let Some(client) = state.delivery_client(uid) else {
                continue;
            };
            if Some(client.user_id) == author_conn {
                continue;
            }
            if !sent_to.insert(client.user_id) {
                continue;
            }
            if let Err(e) = client.send_frame(&frame).await {
                error!("chat broadcast write failed: {e:?}");
            }
        }
    }

    Ok(())
}

/// Collect a room and all its descendants from the room hierarchy.
async fn collect_descendant_rooms(state: &ServerState, root: uuid::Uuid) -> std::collections::HashSet<uuid::Uuid> {
    let rooms = state.get_rooms().await;

    // Build parent -> children map
    let mut children_map: std::collections::HashMap<uuid::Uuid, Vec<uuid::Uuid>> = std::collections::HashMap::new();
    for room in &rooms {
        let room_uuid = room.id.as_ref().and_then(uuid_from_room_id);
        let parent_uuid = room.parent_id.as_ref().and_then(uuid_from_room_id);
        if let (Some(rid), Some(pid)) = (room_uuid, parent_uuid) {
            children_map.entry(pid).or_default().push(rid);
        }
    }

    // BFS from root
    let mut result = std::collections::HashSet::new();
    result.insert(root);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root);
    while let Some(current) = queue.pop_front() {
        if let Some(kids) = children_map.get(&current) {
            for &kid in kids {
                result.insert(kid);
                queue.push_back(kid);
            }
        }
    }
    result
}

/// Handle direct message - route to target user and echo back to sender.
async fn handle_direct_message(
    dm: proto::DirectMessage,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    // IMPORTANT-4: Reject DM to self
    if dm.target_user_id == sender.user_id {
        return send_command_result(&sender, "DirectMessage", false, "Cannot send a DM to yourself").await;
    }

    let sender_name = sender.get_username().await;
    info!(
        "DM from {} (id={}) to user_id={}: {}",
        sender_name, sender.user_id, dm.target_user_id, dm.text
    );

    // Validate target exists and resolve the delivery connection + display name.
    // For a participant this resolves to its controller connection.
    let (target_client, target_username) = match state.get_member(dm.target_user_id) {
        Some(member) => {
            let name = member.identity.display_name().await;
            match state.delivery_client(dm.target_user_id) {
                Some(c) => (c, name),
                None => {
                    return send_command_result(&sender, "DirectMessage", false, "Target user not found").await;
                }
            }
        }
        None => {
            return send_command_result(&sender, "DirectMessage", false, "Target user not found").await;
        }
    };

    let message_id = if dm.id.len() == 16 {
        dm.id
    } else {
        uuid::Uuid::new_v4().into_bytes().to_vec()
    };

    let timestamp_ms = if dm.timestamp_ms > 0 { dm.timestamp_ms } else { now_ms() };

    let dm_event = proto::DirectMessageReceived {
        sender_id: sender.user_id,
        sender_name: sender_name.clone(),
        text: dm.text,
        id: message_id,
        timestamp_ms,
        target_user_id: dm.target_user_id,
        target_username,
    };

    let envelope = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ServerEvent(proto::ServerEvent {
            kind: Some(proto::server_event::Kind::DirectMessageReceived(dm_event.clone())),
        })),
    };
    let frame = encode_frame(&envelope);

    // Send only to the target (or the bridge owning a virtual target).
    // The sender's client already adds the DM locally, no echo needed.
    if let Err(e) = target_client.send_frame(&frame).await {
        error!("Failed to send DM to target: {e:?}");
    }

    Ok(())
}

/// Handle join room request.
async fn handle_join_room(
    jr: proto::JoinRoom,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    let new_room_uuid = jr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);

    // Permission check: ENTER on the target room
    if let Err(denied) = acl::check_permission(&state, &sender, new_room_uuid, Permissions::ENTER, &persistence).await {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

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

    state.set_user_room(sender.user_id, new_room_uuid).await;

    // Evaluate SPEAK permission in the new room — auto server-mute if denied
    let speak_perms = acl::evaluate_user_permissions(&state, &sender, new_room_uuid, &persistence).await;
    let speak_denied = !speak_perms.contains(Permissions::SPEAK);
    let manually_muted = sender.manually_server_muted.load(Ordering::Relaxed);
    let should_server_mute = speak_denied || manually_muted;
    let was_server_muted = sender.server_muted.swap(should_server_mute, Ordering::Relaxed);

    if should_server_mute != was_server_muted {
        // Broadcast server-muted status change (both mute and unmute)
        let status = state.get_user_status(sender.user_id).await;
        broadcast_state_update(
            &state,
            proto::state_update::Update::UserStatusChanged(proto::UserStatusChanged {
                user_id: Some(proto::UserId { value: sender.user_id }),
                is_muted: status.is_muted,
                is_deafened: status.is_deafened,
                server_muted: should_server_mute,
                is_elevated: sender.is_superuser.load(Ordering::Relaxed),
            }),
        )
        .await?;
    }

    // Save last room for registered users
    if let Some(public_key) = state.get_user_public_key(sender.user_id)
        && persistence.is_registered(&public_key)
        && let Err(e) = persistence.update_user_last_room(&public_key, Some(new_room_uuid.into_bytes()))
    {
        warn!("Failed to save user's last room: {e}");
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
    persistence: Arc<Persistence>,
) -> Result<()> {
    info!("CreateRoom: {}", cr.name);

    // Extract parent UUID if provided
    let parent_uuid = cr.parent_id.as_ref().and_then(uuid_from_room_id);

    // Permission check: MAKE_ROOM on parent room
    let check_room = parent_uuid.unwrap_or(ROOT_ROOM_UUID);
    if let Err(denied) = acl::check_permission(&state, &sender, check_room, Permissions::MAKE_ROOM, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    let room_name = cr.name.clone();
    match crate::ops::apply_create_room(&state, &persistence, cr.name, parent_uuid, cr.description).await {
        Ok(_uuid) => {
            send_command_result(&sender, "CreateRoom", true, &format!("Created room '{}'", room_name)).await?;
        }
        Err(msg) => {
            send_command_result(&sender, "CreateRoom", false, &msg).await?;
        }
    }
    Ok(())
}

/// Handle delete room request.
async fn handle_delete_room(
    dr: proto::DeleteRoom,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    let room_uuid = dr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);

    // Permission check: MODIFY_ROOM on the room being deleted
    if let Err(denied) = acl::check_permission(&state, &sender, room_uuid, Permissions::MODIFY_ROOM, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    match crate::ops::apply_delete_room(&state, &persistence, room_uuid).await {
        Ok(room_name) => {
            send_command_result(&sender, "DeleteRoom", true, &format!("Deleted room '{}'", room_name)).await?;
        }
        Err(msg) => {
            send_command_result(&sender, "DeleteRoom", false, &msg).await?;
        }
    }
    Ok(())
}

/// Handle rename room request.
async fn handle_rename_room(
    rr: proto::RenameRoom,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    let room_uuid = rr
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);

    // Permission check: MODIFY_ROOM on the room being renamed
    if let Err(denied) = acl::check_permission(&state, &sender, room_uuid, Permissions::MODIFY_ROOM, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

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
    {
        let persist = &persistence;
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
    persistence: Arc<Persistence>,
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

    // Permission check: MODIFY_ROOM on the room being moved
    if let Err(denied) = acl::check_permission(&state, &sender, room_uuid, Permissions::MODIFY_ROOM, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }
    // Permission check: MAKE_ROOM on the new parent
    if let Err(denied) =
        acl::check_permission(&state, &sender, new_parent_uuid, Permissions::MAKE_ROOM, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

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

    // Reject moves that would create a parent cycle (a room becoming its own
    // ancestor). A cycle makes the parent-walk in ACL evaluation and descendant
    // collection loop forever, and it gets persisted — bricking the server.
    if new_parent_uuid == room_uuid {
        return send_command_result(&sender, "MoveRoom", false, "Cannot move a room into itself").await;
    }
    {
        let rooms = state.get_rooms().await;
        let parent_of = |uuid: Uuid| -> Option<Uuid> {
            rooms
                .iter()
                .find(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(uuid))
                .and_then(|r| r.parent_id.as_ref().and_then(uuid_from_room_id))
        };
        // Walk up from the new parent; if we reach the room being moved, the
        // move would close a cycle. Bound the walk by room count as a guard
        // against any pre-existing cycle in the stored tree.
        let mut cursor = Some(new_parent_uuid);
        let mut steps = 0;
        while let Some(c) = cursor {
            if c == room_uuid {
                return send_command_result(&sender, "MoveRoom", false, "Cannot move a room into its own descendant")
                    .await;
            }
            steps += 1;
            if steps > rooms.len() {
                break;
            }
            cursor = parent_of(c);
        }
    }

    info!("MoveRoom: {} -> parent {}", room_uuid, new_parent_uuid);

    let moved = state.move_room(room_uuid, new_parent_uuid).await;

    if !moved {
        return send_command_result(&sender, "MoveRoom", false, "Room not found").await;
    }

    // Persist the room move
    {
        let persist = &persistence;
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

/// Handle set room description request.
async fn handle_set_room_description(
    srd: proto::SetRoomDescription,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    let room_uuid = srd
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);

    // Permission check: MODIFY_ROOM on the room
    if let Err(denied) = acl::check_permission(&state, &sender, room_uuid, Permissions::MODIFY_ROOM, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    info!("SetRoomDescription: {}", room_uuid);

    let updated = state.set_room_description(room_uuid, srd.description.clone()).await;

    if !updated {
        return send_command_result(&sender, "SetRoomDescription", false, "Room not found").await;
    }

    // Persist the description change
    {
        let persist = &persistence;
        let room_uuid_bytes = room_uuid.into_bytes();
        if let Some(mut room) = persist.get_room(&room_uuid_bytes) {
            room.description = srd.description.clone();
            if let Err(e) = persist.save_room(&room_uuid_bytes, &room) {
                warn!("Failed to persist room description: {e}");
            }
        }
    }

    // Broadcast incremental update to all clients
    broadcast_state_update(
        &state,
        proto::state_update::Update::RoomDescriptionChanged(proto::RoomDescriptionChanged {
            room_id: Some(room_id_from_uuid(room_uuid)),
            description: srd.description.clone(),
        }),
    )
    .await?;

    send_command_result(&sender, "SetRoomDescription", true, "Description updated").await?;
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
    persistence: Arc<Persistence>,
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
    send_server_state_to_client(&sender, &state, &persistence).await?;
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

    // Update the user's status in state (preserve existing server_muted/is_elevated)
    let existing = state.get_user_status(sender.user_id).await;
    let status = UserStatus {
        is_muted: sus.is_muted,
        is_deafened: sus.is_deafened,
        server_muted: existing.server_muted,
        is_elevated: existing.is_elevated,
    };
    state.set_user_status(sender.user_id, status).await;

    // Broadcast the status change to all clients (preserving server_muted/is_elevated)
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserStatusChanged(proto::UserStatusChanged {
            user_id: Some(proto::UserId { value: sender.user_id }),
            is_muted: sus.is_muted,
            is_deafened: sus.is_deafened,
            server_muted: sender.server_muted.load(Ordering::Relaxed),
            is_elevated: sender.is_superuser.load(Ordering::Relaxed),
        }),
    )
    .await?;
    Ok(())
}

/// Send current server state to a single client.
async fn send_server_state_to_client(
    client: &ClientHandle,
    state: &ServerState,
    persistence: &Arc<Persistence>,
) -> Result<()> {
    let mut rooms = state.build_room_list(persistence).await;
    let users = state.build_user_list().await;

    // Build group definitions for client-side ACL evaluation
    let groups: Vec<proto::GroupInfo> = {
        let builtin = ["default", "admin"];
        persistence
            .list_groups()
            .into_iter()
            .map(|(name, pg)| proto::GroupInfo {
                is_builtin: builtin.contains(&name.as_str()),
                name,
                permissions: pg.permissions,
            })
            .collect()
    };

    // Build the ServerState message (without per-client effective_permissions for hash)
    let server_state_for_hash = ProtoServerState {
        rooms: rooms.clone(),
        users: users.clone(),
        groups: groups.clone(),
        slash_commands: vec![], // excluded from the hash anyway
    };

    // Compute the state hash BEFORE setting per-client effective_permissions
    let state_hash = compute_server_state_hash(&server_state_for_hash);

    // Now compute per-room effective permissions for this specific client
    for room in &mut rooms {
        if let Some(room_uuid) = room.id.as_ref().and_then(uuid_from_room_id) {
            let perms = acl::evaluate_user_permissions(state, client, room_uuid, persistence).await;
            room.effective_permissions = perms.bits();
        }
    }

    // Build the actual message with effective_permissions set. Slash commands
    // ride along so clients can discover them for autocomplete.
    let server_state = ProtoServerState {
        rooms,
        users,
        groups,
        slash_commands: state.slash_commands(),
    };

    let env = proto::Envelope {
        state_hash,
        payload: Some(Payload::ServerEvent(proto::ServerEvent {
            kind: Some(proto::server_event::Kind::ServerState(server_state)),
        })),
    };
    let frame = encode_frame(&env);

    info!("server: sending initial ServerState with state_hash and per-room permissions");
    client.send_frame(&frame).await?;

    Ok(())
}

/// Broadcast current server state to all connected clients.
///
/// Unlike `send_server_state_to_client`, this sends to every client.
/// Each client receives per-room effective permissions computed for them.
pub async fn broadcast_server_state(state: &Arc<ServerState>, persistence: &Arc<Persistence>) -> Result<()> {
    // Send per-client state (with per-room effective permissions) to each client
    let clients = state.snapshot_clients();
    for client in clients {
        if let Err(e) = send_server_state_to_client(&client, state, persistence).await {
            debug!(user_id = client.user_id, "failed to send server state: {e:?}");
        }
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
    let server_state = ProtoServerState {
        rooms,
        users,
        groups: vec![],
        slash_commands: vec![],
    };
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

/// Clean up a client: remove from the member table, remove memberships, broadcast update.
///
/// If the client is a controller, also cleans up all participants it owned.
pub async fn cleanup_client(client_handle: &Arc<ClientHandle>, state: &Arc<ServerState>) {
    let user_id = client_handle.user_id;
    let is_controller = client_handle.is_controller.load(Ordering::SeqCst);

    // If this is a controller, clean up all participants it drove.
    if is_controller {
        let participant_ids = state.members_by_owner(crate::state::OwnerId::Connection(user_id));
        for vu_id in participant_ids {
            state.remove_client(vu_id);
            state.remove_user_membership(vu_id).await;
            if let Err(e) = broadcast_state_update(
                state,
                proto::state_update::Update::UserLeft(proto::UserLeft {
                    user_id: Some(proto::UserId { value: vu_id }),
                }),
            )
            .await
            {
                error!(participant_id = vu_id, "failed to broadcast participant leave: {e:?}");
            }
            debug!(
                controller_id = user_id,
                participant_id = vu_id,
                "cleaned up participant"
            );
        }
    }

    // Remove client from the member table (lock-free)
    state.remove_client_by_handle(client_handle);
    // Remove membership
    state.remove_user_membership(user_id).await;
    // Remove session
    state.remove_session(user_id);
    // Remove voice rate limit state
    state.remove_voice_rate(user_id);

    debug!(user_id, "server: cleaned up client");

    // Only broadcast if the client was authenticated and not a controller
    // (controller's own user was already removed from visible state in ControllerHello)
    if client_handle.authenticated.load(Ordering::SeqCst) && !is_controller {
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

/// The connection-independent result of routing an inbound voice datagram.
///
/// Holds the server-authoritative re-encoded bytes plus the roster members that
/// should receive them (the sender already excluded). Delivery-connection
/// resolution and dedup are applied separately by [`dedup_delivery_targets`].
#[derive(Debug, Clone)]
pub struct RelayPacket {
    /// Re-encoded datagram with server-stamped `sender_id` and `room_id`.
    pub bytes: Vec<u8>,
    /// Member user_ids in the sender's room, excluding the sender. These are
    /// roster member ids, *not* yet resolved to delivery connections.
    pub recipients: Vec<u64>,
    /// The room the frame was sent in, used to fan out to in-process audio
    /// subscribers (plugins) alongside the QUIC recipients.
    pub room: Uuid,
    /// The frame in decoded form for in-process delivery via
    /// [`ServerState::fanout_audio`]. Carries the server-authoritative sender.
    pub frame: VoiceFrame,
}

/// Route an inbound voice datagram: validate the sender (incl. controller
/// sender-id spoofing checks), apply the server-mute / self-mute / rate-limit
/// drops, locate the sender's room, stamp the server-authoritative
/// `sender_id`/`room_id`, re-encode, and gather the room's other members.
///
/// This is the connection-independent core of [`handle_datagrams`]: it touches
/// only `state`, never a `quinn::Connection`, so it is unit-testable with
/// participant fixtures and serves as the relay benchmark seam. The caller maps
/// the returned `recipients` to delivery connections via
/// [`dedup_delivery_targets`] and performs the actual sends.
///
/// Returns `None` when the datagram must be dropped: an invalid
/// controller-claimed sender, a server-muted or self-muted sender, an exceeded
/// rate limit, or a sender not currently in any room.
pub async fn build_relay_packet(
    state: &ServerState,
    sender_user_id: u64,
    mut voice_dgram: VoiceDatagram,
    datagram_len: usize,
) -> Option<RelayPacket> {
    // Determine the effective sender. A controller connection may speak on
    // behalf of one of its participants; a normal client always uses its own id.
    let is_controller = state
        .get_client(sender_user_id)
        .is_some_and(|c| c.is_controller.load(Ordering::SeqCst));

    let effective_sender = if is_controller {
        match voice_dgram.sender_id {
            Some(claimed_id) if state.is_participant_of(claimed_id, OwnerId::Connection(sender_user_id)) => claimed_id,
            _ => {
                debug!(
                    controller_id = sender_user_id,
                    claimed_sender = ?voice_dgram.sender_id,
                    "server: controller datagram with invalid sender_id, dropping"
                );
                return None;
            }
        }
    } else {
        sender_user_id
    };

    // Server-muted senders are dropped silently.
    if !is_controller
        && let Some(client) = state.get_client(sender_user_id)
        && client.server_muted.load(Ordering::Relaxed)
    {
        return None;
    }

    // Self-muted senders are dropped silently.
    let status = state.get_user_status(effective_sender).await;
    if status.is_muted {
        debug!(sender = effective_sender, "server: dropping voice from muted user");
        return None;
    }

    // Rate limit by the connection owner so bridge traffic is limited as a
    // whole, not per virtual user.
    if !state.check_voice_rate(sender_user_id, datagram_len) {
        debug!(
            user_id = sender_user_id,
            bytes = datagram_len,
            "server: voice datagram rate limited, dropping"
        );
        return None;
    }
    debug!(
        sender = effective_sender,
        seq = voice_dgram.sequence,
        data_len = voice_dgram.opus_data.len(),
        "server: received voice datagram"
    );

    // Snapshot room memberships to avoid holding locks during relay.
    let room_memberships = state.snapshot_room_memberships().await;

    // Only relay if the sender is actually in a room.
    let Some(actual_room) = room_memberships
        .iter()
        .find_map(|(rid, users)| users.contains(&effective_sender).then_some(*rid))
    else {
        debug!(
            sender = effective_sender,
            "server: sender not in any room, dropping datagram"
        );
        return None;
    };

    // Stamp server-authoritative sender/room, then re-encode.
    voice_dgram.sender_id = Some(effective_sender);
    voice_dgram.room_id = Some(actual_room.as_bytes().to_vec());
    let frame = VoiceFrame {
        sender_id: effective_sender,
        opus_data: voice_dgram.opus_data.clone(),
        sequence: voice_dgram.sequence,
        timestamp_us: voice_dgram.timestamp_us,
        end_of_stream: voice_dgram.end_of_stream,
    };
    let bytes = voice_dgram.encode_to_vec();

    let recipients = room_memberships
        .get(&actual_room)
        .map(|members| members.iter().copied().filter(|&id| id != effective_sender).collect())
        .unwrap_or_default();

    Some(RelayPacket {
        bytes,
        recipients,
        room: actual_room,
        frame,
    })
}

/// Resolve room members to their delivery connections and deduplicate, so a
/// controller driving several participants in the same room receives a datagram
/// only once. `resolve` maps a member id to its delivery-connection id, or
/// `None` if the member has no QUIC delivery target; production passes
/// `|id| state.delivery_client(id).map(|c| c.user_id)`. Order is preserved.
pub fn dedup_delivery_targets(recipients: &[u64], resolve: impl Fn(u64) -> Option<u64>) -> Vec<u64> {
    let mut seen = std::collections::HashSet::new();
    let mut targets = Vec::new();
    for &recipient_id in recipients {
        let Some(delivery_id) = resolve(recipient_id) else {
            continue;
        };
        if seen.insert(delivery_id) {
            targets.push(delivery_id);
        }
    }
    targets
}

/// Handle incoming QUIC datagrams for voice relay.
///
/// Each datagram is routed by [`build_relay_packet`] (the connection-independent
/// core) and then fanned out to the resolved, deduplicated delivery connections.
/// The `sender_user_id` is determined by the connection, not by datagram content.
///
/// # Locking Behavior
///
/// This handler is optimized for the audio path:
/// - Uses a snapshot of room memberships to avoid holding locks during relay
/// - Client iteration is lock-free via DashMap
/// - Datagram sends don't hold any locks
pub async fn handle_datagrams(conn: quinn::Connection, state: Arc<ServerState>, sender_user_id: u64) {
    loop {
        match conn.read_datagram().await {
            Ok(datagram) => {
                let datagram_len = datagram.len();
                match VoiceDatagram::decode(datagram.as_ref()) {
                    Ok(voice_dgram) => {
                        let Some(packet) = build_relay_packet(&state, sender_user_id, voice_dgram, datagram_len).await
                        else {
                            continue;
                        };

                        // Resolve members to delivery connections (a participant's
                        // voice goes through its controller) and dedup.
                        let targets = dedup_delivery_targets(&packet.recipients, |id| {
                            state.delivery_client(id).map(|c| c.user_id)
                        });

                        for delivery_id in targets {
                            let Some(client) = state.get_client(delivery_id) else {
                                continue;
                            };
                            if let Err(e) = client.conn.send_datagram(packet.bytes.clone().into()) {
                                debug!(
                                    user_id = delivery_id,
                                    error = ?e,
                                    "server: failed to relay voice datagram"
                                );
                            }
                        }

                        // Deliver to in-process audio subscribers (plugins) for
                        // this room, alongside the QUIC recipients above.
                        state.fanout_audio(packet.room, &packet.frame);
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

/// Emit a voice frame *as* a participant (a member with no QUIC connection of
/// its own), relaying it into the participant's current room exactly as the
/// datagram path relays a client's voice: to every other room member's delivery
/// connection and to in-process audio subscribers.
///
/// This is the egress counterpart to [`ServerState::subscribe_audio`]; together
/// they let an in-process plugin both hear and speak in a room. The frame's
/// `sender_id` is forced to `sender_id` (the participant) so loopback-skip in
/// [`ServerState::fanout_audio`] keeps a bot from hearing its own output. No-op
/// if the participant is not currently in a room.
pub async fn relay_voice_as(
    state: &Arc<ServerState>,
    sender_id: u64,
    opus_data: Vec<u8>,
    sequence: u32,
    timestamp_us: u64,
    end_of_stream: bool,
) {
    let room_memberships = state.snapshot_room_memberships().await;
    let Some(room) = room_memberships
        .iter()
        .find_map(|(rid, users)| users.contains(&sender_id).then_some(*rid))
    else {
        return;
    };

    let dgram = VoiceDatagram {
        opus_data: opus_data.clone(),
        sequence,
        timestamp_us,
        end_of_stream,
        sender_id: Some(sender_id),
        room_id: Some(room.as_bytes().to_vec()),
    };
    let bytes = dgram.encode_to_vec();

    let recipients: Vec<u64> = room_memberships
        .get(&room)
        .map(|members| members.iter().copied().filter(|&id| id != sender_id).collect())
        .unwrap_or_default();
    let targets = dedup_delivery_targets(&recipients, |id| state.delivery_client(id).map(|c| c.user_id));
    for delivery_id in targets {
        if let Some(client) = state.get_client(delivery_id) {
            let _ = client.conn.send_datagram(bytes.clone().into());
        }
    }

    state.fanout_audio(
        room,
        &VoiceFrame {
            sender_id,
            opus_data,
            sequence,
            timestamp_us,
            end_of_stream,
        },
    );
}

// =============================================================================
// ACL Handlers
// =============================================================================

/// Handle KickUser - kick a user from the server.
async fn handle_kick_user(
    ku: proto::KickUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Permission check: KICK at root
    if let Err(denied) = acl::check_permission(&state, &sender, Uuid::nil(), Permissions::KICK, &persistence).await {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    let target_user_id = ku.target_user_id;

    // Cannot kick yourself (sender-relative; the shared core has no notion of "self")
    if target_user_id == sender.user_id {
        return send_command_result(&sender, "KickUser", false, "Cannot kick yourself").await;
    }

    let kicked_by = sender.get_username().await;
    match crate::ops::apply_kick(&state, target_user_id, &ku.reason, &kicked_by).await {
        Ok(msg) => send_command_result(&sender, "KickUser", true, &msg).await,
        Err(msg) => send_command_result(&sender, "KickUser", false, &msg).await,
    }
}

/// Handle BanUser - ban a user from the server.
///
/// This adds the target user to a "banned" group (creating it if needed with the
/// BANNED permission flag), then kicks them. The BANNED flag is checked at auth
/// time to reject future connections.
async fn handle_ban_user(
    bu: proto::BanUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Permission check: BAN at root
    if let Err(denied) = acl::check_permission(&state, &sender, Uuid::nil(), Permissions::BAN, &persistence).await {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    let target_user_id = bu.target_user_id;

    // Cannot ban yourself (sender-relative; the shared core has no notion of "self")
    if target_user_id == sender.user_id {
        return send_command_result(&sender, "BanUser", false, "Cannot ban yourself").await;
    }

    let banned_by = sender.get_username().await;
    match crate::ops::apply_ban(
        &state,
        &persistence,
        target_user_id,
        bu.duration_seconds,
        &bu.reason,
        &banned_by,
    )
    .await
    {
        Ok(msg) => send_command_result(&sender, "BanUser", true, &msg).await,
        Err(msg) => send_command_result(&sender, "BanUser", false, &msg).await,
    }
}

/// Handle SetServerMute - set server mute on another user.
async fn handle_set_server_mute(
    ssm: proto::SetServerMute,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    let target_user_id = ssm.target_user_id;

    // Find target user's room for permission check
    let target_room = state.get_user_room(target_user_id).await.unwrap_or(ROOT_ROOM_UUID);

    // Permission check: MUTE_DEAFEN in target's room
    if let Err(denied) =
        acl::check_permission(&state, &sender, target_room, Permissions::MUTE_DEAFEN, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    let target_client = match state.get_client(target_user_id) {
        Some(c) => c,
        None => {
            return send_command_result(&sender, "SetServerMute", false, "User not found").await;
        }
    };

    // Set the manual mute flag
    target_client.manually_server_muted.store(ssm.muted, Ordering::Relaxed);

    // When unmuting, re-evaluate SPEAK permission — the user may be in a
    // SPEAK-denied room which should keep them server-muted.
    let effective_muted = if ssm.muted {
        true
    } else {
        let target_room = state.get_user_room(target_user_id).await.unwrap_or(ROOT_ROOM_UUID);
        let perms = acl::evaluate_user_permissions(&state, &target_client, target_room, &persistence).await;

        !perms.contains(Permissions::SPEAK) // manually_muted is false at this point
    };
    target_client.server_muted.store(effective_muted, Ordering::Relaxed);

    let target_username = target_client.get_username().await;
    info!(
        target = %target_username,
        muted = ssm.muted,
        effective_muted = effective_muted,
        "SetServerMute"
    );

    // Broadcast UserStatusChanged
    let status = state.get_user_status(target_user_id).await;
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserStatusChanged(proto::UserStatusChanged {
            user_id: Some(proto::UserId { value: target_user_id }),
            is_muted: status.is_muted,
            is_deafened: status.is_deafened,
            server_muted: effective_muted,
            is_elevated: target_client.is_superuser.load(Ordering::Relaxed),
        }),
    )
    .await?;

    send_command_result(
        &sender,
        "SetServerMute",
        true,
        &format!(
            "{} '{}'",
            if ssm.muted { "Server muted" } else { "Server unmuted" },
            target_username
        ),
    )
    .await
}

/// Handle Elevate - elevate to superuser via sudo password.
async fn handle_elevate(
    elev: proto::Elevate,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Permission check: SUDO at root
    if let Err(denied) = acl::check_permission(&state, &sender, Uuid::nil(), Permissions::SUDO, &persistence).await {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    let persist = &persistence;

    // Load sudo password hash from persistence
    let sudo_hash = match persist.get_raw("sudo_password", b"sudo") {
        Some(hash_bytes) => match String::from_utf8(hash_bytes) {
            Ok(h) => h,
            Err(_) => {
                return send_command_result(&sender, "Elevate", false, "Sudo password not configured").await;
            }
        },
        None => {
            return send_command_result(&sender, "Elevate", false, "Sudo password not configured").await;
        }
    };

    // Verify password (bcrypt)
    match bcrypt::verify(&elev.password, &sudo_hash) {
        Ok(true) => {}
        _ => {
            return send_command_result(&sender, "Elevate", false, "Incorrect password").await;
        }
    }

    // Set superuser flag
    sender.is_superuser.store(true, Ordering::SeqCst);

    let username = sender.get_username().await;
    info!(user = %username, "User elevated to superuser");

    // Broadcast UserStatusChanged with is_elevated=true
    let status = state.get_user_status(sender.user_id).await;
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserStatusChanged(proto::UserStatusChanged {
            user_id: Some(proto::UserId { value: sender.user_id }),
            is_muted: status.is_muted,
            is_deafened: status.is_deafened,
            server_muted: sender.server_muted.load(Ordering::Relaxed),
            is_elevated: true,
        }),
    )
    .await?;

    send_command_result(&sender, "Elevate", true, "Elevated to superuser").await
}

/// Handle CreateGroup - create a new permission group.
async fn handle_create_group(
    cg: proto::CreateGroup,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Permission check: MANAGE_ACL at root
    if let Err(denied) =
        acl::check_permission(&state, &sender, Uuid::nil(), Permissions::MANAGE_ACL, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    match crate::ops::apply_create_group(&state, &persistence, cg.name, cg.permissions).await {
        Ok(msg) => send_command_result(&sender, "CreateGroup", true, &msg).await,
        Err(msg) => send_command_result(&sender, "CreateGroup", false, &msg).await,
    }
}

/// Handle DeleteGroup - delete a permission group.
async fn handle_delete_group(
    dg: proto::DeleteGroup,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Permission check: MANAGE_ACL at root
    if let Err(denied) =
        acl::check_permission(&state, &sender, Uuid::nil(), Permissions::MANAGE_ACL, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    match crate::ops::apply_delete_group(&state, &persistence, dg.name).await {
        Ok(msg) => send_command_result(&sender, "DeleteGroup", true, &msg).await,
        Err(msg) => send_command_result(&sender, "DeleteGroup", false, &msg).await,
    }
}

/// Handle ModifyGroup - modify a permission group.
async fn handle_modify_group(
    mg: proto::ModifyGroup,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Permission check: MANAGE_ACL at root
    if let Err(denied) =
        acl::check_permission(&state, &sender, Uuid::nil(), Permissions::MANAGE_ACL, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    match crate::ops::apply_modify_group(&state, &persistence, mg.name, mg.permissions).await {
        Ok(msg) => send_command_result(&sender, "ModifyGroup", true, &msg).await,
        Err(msg) => send_command_result(&sender, "ModifyGroup", false, &msg).await,
    }
}

/// Handle SetUserGroup - add or remove a user from a group.
async fn handle_set_user_group(
    sug: proto::SetUserGroup,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Permission check: MANAGE_ACL at root
    if let Err(denied) =
        acl::check_permission(&state, &sender, Uuid::nil(), Permissions::MANAGE_ACL, &persistence).await
    {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    match crate::ops::apply_set_user_group(
        &state,
        &persistence,
        sug.target_user_id,
        sug.group,
        sug.add,
        sug.expires_at,
    )
    .await
    {
        Ok(msg) => send_command_result(&sender, "SetUserGroup", true, &msg).await,
        Err(msg) => send_command_result(&sender, "SetUserGroup", false, &msg).await,
    }
}

/// Handle SetRoomAcl - set ACL entries on a room.
async fn handle_set_room_acl(
    sra: proto::SetRoomAcl,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    // Validate room_id — return error instead of silently falling back to root
    let room_uuid = if sra.room_id.len() == 16 {
        match Uuid::from_slice(&sra.room_id) {
            Ok(u) => u,
            Err(_) => {
                return send_command_result(&sender, "SetRoomAcl", false, "Invalid room ID").await;
            }
        }
    } else {
        return send_command_result(&sender, "SetRoomAcl", false, "Missing or invalid room ID").await;
    };

    // Permission check: WRITE on the target room
    if let Err(denied) = acl::check_permission(&state, &sender, room_uuid, Permissions::WRITE, &persistence).await {
        send_permission_denied(&sender, denied).await?;
        return Ok(());
    }

    match crate::ops::apply_set_room_acl(&state, &persistence, room_uuid, sra.inherit_acl, sra.entries).await {
        Ok(msg) => send_command_result(&sender, "SetRoomAcl", true, &msg).await,
        Err(msg) => send_command_result(&sender, "SetRoomAcl", false, &msg).await,
    }
}

// =============================================================================
// Controller / Participant Handlers
// =============================================================================

// --- Reusable participant operations (shared by the controller protocol and
// the in-process plugin capability). Each takes the `owner` / `user_id`
// directly and performs no ownership check — callers authorize first. ---

/// Mint a participant member owned by `owner`, place it in the root room, and
/// broadcast its arrival. Returns the assigned user_id.
///
/// `present` controls roster visibility. A *present* participant is placed in
/// the Root room and announced via `UserJoined`, so it shows in clients' room
/// trees (bridged users, the echo bot once it joins a room). A *hidden*
/// participant gets only a [`Member`] entry — identity, ACL, and the ability to
/// author chat via [`broadcast_chat_in_room`] — but no room membership and no
/// broadcast, so it never appears in the tree (text-only bots like the link
/// cleaner). A hidden participant becomes present the first time it is moved
/// into a room; see [`move_participant_to_room`].
pub(crate) async fn create_participant(
    state: &Arc<ServerState>,
    owner: OwnerId,
    username: String,
    label: Option<String>,
    verified_username: Option<String>,
    groups: Vec<String>,
    present: bool,
) -> Result<u64> {
    let user_id = state.allocate_user_id();
    let identity = Arc::new(Identity::participant(
        username.clone(),
        verified_username,
        label.clone(),
        groups.clone(),
    ));
    state.register_participant(Arc::new(Member {
        user_id,
        identity,
        binding: Binding::Owned { owner },
    }));

    if present {
        state.set_user_room(user_id, ROOT_ROOM_UUID).await;
        broadcast_state_update(
            state,
            proto::state_update::Update::UserJoined(proto::UserJoined {
                user: Some(proto::User {
                    user_id: Some(proto::UserId { value: user_id }),
                    username,
                    current_room: Some(room_id_from_uuid(ROOT_ROOM_UUID)),
                    is_muted: false,
                    is_deafened: false,
                    server_muted: false,
                    is_elevated: false,
                    groups,
                    label,
                }),
            }),
        )
        .await?;
    }

    Ok(user_id)
}

/// Remove a participant member and broadcast its departure.
///
/// `UserLeft` is only broadcast for a participant that was *present* (had a room
/// membership); a hidden participant clients never learned about needs no
/// departure notice.
pub(crate) async fn remove_participant(state: &Arc<ServerState>, user_id: u64) -> Result<()> {
    let was_present = state.get_user_room(user_id).await.is_some();
    state.remove_client(user_id);
    state.remove_user_membership(user_id).await;
    if was_present {
        broadcast_state_update(
            state,
            proto::state_update::Update::UserLeft(proto::UserLeft {
                user_id: Some(proto::UserId { value: user_id }),
            }),
        )
        .await?;
    }
    Ok(())
}

/// Move a participant to a room. Returns false if the room does not exist.
///
/// A participant moving for the first time (a hidden participant with no
/// membership yet) is announced with `UserJoined` so clients learn it exists;
/// an already-present participant emits `UserMoved`.
pub(crate) async fn move_participant_to_room(state: &Arc<ServerState>, user_id: u64, room_uuid: Uuid) -> Result<bool> {
    let rooms = state.get_rooms().await;
    if !rooms
        .iter()
        .any(|r| r.id.as_ref().and_then(uuid_from_room_id) == Some(room_uuid))
    {
        return Ok(false);
    }
    let was_present = state.get_user_room(user_id).await.is_some();
    state.set_user_room(user_id, room_uuid).await;

    if was_present {
        broadcast_state_update(
            state,
            proto::state_update::Update::UserMoved(proto::UserMoved {
                user_id: Some(proto::UserId { value: user_id }),
                to_room_id: Some(room_id_from_uuid(room_uuid)),
            }),
        )
        .await?;
    } else if let Some(member) = state.get_member(user_id) {
        // First appearance: the hidden→present transition. Emit UserJoined so
        // clients render the participant in its new room.
        let status = state.get_user_status(user_id).await;
        broadcast_state_update(
            state,
            proto::state_update::Update::UserJoined(proto::UserJoined {
                user: Some(proto::User {
                    user_id: Some(proto::UserId { value: user_id }),
                    username: member.identity.display_name().await,
                    current_room: Some(room_id_from_uuid(room_uuid)),
                    is_muted: status.is_muted,
                    is_deafened: status.is_deafened,
                    server_muted: status.server_muted,
                    is_elevated: status.is_elevated,
                    groups: member.identity.groups().await,
                    label: member.identity.label.clone(),
                }),
            }),
        )
        .await?;
    }
    Ok(true)
}

/// Update a participant's self-mute/deaf status and broadcast the change.
pub(crate) async fn set_participant_status_core(
    state: &Arc<ServerState>,
    user_id: u64,
    is_muted: bool,
    is_deafened: bool,
) -> Result<()> {
    let status = UserStatus {
        is_muted,
        is_deafened,
        server_muted: false,
        is_elevated: false,
    };
    state.set_user_status(user_id, status).await;
    broadcast_state_update(
        state,
        proto::state_update::Update::UserStatusChanged(proto::UserStatusChanged {
            user_id: Some(proto::UserId { value: user_id }),
            is_muted,
            is_deafened,
            server_muted: false,
            is_elevated: false,
        }),
    )
    .await
}

// --- Controller protocol handlers (owner = the controller connection). ---

/// Handle ControllerHello - register this connection as a controller and hide
/// its user entry. Gated by the MANAGE_PARTICIPANTS permission.
async fn handle_controller_hello(
    ch: proto::ControllerHello,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    if sender.is_controller.load(Ordering::SeqCst) {
        return send_command_result(&sender, "ControllerHello", false, "Already a controller").await;
    }

    // Authority: the connection's identity must hold MANAGE_PARTICIPANTS. This
    // is separate from any group used for the participants it mints.
    if acl::check_permission(
        &state,
        &sender,
        Uuid::nil(),
        Permissions::MANAGE_PARTICIPANTS,
        &persistence,
    )
    .await
    .is_err()
    {
        warn!(
            user_id = sender.user_id,
            "ControllerHello denied: missing MANAGE_PARTICIPANTS"
        );
        return send_command_result(
            &sender,
            "ControllerHello",
            false,
            "Missing MANAGE_PARTICIPANTS permission",
        )
        .await;
    }

    info!(
        user_id = sender.user_id,
        controller_name = %ch.controller_name,
        "ControllerHello: registering controller"
    );
    sender.is_controller.store(true, Ordering::SeqCst);

    // A controller is infrastructure, not a visible user — drop it from the roster.
    state.remove_user_membership(sender.user_id).await;
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserLeft(proto::UserLeft {
            user_id: Some(proto::UserId { value: sender.user_id }),
        }),
    )
    .await?;

    send_command_result(&sender, "ControllerHello", true, "Controller mode activated").await
}

/// Handle RegisterParticipant - mint a participant driven by this controller.
async fn handle_register_participant(
    rp: proto::RegisterParticipant,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    let owner = OwnerId::Connection(sender.user_id);

    // Anonymous participants (external_identity unset) inherit the controller's
    // configured default participant group — looked up by the controller's
    // public key, distinct from the controller's own groups, so they never gain
    // MANAGE_PARTICIPANTS. `external_identity` is reserved for stable
    // per-participant identity and ignored for now (always anonymous).
    let groups: Vec<String> = {
        let pubkey = *sender.public_key.read().await;
        match pubkey {
            Some(pk) => persistence.get_participant_default_group(&pk).into_iter().collect(),
            None => Vec::new(),
        }
    };

    // Bridged users are present in the roster from the moment they register.
    let user_id = create_participant(&state, owner, rp.username.clone(), rp.label.clone(), None, groups, true).await?;

    info!(
        controller_id = sender.user_id,
        participant_id = user_id,
        username = %rp.username,
        "registered participant"
    );

    let reply = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ParticipantRegistered(proto::ParticipantRegistered {
            user_id,
            username: rp.username,
        })),
    };
    let _ = sender.send_frame(&encode_frame(&reply)).await;
    Ok(())
}

/// Handle UnregisterParticipant - remove a participant driven by this controller.
async fn handle_unregister_participant(
    up: proto::UnregisterParticipant,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    if !state.is_participant_of(up.user_id, OwnerId::Connection(sender.user_id)) {
        return send_command_result(&sender, "UnregisterParticipant", false, "Not your participant").await;
    }
    remove_participant(&state, up.user_id).await?;
    send_command_result(&sender, "UnregisterParticipant", true, "Participant removed").await
}

/// Handle MoveParticipant - move a participant to a room.
async fn handle_move_participant(
    mp: proto::MoveParticipant,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    if !state.is_participant_of(mp.user_id, OwnerId::Connection(sender.user_id)) {
        return send_command_result(&sender, "MoveParticipant", false, "Not your participant").await;
    }
    let room_uuid = mp
        .room_id
        .as_ref()
        .and_then(uuid_from_room_id)
        .unwrap_or(ROOT_ROOM_UUID);
    if !move_participant_to_room(&state, mp.user_id, room_uuid).await? {
        return send_command_result(&sender, "MoveParticipant", false, "Room not found").await;
    }
    send_command_result(&sender, "MoveParticipant", true, "Participant moved").await
}

/// Handle SetParticipantStatus - update a participant's mute/deaf status.
async fn handle_set_participant_status(
    sps: proto::SetParticipantStatus,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    if !state.is_participant_of(sps.user_id, OwnerId::Connection(sender.user_id)) {
        return send_command_result(&sender, "SetParticipantStatus", false, "Not your participant").await;
    }
    set_participant_status_core(&state, sps.user_id, sps.is_muted, sps.is_deafened).await
}

/// Handle ParticipantChat - send chat on behalf of a participant.
async fn handle_participant_chat(
    pc: proto::ParticipantChat,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Arc<Persistence>,
) -> Result<()> {
    if !state.is_participant_of(pc.user_id, OwnerId::Connection(sender.user_id)) {
        return send_command_result(&sender, "ParticipantChat", false, "Not your participant").await;
    }
    let id = uuid::Uuid::new_v4().into_bytes().to_vec();
    broadcast_chat_as(&state, &persistence, pc.user_id, pc.text, false, id, now_ms(), None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A controller-driven participant needs no `quinn::Connection`, so the
    /// whole routing core ([`build_relay_packet`]) is exercisable in a unit
    /// test. Delivery resolution (which *does* need a live connection) is
    /// covered separately through [`dedup_delivery_targets`]'s injected
    /// resolver.
    fn participant(uid: u64) -> Arc<Member> {
        Arc::new(Member {
            user_id: uid,
            identity: Arc::new(Identity::participant(format!("user{uid}"), None, None, vec![])),
            binding: Binding::Owned {
                owner: OwnerId::Plugin(1),
            },
        })
    }

    /// A client-sent datagram. `sender_id`/`room_id` are left unset (or spoofed
    /// in the stamping test) — the server is supposed to overwrite them.
    fn client_dgram(seq: u32) -> VoiceDatagram {
        VoiceDatagram {
            opus_data: vec![0xAA; 40],
            sequence: seq,
            ..Default::default()
        }
    }

    // ---- parse_slash_command -----------------------------------------------

    #[test]
    fn slash_command_parses_name_and_args() {
        assert_eq!(parse_slash_command("/echo stop"), Some(("echo".to_string(), "stop")));
        assert_eq!(parse_slash_command("/echo"), Some(("echo".to_string(), "")));
        // Leading whitespace is tolerated; the command is lowercased; args trimmed.
        assert_eq!(
            parse_slash_command("  /ECHO   hello world  "),
            Some(("echo".to_string(), "hello world"))
        );
    }

    #[test]
    fn slash_command_rejects_non_commands() {
        assert_eq!(parse_slash_command("hello"), None);
        assert_eq!(parse_slash_command(""), None);
        assert_eq!(parse_slash_command("/"), None);
        assert_eq!(parse_slash_command("/   "), None);
        // A URL in chat is not a command (it has no leading slash).
        assert_eq!(parse_slash_command("https://example.com"), None);
    }

    // ---- audio subscription fan-out ----------------------------------------

    /// A relayed frame reaches a room's audio subscriber, but never the sink
    /// owned by the frame's own sender (loopback-skip), and never a sink for a
    /// different room. Mirrors how `handle_datagrams` fans `build_relay_packet`
    /// output out to plugins.
    #[tokio::test]
    async fn fanout_delivers_to_room_subscribers_and_skips_self() {
        let state = ServerState::new();
        let room_a = state.create_room("A".to_string()).await;
        let room_b = state.create_room("B".to_string()).await;

        // Listener (owner 2) subscribes to A; speaker is owner 1; a B subscriber
        // (owner 9) must not hear A's audio.
        let (tx_listener, mut rx_listener) = tokio::sync::mpsc::channel(8);
        let (tx_self, mut rx_self) = tokio::sync::mpsc::channel(8);
        let (tx_other_room, mut rx_other_room) = tokio::sync::mpsc::channel(8);
        state.subscribe_audio(room_a, 2, tx_listener);
        state.subscribe_audio(room_a, 1, tx_self); // owned by the sender → skipped
        state.subscribe_audio(room_b, 9, tx_other_room);

        let frame = VoiceFrame {
            sender_id: 1,
            opus_data: vec![0xAB; 8],
            sequence: 42,
            timestamp_us: 0,
            end_of_stream: false,
        };
        state.fanout_audio(room_a, &frame);

        let got = rx_listener.try_recv().expect("listener should receive the frame");
        assert_eq!(got.sender_id, 1);
        assert_eq!(got.sequence, 42);
        assert!(rx_self.try_recv().is_err(), "sender must not hear its own frame");
        assert!(rx_other_room.try_recv().is_err(), "other-room sink must not hear it");
    }

    /// Dropping a subscription (here simulated via explicit unsubscribe) stops
    /// further delivery — the RAII guard relies on this.
    #[tokio::test]
    async fn unsubscribe_stops_delivery() {
        let state = ServerState::new();
        let room = state.create_room("R".to_string()).await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let sub_id = state.subscribe_audio(room, 2, tx);

        state.unsubscribe_audio(room, sub_id);
        state.fanout_audio(
            room,
            &VoiceFrame {
                sender_id: 1,
                opus_data: vec![],
                sequence: 0,
                timestamp_us: 0,
                end_of_stream: true,
            },
        );
        assert!(rx.try_recv().is_err(), "no frame after unsubscribe");
    }

    // ---- participant presence ----------------------------------------------

    /// A hidden participant (the link-cleaner case) gets a roster member but no
    /// room presence, and only gains one — its first appearance — when moved.
    #[tokio::test]
    async fn hidden_participant_has_no_presence_until_moved() {
        let state = Arc::new(ServerState::new());
        let room = state.create_room("R".to_string()).await;

        let id = create_participant(
            &state,
            OwnerId::Plugin(1),
            "bot".into(),
            Some("bot".into()),
            None,
            vec![],
            false,
        )
        .await
        .unwrap();
        assert!(state.get_member(id).is_some(), "hidden participant still has a member");
        assert!(
            state.get_user_room(id).await.is_none(),
            "hidden participant has no room presence"
        );

        assert!(move_participant_to_room(&state, id, room).await.unwrap());
        assert_eq!(
            state.get_user_room(id).await,
            Some(room),
            "moving makes the hidden participant present"
        );
    }

    /// A present participant (the bridge case) starts visible in Root.
    #[tokio::test]
    async fn present_participant_starts_in_root() {
        let state = Arc::new(ServerState::new());
        let id = create_participant(
            &state,
            OwnerId::Connection(1),
            "bridged".into(),
            Some("Mumble".into()),
            None,
            vec![],
            true,
        )
        .await
        .unwrap();
        assert_eq!(state.get_user_room(id).await, Some(ROOT_ROOM_UUID));
    }

    // ---- build_relay_packet: routing ---------------------------------------

    /// The recipient set is exactly the *other* members of the sender's room:
    /// the sender is excluded and members of unrelated rooms never appear. This
    /// pins the core relay routing contract independent of how the snapshot is
    /// structured.
    #[tokio::test]
    async fn relay_targets_room_peers_and_excludes_sender_and_other_rooms() {
        let state = ServerState::new();
        let room_a = state.create_room("A".to_string()).await;
        let room_b = state.create_room("B".to_string()).await;

        for uid in [1, 2, 3, 4] {
            state.register_participant(participant(uid));
        }
        // 1 (sender), 2, 3 in room A; 4 in room B.
        state.set_user_room(1, room_a).await;
        state.set_user_room(2, room_a).await;
        state.set_user_room(3, room_a).await;
        state.set_user_room(4, room_b).await;

        let packet = build_relay_packet(&state, 1, client_dgram(7), 40)
            .await
            .expect("datagram from an in-room sender should relay");

        let got: HashSet<u64> = packet.recipients.iter().copied().collect();
        assert_eq!(got, HashSet::from([2, 3]), "recipients must be room-A peers only");
    }

    /// The server overwrites the client-supplied `sender_id`/`room_id` with the
    /// authenticated sender and its real room — a client cannot spoof identity
    /// or inject audio into another room.
    #[tokio::test]
    async fn relay_stamps_server_authoritative_sender_and_room() {
        let state = ServerState::new();
        let room_a = state.create_room("A".to_string()).await;

        state.register_participant(participant(1));
        state.register_participant(participant(2));
        state.set_user_room(1, room_a).await;
        state.set_user_room(2, room_a).await;

        // Client lies about who it is and which room it's in.
        let mut spoofed = client_dgram(1);
        spoofed.sender_id = Some(99_999);
        spoofed.room_id = Some(Uuid::new_v4().as_bytes().to_vec());

        let packet = build_relay_packet(&state, 1, spoofed, 40).await.unwrap();
        let relayed = VoiceDatagram::decode(packet.bytes.as_slice()).unwrap();

        assert_eq!(relayed.sender_id, Some(1), "sender_id must be the authenticated sender");
        assert_eq!(
            relayed.room_id.as_deref(),
            Some(room_a.as_bytes().as_slice()),
            "room_id must be the sender's actual room"
        );
    }

    /// A self-muted sender's voice is dropped.
    #[tokio::test]
    async fn relay_drops_self_muted_sender() {
        let state = ServerState::new();
        let room_a = state.create_room("A".to_string()).await;
        state.register_participant(participant(1));
        state.set_user_room(1, room_a).await;
        state
            .set_user_status(
                1,
                UserStatus {
                    is_muted: true,
                    ..Default::default()
                },
            )
            .await;

        assert!(build_relay_packet(&state, 1, client_dgram(1), 40).await.is_none());
    }

    /// A sender that belongs to no room produces nothing to relay.
    #[tokio::test]
    async fn relay_drops_sender_not_in_a_room() {
        let state = ServerState::new();
        state.register_participant(participant(1)); // registered but never placed in a room

        assert!(build_relay_packet(&state, 1, client_dgram(1), 40).await.is_none());
    }

    /// A datagram that exceeds the per-second byte budget on its own is dropped.
    #[tokio::test]
    async fn relay_drops_when_rate_limited() {
        let state = ServerState::new();
        let room_a = state.create_room("A".to_string()).await;
        state.register_participant(participant(1));
        state.set_user_room(1, room_a).await;

        // 40 KB in one datagram is over the 32 KB/s window.
        assert!(build_relay_packet(&state, 1, client_dgram(1), 40_000).await.is_none());
    }

    // ---- dedup_delivery_targets --------------------------------------------

    /// Several participants delivering through the same controller connection
    /// collapse to a single send.
    #[test]
    fn dedup_collapses_participants_sharing_a_controller() {
        // Members 10 and 11 deliver via controller 100; member 12 via 101.
        let resolve = |id: u64| match id {
            10 | 11 => Some(100),
            12 => Some(101),
            _ => None,
        };
        let targets = dedup_delivery_targets(&[10, 11, 12], resolve);
        assert_eq!(targets, vec![100, 101]);
    }

    /// Undeliverable members (no QUIC target) are skipped and source order is
    /// otherwise preserved.
    #[test]
    fn dedup_preserves_order_and_skips_undeliverable() {
        let resolve = |id: u64| match id {
            20 => Some(20),
            21 => None, // e.g. a plugin-owned participant
            22 => Some(22),
            _ => None,
        };
        let targets = dedup_delivery_targets(&[22, 21, 20], resolve);
        assert_eq!(targets, vec![22, 20]);
    }

    #[test]
    fn dedup_of_empty_recipients_is_empty() {
        assert!(dedup_delivery_targets(&[], |_| Some(1)).is_empty());
    }
}
