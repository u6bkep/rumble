//! Core Rumble application logic.
//!
//! This module contains [`RumbleApp`], the main application struct that handles
//! all UI rendering and business logic. It is independent of the eframe runner,
//! allowing it to be used with different backends (eframe for desktop, test harness
//! for automated testing).

use backend::{
    AudioSettings, BackendHandle, Command, ConnectConfig, ConnectionState, FileMessage, P2pFileMessage, PipelineConfig,
    ProcessorRegistry, SfxKind, TransferState, VoiceMode, build_default_tx_pipeline, merge_with_default_tx_pipeline,
    register_builtin_processors,
};
use ed25519_dalek::SigningKey;
use eframe::egui;
use egui::Modal;
use egui_ltreeview::{Action, NodeBuilder, TreeView};
use std::path::PathBuf;
use uuid::Uuid;

use crate::{
    hotkeys::HotkeyEvent,
    key_manager::{
        FirstRunState, KeyInfo, KeyManager, KeySource, PendingAgentOp, SshAgentClient, connect_and_list_keys,
        generate_and_add_to_agent, parse_signing_key,
    },
    settings::{
        AcceptedCertificate, Args, AutoDownloadRule, PersistentAudioSettings, PersistentSettings, PersistentVoiceMode,
        TimestampFormat, format_size, get_file_icon,
    },
    toasts::ToastManager,
};

/// Node ID for the tree view - can be either a room or a user.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TreeNodeId {
    Room(Uuid),
    User { room_id: Uuid, user_id: u64 },
}

/// State for the rename room modal
#[derive(Default)]
struct RenameModalState {
    open: bool,
    room_uuid: Option<Uuid>,
    room_name: String,
}

/// State for the move room confirmation modal
#[derive(Default)]
struct MoveRoomModalState {
    open: bool,
    room_id: Option<Uuid>,
    room_name: String,
    new_parent_id: Option<Uuid>,
    new_parent_name: String,
}

/// State for the delete room confirmation modal
#[derive(Default)]
struct DeleteRoomModalState {
    open: bool,
    room_id: Option<Uuid>,
    room_name: String,
}

/// State for the edit room description modal
#[derive(Default)]
struct DescriptionModalState {
    open: bool,
    room_uuid: Option<Uuid>,
    room_name: String,
    description: String,
}

/// State for the download file modal
#[derive(Default)]
struct DownloadModalState {
    open: bool,
    magnet_link: String,
    validation_error: Option<String>,
}

/// State for the image view modal (click-to-enlarge with zoom/pan)
struct ImageViewModalState {
    open: bool,
    image_uri: String,
    image_name: String,
    /// Path to the full-resolution image file on disk
    file_path: Option<PathBuf>,
    /// Current zoom level (1.0 = fit to modal)
    zoom: f32,
    /// Whether the full-resolution image has been loaded into the cache
    fullsize_loaded: bool,
}

impl Default for ImageViewModalState {
    fn default() -> Self {
        Self {
            open: false,
            image_uri: String::new(),
            image_name: String::new(),
            file_path: None,
            zoom: 1.0,
            fullsize_loaded: false,
        }
    }
}

/// Validate a magnet link format.
fn is_valid_magnet(s: &str) -> bool {
    s.starts_with("magnet:?") && s.contains("xt=urn:btih:")
}

/// Categories for the settings sidebar
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
enum SettingsCategory {
    #[default]
    Connection,
    Devices,
    Voice,
    Sounds,
    Processing,
    Encoder,
    Chat,
    FileTransfer,
    Keyboard,
    Statistics,
}

impl SettingsCategory {
    fn all() -> &'static [SettingsCategory] {
        &[
            SettingsCategory::Connection,
            SettingsCategory::Devices,
            SettingsCategory::Voice,
            SettingsCategory::Sounds,
            SettingsCategory::Processing,
            SettingsCategory::Encoder,
            SettingsCategory::Chat,
            SettingsCategory::FileTransfer,
            SettingsCategory::Keyboard,
            SettingsCategory::Statistics,
        ]
    }

    fn label(&self) -> &'static str {
        match self {
            SettingsCategory::Connection => "🔗 Connection",
            SettingsCategory::Devices => "🔊 Devices",
            SettingsCategory::Voice => "🎤 Voice",
            SettingsCategory::Sounds => "🔔 Sounds",
            SettingsCategory::Processing => "⚙ Processing",
            SettingsCategory::Encoder => "📦 Encoder",
            SettingsCategory::Chat => "💬 Chat",
            SettingsCategory::FileTransfer => "📂 File Transfer",
            SettingsCategory::Keyboard => "⌨ Keyboard",
            SettingsCategory::Statistics => "📊 Statistics",
        }
    }
}

/// Which hotkey is being captured
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyCaptureTarget {
    Ptt,
    ToggleMute,
    ToggleDeafen,
}

/// State for the settings modal with pending changes
#[derive(Default)]
struct SettingsModalState {
    /// Currently selected settings categories (Ctrl+click to multi-select)
    selected_categories: std::collections::HashSet<SettingsCategory>,
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
    /// Pending TX pipeline configuration
    pending_tx_pipeline: Option<PipelineConfig>,
    /// Pending show timestamps setting
    pending_show_timestamps: Option<bool>,
    /// Pending timestamp format
    pending_timestamp_format: Option<TimestampFormat>,
    /// Whether any settings have been modified
    dirty: bool,
    /// Which hotkey is currently being captured (None if not capturing)
    hotkey_capture_target: Option<HotkeyCaptureTarget>,
    /// Pending hotkey binding that has a conflict, waiting for user confirmation
    hotkey_conflict_pending: Option<(HotkeyCaptureTarget, crate::settings::HotkeyBinding, &'static str)>,
}

/// The main Rumble application.
///
/// This struct contains all application state and UI logic. It is designed to be
/// independent of the window/event loop runner, allowing it to be used with both
/// eframe (for the desktop application) and a test harness (for automated testing).
pub struct RumbleApp {
    // UI state
    show_connect: bool,
    show_settings: bool,
    show_transfers: bool,
    connect_address: String,
    connect_password: String,
    trust_dev_cert: bool,
    chat_input: String,
    client_name: String,

    // Key manager for Ed25519 identity
    key_manager: KeyManager,

    // First-run setup state (shown if no key is configured)
    first_run_state: FirstRunState,

    // Cached signing key (from key manager after setup/unlock)
    signing_key: Option<SigningKey>,

    // Pending async operation for SSH agent
    pending_agent_op: Option<PendingAgentOp>,

    // Pending file dialog (async to avoid blocking audio)
    pending_file_dialog: Option<tokio::task::JoinHandle<Option<std::path::PathBuf>>>,

    // Pending save file dialog (returns infohash and destination path)
    pending_save_dialog: Option<tokio::task::JoinHandle<Option<(String, std::path::PathBuf)>>>,

    // Tokio runtime handle for async operations (SSH agent, file dialogs)
    tokio_handle: tokio::runtime::Handle,

    // Persistent settings
    persistent_settings: PersistentSettings,
    /// Whether to autoconnect on launch
    autoconnect_on_launch: bool,

    // Backend handle (manages connection, audio, and state)
    backend: BackendHandle,

    // Processor registry for schema lookups
    processor_registry: ProcessorRegistry,

    // Rename modal state
    rename_modal: RenameModalState,

    // Move room confirmation modal state
    move_room_modal: MoveRoomModalState,

    // Download file modal state
    download_modal: DownloadModalState,

    // Delete room confirmation modal state
    delete_room_modal: DeleteRoomModalState,

    // Edit room description modal state
    description_modal: DescriptionModalState,

    // Image view modal state (click-to-enlarge)
    image_view_modal: ImageViewModalState,

    // Settings modal state with pending changes
    settings_modal: SettingsModalState,

    /// Push-to-talk key is held
    push_to_talk_active: bool,

    /// Global hotkey registration status per action (set by EframeWrapper)
    hotkey_registration_status:
        std::collections::HashMap<crate::hotkeys::HotkeyAction, crate::hotkeys::HotkeyRegistrationStatus>,

    /// Whether XDG Portal global shortcuts are available (Wayland)
    portal_hotkeys_available: bool,

    /// Portal shortcut bindings (updated from HotkeyManager)
    #[cfg(target_os = "linux")]
    portal_shortcuts: Vec<crate::portal_hotkeys::ShortcutInfo>,

    /// Callback to open portal settings (set by EframeWrapper)
    #[cfg(target_os = "linux")]
    open_portal_settings_callback: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,

    egui_ctx: egui::Context,

    // Toast notification manager
    toast_manager: ToastManager,

    // Previous connection state for detecting transitions
    prev_connection_state: ConnectionState,

    // SFX event tracking
    prev_self_muted: bool,
    prev_user_ids_in_room: std::collections::HashSet<u64>,
    prev_user_ids_initialized: bool,
    prev_chat_count: usize,

    // RPC server (kept alive for the lifetime of the app)
    _rpc_server: Option<backend::rpc::RpcServer>,
}

impl Drop for RumbleApp {
    fn drop(&mut self) {
        // Send disconnect command before dropping the backend handle
        let state = self.backend.state();
        if state.connection.is_connected() {
            self.backend.send(Command::Disconnect);
        }
        // BackendHandle will clean up the background thread and audio when dropped
    }
}

impl RumbleApp {
    /// Create a new RumbleApp instance.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The egui context for rendering
    /// * `runtime_handle` - A handle to the tokio runtime for async operations
    /// * `args` - Command-line arguments (or defaults for testing)
    pub fn new(ctx: egui::Context, runtime_handle: tokio::runtime::Handle, args: Args) -> Self {
        // Load persistent settings
        let mut persistent_settings = PersistentSettings::load();

        // Get config directory for key manager
        let config_dir = PersistentSettings::config_dir().unwrap_or_else(|| PathBuf::from("."));

        // Create key manager and check for migration from old format
        let mut key_manager = KeyManager::new(config_dir);

        // Migration: If we have an old-style key in persistent_settings but no new key config,
        // migrate it to the new key_manager format
        if key_manager.needs_setup() {
            if let Some(ref hex_key) = persistent_settings.identity_private_key_hex {
                if let Some(signing_key) = parse_signing_key(hex_key) {
                    tracing::info!("Migrating existing identity key to new format");
                    if let Err(e) = key_manager.import_signing_key(signing_key) {
                        tracing::error!("Failed to migrate key: {}", e);
                    }
                }
            }
        }

        // Determine first-run state
        let first_run_state = if key_manager.needs_setup() {
            FirstRunState::SelectMethod
        } else {
            FirstRunState::NotNeeded
        };

        // Get the cached signing key if available
        let signing_key = key_manager.signing_key().cloned();

        // CLI args override persistent settings
        let server_address = args
            .server
            .clone()
            .unwrap_or_else(|| persistent_settings.server_address.clone());
        let server_password = args
            .password
            .clone()
            .unwrap_or_else(|| persistent_settings.server_password.clone());
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

        // Load previously accepted certificates from persistent settings
        {
            use base64::Engine;
            for accepted in &persistent_settings.accepted_certificates {
                match base64::engine::general_purpose::STANDARD.decode(&accepted.certificate_der_base64) {
                    Ok(der_bytes) => {
                        tracing::info!("Loading accepted certificate for {}", accepted.server_name);
                        config.accepted_certs.push(der_bytes);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to decode accepted certificate for {}: {}",
                            accepted.server_name,
                            e
                        );
                    }
                }
            }
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
        backend.send(Command::UpdateAudioSettings {
            settings: audio_settings.clone(),
        });

        // Apply voice mode
        let voice_mode: VoiceMode = persistent_settings.voice_mode.into();
        backend.send(Command::SetVoiceMode { mode: voice_mode });

        // Create processor registry for schema lookups
        let mut processor_registry = ProcessorRegistry::new();
        register_builtin_processors(&mut processor_registry);

        // Apply TX pipeline from persistent config, or build a default one
        // Use merge to ensure new processors are added to existing configs
        let tx_pipeline_config = if let Some(ref config) = persistent_settings.audio.tx_pipeline {
            // Merge persisted config with default to add any new processors
            merge_with_default_tx_pipeline(config, &processor_registry)
        } else {
            // No persisted config - build fresh default
            build_default_tx_pipeline(&processor_registry)
        };
        backend.send(Command::UpdateTxPipeline {
            config: tx_pipeline_config,
        });

        // Apply device selections if set
        if persistent_settings.input_device_id.is_some() {
            backend.send(Command::SetInputDevice {
                device_id: persistent_settings.input_device_id.clone(),
            });
        }
        if persistent_settings.output_device_id.is_some() {
            backend.send(Command::SetOutputDevice {
                device_id: persistent_settings.output_device_id.clone(),
            });
        }

        // Apply file transfer settings to backend
        {
            let ui_settings = &persistent_settings.file_transfer;
            let backend_settings = backend::FileTransferSettings {
                auto_download_enabled: ui_settings.auto_download_enabled,
                auto_download_rules: ui_settings
                    .auto_download_rules
                    .iter()
                    .map(|r| backend::AutoDownloadRule {
                        mime_pattern: r.mime_pattern.clone(),
                        max_size_bytes: r.max_size_bytes,
                    })
                    .collect(),
            };
            backend.send(Command::UpdateFileTransferSettings {
                settings: backend_settings,
            });
        }

        // Send initial status messages via backend
        backend.send(Command::LocalMessage {
            text: format!("Rumble Client v{}", env!("CARGO_PKG_VERSION")),
        });
        backend.send(Command::LocalMessage {
            text: format!("Client name: {}", client_name),
        });

        // Update persistent settings with current values (in case CLI overrode them)
        persistent_settings.server_address = server_address.clone();
        persistent_settings.server_password = server_password.clone();
        persistent_settings.client_name = client_name.clone();
        persistent_settings.trust_dev_cert = trust_dev_cert;

        let autoconnect_on_launch = persistent_settings.autoconnect_on_launch;

        // Save settings
        if let Err(e) = persistent_settings.save() {
            tracing::warn!("Failed to save settings: {}", e);
        }

        let needs_first_run = !matches!(first_run_state, FirstRunState::NotNeeded);

        // Start RPC server for external process control (opt-in via --rpc-server flag or --rpc-socket)
        let rpc_server = if args.rpc_server || args.rpc_socket.is_some() {
            let socket_path = args
                .rpc_socket
                .as_ref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(backend::rpc::default_socket_path);
            match backend.start_rpc_server(socket_path) {
                Ok(server) => {
                    tracing::info!("RPC server started");
                    Some(server)
                }
                Err(e) => {
                    tracing::warn!("Failed to start RPC server: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let mut app = Self {
            show_connect: false,
            show_settings: false,
            show_transfers: false,
            connect_address: server_address,
            connect_password: server_password,
            trust_dev_cert,
            chat_input: String::new(),
            client_name,
            key_manager,
            first_run_state,
            signing_key,
            pending_agent_op: None,
            pending_file_dialog: None,
            pending_save_dialog: None,
            tokio_handle: runtime_handle,
            persistent_settings,
            autoconnect_on_launch,
            backend,
            processor_registry,
            rename_modal: RenameModalState::default(),
            move_room_modal: MoveRoomModalState::default(),
            download_modal: DownloadModalState::default(),
            delete_room_modal: DeleteRoomModalState::default(),
            description_modal: DescriptionModalState::default(),
            image_view_modal: ImageViewModalState::default(),
            settings_modal: SettingsModalState::default(),
            push_to_talk_active: false,
            hotkey_registration_status: std::collections::HashMap::new(),
            portal_hotkeys_available: false,
            #[cfg(target_os = "linux")]
            portal_shortcuts: Vec::new(),
            #[cfg(target_os = "linux")]
            open_portal_settings_callback: None,
            egui_ctx: ctx,
            toast_manager: ToastManager::new(),
            prev_connection_state: ConnectionState::Disconnected,
            prev_self_muted: false,
            prev_user_ids_in_room: std::collections::HashSet::new(),
            prev_user_ids_initialized: false,
            prev_chat_count: 0,
            _rpc_server: rpc_server,
        };

        // If server was specified on command line, connect immediately (unless first run)
        // Otherwise, check autoconnect setting
        if !needs_first_run {
            if args.server.is_some() {
                app.connect();
            } else if autoconnect_on_launch && !app.connect_address.is_empty() {
                app.backend.send(Command::LocalMessage {
                    text: "Auto-connecting...".to_string(),
                });
                app.connect();
            }
        }

        app
    }

    /// Get access to the backend handle.
    pub fn backend(&self) -> &BackendHandle {
        &self.backend
    }

    /// Get access to the toast manager.
    pub fn toast_manager(&mut self) -> &mut ToastManager {
        &mut self.toast_manager
    }

    /// Check if the client is connected to a server.
    pub fn is_connected(&self) -> bool {
        self.backend.state().connection.is_connected()
    }

    /// Try to paste an image from the system clipboard and share it.
    ///
    /// Reads image data via arboard (which works independently of egui_winit's
    /// smithay/text-only clipboard). If the clipboard contains an image, it is
    /// saved to a temporary PNG and shared via the file transfer system.
    ///
    /// TODO: Replace this with `egui::Event::PasteImage` once upstream egui
    /// adds image paste support (https://github.com/emilk/egui/issues/2108).
    /// That would also enable Ctrl+V image paste instead of needing a button.
    pub fn paste_clipboard_image(&mut self) {
        if !self.is_connected() {
            self.toast_manager.warning("Connect to a server before pasting images");
            return;
        }
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(_) => {
                self.toast_manager.error("Could not access clipboard");
                return;
            }
        };
        let img_data = match clipboard.get_image() {
            Ok(d) => d,
            Err(_) => {
                self.toast_manager.warning("No image on clipboard");
                return;
            }
        };
        if let Some(img) = image::RgbaImage::from_raw(
            img_data.width as u32,
            img_data.height as u32,
            img_data.bytes.into_owned(),
        ) {
            if let Ok(temp_dir) = tempfile::tempdir() {
                let temp_path = temp_dir.path().join("clipboard_image.png");
                if img.save(&temp_path).is_ok() {
                    let path = temp_path.clone();
                    // Keep the temp dir alive (cleaned up on process exit)
                    std::mem::forget(temp_dir);
                    self.backend.send(Command::ShareFile { path });
                    self.show_transfers = true;
                    self.toast_manager.success("Sharing pasted image");
                    return;
                }
            }
        }
        self.toast_manager.error("Failed to process clipboard image");
    }

    /// Set the global hotkey registration status from the HotkeyManager.
    pub fn set_hotkey_registration_status(
        &mut self,
        status: std::collections::HashMap<crate::hotkeys::HotkeyAction, crate::hotkeys::HotkeyRegistrationStatus>,
    ) {
        self.hotkey_registration_status = status;
    }

    /// Set whether XDG Portal global shortcuts are available.
    ///
    /// This should be called by the main app after initializing the portal backend.
    pub fn set_portal_hotkeys_available(&mut self, available: bool) {
        self.portal_hotkeys_available = available;
    }

    /// Check if XDG Portal global shortcuts are available.
    pub fn is_portal_hotkeys_available(&self) -> bool {
        self.portal_hotkeys_available
    }

    /// Set the portal shortcut bindings (Linux/Wayland only).
    #[cfg(target_os = "linux")]
    pub fn set_portal_shortcuts(&mut self, shortcuts: Vec<crate::portal_hotkeys::ShortcutInfo>) {
        self.portal_shortcuts = shortcuts;
    }

    /// Set the callback for opening portal settings (Linux/Wayland only).
    #[cfg(target_os = "linux")]
    pub fn set_open_portal_settings_callback<F>(&mut self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.open_portal_settings_callback = Some(std::sync::Arc::new(callback));
    }

    /// Show or hide the file transfers window.
    pub fn set_show_transfers(&mut self, show: bool) {
        self.show_transfers = show;
    }

    /// Check if the transfers window is visible.
    pub fn is_transfers_visible(&self) -> bool {
        self.show_transfers
    }

    /// Get read access to persistent settings (for testing).
    pub fn persistent_settings(&self) -> &PersistentSettings {
        &self.persistent_settings
    }

    /// Get mutable access to persistent settings (for testing).
    pub fn persistent_settings_mut(&mut self) -> &mut PersistentSettings {
        &mut self.persistent_settings
    }

    /// Check if push-to-talk is currently active.
    pub fn push_to_talk_active(&self) -> bool {
        self.push_to_talk_active
    }

    /// Play a sound effect if enabled in settings.
    fn play_sfx(&self, kind: SfxKind) {
        let sfx = &self.persistent_settings.sfx;
        if !sfx.enabled || sfx.disabled_sounds.contains(&kind) || sfx.volume <= 0.0 {
            return;
        }
        self.backend.send(Command::PlaySfx {
            kind,
            volume: sfx.volume,
        });
    }

    /// Connect to the server using current settings.
    fn connect(&mut self) {
        // Check if we have a key configured and can create a signer
        let public_key = match self.key_manager.public_key_bytes() {
            Some(key) => key,
            None => {
                self.backend.send(Command::LocalMessage {
                    text: "Cannot connect: No identity key configured. Please complete first-run setup.".to_string(),
                });
                return;
            }
        };

        let signer = match self.key_manager.create_signer() {
            Some(s) => s,
            None => {
                // Check if key needs unlocking
                if self.key_manager.needs_unlock() {
                    self.backend.send(Command::LocalMessage {
                        text: "Cannot connect: Key is encrypted. Please unlock it in settings.".to_string(),
                    });
                } else {
                    self.backend.send(Command::LocalMessage {
                        text: "Cannot connect: Failed to create signer for key.".to_string(),
                    });
                }
                return;
            }
        };

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

        self.backend.send(Command::LocalMessage {
            text: format!("Connecting to {}...", addr),
        });
        self.backend.send(Command::Connect {
            addr,
            name,
            public_key,
            signer,
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
        let mut persistent_audio: PersistentAudioSettings = (&audio.settings).into();

        // Save the TX pipeline config - prefer pending modal config if available
        // (the async command to backend might not have processed yet)
        persistent_audio.tx_pipeline = self
            .settings_modal
            .pending_tx_pipeline
            .clone()
            .or_else(|| Some(audio.tx_pipeline.clone()));

        self.persistent_settings.audio = persistent_audio;
        self.persistent_settings.voice_mode = self
            .settings_modal
            .pending_voice_mode
            .clone()
            .map(|m| (&m).into())
            .unwrap_or_else(|| (&audio.voice_mode).into());
        self.persistent_settings.input_device_id = audio.selected_input.clone();
        self.persistent_settings.output_device_id = audio.selected_output.clone();

        // Chat settings are already in persistent_settings (applied in apply_pending_settings)

        if let Err(e) = self.persistent_settings.save() {
            tracing::error!("Failed to save settings: {}", e);
            self.backend.send(Command::LocalMessage {
                text: format!("Failed to save settings: {}", e),
            });
        } else {
            self.backend.send(Command::LocalMessage {
                text: "Settings saved.".to_string(),
            });
        }
    }

    /// Attempt to reconnect with the last known connection parameters.
    fn reconnect(&mut self) {
        self.backend.send(Command::LocalMessage {
            text: format!("Reconnecting to {}...", self.connect_address),
        });
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

    /// Handle a global hotkey event.
    ///
    /// This is called from the eframe wrapper when a global hotkey is pressed
    /// (works even when the window is not focused).
    pub fn handle_hotkey_event(&mut self, event: HotkeyEvent) {
        let state = self.backend.state();

        match event {
            HotkeyEvent::PttPressed => {
                tracing::debug!(
                    "Global hotkey: PTT pressed (connected={}, already_active={})",
                    state.connection.is_connected(),
                    self.push_to_talk_active
                );
                if !self.push_to_talk_active && state.connection.is_connected() {
                    self.push_to_talk_active = true;
                    self.start_transmit();
                }
            }
            HotkeyEvent::PttReleased => {
                tracing::debug!("Global hotkey: PTT released (was_active={})", self.push_to_talk_active);
                if self.push_to_talk_active {
                    self.push_to_talk_active = false;
                    self.stop_transmit();
                }
            }
            HotkeyEvent::ToggleMute => {
                tracing::debug!("Global hotkey: Toggle mute");
                if state.connection.is_connected() {
                    self.backend.send(Command::SetMuted {
                        muted: !state.audio.self_muted,
                    });
                }
            }
            HotkeyEvent::ToggleDeafen => {
                tracing::debug!("Global hotkey: Toggle deafen");
                if state.connection.is_connected() {
                    self.backend.send(Command::SetDeafened {
                        deafened: !state.audio.self_deafened,
                    });
                }
            }
        }
    }

    /// Apply all pending settings from the settings modal to the backend.
    fn apply_pending_settings(&mut self) {
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
                self.backend.send(Command::SetVoiceMode { mode: mode.clone() });
            }
        }

        // Apply pending TX pipeline
        if let Some(config) = self.settings_modal.pending_tx_pipeline.clone() {
            self.backend.send(Command::UpdateTxPipeline { config });
        }

        // Apply pending chat settings (these are UI-only, stored in persistent_settings)
        if let Some(show) = self.settings_modal.pending_show_timestamps {
            self.persistent_settings.show_chat_timestamps = show;
        }
        if let Some(format) = self.settings_modal.pending_timestamp_format {
            self.persistent_settings.chat_timestamp_format = format;
        }

        // Apply file transfer settings to backend
        self.sync_file_transfer_settings_to_backend();
    }

    /// Send the current file transfer settings to the backend.
    fn sync_file_transfer_settings_to_backend(&self) {
        let ui_settings = &self.persistent_settings.file_transfer;
        let backend_settings = backend::FileTransferSettings {
            auto_download_enabled: ui_settings.auto_download_enabled,
            auto_download_rules: ui_settings
                .auto_download_rules
                .iter()
                .map(|r| backend::AutoDownloadRule {
                    mime_pattern: r.mime_pattern.clone(),
                    max_size_bytes: r.max_size_bytes,
                })
                .collect(),
        };
        self.backend.send(Command::UpdateFileTransferSettings {
            settings: backend_settings,
        });
    }

    /// Join a room and optionally request chat history if auto-sync is enabled.
    fn join_room(&self, room_id: uuid::Uuid) {
        self.backend.send(Command::JoinRoom { room_id });
        if self.persistent_settings.file_transfer.auto_sync_history {
            // Delay the history request slightly to allow room join to complete
            self.backend.send(Command::RequestChatHistory);
        }
    }

    /// Render the Connection settings category.
    fn render_settings_connection(&mut self, ui: &mut egui::Ui, state: &backend::State) {
        ui.heading("Connection");
        ui.add_space(8.0);

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

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Username setting
        ui.label("Username:");
        let mut pending_username = self
            .settings_modal
            .pending_username
            .clone()
            .unwrap_or_else(|| self.client_name.clone());
        if ui
            .text_edit_singleline(&mut pending_username)
            .on_hover_text("Your display name shown to other users")
            .changed()
        {
            self.settings_modal.pending_username = Some(pending_username);
            self.settings_modal.dirty = true;
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Autoconnect on launch toggle
        let mut pending_autoconnect = self
            .settings_modal
            .pending_autoconnect
            .unwrap_or(self.autoconnect_on_launch);
        if ui
            .checkbox(&mut pending_autoconnect, "Autoconnect on launch")
            .on_hover_text("Automatically connect to the last server when the app starts")
            .changed()
        {
            self.settings_modal.pending_autoconnect = Some(pending_autoconnect);
            self.settings_modal.dirty = true;
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Identity section
        ui.heading("Identity");
        ui.add_space(4.0);

        if let Some(public_key_hex) = self.key_manager.public_key_hex() {
            ui.horizontal(|ui| {
                ui.label("Public Key:");
                // Show truncated key with copy button
                let short_key = format!("{}...{}", &public_key_hex[..8], &public_key_hex[56..]);
                ui.code(&short_key);
                if ui.small_button("📋").on_hover_text("Copy full public key").clicked() {
                    ui.ctx().copy_text(public_key_hex.clone());
                }
            });

            // Show key source
            if let Some(config) = self.key_manager.config() {
                let source_desc = match &config.source {
                    KeySource::LocalPlaintext { .. } => "Local (unencrypted)",
                    KeySource::LocalEncrypted { .. } => "Local (password protected)",
                    KeySource::SshAgent { comment, .. } => &format!("SSH Agent ({})", comment),
                };
                ui.horizontal(|ui| {
                    ui.label("Storage:");
                    ui.label(source_desc);
                });
            }

            ui.add_space(8.0);
            if ui
                .button("🔑 Generate New Identity...")
                .on_hover_text("Generate a new identity key (will replace the current one)")
                .clicked()
            {
                self.first_run_state = FirstRunState::SelectMethod;
            }
        } else {
            ui.colored_label(egui::Color32::YELLOW, "⚠ No identity key configured");
            if ui.button("🔑 Configure Identity...").clicked() {
                self.first_run_state = FirstRunState::SelectMethod;
            }
        }
    }

    /// Render the Devices settings category.
    fn render_settings_devices(&mut self, ui: &mut egui::Ui, state: &backend::State) {
        ui.heading("Audio Devices");
        ui.add_space(8.0);

        let audio = &state.audio;

        // Refresh button (immediate action, not deferred)
        if ui.button("🔄 Refresh Devices").clicked() {
            self.backend.send(Command::RefreshAudioDevices);
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Input device selection (using pending state)
        ui.label("Input Device (Microphone):");
        let pending_input = self.settings_modal.pending_input_device.clone().flatten();
        let current_input_name = pending_input
            .as_ref()
            .and_then(|id| audio.input_devices.iter().find(|d| &d.id == id))
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "Default".to_string());

        egui::ComboBox::from_id_salt("input_device")
            .selected_text(&current_input_name)
            .show_ui(ui, |ui| {
                // Default option
                if ui.selectable_label(pending_input.is_none(), "Default").clicked() {
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
                        .selectable_label(pending_input.as_ref() == Some(&device.id), &label)
                        .clicked()
                    {
                        self.settings_modal.pending_input_device = Some(Some(device.id.clone()));
                        self.settings_modal.dirty = true;
                    }
                }
            });

        ui.add_space(8.0);

        // Output device selection (using pending state)
        ui.label("Output Device (Speakers):");
        let pending_output = self.settings_modal.pending_output_device.clone().flatten();
        let current_output_name = pending_output
            .as_ref()
            .and_then(|id| audio.output_devices.iter().find(|d| &d.id == id))
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "Default".to_string());

        egui::ComboBox::from_id_salt("output_device")
            .selected_text(&current_output_name)
            .show_ui(ui, |ui| {
                // Default option
                if ui.selectable_label(pending_output.is_none(), "Default").clicked() {
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
                        .selectable_label(pending_output.as_ref() == Some(&device.id), &label)
                        .clicked()
                    {
                        self.settings_modal.pending_output_device = Some(Some(device.id.clone()));
                        self.settings_modal.dirty = true;
                    }
                }
            });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Audio Input Level Meter
        ui.label("Input Level:");
        if let Some(level_db) = audio.input_level_db {
            ui.horizontal(|ui| {
                // Normalize level_db to 0.0-1.0 range (assuming -60dB to 0dB range)
                let normalized = ((level_db + 60.0_f32) / 60.0_f32).clamp(0.0, 1.0);
                let color = if level_db > -3.0 {
                    egui::Color32::RED // Clipping
                } else if level_db > -12.0 {
                    egui::Color32::YELLOW // Loud
                } else {
                    egui::Color32::GREEN // Normal
                };
                let (rect, _response) = ui.allocate_exact_size(egui::vec2(200.0, 16.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, egui::Color32::DARK_GRAY);
                let filled_rect =
                    egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * normalized, rect.height()));
                ui.painter().rect_filled(filled_rect, 2.0, color);

                // Draw VAD threshold line if VAD is enabled
                let pipeline = self
                    .settings_modal
                    .pending_tx_pipeline
                    .as_ref()
                    .unwrap_or(&audio.tx_pipeline);
                let vad_threshold = pipeline
                    .processors
                    .iter()
                    .find(|p| p.type_id == "builtin.vad" && p.enabled)
                    .and_then(|p| p.settings.get("threshold_db"))
                    .and_then(|v| v.as_f64())
                    .map(|t| t as f32);

                if let Some(threshold_db) = vad_threshold {
                    let threshold_normalized = ((threshold_db + 60.0) / 60.0).clamp(0.0, 1.0);
                    let threshold_x = rect.min.x + rect.width() * threshold_normalized;
                    ui.painter().line_segment(
                        [egui::pos2(threshold_x, rect.min.y), egui::pos2(threshold_x, rect.max.y)],
                        egui::Stroke::new(2.0, egui::Color32::WHITE),
                    );
                }

                ui.label(format!("{:.0} dB", level_db));
            });
        } else {
            ui.colored_label(egui::Color32::GRAY, "—");
        }
    }

    /// Render the Voice settings category.
    fn render_settings_voice(&mut self, ui: &mut egui::Ui, state: &backend::State) {
        ui.heading("Voice Mode");
        ui.add_space(8.0);

        let audio = &state.audio;

        // Voice mode selector (using pending state)
        let pending_voice_mode = self
            .settings_modal
            .pending_voice_mode
            .clone()
            .unwrap_or(audio.voice_mode.clone());
        ui.horizontal(|ui| {
            if ui
                .selectable_label(matches!(pending_voice_mode, VoiceMode::PushToTalk), "🎤 Push-to-Talk")
                .on_hover_text("Hold SPACE to transmit")
                .clicked()
            {
                self.settings_modal.pending_voice_mode = Some(VoiceMode::PushToTalk);
                self.settings_modal.dirty = true;
            }
            if ui
                .selectable_label(matches!(pending_voice_mode, VoiceMode::Continuous), "📡 Continuous")
                .on_hover_text("Always transmitting when connected. Enable VAD processor for voice-activated behavior.")
                .clicked()
            {
                self.settings_modal.pending_voice_mode = Some(VoiceMode::Continuous);
                self.settings_modal.dirty = true;
            }
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Mute and Deafen toggles (immediate action - these are real-time controls)
        ui.label("Quick Controls:");
        ui.horizontal(|ui| {
            let mute_text = if audio.self_muted { "🔇 Muted" } else { "🎤 Unmuted" };
            let mute_color = if audio.self_muted {
                egui::Color32::RED
            } else {
                egui::Color32::GREEN
            };
            if ui
                .add(egui::Button::new(egui::RichText::new(mute_text).color(mute_color)))
                .on_hover_text("Toggle self-mute (stops transmitting)")
                .clicked()
            {
                self.backend.send(Command::SetMuted {
                    muted: !audio.self_muted,
                });
            }

            let deafen_text = if audio.self_deafened {
                "🔕 Deafened"
            } else {
                "🔔 Hearing"
            };
            let deafen_color = if audio.self_deafened {
                egui::Color32::RED
            } else {
                egui::Color32::GREEN
            };
            if ui
                .add(egui::Button::new(egui::RichText::new(deafen_text).color(deafen_color)))
                .on_hover_text("Toggle self-deafen (stops receiving audio; also mutes)")
                .clicked()
            {
                self.backend.send(Command::SetDeafened {
                    deafened: !audio.self_deafened,
                });
            }
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Status info - show mode-appropriate message
        ui.label("Status:");
        if audio.self_muted {
            ui.colored_label(egui::Color32::RED, "🔇 Muted");
        } else {
            match audio.voice_mode {
                VoiceMode::PushToTalk => {
                    ui.label("Push-to-talk: Hold SPACE to transmit");
                }
                VoiceMode::Continuous => {
                    ui.label("Continuous: Always transmitting (enable VAD for voice activation)");
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
    }

    /// Render the Sounds settings category.
    fn render_settings_sounds(&mut self, ui: &mut egui::Ui) {
        ui.heading("Sound Effects");
        ui.add_space(8.0);

        let mut changed = false;

        // Master enable checkbox
        let mut enabled = self.persistent_settings.sfx.enabled;
        if ui.checkbox(&mut enabled, "Enable sound effects").changed() {
            self.persistent_settings.sfx.enabled = enabled;
            changed = true;
        }

        ui.add_space(4.0);

        // Volume slider
        ui.horizontal(|ui| {
            ui.label("Volume:");
            let mut volume_pct = (self.persistent_settings.sfx.volume * 100.0).round() as i32;
            if ui
                .add(egui::Slider::new(&mut volume_pct, 0..=100).suffix("%"))
                .changed()
            {
                self.persistent_settings.sfx.volume = volume_pct as f32 / 100.0;
                changed = true;
            }
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        ui.label("Individual sounds:");
        ui.add_space(4.0);

        // Per-sound toggle with label and preview button
        for kind in SfxKind::all() {
            ui.horizontal(|ui| {
                let is_disabled = self.persistent_settings.sfx.disabled_sounds.contains(kind);
                let mut sound_enabled = !is_disabled;
                if ui.checkbox(&mut sound_enabled, kind.label()).changed() {
                    if sound_enabled {
                        self.persistent_settings.sfx.disabled_sounds.remove(kind);
                    } else {
                        self.persistent_settings.sfx.disabled_sounds.insert(*kind);
                    }
                    changed = true;
                }
                if ui.small_button("Preview").clicked() {
                    // Play directly regardless of enabled state so user can hear what it sounds like
                    self.backend.send(Command::PlaySfx {
                        kind: *kind,
                        volume: self.persistent_settings.sfx.volume.max(0.3),
                    });
                }
            });
        }

        if changed {
            let _ = self.persistent_settings.save();
        }
    }

    /// Render the Processing settings category.
    fn render_settings_processing(&mut self, ui: &mut egui::Ui, state: &backend::State) {
        ui.heading("Audio Processing");
        ui.add_space(8.0);

        ui.label("TX Pipeline - Audio processing chain applied before encoding:");
        ui.add_space(4.0);

        // Get processor info from registry for display names
        let processor_info: std::collections::HashMap<&str, (&str, &str)> = self
            .processor_registry
            .list_available()
            .into_iter()
            .map(|(type_id, display_name, desc)| (type_id, (display_name, desc)))
            .collect();

        if let Some(ref mut pending_pipeline) = self.settings_modal.pending_tx_pipeline {
            let mut pipeline_changed = false;

            for proc_config in pending_pipeline.processors.iter_mut() {
                // Look up the processor info for display name and description
                let (display_name, description) = processor_info
                    .get(proc_config.type_id.as_str())
                    .copied()
                    .unwrap_or((&proc_config.type_id, "Unknown processor"));

                ui.horizontal(|ui| {
                    // Enabled checkbox
                    if ui
                        .checkbox(&mut proc_config.enabled, display_name)
                        .on_hover_text(description)
                        .changed()
                    {
                        pipeline_changed = true;
                    }
                });

                // Show settings if processor is enabled
                if proc_config.enabled {
                    if let Some(schema) = self.processor_registry.settings_schema(&proc_config.type_id) {
                        if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
                            if !properties.is_empty() {
                                ui.indent(proc_config.type_id.as_str(), |ui| {
                                    for (key, prop_schema) in properties {
                                        if render_schema_field(ui, key, prop_schema, &mut proc_config.settings) {
                                            pipeline_changed = true;
                                        }
                                    }
                                });
                            }
                        }
                    }
                }
                ui.add_space(4.0);
            }

            if pipeline_changed {
                self.settings_modal.dirty = true;
            }
        }

        // Show input level meter with VAD threshold
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        let audio = &state.audio;
        ui.label("Input Level (with VAD threshold):");
        if let Some(level_db) = audio.input_level_db {
            ui.horizontal(|ui| {
                let normalized = ((level_db + 60.0_f32) / 60.0_f32).clamp(0.0, 1.0);
                let color = if level_db > -3.0 {
                    egui::Color32::RED
                } else if level_db > -12.0 {
                    egui::Color32::YELLOW
                } else {
                    egui::Color32::GREEN
                };
                let (rect, _response) = ui.allocate_exact_size(egui::vec2(200.0, 16.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, egui::Color32::DARK_GRAY);
                let filled_rect =
                    egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * normalized, rect.height()));
                ui.painter().rect_filled(filled_rect, 2.0, color);

                // Draw VAD threshold line if VAD is enabled
                let pipeline = self
                    .settings_modal
                    .pending_tx_pipeline
                    .as_ref()
                    .unwrap_or(&audio.tx_pipeline);
                let vad_threshold = pipeline
                    .processors
                    .iter()
                    .find(|p| p.type_id == "builtin.vad" && p.enabled)
                    .and_then(|p| p.settings.get("threshold_db"))
                    .and_then(|v| v.as_f64())
                    .map(|t| t as f32);

                if let Some(threshold_db) = vad_threshold {
                    let threshold_normalized = ((threshold_db + 60.0) / 60.0).clamp(0.0, 1.0);
                    let threshold_x = rect.min.x + rect.width() * threshold_normalized;
                    ui.painter().line_segment(
                        [egui::pos2(threshold_x, rect.min.y), egui::pos2(threshold_x, rect.max.y)],
                        egui::Stroke::new(2.0, egui::Color32::WHITE),
                    );
                }

                ui.label(format!("{:.0} dB", level_db));
            });
        } else {
            ui.colored_label(egui::Color32::GRAY, "—");
        }
    }

    /// Render the Encoder settings category.
    fn render_settings_encoder(&mut self, ui: &mut egui::Ui) {
        ui.heading("Encoder Settings");
        ui.add_space(8.0);

        ui.label("Opus codec and network settings:");
        ui.add_space(4.0);

        if let Some(ref mut settings) = self.settings_modal.pending_settings {
            // FEC toggle
            if ui
                .checkbox(&mut settings.fec_enabled, "Enable Forward Error Correction")
                .on_hover_text("Add redundancy for packet loss recovery")
                .changed()
            {
                self.settings_modal.dirty = true;
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);

            // Bitrate selection
            ui.label("Encoder Bitrate:");
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(settings.bitrate == AudioSettings::BITRATE_LOW, "24 kbps")
                    .on_hover_text("Low quality, minimal bandwidth")
                    .clicked()
                {
                    settings.bitrate = AudioSettings::BITRATE_LOW;
                    self.settings_modal.dirty = true;
                }
                if ui
                    .selectable_label(settings.bitrate == AudioSettings::BITRATE_MEDIUM, "32 kbps")
                    .on_hover_text("Medium quality, good for voice")
                    .clicked()
                {
                    settings.bitrate = AudioSettings::BITRATE_MEDIUM;
                    self.settings_modal.dirty = true;
                }
                if ui
                    .selectable_label(settings.bitrate == AudioSettings::BITRATE_HIGH, "64 kbps")
                    .on_hover_text("High quality (default)")
                    .clicked()
                {
                    settings.bitrate = AudioSettings::BITRATE_HIGH;
                    self.settings_modal.dirty = true;
                }
                if ui
                    .selectable_label(settings.bitrate == AudioSettings::BITRATE_VERY_HIGH, "96 kbps")
                    .on_hover_text("Very high quality, more bandwidth")
                    .clicked()
                {
                    settings.bitrate = AudioSettings::BITRATE_VERY_HIGH;
                    self.settings_modal.dirty = true;
                }
            });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);

            // Encoder complexity slider
            ui.horizontal(|ui| {
                ui.label("Encoder Complexity:");
                if ui
                    .add(egui::Slider::new(&mut settings.encoder_complexity, 0..=10).text(""))
                    .on_hover_text("Higher = better quality but more CPU (0-10)")
                    .changed()
                {
                    self.settings_modal.dirty = true;
                }
            });

            ui.add_space(4.0);

            // Jitter buffer delay slider
            ui.horizontal(|ui| {
                ui.label("Jitter Buffer Delay:");
                if ui
                    .add(egui::Slider::new(&mut settings.jitter_buffer_delay_packets, 1..=10).text("packets"))
                    .on_hover_text("Packets to buffer before playback (1-10). Higher = more latency, smoother audio")
                    .changed()
                {
                    self.settings_modal.dirty = true;
                }
            });
            let jitter_delay_ms = settings.jitter_buffer_delay_packets * 20;
            ui.label(format!("  Playback delay: ~{}ms", jitter_delay_ms));

            ui.add_space(4.0);

            // Packet loss percentage for FEC tuning
            ui.horizontal(|ui| {
                ui.label("Expected Packet Loss:");
                if ui
                    .add(egui::Slider::new(&mut settings.packet_loss_percent, 0..=25).text("%"))
                    .on_hover_text("Expected network packet loss for FEC tuning (0-25%)")
                    .changed()
                {
                    self.settings_modal.dirty = true;
                }
            });
        }
    }

    /// Render the Statistics settings category.
    fn render_settings_statistics(&mut self, ui: &mut egui::Ui, state: &backend::State) {
        ui.heading("Audio Statistics");
        ui.add_space(8.0);

        let stats = &state.audio.stats;

        egui::Grid::new("stats_grid")
            .num_columns(2)
            .spacing([20.0, 4.0])
            .show(ui, |ui| {
                ui.label("Actual Bitrate:");
                ui.label(format!("{:.1} kbps", stats.actual_bitrate_bps / 1000.0));
                ui.end_row();

                ui.label("Avg Frame Size:");
                ui.label(format!("{:.1} bytes", stats.avg_frame_size_bytes));
                ui.end_row();

                ui.label("Packets Sent:");
                ui.label(format!("{}", stats.packets_sent));
                ui.end_row();

                ui.label("Packets Received:");
                ui.label(format!("{}", stats.packets_received));
                ui.end_row();

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
                ui.end_row();

                ui.label("FEC Recovered:");
                ui.label(format!("{}", stats.packets_recovered_fec));
                ui.end_row();

                ui.label("Frames Concealed:");
                ui.label(format!("{}", stats.frames_concealed));
                ui.end_row();

                ui.label("Buffer Level:");
                ui.label(format!("{} packets", stats.playback_buffer_packets));
                ui.end_row();
            });

        ui.add_space(8.0);

        if ui.button("Reset Statistics").clicked() {
            self.backend.send(Command::ResetAudioStats);
        }
    }

    /// Render the Chat settings category.
    fn render_settings_chat(&mut self, ui: &mut egui::Ui) {
        ui.heading("Chat Settings");
        ui.add_space(8.0);

        // Show timestamps toggle
        let show_timestamps = self
            .settings_modal
            .pending_show_timestamps
            .unwrap_or(self.persistent_settings.show_chat_timestamps);
        let mut pending_show = show_timestamps;
        if ui
            .checkbox(&mut pending_show, "Show timestamps")
            .on_hover_text("Display timestamps next to chat messages")
            .changed()
        {
            self.settings_modal.pending_show_timestamps = Some(pending_show);
            self.settings_modal.dirty = true;
        }

        ui.add_space(8.0);

        // Timestamp format dropdown (only enabled when timestamps are shown)
        ui.add_enabled_ui(pending_show, |ui| {
            ui.label("Timestamp format:");
            let current_format = self
                .settings_modal
                .pending_timestamp_format
                .unwrap_or(self.persistent_settings.chat_timestamp_format);

            // Ensure pending_timestamp_format is set so we can mutate it
            if self.settings_modal.pending_timestamp_format.is_none() {
                self.settings_modal.pending_timestamp_format = Some(current_format);
            }

            egui::ComboBox::from_id_salt("timestamp_format")
                .selected_text(current_format.label())
                .show_ui(ui, |ui| {
                    for format in TimestampFormat::all() {
                        if ui
                            .selectable_value(
                                self.settings_modal.pending_timestamp_format.as_mut().unwrap(),
                                *format,
                                format.label(),
                            )
                            .changed()
                        {
                            self.settings_modal.dirty = true;
                        }
                    }
                });
        });
    }

    fn render_settings_file_transfer(&mut self, ui: &mut egui::Ui) {
        ui.heading("File Transfer Settings");
        ui.add_space(8.0);

        // Auto-download section
        ui.strong("Auto-Download");
        ui.add_space(4.0);

        let mut auto_download = self.persistent_settings.file_transfer.auto_download_enabled;
        if ui
            .checkbox(&mut auto_download, "Enable auto-download")
            .on_hover_text("Automatically download files matching the rules below")
            .changed()
        {
            self.persistent_settings.file_transfer.auto_download_enabled = auto_download;
            self.settings_modal.dirty = true;
        }

        ui.add_enabled_ui(auto_download, |ui| {
            // Table-style UI for auto-download rules
            let rules = &mut self.persistent_settings.file_transfer.auto_download_rules;

            // Table header
            egui::Grid::new("auto_download_rules_table")
                .num_columns(3)
                .spacing([8.0, 4.0])
                .striped(true)
                .show(ui, |ui| {
                    // Header row
                    ui.strong("MIME Pattern");
                    ui.strong("Max Size");
                    ui.label(""); // For remove button
                    ui.end_row();

                    // Data rows
                    let mut rule_to_remove: Option<usize> = None;
                    for (i, rule) in rules.iter_mut().enumerate() {
                        // MIME pattern text edit
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut rule.mime_pattern)
                                .desired_width(150.0)
                                .hint_text("e.g. image/*"),
                        );
                        if response.changed() {
                            self.settings_modal.dirty = true;
                        }

                        // Max size drag value
                        let mut size_mb = rule.max_size_bytes / (1024 * 1024);
                        if ui
                            .add(egui::DragValue::new(&mut size_mb).range(0..=1000).suffix(" MB"))
                            .on_hover_text("0 = disabled for this pattern")
                            .changed()
                        {
                            rule.max_size_bytes = size_mb * 1024 * 1024;
                            self.settings_modal.dirty = true;
                        }

                        // Remove button
                        if ui.button("Remove").clicked() {
                            rule_to_remove = Some(i);
                        }
                        ui.end_row();
                    }

                    // Remove rule if requested
                    if let Some(i) = rule_to_remove {
                        rules.remove(i);
                        self.settings_modal.dirty = true;
                    }
                });

            ui.add_space(4.0);

            // Add button
            if ui.button("Add Rule").clicked() {
                rules.push(AutoDownloadRule {
                    mime_pattern: String::new(),
                    max_size_bytes: 10 * 1024 * 1024, // 10 MB default
                });
                self.settings_modal.dirty = true;
            }
        });

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);

        // Bandwidth limits section
        ui.strong("Bandwidth Limits");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("Download limit:");
            let mut dl_limit_kbps = self.persistent_settings.file_transfer.download_speed_limit / 1024;
            if ui
                .add(
                    egui::DragValue::new(&mut dl_limit_kbps)
                        .range(0..=100000)
                        .suffix(" KB/s"),
                )
                .on_hover_text("0 = unlimited")
                .changed()
            {
                self.persistent_settings.file_transfer.download_speed_limit = dl_limit_kbps * 1024;
                self.settings_modal.dirty = true;
            }
            if dl_limit_kbps == 0 {
                ui.label("(unlimited)");
            }
        });

        ui.horizontal(|ui| {
            ui.label("Upload limit:");
            let mut ul_limit_kbps = self.persistent_settings.file_transfer.upload_speed_limit / 1024;
            if ui
                .add(
                    egui::DragValue::new(&mut ul_limit_kbps)
                        .range(0..=100000)
                        .suffix(" KB/s"),
                )
                .on_hover_text("0 = unlimited")
                .changed()
            {
                self.persistent_settings.file_transfer.upload_speed_limit = ul_limit_kbps * 1024;
                self.settings_modal.dirty = true;
            }
            if ul_limit_kbps == 0 {
                ui.label("(unlimited)");
            }
        });

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);

        // Seeding behavior
        ui.strong("Seeding Behavior");
        ui.add_space(4.0);

        let mut seed_after = self.persistent_settings.file_transfer.seed_after_download;
        if ui
            .checkbox(&mut seed_after, "Continue seeding after download")
            .on_hover_text("Keep sharing files with others after download completes")
            .changed()
        {
            self.persistent_settings.file_transfer.seed_after_download = seed_after;
            self.settings_modal.dirty = true;
        }

        let mut cleanup = self.persistent_settings.file_transfer.cleanup_on_exit;
        if ui
            .checkbox(&mut cleanup, "Clean up downloaded files on exit")
            .on_hover_text("Delete downloaded files when the application closes")
            .changed()
        {
            self.persistent_settings.file_transfer.cleanup_on_exit = cleanup;
            self.settings_modal.dirty = true;
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        ui.heading("Chat History");

        let mut auto_sync = self.persistent_settings.file_transfer.auto_sync_history;
        if ui
            .checkbox(&mut auto_sync, "Auto-sync history on room join")
            .on_hover_text("Automatically request chat history from peers when joining a room")
            .changed()
        {
            self.persistent_settings.file_transfer.auto_sync_history = auto_sync;
            self.settings_modal.dirty = true;
        }
    }

    /// Render the keyboard settings panel.
    fn render_settings_keyboard(&mut self, ui: &mut egui::Ui) {
        use crate::{
            hotkeys::{HotkeyAction, HotkeyManager, HotkeyRegistrationStatus},
            settings::{HotkeyBinding, HotkeyModifiers},
        };

        ui.heading("Keyboard Shortcuts");
        ui.add_space(8.0);

        // Check if we're on Wayland
        #[cfg(target_os = "linux")]
        let is_wayland = std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.to_lowercase() == "wayland")
            .unwrap_or(false);
        #[cfg(not(target_os = "linux"))]
        let is_wayland = false;

        // Determine if Portal is the active global shortcut mechanism
        let portal_active = is_wayland && self.portal_hotkeys_available;

        // Determine if we should show the in-app hotkey capture UI.
        // On Wayland with portal, users configure global shortcuts via system settings.
        // On other platforms (or Wayland without portal), show the capture UI.
        let show_capture_ui = !portal_active;

        // --- Global Hotkeys Section ---
        if is_wayland {
            if portal_active {
                ui.strong("Global Hotkeys (via XDG Portal)");
                ui.add_space(4.0);
                ui.label(
                    "These shortcuts work globally, even when Rumble isn't focused. Configure them through your \
                     desktop environment's settings.",
                );
                ui.add_space(8.0);

                // Show the actual bound shortcuts from the portal
                #[cfg(target_os = "linux")]
                {
                    egui::Frame::new()
                        .fill(ui.visuals().extreme_bg_color)
                        .corner_radius(4.0)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            let shortcuts_to_show: Vec<(&str, String)> = if self.portal_shortcuts.is_empty() {
                                vec![
                                    ("Push-to-Talk", "(not configured)".into()),
                                    ("Toggle Mute", "(not configured)".into()),
                                    ("Toggle Deafen", "(not configured)".into()),
                                ]
                            } else {
                                self.portal_shortcuts
                                    .iter()
                                    .map(|s| {
                                        let key_display = if s.trigger_description.is_empty() {
                                            "(not configured)".to_string()
                                        } else {
                                            s.trigger_description.clone()
                                        };
                                        // Leak-free: use description from the struct
                                        (s.description.as_str(), key_display)
                                    })
                                    .collect()
                            };
                            egui::Grid::new("portal_shortcuts_grid")
                                .num_columns(2)
                                .spacing([16.0, 4.0])
                                .show(ui, |ui| {
                                    for (desc, key) in &shortcuts_to_show {
                                        ui.strong(*desc);
                                        if key == "(not configured)" {
                                            ui.colored_label(egui::Color32::from_rgb(160, 160, 160), key.as_str());
                                        } else {
                                            ui.monospace(key.as_str());
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                    ui.add_space(8.0);

                    if ui.button("Configure in System Settings...").clicked() {
                        let handle = self.tokio_handle.clone();
                        handle.spawn(async {
                            if let Err(e) = crate::portal_hotkeys::open_shortcut_settings().await {
                                tracing::error!("Failed to open shortcut settings: {}", e);
                            }
                        });
                    }
                }
            } else {
                ui.strong("Global Hotkeys");
                ui.add_space(4.0);
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_premultiplied(80, 60, 20, 180))
                    .corner_radius(4.0)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 200, 100),
                            "Global shortcuts aren't available on this Wayland compositor.",
                        );
                        ui.add_space(2.0);
                        ui.label("The keys below work only when Rumble's window is focused.");
                        ui.add_space(4.0);
                        ui.colored_label(
                            egui::Color32::from_rgb(160, 160, 160),
                            "Supported compositors: KDE Plasma 5.27+, GNOME 47+, Hyprland",
                        );
                    });
            }
        } else {
            // Not Wayland - show standard global hotkey controls
            ui.strong("Global Hotkeys");
            ui.add_space(4.0);
            let mut global_enabled = self.persistent_settings.keyboard.global_hotkeys_enabled;
            if ui
                .checkbox(&mut global_enabled, "Enable global hotkeys")
                .on_hover_text("When enabled, hotkeys work even when the window is not focused")
                .changed()
            {
                self.persistent_settings.keyboard.global_hotkeys_enabled = global_enabled;
                self.settings_modal.dirty = true;
            }

            ui.add_space(4.0);
            ui.colored_label(
                egui::Color32::from_rgb(150, 150, 150),
                "Note: Changes to global hotkeys require restarting the application.",
            );
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);

        // --- Window-Focused Keys Section ---
        if portal_active {
            ui.strong("Window-Focused Keys");
            ui.add_space(4.0);
            ui.colored_label(
                egui::Color32::from_rgb(160, 160, 160),
                "Fallback bindings used when the Rumble window is focused.",
            );
            ui.add_space(8.0);
        }

        // Helper: render a registration status indicator (only for non-portal mode)
        let render_status_indicator = |ui: &mut egui::Ui, status: HotkeyRegistrationStatus, show: bool| {
            if !show {
                return;
            }
            let (color, tooltip) = match status {
                HotkeyRegistrationStatus::Registered => {
                    (egui::Color32::from_rgb(80, 200, 80), "Global hotkey registered")
                }
                HotkeyRegistrationStatus::Failed => (
                    egui::Color32::from_rgb(220, 60, 60),
                    "Global hotkey registration failed",
                ),
                HotkeyRegistrationStatus::NotConfigured => {
                    (egui::Color32::from_rgb(128, 128, 128), "No global hotkey configured")
                }
            };
            let (rect, response) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 5.0, color);
            response.on_hover_text(tooltip);
        };

        // Helper: check if a binding conflicts with another action's binding
        let find_conflict = |binding: &HotkeyBinding,
                             target: HotkeyCaptureTarget,
                             keyboard: &crate::settings::KeyboardSettings|
         -> Option<&'static str> {
            let bindings: [(HotkeyCaptureTarget, &Option<HotkeyBinding>, &str); 3] = [
                (HotkeyCaptureTarget::Ptt, &keyboard.ptt_hotkey, "Push-to-Talk"),
                (
                    HotkeyCaptureTarget::ToggleMute,
                    &keyboard.toggle_mute_hotkey,
                    "Toggle Mute",
                ),
                (
                    HotkeyCaptureTarget::ToggleDeafen,
                    &keyboard.toggle_deafen_hotkey,
                    "Toggle Deafen",
                ),
            ];
            for (other_target, other_binding, name) in &bindings {
                if *other_target != target {
                    if let Some(existing) = other_binding {
                        if existing == binding {
                            return Some(name);
                        }
                    }
                }
            }
            None
        };

        // Whether to show registration status dots (hide in Portal mode since they
        // reflect fallback registration, not Portal binding status)
        let show_status_dots = !portal_active;

        // Helper to render a hotkey binding row with capture and status indicator
        let render_hotkey_row = |ui: &mut egui::Ui,
                                 label: &str,
                                 binding: &Option<HotkeyBinding>,
                                 target: HotkeyCaptureTarget,
                                 capture_target: &mut Option<HotkeyCaptureTarget>,
                                 dirty: &mut bool,
                                 allow_edit: bool,
                                 reg_status: HotkeyRegistrationStatus,
                                 show_dots: bool|
         -> Option<Option<HotkeyBinding>> {
            let mut result = None;

            ui.horizontal(|ui| {
                render_status_indicator(ui, reg_status, show_dots);
                ui.strong(label);
            });
            ui.add_space(4.0);

            ui.horizontal(|ui| {
                let is_capturing = *capture_target == Some(target);

                if is_capturing {
                    // Show highlighted capture instruction box
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgba_premultiplied(30, 80, 120, 200))
                        .corner_radius(4.0)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(100, 220, 255),
                                "Press the desired key combination, then release. Press Escape to cancel.",
                            );
                        });
                } else {
                    // Show current binding
                    let binding_label = binding
                        .as_ref()
                        .map(|h| h.display())
                        .unwrap_or_else(|| "Not set".to_string());

                    ui.monospace(&binding_label);

                    if allow_edit {
                        if ui.button("Change").clicked() {
                            *capture_target = Some(target);
                        }

                        if binding.is_some() && ui.button("Clear").clicked() {
                            result = Some(None);
                            *dirty = true;
                        }
                    }
                }
            });

            result
        };

        // Capture key input if we're in capture mode using events API
        let mut captured_binding: Option<(HotkeyCaptureTarget, HotkeyBinding)> = None;
        let mut cancel_capture = false;

        if let Some(target) = self.settings_modal.hotkey_capture_target {
            let events = ui.ctx().input(|i| i.events.clone());
            for event in &events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    repeat: false,
                    ..
                } = event
                {
                    if *key == egui::Key::Escape {
                        cancel_capture = true;
                        break;
                    }
                    if let Some(key_str) = HotkeyManager::egui_key_to_string(*key) {
                        captured_binding = Some((
                            target,
                            HotkeyBinding {
                                modifiers: HotkeyModifiers {
                                    ctrl: modifiers.ctrl,
                                    shift: modifiers.shift,
                                    alt: modifiers.alt,
                                    super_key: modifiers.command,
                                },
                                key: key_str,
                            },
                        ));
                        break;
                    }
                }
            }
        }

        // Snapshot registration status
        let ptt_status = self
            .hotkey_registration_status
            .get(&HotkeyAction::PushToTalk)
            .copied()
            .unwrap_or(HotkeyRegistrationStatus::NotConfigured);
        let mute_status = self
            .hotkey_registration_status
            .get(&HotkeyAction::ToggleMute)
            .copied()
            .unwrap_or(HotkeyRegistrationStatus::NotConfigured);
        let deafen_status = self
            .hotkey_registration_status
            .get(&HotkeyAction::ToggleDeafen)
            .copied()
            .unwrap_or(HotkeyRegistrationStatus::NotConfigured);

        // PTT Hotkey
        if let Some(new_binding) = render_hotkey_row(
            ui,
            "Push-to-Talk",
            &self.persistent_settings.keyboard.ptt_hotkey,
            HotkeyCaptureTarget::Ptt,
            &mut self.settings_modal.hotkey_capture_target,
            &mut self.settings_modal.dirty,
            show_capture_ui,
            ptt_status,
            show_status_dots,
        ) {
            self.persistent_settings.keyboard.ptt_hotkey = new_binding;
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);

        // Toggle Mute Hotkey
        if let Some(new_binding) = render_hotkey_row(
            ui,
            "Toggle Mute",
            &self.persistent_settings.keyboard.toggle_mute_hotkey,
            HotkeyCaptureTarget::ToggleMute,
            &mut self.settings_modal.hotkey_capture_target,
            &mut self.settings_modal.dirty,
            show_capture_ui,
            mute_status,
            show_status_dots,
        ) {
            self.persistent_settings.keyboard.toggle_mute_hotkey = new_binding;
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);

        // Toggle Deafen Hotkey
        if let Some(new_binding) = render_hotkey_row(
            ui,
            "Toggle Deafen",
            &self.persistent_settings.keyboard.toggle_deafen_hotkey,
            HotkeyCaptureTarget::ToggleDeafen,
            &mut self.settings_modal.hotkey_capture_target,
            &mut self.settings_modal.dirty,
            show_capture_ui,
            deafen_status,
            show_status_dots,
        ) {
            self.persistent_settings.keyboard.toggle_deafen_hotkey = new_binding;
        }

        // Apply captured binding (with conflict detection)
        if let Some((target, binding)) = captured_binding {
            // Check for conflicts with other bindings
            if let Some(conflict_name) = find_conflict(&binding, target, &self.persistent_settings.keyboard) {
                // Store the conflict for user confirmation (persists across frames)
                self.settings_modal.hotkey_conflict_pending = Some((target, binding, conflict_name));
            } else {
                // No conflict - apply directly
                match target {
                    HotkeyCaptureTarget::Ptt => {
                        self.persistent_settings.keyboard.ptt_hotkey = Some(binding);
                    }
                    HotkeyCaptureTarget::ToggleMute => {
                        self.persistent_settings.keyboard.toggle_mute_hotkey = Some(binding);
                    }
                    HotkeyCaptureTarget::ToggleDeafen => {
                        self.persistent_settings.keyboard.toggle_deafen_hotkey = Some(binding);
                    }
                }
                self.settings_modal.hotkey_capture_target = None;
                self.settings_modal.dirty = true;
            }
        }

        // Show conflict warning UI if a conflict is pending (only relevant in non-portal mode)
        if !portal_active {
            if let Some((target, ref binding, conflict_name)) = self.settings_modal.hotkey_conflict_pending.clone() {
                ui.add_space(8.0);
                egui::Frame::new()
                    .fill(egui::Color32::from_rgba_premultiplied(80, 60, 20, 200))
                    .corner_radius(4.0)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 200, 80),
                            format!(
                                "This key is already bound to {}. Setting it here will remove the other binding.",
                                conflict_name
                            ),
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Apply anyway").clicked() {
                                // Clear the conflicting binding
                                if self.persistent_settings.keyboard.ptt_hotkey.as_ref() == Some(&binding)
                                    && target != HotkeyCaptureTarget::Ptt
                                {
                                    self.persistent_settings.keyboard.ptt_hotkey = None;
                                }
                                if self.persistent_settings.keyboard.toggle_mute_hotkey.as_ref() == Some(&binding)
                                    && target != HotkeyCaptureTarget::ToggleMute
                                {
                                    self.persistent_settings.keyboard.toggle_mute_hotkey = None;
                                }
                                if self.persistent_settings.keyboard.toggle_deafen_hotkey.as_ref() == Some(&binding)
                                    && target != HotkeyCaptureTarget::ToggleDeafen
                                {
                                    self.persistent_settings.keyboard.toggle_deafen_hotkey = None;
                                }
                                // Apply the new binding
                                match target {
                                    HotkeyCaptureTarget::Ptt => {
                                        self.persistent_settings.keyboard.ptt_hotkey = Some(binding.clone());
                                    }
                                    HotkeyCaptureTarget::ToggleMute => {
                                        self.persistent_settings.keyboard.toggle_mute_hotkey = Some(binding.clone());
                                    }
                                    HotkeyCaptureTarget::ToggleDeafen => {
                                        self.persistent_settings.keyboard.toggle_deafen_hotkey = Some(binding.clone());
                                    }
                                }
                                self.settings_modal.hotkey_capture_target = None;
                                self.settings_modal.hotkey_conflict_pending = None;
                                self.settings_modal.dirty = true;
                            }
                            if ui.button("Cancel").clicked() {
                                self.settings_modal.hotkey_capture_target = None;
                                self.settings_modal.hotkey_conflict_pending = None;
                            }
                        });
                    });
            }
        }

        // Cancel capture if escape was pressed
        if cancel_capture {
            self.settings_modal.hotkey_capture_target = None;
            self.settings_modal.hotkey_conflict_pending = None;
        }
    }

    /// Render the first-run setup dialog for key configuration.
    /// Returns true if setup is complete and the dialog should close.
    fn render_first_run_dialog(&mut self, ctx: &egui::Context) {
        // Don't show if not in first-run mode
        if matches!(self.first_run_state, FirstRunState::NotNeeded | FirstRunState::Complete) {
            return;
        }

        // Track what action to take after the UI is rendered
        // This avoids borrow checker issues with closures
        let mut next_state: Option<FirstRunState> = None;
        let mut generate_key_password: Option<Option<String>> = None;
        let mut select_agent_key: Option<KeyInfo> = None;
        let mut generate_agent_key_comment: Option<String> = None;

        let modal = Modal::new(egui::Id::new("first_run_modal")).show(ctx, |ui| {
            ui.set_min_width(400.0);

            // Clone the state to avoid borrowing issues
            let state = self.first_run_state.clone();

            match state {
                FirstRunState::SelectMethod => {
                    ui.heading("Welcome to Rumble! 🎤");
                    ui.add_space(8.0);

                    ui.label("Rumble uses Ed25519 cryptographic keys for secure authentication.");
                    ui.label("This key is your identity and will be used to identify you on servers.");

                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(16.0);

                    ui.heading("Choose how to store your key:");
                    ui.add_space(12.0);

                    // Option 1: Generate local key (simple)
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong("🔑 Generate Local Key");
                            ui.label("(Recommended for most users)");
                        });
                        ui.label("A new key will be generated and stored on this computer.");
                        ui.label("You can optionally protect it with a password.");
                        ui.add_space(8.0);
                        if ui.button("Generate Local Key →").clicked() {
                            next_state = Some(FirstRunState::GenerateLocal {
                                password: String::new(),
                                password_confirm: String::new(),
                                error: None,
                            });
                        }
                    });

                    ui.add_space(8.0);

                    // Option 2: SSH Agent (advanced)
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong("🔐 Use SSH Agent");
                            ui.label("(Advanced)");
                        });
                        ui.label("Use a key stored in your SSH agent for better security.");
                        ui.label("Requires ssh-agent to be running with Ed25519 keys.");

                        // Check if SSH agent is available
                        let agent_available = SshAgentClient::is_available();

                        ui.add_space(8.0);
                        if !agent_available {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "⚠ SSH_AUTH_SOCK not set - ssh-agent not available",
                            );
                        }

                        ui.add_enabled_ui(agent_available, |ui| {
                            if ui.button("Connect to SSH Agent →").clicked() {
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
                    ui.heading("Generate Local Key");
                    ui.add_space(8.0);

                    ui.label("Your key will be stored locally. Optionally add a password for extra security.");
                    ui.label("If you set a password, you'll need to enter it each time you start Rumble.");

                    ui.add_space(16.0);

                    // Clone values for editing
                    let mut pw = password.clone();
                    let mut pw_confirm = password_confirm.clone();

                    ui.horizontal(|ui| {
                        ui.label("Password (optional):");
                        ui.add(egui::TextEdit::singleline(&mut pw).password(true));
                    });

                    ui.horizontal(|ui| {
                        ui.label("Confirm password:");
                        ui.add(egui::TextEdit::singleline(&mut pw_confirm).password(true));
                    });

                    // If text changed, update the state
                    if pw != password || pw_confirm != password_confirm {
                        next_state = Some(FirstRunState::GenerateLocal {
                            password: pw.clone(),
                            password_confirm: pw_confirm.clone(),
                            error: error.clone(),
                        });
                    }

                    if let Some(err) = &error {
                        ui.add_space(8.0);
                        ui.colored_label(egui::Color32::RED, err);
                    }

                    // Show password mismatch warning
                    if !pw.is_empty() && !pw_confirm.is_empty() && pw != pw_confirm {
                        ui.add_space(4.0);
                        ui.colored_label(egui::Color32::YELLOW, "⚠ Passwords don't match");
                    }

                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        if ui.button("← Back").clicked() {
                            next_state = Some(FirstRunState::SelectMethod);
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let can_generate = pw.is_empty() || pw == pw_confirm;

                            if ui
                                .add_enabled(can_generate, egui::Button::new("Generate Key ✓"))
                                .on_disabled_hover_text("Passwords don't match")
                                .clicked()
                            {
                                // Signal that we should generate a key
                                let password_opt = if pw.is_empty() { None } else { Some(pw.clone()) };
                                generate_key_password = Some(password_opt);
                            }
                        });
                    });
                }

                FirstRunState::ConnectingAgent => {
                    ui.heading("Connecting to SSH Agent...");
                    ui.add_space(16.0);
                    ui.spinner();
                    ui.label("Fetching Ed25519 keys from ssh-agent...");

                    ui.add_space(16.0);
                    if ui.button("← Cancel").clicked() {
                        next_state = Some(FirstRunState::SelectMethod);
                    }
                }

                FirstRunState::SelectAgentKey { keys, selected, error } => {
                    ui.heading("Select Key from SSH Agent");
                    ui.add_space(8.0);

                    let mut new_selected = selected;

                    if keys.is_empty() {
                        ui.label("No Ed25519 keys found in the SSH agent.");
                        ui.label("You can generate a new key to add to the agent.");
                    } else {
                        ui.label("Select an existing key or generate a new one:");
                        ui.add_space(8.0);

                        for (i, key) in keys.iter().enumerate() {
                            let text = format!("{} ({})", key.comment, key.fingerprint);
                            if ui.selectable_label(new_selected == Some(i), text).clicked() {
                                new_selected = Some(i);
                            }
                        }
                    }

                    // Update selection if changed
                    if new_selected != selected {
                        next_state = Some(FirstRunState::SelectAgentKey {
                            keys: keys.clone(),
                            selected: new_selected,
                            error: error.clone(),
                        });
                    }

                    if let Some(err) = &error {
                        ui.add_space(8.0);
                        ui.colored_label(egui::Color32::RED, err);
                    }

                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        if ui.button("← Back").clicked() {
                            next_state = Some(FirstRunState::SelectMethod);
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let can_select = new_selected.is_some();
                            if ui
                                .add_enabled(can_select, egui::Button::new("Use Selected Key"))
                                .clicked()
                            {
                                // Signal to save the selected key
                                if let Some(idx) = new_selected {
                                    if let Some(key) = keys.get(idx) {
                                        select_agent_key = Some(key.clone());
                                    }
                                }
                            }

                            if ui.button("Generate New Key").clicked() {
                                next_state = Some(FirstRunState::GenerateAgentKey {
                                    comment: "rumble-identity".to_string(),
                                });
                            }
                        });
                    });
                }

                FirstRunState::GenerateAgentKey { comment } => {
                    ui.heading("Generate Key for SSH Agent");
                    ui.add_space(8.0);

                    ui.label("A new Ed25519 key will be generated and added to your SSH agent.");

                    ui.add_space(8.0);

                    let mut c = comment.clone();
                    ui.horizontal(|ui| {
                        ui.label("Key comment:");
                        ui.text_edit_singleline(&mut c);
                    });

                    if c != comment {
                        next_state = Some(FirstRunState::GenerateAgentKey { comment: c.clone() });
                    }

                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        if ui.button("← Back").clicked() {
                            // Go back to agent to re-list keys
                            next_state = Some(FirstRunState::ConnectingAgent);
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let generating = self.pending_agent_op.is_some();
                            if generating {
                                ui.spinner();
                                ui.label("Generating...");
                            } else if ui.button("Generate and Add to Agent").clicked() {
                                // Signal to generate and add key to agent
                                generate_agent_key_comment = Some(c.clone());
                            }
                        });
                    });
                }

                FirstRunState::Error { message } => {
                    ui.heading("⚠ Error");
                    ui.add_space(8.0);

                    ui.colored_label(egui::Color32::RED, &message);

                    ui.add_space(16.0);

                    if ui.button("← Back to Start").clicked() {
                        next_state = Some(FirstRunState::SelectMethod);
                    }
                }

                FirstRunState::NotNeeded | FirstRunState::Complete => {
                    // Should not be shown, but handle gracefully
                    ui.close();
                }
            }
        });

        // Apply state changes after the UI is done
        if let Some(new_state) = next_state {
            // If transitioning to ConnectingAgent, start the async operation
            if matches!(new_state, FirstRunState::ConnectingAgent) && self.pending_agent_op.is_none() {
                let handle = self.tokio_handle.spawn(connect_and_list_keys());
                self.pending_agent_op = Some(PendingAgentOp::Connect(handle));
            }
            self.first_run_state = new_state;
        }

        // Poll pending async operation
        if let Some(pending_op) = &mut self.pending_agent_op {
            match pending_op {
                PendingAgentOp::Connect(handle) => {
                    if handle.is_finished() {
                        // Take the handle and get the result
                        if let Some(PendingAgentOp::Connect(handle)) = self.pending_agent_op.take() {
                            match self.tokio_handle.block_on(handle) {
                                Ok(Ok(keys)) => {
                                    self.first_run_state = FirstRunState::SelectAgentKey {
                                        keys,
                                        selected: None,
                                        error: None,
                                    };
                                }
                                Ok(Err(e)) => {
                                    self.first_run_state = FirstRunState::Error {
                                        message: format!("Failed to connect to SSH agent: {}", e),
                                    };
                                }
                                Err(e) => {
                                    self.first_run_state = FirstRunState::Error {
                                        message: format!("Agent operation panicked: {}", e),
                                    };
                                }
                            }
                        }
                    }
                }
                PendingAgentOp::AddKey(handle) => {
                    if handle.is_finished() {
                        if let Some(PendingAgentOp::AddKey(handle)) = self.pending_agent_op.take() {
                            match self.tokio_handle.block_on(handle) {
                                Ok(Ok(key_info)) => {
                                    // Save the key config
                                    if let Err(e) = self.key_manager.select_agent_key(&key_info) {
                                        self.first_run_state = FirstRunState::Error {
                                            message: format!("Failed to save key config: {}", e),
                                        };
                                    } else {
                                        self.first_run_state = FirstRunState::Complete;
                                        self.backend.send(Command::LocalMessage {
                                            text: format!("Added new key to SSH agent: {}", key_info.fingerprint),
                                        });
                                    }
                                }
                                Ok(Err(e)) => {
                                    self.first_run_state = FirstRunState::Error {
                                        message: format!("Failed to add key to agent: {}", e),
                                    };
                                }
                                Err(e) => {
                                    self.first_run_state = FirstRunState::Error {
                                        message: format!("Agent operation panicked: {}", e),
                                    };
                                }
                            }
                        }
                    }
                }
                PendingAgentOp::Sign(_) => {
                    // Sign operations are handled elsewhere
                }
            }
        }

        // Handle key generation if requested
        if let Some(password_opt) = generate_key_password {
            let password_str = password_opt.as_deref();
            match self.key_manager.generate_local_key(password_str) {
                Ok(key_info) => {
                    // Get the signing key
                    self.signing_key = self.key_manager.signing_key().cloned();
                    self.first_run_state = FirstRunState::Complete;

                    self.backend.send(Command::LocalMessage {
                        text: format!("Identity key generated: {}", key_info.fingerprint),
                    });
                }
                Err(e) => {
                    self.first_run_state = FirstRunState::GenerateLocal {
                        password: password_opt.clone().unwrap_or_default(),
                        password_confirm: password_opt.unwrap_or_default(),
                        error: Some(format!("Failed to generate key: {}", e)),
                    };
                }
            }
        }

        // Handle selecting an agent key
        if let Some(key_info) = select_agent_key {
            match self.key_manager.select_agent_key(&key_info) {
                Ok(()) => {
                    // Agent keys don't have a signing_key cached locally
                    self.signing_key = None;
                    self.first_run_state = FirstRunState::Complete;

                    self.backend.send(Command::LocalMessage {
                        text: format!("Using SSH agent key: {} ({})", key_info.comment, key_info.fingerprint),
                    });
                }
                Err(e) => {
                    self.first_run_state = FirstRunState::Error {
                        message: format!("Failed to save key config: {}", e),
                    };
                }
            }
        }

        // Handle generating and adding a key to the agent
        if let Some(comment) = generate_agent_key_comment {
            if self.pending_agent_op.is_none() {
                let handle = self.tokio_handle.spawn(generate_and_add_to_agent(comment));
                self.pending_agent_op = Some(PendingAgentOp::AddKey(handle));
            }
        }

        // Don't allow closing via escape/click outside during first run
        let _ = modal;
    }
}

/// Render a single schema field and return true if it was modified.
fn render_schema_field(
    ui: &mut egui::Ui,
    key: &str,
    prop_schema: &serde_json::Value,
    settings: &mut serde_json::Value,
) -> bool {
    let title = prop_schema.get("title").and_then(|t| t.as_str()).unwrap_or(key);
    let description = prop_schema.get("description").and_then(|d| d.as_str()).unwrap_or("");
    let prop_type = prop_schema.get("type").and_then(|t| t.as_str()).unwrap_or("string");

    let mut changed = false;

    ui.horizontal(|ui| {
        ui.label(format!("{}:", title));

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

                if ui
                    .add(egui::Slider::new(&mut value, min..=max))
                    .on_hover_text(description)
                    .changed()
                {
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

                if ui
                    .add(egui::Slider::new(&mut value, min..=max))
                    .on_hover_text(description)
                    .changed()
                {
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
                // String or unknown type - show as text field
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

impl RumbleApp {
    /// Render one frame of the application.
    ///
    /// This is the main entry point for UI rendering. It should be called each frame
    /// by the window runner (eframe or test harness).
    pub fn render(&mut self, ctx: &egui::Context) {
        self.egui_ctx = ctx.clone();

        // Get current state from backend (clone to avoid borrow issues)
        let state = self.backend.state();

        // Detect connection state transitions and fire toasts + SFX
        if state.connection != self.prev_connection_state {
            match &state.connection {
                ConnectionState::Connected { .. } => {
                    self.toast_manager.success("Connected to server");
                    self.play_sfx(SfxKind::Connect);
                }
                ConnectionState::ConnectionLost { error } => {
                    self.toast_manager.error(format!("Connection lost: {}", error));
                    self.play_sfx(SfxKind::Disconnect);
                }
                ConnectionState::Disconnected => {
                    // Only play disconnect sound if we were previously connected
                    if matches!(self.prev_connection_state, ConnectionState::Connected { .. }) {
                        self.play_sfx(SfxKind::Disconnect);
                    }
                }
                ConnectionState::Connecting { .. } => {
                    // Only show reconnecting toast if we were previously connected or lost
                    if matches!(
                        self.prev_connection_state,
                        ConnectionState::Connected { .. } | ConnectionState::ConnectionLost { .. }
                    ) {
                        self.toast_manager.info("Reconnecting to server...");
                    }
                }
                _ => {}
            }
            self.prev_connection_state = state.connection.clone();
        }

        // Detect mute/unmute transitions for SFX
        {
            let current_muted = state.audio.self_muted;
            if current_muted != self.prev_self_muted {
                if current_muted {
                    self.play_sfx(SfxKind::Mute);
                } else {
                    self.play_sfx(SfxKind::Unmute);
                }
                self.prev_self_muted = current_muted;
            }
        }

        // Detect user join/leave in current room for SFX
        if state.connection.is_connected() {
            if let Some(my_room_id) = &state.my_room_id {
                let current_user_ids: std::collections::HashSet<u64> = state
                    .users_in_room(*my_room_id)
                    .iter()
                    .filter_map(|u| u.user_id.as_ref().map(|id| id.value))
                    .filter(|id| state.my_user_id != Some(*id))
                    .collect();

                // Only detect diffs after the first observation (skip initial connect)
                if self.prev_user_ids_initialized {
                    for id in &current_user_ids {
                        if !self.prev_user_ids_in_room.contains(id) {
                            self.play_sfx(SfxKind::UserJoin);
                            break; // One sound per frame is enough
                        }
                    }
                    for id in &self.prev_user_ids_in_room {
                        if !current_user_ids.contains(id) {
                            self.play_sfx(SfxKind::UserLeave);
                            break;
                        }
                    }
                }

                self.prev_user_ids_in_room = current_user_ids;
                self.prev_user_ids_initialized = true;
            }
        } else {
            self.prev_user_ids_in_room.clear();
            self.prev_user_ids_initialized = false;
        }

        // Detect new non-local chat messages for SFX
        {
            let current_count = state.chat_messages.len();
            if current_count > self.prev_chat_count && self.prev_chat_count > 0 {
                // Check if any of the new messages are non-local
                let has_new_remote = state.chat_messages[self.prev_chat_count..].iter().any(|m| !m.is_local);
                if has_new_remote {
                    self.play_sfx(SfxKind::Message);
                }
            }
            self.prev_chat_count = current_count;
        }

        // Poll pending file dialog
        if let Some(handle) = &self.pending_file_dialog {
            if handle.is_finished() {
                if let Some(handle) = self.pending_file_dialog.take() {
                    match self.tokio_handle.block_on(handle) {
                        Ok(Some(path)) => {
                            self.backend.send(Command::ShareFile { path });
                            self.show_transfers = true;
                        }
                        Ok(None) => {
                            // User cancelled the dialog
                        }
                        Err(e) => {
                            tracing::error!("File dialog task panicked: {}", e);
                        }
                    }
                }
            }
        }

        // Poll pending save dialog
        if let Some(handle) = &self.pending_save_dialog {
            if handle.is_finished() {
                if let Some(handle) = self.pending_save_dialog.take() {
                    match self.tokio_handle.block_on(handle) {
                        Ok(Some((infohash, destination))) => {
                            self.backend.send(Command::SaveFileAs { infohash, destination });
                        }
                        Ok(None) => {
                            // User cancelled the dialog
                        }
                        Err(e) => {
                            tracing::error!("Save dialog task panicked: {}", e);
                        }
                    }
                }
            }
        }

        // Request periodic repaint when settings dialog is open (for level meters)
        // This ensures the UI updates even when no user interaction occurs
        if self.show_settings {
            ctx.request_repaint_after(std::time::Duration::from_millis(50)); // 20 FPS for meters
        }

        // Reset push-to-talk state if disconnected
        if !state.connection.is_connected() && self.push_to_talk_active {
            self.push_to_talk_active = false;
        }

        // Handle push-to-talk - window-focused fallback.
        // Global hotkeys are handled in EframeWrapper::update() via handle_hotkey_event().
        // This fallback ALWAYS runs to catch cases where global hotkeys don't work:
        // - Wayland (no global hotkey support)
        // - Global hotkey initialization failed
        // - Window is focused and user presses the key
        // The global hotkey handler runs first and sets push_to_talk_active, so this
        // won't double-trigger.
        //
        // NOTE: We skip PTT activation if a text field is focused (e.g., chat input)
        // to avoid triggering PTT while typing. We still detect key release to stop PTT.
        let text_input_focused = ctx.wants_keyboard_input();
        if let Some(ref binding) = self.persistent_settings.keyboard.ptt_hotkey {
            if let Some(egui_key) = crate::hotkeys::HotkeyManager::key_string_to_egui_key(&binding.key) {
                let key_pressed = ctx.input(|i| {
                    let key_down = i.key_down(egui_key);
                    // Only check modifiers if the binding has any
                    let has_modifiers = binding.modifiers.ctrl
                        || binding.modifiers.shift
                        || binding.modifiers.alt
                        || binding.modifiers.super_key;
                    if has_modifiers {
                        let mods_match = i.modifiers.ctrl == binding.modifiers.ctrl
                            && i.modifiers.shift == binding.modifiers.shift
                            && i.modifiers.alt == binding.modifiers.alt
                            && i.modifiers.command == binding.modifiers.super_key;
                        key_down && mods_match
                    } else {
                        key_down
                    }
                });
                // Only start PTT if not typing in a text field
                if key_pressed && !text_input_focused {
                    if !self.push_to_talk_active {
                        if state.connection.is_connected() {
                            tracing::debug!("PTT fallback: key pressed, starting transmit");
                            self.push_to_talk_active = true;
                            self.start_transmit();
                        } else {
                            // Log once per key press when not connected
                            tracing::trace!("PTT key detected but not connected - PTT requires server connection");
                        }
                    }
                } else if !key_pressed && self.push_to_talk_active {
                    // Always detect key release to stop PTT
                    tracing::debug!("PTT fallback: key released, stopping transmit");
                    self.push_to_talk_active = false;
                    self.stop_transmit();
                }
            }
        }

        // Handle mute toggle hotkey - window-focused fallback (same pattern as PTT)
        if let Some(ref binding) = self.persistent_settings.keyboard.toggle_mute_hotkey {
            if let Some(egui_key) = crate::hotkeys::HotkeyManager::key_string_to_egui_key(&binding.key) {
                let key_pressed = ctx.input(|i| {
                    // Use key_pressed for toggle (one-shot), not key_down (held)
                    let pressed = i.key_pressed(egui_key);
                    let has_modifiers = binding.modifiers.ctrl
                        || binding.modifiers.shift
                        || binding.modifiers.alt
                        || binding.modifiers.super_key;
                    if has_modifiers {
                        let mods_match = i.modifiers.ctrl == binding.modifiers.ctrl
                            && i.modifiers.shift == binding.modifiers.shift
                            && i.modifiers.alt == binding.modifiers.alt
                            && i.modifiers.command == binding.modifiers.super_key;
                        pressed && mods_match
                    } else {
                        pressed
                    }
                });
                if key_pressed && !text_input_focused && state.connection.is_connected() {
                    tracing::debug!("Mute toggle fallback: key pressed, toggling mute");
                    self.backend.send(Command::SetMuted {
                        muted: !state.audio.self_muted,
                    });
                }
            }
        }

        // Handle deafen toggle hotkey - window-focused fallback
        if let Some(ref binding) = self.persistent_settings.keyboard.toggle_deafen_hotkey {
            if let Some(egui_key) = crate::hotkeys::HotkeyManager::key_string_to_egui_key(&binding.key) {
                let key_pressed = ctx.input(|i| {
                    let pressed = i.key_pressed(egui_key);
                    let has_modifiers = binding.modifiers.ctrl
                        || binding.modifiers.shift
                        || binding.modifiers.alt
                        || binding.modifiers.super_key;
                    if has_modifiers {
                        let mods_match = i.modifiers.ctrl == binding.modifiers.ctrl
                            && i.modifiers.shift == binding.modifiers.shift
                            && i.modifiers.alt == binding.modifiers.alt
                            && i.modifiers.command == binding.modifiers.super_key;
                        pressed && mods_match
                    } else {
                        pressed
                    }
                });
                if key_pressed && !text_input_focused && state.connection.is_connected() {
                    tracing::debug!("Deafen toggle fallback: key pressed, toggling deafen");
                    self.backend.send(Command::SetDeafened {
                        deafened: !state.audio.self_deafened,
                    });
                }
            }
        }

        // Handle drag-and-drop file sharing
        let dropped_files = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped_files.is_empty() && state.connection.is_connected() {
            for file in dropped_files {
                if let Some(path) = file.path {
                    // Share the dropped file
                    self.backend.send(Command::ShareFile { path });
                    self.show_transfers = true;
                }
            }
        }

        // TODO: Ctrl+V image paste blocked by egui_winit swallowing Key::V
        // when text clipboard read fails. Track upstream: egui#2108.
        // For now, image paste is via the clipboard button in the chat bar.

        // Show drop target indicator when files are being dragged
        let is_dragging = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if is_dragging && state.connection.is_connected() {
            egui::Area::new(egui::Id::new("drop_target"))
                .fixed_pos(egui::pos2(0.0, 0.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    let rect = ui.ctx().available_rect();
                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgba_unmultiplied(0, 100, 200, 100))
                            .show(ui, |ui| {
                                ui.allocate_ui(rect.size(), |ui| {
                                    ui.centered_and_justified(|ui| {
                                        ui.heading("Drop files here to share");
                                    });
                                });
                            });
                    });
                });
        }

        // Download Modal
        if self.download_modal.open {
            egui::Modal::new(egui::Id::new("download_modal")).show(ctx, |ui| {
                ui.heading("Download File");
                ui.add_space(8.0);
                ui.label("Enter Magnet Link:");
                let response = ui.text_edit_singleline(&mut self.download_modal.magnet_link);
                if response.changed() {
                    // Re-validate on every edit
                    let link = self.download_modal.magnet_link.trim();
                    if link.is_empty() {
                        self.download_modal.validation_error = None;
                    } else if !is_valid_magnet(link) {
                        self.download_modal.validation_error = Some(
                            "Invalid magnet link: must start with \"magnet:?\" and contain \"xt=urn:btih:\""
                                .to_string(),
                        );
                    } else {
                        self.download_modal.validation_error = None;
                    }
                }
                if let Some(err) = &self.download_modal.validation_error {
                    ui.colored_label(egui::Color32::RED, err);
                }
                ui.add_space(8.0);
                let link_valid = is_valid_magnet(self.download_modal.magnet_link.trim());
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.download_modal.open = false;
                    }
                    let download_btn = ui.add_enabled(link_valid, egui::Button::new("Download"));
                    if download_btn.clicked() {
                        self.backend.send(Command::DownloadFile {
                            magnet: self.download_modal.magnet_link.clone(),
                        });
                        self.download_modal.open = false;
                        self.show_transfers = true;
                    }
                });
            });
        }

        // Transfers Window
        if self.show_transfers {
            egui::Window::new("File Transfers")
                .open(&mut self.show_transfers)
                .default_width(400.0)
                .show(ctx, |ui| {
                    if state.file_transfers.is_empty() {
                        ui.label("No active transfers.");
                    } else {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for transfer in &state.file_transfers {
                                ui.group(|ui| {
                                    // Status icon and name
                                    let (icon, status_text, color) = match transfer.state {
                                        TransferState::Pending => ("\u{231b}", "Pending", egui::Color32::GRAY),
                                        TransferState::Checking => ("\u{1f50d}", "Verifying...", egui::Color32::YELLOW),
                                        TransferState::Downloading => {
                                            ("\u{2b07}", "Downloading", egui::Color32::from_rgb(80, 140, 255))
                                        }
                                        TransferState::Seeding => ("\u{2b06}", "Seeding", egui::Color32::GREEN),
                                        TransferState::Paused => {
                                            ("\u{23f8}", "Paused", egui::Color32::from_rgb(160, 160, 160))
                                        }
                                        TransferState::Completed => ("\u{2714}", "Done", egui::Color32::GREEN),
                                        TransferState::Error => ("\u{2718}", "Error", egui::Color32::RED),
                                    };

                                    ui.horizontal(|ui| {
                                        ui.colored_label(color, icon);
                                        let name_text = egui::RichText::new(&transfer.name).color(color);
                                        ui.strong(name_text);
                                    });

                                    // Size info
                                    let size_str = format_size(transfer.size);
                                    ui.horizontal(|ui| {
                                        ui.colored_label(color, format!("{} \u{00b7} {}", size_str, status_text));
                                        if transfer.peers > 0 {
                                            ui.label(format!("\u{00b7} {} peers", transfer.peers));
                                        }
                                    });

                                    // Progress bar for downloading
                                    if matches!(transfer.state, TransferState::Downloading) {
                                        ui.add(
                                            egui::ProgressBar::new(transfer.progress)
                                                .show_percentage()
                                                .animate(true),
                                        );

                                        // Speed info
                                        if transfer.download_speed > 0 || transfer.upload_speed > 0 {
                                            ui.horizontal(|ui| {
                                                if transfer.download_speed > 0 {
                                                    ui.colored_label(
                                                        egui::Color32::from_rgb(80, 140, 255),
                                                        format!("\u{2b07} {}/s", format_size(transfer.download_speed)),
                                                    );
                                                }
                                                if transfer.upload_speed > 0 {
                                                    ui.colored_label(
                                                        egui::Color32::GREEN,
                                                        format!("\u{2b06} {}/s", format_size(transfer.upload_speed)),
                                                    );
                                                }
                                            });
                                        }
                                    }

                                    // Progress bar for checking/verifying
                                    if matches!(transfer.state, TransferState::Checking) {
                                        ui.add(
                                            egui::ProgressBar::new(transfer.progress)
                                                .show_percentage()
                                                .animate(true),
                                        );
                                    }

                                    // Seeding stats
                                    if matches!(transfer.state, TransferState::Seeding) {
                                        ui.horizontal(|ui| {
                                            if transfer.upload_speed > 0 {
                                                ui.colored_label(
                                                    egui::Color32::GREEN,
                                                    format!("\u{2b06} {}/s", format_size(transfer.upload_speed)),
                                                );
                                            } else {
                                                ui.colored_label(egui::Color32::GREEN, "Seeding...");
                                            }
                                        });
                                    }

                                    // Error message
                                    if let Some(err) = &transfer.error {
                                        ui.colored_label(egui::Color32::RED, format!("\u{26a0} {}", err));
                                    }

                                    // Peer details (collapsible)
                                    if !transfer.peer_details.is_empty() {
                                        let infohash_hex = hex::encode(transfer.infohash);
                                        egui::CollapsingHeader::new(format!("Peers ({})", transfer.peer_details.len()))
                                            .id_salt(&infohash_hex)
                                            .show(ui, |ui| {
                                                use backend::events::{
                                                    PeerConnectionType, PeerState as TransferPeerState,
                                                };

                                                egui::Grid::new(format!("peer_grid_{}", infohash_hex))
                                                    .striped(true)
                                                    .spacing([10.0, 4.0])
                                                    .show(ui, |ui| {
                                                        // Header row
                                                        ui.strong("Address");
                                                        ui.strong("Type");
                                                        ui.strong("State");
                                                        ui.strong("Down");
                                                        ui.strong("Up");
                                                        ui.end_row();

                                                        for peer in &transfer.peer_details {
                                                            // Address (truncate if too long)
                                                            let addr_display = if peer.address.len() > 21 {
                                                                format!("{}...", &peer.address[..18])
                                                            } else {
                                                                peer.address.clone()
                                                            };
                                                            ui.label(&addr_display);

                                                            // Connection type with color
                                                            let (type_text, type_color) = match peer.connection_type {
                                                                PeerConnectionType::Direct => {
                                                                    ("Direct", egui::Color32::GREEN)
                                                                }
                                                                PeerConnectionType::Relay => {
                                                                    ("Relay", egui::Color32::YELLOW)
                                                                }
                                                                PeerConnectionType::Utp => {
                                                                    ("uTP", egui::Color32::LIGHT_BLUE)
                                                                }
                                                                PeerConnectionType::Socks => {
                                                                    ("SOCKS", egui::Color32::LIGHT_GRAY)
                                                                }
                                                            };
                                                            ui.colored_label(type_color, type_text);

                                                            // Peer state with color
                                                            let (state_text, state_color) = match peer.state {
                                                                TransferPeerState::Live => {
                                                                    ("Live", egui::Color32::GREEN)
                                                                }
                                                                TransferPeerState::Connecting => {
                                                                    ("Connecting", egui::Color32::YELLOW)
                                                                }
                                                                TransferPeerState::Queued => {
                                                                    ("Queued", egui::Color32::GRAY)
                                                                }
                                                                TransferPeerState::Dead => ("Dead", egui::Color32::RED),
                                                            };
                                                            ui.colored_label(state_color, state_text);

                                                            // Downloaded/uploaded bytes
                                                            ui.label(format_size(peer.downloaded_bytes));
                                                            ui.label(format_size(peer.uploaded_bytes));

                                                            ui.end_row();
                                                        }
                                                    });
                                            });
                                    }

                                    // Actions
                                    ui.horizontal(|ui| {
                                        let infohash = hex::encode(transfer.infohash);

                                        // Pause/Resume buttons
                                        match transfer.state {
                                            TransferState::Downloading
                                            | TransferState::Seeding
                                            | TransferState::Checking => {
                                                if ui.button("⏸ Pause").clicked() {
                                                    self.backend.send(Command::PauseTransfer {
                                                        infohash: infohash.clone(),
                                                    });
                                                }
                                            }
                                            TransferState::Paused => {
                                                if ui.button("▶ Resume").clicked() {
                                                    self.backend.send(Command::ResumeTransfer {
                                                        infohash: infohash.clone(),
                                                    });
                                                }
                                            }
                                            _ => {}
                                        }

                                        // Cancel button for active transfers
                                        if matches!(
                                            transfer.state,
                                            TransferState::Downloading
                                                | TransferState::Checking
                                                | TransferState::Pending
                                                | TransferState::Paused
                                        ) {
                                            if ui.button("✕ Cancel").clicked() {
                                                self.backend.send(Command::CancelTransfer {
                                                    infohash: infohash.clone(),
                                                });
                                            }
                                        }

                                        // Remove/Stop seeding button for completed transfers
                                        if matches!(transfer.state, TransferState::Seeding | TransferState::Completed) {
                                            if ui.button("🗑 Remove").clicked() {
                                                self.backend.send(Command::RemoveTransfer {
                                                    infohash: infohash.clone(),
                                                    delete_file: false,
                                                });
                                            }
                                        }

                                        // Save As button for completed downloads
                                        if matches!(transfer.state, TransferState::Seeding | TransferState::Completed) {
                                            if transfer.local_path.is_some() {
                                                // Store infohash for the save dialog
                                                let dialog_pending = self.pending_save_dialog.is_some();
                                                if ui
                                                    .add_enabled(!dialog_pending, egui::Button::new("💾 Save As..."))
                                                    .clicked()
                                                {
                                                    let infohash_for_dialog = infohash.clone();
                                                    let default_name = transfer.name.clone();
                                                    let handle = self.tokio_handle.spawn(async move {
                                                        rfd::AsyncFileDialog::new()
                                                            .set_file_name(&default_name)
                                                            .save_file()
                                                            .await
                                                            .map(|f| (infohash_for_dialog, f.path().to_path_buf()))
                                                    });
                                                    self.pending_save_dialog = Some(handle);
                                                }
                                            }
                                        }

                                        // Show magnet link copy for seeding transfers
                                        if matches!(transfer.state, TransferState::Seeding | TransferState::Completed) {
                                            if let Some(magnet) = &transfer.magnet {
                                                if ui.button("📋 Copy Magnet").clicked() {
                                                    ui.ctx().copy_text(magnet.clone());
                                                }
                                            }
                                        }
                                    });
                                });
                                ui.add_space(4.0);
                            }
                        });
                    }
                });
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
                            selected_categories: std::iter::once(SettingsCategory::default()).collect(),
                            pending_settings: Some(audio.settings.clone()),
                            pending_input_device: Some(audio.selected_input.clone()),
                            pending_output_device: Some(audio.selected_output.clone()),
                            pending_voice_mode: Some(audio.voice_mode.clone()),
                            pending_autoconnect: Some(self.autoconnect_on_launch),
                            pending_username: Some(self.client_name.clone()),
                            pending_tx_pipeline: Some(audio.tx_pipeline.clone()),
                            pending_show_timestamps: Some(self.persistent_settings.show_chat_timestamps),
                            pending_timestamp_format: Some(self.persistent_settings.chat_timestamp_format),
                            dirty: false,
                            hotkey_capture_target: None,
                            hotkey_conflict_pending: None,
                        };
                        self.show_settings = true;
                        ui.close();
                    }
                });

                ui.menu_button("File Transfer", |ui| {
                    // Don't allow opening another dialog while one is pending
                    let dialog_pending = self.pending_file_dialog.is_some();
                    if ui
                        .add_enabled(!dialog_pending, egui::Button::new("Share File..."))
                        .clicked()
                    {
                        // Spawn async file dialog to avoid blocking audio
                        let handle = self.tokio_handle.spawn(async {
                            rfd::AsyncFileDialog::new()
                                .pick_file()
                                .await
                                .map(|f| f.path().to_path_buf())
                        });
                        self.pending_file_dialog = Some(handle);
                        ui.close();
                    }
                    if ui.button("Download File...").clicked() {
                        self.download_modal.open = true;
                        self.download_modal.magnet_link.clear();
                        self.download_modal.validation_error = None;
                        ui.close();
                    }
                    if ui.button("Show Transfers").clicked() {
                        self.show_transfers = true;
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
                            ui.colored_label(egui::Color32::RED, format!("● Connection Lost: {}", error));
                        }
                        ConnectionState::CertificatePending { .. } => {
                            ui.colored_label(egui::Color32::YELLOW, "⚠ Certificate Verification");
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
                let mute_color = if audio.self_muted {
                    egui::Color32::RED
                } else {
                    egui::Color32::GREEN
                };
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new(mute_icon).size(18.0).color(mute_color),
                    ))
                    .on_hover_text(if audio.self_muted { "Unmute" } else { "Mute" })
                    .clicked()
                {
                    self.backend.send(Command::SetMuted {
                        muted: !audio.self_muted,
                    });
                }

                // Deafen button
                let deafen_icon = if audio.self_deafened { "🔇" } else { "🔊" };
                let deafen_color = if audio.self_deafened {
                    egui::Color32::RED
                } else {
                    egui::Color32::GREEN
                };
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new(deafen_icon).size(18.0).color(deafen_color),
                    ))
                    .on_hover_text(if audio.self_deafened { "Undeafen" } else { "Deafen" })
                    .clicked()
                {
                    self.backend.send(Command::SetDeafened {
                        deafened: !audio.self_deafened,
                    });
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
                        if ui
                            .selectable_label(matches!(audio.voice_mode, VoiceMode::PushToTalk), "🎤 Push-to-Talk")
                            .clicked()
                        {
                            self.backend.send(Command::SetVoiceMode {
                                mode: VoiceMode::PushToTalk,
                            });
                            // Set voice mode directly before saving (async command hasn't processed yet)
                            self.persistent_settings.voice_mode = PersistentVoiceMode::PushToTalk;
                            self.save_settings();
                        }
                        if ui
                            .selectable_label(matches!(audio.voice_mode, VoiceMode::Continuous), "📡 Continuous")
                            .clicked()
                        {
                            self.backend.send(Command::SetVoiceMode {
                                mode: VoiceMode::Continuous,
                            });
                            // Set voice mode directly before saving (async command hasn't processed yet)
                            self.persistent_settings.voice_mode = PersistentVoiceMode::Continuous;
                            self.save_settings();
                        }
                    });

                ui.separator();

                // Settings button
                if ui
                    .add(egui::Button::new(egui::RichText::new("⚙").size(18.0)))
                    .on_hover_text("Settings")
                    .clicked()
                {
                    // Initialize pending settings from current state
                    let audio = self.backend.state().audio.clone();
                    self.settings_modal = SettingsModalState {
                        selected_categories: std::iter::once(SettingsCategory::default()).collect(),
                        pending_settings: Some(audio.settings.clone()),
                        pending_input_device: Some(audio.selected_input.clone()),
                        pending_output_device: Some(audio.selected_output.clone()),
                        pending_voice_mode: Some(audio.voice_mode.clone()),
                        pending_autoconnect: Some(self.autoconnect_on_launch),
                        pending_username: Some(self.client_name.clone()),
                        pending_tx_pipeline: Some(audio.tx_pipeline.clone()),
                        pending_show_timestamps: Some(self.persistent_settings.show_chat_timestamps),
                        pending_timestamp_format: Some(self.persistent_settings.chat_timestamp_format),
                        dirty: false,
                        hotkey_capture_target: None,
                        hotkey_conflict_pending: None,
                    };
                    self.show_settings = true;
                }
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
                            // Collect file actions to trigger (can't trigger inside loop due to borrow)
                            let mut download_magnet: Option<String> = None;
                            let mut open_file_infohash: Option<String> = None;
                            let mut save_file_info: Option<(String, String)> = None; // (infohash, default_name)
                            let mut repost_text: Option<String> = None;
                            let mut pause_infohash: Option<String> = None;
                            let mut resume_infohash: Option<String> = None;
                            let mut cancel_infohash: Option<String> = None;

                            egui::ScrollArea::vertical()
                                .auto_shrink([false; 2])
                                .stick_to_bottom(true)
                                .show(ui, |ui| {
                                    if state.chat_messages.is_empty() {
                                        ui.vertical_centered(|ui| {
                                            ui.add_space(40.0);
                                            let text = if state.connection.is_connected() {
                                                "No messages yet"
                                            } else {
                                                "Connect to a server to start chatting"
                                            };
                                            ui.label(egui::RichText::new(text).italics().color(egui::Color32::GRAY));
                                        });
                                    }

                                    let show_timestamps = self.persistent_settings.show_chat_timestamps;
                                    let timestamp_format = self.persistent_settings.chat_timestamp_format;

                                    for msg in &state.chat_messages {
                                        let timestamp_prefix = if show_timestamps {
                                            format!("[{}] ", timestamp_format.format(msg.timestamp))
                                        } else {
                                            String::new()
                                        };

                                        if msg.is_local {
                                            // Local status messages in gray italics
                                            let text = format!("{}{}", timestamp_prefix, msg.text);
                                            ui.label(egui::RichText::new(text).italics().color(egui::Color32::GRAY));
                                        } else if let Some(p2p_msg) = P2pFileMessage::parse(&msg.text) {
                                            ui.group(|ui| {
                                                ui.horizontal(|ui| {
                                                    ui.label(egui::RichText::new("P2P File").strong());
                                                    ui.label(format!(
                                                        "{} ({} bytes)",
                                                        p2p_msg.file.name, p2p_msg.file.size
                                                    ));
                                                });

                                                ui.label(format!("Peer: {}", p2p_msg.file.peer_id));
                                                ui.label("Addresses:");
                                                for addr in &p2p_msg.file.addrs {
                                                    ui.label(egui::RichText::new(addr));
                                                }

                                                if ui.button("Download (P2P)").clicked() {
                                                    download_magnet = Some(p2p_msg.magnet_link());
                                                }
                                            });
                                        } else if let Some(file_msg) = FileMessage::parse(&msg.text) {
                                            // Look up transfer state for this file
                                            let infohash_hex = &file_msg.file.infohash;
                                            let infohash_bytes: Option<[u8; 20]> =
                                                hex::decode(infohash_hex).ok().and_then(|v| v.try_into().ok());

                                            let transfer = infohash_bytes.and_then(|hash| {
                                                state.file_transfers.iter().find(|t| t.infohash == hash)
                                            });

                                            let is_downloaded = transfer
                                                .map(|t| {
                                                    matches!(t.state, TransferState::Completed | TransferState::Seeding)
                                                })
                                                .unwrap_or(false);

                                            let is_downloading = transfer
                                                .map(|t| {
                                                    matches!(
                                                        t.state,
                                                        TransferState::Downloading | TransferState::Checking
                                                    )
                                                })
                                                .unwrap_or(false);

                                            let is_paused = transfer
                                                .map(|t| matches!(t.state, TransferState::Paused))
                                                .unwrap_or(false);

                                            let is_image = file_msg.file.mime.starts_with("image/");

                                            // File message - render as file widget
                                            ui.group(|ui| {
                                                // For downloaded images, show inline preview
                                                let mut image_shown = false;
                                                if is_downloaded && is_image {
                                                    if let Some(t) = transfer {
                                                        if let Some(ref path) = t.local_path {
                                                            // Create a unique URI for caching
                                                            let uri = format!("bytes://file_preview/{}", infohash_hex);

                                                            // Try to load the image bytes if not already cached
                                                            let bytes_available = match ui.ctx().try_load_bytes(&uri) {
                                                                Ok(_) => true,
                                                                Err(_) => {
                                                                    // Try to read and cache the file
                                                                    if let Ok(bytes) = std::fs::read(path) {
                                                                        ui.ctx().include_bytes(uri.clone(), bytes);
                                                                        true
                                                                    } else {
                                                                        false
                                                                    }
                                                                }
                                                            };

                                                            if bytes_available {
                                                                let response = ui.add(
                                                                    egui::Image::new(&uri)
                                                                        .max_width(300.0)
                                                                        .max_height(200.0)
                                                                        .corner_radius(4.0)
                                                                        .sense(egui::Sense::click()),
                                                                );
                                                                if response.hovered() {
                                                                    ui.ctx().set_cursor_icon(
                                                                        egui::CursorIcon::PointingHand,
                                                                    );
                                                                    // Draw hover border
                                                                    let rect = response.rect;
                                                                    let stroke = egui::Stroke::new(
                                                                        2.0,
                                                                        ui.visuals().selection.stroke.color,
                                                                    );
                                                                    ui.painter().rect_stroke(
                                                                        rect,
                                                                        4.0,
                                                                        stroke,
                                                                        egui::StrokeKind::Outside,
                                                                    );
                                                                }
                                                                if response.clicked() {
                                                                    self.image_view_modal = ImageViewModalState {
                                                                        open: true,
                                                                        image_uri: format!(
                                                                            "bytes://file_fullsize/{}",
                                                                            infohash_hex
                                                                        ),
                                                                        image_name: file_msg.file.name.clone(),
                                                                        file_path: Some(path.clone()),
                                                                        zoom: 1.0,
                                                                        fullsize_loaded: false,
                                                                    };
                                                                }
                                                                image_shown = true;
                                                            }
                                                        }
                                                    }
                                                }

                                                ui.horizontal(|ui| {
                                                    // File icon based on mime type (skip if image preview was shown)
                                                    if !image_shown {
                                                        let icon = get_file_icon(&file_msg.file.mime);
                                                        ui.label(egui::RichText::new(icon).size(24.0));
                                                    }

                                                    ui.vertical(|ui| {
                                                        // Sender and timestamp
                                                        ui.horizontal(|ui| {
                                                            ui.label(egui::RichText::new(&msg.sender).strong());
                                                            if !timestamp_prefix.is_empty() {
                                                                ui.label(
                                                                    egui::RichText::new(&timestamp_prefix)
                                                                        .small()
                                                                        .color(egui::Color32::GRAY),
                                                                );
                                                            }
                                                        });

                                                        // File name
                                                        ui.label(&file_msg.file.name);

                                                        // Size, mime type, and status
                                                        if is_downloading {
                                                            if let Some(t) = transfer {
                                                                let downloaded =
                                                                    (file_msg.file.size as f32 * t.progress) as u64;
                                                                ui.label(
                                                                    egui::RichText::new(format!(
                                                                        "{} / {} · {}/s",
                                                                        format_size(downloaded),
                                                                        format_size(file_msg.file.size),
                                                                        format_size(t.download_speed)
                                                                    ))
                                                                    .small()
                                                                    .color(egui::Color32::YELLOW),
                                                                );
                                                            }
                                                        } else if is_paused {
                                                            ui.label(
                                                                egui::RichText::new(format!(
                                                                    "{} · {} · Paused",
                                                                    format_size(file_msg.file.size),
                                                                    &file_msg.file.mime
                                                                ))
                                                                .small()
                                                                .color(egui::Color32::GRAY),
                                                            );
                                                        } else if is_downloaded {
                                                            ui.label(
                                                                egui::RichText::new(format!(
                                                                    "{} · {} · Downloaded ✓",
                                                                    format_size(file_msg.file.size),
                                                                    &file_msg.file.mime
                                                                ))
                                                                .small()
                                                                .color(egui::Color32::GREEN),
                                                            );
                                                        } else {
                                                            ui.label(
                                                                egui::RichText::new(format!(
                                                                    "{} · {}",
                                                                    format_size(file_msg.file.size),
                                                                    &file_msg.file.mime
                                                                ))
                                                                .small()
                                                                .color(egui::Color32::GRAY),
                                                            );
                                                        }
                                                    });
                                                });

                                                // Progress bar during download
                                                if is_downloading {
                                                    if let Some(t) = transfer {
                                                        ui.add(
                                                            egui::ProgressBar::new(t.progress)
                                                                .show_percentage()
                                                                .animate(true),
                                                        );
                                                    }
                                                }

                                                // Action buttons - different based on transfer state
                                                ui.horizontal(|ui| {
                                                    if is_downloaded {
                                                        // Downloaded: show Open, Save As, Repost buttons
                                                        if ui.button("📂 Open").clicked() {
                                                            open_file_infohash = Some(infohash_hex.clone());
                                                        }
                                                        if ui.button("💾 Save As...").clicked() {
                                                            save_file_info = Some((
                                                                infohash_hex.clone(),
                                                                file_msg.file.name.clone(),
                                                            ));
                                                        }
                                                        if ui.button("🔄 Repost").clicked() {
                                                            repost_text = Some(msg.text.clone());
                                                        }
                                                    } else if is_downloading {
                                                        // Downloading: show Pause, Cancel buttons
                                                        if ui.button("⏸ Pause").clicked() {
                                                            pause_infohash = Some(infohash_hex.clone());
                                                        }
                                                        if ui.button("✕ Cancel").clicked() {
                                                            cancel_infohash = Some(infohash_hex.clone());
                                                        }
                                                    } else if is_paused {
                                                        // Paused: show Resume, Cancel buttons
                                                        if ui.button("▶ Resume").clicked() {
                                                            resume_infohash = Some(infohash_hex.clone());
                                                        }
                                                        if ui.button("✕ Cancel").clicked() {
                                                            cancel_infohash = Some(infohash_hex.clone());
                                                        }
                                                    } else {
                                                        // Not downloaded: show Download, Copy Magnet buttons
                                                        if ui.button("⬇ Download").clicked() {
                                                            download_magnet = Some(file_msg.magnet_link());
                                                        }
                                                        if ui.button("📋 Copy Magnet").clicked() {
                                                            ui.ctx().copy_text(file_msg.magnet_link());
                                                        }
                                                    }
                                                });
                                            });
                                        } else {
                                            // Regular chat message
                                            ui.label(format!("{}{}: {}", timestamp_prefix, msg.sender, msg.text));
                                        }
                                    }
                                });

                            // Handle file actions
                            if let Some(magnet) = download_magnet {
                                self.backend.send(Command::DownloadFile { magnet });
                                self.show_transfers = true;
                            }
                            if let Some(infohash) = open_file_infohash {
                                self.backend.send(Command::OpenFile { infohash });
                            }
                            if let Some((infohash, default_name)) = save_file_info {
                                // Spawn async save dialog
                                let handle = self.tokio_handle.spawn(async move {
                                    rfd::AsyncFileDialog::new()
                                        .set_file_name(&default_name)
                                        .save_file()
                                        .await
                                        .map(|f| (infohash, f.path().to_path_buf()))
                                });
                                self.pending_save_dialog = Some(handle);
                            }
                            if let Some(text) = repost_text {
                                self.backend.send(Command::SendChat { text });
                            }
                            if let Some(infohash) = pause_infohash {
                                self.backend.send(Command::PauseTransfer { infohash });
                            }
                            if let Some(infohash) = resume_infohash {
                                self.backend.send(Command::ResumeTransfer { infohash });
                            }
                            if let Some(infohash) = cancel_infohash {
                                self.backend.send(Command::CancelTransfer { infohash });
                            }
                        });
                        strip.cell(|ui| {
                            ui.add_space(2.0);
                            ui.separator();
                        });
                        strip.cell(|ui| {
                            let connected = state.connection.is_connected();
                            if connected {
                                ui.horizontal(|ui| {
                                    let send = {
                                        let resp = ui.text_edit_singleline(&mut self.chat_input);
                                        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                    } || ui.button("Send").clicked();
                                    if send {
                                        let text = self.chat_input.trim();
                                        if !text.is_empty() {
                                            self.backend.send(Command::SendChat { text: text.to_owned() });
                                            self.chat_input.clear();
                                        }
                                    }

                                    if ui.button("📋").on_hover_text("Paste image from clipboard").clicked() {
                                        self.paste_clipboard_image();
                                    }

                                    if ui
                                        .button("↻ Sync")
                                        .on_hover_text("Request chat history from peers")
                                        .clicked()
                                    {
                                        self.backend.send(Command::RequestChatHistory);
                                    }
                                });
                            } else {
                                ui.horizontal(|ui| {
                                    ui.add_enabled(
                                        false,
                                        egui::TextEdit::singleline(&mut self.chat_input)
                                            .hint_text("Connect to a server to chat"),
                                    );
                                    ui.add_enabled(false, egui::Button::new("Send"));
                                });
                            }
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
                        self.join_room(api::ROOT_ROOM_UUID);
                    }
                    if ui.button("Refresh").clicked() {
                        self.join_room(api::ROOT_ROOM_UUID);
                    }
                });
                ui.separator();
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                // Collect pending commands to execute after tree view
                let mut pending_commands: Vec<Command> = Vec::new();
                let mut pending_rename: Option<(Option<Uuid>, String)> = None;
                let mut pending_delete: Option<(Uuid, String)> = None;
                let mut pending_description: Option<(Uuid, String, String)> = None;

                // Build set of rooms that have users in them or in any descendant
                let mut rooms_with_users: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
                for &room_id in state.room_tree.nodes.keys() {
                    if !state.users_in_room(room_id).is_empty() {
                        rooms_with_users.insert(room_id);
                        for ancestor in state.room_tree.ancestors(room_id) {
                            rooms_with_users.insert(ancestor);
                        }
                    }
                }

                let (_response, actions) = TreeView::<TreeNodeId>::new(egui::Id::new("room_tree"))
                    .override_indent(Some(20.0))
                    .show(ui, |builder| {
                        // Recursive helper to render a room and its children
                        fn render_room(
                            room_id: Uuid,
                            state: &backend::State,
                            rooms_with_users: &std::collections::HashSet<Uuid>,
                            pending_commands: &mut Vec<Command>,
                            pending_rename: &mut Option<(Option<Uuid>, String)>,
                            pending_delete: &mut Option<(Uuid, String)>,
                            pending_description: &mut Option<(Uuid, String, String)>,
                            builder: &mut egui_ltreeview::TreeViewBuilder<TreeNodeId>,
                        ) {
                            let Some(tree_node) = state.room_tree.get(room_id) else {
                                return;
                            };

                            let is_current = state.my_room_id == Some(room_id);
                            let is_root = room_id == api::ROOT_ROOM_UUID;
                            let has_users_in_subtree = rooms_with_users.contains(&room_id);
                            let user_count = state.users_in_room(room_id).len();

                            // Capture values for context menu closure
                            let room_name = tree_node.name.clone();
                            let room_description = tree_node.description.clone().unwrap_or_default();
                            let parent_id = tree_node.parent_id;

                            let room_label = {
                                let count_suffix = if user_count > 0 {
                                    format!(" ({})", user_count)
                                } else {
                                    String::new()
                                };
                                if is_current {
                                    format!("📁 {}{}  [current]", room_name, count_suffix)
                                } else {
                                    format!("📁 {}{}", room_name, count_suffix)
                                }
                            };

                            let room_label = if user_count == 0 && !is_current {
                                egui::RichText::new(room_label).weak()
                            } else {
                                egui::RichText::new(room_label)
                            };

                            let room_node = NodeBuilder::dir(TreeNodeId::Room(room_id))
                                .label(room_label)
                                .default_open(has_users_in_subtree)
                                .activatable(true)
                                .context_menu(|ui| {
                                    ui.set_min_width(200.0);
                                    ui.label(format!("Room: {}", room_name));
                                    ui.separator();
                                    ui.label(format!("ID: {}", room_id));
                                    if let Some(parent) = parent_id {
                                        ui.label(format!("Parent: {}", parent));
                                    } else {
                                        ui.label("Parent: None (root)");
                                    }
                                    if !room_description.is_empty() {
                                        ui.label(egui::RichText::new(&room_description).italics().weak());
                                    }
                                    ui.separator();

                                    if ui.button("Join").clicked() {
                                        pending_commands.push(Command::JoinRoom { room_id });
                                        ui.close();
                                    }
                                    if ui.button("Rename...").clicked() {
                                        *pending_rename = Some((Some(room_id), room_name.clone()));
                                        ui.close();
                                    }
                                    if ui.button("Edit Description...").clicked() {
                                        *pending_description =
                                            Some((room_id, room_name.clone(), room_description.clone()));
                                        ui.close();
                                    }
                                    if ui.button("Add Child Room").clicked() {
                                        pending_commands.push(Command::CreateRoom {
                                            name: "New Room".to_string(),
                                            parent_id: Some(room_id),
                                        });
                                        ui.close();
                                    }
                                    if !is_root && ui.button("Delete Room").clicked() {
                                        *pending_delete = Some((room_id, room_name.clone()));
                                        ui.close();
                                    }
                                });

                            let is_open = builder.node(room_node);

                            if is_open {
                                // Render users in this room
                                for user in state.users_in_room(room_id) {
                                    render_user(room_id, user, state, pending_commands, builder);
                                }

                                // Recursively render child rooms
                                for &child_id in &tree_node.children {
                                    render_room(
                                        child_id,
                                        state,
                                        rooms_with_users,
                                        pending_commands,
                                        pending_rename,
                                        pending_delete,
                                        pending_description,
                                        builder,
                                    );
                                }
                            }

                            builder.close_dir();
                        }

                        // Helper to render a user node
                        fn render_user(
                            room_id: Uuid,
                            user: &api::proto::User,
                            state: &backend::State,
                            pending_commands: &mut Vec<Command>,
                            builder: &mut egui_ltreeview::TreeViewBuilder<TreeNodeId>,
                        ) {
                            let user_id = user.user_id.as_ref().map(|id| id.value).unwrap_or(0);
                            let is_self = state.my_user_id == Some(user_id);

                            let is_talking = if is_self {
                                state.audio.is_transmitting
                            } else {
                                state.audio.talking_users.contains(&user_id)
                            };

                            let (user_muted, user_deafened) = if is_self {
                                (state.audio.self_muted, state.audio.self_deafened)
                            } else {
                                (user.is_muted, user.is_deafened)
                            };

                            let is_locally_muted = state.audio.muted_users.contains(&user_id);
                            let username = user.username.clone();
                            let self_muted = state.audio.self_muted;
                            let self_deafened = state.audio.self_deafened;
                            let user_volume_db = state
                                .audio
                                .per_user_rx
                                .get(&user_id)
                                .map(|c| c.volume_db)
                                .unwrap_or(0.0);

                            let user_node = NodeBuilder::leaf(TreeNodeId::User { room_id, user_id })
                                .label_ui(|ui| {
                                    ui.horizontal(|ui| {
                                        // Use non-selectable labels so clicks pass through to the tree view
                                        if is_talking {
                                            ui.add(
                                                egui::Label::new(egui::RichText::new("🎤").color(egui::Color32::GREEN))
                                                    .selectable(false),
                                            );
                                        } else if user_muted {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new("🎤").color(egui::Color32::DARK_RED),
                                                )
                                                .selectable(false),
                                            );
                                        } else {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new("🎤").color(egui::Color32::DARK_GRAY),
                                                )
                                                .selectable(false),
                                            );
                                        }

                                        if user_deafened {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new("🔇").color(egui::Color32::DARK_RED),
                                                )
                                                .selectable(false),
                                            );
                                        }

                                        if is_locally_muted && !is_self {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new("🔕").color(egui::Color32::YELLOW),
                                                )
                                                .selectable(false),
                                            );
                                        }

                                        ui.add(egui::Label::new(&username).selectable(false));
                                    });
                                })
                                .context_menu(|ui| {
                                    ui.set_min_width(200.0);

                                    ui.label(format!("User: {}", username));
                                    ui.separator();
                                    ui.label(format!("User ID: {}", user_id));
                                    ui.label(format!("In Room: {}", room_id));
                                    ui.separator();

                                    // Self actions
                                    if is_self {
                                        if self_muted {
                                            if ui.button("🎤 Unmute").clicked() {
                                                pending_commands.push(Command::SetMuted { muted: false });
                                                ui.close();
                                            }
                                        } else {
                                            if ui.button("🔇 Mute").clicked() {
                                                pending_commands.push(Command::SetMuted { muted: true });
                                                ui.close();
                                            }
                                        }

                                        if self_deafened {
                                            if ui.button("🔊 Undeafen").clicked() {
                                                pending_commands.push(Command::SetDeafened { deafened: false });
                                                ui.close();
                                            }
                                        } else {
                                            if ui.button("🔇 Deafen").clicked() {
                                                pending_commands.push(Command::SetDeafened { deafened: true });
                                                ui.close();
                                            }
                                        }

                                        ui.separator();

                                        if ui.button("📝 Register").clicked() {
                                            pending_commands.push(Command::RegisterUser { user_id });
                                            ui.close();
                                        }
                                        if ui.button("❌ Unregister").clicked() {
                                            pending_commands.push(Command::UnregisterUser { user_id });
                                            ui.close();
                                        }
                                    } else {
                                        // Other user actions

                                        // Volume slider
                                        ui.horizontal(|ui| {
                                            ui.label("🔊 Volume:");
                                            let mut volume = user_volume_db;
                                            let slider =
                                                egui::Slider::new(&mut volume, -40.0..=20.0).suffix(" dB").step_by(1.0);
                                            if ui.add(slider).changed() {
                                                pending_commands.push(Command::SetUserVolume {
                                                    user_id,
                                                    volume_db: volume,
                                                });
                                            }
                                        });
                                        if ui.button("Reset Volume").clicked() {
                                            pending_commands.push(Command::SetUserVolume {
                                                user_id,
                                                volume_db: 0.0,
                                            });
                                        }

                                        ui.separator();

                                        if is_locally_muted {
                                            if ui.button("🔔 Unmute Locally").clicked() {
                                                pending_commands.push(Command::UnmuteUser { user_id });
                                                ui.close();
                                            }
                                        } else {
                                            if ui.button("🔕 Mute Locally").clicked() {
                                                pending_commands.push(Command::MuteUser { user_id });
                                                ui.close();
                                            }
                                        }

                                        ui.separator();

                                        if ui.button("📝 Register").clicked() {
                                            pending_commands.push(Command::RegisterUser { user_id });
                                            ui.close();
                                        }
                                        if ui.button("❌ Unregister").clicked() {
                                            pending_commands.push(Command::UnregisterUser { user_id });
                                            ui.close();
                                        }
                                    }
                                });

                            builder.node(user_node);
                        }

                        // Render all root-level rooms
                        for &root_id in &state.room_tree.roots {
                            render_room(
                                root_id,
                                &state,
                                &rooms_with_users,
                                &mut pending_commands,
                                &mut pending_rename,
                                &mut pending_delete,
                                &mut pending_description,
                                builder,
                            );
                        }
                    });

                // Handle double-click activation (join room) and drag-and-drop moves
                for action in actions {
                    match action {
                        Action::Activate(activate) => {
                            for node_id in activate.selected {
                                if let TreeNodeId::Room(room_id) = node_id {
                                    pending_commands.push(Command::JoinRoom { room_id });
                                }
                            }
                        }
                        Action::Move(drag_drop) => {
                            // Only allow moving rooms (not users) onto rooms
                            if let Some(TreeNodeId::Room(source_room_id)) = drag_drop.source.first().cloned() {
                                if let TreeNodeId::Room(target_room_id) = drag_drop.target {
                                    if source_room_id != target_room_id {
                                        let source_name = state
                                            .room_tree
                                            .get(source_room_id)
                                            .map(|n| n.name.clone())
                                            .unwrap_or_else(|| "Unknown".to_string());
                                        let target_name = state
                                            .room_tree
                                            .get(target_room_id)
                                            .map(|n| n.name.clone())
                                            .unwrap_or_else(|| "Unknown".to_string());

                                        self.move_room_modal = MoveRoomModalState {
                                            open: true,
                                            room_id: Some(source_room_id),
                                            room_name: source_name,
                                            new_parent_id: Some(target_room_id),
                                            new_parent_name: target_name,
                                        };
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }

                // Execute pending commands
                for cmd in pending_commands {
                    // Check for JoinRoom to trigger auto-sync
                    let is_join_room = matches!(cmd, Command::JoinRoom { .. });
                    self.backend.send(cmd);
                    if is_join_room && self.persistent_settings.file_transfer.auto_sync_history {
                        self.backend.send(Command::RequestChatHistory);
                    }
                }

                // Handle rename modal
                if let Some((room_uuid, room_name)) = pending_rename {
                    self.rename_modal = RenameModalState {
                        open: true,
                        room_uuid,
                        room_name,
                    };
                }

                // Handle delete room confirmation
                if let Some((room_id, room_name)) = pending_delete {
                    self.delete_room_modal = DeleteRoomModalState {
                        open: true,
                        room_id: Some(room_id),
                        room_name,
                    };
                }

                // Handle description edit modal
                if let Some((room_uuid, room_name, description)) = pending_description {
                    self.description_modal = DescriptionModalState {
                        open: true,
                        room_uuid: Some(room_uuid),
                        room_name,
                        description,
                    };
                }
            });
        });

        // First-run setup modal (shown if no identity key is configured)
        self.render_first_run_dialog(ctx);

        // Certificate acceptance modal (shown when server presents untrusted cert)
        if let ConnectionState::CertificatePending { cert_info } = &state.connection {
            let fingerprint = cert_info.fingerprint_hex();
            let server_name = cert_info.server_name.clone();
            let server_addr = cert_info.server_addr.clone();
            let certificate_der = cert_info.certificate_der.clone();

            Modal::new(egui::Id::new("cert_modal")).show(ctx, |ui| {
                ui.set_width(450.0);
                ui.heading("⚠ Untrusted Certificate");

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("The server presented a certificate that is not trusted.")
                        .color(egui::Color32::YELLOW),
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Server:");
                    ui.label(egui::RichText::new(&server_addr).strong());
                });
                ui.horizontal(|ui| {
                    ui.label("Certificate for:");
                    ui.label(egui::RichText::new(&server_name).strong());
                });

                ui.add_space(8.0);
                ui.label("Certificate Fingerprint (SHA256):");
                ui.add(
                    egui::TextEdit::multiline(&mut fingerprint.as_str())
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(2)
                        .interactive(false),
                );

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "If you expected to connect to a server with a self-signed certificate, verify the \
                         fingerprint matches what the server administrator provided.",
                    )
                    .weak(),
                );

                ui.add_space(8.0);
                ui.separator();

                egui::Sides::new().show(
                    ui,
                    |ui| {
                        ui.label(egui::RichText::new("⚠ Only accept if you trust this server").weak());
                    },
                    |ui| {
                        if ui.button("Accept Certificate").clicked() {
                            // Save the certificate to persistent settings
                            use base64::Engine;
                            let accepted_cert = AcceptedCertificate {
                                server_name: server_name.clone(),
                                fingerprint_hex: fingerprint.clone(),
                                certificate_der_base64: base64::engine::general_purpose::STANDARD
                                    .encode(&certificate_der),
                            };

                            // Add to persistent settings if not already present
                            if !self
                                .persistent_settings
                                .accepted_certificates
                                .iter()
                                .any(|c| c.fingerprint_hex == fingerprint)
                            {
                                self.persistent_settings.accepted_certificates.push(accepted_cert);
                                if let Err(e) = self.persistent_settings.save() {
                                    tracing::error!("Failed to save accepted certificate: {}", e);
                                }
                            }

                            self.backend.send(Command::AcceptCertificate);
                            self.backend.send(Command::LocalMessage {
                                text: format!(
                                    "Accepted certificate for {} (saved for future connections)",
                                    server_name
                                ),
                            });
                        }
                        if ui.button("Reject").clicked() {
                            self.backend.send(Command::RejectCertificate);
                            self.backend.send(Command::LocalMessage {
                                text: format!("Rejected certificate for {}", server_name),
                            });
                        }
                    },
                );
            });
        }

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
                ui.checkbox(&mut self.trust_dev_cert, "Trust dev cert (dev-certs/server-cert.der)");
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
                    selected_categories: std::iter::once(SettingsCategory::default()).collect(),
                    pending_settings: Some(audio.settings.clone()),
                    pending_input_device: Some(audio.selected_input.clone()),
                    pending_output_device: Some(audio.selected_output.clone()),
                    pending_voice_mode: Some(audio.voice_mode.clone()),
                    pending_autoconnect: Some(self.autoconnect_on_launch),
                    pending_username: Some(self.client_name.clone()),
                    pending_tx_pipeline: Some(audio.tx_pipeline.clone()),
                    pending_show_timestamps: Some(self.persistent_settings.show_chat_timestamps),
                    pending_timestamp_format: Some(self.persistent_settings.chat_timestamp_format),
                    dirty: false,
                    hotkey_capture_target: None,
                    hotkey_conflict_pending: None,
                };
            }

            let modal = Modal::new(egui::Id::new("settings_modal")).show(ctx, |ui| {
                ui.set_min_size(egui::vec2(600.0, 400.0));
                ui.set_max_size(egui::vec2(800.0, 600.0));

                // Header
                ui.horizontal(|ui| {
                    ui.heading("Settings");
                });
                ui.separator();

                // Main content area with sidebar and content panel
                egui::TopBottomPanel::bottom("settings_footer")
                    .frame(egui::Frame::NONE)
                    .show_inside(ui, |ui| {
                        ui.add_space(4.0);
                        // Show dirty indicator
                        if self.settings_modal.dirty {
                            ui.colored_label(egui::Color32::YELLOW, "Changes not yet applied");
                        }

                        egui::Sides::new().show(
                            ui,
                            |_l| {},
                            |ui| {
                                // Apply button - commits all pending changes and saves to disk
                                let apply_enabled = self.settings_modal.dirty;
                                if ui
                                    .add_enabled(apply_enabled, egui::Button::new("Apply"))
                                    .on_hover_text("Apply changes and save to disk")
                                    .clicked()
                                {
                                    self.apply_pending_settings();
                                    self.settings_modal.dirty = false;
                                    self.save_settings();
                                    self.toast_manager.success("Settings saved");
                                }

                                if ui.button("Cancel").on_hover_text("Discard changes and close").clicked() {
                                    self.settings_modal = SettingsModalState::default();
                                    self.show_settings = false;
                                }

                                if ui
                                    .button("Close")
                                    .on_hover_text("Apply changes, save, and close")
                                    .clicked()
                                {
                                    // Apply changes before closing if dirty
                                    if self.settings_modal.dirty {
                                        self.apply_pending_settings();
                                        self.save_settings();
                                        self.toast_manager.success("Settings saved");
                                    }
                                    self.settings_modal = SettingsModalState::default();
                                    self.show_settings = false;
                                }
                            },
                        );
                    });

                egui::SidePanel::left("settings_sidebar_panel")
                    .resizable(false)
                    .default_width(130.0)
                    .frame(egui::Frame::NONE)
                    .show_inside(ui, |ui| {
                        egui::ScrollArea::vertical().id_salt("settings_sidebar").show(ui, |ui| {
                            ui.add_space(4.0);
                            let ctrl_held = ui.input(|i| i.modifiers.ctrl);
                            for category in SettingsCategory::all() {
                                let selected = self.settings_modal.selected_categories.contains(category);
                                if ui.selectable_label(selected, category.label()).clicked() {
                                    if ctrl_held {
                                        // Ctrl+click: toggle this category in the set
                                        if selected {
                                            self.settings_modal.selected_categories.remove(category);
                                            // Ensure at least one category is selected
                                            if self.settings_modal.selected_categories.is_empty() {
                                                self.settings_modal.selected_categories.insert(*category);
                                            }
                                        } else {
                                            self.settings_modal.selected_categories.insert(*category);
                                        }
                                    } else {
                                        // Normal click: select only this category
                                        self.settings_modal.selected_categories.clear();
                                        self.settings_modal.selected_categories.insert(*category);
                                    }
                                }
                            }
                        });
                    });

                // Content panel takes remaining space
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show_inside(ui, |ui| {
                        egui::ScrollArea::vertical().id_salt("settings_content").show(ui, |ui| {
                            // Render all selected categories in order
                            let mut first = true;
                            for category in SettingsCategory::all() {
                                if self.settings_modal.selected_categories.contains(category) {
                                    if !first {
                                        ui.add_space(16.0);
                                        ui.separator();
                                        ui.add_space(8.0);
                                    }
                                    first = false;

                                    match category {
                                        SettingsCategory::Connection => {
                                            self.render_settings_connection(ui, &state);
                                        }
                                        SettingsCategory::Devices => {
                                            self.render_settings_devices(ui, &state);
                                        }
                                        SettingsCategory::Voice => {
                                            self.render_settings_voice(ui, &state);
                                        }
                                        SettingsCategory::Sounds => {
                                            self.render_settings_sounds(ui);
                                        }
                                        SettingsCategory::Processing => {
                                            self.render_settings_processing(ui, &state);
                                        }
                                        SettingsCategory::Encoder => {
                                            self.render_settings_encoder(ui);
                                        }
                                        SettingsCategory::Chat => {
                                            self.render_settings_chat(ui);
                                        }
                                        SettingsCategory::FileTransfer => {
                                            self.render_settings_file_transfer(ui);
                                        }
                                        SettingsCategory::Keyboard => {
                                            self.render_settings_keyboard(ui);
                                        }
                                        SettingsCategory::Statistics => {
                                            self.render_settings_statistics(ui, &state);
                                        }
                                    }
                                }
                            }
                        });
                    });
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

        // Edit room description modal
        if self.description_modal.open {
            let modal = Modal::new(egui::Id::new("description_modal")).show(ctx, |ui| {
                ui.set_width(350.0);
                ui.heading(format!("Description: {}", self.description_modal.room_name));
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::multiline(&mut self.description_modal.description)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY)
                        .hint_text("Enter room description..."),
                );
                ui.separator();
                egui::Sides::new().show(
                    ui,
                    |_l| {},
                    |ui| {
                        if ui.button("Save").clicked() {
                            if let Some(uuid) = self.description_modal.room_uuid {
                                self.backend.send(Command::SetRoomDescription {
                                    room_id: uuid,
                                    description: self.description_modal.description.trim().to_string(),
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
                self.description_modal.open = false;
            }
        }

        // Move room confirmation modal
        if self.move_room_modal.open {
            let modal = Modal::new(egui::Id::new("move_room_modal")).show(ctx, |ui| {
                ui.set_width(350.0);
                ui.heading("Move Room");
                ui.add_space(8.0);

                ui.label(format!(
                    "Move \"{}\" into \"{}\"?",
                    self.move_room_modal.room_name, self.move_room_modal.new_parent_name
                ));

                ui.add_space(8.0);
                ui.label(egui::RichText::new("This will change the room's parent.").weak());

                ui.add_space(12.0);
                ui.separator();
                egui::Sides::new().show(
                    ui,
                    |_l| {},
                    |ui| {
                        if ui.button("Move").clicked() {
                            if let (Some(room_id), Some(new_parent_id)) =
                                (self.move_room_modal.room_id, self.move_room_modal.new_parent_id)
                            {
                                self.backend.send(Command::MoveRoom { room_id, new_parent_id });
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
                self.move_room_modal.open = false;
            }
        }

        // Delete room confirmation modal
        if self.delete_room_modal.open {
            let modal = Modal::new(egui::Id::new("delete_room_modal")).show(ctx, |ui| {
                ui.set_width(350.0);
                ui.heading("Delete Room");
                ui.add_space(8.0);

                ui.label(format!(
                    "Are you sure you want to delete room '{}'?",
                    self.delete_room_modal.room_name
                ));
                ui.add_space(4.0);
                ui.label(egui::RichText::new("This cannot be undone.").weak());

                ui.add_space(12.0);
                ui.separator();
                egui::Sides::new().show(
                    ui,
                    |_l| {},
                    |ui| {
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new("Delete").color(egui::Color32::RED),
                            ))
                            .clicked()
                        {
                            if let Some(room_id) = self.delete_room_modal.room_id {
                                self.backend.send(Command::DeleteRoom { room_id });
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
                self.delete_room_modal.open = false;
            }
        }

        // Image View Modal (click-to-enlarge with zoom/pan)
        if self.image_view_modal.open {
            // Load the full-resolution image from disk if not yet cached
            if !self.image_view_modal.fullsize_loaded {
                if let Some(ref path) = self.image_view_modal.file_path {
                    let uri = &self.image_view_modal.image_uri;
                    let already_cached = ctx.try_load_bytes(uri).is_ok();
                    if !already_cached {
                        if let Ok(bytes) = std::fs::read(path) {
                            ctx.include_bytes(uri.clone(), bytes);
                        }
                    }
                }
                self.image_view_modal.fullsize_loaded = true;
            }

            // Handle scroll-wheel zoom before the modal consumes events.
            // We use raw_scroll_delta which is always populated regardless of
            // modifier keys (smooth_scroll_delta is empty when Ctrl is held
            // because egui routes Ctrl+Scroll to its global zoom system).
            // Scroll wheel always zooms; drag-to-pan is used for panning.
            let scroll_zoom = ctx.input(|i| {
                let delta = i.raw_scroll_delta.y;
                if delta != 0.0 { Some(delta) } else { None }
            });
            if let Some(delta) = scroll_zoom {
                let zoom_factor = 1.0 + delta * 0.005;
                self.image_view_modal.zoom = (self.image_view_modal.zoom * zoom_factor).clamp(0.25, 10.0);
            }

            let modal = Modal::new(egui::Id::new("image_view_modal")).show(ctx, |ui| {
                // Header with image name and zoom controls
                ui.horizontal(|ui| {
                    ui.heading(&self.image_view_modal.image_name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            ui.close();
                        }
                        if ui
                            .button("Fit")
                            .on_hover_text("Reset zoom to fit image in window")
                            .clicked()
                        {
                            self.image_view_modal.zoom = 1.0;
                        }
                        if ui.button("+").clicked() {
                            self.image_view_modal.zoom = (self.image_view_modal.zoom * 1.25).clamp(0.25, 10.0);
                        }
                        if ui.button("\u{2212}").clicked() {
                            // Unicode minus sign
                            self.image_view_modal.zoom = (self.image_view_modal.zoom / 1.25).clamp(0.25, 10.0);
                        }
                        ui.label(format!("{:.0}%", self.image_view_modal.zoom * 100.0));
                    });
                });
                ui.add_space(4.0);

                // Calculate the viewport for the image: use ~90% of the window
                let content_rect = ctx.content_rect();
                let viewport_width = content_rect.width();
                let viewport_height = content_rect.height();
                let max_width = (viewport_width * 0.9).max(1.0);
                let max_height = ((viewport_height * 0.9) - 80.0).max(1.0);

                // Force the modal to allocate the full space so it doesn't
                // shrink-wrap to a small image
                ui.set_min_size(egui::vec2(max_width, max_height));

                // Get the native image size to compute fitted dimensions
                let image_uri = &self.image_view_modal.image_uri;
                let image_size = ui
                    .ctx()
                    .try_load_texture(
                        image_uri,
                        egui::TextureOptions::LINEAR,
                        egui::SizeHint::Scale(1.0.into()),
                    )
                    .ok()
                    .and_then(|poll| match poll {
                        egui::load::TexturePoll::Ready { texture } => Some(texture.size),
                        _ => None,
                    });

                let zoom = self.image_view_modal.zoom;

                if let Some(native_size) = image_size.filter(|s| s.x > 0.0 && s.y > 0.0) {
                    // Compute the fitted size at zoom 1.0 (fit to viewport)
                    let aspect = native_size.x / native_size.y;
                    let (fit_w, fit_h) = if max_width / max_height > aspect {
                        (max_height * aspect, max_height)
                    } else {
                        (max_width, max_width / aspect)
                    };

                    // Apply zoom to the fitted dimensions
                    let zoomed_w = fit_w * zoom;
                    let zoomed_h = fit_h * zoom;

                    // ScrollArea for panning when zoomed in.
                    // Mouse wheel is disabled here (used for zoom instead);
                    // scrollbars and drag handle panning.
                    egui::ScrollArea::both()
                        .id_salt("image_zoom_scroll")
                        .max_width(max_width)
                        .max_height(max_height)
                        .auto_shrink([false, false])
                        .scroll_source(egui::containers::scroll_area::ScrollSource {
                            scroll_bar: true,
                            drag: true,
                            mouse_wheel: false,
                        })
                        .show(ui, |ui| {
                            ui.add(
                                egui::Image::new(image_uri)
                                    .fit_to_exact_size(egui::vec2(zoomed_w, zoomed_h))
                                    .corner_radius(4.0),
                            );
                        });

                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Scroll to zoom, drag to pan")
                            .small()
                            .color(egui::Color32::GRAY),
                    );
                } else {
                    // Fallback if texture not yet loaded
                    ui.add(
                        egui::Image::new(image_uri)
                            .max_width(max_width)
                            .max_height(max_height)
                            .corner_radius(4.0),
                    );
                }
            });
            if modal.should_close() {
                // Evict the full-resolution image from cache to free memory
                ctx.forget_image(&self.image_view_modal.image_uri);
                self.image_view_modal.open = false;
                self.image_view_modal.fullsize_loaded = false;
            }
        }

        // Render toast notifications (always last, so they overlay everything)
        self.toast_manager.render(ctx);
    }
}
