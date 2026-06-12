//! Dump damascene bundle artifacts (svg + tree + draw_ops + lint + manifest)
//! for every canonical UI scene of `rumble-damascene`.
//!
//! Run:
//!   cargo run -p rumble-damascene --bin dump_bundles
//!   cargo run -p rumble-damascene --bin dump_bundles -- connected cert_pending
//!   cargo run -p rumble-damascene --bin dump_bundles -- --bless           # re-bless goldens
//!   cargo run -p rumble-damascene --bin dump_bundles -- --check           # diff vs goldens (CI-style)
//!
//! Output: `crates/rumble-damascene/out/rumble_<scene>.{svg,tree.txt,draw_ops.txt,lint.txt,shader_manifest.txt}`.
//!
//! Golden regression check: `--bless` writes the deterministic subset
//! (`draw_ops.txt` + `lint.txt`) to the tracked `crates/rumble-damascene/goldens/`
//! dir; `--check` re-renders and fails (exit 1) if any scene drifts. Both modes
//! accept an optional scene-name filter, same as the default dump. The goldens
//! are pinned to the current git-pinned damascene rev — re-bless after an damascene bump.
//!
//! Mirrors the `damascene-volume::render_artifacts` shape: a small
//! `MockBackend` that returns a canned `State`, a `Scene` enum that
//! enumerates the views worth snapshotting, and the same four-line
//! `render_bundle` + `write_bundle` core that damascene-core ships in the
//! prelude. No GPU is involved — the SVG fallback renders the same
//! draw-op stream the wgpu Runner would, so layout regressions show
//! up faithfully without spinning up a window or device.

use std::{path::PathBuf, time::SystemTime};

use damascene_core::prelude::*;

use rumble_client::{
    AudioDeviceInfo, AudioState, AudioStats, ChatMessage, ChatMessageKind, ChatMessageVisibility, Command,
    ConnectionState, DeviceFault, PendingCertificate, State, VoiceMode,
};
use rumble_damascene::{
    ElevateState, Identity, RumbleApp, SettingsOpenSelect, SettingsTab, UnlockState, WizardState,
    backend::UiBackend,
    settings::{GitBuildInfo, set_git_build_info_override},
};
use rumble_desktop_shell::{KeyInfo, RecentServer, SettingsStore};
use rumble_protocol::{
    ChatAttachment, Uuid,
    permissions::{ADMIN_PERMISSIONS, DEFAULT_PERMISSIONS, Permissions},
    proto::{GroupInfo, RelayFileSharePayload, RoomAclEntry, RoomInfo, User, UserId},
    room_id_from_uuid,
};

// ---------------------------------------------------------------------
// Mock backend
// ---------------------------------------------------------------------

/// Returns a canned `State` to the renderer and discards every command.
/// The fixture only exercises the read half of [`UiBackend`]; commands
/// would be no-ops here even if we did record them, since there's no
/// backend to apply them.
struct MockBackend {
    state: State,
    transfers: Vec<rumble_client_traits::file_transfer::TransferStatus>,
    meter: rumble_client::MeterSnapshot,
    stats: AudioStats,
    outputs: rumble_client::OutputFrame,
}

impl UiBackend for MockBackend {
    fn state(&self) -> State {
        self.state.clone()
    }
    fn send(&self, _command: Command) {}
    fn meter(&self) -> rumble_client::MeterSnapshot {
        self.meter
    }
    fn stats(&self) -> AudioStats {
        self.stats
    }
    fn outputs(&self) -> rumble_client::OutputFrame {
        self.outputs
    }
    fn transfers(&self) -> Vec<rumble_client_traits::file_transfer::TransferStatus> {
        self.transfers.clone()
    }
}

// ---------------------------------------------------------------------
// Scene catalog
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Scene {
    /// Disconnected with a populated saved-server list (the typical
    /// returning-user shell).
    Disconnected,
    /// Disconnected with no saved servers — first-run empty state.
    DisconnectedEmpty,
    /// Add-server form open over the disconnected backdrop.
    AddServerModal,
    /// Edit-server form open over the disconnected backdrop.
    EditServerModal,
    /// Connection in progress.
    Connecting,
    /// Live session: rooms, users, chat — exercises the full shell.
    Connected,
    /// Live session with the local user actively transmitting voice —
    /// exercises the self-talking room-tree indicator path that
    /// doesn't go through `talking_users` (which is remote-only).
    ConnectedSelfTalking,
    /// Live session exercising every per-user voice-state glyph in the
    /// room tree at once: idle (self), talking, self-muted, server-muted,
    /// self-deafened, and a peer we've locally muted (additive badge).
    ConnectedUserStates,
    /// Live session that *looks* connected but whose audio devices have
    /// faulted: a hard input (microphone) failure plus a recovering
    /// output (speaker) loss. Exercises the under-toolbar fault banner
    /// (`fault_banner`) in both its destructive and warning treatments.
    ConnectedAudioFault,
    /// Live session with the chat composer holding `/`, showing the
    /// slash-command suggestion list (builtins + server-advertised).
    ConnectedSlashCommands,
    /// Same as [`Scene::ConnectedSlashCommands`] but with a suggestion row
    /// arrow-key-highlighted, capturing the accent-fill keyboard selection.
    ConnectedSlashCommandHighlighted,
    /// Live session with a completed image transfer rendered as an
    /// inline preview in the chat sidebar.
    ConnectedImagePreview,
    /// Live session with the click-to-enlarge image lightbox open.
    ImageLightbox,
    /// Lightbox at 200% zoom with a non-zero pan, exercising the
    /// `−` / `+` / Fit buttons + zoom-percent label in their
    /// "non-default" state and showing the zoomed paint transform.
    ImageLightboxZoomed,
    /// Live session with a completed 3D-model transfer rendered as an
    /// inline poster preview (the offscreen render is seeded directly).
    ConnectedModelPreview,
    /// Live session with the 3D-model orbit lightbox open over the
    /// connected backdrop.
    ModelLightbox,
    /// Connection lost with an error message in the toolbar.
    ConnectionLost,
    /// Cert acceptance modal up over the disconnected backdrop.
    CertPending,
    /// First-run wizard: choose between local key and ssh-agent.
    WizardSelectMethod,
    /// First-run wizard: local-key password entry.
    WizardGenerateLocal,
    /// First-run wizard: ssh-agent key picker.
    WizardSelectAgentKey,
    /// First-run wizard: terminal error screen.
    WizardError,
    /// Encrypted-key unlock prompt at startup.
    UnlockPrompt,
    /// Settings dialog — Connection tab (default).
    SettingsConnection,
    /// Live session with the toolbar transmission-mode dropdown open.
    ToolbarVoiceModeOpen,
    /// Settings dialog — Devices tab with the input dropdown open.
    SettingsDevices,
    /// Settings dialog — Voice tab (encoder/jitter/PTT toggles).
    SettingsVoice,
    /// Settings dialog — Processing tab (TX pipeline editor + level meter).
    SettingsProcessing,
    /// Settings dialog — Sounds tab (sfx toggles + per-event preview).
    SettingsSounds,
    /// Settings dialog — Chat tab with the timestamp-format dropdown open.
    SettingsChat,
    /// Settings dialog — Shortcuts tab (Mumble-style Function/Data/Shortcut
    /// table with the default Space-PTT row).
    SettingsShortcuts,
    /// Settings dialog — Files tab (auto-download + bandwidth).
    SettingsFiles,
    /// Settings dialog — Stats tab (read-only audio metrics).
    SettingsStats,
    /// Settings dialog — Admin tab (Groups & ACL) with the "moderators"
    /// group expanded to show the chip list + base-perm grid.
    SettingsAdmin,
    /// Settings dialog — About tab (build / git describe info).
    SettingsAbout,
    /// Per-room Permissions editor modal over the connected backdrop,
    /// targeting a nested room so the breadcrumb has real depth and
    /// the inherited block has rules to show.
    RoomAclModal,
    /// Sudo elevation modal with a populated password field + an
    /// error message — exercises the form-message treatment.
    ElevatePrompt,
    /// Settings → Connection while the local user holds [`SUDO`] but
    /// hasn't elevated yet — the "Elevate to superuser…" button is
    /// visible.
    SettingsConnectionSudo,
    /// Settings → Connection after a successful elevation — the
    /// button is replaced with the "Elevated this session" notice.
    SettingsConnectionElevated,
    /// Settings → Connection with a configured plaintext local
    /// identity. Exercises the per-source detail block + the
    /// "Switch to SSH agent key…" affordance.
    SettingsConnectionLocalKey,
    /// Settings → Connection bound to an ssh-agent key. Exercises the
    /// agent-reachability badge and the "Re-select SSH agent key…"
    /// affordance.
    SettingsConnectionSshAgent,
    /// Connected session with a file share mid-upload on the sender's
    /// side. The card reads progress from the plugin's TransferStatus.
    FileSharePending,
    /// Connected session with a file share whose upload failed (e.g.
    /// "file too large"). The card reads the error reason from the
    /// plugin's TransferStatus.
    FileShareFailed,
}

impl Scene {
    const ALL: &'static [Scene] = &[
        Scene::Disconnected,
        Scene::DisconnectedEmpty,
        Scene::AddServerModal,
        Scene::EditServerModal,
        Scene::Connecting,
        Scene::Connected,
        Scene::ConnectedSelfTalking,
        Scene::ConnectedUserStates,
        Scene::ConnectedAudioFault,
        Scene::ConnectedSlashCommands,
        Scene::ConnectedSlashCommandHighlighted,
        Scene::ConnectedImagePreview,
        Scene::ImageLightbox,
        Scene::ImageLightboxZoomed,
        Scene::ConnectedModelPreview,
        Scene::ModelLightbox,
        Scene::ConnectionLost,
        Scene::CertPending,
        Scene::WizardSelectMethod,
        Scene::WizardGenerateLocal,
        Scene::WizardSelectAgentKey,
        Scene::WizardError,
        Scene::UnlockPrompt,
        Scene::SettingsConnection,
        Scene::ToolbarVoiceModeOpen,
        Scene::SettingsDevices,
        Scene::SettingsVoice,
        Scene::SettingsProcessing,
        Scene::SettingsSounds,
        Scene::SettingsChat,
        Scene::SettingsShortcuts,
        Scene::SettingsFiles,
        Scene::SettingsStats,
        Scene::SettingsAdmin,
        Scene::SettingsAbout,
        Scene::RoomAclModal,
        Scene::ElevatePrompt,
        Scene::SettingsConnectionSudo,
        Scene::SettingsConnectionElevated,
        Scene::SettingsConnectionLocalKey,
        Scene::SettingsConnectionSshAgent,
        Scene::FileSharePending,
        Scene::FileShareFailed,
    ];

    fn slug(self) -> &'static str {
        match self {
            Scene::Disconnected => "disconnected",
            Scene::DisconnectedEmpty => "disconnected_empty",
            Scene::AddServerModal => "add_server_modal",
            Scene::EditServerModal => "edit_server_modal",
            Scene::Connecting => "connecting",
            Scene::Connected => "connected",
            Scene::ConnectedSelfTalking => "connected_self_talking",
            Scene::ConnectedUserStates => "connected_user_states",
            Scene::ConnectedAudioFault => "connected_audio_fault",
            Scene::ConnectedSlashCommands => "connected_slash_commands",
            Scene::ConnectedSlashCommandHighlighted => "connected_slash_command_highlighted",
            Scene::ConnectedImagePreview => "connected_image_preview",
            Scene::ImageLightbox => "image_lightbox",
            Scene::ImageLightboxZoomed => "image_lightbox_zoomed",
            Scene::ConnectedModelPreview => "connected_model_preview",
            Scene::ModelLightbox => "model_lightbox",
            Scene::ConnectionLost => "connection_lost",
            Scene::CertPending => "cert_pending",
            Scene::WizardSelectMethod => "wizard_select_method",
            Scene::WizardGenerateLocal => "wizard_generate_local",
            Scene::WizardSelectAgentKey => "wizard_select_agent_key",
            Scene::WizardError => "wizard_error",
            Scene::UnlockPrompt => "unlock_prompt",
            Scene::SettingsConnection => "settings_connection",
            Scene::ToolbarVoiceModeOpen => "toolbar_voice_mode_open",
            Scene::SettingsDevices => "settings_devices",
            Scene::SettingsVoice => "settings_voice",
            Scene::SettingsProcessing => "settings_processing",
            Scene::SettingsSounds => "settings_sounds",
            Scene::SettingsChat => "settings_chat",
            Scene::SettingsShortcuts => "settings_shortcuts",
            Scene::SettingsFiles => "settings_files",
            Scene::SettingsStats => "settings_stats",
            Scene::SettingsAdmin => "settings_admin",
            Scene::SettingsAbout => "settings_about",
            Scene::RoomAclModal => "room_acl_modal",
            Scene::ElevatePrompt => "elevate_prompt",
            Scene::SettingsConnectionSudo => "settings_connection_sudo",
            Scene::SettingsConnectionElevated => "settings_connection_elevated",
            Scene::SettingsConnectionLocalKey => "settings_connection_local_key",
            Scene::SettingsConnectionSshAgent => "settings_connection_ssh_agent",
            Scene::FileSharePending => "file_share_pending",
            Scene::FileShareFailed => "file_share_failed",
        }
    }

    fn build_state(self) -> State {
        match self {
            Scene::Disconnected | Scene::DisconnectedEmpty | Scene::AddServerModal | Scene::EditServerModal => {
                State::default()
            }
            Scene::Connecting => State {
                connection: ConnectionState::Connecting {
                    server_addr: "rumble.example:5000".into(),
                },
                ..State::default()
            },
            Scene::Connected | Scene::ConnectedImagePreview | Scene::ImageLightbox | Scene::ImageLightboxZoomed => {
                connected_state()
            }
            Scene::ConnectedModelPreview | Scene::ModelLightbox => connected_state_with_model(),
            Scene::ConnectedSelfTalking => {
                let mut s = connected_state();
                s.audio.is_transmitting = true;
                s
            }
            Scene::ConnectedUserStates => user_states_state(),
            Scene::ConnectedAudioFault => {
                // Session is "connected" but both audio devices have
                // faulted: a hard mic failure (destructive badge) and a
                // mid-session speaker loss the task is retrying (warning
                // badge). Drives the under-toolbar fault banner.
                let mut s = connected_state();
                s.audio.input_fault = Some(DeviceFault {
                    message: "Default input device unavailable".into(),
                    recovering: false,
                });
                s.audio.output_fault = Some(DeviceFault {
                    message: "USB headset disconnected".into(),
                    recovering: true,
                });
                s
            }
            Scene::ConnectedSlashCommands | Scene::ConnectedSlashCommandHighlighted => {
                let mut s = connected_state();
                // Server-advertised commands; merged with the client builtins
                // (/msg, /tree) in the suggestion list.
                s.slash_commands = vec![
                    rumble_protocol::proto::SlashCommand {
                        name: "echo".into(),
                        description: "Summon an echo bot that plays your audio back".into(),
                    },
                    rumble_protocol::proto::SlashCommand {
                        name: "link-cleaner".into(),
                        description: "Strip tracking params from posted links".into(),
                    },
                ];
                s
            }
            Scene::ConnectionLost => State {
                connection: ConnectionState::ConnectionLost {
                    error: "stream closed by peer".into(),
                },
                ..State::default()
            },
            Scene::CertPending => State {
                connection: ConnectionState::CertificatePending {
                    cert_info: demo_pending_cert(),
                },
                ..State::default()
            },
            Scene::WizardSelectMethod
            | Scene::WizardGenerateLocal
            | Scene::WizardSelectAgentKey
            | Scene::WizardError
            | Scene::UnlockPrompt
            | Scene::SettingsConnection
            | Scene::SettingsConnectionLocalKey
            | Scene::SettingsConnectionSshAgent
            | Scene::SettingsChat
            | Scene::SettingsShortcuts
            | Scene::SettingsVoice
            | Scene::SettingsSounds
            | Scene::SettingsFiles
            | Scene::SettingsAbout => State::default(),
            // Toolbar dropdown only renders while connected (the trigger
            // is gated on `state.connection.is_connected()`), so reuse
            // the live-session fixture so the popover anchors correctly.
            Scene::ToolbarVoiceModeOpen => connected_state(),
            // The Processing scene needs a populated TX pipeline so
            // the editor isn't an empty placeholder; the MockBackend
            // discards `UpdateTxPipeline` so we seed the state up
            // front. A simulated -22 dB input level paints the meter.
            Scene::SettingsProcessing => State {
                audio: processing_state(),
                ..State::default()
            },
            // The Devices scene needs realistic input/output device
            // lists so the dropdown menu is non-trivial; the Stats
            // scene needs non-zero counters so the read-only grid
            // shows real numbers.
            Scene::SettingsDevices => State {
                audio: device_state(),
                ..State::default()
            },
            // Stats values come from the MockBackend's stats() snapshot
            // (see build_stats), not from State — the read-only grid
            // reads them via the backend, not app_state.
            Scene::SettingsStats => State::default(),
            Scene::SettingsAdmin => admin_state(),
            Scene::RoomAclModal => room_acl_state(),
            Scene::ElevatePrompt | Scene::SettingsConnectionSudo => sudo_capable_state(false),
            Scene::SettingsConnectionElevated => sudo_capable_state(true),
            Scene::FileSharePending | Scene::FileShareFailed => file_share_state(self),
        }
    }

    /// Meter snapshot the MockBackend hands to the UI on `meter()`.
    /// Scenes that show level bars seed representative dB values so the
    /// dumped SVG looks like a real session; everything else uses the
    /// default (`Unmeasured`) snapshot.
    fn build_meter(self) -> rumble_client::MeterSnapshot {
        use rumble_client::{Level, MeterSnapshot};
        match self {
            Scene::SettingsDevices => MeterSnapshot {
                input_pre: Level::Db(-18.0),
                ..MeterSnapshot::default()
            },
            Scene::SettingsProcessing => MeterSnapshot {
                input_pre: Level::Db(-18.0),
                input_post: Level::Db(-22.0),
            },
            _ => MeterSnapshot::default(),
        }
    }

    /// Live per-stage pipeline outputs the MockBackend hands to the UI on
    /// `outputs()`. The Processing scene seeds a representative RNNoise VAD
    /// probability (and an open gate) plus a noise-gate level so the dumped
    /// meters show filled bars past their triggers; other scenes use the
    /// empty frame. Slot order matches `OutputLayout::derive` for the
    /// default pipeline: the gain stage declares no outputs, so denoise's
    /// `vad_probability` is slot 0 and `voice_active` slot 1, then the
    /// noise gate's `level_db` is slot 2 and `gate_open` slot 3.
    fn build_outputs(self) -> rumble_client::OutputFrame {
        match self {
            Scene::SettingsProcessing => {
                let mut frame = rumble_client::OutputFrame::default();
                frame.values[0] = 0.72; // voice probability → green zone
                frame.values[1] = 1.0; // gate open
                frame.values[2] = -22.0; // noise-gate level (dB) → above trigger
                frame.values[3] = 1.0; // noise gate open
                frame.len = 4;
                frame
            }
            _ => rumble_client::OutputFrame::default(),
        }
    }

    /// Stats roll-up the MockBackend hands to the UI on `stats()`. Only
    /// the Stats scene needs realistic counters; others use the default
    /// (all-zero) roll-up.
    fn build_stats(self) -> AudioStats {
        match self {
            Scene::SettingsStats => AudioStats {
                actual_bitrate_bps: 64_000.0,
                avg_frame_size_bytes: 159.4,
                packets_sent: 12_804,
                packets_received: 12_731,
                packets_lost: 73,
                packets_recovered_fec: 41,
                frames_concealed: 14,
                playback_buffer_packets: 3,
                ..AudioStats::default()
            },
            _ => AudioStats::default(),
        }
    }

    /// Transfers list that the MockBackend exposes via `transfers()`.
    /// Only file_share_* scenes use this; everything else is empty.
    fn build_transfers(self) -> Vec<rumble_client_traits::file_transfer::TransferStatus> {
        use rumble_client_traits::file_transfer::{TransferDirection, TransferId, TransferStage, TransferStatus};
        match self {
            Scene::FileSharePending => vec![TransferStatus {
                id: TransferId("demo-pending".into()),
                name: "design-mockups.tar.gz".into(),
                size: 24 * 1024 * 1024,
                direction: TransferDirection::Upload,
                stage: TransferStage::Active {
                    progress: 0.37,
                    speed_bps: 3 * 1024 * 1024,
                },
                peers: 1,
                peer_details: Vec::new(),
            }],
            Scene::FileShareFailed => vec![TransferStatus {
                id: TransferId("demo-failed".into()),
                name: "movie.mkv".into(),
                size: 4_500_000_000,
                direction: TransferDirection::Upload,
                stage: TransferStage::Failed {
                    reason: "file too large (4.19 GiB); limit is 256 MiB".into(),
                },
                peers: 0,
                peer_details: Vec::new(),
            }],
            _ => Vec::new(),
        }
    }

    /// Drive any local UI state the scene needs by injecting synthetic
    /// events through the real `App::on_event` path. This means the
    /// rendered scene is exactly what the user would see after
    /// performing the same interaction — there's no "fixture-only"
    /// shortcut that the production code can drift away from.
    fn drive_setup(self, app: &mut RumbleApp<MockBackend>) {
        match self {
            Scene::Disconnected => {
                app.set_recent_servers_for_test(demo_recent_servers());
            }
            Scene::DisconnectedEmpty => {
                app.set_recent_servers_for_test(Vec::new());
            }
            Scene::AddServerModal => {
                app.set_recent_servers_for_test(demo_recent_servers());
                app.set_server_form_for_test(rumble_damascene::ServerForm {
                    editing_addr: None,
                    addr: "rumble.new-org.example:5000".to_string(),
                    label: "New org".to_string(),
                    username: "alice".to_string(),
                    error: None,
                });
            }
            Scene::EditServerModal => {
                app.set_recent_servers_for_test(demo_recent_servers());
                app.set_server_form_for_test(rumble_damascene::ServerForm {
                    editing_addr: Some("127.0.0.1:5000".to_string()),
                    addr: "127.0.0.1:5000".to_string(),
                    label: "Local dev".to_string(),
                    username: "alice".to_string(),
                    error: None,
                });
            }
            Scene::WizardSelectMethod => {
                app.set_wizard_state_for_test(WizardState::SelectMethod);
            }
            Scene::WizardGenerateLocal => {
                app.set_wizard_state_for_test(WizardState::GenerateLocal {
                    password: "hunter2".to_string(),
                    confirm: "hunter".to_string(),
                    error: None,
                });
            }
            Scene::WizardSelectAgentKey => {
                app.set_wizard_state_for_test(WizardState::SelectAgentKey {
                    keys: demo_agent_keys(),
                    selected: Some(1),
                    error: None,
                });
            }
            Scene::WizardError => {
                app.set_wizard_state_for_test(WizardState::Error {
                    message: "Failed to connect to SSH agent: SSH_AUTH_SOCK is not set".to_string(),
                });
            }
            Scene::UnlockPrompt => {
                app.set_unlock_state_for_test(UnlockState {
                    password: "••••".to_string(),
                    error: Some("Wrong password — try again.".to_string()),
                });
            }
            Scene::SettingsConnection => app.open_settings_for_test(SettingsTab::Connection),
            Scene::ToolbarVoiceModeOpen => app.set_voice_mode_menu_open_for_test(true),
            Scene::ConnectedSlashCommands => app.set_chat_input_for_test("/"),
            Scene::ConnectedSlashCommandHighlighted => {
                app.set_chat_input_for_test("/");
                // Sorted list is echo, link-cleaner, msg, tree; highlight a
                // non-top row so the accent fill is unmistakable in the golden.
                app.set_chat_command_selected_for_test(Some(1));
            }
            Scene::SettingsDevices => {
                app.open_settings_for_test(SettingsTab::Devices);
                app.open_settings_dropdown_for_test(SettingsOpenSelect::InputDevice);
            }
            Scene::SettingsVoice => app.open_settings_for_test(SettingsTab::Voice),
            Scene::SettingsProcessing => app.open_settings_for_test(SettingsTab::Processing),
            Scene::SettingsSounds => app.open_settings_for_test(SettingsTab::Sounds),
            Scene::SettingsChat => {
                app.open_settings_for_test(SettingsTab::Chat);
                app.open_settings_dropdown_for_test(SettingsOpenSelect::TimestampFormat);
            }
            Scene::SettingsShortcuts => app.open_settings_for_test(SettingsTab::Shortcuts),
            Scene::SettingsFiles => app.open_settings_for_test(SettingsTab::Files),
            Scene::SettingsStats => app.open_settings_for_test(SettingsTab::Stats),
            Scene::SettingsAdmin => {
                app.open_settings_for_test(SettingsTab::Admin);
                app.set_admin_expanded_group_for_test(Some("moderators".to_string()));
            }
            Scene::SettingsAbout => app.open_settings_for_test(SettingsTab::About),
            Scene::RoomAclModal => {
                app.open_room_acl_modal_for_test(Uuid::from_u128(ROOM_STAGE));
            }
            Scene::ElevatePrompt => {
                app.set_elevate_state_for_test(ElevateState {
                    password: "••••••••".to_string(),
                    error: Some("Incorrect password".to_string()),
                });
            }
            Scene::SettingsConnectionSudo | Scene::SettingsConnectionElevated => {
                app.open_settings_for_test(SettingsTab::Connection);
            }
            Scene::SettingsConnectionLocalKey => {
                app.set_local_identity_for_test();
                app.open_settings_for_test(SettingsTab::Connection);
            }
            Scene::SettingsConnectionSshAgent => {
                app.set_ssh_agent_identity_for_test();
                app.open_settings_for_test(SettingsTab::Connection);
            }
            Scene::ConnectedImagePreview => {
                // Pre-decoded image so the preview path renders without
                // a real file-transfer plugin underneath. The fixture
                // is a 64×40 gradient — small enough to fit inline as
                // a tree-dump constant but big enough to read as a
                // proper raster, not a single-pixel swatch.
                app.insert_image_preview_for_test("demo-offer", demo_preview_image());
            }
            Scene::ImageLightbox => {
                // Same fixture as the inline-preview scene, plus the
                // lightbox toggled open over it. The connected backdrop
                // makes the modal scrim's contrast visible.
                app.insert_image_preview_for_test("demo-offer", demo_preview_image());
                app.open_lightbox_for_test("demo-offer", "deploy_notes.md");
            }
            Scene::ImageLightboxZoomed => {
                // 200% zoom + a small downward-right pan, so the
                // header zoom controls render with `+` / `−` / Fit
                // all enabled and the percentage label reads "200%".
                app.insert_image_preview_for_test("demo-offer", demo_preview_image());
                app.open_lightbox_for_test("demo-offer", "deploy_notes.md");
                app.set_lightbox_zoom_for_test(2.0, (40.0, 30.0));
            }
            Scene::ConnectedModelPreview => {
                // Seed a poster directly (no GPU in bundle dumps) so the
                // dispatch swaps the file card for the 3D preview card.
                app.insert_model_thumb_for_test("demo-model", demo_preview_image());
            }
            Scene::ModelLightbox => {
                // Seed parsed geometry and open the orbit lightbox. The
                // SVG fallback renders the `chart3d` element's draw op
                // deterministically (auto-framed from the cube's bounds).
                app.open_model_lightbox_for_test("demo-model", "bracket.stl", demo_cube_model());
            }
            _ => {}
        }
    }

    /// True for scenes that purposefully render the first-run / unlock
    /// modal — those need the suppression hook left alone.
    fn keeps_first_run(self) -> bool {
        matches!(
            self,
            Scene::WizardSelectMethod
                | Scene::WizardGenerateLocal
                | Scene::WizardSelectAgentKey
                | Scene::WizardError
                | Scene::UnlockPrompt
        )
    }
}

// ---------------------------------------------------------------------
// Canned data for the "Connected" scene
// ---------------------------------------------------------------------

const ROOM_LOBBY: u128 = 0x1111_1111_1111_1111_1111_1111_1111_1111;
const ROOM_WORK: u128 = 0x2222_2222_2222_2222_2222_2222_2222_2222;
const ROOM_STAGE: u128 = 0x3333_3333_3333_3333_3333_3333_3333_3333;
const ROOM_STAGE_GAMING: u128 = 0x4444_4444_4444_4444_4444_4444_4444_4444;

fn make_room(uuid: u128, name: &str) -> RoomInfo {
    RoomInfo {
        id: Some(room_id_from_uuid(Uuid::from_u128(uuid))),
        name: name.into(),
        parent_id: None,
        description: None,
        inherit_acl: false,
        acls: Vec::new(),
        effective_permissions: 0,
    }
}

fn make_user(id: u64, name: &str, room: u128, mut tweak: impl FnMut(&mut User)) -> User {
    let mut u = User {
        user_id: Some(UserId { value: id }),
        username: name.into(),
        current_room: Some(room_id_from_uuid(Uuid::from_u128(room))),
        is_muted: false,
        is_deafened: false,
        server_muted: false,
        is_elevated: false,
        groups: Vec::new(),
        label: None,
    };
    tweak(&mut u);
    u
}

fn make_chat(id: u8, sender: &str, text: &str, kind: ChatMessageKind) -> ChatMessage {
    make_chat_full(id, sender, text, kind, None)
}

fn make_chat_full(
    id: u8,
    sender: &str,
    text: &str,
    kind: ChatMessageKind,
    attachment: Option<ChatAttachment>,
) -> ChatMessage {
    let mut bytes = [0u8; 16];
    bytes[15] = id;
    ChatMessage {
        id: bytes,
        sender: sender.into(),
        sender_id: None,
        text: text.into(),
        timestamp: SystemTime::UNIX_EPOCH,
        kind,
        attachment,
        visibility: ChatMessageVisibility::Normal,
    }
}

/// Helper: build a relay-plugin ChatAttachment for fixture chat messages.
fn relay_attachment(payload: RelayFileSharePayload, fallback_text: &str) -> ChatAttachment {
    use prost::Message;
    ChatAttachment {
        namespace: rumble_desktop::FILE_TRANSFER_RELAY_NAMESPACE.to_string(),
        schema_version: rumble_desktop::FILE_TRANSFER_RELAY_PAYLOAD_SCHEMA_VERSION,
        payload: payload.encode_to_vec(),
        fallback_text: fallback_text.into(),
    }
}

/// Fixture for the Admin scene: a connected session where the local
/// user has `MANAGE_ACL` (so the tab is gated open), a mix of built-in
/// and custom groups, four users with overlapping group membership,
/// and a couple of rooms that reference the custom groups in ACLs so
/// "Used in N rooms" surfaces a non-zero count.
fn admin_state() -> State {
    let mut state = State {
        connection: ConnectionState::Connected {
            server_name: "rumble.example".into(),
            user_id: 1,
        },
        rooms: vec![
            RoomInfo {
                acls: vec![RoomAclEntry {
                    group: "moderators".into(),
                    grant: (Permissions::MUTE_DEAFEN | Permissions::MOVE_USER).bits(),
                    deny: 0,
                    apply_here: true,
                    apply_subs: true,
                }],
                ..make_room(ROOM_LOBBY, "Lobby")
            },
            RoomInfo {
                acls: vec![RoomAclEntry {
                    group: "streamers".into(),
                    grant: Permissions::SPEAK.bits(),
                    deny: 0,
                    apply_here: true,
                    apply_subs: false,
                }],
                ..make_room(ROOM_WORK, "Stage")
            },
        ],
        users: vec![
            make_user(1, "alice", ROOM_LOBBY, |u| {
                u.groups = vec!["admin".into(), "moderators".into()];
            }),
            make_user(2, "bob", ROOM_LOBBY, |u| {
                u.groups = vec!["moderators".into()];
            }),
            make_user(3, "charlie", ROOM_WORK, |u| {
                u.groups = vec!["moderators".into(), "streamers".into()];
            }),
            make_user(4, "diana", ROOM_WORK, |u| {
                u.groups = vec!["streamers".into()];
            }),
            make_user(5, "eve", ROOM_LOBBY, |_| {}),
        ],
        my_user_id: Some(1),
        my_room_id: Some(Uuid::from_u128(ROOM_LOBBY)),
        effective_permissions: ADMIN_PERMISSIONS.bits(),
        group_definitions: vec![
            GroupInfo {
                name: "default".into(),
                permissions: DEFAULT_PERMISSIONS.bits(),
                is_builtin: true,
            },
            GroupInfo {
                name: "admin".into(),
                permissions: ADMIN_PERMISSIONS.bits(),
                is_builtin: true,
            },
            GroupInfo {
                name: "moderators".into(),
                permissions: (Permissions::MUTE_DEAFEN | Permissions::MOVE_USER | Permissions::KICK).bits(),
                is_builtin: false,
            },
            GroupInfo {
                name: "streamers".into(),
                permissions: 0,
                is_builtin: false,
            },
        ],
        ..State::default()
    };
    state.rebuild_room_tree();
    state
}

/// Fixture for the per-room ACL editor scene. Three-level room tree
/// (Root → Stage → Stage/Gaming with our edit target one level
/// deeper), real ancestor ACLs so the inherited block has rules to
/// list, and the local-rules editor pre-populated with two entries:
/// one all-on, one with a SPEAK deny so the tri-state buttons show
/// both `+` and `−` selections in the dump.
fn room_acl_state() -> State {
    let stage_uuid = Uuid::from_u128(ROOM_STAGE);
    let stage_gaming_uuid = Uuid::from_u128(ROOM_STAGE_GAMING);
    let _ = stage_gaming_uuid; // currently unused; reserved for a future deeper-modal scene.

    let mut state = State {
        connection: ConnectionState::Connected {
            server_name: "rumble.example".into(),
            user_id: 1,
        },
        rooms: vec![
            // Root holds an ACL granting moderators MUTE_DEAFEN —
            // surfaces in the inherited block of the target room as a
            // `/Root` rule.
            RoomInfo {
                acls: vec![RoomAclEntry {
                    group: "moderators".into(),
                    grant: Permissions::MUTE_DEAFEN.bits(),
                    deny: 0,
                    apply_here: true,
                    apply_subs: true,
                }],
                inherit_acl: false,
                ..make_room_with_parent(rumble_protocol::ROOT_ROOM_UUID.as_u128(), "Root", None)
            },
            // Mid-level parent. No own ACL; just contributes a
            // breadcrumb segment.
            make_room_with_parent(ROOM_WORK, "Lobby", Some(rumble_protocol::ROOT_ROOM_UUID.as_u128())),
            // Edit target: a sub-room with two local entries.
            RoomInfo {
                inherit_acl: true,
                acls: vec![
                    RoomAclEntry {
                        group: "streamers".into(),
                        grant: (Permissions::SPEAK | Permissions::SHARE_FILE).bits(),
                        deny: 0,
                        apply_here: true,
                        apply_subs: true,
                    },
                    RoomAclEntry {
                        group: "default".into(),
                        grant: 0,
                        deny: Permissions::SPEAK.bits(),
                        apply_here: true,
                        apply_subs: false,
                    },
                ],
                ..make_room_with_parent(ROOM_STAGE, "Strategy", Some(ROOM_WORK))
            },
        ],
        users: vec![
            make_user(1, "alice", ROOM_STAGE, |u| {
                u.groups = vec!["admin".into(), "moderators".into()];
            }),
            make_user(2, "bob", ROOM_STAGE, |u| {
                u.groups = vec!["streamers".into()];
            }),
        ],
        my_user_id: Some(1),
        my_room_id: Some(stage_uuid),
        effective_permissions: (Permissions::WRITE | Permissions::TEXT_MESSAGE | Permissions::SPEAK).bits(),
        group_definitions: vec![
            GroupInfo {
                name: "default".into(),
                permissions: DEFAULT_PERMISSIONS.bits(),
                is_builtin: true,
            },
            GroupInfo {
                name: "admin".into(),
                permissions: ADMIN_PERMISSIONS.bits(),
                is_builtin: true,
            },
            GroupInfo {
                name: "moderators".into(),
                permissions: (Permissions::MUTE_DEAFEN | Permissions::MOVE_USER).bits(),
                is_builtin: false,
            },
            GroupInfo {
                name: "streamers".into(),
                permissions: 0,
                is_builtin: false,
            },
        ],
        ..State::default()
    };
    state.rebuild_room_tree();
    state
}

/// Same as `make_room` but lets the fixture set a parent_id. The
/// existing `make_room` always returns root-level rooms; the room-acl
/// scene needs a real tree depth so the breadcrumb has segments.
fn make_room_with_parent(uuid: u128, name: &str, parent: Option<u128>) -> RoomInfo {
    RoomInfo {
        id: Some(room_id_from_uuid(Uuid::from_u128(uuid))),
        name: name.into(),
        parent_id: parent.map(|p| room_id_from_uuid(Uuid::from_u128(p))),
        description: None,
        inherit_acl: true,
        acls: Vec::new(),
        effective_permissions: 0,
    }
}

/// Fixture for the FileShare* scenes: connected session with a single
/// sender-side SenderDraft file-share message whose card draws state
/// from the plugin's TransferStatus (set up via `build_transfers`).
fn file_share_state(scene: Scene) -> State {
    let mut state = connected_state();
    let (transfer_id, name, size, mime, fallback, summary) = match scene {
        Scene::FileSharePending => (
            "demo-pending",
            "design-mockups.tar.gz",
            24 * 1024 * 1024,
            "application/gzip",
            "shared file: design-mockups.tar.gz (24 MB)",
            "shared file: design-mockups.tar.gz (24 MB)",
        ),
        Scene::FileShareFailed => (
            "demo-failed",
            "movie.mkv",
            4_500_000_000,
            "video/x-matroska",
            "shared file: movie.mkv (4.19 GB)",
            "shared file: movie.mkv (4.19 GB)",
        ),
        _ => unreachable!(),
    };
    let attachment = relay_attachment(
        RelayFileSharePayload {
            transfer_id: transfer_id.into(),
            name: name.into(),
            size,
            mime: mime.into(),
            share_data: String::new(),
        },
        fallback,
    );
    let mut msg = make_chat_full(20, "alice", summary, ChatMessageKind::Room, Some(attachment));
    msg.visibility = ChatMessageVisibility::SenderDraft;
    std::sync::Arc::make_mut(&mut state.chat_messages).push(msg);
    state
}

fn connected_state() -> State {
    let mut audio = AudioState {
        voice_mode: VoiceMode::Continuous,
        ..AudioState::default()
    };
    // Bob is talking; Charlie is self-muted.
    audio.talking_users.insert(2);

    let mut state = State {
        connection: ConnectionState::Connected {
            server_name: "rumble.example".into(),
            user_id: 1,
        },
        rooms: vec![make_room(ROOM_LOBBY, "Lobby"), make_room(ROOM_WORK, "Work")],
        users: vec![
            make_user(1, "alice", ROOM_LOBBY, |_| {}),
            make_user(2, "bob", ROOM_WORK, |_| {}),
            make_user(3, "charlie", ROOM_LOBBY, |u| u.is_muted = true),
            make_user(4, "diana", ROOM_WORK, |u| u.server_muted = true),
        ],
        my_user_id: Some(1),
        my_room_id: Some(Uuid::from_u128(ROOM_LOBBY)),
        audio,
        chat_messages: std::sync::Arc::new(vec![
            make_chat(1, "alice", "morning everyone", ChatMessageKind::Room),
            make_chat(2, "bob", "did the deploy go through?", ChatMessageKind::Room),
            make_chat(
                3,
                "charlie",
                "(announcement) maintenance window 14:00 UTC",
                ChatMessageKind::Tree,
            ),
            make_chat_full(
                4,
                "diana",
                "ping me when you're free",
                ChatMessageKind::DirectMessage {
                    other_user_id: 4,
                    other_username: "diana".into(),
                },
                None,
            ),
            make_chat_full(
                5,
                "bob",
                "shared file: deploy_notes.md (12.3 KB)",
                ChatMessageKind::Room,
                Some(relay_attachment(
                    RelayFileSharePayload {
                        transfer_id: "demo-offer".into(),
                        name: "deploy_notes.md".into(),
                        size: 12_345,
                        mime: "text/markdown".into(),
                        share_data: "demo".into(),
                    },
                    "shared file: deploy_notes.md (12.3 KB)",
                )),
            ),
        ]),
        // The chat composer enables itself only when the local user has
        // TEXT_MESSAGE in the current room, so seed the bit so the
        // connected scene shows the active composer (not the disabled
        // hint variant).
        effective_permissions: Permissions::TEXT_MESSAGE.bits(),
        ..State::default()
    };
    state.rebuild_room_tree();
    state
}

/// Connected state with every per-user voice-state glyph present in a
/// single room, so the room-tree status icons each render at least once:
/// idle (self), talking, self-muted, server-muted, self-deafened, and a
/// locally-muted peer (whose `muted_local` badge rides alongside its idle
/// glyph). Deafen sets `is_muted` too, mirroring the client's deafen⇒mute.
fn user_states_state() -> State {
    let mut audio = AudioState {
        voice_mode: VoiceMode::Continuous,
        ..AudioState::default()
    };
    audio.talking_users.insert(2); // bob is talking
    audio.muted_users.insert(6); // we've locally muted frank

    let mut state = State {
        connection: ConnectionState::Connected {
            server_name: "rumble.example".into(),
            user_id: 1,
        },
        rooms: vec![make_room(ROOM_LOBBY, "Lobby")],
        users: vec![
            make_user(1, "alice", ROOM_LOBBY, |_| {}),                    // self, idle
            make_user(2, "bob", ROOM_LOBBY, |_| {}),                      // talking (via talking_users)
            make_user(3, "charlie", ROOM_LOBBY, |u| u.is_muted = true),   // self-muted
            make_user(4, "diana", ROOM_LOBBY, |u| u.server_muted = true), // server-muted
            make_user(5, "erin", ROOM_LOBBY, |u| {
                // Self-deafened: client forces mute on deafen, so both set.
                u.is_deafened = true;
                u.is_muted = true;
            }),
            make_user(6, "frank", ROOM_LOBBY, |_| {}), // locally muted (badge via muted_users)
        ],
        my_user_id: Some(1),
        my_room_id: Some(Uuid::from_u128(ROOM_LOBBY)),
        audio,
        effective_permissions: Permissions::TEXT_MESSAGE.bits(),
        ..State::default()
    };
    state.rebuild_room_tree();
    state
}

/// Connected state with `SUDO` (and `TEXT_MESSAGE`) on the local user
/// so the Connection-tab elevate UI renders. `elevated` flips the
/// local user's `is_elevated` flag — driving the gold name in the room
/// tree and swapping the button for the "already elevated" notice in
/// Settings.
fn sudo_capable_state(elevated: bool) -> State {
    let mut state = connected_state();
    state.effective_permissions = (Permissions::TEXT_MESSAGE | Permissions::SUDO).bits();
    if let Some(me) = state
        .users
        .iter_mut()
        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(1))
    {
        me.is_elevated = elevated;
    }
    state
}

fn device_state() -> AudioState {
    AudioState {
        input_devices: vec![
            AudioDeviceInfo {
                id: "alsa:default".to_string(),
                name: "Default (PulseAudio)".to_string(),
                pipeline: None,
                is_default: true,
            },
            AudioDeviceInfo {
                id: "alsa:usb-mic".to_string(),
                name: "Blue Yeti USB Microphone".to_string(),
                pipeline: None,
                is_default: false,
            },
            AudioDeviceInfo {
                id: "alsa:webcam".to_string(),
                name: "Logitech HD Pro Webcam C920".to_string(),
                pipeline: None,
                is_default: false,
            },
        ],
        output_devices: vec![
            AudioDeviceInfo {
                id: "alsa:hdmi".to_string(),
                name: "HDMI 1 (Built-in Audio)".to_string(),
                pipeline: None,
                is_default: false,
            },
            AudioDeviceInfo {
                id: "alsa:headphones".to_string(),
                name: "Sennheiser HD 600 (USB DAC)".to_string(),
                pipeline: None,
                is_default: true,
            },
        ],
        selected_input: Some("alsa:usb-mic".to_string()),
        selected_output: Some("alsa:headphones".to_string()),
        ..AudioState::default()
    }
}

fn processing_state() -> AudioState {
    use rumble_client::{ProcessorRegistry, build_default_tx_pipeline, register_builtin_processors};
    let mut registry = ProcessorRegistry::new();
    register_builtin_processors(&mut registry);
    let mut tx_pipeline = build_default_tx_pipeline(&registry);
    // Flip the noise gate on so the level meter renders its
    // threshold-break colouring (and the stage's own live meter);
    // otherwise the fixture would show the no-gate fallback.
    if let Some(gate) = tx_pipeline
        .processors
        .iter_mut()
        .find(|p| p.type_id == rumble_client::processors::type_ids::NOISE_GATE)
    {
        gate.enabled = true;
    }
    AudioState {
        tx_pipeline,
        ..AudioState::default()
    }
}

/// Three saved-server entries with realistic last-used spread, so the
/// list scene exercises sort order, label vs no-label rows, and the
/// "never connected yet" subtitle (last_used_unix == 0).
fn demo_recent_servers() -> Vec<RecentServer> {
    // Anchor timestamps relative to a fixed wall clock so the rendered
    // "n minutes ago" text is reproducible across runs of the tool.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    vec![
        RecentServer {
            addr: "127.0.0.1:5000".into(),
            label: "Local dev".into(),
            username: "alice".into(),
            last_used_unix: now.saturating_sub(120),
        },
        RecentServer {
            addr: "rumble.example:5000".into(),
            label: String::new(),
            username: "alice".into(),
            last_used_unix: now.saturating_sub(2 * 86_400),
        },
        RecentServer {
            addr: "voice.team-blue.example:5000".into(),
            label: "Team Blue".into(),
            username: "alice@team-blue".into(),
            // now-relative (not absolute 0) so the rendered "Nd ago" stays
            // fixed across days — an absolute epoch would drift daily.
            last_used_unix: now.saturating_sub(12 * 86_400),
        },
    ]
}

fn demo_agent_keys() -> Vec<KeyInfo> {
    vec![
        KeyInfo {
            fingerprint: "SHA256:7gK3qPL5dEvF8sN1xR9wT2yJ4mB6cZ0aV/X+kH=".into(),
            comment: "alice@workstation".into(),
            public_key: [0u8; 32],
        },
        KeyInfo {
            fingerprint: "SHA256:Q9mNxV3pA2dLcK7yE5sT1jR4hF6oZ8bU/W+iH==".into(),
            comment: "rumble-identity".into(),
            public_key: [1u8; 32],
        },
        KeyInfo {
            fingerprint: "SHA256:M2tXrY5pL7vC1qA8nB3kE9oH4jD6sZ0wU/V+iK==".into(),
            comment: "yubikey".into(),
            public_key: [2u8; 32],
        },
    ]
}

/// Tiny synthetic gradient used as the inline-preview fixture in
/// `Scene::ConnectedImagePreview`. 64×40 RGBA8 with a horizontal red
/// channel ramp + vertical blue ramp so the rendered preview is
/// visually distinguishable from a placeholder rectangle in tree
/// dumps and SVG snapshots alike.
fn demo_preview_image() -> Image {
    let (w, h) = (64u32, 40u32);
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 255) / w.max(1)) as u8;
            let g = 80u8;
            let b = ((y * 255) / h.max(1)) as u8;
            pixels.extend_from_slice(&[r, g, b, 0xff]);
        }
    }
    Image::from_rgba8(w, h, pixels)
}

/// Connected state whose shared-file message carries a 3D-model offer
/// (`bracket.stl`) instead of the default markdown file, so the model
/// preview / lightbox scenes dispatch through the model path.
fn connected_state_with_model() -> State {
    let mut state = connected_state();
    if let Some(msg) = std::sync::Arc::make_mut(&mut state.chat_messages).last_mut() {
        let summary = "shared model: bracket.stl (48.0 KB)";
        msg.text = summary.into();
        msg.attachment = Some(relay_attachment(
            RelayFileSharePayload {
                transfer_id: "demo-model".into(),
                name: "bracket.stl".into(),
                size: 49_152,
                mime: "model/stl".into(),
                share_data: "demo".into(),
            },
            summary,
        ));
    }
    state
}

/// A unit cube as parsed model geometry, for the lightbox scene. Flat
/// per-face normals; deterministic, so the auto-framed `chart3d` draw op
/// is stable across runs.
fn demo_cube_model() -> rumble_damascene::model::LoadedModel {
    use damascene_core::scene::{MeshData, MeshHandle, MeshVertex, glam::Vec3};

    type Face = ([f32; 3], [[f32; 3]; 4]);
    let faces: [Face; 6] = [
        (
            [0., 0., 1.],
            [[-1., -1., 1.], [1., -1., 1.], [1., 1., 1.], [-1., 1., 1.]],
        ),
        (
            [0., 0., -1.],
            [[1., -1., -1.], [-1., -1., -1.], [-1., 1., -1.], [1., 1., -1.]],
        ),
        (
            [1., 0., 0.],
            [[1., -1., 1.], [1., -1., -1.], [1., 1., -1.], [1., 1., 1.]],
        ),
        (
            [-1., 0., 0.],
            [[-1., -1., -1.], [-1., -1., 1.], [-1., 1., 1.], [-1., 1., -1.]],
        ),
        (
            [0., 1., 0.],
            [[-1., 1., 1.], [1., 1., 1.], [1., 1., -1.], [-1., 1., -1.]],
        ),
        (
            [0., -1., 0.],
            [[-1., -1., -1.], [1., -1., -1.], [1., -1., 1.], [-1., -1., 1.]],
        ),
    ];
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (normal, corners) in faces {
        let base = vertices.len() as u32;
        let n = Vec3::from_array(normal);
        for c in corners {
            vertices.push(MeshVertex {
                position: Vec3::from_array(c),
                normal: n,
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    rumble_damascene::model::LoadedModel::Mesh(MeshHandle::new(MeshData {
        vertices,
        indices: Some(indices),
    }))
}

fn demo_pending_cert() -> PendingCertificate {
    PendingCertificate {
        certificate_der: vec![0u8; 32],
        fingerprint: [
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x12, 0x34,
            0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0,
        ],
        server_name: "rumble.example".into(),
        server_addr: "rumble.example:5000".into(),
        username: "alice".into(),
        password: None,
        public_key: [0u8; 32],
    }
}

// ---------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Each scene iteration constructs a fresh RumbleApp, which opens a
    // new XDG GlobalShortcuts portal session on Wayland. After several
    // iterations xdg-desktop-portal stops responding and the dump hangs
    // mid-run. Disable portal init here — `dump_bundles` doesn't need
    // global hotkeys to render the UI tree.
    //
    // Safety: set before any thread is spawned (tokio runtimes are
    // built inside the per-scene loop below).
    unsafe {
        std::env::set_var("RUMBLE_DISABLE_PORTAL", "1");
    }

    // Pin the About-tab build metadata to a fixed fixture. The real values
    // come from `build.rs` (live `git rev-parse` / `git describe`), so they
    // move every commit and made `settings_about` drift on each rebless.
    // Layout is unchanged — same 7+40 char hash shape, empty tag, clean
    // tree — so the scene still exercises ellipsis/fill-width and the
    // clean-state color token under lint.
    set_git_build_info_override(GitBuildInfo {
        describe: "v0.1.1-0-g0123456".to_string(),
        short_hash: "0123456".to_string(),
        full_hash: "0123456789abcdef0123456789abcdef01234567".to_string(),
        tag: String::new(),
        dirty: false,
    });

    // Match the real app's window viewport so layout matches what users see.
    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out");

    // Identity / SettingsStore both want a config dir to write to.
    // Fixtures don't actually mutate either, but the constructors
    // create files, so use a process-local scratch dir. The path is
    // pinned (not `env::temp_dir()`) because it renders into the
    // connection scenes' draw ops — a TMPDIR-dependent path would make
    // the goldens differ between shells.
    let scratch = PathBuf::from("/tmp").join("rumble_damascene_dump_bundles");
    std::fs::create_dir_all(&scratch)?;

    // First positional arg may select a mode; the rest are scene names.
    let mut raw_args = std::env::args().skip(1).peekable();
    let mode = match raw_args.peek().map(String::as_str) {
        Some("--bless") => {
            raw_args.next();
            Mode::Bless
        }
        Some("--check") => {
            raw_args.next();
            Mode::Check
        }
        _ => Mode::Dump,
    };

    let requested: Vec<Scene> = raw_args
        .map(|raw| parse_scene(&raw).unwrap_or_else(|| panic!("unknown scene `{raw}`")))
        .collect();
    let scenes: Vec<Scene> = if requested.is_empty() {
        Scene::ALL.to_vec()
    } else {
        requested
    };

    match mode {
        Mode::Dump => {
            for scene in scenes {
                let bundle = render_scene(scene, &scratch, viewport)?;
                let basename = format!("rumble_{}", scene.slug());
                let written = write_bundle(&bundle, &out_dir, &basename)?;
                for path in &written {
                    println!("wrote {}", path.display());
                }
                if !bundle.lint.findings.is_empty() {
                    eprintln!("\n{basename} lint findings ({}):", bundle.lint.findings.len());
                    eprint!("{}", bundle.lint.text());
                }
            }
        }
        Mode::Bless => {
            let goldens = goldens_dir();
            std::fs::create_dir_all(&goldens)?;
            for scene in scenes {
                let bundle = render_scene(scene, &scratch, viewport)?;
                let basename = format!("rumble_{}", scene.slug());
                for (ext, content) in golden_artifacts(&bundle) {
                    std::fs::write(goldens.join(format!("{basename}.{ext}")), content)?;
                }
                println!("blessed {basename}");
            }
        }
        Mode::Check => {
            let goldens = goldens_dir();
            let mut failures = 0usize;
            let mut checked = 0usize;
            for scene in &scenes {
                let bundle = render_scene(*scene, &scratch, viewport)?;
                let basename = format!("rumble_{}", scene.slug());
                for (ext, actual) in golden_artifacts(&bundle) {
                    checked += 1;
                    let path = goldens.join(format!("{basename}.{ext}"));
                    let Ok(expected) = std::fs::read_to_string(&path) else {
                        failures += 1;
                        eprintln!("MISSING  {basename}.{ext} — no golden; run `--bless`");
                        continue;
                    };
                    if let Some((line, exp, act, n)) = first_diff(&expected, &actual) {
                        failures += 1;
                        eprintln!(
                            "MISMATCH {basename}.{ext}: {n} line(s) differ; first at line {line}:\n  golden: {exp}\n  \
                             actual: {act}"
                        );
                    }
                }
            }
            if failures > 0 {
                eprintln!(
                    "\n{failures} golden mismatch(es) over {checked} artifact(s) in {} scene(s).\nReview the change, \
                     then re-bless with `dump_bundles --bless` and inspect `git diff goldens/`.",
                    scenes.len()
                );
                std::process::exit(1);
            }
            println!(
                "OK: {checked} golden artifact(s) across {} scene(s) match",
                scenes.len()
            );
        }
    }

    Ok(())
}

/// What `dump_bundles` does with the rendered scenes.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Write the full artifact set (svg/tree/draw_ops/manifest/lint) to `out/`.
    Dump,
    /// Overwrite the checked-in goldens with freshly rendered output.
    Bless,
    /// Render and diff against the checked-in goldens; exit non-zero on drift.
    Check,
}

/// Directory holding the checked-in golden artifacts (tracked, unlike `out/`).
fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("goldens")
}

/// The subset of bundle artifacts promoted to goldens. We pin `draw_ops` (the
/// flat paint IR — geometry, colors, fonts, layering, the same stream the wgpu
/// runner consumes) and `lint` (quality findings). We deliberately do *not* pin
/// `tree.txt` (its `source=app.rs:NNN` references churn on unrelated edits),
/// the SVG, or the shader manifest (both redundant re-renderings of `draw_ops`).
fn golden_artifacts(bundle: &Bundle) -> [(&'static str, String); 2] {
    [
        ("draw_ops.txt", draw_ops_text(&bundle.draw_ops)),
        ("lint.txt", bundle.lint.text()),
    ]
}

/// Build a scene's `RumbleApp` against the `MockBackend` and render one bundle.
/// Shared by every mode so the dump and the golden check exercise an identical
/// path.
fn render_scene(scene: Scene, scratch: &std::path::Path, viewport: Rect) -> Result<Bundle, Box<dyn std::error::Error>> {
    let backend = MockBackend {
        state: scene.build_state(),
        transfers: scene.build_transfers(),
        meter: scene.build_meter(),
        stats: scene.build_stats(),
        outputs: scene.build_outputs(),
    };
    // Each scene gets its own freshly-wiped config dir. Identity and settings
    // persist to disk, so a shared dir would let one scene's generated key or
    // saved-server list bleed into later scenes (and across tool runs),
    // breaking golden reproducibility.
    let cfg = scratch.join(scene.slug());
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(&cfg)?;
    let identity = Identity::load(cfg.clone())?;
    let settings = SettingsStore::load_from_path(Some(cfg.join("settings.json")));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let mut app = RumbleApp::new(backend, identity, settings, runtime);
    // Bypass the first-run wizard for connection-state scenes — they illustrate
    // the main shell, not the wizard. Wizard / unlock scenes drive the wizard
    // explicitly and need the suppression *not* applied.
    if !scene.keeps_first_run() {
        app.suppress_first_run_for_test();
    }
    scene.drive_setup(&mut app);

    let theme = app.theme();
    let cx = BuildCx::new(&theme);
    let mut tree = app.build(&cx);
    Ok(render_bundle(&mut tree, viewport))
}

/// First diverging line between two multi-line strings, with the total count of
/// differing lines. Returns `(line_no, golden_line, actual_line, total_diffs)`,
/// or `None` when the two are identical.
fn first_diff(expected: &str, actual: &str) -> Option<(usize, String, String, usize)> {
    let exp: Vec<&str> = expected.lines().collect();
    let act: Vec<&str> = actual.lines().collect();
    let mut first = None;
    let mut count = 0;
    for i in 0..exp.len().max(act.len()) {
        let e = exp.get(i).copied().unwrap_or("<missing>");
        let a = act.get(i).copied().unwrap_or("<missing>");
        if e != a {
            count += 1;
            if first.is_none() {
                first = Some((i + 1, e.to_string(), a.to_string()));
            }
        }
    }
    first.map(|(n, e, a)| (n, e, a, count))
}

fn parse_scene(raw: &str) -> Option<Scene> {
    Scene::ALL.iter().copied().find(|s| s.slug().eq_ignore_ascii_case(raw))
}
