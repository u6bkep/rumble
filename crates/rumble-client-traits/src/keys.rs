//! Identity signing abstraction.
//!
//! The engine signs auth challenges during the QUIC handshake via this trait.
//! `sign` is async so implementations can reach out to hardware-backed
//! keystores (SSH agent, macOS Keychain, Android Keystore, iOS Secure Enclave,
//! etc.) without blocking the calling task.

use async_trait::async_trait;

/// Identity signing — the only key-manager surface the client engine needs.
///
/// Higher-level concerns (key generation, on-disk storage, password unlock,
/// agent enumeration) live in the app or shell layer; they are not part of
/// this trait.
#[async_trait]
pub trait KeySigning: Send + Sync + 'static {
    /// Sign `payload` with the private half of `public_key`.
    ///
    /// Returns the 64-byte Ed25519 signature. `public_key` lets a
    /// multi-identity implementation pick the right key; single-identity
    /// implementations may ignore it.
    async fn sign(&self, public_key: &[u8; 32], payload: &[u8]) -> anyhow::Result<[u8; 64]>;
}
