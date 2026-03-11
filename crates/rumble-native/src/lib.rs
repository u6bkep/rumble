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
pub mod keys;
pub mod storage;
pub mod transport;

pub use audio::{CpalAudioBackend, CpalCaptureStream, CpalPlaybackStream};
pub use codec::{NativeOpusCodec, NativeOpusDecoder, NativeOpusEncoder};
pub use keys::NativeKeySigning;
pub use storage::FileStorage;
pub use transport::QuinnTransport;

// NOTE: The Platform impl is added once all trait implementations are complete.
// See the end of this file for the planned NativePlatform definition.
