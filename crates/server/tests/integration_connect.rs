use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[tokio::test]
async fn server_and_client_connect_and_log_room_state() {
    // Start the server binary in the background.
    // Pick a non-default port to avoid collisions.
    let test_port = 55000u16;
    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .env("RUST_LOG", "info")
        .env("RUMBLE_PORT", test_port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start server binary");

    // Ensure the server process is terminated even on panic/timeout.
    struct ChildGuard(Option<Child>);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(mut c) = self.0.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
    let mut guard = ChildGuard(Some(child));

    // Pipe server stdout/stderr to test output for diagnostics.
    if let Some(out) = guard.0.as_mut().and_then(|c| c.stdout.take()) {
        std::thread::spawn(move || {
            let reader = BufReader::new(out);
            for line in reader.lines().flatten() {
                println!("[server stdout] {}", line);
            }
        });
    }
    if let Some(err) = guard.0.as_mut().and_then(|c| c.stderr.take()) {
        std::thread::spawn(move || {
            let reader = BufReader::new(err);
            for line in reader.lines().flatten() {
                eprintln!("[server stderr] {}", line);
            }
        });
    }

    // Give the server time to bind.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Connect a backend client and wait briefly for room state.
    let config = backend::ConnectConfig::new().with_cert("dev-certs/server-cert.der");
    let mut client = backend::Client::connect(&format!("127.0.0.1:{}", test_port), "test-client", None, config)
        .await
        .expect("client connect");

    // Auto-join is triggered in Client::connect; wait until snapshot has rooms.
    // As an extra nudge, request join to room 1.
    let _ = client.join_room(1).await;

    // Drain events for a short period to observe server activity.
    let mut events_rx = client.take_events_receiver();
    for _ in 0..20 {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), events_rx.recv()).await {
            println!("received event: {:?}", ev.kind);
        }
    }

    // Poll snapshot until rooms appear.
    let mut attempts = 0;
    loop {
        let snap = client.get_snapshot().await;
        if !snap.rooms.is_empty() {
            println!("rooms={} users={}", snap.rooms.len(), snap.users.len());
            break;
        }
        attempts += 1;
        if attempts > 50 { panic!("timeout waiting for room state from server"); }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Cleanup handled by ChildGuard Drop.
}
