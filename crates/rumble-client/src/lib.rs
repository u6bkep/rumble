//! Platform-agnostic Rumble client library.
//!
//! This crate contains the client logic that works across all platforms.
//! Platform-specific implementations (audio, transport, codec, etc.) are
//! injected via the `Platform` trait.

pub mod audio;
pub mod auth;
pub mod cert;
pub mod codec;
pub mod file_transfer;
pub mod keys;
pub mod platform;
pub mod storage;
pub mod transport;

// Re-export key types
pub use audio::{AudioBackend, AudioCaptureStream, AudioPlaybackStream};
pub use cert::{
    CapturedCert, ServerCertInfo, compute_sha256_fingerprint, is_cert_error_message, new_captured_cert,
    peek_captured_cert, take_captured_cert,
};
pub use codec::{VoiceCodec, VoiceDecoder, VoiceEncoder};
pub use file_transfer::{
    FileOffer, FileTransferPlugin, PluginPeerConnectionType, PluginPeerInfo, PluginPeerState, PluginTransferState,
    TransferId, TransferStatus,
};
pub use keys::{KeyInfo, KeySigning, KeySource};
pub use platform::Platform;
pub use storage::PersistentStorage;
pub use transport::{DatagramTransport, TlsConfig, Transport, TransportRecvStream};
