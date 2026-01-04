use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc,
    },
    time::Duration,
};

use backend::{BackendHandle, Command as BackendCommand, ConnectConfig, SigningCallback};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use tempfile::TempDir;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
}

/// Guard that kills the server process on drop and cleans up temp dirs.
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
static PORT_COUNTER: AtomicU16 = AtomicU16::new(58000);

fn next_test_port() -> u16 {
    PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Start a server instance on the given port and return the guard.
fn start_server(port: u16) -> ServerGuard {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let cert_dir = temp_dir.path().join("certs");
    let data_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&cert_dir).expect("failed to create cert dir");
    std::fs::create_dir_all(&data_dir).expect("failed to create data dir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .env("RUST_LOG", "debug")
        .env("RUMBLE_NO_CONFIG", "1")
        .env("RUMBLE_PORT", port.to_string())
        .env("RUMBLE_CERT_DIR", cert_dir.to_str().unwrap())
        .env("RUMBLE_DATA_DIR", data_dir.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start server binary");

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

fn create_backend(cert_path: &std::path::Path, download_dir: PathBuf) -> (BackendHandle, Arc<AtomicBool>) {
    let repaint_called = Arc::new(AtomicBool::new(false));
    let repaint_called_clone = repaint_called.clone();

    let config = ConnectConfig::new()
        .with_cert(cert_path)
        .with_download_dir(download_dir);

    let handle =
        BackendHandle::with_config(move || repaint_called_clone.store(true, Ordering::SeqCst), config);

    (handle, repaint_called)
}

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

fn create_test_credentials() -> ([u8; 32], SigningCallback) {
    let signing_key = SigningKey::generate(&mut OsRng);
    let public_key = signing_key.verifying_key().to_bytes();
    let key_bytes = signing_key.to_bytes();
    
    let signer: SigningCallback = Arc::new(move |payload: &[u8]| {
        use ed25519_dalek::Signer;
        let key = SigningKey::from_bytes(&key_bytes);
        let signature = key.sign(payload);
        Ok(signature.to_bytes())
    });
    
    (public_key, signer)
}

fn send_connect(handle: &BackendHandle, addr: String, name: String) {
    let (public_key, signer) = create_test_credentials();
    handle.send(BackendCommand::Connect {
        addr,
        name,
        public_key,
        signer,
        password: None,
    });
}

#[test]
fn test_file_transfer() {
    init_tracing();
    let port = next_test_port();
    let server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    // Setup Client A (Sharer)
    let temp_dir_a = TempDir::new().expect("failed to create temp dir A");
    let (handle_a, _) = create_backend(&server.cert_path, temp_dir_a.path().to_path_buf());
    send_connect(&handle_a, format!("127.0.0.1:{}", port), "client-a".to_string());
    assert!(wait_for(&handle_a, Duration::from_secs(5), |s| s.connection.is_connected()), "Client A failed to connect");

    // Setup Client B (Downloader)
    let temp_dir_b = TempDir::new().expect("failed to create temp dir B");
    let (handle_b, _) = create_backend(&server.cert_path, temp_dir_b.path().to_path_buf());
    send_connect(&handle_b, format!("127.0.0.1:{}", port), "client-b".to_string());
    assert!(wait_for(&handle_b, Duration::from_secs(5), |s| s.connection.is_connected()), "Client B failed to connect");

    // Create a dummy file for Client A to share
    // Note: We create it in a separate directory to avoid conflicts with the download directory
    let source_dir = TempDir::new().expect("failed to create source dir");
    let file_name = "test_file.txt";
    let file_content = "Hello, Rumble File Transfer!";
    let file_path = source_dir.path().join(file_name);
    std::fs::write(&file_path, file_content).expect("failed to write test file");

    // Client A shares the file
    handle_a.send(BackendCommand::ShareFile { path: file_path.clone() });

    // Wait for Client A to get the magnet link (it appears in chat messages)
    let mut magnet_link = String::new();
    let found_magnet = wait_for(&handle_a, Duration::from_secs(5), |s| {
        for msg in &s.chat_messages {
            if msg.is_local && msg.text.contains("Magnet:") {
                return true;
            }
        }
        false
    });
    
    if !found_magnet {
        let state = handle_a.state();
        println!("Client A chat messages:");
        for msg in &state.chat_messages {
            println!("- [{}] {}", msg.sender, msg.text);
        }
    }
    assert!(found_magnet, "Client A failed to generate magnet link");

    // Retrieve the magnet link
    let state_a = handle_a.state();
    for msg in &state_a.chat_messages {
        if msg.is_local && msg.text.contains("Magnet:") {
            let parts: Vec<&str> = msg.text.split("Magnet: ").collect();
            if parts.len() > 1 {
                magnet_link = parts[1].trim().to_string();
                break;
            }
        }
    }
    assert!(!magnet_link.is_empty(), "Magnet link is empty");
    println!("Magnet link: {}", magnet_link);

    // Wait a bit for Client A to announce to the server
    std::thread::sleep(Duration::from_secs(2));

    // Client B downloads the file
    handle_b.send(BackendCommand::DownloadFile { magnet: magnet_link });

    // Wait for Client B to finish downloading
    // We check file_transfers state
    let download_finished = wait_for(&handle_b, Duration::from_secs(30), |s| {
        for transfer in &s.file_transfers {
            // Check if transfer is finished (Seeding or Completed state, with full progress)
            if transfer.name == file_name && transfer.state.is_finished() && transfer.progress >= 1.0 {
                return true;
            }
        }
        false
    });

    if !download_finished {
        let state = handle_b.state();
        println!("Client B chat messages:");
        for msg in &state.chat_messages {
            println!("- [{}] {}", msg.sender, msg.text);
        }
        println!("Client B file transfers:");
        for transfer in &state.file_transfers {
            println!("- Name: {}, Progress: {}, State: {:?}",
                transfer.name, transfer.progress, transfer.state);
        }
    }
    assert!(download_finished, "Client B failed to finish download");

    // Verify file content on Client B side
    let downloaded_path = temp_dir_b.path().join(file_name);
    
    // Wait a bit for file system sync
    std::thread::sleep(Duration::from_millis(500));
    
    if !downloaded_path.exists() {
        // List dir to debug
        println!("Contents of {:?}:", temp_dir_b.path());
        for entry in std::fs::read_dir(temp_dir_b.path()).unwrap() {
            println!("{:?}", entry.unwrap().path());
        }
    }

    assert!(downloaded_path.exists(), "Downloaded file not found");
    let content = std::fs::read_to_string(downloaded_path).expect("failed to read downloaded file");
    assert_eq!(content, file_content, "File content mismatch");
}
