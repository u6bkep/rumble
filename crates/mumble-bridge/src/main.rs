use std::sync::{Arc, RwLock};

use anyhow::Result;
use clap::Parser;
use ed25519_dalek::SigningKey;
use prost::Message;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

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

    let config = Arc::new(BridgeConfig {
        rumble_addr: cli.rumble_addr,
        mumble_port: cli.mumble_port,
        bridge_name: cli.name,
        welcome_text: cli.welcome_text,
        ..Default::default()
    });

    info!(
        rumble_addr = %config.rumble_addr,
        mumble_port = config.mumble_port,
        bridge_name = %config.bridge_name,
        "Starting Mumble Bridge"
    );

    // Generate Ed25519 keypair for the bridge identity
    let signing_key = SigningKey::from_bytes(&rand::random());

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
        let rumble_conn = match rumble_client::connect(&config.rumble_addr, &config.bridge_name, &signing_key).await {
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
        if let Err(e) = rumble_client::send_bridge_hello(&mut rumble_conn.send, &config.bridge_name).await {
            error!(error = %e, "Failed to send BridgeHello");
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
        info!("Sent BridgeHello");

        // Update bridge state with fresh Rumble state
        {
            let mut state = write_bridge(&bridge_state);
            state.bridge_user_id = Some(rumble_conn.user_id);
            state.rumble_connected = true;
            state.rumble_rooms = rumble_conn.rooms.clone();
            state.rumble_users = rumble_conn.users.clone();

            // Pre-populate channel and user mappings
            for room in &rumble_conn.rooms {
                if let Some(uuid) = room.id.as_ref().and_then(api::uuid_from_room_id) {
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
                if let Err(e) = rumble_client::send_bridge_register_user(&mut rumble_conn.send, username).await {
                    warn!(error = %e, %username, "Failed to re-register virtual user");
                }
            }
            // Room joins and mute/deaf sync happen automatically when
            // BridgeUserRegistered responses arrive in the bridge event loop
            // (pending_join_rooms logic + MumbleMuteDeafChange events)
        }

        // Decompose the Rumble connection
        let rumble_conn_handle = rumble_conn.conn;
        let mut rumble_send = rumble_conn.send;
        let mut rumble_recv = rumble_conn.recv;
        let mut rumble_buf = rumble_conn.buf;

        // Spawn the Rumble receiver task (reliable stream + datagrams -> bridge events)
        let rumble_bridge_tx = bridge_tx.clone();
        let rumble_conn_for_dgram = rumble_conn_handle.clone();
        let recv_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = rumble_client::read_envelope(&mut rumble_recv, &mut rumble_buf) => {
                        match result {
                            Ok(Some(env)) => {
                                let _ = rumble_bridge_tx.send(BridgeEvent::RumbleEnvelope(env));
                            }
                            Ok(None) => {}
                            Err(e) => {
                                error!(error = %e, "Rumble connection error");
                                break;
                            }
                        }
                    }
                    result = rumble_conn_for_dgram.read_datagram() => {
                        match result {
                            Ok(data) => {
                                if let Ok(datagram) = api::proto::VoiceDatagram::decode(&*data) {
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
        });

        // Run the bridge event loop (blocks until shutdown or connection loss).
        // run_bridge returns the bridge_rx so we can reuse it across reconnects.
        let (result, returned_rx) = bridge::run_bridge(
            config.clone(),
            bridge_state.clone(),
            bridge_rx,
            rumble_conn_handle,
            &mut rumble_send,
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
