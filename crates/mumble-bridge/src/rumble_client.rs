use anyhow::{Result, bail};
use api::{
    build_auth_payload, build_session_cert_payload, compute_cert_hash,
    proto::{self, envelope::Payload},
};
use ed25519_dalek::{Signer, SigningKey};
use prost::Message;
use rumble_client::{
    auth::{send_envelope, wait_for_auth_result, wait_for_server_hello},
    transport::{TlsConfig, Transport},
};
use rumble_native::QuinnTransport;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Get current time as milliseconds since UNIX epoch, with a safe fallback.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// An active connection to a Rumble server.
pub struct RumbleConnection {
    pub transport: QuinnTransport,
    pub user_id: u64,
    pub rooms: Vec<proto::RoomInfo>,
    pub users: Vec<proto::User>,
    pub groups: Vec<proto::GroupInfo>,
}

/// Connect to a Rumble server and perform the full auth handshake.
///
/// Returns a `RumbleConnection` with the initial server state.
pub async fn connect(addr: &str, username: &str, signing_key: &SigningKey) -> Result<RumbleConnection> {
    info!(server_addr = %addr, username, "Connecting to Rumble server");

    // Use Transport trait for QUIC connection with accept-all cert verification (bridge use case)
    let tls_config = TlsConfig {
        accept_invalid_certs: true,
        additional_ca_certs: Vec::new(),
        accepted_fingerprints: Vec::new(),
        captured_cert: None,
    };

    let mut transport = QuinnTransport::connect(addr, tls_config).await?;
    info!("Connected to Rumble server via Transport");

    let public_key = signing_key.verifying_key().to_bytes();

    // Send ClientHello
    let hello = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ClientHello(proto::ClientHello {
            username: username.to_string(),
            public_key: public_key.to_vec(),
            password: None,
        })),
    };
    send_envelope(&mut transport, &hello).await?;
    debug!("Sent ClientHello");

    // Wait for ServerHello
    let (nonce, user_id) = wait_for_server_hello(&mut transport).await?;
    info!(user_id, "Received ServerHello");

    // Compute server cert hash from transport
    let server_cert_hash = if let Some(cert_der) = transport.peer_certificate_der() {
        compute_cert_hash(&cert_der)
    } else {
        warn!("Could not get server certificate for hash computation");
        [0u8; 32]
    };

    // Generate session keypair and certificate
    let timestamp_ms = now_ms();
    let expires_ms = timestamp_ms + 24 * 60 * 60 * 1000; // 24h validity
    let session_secret: [u8; 32] = rand::random();
    let session_signing = SigningKey::from_bytes(&session_secret);
    let session_public_bytes: [u8; 32] = session_signing.verifying_key().to_bytes();

    let cert_payload = build_session_cert_payload(&session_public_bytes, timestamp_ms, expires_ms, Some(username));
    let session_signature = signing_key.sign(&cert_payload);

    // Sign auth payload
    let payload = build_auth_payload(&nonce, timestamp_ms, &public_key, user_id, &server_cert_hash);
    let signature = signing_key.sign(&payload);

    // Send Authenticate with session certificate
    let auth = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::Authenticate(proto::Authenticate {
            signature: signature.to_bytes().to_vec(),
            timestamp_ms,
            session_cert: Some(proto::SessionCertificate {
                session_public_key: session_public_bytes.to_vec(),
                issued_ms: timestamp_ms,
                expires_ms,
                device: Some(username.to_string()),
                user_signature: session_signature.to_bytes().to_vec(),
            }),
        })),
    };
    send_envelope(&mut transport, &auth).await?;
    debug!("Sent Authenticate");

    // Wait for ServerState
    let (rooms, users, groups) = wait_for_auth_result(&mut transport).await?;
    info!(
        rooms = rooms.len(),
        users = users.len(),
        groups = groups.len(),
        "Auth complete, received state"
    );

    Ok(RumbleConnection {
        transport,
        user_id,
        rooms,
        users,
        groups,
    })
}

/// Send a chat message to the Rumble server.
pub async fn send_chat(transport: &mut QuinnTransport, sender: &str, text: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ChatMessage(proto::ChatMessage {
            id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            timestamp_ms: now_ms(),
            sender: sender.to_string(),
            text: text.to_string(),
            tree: None,
        })),
    };
    send_envelope(transport, &msg).await
}

/// Send a direct message to a specific Rumble user.
pub async fn send_direct_message(transport: &mut QuinnTransport, target_user_id: u64, text: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::DirectMessage(proto::DirectMessage {
            target_user_id,
            text: text.to_string(),
            id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            timestamp_ms: now_ms(),
        })),
    };
    send_envelope(transport, &msg).await
}

/// Send a tree chat message (broadcast to room and all descendants).
pub async fn send_tree_chat(transport: &mut QuinnTransport, sender: &str, text: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ChatMessage(proto::ChatMessage {
            id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            timestamp_ms: now_ms(),
            sender: sender.to_string(),
            text: text.to_string(),
            tree: Some(true),
        })),
    };
    send_envelope(transport, &msg).await
}

/// Send BridgeHello to declare this connection as a bridge.
pub async fn send_bridge_hello(transport: &mut QuinnTransport, bridge_name: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeHello(proto::BridgeHello {
            bridge_name: bridge_name.to_string(),
        })),
    };
    send_envelope(transport, &msg).await
}

/// Register a virtual user managed by the bridge.
pub async fn send_bridge_register_user(transport: &mut QuinnTransport, username: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeRegisterUser(proto::BridgeRegisterUser {
            username: username.to_string(),
        })),
    };
    send_envelope(transport, &msg).await
}

/// Unregister a virtual user managed by the bridge.
pub async fn send_bridge_unregister_user(transport: &mut QuinnTransport, user_id: u64) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeUnregisterUser(proto::BridgeUnregisterUser { user_id })),
    };
    send_envelope(transport, &msg).await
}

/// Move a virtual user to a room.
pub async fn send_bridge_join_room(transport: &mut QuinnTransport, user_id: u64, room_id: proto::RoomId) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeJoinRoom(proto::BridgeJoinRoom {
            user_id,
            room_id: Some(room_id),
        })),
    };
    send_envelope(transport, &msg).await
}

/// Update a virtual user's mute/deaf status.
pub async fn send_bridge_set_user_status(
    transport: &mut QuinnTransport,
    user_id: u64,
    is_muted: bool,
    is_deafened: bool,
) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeSetUserStatus(proto::BridgeSetUserStatus {
            user_id,
            is_muted,
            is_deafened,
        })),
    };
    send_envelope(transport, &msg).await
}

/// Send a chat message on behalf of a virtual user.
pub async fn send_bridge_chat_message(transport: &mut QuinnTransport, user_id: u64, text: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeChatMessage(proto::BridgeChatMessage {
            user_id,
            text: text.to_string(),
        })),
    };
    send_envelope(transport, &msg).await
}

/// Read the next Rumble envelope from the transport's receive stream.
pub async fn read_envelope(transport: &mut QuinnTransport) -> Result<Option<proto::Envelope>> {
    match transport.recv().await? {
        Some(frame) => {
            let env = proto::Envelope::decode(&*frame)?;
            Ok(Some(env))
        }
        None => bail!("Rumble server closed connection"),
    }
}
