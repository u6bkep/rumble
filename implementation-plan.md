# Rumble Feature Specification

Rumble is a voice chat application (similar to Discord/Mumble) written in Rust. Users can join hierarchical rooms and communicate via voice and text chat.

---

## Current Features

### Voice Communication
- **Push-to-Talk (PTT)**: Hold a key to transmit voice
- **Continuous mode**: Always transmitting (can be combined with VAD for voice-activated)
- **Mute/Deafen**: Self-mute to stop transmitting, self-deafen to stop receiving audio
- **Talking indicators**: Visual feedback showing who is currently speaking
- **Per-user volume**: Adjust volume for individual users
- **Local mute**: Mute specific users locally (only affects your audio)

### Audio Processing
- **Noise suppression**: RNNoise-based denoising on transmitted audio
- **Voice Activity Detection (VAD)**: Energy-based detection to suppress transmission when not speaking
- **Configurable encoder settings**: Bitrate, complexity, frame size, FEC, DTX
- **Audio device selection**: Choose input/output devices, refresh device list

### Rooms
- **Hierarchical room tree**: Rooms can have child rooms (like folders)
- **Room operations**: Create, delete, rename, move rooms
- **Auto-join**: Optionally rejoin last room on connect
- **Drag-and-drop**: Move rooms in the tree via drag-and-drop

### Text Chat
- **Room chat**: Send text messages visible to everyone in your room
- **Timestamps**: Optional message timestamps with configurable format
- **System messages**: Connection status and events shown in chat

### File Sharing
- **BitTorrent-based transfers**: Share files with room members via magnet links
- **Transfer management**: Pause, resume, cancel active transfers
- **Progress tracking**: See upload/download progress and speeds
- **Auto-download rules**: Configure automatic downloads by MIME type and size limits
- **P2P with NAT traversal**: libp2p with DCUtR hole punching and relay fallback
- **Image paste**: Ctrl+V to share clipboard images directly
- **Drag-and-drop**: Drag files into the chat area to share

### Identity & Authentication
- **Ed25519 keypairs**: Cryptographic identity for each user
- **SSH agent integration**: Use keys from your SSH agent
- **Local key storage**: Generate and store keys locally (optional password protection)
- **User registration**: Reserve your username on a server
- **Server password**: Servers can require a password for new users

### Connection
- **QUIC transport**: Modern, encrypted transport protocol
- **Self-signed certificate handling**: Accept/reject untrusted server certificates
- **Certificate persistence**: Remember accepted certificates
- **Auto-connect**: Optionally connect automatically on launch

### Settings
- **Persistent settings**: All preferences saved across sessions
- **Categorized settings panel**: Connection, Devices, Voice, Processing, Encoder, Chat, File Transfer, Statistics

---

## User Flows

### First Launch
1. App shows key setup dialog with three options:
   - Use existing key from SSH agent (list available keys)
   - Generate new key and add to SSH agent
   - Generate key stored locally by Rumble
2. User selects method and completes setup
3. Key is saved for future sessions

### Connecting to a Server
1. Enter server address (e.g., `myserver.com:5000`)
2. Enter display name
3. If server requires password and you're unknown, enter password
4. Click Connect
5. If server has self-signed certificate:
   - Dialog shows certificate fingerprint and server name
   - Accept to trust and save, or Reject to cancel
6. On success, join the Root room and see room tree + user list

### Voice Chat
1. Select voice mode in settings:
   - PTT: Hold spacebar (or configured key) to talk
   - Continuous: Always transmitting (use with VAD for voice-activated)
2. Join a room by clicking it in the tree
3. Your audio is transmitted to others in the same room
4. Talking indicators light up when users speak
5. Use mute/deafen buttons to control your audio state

### Sharing a File
1. Share via one of:
   - Click the share button and select file from picker
   - Drag file into chat area
   - Ctrl+V to paste image from clipboard
2. File is shared - magnet link appears in chat
3. Others click the magnet link to download (or auto-download if rules match)
4. Track progress in the Transfers panel
5. Pause/resume/cancel as needed

### Managing Rooms
1. Right-click a room for context menu:
   - Create child room
   - Rename room
   - Delete room (must be empty)
2. Drag a room onto another room to move it
3. Confirm move in dialog

---

## Architecture (High-Level)

```
┌─────────────────────────────────────────────────────┐
│              Desktop GUI (egui)                     │
│              - Room tree view                       │
│              - User list                            │
│              - Chat panel                           │
│              - Settings                             │
└───────────────────────┬─────────────────────────────┘
                        │ Commands / State reads
                        ▼
┌─────────────────────────────────────────────────────┐
│              Backend Library                        │
│   - Connection management (QUIC)                    │
│   - Audio capture/playback (cpal)                   │
│   - Opus encoding/decoding                          │
│   - Audio processing pipeline                       │
│   - BitTorrent file sharing                         │
│   - State synchronization                           │
└───────────────────────┬─────────────────────────────┘
                        │
         ┌──────────────┼──────────────┐
         ▼              ▼              ▼
    ┌────────┐    ┌──────────┐   ┌────────────┐
    │  API   │    │ Pipeline │   │   Server   │
    │ proto  │    │  audio   │   │  handlers  │
    │ types  │    │ process  │   │  state     │
    └────────┘    └──────────┘   └────────────┘
```

### Transport
- **Control messages**: QUIC reliable streams (room state, chat, auth)
- **Voice data**: QUIC unreliable datagrams (low-latency audio)
- **File transfers**: libp2p with DCUtR hole punching, noise encryption, and relay fallback

### State Model
- Server maintains authoritative state (rooms, users, memberships)
- Clients sync state via incremental updates with hash verification
- Hash mismatch triggers full state resync

### Audio Flow
- Capture → Pipeline (denoise, VAD) → Opus encode → QUIC datagram → Server relay
- Server relay → QUIC datagram → Jitter buffer → Opus decode → Pipeline (gain) → Playback

---

## Roadmap

### Near-term
- [x] Auto-download rules for file transfers *(configurable MIME patterns and size limits in settings)*
- [x] Image paste support in chat *(Ctrl+V to share clipboard images)*
- [x] P2P NAT traversal for file transfers *(libp2p with DCUtR hole punching and relay fallback)*
- [x] Keyboard shortcuts for common actions *(global hotkeys for PTT, Mute, Deafen; configurable in settings; Wayland fallback)*
- [x] Chat history sync *(P2P catchup for late joiners, ephemeral like file sharing)*

### Medium-term
- [ ] Access Control Lists (ACLs) for rooms *(role field exists in persistence, no enforcement)*
- [ ] User roles and permissions *(role infrastructure present, needs hierarchy/checking)*
- [ ] Admin console (manage users, rooms, registrations)
- [ ] Server-side room persistence improvements

### Long-term
- [ ] Mobile clients (Android, iOS)
- [ ] Web client (WASM) *(see [docs/wasm-support.md](docs/wasm-support.md))*
- [ ] Adaptive bitrate based on network conditions
- [ ] Advanced audio processors (AGC, equalizer, limiter)
- [ ] Server plugins/scripting
- [ ] ACME certificate management (Let's Encrypt)

---

## Technical Reference

For detailed technical information, see:
- [CLAUDE.md](CLAUDE.md) - Codebase architecture and build commands
- [crates/api/proto/api.proto](crates/api/proto/api.proto) - Protocol definitions
- [docs/hybrid-p2p-requirements.md](docs/hybrid-p2p-requirements.md) - P2P architecture details

### Crate Layout
```
crates/
├── api/           Protocol definitions and message types
├── pipeline/      Audio processor trait and registry
├── server/        Server binary
├── backend/       Client library (connection, audio, state, P2P)
├── egui-test/     Desktop GUI application
└── harness-cli/   Daemon-based CLI for automated GUI testing
```
