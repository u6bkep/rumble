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

5. make the server hello the single source of truth for the client id
6. add graceful error recovery and reconnect logic on the client.
7. add state hash verification on the client, trigger a resync if the hash does not match.
8. clean up room id usage. the server should remember what room a user was in last and place them there when the client re connects. defaulting to the root. channels should use a UUID rather than an incrementing number, except root is always 0.
9. fix unbounded buffer on audio transmission. gracefully handle slow connections. this will later be improved by varying opus bitrate based on network conditions, but for now just drop old frames if the network is slow.