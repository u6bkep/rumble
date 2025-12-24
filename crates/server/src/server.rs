//! Server startup and connection management.
//!
//! This module provides the main `Server` struct and connection handling logic.

use crate::{
    handlers::{cleanup_client, handle_datagrams, handle_envelope},
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
use std::{net::Ipv4Addr, sync::Arc};
use tracing::{debug, error, info};

/// Configuration for the server.
#[derive(Debug)]
pub struct Config {
    /// Port to listen on.
    pub port: u16,
    /// TLS certificate (DER format).
    pub cert: rustls::pki_types::CertificateDer<'static>,
    /// TLS private key (DER format).
    pub key: rustls::pki_types::PrivateKeyDer<'static>,
}

/// The Rumble VOIP server.
///
/// This struct encapsulates the server's state and provides methods
/// for running and managing the server.
pub struct Server {
    endpoint: Endpoint,
    state: Arc<ServerState>,
}

impl Server {
    /// Create a new server with the given configuration.
    pub fn new(config: Config) -> Result<Self> {
        let endpoint = make_server_endpoint(config)?;
        let state = Arc::new(ServerState::new());
        Ok(Self { endpoint, state })
    }

    /// Get the local address the server is listening on.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Get a reference to the server state (for testing).
    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    /// Run the server, accepting connections until the endpoint is closed.
    ///
    /// This method will run forever unless the endpoint is closed externally.
    pub async fn run(&self) -> Result<()> {
        info!("server_listen_addr = {}", self.endpoint.local_addr()?);

        while let Some(connecting) = self.endpoint.accept().await {
            match connecting.await {
                Ok(new_conn) => {
                    info!("new connection from {}", new_conn.remote_address());
                    let st = self.state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(new_conn, st).await {
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
fn make_server_endpoint(config: Config) -> Result<Endpoint> {
    let mut rustls_config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(vec![config.cert], config.key)?;
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

    let addr = (Ipv4Addr::UNSPECIFIED, config.port).into();
    let endpoint = Endpoint::server(server_config, addr)?;
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
pub async fn handle_connection(conn: quinn::Connection, state: Arc<ServerState>) -> Result<()> {
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
