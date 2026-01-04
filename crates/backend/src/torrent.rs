use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use librqbit::{
    AddTorrent, AddTorrentOptions, ManagedTorrent, ManagedTorrentState, Session,
    SessionOptions, ListenerOptions, api::TorrentIdOrHash,
};
use quinn::Connection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use api::proto;
use api::{encode_frame, try_decode_frame};
use api::proto::envelope::Payload;
use prost::Message;

/// Relay handshake role bytes
const RELAY_ROLE_ACCEPTOR: u8 = 0x01;
#[allow(dead_code)] // Used when dialing through relay (future implementation)
const RELAY_ROLE_DIALER: u8 = 0x02;

/// Normalizes IP addresses for BitTorrent peer connections:
/// - Unmaps IPv4-mapped IPv6 addresses (::ffff:x.x.x.x -> x.x.x.x)
/// - Converts IPv6 loopback (::1) to IPv4 loopback (127.0.0.1)
///
/// The loopback conversion is needed because librqbit listens on 0.0.0.0 (IPv4 only),
/// so peers reported as ::1 won't be reachable unless converted.
fn normalize_peer_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            // Handle IPv4-mapped addresses (::ffff:x.x.x.x)
            if let Some(v4) = v6.to_ipv4_mapped() {
                IpAddr::V4(v4)
            // Handle IPv6 loopback - convert to IPv4 loopback since our listener is IPv4-only
            } else if v6.is_loopback() {
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V6(v6)
            }
        }
        v4 => v4,
    }
}

/// Relay connection information
#[derive(Clone)]
pub struct RelayInfo {
    /// The relay token for authentication
    pub token: [u8; 32],
    /// The relay server port
    pub port: u16,
    /// The relay server IP (same as QUIC connection)
    pub host: IpAddr,
}

/// Information about a shared file, returned from share_file.
#[derive(Debug, Clone)]
pub struct SharedFileInfo {
    /// Original filename.
    pub name: String,
    /// File size in bytes.
    pub size: u64,
    /// MIME type.
    pub mime: String,
    /// 40-character hex-encoded infohash.
    pub infohash: String,
    /// Full magnet link.
    pub magnet: String,
}

impl SharedFileInfo {
    /// Guess MIME type from filename extension.
    fn guess_mime(filename: &str) -> String {
        let ext = std::path::Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        match ext.as_str() {
            // Images
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "ico" => "image/x-icon",
            "bmp" => "image/bmp",
            // Audio
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "ogg" => "audio/ogg",
            "flac" => "audio/flac",
            "m4a" => "audio/mp4",
            // Video
            "mp4" => "video/mp4",
            "webm" => "video/webm",
            "mkv" => "video/x-matroska",
            "avi" => "video/x-msvideo",
            "mov" => "video/quicktime",
            // Documents
            "pdf" => "application/pdf",
            "doc" => "application/msword",
            "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "xls" => "application/vnd.ms-excel",
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "ppt" => "application/vnd.ms-powerpoint",
            "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            // Text
            "txt" => "text/plain",
            "html" | "htm" => "text/html",
            "css" => "text/css",
            "js" => "text/javascript",
            "json" => "application/json",
            "xml" => "application/xml",
            "md" => "text/markdown",
            // Archives
            "zip" => "application/zip",
            "tar" => "application/x-tar",
            "gz" => "application/gzip",
            "rar" => "application/vnd.rar",
            "7z" => "application/x-7z-compressed",
            // Default
            _ => "application/octet-stream",
        }.to_string()
    }
}

pub struct TorrentManager {
    session: Arc<Session>,
    connection: Connection,
    listen_port: u16,
    /// The peer ID for this session.
    peer_id: [u8; 20],
    /// Cancellation token to stop all announce loops when the manager is dropped.
    cancel_token: CancellationToken,
    /// Whether this client needs relay mode (detected automatically)
    needs_relay: AtomicBool,
    /// Current relay info (if relay mode is enabled)
    relay_info: RwLock<Option<RelayInfo>>,
    /// Output folder for downloads.
    output_folder: PathBuf,
}

impl TorrentManager {
    pub async fn new(connection: Connection, temp_dir: PathBuf) -> anyhow::Result<Self> {
        let peer_id_bytes: [u8; 20] = rand::random();
        let peer_id = librqbit::dht::Id20::new(peer_id_bytes);

        let output_folder = temp_dir.clone();

        let session = Session::new_with_opts(
            temp_dir,
            SessionOptions {
                disable_dht: true,
                disable_dht_persistence: true,
                disable_local_service_discovery: true,
                peer_id: Some(peer_id),
                disable_trackers: false, // Enable trackers so add_torrent doesn't fail immediately
                listen: Some(ListenerOptions {
                    // Use IPv6 unspecified address for dual-stack support (both IPv4 and IPv6)
                    listen_addr: SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ).await?;

        let port = session.announce_port().ok_or(anyhow::anyhow!("Failed to get announce port"))?;

        Ok(Self {
            session,
            connection,
            listen_port: port,
            peer_id: peer_id_bytes,
            cancel_token: CancellationToken::new(),
            needs_relay: AtomicBool::new(false),
            relay_info: RwLock::new(None),
            output_folder,
        })
    }

    /// Set whether this client needs relay mode
    pub fn set_needs_relay(&self, needs: bool) {
        self.needs_relay.store(needs, Ordering::SeqCst);
    }

    /// Check if relay mode is needed
    pub fn needs_relay(&self) -> bool {
        self.needs_relay.load(Ordering::SeqCst)
    }

    /// Get current relay info
    pub async fn relay_info(&self) -> Option<RelayInfo> {
        self.relay_info.read().await.clone()
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub async fn share_file(&self, path: PathBuf) -> anyhow::Result<SharedFileInfo> {
        // Get file metadata
        let metadata = tokio::fs::metadata(&path).await?;
        let file_size = metadata.len();
        let file_name = path.file_name()
            .ok_or(anyhow::anyhow!("Invalid file path"))?
            .to_string_lossy()
            .into_owned();
        let mime = SharedFileInfo::guess_mime(&file_name);

        // Create torrent without trackers
        let opts = librqbit::CreateTorrentOptions {
            trackers: vec![], // No trackers
            ..Default::default()
        };

        let torrent_result = librqbit::create_torrent(&path, opts).await?;

        let output_folder = path.parent()
            .ok_or(anyhow::anyhow!("Invalid path"))?
            .to_string_lossy()
            .into_owned();

        // Add to session
        let handle = self.session.add_torrent(
            AddTorrent::from_bytes(torrent_result.as_bytes()?),
            Some(AddTorrentOptions {
                paused: false,
                output_folder: Some(output_folder),
                overwrite: true,
                ..Default::default()
            })
        ).await?;

        let managed_torrent = handle.into_handle().ok_or(anyhow::anyhow!("Failed to get handle"))?;
        let info_hash = managed_torrent.info_hash();

        // Spawn announce loop
        self.spawn_announce_loop(managed_torrent.clone());

        let infohash = hex::encode(info_hash.0);
        let magnet = format!("magnet:?xt=urn:btih:{}", infohash);

        Ok(SharedFileInfo {
            name: file_name,
            size: file_size,
            mime,
            infohash,
            magnet,
        })
    }

    pub async fn download_file(&self, magnet: String) -> anyhow::Result<()> {
        // Parse magnet to get infohash
        let info_hash_hex = magnet.split("xt=urn:btih:").nth(1)
            .ok_or(anyhow::anyhow!("Invalid magnet link"))?
            .split('&').next().unwrap();
        let info_hash_bytes = hex::decode(info_hash_hex)?;
        let info_hash: [u8; 20] = info_hash_bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid infohash length"))?;

        // Use the manager's peer_id for consistency
        let peer_id = self.peer_id;

        // Get initial peers from server
        let peers = match self.announce_to_server(info_hash, peer_id, self.listen_port, proto::tracker_announce::Event::Started as i32).await {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to resolve peers from server: {}", e);
                Vec::new()
            }
        };

        // Add to session
        let handle = self.session.add_torrent(
            AddTorrent::from_url(magnet),
            Some(AddTorrentOptions {
                paused: false,
                initial_peers: Some(peers),
                ..Default::default()
            })
        ).await?;
        
        let managed_torrent = handle.into_handle().ok_or(anyhow::anyhow!("Failed to get handle"))?;
        
        // Spawn announce loop
        self.spawn_announce_loop(managed_torrent.clone());
        
        Ok(())
    }

    async fn announce_to_server(&self, info_hash: [u8; 20], peer_id: [u8; 20], port: u16, event: i32) -> anyhow::Result<Vec<SocketAddr>> {
        let request_id = rand::random::<u32>();
        let needs_relay = self.needs_relay();

        let announce = proto::TrackerAnnounce {
            info_hash: info_hash.to_vec(),
            peer_id: peer_id.to_vec(),
            user_id: 0, // Will be set by server from authenticated session
            port: port as u32,
            uploaded: 0,
            downloaded: 0,
            left: 0, // Unknown
            event,
            numwant: 50,
            request_id,
            needs_relay,
        };

        let (mut send, mut recv) = self.connection.open_bi().await?;
        let env = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::TrackerAnnounce(announce)),
        };
        let frame = encode_frame(&env);
        send.write_all(&frame).await?;

        // Read response
        let mut buf = bytes::BytesMut::new();
        let mut chunk = [0u8; 1024];

        let response = loop {
            let n = recv.read(&mut chunk).await?.ok_or(anyhow::anyhow!("Stream closed"))?;
            buf.extend_from_slice(&chunk[..n]);
            if let Some(frame) = try_decode_frame(&mut buf) {
                let env = proto::Envelope::decode(&*frame)?;
                if let Some(Payload::TrackerAnnounceResponse(r)) = env.payload {
                    break r;
                }
            }
        };

        // Handle relay info if provided
        if let Some(relay) = response.relay {
            if let Ok(token_bytes) = hex::decode(&relay.relay_token) {
                if token_bytes.len() == 32 {
                    let mut token = [0u8; 32];
                    token.copy_from_slice(&token_bytes);
                    let relay_info = RelayInfo {
                        token,
                        port: relay.relay_port as u16,
                        host: self.connection.remote_address().ip(),
                    };
                    *self.relay_info.write().await = Some(relay_info.clone());
                    info!("Received relay info: port={}", relay.relay_port);

                    // Connect to relay as acceptor
                    self.connect_to_relay_as_acceptor(relay_info).await;
                }
            }
        }

        let mut peers = Vec::new();
        for peer in response.peers {
            if let Ok(ip) = peer.ip.parse::<IpAddr>() {
                let ip = normalize_peer_ip(ip);
                let addr = SocketAddr::new(ip, peer.port as u16);
                // Skip self
                if addr.port() == port {
                    continue;
                }
                peers.push(addr);
            }
        }
        info!("Resolved {} peers from server", peers.len());
        for peer in &peers {
            debug!("Initial peer: {}", peer);
        }
        Ok(peers)
    }

    /// Connect to the relay service as an acceptor (NAT'd client waiting for peers).
    async fn connect_to_relay_as_acceptor(&self, relay_info: RelayInfo) {
        let addr = SocketAddr::new(relay_info.host, relay_info.port);
        let token = relay_info.token;
        let cancel_token = self.cancel_token.clone();

        tokio::spawn(async move {
            loop {
                // Check for cancellation
                if cancel_token.is_cancelled() {
                    debug!("Relay acceptor cancelled");
                    break;
                }

                match TcpStream::connect(addr).await {
                    Ok(mut stream) => {
                        info!("Connected to relay as acceptor at {}", addr);

                        // Send handshake: role byte + token
                        if let Err(e) = stream.write_u8(RELAY_ROLE_ACCEPTOR).await {
                            warn!("Failed to write relay role: {}", e);
                            continue;
                        }
                        if let Err(e) = stream.write_all(&token).await {
                            warn!("Failed to write relay token: {}", e);
                            continue;
                        }

                        // Handshake complete - the relay will bridge to a dialer
                        // The connection stays open until a peer connects through the relay
                        // or the connection is closed
                        info!("Relay acceptor registered, waiting for dialers");

                        // Keep the connection open and forward BitTorrent traffic
                        // For now, we just wait for the connection to close
                        let mut buf = [0u8; 1];
                        loop {
                            tokio::select! {
                                _ = cancel_token.cancelled() => {
                                    debug!("Relay acceptor cancelled while waiting");
                                    return;
                                }
                                result = stream.read(&mut buf) => {
                                    match result {
                                        Ok(0) => {
                                            debug!("Relay connection closed");
                                            break;
                                        }
                                        Ok(_) => {
                                            // Data received - this would be BitTorrent traffic
                                            // In a full implementation, we'd forward this to librqbit
                                            debug!("Received data from relay");
                                        }
                                        Err(e) => {
                                            debug!("Relay read error: {}", e);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to connect to relay at {}: {}", addr, e);
                    }
                }

                // Wait before reconnecting
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    /// Returns a clone of the cancellation token for this manager.
    /// Can be used to stop all announce loops.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    fn spawn_announce_loop(&self, torrent: Arc<ManagedTorrent>) {
        let connection = self.connection.clone();
        let info_hash_bytes = torrent.info_hash().0.to_vec();
        let peer_id_bytes = torrent.shared.peer_id.0.to_vec();
        let listen_port = self.listen_port;
        let cancel_token = self.cancel_token.clone();
        let needs_relay = self.needs_relay.load(Ordering::SeqCst);

        tokio::spawn(async move {
            let mut interval = Duration::from_secs(10); // Initial interval
            let mut min_interval = Duration::from_secs(60); // Minimum interval between announces
            let mut event = proto::tracker_announce::Event::Started;

            loop {
                // Check for cancellation or connection close
                if cancel_token.is_cancelled() {
                    debug!("Announce loop cancelled");
                    break;
                }

                // Check if torrent is finished (for downloads) - we can reduce announce frequency
                let stats = torrent.stats();
                let is_seeding = stats.finished;

                // Construct announce request
                let request_id = rand::random::<u32>();

                let announce = proto::TrackerAnnounce {
                    info_hash: info_hash_bytes.clone(),
                    peer_id: peer_id_bytes.clone(),
                    user_id: 0, // Will be set by server from authenticated session
                    port: listen_port as u32,
                    uploaded: stats.uploaded_bytes,
                    downloaded: stats.progress_bytes,
                    left: stats.total_bytes.saturating_sub(stats.progress_bytes),
                    event: event as i32,
                    numwant: 50,
                    request_id,
                    needs_relay,
                };

                // Send via QUIC
                match connection.open_bi().await {
                    Ok((mut send, mut recv)) => {
                        let env = proto::Envelope {
                            state_hash: Vec::new(),
                            payload: Some(Payload::TrackerAnnounce(announce)),
                        };
                        let frame = encode_frame(&env);
                        if let Err(e) = send.write_all(&frame).await {
                            error!("Failed to send TrackerAnnounce: {}", e);
                        } else {
                            // Read response
                            let mut buf = bytes::BytesMut::new();
                            let mut chunk = [0u8; 1024];

                            let read_future = async {
                                loop {
                                    let n = recv.read(&mut chunk).await?.ok_or(anyhow::anyhow!("Stream closed"))?;
                                    buf.extend_from_slice(&chunk[..n]);
                                    if let Some(frame) = try_decode_frame(&mut buf) {
                                        let env = proto::Envelope::decode(&*frame)?;
                                        if let Some(Payload::TrackerAnnounceResponse(r)) = env.payload {
                                            return Ok::<_, anyhow::Error>(r);
                                        }
                                    }
                                }
                            };

                            match tokio::time::timeout(Duration::from_secs(10), read_future).await {
                                Ok(Ok(response)) => {
                                    // Respect interval from server, but not less than min_interval
                                    interval = Duration::from_secs(response.interval.max(60) as u64);
                                    if response.min_interval > 0 {
                                        min_interval = Duration::from_secs(response.min_interval as u64);
                                    }
                                    // Ensure we don't announce more frequently than min_interval
                                    if interval < min_interval {
                                        interval = min_interval;
                                    }

                                    // Add peers
                                    torrent.with_state(|state| {
                                        if let ManagedTorrentState::Live(live) = state {
                                            for peer in response.peers {
                                                if let Ok(ip) = peer.ip.parse::<IpAddr>() {
                                                    let ip = normalize_peer_ip(ip);
                                                    let addr = SocketAddr::new(ip, peer.port as u16);
                                                    // Skip self
                                                    if addr.port() == listen_port {
                                                        continue;
                                                    }
                                                    debug!("Adding peer: {}", addr);
                                                    let _ = live.add_peer_if_not_seen(addr);
                                                }
                                            }
                                        }
                                    });
                                }
                                Ok(Err(e)) => {
                                    warn!("TrackerAnnounce failed: {}", e);
                                }
                                Err(_) => {
                                    warn!("TrackerAnnounce timed out");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Connection closed - stop the announce loop
                        error!("Failed to open stream for TrackerAnnounce: {} - stopping announce loop", e);
                        break;
                    }
                }

                if event == proto::tracker_announce::Event::Started {
                    event = proto::tracker_announce::Event::None;
                }

                // For seeders, we can be less aggressive with announces
                let sleep_duration = if is_seeding {
                    interval.max(Duration::from_secs(300)) // At least 5 minutes between seeder announces
                } else {
                    interval
                };

                // Wait for interval or cancellation
                tokio::select! {
                    _ = tokio::time::sleep(sleep_duration) => {}
                    _ = cancel_token.cancelled() => {
                        debug!("Announce loop cancelled during sleep");
                        break;
                    }
                }
            }

            debug!("Announce loop ended for infohash: {}", hex::encode(&info_hash_bytes));
        });
    }

    /// Pause a torrent by infohash (hex-encoded).
    pub async fn pause_transfer(&self, infohash: &str) -> anyhow::Result<()> {
        let hash_bytes = hex::decode(infohash)?;
        let hash: [u8; 20] = hash_bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid infohash length"))?;
        let id = librqbit::dht::Id20::new(hash);

        let handle = self.session.get(TorrentIdOrHash::Hash(id))
            .ok_or_else(|| anyhow::anyhow!("Transfer not found"))?;

        self.session.pause(&handle).await?;
        info!("Paused transfer: {}", infohash);
        Ok(())
    }

    /// Resume a paused torrent by infohash (hex-encoded).
    pub async fn resume_transfer(&self, infohash: &str) -> anyhow::Result<()> {
        let hash_bytes = hex::decode(infohash)?;
        let hash: [u8; 20] = hash_bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid infohash length"))?;
        let id = librqbit::dht::Id20::new(hash);

        let handle = self.session.get(TorrentIdOrHash::Hash(id))
            .ok_or_else(|| anyhow::anyhow!("Transfer not found"))?;

        self.session.unpause(&handle).await?;
        info!("Resumed transfer: {}", infohash);
        Ok(())
    }

    /// Cancel and remove a torrent by infohash (hex-encoded).
    pub async fn cancel_transfer(&self, infohash: &str, delete_files: bool) -> anyhow::Result<()> {
        let hash_bytes = hex::decode(infohash)?;
        let hash: [u8; 20] = hash_bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid infohash length"))?;
        let id = librqbit::dht::Id20::new(hash);

        self.session.delete(TorrentIdOrHash::Hash(id), delete_files).await?;
        info!("Cancelled transfer: {} (delete_files={})", infohash, delete_files);
        Ok(())
    }

    /// Get the local file path for a completed torrent.
    pub fn get_file_path(&self, infohash: &str) -> anyhow::Result<std::path::PathBuf> {
        let hash_bytes = hex::decode(infohash)?;
        let hash: [u8; 20] = hash_bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid infohash length"))?;
        let id = librqbit::dht::Id20::new(hash);

        let handle = self.session.get(TorrentIdOrHash::Hash(id))
            .ok_or_else(|| anyhow::anyhow!("Transfer not found"))?;

        // Get the file name from the torrent metadata
        let name = handle.name().ok_or_else(|| anyhow::anyhow!("No file name available"))?;

        Ok(self.output_folder.join(name))
    }
}

impl Drop for TorrentManager {
    fn drop(&mut self) {
        // Cancel all announce loops when the manager is dropped
        self.cancel_token.cancel();
    }
}
