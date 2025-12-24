# Plan: MVP with QUIC, Ed25519, Protobuf-Checksum State Hashing

Target: Three phases, but always QUIC (no TCP), Ed25519 identities, and state hashes as checksums over canonical protobuf serialization (excluding the hash field).

## Steps

1. **Phase 1 – QUIC Text-Only Vertical Slice**
   1. Convert `crates/api` to a proper library with `prost` and define minimal proto messages: `ClientHello`, `ServerHello`, `ChatMessage`, `ServerEvent::ChatBroadcast`, and a generic `Envelope` that already includes an optional `state_hash` field.
   2. In `crates/server/src/main.rs`, add `tokio`, `quinn`, and `rustls` (self-signed cert): create a `quinn::Endpoint` in server mode, accept connections, open a single bi-directional control stream per client, and use length-prefixed protobuf frames for all messages.
   3. In `crates/backend`, make a `Client` library that uses `quinn` to connect, opens the control stream, sends `ClientHello`, and exposes `send_chat()` plus an async event stream of `ServerEvent`s to consumers.
   4. Wire `crates/egui-test` to `backend`: integrate or spawn a `tokio` runtime, call `Client::connect` from the “Connect…” dialog, send chat via `send_chat`, and poll the event stream into the existing chat list.

2. **Phase 2 – Rooms, Voice Skeleton over QUIC, and State Hashing in Earnest**
   1. Extend `api` with room and voice messages: `Login` (still minimal), `JoinRoom`, `LeaveRoom`, `RoomState`, `VoiceFrame { room_id, user_id, opus_bytes }`, and new `ServerEvent` variants (`RoomStateUpdate`, `UserJoined`, `UserLeft`, `VoiceFrame`), all wrapped in the common `Envelope { payload, state_hash }`.
   2. Implement basic state-hash plumbing in `api`: implement the hash trait for all state structs (rooms/users), making sure to implement them in such a way as to be portable. the std defaulthasher is not considered portable accross targets or compiler versions.
   3. In `server`, add an in-memory `ServerState` (users, rooms, memberships) and operations: create default Lobby, handle `Login` (without real password yet), `JoinRoom`, `LeaveRoom`, and scoped `ChatMessage`/`VoiceFrame` relay to room members; recompute the protobuf-based state checksum on each mutation and include it in outgoing envelopes.
   4. In `backend`, extend `Client` to manage logical state: track current room, maintain a local room/user snapshot, expose room and user updates to callers, and include the latest known `state_hash` in any state-mutating requests.
   5. Add QUIC datagram-based voice skeleton in `backend` and `server`: for now, send dummy payloads or simple raw audio slices as `VoiceFrame` datagrams linked to room/user; relay on the server without decoding and feed to a placeholder playback path on the client.
   6. Introduce `cpal`-based audio engine in `backend`: implement `AudioEngine` that can start/stop capture and playback; initially wire it to produce simple fixed-size frames that are wrapped in `VoiceFrame` and sent as datagrams.
   7. Update `egui-test` to use live room state and voice controls: room selection mapped to `JoinRoom`, visual indication of current room and users, and a push-to-talk toggle/button that toggles the audio engine and sending of `VoiceFrame`s.

3. **Phase 3 – Ed25519 Identity, Global Password, and Persistence**
   1. Define authentication messages in `api`: `AuthRequest { client_public_key (Ed25519), username, password, state_hash? }` and `AuthResponse { success, user_id, reason?, state_hash }`, plus any supporting types for identities.
   2. In `backend`, integrate Ed25519 keypairs: choose `ed25519-dalek`, generate a keypair on first run and store it in a config directory, load it on startup, and include the public key (and optionally a signature over a challenge later) in `AuthRequest` along with the global password.
   3. In `server`, implement auth handling: load global password from config/CLI/env, validate `AuthRequest`, look up or create a user record keyed by the Ed25519 public key, assign a stable `user_id`, and include the resulting state (with checksum) in `AuthResponse` and subsequent messages.
   4. Add sled-backed persistence in `server`: define `StoredUser` (public key, username, roles) and `StoredRoom` (id, name, parent, minimal ACL fields), load them into `ServerState` on startup, and update sled whenever users or rooms change.
   5. Implement a minimal ACL scaffold in `api`/`server`: `Role` and simple `Permission` enums, with a default policy of “everyone allowed if password is correct”, but route all permission checks through a single function so future ACL logic only touches one place.
   6. Extend `egui-test` connect dialog and status UI: add password and username fields, display authenticated username and connection/auth status, and surface simple error messages for bad password or other auth failures.


note: please use cargo add to add dependencies so the latest versions are used.

## mid stage 2 cleanup

### test driven fixes
these tasks are to be done before moving on with phase 2 completion. before implementing any fixes, write a test that demonstrates the issue and, run it to confirm it fails. after implementing the fix, run the test again to confirm it passes.

each of these will will be handled as a seperate context, so leave notes here about what got done in each, and any relavent notes for later steps.

### opus encoding
we will leave opus encoding until after the mid stage 2 cleanup is done. 

### fix list
1. refactor the server to allow for better testing and isolation as you recommended.

   **DONE (2025-12-23)**
   - Created `crates/server/src/state.rs` with `ServerState` and `ClientHandle` types
     - `ServerState` now has proper methods: `allocate_user_id()`, `register_client()`, `remove_client()`, 
       `set_user_room()`, `get_user_room()`, `get_room_members()`, `create_room()`, `delete_room()`, 
       `rename_room()`, `get_rooms()`, `build_presence_list()`, `remove_user_membership()`
     - 8 unit tests covering state logic (room creation, deletion, membership, etc.)
   - Created `crates/server/src/handlers.rs` with protocol message handling
     - `handle_envelope()` dispatches to specific handlers for each message type
     - `handle_datagrams()` for voice relay
     - `broadcast_room_state()` and `cleanup_client()` helpers
   - Created `crates/server/src/server.rs` with `Server` struct and `Config`
     - `Server::new(config)` creates a server instance
     - `Server::run()` runs the accept loop
     - `Server::state()` provides access for testing
     - `handle_connection()` manages a single client connection
   - Updated `crates/server/src/lib.rs` to re-export all types and document the module structure
   - Slimmed `crates/server/src/main.rs` to ~20 lines - just parses config and calls `Server::new().run()`
   - All 19 integration tests pass, 8 new unit tests pass
   - **Notes for later steps:** The state module still uses multiple `Mutex` locks which will be 
     addressed in step 3. The `VoiceFrame` (stream-based) handler remains but should be removed 
     when we clean up duplicate message types in step 4.

2. clean up the interface between the gui and the backend.The BackendCommand and UiEvent enums are defined locally in main.rs. This creates:

Duplicate state tracking between UI and backend (e.g., RoomUiState vs RoomSnapshot)
The command/event pattern is ad-hoc rather than structured
Audio handling is mixed with UI state in MyApp
Recommendation: The backend crate should expose a proper event-driven API or callback system, rather than requiring the UI to manage channels and spawn tasks.

   **DONE (2025-12-23)**
   - Created `crates/backend/src/events.rs` with proper typed API:
     - `BackendCommand` enum: `Connect`, `Disconnect`, `SendChat`, `JoinRoom`, `CreateRoom`, 
       `DeleteRoom`, `RenameRoom`, `SendVoice`
     - `BackendEvent` enum: `Connected`, `ConnectFailed`, `Disconnected`, `StateUpdated`, 
       `ChatReceived`, `VoiceReceived`, `Error`
     - `ConnectionState` struct: single source of truth for connection state with helper methods
       like `users_in_room()`, `get_room()`, `is_user_in_room()`
   - Created `crates/backend/src/handle.rs` with `BackendHandle`:
     - Manages tokio runtime internally (no need for UI to spawn threads)
     - `new()` and `with_repaint_callback()` constructors
     - `send(command)` for non-blocking command dispatch
     - `poll_event()` for event polling in UI update loop
     - `state()` returns current cached `ConnectionState`
     - `command_sender()` returns clonable `CommandSender` for audio callbacks
   - Created `CommandSender` type for thread-safe command sending from audio callbacks
   - Updated `crates/backend/src/lib.rs` to export new types
   - Refactored `crates/egui-test/src/main.rs`:
     - Removed local `BackendCommand`, `UiEvent`, `RoomUiState` types
     - Removed `run_backend_task`, `handle_server_events`, `handle_voice_datagrams` functions
     - `MyApp` now uses `BackendHandle` instead of managing channels directly
     - Uses `CommandSender` for audio callback thread communication
     - UI reads state from `self.backend.state()` instead of duplicate fields
   - 10 unit tests for events and handle modules, all 37 tests pass
   - Added callback-based API for reactive/MVVM frameworks:
     - `BackendHandle::builder()` with `.on_event(callback)` and `.on_repaint(callback)`
     - `EventCallback` type alias: `Arc<dyn Fn(BackendEvent) + Send + Sync>`
     - `EventDispatcher` helper dispatches events to both channel and optional callback
   - **Notes:** The `BackendHandle` internally spawns a single background thread with tokio 
     runtime. The `CommandSender` is `Clone + Send + Sync` for use in audio callbacks. Both 
     polling and callback APIs work simultaneously - UI can use either or both. Callback API 
     enables easier porting to Android (LiveData/StateFlow), iOS (Combine), and WASM.

3. clean up the locks in the server. make whatever can be lock free, and use a single consolidated lock where not. remove blocking code from the audio path.

   **DONE (2025-12-23)**
   - Refactored `crates/server/src/state.rs` with improved locking strategy:
     - **`next_user_id`**: Now `AtomicU64` for lock-free ID allocation (was `Mutex<u64>`)
     - **`clients`**: Now `DashMap<u64, Arc<ClientHandle>>` for lock-free per-client access (was `Mutex<Vec<...>>`)
     - **`state_data`**: New consolidated `RwLock<StateData>` containing rooms + memberships (was separate `Mutex`es)
   - Updated `ClientHandle`:
     - Now has constructor `ClientHandle::new(send, user_id, conn)`
     - `username` uses `RwLock<String>` with `set_username()` and `get_username()` methods
     - Added `send_frame(&[u8])` helper method for sending framed messages
     - No longer `Clone` (was redundant due to `Arc` wrapper)
   - Updated all handlers in `crates/server/src/handlers.rs`:
     - `handle_chat_message`: Uses `snapshot_clients()` before iteration, no lock held during I/O
     - `handle_voice_frame`: Uses `snapshot_clients()` before iteration
     - `broadcast_room_state`: Uses `snapshot_clients()` before sending
     - `handle_datagrams`: Uses `snapshot_room_memberships()` for voice relay, lock-free client lookup via DashMap
     - `cleanup_client`: Uses `remove_client_by_handle()` (lock-free)
   - Updated `crates/server/src/server.rs`:
     - `allocate_user_id()` is now sync (no await needed)
     - `register_client()` is now sync
     - Uses `client_count()` instead of `clients.lock().await.len()`
   - New state methods for lock-free operation:
     - `register_client()`, `remove_client()`, `remove_client_by_handle()` (sync)
     - `get_client()`, `client_count()`, `for_each_client()`, `snapshot_clients()` (sync)
     - `snapshot_state()`, `snapshot_room_memberships()` (async, minimal lock time)
   - Added 4 new unit tests for the new locking infrastructure
   - All 32 tests pass (12 state tests + 19 integration + 1 doc test)
   - Added `dashmap` dependency to `crates/server/Cargo.toml`
   - **Notes:** The voice datagram path now takes a snapshot of room memberships once, then
     uses lock-free DashMap lookups to find recipients. This eliminates lock contention on
     the audio path. The `StateData` struct is `Clone` to enable efficient snapshots.

4. ~~clean up the api with regards to duplicate message types~~ **DONE**
   - **Removed:** `VoiceFrame` message entirely (was legacy stream-based voice)
   - **Removed:** `VoiceFrame` from `Envelope` payload oneof (field 19 reserved)
   - **Removed:** `VoiceFrame` from `ServerEvent` kind oneof (field 6 reserved)
   - **Removed:** `handle_voice_frame()` function from server handlers
   - **Restructured `VoiceDatagram`** with clear client/server field separation:
     - Fields 1-3 (client-provided): `opus_data`, `sequence`, `timestamp_us`
     - Fields 10-11 (server-set, optional): `sender_id`, `room_id`
   - **Security:** Client no longer sets sender_id or room_id; server determines these
     from the authenticated connection and current room membership
   - Client code simplified: no longer needs to look up user_id or room_id when sending
   - All 32 tests pass

5. ~~make the server hello the single source of truth for the client id~~ **DONE (2025-12-23)**
   - **Changed `Client::connect()`** to wait for `ServerHello` before returning:
     - Sends `ClientHello` and `JoinRoom` messages
     - Waits synchronously for `ServerHello` response
     - Extracts `user_id` from `ServerHello` - this is now the single source of truth
     - Only then spawns background tasks for event handling
   - **Simplified `Client` struct**:
     - `user_id` is now plain `u64` (was `Arc<Mutex<Option<u64>>>`)
     - New synchronous `user_id()` getter (was async `get_user_id() -> Option<u64>`)
     - Removed `set_user_id()` method (no longer needed)
   - **Removed duplicate `my_user_id` from `RoomSnapshot`**:
     - `RoomSnapshot` now only contains: `rooms`, `users`, `current_room_id`
     - User ID is accessed via `Client::user_id()` or `ConnectionState::my_user_id`
   - **Updated `handle.rs`**:
     - `run_backend_task` now uses `c.user_id()` directly (no await, no unwrap_or(0))
     - `handle_server_events` receives `user_id` as parameter (not from snapshot)
     - `BackendEvent::Connected` always has correct `user_id`
   - **Updated all tests**:
     - Removed 11 `set_user_id()` calls that were workarounds for the async issue
     - Tests now rely on server-assigned user_id via `client.user_id()`
     - Added 2 new tests: `test_server_assigns_user_id`, `test_multiple_clients_get_unique_user_ids`
   - All 21 integration tests pass
   - **Notes:** The flow is now: connect → wait for ServerHello → user_id available immediately.
     No race conditions, no polling, no Option handling needed.

6. ~~add graceful error recovery and reconnect logic on the client~~ **PARTIALLY DONE (2025-12-23)**
   - **Added `ConnectionStatus` enum** to `events.rs`:
     - `Disconnected`, `Connecting`, `Connected`, `Reconnecting { attempt, max_attempts }`, `ConnectionLost`
     - Helper methods: `is_connected()`, `is_connecting()`, `is_failed()`
   - **Added `ConnectionLostReason` enum** to `lib.rs`:
     - `StreamClosed`, `StreamError(String)`, `DatagramError(String)`, `ClientDisconnect`
   - **Added connection loss detection to `Client`**:
     - Reader task now signals via `connection_lost_tx` when stream read fails
     - Voice datagram task signals when datagram receive ends
     - New `take_connection_lost_receiver()` and `try_recv_connection_lost()` methods
   - **Added new `BackendEvent` variants**:
     - `ConnectionLost { error, will_reconnect }` - fired when connection unexpectedly lost
     - `ConnectionStatusChanged { status }` - fired on status transitions
   - **Updated `ConnectionState`**:
     - Added `status: ConnectionStatus` field
     - UI can check `state.status` for detailed connection state
   - **Updated `BackendHandle`**:
     - `apply_event()` now handles new events and updates status
     - New `connection_status()` method to get current status
     - `run_backend_task` monitors `connection_lost_rx` via `tokio::select!` with biased ordering
   - **Updated `egui-test`**:
     - Handles `ConnectionLost` and `ConnectionStatusChanged` events
     - Shows status messages in chat for connection state changes
   - **Added 2 new integration tests**:
     - `test_client_detects_server_disconnect` - verifies disconnect detection (30s QUIC timeout)
     - `test_connection_status_enum` - verifies status transitions
   - All tests pass (24 total: 12 server lib + 10 backend + 2 new integration)
   - **Not yet implemented**: Auto-reconnect with exponential backoff. The infrastructure
     is in place (last connection params are saved, `Reconnecting` status exists), but the
     actual reconnect logic is not yet implemented. UI can implement manual reconnect.

7. ~~add state hash verification on the client, trigger a resync if the hash does not match~~ **DONE (2025-12-23)**
   - **Implemented state hash computation** using `blake3` for portability:
     - `compute_room_state_hash()` in `crates/api/src/lib.rs`
     - Deterministic sorting of rooms and users before hashing
     - Uses canonical protobuf serialization for the RoomState
   - **Added `StateUpdate` proto message** for incremental updates:
     - `state_update_type` enum: `ROOM_CREATED`, `ROOM_DELETED`, `ROOM_RENAMED`, `USER_MOVED`, `USER_LEFT`
     - `StateUpdate` message with fields for each update type
     - `expected_hash` field for hash verification after applying update
   - **Added `RequestStateSync` proto message** for resync requests:
     - `expected_hash` and `actual_hash` fields for debugging mismatches
   - **Server sends state hash with all state updates**:
     - `send_room_state_to_client()` computes and includes hash in `RoomStateUpdate`
     - `broadcast_state_update()` helper sends incremental updates with hashes
     - `handle_request_state_sync()` handles resync requests by sending full state
   - **Client verifies hash and requests resync on mismatch**:
     - `RoomSnapshot.last_state_hash` tracks server's last known hash
     - `apply_state_update()` function applies incremental updates locally
     - Hash verification after full state updates and incremental updates
     - Automatic `RequestStateSync` sent when hash mismatch detected
     - `request_state_resync()` method for manual resync requests
     - `resync_count` counter for testing verification
   - **Fixed lock contention** in `request_state_resync()`:
     - Consolidated triple snapshot lock acquisition to single lock/unlock
   - **Added 4 new integration tests**:
     - `test_server_sends_state_hash` - verifies hash included in state updates
     - `test_client_computes_matching_state_hash` - verifies client hash matches server
     - `test_state_hash_mismatch_triggers_resync` - verifies mismatch detection
     - `test_incremental_updates_applied_correctly` - verifies incremental update application
     - `test_request_state_sync_infrastructure` - verifies resync request mechanism
   - All 28 integration tests pass
   - **Notes:** State hash enables detecting missed incremental updates. If hash mismatch
     occurs, client requests full state resync. The hash is computed over canonically
     sorted room/user data for determinism across different orderings.

8. ~~clean up room id usage~~ **PARTIALLY DONE (2025-12-24)**
   - **Changed room IDs from incrementing u64 to UUIDs**:
     - Added `uuid` crate (v1.19.0 with v4 feature) to api, server, backend crates
     - `RoomId` proto changed from `uint64 value` to `bytes uuid` (16 bytes)
     - `ROOT_ROOM_UUID` is `Uuid::nil()` (all zeros) - the permanent root room
   - **API crate UUID helpers**:
     - `room_id_from_uuid(uuid)` - creates RoomId proto from Uuid
     - `uuid_from_room_id(&RoomId)` - extracts Uuid from RoomId proto
     - `root_room_id()` - returns the root room RoomId
     - `is_root_room(&RoomId)` - checks if a room is the root
     - `new_room_id()` - generates a new random UUID v4 room ID
   - **Server updated**:
     - `StateData.memberships` now uses `Vec<(u64, Uuid)>` instead of `Vec<(u64, u64)>`
     - `create_room()` generates UUID v4 and returns `Uuid`
     - `delete_room()`, `rename_room()`, `set_user_room()`, `get_user_room()` use `Uuid`
     - Root room created with `ROOT_ROOM_UUID` on server start
   - **Backend updated**:
     - `RoomSnapshot.current_room_id` changed to `Option<Uuid>`
     - `BackendCommand::JoinRoom`, `DeleteRoom`, `RenameRoom` use `room_uuid: Uuid`
     - `ConnectionState.current_room_id` changed to `Option<Uuid>`
     - `users_in_room(uuid)`, `is_user_in_room(user_id, uuid)`, `get_room(uuid)` use Uuid
   - **UI updated**:
     - Room list uses `api::uuid_from_room_id()` to extract UUIDs
     - All room operations use `room_uuid` instead of `room_id`
     - Root room check uses `api::ROOT_ROOM_UUID`
   - **Tests updated**:
     - All 28 integration tests pass
     - All 22 unit tests pass
   - **Not yet implemented** (requires Phase 3 persistence):
     - Server remembering user's last room by public key
     - Placing users in their last room on reconnect (defaults to root for now)
   - **Notes:** The complete "remember last room" feature requires authentication
     (to identify users by public key) and persistence (to store last room). Currently
     all users are placed in ROOT_ROOM_UUID on connect.
9. fix unbounded buffer on audio transmission. gracefully handle slow connections. this will later be improved by varying opus bitrate based on network conditions, but for now just drop old frames if the network is slow.

   **DONE (2025-12-24)**
   - **Created `crates/backend/src/bounded_voice.rs`** with bounded voice channel implementation:
     - `BoundedVoiceSender<T>` and `BoundedVoiceReceiver<T>` types
     - `VoiceChannelConfig { max_frames }` configuration (default: 50 frames)
     - `VoiceChannelStats` for monitoring dropped frames
     - `try_send()` drops frames when channel is full (non-blocking)
     - Shared counters for `total_sent`, `dropped`, `received` statistics
   - **Updated `BackendHandle` and `CommandSender`** in `handle.rs`:
     - Added separate bounded channel for outbound voice frames
     - `CommandSender::send_voice()` uses bounded channel with drop-on-full semantics
     - `BackendHandle::voice_stats()` returns channel statistics
     - `BackendHandleBuilder::voice_buffer_capacity()` for custom buffer sizing
     - Voice commands still work through command channel for backwards compatibility
   - **Updated `Client` in `lib.rs`** for bounded voice receive:
     - Changed `voice_rx` from `UnboundedReceiver` to `Receiver` (bounded)
     - `VOICE_RECEIVE_BUFFER_SIZE = 100` frames (~500ms at 5ms, ~2s at 20ms)
     - Voice task uses `try_send()` to drop frames when receiver is slow
   - **Updated `AudioOutput` in `audio.rs`** for bounded playback buffer:
     - Added `MAX_PLAYBACK_BUFFER_SAMPLES = 48000` (1 second at 48kHz)
     - Added `max_buffer_size` and `dropped_samples` fields
     - `queue_samples()` now drops old samples when buffer exceeds limit
     - `with_max_buffer()` constructor for custom buffer sizing
     - `dropped_samples()` method to monitor dropped sample count
   - **All 3 unbounded audio paths are now bounded**:
     1. Outbound voice (audio capture → network): BoundedVoiceSender, 50 frames
     2. Inbound voice (network → playback event): tokio::mpsc::channel, 100 frames
     3. Playback buffer (samples → audio output): Vec with max 48000 samples
   - All 13 backend unit tests pass, all 28 server integration tests pass
   - **Notes:** When network is slow, frames are dropped at the earliest possible point
     to minimize memory growth. Statistics are available for debugging/monitoring.
     Future enhancement: adaptive Opus bitrate based on drop rate.