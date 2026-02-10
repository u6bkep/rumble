use std::sync::{Arc, RwLock};

use anyhow::Result;
use clap::Parser;
use ed25519_dalek::SigningKey;
use prost::Message;
use tokio::sync::mpsc;
use tracing::{error, info};

use mumble_bridge::{
    bridge::{self, BridgeEvent},
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

    // Connect to Rumble server
    info!("Connecting to Rumble server at {}", config.rumble_addr);
    let rumble_conn = rumble_client::connect(&config.rumble_addr, &config.bridge_name, &signing_key).await?;
    info!(user_id = rumble_conn.user_id, "Connected to Rumble server");

    // Initialize bridge state with the initial Rumble state
    let bridge_state = Arc::new(RwLock::new(BridgeState::new()));
    {
        let mut state = bridge_state.write().unwrap();
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

    // Create the bridge event channel
    let (bridge_tx, bridge_rx) = mpsc::unbounded_channel::<BridgeEvent>();

    // Set up TLS for Mumble server
    let tls_acceptor = mumble_tls::make_tls_acceptor()?;

    // Decompose the Rumble connection
    let rumble_conn_handle = rumble_conn.conn;
    let mut rumble_send = rumble_conn.send;
    let mut rumble_recv = rumble_conn.recv;
    let mut rumble_buf = rumble_conn.buf;

    // Spawn the Mumble TLS listener
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

    // Spawn the Rumble receiver task (reliable stream + datagrams -> bridge events)
    let rumble_bridge_tx = bridge_tx.clone();
    let rumble_conn_for_dgram = rumble_conn_handle.clone();
    tokio::spawn(async move {
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

    // Run the bridge event loop (blocks until shutdown)
    bridge::run_bridge(config, bridge_state, bridge_rx, rumble_conn_handle, &mut rumble_send).await?;

    Ok(())
}
