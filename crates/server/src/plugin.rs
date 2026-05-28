//! Server plugin system.
//!
//! Plugins are compile-time Rust crates that extend the server. Each plugin
//! gets a [`ServerCtx`] providing controlled access to messaging, state queries,
//! stream creation, and persistence.

use crate::{
    handlers,
    persistence::Persistence,
    state::{ClientHandle, OwnerId, ServerState},
};
use anyhow::Result;
use prost::Message;
use quinn::{RecvStream, SendStream};
use rumble_protocol::proto::{self, Envelope};
use std::sync::Arc;
use uuid::Uuid;

/// Maximum length-prefixed plugin frame the server will accept on a plugin
/// stream. Frames are length-prefixed with `u32 BE`; this caps the prefix so
/// a malicious client can't make us allocate gigabytes for the frame buffer.
pub const MAX_PLUGIN_FRAME_BYTES: usize = 64 * 1024;

/// Read a length-prefixed (`u32 BE`) byte frame from a plugin stream.
///
/// Errors if the framed length exceeds [`MAX_PLUGIN_FRAME_BYTES`].
pub async fn read_length_prefixed(recv: &mut RecvStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let msg_len = u32::from_be_bytes(len_buf) as usize;
    if msg_len > MAX_PLUGIN_FRAME_BYTES {
        anyhow::bail!("plugin frame too large ({msg_len} bytes, max {MAX_PLUGIN_FRAME_BYTES})");
    }
    let mut buf = vec![0u8; msg_len];
    recv.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write a length-prefixed (`u32 BE`) byte frame to a plugin stream.
pub async fn write_length_prefixed(send: &mut SendStream, data: &[u8]) -> Result<()> {
    let len_bytes = (data.len() as u32).to_be_bytes();
    send.write_all(&len_bytes).await?;
    send.write_all(data).await?;
    Ok(())
}

/// Send an `OK` [`proto::PluginStreamAck`] frame, accepting the incoming
/// payload that follows on the same stream.
pub async fn send_plugin_ack_ok(send: &mut SendStream) -> Result<()> {
    let ack = proto::PluginStreamAck {
        status: proto::PluginAckStatus::Ok.into(),
        code: 0,
        error: String::new(),
    };
    write_length_prefixed(send, &ack.encode_to_vec()).await
}

/// Send a `REJECTED` [`proto::PluginStreamAck`] frame and gracefully stop the
/// recv side so the peer doesn't keep streaming a body the server doesn't want.
///
/// The peer's pending body writes get a clean `STOP_SENDING(code)`. Because we
/// flush the ack first and then call `recv.stop`, well-behaved clients can
/// read the typed rejection reason before observing the stop on their write
/// half.
pub async fn reject_plugin_stream(
    send: &mut SendStream,
    recv: &mut RecvStream,
    code: u32,
    error: impl Into<String>,
) -> Result<()> {
    let ack = proto::PluginStreamAck {
        status: proto::PluginAckStatus::Rejected.into(),
        code,
        error: error.into(),
    };
    let encoded = ack.encode_to_vec();
    write_length_prefixed(send, &encoded).await?;
    // Stop the body half. Errors here mean the client already gave up
    // (stream finished or reset), which is fine — we still delivered the ack.
    let _ = recv.stop(quinn::VarInt::from_u32(code));
    Ok(())
}

/// First frame on a plugin-owned QUIC stream.
/// The server reads this to dispatch the stream to the correct plugin.
#[derive(Debug, Clone)]
pub struct StreamHeader {
    /// Plugin name (e.g. "file-relay", "tracker").
    pub plugin: String,
    /// Plugin-specific metadata (e.g. transfer ID, target user).
    pub metadata: Vec<u8>,
}

impl StreamHeader {
    /// Encode a stream header to bytes.
    ///
    /// Wire format: `[plugin_name_len: u16][plugin_name: bytes][metadata: remaining]`
    pub fn encode(&self) -> Vec<u8> {
        let name_bytes = self.plugin.as_bytes();
        let mut buf = Vec::with_capacity(2 + name_bytes.len() + self.metadata.len());
        buf.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&self.metadata);
        buf
    }

    /// Decode a stream header from a QUIC receive stream.
    ///
    /// Reads a u16 plugin name length, then the name, then all remaining
    /// bytes as metadata.
    pub async fn decode(stream: &mut RecvStream) -> Result<Self> {
        // Read plugin name length (2 bytes, big-endian u16)
        let mut len_buf = [0u8; 2];
        stream.read_exact(&mut len_buf).await?;
        let name_len = u16::from_be_bytes(len_buf) as usize;

        // Read plugin name
        let mut name_buf = vec![0u8; name_len];
        stream.read_exact(&mut name_buf).await?;
        let plugin = String::from_utf8(name_buf).map_err(|e| anyhow::anyhow!("invalid UTF-8 in plugin name: {e}"))?;

        // Read remaining bytes as metadata
        let metadata = stream.read_to_end(64 * 1024).await?;

        Ok(Self { plugin, metadata })
    }
}

/// What plugins can access on the server.
///
/// Provides controlled but powerful access to server capabilities.
/// Plugins receive a reference to this during callbacks.
pub struct ServerCtx {
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
}

impl ServerCtx {
    /// Create a new server context.
    pub fn new(state: Arc<ServerState>, persistence: Option<Arc<Persistence>>) -> Self {
        Self { state, persistence }
    }

    /// Send a proto envelope to a specific connected client.
    pub async fn send_to(&self, user_id: u64, envelope: Envelope) -> Result<()> {
        let client = self
            .state
            .get_client(user_id)
            .ok_or_else(|| anyhow::anyhow!("client {user_id} not found"))?;
        let frame = rumble_protocol::encode_frame(&envelope);
        client
            .send_frame(&frame)
            .await
            .map_err(|e| anyhow::anyhow!("failed to send to client {user_id}: {e}"))?;
        Ok(())
    }

    /// Open a new bi-directional QUIC stream to a connected client.
    ///
    /// Writes the given [`StreamHeader`] as the first frame so the remote end
    /// can dispatch it to the correct plugin. Returns the raw stream pair for
    /// the caller to use.
    pub async fn open_stream_to(&self, user_id: u64, header: StreamHeader) -> Result<(SendStream, RecvStream)> {
        let client = self
            .state
            .get_client(user_id)
            .ok_or_else(|| anyhow::anyhow!("client {user_id} not found"))?;
        let (mut send, recv) = client.conn.open_bi().await?;
        send.write_all(&header.encode()).await?;
        Ok((send, recv))
    }

    /// Broadcast an envelope to all clients in a room.
    pub async fn broadcast_room(&self, room_id: Uuid, envelope: Envelope) -> Result<()> {
        let members = self.state.get_room_members(room_id).await;
        let frame = rumble_protocol::encode_frame(&envelope);
        for uid in members {
            if let Some(client) = self.state.get_client(uid)
                && let Err(e) = client.send_frame(&frame).await
            {
                tracing::warn!(user_id = uid, "failed to send to room member: {e}");
            }
        }
        Ok(())
    }

    /// Look up a connected client by user ID.
    pub fn get_client(&self, user_id: u64) -> Option<Arc<ClientHandle>> {
        self.state.get_client(user_id)
    }

    /// Get all user IDs in a room.
    pub fn get_room_members(&self, room_id: Uuid) -> Vec<u64> {
        // Use a blocking snapshot since the state data lock is held briefly.
        // For the async version, callers should use broadcast_room or similar.
        // We use try_read to avoid blocking; fall back to empty if lock is held.
        //
        // NOTE: This is a synchronous method that cannot await the RwLock.
        // For accurate room membership, prefer the async broadcast_room method.
        // This provides a best-effort snapshot.
        let data = self.state.snapshot_state_sync();
        data.memberships
            .iter()
            .filter(|(_, rid)| *rid == room_id)
            .map(|(uid, _)| *uid)
            .collect()
    }

    /// Get all user IDs in a room (async version).
    pub async fn get_room_members_async(&self, room_id: Uuid) -> Vec<u64> {
        self.state.get_room_members(room_id).await
    }

    /// Get a user's current room.
    pub fn get_user_room(&self, user_id: u64) -> Option<Uuid> {
        let data = self.state.snapshot_state_sync();
        data.memberships
            .iter()
            .find(|(uid, _)| *uid == user_id)
            .map(|(_, rid)| *rid)
    }

    /// Get a user's current room (async version).
    pub async fn get_user_room_async(&self, user_id: u64) -> Option<Uuid> {
        self.state.get_user_room(user_id).await
    }

    /// Access the persistence layer (if available).
    pub fn persistence(&self) -> Option<&Arc<Persistence>> {
        self.persistence.as_ref()
    }

    /// Access the shared server state.
    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    /// Post a chat message authored by `author_id` (a participant or client)
    /// into a specific `room`, regardless of the author's own room. Subject to
    /// the author's `TEXT_MESSAGE` ACL in `room`.
    ///
    /// Chat bots use this to reply into the room a triggering message came from
    /// without relocating the bot. A fresh message id and timestamp are minted;
    /// the reply is non-tree and carries no attachment.
    pub async fn post_chat_as(&self, author_id: u64, room: Uuid, text: String) -> Result<()> {
        let id = Uuid::new_v4().into_bytes().to_vec();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        handlers::broadcast_chat_in_room(
            &self.state,
            &self.persistence,
            author_id,
            room,
            text,
            false,
            id,
            ts,
            None,
        )
        .await
    }

    /// Mint a participant driven by this plugin — a roster member with no
    /// connection of its own (e.g. a bot). The returned [`ParticipantHandle`]
    /// drives the participant and removes it when dropped.
    ///
    /// `groups` are the participant's ACL groups; participants are subject to
    /// the same permission machinery as humans. They carry no verified identity,
    /// so they never gain an implicit username-group.
    pub async fn register_participant(
        &self,
        username: String,
        label: Option<String>,
        groups: Vec<String>,
    ) -> Result<ParticipantHandle> {
        // Each plugin participant gets a unique owner token so its lifetime is
        // independent; cleanup is handled by the returned handle on drop.
        let owner = OwnerId::Plugin(self.state.allocate_user_id());
        let user_id = handlers::create_participant(&self.state, owner, username, label, None, groups).await?;
        Ok(ParticipantHandle {
            user_id,
            state: self.state.clone(),
            persistence: self.persistence.clone(),
        })
    }
}

/// A handle to a plugin-owned participant. Drives the participant's room,
/// status, and chat; removes it from the roster when dropped.
pub struct ParticipantHandle {
    user_id: u64,
    state: Arc<ServerState>,
    persistence: Option<Arc<Persistence>>,
}

impl ParticipantHandle {
    /// The participant's server-assigned user_id.
    pub fn user_id(&self) -> u64 {
        self.user_id
    }

    /// Move this participant to a room. Returns false if the room is unknown.
    pub async fn move_to_room(&self, room_uuid: Uuid) -> Result<bool> {
        handlers::move_participant_to_room(&self.state, self.user_id, room_uuid).await
    }

    /// Update this participant's self-mute/deaf status.
    pub async fn set_status(&self, is_muted: bool, is_deafened: bool) -> Result<()> {
        handlers::set_participant_status_core(&self.state, self.user_id, is_muted, is_deafened).await
    }

    /// Post a chat message as this participant to its current room. Subject to
    /// the participant's ACL (TEXT_MESSAGE) like any other member.
    pub async fn post_chat(&self, text: String) -> Result<()> {
        let id = Uuid::new_v4().into_bytes().to_vec();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        handlers::broadcast_chat_as(&self.state, &self.persistence, self.user_id, text, false, id, ts, None).await
    }
}

impl Drop for ParticipantHandle {
    fn drop(&mut self) {
        // Best-effort removal + UserLeft broadcast when the plugin releases the
        // handle (including when the plugin itself is dropped).
        let state = self.state.clone();
        let user_id = self.user_id;
        tokio::spawn(async move {
            let _ = handlers::remove_participant(&state, user_id).await;
        });
    }
}

/// Factory for constructing a configured server plugin.
///
/// Each plugin provides a factory that knows how to deserialize its
/// own config section from TOML and construct a plugin instance.
/// The server passes raw TOML `[plugins.<name>]` sections to matching
/// factories at startup.
pub trait PluginFactory: Send + Sync {
    /// Plugin name — matches the `[plugins.<name>]` section in the config file.
    fn name(&self) -> &str;

    /// Create the plugin from an optional TOML config section.
    /// If `None`, the plugin should use its default configuration.
    fn create(&self, config: Option<toml::Value>) -> anyhow::Result<Box<dyn ServerPlugin>>;
}

/// A server plugin that can handle messages and streams.
///
/// Plugins are registered at server startup and receive callbacks
/// for incoming messages, new QUIC streams, and client disconnects.
#[async_trait::async_trait]
pub trait ServerPlugin: Send + Sync + 'static {
    /// Plugin name for logging and stream routing.
    fn name(&self) -> &str;

    /// Handle a proto envelope on the control stream.
    /// Return `Ok(true)` if handled, `Ok(false)` to pass to next handler.
    async fn on_message(&self, envelope: &Envelope, sender: &Arc<ClientHandle>, ctx: &ServerCtx) -> Result<bool>;

    /// A client opened a new QUIC stream addressed to this plugin.
    async fn on_stream(
        &self,
        header: StreamHeader,
        send: quinn::SendStream,
        recv: RecvStream,
        sender: &Arc<ClientHandle>,
        ctx: &ServerCtx,
    ) -> Result<()> {
        let _ = (header, send, recv, sender, ctx);
        Ok(())
    }

    /// Client disconnected -- clean up any plugin state.
    async fn on_disconnect(&self, client: &Arc<ClientHandle>, ctx: &ServerCtx) {
        let _ = (client, ctx);
    }

    /// Startup hook -- spawn background tasks.
    async fn start(&self, _ctx: &ServerCtx) -> Result<()> {
        Ok(())
    }

    /// Shutdown hook.
    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}
