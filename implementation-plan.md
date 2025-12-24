# Implementation Plan

This project is a simple VOIP application written in Rust. The primary use case is to have rooms where users can join and talk to each other using voice communication and text chat. The application uses a client-server architecture: the server handles room management and user connections, while the client provides the user interface and audio I/O.

---

## Server

The server is responsible for:

- Managing user connections
- Handling room creation and deletion
- Tracking which users are in which rooms
- Relaying voice and text messages between users in the same room
- Enforcing authentication and access control

It will use the Tokio framework for asynchronous networking. The server maintains a list of active rooms and users, and routes voice and text messages to the appropriate recipients.

Persistence will be managed by a sled database. The persistent state includes:

- Known user list (for authentication and identity)
- The room tree
- Access control lists (ACLs) for rooms and roles

The server will be a simple single-process application; there is no need for a distributed architecture initially. All communication is relayed through the server, with no peer-to-peer connections between clients.

The server will be secured with TLS and QUIC. It will support:

- Automatically generating a self-signed certificate
- Using a provided certificate and private key
- Using ACME to obtain trusted certificates from a service like Let's Encrypt

When using ACME, the server must be configured with a domain name.

---

## Client Architecture

The client is split into two crates:
- **backend**: A library crate providing connection management, audio I/O, and state management
- **egui-test**: A GUI application using the egui library

### State-Driven API

The backend exposes a **state-driven API** to the UI. The UI does not poll for individual events; instead:

1. The backend exposes a `State` object representing the complete client state
2. The UI renders based on this state
3. User actions result in **commands** sent to the backend
4. The backend updates state and notifies the UI via a **repaint callback**
5. The UI re-renders from the new state

```
┌─────────────────────────────────────────────────────────────────┐
│                         BackendHandle                           │
├─────────────────────────────────────────────────────────────────┤
│  state: Arc<RwLock<State>>     ◄── UI reads this                │
│  repaint_callback: Fn()        ◄── Called when state changes    │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌─────────────────────┐     ┌─────────────────────┐            │
│  │  Connection Task    │     │    Audio Task       │            │
│  │  (tokio thread)     │     │   (tokio thread)    │            │
│  │                     │     │                     │            │
│  │  - QUIC streams     │     │  - QUIC datagrams   │            │
│  │  - Protocol msgs    │     │  - cpal streams     │            │
│  │  - State sync       │     │  - Opus encode/dec  │            │
│  │                     │     │  - Jitter buffers   │            │
│  └─────────────────────┘     └─────────────────────┘            │
│           │                           │                         │
│           └───────────┬───────────────┘                         │
│                       ▼                                         │
│              state.write() + repaint_callback()                 │
└─────────────────────────────────────────────────────────────────┘
```

**Key principles:**
- The backend runs independently of connection and audio state
- Audio devices can be enumerated and tested without a connection
- Audio configuration persists across connections
- Commands that require a connection return with an error if not connected
- Reconnection logic is handled by the UI, not the backend

### Backend State

The backend exposes a single `State` struct that the UI reads:

```rust
pub struct State {
    // Connection
    pub connection: ConnectionState,
    
    // Server state (when connected)
    pub rooms: Vec<RoomInfo>,
    pub users: Vec<User>,
    pub memberships: Vec<(UserId, RoomId)>,
    pub my_user_id: Option<u64>,
    pub my_room_id: Option<Uuid>,
    
    // Audio
    pub audio: AudioState,
    
    // Chat (recent messages, not persisted)
    pub chat_messages: Vec<ChatMessage>,
}

pub enum ConnectionState {
    Disconnected,
    Connecting { server_addr: String },
    Connected { server_name: String, user_id: u64 },
    ConnectionLost { error: String },
}

pub struct AudioState {
    pub input_devices: Vec<AudioDeviceInfo>,
    pub output_devices: Vec<AudioDeviceInfo>,
    pub selected_input: Option<String>,
    pub selected_output: Option<String>,
    pub transmission_mode: TransmissionMode,
    pub is_transmitting: bool,  // Actual TX state
    pub talking_users: HashSet<u64>,  // Users currently transmitting
}

pub enum TransmissionMode {
    /// Only transmit while PTT key is held
    PushToTalk,
    /// Always transmitting when connected
    Continuous,
    /// Not transmitting (muted)
    Muted,
    // Future: VoiceActivated { threshold: f32 }
}
```

### Backend Commands

Commands are fire-and-forget. The UI sends a command, and the backend updates state asynchronously:

```rust
pub enum Command {
    // Connection
    Connect { addr: String, name: String, password: Option<String> },
    Disconnect,
    
    // Room/Chat
    JoinRoom { room_id: Uuid },
    CreateRoom { name: String },
    DeleteRoom { room_id: Uuid },
    RenameRoom { room_id: Uuid, new_name: String },
    SendChat { text: String },
    
    // Audio configuration (always available)
    SetInputDevice { device_id: Option<String> },
    SetOutputDevice { device_id: Option<String> },
    SetInputVolume { volume: f32 },
    SetOutputVolume { volume: f32 },
    RefreshAudioDevices,
    
    // Transmission control
    SetTransmissionMode { mode: TransmissionMode },
    /// For PTT: start transmitting (only effective in PushToTalk mode)
    StartTransmit,
    /// For PTT: stop transmitting
    StopTransmit,
}
```

### Two Background Tasks

The backend spawns two independent background tasks:

1. **Connection Task** (tokio thread):
   - Manages QUIC connection lifecycle (streams only)
   - Sends/receives reliable protocol messages
   - Updates connection and room state
   - Handles keep-alive and state sync
   - Passes cloned `Connection` handle to audio task on connect

2. **Audio Task** (tokio thread):
   - Owns QUIC datagram send/receive (cloned `Connection` handle)
   - Manages cpal audio streams
   - Runs Opus encoder (capture) and per-user decoders (playback)
   - Manages per-user jitter buffers
   - Sends datagrams directly: `connection.send_datagram()`
   - Receives datagrams directly: `connection.read_datagram()`
   - Updates `talking_users` in state based on jitter buffer contents

**Inter-task communication:**
- Connection task sends `Connection` handle to audio task on connect
- Audio task sends `None` to indicate connection closed
- No voice data flows through channels - audio task handles datagrams directly
- audio task updates `talking_users` in shared state

This separation ensures:
- Audio never blocks on reliable message I/O
- Reliable messages never block on audio
- Minimal latency for voice (no extra channel hop)
- Audio can be configured/tested without a connection
- Clean shutdown of either component independently

### UI Integration (egui)

```rust
// In egui app
struct MyApp {
    backend: BackendHandle,
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Read current state (no polling, just read)
        let state = self.backend.state();
        
        // Render UI based on state
        match &state.connection {
            ConnectionState::Disconnected => {
                // Show connect dialog
                if ui.button("Connect").clicked() {
                    self.backend.send(Command::Connect { ... });
                }
            }
            ConnectionState::Connecting { server_addr } => {
                ui.label(format!("Connecting to {}...", server_addr));
            }
            ConnectionState::Connected { .. } => {
                // Show rooms, users, chat
                self.render_main_ui(ui, &state);
            }
            ConnectionState::ConnectionLost { error } => {
                // Show error and reconnect button (UI handles reconnect)
                ui.label(format!("Connection lost: {}", error));
                if ui.button("Reconnect").clicked() {
                    self.backend.send(Command::Connect { ... });
                }
            }
        }
    }
}

// Backend setup with repaint callback
let ctx = egui_ctx.clone();
let backend = BackendHandle::new(move || ctx.request_repaint());
```

### Audio Transmission Modes

**PushToTalk**: Default mode. User holds a key to transmit.
- `StartTransmit` command begins capture and encoding
- `StopTransmit` command stops capture
- `is_transmitting` in state reflects actual TX state

**Continuous**: Always transmitting when connected.
- Transmission starts automatically on connection
- Audio capture runs continuously
- Future: VAD can be added to this mode to pause transmission during silence

**Muted**: Never transmitting.
- Audio capture is stopped
- Can still receive and play audio from others

### Client Identity

- Each client is identified by a persistent client ID, implemented as a public/private keypair generated on first run and stored locally.
- The public key is sent to the server for authentication; the private key never leaves the client.
- The user-visible name is distinct from the client ID and can be changed by the user.
- Client stores persistent data (keypair, config) in a standard XDG-compliant config directory.
- future: client hello will include signed proof of possession of the private key and a copy of the public key.

---

## API (Protocol)

A separate library crate defines the API for communication between client and server. The API is defined using Protocol Buffers and implemented with Prost.

The API crate holds anything both client and server need to know about the protocol, including message types, state definitions, and serialization logic.

### Transport

- All communication uses QUIC via the `quinn` crate
- The API crate is transport-agnostic where possible, focusing on message types and state
- **Voice data**: Opus-encoded QUIC datagrams for low-latency (unreliable)
- **Control messages**: QUIC streams for reliable, ordered delivery
- **Large data** (future): BitTorrent using `librqbit`, server as tracker over QUIC

### Message Structure

#### Envelope (Reliable Stream Messages)

All reliable messages are wrapped in an `Envelope`:

```protobuf
message Envelope {
  bytes state_hash = 1;
  oneof payload {
    ClientHello client_hello = 10;
    ServerHello server_hello = 11;
    ChatMessage chat_message = 12;
    // ... other message types
  }
}
```

#### State Model

User information and room membership:

```protobuf
// User identity (separate from room membership)
message User {
  UserId user_id = 1;
  string username = 2;
  RoomId current_room = 3;
}

// Room definition
message RoomInfo {
  RoomId id = 1;
  string name = 2;
}

// Full state snapshot
message ServerState {
  repeated RoomInfo rooms = 1;
  repeated User users = 2;
}

```

#### Incremental Updates

State changes are sent as typed incremental updates with expected hash:

```protobuf
message StateUpdate {
  bytes expected_hash = 1;  // Hash after applying this update
  
  oneof update {
    UserMoved user_moved = 10;
    UserJoined user_joined = 11;
    UserLeft user_left = 12;
    RoomCreated room_created = 13;
    RoomDeleted room_deleted = 14;
    RoomRenamed room_renamed = 15;
  }
}

// Simplified: from is implicit, and not needed after the user moves
message UserMoved {
  UserId user_id = 1;
  RoomId to_room_id = 2;
}
```

#### Voice Datagrams

Voice data is sent as QUIC datagrams (unreliable, low-latency):

```protobuf
// Voice data (sent as QUIC datagram, not in Envelope)
message VoiceDatagram {
  bytes opus_data = 1;
  uint32 sequence = 2;
  uint64 timestamp_us = 3;
  
  // Server-set fields (ignored if sent by client)
  optional uint64 sender_id = 10;
  optional bytes room_id = 11;
}
```

The server stamps each datagram with `sender_id` and `room_id` before relaying to room members.

**No explicit start/stop messages** - talking state is derived from jitter buffer contents:
- User is "talking" when their jitter buffer has packets
- User "stopped talking" when buffer drains and timeout expires

### State Synchronization

Both client and server maintain a shared logical state:
- The room list
- Which users are in which rooms
- User state (muted, deafened, roles, ACLs)

When a client connects:
1. Server sends current full state with hash
2. Subsequent changes are sent as incremental updates with expected hash
3. If client's computed hash doesn't match, it requests full resync

The state hash is computed using BLAKE3 over canonical protobuf serialization.

---

## Audio

Audio uses the Opus codec for efficient compression and low latency.

### Parameters
- Sample rate: 48kHz
- Channels: Mono
- Bitrate: Variable (Opus VBR)
- Frame size: 20ms (960 samples)
- Mode: VOIP with DTX and FEC enabled

### Flow

**Capture → Encode → Send:**
1. cpal captures audio in callback, queues samples
2. Audio task encodes with Opus
3. Audio task sends directly: `connection.send_datagram()`

**Receive → Decode → Play:**
1. Audio task receives: `connection.read_datagram()`
2. Extracts `sender_id`, routes to per-user jitter buffer (created lazily)
3. Pulls from all active buffers by sequence number
4. Missing packets → `decoder.decode(None)` for PLC
5. Present packets → `decoder.decode(Some(data))`
6. Mixed PCM from all users fed to cpal playback

### Jitter Buffer

Each remote user has their own jitter buffer and Opus decoder:

```rust
struct RemoteUserAudio {
    user_id: u64,
    decoder: OpusDecoder,
    jitter_buffer: JitterBuffer,
    last_packet_time: Instant,
}
```

**Jitter buffer behavior:**
- Packets inserted by sequence number (handles out-of-order arrival)
- Fixed initial delay (e.g., 60ms / 3 frames) before starting playback
- When a packet is missing at playback time, feed `None` to Opus decoder (triggers PLC)
- Adaptive sizing based on observed jitter (optional enhancement, use fixed size for MVP)

**Talking detection:**
- User is "talking" when packets are in their buffer or recently played
- User "stopped talking" after ~200ms with no new packets
- This naturally handles network drops without explicit signaling

**Decoder lifecycle:**
- Create decoder lazily on first packet from a user
- Keep decoder alive for some time after user stops (maintains PLC state)
- Eventually evict inactive decoders when server send message indicating user left room.

**Packet loss concealment:**
- When sequence gap detected, call `decoder.decode(None)` for each missing packet
- Opus PLC generates comfort audio to mask the gap
- Better audio quality than silence insertion

---

## Authentication, Identity, and ACLs

### Authentication
- Simple global password for server authentication
- Password configured per server instance
- Clients present password + public key when connecting

### Identity
- Users identified by Ed25519 public keys (client IDs)
- Username is display name, distinct from client ID
- Default unauthenticated role for unknown keys

### Access Control (Future)
- All rooms have an ACL system
- ACLs grant/deny permissions to users or roles
- Rules inherit from parent rooms, can be overridden
- Permissions include: join room, speak, send text, create rooms, move users, edit ACLs

---

## Features

### Initial Feature Set (MVP)
1. User authentication (global password + keypair identity)
2. Room management (create, join, leave, delete)
3. Voice communication (PTT, continuous, muted modes)
4. Text chat over QUIC streams
5. State synchronization with hash verification

### Later Features
1. User roles and ACLs for fine-grained permissions
2. Voice activity detection (VAD) transmission mode
3. File transfers using BitTorrent with server as tracker
4. Chat history snapshot sharing via torrent
5. Mobile client support (Android, iOS, WASM)
6. Server admin interface (CLI or web UI)
7. Server plugins (RPC interface, scripting, loadable modules)

---

## Crate Layout

```
crates/
├── api/           Protocol Buffers, message types, state structures, hash computation
├── server/        Server binary: connections, room management, message routing
├── backend/       Client library: connection, audio, state management
└── egui-test/     Desktop GUI using egui
```

---

## Open Questions

1. **ACL Schema**: Detailed permission list, inheritance rules, conflict resolution
2. **Audio Tuning**: Optimal jitter buffer size, FEC settings, bitrate targets
3. **TLS/ACME**: Challenge type, built-in vs external proxy
4. **Per-User Decoder Lifetime**: How long to keep decoder state for inactive users (LRU?)
5. **Continuous Mode + VAD**: When VAD is added, should it auto-mute during silence or just mark state?
