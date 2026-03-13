//! Shared auth handshake helpers for Rumble protocol connections.
//!
//! These functions handle the common envelope send/receive patterns used
//! during the authentication handshake by both the backend client and the
//! mumble-bridge.

use api::proto::{self, envelope::Payload};
use prost::Message;

use crate::transport::Transport;

/// Send a protobuf envelope via the transport.
pub async fn send_envelope<T: Transport>(transport: &mut T, env: &proto::Envelope) -> anyhow::Result<()> {
    let data = env.encode_to_vec();
    transport.send(&data).await
}

/// Wait for ServerHello message and extract nonce and user_id.
pub async fn wait_for_server_hello<T: Transport>(transport: &mut T) -> anyhow::Result<([u8; 32], u64)> {
    loop {
        match transport.recv().await? {
            Some(frame) => {
                if let Ok(env) = proto::Envelope::decode(&*frame) {
                    match env.payload {
                        Some(Payload::ServerHello(sh)) => {
                            if sh.nonce.len() != 32 {
                                return Err(anyhow::anyhow!("Invalid nonce length in ServerHello"));
                            }
                            let nonce: [u8; 32] = sh.nonce.try_into().unwrap();
                            return Ok((nonce, sh.user_id));
                        }
                        Some(Payload::AuthFailed(af)) => {
                            return Err(anyhow::anyhow!("Authentication failed: {}", af.error));
                        }
                        _ => {}
                    }
                }
            }
            None => {
                return Err(anyhow::anyhow!("Server closed connection during handshake"));
            }
        }
    }
}

/// Wait for authentication result (ServerState or AuthFailed).
///
/// Returns `(rooms, users, groups)` from the initial server state.
pub async fn wait_for_auth_result<T: Transport>(
    transport: &mut T,
) -> anyhow::Result<(Vec<proto::RoomInfo>, Vec<proto::User>, Vec<proto::GroupInfo>)> {
    loop {
        match transport.recv().await? {
            Some(frame) => {
                if let Ok(env) = proto::Envelope::decode(&*frame) {
                    match env.payload {
                        Some(Payload::AuthFailed(af)) => {
                            return Err(anyhow::anyhow!("Authentication failed: {}", af.error));
                        }
                        Some(Payload::ServerEvent(se)) => {
                            if let Some(proto::server_event::Kind::ServerState(ss)) = se.kind {
                                return Ok((ss.rooms, ss.users, ss.groups));
                            }
                        }
                        _ => {}
                    }
                }
            }
            None => {
                return Err(anyhow::anyhow!("Server closed connection during authentication"));
            }
        }
    }
}
