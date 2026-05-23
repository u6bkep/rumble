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

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};

use anyhow::Result;
use async_trait::async_trait;
use prost::Message;
use rumble_client_traits::{
    file_transfer::{
        FileOffer, FileTransferPlugin, PluginEventSink, PluginNotificationLevel, TransferDirection, TransferId,
        TransferStatus,
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

/// Internal state for a pending or active transfer.
struct TransferEntry {
    id: TransferId,
    name: String,
    size: u64,
    mime: String,
    /// Local path (for completed downloads or outgoing uploads).
    path: Option<PathBuf>,
    /// Opaque share_data string (uploads only). Cached so a duplicate
    /// in-flight share can return the existing offer instead of
    /// re-uploading.
    share_data: Option<String>,
    /// Room ID the upload was tagged with at start. Used to detect
    /// in-flight uploads that need to be cancelled when the user
    /// switches rooms.
    room_id: Option<String>,
    progress: f32,
    is_complete: bool,
    is_upload: bool,
    error: Option<String>,
    /// Cancellation token for this transfer's task.
    cancel: CancellationToken,
    /// Smoothed bytes-per-second over the last sampling window. Updated
    /// by the upload/fetch tasks via [`update_speed`]; reads from
    /// `transfers()` see the most recent value.
    speed_bps: u64,
    /// Bytes transferred at the last speed sample. Combined with
    /// `last_sample_at` to compute a fresh `speed_bps`.
    last_sample_bytes: u64,
    /// Wall-clock instant of the last speed sample.
    last_sample_at: Instant,
}

impl TransferEntry {
    fn to_status(&self) -> TransferStatus {
        let state = if self.error.is_some() {
            rumble_client_traits::file_transfer::PluginTransferState::Error
        } else if self.is_complete {
            rumble_client_traits::file_transfer::PluginTransferState::Seeding
        } else if self.is_upload {
            rumble_client_traits::file_transfer::PluginTransferState::Initializing
        } else {
            rumble_client_traits::file_transfer::PluginTransferState::Downloading
        };
        let (download_speed, upload_speed) = if self.is_upload {
            (0, self.speed_bps)
        } else {
            (self.speed_bps, 0)
        };
        TransferStatus {
            id: self.id.clone(),
            name: self.name.clone(),
            size: self.size,
            direction: if self.is_upload {
                TransferDirection::Upload
            } else {
                TransferDirection::Download
            },
            progress: self.progress,
            download_speed,
            upload_speed,
            peers: 0,
            state,
            is_finished: self.is_complete,
            error: self.error.clone(),
            local_path: self.path.clone(),
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

/// Update an entry's smoothed bytes-per-second sample. Re-baselines
/// when at least 250 ms has elapsed since the last sample, which keeps
/// the displayed speed steady at high data rates without making low-
/// rate transfers look stalled. Caller already holds the transfers map
/// lock.
fn update_speed(entry: &mut TransferEntry, bytes_so_far: u64) {
    let now = Instant::now();
    let elapsed = now.duration_since(entry.last_sample_at);
    if elapsed.as_millis() < 250 {
        return;
    }
    let delta = bytes_so_far.saturating_sub(entry.last_sample_bytes);
    let secs = elapsed.as_secs_f64().max(0.001);
    entry.speed_bps = (delta as f64 / secs) as u64;
    entry.last_sample_bytes = bytes_so_far;
    entry.last_sample_at = now;
}

type TransferMap = Arc<parking_lot::Mutex<HashMap<String, TransferEntry>>>;

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
}

impl FileTransferRelayPlugin {
    /// Create a new relay file transfer plugin.
    ///
    /// - `opener`: Transport-agnostic stream opener for opening streams to the server.
    /// - `downloads_dir`: Directory where fetched files are saved.
    /// - `event_sink`: Optional callback for user-visible toast events.
    pub fn new(opener: Arc<dyn StreamOpener>, downloads_dir: PathBuf, event_sink: Option<PluginEventSink>) -> Self {
        Self {
            opener,
            downloads_dir,
            transfers: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            room_id: parking_lot::Mutex::new(String::new()),
            event_sink,
        }
    }

    fn emit(&self, level: PluginNotificationLevel, msg: impl Into<String>) {
        if let Some(sink) = &self.event_sink {
            (sink)(level, msg.into());
        }
    }
}

/// Write a stream header for the "file-relay" plugin.
async fn write_file_relay_header(send: &mut dyn BiSendStream) -> Result<()> {
    let header = rumble_client_traits::StreamHeader {
        plugin: "file-relay".to_owned(),
    };
    header.write_to(send).await
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
) {
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
        r = do_upload(
            &opener, &transfer_id, &room_id, &file_name, file_size, &mime, &path, &transfers,
        ) => r,
    };

    match result {
        Ok(()) => {
            let mut t = transfers.lock();
            if let Some(entry) = t.get_mut(&transfer_id) {
                entry.progress = 1.0;
                entry.is_complete = true;
            }
            info!(transfer_id, "relay upload complete");
        }
        Err(e) => {
            let msg = e.to_string();
            // Cancellation is expected (user navigated away, room
            // change, explicit cancel); don't toast for those — the
            // failure card carries enough signal already.
            let cancelled = cancel.is_cancelled() || msg == "cancelled";
            let mut t = transfers.lock();
            if let Some(entry) = t.get_mut(&transfer_id) {
                entry.error = Some(msg.clone());
            }
            drop(t);
            warn!(transfer_id, error = %msg, "relay upload failed");
            if !cancelled && let Some(sink) = &event_sink {
                (sink)(PluginNotificationLevel::Error, format!("Upload failed: {msg}"));
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
            anyhow::bail!("upload rejected by server (code {code}): {error}");
        }
    }

    // --- Phase 3: stream body. ---
    let file = tokio::fs::File::open(path).await?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut buf = vec![0u8; 64 * 1024];
    let mut sent: u64 = 0;

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
                entry.progress = if file_size > 0 {
                    (sent as f32) / (file_size as f32)
                } else {
                    1.0
                };
                update_speed(entry, sent);
            }
        }
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
async fn run_fetch(
    opener: Arc<dyn StreamOpener>,
    transfer_id: String,
    downloads_dir: PathBuf,
    transfers: TransferMap,
    cancel: CancellationToken,
    event_sink: Option<PluginEventSink>,
) {
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
        r = do_fetch(&opener, &transfer_id, &downloads_dir, &transfers) => r,
    };

    match result {
        Ok(dest) => {
            let mut t = transfers.lock();
            if let Some(entry) = t.get_mut(&transfer_id) {
                entry.progress = 1.0;
                entry.is_complete = true;
                entry.path = Some(dest.clone());
            }
            info!(transfer_id, dest = %dest.display(), "relay fetch complete");
        }
        Err(e) => {
            let msg = e.to_string();
            let cancelled = cancel.is_cancelled() || msg == "cancelled";
            let mut t = transfers.lock();
            if let Some(entry) = t.get_mut(&transfer_id) {
                entry.error = Some(msg.clone());
            }
            drop(t);
            warn!(transfer_id, error = %msg, "relay fetch failed");
            if !cancelled && let Some(sink) = &event_sink {
                (sink)(PluginNotificationLevel::Error, format!("Download failed: {msg}"));
            }
        }
    }
}

async fn do_fetch(
    opener: &Arc<dyn StreamOpener>,
    transfer_id: &str,
    downloads_dir: &std::path::Path,
    transfers: &TransferMap,
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

    // Update transfer metadata from server response.
    {
        let mut t = transfers.lock();
        if let Some(entry) = t.get_mut(transfer_id) {
            entry.name = file_name.clone();
            entry.size = file_size;
        }
    }

    // Write to downloads directory. Auto-rename on collision so a
    // re-download of the same name doesn't silently overwrite an
    // earlier copy the user might still want.
    std::fs::create_dir_all(downloads_dir)?;
    let dest = unique_dest_path(downloads_dir, &file_name);
    let file = tokio::fs::File::create(&dest).await?;
    let mut writer = tokio::io::BufWriter::new(file);
    let mut buf = vec![0u8; 64 * 1024];
    let mut received: u64 = 0;

    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) if n > 0 => {
                tokio::io::AsyncWriteExt::write_all(&mut writer, &buf[..n]).await?;
                received += n as u64;

                let mut t = transfers.lock();
                if let Some(entry) = t.get_mut(transfer_id) {
                    entry.progress = if file_size > 0 {
                        (received as f32) / (file_size as f32)
                    } else {
                        1.0
                    };
                    update_speed(entry, received);
                }
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

#[async_trait]
impl FileTransferPlugin for FileTransferRelayPlugin {
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
                if entry.is_upload
                    && !entry.is_complete
                    && entry.error.is_none()
                    && entry.room_id.as_deref() == Some(old_room.as_str())
                {
                    entry.cancel.cancel();
                    entry.error = Some("room changed".to_owned());
                    n += 1;
                }
            }
            n
        };

        if cancelled > 0 {
            let noun = if cancelled == 1 { "upload" } else { "uploads" };
            self.emit(
                PluginNotificationLevel::Warn,
                format!("{cancelled} {noun} cancelled due to room change"),
            );
        }
    }

    fn share(&self, path: PathBuf) -> Result<FileOffer> {
        let metadata = std::fs::metadata(&path)?;
        let file_size = metadata.len();

        if file_size > rumble_client_traits::MAX_UPLOAD_BYTES {
            self.emit(
                PluginNotificationLevel::Error,
                format!(
                    "File too large ({}); limit is {}",
                    format_bytes_simple(file_size),
                    format_bytes_simple(rumble_client_traits::MAX_UPLOAD_BYTES),
                ),
            );
            anyhow::bail!(
                "file exceeds max upload size of {} bytes",
                rumble_client_traits::MAX_UPLOAD_BYTES
            );
        }

        // Duplicate-in-flight detection: if the same path is already
        // uploading and not yet complete, return the existing FileOffer
        // instead of re-uploading. This dedups e.g. double-click on the
        // share button or a paste while the previous paste is still in
        // flight.
        {
            let t = self.transfers.lock();
            if let Some(existing) = t.values().find(|e| {
                e.is_upload && !e.is_complete && e.error.is_none() && e.path.as_deref() == Some(path.as_path())
            }) && let Some(share_data) = existing.share_data.clone()
            {
                self.emit(
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

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_owned());
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let mime = mime_guess::from_path(&file_name).first_or_octet_stream().to_string();
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
            path: Some(path.clone()),
            share_data: Some(share_data_json.clone()),
            room_id: if room_id.is_empty() {
                None
            } else {
                Some(room_id.clone())
            },
            progress: 0.0,
            is_complete: false,
            is_upload: true,
            error: None,
            cancel: cancel.clone(),
            speed_bps: 0,
            last_sample_bytes: 0,
            last_sample_at: Instant::now(),
        };

        self.transfers.lock().insert(transfer_id.clone(), entry);

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
            path: None,
            share_data: None,
            room_id: None,
            progress: 0.0,
            is_complete: false,
            is_upload: false,
            error: None,
            cancel: cancel.clone(),
            speed_bps: 0,
            last_sample_bytes: 0,
            last_sample_at: Instant::now(),
        };

        self.transfers.lock().insert(transfer_id.clone(), entry);

        // Spawn fetch task.
        let opener = self.opener.clone();
        let downloads_dir = self.downloads_dir.clone();
        let transfers = self.transfers.clone();
        let tid = transfer_id.clone();
        let sink = self.event_sink.clone();
        tokio::spawn(run_fetch(opener, tid, downloads_dir, transfers, cancel, sink));

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

    fn cancel(&self, id: &TransferId, _delete_files: bool) -> Result<()> {
        let t = self.transfers.lock();
        if let Some(entry) = t.get(&id.0) {
            entry.cancel.cancel();
        }
        Ok(())
    }

    fn get_file_path(&self, id: &TransferId) -> Result<PathBuf> {
        let t = self.transfers.lock();
        t.get(&id.0)
            .and_then(|e| e.path.clone())
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
}
