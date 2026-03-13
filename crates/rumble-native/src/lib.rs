//! Native desktop Platform implementation for rumble-client.
//!
//! Provides concrete implementations of the platform traits using:
//! - **quinn** for QUIC transport
//! - **cpal** for audio I/O
//! - **opus** for voice codec
//! - **ed25519-dalek** + SSH agent for key management
//! - **serde_json** + filesystem for persistent storage

pub mod audio;
pub mod cert_verifier;
pub mod codec;
pub mod file_transfer_relay;
pub mod keys;
pub mod storage;
pub mod transport;

pub use audio::{CpalAudioBackend, CpalCaptureStream, CpalPlaybackStream};
pub use codec::{NativeOpusCodec, NativeOpusDecoder, NativeOpusEncoder};
pub use file_transfer_relay::FileTransferRelayPlugin;
pub use keys::NativeKeySigning;
pub use storage::FileStorage;
pub use transport::{QuinnDatagramHandle, QuinnRecvStream, QuinnTransport};

// Re-export quinn::Connection for downstream crates that need raw QUIC access
// (e.g., mumble-bridge for datagrams and close detection)
pub use quinn::Connection as QuinnConnection;

use rumble_client::Platform;

/// Native desktop platform using quinn, cpal, opus, and filesystem storage.
pub struct NativePlatform;

impl Platform for NativePlatform {
    type Transport = QuinnTransport;
    type AudioBackend = CpalAudioBackend;
    type Codec = NativeOpusCodec;
    type Storage = FileStorage;
    type KeyManager = NativeKeySigning;
}
