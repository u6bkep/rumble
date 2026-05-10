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

When fixing build issues, run `cargo build -p rumble-aetna` and address the **first** error (later errors are often cascading).

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
┌──────────────┐ ┌─────────────┐ ┌────────────┐
│rumble-protocol│ │ rumble-audio│ │   server   │
│    proto     │ │   audio     │ │  handlers  │
│    types     │ │   procs     │ │  state     │
└──────────────┘ └─────────────┘ └────────────┘
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

### Deprecated Crates (reference only)

These are kept around as references when porting functionality forward. **Do not add features to them** unless explicitly asked.

- **rumble-egui** — egui-based GUI client; the most feature-complete pre-aetna implementation. Consult when porting flows (settings UI, file transfer, hotkey config, ACL editor) to aetna.
- **rumble-next** — egui-based testbed for a theming/visual-redesign pass on top of `rumble-widgets`. Useful as visual-design reference only.
- **rumble-widgets** — custom egui widget library (radio, toggle, slider, tree, combo box, Luna theme). Consumed only by `rumble-next`.
- **harness-cli** — daemon-based CLI for automating the egui client. Superseded by aetna's bundle-dump tooling (see GUI Testing below).

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
Server sends incremental `StateUpdate` messages with BLAKE3 hash. Client verifies hash after applying; requests full resync on mismatch.

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
```

Each scene produces `rumble_<scene>.{svg,tree.txt,draw_ops.txt,lint.txt,shader_manifest.txt}`. The SVG fallback renders the same draw-op stream as the wgpu Runner, so layout regressions are visible without spinning up a window or device. Lint findings (raw colors, overflow, weak focus, scrollbar overlap, etc.) land in `lint.txt` — review them before declaring a UI change done.

To add a new scene, extend the `Scene` enum and `drive_setup` in `crates/rumble-aetna/src/bin/dump_bundles.rs`.

## Vendored Dependencies

Located in `vendor/`. Used primarily for reference; code links against GitHub versions if modified from upstream.

- `aetna/` — UI library powering `rumble-aetna` (aetna-core + aetna-winit-wgpu + fonts/markdown crates).
- `opus-rs` — Opus audio codec bindings.
- `egui_ltreeview` — tree widget; used only by deprecated `rumble-egui`.
