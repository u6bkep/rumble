//! Integration tests for server functionality.
//!
//! These tests spin up a server instance and use a minimal test client to test
//! room management, state synchronization, and multi-client interactions.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

use anyhow::Result;
use api::{
    encode_frame, try_decode_frame, room_id_from_uuid, uuid_from_room_id,
    proto::{self, envelope::Payload, Envelope, RoomInfo, User},
};
use bytes::BytesMut;
use prost::Message;
use quinn::{crypto::rustls::QuicClientConfig, Endpoint};
use std::sync::Arc;

/// Guard that kills the server process on drop.
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
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
fn start_server(port: u16) -> ChildGuard {
    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .env("RUST_LOG", "debug")
        .env("RUMBLE_PORT", port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start server binary");

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

    ChildGuard(Some(child))
}

// =============================================================================
// Test Client
// =============================================================================

/// A minimal test client for integration tests.
struct TestClient {
    conn: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    buf: BytesMut,
    user_id: u64,
    rooms: Vec<RoomInfo>,
    users: Vec<User>,
}

impl TestClient {
    /// Connect to a server and complete the handshake.
    async fn connect(addr: &str, name: &str) -> Result<Self> {
        // Load dev certificate
        let cert_der = std::fs::read("dev-certs/server-cert.der")?;
        let cert = rustls::pki_types::CertificateDer::from(cert_der);

        // Build TLS config
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert)?;

        let rustls_config = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
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
        };

        // Send ClientHello
        client.send_client_hello(name).await?;

        // Wait for ServerHello and initial ServerState
        client.wait_for_handshake().await?;

        Ok(client)
    }

    async fn send_client_hello(&mut self, name: &str) -> Result<()> {
        let env = Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::ClientHello(proto::ClientHello {
                client_name: name.to_string(),
                password: String::new(),
            })),
        };
        let frame = encode_frame(&env);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    async fn wait_for_handshake(&mut self) -> Result<()> {
        let mut got_hello = false;
        let mut got_state = false;

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
                                    got_hello = true;
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
                                _ => {}
                            }
                        }
                    }
                }
                Ok(Ok(None)) => {
                    anyhow::bail!("Connection closed during handshake");
                }
                Ok(Err(e)) => {
                    anyhow::bail!("Read error during handshake: {}", e);
                }
                Err(_) => {
                    // Timeout, continue loop
                }
            }
        }

        if !got_hello || !got_state {
            anyhow::bail!("Handshake incomplete: got_hello={}, got_state={}", got_hello, got_state);
        }

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
            }
        }
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
}

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
async fn test_client_connects_to_server() {
    let port = next_test_port();
    let _server = start_server(port);

    // Give the server time to bind.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect a client.
    let client = TestClient::connect(&format!("127.0.0.1:{}", port), "test-client")
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
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = TestClient::connect(&format!("127.0.0.1:{}", port), "user-id-test")
        .await
        .expect("client should connect");

    // User ID should be > 0 (0 is never assigned by the server).
    assert!(client.user_id() > 0, "expected user_id > 0");
}

#[tokio::test]
async fn test_multiple_clients_get_unique_user_ids() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = TestClient::connect(&format!("127.0.0.1:{}", port), "client-1")
        .await
        .expect("client 1 should connect");

    let client2 = TestClient::connect(&format!("127.0.0.1:{}", port), "client-2")
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
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "room-creator")
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
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "room-deleter")
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
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = TestClient::connect(&format!("127.0.0.1:{}", port), "room-renamer")
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
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = TestClient::connect(&format!("127.0.0.1:{}", port), "visible-user")
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
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = TestClient::connect(&format!("127.0.0.1:{}", port), "first-client")
        .await
        .expect("client 1 should connect");

    let mut client2 = TestClient::connect(&format!("127.0.0.1:{}", port), "second-client")
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
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client1 = TestClient::connect(&format!("127.0.0.1:{}", port), "broadcaster")
        .await
        .expect("client 1 should connect");

    let mut client2 = TestClient::connect(&format!("127.0.0.1:{}", port), "listener")
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
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = TestClient::connect(&format!("127.0.0.1:{}", port), "will-disconnect")
        .await
        .expect("client 1 should connect");

    let mut client2 = TestClient::connect(&format!("127.0.0.1:{}", port), "observer")
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
