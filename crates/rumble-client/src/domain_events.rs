//! Typed event channels emitted by the backend tasks.
//!
//! The connection task and audio task translate their inputs
//! (commands from the UI, envelopes from the server, audio frames
//! from capture/playback) into the events defined here. A single
//! [`projection`][crate::projection] task subscribes to all of them
//! and is the sole writer of [`State`] — the shared snapshot the UI
//! reads each frame. Other consumers (toast manager, SFX pump,
//! auto-download) can subscribe to whichever channels they care
//! about via the `subscribe_*` methods on
//! [`BackendHandle`][crate::handle::BackendHandle].
//!
//! Each domain lives on its own [`tokio::sync::broadcast`] channel.
//! Per-domain retention policies follow naturally from this split:
//! `ChatEvent`s feed an effectively unbounded log
//! (`State::chat_messages`), `VoiceEvent`s are fire-and-forget
//! transitions, etc.
//!
//! ### Why per-domain instead of a single uber-enum
//!
//! Subscribers tend to care about exactly one slice (toasts watch
//! connection lifecycle, SFX watches voice transitions, the chat
//! renderer watches chat). Per-domain channels mean a subscriber's
//! `recv` returns a typed variant it can `match` exhaustively rather
//! than a generic `BackendEvent` whose 90% of variants it would
//! `_ => continue` past. It also lets each domain evolve its
//! retention/lag policy independently.

use std::collections::HashMap;

use rumble_audio::{PipelineConfig, UserRxConfig};
use rumble_client_traits::file_transfer::{TransferDirection, TransferId, TransferStage};
use rumble_protocol::{
    Uuid,
    proto::{GroupInfo, RoomAclEntry, RoomInfo, SlashCommand, User},
};

use crate::events::{AudioDeviceInfo, AudioSettings, ChatMessage, PendingCertificate, VoiceMode};

/// Capacity used for every `broadcast::channel` in the backend.
/// 1024 is generous enough that the projection task — which is the
/// primary consumer and runs on its own tokio task — should never
/// fall behind by more than a tiny fraction of this. If it does
/// lag, the projection treats it as a hard fault and requests a
/// fresh `ServerState` from the server (a cheap belt-and-suspenders
/// reset that gets us back to a known-good projection).
pub const EVENT_BROADCAST_CAPACITY: usize = 1024;

/// Which side of the audio path a device is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Input,
    Output,
}

// =============================================================================
// ChatEvent
// =============================================================================

/// Mutations to `state.chat_messages`.
///
/// The "long log" domain (per the design discussion): chat is the
/// only event stream we expect to retain indefinitely in `State`.
/// Every variant here carries enough payload that a projection can
/// apply it without re-reading the underlying network envelope.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// A normal wire-roundtripped chat message. Used for:
    /// - peer-broadcast room chat received from the server
    /// - direct messages received from another user
    /// - the sender's own outgoing chat, inserted locally because the
    ///   server no longer echoes chat back to the sender
    ///
    /// Carries a fully-populated `ChatMessage` ready to push. Projection
    /// dedups by `id` and trims the log to the configured cap.
    MessageAdded { msg: ChatMessage },

    /// Locally-generated system notice (command results, welcome banner,
    /// "Requesting chat history…", connection-error explanations).
    /// Renders as italic system text; never goes on the wire.
    SystemNotice { text: String },

    /// Sender-side in-flight file-share card was started. Carries the
    /// `ChatMessageVisibility::SenderDraft` entry to insert.
    SenderDraftInserted { msg: ChatMessage },

    /// Sender-side mirror for an already-broadcast file share. Inserted
    /// after the upload finishes so chat-history-sync requests from
    /// late peers include the share. Same `id` as the draft.
    SenderMirrorInserted { msg: ChatMessage },

    /// Chat-history-sync merge from a peer. Pre-deduplicated against
    /// the existing log; the projection just pushes these in order and
    /// re-sorts.
    HistoryMerged { msgs: Vec<ChatMessage> },
}

// =============================================================================
// VoiceEvent
// =============================================================================

/// Audio/voice state transitions.
///
/// Emitted by the audio task (talking detection, mute toggles, stats
/// updates, device enumeration) and by the connection task for the
/// server-driven cases (`ServerMutedChanged` when an admin force-mutes us).
#[derive(Debug, Clone)]
pub enum VoiceEvent {
    /// A remote user just started transmitting voice.
    UserStartedTalking { user_id: u64 },
    /// A remote user just stopped transmitting voice.
    UserStoppedTalking { user_id: u64 },

    /// Local self-mute toggle (we asked the audio task to mute).
    SelfMutedChanged { muted: bool },
    /// Local self-deafen toggle.
    SelfDeafenedChanged { deafened: bool },
    /// Local mute-this-user toggle (we won't hear `user_id`).
    LocalMuteToggled { user_id: u64, muted: bool },

    /// The server applied/cleared a server-mute on us (admin action or
    /// SPEAK-denied room join). Projection updates the matching `User`
    /// entry in `state.users`; UI surfaces a toast separately if needed.
    ServerMutedChanged { muted: bool },

    /// Local transmission state (effectively gated by PTT/Continuous +
    /// self-mute + server-mute + transmit-active).
    TransmittingChanged { active: bool },

    // Sampled audio signals — per-frame input levels and the periodic
    // stats roll-up — are published over `snapshot` channels (see
    // `crate::snapshot`, read via `BackendHandle::meter()` / `stats()`)
    // rather than this event bus. The UI samples them on repaint; the
    // audio task writes them on its own cadence. No VoiceEvent variant
    // carries level or stats data.
    /// Device enumeration result, after `Command::RefreshAudioDevices`
    /// or a hotplug-triggered re-enum.
    DevicesEnumerated {
        input: Vec<AudioDeviceInfo>,
        output: Vec<AudioDeviceInfo>,
    },

    /// User-selected input/output device changed. `None` means
    /// "system default."
    ///
    /// Emitted only *after* the underlying stream actually opens, so the UI
    /// never shows a selection that isn't really capturing/playing. A failed
    /// selection emits [`VoiceEvent::DeviceUnavailable`] instead.
    SelectedDeviceChanged { kind: DeviceKind, id: Option<String> },

    /// Opening the selected input/output device failed (it's gone, busy, or
    /// the backend rejected it). The selection was *not* applied — capture or
    /// playback is not running. The UI should surface this and prompt the user
    /// to pick another device.
    DeviceUnavailable { kind: DeviceKind, message: String },

    /// A live capture/playback device died mid-session (unplug, sound-server
    /// crash). The audio task has torn the stream down; `recovering` is true
    /// while it retries re-opening on its poll tick and false once it gives up
    /// (e.g. nothing to re-open against). Surfaced to the UI so "connected"
    /// doesn't keep implying working audio.
    DeviceError {
        kind: DeviceKind,
        message: String,
        recovering: bool,
    },

    /// Voice activation mode toggled between PushToTalk and Continuous.
    VoiceModeChanged { mode: VoiceMode },

    /// Opus encoder / jitter-buffer settings changed (bitrate, complexity,
    /// FEC, jitter delay, etc.). The projection mirrors this into
    /// `state.audio.settings`; the audio task already applied it to its
    /// live encoder before emitting.
    AudioSettingsChanged { settings: AudioSettings },

    /// TX (microphone) pipeline configuration changed.
    TxPipelineChanged { config: PipelineConfig },
    /// Default RX pipeline configuration changed (applies to all users
    /// without per-user overrides).
    RxPipelineDefaultsChanged { config: PipelineConfig },
    /// Per-user RX configuration set or updated.
    UserRxConfigChanged { user_id: u64, config: UserRxConfig },
    /// Per-user RX configuration cleared; user reverts to defaults.
    UserRxOverrideCleared { user_id: u64 },
}

// =============================================================================
// ConnectionEvent
// =============================================================================

/// Connection lifecycle and identity-adjacent transitions.
///
/// Subscribers (toast manager, reconnect-backoff logic, SFX pump for
/// connect/disconnect cues) typically want the *transitions*, not the
/// current state — that's what `State.connection` already exposes.
#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    /// QUIC handshake / authentication attempt started against
    /// `server_addr`.
    ConnectStarted { server_addr: String },

    /// Server presented an untrusted certificate; user must
    /// accept or reject before we proceed.
    CertificatePending { cert_info: PendingCertificate },

    /// Authentication succeeded; we are now in the server's
    /// session and have our final `user_id`.
    Connected {
        server_name: String,
        user_id: u64,
        session_public_key: [u8; 32],
        session_id: [u8; 32],
    },

    /// Clean disconnect (we asked, or the server told us, but no
    /// error). Distinct from `ConnectionLost`.
    Disconnected,

    /// Connection dropped unexpectedly (network error, timeout,
    /// server crash). Reconnect logic keys off this.
    ConnectionLost { error: String },

    /// Server kicked us off. Carries the reason for the disconnect
    /// dialog. Projection sets `state.kicked = Some(reason)`.
    Kicked { reason: String },

    /// Server denied a command we issued (insufficient permission).
    /// Projection sets `state.permission_denied = Some(message)` so
    /// the UI can surface a toast on next frame.
    PermissionDenied { message: String },

    /// Superuser elevation transitioned. `is_elevated` mirrors the
    /// server's view; updated locally only after the server
    /// acknowledges via a state update.
    ElevatedChanged { is_elevated: bool },

    /// Server welcome banner / MOTD. Rendered as a `SystemNotice`
    /// in chat by the projection (we keep it as a distinct event in
    /// case future consumers want to handle it differently).
    WelcomeMessageReceived { text: String },
}

// =============================================================================
// RoomEvent
// =============================================================================

/// Server-state churn: rooms, users, groups, ACLs, our own room
/// membership. The biggest event family by variant count because
/// the server delivers a granular `StateUpdate` stream covering all
/// of these.
#[derive(Debug, Clone)]
pub enum RoomEvent {
    /// Full server-state replacement, as delivered by the
    /// `ServerState` envelope at connection startup or after a hash
    /// mismatch. Projection clears and refills the relevant `State`
    /// fields. Distinct from a sequence of incremental adds so the
    /// projection can apply it as one atomic write.
    FullStateReplaced {
        rooms: Vec<RoomInfo>,
        users: Vec<User>,
        groups: Vec<GroupInfo>,
        /// Slash-command catalog reported by the server. Empty on the
        /// disconnect/reset variants, which clear it.
        slash_commands: Vec<SlashCommand>,
        per_room_permissions: HashMap<Uuid, u32>,
    },

    RoomAdded {
        room: RoomInfo,
    },
    RoomRemoved {
        room_id: Uuid,
    },
    RoomRenamed {
        room_id: Uuid,
        name: String,
    },
    RoomMoved {
        room_id: Uuid,
        new_parent: Option<Uuid>,
    },
    RoomDescriptionSet {
        room_id: Uuid,
        description: Option<String>,
    },
    /// Room ACL was edited. The `effective` field is the server's
    /// computed effective permission bitmask for the editing user's
    /// own view of this room; `per_room_recompute` carries any
    /// downstream effective-permission updates (for inheriting
    /// descendant rooms).
    RoomAclChanged {
        room_id: Uuid,
        inherit_acl: bool,
        acls: Vec<RoomAclEntry>,
        effective: u32,
        per_room_recompute: HashMap<Uuid, u32>,
    },

    UserJoined {
        user: User,
    },
    UserLeft {
        user_id: u64,
    },
    UserMoved {
        user_id: u64,
        room_id: Option<Uuid>,
    },
    UserUpdated {
        user: User,
    },

    GroupAdded {
        group: GroupInfo,
    },
    GroupRemoved {
        name: String,
    },
    GroupModified {
        group: GroupInfo,
    },

    /// Our own user moved rooms (could be our own action or a
    /// server-driven move). Projection updates `state.my_room_id`.
    SelfMovedToRoom {
        room_id: Uuid,
    },
}

// =============================================================================
// TransferEvent
// =============================================================================

/// File transfer lifecycle, lifted from the plugin's
/// `PluginEvent` stream into a backend-layer event so consumers
/// (chat card, media cache, auto-download) all speak one type
/// without each subscribing to the plugin directly.
///
/// One variant for now: the plugin only fires lifecycle transitions
/// (`PluginEvent::TransferStageChanged`), and the initial transition
/// into `Active` doubles as a "started" signal. Subscribers needing a
/// full snapshot can call `plugin.transfers()`.
#[derive(Debug, Clone)]
pub enum TransferEvent {
    /// An existing transfer transitioned to a new lifecycle stage
    /// (`Active` on creation, `Active` → `Done`, `Active` → `Failed`,
    /// etc.). Intra-`Active` progress updates do NOT fire this — UI
    /// progress bars keep reading the plugin's snapshot.
    StageChanged {
        id: TransferId,
        direction: TransferDirection,
        name: String,
        stage: TransferStage,
    },
}
