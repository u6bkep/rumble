//! RX (receive) audio-path microbenchmarks: the jitter buffer and the per-tick
//! multi-speaker mix. These exercise the connection-independent seams extracted
//! from `audio_task.rs` (`UserAudioState`, `mix_peer_frames`) — no sockets, no
//! cpal, no playback sink.
//!
//! Run: `cargo bench -p rumble-client --bench rx_audio`
//!
//! A trivial `BenchDecoder` stands in for Opus so these isolate the buffer
//! bookkeeping, the per-frame `vec![0.0; 960]` allocation, and the mix-loop
//! scaling. Raw Opus decode cost is measured separately by the `opus_decode`
//! bench in rumble-desktop — add ~20 µs/frame mentally for the real decoder.
//!
//! Baseline-gated workflow:
//!   cargo bench -p rumble-client -- --save-baseline before
//!   # ... optimize ...
//!   cargo bench -p rumble-client -- --baseline before

use std::{collections::HashMap, hint::black_box};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rumble_client::{
    AudioDumper, OPUS_FRAME_SIZE,
    audio_task::{UserAudioState, mix_peer_frames},
};
use rumble_client_traits::VoiceDecoder;

/// Cheap stand-in for the Opus decoder: fills the output with a tiny ramp so
/// the mixer sees varied non-zero data, at negligible cost. Lets the bench
/// isolate buffer + mix overhead from codec time.
struct BenchDecoder;

impl VoiceDecoder for BenchDecoder {
    fn decode(&mut self, _data: &[u8], output: &mut [f32]) -> anyhow::Result<usize> {
        for (i, s) in output.iter_mut().enumerate() {
            *s = ((i & 0xff) as f32 / 255.0) * 0.1;
        }
        Ok(output.len())
    }
    fn decode_plc(&mut self, output: &mut [f32]) -> anyhow::Result<usize> {
        output.fill(0.0);
        Ok(output.len())
    }
    fn decode_fec(&mut self, _data: &[u8], output: &mut [f32]) -> anyhow::Result<usize> {
        output.fill(0.0);
        Ok(output.len())
    }
}

/// A fixed Opus-sized payload (size is all the cheap decoder cares about).
fn payload() -> Vec<u8> {
    vec![0u8; 80]
}

/// Drive `n` packets through one fresh buffer in a given arrival pattern,
/// draining a frame after each insert. `gap` marks every Nth sequence dropped
/// (0 = no loss). When `reorder` is set, adjacent packets arrive swapped.
fn run_stream(n: u32, gap: u32, reorder: bool) {
    let mut state = UserAudioState::new(BenchDecoder, 2);
    let mut frame = [0.0f32; OPUS_FRAME_SIZE];
    let mut seq = 0u32;
    while seq < n {
        if reorder && seq + 1 < n {
            // Adjacent pair arrives out of order.
            state.insert_packet(seq + 1, payload());
            state.insert_packet(seq, payload());
            black_box(state.decode_next_into(&mut frame));
            black_box(state.decode_next_into(&mut frame));
            seq += 2;
        } else {
            // Drop every `gap`-th packet to exercise the FEC/PLC paths.
            if gap == 0 || !seq.is_multiple_of(gap) {
                state.insert_packet(seq, payload());
            }
            black_box(state.decode_next_into(&mut frame));
            seq += 1;
        }
    }
}

fn bench_jitter_buffer(c: &mut Criterion) {
    const N: u32 = 64;
    let mut group = c.benchmark_group("jitter_buffer");
    group.throughput(Throughput::Elements(N as u64));
    group.bench_function("in_order", |b| b.iter(|| run_stream(N, 0, false)));
    group.bench_function("reordered", |b| b.iter(|| run_stream(N, 0, true)));
    group.bench_function("loss_10pct", |b| b.iter(|| run_stream(N, 10, false)));
    group.finish();
}

fn bench_rx_mix(c: &mut Criterion) {
    let dumper = AudioDumper::disabled();
    let mut group = c.benchmark_group("rx_mix_n_speakers");
    for &n in &[1u64, 5, 20] {
        // n peers, each pre-seeded so they're ready on the first tick.
        let mut peers: HashMap<u64, UserAudioState<BenchDecoder>> = HashMap::new();
        for uid in 0..n {
            let mut s = UserAudioState::new(BenchDecoder, 1);
            s.insert_packet(0, payload());
            peers.insert(uid, s);
        }
        let mut seq = 1u32;
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                // Steady state: one packet arrives per peer, then one mix tick.
                for s in peers.values_mut() {
                    s.insert_packet(seq, payload());
                }
                seq = seq.wrapping_add(1);
                black_box(mix_peer_frames(&mut peers, &dumper))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_jitter_buffer, bench_rx_mix);
criterion_main!(benches);

// Reference OPUS_FRAME_SIZE so an accidental frame-size change is visible here.
const _: () = assert!(OPUS_FRAME_SIZE == 960);
