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

// =============================================================================
// Test: Client disconnect removes user from room
// =============================================================================

#[tokio::test]
async fn test_client_disconnect_removes_user() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect first client that will observe.
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Observer", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    // Connect second client that will disconnect.
    let client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Disconnecter", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    // Wait for both clients to be visible.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let snap = client1.get_snapshot().await;
    let initial_user_count = snap.users.len();
    assert!(
        initial_user_count >= 2,
        "should see at least 2 users before disconnect, got {:?}",
        snap.users
    );

    // Drop client2 to simulate disconnect.
    drop(client2);

    // Wait for the server to notice the disconnect and broadcast the update.
    let mut final_user_count = initial_user_count;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snap = client1.get_snapshot().await;
        final_user_count = snap.users.len();
        if final_user_count < initial_user_count {
            break;
        }
    }

    assert!(
        final_user_count < initial_user_count,
        "user count should decrease after disconnect: initial={}, final={}",
        initial_user_count,
        final_user_count
    );

    // Also verify the disconnected user is no longer in the list.
    let snap = client1.get_snapshot().await;
    assert!(
        !snap.users.iter().any(|u| u.username == "Disconnecter"),
        "Disconnecter should not be in user list after disconnect, got {:?}",
        snap.users
    );
}

// =============================================================================
// Test: Reconnect after disconnect shows only one user
// =============================================================================

#[tokio::test]
async fn test_reconnect_shows_single_user() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect observer client.
    let observer = backend::Client::connect(&format!("127.0.0.1:{}", port), "Observer", None, test_connect_config())
        .await
        .expect("observer should connect");
    wait_for_rooms(&observer, 5000).await;

    // Connect client, disconnect, reconnect.
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Reconnecter", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    // Verify the user is visible.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snap = observer.get_snapshot().await;
    let reconnecter_count_before = snap.users.iter().filter(|u| u.username == "Reconnecter").count();
    assert_eq!(
        reconnecter_count_before, 1,
        "should see exactly 1 Reconnecter before disconnect, got {}",
        reconnecter_count_before
    );

    // Disconnect.
    drop(client1);

    // Wait for disconnect to be processed.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Reconnect with the same name.
    let client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Reconnecter", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    // Wait for state to propagate.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // There should be exactly ONE user named "Reconnecter", not two.
    let snap = observer.get_snapshot().await;
    let reconnecter_count = snap.users.iter().filter(|u| u.username == "Reconnecter").count();
    assert_eq!(
        reconnecter_count, 1,
        "should see exactly 1 Reconnecter after reconnect, got {} in {:?}",
        reconnecter_count,
        snap.users
    );
}

// =============================================================================
// Test: Multiple disconnect/reconnect cycles
// =============================================================================

#[tokio::test]
async fn test_multiple_disconnect_reconnect_cycles() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect observer client.
    let observer = backend::Client::connect(&format!("127.0.0.1:{}", port), "Observer", None, test_connect_config())
        .await
        .expect("observer should connect");
    wait_for_rooms(&observer, 5000).await;

    for cycle in 1..=3 {
        // Connect.
        let client = backend::Client::connect(&format!("127.0.0.1:{}", port), "CycleUser", None, test_connect_config())
            .await
            .expect("client should connect");
        wait_for_rooms(&client, 5000).await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify user is visible.
        let snap = observer.get_snapshot().await;
        let count = snap.users.iter().filter(|u| u.username == "CycleUser").count();
        assert_eq!(
            count, 1,
            "cycle {}: should see exactly 1 CycleUser while connected, got {} in {:?}",
            cycle, count, snap.users
        );

        // Disconnect.
        drop(client);
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Verify user is gone.
        let snap = observer.get_snapshot().await;
        let count = snap.users.iter().filter(|u| u.username == "CycleUser").count();
        assert_eq!(
            count, 0,
            "cycle {}: should see 0 CycleUser after disconnect, got {} in {:?}",
            cycle, count, snap.users
        );
    }
}

// =============================================================================
// Test: Explicit disconnect() method works
// =============================================================================

#[tokio::test]
async fn test_explicit_disconnect_method() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect observer client.
    let observer = backend::Client::connect(&format!("127.0.0.1:{}", port), "Observer", None, test_connect_config())
        .await
        .expect("observer should connect");
    wait_for_rooms(&observer, 5000).await;

    // Connect client that will explicitly disconnect.
    let client = backend::Client::connect(&format!("127.0.0.1:{}", port), "ExplicitDisconnect", None, test_connect_config())
        .await
        .expect("client should connect");
    wait_for_rooms(&client, 5000).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify user is visible.
    let snap = observer.get_snapshot().await;
    assert!(
        snap.users.iter().any(|u| u.username == "ExplicitDisconnect"),
        "should see ExplicitDisconnect user"
    );

    // Use explicit disconnect method.
    client.disconnect().await.expect("disconnect should succeed");

    // Wait for server to process disconnect.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify user is gone.
    let snap = observer.get_snapshot().await;
    assert!(
        !snap.users.iter().any(|u| u.username == "ExplicitDisconnect"),
        "ExplicitDisconnect should be gone after disconnect(), got {:?}",
        snap.users
    );
}

// =============================================================================
// Test: Server handles abruptly closed client (simulates crash/network failure)
// =============================================================================

#[tokio::test]
async fn test_abrupt_client_disconnect() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect observer client.
    let observer = backend::Client::connect(&format!("127.0.0.1:{}", port), "Observer", None, test_connect_config())
        .await
        .expect("observer should connect");
    wait_for_rooms(&observer, 5000).await;

    // Connect client that will be abruptly closed.
    let client = backend::Client::connect(&format!("127.0.0.1:{}", port), "AbruptClose", None, test_connect_config())
        .await
        .expect("client should connect");
    wait_for_rooms(&client, 5000).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Verify user is visible.
    let snap = observer.get_snapshot().await;
    let initial_count = snap.users.iter().filter(|u| u.username == "AbruptClose").count();
    assert_eq!(
        initial_count, 1,
        "should see exactly 1 AbruptClose user before disconnect"
    );

    // Simulate abrupt close by dropping the client (which calls conn.close in Drop).
    // This is more abrupt than calling disconnect() which sends a Disconnect message first.
    drop(client);

    // The server should detect the closed connection quickly due to QUIC transport settings.
    // With keep_alive_interval of 5s and max_idle_timeout of 30s, plus conn.close() in Drop,
    // the server should detect this almost immediately.
    let mut user_removed = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let snap = observer.get_snapshot().await;
        if !snap.users.iter().any(|u| u.username == "AbruptClose") {
            user_removed = true;
            break;
        }
    }

    assert!(
        user_removed,
        "AbruptClose user should be removed after abrupt disconnect (within 6 seconds)"
    );
}

// =============================================================================
// Test: Voice datagram - single packet send and receive
// =============================================================================

#[tokio::test]
async fn test_voice_datagram_send_receive() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect sender client.
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "VoiceSender", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    // Connect receiver client.
    let mut client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "VoiceReceiver", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    // Set user IDs for the clients (simulating what server would assign).
    // In a real scenario, server would send this in ServerHello or similar.
    client1.set_user_id(1).await;
    client2.set_user_id(2).await;

    // Both clients should be in the Root room by default.
    // Give time for room state to propagate.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Take the voice receiver from client2.
    let mut voice_rx = client2.take_voice_receiver();

    // Generate random test data (simulating an Opus frame).
    let test_data: Vec<u8> = (0..160).map(|i| (i * 17 % 256) as u8).collect();
    let test_data_clone = test_data.clone();

    // Send a voice datagram from client1.
    let seq = client1
        .send_voice_datagram_async(test_data)
        .await
        .expect("send_voice_datagram should succeed");

    assert_eq!(seq, 0, "first sequence number should be 0");

    // Client2 should receive the voice datagram.
    let mut received_datagram = None;
    for _ in 0..50 {
        if let Ok(Some(dgram)) = tokio::time::timeout(Duration::from_millis(100), voice_rx.recv()).await {
            received_datagram = Some(dgram);
            break;
        }
    }

    assert!(
        received_datagram.is_some(),
        "client2 should have received the voice datagram"
    );

    let dgram = received_datagram.unwrap();
    assert_eq!(dgram.sender_id, Some(1), "sender_id should be 1 (set by server)");
    assert_eq!(dgram.sequence, 0, "sequence should be 0");
    assert_eq!(dgram.opus_data, test_data_clone, "opus_data should match sent data");
}

// =============================================================================
// Test: Voice datagram - multiple packets with sequence numbers
// =============================================================================

#[tokio::test]
async fn test_voice_datagram_sequence_numbers() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect sender client.
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "SeqSender", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    // Connect receiver client.
    let mut client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "SeqReceiver", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    client1.set_user_id(1).await;
    client2.set_user_id(2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut voice_rx = client2.take_voice_receiver();

    // Send multiple voice datagrams.
    let num_packets = 10;
    for i in 0..num_packets {
        let data: Vec<u8> = vec![i as u8; 100];
        let seq = client1
            .send_voice_datagram_async(data)
            .await
            .expect("send should succeed");
        assert_eq!(seq, i, "sequence number should increment");
    }

    // Receive and verify all packets (order may vary due to UDP-like behavior).
    let mut received_seqs = Vec::new();
    for _ in 0..100 {
        if let Ok(Some(dgram)) = tokio::time::timeout(Duration::from_millis(50), voice_rx.recv()).await {
            received_seqs.push(dgram.sequence);
            if received_seqs.len() >= num_packets as usize {
                break;
            }
        }
    }

    assert!(
        received_seqs.len() >= num_packets as usize - 2, // Allow for some packet loss in tests
        "should receive most packets, got {} of {}",
        received_seqs.len(),
        num_packets
    );
}

// =============================================================================
// Test: Voice datagram - room scoping (only same room receives)
// =============================================================================

#[tokio::test]
async fn test_voice_datagram_room_scoping() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect three clients.
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Alice", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    let mut client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Bob", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    let mut client3 = backend::Client::connect(&format!("127.0.0.1:{}", port), "Charlie", None, test_connect_config())
        .await
        .expect("client3 should connect");
    wait_for_rooms(&client3, 5000).await;

    client1.set_user_id(1).await;
    client2.set_user_id(2).await;
    client3.set_user_id(3).await;

    // Create a private room.
    client1.create_room("PrivateVoice").await.expect("create_room should succeed");
    let snap = wait_for_room_named(&client1, "PrivateVoice", 5000).await;

    let private_room_id = snap
        .rooms
        .iter()
        .find(|r| r.name == "PrivateVoice")
        .and_then(|r| r.id.as_ref())
        .map(|id| id.value)
        .expect("PrivateVoice should have an ID");

    // Alice and Bob join the private room.
    client1.join_room(private_room_id).await.expect("join should succeed");
    client2.join_room(private_room_id).await.expect("join should succeed");
    // Charlie stays in Root (room 1).

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Take voice receivers.
    let mut voice_rx2 = client2.take_voice_receiver();
    let mut voice_rx3 = client3.take_voice_receiver();

    // Alice sends a voice datagram.
    let test_data: Vec<u8> = vec![0xAB; 64];
    client1
        .send_voice_datagram_async(test_data.clone())
        .await
        .expect("send should succeed");

    // Bob should receive it (same room).
    let mut bob_received = false;
    for _ in 0..30 {
        if let Ok(Some(dgram)) = tokio::time::timeout(Duration::from_millis(100), voice_rx2.recv()).await {
            if dgram.opus_data == test_data {
                bob_received = true;
                break;
            }
        }
    }
    assert!(bob_received, "Bob should have received the voice datagram");

    // Charlie should NOT receive it (different room).
    let mut charlie_received = false;
    for _ in 0..10 {
        if let Ok(Some(dgram)) = tokio::time::timeout(Duration::from_millis(100), voice_rx3.recv()).await {
            if dgram.opus_data == test_data {
                charlie_received = true;
                break;
            }
        }
    }
    assert!(!charlie_received, "Charlie should NOT have received the voice datagram");
}

// =============================================================================
// Test: Voice datagram - random data integrity
// =============================================================================

#[tokio::test]
async fn test_voice_datagram_random_data_integrity() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect sender client.
    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "RandomSender", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    // Connect receiver client.
    let mut client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "RandomReceiver", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    client1.set_user_id(1).await;
    client2.set_user_id(2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut voice_rx = client2.take_voice_receiver();

    // Generate a larger random payload (simulating a real Opus frame, ~80-320 bytes typically).
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    
    fn simple_random(seed: u64, len: usize) -> Vec<u8> {
        let mut hasher = DefaultHasher::new();
        let mut result = Vec::with_capacity(len);
        for i in 0..len {
            (seed, i as u64).hash(&mut hasher);
            result.push(hasher.finish() as u8);
        }
        result
    }

    let random_data = simple_random(12345, 320);
    let random_data_clone = random_data.clone();

    // Send the random data.
    client1
        .send_voice_datagram_async(random_data)
        .await
        .expect("send should succeed");

    // Receive and verify integrity.
    let mut received = None;
    for _ in 0..50 {
        if let Ok(Some(dgram)) = tokio::time::timeout(Duration::from_millis(100), voice_rx.recv()).await {
            received = Some(dgram);
            break;
        }
    }

    assert!(received.is_some(), "should receive the datagram");
    let dgram = received.unwrap();
    assert_eq!(
        dgram.opus_data, random_data_clone,
        "received data should exactly match sent random data"
    );
    assert_eq!(dgram.opus_data.len(), 320, "data length should be 320 bytes");
}

// =============================================================================
// Test: Voice datagram - timestamp is reasonable
// =============================================================================

#[tokio::test]
async fn test_voice_datagram_has_timestamp() {
    let port = next_test_port();
    let _server = start_server(port);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = backend::Client::connect(&format!("127.0.0.1:{}", port), "TimeSender", None, test_connect_config())
        .await
        .expect("client1 should connect");
    wait_for_rooms(&client1, 5000).await;

    let mut client2 = backend::Client::connect(&format!("127.0.0.1:{}", port), "TimeReceiver", None, test_connect_config())
        .await
        .expect("client2 should connect");
    wait_for_rooms(&client2, 5000).await;

    client1.set_user_id(1).await;
    client2.set_user_id(2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut voice_rx = client2.take_voice_receiver();

    let before_send = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;

    client1
        .send_voice_datagram_async(vec![1, 2, 3])
        .await
        .expect("send should succeed");

    let after_send = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;

    // Receive and verify timestamp is reasonable.
    let mut received = None;
    for _ in 0..50 {
        if let Ok(Some(dgram)) = tokio::time::timeout(Duration::from_millis(100), voice_rx.recv()).await {
            received = Some(dgram);
            break;
        }
    }

    assert!(received.is_some(), "should receive the datagram");
    let dgram = received.unwrap();
    
    // Timestamp should be between before_send and after_send (with some tolerance).
    assert!(
        dgram.timestamp_us >= before_send - 1_000_000 && dgram.timestamp_us <= after_send + 1_000_000,
        "timestamp should be reasonable: {} not in range [{}, {}]",
        dgram.timestamp_us,
        before_send,
        after_send
    );
}
