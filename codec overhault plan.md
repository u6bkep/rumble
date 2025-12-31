# Codec Overhaul Plan

## Summary of Discrepancies

The implementation plan specifies a significantly different encoder/decoder lifecycle and DTX handling strategy than what's currently implemented. The key changes involve:

1. **Encoder lifecycle**: connection-scoped, not transmission-scoped
2. **Decoder lifecycle**: server-state-driven, not packet-arrival-driven
3. **DTX frame handling**: skip sending DTX frames with periodic keepalives
4. **Sequence numbering**: only increment on actual send

---

## 1. Encoder Lifecycle Changes

### Current Implementation (`audio_task.rs:920-950`)
- Encoder is created in `start_transmission()` and destroyed in `stop_transmission()`
- New encoder created for each PTT press or continuous mode activation
- Encoder dropped when transmission stops

### Required Changes
- **Create encoder on connection**, not transmission start
- **Keep encoder alive** for entire connection lifetime
- **Never reset** the encoder mid-session (remove `reset()` call on transmission restart)
- On transmission stop: encoder sits idle (allocated but not encoding)

### Files to Modify
- audio_task.rs: Move encoder creation to `ConnectionEstablished` handler
- audio_task.rs: Store encoder at task level, not in callback closure
- audio_task.rs: Pass encoder reference to capture callback instead of creating new one

---

## 2. DTX Frame Handling (TX Side)

### Current Implementation
- All encoded frames are sent immediately
- No DTX frame detection or suppression
- Sequence number increments for every frame

### Required Changes (Plan: lines 476-484)
After encoding, check if frame length ≤2 bytes (DTX silence frame):
- **Track time since last send**
- If DTX frame **and** last sent <400ms ago → **skip sending entirely**
- If DTX frame **and** last sent ≥400ms ago → **send as keepalive**
- If non-DTX frame (voice) → **send immediately**
- **Sequence number only increments when packet is actually sent**

### Files to Modify
- audio_task.rs: Add `last_send_time: Instant` tracking
- audio_task.rs: Add DTX detection logic in the encode-send path (around line 810)
- codec.rs: Add helper method `is_dtx_frame(encoded: &[u8]) -> bool` (check if len ≤2)

---

## 3. Decoder Lifecycle Changes

### Current Implementation (`audio_task.rs:1136-1160`)
- Decoders created lazily on first packet from user (`user_audio.entry().or_insert_with`)
- Decoder recreated when sender restarts (lines 202, 234)
- No proactive creation based on server state

### Required Changes (Plan: lines 486-497)
- **Create decoders based on server state**, not packet arrival
- When user joins our room → create `UserAudioState` (decoder + jitter buffer + pipeline)
- When user leaves room → destroy their `UserAudioState`
- When we change rooms → destroy all, create new ones for users in new room
- **Never recreate decoder on sender restart** - persist through DTX silence

### Files to Modify
- audio_task.rs: Add handler for room/user state changes from connection task
- audio_task.rs: Remove decoder recreation logic on sender restart (lines 197-206, 228-237)
- handle.rs: Send room membership changes to audio task
- Add new `AudioCommand` variants: `UserJoinedRoom`, `UserLeftRoom`, `WeChangedRooms`

---

## 4. RX DTX and Packet Loss Detection

### Current Implementation
- Sequence gaps trigger PLC
- Decoder reset on "sender restart" detection
- Uses FEC from next packet when available

### Required Changes (Plan: lines 493-496)
- **Sequence numbers are consecutive** (no gaps from DTX skipping on sender)
- Any sequence gap = true packet loss → invoke PLC
- DTX keepalive frames (≤2 bytes) decode to silence normally
- **Remove decoder reset on sender restart** - let it persist
- Keep `end_of_stream` handling as-is (it's correct)

### Files to Modify
- audio_task.rs: Remove the "sender restart" detection and decoder reset logic
- The receiver doesn't need special DTX handling - Opus handles it automatically

---

## 5. End-of-Stream Handling

### Current Implementation
- ✅ EOS packets sent when transmission stops
- ✅ EOS marks `stream_ended = true`
- ✅ After EOS, missing packets not counted as lost

### No Changes Required
This part aligns with the plan.

---

## 6. Inter-Task Communication for Room State

### Current Implementation
- Audio task doesn't know about room membership
- Creates decoders on packet arrival

### Required Changes
Need a way for connection task to inform audio task about room membership:

```rust
pub enum AudioCommand {
    // ... existing ...
    
    /// A user joined our current room - create their decoder/pipeline
    UserJoinedRoom { user_id: u64 },
    
    /// A user left our current room - destroy their decoder/pipeline  
    UserLeftRoom { user_id: u64 },
    
    /// We changed rooms - destroy all, create for new room members
    RoomChanged { user_ids_in_room: Vec<u64> },
}
```

### Files to Modify
- audio_task.rs: Add new command handlers
- handle.rs: Send these commands when processing `UserMoved`, `UserJoined`, `UserLeft` state updates
- handle.rs: Send `RoomChanged` when our own room changes

---

## Implementation Order

1. **Encoder lifecycle** - Move to connection-scoped (most architectural change)
2. **DTX frame handling** - Add detection and skip logic
3. **Remove decoder reset** - Simple removal of problematic code
4. **Server-state-driven decoders** - Add inter-task communication
5. **Testing** - Verify DTX keepalives work, sequence numbers correct

---

## Testing Considerations

- Verify encoder persists through multiple PTT cycles
- Verify DTX frames (silence) produce ≤2 byte outputs
- Verify keepalives sent ~every 400ms during silence
- Verify sequence numbers only increment on actual sends
- Verify decoders created before first packet arrives
- Verify no decoder reset on new transmission from same user