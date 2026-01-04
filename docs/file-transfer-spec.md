# File Transfer Specification

This document specifies the file transfer system for Rumble, enabling users to share files in chat using BitTorrent protocol with the Rumble server acting as a private tracker.

## Overview

File transfer extends the chat system, similar to posting files in Discord. Files are distributed peer-to-peer using BitTorrent, with the Rumble server providing:
- Private tracker for peer discovery
- User ID mapping for UI display
- Optional relay for NAT'd clients

## Design Goals

1. **Chat-native**: Files appear as rich messages in chat with inline previews
2. **Ephemeral by default**: Transfers are tied to connected users; cleanup when seeders leave
3. **Bandwidth-conscious**: Protect voice quality with configurable limits
4. **NAT-friendly**: Explicit relay mode for clients behind NAT

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                           Client A (Seeder)                          │
│  ┌────────────────┐    ┌─────────────────┐    ┌──────────────────┐  │
│  │ TorrentManager │───▶│ Announce Loop   │───▶│ librqbit Session │  │
│  │                │    │ (QUIC streams)  │    │ (TCP listener)   │  │
│  └────────────────┘    └─────────────────┘    └────────┬─────────┘  │
└───────────────────────────────────────────────────────┼─────────────┘
                                                        │ Direct TCP
                    QUIC ↓                              │ (or relay)
┌───────────────────────┼───────────────────────────────┼─────────────┐
│                    Server                             │             │
│  ┌─────────────────────────────────────┐              │             │
│  │              Tracker                │              │             │
│  │  - Swarm management                 │              │             │
│  │  - User ID ↔ Peer ID mapping        │              │             │
│  │  - Lifecycle (cleanup on disconnect)│              │             │
│  └─────────────────────────────────────┘              │             │
│  ┌─────────────────────────────────────┐              │             │
│  │           Relay Service             │◀─────────────┘             │
│  │  - Speed-limited forwarding         │                            │
│  │  - Explicit opt-in                  │                            │
│  └─────────────────────────────────────┘                            │
└─────────────────────────────────────────────────────────────────────┘
                    QUIC ↓
┌─────────────────────────────────────────────────────────────────────┐
│                           Client B (Leecher)                         │
└─────────────────────────────────────────────────────────────────────┘
```

## Protocol

### Chat Message Format

File shares are posted as chat messages with JSON-encoded content. The client detects file messages by:
1. Attempting JSON parse
2. Validating against the file message schema
3. Rejecting messages with extraneous fields (prevents false positives on user-pasted JSON)

#### Message Schema

```json
{
  "$schema": "https://rumble.example/schemas/file-message-v1.json",
  "type": "file",
  "file": {
    "name": "photo.jpg",
    "size": 1048576,
    "mime": "image/jpeg",
    "infohash": "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
  }
}
```

| Field           | Type    | Description                             |
| --------------- | ------- | --------------------------------------- |
| `type`          | string  | Must be `"file"`                        |
| `file.name`     | string  | Original filename                       |
| `file.size`     | integer | File size in bytes                      |
| `file.mime`     | string  | MIME type (e.g., `image/jpeg`)          |
| `file.infohash` | string  | 40-character hex-encoded SHA-1 infohash |

The magnet link is derived from infohash: `magnet:?xt=urn:btih:{infohash}`

#### API Changes

Add fields to `ChatMessage` and `ChatBroadcast` in `api.proto`:

```protobuf
message ChatMessage {
  bytes id = 1;           // 16-byte UUID
  int64 timestamp_ms = 2; // Unix timestamp milliseconds
  string sender = 3;
  string text = 4;        // EasyMark or JSON for file messages
}

message ChatBroadcast {
  bytes id = 1;
  int64 timestamp_ms = 2;
  string sender = 3;
  string text = 4;
}
```

### Tracker Protocol

#### TrackerAnnounce (Updated)

```protobuf
message TrackerAnnounce {
  bytes info_hash = 1;    // 20 bytes
  bytes peer_id = 2;      // 20 bytes
  uint64 user_id = 3;     // NEW: Rumble user ID
  uint32 port = 4;
  uint64 uploaded = 5;
  uint64 downloaded = 6;
  uint64 left = 7;
  Event event = 8;
  uint32 numwant = 9;
  uint32 request_id = 10;
  bool needs_relay = 11;  // NEW: Client requests relay mode

  enum Event {
    NONE = 0;
    COMPLETED = 1;
    STARTED = 2;
    STOPPED = 3;
  }
}
```

#### TrackerAnnounceResponse (Updated)

```protobuf
message TrackerAnnounceResponse {
  uint32 interval = 1;
  uint32 min_interval = 2;
  uint32 complete = 3;
  uint32 incomplete = 4;
  repeated PeerInfo peers = 5;
  uint32 request_id = 6;
  optional RelayInfo relay = 7;  // NEW: Relay assignment if requested
}

message PeerInfo {
  bytes peer_id = 1;
  uint64 user_id = 2;     // NEW: Rumble user ID for display
  string ip = 3;
  uint32 port = 4;
  bool supports_relay = 5; // NEW: Peer can receive via relay
}

message RelayInfo {
  string relay_token = 1;  // Token to authenticate relay requests
  uint32 relay_port = 2;   // Server relay port
}
```

### Relay Protocol

For NAT'd clients, the server provides an explicit relay service.

#### Relay Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                           Server                                     │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                    Relay Service (TCP)                        │   │
│  │  ┌───────────────────┐    ┌───────────────────────────────┐  │   │
│  │  │ Token Manager     │    │ Connection Matcher            │  │   │
│  │  │ - Generate tokens │    │ - Queue waiting acceptors     │  │   │
│  │  │ - Validate tokens │    │ - Match dialers to acceptors  │  │   │
│  │  └───────────────────┘    │ - Bridge TCP streams          │  │   │
│  │                           └───────────────────────────────┘  │   │
│  └─────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
         ↑ TCP                                           ↑ TCP
         │ (Acceptor)                                    │ (Dialer)
┌────────┴───────┐                              ┌────────┴───────┐
│  NAT'd Client  │                              │  Normal Peer   │
│  (behind NAT)  │                              │  (can dial)    │
└────────────────┘                              └────────────────┘
```

#### Relay Handshake Protocol

1. Client connects to relay TCP port
2. Client sends handshake:
   - 1 byte: Role (0x01 = Acceptor, 0x02 = Dialer)
   - 32 bytes: Relay token
3. For Acceptors: Connection is queued, waiting for a matching Dialer
4. For Dialers: Server looks up matching Acceptor and bridges connections
5. Once matched: Raw BitTorrent protocol bytes are forwarded bidirectionally

#### Requesting Relay

1. Client sets `needs_relay = true` in `TrackerAnnounce`
2. Server returns `RelayInfo` with authentication token (hex-encoded)
3. Client connects to relay port as Acceptor with token
4. Other peers see `supports_relay = true` and can dial through relay

#### Relay Limits

- Per-user speed limit (configurable, default: 1 MB/s)
- Server global limit (configurable, default: 10 MB/s)
- Relay sessions timeout after inactivity (default: 60 seconds)
- Stale acceptors are cleaned up periodically

## Client Behavior

### Sharing a File

1. User selects file via UI
2. Client creates torrent (single file, no trackers in .torrent)
3. Client adds torrent to librqbit session
4. Client sends `TrackerAnnounce` with `event=STARTED`
5. Client posts file message to chat
6. Client spawns announce loop (periodic re-announce)

### Downloading a File

1. User clicks download on file message (or auto-download triggers)
2. Client extracts infohash from message
3. Client sends `TrackerAnnounce` to get peers
4. Client adds magnet to librqbit with initial peers
5. Client spawns announce loop
6. On completion, client continues seeding until exit or manual delete

### Lifecycle

| Event             | Client Action                          | Server Action                    |
| ----------------- | -------------------------------------- | -------------------------------- |
| Share file        | Add to session, announce, post message | Add peer to swarm                |
| Download file     | Announce, add magnet, download         | Add peer to swarm                |
| Client disconnect | Stop seeding (implicit)                | Remove peer from all swarms      |
| All seeders leave | N/A                                    | Clean up swarm, file unavailable |
| Manual delete     | Remove from session, announce STOPPED  | Remove peer from swarm           |
| Client exit       | Clean up seed directory (configurable) | Remove peer from all swarms      |

### Deduplication

Files with identical content have identical infohash (SHA-1 of torrent info dict). When two users share the same file:
- Both join the same swarm automatically
- UI shows both users as seeders
- Either seeder leaving still allows download from the other

## UI Specification

### Chat Inline Display

#### Before Download
```
┌─────────────────────────────────────┐
│ 📄 presentation.pdf                 │
│ 2.4 MB · application/pdf            │
│ [Download] [Copy Magnet]            │
└─────────────────────────────────────┘
```

#### During Download
```
┌─────────────────────────────────────┐
│ 📄 presentation.pdf                 │
│ 1.2 / 2.4 MB · 500 KB/s             │
│ ████████░░░░░░░░ 50%                │
│ [Pause] [Cancel]                    │
└─────────────────────────────────────┘
```

#### After Download
```
┌─────────────────────────────────────┐
│ 📄 presentation.pdf                 │
│ 2.4 MB · application/pdf            │
│ Downloaded ✓                        │
│ [Open] [Save As...] [Repost]        │
└─────────────────────────────────────┘
```

#### After Download (Image)
```
┌─────────────────────────────────────┐
│ ┌─────────────────────────────────┐ │
│ │                                 │ │
│ │        [Image Preview]          │ │
│ │                                 │ │
│ └─────────────────────────────────┘ │
│ photo.jpg · 1.2 MB · Downloaded ✓   │
│ [Open] [Save As...] [Repost]        │
└─────────────────────────────────────┘
```

- **Open**: Opens the file with the system default application
- **Save As**: Copies the file to a user-selected location
- **Repost**: Re-sends the file share message so new users in chat can download

#### Unavailable
```
┌─────────────────────────────────────┐
│ 📄 presentation.pdf                 │
│ 2.4 MB · application/pdf            │
│ ⚠️ No seeders available             │
└─────────────────────────────────────┘
```

### Transfers Menu

Dedicated panel listing all active transfers:

```
Transfers
─────────────────────────────────────────
↓ photo.jpg          1.2 MB   Complete
  Seeding · 2 peers · ↑ 500 KB/s
  [Stop Seeding] [Save As...] [Delete]

↓ video.mp4          150 MB   Downloading
  45% · 3 peers · ↓ 2.1 MB/s
  ████████░░░░░░░░░░
  [Pause] [Cancel]

↑ document.pdf       2.4 MB   Seeding
  3 peers · ↑ 100 KB/s
  [Stop Seeding] [Delete]
─────────────────────────────────────────
```

### Settings

#### Download Directory

```
Download Directory
─────────────────────────────────────────
Location: [~/Downloads/rumble     ] [Browse]
─────────────────────────────────────────
```

Command-line override: `--download-dir <path>`

When downloading a file that already exists at the destination:
1. The torrent client adds the existing file to the session
2. Verification runs to check piece integrity
3. Only missing/corrupt pieces are downloaded
4. If fully verified, download completes immediately

#### Auto-Download Rules

Table-based configuration with mime pattern and size limit:

```
Auto-Download Rules
─────────────────────────────────────────
Pattern          Max Size    [Add Rule]
─────────────────────────────────────────
image/*          10 MB       [Remove]
audio/*          50 MB       [Remove]
video/*          0 (disabled)[Remove]
application/pdf  5 MB        [Remove]
─────────────────────────────────────────
```

Default rules:
- `image/*` → 10 MB
- `audio/*` → 50 MB
- `text/*` → 1 MB

#### Bandwidth Limits

```
Bandwidth Limits
─────────────────────────────────────────
Download limit:  [Unlimited ▾]  KB/s
Upload limit:    [500       ]  KB/s
─────────────────────────────────────────
☐ Auto-seed downloaded files
☑ Clean up seed files on exit
─────────────────────────────────────────
```

## State Management

### FileTransferState (Updated)

```rust
pub struct FileTransferState {
    pub infohash: [u8; 20],
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub progress: f32,           // 0.0 to 1.0
    pub download_speed: u64,     // bytes/sec
    pub upload_speed: u64,       // bytes/sec
    pub seeders: Vec<u64>,       // User IDs currently seeding
    pub state: TransferState,
    pub error: Option<String>,
}

pub enum TransferState {
    Pending,      // In queue, not started
    Checking,     // Verifying existing data
    Downloading,
    Seeding,
    Paused,
    Completed,    // Downloaded but not seeding
    Error,
}
```

## Future Considerations

### Chat History Sync

Chat messages with UUIDs and timestamps enable future sync feature:
1. User requests chat history from peer
2. Peer creates torrent of chat log (JSON lines)
3. Transfer via standard file transfer
4. Recipient deduplicates by UUID, orders by timestamp

### Room Scope Evolution

Current: Files shared to current room only (connected users).

Future possibilities:
- Direct messages: Peer-to-peer only, no server involvement
- Cascading rooms: Share to room and all sub-rooms
- Persistent channels: Server stores file metadata, peers seed

### Access Control

File sharing permissions delegated to upcoming ACL system:
- `file.share` — Can share files
- `file.download` — Can download files
- `file.relay` — Can use server relay

Unauthenticated users use configurable default ACL set.

## Implementation Checklist

### Phase 0: Bug Fixes (Existing Code) ✅

| Priority | Location                      | Issue                                                                                           | Fix                                                    | Status |
| -------- | ----------------------------- | ----------------------------------------------------------------------------------------------- | ------------------------------------------------------ | ------ |
| High     | `torrent.rs:211-319`          | Announce loop never terminates — runs forever even after torrent completes or connection closes | Add cancellation token, stop on disconnect/completion  | ✅ Done |
| High     | `torrent.rs:23-24`            | `pending_requests` HashMap is dead code — never used, responses read inline                     | Remove unused field and `handle_announce_response()`   | ✅ Done |
| Medium   | `tracker.rs:99-101`           | Probabilistic pruning (`rand < 10`) is unreliable                                               | Add background cleanup task or prune on every announce | ✅ Done |
| Medium   | `torrent.rs:107`              | `download_file()` generates new peer_id but session already has one                             | Use session's peer_id consistently                     | ✅ Done |
| Medium   | `torrent.rs:177-188, 274-286` | Duplicate IPv6-to-IPv4 unmapping code in 3 places                                               | Extract to helper function                             | ✅ Done |
| Medium   | `tracker.rs:110-115`          | Peer selection is sequential, not random; should prioritize seeders                             | Randomize and prefer peers with `left=0`               | ✅ Done |
| Low      | `torrent.rs:269`              | Hardcoded minimum interval ignores `min_interval` from response                                 | Respect `min_interval` field                           | ✅ Done |
| Low      | `torrent.rs:291`              | Debug `println!` should use tracing                                                             | Replace with `tracing::debug!`                         | ✅ Done |

### Phase 1: Core Protocol ✅
- [x] Update `TrackerAnnounce` with `user_id`, `needs_relay`
- [x] Update `TrackerAnnounceResponse` with user IDs in peers
- [x] Update `ChatMessage`/`ChatBroadcast` with UUID, timestamp
- [x] Implement file message JSON schema validation
- [x] Fix announce loop lifecycle (stop on disconnect/complete)
- [x] Clean up swarms when all seeders disconnect

### Phase 2: Client Features ✅
- [x] File message rendering in chat (file widget with icon, name, size, download button)
- [x] Transfers menu UI (enhanced with state icons, speed display, peer count)
- [x] Auto-download settings (FileTransfer settings category with MIME patterns)
- [x] Bandwidth limit settings (download/upload speed limits in settings)
- [x] Pause/resume/cancel controls (using librqbit Session.pause/unpause/delete API)
- [x] "Save As" for completed downloads (async file picker with copy to destination)
- [x] Seed file cleanup on exit (configurable setting added)
- [x] Downloaded file card with Open/Save As/Repost buttons (replaces Download button when file is available)
- [x] Open file with system default application
- [x] Progress bar in chat file card during download (with Pause/Cancel buttons)
- [x] Inline image preview for downloaded images (up to 300x200px thumbnail)

### Phase 3: NAT Traversal ✅
- [x] Implement relay service on server
- [x] Client relay mode detection and request
- [x] Relay authentication and rate limiting

### Phase 4: Polish (3/4 Complete)
- [x] Inline media preview (images displayed inline when downloaded)
- [ ] Progress notifications (toast system)
- [x] Drag-and-drop file sharing (visual drop indicator when dragging files)
- [x] Paste image from clipboard (Ctrl+V to paste and share clipboard images)
