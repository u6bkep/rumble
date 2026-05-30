# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Rumble is a voice chat application (similar to Discord/Mumble) written in Rust. Users can join hierarchical rooms and communicate via voice and text chat. The application uses a client-server architecture with QUIC transport, Ed25519 authentication, and Opus audio codec.

## Build Commands

```bash
cargo build                    # Build all crates
cargo run --bin server         # Run the server
cargo run -p rumble-aetna      # Run the GUI client
cargo test                     # Run all tests
cargo +nightly fmt             # Format code
RUST_LOG=debug cargo run -p rumble-aetna  # Run with debug logging
```

## Crate Architecture

```
            ┌─────────────────────────┐
            │   rumble-aetna (GUI)    │
            │  aetna-core / winit-wgpu│
            └───────────┬─────────────┘
                        │
                        ▼
            ┌─────────────────────────────────────────────┐
            │       rumble-desktop-shell                  │
            │  settings store, identity files (Argon2 +   │
            │  ChaCha20Poly1305), ssh-agent, global       │
            │  hotkeys, XDG GlobalShortcuts portal        │
            └─────────────────────┬───────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────┐
│              rumble-client (Client Library)                 │
│   BackendHandle with Arc<RwLock<State>>                     │
│   ┌─────────────────┐  ┌────────────────────┐               │
│   │ Connection Task │  │ Audio Task         │               │
│   │ - QUIC streams  │  │ - QUIC datagrams   │               │
│   │ - Protocol msgs │  │ - cpal I/O         │               │
│   │ - State sync    │  │ - Opus encode/dec  │               │
│   └─────────────────┘  └────────────────────┘               │
│   Platform trait via rumble-client-traits                   │
│   Desktop impl provided by rumble-desktop                   │
└───────────────────────┬─────────────────────────────────────┘
                        │
         ┌──────────────┼──────────────┐
         ▼              ▼              ▼
┌───────────────┐ ┌─────────────┐ ┌────────────┐
│rumble-protocol│ │ rumble-audio│ │   server   │
│    proto      │ │   audio     │ │  handlers  │
│    types      │ │   procs     │ │  state     │
└───────────────┘ └─────────────┘ └────────────┘
                                      ▲
                                      │ Bridge protocol
                               ┌──────┴──────┐
                               │mumble-bridge│
                               │ Mumble↔     │
                               │ Rumble proxy│
                               └─────────────┘
```

### Crate Responsibilities

- **rumble-protocol**: Protocol Buffers definitions (`proto/api.proto`), message framing, BLAKE3 state hashing, shared types (`State`, `Command`, `ConnectionState`, etc.)
- **rumble-client**: Client engine — QUIC connection, audio I/O, Opus codec, jitter buffers; depends only on `rumble-client-traits` (no platform code)
- **rumble-client-traits**: Platform-agnostic client traits (transport, audio, codec, keys, storage)
- **rumble-desktop**: Native desktop Platform implementation (quinn, cpal, opus, ed25519)
- **rumble-desktop-shell**: Shared shell-level concerns for the desktop client — persistent settings store, identity-file management (encrypted-at-rest via Argon2 + ChaCha20Poly1305), ssh-agent identity, cross-platform global hotkeys, XDG GlobalShortcuts portal.
- **rumble-audio**: Pluggable audio processor framework (denoise, VAD, gain control)
- **rumble-aetna**: GUI client. Built on aetna-core + aetna-winit-wgpu (vendored at `vendor/aetna/`, uses winit + wgpu directly). `App` impl projects `(state, ui_state) → El` tree per frame; `UiBackend` adapter wraps `BackendHandle`. Native SVG/icon support and color emoji.
- **rumble-video**: Thin safe wrapper over libmpv (player + software-render APIs) used by aetna's video lightbox.
- **server**: Server binary — room management, user auth, message relay, persistence (sled), ACL system
- **mumble-bridge**: Bidirectional bridge between Mumble and Rumble servers, proxying voice and chat

## Documentation

Authoritative subsystem docs live in `docs/`. Start with the overview, then drill in:

- **`docs/architecture.md`** — top-level map: crate roles, end-to-end data flow, and the core runtime patterns (state-driven UI, projection sole-writer, two-task client, lock-free server).
- **`docs/quic-protocol.md`** — the wire protocol: QUIC transport, protobuf `Envelope` framing, the Ed25519 auth handshake, state sync, and voice datagrams.
- **`docs/acl-system.md`** — permission bitflags, groups (incl. the implicit username-as-group), per-room ACLs, and the root→target evaluation algorithm.
- **`docs/audio-subsystem.md`** — Opus codec, the audio task, jitter buffers, the processor pipeline, and the per-peer decoder-lifetime invariant (see also below).
- **`docs/testing-strategy.md`** — server integration tests and the aetna `dump_bundles` lint/snapshot pipeline.
- **`docs/v2-architecture.md`** — *historical* design doc for the platform-trait decoupling; kept for rationale, not as a current API reference.

## Key Architecture Patterns

### State-Driven UI
The client exposes a shared `State` via `Arc<RwLock<State>>`. The UI reads state directly for rendering and sends fire-and-forget commands. Client updates state and calls repaint callback to notify UI.

### Two-Task Client Design
1. **Connection Task**: QUIC reliable streams for protocol messages and state sync
2. **Audio Task**: QUIC unreliable datagrams for voice, cpal streams for audio I/O

### Lock-Free Server
- `AtomicU64` for user ID generation
- `DashMap` for per-client lock-free access
- Single `RwLock<StateData>` for rooms/memberships
- Voice relay uses snapshots to avoid holding locks during I/O

### State Synchronization
Server sends incremental `StateUpdate` messages, each carrying a BLAKE3 hash of the post-apply state. The resync round-trip (`RequestStateSync` → full-state push) exists server-side, but the client does **not** yet recompute/verify the hash or trigger resync — client-side verification is a known gap, not a guarantee. See `docs/quic-protocol.md`.

## Protocol Details

- **Transport**: QUIC (quinn) - reliable streams for control, unreliable datagrams for voice
- **Serialization**: Protocol Buffers (prost) - see `crates/rumble-protocol/proto/api.proto`
- **Audio Format**: Opus at 48kHz, 20ms frames (960 samples)
- **Authentication**: Ed25519 signatures with optional SSH agent support
- **File Sharing**: Server relay (with plugin architecture for alternative backends)

## Audio: Opus Decoder Lifetime (important)

Each remote peer must have a **long-lived Opus decoder instance** that persists across talk spurts. It should only be dropped when the peer leaves the room/session (or after a very long TTL GC fallback). Re-initializing decoders per received packet/talkspurt will cause `rumble_client::codec: codec: decoder initialized` spam and audible crackle/pop at start of speech.

## Formatting

Uses `imports_granularity = "Crate"` in rustfmt.toml — group imports by crate.

## GUI Testing (rumble-aetna)

aetna has its own bundle/lint pipeline that replaces the old egui screenshot harness. The `dump_bundles` binary in `rumble-aetna` runs the real `App::on_event` path against a `MockBackend` returning canned `State`, then writes per-scene artifacts to `crates/rumble-aetna/out/` (gitignored):

```bash
cargo run -p rumble-aetna --bin dump_bundles                       # dump every scene
cargo run -p rumble-aetna --bin dump_bundles -- connected cert_pending  # specific scenes
cargo run -p rumble-aetna --bin dump_bundles -- --check             # diff vs checked-in goldens (exit 1 on drift)
cargo run -p rumble-aetna --bin dump_bundles -- --bless             # re-bless goldens after an intended UI change
```

Each scene produces `rumble_<scene>.{svg,tree.txt,draw_ops.txt,lint.txt,shader_manifest.txt}`. The SVG fallback renders the same draw-op stream as the wgpu Runner, so layout regressions are visible without spinning up a window or device. Lint findings (raw colors, overflow, weak focus, scrollbar overlap, etc.) land in `lint.txt` — review them before declaring a UI change done.

**Golden regression check.** `--check` re-renders every scene and diffs the deterministic subset (`draw_ops.txt` + `lint.txt`) against the checked-in goldens in `crates/rumble-aetna/goldens/` (tracked, unlike `out/`); it exits non-zero on any drift. Run it after touching UI code. When a change is intentional, run `--bless`, then review `git diff crates/rumble-aetna/goldens/` to confirm only the expected scenes moved. Goldens are pinned to the current git-pinned aetna rev — re-bless after an aetna bump. For new fixtures, keep them deterministic (no wall-clock or random keys): each scene renders against its own freshly-wiped config dir, and identity hooks install a fixed key.

To add a new scene, extend the `Scene` enum and `drive_setup` in `crates/rumble-aetna/src/bin/dump_bundles.rs`, then `--bless` to capture its goldens.

## External Dependencies (git-pinned)

Cargo consumes these from upstream GitHub at a pinned rev rather than crates.io. The `vendor/` directory is gitignored and holds local working copies for easy reference; Cargo does not consult it for builds.

- **aetna** — UI library powering `rumble-aetna`. Pinned in the root `Cargo.toml` `[workspace.dependencies]` block to a rev of `https://github.com/computer-whisperer/aetna`; consumers (`rumble-aetna`, `rumble-video`) inherit via `{ workspace = true }`, so bumping the rev is a single-line change. To iterate locally against `vendor/aetna` without touching tracked files, run cargo through `scripts/aetna-local.sh` (e.g. `scripts/aetna-local.sh build -p rumble-aetna`). It applies the gitignored `.cargo/local-aetna.toml` overlay as a `--config` patch and restores `Cargo.lock` afterwards, so CI and other machines stay on the git pin.
- **opus-rs** — Opus audio codec bindings, pinned via `opus = { git = "...", rev = "..." }` in `crates/rumble-desktop/Cargo.toml`.
