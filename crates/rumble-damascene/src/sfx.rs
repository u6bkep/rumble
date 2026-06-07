//! Sound-effect edge detector. Diffs each frame's `State` against the
//! previous frame to fire event cues once per change (new remote chat
//! messages, connect/disconnect, server-mute, room join/leave, our own
//! channel switches). The first observation of each tracked value seeds
//! the baseline without firing, so connecting to a populated server
//! doesn't dump a flurry of beeps.
//!
//! Owns only the previous-frame snapshot it needs to tell a change from
//! a steady state. The message watermark (`prev_chat_count`) stays on
//! the App because it is shared with chat autoscroll and auto-download;
//! the App passes the post-watermark message slice into [`SfxEngine::detect`].

use std::collections::HashSet;

use rumble_client::{ChatMessage, ChatMessageKind, ChatMessageVisibility, SfxKind, State};
use uuid::Uuid;

/// Edge-detects per-frame `State` transitions and returns the cues to
/// play this frame, in the same order the old inline `pump_sfx` fired
/// them.
#[derive(Default)]
pub struct SfxEngine {
    prev_connected: bool,
    prev_server_muted: bool,
    prev_room_id: Option<Uuid>,
    prev_room_members: HashSet<u64>,
    /// Room id of a locally-initiated `JoinRoom` awaiting confirmation,
    /// set by the room-tree dispatch in `on_event`, consumed here to tell
    /// our own channel switch (`SelfChannelJoin`) apart from being
    /// relocated by an admin (`SelfChannelMoved`).
    pub pending_self_join: Option<Uuid>,
}

impl SfxEngine {
    /// `new_messages` is the chat-message slice that arrived since the
    /// previous frame — the caller passes an EMPTY slice on the seeding
    /// frame (when `prev_chat_count == 0`) so the historical backlog
    /// replayed on first connect doesn't beep.
    pub fn detect(&mut self, snapshot: &State, new_messages: &[ChatMessage]) -> Vec<SfxKind> {
        let mut sounds = Vec::new();

        // New remote chat messages. Direct messages get a distinct cue.
        if !new_messages.is_empty() {
            let mut had_dm = false;
            let mut had_room = false;
            for m in new_messages.iter().filter(|m| {
                !matches!(
                    m.visibility,
                    ChatMessageVisibility::System | ChatMessageVisibility::SenderMirror
                )
            }) {
                match m.kind {
                    ChatMessageKind::DirectMessage { .. } => had_dm = true,
                    _ => had_room = true,
                }
            }
            if had_dm {
                sounds.push(SfxKind::PrivateMessage);
            }
            if had_room {
                sounds.push(SfxKind::Message);
            }
        }

        // Connect / disconnect transitions. A kick produces a disconnect
        // with `kicked` set — play the harsher Kicked cue instead.
        let connected = snapshot.connection.is_connected();
        if connected != self.prev_connected {
            sounds.push(if connected {
                SfxKind::Connect
            } else if snapshot.kicked.is_some() {
                SfxKind::Kicked
            } else {
                SfxKind::Disconnect
            });
            self.prev_connected = connected;
        }

        // Server-side mute by an admin (distinct from our own self-mute).
        let server_muted = snapshot
            .my_user_id
            .and_then(|id| snapshot.get_user(id))
            .is_some_and(|u| u.server_muted);
        if server_muted && !self.prev_server_muted {
            sounds.push(SfxKind::ServerMute);
        }
        self.prev_server_muted = server_muted;

        // Our own room changes (SelfChannelJoin / SelfChannelMoved) and
        // remote users entering / leaving our room (UserJoin / UserLeave).
        let my_id = snapshot.my_user_id;
        let members: HashSet<u64> = match snapshot.my_room_id {
            Some(room) => snapshot
                .users_in_room(room)
                .iter()
                .filter_map(|u| u.user_id.as_ref().map(|id| id.value))
                .filter(|id| Some(*id) != my_id)
                .collect(),
            None => HashSet::new(),
        };
        if snapshot.my_room_id == self.prev_room_id {
            if members.difference(&self.prev_room_members).next().is_some() {
                sounds.push(SfxKind::UserJoin);
            }
            if self.prev_room_members.difference(&members).next().is_some() {
                sounds.push(SfxKind::UserLeave);
            }
        } else {
            // Our room id changed. Skip connect/disconnect transitions
            // (prev or current room is None) — Connect/Disconnect cover
            // those. A switch to the room we just asked to join is our own
            // action; anything else means we were relocated.
            if self.prev_room_id.is_some() && snapshot.my_room_id.is_some() {
                if snapshot.my_room_id == self.pending_self_join {
                    sounds.push(SfxKind::SelfChannelJoin);
                } else {
                    sounds.push(SfxKind::SelfChannelMoved);
                }
            }
            self.pending_self_join = None;
        }
        self.prev_room_id = snapshot.my_room_id;
        self.prev_room_members = members;

        sounds
    }
}
