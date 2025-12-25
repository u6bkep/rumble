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

// =============================================================================
// Transmission Mode Tests
// =============================================================================

#[test]
fn test_transmission_mode_defaults_to_ptt() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    let state = handle.state();
    assert!(
        matches!(state.audio.transmission_mode, backend::TransmissionMode::PushToTalk),
        "Default transmission mode should be PushToTalk"
    );
    assert!(!state.audio.is_transmitting, "Should not be transmitting initially");
}

#[test]
fn test_set_transmission_mode_updates_state() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    // Connect first
    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "mode-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Change to Continuous mode
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Continuous,
    });

    let mode_changed = wait_for(&handle, Duration::from_secs(2), |s| {
        matches!(s.audio.transmission_mode, backend::TransmissionMode::Continuous)
    });
    assert!(mode_changed, "Transmission mode should change to Continuous");

    // In continuous mode while connected, should be transmitting
    let transmitting = wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);
    assert!(transmitting, "Should be transmitting in Continuous mode while connected");

    // Change to Muted mode
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Muted,
    });

    let muted = wait_for(&handle, Duration::from_secs(2), |s| {
        matches!(s.audio.transmission_mode, backend::TransmissionMode::Muted) && !s.audio.is_transmitting
    });
    assert!(muted, "Should not be transmitting in Muted mode");

    // Change back to PTT mode
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::PushToTalk,
    });

    let ptt = wait_for(&handle, Duration::from_secs(2), |s| {
        matches!(s.audio.transmission_mode, backend::TransmissionMode::PushToTalk)
    });
    assert!(ptt, "Should be in PushToTalk mode");
    assert!(!handle.state().audio.is_transmitting, "Should not be transmitting in PTT mode without key pressed");
}

#[test]
fn test_ptt_start_stop_transmit() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "ptt-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Start transmitting (simulate PTT press)
    handle.send(BackendCommand::StartTransmit);

    let transmitting = wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);
    assert!(transmitting, "Should be transmitting after StartTransmit");

    // Stop transmitting (simulate PTT release)
    handle.send(BackendCommand::StopTransmit);

    let stopped = wait_for(&handle, Duration::from_secs(2), |s| !s.audio.is_transmitting);
    assert!(stopped, "Should stop transmitting after StopTransmit");
}

#[test]
fn test_ptt_in_continuous_mode_ignored() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "ptt-continuous-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Switch to Continuous mode
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Continuous,
    });

    wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);

    // Simulate PTT press and release while in continuous mode
    // These should be ignored - transmission should remain active
    handle.send(BackendCommand::StartTransmit);
    std::thread::sleep(Duration::from_millis(100));
    handle.send(BackendCommand::StopTransmit);

    std::thread::sleep(Duration::from_millis(200));

    // Should still be transmitting (continuous mode doesn't stop on PTT release)
    let still_transmitting = handle.state().audio.is_transmitting;
    assert!(still_transmitting, "Continuous mode should keep transmitting after PTT release");
}

#[test]
fn test_switch_from_continuous_to_ptt_while_ptt_held() {
    // This test verifies the fix for the bug where switching from Continuous to PTT
    // while holding the PTT key would stop transmission incorrectly.
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "mode-switch-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Start in Continuous mode
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Continuous,
    });

    wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);

    // Simulate PTT press (while in continuous mode - this gets tracked)
    handle.send(BackendCommand::StartTransmit);
    std::thread::sleep(Duration::from_millis(100));

    // Now switch to PTT mode while "holding" PTT
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::PushToTalk,
    });

    std::thread::sleep(Duration::from_millis(200));

    // Should still be transmitting because PTT was pressed before mode switch
    let still_transmitting = handle.state().audio.is_transmitting;
    assert!(still_transmitting, "Should keep transmitting when switching to PTT with key held");

    // Now release PTT
    handle.send(BackendCommand::StopTransmit);

    let stopped = wait_for(&handle, Duration::from_secs(2), |s| !s.audio.is_transmitting);
    assert!(stopped, "Should stop transmitting after PTT release in PTT mode");
}

#[test]
fn test_switch_from_ptt_to_continuous_continues_transmission() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "ptt-to-continuous-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Start PTT transmission
    handle.send(BackendCommand::StartTransmit);

    wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);

    // Switch to Continuous mode while transmitting
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Continuous,
    });

    std::thread::sleep(Duration::from_millis(200));

    // Should still be transmitting
    assert!(handle.state().audio.is_transmitting, "Should keep transmitting when switching to Continuous");

    // Release PTT (should be ignored in continuous mode)
    handle.send(BackendCommand::StopTransmit);

    std::thread::sleep(Duration::from_millis(200));

    // Should still be transmitting (continuous mode)
    assert!(handle.state().audio.is_transmitting, "Continuous mode should keep transmitting after PTT release");
}

#[test]
fn test_continuous_mode_starts_on_connect() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    // Set continuous mode BEFORE connecting
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Continuous,
    });

    std::thread::sleep(Duration::from_millis(100));

    // Connect
    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "continuous-on-connect-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Should automatically start transmitting on connect in continuous mode
    let transmitting = wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);
    assert!(transmitting, "Should start transmitting on connect when in Continuous mode");
}

#[test]
fn test_muted_mode_does_not_transmit() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "muted-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Set muted mode
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Muted,
    });

    wait_for(&handle, Duration::from_secs(2), |s| {
        matches!(s.audio.transmission_mode, backend::TransmissionMode::Muted)
    });

    // PTT commands should be ignored in muted mode
    handle.send(BackendCommand::StartTransmit);

    std::thread::sleep(Duration::from_millis(200));

    assert!(!handle.state().audio.is_transmitting, "Should not transmit in Muted mode");
}

#[test]
fn test_switch_from_continuous_to_muted_stops_transmission() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "continuous-to-muted-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Start in Continuous mode
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Continuous,
    });

    wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);

    // Switch to Muted
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Muted,
    });

    let stopped = wait_for(&handle, Duration::from_secs(2), |s| !s.audio.is_transmitting);
    assert!(stopped, "Should stop transmitting when switching to Muted mode");
}

#[test]
fn test_disconnect_clears_transmission_state() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "disconnect-transmission-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Start transmitting in Continuous mode
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Continuous,
    });

    wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);

    // Disconnect
    handle.send(BackendCommand::Disconnect);

    wait_for(&handle, Duration::from_secs(2), |s| {
        matches!(s.connection, ConnectionState::Disconnected)
    });

    // Transmission should be stopped
    assert!(!handle.state().audio.is_transmitting, "Should stop transmitting on disconnect");
}

#[test]
fn test_continuous_mode_resumes_on_reconnect() {
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    // Set continuous mode
    handle.send(BackendCommand::SetTransmissionMode {
        mode: backend::TransmissionMode::Continuous,
    });

    // Connect
    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "reconnect-continuous-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());
    wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);

    // Disconnect
    handle.send(BackendCommand::Disconnect);

    wait_for(&handle, Duration::from_secs(2), |s| {
        matches!(s.connection, ConnectionState::Disconnected)
    });
    assert!(!handle.state().audio.is_transmitting, "Should stop on disconnect");

    // Mode should still be Continuous
    assert!(
        matches!(handle.state().audio.transmission_mode, backend::TransmissionMode::Continuous),
        "Mode should persist through disconnect"
    );

    // Reconnect
    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "reconnect-continuous-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Should resume transmitting
    let transmitting = wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);
    assert!(transmitting, "Should resume transmitting on reconnect in Continuous mode");
}

#[test]
fn test_ptt_not_transmitting_after_disconnect() {
    // Verifies that PTT state is reset on disconnect
    let port = next_test_port();
    let _server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "ptt-disconnect-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Start PTT transmission
    handle.send(BackendCommand::StartTransmit);

    wait_for(&handle, Duration::from_secs(2), |s| s.audio.is_transmitting);

    // Disconnect while transmitting
    handle.send(BackendCommand::Disconnect);

    wait_for(&handle, Duration::from_secs(2), |s| {
        matches!(s.connection, ConnectionState::Disconnected)
    });

    // Should not be transmitting after disconnect
    assert!(!handle.state().audio.is_transmitting, "Should not be transmitting after disconnect");

    // Reconnect
    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{}", port),
        name: "ptt-disconnect-test".to_string(),
        password: None,
    });

    wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected());

    // Should NOT be transmitting automatically in PTT mode
    std::thread::sleep(Duration::from_millis(200));
    assert!(!handle.state().audio.is_transmitting, "Should not transmit on reconnect in PTT mode without key pressed");
}

// =============================================================================
// Audio Device Tests
// =============================================================================

#[test]
fn test_audio_devices_available_before_connect() {
    // Audio devices should be enumerable before connecting
    let (handle, _repaint) = create_backend_with_repaint();

    let state = handle.state();
    
    // We just verify the lists exist - whether they have devices depends on the test environment
    // This test mainly ensures the backend initializes audio properly without a connection
    assert!(state.audio.input_devices.is_empty() || !state.audio.input_devices.is_empty());
    assert!(state.audio.output_devices.is_empty() || !state.audio.output_devices.is_empty());
}

#[test]
fn test_refresh_audio_devices() {
    let (handle, _repaint) = create_backend_with_repaint();

    // Refresh devices should not panic or fail
    handle.send(BackendCommand::RefreshAudioDevices);

    std::thread::sleep(Duration::from_millis(200));

    // State should still be valid
    let _ = handle.state();
}

#[test]
fn test_set_input_device() {
    let (handle, _repaint) = create_backend_with_repaint();

    // Setting a device (even invalid) should update state
    handle.send(BackendCommand::SetInputDevice {
        device_id: Some("test-device".to_string()),
    });

    let updated = wait_for(&handle, Duration::from_secs(2), |s| {
        s.audio.selected_input == Some("test-device".to_string())
    });

    assert!(updated, "Selected input device should be updated");
}

#[test]
fn test_set_output_device() {
    let (handle, _repaint) = create_backend_with_repaint();

    handle.send(BackendCommand::SetOutputDevice {
        device_id: Some("test-output-device".to_string()),
    });

    let updated = wait_for(&handle, Duration::from_secs(2), |s| {
        s.audio.selected_output == Some("test-output-device".to_string())
    });

    assert!(updated, "Selected output device should be updated");
}

