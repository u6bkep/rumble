//! Daemon process for managing GUI test instances.
//!
//! The daemon:
//! - Listens on a Unix socket for commands
//! - Manages the Rumble server instance
//! - Manages multiple GUI client instances
//! - Handles headless rendering for screenshots

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use anyhow::{Context, Result};
use eframe::egui;
use egui_kittest::kittest::{self, AccessKitNode, NodeT, Queryable};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::RwLock,
};
use tracing::{debug, error, info};

use egui_test::{Args, HotkeyBinding, HotkeyModifiers, RumbleApp};

/// A simple wrapper around AccessKitNode that implements NodeT for querying.
#[derive(Clone)]
struct QueryNode<'a> {
    node: AccessKitNode<'a>,
}

impl std::fmt::Debug for QueryNode<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        kittest::debug_fmt_node(self, f)
    }
}

impl<'tree> NodeT<'tree> for QueryNode<'tree> {
    fn accesskit_node(&self) -> AccessKitNode<'tree> {
        self.node.clone()
    }

    fn new_related(&self, child_node: AccessKitNode<'tree>) -> Self {
        Self { node: child_node }
    }
}

use crate::{
    protocol::{ClientInfo, Command, Response, ResponseData},
    renderer::Renderer,
};

/// Default socket path for the daemon.
pub fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    runtime_dir.join("rumble-harness.sock")
}

/// A managed GUI client instance.
struct ClientInstance {
    id: u32,
    name: String,
    ctx: egui::Context,
    app: RumbleApp,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    width: u32,
    height: u32,
    /// Current mouse position
    mouse_pos: egui::Pos2,
    /// Last frame's output for screenshots
    last_output: Option<egui::FullOutput>,
    /// wgpu renderer for screenshots
    renderer: Renderer,
    /// kittest state for widget queries (AccessKit-based)
    kittest_state: Option<kittest::State>,
}

impl ClientInstance {
    fn new(id: u32, args: Args) -> Result<Self> {
        let name = args.name.clone().unwrap_or_else(|| format!("client-{}", id));

        // Create a dedicated runtime for this client
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("Failed to create Tokio runtime")?;

        let ctx = egui::Context::default();

        // Enable AccessKit for widget queries
        ctx.enable_accesskit();

        // Set up screen size
        let width = 1280u32;
        let height = 720u32;

        let app = RumbleApp::new(ctx.clone(), runtime.handle().clone(), args);

        Ok(Self {
            id,
            name,
            ctx,
            app,
            runtime,
            width,
            height,
            mouse_pos: egui::Pos2::ZERO,
            last_output: None,
            renderer: Renderer::new(),
            kittest_state: None,
        })
    }

    fn run_frame(&mut self) {
        let screen_rect =
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(self.width as f32, self.height as f32));

        let mut raw_input = egui::RawInput::default();
        raw_input.screen_rect = Some(screen_rect);
        raw_input.max_texture_side = Some(8192);
        raw_input.time = Some(std::time::Instant::now().elapsed().as_secs_f64());

        let mut output = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });

        // Handle texture updates for the wgpu renderer
        self.renderer.handle_delta(&output.textures_delta);

        // Update kittest state for widget queries
        if let Some(accesskit_update) = output.platform_output.accesskit_update.take() {
            match &mut self.kittest_state {
                Some(state) => state.update(accesskit_update),
                None => self.kittest_state = Some(kittest::State::new(accesskit_update)),
            }
        }

        // Store output for screenshots
        self.last_output = Some(output);
    }

    fn click(&mut self, x: f32, y: f32) {
        let pos = egui::pos2(x, y);
        self.mouse_pos = pos;

        // Press
        let mut raw_input = egui::RawInput::default();
        raw_input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(self.width as f32, self.height as f32),
        ));
        raw_input.events.push(egui::Event::PointerMoved(pos));
        raw_input.events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });

        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });

        // Release
        let mut raw_input = egui::RawInput::default();
        raw_input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(self.width as f32, self.height as f32),
        ));
        raw_input.events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });

        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    fn mouse_move(&mut self, x: f32, y: f32) {
        let pos = egui::pos2(x, y);
        self.mouse_pos = pos;

        let mut raw_input = egui::RawInput::default();
        raw_input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(self.width as f32, self.height as f32),
        ));
        raw_input.events.push(egui::Event::PointerMoved(pos));

        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    fn key_event(&mut self, key: egui::Key, pressed: bool) {
        let mut raw_input = egui::RawInput::default();
        raw_input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(self.width as f32, self.height as f32),
        ));
        raw_input.events.push(egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });

        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    fn type_text(&mut self, text: &str) {
        let mut raw_input = egui::RawInput::default();
        raw_input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(self.width as f32, self.height as f32),
        ));
        raw_input.events.push(egui::Event::Text(text.to_string()));

        let _ = self.ctx.run(raw_input, |ctx| {
            self.app.render(ctx);
        });
    }

    fn screenshot(
        &mut self,
        output_path: Option<&str>,
        crop: Option<&crate::protocol::CropRect>,
    ) -> Result<(String, u32, u32)> {
        // Run a frame first to ensure we have current content
        self.run_frame();

        let output = self
            .last_output
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No frame output available"))?;

        let image = self
            .renderer
            .render(&self.ctx, output)
            .map_err(|e| anyhow::anyhow!(e))?;

        let (final_image, out_width, out_height) = if let Some(crop) = crop {
            let (img_w, img_h) = image.dimensions();
            anyhow::ensure!(
                crop.x < img_w && crop.y < img_h,
                "Crop origin ({},{}) is outside image bounds ({}x{})",
                crop.x,
                crop.y,
                img_w,
                img_h
            );

            // Clamp width/height to image bounds
            let cw = crop.width.min(img_w - crop.x);
            let ch = crop.height.min(img_h - crop.y);

            let cropped = image::imageops::crop_imm(&image, crop.x, crop.y, cw, ch).to_image();
            (cropped, cw, ch)
        } else {
            let w = self.width;
            let h = self.height;
            (image, w, h)
        };

        let mut png_data = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_data);
        final_image.write_with_encoder(encoder)?;

        let path = if let Some(p) = output_path {
            PathBuf::from(p)
        } else {
            let dir = std::env::temp_dir().join("rumble-harness");
            std::fs::create_dir_all(&dir)?;
            dir.join(format!("screenshot-{}-{}.png", self.id, chrono_timestamp()))
        };

        std::fs::write(&path, &png_data)?;

        Ok((path.to_string_lossy().to_string(), out_width, out_height))
    }

    fn is_connected(&self) -> bool {
        self.app.is_connected()
    }

    /// Check if a widget with the given label exists.
    fn has_widget(&self, label: &str) -> bool {
        self.kittest_state
            .as_ref()
            .and_then(|state| {
                let root = QueryNode { node: state.root() };
                root.query_by_label(label)
            })
            .is_some()
    }

    /// Get the bounding rectangle of a widget by label.
    fn widget_rect(&self, label: &str) -> Option<egui::Rect> {
        self.kittest_state.as_ref().and_then(|state| {
            let root = QueryNode { node: state.root() };
            root.query_by_label(label).map(|node| {
                let rect = node
                    .accesskit_node()
                    .bounding_box()
                    .expect("Widget should have bounding box");
                egui::Rect::from_min_max(
                    egui::pos2(rect.x0 as f32, rect.y0 as f32),
                    egui::pos2(rect.x1 as f32, rect.y1 as f32),
                )
            })
        })
    }

    /// Click a widget by its accessible label.
    fn click_widget(&mut self, label: &str) -> bool {
        if let Some(rect) = self.widget_rect(label) {
            let center = rect.center();
            self.click(center.x, center.y);
            true
        } else {
            false
        }
    }

    /// Run frames until UI settles (no pending repaints).
    /// Returns the number of frames that were run.
    fn run_until_stable(&mut self) -> u32 {
        const MAX_FRAMES: u32 = 500;
        const STABLE_THRESHOLD: u32 = 3; // Need this many stable frames in a row

        let mut stable_count = 0u32;
        let mut frames_run = 0u32;

        for _ in 0..MAX_FRAMES {
            self.run_frame();
            frames_run += 1;

            // Check if UI wants to repaint soon via viewport output
            // If repaint_delay is long (> 1 second), UI is stable
            let wants_repaint = self
                .last_output
                .as_ref()
                .and_then(|o| o.viewport_output.get(&egui::ViewportId::ROOT))
                .map(|v| v.repaint_delay < std::time::Duration::from_secs(1))
                .unwrap_or(true);

            if !wants_repaint {
                stable_count += 1;
                if stable_count >= STABLE_THRESHOLD {
                    break;
                }
            } else {
                stable_count = 0;
            }
        }

        frames_run
    }

    fn info(&self) -> ClientInfo {
        ClientInfo {
            id: self.id,
            name: self.name.clone(),
            connected: self.is_connected(),
        }
    }

    /// Enable or disable auto-download.
    fn set_auto_download(&mut self, enabled: bool) {
        use backend::Command;
        let backend = self.app.backend();

        // Get current settings and update
        let state = backend.state();
        let mut settings = state.file_transfer_settings.clone();
        settings.auto_download_enabled = enabled;

        // Send the updated settings to the backend
        backend.send(Command::UpdateFileTransferSettings { settings });
    }

    /// Set auto-download rules.
    fn set_auto_download_rules(&mut self, rules: Vec<crate::protocol::AutoDownloadRuleConfig>) {
        use backend::{AutoDownloadRule, Command};
        let backend = self.app.backend();

        // Get current settings and update
        let state = backend.state();
        let mut settings = state.file_transfer_settings.clone();
        settings.auto_download_rules = rules
            .into_iter()
            .map(|r| AutoDownloadRule {
                mime_pattern: r.mime_pattern,
                max_size_bytes: r.max_size_bytes,
            })
            .collect();

        // Send the updated settings to the backend
        backend.send(Command::UpdateFileTransferSettings { settings });
    }

    /// Get file transfer settings.
    fn get_file_transfer_settings(&self) -> (bool, Vec<crate::protocol::AutoDownloadRuleConfig>) {
        let backend = self.app.backend();
        let state = backend.state();
        let settings = &state.file_transfer_settings;

        let rules = settings
            .auto_download_rules
            .iter()
            .map(|r| crate::protocol::AutoDownloadRuleConfig {
                mime_pattern: r.mime_pattern.clone(),
                max_size_bytes: r.max_size_bytes,
            })
            .collect();

        (settings.auto_download_enabled, rules)
    }

    /// Share a file via the backend.
    fn share_file(&mut self, path: &str) -> anyhow::Result<(String, String)> {
        use backend::Command;

        // Send share command to backend
        self.app.backend().send(Command::ShareFile {
            path: std::path::PathBuf::from(path),
        });

        // Run frames to let the share complete
        for _ in 0..50 {
            self.run_frame();
            // Small sleep to allow async operations
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Check if we have a file transfer
        let state = self.app.backend().state();
        if let Some(transfer) = state.file_transfers.first() {
            let infohash = hex::encode(transfer.infohash);
            let magnet = transfer.magnet.clone().unwrap_or_default();
            Ok((infohash, magnet))
        } else {
            Err(anyhow::anyhow!("File share did not produce a transfer"))
        }
    }

    /// Get list of file transfers.
    fn get_file_transfers(&self) -> Vec<crate::protocol::FileTransferInfo> {
        use backend::TransferState;
        let backend = self.app.backend();
        let state = backend.state();

        state
            .file_transfers
            .iter()
            .map(|t| crate::protocol::FileTransferInfo {
                infohash: hex::encode(t.infohash),
                name: t.name.clone(),
                size: t.size,
                progress: t.progress,
                state: format!("{:?}", t.state),
                is_downloading: matches!(t.state, TransferState::Downloading),
            })
            .collect()
    }
}

fn chrono_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse a key name string to egui::Key.
fn parse_key(key_name: &str) -> Option<egui::Key> {
    match key_name.to_lowercase().as_str() {
        "space" => Some(egui::Key::Space),
        "enter" | "return" => Some(egui::Key::Enter),
        "escape" | "esc" => Some(egui::Key::Escape),
        "tab" => Some(egui::Key::Tab),
        "backspace" => Some(egui::Key::Backspace),
        "delete" => Some(egui::Key::Delete),
        "insert" => Some(egui::Key::Insert),
        "home" => Some(egui::Key::Home),
        "end" => Some(egui::Key::End),
        "pageup" => Some(egui::Key::PageUp),
        "pagedown" => Some(egui::Key::PageDown),
        "left" => Some(egui::Key::ArrowLeft),
        "right" => Some(egui::Key::ArrowRight),
        "up" => Some(egui::Key::ArrowUp),
        "down" => Some(egui::Key::ArrowDown),
        "a" => Some(egui::Key::A),
        "b" => Some(egui::Key::B),
        "c" => Some(egui::Key::C),
        "d" => Some(egui::Key::D),
        "e" => Some(egui::Key::E),
        "f" => Some(egui::Key::F),
        "g" => Some(egui::Key::G),
        "h" => Some(egui::Key::H),
        "i" => Some(egui::Key::I),
        "j" => Some(egui::Key::J),
        "k" => Some(egui::Key::K),
        "l" => Some(egui::Key::L),
        "m" => Some(egui::Key::M),
        "n" => Some(egui::Key::N),
        "o" => Some(egui::Key::O),
        "p" => Some(egui::Key::P),
        "q" => Some(egui::Key::Q),
        "r" => Some(egui::Key::R),
        "s" => Some(egui::Key::S),
        "t" => Some(egui::Key::T),
        "u" => Some(egui::Key::U),
        "v" => Some(egui::Key::V),
        "w" => Some(egui::Key::W),
        "x" => Some(egui::Key::X),
        "y" => Some(egui::Key::Y),
        "z" => Some(egui::Key::Z),
        "0" => Some(egui::Key::Num0),
        "1" => Some(egui::Key::Num1),
        "2" => Some(egui::Key::Num2),
        "3" => Some(egui::Key::Num3),
        "4" => Some(egui::Key::Num4),
        "5" => Some(egui::Key::Num5),
        "6" => Some(egui::Key::Num6),
        "7" => Some(egui::Key::Num7),
        "8" => Some(egui::Key::Num8),
        "9" => Some(egui::Key::Num9),
        "f1" => Some(egui::Key::F1),
        "f2" => Some(egui::Key::F2),
        "f3" => Some(egui::Key::F3),
        "f4" => Some(egui::Key::F4),
        "f5" => Some(egui::Key::F5),
        "f6" => Some(egui::Key::F6),
        "f7" => Some(egui::Key::F7),
        "f8" => Some(egui::Key::F8),
        "f9" => Some(egui::Key::F9),
        "f10" => Some(egui::Key::F10),
        "f11" => Some(egui::Key::F11),
        "f12" => Some(egui::Key::F12),
        _ => None,
    }
}

/// Server process handle.
struct ServerHandle {
    /// Shutdown channel
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Path to the server's certificate
    cert_path: PathBuf,
}

/// Daemon state shared across connections.
struct DaemonState {
    clients: HashMap<u32, ClientInstance>,
    next_client_id: AtomicU32,
    server_handle: Option<ServerHandle>,
    server_port: u16,
    /// Path to the server's certificate (if server was started by daemon)
    server_cert_path: Option<PathBuf>,
}

impl DaemonState {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
            next_client_id: AtomicU32::new(1),
            server_handle: None,
            server_port: 0,
            server_cert_path: None,
        }
    }
}

/// The daemon server.
pub struct Daemon {
    socket_path: PathBuf,
    state: Arc<RwLock<DaemonState>>,
}

impl Daemon {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            state: Arc::new(RwLock::new(DaemonState::new())),
        }
    }

    /// Run the daemon, listening for connections.
    pub async fn run(&self) -> Result<()> {
        // Clean up old socket
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        let listener = UnixListener::bind(&self.socket_path).context("Failed to bind Unix socket")?;

        info!("Daemon listening on {:?}", self.socket_path);

        // Handle shutdown signals
        let state = self.state.clone();
        let shutdown_state = state.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("Received shutdown signal");
            // Cleanup will happen when daemon exits
            let _ = shutdown_state;
        });

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let state = self.state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, state).await {
                            error!("Connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Accept error: {}", e);
                }
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Clean up socket file
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Handle a single client connection.
async fn handle_connection(stream: UnixStream, state: Arc<RwLock<DaemonState>>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF
            break;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        debug!("Received command: {}", line);

        let response = match serde_json::from_str::<Command>(line) {
            Ok(cmd) => process_command(cmd, &state).await,
            Err(e) => Response::error(format!("Invalid command: {}", e)),
        };

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        // Check if this was a shutdown command
        if matches!(
            response,
            Response::Ok {
                data: ResponseData::Ack
            }
        ) {
            if let Ok(Command::Shutdown) = serde_json::from_str::<Command>(line) {
                info!("Shutdown requested, exiting...");
                std::process::exit(0);
            }
        }
    }

    Ok(())
}

/// Process a single command.
async fn process_command(cmd: Command, state: &Arc<RwLock<DaemonState>>) -> Response {
    match cmd {
        Command::Ping => Response::ok(ResponseData::Pong),

        Command::Shutdown => Response::ack(),

        Command::Status => {
            let state = state.read().await;
            Response::ok(ResponseData::Status {
                server_running: state.server_handle.is_some(),
                client_count: state.clients.len() as u32,
            })
        }

        Command::ServerStart { port } => {
            let mut state = state.write().await;
            if state.server_handle.is_some() {
                return Response::error("Server already running");
            }

            // Start the server in a background task
            match start_server(port).await {
                Ok(handle) => {
                    state.server_cert_path = Some(handle.cert_path.clone());
                    state.server_handle = Some(handle);
                    state.server_port = port;
                    Response::ack()
                }
                Err(e) => Response::error(format!("Failed to start server: {}", e)),
            }
        }

        Command::ServerStop => {
            let mut state = state.write().await;
            if let Some(handle) = state.server_handle.take() {
                if let Some(tx) = handle.shutdown_tx {
                    let _ = tx.send(());
                }
                Response::ack()
            } else {
                Response::error("Server not running")
            }
        }

        Command::ClientNew { name, server } => {
            let mut state = state.write().await;
            let id = state.next_client_id.fetch_add(1, Ordering::SeqCst);

            // Pass the server cert path if the daemon started the server
            let cert = state.server_cert_path.as_ref().map(|p| p.to_string_lossy().to_string());

            let args = Args {
                name,
                server,
                cert,
                trust_dev_cert: false, // Not needed if we pass the actual cert
                ..Default::default()
            };

            match ClientInstance::new(id, args) {
                Ok(client) => {
                    state.clients.insert(id, client);
                    Response::ok(ResponseData::ClientCreated { id })
                }
                Err(e) => Response::error(format!("Failed to create client: {}", e)),
            }
        }

        Command::ClientList => {
            let state = state.read().await;
            let clients: Vec<ClientInfo> = state.clients.values().map(|c| c.info()).collect();
            Response::ok(ResponseData::ClientList { clients })
        }

        Command::ClientClose { id } => {
            let mut state = state.write().await;
            if state.clients.remove(&id).is_some() {
                Response::ack()
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::Screenshot { id, output, crop } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                match client.screenshot(output.as_deref(), crop.as_ref()) {
                    Ok((path, width, height)) => Response::ok(ResponseData::Screenshot { path, width, height }),
                    Err(e) => Response::error(format!("Screenshot failed: {}", e)),
                }
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::Click { id, x, y } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                client.click(x, y);
                Response::ack()
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::MouseMove { id, x, y } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                client.mouse_move(x, y);
                Response::ack()
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::KeyPress { id, key } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                if let Some(k) = parse_key(&key) {
                    client.key_event(k, true);
                    Response::ack()
                } else {
                    Response::error(format!("Unknown key: {}", key))
                }
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::KeyRelease { id, key } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                if let Some(k) = parse_key(&key) {
                    client.key_event(k, false);
                    Response::ack()
                } else {
                    Response::error(format!("Unknown key: {}", key))
                }
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::KeyTap { id, key } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                if let Some(k) = parse_key(&key) {
                    client.key_event(k, true);
                    client.key_event(k, false);
                    Response::ack()
                } else {
                    Response::error(format!("Unknown key: {}", key))
                }
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::TypeText { id, text } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                client.type_text(&text);
                Response::ack()
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::RunFrames { id, count } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                for _ in 0..count {
                    client.run_frame();
                }
                Response::ok(ResponseData::FramesRun { count })
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::IsConnected { id } => {
            let state = state.read().await;
            if let Some(client) = state.clients.get(&id) {
                Response::ok(ResponseData::Connected {
                    connected: client.is_connected(),
                })
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::GetState { id } => {
            let state = state.read().await;
            if let Some(client) = state.clients.get(&id) {
                // Get backend state and serialize to JSON
                let backend = client.app.backend();
                let backend_state = backend.state();

                // Create a simplified state representation
                let state_json = serde_json::json!({
                    "connected": client.is_connected(),
                    "rooms": backend_state.rooms.iter().map(|r| {
                        let room_uuid = r.id.as_ref().and_then(api::uuid_from_room_id);
                        let parent_uuid = r.parent_id.as_ref().and_then(api::uuid_from_room_id);
                        serde_json::json!({
                            "uuid": room_uuid.map(|u| u.to_string()),
                            "name": r.name,
                            "parent_uuid": parent_uuid.map(|u| u.to_string()),
                        })
                    }).collect::<Vec<_>>(),
                    "users": backend_state.users.iter().map(|u| {
                        let room_uuid = u.current_room.as_ref().and_then(api::uuid_from_room_id);
                        serde_json::json!({
                            "id": u.user_id.as_ref().map(|id| id.value),
                            "name": u.username,
                            "room_uuid": room_uuid.map(|uuid| uuid.to_string()),
                        })
                    }).collect::<Vec<_>>(),
                    "my_user_id": backend_state.my_user_id,
                    "audio": {
                        "self_muted": backend_state.audio.self_muted,
                        "self_deafened": backend_state.audio.self_deafened,
                        "is_transmitting": backend_state.audio.is_transmitting,
                    },
                });

                Response::ok(ResponseData::State { state: state_json })
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::ClickWidget { id, label } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                // Run a frame first to ensure kittest state is initialized
                client.run_frame();
                if client.click_widget(&label) {
                    Response::ack()
                } else {
                    Response::error(format!("Widget '{}' not found", label))
                }
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::HasWidget { id, label } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                // Run a frame first to ensure kittest state is initialized
                client.run_frame();
                Response::ok(ResponseData::WidgetExists {
                    exists: client.has_widget(&label),
                })
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::WidgetRect { id, label } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                // Run a frame first to ensure kittest state is initialized
                client.run_frame();
                if let Some(rect) = client.widget_rect(&label) {
                    Response::ok(ResponseData::WidgetRect {
                        found: true,
                        x: Some(rect.min.x),
                        y: Some(rect.min.y),
                        width: Some(rect.width()),
                        height: Some(rect.height()),
                    })
                } else {
                    Response::ok(ResponseData::WidgetRect {
                        found: false,
                        x: None,
                        y: None,
                        width: None,
                        height: None,
                    })
                }
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::Run { id } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                let count = client.run_until_stable();
                Response::ok(ResponseData::FramesRun { count })
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::SetAutoDownload { id, enabled } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                client.set_auto_download(enabled);
                client.run_frame();
                Response::ack()
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::SetAutoDownloadRules { id, rules } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                client.set_auto_download_rules(rules);
                client.run_frame();
                Response::ack()
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::GetFileTransferSettings { id } => {
            let state = state.read().await;
            if let Some(client) = state.clients.get(&id) {
                let (enabled, rules) = client.get_file_transfer_settings();
                Response::ok(ResponseData::FileTransferSettings {
                    auto_download_enabled: enabled,
                    auto_download_rules: rules,
                })
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::SetHotkey { id, action, key } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                let binding = key.map(|k| HotkeyBinding {
                    modifiers: HotkeyModifiers::default(),
                    key: k,
                });

                let settings = client.app.persistent_settings_mut();
                match action.as_str() {
                    "ptt" => {
                        settings.keyboard.ptt_hotkey = binding;
                        Response::ack()
                    }
                    "mute" => {
                        settings.keyboard.toggle_mute_hotkey = binding;
                        Response::ack()
                    }
                    "deafen" => {
                        settings.keyboard.toggle_deafen_hotkey = binding;
                        Response::ack()
                    }
                    _ => Response::error(format!("Unknown action '{}'. Use 'ptt', 'mute', or 'deafen'.", action)),
                }
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::ShareFile { id, path } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                match client.share_file(&path) {
                    Ok((infohash, magnet_link)) => {
                        // Run some frames to process the share
                        for _ in 0..10 {
                            client.run_frame();
                        }
                        Response::ok(ResponseData::FileShared { infohash, magnet_link })
                    }
                    Err(e) => Response::error(format!("Failed to share file: {}", e)),
                }
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::GetFileTransfers { id } => {
            let state = state.read().await;
            if let Some(client) = state.clients.get(&id) {
                let transfers = client.get_file_transfers();
                Response::ok(ResponseData::FileTransfers { transfers })
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }

        Command::ShowTransfers { id, show } => {
            let mut state = state.write().await;
            if let Some(client) = state.clients.get_mut(&id) {
                client.app.set_show_transfers(show);
                client.run_frame();
                Response::ack()
            } else {
                Response::error(format!("Client {} not found", id))
            }
        }
    }
}

/// Start the Rumble server.
async fn start_server(port: u16) -> Result<ServerHandle> {
    use server::{Config, Server, generate_self_signed_cert};

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

    // Generate self-signed cert in a temp directory for testing
    let cert_dir = std::env::temp_dir().join("rumble-harness-certs");
    let (certs, key) = generate_self_signed_cert(&cert_dir, "localhost")?;

    // The cert file path for clients to trust (PEM format)
    let cert_path = cert_dir.join("fullchain.pem");

    let bind_addr = format!("0.0.0.0:{}", port);

    let config = Config {
        bind: bind_addr.parse()?,
        certs,
        key,
        data_dir: None,
        relay: None,
    };

    let server = Server::new(config)?;

    tokio::spawn(async move {
        tokio::select! {
            result = server.run() => {
                if let Err(e) = result {
                    error!("Server error: {}", e);
                }
            }
            _ = &mut shutdown_rx => {
                info!("Server shutdown requested");
            }
        }
    });

    Ok(ServerHandle {
        shutdown_tx: Some(shutdown_tx),
        cert_path,
    })
}
