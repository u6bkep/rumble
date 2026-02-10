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
- **egui-test**: GUI client using egui with tree view for room hierarchy; also exports `TestHarness` for programmatic UI control
- **harness-cli**: Daemon-based CLI for automated GUI testing with screenshots and input injection

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

## GUI Test Harness (for agents and automated testing)

The `egui-test` crate is structured as both a library and binary, enabling programmatic control of the GUI for agents and integration tests.

### Architecture

```
crates/egui-test/src/
├── lib.rs           # Library exports: RumbleApp, TestHarness, Args
├── main.rs          # Thin eframe wrapper (human-facing app)
├── app.rs           # RumbleApp - core application logic
├── settings.rs      # Persistent settings types
├── key_manager.rs   # Ed25519 key management
└── harness.rs       # TestHarness for programmatic control
```

The key separation: `RumbleApp` contains all UI logic and is independent of eframe. The desktop app wraps it in an eframe runner, while tests/agents use `TestHarness` directly.

### Using TestHarness

```rust
use egui_test::{TestHarness, Args};

// Create harness with default settings
let mut harness = TestHarness::new();

// Or with custom args
let args = Args {
    server: Some("127.0.0.1:5000".to_string()),
    name: Some("test-bot".to_string()),
    ..Default::default()
};
let mut harness = TestHarness::with_args(args);

// Run frames to advance the UI
harness.run_frame();       // Single frame
harness.run_frames(10);    // Multiple frames

// Inject input events
harness.key_press(egui::Key::Space);   // Push-to-talk
harness.key_release(egui::Key::Space);
harness.click(egui::pos2(100.0, 200.0));
harness.type_text("Hello, world!");

// Introspect state
let connected = harness.is_connected();
let app = harness.app();  // Access RumbleApp directly
let backend_state = app.backend().state();  // Full backend state
```

### RumbleApp API

The core application exposes:

```rust
impl RumbleApp {
    /// Create with egui context, tokio handle, and CLI args
    pub fn new(ctx: egui::Context, runtime_handle: Handle, args: Args) -> Self;

    /// Render one frame (called by runner each frame)
    pub fn render(&mut self, ctx: &egui::Context);

    /// Access the backend handle for state/commands
    pub fn backend(&self) -> &BackendHandle;

    /// Check connection status
    pub fn is_connected(&self) -> bool;
}
```

### Writing Agent Tests

```rust
#[test]
fn test_agent_can_connect() {
    let mut harness = TestHarness::with_args(Args {
        server: Some("127.0.0.1:5000".to_string()),
        name: Some("agent".to_string()),
        trust_dev_cert: true,
        ..Default::default()
    });

    // Run frames to let connection establish
    harness.run_frames(100);

    // Check connection state
    assert!(harness.is_connected());

    // Interact with rooms, chat, etc. via backend
    let state = harness.app().backend().state();
    assert!(!state.rooms.is_empty());
}
```

## Harness CLI (for agent iteration loops)

The `harness-cli` crate provides a daemon-based CLI for automated GUI testing. Agents can use it to iteratively develop UI changes with screenshot feedback.

### Quick Start (Simplified)

```bash
# One command to start everything (daemon + server + client)
cargo run -p harness-cli -- up --screenshot /tmp/ui.png

# After making code changes, rebuild and screenshot in one command
cargo run -p harness-cli -- iterate -o /tmp/ui.png

# Clean teardown
cargo run -p harness-cli -- down
```

### Agent Iteration Workflow

```bash
# 1. Setup (one command)
rumble-harness up --screenshot /tmp/ui.png

# 2. Review screenshot, make code changes...

# 3. Rebuild and screenshot (one command)
rumble-harness iterate -o /tmp/ui.png

# 4. Repeat steps 2-3 until done

# 5. Cleanup
rumble-harness down
```

### Additional Commands

```bash
# Check what's running
rumble-harness status

# Manual interaction
rumble-harness client click 1 100 200
rumble-harness client type 1 "Hello"
rumble-harness client key-tap 1 enter

# Get backend state as JSON
rumble-harness client state 1
```

See [crates/harness-cli/README.md](crates/harness-cli/README.md) for full documentation.

## Audio: Opus decoder lifetime (important)

Each remote peer must have a **long-lived Opus decoder instance** that persists across talk spurts.
It should only be dropped when the peer leaves the room/session (or after a very long TTL GC fallback).
Re-initializing decoders per received packet/talkspurt will cause:
- `backend::codec: codec: decoder initialized` spam
- audible crackle/pop at start of speech (decoder state reset)

## P2P NAT Traversal Testing

Docker-based test environment for P2P connectivity with NAT simulation using libp2p.

### Location

```
docker/p2p-test/
├── src/
│   ├── test_node.rs    # Test node with DCUtR hole punching
│   └── relay_server.rs # Relay server for NAT traversal
├── run-test.sh         # Test runner script
├── docker-compose.yml  # Full NAT simulation environment
└── docker-compose.simple.yml  # Simple direct/relay tests
```

### Running Tests

```bash
cd docker/p2p-test

# Build Docker images
./run-test.sh build

# Run specific tests
./run-test.sh direct      # Direct connection (no NAT)
./run-test.sh relay       # Connection via relay circuit
./run-test.sh holepunch   # NAT hole punching with DCUtR

# Run all tests
./run-test.sh all

# Clean up containers
./run-test.sh cleanup

# Interactive mode (start relay, get instructions)
./run-test.sh interactive
```

### Network Topology (holepunch test)

```
                    ┌─────────────────┐
                    │     Relay       │
                    │   10.99.0.10    │
                    └────────┬────────┘
                             │ public-net (10.99.0.0/24)
            ┌────────────────┴────────────────┐
            │                                 │
     ┌──────┴──────┐                   ┌──────┴──────┐
     │   NAT-A     │                   │   NAT-B     │
     │ 10.99.0.20  │                   │ 10.99.0.30  │
     │ 10.99.1.2   │                   │ 10.99.2.2   │
     └──────┬──────┘                   └──────┴──────┘
            │ private-a (10.99.1.0/24)        │ private-b (10.99.2.0/24)
            │                                 │
     ┌──────┴──────┐                   ┌──────┴──────┐
     │   Node-A    │                   │   Node-B    │
     │ 10.99.1.10  │                   │ 10.99.2.10  │
     │ (sharer)    │                   │ (fetcher)   │
     └─────────────┘                   └─────────────┘
```

### What the Tests Do

1. **direct**: Node-A shares a file, Node-B connects directly (no NAT)
2. **relay**: Node-A listens via relay circuit, Node-B fetches through relay
3. **holepunch**: Both nodes behind NAT, DCUtR attempts hole punch, falls back to relay

### Expected Behavior

- With symmetric NAT (iptables MASQUERADE), hole punching will fail with "Connection refused"
- This is expected - symmetric NAT creates destination-specific port mappings
- The test succeeds via relay fallback: `FILE RECEIVED` with `HOLEPUNCH_FAILED`
- Successful hole punch shows: `HOLEPUNCH_SUCCESS` (requires endpoint-independent NAT)
