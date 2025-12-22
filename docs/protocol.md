# Rumble Wire Protocol (Phase 1)

This document summarizes the client/server control protocol used over QUIC in Phase 1.

## Transport & Framing
- **QUIC streams:** Control messages flow over a single bidirectional QUIC stream per connection.
- **ALPN:** `rumble`.
- **Framing:** Each protobuf message is sent as a 4-byte big-endian length prefix followed by the protobuf-encoded payload.
  - Prefix: `u32` number of bytes for the subsequent protobuf.
  - Decoder may accumulate bytes and parse when a full frame is present.

## Top-Level Envelope
All messages are wrapped in `Envelope`:
- `state_hash: bytes` — reserved (unused in Phase 1).
- `payload: oneof`
  - `ClientHello`
  - `ServerHello`
  - `ChatMessage`
  - `Disconnect`
  - `ServerEvent`

## Messages
- `ClientHello { client_name: string, password: string }`
  - Sent by client immediately after opening the control stream.
  - `password` may be empty; if server requires a password via `RUMBLE_SERVER_PASSWORD`, it must match.

- `ServerHello { server_name: string }`
  - Sent by server upon accepting `ClientHello`.

- `ChatMessage { sender: string, text: string }`
  - Sent by client to server.

- `ServerEvent { oneof kind }`
  - `ChatBroadcast { sender: string, text: string }` — broadcast of chat messages to all clients.
  - `KeepAlive { epoch_ms: uint64 }` — periodic server ping.

- `Disconnect { reason: string }`
  - Either side may send to indicate graceful termination.

## Keep-Alive & Timeouts
- **Server Keep-Alive Interval:** 15 seconds.
  - For each connected client, the server sends `ServerEvent { KeepAlive { epoch_ms } }`.
  - `epoch_ms` is milliseconds since Unix epoch (system clock).
- **Client Echo:** Upon receiving `KeepAlive`, the client immediately echoes a `ServerEvent { KeepAlive { epoch_ms } }` back to the server, preserving the timestamp.
- **Timeouts (Phase 1 behavior):**
  - Server does not yet enforce disconnection based on missing echoes; logs failures only when write errors occur.
  - Future phases may add: if no keep-alive echo is observed within `2 * interval` (30s), consider the client unhealthy and close the stream.

## Disconnect Semantics
- **Client-initiated:** Client sends `Envelope { Disconnect { reason } }`. Server logs the reason and gracefully finishes the send stream.
- **Server-initiated:** Server may send `Disconnect` (e.g., auth failure) then reset/finish the stream.
- **Stream closure:** After `Disconnect`, each side should close its send half (`finish`) and stop background tasks associated with the stream.

## Authentication (optional in Phase 1)
- If env var `RUMBLE_SERVER_PASSWORD` is set and non-empty, the server expects `ClientHello.password` to match. On mismatch, server resets the stream after logging.

## Ordering & Concurrency
- Messages are independent frames; application logic processes frames in arrival order per stream.
- Broadcasts are sent to all currently registered clients upon chat receipt.

## Versioning
- Package: `rumble.api.v1`.
- Changes in Phase 1:
  - Added `ServerEvent::KeepAlive` and top-level `Disconnect` to `Envelope`.

