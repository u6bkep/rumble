//! The projection task: sole writer of `Arc<RwLock<State>>`.
//!
//! Subscribes to every domain event channel
//! ([`ChatEvent`][crate::domain_events::ChatEvent],
//! [`VoiceEvent`][crate::domain_events::VoiceEvent], etc.) and
//! folds them into the shared [`State`] snapshot that the UI reads
//! each frame. Conceptually a [CQRS] read-model maintainer: the
//! events are the source of truth; `State` is a cached projection.
//!
//! ## Phase 1 (current): wiring stub
//!
//! Today this task is a logging passthrough — it receives events
//! and traces them, but does not yet write to `State`. The
//! connection and audio tasks still mutate `State` directly. The
//! infrastructure is in place so a follow-up commit can flip the
//! writer in one motion: remove `write_state` from the tasks,
//! switch the projection from `trace!` to actual field updates,
//! same observable behaviour.
//!
//! ## Phase 2 (planned): sole writer
//!
//! - Tasks stop writing `State`.
//! - Each `apply_*` helper here mutates `State` from its event.
//! - Effective-permission recompute (currently
//!   `recalculate_effective_permissions` in handle.rs) moves into
//!   the projection's `RoomEvent` handlers, since the projection
//!   owns the inputs.
//! - On broadcast lag (consumer fell too far behind the 1024-deep
//!   ring), trigger a server resync via `ServerState` and reset.
//!
//! ## Why a separate task instead of folding into the connection task
//!
//! The connection task currently does too many jobs (network I/O,
//! command dispatch, state mutation). Splitting the state writer
//! out lets the connection task be purely an
//! "inputs → typed events" translator. It also makes the
//! single-writer invariant for `State` *structural* rather than
//! aspirational: nothing in the codebase (other than this file)
//! has a `&mut` path into `State`.
//!
//! [CQRS]: https://martinfowler.com/bliki/CQRS.html

use std::sync::{Arc, RwLock};

use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};
use tracing::{debug, error, warn};

use crate::{
    domain_events::{ChatEvent, ConnectionEvent, RoomEvent, TransferEvent, VoiceEvent},
    events::State,
};

/// Bundle of broadcast `Sender`s passed to the connection and audio
/// tasks at startup. They emit into these; the projection task
/// subscribes to the matching receivers.
///
/// `Clone` is cheap (a few `broadcast::Sender` Arc-clones).
#[derive(Clone)]
pub struct EventBus {
    pub chat: broadcast::Sender<ChatEvent>,
    pub voice: broadcast::Sender<VoiceEvent>,
    pub connection: broadcast::Sender<ConnectionEvent>,
    pub room: broadcast::Sender<RoomEvent>,
    pub transfer: broadcast::Sender<TransferEvent>,
}

impl EventBus {
    /// Create a fresh bus with the workspace-standard channel capacity.
    pub fn new() -> Self {
        use crate::domain_events::EVENT_BROADCAST_CAPACITY as CAP;
        Self {
            chat: broadcast::channel(CAP).0,
            voice: broadcast::channel(CAP).0,
            connection: broadcast::channel(CAP).0,
            room: broadcast::channel(CAP).0,
            transfer: broadcast::channel(CAP).0,
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the projection task on the current tokio runtime.
///
/// Returns immediately; the task lives as long as at least one
/// `Sender` on the bus is alive (i.e. as long as the connection or
/// audio task is running). When all senders drop, every `recv()`
/// returns `Closed` and the task exits cleanly.
///
/// Phase 1: applies no state mutations — the connection/audio tasks
/// are still the writers. The events are received and traced; this
/// proves the wiring without changing observable behaviour. See
/// module docs.
pub fn spawn_projection_task(
    state: Arc<RwLock<State>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    bus: &EventBus,
) -> tokio::task::JoinHandle<()> {
    let mut chat_rx = bus.chat.subscribe();
    let mut voice_rx = bus.voice.subscribe();
    let mut conn_rx = bus.connection.subscribe();
    let mut room_rx = bus.room.subscribe();
    let mut transfer_rx = bus.transfer.subscribe();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Each branch handles one domain. `Closed` means every
                // sender for that channel has dropped — we tolerate the
                // others continuing (e.g. audio task can shut down
                // independently of connection task during connect/disconnect
                // cycles). `Lagged` is a hard fault — see module docs.
                chat = chat_rx.recv() => match chat {
                    Ok(ev) => apply_chat(&state, ev, &repaint),
                    Err(RecvError::Lagged(skipped)) => on_lag("chat", skipped),
                    Err(RecvError::Closed) => {
                        debug!("projection: chat channel closed");
                        if all_closed(&chat_rx, &voice_rx, &conn_rx, &room_rx, &transfer_rx) {
                            break;
                        }
                    }
                },
                voice = voice_rx.recv() => match voice {
                    Ok(ev) => apply_voice(&state, ev, &repaint),
                    Err(RecvError::Lagged(skipped)) => on_lag("voice", skipped),
                    Err(RecvError::Closed) => {
                        debug!("projection: voice channel closed");
                        if all_closed(&chat_rx, &voice_rx, &conn_rx, &room_rx, &transfer_rx) {
                            break;
                        }
                    }
                },
                conn = conn_rx.recv() => match conn {
                    Ok(ev) => apply_connection(&state, ev, &repaint),
                    Err(RecvError::Lagged(skipped)) => on_lag("connection", skipped),
                    Err(RecvError::Closed) => {
                        debug!("projection: connection channel closed");
                        if all_closed(&chat_rx, &voice_rx, &conn_rx, &room_rx, &transfer_rx) {
                            break;
                        }
                    }
                },
                room = room_rx.recv() => match room {
                    Ok(ev) => apply_room(&state, ev, &repaint),
                    Err(RecvError::Lagged(skipped)) => on_lag("room", skipped),
                    Err(RecvError::Closed) => {
                        debug!("projection: room channel closed");
                        if all_closed(&chat_rx, &voice_rx, &conn_rx, &room_rx, &transfer_rx) {
                            break;
                        }
                    }
                },
                transfer = transfer_rx.recv() => match transfer {
                    Ok(ev) => apply_transfer(&state, ev, &repaint),
                    Err(RecvError::Lagged(skipped)) => on_lag("transfer", skipped),
                    Err(RecvError::Closed) => {
                        debug!("projection: transfer channel closed");
                        if all_closed(&chat_rx, &voice_rx, &conn_rx, &room_rx, &transfer_rx) {
                            break;
                        }
                    }
                },
            }
        }
        debug!("projection task exiting");
    })
}

fn on_lag(domain: &'static str, skipped: u64) {
    // Phase 1: log and continue. Phase 2 will trigger a server
    // resync (`ServerState` re-request) so the projection re-syncs
    // from a known-good baseline.
    error!("projection: {domain} channel lagged, skipped {skipped} events");
}

/// True iff every domain receiver has seen its channel closed (all
/// senders dropped). Returning from `recv` with `Closed` only tells
/// us one channel is gone; we keep the task alive until all are.
fn all_closed(
    chat: &broadcast::Receiver<ChatEvent>,
    voice: &broadcast::Receiver<VoiceEvent>,
    conn: &broadcast::Receiver<ConnectionEvent>,
    room: &broadcast::Receiver<RoomEvent>,
    transfer: &broadcast::Receiver<TransferEvent>,
) -> bool {
    // `try_recv` on a closed channel returns `Closed`; on an empty
    // open channel it returns `Empty`. We treat anything not-Closed
    // as "still open" without consuming the message.
    is_closed(chat) && is_closed(voice) && is_closed(conn) && is_closed(room) && is_closed(transfer)
}

fn is_closed<T: Clone>(rx: &broadcast::Receiver<T>) -> bool {
    // Hack: we can't `try_recv()` through a shared reference. Use the
    // sender count: when it hits zero, the channel is closed.
    rx.sender_strong_count() == 0
}

// =============================================================================
// Phase 1: apply_* are tracing stubs. Phase 2 will fold each event
// into the appropriate `State` field mutation here.
// =============================================================================

fn apply_chat(_state: &Arc<RwLock<State>>, ev: ChatEvent, _repaint: &Arc<dyn Fn() + Send + Sync>) {
    debug!(target: "rumble_client::projection", "chat event (stub, not yet applied): {:?}", ev);
}

fn apply_voice(_state: &Arc<RwLock<State>>, ev: VoiceEvent, _repaint: &Arc<dyn Fn() + Send + Sync>) {
    // VoiceEvent::InputLevel and ::StatsUpdated fire frequently
    // (every audio frame / every stats tick). Suppress to trace.
    match &ev {
        VoiceEvent::InputLevel { .. } | VoiceEvent::StatsUpdated { .. } => {
            tracing::trace!(target: "rumble_client::projection", "voice event (stub): {:?}", ev);
        }
        _ => {
            debug!(target: "rumble_client::projection", "voice event (stub, not yet applied): {:?}", ev);
        }
    }
}

fn apply_connection(_state: &Arc<RwLock<State>>, ev: ConnectionEvent, _repaint: &Arc<dyn Fn() + Send + Sync>) {
    debug!(target: "rumble_client::projection", "connection event (stub, not yet applied): {:?}", ev);
}

fn apply_room(_state: &Arc<RwLock<State>>, ev: RoomEvent, _repaint: &Arc<dyn Fn() + Send + Sync>) {
    // FullStateReplaced carries the entire user/room/group set; its
    // Debug is enormous. Summarise.
    match &ev {
        RoomEvent::FullStateReplaced {
            rooms, users, groups, ..
        } => {
            debug!(
                target: "rumble_client::projection",
                "room FullStateReplaced (stub): rooms={} users={} groups={}",
                rooms.len(),
                users.len(),
                groups.len()
            );
        }
        _ => {
            debug!(target: "rumble_client::projection", "room event (stub, not yet applied): {:?}", ev);
        }
    }
}

fn apply_transfer(_state: &Arc<RwLock<State>>, ev: TransferEvent, _repaint: &Arc<dyn Fn() + Send + Sync>) {
    debug!(target: "rumble_client::projection", "transfer event (stub, not yet applied): {:?}", ev);
}

// Silence unused warnings on the broadcast `TryRecvError` import — it'll
// be used by phase 2's resync logic.
#[allow(dead_code)]
fn _phase2_resync_marker(_: TryRecvError) {
    warn!("phase 2: implement server resync on lag");
}
