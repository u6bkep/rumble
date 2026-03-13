//! Relay-based file transfer plugin for native clients.
//!
//! Implements [`FileTransferPlugin`] by sending file data through the server
//! as a relay. The server opens a corresponding stream to the recipient and
//! pipes bytes through.
//!
//! ## Sending flow
//!
//! 1. `share(path)` stores the file metadata locally and returns a [`FileOffer`].
//!    The offer's `share_data` contains a JSON-encoded [`RelayShareData`] with
//!    the transfer_id and recipient information to be filled in later.
//! 2. The caller sends the offer to a recipient (e.g., via chat message).
//! 3. `send_to(transfer_id, recipient_user_id)` opens a QUIC stream to the
//!    server tagged with StreamHeader `"file-relay"`, writes a length-prefixed
//!    [`RelayRequest`], and then streams the file data.
//!
//! ## Receiving flow
//!
//! The plugin spawns a background task that accepts incoming QUIC streams
//! tagged with `"file-relay"`. When a stream arrives, it reads the
//! length-prefixed [`RelayOffer`], creates a temp file, and copies bytes
//! from the stream.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use api::proto;
use prost::Message;
use rumble_client::file_transfer::{FileOffer, FileTransferPlugin, TransferId, TransferStatus};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Serialized into `FileOffer.share_data` so recipients know this is a relay
/// offer and can call `download()` with it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct RelayShareData {
    pub transfer_id: String,
    pub file_name: String,
    pub file_size: u64,
}

/// Internal state for a pending or active transfer.
#[derive(Clone, Debug)]
struct TransferEntry {
    id: TransferId,
    name: String,
    size: u64,
    /// Local path (for outgoing transfers).
    path: Option<PathBuf>,
    progress: f32,
    download_speed: u64,
    upload_speed: u64,
    is_complete: bool,
    error: Option<String>,
}

impl TransferEntry {
    fn to_status(&self) -> TransferStatus {
        let state = if self.error.is_some() {
            rumble_client::file_transfer::PluginTransferState::Error
        } else if self.is_complete {
            rumble_client::file_transfer::PluginTransferState::Seeding
        } else {
            rumble_client::file_transfer::PluginTransferState::Downloading
        };
        TransferStatus {
            id: self.id.clone(),
            infohash: [0u8; 20], // relay transfers don't have infohashes
            name: self.name.clone(),
            size: self.size,
            progress: self.progress,
            download_speed: self.download_speed,
            upload_speed: self.upload_speed,
            peers: 0,
            state,
            is_finished: self.is_complete,
            error: self.error.clone(),
            magnet: None,
            local_path: self.path.clone(),
            peer_details: Vec::new(),
        }
    }
}

/// Background command sent from the plugin API to the async worker.
enum Command {
    /// Initiate an outgoing relay: send file data to server for delivery to recipient.
    Send {
        transfer_id: String,
        recipient_user_id: u64,
        file_name: String,
        file_size: u64,
        path: PathBuf,
    },
    /// Cancel a transfer.
    Cancel { transfer_id: String },
}

/// Relay-based [`FileTransferPlugin`] implementation for native desktop clients.
///
/// Construct with [`FileTransferRelayPlugin::new`], passing a `quinn::Connection`
/// and a downloads directory. The plugin spawns a background task to process
/// outgoing commands and incoming streams.
pub struct FileTransferRelayPlugin {
    transfers: Arc<Mutex<HashMap<String, TransferEntry>>>,
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// Stored for potential future use (e.g., listing downloaded files).
    _downloads_dir: PathBuf,
}

impl FileTransferRelayPlugin {
    /// Create a new relay file transfer plugin.
    ///
    /// - `conn`: The QUIC connection to the server.
    /// - `downloads_dir`: Directory where incoming files are saved.
    pub fn new(conn: quinn::Connection, downloads_dir: PathBuf) -> Self {
        let transfers: Arc<Mutex<HashMap<String, TransferEntry>>> = Arc::new(Mutex::new(HashMap::new()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        // Spawn the background worker.
        let worker_transfers = transfers.clone();
        let worker_conn = conn.clone();
        let worker_downloads = downloads_dir.clone();
        tokio::spawn(async move {
            run_worker(worker_conn, cmd_rx, worker_transfers, worker_downloads).await;
        });

        // Spawn the incoming stream listener.
        let recv_transfers = transfers.clone();
        let recv_downloads = downloads_dir.clone();
        tokio::spawn(async move {
            run_incoming_listener(conn, recv_transfers, recv_downloads).await;
        });

        Self {
            transfers,
            cmd_tx,
            _downloads_dir: downloads_dir,
        }
    }

    /// Initiate sending a previously shared file to a specific recipient.
    ///
    /// Call this after `share()` returns a `FileOffer`, typically when the
    /// recipient requests the file.
    pub fn send_to(&self, transfer_id: &str, recipient_user_id: u64) -> Result<()> {
        let entry = {
            let transfers = self.transfers.lock().unwrap();
            transfers
                .get(transfer_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown transfer_id: {transfer_id}"))?
        };

        let path = entry
            .path
            .ok_or_else(|| anyhow::anyhow!("no local path for transfer {transfer_id}"))?;

        self.cmd_tx.send(Command::Send {
            transfer_id: transfer_id.to_owned(),
            recipient_user_id,
            file_name: entry.name,
            file_size: entry.size,
            path,
        })?;

        Ok(())
    }
}

impl FileTransferPlugin for FileTransferRelayPlugin {
    fn share(&self, path: PathBuf) -> Result<FileOffer> {
        let metadata = std::fs::metadata(&path)?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_owned());
        let file_size = metadata.len();
        let transfer_id = uuid::Uuid::new_v4().to_string();

        let share_data = RelayShareData {
            transfer_id: transfer_id.clone(),
            file_name: file_name.clone(),
            file_size,
        };

        let entry = TransferEntry {
            id: TransferId(transfer_id.clone()),
            name: file_name.clone(),
            size: file_size,
            path: Some(path),
            progress: 0.0,
            download_speed: 0,
            upload_speed: 0,
            is_complete: false,
            error: None,
        };

        self.transfers.lock().unwrap().insert(transfer_id.clone(), entry);

        let mime = mime_guess::from_path(&file_name).first_or_octet_stream().to_string();

        Ok(FileOffer {
            id: TransferId(transfer_id),
            name: file_name,
            size: file_size,
            mime,
            share_data: serde_json::to_string(&share_data)?,
        })
    }

    fn download(&self, share_data: &str) -> Result<TransferId> {
        // Parse the share_data to get relay information.
        let relay_data: RelayShareData =
            serde_json::from_str(share_data).map_err(|e| anyhow::anyhow!("invalid relay share_data: {e}"))?;

        let transfer_id = relay_data.transfer_id.clone();

        // Register the transfer as a pending download.
        let entry = TransferEntry {
            id: TransferId(transfer_id.clone()),
            name: relay_data.file_name,
            size: relay_data.file_size,
            path: None,
            progress: 0.0,
            download_speed: 0,
            upload_speed: 0,
            is_complete: false,
            error: None,
        };

        self.transfers.lock().unwrap().insert(transfer_id.clone(), entry);

        // For the relay model, the download is initiated by the sender pushing
        // data through the server (incoming stream). The download() call just
        // registers interest. The actual data arrives via the incoming stream
        // listener.

        Ok(TransferId(transfer_id))
    }

    fn transfers(&self) -> Vec<TransferStatus> {
        self.transfers.lock().unwrap().values().map(|e| e.to_status()).collect()
    }

    fn pause(&self, _id: &TransferId) -> Result<()> {
        // Relay transfers don't support pause — data streams continuously.
        anyhow::bail!("relay transfers cannot be paused")
    }

    fn resume(&self, _id: &TransferId) -> Result<()> {
        anyhow::bail!("relay transfers cannot be resumed")
    }

    fn cancel(&self, id: &TransferId, _delete_files: bool) -> Result<()> {
        self.cmd_tx.send(Command::Cancel {
            transfer_id: id.0.clone(),
        })?;
        Ok(())
    }

    fn get_file_path(&self, id: &TransferId) -> Result<PathBuf> {
        let transfers = self.transfers.lock().unwrap();
        transfers
            .get(&id.0)
            .and_then(|e| e.path.clone())
            .ok_or_else(|| anyhow::anyhow!("no file path for transfer {}", id.0))
    }
}

/// Background worker that processes outgoing send commands.
async fn run_worker(
    conn: quinn::Connection,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    transfers: Arc<Mutex<HashMap<String, TransferEntry>>>,
    _downloads_dir: PathBuf,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::Send {
                transfer_id,
                recipient_user_id,
                file_name,
                file_size,
                path,
            } => {
                let conn = conn.clone();
                let transfers = transfers.clone();
                let tid = transfer_id.clone();

                tokio::spawn(async move {
                    if let Err(e) =
                        send_file(conn, &tid, recipient_user_id, &file_name, file_size, &path, &transfers).await
                    {
                        warn!(transfer_id = %tid, "send_file error: {e}");
                        if let Ok(mut t) = transfers.lock() {
                            if let Some(entry) = t.get_mut(&tid) {
                                entry.error = Some(e.to_string());
                            }
                        }
                    }
                });
            }
            Command::Cancel { transfer_id } => {
                // Mark as cancelled. The actual stream abort happens implicitly
                // when the entry is dropped or the stream is closed.
                if let Ok(mut t) = transfers.lock() {
                    if let Some(entry) = t.get_mut(&transfer_id) {
                        entry.error = Some("cancelled".to_owned());
                    }
                }
            }
        }
    }
}

/// Open a QUIC stream to the server and send a file.
async fn send_file(
    conn: quinn::Connection,
    transfer_id: &str,
    recipient_user_id: u64,
    file_name: &str,
    file_size: u64,
    path: &std::path::Path,
    transfers: &Arc<Mutex<HashMap<String, TransferEntry>>>,
) -> Result<()> {
    // Open a bi-directional stream.
    let (mut send, _recv) = conn.open_bi().await?;

    // Write the StreamHeader: plugin name "file-relay".
    let plugin_name = b"file-relay";
    send.write_all(&(plugin_name.len() as u16).to_be_bytes()).await?;
    send.write_all(plugin_name).await?;

    // Write the RelayRequest (length-prefixed protobuf).
    let request = proto::RelayRequest {
        recipient_user_id,
        file_name: file_name.to_owned(),
        file_size,
        transfer_id: transfer_id.to_owned(),
    };
    let req_bytes = request.encode_to_vec();
    send.write_all(&(req_bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&req_bytes).await?;

    // Stream the file data.
    let file = tokio::fs::File::open(path).await?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut buf = vec![0u8; 32 * 1024];
    let mut sent: u64 = 0;

    loop {
        let n = tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
        sent += n as u64;

        // Update progress.
        if let Ok(mut t) = transfers.lock() {
            if let Some(entry) = t.get_mut(transfer_id) {
                entry.progress = if file_size > 0 {
                    (sent as f32) / (file_size as f32)
                } else {
                    1.0
                };
            }
        }
    }

    // Close the send half to signal completion.
    send.finish()?;

    // Mark complete.
    if let Ok(mut t) = transfers.lock() {
        if let Some(entry) = t.get_mut(transfer_id) {
            entry.progress = 1.0;
            entry.is_complete = true;
        }
    }

    info!(transfer_id, bytes = sent, "relay send complete");
    Ok(())
}

/// Listen for incoming QUIC streams from the server that carry relay file data.
async fn run_incoming_listener(
    conn: quinn::Connection,
    transfers: Arc<Mutex<HashMap<String, TransferEntry>>>,
    downloads_dir: PathBuf,
) {
    loop {
        match conn.accept_bi().await {
            Ok((send, mut recv)) => {
                // Probe for "file-relay" stream header.
                let mut len_buf = [0u8; 2];
                if recv.read_exact(&mut len_buf).await.is_err() {
                    continue;
                }
                let name_len = u16::from_be_bytes(len_buf) as usize;
                if name_len == 0 || name_len > 255 {
                    continue;
                }
                let mut name_buf = vec![0u8; name_len];
                if recv.read_exact(&mut name_buf).await.is_err() {
                    continue;
                }
                let plugin_name = match std::str::from_utf8(&name_buf) {
                    Ok(s) => s.to_owned(),
                    Err(_) => continue,
                };

                if plugin_name != "file-relay" {
                    // Not our stream; drop it.
                    continue;
                }

                let transfers = transfers.clone();
                let downloads_dir = downloads_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_incoming_relay(send, recv, transfers, downloads_dir).await {
                        warn!("incoming relay error: {e}");
                    }
                });
            }
            Err(_) => {
                // Connection closed.
                break;
            }
        }
    }
}

/// Handle a single incoming relay stream (server -> client).
async fn handle_incoming_relay(
    _send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    transfers: Arc<Mutex<HashMap<String, TransferEntry>>>,
    downloads_dir: PathBuf,
) -> Result<()> {
    // Read length-prefixed RelayOffer.
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let offer_len = u32::from_be_bytes(len_buf) as usize;
    if offer_len > 64 * 1024 {
        anyhow::bail!("RelayOffer too large ({offer_len} bytes)");
    }

    let mut offer_buf = vec![0u8; offer_len];
    recv.read_exact(&mut offer_buf).await?;
    let offer = proto::RelayOffer::decode(&offer_buf[..])?;

    let transfer_id = offer.transfer_id.clone();
    let file_name = offer.file_name.clone();
    let file_size = offer.file_size;

    info!(
        transfer_id = %transfer_id,
        sender = offer.sender_user_id,
        file = %file_name,
        size = file_size,
        "incoming relay offer"
    );

    // Register the transfer if not already known.
    {
        let mut t = transfers.lock().unwrap();
        t.entry(transfer_id.clone()).or_insert_with(|| TransferEntry {
            id: TransferId(transfer_id.clone()),
            name: file_name.clone(),
            size: file_size,
            path: None,
            progress: 0.0,
            download_speed: 0,
            upload_speed: 0,
            is_complete: false,
            error: None,
        });
    }

    // Write to downloads directory.
    let dest = downloads_dir.join(&file_name);
    std::fs::create_dir_all(&downloads_dir)?;
    let file = tokio::fs::File::create(&dest).await?;
    let mut writer = tokio::io::BufWriter::new(file);
    let mut buf = vec![0u8; 32 * 1024];
    let mut received: u64 = 0;

    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                tokio::io::AsyncWriteExt::write_all(&mut writer, &buf[..n]).await?;
                received += n as u64;

                if let Ok(mut t) = transfers.lock() {
                    if let Some(entry) = t.get_mut(&transfer_id) {
                        entry.progress = if file_size > 0 {
                            (received as f32) / (file_size as f32)
                        } else {
                            1.0
                        };
                    }
                }
            }
            Ok(None) => {
                // Stream closed -- transfer complete.
                break;
            }
            Err(e) => {
                if let Ok(mut t) = transfers.lock() {
                    if let Some(entry) = t.get_mut(&transfer_id) {
                        entry.error = Some(e.to_string());
                    }
                }
                return Err(anyhow::anyhow!("recv error: {e}"));
            }
        }
    }

    tokio::io::AsyncWriteExt::flush(&mut writer).await?;

    // Mark complete.
    if let Ok(mut t) = transfers.lock() {
        if let Some(entry) = t.get_mut(&transfer_id) {
            entry.progress = 1.0;
            entry.is_complete = true;
            entry.path = Some(dest.clone());
        }
    }

    info!(transfer_id = %transfer_id, bytes = received, dest = %dest.display(), "relay download complete");
    Ok(())
}
