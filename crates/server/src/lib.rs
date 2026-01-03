//! Rumble VOIP Server Library
//!
//! This crate provides the core server functionality for the Rumble voice chat application.
//! It is designed to be testable and reusable, with the server logic separated from the
//! binary entrypoint.
//!
//! # Architecture
//!
//! The server is organized into several modules:
//!
//! - [`config`]: Configuration management (TOML file + CLI args)
//! - [`state`]: Server state management (rooms, users, memberships)
//! - [`handlers`]: Protocol message handling
//! - [`server`]: Server startup and connection management
//!
//! # Configuration
//!
//! The server uses a layered configuration system:
//! 1. Default values
//! 2. Configuration file (rumble-server.toml)
//! 3. Command-line arguments (highest priority)
//!
//! See [`config::ServerConfig`] for details.
//!
//! # Locking Strategy
//!
//! The server uses a carefully designed locking strategy to minimize contention
//! and avoid blocking the audio path:
//!
//! - **User ID allocation**: Lock-free via `AtomicU64`
//! - **Client storage**: `DashMap` for lock-free per-client access
//! - **State data**: Single `RwLock<StateData>` for rooms and memberships
//! - **Voice relay**: Uses snapshots to avoid holding locks during I/O
//!
//! See [`state`] module documentation for details.
//!
//! # Example
//!
//! ```no_run
//! use server::{Server, Config, ServerConfig};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let server_config = ServerConfig::load()?;
//!     let (certs, key) = server_config.load_certificates()?;
//!     let data_dir = server_config.data_dir().ok().map(|p| p.to_string_lossy().to_string());
//!     let config = Config { bind: server_config.bind, certs, key, data_dir };
//!     let server = Server::new(config)?;
//!     server.run().await
//! }
//! ```

pub mod config;
pub mod handlers;
pub mod persistence;
pub mod relay;
pub mod server;
pub mod state;
pub mod tracker;

// Re-export main types for convenience
pub use config::{ServerConfig, load_pem_certificates, generate_self_signed_cert};
pub use persistence::{Persistence, PersistedRoom, RegisteredUser};
pub use relay::{RelayConfig, RelayService, RelayTokenManager};
pub use server::{Config, Server};
pub use state::{ClientHandle, ServerState, StateData};
