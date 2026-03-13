//! BitTorrent-based implementation of the `FileTransferPlugin` trait.
//!
//! Wraps [`TorrentManager`](crate::torrent::TorrentManager) and bridges
//! the async BitTorrent operations into the synchronous trait interface
//! using `tokio::task::block_in_place`.

use std::path::PathBuf;

use rumble_client::file_transfer::{
    FileOffer, FileTransferPlugin, PluginPeerConnectionType, PluginPeerInfo, PluginPeerState, PluginTransferState,
    TransferId, TransferStatus,
};

use crate::torrent::TorrentManager;

/// BitTorrent-based file transfer plugin using librqbit.
///
/// Created via [`BitTorrentFileTransfer::new`] which takes a raw `quinn::Connection`
/// and a temporary directory for downloads.
pub struct BitTorrentFileTransfer {
    manager: TorrentManager,
}

impl BitTorrentFileTransfer {
    /// Create a new BitTorrent file transfer plugin.
    ///
    /// This is async because `TorrentManager::new` starts a librqbit session.
    pub async fn new(connection: quinn::Connection, temp_dir: PathBuf) -> anyhow::Result<Self> {
        let manager = TorrentManager::new(connection, temp_dir).await?;
        Ok(Self { manager })
    }

    /// Access the underlying `TorrentManager` for operations not covered by the trait.
    pub fn manager(&self) -> &TorrentManager {
        &self.manager
    }

    /// Set whether this client needs relay mode for NAT traversal.
    pub fn set_needs_relay(&self, needs: bool) {
        self.manager.set_needs_relay(needs);
    }

    /// Helper: run an async block synchronously within the current tokio runtime.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
    }
}

impl FileTransferPlugin for BitTorrentFileTransfer {
    fn share(&self, path: PathBuf) -> anyhow::Result<FileOffer> {
        let info = Self::block_on(self.manager.share_file(path))?;
        Ok(FileOffer {
            id: TransferId(info.infohash.clone()),
            name: info.name,
            size: info.size,
            mime: info.mime,
            share_data: info.magnet,
        })
    }

    fn download(&self, magnet: &str) -> anyhow::Result<TransferId> {
        Self::block_on(self.manager.download_file(magnet.to_string()))?;
        // Extract infohash from magnet link
        let infohash = magnet
            .split("xt=urn:btih:")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .unwrap_or("")
            .to_string();
        Ok(TransferId(infohash))
    }

    fn transfers(&self) -> Vec<TransferStatus> {
        use librqbit::http_api_types::{PeerStatsFilter, PeerStatsSnapshot};

        self.manager.session().with_torrents(|iter| {
            let mut transfers = Vec::new();
            for (_id, handle) in iter {
                let stats = handle.stats();
                let name = handle.name().unwrap_or_else(|| "Unknown".to_string());
                let progress = if stats.total_bytes > 0 {
                    stats.progress_bytes as f32 / stats.total_bytes as f32
                } else {
                    0.0
                };

                let state = match stats.state {
                    librqbit::TorrentStatsState::Initializing => PluginTransferState::Initializing,
                    librqbit::TorrentStatsState::Paused => PluginTransferState::Paused,
                    librqbit::TorrentStatsState::Error => PluginTransferState::Error,
                    librqbit::TorrentStatsState::Live => {
                        if stats.finished {
                            PluginTransferState::Seeding
                        } else {
                            PluginTransferState::Downloading
                        }
                    }
                };

                let info_hash = handle.info_hash();
                let infohash_hex = hex::encode(info_hash.0);
                let magnet = format!("magnet:?xt=urn:btih:{}", &infohash_hex);

                let local_path = if stats.finished {
                    self.manager.get_file_path(&infohash_hex).ok()
                } else {
                    None
                };

                let (download_speed, upload_speed, peers) = stats
                    .live
                    .as_ref()
                    .map(|l| {
                        let dl = l.download_speed.as_bytes();
                        let ul = l.upload_speed.as_bytes();
                        let peer_stats = &l.snapshot.peer_stats;
                        let peer_count = peer_stats.live + peer_stats.queued + peer_stats.connecting;
                        (dl, ul, peer_count)
                    })
                    .unwrap_or((0, 0, 0));

                let peer_details = handle
                    .live()
                    .map(|live| {
                        let filter = PeerStatsFilter::default();
                        let snapshot: PeerStatsSnapshot = live.per_peer_stats_snapshot(filter);

                        snapshot
                            .peers
                            .into_iter()
                            .map(|(addr_str, peer_stats)| {
                                let addr: Option<std::net::SocketAddr> = addr_str.parse().ok();
                                let is_relay = addr.as_ref().map(|a| self.manager.is_relay_proxy(a)).unwrap_or(false);

                                let connection_type = if is_relay {
                                    PluginPeerConnectionType::Relay
                                } else if let Some(ref conn_kind) = peer_stats.conn_kind {
                                    let kind_str = serde_json::to_string(conn_kind)
                                        .unwrap_or_default()
                                        .trim_matches('"')
                                        .to_string();
                                    match kind_str.as_str() {
                                        "tcp" => PluginPeerConnectionType::Direct,
                                        "utp" => PluginPeerConnectionType::Utp,
                                        "socks" => PluginPeerConnectionType::Socks,
                                        _ => PluginPeerConnectionType::Direct,
                                    }
                                } else {
                                    PluginPeerConnectionType::Direct
                                };

                                let peer_state = match peer_stats.state {
                                    "live" => PluginPeerState::Live,
                                    "connecting" => PluginPeerState::Connecting,
                                    "queued" => PluginPeerState::Queued,
                                    _ => PluginPeerState::Dead,
                                };

                                let display_addr = if is_relay {
                                    addr.and_then(|a| self.manager.get_relay_original_addr(&a))
                                        .map(|a| a.to_string())
                                        .unwrap_or(addr_str.clone())
                                } else {
                                    addr_str.clone()
                                };

                                PluginPeerInfo {
                                    address: display_addr,
                                    connection_type,
                                    state: peer_state,
                                    downloaded_bytes: peer_stats.counters.fetched_bytes,
                                    uploaded_bytes: peer_stats.counters.uploaded_bytes,
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                let error = if matches!(state, PluginTransferState::Error) {
                    stats.error.as_deref().map(|s| s.to_string())
                } else {
                    None
                };

                transfers.push(TransferStatus {
                    id: TransferId(infohash_hex),
                    infohash: info_hash.0,
                    name,
                    size: stats.total_bytes,
                    progress,
                    download_speed,
                    upload_speed,
                    peers,
                    state,
                    is_finished: stats.finished,
                    error,
                    magnet: Some(magnet),
                    local_path,
                    peer_details,
                });
            }
            transfers
        })
    }

    fn pause(&self, id: &TransferId) -> anyhow::Result<()> {
        Self::block_on(self.manager.pause_transfer(&id.0))
    }

    fn resume(&self, id: &TransferId) -> anyhow::Result<()> {
        Self::block_on(self.manager.resume_transfer(&id.0))
    }

    fn cancel(&self, id: &TransferId, delete_files: bool) -> anyhow::Result<()> {
        Self::block_on(self.manager.cancel_transfer(&id.0, delete_files))
    }

    fn get_file_path(&self, id: &TransferId) -> anyhow::Result<PathBuf> {
        self.manager.get_file_path(&id.0)
    }
}
