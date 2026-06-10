//! Optional file transfer capability.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::transport::{BiRecvStream, BiSendStream};

/// Client-side maximum upload size, mirroring the server's default
/// `RelayCacheConfig::max_file_size`. Used for fast-failing oversized
/// shares before any bytes hit the wire. Servers configured higher
/// than this still work; lower-configured servers will reject the
/// upload downstream with their own error.
pub const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Severity level for plugin-emitted toast events. Mirrors
/// `rumble_client::NotificationLevel` so the traits crate doesn't need a
/// reverse dependency on `rumble-client`. Consumers map this onto their
/// own toast severity type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginNotificationLevel {
    Info,
    Warn,
    Error,
}

/// One-way event from a [`FileTransferPlugin`] back to the host
/// (the `rumble-client` connection task, which forwards onto the
/// UI-facing `BackendEvent` channel).
///
/// Two flavours: user-visible notifications (toasts) and structural
/// lifecycle transitions. The latter is what makes the UI's media
/// cache event-driven — instead of polling `plugin.transfers()` every
/// frame to spot newly-completed transfers, listeners react to a
/// single `TransferStageChanged` per actual transition.
///
/// Progress updates within `Active` deliberately don't fire events.
/// The smoothed bytes-per-second sample re-bases every 250ms and the
/// fraction changes continuously — flooding the channel would cost
/// more than the UI saves by getting incremental updates. Progress
/// bars keep reading `plugin.transfers()` directly.
#[derive(Debug, Clone)]
pub enum PluginEvent {
    /// User-visible notification (upload rejected, duplicate share,
    /// etc.). The host wraps this in a toast.
    Notification {
        level: PluginNotificationLevel,
        text: String,
    },
    /// The named transfer just transitioned to a new `TransferStage`.
    /// Emitted on every variant change — including the initial
    /// transition into `Active` (or pre-flight `Failed`) at creation
    /// time — but NOT on intra-`Active` progress mutations.
    TransferStageChanged {
        id: TransferId,
        direction: TransferDirection,
        /// File name, threaded through so listeners that surface a
        /// human label (toast, log line) don't have to re-look it up
        /// from a separate transfer snapshot.
        name: String,
        stage: TransferStage,
    },
}

/// Construction-time callback the host passes to a plugin so the
/// plugin can emit [`PluginEvent`]s back. `Arc<dyn Fn>` keeps the
/// plugin trait object-safe and lets the sink be cloned into spawned
/// tasks (`run_upload`, `run_fetch`).
pub type PluginEventSink = std::sync::Arc<dyn Fn(PluginEvent) + Send + Sync>;

/// Live-updatable transfer speed limits shared between the host and a
/// [`FileTransferPlugin`].
///
/// The host (e.g. `rumble-client`'s connection task) keeps one of these
/// for the lifetime of the process and hands clones to each plugin it
/// constructs; updating the limits through any clone is immediately
/// visible to in-flight transfers, which re-read the values on every
/// throttle tick.
///
/// Both limits are in **bytes per second**; `0` means unlimited.
#[derive(Clone, Debug, Default)]
pub struct TransferSpeedLimits {
    inner: std::sync::Arc<SpeedLimitsInner>,
}

#[derive(Debug, Default)]
struct SpeedLimitsInner {
    download_bps: std::sync::atomic::AtomicU64,
    upload_bps: std::sync::atomic::AtomicU64,
}

impl TransferSpeedLimits {
    /// Create limits with initial values (bytes/sec; `0` = unlimited).
    pub fn new(download_bps: u64, upload_bps: u64) -> Self {
        let limits = Self::default();
        limits.set(download_bps, upload_bps);
        limits
    }

    /// Current download (receive) limit in bytes/sec; `0` = unlimited.
    pub fn download_bps(&self) -> u64 {
        self.inner.download_bps.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Current upload (send) limit in bytes/sec; `0` = unlimited.
    pub fn upload_bps(&self) -> u64 {
        self.inner.upload_bps.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Update both limits (bytes/sec; `0` = unlimited). Takes effect on
    /// the next throttle tick of any in-flight transfer.
    pub fn set(&self, download_bps: u64, upload_bps: u64) {
        self.inner
            .download_bps
            .store(download_bps, std::sync::atomic::Ordering::Relaxed);
        self.inner
            .upload_bps
            .store(upload_bps, std::sync::atomic::Ordering::Relaxed);
    }
}

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

/// Lifecycle stage of a transfer. Replaces the prior trio of
/// `state` / `is_finished` / `error` fields — those were partially
/// redundant and let consumers gate on different signals, which is
/// how an oversized upload could lose its error card the moment a
/// thumbnail finished decoding. An enum makes the inconsistent
/// combinations unrepresentable.
///
/// State-dependent payloads live inside the variant: progress only
/// makes sense while a transfer is moving, `local_path` only after
/// it finishes successfully, `reason` only on failure.
#[derive(Debug, Clone)]
pub enum TransferStage {
    /// Transfer is moving bytes (or about to). `speed_bps` is the
    /// smoothed throughput in the active direction; `0` before the
    /// first sample window elapses.
    Active { progress: f32, speed_bps: u64 },
    /// Transfer is suspended at `progress`. No bytes flowing.
    Paused { progress: f32 },
    /// Transfer completed and the file is at `local_path`. Used for
    /// both successful uploads (sender's source file) and successful
    /// downloads (newly written file).
    Done { local_path: PathBuf },
    /// Transfer failed. `reason` is the human-readable error already
    /// fit for display on the chat failure card.
    Failed { reason: String },
}

impl TransferStage {
    /// Progress in `[0.0, 1.0]`. `Done` reports `1.0`, `Failed` reports
    /// `0.0`, `Paused`/`Active` report their stored fraction.
    pub fn progress(&self) -> f32 {
        match self {
            Self::Active { progress, .. } | Self::Paused { progress } => progress.clamp(0.0, 1.0),
            Self::Done { .. } => 1.0,
            Self::Failed { .. } => 0.0,
        }
    }

    /// True for transfers that won't change stage further — `Done` or
    /// `Failed`. Replaces the old `is_finished` field; consumers
    /// gating "should I act on this completed transfer?" should also
    /// check it's `Done` specifically, since `Failed` is terminal too.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Failed { .. })
    }
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

/// Status of a file transfer in progress. The lifecycle stage and its
/// per-stage payload (progress, error, completed path) live inside
/// [`TransferStage`]; consumers `match` on it rather than checking
/// `is_finished` against `error` against `state` and hoping they agree.
#[derive(Debug, Clone)]
pub struct TransferStatus {
    pub id: TransferId,
    pub name: String,
    pub size: u64,
    /// Upload vs download from the local client's perspective. Orthogonal
    /// to stage (an upload can be Active, Done, or Failed; same for a
    /// download).
    pub direction: TransferDirection,
    /// Lifecycle stage with stage-specific payload.
    pub stage: TransferStage,
    /// Number of connected peers (relay plugin always reports `0`).
    pub peers: u32,
    /// Per-peer details.
    pub peer_details: Vec<PluginPeerInfo>,
}

impl TransferStatus {
    /// Convenience: the `local_path` from a [`TransferStage::Done`]
    /// stage, or `None` for any other stage. Used by code that only
    /// cares about completed transfers' files (file-open buttons,
    /// preview decoders).
    pub fn done_path(&self) -> Option<&PathBuf> {
        match &self.stage {
            TransferStage::Done { local_path } => Some(local_path),
            _ => None,
        }
    }
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
    /// Reverse-DNS plugin identifier (e.g. "rumble.file_transfer.relay").
    /// Used as the `ChatAttachment.namespace` for messages this plugin
    /// produces; receiving clients look this up in their renderer
    /// registry to find a matching handler.
    fn namespace(&self) -> &'static str;

    /// Share a local file and return metadata for recipients.
    fn share(&self, path: PathBuf) -> anyhow::Result<FileOffer>;

    /// Encode a `FileOffer` (from `share`) into the `ChatAttachment`
    /// payload format this plugin uses on the wire. The plugin owns its
    /// payload schema; callers just take the bytes and embed them.
    fn encode_attachment(&self, offer: &FileOffer) -> rumble_protocol::ChatAttachment;

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
