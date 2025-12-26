use backend::{AudioSettings, BackendHandle, Command, ConnectionState, ConnectConfig, VoiceMode};
use clap::Parser;
use directories::ProjectDirs;
use eframe::egui;
use egui::{CollapsingHeader, Modal};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

/// Rumble - A voice chat client
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Server address to connect to (e.g., 127.0.0.1:5000)
    #[arg(short, long)]
    server: Option<String>,

    /// Username for the connection
    #[arg(short, long)]
    name: Option<String>,

    /// Password for the server (optional)
    #[arg(short, long)]
    password: Option<String>,

    /// Trust the development certificate (dev-certs/server-cert.der)
    #[arg(long, default_value_t = true)]
    trust_dev_cert: bool,

    /// Path to a custom server certificate to trust
    #[arg(long)]
    cert: Option<String>,
}

// =============================================================================
// Persistent Settings
// =============================================================================

/// Persistent settings saved to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistentSettings {
    // Connection settings
    pub server_address: String,
    pub server_password: String,
    pub trust_dev_cert: bool,
    pub custom_cert_path: Option<String>,
    pub client_name: String,
    
    // Autoconnect
    pub autoconnect_on_launch: bool,
    
    // Audio settings
    pub audio: PersistentAudioSettings,
    
    // Voice mode
    pub voice_mode: PersistentVoiceMode,
    
    // Selected devices (by ID)
    pub input_device_id: Option<String>,
    pub output_device_id: Option<String>,
}

impl Default for PersistentSettings {
    fn default() -> Self {
        Self {
            server_address: String::new(),
            server_password: String::new(),
            trust_dev_cert: true,
            custom_cert_path: None,
            client_name: format!("user-{}", Uuid::new_v4().simple()),
            autoconnect_on_launch: false,
            audio: PersistentAudioSettings::default(),
            voice_mode: PersistentVoiceMode::PushToTalk,
            input_device_id: None,
            output_device_id: None,
        }
    }
}

/// Serializable audio settings (mirrors backend::AudioSettings).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistentAudioSettings {
    pub denoise_enabled: bool,
    pub bitrate: i32,
    pub encoder_complexity: i32,
    pub jitter_buffer_delay_packets: u32,
    pub fec_enabled: bool,
    pub packet_loss_percent: i32,
}

impl Default for PersistentAudioSettings {
    fn default() -> Self {
        Self {
            denoise_enabled: true,
            bitrate: 64000,
            encoder_complexity: 10,
            jitter_buffer_delay_packets: 3,
            fec_enabled: true,
            packet_loss_percent: 5,
        }
    }
}

impl From<&AudioSettings> for PersistentAudioSettings {
    fn from(s: &AudioSettings) -> Self {
        Self {
            denoise_enabled: s.denoise_enabled,
            bitrate: s.bitrate,
            encoder_complexity: s.encoder_complexity,
            jitter_buffer_delay_packets: s.jitter_buffer_delay_packets,
            fec_enabled: s.fec_enabled,
            packet_loss_percent: s.packet_loss_percent,
        }
    }
}

impl From<&PersistentAudioSettings> for AudioSettings {
    fn from(s: &PersistentAudioSettings) -> Self {
        Self {
            denoise_enabled: s.denoise_enabled,
            bitrate: s.bitrate,
            encoder_complexity: s.encoder_complexity,
            jitter_buffer_delay_packets: s.jitter_buffer_delay_packets,
            fec_enabled: s.fec_enabled,
            packet_loss_percent: s.packet_loss_percent,
        }
    }
}

/// Serializable voice mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum PersistentVoiceMode {
    PushToTalk,
    Continuous,
}

impl Default for PersistentVoiceMode {
    fn default() -> Self {
        Self::PushToTalk
    }
}

impl From<&VoiceMode> for PersistentVoiceMode {
    fn from(m: &VoiceMode) -> Self {
        match m {
            VoiceMode::PushToTalk => Self::PushToTalk,
            VoiceMode::Continuous => Self::Continuous,
        }
    }
}

impl From<PersistentVoiceMode> for VoiceMode {
    fn from(m: PersistentVoiceMode) -> Self {
        match m {
            PersistentVoiceMode::PushToTalk => VoiceMode::PushToTalk,
            PersistentVoiceMode::Continuous => VoiceMode::Continuous,
        }
    }
}

impl PersistentSettings {
    /// Get the config directory path.
    fn config_dir() -> Option<PathBuf> {
        ProjectDirs::from("com", "rumble", "Rumble").map(|dirs| dirs.config_dir().to_path_buf())
    }

    /// Get the settings file path.
    fn settings_path() -> Option<PathBuf> {
        Self::config_dir().map(|dir| dir.join("settings.json"))
    }

    /// Load settings from disk, or return defaults if not found.
    pub fn load() -> Self {
        let Some(path) = Self::settings_path() else {
            log::warn!("Could not determine config directory, using defaults");
            return Self::default();
        };

        match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(settings) => {
                    log::info!("Loaded settings from {}", path.display());
                    settings
                }
                Err(e) => {
                    log::warn!("Failed to parse settings file: {}, using defaults", e);
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("No settings file found, using defaults");
                Self::default()
            }
            Err(e) => {
                log::warn!("Failed to read settings file: {}, using defaults", e);
                Self::default()
            }
        }
    }

    /// Save settings to disk.
    pub fn save(&self) -> Result<(), String> {
        let Some(dir) = Self::config_dir() else {
            return Err("Could not determine config directory".to_string());
        };

        // Create config directory if it doesn't exist
        if let Err(e) = fs::create_dir_all(&dir) {
            return Err(format!("Failed to create config directory: {}", e));
        }

        let path = dir.join("settings.json");
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize settings: {}", e))?;

        fs::write(&path, contents).map_err(|e| format!("Failed to write settings file: {}", e))?;

        log::info!("Saved settings to {}", path.display());
        Ok(())
    }
}

fn main() -> eframe::Result<()> {
    env_logger::init();
    let args = Args::parse();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Rumble",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(MyApp::new(cc.egui_ctx.clone(), args)))
        }),
    )
}

/// State for the rename room modal
#[derive(Default)]
struct RenameModalState {
    open: bool,
    room_uuid: Option<Uuid>,
    room_name: String,
}

/// State for the settings modal with pending changes
#[derive(Default)]
struct SettingsModalState {
    /// Pending audio settings (initialized when modal opens)
    pending_settings: Option<AudioSettings>,
    /// Pending input device selection
    pending_input_device: Option<Option<String>>,
    /// Pending output device selection  
    pending_output_device: Option<Option<String>>,
    /// Pending voice mode
    pending_voice_mode: Option<VoiceMode>,
    /// Pending autoconnect setting
    pending_autoconnect: Option<bool>,
    /// Pending username
    pending_username: Option<String>,
    /// Whether any settings have been modified
    dirty: bool,
}

struct MyApp {
    // UI state
    show_connect: bool,
    show_settings: bool,
    connect_address: String,
    connect_password: String,
    trust_dev_cert: bool,
    chat_messages: Vec<String>,
    chat_input: String,
    client_name: String,
    
    // Persistent settings
    persistent_settings: PersistentSettings,
    /// Whether to autoconnect on launch
    autoconnect_on_launch: bool,

    // Backend handle (manages connection, audio, and state)
    backend: BackendHandle,

    // Rename modal state
    rename_modal: RenameModalState,

    // Settings modal state with pending changes
    settings_modal: SettingsModalState,

    /// Push-to-talk key is held
    push_to_talk_active: bool,

    egui_ctx: egui::Context,
}

impl Drop for MyApp {
    fn drop(&mut self) {
        // Send disconnect command before dropping the backend handle
        let state = self.backend.state();
        if state.connection.is_connected() {
            self.backend.send(Command::Disconnect);
        }
        // BackendHandle will clean up the background thread and audio when dropped
    }
}

impl MyApp {
    fn new(ctx: egui::Context, args: Args) -> Self {
        // Load persistent settings
        let mut persistent_settings = PersistentSettings::load();
        
        // CLI args override persistent settings
        let server_address = args.server.clone().unwrap_or_else(|| persistent_settings.server_address.clone());
        let server_password = args.password.clone().unwrap_or_else(|| persistent_settings.server_password.clone());
        let client_name = args.name.clone().unwrap_or_else(|| {
            if persistent_settings.client_name.is_empty() {
                format!("user-{}", Uuid::new_v4().simple())
            } else {
                persistent_settings.client_name.clone()
            }
        });
        let trust_dev_cert = args.trust_dev_cert || persistent_settings.trust_dev_cert;
        
        // Build connect config based on CLI args and persistent settings
        let mut config = ConnectConfig::new();
        if trust_dev_cert {
            config = config.with_cert("dev-certs/server-cert.der");
        }
        if let Some(cert_path) = &args.cert {
            config = config.with_cert(cert_path);
        } else if let Some(cert_path) = &persistent_settings.custom_cert_path {
            config = config.with_cert(cert_path);
        }
        if let Ok(cert_path) = std::env::var("RUMBLE_SERVER_CERT_PATH") {
            config = config.with_cert(cert_path);
        }

        // Create backend handle with repaint callback
        let ctx_for_repaint = ctx.clone();
        let backend = BackendHandle::with_config(
            move || {
                ctx_for_repaint.request_repaint();
            },
            config,
        );
        
        // Apply persistent audio settings to backend
        let audio_settings: AudioSettings = (&persistent_settings.audio).into();
        backend.send(Command::UpdateAudioSettings { settings: audio_settings });
        
        // Apply voice mode
        backend.send(Command::SetVoiceMode { mode: persistent_settings.voice_mode.into() });
        
        // Apply device selections if set
        if persistent_settings.input_device_id.is_some() {
            backend.send(Command::SetInputDevice { device_id: persistent_settings.input_device_id.clone() });
        }
        if persistent_settings.output_device_id.is_some() {
            backend.send(Command::SetOutputDevice { device_id: persistent_settings.output_device_id.clone() });
        }

        let mut chat_messages = Vec::new();
        chat_messages.push(format!(
            "Rumble Client v{}",
            env!("CARGO_PKG_VERSION")
        ));
        chat_messages.push(format!("Client name: {}", client_name));
        
        // Update persistent settings with current values (in case CLI overrode them)
        persistent_settings.server_address = server_address.clone();
        persistent_settings.server_password = server_password.clone();
        persistent_settings.client_name = client_name.clone();
        persistent_settings.trust_dev_cert = trust_dev_cert;
        
        let autoconnect_on_launch = persistent_settings.autoconnect_on_launch;

        let mut app = Self {
            show_connect: false,
            show_settings: false,
            connect_address: server_address,
            connect_password: server_password,
            trust_dev_cert,
            chat_messages,
            chat_input: String::new(),
            client_name,
            persistent_settings,
            autoconnect_on_launch,
            backend,
            rename_modal: RenameModalState::default(),
            settings_modal: SettingsModalState::default(),
            push_to_talk_active: false,
            egui_ctx: ctx,
        };

        // If server was specified on command line, connect immediately
        // Otherwise, check autoconnect setting
        if args.server.is_some() {
            app.connect();
        } else if autoconnect_on_launch && !app.connect_address.is_empty() {
            app.chat_messages.push("Auto-connecting...".to_string());
            app.connect();
        }

        app
    }

    /// Connect to the server using current settings.
    fn connect(&mut self) {
        let addr = if self.connect_address.trim().is_empty() {
            "127.0.0.1:5000".to_string()
        } else {
            self.connect_address.trim().to_string()
        };
        let name = self.client_name.clone();
        let password = if self.connect_password.trim().is_empty() {
            None
        } else {
            Some(self.connect_password.trim().to_string())
        };

        self.chat_messages.push(format!("Connecting to {}...", addr));
        self.backend.send(Command::Connect {
            addr,
            name,
            password,
        });
    }
    
    /// Save current settings to persistent storage.
    fn save_settings(&mut self) {
        // Update persistent settings from current state
        self.persistent_settings.server_address = self.connect_address.clone();
        self.persistent_settings.server_password = self.connect_password.clone();
        self.persistent_settings.client_name = self.client_name.clone();
        self.persistent_settings.trust_dev_cert = self.trust_dev_cert;
        self.persistent_settings.autoconnect_on_launch = self.autoconnect_on_launch;
        
        // Save audio settings from backend state
        let audio = self.backend.state().audio.clone();
        self.persistent_settings.audio = (&audio.settings).into();
        self.persistent_settings.voice_mode = (&audio.voice_mode).into();
        self.persistent_settings.input_device_id = audio.selected_input.clone();
        self.persistent_settings.output_device_id = audio.selected_output.clone();
        
        if let Err(e) = self.persistent_settings.save() {
            log::error!("Failed to save settings: {}", e);
            self.chat_messages.push(format!("Failed to save settings: {}", e));
        } else {
            self.chat_messages.push("Settings saved.".to_string());
        }
    }

    /// Attempt to reconnect with the last known connection parameters.
    fn reconnect(&mut self) {
        self.chat_messages
            .push(format!("Reconnecting to {}...", self.connect_address));
        self.connect();
    }

    /// Start audio transmission (push-to-talk).
    fn start_transmit(&mut self) {
        self.backend.send(Command::StartTransmit);
    }

    /// Stop audio transmission.
    fn stop_transmit(&mut self) {
        self.backend.send(Command::StopTransmit);
    }

    /// Check if connected based on current state.
    #[allow(dead_code)]
    fn is_connected(&self) -> bool {
        self.backend.state().connection.is_connected()
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.egui_ctx = ctx.clone();

        // Get current state from backend (clone to avoid borrow issues)
        let state = self.backend.state();

        // Reset push-to-talk state if disconnected
        if !state.connection.is_connected() && self.push_to_talk_active {
            self.push_to_talk_active = false;
        }

        // Handle push-to-talk (Space key)
        // Always track Space key state and send commands - the backend handles
        // mode-specific behavior (e.g., ignoring PTT in Continuous mode for
        // start/stop, but still tracking the state for mode switches)
        let space_pressed = ctx.input(|i| i.key_down(egui::Key::Space));
        if space_pressed && !self.push_to_talk_active && state.connection.is_connected() {
            self.push_to_talk_active = true;
            self.start_transmit();
        } else if !space_pressed && self.push_to_talk_active {
            self.push_to_talk_active = false;
            self.stop_transmit();
        }

        // Top menu
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Server", |ui| {
                    if ui.button("Connect...").clicked() {
                        self.show_connect = true;
                        ui.close();
                    }
                    if state.connection.is_connected() {
                        if ui.button("Disconnect").clicked() {
                            self.backend.send(Command::Disconnect);
                            ui.close();
                        }
                    }
                    // Show reconnect option when not connected and we have an address
                    if !state.connection.is_connected() && !self.connect_address.is_empty() {
                        if ui.button("Reconnect").clicked() {
                            self.reconnect();
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Settings", |ui| {
                    if ui.button("Open Settings").clicked() {
                        // Initialize pending settings from current state
                        let audio = self.backend.state().audio.clone();
                        self.settings_modal = SettingsModalState {
                            pending_settings: Some(audio.settings.clone()),
                            pending_input_device: Some(audio.selected_input.clone()),
                            pending_output_device: Some(audio.selected_output.clone()),
                            pending_voice_mode: Some(audio.voice_mode.clone()),
                            pending_autoconnect: Some(self.autoconnect_on_launch),
                            pending_username: Some(self.client_name.clone()),
                            dirty: false,
                        };
                        self.show_settings = true;
                        ui.close();
                    }
                });

                // Show connection status indicator on the right side
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match &state.connection {
                        ConnectionState::Connected { .. } => {
                            ui.colored_label(egui::Color32::GREEN, "● Connected");
                        }
                        ConnectionState::Connecting { .. } => {
                            ui.colored_label(egui::Color32::YELLOW, "● Connecting...");
                        }
                        ConnectionState::ConnectionLost { error } => {
                            if ui.button("⟳ Reconnect").clicked() {
                                self.reconnect();
                            }
                            ui.colored_label(
                                egui::Color32::RED,
                                format!("● Connection Lost: {}", error),
                            );
                        }
                        ConnectionState::Disconnected => {
                            ui.colored_label(egui::Color32::GRAY, "○ Disconnected");
                        }
                    }
                });
            });
        });

        // Toolbar row with audio controls
        egui::TopBottomPanel::top("toolbar_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let audio = &state.audio;
                
                // Mute button
                let mute_icon = if audio.self_muted { "🔇" } else { "🎤" };
                let mute_color = if audio.self_muted { egui::Color32::RED } else { egui::Color32::GREEN };
                if ui.add(egui::Button::new(egui::RichText::new(mute_icon).size(18.0).color(mute_color)))
                    .on_hover_text(if audio.self_muted { "Unmute" } else { "Mute" })
                    .clicked()
                {
                    self.backend.send(Command::SetMuted { muted: !audio.self_muted });
                }
                
                // Deafen button
                let deafen_icon = if audio.self_deafened { "🔇" } else { "🔊" };
                let deafen_color = if audio.self_deafened { egui::Color32::RED } else { egui::Color32::GREEN };
                if ui.add(egui::Button::new(egui::RichText::new(deafen_icon).size(18.0).color(deafen_color)))
                    .on_hover_text(if audio.self_deafened { "Undeafen" } else { "Deafen" })
                    .clicked()
                {
                    self.backend.send(Command::SetDeafened { deafened: !audio.self_deafened });
                }
                
                ui.separator();
                
                // Transmit mode dropdown
                let mode_text = match audio.voice_mode {
                    VoiceMode::PushToTalk => "🎤 PTT",
                    VoiceMode::Continuous => "📡 Continuous",
                };
                egui::ComboBox::from_id_salt("transmit_mode")
                    .selected_text(mode_text)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(
                            matches!(audio.voice_mode, VoiceMode::PushToTalk),
                            "🎤 Push-to-Talk"
                        ).clicked() {
                            self.backend.send(Command::SetVoiceMode { mode: VoiceMode::PushToTalk });
                            self.save_settings();
                        }
                        if ui.selectable_label(
                            matches!(audio.voice_mode, VoiceMode::Continuous),
                            "📡 Continuous"
                        ).clicked() {
                            self.backend.send(Command::SetVoiceMode { mode: VoiceMode::Continuous });
                            self.save_settings();
                        }
                    });
                
                ui.separator();
                
                // Settings button
                if ui.add(egui::Button::new(egui::RichText::new("⚙").size(18.0)))
                    .on_hover_text("Settings")
                    .clicked()
                {
                    // Initialize pending settings from current state
                    let audio = self.backend.state().audio.clone();
                    self.settings_modal = SettingsModalState {
                        pending_settings: Some(audio.settings.clone()),
                        pending_input_device: Some(audio.selected_input.clone()),
                        pending_output_device: Some(audio.selected_output.clone()),
                        pending_voice_mode: Some(audio.voice_mode.clone()),
                        pending_autoconnect: Some(self.autoconnect_on_launch),
                        pending_username: Some(self.client_name.clone()),
                        dirty: false,
                    };
                    self.show_settings = true;
                }
                
                // Transmitting indicator
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if audio.is_transmitting {
                        ui.colored_label(egui::Color32::GREEN, "🎤 TX");
                    }
                });
            });
        });

        // Chat panel
        egui::SidePanel::left("left_panel")
            .default_width(320.0)
            .show(ctx, |ui| {
                egui_extras::StripBuilder::new(ui)
                    .size(egui_extras::Size::remainder())
                    .size(egui_extras::Size::exact(4.0))
                    .size(egui_extras::Size::exact(28.0))
                    .vertical(|mut strip| {
                        strip.cell(|ui| {
                            ui.heading("Text Chat");
                            ui.separator();
                            egui::ScrollArea::vertical()
                                .auto_shrink([false; 2])
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    for msg in &self.chat_messages {
                                        ui.label(msg);
                                    }
                                });
                        });
                        strip.cell(|ui| {
                            ui.add_space(2.0);
                            ui.separator();
                        });
                        strip.cell(|ui| {
                            ui.horizontal(|ui| {
                                let send = {
                                    let resp = ui.text_edit_singleline(&mut self.chat_input);
                                    resp.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                } || ui.button("Send").clicked();
                                if send {
                                    let text = self.chat_input.trim();
                                    if !text.is_empty() && state.connection.is_connected() {
                                        self.backend.send(Command::SendChat {
                                            text: text.to_owned(),
                                        });
                                        self.chat_input.clear();
                                    }
                                }
                            });
                        });
                    });
            });

        // Rooms + users
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Rooms");
            ui.separator();

            if !state.connection.is_connected() {
                ui.label("Not connected. Use Server > Connect...");
            }
            if state.connection.is_connected() && state.rooms.is_empty() {
                ui.horizontal(|ui| {
                    ui.label("No rooms received yet.");
                    if ui.button("Join Root").clicked() {
                        self.backend.send(Command::JoinRoom {
                            room_id: api::ROOT_ROOM_UUID,
                        });
                    }
                    if ui.button("Refresh").clicked() {
                        self.backend.send(Command::JoinRoom {
                            room_id: api::ROOT_ROOM_UUID,
                        });
                    }
                });
                ui.separator();
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for room in state.rooms.iter() {
                    let room_uuid = room.id.as_ref().and_then(api::uuid_from_room_id);
                    let is_current = state.my_room_id == room_uuid;
                    let mut text = room.name.clone();
                    if is_current {
                        text.push_str("  (current)");
                    }
                    let resp = ui.selectable_label(is_current, text);
                    if resp.clicked() {
                        if let Some(uuid) = room_uuid {
                            self.backend.send(Command::JoinRoom { room_id: uuid });
                        }
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Join").clicked() {
                            if let Some(uuid) = room_uuid {
                                self.backend.send(Command::JoinRoom { room_id: uuid });
                            }
                            ui.close();
                        }
                        if ui.button("Rename...").clicked() {
                            self.rename_modal = RenameModalState {
                                open: true,
                                room_uuid,
                                room_name: room.name.clone(),
                            };
                            ui.close();
                        }
                        if ui.button("Add Room").clicked() {
                            self.backend.send(Command::CreateRoom {
                                name: "New Room".to_string(),
                            });
                            ui.close();
                        }
                        let is_root = room_uuid == Some(api::ROOT_ROOM_UUID);
                        if !is_root && ui.button("Delete Room").clicked() {
                            if let Some(uuid) = room_uuid {
                                self.backend.send(Command::DeleteRoom { room_id: uuid });
                            }
                            ui.close();
                        }
                    });
                    // Show users in this room
                    CollapsingHeader::new("Users")
                        .id_salt(room_uuid)
                        .default_open(is_current)
                        .show(ui, |ui| {
                            if let Some(uuid) = room_uuid {
                                for user in state.users_in_room(uuid) {
                                    let user_id =
                                        user.user_id.as_ref().map(|id| id.value).unwrap_or(0);
                                    let is_self = state.my_user_id == Some(user_id);

                                    // Check if user is talking
                                    // For self, use is_transmitting from backend; for others, check talking_users
                                    let is_talking = if is_self {
                                        state.audio.is_transmitting
                                    } else {
                                        state.audio.talking_users.contains(&user_id)
                                    };

                                    // Determine mute/deafen status
                                    // For self, use local audio state; for others, use user proto fields
                                    let (user_muted, user_deafened) = if is_self {
                                        (state.audio.self_muted, state.audio.self_deafened)
                                    } else {
                                        (user.is_muted, user.is_deafened)
                                    };

                                    ui.horizontal(|ui| {
                                        // Microphone/talking indicator
                                        if is_talking {
                                            ui.colored_label(egui::Color32::GREEN, "🎤");
                                        } else if user_muted {
                                            ui.colored_label(egui::Color32::DARK_RED, "🎤");
                                        } else {
                                            ui.colored_label(egui::Color32::DARK_GRAY, "🎤");
                                        }
                                        
                                        // Deafen indicator (only show if deafened)
                                        if user_deafened {
                                            ui.colored_label(egui::Color32::DARK_RED, "🔇");
                                        }
                                        
                                        ui.label(&user.username);
                                    });
                                }
                            }
                        });
                    ui.separator();
                }
            });
        });

        // Connect modal
        if self.show_connect {
            let modal = Modal::new(egui::Id::new("connect_modal")).show(ctx, |ui| {
                ui.set_width(280.0);
                ui.heading("Connect to Server");
                ui.label("Server address:");
                ui.text_edit_singleline(&mut self.connect_address);
                ui.label("Username:");
                ui.text_edit_singleline(&mut self.client_name);
                ui.label("Password (optional):");
                ui.text_edit_singleline(&mut self.connect_password);
                ui.checkbox(
                    &mut self.trust_dev_cert,
                    "Trust dev cert (dev-certs/server-cert.der)",
                );
                ui.separator();
                egui::Sides::new().show(
                    ui,
                    |_l| {},
                    |ui| {
                        if ui.button("Connect").clicked() {
                            self.connect();
                            ui.close();
                        }
                        if ui.button("Cancel").clicked() {
                            ui.close();
                        }
                    },
                );
            });
            if modal.should_close() {
                self.show_connect = false;
            }
        }

        // Settings modal
        if self.show_settings {
            // Initialize pending settings if not already done (e.g., on first open)
            if self.settings_modal.pending_settings.is_none() {
                let audio = state.audio.clone();
                self.settings_modal = SettingsModalState {
                    pending_settings: Some(audio.settings.clone()),
                    pending_input_device: Some(audio.selected_input.clone()),
                    pending_output_device: Some(audio.selected_output.clone()),
                    pending_voice_mode: Some(audio.voice_mode.clone()),
                    pending_autoconnect: Some(self.autoconnect_on_launch),
                    pending_username: Some(self.client_name.clone()),
                    dirty: false,
                };
            }
            
            let modal = Modal::new(egui::Id::new("settings_modal"))
                .show(ctx, |ui| {
                ui.set_width(400.0);
                ui.heading("Settings");

                // Connection settings
                ui.collapsing("Connection", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Server:");
                        ui.label(&self.connect_address);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        ui.label(if state.connection.is_connected() {
                            "Connected"
                        } else {
                            "Disconnected"
                        });
                    });
                    
                    ui.separator();
                    
                    // Username setting
                    ui.label("Username:");
                    let mut pending_username = self.settings_modal.pending_username.clone()
                        .unwrap_or_else(|| self.client_name.clone());
                    if ui.text_edit_singleline(&mut pending_username)
                        .on_hover_text("Your display name shown to other users")
                        .changed()
                    {
                        self.settings_modal.pending_username = Some(pending_username);
                        self.settings_modal.dirty = true;
                    }
                    
                    ui.separator();
                    
                    // Autoconnect on launch toggle
                    let mut pending_autoconnect = self.settings_modal.pending_autoconnect.unwrap_or(self.autoconnect_on_launch);
                    if ui.checkbox(&mut pending_autoconnect, "Autoconnect on launch")
                        .on_hover_text("Automatically connect to the last server when the app starts")
                        .changed() 
                    {
                        self.settings_modal.pending_autoconnect = Some(pending_autoconnect);
                        self.settings_modal.dirty = true;
                    }
                });

                ui.separator();

                // Audio settings
                ui.collapsing("Audio", |ui| {
                    // Clone audio state to avoid borrow conflicts
                    let audio = state.audio.clone();

                    // Refresh button (immediate action, not deferred)
                    if ui.button("🔄 Refresh Devices").clicked() {
                        self.backend.send(Command::RefreshAudioDevices);
                    }

                    ui.separator();

                    // Input device selection (using pending state)
                    ui.label("Input Device (Microphone):");
                    let pending_input = self.settings_modal.pending_input_device.clone().flatten();
                    let current_input_name = pending_input.as_ref()
                        .and_then(|id| audio.input_devices.iter().find(|d| &d.id == id))
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "Default".to_string());

                    egui::ComboBox::from_id_salt("input_device")
                        .selected_text(&current_input_name)
                        .show_ui(ui, |ui| {
                            // Default option
                            if ui
                                .selectable_label(pending_input.is_none(), "Default")
                                .clicked()
                            {
                                self.settings_modal.pending_input_device = Some(None);
                                self.settings_modal.dirty = true;
                            }

                            for device in &audio.input_devices {
                                let label = if device.is_default {
                                    format!("{} (default)", device.name)
                                } else {
                                    device.name.clone()
                                };
                                if ui
                                    .selectable_label(
                                        pending_input.as_ref() == Some(&device.id),
                                        &label,
                                    )
                                    .clicked()
                                {
                                    self.settings_modal.pending_input_device = Some(Some(device.id.clone()));
                                    self.settings_modal.dirty = true;
                                }
                            }
                        });

                    ui.separator();

                    // Output device selection (using pending state)
                    ui.label("Output Device (Speakers):");
                    let pending_output = self.settings_modal.pending_output_device.clone().flatten();
                    let current_output_name = pending_output.as_ref()
                        .and_then(|id| audio.output_devices.iter().find(|d| &d.id == id))
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "Default".to_string());

                    egui::ComboBox::from_id_salt("output_device")
                        .selected_text(&current_output_name)
                        .show_ui(ui, |ui| {
                            // Default option
                            if ui
                                .selectable_label(pending_output.is_none(), "Default")
                                .clicked()
                            {
                                self.settings_modal.pending_output_device = Some(None);
                                self.settings_modal.dirty = true;
                            }

                            for device in &audio.output_devices {
                                let label = if device.is_default {
                                    format!("{} (default)", device.name)
                                } else {
                                    device.name.clone()
                                };
                                if ui
                                    .selectable_label(
                                        pending_output.as_ref() == Some(&device.id),
                                        &label,
                                    )
                                    .clicked()
                                {
                                    self.settings_modal.pending_output_device = Some(Some(device.id.clone()));
                                    self.settings_modal.dirty = true;
                                }
                            }
                        });

                    ui.separator();

                    // Voice mode selector (using pending state)
                    ui.label("Voice Mode:");
                    let pending_voice_mode = self.settings_modal.pending_voice_mode.clone().unwrap_or(audio.voice_mode.clone());
                    ui.horizontal(|ui| {
                        if ui.selectable_label(
                            matches!(pending_voice_mode, VoiceMode::PushToTalk),
                            "Push-to-Talk",
                        ).clicked() {
                            self.settings_modal.pending_voice_mode = Some(VoiceMode::PushToTalk);
                            self.settings_modal.dirty = true;
                        }
                        if ui.selectable_label(
                            matches!(pending_voice_mode, VoiceMode::Continuous),
                            "Continuous",
                        ).clicked() {
                            self.settings_modal.pending_voice_mode = Some(VoiceMode::Continuous);
                            self.settings_modal.dirty = true;
                        }
                    });

                    ui.separator();
                    
                    // Mute and Deafen toggles (immediate action - these are real-time controls)
                    ui.horizontal(|ui| {
                        let mute_text = if audio.self_muted { "🔇 Muted" } else { "🎤 Unmuted" };
                        let mute_color = if audio.self_muted { egui::Color32::RED } else { egui::Color32::GREEN };
                        if ui.add(egui::Button::new(egui::RichText::new(mute_text).color(mute_color)))
                            .on_hover_text("Toggle self-mute (stops transmitting)")
                            .clicked()
                        {
                            self.backend.send(Command::SetMuted { muted: !audio.self_muted });
                        }
                        
                        let deafen_text = if audio.self_deafened { "🔕 Deafened" } else { "🔔 Hearing" };
                        let deafen_color = if audio.self_deafened { egui::Color32::RED } else { egui::Color32::GREEN };
                        if ui.add(egui::Button::new(egui::RichText::new(deafen_text).color(deafen_color)))
                            .on_hover_text("Toggle self-deafen (stops receiving audio; also mutes)")
                            .clicked()
                        {
                            self.backend.send(Command::SetDeafened { deafened: !audio.self_deafened });
                        }
                    });

                    ui.separator();

                    // Status info - show mode-appropriate message
                    if audio.self_muted {
                        ui.colored_label(egui::Color32::RED, "🔇 Muted");
                    } else {
                        match audio.voice_mode {
                            VoiceMode::PushToTalk => {
                                ui.label("Push-to-talk: Hold SPACE to transmit");
                            }
                            VoiceMode::Continuous => {
                                ui.label("Continuous: Always transmitting when connected");
                            }
                        }
                    }
                    
                    if audio.self_deafened {
                        ui.colored_label(egui::Color32::RED, "🔕 Deafened (not receiving audio)");
                    }

                    if audio.is_transmitting {
                        ui.colored_label(egui::Color32::GREEN, "🎤 Transmitting...");
                    } else if !audio.self_muted {
                        ui.label("🔇 Not transmitting");
                    }
                    
                    ui.separator();
                    
                    // Audio Pipeline Settings (using pending state)
                    ui.heading("Audio Pipeline Settings");
                    
                    if let Some(ref mut settings) = self.settings_modal.pending_settings {
                        // RNNoise toggle
                        if ui.checkbox(&mut settings.denoise_enabled, "Enable RNNoise Denoising")
                            .on_hover_text("Apply noise suppression to microphone input")
                            .changed() {
                            self.settings_modal.dirty = true;
                        }
                        
                        // FEC toggle
                        if ui.checkbox(&mut settings.fec_enabled, "Enable Forward Error Correction")
                            .on_hover_text("Add redundancy for packet loss recovery")
                            .changed() {
                            self.settings_modal.dirty = true;
                        }
                        
                        ui.separator();
                        
                        // Bitrate selection
                        ui.label("Encoder Bitrate:");
                        ui.horizontal(|ui| {
                            if ui.selectable_label(settings.bitrate == AudioSettings::BITRATE_LOW, "24 kbps")
                                .on_hover_text("Low quality, minimal bandwidth")
                                .clicked() {
                                settings.bitrate = AudioSettings::BITRATE_LOW;
                                self.settings_modal.dirty = true;
                            }
                            if ui.selectable_label(settings.bitrate == AudioSettings::BITRATE_MEDIUM, "32 kbps")
                                .on_hover_text("Medium quality, good for voice")
                                .clicked() {
                                settings.bitrate = AudioSettings::BITRATE_MEDIUM;
                                self.settings_modal.dirty = true;
                            }
                            if ui.selectable_label(settings.bitrate == AudioSettings::BITRATE_HIGH, "64 kbps")
                                .on_hover_text("High quality (default)")
                                .clicked() {
                                settings.bitrate = AudioSettings::BITRATE_HIGH;
                                self.settings_modal.dirty = true;
                            }
                            if ui.selectable_label(settings.bitrate == AudioSettings::BITRATE_VERY_HIGH, "96 kbps")
                                .on_hover_text("Very high quality, more bandwidth")
                                .clicked() {
                                settings.bitrate = AudioSettings::BITRATE_VERY_HIGH;
                                self.settings_modal.dirty = true;
                            }
                        });
                        
                        ui.separator();
                        
                        // Encoder complexity slider
                        ui.horizontal(|ui| {
                            ui.label("Encoder Complexity:");
                            if ui.add(egui::Slider::new(&mut settings.encoder_complexity, 0..=10)
                                .text(""))
                                .on_hover_text("Higher = better quality but more CPU (0-10)")
                                .changed() {
                                self.settings_modal.dirty = true;
                            }
                        });
                        
                        // Jitter buffer delay slider
                        ui.horizontal(|ui| {
                            ui.label("Jitter Buffer Delay:");
                            if ui.add(egui::Slider::new(&mut settings.jitter_buffer_delay_packets, 1..=10)
                                .text("packets"))
                                .on_hover_text("Packets to buffer before playback (1-10). Higher = more latency, smoother audio")
                                .changed() {
                                self.settings_modal.dirty = true;
                            }
                        });
                        let jitter_delay_ms = settings.jitter_buffer_delay_packets * 20;
                        ui.label(format!("  Playback delay: ~{}ms", jitter_delay_ms));
                        
                        // Packet loss percentage for FEC tuning
                        ui.horizontal(|ui| {
                            ui.label("Expected Packet Loss:");
                            if ui.add(egui::Slider::new(&mut settings.packet_loss_percent, 0..=25)
                                .text("%"))
                                .on_hover_text("Expected network packet loss for FEC tuning (0-25%)")
                                .changed() {
                                self.settings_modal.dirty = true;
                            }
                        });
                    }
                    
                    ui.separator();
                    
                    // Audio Statistics
                    ui.heading("Audio Statistics");
                    
                    let stats = &audio.stats;
                    
                    ui.horizontal(|ui| {
                        ui.label("Actual Bitrate:");
                        ui.label(format!("{:.1} kbps", stats.actual_bitrate_bps / 1000.0));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Avg Frame Size:");
                        ui.label(format!("{:.1} bytes", stats.avg_frame_size_bytes));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Packets Sent:");
                        ui.label(format!("{}", stats.packets_sent));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Packets Received:");
                        ui.label(format!("{}", stats.packets_received));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Packet Loss:");
                        let loss_pct = stats.packet_loss_percent();
                        let color = if loss_pct > 5.0 {
                            egui::Color32::RED
                        } else if loss_pct > 1.0 {
                            egui::Color32::YELLOW
                        } else {
                            egui::Color32::GREEN
                        };
                        ui.colored_label(color, format!("{:.1}% ({} lost)", loss_pct, stats.packets_lost));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("FEC Recovered:");
                        ui.label(format!("{}", stats.packets_recovered_fec));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Frames Concealed:");
                        ui.label(format!("{}", stats.frames_concealed));
                    });
                    
                    ui.horizontal(|ui| {
                        ui.label("Buffer Level:");
                        ui.label(format!("{} packets", stats.playback_buffer_packets));
                    });
                    
                    if ui.button("Reset Statistics").clicked() {
                        self.backend.send(Command::ResetAudioStats);
                    }
                });

                ui.separator();
                
                // Show dirty indicator
                if self.settings_modal.dirty {
                    ui.colored_label(egui::Color32::YELLOW, "⚠ Unsaved changes");
                }
                
                egui::Sides::new().show(
                    ui,
                    |_l| {},
                    |ui| {
                        // Apply button - commits all pending changes and saves to disk
                        let apply_enabled = self.settings_modal.dirty;
                        if ui.add_enabled(apply_enabled, egui::Button::new("Apply")).clicked() {
                            // Apply pending autoconnect setting
                            if let Some(autoconnect) = self.settings_modal.pending_autoconnect {
                                self.autoconnect_on_launch = autoconnect;
                            }
                            
                            // Apply pending username
                            if let Some(username) = self.settings_modal.pending_username.clone() {
                                if !username.trim().is_empty() {
                                    self.client_name = username;
                                }
                            }
                            
                            // Apply pending audio settings
                            if let Some(settings) = self.settings_modal.pending_settings.clone() {
                                self.backend.send(Command::UpdateAudioSettings { settings });
                            }
                            
                            // Apply pending input device
                            if let Some(device_id) = self.settings_modal.pending_input_device.clone() {
                                let current = self.backend.state().audio.selected_input.clone();
                                if device_id != current {
                                    self.backend.send(Command::SetInputDevice { device_id });
                                }
                            }
                            
                            // Apply pending output device
                            if let Some(device_id) = self.settings_modal.pending_output_device.clone() {
                                let current = self.backend.state().audio.selected_output.clone();
                                if device_id != current {
                                    self.backend.send(Command::SetOutputDevice { device_id });
                                }
                            }
                            
                            // Apply pending voice mode
                            if let Some(mode) = self.settings_modal.pending_voice_mode.clone() {
                                let current = self.backend.state().audio.voice_mode.clone();
                                if mode != current {
                                    self.backend.send(Command::SetVoiceMode { mode });
                                }
                            }
                            
                            self.settings_modal.dirty = false;
                            
                            // Save settings to disk
                            self.save_settings();
                        }
                        
                        if ui.button("Cancel").clicked() {
                            self.settings_modal = SettingsModalState::default();
                            self.show_settings = false;
                        }
                        
                        if ui.button("Ok").clicked() {
                            // Apply changes before closing if dirty
                            if self.settings_modal.dirty {
                                // Apply pending autoconnect setting
                                if let Some(autoconnect) = self.settings_modal.pending_autoconnect {
                                    self.autoconnect_on_launch = autoconnect;
                                }
                                
                                // Apply pending username
                                if let Some(username) = self.settings_modal.pending_username.clone() {
                                    if !username.trim().is_empty() {
                                        self.client_name = username;
                                    }
                                }
                                
                                if let Some(settings) = self.settings_modal.pending_settings.clone() {
                                    self.backend.send(Command::UpdateAudioSettings { settings });
                                }
                                if let Some(device_id) = self.settings_modal.pending_input_device.clone() {
                                    let current = self.backend.state().audio.selected_input.clone();
                                    if device_id != current {
                                        self.backend.send(Command::SetInputDevice { device_id });
                                    }
                                }
                                if let Some(device_id) = self.settings_modal.pending_output_device.clone() {
                                    let current = self.backend.state().audio.selected_output.clone();
                                    if device_id != current {
                                        self.backend.send(Command::SetOutputDevice { device_id });
                                    }
                                }
                                if let Some(mode) = self.settings_modal.pending_voice_mode.clone() {
                                    let current = self.backend.state().audio.voice_mode.clone();
                                    if mode != current {
                                        self.backend.send(Command::SetVoiceMode { mode });
                                    }
                                }
                                
                                // Save settings to disk
                                self.save_settings();
                            }
                            self.settings_modal = SettingsModalState::default();
                            self.show_settings = false;
                        }
                    },
                );
            });
            // Don't auto-close on click outside - only close via Cancel/Close buttons
            let _ = modal;
        }

        // Rename room modal
        if self.rename_modal.open {
            let modal = Modal::new(egui::Id::new("rename_modal")).show(ctx, |ui| {
                ui.set_width(280.0);
                ui.heading("Rename Room");
                ui.label("New name:");
                ui.text_edit_singleline(&mut self.rename_modal.room_name);
                ui.separator();
                egui::Sides::new().show(
                    ui,
                    |_l| {},
                    |ui| {
                        if ui.button("Rename").clicked() {
                            let new_name = self.rename_modal.room_name.trim().to_string();
                            if !new_name.is_empty() {
                                if let Some(uuid) = self.rename_modal.room_uuid {
                                    self.backend.send(Command::RenameRoom {
                                        room_id: uuid,
                                        new_name,
                                    });
                                }
                            }
                            ui.close();
                        }
                        if ui.button("Cancel").clicked() {
                            ui.close();
                        }
                    },
                );
            });
            if modal.should_close() {
                self.rename_modal.open = false;
            }
        }
    }
}
