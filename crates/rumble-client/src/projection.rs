//! The projection task: sole writer of `Arc<RwLock<State>>`.
//!
//! Subscribes to every domain event channel
//! ([`ChatEvent`][crate::domain_events::ChatEvent],
//! [`VoiceEvent`][crate::domain_events::VoiceEvent], etc.) and
//! folds them into the shared [`State`] snapshot that the UI reads
//! each frame. Conceptually a [CQRS] read-model maintainer: the
//! events are the source of truth; `State` is a cached projection.
//!
//! ## Sole-writer invariant
//!
//! Nothing else in the active code path acquires a write lock on
//! `state`. The connection and audio tasks emit typed events; this
//! task applies them. `BackendHandle::state_mut()` exists as a
//! public escape hatch for the deprecated egui clients to clear
//! one-shot fields, but the active damascene path does not call it.
//!
//! ## Why a separate task instead of folding into the connection task
//!
//! The connection task already does too many jobs (network I/O,
//! command dispatch). Splitting the state writer out lets the
//! connection task be purely an "inputs → typed events" translator
//! and makes the single-writer invariant for `State` *structural*
//! rather than aspirational.
//!
//! ## Resync-on-lag / hash-mismatch
//!
//! `tokio::broadcast` returns `RecvError::Lagged(n)` when a consumer
//! falls behind the ring; at that point `state` is divergent from the
//! event log. The projection also verifies each `StateUpdate` against
//! the server's `expected_hash` (carried as a
//! [`RoomEvent::StateHashCheckpoint`]). On either a lag or a hash
//! mismatch it sends [`Command::RequestStateSync`] back through the
//! connection task, which asks the server for a fresh `ServerState`.
//! The server's reply arrives as a `FullStateReplaced` event that
//! atomically rebuilds the snapshot from a known-good baseline.
//! Capacity is 1024, which should be ample; a lag in practice is a
//! real bug, but the resync keeps us correct rather than silently
//! divergent.
//!
//! [CQRS]: https://martinfowler.com/bliki/CQRS.html

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use rumble_client_traits::FileTransferPlugin;
use tokio::sync::{
    broadcast::{self, error::RecvError},
    mpsc,
};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::{
    audio_task::{AudioCommand, AudioTaskHandle},
    domain_events::{ChatEvent, ConnectionEvent, RoomEvent, TransferEvent, VoiceEvent},
    events::{Command, State},
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

    /// Subscribe to every domain channel at once, returning the projection's
    /// receiver set.
    ///
    /// Call this **before** spawning any emitter (the audio task, the
    /// connection task) so the receivers buffer events from the very start.
    /// A `tokio::broadcast` send with zero live receivers is silently
    /// dropped — so if the projection only subscribed *inside* its task
    /// (which runs on a separately-spawned runtime thread), an event emitted
    /// in the gap before that thread is first scheduled would be lost. For a
    /// connected client the lag/hash resync would paper over it, but a
    /// disconnected client (e.g. an early `SetInputDevice`) has no resync
    /// path and loses the update permanently. Subscribing up front closes
    /// that race.
    pub fn subscribe_all(&self) -> BusReceivers {
        BusReceivers {
            chat: self.chat.subscribe(),
            voice: self.voice.subscribe(),
            connection: self.connection.subscribe(),
            room: self.room.subscribe(),
            transfer: self.transfer.subscribe(),
        }
    }
}

/// The projection task's receiver set, subscribed up front by
/// [`EventBus::subscribe_all`] and moved into [`spawn_projection_task`].
pub struct BusReceivers {
    chat: broadcast::Receiver<ChatEvent>,
    voice: broadcast::Receiver<VoiceEvent>,
    connection: broadcast::Receiver<ConnectionEvent>,
    room: broadcast::Receiver<RoomEvent>,
    transfer: broadcast::Receiver<TransferEvent>,
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
pub fn spawn_projection_task(
    state: Arc<RwLock<State>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    // Receivers subscribed up front via [`EventBus::subscribe_all`], before
    // any emitter was spawned, so no startup event is missed. Passed in
    // (rather than subscribed here) because this fn runs on a runtime thread
    // that may be scheduled well after the emitters have begun.
    receivers: BusReceivers,
    audio_task: AudioTaskHandle,
    file_transfer: Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>>,
    // Command channel back into the connection task, used to request a
    // full ServerState resync when the projection detects divergence
    // (a `StateUpdate` hash mismatch or a broadcast-channel lag). This
    // doesn't violate the sole-writer invariant: the projection still
    // never writes `State` itself for the resync — it asks the server,
    // whose reply arrives as a `FullStateReplaced` event applied here.
    command_tx: mpsc::UnboundedSender<Command>,
) -> tokio::task::JoinHandle<()> {
    let BusReceivers {
        chat: mut chat_rx,
        voice: mut voice_rx,
        connection: mut conn_rx,
        room: mut room_rx,
        transfer: mut transfer_rx,
    } = receivers;

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
                    Err(RecvError::Lagged(skipped)) => on_lag("chat", skipped, &command_tx),
                    Err(RecvError::Closed) => {
                        debug!("projection: chat channel closed");
                        if all_closed(&chat_rx, &voice_rx, &conn_rx, &room_rx, &transfer_rx) {
                            break;
                        }
                    }
                },
                voice = voice_rx.recv() => match voice {
                    Ok(ev) => apply_voice(&state, ev, &repaint),
                    Err(RecvError::Lagged(skipped)) => on_lag("voice", skipped, &command_tx),
                    Err(RecvError::Closed) => {
                        debug!("projection: voice channel closed");
                        if all_closed(&chat_rx, &voice_rx, &conn_rx, &room_rx, &transfer_rx) {
                            break;
                        }
                    }
                },
                conn = conn_rx.recv() => match conn {
                    Ok(ev) => apply_connection(&state, ev, &repaint),
                    Err(RecvError::Lagged(skipped)) => on_lag("connection", skipped, &command_tx),
                    Err(RecvError::Closed) => {
                        debug!("projection: connection channel closed");
                        if all_closed(&chat_rx, &voice_rx, &conn_rx, &room_rx, &transfer_rx) {
                            break;
                        }
                    }
                },
                room = room_rx.recv() => match room {
                    Ok(ev) => apply_room(&state, ev, &repaint, &audio_task, &file_transfer, &command_tx),
                    Err(RecvError::Lagged(skipped)) => on_lag("room", skipped, &command_tx),
                    Err(RecvError::Closed) => {
                        debug!("projection: room channel closed");
                        if all_closed(&chat_rx, &voice_rx, &conn_rx, &room_rx, &transfer_rx) {
                            break;
                        }
                    }
                },
                transfer = transfer_rx.recv() => match transfer {
                    Ok(ev) => apply_transfer(&state, ev, &repaint),
                    Err(RecvError::Lagged(skipped)) => on_lag("transfer", skipped, &command_tx),
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

fn on_lag(domain: &'static str, skipped: u64, command_tx: &mpsc::UnboundedSender<Command>) {
    // A lag means we dropped `skipped` events and `state` is now
    // divergent from the event log. Request a fresh ServerState so the
    // connection task's reply rebuilds the snapshot from a known-good
    // baseline (FullStateReplaced). The empty hashes mark this as a
    // lag-triggered resync rather than a hash mismatch.
    error!("projection: {domain} channel lagged, skipped {skipped} events; requesting resync");
    request_resync(command_tx, Vec::new(), Vec::new());
}

/// Ask the connection task to request a full ServerState from the
/// server. Fire-and-forget: if the channel is closed the connection is
/// already tearing down and a resync is moot.
fn request_resync(command_tx: &mpsc::UnboundedSender<Command>, expected_hash: Vec<u8>, actual_hash: Vec<u8>) {
    if command_tx
        .send(Command::RequestStateSync {
            expected_hash,
            actual_hash,
        })
        .is_err()
    {
        debug!("projection: resync requested but command channel closed");
    }
}

/// Verify the just-applied state against the server's expected hash.
///
/// Reconstructs the canonical `proto::ServerState` that the server hashes
/// in `broadcast_state_update` and re-hashes it with the shared helper
/// [`rumble_protocol::compute_server_state_hash`]. To match the server's
/// `StateUpdate.expected_hash` byte-for-byte we must mirror exactly what
/// the server feeds the hasher there, which is **not** the same as the
/// wire `ServerState` we received:
///
/// - `effective_permissions` is per-client and set on the wire copy
///   *after* hashing; the server hashes rooms with it left at `0`. Our
///   `State.rooms` carry the non-zero per-client value, so we must zero
///   it per room before hashing or we'd mismatch on every update.
/// - `groups` is excluded (`vec![]`) from the incremental-update hash.
/// - `slash_commands` is excluded (the helper clears it regardless).
///
/// ACL entries and all other room/user fields are included and must match.
/// A mismatch means our incremental apply diverged from the server's
/// view — request a full resync so the next `FullStateReplaced` rebuilds
/// us from a known-good baseline.
fn verify_state_hash(state: &Arc<RwLock<State>>, expected_hash: &[u8], command_tx: &mpsc::UnboundedSender<Command>) {
    let server_state = {
        let s = match state.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        state_hash_input(&s)
    };

    let actual_hash = rumble_protocol::compute_server_state_hash(&server_state);
    if actual_hash != expected_hash {
        warn!(
            expected = %hex_prefix(expected_hash),
            actual = %hex_prefix(&actual_hash),
            "projection: state hash mismatch after StateUpdate; requesting resync"
        );
        request_resync(command_tx, expected_hash.to_vec(), actual_hash);
    }
}

/// Build the canonical `ServerState` the server hashes for a
/// `StateUpdate.expected_hash`, from the client's local snapshot.
///
/// Must mirror `server::handlers::broadcast_state_update` exactly:
/// rooms with `effective_permissions` zeroed (it's per-client and set on
/// the wire copy after hashing), and empty `groups` / `slash_commands`
/// (both excluded from the incremental-update hash). See
/// [`verify_state_hash`] for the full rationale.
fn state_hash_input(s: &State) -> rumble_protocol::proto::ServerState {
    let rooms = s
        .rooms
        .iter()
        .map(|r| rumble_protocol::proto::RoomInfo {
            effective_permissions: 0,
            ..r.clone()
        })
        .collect();
    rumble_protocol::proto::ServerState {
        rooms,
        users: s.users.clone(),
        groups: Vec::new(),
        slash_commands: Vec::new(),
    }
}

/// Short hex prefix of a hash for log readability (full 32-byte hashes
/// are noise in logs; the first few bytes are enough to correlate).
fn hex_prefix(bytes: &[u8]) -> String {
    bytes.iter().take(4).map(|b| format!("{b:02x}")).collect()
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

/// Trim the chat log to the recent-message cap. The cap is the same
/// 100-entry sliding window the connection task used to enforce
/// inline at every push site. Centralised here so we only have to
/// reason about it in one place.
const CHAT_MESSAGE_CAP: usize = 100;

fn apply_chat(state: &Arc<RwLock<State>>, ev: ChatEvent, repaint: &Arc<dyn Fn() + Send + Sync>) {
    let mut s = match state.write() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match ev {
        ChatEvent::MessageAdded { msg } => {
            // Dedup by id covers history-sync re-arrival of a message we
            // already have (including the sender's SenderMirror coming
            // back via someone else's history share).
            if s.chat_messages.iter().any(|m| m.id == msg.id) {
                return;
            }
            s.chat_messages.push(msg);
        }
        ChatEvent::SystemNotice { text } => {
            s.chat_messages.push(crate::events::ChatMessage {
                id: uuid::Uuid::new_v4().into_bytes(),
                sender: String::new(),
                sender_id: None,
                text,
                timestamp: std::time::SystemTime::now(),
                kind: Default::default(),
                attachment: None,
                visibility: crate::events::ChatMessageVisibility::System,
            });
        }
        ChatEvent::SenderDraftInserted { msg } | ChatEvent::SenderMirrorInserted { msg } => {
            // Draft and Mirror twins share an id intentionally
            // (sender's own outgoing share has a local card AND a
            // history-sync twin). The render/history filters tell
            // them apart by visibility; no dedup here.
            s.chat_messages.push(msg);
        }
        ChatEvent::HistoryMerged { msgs } => {
            // Caller (connection task) pre-filtered against existing
            // ids; we still re-check defensively because between the
            // filter snapshot and our apply, another event may have
            // added an entry that collides.
            let existing: std::collections::HashSet<[u8; 16]> = s.chat_messages.iter().map(|m| m.id).collect();
            let mut added = false;
            for msg in msgs {
                if !existing.contains(&msg.id) {
                    s.chat_messages.push(msg);
                    added = true;
                }
            }
            if added {
                s.chat_messages.sort_by_key(|m| m.timestamp);
            }
        }
    }
    // Cap retention. Same 100-entry sliding window the connection
    // task used to enforce at every push site, now centralised.
    let len = s.chat_messages.len();
    if len > CHAT_MESSAGE_CAP {
        s.chat_messages.drain(0..len - CHAT_MESSAGE_CAP);
    }
    drop(s);
    repaint();
}

fn apply_voice(state: &Arc<RwLock<State>>, ev: VoiceEvent, repaint: &Arc<dyn Fn() + Send + Sync>) {
    // Every voice event here is a discrete fact worth a repaint. The
    // high-frequency sampled signals (meter levels, stats roll-up) ride
    // `snapshot` channels instead and never reach this path, so there's
    // no longer a "suppress repaint for noisy events" carve-out.
    let mut s = match state.write() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match ev {
        VoiceEvent::UserStartedTalking { user_id } => {
            s.audio.talking_users.insert(user_id);
        }
        VoiceEvent::UserStoppedTalking { user_id } => {
            s.audio.talking_users.remove(&user_id);
        }
        VoiceEvent::SelfMutedChanged { muted } => {
            s.audio.self_muted = muted;
        }
        VoiceEvent::SelfDeafenedChanged { deafened } => {
            s.audio.self_deafened = deafened;
        }
        VoiceEvent::LocalMuteToggled { user_id, muted } => {
            if muted {
                s.audio.muted_users.insert(user_id);
            } else {
                s.audio.muted_users.remove(&user_id);
            }
        }
        VoiceEvent::ServerMutedChanged { .. } => {
            // The `server_muted` bit lives on the matching `User` entry
            // in `state.users`, which is written by the RoomEvent stream
            // (UserStatusChanged). This event exists purely as a
            // notification for SFX / toasts.
        }
        VoiceEvent::TransmittingChanged { active } => {
            s.audio.is_transmitting = active;
        }
        VoiceEvent::DevicesEnumerated { input, output } => {
            s.audio.input_devices = input;
            s.audio.output_devices = output;
        }
        VoiceEvent::SelectedDeviceChanged { kind, id } => match kind {
            // A successful selection (emitted only after the stream actually
            // opened) clears any prior fault for that side.
            crate::domain_events::DeviceKind::Input => {
                s.audio.selected_input = id;
                s.audio.input_fault = None;
            }
            crate::domain_events::DeviceKind::Output => {
                s.audio.selected_output = id;
                s.audio.output_fault = None;
            }
        },
        VoiceEvent::DeviceUnavailable { kind, message } => {
            let fault = crate::events::DeviceFault {
                message,
                recovering: false,
            };
            match kind {
                crate::domain_events::DeviceKind::Input => s.audio.input_fault = Some(fault),
                crate::domain_events::DeviceKind::Output => s.audio.output_fault = Some(fault),
            }
        }
        VoiceEvent::DeviceError {
            kind,
            message,
            recovering,
        } => {
            let fault = crate::events::DeviceFault { message, recovering };
            match kind {
                crate::domain_events::DeviceKind::Input => s.audio.input_fault = Some(fault),
                crate::domain_events::DeviceKind::Output => s.audio.output_fault = Some(fault),
            }
        }
        VoiceEvent::VoiceModeChanged { mode } => {
            s.audio.voice_mode = mode;
        }
        VoiceEvent::AudioSettingsChanged { settings } => {
            s.audio.settings = settings;
        }
        VoiceEvent::TxPipelineChanged { config } => {
            s.audio.tx_pipeline = config;
        }
        VoiceEvent::RxPipelineDefaultsChanged { config } => {
            s.audio.rx_pipeline_defaults = config;
        }
        VoiceEvent::UserRxConfigChanged { user_id, config } => {
            s.audio.per_user_rx.insert(user_id, config);
        }
        VoiceEvent::UserRxOverrideCleared { user_id } => {
            s.audio.per_user_rx.remove(&user_id);
        }
    }
    drop(s);
    repaint();
}

fn apply_connection(state: &Arc<RwLock<State>>, ev: ConnectionEvent, repaint: &Arc<dyn Fn() + Send + Sync>) {
    let mut s = match state.write() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let mut recompute_perms = false;
    match ev {
        ConnectionEvent::ConnectStarted { server_addr } => {
            s.connection = crate::events::ConnectionState::Connecting { server_addr };
            // A new connection attempt starts a fresh chat log. Messages
            // from the previous server must not interleave with the new
            // one's — in particular file-offer cards, whose share ids are
            // only meaningful on the server that issued them. A transient
            // drop (ConnectionLost / Disconnected) keeps the log visible;
            // only an explicit connect wipes it.
            s.chat_messages.clear();
        }
        ConnectionEvent::CertificatePending { cert_info } => {
            s.connection = crate::events::ConnectionState::CertificatePending { cert_info };
        }
        ConnectionEvent::Connected {
            server_name,
            user_id,
            session_public_key,
            session_id,
        } => {
            s.connection = crate::events::ConnectionState::Connected { server_name, user_id };
            // A fresh authenticated connection clears any prior kick
            // reason — it was for the previous session.
            s.kicked = None;
            s.my_user_id = Some(user_id);
            s.my_session_public_key = Some(session_public_key);
            s.my_session_id = Some(session_id);
            // Now that my_user_id is set, any FullStateReplaced that
            // landed before Connected (broadcast cross-channel order is
            // not guaranteed) can finally compute permissions correctly.
            recompute_perms = true;
        }
        ConnectionEvent::Disconnected => {
            s.connection = crate::events::ConnectionState::Disconnected;
            s.my_user_id = None;
            s.my_room_id = None;
            s.my_session_public_key = None;
            s.my_session_id = None;
        }
        ConnectionEvent::ConnectionLost { error } => {
            s.connection = crate::events::ConnectionState::ConnectionLost { error };
            // Same clear-on-loss policy as Disconnected — the session
            // identity is gone, the next connect attempt will populate
            // fresh values.
            s.my_user_id = None;
            s.my_room_id = None;
            s.my_session_public_key = None;
            s.my_session_id = None;
        }
        ConnectionEvent::Kicked { reason } => {
            s.kicked = Some(reason);
        }
        ConnectionEvent::PermissionDenied { message } => {
            // No `State` mutation: the user-facing surface is the
            // `BackendEvent::Toast` the receiver task emits alongside this
            // event. The variant stays on the bus for typed subscribers.
            let _ = message;
        }
        ConnectionEvent::ElevatedChanged { .. } => {
            // The is_elevated bit lives on the matching `User` entry,
            // which is updated by the RoomEvent stream (UserStatusChanged
            // / FullStateReplaced). Nothing to do here.
        }
        ConnectionEvent::WelcomeMessageReceived { text } => {
            // Welcome text is rendered as a chat system notice; the
            // ChatEvent::SystemNotice path is the actual writer. This
            // variant exists so future consumers (e.g. a server-info
            // panel) can react to the welcome without parsing chat.
            let _ = text;
        }
    }
    drop(s);
    if recompute_perms {
        recompute_effective_permissions(state);
    }
    repaint();
}

fn apply_room(
    state: &Arc<RwLock<State>>,
    ev: RoomEvent,
    repaint: &Arc<dyn Fn() + Send + Sync>,
    audio_task: &AudioTaskHandle,
    file_transfer: &Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>>,
    command_tx: &mpsc::UnboundedSender<Command>,
) {
    // Hash-checkpoint events carry no state mutation: they verify that
    // the delta(s) we just applied left us at the server's expected
    // hash, and request a resync on divergence. Handled up front so the
    // rest of this function can assume a real mutation.
    if let RoomEvent::StateHashCheckpoint { expected_hash } = &ev {
        verify_state_hash(state, expected_hash, command_tx);
        return;
    }
    // Collect side effects (AudioCommand dispatches, plugin
    // room-id updates) under the write lock, then drop the lock
    // before issuing them — the audio task uses an unbounded mpsc so
    // send doesn't block, but plugin.set_room_id may acquire its own
    // lock and we don't want a lock-ordering surprise.
    let mut audio_cmds: Vec<AudioCommand> = Vec::new();
    let mut new_room_id_for_ft: Option<Option<Uuid>> = None;
    let mut recompute_perms = false;

    let mut s = match state.write() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match ev {
        RoomEvent::FullStateReplaced {
            rooms,
            users,
            groups,
            slash_commands,
            per_room_permissions,
        } => {
            s.per_room_permissions = per_room_permissions;
            s.rooms = rooms;
            s.users = users;
            s.group_definitions = groups;
            s.slash_commands = slash_commands;
            s.rebuild_room_tree();

            if let Some(my_room) = s.my_room_id
                && let Some(&perms) = s.per_room_permissions.get(&my_room)
            {
                s.effective_permissions = perms;
            }

            // Notify audio task of the user set in our current room so
            // it can pre-create decoders, and propagate the matching
            // server_muted flag.
            if let Some(my_room_id) = s.my_room_id {
                audio_cmds.push(AudioCommand::RoomChanged {
                    user_ids_in_room: room_user_ids(&s, my_room_id, s.my_user_id),
                });
            }
            if let Some(my_user_id) = s.my_user_id
                && let Some(muted) = s
                    .users
                    .iter()
                    .find(|u| u.user_id.as_ref().map(|x| x.value) == Some(my_user_id))
                    .map(|u| u.server_muted)
            {
                audio_cmds.push(AudioCommand::SetServerMuted { muted });
            }
            recompute_perms = true;
        }
        RoomEvent::RoomAdded { room } => {
            s.rooms.push(room);
            s.rebuild_room_tree();
            recompute_perms = true;
        }
        RoomEvent::RoomRemoved { room_id } => {
            s.rooms
                .retain(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) != Some(room_id));
            s.rebuild_room_tree();
            recompute_perms = true;
        }
        RoomEvent::RoomRenamed { room_id, name } => {
            if let Some(room) = s
                .rooms
                .iter_mut()
                .find(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) == Some(room_id))
            {
                room.name = name;
            }
            s.rebuild_room_tree();
        }
        RoomEvent::RoomMoved { room_id, new_parent } => {
            if let Some(room) = s
                .rooms
                .iter_mut()
                .find(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) == Some(room_id))
            {
                room.parent_id = new_parent.map(rumble_protocol::room_id_from_uuid);
            }
            s.rebuild_room_tree();
        }
        RoomEvent::RoomDescriptionSet { room_id, description } => {
            if let Some(room) = s
                .rooms
                .iter_mut()
                .find(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) == Some(room_id))
            {
                room.description = description;
            }
            s.rebuild_room_tree();
        }
        RoomEvent::RoomAclChanged {
            room_id,
            inherit_acl,
            acls,
            effective: _,
            per_room_recompute: _,
        } => {
            if let Some(room) = s
                .rooms
                .iter_mut()
                .find(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id) == Some(room_id))
            {
                room.inherit_acl = inherit_acl;
                room.acls = acls;
            }
            recompute_perms = true;
        }
        RoomEvent::UserJoined { user } => {
            let user_id_value = user.user_id.as_ref().map(|id| id.value);
            let already_exists = s
                .users
                .iter()
                .any(|u| u.user_id.as_ref().map(|id| id.value) == user_id_value);
            if !already_exists {
                let my_room_id = s.my_room_id;
                let user_room = user.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id);
                let notify_audio = user_id_value.is_some() && my_room_id.is_some() && user_room == my_room_id;
                s.users.push(user);
                if notify_audio && let Some(uid) = user_id_value {
                    audio_cmds.push(AudioCommand::UserJoinedRoom { user_id: uid });
                }
            }
        }
        RoomEvent::UserLeft { user_id } => {
            let my_room_id = s.my_room_id;
            let was_in_our_room = s
                .users
                .iter()
                .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
                .map(|u| u.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id) == my_room_id)
                .unwrap_or(false);
            s.users
                .retain(|u| u.user_id.as_ref().map(|id| id.value) != Some(user_id));
            if was_in_our_room && my_room_id.is_some() {
                audio_cmds.push(AudioCommand::UserLeftRoom { user_id });
            }
        }
        RoomEvent::UserMoved { user_id, room_id } => {
            let my_room_id = s.my_room_id;
            let my_user_id = s.my_user_id;

            let from_room_id = s
                .users
                .iter()
                .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
                .and_then(|u| u.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id));

            if let Some(user) = s
                .users
                .iter_mut()
                .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
            {
                user.current_room = room_id.map(rumble_protocol::room_id_from_uuid);
            }

            if my_user_id == Some(user_id) {
                // Our own move. Decided here, against the State this task
                // owns — the receiver used to read `my_user_id` from a
                // snapshot to fire a separate SelfMovedToRoom, a cross-task
                // read that could race the projection's own applies
                // (issue #39 class). Same side effects as SelfMovedToRoom:
                // my_room_id, audio-task room change, file-transfer nudge,
                // permission recompute.
                if let Some(rid) = room_id {
                    s.my_room_id = Some(rid);
                    new_room_id_for_ft = Some(Some(rid));
                    audio_cmds.push(AudioCommand::RoomChanged {
                        user_ids_in_room: room_user_ids(&s, rid, my_user_id),
                    });
                    recompute_perms = true;
                }
            } else {
                let joined_our_room = my_room_id.is_some() && room_id == my_room_id;
                let left_our_room = my_room_id.is_some() && from_room_id == my_room_id;
                if joined_our_room {
                    audio_cmds.push(AudioCommand::UserJoinedRoom { user_id });
                } else if left_our_room {
                    audio_cmds.push(AudioCommand::UserLeftRoom { user_id });
                }
            }
        }
        RoomEvent::UserStatusChanged {
            user_id,
            is_muted,
            is_deafened,
            server_muted,
            is_elevated,
        } => {
            let my_user_id = s.my_user_id;
            if let Some(user) = s
                .users
                .iter_mut()
                .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
            {
                let prev_server_muted = user.server_muted;
                let prev_elevated = user.is_elevated;
                user.is_muted = is_muted;
                user.is_deafened = is_deafened;
                user.server_muted = server_muted;
                user.is_elevated = is_elevated;
                if my_user_id == Some(user_id) {
                    if prev_server_muted != server_muted {
                        audio_cmds.push(AudioCommand::SetServerMuted { muted: server_muted });
                    }
                    // Elevation bypasses ACL evaluation entirely, so a
                    // flip changes our effective permissions.
                    if prev_elevated != is_elevated {
                        recompute_perms = true;
                    }
                }
            } else {
                // Room-channel ordering guarantees any preceding
                // UserJoined was applied before this delta, so an unknown
                // user here is real divergence (e.g. we missed the join).
                // Drop the delta; the StateHashCheckpoint that follows it
                // will catch the divergence and trigger a resync on
                // hash-capable servers.
                warn!(user_id, "projection: UserStatusChanged for unknown user; dropping");
            }
        }
        RoomEvent::UserGroupChanged { user_id, group, added } => {
            let my_user_id = s.my_user_id;
            if let Some(user) = s
                .users
                .iter_mut()
                .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(user_id))
            {
                let changed = if added {
                    if user.groups.contains(&group) {
                        false
                    } else {
                        user.groups.push(group);
                        true
                    }
                } else {
                    let before = user.groups.len();
                    user.groups.retain(|g| g != &group);
                    user.groups.len() != before
                };
                if changed && my_user_id == Some(user_id) {
                    recompute_perms = true;
                }
            } else {
                // Same divergence rationale as UserStatusChanged above.
                warn!(
                    user_id,
                    group, "projection: UserGroupChanged for unknown user; dropping"
                );
            }
        }
        RoomEvent::GroupAdded { group } | RoomEvent::GroupModified { group } => {
            if let Some(existing) = s.group_definitions.iter_mut().find(|g| g.name == group.name) {
                *existing = group;
            } else {
                s.group_definitions.push(group);
            }
            recompute_perms = true;
        }
        RoomEvent::GroupRemoved { name } => {
            s.group_definitions.retain(|g| g.name != name);
            recompute_perms = true;
        }
        RoomEvent::SelfMovedToRoom { room_id } => {
            s.my_room_id = Some(room_id);
            new_room_id_for_ft = Some(Some(room_id));
            audio_cmds.push(AudioCommand::RoomChanged {
                user_ids_in_room: room_user_ids(&s, room_id, s.my_user_id),
            });
            recompute_perms = true;
        }
        RoomEvent::StateHashCheckpoint { .. } => {
            // Handled by the early return above; the match is only
            // reached for state-mutating events.
            unreachable!("StateHashCheckpoint is handled before the mutation match");
        }
    }
    drop(s);

    for cmd in audio_cmds {
        audio_task.send(cmd);
    }
    if let Some(new_room) = new_room_id_for_ft
        && let Ok(guard) = file_transfer.read()
        && let Some(ft) = guard.as_ref()
    {
        ft.set_room_id(new_room.map(|r| r.to_string()).unwrap_or_default());
    }
    if recompute_perms {
        recompute_effective_permissions(state);
    }
    repaint();
}

/// User ids co-located in `room_id`, excluding `exclude` (our own id).
/// Feeds `AudioCommand::RoomChanged` so the audio task can pre-create
/// decoders for everyone we can now hear.
fn room_user_ids(s: &State, room_id: Uuid, exclude: Option<u64>) -> Vec<u64> {
    s.users
        .iter()
        .filter_map(|u| {
            let uid = u.user_id.as_ref().map(|id| id.value)?;
            let user_room = u.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
            if user_room == room_id && Some(uid) != exclude {
                Some(uid)
            } else {
                None
            }
        })
        .collect()
}

// =============================================================================
// Helpers moved from handle.rs — projection.rs owns these now that the
// projection task is the sole writer of permission-related state.
// =============================================================================

fn recompute_effective_permissions(state: &Arc<RwLock<State>>) {
    let s = match state.read() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let my_user_id = match s.my_user_id {
        Some(id) => id,
        None => return,
    };

    let mut user_groups = vec!["default".to_string()];
    if let Some(me) = s
        .users
        .iter()
        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(my_user_id))
    {
        for g in &me.groups {
            if !user_groups.contains(g) {
                user_groups.push(g.clone());
            }
        }
        if !user_groups.contains(&me.username) {
            user_groups.push(me.username.clone());
        }
    }

    let mut group_perms = HashMap::new();
    for gd in &s.group_definitions {
        group_perms.insert(
            gd.name.clone(),
            rumble_protocol::permissions::Permissions::from_bits_truncate(gd.permissions),
        );
    }

    let is_elevated = s
        .users
        .iter()
        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(my_user_id))
        .map(|u| u.is_elevated)
        .unwrap_or(false);

    let my_room_id = s.my_room_id;

    let room_uuids: Vec<Uuid> = s
        .rooms
        .iter()
        .filter_map(|r| r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id))
        .collect();

    let rooms_snapshot = s.rooms.clone();
    drop(s);

    let mut per_room = HashMap::new();
    for room_uuid in &room_uuids {
        let room_chain = build_room_chain(&rooms_snapshot, *room_uuid);
        let ref_chain: Vec<(Uuid, Option<&rumble_protocol::permissions::RoomAclData>)> =
            room_chain.iter().map(|(uuid, acl)| (*uuid, acl.as_ref())).collect();
        let effective =
            rumble_protocol::permissions::effective_permissions(&user_groups, &group_perms, &ref_chain, is_elevated);
        per_room.insert(*room_uuid, effective.bits());
    }

    let mut s = match state.write() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    s.per_room_permissions = per_room;
    if let Some(my_room) = my_room_id
        && let Some(&perms) = s.per_room_permissions.get(&my_room)
    {
        s.effective_permissions = perms;
    }
}

fn build_room_chain(
    rooms: &[rumble_protocol::proto::RoomInfo],
    target: Uuid,
) -> Vec<(Uuid, Option<rumble_protocol::permissions::RoomAclData>)> {
    use rumble_protocol::permissions::{AclEntry, Permissions, RoomAclData};

    let mut path = Vec::new();
    // Guards the parent walk against cycles in the rooms map. The server
    // is supposed to reject cyclic moves, but a malicious or buggy server
    // can still deliver one, and the projection task — the sole `State`
    // writer — must never spin on it. A revisited room terminates the
    // walk as if the root had been reached.
    let mut visited = std::collections::HashSet::new();
    let mut current = target;

    loop {
        if !visited.insert(current) {
            tracing::warn!(
                target: "rumble_client::projection",
                room = %current,
                "room parent cycle detected; truncating ancestor chain"
            );
            break;
        }
        path.push(current);
        if current == rumble_protocol::ROOT_ROOM_UUID {
            break;
        }
        let parent = rooms.iter().find_map(|r| {
            let rid = r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
            if rid == current {
                r.parent_id.as_ref().and_then(rumble_protocol::uuid_from_room_id)
            } else {
                None
            }
        });
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }

    path.reverse();
    if path.first() != Some(&rumble_protocol::ROOT_ROOM_UUID) {
        path.insert(0, rumble_protocol::ROOT_ROOM_UUID);
    }
    path.dedup();

    path.into_iter()
        .map(|room_uuid| {
            let acl_data = rooms.iter().find_map(|r| {
                let rid = r.id.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
                if rid == room_uuid && !r.acls.is_empty() {
                    Some(RoomAclData {
                        inherit_acl: r.inherit_acl,
                        entries: r
                            .acls
                            .iter()
                            .map(|e| AclEntry {
                                group: e.group.clone(),
                                grant: Permissions::from_bits_truncate(e.grant),
                                deny: Permissions::from_bits_truncate(e.deny),
                                apply_here: e.apply_here,
                                apply_subs: e.apply_subs,
                            })
                            .collect(),
                    })
                } else {
                    None
                }
            });
            (room_uuid, acl_data)
        })
        .collect()
}

fn apply_transfer(_state: &Arc<RwLock<State>>, ev: TransferEvent, _repaint: &Arc<dyn Fn() + Send + Sync>) {
    // Transfers are not stored in `State`; they're queried via
    // `plugin.transfers()`. This variant exists for external
    // subscribers (chat card, media cache, auto-download) that want
    // the typed stream alongside the existing mpsc UX path.
    tracing::trace!(target: "rumble_client::projection", "transfer event: {:?}", ev);
}

#[cfg(test)]
mod tests {
    use rumble_protocol::{
        compute_server_state_hash,
        proto::{RoomId, RoomInfo, ServerState, User, UserId},
    };

    use super::*;

    fn room(uuid: u128, name: &str, effective: u32) -> RoomInfo {
        RoomInfo {
            id: Some(RoomId {
                uuid: uuid.to_be_bytes().to_vec(),
            }),
            name: name.to_string(),
            parent_id: None,
            description: None,
            inherit_acl: true,
            acls: vec![],
            effective_permissions: effective,
        }
    }

    fn room_with_parent(uuid: u128, parent: u128, name: &str) -> RoomInfo {
        let mut r = room(uuid, name, 0);
        r.parent_id = Some(RoomId {
            uuid: parent.to_be_bytes().to_vec(),
        });
        r
    }

    fn user(id: u64, name: &str) -> User {
        User {
            user_id: Some(UserId { value: id }),
            username: name.to_string(),
            current_room: None,
            is_muted: false,
            is_deafened: false,
            server_muted: false,
            is_elevated: false,
            groups: vec![],
            label: None,
        }
    }

    /// The whole point of `state_hash_input`: a client snapshot whose
    /// rooms carry the per-client `effective_permissions` (non-zero, as
    /// received on the wire) must still hash to the same value the server
    /// fed its hasher in `broadcast_state_update` — which uses rooms with
    /// `effective_permissions: 0`, empty groups, and empty slash commands.
    /// If this drifts, every StateUpdate would spuriously trigger a resync.
    #[test]
    fn state_hash_input_matches_server_broadcast_update_input() {
        // Client rooms carry non-zero per-client effective permissions and
        // group definitions, neither of which the server hashes for a
        // StateUpdate.
        let s = State {
            rooms: vec![room(1, "Root", 0x1ff), room(2, "Lobby", 0x42)],
            users: vec![user(1, "alice"), user(2, "bob")],
            group_definitions: vec![rumble_protocol::proto::GroupInfo {
                name: "admin".to_string(),
                permissions: 0xffff_ffff,
                is_builtin: true,
            }],
            ..Default::default()
        };

        // Server's broadcast_state_update hash input: same rooms but with
        // effective_permissions zeroed, groups + slash_commands empty.
        let server_input = ServerState {
            rooms: vec![room(1, "Root", 0), room(2, "Lobby", 0)],
            users: s.users.clone(),
            groups: vec![],
            slash_commands: vec![],
        };

        assert_eq!(
            compute_server_state_hash(&state_hash_input(&s)),
            compute_server_state_hash(&server_input),
            "client-side hash input must match the server's StateUpdate hash input"
        );
    }

    /// A divergent local state (an extra user the server doesn't have)
    /// must produce a different hash, so `verify_state_hash` actually
    /// catches drift rather than silently matching.
    #[test]
    fn state_hash_input_detects_divergence() {
        let s = State {
            rooms: vec![room(1, "Root", 0)],
            users: vec![user(1, "alice")],
            ..Default::default()
        };

        let server_input = ServerState {
            rooms: vec![room(1, "Root", 0)],
            users: vec![user(1, "alice"), user(2, "ghost")],
            groups: vec![],
            slash_commands: vec![],
        };

        assert_ne!(
            compute_server_state_hash(&state_hash_input(&s)),
            compute_server_state_hash(&server_input),
            "a missing user must change the hash"
        );
    }

    /// A 2-node parent cycle (A → B → A) that never reaches the root
    /// must terminate with a finite chain. The projection task is the
    /// sole `State` writer, so an unbounded walk here would freeze the
    /// whole client.
    #[test]
    fn build_room_chain_terminates_on_parent_cycle() {
        let rooms = vec![room_with_parent(1, 2, "A"), room_with_parent(2, 1, "B")];

        let chain = build_room_chain(&rooms, Uuid::from_u128(1));

        let ids: Vec<Uuid> = chain.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![rumble_protocol::ROOT_ROOM_UUID, Uuid::from_u128(2), Uuid::from_u128(1)],
            "cycle walk must cut at the revisit and anchor at the root"
        );
    }

    /// A room that is its own parent must terminate immediately rather
    /// than walking forever.
    #[test]
    fn build_room_chain_terminates_on_self_parent() {
        let rooms = vec![room_with_parent(1, 1, "A")];

        let chain = build_room_chain(&rooms, Uuid::from_u128(1));

        let ids: Vec<Uuid> = chain.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![rumble_protocol::ROOT_ROOM_UUID, Uuid::from_u128(1)]);
    }

    // =========================================================================
    // apply_room delta application (issue #39)
    // =========================================================================

    /// Drives `apply_room` directly — synchronous, no runtime — with
    /// observable side-effect channels.
    struct Harness {
        state: Arc<RwLock<State>>,
        repaint: Arc<dyn Fn() + Send + Sync>,
        audio_task: AudioTaskHandle,
        audio_rx: mpsc::UnboundedReceiver<AudioCommand>,
        file_transfer: Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>>,
        command_tx: mpsc::UnboundedSender<Command>,
        // Held so resync requests don't hit a closed channel.
        _command_rx: mpsc::UnboundedReceiver<Command>,
    }

    impl Harness {
        fn new(initial: State) -> Self {
            let (audio_tx, audio_rx) = mpsc::unbounded_channel();
            let (command_tx, command_rx) = mpsc::unbounded_channel();
            Self {
                state: Arc::new(RwLock::new(initial)),
                repaint: Arc::new(|| {}),
                audio_task: AudioTaskHandle::from_raw(audio_tx),
                audio_rx,
                file_transfer: Arc::new(RwLock::new(None)),
                command_tx,
                _command_rx: command_rx,
            }
        }

        fn apply(&mut self, ev: RoomEvent) {
            apply_room(
                &self.state,
                ev,
                &self.repaint,
                &self.audio_task,
                &self.file_transfer,
                &self.command_tx,
            );
        }

        fn user(&self, id: u64) -> Option<rumble_protocol::proto::User> {
            self.state
                .read()
                .unwrap()
                .users
                .iter()
                .find(|u| u.user_id.as_ref().map(|x| x.value) == Some(id))
                .cloned()
        }

        fn drain_audio(&mut self) -> Vec<AudioCommand> {
            let mut out = Vec::new();
            while let Ok(cmd) = self.audio_rx.try_recv() {
                out.push(cmd);
            }
            out
        }
    }

    /// The issue #39 race, encoded deterministically: a join followed by
    /// two back-to-back deltas for the same user. With the old
    /// receiver-side read-modify-write, the second delta's payload could
    /// be built from a snapshot that predates the first delta's apply and
    /// silently revert it. Deltas applied in order by the sole writer
    /// must compose instead.
    #[test]
    fn back_to_back_user_deltas_compose() {
        let mut h = Harness::new(State::default());

        h.apply(RoomEvent::UserJoined { user: user(2, "alice") });
        h.apply(RoomEvent::UserStatusChanged {
            user_id: 2,
            is_muted: true,
            is_deafened: false,
            server_muted: false,
            is_elevated: false,
        });
        h.apply(RoomEvent::UserGroupChanged {
            user_id: 2,
            group: "admin".to_string(),
            added: true,
        });

        let alice = h.user(2).expect("alice must exist");
        assert!(alice.is_muted, "first delta (mute) must survive the second");
        assert_eq!(
            alice.groups,
            vec!["admin".to_string()],
            "second delta (group add) must land on top of the first"
        );
    }

    /// A status delta for a user we don't know about (real divergence —
    /// in-channel ordering means any preceding UserJoined was already
    /// applied) is dropped without panicking and without conjuring a
    /// user; a subsequent join + delta works normally.
    #[test]
    fn status_change_for_unknown_user_is_dropped() {
        let mut h = Harness::new(State::default());

        h.apply(RoomEvent::UserStatusChanged {
            user_id: 7,
            is_muted: true,
            is_deafened: true,
            server_muted: true,
            is_elevated: false,
        });
        assert!(h.user(7).is_none(), "a dropped delta must not create a user");

        h.apply(RoomEvent::UserJoined { user: user(7, "bob") });
        h.apply(RoomEvent::UserGroupChanged {
            user_id: 7,
            group: "mods".to_string(),
            added: true,
        });
        let bob = h.user(7).expect("bob joined");
        assert!(!bob.is_muted, "the pre-join delta must not be retroactively applied");
        assert_eq!(bob.groups, vec!["mods".to_string()]);
    }

    /// A server-mute flip on *our* user dispatches SetServerMuted to the
    /// audio task exactly once per transition (no command on a repeat of
    /// the same value).
    #[test]
    fn self_server_mute_flip_dispatches_audio_command() {
        let mut h = Harness::new(State {
            my_user_id: Some(1),
            users: vec![user(1, "me")],
            ..Default::default()
        });

        let status = |server_muted: bool| RoomEvent::UserStatusChanged {
            user_id: 1,
            is_muted: false,
            is_deafened: false,
            server_muted,
            is_elevated: false,
        };

        h.apply(status(true));
        let cmds = h.drain_audio();
        assert!(
            matches!(cmds.as_slice(), [AudioCommand::SetServerMuted { muted: true }]),
            "expected exactly one SetServerMuted(true), got {cmds:?}"
        );

        h.apply(status(true));
        let cmds = h.drain_audio();
        assert!(cmds.is_empty(), "no transition, no command; got {cmds:?}");
    }

    /// Our own UserMoved is detected by the projection against its own
    /// `my_user_id` (the receiver no longer pre-reads State to emit a
    /// separate SelfMovedToRoom): my_room_id updates and the audio task
    /// gets the co-located peer set.
    #[test]
    fn self_user_moved_updates_my_room_and_audio() {
        let target = Uuid::from_u128(5);
        let mut peer = user(2, "peer");
        peer.current_room = Some(rumble_protocol::room_id_from_uuid(target));
        let mut h = Harness::new(State {
            my_user_id: Some(1),
            users: vec![user(1, "me"), peer],
            ..Default::default()
        });

        h.apply(RoomEvent::UserMoved {
            user_id: 1,
            room_id: Some(target),
        });

        assert_eq!(h.state.read().unwrap().my_room_id, Some(target));
        let cmds = h.drain_audio();
        assert!(
            matches!(cmds.as_slice(), [AudioCommand::RoomChanged { user_ids_in_room }] if user_ids_in_room == &vec![2]),
            "expected RoomChanged with the co-located peer, got {cmds:?}"
        );
    }
}
