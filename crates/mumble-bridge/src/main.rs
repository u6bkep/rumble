use std::sync::{Arc, RwLock};

use anyhow::Result;
use clap::Parser;
use prost::Message;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use ::rumble_client_traits::transport::{Transport, TransportRecvStream};
use mumble_bridge::{
    bridge::{self, BridgeEvent, BridgeLoopState, read_bridge, write_bridge},
    config::BridgeConfig,
    mumble_server, mumble_tls, rumble_client,
    state::BridgeState,
};

#[derive(Parser, Debug)]
#[command(name = "mumble-bridge", about = "Bridge between Mumble and Rumble voice chat")]
struct Cli {
    /// Rumble server address (host:port)
    #[arg(short, long, default_value = "127.0.0.1:5000")]
    rumble_addr: String,

    /// Port to listen for Mumble clients
    #[arg(short = 'p', long, default_value_t = 64738)]
    mumble_port: u16,

    /// Bridge display name on the Rumble server
    #[arg(short, long, default_value = "MumbleBridge")]
    name: String,

    /// Welcome text shown to Mumble clients
    #[arg(short, long, default_value = "Connected to Rumble via Mumble Bridge")]
    welcome_text: String,

    /// Trust the Rumble server's TLS cert from this PEM/DER file (CA or leaf).
    #[arg(long, value_name = "PATH")]
    rumble_cert: Option<std::path::PathBuf>,

    /// Pin the Rumble server's leaf cert by SHA-256 fingerprint (hex, colons
    /// optional). Hostname-independent; correct for self-signed servers.
    #[arg(long, value_name = "HEX")]
    rumble_cert_fingerprint: Option<String>,

    /// DANGER: accept any Rumble server certificate without verification.
    /// Only for trusted networks/testing.
    #[arg(long)]
    rumble_insecure: bool,

    /// Path to the bridge's persistent Ed25519 identity file. Generated on
    /// first run (0600 on Unix); authorize it once on the server with
    /// `server add-controller <public-key>`. A stable identity is required so
    /// the controller authorization survives restarts.
    #[arg(long, value_name = "PATH", default_value = "bridge-identity.key")]
    identity_file: std::path::PathBuf,
}

/// Parse a hex SHA-256 fingerprint (colons/whitespace allowed) into 32 bytes.
fn parse_fingerprint(s: &str) -> Result<[u8; 32]> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace() && *c != ':').collect();
    let bytes = (0..cleaned.len())
        .step_by(2)
        .map(|i| {
            cleaned
                .get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
                .ok_or_else(|| anyhow::anyhow!("invalid hex in fingerprint"))
        })
        .collect::<Result<Vec<u8>>>()?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("fingerprint must be 32 bytes (64 hex chars)"))
}

/// Build the Rumble server cert trust policy from the CLI flags.
fn resolve_tls_trust(cli: &Cli) -> Result<mumble_bridge::config::RumbleTlsTrust> {
    use mumble_bridge::config::RumbleTlsTrust;
    match (&cli.rumble_cert, &cli.rumble_cert_fingerprint, cli.rumble_insecure) {
        (Some(_), _, true) | (_, Some(_), true) | (Some(_), Some(_), _) => {
            anyhow::bail!("--rumble-cert, --rumble-cert-fingerprint and --rumble-insecure are mutually exclusive")
        }
        (Some(path), None, false) => Ok(RumbleTlsTrust::CertFile(path.clone())),
        (None, Some(fp), false) => Ok(RumbleTlsTrust::Fingerprint(parse_fingerprint(fp)?)),
        (None, None, true) => Ok(RumbleTlsTrust::Insecure),
        (None, None, false) => Ok(RumbleTlsTrust::WebPki),
    }
}

/// Translate the trust policy into a transport `TlsConfig`.
fn build_tls_config(
    trust: &mumble_bridge::config::RumbleTlsTrust,
) -> Result<rumble_client_traits::transport::TlsConfig> {
    use mumble_bridge::config::RumbleTlsTrust;
    use rumble_client_traits::transport::TlsConfig;
    Ok(match trust {
        RumbleTlsTrust::WebPki => TlsConfig::default(),
        RumbleTlsTrust::CertFile(path) => {
            let bytes = std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("failed to read --rumble-cert {}: {e}", path.display()))?;
            TlsConfig {
                additional_ca_certs: vec![bytes],
                ..Default::default()
            }
        }
        RumbleTlsTrust::Fingerprint(fp) => TlsConfig {
            accepted_fingerprints: vec![*fp],
            ..Default::default()
        },
        RumbleTlsTrust::Insecure => {
            warn!("--rumble-insecure: skipping Rumble server certificate verification");
            TlsConfig {
                accept_invalid_certs: true,
                ..Default::default()
            }
        }
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let rumble_tls = resolve_tls_trust(&cli)?;

    let config = Arc::new(BridgeConfig {
        rumble_addr: cli.rumble_addr,
        mumble_port: cli.mumble_port,
        bridge_name: cli.name,
        welcome_text: cli.welcome_text,
        rumble_tls,
        ..Default::default()
    });

    info!(
        rumble_addr = %config.rumble_addr,
        mumble_port = config.mumble_port,
        bridge_name = %config.bridge_name,
        "Starting Mumble Bridge"
    );

    // Load (or first-run generate) the bridge's persistent Ed25519 identity so
    // the server's controller authorization survives restarts.
    let signing_key = mumble_bridge::identity::load_or_create_identity(&cli.identity_file)?;
    let controller_key_b64 = mumble_bridge::identity::public_key_b64(&signing_key);
    info!(
        identity_file = %cli.identity_file.display(),
        public_key = %controller_key_b64,
        "Bridge identity ready — authorize once with `server add-controller <public_key>`"
    );

    // Shutdown signal: ctrl_c or SIGTERM
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
            tokio::select! {
                _ = ctrl_c => { info!("Received Ctrl+C"); }
                _ = sigterm.recv() => { info!("Received SIGTERM"); }
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.ok();
            info!("Received Ctrl+C");
        }
        let _ = shutdown_tx.send(true);
    });

    // Set up TLS for Mumble server (shared across reconnects)
    let tls_acceptor = mumble_tls::make_tls_acceptor()?;

    // Bridge state persists across reconnects so Mumble clients stay connected
    let bridge_state = Arc::new(RwLock::new(BridgeState::new()));

    // Create the bridge event channel (shared across reconnects)
    let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel::<BridgeEvent>();

    // Spawn the Mumble TLS listener (runs independently of Rumble connection)
    let mumble_state = bridge_state.clone();
    let mumble_config = config.clone();
    let mumble_bridge_tx = bridge_tx.clone();
    tokio::spawn(async move {
        if let Err(e) =
            mumble_server::run_mumble_server(mumble_config, tls_acceptor, mumble_state, mumble_bridge_tx).await
        {
            error!(error = %e, "Mumble server error");
        }
    });

    // Persistent bridge loop state (survives reconnects)
    let mut loop_state = BridgeLoopState::new();

    // Reconnection loop with exponential backoff
    let mut backoff_secs = 1u64;
    const MAX_BACKOFF_SECS: u64 = 30;

    loop {
        // Check shutdown before attempting connection
        if *shutdown_rx.borrow() {
            info!("Shutdown requested, not reconnecting");
            break;
        }

        info!("Connecting to Rumble server at {}", config.rumble_addr);
        let tls_config = build_tls_config(&config.rumble_tls)?;
        let rumble_conn =
            match rumble_client::connect(&config.rumble_addr, &config.bridge_name, &signing_key, tls_config).await {
                Ok(conn) => {
                    backoff_secs = 1; // Reset backoff on successful connection
                    conn
                }
                Err(e) => {
                    error!(error = %e, "Failed to connect to Rumble server");
                    info!(backoff_secs, "Reconnecting after backoff");
                    let mut shutdown_wait = shutdown_rx.clone();
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                        _ = shutdown_wait.changed() => {
                            info!("Shutdown during reconnect backoff");
                            break;
                        }
                    }
                    backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                    continue;
                }
            };

        let mut rumble_conn = rumble_conn;
        info!(user_id = rumble_conn.user_id, "Connected to Rumble server");

        // Declare this connection as a bridge
        if let Err(e) = rumble_client::send_controller_hello(&mut rumble_conn.transport, &config.bridge_name).await {
            error!(error = %e, "Failed to send ControllerHello");
            info!(backoff_secs, "Reconnecting after backoff");
            let mut shutdown_wait = shutdown_rx.clone();
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                _ = shutdown_wait.changed() => {
                    info!("Shutdown during reconnect backoff");
                    break;
                }
            }
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
            continue;
        }
        info!("Sent ControllerHello");

        // Update bridge state with fresh Rumble state
        {
            let mut state = write_bridge(&bridge_state);
            state.bridge_user_id = Some(rumble_conn.user_id);
            state.rumble_connected = true;
            state.rumble_rooms = rumble_conn.rooms.clone();
            state.rumble_users = rumble_conn.users.clone();

            // Pre-populate channel and user mappings
            for room in &rumble_conn.rooms {
                if let Some(uuid) = room.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) {
                    state.channels.get_or_insert(uuid);
                }
            }
            for user in &rumble_conn.users {
                let rumble_id = user.user_id.as_ref().map(|id| id.value).unwrap_or(0);
                if rumble_id != rumble_conn.user_id {
                    state.users.get_or_insert(rumble_id);
                }
            }
        }

        // Re-register all currently connected Mumble clients as virtual users
        let clients_to_reregister: Vec<(u32, String)> = {
            let state = read_bridge(&bridge_state);
            state
                .mumble_clients
                .values()
                .map(|c| (c.session, c.username.clone()))
                .collect()
        };

        if !clients_to_reregister.is_empty() {
            info!(
                count = clients_to_reregister.len(),
                "Re-registering Mumble clients after reconnect"
            );
            // Clear stale virtual user mappings from previous connection
            {
                let mut state = write_bridge(&bridge_state);
                state.virtual_user_map.clear();
                state.reverse_virtual_user_map.clear();
                state.pending_registrations.clear();
            }

            for (session, username) in &clients_to_reregister {
                {
                    let mut state = write_bridge(&bridge_state);
                    state.pending_registrations.push((username.clone(), *session));
                }
                if let Err(e) =
                    rumble_client::send_register_participant(&mut rumble_conn.transport, username, Some("Mumble")).await
                {
                    warn!(error = %e, %username, "Failed to re-register participant");
                }
            }
            // Room joins and mute/deaf sync happen automatically when
            // ParticipantRegistered responses arrive in the bridge event loop
            // (pending_join_rooms logic + MumbleMuteDeafChange events)
        }

        // Split the transport: recv stream for receiver task, transport for sending,
        // raw quinn::Connection for datagrams + closed detection.
        let mut rumble_transport = rumble_conn.transport;
        let rumble_conn_handle = rumble_transport.connection().clone();
        let mut rumble_recv = rumble_transport.take_recv();

        // Spawn the Rumble receiver task (reliable stream + datagrams -> bridge events)
        let rumble_bridge_tx = bridge_tx.clone();
        let rumble_conn_for_dgram = rumble_conn_handle.clone();
        let recv_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = rumble_recv.recv() => {
                        match result {
                            Ok(Some(frame)) => {
                                if let Ok(env) = prost::Message::decode(&*frame) {
                                    let _ = rumble_bridge_tx.send(BridgeEvent::RumbleEnvelope(env));
                                }
                            }
                            Ok(None) => {
                                error!("Rumble connection closed");
                                break;
                            }
                            Err(e) => {
                                error!(error = %e, "Rumble connection error");
                                break;
                            }
                        }
                    }
                    result = rumble_conn_for_dgram.read_datagram() => {
                        match result {
                            Ok(data) => {
                                if let Ok(datagram) = rumble_protocol::proto::VoiceDatagram::decode(&*data) {
                                    let _ = rumble_bridge_tx.send(BridgeEvent::RumbleVoice(datagram));
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "Rumble datagram error");
                                break;
                            }
                        }
                    }
                }
            }
            // Signal the bridge event loop that inbound Rumble data has stopped.
            // The bridge will break its loop and trigger the reconnect path.
            let _ = rumble_bridge_tx.send(BridgeEvent::RumbleReceiverDied);
        });

        // Run the bridge event loop (blocks until shutdown or connection loss).
        // run_bridge returns the bridge_rx so we can reuse it across reconnects.
        let (result, returned_rx) = bridge::run_bridge(
            config.clone(),
            bridge_state.clone(),
            bridge_rx,
            rumble_conn_handle,
            &mut rumble_transport,
            shutdown_rx.clone(),
            &mut loop_state,
        )
        .await;
        bridge_rx = returned_rx;

        recv_handle.abort();

        // Mark Rumble as disconnected
        {
            let mut state = write_bridge(&bridge_state);
            state.rumble_connected = false;
        }

        if *shutdown_rx.borrow() {
            info!("Shutting down");
            break;
        }

        match result {
            Ok(()) => {
                warn!("Bridge event loop ended unexpectedly, reconnecting");
            }
            Err(e) => {
                error!(error = %e, "Bridge error, reconnecting");
            }
        }

        info!(backoff_secs, "Reconnecting after backoff");
        let mut shutdown_wait = shutdown_rx.clone();
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
            _ = shutdown_wait.changed() => {
                info!("Shutdown during reconnect backoff");
                break;
            }
        }
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    Ok(())
}
