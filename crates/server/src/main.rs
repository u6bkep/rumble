//! Rumble VOIP Server Binary
//!
//! This is a thin wrapper around the server library that sets up logging
//! and runs the server.

use anyhow::Result;
use server::{load_or_create_dev_cert, Config, Server};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("RUST_LOG"))
        .init();

    let port: u16 = std::env::var("RUMBLE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);

    let (cert, key) = load_or_create_dev_cert()?;
    let config = Config { port, cert, key };

    let server = Server::new(config)?;
    server.run().await
}
