use eframe::egui;
use egui::{CollapsingHeader, Modal, RichText, Ui};
use uuid::Uuid;
use backend::Client;
use tokio::runtime::Runtime;

fn main() -> eframe::Result<()> {
    env_logger::init();
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
            Ok(Box::new(MyApp::new(cc.egui_ctx.clone())))
        }),
    )
}

#[derive(Clone, Default)]
struct RoomUiState {
    current_room_id: Option<u64>,
}

struct MyApp {
    name: String,
    rooms: Vec<api::proto::RoomInfo>,
    presences: Vec<api::proto::UserPresence>,
    room_ui: RoomUiState,
    show_connect: bool,
    show_settings: bool,
    connect_address: String,
    connect_password: String,
    trust_dev_cert: bool,
    chat_messages: Vec<String>,
    chat_input: String,
    rt: Runtime,
    client: Option<Client>,
    client_name: String,
    chat_incoming_tx: std::sync::mpsc::Sender<String>,
    chat_incoming_rx: std::sync::mpsc::Receiver<String>,
    egui_ctx: egui::Context,
}

impl MyApp {
    fn new(ctx: egui::Context) -> Self {
        let (chat_incoming_tx, chat_incoming_rx) = std::sync::mpsc::channel();
        Self {
            name: "Rumble".into(),
            rooms: Vec::new(),
            presences: Vec::new(),
            room_ui: RoomUiState::default(),
            show_connect: false,
            show_settings: false,
            connect_address: String::new(),
            connect_password: String::new(),
            trust_dev_cert: true,
            chat_messages: Vec::new(),
            chat_input: String::new(),
            rt: Runtime::new().expect("create tokio runtime"),
            client: None,
            client_name: format!("user-{}", Uuid::new_v4().simple()),
            chat_incoming_tx,
            chat_incoming_rx,
            egui_ctx: ctx,
        }
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.egui_ctx = ctx.clone();

        // Top menu
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Server", |ui| { if ui.button("Connect...").clicked() { self.show_connect = true; ui.close(); } });
                ui.menu_button("Settings", |ui| { if ui.button("Open Settings").clicked() { self.show_settings = true; ui.close(); } });
            });
        });

        // Chat panel
        egui::SidePanel::left("left_panel").default_width(320.0).show(ctx, |ui| {
            while let Ok(msg) = self.chat_incoming_rx.try_recv() { self.chat_messages.push(msg); }
            egui_extras::StripBuilder::new(ui)
                .size(egui_extras::Size::remainder())
                .size(egui_extras::Size::exact(4.0))
                .size(egui_extras::Size::exact(28.0))
                .vertical(|mut strip| {
                    strip.cell(|ui| {
                        ui.heading("Text Chat"); ui.separator();
                        egui::ScrollArea::vertical().auto_shrink([false; 2]).stick_to_bottom(true).show(ui, |ui| {
                            for msg in &self.chat_messages { ui.label(msg); }
                        });
                    });
                    strip.cell(|ui| { ui.add_space(2.0); ui.separator(); });
                    strip.cell(|ui| {
                        ui.horizontal(|ui| {
                            let send = {
                                let resp = ui.text_edit_singleline(&mut self.chat_input);
                                resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            } || ui.button("Send").clicked();
                            if send {
                                let text = self.chat_input.trim();
                                if !text.is_empty() {
                                    let to_send = text.to_owned();
                                    if let Some(client) = &mut self.client {
                                        let name = self.client_name.clone();
                                        let _ = self.rt.block_on(async { client.send_chat(&name, &to_send).await });
                                    }
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
            // Pull latest snapshot from client
            if let Some(client) = &self.client {
                let snap = self.rt.block_on(async { client.get_snapshot().await });
                self.rooms = snap.rooms.clone();
                self.presences = snap.users.clone();
                self.room_ui.current_room_id = snap.current_room_id;
            }
            if self.client.is_none() {
                ui.label("Not connected. Use Server > Connect...");
            }
            if self.client.is_some() && self.rooms.is_empty() {
                ui.horizontal(|ui| {
                    ui.label("No rooms received yet.");
                    if ui.button("Join Root").clicked() {
                        if let Some(client) = &self.client {
                            let _ = self.rt.block_on(async { client.join_room(1).await });
                        }
                    }
                    if ui.button("Refresh").clicked() {
                        if let Some(client) = &self.client {
                            // Nudge server by joining root (1) to trigger state push
                            let _ = self.rt.block_on(async { client.join_room(1).await });
                        }
                    }
                });
                ui.separator();
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for room in &self.rooms {
                    let rid = room.id.as_ref().map(|r| r.value).unwrap_or(0);
                    let is_current = self.room_ui.current_room_id == Some(rid);
                    let mut text = room.name.clone();
                    if is_current { text.push_str("  (current)"); }
                    let resp = ui.selectable_label(is_current, text);
                    if resp.clicked() {
                        if let Some(client) = &self.client {
                            let _ = self.rt.block_on(async { client.join_room(rid).await });
                        }
                        self.room_ui.current_room_id = Some(rid);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Join").clicked() {
                            if let Some(client) = &self.client {
                                let _ = self.rt.block_on(async { client.join_room(rid).await });
                            }
                            self.room_ui.current_room_id = Some(rid);
                            ui.close_menu();
                        }
                        if ui.button("Rename").clicked() {
                            // capture new name via text edit modal; for now, inline prompt
                            let mut new_name = room.name.clone();
                            ui.text_edit_singleline(&mut new_name);
                            if ui.button("Apply").clicked() {
                                if let Some(client) = &self.client {
                                    let _ = self.rt.block_on(async { client.rename_room(rid, &new_name).await });
                                }
                                ui.close_menu();
                            }
                        }
                        if ui.button("Add Room").clicked() {
                            if let Some(client) = &self.client {
                                let _ = self.rt.block_on(async { client.create_room("New Room").await });
                            }
                            ui.close_menu();
                        }
                        if rid != 1 && ui.button("Delete Room").clicked() {
                            if let Some(client) = &self.client {
                                let _ = self.rt.block_on(async { client.delete_room(rid).await });
                            }
                            ui.close_menu();
                        }
                    });
                    // Show users in this room
                    CollapsingHeader::new("Users").default_open(is_current).show(ui, |ui| {
                        for up in self.presences.iter().filter(|u| u.room_id.as_ref().map(|r| r.value) == Some(rid)) {
                            ui.label(&up.username);
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
                ui.label("Server address:"); ui.text_edit_singleline(&mut self.connect_address);
                ui.label("Password (optional):"); ui.text_edit_singleline(&mut self.connect_password);
                ui.checkbox(&mut self.trust_dev_cert, "Trust dev cert (dev-certs/server-cert.der)");
                ui.separator();
                egui::Sides::new().show(ui, |_l| {}, |ui| {
                    let mut should_close = false;
                    if ui.button("Connect").clicked() {
                        let addr = if self.connect_address.trim().is_empty() { "127.0.0.1:5000".to_string() } else { self.connect_address.trim().to_string() };
                        let name = self.client_name.clone();
                        let password = if self.connect_password.trim().is_empty() { None } else { Some(self.connect_password.trim().to_string()) };
                        let trust_dev = self.trust_dev_cert;
                        let handle = self.rt.handle().clone();
                        let (tx, rx) = std::sync::mpsc::channel();
                        let addr_for_log = addr.clone();
                        
                        // Build connect config based on UI settings
                        let mut config = backend::ConnectConfig::new();
                        if trust_dev {
                            // Add the dev certificate path
                            config = config.with_cert("dev-certs/server-cert.der");
                        }
                        // Also check environment variable for additional cert path
                        if let Ok(cert_path) = std::env::var("RUMBLE_SERVER_CERT_PATH") {
                            config = config.with_cert(cert_path);
                        }
                        
                        std::thread::spawn(move || {
                            handle.block_on(async move {
                                let c = Client::connect(&addr, &name, password.as_deref(), config).await;
                                if let Err(e) = &c { eprintln!("connect error: {e:?}"); }
                                let _ = tx.send(c);
                            });
                        });
                        match rx.recv() {
                            Ok(Ok(mut c)) => {
                                self.chat_messages.push(format!("Connected to {}", addr_for_log));
                                let events_rx = c.take_events_receiver();
                                self.client = Some(c);
                                let forward_tx = self.chat_incoming_tx.clone();
                                let ctx_clone = ctx.clone();
                                std::thread::spawn(move || {
                                    let rt = tokio::runtime::Runtime::new().expect("bg runtime");
                                    rt.block_on(async move {
                                        let mut rx_events = events_rx;
                                        while let Some(ev) = rx_events.recv().await {
                                            match ev.kind {
                                                Some(api::proto::server_event::Kind::ChatBroadcast(cb)) => {
                                                    let _ = forward_tx.send(format!("{}: {}", cb.sender, cb.text));
                                                    ctx_clone.request_repaint();
                                                }
                                                Some(api::proto::server_event::Kind::RoomStateUpdate(_)) => {
                                                    // Trigger repaint so CentralPanel pulls fresh snapshot
                                                    ctx_clone.request_repaint();
                                                }
                                                _ => {}
                                            }
                                        }
                                    });
                                });
                            }
                            Ok(Err(e)) => { self.chat_messages.push(format!("Failed to connect: {e}")); }
                            Err(e) => { self.chat_messages.push(format!("Connect channel error: {e}")); }
                        }
                        should_close = true;
                    }
                    if ui.button("Cancel").clicked() { ui.close(); }
                    if should_close { ui.close(); }
                });
            });
            if modal.should_close() { self.show_connect = false; }
        }

        // Settings modal
        if self.show_settings {
            let modal = Modal::new(egui::Id::new("settings_modal")).show(ctx, |ui| {
                ui.set_width(320.0);
                ui.heading("Settings");
                ui.label("server url:"); ui.label(self.connect_address.as_str());
                ui.separator();
                egui::Sides::new().show(ui, |_l| {}, |ui| { if ui.button("Close").clicked() { ui.close(); } });
            });
            if modal.should_close() { self.show_settings = false; }
        }
    }
}
