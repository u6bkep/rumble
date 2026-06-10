//! Relay-based file transfer plugin for native clients.
//!
//! Implements [`FileTransferPlugin`] using the server's store-and-serve cache.
//!
//! ## Upload flow
//!
//! 1. `share(path)` reads file metadata, generates a transfer_id, and spawns an
//!    upload task that opens a `"file-relay"` stream and sends the file to the
//!    server's cache.
//! 2. Returns a [`FileOffer`] whose `share_data` is JSON-encoded [`RelayShareData`].
//!
//! ## Download (fetch) flow
//!
//! 1. `download(share_data)` parses the [`RelayShareData`], spawns a fetch task
//!    that opens a `"file-relay"` stream and requests the file from the server.
//! 2. The fetched data is written to the downloads directory.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use async_trait::async_trait;
use prost::Message;
use rumble_client_traits::{
    file_transfer::{
        FileOffer, FileTransferPlugin, PluginEvent, PluginEventSink, PluginNotificationLevel, TransferDirection,
        TransferId, TransferSpeedLimits, TransferStage, TransferStatus,
    },
    transport::{
        BiRecvStream, BiSendStream, PluginAck, StreamOpener, read_length_prefixed, read_plugin_ack,
        write_length_prefixed,
    },
};
use rumble_protocol::proto;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Serialized into `FileOffer.share_data` so recipients know this is a relay
/// offer and can call `download()` with it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct RelayShareData {
    pub transfer_id: String,
    pub file_name: String,
    pub file_size: u64,
    pub mime: String,
}

/// Encode a `FileOffer` into the relay plugin's ChatAttachment payload.
/// The payload schema is `proto::RelayFileSharePayload` (defined in
/// api.proto for build convenience but semantically owned by this plugin).
pub fn encode_payload(offer: &FileOffer) -> Vec<u8> {
    let msg = proto::RelayFileSharePayload {
        transfer_id: offer.id.0.clone(),
        name: offer.name.clone(),
        size: offer.size,
        mime: offer.mime.clone(),
        share_data: offer.share_data.clone(),
    };
    msg.encode_to_vec()
}

/// Decode a ChatAttachment payload produced by `encode_payload`. Returns
/// `None` if the payload is malformed.
pub fn decode_payload(payload: &[u8]) -> Option<proto::RelayFileSharePayload> {
    proto::RelayFileSharePayload::decode(payload).ok()
}

/// Current schema version for the relay payload format.
pub const PAYLOAD_SCHEMA_VERSION: u32 = 1;

/// Internal state for a pending or active transfer. The stage enum
/// is the single source of truth for what's going on — separate
/// `is_complete`/`error`/`progress` fields were what let the original
/// bug create contradictory states ("complete AND errored").
struct TransferEntry {
    id: TransferId,
    name: String,
    size: u64,
    mime: String,
    /// Opaque share_data string (uploads only). Cached so a duplicate
    /// in-flight share can return the existing offer instead of
    /// re-uploading.
    share_data: Option<String>,
    /// Room ID the upload was tagged with at start. Used to detect
    /// in-flight uploads that need to be cancelled when the user
    /// switches rooms.
    room_id: Option<String>,
    direction: TransferDirection,
    stage: TransferStage,
    /// Original on-disk path being uploaded (uploads only; `None` for
    /// downloads). Kept separate from `stage` so the duplicate-in-flight
    /// check can find an Active upload by path without waiting for the
    /// transfer to land in `Done`. On successful completion this same
    /// path also lands in `stage = Done { local_path }`.
    source_path: Option<PathBuf>,
    /// Cancellation token for this transfer's task.
    cancel: CancellationToken,
    /// Bytes transferred at the last speed sample. Combined with
    /// `last_sample_at` to compute a fresh `speed_bps`.
    last_sample_bytes: u64,
    /// Wall-clock instant of the last speed sample.
    last_sample_at: Instant,
    /// Destination path being written by an active or completed download.
    /// Set by the fetch task before the first byte is written, so
    /// `run_fetch` can delete any partial file on any non-success exit
    /// (including task cancellation). Always `None` for uploads.
    dest_path: Option<PathBuf>,
    /// Instructs the fetch task to remove `dest_path` even on a
    /// successful download completion. Set by `cancel(delete_files = true)`
    /// so the task — the sole writer of downloaded files — can honor the
    /// deletion request without racing the on-disk write. Always `false`
    /// for uploads.
    delete_on_complete: bool,
}

impl TransferEntry {
    fn to_status(&self) -> TransferStatus {
        TransferStatus {
            id: self.id.clone(),
            name: self.name.clone(),
            size: self.size,
            direction: self.direction,
            stage: self.stage.clone(),
            peers: 0,
            peer_details: Vec::new(),
        }
    }
}

/// Pick a destination path that won't clobber an existing file. If
/// `dir/name` is free, returns it; otherwise inserts ` (N)` before the
/// extension and increments `N` until the resulting path is free
/// (capped to keep the loop bounded against pathological dirs).
///
/// Examples:
/// - `dir/foo.png` taken → `dir/foo (2).png`
/// - `dir/foo (2).png` also taken → `dir/foo (3).png`
/// - `dir/no-extension` taken → `dir/no-extension (2)`
fn unique_dest_path(dir: &std::path::Path, file_name: &str) -> PathBuf {
    let initial = dir.join(file_name);
    if !initial.exists() {
        return initial;
    }
    let path = std::path::Path::new(file_name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(file_name);
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 2..10_000u32 {
        let candidate_name = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = dir.join(&candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    initial
}

/// Update an `Active` entry's progress + smoothed bytes-per-second
/// sample. Re-baselines when at least 250 ms has elapsed, which keeps
/// the displayed speed steady at high data rates without making
/// low-rate transfers look stalled. No-op if the entry isn't `Active`
/// — a transfer that's already terminal shouldn't be having its
/// progress overwritten by a stale in-flight write. Caller already
/// holds the transfers map lock.
fn update_active_progress(entry: &mut TransferEntry, bytes_so_far: u64, total_size: u64) {
    let TransferStage::Active { progress, speed_bps } = &mut entry.stage else {
        return;
    };
    *progress = if total_size > 0 {
        (bytes_so_far as f32) / (total_size as f32)
    } else {
        1.0
    };
    let now = Instant::now();
    let elapsed = now.duration_since(entry.last_sample_at);
    if elapsed.as_millis() < 250 {
        return;
    }
    let delta = bytes_so_far.saturating_sub(entry.last_sample_bytes);
    let secs = elapsed.as_secs_f64().max(0.001);
    *speed_bps = (delta as f64 / secs) as u64;
    entry.last_sample_bytes = bytes_so_far;
    entry.last_sample_at = now;
}

type TransferMap = Arc<parking_lot::Mutex<HashMap<String, TransferEntry>>>;

/// Token-bucket throttle for the transfer loops.
///
/// One bucket per transfer task, fed by the live limit from
/// [`TransferSpeedLimits`] on every call — so a settings change applies
/// to in-flight transfers at their next chunk. The balance may go
/// negative ("debt") when a chunk exceeds the remaining budget; the
/// caller then sleeps until the debt is repaid, which means chunks
/// larger than the bucket capacity still make progress instead of
/// stalling forever. Burst budget is capped at ~1 second of allowance.
struct TokenBucket {
    /// Token balance in bytes. Negative = debt to sleep off.
    tokens: f64,
    /// Instant of the last refill computation.
    last_refill: Instant,
}

impl TokenBucket {
    fn new() -> Self {
        Self {
            tokens: 0.0,
            last_refill: Instant::now(),
        }
    }

    /// Account for `bytes` just transferred under `limit_bps` and return
    /// how long the caller should sleep before moving the next chunk.
    /// `limit_bps == 0` means unlimited: no sleep, and the bucket state
    /// is reset so a limit applied mid-transfer starts from a clean
    /// baseline instead of a stale balance.
    fn consume(&mut self, bytes: u64, limit_bps: u64, now: Instant) -> Duration {
        if limit_bps == 0 {
            self.tokens = 0.0;
            self.last_refill = now;
            return Duration::ZERO;
        }
        let limit = limit_bps as f64;
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        // Refill at the limit rate, capping the burst budget at ~1s of
        // allowance so a long idle gap can't bank unlimited credit.
        self.tokens = (self.tokens + elapsed * limit).min(limit);
        self.last_refill = now;
        self.tokens -= bytes as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / limit)
        }
    }

    /// Async wrapper: consume and sleep off any debt. Cancellation-safe —
    /// callers run inside a `tokio::select!` with the transfer's
    /// cancellation token, which aborts the sleep along with the I/O.
    async fn throttle(&mut self, bytes: u64, limit_bps: u64) {
        let wait = self.consume(bytes, limit_bps, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// Relay-based [`FileTransferPlugin`] using the server's store-and-serve cache.
///
/// Construct with [`FileTransferRelayPlugin::new`], passing a [`StreamOpener`]
/// and a downloads directory.
pub struct FileTransferRelayPlugin {
    opener: Arc<dyn StreamOpener>,
    downloads_dir: PathBuf,
    transfers: TransferMap,
    /// Room ID (hex UUID string) for the current room. Updated externally.
    room_id: parking_lot::Mutex<String>,
    /// Optional sink for surfacing user-visible toasts (rejection,
    /// duplicate share, room-change cancel, etc.). Set to `None` for
    /// tests or environments without a UI.
    event_sink: Option<PluginEventSink>,
    /// Shared, live-updatable bandwidth caps (bytes/sec, 0 = unlimited).
    /// Upload/fetch loops re-read these on every chunk, so settings
    /// changes apply to in-flight transfers.
    speed_limits: TransferSpeedLimits,
}

impl FileTransferRelayPlugin {
    /// Create a new relay file transfer plugin.
    ///
    /// - `opener`: Transport-agnostic stream opener for opening streams to the server.
    /// - `downloads_dir`: Directory where fetched files are saved.
    /// - `event_sink`: Optional callback for user-visible toast events.
    /// - `speed_limits`: Live-updatable bandwidth caps enforced by the
    ///   upload/fetch loops (bytes/sec, 0 = unlimited).
    pub fn new(
        opener: Arc<dyn StreamOpener>,
        downloads_dir: PathBuf,
        event_sink: Option<PluginEventSink>,
        speed_limits: TransferSpeedLimits,
    ) -> Self {
        Self {
            opener,
            downloads_dir,
            transfers: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            room_id: parking_lot::Mutex::new(String::new()),
            event_sink,
            speed_limits,
        }
    }

    fn emit_notification(&self, level: PluginNotificationLevel, msg: impl Into<String>) {
        emit_notification(self.event_sink.as_ref(), level, msg);
    }
}

/// Module-level emitters so spawned tasks (`run_upload`, `run_fetch`)
/// that don't hold `&Self` can still push events through a cloned sink.
fn emit_notification(sink: Option<&PluginEventSink>, level: PluginNotificationLevel, msg: impl Into<String>) {
    if let Some(s) = sink {
        s(PluginEvent::Notification {
            level,
            text: msg.into(),
        });
    }
}

/// Push a `TransferStageChanged` for `entry`'s current stage. Call
/// this after every assignment to `entry.stage` — the helper centralises
/// the field-by-field copy so the relay code's mutation sites stay
/// readable and don't drift from the event payload schema.
fn emit_stage(sink: Option<&PluginEventSink>, entry: &TransferEntry) {
    if let Some(s) = sink {
        s(PluginEvent::TransferStageChanged {
            id: entry.id.clone(),
            direction: entry.direction,
            name: entry.name.clone(),
            stage: entry.stage.clone(),
        });
    }
}

/// Write a stream header for the "file-relay" plugin.
async fn write_file_relay_header(send: &mut dyn BiSendStream) -> Result<()> {
    let header = rumble_client_traits::StreamHeader {
        plugin: "file-relay".to_owned(),
    };
    header.write_to(send).await
}

/// Validates byte counts during a relay fetch.
///
/// Returns `Err` if either:
/// - `file_size` exceeds [`rumble_client_traits::MAX_UPLOAD_BYTES`] (the
///   absolute download ceiling, which mirrors the server relay cap of 256 MiB).
///   A server that declares a larger file size is lying or misconfigured; we
///   refuse before writing a single byte.
/// - `received` exceeds `file_size` (protocol violation: the server streamed
///   more data than it declared).
///
/// Call once before the fetch loop with `received = 0` to fast-fail oversized
/// declarations, then again inside the loop after each `received += n`.
fn validate_fetch_progress(received: u64, file_size: u64) -> anyhow::Result<()> {
    if file_size > rumble_client_traits::MAX_UPLOAD_BYTES {
        anyhow::bail!(
            "server-declared file size ({file_size} bytes) exceeds the maximum allowed {} bytes (mirrors the server \
             relay cap); refusing download",
            rumble_client_traits::MAX_UPLOAD_BYTES,
        );
    }
    if received > file_size {
        anyhow::bail!("server sent more data than declared ({received} > {file_size} bytes): protocol violation");
    }
    Ok(())
}

/// Upload a file to the server's relay cache.
#[allow(clippy::too_many_arguments)]
async fn run_upload(
    opener: Arc<dyn StreamOpener>,
    transfer_id: String,
    room_id: String,
    file_name: String,
    file_size: u64,
    mime: String,
    path: PathBuf,
    transfers: TransferMap,
    cancel: CancellationToken,
    event_sink: Option<PluginEventSink>,
    speed_limits: TransferSpeedLimits,
) {
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
        r = do_upload(
            &opener, &transfer_id, &room_id, &file_name, file_size, &mime, &path, &transfers, &speed_limits,
        ) => r,
    };

    let mut t = transfers.lock();
    let Some(entry) = t.get_mut(&transfer_id) else {
        return;
    };
    // Terminal stages are sticky: once set externally (e.g., cancel() or
    // set_room_id()), the task must not overwrite them. This prevents
    // `Failed{"room changed"}` from being clobbered by `Failed{"cancelled"}`
    // and stops a long-completed task from reviving a Done/Failed entry.
    let already_terminal = entry.stage.is_terminal();
    if already_terminal {
        return;
    }
    match result {
        Ok(()) => {
            entry.stage = TransferStage::Done {
                local_path: path.clone(),
            };
            emit_stage(event_sink.as_ref(), entry);
            info!(transfer_id, "relay upload complete");
        }
        Err(e) => {
            let msg = e.to_string();
            // Cancellation is expected (user navigated away, room
            // change, explicit cancel); don't toast for those — the
            // failure card carries enough signal already.
            let cancelled = cancel.is_cancelled() || msg == "cancelled";
            entry.stage = TransferStage::Failed { reason: msg.clone() };
            emit_stage(event_sink.as_ref(), entry);
            drop(t);
            warn!(transfer_id, error = %msg, "relay upload failed");
            if !cancelled {
                emit_notification(
                    event_sink.as_ref(),
                    PluginNotificationLevel::Error,
                    format!("Upload failed: {msg}"),
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn do_upload(
    opener: &Arc<dyn StreamOpener>,
    transfer_id: &str,
    room_id: &str,
    file_name: &str,
    file_size: u64,
    mime: &str,
    path: &std::path::Path,
    transfers: &TransferMap,
    speed_limits: &TransferSpeedLimits,
) -> Result<()> {
    let (mut send, mut recv) = opener.open_bi().await?;

    // --- Phase 1: send request metadata. ---
    write_file_relay_header(&mut *send).await?;
    send.write_all(&[0x01]).await?; // upload type

    let upload_msg = proto::RelayUpload {
        transfer_id: transfer_id.to_owned(),
        room_id: room_id.to_owned(),
        file_name: file_name.to_owned(),
        file_size,
        mime: mime.to_owned(),
    };
    write_length_prefixed(&mut *send, &upload_msg.encode_to_vec()).await?;

    // --- Phase 2: wait for admission ack before streaming the body. ---
    // Reading the ack *before* writing any body bytes is the whole point of
    // the phased handshake: it lets the server reject the request (size,
    // quota, ACL) without us shipping the file across the wire.
    match read_plugin_ack(&mut *recv).await? {
        PluginAck::Ok => {}
        PluginAck::Rejected { code, error } => {
            // The reason string is what the user sees on the failure
            // card; the numeric code is dev/ops detail and stays in the
            // log. Don't glue them together — the UI surface already
            // labels this as an upload failure.
            warn!(transfer_id, code, error = %error, "server rejected upload");
            anyhow::bail!("{error}");
        }
    }

    // --- Phase 3: stream body. ---
    let file = tokio::fs::File::open(path).await?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut buf = vec![0u8; 64 * 1024];
    let mut sent: u64 = 0;
    let mut bucket = TokenBucket::new();

    loop {
        let n = tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
        sent += n as u64;

        // Update progress + speed sample.
        {
            let mut t = transfers.lock();
            if let Some(entry) = t.get_mut(transfer_id) {
                update_active_progress(entry, sent, file_size);
            }
        }

        // Enforce the user's upload cap. Re-reads the live limit each
        // chunk so a settings change applies mid-transfer; 0 = unlimited.
        bucket.throttle(n as u64, speed_limits.upload_bps()).await;
    }

    // Signal end of upload data.
    send.finish().await?;

    // --- Phase 4: read completion response. ---
    let resp_bytes = read_length_prefixed(&mut *recv).await?;
    let resp = proto::RelayUploadResponse::decode(&resp_bytes[..])?;

    let status = proto::RelayResult::try_from(resp.status).unwrap_or(proto::RelayResult::Error);
    if status != proto::RelayResult::Ok {
        anyhow::bail!("upload rejected: {} ({})", resp.error, status.as_str_name());
    }

    Ok(())
}

/// Fetch a file from the server's relay cache.
#[allow(clippy::too_many_arguments)]
async fn run_fetch(
    opener: Arc<dyn StreamOpener>,
    transfer_id: String,
    downloads_dir: PathBuf,
    transfers: TransferMap,
    cancel: CancellationToken,
    event_sink: Option<PluginEventSink>,
    speed_limits: TransferSpeedLimits,
) {
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
        r = do_fetch(&opener, &transfer_id, &downloads_dir, &transfers, &speed_limits) => r,
    };
    // By the time we reach here the do_fetch future (and any BufWriter/File
    // it held) has either returned or been dropped by the select. The file
    // handle is closed before any remove_file call below.

    let mut t = transfers.lock();

    // Read cleanup fields under the lock so cancel() cannot race our read.
    let (partial_path, delete_on_complete) = t
        .get(&transfer_id)
        .map(|e| (e.dest_path.clone(), e.delete_on_complete))
        .unwrap_or((None, false));

    let Some(entry) = t.get_mut(&transfer_id) else {
        // Entry was removed concurrently (shouldn't happen in practice).
        drop(t);
        if let Some(path) = partial_path {
            let _ = std::fs::remove_file(&path);
        }
        return;
    };

    // Terminal stages are sticky: once set externally (e.g., cancel()),
    // the task must not overwrite them. The file must still be cleaned up.
    let already_terminal = entry.stage.is_terminal();
    if already_terminal {
        drop(t);
        // task won the I/O race but cancel() won the stage race.
        // For errors the partial file is always removed; for an Ok result
        // the caller's delete_on_complete preference governs.
        let should_delete = match &result {
            Ok(_) => delete_on_complete,
            Err(_) => true,
        };
        if should_delete && let Some(path) = partial_path {
            let _ = std::fs::remove_file(&path);
        }
        return;
    }

    match result {
        Ok(dest) => {
            if delete_on_complete {
                // cancel(delete_files=true) was called concurrently and the
                // stage hadn't gone terminal yet; honour the deletion request.
                entry.stage = TransferStage::Failed {
                    reason: "cancelled".to_owned(),
                };
                emit_stage(event_sink.as_ref(), entry);
                drop(t);
                let _ = std::fs::remove_file(&dest);
            } else {
                entry.stage = TransferStage::Done {
                    local_path: dest.clone(),
                };
                emit_stage(event_sink.as_ref(), entry);
                drop(t);
                info!(transfer_id, dest = %dest.display(), "relay fetch complete");
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let cancelled = cancel.is_cancelled() || msg == "cancelled";
            entry.stage = TransferStage::Failed { reason: msg.clone() };
            emit_stage(event_sink.as_ref(), entry);
            drop(t);
            warn!(transfer_id, error = %msg, "relay fetch failed");
            if !cancelled {
                emit_notification(
                    event_sink.as_ref(),
                    PluginNotificationLevel::Error,
                    format!("Download failed: {msg}"),
                );
            }
            // Always remove partial/failed downloads — a partial file has no
            // value and would confuse the user.
            if let Some(path) = partial_path {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

async fn do_fetch(
    opener: &Arc<dyn StreamOpener>,
    transfer_id: &str,
    downloads_dir: &std::path::Path,
    transfers: &TransferMap,
    speed_limits: &TransferSpeedLimits,
) -> Result<PathBuf> {
    let (mut send, mut recv) = opener.open_bi().await?;

    // Write StreamHeader + type discriminator + RelayFetch.
    write_file_relay_header(&mut *send).await?;
    send.write_all(&[0x02]).await?; // fetch type

    let fetch_msg = proto::RelayFetch {
        transfer_id: transfer_id.to_owned(),
    };
    write_length_prefixed(&mut *send, &fetch_msg.encode_to_vec()).await?;

    // We don't send any more data on the upload direction.
    send.finish().await?;

    // Read server's response header.
    let resp_bytes = read_length_prefixed(&mut *recv).await?;
    let resp = proto::RelayFetchResponse::decode(&resp_bytes[..])?;

    let status = proto::RelayResult::try_from(resp.status).unwrap_or(proto::RelayResult::Error);
    if status != proto::RelayResult::Ok {
        anyhow::bail!("fetch failed: {} ({})", resp.error, status.as_str_name());
    }

    // Sanitize file name to prevent path traversal.
    let file_name = std::path::Path::new(&resp.file_name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_owned());
    let file_size = resp.file_size;

    // Reject the transfer immediately if the server declares a file size
    // that exceeds the absolute ceiling. This catches both misconfigured
    // servers and hostile peers before any disk space is consumed.
    validate_fetch_progress(0, file_size)?;

    // Write to downloads directory. Auto-rename on collision so a
    // re-download of the same name doesn't silently overwrite an
    // earlier copy the user might still want.
    std::fs::create_dir_all(downloads_dir)?;
    let dest = unique_dest_path(downloads_dir, &file_name);

    // Update transfer metadata and record the destination path under one
    // lock. dest_path must be set before File::create so that run_fetch
    // can clean up any partial file if the task is cancelled after this
    // point.
    {
        let mut t = transfers.lock();
        if let Some(entry) = t.get_mut(transfer_id) {
            entry.name = file_name.clone();
            entry.size = file_size;
            entry.dest_path = Some(dest.clone());
        }
    }

    let file = tokio::fs::File::create(&dest).await?;
    let mut writer = tokio::io::BufWriter::new(file);
    let mut buf = vec![0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut bucket = TokenBucket::new();

    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) if n > 0 => {
                tokio::io::AsyncWriteExt::write_all(&mut writer, &buf[..n]).await?;
                received += n as u64;

                // Enforce size cap inside the loop. Catches a server that
                // streams past its declared size (exact overrun = protocol
                // violation) and guards against a declared-size lie via the
                // absolute ceiling checked in validate_fetch_progress.
                validate_fetch_progress(received, file_size)?;

                {
                    let mut t = transfers.lock();
                    if let Some(entry) = t.get_mut(transfer_id) {
                        update_active_progress(entry, received, file_size);
                    }
                }

                // Enforce the user's download cap by pacing our reads —
                // QUIC stream flow control pushes the backpressure to the
                // server. Re-reads the live limit each chunk; 0 = unlimited.
                bucket.throttle(n as u64, speed_limits.download_bps()).await;
            }
            Ok(Some(_)) => continue, // zero-length read
            Ok(None) => break,       // stream closed
            Err(e) => return Err(anyhow::anyhow!("recv error: {e}")),
        }
    }

    if received != file_size {
        anyhow::bail!("incomplete download: received {received} of {file_size} bytes");
    }

    tokio::io::AsyncWriteExt::flush(&mut writer).await?;

    Ok(dest)
}

/// Reverse-DNS namespace for this plugin's chat attachments. Public so
/// the damascene renderer can match against it without depending on the
/// concrete plugin type.
pub const NAMESPACE: &str = "rumble.file_transfer.relay";

#[async_trait]
impl FileTransferPlugin for FileTransferRelayPlugin {
    fn namespace(&self) -> &'static str {
        NAMESPACE
    }

    fn encode_attachment(&self, offer: &FileOffer) -> rumble_protocol::ChatAttachment {
        rumble_protocol::ChatAttachment {
            namespace: NAMESPACE.to_string(),
            schema_version: PAYLOAD_SCHEMA_VERSION,
            payload: encode_payload(offer),
            // The only thing non-relay-aware clients see; keep it human.
            fallback_text: format!("shared file \"{}\" ({})", offer.name, format_bytes_simple(offer.size)),
        }
    }

    fn set_room_id(&self, room_id: String) {
        let new_room = room_id.clone();
        let old_room = {
            let mut guard = self.room_id.lock();
            let old = guard.clone();
            *guard = room_id;
            old
        };
        if old_room == new_room {
            return;
        }

        // Cancel any in-flight uploads still tagged with the previous
        // room — receivers in the old room won't see the offer broadcast
        // anyway, and the bytes shouldn't keep flowing.
        let cancelled = {
            let mut t = self.transfers.lock();
            let mut n = 0;
            for entry in t.values_mut() {
                if entry.direction == TransferDirection::Upload
                    && matches!(entry.stage, TransferStage::Active { .. } | TransferStage::Paused { .. })
                    && entry.room_id.as_deref() == Some(old_room.as_str())
                {
                    entry.cancel.cancel();
                    entry.stage = TransferStage::Failed {
                        reason: "room changed".to_owned(),
                    };
                    emit_stage(self.event_sink.as_ref(), entry);
                    n += 1;
                }
            }
            n
        };

        if cancelled > 0 {
            let noun = if cancelled == 1 { "upload" } else { "uploads" };
            self.emit_notification(
                PluginNotificationLevel::Warn,
                format!("{cancelled} {noun} cancelled due to room change"),
            );
        }
    }

    fn share(&self, path: PathBuf) -> Result<FileOffer> {
        let metadata = std::fs::metadata(&path)?;
        let file_size = metadata.len();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_owned());
        let mime = mime_guess::from_path(&file_name).first_or_octet_stream().to_string();

        if file_size > rumble_client_traits::MAX_UPLOAD_BYTES {
            // Pre-flight failure: create a synthetic Failed entry in the
            // transfers table so the UI's chat card can read the failure
            // from TransferStatus the same way it would for any other
            // upload error. The plugin is the single source of truth for
            // transfer state, including pre-flight rejections.
            let transfer_id = uuid::Uuid::new_v4().to_string();
            let reason = format!(
                "file too large ({}); limit is {}",
                format_bytes_simple(file_size),
                format_bytes_simple(rumble_client_traits::MAX_UPLOAD_BYTES),
            );
            let entry = TransferEntry {
                id: TransferId(transfer_id.clone()),
                name: file_name.clone(),
                size: file_size,
                mime: mime.clone(),
                share_data: None,
                room_id: None,
                direction: TransferDirection::Upload,
                stage: TransferStage::Failed { reason },
                source_path: None,
                cancel: CancellationToken::new(),
                last_sample_bytes: 0,
                last_sample_at: Instant::now(),
                dest_path: None,
                delete_on_complete: false,
            };
            {
                let mut t = self.transfers.lock();
                t.insert(transfer_id.clone(), entry);
                emit_stage(self.event_sink.as_ref(), t.get(&transfer_id).expect("just inserted"));
            }
            return Ok(FileOffer {
                id: TransferId(transfer_id),
                name: file_name,
                size: file_size,
                mime,
                share_data: String::new(),
            });
        }

        // Duplicate-in-flight detection: if the same path is already
        // uploading (Active or Paused), return the existing FileOffer
        // instead of re-uploading. This dedups e.g. double-click on the
        // share button or a paste while the previous paste is still in
        // flight.
        {
            let t = self.transfers.lock();
            if let Some(existing) = t.values().find(|e| {
                e.direction == TransferDirection::Upload
                    && matches!(e.stage, TransferStage::Active { .. } | TransferStage::Paused { .. })
                    && e.source_path.as_deref() == Some(path.as_path())
            }) && let Some(share_data) = existing.share_data.clone()
            {
                self.emit_notification(
                    PluginNotificationLevel::Warn,
                    format!("Already uploading \"{}\"", existing.name),
                );
                return Ok(FileOffer {
                    id: existing.id.clone(),
                    name: existing.name.clone(),
                    size: existing.size,
                    mime: existing.mime.clone(),
                    share_data,
                });
            }
        }

        let transfer_id = uuid::Uuid::new_v4().to_string();
        let room_id = self.room_id.lock().clone();

        let share_data = RelayShareData {
            transfer_id: transfer_id.clone(),
            file_name: file_name.clone(),
            file_size,
            mime: mime.clone(),
        };
        let share_data_json = serde_json::to_string(&share_data)?;

        let cancel = CancellationToken::new();
        let entry = TransferEntry {
            id: TransferId(transfer_id.clone()),
            name: file_name.clone(),
            size: file_size,
            mime: mime.clone(),
            share_data: Some(share_data_json.clone()),
            room_id: if room_id.is_empty() {
                None
            } else {
                Some(room_id.clone())
            },
            direction: TransferDirection::Upload,
            stage: TransferStage::Active {
                progress: 0.0,
                speed_bps: 0,
            },
            source_path: Some(path.clone()),
            cancel: cancel.clone(),
            last_sample_bytes: 0,
            last_sample_at: Instant::now(),
            dest_path: None,
            delete_on_complete: false,
        };

        {
            let mut t = self.transfers.lock();
            t.insert(transfer_id.clone(), entry);
            emit_stage(self.event_sink.as_ref(), t.get(&transfer_id).expect("just inserted"));
        }

        // Spawn upload task.
        let opener = self.opener.clone();
        let transfers = self.transfers.clone();
        let tid = transfer_id.clone();
        let sink = self.event_sink.clone();
        tokio::spawn(run_upload(
            opener,
            tid,
            room_id,
            file_name.clone(),
            file_size,
            mime.clone(),
            path,
            transfers,
            cancel,
            sink,
            self.speed_limits.clone(),
        ));

        Ok(FileOffer {
            id: TransferId(transfer_id),
            name: file_name,
            size: file_size,
            mime,
            share_data: share_data_json,
        })
    }

    fn download(&self, share_data: &str) -> Result<TransferId> {
        let relay_data: RelayShareData =
            serde_json::from_str(share_data).map_err(|e| anyhow::anyhow!("invalid relay share_data: {e}"))?;

        let transfer_id = relay_data.transfer_id.clone();

        let cancel = CancellationToken::new();
        let entry = TransferEntry {
            id: TransferId(transfer_id.clone()),
            name: relay_data.file_name,
            size: relay_data.file_size,
            mime: relay_data.mime,
            share_data: None,
            room_id: None,
            direction: TransferDirection::Download,
            stage: TransferStage::Active {
                progress: 0.0,
                speed_bps: 0,
            },
            source_path: None,
            cancel: cancel.clone(),
            last_sample_bytes: 0,
            last_sample_at: Instant::now(),
            dest_path: None,
            delete_on_complete: false,
        };

        {
            let mut t = self.transfers.lock();
            t.insert(transfer_id.clone(), entry);
            emit_stage(self.event_sink.as_ref(), t.get(&transfer_id).expect("just inserted"));
        }

        // Spawn fetch task.
        let opener = self.opener.clone();
        let downloads_dir = self.downloads_dir.clone();
        let transfers = self.transfers.clone();
        let tid = transfer_id.clone();
        let sink = self.event_sink.clone();
        tokio::spawn(run_fetch(
            opener,
            tid,
            downloads_dir,
            transfers,
            cancel,
            sink,
            self.speed_limits.clone(),
        ));

        Ok(TransferId(transfer_id))
    }

    fn transfers(&self) -> Vec<TransferStatus> {
        self.transfers.lock().values().map(|e| e.to_status()).collect()
    }

    fn pause(&self, _id: &TransferId) -> Result<()> {
        anyhow::bail!("relay transfers cannot be paused")
    }

    fn resume(&self, _id: &TransferId) -> Result<()> {
        anyhow::bail!("relay transfers cannot be resumed")
    }

    /// Cancel a transfer.
    ///
    /// `delete_files` semantics per direction:
    ///
    /// - **Active download**: the running task always removes the partial
    ///   destination file regardless of `delete_files` — partial files have
    ///   no value. `delete_files = true` additionally causes the task to
    ///   delete the file even if it finished successfully during the
    ///   cancellation race window.
    /// - **Completed download** (`Done`): if `delete_files = true`, the
    ///   destination file is removed immediately (transfer is already
    ///   terminal, no task is running).
    /// - **Upload (any stage)**: the task is cancelled; no local file is
    ///   ever deleted. The source file being uploaded is the user's own
    ///   file and must not be touched.
    fn cancel(&self, id: &TransferId, delete_files: bool) -> Result<()> {
        let mut t = self.transfers.lock();
        let Some(entry) = t.get_mut(&id.0) else {
            return Ok(());
        };

        if entry.stage.is_terminal() {
            // Transfer already finished. The only remaining action is
            // removing a completed download's file if requested.
            if delete_files
                && entry.direction == TransferDirection::Download
                && let TransferStage::Done { local_path } = &entry.stage
            {
                let path = local_path.clone();
                drop(t);
                let _ = std::fs::remove_file(&path);
            }
            return Ok(());
        }

        // Fire the cancellation token so the running task exits at its
        // next checkpoint.
        entry.cancel.cancel();

        // For downloads, record the deletion preference so the task can
        // honor it even if it finishes between now and when the token fires.
        if delete_files && entry.direction == TransferDirection::Download {
            entry.delete_on_complete = true;
        }

        // Set the stage to terminal now. This guarantees the entry can
        // never stay Active if the task has already exited between its last
        // select arm and the token check. The task's terminal block checks
        // is_terminal() before writing; a concurrent write is a no-op.
        entry.stage = TransferStage::Failed {
            reason: "cancelled".to_owned(),
        };
        emit_stage(self.event_sink.as_ref(), entry);

        Ok(())
    }

    fn get_file_path(&self, id: &TransferId) -> Result<PathBuf> {
        let t = self.transfers.lock();
        // Successful uploads keep `source_path`; successful downloads
        // land in `Done { local_path }`. Try Done first since it's the
        // standard "completed transfer" signal.
        t.get(&id.0)
            .and_then(|e| match &e.stage {
                TransferStage::Done { local_path } => Some(local_path.clone()),
                _ => e.source_path.clone(),
            })
            .ok_or_else(|| anyhow::anyhow!("no file path for transfer {}", id.0))
    }

    async fn on_incoming_stream(&self, _send: Box<dyn BiSendStream>, _recv: Box<dyn BiRecvStream>) {
        // In the cache model, the client always initiates streams.
        // Server-initiated streams are not expected.
        warn!("unexpected incoming file-relay stream from server (cache model)");
    }
}

/// Compact human-readable byte size for toast strings. Plain enough to
/// avoid a `bytesize` dep here; mirrors the formatting used elsewhere in
/// the UI ("1.4 MiB").
fn format_bytes_simple(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &std::path::Path) {
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn unique_dest_path_uses_initial_when_free() {
        let dir = tempfile::tempdir().unwrap();
        let p = unique_dest_path(dir.path(), "foo.png");
        assert_eq!(p, dir.path().join("foo.png"));
    }

    #[test]
    fn unique_dest_path_appends_suffix_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("foo.png"));
        let p = unique_dest_path(dir.path(), "foo.png");
        assert_eq!(p, dir.path().join("foo (2).png"));
    }

    #[test]
    fn unique_dest_path_walks_past_multiple_collisions() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("foo.png"));
        touch(&dir.path().join("foo (2).png"));
        touch(&dir.path().join("foo (3).png"));
        let p = unique_dest_path(dir.path(), "foo.png");
        assert_eq!(p, dir.path().join("foo (4).png"));
    }

    #[test]
    fn unique_dest_path_handles_missing_extension() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("readme"));
        let p = unique_dest_path(dir.path(), "readme");
        assert_eq!(p, dir.path().join("readme (2)"));
    }

    // --- validate_fetch_progress ---

    #[test]
    fn validate_fetch_progress_accepts_zero_received() {
        // A zero-byte received count with a valid file_size is always OK.
        assert!(validate_fetch_progress(0, 0).is_ok());
        assert!(validate_fetch_progress(0, 1024).is_ok());
        assert!(
            validate_fetch_progress(0, rumble_client_traits::MAX_UPLOAD_BYTES).is_ok(),
            "exactly at ceiling should be accepted"
        );
    }

    #[test]
    fn validate_fetch_progress_rejects_oversized_declaration() {
        // Server-declared file_size above the ceiling must be rejected before
        // any bytes are written.
        let result = validate_fetch_progress(0, rumble_client_traits::MAX_UPLOAD_BYTES + 1);
        assert!(result.is_err(), "file_size one byte over ceiling must fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("exceeds"), "error message should mention ceiling: {msg}");
    }

    #[test]
    fn validate_fetch_progress_rejects_received_equal_to_ceiling_plus_one() {
        // Even when declared size equals the ceiling, receiving one more byte
        // is a protocol violation.
        let ceiling = rumble_client_traits::MAX_UPLOAD_BYTES;
        let result = validate_fetch_progress(ceiling + 1, ceiling);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("protocol violation"),
            "overrun error must say protocol violation: {msg}"
        );
    }

    #[test]
    fn validate_fetch_progress_accepts_partial_and_exact_receive() {
        // Receiving up to and including file_size is normal progress.
        assert!(validate_fetch_progress(100, 200).is_ok());
        assert!(validate_fetch_progress(200, 200).is_ok(), "exact receive is OK");
    }

    #[test]
    fn validate_fetch_progress_rejects_overrun() {
        // Receiving more than declared is always a protocol violation.
        assert!(validate_fetch_progress(201, 200).is_err());
        assert!(
            validate_fetch_progress(1, 0).is_err(),
            "any bytes when size=0 is an overrun"
        );
    }

    // --- TokenBucket ---

    #[test]
    fn token_bucket_unlimited_never_sleeps() {
        let mut bucket = TokenBucket::new();
        let now = Instant::now();
        for i in 0..10 {
            let wait = bucket.consume(10 * 1024 * 1024, 0, now + Duration::from_millis(i));
            assert_eq!(wait, Duration::ZERO, "limit 0 must never throttle");
        }
    }

    #[test]
    fn token_bucket_throttles_at_limit() {
        // 100 KiB/s limit, fresh bucket (zero balance): a 100 KiB chunk
        // puts us 100 KiB in debt → ~1s sleep.
        let mut bucket = TokenBucket::new();
        let now = Instant::now();
        let wait = bucket.consume(100 * 1024, 100 * 1024, now);
        assert!(
            (wait.as_secs_f64() - 1.0).abs() < 0.01,
            "expected ~1s wait, got {wait:?}"
        );
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let mut bucket = TokenBucket::new();
        let limit = 100 * 1024u64; // 100 KiB/s
        let t0 = Instant::now();
        // Consume a full second of budget at t0 → debt = limit.
        let wait = bucket.consume(limit, limit, t0);
        assert!(wait > Duration::ZERO);
        // One second later the refill has repaid the debt (balance back
        // to 0), so a tiny chunk owes only its own negligible cost.
        let wait = bucket.consume(1, limit, t0 + Duration::from_secs(1));
        assert!(
            wait < Duration::from_millis(1),
            "debt should be repaid after 1s, got {wait:?}"
        );
    }

    #[test]
    fn token_bucket_caps_burst_to_one_second() {
        let mut bucket = TokenBucket::new();
        let limit = 100 * 1024u64;
        let t0 = Instant::now();
        bucket.consume(0, limit, t0);
        // After a long idle gap the bucket holds at most 1s of budget:
        // consuming 2s worth must leave ~1s of debt.
        let wait = bucket.consume(2 * limit, limit, t0 + Duration::from_secs(60));
        assert!(
            (wait.as_secs_f64() - 1.0).abs() < 0.01,
            "burst must be capped at ~1s of budget, got {wait:?}"
        );
    }

    #[test]
    fn token_bucket_oversized_chunk_makes_progress() {
        // A chunk larger than the whole bucket capacity goes into debt
        // proportional to its size rather than blocking forever.
        let mut bucket = TokenBucket::new();
        let limit = 1024u64; // 1 KiB/s
        let now = Instant::now();
        let wait = bucket.consume(10 * 1024, limit, now);
        assert!(
            (wait.as_secs_f64() - 10.0).abs() < 0.01,
            "10 KiB at 1 KiB/s should owe ~10s, got {wait:?}"
        );
    }

    #[test]
    fn token_bucket_limit_change_applies_immediately() {
        let mut bucket = TokenBucket::new();
        let t0 = Instant::now();
        // Unlimited: no wait, state stays reset.
        assert_eq!(bucket.consume(1024 * 1024, 0, t0), Duration::ZERO);
        // Limit switched on mid-transfer: next chunk is throttled from a
        // clean baseline (no stale credit, no stale debt).
        let wait = bucket.consume(2048, 1024, t0);
        assert!(
            (wait.as_secs_f64() - 2.0).abs() < 0.01,
            "2 KiB at 1 KiB/s from empty bucket should owe ~2s, got {wait:?}"
        );
    }
}
