//! Opus codec microbenchmarks — the per-frame encode/decode cost on the voice
//! path. Pure functions of a PCM/packet buffer; no async runtime or device.
//! Requires the `codec` feature (on by default).
//!
//! Run: `cargo bench -p rumble-desktop --bench opus_codec`
//!
//! Baseline-gated workflow:
//!   cargo bench -p rumble-desktop -- --save-baseline before
//!   # ... optimize ...
//!   cargo bench -p rumble-desktop -- --baseline before

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rumble_client_traits::{
    VoiceCodec, VoiceDecoder, VoiceEncoder,
    codec::{EncoderSettings, OPUS_FRAME_SIZE, OPUS_MAX_PACKET_SIZE},
};
use rumble_desktop::NativeOpusCodec;

/// One 20 ms frame (960 samples @ 48 kHz) of a `freq` Hz sine at -10 dBFS.
fn tone(freq: f32) -> Vec<f32> {
    (0..OPUS_FRAME_SIZE)
        .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / 48_000.0).sin() * 0.3)
        .collect()
}

fn bench_opus_encode(c: &mut Criterion) {
    let pcm = tone(440.0);
    let mut group = c.benchmark_group("opus_encode");
    for (label, settings) in [
        (
            "vbr_fec",
            EncoderSettings {
                vbr_enabled: true,
                fec_enabled: true,
                ..Default::default()
            },
        ),
        (
            "cbr_nofec",
            EncoderSettings {
                vbr_enabled: false,
                fec_enabled: false,
                ..Default::default()
            },
        ),
    ] {
        let mut enc = NativeOpusCodec::create_encoder(&settings).unwrap();
        let mut out = vec![0u8; OPUS_MAX_PACKET_SIZE];
        group.bench_function(label, |b| {
            b.iter(|| black_box(enc.encode(black_box(&pcm), &mut out).unwrap()));
        });
    }
    group.finish();
}

fn bench_opus_decode(c: &mut Criterion) {
    // A single FEC-enabled packet to feed the decode paths.
    let pcm = tone(440.0);
    let mut enc = NativeOpusCodec::create_encoder(&EncoderSettings {
        fec_enabled: true,
        ..Default::default()
    })
    .unwrap();
    let mut packet = vec![0u8; OPUS_MAX_PACKET_SIZE];
    let n = enc.encode(&pcm, &mut packet).unwrap();
    packet.truncate(n);

    let mut group = c.benchmark_group("opus_decode");
    let mut out = vec![0f32; OPUS_FRAME_SIZE];

    // Each path gets its own long-lived decoder (the production invariant — a
    // decoder persists across a talk spurt, never re-created per packet).
    group.bench_function("normal", |b| {
        let mut dec = NativeOpusCodec::create_decoder().unwrap();
        b.iter(|| black_box(dec.decode(black_box(&packet), &mut out).unwrap()));
    });
    group.bench_function("plc", |b| {
        let mut dec = NativeOpusCodec::create_decoder().unwrap();
        b.iter(|| black_box(dec.decode_plc(&mut out).unwrap()));
    });
    group.bench_function("fec", |b| {
        let mut dec = NativeOpusCodec::create_decoder().unwrap();
        b.iter(|| black_box(dec.decode_fec(black_box(&packet), &mut out).unwrap()));
    });
    group.finish();
}

criterion_group!(benches, bench_opus_encode, bench_opus_decode);
criterion_main!(benches);
