use eframe::egui;
use egui::{CollapsingHeader, Modal};
use uuid::Uuid;
use backend::Client;
use std::sync::Arc;
use tokio::sync::mpsc as tokio_mpsc;
use clap::Parser;

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

#[derive(Clone, Default)]
struct RoomUiState {
    current_room_id: Option<u64>,
}

/// Commands sent from UI to the async backend task
enum BackendCommand {
    Connect { addr: String, name: String, password: Option<String>, config: backend::ConnectConfig },
    Disconnect,
    SendChat { sender: String, text: String },
    JoinRoom { room_id: u64 },
    CreateRoom { name: String },
    DeleteRoom { room_id: u64 },
    RenameRoom { room_id: u64, new_name: String },
}

/// Events sent from backend task to UI
enum UiEvent {
    Connected,
    ConnectFailed(String),
    Disconnected,
    ChatMessage(String),
    SnapshotUpdated(backend::RoomSnapshot),
}

/// State for the rename room modal
#[derive(Default)]
struct RenameModalState {
    open: bool,
    room_id: u64,
    room_name: String,
}

struct MyApp {
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
    connected: bool,
    client_name: String,
    
    // Async communication channels (command_tx wrapped in Option for clean shutdown)
    command_tx: Option<tokio_mpsc::UnboundedSender<BackendCommand>>,
    ui_event_rx: std::sync::mpsc::Receiver<UiEvent>,
    
    // Rename modal state
    rename_modal: RenameModalState,
    
    egui_ctx: egui::Context,
    
    // Handle to the background thread for clean shutdown.
    backend_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MyApp {
    fn drop(&mut self) {
        // Drop command_tx first to signal the backend task to exit.
        // This closes the channel, causing command_rx.recv() to return None.
        drop(self.command_tx.take());
        
        // Wait for the backend thread to finish (it will disconnect the client).
        if let Some(thread) = self.backend_thread.take() {
            eprintln!("Waiting for backend thread to finish...");
            let _ = thread.join();
            eprintln!("Backend thread finished.");
        }
    }
}

impl MyApp {
    fn new(ctx: egui::Context, args: Args) -> Self {
        let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
        let (ui_event_tx, ui_event_rx) = std::sync::mpsc::channel();
        
        // Spawn the background async runtime and task
        let ctx_clone = ctx.clone();
        let backend_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async move {
                run_backend_task(command_rx, ui_event_tx, ctx_clone).await;
            });
        });
        
        // Use provided name or generate a random one
        let client_name = args.name.clone().unwrap_or_else(|| format!("user-{}", Uuid::new_v4().simple()));
        
        // Use provided server address or default empty
        let connect_address = args.server.clone().unwrap_or_default();
        
        // Use provided password or empty
        let connect_password = args.password.clone().unwrap_or_default();
        
        let command_tx = Some(command_tx);
        
        // If server was specified on command line, connect immediately
        if let Some(server_addr) = args.server {
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
            
            if let Some(tx) = &command_tx {
                let _ = tx.send(BackendCommand::Connect {
                    addr,
                    name: client_name.clone(),
                    password: args.password,
                    config,
                });
            }
        }
        
        Self {
            rooms: Vec::new(),
            presences: Vec::new(),
            room_ui: RoomUiState::default(),
            show_connect: false,
            show_settings: false,
            connect_address,
            connect_password,
            trust_dev_cert: args.trust_dev_cert,
            chat_messages: Vec::new(),
            chat_input: String::new(),
            connected: false,
            client_name,
            command_tx,
            ui_event_rx,
            rename_modal: RenameModalState::default(),
            egui_ctx: ctx,
            backend_thread: Some(backend_thread),
        }
    }
    
    /// Send a command to the backend task (non-blocking)
    fn send_command(&self, cmd: BackendCommand) {
        if let Some(tx) = &self.command_tx {
            let _ = tx.send(cmd);
        }
    }
}

/// Background task that owns the Client and processes commands
async fn run_backend_task(
    mut command_rx: tokio_mpsc::UnboundedReceiver<BackendCommand>,
    ui_event_tx: std::sync::mpsc::Sender<UiEvent>,
    ctx: egui::Context,
) {
    let mut client: Option<Client> = None;
    
    loop {
        tokio::select! {
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    BackendCommand::Connect { addr, name, password, config } => {
                        match Client::connect(&addr, &name, password.as_deref(), config).await {
                            Ok(mut c) => {
                                let events_rx = c.take_events_receiver();
                                let snap = c.get_snapshot().await;
                                let _ = ui_event_tx.send(UiEvent::Connected);
                                let _ = ui_event_tx.send(UiEvent::SnapshotUpdated(snap));
                                
                                // Spawn event listener
                                let tx = ui_event_tx.clone();
                                let ctx_ev = ctx.clone();
                                let snapshot = c.snapshot.clone();
                                tokio::spawn(async move {
                                    handle_server_events(events_rx, tx, ctx_ev, snapshot).await;
                                });
                                
                                client = Some(c);
                            }
                            Err(e) => {
                                let _ = ui_event_tx.send(UiEvent::ConnectFailed(e.to_string()));
                            }
                        }
                        ctx.request_repaint();
                    }
                    BackendCommand::Disconnect => {
                        if let Some(c) = client.take() {
                            if let Err(e) = c.disconnect().await {
                                eprintln!("disconnect error: {e}");
                            }
                            let _ = ui_event_tx.send(UiEvent::Disconnected);
                            ctx.request_repaint();
                        }
                    }
                    BackendCommand::SendChat { sender, text } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.send_chat(&sender, &text).await {
                                eprintln!("send_chat error: {e}");
                            }
                        }
                    }
                    BackendCommand::JoinRoom { room_id } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.join_room(room_id).await {
                                eprintln!("join_room error: {e}");
                            }
                            let snap = c.get_snapshot().await;
                            let _ = ui_event_tx.send(UiEvent::SnapshotUpdated(snap));
                            ctx.request_repaint();
                        }
                    }
                    BackendCommand::CreateRoom { name } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.create_room(&name).await {
                                eprintln!("create_room error: {e}");
                            }
                        }
                    }
                    BackendCommand::DeleteRoom { room_id } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.delete_room(room_id).await {
                                eprintln!("delete_room error: {e}");
                            }
                        }
                    }
                    BackendCommand::RenameRoom { room_id, new_name } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.rename_room(room_id, &new_name).await {
                                eprintln!("rename_room error: {e}");
                            }
                        }
                    }
                }
            }
            else => break,
        }
    }
    
    // Clean up: explicitly disconnect the client before exiting.
    // This ensures the server is notified even when the UI is closed abruptly.
    if let Some(c) = client.take() {
        eprintln!("Backend task exiting, disconnecting client...");
        let _ = c.disconnect().await;
    }
}

/// Handle incoming server events
async fn handle_server_events(
    mut events_rx: tokio::sync::mpsc::UnboundedReceiver<api::proto::ServerEvent>,
    ui_event_tx: std::sync::mpsc::Sender<UiEvent>,
    ctx: egui::Context,
    snapshot: Arc<tokio::sync::Mutex<backend::RoomSnapshot>>,
) {
    while let Some(ev) = events_rx.recv().await {
        match ev.kind {
            Some(api::proto::server_event::Kind::ChatBroadcast(cb)) => {
                let msg = format!("{}: {}", cb.sender, cb.text);
                let _ = ui_event_tx.send(UiEvent::ChatMessage(msg));
                ctx.request_repaint();
            }
            Some(api::proto::server_event::Kind::RoomStateUpdate(_)) => {
                let snap = snapshot.lock().await.clone();
                let _ = ui_event_tx.send(UiEvent::SnapshotUpdated(snap));
                ctx.request_repaint();
            }
            _ => {}
        }
    }
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.egui_ctx = ctx.clone();

        // Process incoming UI events (non-blocking)
        while let Ok(event) = self.ui_event_rx.try_recv() {
            match event {
                UiEvent::Connected => {
                    self.chat_messages.push(format!("Connected to {}", self.connect_address));
                    self.connected = true;
                }
                UiEvent::ConnectFailed(err) => {
                    self.chat_messages.push(format!("Failed to connect: {}", err));
                }
                UiEvent::Disconnected => {
                    self.chat_messages.push("Disconnected from server".to_string());
                    self.connected = false;
                    self.rooms.clear();
                    self.presences.clear();
                    self.room_ui.current_room_id = None;
                }
                UiEvent::ChatMessage(msg) => {
                    self.chat_messages.push(msg);
                }
                UiEvent::SnapshotUpdated(snap) => {
                    self.rooms = snap.rooms;
                    self.presences = snap.users;
                    self.room_ui.current_room_id = snap.current_room_id;
                }
            }
        }

        // Top menu
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Server", |ui| { 
                    if ui.button("Connect...").clicked() { 
                        self.show_connect = true; 
                        ui.close(); 
                    }
                    if self.connected {
                        if ui.button("Disconnect").clicked() {
                            self.send_command(BackendCommand::Disconnect);
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Settings", |ui| { if ui.button("Open Settings").clicked() { self.show_settings = true; ui.close(); } });
            });
        });

        // Chat panel
        egui::SidePanel::left("left_panel").default_width(320.0).show(ctx, |ui| {
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
                                if !text.is_empty() && self.connected {
                                    self.send_command(BackendCommand::SendChat {
                                        sender: self.client_name.clone(),
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
            
            if !self.connected {
                ui.label("Not connected. Use Server > Connect...");
            }
            if self.connected && self.rooms.is_empty() {
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
                for room in self.rooms.clone() {
                    let rid = room.id.as_ref().map(|r| r.value).unwrap_or(0);
                    let is_current = self.room_ui.current_room_id == Some(rid);
                    let mut text = room.name.clone();
                    if is_current { text.push_str("  (current)"); }
                    let resp = ui.selectable_label(is_current, text);
                    if resp.clicked() {
                        self.send_command(BackendCommand::JoinRoom { room_id: rid });
                        self.room_ui.current_room_id = Some(rid);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Join").clicked() {
                            self.send_command(BackendCommand::JoinRoom { room_id: rid });
                            self.room_ui.current_room_id = Some(rid);
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
                            self.send_command(BackendCommand::CreateRoom { name: "New Room".to_string() });
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
                        
                        self.send_command(BackendCommand::Connect { addr, name, password, config });
                        ui.close();
                    }
                    if ui.button("Cancel").clicked() { ui.close(); }
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
        
        // Rename room modal
        if self.rename_modal.open {
            let modal = Modal::new(egui::Id::new("rename_modal")).show(ctx, |ui| {
                ui.set_width(280.0);
                ui.heading("Rename Room");
                ui.label("New name:");
                ui.text_edit_singleline(&mut self.rename_modal.room_name);
                ui.separator();
                egui::Sides::new().show(ui, |_l| {}, |ui| {
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
                    if ui.button("Cancel").clicked() { ui.close(); }
                });
            });
            if modal.should_close() { self.rename_modal.open = false; }
        }
    }
}
