use backend::{AudioConfig, AudioDeviceInfo, AudioInput, AudioOutput, AudioSystem};
use backend::{BackendCommand, BackendEvent, BackendHandle};
use backend::{bytes_to_samples, samples_to_bytes};
use clap::Parser;
use eframe::egui;
use egui::{CollapsingHeader, Modal};
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
    room_id: u64,
    room_name: String,
}

/// Audio settings state
struct AudioSettings {
    /// Audio subsystem handle
    audio_system: AudioSystem,
    /// Available input devices
    input_devices: Vec<AudioDeviceInfo>,
    /// Available output devices  
    output_devices: Vec<AudioDeviceInfo>,
    /// Selected input device index (None = default)
    selected_input: Option<usize>,
    /// Selected output device index (None = default)
    selected_output: Option<usize>,
    /// Whether microphone is enabled (push-to-talk or voice activity)
    /// Note: Currently push-to-talk is the only mode; this field is reserved for voice activity.
    #[allow(dead_code)]
    mic_enabled: bool,
    /// Whether audio output is enabled
    output_enabled: bool,
    /// Input volume (0.0 - 2.0, 1.0 = normal)
    input_volume: f32,
    /// Output volume (0.0 - 2.0, 1.0 = normal)
    output_volume: f32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        let audio_system = AudioSystem::new();
        let input_devices = audio_system.list_input_devices();
        let output_devices = audio_system.list_output_devices();

        // Find default device indices
        let selected_input = input_devices.iter().find(|d| d.is_default).map(|d| d.index);
        let selected_output = output_devices
            .iter()
            .find(|d| d.is_default)
            .map(|d| d.index);

        Self {
            audio_system,
            input_devices,
            output_devices,
            selected_input,
            selected_output,
            mic_enabled: false,
            output_enabled: true,
            input_volume: 1.0,
            output_volume: 1.0,
        }
    }
}

impl AudioSettings {
    /// Refresh the device lists
    fn refresh_devices(&mut self) {
        self.input_devices = self.audio_system.list_input_devices();
        self.output_devices = self.audio_system.list_output_devices();
    }
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

    // Backend handle (manages connection and state)
    backend: BackendHandle,

    // Rename modal state
    rename_modal: RenameModalState,

    // Audio settings and state
    audio_settings: AudioSettings,
    /// Active audio input stream (mic capture)
    audio_input: Option<AudioInput>,
    /// Active audio output stream (playback)
    audio_output: Option<AudioOutput>,
    /// Push-to-talk key is held
    push_to_talk_active: bool,
    /// Track which users are currently talking (user_id -> last voice time)
    talking_users: std::collections::HashMap<u64, std::time::Instant>,

    egui_ctx: egui::Context,
}

impl Drop for MyApp {
    fn drop(&mut self) {
        // Stop audio streams first.
        self.audio_input = None;
        self.audio_output = None;

        // Send disconnect command before dropping the backend handle
        if self.backend.is_connected() {
            self.backend.send(BackendCommand::Disconnect);
        }
        // BackendHandle will clean up the background thread when dropped
    }
}

impl MyApp {
    fn new(ctx: egui::Context, args: Args) -> Self {
        // Create backend handle with repaint callback
        let ctx_for_repaint = ctx.clone();
        let backend = BackendHandle::with_repaint_callback(Some(move || {
            ctx_for_repaint.request_repaint();
        }));

        // Use provided name or generate a random one
        let client_name = args
            .name
            .clone()
            .unwrap_or_else(|| format!("user-{}", Uuid::new_v4().simple()));

        // Use provided server address or default empty
        let connect_address = args.server.clone().unwrap_or_default();

        // Use provided password or empty
        let connect_password = args.password.clone().unwrap_or_default();

        // If server was specified on command line, connect immediately
        if let Some(server_addr) = &args.server {
            let addr = if server_addr.trim().is_empty() {
                "127.0.0.1:5000".to_string()
            } else {
                server_addr.trim().to_string()
            };

            // Build connect config based on CLI args
            let mut config = backend::ConnectConfig::new();
            if args.trust_dev_cert {
                config = config.with_cert("dev-certs/server-cert.der");
            }
            if let Some(cert_path) = &args.cert {
                config = config.with_cert(cert_path);
            }
            if let Ok(cert_path) = std::env::var("RUMBLE_SERVER_CERT_PATH") {
                config = config.with_cert(cert_path);
            }

            backend.send(BackendCommand::Connect {
                addr,
                name: client_name.clone(),
                password: args.password.clone(),
                config,
            });
        }

        Self {
            show_connect: false,
            show_settings: false,
            connect_address,
            connect_password,
            trust_dev_cert: args.trust_dev_cert,
            chat_messages: Vec::new(),
            chat_input: String::new(),
            client_name,
            backend,
            rename_modal: RenameModalState::default(),
            audio_settings: AudioSettings::default(),
            audio_input: None,
            audio_output: None,
            push_to_talk_active: false,
            talking_users: std::collections::HashMap::new(),
            egui_ctx: ctx,
        }
    }

    /// Send a command to the backend (non-blocking)
    fn send_command(&self, cmd: BackendCommand) {
        self.backend.send(cmd);
    }

    /// Start audio capture with the selected input device.
    fn start_audio_input(&mut self) {
        if self.audio_input.is_some() {
            return; // Already running
        }

        let device = if let Some(idx) = self.audio_settings.selected_input {
            self.audio_settings.audio_system.get_input_device(idx)
        } else {
            self.audio_settings.audio_system.default_input_device()
        };

        let Some(device) = device else {
            eprintln!("No input device available");
            return;
        };

        let config = AudioConfig::default();
        let input_volume = self.audio_settings.input_volume;
        
        // Get a command sender that can be used from the audio callback thread
        let command_sender = self.backend.command_sender();
        
        match AudioInput::new(&device, &config, move |samples| {
            // Apply volume and send to backend via command sender
            let adjusted: Vec<f32> = samples.iter().map(|s| s * input_volume).collect();
            let bytes = samples_to_bytes(&adjusted);
            command_sender.send_voice(bytes);
        }) {
            Ok(input) => {
                self.audio_input = Some(input);
                eprintln!("Audio input started");
            }
            Err(e) => {
                eprintln!("Failed to start audio input: {}", e);
            }
        }
    }

    /// Stop audio capture.
    fn stop_audio_input(&mut self) {
        if let Some(input) = self.audio_input.take() {
            input.pause();
            eprintln!("Audio input stopped");
        }
    }

    /// Start audio playback with the selected output device.
    fn start_audio_output(&mut self) {
        if self.audio_output.is_some() {
            return; // Already running
        }

        let device = if let Some(idx) = self.audio_settings.selected_output {
            self.audio_settings.audio_system.get_output_device(idx)
        } else {
            self.audio_settings.audio_system.default_output_device()
        };

        let Some(device) = device else {
            eprintln!("No output device available");
            return;
        };

        let config = AudioConfig::default();

        match AudioOutput::new(&device, &config) {
            Ok(output) => {
                self.audio_output = Some(output);
                eprintln!("Audio output started");
            }
            Err(e) => {
                eprintln!("Failed to start audio output: {}", e);
            }
        }
    }

    /// Stop audio playback.
    fn stop_audio_output(&mut self) {
        if let Some(output) = self.audio_output.take() {
            output.pause();
            eprintln!("Audio output stopped");
        }
    }

    /// Queue received audio for playback.
    fn play_received_audio(&self, audio_bytes: &[u8]) {
        if let Some(output) = &self.audio_output {
            let samples = bytes_to_samples(audio_bytes);
            // Apply output volume
            let adjusted: Vec<f32> = samples
                .iter()
                .map(|s| s * self.audio_settings.output_volume)
                .collect();
            output.queue_samples(&adjusted);
        }
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.egui_ctx = ctx.clone();

        // Process incoming backend events (non-blocking)
        while let Some(event) = self.backend.poll_event() {
            match event {
                BackendEvent::Connected { user_id, client_name } => {
                    self.chat_messages
                        .push(format!("Connected to {} as {} (id: {})", 
                            self.connect_address, client_name, user_id));
                    // Start audio output when connected
                    if self.audio_settings.output_enabled {
                        self.start_audio_output();
                    }
                }
                BackendEvent::ConnectFailed { error } => {
                    self.chat_messages
                        .push(format!("Failed to connect: {}", error));
                }
                BackendEvent::Disconnected { reason } => {
                    let msg = match reason {
                        Some(r) => format!("Disconnected from server: {}", r),
                        None => "Disconnected from server".to_string(),
                    };
                    self.chat_messages.push(msg);
                    // Stop audio when disconnected
                    self.stop_audio_input();
                    self.stop_audio_output();
                }
                BackendEvent::ChatReceived { sender, text } => {
                    self.chat_messages.push(format!("{}: {}", sender, text));
                }
                BackendEvent::StateUpdated { state: _ } => {
                    // State is automatically updated in the backend handle
                }
                BackendEvent::VoiceReceived {
                    sender_id,
                    audio_bytes,
                } => {
                    // Track that this user is talking
                    self.talking_users
                        .insert(sender_id, std::time::Instant::now());
                    self.play_received_audio(&audio_bytes);
                }
                BackendEvent::Error { message } => {
                    self.chat_messages.push(format!("Error: {}", message));
                }
            }
        }

        // Clean up stale talking indicators (after 200ms of no voice data)
        let now = std::time::Instant::now();
        self.talking_users
            .retain(|_, last_time| now.duration_since(*last_time).as_millis() < 200);

        // Handle push-to-talk (Space key)
        let space_pressed = ctx.input(|i| i.key_down(egui::Key::Space));
        if space_pressed && !self.push_to_talk_active && self.backend.is_connected() {
            self.push_to_talk_active = true;
            self.start_audio_input();
        } else if !space_pressed && self.push_to_talk_active {
            self.push_to_talk_active = false;
            self.stop_audio_input();
        }

        // Top menu
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Server", |ui| {
                    if ui.button("Connect...").clicked() {
                        self.show_connect = true;
                        ui.close();
                    }
                    if self.backend.is_connected() {
                        if ui.button("Disconnect").clicked() {
                            self.send_command(BackendCommand::Disconnect);
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Settings", |ui| {
                    if ui.button("Open Settings").clicked() {
                        self.show_settings = true;
                        ui.close();
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
                                    if !text.is_empty() && self.backend.is_connected() {
                                        self.send_command(BackendCommand::SendChat {
                                            text: text.to_owned(),
                                        });
                                        self.chat_input.clear();
                                    }
                                }
                            });
                        });
                    });
            });

        // Get current state from backend (clone to avoid borrow issues)
        let state = self.backend.state().clone();

        // Rooms + users
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Rooms");
            ui.separator();

            if !state.connected {
                ui.label("Not connected. Use Server > Connect...");
            }
            if state.connected && state.rooms.is_empty() {
                ui.horizontal(|ui| {
                    ui.label("No rooms received yet.");
                    if ui.button("Join Root").clicked() {
                        self.send_command(BackendCommand::JoinRoom { room_id: 1 });
                    }
                    if ui.button("Refresh").clicked() {
                        self.send_command(BackendCommand::JoinRoom { room_id: 1 });
                    }
                });
                ui.separator();
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for room in state.rooms.iter() {
                    let rid = room.id.as_ref().map(|r| r.value).unwrap_or(0);
                    let is_current = state.current_room_id == Some(rid);
                    let mut text = room.name.clone();
                    if is_current {
                        text.push_str("  (current)");
                    }
                    let resp = ui.selectable_label(is_current, text);
                    if resp.clicked() {
                        self.send_command(BackendCommand::JoinRoom { room_id: rid });
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Join").clicked() {
                            self.send_command(BackendCommand::JoinRoom { room_id: rid });
                            ui.close();
                        }
                        if ui.button("Rename...").clicked() {
                            self.rename_modal = RenameModalState {
                                open: true,
                                room_id: rid,
                                room_name: room.name.clone(),
                            };
                            ui.close();
                        }
                        if ui.button("Add Room").clicked() {
                            self.send_command(BackendCommand::CreateRoom {
                                name: "New Room".to_string(),
                            });
                            ui.close();
                        }
                        if rid != 1 && ui.button("Delete Room").clicked() {
                            self.send_command(BackendCommand::DeleteRoom { room_id: rid });
                            ui.close();
                        }
                    });
                    // Show users in this room
                    CollapsingHeader::new("Users")
                        .id_salt(rid)
                        .default_open(is_current)
                        .show(ui, |ui| {
                            for up in state.users_in_room(rid) {
                                let user_id = up.user_id.as_ref().map(|id| id.value).unwrap_or(0);
                                let is_self = state.my_user_id == Some(user_id);

                                // Check if user is talking
                                // For self, use push_to_talk_active; for others, check talking_users
                                let is_talking = if is_self {
                                    self.push_to_talk_active
                                } else {
                                    self.talking_users.contains_key(&user_id)
                                };

                                ui.horizontal(|ui| {
                                    if is_talking {
                                        ui.colored_label(egui::Color32::LIGHT_GREEN, "🎤");
                                    } else {
                                        ui.colored_label(egui::Color32::DARK_GRAY, "🔇");
                                    }
                                    // let label = format!("{}",up.username);
                                    ui.label(up.username.to_owned());
                                });
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

                            // Build connect config based on UI settings
                            let mut config = backend::ConnectConfig::new();
                            if self.trust_dev_cert {
                                config = config.with_cert("dev-certs/server-cert.der");
                            }
                            if let Ok(cert_path) = std::env::var("RUMBLE_SERVER_CERT_PATH") {
                                config = config.with_cert(cert_path);
                            }

                            self.send_command(BackendCommand::Connect {
                                addr,
                                name,
                                password,
                                config,
                            });
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
            let modal = Modal::new(egui::Id::new("settings_modal")).show(ctx, |ui| {
                ui.set_width(400.0);
                ui.heading("Settings");

                // Connection info
                ui.collapsing("Connection", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Server:");
                        ui.label(&self.connect_address);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        ui.label(if self.backend.is_connected() {
                            "Connected"
                        } else {
                            "Disconnected"
                        });
                    });
                });

                ui.separator();

                // Audio settings
                ui.collapsing("Audio", |ui| {
                    // Refresh button
                    if ui.button("🔄 Refresh Devices").clicked() {
                        self.audio_settings.refresh_devices();
                    }

                    ui.separator();

                    // Input device selection
                    ui.label("Input Device (Microphone):");
                    let current_input_name = self
                        .audio_settings
                        .selected_input
                        .and_then(|idx| {
                            self.audio_settings
                                .input_devices
                                .iter()
                                .find(|d| d.index == idx)
                        })
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "Default".to_string());

                    // Track selection change outside the borrow
                    let mut input_device_changed: Option<Option<usize>> = None;

                    egui::ComboBox::from_id_salt("input_device")
                        .selected_text(&current_input_name)
                        .show_ui(ui, |ui| {
                            // Default option
                            if ui
                                .selectable_label(
                                    self.audio_settings.selected_input.is_none(),
                                    "Default",
                                )
                                .clicked()
                            {
                                input_device_changed = Some(None);
                            }

                            for device in &self.audio_settings.input_devices {
                                let label = if device.is_default {
                                    format!("{} (default)", device.name)
                                } else {
                                    device.name.clone()
                                };
                                if ui
                                    .selectable_label(
                                        self.audio_settings.selected_input == Some(device.index),
                                        &label,
                                    )
                                    .clicked()
                                {
                                    input_device_changed = Some(Some(device.index));
                                }
                            }
                        });

                    // Handle input device change after the borrow ends
                    if let Some(new_selection) = input_device_changed {
                        self.audio_settings.selected_input = new_selection;
                        if self.audio_input.is_some() {
                            self.stop_audio_input();
                            self.start_audio_input();
                        }
                    }

                    // Input volume slider
                    ui.horizontal(|ui| {
                        ui.label("Input Volume:");
                        ui.add(
                            egui::Slider::new(&mut self.audio_settings.input_volume, 0.0..=2.0)
                                .show_value(true)
                                .suffix("x"),
                        );
                    });

                    ui.separator();

                    // Output device selection
                    ui.label("Output Device (Speakers):");
                    let current_output_name = self
                        .audio_settings
                        .selected_output
                        .and_then(|idx| {
                            self.audio_settings
                                .output_devices
                                .iter()
                                .find(|d| d.index == idx)
                        })
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "Default".to_string());

                    // Track selection change outside the borrow
                    let mut output_device_changed: Option<Option<usize>> = None;

                    egui::ComboBox::from_id_salt("output_device")
                        .selected_text(&current_output_name)
                        .show_ui(ui, |ui| {
                            // Default option
                            if ui
                                .selectable_label(
                                    self.audio_settings.selected_output.is_none(),
                                    "Default",
                                )
                                .clicked()
                            {
                                output_device_changed = Some(None);
                            }

                            for device in &self.audio_settings.output_devices {
                                let label = if device.is_default {
                                    format!("{} (default)", device.name)
                                } else {
                                    device.name.clone()
                                };
                                if ui
                                    .selectable_label(
                                        self.audio_settings.selected_output == Some(device.index),
                                        &label,
                                    )
                                    .clicked()
                                {
                                    output_device_changed = Some(Some(device.index));
                                }
                            }
                        });

                    // Handle output device change after the borrow ends
                    if let Some(new_selection) = output_device_changed {
                        self.audio_settings.selected_output = new_selection;
                        if self.audio_output.is_some() {
                            self.stop_audio_output();
                            self.start_audio_output();
                        }
                    }

                    // Output volume slider
                    ui.horizontal(|ui| {
                        ui.label("Output Volume:");
                        ui.add(
                            egui::Slider::new(&mut self.audio_settings.output_volume, 0.0..=2.0)
                                .show_value(true)
                                .suffix("x"),
                        );
                    });

                    ui.separator();

                    // Output enabled toggle
                    ui.checkbox(
                        &mut self.audio_settings.output_enabled,
                        "Enable audio output",
                    );

                    // Status info
                    ui.separator();
                    ui.label("Push-to-talk: Hold SPACE to transmit");

                    if self.push_to_talk_active {
                        ui.colored_label(egui::Color32::GREEN, "🎤 Transmitting...");
                    } else {
                        ui.label("🔇 Not transmitting");
                    }

                    // Show buffer status if output is active
                    if let Some(output) = &self.audio_output {
                        let buffered = output.buffered_samples();
                        ui.label(format!("Playback buffer: {} samples", buffered));
                    }
                });

                ui.separator();
                egui::Sides::new().show(
                    ui,
                    |_l| {},
                    |ui| {
                        if ui.button("Close").clicked() {
                            ui.close();
                        }
                    },
                );
            });
            if modal.should_close() {
                self.show_settings = false;
            }
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
                                self.send_command(BackendCommand::RenameRoom {
                                    room_id: self.rename_modal.room_id,
                                    new_name,
                                });
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
