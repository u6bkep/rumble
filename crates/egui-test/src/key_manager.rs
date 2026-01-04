//! Key management for Ed25519 authentication.
//!
//! Handles:
//! - Local key storage (plaintext or password-protected)
//! - SSH agent integration (preferred for security)
//! - First-run setup dialog coordination

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

/// How the key is stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum KeySource {
    /// Key is stored locally in plaintext (simplest, less secure).
    LocalPlaintext {
        /// The Ed25519 private key (32 bytes) as hex string.
        private_key_hex: String,
    },
    /// Key is stored locally, encrypted with a password.
    LocalEncrypted {
        /// The encrypted private key data.
        encrypted_data: String,
        /// Salt used for key derivation (hex encoded).
        salt_hex: String,
        /// Nonce used for encryption (hex encoded).
        nonce_hex: String,
    },
    /// Key is stored in SSH agent; we remember the public key to find it.
    SshAgent {
        /// The public key fingerprint (SHA256:base64).
        fingerprint: String,
        /// Comment associated with the key in the agent.
        comment: String,
    },
}

/// Persistent key configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyConfig {
    /// How/where the key is stored.
    pub source: KeySource,
    /// The Ed25519 public key (32 bytes) as hex string.
    pub public_key_hex: String,
}

impl KeyConfig {
    /// Get the public key bytes.
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
    /// SHA256 fingerprint of the public key.
    pub fingerprint: String,
    /// Comment or label for the key.
    pub comment: String,
    /// The public key (32 bytes).
    pub public_key: [u8; 32],
}

impl KeyInfo {
    /// Create KeyInfo from a public key.
    pub fn from_public_key(public_key: [u8; 32], comment: String) -> Self {
        Self {
            fingerprint: compute_fingerprint(&public_key),
            comment,
            public_key,
        }
    }
}

/// State of the first-run setup flow.
#[derive(Debug, Clone, Default)]
pub enum FirstRunState {
    /// Not in first-run mode (key is configured).
    #[default]
    NotNeeded,
    /// Showing the main selection screen.
    SelectMethod,
    /// Generating a new local key (with optional password).
    GenerateLocal {
        /// Password for encryption (empty = no encryption).
        password: String,
        /// Password confirmation.
        password_confirm: String,
        /// Error message if passwords don't match.
        error: Option<String>,
    },
    /// Connecting to SSH agent.
    ConnectingAgent,
    /// Selecting a key from SSH agent.
    SelectAgentKey {
        /// Available keys from agent.
        keys: Vec<KeyInfo>,
        /// Currently selected key index.
        selected: Option<usize>,
        /// Error message if any.
        error: Option<String>,
    },
    /// Generating a new key to add to agent.
    GenerateAgentKey {
        /// Comment for the new key.
        comment: String,
    },
    /// Error state.
    Error {
        message: String,
    },
    /// Setup complete.
    Complete,
}

/// Result of first-run setup (for future use when key unlock dialog is needed).
#[allow(dead_code)]
#[derive(Debug)]
pub enum SetupResult {
    /// User generated a local key.
    LocalKeyGenerated(KeyConfig, SigningKey),
    /// User selected an agent key.
    AgentKeySelected(KeyConfig),
    /// User cancelled setup.
    Cancelled,
}

/// Key manager handles key storage and retrieval.
pub struct KeyManager {
    /// Path to the config directory.
    config_dir: PathBuf,
    /// Path to the identity config file.
    config_path: PathBuf,
    /// Current key configuration.
    config: Option<KeyConfig>,
    /// Cached signing key (for local keys).
    cached_signing_key: Option<SigningKey>,
}

impl KeyManager {
    /// Create a new key manager with the given config directory.
    pub fn new(config_dir: PathBuf) -> Self {
        let config_path = config_dir.join("identity.json");
        let config = Self::load_config(&config_path);
        
        // If we have a local plaintext key, cache the signing key
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
    
    /// Check if first-run setup is needed.
    pub fn needs_setup(&self) -> bool {
        self.config.is_none()
    }
    
    /// Get the current key config.
    pub fn config(&self) -> Option<&KeyConfig> {
        self.config.as_ref()
    }
    
    /// Get the public key bytes if configured.
    pub fn public_key_bytes(&self) -> Option<[u8; 32]> {
        self.config.as_ref()?.public_key_bytes()
    }
    
    /// Get the public key as hex string.
    pub fn public_key_hex(&self) -> Option<String> {
        self.config.as_ref().map(|c| c.public_key_hex.clone())
    }
    
    /// Get the cached signing key (for local keys).
    pub fn signing_key(&self) -> Option<&SigningKey> {
        self.cached_signing_key.as_ref()
    }
    
    /// Create a signing callback for the backend.
    /// 
    /// Returns None if key is not available or requires unlocking.
    pub fn create_signer(&self) -> Option<backend::SigningCallback> {
        let config = self.config.as_ref()?;
        
        match &config.source {
            KeySource::LocalPlaintext { private_key_hex } => {
                let signing_key = parse_signing_key(private_key_hex)?;
                let signing_key_bytes = signing_key.to_bytes();
                
                Some(Arc::new(move |payload: &[u8]| {
                    use ed25519_dalek::Signer;
                    let key = SigningKey::from_bytes(&signing_key_bytes);
                    let signature = key.sign(payload);
                    Ok(signature.to_bytes())
                }))
            }
            KeySource::LocalEncrypted { .. } => {
                // Encrypted keys require unlocking first
                // Use the cached signing key if available
                let signing_key = self.cached_signing_key.as_ref()?;
                let signing_key_bytes = signing_key.to_bytes();
                
                Some(Arc::new(move |payload: &[u8]| {
                    use ed25519_dalek::Signer;
                    let key = SigningKey::from_bytes(&signing_key_bytes);
                    let signature = key.sign(payload);
                    Ok(signature.to_bytes())
                }))
            }
            KeySource::SshAgent { fingerprint, .. } => {
                // Use the SSH agent for signing
                Some(create_agent_signer(fingerprint.clone()))
            }
        }
    }
    
    /// Generate a new local key with optional password protection.
    pub fn generate_local_key(&mut self, password: Option<&str>) -> anyhow::Result<KeyInfo> {
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = signing_key.verifying_key().to_bytes();
        let private_key = signing_key.to_bytes();
        
        let source = if let Some(pw) = password {
            if pw.is_empty() {
                // Empty password = no encryption
                KeySource::LocalPlaintext {
                    private_key_hex: hex::encode(private_key),
                }
            } else {
                // Encrypt with password
                let (encrypted, salt, nonce) = encrypt_key(&private_key, pw)?;
                KeySource::LocalEncrypted {
                    encrypted_data: encrypted,
                    salt_hex: salt,
                    nonce_hex: nonce,
                }
            }
        } else {
            // No password = plaintext storage
            KeySource::LocalPlaintext {
                private_key_hex: hex::encode(private_key),
            }
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
    
    /// Unlock a password-protected local key.
    #[allow(dead_code)]
    pub fn unlock_local_key(&mut self, password: &str) -> anyhow::Result<()> {
        let config = self.config.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No key configured"))?;
        
        if let KeySource::LocalEncrypted { encrypted_data, salt_hex, nonce_hex } = &config.source {
            let private_key = decrypt_key(encrypted_data, salt_hex, nonce_hex, password)?;
            let signing_key = SigningKey::from_bytes(&private_key);
            self.cached_signing_key = Some(signing_key);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Key is not encrypted"))
        }
    }
    
    /// Check if the key requires unlocking.
    pub fn needs_unlock(&self) -> bool {
        if let Some(config) = &self.config {
            matches!(&config.source, KeySource::LocalEncrypted { .. }) 
                && self.cached_signing_key.is_none()
        } else {
            false
        }
    }
    
    /// Set the key config (used when importing from old settings).
    #[allow(dead_code)]
    pub fn set_config(&mut self, config: KeyConfig, signing_key: Option<SigningKey>) {
        self.config = Some(config);
        self.cached_signing_key = signing_key;
    }
    
    /// Import a signing key from raw bytes (migration from old format).
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
    
    /// Select an existing key from the SSH agent.
    pub fn select_agent_key(&mut self, key_info: &KeyInfo) -> anyhow::Result<()> {
        let config = KeyConfig {
            source: KeySource::SshAgent {
                fingerprint: key_info.fingerprint.clone(),
                comment: key_info.comment.clone(),
            },
            public_key_hex: hex::encode(key_info.public_key),
        };
        
        self.config = Some(config);
        // No cached signing key for agent keys - we use the agent for signing
        self.cached_signing_key = None;
        self.save_config()?;
        
        tracing::info!("Selected SSH agent key: {} ({})", key_info.fingerprint, key_info.comment);
        Ok(())
    }
    
    /// Save the current config to disk.
    pub fn save_config(&self) -> anyhow::Result<()> {
        if let Some(config) = &self.config {
            // Ensure config directory exists
            std::fs::create_dir_all(&self.config_dir)?;
            
            let contents = serde_json::to_string_pretty(config)?;
            std::fs::write(&self.config_path, contents)?;
            
            tracing::info!("Saved identity config to {}", self.config_path.display());
        }
        Ok(())
    }
    
    /// Load config from disk.
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
    
    /// Get the config directory path.
    #[allow(dead_code)]
    pub fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }
}

/// Compute SHA256 fingerprint of a public key.
pub fn compute_fingerprint(public_key: &[u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    let hash = hasher.finalize();
    // Use first 16 bytes, format as hex pairs separated by colons
    let hex_parts: Vec<String> = hash.iter().take(16).map(|b| format!("{:02x}", b)).collect();
    format!("SHA256:{}", hex_parts.join(":"))
}

/// Parse a signing key from hex string.
pub fn parse_signing_key(hex_key: &str) -> Option<SigningKey> {
    let bytes = hex::decode(hex_key).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Some(SigningKey::from_bytes(&arr))
}

/// Encrypt a private key with a password using Argon2 + ChaCha20-Poly1305.
fn encrypt_key(key: &[u8; 32], password: &str) -> anyhow::Result<(String, String, String)> {
    use argon2::{Argon2, password_hash::SaltString};
    use chacha20poly1305::{
        aead::{Aead, KeyInit},
        ChaCha20Poly1305, Nonce,
    };
    
    // Generate random salt and nonce
    let salt = SaltString::generate(&mut OsRng);
    let mut nonce_bytes = [0u8; 12];
    rand::RngCore::fill_bytes(&mut OsRng, &mut nonce_bytes);
    
    // Derive encryption key using Argon2
    let mut derived_key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt.as_str().as_bytes(), &mut derived_key)
        .map_err(|e| anyhow::anyhow!("Key derivation failed: {}", e))?;
    
    // Encrypt with ChaCha20-Poly1305
    let cipher = ChaCha20Poly1305::new_from_slice(&derived_key)
        .map_err(|e| anyhow::anyhow!("Cipher init failed: {}", e))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, key.as_slice())
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;
    
    Ok((
        hex::encode(ciphertext),
        salt.as_str().to_string(),
        hex::encode(nonce_bytes),
    ))
}

/// Decrypt a private key with a password.
fn decrypt_key(
    encrypted_hex: &str,
    salt: &str,
    nonce_hex: &str,
    password: &str,
) -> anyhow::Result<[u8; 32]> {
    use argon2::Argon2;
    use chacha20poly1305::{
        aead::{Aead, KeyInit},
        ChaCha20Poly1305, Nonce,
    };
    
    // Decode hex values
    let ciphertext = hex::decode(encrypted_hex)?;
    let nonce_bytes = hex::decode(nonce_hex)?;
    if nonce_bytes.len() != 12 {
        return Err(anyhow::anyhow!("Invalid nonce length"));
    }
    
    // Derive encryption key using Argon2
    let mut derived_key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt.as_bytes(), &mut derived_key)
        .map_err(|e| anyhow::anyhow!("Key derivation failed: {}", e))?;
    
    // Decrypt with ChaCha20-Poly1305
    let cipher = ChaCha20Poly1305::new_from_slice(&derived_key)
        .map_err(|e| anyhow::anyhow!("Cipher init failed: {}", e))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ciphertext.as_slice())
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

/// Get the SSH agent socket/pipe path for the current platform.
/// 
/// On Unix, this reads the SSH_AUTH_SOCK environment variable.
/// On Windows, this checks for SSH_AUTH_SOCK first, then falls back to the default
/// OpenSSH for Windows named pipe.
fn get_agent_path() -> anyhow::Result<String> {
    // First try SSH_AUTH_SOCK (works on both platforms)
    if let Ok(path) = std::env::var("SSH_AUTH_SOCK") {
        return Ok(path);
    }
    
    // On Windows, try the default OpenSSH pipe
    #[cfg(windows)]
    {
        // OpenSSH for Windows uses this named pipe by default
        let default_pipe = r"\\.\pipe\openssh-ssh-agent";
        return Ok(default_pipe.to_string());
    }
    
    #[cfg(not(windows))]
    {
        Err(anyhow::anyhow!("SSH_AUTH_SOCK not set - is ssh-agent running?"))
    }
}

/// SSH agent client wrapper for Ed25519 key operations.
/// 
/// Uses ssh-agent-lib to communicate with the SSH agent over Unix socket (Unix)
/// or named pipe (Windows).
pub struct SshAgentClient {
    /// The underlying SSH agent session.
    /// We use a boxed trait object to support both Unix and Windows stream types.
    client: Box<dyn ssh_agent_lib::agent::Session + Send + Sync>,
}

impl SshAgentClient {
    /// Connect to the SSH agent.
    /// 
    /// On Unix, uses the SSH_AUTH_SOCK environment variable.
    /// On Windows, uses SSH_AUTH_SOCK if set, otherwise falls back to the default
    /// OpenSSH for Windows named pipe.
    pub async fn connect() -> anyhow::Result<Self> {
        let agent_path = get_agent_path()?;
        
        tracing::debug!("Connecting to SSH agent at: {}", agent_path);
        
        // Parse the path into a service-binding Binding
        let binding = Self::parse_agent_binding(&agent_path)?;
        
        // Convert to Stream and connect
        let stream: service_binding::Stream = binding.try_into()
            .map_err(|e: std::io::Error| anyhow::anyhow!("Failed to connect to SSH agent: {}", e))?;
        
        let client = ssh_agent_lib::client::connect(stream)
            .map_err(|e| anyhow::anyhow!("Failed to create SSH agent client: {}", e))?;
        
        tracing::info!("Connected to SSH agent");
        Ok(Self { client })
    }
    
    /// Parse an agent path into a service-binding Binding.
    fn parse_agent_binding(path: &str) -> anyhow::Result<service_binding::Binding> {
        // Check if it's a Windows named pipe
        if path.starts_with(r"\\") {
            return Ok(service_binding::Binding::NamedPipe(path.into()));
        }
        
        // Check if it's a npipe:// URL
        if path.starts_with("npipe://") {
            return path.parse()
                .map_err(|e| anyhow::anyhow!("Failed to parse named pipe path: {:?}", e));
        }
        
        // Otherwise treat it as a Unix socket path
        #[cfg(unix)]
        {
            Ok(service_binding::Binding::FilePath(path.into()))
        }
        
        #[cfg(not(unix))]
        {
            Err(anyhow::anyhow!("Unix socket paths not supported on Windows: {}", path))
        }
    }
    
    /// Check if SSH agent is available.
    pub fn is_available() -> bool {
        get_agent_path().is_ok()
    }
    
    /// List Ed25519 keys from the SSH agent.
    pub async fn list_ed25519_keys(&mut self) -> anyhow::Result<Vec<KeyInfo>> {
        use ssh_key::public::KeyData;
        
        let identities = self.client.request_identities().await
            .map_err(|e| anyhow::anyhow!("Failed to request identities: {}", e))?;
        
        tracing::debug!("Found {} keys in SSH agent", identities.len());
        
        let mut ed25519_keys = Vec::new();
        
        for id in identities {
            // Check if it's an Ed25519 key
            if let KeyData::Ed25519(ed_key) = &id.pubkey {
                // Extract the 32-byte public key
                let public_key: [u8; 32] = ed_key.0;
                let key_info = KeyInfo::from_public_key(public_key, id.comment.clone());
                tracing::debug!("Found Ed25519 key: {} ({})", key_info.fingerprint, key_info.comment);
                ed25519_keys.push(key_info);
            }
        }
        
        tracing::info!("Found {} Ed25519 keys in SSH agent", ed25519_keys.len());
        Ok(ed25519_keys)
    }
    
    /// Sign data using a key identified by its fingerprint.
    pub async fn sign(&mut self, fingerprint: &str, data: &[u8]) -> anyhow::Result<[u8; 64]> {
        use ssh_agent_lib::proto::SignRequest;
        use ssh_key::public::KeyData;
        
        tracing::debug!("Signing {} bytes with key {}", data.len(), fingerprint);
        
        // Find the key by fingerprint
        let identities = self.client.request_identities().await
            .map_err(|e| anyhow::anyhow!("Failed to request identities: {}", e))?;
        
        let identity = identities.into_iter()
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
            flags: 0, // No special flags for Ed25519
        };
        
        let signature = self.client.sign(request).await
            .map_err(|e| anyhow::anyhow!("Signing failed: {}", e))?;
        
        // The ssh-key crate's Signature::as_bytes() returns the raw signature data.
        // For Ed25519, this is already the 64-byte raw signature.
        let sig_bytes = signature.as_bytes();
        
        tracing::debug!("Signature algorithm: {:?}, length: {}", signature.algorithm(), sig_bytes.len());
        
        if sig_bytes.len() != 64 {
            return Err(anyhow::anyhow!(
                "Invalid signature format: expected 64 bytes for Ed25519, got {}",
                sig_bytes.len()
            ));
        }
        
        let mut raw_sig = [0u8; 64];
        raw_sig.copy_from_slice(sig_bytes);
        
        tracing::debug!("Successfully signed data");
        Ok(raw_sig)
    }
    
    /// Add a new Ed25519 key to the agent.
    pub async fn add_key(&mut self, signing_key: &SigningKey, comment: &str) -> anyhow::Result<()> {
        use ssh_agent_lib::proto::{AddIdentity, Credential};
        use ssh_key::private::{Ed25519Keypair, KeypairData};
        use ssh_key::public::Ed25519PublicKey;
        
        tracing::debug!("Adding key to SSH agent with comment: {}", comment);
        
        // Create ssh-key types
        let public_key_bytes = signing_key.verifying_key().to_bytes();
        let private_key_bytes = signing_key.to_bytes();
        
        let ed25519_keypair = Ed25519Keypair {
            public: Ed25519PublicKey(public_key_bytes),
            private: ssh_key::private::Ed25519PrivateKey::from_bytes(&private_key_bytes),
        };
        
        let keypair_data = KeypairData::Ed25519(ed25519_keypair);
        
        let credential = Credential::Key { 
            privkey: keypair_data, 
            comment: comment.to_string(),
        };
        
        let identity = AddIdentity { credential };
        
        self.client.add_identity(identity).await
            .map_err(|e| anyhow::anyhow!("Failed to add key to agent: {}", e))?;
        
        tracing::info!("Successfully added key to SSH agent");
        Ok(())
    }
}

/// Async result type for SSH agent operations (for future enhanced UI).
#[allow(dead_code)]
#[derive(Debug)]
pub enum AgentResult {
    /// Successfully connected to agent.
    Connected,
    /// List of available keys.
    Keys(Vec<KeyInfo>),
    /// Key was added successfully.
    KeyAdded(KeyInfo),
    /// Error occurred.
    Error(String),
}

/// Pending async operation for the UI to poll.
pub enum PendingAgentOp {
    /// Connecting to agent.
    Connect(tokio::task::JoinHandle<anyhow::Result<Vec<KeyInfo>>>),
    /// Adding a key to agent.
    AddKey(tokio::task::JoinHandle<anyhow::Result<KeyInfo>>),
    /// Signing data (for future use).
    #[allow(dead_code)]
    Sign(tokio::task::JoinHandle<anyhow::Result<[u8; 64]>>),
}

/// Shared agent client for async operations (for future use).
#[allow(dead_code)]
pub type SharedAgentClient = Arc<TokioMutex<Option<SshAgentClient>>>;

/// Connect to SSH agent and list keys (async helper for UI).
pub async fn connect_and_list_keys() -> anyhow::Result<Vec<KeyInfo>> {
    let mut client = SshAgentClient::connect().await?;
    client.list_ed25519_keys().await
}

/// Generate a key and add it to the SSH agent (async helper for UI).
pub async fn generate_and_add_to_agent(comment: String) -> anyhow::Result<KeyInfo> {
    let mut client = SshAgentClient::connect().await?;
    
    // Generate a new keypair
    let signing_key = SigningKey::generate(&mut OsRng);
    let public_key = signing_key.verifying_key().to_bytes();
    
    // Add to agent
    client.add_key(&signing_key, &comment).await?;
    
    Ok(KeyInfo::from_public_key(public_key, comment))
}

/// Create an async signer using the SSH agent.
/// 
/// Returns a closure that can be called to sign data using the SSH agent.
pub fn create_agent_signer(fingerprint: String) -> backend::SigningCallback {
    Arc::new(move |payload: &[u8]| {
        // We need to do blocking I/O here since SigningCallback is sync.
        // This is a limitation - ideally we'd use async all the way through.
        let fingerprint = fingerprint.clone();
        let payload = payload.to_vec();
        
        // Spawn a dedicated thread to run the async operation.
        // This avoids issues with nested runtimes - we can't create a runtime
        // from within a runtime, but we can spawn a thread and create one there.
        let (tx, rx) = std::sync::mpsc::channel();
        
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("Failed to create runtime: {}", e))
                .and_then(|rt| {
                    rt.block_on(async {
                        let mut client = SshAgentClient::connect().await
                            .map_err(|e| format!("Failed to connect to agent: {}", e))?;
                        
                        client.sign(&fingerprint, &payload).await
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
    fn test_compute_fingerprint() {
        let key = [0u8; 32];
        let fp = compute_fingerprint(&key);
        assert!(fp.starts_with("SHA256:"));
    }
    
    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = [42u8; 32];
        let password = "test_password";
        
        let (encrypted, salt, nonce) = encrypt_key(&key, password).unwrap();
        let decrypted = decrypt_key(&encrypted, &salt, &nonce, password).unwrap();
        
        assert_eq!(key, decrypted);
    }
    
    #[test]
    fn test_decrypt_wrong_password() {
        let key = [42u8; 32];
        let password = "correct_password";
        let wrong_password = "wrong_password";
        
        let (encrypted, salt, nonce) = encrypt_key(&key, password).unwrap();
        let result = decrypt_key(&encrypted, &salt, &nonce, wrong_password);
        
        assert!(result.is_err());
    }
    
    #[test]
    fn test_parse_signing_key() {
        let key = SigningKey::generate(&mut OsRng);
        let hex = hex::encode(key.to_bytes());
        
        let parsed = parse_signing_key(&hex).unwrap();
        assert_eq!(key.to_bytes(), parsed.to_bytes());
    }
}
