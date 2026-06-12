//! TX audio-processing pipeline microbenchmarks — the per-frame cost of the
//! capture-side processor chain (gain → noise gate → RNNoise denoise). Pure functions
//! of a PCM frame; no async runtime or device.
//!
//! `denoise_frame` isolates the dominant TX cost (RNNoise inference);
//! `tx_pipeline` measures the assembled chain so you can see what each stage
//! adds. Run: `cargo bench -p rumble-client --bench audio_pipeline`
//!
//! Baseline-gated workflow:
//!   cargo bench -p rumble-client -- --save-baseline before
//!   # ... optimize ...
//!   cargo bench -p rumble-client -- --baseline before

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rumble_client::{
    AudioPipeline, AudioProcessor, DenoiseProcessor, GainProcessor, NoiseGateProcessor, OPUS_FRAME_SIZE,
};

const SAMPLE_RATE: u32 = 48_000;

/// One 20 ms frame (960 samples @ 48 kHz) of a 440 Hz sine at -10 dBFS.
fn tone() -> Vec<f32> {
    (0..OPUS_FRAME_SIZE)
        .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SAMPLE_RATE as f32).sin() * 0.3)
        .collect()
}

fn build_pipeline(spec: &str) -> AudioPipeline {
    let mut p = AudioPipeline::new(OPUS_FRAME_SIZE);
    if spec.contains("gain") {
        p.add(Box::new(GainProcessor::new(6.0)));
    }
    if spec.contains("gate") {
        p.add(Box::new(NoiseGateProcessor::new()));
    }
    if spec.contains("denoise") {
        p.add(Box::new(DenoiseProcessor::new()));
    }
    p
}

fn bench_tx_pipeline(c: &mut Criterion) {
    let input = tone();
    let mut group = c.benchmark_group("tx_pipeline");
    for spec in ["gain", "gain+gate", "gain+denoise", "gain+gate+denoise"] {
        let mut pipeline = build_pipeline(spec);
        let mut buf = input.clone();
        group.bench_function(spec, |b| {
            b.iter(|| {
                // Refresh the in-place buffer each iter (process mutates it);
                // the 960-sample memcpy is dwarfed by denoise inference.
                buf.copy_from_slice(&input);
                black_box(pipeline.process(&mut buf, SAMPLE_RATE))
            });
        });
    }
    group.finish();
}

fn bench_denoise_frame(c: &mut Criterion) {
    let input = tone();
    let mut denoise = DenoiseProcessor::new();
    let mut buf = input.clone();
    c.bench_function("denoise_frame", |b| {
        b.iter(|| {
            buf.copy_from_slice(&input);
            black_box(denoise.process(&mut buf, SAMPLE_RATE))
        });
    });
}

criterion_group!(benches, bench_tx_pipeline, bench_denoise_frame);
criterion_main!(benches);
