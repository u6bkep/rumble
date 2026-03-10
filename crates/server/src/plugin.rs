//! Server plugin system.
//!
//! Plugins are compile-time Rust crates that extend the server. Each plugin
//! gets a [`ServerCtx`] providing controlled access to messaging, state queries,
//! stream creation, and persistence.

use crate::{
    persistence::Persistence,
    state::{ClientHandle, ServerState},
};
use anyhow::Result;
use api::proto::Envelope;
use quinn::RecvStream;
use std::sync::Arc;
use uuid::Uuid;

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
        let frame = api::encode_frame(&envelope);
        client
            .send_frame(&frame)
            .await
            .map_err(|e| anyhow::anyhow!("failed to send to client {user_id}: {e}"))?;
        Ok(())
    }

    /// Broadcast an envelope to all clients in a room.
    pub async fn broadcast_room(&self, room_id: Uuid, envelope: Envelope) -> Result<()> {
        let members = self.state.get_room_members(room_id).await;
        let frame = api::encode_frame(&envelope);
        for uid in members {
            if let Some(client) = self.state.get_client(uid) {
                if let Err(e) = client.send_frame(&frame).await {
                    tracing::warn!(user_id = uid, "failed to send to room member: {e}");
                }
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
