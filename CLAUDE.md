# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Rumble is a voice chat application (similar to Discord/Mumble) written in Rust. Users can join hierarchical rooms and communicate via voice and text chat. The application uses a client-server architecture with QUIC transport, Ed25519 authentication, and Opus audio codec.

## Build Commands

```bash
cargo build                    # Build all crates
cargo run --bin server         # Run the server
cargo run -p egui-test         # Run the GUI client
cargo test                     # Run all tests
cargo +nightly fmt             # Format code
RUST_LOG=debug cargo run -p egui-test  # Run with debug logging
```

## Building egui-test (debugging tip)

When fixing build issues, run:
```bash
cargo build -p egui-test
```
and address the **first** error shown (later errors are often cascading).

## Crate Architecture

```
┌─────────────────────────────────────────────────────┐
│              egui-test (GUI Application)            │
│              Uses egui + eframe for UI              │
└───────────────────────┬─────────────────────────────┘
                        │ Commands / State reads
                        ▼
┌─────────────────────────────────────────────────────┐
│              backend (Client Library)               │
│   BackendHandle with Arc<RwLock<State>>             │
│   ┌─────────────────┐  ┌────────────────────┐       │
│   │ Connection Task │  │ Audio Task         │       │
│   │ - QUIC streams  │  │ - QUIC datagrams   │       │
│   │ - Protocol msgs │  │ - cpal I/O         │       │
│   │ - State sync    │  │ - Opus encode/dec  │       │
│   └─────────────────┘  └────────────────────┘       │
└───────────────────────┬─────────────────────────────┘
                        │
         ┌──────────────┼──────────────┐
         ▼              ▼              ▼
    ┌────────┐    ┌──────────┐   ┌────────────┐
    │  api   │    │ pipeline │   │   server   │
    │ proto  │    │ audio    │   │  handlers  │
    │ types  │    │ procs    │   │  state     │
    └────────┘    └──────────┘   └────────────┘
```

### Crate Responsibilities

- **api**: Protocol Buffers definitions (`proto/api.proto`), message framing, BLAKE3 state hashing
- **backend**: Client library - QUIC connection, audio I/O (cpal), Opus codec, jitter buffers
- **server**: Server binary - room management, user auth, message relay, persistence (sled)
- **pipeline**: Pluggable audio processor framework (denoise, VAD, gain control)
- **egui-test**: GUI client using egui with tree view for room hierarchy

## Key Architecture Patterns

### State-Driven UI
The backend exposes a shared `State` via `Arc<RwLock<State>>`. The UI reads state directly for rendering and sends fire-and-forget commands. Backend updates state and calls repaint callback to notify UI.

### Two-Task Backend Design
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
- **Serialization**: Protocol Buffers (prost) - see `crates/api/proto/api.proto`
- **Audio Format**: Opus at 48kHz, 20ms frames (960 samples)
- **Authentication**: Ed25519 signatures with optional SSH agent support
- **File Sharing**: BitTorrent

## Formatting

Uses `imports_granularity = "Crate"` in rustfmt.toml - group imports by crate.

## Vendored Dependencies

Vendored Dependencies are used primaruly for reference, with code being linked against github versions if modified from upstream.

Located in `vendor/`:
- `egui_ltreeview` - Tree view widget for room hierarchy
- `rqbit` - BitTorrent client (for file sharing feature)
- `torrust-tracker` - BitTorrent tracker

## Audio: Opus decoder lifetime (important)

Each remote peer must have a **long-lived Opus decoder instance** that persists across talk spurts.
It should only be dropped when the peer leaves the room/session (or after a very long TTL GC fallback).
Re-initializing decoders per received packet/talkspurt will cause:
- `backend::codec: codec: decoder initialized` spam
- audible crackle/pop at start of speech (decoder state reset)
