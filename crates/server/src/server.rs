//! Server startup and connection management.
//!
//! This module provides the main `Server` struct and connection handling logic.

use crate::{
    handlers::{cleanup_client, handle_datagrams, handle_envelope},
    persistence::Persistence,
    state::{ClientHandle, ServerState},
};
use anyhow::Result;
use api::{
    proto::{self, Envelope},
    try_decode_frame,
};
use bytes::BytesMut;
use prost::Message;
use quinn::{Endpoint, ServerConfig};
use std::{net::SocketAddr, sync::Arc};
use tracing::{debug, error, info};

/// Configuration for the server.
#[derive(Debug)]
pub struct Config {
    /// Socket address to bind to (IPv4 or IPv6 with port).
    pub bind: SocketAddr,
    /// TLS certificate chain (PEM format, certbot-style).
    pub certs: Vec<rustls::pki_types::CertificateDer<'static>>,
    /// TLS private key (PEM format).
    pub key: rustls::pki_types::PrivateKeyDer<'static>,
    /// Optional path for the persistence database.
    pub data_dir: Option<String>,
}

/// The Rumble VOIP server.
///
/// This struct encapsulates the server's state and provides methods
/// for running and managing the server.
pub struct Server {
    endpoint: Endpoint,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
}

impl Server {
    /// Create a new server with the given configuration.
    pub fn new(config: Config) -> Result<Self> {
        // Store first cert DER for hash computation (leaf certificate)
        let cert_der = config.certs.first()
            .map(|c| c.to_vec())
            .unwrap_or_default();
        
        let endpoint = make_server_endpoint(&config)?;
        let state = Arc::new(ServerState::with_cert(cert_der));
        
        // Initialize persistence if data_dir is provided
        let persistence = if let Some(ref data_dir) = config.data_dir {
            let db_path = format!("{}/rumble.db", data_dir);
            match Persistence::open(&db_path) {
                Ok(p) => {
                    info!("Opened persistence database at {}", db_path);
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
        
        Ok(Self { endpoint, state, persistence })
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
                if uuid == api::ROOT_ROOM_UUID {
                    continue;
                }
                // Convert parent bytes to UUID if present
                let parent = room.parent.map(uuid::Uuid::from_bytes);
                if self.state.add_room_with_uuid_and_parent(uuid, room.name.clone(), parent).await {
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
        
        info!("server_listen_addr = {}", self.endpoint.local_addr()?);

        while let Some(connecting) = self.endpoint.accept().await {
            match connecting.await {
                Ok(new_conn) => {
                    info!("new connection from {}", new_conn.remote_address());
                    let st = self.state.clone();
                    let persist = self.persistence.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(new_conn, st, persist).await {
                            error!("connection error: {e:?}");
                        }
                    });
                }
                Err(e) => {
                    error!("incoming connection failed: {e:?}");
                }
            }
        }

        self.endpoint.wait_idle().await;
        Ok(())
    }

    /// Close the server endpoint, causing `run()` to return.
    pub fn close(&self) {
        self.endpoint
            .close(quinn::VarInt::from_u32(0), b"server shutdown");
    }
}

/// Create a QUIC server endpoint with the given configuration.
fn make_server_endpoint(config: &Config) -> Result<Endpoint> {
    let mut rustls_config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::aws_lc_rs::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(config.certs.clone(), config.key.clone_key())?;
    rustls_config.alpn_protocols = vec![b"rumble".to_vec()];

    let mut server_config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(rustls_config))?,
    ));

    // Configure transport for faster disconnect detection
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into().unwrap()));
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport_config.datagram_receive_buffer_size(Some(65536));
    server_config.transport_config(Arc::new(transport_config));

    let endpoint = Endpoint::server(server_config, config.bind)?;
    Ok(endpoint)
}

/// Handle a single client connection.
///
/// This manages the connection lifecycle:
/// 1. Assign a user ID (lock-free)
/// 2. Spawn datagram handler
/// 3. Accept bidirectional streams
/// 4. Process messages on each stream
/// 5. Clean up on disconnect
pub async fn handle_connection(
    conn: quinn::Connection,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
) -> Result<()> {
    // Assign user_id at connection level - this is the authoritative identity.
    // This is lock-free (AtomicU64).
    let user_id = state.allocate_user_id();
    info!(
        user_id,
        remote = %conn.remote_address(),
        "assigned user_id for connection"
    );

    // Track all client handles for this connection so we can clean up when connection closes.
    let mut connection_clients: Vec<Arc<ClientHandle>> = Vec::new();

    // Spawn a task to handle incoming datagrams for voice relay.
    let conn_for_datagrams = conn.clone();
    let state_for_datagrams = state.clone();
    tokio::spawn(async move {
        handle_datagrams(conn_for_datagrams, state_for_datagrams, user_id).await;
    });

    loop {
        match conn.accept_bi().await {
            Ok((send_stream, mut recv)) => {
                info!("new bi stream opened");

                // Create client handle using the new constructor
                let client_handle = Arc::new(ClientHandle::new(send_stream, user_id, conn.clone()));

                // Register client (lock-free DashMap insert)
                state.register_client(client_handle.clone());
                let client_count = state.client_count();
                debug!(total_clients = client_count, "server: client registered");

                // Track this client for connection-level cleanup.
                connection_clients.push(client_handle.clone());

                let mut buf = BytesMut::new();
                let persist = persistence.clone();

                // Read loop - handle errors gracefully
                loop {
                    let mut chunk = [0u8; 1024];
                    let read_result = tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        recv.read(&mut chunk),
                    )
                    .await;

                    match read_result {
                        Ok(Ok(Some(n))) => {
                            debug!(bytes = n, "server: received bytes on stream");
                            buf.extend_from_slice(&chunk[..n]);
                            while let Some(frame) = try_decode_frame(&mut buf) {
                                match Envelope::decode(&*frame) {
                                    Ok(env) => {
                                        debug!(
                                            frame_len = frame.len(),
                                            "server: decoded envelope frame"
                                        );
                                        if let Err(e) = handle_envelope(
                                            env,
                                            client_handle.clone(),
                                            state.clone(),
                                            persist.clone(),
                                        )
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
                            info!("stream closed by peer");
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
                            let frame = api::encode_frame(&env);
                            // Use the send_frame helper method
                            if client_handle.send_frame(&frame).await.is_err() {
                                info!("connection dead after read timeout");
                                break;
                            }
                        }
                    }
                }

                // Stream closed - clean up this client
                cleanup_client(&client_handle, &state).await;

                // Remove from connection tracking
                connection_clients.retain(|h| !Arc::ptr_eq(h, &client_handle));
            }
            Err(e) => {
                info!("connection closed: {e:?}");
                break;
            }
        }
    }

    // Connection-level cleanup: clean up any remaining clients from this connection.
    for client_handle in connection_clients {
        cleanup_client(&client_handle, &state).await;
    }

    Ok(())
}
