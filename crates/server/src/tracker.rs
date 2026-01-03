use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info};

pub type InfoHash = [u8; 20];
pub type PeerId = [u8; 20];

#[derive(Debug, Clone)]
pub struct Peer {
    pub peer_id: PeerId,
    pub user_id: u64,        // Rumble user ID
    pub ip: IpAddr,
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub last_seen: Instant,
    pub needs_relay: bool,   // Client requested relay mode
}

impl Peer {
    /// Returns true if this peer is a seeder (has the complete file).
    pub fn is_seeder(&self) -> bool {
        self.left == 0
    }
}

#[derive(Debug)]
pub struct Swarm {
    pub peers: HashMap<PeerId, Peer>,
    pub completed: u32, // Number of seeders (left=0)
}

impl Swarm {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            completed: 0,
        }
    }

    /// Prune peers that haven't been seen within the timeout.
    /// Returns true if the swarm is now empty.
    pub fn prune(&mut self, timeout: Duration) -> bool {
        let now = Instant::now();
        self.peers.retain(|_, peer| {
            now.duration_since(peer.last_seen) < timeout
        });
        // Re-calculate completed count
        self.completed = self.peers.values().filter(|p| p.left == 0).count() as u32;
        self.peers.is_empty()
    }

    /// Returns true if the swarm has any seeders.
    pub fn has_seeders(&self) -> bool {
        self.completed > 0
    }
}

#[derive(Debug)]
pub struct Tracker {
    pub swarms: RwLock<HashMap<InfoHash, Swarm>>,
    pub peer_timeout: Duration,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            swarms: RwLock::new(HashMap::new()),
            peer_timeout: Duration::from_secs(60 * 30), // 30 minutes
        }
    }

    /// Spawn a background task that periodically prunes inactive peers.
    pub fn spawn_cleanup_task(self: &Arc<Self>) {
        let tracker = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                tracker.prune_all().await;
            }
        });
    }

    /// Prune inactive peers from all swarms and remove empty swarms.
    async fn prune_all(&self) {
        let mut swarms = self.swarms.write().await;
        let timeout = self.peer_timeout;

        // Collect swarms to remove (those that are empty after pruning)
        let mut to_remove = Vec::new();

        for (info_hash, swarm) in swarms.iter_mut() {
            if swarm.prune(timeout) {
                to_remove.push(*info_hash);
            }
        }

        // Remove empty swarms
        for info_hash in to_remove {
            swarms.remove(&info_hash);
            debug!("Removed empty swarm: {}", hex::encode(info_hash));
        }
    }

    pub async fn announce(
        &self,
        info_hash: InfoHash,
        peer_id: PeerId,
        user_id: u64,
        ip: IpAddr,
        port: u16,
        uploaded: u64,
        downloaded: u64,
        left: u64,
        event: Option<api::proto::tracker_announce::Event>,
        needs_relay: bool,
    ) -> (u32, u32, Vec<Peer>) {
        let mut swarms = self.swarms.write().await;
        let swarm = swarms.entry(info_hash).or_insert_with(Swarm::new);

        // Handle event
        use api::proto::tracker_announce::Event;
        match event {
            Some(Event::Stopped) => {
                swarm.peers.remove(&peer_id);
                info!("Peer stopped: {} for infohash {}", hex::encode(peer_id), hex::encode(info_hash));
            }
            _ => {
                let peer = Peer {
                    peer_id,
                    user_id,
                    ip,
                    port,
                    uploaded,
                    downloaded,
                    left,
                    last_seen: Instant::now(),
                    needs_relay,
                };
                swarm.peers.insert(peer_id, peer);
            }
        }

        // Update seeder count
        swarm.completed = swarm.peers.values().filter(|p| p.left == 0).count() as u32;

        let complete = swarm.completed;
        let incomplete = swarm.peers.len() as u32 - complete;

        // Select peers to return: prioritize seeders, then randomize
        // Collect all peers except the requester
        let mut seeders: Vec<Peer> = swarm
            .peers
            .values()
            .filter(|p| p.peer_id != peer_id && p.is_seeder())
            .cloned()
            .collect();

        let mut leechers: Vec<Peer> = swarm
            .peers
            .values()
            .filter(|p| p.peer_id != peer_id && !p.is_seeder())
            .cloned()
            .collect();

        // Shuffle both lists
        let mut rng = rand::thread_rng();
        seeders.shuffle(&mut rng);
        leechers.shuffle(&mut rng);

        // Return seeders first, then leechers, up to 50 total
        let mut peers = Vec::with_capacity(50);
        peers.extend(seeders.into_iter().take(50));
        let remaining = 50 - peers.len();
        peers.extend(leechers.into_iter().take(remaining));

        (complete, incomplete, peers)
    }

    pub async fn scrape(&self, info_hashes: Vec<InfoHash>) -> HashMap<InfoHash, (u32, u32, u32)> {
        let swarms = self.swarms.read().await;
        let mut results = HashMap::new();

        for hash in info_hashes {
            if let Some(swarm) = swarms.get(&hash) {
                let complete = swarm.completed;
                let downloaded = 0; // We don't track total downloads yet
                let incomplete = swarm.peers.len() as u32 - complete;
                results.insert(hash, (complete, downloaded, incomplete));
            }
        }
        results
    }
}
