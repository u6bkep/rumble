# Architecture Revision

This document outlines architectural issues identified in the current codebase and the decisions made to address them. These changes have been incorporated into `implementation-plan.md`.

**Status: DECISIONS MADE - See implementation-plan.md for the updated architecture**

## Summary of Issues and Decisions

| Issue | Decision |
|-------|----------|
| Backend API blocking | Two-task architecture: connection task + audio task |
| Backend lifecycle coupling | State-driven API, backend runs independently |
| Redundant event variants | Removed polling, UI reads state directly |
| Audio transmission signals | Added VoiceStart/VoiceStop protocol messages |
| Audio mode limitations | Added TransmissionMode enum (PTT, Continuous, Muted) |
| API message redundancy | Separated User from RoomMembership in protocol |
| UI integration | Callback-based repaint, no event polling |
| Reconnection | UI handles reconnection, not backend |

## Detailed Decisions

### 1. Backend Architecture: Two-Task Design

**Problem**: `BackendHandle::send()` blocked on network I/O, freezing the UI.

**Solution**: Two independent background tasks:
- **Connection Task** (tokio runtime): Manages QUIC connection, protocol messages
- **Audio Task** (dedicated thread): Manages cpal streams, Opus codec, jitter buffer

These communicate via channels. Neither blocks the other.

### 2. State-Driven API

**Problem**: UI polled for individual events, leading to complex event handling.

**Solution**: 
- Backend exposes `State` via `Arc<RwLock<State>>`
- UI calls `backend.state()` to get current state snapshot
- Backend calls `repaint_callback()` when state changes
- UI re-renders from the new state
- No event polling, no event matching

### 3. Audio Transmission Modes

**Problem**: Only PTT was supported.

**Solution**: `TransmissionMode` enum:
- `PushToTalk`: Only transmit while PTT held
- `Continuous`: Always transmitting when connected
- `Muted`: Never transmitting

VAD will be added to Continuous mode in the future.

### 4. Voice Transmission Signals

**Problem**: No decoder state management, no jitter buffering.

**Solution**: Protocol messages `VoiceStart` and `VoiceStop`:
- Sent when user begins/ends transmission
- Server relays to room members
- Receivers can initialize decoder, start jitter buffer, show UI indicator

### 5. Reconnection Handling

**Problem**: Unclear who handles reconnection.

**Decision**: **UI handles reconnection**.
- Backend transitions to `ConnectionLost` state
- UI decides whether to show error, prompt user, or auto-reconnect
- Keeps backend simpler, gives UI full control

### 6. API Message Cleanup

**Problem**: `UserPresence` bundled user identity with room membership.

**Solution**: Separate concerns:
- `User { user_id, username }` - identity
- `RoomMembership { user_id, room_id }` - location
- `ServerState` contains lists of both

This also simplifies incremental updates (e.g., `UserMoved` only needs `user_id` and `to_room_id`).

---

## Implementation Order

1. **Refactor backend to state-driven API** - Remove event polling, add state struct
2. **Split into two tasks** - Connection task + Audio task
3. **Add TransmissionMode** - PTT, Continuous, Muted
4. **Update protocol** - Separate User/RoomMembership, add VoiceStart/VoiceStop
5. **Add jitter buffer** - Per-user buffering for voice playback
6. **Update egui-test** - Render from state, send commands

See `plan-minimumViableImplementation.disabled.md` for previous incremental work notes.
