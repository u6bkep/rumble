//! BitTorrent tracker plugin for file transfer.
//!
//! Wraps the existing [`Tracker`] into a [`ServerPlugin`] so that
//! `TrackerAnnounce` and `TrackerScrape` messages are dispatched through the
//! plugin system instead of being handled inline in `handlers.rs`.

use std::sync::{Arc, atomic::Ordering};

use anyhow::Result;
use api::{
    ROOT_ROOM_UUID, encode_frame,
    permissions::Permissions,
    proto::{self, envelope::Payload},
};
use tracing::{debug, info};

use crate::{
    acl,
    handlers::send_permission_denied,
    plugin::{ServerCtx, ServerPlugin},
    state::ClientHandle,
    tracker::Tracker,
};

/// Plugin that handles BitTorrent tracker announce and scrape messages.
///
/// Owns its own [`Tracker`] instance and processes `TrackerAnnounce` and
/// `TrackerScrape` payloads, generating relay tokens via the server state's
/// `RelayTokenManager` when clients request NAT relay mode.
pub struct FileTransferBittorrentPlugin {
    tracker: Arc<Tracker>,
}

impl FileTransferBittorrentPlugin {
    /// Create a new plugin with a fresh tracker state.
    pub fn new() -> Self {
        let tracker = Arc::new(Tracker::new());
        tracker.spawn_cleanup_task();
        Self { tracker }
    }

    async fn handle_announce(
        &self,
        msg: &proto::TrackerAnnounce,
        sender: &Arc<ClientHandle>,
        ctx: &ServerCtx,
    ) -> Result<bool> {
        // Require authentication
        if !sender.authenticated.load(Ordering::SeqCst) {
            return Ok(true);
        }

        let state = ctx.state();
        let persistence = ctx.persistence().cloned();

        // Permission check: SHARE_FILE in sender's room
        let sender_room = state.get_user_room(sender.user_id).await.unwrap_or(ROOT_ROOM_UUID);
        if let Err(denied) =
            acl::check_permission(state, sender, sender_room, Permissions::SHARE_FILE, &persistence).await
        {
            send_permission_denied(sender, denied).await?;
            return Ok(true);
        }

        let info_hash: [u8; 20] = msg
            .info_hash
            .clone()
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("Invalid info_hash length {}, expected 20", v.len()))?;
        let peer_id: [u8; 20] = msg
            .peer_id
            .clone()
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("Invalid peer_id length {}, expected 20", v.len()))?;

        // Use the authenticated user ID from the sender, not from the message
        // This prevents spoofing
        let user_id = sender.user_id;

        info!(
            "Received TrackerAnnounce from user={} info_hash={} needs_relay={}",
            user_id,
            hex::encode(info_hash),
            msg.needs_relay
        );

        // Use the client's IP address from the connection
        let ip = sender.conn.remote_address().ip();

        let event = proto::tracker_announce::Event::try_from(msg.event).ok();

        // Generate relay token BEFORE announce if client needs relay
        let relay_port = state.relay_port();
        let relay_token = if msg.needs_relay && relay_port > 0 {
            Some(state.relay_tokens.generate_token(user_id))
        } else {
            if msg.needs_relay && relay_port == 0 {
                debug!("Client requested relay but relay service is not enabled");
            }
            None
        };

        let (complete, incomplete, peers) = self
            .tracker
            .announce(
                info_hash,
                peer_id,
                user_id,
                ip,
                msg.port as u16,
                msg.uploaded,
                msg.downloaded,
                msg.left,
                event,
                msg.needs_relay,
                relay_token,
            )
            .await;

        // Check if any peers need relay - if so, client needs to know the relay port
        let has_relay_peers = peers.iter().any(|p| p.needs_relay && p.relay_token.is_some());

        // Include relay info if:
        // 1. Client requested relay mode (they need their own token)
        // 2. OR there are relay peers (client needs to know relay port to reach them)
        let relay = if relay_port > 0 && (msg.needs_relay || has_relay_peers) {
            let token = if msg.needs_relay {
                relay_token.map(|t| hex::encode(t)).unwrap_or_default()
            } else {
                // Client doesn't need a token for themselves, but we still tell them the port
                String::new()
            };
            Some(proto::RelayInfo {
                relay_token: token,
                relay_port: relay_port as u32,
            })
        } else {
            None
        };

        let response = proto::TrackerAnnounceResponse {
            interval: 1800,
            min_interval: 60,
            complete,
            incomplete,
            peers: peers
                .into_iter()
                .map(|p| proto::PeerInfo {
                    peer_id: p.peer_id.to_vec(),
                    user_id: p.user_id,
                    ip: p.ip.to_string(),
                    port: p.port as u32,
                    supports_relay: p.needs_relay,
                    relay_token: p.relay_token.map(|t| hex::encode(t)),
                })
                .collect(),
            request_id: msg.request_id,
            relay,
        };

        let response_envelope = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::TrackerAnnounceResponse(response)),
        };

        let frame = encode_frame(&response_envelope);
        sender.send_frame(&frame).await?;

        Ok(true)
    }

    async fn handle_scrape(&self, msg: &proto::TrackerScrape, sender: &Arc<ClientHandle>) -> Result<bool> {
        // Require authentication
        if !sender.authenticated.load(Ordering::SeqCst) {
            return Ok(true);
        }

        let info_hashes: Vec<[u8; 20]> = msg
            .info_hashes
            .iter()
            .filter_map(|h| h.clone().try_into().ok())
            .collect();

        let stats = self.tracker.scrape(info_hashes).await;

        let mut files = std::collections::HashMap::new();
        for (hash, (complete, downloaded, incomplete)) in stats {
            let hash_hex = hex::encode(hash);
            files.insert(
                hash_hex,
                proto::ScrapeStats {
                    complete,
                    downloaded,
                    incomplete,
                },
            );
        }

        let response = proto::TrackerScrapeResponse { files };

        let envelope = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::TrackerScrapeResponse(response)),
        };

        let frame = encode_frame(&envelope);
        sender.send_frame(&frame).await?;

        Ok(true)
    }
}

impl Default for FileTransferBittorrentPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ServerPlugin for FileTransferBittorrentPlugin {
    fn name(&self) -> &str {
        "file-transfer-bittorrent"
    }

    async fn on_message(
        &self,
        envelope: &proto::Envelope,
        sender: &Arc<ClientHandle>,
        ctx: &ServerCtx,
    ) -> Result<bool> {
        match &envelope.payload {
            Some(Payload::TrackerAnnounce(ta)) => self.handle_announce(ta, sender, ctx).await,
            Some(Payload::TrackerScrape(ts)) => self.handle_scrape(ts, sender).await,
            _ => Ok(false),
        }
    }

    async fn on_disconnect(&self, client: &Arc<ClientHandle>, _ctx: &ServerCtx) {
        // Remove all announcements for the disconnecting client from the tracker
        let user_id = client.user_id;
        let mut swarms = self.tracker.swarms.write().await;
        let mut empty_hashes = Vec::new();
        for (info_hash, swarm) in swarms.iter_mut() {
            swarm.peers.retain(|_, peer| peer.user_id != user_id);
            swarm.completed = swarm.peers.values().filter(|p| p.left == 0).count() as u32;
            if swarm.peers.is_empty() {
                empty_hashes.push(*info_hash);
            }
        }
        for hash in empty_hashes {
            swarms.remove(&hash);
        }
    }
}
