//! Key management for Ed25519 authentication.
//!
//! Lifted from `rumble-egui/src/key_manager.rs`. The on-disk format and
//! file path (`<config>/identity.json`) are unchanged so existing
//! identities load straight in. Signing is exposed to the engine via
//! [`KeyManagerSigner`], which implements [`rumble_client_traits::KeySigning`]
//! and is the seam future macOS Keychain / mobile keyring backends will
//! plug into.
//!
//! Encrypted-key support requires the `encrypted-keys` feature; SSH
//! agent support requires `ssh-agent`. The `KeySource` enum keeps all
//! three variants regardless — opting out of a feature only disables
//! the *handling* code, so an old identity file with a now-disabled
//! variant gives a clean error rather than corrupting on save.

use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use ed25519_dalek::SigningKey;
use rumble_client_traits::KeySigning;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(feature = "ssh-agent")]
use tokio::sync::Mutex as TokioMutex;

/// Write `contents` to `path` atomically, owner-read/write only (`0600`) on Unix.
///
/// The identity file can hold a plaintext private key, so it must not be
/// world-readable. Atomicity is achieved by writing to a sibling temp file
/// (`<name>.tmp` in the same directory), calling `sync_all`, then renaming
/// into place. A crash between write and rename leaves the original intact
/// rather than truncating it to zero.
///
/// On Unix the temp file is created with mode `0600` directly; permissions are
/// also tightened before rename to cover a pre-existing temp file with a looser
/// mode. On other platforms the temp+rename pattern still applies; access
/// control relies on the containing directory.
fn write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp_path = std::path::PathBuf::from(tmp_name);

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        file.write_all(contents)?;
        // Tighten permissions before rename: `.mode()` applies only on creation,
        // so a pre-existing temp file may carry a looser mode.
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }

    std::fs::rename(&tmp_path, path)
}

/// Rename `path` to `<path>.corrupt` so a subsequent overwrite cannot destroy
/// the raw bytes. Failure to rename is non-fatal: the caller must decide whether
/// to proceed anyway or disable further writes.
fn backup_corrupt_identity_file(path: &std::path::Path) {
    let mut backup_name = path.as_os_str().to_owned();
    backup_name.push(".corrupt");
    let backup = std::path::PathBuf::from(backup_name);
    match std::fs::rename(path, &backup) {
        Ok(()) => tracing::warn!("identity: corrupt file preserved as {}", backup.display()),
        Err(e) => tracing::warn!("identity: could not rename corrupt file to {}: {e}", backup.display()),
    }
}

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
            write_private(&self.config_path, contents.as_bytes())?;
            tracing::info!("Saved identity config to {}", self.config_path.display());
        }
        Ok(())
    }

    fn load_config(path: &PathBuf) -> Option<KeyConfig> {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                // File exists but is unreadable. Back it up so the setup flow
                // cannot silently destroy the raw bytes via save_config.
                tracing::warn!("identity: could not read {}: {e}", path.display());
                backup_corrupt_identity_file(path);
                return None;
            }
        };
        match serde_json::from_str(&data) {
            Ok(config) => Some(config),
            Err(e) => {
                // File is present but unparseable. Back it up before the setup
                // flow calls save_config(), which would overwrite it.
                tracing::warn!("identity: failed to parse {}: {e}", path.display());
                backup_corrupt_identity_file(path);
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

/// `KeySigning` adapter that signs using a shared [`KeyManager`].
///
/// Wraps the manager in an [`Arc<RwLock<_>>`] so the UI can keep mutating
/// the identity (generate, unlock, switch agent key) while the backend
/// holds a long-lived signer that always reflects the latest state.
///
/// Locking is brief: we snapshot the source + cached key under the lock,
/// release it, then do any async work (SSH-agent IPC) without holding it.
pub struct KeyManagerSigner {
    inner: Arc<RwLock<KeyManager>>,
}

impl KeyManagerSigner {
    pub fn new(inner: Arc<RwLock<KeyManager>>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl KeySigning for KeyManagerSigner {
    async fn sign(&self, _public_key: &[u8; 32], payload: &[u8]) -> anyhow::Result<[u8; 64]> {
        let (source, cached_key) = {
            let km = self
                .inner
                .read()
                .map_err(|_| anyhow::anyhow!("KeyManager lock poisoned"))?;
            let config = km
                .config
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No identity configured"))?;
            (config.source.clone(), km.cached_signing_key.clone())
        };

        match source {
            KeySource::LocalPlaintext { private_key_hex } => {
                use ed25519_dalek::Signer;
                let signing_key =
                    parse_signing_key(&private_key_hex).ok_or_else(|| anyhow::anyhow!("Invalid stored private key"))?;
                Ok(signing_key.sign(payload).to_bytes())
            }
            KeySource::LocalEncrypted { .. } => {
                use ed25519_dalek::Signer;
                let sk = cached_key.ok_or_else(|| anyhow::anyhow!("Encrypted identity not unlocked"))?;
                Ok(sk.sign(payload).to_bytes())
            }
            #[cfg(feature = "ssh-agent")]
            KeySource::SshAgent { fingerprint, .. } => {
                let mut client = SshAgentClient::connect().await?;
                client.sign(&fingerprint, payload).await
            }
            #[cfg(not(feature = "ssh-agent"))]
            KeySource::SshAgent { .. } => anyhow::bail!("ssh-agent feature is disabled"),
        }
    }
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
            if let KeyData::Ed25519(ed_key) = id.credential.key_data() {
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
                if let KeyData::Ed25519(ed_key) = id.credential.key_data() {
                    let public_key: [u8; 32] = ed_key.0;
                    compute_fingerprint(&public_key) == fingerprint
                } else {
                    false
                }
            })
            .ok_or_else(|| anyhow::anyhow!("Key with fingerprint {} not found in agent", fingerprint))?;

        let request = SignRequest {
            credential: identity.credential.clone(),
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
        use ssh_agent_lib::proto::{AddIdentity, PrivateCredential};
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

        let credential = PrivateCredential::Key {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir_for_test() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rumble-key-manager-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

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

    /// write_private must leave no temp file behind and write the expected bytes.
    #[test]
    fn write_private_atomic_no_leftover_temp() {
        let dir = tempdir_for_test();
        let target = dir.join("identity.json");
        let content = b"{ \"test\": true }";

        write_private(&target, content).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), content);

        let mut tmp_name = target.as_os_str().to_owned();
        tmp_name.push(".tmp");
        assert!(
            !std::path::Path::new(&tmp_name).exists(),
            "temp file must be cleaned up after successful write"
        );
    }

    /// write_private must produce a 0600 file on Unix.
    #[cfg(unix)]
    #[test]
    fn write_private_sets_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir_for_test();
        let target = dir.join("identity.json");
        write_private(&target, b"secret").unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "identity file must be owner-read/write only");
    }

    /// A corrupt identity file must be renamed to .corrupt before the setup
    /// flow can regenerate and save a new key.
    #[test]
    fn load_config_preserves_corrupt_file_as_backup() {
        let dir = tempdir_for_test();
        let identity_path = dir.join("identity.json");
        std::fs::write(&identity_path, b"not valid json {{{").unwrap();

        let km = KeyManager::new(dir.clone());

        assert!(km.needs_setup(), "corrupt identity should trigger setup");

        // Original file must have been renamed to the backup.
        let mut backup_name = identity_path.as_os_str().to_owned();
        backup_name.push(".corrupt");
        let backup = std::path::PathBuf::from(backup_name);
        assert!(backup.exists(), "corrupt backup must exist at identity.json.corrupt");
        assert_eq!(
            std::fs::read(&backup).unwrap(),
            b"not valid json {{{",
            "backup must contain the original bytes verbatim"
        );
        assert!(
            !identity_path.exists(),
            "original corrupt file must have been renamed to backup"
        );
    }
}
