//! Dump aetna bundle artifacts (svg + tree + draw_ops + lint + manifest)
//! for every canonical UI scene of `rumble-aetna`.
//!
//! Run:
//!   cargo run -p rumble-aetna --bin dump_bundles
//!   cargo run -p rumble-aetna --bin dump_bundles -- connected cert_pending
//!
//! Output: `crates/rumble-aetna/out/rumble_<scene>.{svg,tree.txt,draw_ops.txt,lint.txt,shader_manifest.txt}`.
//!
//! Mirrors the `aetna-volume::render_artifacts` shape: a small
//! `MockBackend` that returns a canned `State`, a `Scene` enum that
//! enumerates the views worth snapshotting, and the same four-line
//! `render_bundle` + `write_bundle` core that aetna-core ships in the
//! prelude. No GPU is involved — the SVG fallback renders the same
//! draw-op stream the wgpu Runner would, so layout regressions show
//! up faithfully without spinning up a window or device.

use std::{path::PathBuf, sync::Arc, time::SystemTime};

use aetna_core::prelude::*;

use rumble_aetna::{
    Identity, RumbleApp, SettingsOpenSelect, SettingsTab, UnlockState, WizardState, backend::UiBackend,
};
use rumble_desktop_shell::{KeyInfo, RecentServer, SettingsStore};
use rumble_protocol::{
    AudioDeviceInfo, AudioState, AudioStats, ChatAttachment, ChatMessage, ChatMessageKind, Command, ConnectionState,
    FileOfferInfo, PendingCertificate, State, Uuid, VoiceMode,
    permissions::Permissions,
    proto::{RoomInfo, User, UserId},
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
}

impl UiBackend for MockBackend {
    fn state(&self) -> State {
        self.state.clone()
    }
    fn send(&self, _command: Command) {}
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
    /// Live session with a completed image transfer rendered as an
    /// inline preview in the chat sidebar.
    ConnectedImagePreview,
    /// Live session with the click-to-enlarge image lightbox open.
    ImageLightbox,
    /// Lightbox at 200% zoom with a non-zero pan, exercising the
    /// `−` / `+` / Fit buttons + zoom-percent label in their
    /// "non-default" state and showing the zoomed paint transform.
    ImageLightboxZoomed,
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
    /// Toolbar "Identity" modal showing the configured key + regenerate.
    IdentityModal,
    /// Settings dialog — Connection tab (default).
    SettingsConnection,
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
    /// Settings dialog — Files tab (auto-download + bandwidth).
    SettingsFiles,
    /// Settings dialog — Stats tab (read-only audio metrics).
    SettingsStats,
}

impl Scene {
    const ALL: &'static [Scene] = &[
        Scene::Disconnected,
        Scene::DisconnectedEmpty,
        Scene::AddServerModal,
        Scene::EditServerModal,
        Scene::Connecting,
        Scene::Connected,
        Scene::ConnectedImagePreview,
        Scene::ImageLightbox,
        Scene::ImageLightboxZoomed,
        Scene::ConnectionLost,
        Scene::CertPending,
        Scene::WizardSelectMethod,
        Scene::WizardGenerateLocal,
        Scene::WizardSelectAgentKey,
        Scene::WizardError,
        Scene::UnlockPrompt,
        Scene::IdentityModal,
        Scene::SettingsConnection,
        Scene::SettingsDevices,
        Scene::SettingsVoice,
        Scene::SettingsProcessing,
        Scene::SettingsSounds,
        Scene::SettingsChat,
        Scene::SettingsFiles,
        Scene::SettingsStats,
    ];

    fn slug(self) -> &'static str {
        match self {
            Scene::Disconnected => "disconnected",
            Scene::DisconnectedEmpty => "disconnected_empty",
            Scene::AddServerModal => "add_server_modal",
            Scene::EditServerModal => "edit_server_modal",
            Scene::Connecting => "connecting",
            Scene::Connected => "connected",
            Scene::ConnectedImagePreview => "connected_image_preview",
            Scene::ImageLightbox => "image_lightbox",
            Scene::ImageLightboxZoomed => "image_lightbox_zoomed",
            Scene::ConnectionLost => "connection_lost",
            Scene::CertPending => "cert_pending",
            Scene::WizardSelectMethod => "wizard_select_method",
            Scene::WizardGenerateLocal => "wizard_generate_local",
            Scene::WizardSelectAgentKey => "wizard_select_agent_key",
            Scene::WizardError => "wizard_error",
            Scene::UnlockPrompt => "unlock_prompt",
            Scene::IdentityModal => "identity_modal",
            Scene::SettingsConnection => "settings_connection",
            Scene::SettingsDevices => "settings_devices",
            Scene::SettingsVoice => "settings_voice",
            Scene::SettingsProcessing => "settings_processing",
            Scene::SettingsSounds => "settings_sounds",
            Scene::SettingsChat => "settings_chat",
            Scene::SettingsFiles => "settings_files",
            Scene::SettingsStats => "settings_stats",
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
            | Scene::IdentityModal
            | Scene::SettingsConnection
            | Scene::SettingsChat
            | Scene::SettingsVoice
            | Scene::SettingsSounds
            | Scene::SettingsFiles => State::default(),
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
            Scene::SettingsStats => State {
                audio: stats_state(),
                ..State::default()
            },
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
                app.set_server_form_for_test(rumble_aetna::ServerForm {
                    editing_index: None,
                    addr: "rumble.new-org.example:5000".to_string(),
                    label: "New org".to_string(),
                    username: "alice".to_string(),
                    error: None,
                });
            }
            Scene::EditServerModal => {
                app.set_recent_servers_for_test(demo_recent_servers());
                app.set_server_form_for_test(rumble_aetna::ServerForm {
                    editing_index: Some(0),
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
            Scene::IdentityModal => {
                app.set_identity_modal_open_for_test(true);
            }
            Scene::SettingsConnection => app.open_settings_for_test(SettingsTab::Connection),
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
            Scene::SettingsFiles => app.open_settings_for_test(SettingsTab::Files),
            Scene::SettingsStats => app.open_settings_for_test(SettingsTab::Stats),
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
    };
    tweak(&mut u);
    u
}

fn make_chat(id: u8, sender: &str, text: &str, kind: ChatMessageKind) -> ChatMessage {
    make_chat_full(id, sender, text, kind, false, None)
}

fn make_chat_full(
    id: u8,
    sender: &str,
    text: &str,
    kind: ChatMessageKind,
    is_local: bool,
    attachment: Option<ChatAttachment>,
) -> ChatMessage {
    let mut bytes = [0u8; 16];
    bytes[15] = id;
    ChatMessage {
        id: bytes,
        sender: sender.into(),
        text: text.into(),
        timestamp: SystemTime::UNIX_EPOCH,
        is_local,
        kind,
        attachment,
    }
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
        chat_messages: vec![
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
                false,
                None,
            ),
            make_chat_full(
                5,
                "bob",
                "shared file: deploy_notes.md (12.3 KB)",
                ChatMessageKind::Room,
                false,
                Some(ChatAttachment::FileOffer(FileOfferInfo {
                    schema_version: 1,
                    transfer_id: "demo-offer".into(),
                    name: "deploy_notes.md".into(),
                    size: 12_345,
                    mime: "text/markdown".into(),
                    share_data: "demo".into(),
                })),
            ),
        ],
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
    // Flip VAD on so the level meter renders its threshold-break
    // colouring; otherwise the fixture would show the no-VAD fallback.
    if let Some(vad) = tx_pipeline.processors.iter_mut().find(|p| p.type_id == "builtin.vad") {
        vad.enabled = true;
    }
    AudioState {
        tx_pipeline,
        input_level_db: Some(-22.0),
        ..AudioState::default()
    }
}

fn stats_state() -> AudioState {
    let stats = AudioStats {
        actual_bitrate_bps: 64_000.0,
        avg_frame_size_bytes: 159.4,
        packets_sent: 12_804,
        packets_received: 12_731,
        packets_lost: 73,
        packets_recovered_fec: 41,
        frames_concealed: 14,
        playback_buffer_packets: 3,
        ..AudioStats::default()
    };
    AudioState {
        stats,
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
            last_used_unix: 0,
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

fn demo_pending_cert() -> PendingCertificate {
    let no_op_signer: rumble_client::SigningCallback =
        Arc::new(|_payload: &[u8]| Err("fixture identity is not signing".to_string()));
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
        signer: no_op_signer,
    }
}

// ---------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Match the real app's window viewport so layout matches what users see.
    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out");

    // Identity / SettingsStore both want a config dir to write to.
    // Fixtures don't actually mutate either, but the constructors
    // create files, so use a process-local scratch dir.
    let scratch = std::env::temp_dir().join("rumble_aetna_dump_bundles");
    std::fs::create_dir_all(&scratch)?;

    let requested: Vec<Scene> = std::env::args()
        .skip(1)
        .map(|raw| parse_scene(&raw).unwrap_or_else(|| panic!("unknown scene `{raw}`")))
        .collect();
    let scenes: Vec<Scene> = if requested.is_empty() {
        Scene::ALL.to_vec()
    } else {
        requested
    };

    for scene in scenes {
        let backend = MockBackend {
            state: scene.build_state(),
        };
        let identity = Identity::load(scratch.clone())?;
        let settings = SettingsStore::load_from_path(Some(scratch.join("settings.json")));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let mut app = RumbleApp::new(backend, identity, settings, runtime);
        // Bypass the first-run wizard for connection-state scenes — they
        // illustrate the main shell, not the wizard. Wizard / unlock
        // scenes drive the wizard explicitly and need the suppression
        // *not* applied.
        if !scene.keeps_first_run() {
            app.suppress_first_run_for_test();
        }
        scene.drive_setup(&mut app);

        let theme = app.theme();
        let cx = BuildCx::new(&theme);
        let mut tree = app.build(&cx);
        let bundle = render_bundle(&mut tree, viewport);

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

    Ok(())
}

fn parse_scene(raw: &str) -> Option<Scene> {
    Scene::ALL.iter().copied().find(|s| s.slug().eq_ignore_ascii_case(raw))
}
