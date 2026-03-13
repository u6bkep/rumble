//! Optional file transfer capability.

use std::path::PathBuf;

/// Unique identifier for a file transfer (hex-encoded infohash for BitTorrent).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransferId(pub String);

/// Metadata about a file being offered for transfer.
#[derive(Debug, Clone)]
pub struct FileOffer {
    pub id: TransferId,
    pub name: String,
    pub size: u64,
    pub mime: String,
    /// Opaque data to share with recipients (e.g., magnet link).
    pub share_data: String,
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
    /// Raw infohash bytes (20 bytes for BitTorrent).
    pub infohash: [u8; 20],
    pub name: String,
    pub size: u64,
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
    /// Magnet link for this transfer.
    pub magnet: Option<String>,
    /// Local file path if available.
    pub local_path: Option<PathBuf>,
    /// Per-peer details.
    pub peer_details: Vec<PluginPeerInfo>,
}

/// Optional file transfer capability, injected into BackendHandle.
///
/// Not part of `Platform` — different deployments can use different
/// strategies (BitTorrent, direct transfer, etc.) or disable file
/// transfer entirely.
///
/// Methods are synchronous. Implementations running inside a tokio runtime
/// should use `tokio::task::block_in_place` + `Handle::current().block_on()`
/// for async operations.
pub trait FileTransferPlugin: Send + Sync + 'static {
    /// Share a local file and return metadata for recipients.
    fn share(&self, path: PathBuf) -> anyhow::Result<FileOffer>;

    /// Begin downloading a file from a magnet link (or other opaque share data).
    fn download(&self, magnet: &str) -> anyhow::Result<TransferId>;

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

    /// Get the local file path for a transfer (by hex infohash).
    fn get_file_path(&self, id: &TransferId) -> anyhow::Result<PathBuf>;
}
