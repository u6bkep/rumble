## Plan: MVP with QUIC, Ed25519, Protobuf-Checksum State Hashing

Target: Three phases, but always QUIC (no TCP), Ed25519 identities, and state hashes as checksums over canonical protobuf serialization (excluding the hash field).

### Steps

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