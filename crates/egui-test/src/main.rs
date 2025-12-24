use backend::{BackendCommand, BackendEvent, BackendHandle};
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
    room_uuid: Option<Uuid>,
    room_name: String,
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

    // Backend handle (manages connection, audio, and state)
    backend: BackendHandle,

    // Rename modal state
    rename_modal: RenameModalState,

    /// Push-to-talk key is held
    push_to_talk_active: bool,
    /// Track which users are currently talking (user_id -> is_talking)
    talking_users: std::collections::HashSet<u64>,

    egui_ctx: egui::Context,
}

impl Drop for MyApp {
    fn drop(&mut self) {
        // Send disconnect command before dropping the backend handle
        if self.backend.is_connected() {
            self.backend.send(BackendCommand::Disconnect);
        }
        // BackendHandle will clean up the background thread and audio when dropped
    }
}

impl MyApp {
    fn new(ctx: egui::Context, args: Args) -> Self {
        // Create backend handle with repaint callback
        let ctx_for_repaint = ctx.clone();
        let mut backend = BackendHandle::with_repaint_callback(move || {
            ctx_for_repaint.request_repaint();
        });

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
            push_to_talk_active: false,
            talking_users: std::collections::HashSet::new(),
            egui_ctx: ctx,
        }
    }

    /// Send a command to the backend (non-blocking)
    fn send_command(&mut self, cmd: BackendCommand) {
        self.backend.send(cmd);
    }

    /// Attempt to reconnect with the last known connection parameters.
    fn reconnect(&mut self) {
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

        // Build connect config based on saved settings
        let mut config = backend::ConnectConfig::new();
        if self.trust_dev_cert {
            config = config.with_cert("dev-certs/server-cert.der");
        }
        if let Ok(cert_path) = std::env::var("RUMBLE_SERVER_CERT_PATH") {
            config = config.with_cert(cert_path);
        }

        self.chat_messages
            .push(format!("Reconnecting to {}...", addr));
        self.send_command(BackendCommand::Connect {
            addr,
            name,
            password,
            config,
        });
    }

    /// Start audio capture (push-to-talk)
    fn start_voice_capture(&mut self) {
        self.backend.send(BackendCommand::StartVoiceCapture);
    }

    /// Stop audio capture
    fn stop_voice_capture(&mut self) {
        self.backend.send(BackendCommand::StopVoiceCapture);
    }

    /// Start audio playback
    fn start_playback(&mut self) {
        self.backend.send(BackendCommand::StartPlayback);
    }

    /// Stop audio playback
    fn stop_playback(&mut self) {
        self.backend.send(BackendCommand::StopPlayback);
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.egui_ctx = ctx.clone();

        // Process incoming backend events (non-blocking)
        while let Some(event) = self.backend.poll_event() {
            match event {
                BackendEvent::Connected {
                    user_id,
                    client_name,
                } => {
                    self.chat_messages.push(format!(
                        "Connected to {} as {} (id: {})",
                        self.connect_address, client_name, user_id
                    ));
                    // Start audio playback when connected
                    self.start_playback();
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
                    self.stop_voice_capture();
                    self.stop_playback();
                }
                BackendEvent::ConnectionLost {
                    error,
                    will_reconnect,
                } => {
                    let msg = if will_reconnect {
                        format!("Connection lost: {}. Reconnecting...", error)
                    } else {
                        format!("Connection lost: {}", error)
                    };
                    self.chat_messages.push(msg);
                    // Stop audio when connection is lost
                    self.stop_voice_capture();
                    self.stop_playback();
                }
                BackendEvent::ConnectionStatusChanged { status } => {
                    // Log status changes for debugging
                    use backend::ConnectionStatus;
                    match status {
                        ConnectionStatus::Connecting => {
                            self.chat_messages.push("Connecting...".to_string());
                        }
                        ConnectionStatus::Connected => {
                            // Already handled by BackendEvent::Connected
                        }
                        ConnectionStatus::Reconnecting {
                            attempt,
                            max_attempts,
                        } => {
                            self.chat_messages.push(format!(
                                "Reconnecting (attempt {}/{})...",
                                attempt,
                                if max_attempts == 0 {
                                    "∞".to_string()
                                } else {
                                    max_attempts.to_string()
                                }
                            ));
                        }
                        ConnectionStatus::ConnectionLost => {
                            // Already handled by BackendEvent::ConnectionLost
                        }
                        ConnectionStatus::Disconnected => {
                            // Already handled by BackendEvent::Disconnected
                        }
                    }
                }
                BackendEvent::ChatReceived { sender, text } => {
                    self.chat_messages.push(format!("{}: {}", sender, text));
                }
                BackendEvent::StateUpdated { state: _ } => {
                    // State is automatically updated in the backend handle
                }
                BackendEvent::VoiceActivity {
                    user_id,
                    is_talking,
                } => {
                    // Track which users are talking
                    if is_talking {
                        self.talking_users.insert(user_id);
                    } else {
                        self.talking_users.remove(&user_id);
                    }
                }
                BackendEvent::AudioStateChanged { state: _ } => {
                    // Audio state is updated in backend.state().audio automatically
                    // No action needed here - UI reads from backend state directly
                }
                BackendEvent::Error { message } => {
                    self.chat_messages.push(format!("Error: {}", message));
                }
            }
        }

        // Handle push-to-talk (Space key)
        let space_pressed = ctx.input(|i| i.key_down(egui::Key::Space));
        if space_pressed && !self.push_to_talk_active && self.backend.is_connected() {
            self.push_to_talk_active = true;
            self.start_voice_capture();
        } else if !space_pressed && self.push_to_talk_active {
            self.push_to_talk_active = false;
            self.stop_voice_capture();
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
                    // Show reconnect option when not connected and we have an address
                    if !self.backend.is_connected() && !self.connect_address.is_empty() {
                        if ui.button("Reconnect").clicked() {
                            self.reconnect();
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

                // Show connection status indicator on the right side
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    use backend::ConnectionStatus;
                    let status = self.backend.connection_status();
                    match status {
                        ConnectionStatus::Connected => {
                            ui.colored_label(egui::Color32::GREEN, "● Connected");
                        }
                        ConnectionStatus::Connecting => {
                            ui.colored_label(egui::Color32::YELLOW, "● Connecting...");
                        }
                        ConnectionStatus::Reconnecting { attempt, .. } => {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                format!("● Reconnecting ({})...", attempt),
                            );
                        }
                        ConnectionStatus::ConnectionLost => {
                            if ui.button("⟳ Reconnect").clicked() {
                                self.reconnect();
                            }
                            ui.colored_label(egui::Color32::RED, "● Connection Lost");
                        }
                        ConnectionStatus::Disconnected => {
                            ui.colored_label(egui::Color32::GRAY, "○ Disconnected");
                        }
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
                        self.send_command(BackendCommand::JoinRoom {
                            room_uuid: api::ROOT_ROOM_UUID,
                        });
                    }
                    if ui.button("Refresh").clicked() {
                        self.send_command(BackendCommand::JoinRoom {
                            room_uuid: api::ROOT_ROOM_UUID,
                        });
                    }
                });
                ui.separator();
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for room in state.rooms.iter() {
                    let room_uuid = room.id.as_ref().and_then(api::uuid_from_room_id);
                    let is_current = state.current_room_id == room_uuid;
                    let mut text = room.name.clone();
                    if is_current {
                        text.push_str("  (current)");
                    }
                    let resp = ui.selectable_label(is_current, text);
                    if resp.clicked() {
                        if let Some(uuid) = room_uuid {
                            self.send_command(BackendCommand::JoinRoom { room_uuid: uuid });
                        }
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Join").clicked() {
                            if let Some(uuid) = room_uuid {
                                self.send_command(BackendCommand::JoinRoom { room_uuid: uuid });
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
                            self.send_command(BackendCommand::CreateRoom {
                                name: "New Room".to_string(),
                            });
                            ui.close();
                        }
                        let is_root = room_uuid == Some(api::ROOT_ROOM_UUID);
                        if !is_root && ui.button("Delete Room").clicked() {
                            if let Some(uuid) = room_uuid {
                                self.send_command(BackendCommand::DeleteRoom { room_uuid: uuid });
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
                                for up in state.users_in_room(uuid) {
                                    let user_id =
                                        up.user_id.as_ref().map(|id| id.value).unwrap_or(0);
                                    let is_self = state.my_user_id == Some(user_id);

                                    // Check if user is talking
                                    // For self, use push_to_talk_active; for others, check talking_users
                                    let is_talking = if is_self {
                                        self.push_to_talk_active
                                    } else {
                                        self.talking_users.contains(&user_id)
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
                    // Clone audio state to avoid borrow conflicts with send()
                    let audio = self.backend.state().audio.clone();

                    // Refresh button
                    if ui.button("🔄 Refresh Devices").clicked() {
                        self.backend.send(BackendCommand::RefreshAudioDevices);
                    }

                    ui.separator();

                    // Input device selection
                    ui.label("Input Device (Microphone):");
                    let current_input_name = audio
                        .selected_input_device()
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "Default".to_string());

                    // Track selection change
                    let mut input_device_changed: Option<Option<String>> = None;

                    egui::ComboBox::from_id_salt("input_device")
                        .selected_text(&current_input_name)
                        .show_ui(ui, |ui| {
                            // Default option
                            if ui
                                .selectable_label(audio.selected_input_id.is_none(), "Default")
                                .clicked()
                            {
                                input_device_changed = Some(None);
                            }

                            for device in &audio.input_devices {
                                let label = if device.is_default {
                                    format!("{} (default)", device.name)
                                } else {
                                    device.name.clone()
                                };
                                if ui
                                    .selectable_label(
                                        audio.selected_input_id.as_ref() == Some(&device.id),
                                        &label,
                                    )
                                    .clicked()
                                {
                                    input_device_changed = Some(Some(device.id.clone()));
                                }
                            }
                        });

                    // Handle input device change
                    if let Some(new_selection) = input_device_changed {
                        self.backend.send(BackendCommand::SetInputDevice {
                            device_id: new_selection,
                        });
                    }

                    // Input volume slider
                    let mut input_volume = audio.input_volume;
                    ui.horizontal(|ui| {
                        ui.label("Input Volume:");
                        if ui
                            .add(
                                egui::Slider::new(&mut input_volume, 0.0..=2.0)
                                    .show_value(true)
                                    .suffix("x"),
                            )
                            .changed()
                        {
                            // Note: We can't call send() in the closure due to borrow rules
                            // The slider change is handled below
                        }
                    });
                    // Handle volume change after the ui.horizontal closure
                    if input_volume != audio.input_volume {
                        self.backend.send(BackendCommand::SetInputVolume {
                            volume: input_volume,
                        });
                    }

                    ui.separator();

                    // Output device selection
                    ui.label("Output Device (Speakers):");
                    let current_output_name = audio
                        .selected_output_device()
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| "Default".to_string());

                    // Track selection change
                    let mut output_device_changed: Option<Option<String>> = None;

                    egui::ComboBox::from_id_salt("output_device")
                        .selected_text(&current_output_name)
                        .show_ui(ui, |ui| {
                            // Default option
                            if ui
                                .selectable_label(audio.selected_output_id.is_none(), "Default")
                                .clicked()
                            {
                                output_device_changed = Some(None);
                            }

                            for device in &audio.output_devices {
                                let label = if device.is_default {
                                    format!("{} (default)", device.name)
                                } else {
                                    device.name.clone()
                                };
                                if ui
                                    .selectable_label(
                                        audio.selected_output_id.as_ref() == Some(&device.id),
                                        &label,
                                    )
                                    .clicked()
                                {
                                    output_device_changed = Some(Some(device.id.clone()));
                                }
                            }
                        });

                    // Handle output device change
                    if let Some(new_selection) = output_device_changed {
                        self.backend.send(BackendCommand::SetOutputDevice {
                            device_id: new_selection,
                        });
                    }

                    // Output volume slider
                    let mut output_volume = audio.output_volume;
                    ui.horizontal(|ui| {
                        ui.label("Output Volume:");
                        ui.add(
                            egui::Slider::new(&mut output_volume, 0.0..=2.0)
                                .show_value(true)
                                .suffix("x"),
                        );
                    });
                    // Handle volume change after the ui.horizontal closure
                    if output_volume != audio.output_volume {
                        self.backend.send(BackendCommand::SetOutputVolume {
                            volume: output_volume,
                        });
                    }

                    ui.separator();

                    // Status info
                    ui.label("Push-to-talk: Hold SPACE to transmit");

                    if audio.is_capturing {
                        ui.colored_label(egui::Color32::GREEN, "🎤 Transmitting...");
                    } else {
                        ui.label("🔇 Not transmitting");
                    }

                    if audio.is_playing {
                        ui.label("🔊 Audio output active");
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
                                if let Some(uuid) = self.rename_modal.room_uuid {
                                    self.send_command(BackendCommand::RenameRoom {
                                        room_uuid: uuid,
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
