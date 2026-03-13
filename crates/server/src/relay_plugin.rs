//! File transfer relay plugin.
//!
//! The sender opens a QUIC stream to the server with StreamHeader
//! `plugin = "file-relay"`. The first frame on the stream is a
//! length-prefixed [`RelayRequest`] protobuf. The server reads it,
//! opens a corresponding stream to the recipient with a [`RelayOffer`],
//! and then copies bytes bidirectionally until one side closes.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Result;
use api::proto::{self, envelope::Payload, relay_status};
use dashmap::DashMap;
use prost::Message;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    plugin::{ServerCtx, ServerPlugin, StreamHeader},
    state::ClientHandle,
};

/// Tracks a single active relay.
struct RelayState {
    sender_id: u64,
    recipient_id: u64,
    cancel: CancellationToken,
}

/// Shared state across spawned tasks and the plugin.
struct Inner {
    /// Active relays keyed by transfer_id.
    active: DashMap<String, RelayState>,
    /// Total bytes relayed (stats).
    bytes_relayed: AtomicU64,
}

/// Server-side plugin that relays file data between two clients.
///
/// The sender opens a plugin stream ("file-relay") with a [`RelayRequest`]
/// header. The server opens a matching stream to the recipient with a
/// [`RelayOffer`] header and then pipes bytes from sender to recipient.
pub struct FileTransferRelayPlugin {
    inner: Arc<Inner>,
}

impl FileTransferRelayPlugin {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                active: DashMap::new(),
                bytes_relayed: AtomicU64::new(0),
            }),
        }
    }

    /// Send a [`RelayStatus`] envelope to a client on the control stream.
    async fn send_status(
        ctx: &ServerCtx,
        user_id: u64,
        transfer_id: &str,
        status: relay_status::Status,
        error_msg: &str,
    ) {
        let envelope = proto::Envelope {
            state_hash: Vec::new(),
            payload: Some(Payload::RelayStatus(proto::RelayStatus {
                transfer_id: transfer_id.to_owned(),
                status: status.into(),
                error: error_msg.to_owned(),
            })),
        };
        if let Err(e) = ctx.send_to(user_id, envelope).await {
            warn!(user_id, "failed to send relay status: {e}");
        }
    }
}

impl Default for FileTransferRelayPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ServerPlugin for FileTransferRelayPlugin {
    fn name(&self) -> &str {
        "file-relay"
    }

    async fn on_message(
        &self,
        envelope: &proto::Envelope,
        _sender: &Arc<ClientHandle>,
        _ctx: &ServerCtx,
    ) -> Result<bool> {
        // We handle RelayStatus messages from recipients (accept/reject).
        match &envelope.payload {
            Some(Payload::RelayStatus(rs)) => {
                let status = relay_status::Status::try_from(rs.status).unwrap_or(relay_status::Status::Failed);
                match status {
                    relay_status::Status::Rejected => {
                        // Recipient rejected -- cancel the relay.
                        if let Some((_, relay)) = self.inner.active.remove(&rs.transfer_id) {
                            relay.cancel.cancel();
                            info!(transfer_id = %rs.transfer_id, "relay rejected by recipient");
                        }
                    }
                    _ => {
                        // Accepted / Completed / Failed -- informational for now.
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn on_stream(
        &self,
        _header: StreamHeader,
        sender_send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        sender: &Arc<ClientHandle>,
        ctx: &ServerCtx,
    ) -> Result<()> {
        // The dispatch layer already consumed the plugin name from the stream.
        // The remaining bytes on `recv` start with our RelayRequest (length-prefixed).

        // Read a 4-byte big-endian length prefix for the RelayRequest protobuf.
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read RelayRequest length: {e}"))?;
        let req_len = u32::from_be_bytes(len_buf) as usize;

        if req_len > 64 * 1024 {
            anyhow::bail!("RelayRequest too large ({req_len} bytes)");
        }

        let mut req_buf = vec![0u8; req_len];
        recv.read_exact(&mut req_buf)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read RelayRequest body: {e}"))?;

        let request = proto::RelayRequest::decode(&req_buf[..])
            .map_err(|e| anyhow::anyhow!("failed to decode RelayRequest: {e}"))?;

        let sender_id = sender.user_id;
        let recipient_id = request.recipient_user_id;
        let transfer_id = request.transfer_id.clone();

        info!(
            sender = sender_id,
            recipient = recipient_id,
            file = %request.file_name,
            size = request.file_size,
            transfer_id = %transfer_id,
            "file relay request"
        );

        // Validate recipient is connected.
        if ctx.get_client(recipient_id).is_none() {
            Self::send_status(
                ctx,
                sender_id,
                &transfer_id,
                relay_status::Status::Failed,
                "recipient not connected",
            )
            .await;
            return Ok(());
        }

        // Build the offer for the recipient.
        let offer = proto::RelayOffer {
            sender_user_id: sender_id,
            file_name: request.file_name.clone(),
            file_size: request.file_size,
            transfer_id: transfer_id.clone(),
        };
        let offer_bytes = offer.encode_to_vec();

        // Open a stream to the recipient.
        let offer_header = StreamHeader {
            plugin: "file-relay".to_owned(),
            metadata: Vec::new(), // metadata is sent inline after the header
        };

        let (mut recipient_send, recipient_recv) = match ctx.open_stream_to(recipient_id, offer_header).await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(transfer_id = %transfer_id, "cannot open stream to recipient: {e}");
                Self::send_status(
                    ctx,
                    sender_id,
                    &transfer_id,
                    relay_status::Status::Failed,
                    &format!("cannot reach recipient: {e}"),
                )
                .await;
                return Ok(());
            }
        };

        // Write the RelayOffer (length-prefixed) on the recipient stream.
        let len_bytes = (offer_bytes.len() as u32).to_be_bytes();
        if let Err(e) = recipient_send.write_all(&len_bytes).await {
            warn!(transfer_id = %transfer_id, "failed to write offer length: {e}");
            return Ok(());
        }
        if let Err(e) = recipient_send.write_all(&offer_bytes).await {
            warn!(transfer_id = %transfer_id, "failed to write offer: {e}");
            return Ok(());
        }

        // Register cancellation.
        let cancel = CancellationToken::new();
        self.inner.active.insert(
            transfer_id.clone(),
            RelayState {
                sender_id,
                recipient_id,
                cancel: cancel.clone(),
            },
        );

        // Spawn a bidirectional copy task.
        let inner = self.inner.clone();
        let tid = transfer_id.clone();

        tokio::spawn(async move {
            let result = tokio::select! {
                _ = cancel.cancelled() => {
                    info!(transfer_id = %tid, "relay cancelled");
                    Err(anyhow::anyhow!("cancelled"))
                }
                r = relay_copy(
                    recv,
                    recipient_send,
                    recipient_recv,
                    sender_send,
                    &inner.bytes_relayed,
                ) => r,
            };

            match &result {
                Ok(bytes) => {
                    info!(transfer_id = %tid, bytes = bytes, "relay completed");
                }
                Err(e) => {
                    // Connection resets are normal when streams close.
                    warn!(transfer_id = %tid, "relay error: {e}");
                }
            }

            inner.active.remove(&tid);
        });

        Ok(())
    }

    async fn on_disconnect(&self, client: &Arc<ClientHandle>, _ctx: &ServerCtx) {
        let user_id = client.user_id;
        // Cancel all relays involving this user.
        let to_remove: Vec<String> = self
            .inner
            .active
            .iter()
            .filter(|entry| {
                let state = entry.value();
                state.sender_id == user_id || state.recipient_id == user_id
            })
            .map(|entry| entry.key().clone())
            .collect();

        for tid in to_remove {
            if let Some((_, relay)) = self.inner.active.remove(&tid) {
                relay.cancel.cancel();
                info!(transfer_id = %tid, user_id, "relay cancelled on disconnect");
            }
        }
    }
}

/// Copy data from sender -> recipient. Returns total bytes relayed.
///
/// The sender's send stream and recipient's recv stream are kept alive but
/// unused for now (reserved for future bidirectional ack/flow control).
async fn relay_copy(
    mut sender_recv: quinn::RecvStream,
    mut recipient_send: quinn::SendStream,
    _recipient_recv: quinn::RecvStream,
    _sender_send: quinn::SendStream,
    bytes_counter: &AtomicU64,
) -> Result<u64> {
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 32 * 1024]; // 32 KiB chunks

    loop {
        match sender_recv.read(&mut buf).await {
            Ok(Some(n)) => {
                recipient_send
                    .write_all(&buf[..n])
                    .await
                    .map_err(|e| anyhow::anyhow!("write to recipient failed: {e}"))?;
                total += n as u64;
                bytes_counter.fetch_add(n as u64, Ordering::Relaxed);
            }
            Ok(None) => {
                // Sender closed their send half -- transfer complete.
                recipient_send
                    .finish()
                    .map_err(|e| anyhow::anyhow!("failed to finish recipient stream: {e}"))?;
                break;
            }
            Err(e) => {
                return Err(anyhow::anyhow!("read from sender failed: {e}"));
            }
        }
    }

    Ok(total)
}
