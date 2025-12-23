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
//! - [`state`]: Server state management (rooms, users, memberships)
//! - [`handlers`]: Protocol message handling
//! - [`server`]: Server startup and connection management
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
//! use server::{Server, Config, load_or_create_dev_cert};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let (cert, key) = load_or_create_dev_cert()?;
//!     let config = Config { port: 5000, cert, key };
//!     let server = Server::new(config)?;
//!     server.run().await
//! }
//! ```

pub mod handlers;
pub mod server;
pub mod state;

// Re-export main types for convenience
pub use server::{Config, Server};
pub use state::{ClientHandle, ServerState, StateData};

use anyhow::Result;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tracing::info;

/// Generate or load a persistent development self-signed certificate/key pair.
/// 
/// Certificates are stored as DER in `dev-certs/server-cert.der` and 
/// key in `dev-certs/server-key.der`.
/// 
/// # Returns
/// A tuple of (certificate, private key) suitable for use with rustls/quinn.
pub fn load_or_create_dev_cert() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert_path = std::path::Path::new("dev-certs/server-cert.der");
    let key_path = std::path::Path::new("dev-certs/server-key.der");

    if cert_path.exists() && key_path.exists() {
        let cert_bytes = std::fs::read(cert_path)?;
        let key_bytes = std::fs::read(key_path)?;
        info!("Loaded existing dev cert and key (DER)");
        let cert = CertificateDer::from(cert_bytes);
        let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(key_bytes);
        return Ok((cert, PrivateKeyDer::from(pkcs8)));
    }

    std::fs::create_dir_all("dev-certs").ok();
    let ck = rcgen::generate_simple_self_signed(["localhost".into()])?;
    let pem = ck.cert.pem();
    let der = pem_to_der(&pem)?;
    let key_der = ck.signing_key.serialize_der();

    std::fs::write(cert_path, &der).ok();
    std::fs::write("dev-certs/server-cert.pem", pem.as_bytes()).ok();
    std::fs::write(key_path, key_der.as_slice()).ok();

    let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(key_der.clone());
    info!("Generated new dev cert and key (DER + PEM)");
    Ok((CertificateDer::from(der), PrivateKeyDer::from(pkcs8)))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let mut b64 = String::new();
    for line in pem.lines() {
        if line.starts_with("-----") {
            continue;
        }
        b64.push_str(line.trim());
    }
    let der = B64
        .decode(b64)
        .map_err(|e| anyhow::anyhow!("PEM decode error: {e}"))?;
    Ok(der)
}
