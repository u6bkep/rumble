//! Rumble wire protocol API definitions.
//!
//! Generated protobuf types live in the `proto` module. This crate also
//! provides small helpers for framing protobuf messages over QUIC streams.

use blake3::Hasher;
use bytes::{Buf, BufMut, BytesMut};
use prost::Message;
pub use uuid::Uuid;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/rumble.api.v1.rs"));
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
    uuid_from_room_id(room_id)
        .map(|u| u == ROOT_ROOM_UUID)
        .unwrap_or(false)
}

/// Generate a new random RoomId.
pub fn new_room_id() -> proto::RoomId {
    room_id_from_uuid(Uuid::new_v4())
}

/// Encode a protobuf message into a length-prefixed frame.
pub fn encode_frame<M: Message>(msg: &M) -> Vec<u8> {
    let mut buf = BytesMut::new();
    // Reserve space for length prefix.
    buf.reserve(4 + msg.encoded_len());
    buf.put_u32(msg.encoded_len() as u32);
    msg.encode(&mut buf)
        .expect("encoding to BytesMut cannot fail");
    buf.to_vec()
}

/// Attempt to read a single length-prefixed frame from the buffer.
///
/// Returns `Some(frame_bytes)` when a full frame is available, leaving any
/// remaining bytes in `src`. Returns `None` if not enough data is present yet.
pub fn try_decode_frame(src: &mut BytesMut) -> Option<Vec<u8>> {
    const LEN_PREFIX: usize = 4;
    if src.len() < LEN_PREFIX {
        return None;
    }

    let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;
    if src.len() < LEN_PREFIX + len {
        return None;
    }

    // Split off the frame including the length prefix.
    src.advance(LEN_PREFIX);
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
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    hasher.finalize().into()
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
        use ed25519_dalek::{SigningKey, Signer, Verifier};
        use rand::rngs::OsRng;
        
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = signing_key.verifying_key();
        
        let payload = build_auth_payload(
            &[0u8; 32],
            1234567890123,
            &public_key.to_bytes(),
            42,
            &[0u8; 32],
        );
        
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
