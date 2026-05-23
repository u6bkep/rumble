//! Optional file transfer capability.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::transport::{BiRecvStream, BiSendStream};

/// Unique identifier for a file transfer (typically a UUID string).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransferId(pub String);

/// Metadata about a file being offered for transfer.
#[derive(Debug, Clone)]
pub struct FileOffer {
    pub id: TransferId,
    pub name: String,
    pub size: u64,
    pub mime: String,
    /// Opaque data to share with recipients (e.g., relay metadata).
    pub share_data: String,
}

/// Whether a transfer is an upload or download from the local client's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

/// State of a transfer from the plugin's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginTransferState {
    Initializing,
    Downloading,
    Seeding,
    Paused,
    Error,
}

/// Connection type for a transfer peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginPeerConnectionType {
    Direct,
    Relay,
    Utp,
    Socks,
}

/// State of a peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginPeerState {
    Connecting,
    Live,
    Queued,
    Dead,
}

/// Information about a connected peer in a transfer.
#[derive(Debug, Clone)]
pub struct PluginPeerInfo {
    pub address: String,
    pub connection_type: PluginPeerConnectionType,
    pub state: PluginPeerState,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
}

/// Status of a file transfer in progress.
#[derive(Debug, Clone)]
pub struct TransferStatus {
    pub id: TransferId,
    pub name: String,
    pub size: u64,
    /// Whether this is an upload or download from the local client's perspective.
    pub direction: TransferDirection,
    /// Progress as a fraction in [0.0, 1.0].
    pub progress: f32,
    /// Download speed in bytes per second.
    pub download_speed: u64,
    /// Upload speed in bytes per second.
    pub upload_speed: u64,
    /// Number of connected peers.
    pub peers: u32,
    /// Current transfer state.
    pub state: PluginTransferState,
    /// Whether the transfer is finished (fully downloaded).
    pub is_finished: bool,
    /// Error message if in error state.
    pub error: Option<String>,
    /// Local file path if available.
    pub local_path: Option<PathBuf>,
    /// Per-peer details.
    pub peer_details: Vec<PluginPeerInfo>,
}

/// Optional file transfer capability, injected into BackendHandle.
///
/// Not part of `Platform` — different deployments can use different
/// strategies (relay, direct transfer, etc.) or disable file
/// transfer entirely.
///
/// Most methods are synchronous. Implementations running inside a tokio runtime
/// should use `tokio::task::block_in_place` + `Handle::current().block_on()`
/// for async operations. The [`on_incoming_stream`](Self::on_incoming_stream)
/// method is async and called from the stream dispatch task.
#[async_trait]
pub trait FileTransferPlugin: Send + Sync + 'static {
    /// Share a local file and return metadata for recipients.
    fn share(&self, path: PathBuf) -> anyhow::Result<FileOffer>;

    /// Begin downloading a file from opaque share data (e.g., relay metadata).
    fn download(&self, share_data: &str) -> anyhow::Result<TransferId>;

    /// List all active transfers and their status.
    fn transfers(&self) -> Vec<TransferStatus>;

    /// Pause an active transfer.
    fn pause(&self, id: &TransferId) -> anyhow::Result<()>;

    /// Resume a paused transfer.
    fn resume(&self, id: &TransferId) -> anyhow::Result<()>;

    /// Cancel an active transfer.
    ///
    /// If `delete_files` is true, also remove downloaded files from disk.
    fn cancel(&self, id: &TransferId, delete_files: bool) -> anyhow::Result<()>;

    /// Get the local file path for a completed transfer.
    fn get_file_path(&self, id: &TransferId) -> anyhow::Result<PathBuf>;

    /// Update the current room ID (e.g., hex UUID string).
    ///
    /// Called by the backend when the user joins or changes rooms so that
    /// uploads are tagged with the correct room. The default is a no-op
    /// (plugins that don't need room context can ignore it).
    fn set_room_id(&self, _room_id: String) {
        // Default: no-op
    }

    /// Handle an incoming server-initiated stream dispatched by the stream dispatch task.
    ///
    /// Called when the server opens a bi-directional stream with a `StreamHeader`
    /// matching this plugin. The `StreamHeader` has already been consumed from the
    /// recv stream; the remaining bytes are plugin-specific payload.
    ///
    /// The default implementation drops the streams (no-op).
    async fn on_incoming_stream(&self, _send: Box<dyn BiSendStream>, _recv: Box<dyn BiRecvStream>) {
        // Default: no-op
    }
}
