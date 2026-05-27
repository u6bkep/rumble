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

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use rumble_client_traits::FileTransferPlugin;
use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::{
    audio_task::{AudioCommand, AudioTaskHandle},
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
    audio_task: AudioTaskHandle,
    file_transfer: Arc<RwLock<Option<Arc<dyn FileTransferPlugin>>>>,
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
                    Ok(ev) => apply_room(&state, ev, &repaint, &audio_task, &file_transfer),
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
    // High-frequency events (InputLevel fires per audio frame ~50Hz,
    // StatsUpdated every 500ms): write state but skip repaint. The UI
    // reads `audio.input_level_db` and `audio.stats` on its next
    // already-scheduled redraw — no need to wake it just for a meter
    // tick.
    let suppress_repaint = matches!(ev, VoiceEvent::InputLevel { .. } | VoiceEvent::StatsUpdated { .. });

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
            // (UserUpdated). This event exists purely as a notification
            // for SFX / toasts.
        }
        VoiceEvent::TransmittingChanged { active } => {
            s.audio.is_transmitting = active;
        }
        VoiceEvent::InputLevel { db } => {
            s.audio.input_level_db = Some(db);
        }
        VoiceEvent::StatsUpdated { stats } => {
            // Preserve buffer_underruns — nothing increments it today,
            // but if/when it gets wired up, the audio task's roll-up
            // doesn't know about it.
            let preserved_underruns = s.audio.stats.buffer_underruns;
            s.audio.stats = stats;
            s.audio.stats.buffer_underruns = preserved_underruns;
        }
        VoiceEvent::DevicesEnumerated { input, output } => {
            s.audio.input_devices = input;
            s.audio.output_devices = output;
        }
        VoiceEvent::SelectedDeviceChanged { kind, id } => match kind {
            crate::domain_events::DeviceKind::Input => s.audio.selected_input = id,
            crate::domain_events::DeviceKind::Output => s.audio.selected_output = id,
        },
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
    if !suppress_repaint {
        repaint();
    }
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
            s.permission_denied = Some(message);
        }
        ConnectionEvent::ElevatedChanged { .. } => {
            // The is_elevated bit lives on the matching `User` entry,
            // which is updated by the RoomEvent stream (UserUpdated /
            // FullStateReplaced). Nothing to do here.
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
) {
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
            per_room_permissions,
        } => {
            s.per_room_permissions = per_room_permissions;
            s.rooms = rooms;
            s.users = users;
            s.group_definitions = groups;
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
                let my_user_id = s.my_user_id;
                let user_ids_in_room: Vec<u64> = s
                    .users
                    .iter()
                    .filter_map(|u| {
                        let user_id = u.user_id.as_ref().map(|id| id.value)?;
                        let user_room = u.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
                        if user_room == my_room_id && Some(user_id) != my_user_id {
                            Some(user_id)
                        } else {
                            None
                        }
                    })
                    .collect();
                audio_cmds.push(AudioCommand::RoomChanged { user_ids_in_room });
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
                room.parent_id = new_parent.map(|p| rumble_protocol::room_id_from_uuid(p));
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
                user.current_room = room_id.map(|r| rumble_protocol::room_id_from_uuid(r));
            }

            if my_user_id == Some(user_id) {
                // Our own move — handled by the matching SelfMovedToRoom
                // event the connection task fires alongside. Nothing else
                // to do here.
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
        RoomEvent::UserUpdated { user } => {
            let my_user_id = s.my_user_id;
            let new_user_id = user.user_id.as_ref().map(|id| id.value);
            if let Some(existing) = s
                .users
                .iter_mut()
                .find(|u| u.user_id.as_ref().map(|id| id.value) == new_user_id)
            {
                let prev_server_muted = existing.server_muted;
                let prev_groups = existing.groups.clone();
                let new_server_muted = user.server_muted;
                let new_groups = user.groups.clone();
                *existing = user;
                if my_user_id.is_some() && my_user_id == new_user_id && prev_server_muted != new_server_muted {
                    audio_cmds.push(AudioCommand::SetServerMuted {
                        muted: new_server_muted,
                    });
                }
                if my_user_id == new_user_id && prev_groups != new_groups {
                    recompute_perms = true;
                }
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

            let my_user_id = s.my_user_id;
            let user_ids_in_room: Vec<u64> = s
                .users
                .iter()
                .filter_map(|u| {
                    let uid = u.user_id.as_ref().map(|id| id.value)?;
                    let user_room = u.current_room.as_ref().and_then(rumble_protocol::uuid_from_room_id)?;
                    if user_room == room_id && Some(uid) != my_user_id {
                        Some(uid)
                    } else {
                        None
                    }
                })
                .collect();
            audio_cmds.push(AudioCommand::RoomChanged { user_ids_in_room });
            recompute_perms = true;
        }
    }
    drop(s);

    for cmd in audio_cmds {
        audio_task.send(cmd);
    }
    if let Some(new_room) = new_room_id_for_ft {
        if let Ok(guard) = file_transfer.read()
            && let Some(ft) = guard.as_ref()
        {
            ft.set_room_id(new_room.map(|r| r.to_string()).unwrap_or_default());
        }
    }
    if recompute_perms {
        recompute_effective_permissions(state);
    }
    repaint();
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
    let mut current = target;

    loop {
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

// Silence unused warnings on the broadcast `TryRecvError` import — it'll
// be used by phase 2's resync logic.
#[allow(dead_code)]
fn _phase2_resync_marker(_: TryRecvError) {
    warn!("phase 2: implement server resync on lag");
}
