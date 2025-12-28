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
    pub voice_mode: VoiceMode,
    pub self_muted: bool,       // Orthogonal mute toggle
    pub self_deafened: bool,    // Orthogonal deafen toggle (implies muted)
    pub is_transmitting: bool,  // Actual TX state
    pub talking_users: HashSet<u64>,  // Users currently transmitting
    pub tx_pipeline: PipelineConfig,  // TX processing pipeline config
    pub rx_pipeline_defaults: PipelineConfig,  // Default RX pipeline for new users
    pub per_user_rx: HashMap<u64, UserRxConfig>,  // Per-user RX overrides
}

pub enum VoiceMode {
    /// Only transmit while PTT key is held
    PushToTalk,
    /// Always transmitting when connected (subject to pipeline suppression)
    Continuous,
}
```

Note: `Muted` is now a separate orthogonal toggle (`self_muted`), not a voice mode.

Note: Voice Activity Detection (VAD) is not a voice mode but rather a pipeline processor. To achieve "voice activated" transmission, use Continuous mode with the VAD processor enabled in the TX pipeline.

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
    
    // Audio device configuration (always available)
    SetInputDevice { device_id: Option<String> },
    SetOutputDevice { device_id: Option<String> },
    RefreshAudioDevices,
    
    // Voice mode and mute control
    SetVoiceMode { mode: VoiceMode },
    SetMuted { muted: bool },
    SetDeafened { deafened: bool },
    /// For PTT: start transmitting (only effective in PushToTalk mode)
    StartTransmit,
    /// For PTT: stop transmitting
    StopTransmit,
    
    // Audio pipeline configuration
    UpdateTxPipeline { config: PipelineConfig },
    UpdateRxPipelineDefaults { config: PipelineConfig },
    UpdateUserRxConfig { user_id: u64, config: UserRxConfig },
    ClearUserRxOverride { user_id: u64 },
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

### Voice Modes

Voice mode controls *when* transmission is triggered. Mute is a separate orthogonal toggle.

**PushToTalk** (default): User holds a key to transmit.
- `StartTransmit` command begins capture, processing, and encoding
- `StopTransmit` command stops capture
- `is_transmitting` in state reflects actual TX state
- "Talking" indicator lights when PTT is active
- TX pipeline still runs on captured audio (for processing)

**Continuous**: Always transmitting when connected (unless muted or suppressed by pipeline).
- Audio capture runs continuously while connected
- All audio passes through the TX pipeline
- If VAD processor is enabled, it can suppress transmission when no voice is detected
- This effectively provides "voice activated" behavior when VAD is enabled
- "Talking" indicator lights when audio is actually transmitted (not suppressed)

**Mute Toggle** (orthogonal to voice mode):
- When `self_muted = true`, never transmit regardless of voice mode
- Can still receive and play audio from others (unless deafened)

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
  bool is_muted = 4;     // User has muted themselves (not transmitting)
  bool is_deafened = 5;  // User has deafened themselves (not receiving audio; implies muted)
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

**User Status (Mute/Deafen):**
- `is_muted`: The user has muted themselves and is not transmitting voice.
- `is_deafened`: The user has deafened themselves and is not receiving audio. Deafen implies mute.
- Clients send `SetUserStatus` to notify the server when their mute/deafen state changes.
- The server broadcasts `UserStatusChanged` updates to all clients so they can display status icons.
- This is distinct from local muting (where client A mutes client B locally).

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
    UserStatusChanged user_status_changed = 16;
  }
}

// Simplified: from is implicit, and not needed after the user moves
message UserMoved {
  UserId user_id = 1;
  RoomId to_room_id = 2;
}

// Incremental update: user's mute/deafen status changed.
message UserStatusChanged {
  UserId user_id = 1;
  bool is_muted = 2;
  bool is_deafened = 3;
}

// Client notifies server of mute/deafen state changes.
message SetUserStatus {
  bool is_muted = 1;
  bool is_deafened = 2;
}
```

> **Note**: The full protocol is implemented in `crates/api/proto/api.proto`. The examples above are excerpts.
```

#### Voice Datagrams

Voice data is sent as QUIC datagrams (unreliable, low-latency):

```protobuf
// Voice data (sent as QUIC datagram, not in Envelope)
message VoiceDatagram {
  bytes opus_data = 1;
  uint32 sequence = 2;
  uint64 timestamp_us = 3;
  bool end_of_stream = 4;  // Signals intentional end of voice (for DTX vs PLC)
  
  // Server-set fields (ignored if sent by client)
  optional uint64 sender_id = 10;
  optional bytes room_id = 11;
}
```

The server stamps each datagram with `sender_id` and `room_id` before relaying to room members.

**End-of-stream signaling:**
- When the user stops transmitting (PTT release, VAD silence, or mute), the final packet has `end_of_stream = true`
- Receivers use this to distinguish intentional silence from packet loss:
  - `end_of_stream = true`: Stop playback cleanly, no PLC needed
  - Missing packets without EOS: Invoke Opus PLC to conceal the gap
- This enables proper DTX behavior where the encoder stops sending during silence
- `opus_data` may be empty when `end_of_stream = true`

**Talking state detection:**
- User is "talking" when their jitter buffer has packets and no EOS received
- User "stopped talking" when EOS packet received or buffer drains with timeout

### State Synchronization

Both client and server maintain a shared logical state:
- The room list (with Root room at UUID 0 always present)
- Which users are in which rooms
- User state (muted, deafened, roles, ACLs)

When a client connects:
1. Server sends current full state with hash
2. Subsequent changes are sent as incremental updates with expected hash
3. If client's computed hash doesn't match, it requests full resync via `RequestStateSync`

The state hash is computed using BLAKE3 over canonical protobuf serialization. The `ServerState` is canonicalized by sorting rooms by UUID and users by user_id before hashing to ensure deterministic results.

> **Note**: State hash computation is implemented in `crates/api/src/lib.rs` via `compute_server_state_hash()`.

---

## Audio

Audio uses the Opus codec for efficient compression and low latency.

### Parameters
- Sample rate: 48kHz (hardware), configurable encoding rate
- Channels: Mono
- Bitrate: Variable (Opus VBR), user-configurable
- Frame size: 20ms default (960 samples at 48kHz), configurable
- Mode: VOIP with DTX and FEC enabled

### DTX (Discontinuous Transmission)

Opus DTX allows bandwidth savings during silence:
- Encoder outputs frames with length ≤2 bytes when it detects silence
- These "DTX frames" signal that the audio is silent and can be skipped
- Sender tracks consecutive DTX frames and skips sending most of them
- To help receivers distinguish DTX silence from network drops, send a DTX keepalive frame every ~400ms
- This is separate from VAD suppression: DTX operates at the codec level even when VAD is disabled or has long hold times

### Codec Lifecycle

**Encoder (TX):**
- Created when connection is established, destroyed on disconnect
- Persists for the entire connection lifetime (never reset mid-session)
- When `suppress = true` (VAD/PTT/mute): stop audio capture and encoding entirely
- Encoder remains allocated but idle; resumes cleanly when transmission restarts
- On transmission stop, send final packet with `end_of_stream = true`

**DTX frame handling (TX):**
- After encoding, check if output frame length ≤2 bytes (DTX silence frame)
- Track time since last send
- If DTX frame and last sent was <400ms ago: skip sending entirely
- If DTX frame and last sent was ≥400ms ago: send as keepalive, increment sequence
- If non-DTX frame (voice): send immediately, increment sequence
- Sequence number only increments when a packet is actually sent
- Receiver uses time-based detection (>500ms gap) for true packet loss

**Decoders (RX):**
- Created/destroyed based on server state, not packet arrival
- When a user joins the same room: create their RX pipeline (decoder + jitter buffer + processors)
- When a user leaves the room: destroy their RX pipeline
- This ensures decoder state is ready before first packet arrives
- Decoder persists through silence periods (DTX) to maintain PLC state
- Use `end_of_stream` flag to distinguish intentional stop from packet loss

**DTX and drop detection (RX):**
- Sequence numbers are consecutive (no gaps from DTX skipping)
- Any sequence gap indicates true packet loss - invoke PLC for missing frames
- If `end_of_stream = true`: user stopped, no PLC needed
- DTX keepalive frames (≤2 bytes) decode to silence, confirming stream is alive

### Flow

**Capture → Process → Encode → Send:**
1. cpal captures audio in callback at hardware sample rate
2. Samples queued and batched into frames (default 20ms)
3. TX pipeline processes frame (denoise → VAD → other stages)
4. If `suppress`: send `end_of_stream = true` packet (if transitioning), then stop capture
5. If `!suppress`: encode frame through Opus
6. Check encoded frame length:
   - If >2 bytes (voice): send datagram with next sequence number
   - If ≤2 bytes (DTX silence) and <400ms since last send: skip entirely
   - If ≤2 bytes (DTX silence) and ≥400ms since last send: send keepalive with next sequence number
7. Sequence number only increments when packet is sent
8. If `suppress`: encoder sits idle (not reset), waiting for next transmission

**Receive → Decode → Process → Mix → Play:**
1. Audio task receives datagram, extracts `sender_id`
2. Routes to per-user jitter buffer (pre-created based on room membership)
3. If `end_of_stream`: mark user as not talking, stop expecting more packets
4. For missing sequence numbers: invoke Opus PLC (sequence gaps or 500ms since last packet = real packet loss)
5. Opus decodes frame (DTX frames ≤2 bytes decode to silence)
6. Per-user RX pipeline processes decoded PCM
7. Mixed PCM from all users fed to cpal playback

---

## Audio Processing Pipeline

The audio processing pipeline provides a pluggable architecture for audio processing stages on both transmit (TX) and receive (RX) paths.

### Design Principles

1. **Unified interface**: All processors implement the same trait
2. **Runtime configurable**: Processors can be enabled/disabled and configured at runtime
3. **Per-user RX pipelines**: Each incoming user has their own pipeline instance
4. **Global defaults with overrides**: UI can set defaults for all users, with per-user overrides
5. **Frame-based processing**: Processors operate on fixed-size frames (default 20ms)
6. **Adaptable parameters**: Frame size and sample rate can be adjusted for bandwidth/latency tradeoffs

### Processor Trait

```rust
/// Result from processing an audio frame.
pub struct ProcessorResult {
    /// Suppress this frame from transmission/playback.
    /// Pipeline uses OR logic: any processor returning true suppresses the frame.
    pub suppress: bool,
    
    /// Audio level in dB (for metering UI). Last Some(x) wins.
    pub level_db: Option<f32>,
}

impl Default for ProcessorResult {
    fn default() -> Self {
        Self { suppress: false, level_db: None }
    }
}

/// A stage in the audio processing pipeline.
pub trait AudioProcessor: Send + 'static {
    /// Process a frame of audio samples in-place.
    /// Frame size and sample rate are provided for processors that need them.
    fn process(
        &mut self, 
        samples: &mut [f32], 
        sample_rate: u32,
    ) -> ProcessorResult;
    
    /// Human-readable name for debugging/UI.
    fn name(&self) -> &'static str;
    
    /// Reset internal state (e.g., on transmission start/stop).
    fn reset(&mut self) {}
    
    /// Get current configuration as a serializable value (for UI display).
    fn config(&self) -> ProcessorConfig;
    
    /// Update configuration at runtime.
    fn set_config(&mut self, config: &ProcessorConfig);
    
    /// Whether this processor is currently enabled.
    fn is_enabled(&self) -> bool;
    
    /// Enable or disable this processor.
    fn set_enabled(&mut self, enabled: bool);
}

/// Factory for creating processor instances from serialized config.
/// Each processor type registers a factory function.
pub trait ProcessorFactory: Send + Sync {
    /// Unique identifier for this processor type (e.g., "builtin.denoise", "myplugin.autotune").
    fn type_id(&self) -> &'static str;
    
    /// Human-readable name for UI.
    fn display_name(&self) -> &'static str;
    
    /// Create a new processor instance with default settings.
    fn create_default(&self) -> Box<dyn AudioProcessor>;
    
    /// Create a processor instance from serialized settings.
    fn create_from_config(&self, config: &serde_json::Value) -> Result<Box<dyn AudioProcessor>, String>;
    
    /// Get the JSON schema for this processor's settings (for UI generation).
    fn settings_schema(&self) -> serde_json::Value;
}
```

### Pipeline Structure

The pipeline uses dynamic configuration to support external processor implementations:

```rust
/// Configuration for a single processor instance.
/// Uses JSON for settings to allow extensibility without enum variants.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessorConfig {
    /// Processor type identifier (e.g., "builtin.denoise", "builtin.vad").
    pub type_id: String,
    /// Whether this processor is enabled.
    pub enabled: bool,
    /// Processor-specific settings as JSON.
    pub settings: serde_json::Value,
}

/// Registry for processor factories.
/// Backend registers built-in processors; plugins can register additional ones.
pub struct ProcessorRegistry {
    factories: HashMap<String, Box<dyn ProcessorFactory>>,
}

impl ProcessorRegistry {
    pub fn register(&mut self, factory: Box<dyn ProcessorFactory>) { ... }
    pub fn create(&self, config: &ProcessorConfig) -> Result<Box<dyn AudioProcessor>, String> { ... }
    pub fn list_available(&self) -> Vec<(&str, &str)> { ... }  // (type_id, display_name)
}

/// Complete pipeline configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Ordered list of processor configs. Order determines processing order.
    pub processors: Vec<ProcessorConfig>,
    /// Frame size in samples (default 960 = 20ms at 48kHz).
    pub frame_size: usize,
}

/// Per-user RX configuration with optional overrides.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRxConfig {
    /// If Some, use these overrides; if None, use global defaults.
    pub pipeline_override: Option<PipelineConfig>,
    /// Per-user volume adjustment (always available, in dB).
    pub volume_db: f32,
}
```

### Built-in Processor Type IDs

Built-in processors use the `builtin.` prefix:
- `builtin.denoise` - RNNoise denoising
- `builtin.vad` - Voice activity detection
- `builtin.gain` - Volume adjustment
- `builtin.compressor` - Dynamic range compression
- `builtin.noisegate` - Noise gate

External crates can register processors with their own prefix (e.g., `myplugin.autotune`).

### Built-in Processors

**DenoiseProcessor** (`builtin.denoise`): RNNoise-based noise suppression.
- Uses nnnoiseless library
- Operates on 10ms chunks internally (480 samples at 48kHz)
- No user-configurable parameters
- TX default: enabled

**VadProcessor** (`builtin.vad`): Voice Activity Detection.
- Energy-based detection with configurable threshold
- Holdoff timer to avoid cutting off speech endings
- Sets `suppress = true` when no voice detected
- In Continuous mode with VAD enabled, provides "voice activated" behavior
- Can also be used in PTT mode to avoid transmitting silence/noise
- TX only

**GainProcessor**: Simple volume adjustment.
- Configurable gain in dB
- Used for per-user volume control on RX
- RX default: 0 dB (unity gain)

**CompressorProcessor**: Dynamic range compression.
- Reduces dynamic range for more consistent levels
- Configurable threshold, ratio, attack, release
- Useful as global RX post-processor

**NoiseGateProcessor**: Gate audio below threshold.
- Sets `suppress = true` for frames below threshold
- Configurable threshold, attack, release
- Alternative to VAD for simple gating

### Pipeline Execution

```rust
impl AudioPipeline {
    pub fn process(&mut self, samples: &mut [f32], sample_rate: u32) -> ProcessorResult {
        let mut result = ProcessorResult::default();
        
        for processor in &mut self.processors {
            if processor.is_enabled() {
                let r = processor.process(samples, sample_rate);
                // OR logic for suppress
                if r.suppress {
                    result.suppress = true;
                }
                // Last level_db wins
                if r.level_db.is_some() {
                    result.level_db = r.level_db;
                }
            }
        }
        
        result
    }
}
```

### TX Pipeline Integration

The TX pipeline runs after audio capture, before Opus encoding:

```
cpal capture → frame buffer → [TX Pipeline] → Opus encode → conditional send
                                    ↓                              ↓
                              ProcessorResult              if !suppress: send datagram
                                    ↓                      if transitioning to suppress:
                         track suppress state                send with end_of_transmission=true
                         is_transmitting = !suppress       if suppress: don't send (DTX)
```

**Key points:**
- Audio is captured and processed through the pipeline when active (PTT held or Continuous mode)
- When transitioning from `!suppress` to `suppress`, send a final packet with `end_of_stream = true`
- When `suppress = true`: stop capture and encoding; encoder sits idle but is not reset
- This allows receivers to distinguish intentional silence (EOS) from packet loss (needs PLC)
- Encoder remains allocated for the connection lifetime, avoiding reset overhead on each PTT cycle

The pipeline runs in both voice modes:
- **PTT mode**: Pipeline processes audio while key is held; `result.suppress` can still prevent transmission
- **Continuous mode**: Pipeline runs continuously; VAD processor (if enabled) sets `suppress` to provide voice-activated behavior
- "Talking" indicator reflects actual transmission state (`!result.suppress`)

### RX Pipeline Integration

Each remote user has their own RX pipeline:

```
recv datagram → jitter buffer → Opus decode → [User RX Pipeline] → mix
                                                      ↓
                                              ProcessorResult
                                                      ↓
                                         if result.suppress: skip frame

After mixing all users:
[Global RX Pipeline (optional)] → cpal playback
```

Per-user pipeline allows:
- Individual volume adjustment
- Per-user noise reduction or gating
- Muting specific users (via UI setting `suppress = true` always)

### UI Configuration

The UI provides:

1. **TX Pipeline Settings panel:**
   - Toggle each processor on/off
   - Configure processor-specific settings
   - Visual audio level meter (using `level_db` from result)

2. **RX Defaults panel:**
   - Configure default pipeline for all users
   - Same controls as TX

3. **Per-User RX (in user list):**
   - Volume slider (maps to GainProcessor)
   - "Use custom settings" checkbox
   - If custom: full pipeline config like TX
   - Local mute button (separate from server mute status)

4. **Global RX Post-processing (optional):**
   - Compressor or other processing after mixing
   - Applies to final mixed output

### Default Configurations

**TX Pipeline (default):**
1. DenoiseProcessor (enabled)
2. VadProcessor (disabled by default; enable for voice-activated transmission in Continuous mode)

**RX Pipeline (default per-user):**
1. GainProcessor at 0 dB

**RX Global Post-processing (default):**
- None (passthrough)

### Sample Rate and Frame Size

- Capture and playback use hardware's preferred sample rate (typically 48kHz)
- Processors adapt to the actual sample rate
- Frame size is configurable (default 20ms)
- Opus encoder handles downsampling internally if bandwidth requires
- Future: automatic adjustment based on network conditions

---

### Jitter Buffer

Each remote user has their own jitter buffer and Opus decoder:

```rust
struct RemoteUserAudio {
    user_id: u64,
    decoder: OpusDecoder,
    jitter_buffer: JitterBuffer,
    rx_pipeline: AudioPipeline,
    is_talking: bool,
    received_eos: bool,       // End-of-stream received
    last_packet_time: Instant, // For DTX vs drop detection
}
```

**Jitter buffer behavior:**
- Packets inserted by sequence number (handles out-of-order arrival)
- Fixed initial delay (e.g., 60ms / 3 frames) before starting playback
- Track `last_packet_time` for DTX-aware drop detection
- Adaptive sizing based on observed jitter (optional enhancement, use fixed size for MVP)

**Packet loss detection:**
- Sequence numbers are consecutive - any gap indicates actual network loss
- Invoke PLC for each missing sequence number
- DTX keepalive frames (≤2 bytes) arrive every ~400ms during silence, decode to silence
- When `received_eos = true`: stop playback cleanly, no PLC needed

**Talking detection:**
- User is "talking" when packets are in their buffer and `!received_eos`
- User "stopped talking" when `end_of_stream` packet received
- On new packet after EOS: clear `received_eos`, user is talking again
- Fallback timeout (~500ms) for cases where EOS packet is lost

**RX pipeline lifecycle (server-state-driven):**
- When server state indicates user joined our room: create `RemoteUserAudio` (decoder + jitter buffer + pipeline)
- When server state indicates user left our room: destroy their `RemoteUserAudio`
- When we change rooms: destroy all existing pipelines, create new ones for users in new room
- Pipelines are created proactively, not lazily on first packet
- This ensures decoder state is initialized and ready before audio arrives
- Decoder state persists through DTX silence periods within the session

**Packet loss concealment:**
- Sequence gaps indicate real packet loss (sequence only increments on send)
- Invoke Opus PLC (`decoder.decode(None)`) for each missing sequence number
- PLC generates comfort audio to mask network drops
- When `received_eos = true`: no PLC, silence is intentional

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

### Current Development
1. **Audio Processing Pipeline**: Pluggable processor architecture for TX/RX
2. **Voice Activity Detection (VAD)**: Pipeline processor for voice-gated transmission in Continuous mode
3. **Per-user RX processing**: Volume control and optional processing per user, with the UI exposing a global rx pipeline that is internally implemented as applying the same pipeline to each user unless overridden.

### Later Features
1. User roles and ACLs for fine-grained permissions
2. File transfers using BitTorrent with server as tracker
3. Chat history snapshot sharing via torrent
4. Mobile client support (Android, iOS, WASM)
5. Server admin interface (CLI or web UI)
6. Server plugins (RPC interface, scripting, loadable modules)
7. Adaptive bitrate/quality based on network conditions
8. Advanced audio processors (AGC, equalizer, limiter)

---

## Crate Layout

```
crates/
├── api/           Protocol Buffers, message types, state structures, hash computation
├── pipeline/      Audio processing pipeline traits and registry (no dependencies on backend)
├── server/        Server binary: connections, room management, message routing
├── backend/       Client library: connection, audio, state management, built-in processors
└── egui-test/     Desktop GUI using egui
```

The `pipeline` crate is intentionally minimal with few dependencies, containing only:
- `AudioProcessor` trait
- `ProcessorFactory` trait  
- `ProcessorResult` struct
- `ProcessorConfig` struct (serde-serializable)
- `PipelineConfig` struct
- `ProcessorRegistry` 

This allows external crates to depend only on `pipeline` to implement custom processors,
without pulling in the full `backend` dependency tree.

---

## Open Questions

1. **ACL Schema**: Detailed permission list, inheritance rules, conflict resolution
2. **Audio Tuning**: Optimal jitter buffer size, FEC settings, bitrate targets
3. **TLS/ACME**: Challenge type, built-in vs external proxy
