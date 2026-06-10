//! Server startup and connection management.
//!
//! This module provides the main `Server` struct and connection handling logic.

use crate::{
    handlers::{cleanup_client, handle_datagrams, handle_envelope},
    persistence::Persistence,
    plugin::{ServerCtx, ServerPlugin, StreamHeader},
    state::{ClientHandle, Identity, ServerState},
};
use anyhow::Result;
use bytes::BytesMut;
use prost::Message;
use quinn::{Endpoint, ServerConfig};
use rumble_protocol::{
    proto::{self, Envelope},
    try_decode_frame,
};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

/// Configuration for the server.
pub struct Config {
    /// Socket address to bind to (IPv4 or IPv6 with port).
    pub bind: SocketAddr,
    /// TLS certificate chain (PEM format, certbot-style).
    pub certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    /// TLS private key (PEM format).
    pub key: rustls::pki_types::PrivateKeyDer<'static>,
    /// Directory holding `fullchain.pem` / `privkey.pem`. Retained so a live
    /// reload (SIGHUP) can re-read the certs after a certbot renewal.
    pub cert_dir: PathBuf,
    /// Optional path for the persistence database.
    pub data_dir: Option<String>,
    /// Welcome message (MOTD) sent to clients after authentication.
    pub welcome_message: Option<String>,
    /// Server plugins (compile-time extensions).
    pub plugins: Vec<Box<dyn ServerPlugin>>,
    /// Web admin control-plane settings, when enabled.
    pub web: Option<crate::config::WebSettings>,
}

/// The Rumble VOIP server.
///
/// This struct encapsulates the server's state and provides methods
/// for running and managing the server.
pub struct Server {
    endpoint: Endpoint,
    cert_dir: PathBuf,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
    plugins: Vec<Arc<dyn ServerPlugin>>,
    plugin_ctx: Arc<ServerCtx>,
    web: Option<crate::config::WebSettings>,
}

impl Server {
    /// Create a new server with the given configuration.
    pub fn new(config: Config) -> Result<Self> {
        // Store first cert DER for hash computation (leaf certificate)
        let cert_der = config.certs.first().map(|c| c.to_vec()).unwrap_or_default();
        let cert_dir = config.cert_dir.clone();

        let endpoint = make_server_endpoint(&config)?;
        let state = Arc::new(ServerState::with_cert_and_welcome(cert_der, config.welcome_message));

        // Initialize persistence if data_dir is provided
        let persistence = if let Some(ref data_dir) = config.data_dir {
            let db_path = format!("{}/rumble.db", data_dir);
            match Persistence::open(&db_path) {
                Ok(p) => {
                    info!("Opened persistence database at {}", db_path);
                    // Ensure default permission groups exist
                    if let Err(e) = p.ensure_default_groups() {
                        error!("Failed to create default groups: {e}");
                    }
                    Some(Arc::new(p))
                }
                Err(e) => {
                    error!("Failed to open persistence database: {e}");
                    None
                }
            }
        } else {
            None
        };

        // Create plugin context and wrap plugins in Arc
        let plugin_ctx = Arc::new(ServerCtx::new(state.clone(), persistence.clone()));
        let plugins: Vec<Arc<dyn ServerPlugin>> = config.plugins.into_iter().map(Arc::from).collect();

        // Aggregate the plugins' slash commands once; reported to clients in the
        // full ServerState snapshot for autocomplete.
        let slash_commands: Vec<proto::SlashCommand> = plugins
            .iter()
            .flat_map(|p| p.commands())
            .map(|c| proto::SlashCommand {
                name: c.name,
                description: c.description,
            })
            .collect();
        state.set_slash_commands(slash_commands);

        Ok(Self {
            endpoint,
            cert_dir,
            state,
            persistence,
            plugins,
            plugin_ctx,
            web: config.web,
        })
    }

    /// Get the local address the server is listening on.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Get a reference to the server state (for testing).
    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    /// Load persisted rooms into the server state.
    async fn load_persisted_rooms(&self) {
        if let Some(ref persist) = self.persistence {
            let rooms = persist.get_all_rooms();
            let count = rooms.len();
            for (uuid_bytes, room) in rooms {
                let uuid = uuid::Uuid::from_bytes(uuid_bytes);
                // Skip the root room UUID (all zeros)
                if uuid == rumble_protocol::ROOT_ROOM_UUID {
                    continue;
                }
                // Convert parent bytes to UUID if present
                let parent = room.parent.map(uuid::Uuid::from_bytes);
                let description = if room.description.is_empty() {
                    None
                } else {
                    Some(room.description.clone())
                };
                if self
                    .state
                    .add_room_with_uuid_and_parent_desc(uuid, room.name.clone(), parent, description)
                    .await
                {
                    debug!("Loaded persisted room: {} ({})", room.name, uuid);
                }
            }
            if count > 0 {
                info!("Loaded {} persisted room(s)", count);
            }
        }
    }

    /// Run the server, accepting connections until the endpoint is closed.
    ///
    /// This method will run forever unless the endpoint is closed externally.
    pub async fn run(&self) -> Result<()> {
        // Load persisted rooms before accepting connections
        self.load_persisted_rooms().await;

        // Spawn background task to sweep expired timed bans every 60 seconds.
        if let Some(persist) = self.persistence.clone() {
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    let now_secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let swept = persist.sweep_expired_bans(now_secs);
                    if swept > 0 {
                        info!(count = swept, "swept expired bans");
                    }
                }
            });
        }

        // On Unix, reload the TLS certificate on SIGHUP so a certbot renewal is
        // applied without dropping live connections. A certbot deploy-hook runs
        // `docker compose kill -s HUP rumble-server` (tini forwards the signal).
        #[cfg(unix)]
        {
            let endpoint = self.endpoint.clone();
            let state = self.state.clone();
            let cert_dir = self.cert_dir.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                let mut hup = match signal(SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("failed to install SIGHUP handler (cert reload disabled): {e}");
                        return;
                    }
                };
                while hup.recv().await.is_some() {
                    info!(cert_dir = %cert_dir.display(), "SIGHUP received — reloading TLS certificate");
                    match reload_certificate(&cert_dir, &endpoint, &state) {
                        Ok(()) => info!("TLS certificate reloaded; new connections use the renewed cert"),
                        Err(e) => error!("TLS certificate reload failed (keeping current cert): {e}"),
                    }
                }
            });
        }

        // Start the web admin control-plane, if enabled.
        if let Some(web_settings) = self.web.clone() {
            crate::web::spawn(self.state.clone(), self.persistence.clone(), web_settings);
        } else {
            info!("web admin disabled (set [web] enabled = true, or RUMBLE_WEB_ENABLED=1)");
        }

        // Start plugins before accepting connections
        for plugin in &self.plugins {
            info!(plugin = plugin.name(), "starting plugin");
            plugin.start(&self.plugin_ctx).await?;
        }

        info!("server_listen_addr = {}", self.endpoint.local_addr()?);

        while let Some(connecting) = self.endpoint.accept().await {
            match connecting.await {
                Ok(new_conn) => {
                    info!("new connection from {}", new_conn.remote_address());
                    let st = self.state.clone();
                    let persist = self.persistence.clone();
                    let plugins = self.plugins.clone();
                    let ctx = self.plugin_ctx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(new_conn, st, persist, plugins, ctx).await {
                            error!("connection error: {e:?}");
                        }
                    });
                }
                Err(e) => {
                    error!("incoming connection failed: {e:?}");
                }
            }
        }

        // Stop plugins on shutdown
        for plugin in &self.plugins {
            info!(plugin = plugin.name(), "stopping plugin");
            if let Err(e) = plugin.stop().await {
                error!(plugin = plugin.name(), "plugin stop error: {e:?}");
            }
        }

        self.endpoint.wait_idle().await;
        Ok(())
    }

    /// Close the server endpoint, causing `run()` to return.
    pub fn close(&self) {
        self.endpoint.close(quinn::VarInt::from_u32(0), b"server shutdown");
    }
}

/// Build a quinn [`ServerConfig`] from a cert chain + key.
///
/// Shared by initial startup and live cert reload so both paths produce an
/// identical TLS + transport configuration.
fn build_quic_server_config(
    certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
) -> Result<ServerConfig> {
    let mut rustls_config =
        rustls::ServerConfig::builder_with_provider(rustls::crypto::aws_lc_rs::default_provider().into())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
    rustls_config.alpn_protocols = vec![b"rumble".to_vec()];

    let mut server_config = ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(
        Arc::new(rustls_config),
    )?));

    // Configure transport for faster disconnect detection
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.max_idle_timeout(Some(
        std::time::Duration::from_secs(30)
            .try_into()
            .expect("30s is valid for quinn idle timeout"),
    ));
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport_config.datagram_receive_buffer_size(Some(65536));
    server_config.transport_config(Arc::new(transport_config));

    Ok(server_config)
}

/// Re-read `fullchain.pem` / `privkey.pem` from `cert_dir` and apply them to the
/// live endpoint and the auth cert-hash.
///
/// Unlike startup, this never falls back to a self-signed cert: if the files are
/// missing or invalid it returns an error and the caller keeps the current cert.
/// The cert-hash is updated before the endpoint so a connection that handshakes
/// against the renewed cert finds the matching hash at auth time; a connection
/// mid-handshake across the swap may need one reconnect (rare, monthly).
#[cfg(unix)]
fn reload_certificate(cert_dir: &std::path::Path, endpoint: &Endpoint, state: &Arc<ServerState>) -> Result<()> {
    let fullchain = cert_dir.join("fullchain.pem");
    let privkey = cert_dir.join("privkey.pem");
    let (certs, key) = crate::config::load_pem_certificates(&fullchain, &privkey)?;
    let leaf = certs.first().map(|c| c.to_vec()).unwrap_or_default();
    let server_config = build_quic_server_config(certs, key)?;
    state.set_server_cert_der(leaf);
    endpoint.set_server_config(Some(server_config));
    Ok(())
}

/// Create a QUIC server endpoint with the given configuration.
fn make_server_endpoint(config: &Config) -> Result<Endpoint> {
    let server_config = build_quic_server_config(config.certs.clone(), config.key.clone_key())?;
    let endpoint = Endpoint::server(server_config, config.bind)?;
    Ok(endpoint)
}

/// Handle a single client connection.
///
/// This manages the connection lifecycle:
/// 1. Assign a user ID (lock-free)
/// 2. Spawn datagram handler
/// 3. Accept bidirectional streams
/// 4. Process messages on each stream (first stream = control, additional = plugin streams)
/// 5. Notify plugins on disconnect
/// 6. Clean up on disconnect
pub async fn handle_connection(
    conn: quinn::Connection,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
    plugins: Vec<Arc<dyn ServerPlugin>>,
    plugin_ctx: Arc<ServerCtx>,
) -> Result<()> {
    // Assign user_id at connection level - this is the authoritative identity.
    // This is lock-free (AtomicU64).
    let user_id = state.allocate_user_id();
    info!(
        user_id,
        remote = %conn.remote_address(),
        "assigned user_id for connection"
    );

    // Spawn a task to handle incoming datagrams for voice relay.
    let conn_for_datagrams = conn.clone();
    let state_for_datagrams = state.clone();
    tokio::spawn(async move {
        handle_datagrams(conn_for_datagrams, state_for_datagrams, user_id).await;
    });

    // Shared state for all streams on this connection. The identity is the
    // single home for name/groups/label; every stream's handle and the member
    // entry share this same Arc.
    let identity = Arc::new(Identity::empty());
    let public_key = Arc::new(RwLock::new(None));
    let authenticated = Arc::new(AtomicBool::new(false));

    // Track whether we've registered the client (only register once, on first stream)
    let mut client_handle: Option<Arc<ClientHandle>> = None;
    let mut is_first_stream = true;

    loop {
        match conn.accept_bi().await {
            Ok((send_stream, recv)) => {
                info!("new bi stream opened (first={})", is_first_stream);

                if is_first_stream {
                    // First stream - create and register the client (control stream)
                    let handle = Arc::new(ClientHandle::new(
                        send_stream,
                        user_id,
                        conn.clone(),
                        public_key.clone(),
                        authenticated.clone(),
                        identity.clone(),
                    ));

                    // Register client (lock-free DashMap insert)
                    state.register_client(handle.clone());
                    let client_count = state.client_count();
                    debug!(total_clients = client_count, "server: client registered");

                    client_handle = Some(handle.clone());
                    is_first_stream = false;

                    let persistence = persistence.clone();
                    let state_clone = state.clone();
                    let plugins_clone = plugins.clone();
                    let ctx_clone = plugin_ctx.clone();

                    tokio::spawn(async move {
                        run_envelope_stream(recv, handle, state_clone, persistence, plugins_clone, ctx_clone, true)
                            .await;
                    });
                } else if let Some(ref primary_handle) = client_handle {
                    // Additional stream - probe for plugin stream header
                    let primary_handle = primary_handle.clone();
                    let plugins_clone = plugins.clone();
                    let ctx_clone = plugin_ctx.clone();
                    let persistence = persistence.clone();
                    let state_clone = state.clone();
                    let conn_clone = conn.clone();
                    let identity_clone = identity.clone();
                    let public_key_clone = public_key.clone();
                    let authenticated_clone = authenticated.clone();

                    tokio::spawn(async move {
                        dispatch_secondary_stream(
                            send_stream,
                            recv,
                            user_id,
                            conn_clone,
                            identity_clone,
                            public_key_clone,
                            authenticated_clone,
                            primary_handle,
                            state_clone,
                            persistence,
                            plugins_clone,
                            ctx_clone,
                        )
                        .await;
                    });
                } else {
                    // Non-primary stream but no client_handle yet (shouldn't happen)
                    error!("received non-primary stream before client was registered");
                }
            }
            Err(e) => {
                info!("connection closed: {e:?}");
                break;
            }
        }
    }

    // Connection closed - ensure cleanup happens if primary stream is still active
    if let Some(handle) = client_handle {
        for plugin in &plugins {
            plugin.on_disconnect(&handle, &plugin_ctx).await;
        }
        cleanup_client(&handle, &state).await;
    }

    Ok(())
}

/// Run the envelope read loop on a stream (used for control streams and
/// secondary streams that are not plugin-owned).
async fn run_envelope_stream(
    recv: quinn::RecvStream,
    handle: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
    plugins: Vec<Arc<dyn ServerPlugin>>,
    ctx: Arc<ServerCtx>,
    is_primary: bool,
) {
    run_envelope_stream_with_prefix(
        recv,
        handle,
        state,
        persistence,
        plugins,
        ctx,
        is_primary,
        BytesMut::new(),
    )
    .await;
}

/// Like [`run_envelope_stream`] but seeds the read buffer with already-consumed
/// bytes (used when plugin header probing consumed bytes that turned out to be
/// envelope data).
#[allow(clippy::too_many_arguments)]
async fn run_envelope_stream_with_prefix(
    mut recv: quinn::RecvStream,
    handle: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
    plugins: Vec<Arc<dyn ServerPlugin>>,
    ctx: Arc<ServerCtx>,
    is_primary: bool,
    initial_buf: BytesMut,
) {
    let mut buf = initial_buf;

    // Process any frames already present in the seed buffer
    while let Some(frame) = try_decode_frame(&mut buf) {
        match Envelope::decode(&*frame) {
            Ok(env) => {
                info!(frame_len = frame.len(), "server: decoded envelope frame");
                if let Err(e) =
                    handle_envelope(env, handle.clone(), state.clone(), persistence.clone(), &plugins, &ctx).await
                {
                    error!("handle_envelope error: {e:?}");
                }
            }
            Err(e) => error!("failed to decode envelope: {e:?}"),
        }
    }

    // Read loop
    loop {
        let mut chunk = [0u8; 1024];
        let read_result = tokio::time::timeout(std::time::Duration::from_secs(30), recv.read(&mut chunk)).await;

        match read_result {
            Ok(Ok(Some(n))) => {
                info!(bytes = n, "server: received bytes on stream");
                buf.extend_from_slice(&chunk[..n]);
                // Bound memory: refuse to keep buffering for an oversized frame.
                // A complete frame is always decoded once enough bytes arrive, so
                // exceeding the cap means a peer declared a huge (or never-
                // completing) length — drop the connection rather than OOM.
                if buf.len() > rumble_protocol::MAX_FRAME_LEN {
                    error!(
                        "control frame exceeds MAX_FRAME_LEN ({} bytes buffered); closing stream",
                        buf.len()
                    );
                    break;
                }
                while let Some(frame) = try_decode_frame(&mut buf) {
                    match Envelope::decode(&*frame) {
                        Ok(env) => {
                            info!(frame_len = frame.len(), "server: decoded envelope frame");
                            if let Err(e) =
                                handle_envelope(env, handle.clone(), state.clone(), persistence.clone(), &plugins, &ctx)
                                    .await
                            {
                                error!("handle_envelope error: {e:?}");
                            }
                        }
                        Err(e) => error!("failed to decode envelope: {e:?}"),
                    }
                }
            }
            Ok(Ok(None)) => {
                info!("stream closed by peer (primary={})", is_primary);
                break;
            }
            Ok(Err(e)) => {
                info!("stream read error (likely disconnect): {e:?}");
                break;
            }
            Err(_) => {
                // Read timeout - check if connection is still alive
                info!("read timeout, checking connection health");
                let env = proto::Envelope {
                    state_hash: Vec::new(),
                    payload: None,
                };
                let frame = rumble_protocol::encode_frame(&env);
                if handle.send_frame(&frame).await.is_err() {
                    info!("connection dead after read timeout");
                    break;
                }
            }
        }
    }

    // Only cleanup if this was the primary stream
    if is_primary {
        for plugin in &plugins {
            plugin.on_disconnect(&handle, &ctx).await;
        }
        cleanup_client(&handle, &state).await;
    }
}

/// Maximum plugin name length we consider valid when probing stream headers.
const MAX_PLUGIN_NAME_LEN: u16 = 255;

/// Dispatch a secondary (non-primary) stream. Probes the first bytes to
/// determine if it carries a [`StreamHeader`] addressed to a registered plugin.
/// If so, the stream is handed off to that plugin. Otherwise it falls back to
/// the normal envelope processing loop.
#[allow(clippy::too_many_arguments)]
async fn dispatch_secondary_stream(
    send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    user_id: u64,
    conn: quinn::Connection,
    identity: Arc<Identity>,
    public_key: Arc<RwLock<Option<[u8; 32]>>>,
    authenticated: Arc<AtomicBool>,
    primary_handle: Arc<ClientHandle>,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
    plugins: Vec<Arc<dyn ServerPlugin>>,
    ctx: Arc<ServerCtx>,
) {
    // --- Step 1: Read the 2-byte name-length prefix ---
    let mut len_buf = [0u8; 2];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(e) => {
            info!("secondary stream closed before header could be read: {e:?}");
            return;
        }
    }
    let name_len = u16::from_be_bytes(len_buf) as usize;

    // --- Step 2: Sanity-check the length ---
    if name_len > 0 && name_len <= MAX_PLUGIN_NAME_LEN as usize {
        // Read the candidate plugin name
        let mut name_buf = vec![0u8; name_len];
        match recv.read_exact(&mut name_buf).await {
            Ok(()) => {}
            Err(e) => {
                info!("secondary stream closed while reading plugin name: {e:?}");
                return;
            }
        }

        // Check for valid UTF-8 and matching plugin
        if let Ok(plugin_name) = std::str::from_utf8(&name_buf)
            && let Some(plugin) = plugins.iter().find(|p| p.name() == plugin_name)
        {
            // It's a plugin stream -- build header and dispatch
            info!(plugin = plugin_name, "routing secondary stream to plugin");
            let header = StreamHeader {
                plugin: plugin_name.to_owned(),
                metadata: Vec::new(), // plugin reads its own metadata from recv
            };
            if let Err(e) = plugin.on_stream(header, send, recv, &primary_handle, &ctx).await {
                error!(plugin = plugin_name, "plugin on_stream error: {e:?}");
            }
            return;
        }

        // Not a plugin stream -- fall back to envelope processing.
        // Re-assemble the bytes we already consumed into the read buffer.
        let mut seed = BytesMut::with_capacity(2 + name_len);
        seed.extend_from_slice(&len_buf);
        seed.extend_from_slice(&name_buf);

        let handle = Arc::new(ClientHandle::new(
            send,
            user_id,
            conn,
            public_key,
            authenticated,
            identity,
        ));
        run_envelope_stream_with_prefix(recv, handle, state, persistence, plugins, ctx, false, seed).await;
    } else {
        // name_len was 0 or too large -- definitely not a plugin header.
        let mut seed = BytesMut::with_capacity(2);
        seed.extend_from_slice(&len_buf);

        let handle = Arc::new(ClientHandle::new(
            send,
            user_id,
            conn,
            public_key,
            authenticated,
            identity,
        ));
        run_envelope_stream_with_prefix(recv, handle, state, persistence, plugins, ctx, false, seed).await;
    }
}
