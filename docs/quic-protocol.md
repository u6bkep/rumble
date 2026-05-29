# Rumble QUIC Protocol Reference

This is the authoritative reference for the Rumble client↔server wire protocol as it
exists today. The protocol is QUIC + Protocol Buffers: reliable bidirectional streams
carry length-delimited protobuf control messages, and unreliable datagrams carry voice.
The protobuf schema lives in `crates/rumble-protocol/proto/api.proto` (package
`rumble.api.v1`), framing/hashing helpers in `crates/rumble-protocol/src/lib.rs`, the
desktop QUIC transport in `crates/rumble-desktop/src/transport.rs`, and the server-side
message handling in `crates/server/src/handlers.rs`.

## QUIC Transport

- **ALPN**: `"rumble"` (bytes `b"rumble"`). Set on the client in
  `rumble-desktop/src/transport.rs` (`client_cfg.alpn_protocols = vec![b"rumble".to_vec()]`)
  and on the server in `server/src/server.rs` (`rustls_config.alpn_protocols`).
- **TLS**: QUIC over TLS 1.3 (quinn). The client config forces
  `rustls::version::TLS13`. Client uses `with_no_client_auth` — application-layer Ed25519
  auth (below) provides identity, not mTLS.
- **Control channel**: a single reliable **bidirectional** stream. The client opens it,
  sends framed `Envelope` messages, and reads framed `Envelope` replies/broadcasts.
- **Voice channel**: QUIC **unreliable datagrams** carrying a single `VoiceDatagram`
  protobuf each (not wrapped in `Envelope`), for low-latency, loss-tolerant audio.
- **Idle timeout**: client transport config sets a 30s max idle timeout.

## Wire Framing

Reliable-stream messages are **length-delimited protobuf** (prost). Helpers in
`rumble-protocol/src/lib.rs`:

- `encode_frame(&M)` — prost `encode_length_delimited`: a protobuf varint length prefix
  followed by the encoded message body. `encode_frame_raw(&[u8])` does the same for
  already-serialized bytes.
- `try_decode_frame(&mut BytesMut)` — peeks the varint length delimiter, returns the
  frame body once `delimiter_len + len` bytes are buffered, else `None` (partial read).

```
[varint length][protobuf-encoded Envelope] [varint length][protobuf-encoded Envelope] ...
```

Voice datagrams are **not** length-prefixed: each datagram is exactly one
`VoiceDatagram.encode_to_vec()` and is decoded with `VoiceDatagram::decode(datagram)`.

## The Envelope

All reliable-stream messages are wrapped in `Envelope` (`api.proto`):

```proto
message Envelope {
  bytes state_hash = 1;     // BLAKE3 hash over canonical ServerState protobuf
  oneof payload { ... }
}
```

`state_hash` is populated by the server only on state-bearing messages (notably
`StateUpdate` and the initial `ServerState`); it is left empty (`Vec::new()`) on most
others (e.g. `ServerHello`, `AuthFailed`, `CommandResult`). The `payload` oneof variants
present in the proto, grouped by direction/role:

**Authentication (fields 10, 11, 15, 17)**
- `ClientHello client_hello = 10` (C→S)
- `ServerHello server_hello = 11` (S→C)
- `Authenticate authenticate = 15` (C→S)
- `AuthFailed auth_failed = 17` (S→C)

**Chat, status, room ops**
- `ChatMessage chat_message = 12` (C→S)
- `Disconnect disconnect = 13` (C→S graceful close)
- `ServerEvent server_event = 14` (S→C; see below)
- `JoinRoom join_room = 16` (C→S)
- `DirectMessage direct_message = 27` (C→S)
- `CreateRoom create_room = 20`, `DeleteRoom delete_room = 21`,
  `RenameRoom rename_room = 22`, `MoveRoom move_room = 25`,
  `SetRoomDescription set_room_description = 26` (C→S)
- `RequestStateSync request_state_sync = 23` (C→S resync trigger)
- `SetUserStatus set_user_status = 24` (C→S self mute/deafen)

**Registration (40-42)**
- `RegisterUser register_user = 40`, `UnregisterUser unregister_user = 41` (C→S)
- `CommandResult command_result = 42` (S→C generic success/failure response)

**ACL (80-92)**
- C→S: `KickUser = 80`, `BanUser = 81`, `SetServerMute = 83`, `Elevate = 84`,
  `CreateGroup = 85`, `DeleteGroup = 86`, `ModifyGroup = 87`, `SetUserGroup = 88`,
  `SetRoomAcl = 89`
- S→C: `PermissionDenied = 91`, `UserKicked = 92`

**Controller / Participant (70-76)** — an out-of-process extension (e.g. the Mumble
bridge) that mints "participants" with no QUIC connection of their own:
- C→S: `ControllerHello = 70`, `RegisterParticipant = 71`, `UnregisterParticipant = 72`,
  `MoveParticipant = 73`, `SetParticipantStatus = 74`, `ParticipantChat = 75`
- S→C: `ParticipantRegistered = 76`

`ServerEvent` (the S→C push wrapper) is itself a oneof (`server_event::Kind`):
`chat_broadcast = 1` (`ChatBroadcast`), `keep_alive = 2` (`KeepAlive`),
`server_state = 3` (`ServerState` full snapshot), `state_update = 7` (`StateUpdate`
incremental), `welcome_message = 8` (`WelcomeMessage` / MOTD),
`direct_message_received = 9` (`DirectMessageReceived`).

## Connection + Authentication Flow

Ed25519 challenge-response. The server processes `ClientHello` and `Authenticate` in
`handlers.rs` (`handle_client_hello`, `handle_authenticate`).

```
client                                   server
  │  open bidi control stream            │
  │ ── ClientHello ───────────────────►  │  validate pubkey (32B); check username not
  │    {username, public_key,            │  registered to a different key; check server
  │     password?}                       │  password for unknown keys (RUMBLE_SERVER_PASSWORD)
  │                                       │  generate 32B nonce, store PendingAuth
  │  ◄────────────── ServerHello ──────  │  {nonce, server_name, user_id (session-assigned)}
  │                                       │
  │ ── Authenticate ──────────────────►  │  verify timestamp within ±5min; rebuild
  │    {signature(64B),                  │  auth payload; verify_strict signature;
  │     timestamp_ms,                    │  check BANNED via ACL; verify session_cert
  │     session_cert}                    │  signature/expiry
  │                                       │  mark authenticated, load groups + verified
  │                                       │  username, choose initial room
  │  ◄──── ServerEvent{ServerState} ───  │  initial full state push (with state_hash)
  │  ◄──── (broadcast) UserJoined ─────  │  StateUpdate to all clients
  │  ◄──── ServerEvent{WelcomeMessage}─  │  if MOTD configured
```

**Signature payload** — `build_auth_payload` in `rumble-protocol/src/lib.rs`, 112 bytes
fixed-length, big-endian integers:

```
nonce(32) ‖ timestamp_ms(8) ‖ public_key(32) ‖ user_id(8) ‖ server_cert_hash(32)
```

`server_cert_hash` is SHA-256 of the server's DER TLS certificate (`compute_cert_hash`),
binding the auth to the specific TLS session. Signature is verified with
`VerifyingKey::verify_strict`.

**Session certificate** — `Authenticate.session_cert` is a `SessionCertificate`: an
ephemeral Ed25519 session key signed by the user's long-term key, for peer attestation in
future P2P flows. The server verifies `user_signature` over the body produced by
`build_session_cert_payload` (`session_public_key(32) ‖ issued_ms(8) ‖ expires_ms(8) ‖
device_len(u16) ‖ device_utf8`), checks `expires_ms > issued_ms` and not-expired, then
records a `SessionEntry`. The session id is BLAKE3 over the 32-byte session key
(`compute_session_id`). On any failure the server replies `AuthFailed{error}`, finishes
the stream, and closes the connection.

Unauthenticated connections: `handle_envelope` drops all non-auth payloads until
`sender.authenticated` is set (each handler arm early-returns if not authenticated).

## State Synchronization

State is the set of rooms, users, and permission groups. The canonical container is
`ServerState { repeated RoomInfo rooms; repeated User users; repeated GroupInfo groups }`.

**Hashing** — `compute_server_state_hash` (`rumble-protocol/src/lib.rs`): clones the
`ServerState`, sorts `rooms` by UUID bytes and `users` by `user_id` for determinism,
prost-encodes, and BLAKE3-hashes. `RoomInfo.effective_permissions` is per-client and is
**excluded from the hash** — the server computes the hash from a `ServerState` built
*before* filling in `effective_permissions` (`send_server_state_to_client`).

**Initial / full sync** — on auth (and on resync request) the server sends
`ServerEvent{ServerState}` with the matching `state_hash` set on the `Envelope`.

**Incremental updates** — `broadcast_state_update` sends `ServerEvent{StateUpdate}` to all
clients. `StateUpdate` carries `expected_hash` (the hash *after* applying) plus a `update`
oneof. Update variants (`state_update::Update`):

- `UserMoved = 10`, `UserJoined = 11`, `UserLeft = 12`
- `RoomCreated = 13`, `RoomDeleted = 14` (with `fallback_room_id`), `RoomRenamed = 15`,
  `RoomMoved = 17`, `RoomDescriptionChanged = 18`
- `UserStatusChanged = 16` (mute/deafen/server_muted/is_elevated)
- `GroupChanged = 20`, `UserGroupChanged = 21`, `RoomAclChanged = 22`

**Verify / resync** — the protocol is *designed* for the client to apply the update,
recompute the hash, and on mismatch send `RequestStateSync{expected_hash, actual_hash}`
(the two hashes are debug-only); the server responds by re-sending the full `ServerState`
(`handle_request_state_sync` → `send_server_state_to_client`). **As of today the client
does not implement this check**: `rumble-client` never recomputes/compares the hash and
never sends `RequestStateSync` — all client-side `state_hash` fields go out empty
(`Vec::new()`). The server-attached hash and the resync handler exist and work, but
client-driven verification is a known gap. `Envelope.state_hash` on a `StateUpdate`
mirrors `StateUpdate.expected_hash`.

## Voice Datagrams

`VoiceDatagram` (`api.proto`), sent as a raw QUIC datagram (no `Envelope`, no length
prefix):

```proto
message VoiceDatagram {
  bytes  opus_data    = 1;  // Opus frame (may be empty when end_of_stream)
  uint32 sequence     = 2;  // client-incremented, for ordering/jitter buffer
  uint64 timestamp_us = 3;  // capture time (optional; receiver may use arrival time)
  bool   end_of_stream= 4;  // sender stopped (PTT release / VAD / mute)
  optional uint64 sender_id = 10;  // set by server on relay
  optional bytes  room_id   = 11;  // set by server on relay (16-byte UUID)
}
```

- **Client → server**: only `opus_data`, `sequence`, `timestamp_us`, `end_of_stream` are
  meaningful. `sender_id`/`room_id` are ignored from ordinary clients — the server derives
  them from the authenticated connection.
- **Server relay** (`handle_datagrams` in `handlers.rs`): decodes the datagram, determines
  the sender's room from a membership snapshot, overwrites `sender_id` and `room_id`
  (server-authoritative), re-encodes, and `send_datagram`s to every other member of that
  room. Voice is rate-limited per user (`check_voice_rate`); if the sender is in no room,
  the datagram is dropped.
- **Controller exception**: for a controller connection, the server *trusts* the
  datagram's `sender_id` if it names a participant that controller drives
  (`is_participant_of`), so one bridge connection can relay voice for many participants.
  Recipients are resolved to their *delivery connection* (a participant's audio is sent to
  its controller) and deduped per connection so a controller receives each datagram once.

## File Relay (separate stream protocol)

File transfer does **not** use `Envelope` payloads (proto fields 94-99 are reserved). It
runs on dedicated QUIC streams identified by a stream-type discriminator byte, with
length-prefixed `RelayUpload` / `RelayUploadResponse` / `RelayFetch` / `RelayFetchResponse`
messages framing raw file bytes (see the `api.proto` "File Relay Messages" section and
`PluginStreamAck` admission-control pattern). Chat references a transfer via a
`ChatAttachment` sidecar on `ChatMessage`/`ChatBroadcast` (namespace
`rumble.file_transfer.relay`, payload = `RelayFileSharePayload`). The stream wiring lives
outside the proto and was not traced for this reference.
