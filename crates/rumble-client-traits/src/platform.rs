//! Platform bundle trait that ties together the engine's platform-specific
//! associated types.

use std::{path::PathBuf, sync::Arc};

use crate::{
    FileTransferPlugin, StreamOpener, audio::AudioBackend, codec::VoiceCodec, file_transfer::PluginEventSink,
    transport::Transport,
};

/// Bundle trait grouping the platform-specific associated types the engine
/// consumes generically.
///
/// Key signing is intentionally *not* an associated type here — the engine
/// holds an `Arc<dyn KeySigning>` provided by the app, which keeps multiple
/// identity sources (e.g. shell SSH agent + macOS Keychain) selectable at
/// runtime without re-parameterising `BackendHandle`.
pub trait Platform: Send + Sync + 'static {
    type Transport: Transport;
    type AudioBackend: AudioBackend;
    type Codec: VoiceCodec;

    /// Create the file transfer plugin for this platform, if supported.
    ///
    /// Returns `None` if the platform does not support file transfers.
    ///
    /// `event_sink` is an opaque callback the plugin can use to surface
    /// user-visible toasts (e.g. relay rejection, dup-upload warning)
    /// without depending on `rumble-client`'s `BackendEvent` type.
    fn create_file_transfer_plugin(
        opener: Arc<dyn StreamOpener>,
        downloads_dir: PathBuf,
        event_sink: Option<PluginEventSink>,
    ) -> Option<Arc<dyn FileTransferPlugin>>;
}
