# Rumble

Rumble is a voice + text chat application written in Rust, in the spirit of Mumble.
Users join a hierarchical tree of rooms and talk over low-latency voice (Opus)
and text chat. It uses a client/server architecture with a QUIC transport, Ed25519
identities, and a permission/ACL system modeled loosely on Mumble.

> **Status:** active development. Interfaces and on-disk formats may change between
> commits.

## Features

- **Voice chat** — Opus at 48 kHz over unreliable QUIC datagrams, with per-peer jitter
  buffers and a pluggable audio-processing pipeline (denoise, VAD, gain).
- **Text chat** — per-room messages and direct messages, with file-share attachments via
  a server relay.
- **Hierarchical rooms** — a tree of rooms with per-room and inherited permissions.
- **Identity & auth** — Ed25519 key-based authentication, with optional ssh-agent
  identities and encrypted-at-rest identity files (Argon2 + ChaCha20Poly1305).
- **Access control** — server-wide permission groups plus per-room grant/deny ACLs, kick
  and ban, server-mute, and session-only superuser elevation. See
  [`docs/acl-system.md`](docs/acl-system.md).
- **Native desktop GUI** — `rumble-damascene`, built on the [damascene](https://github.com/computer-whisperer/damascene)
  UI library, with global push-to-talk hotkeys (incl. the XDG GlobalShortcuts portal on
  Wayland).
- **Mumble bridge** — `mumble-bridge` proxies a real Mumble server's clients into a Rumble
  server as participants.

## Requirements

- A recent Rust toolchain (the workspace uses **edition 2024**); a **nightly** toolchain
  with `rustfmt` is used for formatting.
- Native libraries for the desktop client: a working audio stack for [`cpal`], Opus, and
  (for the video lightbox) `libmpv`.

## Quick Start

```bash
# Build everything
cargo build

# Run the server (binds to [::]:5000 by default; writes a rumble-server.toml
# on first run that you can edit afterwards)
cargo run --bin server

# Run the desktop GUI client
cargo run -p rumble-damascene

# ...with debug logging
RUST_LOG=debug cargo run -p rumble-damascene
```

The server reads configuration from `rumble-server.toml` (auto-created with commented
defaults) and accepts CLI overrides (`--bind`, `--data-dir`, `--cert-dir`, `--domain`,
`--log-level`) and a few environment variables (e.g. `RUMBLE_SERVER_PASSWORD`,
`RUMBLE_WELCOME_MESSAGE`).

## Server administration

The `server` binary has admin subcommands that operate on the sled database and exit. Each
takes an optional trailing data directory (default `data`):

```bash
server add-admin <base64-public-key>                  # add a key to the admin group
server set-sudo-password <password>                   # set the superuser elevation password
server add-controller <base64-public-key>             # grant MANAGE_PARTICIPANTS (e.g. a bridge)
server set-participant-group <base64-public-key> <group>  # default group for a controller's participants
```

See [`docs/acl-system.md`](docs/acl-system.md) for the permission model behind these.

## Architecture

```
rumble-damascene (GUI)  →  rumble-client (engine)  →  rumble-protocol (QUIC + protobuf)  →  server
                                                                                          ▲
                                                                          mumble-bridge ──┘ (controller)
```

The GUI reads a shared `State` snapshot each frame and sends fire-and-forget commands; a
**projection task** is the sole writer of that state. The server is largely lock-free
(atomic IDs, `DashMap` per-client, a single `RwLock` over room/membership data) and relays
voice from snapshots to avoid holding locks during I/O.

Start with [`docs/architecture.md`](docs/architecture.md) for the full picture. Deeper
references:

- [`docs/deployment.md`](docs/deployment.md) — docker-compose + host certbot: TLS, renewal hot-reload, bootstrap, bridge
- [`docs/quic-protocol.md`](docs/quic-protocol.md) — wire protocol, framing, auth, state sync, voice
- [`docs/acl-system.md`](docs/acl-system.md) — permissions, groups, per-room ACLs, evaluation
- [`docs/audio-subsystem.md`](docs/audio-subsystem.md) — Opus codec, jitter buffers, processor pipeline
- [`docs/testing-strategy.md`](docs/testing-strategy.md) — integration tests + the damascene UI lint pipeline
- [`docs/v2-architecture.md`](docs/v2-architecture.md) — *historical* platform-abstraction design

## Repository layout

| Crate | Role |
|---|---|
| `rumble-protocol` | Wire protocol: protobuf types, QUIC framing, BLAKE3 state hashing |
| `rumble-client` | Platform-agnostic client engine: connection + audio + projection tasks |
| `rumble-client-traits` | Platform-agnostic traits (transport, audio, codec, keys, storage) |
| `rumble-desktop` | Native desktop `Platform` impl: quinn, cpal, opus, ed25519 |
| `rumble-desktop-shell` | Settings store, encrypted identity files, ssh-agent, global hotkeys |
| `rumble-audio` | Pluggable audio-processor framework (denoise, VAD, gain) |
| `rumble-damascene` | The desktop GUI client |
| `rumble-video` | Thin libmpv wrapper for the video lightbox |
| `server` | Server binary: rooms, members, ACL, voice/chat relay, sled persistence |
| `mumble-bridge` | Bidirectional Mumble ↔ Rumble proxy |

## Development

```bash
cargo test                 # run the test suite
cargo +nightly fmt         # format (imports grouped by crate; see rustfmt.toml)
scripts/ci.sh              # local pre-flight: rustfmt --check + clippy -D warnings
```

UI changes are validated headlessly with the `dump_bundles` tool, which renders canned
scenes through the real app event path and emits SVG/lint artifacts:

```bash
cargo run -p rumble-damascene --bin dump_bundles
```

See [`docs/testing-strategy.md`](docs/testing-strategy.md) for details.

## License

Licensed under the [MIT License](LICENSE).
