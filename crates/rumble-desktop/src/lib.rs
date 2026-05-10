//! Native desktop Platform implementation for rumble-client-traits.
//!
//! Provides concrete implementations of the platform traits using:
//! - **quinn** for QUIC transport
//! - **cpal** for audio I/O (with PulseAudio as primary backend on Linux)
//! - **opus** for voice codec
//!
//! Identity / key signing is intentionally not bundled here — the engine
//! consumes an `Arc<dyn KeySigning>` provided by the app, which is the right
//! seam for swapping in macOS Keychain, mobile keyring, SSH-agent, or
//! hardware-token backends without touching this crate.

#[cfg(feature = "audio")]
pub mod audio;
pub mod cert_verifier;
#[cfg(feature = "codec")]
pub mod codec;
pub mod file_transfer_relay;
pub mod transport;

#[cfg(feature = "audio")]
pub use audio::{
    CpalAudioBackend, CpalCaptureStream, CpalPlaybackStream, DesktopAudioBackend, DesktopCaptureStream,
    DesktopPlaybackStream,
};
#[cfg(feature = "codec")]
pub use codec::{NativeOpusCodec, NativeOpusDecoder, NativeOpusEncoder};
pub use file_transfer_relay::FileTransferRelayPlugin;
pub use transport::{
    QuinnBiRecvStream, QuinnBiSendStream, QuinnBiStreamHandle, QuinnDatagramHandle, QuinnRecvStream, QuinnTransport,
};

// Re-export quinn::Connection for downstream crates that need raw QUIC access
// (e.g., mumble-bridge for datagrams and close detection)
pub use quinn::Connection as QuinnConnection;

#[cfg(all(feature = "audio", feature = "codec"))]
use std::{path::PathBuf, sync::Arc};

#[cfg(all(feature = "audio", feature = "codec"))]
use rumble_client_traits::Platform;

/// Native desktop platform using quinn, cpal, and opus.
///
/// The full Platform impl requires both `audio` and `codec` features (on by
/// default). Server-side daemons that only need transport pieces (e.g.
/// mumble-bridge) can disable default features and use `QuinnTransport`
/// directly without instantiating `NativePlatform`.
pub struct NativePlatform;

#[cfg(all(feature = "audio", feature = "codec"))]
impl Platform for NativePlatform {
    type Transport = QuinnTransport;
    type AudioBackend = DesktopAudioBackend;
    type Codec = NativeOpusCodec;

    fn create_file_transfer_plugin(
        opener: Arc<dyn rumble_client_traits::StreamOpener>,
        downloads_dir: PathBuf,
    ) -> Option<Arc<dyn rumble_client_traits::FileTransferPlugin>> {
        Some(Arc::new(crate::FileTransferRelayPlugin::new(opener, downloads_dir)))
    }
}
