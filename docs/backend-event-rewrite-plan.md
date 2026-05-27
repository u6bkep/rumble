# BackendEvent rewrite — handoff plan

Status as of `f9ccc99` (2026-05-27). The rewrite is half-done: chat and
connection-lifecycle domains are converted, room/voice/transfer are
not. This document captures what's left and the gotchas a fresh
session needs to know.

## What this rewrite is doing

Old shape: the connection task and audio task both hold
`Arc<RwLock<State>>` and mutate it directly. State mutation is
scattered across ~240 sites in `crates/rumble-client/src/handle.rs`
and `crates/rumble-client/src/audio_task.rs`. `BackendEvent` (an
mpsc channel) exists as a side channel for toasts/transfer-stage
events; it's an afterthought, not the source of truth.

New shape: tasks emit typed per-domain events
(`ChatEvent`/`VoiceEvent`/`ConnectionEvent`/`RoomEvent`/`TransferEvent`)
on `tokio::broadcast` channels. A separate **projection task**
subscribes to all of them and is the sole writer of `State`. Tasks
become input → typed-event translators. External subscribers
(toast manager, SFX pump, future "what just happened" consumers)
attach via `BackendHandle::subscribe_*` to whichever domain they
care about.

Key files:
- `crates/rumble-client/src/domain_events.rs` — event enums (done)
- `crates/rumble-client/src/projection.rs` — projection task + `EventBus`
- `crates/rumble-client/src/handle.rs` — connection task; `BackendHandle::new` spawns projection
- `crates/rumble-client/src/audio_task.rs` — audio task (not yet wired to bus)
- `crates/rumble-client/src/lib.rs` — re-exports

## What's committed (done)

- `eaed0e4` — events + projection scaffolding. Projection task spawned
  by `BackendHandle::new`; subscribe_* methods on `BackendHandle`;
  tasks still mutate `State` directly.
- `f5c9f1c` — chat domain converted. Every `chat_messages.push` site
  in handle.rs now emits a `ChatEvent`. Projection's `apply_chat`
  is the sole writer of `state.chat_messages`. Includes the 100-entry
  trim and dedup-by-id logic.
- `f9ccc99` — connection-lifecycle fields converted. The projection
  is now the sole writer of `state.connection`, `state.my_user_id`,
  `state.my_session_public_key`, `state.my_session_id`,
  `state.kicked`, `state.permission_denied`.

Build, tests, and aetna's `dump_bundles` all green at `f9ccc99`.

## What remains

### 1. Room domain (the big one)

Affects: `state.rooms`, `state.users`, `state.group_definitions`,
`state.room_tree`, `state.my_room_id`, `state.effective_permissions`,
`state.per_room_permissions`.

Sites in `handle.rs`:
- `Command::Connect` success path — currently sets rooms/users/groups/
  my_room_id/room_tree directly after the `ConnectionEvent::Connected`
  emission. Should emit `RoomEvent::FullStateReplaced` +
  `RoomEvent::SelfMovedToRoom` (in that order on the same channel
  so ordering is preserved within the room broadcast).
- `Command::AcceptCertificate` success path — same shape.
- `Command::Disconnect` — currently clears rooms/users + rebuilds room
  tree directly. Replace with the projection's `Disconnected` handler
  doing the same. (Note: today's `ConnectionEvent::Disconnected`
  handler in `apply_connection` clears connection-identity fields but
  not rooms — extend it, or fire a separate `RoomEvent::FullStateReplaced
  { rooms: vec![], users: vec![], … }`.)
- Receiver task's "remote closed" cleanup at the bottom of
  `run_receiver_task` — same shape as Disconnect.
- All of `apply_state_update` (~290 lines, 13 variants) — port each
  `proto::state_update::Update::*` arm to emit the matching
  `RoomEvent` variant instead of mutating state. Variants are
  `RoomCreated → RoomAdded`, `RoomDeleted → RoomRemoved`,
  `RoomRenamed`, `RoomMoved`, `RoomDescriptionChanged →
  RoomDescriptionSet`, `UserJoined`, `UserLeft`, `UserMoved`,
  `UserStatusChanged → UserUpdated`, `GroupChanged →
  GroupAdded/Modified/Removed`, `UserGroupChanged → UserUpdated`,
  `RoomAclChanged`.

#### Audio-task side effects — the non-trivial bit

Several variants in `apply_state_update` send `AudioCommand`s to the
audio task as side effects of state mutation:

| RoomEvent triggers                          | AudioCommand sent                  |
| ------------------------------------------- | ---------------------------------- |
| `UserJoined` where user is in our room       | `UserJoinedRoom { user_id }`        |
| `UserLeft` where user was in our room        | `UserLeftRoom { user_id }`          |
| `UserMoved` where it's us moving              | `RoomChanged { user_ids_in_room }`  |
| `UserMoved` where someone joined our room    | `UserJoinedRoom { user_id }`        |
| `UserMoved` where someone left our room      | `UserLeftRoom { user_id }`          |
| `UserUpdated` where our `server_muted` flipped | `SetServerMuted { muted }`         |
| `FullStateReplaced` (on Connect)             | `RoomChanged { user_ids_in_room }` + `SetServerMuted` if applicable |

The projection task currently doesn't have an `AudioTaskHandle`. Options:

1. **Thread `AudioTaskHandle` into the projection.** Add to
   `spawn_projection_task` parameters and `BackendHandle::new`. The
   projection then sends `AudioCommand`s as part of its `apply_room`
   logic. Clean, but couples the projection to the audio task.

2. **Have the connection task subscribe to `RoomEvent` and handle
   side effects.** The projection writes state; the connection task
   (or a small sibling task) listens to `RoomEvent`s and sends
   `AudioCommand`s based on transitions. Decouples projection from
   audio, but adds a second `RoomEvent` consumer with subtle ordering
   relative to the projection.

3. **Emit a derived event the audio task subscribes to.** E.g.
   `VoiceEvent::RoomMembershipChanged { user_ids_in_room }` emitted
   by the projection when it sees room-relevant `RoomEvent`s.
   Cleanest separation but introduces an event ping-pong (room
   triggers voice).

Recommendation: **option 1** for now. The audio task is the natural
consumer of these side effects and the projection already knows the
state context. The alternative architectures are appealing but
this is pre-release — pick the simple thing and revisit if it
becomes painful.

#### `recalculate_effective_permissions` — port into projection

Currently at `handle.rs:2582`. Reads `state.users`, `state.rooms`,
`state.group_definitions`, `state.my_user_id`, `state.my_room_id`,
and writes `state.effective_permissions` and
`state.per_room_permissions`.

Move it into `projection.rs` as a private function. Call it from
`apply_room` after any of: `FullStateReplaced`, `RoomAdded`,
`RoomRemoved`, `RoomAclChanged`, `GroupAdded/Modified/Removed`,
`UserUpdated` (groups changed), `UserMoved` (when it's us moving).
Currently called inline in those handlers in `apply_state_update`.

`build_client_room_chain` (`handle.rs:2660`) is a helper — move it too.

#### Implementation order for room

1. Move `recalculate_effective_permissions` + `build_client_room_chain`
   into `projection.rs` (private fns). Keep the old ones in `handle.rs`
   working for now.
2. Add `AudioTaskHandle` parameter to `spawn_projection_task` and
   thread through `BackendHandle::new`.
3. Implement `apply_room` for all 13+ variants. The audio-task side
   effects fire from inside `apply_room` after the state mutation.
4. Convert the Connect/AcceptCertificate success paths to emit
   `RoomEvent::FullStateReplaced` + `RoomEvent::SelfMovedToRoom`.
5. Convert Disconnect/receiver-task-cleanup paths.
6. Rewrite `apply_state_update` body to emit events instead of
   mutating state. Most of the function becomes a `match` returning
   the matching `RoomEvent` and one `bus.room.send(...)`.
7. Delete the old `recalculate_effective_permissions` /
   `build_client_room_chain` from handle.rs once nothing calls them.

### 2. Voice domain (audio_task.rs)

Affects: every field under `state.audio.*`
(`self_muted`, `self_deafened`, `muted_users`, `is_transmitting`,
`talking_users`, `voice_mode`, `selected_input`, `selected_output`,
`input_devices`, `output_devices`, `stats`, `tx_pipeline`,
`rx_pipeline_defaults`, `per_user_rx`, `input_level_db`).

The audio task needs an `EventBus` (or just `broadcast::Sender<VoiceEvent>`).
`spawn_audio_task` / `AudioTaskConfig` needs a new field; thread it
through from `BackendHandle::new`.

Sites: ~90 `write_state(&state)` + `s.audio.*` mutations in
`audio_task.rs`. Each replaces with `bus.voice.send(VoiceEvent::*)`.
Categories:

- Talking detection: `UserStartedTalking`/`UserStoppedTalking` —
  fired from the voice packet receive loop and the silence-timeout
  branch.
- Mute/deafen toggles: handlers for `AudioCommand::SetMuted`,
  `SetDeafened`, `MuteUser`, `UnmuteUser`, `SetServerMuted` →
  `SelfMutedChanged`/`SelfDeafenedChanged`/`LocalMuteToggled`/
  `ServerMutedChanged`.
- `is_transmitting` updates from the capture loop →
  `TransmittingChanged`.
- Stats roll-up tick → `StatsUpdated`.
- Input level meter → `InputLevel`.
- Device enum / selection commands → `DevicesEnumerated`,
  `SelectedDeviceChanged`.
- Voice mode change → `VoiceModeChanged`.
- Pipeline config commands → `TxPipelineChanged`,
  `RxPipelineDefaultsChanged`, `UserRxConfigChanged`,
  `UserRxOverrideCleared`.

`apply_voice` in `projection.rs` mirrors these — straightforward field
updates. The two high-frequency events (`InputLevel`, `StatsUpdated`)
should keep their `trace!` log level inside `apply_voice` so they
don't flood debug logs.

### 3. Transfer domain (smallest)

The existing `plugin_event_sink` (`handle.rs:2016`) wraps
`PluginEvent::TransferStageChanged` into
`BackendEvent::TransferStageChanged` for the existing mpsc channel.
Add a parallel emission to `bus.transfer.send(TransferEvent::*)`.

Keep the existing mpsc path — it's the UX-notification channel and
nothing about that role changes. The `TransferEvent` broadcast is the
domain-event path for new subscribers that want the typed transfer
stream.

The projection's `apply_transfer` doesn't need to mutate `State` —
transfers are queried via `plugin.transfers()`, not stored in `State`.
The variant exists for external subscribers. Today's stub
`debug!`-only `apply_transfer` is correct end-state.

### 4. Final cleanup

After room + voice convert:

- Remove `state: Arc<RwLock<State>>` parameter from
  `run_connection_task`, `run_receiver_task`, `handle_server_message`,
  and audio task functions. They should no longer need read access
  to state — anything they need (e.g. `my_user_id` for sender-id on
  outgoing chat) can come from the captured context or a snapshot
  taken at the right time.
  - Caveat: some sites read state to make decisions (e.g.
    "was that user in our room?"). Those reads stay; only the writes
    move out.
- The projection task becomes the structural single-writer
  invariant: nothing in the codebase has a `state.write()` path
  except `projection.rs`. Grep `state.write()` and `write_state(` —
  after the conversion, only projection.rs should match.
- Drop the phase-2-in-progress doc comment on `BackendHandle.bus` and
  on `run_connection_task`'s `bus:` parameter.
- Move `recalculate_effective_permissions` and
  `build_client_room_chain` out of `handle.rs` (into `projection.rs`).

## Gotchas

### Broadcast channel ordering

Within a single domain channel, events arrive in send order
(`tokio::broadcast` is FIFO per sender). Across channels, no
guarantees. The projection's `tokio::select!` picks whichever channel
has a ready event first.

Concretely: a `ConnectionEvent::Connected` and the matching
`RoomEvent::FullStateReplaced` might be applied in either order. The
chat conversion already handles this — `Connected` carries
`session_public_key` and `session_id` so it's self-contained, and
`FullStateReplaced` doesn't depend on Connected having fired first.
For room conversion: don't have any room event depend on connection
event ordering. If `my_room_id` needs to be set from a `users` lookup,
do it inside `apply_room::FullStateReplaced` using the event's own
`users` payload + state's existing `my_user_id`, OR fire a separate
`RoomEvent::SelfMovedToRoom` event right after `FullStateReplaced` on
the same channel (preserves order).

### Repaint

Every successful event apply in the projection calls `repaint()`.
This is fine — repaint is idempotent (it just notifies the UI thread
to re-render). High-frequency events (`VoiceEvent::InputLevel`,
`VoiceEvent::StatsUpdated`) should consider not repainting per-event
to avoid burning UI thread cycles; the UI reads these via the live
state and a generic per-frame redraw covers them.

### `send` failures

`broadcast::Sender::send` returns `Err` if there are no subscribers.
The projection task is always subscribed while alive, so this should
not happen in practice. The convention in committed code is
`let _ = bus.chat.send(...);` — discard the result rather than
`.unwrap()`. Don't change to `.expect()` without thinking — a panic
in the connection task on a dropped projection would lose work in
ways that are awkward to debug.

### Lag handling

`broadcast::Receiver` returns `RecvError::Lagged(skipped)` if the
consumer fell behind. Today the projection logs and continues.
**Eventually** (post-phase-2) this should trigger a server resync
(`ServerState` re-request) because the projection's `State` is now
divergent from the event log. See `projection::on_lag`. Capacity is
1024 which should be ample; if it ever lags in practice, that's a
real bug.

### Testing

The flaky integration test we hit on the connection conversion
(`test_disconnect_clears_transmission_state` in
`crates/server/tests/backend_handle_integration.rs`) passed on
single-run but failed in the full-test-suite once. Both are
"wait for state, then assert" — they may need slightly longer
timeouts under load if the event indirection adds latency.

If a test starts failing on event indirection, prefer extending the
`wait_for` timeout over reverting the conversion. The async event
flow adds at most a few hundred microseconds on a non-pathological
runtime.

## Suggested next-session execution

A complete phase-2 finish in one session, if context allows:

1. Voice domain first (smallest non-trivial). Pattern is just
   "many sites, each a one-line replace". No new design questions
   beyond "thread bus through `AudioTaskConfig`."
2. Transfer next (forwarder change). 20-line edit.
3. Room last (biggest, requires the audio-task-handle design
   question — recommended option 1 above).
4. Final cleanup commit.

Each lands as its own commit. Build/test/fmt between them. The
deprecated egui crates need to stay buildable via `-p` (they don't
get rebuilt by default thanks to `default-members` from `d5ed152`).

If context runs short, room is the natural skip — it stands alone
as a final commit, the others are independent.
