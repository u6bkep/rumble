//! End-to-end voice relay test.
//!
//! Spins up a real server and two backends, both using a `MockAudioBackend`
//! so the engine runs against fake mic/speaker streams instead of cpal.
//! Real Quinn transport + real Opus codec — the only thing replaced is the
//! audio device.

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::{Duration, Instant},
};

use ed25519_dalek::SigningKey;
use rumble_client::{Command as BackendCommand, ConnectConfig, events::VoiceMode};
use rumble_client_traits::test_audio::{MOCK_FRAME_SIZE, MockAudio, MockAudioBackend};
use tempfile::TempDir;

// =============================================================================
// Test platform: real Quinn + real Opus, mocked audio device
// =============================================================================

struct TestPlatform;

impl rumble_client_traits::Platform for TestPlatform {
    type Transport = rumble_desktop::QuinnTransport;
    type AudioBackend = MockAudioBackend;
    type Codec = rumble_desktop::NativeOpusCodec;

    fn create_file_transfer_plugin(
        _opener: Arc<dyn rumble_client_traits::StreamOpener>,
        _downloads_dir: PathBuf,
        _event_sink: Option<rumble_client_traits::PluginEventSink>,
    ) -> Option<Arc<dyn rumble_client_traits::FileTransferPlugin>> {
        None
    }
}

type TestBackend = rumble_client::handle::BackendHandle<TestPlatform>;

// =============================================================================
// Server / port helpers (same shape as backend_handle_integration.rs)
// =============================================================================

struct ServerGuard {
    child: Option<Child>,
    _temp_dir: TempDir,
    cert_path: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

static PORT_COUNTER: AtomicU16 = AtomicU16::new(58000);

fn next_test_port() -> u16 {
    PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn start_server(port: u16) -> ServerGuard {
    let temp_dir = TempDir::new().expect("temp dir");
    let cert_dir = temp_dir.path().join("certs");
    let data_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&cert_dir).expect("create cert dir");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .env("RUST_LOG", "info")
        .env("RUMBLE_NO_CONFIG", "1")
        .env("RUMBLE_PORT", port.to_string())
        .env("RUMBLE_CERT_DIR", cert_dir.to_str().unwrap())
        .env("RUMBLE_DATA_DIR", data_dir.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start server binary");

    if let Some(out) = child.stdout.take() {
        let p = port;
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                println!("[server:{p} stdout] {line}");
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let p = port;
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("[server:{p} stderr] {line}");
            }
        });
    }

    let cert_path = cert_dir.join("fullchain.pem");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cert_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    ServerGuard {
        child: Some(child),
        _temp_dir: temp_dir,
        cert_path,
    }
}

// =============================================================================
// Test signer
// =============================================================================

struct TestSigner {
    key: SigningKey,
}

#[async_trait::async_trait]
impl rumble_client_traits::KeySigning for TestSigner {
    async fn sign(&self, _public_key: &[u8; 32], payload: &[u8]) -> anyhow::Result<[u8; 64]> {
        use ed25519_dalek::Signer;
        Ok(self.key.sign(payload).to_bytes())
    }
}

fn build_test_signer() -> ([u8; 32], Arc<dyn rumble_client_traits::KeySigning>) {
    let key = SigningKey::from_bytes(&rand::random());
    let public_key = key.verifying_key().to_bytes();
    let signer: Arc<dyn rumble_client_traits::KeySigning> = Arc::new(TestSigner { key });
    (public_key, signer)
}

fn make_backend(cert_path: &std::path::Path) -> (TestBackend, MockAudio, [u8; 32]) {
    let (mock_backend, audio) = MockAudioBackend::new();
    // Dev cert SAN is "localhost" but tests dial by IP, so pin the leaf cert by
    // fingerprint (name-independent) rather than trusting it as a CA root.
    let pem = std::fs::read(cert_path).expect("read server cert");
    let leaf_der = rustls_pemfile::certs(&mut pem.as_slice())
        .next()
        .expect("at least one cert in fullchain.pem")
        .expect("parse leaf cert")
        .to_vec();
    let mut config = ConnectConfig::new();
    config.accepted_certs.push(leaf_der);
    let (public_key, signer) = build_test_signer();
    let handle = TestBackend::with_audio_backend(|| {}, config, signer, mock_backend);
    (handle, audio, public_key)
}

fn wait_for<F>(handle: &TestBackend, timeout: Duration, mut condition: F) -> bool
where
    F: FnMut(&rumble_client::State) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition(&handle.state()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

// =============================================================================
// Audio test helpers
// =============================================================================

/// Generate a 1 kHz sine frame of the given length at 48 kHz.
fn sine_frame(start_sample: usize, len: usize) -> Vec<f32> {
    let freq = 1000.0_f32;
    let sample_rate = 48_000.0_f32;
    (0..len)
        .map(|i| {
            let t = (start_sample + i) as f32 / sample_rate;
            0.5 * (2.0 * std::f32::consts::PI * freq * t).sin()
        })
        .collect()
}

/// Sum-of-squares energy of a slice. Bigger = more signal.
fn energy(samples: &[f32]) -> f32 {
    samples.iter().map(|s| s * s).sum()
}

// =============================================================================
// Tests
// =============================================================================

/// Two clients connect to a server, both join the root room. Client 1 streams
/// a sine into its mock mic; we verify Client 2's mock speaker actually emits
/// non-trivial audio.
#[test]
fn voice_relay_end_to_end() {
    let port = next_test_port();
    let server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle1, audio1, pk1) = make_backend(&server.cert_path);
    let (handle2, audio2, pk2) = make_backend(&server.cert_path);

    let addr = format!("127.0.0.1:{port}");
    handle1.send(BackendCommand::Connect {
        addr: addr.clone(),
        name: "client1".into(),
        public_key: pk1,
        password: None,
    });
    handle2.send(BackendCommand::Connect {
        addr,
        name: "client2".into(),
        public_key: pk2,
        password: None,
    });

    assert!(
        wait_for(&handle1, Duration::from_secs(5), |s| s.connection.is_connected()),
        "client1 should connect"
    );
    assert!(
        wait_for(&handle2, Duration::from_secs(5), |s| s.connection.is_connected()),
        "client2 should connect"
    );

    let user1_id = handle1.state().my_user_id.expect("client1 user id");
    let user2_id = handle2.state().my_user_id.expect("client2 user id");
    assert_ne!(user1_id, user2_id);

    // Both clients land in Root by default; confirm client2 sees client1.
    assert!(
        wait_for(&handle2, Duration::from_secs(2), |s| {
            s.users
                .iter()
                .any(|u| u.user_id.as_ref().map(|id| id.value) == Some(user1_id))
        }),
        "client2 should see client1 in user list"
    );

    // Switch client1 to continuous mode + unmute so audio flows without PTT.
    handle1.send(BackendCommand::SetVoiceMode {
        mode: VoiceMode::Continuous,
    });
    handle1.send(BackendCommand::SetMuted { muted: false });

    // Wait for the audio task to actually activate capture.
    assert!(
        audio1.wait_for_capture_active(Duration::from_secs(2)),
        "client1 capture should activate"
    );

    // The encoder discards the first 3 frames (`frames_to_discard` in
    // start_transmission) — push silence to flush, then push real audio.
    audio1.inject_silence(5);
    for i in 0..100 {
        audio1.inject_capture(sine_frame(i * MOCK_FRAME_SIZE, MOCK_FRAME_SIZE));
    }

    // Wait until client2 has received some opus packets from the relay.
    // Stats now ride a snapshot channel on the handle, not State, so
    // read them via `handle.stats()` (ignoring the State passed to the
    // condition closure).
    assert!(
        wait_for(&handle2, Duration::from_secs(3), |_s| handle2.stats().bytes_received
            > 200),
        "client2 should receive bytes from relay (got {})",
        handle2.stats().bytes_received,
    );

    // Give the audio task a few mix ticks to fill the playback buffer.
    std::thread::sleep(Duration::from_millis(300));

    // Pump 1 second of playback — should contain decoded sine, not silence.
    let played = audio2.pump_playback(48_000);
    let e = energy(&played);
    println!(
        "client2 playback energy: {e:.3} over {} samples (peak {:.3})",
        played.len(),
        played.iter().cloned().fold(0.0_f32, f32::max),
    );
    assert!(e > 1.0, "client2 playback should have non-trivial energy, got {e}");

    // Sanity: client1 transmitted, client2 received.
    let s1 = handle1.stats();
    let s2 = handle2.stats();
    assert!(s1.packets_sent > 0, "client1 should report packets_sent > 0");
    assert!(s2.packets_received > 0, "client2 should report packets_received > 0");
    println!(
        "stats — sent: {}, received: {}, bytes_received: {}",
        s1.packets_sent, s2.packets_received, s2.bytes_received,
    );
}

#[test]
fn live_input_reconfiguration_keeps_capture_active() {
    let port = next_test_port();
    let server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle, audio, pk) = make_backend(&server.cert_path);

    handle.send(BackendCommand::Connect {
        addr: format!("127.0.0.1:{port}"),
        name: "reconfig-sender".into(),
        public_key: pk,
        password: None,
    });

    assert!(
        wait_for(&handle, Duration::from_secs(5), |s| s.connection.is_connected()),
        "client should connect"
    );

    handle.send(BackendCommand::SetVoiceMode {
        mode: VoiceMode::Continuous,
    });
    assert!(
        audio.wait_for_capture_active(Duration::from_secs(2)),
        "capture should activate before reconfiguration"
    );

    handle.send(BackendCommand::UpdateAudioSettings {
        settings: rumble_client::AudioSettings::default(),
    });
    assert!(
        audio.wait_for_capture_active(Duration::from_secs(2)),
        "capture should reactivate after audio settings rebuild the stream"
    );

    handle.send(BackendCommand::SetInputDevice {
        device_id: Some("mock-input".into()),
    });
    assert!(
        audio.wait_for_capture_active(Duration::from_secs(2)),
        "capture should reactivate after input device changes"
    );
}

/// Sanity: with capture inactive (PTT mode, button not held), no datagrams
/// flow. This catches regressions where mute / PTT gating breaks.
#[test]
fn no_relay_when_ptt_not_held() {
    let port = next_test_port();
    let server = start_server(port);
    std::thread::sleep(Duration::from_millis(500));

    let (handle1, audio1, pk1) = make_backend(&server.cert_path);
    let (handle2, _audio2, pk2) = make_backend(&server.cert_path);

    let addr = format!("127.0.0.1:{port}");
    handle1.send(BackendCommand::Connect {
        addr: addr.clone(),
        name: "ptt-sender".into(),
        public_key: pk1,
        password: None,
    });
    handle2.send(BackendCommand::Connect {
        addr,
        name: "ptt-receiver".into(),
        public_key: pk2,
        password: None,
    });

    assert!(wait_for(&handle1, Duration::from_secs(5), |s| s
        .connection
        .is_connected()));
    assert!(wait_for(&handle2, Duration::from_secs(5), |s| s
        .connection
        .is_connected()));

    // Default mode is PushToTalk; no StartTransmit ⇒ capture stays inactive.
    handle1.send(BackendCommand::SetMuted { muted: false });

    // Even if we push frames, they should not be encoded/sent.
    for i in 0..50 {
        audio1.inject_capture(sine_frame(i * MOCK_FRAME_SIZE, MOCK_FRAME_SIZE));
    }
    std::thread::sleep(Duration::from_millis(500));

    assert_eq!(
        handle1.stats().packets_sent,
        0,
        "no packets should be sent without PTT held in PTT mode"
    );
    assert_eq!(handle2.stats().bytes_received, 0, "client2 should receive nothing");
}
