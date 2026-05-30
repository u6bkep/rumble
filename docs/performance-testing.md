# Performance Testing & Optimization Plan

A plan for making rumble's performance-critical paths *safe to optimize* and
*measurable*.

**Status (2026-05-29):** the regression-test nets are in place for all four hot
paths (client audio, server state-sync, server relay, GUI goldens), and the
**server benchmark scaffold has landed**: `crates/server/benches/server_hotpaths.rs`
(criterion) covers `relay_fanout`, `state_rebuild`, `state_hash`, and `acl_eval`.
Run with `cargo bench -p server`. The audio and GUI benches below are not yet
written.

The premise: the workspace already has strong behavioral test coverage (~220
unit tests, 58 server integration tests, end-to-end voice over real Quinn+Opus
in `voice_relay.rs`). What's missing is two specific things that gate
optimization work:

1. **Measurement** — we cannot currently say where CPU/allocations go, or
   whether a change made things faster or slower. No `cargo bench`, no criterion,
   no allocation tracking.
2. **Characterization safety nets** — the existing tests assert *"it works,"* not
   *"it still produces the exact same output."* To rewrite a hot loop's buffer
   strategy or locking and *know* you didn't change observable behavior, you need
   golden-output tests that pin the current behavior precisely.

These are separate jobs and the doc treats them separately per subsystem.

---

## Tooling

### `cargo bench` and harnesses

`cargo bench` is the runner; it needs a *harness library* behind it.

- **Do not use the built-in `#[bench]` / `test::Bencher` harness.** It is
  nightly-only/unstable, gives a single mean with no statistics, no baseline
  comparison, and no allocation counting. Effectively abandoned.
- **Use a real harness via `harness = false`.** Add the harness as a
  dev-dependency, set `harness = false` on each `[[bench]]` target in
  `Cargo.toml`, and `cargo bench` runs it.

### Recommendation: criterion (primary), divan (selective)

**criterion** is the primary choice. Its decisive feature for this work is
baseline comparison:

```bash
cargo bench --bench audio_pipeline -- --save-baseline before
# ... make the optimization ...
cargo bench --bench audio_pipeline -- --baseline before
# criterion prints "change: -12.4% (p = 0.00 < 0.05)  Performance has improved."
```

That regression signal *is* the safety net for "broader optimization changes" —
it tells you with statistical confidence whether you helped or hurt, which the
bare harness cannot. criterion also handles warmup, outlier detection, and HTML
reports.

**divan** is worth adding selectively for the paths where the question is
*allocations per operation*, not wall-time — it has first-class allocation
counting (`#[divan::bench]` with `AllocProfiler`). Several hot paths below have
per-frame allocation smells (the `vec![0.0f32; 960]` per-decode, per-frame `El`
tree rebuild, per-recipient `bytes.clone()`). divan answers "how many bytes did
this allocate" directly. It's lighter and faster to run than criterion.

Suggested split: criterion for throughput/latency baselines and CI regression
gating; divan where the optimization target is allocation count specifically.
Start with criterion only; add divan if/when allocation counts become the focus.

### Workspace wiring

- Add to `[workspace.dependencies]`: `criterion = { version = "0.5", features =
  ["html_reports"] }` (and `divan = "0.1"` if/when used), so per-crate
  `Cargo.toml`s inherit a single pinned version.
- Each benched crate gets `[[bench]]` entries with `harness = false` and a
  `benches/` dir.
- Feature flags already make isolation feasible: `rumble-desktop`'s `codec` and
  `audio` features (Cargo.toml:7-21) let us bench Opus standalone, and
  `mumble-bridge` already builds with `default-features = false` (no cpal), so
  audio-free server benching is a proven configuration.

---

## Subsystem 1 — Client audio (per-20ms-frame path)

**Hot path:** `crates/rumble-client/src/audio_task.rs`, runs at 50 Hz
continuously during a call.

- **TX:** cpal callback → copy to scratch (audio_task.rs:1448) → processor
  pipeline (`rumble-audio` lib.rs:518) where **RNNoise denoise** does 2× ML
  inference per frame (denoise.rs:147) → Opus encode (audio_task.rs:1502) →
  protobuf encode → QUIC datagram.
- **RX:** every 20ms, *per speaking peer*: pop from `BTreeMap<u32, Vec<u8>>`
  jitter buffer → Opus decode / FEC / PLC (audio_task.rs:404/446/456) → RX
  pipeline → volume → mix (audio_task.rs:1666). **Scales with concurrent
  speakers.**
- **Allocation smells:** `vec![0.0f32; 960]` allocated per-decode, per-peer,
  per-frame (audio_task.rs:403 plus FEC/PLC fallbacks); playback buffer
  `Mutex<VecDeque<f32>>` locked per-sample in the output callback and held across
  the whole mix `extend` (audio_task.rs:1565, 1725).

### Benchmarks (criterion, optionally divan for allocs)

Locations: `crates/rumble-desktop/benches/opus_codec.rs` (Opus, needs the
`codec` feature) and `crates/rumble-client/benches/audio_pipeline.rs`
(processor chain). The codec/pipeline pieces are pure functions of input buffers
— no async runtime needed.

| Bench | Measures | Parameterize over | Status |
|-------|----------|-------------------|--------|
| `tx_pipeline` | TX processor chain per frame | gain / gain+vad / gain+denoise / full | **done** |
| `denoise_frame` | RNNoise inference alone | — (isolates the dominant TX cost) | **done** |
| `opus_encode` | Encode one 960-sample frame | VBR+FEC / CBR no-FEC | **done** |
| `opus_decode` | Decode + FEC + PLC paths | normal / FEC-recovery / PLC | **done** |
| `rx_mix_n_speakers` | Full RX decode+pipeline+mix tick | **N = 1, 5, 20** concurrent speakers | **TODO — needs extraction** |
| `jitter_buffer` | insert + get_next_frame | in-order / reordered / 10% loss | **TODO — needs extraction** |

First numbers (release, this machine): RNNoise `denoise_frame` ≈ **88 µs/frame**
— the dominant TX cost by ~3 orders of magnitude over gain (115 ns) and VAD
(870 ns), so `tx_pipeline/full` is essentially denoise. `opus_encode` ≈ 136 µs;
`opus_decode` normal/FEC ≈ 20 µs, PLC ≈ 67 ns. Both encode and denoise sit well
inside the 20 ms (20 000 µs) frame budget but are the clear hotspots.

`rx_mix_n_speakers` (the would-be headline bench — it measures whether the
per-frame `vec![0.0f32; 960]` allocations matter and how mixing scales with room
size) and `jitter_buffer` are **not yet written**: the jitter buffer struct and
the mix tick are private inside `audio_task.rs` (`jitter_buffer: BTreeMap<u32,
Vec<u8>>` + `get_next_frame`). Benching them needs the same kind of seam
extraction the relay path got — pull the per-peer playback state and the mix
tick into callable units. Deferred to its own pass.

### Characterization safety nets

The audio pipeline already has 60+ behavioral tests; what's missing for
optimization is **golden-output pinning**:

- **Codec round-trip determinism:** fixed input PCM → Opus encode → decode →
  assert the decoded PCM matches a checked-in golden (within a tight tolerance,
  since Opus is lossy but deterministic for fixed settings). Lets you swap buffer
  reuse / allocation strategy and prove the audio is unchanged.
- **Jitter buffer play-sequence:** scripted packet arrivals (reordered, gapped,
  duplicate, stream-restart) → assert the exact sequence of frames played and the
  exact stats counters (`packets_lost`, `packets_recovered_fec`,
  `frames_concealed`). This pins the trickiest state machine before anyone
  touches it.
- **Decoder-lifetime invariant** (already documented as critical in CLAUDE.md):
  a test asserting decoders are *not* recreated across talk spurts — guard
  against an optimization accidentally reintroducing per-packet decoder init.

---

## Subsystem 2 — Server scaling

**Two distinct paths**, `crates/server/src/handlers.rs`:

1. **Voice relay (per packet, × senders)** — `handle_datagrams`
   (handlers.rs:1716): decode → atomic mute/rate checks → `snapshot_room_memberships()`
   (clones membership under lock then releases — the *good* pattern,
   handlers.rs:1788) → `encode_to_vec()` once → **per-recipient `bytes.clone()`**
   send (handlers.rs:1843). ACL is deliberately *off* this path.
2. **State sync (per state change)** — the heavier concern. Every join/leave/
   mute/room op triggers a *full* state rebuild + per-user identity `RwLock`
   reads + full **BLAKE3 rehash** + per-client ACL eval on full sends
   (handlers.rs:1529, 1611). O(rooms × users) per change, paid on every bit of
   churn.

### Benchmarks (criterion)

The relay core and state-sync builders are the targets. Two integration styles:

- **Microbenchmarks** (`crates/server/benches/`, no network): call the relay/
  state-build functions directly against a synthetic state populated with N
  users / M rooms. The `server` crate already has a `lib.rs` exposing
  `server::state::{ServerState, Member, Identity, Binding, OwnerId}` and
  `server::handlers`, so benches reach internals directly (the integration tests
  spawn the real binary by choice, not necessity). Synthetic population uses
  public API with no QUIC: `ServerState::new()` → `register_participant` with
  `Binding::Owned { owner: OwnerId::Plugin(n) }` (owned participants need only an
  `Identity`, no connection) → `set_user_room` → `set_user_status`. See the
  refactoring note below for what's callable as-is vs. what needs extraction.

| Bench | Measures | Parameterize over | Status |
|-------|----------|-------------------|--------|
| `relay_fanout` | `build_relay_packet`: snapshot + room scan + re-encode + recipient gather | recipients = 10 / 50 / 200 | **done** |
| `state_rebuild` | full `build_user_list` + `build_room_list` projection | users × rooms grid (10×4, 50×8, 200×16) | **done** |
| `state_hash` | BLAKE3 over assembled `ServerState` | same grid | **done** |
| `acl_eval` | `evaluate_member_permissions` for one member | room-chain depth = 1 / 4 / 16 | **done** |

All four live in `crates/server/benches/server_hotpaths.rs`. The synthetic state
uses owned participants (`Binding::Owned { OwnerId::Plugin }`) — no connection.
`relay_fanout` resets the per-sender voice-rate window each iteration so a long
criterion sample doesn't trip the 32 KB/s limiter and start timing the
early-return path instead of the full relay. The lib + binary targets set
`bench = false` so `cargo bench -p server` dispatches only to the criterion
target (the libtest harness rejects criterion's `--save-baseline` flags).

`relay_fanout` quantifies the per-recipient clone cost; `state_rebuild` +
`state_hash` quantify the churn cost that dominates under many users.

#### Refactoring required (assessed)

Lighter than "refactor the relay" — most targets are callable as-is:

- **State-sync benches — no refactoring.** `build_user_list` (state.rs:862),
  `snapshot_room_memberships` (state.rs:910), `build_room_list` (state.rs:808),
  and `compute_server_state_hash` (state.rs:326) are already public and touch
  only `state_data`/`members`/persistence — no connection. Populate with owned
  participants per above and call directly. This is the dominant server cost and
  is fully benchable today.

- **Relay fan-out — extraction DONE (2026-05-29).** `handle_datagrams` was
  monolithic; the connection-independent middle (decode → sender resolution →
  mute/rate checks → snapshot → room scan → re-encode → recipient gather) is now
  `build_relay_packet(&ServerState, sender_user_id, VoiceDatagram, len) ->
  Option<RelayPacket{ bytes, recipients }>` (handlers.rs). It touches only
  `state` (the sender may be an owned participant, no connection needed), so it
  is directly callable from a bench and captures the bulk of per-packet CPU and
  allocations (snapshot clone + protobuf re-encode). The dedup-by-connection step
  is a separate pure `dedup_delivery_targets(recipients, resolve_fn)` whose
  injected resolver closure makes it unit-testable without a connection.
  Characterization tests landed in `handlers::tests` (8). Ready to bench as-is.

- **Per-recipient clone + actual send — not micro-benchable; use the load
  harness.** `ClientHandle.conn` is a concrete `quinn::Connection` (state.rs:209)
  with no trait abstraction, and `delivery_client` (state.rs:581) bottoms out at
  a `ClientHandle` with a real `conn`. Exercising the dedup-by-connection + clone
  + `send_datagram` tail therefore needs live clients — measure it in the
  loopback load harness (below), calling `handle_datagrams` unmodified.
  Abstracting the sink behind a trait to fake it would touch every `ClientHandle`
  construction and is a special-case test bypass we explicitly avoid; the clone
  itself (a ~100–300 B `Vec<u8>` × N) is trivially measurable in isolation if
  ever needed.

- **Load/throughput harness** (optional, later): the integration-test machinery
  in `crates/server/tests/` already spawns a real server and connects real
  `BackendHandle` clients. A bench that connects K clients to one room and
  streams voice, measuring relay latency/CPU at the server, would validate the
  microbench conclusions end-to-end. Heavier; propose after microbenches.

### Characterization safety nets

- **State-hash determinism & stability:** assert `compute_server_state_hash`
  is order-independent and stable across rebuilds for the same logical state —
  so an optimization that changes *how* state is assembled can't silently change
  the hash clients rely on.
- **Relay fan-out correctness:** for a synthetic room with controllers + plain
  members, assert the exact recipient set and that dedup-by-connection
  (`sent_to`, handlers.rs:1843) is correct. Pins behavior before optimizing the
  clone path.
- **ACL evaluation golden cases:** the 12 existing permission tests cover
  correctness; add a few fixed room-hierarchy + group fixtures with known
  expected permission bitmasks, so caching/restructuring ACL eval is provably
  equivalent.

---

## Subsystem 3 — GUI render (30 FPS full-tree rebuild)

**Hot path:** `crates/rumble-aetna/src/app.rs`. `main.rs:75` pins a **33ms
redraw interval**, so `build()` (app.rs:400-651) runs ~30×/sec **even when idle**.
Each call clones the full `State` (app.rs:402), clones the transfer list
(app.rs:405-407), and rebuilds the entire `El` tree from scratch — O(messages) +
O(rooms+users) + overlays — with no diffing, plus per-message `format!` and
markdown parsing.

Note: the continuous redraw is a *known workaround* — main.rs comments that
aetna "doesn't expose an event-loop wakeup hook today," so the 33ms poll stands
in for event-driven repaint. Any "redraw only on change" optimization is
therefore partly an upstream-aetna conversation (see the aetna-relationship
note), not purely a rumble change.

### Benchmarks (criterion, headless)

The existing `dump_bundles` path (`crates/rumble-aetna/src/bin/dump_bundles.rs`)
already drives the **real** `App::build` against a `MockBackend` with canned
`State`, headlessly, no GPU/window. A bench reuses that exact harness:

| Bench | Measures | Parameterize over |
|-------|----------|-------------------|
| `projection_build` | one `app.build(&cx)` call (tree construction) | **chat messages = 10 / 100 / 1000**, rooms/users |
| `projection_layout` | build + `render_bundle` (measure/layout pass) | same |

These answer: how much does idle 30 FPS cost on a busy server, and what's the
allocation cost per frame as chat/room counts grow. divan's allocator on
`projection_build` directly counts the per-frame `El`/`Vec`/`String` churn.

### Characterization safety nets

The GUI's safety net already exists in a different form — `dump_bundles` writes
`.tree.txt` / `.draw_ops.txt` / `.svg` golden artifacts per scene, which *are*
characterization snapshots of the projection output. For optimization work the
addition is: **promote those artifacts to checked-in goldens with a diff check**
(today they're gitignored output reviewed by eye). Then a projection optimization
that changes the rendered tree fails a snapshot diff instead of passing silently.
Add high-message-count scenes so the optimization target (busy chat) is covered.

---

## Cross-cutting

### Allocation tracking

For the three "per-frame/per-packet allocation" questions (`rx_mix_n_speakers`,
`relay_fanout`, `projection_build`), wall-time alone undersells the cost —
allocator pressure shows up as latency variance and GC-like stalls, not mean
time. This is where divan's `AllocProfiler` earns its place; alternatively
`dhat` as a dev-dependency for one-off heap profiling runs.

### Profiling (complementary to benches)

Benches tell you *if* something is slow; profiling tells you *where*. For the
deeper passes: `cargo flamegraph` (perf-based) on a running server under the load
harness, and on the client during a multi-speaker call. Not a CI artifact — a
manual investigation tool. Worth a `scripts/` helper once benches exist.

### CI integration

CI currently runs fmt + clippy only (not even `cargo test`). Options, lightest
first:
- **Manual baselines** (start here): developers run `cargo bench -- --baseline`
  locally before/after an optimization. No CI cost.
- **PR-triggered bench job** (later): run criterion against a saved baseline and
  comment the delta. Benches are noisy on shared CI runners — gate on a large
  threshold (e.g. >10%) to avoid false alarms, or run only on a `perf` label.

### A note on flakiness

The server integration tests already warn that real-process + real-QUIC tests
flake under concurrent load (testing-strategy.md). Any *load* bench inherits
this — keep load benches out of the default `cargo bench` run (separate
`--bench` target, not run by CI) so noise doesn't poison the microbench signal.

---

## Suggested sequencing

The subsystems are independent and can be done in any order; within each, benches
before optimization (evidence before theories), and characterization tests
before the optimization that would break them.

1. **Wire tooling** — add criterion to `[workspace.dependencies]`, prove the
   loop with *one* bench (suggest `opus_decode` or `denoise_frame` — pure, no
   refactor needed). Establish the `--save-baseline` workflow.
2. **Client audio benches** — `rx_mix_n_speakers` + jitter buffer + codec, since
   audio is the always-on hot path and needs no server refactor.
3. **Server benches** — state-sync benches need no refactor (call the public
   `ServerState` methods against owned-participant fixtures). The relay
   microbench wants one small extraction of the packet-build core out of
   `handle_datagrams`; the full per-recipient send path is deferred to the load
   harness. See "Refactoring required (assessed)" above.
4. **GUI benches** — reuse the `dump_bundles` harness; add busy-chat scenes.
5. **Characterization safety nets** — add per subsystem *before* optimizing that
   subsystem, not all up front.
6. **Optimize** — guided by baselines, with the safety nets catching behavior
   changes.
