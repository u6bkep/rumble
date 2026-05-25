# Plugin Attachment Redesign

**Status:** Proposed.
**Motivation:** The current chat-attachment system bakes one specific plugin (`FileOffer` for the relay) into the wire schema, conflates upload lifecycle with chat message state, and produces invisible failure feedback. This redesign moves attachments to a generic plugin envelope and separates transfer state from chat state.

## Goals

1. **Plugin-namespaced attachments** so new attachment types (alternative file-transfer plugins, polls, voice memos) can ship without editing the core proto.
2. **One card per share in the sender's timeline.** Today a successful share produces three log entries on the sender's screen ("Sharing /path…", an invisible local optimistic card, and a server-echoed card). Target: one entry, transitioning visually from "uploading" to "uploaded" in place.
3. **Failures are visible.** Pre-flight rejection (oversize), server admission rejection, and post-upload broadcast failure all surface on the same card.
4. **Transfer state lives in the plugin, not in chat.** The chat layer holds a passive reference; the plugin owns progress, error, peers, and any other transport details. UI reads plugin state by transfer-id.

## Non-goals

- Multi-device sync of in-flight uploads. The sender's in-progress card is ephemeral and lives only on the originating client.
- Wire compatibility with old clients. Hard cutover — this is the only consumer.
- Persistent local chat history. Restart still drops `chat_messages` and rebuilds from peers via `RequestChatHistory`.

---

## Identifiers

Two distinct IDs, separate namespaces:

| ID | Owner | Purpose |
|---|---|---|
| **Chat message id** (`bytes`, 16-byte UUID) | chat layer | dedup across history sync, ordering, edit/reply targets |
| **Transfer id** (`string` UUID) | file-transfer plugin | identify the cached file in the plugin's transfer table; embedded in the plugin's payload |

These never alias. A failed share has a chat-message id but no broadcast; a single broadcast share has one chat-message id shared between sender's `remote_only` record and recipients' visible copy.

---

## Wire format

### `PluginAttachment` (replaces `ChatAttachment.oneof`)

```protobuf
// Drop the existing `oneof kind { FileOffer = 1 }`. Replace ChatAttachment with:
message ChatAttachment {
  // Reverse-DNS plugin identifier, e.g. "rumble.file_transfer.relay".
  // Clients without a matching renderer/handler fall back to `fallback_text`.
  string namespace = 1;
  // Schema version for the namespace's payload format. Plugin-defined.
  uint32 schema_version = 2;
  // Opaque plugin-defined payload (encoded however the plugin chooses).
  bytes payload = 3;
  // What to show when no plugin matches `namespace`. Required.
  string fallback_text = 4;
}
```

`FileOffer` and `chat_attachment::Kind` are removed from the proto entirely.

### Server: no echo to sender

`ChatMessage` broadcast in `server/handlers.rs` skips the sender's connection. This is a universal rule, not per-message — applies to all chat broadcasts (text, attachments, system events targeting a room).

The bridge owns its own Mumble-side echo if its protocol needs it; the Rumble server stays simple.

---

## Client data model

### `ChatMessage` (in `rumble-protocol/src/types.rs`)

Drop `phase: LifecyclePhase`. Add two booleans (both default `false`, both client-local only, neither serialized over the wire or into history-sync shares):

```rust
pub struct ChatMessage {
    // ... existing fields ...
    pub attachment: Option<ChatAttachment>,  // now generic
    pub local_only: bool,
    pub remote_only: bool,
}
```

| Flag | Sender renders? | Goes on broadcast? | Included in history-sync share? | Flag itself on the wire? |
|---|---|---|---|---|
| `local_only` | yes | no | no | n/a |
| `remote_only` | no | yes | yes | no — stripped on egress |

A message can never have both flags. Default-false is the normal case (rendered, broadcast, synced).

`remote_only` is essential, not optional: it's the sender's local copy of the broadcast they sent out, so that peers who join later and ask *the sender* for history get the message. The flag is sender-local — it's stripped on every wire egress (broadcast and history-sync share). After restart the sender's `remote_only` entries are gone with the rest of `chat_messages`; resyncing from peers brings them back without the flag, at which point the sender renders them as a receiver and can re-download via the cached share.

`LifecyclePhase` and `FailedUpload(Option<String>)` are removed. The card derives upload state from the plugin's `TransferStatus` keyed by transfer-id (inside the attachment payload). On the sender's client, a `local_only` card during upload reads `TransferStatus.progress` and `TransferStatus.error` directly. On a receiver's client, no `TransferStatus` exists until they click Download — same as today.

### Sender flow (file share via relay)

```
1. User drops file.
   → app.rs sends Command::ShareFile { path }.
   (Drop the synchronous "Sharing /path" Command::LocalMessage from app.rs:935.)

2. handle.rs ShareFile handler:
   a. Generate message_id (uuid::Uuid::new_v4()).
   b. Pre-flight validate (size, stat). On failure → see "Failure paths" below.
   c. ft.share(path) → returns FileOffer { transfer_id, name, size, mime, share_data }.
   d. Build ChatAttachment {
         namespace: "rumble.file_transfer.relay",
         schema_version: 1,
         payload: relay-specific encoding of {transfer_id, name, size, mime, share_data},
         fallback_text: format!("shared file \"{name}\" ({size})"),
      }.
   e. Insert local_only message into chat_messages with this attachment and message_id.
   f. Spawn watcher polling ft.transfers() for completion or error.

3. Card renders from plugin state:
   - in-flight: progress bar from TransferStatus.progress
   - error: TransferStatus.error string under "Upload failed" header
   - complete: "open / reveal" actions for the sender (status.local_path)

4. Watcher reports completion:
   → DeferredAction::FinalizeShareUpload { message_id, attachment }.
   → Connection task inserts a SECOND chat message with the SAME message_id,
     remote_only=true, same attachment.
   → Broadcast envelope built from this remote_only message (flag stripped on egress).
   → Server forwards to others, does NOT echo to sender.

5. Watcher reports failure:
   → No broadcast. The local_only card now renders the error from TransferStatus.error.
   → No remote_only entry created.
```

The sender ends up with two `chat_messages` entries with the same id: one `local_only` (visible card, derived from plugin state), one `remote_only` (invisible record, identical attachment content to what was broadcast). The `remote_only` entry IS included in history-sync shares (with the flag stripped on egress) — that's how peers who join the room later and ask the sender for history receive the share. Without it, the broadcast would be ephemeral from the sender's perspective and late-joining peers would miss it whenever they hit the sender first for history.

### Receiver flow

```
1. Server forwards ChatBroadcast.
2. handle.rs receive handler:
   a. Decode attachment → ChatAttachment { namespace, payload, fallback_text }.
   b. Dedup: if chat_messages already has a message with this id, drop the incoming
      one. (Covers history-sync re-arrival of a message we already broadcast.)
   c. Otherwise insert with local_only=false, remote_only=false.
3. UI: look up namespace in renderer registry.
   - "rumble.file_transfer.relay" → file-transfer renderer
   - unknown namespace → render fallback_text as plain text
```

### History sync

`ChatHistoryShareMessage` filters out any message with `local_only` from the outgoing share. `remote_only` messages **are** included, with the `remote_only` flag stripped on egress (it's a sender-local rendering hint, not a wire-level attribute). Receiving side dedups by message id (already implemented at handle.rs:2150).

---

## Plugin traits

### `FileTransferPlugin` (in `rumble-client-traits`)

Add one method:

```rust
pub trait FileTransferPlugin: Send + Sync + 'static {
    /// Reverse-DNS namespace that uniquely identifies this plugin's
    /// attachment payload format. Used by the receive dispatcher to
    /// route incoming ChatAttachments and by the UI to look up the
    /// matching renderer.
    fn namespace(&self) -> &'static str;

    // ... existing methods (share, download, transfers, cancel, ...) ...
}
```

The `share()` method's `FileOffer` return type stays (it's the plugin's internal API to its caller, not on the wire anymore). The caller in `handle.rs` is responsible for encoding the `ChatAttachment.payload` — the plugin's payload schema is plugin-private but the encode/decode helpers live with the plugin's crate.

### Backend dispatch

`rumble-client` keeps a single optional file-transfer plugin (today's setup). When a `ChatAttachment` arrives, the receive handler inserts the chat message verbatim — there's no backend-side dispatch step. The UI looks up `namespace` in its renderer registry; the renderer decodes the payload using the matching plugin's helper. Download is initiated by the UI calling `plugin.download(share_data)` after the user clicks the button on the card.

Future: when we have multiple file-transfer plugins, the backend grows a `HashMap<&'static str, Arc<dyn FileTransferPlugin>>` keyed by namespace.

---

## UI rendering (rumble-aetna)

### Renderer registry

```rust
// In rumble-aetna, somewhere central (app.rs or a new chat_attachments.rs).
struct AttachmentRenderer {
    render: fn(&ChatAttachment, &RenderContext) -> El,
}

static RENDERERS: Lazy<HashMap<&'static str, AttachmentRenderer>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("rumble.file_transfer.relay", AttachmentRenderer { render: relay_render });
    m
});
```

The `RenderContext` bundles whatever the renderer needs that isn't on the attachment: image cache, video thumbs, transfers map, the file-transfer plugin handle, pending-cancel state, etc. — basically the args currently passed to `file_offer_card`.

### Plugin-specific rendering

Aetna renderers are plugin-specific by design (per Ben's call). A `rumble.file_transfer.relay` renderer reads relay-specific payload fields (`share_data` interpretation, peer detail format, progress shape). A future `rumble.file_transfer.bittorrent` renderer would have its own renderer file with bittorrent-specific UI (peer list, seed/leech counts, ratio).

### Shared helpers

Content-rendering helpers stay generic and live in `rumble-aetna` outside any specific renderer:

- `image_preview(bytes, fit, hover_state)` — image card body
- `video_poster(thumb_bytes, duration)` — video card body
- `format_size(bytes)`, `format_duration(ms)`
- `progress_indeterminate(color)`, `progress_determinate(fraction, color)`

A plugin renderer calls these from inside its own card layout.

### Drop `Command::LocalMessage` for the "Sharing /path" preamble

Currently `app.rs:935` inserts a separate italic "Sharing /path" line synchronously. After the cutover, the card itself is the share record — drop the preamble.

`Command::LocalMessage` stays for genuine system text ("Connect to a server before sharing files", future "User X joined", etc.). Those don't have attachments.

---

## What gets removed

- `proto::FileOffer` and `chat_attachment::Kind` (proto generated types).
- `rumble_protocol::types::ChatAttachment::FileOffer` variant.
- `rumble_protocol::types::FileOfferInfo` (replaced by relay-plugin-private decoded form).
- `rumble_protocol::types::LifecyclePhase` and the `phase` field on `ChatMessage`.
- `State::set_message_phase`.
- `chat_attachment_from_proto` (replaced by attachment passthrough).
- `app.rs:935` "Sharing /path" `LocalMessage` insertion.
- `chat.rs:472` `is_local` short-circuit (no longer relevant once we go through namespaced renderers).
- Server-side `ChatMessage` echo to sender (server/handlers.rs ~836).

---

## What gets added

- `proto::ChatAttachment { namespace, schema_version, payload, fallback_text }`.
- `ChatMessage.local_only` and `ChatMessage.remote_only` boolean fields.
- `FileTransferPlugin::namespace()` trait method.
- Relay plugin payload encoder/decoder in `rumble-desktop` (private to that crate).
- Aetna `RENDERERS` registry + `relay_render` function.
- Server skip-sender filter in `broadcast_chat_message`.

---

## Open questions

1. **Bridge audit.** Confirm `mumble-bridge` still works after the no-echo-to-sender change. It connects as a normal client and sends ChatMessages on behalf of virtual users; if it relied on receiving its own messages back to forward them to Mumble, that needs fixing in the bridge code. Audit before cutover.

## Resolved decisions (recorded for future reference)

- **Relay payload schema is public**, defined in `rumble-desktop` and re-exported, so the aetna renderer can `prost::decode` directly without a runtime call into the plugin.
- **No server-side ACL on attachments.** The server forwards `ChatAttachment` unchanged; admission control lives in the plugin's upload stream handshake (size, quota, room mismatch), not in chat-message broadcast.
- **Keep `remote_only`.** It's required for history-sync correctness — without it, late-joining peers who ask the sender for history wouldn't see the share.

---

## Migration steps (hard cutover)

Order matters — proto first so types compile, then traits, then internals, then UI.

1. **Proto change.** Replace `FileOffer` and `oneof kind` in `api.proto` with the new flat `ChatAttachment`. Regenerate.
2. **Protocol types.** Update `rumble_protocol::types::ChatAttachment` to mirror the new proto. Delete `FileOfferInfo`, `LifecyclePhase`, `phase` field, `set_message_phase`. Add `local_only` / `remote_only` to `ChatMessage`. Update `chat_message_to_proto` / `chat_attachment_from_proto`.
3. **Plugin trait.** Add `FileTransferPlugin::namespace()`. Implement on the relay plugin → `"rumble.file_transfer.relay"`.
4. **Relay plugin payload.** Define the relay-private payload schema (a new proto message in api.proto or a prost module local to `rumble-desktop`). Add `encode_payload` / `decode_payload` helpers. Plugin's `share()` and the receive path use them.
5. **rumble-client ShareFile rework.** Generate message_id up front. Insert local_only card (with namespaced attachment) before fast-fail/share, so failures render visibly. Spawn watcher; on completion insert remote_only entry and broadcast.
6. **Server.** Skip sender in `broadcast_chat_message`. Drop any references to the typed `FileOffer` (server doesn't inspect it, but the type goes away from the import list).
7. **Receive handler.** Dedup by message id before inserting. Drop the old `chat_attachment_from_proto` typed conversion.
8. **History sync.** Filter `local_only` out of share-outgoing; strip the `remote_only` flag (but keep the message) so peers receive a clean copy.
9. **Aetna renderer registry.** New module. Move file-offer-specific code from `chat.rs` into `attachments/relay.rs` (or similar). Keep generic helpers (image_preview, format_size, progress_indeterminate) at chat.rs / module top.
10. **Aetna chat.rs.** Drop `is_local` short-circuit; the new code path dispatches by namespace for messages with attachments and renders plain text + system styling for `is_local` without attachment.
11. **app.rs:935.** Drop the "Sharing /path" preamble.
12. **Bridge audit.** Run a Mumble↔Rumble round-trip end-to-end after server skip-sender lands.
13. **dump_bundles scenes.** Add `file_share_pending`, `file_share_failed`, `file_share_complete` scenes so the new states have lint/SVG coverage.

Each step compiles independently if you're careful about ordering — steps 2 and 3 together cause a wide ripple, so plan to do them in one sitting.
