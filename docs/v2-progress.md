# V2 Architecture Migration — Progress & Remaining Work

**Last updated:** 2026-03-11
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
| **5b** | Full Transport integration | Not started | — |
| **5c** | `BackendHandle<P: Platform>` | Not started | — |
| **5d** | Switch `egui-test` to rumble-client | Not started | — |
| **5e** | Switch `mumble-bridge` to rumble-client | Blocked | — |
| **5f** | Deprecate `backend` crate | Not started | — |
| **6** | WASM platform | Deferred indefinitely | — |

---

## What's Done

### Phase 1 — Shared types in `api`

State, Command, AudioDeviceInfo, EncoderSettings, SigningCallback, and other shared types live in `api/src/types.rs`. Both `backend` and `egui-test` import from `api`.

### Phase 2a — Trait definitions in `rumble-client`

All six trait families are defined and exported:

| Trait | File | Methods |
|-------|------|---------|
| `Platform` | `platform.rs` | Bundle of associated types |
| `Transport` + `DatagramTransport` | `transport.rs` | connect, send/recv, datagram, close |
| `AudioBackend` + streams | `audio.rs` | list devices, open input/output |
| `VoiceCodec` + encoder/decoder | `codec.rs` | encode, decode, PLC, FEC, settings |
| `PersistentStorage` | `storage.rs` | load, save, delete, list_keys |
| `KeySigning` | `keys.rs` | list_keys, get_signer, generate, import |
| `FileTransferPlugin` | `file_transfer.rs` | share, download, transfers, cancel |

The `Transport` trait was extended in Phase 5a with `type Datagram: DatagramTransport` and `datagram_handle()` to split reliable vs unreliable I/O for the two-task architecture.

### Phase 3 — `rumble-native` implementations

All Platform trait impls exist with `NativePlatform` as the bundle:

| Impl | File | Lines | Tests |
|------|------|-------|-------|
| `QuinnTransport` + `QuinnDatagramHandle` | `transport.rs` | ~230 | 0 |
| `CpalAudioBackend` + streams | `audio.rs` | ~640 | 0 |
| `NativeOpusCodec` + encoder/decoder | `codec.rs` | ~210 | 5 |
| `FileStorage` | `storage.rs` | ~200 | 7 |
| `NativeKeySigning` (local + SSH agent) | `keys.rs` | ~450 | 4 |
| `FingerprintVerifier` + `AcceptAllVerifier` | `cert_verifier.rs` | ~180 | 0 |

### Phase 4 — Server plugin system

- **ServerPlugin trait** (`plugin.rs`): `on_message`, `on_stream`, `on_disconnect`, `start`, `stop`
- **ServerCtx**: messaging (send_to, broadcast_room), state queries, `open_stream_to`, persistence
- **StreamHeader**: first frame on plugin-owned QUIC streams (u16 name_len + name + metadata)
- **Stream dispatch** (`server.rs`): secondary streams probed for StreamHeader, dispatched to matching plugin
- **FileTransferBittorrentPlugin** (`tracker_plugin.rs`): handles TrackerAnnounce + TrackerScrape, owns its own Tracker instance

### Phase 5a — Datagram transport abstraction

- `DatagramTransport` trait added to `rumble-client` (send_datagram + recv_datagram)
- `audio_task.rs` uses `Arc<dyn DatagramTransport>` instead of `quinn::Connection`
- `handle.rs` wraps `quinn::Connection` in `QuinnDatagramHandle` before passing to audio task
- Fixed framing bug: `QuinnTransport::send()` now uses varint prefix (matching `api::try_decode_frame`)

---

## Remaining Work

### Phase 5b — Integrate Transport trait into connection task

**Goal:** `handle.rs`'s `connect_to_server()` and `run_connection_task()` use `Transport::send()/recv()` instead of raw quinn streams.

**Current state:** `handle.rs` has 16 direct `quinn::` references:
- `make_client_endpoint()` (~70 lines): creates quinn Endpoint with rustls config
- `connect_to_server()` (~170 lines): QUIC handshake, auth, returns `(quinn::Connection, SendStream, RecvStream)`
- `run_connection_task()`: holds `Option<quinn::SendStream>`, calls `send.write_all(&encode_frame(...))`
- `run_recv_loop()`: holds `quinn::RecvStream`, reads frames
- Connection close: `conn.close(quinn::VarInt::from_u32(0), b"shutdown")`

**Blocker: Interactive cert verification.** The backend's `InteractiveCertVerifier` (in `cert_verifier.rs`) captures unknown certs via shared `Arc<Mutex<Option<ServerCertInfo>>>` state for UI prompts. This is deeply coupled to the connection setup:

1. `make_client_endpoint()` injects `InteractiveCertVerifier` into rustls config
2. Connection fails with `UnknownIssuer` → verifier stores cert in shared state
3. `run_connection_task()` detects cert error, puts `PendingCertificate` in UI state
4. User approves → retries with cert added to `ConnectConfig.accepted_certs`

The `Transport::connect()` trait has no way to express this interactive flow. Options:
- **A)** Extend `TlsConfig` with a captured-cert callback or channel
- **B)** Keep cert verification at the `BackendHandle` level — try connect, catch cert error, prompt user, retry with fingerprint in `TlsConfig.accepted_fingerprints`
- **C)** Move `InteractiveCertVerifier` into `rumble-native` and expose it through a platform-specific extension trait

**Recommended approach:** Option B. `Transport::connect()` fails normally on unknown certs. `BackendHandle` catches the error, extracts cert info from the error chain, prompts user, then retries with the fingerprint added to `TlsConfig.accepted_fingerprints`. This keeps the Transport trait clean and moves the UI-specific retry logic to the right layer.

**Work items:**
1. Make `QuinnTransport::connect()` work with `InteractiveCertVerifier` (or use `FingerprintVerifier` after user approval)
2. Refactor `connect_to_server()` to use `Transport::connect()` + auth handshake over `Transport::send()/recv()`
3. Replace `quinn::SendStream` usage in `run_connection_task()` with `Transport::send()`
4. Replace `run_recv_loop()` with `Transport::recv()` loop
5. Preserve connection close via `Transport::close()`

**Estimated scope:** ~500 lines changed in handle.rs, ~50 lines in transport.rs/QuinnTransport

### Phase 5c — Make `BackendHandle` generic over `Platform`

**Goal:** `BackendHandle<P: Platform>` with `type Handle = BackendHandle<NativePlatform>` alias.

**Depends on:** Phase 5b (Transport integration)

**Work items:**
1. Add `P: Platform` type parameter to `BackendHandle`, `run_connection_task`, `spawn_audio_task`
2. Replace `VoiceEncoder`/`VoiceDecoder` concrete types in `audio_task.rs` with `P::Codec` trait usage
3. Replace `AudioSystem`/`AudioInput`/`AudioOutput` in `audio_task.rs` with `P::AudioBackend`
4. Wire `P::Storage` for settings persistence (currently filesystem-only in egui-test)
5. Wire `P::KeyManager` for auth signing (currently in egui-test's `key_manager.rs`)
6. Accept `Option<Box<dyn FileTransferPlugin>>` in `BackendHandle::new()`

**Key challenge: audio model mismatch.** The backend's `AudioOutput` uses a push model (backend writes to a shared ring buffer, cpal reads from it). The `AudioBackend` trait's `open_output()` uses a pull model (callback fills buffer). `CpalAudioBackend` in rumble-native already implements the pull model. The backend's `audio_task.rs` will need adapting to match.

**Estimated scope:** ~300 lines in audio_task.rs, ~100 lines in handle.rs, ~50 lines in lib.rs

### Phase 5d — Switch `egui-test` to `rumble-client` + `rumble-native`

**Goal:** `egui-test` depends on `rumble-client` + `rumble-native` instead of `backend`.

**Current state:** `egui-test/Cargo.toml` has `backend = { path = "../backend" }`. No rumble-client/rumble-native deps.

**Depends on:** Phase 5c (generic BackendHandle)

**Work items:**
1. Add `rumble-client` + `rumble-native` deps to `egui-test/Cargo.toml`
2. Replace `use backend::BackendHandle` with `use backend::BackendHandle` (type alias to `BackendHandle<NativePlatform>`)
3. Move key management from `egui-test/key_manager.rs` to use `NativeKeySigning`
4. Move settings persistence to use `FileStorage`
5. Update imports throughout `app.rs`

**Estimated scope:** Mostly import changes. The `key_manager.rs` (~800 lines) refactor is the largest piece.

### Phase 5e — Switch `mumble-bridge` to `rumble-client`

**Goal:** Delete `mumble-bridge/src/rumble_client.rs` (430 lines of duplicated connection logic).

**Blocker: Crypto provider mismatch.**

| Crate | Crypto Provider | Quinn Feature |
|-------|-----------------|---------------|
| `backend` + `rumble-native` | `aws-lc-rs` | `rustls-aws-lc-rs` |
| `mumble-bridge` | `ring` | `rustls-ring` |

The bridge cannot use `QuinnTransport` from rumble-native without switching its crypto provider from `ring` to `aws-lc-rs`. This also affects its Mumble TLS server (uses `tokio-rustls` with ring).

**Work items:**
1. Switch mumble-bridge from `ring` to `aws-lc-rs` (change quinn/rustls/tokio-rustls features in Cargo.toml)
2. Verify Mumble TLS server still works with aws-lc-rs
3. Replace `mumble_client.rs` connect logic with `QuinnTransport::connect()` + shared auth helpers
4. Keep bridge-specific envelope helpers (`send_bridge_hello`, `send_bridge_register_user`, etc.) — these construct bridge-protocol messages, not duplicates of core logic
5. Use `Transport::send()` for reliable messages, `DatagramTransport` for voice relay

**Risk:** The `AcceptAnyCert` verifier in mumble-bridge bypasses ALL TLS verification (including signature verification). The `AcceptAllVerifier` in rumble-native properly delegates signature verification to the crypto provider. Behavior should be equivalent for the bridge use case but needs testing.

**Estimated scope:** ~200 lines deleted, ~50 lines changed, Cargo.toml feature swap

### Phase 5f — Deprecate `backend` crate

**Goal:** `backend` becomes a thin re-export shim or is deleted entirely.

**Depends on:** All other Phase 5 steps complete.

**What moves out of backend:**

| Module | Lines | Destination |
|--------|-------|-------------|
| `audio.rs` | 703 | Replaced by `rumble-native::CpalAudioBackend` |
| `codec.rs` | 701 | Replaced by `rumble-native::NativeOpusCodec` |
| `cert_verifier.rs` | 357 | `InteractiveCertVerifier` → `rumble-native` or stays in handle.rs |
| `handle.rs` | 3160 | Stays (becomes generic `BackendHandle<P>`) |
| `audio_task.rs` | 2040 | Stays (becomes generic over `P::Codec` + `P::AudioBackend`) |
| `torrent.rs` | 937 | → `rumble-native` as `FileTransferPlugin` impl |
| `p2p.rs` | 495 | → `rumble-native` (feature-gated) |
| `rpc.rs` | 414 | Stays (native-only RPC) |
| `events.rs` | 239 | Already mostly in `api/types.rs` |
| `bounded_voice.rs` | 251 | Stays (pure Rust, platform-agnostic) |
| `sfx.rs` + `synth.rs` | 447 | Stays (pure Rust) |
| `audio_dump.rs` | 261 | Stays (debug utility) |
| `processors.rs` | — | Stays (pipeline wrappers) |

After this, `backend` contains only platform-agnostic client logic (the "brain") — which is what `rumble-client` was supposed to be. At this point either:
- Rename `backend` → incorporate into `rumble-client`
- Or keep `backend` as the "wired-up" crate that combines `rumble-client` abstractions with platform-specific impls

### Phase 6 — WASM (deferred indefinitely)

No work planned. The trait infrastructure is ready for when WASM threading stabilizes.

---

## Test Coverage Gaps

| Module | Tests | Notes |
|--------|-------|-------|
| `rumble-native/transport.rs` | 0 | Needs integration test with actual framing |
| `rumble-native/audio.rs` | 0 | Hard to test without audio hardware |
| `rumble-native/cert_verifier.rs` | 0 | Should test fingerprint matching logic |
| `rumble-native/keys.rs` | 4 | Good coverage of local keys; SSH agent untestable in CI |
| `rumble-native/codec.rs` | 5 | Good coverage |
| `rumble-native/storage.rs` | 7 | Good coverage |
| `rumble-client` (traits) | 0 | Could add mock Platform impl for trait boundary testing |

**Recommendation:** Add at least a framing roundtrip test for `encode_frame_raw` / `try_decode_frame` to verify the wire format fix.

---

## Suggested Sprint Order

The remaining work has clear dependencies:

```
5b (Transport in handle.rs)
  └→ 5c (BackendHandle<P>)
       ├→ 5d (egui-test switch)
       └→ 5f (deprecate backend)

5e (mumble-bridge) — independent, blocked by crypto provider swap
```

**Recommended next sprint:** Phase 5b — integrate Transport trait into handle.rs. This is the hardest remaining piece and unblocks everything else. The interactive cert verification redesign (option B above) is the key design decision.
