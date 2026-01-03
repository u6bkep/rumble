//! Rumble wire protocol API definitions.
//!
//! Generated protobuf types live in the `proto` module. This crate also
//! provides small helpers for framing protobuf messages over QUIC streams.

use blake3::Hasher;
use bytes::BytesMut;
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
    msg.encode_length_delimited(&mut buf)
        .expect("encoding to BytesMut cannot fail");
    buf.to_vec()
}

/// Attempt to read a single length-prefixed frame from the buffer.
///
/// Returns `Some(frame_bytes)` when a full frame is available, leaving any
/// remaining bytes in `src`. Returns `None` if not enough data is present yet.
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

// =============================================================================
// File Message Types
// =============================================================================

use serde::{Deserialize, Serialize};

/// Schema URL for file messages (for validation purposes).
pub const FILE_MESSAGE_SCHEMA: &str = "https://rumble.example/schemas/file-message-v1.json";

/// File metadata for chat file messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileInfo {
    /// Original filename.
    pub name: String,
    /// File size in bytes.
    pub size: u64,
    /// MIME type (e.g., "image/jpeg").
    pub mime: String,
    /// 40-character hex-encoded SHA-1 infohash.
    pub infohash: String,
}

/// A file message that can be sent in chat.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMessage {
    /// Schema URL (must match FILE_MESSAGE_SCHEMA).
    #[serde(rename = "$schema")]
    pub schema: String,
    /// Message type (must be "file").
    #[serde(rename = "type")]
    pub message_type: String,
    /// File information.
    pub file: FileInfo,
}

impl FileMessage {
    /// Create a new file message.
    pub fn new(name: String, size: u64, mime: String, infohash: String) -> Self {
        Self {
            schema: FILE_MESSAGE_SCHEMA.to_string(),
            message_type: "file".to_string(),
            file: FileInfo {
                name,
                size,
                mime,
                infohash,
            },
        }
    }

    /// Serialize to JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Derive a magnet link from the infohash.
    pub fn magnet_link(&self) -> String {
        format!("magnet:?xt=urn:btih:{}", self.file.infohash)
    }
}

/// Result of parsing a potential file message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileMessageParseResult {
    /// Successfully parsed as a file message.
    FileMessage(FileMessage),
    /// Not a file message (plain text or other content).
    NotFileMessage,
    /// JSON parsed but schema/type didn't match or had extraneous fields.
    InvalidFileMessage(String),
}

/// Parse a chat message text to check if it's a file message.
///
/// This implements the validation specified in the file transfer spec:
/// 1. Attempt JSON parse
/// 2. Validate against the file message schema
/// 3. Reject messages with extraneous fields (prevents false positives)
pub fn parse_file_message(text: &str) -> FileMessageParseResult {
    // Try to parse as JSON
    let json_value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return FileMessageParseResult::NotFileMessage,
    };

    // Must be an object
    let obj = match json_value.as_object() {
        Some(o) => o,
        None => return FileMessageParseResult::NotFileMessage,
    };

    // Check for file message type indicator
    let message_type = match obj.get("type").and_then(|v| v.as_str()) {
        Some(t) if t == "file" => t,
        Some(_) => return FileMessageParseResult::NotFileMessage,
        None => return FileMessageParseResult::NotFileMessage,
    };

    // Validate schema if present
    if let Some(schema) = obj.get("$schema") {
        if schema.as_str() != Some(FILE_MESSAGE_SCHEMA) {
            return FileMessageParseResult::InvalidFileMessage(
                "Invalid schema URL".to_string()
            );
        }
    }

    // Check for extraneous top-level fields
    let allowed_fields = ["$schema", "type", "file"];
    for key in obj.keys() {
        if !allowed_fields.contains(&key.as_str()) {
            return FileMessageParseResult::InvalidFileMessage(
                format!("Extraneous field: {}", key)
            );
        }
    }

    // Validate file object
    let file_obj = match obj.get("file").and_then(|v| v.as_object()) {
        Some(f) => f,
        None => {
            return FileMessageParseResult::InvalidFileMessage(
                "Missing 'file' object".to_string()
            );
        }
    };

    // Check for extraneous file fields
    let allowed_file_fields = ["name", "size", "mime", "infohash"];
    for key in file_obj.keys() {
        if !allowed_file_fields.contains(&key.as_str()) {
            return FileMessageParseResult::InvalidFileMessage(
                format!("Extraneous file field: {}", key)
            );
        }
    }

    // Validate required fields
    let name = match file_obj.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return FileMessageParseResult::InvalidFileMessage(
                "Missing 'file.name'".to_string()
            );
        }
    };

    let size = match file_obj.get("size").and_then(|v| v.as_u64()) {
        Some(s) => s,
        None => {
            return FileMessageParseResult::InvalidFileMessage(
                "Missing or invalid 'file.size'".to_string()
            );
        }
    };

    let mime = match file_obj.get("mime").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => {
            return FileMessageParseResult::InvalidFileMessage(
                "Missing 'file.mime'".to_string()
            );
        }
    };

    let infohash = match file_obj.get("infohash").and_then(|v| v.as_str()) {
        Some(h) => h.to_string(),
        None => {
            return FileMessageParseResult::InvalidFileMessage(
                "Missing 'file.infohash'".to_string()
            );
        }
    };

    // Validate infohash format (40 hex chars)
    if infohash.len() != 40 || !infohash.chars().all(|c| c.is_ascii_hexdigit()) {
        return FileMessageParseResult::InvalidFileMessage(
            "Invalid infohash format (expected 40 hex characters)".to_string()
        );
    }

    FileMessageParseResult::FileMessage(FileMessage {
        schema: FILE_MESSAGE_SCHEMA.to_string(),
        message_type: message_type.to_string(),
        file: FileInfo {
            name,
            size,
            mime,
            infohash,
        },
    })
}

#[cfg(test)]
mod file_message_tests {
    use super::*;

    #[test]
    fn test_parse_valid_file_message() {
        let json = r#"{
            "$schema": "https://rumble.example/schemas/file-message-v1.json",
            "type": "file",
            "file": {
                "name": "photo.jpg",
                "size": 1048576,
                "mime": "image/jpeg",
                "infohash": "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
            }
        }"#;

        match parse_file_message(json) {
            FileMessageParseResult::FileMessage(fm) => {
                assert_eq!(fm.file.name, "photo.jpg");
                assert_eq!(fm.file.size, 1048576);
                assert_eq!(fm.file.mime, "image/jpeg");
                assert_eq!(fm.file.infohash, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
            }
            other => panic!("Expected FileMessage, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_plain_text() {
        let text = "Hello, world!";
        assert_eq!(parse_file_message(text), FileMessageParseResult::NotFileMessage);
    }

    #[test]
    fn test_parse_user_json() {
        // User-pasted JSON that isn't a file message
        let json = r#"{"foo": "bar", "baz": 123}"#;
        assert_eq!(parse_file_message(json), FileMessageParseResult::NotFileMessage);
    }

    #[test]
    fn test_parse_extraneous_fields() {
        let json = r#"{
            "type": "file",
            "file": {
                "name": "test.txt",
                "size": 100,
                "mime": "text/plain",
                "infohash": "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
            },
            "extra": "field"
        }"#;

        match parse_file_message(json) {
            FileMessageParseResult::InvalidFileMessage(msg) => {
                assert!(msg.contains("Extraneous field"));
            }
            other => panic!("Expected InvalidFileMessage, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_invalid_infohash() {
        let json = r#"{
            "type": "file",
            "file": {
                "name": "test.txt",
                "size": 100,
                "mime": "text/plain",
                "infohash": "tooshort"
            }
        }"#;

        match parse_file_message(json) {
            FileMessageParseResult::InvalidFileMessage(msg) => {
                assert!(msg.contains("infohash"));
            }
            other => panic!("Expected InvalidFileMessage, got {:?}", other),
        }
    }

    #[test]
    fn test_file_message_to_json() {
        let fm = FileMessage::new(
            "test.txt".to_string(),
            1024,
            "text/plain".to_string(),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d".to_string(),
        );
        let json = fm.to_json().unwrap();
        assert!(json.contains("test.txt"));
        assert!(json.contains("1024"));
    }

    #[test]
    fn test_magnet_link() {
        let fm = FileMessage::new(
            "test.txt".to_string(),
            1024,
            "text/plain".to_string(),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d".to_string(),
        );
        assert_eq!(
            fm.magnet_link(),
            "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );
    }
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
