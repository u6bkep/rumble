use std::sync::Arc;

use anyhow::{Result, bail};
use api::{
    build_auth_payload, build_session_cert_payload, compute_cert_hash, encode_frame,
    proto::{self, envelope::Payload},
    try_decode_frame,
};
use bytes::BytesMut;
use ed25519_dalek::{Signer, SigningKey};
use prost::Message;
use quinn::Endpoint;
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
    pub conn: quinn::Connection,
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
    pub buf: BytesMut,
    pub user_id: u64,
    pub rooms: Vec<proto::RoomInfo>,
    pub users: Vec<proto::User>,
}

/// Connect to a Rumble server and perform the full auth handshake.
///
/// Returns a `RumbleConnection` with the initial server state.
pub async fn connect(addr: &str, username: &str, signing_key: &SigningKey) -> Result<RumbleConnection> {
    info!(server_addr = %addr, username, "Connecting to Rumble server");

    let socket_addr: std::net::SocketAddr = addr
        .parse()
        .or_else(|_| format!("{}:5000", addr).parse())
        .map_err(|e| anyhow::anyhow!("Invalid address '{}': {}", addr, e))?;

    // Create QUIC endpoint with danger_accept_invalid_certs for bridge use
    let endpoint = make_bridge_endpoint(socket_addr)?;

    let conn = endpoint.connect(socket_addr, "localhost")?.await?;
    info!(remote = %conn.remote_address(), "QUIC connected");

    let (mut send, mut recv) = conn.open_bi().await?;
    debug!("Opened bi-directional stream");

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
    send.write_all(&encode_frame(&hello)).await?;
    debug!("Sent ClientHello");

    // Wait for ServerHello
    let mut buf = BytesMut::new();
    let (nonce, user_id) = wait_for_server_hello(&mut recv, &mut buf).await?;
    info!(user_id, "Received ServerHello");

    // Compute server cert hash
    let server_cert_hash = compute_server_cert_hash(&conn);

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
    send.write_all(&encode_frame(&auth)).await?;
    debug!("Sent Authenticate");

    // Wait for ServerState
    let (rooms, users) = wait_for_auth_result(&mut recv, &mut buf).await?;
    info!(
        rooms = rooms.len(),
        users = users.len(),
        "Auth complete, received state"
    );

    Ok(RumbleConnection {
        conn,
        send,
        recv,
        buf,
        user_id,
        rooms,
        users,
    })
}

/// Send a chat message to the Rumble server.
pub async fn send_chat(send: &mut quinn::SendStream, sender: &str, text: &str) -> Result<()> {
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
    send.write_all(&encode_frame(&msg)).await?;
    Ok(())
}

/// Send a direct message to a specific Rumble user.
pub async fn send_direct_message(send: &mut quinn::SendStream, target_user_id: u64, text: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::DirectMessage(proto::DirectMessage {
            target_user_id,
            text: text.to_string(),
            id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            timestamp_ms: now_ms(),
        })),
    };
    send.write_all(&encode_frame(&msg)).await?;
    Ok(())
}

/// Send a tree chat message (broadcast to room and all descendants).
pub async fn send_tree_chat(send: &mut quinn::SendStream, sender: &str, text: &str) -> Result<()> {
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
    send.write_all(&encode_frame(&msg)).await?;
    Ok(())
}

/// Send BridgeHello to declare this connection as a bridge.
pub async fn send_bridge_hello(send: &mut quinn::SendStream, bridge_name: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeHello(proto::BridgeHello {
            bridge_name: bridge_name.to_string(),
        })),
    };
    send.write_all(&encode_frame(&msg)).await?;
    Ok(())
}

/// Register a virtual user managed by the bridge.
pub async fn send_bridge_register_user(send: &mut quinn::SendStream, username: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeRegisterUser(proto::BridgeRegisterUser {
            username: username.to_string(),
        })),
    };
    send.write_all(&encode_frame(&msg)).await?;
    Ok(())
}

/// Unregister a virtual user managed by the bridge.
pub async fn send_bridge_unregister_user(send: &mut quinn::SendStream, user_id: u64) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeUnregisterUser(proto::BridgeUnregisterUser { user_id })),
    };
    send.write_all(&encode_frame(&msg)).await?;
    Ok(())
}

/// Move a virtual user to a room.
pub async fn send_bridge_join_room(send: &mut quinn::SendStream, user_id: u64, room_id: proto::RoomId) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeJoinRoom(proto::BridgeJoinRoom {
            user_id,
            room_id: Some(room_id),
        })),
    };
    send.write_all(&encode_frame(&msg)).await?;
    Ok(())
}

/// Update a virtual user's mute/deaf status.
pub async fn send_bridge_set_user_status(
    send: &mut quinn::SendStream,
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
    send.write_all(&encode_frame(&msg)).await?;
    Ok(())
}

/// Send a chat message on behalf of a virtual user.
pub async fn send_bridge_chat_message(send: &mut quinn::SendStream, user_id: u64, text: &str) -> Result<()> {
    let msg = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::BridgeChatMessage(proto::BridgeChatMessage {
            user_id,
            text: text.to_string(),
        })),
    };
    send.write_all(&encode_frame(&msg)).await?;
    Ok(())
}

/// Read the next Rumble envelope from the stream.
pub async fn read_envelope(recv: &mut quinn::RecvStream, buf: &mut BytesMut) -> Result<Option<proto::Envelope>> {
    // First check if we already have a complete frame buffered
    if let Some(frame) = try_decode_frame(buf) {
        let env = proto::Envelope::decode(&*frame)?;
        return Ok(Some(env));
    }

    // Read more data
    let mut chunk = [0u8; 4096];
    match recv.read(&mut chunk).await? {
        Some(n) => {
            buf.extend_from_slice(&chunk[..n]);
            if let Some(frame) = try_decode_frame(buf) {
                let env = proto::Envelope::decode(&*frame)?;
                Ok(Some(env))
            } else {
                Ok(None)
            }
        }
        None => bail!("Rumble server closed connection"),
    }
}

fn make_bridge_endpoint(remote_addr: std::net::SocketAddr) -> Result<Endpoint> {
    let bind_addr: std::net::SocketAddr = if remote_addr.is_ipv6() {
        "[::]:0".parse().expect("valid IPv6 bind address literal")
    } else {
        "0.0.0.0:0".parse().expect("valid IPv4 bind address literal")
    };
    let mut endpoint = Endpoint::client(bind_addr)?;

    // Use a permissive TLS config that accepts any certificate (bridge use case)
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut client_cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    client_cfg.alpn_protocols = vec![b"rumble".to_vec()];

    let rustls_config = Arc::new(client_cfg);
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config)?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        std::time::Duration::from_secs(30)
            .try_into()
            .expect("30s is valid for quinn idle timeout"),
    ));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    client_config.transport_config(Arc::new(transport));

    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// TLS certificate verifier that accepts any certificate (for bridge -> rumble connection).
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

fn compute_server_cert_hash(conn: &quinn::Connection) -> [u8; 32] {
    if let Some(peer_identity) = conn.peer_identity() {
        if let Some(certs) = peer_identity.downcast_ref::<Vec<rustls::pki_types::CertificateDer<'_>>>() {
            if let Some(cert) = certs.first() {
                return compute_cert_hash(cert.as_ref());
            }
        }
    }
    warn!("Could not get server certificate for hash computation");
    [0u8; 32]
}

async fn wait_for_server_hello(recv: &mut quinn::RecvStream, buf: &mut BytesMut) -> Result<([u8; 32], u64)> {
    loop {
        let mut chunk = [0u8; 4096];
        match recv.read(&mut chunk).await? {
            Some(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(frame) = try_decode_frame(buf) {
                    if let Ok(env) = proto::Envelope::decode(&*frame) {
                        match env.payload {
                            Some(Payload::ServerHello(sh)) => {
                                if sh.nonce.len() != 32 {
                                    bail!("Invalid nonce length in ServerHello");
                                }
                                let nonce: [u8; 32] = sh.nonce.try_into().unwrap();
                                return Ok((nonce, sh.user_id));
                            }
                            Some(Payload::AuthFailed(af)) => {
                                bail!("Authentication failed: {}", af.error);
                            }
                            _ => {}
                        }
                    }
                }
            }
            None => bail!("Server closed connection during handshake"),
        }
    }
}

async fn wait_for_auth_result(
    recv: &mut quinn::RecvStream,
    buf: &mut BytesMut,
) -> Result<(Vec<proto::RoomInfo>, Vec<proto::User>)> {
    loop {
        let mut chunk = [0u8; 4096];
        match recv.read(&mut chunk).await? {
            Some(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(frame) = try_decode_frame(buf) {
                    if let Ok(env) = proto::Envelope::decode(&*frame) {
                        match env.payload {
                            Some(Payload::AuthFailed(af)) => {
                                bail!("Authentication failed: {}", af.error);
                            }
                            Some(Payload::ServerEvent(se)) => {
                                if let Some(proto::server_event::Kind::ServerState(ss)) = se.kind {
                                    return Ok((ss.rooms, ss.users));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            None => bail!("Server closed connection during authentication"),
        }
    }
}
