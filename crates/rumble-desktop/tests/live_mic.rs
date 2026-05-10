//! Live microphone smoke test.
//!
//! Marked `#[ignore]` because it depends on host audio: requires a running
//! PulseAudio / PipeWire-Pulse server and an actual mic that produces
//! non-silent samples. Run on demand to confirm that the desktop audio
//! backend can open the system default input and pull real PCM:
//!
//! ```bash
//! cargo test -p rumble-desktop --test live_mic -- --ignored --nocapture
//! ```

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rumble_client_traits::AudioBackend;
use rumble_desktop::DesktopAudioBackend;

#[test]
#[ignore = "requires a running Pulse/PipeWire server and a real microphone"]
fn live_default_mic_produces_samples() {
    let backend = DesktopAudioBackend::new();

    let inputs = backend.list_input_devices();
    println!("=== input devices ({}) ===", inputs.len());
    for d in &inputs {
        println!(
            "  default={:<5} id={:?}\n            name={:?}",
            d.is_default, d.id, d.name
        );
    }
    let default = inputs.iter().find(|d| d.is_default);
    assert!(default.is_some(), "no default input device — Pulse server reachable?");
    println!("\nopening default input (device_id=None) for 1.5 s...");

    let frames: Arc<Mutex<Vec<Vec<f32>>>> = Arc::new(Mutex::new(Vec::new()));
    let frames_for_cb = frames.clone();
    let stream = backend
        .open_input(
            None,
            Box::new(move |samples| {
                if let Ok(mut g) = frames_for_cb.lock() {
                    g.push(samples.to_vec());
                }
            }),
        )
        .expect("open default input");

    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(stream);

    let frames = Arc::try_unwrap(frames).unwrap().into_inner().unwrap();
    let total_samples: usize = frames.iter().map(|f| f.len()).sum();
    let energy: f64 = frames
        .iter()
        .flat_map(|f| f.iter().copied())
        .map(|s| (s as f64) * (s as f64))
        .sum();
    let peak: f32 = frames
        .iter()
        .flat_map(|f| f.iter().copied())
        .fold(0.0f32, |m, s| m.max(s.abs()));

    println!(
        "captured {} frames ({} samples, ~{:.0} ms)\n  energy={:.4}  peak={:.4}",
        frames.len(),
        total_samples,
        total_samples as f64 / 48.0,
        energy,
        peak,
    );

    assert!(!frames.is_empty(), "no frames received from input stream");
    assert!(
        total_samples >= 48_000 / 2,
        "expected at least ~500 ms of audio, got {} samples",
        total_samples,
    );
    assert!(
        peak > 1e-4,
        "input is digital silence (peak={peak:.6}). Mic muted at OS level, wrong source selected, or hardware problem."
    );
}
