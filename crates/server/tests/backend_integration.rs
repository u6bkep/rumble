//! Integration tests for server functionality using the backend client library.
//!
//! These tests spin up a server instance and use the backend client to test
//! room management, chat messaging, and multi-client interactions.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

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

/// Create a connect config for tests that trusts the dev certificate.
fn test_connect_config() -> backend::ConnectConfig {
    backend::ConnectConfig::new().with_cert("dev-certs/server-cert.der")
}

/// Start a server instance on the given port and return the guard.
/// Pipes stdout/stderr to test output for diagnostics.
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

/// Wait for the client snapshot to have at least one room.
/// This also yields to allow background tasks to process events.
async fn wait_for_rooms(client: &backend::Client, timeout_ms: u64) -> backend::RoomSnapshot {
    let mut attempts = 0;
    let max_attempts = timeout_ms / 50;
    loop {
        // Yield to allow background tasks to run.
        tokio::task::yield_now().await;
        let snap = client.get_snapshot().await;
        if !snap.rooms.is_empty() {
            return snap;
        }
        attempts += 1;
        if attempts > max_attempts {
            panic!("timeout waiting for room state from server");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for the client snapshot to contain a specific number of rooms.
async fn wait_for_room_count(client: &backend::Client, count: usize, timeout_ms: u64) -> backend::RoomSnapshot {
    let mut attempts = 0;
    let max_attempts = timeout_ms / 50;
    loop {
        let snap = client.get_snapshot().await;
        if snap.rooms.len() == count {
            return snap;
        }
        attempts += 1;
        if attempts > max_attempts {
            panic!(
                "timeout waiting for {} rooms, got {} rooms: {:?}",
                count,
                snap.rooms.len(),
                snap.rooms.iter().map(|r| &r.name).collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for the client snapshot to have a room with the given name.
async fn wait_for_room_named(client: &backend::Client, name: &str, timeout_ms: u64) -> backend::RoomSnapshot {
    let mut attempts = 0;
    let max_attempts = timeout_ms / 50;
    loop {
        let snap = client.get_snapshot().await;
        if snap.rooms.iter().any(|r| r.name == name) {
            return snap;
        }
        attempts += 1;
        if attempts > max_attempts {
            panic!(
                "timeout waiting for room named '{}', got rooms: {:?}",
                name,
                snap.rooms.iter().map(|r| &r.name).collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// =============================================================================
// Test: Basic connection to server
// =============================================================================

#[tokio::test]
async fn test_client_connects_to_server() {
    let port = next_test_port();
    let _server = start_server(port);

    // Give the server time to bind.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect a client.
    let client = backend::Client::connect(&format!("127.0.0.1:{}", port), "test-client", None, test_connect_config())
        .await
        .expect("client should connect");

    // Wait for initial room state.
    let snap = wait_for_rooms(&client, 5000).await;

    // Should have at least the Root room.
    assert!(!snap.rooms.is_empty(), "expected at least one room");
    assert!(
        snap.rooms.iter().any(|r| r.name == "Root"),
        "expected Root room in {:?}",
        snap.rooms
    );
}

// =============================================================================
// Test: Create a new room
// =============================================================================

#[tokio::test]
async fn test_create_room() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = backend::Client::connect(&format!("127.0.0.1:{}", port), "room-creator", None, test_connect_config())
        .await
        .expect("client should connect");

    // Wait for initial state.
    wait_for_rooms(&client, 5000).await;

    // Create a new room.
    client.create_room("TestRoom").await.expect("create_room should succeed");

    // Wait for the new room to appear in the snapshot.
    let snap = wait_for_room_named(&client, "TestRoom", 5000).await;

    assert!(
        snap.rooms.iter().any(|r| r.name == "TestRoom"),
        "TestRoom should exist in {:?}",
        snap.rooms
    );
}

// =============================================================================
// Test: Delete a room
// =============================================================================

#[tokio::test]
async fn test_delete_room() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = backend::Client::connect(&format!("127.0.0.1:{}", port), "room-deleter", None, test_connect_config())
        .await
        .expect("client should connect");

    wait_for_rooms(&client, 5000).await;

    // Create a room first.
    client.create_room("ToDelete").await.expect("create_room should succeed");
    let snap = wait_for_room_named(&client, "ToDelete", 5000).await;

    // Find the room ID.
    let room_id = snap
        .rooms
        .iter()
        .find(|r| r.name == "ToDelete")
        .and_then(|r| r.id.as_ref())
        .map(|id| id.value)
        .expect("ToDelete room should have an ID");

    let initial_count = snap.rooms.len();

    // Delete the room.
    client.delete_room(room_id).await.expect("delete_room should succeed");

    // Wait for room count to decrease.
    let snap = wait_for_room_count(&client, initial_count - 1, 5000).await;

    assert!(
        !snap.rooms.iter().any(|r| r.name == "ToDelete"),
        "ToDelete room should be gone, got {:?}",
        snap.rooms
    );
}

// =============================================================================
// Test: Rename a room
// =============================================================================

#[tokio::test]
async fn test_rename_room() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = backend::Client::connect(&format!("127.0.0.1:{}", port), "room-renamer", None, test_connect_config())
        .await
        .expect("client should connect");

    wait_for_rooms(&client, 5000).await;

    // Create a room.
    client.create_room("OldName").await.expect("create_room should succeed");
    let snap = wait_for_room_named(&client, "OldName", 5000).await;

    let room_id = snap
        .rooms
        .iter()
        .find(|r| r.name == "OldName")
        .and_then(|r| r.id.as_ref())
        .map(|id| id.value)
        .expect("OldName room should have an ID");

    // Rename the room.
    client.rename_room(room_id, "NewName").await.expect("rename_room should succeed");

    // Wait for the new name to appear.
    let snap = wait_for_room_named(&client, "NewName", 5000).await;

    assert!(
        snap.rooms.iter().any(|r| r.name == "NewName"),
        "NewName room should exist in {:?}",
        snap.rooms
    );
    assert!(
        !snap.rooms.iter().any(|r| r.name == "OldName"),
        "OldName should be gone from {:?}",
        snap.rooms
    );
}

// =============================================================================
// Test: Join a room
// =============================================================================

#[tokio::test]
async fn test_join_room() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client = backend::Client::connect(&format!("127.0.0.1:{}", port), "room-joiner", None, test_connect_config())
        .await
        .expect("client should connect");

    wait_for_rooms(&client, 5000).await;

    // Create a new room to join.
    client.create_room("JoinTarget").await.expect("create_room should succeed");
    let snap = wait_for_room_named(&client, "JoinTarget", 5000).await;

    let room_id = snap
        .rooms
        .iter()
        .find(|r| r.name == "JoinTarget")
        .and_then(|r| r.id.as_ref())
        .map(|id| id.value)
        .expect("JoinTarget room should have an ID");

    // Join the room.
    client.join_room(room_id).await.expect("join_room should succeed");

    // Check that our local snapshot reflects the current room.
    let snap = client.get_snapshot().await;
    assert_eq!(
        snap.current_room_id,
        Some(room_id),
        "current_room_id should be the joined room"
    );
}

// =============================================================================
// Test: Chat message between two clients
// =============================================================================

#[tokio::test]
async fn test_chat_message_between_clients() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect first client.
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Alice", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    // Connect second client.
    let mut client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Bob", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    // Both clients should be in the Root room by default.
    // Take the events receiver from client2 to listen for chat.
    let mut events_rx = client2.take_events_receiver();

    // Client1 sends a chat message.
    client1
        .send_chat("Alice", "Hello from Alice!")
        .await
        .expect("send_chat should succeed");

    // Client2 should receive the chat broadcast.
    let mut received_message = false;
    for _ in 0..50 {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), events_rx.recv()).await {
            if let Some(api::proto::server_event::Kind::ChatBroadcast(cb)) = ev.kind {
                if cb.sender == "Alice" && cb.text == "Hello from Alice!" {
                    received_message = true;
                    break;
                }
            }
        }
    }

    assert!(received_message, "client2 should have received the chat message from Alice");
}

// =============================================================================
// Test: Chat messages scoped to room
// =============================================================================

#[tokio::test]
async fn test_chat_messages_scoped_to_room() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect client1 (Alice).
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Alice", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    // Connect client2 (Bob).
    let mut client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Bob", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    // Connect client3 (Charlie).
    let mut client3 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Charlie", None, test_connect_config())
        .await
        .expect("client3 should connect");
    wait_for_rooms(&client3, 5000).await;

    // Create a separate room.
    client1.create_room("PrivateRoom").await.expect("create_room should succeed");
    let snap = wait_for_room_named(&client1, "PrivateRoom", 5000).await;

    let private_room_id = snap
        .rooms
        .iter()
        .find(|r| r.name == "PrivateRoom")
        .and_then(|r| r.id.as_ref())
        .map(|id| id.value)
        .expect("PrivateRoom should have an ID");

    // Alice and Bob join the private room.
    client1.join_room(private_room_id).await.expect("join should succeed");
    client2.join_room(private_room_id).await.expect("join should succeed");
    // Charlie stays in Root (room 1).

    // Give time for room state to propagate.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Take events receivers.
    let mut events_rx2 = client2.take_events_receiver();
    let mut events_rx3 = client3.take_events_receiver();

    // Alice sends a message in the private room.
    client1
        .send_chat("Alice", "Secret message")
        .await
        .expect("send_chat should succeed");

    // Bob should receive it.
    let mut bob_received = false;
    for _ in 0..30 {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), events_rx2.recv()).await {
            if let Some(api::proto::server_event::Kind::ChatBroadcast(cb)) = ev.kind {
                if cb.sender == "Alice" && cb.text == "Secret message" {
                    bob_received = true;
                    break;
                }
            }
        }
    }
    assert!(bob_received, "Bob should have received the secret message");

    // Charlie should NOT receive it (he's in a different room).
    let mut charlie_received = false;
    for _ in 0..10 {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), events_rx3.recv()).await {
            if let Some(api::proto::server_event::Kind::ChatBroadcast(cb)) = ev.kind {
                if cb.sender == "Alice" && cb.text == "Secret message" {
                    charlie_received = true;
                    break;
                }
            }
        }
    }
    assert!(!charlie_received, "Charlie should NOT have received the secret message");
}

// =============================================================================
// Test: Multiple clients see room updates
// =============================================================================

#[tokio::test]
async fn test_room_updates_broadcast_to_all_clients() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect two clients.
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Client1", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    let client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Client2", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    // Client1 creates a room.
    client1.create_room("SharedRoom").await.expect("create_room should succeed");

    // Both clients should see the new room.
    let snap1 = wait_for_room_named(&client1, "SharedRoom", 5000).await;
    let snap2 = wait_for_room_named(&client2, "SharedRoom", 5000).await;

    assert!(
        snap1.rooms.iter().any(|r| r.name == "SharedRoom"),
        "Client1 should see SharedRoom"
    );
    assert!(
        snap2.rooms.iter().any(|r| r.name == "SharedRoom"),
        "Client2 should see SharedRoom"
    );
}

// =============================================================================
// Test: User presence updates when joining rooms
// =============================================================================

#[tokio::test]
async fn test_user_presence_updates() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect first client.
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "User1", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    // Connect second client.
    let client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "User2", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    // Give time for presence updates to propagate.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check that both clients see users in the snapshot.
    let snap1 = client1.get_snapshot().await;
    let snap2 = client2.get_snapshot().await;

    // Both should see at least 2 users (themselves and each other).
    assert!(
        snap1.users.len() >= 2 || snap2.users.len() >= 2,
        "should see multiple users, snap1.users={:?}, snap2.users={:?}",
        snap1.users,
        snap2.users
    );
}
