//! Key management for Ed25519 authentication.
//!
//! Lifted from `rumble-egui/src/key_manager.rs`. The on-disk format,
//! file path (`<config>/identity.json`), and `SigningCallback` shape
//! are unchanged so existing identities load straight in.
//!
//! Encrypted-key support requires the `encrypted-keys` feature; SSH
//! agent support requires `ssh-agent`. The `KeySource` enum keeps all
//! three variants regardless — opting out of a feature only disables
//! the *handling* code, so an old identity file with a now-disabled
//! variant gives a clean error rather than corrupting on save.

use std::{path::PathBuf, sync::Arc};

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(feature = "ssh-agent")]
use tokio::sync::Mutex as TokioMutex;

/// How the key is stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum KeySource {
    /// Key is stored locally in plaintext (simplest, less secure).
    LocalPlaintext {
        /// The Ed25519 private key (32 bytes) as hex string.
        private_key_hex: String,
    },
    /// Key is stored locally, encrypted with a password. Requires the
    /// `encrypted-keys` feature to read/write.
    LocalEncrypted {
        encrypted_data: String,
        salt_hex: String,
        nonce_hex: String,
    },
    /// Key is stored in SSH agent; we remember the public key to find
    /// it again. Requires the `ssh-agent` feature.
    SshAgent {
        /// SHA256:hex fingerprint of the public key.
        fingerprint: String,
        /// Comment associated with the key in the agent.
        comment: String,
    },
}

/// Persistent key configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyConfig {
    pub source: KeySource,
    /// The Ed25519 public key (32 bytes) as hex string.
    pub public_key_hex: String,
}

impl KeyConfig {
    pub fn public_key_bytes(&self) -> Option<[u8; 32]> {
        let bytes = hex::decode(&self.public_key_hex).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Some(arr)
    }
}

/// Key info for display (from agent or local).
#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub fingerprint: String,
    pub comment: String,
    pub public_key: [u8; 32],
}

impl KeyInfo {
    pub fn from_public_key(public_key: [u8; 32], comment: String) -> Self {
        Self {
            fingerprint: compute_fingerprint(&public_key),
            comment,
            public_key,
        }
    }
}

/// Result of first-run setup.
#[allow(dead_code, clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SetupResult {
    LocalKeyGenerated(KeyConfig, SigningKey),
    AgentKeySelected(KeyConfig),
    Cancelled,
}

/// Async result type for SSH agent operations (UI consumes these).
#[allow(dead_code)]
#[derive(Debug)]
pub enum AgentResult {
    Connected,
    Keys(Vec<KeyInfo>),
    KeyAdded(KeyInfo),
    Error(String),
}

/// Key manager handles key storage and retrieval.
pub struct KeyManager {
    config_dir: PathBuf,
    config_path: PathBuf,
    config: Option<KeyConfig>,
    cached_signing_key: Option<SigningKey>,
}

impl KeyManager {
    pub fn new(config_dir: PathBuf) -> Self {
        let config_path = config_dir.join("identity.json");
        let config = Self::load_config(&config_path);

        // Plaintext keys cache the SigningKey eagerly; encrypted keys
        // wait for `unlock_local_key`.
        let cached_signing_key = config.as_ref().and_then(|c| {
            if let KeySource::LocalPlaintext { private_key_hex } = &c.source {
                parse_signing_key(private_key_hex)
            } else {
                None
            }
        });

        Self {
            config_dir,
            config_path,
            config,
            cached_signing_key,
        }
    }

    pub fn needs_setup(&self) -> bool {
        self.config.is_none()
    }

    pub fn config(&self) -> Option<&KeyConfig> {
        self.config.as_ref()
    }

    pub fn public_key_bytes(&self) -> Option<[u8; 32]> {
        self.config.as_ref()?.public_key_bytes()
    }

    pub fn public_key_hex(&self) -> Option<String> {
        self.config.as_ref().map(|c| c.public_key_hex.clone())
    }

    pub fn signing_key(&self) -> Option<&SigningKey> {
        self.cached_signing_key.as_ref()
    }

    /// Build a signing callback for the backend. `None` if no key is
    /// configured, the encrypted key hasn't been unlocked, or the
    /// configured source variant requires a feature that isn't built
    /// in (e.g. an SSH-agent identity loaded by a binary built without
    /// the `ssh-agent` feature).
    pub fn create_signer(&self) -> Option<rumble_client::SigningCallback> {
        let config = self.config.as_ref()?;
        match &config.source {
            KeySource::LocalPlaintext { private_key_hex } => {
                let signing_key = parse_signing_key(private_key_hex)?;
                Some(local_signer(signing_key))
            }
            KeySource::LocalEncrypted { .. } => {
                // The decryption happens in `unlock_local_key`; here
                // we just hand out a signer based on the cached key.
                let signing_key = self.cached_signing_key.as_ref()?;
                Some(local_signer(signing_key.clone()))
            }
            KeySource::SshAgent { fingerprint, .. } => {
                #[cfg(feature = "ssh-agent")]
                {
                    Some(create_agent_signer(fingerprint.clone()))
                }
                #[cfg(not(feature = "ssh-agent"))]
                {
                    let _ = fingerprint;
                    tracing::error!("SSH-agent identity loaded but ssh-agent feature is disabled");
                    None
                }
            }
        }
    }

    /// Generate a new local key, optionally password-protected. Empty
    /// password = plaintext storage.
    pub fn generate_local_key(&mut self, password: Option<&str>) -> anyhow::Result<KeyInfo> {
        let signing_key = SigningKey::from_bytes(&rand::random());
        let public_key = signing_key.verifying_key().to_bytes();
        let private_key = signing_key.to_bytes();

        let source = match password {
            Some(pw) if !pw.is_empty() => {
                #[cfg(feature = "encrypted-keys")]
                {
                    let (encrypted, salt, nonce) = encrypt_key(&private_key, pw)?;
                    KeySource::LocalEncrypted {
                        encrypted_data: encrypted,
                        salt_hex: salt,
                        nonce_hex: nonce,
                    }
                }
                #[cfg(not(feature = "encrypted-keys"))]
                {
                    let _ = pw;
                    anyhow::bail!("encrypted-keys feature is disabled — cannot password-protect local keys");
                }
            }
            _ => KeySource::LocalPlaintext {
                private_key_hex: hex::encode(private_key),
            },
        };

        let config = KeyConfig {
            source,
            public_key_hex: hex::encode(public_key),
        };

        self.config = Some(config);
        self.cached_signing_key = Some(signing_key);
        self.save_config()?;

        Ok(KeyInfo::from_public_key(public_key, "Rumble Identity".to_string()))
    }

    /// Decrypt a password-protected key into the cache.
    #[cfg(feature = "encrypted-keys")]
    pub fn unlock_local_key(&mut self, password: &str) -> anyhow::Result<()> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No key configured"))?;

        if let KeySource::LocalEncrypted {
            encrypted_data,
            salt_hex,
            nonce_hex,
        } = &config.source
        {
            let private_key = decrypt_key(encrypted_data, salt_hex, nonce_hex, password)?;
            let signing_key = SigningKey::from_bytes(&private_key);
            self.cached_signing_key = Some(signing_key);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Key is not encrypted"))
        }
    }

    /// Whether a key is configured but locked (requires a password).
    pub fn needs_unlock(&self) -> bool {
        if let Some(config) = &self.config {
            matches!(&config.source, KeySource::LocalEncrypted { .. }) && self.cached_signing_key.is_none()
        } else {
            false
        }
    }

    pub fn set_config(&mut self, config: KeyConfig, signing_key: Option<SigningKey>) {
        self.config = Some(config);
        self.cached_signing_key = signing_key;
    }

    /// Import a raw `SigningKey` and persist it as plaintext.
    pub fn import_signing_key(&mut self, signing_key: SigningKey) -> anyhow::Result<()> {
        let public_key = signing_key.verifying_key().to_bytes();
        let private_key = signing_key.to_bytes();

        let config = KeyConfig {
            source: KeySource::LocalPlaintext {
                private_key_hex: hex::encode(private_key),
            },
            public_key_hex: hex::encode(public_key),
        };

        self.config = Some(config);
        self.cached_signing_key = Some(signing_key);
        self.save_config()?;

        Ok(())
    }

    /// Bind the manager to a key already living in the SSH agent.
    #[cfg(feature = "ssh-agent")]
    pub fn select_agent_key(&mut self, key_info: &KeyInfo) -> anyhow::Result<()> {
        let config = KeyConfig {
            source: KeySource::SshAgent {
                fingerprint: key_info.fingerprint.clone(),
                comment: key_info.comment.clone(),
            },
            public_key_hex: hex::encode(key_info.public_key),
        };

        self.config = Some(config);
        self.cached_signing_key = None;
        self.save_config()?;

        tracing::info!(
            "Selected SSH agent key: {} ({})",
            key_info.fingerprint,
            key_info.comment
        );
        Ok(())
    }

    pub fn save_config(&self) -> anyhow::Result<()> {
        if let Some(config) = &self.config {
            std::fs::create_dir_all(&self.config_dir)?;
            let contents = serde_json::to_string_pretty(config)?;
            std::fs::write(&self.config_path, contents)?;
            tracing::info!("Saved identity config to {}", self.config_path.display());
        }
        Ok(())
    }

    fn load_config(path: &PathBuf) -> Option<KeyConfig> {
        let data = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str(&data) {
            Ok(config) => Some(config),
            Err(e) => {
                tracing::warn!("Failed to parse identity config: {}", e);
                None
            }
        }
    }

    pub fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }
}

/// SHA256 fingerprint of a public key, formatted as
/// `SHA256:xx:xx:...` (16 bytes). Stable across releases — used as
/// the on-disk identifier for SSH-agent key selection.
pub fn compute_fingerprint(public_key: &[u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    let hash = hasher.finalize();
    let hex_parts: Vec<String> = hash.iter().take(16).map(|b| format!("{:02x}", b)).collect();
    format!("SHA256:{}", hex_parts.join(":"))
}

pub fn parse_signing_key(hex_key: &str) -> Option<SigningKey> {
    let bytes = hex::decode(hex_key).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Some(SigningKey::from_bytes(&arr))
}

fn local_signer(signing_key: SigningKey) -> rumble_client::SigningCallback {
    let signing_key_bytes = signing_key.to_bytes();
    Arc::new(move |payload: &[u8]| {
        use ed25519_dalek::Signer;
        let key = SigningKey::from_bytes(&signing_key_bytes);
        Ok(key.sign(payload).to_bytes())
    })
}

// =============================================================================
// Encrypted key helpers
// =============================================================================

#[cfg(feature = "encrypted-keys")]
fn encrypt_key(key: &[u8; 32], password: &str) -> anyhow::Result<(String, String, String)> {
    use argon2::Argon2;
    use chacha20poly1305::{
        ChaCha20Poly1305, Nonce,
        aead::{Aead, KeyInit},
    };

    let mut salt_bytes = [0u8; 16];
    let mut nonce_bytes = [0u8; 12];
    getrandom::fill(&mut salt_bytes).map_err(|e| anyhow::anyhow!("Failed to generate random salt: {}", e))?;
    getrandom::fill(&mut nonce_bytes).map_err(|e| anyhow::anyhow!("Failed to generate random nonce: {}", e))?;

    let salt_hex = hex::encode(salt_bytes);

    let mut derived_key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt_hex.as_bytes(), &mut derived_key)
        .map_err(|e| anyhow::anyhow!("Key derivation failed: {}", e))?;

    let cipher =
        ChaCha20Poly1305::new_from_slice(&derived_key).map_err(|e| anyhow::anyhow!("Cipher init failed: {}", e))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, key.as_slice())
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    Ok((hex::encode(ciphertext), salt_hex, hex::encode(nonce_bytes)))
}

#[cfg(feature = "encrypted-keys")]
fn decrypt_key(encrypted_hex: &str, salt: &str, nonce_hex: &str, password: &str) -> anyhow::Result<[u8; 32]> {
    use argon2::Argon2;
    use chacha20poly1305::{
        ChaCha20Poly1305, Nonce,
        aead::{Aead, KeyInit},
    };

    let ciphertext = hex::decode(encrypted_hex)?;
    let nonce_bytes = hex::decode(nonce_hex)?;
    if nonce_bytes.len() != 12 {
        return Err(anyhow::anyhow!("Invalid nonce length"));
    }

    let mut derived_key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt.as_bytes(), &mut derived_key)
        .map_err(|e| anyhow::anyhow!("Key derivation failed: {}", e))?;

    let cipher =
        ChaCha20Poly1305::new_from_slice(&derived_key).map_err(|e| anyhow::anyhow!("Cipher init failed: {}", e))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_slice())
        .map_err(|_| anyhow::anyhow!("Decryption failed - wrong password?"))?;

    if plaintext.len() != 32 {
        return Err(anyhow::anyhow!("Invalid decrypted key length"));
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&plaintext);
    Ok(key)
}

// =============================================================================
// SSH Agent Client
// =============================================================================

#[cfg(feature = "ssh-agent")]
fn get_agent_path() -> anyhow::Result<String> {
    if let Ok(path) = std::env::var("SSH_AUTH_SOCK") {
        return Ok(path);
    }

    #[cfg(windows)]
    {
        Ok(r"\\.\pipe\openssh-ssh-agent".to_string())
    }

    #[cfg(not(windows))]
    {
        Err(anyhow::anyhow!("SSH_AUTH_SOCK not set - is ssh-agent running?"))
    }
}

/// SSH agent client wrapper for Ed25519 key operations.
#[cfg(feature = "ssh-agent")]
pub struct SshAgentClient {
    client: Box<dyn ssh_agent_lib::agent::Session + Send + Sync>,
}

#[cfg(feature = "ssh-agent")]
impl SshAgentClient {
    pub async fn connect() -> anyhow::Result<Self> {
        let agent_path = get_agent_path()?;
        tracing::debug!("Connecting to SSH agent at: {}", agent_path);

        let binding = Self::parse_agent_binding(&agent_path)?;
        let stream: service_binding::Stream = binding
            .try_into()
            .map_err(|e: std::io::Error| anyhow::anyhow!("Failed to connect to SSH agent: {}", e))?;

        let client = ssh_agent_lib::client::connect(stream)
            .map_err(|e| anyhow::anyhow!("Failed to create SSH agent client: {}", e))?;

        tracing::info!("Connected to SSH agent");
        Ok(Self { client })
    }

    fn parse_agent_binding(path: &str) -> anyhow::Result<service_binding::Binding> {
        if path.starts_with(r"\\") {
            return Ok(service_binding::Binding::NamedPipe(path.into()));
        }
        if path.starts_with("npipe://") {
            return path
                .parse()
                .map_err(|e| anyhow::anyhow!("Failed to parse named pipe path: {:?}", e));
        }

        #[cfg(unix)]
        {
            Ok(service_binding::Binding::FilePath(path.into()))
        }

        #[cfg(not(unix))]
        {
            Err(anyhow::anyhow!("Unix socket paths not supported on Windows: {}", path))
        }
    }

    pub fn is_available() -> bool {
        get_agent_path().is_ok()
    }

    pub async fn list_ed25519_keys(&mut self) -> anyhow::Result<Vec<KeyInfo>> {
        use ssh_key::public::KeyData;

        let identities = self
            .client
            .request_identities()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to request identities: {}", e))?;

        let mut ed25519_keys = Vec::new();
        for id in identities {
            if let KeyData::Ed25519(ed_key) = &id.pubkey {
                let public_key: [u8; 32] = ed_key.0;
                ed25519_keys.push(KeyInfo::from_public_key(public_key, id.comment.clone()));
            }
        }

        tracing::info!("Found {} Ed25519 keys in SSH agent", ed25519_keys.len());
        Ok(ed25519_keys)
    }

    pub async fn sign(&mut self, fingerprint: &str, data: &[u8]) -> anyhow::Result<[u8; 64]> {
        use ssh_agent_lib::proto::SignRequest;
        use ssh_key::public::KeyData;

        let identities = self
            .client
            .request_identities()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to request identities: {}", e))?;

        let identity = identities
            .into_iter()
            .find(|id| {
                if let KeyData::Ed25519(ed_key) = &id.pubkey {
                    let public_key: [u8; 32] = ed_key.0;
                    compute_fingerprint(&public_key) == fingerprint
                } else {
                    false
                }
            })
            .ok_or_else(|| anyhow::anyhow!("Key with fingerprint {} not found in agent", fingerprint))?;

        let request = SignRequest {
            pubkey: identity.pubkey.clone(),
            data: data.to_vec(),
            flags: 0,
        };

        let signature = self
            .client
            .sign(request)
            .await
            .map_err(|e| anyhow::anyhow!("Signing failed: {}", e))?;

        let sig_bytes = signature.as_bytes();
        if sig_bytes.len() != 64 {
            return Err(anyhow::anyhow!(
                "Invalid signature format: expected 64 bytes for Ed25519, got {}",
                sig_bytes.len()
            ));
        }

        let mut raw_sig = [0u8; 64];
        raw_sig.copy_from_slice(sig_bytes);
        Ok(raw_sig)
    }

    pub async fn add_key(&mut self, signing_key: &SigningKey, comment: &str) -> anyhow::Result<()> {
        use ssh_agent_lib::proto::{AddIdentity, Credential};
        use ssh_key::{
            private::{Ed25519Keypair, KeypairData},
            public::Ed25519PublicKey,
        };

        let public_key_bytes = signing_key.verifying_key().to_bytes();
        let private_key_bytes = signing_key.to_bytes();

        let ed25519_keypair = Ed25519Keypair {
            public: Ed25519PublicKey(public_key_bytes),
            private: ssh_key::private::Ed25519PrivateKey::from_bytes(&private_key_bytes),
        };

        let credential = Credential::Key {
            privkey: KeypairData::Ed25519(ed25519_keypair),
            comment: comment.to_string(),
        };

        self.client
            .add_identity(AddIdentity { credential })
            .await
            .map_err(|e| anyhow::anyhow!("Failed to add key to agent: {}", e))?;

        tracing::info!("Successfully added key to SSH agent");
        Ok(())
    }
}

/// Pending async operation for the UI to poll.
#[cfg(feature = "ssh-agent")]
pub enum PendingAgentOp {
    Connect(tokio::task::JoinHandle<anyhow::Result<Vec<KeyInfo>>>),
    AddKey(tokio::task::JoinHandle<anyhow::Result<KeyInfo>>),
    #[allow(dead_code)]
    Sign(tokio::task::JoinHandle<anyhow::Result<[u8; 64]>>),
}

#[cfg(feature = "ssh-agent")]
#[allow(dead_code)]
pub type SharedAgentClient = Arc<TokioMutex<Option<SshAgentClient>>>;

/// Connect to SSH agent and list keys (async helper for UI).
#[cfg(feature = "ssh-agent")]
pub async fn connect_and_list_keys() -> anyhow::Result<Vec<KeyInfo>> {
    let mut client = SshAgentClient::connect().await?;
    client.list_ed25519_keys().await
}

/// Generate a key and add it to the SSH agent.
#[cfg(feature = "ssh-agent")]
pub async fn generate_and_add_to_agent(comment: String) -> anyhow::Result<KeyInfo> {
    let mut client = SshAgentClient::connect().await?;

    let signing_key = SigningKey::from_bytes(&rand::random());
    let public_key = signing_key.verifying_key().to_bytes();
    client.add_key(&signing_key, &comment).await?;

    Ok(KeyInfo::from_public_key(public_key, comment))
}

/// Build a SigningCallback that hits the SSH agent on every signature.
///
/// `SigningCallback` is sync, but the agent client is async. To avoid
/// nested-runtime issues we spawn a one-shot thread per signature. This
/// is fine for the handshake but would be wasteful for high-frequency
/// signing — keep an eye on it if we ever add periodic re-auth.
#[cfg(feature = "ssh-agent")]
pub fn create_agent_signer(fingerprint: String) -> rumble_client::SigningCallback {
    Arc::new(move |payload: &[u8]| {
        let fingerprint = fingerprint.clone();
        let payload = payload.to_vec();

        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("Failed to create runtime: {}", e))
                .and_then(|rt| {
                    rt.block_on(async {
                        let mut client = SshAgentClient::connect()
                            .await
                            .map_err(|e| format!("Failed to connect to agent: {}", e))?;

                        client
                            .sign(&fingerprint, &payload)
                            .await
                            .map_err(|e| format!("Signing failed: {}", e))
                    })
                });
            let _ = tx.send(result);
        });

        rx.recv().map_err(|e| format!("Channel receive error: {}", e))?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_format() {
        let fp = compute_fingerprint(&[0u8; 32]);
        assert!(fp.starts_with("SHA256:"));
    }

    #[cfg(feature = "encrypted-keys")]
    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [42u8; 32];
        let (encrypted, salt, nonce) = encrypt_key(&key, "test_password").unwrap();
        let decrypted = decrypt_key(&encrypted, &salt, &nonce, "test_password").unwrap();
        assert_eq!(key, decrypted);
    }

    #[cfg(feature = "encrypted-keys")]
    #[test]
    fn decrypt_wrong_password() {
        let key = [42u8; 32];
        let (encrypted, salt, nonce) = encrypt_key(&key, "correct").unwrap();
        assert!(decrypt_key(&encrypted, &salt, &nonce, "wrong").is_err());
    }

    #[test]
    fn parse_signing_key_roundtrip() {
        let key = SigningKey::from_bytes(&rand::random());
        let hex = hex::encode(key.to_bytes());
        let parsed = parse_signing_key(&hex).unwrap();
        assert_eq!(key.to_bytes(), parsed.to_bytes());
    }
}
