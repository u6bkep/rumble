//! File transfer relay plugin — store-and-serve cache model.
//!
//! Clients upload files to the server, which caches them per-room.
//! Other clients in the room can then fetch cached files by transfer ID.
//!
//! ## Upload flow (phased handshake)
//!
//! 1. Client opens a `"file-relay"` stream, writes type discriminator `0x01`,
//!    then a length-prefixed [`proto::RelayUpload`] with the file metadata.
//! 2. Server validates size limits, quota, and room membership, then sends a
//!    length-prefixed [`proto::PluginStreamAck`]. On rejection the server
//!    also calls `recv.stop` so the client does not stream the body.
//! 3. On `Ok` ack, the client streams the raw file bytes and finishes the
//!    send side.
//! 4. Server stores in the room-scoped cache and writes a length-prefixed
//!    [`proto::RelayUploadResponse`] as the final completion result.
//!
//! ## Fetch flow
//!
//! 1. Client opens a `"file-relay"` stream to the server.
//! 2. Sends type discriminator `0x02`, then length-prefixed
//!    [`proto::RelayFetch`].
//! 3. Server responds with length-prefixed [`proto::RelayFetchResponse`],
//!    then raw file bytes (if found).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use dashmap::DashMap;
use prost::Message;
use rumble_protocol::proto;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    plugin::{
        ServerCtx, ServerPlugin, StreamHeader, read_length_prefixed, reject_plugin_stream, send_plugin_ack_ok,
        write_length_prefixed,
    },
    state::ClientHandle,
};

/// Plugin-specific ack rejection codes for the file relay.
const REJECT_CODE_TOO_LARGE: u32 = 1;
const REJECT_CODE_CACHE_FULL: u32 = 2;
const REJECT_CODE_ROOM_MISMATCH: u32 = 3;

/// A cached file entry.
struct CachedFile {
    room_id: String,
    file_name: String,
    file_size: u64,
    mime: String,
    data: Vec<u8>,
    created_at: Instant,
}

/// Configuration for the relay cache.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RelayCacheConfig {
    /// Cache entry lifetime (default "30m").
    #[serde(default = "default_ttl", with = "humantime_serde")]
    pub ttl: Duration,

    /// Evict entries when their room empties (default true).
    #[serde(default = "default_evict_on_room_clear")]
    pub evict_on_room_clear: bool,

    /// Max total cache size (default "500 MB").
    #[serde(default = "default_max_total_size")]
    pub max_total_size: bytesize::ByteSize,

    /// Max single file size (default "100 MB").
    #[serde(default = "default_max_file_size")]
    pub max_file_size: bytesize::ByteSize,
}

fn default_ttl() -> Duration {
    Duration::from_secs(30 * 60)
}

fn default_evict_on_room_clear() -> bool {
    true
}

fn default_max_total_size() -> bytesize::ByteSize {
    bytesize::ByteSize::mib(1024)
}

fn default_max_file_size() -> bytesize::ByteSize {
    bytesize::ByteSize::mib(256)
}

impl Default for RelayCacheConfig {
    fn default() -> Self {
        Self {
            ttl: default_ttl(),
            evict_on_room_clear: default_evict_on_room_clear(),
            max_total_size: default_max_total_size(),
            max_file_size: default_max_file_size(),
        }
    }
}

/// Server-side file relay plugin using a store-and-serve cache model.
///
/// Uploaded files are held in memory keyed by transfer ID. Fetch requests
/// look up the cache and stream the data back to the requester.
pub struct FileTransferRelayPlugin {
    /// Room-scoped file cache: transfer_id -> CachedFile.
    cache: Arc<DashMap<String, CachedFile>>,
    /// Configuration.
    config: RelayCacheConfig,
    /// Total bytes currently cached (for quota enforcement).
    total_cached: Arc<AtomicU64>,
    /// Parent cancellation token — cancelled on stop().
    cancel: CancellationToken,
}

impl FileTransferRelayPlugin {
    /// Create a new relay plugin with default configuration.
    pub fn new() -> Self {
        Self::with_config(RelayCacheConfig::default())
    }

    /// Create a new relay plugin with the given configuration.
    pub fn with_config(config: RelayCacheConfig) -> Self {
        Self {
            cache: Arc::new(DashMap::new()),
            config,
            total_cached: Arc::new(AtomicU64::new(0)),
            cancel: CancellationToken::new(),
        }
    }

    /// Handle an upload stream.
    async fn handle_upload(
        &self,
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        sender: &Arc<ClientHandle>,
        ctx: &ServerCtx,
    ) -> Result<()> {
        // Read length-prefixed RelayUpload proto (request metadata).
        let msg_buf = read_length_prefixed(&mut recv).await?;
        let upload = proto::RelayUpload::decode(&msg_buf[..])?;

        let user_id = sender.user_id;
        let transfer_id = upload.transfer_id.clone();

        info!(
            user_id,
            transfer_id = %transfer_id,
            file = %upload.file_name,
            size = upload.file_size,
            room = %upload.room_id,
            "file relay upload request"
        );

        // --- Phase 1: admission control. Reject before the client streams a body. ---

        if upload.file_size > self.config.max_file_size.as_u64() {
            let reason = format!(
                "file too large: {} bytes (max {})",
                upload.file_size, self.config.max_file_size
            );
            info!(transfer_id = %transfer_id, "{reason}");
            reject_plugin_stream(&mut send, &mut recv, REJECT_CODE_TOO_LARGE, reason).await?;
            return Ok(());
        }

        let current_total = self.total_cached.load(Ordering::Relaxed);
        if current_total + upload.file_size > self.config.max_total_size.as_u64() {
            info!(transfer_id = %transfer_id, "rejecting upload: cache full");
            reject_plugin_stream(&mut send, &mut recv, REJECT_CODE_CACHE_FULL, "server cache full").await?;
            return Ok(());
        }

        if let Some(actual_room) = ctx.get_user_room(user_id) {
            let actual_room_str = actual_room.to_string();
            if actual_room_str != upload.room_id {
                let reason = format!("room mismatch: you are in {actual_room_str}, not {}", upload.room_id);
                info!(transfer_id = %transfer_id, "{reason}");
                reject_plugin_stream(&mut send, &mut recv, REJECT_CODE_ROOM_MISMATCH, reason).await?;
                return Ok(());
            }
        }

        // Admit. The client waits for this ack before sending body bytes.
        send_plugin_ack_ok(&mut send).await?;

        // --- Phase 2: read the body. ---

        let file_size = upload.file_size as usize;
        let mut data = Vec::with_capacity(file_size.min(32 * 1024 * 1024)); // pre-alloc capped at 32MB
        let mut remaining = file_size;
        let mut buf = vec![0u8; 64 * 1024];

        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            match recv.read(&mut buf[..to_read]).await {
                Ok(Some(n)) if n > 0 => {
                    data.extend_from_slice(&buf[..n]);
                    remaining -= n;
                }
                Ok(Some(_)) => continue, // zero-length read, retry
                Ok(None) => {
                    let resp = proto::RelayUploadResponse {
                        status: proto::RelayResult::Error.into(),
                        error: format!("stream closed after {} of {} bytes", data.len(), file_size),
                    };
                    write_length_prefixed(&mut send, &resp.encode_to_vec()).await?;
                    return Ok(());
                }
                Err(e) => {
                    warn!(transfer_id = %transfer_id, "upload read error: {e}");
                    return Err(e.into());
                }
            }
        }

        // Store in cache.
        let actual_size = data.len() as u64;
        if let Some(old) = self.cache.get(&transfer_id) {
            self.total_cached.fetch_sub(old.file_size, Ordering::Relaxed);
        }
        self.cache.insert(
            transfer_id.clone(),
            CachedFile {
                room_id: upload.room_id.clone(),
                file_name: upload.file_name.clone(),
                file_size: actual_size,
                mime: upload.mime.clone(),
                data,
                created_at: Instant::now(),
            },
        );
        self.total_cached.fetch_add(actual_size, Ordering::Relaxed);

        info!(
            transfer_id = %transfer_id,
            bytes = actual_size,
            "file cached"
        );

        // --- Phase 3: completion response. ---
        let resp = proto::RelayUploadResponse {
            status: proto::RelayResult::Ok.into(),
            error: String::new(),
        };
        write_length_prefixed(&mut send, &resp.encode_to_vec()).await?;

        Ok(())
    }

    /// Handle a fetch stream.
    async fn handle_fetch(&self, mut send: quinn::SendStream, mut recv: quinn::RecvStream) -> Result<()> {
        let msg_buf = read_length_prefixed(&mut recv).await?;
        let fetch = proto::RelayFetch::decode(&msg_buf[..])?;

        let transfer_id = fetch.transfer_id.clone();

        debug!(transfer_id = %transfer_id, "file relay fetch request");

        // Look up in cache.
        let entry = self.cache.get(&transfer_id);
        match entry {
            Some(cached) => {
                let resp = proto::RelayFetchResponse {
                    status: proto::RelayResult::Ok.into(),
                    file_name: cached.file_name.clone(),
                    file_size: cached.file_size,
                    mime: cached.mime.clone(),
                    error: String::new(),
                };
                write_length_prefixed(&mut send, &resp.encode_to_vec()).await?;

                // Write raw file bytes.
                send.write_all(&cached.data).await?;
                send.finish()?;

                info!(
                    transfer_id = %transfer_id,
                    bytes = cached.file_size,
                    "file served from cache"
                );
            }
            None => {
                let resp = proto::RelayFetchResponse {
                    status: proto::RelayResult::NotFound.into(),
                    file_name: String::new(),
                    file_size: 0,
                    mime: String::new(),
                    error: "transfer not found or expired".to_owned(),
                };
                write_length_prefixed(&mut send, &resp.encode_to_vec()).await?;
                send.finish()?;

                debug!(transfer_id = %transfer_id, "fetch: not found");
            }
        }

        Ok(())
    }

    /// Remove all cache entries for a given room.
    fn evict_room(&self, room_id: &str) {
        let to_remove: Vec<String> = self
            .cache
            .iter()
            .filter(|entry| entry.value().room_id == room_id)
            .map(|entry| entry.key().clone())
            .collect();

        for tid in &to_remove {
            if let Some((_, cached)) = self.cache.remove(tid) {
                self.total_cached.fetch_sub(cached.file_size, Ordering::Relaxed);
            }
        }

        if !to_remove.is_empty() {
            info!(room_id, count = to_remove.len(), "evicted room cache entries");
        }
    }
}

impl Default for FileTransferRelayPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ServerPlugin for FileTransferRelayPlugin {
    fn name(&self) -> &str {
        "file-relay"
    }

    async fn on_message(
        &self,
        _envelope: &proto::Envelope,
        _sender: &Arc<ClientHandle>,
        _ctx: &ServerCtx,
    ) -> Result<bool> {
        // The cache model has no control-stream messages.
        Ok(false)
    }

    async fn on_stream(
        &self,
        _header: StreamHeader,
        send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        sender: &Arc<ClientHandle>,
        ctx: &ServerCtx,
    ) -> Result<()> {
        // Read the type discriminator byte.
        let mut type_buf = [0u8; 1];
        recv.read_exact(&mut type_buf).await?;

        let child_cancel = self.cancel.child_token();

        match type_buf[0] {
            0x01 => {
                // Upload
                tokio::select! {
                    _ = child_cancel.cancelled() => {
                        debug!("upload stream cancelled by shutdown");
                    }
                    result = self.handle_upload(send, recv, sender, ctx) => {
                        if let Err(e) = result {
                            warn!(user_id = sender.user_id, "upload error: {e}");
                        }
                    }
                }
            }
            0x02 => {
                // Fetch
                tokio::select! {
                    _ = child_cancel.cancelled() => {
                        debug!("fetch stream cancelled by shutdown");
                    }
                    result = self.handle_fetch(send, recv) => {
                        if let Err(e) = result {
                            warn!(user_id = sender.user_id, "fetch error: {e}");
                        }
                    }
                }
            }
            other => {
                warn!(user_id = sender.user_id, type_byte = other, "unknown relay stream type");
            }
        }

        Ok(())
    }

    async fn on_disconnect(&self, client: &Arc<ClientHandle>, ctx: &ServerCtx) {
        if !self.config.evict_on_room_clear {
            return;
        }

        let user_id = client.user_id;

        // Find the room the user was in.
        let room_id = match ctx.get_user_room(user_id) {
            Some(r) => r,
            None => return,
        };

        // Check if the room is now empty (this user is the last one leaving).
        // get_room_members returns a snapshot; the user may still be in it.
        let members = ctx.get_room_members(room_id);
        let remaining = members.iter().filter(|uid| **uid != user_id).count();

        if remaining == 0 {
            let room_str = room_id.to_string();
            self.evict_room(&room_str);
        }
    }

    async fn start(&self, _ctx: &ServerCtx) -> Result<()> {
        // Spawn the TTL sweep task as a child of our cancellation token.
        let cache = self.cache.clone();
        let total_cached = self.total_cached.clone();
        let ttl = self.config.ttl;
        let child = self.cancel.child_token();

        tokio::spawn(async move {
            let interval = Duration::from_secs(60);
            loop {
                tokio::select! {
                    _ = child.cancelled() => {
                        debug!("TTL sweep task cancelled");
                        break;
                    }
                    _ = tokio::time::sleep(interval) => {
                        let now = Instant::now();
                        let expired: Vec<String> = cache
                            .iter()
                            .filter(|entry| now.duration_since(entry.value().created_at) > ttl)
                            .map(|entry| entry.key().clone())
                            .collect();

                        for tid in &expired {
                            if let Some((_, cached)) = cache.remove(tid) {
                                total_cached.fetch_sub(cached.file_size, Ordering::Relaxed);
                            }
                        }

                        if !expired.is_empty() {
                            info!(count = expired.len(), "TTL sweep evicted entries");
                        }
                    }
                }
            }
        });

        info!(
            ttl = ?self.config.ttl,
            max_file = %self.config.max_file_size,
            max_total = %self.config.max_total_size,
            "file relay plugin started"
        );

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel.cancel();
        info!("file relay plugin stopped");
        Ok(())
    }
}

/// Factory for creating [`FileTransferRelayPlugin`] from TOML config.
pub struct FileTransferRelayFactory;

impl crate::plugin::PluginFactory for FileTransferRelayFactory {
    fn name(&self) -> &str {
        "file-relay"
    }

    fn create(&self, config: Option<toml::Value>) -> anyhow::Result<Box<dyn ServerPlugin>> {
        let config: RelayCacheConfig = match config {
            Some(v) => v.try_into()?,
            None => RelayCacheConfig::default(),
        };
        Ok(Box::new(FileTransferRelayPlugin::with_config(config)))
    }
}
