//! Ed25519 identity wrapper, backed by `rumble_desktop_shell::KeyManager`.
//!
//! Mirrors `rumble-next::identity::Identity` so the on-disk
//! `identity.json` written by either client loads identically here.
//!
//! The underlying `KeyManager` is held inside an `Arc<RwLock<...>>` so the
//! UI can continue mutating identity state (generate, unlock, switch agent
//! key) while the backend holds a long-lived `Arc<dyn KeySigning>` produced
//! by [`Identity::signer`]. Signs see the current state on every call.

use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

use ed25519_dalek::SigningKey;
use rumble_client_traits::KeySigning;
use rumble_desktop_shell::{KeyInfo, KeyManager, KeyManagerSigner, compute_fingerprint};

pub struct Identity {
    manager: Arc<RwLock<KeyManager>>,
    public_key: Option<[u8; 32]>,
}

impl Identity {
    pub fn load(config_dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config_dir)?;
        let km = KeyManager::new(config_dir);
        let public_key = km.public_key_bytes();
        Ok(Self {
            manager: Arc::new(RwLock::new(km)),
            public_key,
        })
    }

    pub fn public_key(&self) -> Option<[u8; 32]> {
        self.public_key
    }

    /// Long-lived signer that the backend installs at construction. Reads
    /// the latest identity state on every signature, so UI updates
    /// (unlock, switch agent key) take effect without rebuilding the
    /// `BackendHandle`.
    pub fn signer(&self) -> Arc<dyn KeySigning> {
        Arc::new(KeyManagerSigner::new(self.manager.clone()))
    }

    pub fn needs_setup(&self) -> bool {
        self.manager.read().expect("identity lock poisoned").needs_setup()
    }

    pub fn needs_unlock(&self) -> bool {
        self.manager.read().expect("identity lock poisoned").needs_unlock()
    }

    pub fn generate_local_key(&mut self, password: Option<&str>) -> anyhow::Result<KeyInfo> {
        let info = self
            .manager
            .write()
            .expect("identity lock poisoned")
            .generate_local_key(password)?;
        self.public_key = Some(info.public_key);
        Ok(info)
    }

    pub fn select_agent_key(&mut self, key_info: &KeyInfo) -> anyhow::Result<()> {
        self.manager
            .write()
            .expect("identity lock poisoned")
            .select_agent_key(key_info)?;
        self.public_key = Some(key_info.public_key);
        Ok(())
    }

    pub fn unlock(&mut self, password: &str) -> anyhow::Result<()> {
        let mut km = self.manager.write().expect("identity lock poisoned");
        km.unlock_local_key(password)?;
        self.public_key = km.public_key_bytes();
        Ok(())
    }

    pub fn fingerprint(&self) -> String {
        self.public_key
            .map(|key| compute_fingerprint(&key))
            .unwrap_or_else(|| "(not set up)".to_string())
    }

    pub fn signing_key(&self) -> Option<SigningKey> {
        self.manager
            .read()
            .expect("identity lock poisoned")
            .signing_key()
            .cloned()
    }

    /// Read-only access to the underlying manager. Returns a guard;
    /// `&*identity.manager()` gives a `&KeyManager` for methods like
    /// `config()` and `config_dir()`.
    pub fn manager(&self) -> std::sync::RwLockReadGuard<'_, KeyManager> {
        self.manager.read().expect("identity lock poisoned")
    }

    /// Write access for first-run/unlock flows.
    pub fn manager_mut(&self) -> std::sync::RwLockWriteGuard<'_, KeyManager> {
        self.manager.write().expect("identity lock poisoned")
    }
}
