//! BitTorrent Relay Service for NAT Traversal
//!
//! This module implements a TCP relay service that allows NAT'd clients to
//! participate in BitTorrent transfers by proxying connections through the server.
//!
//! # Protocol
//!
//! The relay uses a simple handshake protocol:
//!
//! 1. Client connects to relay TCP port
//! 2. Client sends a 1-byte role indicator: 0x01 for acceptor, 0x02 for dialer
//! 3. Client sends their 32-byte relay token
//! 4. For dialers: server bridges to the matching acceptor
//!    For acceptors: server waits for a matching dialer
//! 5. Once matched, raw BitTorrent protocol bytes are forwarded bidirectionally
//!
//! # Rate Limiting
//!
//! Each user has a configurable bandwidth limit for relay traffic.
//! Default: 1 MB/s per user, 10 MB/s global.

use dashmap::DashMap;
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::RwLock,
};
use tracing::{debug, error, info, warn};

/// Relay handshake role bytes
const ROLE_ACCEPTOR: u8 = 0x01;
const ROLE_DIALER: u8 = 0x02;

/// Default rate limits
const DEFAULT_USER_RATE_LIMIT: u64 = 1_048_576; // 1 MB/s per user
const DEFAULT_GLOBAL_RATE_LIMIT: u64 = 10_485_760; // 10 MB/s global

/// Relay token (32 bytes, hex-encoded as 64 chars in protocol)
pub type RelayToken = [u8; 32];

/// Relay session waiting for a peer connection
struct WaitingAcceptor {
    stream: TcpStream,
    user_id: u64,
    registered_at: Instant,
}

/// Active relay session state (for future monitoring/metrics)
#[allow(dead_code)]
struct RelaySession {
    user_id: u64,
    bytes_transferred: AtomicU64,
    started_at: Instant,
}

/// Rate limiter for a user
struct UserRateLimit {
    bytes_this_second: AtomicU64,
    last_reset: RwLock<Instant>,
    limit_bytes_per_sec: u64,
}

impl UserRateLimit {
    fn new(limit: u64) -> Self {
        Self {
            bytes_this_second: AtomicU64::new(0),
            last_reset: RwLock::new(Instant::now()),
            limit_bytes_per_sec: limit,
        }
    }

    async fn try_consume(&self, bytes: u64) -> bool {
        let now = Instant::now();
        {
            let mut last = self.last_reset.write().await;
            if now.duration_since(*last) >= Duration::from_secs(1) {
                self.bytes_this_second.store(0, Ordering::SeqCst);
                *last = now;
            }
        }

        let current = self.bytes_this_second.fetch_add(bytes, Ordering::SeqCst);
        if current + bytes > self.limit_bytes_per_sec {
            // Over limit, undo the add
            self.bytes_this_second.fetch_sub(bytes, Ordering::SeqCst);
            false
        } else {
            true
        }
    }
}

/// Configuration for the relay service
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Port to listen on for relay connections
    pub port: u16,
    /// Per-user bandwidth limit in bytes/sec
    pub user_rate_limit: u64,
    /// Global bandwidth limit in bytes/sec
    pub global_rate_limit: u64,
    /// Timeout for acceptors waiting for dialers
    pub acceptor_timeout: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            port: 0, // OS assigns port
            user_rate_limit: DEFAULT_USER_RATE_LIMIT,
            global_rate_limit: DEFAULT_GLOBAL_RATE_LIMIT,
            acceptor_timeout: Duration::from_secs(60),
        }
    }
}

/// Token manager for relay authentication
pub struct RelayTokenManager {
    /// Maps relay tokens to user IDs
    tokens: DashMap<RelayToken, u64>,
    /// Maps user_id to their current token (for cleanup)
    user_tokens: DashMap<u64, RelayToken>,
}

impl RelayTokenManager {
    pub fn new() -> Self {
        Self {
            tokens: DashMap::new(),
            user_tokens: DashMap::new(),
        }
    }

    /// Generate a new relay token for a user
    pub fn generate_token(&self, user_id: u64) -> RelayToken {
        // Revoke any existing token for this user
        if let Some((_, old_token)) = self.user_tokens.remove(&user_id) {
            self.tokens.remove(&old_token);
        }

        let token: RelayToken = rand::random();
        self.tokens.insert(token, user_id);
        self.user_tokens.insert(user_id, token);
        token
    }

    /// Validate a token and return the user ID
    pub fn validate_token(&self, token: &RelayToken) -> Option<u64> {
        self.tokens.get(token).map(|r| *r.value())
    }

    /// Revoke a user's token
    pub fn revoke_token(&self, user_id: u64) {
        if let Some((_, token)) = self.user_tokens.remove(&user_id) {
            self.tokens.remove(&token);
        }
    }
}

impl Default for RelayTokenManager {
    fn default() -> Self {
        Self::new()
    }
}

/// The relay service
pub struct RelayService {
    config: RelayConfig,
    token_manager: Arc<RelayTokenManager>,
    /// Acceptors waiting for dialers, keyed by token
    waiting_acceptors: Arc<DashMap<RelayToken, WaitingAcceptor>>,
    /// Rate limiters per user
    user_rate_limits: Arc<DashMap<u64, Arc<UserRateLimit>>>,
    /// Global rate limiter
    global_rate_limit: Arc<UserRateLimit>,
    /// Active sessions for monitoring
    active_sessions: Arc<AtomicU64>,
    /// The actual listening port (may differ from config if port was 0)
    listening_port: Arc<RwLock<u16>>,
}

impl RelayService {
    pub fn new(config: RelayConfig, token_manager: Arc<RelayTokenManager>) -> Self {
        let global_rate_limit = Arc::new(UserRateLimit::new(config.global_rate_limit));
        Self {
            config,
            token_manager,
            waiting_acceptors: Arc::new(DashMap::new()),
            user_rate_limits: Arc::new(DashMap::new()),
            global_rate_limit,
            active_sessions: Arc::new(AtomicU64::new(0)),
            listening_port: Arc::new(RwLock::new(0)),
        }
    }

    /// Get the port the relay is listening on
    pub async fn port(&self) -> u16 {
        *self.listening_port.read().await
    }

    /// Get the token manager
    pub fn token_manager(&self) -> &Arc<RelayTokenManager> {
        &self.token_manager
    }

    /// Start the relay service
    pub async fn run(self: Arc<Self>, bind_addr: SocketAddr) -> anyhow::Result<()> {
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;
        *self.listening_port.write().await = local_addr.port();

        info!("Relay service listening on {}", local_addr);

        // Spawn cleanup task for stale acceptors
        let cleanup_self = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                cleanup_self.cleanup_stale_acceptors().await;
            }
        });

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    debug!("Relay connection from {}", peer_addr);
                    let service = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = service.handle_connection(stream, peer_addr).await {
                            debug!("Relay connection error from {}: {}", peer_addr, e);
                        }
                    });
                }
                Err(e) => {
                    error!("Relay accept error: {}", e);
                }
            }
        }
    }

    async fn cleanup_stale_acceptors(&self) {
        let now = Instant::now();
        let timeout = self.config.acceptor_timeout;

        let mut stale_tokens = Vec::new();
        for entry in self.waiting_acceptors.iter() {
            if now.duration_since(entry.value().registered_at) > timeout {
                stale_tokens.push(*entry.key());
            }
        }

        for token in stale_tokens {
            if self.waiting_acceptors.remove(&token).is_some() {
                debug!("Removed stale acceptor with token {}", hex::encode(token));
            }
        }
    }

    async fn handle_connection(&self, mut stream: TcpStream, peer_addr: SocketAddr) -> anyhow::Result<()> {
        // Set TCP nodelay for lower latency
        stream.set_nodelay(true)?;

        // Read role byte
        let role = stream.read_u8().await?;

        // Read 32-byte token
        let mut token = [0u8; 32];
        stream.read_exact(&mut token).await?;

        // Validate token
        let user_id = match self.token_manager.validate_token(&token) {
            Some(id) => id,
            None => {
                warn!("Invalid relay token from {}", peer_addr);
                return Ok(());
            }
        };

        match role {
            ROLE_ACCEPTOR => self.handle_acceptor(stream, token, user_id, peer_addr).await,
            ROLE_DIALER => self.handle_dialer(stream, token, user_id, peer_addr).await,
            _ => {
                warn!("Invalid role byte {} from {}", role, peer_addr);
                Ok(())
            }
        }
    }

    async fn handle_acceptor(
        &self,
        stream: TcpStream,
        token: RelayToken,
        user_id: u64,
        peer_addr: SocketAddr,
    ) -> anyhow::Result<()> {
        info!(
            "Acceptor registered: user={} token={} from {}",
            user_id,
            hex::encode(token),
            peer_addr
        );

        // Store the waiting acceptor
        let acceptor = WaitingAcceptor {
            stream,
            user_id,
            registered_at: Instant::now(),
        };
        self.waiting_acceptors.insert(token, acceptor);

        Ok(())
    }

    async fn handle_dialer(
        &self,
        dialer_stream: TcpStream,
        token: RelayToken,
        user_id: u64,
        peer_addr: SocketAddr,
    ) -> anyhow::Result<()> {
        // Find matching acceptor
        let acceptor = match self.waiting_acceptors.remove(&token) {
            Some((_, a)) => a,
            None => {
                debug!(
                    "No acceptor waiting for token {} from {}",
                    hex::encode(token),
                    peer_addr
                );
                return Ok(());
            }
        };

        info!(
            "Relay bridge established: dialer={} -> acceptor={} (token={})",
            user_id,
            acceptor.user_id,
            hex::encode(token)
        );

        self.active_sessions.fetch_add(1, Ordering::SeqCst);

        // Get or create rate limiters for both users
        let dialer_limit = self.get_user_rate_limit(user_id);
        let acceptor_limit = self.get_user_rate_limit(acceptor.user_id);

        // Bridge the two streams
        let result = self
            .bridge_streams(dialer_stream, acceptor.stream, dialer_limit, acceptor_limit)
            .await;

        self.active_sessions.fetch_sub(1, Ordering::SeqCst);

        if let Err(e) = result {
            debug!("Relay bridge ended: {}", e);
        }

        Ok(())
    }

    fn get_user_rate_limit(&self, user_id: u64) -> Arc<UserRateLimit> {
        self.user_rate_limits
            .entry(user_id)
            .or_insert_with(|| Arc::new(UserRateLimit::new(self.config.user_rate_limit)))
            .clone()
    }

    async fn bridge_streams(
        &self,
        mut dialer: TcpStream,
        mut acceptor: TcpStream,
        dialer_limit: Arc<UserRateLimit>,
        acceptor_limit: Arc<UserRateLimit>,
    ) -> anyhow::Result<()> {
        let (mut dr, mut dw) = dialer.split();
        let (mut ar, mut aw) = acceptor.split();

        let global = self.global_rate_limit.clone();
        let global2 = self.global_rate_limit.clone();
        let acceptor_limit2 = acceptor_limit.clone();

        // Forward dialer -> acceptor
        let d_to_a = async move {
            let mut buf = [0u8; 8192];
            loop {
                let n = dr.read(&mut buf).await?;
                if n == 0 {
                    break;
                }

                // Rate limit check
                if !dialer_limit.try_consume(n as u64).await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if !global.try_consume(n as u64).await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }

                aw.write_all(&buf[..n]).await?;
            }
            Ok::<_, anyhow::Error>(())
        };

        // Forward acceptor -> dialer
        let a_to_d = async move {
            let mut buf = [0u8; 8192];
            loop {
                let n = ar.read(&mut buf).await?;
                if n == 0 {
                    break;
                }

                // Rate limit check
                if !acceptor_limit2.try_consume(n as u64).await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if !global2.try_consume(n as u64).await {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }

                dw.write_all(&buf[..n]).await?;
            }
            Ok::<_, anyhow::Error>(())
        };

        // Run both directions concurrently
        tokio::select! {
            r = d_to_a => r?,
            r = a_to_d => r?,
        }

        Ok(())
    }
}
