//! Ed25519 identity / key management. Three storage modes share one
//! `KeyManager`:
//!
//! - **Plaintext** — key bytes on disk, simplest, not recommended for
//!   shared machines.
//! - **Encrypted** — Argon2-derived key + ChaCha20-Poly1305, behind the
//!   `encrypted-keys` feature.
//! - **SSH agent** — agent stores the private key; we remember the
//!   fingerprint, behind the `ssh-agent` feature.
//!
//! Lifted verbatim from `rumble-egui/src/key_manager.rs` so both
//! clients share one identity store. The egui first-run wizard's UI
//! state (`FirstRunState`) stays in rumble-egui — it's tightly coupled
//! to that paradigm's render code.

pub mod key_manager;

pub use key_manager::{
    AgentResult, KeyConfig, KeyInfo, KeyManager, KeyManagerSigner, KeySource, SetupResult, compute_fingerprint,
    parse_signing_key,
};

#[cfg(feature = "ssh-agent")]
pub use key_manager::{
    PendingAgentOp, SharedAgentClient, SshAgentClient, connect_and_list_keys, generate_and_add_to_agent,
};
