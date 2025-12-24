//! Rumble wire protocol API definitions.
//!
//! Generated protobuf types live in the `proto` module. This crate also
//! provides small helpers for framing protobuf messages over QUIC streams.

use bytes::{Buf, BufMut, BytesMut};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/rumble.api.v1.rs"));
}

use prost::Message;
use blake3::Hasher;

/// Encode a protobuf message into a length-prefixed frame.
pub fn encode_frame<M: Message>(msg: &M) -> Vec<u8> {
    let mut buf = BytesMut::new();
    // Reserve space for length prefix.
    buf.reserve(4 + msg.encoded_len());
    buf.put_u32(msg.encoded_len() as u32);
    msg.encode(&mut buf).expect("encoding to BytesMut cannot fail");
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

/// Compute a portable state hash over a protobuf `Message` by hashing its
/// canonical prost encoding bytes. This function should only be used on
/// messages that do not include the `state_hash` field to avoid recursion.
pub fn compute_state_hash<M: Message>(msg: &M) -> Vec<u8> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf).expect("prost encode failed");
    let mut hasher = Hasher::new();
    hasher.update(&buf);
    let digest = hasher.finalize();
    digest.as_bytes().to_vec()
}

/// Compute a state hash from a RoomState message with canonical sorting.
/// 
/// This function canonicalizes the RoomState by sorting rooms by ID and users
/// by user_id before hashing. This ensures deterministic hashes regardless of
/// the order items were added.
/// 
/// This is the standard hash function used for state synchronization between
/// client and server.
pub fn compute_room_state_hash(room_state: &proto::RoomState) -> Vec<u8> {
    // Create a canonicalized copy with sorted rooms and users for determinism
    let mut canonical = room_state.clone();
    
    // Sort rooms by ID for deterministic ordering
    canonical.rooms.sort_by(|a, b| {
        let a_id = a.id.as_ref().map(|r| r.value).unwrap_or(0);
        let b_id = b.id.as_ref().map(|r| r.value).unwrap_or(0);
        a_id.cmp(&b_id)
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
