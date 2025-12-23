## Implementation Plan

This project is a simple VOIP application written in Rust. The primary use case is to have rooms where users can join and talk to each other using voice communication and text chat. The application uses a client–server architecture: the server handles room management and user connections, while the client provides the user interface and audio I/O.

### Server

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

The server will be secured with TLS and quic. It will:

- Automatically generating a self-signed certificate
- Using a provided certificate and private key
- Using ACME to obtain trusted certificates from a service like Let’s Encrypt

When using ACME, the server must be configured with a domain name.

### Client

The client side will start with a library crate that provides core functionality:

- Connecting to the server
- Handling authentication and identity
- Managing client-side state (current room, user status, etc.)
- Audio capture and playback

Audio I/O will use the `cpal` crate for cross-platform input and output, and a rust wrapper around the Opus codec for audio encoding and decoding.

On top of this library, a binary crate will provide a graphical user interface using the EGUI library. The initial target platforms are desktop Linux and Windows. Later, mobile platforms (e.g., Android, iOS, wasm) may be supported by reusing the same client library with different UI layers.

Client identity and authentication:

- Each client is identified by a persistent client ID, implemented as a public/private keypair generated on first run and stored locally.
- The public key is sent to the server for authentication; the private key never leaves the client.
- The user-visible name is distinct from the client ID and can be changed by the user.

Voice detection will be the primary mode of voice transmission; the exact keybinding and behavior will be defined later.

client will store its persistent data (keypair, config) in a standard XDG-compliant config directory.

### API

A separate library crate will define the API for communication between client and server. The API will be defined using Protocol Buffers and implemented with Prost.

the API crate will hold anything both client and server need to know about the protocol, including message types, state definitions, and serialization logic.

#### Transport

- All communication between client and server will use QUIC via the `quinn` crate.
- The API crate will be transport-agnostic where possible, focusing on message types and state, not concrete transport types.
- Voice data will be sent as Opus-encoded QUIC datagrams for low-latency communication. (48khz, mono, variable bit rate and frame size, VOIP mode DTX/FEC)
- Text chat and state changes will use QUIC streams for reliable, ordered delivery and stream prioritization.
- larger data (e.g., images, file transfers) will use bit torrent using `librqbit`, and the server acting as a tracker over quic.

#### State Model and Synchronization

Both client and server maintain a shared logical state. This state includes:

- The room tree (hierarchy of rooms)
- Which users are in which rooms
- User state (muted, deafened, roles, ACLs, etc.)
- torrent file list and metadata.

When a client connects:

1. The server sends the current full state to the client.
2. Subsequent changes are sent as incremental state change messages.

The internal state will be hashable. Every message that modifies the state includes the hash of the expected current state. This allows a participant (client or server) to detect synchronization issues and request a full state update if necessary.

Clients and server are peers in terms of proposing state changes. Both sides can initiate changes (for example, user moves rooms, mutes themselves, or server applies an admin action). The server is the authority:

- It accepts or rejects changes based on access control rules.
- Accepted changes are propagated to all connected clients.

Conflict resolution is “first message processed wins”. If a client’s change is rejected because the expected state hash does not match, the client is expected to:

- Handle the rejection gracefully.
- Refresh or resync its local state from the server as needed.

The state hash will be computed over a canonical serialization of the room tree and user states. The exact serialization format and hash algorithm will be decided later.

when a client sends larger data:
1. the client creates a magnet link for the data, and send the metadata to the server.
2. the client sends a text message to the room with the magnet link.
3. other clients show the link as a download button, and when clicked, start downloading the data using bittorrent.

the torrent data will not be persisted. clients will clean up old data on exit, or after a configurable timeout.

old session data, such as chat history, will not be persisted. but, a client connecting to a room with existing users will have the option to use the torrent system to request a cononicalized snapshot of room history from other clients in the room, including what other large files are still available. the server will drop old tracker data when no more seeders are available.

### Audio

Audio will be encoded using the Opus codec for efficient compression and low latency.

- Clients capture audio via `cpal`, encode it as Opus, and send Opus packets to the server as QUIC datagrams.
- The server does not need to decode and re-encode audio streams; it simply relays encoded audio packets between the appropriate clients in a room.

Details such as sample rate, bitrate, frame size, and Opus features (FEC, DTX, etc.) will be chosen later, based on latency and quality requirements.

### Authentication, Identity, and ACLs

Authentication:

- A simple global password will be used for authentication.
- The password is configured per server instance.
- Clients must present this password (combined with their keypair) when connecting or registering.

Identity:

- Users are primarily identified by their public keys (client IDs).
- There is a default unauthenticated role for unknown or untrusted public keys.

Access control:

- All rooms have an ACL system.
- ACLs can grant or deny specific permissions to users or roles.
- Rules can inherit from parent rooms (“trickle down”) and be overridden in child rooms.

Examples of permissions (to be defined precisely later):

- Connect to the server
- Join a room
- Speak/send voice
- Send text messages
- Create or delete rooms
- Move other users
- Edit ACLs

The persistent state will include:

- User accounts/identities (public keys, usernames, roles)
- Room tree structure
- ACLs attached to rooms and/or roles

### Features

Initial feature set:

1. User authentication (global server password + keypair-based identity)
2. Persistent users and rooms stored by the server
3. Room management (create, join, leave, delete)
4. Voice communication (push-to-talk, Opus over QUIC datagrams)
5. Text chat over QUIC streams


later features:
1. User roles and ACLs for fine-grained permissions
2. File transfers using BitTorrent (librqbit) with server as tracker
3. chat history snapshot sharing via torrent
4. Mobile client support (Android, iOS, wasm)
5. Server admin interface (CLI or web UI)
6. server plugins for custom features (RPC interface, scripting, or loadable modules)

### Open Questions / TBD

1. Detailed ACL schema (permission list, inheritance rules, conflict resolution).
2. Exact state hashing scheme (serialization format, hash algorithm, full-state vs per-subtree).
3. Audio parameters (sample rate, bitrate, mono/stereo, Opus options).
4. TLS/ACME integration details (challenge type, built-in vs external proxy, configuration model).
5. Reconnect behavior and error handling (automatic reconnect/backoff, state resync UX, how rejections are surfaced).


### layout of crates
- `crates/api`: Protocol Buffers definitions, message types, state structures, serialization logic.
- `crates/server`: Server application, handling connections, room management, state tracking, message routing.
- `crates/backend`: Client library, connection management, authentication, state tracking, audio I/O.
- `crates/egui-test`: Client application with EGUI-based graphical user interface.