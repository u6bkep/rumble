# Chat History Sync

P2P-based chat history synchronization for late joiners. Ephemeral by design - history lives only while users are connected.

## Overview

When a user joins a room mid-conversation, they can request chat history from peers who have it. This uses the existing P2P file sharing infrastructure - history is serialized as a JSON file and transferred like any other shared file.

## User Flow

1. User joins room, sees empty or partial chat
2. User clicks "Sync History" button (or auto-sync if enabled)
3. Client broadcasts a history offer request to room peers
4. A peer with history responds by sharing `chat-history.json`
5. Client receives, parses, and merges messages into local state
6. Chat now shows the conversation the user missed

## Protocol

### History Request

Sent as a chat message to the room:

```json
{
  "type": "chat-history-request"
}
```

### History Offer

When a peer receives a history request, they serialize their non-local messages and share as a file:

```json
{
  "type": "file",
  "file": {
    "name": "chat-history.json",
    "size": 12345,
    "mime": "application/x-rumble-chat-history",
    "infohash": "abc123..."
  }
}
```

### History Content Format

The `chat-history.json` file contains:

```json
{
  "version": 1,
  "room_id": "uuid-hex-string",
  "messages": [
    {
      "id": "uuid-hex-string",
      "sender": "alice",
      "text": "hello everyone",
      "timestamp": 1706400000000
    }
  ]
}
```

## Merge Algorithm

1. Parse received history JSON
2. For each message in received history:
   - Check if UUID already exists in local messages
   - If not, insert into local messages
3. Sort all messages by timestamp
4. Trigger UI repaint

## Settings

### Auto-Download Rule

Add default rule for chat history:

```rust
AutoDownloadRule {
    mime_pattern: "application/x-rumble-chat-history".into(),
    max_size: 1_000_000,  // 1MB limit
    enabled: true,        // User can disable
}
```

### Auto-Sync on Join

New setting in connection or chat settings:

```rust
pub auto_sync_history: bool,  // default: false
```

## Filtering

**Included in history export:**
- Messages where `is_local == false` (received from other users)
- File share messages (preserved for download buttons)

**Excluded from history export:**
- Local system messages (`is_local == true`)
- Connection status messages
- Error messages

## Implementation Status

### Backend (`crates/backend`) - COMPLETE

- [x] Add `ChatHistoryRequestMessage` and `ChatHistoryContent` types to events.rs
- [x] Add `ChatHistoryContent::from_messages()` for serialization
- [x] Add `ChatHistoryContent::to_messages()` for deserialization
- [x] Handle history request in ChatBroadcast handler (triggers ShareChatHistory)
- [x] Add `Command::RequestChatHistory` - sends request to room
- [x] Add `Command::ShareChatHistory` - shares history via P2P

### GUI (`crates/egui-test`) - COMPLETE

- [x] Add "↻ Sync" button to chat panel (next to Send)
- [x] Add `auto_sync_history` to FileTransferSettings
- [x] Auto-sync on room join when setting enabled
- [x] Handle history file download (merge into chat_messages)

## Edge Cases

| Scenario | Handling |
|----------|----------|
| No peers in room | Show "No history available" toast |
| Multiple peers respond | Accept first valid offer, ignore others |
| Peer disconnects mid-transfer | P2P layer handles retry/fallback |
| Malformed JSON | Log error, ignore, don't crash |
| Duplicate request | Debounce, ignore if sync in progress |
| Very large history | Size limit in auto-download rule |

## Security Considerations

- History comes from peers, not server - could be fabricated
- UUIDs should be validated as proper format
- Timestamps should be sanity-checked (not far future)
- Text content is already untrusted (same as live chat)
