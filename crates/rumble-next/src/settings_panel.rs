//! Settings overlay — a modal window reachable from each paradigm's
//! chrome. Organised as categories (Connection, Voice, Devices, About)
//! that read the live state and emit backend `Command`s on change.
//!
//! Kept small on purpose: this is the first-pass parity with
//! `rumble-egui`'s multi-page settings dialog. Categories that require
//! deeper UI (pipelines, ACL admin) can grow here incrementally.

use eframe::egui::{self, Align, Layout, Margin, RichText, Ui};
use rumble_client::{AudioSettings, Command, ConnectionState, PipelineConfig, ProcessorRegistry, State, VoiceMode};
use rumble_desktop_shell::{PersistentVoiceMode, SettingsStore, TimestampFormat};
use rumble_protocol::permissions::Permissions;
use rumble_widgets::{
    ButtonArgs, ComboBox, GroupBox, PressableRole, Radio, SurfaceFrame, SurfaceKind, TextRole, UiExt,
};

use crate::{backend::UiBackend, shell::Shell};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum SettingsCategory {
    #[default]
    Connection,
    Voice,
    Devices,
    Processing,
    Chat,
    Files,
    Statistics,
    Admin,
    About,
}

impl SettingsCategory {
    pub const ALL: &'static [SettingsCategory] = &[
        Self::Connection,
        Self::Voice,
        Self::Devices,
        Self::Processing,
        Self::Chat,
        Self::Files,
        Self::Statistics,
        Self::Admin,
        Self::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Connection => "Connection",
            Self::Voice => "Voice",
            Self::Devices => "Devices",
            Self::Processing => "Processing",
            Self::Chat => "Chat",
            Self::Files => "Files",
            Self::Statistics => "Statistics",
            Self::Admin => "Admin",
            Self::About => "About",
        }
    }
}

/// Per-category transient state (text inputs, currently-edited group,
/// etc). Only the Admin page needs anything beyond the active category;
/// keeping these here means `SettingsState` stays a single owned struct.
#[derive(Debug, Default)]
pub struct AdminPageState {
    pub editing_group: Option<String>,
    pub editing_permissions: u32,
    pub new_group_name: String,
    pub new_group_permissions: u32,
    /// Per-user dropdown selection (which group to add/remove from).
    /// Keyed by user_id; pruned on each render.
    pub user_group_pick: std::collections::HashMap<u64, String>,
}

#[derive(Debug, Default)]
pub struct SettingsState {
    pub category: SettingsCategory,
    pub admin: AdminPageState,
}

#[allow(clippy::too_many_arguments)]
pub fn render<B: UiBackend>(
    ctx: &egui::Context,
    settings: &mut SettingsState,
    store: &mut SettingsStore,
    state: &State,
    backend: &B,
    processor_registry: &ProcessorRegistry,
    identity_public_key_hex: &str,
    shell: &mut Shell,
) {
    if !shell.settings_open {
        return;
    }

    let mut should_close = false;

    egui::Window::new("Settings")
        .collapsible(false)
        .resizable(true)
        .default_size([640.0, 420.0])
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                sidebar(ui, settings);
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_min_width(420.0);
                    match settings.category {
                        SettingsCategory::Connection => connection_page(ui, state, backend, shell),
                        SettingsCategory::Voice => voice_page(ui, state, backend, store),
                        SettingsCategory::Devices => devices_page(ui, state, backend, store),
                        SettingsCategory::Processing => processing_page(ui, state, backend, processor_registry, store),
                        SettingsCategory::Chat => chat_page(ui, store),
                        SettingsCategory::Files => files_page(ui, store),
                        SettingsCategory::Statistics => statistics_page(ui, state, backend),
                        SettingsCategory::Admin => admin_page(ui, state, backend, &mut settings.admin),
                        SettingsCategory::About => about_page(ui, identity_public_key_hex),
                    }
                });
            });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ButtonArgs::new("Close")
                        .role(PressableRole::Primary)
                        .min_width(90.0)
                        .show(ui)
                        .clicked()
                    {
                        should_close = true;
                    }
                });
            });
        });

    if should_close {
        shell.settings_open = false;
    }
}

fn sidebar(ui: &mut Ui, settings: &mut SettingsState) {
    ui.vertical(|ui| {
        ui.set_min_width(140.0);
        for c in SettingsCategory::ALL {
            let active = settings.category == *c;
            if ButtonArgs::new(c.label())
                .role(PressableRole::Ghost)
                .active(active)
                .min_width(130.0)
                .show(ui)
                .clicked()
            {
                settings.category = *c;
            }
        }
    });
}

fn connection_page<B: UiBackend>(ui: &mut Ui, state: &State, backend: &B, shell: &mut Shell) {
    ui.label(
        RichText::new("Connection")
            .font(ui.theme().font(TextRole::Heading))
            .strong(),
    );
    ui.add_space(6.0);

    GroupBox::new("Status")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let tokens = ui.theme().tokens().clone();
            let line = match &state.connection {
                ConnectionState::Disconnected => "Not connected".to_string(),
                ConnectionState::Connecting { server_addr } => format!("Connecting to {server_addr}…"),
                ConnectionState::Connected { server_name, user_id } => {
                    format!("Connected to {server_name} as user #{user_id}")
                }
                ConnectionState::ConnectionLost { error } => format!("Lost: {error}"),
                ConnectionState::CertificatePending { cert_info } => {
                    format!("Cert pending · {}", cert_info.fingerprint_short())
                }
            };
            ui.label(RichText::new(line).color(tokens.text).font(tokens.font_body.clone()));
        });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let connected = state.connection.is_connected();
        if ButtonArgs::new("Disconnect")
            .role(PressableRole::Danger)
            .disabled(!connected)
            .min_width(120.0)
            .show(ui)
            .clicked()
        {
            backend.send(Command::Disconnect);
        }
    });

    if state.connection.is_connected() {
        ui.add_space(8.0);
        let is_elevated = my_user(state).map(|u| u.is_elevated).unwrap_or(false);
        GroupBox::new("Superuser")
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                if is_elevated {
                    ui.label(RichText::new("You are currently elevated to superuser.").color(ui.theme().tokens().text));
                } else {
                    ui.label(
                        RichText::new("Elevate to bypass ACL checks for the rest of this session.")
                            .color(ui.theme().tokens().text_muted),
                    );
                    ui.add_space(6.0);
                    if ButtonArgs::new("Elevate to superuser…")
                        .role(PressableRole::Default)
                        .min_width(180.0)
                        .show(ui)
                        .clicked()
                    {
                        shell.open_elevate_modal();
                    }
                }
            });
    }
}

/// Lookup the connected user's `User` record. `None` if not yet visible
/// in the user list (e.g. mid-handshake) or if not connected.
fn my_user(state: &State) -> Option<&rumble_protocol::proto::User> {
    let my_id = state.my_user_id?;
    state
        .users
        .iter()
        .find(|u| u.user_id.as_ref().map(|id| id.value) == Some(my_id))
}

fn voice_page<B: UiBackend>(ui: &mut Ui, state: &State, backend: &B, store: &mut SettingsStore) {
    ui.label(RichText::new("Voice").font(ui.theme().font(TextRole::Heading)).strong());
    ui.add_space(6.0);

    GroupBox::new("Activation mode")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let mut mode = state.audio.voice_mode;
            let before = mode;
            ui.vertical(|ui| {
                Radio::new(&mut mode, VoiceMode::PushToTalk, "Push-to-talk").show(ui);
                Radio::new(&mut mode, VoiceMode::Continuous, "Continuous / voice activity").show(ui);
            });
            if mode != before {
                backend.send(Command::SetVoiceMode { mode });
                let persist = PersistentVoiceMode::from(&mode);
                store.modify(|s| s.voice_mode = persist);
            }
        });

    ui.add_space(8.0);

    encoder_group(ui, state, backend, store);

    ui.add_space(8.0);

    GroupBox::new("Self")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let muted = state.audio.self_muted;
            let deafened = state.audio.self_deafened;
            ui.horizontal(|ui| {
                if ButtonArgs::new(if muted { "Unmute microphone" } else { "Mute microphone" })
                    .role(PressableRole::Default)
                    .active(muted)
                    .show(ui)
                    .clicked()
                {
                    backend.send(Command::SetMuted { muted: !muted });
                }
                if ButtonArgs::new(if deafened { "Undeafen" } else { "Deafen" })
                    .role(PressableRole::Danger)
                    .active(deafened)
                    .show(ui)
                    .clicked()
                {
                    backend.send(Command::SetDeafened { deafened: !deafened });
                }
            });
        });

    ui.add_space(8.0);

    GroupBox::new("Sound effects")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let mut enabled = store.settings().sfx.enabled;
            if ui.checkbox(&mut enabled, "Enable sound effects").changed() {
                store.modify(|s| s.sfx.enabled = enabled);
            }
            ui.add_space(4.0);
            ui.add_enabled_ui(enabled, |ui| {
                let mut volume = store.settings().sfx.volume;
                let before = volume;
                ui.label(
                    RichText::new("Volume")
                        .color(ui.theme().tokens().text_muted)
                        .font(ui.theme().font(TextRole::Label)),
                );
                rumble_widgets::Slider::new(&mut volume, 0.0..=1.0).show(ui);
                if (volume - before).abs() > 0.001 {
                    store.modify(|s| s.sfx.volume = volume.clamp(0.0, 1.0));
                }

                ui.add_space(8.0);
                ui.label(
                    RichText::new("Per-event")
                        .color(ui.theme().tokens().text_muted)
                        .font(ui.theme().font(TextRole::Label)),
                );
                for kind in rumble_client::SfxKind::all().iter().copied() {
                    ui.horizontal(|ui| {
                        let mut on = !store.settings().sfx.disabled_sounds.contains(&kind);
                        if ui.checkbox(&mut on, kind.label()).changed() {
                            store.modify(|s| {
                                if on {
                                    s.sfx.disabled_sounds.remove(&kind);
                                } else {
                                    s.sfx.disabled_sounds.insert(kind);
                                }
                            });
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ButtonArgs::new("Preview").role(PressableRole::Ghost).show(ui).clicked() {
                                let v = store.settings().sfx.volume.clamp(0.0, 1.0);
                                backend.send(Command::PlaySfx { kind, volume: v });
                            }
                        });
                    });
                }
            });
        });
}

/// Opus encoder + jitter buffer controls. Mirrors `rumble-egui`'s
/// "Audio Quality" group: bitrate, complexity, jitter buffer, FEC, and
/// expected packet-loss percentage. Each change dispatches
/// `Command::UpdateAudioSettings` and persists to the shell settings
/// store so the next launch starts at the same place.
fn encoder_group<B: UiBackend>(ui: &mut Ui, state: &State, backend: &B, store: &mut SettingsStore) {
    GroupBox::new("Audio quality (encoder)")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let original = state.audio.settings.clone();
            let mut working = original.clone();

            ui.horizontal(|ui| {
                ui.label("Bitrate (kbps)");
                let mut kbps = working.bitrate / 1000;
                if ui.add(egui::Slider::new(&mut kbps, 6..=128)).changed() {
                    working.bitrate = kbps * 1000;
                }
            });
            ui.label(
                RichText::new(working.bitrate_label())
                    .color(ui.theme().tokens().text_muted)
                    .font(ui.theme().font(TextRole::Label)),
            );

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Complexity");
                ui.add(egui::Slider::new(&mut working.encoder_complexity, 0..=10))
                    .on_hover_text("0 = lowest CPU, 10 = best quality");
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Jitter buffer (packets)");
                ui.add(egui::Slider::new(&mut working.jitter_buffer_delay_packets, 1..=10))
                    .on_hover_text("Packets buffered before playback. Higher = smoother under jitter, more latency.");
            });

            ui.add_space(4.0);
            ui.checkbox(&mut working.fec_enabled, "Forward error correction (FEC)")
                .on_hover_text("Adds redundancy so receivers can reconstruct lost packets.");

            ui.add_space(4.0);
            ui.add_enabled_ui(working.fec_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Expected loss (%)");
                    ui.add(egui::Slider::new(&mut working.packet_loss_percent, 0..=50))
                        .on_hover_text("Tunes FEC redundancy for the network you actually have.");
                });
            });

            if working != original {
                backend.send(Command::UpdateAudioSettings {
                    settings: working.clone(),
                });
                let persist = persistent_audio_from(&working, store.settings().audio.tx_pipeline.clone());
                store.modify(|s| s.audio = persist);
            }
        });
}

/// Build a `PersistentAudioSettings` from current `AudioSettings`,
/// preserving the existing `tx_pipeline` so encoder edits don't wipe
/// the user's processor config.
fn persistent_audio_from(
    settings: &AudioSettings,
    tx_pipeline: Option<PipelineConfig>,
) -> rumble_desktop_shell::PersistentAudioSettings {
    rumble_desktop_shell::PersistentAudioSettings {
        bitrate: settings.bitrate,
        encoder_complexity: settings.encoder_complexity,
        jitter_buffer_delay_packets: settings.jitter_buffer_delay_packets,
        fec_enabled: settings.fec_enabled,
        packet_loss_percent: settings.packet_loss_percent,
        tx_pipeline,
    }
}

fn devices_page<B: UiBackend>(ui: &mut Ui, state: &State, backend: &B, store: &mut SettingsStore) {
    ui.label(
        RichText::new("Audio devices")
            .font(ui.theme().font(TextRole::Heading))
            .strong(),
    );
    ui.add_space(6.0);

    device_picker(
        ui,
        "Input (microphone)",
        "input_device",
        &state.audio.input_devices,
        state.audio.selected_input.as_deref(),
        |new_id| {
            backend.send(Command::SetInputDevice {
                device_id: new_id.clone(),
            });
            store.modify(|s| s.input_device_id = new_id);
        },
    );
    ui.add_space(8.0);
    device_picker(
        ui,
        "Output (speakers)",
        "output_device",
        &state.audio.output_devices,
        state.audio.selected_output.as_deref(),
        |new_id| {
            backend.send(Command::SetOutputDevice {
                device_id: new_id.clone(),
            });
            store.modify(|s| s.output_device_id = new_id);
        },
    );

    ui.add_space(10.0);
    if ButtonArgs::new("Refresh device list")
        .role(PressableRole::Default)
        .show(ui)
        .clicked()
    {
        backend.send(Command::RefreshAudioDevices);
    }
}

fn device_picker(
    ui: &mut Ui,
    title: &str,
    id_salt: &str,
    devices: &[rumble_client::AudioDeviceInfo],
    current_id: Option<&str>,
    on_change: impl FnOnce(Option<String>),
) {
    GroupBox::new(title)
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let mut labels: Vec<String> = Vec::with_capacity(devices.len() + 1);
            labels.push("System default".to_string());
            for d in devices {
                labels.push(device_label(d));
            }
            let mut selected_idx = match current_id {
                None => 0,
                Some(id) => devices.iter().position(|d| d.id == id).map(|i| i + 1).unwrap_or(0),
            };
            let before = selected_idx;
            ComboBox::new(id_salt, &mut selected_idx, labels).width(420.0).show(ui);
            if selected_idx != before {
                let new_id = if selected_idx == 0 {
                    None
                } else {
                    devices.get(selected_idx - 1).map(|d| d.id.clone())
                };
                on_change(new_id);
            }
        });
}

/// Format a device entry for display: `name [pipeline] (default)`.
///
/// The pipeline tag (e.g. `pipewire`, `pulse`, `front:CARD=...`) is the
/// only way to tell apart entries that share a `name` — ALSA happily
/// hands us the same physical card under several pcm endpoints — so we
/// always show it when present and not redundant with the name.
fn device_label(d: &rumble_client::AudioDeviceInfo) -> String {
    let mut s = d.name.clone();
    if let Some(pipeline) = &d.pipeline
        && pipeline != &d.name
    {
        s.push_str(&format!("  [{pipeline}]"));
    }
    if d.is_default {
        s.push_str(" (default)");
    }
    s
}

/// TX-pipeline editor. Renders the live `state.audio.tx_pipeline`,
/// dispatches `Command::UpdateTxPipeline` whenever the user changes a
/// processor's enabled flag, a schema-driven parameter, or the order /
/// presence of a stage. The next frame's snapshot reflects the change,
/// so the UI stays in lock-step with what the audio task is actually
/// running — no "pending" buffer.
fn processing_page<B: UiBackend>(
    ui: &mut Ui,
    state: &State,
    backend: &B,
    registry: &ProcessorRegistry,
    store: &mut SettingsStore,
) {
    ui.label(
        RichText::new("Audio processing")
            .font(ui.theme().font(TextRole::Heading))
            .strong(),
    );
    ui.add_space(4.0);
    ui.label(
        RichText::new("Pipeline applied before encoding. Order matters: each stage feeds the next.")
            .color(ui.theme().tokens().text_muted)
            .font(ui.theme().font(TextRole::Label)),
    );
    ui.add_space(8.0);

    // Edit a clone — if anything changed we send the whole pipeline
    // back to the audio task. Cloning a config with three processors
    // is trivial; not worth a per-field diff.
    let original = state.audio.tx_pipeline.clone();
    let mut working = original.clone();
    let mut dirty = false;

    if working.processors.is_empty() {
        ui.label(RichText::new("No processors. Add a stage below.").color(ui.theme().tokens().text_muted));
    }

    let info: std::collections::HashMap<String, (String, String)> = registry
        .list_available()
        .into_iter()
        .map(|(id, name, desc)| (id.to_string(), (name.to_string(), desc.to_string())))
        .collect();

    // Structural ops are deferred until after the iteration so we don't
    // mutate `working.processors` while iterating it. At most one of
    // these is set per frame — the first matching click wins.
    let mut move_up: Option<usize> = None;
    let mut move_down: Option<usize> = None;
    let mut remove_at: Option<usize> = None;

    let count = working.processors.len();
    for (i, proc) in working.processors.iter_mut().enumerate() {
        let (display_name, description) = info
            .get(&proc.type_id)
            .cloned()
            .unwrap_or_else(|| (proc.type_id.clone(), "Unknown processor".into()));

        GroupBox::new(display_name.clone())
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut enabled = proc.enabled;
                    if ui
                        .checkbox(&mut enabled, "Enabled")
                        .on_hover_text(description)
                        .changed()
                    {
                        proc.enabled = enabled;
                        dirty = true;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ButtonArgs::new("✖")
                            .role(PressableRole::Ghost)
                            .show(ui)
                            .on_hover_text("Remove this stage")
                            .clicked()
                        {
                            remove_at = Some(i);
                        }
                        if ButtonArgs::new("⏷")
                            .role(PressableRole::Ghost)
                            .disabled(i + 1 >= count)
                            .show(ui)
                            .on_hover_text("Move down")
                            .clicked()
                        {
                            move_down = Some(i);
                        }
                        if ButtonArgs::new("⏶")
                            .role(PressableRole::Ghost)
                            .disabled(i == 0)
                            .show(ui)
                            .on_hover_text("Move up")
                            .clicked()
                        {
                            move_up = Some(i);
                        }
                    });
                });

                if !proc.enabled {
                    return;
                }

                let Some(schema) = registry.settings_schema(&proc.type_id) else {
                    ui.label(
                        RichText::new("(no settings)")
                            .color(ui.theme().tokens().text_muted)
                            .font(ui.theme().font(TextRole::Label)),
                    );
                    return;
                };
                let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
                    return;
                };
                if properties.is_empty() {
                    return;
                }
                ui.add_space(4.0);
                for (key, prop_schema) in properties {
                    if render_schema_field(ui, key, prop_schema, &mut proc.settings) {
                        dirty = true;
                    }
                }
            });
        ui.add_space(6.0);
    }

    if let Some(i) = move_up {
        working.processors.swap(i - 1, i);
        dirty = true;
    } else if let Some(i) = move_down {
        working.processors.swap(i, i + 1);
        dirty = true;
    } else if let Some(i) = remove_at {
        working.processors.remove(i);
        dirty = true;
    }

    add_stage_picker(ui, &mut working, registry, &info, &mut dirty);

    if dirty {
        backend.send(Command::UpdateTxPipeline {
            config: working.clone(),
        });
        store.modify(|s| s.audio.tx_pipeline = Some(working));
    }

    ui.add_space(8.0);
    GroupBox::new("Input level")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            input_level_meter(ui, state, &original);
        });
}

/// "Add a stage" picker rendered below the existing processor list.
/// Lists every registered processor type — duplicates are allowed,
/// since a user can legitimately want the same processor at two
/// points in the chain (e.g. gain pre- and post-denoise). New stages
/// land at the end of the pipeline; the user can reorder afterwards.
fn add_stage_picker(
    ui: &mut Ui,
    working: &mut PipelineConfig,
    registry: &ProcessorRegistry,
    info: &std::collections::HashMap<String, (String, String)>,
    dirty: &mut bool,
) {
    GroupBox::new("Add stage")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let mut available: Vec<(String, String)> = registry
                .list_available()
                .into_iter()
                .map(|(id, _, _)| {
                    let label = info
                        .get(id)
                        .map(|(name, _)| name.clone())
                        .unwrap_or_else(|| id.to_string());
                    (id.to_string(), label)
                })
                .collect();
            available.sort_by(|a, b| a.1.cmp(&b.1));

            if available.is_empty() {
                ui.label(RichText::new("No processors registered.").color(ui.theme().tokens().text_muted));
                return;
            }

            // ComboBox needs an `selected_idx` against a Vec<String>.
            // Index 0 is a sentinel "Choose…" entry so the picker
            // doesn't pre-select a real type and mislead the user.
            let mut labels: Vec<String> = Vec::with_capacity(available.len() + 1);
            labels.push("Choose…".to_string());
            labels.extend(available.iter().map(|(_, label)| label.clone()));
            let mut idx = 0usize;
            ui.horizontal(|ui| {
                ComboBox::new("add_stage_picker", &mut idx, labels)
                    .width(280.0)
                    .show(ui);
                if idx > 0
                    && let Some((type_id, _)) = available.get(idx - 1)
                    && let Some(config) = registry.default_config(type_id)
                {
                    working.processors.push(config);
                    *dirty = true;
                }
            });
        });
}

/// Render one JSON-schema property as a settings field. Mirrors the
/// dispatch in `rumble-egui::render_schema_field` — same `type` →
/// widget mapping (number → Slider, integer → Slider, boolean →
/// checkbox, string → text input), so behaviour is consistent across
/// clients.
fn render_schema_field(
    ui: &mut Ui,
    key: &str,
    prop_schema: &serde_json::Value,
    settings: &mut serde_json::Value,
) -> bool {
    let title = prop_schema.get("title").and_then(|t| t.as_str()).unwrap_or(key);
    let description = prop_schema.get("description").and_then(|d| d.as_str()).unwrap_or("");
    let prop_type = prop_schema.get("type").and_then(|t| t.as_str()).unwrap_or("string");

    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(format!("{title}:"));
        match prop_type {
            "number" => {
                let default = prop_schema.get("default").and_then(|d| d.as_f64()).unwrap_or(0.0) as f32;
                let min = prop_schema.get("minimum").and_then(|m| m.as_f64()).unwrap_or(-100.0) as f32;
                let max = prop_schema.get("maximum").and_then(|m| m.as_f64()).unwrap_or(100.0) as f32;
                let mut value = settings
                    .get(key)
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or(default);
                let resp = ui
                    .add(egui::Slider::new(&mut value, min..=max))
                    .on_hover_text(description);
                if resp.changed() {
                    settings[key] = serde_json::json!(value);
                    changed = true;
                }
            }
            "integer" => {
                let default = prop_schema.get("default").and_then(|d| d.as_i64()).unwrap_or(0) as i32;
                let min = prop_schema.get("minimum").and_then(|m| m.as_i64()).unwrap_or(0) as i32;
                let max = prop_schema.get("maximum").and_then(|m| m.as_i64()).unwrap_or(1000) as i32;
                let mut value = settings
                    .get(key)
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32)
                    .unwrap_or(default);
                let resp = ui
                    .add(egui::Slider::new(&mut value, min..=max))
                    .on_hover_text(description);
                if resp.changed() {
                    settings[key] = serde_json::json!(value);
                    changed = true;
                }
            }
            "boolean" => {
                let default = prop_schema.get("default").and_then(|d| d.as_bool()).unwrap_or(false);
                let mut value = settings.get(key).and_then(|v| v.as_bool()).unwrap_or(default);
                if ui.checkbox(&mut value, "").on_hover_text(description).changed() {
                    settings[key] = serde_json::json!(value);
                    changed = true;
                }
            }
            _ => {
                let default = prop_schema.get("default").and_then(|d| d.as_str()).unwrap_or("");
                let mut value = settings
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or(default)
                    .to_string();
                if ui.text_edit_singleline(&mut value).on_hover_text(description).changed() {
                    settings[key] = serde_json::json!(value);
                    changed = true;
                }
            }
        }
    });
    changed
}

/// Input-level bar with a vertical line at the current VAD threshold.
/// Helps the user calibrate VAD: the line sits where the gate opens,
/// the coloured bar shows what the mic is picking up right now.
fn input_level_meter(ui: &mut Ui, state: &State, pipeline: &PipelineConfig) {
    let level_db = state.audio.input_level_db;
    let vad_threshold = pipeline
        .processors
        .iter()
        .find(|p| p.type_id == "builtin.vad" && p.enabled)
        .and_then(|p| p.settings.get("threshold_db"))
        .and_then(|v| v.as_f64())
        .map(|t| t as f32);

    let Some(level_db) = level_db else {
        ui.label(RichText::new("— (no input)").color(ui.theme().tokens().text_muted));
        return;
    };

    ui.horizontal(|ui| {
        let normalized = ((level_db + 60.0) / 60.0).clamp(0.0, 1.0);
        let color = if level_db > -3.0 {
            egui::Color32::from_rgb(0xF4, 0x43, 0x36)
        } else if level_db > -12.0 {
            egui::Color32::from_rgb(0xFF, 0x98, 0x00)
        } else {
            egui::Color32::from_rgb(0x4C, 0xAF, 0x50)
        };
        let (rect, _) = ui.allocate_exact_size(egui::vec2(220.0, 16.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 2.0, egui::Color32::DARK_GRAY);
        let filled = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * normalized, rect.height()));
        ui.painter().rect_filled(filled, 2.0, color);
        if let Some(threshold) = vad_threshold {
            let n = ((threshold + 60.0) / 60.0).clamp(0.0, 1.0);
            let x = rect.min.x + rect.width() * n;
            ui.painter().line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                egui::Stroke::new(2.0, egui::Color32::WHITE),
            );
        }
        ui.label(format!("{level_db:.1} dB"));
    });
}

fn chat_page(ui: &mut Ui, store: &mut SettingsStore) {
    ui.label(RichText::new("Chat").font(ui.theme().font(TextRole::Heading)).strong());
    ui.add_space(6.0);

    GroupBox::new("Timestamps")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let mut show = store.settings().chat.show_timestamps;
            if ui.checkbox(&mut show, "Show timestamps next to messages").changed() {
                store.modify(|s| s.chat.show_timestamps = show);
            }

            ui.add_space(4.0);
            ui.add_enabled_ui(show, |ui| {
                let current = store.settings().chat.timestamp_format;
                let labels: Vec<String> = TimestampFormat::ALL.iter().map(|f| f.label().to_string()).collect();
                let mut idx = TimestampFormat::ALL.iter().position(|f| *f == current).unwrap_or(0);
                let before = idx;
                ComboBox::new("chat_timestamp_format", &mut idx, labels)
                    .width(280.0)
                    .show(ui);
                if idx != before
                    && let Some(&fmt) = TimestampFormat::ALL.get(idx)
                {
                    store.modify(|s| s.chat.timestamp_format = fmt);
                }
            });
        });

    ui.add_space(8.0);
    GroupBox::new("History")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let mut auto = store.settings().chat.auto_sync_history;
            if ui
                .checkbox(&mut auto, "Request peer chat history when joining a room")
                .changed()
            {
                store.modify(|s| s.chat.auto_sync_history = auto);
            }
            ui.label(
                RichText::new("Use the ⟳ sync button next to the composer for one-shot history requests.")
                    .color(ui.theme().tokens().text_muted)
                    .font(ui.theme().font(TextRole::Label)),
            );
        });
}

/// Files settings — auto-download rules. Bandwidth caps and the
/// seed/cleanup-on-exit flags exist in `FileTransferSettings` (so a
/// settings file written by `rumble-egui` round-trips cleanly) but the
/// relay plugin doesn't enforce them today, so they're not exposed
/// here. Re-add them once the plugin actually honours the values.
fn files_page(ui: &mut Ui, store: &mut SettingsStore) {
    ui.label(RichText::new("Files").font(ui.theme().font(TextRole::Heading)).strong());
    ui.add_space(6.0);

    GroupBox::new("Auto-download")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let mut enabled = store.settings().file_transfer.auto_download_enabled;
            if ui.checkbox(&mut enabled, "Pull matching files automatically").changed() {
                store.modify(|s| s.file_transfer.auto_download_enabled = enabled);
            }
            ui.add_space(4.0);
            ui.add_enabled_ui(enabled, |ui| {
                ui.label(
                    RichText::new("Rules (MIME pattern + max size). 0 MB disables a rule.")
                        .color(ui.theme().tokens().text_muted)
                        .font(ui.theme().font(TextRole::Label)),
                );
                ui.add_space(4.0);

                // Mutate a working copy so we can call `store.modify`
                // once at the end with the resolved Vec, instead of
                // saving on every keystroke in the MIME field.
                let mut rules = store.settings().file_transfer.auto_download_rules.clone();
                let mut dirty = false;
                let mut to_remove: Option<usize> = None;
                for (i, rule) in rules.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut rule.mime_pattern)
                                .desired_width(180.0)
                                .hint_text("image/*"),
                        );
                        if resp.lost_focus() {
                            dirty = true;
                        }
                        let mut size_mb = (rule.max_size_bytes / (1024 * 1024)) as u32;
                        let before = size_mb;
                        ui.add(egui::Slider::new(&mut size_mb, 0..=512).suffix(" MB"));
                        if size_mb != before {
                            rule.max_size_bytes = u64::from(size_mb) * 1024 * 1024;
                            dirty = true;
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ButtonArgs::new("✖")
                                .role(PressableRole::Ghost)
                                .show(ui)
                                .on_hover_text("Remove rule")
                                .clicked()
                            {
                                to_remove = Some(i);
                            }
                        });
                    });
                }
                if let Some(idx) = to_remove {
                    rules.remove(idx);
                    dirty = true;
                }
                if ButtonArgs::new("+ Add rule")
                    .role(PressableRole::Default)
                    .show(ui)
                    .clicked()
                {
                    rules.push(rumble_desktop_shell::AutoDownloadRule {
                        mime_pattern: String::new(),
                        max_size_bytes: 10 * 1024 * 1024,
                    });
                    dirty = true;
                }
                if dirty {
                    store.modify(|s| s.file_transfer.auto_download_rules = rules);
                }
            });
        });
}

fn statistics_page<B: UiBackend>(ui: &mut Ui, state: &State, backend: &B) {
    ui.label(
        RichText::new("Statistics")
            .font(ui.theme().font(TextRole::Heading))
            .strong(),
    );
    ui.add_space(6.0);

    let audio = &state.audio;
    let stats = &audio.stats;
    GroupBox::new("Audio")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            stat_row(
                ui,
                "Input level (dB)",
                &audio
                    .input_level_db
                    .map(|v| format!("{v:.1}"))
                    .unwrap_or_else(|| "—".into()),
            );
            stat_row(ui, "Transmitting", &audio.is_transmitting.to_string());
            stat_row(ui, "Self muted", &audio.self_muted.to_string());
            stat_row(ui, "Self deafened", &audio.self_deafened.to_string());
        });

    ui.add_space(8.0);
    GroupBox::new("Codec / network")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            stat_row(
                ui,
                "Actual bitrate",
                &format!("{:.1} kbps", stats.actual_bitrate_bps / 1000.0),
            );
            stat_row(ui, "Avg frame size", &format!("{:.1} B", stats.avg_frame_size_bytes));
            stat_row(ui, "Packets sent", &stats.packets_sent.to_string());
            stat_row(ui, "Packets received", &stats.packets_received.to_string());
            packet_loss_row(ui, stats.packet_loss_percent(), stats.packets_lost);
            stat_row(ui, "FEC recovered", &stats.packets_recovered_fec.to_string());
            stat_row(ui, "Frames concealed", &stats.frames_concealed.to_string());
            stat_row(
                ui,
                "Buffer level",
                &format!("{} packets", stats.playback_buffer_packets),
            );
            stat_row(ui, "Buffer underruns", &stats.buffer_underruns.to_string());
        });

    ui.add_space(6.0);
    if ButtonArgs::new("Reset statistics")
        .role(PressableRole::Default)
        .show(ui)
        .clicked()
    {
        backend.send(Command::ResetAudioStats);
    }

    ui.add_space(8.0);
    GroupBox::new("Connection")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let summary = match &state.connection {
                ConnectionState::Connected { server_name, .. } => format!("connected · {server_name}"),
                ConnectionState::Connecting { server_addr } => format!("connecting to {server_addr}"),
                ConnectionState::Disconnected => "disconnected".to_string(),
                ConnectionState::ConnectionLost { error } => format!("lost: {error}"),
                ConnectionState::CertificatePending { .. } => "awaiting certificate approval".to_string(),
            };
            stat_row(ui, "State", &summary);
            stat_row(
                ui,
                "Users in room",
                &crate::adapters::peers_in_current_room(state).to_string(),
            );
        });
}

/// Packet-loss row coloured by severity — green/amber/red matches the
/// thresholds rumble-egui uses, so the visual signal is consistent
/// across clients even if the metric is the same number.
fn packet_loss_row(ui: &mut Ui, loss_pct: f32, lost: u64) {
    ui.horizontal(|ui| {
        let tokens = ui.theme().tokens().clone();
        ui.label(
            RichText::new("Packet loss")
                .color(tokens.text_muted)
                .font(tokens.font_label.clone()),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let color = if loss_pct > 5.0 {
                egui::Color32::from_rgb(0xF4, 0x43, 0x36)
            } else if loss_pct > 1.0 {
                egui::Color32::from_rgb(0xFF, 0x98, 0x00)
            } else {
                egui::Color32::from_rgb(0x4C, 0xAF, 0x50)
            };
            ui.colored_label(
                color,
                RichText::new(format!("{loss_pct:.1}% ({lost} lost)")).font(tokens.font_mono.clone()),
            );
        });
    });
}

fn stat_row(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        let tokens = ui.theme().tokens().clone();
        ui.label(
            RichText::new(label)
                .color(tokens.text_muted)
                .font(tokens.font_label.clone()),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).font(tokens.font_mono.clone()));
        });
    });
}

/// Admin page: server-wide group management plus per-user group
/// membership editing. Mirrors `rumble-egui::render_settings_admin`.
/// Backend Commands: `CreateGroup`, `ModifyGroup`, `DeleteGroup`, and
/// `SetUserGroup`. The page is always rendered (even without
/// `MANAGE_ACL`) — the server rejects mutations without permission and
/// the rejection toast is the right user-facing signal.
fn admin_page<B: UiBackend>(ui: &mut Ui, state: &State, backend: &B, admin: &mut AdminPageState) {
    ui.label(RichText::new("Admin").font(ui.theme().font(TextRole::Heading)).strong());
    ui.add_space(6.0);

    if !state.connection.is_connected() {
        ui.label(RichText::new("Connect to a server to manage groups and ACLs.").color(ui.theme().tokens().text_muted));
        return;
    }

    // Drop dropdown selections for users that aren't in the roster
    // anymore (disconnect, server-side delete). Keeping stale entries
    // is harmless but the map grows without bound.
    let active: std::collections::HashSet<u64> = state
        .users
        .iter()
        .filter_map(|u| u.user_id.as_ref().map(|id| id.value))
        .collect();
    admin.user_group_pick.retain(|uid, _| active.contains(uid));

    let groups = state.group_definitions.clone();
    let group_names: Vec<String> = groups.iter().map(|g| g.name.clone()).collect();

    GroupBox::new("Groups")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            for group in &groups {
                let perms = Permissions::from_bits_truncate(group.permissions);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&group.name).strong().color(ui.theme().tokens().text));
                    ui.label(
                        RichText::new(format_permission_summary(perms))
                            .color(ui.theme().tokens().text_muted)
                            .font(ui.theme().font(TextRole::Label)),
                    )
                    .on_hover_text(format_permission_details(perms));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let builtin = group.is_builtin;
                        if !builtin && ButtonArgs::new("Delete").role(PressableRole::Danger).show(ui).clicked() {
                            backend.send(Command::DeleteGroup {
                                name: group.name.clone(),
                            });
                        }
                        if ButtonArgs::new("Edit").role(PressableRole::Default).show(ui).clicked() {
                            admin.editing_group = Some(group.name.clone());
                            admin.editing_permissions = group.permissions;
                        }
                        if builtin {
                            ui.label(
                                RichText::new("(built-in)")
                                    .color(ui.theme().tokens().text_muted)
                                    .font(ui.theme().font(TextRole::Label)),
                            );
                        }
                    });
                });
                ui.add_space(2.0);
            }
        });

    if let Some(name) = admin.editing_group.clone() {
        ui.add_space(8.0);
        GroupBox::new(format!("Edit: {name}"))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                permission_checkboxes(ui, &mut admin.editing_permissions, "edit_group");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ButtonArgs::new("Save").role(PressableRole::Primary).show(ui).clicked() {
                        backend.send(Command::ModifyGroup {
                            name: name.clone(),
                            permissions: admin.editing_permissions,
                        });
                        admin.editing_group = None;
                    }
                    if ButtonArgs::new("Cancel")
                        .role(PressableRole::Default)
                        .show(ui)
                        .clicked()
                    {
                        admin.editing_group = None;
                    }
                });
            });
    }

    ui.add_space(8.0);
    GroupBox::new("Create group")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut admin.new_group_name).desired_width(200.0));
            });
            ui.add_space(4.0);
            permission_checkboxes(ui, &mut admin.new_group_permissions, "new_group");
            ui.add_space(6.0);
            let trimmed = admin.new_group_name.trim().to_string();
            if ButtonArgs::new("+ Create")
                .role(PressableRole::Primary)
                .disabled(trimmed.is_empty())
                .show(ui)
                .clicked()
            {
                backend.send(Command::CreateGroup {
                    name: trimmed,
                    permissions: admin.new_group_permissions,
                });
                admin.new_group_name.clear();
                admin.new_group_permissions = 0;
            }
        });

    ui.add_space(8.0);
    GroupBox::new("User groups")
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            if state.users.is_empty() {
                ui.label(RichText::new("No users connected.").color(ui.theme().tokens().text_muted));
                return;
            }
            for user in &state.users {
                let user_id = match user.user_id.as_ref().map(|id| id.value) {
                    Some(id) => id,
                    None => continue,
                };
                ui.horizontal(|ui| {
                    let label = if user.groups.is_empty() {
                        format!("{}: (none)", user.username)
                    } else {
                        format!("{}: {}", user.username, user.groups.join(", "))
                    };
                    ui.label(RichText::new(label).color(ui.theme().tokens().text));
                });
                ui.horizontal(|ui| {
                    let pick = admin
                        .user_group_pick
                        .entry(user_id)
                        .or_insert_with(|| group_names.first().cloned().unwrap_or_default());
                    let mut idx = group_names.iter().position(|g| g == pick).unwrap_or(0);
                    if !group_names.is_empty() {
                        ComboBox::new(format!("user_group_pick_{user_id}"), &mut idx, group_names.clone())
                            .width(180.0)
                            .show(ui);
                        if let Some(name) = group_names.get(idx) {
                            *pick = name.clone();
                        }
                    }
                    let group = pick.clone();
                    let already_member = user.groups.contains(&group);
                    let label = if already_member { "− Remove" } else { "+ Add" };
                    if !group.is_empty() && ButtonArgs::new(label).role(PressableRole::Default).show(ui).clicked() {
                        backend.send(Command::SetUserGroup {
                            target_user_id: user_id,
                            group,
                            add: !already_member,
                            expires_at: 0,
                        });
                    }
                });
                ui.add_space(4.0);
            }
        });
}

/// Render a wrapping grid of permission checkboxes (room-scoped first,
/// server-scoped second). `id_prefix` namespaces widget IDs so two
/// instances of the grid in the same frame don't collide.
fn permission_checkboxes(ui: &mut Ui, bits: &mut u32, id_prefix: &str) {
    let room_perms: &[(Permissions, &str)] = &[
        (Permissions::TRAVERSE, "Traverse"),
        (Permissions::ENTER, "Enter"),
        (Permissions::SPEAK, "Speak"),
        (Permissions::TEXT_MESSAGE, "Text"),
        (Permissions::SHARE_FILE, "Files"),
        (Permissions::MUTE_DEAFEN, "Mute/Deafen"),
        (Permissions::MOVE_USER, "Move user"),
        (Permissions::MAKE_ROOM, "Make room"),
        (Permissions::MODIFY_ROOM, "Modify room"),
        (Permissions::WRITE, "Edit ACL"),
    ];
    let server_perms: &[(Permissions, &str)] = &[
        (Permissions::KICK, "Kick"),
        (Permissions::BAN, "Ban"),
        (Permissions::REGISTER, "Register"),
        (Permissions::SELF_REGISTER, "Self-register"),
        (Permissions::MANAGE_ACL, "Manage ACL"),
        (Permissions::SUDO, "Sudo"),
    ];
    ui.label(
        RichText::new("Room-scoped")
            .color(ui.theme().tokens().text_muted)
            .font(ui.theme().font(TextRole::Label)),
    );
    permission_grid(ui, bits, room_perms, id_prefix, "room");
    ui.add_space(4.0);
    ui.label(
        RichText::new("Server-scoped")
            .color(ui.theme().tokens().text_muted)
            .font(ui.theme().font(TextRole::Label)),
    );
    permission_grid(ui, bits, server_perms, id_prefix, "server");
}

fn permission_grid(ui: &mut Ui, bits: &mut u32, perms: &[(Permissions, &str)], id_prefix: &str, scope: &str) {
    ui.horizontal_wrapped(|ui| {
        for (perm, label) in perms {
            let mut on = Permissions::from_bits_truncate(*bits).contains(*perm);
            let _ = ui.push_id((id_prefix, scope, *label), |ui| {
                if ui.checkbox(&mut on, *label).changed() {
                    if on {
                        *bits |= perm.bits();
                    } else {
                        *bits &= !perm.bits();
                    }
                }
            });
        }
    });
}

/// Brief permission summary used in the Admin groups list. Lists the
/// granted bits for short permission sets, otherwise falls back to a
/// count to keep the row short.
fn format_permission_summary(perms: Permissions) -> String {
    let parts = permission_names(perms);
    if parts.is_empty() {
        "none".to_string()
    } else if parts.len() <= 4 {
        parts.join(", ")
    } else {
        format!("{} permissions", parts.len())
    }
}

/// Full permission detail used as the row tooltip — shows every
/// granted bit so the user can audit groups without opening the editor.
fn format_permission_details(perms: Permissions) -> String {
    let parts = permission_names(perms);
    if parts.is_empty() {
        "No permissions".into()
    } else {
        parts.join(", ")
    }
}

fn permission_names(perms: Permissions) -> Vec<&'static str> {
    const ALL: &[(Permissions, &str)] = &[
        (Permissions::TRAVERSE, "Traverse"),
        (Permissions::ENTER, "Enter"),
        (Permissions::SPEAK, "Speak"),
        (Permissions::TEXT_MESSAGE, "Text"),
        (Permissions::SHARE_FILE, "Files"),
        (Permissions::MUTE_DEAFEN, "Mute/Deafen"),
        (Permissions::MOVE_USER, "Move"),
        (Permissions::MAKE_ROOM, "Make room"),
        (Permissions::MODIFY_ROOM, "Modify room"),
        (Permissions::WRITE, "Edit ACL"),
        (Permissions::KICK, "Kick"),
        (Permissions::BAN, "Ban"),
        (Permissions::REGISTER, "Register"),
        (Permissions::SELF_REGISTER, "Self-register"),
        (Permissions::MANAGE_ACL, "Manage ACL"),
        (Permissions::SUDO, "Sudo"),
    ];
    ALL.iter()
        .filter(|(flag, _)| perms.contains(*flag))
        .map(|(_, name)| *name)
        .collect()
}

fn about_page(ui: &mut Ui, public_key_hex: &str) {
    ui.label(RichText::new("About").font(ui.theme().font(TextRole::Heading)).strong());
    ui.add_space(6.0);

    SurfaceFrame::new(SurfaceKind::Panel)
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let tokens = ui.theme().tokens().clone();
            ui.label(RichText::new("rumble-next").color(tokens.text).strong());
            ui.label(
                RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                    .color(tokens.text_muted)
                    .font(tokens.font_mono.clone()),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("Your public key")
                    .color(tokens.text_muted)
                    .font(tokens.font_label.clone()),
            );
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if public_key_hex.is_empty() {
                        "(not set up)"
                    } else {
                        public_key_hex
                    })
                    .color(tokens.text)
                    .font(tokens.font_mono.clone()),
                );
                if ButtonArgs::new("Copy")
                    .role(PressableRole::Default)
                    .disabled(public_key_hex.is_empty())
                    .show(ui)
                    .clicked()
                {
                    match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(public_key_hex)) {
                        Ok(()) => {}
                        Err(e) => tracing::warn!("clipboard: failed to copy public key: {e}"),
                    }
                }
            });
        });
}
