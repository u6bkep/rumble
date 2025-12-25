//! Integration tests for the backend crate interacting with the server.
//!
//! These tests use the actual `BackendHandle` from the backend crate to test
//! the full client-server interaction through the state-driven API.

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc,
    },
    time::Duration,
};

use backend::{BackendHandle, Command as BackendCommand, ConnectConfig, ConnectionState};

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
static PORT_COUNTER: AtomicU16 = AtomicU16::new(57000);

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

/// Helper to create a BackendHandle with the dev certificate and a repaint counter.
fn create_backend_with_repaint() -> (BackendHandle, Arc<AtomicBool>) {
    let repaint_called = Arc::new(AtomicBool::new(false));
    let repaint_called_clone = repaint_called.clone();

    let config = ConnectConfig::new().with_cert("dev-certs/server-cert.der");

    let handle =
        BackendHandle::with_config(move || repaint_called_clone.store(true, Ordering::SeqCst), config);

    (handle, repaint_called)
}

/// Wait for a condition to become true, polling state periodically.
fn wait_for<F>(handle: &BackendHandle, timeout: Duration, condition: F) -> bool
where
    F: Fn(&backend::State) -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if condition(&handle.state()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

// =============================================================================
// Tests
// =============================================================================

#[test]
fn test_backend_connects_to_server() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    // Initially disconnected
    assert!(matches!(
        handle.state().connection,
        ConnectionState::Disconnected
    ));

    // Send connect command
    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "backend-test".to_string(),
        password: None,
    });

    // Wait for connection
    let connected = wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    assert!(connected, "Backend should connect to server");

    let state = handle.state();
    assert!(state.my_user_id.is_some(), "Should have a user ID");
    assert!(!state.rooms.is_empty(), "Should have rooms from server");

    // Check we got the Root room
    assert!(
        state.rooms.iter().any(|r| r.name == "Root"),
        "Should have Root room"
    );
}

#[test]
fn test_backend_disconnect() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    // Connect
    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "disconnect-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());
    assert!(handle.is_connected());

    // Disconnect
    handle.send(BackendCommand::Disconnect);

    let disconnected = wait_for(&handle, Duration::from_secs(2), |s| {
        matches!(s.connection, ConnectionState::Disconnected)
    });

    assert!(disconnected, "Backend should disconnect");
    assert!(handle.state().my_user_id.is_none());
    assert!(handle.state().rooms.is_empty());
}

#[test]
fn test_backend_create_room() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "room-creator".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    let initial_room_count = handle.state().rooms.len();

    // Create a new room
    handle.send(BackendCommand::CreateRoom {
        name: "Backend Test Room".to_string(),
    });

    // Wait for room to appear
    let room_created = wait_for(&handle, Duration::from_secs(2), |s| {
        s.rooms.len() > initial_room_count
    });

    assert!(room_created, "Room should be created");
    assert!(
        handle.state().rooms.iter().any(|r| r.name == "Backend Test Room"),
        "Created room should exist"
    );
}

#[test]
fn test_backend_delete_room() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "room-deleter".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Create a room to delete
    handle.send(BackendCommand::CreateRoom {
        name: "Room To Delete".to_string(),
    });

    wait_for(&handle, Duration::from_secs(2), |s| {
        s.rooms.iter().any(|r| r.name == "Room To Delete")
    });

    // Get the room's UUID
    let room_uuid = handle
        .state()
        .rooms
        .iter()
        .find(|r| r.name == "Room To Delete")
        .and_then(|r| r.id.as_ref())
        .and_then(api::uuid_from_room_id)
        .expect("Should find created room");

    let count_before = handle.state().rooms.len();

    // Delete the room
    handle.send(BackendCommand::DeleteRoom { room_id: room_uuid });

    let room_deleted = wait_for(&handle, Duration::from_secs(2), |s| s.rooms.len() < count_before);

    assert!(room_deleted, "Room should be deleted");
    assert!(
        !handle.state().rooms.iter().any(|r| r.name == "Room To Delete"),
        "Deleted room should be gone"
    );
}

#[test]
fn test_backend_rename_room() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "room-renamer".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Create a room to rename
    handle.send(BackendCommand::CreateRoom {
        name: "Original Room Name".to_string(),
    });

    wait_for(&handle, Duration::from_secs(2), |s| {
        s.rooms.iter().any(|r| r.name == "Original Room Name")
    });

    let room_uuid = handle
        .state()
        .rooms
        .iter()
        .find(|r| r.name == "Original Room Name")
        .and_then(|r| r.id.as_ref())
        .and_then(api::uuid_from_room_id)
        .expect("Should find created room");

    // Rename the room
    handle.send(BackendCommand::RenameRoom {
        room_id: room_uuid,
        new_name: "Renamed Room".to_string(),
    });

    let room_renamed = wait_for(&handle, Duration::from_secs(2), |s| {
        s.rooms.iter().any(|r| r.name == "Renamed Room")
    });

    assert!(room_renamed, "Room should be renamed");
    assert!(
        !handle.state().rooms.iter().any(|r| r.name == "Original Room Name"),
        "Old name should be gone"
    );
}

#[test]
fn test_backend_user_appears_in_state() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "visible-user".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    let state = handle.state();
    let my_user_id = state.my_user_id.expect("Should have user ID");

    // Check that our user is in the users list
    assert!(
        state.users.iter().any(|u| {
            u.user_id.as_ref().map(|id| id.value) == Some(my_user_id)
                && u.username == "visible-user"
        }),
        "Should see ourselves in users list"
    );
}

#[test]
fn test_two_backends_see_each_other() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle1, _repaint1) = create_backend_with_repaint();
    let (handle2, _repaint2) = create_backend_with_repaint();

    // Connect first backend
    handle1.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "first-backend".to_string(),
        password: None,
    });

    wait_for(&handle1, Duration::from_secs(5), |s| s.connection.is_connected());
    let user1_id = handle1.state().my_user_id.expect("user 1 should have ID");

    // Connect second backend
    handle2.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "second-backend".to_string(),
        password: None,
    });

    wait_for(&handle2, Duration::from_secs(5), |s| s.connection.is_connected());
    let user2_id = handle2.state().my_user_id.expect("user 2 should have ID");

    assert_ne!(user1_id, user2_id, "Users should have different IDs");

    // Backend 2 should see backend 1 in its user list
    let backend2_sees_backend1 = wait_for(&handle2, Duration::from_secs(2), |s| {
        s.users
            .iter()
            .any(|u| u.user_id.as_ref().map(|id| id.value) == Some(user1_id))
    });

    assert!(backend2_sees_backend1, "Backend 2 should see backend 1");

    // Backend 1 should receive update about backend 2
    // (may need to wait for the state update)
    let backend1_sees_backend2 = wait_for(&handle1, Duration::from_secs(2), |s| {
        s.users
            .iter()
            .any(|u| u.user_id.as_ref().map(|id| id.value) == Some(user2_id))
    });

    assert!(backend1_sees_backend2, "Backend 1 should see backend 2");
}

#[test]
fn test_no_duplicate_users_when_second_client_connects() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle1, _repaint1) = create_backend_with_repaint();

    // Connect first backend
    handle1.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "first-client".to_string(),
        password: None,
    });

    wait_for(&handle1, Duration::from_secs(5), |s| s.connection.is_connected());
    let user1_id = handle1.state().my_user_id.expect("user 1 should have ID");

    // Verify client 1 only appears once in its own user list
    let user1_count_before = handle1
        .state()
        .users
        .iter()
        .filter(|u| u.user_id.as_ref().map(|id| id.value) == Some(user1_id))
        .count();
    assert_eq!(user1_count_before, 1, "User 1 should appear exactly once before second client connects");

    // Connect second backend
    let (handle2, _repaint2) = create_backend_with_repaint();
    handle2.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "second-client".to_string(),
        password: None,
    });

    wait_for(&handle2, Duration::from_secs(5), |s| s.connection.is_connected());
    let user2_id = handle2.state().my_user_id.expect("user 2 should have ID");

    // Wait for handle1 to see handle2
    wait_for(&handle1, Duration::from_secs(2), |s| {
        s.users
            .iter()
            .any(|u| u.user_id.as_ref().map(|id| id.value) == Some(user2_id))
    });

    // Verify client 1 still only appears once (no duplicates from UserJoined broadcast)
    let user1_count_after = handle1
        .state()
        .users
        .iter()
        .filter(|u| u.user_id.as_ref().map(|id| id.value) == Some(user1_id))
        .count();
    assert_eq!(user1_count_after, 1, "User 1 should still appear exactly once after second client connects");

    // Verify client 2 only appears once in both client lists
    let user2_count_in_handle1 = handle1
        .state()
        .users
        .iter()
        .filter(|u| u.user_id.as_ref().map(|id| id.value) == Some(user2_id))
        .count();
    assert_eq!(user2_count_in_handle1, 1, "User 2 should appear exactly once in client 1's list");

    let user2_count_in_handle2 = handle2
        .state()
        .users
        .iter()
        .filter(|u| u.user_id.as_ref().map(|id| id.value) == Some(user2_id))
        .count();
    assert_eq!(user2_count_in_handle2, 1, "User 2 should appear exactly once in its own list");
}

#[test]
fn test_backend_room_updates_broadcast() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle1, _repaint1) = create_backend_with_repaint();
    let (handle2, _repaint2) = create_backend_with_repaint();

    // Connect both backends
    handle1.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "broadcaster".to_string(),
        password: None,
    });
    handle2.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "listener".to_string(),
        password: None,
    });

    wait_for(&handle1, Duration::from_secs(5), |s| s.connection.is_connected());
    wait_for(&handle2, Duration::from_secs(5), |s| s.connection.is_connected());

    // Backend 1 creates a room
    handle1.send(BackendCommand::CreateRoom {
        name: "Broadcast Room".to_string(),
    });

    // Both should see the new room
    let handle1_sees_room = wait_for(&handle1, Duration::from_secs(2), |s| {
        s.rooms.iter().any(|r| r.name == "Broadcast Room")
    });
    let handle2_sees_room = wait_for(&handle2, Duration::from_secs(2), |s| {
        s.rooms.iter().any(|r| r.name == "Broadcast Room")
    });

    assert!(handle1_sees_room, "Backend 1 should see new room");
    assert!(handle2_sees_room, "Backend 2 should see new room");
}

#[test]
fn test_backend_disconnect_removes_user() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle1, _repaint1) = create_backend_with_repaint();
    let (handle2, _repaint2) = create_backend_with_repaint();

    // Connect both backends
    handle1.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "will-disconnect".to_string(),
        password: None,
    });

    wait_for(&handle1, Duration::from_secs(5), |s| s.connection.is_connected());
    let user1_id = handle1.state().my_user_id.expect("user 1 should have ID");

    handle2.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "observer".to_string(),
        password: None,
    });

    wait_for(&handle2, Duration::from_secs(5), |s| s.connection.is_connected());

    // Backend 2 should see backend 1
    wait_for(&handle2, Duration::from_secs(2), |s| {
        s.users
            .iter()
            .any(|u| u.user_id.as_ref().map(|id| id.value) == Some(user1_id))
    });

    // Backend 1 disconnects
    handle1.send(BackendCommand::Disconnect);

    // Wait for backend 2 to see the user leave
    let user1_gone = wait_for(&handle2, Duration::from_secs(3), |s| {
        !s.users
            .iter()
            .any(|u| u.user_id.as_ref().map(|id| id.value) == Some(user1_id))
    });

    assert!(user1_gone, "Backend 1 should be gone from user list after disconnect");
}

#[test]
fn test_backend_join_room() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "room-joiner".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Create a new room
    handle.send(BackendCommand::CreateRoom {
        name: "Room To Join".to_string(),
    });

    wait_for(&handle, Duration::from_secs(2), |s| {
        s.rooms.iter().any(|r| r.name == "Room To Join")
    });

    let room_uuid = handle
        .state()
        .rooms
        .iter()
        .find(|r| r.name == "Room To Join")
        .and_then(|r| r.id.as_ref())
        .and_then(api::uuid_from_room_id)
        .expect("Should find created room");

    // Join the room
    handle.send(BackendCommand::JoinRoom { room_id: room_uuid });

    // Wait for the join to be reflected in state
    // The user's current_room should be updated
    let joined = wait_for(&handle, Duration::from_secs(2), |s| {
        let my_id = s.my_user_id;
        s.users.iter().any(|u| {
            u.user_id.as_ref().map(|id| id.value) == my_id
                && u.current_room
                    .as_ref()
                    .and_then(api::uuid_from_room_id)
                    == Some(room_uuid)
        })
    });

    assert!(joined, "User should be in the new room");
}

#[test]
#[ignore = "Requires ~40s to wait for QUIC idle timeout - run with --ignored flag"]
fn test_backend_connection_lost_on_server_shutdown() {
    let port = next_test_port();
    let mut server_guard = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "connection-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Kill the server
    if let Some(mut c) = server_guard.0.take() {
        let _ = c.kill();
        let _ = c.wait();
    }

    // Wait for connection lost state
    // Note: QUIC detection of connection loss can take a while (up to idle timeout).
    // The keep-alive interval is 5s and idle timeout is 30s, so worst case is 35s.
    // We wait a reasonable amount of time for the connection loss to be detected.
    let connection_lost = wait_for(&handle, Duration::from_secs(40), |s| {
        matches!(s.connection, ConnectionState::ConnectionLost { .. })
    });

    assert!(connection_lost, "Backend should detect connection lost");
}

#[test]
fn test_backend_repaint_callback_called() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, repaint_called) = create_backend_with_repaint();

    // Reset repaint flag
    repaint_called.store(false, Ordering::SeqCst);

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "repaint-test".to_string(),
        password: None,
    });

    // Wait for connection and check repaint was called
    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Repaint should have been called at some point during connection
    assert!(repaint_called.load(Ordering::SeqCst), "Repaint callback should be called");
}
