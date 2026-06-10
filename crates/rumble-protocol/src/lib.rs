//! Rumble wire protocol API definitions.
//!
//! Generated protobuf types live in the `proto` module. This crate also
//! provides small helpers for framing protobuf messages over QUIC streams,
//! state hashing, and the [`ChatAttachment`] sidecar type that both
//! client and server need.
//!
//! Client-side in-memory state types (`State`, `Command`, `ChatMessage`,
//! `AudioState`, etc.) live in `rumble_client::events` — they are
//! pure-data but client-shaped, and don't belong in a crate named for
//! the wire format.

use blake3::Hasher;
use bytes::BytesMut;
use prost::Message;
pub use uuid::Uuid;

pub mod permissions;
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/rumble.api.v1.rs"));
}

// =============================================================================
// Chat Attachment (wire-shape sidecar shared by client + traits + plugins)
// =============================================================================

/// Plugin-namespaced sidecar attached to a chat message. The server
/// forwards this unchanged; receivers dispatch by `namespace` to find a
/// matching plugin renderer/handler. Clients without a matching plugin
/// display `fallback_text`.
///
/// Lives at the wire layer because the [`FileTransferPlugin`] trait
/// (in `rumble-client-traits`) needs to produce and consume these, and
/// the on-disk chat-history sync format embeds them as JSON.
///
/// [`FileTransferPlugin`]: ../rumble_client_traits/file_transfer/trait.FileTransferPlugin.html
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChatAttachment {
    /// Reverse-DNS plugin identifier (e.g. "rumble.file_transfer.relay").
    pub namespace: String,
    /// Schema version for the payload format, plugin-defined.
    pub schema_version: u32,
    /// Opaque plugin-encoded payload (consumed by the matching plugin).
    pub payload: Vec<u8>,
    /// Text shown when no plugin matches `namespace`.
    pub fallback_text: String,
}

pub fn chat_attachment_from_proto(att: proto::ChatAttachment) -> ChatAttachment {
    ChatAttachment {
        namespace: att.namespace,
        schema_version: att.schema_version,
        payload: att.payload,
        fallback_text: att.fallback_text,
    }
}

pub fn chat_attachment_to_proto(att: &ChatAttachment) -> proto::ChatAttachment {
    proto::ChatAttachment {
        namespace: att.namespace.clone(),
        schema_version: att.schema_version,
        payload: att.payload.clone(),
        fallback_text: att.fallback_text.clone(),
    }
}

/// The Root room UUID (all zeros).
/// This is the default room that all users join when connecting.
pub const ROOT_ROOM_UUID: Uuid = Uuid::nil();

/// Create a RoomId from a UUID.
pub fn room_id_from_uuid(uuid: Uuid) -> proto::RoomId {
    proto::RoomId {
        uuid: uuid.as_bytes().to_vec(),
    }
}

/// Extract a UUID from a RoomId.
/// Returns None if the RoomId's uuid field is not exactly 16 bytes.
pub fn uuid_from_room_id(room_id: &proto::RoomId) -> Option<Uuid> {
    if room_id.uuid.len() == 16 {
        let bytes: [u8; 16] = room_id.uuid.as_slice().try_into().ok()?;
        Some(Uuid::from_bytes(bytes))
    } else {
        None
    }
}

/// Create a new RoomId for the Root room (UUID 0).
pub fn root_room_id() -> proto::RoomId {
    room_id_from_uuid(ROOT_ROOM_UUID)
}

/// Check if a RoomId represents the Root room.
pub fn is_root_room(room_id: &proto::RoomId) -> bool {
    uuid_from_room_id(room_id).map(|u| u == ROOT_ROOM_UUID).unwrap_or(false)
}

/// Generate a new random RoomId.
pub fn new_room_id() -> proto::RoomId {
    room_id_from_uuid(Uuid::new_v4())
}

/// Encode a protobuf message into a length-prefixed frame.
pub fn encode_frame<M: Message>(msg: &M) -> Vec<u8> {
    let mut buf = BytesMut::new();
    msg.encode_length_delimited(&mut buf)
        .expect("encoding to BytesMut cannot fail");
    buf.to_vec()
}

/// Encode raw bytes into a varint length-prefixed frame.
///
/// Same wire format as `encode_frame` but takes pre-serialized bytes
/// instead of a prost `Message`. Used by transport implementations that
/// receive already-encoded protobuf data.
pub fn encode_frame_raw(data: &[u8]) -> Vec<u8> {
    let delimiter_len = prost::length_delimiter_len(data.len());
    let mut buf = Vec::with_capacity(delimiter_len + data.len());
    prost::encode_length_delimiter(data.len(), &mut buf).expect("encoding length delimiter cannot fail");
    buf.extend_from_slice(data);
    buf
}

/// Maximum size of a single length-prefixed control frame (envelope).
///
/// Control messages are small (chat, state sync); bulk data goes over separate
/// plugin streams, not envelopes. A reader fed from an untrusted socket should
/// refuse to buffer more than this so a peer can't declare a huge length and
/// exhaust memory by trickling bytes for a frame that never completes.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Attempt to read a single length-prefixed frame from the buffer.
///
/// Returns `Some(frame_bytes)` when a full frame is available, leaving any
/// remaining bytes in `src`. Returns `None` if not enough data is present yet.
///
/// Note: this does not itself enforce [`MAX_FRAME_LEN`] — a reader fed from an
/// untrusted socket must cap its accumulation buffer (see the server read loop).
pub fn try_decode_frame(src: &mut BytesMut) -> Option<Vec<u8>> {
    // Peek at the length delimiter
    let mut peek_buf = src.clone();
    let len = match prost::decode_length_delimiter(&mut peek_buf) {
        Ok(len) => len,
        Err(_) => return None,
    };

    let delimiter_len = prost::length_delimiter_len(len);
    if src.len() < delimiter_len + len {
        return None;
    }

    // Advance past the delimiter
    let _ = prost::decode_length_delimiter(&mut *src).unwrap();
    // Split off the frame
    let frame = src.split_to(len);
    Some(frame.to_vec())
}

/// Compute a state hash from a ServerState message with canonical sorting.
///
/// This function canonicalizes the ServerState by sorting rooms by ID and users
/// by user_id before hashing. This ensures deterministic hashes regardless of
/// the order items were added.
///
/// This is the standard hash function used for state synchronization between
/// client and server.
pub fn compute_server_state_hash(server_state: &proto::ServerState) -> Vec<u8> {
    // Create a canonicalized copy with sorted rooms and users for determinism
    let mut canonical = server_state.clone();

    // Slash commands are static connection metadata, not mutable state — they
    // ride along in the full snapshot but must not influence the hash (which
    // gates incremental-update verification).
    canonical.slash_commands.clear();

    // Groups are excluded for the same reason: only the *full* ServerState push
    // carries the group list, while every incremental `StateUpdate` is built
    // with `groups: vec![]` (the Update variants have no group field). Hashing
    // groups would make the full-sync baseline diverge from every incremental
    // hash for the same logical state, resync-looping a verifying client.
    canonical.groups.clear();

    // Sort rooms by UUID bytes for deterministic ordering
    canonical.rooms.sort_by(|a, b| {
        let a_id = a.id.as_ref().map(|r| r.uuid.as_slice()).unwrap_or(&[]);
        let b_id = b.id.as_ref().map(|r| r.uuid.as_slice()).unwrap_or(&[]);
        a_id.cmp(b_id)
    });

    // Sort users by user_id for deterministic ordering
    canonical.users.sort_by(|a, b| {
        let a_id = a.user_id.as_ref().map(|u| u.value).unwrap_or(0);
        let b_id = b.user_id.as_ref().map(|u| u.value).unwrap_or(0);
        a_id.cmp(&b_id)
    });

    // Serialize to bytes
    let mut buf = Vec::with_capacity(canonical.encoded_len());
    canonical.encode(&mut buf).expect("prost encode failed");

    // Hash with blake3
    let mut hasher = Hasher::new();
    hasher.update(&buf);
    let digest = hasher.finalize();
    digest.as_bytes().to_vec()
}

/// Helper to attach a just-computed state hash to an `Envelope`.
pub fn with_state_hash(mut env: proto::Envelope, hash: Vec<u8>) -> proto::Envelope {
    env.state_hash = hash;
    env
}

// =============================================================================
// Ed25519 Authentication Helpers
// =============================================================================

/// Compute the signature payload for authentication.
///
/// Layout (112 bytes total, all fixed-length):
/// - nonce: 32 bytes
/// - timestamp_ms: 8 bytes (big-endian)
/// - public_key: 32 bytes
/// - user_id: 8 bytes (big-endian)
/// - server_cert_hash: 32 bytes (SHA256 of server TLS certificate)
pub fn build_auth_payload(
    nonce: &[u8; 32],
    timestamp_ms: i64,
    public_key: &[u8; 32],
    user_id: u64,
    server_cert_hash: &[u8; 32],
) -> [u8; 112] {
    let mut payload = [0u8; 112];
    payload[0..32].copy_from_slice(nonce);
    payload[32..40].copy_from_slice(&timestamp_ms.to_be_bytes());
    payload[40..72].copy_from_slice(public_key);
    payload[72..80].copy_from_slice(&user_id.to_be_bytes());
    payload[80..112].copy_from_slice(server_cert_hash);
    payload
}

/// Compute SHA256 hash of a DER-encoded certificate.
pub fn compute_cert_hash(cert_der: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    hasher.finalize().into()
}

/// Compute a session identifier from an ephemeral session public key.
/// Uses BLAKE3 over the raw 32-byte key to produce a stable 32-byte ID.
pub fn compute_session_id(session_public_key: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(session_public_key);
    hasher.finalize().into()
}

/// Build the payload that the user's long-term key signs to issue a session certificate.
/// Layout (variable length due to optional device string):
/// - session_public_key: 32 bytes
/// - issued_ms: 8 bytes (big-endian)
/// - expires_ms: 8 bytes (big-endian)
/// - device length (u16 big-endian) + UTF-8 bytes (may be zero length)
pub fn build_session_cert_payload(
    session_public_key: &[u8; 32],
    issued_ms: i64,
    expires_ms: i64,
    device: Option<&str>,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32 + 8 + 8 + 2 + device.map(|d| d.len()).unwrap_or(0));
    buf.extend_from_slice(session_public_key);
    buf.extend_from_slice(&issued_ms.to_be_bytes());
    buf.extend_from_slice(&expires_ms.to_be_bytes());
    let device_bytes = device.unwrap_or("").as_bytes();
    let len: u16 = device_bytes.len() as u16;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(device_bytes);
    buf
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn test_build_auth_payload() {
        let nonce = [1u8; 32];
        let timestamp_ms = 1234567890123i64;
        let public_key = [2u8; 32];
        let user_id = 42u64;
        let cert_hash = [3u8; 32];

        let payload = build_auth_payload(&nonce, timestamp_ms, &public_key, user_id, &cert_hash);

        assert_eq!(payload.len(), 112);
        assert_eq!(&payload[0..32], &nonce);
        assert_eq!(&payload[32..40], &timestamp_ms.to_be_bytes());
        assert_eq!(&payload[40..72], &public_key);
        assert_eq!(&payload[72..80], &user_id.to_be_bytes());
        assert_eq!(&payload[80..112], &cert_hash);
    }

    #[test]
    fn test_signature_verification() {
        use ed25519_dalek::{Signer, SigningKey, Verifier};

        let signing_key = SigningKey::from_bytes(&rand::random());
        let public_key = signing_key.verifying_key();

        let payload = build_auth_payload(&[0u8; 32], 1234567890123, &public_key.to_bytes(), 42, &[0u8; 32]);

        let signature = signing_key.sign(&payload);

        assert!(public_key.verify(&payload, &signature).is_ok());
    }

    #[test]
    fn test_compute_cert_hash() {
        let cert_data = b"test certificate data";
        let hash = compute_cert_hash(cert_data);
        assert_eq!(hash.len(), 32);

        // Same input should produce same hash
        let hash2 = compute_cert_hash(cert_data);
        assert_eq!(hash, hash2);

        // Different input should produce different hash
        let hash3 = compute_cert_hash(b"different data");
        assert_ne!(hash, hash3);
    }
}

#[cfg(test)]
mod state_hash_tests {
    //! Characterization tests for `compute_server_state_hash`. Clients compare
    //! this hash to detect state divergence, so two things matter and are
    //! pinned here: it must be *independent of room/user ordering* (the server
    //! assembles state from nondeterministic maps), and the exact wire hash for
    //! a fixed fixture must be *stable* (a change means the encoding changed and
    //! every client will resync — update the pinned value deliberately).
    use super::*;

    fn room(uuid: u128, name: &str) -> proto::RoomInfo {
        proto::RoomInfo {
            id: Some(proto::RoomId {
                uuid: uuid.to_be_bytes().to_vec(),
            }),
            name: name.to_string(),
            parent_id: None,
            description: None,
            inherit_acl: true,
            acls: vec![],
            effective_permissions: 0,
        }
    }

    fn user(id: u64, name: &str) -> proto::User {
        proto::User {
            user_id: Some(proto::UserId { value: id }),
            username: name.to_string(),
            current_room: None,
            is_muted: false,
            is_deafened: false,
            server_muted: false,
            is_elevated: false,
            groups: vec![],
            label: None,
        }
    }

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn state_hash_is_order_independent() {
        let rooms = vec![room(1, "a"), room(2, "b"), room(3, "c")];
        let users = vec![user(10, "x"), user(20, "y"), user(30, "z")];

        let forward = proto::ServerState {
            rooms: rooms.clone(),
            users: users.clone(),
            groups: vec![],
            slash_commands: vec![],
        };

        let mut rooms_rev = rooms;
        rooms_rev.reverse();
        let mut users_rev = users;
        users_rev.reverse();
        let reversed = proto::ServerState {
            rooms: rooms_rev,
            users: users_rev,
            groups: vec![],
            slash_commands: vec![],
        };

        assert_eq!(
            compute_server_state_hash(&forward),
            compute_server_state_hash(&reversed),
            "the state hash must not depend on room/user ordering"
        );
    }

    /// The hash must ignore `groups`: the full-state push carries the real
    /// group list, but every incremental `StateUpdate` is built with empty
    /// groups, so including them would diverge the two producers for the same
    /// logical state and resync-loop a verifying client.
    #[test]
    fn state_hash_ignores_groups() {
        let base = proto::ServerState {
            rooms: vec![room(1, "Root")],
            users: vec![user(1, "alice")],
            groups: vec![],
            slash_commands: vec![],
        };
        let with_groups = proto::ServerState {
            groups: vec![
                proto::GroupInfo {
                    name: "default".to_string(),
                    is_builtin: true,
                    permissions: 0x0f,
                },
                proto::GroupInfo {
                    name: "admin".to_string(),
                    is_builtin: true,
                    permissions: 0xffff,
                },
            ],
            ..base.clone()
        };

        assert_eq!(
            compute_server_state_hash(&base),
            compute_server_state_hash(&with_groups),
            "group definitions must not influence the state hash"
        );
    }

    #[test]
    fn state_hash_matches_pinned_value() {
        let state = proto::ServerState {
            rooms: vec![room(1, "Root")],
            users: vec![user(1, "alice")],
            groups: vec![],
            slash_commands: vec![],
        };

        // Golden value: update ONLY when the ServerState wire encoding changes
        // on purpose — a silent change here means clients will resync.
        assert_eq!(
            to_hex(&compute_server_state_hash(&state)),
            "7b83a5d8da7bc18c942bdf3a7ea6e5a2068611ef8cfdc6a1f1c5861f8ebc5660",
            "ServerState hash encoding changed"
        );
    }
}
