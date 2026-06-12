# Rumble Architecture Overview

Rumble is a Mumble/Discord-like voice + text chat application written in Rust. Users join a
hierarchical tree of rooms and talk over voice (Opus) and text chat. The stack is a QUIC
client/server split: a GUI client (`rumble-damascene`) drives a platform-agnostic client engine
(`rumble-client`), which speaks a protobuf-over-QUIC wire protocol (`rumble-protocol`) to the
`server` (rooms, members, ACL, sled persistence). `mumble-bridge` connects a real Mumble server
to a Rumble server as a *controller*. This document is the top-level map; subsystem depth is
deferred to the sibling docs listed at the end. Source lives under `crates/`.

---

## Crate Map (active crates)

| Crate | Role |
|---|---|
| `rumble-protocol` | Wire protocol: protobuf types (`proto/api.proto` → `proto` module), QUIC frame framing, BLAKE3 `compute_server_state_hash`, shared sidecar types (`ChatAttachment`). |
| `rumble-client` | Platform-agnostic client engine: QUIC connection task, audio task, Opus codec, jitter buffers, projection task; the `State` snapshot the UI reads. |
| `rumble-client-traits` | Platform-agnostic traits the engine depends on (`Transport`, `AudioBackend`, `KeySigning`, `FileTransferPlugin`, storage) — no platform code. |
| `rumble-desktop` | Native desktop `Platform` implementation: quinn (QUIC), cpal (audio I/O), opus, ed25519. |
| `rumble-desktop-shell` | Desktop shell concerns: persistent settings store, encrypted-at-rest identity files (Argon2 + ChaCha20Poly1305), ssh-agent identity, cross-platform + XDG-portal global hotkeys. |
| `rumble-audio` | Pluggable audio-processor framework (denoise, noise gate, gain control). |
| `rumble-damascene` | The GUI client, built on the damascene UI library; projects `(State, ui_state) → El` tree per frame, wraps `BackendHandle` via a `UiBackend` adapter. |
| `rumble-video` | Thin safe wrapper over libmpv (player + software-render) for damascene's video lightbox. |
| `server` | Server binary: room tree, member/identity roster, ACL evaluation, voice/chat relay, sled persistence. |
| `mumble-bridge` | Bidirectional Mumble↔Rumble proxy; registers Mumble clients as participants on the Rumble server via the controller protocol. |

---

## End-to-End Data Flow

```
  rumble-damascene (GUI)
    │   reads State each frame, sends Command (fire-and-forget)
    ▼
  rumble-client  ── BackendHandle: Arc<RwLock<State>> + command_tx
    │   ┌──────────────────┐   ┌────────────────┐   ┌──────────────────┐
    │   │ Connection Task  │   │  Audio Task    │   │ Projection Task  │
    │   │ reliable QUIC    │   │ unreliable     │   │ SOLE writer of   │
    │   │ streams (proto)  │   │ QUIC datagrams │   │ State; folds     │
    │   └──────┬───────────┘   └──────┬─────────┘   │ domain events    │
    │          │ typed domain events  │ (voice)     └──────▲───────────┘
    │          └──────────────────────┴────────────────────┘  (EventBus)
    ▼
  rumble-protocol  ── protobuf Envelope framing + BLAKE3 state hash
    │
    ▼
  server  ── DashMap<clients>, RwLock<StateData>{rooms, memberships, statuses},
    │         AtomicU64 ids, ACL eval, sled persistence
    ▲
    │ controller protocol (RegisterParticipant / MoveParticipant / ...)
  mumble-bridge ── proxies a real Mumble server's clients as Rumble participants
```

The client exposes a shared `State` via `Arc<RwLock<State>>` (`crates/rumble-client/src/events.rs`).
The UI reads it directly each frame for rendering and sends fire-and-forget `Command`s; the engine
applies state changes and invokes a repaint callback to nudge the UI.

---

## Core Runtime Patterns

### State-driven UI + incremental sync with BLAKE3 hash

- On connect the server sends a full `ServerState` (rooms, users, groups). Thereafter it pushes
  incremental `StateUpdate` messages, each carrying the BLAKE3 hash the client's state *should*
  have after applying the delta.
- Server side: `broadcast_state_update` in `crates/server/src/handlers.rs` rebuilds the
  `ServerState`, computes `compute_server_state_hash` (`crates/rumble-protocol/src/lib.rs`), and
  attaches it both to `StateUpdate.expected_hash` and the enclosing `Envelope.state_hash`.
- `compute_server_state_hash` sorts users by `user_id` before hashing so the digest is
  order-independent (`crates/rumble-protocol/src/lib.rs`).
- **Client-side verification is not currently wired.** The server attaches the hash and supports a
  resync round-trip (`RequestStateSync` → full-state push via `handle_request_state_sync`,
  `crates/server/src/handlers.rs`), but `rumble-client` does not recompute/compare the hash and never
  sends `RequestStateSync` (all client `state_hash` fields are sent empty). The hash is wired for
  server-pushed integrity and a future client check; treat client verification as a known gap, not a
  guarantee.

### Projection task — sole writer of `State`

- `crates/rumble-client/src/projection.rs` runs the projection task. It is the **only** writer of
  the client's `Arc<RwLock<State>>` in the active path.
- The connection and audio tasks never touch `State` directly; they translate inputs into typed
  per-domain events (`ChatEvent`, `VoiceEvent`, `ConnectionEvent`, `RoomEvent`, `TransferEvent` in
  `crates/rumble-client/src/domain_events.rs`) and emit them on an `EventBus` of
  `tokio::broadcast` channels (capacity `EVENT_BROADCAST_CAPACITY`, 1024).
- The projection `select!`s over every channel and folds events into the snapshot (CQRS-style: the
  event log is the source of truth, `State` is a cached read-model). This makes the single-writer
  invariant structural rather than convention.
- `Envelope` parsing in the connection task (`apply_state_update`, `crates/rumble-client/src/handle.rs`)
  converts `StateUpdate` deltas and `ServerState` into `RoomEvent`s
  (`RoomEvent::RoomAdded`, `RoomEvent::FullStateReplaced`, etc.) rather than mutating state inline.
- Resync-on-lag is a known TODO: `broadcast::RecvError::Lagged` is logged but not yet recovered
  (see module docs in `projection.rs`).

### Two-task client design

- **Connection task** — owns the QUIC connection lifecycle over *reliable streams*, sends/receives
  protobuf control messages, and hands a datagram-transport handle to the audio task on connect
  (`crates/rumble-client/src/handle.rs`).
- **Audio task** — owns *unreliable QUIC datagrams* for voice plus cpal capture/playback, runs the
  Opus encoder and a long-lived per-peer decoder, and maintains per-user jitter buffers keyed by
  sequence number (`crates/rumble-client/src/audio_task.rs`). It updates `talking_users` in `State`
  via voice events. (Opus decoder lifetime: one decoder per peer for the session — re-initializing
  per packet causes audible artifacts; see `CLAUDE.md`.)

### Lock-free server

`crates/server/src/state.rs` documents and implements the locking strategy:

- `next_user_id: AtomicU64` — lock-free user-ID allocation (starts at 1).
- `clients: DashMap` — lock-free per-client access for the connection/voice fan-out.
- `state_data: RwLock<StateData>` — a *single* lock consolidating `rooms`, `memberships`, and
  `user_statuses`, which avoids nested-lock deadlocks.
- The voice/datagram path takes snapshots (`snapshot_clients`) and releases locks before doing
  network I/O, so the audio relay never blocks on state mutations.

### Roster: Member / Identity / Binding

The server roster is one participant model. An `Identity` (`crates/server/src/state.rs`) is the
single home for `display_name`, ACL groups, verified username, and display marker; a connected
client and its roster `Member` share the *same* `Arc<Identity>`. This is what lets `mumble-bridge`
register Mumble clients as first-class participants over the controller protocol
(`RegisterParticipant` / `MoveParticipant` / `SetParticipantStatus` in `crates/mumble-bridge/src/bridge.rs`).

---

## Where to Read More

- `docs/quic-protocol.md` — wire protocol details: QUIC transport, protobuf envelopes/framing, auth handshake, message catalog.
- `docs/acl-system.md` — permission groups, per-room grant/deny ACLs, evaluation order, superuser/controller bootstrap.
- `docs/audio-subsystem.md` — Opus codec, datagram voice format, jitter buffers, audio-processor pipeline.
- `docs/testing-strategy.md` — test layout and the damascene `dump_bundles` UI lint/snapshot pipeline.
- `docs/v2-architecture.md` — *historical* platform-abstraction design doc (how the client was decoupled from `rumble-desktop` behind `rumble-client-traits`); kept for design rationale, not current API reference.
