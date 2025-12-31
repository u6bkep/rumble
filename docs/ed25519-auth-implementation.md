# Ed25519 Authentication Implementation Plan

This document details the implementation of Ed25519-based user authentication for Rumble, including SSH agent integration for key storage.

---

## Implementation Status

| Phase | Description | Status |
|-------|-------------|--------|
| Phase 1 | Protocol Changes (api crate) | ✅ **Complete** |
| Phase 2 | Server Implementation | ✅ **Complete** |
| Phase 3 | Backend Implementation | ✅ **Complete** |
| Phase 4 | UI Key Manager (egui-test) | ✅ **Complete** |
| Phase 5 | Testing | ✅ **Complete** |

### Phase 1 Implementation Notes

Implemented in `crates/api/proto/api.proto`:
- ✅ `ClientHello` with `username`, `public_key`, and optional `password`
- ✅ `ServerHello` with `nonce`, `server_name`, and `user_id`
- ✅ `Authenticate` with `signature` and `timestamp_ms`
- ✅ `AuthFailed` with `error` message
- ✅ `RegisterUser`, `UnregisterUser`, `RegistrationResult` for user registration

Implemented in `crates/api/src/lib.rs`:
- ✅ `build_auth_payload()` - constructs 112-byte payload for signing
- ✅ `compute_cert_hash()` - SHA256 hash of server certificate
- ✅ Unit tests for payload construction and signature verification

### Phase 2 Implementation Notes

Implemented in `crates/server/src/handlers.rs`:
- ✅ `handle_client_hello()` - validates key, checks password, sends ServerHello with nonce
- ✅ `handle_authenticate()` - verifies Ed25519 signature, sends ServerState
- ✅ `handle_register_user()` and `handle_unregister_user()` for user registration
- ✅ All handlers check `authenticated` flag before processing messages

Implemented in `crates/server/src/state.rs`:
- ✅ `PendingAuth` struct for tracking authentication state
- ✅ `sessions: DashMap<u64, [u8; 32]>` for user_id → public_key mapping
- ✅ `server_cert_hash()` for certificate binding
- ✅ Session management methods

Implemented in `crates/server/src/persistence.rs`:
- ✅ `Persistence` struct with sled database
- ✅ `RegisteredUser` struct with username, roles, last_room
- ✅ Methods for known keys, registered users, username availability
- ✅ Unit tests for persistence operations

### Phase 3 Implementation Notes

Implemented in `crates/backend/src/events.rs`:
- ✅ `SigningCallback` type: `Arc<dyn Fn(&[u8]) -> Result<[u8; 64], String> + Send + Sync>`
- ✅ `Command::Connect` with `public_key`, `signer`, and optional `password`
- ✅ `Command::RegisterUser` and `Command::UnregisterUser`

Implemented in `crates/backend/src/handle.rs`:
- ✅ `connect_to_server()` with full Ed25519 handshake
- ✅ `wait_for_server_hello()` extracts nonce and user_id
- ✅ `wait_for_auth_result()` handles ServerState or AuthFailed
- ✅ `compute_server_cert_hash()` for certificate binding

### Phase 4 Implementation Notes

Implemented in `crates/egui-test/src/key_manager.rs`:
- ✅ `KeySource` enum with `LocalPlaintext`, `LocalEncrypted`, `SshAgent` variants  
- ✅ `KeyConfig` struct for persistence to `~/.config/rumble/Rumble/identity.json`
- ✅ `KeyManager` with config loading/saving, key generation, fingerprint calculation
- ✅ `SshAgentClient` with `connect()`, `list_ed25519_keys()`, `sign()`, `add_key()` methods
- ✅ SSH agent integration via `ssh-agent-lib` v0.5
- ✅ Local key encryption with Argon2 (KDF) + ChaCha20-Poly1305 (AEAD)
- ✅ `create_signer()` method that handles both local and SSH agent keys
- ✅ Unit tests: fingerprint, parse_signing_key, encryption roundtrip, wrong password

Implemented in `crates/egui-test/src/main.rs`:
- ✅ First-run detection and setup dialog UI
- ✅ SSH agent key selection flow
- ✅ Generate key and add to agent flow
- ✅ Generate local key (plaintext or encrypted) flow
- ✅ Identity display in Connection settings
- ✅ "Generate New Identity" button to re-run setup

---

## Overview

The authentication system provides:
- Ed25519 keypair-based user identity
- Challenge-response authentication bound to the TLS connection
- SSH agent integration for secure key storage (preferred)
- Local key storage as fallback (password-protected or plaintext)
- User registration for name reservation

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                              egui-test (UI)                              │
├─────────────────────────────────────────────────────────────────────────┤
│  KeyManager (new module)                                                 │
│  ├─ SSH Agent integration (ssh-agent-lib)                               │
│  ├─ Local key storage (~/.config/rumble/identity.json)                  │
│  └─ First-run setup dialog                                              │
│                                                                          │
│  Provides: public_key, SigningCallback → Backend.Connect                │
└──────────────────────────────────┬──────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                              backend                                     │
├─────────────────────────────────────────────────────────────────────────┤
│  Connect command now requires:                                           │
│  ├─ public_key: [u8; 32]                                                │
│  ├─ signer: SigningCallback (async, handles challenge signing)          │
│  └─ password: Option<String> (server password, not key password)        │
│                                                                          │
│  Handshake:                                                              │
│  1. Send ClientHello { username, public_key, password }                 │
│  2. Receive ServerHello { nonce, server_name, user_id }                 │
│  3. Compute signature payload (nonce, timestamp, pubkey, user_id, cert) │
│  4. Call signer(payload) → signature                                    │
│  5. Send Authenticate { signature, timestamp }                          │
│  6. Receive ServerState or AuthFailed                                   │
└─────────────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                              server                                      │
├─────────────────────────────────────────────────────────────────────────┤
│  Handshake handler:                                                      │
│  1. Receive ClientHello                                                  │
│  2. Check username availability / registration                          │
│  3. Check password (if unknown key)                                      │
│  4. Generate nonce, send ServerHello                                    │
│  5. Receive Authenticate                                                 │
│  6. Verify signature against public_key                                 │
│  7. Check timestamp (±5 minutes)                                        │
│  8. On success: send ServerState, remember key                          │
│  9. On failure: send AuthFailed, close connection                       │
│                                                                          │
│  Persistence (sled):                                                     │
│  ├─ registered_users: public_key → { username, roles, last_room }       │
│  └─ known_keys: Set<public_key> (for password bypass)                   │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## Phase 1: Protocol Changes (api crate)

### 1.1 Update api.proto

Add/modify these messages in `crates/api/proto/api.proto`:

```protobuf
// =============================================================================
// Authentication Messages
// =============================================================================

// Sent by client immediately after connecting.
message ClientHello {
  string username = 1;
  bytes public_key = 2;       // Ed25519 public key (32 bytes)
  optional string password = 3; // Server password (required for unknown keys)
}

// Sent by server after ClientHello validation.
message ServerHello {
  bytes nonce = 1;            // Random 32-byte challenge
  string server_name = 2;
  uint64 user_id = 3;         // Assigned user ID for this session
}

// Sent by client to prove key ownership.
message Authenticate {
  bytes signature = 1;        // Ed25519 signature (64 bytes)
  int64 timestamp_ms = 2;     // Unix timestamp in milliseconds
}

// Sent by server on authentication failure.
message AuthFailed {
  string error = 1;           // Human-readable error message
}

// =============================================================================
// Registration Messages
// =============================================================================

// Register a user (binds username to public key permanently).
message RegisterUser {
  uint64 user_id = 1;         // User to register (allows admin to register others)
}

// Unregister a user (unbinds username from public key).
message UnregisterUser {
  uint64 user_id = 1;
}

// Result of registration/unregistration.
message RegistrationResult {
  bool success = 1;
  optional string error = 2;
}
```

### 1.2 Update Envelope

Add new payload variants to the Envelope message:

```protobuf
message Envelope {
  bytes state_hash = 1;
  oneof payload {
    // Authentication (10-19)
    ClientHello client_hello = 10;
    ServerHello server_hello = 11;
    Authenticate authenticate = 12;
    AuthFailed auth_failed = 13;
    
    // ... existing messages ...
    
    // Registration (40-49)
    RegisterUser register_user = 40;
    UnregisterUser unregister_user = 41;
    RegistrationResult registration_result = 42;
  }
}
```

### 1.3 Add Auth Helpers to api/src/lib.rs

```rust
/// Compute the signature payload for authentication.
/// 
/// Layout (112 bytes total, all fixed-length):
/// - nonce: 32 bytes
/// - timestamp_ms: 8 bytes (big-endian)
/// - public_key: 32 bytes
/// - user_id: 8 bytes (big-endian)
/// - server_cert_hash: 32 bytes (SHA256 of server TLS certificate)
pub fn build_auth_payload(
    nonce: &[u8; 32],
    timestamp_ms: i64,
    public_key: &[u8; 32],
    user_id: u64,
    server_cert_hash: &[u8; 32],
) -> [u8; 112] {
    let mut payload = [0u8; 112];
    payload[0..32].copy_from_slice(nonce);
    payload[32..40].copy_from_slice(&timestamp_ms.to_be_bytes());
    payload[40..72].copy_from_slice(public_key);
    payload[72..80].copy_from_slice(&user_id.to_be_bytes());
    payload[80..112].copy_from_slice(server_cert_hash);
    payload
}

/// Compute SHA256 hash of a DER-encoded certificate.
pub fn compute_cert_hash(cert_der: &[u8]) -> [u8; 32] {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    hasher.finalize().into()
}
```

### 1.4 Dependencies

Add to `crates/api/Cargo.toml`:
```toml
sha2 = "0.10"
ed25519-dalek = { version = "2", features = ["rand_core"] }
```

---

## Phase 2: Server Implementation

### 2.1 Add Persistence Layer

Create `crates/server/src/persistence.rs`:

```rust
use sled::Db;
use std::collections::HashSet;

/// User registration data stored in the database.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegisteredUser {
    pub username: String,
    pub roles: Vec<String>,  // Future: for ACLs
    pub last_room: Option<[u8; 16]>,  // UUID bytes
}

/// Server persistence layer using sled.
pub struct Persistence {
    db: Db,
}

impl Persistence {
    pub fn open(path: &str) -> anyhow::Result<Self> { ... }
    
    /// Get registered user by public key.
    pub fn get_registered_user(&self, public_key: &[u8; 32]) -> Option<RegisteredUser> { ... }
    
    /// Register a user (bind username to public key).
    pub fn register_user(&self, public_key: &[u8; 32], user: RegisteredUser) -> anyhow::Result<()> { ... }
    
    /// Unregister a user.
    pub fn unregister_user(&self, public_key: &[u8; 32]) -> anyhow::Result<()> { ... }
    
    /// Check if username is taken by a different key.
    pub fn is_username_taken(&self, username: &str, public_key: &[u8; 32]) -> bool { ... }
    
    /// Check if a public key is known (has connected before).
    pub fn is_known_key(&self, public_key: &[u8; 32]) -> bool { ... }
    
    /// Mark a key as known.
    pub fn add_known_key(&self, public_key: &[u8; 32]) -> anyhow::Result<()> { ... }
}
```

### 2.2 Update ServerState

Add to `crates/server/src/state.rs`:

```rust
pub struct ServerState {
    // ... existing fields ...
    
    /// Persistence layer for registered users and known keys.
    persistence: Persistence,
    
    /// Active sessions: user_id → public_key (for looking up identity).
    sessions: DashMap<u64, [u8; 32]>,
    
    /// Pending auth: connection_id → (nonce, user_id, public_key).
    pending_auth: DashMap<usize, PendingAuth>,
}

struct PendingAuth {
    nonce: [u8; 32],
    user_id: u64,
    public_key: [u8; 32],
    timestamp: Instant,
}
```

### 2.3 Update ClientHandle

Add to `crates/server/src/state.rs`:

```rust
pub struct ClientHandle {
    // ... existing fields ...
    
    /// The client's Ed25519 public key (set after authentication).
    pub public_key: RwLock<Option<[u8; 32]>>,
    
    /// Whether authentication is complete.
    pub authenticated: AtomicBool,
}
```

### 2.4 Update Handshake Handler

Modify `crates/server/src/handlers.rs`:

```rust
/// Handle ClientHello - begin authentication handshake.
async fn handle_client_hello(
    ch: proto::ClientHello,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    conn: &quinn::Connection,
) -> Result<()> {
    // 1. Validate public key length
    if ch.public_key.len() != 32 {
        return send_auth_failed(&sender, "Invalid public key length").await;
    }
    let public_key: [u8; 32] = ch.public_key.try_into().unwrap();
    
    // 2. Check username availability
    if state.persistence.is_username_taken(&ch.username, &public_key) {
        return send_auth_failed(&sender, "Username already taken").await;
    }
    
    // 3. Check password for unknown keys
    let is_known = state.persistence.is_known_key(&public_key);
    if !is_known {
        let required_password = std::env::var("RUMBLE_SERVER_PASSWORD").ok();
        if let Some(required) = required_password {
            if !required.is_empty() {
                match &ch.password {
                    Some(pw) if pw == &required => { /* OK */ }
                    _ => return send_auth_failed(&sender, "Password required").await,
                }
            }
        }
    }
    
    // 4. Generate nonce and store pending auth
    let nonce: [u8; 32] = rand::random();
    state.pending_auth.insert(sender.user_id as usize, PendingAuth {
        nonce,
        user_id: sender.user_id,
        public_key,
        timestamp: Instant::now(),
    });
    
    // 5. Store username (may be overwritten by registered name later)
    sender.set_username(ch.username).await;
    *sender.public_key.write().await = Some(public_key);
    
    // 6. Send ServerHello
    let reply = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ServerHello(proto::ServerHello {
            nonce: nonce.to_vec(),
            server_name: "Rumble Server".to_string(),
            user_id: sender.user_id,
        })),
    };
    sender.send_frame(&encode_frame(&reply)).await?;
    
    Ok(())
}

/// Handle Authenticate - verify signature and complete handshake.
async fn handle_authenticate(
    auth: proto::Authenticate,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
    conn: &quinn::Connection,
) -> Result<()> {
    // 1. Get pending auth state
    let pending = match state.pending_auth.remove(&(sender.user_id as usize)) {
        Some((_, p)) => p,
        None => return send_auth_failed(&sender, "No pending authentication").await,
    };
    
    // 2. Check timestamp (±5 minutes)
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let diff_ms = (now_ms - auth.timestamp_ms).abs();
    if diff_ms > 5 * 60 * 1000 {
        return send_auth_failed(&sender, "Timestamp out of range").await;
    }
    
    // 3. Compute expected signature payload
    let cert_hash = compute_cert_hash_from_connection(conn);
    let payload = api::build_auth_payload(
        &pending.nonce,
        auth.timestamp_ms,
        &pending.public_key,
        pending.user_id,
        &cert_hash,
    );
    
    // 4. Verify signature
    let signature: [u8; 64] = auth.signature.try_into()
        .map_err(|_| anyhow::anyhow!("Invalid signature length"))?;
    
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&pending.public_key)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signature);
    
    if verifying_key.verify_strict(&payload, &sig).is_err() {
        return send_auth_failed(&sender, "Invalid signature").await;
    }
    
    // 5. Authentication successful
    sender.authenticated.store(true, Ordering::SeqCst);
    
    // 6. Mark key as known
    state.persistence.add_known_key(&pending.public_key)?;
    
    // 7. Check for registered username (overrides client-provided)
    if let Some(registered) = state.persistence.get_registered_user(&pending.public_key) {
        sender.set_username(registered.username).await;
    }
    
    // 8. Track session
    state.sessions.insert(sender.user_id, pending.public_key);
    
    // 9. Auto-join Root room
    state.set_user_room(sender.user_id, ROOT_ROOM_UUID).await;
    
    // 10. Send initial ServerState
    send_server_state_to_client(&sender, &state).await?;
    
    // 11. Broadcast user joined
    broadcast_state_update(
        &state,
        proto::state_update::Update::UserJoined(proto::UserJoined {
            user: Some(proto::User {
                user_id: Some(proto::UserId { value: sender.user_id }),
                username: sender.username().await,
                current_room: Some(room_id_from_uuid(ROOT_ROOM_UUID)),
                is_muted: false,
                is_deafened: false,
            }),
        }),
    ).await?;
    
    Ok(())
}

/// Get the SHA256 hash of the server's TLS certificate from the connection.
fn compute_cert_hash_from_connection(conn: &quinn::Connection) -> [u8; 32] {
    // The server knows its own certificate; we can get it from config
    // For now, use a placeholder - actual implementation depends on how
    // we store the server cert at startup
    todo!("Get server cert DER and hash it")
}

async fn send_auth_failed(sender: &ClientHandle, error: &str) -> Result<()> {
    let reply = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::AuthFailed(proto::AuthFailed {
            error: error.to_string(),
        })),
    };
    sender.send_frame(&encode_frame(&reply)).await?;
    // Close connection after auth failure
    sender.close();
    Ok(())
}
```

### 2.5 Add Registration Handlers

```rust
/// Handle RegisterUser request.
async fn handle_register_user(
    req: proto::RegisterUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    // Get the target user's public key from active sessions
    let target_key = match state.sessions.get(&req.user_id) {
        Some(key) => *key,
        None => {
            return send_registration_result(&sender, false, "User not found").await;
        }
    };
    
    // Get the target user's current username
    let username = state.get_username(req.user_id).await
        .ok_or_else(|| anyhow::anyhow!("User not found"))?;
    
    // Check if already registered
    if state.persistence.get_registered_user(&target_key).is_some() {
        return send_registration_result(&sender, false, "User already registered").await;
    }
    
    // Register
    state.persistence.register_user(&target_key, RegisteredUser {
        username,
        roles: vec![],
        last_room: None,
    })?;
    
    send_registration_result(&sender, true, "").await
}

/// Handle UnregisterUser request.
async fn handle_unregister_user(
    req: proto::UnregisterUser,
    sender: Arc<ClientHandle>,
    state: Arc<ServerState>,
) -> Result<()> {
    // Get the target user's public key
    let target_key = match state.sessions.get(&req.user_id) {
        Some(key) => *key,
        None => {
            return send_registration_result(&sender, false, "User not found").await;
        }
    };
    
    // Unregister
    state.persistence.unregister_user(&target_key)?;
    
    send_registration_result(&sender, true, "").await
}
```

### 2.6 Dependencies

Add to `crates/server/Cargo.toml`:
```toml
sled = "0.34"
ed25519-dalek = { version = "2", features = ["rand_core"] }
rand = "0.8"
sha2 = "0.10"
serde = { version = "1.0", features = ["derive"] }
bincode = "1.3"  # For sled serialization
```

---

## Phase 3: Backend Implementation

### 3.1 Update Command Enum

Modify `crates/backend/src/events.rs`:

```rust
/// Callback for signing authentication challenges.
/// 
/// The UI provides this callback. It may:
/// - Sign using a local Ed25519 private key
/// - Sign using the SSH agent
/// 
/// Takes the payload to sign, returns the 64-byte signature or an error.
pub type SigningCallback = Arc<dyn Fn(&[u8]) -> Result<[u8; 64], String> + Send + Sync>;

pub enum Command {
    // Connection (signer is required; UI handles key storage/retrieval)
    Connect {
        addr: String,
        name: String,
        public_key: [u8; 32],      // Ed25519 public key
        signer: SigningCallback,   // Signs auth challenge
        password: Option<String>,  // Server password (for unknown keys)
    },
    
    // ... existing commands ...
    
    // Registration
    RegisterUser { user_id: u64 },
    UnregisterUser { user_id: u64 },
}
```

### 3.2 Update Connection Handshake

Modify `crates/backend/src/handle.rs`:

```rust
async fn connect_to_server(
    addr: &str,
    client_name: &str,
    public_key: [u8; 32],
    signer: SigningCallback,
    password: Option<&str>,
    config: &ConnectConfig,
) -> anyhow::Result<(
    quinn::Connection,
    quinn::SendStream,
    quinn::RecvStream,
    BytesMut,
    u64,
    Vec<proto::RoomInfo>,
    Vec<proto::User>,
)> {
    // ... existing connection setup ...
    
    // 1. Send ClientHello
    let hello = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::ClientHello(proto::ClientHello {
            username: client_name.to_string(),
            public_key: public_key.to_vec(),
            password: password.map(|s| s.to_string()),
        })),
    };
    send.write_all(&encode_frame(&hello)).await?;
    
    // 2. Wait for ServerHello
    let (nonce, server_name, user_id) = wait_for_server_hello(&mut recv, &mut buf).await?;
    
    // 3. Compute signature payload
    let server_cert_hash = compute_server_cert_hash(&conn);
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    
    let payload = api::build_auth_payload(
        &nonce,
        timestamp_ms,
        &public_key,
        user_id,
        &server_cert_hash,
    );
    
    // 4. Sign (may call SSH agent)
    let signature = signer(&payload)
        .map_err(|e| anyhow::anyhow!("Signing failed: {}", e))?;
    
    // 5. Send Authenticate
    let auth = proto::Envelope {
        state_hash: Vec::new(),
        payload: Some(Payload::Authenticate(proto::Authenticate {
            signature: signature.to_vec(),
            timestamp_ms,
        })),
    };
    send.write_all(&encode_frame(&auth)).await?;
    
    // 6. Wait for ServerState or AuthFailed
    let (rooms, users) = wait_for_auth_result(&mut recv, &mut buf).await?;
    
    Ok((conn, send, recv, buf, user_id, rooms, users))
}

fn compute_server_cert_hash(conn: &quinn::Connection) -> [u8; 32] {
    // Get peer certificates from the connection
    let peer_certs = conn.peer_identity()
        .and_then(|id| id.downcast::<Vec<rustls::pki_types::CertificateDer>>().ok())
        .unwrap_or_default();
    
    if let Some(cert) = peer_certs.first() {
        api::compute_cert_hash(cert.as_ref())
    } else {
        // Fallback: should not happen with proper TLS
        [0u8; 32]
    }
}
```

### 3.3 Dependencies

Add to `crates/backend/Cargo.toml`:
```toml
sha2 = "0.10"
```

---

## Phase 4: UI Key Manager (egui-test) ✅ COMPLETE

### 4.1 Create Key Manager Module ✅

Create `crates/egui-test/src/key_manager.rs`:

```rust
//! Key management for Ed25519 authentication.
//!
//! Handles:
//! - SSH agent integration (preferred)
//! - Local key storage (fallback)
//! - First-run setup dialog

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// How the key is stored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum KeySource {
    /// Key is stored in SSH agent; we remember fingerprint.
    Agent { fingerprint: String },
    /// Key is stored locally in config.
    Local { 
        /// Password-encrypted private key (if password set).
        encrypted_key: Option<Vec<u8>>,
        /// Plaintext private key (if no password).
        plaintext_key: Option<[u8; 32]>,
    },
}

/// Persistent key configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KeyConfig {
    pub source: KeySource,
    pub public_key: [u8; 32],
}

/// Key info for display (from agent or local).
#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub fingerprint: String,
    pub comment: String,
    pub public_key: [u8; 32],
}

/// Key manager state.
pub struct KeyManager {
    config_path: PathBuf,
    config: Option<KeyConfig>,
    agent: Option<SshAgentClient>,
    runtime: tokio::runtime::Handle,
}

impl KeyManager {
    /// Create a new key manager.
    pub fn new(config_dir: PathBuf) -> Self {
        let config_path = config_dir.join("identity.json");
        let config = Self::load_config(&config_path);
        
        Self {
            config_path,
            config,
            agent: None,
            runtime: tokio::runtime::Handle::current(),
        }
    }
    
    /// Check if first-run setup is needed.
    pub fn needs_setup(&self) -> bool {
        self.config.is_none()
    }
    
    /// Get the public key (if configured).
    pub fn public_key(&self) -> Option<[u8; 32]> {
        self.config.as_ref().map(|c| c.public_key)
    }
    
    /// Create a signing callback for the backend.
    /// 
    /// Returns None if key is not available (e.g., agent key not loaded).
    pub fn create_signer(&self) -> Option<backend::SigningCallback> {
        let config = self.config.clone()?;
        
        match config.source {
            KeySource::Agent { fingerprint } => {
                // Create signer that calls SSH agent
                let agent = self.agent.clone()?;
                Some(Arc::new(move |payload: &[u8]| {
                    // Block on async agent call
                    // Note: In production, might want to handle this differently
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(async {
                        agent.sign(&fingerprint, payload).await
                    })
                }))
            }
            KeySource::Local { plaintext_key: Some(key), .. } => {
                // Create signer using local key
                let signing_key = ed25519_dalek::SigningKey::from_bytes(&key);
                Some(Arc::new(move |payload: &[u8]| {
                    use ed25519_dalek::Signer;
                    let sig = signing_key.sign(payload);
                    Ok(sig.to_bytes())
                }))
            }
            KeySource::Local { encrypted_key: Some(_), .. } => {
                // Would need password prompt - return None for now
                // UI should detect this and prompt for password
                None
            }
            _ => None,
        }
    }
    
    // === SSH Agent Integration ===
    
    /// Connect to SSH agent.
    pub async fn connect_agent(&mut self) -> anyhow::Result<()> {
        self.agent = Some(SshAgentClient::connect().await?);
        Ok(())
    }
    
    /// List keys from SSH agent.
    pub async fn list_agent_keys(&mut self) -> anyhow::Result<Vec<KeyInfo>> {
        let agent = self.agent.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Agent not connected"))?;
        agent.list_keys().await
    }
    
    /// Select an existing key from the agent.
    pub fn select_agent_key(&mut self, key: &KeyInfo) -> anyhow::Result<()> {
        self.config = Some(KeyConfig {
            source: KeySource::Agent { fingerprint: key.fingerprint.clone() },
            public_key: key.public_key,
        });
        self.save_config()?;
        Ok(())
    }
    
    /// Generate a new key and add to agent.
    pub async fn generate_agent_key(&mut self, comment: &str) -> anyhow::Result<KeyInfo> {
        let agent = self.agent.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Agent not connected"))?;
        
        // Generate keypair
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let public_key = signing_key.verifying_key().to_bytes();
        
        // Add to agent
        agent.add_key(&signing_key, comment).await?;
        
        let fingerprint = compute_fingerprint(&public_key);
        let key_info = KeyInfo {
            fingerprint: fingerprint.clone(),
            comment: comment.to_string(),
            public_key,
        };
        
        // Save config
        self.config = Some(KeyConfig {
            source: KeySource::Agent { fingerprint },
            public_key,
        });
        self.save_config()?;
        
        Ok(key_info)
    }
    
    // === Local Key Storage ===
    
    /// Generate a new local key.
    pub fn generate_local_key(&mut self, password: Option<&str>) -> anyhow::Result<KeyInfo> {
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let public_key = signing_key.verifying_key().to_bytes();
        let private_key = signing_key.to_bytes();
        
        let source = if let Some(pw) = password {
            // Encrypt with password
            let encrypted = encrypt_key(&private_key, pw)?;
            KeySource::Local { encrypted_key: Some(encrypted), plaintext_key: None }
        } else {
            // Store plaintext
            KeySource::Local { encrypted_key: None, plaintext_key: Some(private_key) }
        };
        
        self.config = Some(KeyConfig { source, public_key });
        self.save_config()?;
        
        Ok(KeyInfo {
            fingerprint: compute_fingerprint(&public_key),
            comment: "rumble-local".to_string(),
            public_key,
        })
    }
    
    /// Unlock a password-protected local key.
    pub fn unlock_local_key(&mut self, password: &str) -> anyhow::Result<()> {
        let config = self.config.as_mut()
            .ok_or_else(|| anyhow::anyhow!("No config"))?;
        
        if let KeySource::Local { encrypted_key: Some(enc), plaintext_key } = &mut config.source {
            let decrypted = decrypt_key(enc, password)?;
            *plaintext_key = Some(decrypted);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Key is not encrypted"))
        }
    }
    
    // === Helpers ===
    
    fn load_config(path: &PathBuf) -> Option<KeyConfig> {
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }
    
    fn save_config(&self) -> anyhow::Result<()> {
        if let Some(config) = &self.config {
            let data = serde_json::to_string_pretty(config)?;
            std::fs::write(&self.config_path, data)?;
        }
        Ok(())
    }
}

/// SSH agent client wrapper.
struct SshAgentClient {
    client: ssh_agent_lib::client::Client<tokio::net::UnixStream>,
}

impl SshAgentClient {
    async fn connect() -> anyhow::Result<Self> {
        let socket_path = std::env::var("SSH_AUTH_SOCK")?;
        let stream = tokio::net::UnixStream::connect(&socket_path).await?;
        let client = ssh_agent_lib::client::Client::new(stream);
        Ok(Self { client })
    }
    
    async fn list_keys(&mut self) -> anyhow::Result<Vec<KeyInfo>> {
        use ssh_agent_lib::agent::Session;
        
        let identities = self.client.request_identities().await?;
        
        identities.into_iter()
            .filter_map(|id| {
                // Parse public key, filter to Ed25519 only
                let key = ssh_key::PublicKey::from_bytes(&id.pubkey_blob).ok()?;
                if !matches!(key.algorithm(), ssh_key::Algorithm::Ed25519) {
                    return None;
                }
                
                let public_key: [u8; 32] = key.key_data()
                    .ed25519()?
                    .0
                    .try_into()
                    .ok()?;
                
                Some(KeyInfo {
                    fingerprint: key.fingerprint(ssh_key::HashAlg::Sha256).to_string(),
                    comment: id.comment,
                    public_key,
                })
            })
            .collect()
    }
    
    async fn sign(&mut self, fingerprint: &str, data: &[u8]) -> Result<[u8; 64], String> {
        use ssh_agent_lib::agent::Session;
        use ssh_agent_lib::proto::SignRequest;
        
        // Find the key by fingerprint
        let identities = self.client.request_identities().await
            .map_err(|e| e.to_string())?;
        
        let identity = identities.into_iter()
            .find(|id| {
                if let Ok(key) = ssh_key::PublicKey::from_bytes(&id.pubkey_blob) {
                    key.fingerprint(ssh_key::HashAlg::Sha256).to_string() == fingerprint
                } else {
                    false
                }
            })
            .ok_or_else(|| "Key not found in agent".to_string())?;
        
        let request = SignRequest {
            pubkey_blob: identity.pubkey_blob,
            data: data.to_vec(),
            flags: 0,
        };
        
        let signature = self.client.sign(request).await
            .map_err(|e| e.to_string())?;
        
        // Extract raw Ed25519 signature (skip SSH signature wrapper)
        let sig_bytes: [u8; 64] = signature.as_bytes()
            .try_into()
            .map_err(|_| "Invalid signature length")?;
        
        Ok(sig_bytes)
    }
    
    async fn add_key(&mut self, key: &ed25519_dalek::SigningKey, comment: &str) -> anyhow::Result<()> {
        use ssh_agent_lib::agent::Session;
        use ssh_agent_lib::proto::AddIdentity;
        
        // Convert to SSH key format
        let ssh_key = ssh_key::PrivateKey::from(key.clone());
        let encoded = ssh_key.to_bytes()?;
        
        let identity = AddIdentity {
            privkey: encoded.to_vec(),
            comment: comment.to_string(),
        };
        
        self.client.add_identity(identity).await?;
        Ok(())
    }
}

fn compute_fingerprint(public_key: &[u8; 32]) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    let hash = hasher.finalize();
    format!("SHA256:{}", base64::encode(&hash))
}

fn encrypt_key(key: &[u8; 32], password: &str) -> anyhow::Result<Vec<u8>> {
    // Use argon2 for key derivation + ChaCha20-Poly1305 for encryption
    // This is a simplified example - use a proper crypto library
    todo!("Implement key encryption")
}

fn decrypt_key(encrypted: &[u8], password: &str) -> anyhow::Result<[u8; 32]> {
    todo!("Implement key decryption")
}
```

### 4.2 First-Run Setup Dialog ✅

Add to `crates/egui-test/src/main.rs`:

```rust
/// First-run setup dialog for key configuration.
fn show_first_run_dialog(ctx: &egui::Context, key_manager: &mut KeyManager) -> Option<SetupResult> {
    let mut result = None;
    
    egui::Window::new("Welcome to Rumble")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.heading("Key Setup");
            ui.separator();
            
            ui.label("Rumble uses Ed25519 keys for authentication.");
            ui.label("Choose how to manage your key:");
            ui.add_space(16.0);
            
            // Option 1: Use existing key from SSH agent
            if ui.button("🔑 Use existing key from SSH agent").clicked() {
                result = Some(SetupResult::ShowAgentKeyList);
            }
            ui.label("  List keys from your SSH agent and select one.");
            
            ui.add_space(8.0);
            
            // Option 2: Generate new key in SSH agent
            if ui.button("✨ Generate new key in SSH agent").clicked() {
                result = Some(SetupResult::GenerateAgentKey);
            }
            ui.label("  Create a new Ed25519 key and add it to your agent.");
            
            ui.add_space(8.0);
            
            // Option 3: Generate local key
            if ui.button("💾 Generate key stored by Rumble").clicked() {
                result = Some(SetupResult::ShowLocalKeyOptions);
            }
            ui.label("  Create a key stored in Rumble's config directory.");
        });
    
    result
}

/// Show agent key selection dialog.
fn show_agent_key_list(
    ctx: &egui::Context, 
    keys: &[KeyInfo], 
    selected: &mut Option<usize>
) -> Option<AgentKeyAction> {
    let mut action = None;
    
    egui::Window::new("Select Key from SSH Agent")
        .collapsible(false)
        .resizable(true)
        .show(ctx, |ui| {
            if keys.is_empty() {
                ui.label("No Ed25519 keys found in SSH agent.");
                if ui.button("Generate new key").clicked() {
                    action = Some(AgentKeyAction::Generate);
                }
            } else {
                ui.label("Select a key:");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, key) in keys.iter().enumerate() {
                        let selected = *selected == Some(i);
                        if ui.selectable_label(selected, format!(
                            "{} ({})", 
                            key.fingerprint, 
                            key.comment
                        )).clicked() {
                            *selected = Some(i);
                        }
                    }
                });
                
                ui.separator();
                
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        action = Some(AgentKeyAction::Cancel);
                    }
                    if let Some(i) = selected {
                        if ui.button("Use this key").clicked() {
                            action = Some(AgentKeyAction::Select(*i));
                        }
                    }
                });
            }
        });
    
    action
}
```

### 4.3 Dependencies ✅

Add to `crates/egui-test/Cargo.toml`:
```toml
ssh-agent-lib = "0.5"
ssh-key = { version = "0.6", features = ["ed25519"] }
ed25519-dalek = { version = "2", features = ["rand_core"] }
sha2 = "0.10"
base64 = "0.21"
rand = "0.8"
# For key encryption (optional)
argon2 = "0.5"
chacha20poly1305 = "0.10"
```

---

## Phase 5: Testing

### 5.1 Unit Tests

Add to `crates/api/src/lib.rs`:
```rust
#[cfg(test)]
mod auth_tests {
    use super::*;
    
    #[test]
    fn test_build_auth_payload() {
        let nonce = [1u8; 32];
        let timestamp_ms = 1234567890123i64;
        let public_key = [2u8; 32];
        let user_id = 42u64;
        let cert_hash = [3u8; 32];
        
        let payload = build_auth_payload(&nonce, timestamp_ms, &public_key, user_id, &cert_hash);
        
        assert_eq!(payload.len(), 112);
        assert_eq!(&payload[0..32], &nonce);
        assert_eq!(&payload[32..40], &timestamp_ms.to_be_bytes());
        assert_eq!(&payload[40..72], &public_key);
        assert_eq!(&payload[72..80], &user_id.to_be_bytes());
        assert_eq!(&payload[80..112], &cert_hash);
    }
    
    #[test]
    fn test_signature_verification() {
        use ed25519_dalek::{SigningKey, Signer, Verifier};
        
        let signing_key = SigningKey::generate(&mut rand::thread_rng());
        let public_key = signing_key.verifying_key();
        
        let payload = build_auth_payload(
            &[0u8; 32],
            1234567890123,
            &public_key.to_bytes(),
            42,
            &[0u8; 32],
        );
        
        let signature = signing_key.sign(&payload);
        
        assert!(public_key.verify(&payload, &signature).is_ok());
    }
}
```

### 5.2 Integration Tests

Add to `crates/server/tests/auth_integration.rs`:
```rust
#[tokio::test]
async fn test_successful_auth() {
    // Start server
    // Generate client keypair
    // Connect and authenticate
    // Verify we receive ServerState
}

#[tokio::test]
async fn test_auth_wrong_password() {
    // Start server with password
    // Connect with wrong password
    // Verify we receive AuthFailed
}

#[tokio::test]
async fn test_auth_username_taken() {
    // Start server
    // Register user "alice" with key1
    // Try to connect as "alice" with key2
    // Verify we receive AuthFailed "Username already taken"
}

#[tokio::test]
async fn test_registered_username_used() {
    // Register user with name "alice"
    // Connect claiming name "bob" with same key
    // Verify we see "alice" in ServerState
}
```

---

## Migration Notes

### Breaking Changes

1. **ClientHello format changed**: Now includes `public_key`, `password` is optional
2. **ServerHello format changed**: Now includes `nonce` 
3. **New handshake step**: Client must send `Authenticate` after `ServerHello`
4. **Command::Connect changed**: Now requires `public_key` and `signer`

### Migration Path

1. Update api.proto and regenerate
2. Implement server auth (can run alongside old clients temporarily)
3. Update backend with new handshake
4. Add key manager to UI
5. Remove legacy password-only auth

---

## Security Considerations

1. **Nonce uniqueness**: Server generates cryptographically random 32-byte nonces
2. **Timestamp validation**: ±5 minute window prevents replay of old signatures
3. **Certificate binding**: Signature includes server cert hash, preventing MITM relay
4. **Key storage**: SSH agent is preferred; local keys can be password-protected
5. **No key transmission**: Private keys never leave the client; only signatures are sent
6. **Session binding**: Signature includes user_id assigned by server for this session

---

## Future Enhancements

1. **Key rotation**: Allow users to update their registered key
2. **Multiple keys per user**: Support linking multiple keys to one registration
3. **Hardware key support**: Smart cards, YubiKeys via agent or PKCS#11
4. **Key revocation**: Admin ability to ban specific public keys
5. **Proof of possession**: Add challenge for registered username claims
