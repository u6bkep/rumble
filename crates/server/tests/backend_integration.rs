//! Integration tests for server functionality.
//!
//! These tests spin up a server instance and use a minimal test client to test
//! room management, state synchronization, and multi-client interactions.

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU16, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use api::{
    build_auth_payload, compute_cert_hash, encode_frame, try_decode_frame, room_id_from_uuid, uuid_from_room_id,
    proto::{self, envelope::Payload, Envelope, RoomInfo, User},
};
use bytes::BytesMut;
use ed25519_dalek::{SigningKey, Signer};
use prost::Message;
use quinn::{crypto::rustls::QuicClientConfig, Endpoint};
use rand::rngs::OsRng;
use rustls_pemfile;
use std::sync::Arc;
use tempfile::TempDir;

/// Guard that kills the server process on drop and cleans up temp dir.
struct ServerGuard {
    child: Option<Child>,
    _temp_dir: TempDir,
    pub cert_path: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Atomic counter for unique test ports to avoid collisions between parallel tests.
static PORT_COUNTER: AtomicU16 = AtomicU16::new(56000);

fn next_test_port() -> u16 {
    PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Start a server instance on the given port and return the guard.
fn start_server(port: u16) -> ServerGuard {
    start_server_with_options(port, None, None)
}

/// Start a server instance with optional data directory override and password.
/// Always creates an isolated temp directory for certs and defaults data.
fn start_server_with_options(port: u16, data_dir_override: Option<&str>, password: Option<&str>) -> ServerGuard {
    // Create unique temp directory for this server instance
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let cert_dir = temp_dir.path().join("certs");
    let default_data_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&cert_dir).expect("failed to create cert dir");
    std::fs::create_dir_all(&default_data_dir).expect("failed to create data dir");

    let data_dir = data_dir_override.unwrap_or_else(|| default_data_dir.to_str().unwrap());

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_server"));
    cmd.env("RUST_LOG", "debug")
        .env("RUMBLE_NO_CONFIG", "1")
        .env("RUMBLE_PORT", port.to_string())
        .env("RUMBLE_CERT_DIR", cert_dir.to_str().unwrap())
        .env("RUMBLE_DATA_DIR", data_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    
    if let Some(pw) = password {
        cmd.env("RUMBLE_SERVER_PASSWORD", pw);
    }
    
    let mut child = cmd.spawn().expect("failed to start server binary");

    // Pipe server stdout/stderr to test output.
    if let Some(out) = child.stdout.take() {
        let port_copy = port;
        std::thread::spawn(move || {
            let reader = BufReader::new(out);
            for line in reader.lines().flatten() {
                println!("[server:{} stdout] {}", port_copy, line);
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let port_copy = port;
        std::thread::spawn(move || {
            let reader = BufReader::new(err);
            for line in reader.lines().flatten() {
                eprintln!("[server:{} stderr] {}", port_copy, line);
            }
        });
    }

    // Wait for certs to be generated
    let cert_path = cert_dir.join("fullchain.pem");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !cert_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    ServerGuard {
        child: Some(child),
        _temp_dir: temp_dir,
        cert_path,
    }
}

/// Start a server with a temporary data directory for persistence testing.
/// Returns (port, server_guard, data_dir_guard).
/// The data_dir_guard must be kept alive to prevent cleanup during the test.
fn start_server_with_persistence() -> (u16, ServerGuard, tempfile::TempDir) {
    let data_temp_dir = tempfile::tempdir().expect("failed to create temp dir for data");
    let port = next_test_port();
    let server = start_server_with_options(port, Some(data_temp_dir.path().to_str().unwrap()), None);
    (port, server, data_temp_dir)
}

/// Load certificate(s) from a file, supporting both PEM and DER formats.
fn load_cert_file(cert_path: &std::path::Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let cert_bytes = std::fs::read(cert_path)?;
    
    if cert_bytes.starts_with(b"-----BEGIN") {
        // PEM format
        let mut reader = std::io::BufReader::new(cert_bytes.as_slice());
        let certs: Vec<rustls::pki_types::CertificateDer<'static>> = 
            rustls_pemfile::certs(&mut reader)
                .filter_map(|r| r.ok())
                .collect();
        if certs.is_empty() {
            anyhow::bail!("No certificates found in PEM file");
        }
        Ok(certs)
    } else {
        // DER format - single cert
        Ok(vec![rustls::pki_types::CertificateDer::from(cert_bytes)])
    }
}

/// Low-level connection that doesn't complete auth - for testing auth failure modes.
struct RawConnection {
    _endpoint: Endpoint,
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    buf: BytesMut,
    user_id: u64,
    nonce: Option<[u8; 32]>,
    cert_der: Vec<u8>,
}

impl RawConnection {
    /// Create a connection and send ClientHello, but don't send Authenticate yet.
    async fn connect_no_auth(addr: &str, name: &str, public_key: &[u8; 32], password: Option<&str>, cert_path: &std::path::Path) -> Result<Self> {
        let certs = load_cert_file(cert_path)?;
        let cert_der = certs[0].to_vec(); // Keep first cert for hash

        let mut roots = rustls::RootCertStore::empty();
        for cert in certs {
            roots.add(cert)?;
        }

        let rustls_config = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();

        let mut rustls_config = rustls_config;
        rustls_config.alpn_protocols = vec![b"rumble".to_vec()];

        let client_config = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(Arc::new(rustls_config))?,
        ));

        let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(client_config);

        let socket_addr: std::net::SocketAddr = addr.parse()?;
        let conn = endpoint.connect(socket_addr, "localhost")?.await?;
        let (send, recv) = conn.open_bi().await?;

        let mut raw = Self {
            _endpoint: endpoint,
            conn,
            send,
            recv,
            buf: BytesMut::new(),
            user_id: 0,
            nonce: None,
            cert_der,
        };

        // Send ClientHello
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::ClientHello(proto::ClientHello {
                username: name.to_string(),
                public_key: public_key.to_vec(),
                password: password.map(|s| s.to_string()),
            })),
        };
        let frame = encode_frame(&env);
        raw.send.write_all(&frame).await?;

        // Wait for ServerHello
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while raw.nonce.is_none() && std::time::Instant::now() < deadline {
            let mut chunk = [0u8; 4096];
            match tokio::time::timeout(Duration::from_millis(100), raw.recv.read(&mut chunk)).await {
                Ok(Ok(Some(n))) => {
                    raw.buf.extend_from_slice(&chunk[..n]);
                    while let Some(frame) = try_decode_frame(&mut raw.buf) {
                        if let Ok(env) = Envelope::decode(&*frame) {
                            if let Some(Payload::ServerHello(sh)) = env.payload {
                                raw.user_id = sh.user_id;
                                if sh.nonce.len() == 32 {
                                    let mut n = [0u8; 32];
                                    n.copy_from_slice(&sh.nonce);
                                    raw.nonce = Some(n);
                                }
                            }
                        }
                    }
                }
                Ok(Ok(None)) => anyhow::bail!("Connection closed"),
                Ok(Err(e)) => anyhow::bail!("Read error: {}", e),
                Err(_) => {} // timeout, continue
            }
        }

        if raw.nonce.is_none() {
            anyhow::bail!("Did not receive ServerHello with nonce");
        }

        Ok(raw)
    }

    /// Send a custom Authenticate message with provided signature and timestamp.
    async fn send_authenticate(&mut self, signature: &[u8], timestamp_ms: i64) -> Result<()> {
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::Authenticate(proto::Authenticate {
                signature: signature.to_vec(),
                timestamp_ms,
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// Wait for auth result (either ServerState or AuthFailed).
    async fn wait_for_auth_result(&mut self, timeout: Duration) -> Result<AuthResult> {
        let deadline = std::time::Instant::now() + timeout;

        while std::time::Instant::now() < deadline {
            let mut chunk = [0u8; 4096];
            match tokio::time::timeout(Duration::from_millis(100), self.recv.read(&mut chunk)).await {
                Ok(Ok(Some(n))) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    while let Some(frame) = try_decode_frame(&mut self.buf) {
                        if let Ok(env) = Envelope::decode(&*frame) {
                            match env.payload {
                                Some(Payload::AuthFailed(af)) => {
                                    return Ok(AuthResult::Failed(af.error));
                                }
                                Some(Payload::ServerEvent(se)) => {
                                    if let Some(proto::server_event::Kind::ServerState(_)) = se.kind {
                                        return Ok(AuthResult::Success);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Ok(Ok(None)) => anyhow::bail!("Connection closed"),
                Ok(Err(e)) => anyhow::bail!("Read error: {}", e),
                Err(_) => {} // timeout, continue
            }
        }

        anyhow::bail!("Timeout waiting for auth result")
    }

    fn close(&self) {
        self.conn.close(quinn::VarInt::from_u32(0), b"test done");
    }
}

enum AuthResult {
    Success,
    Failed(String),
}

// =============================================================================
// Test Client
// =============================================================================

/// Result of attempting to connect - can be success or auth failure.
enum ConnectResult {
    Success(TestClient),
    AuthFailed(String),
}

/// Result of the handshake phase.
enum HandshakeResult {
    Success,
    AuthFailed(String),
}

/// A minimal test client for integration tests.
struct TestClient {
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    buf: BytesMut,
    user_id: u64,
    rooms: Vec<RoomInfo>,
    users: Vec<User>,
    signing_key: SigningKey,
}

impl TestClient {
    /// Connect to a server and complete the Ed25519 authentication handshake.
    async fn connect(addr: &str, name: &str, cert_path: &std::path::Path) -> Result<Self> {
        // Generate a keypair for this test client
        let signing_key = SigningKey::generate(&mut OsRng);
        Self::connect_with_key(addr, name, signing_key, None, cert_path).await
    }
    
    /// Connect with a specific signing key (for testing with existing identity).
    async fn connect_with_key(addr: &str, name: &str, signing_key: SigningKey, password: Option<&str>, cert_path: &std::path::Path) -> Result<Self> {
        match Self::try_connect_with_key(addr, name, signing_key, password, cert_path).await? {
            ConnectResult::Success(client) => Ok(client),
            ConnectResult::AuthFailed(error) => Err(anyhow::anyhow!("Authentication failed: {}", error)),
        }
    }
    
    /// Attempt to connect, returning either success or auth failure reason.
    async fn try_connect(addr: &str, name: &str, cert_path: &std::path::Path) -> Result<ConnectResult> {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self::try_connect_with_key(addr, name, signing_key, None, cert_path).await
    }
    
    /// Attempt to connect with a specific key, returning either success or auth failure reason.
    async fn try_connect_with_key(addr: &str, name: &str, signing_key: SigningKey, password: Option<&str>, cert_path: &std::path::Path) -> Result<ConnectResult> {
        let public_key = signing_key.verifying_key().to_bytes();
        
        // Load certificate(s)
        let certs = load_cert_file(cert_path)?;
        let cert_der = certs[0].to_vec(); // Keep first cert for hash

        // Build TLS config
        let mut roots = rustls::RootCertStore::empty();
        for cert in certs {
            roots.add(cert)?;
        }

        let rustls_config = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();

        let mut rustls_config = rustls_config;
        rustls_config.alpn_protocols = vec![b"rumble".to_vec()];

        let client_config = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(Arc::new(rustls_config))?,
        ));

        // Create endpoint
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(client_config);

        // Connect
        let socket_addr: std::net::SocketAddr = addr.parse()?;
        let conn = endpoint.connect(socket_addr, "localhost")?.await?;

        // Open bidirectional stream
        let (send, recv) = conn.open_bi().await?;

        let mut client = Self {
            conn,
            send,
            recv,
            buf: BytesMut::new(),
            user_id: 0,
            rooms: Vec::new(),
            users: Vec::new(),
            signing_key,
        };

        // Perform Ed25519 authentication handshake
        client.send_client_hello(name, &public_key, password).await?;

        // Wait for ServerHello, sign the challenge, and wait for ServerState
        match client.wait_for_handshake(&cert_der).await? {
            HandshakeResult::Success => Ok(ConnectResult::Success(client)),
            HandshakeResult::AuthFailed(error) => Ok(ConnectResult::AuthFailed(error)),
        }
    }

    async fn send_client_hello(&mut self, name: &str, public_key: &[u8; 32], password: Option<&str>) -> Result<()> {
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::ClientHello(proto::ClientHello {
                username: name.to_string(),
                public_key: public_key.to_vec(),
                password: password.map(|s| s.to_string()),
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    async fn wait_for_handshake(&mut self, cert_der: &[u8]) -> Result<HandshakeResult> {
        let mut got_hello = false;
        let mut got_state = false;
        let mut nonce: Option<[u8; 32]> = None;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);

        while (!got_hello || !got_state) && std::time::Instant::now() < deadline {
            let mut chunk = [0u8; 4096];
            match tokio::time::timeout(Duration::from_millis(100), self.recv.read(&mut chunk)).await
            {
                Ok(Ok(Some(n))) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    while let Some(frame) = try_decode_frame(&mut self.buf) {
                        if let Ok(env) = Envelope::decode(&*frame) {
                            match env.payload {
                                Some(Payload::ServerHello(sh)) => {
                                    self.user_id = sh.user_id;
                                    if sh.nonce.len() == 32 {
                                        let mut n = [0u8; 32];
                                        n.copy_from_slice(&sh.nonce);
                                        nonce = Some(n);
                                    }
                                    got_hello = true;
                                    
                                    // Send Authenticate message
                                    if let Some(n) = nonce {
                                        self.send_authenticate(n, cert_der).await?;
                                    }
                                }
                                Some(Payload::ServerEvent(se)) => {
                                    if let Some(proto::server_event::Kind::ServerState(ss)) =
                                        se.kind
                                    {
                                        self.rooms = ss.rooms;
                                        self.users = ss.users;
                                        got_state = true;
                                    }
                                }
                                Some(Payload::AuthFailed(af)) => {
                                    return Ok(HandshakeResult::AuthFailed(af.error));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Ok(Ok(None)) | Ok(Err(_)) => {
                    // Connection closed or error - check if there's a queued AuthFailed message
                    // in our buffer before giving up
                    while let Some(frame) = try_decode_frame(&mut self.buf) {
                        if let Ok(env) = Envelope::decode(&*frame) {
                            if let Some(Payload::AuthFailed(af)) = env.payload {
                                return Ok(HandshakeResult::AuthFailed(af.error));
                            }
                        }
                    }
                    anyhow::bail!("Connection closed during handshake");
                }
                Err(_) => {
                    // Timeout, continue loop
                }
            }
        }

        if !got_hello || !got_state {
            anyhow::bail!("Handshake incomplete: got_hello={}, got_state={}", got_hello, got_state);
        }

        Ok(HandshakeResult::Success)
    }

    /// Send the Authenticate message with Ed25519 signature.
    async fn send_authenticate(&mut self, nonce: [u8; 32], cert_der: &[u8]) -> Result<()> {
        let public_key = self.signing_key.verifying_key().to_bytes();
        let server_cert_hash = compute_cert_hash(cert_der);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        
        let payload = build_auth_payload(
            &nonce,
            timestamp_ms,
            &public_key,
            self.user_id,
            &server_cert_hash,
        );
        
        let signature = self.signing_key.sign(&payload);
        
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::Authenticate(proto::Authenticate {
                signature: signature.to_bytes().to_vec(),
                timestamp_ms,
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// Send a JoinRoom message.
    async fn join_room(&mut self, room_uuid: uuid::Uuid) -> Result<()> {
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::JoinRoom(proto::JoinRoom {
                room_id: Some(room_id_from_uuid(room_uuid)),
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// Send a CreateRoom message.
    async fn create_room(&mut self, name: &str) -> Result<()> {
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::CreateRoom(proto::CreateRoom {
                name: name.to_string(),
                parent_id: None,
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// Send a DeleteRoom message.
    async fn delete_room(&mut self, room_uuid: uuid::Uuid) -> Result<()> {
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::DeleteRoom(proto::DeleteRoom {
                room_id: Some(room_id_from_uuid(room_uuid)),
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// Send a RenameRoom message.
    async fn rename_room(&mut self, room_uuid: uuid::Uuid, new_name: &str) -> Result<()> {
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::RenameRoom(proto::RenameRoom {
                room_id: Some(room_id_from_uuid(room_uuid)),
                new_name: new_name.to_string(),
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// Receive and process messages for a duration, updating internal state.
    async fn process_messages(&mut self, duration: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + duration;

        while std::time::Instant::now() < deadline {
            let mut chunk = [0u8; 4096];
            match tokio::time::timeout(Duration::from_millis(50), self.recv.read(&mut chunk)).await
            {
                Ok(Ok(Some(n))) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    while let Some(frame) = try_decode_frame(&mut self.buf) {
                        if let Ok(env) = Envelope::decode(&*frame) {
                            self.handle_envelope(env);
                        }
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(_)) => break,
                Err(_) => {} // timeout, continue
            }
        }
        Ok(())
    }

    fn handle_envelope(&mut self, env: Envelope) {
        if let Some(Payload::ServerEvent(se)) = env.payload {
            if let Some(kind) = se.kind {
                match kind {
                    proto::server_event::Kind::ServerState(ss) => {
                        self.rooms = ss.rooms;
                        self.users = ss.users;
                    }
                    proto::server_event::Kind::StateUpdate(su) => {
                        self.apply_state_update(su);
                    }
                    // Ignore other event types in tests
                    _ => {}
                }
            }
        }
    }

    fn apply_state_update(&mut self, update: proto::StateUpdate) {
        if let Some(u) = update.update {
            match u {
                proto::state_update::Update::RoomCreated(rc) => {
                    if let Some(room) = rc.room {
                        self.rooms.push(room);
                    }
                }
                proto::state_update::Update::RoomDeleted(rd) => {
                    if let Some(rid) = rd.room_id.and_then(|r| uuid_from_room_id(&r)) {
                        self.rooms.retain(|r| {
                            r.id.as_ref().and_then(uuid_from_room_id) != Some(rid)
                        });
                    }
                }
                proto::state_update::Update::RoomRenamed(rr) => {
                    if let Some(rid) = rr.room_id.and_then(|r| uuid_from_room_id(&r)) {
                        if let Some(room) = self.rooms.iter_mut().find(|r| {
                            r.id.as_ref().and_then(uuid_from_room_id) == Some(rid)
                        }) {
                            room.name = rr.new_name;
                        }
                    }
                }
                proto::state_update::Update::UserJoined(uj) => {
                    if let Some(user) = uj.user {
                        self.users.push(user);
                    }
                }
                proto::state_update::Update::UserLeft(ul) => {
                    if let Some(uid) = ul.user_id {
                        self.users
                            .retain(|u| u.user_id.as_ref().map(|id| id.value) != Some(uid.value));
                    }
                }
                proto::state_update::Update::UserMoved(um) => {
                    if let (Some(uid), Some(to_room)) = (um.user_id, um.to_room_id) {
                        if let Some(user) = self
                            .users
                            .iter_mut()
                            .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                        {
                            user.current_room = Some(to_room);
                        }
                    }
                }
                proto::state_update::Update::UserStatusChanged(usc) => {
                    if let Some(uid) = usc.user_id {
                        if let Some(user) = self
                            .users
                            .iter_mut()
                            .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(uid.value))
                        {
                            user.is_muted = usc.is_muted;
                            user.is_deafened = usc.is_deafened;
                        }
                    }
                }
                proto::state_update::Update::RoomMoved(rm) => {
                    if let Some(rid) = rm.room_id.and_then(|r| uuid_from_room_id(&r)) {
                        if let Some(room) = self.rooms.iter_mut().find(|r| {
                            r.id.as_ref().and_then(uuid_from_room_id) == Some(rid)
                        }) {
                            room.parent_id = rm.new_parent_id;
                        }
                    }
                }
            }
        }
    }

    /// Send a RegisterUser message for a target user.
    async fn register_user(&mut self, target_user_id: u64) -> Result<()> {
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::RegisterUser(proto::RegisterUser {
                user_id: Some(proto::UserId { value: target_user_id }),
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// Send an UnregisterUser message for a target user.
    async fn unregister_user(&mut self, target_user_id: u64) -> Result<()> {
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::UnregisterUser(proto::UnregisterUser {
                user_id: Some(proto::UserId { value: target_user_id }),
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// Wait for a CommandResult message.
    async fn wait_for_command_result(&mut self, timeout: Duration) -> Result<Option<proto::CommandResult>> {
        let deadline = std::time::Instant::now() + timeout;

        while std::time::Instant::now() < deadline {
            let mut chunk = [0u8; 4096];
            match tokio::time::timeout(Duration::from_millis(50), self.recv.read(&mut chunk)).await
            {
                Ok(Ok(Some(n))) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    while let Some(frame) = try_decode_frame(&mut self.buf) {
                        if let Ok(env) = Envelope::decode(&*frame) {
                            match env.payload {
                                Some(Payload::CommandResult(cr)) => {
                                    return Ok(Some(cr));
                                }
                                Some(Payload::ServerEvent(se)) => {
                                    // Also process server events to keep state updated
                                    if let Some(kind) = se.kind {
                                        match kind {
                                            proto::server_event::Kind::ServerState(ss) => {
                                                self.rooms = ss.rooms;
                                                self.users = ss.users;
                                            }
                                            proto::server_event::Kind::StateUpdate(su) => {
                                                self.apply_state_update(su);
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(_)) => break,
                Err(_) => {} // timeout, continue
            }
        }
        Ok(None)
    }

    /// Close the connection.
    fn close(&self) {
        self.conn.close(quinn::VarInt::from_u32(0), b"test done");
    }

    /// Get rooms.
    fn rooms(&self) -> &[RoomInfo] {
        &self.rooms
    }

    /// Get users.
    fn users(&self) -> &[User] {
        &self.users
    }

    /// Get user ID.
    fn user_id(&self) -> u64 {
        self.user_id
    }

    /// Get the signing key for this client.
    fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
}

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
async fn test_client_connects_to_server() {
    let port = next_test_port();
    let server = start_server(port);

    // Give the server time to bind.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect a client.
    let client = TestClient::connect(&format!("127.0.0.1:{}", port), "test-client", &server.cert_path)
        .await
        .expect("client should connect");

    // Should have at least the Root room.
    assert!(!client.rooms().is_empty(), "expected at least one room");
    assert!(
        client.rooms().iter().any(|r| r.name == "Root"),
        "expected Root room in {:?}",
        client.rooms()
    );
}

#[tokio::test]
async fn test_server_assigns_user_id() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = TestClient::connect(&format!("127.0.0.1:{}", port), "user-id-test", &server.cert_path)
        .await
        .expect("client should connect");

    // User ID should be > 0 (0 is never assigned by the server).
    assert!(client.user_id() > 0, "expected user_id > 0");
}

#[tokio::test]
async fn test_multiple_clients_get_unique_user_ids() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = TestClient::connect(&format!("127.0.0.1:{}", port), "client-1", &server.cert_path)
        .await
        .expect("client 1 should connect");

    let client2 = TestClient::connect(&format!("127.0.0.1:{}", port), "client-2", &server.cert_path)
        .await
        .expect("client 2 should connect");

    assert_ne!(
        client1.user_id(),
        client2.user_id(),
        "clients should have different user IDs"
    );
}

#[tokio::test]
async fn test_create_room() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "room-creator", &server.cert_path)
        .await
        .expect("client should connect");

    let initial_count = client.rooms().len();

    // Create a new room
    client
        .create_room("Test Room")
        .await
        .expect("create room should succeed");

    // Process messages to get the update
    client
        .process_messages(Duration::from_millis(500))
        .await
        .unwrap();

    assert_eq!(
        client.rooms().len(),
        initial_count + 1,
        "should have one more room"
    );
    assert!(
        client.rooms().iter().any(|r| r.name == "Test Room"),
        "should have the new room"
    );
}

#[tokio::test]
async fn test_delete_room() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "room-deleter", &server.cert_path)
        .await
        .expect("client should connect");

    // Create a room to delete
    client.create_room("Room To Delete").await.unwrap();
    client
        .process_messages(Duration::from_millis(500))
        .await
        .unwrap();

    let room_uuid = client
        .rooms()
        .iter()
        .find(|r| r.name == "Room To Delete")
        .and_then(|r| r.id.as_ref())
        .and_then(uuid_from_room_id)
        .expect("should find created room");

    let count_before = client.rooms().len();

    // Delete the room
    client.delete_room(room_uuid).await.unwrap();
    client
        .process_messages(Duration::from_millis(500))
        .await
        .unwrap();

    assert_eq!(
        client.rooms().len(),
        count_before - 1,
        "should have one less room"
    );
    assert!(
        !client.rooms().iter().any(|r| r.name == "Room To Delete"),
        "deleted room should be gone"
    );
}

#[tokio::test]
async fn test_rename_room() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "room-renamer", &server.cert_path)
        .await
        .expect("client should connect");

    // Create a room to rename
    client.create_room("Original Name").await.unwrap();
    client
        .process_messages(Duration::from_millis(500))
        .await
        .unwrap();

    let room_uuid = client
        .rooms()
        .iter()
        .find(|r| r.name == "Original Name")
        .and_then(|r| r.id.as_ref())
        .and_then(uuid_from_room_id)
        .expect("should find created room");

    // Rename the room
    client.rename_room(room_uuid, "New Name").await.unwrap();
    client
        .process_messages(Duration::from_millis(500))
        .await
        .unwrap();

    assert!(
        client.rooms().iter().any(|r| r.name == "New Name"),
        "room should be renamed"
    );
    assert!(
        !client.rooms().iter().any(|r| r.name == "Original Name"),
        "old name should be gone"
    );
}

#[tokio::test]
async fn test_user_appears_in_users_list() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = TestClient::connect(&format!("127.0.0.1:{}", port), "visible-user", &server.cert_path)
        .await
        .expect("client should connect");

    // Check that our user is in the users list
    assert!(
        client.users().iter().any(|u| {
            u.user_id.as_ref().map(|id| id.value) == Some(client.user_id())
                && u.username == "visible-user"
        }),
        "should see ourselves in users list"
    );
}

#[tokio::test]
async fn test_second_client_sees_first_client() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = TestClient::connect(&format!("127.0.0.1:{}", port), "first-client", &server.cert_path)
        .await
        .expect("client 1 should connect");

    let mut client2 = TestClient::connect(&format!("127.0.0.1:{}", port), "second-client", &server.cert_path)
        .await
        .expect("client 2 should connect");

    // Client 2 should see client 1 in the users list
    assert!(
        client2.users().iter().any(|u| {
            u.user_id.as_ref().map(|id| id.value) == Some(client1.user_id())
        }),
        "client 2 should see client 1 in users list"
    );

    // Client 1 receives updates too - process messages to see client 2
    // (In a real scenario we'd need to do this, but the initial state for client2
    // already includes both users from the server's perspective)
}

#[tokio::test]
async fn test_room_updates_broadcast_to_all_clients() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client1 = TestClient::connect(&format!("127.0.0.1:{}", port), "broadcaster", &server.cert_path)
        .await
        .expect("client 1 should connect");

    let mut client2 = TestClient::connect(&format!("127.0.0.1:{}", port), "listener", &server.cert_path)
        .await
        .expect("client 2 should connect");

    // Client 1 creates a room
    client1.create_room("Shared Room").await.unwrap();

    // Both clients process messages
    client1
        .process_messages(Duration::from_millis(500))
        .await
        .unwrap();
    client2
        .process_messages(Duration::from_millis(500))
        .await
        .unwrap();

    // Both should see the new room
    assert!(
        client1.rooms().iter().any(|r| r.name == "Shared Room"),
        "client 1 should see new room"
    );
    assert!(
        client2.rooms().iter().any(|r| r.name == "Shared Room"),
        "client 2 should see new room"
    );
}

#[tokio::test]
async fn test_client_disconnect_removes_user() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = TestClient::connect(&format!("127.0.0.1:{}", port), "will-disconnect", &server.cert_path)
        .await
        .expect("client 1 should connect");

    let mut client2 = TestClient::connect(&format!("127.0.0.1:{}", port), "observer", &server.cert_path)
        .await
        .expect("client 2 should connect");

    let user1_id = client1.user_id();

    // Client 2 should see client 1
    assert!(
        client2
            .users()
            .iter()
            .any(|u| u.user_id.as_ref().map(|id| id.value) == Some(user1_id)),
        "client 2 should see client 1"
    );

    // Client 1 disconnects
    client1.close();

    // Give server time to process disconnect
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Client 2 processes messages to get the UserLeft update
    client2
        .process_messages(Duration::from_millis(500))
        .await
        .unwrap();

    // Client 1 should no longer be in the users list
    assert!(
        !client2
            .users()
            .iter()
            .any(|u| u.user_id.as_ref().map(|id| id.value) == Some(user1_id)),
        "client 1 should be gone after disconnect"
    );
}

// =============================================================================
// Registration Tests
// =============================================================================

#[tokio::test]
async fn test_user_can_register_themselves() {
    let (port, server, _temp_dir) = start_server_with_persistence();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "register-test", &server.cert_path)
        .await
        .expect("client should connect");

    // Register self
    client.register_user(client.user_id()).await.unwrap();

    let result = client.wait_for_command_result(Duration::from_secs(2)).await.unwrap();
    assert!(result.is_some(), "should receive command result");
    let result = result.unwrap();
    assert!(result.success, "registration should succeed: {}", result.message);

    client.close();
}

#[tokio::test]
async fn test_registered_user_can_reconnect_with_same_key() {
    let (port, server, _temp_dir) = start_server_with_persistence();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // First connection: register
    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "persist-test", &server.cert_path)
        .await
        .expect("client should connect");

    let signing_key = client.signing_key().clone();

    client.register_user(client.user_id()).await.unwrap();
    let result = client.wait_for_command_result(Duration::from_secs(2)).await.unwrap();
    assert!(result.is_some() && result.unwrap().success, "registration should succeed");

    client.close();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Second connection: reconnect with same key - should work
    let client2 = TestClient::connect_with_key(
        &format!("127.0.0.1:{}", port),
        "persist-test",
        signing_key,
        None,
        &server.cert_path,
    )
    .await
    .expect("should reconnect with same key");

    assert!(client2.user_id() > 0, "should get a valid user id");
    client2.close();
}

#[tokio::test]
async fn test_registered_user_name_overridden_by_server() {
    let (port, server, _temp_dir) = start_server_with_persistence();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // First connection: register with name "registered-name"
    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "registered-name", &server.cert_path)
        .await
        .expect("client should connect");

    let signing_key = client.signing_key().clone();

    client.register_user(client.user_id()).await.unwrap();
    let result = client.wait_for_command_result(Duration::from_secs(2)).await.unwrap();
    assert!(result.is_some() && result.unwrap().success, "registration should succeed");

    client.close();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Second connection: provide a DIFFERENT name with the same key
    // The server should allow connection but use the registered name
    let client2 = TestClient::connect_with_key(
        &format!("127.0.0.1:{}", port),
        "attempted-different-name",
        signing_key,
        None,
        &server.cert_path,
    )
    .await
    .expect("connection should succeed");

    // Find our user in the users list
    let our_user = client2.users().iter().find(|u| {
        u.user_id.as_ref().map(|id| id.value) == Some(client2.user_id())
    });

    assert!(our_user.is_some(), "should find ourselves in users list");
    let our_user = our_user.unwrap();
    
    // The server should have overridden the name with the registered name
    assert_eq!(
        our_user.username, "registered-name",
        "server should use registered name, not the name provided in hello"
    );

    client2.close();
}

#[tokio::test]
async fn test_different_key_cannot_use_registered_username() {
    let (port, server, _temp_dir) = start_server_with_persistence();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // First connection: register with name "protected-name"
    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "protected-name", &server.cert_path)
        .await
        .expect("client should connect");

    client.register_user(client.user_id()).await.unwrap();
    let result = client.wait_for_command_result(Duration::from_secs(2)).await.unwrap();
    assert!(result.is_some() && result.unwrap().success, "registration should succeed");

    client.close();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Second connection: try to use the same name with a DIFFERENT key - should fail
    let result = TestClient::try_connect(&format!("127.0.0.1:{}", port), "protected-name", &server.cert_path)
        .await
        .expect("connection attempt should not error");

    match result {
        ConnectResult::AuthFailed(error) => {
            assert!(
                error.contains("registered") || error.contains("taken"),
                "error should mention username is taken: {}",
                error
            );
        }
        ConnectResult::Success(_) => {
            panic!("different key should NOT be able to use a registered username");
        }
    }
}

#[tokio::test]
async fn test_unregister_allows_username_reuse() {
    let (port, server, _temp_dir) = start_server_with_persistence();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // First connection: register and then unregister
    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "will-unregister", &server.cert_path)
        .await
        .expect("client should connect");

    // Register
    client.register_user(client.user_id()).await.unwrap();
    let result = client.wait_for_command_result(Duration::from_secs(2)).await.unwrap();
    assert!(result.is_some() && result.unwrap().success, "registration should succeed");

    // Unregister
    client.unregister_user(client.user_id()).await.unwrap();
    let result = client.wait_for_command_result(Duration::from_secs(2)).await.unwrap();
    assert!(result.is_some() && result.unwrap().success, "unregistration should succeed");

    client.close();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Second connection: different key should now be able to use the name
    let result = TestClient::try_connect(&format!("127.0.0.1:{}", port), "will-unregister", &server.cert_path)
        .await
        .expect("connection attempt should not error");

    match result {
        ConnectResult::Success(client2) => {
            assert!(client2.user_id() > 0, "should get a valid user id");
            client2.close();
        }
        ConnectResult::AuthFailed(error) => {
            panic!("should be able to use unregistered username, but got: {}", error);
        }
    }
}

// =============================================================================
// Auth Failure Mode Tests
// =============================================================================

#[tokio::test]
async fn test_auth_failure_invalid_signature() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let signing_key = SigningKey::generate(&mut OsRng);
    let public_key = signing_key.verifying_key().to_bytes();

    let mut raw = RawConnection::connect_no_auth(
        &format!("127.0.0.1:{}", port),
        "bad-sig-test",
        &public_key,
        None,
        &server.cert_path,
    )
    .await
    .expect("should establish connection");

    // Send garbage signature
    let bad_signature = vec![0u8; 64];
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    raw.send_authenticate(&bad_signature, timestamp_ms).await.unwrap();

    let result = raw.wait_for_auth_result(Duration::from_secs(2)).await.unwrap();
    match result {
        AuthResult::Failed(error) => {
            assert!(
                error.to_lowercase().contains("signature") || error.to_lowercase().contains("invalid"),
                "error should mention signature issue: {}",
                error
            );
        }
        AuthResult::Success => {
            panic!("invalid signature should be rejected");
        }
    }

    raw.close();
}

#[tokio::test]
async fn test_auth_failure_timestamp_out_of_range() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let signing_key = SigningKey::generate(&mut OsRng);
    let public_key = signing_key.verifying_key().to_bytes();

    let mut raw = RawConnection::connect_no_auth(
        &format!("127.0.0.1:{}", port),
        "old-timestamp-test",
        &public_key,
        None,
        &server.cert_path,
    )
    .await
    .expect("should establish connection");

    let nonce = raw.nonce.unwrap();
    let server_cert_hash = compute_cert_hash(&raw.cert_der);
    
    // Use a timestamp from 10 minutes ago (server allows ±5 minutes)
    let old_timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64 - 10 * 60 * 1000;

    let payload = build_auth_payload(
        &nonce,
        old_timestamp_ms,
        &public_key,
        raw.user_id,
        &server_cert_hash,
    );

    let signature = signing_key.sign(&payload);
    raw.send_authenticate(&signature.to_bytes(), old_timestamp_ms).await.unwrap();

    let result = raw.wait_for_auth_result(Duration::from_secs(2)).await.unwrap();
    match result {
        AuthResult::Failed(error) => {
            assert!(
                error.to_lowercase().contains("timestamp") || error.to_lowercase().contains("time") || error.to_lowercase().contains("expired"),
                "error should mention timestamp issue: {}",
                error
            );
        }
        AuthResult::Success => {
            panic!("old timestamp should be rejected");
        }
    }

    raw.close();
}

#[tokio::test]
async fn test_auth_failure_wrong_user_id_in_signature() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let signing_key = SigningKey::generate(&mut OsRng);
    let public_key = signing_key.verifying_key().to_bytes();

    let mut raw = RawConnection::connect_no_auth(
        &format!("127.0.0.1:{}", port),
        "wrong-userid-test",
        &public_key,
        None,
        &server.cert_path,
    )
    .await
    .expect("should establish connection");

    let nonce = raw.nonce.unwrap();
    let server_cert_hash = compute_cert_hash(&raw.cert_der);
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Sign with WRONG user_id (we use 999999 instead of the actual assigned user_id)
    let wrong_user_id = 999999u64;
    let payload = build_auth_payload(
        &nonce,
        timestamp_ms,
        &public_key,
        wrong_user_id,
        &server_cert_hash,
    );

    let signature = signing_key.sign(&payload);
    raw.send_authenticate(&signature.to_bytes(), timestamp_ms).await.unwrap();

    let result = raw.wait_for_auth_result(Duration::from_secs(2)).await.unwrap();
    match result {
        AuthResult::Failed(error) => {
            // The signature won't verify because the payload has wrong user_id
            assert!(
                error.to_lowercase().contains("signature") || error.to_lowercase().contains("invalid"),
                "error should mention signature/validation issue: {}",
                error
            );
        }
        AuthResult::Success => {
            panic!("wrong user_id in signature should be rejected");
        }
    }

    raw.close();
}

#[tokio::test]
async fn test_auth_success_with_valid_credentials() {
    let port = next_test_port();
    let server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let signing_key = SigningKey::generate(&mut OsRng);
    let public_key = signing_key.verifying_key().to_bytes();

    let mut raw = RawConnection::connect_no_auth(
        &format!("127.0.0.1:{}", port),
        "valid-auth-test",
        &public_key,
        None,
        &server.cert_path,
    )
    .await
    .expect("should establish connection");

    let nonce = raw.nonce.unwrap();
    let server_cert_hash = compute_cert_hash(&raw.cert_der);
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Create valid signature
    let payload = build_auth_payload(
        &nonce,
        timestamp_ms,
        &public_key,
        raw.user_id,
        &server_cert_hash,
    );

    let signature = signing_key.sign(&payload);
    raw.send_authenticate(&signature.to_bytes(), timestamp_ms).await.unwrap();

    let result = raw.wait_for_auth_result(Duration::from_secs(2)).await.unwrap();
    match result {
        AuthResult::Success => {
            // Expected!
        }
        AuthResult::Failed(error) => {
            panic!("valid credentials should be accepted, but got: {}", error);
        }
    }

    raw.close();
}

// =============================================================================
// User Last Room Persistence Tests
// =============================================================================

/// Test that a registered user's last room is saved when they join a room.
#[tokio::test]
async fn test_registered_user_last_room_persisted() {
    let (port, server, _temp_dir) = start_server_with_persistence();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create a client with a specific signing key
    let signing_key = SigningKey::generate(&mut OsRng);
    
    // First connection: connect and register the user
    let mut client = TestClient::connect_with_key(
        &format!("127.0.0.1:{}", port),
        "room-test-user",
        signing_key.clone(),
        None,
        &server.cert_path,
    )
    .await
    .expect("should connect");

    // Register self and wait for confirmation
    client.register_user(client.user_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create a room
    client.create_room("Test Room").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.process_messages(Duration::from_millis(300)).await.unwrap();
    
    // Find the created room
    let room_uuid = client.rooms.iter()
        .find(|r| r.name == "Test Room")
        .and_then(|r| r.id.as_ref().and_then(uuid_from_room_id))
        .expect("should find created room");

    // Join the room
    client.join_room(room_uuid).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    // Disconnect
    client.close();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Second connection: reconnect with the same key and verify we're in the same room
    let client2 = TestClient::connect_with_key(
        &format!("127.0.0.1:{}", port),
        "room-test-user",
        signing_key,
        None,
        &server.cert_path,
    )
    .await
    .expect("should reconnect");

    // Check that we're in the correct room
    let my_user = client2.users.iter()
        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(client2.user_id))
        .expect("should find self in user list");
    
    let current_room_uuid = my_user.current_room.as_ref()
        .and_then(uuid_from_room_id)
        .expect("should have a current room");
    
    assert_eq!(current_room_uuid, room_uuid, "registered user should be restored to last room");
    
    client2.close();
}

/// Test that an unregistered user always starts in the Root room.
#[tokio::test]
async fn test_unregistered_user_starts_in_root() {
    let (port, server, _temp_dir) = start_server_with_persistence();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create a client with a specific signing key (but don't register)
    let signing_key = SigningKey::generate(&mut OsRng);
    
    // First connection: connect, create and join a room, but don't register
    let mut client = TestClient::connect_with_key(
        &format!("127.0.0.1:{}", port),
        "unregistered-user",
        signing_key.clone(),
        None,
        &server.cert_path,
    )
    .await
    .expect("should connect");

    // Create a room
    client.create_room("Unregistered Test Room").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.process_messages(Duration::from_millis(300)).await.unwrap();
    
    // Find the created room
    let room_uuid = client.rooms.iter()
        .find(|r| r.name == "Unregistered Test Room")
        .and_then(|r| r.id.as_ref().and_then(uuid_from_room_id))
        .expect("should find created room");

    // Join the room (without registering)
    client.join_room(room_uuid).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    // Disconnect
    client.close();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Second connection: reconnect and verify we're in Root (not the last room)
    let client2 = TestClient::connect_with_key(
        &format!("127.0.0.1:{}", port),
        "unregistered-user",
        signing_key,
        None,
        &server.cert_path,
    )
    .await
    .expect("should reconnect");

    // Check that we're in the Root room
    let my_user = client2.users.iter()
        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(client2.user_id))
        .expect("should find self in user list");
    
    let current_room_uuid = my_user.current_room.as_ref()
        .and_then(uuid_from_room_id)
        .expect("should have a current room");
    
    assert_eq!(current_room_uuid, api::ROOT_ROOM_UUID, "unregistered user should start in Root room");
    
    client2.close();
}

/// Test that if a user's last room is deleted, they start in Root instead.
#[tokio::test]
async fn test_last_room_deleted_falls_back_to_root() {
    let (port, server, _temp_dir) = start_server_with_persistence();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create a registered user and join a room
    let signing_key = SigningKey::generate(&mut OsRng);
    
    let mut client = TestClient::connect_with_key(
        &format!("127.0.0.1:{}", port),
        "fallback-test-user",
        signing_key.clone(),
        None,
        &server.cert_path,
    )
    .await
    .expect("should connect");

    // Register self
    client.register_user(client.user_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create and join a room
    client.create_room("Will Be Deleted").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    client.process_messages(Duration::from_millis(300)).await.unwrap();
    
    let room_uuid = client.rooms.iter()
        .find(|r| r.name == "Will Be Deleted")
        .and_then(|r| r.id.as_ref().and_then(uuid_from_room_id))
        .expect("should find created room");

    client.join_room(room_uuid).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    // Now delete the room while still connected
    client.delete_room(room_uuid).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    // Disconnect
    client.close();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Reconnect - should be in Root since the last room was deleted
    let client2 = TestClient::connect_with_key(
        &format!("127.0.0.1:{}", port),
        "fallback-test-user",
        signing_key,
        None,
        &server.cert_path,
    )
    .await
    .expect("should reconnect");

    let my_user = client2.users.iter()
        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(client2.user_id))
        .expect("should find self in user list");
    
    let current_room_uuid = my_user.current_room.as_ref()
        .and_then(uuid_from_room_id)
        .expect("should have a current room");
    
    assert_eq!(current_room_uuid, api::ROOT_ROOM_UUID, "user should fall back to Root when last room is deleted");
    
    client2.close();
}

/// Test that persisted rooms survive server restart.
#[tokio::test]
async fn test_rooms_persist_across_restart() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let port = next_test_port();
    
    // Start server and create a room
    {
        let server = start_server_with_options(port, Some(temp_dir.path().to_str().unwrap()), None);
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "room-creator", &server.cert_path)
            .await
            .expect("should connect");
        
        client.create_room("Persistent Room").await.unwrap();
        client.process_messages(Duration::from_millis(500)).await.unwrap();
        
        // Verify room exists
        assert!(
            client.rooms.iter().any(|r| r.name == "Persistent Room"),
            "room should be created"
        );
        
        client.close();
        // Server drops here
    }
    
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Restart server on a different port (to avoid port conflicts)
    let port2 = next_test_port();
    {
        let server = start_server_with_options(port2, Some(temp_dir.path().to_str().unwrap()), None);
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        let mut client = TestClient::connect(&format!("127.0.0.1:{}", port2), "room-checker", &server.cert_path)
            .await
            .expect("should connect to restarted server");
        
        // Room should still exist
        assert!(
            client.rooms.iter().any(|r| r.name == "Persistent Room"),
            "room should persist across server restart"
        );
        
        client.close();
    }
}
