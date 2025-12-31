//! Rumble VOIP Server Binary
//!
//! This is a thin wrapper around the server library that sets up logging
//! and runs the server.
//!
//! # Configuration
//!
//! The server can be configured via:
//! 1. Configuration file (rumble-server.toml) - created with defaults if missing
//! 2. Command-line arguments (override config file)
//!
//! Run with `--help` for available options.

use anyhow::Result;
use server::{Config, Server, ServerConfig};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration (CLI args + config file)
    let server_config = ServerConfig::load()?;

    // Initialize logging with configured level
    let env_filter = tracing_subscriber::EnvFilter::try_new(&server_config.log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .init();

    info!("Rumble server starting...");
    info!("Bind address: {}", server_config.bind);
    info!("Data directory: {}", server_config.data_dir.display());
    info!("Cert directory: {}", server_config.cert_dir.display());
    info!("Domain: {}", server_config.domain);

    // Load certificates
    let (certs, key) = server_config.load_certificates()?;

    // Build server config
    let data_dir = server_config.data_dir().ok().map(|p| p.to_string_lossy().to_string());
    let config = Config {
        bind: server_config.bind,
        certs,
        key,
        data_dir,
    };

    let server = Server::new(config)?;
    server.run().await
}
