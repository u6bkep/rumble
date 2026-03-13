# V2 Architecture Migration — Progress & Remaining Work

**Last updated:** 2026-03-12
**Design doc:** `docs/v2-architecture.md`

---

## Phase Summary

| Phase | Description | Status | Commits |
|-------|-------------|--------|---------|
| **1** | Move shared types to `api` | Done | `7468040` |
| **2a** | Define traits in `rumble-client` | Done | `2318033` |
| **3** | Implement `rumble-native` | Done | `e930c71`, `0158da6` |
| **4a** | Server plugin infrastructure | Done | `90c84df` |
| **4b** | Extract tracker into plugin | Done | `0a321ed` |
| **4c** | Plugin stream routing | Done | `a833c10` |
| **5a** | Datagram transport abstraction | Done | `e6effb8`, `b82419d` |
| **5b** | Full Transport integration | Done | `7b5485d` |
| **5c** | `BackendHandle<P: Platform>` | Done | `7b5485d` |
| **5d** | Switch `egui-test` to rumble-client | Done (via alias) | `7b5485d` |
| **5e** | Switch `mumble-bridge` to rumble-client | Done | `7b5485d` |
| **5f** | Backend dead code removal | Done | `7b5485d` |
| **5g** | Auth deduplication | In progress | — |
| **5h** | Extract torrent.rs to rumble-native | In progress | — |
| **5i** | File-transfer-relay plugin | In progress | — |
| **6** | WASM platform | Deferred indefinitely | — |

---

## Architecture Review (2026-03-12)

A thorough review of the v2 implementation against the design document was conducted across all six major components. Key findings:

### Trait Alignment

All 7 trait families in `rumble-client` match the design doc signatures precisely. Intentional enhancements beyond the design:
- **Transport split**: `DatagramTransport` + `TransportRecvStream` subtypes support the two-task architecture
- **TlsConfig extension**: `captured_cert` field enables interactive cert verification UX
- **FileTransferPlugin**: `download()` returns `TransferId` instead of `TransferHandle` (simpler API)
- **AudioBackend**: `Default` supertrait added for ergonomic construction in generic code

### rumble-native Implementations

All 6 Platform trait impls are complete with no stubs or `todo!()`. Test coverage: codec (5), storage (7), keys (4), transport (0), audio (0), cert_verifier (0).

### Server Plugin System

Complete and production-ready: `ServerPlugin` trait, `ServerCtx`, `StreamHeader`, stream dispatch, `FileTransferBittorrentPlugin`. Plugins get first look at messages before built-in handlers.

### Backend Generics

`BackendHandle<P: Platform>` fully generic. Zero `quinn::` references in handle.rs. Audio task generic over Platform. ~2,350 lines of dead code removed (audio.rs 698→16, codec.rs 674→36).

### Issues Identified

1. **Auth handshake duplication** — `send_envelope`, `wait_for_server_hello`, `wait_for_auth_result` duplicated between handle.rs and mumble-bridge. Bridge version also drops `groups` from `wait_for_auth_result` (bug). → Phase 5g
2. **TorrentManager not behind trait** — Still uses `dyn Any` downcast hack to get raw quinn::Connection. Needs extraction to rumble-native as `FileTransferPlugin` impl. → Phase 5h
3. **No file-transfer-relay plugin** — Design specifies simple relay as priority, but only BitTorrent plugin exists. The `on_stream()` dispatch infrastructure is unused. → Phase 5i
4. **p2p.rs still in backend** — Should move to rumble-native (feature-gated). Lower priority than torrent extraction.

---

## What's Done

### Phase 1 — Shared types in `api`

State, Command, AudioDeviceInfo, EncoderSettings, SigningCallback, and other shared types live in `api/src/types.rs`. Both `backend` and `egui-test` import from `api`.

### Phase 2a — Trait definitions in `rumble-client`

All trait families defined and exported:

| Trait | File | Methods |
|-------|------|---------|
| `Platform` | `platform.rs` | Bundle of 5 associated types |
| `Transport` + `DatagramTransport` + `TransportRecvStream` | `transport.rs` | connect, send/recv, datagram, take_recv, close |
| `AudioBackend` + streams | `audio.rs` | list devices, open input/output |
| `VoiceCodec` + encoder/decoder | `codec.rs` | encode, decode, PLC, FEC, settings |
| `PersistentStorage` | `storage.rs` | load, save, delete, list_keys |
| `KeySigning` | `keys.rs` | list_keys, get_signer, generate, import |
| `FileTransferPlugin` | `file_transfer.rs` | share, download, transfers, cancel |

### Phase 3 — `rumble-native` implementations

All Platform trait impls with `NativePlatform` bundle:

| Impl | File | Tests |
|------|------|-------|
| `QuinnTransport` + `QuinnDatagramHandle` | `transport.rs` | 0 |
| `CpalAudioBackend` + streams | `audio.rs` | 0 |
| `NativeOpusCodec` + encoder/decoder | `codec.rs` | 5 |
| `FileStorage` | `storage.rs` | 7 |
| `NativeKeySigning` (local + SSH agent) | `keys.rs` | 4 |
| `FingerprintVerifier` + `AcceptAllVerifier` + `InteractiveCertVerifier` | `cert_verifier.rs` | 0 |

### Phase 4 — Server plugin system

- **ServerPlugin trait** (`plugin.rs`): `on_message`, `on_stream`, `on_disconnect`, `start`, `stop`
- **ServerCtx**: send_to, broadcast_room, open_stream_to, state queries, persistence
- **StreamHeader**: u16 name_len + name + metadata wire format
- **Stream dispatch** (`server.rs`): secondary streams probed for StreamHeader, dispatched to matching plugin
- **FileTransferBittorrentPlugin** (`tracker_plugin.rs`): TrackerAnnounce + TrackerScrape handling

### Phase 5a-5f — Platform abstraction complete

- **5a**: `DatagramTransport` trait, audio_task decoupled from quinn
- **5b**: `Transport::send()/recv()` in handle.rs, `TransportRecvStream`, `TlsConfig` with `CapturedCert`
- **5c**: `BackendHandle<P: Platform>` fully generic, audio_task generic, type aliases
- **5d**: egui-test switch via `pub type BackendHandle = handle::BackendHandle<NativePlatform>`
- **5e**: mumble-bridge uses `QuinnTransport`, aws-lc-rs crypto
- **5f**: Removed ~2,350 lines dead code (AudioSystem, AudioInput, AudioOutput, VoiceEncoder, VoiceDecoder, CodecError). Removed cpal + opus deps from backend.

**Backend now contains only platform-agnostic client logic:**
- `handle.rs`: Generic `BackendHandle<P: Platform>` — connection task, command handling
- `audio_task.rs`: Generic audio task — jitter buffers, mixing, voice I/O
- `bounded_voice.rs`, `sfx.rs`, `synth.rs`: Pure Rust utilities
- `events.rs`, `processors/`, `rpc.rs`: State types, pipeline wrappers, RPC
- `audio_dump.rs`: Debug utility
- `torrent.rs`, `p2p.rs`: Awaiting extraction to rumble-native (Phase 5h)
- `cert_verifier.rs`: Thin re-export shim

---

## In Progress

### Phase 5g — Auth deduplication

Extract shared auth handshake helpers from handle.rs and mumble-bridge to `rumble-client/src/auth.rs`:
- `send_envelope<T: Transport>()`
- `wait_for_server_hello<T: Transport>()`
- `wait_for_auth_result<T: Transport>()` (bridge version was missing `groups` — bug fix)

### Phase 5h — Extract torrent.rs to rumble-native

- Move `backend/torrent.rs` to `rumble-native/src/torrent.rs`
- Wrap as `BitTorrentFileTransfer` implementing `FileTransferPlugin` trait
- Update `BackendHandle` to accept `Option<Box<dyn FileTransferPlugin>>` (remove `dyn Any` downcast hack)
- Move librqbit dependency from backend to rumble-native

### Phase 5i — File-transfer-relay plugin

Server-side:
- `relay_plugin.rs`: First plugin to use `on_stream()` dispatch
- Simple QUIC stream pipe between sender and recipient
- Proto messages for relay coordination

Client-side:
- `rumble-native/src/file_transfer_relay.rs` implementing `FileTransferPlugin`
- Uses QUIC streams tagged with StreamHeader "file-relay"

---

## Remaining Future Work

| Item | Priority | Notes |
|------|----------|-------|
| Move `p2p.rs` to rumble-native | Low | Feature-gated, not blocking |
| Remove remaining quinn deps from backend | Low | Blocked by torrent + p2p extraction |
| Wire `P::Storage` and `P::KeyManager` into BackendHandle | Low | Currently deferred nice-to-haves |
| WASM platform (Phase 6) | Deferred | Trait infrastructure ready when WASM threading stabilizes |

---

## Test Coverage

| Module | Tests | Notes |
|--------|-------|-------|
| `rumble-native/transport.rs` | 0 | Needs integration test with framing roundtrip |
| `rumble-native/audio.rs` | 0 | Hard to test without audio hardware |
| `rumble-native/cert_verifier.rs` | 0 | Should test fingerprint matching logic |
| `rumble-native/keys.rs` | 4 | Good coverage of local keys; SSH agent untestable in CI |
| `rumble-native/codec.rs` | 5 | Good coverage |
| `rumble-native/storage.rs` | 7 | Good coverage |
| `rumble-client` (traits) | 0 | Could add mock Platform impl for trait boundary testing |
