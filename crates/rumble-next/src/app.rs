//! Top-level `eframe::App` — owns the backend handle, user identity,
//! paradigm choice, and connect form. Installs the right theme and
//! dispatches to either the connect view or the active paradigm.

use eframe::egui::{self, CentralPanel, Context, Layout, Modal, RichText, TopBottomPanel};
use rumble_client::{
    AudioSettings, Command, ConnectConfig, ProcessorRegistry, VoiceMode, build_default_tx_pipeline,
    handle::BackendHandle, merge_with_default_tx_pipeline, register_builtin_processors,
};
use rumble_widgets::{ButtonArgs, PressableRole, SurfaceFrame, SurfaceKind, UiExt, install_theme};

pub use crate::paradigm::Paradigm;
use crate::toasts::ToastManager;
use rumble_desktop_shell::{
    KeyInfo, SettingsStore,
    hotkeys::{HotkeyEvent, HotkeyManager},
    identity::key_manager::{PendingAgentOp, SshAgentClient, connect_and_list_keys, generate_and_add_to_agent},
};

use crate::{
    backend::{NativeUiBackend, UiBackend},
    connect_view::{self, ConnectForm},
    identity::{Identity, default_config_dir},
    paradigm,
    settings_panel::{self, SettingsState},
    shell::Shell,
};

pub struct App<B = NativeUiBackend> {
    pub paradigm: Paradigm,
    pub dark: bool,
    /// Last installed `(paradigm, dark)` pair — re-install the theme
    /// whenever either changes.
    installed: Option<(Paradigm, bool)>,
    pub shell: Shell,
    pub form: ConnectForm,
    pub identity: Identity,
    pub backend: B,
    /// Dispatch `Command::Connect` once, on the first frame. Set via
    /// `RUMBLE_NEXT_AUTOCONNECT=1`; lets us smoke-test connected UI
    /// headlessly.
    auto_connect_pending: bool,
    pub toasts: ToastManager,
    /// Tracks the previous connection state so we can emit a toast only
    /// when the state transitions (not every frame the state is stable).
    prev_connection: Option<ConnectionKind>,
    /// Last observed `self_muted` so we can play `Mute` / `Unmute` SFX
    /// on transitions — covers both UI button and global hotkey paths
    /// without double-firing.
    prev_self_muted: bool,
    /// User IDs we last saw in our current room. Diff against the next
    /// frame's set to detect joins/leaves and play the matching SFX.
    /// `prev_user_ids_initialized` suppresses sfx on the very first
    /// observation, so connecting into a populated room doesn't dump
    /// a flurry of join sounds.
    prev_user_ids_in_room: std::collections::HashSet<u64>,
    prev_user_ids_initialized: bool,
    /// Chat-message count at the previous frame. A bump means new
    /// messages arrived; we play `Message` once if any of them are
    /// from a remote user.
    prev_chat_count: usize,
    /// In-memory state of the settings panel (which category is open).
    pub settings_ui: SettingsState,
    /// Persisted user settings (paradigm, dark mode, recent servers,
    /// trusted certs). Saved synchronously on each mutation.
    pub settings: SettingsStore,
    /// Global hotkey service (PTT hold-to-talk, toggle mute, toggle
    /// deafen). Wired in step 3 of the rumble-next bringup.
    hotkeys: HotkeyManager,
    /// Tokio runtime kept alive for the duration of `App` so the
    /// XDG GlobalShortcuts portal listener keeps running. Held even
    /// on non-Wayland systems — the cost of an unused runtime is
    /// negligible compared to the conditional-construction churn.
    _runtime: tokio::runtime::Runtime,
    /// Processor registry used by the settings panel to enumerate
    /// available TX-pipeline stages and introspect their schemas.
    /// Built once at startup; the same factories the audio task uses
    /// internally, so what the UI shows matches what actually runs.
    pub processor_registry: ProcessorRegistry,
    first_run_state: FirstRunState,
    pending_agent_op: Option<PendingAgentOp>,
    unlock_state: UnlockState,
    /// Tokio runtime handle for spawning the async file picker. Cloned
    /// from `_runtime` at construction time.
    runtime_handle: tokio::runtime::Handle,
    /// In-flight file-share dialog. `Some` while the OS picker is open;
    /// the polling step in `update` consumes the result and dispatches
    /// `Command::ShareFile`.
    pending_file_dialog: Option<tokio::task::JoinHandle<Option<std::path::PathBuf>>>,
    /// `transfer_id`s we have already routed through the auto-download
    /// flow this session. Mid-session reconnects (or on-demand history
    /// replays) re-emit the same offer; without this guard each replay
    /// would kick off another download. Cleared on connection drop.
    auto_handled_offers: std::collections::HashSet<String>,
    /// Last room we observed ourselves in. Diff against `state.my_room_id`
    /// each frame to spot room joins and (optionally) request chat
    /// history for the new room.
    prev_my_room_id: Option<uuid::Uuid>,
}

#[derive(Debug, Clone, Default)]
enum FirstRunState {
    #[default]
    NotNeeded,
    SelectMethod,
    GenerateLocal {
        password: String,
        password_confirm: String,
        error: Option<String>,
    },
    ConnectingAgent,
    SelectAgentKey {
        keys: Vec<KeyInfo>,
        selected: Option<usize>,
        error: Option<String>,
    },
    GenerateAgentKey {
        comment: String,
    },
    Error {
        message: String,
    },
    Complete,
}

#[derive(Default)]
struct UnlockState {
    password: String,
    error: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum ConnectionKind {
    Disconnected,
    Connecting,
    Connected,
    Lost,
    CertPending,
}

impl ConnectionKind {
    fn from(state: &rumble_client::ConnectionState) -> Self {
        use rumble_client::ConnectionState as S;
        match state {
            S::Disconnected => Self::Disconnected,
            S::Connecting { .. } => Self::Connecting,
            S::Connected { .. } => Self::Connected,
            S::ConnectionLost { .. } => Self::Lost,
            S::CertificatePending { .. } => Self::CertPending,
        }
    }
}

struct InitialState {
    identity: Identity,
    first_run_state: FirstRunState,
    settings: SettingsStore,
    paradigm: Paradigm,
    dark: bool,
}

impl App<NativeUiBackend> {
    pub fn new(cc: &eframe::CreationContext<'_>) -> std::io::Result<Self> {
        let initial = Self::load_initial_state()?;
        let config = Self::connect_config_from_settings(&initial.settings);

        // Wire up egui_extras' image loaders so `egui::Image::new("file://…")`
        // can decode PNG/JPEG/etc. for inline previews and the lightbox.
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let ctx_for_repaint = cc.egui_ctx.clone();
        let signer = initial.identity.signer();
        let backend = BackendHandle::with_config(
            move || {
                ctx_for_repaint.request_repaint();
            },
            config,
            signer,
        );
        Self::from_initial_state(cc, NativeUiBackend::new(backend), initial)
    }

    fn connect_config_from_settings(settings: &SettingsStore) -> ConnectConfig {
        let mut config = ConnectConfig::new();
        // Convenience: trust dev certs checked into the repo so the
        // first-connect flow doesn't require hand-approval when running
        // against a local server.
        for candidate in ["dev-certs/server-cert.der", "certs/fullchain.pem"] {
            if std::path::Path::new(candidate).exists() {
                config = config.with_cert(candidate);
            }
        }
        if let Ok(cert_path) = std::env::var("RUMBLE_SERVER_CERT_PATH") {
            config = config.with_cert(cert_path);
        }
        // Re-trust certs the user has already accepted in past sessions.
        // Without this, every restart triggers another approval prompt
        // for self-signed servers — the gripe step 2 was meant to fix.
        for entry in &settings.settings().accepted_certificates {
            match entry.der_bytes() {
                Some(der) => config.accepted_certs.push(der),
                None => tracing::warn!(
                    "settings: accepted cert for {} has invalid base64 — ignored",
                    entry.server_name
                ),
            }
        }
        config
    }
}

impl<B: UiBackend> App<B> {
    fn load_initial_state() -> std::io::Result<InitialState> {
        let config_dir = default_config_dir();
        let identity = Identity::load(&config_dir)?;
        let first_run_state = if identity.needs_setup() {
            FirstRunState::SelectMethod
        } else {
            FirstRunState::NotNeeded
        };

        let settings = SettingsStore::load_from_path(Some(config_dir.join("desktop-shell.json")));
        let paradigm = settings
            .settings()
            .paradigm
            .as_deref()
            .and_then(Paradigm::from_persist_str)
            .unwrap_or(Paradigm::Modern);
        let dark = settings.settings().dark;

        Ok(InitialState {
            identity,
            first_run_state,
            settings,
            paradigm,
            dark,
        })
    }

    pub fn new_with_backend(cc: &eframe::CreationContext<'_>, backend: B) -> std::io::Result<Self> {
        let initial = Self::load_initial_state()?;
        // Install image loaders for the harness path too — kittest-driven
        // tests render the same chat image previews / lightbox.
        egui_extras::install_image_loaders(&cc.egui_ctx);
        Self::from_initial_state(cc, backend, initial)
    }

    fn from_initial_state(
        _cc: &eframe::CreationContext<'_>,
        backend: B,
        initial: InitialState,
    ) -> std::io::Result<Self> {
        let mut processor_registry = ProcessorRegistry::new();
        register_builtin_processors(&mut processor_registry);

        // Restore the user's audio choices on the backend before any
        // UI runs. Mirrors `rumble-egui::App::new`: without this, every
        // launch reverts to default encoder settings, system-default
        // input/output devices, PTT mode, and the stock TX pipeline —
        // even if the user changed them in a prior session.
        let persistent_audio = &initial.settings.settings().audio;
        backend.send(Command::UpdateAudioSettings {
            settings: AudioSettings::from(persistent_audio),
        });
        backend.send(Command::SetVoiceMode {
            mode: VoiceMode::from(initial.settings.settings().voice_mode),
        });
        let tx_pipeline_config = match &persistent_audio.tx_pipeline {
            Some(stored) => merge_with_default_tx_pipeline(stored, &processor_registry),
            None => build_default_tx_pipeline(&processor_registry),
        };
        backend.send(Command::UpdateTxPipeline {
            config: tx_pipeline_config,
        });
        if initial.settings.settings().input_device_id.is_some() {
            backend.send(Command::SetInputDevice {
                device_id: initial.settings.settings().input_device_id.clone(),
            });
        }
        if initial.settings.settings().output_device_id.is_some() {
            backend.send(Command::SetOutputDevice {
                device_id: initial.settings.settings().output_device_id.clone(),
            });
        }

        // Two ways to auto-connect at launch:
        //   1. The `RUMBLE_NEXT_AUTOCONNECT` env var still works for
        //      headless smoke runs (`examples/screenshot.rs`).
        //   2. The `auto_connect_addr` setting points at one of the
        //      saved servers — set via the connect view's "connect on
        //      launch" checkbox. This is the user-facing path.
        let auto_connect_pending = std::env::var("RUMBLE_NEXT_AUTOCONNECT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
            || initial.settings.settings().auto_connect_addr.is_some();

        // Tokio runtime for portal-backed hotkeys. A single worker is
        // enough — the portal listener spends almost all its time
        // awaiting D-Bus signals.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| std::io::Error::other(format!("tokio runtime: {e}")))?;

        let mut hotkeys = HotkeyManager::new();
        let runtime_handle = runtime.handle().clone();
        runtime.block_on(async {
            hotkeys.init_portal_backend(runtime_handle.clone()).await;
        });
        if let Err(e) = hotkeys.register_from_settings(&initial.settings.settings().keyboard) {
            tracing::warn!("hotkey registration failed: {e}");
        }

        Ok(Self {
            paradigm: initial.paradigm,
            dark: initial.dark,
            installed: None,
            shell: Shell::default(),
            form: ConnectForm::default(),
            identity: initial.identity,
            backend,
            auto_connect_pending,
            toasts: ToastManager::new(),
            prev_connection: None,
            prev_self_muted: false,
            prev_user_ids_in_room: std::collections::HashSet::new(),
            prev_user_ids_initialized: false,
            prev_chat_count: 0,
            settings_ui: SettingsState::default(),
            settings: initial.settings,
            hotkeys,
            _runtime: runtime,
            processor_registry,
            first_run_state: initial.first_run_state,
            pending_agent_op: None,
            unlock_state: UnlockState::default(),
            runtime_handle,
            pending_file_dialog: None,
            auto_handled_offers: std::collections::HashSet::new(),
            prev_my_room_id: None,
        })
    }
}

impl<B: UiBackend> eframe::App for App<B> {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        let want = (self.paradigm, self.dark);
        if self.installed != Some(want) {
            install_theme(ctx, self.paradigm.make_theme(self.dark));
            self.installed = Some(want);
        }

        // One state snapshot per frame. The backend calls our repaint
        // callback when something changes, so we get a fresh snapshot
        // exactly when needed.
        let state = self.backend.state();

        self.pump_auto_downloads(&state);
        self.pump_toasts(&state);
        self.pump_hotkeys(ctx, &state);
        self.pump_file_dialog();
        self.pump_pending_opens();
        self.pump_room_join(&state);

        // Refresh display preferences that the shell needs each frame.
        // Cheap copies; toggling the setting takes effect immediately.
        let chat = &self.settings.settings().chat;
        self.shell.chat_timestamp_format = chat.show_timestamps.then_some(chat.timestamp_format);

        // Auto-connect once if requested. Resolve the target server in
        // priority order:
        //   1. `auto_connect_addr` setting → look up its `RecentServer`
        //      and use its saved username.
        //   2. Otherwise (env-var smoke path), fall back to the form
        //      defaults (currently `[::1]:5000` / $USER).
        if self.auto_connect_pending
            && self.identity.public_key().is_some()
            && !self.identity.needs_unlock()
            && matches!(state.connection, rumble_client::ConnectionState::Disconnected)
        {
            let target = self
                .settings
                .settings()
                .auto_connect_addr
                .clone()
                .and_then(|addr| {
                    self.settings
                        .settings()
                        .recent_servers
                        .iter()
                        .find(|r| r.addr == addr)
                        .map(|r| (r.addr.clone(), r.username.clone()))
                })
                .unwrap_or_else(|| (self.form.editing.addr.clone(), self.form.editing.username.clone()));
            self.backend.send(Command::Connect {
                addr: target.0,
                name: target.1,
                public_key: self.identity.public_key().expect("checked above"),
                password: None,
            });
            self.auto_connect_pending = false;
        } else if self.auto_connect_pending && (self.identity.public_key().is_none() || self.identity.needs_unlock()) {
            // Keep the setting intact, but do not repeatedly try to
            // connect with an unusable signer while the modal is open.
            self.auto_connect_pending = false;
        }

        TopBottomPanel::top("rumble_next_paradigm_picker")
            .resizable(false)
            .show(ctx, |ui| {
                SurfaceFrame::new(SurfaceKind::Toolbar)
                    .inner_margin(egui::Margin::symmetric(10, 4))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let tokens = ui.theme().tokens().clone();
                            ui.label(
                                RichText::new("rumble-next · paradigm:")
                                    .color(tokens.text_muted)
                                    .font(tokens.font_label.clone()),
                            );
                            for p in Paradigm::ALL {
                                let active = *p == self.paradigm;
                                if ButtonArgs::new(p.label())
                                    .role(PressableRole::Accent)
                                    .active(active)
                                    .min_width(110.0)
                                    .show(ui)
                                    .clicked()
                                    && self.paradigm != *p
                                {
                                    self.paradigm = *p;
                                    self.settings.modify(|s| {
                                        s.paradigm = Some(p.as_persist_str().to_string());
                                    });
                                }
                            }

                            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                                // Connection summary on the far right.
                                let summary = crate::adapters::connection_summary(&state);
                                ui.label(
                                    RichText::new(summary)
                                        .color(tokens.text_muted)
                                        .font(tokens.font_mono.clone()),
                                );

                                ui.add_space(12.0);

                                // Dark-mode toggle. Sun glyph on light,
                                // moon on dark — click flips.
                                let (label, active) = if self.dark {
                                    ("🌙 dark", true)
                                } else {
                                    ("☀ light", false)
                                };
                                if ButtonArgs::new(label)
                                    .role(PressableRole::Ghost)
                                    .active(active)
                                    .min_width(72.0)
                                    .show(ui)
                                    .clicked()
                                {
                                    self.dark = !self.dark;
                                    let dark = self.dark;
                                    self.settings.modify(|s| s.dark = dark);
                                }
                            });
                        });
                    });
            });

        CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
            let tokens = ui.theme().tokens().clone();
            ui.painter()
                .rect_filled(ui.max_rect(), egui::CornerRadius::ZERO, tokens.surface);

            if !state.connection.is_connected() {
                connect_view::render(
                    ui,
                    &state,
                    &mut self.form,
                    &mut self.settings,
                    &self.identity,
                    &self.backend,
                );
                return;
            }

            match self.paradigm {
                Paradigm::Modern => paradigm::modern::render(ui, &mut self.shell, &state, &self.backend),
                Paradigm::MumbleClassic => paradigm::mumble::render(ui, &mut self.shell, &state, &self.backend),
                Paradigm::Luna => paradigm::luna::render(ui, &mut self.shell, &state, &self.backend),
            }
        });

        if state.connection.is_connected() {
            self.shell.render_overlays(ctx, &state, &self.backend);
        }

        self.render_first_run_dialog(ctx);
        self.render_unlock_dialog(ctx);

        let pub_hex = self.identity.public_key().map(hex::encode).unwrap_or_default();
        settings_panel::render(
            ctx,
            &mut self.settings_ui,
            &mut self.settings,
            &state,
            &self.backend,
            &self.processor_registry,
            &pub_hex,
            &mut self.shell,
        );

        self.toasts.render(ctx);
    }
}

impl<B: UiBackend> App<B> {
    fn render_first_run_dialog(&mut self, ctx: &Context) {
        if matches!(self.first_run_state, FirstRunState::NotNeeded | FirstRunState::Complete) {
            return;
        }

        let mut next_state = None;
        let mut generate_key_password: Option<Option<String>> = None;
        let mut select_agent_key: Option<KeyInfo> = None;
        let mut generate_agent_key_comment: Option<String> = None;

        Modal::new(egui::Id::new("rumble_next_first_run_modal")).show(ctx, |ui| {
            ui.set_min_width(420.0);
            match self.first_run_state.clone() {
                FirstRunState::SelectMethod => {
                    ui.heading("Set up your Rumble identity");
                    ui.add_space(8.0);
                    ui.label("Rumble uses an Ed25519 key as your server identity.");
                    ui.label("Choose where this client should keep that key.");
                    ui.add_space(14.0);

                    ui.group(|ui| {
                        ui.strong("Generate local key");
                        ui.label("Store a new key on this computer, optionally password-protected.");
                        ui.add_space(6.0);
                        if ui.button("Generate local key").clicked() {
                            next_state = Some(FirstRunState::GenerateLocal {
                                password: String::new(),
                                password_confirm: String::new(),
                                error: None,
                            });
                        }
                    });

                    ui.add_space(8.0);
                    ui.group(|ui| {
                        ui.strong("Use SSH agent");
                        ui.label("Use an Ed25519 key already available from ssh-agent.");
                        if !SshAgentClient::is_available() {
                            ui.colored_label(ui.theme().tokens().accent, "SSH_AUTH_SOCK is not set.");
                        }
                        ui.add_enabled_ui(SshAgentClient::is_available(), |ui| {
                            if ui.button("Choose SSH agent key").clicked() {
                                next_state = Some(FirstRunState::ConnectingAgent);
                            }
                        });
                    });
                }
                FirstRunState::GenerateLocal {
                    password,
                    password_confirm,
                    error,
                } => {
                    ui.heading("Generate local key");
                    ui.add_space(8.0);
                    ui.label("Leave the password blank for plaintext storage.");
                    ui.add_space(10.0);

                    let mut pw = password.clone();
                    let mut confirm = password_confirm.clone();
                    ui.horizontal(|ui| {
                        ui.label("Password");
                        ui.add(egui::TextEdit::singleline(&mut pw).password(true));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Confirm");
                        ui.add(egui::TextEdit::singleline(&mut confirm).password(true));
                    });
                    if pw != password || confirm != password_confirm {
                        next_state = Some(FirstRunState::GenerateLocal {
                            password: pw.clone(),
                            password_confirm: confirm.clone(),
                            error: error.clone(),
                        });
                    }
                    if let Some(error) = error {
                        ui.colored_label(ui.theme().tokens().danger, error);
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            next_state = Some(FirstRunState::SelectMethod);
                        }
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                            let can_generate = pw.is_empty() || pw == confirm;
                            if ui
                                .add_enabled(can_generate, egui::Button::new("Generate key"))
                                .on_disabled_hover_text("Passwords do not match")
                                .clicked()
                            {
                                generate_key_password = Some((!pw.is_empty()).then_some(pw.clone()));
                            }
                        });
                    });
                }
                FirstRunState::ConnectingAgent => {
                    ui.heading("Connecting to SSH agent");
                    ui.add_space(12.0);
                    ui.spinner();
                    ui.label("Fetching Ed25519 keys...");
                    ui.add_space(12.0);
                    if ui.button("Cancel").clicked() {
                        next_state = Some(FirstRunState::SelectMethod);
                    }
                }
                FirstRunState::SelectAgentKey { keys, selected, error } => {
                    ui.heading("Select SSH agent key");
                    ui.add_space(8.0);
                    let mut new_selected = selected;
                    if keys.is_empty() {
                        ui.label("No Ed25519 keys were found in the agent.");
                    } else {
                        for (i, key) in keys.iter().enumerate() {
                            let label = format!("{} ({})", key.comment, key.fingerprint);
                            if ui.selectable_label(new_selected == Some(i), label).clicked() {
                                new_selected = Some(i);
                            }
                        }
                    }
                    if new_selected != selected {
                        next_state = Some(FirstRunState::SelectAgentKey {
                            keys: keys.clone(),
                            selected: new_selected,
                            error: error.clone(),
                        });
                    }
                    if let Some(error) = error {
                        ui.colored_label(ui.theme().tokens().danger, error);
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            next_state = Some(FirstRunState::SelectMethod);
                        }
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Generate new agent key").clicked() {
                                next_state = Some(FirstRunState::GenerateAgentKey {
                                    comment: "rumble-identity".to_string(),
                                });
                            }
                            if ui
                                .add_enabled(new_selected.is_some(), egui::Button::new("Use selected key"))
                                .clicked()
                                && let Some(idx) = new_selected
                                && let Some(key) = keys.get(idx)
                            {
                                select_agent_key = Some(key.clone());
                            }
                        });
                    });
                }
                FirstRunState::GenerateAgentKey { comment } => {
                    ui.heading("Generate SSH agent key");
                    ui.add_space(8.0);
                    let mut next_comment = comment.clone();
                    ui.horizontal(|ui| {
                        ui.label("Comment");
                        ui.text_edit_singleline(&mut next_comment);
                    });
                    if next_comment != comment {
                        next_state = Some(FirstRunState::GenerateAgentKey {
                            comment: next_comment.clone(),
                        });
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            next_state = Some(FirstRunState::ConnectingAgent);
                        }
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.pending_agent_op.is_some() {
                                ui.spinner();
                                ui.label("Generating...");
                            } else if ui.button("Generate and add").clicked() {
                                generate_agent_key_comment = Some(next_comment.clone());
                            }
                        });
                    });
                }
                FirstRunState::Error { message } => {
                    ui.heading("Identity setup failed");
                    ui.colored_label(ui.theme().tokens().danger, message);
                    ui.add_space(12.0);
                    if ui.button("Back").clicked() {
                        next_state = Some(FirstRunState::SelectMethod);
                    }
                }
                FirstRunState::NotNeeded | FirstRunState::Complete => {}
            }
        });

        if let Some(new_state) = next_state {
            if matches!(new_state, FirstRunState::ConnectingAgent) && self.pending_agent_op.is_none() {
                let handle = self._runtime.handle().spawn(connect_and_list_keys());
                self.pending_agent_op = Some(PendingAgentOp::Connect(handle));
            }
            self.first_run_state = new_state;
        }

        self.poll_agent_op();

        if let Some(password) = generate_key_password {
            match self.identity.generate_local_key(password.as_deref()) {
                Ok(info) => {
                    self.first_run_state = FirstRunState::Complete;
                    self.toasts
                        .success(format!("Identity key generated: {}", info.fingerprint));
                }
                Err(e) => {
                    self.first_run_state = FirstRunState::GenerateLocal {
                        password: password.clone().unwrap_or_default(),
                        password_confirm: password.unwrap_or_default(),
                        error: Some(format!("Failed to generate key: {e}")),
                    };
                }
            }
        }

        if let Some(key_info) = select_agent_key {
            match self.identity.select_agent_key(&key_info) {
                Ok(()) => {
                    self.first_run_state = FirstRunState::Complete;
                    self.toasts
                        .success(format!("Using SSH agent key: {}", key_info.fingerprint));
                }
                Err(e) => {
                    self.first_run_state = FirstRunState::Error {
                        message: format!("Failed to save key config: {e}"),
                    };
                }
            }
        }

        if let Some(comment) = generate_agent_key_comment
            && self.pending_agent_op.is_none()
        {
            let handle = self._runtime.handle().spawn(generate_and_add_to_agent(comment));
            self.pending_agent_op = Some(PendingAgentOp::AddKey(handle));
        }
    }

    fn poll_agent_op(&mut self) {
        let Some(op) = &mut self.pending_agent_op else {
            return;
        };
        match op {
            PendingAgentOp::Connect(handle) if handle.is_finished() => {
                if let Some(PendingAgentOp::Connect(handle)) = self.pending_agent_op.take() {
                    match self._runtime.block_on(handle) {
                        Ok(Ok(keys)) => {
                            self.first_run_state = FirstRunState::SelectAgentKey {
                                keys,
                                selected: None,
                                error: None,
                            };
                        }
                        Ok(Err(e)) => {
                            self.first_run_state = FirstRunState::Error {
                                message: format!("Failed to connect to SSH agent: {e}"),
                            };
                        }
                        Err(e) => {
                            self.first_run_state = FirstRunState::Error {
                                message: format!("Agent operation failed: {e}"),
                            };
                        }
                    }
                }
            }
            PendingAgentOp::AddKey(handle) if handle.is_finished() => {
                if let Some(PendingAgentOp::AddKey(handle)) = self.pending_agent_op.take() {
                    match self._runtime.block_on(handle) {
                        Ok(Ok(key_info)) => match self.identity.select_agent_key(&key_info) {
                            Ok(()) => {
                                self.first_run_state = FirstRunState::Complete;
                                self.toasts
                                    .success(format!("Added SSH agent key: {}", key_info.fingerprint));
                            }
                            Err(e) => {
                                self.first_run_state = FirstRunState::Error {
                                    message: format!("Failed to save key config: {e}"),
                                };
                            }
                        },
                        Ok(Err(e)) => {
                            self.first_run_state = FirstRunState::Error {
                                message: format!("Failed to add key to agent: {e}"),
                            };
                        }
                        Err(e) => {
                            self.first_run_state = FirstRunState::Error {
                                message: format!("Agent operation failed: {e}"),
                            };
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn render_unlock_dialog(&mut self, ctx: &Context) {
        if !self.identity.needs_unlock() || !matches!(self.first_run_state, FirstRunState::NotNeeded) {
            return;
        }

        let mut unlock = false;
        Modal::new(egui::Id::new("rumble_next_unlock_identity_modal")).show(ctx, |ui| {
            ui.set_min_width(360.0);
            ui.heading("Unlock identity");
            ui.add_space(8.0);
            ui.label("Enter the password for your encrypted Rumble identity.");
            ui.add_space(10.0);
            let resp = ui.add(egui::TextEdit::singleline(&mut self.unlock_state.password).password(true));
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                unlock = true;
            }
            if let Some(error) = &self.unlock_state.error {
                ui.colored_label(ui.theme().tokens().danger, error);
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!self.unlock_state.password.is_empty(), egui::Button::new("Unlock"))
                    .clicked()
                {
                    unlock = true;
                }
            });
        });

        if unlock {
            match self.identity.unlock(&self.unlock_state.password) {
                Ok(()) => {
                    self.unlock_state = UnlockState::default();
                    self.toasts.success("Identity unlocked");
                }
                Err(e) => {
                    self.unlock_state.error = Some(e.to_string());
                }
            }
        }
    }

    /// Translate one-shot state signals into toast notifications:
    /// connection transitions, permission-denied messages, and kick
    /// reasons. `permission_denied` and `kicked` are `take()`-style
    /// fields on `State` — we drain them via `state_mut()` so a single
    /// event surfaces exactly once.
    fn pump_toasts(&mut self, state: &rumble_client::State) {
        let kind = ConnectionKind::from(&state.connection);
        if self.prev_connection != Some(kind) {
            if let Some(prev) = self.prev_connection {
                match (prev, kind) {
                    (_, ConnectionKind::Connected) => {
                        self.toasts.success("Connected to server");
                        self.play_sfx(rumble_client::SfxKind::Connect);
                    }
                    (ConnectionKind::Connected, ConnectionKind::Lost) => {
                        if let rumble_client::ConnectionState::ConnectionLost { error } = &state.connection {
                            self.toasts.error(format!("Connection lost: {error}"));
                            self.play_sfx(rumble_client::SfxKind::Disconnect);
                        }
                    }
                    (ConnectionKind::Connecting, ConnectionKind::Lost) => {
                        if let rumble_client::ConnectionState::ConnectionLost { error } = &state.connection {
                            self.toasts.error(format!("Could not connect: {error}"));
                        }
                    }
                    _ => {}
                }
            }
            // On any disconnect transition, drop the per-session caches
            // that don't survive a server change: auto-handled offer ids
            // (so reconnecting to a new server with overlapping ids still
            // gets a fresh evaluation) and the chat-count cursor (so the
            // cold-connect gate in `pump_auto_downloads` re-engages).
            if matches!(kind, ConnectionKind::Disconnected | ConnectionKind::Lost) {
                self.auto_handled_offers.clear();
                self.prev_chat_count = 0;
            }
            self.prev_connection = Some(kind);
        }

        // Drain one-shot fields. These are `Option<String>`; taking them
        // ensures we only fire the toast once.
        let (perm, kicked) = self
            .backend
            .update_state(|state| (state.permission_denied.take(), state.kicked.take()));
        if let Some(msg) = perm {
            self.toasts.error(format!("Permission denied: {msg}"));
        }
        if let Some(reason) = kicked {
            self.toasts.error(format!("You were kicked: {reason}"));
        }

        self.detect_sfx_events(state);
    }

    /// Diff the current state snapshot against the previous frame to
    /// fire one-shot SFX events: self-mute toggle, peer join/leave in
    /// the active room, and incoming chat messages. Mirrors the same
    /// flow in `rumble-egui::process_state` — sourcing all triggers
    /// from state diffs (rather than command sites) means UI buttons,
    /// hotkeys, and server-driven changes all surface the same sound
    /// without per-call-site instrumentation.
    fn detect_sfx_events(&mut self, state: &rumble_client::State) {
        let muted = state.audio.self_muted;
        if muted != self.prev_self_muted {
            self.play_sfx(if muted {
                rumble_client::SfxKind::Mute
            } else {
                rumble_client::SfxKind::Unmute
            });
            self.prev_self_muted = muted;
        }

        if state.connection.is_connected()
            && let Some(my_room_id) = state.my_room_id
        {
            let current: std::collections::HashSet<u64> = state
                .users_in_room(my_room_id)
                .iter()
                .filter_map(|u| u.user_id.as_ref().map(|id| id.value))
                .filter(|id| state.my_user_id != Some(*id))
                .collect();

            if self.prev_user_ids_initialized {
                if current.iter().any(|id| !self.prev_user_ids_in_room.contains(id)) {
                    self.play_sfx(rumble_client::SfxKind::UserJoin);
                }
                if self.prev_user_ids_in_room.iter().any(|id| !current.contains(id)) {
                    self.play_sfx(rumble_client::SfxKind::UserLeave);
                }
            }
            self.prev_user_ids_in_room = current;
            self.prev_user_ids_initialized = true;
        } else {
            self.prev_user_ids_in_room.clear();
            self.prev_user_ids_initialized = false;
        }

        let count = state.chat_messages.len();
        if count > self.prev_chat_count
            && self.prev_chat_count > 0
            && state.chat_messages[self.prev_chat_count..]
                .iter()
                .any(|m| !m.visibility.is_system())
        {
            self.play_sfx(rumble_client::SfxKind::Message);
        }
        self.prev_chat_count = count;
    }

    /// Auto-accept incoming `FileOffer` chat attachments that match the
    /// user's `FileTransferSettings` rules. Walks the same `[prev..len]`
    /// slice the SFX pump uses, so it benefits from the same first-
    /// frame gate (we don't replay history when connecting to a server
    /// that has a chat backlog).
    ///
    /// Skips offers we sent ourselves and offers the user has already
    /// clicked Download on. Each accepted offer is recorded in
    /// `Shell::accepted_offers` so the card flips to "Downloading…"
    /// without waiting for the (still-TODO) live transfers panel.
    fn pump_auto_downloads(&mut self, state: &rumble_client::State) {
        let prev = self.prev_chat_count;
        let count = state.chat_messages.len();
        // First-frame gate: don't auto-accept historical offers replayed
        // by the server when we first connect.
        if prev == 0 || count <= prev {
            return;
        }
        let settings = self.settings.settings().file_transfer.clone();
        let my_username = state
            .my_user_id
            .and_then(|id| state.get_user(id))
            .map(|u| u.username.clone());

        for msg in &state.chat_messages[prev..] {
            if msg.visibility.is_system() {
                continue;
            }
            let Some(att) = msg.attachment.as_ref() else {
                continue;
            };
            let Ok(offer) =
                <rumble_protocol::proto::RelayFileSharePayload as prost::Message>::decode(att.payload.as_slice())
            else {
                continue;
            };
            if my_username.as_deref() == Some(msg.sender.as_str()) {
                continue;
            }
            // Idempotency guard for history replay: a mid-session
            // reconnect (or `RequestChatHistory`) re-pushes the same
            // offers, but `prev_chat_count` is non-zero so the cold-
            // connect gate above doesn't catch them. Keying on the
            // offer's `transfer_id` makes replays no-ops.
            if !self.auto_handled_offers.insert(offer.transfer_id.clone()) {
                continue;
            }
            if settings.auto_download_enabled && settings.should_auto_download(&offer.mime, offer.size) {
                tracing::info!(
                    "auto-download: accepting offer {} ({} bytes, mime={})",
                    offer.name,
                    offer.size,
                    offer.mime
                );
                self.backend.send(Command::DownloadFile {
                    share_data: offer.share_data.clone(),
                });
                self.shell.mark_offer_accepted(offer.transfer_id.clone());
            } else {
                // No rule matched — surface a manual accept/deny
                // prompt. `auto_handled_offers` already contains the
                // id, so a history replay won't re-prompt even if the
                // user dismisses with Deny.
                self.shell.queue_offer_prompt(crate::shell::OfferPrompt {
                    transfer_id: offer.transfer_id.clone(),
                    name: offer.name.clone(),
                    size: offer.size,
                    mime: offer.mime.clone(),
                    share_data: offer.share_data.clone(),
                    from: msg.sender.clone(),
                });
            }
        }
    }

    /// Watch for `my_room_id` transitions and, when the user has opted
    /// into history sync, ask the room for its backlog. Triggers on
    /// every change (cold connect, manual room hop, server-driven move)
    /// because the user's intent — "always show me what was said
    /// before I arrived" — applies uniformly.
    fn pump_room_join(&mut self, state: &rumble_client::State) {
        if state.my_room_id == self.prev_my_room_id {
            return;
        }
        if state.my_room_id.is_some()
            && state.connection.is_connected()
            && self.settings.settings().chat.auto_sync_history
        {
            self.backend.send(Command::RequestChatHistory);
        }
        self.prev_my_room_id = state.my_room_id;
    }

    /// Bridge the composer's "share file" button to the OS file picker.
    /// Two phases:
    ///
    ///   1. If the shell raised `share_file_requested` and no dialog is
    ///      already in flight, spawn `rfd::AsyncFileDialog` on our
    ///      tokio runtime and stash the JoinHandle.
    ///   2. If a dialog is in flight and finished, drain the result and
    ///      dispatch `Command::ShareFile { path }` (or drop on cancel).
    ///
    /// The picker runs off the main thread because some platforms
    /// (notably KDE / portal-based dialogs) can take a noticeable
    /// fraction of a second to open, and blocking the audio render loop
    /// while it does is unacceptable.
    fn pump_file_dialog(&mut self) {
        if self.shell.share_file_requested {
            self.shell.share_file_requested = false;
            if self.pending_file_dialog.is_none() {
                let handle = self.runtime_handle.spawn(async {
                    rfd::AsyncFileDialog::new()
                        .pick_file()
                        .await
                        .map(|f| f.path().to_path_buf())
                });
                self.pending_file_dialog = Some(handle);
            }
        }

        if let Some(handle) = &self.pending_file_dialog
            && handle.is_finished()
            && let Some(handle) = self.pending_file_dialog.take()
        {
            match self.runtime_handle.block_on(handle) {
                Ok(Some(path)) => {
                    self.backend.send(Command::ShareFile { path });
                    self.toasts.success("File queued for sharing");
                    // Pop open the transfers window so the user
                    // immediately sees upload progress.
                    self.shell.transfers_open = true;
                }
                Ok(None) => {
                    // User dismissed the picker — no action required.
                }
                Err(e) => {
                    tracing::error!("file dialog task panicked: {e}");
                    self.toasts.error("File picker failed — see logs");
                }
            }
        }
    }

    /// Drain "Open" / "Show in folder" actions queued by the
    /// transfers window. `open` shells out to the platform handler
    /// (xdg-open / Finder / explorer) and can take a noticeable
    /// fraction of a second on cold cache, so we run it on our
    /// tokio runtime rather than blocking the render loop.
    fn pump_pending_opens(&mut self) {
        for action in std::mem::take(&mut self.shell.pending_open) {
            match action {
                crate::shell::PendingOpen::File(path) => {
                    self.runtime_handle.spawn_blocking(move || {
                        if let Err(e) = open::that(&path) {
                            tracing::warn!("open file {} failed: {e}", path.display());
                        }
                    });
                }
                crate::shell::PendingOpen::InFolder(path) => {
                    self.runtime_handle.spawn_blocking(move || {
                        let target = path.parent().unwrap_or(&path);
                        if let Err(e) = open::that(target) {
                            tracing::warn!("show in folder {} failed: {e}", target.display());
                        }
                    });
                }
                crate::shell::PendingOpen::SaveAs { src, suggested_name } => {
                    // Async pick + blocking copy keeps the UI responsive
                    // while the dialog is open, and matches how the share
                    // picker is plumbed (`pump_file_dialog`).
                    let handle = self.runtime_handle.spawn(async move {
                        let dest = rfd::AsyncFileDialog::new()
                            .set_file_name(&suggested_name)
                            .save_file()
                            .await
                            .map(|f| f.path().to_path_buf());
                        (src, dest)
                    });
                    let runtime = self.runtime_handle.clone();
                    self.runtime_handle.spawn(async move {
                        let Ok((src, dest)) = handle.await else { return };
                        let Some(dest) = dest else { return };
                        runtime.spawn_blocking(move || {
                            if let Err(e) = std::fs::copy(&src, &dest) {
                                tracing::warn!("save as: copy {} → {} failed: {e}", src.display(), dest.display());
                            }
                        });
                    });
                }
            }
        }
    }

    /// Drain hotkey events and turn them into backend commands.
    /// Mirrors `rumble-egui::handle_hotkey_event` so behaviour stays
    /// consistent across clients.
    ///
    /// Two sources of events:
    ///   1. Global hotkeys (X11 / Windows / macOS / Wayland portal)
    ///      via `HotkeyManager::poll_events()`.
    ///   2. Window-focused fallback — when the window has focus and
    ///      no global registration is active (e.g. Wayland without
    ///      portal), we synthesise PTT press/release from egui's
    ///      `key_pressed` / `key_released` so the user always has a
    ///      working PTT inside the window.
    ///
    /// Mute/deafen / PTT only act when connected — toggling state
    /// while disconnected would be confusing.
    fn pump_hotkeys(&mut self, ctx: &eframe::egui::Context, state: &rumble_client::State) {
        for event in self.hotkeys.poll_events() {
            self.dispatch_hotkey(event, state);
        }

        // Window-focused fallback. Iterate every configured shortcut
        // with a binding and dispatch on key press / release. Only
        // fires if global hotkeys aren't bound — otherwise we'd
        // double-dispatch on every keypress.
        if self.hotkeys.is_available() || self.hotkeys.has_portal_backend() {
            return;
        }
        for entry in self.settings.settings().keyboard.shortcuts.clone() {
            let Some(binding) = entry.binding.as_ref() else {
                continue;
            };
            let Some(key) = HotkeyManager::key_string_to_egui_key(&binding.key) else {
                continue;
            };
            let (pressed, released) = ctx.input(|i| (i.key_pressed(key), i.key_released(key)));
            if pressed {
                self.dispatch_hotkey(
                    HotkeyEvent::Pressed {
                        function: entry.function,
                        data: entry.data,
                    },
                    state,
                );
            }
            if released {
                self.dispatch_hotkey(
                    HotkeyEvent::Released {
                        function: entry.function,
                        data: entry.data,
                    },
                    state,
                );
            }
        }
    }

    fn dispatch_hotkey(&self, event: HotkeyEvent, state: &rumble_client::State) {
        use rumble_desktop_shell::{HotkeyData, HotkeyFunction};
        if !state.connection.is_connected() {
            return;
        }
        let server_muted = crate::adapters::am_i_server_muted(state);
        match event {
            HotkeyEvent::Pressed {
                function: HotkeyFunction::PushToTalk,
                data: HotkeyData::Hold,
            } => {
                if server_muted {
                    return;
                }
                self.backend.send(Command::StartTransmit);
            }
            HotkeyEvent::Released {
                function: HotkeyFunction::PushToTalk,
                data: HotkeyData::Hold,
            } => {
                self.backend.send(Command::StopTransmit);
            }
            HotkeyEvent::Pressed {
                function: HotkeyFunction::MuteSelf,
                data,
            } => {
                let muted = match data {
                    HotkeyData::Toggle => !state.audio.self_muted,
                    HotkeyData::On => true,
                    HotkeyData::Off => false,
                    HotkeyData::Hold => return,
                };
                self.backend.send(Command::SetMuted { muted });
            }
            HotkeyEvent::Pressed {
                function: HotkeyFunction::DeafenSelf,
                data,
            } => {
                let deafened = match data {
                    HotkeyData::Toggle => !state.audio.self_deafened,
                    HotkeyData::On => true,
                    HotkeyData::Off => false,
                    HotkeyData::Hold => return,
                };
                self.backend.send(Command::SetDeafened { deafened });
            }
            HotkeyEvent::Pressed { .. } | HotkeyEvent::Released { .. } => {}
        }
    }

    /// Dispatch a sound-effect playback command, gated by the user's
    /// SFX settings. Volume mixes the configured level with the
    /// per-event default (1.0 here — calls don't currently customise).
    fn play_sfx(&self, kind: rumble_client::SfxKind) {
        let sfx = &self.settings.settings().sfx;
        if !sfx.is_kind_enabled(kind) || sfx.volume <= 0.0 {
            return;
        }
        self.backend.send(Command::PlaySfx {
            kind,
            volume: sfx.volume.clamp(0.0, 1.0),
        });
    }
}

#[cfg(feature = "test-harness")]
impl<B: UiBackend> App<B> {
    pub fn suppress_first_run_for_test(&mut self) {
        self.first_run_state = FirstRunState::NotNeeded;
        self.unlock_state = UnlockState::default();
    }

    pub fn set_state_for_test(&mut self, state: rumble_protocol::State) {
        self.backend.update_state(|current| *current = state);
    }

    pub fn update_state_for_test<R>(&mut self, f: impl FnOnce(&mut rumble_protocol::State) -> R) -> R {
        self.backend.update_state(f)
    }
}
